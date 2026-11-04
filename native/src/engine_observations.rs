//! App-owned command history and asynchronous waits over committed receipts.
//! No request here schedules work, replays an effect, or releases admission.
use super::*;
use crate::agent_efficiency::AsyncWaitBridge;
use rudder_engine::{agent_efficiency::{BoundedWaitRegistry, CancelWait, SettledOutcome, WaitResult},
    journal::{Journal, OwnerFence, CommandState}};
use std::collections::HashMap;
use std::sync::Arc;

pub(crate) struct EngineObservations {
    registry: BoundedWaitRegistry,
    bridge: AsyncWaitBridge,
    pending: HashMap<String,(mpsc::SyncSender<serde_json::Value>,CancelWait,serde_json::Value)>,
    wake: PtyOutputWaker,
    last_pump: Option<Instant>,
}
impl EngineObservations {
    pub fn new(wake: Option<PtyOutputWaker>) -> Result<Self> {
        let registry=BoundedWaitRegistry::new(16,256)?;
        Ok(Self {bridge:AsyncWaitBridge::new(registry.clone(),16)?,registry,
            pending:HashMap::new(),wake:wake.unwrap_or_else(||Arc::new(||{})),last_pump:None})
    }
    pub fn poll(&mut self,journal:&mut Journal,owner:&OwnerFence)->Result<()> {
        if self.last_pump.is_none_or(|last|last.elapsed()>=Duration::from_millis(100)) {
            for delivery in journal.apply_surface_completions(owner,"native-app-v1",8,epoch_ms()?)? {
                let scope=journal.command_wait_scope(owner,&delivery.receipt.command_id)?;
                // Settled means this command has a terminal receipt, not that
                // its backend task, integration or deployment succeeded.
                self.registry.observe(&scope,SettledOutcome::Succeeded)?;
            }
            self.last_pump=Some(Instant::now());
        }
        while let Some(message)=self.bridge.try_recv() {
            let (id,result)=message.into_parts();
            if let Some((sender,_,mut status))=self.pending.remove(&id) {
                status["command_wait"]=match result {
                    WaitResult::Settled {scope,..} => {
                        let mut receipt=serde_json::to_value(journal.command_receipt(owner,&scope.id)?)?;
                        if serde_json::to_vec(&receipt)?.len()>8192 {
                            receipt["result"]=serde_json::Value::Null;
                            receipt["result_omitted"]=serde_json::Value::Bool(true);
                        }
                        // Observing a committed command does not classify its
                        // result or the backend, verification or deployment.
                        serde_json::json!({"status":"receipt_committed","scope":scope,"receipt":receipt})
                    },
                    other=>serde_json::to_value(other)?,
                };
                let _=sender.send(serde_json::json!({"ok":true,"status":status}));
            }
        }
        Ok(())
    }
    pub fn begin(&mut self,journal:&Journal,owner:&OwnerFence,id:&str,timeout_ms:u64,
        sender:mpsc::SyncSender<serde_json::Value>,status:serde_json::Value)->Result<()> {
        anyhow::ensure!(timeout_ms<=2000,"command wait must be at most 2000 ms");
        let scope=journal.command_wait_scope(owner,id)?;
        let receipt=journal.command_receipt(owner,id)?;
        if receipt.state==CommandState::Completed {self.registry.observe(&scope,SettledOutcome::Succeeded)?;}
        if receipt.state==CommandState::Uncertain {
            let mut status=status;status["command_wait"]=serde_json::json!({"status":"uncertain","scope":scope});
            let _=sender.send(serde_json::json!({"ok":true,"status":status}));return Ok(());
        }
        let request=uuid::Uuid::new_v4().to_string();
        let cancellation=self.bridge.start(request.clone(),&scope,Duration::from_millis(timeout_ms),self.wake.clone())?;
        self.pending.insert(request,(sender,cancellation,status));
        Ok(())
    }
}
impl Drop for EngineObservations {
    fn drop(&mut self) {for (_,cancel,_) in self.pending.values(){cancel.cancel();}}
}

fn epoch_ms()->Result<i64>{Ok(SystemTime::now().duration_since(UNIX_EPOCH)?.as_millis().try_into()?)}

#[cfg(test)]
mod tests {
    use super::*;
    use rudder_engine::journal::{CapabilityGrant,CommandRequest};
    use serde_json::json;
    use std::collections::BTreeSet;

    struct Fixture {root:PathBuf,path:PathBuf,journal:Journal,owner:OwnerFence,id:String}
    impl Drop for Fixture {fn drop(&mut self){let _=fs::remove_dir_all(&self.root);}}
    impl Fixture {
        fn new()->Self {
            let root=(if std::path::Path::new("/workspace").is_dir(){PathBuf::from("/workspace/rudder-scratch/n13")}else{std::env::temp_dir().canonicalize().unwrap().join("rudder-n13")}).join(format!("engine-observations-{}",uuid::Uuid::new_v4()));
            fs::create_dir_all(&root).unwrap();let path=root.join("journal.sqlite");
            let mut journal=Journal::open(&path).unwrap();let owner=journal.create_project("observations","app-owner","accepted-head",1).unwrap();
            journal.register_run(&owner,"sender",1).unwrap();journal.register_run(&owner,"target",1).unwrap();
            let token=journal.issue_capability(&owner,&CapabilityGrant {principal:"sender".into(),run_id:"sender".into(),run_epoch:1,
                operations:["command:inspect".into()].into(),recipients:[("target".into(),1)].into(),
                scopes:["project".into()].into(),topics:BTreeSet::new(),expires_at_ms:None},1).unwrap();
            let accepted=journal.accept_command(&token,&CommandRequest {idempotency_key:"inspect-once".into(),target_run:"target".into(),
                target_epoch:1,kind:"inspect".into(),scope:"project".into(),payload:json!({"inspect":"current"})},2).unwrap();
            Self {root,path,journal,owner,id:accepted.command_id}
        }
        fn finish(&mut self) {
            let claim=self.journal.claim_effect(&self.owner,&self.id,3).unwrap().unwrap();
            self.journal.finish_effect(&claim,&json!({"backend_complete":true,"verification":"pending"}),4).unwrap();
        }
    }
    fn observations()->(EngineObservations,mpsc::Receiver<()>) {
        let (sender,receiver)=mpsc::sync_channel(16);
        let wake:PtyOutputWaker=Arc::new(move||{let _=sender.try_send(());});
        (EngineObservations::new(Some(wake)).unwrap(),receiver)
    }

    #[test]
    fn engine_observations_background_wait_wakes_on_real_completion_and_history_cursor_survives_reopen() {
        let mut f=Fixture::new();let (mut view,wake)=observations();let (sender,response)=mpsc::sync_channel(1);
        let start=Instant::now();view.begin(&f.journal,&f.owner,&f.id,2000,sender,json!({"surface":"app"})).unwrap();
        assert!(start.elapsed()<Duration::from_millis(500),"App blocked on a pending engine command");
        assert!(response.try_recv().is_err());assert_eq!(view.pending.len(),1);
        assert_eq!(f.journal.command_receipt(&f.owner,&f.id).unwrap().state,CommandState::Accepted);
        f.finish();view.poll(&mut f.journal,&f.owner).unwrap();
        wake.recv_timeout(Duration::from_secs(2)).unwrap();view.poll(&mut f.journal,&f.owner).unwrap();
        let returned=response.recv_timeout(Duration::from_secs(1)).unwrap();
        assert_eq!(returned["status"]["surface"],"app");assert_eq!(returned["status"]["command_wait"]["status"],"receipt_committed");
        assert_eq!(returned["status"]["command_wait"]["scope"]["id"],f.id);
        assert!(view.pending.is_empty());
        assert_eq!(f.journal.surface_receipts(&f.owner,"command-history-v1",10).unwrap().len(),1);
        assert!(f.journal.completion_outbox(&f.owner,"native-app-v1",10).unwrap().is_empty());
        let mut reopened=Journal::open(&f.path).unwrap();let (mut second,wake)=observations();
        let (sender,response)=mpsc::sync_channel(1);second.begin(&reopened,&f.owner,&f.id,2000,sender,json!({})).unwrap();
        wake.recv_timeout(Duration::from_secs(2)).unwrap();second.poll(&mut reopened,&f.owner).unwrap();
        assert_eq!(response.recv_timeout(Duration::from_secs(1)).unwrap()["status"]["command_wait"]["status"],"receipt_committed");
        assert_eq!(reopened.surface_receipts(&f.owner,"command-history-v1",10).unwrap().len(),1);
        assert!(reopened.claim_effect(&f.owner,&f.id,5).unwrap().is_none());
        assert_eq!(reopened.command_receipt(&f.owner,&f.id).unwrap().result.unwrap()["verification"],"pending");
    }

    #[test]
    fn engine_observations_rejected_command_reports_receipt_without_success_claim() {
        let mut f=Fixture::new();let (mut view,wake)=observations();
        let claim=f.journal.claim_effect(&f.owner,&f.id,3).unwrap().unwrap();
        f.journal.finish_effect(&claim,&json!({"status":"rejected","reason":"adapter_queue_full"}),4).unwrap();
        let (sender,response)=mpsc::sync_channel(1);
        view.begin(&f.journal,&f.owner,&f.id,2000,sender,json!({})).unwrap();
        wake.recv_timeout(Duration::from_secs(2)).unwrap();view.poll(&mut f.journal,&f.owner).unwrap();
        let returned=response.recv_timeout(Duration::from_secs(1)).unwrap();
        let observation=&returned["status"]["command_wait"];
        assert_eq!(observation["status"],"receipt_committed");
        assert_eq!(observation["receipt"]["result"]["status"],"rejected");
        assert!(observation.get("outcome").is_none());
    }

    #[test]
    fn engine_observations_timeout_is_observation_only_and_preserves_pending_effect_and_admission() {
        let mut f=Fixture::new();let (mut view,wake)=observations();
        let scope=f.journal.command_wait_scope(&f.owner,&f.id).unwrap();view.registry.admit(&scope).unwrap();
        let (sender,response)=mpsc::sync_channel(1);view.begin(&f.journal,&f.owner,&f.id,0,sender,json!({})).unwrap();
        wake.recv_timeout(Duration::from_secs(2)).unwrap();view.poll(&mut f.journal,&f.owner).unwrap();
        let returned=response.recv_timeout(Duration::from_secs(1)).unwrap();
        assert_eq!(returned["status"]["command_wait"]["status"],"timeout");
        assert_eq!(returned["status"]["command_wait"]["name"],"job_wait_timeout");
        assert!(view.registry.is_admitted(&scope));assert!(view.pending.is_empty());
        assert_eq!(f.journal.command_receipt(&f.owner,&f.id).unwrap().state,CommandState::Accepted);
        assert_eq!(f.journal.ready_commands(&f.owner,10).unwrap().len(),1);
        assert!(f.journal.surface_receipts(&f.owner,"command-history-v1",10).unwrap().is_empty());
        let (sender,_)=mpsc::sync_channel(1);assert!(view.begin(&f.journal,&f.owner,&f.id,2001,sender,json!({})).is_err());
    }

    #[test]
    fn engine_observations_uncertain_is_explicit_and_stale_owner_cannot_wait_or_apply_history() {
        let mut f=Fixture::new();let claim=f.journal.claim_effect(&f.owner,&f.id,3).unwrap().unwrap();
        f.journal.recover(&f.owner,4).unwrap();let (mut view,_wake)=observations();
        let (sender,response)=mpsc::sync_channel(1);view.begin(&f.journal,&f.owner,&f.id,2000,sender,json!({})).unwrap();
        assert_eq!(response.recv_timeout(Duration::from_secs(1)).unwrap()["status"]["command_wait"]["status"],"uncertain");
        assert!(view.pending.is_empty());view.poll(&mut f.journal,&f.owner).unwrap();
        assert_eq!(f.journal.command_receipt(&f.owner,&f.id).unwrap().state,CommandState::Uncertain);
        assert!(f.journal.surface_receipts(&f.owner,"command-history-v1",10).unwrap().is_empty());
        f.journal.reconcile_effect(&f.owner,&f.id,&rudder_engine::journal::Reconciliation::Completed {
            result:json!({"reconciled":true}),evidence:json!({"observed_external_effect":claim.claim_id})},5).unwrap();
        let replacement=f.journal.transfer_owner(&f.owner,"replacement",6).unwrap();
        let (sender,_)=mpsc::sync_channel(1);assert!(view.begin(&f.journal,&f.owner,&f.id,2000,sender,json!({})).is_err());
        let (mut stale,_)=observations();assert!(stale.poll(&mut f.journal,&f.owner).is_err());
        assert!(f.journal.surface_receipts(&replacement,"command-history-v1",10).unwrap().is_empty());
        stale.poll(&mut f.journal,&replacement).unwrap();
        assert_eq!(f.journal.surface_receipts(&replacement,"command-history-v1",10).unwrap().len(),1);
    }
}

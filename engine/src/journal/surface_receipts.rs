//! Durable application by local product surfaces. These receipts are not peer
//! acknowledgments and never authorize scheduling or an external process.
use super::*;
use crate::agent_efficiency::{WaitKind, WaitScope};
use super::restart_recovery::CompletionDelivery;

impl Journal {
    /// Explicit migration/recovery only, including an inactive exact owner.
    pub fn recorded_run_inventory(&self, owner: &OwnerFence) -> Result<Vec<AgentIdentity>> {
        ensure!(self.project_state(&owner.project_id)?.owner == *owner,"recorded owner differs");
        let mut statement=self.conn.prepare("SELECT run_id,epoch FROM runs WHERE project_id=?1 ORDER BY run_id LIMIT 1025")?;
        let rows=statement.query_map([&owner.project_id],|row|Ok(AgentIdentity {
            project_id:owner.project_id.clone(),run_id:row.get(0)?,run_epoch:db_u64(row,1)?
        }))?.collect::<rusqlite::Result<Vec<_>>>()?;
        ensure!(rows.len()<=1024,"recorded run inventory exceeds bound");
        Ok(rows)
    }
    /// Recovery inspection only: accepts an inactive exact recorded owner,
    /// grants no write/scheduling authority, and never advances a cursor.
    pub fn recorded_surface_receipt(&self, owner: &OwnerFence, key: &str, kind: &str) -> Result<Option<Value>> {
        valid_id(key)?; valid_id(kind)?;
        ensure!(self.project_state(&owner.project_id)?.owner == *owner,"recorded owner differs");
        let prior:Option<String>=self.conn.query_row(
            "SELECT payload_json FROM events WHERE project_id=?1 AND kind='surface-receipt' AND json_extract(payload_json,'$.kind')=?2 AND json_extract(payload_json,'$.key')=?3 ORDER BY sequence LIMIT 1",
            params![owner.project_id,kind,key],|row|row.get(0)).optional()?;
        prior.map(|encoded|Ok(serde_json::from_str::<Value>(&encoded)?["result"].clone())).transpose()
    }
    pub fn record_surface_receipt(&mut self, owner: &OwnerFence, key: &str, kind: &str,
        request: &Value, result: &Value, now_ms: i64) -> Result<Value> {
        valid_id(key)?; valid_id(kind)?;
        ensure!(serde_json::to_vec(request)?.len() <= 256 * 1024
            && serde_json::to_vec(result)?.len() <= 256 * 1024, "surface receipt exceeds bound");
        let tx = self.conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
        assert_owner(&tx, owner)?;
        let prior: Option<String> = tx.query_row(
            "SELECT payload_json FROM events WHERE project_id=?1 AND kind='surface-receipt' AND json_extract(payload_json,'$.kind')=?2 AND json_extract(payload_json,'$.key')=?3 ORDER BY sequence LIMIT 1",
            params![owner.project_id,kind,key], |row|row.get(0)).optional()?;
        if let Some(prior) = prior {
            let prior: Value = serde_json::from_str(&prior)?;
            ensure!(prior["request"] == *request, "surface receipt idempotency conflict");
            return Ok(prior["result"].clone());
        }
        append_event(&tx,&owner.project_id,"surface-receipt",&owner.owner_id,
            &serde_json::json!({"key":key,"kind":kind,"request":request,"result":result}),now_ms)?;
        tx.commit()?;
        Ok(result.clone())
    }

    pub fn surface_receipts(&self, owner: &OwnerFence, kind: &str, limit: usize) -> Result<Vec<Value>> {
        assert_owner(&self.conn,owner)?; valid_id(kind)?;
        ensure!((1..=256).contains(&limit),"surface receipt page must be 1..256");
        let mut statement=self.conn.prepare("SELECT payload_json FROM events WHERE project_id=?1 AND kind='surface-receipt' AND json_extract(payload_json,'$.kind')=?2 ORDER BY sequence DESC LIMIT ?3")?;
        let rows=statement.query_map(params![owner.project_id,kind,limit as i64],|row|row.get::<_,String>(0))?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        rows.into_iter().map(|row|Ok(serde_json::from_str::<Value>(&row)?["result"].clone())).collect()
    }

    /// Apply a bounded completed prefix to the persistent command-history
    /// surface before acknowledging its outbox cursor. A crash between these
    /// commits retries the immutable application, without repeating the effect.
    pub fn apply_surface_completions(&mut self, owner: &OwnerFence, consumer: &str,
        limit: usize, now_ms: i64) -> Result<Vec<CompletionDelivery>> {
        self.backfill_completion_outbox(owner,limit,now_ms)?;
        let deliveries=self.completion_outbox(owner,consumer,limit)?;
        for delivery in &deliveries {
            let request=serde_json::json!({"consumer":consumer,"command_id":delivery.receipt.command_id});
            // Full result remains in the authoritative command receipt. Keep
            // the view bounded even for a large backend response.
            let mut result=serde_json::to_value(delivery)?;
            if serde_json::to_vec(&result)?.len()>8192 {
                result["receipt"]["result"]=Value::Null;
                result["result_omitted"]=Value::Bool(true);
            }
            // The transport sequence is a delivery cursor, not application
            // identity. Also recognize receipts written by the previous
            // sequence-keyed implementation across an upgrade/crash window.
            let tx=self.conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
            assert_owner(&tx,owner)?;
            let prior:Option<String>=tx.query_row(
                "SELECT payload_json FROM events WHERE project_id=?1 AND kind='surface-receipt' AND json_extract(payload_json,'$.kind')='command-history-v1' AND json_extract(payload_json,'$.request.consumer')=?2 AND json_extract(payload_json,'$.request.command_id')=?3 ORDER BY sequence LIMIT 1",
                params![owner.project_id,consumer,delivery.receipt.command_id],|row|row.get(0)).optional()?;
            if let Some(prior)=prior {
                let prior:Value=serde_json::from_str(&prior)?;
                ensure!(prior["result"]["receipt"]==result["receipt"],
                    "completion command identity has conflicting durable receipt");
            } else {
                append_event(&tx,&owner.project_id,"surface-receipt",&owner.owner_id,
                    &serde_json::json!({"key":format!("{consumer}:{}",delivery.receipt.command_id),
                        "kind":"command-history-v1","request":request,"result":result}),now_ms)?;
            }
            tx.commit()?;
            self.acknowledge_completions(owner,consumer,delivery.sequence,now_ms)?;
        }
        Ok(deliveries)
    }

    /// Scope comes from the actual accepted row, never a proposed run/epoch.
    pub fn command_wait_scope(&self, owner: &OwnerFence, id: &str) -> Result<WaitScope> {
        assert_owner(&self.conn,owner)?; valid_id(id)?;
        let (run_id,epoch):(String,u64)=self.conn.query_row(
            "SELECT target_run,target_epoch FROM commands WHERE project_id=?1 AND command_id=?2",
            params![owner.project_id,id], |row|Ok((row.get(0)?,db_u64(row,1)?)))?;
        Ok(WaitScope {project_id:owner.project_id.clone(),owner_epoch:owner.epoch,
            run_id,run_epoch:epoch,kind:WaitKind::Job,id:id.into()})
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use std::path::PathBuf;

    struct Fixture { _directory:tempfile::TempDir,path:PathBuf,journal:Journal,owner:OwnerFence,token:String }
    impl Fixture {
        fn new()->Self {
            let root=Path::new("/workspace/rudder-scratch/n13");std::fs::create_dir_all(root).unwrap();
            let directory=tempfile::Builder::new().prefix("surface-receipts-").tempdir_in(root).unwrap();
            let path=directory.path().join("journal.sqlite");let mut journal=Journal::open(&path).unwrap();
            let owner=journal.create_project("surface-project","original","accepted-head",1).unwrap();
            journal.register_run(&owner,"sender",1).unwrap();journal.register_run(&owner,"target",1).unwrap();
            let token=journal.issue_capability(&owner,&CapabilityGrant {
                principal:"sender".into(),run_id:"sender".into(),run_epoch:1,
                operations:["command:inspect".into()].into(),recipients:[("target".into(),1)].into(),
                scopes:["project".into()].into(),topics:BTreeSet::new(),expires_at_ms:None },1).unwrap();
            Self {_directory:directory,path,journal,owner,token}
        }
        fn complete(&mut self,result:Value)->String {
            let receipt=self.journal.accept_command(&self.token,&CommandRequest {
                idempotency_key:Uuid::new_v4().to_string(),target_run:"target".into(),target_epoch:1,
                kind:"inspect".into(),scope:"project".into(),payload:json!({"inspect":"current state"}) },2).unwrap();
            let claim=self.journal.claim_effect(&self.owner,&receipt.command_id,3).unwrap().unwrap();
            self.journal.finish_effect(&claim,&result,4).unwrap();receipt.command_id
        }
    }

    #[test]
    fn surface_receipts_persist_first_application_and_refuse_request_conflicts_or_stale_owner() {
        let mut f=Fixture::new();let request=json!({"command":"accepted-command"});let result=json!({"applied":true});
        assert_eq!(f.journal.record_surface_receipt(&f.owner,"once","history",&request,&result,2).unwrap(),result);
        drop(f.journal);f.journal=Journal::open(&f.path).unwrap();
        assert_eq!(f.journal.record_surface_receipt(&f.owner,"once","history",&request,&json!({"applied":"duplicate"}),3).unwrap(),result);
        assert!(f.journal.record_surface_receipt(&f.owner,"once","history",&json!({"command":"different"}),&result,3).is_err());
        assert_eq!(f.journal.surface_receipts(&f.owner,"history",10).unwrap(),vec![result.clone()]);
        assert!(f.journal.surface_receipts(&f.owner,"history",0).is_err());
        assert!(f.journal.surface_receipts(&f.owner,"history",257).is_err());
        let new=f.journal.transfer_owner(&f.owner,"replacement",4).unwrap();
        assert!(f.journal.record_surface_receipt(&f.owner,"new","history",&request,&result,5).is_err());
        assert!(f.journal.surface_receipts(&f.owner,"history",10).is_err());
        assert_eq!(f.journal.surface_receipts(&new,"history",10).unwrap(),vec![result]);
    }

    #[test]
    fn surface_receipts_apply_durably_before_ack_and_retry_without_repeating_completed_effect() {
        let mut f=Fixture::new();let id=f.complete(json!({"backend_complete":true,"verification":"pending"}));
        // Abort only the acknowledgment transaction. The preceding durable
        // application must survive this exact disconnect/crash window.
        f.journal.conn.execute_batch("CREATE TRIGGER fail_surface_ack BEFORE INSERT ON events WHEN NEW.kind='command-results-acknowledged' BEGIN SELECT RAISE(ABORT,'simulated disconnect before acknowledgment'); END;").unwrap();
        assert!(f.journal.apply_surface_completions(&f.owner,"app-history",10,5).is_err());
        assert_eq!(f.journal.surface_receipts(&f.owner,"command-history-v1",10).unwrap().len(),1);
        assert_eq!(f.journal.completion_outbox(&f.owner,"app-history",10).unwrap().len(),1);
        drop(f.journal);f.journal=Journal::open(&f.path).unwrap();
        f.journal.conn.execute_batch("DROP TRIGGER fail_surface_ack").unwrap();
        let deliveries=f.journal.apply_surface_completions(&f.owner,"app-history",10,6).unwrap();
        assert_eq!(deliveries.len(),1);assert_eq!(deliveries[0].receipt.command_id,id);
        assert!(f.journal.completion_outbox(&f.owner,"app-history",10).unwrap().is_empty());
        assert_eq!(f.journal.surface_receipts(&f.owner,"command-history-v1",10).unwrap().len(),1);
        assert!(f.journal.apply_surface_completions(&f.owner,"app-history",10,7).unwrap().is_empty());
        assert!(f.journal.claim_effect(&f.owner,&id,8).unwrap().is_none());
        let started:i64=f.journal.conn.query_row("SELECT count(*) FROM events WHERE kind='effect-started'",[],|r|r.get(0)).unwrap();
        assert_eq!(started,1);
        // Another actual surface has its own durable cursor; one surface's
        // application never claims a peer or user acknowledged the result.
        assert_eq!(f.journal.completion_outbox(&f.owner,"other-history",10).unwrap().len(),1);
    }

    #[test]
    fn surface_completions_deduplicate_command_identity_across_outbox_sequences_and_legacy_keys() {
        let mut f=Fixture::new();let id=f.complete(json!({"completed":true}));
        let original=f.journal.completion_outbox(&f.owner,"history",1).unwrap().remove(0);
        // Previous clients could commit the sequence-keyed application and
        // crash before acknowledging. Preserve that application on upgrade.
        f.journal.record_surface_receipt(&f.owner,&format!("history:{}",original.sequence),
            "command-history-v1",&json!({"consumer":"history","sequence":original.sequence,"command_id":id}),
            &serde_json::to_value(&original).unwrap(),5).unwrap();
        let tx=f.journal.conn.transaction().unwrap();
        super::super::commands::publish_completion(&tx,&id,6).unwrap();tx.commit().unwrap();
        let replay=f.journal.apply_surface_completions(&f.owner,"history",10,7).unwrap();
        assert_eq!(replay.len(),2);assert_ne!(replay[0].sequence,replay[1].sequence);
        assert_eq!(replay[0].receipt.command_id,replay[1].receipt.command_id);
        assert_eq!(f.journal.surface_receipts(&f.owner,"command-history-v1",10).unwrap().len(),1);
        drop(f.journal);f.journal=Journal::open(&f.path).unwrap();
        assert!(f.journal.completion_outbox(&f.owner,"history",10).unwrap().is_empty());
        // Each consumer applies once, while repeated publication of the same
        // command never becomes a second command-history application.
        assert_eq!(f.journal.apply_surface_completions(&f.owner,"other",10,8).unwrap().len(),2);
        assert_eq!(f.journal.surface_receipts(&f.owner,"command-history-v1",10).unwrap().len(),2);
    }

    #[test]
    fn surface_receipts_bound_large_views_but_preserve_authoritative_result_and_wait_scope() {
        let mut f=Fixture::new();let result=json!({"output":"x".repeat(10000)});let id=f.complete(result.clone());
        let scope=f.journal.command_wait_scope(&f.owner,&id).unwrap();
        assert_eq!(scope.project_id,f.owner.project_id);assert_eq!(scope.owner_epoch,f.owner.epoch);
        assert_eq!(scope.run_id,"target");assert_eq!(scope.run_epoch,1);assert_eq!(scope.id,id);assert_eq!(scope.kind,WaitKind::Job);
        f.journal.apply_surface_completions(&f.owner,"history",1,5).unwrap();
        let views=f.journal.surface_receipts(&f.owner,"command-history-v1",1).unwrap();
        assert_eq!(views[0]["result_omitted"],true);assert!(views[0]["receipt"]["result"].is_null());
        assert!(serde_json::to_vec(&views[0]).unwrap().len()<8192);
        assert_eq!(f.journal.command_receipt(&f.owner,&id).unwrap().result,Some(result));
        assert!(f.journal.command_wait_scope(&f.owner,"not-accepted").is_err());
        let replacement=f.journal.transfer_owner(&f.owner,"replacement",6).unwrap();
        assert!(f.journal.command_wait_scope(&f.owner,&id).is_err());
        assert_eq!(f.journal.command_wait_scope(&replacement,&id).unwrap().run_epoch,1);
    }

    #[test]
    fn surface_receipts_recorded_inventory_reads_inactive_exact_owner_without_granting_authority() {
        let mut f=Fixture::new();let original=f.journal.recorded_run_inventory(&f.owner).unwrap();assert_eq!(original.len(),2);
        f.journal.record_surface_receipt(&f.owner,"retained-adoption","adoption",&json!({"owner":f.owner}),&json!({"proof":"durable"}),1).unwrap();
        f.journal.release_drained_owner(&f.owner,2).unwrap();let state=f.journal.project_state(&f.owner.project_id).unwrap();
        assert!(!state.owner_active);assert!(f.journal.recorded_run_inventory(&f.owner).is_err());
        assert!(f.journal.recorded_surface_receipt(&f.owner,"retained-adoption","adoption").is_err());
        assert_eq!(f.journal.recorded_surface_receipt(&state.owner,"retained-adoption","adoption").unwrap(),Some(json!({"proof":"durable"})));
        assert!(f.journal.recorded_surface_receipt(&state.owner,"missing","adoption").unwrap().is_none());
        let inactive=f.journal.recorded_run_inventory(&state.owner).unwrap();assert_eq!(inactive.len(),2);
        for run in inactive {assert_eq!(run.project_id,f.owner.project_id);assert_eq!(run.run_epoch,2);}
        assert!(f.journal.register_run(&state.owner,"new",3).is_err());
        assert!(f.journal.record_surface_receipt(&state.owner,"not-authority","history",&json!({}),&json!({}),3).is_err());
        let replacement=f.journal.transfer_recovered_owner(&state.owner,"next",4).unwrap();
        assert!(f.journal.recorded_run_inventory(&state.owner).is_err());
        assert_eq!(f.journal.recorded_run_inventory(&replacement).unwrap().len(),2);
    }
}

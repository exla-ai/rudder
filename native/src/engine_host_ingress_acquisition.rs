// Child of engine_host. Connected ingress-only acquisition: original actual
// lock/Journal survive every error/panic. No path/owner.json/readback restores
// the original transfer ACK. Legacy consuming acquisition stays unchanged.
use super::*;
use rudder_native::host_ingress::{HostIngressConfigOwner,AllowedPreparedProject};
use rudder_engine::execution::finalization_budget::FinalizationBudget;
use std::any::Any;

pub(crate) struct RetainedManagedAcquisition {
    owner:Option<EngineOwnership>,journal:Option<Journal>,held_journal:Option<HeldJournalFile>,lock:Option<CheckoutLock>,
    repo_anchor:Option<File>,home_anchor:Option<File>,repo_id:Option<(u64,u64)>,home_id:Option<(u64,u64)>,
    config:Arc<HostIngressConfigOwner>,selection:AllowedPreparedProject,original:FinalizationBudget,cancelled:Arc<AtomicBool>,
    attempted:bool,replacement:owner_restart::reconciliation::GuardedReplacement,acknowledged:bool,panic:Option<Box<dyn Any+Send>>,
    previous:Option<OwnerFence>,instance:Option<String>,
    restart_recovery:Option<restart::NativeRestartRecovery>,
}
impl RetainedManagedAcquisition {
    /// Inspection only, including after the original request deadline. Reading
    /// this report neither retries transfer nor acknowledges external custody.
    pub(crate) fn restart_recovery(&self)->Option<&restart::NativeRestartRecovery> {self.restart_recovery.as_ref()}
    /// The caller's actual machine effect-start ACK already exists. Alias
    /// selects only from this actual immutable config, not arbitrary request
    /// paths or a copied prepared receipt. Stage BEFORE filesystem effects.
    pub(crate) fn capture_retaining(out:&mut Option<Self>,config:Arc<HostIngressConfigOwner>,alias:&str,
        original:&FinalizationBudget,cancelled:Arc<AtomicBool>)->Result<()> {
        config.assert_current()?;original.remaining()?;anyhow::ensure!(out.is_none(),"original acquisition already captured");
        let selection=config.data().projects.iter().find(|p|p.alias==alias).context("project is outside actual host config")?.clone();
        for private in [&selection.home,&selection.cas,&selection.endpoints,&config.data().state_directory] {
            anyhow::ensure!(!private.starts_with(&selection.checkout)&&!selection.checkout.starts_with(private),"host private namespace overlaps configured source checkout");
        }
        *out=Some(Self {owner:None,journal:None,held_journal:None,lock:None,repo_anchor:None,home_anchor:None,repo_id:None,home_id:None,
            config,selection,original:original.clone(),cancelled,attempted:false,replacement:Default::default(),acknowledged:false,panic:None,restart_recovery:None,previous:None,instance:None});Ok(())
    }
    fn time(&self)->Result<()> {if self.cancelled.load(Ordering::Acquire){self.original.cancel();bail!("original ingress acquisition cancelled");}self.original.remaining()?;self.config.assert_current()}
    fn custody(&self)->Result<()> {
        self.time()?;if let Some(held)=&self.held_journal {held.check_current()?;}
        for (path,file,id) in [(&self.selection.checkout,&self.repo_anchor,self.repo_id),(&self.selection.home,&self.home_anchor,self.home_id)] {
            for m in [file.as_ref().context("original acquisition directory absent")?.metadata()?,fs::symlink_metadata(path)?] {
                anyhow::ensure!(m.is_dir()&&!m.file_type().is_symlink()&&Some((m.dev(),m.ino()))==id&&m.uid()==1500
                    &&m.gid()==1500&&m.mode()&0o022==0,"original configured project/home directory changed");
                if path==&self.selection.home {anyhow::ensure!(m.mode()&0o7777==0o700,"configured private home is not0700");}
            }
        }Ok(())
    }
    pub(crate) fn acquire_prepared_retaining(&mut self)->Result<()> {
        anyhow::ensure!(!self.attempted,"original acquisition already attempted; explicit recovery required");self.attempted=true;
        let result=std::panic::catch_unwind(std::panic::AssertUnwindSafe(||self.acquire_inner()));
        match result {Ok(result)=>result,Err(panic)=>{self.panic=Some(panic);Err(anyhow::anyhow!("project acquisition panicked; original lock/Journal retained"))}}
    }
    fn acquire_inner(&mut self)->Result<()> {
        self.time()?;let p=&self.selection;
        anyhow::ensure!(p.checkout.canonicalize()?==p.checkout&&p.home.canonicalize()?==p.home,"configured project/home redirects");
        self.repo_anchor=Some(OpenOptions::new().read(true).custom_flags(libc::O_DIRECTORY|libc::O_CLOEXEC|libc::O_NOFOLLOW).open(&p.checkout)?);
        self.home_anchor=Some(OpenOptions::new().read(true).custom_flags(libc::O_DIRECTORY|libc::O_CLOEXEC|libc::O_NOFOLLOW).open(&p.home)?);
        let m=self.repo_anchor.as_ref().unwrap().metadata()?;self.repo_id=Some((m.dev(),m.ino()));
        let m=self.home_anchor.as_ref().unwrap().metadata()?;self.home_id=Some((m.dev(),m.ino()));self.custody()?;
        // Own BEFORE any Journal transfer/recovery/owner-record write.
        self.lock=Some(acquire_checkout_lock(&self.selection.checkout)?);self.custody()?;
        let attach=self.selection.checkout.join(".rudder").join(ATTACH_SOCKET_NAME);
        anyhow::ensure!(!attach_is_live(&attach)?,"live legacy dashboard owns configured checkout");reject_legacy_writers(&self.selection.checkout)?;
        let paths=FabricPaths::new(&self.selection.home);
        let m=fs::symlink_metadata(&paths.journal)?;
        anyhow::ensure!(m.is_file()&&!m.file_type().is_symlink()&&m.uid()==1500&&m.nlink()==1&&m.mode()&0o7777==0o600,"prepared project needs its existing private Journal");
        self.journal=Some(Journal::open_with_busy_timeout(&paths.journal,Duration::from_millis(100))?);
        self.held_journal=Some(self.journal.as_ref().unwrap().hold_database_file()?);self.custody()?;
        let journal=self.journal.as_ref().unwrap();
        let project=existing_prepared_project(journal,&self.selection)?;
        self.previous=Some(journal.project_state(&project)?.owner);
        self.instance=Some(uuid::Uuid::new_v4().to_string());
        self.resume_prepared_inner(None)
    }
    /// Explicit OFF-App continuation of a refused acquisition. The caller
    /// supplies its actual original external protocol owner, not reconstructed
    /// paths, a copied receipt or an observer flag. No acquisition replay or new
    /// budget is created. Err leaves every carrier in this SAME value.
    pub(crate) fn reconcile_prepared_retaining(&mut self, external:&mut dyn owner_restart::reconciliation::OriginalExternalWork)->Result<()> {
        anyhow::ensure!(self.attempted&&self.previous.is_some()&&self.panic.is_none(),"original acquisition is not resumable");
        let result=std::panic::catch_unwind(std::panic::AssertUnwindSafe(||self.resume_prepared_inner(Some(external))));
        match result {Ok(result)=>result,Err(panic)=>{self.panic=Some(panic);Err(anyhow::anyhow!("project reconciliation panicked; original carriers retained"))}}
    }
    fn resume_prepared_inner(&mut self, external:Option<&mut dyn owner_restart::reconciliation::OriginalExternalWork>)->Result<()> {
        self.custody()?;
        let instance=self.instance.as_ref().context("original acquisition instance absent")?.clone();
        if self.owner.is_none() {
        anyhow::ensure!(self.lock.is_some(),"original checkout lock absent");
        let previous=self.previous.as_ref().context("original recorded owner absent")?;
        // Revalidate immutable configuration/prepare binding before callbacks
        // or transfer. This does not reconstruct external retirement authority.
        existing_prepared_project(self.journal.as_ref().context("original acquisition Journal absent")?,&self.selection)?;
        let fence=owner_restart::reconciliation::reconcile_locked_owner(self.journal.as_mut().unwrap(),previous,&format!("fabric:{instance}"),
            external,&self.original,&mut self.replacement,&mut self.restart_recovery)?;
        let lock=self.lock.take().unwrap();let journal=self.journal.take().unwrap();
        let record=OwnerRecord {format_version:ENGINE_FORMAT,repo:self.selection.checkout.clone(),authority:"fabric".into(),
            implementation:"managed-machine-ingress".into(),pid:std::process::id(),owner:RecordedFence::from(&fence),instance_id:instance,
            attach_socket:self.selection.checkout.join(".rudder").join(ATTACH_SOCKET_NAME),control_socket:None,state:"starting".into(),started_at:now_stamp(),control_nonce:None};
        let lifetime=Arc::new(EngineOwnerLifetime {lock:Arc::new(lock),assigned:None,repo:record.repo.clone(),owner:fence.clone(),authority:NativeAuthority::Fabric,
            process_id:std::process::id(),revoked:AtomicBool::new(false)});
        self.owner=Some(EngineOwnership {lifetime,journal,fence,record_path:record.repo.join(".rudder/engine/owner.json"),record,
            released:false,identity_home:self.selection.home.clone(),managed_store:None,migration_records:None,
            restart_recovery:self.restart_recovery.clone()});
        }
        self.custody()?;
        let actual=self.owner.as_mut().unwrap();
        write_owner_record(&actual.record_path,&actual.record)?;
        actual.prepare_managed_project(&self.selection.prepare)?;actual.assert_current()?;
        self.custody()?;self.acknowledged=true;Ok(())
    }
    /// Pure ownership transfer after all original ACKs; Err keeps SAME owner.
    pub(crate) fn take_acknowledged(&mut self)->Result<EngineOwnership> {
        self.custody()?;anyhow::ensure!(self.acknowledged&&self.replacement.acknowledgment().is_some()&&self.panic.is_none(),"original acquisition ACK unavailable");
        self.owner.take().context("original acquired owner already transferred")
    }
}

// Validation only, no owner mint/transfer. Actual entry invokes it ONLY while
// holding the original Root policy, directory anchors, lock and staged Journal.
fn existing_prepared_project(journal:&Journal,selection:&AllowedPreparedProject)->Result<String> {
    let project=local_project_id(&selection.checkout)?;let request=&selection.prepare;
    let namespace=crate::managed_store::namespace(&selection.home,&selection.checkout,&project,journal)?;
    anyhow::ensure!(request.namespace==namespace&&crate::managed_store::read_prepare_intent(&namespace,journal)?==*request
        &&journal.managed_project(&project)?.as_ref().is_some_and(|receipt|receipt.request==*request),"actual prepared Journal/intent differs from original request");
    anyhow::ensure!(crate::managed_store::inspect_experimental(&namespace)?.is_empty(),"historical/experimental project needs explicit separate recovery");Ok(project)
}
#[cfg(test)]
#[path="managed_ingress_prepared_validation_tests.rs"]
mod tests;

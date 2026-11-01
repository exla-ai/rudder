use super::*;
use std::{cell::RefCell,sync::mpsc,fs,os::unix::fs::DirBuilderExt};
use crate::engine_host::{EngineOwnership,NativeAuthority};
fn selected()->ManagedTaskOptIn {ManagedTaskOptIn::selected("fixture-api".into(),Backend::Codex,SelectedTaskCredentialKind::ApiKeyV1,ManagedTaskNetworkSelection::OrdinaryNetworkV1).unwrap()}
fn target()->ManagedTaskTarget {ManagedTaskTarget::validate("explicit-row".into(),"literal task".into(),"selected-model".into(),Backend::Codex,AgentMode::Execute,AgentStatus::Stopped).unwrap()}
#[test]
fn accepted_task_uses_same_original_budget_and_cancellation_without_another_deadline() {
    let c=ManagedTaskController::new();let original=FinalizationBudget::begin(Duration::from_secs(2)).unwrap();let cancelled=Arc::new(AtomicBool::new(false));
    let admitted=c.admit_original_target(target(),selected(),&original,cancelled.clone()).unwrap();assert_eq!(admitted.budget.constraint(),original.constraint());assert!(Arc::ptr_eq(&admitted.cancelled,&cancelled));
    original.cancel();let called=AtomicBool::new(false);assert!(admitted.load_profile(|_,_,_|{called.store(true,Ordering::Release);anyhow::bail!("must not run")}).is_err());assert!(!called.load(Ordering::Acquire));
    assert!(c.admit_original_target(target(),selected(),&original,cancelled).is_err());assert_eq!(c.active.load(Ordering::Acquire),1);drop(admitted);assert_eq!(c.active.load(Ordering::Acquire),0);
}
struct ActualProject {root:PathBuf,owner:EngineOwnership,project:Arc<ManagedTaskProject>}
impl ActualProject {fn new()->Self {
    let root=std::env::temp_dir().join(format!("rudder-ingress-factory-{}",uuid::Uuid::new_v4()));fs::DirBuilder::new().mode(0o700).create(&root).unwrap();let root=root.canonicalize().unwrap();let repo=root.join("repo");let home=root.join("home");
    for path in [&repo,&home]{fs::DirBuilder::new().mode(0o700).create(path).unwrap();}
    let mut owner=EngineOwnership::acquire_in_home(&repo,NativeAuthority::Fabric,&home).unwrap();let fence=owner.journal_parts().1.clone();let namespace=crate::managed_store::namespace(&home,&repo,&fence.project_id,owner.journal_parts().0).unwrap();
    owner.prepare_managed_project(&rudder_engine::journal::ManagedProjectPrepare{request_id:uuid::Uuid::new_v4().to_string(),namespace}).unwrap();let project=ManagedTaskProject::capture(&mut owner,&selected()).unwrap();Self{root,owner,project}
}}
impl Drop for ActualProject {fn drop(&mut self){let _=self.owner.finish(false);let _=fs::remove_dir_all(&self.root);}}
struct TlsGate {entered:mpsc::SyncSender<()>,release:mpsc::Receiver<()>}
impl Drop for TlsGate {fn drop(&mut self){self.entered.send(()).unwrap();self.release.recv().unwrap();}}
thread_local! {static TLS:RefCell<Option<TlsGate>>=const {RefCell::new(None)};}
#[test]
fn consuming_factory_error_keeps_charge_through_actual_original_thread_tls_join() {
    let actual=ActualProject::new();let project=actual.project.clone();
    let c=ManagedTaskController::new();let first=c.admit_target(target(),selected()).unwrap();let second=c.admit_target(target(),selected()).unwrap();let retained=first.retain_factory_until_join();
    let(entered,saw_enter)=mpsc::sync_channel(1);let(release,released)=mpsc::sync_channel(1);
    let worker=std::thread::spawn(move||{TLS.with(|tls|*tls.borrow_mut()=Some(TlsGate{entered,release:released}));
        assert!(first.prepare(&project,|_,_,_|anyhow::bail!("actual selected fixture profile refused"),|_,_,_|panic!("resources must not be reached")).is_err());
    });
    saw_enter.recv_timeout(Duration::from_secs(2)).unwrap();assert_eq!(c.active.load(Ordering::Acquire),2);assert!(c.admit_target(target(),selected()).is_err());
    release.send(()).unwrap();worker.join().unwrap();assert!(c.admit_target(target(),selected()).is_err(),"actual result capsule still retains its admission until explicit settlement");drop(retained);
    let third=c.admit_target(target(),selected()).unwrap();drop(third);drop(second);assert_eq!(c.active.load(Ordering::Acquire),0);
}

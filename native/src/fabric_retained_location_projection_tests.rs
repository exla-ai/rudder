use super::*;
use rudder_engine::execution::finalization_budget::FinalizationBudget;
fn fixture()->(crate::managed_store::tests::Fixture,crate::managed_store::ManagedRunLocation,crate::managed_launch::WorkspaceAssignment){
    let mut f=crate::managed_store::tests::Fixture::new();let store=f.prepare();let source=f.source("run");let location=store.run("run").unwrap();location.persist_reservation(&source).unwrap();fs::set_permissions(FabricPaths::new(&f.home).journal,fs::Permissions::from_mode(0o600)).unwrap();let assignment=crate::managed_store::tests::assignment(&source);(f,location,assignment)
}
fn budget()->FinalizationBudget{FinalizationBudget::begin(Duration::from_secs(5)).unwrap()}
#[test]
fn actual_retained_location_persists_exact_assignment_before_original_issuance_and_cleanup(){
    let (mut f,location,assignment)=fixture();let mut owner=AgentToolsProjectionOwner::new();let projected=prepare_agent_tools_for_location_retaining(&mut owner,&mut f.journal,Backend::Codex,&f.home,&assignment.fence,Some(&assignment),&location,&budget(),&AtomicBool::new(false)).unwrap();
    assert_eq!(location.assignment(&assignment.fence.attempt_id).unwrap(),Some(assignment.clone()));let identity:AgentIdentityFile=read_private(&projected.identity_file,16*1024).unwrap();assert!(identity.token==owner.current.as_ref().unwrap().acknowledged_token().unwrap());f.journal.authorize_workspace(&identity.token,&assignment.observation,epoch_ms().unwrap()).unwrap();owner.settle_unused(&mut f.journal,&budget()).unwrap();assert!(owner.is_settled());assert!(f.journal.authenticate_agent(&identity.token,epoch_ms().unwrap()).is_err());
}
#[test]
fn foreign_actual_location_cannot_supply_namespace_or_issue_a_token_for_original_writer(){
    let (mut original,location,assignment)=fixture();let (_foreign,other,other_assignment)=fixture();let mut owner=AgentToolsProjectionOwner::new();assert!(prepare_agent_tools_for_location_retaining(&mut owner,&mut original.journal,Backend::Codex,&original.home,&assignment.fence,Some(&assignment),&other,&budget(),&AtomicBool::new(false)).is_err());assert!(owner.started&&owner.pending.is_none()&&owner.current.is_none()&&owner.is_settled());assert!(location.assignment(&assignment.fence.attempt_id).unwrap().is_none());assert!(other.assignment(&other_assignment.fence.attempt_id).unwrap().is_none());owner.settle_unused(&mut original.journal,&budget()).unwrap();
    // Real local fixtures exercise projection/retirement; no Root/PG/fresh
    // namespace capability or host lease is fabricated by these tests.
}

use super::*;
use rudder_engine::{execution::finalization_budget::FinalizationBudget,
    journal::CapabilityIssuanceObservation};

fn budget() -> FinalizationBudget { FinalizationBudget::begin(Duration::from_secs(5)).unwrap() }
fn fixture() -> (crate::managed_store::tests::Fixture, crate::managed_launch::WorkspaceAssignment) {
    let mut f = crate::managed_store::tests::Fixture::new(); let store = f.prepare(); let source = f.source("run");
    store.run("run").unwrap().persist_reservation(&source).unwrap();
    fs::set_permissions(FabricPaths::new(&f.home).journal,fs::Permissions::from_mode(0o600)).unwrap();
    let assignment = crate::managed_store::tests::assignment(&source); (f,assignment)
}
fn prepare(f: &mut crate::managed_store::tests::Fixture, assignment: &crate::managed_launch::WorkspaceAssignment,
    owner: &mut AgentToolsProjectionOwner) -> Result<AgentToolsProjection> {
    prepare_agent_tools_for_attempt_retaining(owner,&mut f.journal,Backend::Codex,&f.home,&assignment.fence,
        Some(assignment),&budget(),&AtomicBool::new(false))
}

#[test]
fn never_bound_owner_is_settled_and_empty_cleanup_has_no_budget_or_journal_effect() {
    let (mut f,_) = fixture(); let mut owner = AgentToolsProjectionOwner::new(); assert!(owner.is_settled());
    let expired = budget(); expired.cancel(); owner.settle_unused(&mut f.journal,&expired).unwrap();
    assert!(owner.is_settled() && owner.fenced && owner.current.is_none() && owner.pending.is_none());
}

#[test]
fn acknowledged_projection_retains_original_grant_and_exact_unused_retirement() {
    let (mut f,assignment) = fixture(); let mut owner = AgentToolsProjectionOwner::new();
    let projection = prepare(&mut f,&assignment,&mut owner).unwrap(); assert!(!owner.is_settled());
    assert!(owner.pending.is_none() && owner.current.is_some());
    let token = owner.current.as_ref().unwrap().acknowledged_token().unwrap().to_owned();
    let identity: AgentIdentityFile = read_private(&projection.identity_file,16*1024).unwrap();
    assert!(identity.token == token); f.journal.authorize_workspace(&token,&assignment.observation,epoch_ms().unwrap()).unwrap();
    owner.settle_unused(&mut f.journal,&budget()).unwrap(); assert!(owner.is_settled());
    assert!(f.journal.authenticate_agent(&token,epoch_ms().unwrap()).is_err());
    assert_eq!(f.journal.observe_prepared_capability(owner.current.as_ref().unwrap()).unwrap(),CapabilityIssuanceObservation::Revoked);
    owner.settle_unused(&mut f.journal,&budget()).unwrap();
}

#[test]
fn actual_issuance_followed_by_projection_error_keeps_pending_token_and_forbids_retry() {
    let (mut f,assignment) = fixture(); let mut owner = AgentToolsProjectionOwner::new();
    fs::write(FabricPaths::new(&f.home).root.join("identities"),b"fixture blocks identity directory").unwrap();
    assert!(prepare(&mut f,&assignment,&mut owner).is_err());
    let original = owner.pending.as_ref().unwrap().acknowledged_token().unwrap().to_owned();
    assert!(!owner.is_settled() && owner.current.is_none());
    assert_eq!(f.journal.observe_prepared_capability(owner.pending.as_ref().unwrap()).unwrap(),CapabilityIssuanceObservation::Present);
    assert!(prepare(&mut f,&assignment,&mut owner).is_err());
    assert!(owner.pending.as_ref().unwrap().acknowledged_token().unwrap() == original);
    owner.settle_unused(&mut f.journal,&budget()).unwrap(); assert!(owner.is_settled());
    assert!(f.journal.authenticate_agent(&original,epoch_ms().unwrap()).is_err());
}

#[test]
fn cancelled_initial_call_creates_no_issuance_and_cannot_be_reused() {
    let (mut f,assignment) = fixture(); let mut owner = AgentToolsProjectionOwner::new();
    assert!(prepare_agent_tools_for_attempt_retaining(&mut owner,&mut f.journal,Backend::Codex,&f.home,
        &assignment.fence,Some(&assignment),&budget(),&AtomicBool::new(true)).is_err());
    assert!(owner.is_settled() && owner.pending.is_none() && owner.current.is_none());
    assert!(prepare(&mut f,&assignment,&mut owner).is_err()); owner.settle_unused(&mut f.journal,&budget()).unwrap();
}

#[test]
fn wrong_writer_does_not_retire_or_reconstruct_original_token() {
    let (mut f,assignment) = fixture(); let mut owner = AgentToolsProjectionOwner::new(); prepare(&mut f,&assignment,&mut owner).unwrap();
    let (mut foreign,_) = fixture(); assert!(owner.settle_unused(&mut foreign.journal,&budget()).is_err()); assert!(!owner.is_settled());
    assert_eq!(f.journal.observe_prepared_capability(owner.current.as_ref().unwrap()).unwrap(),CapabilityIssuanceObservation::Present);
    owner.settle_unused(&mut f.journal,&budget()).unwrap(); assert!(owner.is_settled());
}

#[test]
fn projection_files_are_not_cleanup_authority_and_expired_cleanup_retains_owner() {
    let (mut f,assignment) = fixture(); let mut owner = AgentToolsProjectionOwner::new(); let projection=prepare(&mut f,&assignment,&mut owner).unwrap();
    fs::remove_file(projection.identity_file).unwrap(); // The original opaque issuance is sufficient.
    let expired = budget(); expired.cancel(); assert!(owner.settle_unused(&mut f.journal,&expired).is_err()); assert!(!owner.is_settled());
    owner.settle_unused(&mut f.journal,&budget()).unwrap(); assert!(owner.is_settled());
}

#[test]
fn advanced_run_cannot_turn_unretired_original_issuance_into_settled_data() {
    let (mut f,assignment) = fixture(); let mut owner=AgentToolsProjectionOwner::new(); prepare(&mut f,&assignment,&mut owner).unwrap();
    f.journal.observe_run_attempt(&f.owner,"run",&uuid::Uuid::new_v4().to_string(),epoch_ms().unwrap()).unwrap();
    assert!(owner.settle_unused(&mut f.journal,&budget()).is_err()); assert!(!owner.is_settled() && owner.current.is_some());
}

#[test]
fn original_projection_owner_can_remain_in_off_app_supervisor_custody() {
    fn send<T: Send + 'static>() {} send::<AgentToolsProjectionOwner>();
}

// Exercise the production Journal/FS core using ORIGINAL prepared issuances.
// This does not fabricate an EngineSourceLease or test the separate host adapter.
fn rotation_fixture() -> (crate::managed_store::tests::Fixture, ProjectionPublication, Arc<SharedProjectionInner>) {
    let (mut f,assignment)=fixture(); let mut owner=AgentToolsProjectionOwner::new(); prepare(&mut f,&assignment,&mut owner).unwrap();
    let publication=owner.publication.take().unwrap();
    let current=RetainedProjection {issuance:owner.current.take().unwrap(),grant:publication.grant.clone(),issued_at:publication.issued_at,hash:Some(publication.hash)};
    let inner=Arc::new(SharedProjectionInner {state:std::sync::Mutex::new(SharedProjectionState {current,pending:None}),
        fenced:AtomicBool::new(false),settled:AtomicBool::new(false)});
    (f,publication,inner)
}
fn peers(f: &crate::managed_store::tests::Fixture) -> BTreeMap<String,u64> {
    f.journal.owned_runs(&f.owner).unwrap().into_iter().map(|run|(run.run_id,run.run_epoch)).collect()
}

#[test]
fn retained_peer_refresh_replaces_projection_then_retires_exact_old_issuance() {
    let (mut f,binding,inner)=rotation_fixture();
    let old={inner.state.lock().unwrap().current.issuance.acknowledged_token().unwrap().to_owned()};
    f.journal.register_run(&f.owner,"new-peer",epoch_ms().unwrap()).unwrap(); let recipients=peers(&f); let issued_at=epoch_ms().unwrap();
    let projection=refresh_projection_with_writer(&binding,&inner,&mut f.journal,&recipients,issued_at,&budget(),&AtomicBool::new(false)).unwrap().unwrap();
    let state=inner.state.lock().unwrap(); assert!(state.pending.is_none());
    assert_eq!(state.current.grant.recipients,recipients); assert_eq!(state.current.grant.expires_at_ms,Some(issued_at+24*60*60*1000));
    let current=state.current.issuance.acknowledged_token().unwrap(); assert!(current!=old);
    let record:AgentIdentityFile=read_private(&projection.identity_file,16*1024).unwrap(); assert!(record.token==current);
    assert!(f.journal.authenticate_agent(&old,epoch_ms().unwrap()).is_err());
    assert_eq!(f.journal.observe_prepared_capability(&state.current.issuance).unwrap(),CapabilityIssuanceObservation::Present);
    drop(state);retire_projection_with_writer(&inner,&mut f.journal,&budget()).unwrap();assert!(inner.settled.load(Ordering::Acquire));
}

#[test]
fn unchanged_peer_refresh_keeps_original_token_and_expiry_without_renewal() {
    let (mut f,binding,inner)=rotation_fixture();let recipients=peers(&f);
    let old={inner.state.lock().unwrap().current.issuance.acknowledged_token().unwrap().to_owned()};
    assert!(refresh_projection_with_writer(&binding,&inner,&mut f.journal,&recipients,epoch_ms().unwrap(),&budget(),&AtomicBool::new(false)).unwrap().is_none());
    let state=inner.state.lock().unwrap();assert!(state.pending.is_none());
    assert!(state.current.issuance.acknowledged_token().unwrap()==old);assert_eq!(state.current.grant.expires_at_ms,binding.grant.expires_at_ms);
    drop(state);retire_projection_with_writer(&inner,&mut f.journal,&budget()).unwrap();
}

#[test]
fn uncertain_projection_retains_current_plus_pending_and_blocks_a_third_issuance() {
    let (mut f,binding,inner)=rotation_fixture();let old={inner.state.lock().unwrap().current.issuance.acknowledged_token().unwrap().to_owned()};
    let directory=FabricPaths::new(&f.home).root.join("identities");fs::rename(&directory,directory.with_extension("retained-original")).unwrap();fs::write(&directory,b"fixture blocks new projection").unwrap();
    f.journal.register_run(&f.owner,"new-peer",epoch_ms().unwrap()).unwrap();let recipients=peers(&f);
    assert!(refresh_projection_with_writer(&binding,&inner,&mut f.journal,&recipients,epoch_ms().unwrap(),&budget(),&AtomicBool::new(false)).is_err());
    let pending={let state=inner.state.lock().unwrap();assert!(state.current.issuance.acknowledged_token().unwrap()==old);
        assert_eq!(f.journal.observe_prepared_capability(&state.current.issuance).unwrap(),CapabilityIssuanceObservation::Present);
        let p=&state.pending.as_ref().unwrap().issuance;assert_eq!(f.journal.observe_prepared_capability(p).unwrap(),CapabilityIssuanceObservation::Present);p.acknowledged_token().unwrap().to_owned()};
    f.journal.register_run(&f.owner,"another-peer",epoch_ms().unwrap()).unwrap();let changed=peers(&f);
    assert!(refresh_projection_with_writer(&binding,&inner,&mut f.journal,&changed,epoch_ms().unwrap(),&budget(),&AtomicBool::new(false)).is_err());
    assert!(inner.state.lock().unwrap().pending.as_ref().unwrap().issuance.acknowledged_token().unwrap()==pending);
    assert!(f.journal.authenticate_agent(&old,epoch_ms().unwrap()).is_ok());
    retire_projection_with_writer(&inner,&mut f.journal,&budget()).unwrap();assert!(inner.settled.load(Ordering::Acquire));
    assert!(f.journal.authenticate_agent(&old,epoch_ms().unwrap()).is_err());assert!(f.journal.authenticate_agent(&pending,epoch_ms().unwrap()).is_err());
}

#[test]
fn cancelled_or_stale_refresh_snapshot_does_not_create_a_pending_issuance() {
    let (mut f,binding,inner)=rotation_fixture();let stale=peers(&f);
    f.journal.register_run(&f.owner,"new-peer",epoch_ms().unwrap()).unwrap();let actual=peers(&f);
    assert!(refresh_projection_with_writer(&binding,&inner,&mut f.journal,&actual,epoch_ms().unwrap(),&budget(),&AtomicBool::new(true)).is_err());
    assert!(refresh_projection_with_writer(&binding,&inner,&mut f.journal,&stale,epoch_ms().unwrap(),&budget(),&AtomicBool::new(false)).is_err());
    assert!(inner.state.lock().unwrap().pending.is_none());retire_projection_with_writer(&inner,&mut f.journal,&budget()).unwrap();
}

#[test]
fn poisoned_projection_can_only_retire_originals_and_cannot_refresh_from_readback() {
    let (mut f,binding,inner)=rotation_fixture();let held=inner.clone();
    assert!(std::panic::catch_unwind(std::panic::AssertUnwindSafe(move||{let _guard=held.state.lock().unwrap();panic!("fixture projection poison");})).is_err());
    let recipients=peers(&f);assert!(refresh_projection_with_writer(&binding,&inner,&mut f.journal,&recipients,epoch_ms().unwrap(),&budget(),&AtomicBool::new(false)).is_err());
    retire_projection_with_writer(&inner,&mut f.journal,&budget()).unwrap();assert!(inner.settled.load(Ordering::Acquire));
}

#[test]
fn retired_shared_projection_is_absorbing_and_does_not_reissue_current() {
    let (mut f,binding,inner)=rotation_fixture();retire_projection_with_writer(&inner,&mut f.journal,&budget()).unwrap();
    let recipients=peers(&f);assert!(refresh_projection_with_writer(&binding,&inner,&mut f.journal,&recipients,epoch_ms().unwrap(),&budget(),&AtomicBool::new(false)).is_err());
    assert!(inner.state.lock().unwrap().pending.is_none());
}

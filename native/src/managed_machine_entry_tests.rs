//! Focused definitions, UNRUN. No fake Root client/ACK/owner constructor.
use super::*;
fn args(values:&[&str])->Vec<OsString> {values.iter().map(OsString::from).collect()}

#[test]
fn managed_family_is_claimed_before_smoke_and_malformed_suffix_can_never_fall_through() {
    assert!(route_selected(&args(&["engine","managed-machine","--smoke"]),false,false));
    assert!(route_selected(&args(&["engine","managed-machine","--repo","/user/path"]),false,false));
    assert!(!route_selected(&args(&["engine","serve"]),false,false));
    assert!(!route_selected(&args(&["--smoke"]),false,false));
    assert!(!route_selected(&args(&[]),false,false));
}
#[test]
fn protected_bridge_or_received_data_claims_routing_without_minting_authority() {
    for value in [args(&["--smoke"]),args(&["fabric","status"]),args(&[])] {
        assert!(route_selected(&value,true,false));assert!(route_selected(&value,false,true));
    }
    use std::os::unix::ffi::OsStringExt;
    assert!(!route_selected(&[OsString::from_vec(vec![255])],false,false));
}
fn selected()->ManagedTaskOptIn {ManagedTaskOptIn::selected("explicit-profile".into(),Backend::Codex,
    crate::managed_task_launch::SelectedTaskCredentialKind::ApiKeyV1,
    crate::managed_task_launch::ManagedTaskNetworkSelection::OrdinaryNetworkV1).unwrap()}
#[test]
fn display_needs_explicit_matching_backend_model_and_session_data() {
    let mut display=ManagedProjectDisplay {backend:Backend::Codex,model:"explicit-model".into(),effort:Some(EffortLevel::High),session_started_iso:"explicit-session-time".into()};
    display.check(&selected()).unwrap();display.model.clear();assert!(display.check(&selected()).is_err());
    display.model="explicit-model".into();display.backend=Backend::Claude;assert!(display.check(&selected()).is_err());
    display.backend=Backend::Codex;display.session_started_iso.clear();assert!(display.check(&selected()).is_err());
}
#[test]
fn original_request_cancellation_is_absorbing_without_a_fresh_worker_deadline() {
    let original=FinalizationBudget::begin(Duration::from_secs(2)).unwrap();let queued=original.clone();
    let cancelled=AtomicBool::new(false);time(&queued,&cancelled).unwrap();original.cancel();
    assert!(time(&queued,&cancelled).is_err());assert_eq!(queued.constraint(),original.constraint());
    let original=FinalizationBudget::begin(Duration::from_secs(2)).unwrap();cancelled.store(true,Ordering::Release);
    assert!(time(&original,&cancelled).is_err());
}
#[test]
fn failed_queue_delivery_keeps_the_actual_request_in_its_registered_cell() {
    struct Owner(Arc<std::sync::atomic::AtomicUsize>);
    impl Drop for Owner {fn drop(&mut self) {self.0.fetch_add(1,Ordering::AcqRel);}}
    let drops=Arc::new(std::sync::atomic::AtomicUsize::new(0));let actual=Cell::new(Owner(drops.clone()));
    let registered=actual.clone();let (send,receive)=mpsc::sync_channel(1);drop(receive);
    let returned=match send.try_send(actual) {Err(mpsc::TrySendError::Full(value))|Err(mpsc::TrySendError::Disconnected(value))=>value,
        Ok(())=>panic!("closed queue unexpectedly accepted owner")};registered.phase.store(RECOVERY,Ordering::Release);
    assert_eq!(drops.load(Ordering::Acquire),0);drop(returned);assert_eq!(drops.load(Ordering::Acquire),0);
    drop(registered);assert_eq!(drops.load(Ordering::Acquire),1);
}

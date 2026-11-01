//! Protocol/clock/thread tests, not a Root ACK or provider qualification issuer.
use super::*;
#[test]
fn qualified_safety_reserve_is_conservative_and_unconfigured_refuses() {
    let mut policy=HostOwnerLeaseSafety {qualification_receipt_sha256:"a".repeat(64),margin_milliseconds:250,engine_rate_error_ppm:1000,database_rate_error_ppm:1000,max_positive_clock_step_milliseconds:100};
    let reserve=policy.reserve_nanoseconds().unwrap();assert!(reserve>350_000_000&&reserve<500_000_000);
    policy.qualification_receipt_sha256="0".repeat(64);assert!(policy.reserve_nanoseconds().is_err());policy.qualification_receipt_sha256="a".repeat(64);policy.engine_rate_error_ppm=1_000_000;assert!(policy.reserve_nanoseconds().is_err());
}
#[test]
fn actual_budget_observation_preserves_same_original_anchor() {
    let original=FinalizationBudget::begin(Duration::from_secs(1)).unwrap();let first=original.observed_clock_bounds().unwrap();let clone=original.clone();let second=clone.observed_clock_bounds().unwrap();
    assert_eq!(first.0,second.0);assert!(second.1>=first.1);original.cancel();assert!(clone.observed_clock_bounds().is_err());
}
#[test]
fn renewal_cannot_switch_actual_owner_tuple() {
    let grant=Grant {format_version:1,authority_id:uuid::Uuid::new_v4().to_string(),account_id:"account".into(),project_id:uuid::Uuid::new_v4().to_string(),engine_instance_id:uuid::Uuid::new_v4().to_string(),machine_boot_id:"boot".into(),owner_epoch:"2".into(),lease_id:uuid::Uuid::new_v4().to_string(),renewal_serial:"0".into(),request_id:"acquire".into(),issued_at:"audit".into(),lease_until:"audit".into(),duration_ms:15000};
    let mut renewed=grant.clone();renewed.renewal_serial="1".into();renewed.request_id=uuid::Uuid::new_v4().to_string();assert!(grant.same_owner(&renewed));renewed.lease_id=uuid::Uuid::new_v4().to_string();assert!(!grant.same_owner(&renewed));renewed=grant.clone();renewed.engine_instance_id=uuid::Uuid::new_v4().to_string();assert!(!grant.same_owner(&renewed));
}
#[test]
fn actual_worker_join_waits_for_tls_after_body_completion() {
    use std::sync::mpsc;thread_local!{static HOLD:std::cell::RefCell<Option<TlsHold>>=const{std::cell::RefCell::new(None)};}
    struct TlsHold(std::sync::mpsc::Receiver<()>);impl Drop for TlsHold {fn drop(&mut self){let _=self.0.recv();}}
    let (release,held)=mpsc::channel();let (done,body_done)=mpsc::channel();let thread=std::thread::spawn(move||{HOLD.with(|v|*v.borrow_mut()=Some(TlsHold(held)));done.send(()).unwrap();});
    let original=OwnedWorkerJoin::new(thread);original.start_join_observer().unwrap();body_done.recv_timeout(Duration::from_secs(1)).unwrap();assert!(original.await_joined(Duration::from_millis(5)).is_err());release.send(()).unwrap();assert!(original.await_joined(Duration::from_secs(1)).is_ok());assert!(original.try_joined().unwrap().is_some());
}

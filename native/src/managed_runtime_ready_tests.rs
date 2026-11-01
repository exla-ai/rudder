//! Binary caller absence/role regressions, no Root client/ready ACK substitute.
use super::*;

#[test]
fn unknown_pending_machine_retains_no_ready_observation_or_snapshot() {
    let machine = MachineRuntimeImage::new(thread::current().id());
    assert!(machine.selected().is_err());
    assert!(machine.client.is_none() && machine.ready.is_none());
    assert!(machine.ready_observation_data().is_none());
}

#[test]
fn app_cannot_enter_observer_root_io_and_pure_snapshot_reads_do_not_create_owner() {
    let machine = MachineRuntimeImage::new(thread::current().id());
    assert!(machine.off_app().is_err());
    for _ in 0..3 { assert!(machine.ready_observation_data().is_none()); }
    assert!(machine.client.is_none() && machine.ready.is_none());
}

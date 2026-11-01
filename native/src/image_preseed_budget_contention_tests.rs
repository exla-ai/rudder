use super::*;
use std::{sync::mpsc,thread};
#[test]
fn actual_shared_original_clock_waits_for_held_short_lock_instead_of_fencing_healthy_import(){
    let budget=ImagePreseedBudget::begin(Duration::from_secs(5)).unwrap();let constraint=budget.constraint();let before=budget.remaining().unwrap();
    let held=budget.0.last.lock().unwrap();let (entered,begin)=mpsc::sync_channel(1);let (sent,result)=mpsc::sync_channel(1);let actual=budget.clone();
    let worker=thread::spawn(move||{entered.send(()).unwrap();let answer=actual.remaining();sent.send(answer).unwrap();});begin.recv_timeout(Duration::from_secs(2)).unwrap();
    // The test owns the actual contested resource throughout this interval.
    // The old try_lock path returns/fences here; no provider/clock injection,
    // test retry, implementation-mirrored mutex or widened lifetime is used.
    assert!(matches!(result.recv_timeout(Duration::from_millis(100)),Err(mpsc::RecvTimeoutError::Timeout)));
    assert!(!budget.0.cancelled.load(Ordering::Acquire));drop(held);let remaining=result.recv_timeout(Duration::from_secs(2)).unwrap().unwrap();worker.join().unwrap();
    assert!(remaining<before);assert_eq!(budget.constraint(),constraint);assert!(!budget.0.cancelled.load(Ordering::Acquire));budget.remaining().unwrap();
}
#[test]
fn cancellation_during_real_clock_lock_contention_stays_absorbing_after_unlock(){
    let budget=ImagePreseedBudget::begin(Duration::from_secs(5)).unwrap();let constraint=budget.constraint();let held=budget.0.last.lock().unwrap();let (entered,begin)=mpsc::sync_channel(1);let actual=budget.clone();let worker=thread::spawn(move||{entered.send(()).unwrap();actual.remaining()});
    begin.recv_timeout(Duration::from_secs(2)).unwrap();budget.cancel();drop(held);assert!(worker.join().unwrap().is_err());assert!(budget.remaining().is_err());assert_eq!(budget.constraint(),constraint);
}

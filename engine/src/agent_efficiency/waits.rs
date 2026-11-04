use crate::journal::CapabilityIdentity;
use anyhow::{ensure, Result};
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::sync::{mpsc, Arc, Mutex, MutexGuard, Weak};
use std::time::{Duration, Instant};

const MAX_TIMEOUT: Duration = Duration::from_secs(300);

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum WaitKind {
    Job,
    Release,
}

/// An observation key, not a credential. Construct only after the authority has
/// authenticated the existing identity and authorized the requested target.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct WaitScope {
    pub project_id: String,
    pub owner_epoch: u64,
    pub run_id: String,
    pub run_epoch: u64,
    pub kind: WaitKind,
    pub id: String,
}

impl WaitScope {
    pub fn from_identity(
        identity: &CapabilityIdentity,
        kind: WaitKind,
        id: String,
    ) -> Result<Self> {
        let scope = Self {
            project_id: identity.project_id.clone(),
            owner_epoch: identity.owner_epoch,
            run_id: identity.run_id.clone(),
            run_epoch: identity.run_epoch,
            kind,
            id,
        };
        scope.validate()?;
        Ok(scope)
    }

    fn validate(&self) -> Result<()> {
        for value in [&self.project_id, &self.run_id, &self.id] {
            ensure!(
                !value.is_empty() && value.len() <= 256 && !value.chars().any(char::is_control),
                "invalid wait scope identity"
            );
        }
        ensure!(
            self.owner_epoch > 0 && self.run_epoch > 0,
            "invalid wait epoch"
        );
        Ok(())
    }

    fn timeout(&self) -> WaitResult {
        WaitResult::Timeout {
            scope: self.clone(),
            name: match self.kind {
                WaitKind::Job => "job_wait_timeout",
                WaitKind::Release => "release_wait_timeout",
            }
            .to_owned(),
        }
    }

    fn cancelled(&self) -> WaitResult {
        WaitResult::Cancelled {
            scope: self.clone(),
            name: "wait_cancelled".to_owned(),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SettledOutcome {
    Succeeded,
    Failed,
    Cancelled,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "status", rename_all = "snake_case", deny_unknown_fields)]
pub enum WaitResult {
    Settled {
        scope: WaitScope,
        outcome: SettledOutcome,
    },
    Timeout {
        scope: WaitScope,
        name: String,
    },
    Cancelled {
        scope: WaitScope,
        name: String,
    },
}

struct Watch {
    deadline: Instant,
    sender: mpsc::SyncSender<WaitResult>,
}
struct Entry {
    admitted: bool,
    exited: bool,
    outcome: Option<SettledOutcome>,
    watchers: BTreeMap<u64, Watch>,
}
struct State {
    entries: BTreeMap<WaitScope, Entry>,
    waiters: usize,
    next_id: u64,
}
fn lock(state: &Mutex<State>) -> MutexGuard<'_, State> {
    state
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
}

#[derive(Clone)]
pub struct BoundedWaitRegistry {
    state: Arc<Mutex<State>>,
    max_waiters: usize,
    max_entries: usize,
}

impl Default for BoundedWaitRegistry {
    fn default() -> Self {
        Self::new(256, 1024).expect("positive fixed bounds")
    }
}

impl BoundedWaitRegistry {
    pub fn new(max_waiters: usize, max_entries: usize) -> Result<Self> {
        ensure!(
            max_waiters > 0 && max_entries > 0,
            "wait bounds must be positive"
        );
        Ok(Self {
            state: Arc::new(Mutex::new(State {
                entries: BTreeMap::new(),
                waiters: 0,
                next_id: 1,
            })),
            max_waiters,
            max_entries,
        })
    }

    fn ensure_entry(&self, state: &mut State, scope: &WaitScope) -> Result<()> {
        scope.validate()?;
        if state.entries.contains_key(scope) {
            return Ok(());
        }
        if state.entries.len() >= self.max_entries {
            // Authoritative callers replay a snapshot after an evicted result.
            // Live admission and active subscriptions are never evicted.
            let evict = state
                .entries
                .iter()
                .find(|(_, entry)| !entry.admitted && entry.watchers.is_empty())
                .map(|(key, _)| key.clone());
            if let Some(key) = evict {
                state.entries.remove(&key);
            }
        }
        ensure!(state.entries.len() < self.max_entries, "wait_entry_limit");
        state.entries.insert(
            scope.clone(),
            Entry {
                admitted: false,
                exited: false,
                outcome: None,
                watchers: BTreeMap::new(),
            },
        );
        Ok(())
    }

    /// Mirror an actual admission. This helper makes no admission decision.
    pub fn admit(&self, scope: &WaitScope) -> Result<()> {
        let mut state = lock(&self.state);
        self.ensure_entry(&mut state, scope)?;
        let entry = state.entries.get_mut(scope).expect("entry inserted");
        ensure!(!entry.exited, "cannot readmit an exited attempt");
        entry.admitted = true;
        Ok(())
    }

    pub fn is_admitted(&self, scope: &WaitScope) -> bool {
        lock(&self.state)
            .entries
            .get(scope)
            .is_some_and(|entry| entry.admitted)
    }

    /// The process owner calls this only after actual exit, never after task
    /// settlement, an interrupt request, a cancelled subscription or timeout.
    pub fn exited(&self, scope: &WaitScope) -> Result<()> {
        let mut state = lock(&self.state);
        self.ensure_entry(&mut state, scope)?;
        let entry = state.entries.get_mut(scope).expect("entry inserted");
        entry.admitted = false;
        entry.exited = true;
        Ok(())
    }

    /// Publish a committed authoritative observation. First observation wins;
    /// duplicate/conflicting observations must be reconciled by the caller.
    pub fn observe(&self, scope: &WaitScope, outcome: SettledOutcome) -> Result<bool> {
        let mut state = lock(&self.state);
        self.ensure_entry(&mut state, scope)?;
        let entry = state.entries.get_mut(scope).expect("entry inserted");
        if entry.outcome.is_some() {
            return Ok(false);
        }
        entry.outcome = Some(outcome);
        let watchers = std::mem::take(&mut entry.watchers);
        state.waiters -= watchers.len();
        for (_, watcher) in watchers {
            let result = if Instant::now() >= watcher.deadline {
                scope.timeout()
            } else {
                WaitResult::Settled {
                    scope: scope.clone(),
                    outcome,
                }
            };
            // One slot and one sender, removed before sending: never blocks.
            let _ = watcher.sender.try_send(result);
        }
        Ok(true)
    }

    /// Subscribe without blocking. Consume wait on a background worker, or use
    /// the native bridge; never wait on the App loop.
    pub fn subscribe(&self, scope: &WaitScope, timeout: Duration) -> Result<Subscription> {
        ensure!(
            timeout <= MAX_TIMEOUT,
            "wait timeout must be at most 300000 ms"
        );
        let mut state = lock(&self.state);
        self.ensure_entry(&mut state, scope)?;
        let (sender, receiver) = mpsc::sync_channel(1);
        let deadline = Instant::now() + timeout;
        let outcome = state.entries.get(scope).and_then(|entry| entry.outcome);
        let id = if let Some(outcome) = outcome {
            let _ = sender.try_send(WaitResult::Settled {
                scope: scope.clone(),
                outcome,
            });
            None
        } else if timeout.is_zero() {
            let _ = sender.try_send(scope.timeout());
            None
        } else {
            ensure!(state.waiters < self.max_waiters, "wait_subscription_limit");
            let id = state.next_id;
            state.next_id = id
                .checked_add(1)
                .ok_or_else(|| anyhow::anyhow!("wait subscription ids exhausted"))?;
            state
                .entries
                .get_mut(scope)
                .expect("entry inserted")
                .watchers
                .insert(id, Watch { deadline, sender });
            state.waiters += 1;
            Some(id)
        };
        Ok(Subscription {
            receiver,
            deadline,
            cancel: CancelWait {
                state: Arc::downgrade(&self.state),
                scope: scope.clone(),
                id,
            },
        })
    }

    pub fn pending_subscriptions(&self) -> usize {
        lock(&self.state).waiters
    }
}

/// Cancellation only settles one observation; it never interrupts a worker or
/// changes its authoritative lifecycle/admission.
#[derive(Clone)]
pub struct CancelWait {
    state: Weak<Mutex<State>>,
    scope: WaitScope,
    id: Option<u64>,
}
impl CancelWait {
    fn finish(&self, result: WaitResult) -> bool {
        let (Some(state), Some(id)) = (self.state.upgrade(), self.id) else {
            return false;
        };
        let mut state = lock(&state);
        let watcher = state
            .entries
            .get_mut(&self.scope)
            .and_then(|entry| entry.watchers.remove(&id));
        if let Some(watcher) = watcher {
            state.waiters -= 1;
            let _ = watcher.sender.try_send(result);
            true
        } else {
            false
        }
    }
    pub fn cancel(&self) -> bool {
        self.finish(self.scope.cancelled())
    }
}

pub struct Subscription {
    receiver: mpsc::Receiver<WaitResult>,
    deadline: Instant,
    cancel: CancelWait,
}
impl Subscription {
    pub fn cancellation(&self) -> CancelWait {
        self.cancel.clone()
    }
    /// One blocking event receive with a deadline, not repeated polling.
    pub fn wait(self) -> WaitResult {
        match self
            .receiver
            .recv_timeout(self.deadline.saturating_duration_since(Instant::now()))
        {
            Ok(result) => result,
            Err(mpsc::RecvTimeoutError::Disconnected) => self.cancel.scope.cancelled(),
            Err(mpsc::RecvTimeoutError::Timeout) => {
                self.cancel.finish(self.cancel.scope.timeout());
                // If publication won the lock, its one result is already sent.
                self.receiver
                    .recv()
                    .unwrap_or_else(|_| self.cancel.scope.cancelled())
            }
        }
    }
}
impl Drop for Subscription {
    fn drop(&mut self) {
        self.cancel.cancel();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn scope(kind: WaitKind) -> WaitScope {
        WaitScope {
            project_id: "project".into(),
            owner_epoch: 1,
            run_id: "run".into(),
            run_epoch: 1,
            kind,
            id: "task".into(),
        }
    }

    #[test]
    fn one_settled_result_does_not_release_admission() {
        let registry = BoundedWaitRegistry::new(2, 2).unwrap();
        let scope = scope(WaitKind::Job);
        registry.admit(&scope).unwrap();
        let subscription = registry.subscribe(&scope, Duration::from_secs(1)).unwrap();
        assert!(registry.observe(&scope, SettledOutcome::Succeeded).unwrap());
        assert!(!registry.observe(&scope, SettledOutcome::Failed).unwrap());
        assert!(matches!(
            subscription.wait(),
            WaitResult::Settled {
                outcome: SettledOutcome::Succeeded,
                ..
            }
        ));
        assert_eq!(registry.pending_subscriptions(), 0);
        assert!(registry.is_admitted(&scope));
        registry.exited(&scope).unwrap();
        assert!(!registry.is_admitted(&scope));
        assert!(registry.admit(&scope).is_err());
    }

    #[test]
    fn timeout_and_cancel_leave_actual_worker_admitted() {
        let registry = BoundedWaitRegistry::default();
        let scope = scope(WaitKind::Release);
        registry.admit(&scope).unwrap();
        assert!(
            matches!(registry.subscribe(&scope, Duration::ZERO).unwrap().wait(),
            WaitResult::Timeout { name, .. } if name == "release_wait_timeout")
        );
        let subscription = registry.subscribe(&scope, Duration::from_secs(1)).unwrap();
        let cancellation = subscription.cancellation();
        assert!(cancellation.cancel());
        assert!(!cancellation.cancel());
        assert!(
            matches!(subscription.wait(), WaitResult::Cancelled { name, .. } if name == "wait_cancelled")
        );
        assert!(registry.is_admitted(&scope));
        assert_eq!(registry.pending_subscriptions(), 0);
    }

    #[test]
    fn subscriptions_are_bounded_and_epoch_scoped() {
        let registry = BoundedWaitRegistry::new(1, 2).unwrap();
        let scope = scope(WaitKind::Job);
        let subscription = registry.subscribe(&scope, Duration::from_secs(1)).unwrap();
        assert!(registry.subscribe(&scope, Duration::from_secs(1)).is_err());
        let mut newer = scope.clone();
        newer.run_epoch += 1;
        registry.observe(&newer, SettledOutcome::Succeeded).unwrap();
        assert_eq!(registry.pending_subscriptions(), 1);
        subscription.cancellation().cancel();
        assert!(matches!(subscription.wait(), WaitResult::Cancelled { .. }));
        assert!(registry
            .subscribe(&newer, Duration::from_secs(301))
            .is_err());
    }

    #[test]
    fn dropping_subscription_frees_only_subscription_capacity() {
        let registry = BoundedWaitRegistry::new(1, 1).unwrap();
        let scope = scope(WaitKind::Job);
        registry.admit(&scope).unwrap();
        drop(registry.subscribe(&scope, Duration::from_secs(1)).unwrap());
        assert_eq!(registry.pending_subscriptions(), 0);
        assert!(registry.is_admitted(&scope));
        let mut other = scope.clone();
        other.id = "other".into();
        assert!(registry.subscribe(&other, Duration::ZERO).is_err());
    }
}

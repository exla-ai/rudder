//! Background observation bridge. The App loop drains typed messages and owns
//! all App/PTY mutations; waiting workers never borrow App or its terminals.

use anyhow::{ensure, Result};
use rudder_engine::agent_efficiency::{BoundedWaitRegistry, CancelWait, WaitResult, WaitScope};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{mpsc, Arc};
use std::time::Duration;

struct Permit(Arc<AtomicUsize>);
impl Drop for Permit {
    fn drop(&mut self) {
        self.0.fetch_sub(1, Ordering::AcqRel);
    }
}

pub(crate) struct WaitFinished {
    pub request_id: String,
    pub result: WaitResult,
    _permit: Permit,
}
impl WaitFinished {
    pub fn into_parts(self) -> (String, WaitResult) {
        (self.request_id, self.result)
    }
}

/// Bounded active receives plus unread results. It uses no polling thread and
/// no separate scheduler. Queue capacity and permits make result delivery
/// nonblocking even if the UI temporarily stops draining its completion queue.
pub(crate) struct AsyncWaitBridge {
    registry: BoundedWaitRegistry,
    pending: Arc<AtomicUsize>,
    max_pending: usize,
    sender: mpsc::SyncSender<WaitFinished>,
    receiver: mpsc::Receiver<WaitFinished>,
}

impl AsyncWaitBridge {
    pub fn new(registry: BoundedWaitRegistry, max_pending: usize) -> Result<Self> {
        ensure!(max_pending > 0, "wait bridge bound must be positive");
        let (sender, receiver) = mpsc::sync_channel(max_pending);
        Ok(Self {
            registry,
            pending: Arc::new(AtomicUsize::new(0)),
            max_pending,
            sender,
            receiver,
        })
    }

    /// Enqueue a background subscription and return immediately. `wake` should
    /// only notify the existing App event source; it must not mutate App.
    pub fn start(
        &self,
        request_id: String,
        scope: &WaitScope,
        timeout: Duration,
        wake: Arc<dyn Fn() + Send + Sync>,
    ) -> Result<CancelWait> {
        ensure!(
            !request_id.is_empty()
                && request_id.len() <= 128
                && !request_id.chars().any(char::is_control),
            "invalid wait request id"
        );
        self.pending
            .fetch_update(Ordering::AcqRel, Ordering::Acquire, |pending| {
                (pending < self.max_pending).then(|| pending + 1)
            })
            .map_err(|_| anyhow::anyhow!("wait_bridge_limit"))?;
        let permit = Permit(self.pending.clone());
        let subscription = self.registry.subscribe(scope, timeout)?;
        let cancellation = subscription.cancellation();
        let sender = self.sender.clone();
        std::thread::Builder::new()
            .name("rudder-bounded-wait".into())
            .spawn(move || {
                let message = WaitFinished {
                    request_id,
                    result: subscription.wait(),
                    _permit: permit,
                };
                // Every queued result retains its permit. With at most max_pending
                // total permits, this send always has room unless the bridge died.
                if sender.try_send(message).is_ok() {
                    wake();
                }
            })?;
        Ok(cancellation)
    }

    /// Call from the App loop on its existing background event notification.
    pub fn try_recv(&self) -> Option<WaitFinished> {
        self.receiver.try_recv().ok()
    }
    pub fn pending(&self) -> usize {
        self.pending.load(Ordering::Acquire)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use rudder_engine::agent_efficiency::{SettledOutcome, WaitKind};

    #[test]
    fn background_wait_returns_one_typed_message_and_retains_admission() {
        let registry = BoundedWaitRegistry::default();
        let scope = WaitScope {
            project_id: "project".into(),
            owner_epoch: 1,
            run_id: "run".into(),
            run_epoch: 1,
            kind: WaitKind::Job,
            id: "job".into(),
        };
        registry.admit(&scope).unwrap();
        let bridge = AsyncWaitBridge::new(registry.clone(), 1).unwrap();
        let (wake_sender, wake_receiver) = mpsc::sync_channel(1);
        let wake: Arc<dyn Fn() + Send + Sync> = Arc::new(move || {
            let _ = wake_sender.try_send(());
        });
        bridge
            .start(
                "request".into(),
                &scope,
                Duration::from_secs(1),
                wake.clone(),
            )
            .unwrap();
        assert!(bridge
            .start("too-many".into(), &scope, Duration::ZERO, wake.clone())
            .is_err());
        registry.observe(&scope, SettledOutcome::Succeeded).unwrap();
        wake_receiver.recv_timeout(Duration::from_secs(2)).unwrap();
        // A queued result still consumes bounded bridge capacity.
        assert_eq!(bridge.pending(), 1);
        assert!(bridge
            .start("still-full".into(), &scope, Duration::ZERO, wake)
            .is_err());
        let (request, result) = bridge.try_recv().unwrap().into_parts();
        assert_eq!(request, "request");
        assert!(matches!(
            result,
            WaitResult::Settled {
                outcome: SettledOutcome::Succeeded,
                ..
            }
        ));
        assert_eq!(bridge.pending(), 0);
        assert!(bridge.try_recv().is_none());
        assert!(registry.is_admitted(&scope));
    }
}

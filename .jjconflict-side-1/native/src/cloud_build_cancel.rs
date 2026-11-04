//! One helper invocation's original cancellation owner. Signal handlers only
//! set an atomic flag; the retained monitor calls the original control off the
//! signal stack and is joined before the helper reports a result.
use anyhow::{ensure, Context, Result};
use rudder_engine::build::BuildExecutionControl;
use std::{
    sync::{atomic::{AtomicBool, Ordering}, Arc},
    thread::{self, JoinHandle},
    time::Duration,
};

static OWNED: AtomicBool = AtomicBool::new(false);
static REQUESTED: AtomicBool = AtomicBool::new(false);

extern "C" fn request_stop(_: libc::c_int) {
    REQUESTED.store(true, Ordering::SeqCst);
}

pub(super) struct CancellationReport {
    pub requested: bool,
    pub publication_prevented: bool,
}

pub(super) struct BuildCancellation {
    original: Arc<BuildExecutionControl>,
    done: Arc<AtomicBool>,
    worker: Option<JoinHandle<Result<Option<bool>>>>,
    handlers: Vec<(libc::c_int, libc::sigaction)>,
    owns_signals: bool,
}

impl BuildCancellation {
    pub fn install(original: Arc<BuildExecutionControl>) -> Result<Self> {
        ensure!(OWNED.compare_exchange(false, true, Ordering::SeqCst, Ordering::SeqCst).is_ok(),
            "original build cancellation owner already installed");
        REQUESTED.store(false, Ordering::SeqCst);
        let mut owner = Self { original, done: Arc::new(AtomicBool::new(false)),
            worker: None, handlers: Vec::new(), owns_signals: true };
        for signal in [libc::SIGTERM, libc::SIGINT] {
            let mut action: libc::sigaction = unsafe { std::mem::zeroed() };
            action.sa_sigaction = request_stop as *const () as libc::sighandler_t;
            // No allocation, locking, Rust IO or control access in the handler.
            unsafe { libc::sigemptyset(&mut action.sa_mask); }
            let mut previous: libc::sigaction = unsafe { std::mem::zeroed() };
            let installed = unsafe { libc::sigaction(signal, &action, &mut previous) };
            ensure!(installed == 0, "install original build cancellation handler: {}", std::io::Error::last_os_error());
            owner.handlers.push((signal, previous));
        }
        let control = owner.original.clone();
        let done = owner.done.clone();
        owner.worker = Some(thread::Builder::new().name("build-cancellation".into()).spawn(move || {
            loop {
                if REQUESTED.load(Ordering::SeqCst) { return control.cancel().map(Some); }
                if done.load(Ordering::SeqCst) { return Ok(None); }
                thread::sleep(Duration::from_millis(2));
            }
        }).context("spawn original build cancellation monitor")?);
        Ok(owner)
    }

    pub fn check(&self) -> Result<()> {
        if REQUESTED.load(Ordering::SeqCst) {
            self.original.cancel()?;
            anyhow::bail!("managed build cancelled before execution; original control retained");
        }
        Ok(())
    }

    fn join_monitor(&mut self) -> Result<Option<bool>> {
        self.done.store(true, Ordering::SeqCst);
        match self.worker.take() {
            Some(worker) => worker.join().map_err(|_| anyhow::anyhow!("original build cancellation monitor panicked"))?,
            None => Ok(None),
        }
    }

    fn restore_handlers(&mut self) -> Result<()> {
        while let Some((signal, previous)) = self.handlers.last() {
            ensure!(unsafe { libc::sigaction(*signal, previous, std::ptr::null_mut()) } == 0,
                "restore original build cancellation handler: {}", std::io::Error::last_os_error());
            self.handlers.pop();
        }
        Ok(())
    }

    /// Call only after synchronous build execution and original daemon stop.
    /// cancel() may be waiting on publication's mutex; joining that same
    /// monitor preserves its acknowledgment rather than replacing the attempt.
    pub fn finish(mut self) -> Result<CancellationReport> {
        let observed = self.join_monitor();
        let restored = self.restore_handlers();
        // A signal can arrive while the monitor is joining. Restore first,
        // then include the last handler store before relinquishing ownership.
        let late = if REQUESTED.load(Ordering::SeqCst) { Some(self.original.cancel()?) } else { None };
        let acknowledged = late.or(observed?);
        restored?;
        Ok(CancellationReport { requested: acknowledged.is_some(), publication_prevented: acknowledged == Some(true) })
    }
}

impl Drop for BuildCancellation {
    fn drop(&mut self) {
        let _ = self.join_monitor();
        let restored = self.restore_handlers().is_ok();
        if self.owns_signals && restored {
            OWNED.store(false, Ordering::SeqCst);
            self.owns_signals = false;
        }
    }
}

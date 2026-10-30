//! PTY-backed terminal pane abstraction for the native Rudder app.
//!
//! `TerminalPane` intentionally keeps the process/PTY plumbing separate from
//! the rest of the app. It currently uses `portable-pty` for cross-platform PTY
//! creation and `vt100` for a plain-text terminal screen buffer. The public API
//! is small enough that the buffer backend can be replaced with
//! `alacritty_terminal` later without forcing app layout code to change.

use std::borrow::Cow;
use std::fmt;
use std::io::{Read, Write};
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::mpsc::{self, Receiver};
use std::sync::{Arc, Condvar, Mutex, OnceLock};

use crate::term_colors::{colors_generation, theme_change_report, QueryScanner};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

const EXIT_OUTPUT_GRACE: Duration = Duration::from_secs(2);
const MAX_OWNED_PTY_CHILDREN: usize = 512;
const REAP_POLL_INTERVAL: Duration = Duration::from_millis(10);

#[derive(Default)]
struct ReapObservation {
    status: Option<portable_pty::ExitStatus>,
    error: Option<String>,
}

/// Initialized before any child is created. Admission includes live children
/// and queued teardown; an uninterruptible child cannot grow this queue forever.
/// The worker owns every queued handle until a nonblocking wait proves reaping.
struct ChildReaper {
    jobs: Mutex<Vec<ReapJob>>,
    wake: Condvar,
    admitted: AtomicUsize,
    limit: usize,
    closing: AtomicBool,
}
struct ChildReservation {
    reaper: Arc<ChildReaper>,
}
impl Drop for ChildReservation {
    fn drop(&mut self) {
        self.reaper.admitted.fetch_sub(1, Ordering::AcqRel);
    }
}
struct ReapJob {
    child: Box<dyn Child + Send + Sync>,
    observation: Arc<Mutex<ReapObservation>>,
    _reservation: ChildReservation,
}
impl ChildReaper {
    fn start(limit: usize) -> std::io::Result<Arc<Self>> {
        let mut jobs = Vec::new();
        jobs.try_reserve_exact(limit)
            .map_err(std::io::Error::other)?;
        let reaper = Arc::new(Self {
            jobs: Mutex::new(jobs),
            wake: Condvar::new(),
            admitted: AtomicUsize::new(0),
            limit,
            closing: AtomicBool::new(false),
        });
        let worker = reaper.clone();
        // The worker has no App reference, scheduler, credentials or callbacks.
        // Its handle is deliberately detached; no UI path joins it.
        thread::Builder::new()
            .name("rudder-pty-reaper".into())
            .spawn(move || worker.run())?;
        Ok(reaper)
    }
    fn reserve(self: &Arc<Self>) -> Result<ChildReservation> {
        self.admitted
            .fetch_update(Ordering::AcqRel, Ordering::Acquire, |count| {
                (count < self.limit).then(|| count + 1)
            })
            .map_err(|_| {
                anyhow!("PTY child limit reached; waiting for owned children to be reaped")
            })?;
        Ok(ChildReservation {
            reaper: self.clone(),
        })
    }
    fn run(&self) {
        loop {
            let mut jobs = self
                .jobs
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner());
            while jobs.is_empty() {
                if self.closing.load(Ordering::Acquire) {
                    return;
                }
                jobs = self
                    .wake
                    .wait(jobs)
                    .unwrap_or_else(|poisoned| poisoned.into_inner());
            }
            let mut index = 0;
            while index < jobs.len() {
                // A bad child cannot take down the owner of other child handles.
                let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                    jobs[index].child.try_wait()
                }));
                match result {
                    Ok(Ok(Some(status))) => {
                        let job = jobs.swap_remove(index);
                        drop(job.child);
                        drop(job._reservation);
                        let mut observed =
                            job.observation.lock().unwrap_or_else(|p| p.into_inner());
                        observed.status = Some(status);
                        observed.error = None;
                        drop(observed);
                        continue;
                    }
                    Ok(Ok(None)) => {}
                    Ok(Err(error)) if error.kind() == std::io::ErrorKind::Interrupted => {}
                    error => {
                        let message = match error {
                            Ok(Err(error)) => error.to_string(),
                            _ => "PTY child status polling failed".to_string(),
                        };
                        jobs[index]
                            .observation
                            .lock()
                            .unwrap_or_else(|p| p.into_inner())
                            .error = Some(message);
                        // Keep ownership and admission; an error is not reaping.
                    }
                }
                index += 1;
            }
            drop(jobs);
            thread::sleep(REAP_POLL_INTERVAL);
        }
    }
}
fn child_reaper() -> Result<&'static Arc<ChildReaper>> {
    static REAPER: OnceLock<std::result::Result<Arc<ChildReaper>, String>> = OnceLock::new();
    REAPER
        .get_or_init(|| {
            ChildReaper::start(MAX_OWNED_PTY_CHILDREN).map_err(|error| error.to_string())
        })
        .as_ref()
        .map_err(|error| anyhow!("cannot initialize PTY child reaper: {error}"))
}

/// Own the child even if a later PTY initialization step fails. Normal polls
/// remain direct WNOHANG calls; only explicit/implicit teardown queues a handle.
struct OwnedPtyChild {
    child: Option<Box<dyn Child + Send + Sync>>,
    reservation: Option<ChildReservation>,
    pending: Option<Arc<Mutex<ReapObservation>>>,
    pid: Option<u32>,
    status: Option<portable_pty::ExitStatus>,
}
impl OwnedPtyChild {
    fn new(child: Box<dyn Child + Send + Sync>, reservation: ChildReservation) -> Self {
        Self {
            pid: child.process_id(),
            child: Some(child),
            reservation: Some(reservation),
            pending: None,
            status: None,
        }
    }
    fn try_wait(&mut self) -> std::io::Result<Option<portable_pty::ExitStatus>> {
        if let Some(status) = &self.status {
            return Ok(Some(status.clone()));
        }
        let status = if let Some(child) = self.child.as_mut() {
            child.try_wait()?
        } else if let Some(pending) = &self.pending {
            let observed = pending.lock().unwrap_or_else(|p| p.into_inner());
            if let Some(error) = &observed.error {
                return Err(std::io::Error::other(error.clone()));
            }
            observed.status.clone()
        } else {
            None
        };
        if let Some(status) = &status {
            self.status = Some(status.clone());
            self.reservation.take();
        }
        Ok(status)
    }
    fn process_id(&self) -> Option<u32> {
        if self.status.is_some()
            || self.pending.as_ref().is_some_and(|pending| {
                pending
                    .lock()
                    .unwrap_or_else(|p| p.into_inner())
                    .status
                    .is_some()
            })
        {
            None
        } else {
            self.pid
        }
    }
    fn request_termination(&mut self) {
        if self.child.is_none() {
            return;
        }
        if matches!(self.try_wait(), Ok(Some(_))) {
            self.child.take();
            return;
        }
        let mut child = self.child.take().expect("owned PTY child");
        #[cfg(unix)]
        if let Some(pid) = self.pid.filter(|pid| *pid > 1) {
            // The unreaped child still reserves its PID. Invalidate controls
            // before this method; never signal an already reaped/reused PID.
            unsafe {
                libc::kill(-(pid as libc::pid_t), libc::SIGTERM);
                libc::kill(-(pid as libc::pid_t), libc::SIGKILL);
            }
        }
        let _ = child.kill();
        let reservation = self.reservation.take().expect("live child admission");
        let reaper = reservation.reaper.clone();
        let observation = Arc::new(Mutex::new(ReapObservation::default()));
        self.pending = Some(observation.clone());
        reaper
            .jobs
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .push(ReapJob {
                child,
                observation,
                _reservation: reservation,
            });
        reaper.wake.notify_one();
    }
}
impl Drop for OwnedPtyChild {
    fn drop(&mut self) {
        self.request_termination();
    }
}

/// Wait only on the reader thread, with a bounded timeout so an inherited slave
/// cannot keep a stopped pane's reader alive. Darwin's poll does not reliably
/// support terminal devices; kqueue also avoids select's FD_SETSIZE limit.
#[cfg(unix)]
struct PtyReadiness {
    #[cfg(not(target_os = "macos"))]
    fd: Option<std::os::fd::RawFd>,
    #[cfg(target_os = "macos")]
    queue: Option<std::os::fd::OwnedFd>,
}

#[cfg(unix)]
impl PtyReadiness {
    fn new(fd: Option<std::os::fd::RawFd>) -> std::io::Result<Self> {
        #[cfg(target_os = "macos")]
        {
            use std::os::fd::{AsRawFd, FromRawFd};

            let queue = if let Some(fd) = fd {
                let raw_queue = unsafe { libc::kqueue() };
                if raw_queue < 0 {
                    return Err(std::io::Error::last_os_error());
                }
                // Own the queue immediately so every initialization error closes it.
                let queue = unsafe { std::os::fd::OwnedFd::from_raw_fd(raw_queue) };
                if unsafe { libc::fcntl(queue.as_raw_fd(), libc::F_SETFD, libc::FD_CLOEXEC) } < 0 {
                    return Err(std::io::Error::last_os_error());
                }
                let change = libc::kevent {
                    ident: fd as _,
                    filter: libc::EVFILT_READ,
                    flags: libc::EV_ADD | libc::EV_ENABLE,
                    fflags: 0,
                    data: 0,
                    udata: std::ptr::null_mut(),
                };
                let timeout = libc::timespec {
                    tv_sec: 0,
                    tv_nsec: 0,
                };
                if unsafe {
                    libc::kevent(
                        queue.as_raw_fd(),
                        &change,
                        1,
                        std::ptr::null_mut(),
                        0,
                        &timeout,
                    )
                } < 0
                {
                    return Err(std::io::Error::last_os_error());
                }
                Some(queue)
            } else {
                None
            };
            Ok(Self { queue })
        }
        #[cfg(not(target_os = "macos"))]
        Ok(Self { fd })
    }

    fn wait(&self) -> std::io::Result<bool> {
        #[cfg(target_os = "macos")]
        {
            use std::os::fd::AsRawFd;

            let Some(queue) = self.queue.as_ref() else {
                return Ok(true);
            };
            let mut event = std::mem::MaybeUninit::<libc::kevent>::uninit();
            let timeout = libc::timespec {
                tv_sec: 0,
                tv_nsec: 50_000_000,
            };
            let ready = unsafe {
                libc::kevent(
                    queue.as_raw_fd(),
                    std::ptr::null(),
                    0,
                    event.as_mut_ptr(),
                    1,
                    &timeout,
                )
            };
            if ready < 0 {
                return Err(std::io::Error::last_os_error());
            }
            if ready == 0 {
                return Ok(false);
            }
            // kevent initialized one event. EV_EOF still permits reading buffered
            // tail bytes; only an actual registration/runtime error ends the reader.
            let event = unsafe { event.assume_init() };
            if event.flags & libc::EV_ERROR != 0 && event.data != 0 {
                return Err(std::io::Error::from_raw_os_error(event.data as i32));
            }
            Ok(true)
        }
        #[cfg(not(target_os = "macos"))]
        {
            let Some(fd) = self.fd else {
                return Ok(true);
            };
            let mut poll = libc::pollfd {
                fd,
                events: libc::POLLIN,
                revents: 0,
            };
            let ready = unsafe { libc::poll(&mut poll, 1, 50) };
            if ready < 0 {
                Err(std::io::Error::last_os_error())
            } else {
                Ok(ready > 0)
            }
        }
    }
}

use anyhow::{anyhow, Context, Result};
use portable_pty::{Child, CommandBuilder, MasterPty, PtySize};

const DEFAULT_ROWS: u16 = 24;
const DEFAULT_COLS: u16 = 80;
/// Rows of history a pane keeps. 2,000 was too few for message navigation:
/// one Claude exchange in a real worker ran 1,674 rows, so ⌥v reached the
/// previous question and then hit the end of what was kept. Each retained
/// row costs ~36 bytes per column (styled cells), so 10,000 rows at 120
/// columns is ~43MB, paid only by a pane that actually fills it.
const DEFAULT_SCROLLBACK_LINES: usize = 10_000;

/// Callback invoked by a pane's reader thread each time it hands fresh PTY bytes
/// to the output channel, so the main event loop can wake and drain/redraw them
/// immediately instead of waiting for its next poll tick. The closure is expected
/// to coalesce (cheap, idempotent) — it may fire once per 8KB read burst.
pub type PtyOutputWaker = Arc<dyn Fn() + Send + Sync>;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct TerminalSize {
    pub rows: u16,
    pub cols: u16,
}

impl TerminalSize {
    pub fn new(rows: u16, cols: u16) -> Result<Self> {
        if rows == 0 || cols == 0 {
            return Err(anyhow!("terminal size must be non-zero"));
        }

        Ok(Self { rows, cols })
    }

    fn pty_size(self) -> PtySize {
        PtySize {
            rows: self.rows,
            cols: self.cols,
            pixel_width: 0,
            pixel_height: 0,
        }
    }
}

impl Default for TerminalSize {
    fn default() -> Self {
        Self {
            rows: DEFAULT_ROWS,
            cols: DEFAULT_COLS,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TerminalCommand {
    pub program: String,
    pub args: Vec<String>,
    pub env: Vec<(String, String)>,
}

impl TerminalCommand {
    pub fn new(program: impl Into<String>) -> Self {
        Self {
            program: program.into(),
            args: Vec::new(),
            env: Vec::new(),
        }
    }

    pub fn with_args(
        program: impl Into<String>,
        args: impl IntoIterator<Item = impl Into<String>>,
    ) -> Self {
        Self {
            program: program.into(),
            args: args.into_iter().map(Into::into).collect(),
            env: Vec::new(),
        }
    }

    pub fn with_env(mut self, key: impl Into<String>, value: impl Into<String>) -> Self {
        self.env.push((key.into(), value.into()));
        self
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TerminalPaneOptions {
    pub size: TerminalSize,
    pub cwd: Option<PathBuf>,
    pub scrollback_lines: usize,
    pub term: String,
    /// Render ONLY the live screen (the current vt100 grid), never accumulated
    /// scrollback. Set for the interactive plan-mode front-end: agent TUIs render
    /// inline (no alt-screen), so its startup welcome + redraw frames otherwise pile
    /// up in region_scrollback and the pane shows stacked duplicate banners. The
    /// model redraws its full visible UI each turn, so the live screen alone is the
    /// correct view; we trade PageUp history (irrelevant here) for a clean pane.
    pub live_screen_only: bool,
}

impl Default for TerminalPaneOptions {
    fn default() -> Self {
        Self {
            size: TerminalSize::default(),
            cwd: None,
            scrollback_lines: DEFAULT_SCROLLBACK_LINES,
            term: "xterm-256color".to_string(),
            live_screen_only: false,
        }
    }
}

pub struct TerminalPane {
    /// Stable for this process attempt. A resumed pane gets a fresh identity,
    /// even if the OS reuses a PID or the backend resumes the same conversation.
    instance_id: String,
    execution_profile: Option<String>,
    agent_link_launch_json: Option<String>,
    before_terminate: Option<Arc<dyn Fn() + Send + Sync>>,
    master: Box<dyn MasterPty + Send>,
    child: OwnedPtyChild,
    /// Shared with the reader thread, which answers the agent's color queries.
    writer: Arc<Mutex<Box<dyn Write + Send>>>,
    /// The agent asked for mode 2031 (appearance-change reports).
    wants_theme_reports: Arc<AtomicBool>,
    /// The terminal-colors generation this agent was last told about.
    theme_generation: u64,
    output_rx: Receiver<Vec<u8>>,
    output_eof: bool,
    exit_observed: Option<(portable_pty::ExitStatus, Instant)>,
    output_only: bool,
    terminated: bool,
    reader_thread: Option<JoinHandle<()>>,
    reader_stop: Arc<AtomicBool>,
    parser: vt100::Parser,
    size: TerminalSize,
    alternate_history: Vec<Vec<String>>,
    alternate_history_offset: usize,
    last_alternate_snapshot: Vec<String>,
    alternate_history_limit: usize,
    region_scrollback: Vec<Vec<StyledTerminalCell>>,
    region_scrollback_offset: usize,
    region_scrollback_limit: usize,
    /// Bumped whenever what the pane shows may have changed (every cache
    /// invalidation). An attached terminal compares it to skip re-rendering
    /// a pane that has not moved.
    render_generation: u64,
    tracked_scroll_region: Option<(u16, u16)>,
    ansi_state: AnsiTrackState,
    notification_scanner: OscNotificationScanner,
    /// The OSC 8 link currently open: its uri and where the cursor stood.
    open_hyperlink: Option<(String, (u16, u16))>,
    /// Text the agent wrapped in OSC 8 links, newest last. `vt100` drops the
    /// escapes, so this is the only record of where a link points.
    hyperlinks: std::collections::VecDeque<TerminalHyperlink>,
    styled_lines_cache: Option<Vec<Vec<StyledTerminalCell>>>,
    output_log: String,
    output_log_limit: usize,
    live_screen_only: bool,
    /// Optional waker shared with the reader thread. Kept behind a mutex so the
    /// app can install it after the pane is spawned (see [`set_output_waker`]).
    output_waker: Arc<Mutex<Option<PtyOutputWaker>>>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum AnsiTrackState {
    Ground,
    Escape,
    Csi(Vec<u8>),
}

/// A desktop-notification escape captured from worker output: OSC 777
/// (`ESC ] 777 ; notify ; title ; body`) or OSC 9 (`ESC ] 9 ; body`).
///
/// Claude Code emits these itself ("Claude needs your permission", "Claude is
/// waiting for your input"), but `vt100` consumes and silently drops OSCs it
/// does not model — so the backend's own notifications died inside the embedded
/// terminal and never reached the real emulator. The scanner below lifts them
/// out of the byte stream so the app can re-emit them to the outer terminal.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TerminalNotification {
    pub title: String,
    pub body: String,
}

/// Parse one OSC payload (the bytes between `ESC ]` and its terminator) as a
/// desktop notification. Returns `None` for every other OSC (titles, clipboard,
/// hyperlinks, ...).
pub fn parse_osc_notification(payload: &str) -> Option<TerminalNotification> {
    let (title, body) = if let Some(rest) = payload.strip_prefix("777;notify;") {
        // urxvt format is title;body — a lone field with no separator is a
        // bare message, not a title.
        match rest.split_once(';') {
            Some((title, body)) => (title, body),
            None => ("", rest),
        }
    } else if let Some(rest) = payload.strip_prefix("9;") {
        ("", rest)
    } else {
        return None;
    };
    if title.trim().is_empty() && body.trim().is_empty() {
        return None;
    }
    Some(TerminalNotification {
        title: title.trim().to_string(),
        body: body.trim().to_string(),
    })
}

/// Longest OSC payload the scanner accumulates. Anything larger (an OSC 52
/// clipboard blob, a runaway unterminated sequence) is truncated in place; the
/// scanner still tracks the sequence to its terminator so stream state stays
/// aligned with the real parser.
const OSC_SCAN_MAX_BYTES: usize = 4096;
/// Undrained notifications kept per pane; older ones are dropped first. A pane
/// is drained every tick while its process lives, so this only bounds panes
/// nobody is sweeping.
const MAX_PENDING_NOTIFICATIONS: usize = 16;

#[derive(Debug, Default)]
enum OscScanState {
    #[default]
    Ground,
    Escape,
    Osc(Vec<u8>),
    /// Saw ESC inside an OSC: either the start of the ST terminator (`ESC \`)
    /// or a malformed sequence.
    OscEscape(Vec<u8>),
}

/// Byte-at-a-time scanner that runs alongside `vt100`, capturing notification
/// OSCs the parser would drop. Chunk boundaries are irrelevant: state carries
/// across calls, so a sequence split anywhere still parses.
#[derive(Debug, Default)]
pub struct OscNotificationScanner {
    state: OscScanState,
    pending: Vec<TerminalNotification>,
    /// The OSC 8 hyperlink edge completed by the most recent byte, handed back
    /// by `scan` so the pane can read the cursor at exactly that point.
    hyperlink: Option<OscHyperlink>,
}

/// One edge of an OSC 8 hyperlink (`ESC ] 8 ; params ; uri ST`): a non-empty
/// uri opens a link, an empty one closes it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum OscHyperlink {
    Open(String),
    Close,
}

/// Parse one OSC payload as a hyperlink edge. `None` for every other OSC.
pub fn parse_osc_hyperlink(payload: &str) -> Option<OscHyperlink> {
    let rest = payload.strip_prefix("8;")?;
    let (_params, uri) = rest.split_once(';')?;
    let uri = uri.trim();
    if uri.is_empty() {
        Some(OscHyperlink::Close)
    } else {
        Some(OscHyperlink::Open(uri.to_string()))
    }
}

/// A run of on-screen text an agent marked as a hyperlink. Held as text rather
/// than cell coordinates because the cells move (scrolling, scrollback, Ink
/// repaints) while the text the user clicks on does not.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TerminalHyperlink {
    pub text: String,
    pub url: String,
}

/// Recent hyperlinks kept per pane. Older ones fall off first; a link that has
/// scrolled this far away is no longer on screen to be clicked.
const MAX_TERMINAL_HYPERLINKS: usize = 512;

impl OscNotificationScanner {
    /// Advance one byte. Returns the hyperlink edge this byte completed, if any.
    pub fn scan(&mut self, byte: u8) -> Option<OscHyperlink> {
        self.state = match std::mem::take(&mut self.state) {
            OscScanState::Ground if byte == 0x1b => OscScanState::Escape,
            OscScanState::Ground => OscScanState::Ground,
            OscScanState::Escape if byte == b']' => OscScanState::Osc(Vec::new()),
            OscScanState::Escape if byte == 0x1b => OscScanState::Escape,
            OscScanState::Escape => OscScanState::Ground,
            OscScanState::Osc(bytes) if byte == 0x07 => {
                self.complete(bytes);
                OscScanState::Ground
            }
            OscScanState::Osc(bytes) if byte == 0x1b => OscScanState::OscEscape(bytes),
            OscScanState::Osc(mut bytes) => {
                if bytes.len() < OSC_SCAN_MAX_BYTES {
                    bytes.push(byte);
                }
                OscScanState::Osc(bytes)
            }
            OscScanState::OscEscape(bytes) if byte == b'\\' => {
                self.complete(bytes);
                OscScanState::Ground
            }
            OscScanState::OscEscape(_) if byte == 0x1b => OscScanState::Escape,
            OscScanState::OscEscape(_) => OscScanState::Ground,
        };
        self.hyperlink.take()
    }

    fn complete(&mut self, bytes: Vec<u8>) {
        let payload = String::from_utf8_lossy(&bytes);
        if let Some(edge) = parse_osc_hyperlink(&payload) {
            self.hyperlink = Some(edge);
            return;
        }
        if let Some(note) = parse_osc_notification(&payload) {
            if self.pending.len() >= MAX_PENDING_NOTIFICATIONS {
                self.pending.remove(0);
            }
            self.pending.push(note);
        }
    }

    pub fn take(&mut self) -> Vec<TerminalNotification> {
        std::mem::take(&mut self.pending)
    }
}

/// The url under char column `col` of `line`. Agent-marked links win (newest
/// first, since the same text can point somewhere new later); otherwise a bare
/// `http(s)://` or `file://` url in the text itself, which is how Codex and
/// most CLI output print links.
pub fn hyperlink_at<'a>(
    line: &str,
    col: usize,
    links: impl Iterator<Item = &'a TerminalHyperlink>,
) -> Option<String> {
    let chars: Vec<char> = line.chars().collect();
    if col >= chars.len() {
        return None;
    }
    for link in links {
        let needle: Vec<char> = link.text.chars().collect();
        if needle.is_empty() || needle.len() > chars.len() {
            continue;
        }
        let first = col.saturating_sub(needle.len() - 1);
        let last = col.min(chars.len() - needle.len());
        if (first..=last).any(|start| chars[start..start + needle.len()] == needle[..]) {
            return Some(link.url.clone());
        }
    }
    bare_url_at(&chars, col)
}

fn bare_url_at(chars: &[char], col: usize) -> Option<String> {
    let is_break = |c: char| c.is_whitespace() || matches!(c, '<' | '>' | '"' | '`' | '│' | '|');
    if is_break(chars[col]) {
        return None;
    }
    let mut start = col;
    while start > 0 && !is_break(chars[start - 1]) {
        start -= 1;
    }
    let mut end = col + 1;
    while end < chars.len() && !is_break(chars[end]) {
        end += 1;
    }
    let token: String = chars[start..end].iter().collect();
    // The url may sit inside punctuation: "(see https://x.y/z)." or "[1]:https://…".
    let offset = ["https://", "http://", "file://"]
        .iter()
        .filter_map(|scheme| token.find(scheme))
        .min()?;
    let url_start = start + token[..offset].chars().count();
    if col < url_start {
        return None;
    }
    let mut url = token[offset..].to_string();
    // Trailing sentence punctuation is not part of the url; a closing paren is,
    // only when the url opened one (wikipedia-style links).
    loop {
        let Some(last) = url.chars().last() else {
            break;
        };
        let unbalanced_paren = last == ')' && url.matches('(').count() < url.matches(')').count();
        if matches!(last, '.' | ',' | ';' | ':' | '!' | '?' | '\'' | ']' | '}') || unbalanced_paren
        {
            url.pop();
        } else {
            break;
        }
    }
    if url.ends_with("://") {
        return None;
    }
    if col >= url_start + url.chars().count() {
        return None;
    }
    Some(url)
}

/// The number of bytes a single cell's grapheme cluster can occupy.
///
/// This mirrors `vt100`'s own fixed-size cell storage (`CONTENT_BYTES = 22`).
/// Because the parser physically cannot hand us more than that, inline storage
/// of the same width is lossless: there is no heap-fallback case to get wrong.
const CELL_CONTENT_BYTES: usize = 22;

/// One cell's text, stored inline instead of in a `String`.
///
/// The grid is re-materialized from the parser on every frame that follows new
/// output, so a `String` here meant one heap allocation per cell — roughly 9,300
/// mallocs per frame for a full-screen pane, which profiling showed dominating
/// render time. Inline storage makes the whole cell `Copy`, so rebuilding a row
/// and cloning the visible window are both plain memory copies.
#[derive(Clone, Copy, PartialEq, Eq)]
pub struct CellContents {
    bytes: [u8; CELL_CONTENT_BYTES],
    len: u8,
}

impl CellContents {
    pub const SPACE: Self = Self::from_ascii(b' ');

    const fn from_ascii(byte: u8) -> Self {
        let mut bytes = [0u8; CELL_CONTENT_BYTES];
        bytes[0] = byte;
        Self { bytes, len: 1 }
    }

    /// Copies `text` inline. Anything that would not fit is truncated on a char
    /// boundary rather than panicking or corrupting UTF-8; vt100 cannot produce
    /// such a value, so this only guards future callers.
    pub fn new(text: &str) -> Self {
        let mut end = text.len().min(CELL_CONTENT_BYTES);
        while end > 0 && !text.is_char_boundary(end) {
            end -= 1;
        }
        let mut bytes = [0u8; CELL_CONTENT_BYTES];
        bytes[..end].copy_from_slice(&text.as_bytes()[..end]);
        Self {
            bytes,
            len: end as u8,
        }
    }

    pub fn from_char(value: char) -> Self {
        let mut buffer = [0u8; 4];
        Self::new(value.encode_utf8(&mut buffer))
    }

    pub fn as_str(&self) -> &str {
        // Always valid: every constructor copies from an existing `&str` and
        // only ever cuts on a char boundary.
        str::from_utf8(&self.bytes[..self.len as usize]).unwrap_or_default()
    }

    pub fn is_empty(&self) -> bool {
        self.len == 0
    }

    /// Text for a ratatui `Span`, which wants `Cow<'static, str>`.
    ///
    /// The overwhelmingly common cell is a single ASCII byte, and those are
    /// served from a static table — so the render path allocates nothing at all
    /// for ordinary text, and only pays for wide characters and emoji.
    pub fn span_text(&self) -> Cow<'static, str> {
        if self.len == 1 {
            if let Some(text) = ascii_str(self.bytes[0]) {
                return Cow::Borrowed(text);
            }
        }
        Cow::Owned(self.as_str().to_string())
    }
}

/// `[0, 1, 2, ..., 127]`, so any single ASCII byte can be handed out as a
/// `&'static str` slice of length one without allocating.
static ASCII_BYTES: [u8; 128] = {
    let mut table = [0u8; 128];
    let mut index = 0;
    while index < 128 {
        table[index] = index as u8;
        index += 1;
    }
    table
};

fn ascii_str(byte: u8) -> Option<&'static str> {
    let index = byte as usize;
    let slice = ASCII_BYTES.get(index..index.checked_add(1)?)?;
    str::from_utf8(slice).ok()
}

impl std::ops::Deref for CellContents {
    type Target = str;

    fn deref(&self) -> &str {
        self.as_str()
    }
}

impl fmt::Debug for CellContents {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        fmt::Debug::fmt(self.as_str(), formatter)
    }
}

impl PartialEq<str> for CellContents {
    fn eq(&self, other: &str) -> bool {
        self.as_str() == other
    }
}

impl PartialEq<&str> for CellContents {
    fn eq(&self, other: &&str) -> bool {
        self.as_str() == *other
    }
}

impl From<&str> for CellContents {
    fn from(value: &str) -> Self {
        Self::new(value)
    }
}

/// A `Copy` cell with no heap tail: rebuilding a row and cloning the visible
/// window are both memory copies, and a scrollback row costs exactly its cells.
/// The assertion is here so a future field cannot quietly reintroduce a pointer
/// (and with it, a per-cell allocation) without someone noticing.
const _: () = assert!(std::mem::size_of::<StyledTerminalCell>() == 36);

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct StyledTerminalCell {
    pub contents: CellContents,
    pub fg: vt100::Color,
    pub bg: vt100::Color,
    pub bold: bool,
    pub dim: bool,
    pub italic: bool,
    pub underline: bool,
    pub inverse: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct TerminalCursor {
    pub row: u16,
    pub col: u16,
    pub visible: bool,
}

impl TerminalPane {
    /// Spawn the user's shell when `command` is `None`, or spawn the supplied
    /// program and arguments when it is `Some`.
    ///
    /// Output is collected by a background reader thread. Call
    /// [`drain_output`](Self::drain_output) to feed pending bytes into the
    /// terminal buffer, then [`visible_lines_snapshot`](Self::visible_lines_snapshot)
    /// to render the current screen as plain text without draining again.
    pub fn spawn_shell_or_command(
        command: Option<TerminalCommand>,
        options: TerminalPaneOptions,
    ) -> Result<Self> {
        // Reserve ownership and start the reaper before a fallible spawn can
        // leave a real child behind. Teardown never needs to spawn a thread.
        let child_reservation = child_reaper()?.reserve()?;
        let instance_id = command
            .as_ref()
            .and_then(|cmd| {
                cmd.env
                    .iter()
                    .rev()
                    .find(|(key, _)| key == "RUDDER_FABRIC_ATTEMPT_ID")
            })
            .and_then(|(_, value)| uuid::Uuid::parse_str(value).ok())
            .unwrap_or_else(uuid::Uuid::new_v4)
            .to_string();
        let launch_env = |name: &str| {
            command
                .as_ref()
                .and_then(|cmd| cmd.env.iter().rev().find(|(key, _)| key == name))
                .map(|(_, value)| value.clone())
        };
        let execution_profile = launch_env("RUDDER_EXECUTION_PROFILE");
        let agent_link_launch_json = launch_env("RUDDER_AGENT_LINK_LAUNCH");
        let pty_system = portable_pty::native_pty_system();
        let pair = pty_system
            .openpty(options.size.pty_size())
            .context("failed to open PTY")?;
        #[cfg(unix)]
        let reader_readiness = PtyReadiness::new(pair.master.as_raw_fd())
            .context("failed to initialize PTY reader readiness")?;

        let mut builder = command_builder(command, &options.term)?;
        if let Some(cwd) = &options.cwd {
            builder.cwd(cwd);
        }

        let child = pair
            .slave
            .spawn_command(builder)
            .context("failed to spawn command in PTY")?;
        let child = OwnedPtyChild::new(child, child_reservation);
        drop(pair.slave);

        let mut reader = pair
            .master
            .try_clone_reader()
            .context("failed to clone PTY reader")?;
        let writer = Arc::new(Mutex::new(
            pair.master
                .take_writer()
                .context("failed to open PTY writer")?,
        ));
        let reply_writer = Arc::clone(&writer);
        let wants_theme_reports = Arc::new(AtomicBool::new(false));
        let reader_wants_theme_reports = Arc::clone(&wants_theme_reports);
        let (output_tx, output_rx) = mpsc::channel();
        let output_waker: Arc<Mutex<Option<PtyOutputWaker>>> = Arc::new(Mutex::new(None));
        let reader_waker = Arc::clone(&output_waker);
        let reader_stop = Arc::new(AtomicBool::new(false));
        let stopped = Arc::clone(&reader_stop);
        let reader_thread = thread::Builder::new()
            .name("rudder-pty-reader".to_string())
            .spawn(move || {
                let mut buf = [0_u8; 8192];
                // Answered here, not on the main loop: an unfocused pane is only
                // drained every 500ms, and an agent asking for the terminal's
                // colors at startup gives up long before that.
                let mut queries = QueryScanner::default();
                loop {
                    if stopped.load(Ordering::Acquire) {
                        break;
                    }
                    #[cfg(unix)]
                    {
                        let ready = reader_readiness.wait();
                        if stopped.load(Ordering::Acquire) {
                            break;
                        }
                        match ready {
                            Ok(true) => {}
                            Ok(false) => continue,
                            Err(err) if err.kind() == std::io::ErrorKind::Interrupted => continue,
                            Err(_) => break,
                        }
                    }
                    match reader.read(&mut buf) {
                        Ok(0) => break,
                        Ok(n) => {
                            let replies = queries.scan(&buf[..n]);
                            reader_wants_theme_reports
                                .store(queries.wants_theme_reports, Ordering::Relaxed);
                            if !replies.is_empty() {
                                if let Ok(mut writer) = reply_writer.lock() {
                                    let _ =
                                        writer.write_all(&replies).and_then(|()| writer.flush());
                                }
                            }
                            if output_tx.send(buf[..n].to_vec()).is_err() {
                                break;
                            }
                            // Nudge the main event loop so freshly-arrived bytes are
                            // drained and drawn immediately, instead of waiting up to a
                            // full tick. The waker itself coalesces, so firing per read
                            // burst is cheap; a poisoned lock just skips the wake and
                            // falls back to the tick cadence.
                            if let Ok(guard) = reader_waker.lock() {
                                if let Some(waker) = guard.as_ref() {
                                    waker();
                                }
                            }
                        }
                        // A transient interruption (e.g. EINTR from a signal) is NOT
                        // end-of-stream: retry rather than tearing down the reader, or a
                        // still-running agent's output would freeze and completion would
                        // hang on the scrape fallback. Only a real EOF/error ends the loop.
                        Err(err) if err.kind() == std::io::ErrorKind::Interrupted => continue,
                        Err(_) => break,
                    }
                }
                // Publish EOF only after the final chunk, then wake the actor
                // even when the last read produced no bytes.
                drop(output_tx);
                if let Ok(guard) = reader_waker.lock() {
                    if let Some(waker) = guard.as_ref() {
                        waker();
                    }
                }
            })
            .context("failed to start PTY reader thread")?;

        Ok(Self {
            instance_id,
            execution_profile,
            agent_link_launch_json,
            before_terminate: None,
            master: pair.master,
            child,
            writer,
            wants_theme_reports,
            theme_generation: colors_generation(),
            output_rx,
            output_eof: false,
            exit_observed: None,
            output_only: false,
            terminated: false,
            reader_thread: Some(reader_thread),
            reader_stop,
            parser: vt100::Parser::new(
                options.size.rows,
                options.size.cols,
                options.scrollback_lines,
            ),
            size: options.size,
            alternate_history: Vec::new(),
            alternate_history_offset: 0,
            last_alternate_snapshot: Vec::new(),
            alternate_history_limit: options.scrollback_lines.max(DEFAULT_ROWS as usize),
            region_scrollback: Vec::new(),
            region_scrollback_offset: 0,
            region_scrollback_limit: options.scrollback_lines,
            render_generation: 0,
            tracked_scroll_region: None,
            ansi_state: AnsiTrackState::Ground,
            notification_scanner: OscNotificationScanner::default(),
            open_hyperlink: None,
            hyperlinks: std::collections::VecDeque::new(),
            styled_lines_cache: None,
            output_log: String::new(),
            output_log_limit: 200_000,
            live_screen_only: options.live_screen_only,
            output_waker,
        })
    }

    pub fn instance_id(&self) -> &str {
        &self.instance_id
    }

    pub fn execution_profile(&self) -> Option<&str> {
        self.execution_profile.as_deref()
    }

    pub fn agent_link_launch_json(&self) -> Option<&str> {
        self.agent_link_launch_json.as_deref()
    }

    /// Invalidate control capabilities before any PTY process-tree teardown,
    /// including implicit drop during replacement or deletion.
    pub fn set_before_terminate(&mut self, callback: Arc<dyn Fn() + Send + Sync>) {
        self.before_terminate = Some(callback);
    }

    /// Install (or replace) the waker the reader thread fires after each output
    /// send. Called by the app right after spawning a pane so PTY output wakes the
    /// main loop; panes never given a waker simply fall back to the tick cadence.
    pub fn set_output_waker(&self, waker: PtyOutputWaker) {
        if let Ok(mut guard) = self.output_waker.lock() {
            *guard = Some(waker);
        }
    }

    pub fn write_input(&mut self, bytes: &[u8]) -> Result<()> {
        if self.output_only {
            return Err(anyhow!("command finished; retained output is read-only"));
        }
        if self.terminated {
            return Err(anyhow!("process teardown requested; input unavailable"));
        }
        let write_result = match self.writer.lock() {
            Ok(mut writer) => writer.write_all(bytes).and_then(|()| writer.flush()),
            Err(_) => Err(std::io::Error::other("PTY writer lock poisoned")),
        };
        match write_result {
            Ok(()) => Ok(()),
            Err(err) => {
                if let Some(reason) = self.child_exit_summary() {
                    Err(anyhow!("agent process exited ({reason})"))
                } else {
                    Err(anyhow::Error::new(err).context("failed to write input to PTY"))
                }
            }
        }
    }

    fn child_exit_summary(&mut self) -> Option<String> {
        match self.try_wait() {
            Ok(Some(status)) => {
                let code = status.exit_code();
                if status.success() {
                    Some("exit 0".to_string())
                } else {
                    Some(describe_exit_code(code))
                }
            }
            _ => None,
        }
    }

    pub fn is_alive(&mut self) -> bool {
        match self.try_wait() {
            Ok(None) => true,
            Ok(Some(_)) => false,
            Err(_) => self.child.process_id().is_some(),
        }
    }

    pub fn resize(&mut self, size: TerminalSize) -> Result<()> {
        self.master
            .resize(size.pty_size())
            .context("failed to resize PTY")?;
        self.parser.screen_mut().set_size(size.rows, size.cols);
        self.size = size;
        self.last_alternate_snapshot.clear();
        self.tracked_scroll_region = None;
        // Region scrollback rows were captured at the old column width; rendering
        // them under the new geometry splices mismatched-width rows. Drop the
        // captured history and offset so the post-resize view is consistent.
        self.region_scrollback.clear();
        self.region_scrollback_offset = 0;
        self.invalidate_render_cache();
        Ok(())
    }

    /// Drain all currently available process output into the terminal buffer.
    ///
    /// The returned bytes are raw terminal output for logging/debugging. The
    /// in-memory buffer is updated before this method returns.
    pub fn drain_output(&mut self) -> Vec<u8> {
        self.report_theme_change();
        let mut drained = Vec::new();
        loop {
            match self.output_rx.try_recv() {
                Ok(chunk) => {
                    self.process_output_chunk(&chunk);
                    self.append_output_log(&chunk);
                    drained.extend_from_slice(&chunk);
                }
                Err(mpsc::TryRecvError::Disconnected) => {
                    self.output_eof = true;
                    break;
                }
                Err(mpsc::TryRecvError::Empty) => break,
            }
        }
        if !drained.is_empty() {
            self.invalidate_render_cache();
        }
        drained
    }

    /// The terminal's colors changed (light/dark): tell an agent that asked.
    fn report_theme_change(&mut self) {
        let generation = colors_generation();
        if generation == self.theme_generation {
            return;
        }
        self.theme_generation = generation;
        if !self.wants_theme_reports.load(Ordering::Relaxed) {
            return;
        }
        if let Some(report) = theme_change_report() {
            let _ = self.write_input(&report);
        }
    }

    /// Return the current visible terminal rows as plain text.
    ///
    /// This omits attributes, cursor shape, selection state, and image/graphics
    /// protocols. Those are the main reasons the interface is isolated: a later
    /// `alacritty_terminal` backend can preserve richer cell metadata while
    /// keeping callers on this app-facing API.
    pub fn visible_lines(&mut self) -> Vec<String> {
        self.drain_output();
        self.visible_lines_snapshot()
    }

    /// Whether `needle` appears on the child's live screen right now — the
    /// current rows, regardless of any scrollback offset Rudder's own viewport
    /// is showing. Drains pending output first so the answer reflects the
    /// latest repaint. Used to read a TUI's mode off its own chrome (e.g. the
    /// footer Claude Code shows while its transcript view is open).
    pub fn live_screen_contains(&mut self, needle: &str) -> bool {
        self.live_screen_lines()
            .iter()
            .any(|line| line.contains(needle))
    }

    /// The child's live screen rows right now (see `live_screen_contains`).
    pub fn live_screen_lines(&mut self) -> Vec<String> {
        self.drain_output();
        self.current_visible_lines_snapshot()
    }

    pub fn visible_lines_snapshot(&self) -> Vec<String> {
        // Plan-mode front-end: always the live screen, no scrollback (see the field doc).
        if self.live_screen_only {
            return self.current_visible_lines_snapshot();
        }
        if let Some(snapshot) = self.alternate_history_snapshot() {
            return snapshot.clone();
        }

        if self.region_scrollback_offset == 0 {
            return self.current_visible_lines_snapshot();
        }

        self.region_scrollback_view(self.current_visible_lines_snapshot(), |cells| {
            styled_cells_to_plain(cells)
        })
    }

    pub fn output_log_snapshot(&self) -> &str {
        &self.output_log
    }

    fn current_visible_lines_snapshot(&self) -> Vec<String> {
        self.parser
            .screen()
            .rows(0, self.size.cols)
            .map(|line| line.trim_end_matches(' ').to_string())
            .collect()
    }

    pub fn styled_lines(&mut self) -> Vec<Vec<StyledTerminalCell>> {
        self.drain_output();
        self.styled_lines_snapshot()
    }

    pub fn styled_lines_snapshot(&mut self) -> Vec<Vec<StyledTerminalCell>> {
        if self.styled_lines_cache.is_none() {
            self.styled_lines_cache = Some(self.compute_styled_lines_snapshot());
        }
        self.styled_lines_cache.clone().unwrap_or_default()
    }

    pub fn styled_line_window_snapshot(
        &mut self,
        height: usize,
    ) -> (usize, Vec<Vec<StyledTerminalCell>>) {
        if self.styled_lines_cache.is_none() {
            self.styled_lines_cache = Some(self.compute_styled_lines_snapshot());
        }
        let Some(rows) = self.styled_lines_cache.as_ref() else {
            return (0, Vec::new());
        };
        let start = rows.len().saturating_sub(height);
        (start, rows[start..].iter().take(height).cloned().collect())
    }

    /// The last `height` rows that have anything on them, ending at the
    /// lowest non-blank row (or the cursor row, whichever is lower), for a
    /// thumbnail of the pane. A screen taller than its content keeps its
    /// output and composer at the top, so the plain bottom window would be
    /// blank. Reuses the render cache; cells are `Copy`, nothing per cell
    /// allocates.
    pub fn styled_tail_window_snapshot(&mut self, height: usize) -> Vec<Vec<StyledTerminalCell>> {
        if self.styled_lines_cache.is_none() {
            self.styled_lines_cache = Some(self.compute_styled_lines_snapshot());
        }
        let Some(rows) = self.styled_lines_cache.as_ref() else {
            return Vec::new();
        };
        let content_end = rows
            .iter()
            .rposition(|row| row.iter().any(|cell| cell.contents.as_str().trim() != ""))
            .map(|last| last + 1)
            .unwrap_or(0);
        // The cursor row counts as content only on the live screen (no
        // scrollback offset), where screen rows and cache rows line up.
        let cursor_end = if rows.len() == self.size.rows as usize {
            (self.parser.screen().cursor_position().0 as usize + 1).min(rows.len())
        } else {
            0
        };
        let end = content_end.max(cursor_end);
        let start = end.saturating_sub(height);
        rows[start..end].to_vec()
    }

    fn compute_styled_lines_snapshot(&self) -> Vec<Vec<StyledTerminalCell>> {
        // Plan-mode front-end: always the live screen, no scrollback (see the field doc).
        if self.live_screen_only {
            return self.current_styled_screen_rows();
        }
        if let Some(snapshot) = self.alternate_history_snapshot() {
            return snapshot
                .iter()
                .map(|line| {
                    line.chars()
                        .map(|ch| StyledTerminalCell::plain(CellContents::from_char(ch)))
                        .collect()
                })
                .collect();
        }

        let current = self.current_styled_screen_rows();
        if self.region_scrollback_offset > 0 {
            return self.region_scrollback_view(current, Clone::clone);
        }

        current
    }

    fn current_styled_screen_rows(&self) -> Vec<Vec<StyledTerminalCell>> {
        let mut rows = Vec::with_capacity(self.size.rows as usize);

        for row in 0..self.size.rows {
            rows.push(self.styled_screen_row(row));
        }

        rows
    }

    fn styled_screen_row(&self, row: u16) -> Vec<StyledTerminalCell> {
        let screen = self.parser.screen();
        let mut cells = Vec::with_capacity(self.size.cols as usize);
        for col in 0..self.size.cols {
            let Some(cell) = screen.cell(row, col) else {
                continue;
            };
            if cell.is_wide_continuation() {
                continue;
            }
            let contents = if cell.has_contents() {
                CellContents::new(cell.contents())
            } else {
                CellContents::SPACE
            };
            cells.push(StyledTerminalCell {
                contents,
                fg: cell.fgcolor(),
                bg: cell.bgcolor(),
                bold: cell.bold(),
                dim: cell.dim(),
                italic: cell.italic(),
                underline: cell.underline(),
                inverse: cell.inverse(),
            });
        }

        trim_styled_cells(&mut cells);
        cells
    }

    fn region_scrollback_view<T, F>(&self, current: Vec<T>, map_history: F) -> Vec<T>
    where
        T: Clone,
        F: Fn(&Vec<StyledTerminalCell>) -> T,
    {
        let height = self.size.rows as usize;
        let total = self.region_scrollback.len() + current.len();
        let end = total.saturating_sub(self.region_scrollback_offset);
        let start = end.saturating_sub(height);
        let mut rows = Vec::with_capacity(height);

        for index in start..end {
            if index < self.region_scrollback.len() {
                rows.push(map_history(&self.region_scrollback[index]));
            } else if let Some(row) = current.get(index - self.region_scrollback.len()) {
                rows.push(row.clone());
            }
        }

        rows
    }

    pub fn size(&self) -> TerminalSize {
        self.size
    }

    /// How many rows of history this pane retains before the oldest fall off.
    pub fn scrollback_limit(&self) -> usize {
        self.region_scrollback_limit
    }

    pub fn scrollback(&self) -> usize {
        if self.parser.screen().alternate_screen() {
            return self.alternate_history_offset;
        }
        self.parser.screen().scrollback() + self.region_scrollback_offset
    }

    pub fn wants_sgr_mouse_events(&mut self) -> bool {
        self.drain_output();
        self.wants_sgr_mouse_events_snapshot()
    }

    pub fn wants_sgr_mouse_events_snapshot(&self) -> bool {
        if self.output_only {
            return false;
        }
        let screen = self.parser.screen();
        screen.mouse_protocol_mode() != vt100::MouseProtocolMode::None
            && screen.mouse_protocol_encoding() == vt100::MouseProtocolEncoding::Sgr
    }

    pub fn uses_alternate_screen(&mut self) -> bool {
        self.drain_output();
        self.uses_alternate_screen_snapshot()
    }

    pub fn uses_alternate_screen_snapshot(&self) -> bool {
        self.parser.screen().alternate_screen()
    }

    pub fn scrollback_by(&mut self, rows: isize) {
        if self.parser.screen().alternate_screen() {
            self.capture_alternate_snapshot();
            let max_offset = self.alternate_history.len().saturating_sub(1);
            let before = self.alternate_history_offset;
            self.alternate_history_offset = if rows.is_negative() {
                self.alternate_history_offset
                    .saturating_sub(rows.unsigned_abs())
            } else {
                self.alternate_history_offset.saturating_add(rows as usize)
            }
            .min(max_offset);
            if self.alternate_history_offset != before {
                self.invalidate_render_cache();
            }
            return;
        }

        let before = self.scrollback();
        if rows.is_negative() {
            let mut remaining = rows.unsigned_abs();
            if self.region_scrollback_offset > 0 {
                let consumed = remaining.min(self.region_scrollback_offset);
                self.region_scrollback_offset -= consumed;
                remaining -= consumed;
            }
            if remaining > 0 {
                let parser_before = self.parser.screen().scrollback();
                self.parser
                    .screen_mut()
                    .set_scrollback(parser_before.saturating_sub(remaining));
            }
        } else if rows > 0 {
            let mut remaining = rows as usize;
            if self.region_scrollback_offset == 0 {
                let parser_before = self.parser.screen().scrollback();
                self.parser
                    .screen_mut()
                    .set_scrollback(parser_before.saturating_add(remaining));
                let parser_after = self.parser.screen().scrollback();
                remaining = remaining.saturating_sub(parser_after.saturating_sub(parser_before));
            }
            if remaining > 0 && !self.region_scrollback.is_empty() {
                self.region_scrollback_offset = self
                    .region_scrollback_offset
                    .saturating_add(remaining)
                    .min(self.region_scrollback.len());
            }
        }

        if self.scrollback() != before {
            self.invalidate_render_cache();
        }
    }

    pub fn reset_scrollback(&mut self) {
        if self.alternate_history_offset != 0
            || self.region_scrollback_offset != 0
            || self.parser.screen().scrollback() != 0
        {
            self.alternate_history_offset = 0;
            self.region_scrollback_offset = 0;
            self.parser.screen_mut().set_scrollback(0);
            self.invalidate_render_cache();
        }
    }

    /// Where the view's first row sits in the pane's whole history, counted
    /// from the oldest line kept: region scrollback, then the parser's own
    /// scrollback, then the live screen (the same order `scrollback_by` walks).
    ///
    /// A mouse selection is anchored to these line numbers, not to screen
    /// rows, so it stays on the same text while the view scrolls under it (a
    /// drag past the pane's edge, the wheel, new output while scrolled back)
    /// and can reach rows that are no longer on screen. Counting from the top
    /// keeps a line's number fixed as output arrives below it.
    ///
    /// The alternate screen keeps whole-frame history instead of lines, and a
    /// live-only pane has none, so both answer 0: there the screen rows ARE
    /// the line numbers.
    pub fn view_top_line(&mut self) -> usize {
        if self.live_screen_only || self.parser.screen().alternate_screen() {
            return 0;
        }
        let history = self.region_scrollback.len() + self.parser_scrollback_len();
        history.saturating_sub(self.scrollback())
    }

    /// Plain text of history lines `start..=end` (see `view_top_line`), one
    /// entry per line; a line that is gone (trimmed off the far end of the
    /// history) comes back empty. The parser's scrollback is only readable
    /// through its view, so the view is moved to each stretch and put back.
    pub fn history_lines(&mut self, start: usize, end: usize) -> Vec<String> {
        if end < start {
            return Vec::new();
        }
        if self.live_screen_only || self.parser.screen().alternate_screen() {
            let visible = self.visible_lines_snapshot();
            return (start..=end)
                .map(|index| visible.get(index).cloned().unwrap_or_default())
                .collect();
        }
        let region = self.region_scrollback.len();
        let history = self.parser_scrollback_len();
        let rows = self.size.rows as usize;
        let saved = self.parser.screen().scrollback();
        let mut lines = Vec::with_capacity(end - start + 1);
        let mut index = start;
        while index <= end {
            if index < region {
                lines.push(styled_cells_to_plain(&self.region_scrollback[index]));
                index += 1;
                continue;
            }
            let line = index - region;
            if line >= history + rows {
                break;
            }
            // Put `line` at the top of the view (or as high as it can go).
            let offset = history.saturating_sub(line);
            self.parser.screen_mut().set_scrollback(offset);
            let top = history - offset;
            for text in self
                .current_visible_lines_snapshot()
                .into_iter()
                .skip(line - top)
            {
                if index > end {
                    break;
                }
                lines.push(text);
                index += 1;
            }
        }
        self.parser.screen_mut().set_scrollback(saved);
        lines.resize(end - start + 1, String::new());
        lines
    }

    /// How many lines the parser holds above its screen. vt100 has no getter
    /// for it, but clamps an over-large offset to exactly that.
    fn parser_scrollback_len(&mut self) -> usize {
        let screen = self.parser.screen_mut();
        let saved = screen.scrollback();
        screen.set_scrollback(usize::MAX);
        let len = screen.scrollback();
        screen.set_scrollback(saved);
        len
    }

    pub fn cursor(&self) -> TerminalCursor {
        let (row, col) = self.parser.screen().cursor_position();
        TerminalCursor {
            row,
            col,
            visible: !self.output_only && !self.parser.screen().hide_cursor(),
        }
    }

    pub fn child_process_id(&self) -> Option<u32> {
        if self.exit_observed.is_some() {
            None
        } else {
            self.child.process_id()
        }
    }

    pub fn try_wait(&mut self) -> Result<Option<portable_pty::ExitStatus>> {
        if let Some((status, _)) = &self.exit_observed {
            return Ok(Some(status.clone()));
        }
        let status = self
            .child
            .try_wait()
            .context("failed to poll child status")?;
        if let Some(status) = &status {
            self.exit_observed = Some((status.clone(), Instant::now()));
        }
        Ok(status)
    }

    pub fn exit_observed(&self) -> bool {
        self.exit_observed.is_some()
    }
    pub fn is_output_only(&self) -> bool {
        self.output_only
    }

    /// Process exit and reader completion are independent. Never block the App
    /// or consume bytes here: planners must receive every drained raw chunk.
    /// A descendant holding the slave cannot delay exit finalization forever.
    pub fn try_wait_after_output(&mut self) -> Result<Option<portable_pty::ExitStatus>> {
        let status = self.try_wait()?;
        if status.is_some()
            && !self.output_eof
            && self
                .exit_observed
                .as_ref()
                .is_some_and(|(_, at)| at.elapsed() < EXIT_OUTPUT_GRACE)
        {
            return Ok(None);
        }
        Ok(status)
    }

    /// Retain the emulator of an exited plain command, never a writable child
    /// or an old process ID that Drop might later signal after PID reuse.
    pub fn retain_output_after_exit(&mut self) {
        if self.exit_observed.is_none() {
            return;
        }
        self.terminate_child();
        self.output_only = true;
        self.drain_output();
    }

    /// Invalidate controls and request process-group teardown without waiting
    /// on the actor. `try_wait`/`child_process_id` report actual reaping, which
    /// can remain pending while the OS finishes draining a terminal.
    pub fn request_termination(&mut self) {
        self.terminate_child();
    }
}

impl StyledTerminalCell {
    pub fn plain(contents: CellContents) -> Self {
        Self {
            contents,
            fg: vt100::Color::Default,
            bg: vt100::Color::Default,
            bold: false,
            dim: false,
            italic: false,
            underline: false,
            inverse: false,
        }
    }
}

fn trim_styled_cells(cells: &mut Vec<StyledTerminalCell>) {
    while cells
        .last()
        .is_some_and(|cell| cell.contents == " " && cell.bg == vt100::Color::Default)
    {
        cells.pop();
    }
}

fn styled_cells_to_plain(cells: &[StyledTerminalCell]) -> String {
    let mut text = String::new();
    for cell in cells {
        text.push_str(cell.contents.as_str());
    }
    text
}

impl TerminalPane {
    fn process_output_chunk(&mut self, chunk: &[u8]) {
        for byte in chunk {
            self.capture_top_origin_scroll_region_line_before(*byte);
            self.parser.process(&[*byte]);
            self.track_ansi_byte(*byte);
            if let Some(edge) = self.notification_scanner.scan(*byte) {
                self.on_hyperlink_edge(edge);
            }
        }
        self.capture_alternate_snapshot();
    }

    /// An OSC 8 edge has just been parsed, so the cursor sits exactly where the
    /// linked text starts (open) or ends (close).
    fn on_hyperlink_edge(&mut self, edge: OscHyperlink) {
        let cursor = self.parser.screen().cursor_position();
        // An open while another is open implicitly closes the first.
        if let Some((url, start)) = self.open_hyperlink.take() {
            self.record_hyperlink(url, start, cursor);
        }
        if let OscHyperlink::Open(url) = edge {
            self.open_hyperlink = Some((url, cursor));
        }
    }

    fn record_hyperlink(&mut self, url: String, start: (u16, u16), end: (u16, u16)) {
        if end < start {
            return;
        }
        // The cursor is in live-grid coordinates; read the live grid even if the
        // user is scrolled back right now.
        let scrolled = self.parser.screen().scrollback();
        if scrolled != 0 {
            self.parser.screen_mut().set_scrollback(0);
        }
        let screen = self.parser.screen();
        let (_, cols) = screen.size();
        let mut texts = Vec::new();
        // One entry per screen row: a link that wrapped is clicked one row at a time.
        for row in start.0..=end.0 {
            let from = if row == start.0 { start.1 } else { 0 };
            let to = if row == end.0 { end.1 } else { cols };
            if to <= from {
                continue;
            }
            let text = screen
                .contents_between(row, from, row, to)
                .trim()
                .to_string();
            if !text.is_empty() {
                texts.push(text);
            }
        }
        if scrolled != 0 {
            self.parser.screen_mut().set_scrollback(scrolled);
        }
        for text in texts {
            let link = TerminalHyperlink {
                text,
                url: url.clone(),
            };
            self.hyperlinks.retain(|existing| existing != &link);
            if self.hyperlinks.len() >= MAX_TERMINAL_HYPERLINKS {
                self.hyperlinks.pop_front();
            }
            self.hyperlinks.push_back(link);
        }
    }

    /// The url under column `col` (a char index) of `line`, a row of this pane
    /// as shown: an OSC 8 link the agent emitted, else a bare url in the text.
    pub fn hyperlink_at(&self, line: &str, col: usize) -> Option<String> {
        hyperlink_at(line, col, self.hyperlinks.iter().rev())
    }

    /// Drain desktop-notification OSCs (777/9) the worker emitted since the
    /// last call. The sweep re-emits these to the outer terminal — the only way
    /// a backend's own "needs your permission" notification survives the
    /// embedded parser.
    pub fn take_notifications(&mut self) -> Vec<TerminalNotification> {
        self.notification_scanner.take()
    }

    fn capture_top_origin_scroll_region_line_before(&mut self, byte: u8) {
        if byte != b'\n' {
            return;
        }

        let Some((top, bottom)) = self.tracked_scroll_region else {
            return;
        };
        if top != 0 || bottom >= self.size.rows.saturating_sub(1) {
            return;
        }

        let (cursor_row, _) = self.parser.screen().cursor_position();
        if cursor_row != bottom {
            return;
        }

        let cells = self.styled_screen_row(top);
        if cells.is_empty() {
            return;
        }
        self.push_region_scrollback(cells);
    }

    fn push_region_scrollback(&mut self, cells: Vec<StyledTerminalCell>) {
        self.region_scrollback.push(cells);
        if self.region_scrollback_offset > 0 {
            self.region_scrollback_offset = self
                .region_scrollback_offset
                .saturating_add(1)
                .min(self.region_scrollback.len());
        }

        if self.region_scrollback.len() > self.region_scrollback_limit {
            let overflow = self.region_scrollback.len() - self.region_scrollback_limit;
            self.region_scrollback.drain(0..overflow);
            self.region_scrollback_offset = self.region_scrollback_offset.saturating_sub(overflow);
        }
        self.invalidate_render_cache();
    }

    fn track_ansi_byte(&mut self, byte: u8) {
        let state = std::mem::replace(&mut self.ansi_state, AnsiTrackState::Ground);
        self.ansi_state = match state {
            AnsiTrackState::Ground if byte == 0x1b => AnsiTrackState::Escape,
            AnsiTrackState::Ground => AnsiTrackState::Ground,
            AnsiTrackState::Escape if byte == b'[' => AnsiTrackState::Csi(Vec::new()),
            AnsiTrackState::Escape if byte == 0x1b => AnsiTrackState::Escape,
            AnsiTrackState::Escape => AnsiTrackState::Ground,
            AnsiTrackState::Csi(mut bytes) => {
                bytes.push(byte);
                if (0x40..=0x7e).contains(&byte) {
                    self.apply_tracked_csi(&bytes);
                    AnsiTrackState::Ground
                } else {
                    AnsiTrackState::Csi(bytes)
                }
            }
        };
    }

    fn apply_tracked_csi(&mut self, bytes: &[u8]) {
        if bytes.last() != Some(&b'r') {
            return;
        }
        let params = &bytes[..bytes.len().saturating_sub(1)];
        if params.is_empty() {
            self.tracked_scroll_region = None;
            return;
        }
        if params
            .iter()
            .any(|byte| !byte.is_ascii_digit() && *byte != b';')
        {
            return;
        }

        let text = String::from_utf8_lossy(params);
        let mut parts = text.split(';');
        let top = parts
            .next()
            .and_then(|part| part.parse::<u16>().ok())
            .unwrap_or(1);
        let bottom = parts
            .next()
            .and_then(|part| part.parse::<u16>().ok())
            .unwrap_or(self.size.rows);
        if top == 0 || bottom == 0 || top > bottom {
            return;
        }

        let top = top.saturating_sub(1).min(self.size.rows.saturating_sub(1));
        let bottom = bottom
            .saturating_sub(1)
            .min(self.size.rows.saturating_sub(1));
        self.tracked_scroll_region = Some((top, bottom));
    }

    fn capture_alternate_snapshot(&mut self) {
        if !self.parser.screen().alternate_screen() {
            self.alternate_history_offset = 0;
            self.last_alternate_snapshot.clear();
            return;
        }

        let snapshot = self.current_visible_lines_snapshot();
        if snapshot == self.last_alternate_snapshot {
            return;
        }

        self.last_alternate_snapshot = snapshot.clone();
        self.alternate_history.push(snapshot);
        self.invalidate_render_cache();
        if self.alternate_history.len() > self.alternate_history_limit {
            let overflow = self.alternate_history.len() - self.alternate_history_limit;
            self.alternate_history.drain(0..overflow);
            self.alternate_history_offset = self.alternate_history_offset.saturating_sub(overflow);
            self.invalidate_render_cache();
        }
    }

    fn alternate_history_snapshot(&self) -> Option<&Vec<String>> {
        if !self.parser.screen().alternate_screen() || self.alternate_history_offset == 0 {
            return None;
        }
        let index = self
            .alternate_history
            .len()
            .checked_sub(1 + self.alternate_history_offset)?;
        self.alternate_history.get(index)
    }

    /// Drop the materialized styled-cell grid so the next snapshot rebuilds it.
    /// Called on every path that changes what the screen shows; also public so
    /// benchmarks can measure a cold rebuild without faking terminal output.
    pub fn invalidate_render_cache(&mut self) {
        self.styled_lines_cache = None;
        self.render_generation = self.render_generation.wrapping_add(1);
    }

    /// See the field: changes whenever the visible content may have.
    pub fn render_generation(&self) -> u64 {
        self.render_generation
    }

    fn append_output_log(&mut self, chunk: &[u8]) {
        self.output_log.push_str(&String::from_utf8_lossy(chunk));
        if self.output_log.len() > self.output_log_limit {
            let overflow = self.output_log.len() - self.output_log_limit;
            let drain_to = self
                .output_log
                .char_indices()
                .map(|(index, _)| index)
                .find(|index| *index >= overflow)
                .unwrap_or(overflow);
            self.output_log.drain(..drain_to);
        }
    }
}

impl Drop for TerminalPane {
    fn drop(&mut self) {
        self.terminate_child();
        self.reader_stop.store(true, Ordering::Release);
        // An inherited slave can delay EOF. Never join a reader on the App
        // actor. Its bounded readiness wait closes the remaining master handles;
        // Darwin can then finish ttywait and the reserved reaper proves exit.
        self.reader_thread.take();
    }
}

/// A shell reports a signal death as 128 + signal number, so an agent killed by
/// SIGTERM surfaces as the bare number 143 — which tells the reader nothing about
/// what happened, and in particular does not say that the agent was KILLED rather
/// than that its work failed. Name the signal so the death note is diagnosable.
pub fn describe_exit_code(code: u32) -> String {
    let signal = match code {
        130 => Some("SIGINT"),
        137 => Some("SIGKILL"),
        139 => Some("SIGSEGV"),
        143 => Some("SIGTERM"),
        _ => None,
    };
    match signal {
        // SIGKILL is overwhelmingly the OOM killer on a dev machine; SIGTERM is a
        // deliberate stop, from Rudder's own teardown, a system shutdown/sleep, or
        // a manual kill. Both mean the work was interrupted, not that it errored.
        Some("SIGKILL") => "killed (SIGKILL) - usually the OS out-of-memory killer".to_string(),
        Some("SIGTERM") => {
            "terminated (SIGTERM) - stopped by Rudder, a system shutdown, or a manual kill"
                .to_string()
        }
        Some(name) => format!("killed by {name}"),
        None => format!("exit {code}"),
    }
}

impl TerminalPane {
    /// Invalidate controls and request teardown; never wait on the actor.
    ///
    /// `portable-pty` runs `setsid()` in the child's `pre_exec`, so the child
    /// is a session/process-group leader and its pgid equals its pid. Signaling
    /// the group is safe while the unreaped leader reserves its PID. After
    /// leader reaping we do not signal its numeric PID again. Closing the PTY
    /// supplies hangup, but does not prove stubborn/detached descendants stopped.
    /// The reserved reaper owns the direct child until actual wait proof; that
    /// proof alone is never sufficient for checkpoint/writer quiescence.
    fn terminate_child(&mut self) {
        if let Some(callback) = self.before_terminate.take() {
            callback();
        }
        if self.terminated {
            return;
        }
        self.terminated = true;
        self.child.request_termination();
    }
}

fn command_builder(command: Option<TerminalCommand>, term: &str) -> Result<CommandBuilder> {
    match command {
        Some(command) => {
            if command.program.is_empty() {
                return Err(anyhow!("command program must not be empty"));
            }

            let mut builder = CommandBuilder::new(command.program);
            builder.args(command.args);
            // The pane reads OSC 8 links itself (click to open), so tell
            // `supports-hyperlinks` callers like Claude Code to emit them
            // whatever the outer terminal is. A command can still override it.
            builder.env("FORCE_HYPERLINK", "1");
            for (key, value) in command.env {
                if is_term_env_key(&key) {
                    continue;
                }
                builder.env(key, value);
            }
            builder.env("TERM", term);
            Ok(builder)
        }
        None => {
            let shell = default_shell();
            if shell.is_empty() {
                return Err(anyhow!("could not determine default shell"));
            }
            let mut builder = CommandBuilder::new(shell);
            builder.env("TERM", term);
            Ok(builder)
        }
    }
}

#[cfg(windows)]
fn is_term_env_key(key: &str) -> bool {
    key.eq_ignore_ascii_case("TERM")
}

#[cfg(not(windows))]
fn is_term_env_key(key: &str) -> bool {
    key == "TERM"
}

#[cfg(windows)]
fn default_shell() -> String {
    std::env::var("COMSPEC").unwrap_or_else(|_| "cmd.exe".to_string())
}

#[cfg(not(windows))]
fn default_shell() -> String {
    std::env::var("SHELL").unwrap_or_else(|_| "/bin/sh".to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    struct TestReaper(Arc<ChildReaper>);
    impl Drop for TestReaper {
        fn drop(&mut self) {
            self.0.closing.store(true, Ordering::Release);
            self.0.wake.notify_all();
        }
    }
    #[derive(Debug)]
    struct FakeChild {
        ready: Arc<AtomicBool>,
        polls: Arc<AtomicUsize>,
        kills: Arc<AtomicUsize>,
        fail_poll: Option<usize>,
    }
    impl portable_pty::ChildKiller for FakeChild {
        fn kill(&mut self) -> std::io::Result<()> {
            self.kills.fetch_add(1, Ordering::SeqCst);
            Ok(())
        }
        fn clone_killer(&self) -> Box<dyn portable_pty::ChildKiller + Send + Sync> {
            Box::new(Self {
                ready: self.ready.clone(),
                polls: self.polls.clone(),
                kills: self.kills.clone(),
                fail_poll: None,
            })
        }
    }
    impl Child for FakeChild {
        fn try_wait(&mut self) -> std::io::Result<Option<portable_pty::ExitStatus>> {
            let poll = self.polls.fetch_add(1, Ordering::SeqCst);
            if self.fail_poll == Some(poll) {
                return Err(std::io::Error::other("fixture wait failure"));
            }
            Ok(self
                .ready
                .load(Ordering::SeqCst)
                .then(|| portable_pty::ExitStatus::with_exit_code(17)))
        }
        fn wait(&mut self) -> std::io::Result<portable_pty::ExitStatus> {
            panic!("blocking child wait is forbidden");
        }
        // Zero is an observation-only fixture ID; never signal a real process.
        fn process_id(&self) -> Option<u32> {
            Some(0)
        }
        #[cfg(windows)]
        fn as_raw_handle(&self) -> Option<std::os::windows::io::RawHandle> {
            None
        }
    }
    fn fake_child(
        reaper: &Arc<ChildReaper>,
        fail_poll: Option<usize>,
    ) -> (OwnedPtyChild, Arc<AtomicBool>, Arc<AtomicUsize>) {
        let ready = Arc::new(AtomicBool::new(false));
        let kills = Arc::new(AtomicUsize::new(0));
        let child = FakeChild {
            ready: ready.clone(),
            polls: Arc::new(AtomicUsize::new(0)),
            kills: kills.clone(),
            fail_poll,
        };
        (
            OwnedPtyChild::new(Box::new(child), reaper.reserve().unwrap()),
            ready,
            kills,
        )
    }

    #[test]
    fn reaper_admission_includes_live_and_pending_children_and_releases_only_on_proof() {
        let reaper = TestReaper(ChildReaper::start(2).unwrap());
        let (mut child, ready, kills) = fake_child(&reaper.0, None);
        let unused = reaper.0.reserve().unwrap();
        assert!(reaper.0.reserve().is_err());
        let started = Instant::now();
        child.request_termination();
        assert!(started.elapsed() < Duration::from_millis(200));
        assert_eq!(child.process_id(), Some(0), "a kill request is not reaping");
        assert!(child.try_wait().unwrap().is_none());
        assert!(
            reaper.0.reserve().is_err(),
            "pending child retains its slot"
        );
        child.request_termination();
        assert_eq!(
            kills.load(Ordering::SeqCst),
            1,
            "teardown cannot signal twice"
        );
        drop(unused);
        ready.store(true, Ordering::SeqCst);
        let deadline = Instant::now() + Duration::from_secs(2);
        while child.try_wait().unwrap().is_none() {
            assert!(Instant::now() < deadline);
            thread::sleep(Duration::from_millis(5));
        }
        assert_eq!(child.try_wait().unwrap().unwrap().exit_code(), 17);
        assert!(child.process_id().is_none());
        assert_eq!(reaper.0.admitted.load(Ordering::Acquire), 0);
        let _first = reaper.0.reserve().unwrap();
        let _second = reaper.0.reserve().unwrap();
    }

    #[test]
    fn reaper_keeps_a_faulted_child_owned_without_blocking_other_reaping() {
        let reaper = TestReaper(ChildReaper::start(2).unwrap());
        let (mut faulted, ready_faulted, _) = fake_child(&reaper.0, Some(1));
        let (mut other, ready_other, _) = fake_child(&reaper.0, None);
        faulted.request_termination();
        other.request_termination();
        ready_other.store(true, Ordering::SeqCst);
        let deadline = Instant::now() + Duration::from_secs(2);
        while other.try_wait().unwrap().is_none() || faulted.try_wait().is_ok() {
            assert!(Instant::now() < deadline);
            thread::sleep(Duration::from_millis(5));
        }
        assert_eq!(faulted.process_id(), Some(0));
        assert_eq!(reaper.0.admitted.load(Ordering::Acquire), 1);
        ready_faulted.store(true, Ordering::SeqCst);
        while !matches!(faulted.try_wait(), Ok(Some(_))) {
            assert!(Instant::now() < deadline);
            thread::sleep(Duration::from_millis(5));
        }
        assert_eq!(reaper.0.admitted.load(Ordering::Acquire), 0);
    }

    #[test]
    fn dropping_a_stopped_reader_with_undrained_exit_output_never_waits_on_the_actor() {
        let mut pane = TerminalPane::spawn_shell_or_command(
            Some(TerminalCommand::with_args("/bin/sh", ["-c", "printf 'reader-ready\\n'; read -r value; printf 'undrained final proof\\n'; exit 7"])),
            TerminalPaneOptions::default(),
        ).unwrap();
        let deadline = Instant::now() + Duration::from_secs(5);
        while !pane.output_log.contains("reader-ready") {
            pane.drain_output();
            assert!(Instant::now() < deadline);
            thread::sleep(Duration::from_millis(5));
        }
        pane.reader_stop.store(true, Ordering::Release);
        while !pane.reader_thread.as_ref().unwrap().is_finished() {
            assert!(Instant::now() < deadline);
            thread::sleep(Duration::from_millis(5));
        }
        pane.write_input(b"finish\n").unwrap();
        thread::sleep(Duration::from_millis(30));
        // Name physical handle destruction separately from the nonblocking
        // termination request. Empty stand-ins allow the same real master ->
        // writer -> remaining-pane drop order without unsafe field extraction.
        let stand_in = portable_pty::native_pty_system()
            .openpty(pane.size.pty_size())
            .unwrap();
        let master = std::mem::replace(&mut pane.master, stand_in.master);
        let sink: Arc<Mutex<Box<dyn Write + Send>>> =
            Arc::new(Mutex::new(Box::new(std::io::sink())));
        let writer = std::mem::replace(&mut pane.writer, sink);
        let started = Instant::now();
        eprintln!("undrained-teardown phase=request-start elapsed_ms=0");
        pane.request_termination();
        eprintln!(
            "undrained-teardown phase=request-finished elapsed_ms={}",
            started.elapsed().as_millis()
        );
        let pending = pane.child.pending.clone();
        let already_reaped = pane.child.status.is_some();
        drop(master);
        eprintln!(
            "undrained-teardown phase=master-dropped elapsed_ms={}",
            started.elapsed().as_millis()
        );
        drop(writer);
        eprintln!(
            "undrained-teardown phase=writer-dropped elapsed_ms={}",
            started.elapsed().as_millis()
        );
        drop(pane);
        eprintln!(
            "undrained-teardown phase=pane-dropped elapsed_ms={}",
            started.elapsed().as_millis()
        );
        assert!(
            started.elapsed() < Duration::from_millis(250),
            "teardown blocked on undrained slave output elapsed_ms={}",
            started.elapsed().as_millis()
        );
        if let Some(pending) = pending {
            let deadline = Instant::now() + Duration::from_secs(5);
            while pending.lock().unwrap().status.is_none() {
                assert!(
                    Instant::now() < deadline,
                    "closed PTY did not permit actual child reaping"
                );
                thread::sleep(Duration::from_millis(5));
            }
        } else {
            assert!(already_reaped);
        }
    }

    #[test]
    fn real_pty_readiness_delivers_output_and_eof_before_exit_grace() {
        let mut pane = TerminalPane::spawn_shell_or_command(
            Some(TerminalCommand::with_args(
                "/bin/sh",
                [
                    "-c",
                    "printf 'readiness-start\\n'; sleep 0.05; printf 'readiness-tail\\n'; exit 7",
                ],
            )),
            TerminalPaneOptions::default(),
        )
        .unwrap();
        let deadline = Instant::now() + Duration::from_secs(5);
        let status = loop {
            pane.drain_output();
            if let Some(status) = pane.try_wait_after_output().unwrap() {
                break status;
            }
            assert!(
                Instant::now() < deadline,
                "real PTY output/exit did not finish"
            );
            thread::sleep(Duration::from_millis(5));
        };
        assert_eq!(status.exit_code(), 7);
        assert!(
            pane.output_eof,
            "reader must observe actual EOF, not the exit timeout"
        );
        assert!(pane.output_log.contains("readiness-start"));
        assert!(pane.output_log.contains("readiness-tail"));
    }

    #[test]
    fn stopped_pty_reader_finishes_while_live_child_keeps_slave_open() {
        let mut pane = TerminalPane::spawn_shell_or_command(
            Some(TerminalCommand::with_args(
                "/bin/sh",
                ["-c", "printf 'reader-ready\\n'; read -r value"],
            )),
            TerminalPaneOptions::default(),
        )
        .unwrap();
        let deadline = Instant::now() + Duration::from_secs(5);
        while !pane.output_log.contains("reader-ready") {
            pane.drain_output();
            assert!(
                Instant::now() < deadline,
                "reader did not receive live PTY output"
            );
            thread::sleep(Duration::from_millis(5));
        }
        pane.reader_stop.store(true, Ordering::Release);
        let reader = pane.reader_thread.take().unwrap();
        let deadline = Instant::now() + Duration::from_secs(2);
        while !reader.is_finished() {
            assert!(
                Instant::now() < deadline,
                "stopped reader remained blocked on an open slave"
            );
            thread::sleep(Duration::from_millis(5));
        }
        reader.join().unwrap();
        assert!(
            pane.try_wait().unwrap().is_none(),
            "reader shutdown must not require child exit"
        );
        // Dropping the pane still invalidates controls and terminates/reaps its child.
    }

    #[test]
    fn exit_output_waits_for_forwarded_tail_and_reader_eof() {
        let mut pane = TerminalPane::spawn_shell_or_command(
            Some(TerminalCommand::with_args("/bin/sh", ["-c", "exit 0"])),
            TerminalPaneOptions::default(),
        )
        .unwrap();
        while pane.try_wait().unwrap().is_none() {
            std::thread::sleep(Duration::from_millis(5));
        }
        let (sender, receiver) = mpsc::channel();
        pane.output_rx = receiver;
        pane.output_eof = false;
        sender.send(b"final command proof\r\n".to_vec()).unwrap();
        assert!(
            pane.try_wait_after_output().unwrap().is_none(),
            "process exit cannot skip undrained output"
        );
        assert!(String::from_utf8(pane.drain_output())
            .unwrap()
            .contains("final command proof"));
        assert!(
            pane.try_wait_after_output().unwrap().is_none(),
            "a temporarily empty queue is not EOF"
        );
        drop(sender);
        pane.drain_output();
        assert!(pane.try_wait_after_output().unwrap().unwrap().success());
    }

    #[test]
    fn exit_output_bound_keeps_eof_honest_and_retained_pane_has_no_live_pid() {
        use std::sync::atomic::AtomicUsize;
        let mut pane = TerminalPane::spawn_shell_or_command(
            Some(TerminalCommand::with_args("/bin/sh", ["-c", "exit 0"])),
            TerminalPaneOptions::default(),
        )
        .unwrap();
        while pane.try_wait().unwrap().is_none() {
            std::thread::sleep(Duration::from_millis(5));
        }
        let (sender, receiver) = mpsc::channel();
        pane.output_rx = receiver;
        pane.output_eof = false;
        pane.exit_observed.as_mut().unwrap().1 =
            Instant::now() - EXIT_OUTPUT_GRACE - Duration::from_millis(1);
        assert!(pane.try_wait_after_output().unwrap().unwrap().success());
        assert!(!pane.output_eof, "timeout does not invent EOF");
        let called = Arc::new(AtomicUsize::new(0));
        let callback = called.clone();
        pane.set_before_terminate(Arc::new(move || {
            callback.fetch_add(1, Ordering::SeqCst);
        }));
        pane.retain_output_after_exit();
        assert!(pane.is_output_only());
        assert!(pane.child_process_id().is_none());
        assert!(pane.write_input(b"must not execute").is_err());
        drop(pane);
        assert_eq!(
            called.load(Ordering::SeqCst),
            1,
            "retired output cannot later signal a reused PID or repeat teardown"
        );
        drop(sender);
    }

    #[test]
    fn control_invalidation_precedes_every_explicit_or_implicit_pty_teardown() {
        use std::sync::atomic::AtomicUsize;
        for implicit_drop in [false, true] {
            let attempt = uuid::Uuid::new_v4().to_string();
            let command = TerminalCommand::with_args("/bin/sh", ["-c", "sleep 60"])
                .with_env("RUDDER_FABRIC_ATTEMPT_ID", &attempt)
                .with_env("RUDDER_EXECUTION_PROFILE", "claude-sdk")
                .with_env("RUDDER_AGENT_LINK_LAUNCH", "{\"format_version\":1}");
            let mut pane =
                TerminalPane::spawn_shell_or_command(Some(command), TerminalPaneOptions::default())
                    .unwrap();
            assert_eq!(pane.instance_id(), attempt);
            assert_eq!(pane.execution_profile(), Some("claude-sdk"));
            assert!(pane.agent_link_launch_json().is_some());
            let pid = pane.child_process_id().unwrap() as libc::pid_t;
            let calls = Arc::new(AtomicUsize::new(0));
            let captured = calls.clone();
            pane.set_before_terminate(Arc::new(move || {
                assert_eq!(
                    unsafe { libc::kill(pid, 0) },
                    0,
                    "controls must close while the owned process still exists"
                );
                captured.fetch_add(1, Ordering::SeqCst);
            }));
            if !implicit_drop {
                pane.request_termination();
                assert!(pane.write_input(b"must not revive teardown\n").is_err());
            }
            drop(pane);
            assert_eq!(calls.load(Ordering::SeqCst), 1);
            let deadline = Instant::now() + Duration::from_secs(5);
            while unsafe { libc::kill(pid, 0) } == 0 {
                assert!(Instant::now() < deadline, "owned child was not reaped");
                thread::sleep(Duration::from_millis(5));
            }
            assert_eq!(unsafe { libc::kill(pid, 0) }, -1, "owned child was reaped");
        }
    }

    #[test]
    fn osc_notification_parsing_recognizes_777_and_9_only() {
        assert_eq!(
            parse_osc_notification("777;notify;Claude Code;Claude needs your permission"),
            Some(TerminalNotification {
                title: "Claude Code".to_string(),
                body: "Claude needs your permission".to_string(),
            })
        );
        // A 777 with no body separator is a bare message, not a title.
        assert_eq!(
            parse_osc_notification("777;notify;Claude is waiting for your input"),
            Some(TerminalNotification {
                title: String::new(),
                body: "Claude is waiting for your input".to_string(),
            })
        );
        // iTerm2-style OSC 9.
        assert_eq!(
            parse_osc_notification("9;Task complete"),
            Some(TerminalNotification {
                title: String::new(),
                body: "Task complete".to_string(),
            })
        );
        // Window titles, clipboard, hyperlinks: not notifications.
        assert_eq!(parse_osc_notification("0;some title"), None);
        assert_eq!(parse_osc_notification("52;c;YWJj"), None);
        assert_eq!(parse_osc_notification("8;;https://x"), None);
        assert_eq!(parse_osc_notification("777;notify;;"), None);
    }

    #[test]
    fn notification_scanner_survives_chunk_splits_and_both_terminators() {
        let mut scanner = OscNotificationScanner::default();
        // BEL-terminated, split mid-sequence across "chunks" (scan is per-byte,
        // so any split point is exercised by feeding byte-at-a-time).
        let stream = b"hello \x1b]777;notify;Claude Code;needs you\x07 world \x1b]9;done\x1b\\ tail \x1b]0;title\x07";
        for byte in stream.iter() {
            scanner.scan(*byte);
        }
        let notes = scanner.take();
        assert_eq!(notes.len(), 2, "777 and 9 captured, title OSC ignored");
        assert_eq!(notes[0].title, "Claude Code");
        assert_eq!(notes[0].body, "needs you");
        assert_eq!(notes[1].body, "done");
        assert!(scanner.take().is_empty(), "take drains");
    }

    #[test]
    fn notification_scanner_caps_runaway_payloads_without_desyncing() {
        let mut scanner = OscNotificationScanner::default();
        scanner.scan(0x1b);
        scanner.scan(b']');
        for _ in 0..(OSC_SCAN_MAX_BYTES * 2) {
            scanner.scan(b'x');
        }
        scanner.scan(0x07);
        // Not a notification prefix, so nothing pending — but the stream is
        // back in ground state and a following notification still parses.
        for byte in b"\x1b]9;after\x07".iter() {
            scanner.scan(*byte);
        }
        let notes = scanner.take();
        assert_eq!(notes.len(), 1);
        assert_eq!(notes[0].body, "after");
    }

    #[test]
    fn terminal_size_rejects_zero_dimensions() {
        assert!(TerminalSize::new(0, 80).is_err());
        assert!(TerminalSize::new(24, 0).is_err());
    }

    #[test]
    fn visible_lines_are_plain_text_after_vt100_sequences() {
        let mut pane_buffer = vt100::Parser::new(2, 10, 10);
        pane_buffer.process(b"\x1b[31mred\x1b[0m\r\nplain");

        let lines: Vec<_> = pane_buffer
            .screen()
            .rows(0, 10)
            .map(|line| line.trim_end_matches(' ').to_string())
            .collect();

        assert_eq!(lines, vec!["red", "plain"]);
    }

    #[cfg(not(windows))]
    #[test]
    fn output_waker_fires_when_child_produces_output() {
        use std::sync::atomic::{AtomicBool, Ordering};

        let woken = Arc::new(AtomicBool::new(false));
        // Delay the child's first output so the waker is reliably installed before
        // any bytes arrive (otherwise a fast `echo` could send + check the waker
        // slot while it is still None, and the test would flake).
        let pane = TerminalPane::spawn_shell_or_command(
            Some(TerminalCommand::with_args(
                "/bin/sh",
                ["-lc", "sleep 0.2; echo hi"],
            )),
            TerminalPaneOptions::default(),
        )
        .expect("spawn test pty");

        let woken_for_waker = Arc::clone(&woken);
        let waker: PtyOutputWaker = Arc::new(move || {
            woken_for_waker.store(true, Ordering::SeqCst);
        });
        pane.set_output_waker(waker);

        // The reader thread should fire the waker once the child's output arrives.
        let mut fired = false;
        for _ in 0..100 {
            if woken.load(Ordering::SeqCst) {
                fired = true;
                break;
            }
            std::thread::sleep(std::time::Duration::from_millis(20));
        }
        assert!(fired, "waker was not invoked after child produced output");
    }

    #[test]
    fn term_env_key_matching_uses_platform_case_rules() {
        assert!(is_term_env_key("TERM"));

        #[cfg(windows)]
        {
            assert!(is_term_env_key("term"));
            assert!(is_term_env_key("Term"));
        }

        #[cfg(not(windows))]
        {
            assert!(!is_term_env_key("term"));
            assert!(!is_term_env_key("Term"));
        }
    }

    #[cfg(not(windows))]
    #[test]
    fn top_origin_scroll_region_history_scrolls_like_terminal_scrollback() {
        let command = TerminalCommand::with_args(
            "/bin/sh",
            [
                "-lc",
                "printf '\\033[1;3r\\033[3;1H\\r\\nhistory001\\r\\nhistory002\\r\\nhistory003\\r\\nhistory004\\r\\nhistory005\\033[r'; sleep 1",
            ],
        );
        let mut pane = TerminalPane::spawn_shell_or_command(
            Some(command),
            TerminalPaneOptions {
                size: TerminalSize { rows: 5, cols: 20 },
                scrollback_lines: 100,
                ..Default::default()
            },
        )
        .expect("spawn test pty");

        for _ in 0..20 {
            std::thread::sleep(std::time::Duration::from_millis(25));
            pane.drain_output();
            if pane
                .visible_lines_snapshot()
                .join("\n")
                .contains("history005")
            {
                break;
            }
        }

        assert_eq!(pane.scrollback(), 0);
        pane.scrollback_by(2);
        let output = pane.visible_lines_snapshot().join("\n");
        assert!(output.contains("history001"), "output was {output:?}");
        assert!(output.contains("history005"), "output was {output:?}");
    }

    #[test]
    fn osc_hyperlink_parsing_reads_open_and_close() {
        assert_eq!(
            parse_osc_hyperlink("8;;https://example.com/a"),
            Some(OscHyperlink::Open("https://example.com/a".to_string()))
        );
        assert_eq!(
            parse_osc_hyperlink("8;id=7;file:///tmp/x.rs"),
            Some(OscHyperlink::Open("file:///tmp/x.rs".to_string()))
        );
        assert_eq!(parse_osc_hyperlink("8;;"), Some(OscHyperlink::Close));
        assert_eq!(parse_osc_hyperlink("0;title"), None);
        assert_eq!(parse_osc_hyperlink("9;done"), None);
    }

    #[test]
    fn hyperlink_at_prefers_agent_links_then_bare_urls() {
        let links = [TerminalHyperlink {
            text: "src/main.rs".to_string(),
            url: "file:///repo/src/main.rs".to_string(),
        }];
        let line = "  Edited src/main.rs, see https://github.com/o/r/pull/12.";
        let col = line.find("main.rs").unwrap();
        assert_eq!(
            hyperlink_at(line, col, links.iter()).as_deref(),
            Some("file:///repo/src/main.rs")
        );
        let col = line.find("pull").unwrap();
        assert_eq!(
            hyperlink_at(line, col, links.iter()).as_deref(),
            Some("https://github.com/o/r/pull/12"),
            "trailing period is not part of the url"
        );
        assert_eq!(
            hyperlink_at(line, line.find("see").unwrap(), links.iter()),
            None
        );
        assert_eq!(hyperlink_at(line, 0, links.iter()), None);
        assert_eq!(hyperlink_at(line, 500, links.iter()), None);
    }

    #[test]
    fn bare_urls_inside_punctuation_and_parens() {
        let none = std::iter::empty();
        let line = "(docs: https://en.wikipedia.org/wiki/Rust_(language))";
        assert_eq!(
            hyperlink_at(line, line.find("wiki/").unwrap(), none).as_deref(),
            Some("https://en.wikipedia.org/wiki/Rust_(language)")
        );
        let line = "[link](https://x.dev/a)";
        assert_eq!(
            hyperlink_at(line, line.find("x.dev").unwrap(), std::iter::empty()).as_deref(),
            Some("https://x.dev/a")
        );
        // Clicking the "[link](" part of the token is not the url.
        assert_eq!(hyperlink_at(line, 1, std::iter::empty()), None);
        assert_eq!(
            hyperlink_at("https:// nothing", 2, std::iter::empty()),
            None
        );
    }

    #[cfg(not(windows))]
    #[test]
    fn osc8_links_are_captured_with_their_on_screen_text() {
        let command = TerminalCommand::with_args(
            "/bin/sh",
            [
                "-lc",
                "printf 'see \\033]8;;https://example.com/pr/1\\033\\\\PR #1\\033]8;;\\033\\\\ now\\r\\n'; sleep 1",
            ],
        );
        let mut pane = TerminalPane::spawn_shell_or_command(
            Some(command),
            TerminalPaneOptions {
                size: TerminalSize { rows: 5, cols: 40 },
                scrollback_lines: 100,
                ..Default::default()
            },
        )
        .expect("spawn test pty");
        for _ in 0..40 {
            std::thread::sleep(std::time::Duration::from_millis(25));
            pane.drain_output();
            if pane.visible_lines_snapshot().join("\n").contains("now") {
                break;
            }
        }
        let lines = pane.visible_lines_snapshot();
        let line = lines
            .iter()
            .find(|l| l.contains("PR #1"))
            .expect("link row");
        let col = line.find("#1").unwrap();
        assert_eq!(
            pane.hyperlink_at(line, col).as_deref(),
            Some("https://example.com/pr/1")
        );
        assert_eq!(pane.hyperlink_at(line, line.find("now").unwrap()), None);
    }

    #[cfg(not(windows))]
    #[test]
    fn live_screen_only_renders_just_the_current_screen_no_scrollback() {
        // Emulate Claude Code's inline redraws: print far more lines than the screen
        // holds. A normal pane accumulates them in region_scrollback (which is what
        // stacked the duplicate welcome banners); a live_screen_only pane shows ONLY
        // the current screen, so the pane stays clean.
        let command = TerminalCommand::with_args(
            "/bin/sh",
            [
                "-lc",
                "for i in $(seq 1 40); do printf 'line%02d\\r\\n' \"$i\"; done; printf 'TAIL'; sleep 1",
            ],
        );
        let mut pane = TerminalPane::spawn_shell_or_command(
            Some(command),
            TerminalPaneOptions {
                size: TerminalSize { rows: 5, cols: 20 },
                scrollback_lines: 100,
                live_screen_only: true,
                ..Default::default()
            },
        )
        .expect("spawn test pty");

        for _ in 0..20 {
            std::thread::sleep(std::time::Duration::from_millis(25));
            pane.drain_output();
            if pane.visible_lines_snapshot().join("\n").contains("TAIL") {
                break;
            }
        }

        // Only the live screen: exactly `rows` lines, no accumulated history.
        let lines = pane.visible_lines_snapshot();
        assert_eq!(
            lines.len(),
            5,
            "live screen is exactly the row count: {lines:?}"
        );
        let joined = lines.join("\n");
        assert!(
            joined.contains("TAIL"),
            "shows the latest screen: {joined:?}"
        );
        assert!(
            !joined.contains("line01"),
            "early lines scrolled off, not stacked: {joined:?}"
        );
        // Scrolling back is a no-op: there is no captured history to reveal.
        assert_eq!(pane.scrollback(), 0);
        pane.scrollback_by(10);
        assert!(
            !pane.visible_lines_snapshot().join("\n").contains("line01"),
            "live_screen_only never exposes scrollback history"
        );
    }

    #[cfg(not(windows))]
    #[test]
    fn pty_child_normalizes_term_without_dropping_case_distinct_env_on_unix() {
        let command = TerminalCommand::with_args(
            "/bin/sh",
            [
                "-lc",
                "printf 'term=%s\\r\\nTerm=%s\\r\\nTERM=%s\\r\\n' \"$term\" \"$Term\" \"$TERM\"; sleep 1",
            ],
        )
        .with_env("term", "lowercase")
        .with_env("Term", "mixedcase")
        .with_env("TERM", "xterm-kitty");
        let mut pane = TerminalPane::spawn_shell_or_command(
            Some(command),
            TerminalPaneOptions {
                size: TerminalSize { rows: 5, cols: 40 },
                scrollback_lines: 100,
                ..Default::default()
            },
        )
        .expect("spawn test pty");

        for _ in 0..20 {
            std::thread::sleep(std::time::Duration::from_millis(25));
            pane.drain_output();
            if pane
                .visible_lines_snapshot()
                .join("\n")
                .contains("TERM=xterm-256color")
            {
                break;
            }
        }

        let output = pane.visible_lines_snapshot().join("\n");
        assert!(output.contains("term=lowercase"), "output was {output:?}");
        assert!(output.contains("Term=mixedcase"), "output was {output:?}");
        assert!(
            output.contains("TERM=xterm-256color"),
            "output was {output:?}"
        );
        assert!(
            !output.contains("term=xterm-kitty"),
            "output was {output:?}"
        );
    }
}

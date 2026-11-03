//! Native terminal recovery, called only by the App loop.
//!
//! Socket readers deliver typed messages carrying their captured connection
//! generation. They never mutate this state or write the PTY. A cursor is a
//! rendering checkpoint, not an operation receipt. Protocol adapters must
//! authenticate the project before constructing these messages.

use std::collections::VecDeque;
use std::time::{Duration, Instant};

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Cursor {
    pub generation: u64,
    pub sequence: u64,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Owner {
    pub owner_epoch: u64,
    pub run_epoch: u64,
}

#[derive(Clone, Debug)]
pub struct Snapshot {
    pub cursor: Cursor,
    pub reset: bool,
    pub gap: bool,
    pub rows: u16,
    pub cols: u16,
    /// Complete terminal state, for example vt100::Screen::state_formatted().
    /// Never an arbitrary tail of PTY bytes.
    pub screen: Vec<u8>,
    /// Separately bounded history. Adapters retain styled history in their
    /// existing pane model; these lines provide accessible/plain-text history.
    pub history: Vec<String>,
}

/// Concrete adapter for the TypeScript canonical snapshot contract. The outer
/// terminal-screen envelope supplies its own version, generation and sequence.
#[derive(serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub struct WireScreen<'a> {
    pub format: &'static str,
    pub columns: u16,
    pub rows: u16,
    pub screen: &'a str,
    pub scrollback: &'a [String],
}
impl Snapshot {
    pub fn wire_screen(&self) -> Result<WireScreen<'_>, std::str::Utf8Error> {
        Ok(WireScreen {
            format: "vt100-ansi-v1",
            columns: self.cols,
            rows: self.rows,
            screen: std::str::from_utf8(&self.screen)?,
            scrollback: &self.history,
        })
    }
}

/// Observes the authoritative parser's original bytes without duplicating its
/// screen. Call observe after the existing parser processes a PTY chunk; a
/// reconnect does not reset this producer's boundary or sequence.
pub struct ScreenExporter {
    boundary: Boundary,
    generation: u64,
    sequence: u64,
    dirty: bool,
    max_frame_bytes: usize,
    max_history_lines: usize,
    max_cells: usize,
}
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ExportError {
    IncompleteFrame,
    SizeLimit,
    SequenceExhausted,
}
impl ScreenExporter {
    pub fn new(
        generation: u64,
        max_frame_bytes: usize,
        max_history_lines: usize,
        max_cells: usize,
    ) -> Self {
        assert!(max_frame_bytes > 0 && max_cells > 0);
        Self {
            boundary: Boundary::default(),
            generation,
            sequence: 0,
            dirty: true,
            max_frame_bytes,
            max_history_lines,
            max_cells,
        }
    }
    pub fn observe(&mut self, bytes: &[u8]) {
        self.boundary.process(bytes);
        self.dirty |= !bytes.is_empty();
    }
    /// Call after an authoritative parser resize or other non-byte state change.
    pub fn mark_changed(&mut self) {
        self.dirty = true;
    }
    pub fn complete(&self) -> bool {
        self.boundary.complete()
    }
    /// EOF cannot complete a truncated escape/codepoint. Publish the actual
    /// parsed screen with an explicit reset/gap before exit, discarding only
    /// parser fragments that never became authoritative screen content.
    pub fn capture_final(&mut self, screen: &mut vt100::Screen) -> Result<Snapshot, ExportError> {
        self.boundary = Boundary::default();
        self.dirty = true;
        self.capture(screen, true, true)
    }
    /// Produces full parser state and newest bounded history. Temporarily moves
    /// the parser viewport, then restores its exact original scroll position.
    /// Call on the App loop, not concurrently with parsing or painting.
    pub fn capture(
        &mut self,
        screen: &mut vt100::Screen,
        reset: bool,
        gap: bool,
    ) -> Result<Snapshot, ExportError> {
        if !self.complete() {
            return Err(ExportError::IncompleteFrame);
        }
        let (rows, cols) = screen.size();
        if rows == 0 || cols == 0 || usize::from(rows) * usize::from(cols) > self.max_cells {
            return Err(ExportError::SizeLimit);
        }
        let sequence = if self.dirty {
            self.sequence
                .checked_add(1)
                .ok_or(ExportError::SequenceExhausted)?
        } else {
            self.sequence
        };
        let saved_scrollback = screen.scrollback();
        screen.set_scrollback(0);
        let state = screen.state_formatted();
        if state.len() > self.max_frame_bytes {
            screen.set_scrollback(saved_scrollback);
            return Err(ExportError::SizeLimit);
        }
        let mut history = Vec::new();
        let mut history_bytes = 0usize;
        if !screen.alternate_screen() {
            for offset in 1..=self.max_history_lines {
                screen.set_scrollback(offset);
                if screen.scrollback() < offset {
                    break;
                }
                let line = screen.rows(0, cols).next().unwrap_or_default();
                if line.len()
                    > self
                        .max_frame_bytes
                        .saturating_sub(history_bytes + state.len())
                {
                    break;
                }
                history_bytes += line.len();
                history.push(line);
            }
        }
        history.reverse();
        screen.set_scrollback(saved_scrollback);
        self.sequence = sequence;
        self.dirty = false;
        Ok(Snapshot {
            cursor: Cursor {
                generation: self.generation,
                sequence,
            },
            reset,
            gap,
            rows,
            cols,
            screen: state,
            history,
        })
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Receive {
    Applied { paint_ready: bool },
    SnapshotRequired,
    Stale,
    Exited(i32),
}

/// A persistent parser inside one connection, with fresh decoder and escape
/// state at every reconnect or snapshot replacement. Limits apply before any
/// allocation driven by a remote size/history message.
pub struct View {
    generation: u64,
    cursor: Option<Cursor>,
    awaiting_snapshot: bool,
    parser: vt100::Parser,
    boundary: Boundary,
    history: Vec<String>,
    max_frame_bytes: usize,
    max_history_lines: usize,
    max_cells: usize,
    exited: bool,
}

impl View {
    pub fn new(max_frame_bytes: usize, max_history_lines: usize, max_cells: usize) -> Self {
        assert!(max_frame_bytes > 0 && max_cells > 0);
        Self {
            generation: 0,
            cursor: None,
            awaiting_snapshot: true,
            parser: vt100::Parser::new(1, 1, max_history_lines),
            boundary: Boundary::default(),
            history: Vec::new(),
            max_frame_bytes,
            max_history_lines,
            max_cells,
            exited: false,
        }
    }

    /// Capture the returned token in every socket callback, including close and
    /// asynchronous paint completion. Overflow requires creating a new View.
    pub fn reconnect(&mut self) -> u64 {
        self.generation = self
            .generation
            .checked_add(1)
            .expect("connection generation exhausted");
        self.awaiting_snapshot = true;
        self.parser = vt100::Parser::new(1, 1, self.max_history_lines);
        self.boundary = Boundary::default();
        self.exited = false;
        self.generation
    }

    pub fn is_current(&self, generation: u64) -> bool {
        generation == self.generation
    }

    /// Resume cursors may be sent to the server, but the client still requires
    /// an explicit complete snapshot before accepting this connection's deltas.
    pub fn resume_cursor(&self) -> Option<&Cursor> {
        self.cursor.as_ref()
    }
    pub fn screen(&self) -> &vt100::Screen {
        self.parser.screen()
    }
    pub fn history(&self) -> &[String] {
        &self.history
    }

    pub fn snapshot(&mut self, generation: u64, snapshot: Snapshot) -> Receive {
        if !self.is_current(generation) {
            return Receive::Stale;
        }
        if self.exited
            && self
                .cursor
                .as_ref()
                .is_some_and(|cursor| cursor.generation >= snapshot.cursor.generation)
        {
            return Receive::Stale;
        }
        let history_bytes = snapshot
            .history
            .iter()
            .try_fold(0usize, |n, s| n.checked_add(s.len()));
        if snapshot.rows == 0
            || snapshot.cols == 0
            || usize::from(snapshot.rows) * usize::from(snapshot.cols) > self.max_cells
            || snapshot.screen.len() > self.max_frame_bytes
            || snapshot.history.len() > self.max_history_lines
            || history_bytes
                .and_then(|n| n.checked_add(snapshot.screen.len()))
                .map_or(true, |n| n > self.max_frame_bytes)
        {
            return self.gap();
        }
        if let Some(previous) = self.cursor.as_ref() {
            if snapshot.cursor.generation < previous.generation {
                return Receive::Stale;
            }
            if snapshot.cursor.generation == previous.generation {
                if snapshot.cursor.sequence < previous.sequence {
                    return Receive::Stale;
                }
                let contiguous = previous.sequence.checked_add(1) == Some(snapshot.cursor.sequence);
                if !self.awaiting_snapshot && !contiguous && !(snapshot.reset && snapshot.gap) {
                    return self.gap();
                }
            } else if !(snapshot.reset && snapshot.gap) {
                return self.gap();
            }
        }
        if self.awaiting_snapshot && !snapshot.reset {
            return self.gap();
        }
        let mut boundary = Boundary::default();
        boundary.process(&snapshot.screen);
        if !boundary.complete() {
            return self.gap();
        }
        let mut parser = vt100::Parser::new(snapshot.rows, snapshot.cols, self.max_history_lines);
        parser.process(&snapshot.screen);
        self.parser = parser;
        self.boundary = boundary;
        self.history = snapshot.history;
        self.cursor = Some(snapshot.cursor);
        self.awaiting_snapshot = false;
        self.exited = false;
        Receive::Applied { paint_ready: true }
    }

    pub fn delta(&mut self, generation: u64, cursor: Cursor, bytes: &[u8]) -> Receive {
        if !self.is_current(generation) {
            return Receive::Stale;
        }
        if self.awaiting_snapshot
            || self.exited
            || bytes.len() > self.max_frame_bytes
            || !self.cursor.as_ref().is_some_and(|previous| {
                previous.generation == cursor.generation
                    && previous.sequence.checked_add(1) == Some(cursor.sequence)
            })
        {
            return self.gap();
        }
        self.parser.process(bytes);
        self.boundary.process(bytes);
        self.cursor = Some(cursor);
        Receive::Applied {
            paint_ready: self.boundary.complete(),
        }
    }

    /// Apply only after all earlier output messages. A terminal exit message is
    /// rejected across a missing output range rather than hiding the gap.
    pub fn exit(&mut self, generation: u64, after: &Cursor, code: i32) -> Receive {
        if !self.is_current(generation) {
            return Receive::Stale;
        }
        if self.awaiting_snapshot
            || self.cursor.as_ref() != Some(after)
            || !self.boundary.complete()
        {
            return self.gap();
        }
        self.exited = true;
        Receive::Exited(code)
    }

    fn gap(&mut self) -> Receive {
        self.awaiting_snapshot = true;
        Receive::SnapshotRequired
    }
}

/// Input authorization has no replay queue. Replacing either socket or owner
/// discards control. Regrant control only from an authenticated current-owner
/// message, then write accepted bytes immediately on the App loop.
#[derive(Default)]
pub struct InputAuthority {
    generation: u64,
    owner: Option<Owner>,
    controller: Option<String>,
}

impl InputAuthority {
    pub fn replace(&mut self, generation: u64, owner: Owner) -> bool {
        if generation < self.generation
            || self.owner.as_ref().is_some_and(|previous| {
                (owner.owner_epoch, owner.run_epoch) < (previous.owner_epoch, previous.run_epoch)
            })
        {
            return false;
        }
        if generation == self.generation && self.owner.as_ref() == Some(&owner) {
            return true;
        }
        self.generation = generation;
        self.owner = Some(owner);
        self.controller = None;
        true
    }
    pub fn grant(&mut self, generation: u64, owner: &Owner, client: String) -> bool {
        if generation != self.generation || self.owner.as_ref() != Some(owner) || client.is_empty()
        {
            return false;
        }
        self.controller = Some(client);
        true
    }
    pub fn revoke(&mut self) {
        self.controller = None;
    }
    pub fn accepts(&self, generation: u64, owner: &Owner, client: &str) -> bool {
        generation == self.generation
            && self.owner.as_ref() == Some(owner)
            && self.controller.as_deref() == Some(client)
    }
    /// Privileged requests use their own durable operation/receipt path, never
    /// this byte-input path. Resize uses the same authority check as input.
    pub fn input<'a>(
        &self,
        generation: u64,
        owner: &Owner,
        client: &str,
        bytes: &'a [u8],
    ) -> Option<&'a [u8]> {
        self.accepts(generation, owner, client).then_some(bytes)
    }
}

/// One surface's bounded coalescing queue. A slow surface affects only its own
/// queue. Overflow requires a snapshot; it never keeps a suffix of escape bytes.
pub struct OutputQueue {
    bytes: VecDeque<Vec<u8>>,
    queued: usize,
    limit: usize,
    latency: Duration,
    started: Option<Instant>,
    needs_snapshot: bool,
}
impl OutputQueue {
    pub fn new(limit: usize, latency: Duration) -> Self {
        assert!(limit > 0);
        Self {
            bytes: VecDeque::new(),
            queued: 0,
            limit,
            latency,
            started: None,
            needs_snapshot: false,
        }
    }
    pub fn push(&mut self, bytes: &[u8], now: Instant) -> bool {
        if self.needs_snapshot {
            return false;
        }
        if bytes.len() > self.limit.saturating_sub(self.queued) {
            self.bytes.clear();
            self.queued = 0;
            self.started = None;
            self.needs_snapshot = true;
            return false;
        }
        if !bytes.is_empty() {
            self.started.get_or_insert(now);
            self.bytes.push_back(bytes.to_vec());
            self.queued += bytes.len();
        }
        true
    }
    pub fn queued_bytes(&self) -> usize {
        self.queued
    }
    pub fn needs_snapshot(&self) -> bool {
        self.needs_snapshot
    }
    pub fn due(&self, now: Instant) -> bool {
        self.started
            .is_some_and(|start| now.saturating_duration_since(start) >= self.latency)
    }
    /// Call before enqueuing exit/control. Caller checks socket backpressure
    /// before taking bytes, so at most one bounded frame is in flight per view.
    pub fn take(&mut self) -> Option<Vec<u8>> {
        if self.bytes.is_empty() || self.needs_snapshot {
            return None;
        }
        let mut result = Vec::with_capacity(self.queued);
        for bytes in self.bytes.drain(..) {
            result.extend(bytes);
        }
        self.queued = 0;
        self.started = None;
        Some(result)
    }
    pub fn reset_after_snapshot(&mut self) {
        self.bytes.clear();
        self.queued = 0;
        self.started = None;
        self.needs_snapshot = false;
    }
}

/// Local measurements use one monotonic clock. Correlation IDs join stages
/// across processes; subtracting their timestamps requires an explicit clock
/// synchronization/error bound and must not be reported as local latency.
#[derive(Clone, Debug)]
pub struct Trace {
    pub correlation_id: String,
    pub stage: &'static str,
    pub elapsed: Duration,
    pub authoritative: bool,
}
pub fn trace(
    start: Instant,
    now: Instant,
    correlation_id: String,
    stage: &'static str,
    authoritative: bool,
) -> Trace {
    Trace {
        correlation_id,
        stage,
        elapsed: now.saturating_duration_since(start),
        authoritative,
    }
}

#[derive(Default)]
struct Boundary {
    state: Escape,
    utf8_remaining: u8,
    synchronized: bool,
    csi: PrivateModes,
}
/// Constant-memory DEC private-mode parameter scanner. A mode may occur
/// anywhere in a semicolon-separated list, even across byte chunks. Oversized
/// numeric parameters never wrap into mode 2026 or hide later valid modes.
struct PrivateModes {
    first: bool,
    private: bool,
    valid: bool,
    value: Option<u16>,
    synchronized: bool,
}
impl Default for PrivateModes {
    fn default() -> Self {
        Self {
            first: true,
            private: false,
            valid: true,
            value: Some(0),
            synchronized: false,
        }
    }
}
impl PrivateModes {
    fn clear(&mut self) {
        *self = Self::default();
    }
    fn process(&mut self, byte: u8) {
        if self.first && byte == b'?' {
            self.private = true;
        } else if byte.is_ascii_digit() {
            self.value = self
                .value
                .and_then(|n| n.checked_mul(10))
                .and_then(|n| n.checked_add(u16::from(byte - b'0')));
        } else if byte == b';' {
            self.synchronized |= self.value == Some(2026);
            self.value = Some(0);
        } else {
            self.valid = false;
        }
        self.first = false;
    }
    fn contains_synchronized_update(&self) -> bool {
        self.private && self.valid && (self.synchronized || self.value == Some(2026))
    }
}

#[derive(Default)]
enum Escape {
    #[default]
    Ground,
    Esc,
    Csi,
    String,
    StringEsc,
}
impl Boundary {
    fn complete(&self) -> bool {
        matches!(self.state, Escape::Ground) && self.utf8_remaining == 0 && !self.synchronized
    }
    fn process(&mut self, bytes: &[u8]) {
        for &byte in bytes {
            if self.utf8_remaining > 0 {
                if byte & 0xc0 == 0x80 {
                    self.utf8_remaining -= 1;
                    continue;
                }
                self.utf8_remaining = 0;
            }
            if byte >= 0xc2 {
                self.utf8_remaining = match byte {
                    0xc2..=0xdf => 1,
                    0xe0..=0xef => 2,
                    0xf0..=0xf4 => 3,
                    _ => 0,
                };
                continue;
            }
            if byte == 0x18 || byte == 0x1a {
                self.state = Escape::Ground;
                self.csi.clear();
                continue;
            }
            match self.state {
                Escape::Ground => {
                    if byte == 0x1b {
                        self.state = Escape::Esc;
                    }
                }
                Escape::Esc => {
                    self.state = match byte {
                        b'[' => {
                            self.csi.clear();
                            Escape::Csi
                        }
                        b']' | b'P' | b'X' | b'^' | b'_' => Escape::String,
                        0x20..=0x2f => Escape::Esc,
                        _ => Escape::Ground,
                    }
                }
                Escape::Csi => {
                    if (0x40..=0x7e).contains(&byte) {
                        if self.csi.contains_synchronized_update() {
                            if byte == b'h' {
                                self.synchronized = true;
                            } else if byte == b'l' {
                                self.synchronized = false;
                            }
                        }
                        self.state = Escape::Ground;
                    } else {
                        self.csi.process(byte);
                    }
                }
                Escape::String => {
                    if byte == 0x07 {
                        self.state = Escape::Ground;
                    } else if byte == 0x1b {
                        self.state = Escape::StringEsc;
                    }
                }
                Escape::StringEsc => {
                    self.state = if byte == b'\\' {
                        Escape::Ground
                    } else {
                        Escape::String
                    }
                }
            }
        }
    }
}

#[cfg(test)]
mod tests;

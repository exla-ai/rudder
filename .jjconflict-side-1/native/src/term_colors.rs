//! The real terminal's colors, answered to the agents in Rudder's panes.
//!
//! opencode, Codex and Claude Code's auto theme pick light or dark by asking the
//! terminal for its colors (OSC 10 foreground, OSC 11 background) and, with
//! mode 2031, by hearing when the appearance changes (`CSI ? 997 ; 1|2 n`, 1
//! dark, 2 light; `CSI ? 996 n` asks). An agent in a pane asks the PANE, and the
//! pane used to say nothing, so every agent came up dark in a light terminal.
//!
//! Colors come from the outer terminal when the dashboard starts (asked once,
//! see `query_outer_terminal`) or from `<rudder home>/terminal-colors.json`,
//! which the cloud supervisor writes from the attaching laptop's terminal and
//! rewrites when it switches appearance. Each pane's reader thread answers
//! queries the moment they arrive (`QueryScanner`), so an agent waiting at
//! startup is never late; a change reaches agents that asked for mode 2031.

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::RwLock;
use std::time::SystemTime;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct TerminalColors {
    pub fg: Option<[u8; 3]>,
    pub bg: [u8; 3],
}

impl TerminalColors {
    /// What mode 2031 reports: 1 dark, 2 light.
    pub fn is_dark(&self) -> bool {
        let [r, g, b] = self.bg.map(f32::from);
        0.299 * r + 0.587 * g + 0.114 * b < 128.0
    }

    fn reply(&self, which: u8, st: bool) -> Option<Vec<u8>> {
        let [r, g, b] = match which {
            10 => self.fg.unwrap_or(if self.is_dark() { [0xee; 3] } else { [0x11; 3] }),
            11 => self.bg,
            _ => return None,
        };
        let end = if st { "\x1b\\" } else { "\x07" };
        Some(format!("\x1b]{which};rgb:{r:02x}{r:02x}/{g:02x}{g:02x}/{b:02x}{b:02x}{end}").into_bytes())
    }

    fn theme_report(&self) -> Vec<u8> {
        format!("\x1b[?997;{}n", if self.is_dark() { 1 } else { 2 }).into_bytes()
    }
}

static COLORS: RwLock<Option<TerminalColors>> = RwLock::new(None);
/// Bumped on every change, so panes can tell their agents once each.
static GENERATION: AtomicU64 = AtomicU64::new(0);

pub fn terminal_colors() -> Option<TerminalColors> {
    COLORS.read().ok().and_then(|guard| *guard)
}

pub fn colors_generation() -> u64 {
    GENERATION.load(Ordering::SeqCst)
}

/// Record the terminal's colors; true when they changed.
pub fn set_terminal_colors(colors: TerminalColors) -> bool {
    let Ok(mut guard) = COLORS.write() else {
        return false;
    };
    if *guard == Some(colors) {
        return false;
    }
    *guard = Some(colors);
    GENERATION.fetch_add(1, Ordering::SeqCst);
    true
}

pub fn parse_hex_color(value: &str) -> Option<[u8; 3]> {
    let hex = value.trim().strip_prefix('#')?;
    if hex.len() != 6 || !hex.bytes().all(|b| b.is_ascii_hexdigit()) {
        return None;
    }
    let byte = |i: usize| u8::from_str_radix(&hex[i..i + 2], 16).ok();
    Some([byte(0)?, byte(2)?, byte(4)?])
}

/// `rgb:R/G/B` with 1 to 4 hex digits a channel (an OSC 10/11 answer).
pub fn parse_xparse_color(value: &str) -> Option<[u8; 3]> {
    let rest = value.strip_prefix("rgb:")?;
    let mut out = [0_u8; 3];
    let mut parts = rest.split('/');
    for slot in &mut out {
        let part = parts.next()?;
        if part.is_empty() || part.len() > 4 {
            return None;
        }
        let max = (1_u32 << (part.len() * 4)) - 1;
        let n = u32::from_str_radix(part, 16).ok()?;
        *slot = ((n * 255 + max / 2) / max) as u8;
    }
    parts.next().is_none().then_some(out)
}

pub fn colors_file() -> Option<PathBuf> {
    let home = std::env::var_os("RUDDER_HOME")
        .filter(|value| !value.is_empty())
        .map(PathBuf::from)
        .or_else(|| {
            std::env::var_os("HOME")
                .filter(|value| !value.is_empty())
                .map(|home| PathBuf::from(home).join(".rudder"))
        })?;
    Some(home.join("terminal-colors.json"))
}

pub fn read_colors_file(path: &Path) -> Option<TerminalColors> {
    let text = std::fs::read_to_string(path).ok()?;
    let value: serde_json::Value = serde_json::from_str(&text).ok()?;
    let bg = parse_hex_color(value.get("bg")?.as_str()?)?;
    let fg = value.get("fg").and_then(|v| v.as_str()).and_then(parse_hex_color);
    Some(TerminalColors { fg, bg })
}

/// Re-reads the colors file when it changes. Cheap to call every second: one
/// `stat` unless the file moved.
#[derive(Default)]
pub struct ColorsFileWatch {
    seen: Option<SystemTime>,
}

impl ColorsFileWatch {
    pub fn poll(&mut self) -> bool {
        let Some(path) = colors_file() else {
            return false;
        };
        let Ok(modified) = std::fs::metadata(&path).and_then(|meta| meta.modified()) else {
            return false;
        };
        if self.seen == Some(modified) {
            return false;
        }
        self.seen = Some(modified);
        read_colors_file(&path).is_some_and(set_terminal_colors)
    }
}

#[derive(Debug, Default)]
enum ScanState {
    #[default]
    Ground,
    Escape,
    Osc(Vec<u8>),
    OscEscape(Vec<u8>),
    Csi(Vec<u8>),
}

/// What an agent's output asked of the terminal, answered by the pane.
#[derive(Debug, Default)]
pub struct QueryScanner {
    state: ScanState,
    /// The agent turned on mode 2031: tell it when the appearance changes.
    pub wants_theme_reports: bool,
}

impl QueryScanner {
    /// Scan output bytes; the replies they call for (empty while the colors are
    /// unknown, which leaves the agent to its own default as before).
    pub fn scan(&mut self, bytes: &[u8]) -> Vec<u8> {
        let mut replies = Vec::new();
        for &byte in bytes {
            self.state = match std::mem::take(&mut self.state) {
                ScanState::Ground if byte == 0x1b => ScanState::Escape,
                ScanState::Ground => ScanState::Ground,
                ScanState::Escape if byte == b']' => ScanState::Osc(Vec::new()),
                ScanState::Escape if byte == b'[' => ScanState::Csi(Vec::new()),
                ScanState::Escape if byte == 0x1b => ScanState::Escape,
                ScanState::Escape => ScanState::Ground,
                ScanState::Osc(payload) if byte == 0x07 => {
                    self.osc(&payload, false, &mut replies);
                    ScanState::Ground
                }
                ScanState::Osc(payload) if byte == 0x1b => ScanState::OscEscape(payload),
                ScanState::Osc(mut payload) => {
                    // Only short queries matter; a long OSC (a clipboard blob)
                    // is followed to its end without being kept.
                    if payload.len() < 16 {
                        payload.push(byte);
                    } else {
                        payload.clear();
                        payload.push(b'x');
                    }
                    ScanState::Osc(payload)
                }
                ScanState::OscEscape(payload) if byte == b'\\' => {
                    self.osc(&payload, true, &mut replies);
                    ScanState::Ground
                }
                ScanState::OscEscape(_) if byte == 0x1b => ScanState::Escape,
                ScanState::OscEscape(_) => ScanState::Ground,
                ScanState::Csi(mut params) => {
                    if (0x40..=0x7e).contains(&byte) {
                        self.csi(&params, byte, &mut replies);
                        ScanState::Ground
                    } else if params.len() < 16 {
                        params.push(byte);
                        ScanState::Csi(params)
                    } else {
                        ScanState::Ground
                    }
                }
            };
        }
        replies
    }

    fn osc(&mut self, payload: &[u8], st: bool, replies: &mut Vec<u8>) {
        let which = match payload {
            b"10;?" => 10,
            b"11;?" => 11,
            _ => return,
        };
        if let Some(reply) = terminal_colors().and_then(|colors| colors.reply(which, st)) {
            replies.extend_from_slice(&reply);
        }
    }

    fn csi(&mut self, params: &[u8], final_byte: u8, replies: &mut Vec<u8>) {
        match (params, final_byte) {
            (b"?2031", b'h') => self.wants_theme_reports = true,
            (b"?2031", b'l') => self.wants_theme_reports = false,
            (b"?996", b'n') => {
                if let Some(colors) = terminal_colors() {
                    replies.extend_from_slice(&colors.theme_report());
                }
            }
            _ => {}
        }
    }
}

/// The notice an agent that asked for mode 2031 gets when the colors change.
pub fn theme_change_report() -> Option<Vec<u8>> {
    terminal_colors().map(|colors| colors.theme_report())
}

/// Ask the dashboard's own terminal for its colors, once, before the dashboard
/// reads keys. DA1 (`CSI c`) goes last: every terminal answers it, so one that
/// ignores OSC 10/11 costs no wait. Raw mode must already be on.
#[cfg(unix)]
pub fn query_outer_terminal(timeout: std::time::Duration) -> Option<TerminalColors> {
    use std::io::Write;
    use std::time::Instant;
    let mut stdout = std::io::stdout();
    stdout
        .write_all(b"\x1b]10;?\x1b\\\x1b]11;?\x1b\\\x1b[c")
        .and_then(|()| stdout.flush())
        .ok()?;
    let deadline = Instant::now() + timeout;
    let mut got = Vec::new();
    loop {
        let left = deadline.saturating_duration_since(Instant::now());
        if left.is_zero() {
            break;
        }
        let mut pfd = libc::pollfd { fd: 0, events: libc::POLLIN, revents: 0 };
        let ready = unsafe { libc::poll(&mut pfd, 1, left.as_millis().max(1) as i32) };
        if ready <= 0 {
            break;
        }
        let mut buf = [0_u8; 256];
        let n = unsafe { libc::read(0, buf.as_mut_ptr().cast(), buf.len()) };
        if n <= 0 {
            break;
        }
        got.extend_from_slice(&buf[..n as usize]);
        // The DA1 answer (`CSI ? ... c`) comes after the color answers.
        if let Some(start) = find(&got, b"\x1b[?") {
            if got[start..].iter().any(|&b| b == b'c') {
                break;
            }
        }
    }
    parse_color_replies(&got)
}

#[cfg(not(unix))]
pub fn query_outer_terminal(_timeout: std::time::Duration) -> Option<TerminalColors> {
    None
}

fn find(haystack: &[u8], needle: &[u8]) -> Option<usize> {
    haystack.windows(needle.len()).position(|window| window == needle)
}

/// Colors out of a terminal's OSC 10/11 answers.
pub fn parse_color_replies(bytes: &[u8]) -> Option<TerminalColors> {
    let text = String::from_utf8_lossy(bytes);
    let mut fg = None;
    let mut bg = None;
    for part in text.split("\x1b]").skip(1) {
        let body = part.split(['\x07', '\x1b']).next().unwrap_or("");
        if let Some(value) = body.strip_prefix("10;") {
            fg = parse_xparse_color(value);
        } else if let Some(value) = body.strip_prefix("11;") {
            bg = parse_xparse_color(value);
        }
    }
    Some(TerminalColors { fg, bg: bg? })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_terminal_answers() {
        let answers = b"\x1b]10;rgb:1c1c/1c1c/1c1c\x1b\\\x1b]11;rgb:ffff/ffff/fefe\x07\x1b[?62;22c";
        let colors = parse_color_replies(answers).unwrap();
        assert_eq!(colors.bg, [0xff, 0xff, 0xfe]);
        assert_eq!(colors.fg, Some([0x1c, 0x1c, 0x1c]));
        assert!(!colors.is_dark());
        assert_eq!(parse_xparse_color("rgb:f/8/0"), Some([255, 136, 0]));
        assert_eq!(parse_color_replies(b"\x1b[?62c"), None);
    }

    #[test]
    fn answers_queries_in_the_terminator_they_used() {
        let colors = TerminalColors { fg: Some([0x11, 0x22, 0x33]), bg: [0xfa, 0xfa, 0xfa] };
        assert_eq!(colors.reply(11, true).unwrap(), b"\x1b]11;rgb:fafa/fafa/fafa\x1b\\".to_vec());
        assert_eq!(colors.reply(10, false).unwrap(), b"\x1b]10;rgb:1111/2222/3333\x07".to_vec());
        assert_eq!(colors.theme_report(), b"\x1b[?997;2n".to_vec());
    }

    #[test]
    fn scanner_follows_mode_2031_across_split_chunks() {
        let mut scanner = QueryScanner::default();
        // Colors unknown in this process-global may be set by another test;
        // only the mode tracking is asserted here.
        scanner.scan(b"\x1b[?20");
        scanner.scan(b"31h text");
        assert!(scanner.wants_theme_reports);
        scanner.scan(b"\x1b[?2031l");
        assert!(!scanner.wants_theme_reports);
    }

    #[test]
    fn scanner_answers_once_colors_are_known() {
        set_terminal_colors(TerminalColors { fg: None, bg: [0xff, 0xff, 0xff] });
        let mut scanner = QueryScanner::default();
        let replies = scanner.scan(b"\x1b]10;?\x07\x1b]11;?\x1b\\\x1b]0;title\x07\x1b[?996n");
        assert_eq!(
            replies,
            b"\x1b]10;rgb:1111/1111/1111\x07\x1b]11;rgb:ffff/ffff/ffff\x1b\\\x1b[?997;2n".to_vec()
        );
        // A long OSC is not mistaken for a query.
        assert!(scanner.scan(b"\x1b]52;c;aGVsbG8gd29ybGQgMTE7Pw==\x07").is_empty());
    }

    #[test]
    fn reads_the_supervisor_file() {
        let dir = std::env::temp_dir().join(format!("rudder-colors-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("terminal-colors.json");
        std::fs::write(&path, r##"{"fg":"#222222","bg":"#f6f4ef"}"##).unwrap();
        let colors = read_colors_file(&path).unwrap();
        assert_eq!(colors.bg, [0xf6, 0xf4, 0xef]);
        assert_eq!(colors.fg, Some([0x22, 0x22, 0x22]));
        let _ = std::fs::remove_dir_all(dir);
    }
}

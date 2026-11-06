//! Central truecolor theme for the Rudder dashboard.
//!
//! The default color mode leaves the terminal foreground/background untouched so
//! Rudder blends with the user's tab bar and terminal theme. A saved `paper` mode
//! keeps the previous pure-white canvas for users who prefer that look. Accent,
//! status, and selection colors remain explicit in both modes.
#![allow(dead_code)]
use super::*;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum ColorMode {
    Terminal,
    Paper,
}

impl ColorMode {
    pub(crate) fn parse(value: &str) -> Option<Self> {
        match normalize_color_mode(value).as_str() {
            "terminal" | "term" | "default" | "native" | "transparent" | "terminalbackground"
            | "terminalbg" | "bg" => Some(Self::Terminal),
            "paper" | "white" | "light" => Some(Self::Paper),
            _ => None,
        }
    }

    pub(crate) fn as_str(self) -> &'static str {
        match self {
            Self::Terminal => "terminal",
            Self::Paper => "paper",
        }
    }

    pub(crate) fn label(self) -> &'static str {
        match self {
            Self::Terminal => "terminal background",
            Self::Paper => "paper white",
        }
    }

    pub(crate) fn toggled(self) -> Self {
        match self {
            Self::Terminal => Self::Paper,
            Self::Paper => Self::Terminal,
        }
    }
}

fn normalize_color_mode(value: &str) -> String {
    value
        .trim()
        .to_ascii_lowercase()
        .chars()
        .filter(|ch| ch.is_ascii_alphanumeric())
        .collect()
}

thread_local! {
    static CURRENT_COLOR_MODE: std::cell::Cell<ColorMode> =
        const { std::cell::Cell::new(ColorMode::Terminal) };
}

pub(crate) fn set_color_mode(mode: ColorMode) {
    CURRENT_COLOR_MODE.with(|current| current.set(mode));
}

pub(crate) fn color_mode() -> ColorMode {
    CURRENT_COLOR_MODE.with(std::cell::Cell::get)
}

pub(crate) fn terminal_background_mode() -> bool {
    color_mode() == ColorMode::Terminal
}

// --- Core palette. PAPER is used as the optional paper-mode canvas and as
// high-contrast text on filled accent badges.
/// The optional paper-mode canvas.
pub(crate) const PAPER: Color = Color::Rgb(0xFF, 0xFF, 0xFF);
/// Primary text: near-black ink (matches the site's #1a1a1a).
pub(crate) const INK: Color = Color::Rgb(0x1A, 0x1A, 0x1A);
/// Primary focus accent: a teal that reads cleanly on white.
pub(crate) const ACCENT: Color = Color::Rgb(0x0B, 0x74, 0x7C);
/// Slightly deeper teal used as the fill behind the focused pane title.
pub(crate) const ACCENT_DEEP: Color = Color::Rgb(0x0B, 0x6E, 0x74);
/// Amber secondary accent (amber-700), legible on white.
pub(crate) const ACCENT_2: Color = Color::Rgb(0xB4, 0x53, 0x09);
/// Readable cool gray for secondary text.
pub(crate) const MUTED: Color = Color::Rgb(0x6B, 0x72, 0x80);
/// Light gray for hairline borders and de-emphasized text on white.
pub(crate) const FAINT: Color = Color::Rgb(0xA6, 0xAD, 0xB8);
/// Pale teal background for the selection highlight (with ink text on top).
pub(crate) const SURFACE_SEL: Color = Color::Rgb(0xD9, 0xEE, 0xF0);

// --- Legacy const names, re-pointed to the new palette ---
// Kept so render.rs inline modals, selection.rs highlights, and the status
// helpers below continue to compile and automatically adopt the new look.
pub(crate) const FOCUS_COLOR: Color = ACCENT;
pub(crate) const INACTIVE_COLOR: Color = FAINT;
pub(crate) const MODEL_COLOR: Color = Color::Rgb(0x7E, 0x22, 0xCE); // purple-700
pub(crate) const RUNNING_COLOR: Color = Color::Rgb(0xB4, 0x53, 0x09); // amber-700
pub(crate) const DONE_COLOR: Color = Color::Rgb(0x15, 0x80, 0x3D); // green-700
pub(crate) const FAILED_COLOR: Color = Color::Rgb(0xC0, 0x39, 0x2B); // red-600
pub(crate) const CLOUD_COLOR: Color = ACCENT; // teal

// --- DAG node status colors. Distinct + quiet so no state reads as gray, all
// legible on white. ---
pub(crate) const ST_PLANNED: Color = Color::Rgb(0x47, 0x55, 0x69); // slate-600
pub(crate) const ST_READY: Color = ACCENT; // teal
pub(crate) const ST_RUNNING: Color = RUNNING_COLOR; // amber
pub(crate) const ST_REVIEW: Color = Color::Rgb(0xA2, 0x1C, 0xAF); // magenta-700
pub(crate) const ST_BLOCKED: Color = Color::Rgb(0xC2, 0x41, 0x0C); // orange-700
pub(crate) const ST_MERGED: Color = DONE_COLOR; // green
pub(crate) const ST_FAILED: Color = FAILED_COLOR; // red

/// Filled-circle glyph used as a colored status badge in the agent list.
pub(crate) const BADGE: &str = "\u{25CF}"; // ●

/// Color for an agent's status. Done is green, merged takes the accent (the
/// work landed), and anything that needs you reads amber.
pub(crate) fn status_color(status: AgentStatus) -> Color {
    let p = pal();
    match status {
        AgentStatus::Running => p.running,
        AgentStatus::Done => p.done,
        AgentStatus::Merged => p.merged,
        AgentStatus::Failed | AgentStatus::Orphaned => p.failed,
        AgentStatus::Stopped | AgentStatus::Paused => p.muted,
        AgentStatus::Migrated => p.accent,
    }
}

/// Section-header style ("main" / "workspaces" / "merged"): a quiet, thin label.
/// No bold (the theme is thin); hierarchy comes from color + spacing.
pub(crate) fn header_style(focused: bool) -> Style {
    let p = pal();
    Style::default().fg(if focused { p.muted } else { p.faint })
}

// --- 4.2 palette ------------------------------------------------------------
//
// One accent, four status colors and three greys, tuned twice: once for dark
// terminals and once for light ones (and paper mode). Which one applies comes
// from the outer terminal's background (term_colors.rs asks it at startup), so
// the chrome follows the same light/dark answer the agents in the panes get.

/// The resolved palette for the current frame.
#[derive(Clone, Copy, Debug)]
pub(crate) struct Palette {
    pub accent: Color,
    pub running: Color,
    pub waiting: Color,
    pub done: Color,
    pub merged: Color,
    pub failed: Color,
    /// Secondary text.
    pub muted: Color,
    /// Hairlines and separators. Too low in contrast for words someone has to
    /// read; use `muted` or `term_fg()` for those.
    pub faint: Color,
}

const DARK: Palette = Palette {
    accent: Color::Rgb(0xA3, 0x94, 0xFF),
    running: Color::Rgb(0x6C, 0xB6, 0xFF),
    waiting: Color::Rgb(0xF5, 0xC2, 0x4C),
    done: Color::Rgb(0x5F, 0xD3, 0x8D),
    merged: Color::Rgb(0xA3, 0x94, 0xFF),
    failed: Color::Rgb(0xFF, 0x7A, 0x7A),
    muted: Color::Rgb(0x8C, 0x93, 0xA3),
    faint: Color::Rgb(0x4E, 0x55, 0x63),
};

const LIGHT: Palette = Palette {
    accent: Color::Rgb(0x5B, 0x45, 0xD6),
    running: Color::Rgb(0x1F, 0x6F, 0xEB),
    waiting: Color::Rgb(0xA8, 0x5B, 0x00),
    done: Color::Rgb(0x16, 0x83, 0x45),
    merged: Color::Rgb(0x5B, 0x45, 0xD6),
    failed: Color::Rgb(0xD0, 0x30, 0x30),
    muted: Color::Rgb(0x6B, 0x72, 0x80),
    faint: Color::Rgb(0xB4, 0xBA, 0xC4),
};

thread_local! {
    static DARK_CHROME: std::cell::Cell<bool> = const { std::cell::Cell::new(true) };
}

/// Re-read light or dark once per frame (a lock read, so not per span). An
/// unknown terminal is treated as dark, the common case; paper mode is light.
pub(crate) fn refresh_palette() {
    let dark = color_mode() == ColorMode::Terminal
        && rudder_native::term_colors::terminal_colors()
            .map(|colors| colors.is_dark())
            .unwrap_or(true);
    DARK_CHROME.with(|cell| cell.set(dark));
}

/// Pick the dark or light palette for this thread without asking the terminal,
/// so a test can draw either one. `render` re-reads it, so draw the widget
/// directly.
#[cfg(test)]
pub(crate) fn set_dark_chrome_for_test(dark: bool) {
    DARK_CHROME.with(|cell| cell.set(dark));
}

pub(crate) fn pal() -> Palette {
    if DARK_CHROME.with(std::cell::Cell::get) {
        DARK
    } else {
        LIGHT
    }
}

/// A typical dark terminal's background and text, for a terminal that did not
/// report its colors. The light variant uses PAPER and INK.
const DARK_CANVAS: Color = Color::Rgb(0x12, 0x12, 0x18);
const DARK_INK: Color = Color::Rgb(0xE6, 0xE6, 0xF0);

/// The canvas behind the chrome: the terminal's own background when it told
/// us, otherwise a typical dark or light one. Fades and flashes blend toward it.
pub(crate) fn canvas() -> Color {
    if color_mode() == ColorMode::Paper {
        return PAPER;
    }
    if let Some(colors) = rudder_native::term_colors::terminal_colors() {
        let [r, g, b] = colors.bg;
        return Color::Rgb(r, g, b);
    }
    if DARK_CHROME.with(std::cell::Cell::get) {
        DARK_CANVAS
    } else {
        PAPER
    }
}

/// Default text color for blending: the terminal's foreground when known.
pub(crate) fn ink() -> Color {
    if color_mode() == ColorMode::Paper {
        return INK;
    }
    if let Some([r, g, b]) = rudder_native::term_colors::terminal_colors().and_then(|c| c.fg) {
        return Color::Rgb(r, g, b);
    }
    if DARK_CHROME.with(std::cell::Cell::get) {
        DARK_INK
    } else {
        INK
    }
}

/// The terminal's own text color, for words that must always be readable
/// (notices, the keys in the footer). `Reset` inherits the user's theme, so it
/// reads on a light, dark or unreported background alike; paper mode keeps ink.
pub(crate) fn term_fg() -> Color {
    if terminal_background_mode() {
        Color::Reset
    } else {
        INK
    }
}

/// Mix `from` toward `to` by `t` (0 keeps `from`, 1 is `to`). Non-RGB colors
/// snap at the halfway point instead of blending.
pub(crate) fn blend(from: Color, to: Color, t: f32) -> Color {
    let t = t.clamp(0.0, 1.0);
    match (from, to) {
        (Color::Rgb(r1, g1, b1), Color::Rgb(r2, g2, b2)) => {
            let mix = |a: u8, b: u8| (f32::from(a) + (f32::from(b) - f32::from(a)) * t).round() as u8;
            Color::Rgb(mix(r1, r2), mix(g1, g2), mix(b1, b2))
        }
        _ if t < 0.5 => from,
        _ => to,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// WCAG 2 contrast ratio between two RGB colors, from 1 to 21.
    fn contrast(a: Color, b: Color) -> f64 {
        fn luminance(color: Color) -> f64 {
            let Color::Rgb(r, g, b) = color else {
                panic!("not an RGB color: {color:?}");
            };
            let channel = |c: u8| {
                let c = f64::from(c) / 255.0;
                if c <= 0.04045 {
                    c / 12.92
                } else {
                    ((c + 0.055) / 1.055).powf(2.4)
                }
            };
            0.2126 * channel(r) + 0.7152 * channel(g) + 0.0722 * channel(b)
        }
        let (la, lb) = (luminance(a), luminance(b));
        (la.max(lb) + 0.05) / (la.min(lb) + 0.05)
    }

    struct RestoreChrome;
    impl Drop for RestoreChrome {
        fn drop(&mut self) {
            DARK_CHROME.with(|cell| cell.set(true));
            set_color_mode(ColorMode::Terminal);
        }
    }

    #[test]
    fn notice_pill_and_footer_text_read_on_dark_and_light_terminals() {
        let _restore = RestoreChrome;
        // Each variant against the background and text a terminal of that kind
        // typically has: what canvas() and ink() fall back to when it does not
        // report its colors. `Reset` is the terminal's own text color.
        let variants = [("dark", true, DARK_CANVAS, DARK_INK), ("light", false, PAPER, INK)];
        for (variant, dark, bg, fg) in variants {
            set_color_mode(ColorMode::Terminal);
            DARK_CHROME.with(|cell| cell.set(dark));
            let resolve = |color: Option<Color>| match color {
                None | Some(Color::Reset) => fg,
                Some(color) => color,
            };
            for kind in [NoticeKind::Info, NoticeKind::Error, NoticeKind::Confirm] {
                let colors = notice_pill_colors(kind);
                // The words inherit the terminal's theme instead of a fixed
                // color that only suits one background.
                assert_eq!(colors.text.fg, Some(Color::Reset), "{variant} {kind:?} pill text");
                assert!(
                    !colors.text.add_modifier.contains(Modifier::DIM),
                    "{variant} {kind:?} pill text is dim"
                );
                let text = contrast(resolve(colors.text.fg), bg);
                assert!(text >= 4.5, "{variant} {kind:?} pill text contrast {text:.2}");
                let glyph = contrast(colors.glyph, bg);
                assert!(glyph >= 4.5, "{variant} {kind:?} pill glyph contrast {glyph:.2}");
                let border = contrast(colors.border, bg);
                assert!(border >= 3.0, "{variant} {kind:?} pill border contrast {border:.2}");
            }
            assert!(
                notice_pill_colors(NoticeKind::Confirm)
                    .text
                    .add_modifier
                    .contains(Modifier::BOLD),
                "{variant}: a pending confirmation is the most legible notice"
            );
            // A notice settled into the footer, and the footer's key labels.
            for text in ["deleted agent from dashboard", "merge stopped: boom"] {
                let ratio = contrast(resolve(notice_style(text).fg), bg);
                assert!(ratio >= 4.5, "{variant} footer notice {text:?} contrast {ratio:.2}");
            }
            let muted = contrast(pal().muted, bg);
            assert!(muted >= 4.5, "{variant} muted contrast {muted:.2}");
        }
        // Paper mode paints its own canvas, so the words are ink, not Reset.
        set_color_mode(ColorMode::Paper);
        DARK_CHROME.with(|cell| cell.set(false));
        assert_eq!(notice_pill_colors(NoticeKind::Confirm).text.fg, Some(INK));
        assert!(contrast(INK, PAPER) >= 4.5);
    }
}

//! The selected agent's context and time, graphed at the bottom of the
//! sidebar: how full the window is, the fixed base versus the conversation,
//! context size per request, and where the time went.
//!
//! Render only. The numbers are the `TimelineSummary` that timeline.rs parses
//! off the App loop into `app.diff_panel.timeline`; nothing here reads a file.
//! The panel is the only place context shows: the agent card has none, and a
//! sidebar without room (narrow layout, short terminal, worker full screen)
//! shows nothing.

use super::*;
use crate::timeline::{format_duration_ms, ContextUsage, TimelineSummary};
use unicode_width::UnicodeWidthStr;

/// Rows the panel takes with graphs: three headers, four bars and legends.
pub(crate) const PANEL_HEIGHT: u16 = 13;
/// A backend without usage gets a header and one line instead.
const NOTE_HEIGHT: u16 = 2;
const COMPACT_HEIGHT: u16 = 4;
const GROWTH_HEIGHT: u16 = 6;
/// The agent list keeps at least this many rows, or the panel stays hidden.
pub(crate) const MIN_LIST_ROWS: u16 = 10;

const BAR: &str = "\u{2588}"; // █
const FREE: &str = "\u{2591}"; // ░
const SWATCH: &str = "\u{25a0}"; // ■
const MARKER: &str = "\u{2503}"; // ┃
const SPARKS: [&str; 8] = ["▁", "▂", "▃", "▄", "▅", "▆", "▇", "█"];

pub(crate) enum Panel<'a> {
    Note(&'static str),
    Usage { summary: Option<&'a TimelineSummary>, window: Option<u64> },
}

fn panel_for(app: &App) -> Option<Panel<'_>> {
    let agent = app.agents.get(app.selected_agent)?;
    if is_cloud_agent(agent) || agent.backend == Backend::Opencode {
        return Some(Panel::Note("usage unavailable for this backend"));
    }
    if agent.plain_process {
        return Some(Panel::Note("no model usage in this pane"));
    }
    let summary = app
        .diff_panel
        .timeline
        .as_ref()
        .filter(|(id, _)| *id == agent.id)
        .map(|(_, summary)| summary);
    Some(Panel::Usage { summary, window: claude_window(agent) })
}

/// Claude Code does not log its window size. A `[1m]` model is the one case
/// where the run itself says it: the user picked the 1M window.
fn claude_window(agent: &AgentRun) -> Option<u64> {
    (agent.backend == Backend::Claude && agent.model.to_ascii_lowercase().ends_with("[1m]"))
        .then_some(1_000_000)
}

/// Take the panel's rows off the bottom of the sidebar. Only the side-by-side
/// layout has one (the gutter is the tell: never the narrow layout or a list
/// that fills the band), and only while the list keeps `MIN_LIST_ROWS`.
pub(crate) fn carve(app: &App, mut layout: SplitLayout) -> (SplitLayout, Option<Rect>) {
    let (Some(sidebar), Some(_)) = (layout.agents, layout.agents_gutter) else {
        return (layout, None);
    };
    let Some(panel) = panel_for(app) else {
        return (layout, None);
    };
    let Some(height) = panel_height(&panel, sidebar) else {
        return (layout, None);
    };
    let list = Rect { height: sidebar.height - height, ..sidebar };
    layout.agents = Some(list);
    (layout, Some(Rect { y: list.bottom(), height, ..sidebar }))
}

/// Charts take at most a third of the sidebar, leaving the agent list useful
/// on ordinary terminals. Waiting and unsupported panes need only two rows.
fn panel_height(panel: &Panel<'_>, sidebar: Rect) -> Option<u16> {
    if sidebar.width < 24 {
        return None;
    }
    let has_context = matches!(panel, Panel::Usage { summary: Some(summary), .. }
        if summary.context.as_ref().is_some_and(|c| c.input > 0));
    let height = if !has_context {
        NOTE_HEIGHT
    } else {
        let budget = (sidebar.height / 3).min(sidebar.height.saturating_sub(MIN_LIST_ROWS));
        [PANEL_HEIGHT, GROWTH_HEIGHT, COMPACT_HEIGHT].into_iter().find(|height| *height <= budget)?
    };
    (sidebar.height >= MIN_LIST_ROWS + height).then_some(height)
}

pub(crate) fn render(frame: &mut Frame<'_>, area: Rect, app: &App) {
    let Some(panel) = panel_for(app) else {
        return;
    };
    // The same one-column inset as the list above it.
    let inner = Rect { x: area.x.saturating_add(1), width: area.width.saturating_sub(2), ..area };
    frame.render_widget(Block::default().style(app_style()), area);
    frame.render_widget(Paragraph::new(lines_for_height(&panel, inner.width as usize, inner.height)).style(app_style()), inner);
}

fn lines_for_height(panel: &Panel<'_>, width: usize, height: u16) -> Vec<Line<'static>> {
    let mut lines = panel_lines(panel, width);
    if height == GROWTH_HEIGHT && lines.len() >= 9 {
        // Keep the fill and its cache legend, followed by request growth.
        // The base estimate and timing graphs belong to the expanded panel.
        lines.drain(4..7);
    }
    lines.truncate(height as usize);
    lines
}

pub(crate) fn panel_lines(panel: &Panel<'_>, width: usize) -> Vec<Line<'static>> {
    match panel {
        Panel::Note(note) => vec![header("Context", String::new(), width), muted_line(note)],
        Panel::Usage { summary, window } => usage_lines(*summary, *window, width),
    }
}

fn usage_lines(summary: Option<&TimelineSummary>, window: Option<u64>, width: usize) -> Vec<Line<'static>> {
    let mut lines = Vec::new();
    match summary.and_then(|s| s.context.as_ref()).filter(|c| c.input > 0) {
        Some(context) => context_lines(context, window, width, &mut lines),
        None => {
            lines.push(header("Context", String::new(), width));
            lines.push(muted_line("waiting for the first request"));
        }
    }
    lines.resize(9, Line::default());
    if let Some(summary) = summary {
        time_lines(summary, width, &mut lines);
    }
    lines
}

/// Rows 0-8: fill bar and legend, base bar and legend, growth sparkline.
fn context_lines(c: &ContextUsage, window: Option<u64>, width: usize, lines: &mut Vec<Line<'static>>) {
    let p = pal();
    let input = c.input;
    let window = c.capacity.or(window).filter(|n| *n > 0);
    let compact = c.auto_compact_at.filter(|n| *n > 0);
    // The bar's right end: the window when it is known, else the point where
    // this conversation last compacted on its own, else just what is used.
    let (scale, free_label) = match (window, compact) {
        (Some(w), _) => (w.max(input), Some("free")),
        (None, Some(k)) => (k.max(input), Some("to compact")),
        (None, None) => (input, None),
    };
    let marker = compact.filter(|k| window.is_some() && *k < scale);
    let used_cells = segment_widths(&[input, scale - input], width)[0];
    let free = scale - input;

    let right = if scale > input || window.is_some() {
        format!("{} / {} · {}%", tokens(input), tokens(scale), percent(input, scale))
    } else {
        tokens(input)
    };
    lines.push(header("Context", right, width));

    // 1. Fill: cached, cache write and fresh input, then free space.
    let (cached, write) = (c.cached.unwrap_or(0).min(input), c.cache_write.unwrap_or(0));
    let write = write.min(input - cached);
    let fresh = input - cached - write;
    let parts: Vec<(u64, Color)> = if c.cached.is_some() {
        vec![(cached, p.done), (write, p.waiting), (fresh, p.running)]
    } else {
        vec![(input, p.running)]
    };
    let mut cells = filled_cells(&parts, used_cells, width);
    if let Some(at) = marker {
        let col = ((at as f64 / scale as f64) * width as f64).round() as usize;
        if let Some(cell) = cells.get_mut(col.min(width.saturating_sub(1))) {
            *cell = (MARKER, p.failed);
        }
    }
    lines.push(cells_line(&cells));
    let mut items = Vec::new();
    if c.cached.is_some() {
        items.push(item(SWATCH, p.done, format!("cached {}", tokens(cached)), 0));
        if c.cache_write.is_some() {
            items.push(item(SWATCH, p.waiting, format!("write {}", tokens(write)), 1));
        }
        items.push(item(SWATCH, p.running, format!("fresh {}", tokens(fresh)), 2));
    } else {
        items.push(item(SWATCH, p.running, format!("input {}", tokens(input)), 0));
    }
    if let Some(at) = marker {
        items.push(item(MARKER, p.failed, format!("compacted at {}", tokens(at)), 3));
    }
    match free_label {
        // Without a window, the room left before compaction is the point.
        Some(label) => items.push(item(FREE, p.faint, format!("{label} {}", tokens(free)), if window.is_some() { 4 } else { 3 })),
        None => items.push(item("", p.muted, "window size unknown".into(), 5)),
    }
    lines.extend(legend(items, width, 2));

    // 2. Where it comes from: the first request is the fixed base (system
    // prompt, tools, memory files, plus the opening prompt, hence "about").
    match c.base {
        Some(base) => {
            let base = base.min(input);
            let conversation = blend(p.accent, canvas(), 0.45);
            let cells = filled_cells(&[(base, p.accent), (input - base, conversation)], used_cells, width);
            lines.push(cells_line(&cells));
            lines.extend(legend(
                vec![
                    item(SWATCH, p.accent, format!("base ~{} (first request)", tokens(base)), 0),
                    item(SWATCH, conversation, format!("conversation {}", tokens(input - base)), 1),
                ],
                width,
                2,
            ));
        }
        None => {
            lines.push(muted_line("base shows after the next request"));
            lines.extend([Line::default(), Line::default()]);
        }
    }

    // 3. Growth: context size per request, so compactions show as drops.
    let shown = &c.history[c.history.len().saturating_sub(width)..];
    let peak = shown.iter().copied().max().unwrap_or(0);
    let right = if shown.is_empty() {
        String::new()
    } else {
        format!("{} requests · peak {}", shown.len(), tokens(peak))
    };
    lines.push(header("Growth", right, width));
    if shown.is_empty() {
        lines.push(muted_line("shows after the next request"));
    } else {
        lines.push(Line::from(Span::styled(sparkline(shown, peak), Style::default().fg(p.accent))));
    }
}

/// Rows 9-12: where the agent's time went.
fn time_lines(s: &TimelineSummary, width: usize, lines: &mut Vec<Line<'static>>) {
    let p = pal();
    let parts = [
        ("model", s.model_ms, p.accent),
        ("tools", s.tool_ms, p.running),
        ("install", s.install_ms, blend(p.running, p.done, 0.5)),
        ("build", s.build_ms, p.waiting),
        ("test", s.test_ms, p.done),
        ("wait", s.wait_ms, p.muted),
    ];
    let total: u64 = parts.iter().map(|(_, ms, _)| ms).sum();
    if total == 0 {
        return;
    }
    lines.push(header("Time", format_duration_ms(total), width));
    let widths = segment_widths(&parts.map(|(_, ms, _)| ms), width);
    let mut cells = Vec::with_capacity(width);
    for ((_, _, color), cells_for) in parts.iter().zip(widths) {
        cells.extend(std::iter::repeat_n((BAR, *color), cells_for));
    }
    lines.push(cells_line(&cells));
    // The smallest shares drop first when the legend runs out of room.
    let mut by_size: Vec<u64> = parts.iter().map(|(_, ms, _)| *ms).filter(|ms| *ms > 0).collect();
    by_size.sort_unstable_by(|a, b| b.cmp(a));
    let items = parts
        .iter()
        .filter(|(_, ms, _)| *ms > 0)
        .map(|(label, ms, color)| {
            let pct = percent(*ms, total);
            let pct = if pct == 0 { "<1".to_string() } else { pct.to_string() };
            let rank = by_size.iter().position(|v| v == ms).unwrap_or(0);
            item(SWATCH, *color, format!("{label} {pct}%"), rank)
        })
        .collect();
    lines.extend(legend(items, width, 2));
}

/// Cells per part, summing to exactly `width` whenever any part is nonzero:
/// largest remainders round, and a part that is there keeps at least one cell
/// (taken from the widest) so a sliver never disappears.
pub(crate) fn segment_widths(values: &[u64], width: usize) -> Vec<usize> {
    let total: u128 = values.iter().map(|v| u128::from(*v)).sum();
    if total == 0 || width == 0 {
        return vec![0; values.len()];
    }
    let exact: Vec<u128> = values.iter().map(|v| u128::from(*v) * width as u128).collect();
    let mut cells: Vec<usize> = exact.iter().map(|e| (e / total) as usize).collect();
    let mut left = width - cells.iter().sum::<usize>();
    let mut order: Vec<usize> = (0..values.len()).collect();
    order.sort_by_key(|&i| std::cmp::Reverse(exact[i] % total));
    for i in order {
        if left == 0 {
            break;
        }
        cells[i] += 1;
        left -= 1;
    }
    for i in 0..values.len() {
        if values[i] > 0 && cells[i] == 0 {
            let widest = (0..cells.len()).max_by_key(|&j| cells[j]).unwrap_or(i);
            if cells[widest] > 1 {
                cells[widest] -= 1;
                cells[i] = 1;
            }
        }
    }
    cells
}

/// `used` cells split between `parts`, then free cells to `width`. Two bars
/// on one scale share `used`, so their filled ends line up.
fn filled_cells(parts: &[(u64, Color)], used: usize, width: usize) -> Vec<(&'static str, Color)> {
    let values: Vec<u64> = parts.iter().map(|(v, _)| *v).collect();
    let mut cells = Vec::with_capacity(width);
    for ((_, color), n) in parts.iter().zip(segment_widths(&values, used)) {
        cells.extend(std::iter::repeat_n((BAR, *color), n));
    }
    let faint = pal().faint;
    cells.resize(width.max(cells.len()), (FREE, faint));
    cells
}

fn cells_line(cells: &[(&'static str, Color)]) -> Line<'static> {
    let mut spans: Vec<Span<'static>> = Vec::new();
    let mut run = String::new();
    let mut run_color: Option<Color> = None;
    for (glyph, color) in cells {
        if run_color != Some(*color) && !run.is_empty() {
            spans.push(Span::styled(std::mem::take(&mut run), Style::default().fg(run_color.unwrap_or(Color::Reset))));
        }
        run_color = Some(*color);
        run.push_str(glyph);
    }
    if !run.is_empty() {
        spans.push(Span::styled(run, Style::default().fg(run_color.unwrap_or(Color::Reset))));
    }
    Line::from(spans)
}

fn sparkline(values: &[u64], peak: u64) -> String {
    values
        .iter()
        .map(|v| {
            let level = (u128::from(*v) * 8).div_ceil(u128::from(peak.max(1))) as usize;
            SPARKS[level.clamp(1, 8) - 1]
        })
        .collect()
}

/// `Label ──────── right`, the rule filling what the two leave.
fn header(label: &str, right: String, width: usize) -> Line<'static> {
    let p = pal();
    let taken = label.width() + right.width() + if right.is_empty() { 1 } else { 2 };
    let mut spans = vec![
        Span::styled(label.to_string(), Style::default().fg(p.muted)),
        Span::raw(" "),
        Span::styled("─".repeat(width.saturating_sub(taken)), Style::default().fg(p.faint)),
    ];
    if !right.is_empty() {
        spans.push(Span::raw(" "));
        spans.push(Span::styled(right, Style::default().fg(term_fg())));
    }
    Line::from(spans)
}

fn muted_line(text: &str) -> Line<'static> {
    Line::from(Span::styled(text.to_string(), Style::default().fg(pal().muted)))
}

struct Item {
    glyph: &'static str,
    color: Color,
    text: String,
    /// Lower stays longer when the rows run out.
    rank: usize,
}

fn item(glyph: &'static str, color: Color, text: String, rank: usize) -> Item {
    Item { glyph, color, text, rank }
}

impl Item {
    fn width(&self) -> usize {
        if self.glyph.is_empty() {
            self.text.width()
        } else {
            self.glyph.width() + 1 + self.text.width()
        }
    }
}

/// Exactly `rows` lines of legend. Items flow left to right; when they do not
/// fit, the gap narrows to one space, then the highest-ranked item drops.
fn legend(mut items: Vec<Item>, width: usize, rows: usize) -> Vec<Line<'static>> {
    loop {
        for gap in [2, 1] {
            if let Some(mut lines) = flow(&items, width, rows, gap) {
                lines.resize(rows, Line::default());
                return lines;
            }
        }
        let Some(drop) = (0..items.len()).max_by_key(|&i| items[i].rank) else {
            return vec![Line::default(); rows];
        };
        items.remove(drop);
    }
}

fn flow(items: &[Item], width: usize, rows: usize, gap: usize) -> Option<Vec<Line<'static>>> {
    let muted = pal().muted;
    let mut lines: Vec<Vec<Span<'static>>> = vec![Vec::new()];
    let mut used = 0;
    for item in items {
        let w = item.width();
        if used > 0 && used + gap + w > width {
            lines.push(Vec::new());
            used = 0;
        }
        if lines.len() > rows || w > width {
            return None;
        }
        let row = lines.last_mut()?;
        if used > 0 {
            row.push(Span::raw(" ".repeat(gap)));
            used += gap;
        }
        if !item.glyph.is_empty() {
            row.push(Span::styled(item.glyph, Style::default().fg(item.color)));
            row.push(Span::raw(" "));
        }
        row.push(Span::styled(item.text.clone(), Style::default().fg(muted)));
        used += w;
    }
    Some(lines.into_iter().map(Line::from).collect())
}

fn percent(part: u64, whole: u64) -> u64 {
    if whole == 0 {
        return 0;
    }
    ((u128::from(part) * 100 + u128::from(whole) / 2) / u128::from(whole)) as u64
}

/// Token counts as short as a narrow panel needs: `850`, `9.8k`, `142k`, `1.2M`.
pub(crate) fn tokens(n: u64) -> String {
    let trim = |s: String| s.replace(".0", "");
    if n < 1_000 {
        n.to_string()
    } else if n < 9_950 {
        trim(format!("{:.1}k", n as f64 / 1_000.0))
    } else if n < 999_500 {
        format!("{}k", (n as f64 / 1_000.0).round() as u64)
    } else {
        trim(format!("{:.1}M", n as f64 / 1_000_000.0))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn text(lines: &[Line<'_>]) -> Vec<String> {
        lines.iter().map(|l| l.spans.iter().map(|s| s.content.as_ref()).collect()).collect()
    }

    #[test]
    fn panel_adapts_to_sidebar_space_and_keeps_the_list() {
        let summary = TimelineSummary {
            context: Some(ContextUsage { input: 90_000, ..Default::default() }),
            ..Default::default()
        };
        let usage = Panel::Usage { summary: Some(&summary), window: None };
        for (height, expected) in [(11, None), (13, None), (14, Some(4)), (17, Some(4)), (18, Some(6)), (38, Some(6)), (39, Some(13))] {
            let chosen = panel_height(&usage, Rect::new(0, 0, 34, height));
            assert_eq!(chosen, expected, "sidebar height {height}");
            if let Some(rows) = chosen {
                assert!(height - rows >= MIN_LIST_ROWS);
                assert!(rows * 3 <= height);
            }
        }
        assert_eq!(panel_height(&usage, Rect::new(0, 0, 23, 48)), None);
        for panel in [Panel::Note("usage unavailable"), Panel::Usage { summary: None, window: None }] {
            assert_eq!(panel_height(&panel, Rect::new(0, 0, 34, 48)), Some(2));
            assert_eq!(panel_height(&panel, Rect::new(0, 0, 34, 12)), Some(2));
            assert_eq!(panel_height(&panel, Rect::new(0, 0, 34, 11)), None);
        }
    }

    #[test]
    fn compact_panel_keeps_context_cache_and_growth_without_inventing_capacity() {
        let summary = TimelineSummary {
            context: Some(ContextUsage {
                input: 90_000,
                cached: Some(60_000),
                base: Some(20_000),
                history: vec![20_000, 50_000, 90_000],
                ..Default::default()
            }),
            model_ms: 10_000,
            ..Default::default()
        };
        let panel = Panel::Usage { summary: Some(&summary), window: None };
        let compact = lines_for_height(&panel, 32, COMPACT_HEIGHT);
        assert_eq!(compact.len(), 4);
        assert_eq!(compact[1].width(), 32);
        let shown = text(&compact).join("\n");
        for want in ["Context", "90k", "cached 60k", "fresh 30k", "window size unknown"] {
            assert!(shown.contains(want), "{want} missing:\n{shown}");
        }
        assert!(!shown.contains('%'), "unknown capacity has no fill percentage");
        let growth = lines_for_height(&panel, 32, GROWTH_HEIGHT);
        assert_eq!(growth.len(), 6);
        let shown = text(&growth).join("\n");
        assert!(shown.contains("Growth"));
        assert!(shown.contains("3 requests"));
        assert!(!shown.contains("base ~"));
        assert!(!shown.contains("Time"));
    }

    #[test]
    fn bars_fill_the_width_exactly_and_counts_read_short() {
        for (values, width) in [
            (vec![98_000, 14_000, 30_000, 58_000], 32),
            (vec![1, 1, 1, 199_997], 32),
            (vec![5, 0, 7], 3),
            (vec![1, 2, 3, 4, 5, 6], 30),
        ] {
            let cells = segment_widths(&values, width);
            assert_eq!(cells.iter().sum::<usize>(), width, "{values:?}");
            for (v, n) in values.iter().zip(&cells) {
                assert_eq!(*v > 0, *n > 0, "a part that is there shows: {values:?} -> {cells:?}");
            }
        }
        assert_eq!(segment_widths(&[0, 0], 10), vec![0, 0]);
        assert_eq!(
            [850, 9_800, 10_000, 142_400, 999_600, 1_250_000].map(tokens),
            ["850", "9.8k", "10k", "142k", "1M", "1.2M"]
        );

        let summary = TimelineSummary {
            context: Some(ContextUsage {
                input: 142_000,
                cached: Some(98_000),
                cache_write: Some(14_000),
                capacity: Some(200_000),
                base: Some(38_000),
                history: vec![40_000, 90_000, 142_000],
                ..Default::default()
            }),
            model_ms: 60_000,
            tool_ms: 30_000,
            wait_ms: 10_000,
            ..Default::default()
        };
        let lines = usage_lines(Some(&summary), None, 32);
        assert_eq!(lines.len(), PANEL_HEIGHT as usize);
        for row in [1, 4, 10] {
            assert_eq!(lines[row].width(), 32, "bar row {row}");
        }
        let shown = text(&lines).join("\n");
        for want in ["142k / 200k · 71%", "cached 98k", "write 14k", "fresh 30k", "base ~38k", "conversation 104k", "3 requests · peak 142k", "model 60%", "wait 10%"] {
            assert!(shown.contains(want), "{want} missing:\n{shown}");
        }
    }
}

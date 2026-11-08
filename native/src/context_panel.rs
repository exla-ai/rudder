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
use crate::timeline::TimelineSummary;
use unicode_width::UnicodeWidthStr;

/// One thin meter, a small trace, and supporting stats.
pub(crate) const PANEL_HEIGHT: u16 = 7;
/// A backend without usage gets a header and one line instead.
const NOTE_HEIGHT: u16 = 2;
const COMPACT_HEIGHT: u16 = 3;
const GROWTH_HEIGHT: u16 = 5;
/// The agent list keeps at least this many rows, or the panel stays hidden.
pub(crate) const MIN_LIST_ROWS: u16 = 10;

const BAR: &str = "━";
const FREE: &str = "─";
const MARKER: &str = "╎";
const SPARKS: [&str; 4] = ["⡀", "⠄", "⠂", "⠁"];

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
    let mut lines = match panel {
        Panel::Note(note) => vec![header("Context", String::new(), width), muted_line(note)],
        Panel::Usage { summary, window } => usage_lines(*summary, *window, width, height),
    };
    lines.truncate(height as usize);
    lines
}

fn usage_lines(summary: Option<&TimelineSummary>, window: Option<u64>, width: usize, height: u16) -> Vec<Line<'static>> {
    let Some(context) = summary.and_then(|s| s.context.as_ref()).filter(|c| c.input > 0) else {
        return vec![header("Context", String::new(), width), muted_line("waiting for the first request")];
    };
    let p = pal();
    let input = context.input;
    let window = context.capacity.or(window).filter(|n| *n > 0);
    let compact = context.auto_compact_at.filter(|n| *n > 0);
    let scale = window.or(compact).unwrap_or(input).max(input);
    let right = match (window, compact) {
        (Some(_), _) => format!("{}/{} {}%", tokens(input), tokens(scale), percent(input, scale)),
        (None, Some(_)) => format!("{}/~{}", tokens(input), tokens(scale)),
        (None, None) => format!("{}/?", tokens(input)),
    };
    let mut lines = vec![header("Context", right, width)];
    let cached = context.cached.unwrap_or(0).min(input);
    let write = context.cache_write.unwrap_or(0).min(input - cached);
    let fresh = input - cached - write;
    let parts = if context.cached.is_some() {
        vec![(cached, p.done), (write, p.waiting), (fresh, p.running), (scale - input, p.faint)]
    } else {
        vec![(input, p.running), (scale - input, p.faint)]
    };
    // Round against the whole window. A few fresh tokens must not look like
    // several percent of the window just to force a colored cell to appear.
    let widths = segment_widths(&parts.iter().map(|(n, _)| *n).collect::<Vec<_>>(), width);
    let mut cells = Vec::with_capacity(width);
    for (index, ((_, color), count)) in parts.iter().zip(widths).enumerate() {
        cells.extend(std::iter::repeat_n((if index + 1 == parts.len() { FREE } else { BAR }, *color), count));
    }
    if let Some(at) = compact.filter(|k| window.is_some() && *k < scale) {
        let col = ((at as f64 / scale as f64) * width as f64).round() as usize;
        if let Some(cell) = cells.get_mut(col.min(width.saturating_sub(1))) {
            *cell = (MARKER, p.muted);
        }
    }
    lines.push(cells_line(&cells));
    let mut items = if context.cached.is_some() {
        vec![
            item("", p.done, format!("cached {}", tokens(cached)), 0),
            item("", p.running, format!("fresh {}", tokens(fresh)), 1),
        ]
    } else {
        vec![item("", p.running, format!("input {}", tokens(input)), 0)]
    };
    if write > 0 {
        items.push(item("", p.waiting, format!("write {}", tokens(write)), 2));
    }
    lines.extend(legend(items, width, 1));
    if height >= GROWTH_HEIGHT {
        // Keep the trace small even on a wide sidebar. Dots show the shape
        // without turning a flat history into another solid rectangle.
        let count = context.history.len();
        let suffix = format!("{count} req");
        let trace_width = width.saturating_sub(7 + suffix.width() + 2).min(16);
        let trace = sparkline(&context.history, trace_width);
        lines.push(Line::from(vec![
            Span::styled("Growth ", Style::default().fg(p.muted)),
            Span::styled(trace.clone(), Style::default().fg(p.accent)),
            Span::raw(" ".repeat(width.saturating_sub(7 + trace.width() + suffix.width()))),
            Span::styled(suffix, Style::default().fg(p.muted)),
        ]));
        if height >= PANEL_HEIGHT {
            lines.push(match context.base {
                Some(base) => muted_line(&format!("base ~{}  chat {}", tokens(base.min(input)), tokens(input.saturating_sub(base)))),
                None => muted_line("base not available yet"),
            });
        }
        if let Some(summary) = summary {
            lines.extend(time_lines(summary, width, if height >= PANEL_HEIGHT { 2 } else { 1 }));
        }
    }
    lines
}

/// Timing stays a quiet text summary. A full bar mostly showing idle time
/// competes with the context meter without helping read it.
fn time_lines(s: &TimelineSummary, width: usize, rows: usize) -> Vec<Line<'static>> {
    let p = pal();
    let tools = s.tool_ms.saturating_add(s.install_ms).saturating_add(s.build_ms).saturating_add(s.test_ms);
    let total = s.model_ms.saturating_add(tools).saturating_add(s.wait_ms);
    let share = |ms| {
        let pct = percent(ms, total);
        if pct == 0 && ms > 0 { "<1%".to_string() } else { format!("{pct}%") }
    };
    let items = [("model", s.model_ms, 0), ("tools", tools, 2), ("wait", s.wait_ms, 1)]
        .into_iter()
        .filter(|(_, ms, _)| *ms > 0)
        .map(|(name, ms, rank)| item("", p.muted, format!("{name} {}", share(ms)), rank))
        .collect();
    legend(items, width, rows)
}

/// Cells per part, summing to exactly `width` whenever any part is nonzero:
/// largest remainders round. Sub-cell values stay in the legend rather than
/// exaggerating tiny shares to a whole cell.
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

fn sparkline(values: &[u64], width: usize) -> String {
    let count = values.len().min(width);
    let peak = values.iter().copied().max().unwrap_or(1).max(1);
    (0..count)
        .map(|i| {
            let at = if count <= 1 { values.len() - 1 } else { i * (values.len() - 1) / (count - 1) };
            let level = (u128::from(values[at]) * 4).div_ceil(u128::from(peak)) as usize;
            SPARKS[level.clamp(1, 4) - 1]
        })
        .collect()
}

/// Separate label and value with whitespace, not a heavy horizontal rule.
fn header(label: &str, right: String, width: usize) -> Line<'static> {
    let p = pal();
    let gap = width.saturating_sub(label.width() + right.width());
    Line::from(vec![
        Span::styled(label.to_string(), Style::default().fg(p.muted)),
        Span::raw(" ".repeat(gap)),
        Span::styled(right, Style::default().fg(term_fg())),
    ])
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
        if let Some((label, value)) = item.text.rsplit_once(' ') {
            row.push(Span::styled(format!("{label} "), Style::default().fg(pal().muted)));
            row.push(Span::styled(value.to_string(), Style::default().fg(term_fg())));
        } else {
            row.push(Span::styled(item.text.clone(), Style::default().fg(term_fg())));
        }
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
    use crate::timeline::ContextUsage;

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
        for (height, expected) in [(11, None), (12, None), (13, Some(3)), (14, Some(3)), (15, Some(5)), (20, Some(5)), (21, Some(7)), (39, Some(7))] {
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
        assert_eq!(compact.len(), 3);
        assert_eq!(compact[1].width(), 32);
        let shown = text(&compact).join("\n");
        for want in ["Context", "90k/?", "cached 60k", "fresh 30k"] {
            assert!(shown.contains(want), "{want} missing:\n{shown}");
        }
        assert!(!shown.contains('%'), "unknown capacity has no fill percentage");
        let growth = lines_for_height(&panel, 32, GROWTH_HEIGHT);
        assert_eq!(growth.len(), 5);
        let shown = text(&growth).join("\n");
        assert!(shown.contains("Growth"));
        assert!(shown.contains("3 req"));
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
                if *v == 0 { assert_eq!(*n, 0); }
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
        let lines = usage_lines(Some(&summary), None, 32, PANEL_HEIGHT);
        assert_eq!(lines.len(), PANEL_HEIGHT as usize);
        for row in [1] {
            assert_eq!(lines[row].width(), 32, "bar row {row}");
        }
        let shown = text(&lines).join("\n");
        for want in ["142k/200k 71%", "cached 98k", "write 14k", "fresh 30k", "base ~38k", "chat 104k", "3 req", "model 60%", "wait 10%"] {
            assert!(shown.contains(want), "{want} missing:\n{shown}");
        }
    }
    #[test]
    fn thin_panel_fits_narrow_sidebars_and_does_not_inflate_fresh_tokens() {
        let summary = TimelineSummary {
            context: Some(ContextUsage {
                input: 144_000, cached: Some(143_717), capacity: Some(475_000),
                base: Some(34_000), history: vec![34_000, 80_000, 120_000, 144_000],
                ..Default::default()
            }),
            model_ms: 11_000, tool_ms: 1, wait_ms: 89_000,
            ..Default::default()
        };
        let panel = Panel::Usage { summary: Some(&summary), window: None };
        for width in [22, 24, 30, 40, 60] {
            for height in [COMPACT_HEIGHT, GROWTH_HEIGHT, PANEL_HEIGHT] {
                let lines = lines_for_height(&panel, width, height);
                let shown = text(&lines).join("\n");
                assert!(lines.len() <= height as usize);
                assert!(lines.iter().all(|line| line.width() <= width), "width {width}:\n{shown}");
                assert!(!shown.contains('█') && !shown.contains('░'), "no solid slabs: {shown}");
                assert!(shown.contains("fresh 283"), "small shares stay legible: {shown}");
                assert!(!lines[1].spans.iter().any(|span| span.style.fg == Some(pal().running)), "283 tokens must not get a whole meter cell");
            }
        }
        assert_eq!(segment_widths(&[143_717, 283, 331_000], 32), vec![10, 0, 22]);
        assert_eq!(sparkline(&[], 16), "");
        assert_eq!(sparkline(&[10, 20], 0), "");
        assert_eq!(sparkline(&[10; 60], 16).width(), 16);
    }

}

#![allow(unused_imports)]
//! ratatui rendering: panes, prompts, layout, styles, and scroll math.
use super::*;
use crate::plan_stream::{PlanEntry, PlanEntryKind};

/// Below this many columns the sidebar and the worker stop sharing the screen.
pub(crate) const NARROW_LAYOUT_WIDTH: u16 = 90;

pub(crate) fn render(frame: &mut Frame<'_>, app: &mut App) {
    render_at(frame, app, Instant::now());
}

/// `render` at a given moment. Only the full-screen move reads it; tests pass
/// a pinned clock to draw any frame of that move.
pub(crate) fn render_at(frame: &mut Frame<'_>, app: &mut App, now: Instant) {
    refresh_palette();
    let area = frame.area();
    frame.render_widget(Clear, area);
    // In terminal-background mode this leaves the terminal foreground/background
    // untouched; in paper mode it paints the previous white canvas.
    frame.render_widget(Block::default().style(app_style()), area);

    // GOD'S VIEW (⌥g): every live agent tiled over the whole screen. The panes
    // underneath are not drawn, so none of them keeps a hit-test rect.
    if app.gods_view {
        app.agents_area = None;
        app.worker_area = None;
        app.diff_area = None;
        app.task_area = None;
        render_gods_view(frame, area, app);
        render_cloud_prompt(frame, area, app);
        render_merge_prompt(frame, area, app);
        return;
    }

    let task_height = task_pane_height(app, area.width);

    // ⌥h MID-MOVE: the panes are on their way into or out of full screen.
    if let Some((pane, fullness)) = solo_transition_at(app, now) {
        render_solo_transition(frame, area, task_height, app, pane, fullness);
        return;
    }

    // SOLO: the worker (or the open diff panel) takes the whole screen and the
    // others are not drawn.
    if app.solo_pane {
        // FULL SCREEN: no sidebar, no task line, no gutters. The agent list and the
        // task line are never shown alone; toggle_solo_pane moves focus off them
        // so keystrokes still land somewhere you can see, and ⌥h restores both the
        // split and the focus you had.
        let solo = app.solo_shown_pane();
        // Areas the hit-testing and scroll code read: a hidden pane gets NONE, so a
        // stale rect cannot take a click meant for the pane that is actually drawn.
        app.agents_area = None;
        app.worker_area = (solo == FocusPane::Worker).then_some(area);
        app.diff_area = (solo == FocusPane::Diff).then_some(area);
        app.task_area = None;
        if solo == FocusPane::Diff {
            render_diff_panel(frame, area, app);
        } else {
            render_worker(frame, area, app);
        }
        render_cloud_prompt(frame, area, app);
        render_merge_prompt(frame, area, app);
        render_notice_pill(frame, area, app);
        render_keys_overlay(frame, area, app);
        render_mouse_debug(frame, area, app);
        return;
    }

    let layout = split_layout(app, area, task_height);
    app.task_area = Some(layout.task);
    app.agents_area = layout.agents;
    app.worker_area = layout.worker;
    app.diff_area = layout.diff;

    render_header(frame, layout.header, app);
    if let Some(rect) = layout.agents {
        render_agents(frame, rect, app);
    }
    if let Some(rect) = layout.agents_gutter {
        render_gutter(frame, rect, Gutter::Vertical);
    }
    if let Some(rect) = layout.worker {
        render_worker(frame, rect, app);
    }
    if let Some(rect) = layout.diff_gutter {
        render_gutter(frame, rect, Gutter::Vertical);
    }
    if let Some(rect) = layout.diff {
        render_diff_panel(frame, rect, app);
    }
    render_footer(frame, layout.footer, app);
    render_notice_pill(frame, layout.content, app);
    render_task(frame, layout.task, app);
    render_suggestions(frame, layout.task, app);
    render_cloud_prompt(frame, area, app);
    render_merge_prompt(frame, area, app);
    render_keys_overlay(frame, area, app);
    render_mouse_debug(frame, area, app);
    render_perf_hud(frame, area, app);
}

/// Where each part of the split dashboard goes. A pane that is not drawn is
/// None. Pure, so the full-screen move can lay out the split it moves from or
/// to without drawing it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct SplitLayout {
    pub(crate) header: Rect,
    /// The band between the header and the task line: panes go here.
    pub(crate) content: Rect,
    pub(crate) task: Rect,
    pub(crate) footer: Rect,
    pub(crate) agents: Option<Rect>,
    pub(crate) agents_gutter: Option<Rect>,
    pub(crate) worker: Option<Rect>,
    pub(crate) diff_gutter: Option<Rect>,
    pub(crate) diff: Option<Rect>,
}

pub(crate) fn split_layout(app: &App, area: Rect, task_height: u16) -> SplitLayout {
    // Header (wordmark, project, branch) and footer (the keys that matter right
    // now) are one quiet row each, and give way on a very short screen.
    let chrome_rows = u16::from(area.height >= 14);
    let rows = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(chrome_rows),
            Constraint::Min(6),
            Constraint::Length(task_height),
            Constraint::Length(chrome_rows),
        ])
        .split(area);
    let mut layout = SplitLayout {
        header: rows[0],
        content: rows[1],
        task: rows[2],
        footer: rows[3],
        agents: None,
        agents_gutter: None,
        worker: None,
        diff_gutter: None,
        diff: None,
    };

    // Narrow (zoomed in, a split, a small window): a 34-column sidebar next to
    // the worker leaves neither readable, so one of them takes the width. The
    // worker normally; the agent list while it has focus (Option-1 / ^W 1).
    // The same rule the diff panel follows below.
    if area.width < NARROW_LAYOUT_WIDTH {
        if app.focus == FocusPane::Agents {
            layout.agents = Some(layout.content);
        } else if app.diff_panel.open && app.focus == FocusPane::Diff {
            layout.diff = Some(layout.content);
        } else {
            layout.worker = Some(layout.content);
        }
        return layout;
    }

    let main = Layout::default()
        .direction(Direction::Horizontal)
        .constraints([
            Constraint::Length(34),
            Constraint::Length(1),
            Constraint::Min(42),
        ])
        .split(layout.content);
    layout.agents = Some(main[0]);
    layout.agents_gutter = Some(main[1]);
    if app.diff_panel.open {
        // The diff panel sits beside the worker when there is room for both;
        // on a narrow screen it takes the worker's place (⌥d brings the
        // conversation back), never a squeezed pair that nobody can read.
        if main[2].width >= 100 {
            let split = Layout::default()
                .direction(Direction::Horizontal)
                .constraints([
                    Constraint::Percentage(45),
                    Constraint::Length(1),
                    Constraint::Percentage(55),
                ])
                .split(main[2]);
            layout.worker = Some(split[0]);
            layout.diff_gutter = Some(split[1]);
            layout.diff = Some(split[2]);
        } else {
            layout.diff = Some(main[2]);
        }
    } else {
        layout.worker = Some(main[2]);
    }
    layout
}

/// One frame of the ⌥h move. The split is drawn as it stands, minus the moving
/// pane; the moving pane is drawn on top at a rect between its slot in the
/// split and the whole screen. Going full screen it grows over the agent list,
/// the task line and the header and footer rows; coming back it shrinks and
/// uncovers them.
///
/// The moving pane lays its content out for its slot (`transition_slot`), the
/// smaller of its two sizes, so the text inside never rewraps and the agent's
/// PTY is not resized frame by frame: it takes its new size once.
fn render_solo_transition(
    frame: &mut Frame<'_>,
    area: Rect,
    task_height: u16,
    app: &mut App,
    pane: FocusPane,
    fullness: f32,
) {
    let leaving = !app.solo_pane;
    let layout = split_layout(app, area, task_height);
    let slot = match pane {
        FocusPane::Diff => layout.diff.or(layout.worker),
        _ => layout.worker.or(layout.diff),
    }
    .unwrap_or(layout.content);
    let moving = lerp_rect(slot, area, fullness);

    // The worker's title repeats the task when the agent list is not beside it;
    // it reads as it will once the move lands.
    app.agents_area = if leaving { layout.agents } else { None };
    render_header(frame, layout.header, app);
    if let Some(rect) = layout.agents {
        render_agents(frame, rect, app);
    }
    if let Some(rect) = layout.agents_gutter {
        render_gutter(frame, rect, Gutter::Vertical);
    }
    if let Some(rect) = layout.worker.filter(|_| pane != FocusPane::Worker) {
        render_worker(frame, rect, app);
    }
    if let Some(rect) = layout.diff_gutter {
        render_gutter(frame, rect, Gutter::Vertical);
    }
    if let Some(rect) = layout.diff.filter(|_| pane != FocusPane::Diff) {
        render_diff_panel(frame, rect, app);
    }
    render_footer(frame, layout.footer, app);
    // The prompt's cursor shows only once the moving pane is off the prompt.
    render_task_with_cursor(frame, layout.task, app, !moving.intersects(layout.task));
    render_suggestions(frame, layout.task, app);

    frame.render_widget(Clear, moving);
    frame.render_widget(Block::default().style(app_style()), moving);
    app.transition_slot = Some(slot);
    if pane == FocusPane::Diff {
        render_diff_panel(frame, moving, app);
    } else {
        render_worker(frame, moving, app);
    }
    app.transition_slot = None;

    // Only the moving pane takes clicks while it moves: the panes under it are
    // partly covered, and a rect for one of them would take clicks meant for it.
    app.agents_area = None;
    app.task_area = None;
    app.worker_area = (pane != FocusPane::Diff).then_some(moving);
    app.diff_area = (pane == FocusPane::Diff).then_some(moving);

    // The notice rides between where each resting layout puts it.
    render_notice_pill(frame, lerp_rect(layout.content, area, fullness), app);
    render_cloud_prompt(frame, area, app);
    render_merge_prompt(frame, area, app);
    render_keys_overlay(frame, area, app);
    render_mouse_debug(frame, area, app);
    if leaving {
        render_perf_hud(frame, area, app);
    }
}

/// The rect `amount` of the way from `from` to `to`, each edge rounded to a
/// whole cell: 0 is `from`, 1 is `to`.
pub(crate) fn lerp_rect(from: Rect, to: Rect, amount: f32) -> Rect {
    let amount = amount.clamp(0.0, 1.0);
    let mix = |a: u16, b: u16| {
        (f32::from(a) + (f32::from(b) - f32::from(a)) * amount).round() as u16
    };
    let left = mix(from.left(), to.left());
    let top = mix(from.top(), to.top());
    let right = mix(from.right(), to.right()).max(left);
    let bottom = mix(from.bottom(), to.bottom()).max(top);
    Rect::new(left, top, right - left, bottom - top)
}

/// `/Users/me/code/x` as `~/code/x`: the sidebar is narrow and the home
/// prefix is the part nobody needs to read.
pub(crate) fn home_relative(path: &Path) -> String {
    match std::env::var_os("HOME").map(PathBuf::from) {
        Some(home) if !home.as_os_str().is_empty() => match path.strip_prefix(&home) {
            Ok(rest) if rest.as_os_str().is_empty() => "~".to_string(),
            Ok(rest) => format!("~/{}", rest.display()),
            Err(_) => path.display().to_string(),
        },
        _ => path.display().to_string(),
    }
}

/// Split one sidebar row into rows no wider than `width`, breaking at a space
/// when there is one. Continuations are indented two past the row's own
/// leading space, so a wrapped title or status reads as part of its row.
pub(crate) fn wrap_sidebar_line(line: Line<'_>, width: usize) -> Vec<Line<'_>> {
    use unicode_width::UnicodeWidthStr;
    if width < 8 || line.width() <= width {
        return vec![line];
    }
    let leading = line
        .spans
        .iter()
        .flat_map(|span| span.content.chars())
        .take_while(|ch| *ch == ' ')
        .count();
    let indent = (leading + 2).min(width / 3);
    let line_style = line.style;

    // (grapheme, style) cells, so a break never splits a styled run's glyph.
    let mut cells: Vec<(String, Style)> = Vec::new();
    for span in &line.spans {
        for ch in span.content.chars() {
            cells.push((ch.to_string(), span.style));
        }
    }

    let mut rows: Vec<Vec<(String, Style)>> = Vec::new();
    let mut start = 0;
    let mut first = true;
    while start < cells.len() {
        let room = if first { width } else { width - indent };
        let mut used = 0;
        let mut end = start;
        while end < cells.len() {
            let w = cells[end].0.width();
            if used + w > room {
                break;
            }
            used += w;
            end += 1;
        }
        if end < cells.len() {
            // Back up to the last space in this row, unless that leaves it nearly
            // empty (a path or a long word just breaks where it must).
            if let Some(space) = (start..end).rev().find(|&i| cells[i].0 == " ") {
                if space > start + room / 3 {
                    end = space + 1;
                }
            }
        }
        if end == start {
            end = start + 1;
        }
        let mut row: Vec<(String, Style)> = Vec::new();
        if !first {
            row.push((" ".repeat(indent), Style::default()));
        }
        let mut piece: Vec<(String, Style)> = cells[start..end].to_vec();
        if !first {
            while piece.first().is_some_and(|cell| cell.0 == " ") {
                piece.remove(0);
            }
        }
        row.extend(piece);
        rows.push(row);
        start = end;
        first = false;
    }

    rows.into_iter()
        .map(|row| {
            let mut spans: Vec<Span<'static>> = Vec::new();
            for (text, style) in row {
                match spans.last_mut() {
                    Some(last) if last.style == style => last.content.to_mut().push_str(&text),
                    _ => spans.push(Span::styled(text, style)),
                }
            }
            Line::from(spans).style(line_style)
        })
        .collect()
}

#[derive(Clone, Copy)]
pub(crate) enum Gutter {
    Horizontal,
    Vertical,
}

pub(crate) fn render_gutter(frame: &mut Frame<'_>, area: Rect, gutter: Gutter) {
    let style = muted_style(false);
    let line = match gutter {
        Gutter::Horizontal => " ".repeat(area.width as usize),
        Gutter::Vertical => " ".to_string(),
    };

    let lines = vec![Line::from(Span::styled(line, style)); area.height as usize];
    frame.render_widget(Paragraph::new(lines).style(app_style()), area);
}

pub(crate) fn render_mouse_debug(frame: &mut Frame<'_>, area: Rect, app: &App) {
    if !app.mouse_debug {
        return;
    }
    let text = app
        .mouse_debug_last
        .as_deref()
        .unwrap_or("waiting for mouse event");
    let width = area.width.saturating_sub(4).min(120).max(20);
    let height = 3_u16.min(area.height);
    let x = area.right().saturating_sub(width + 2);
    let y = area.bottom().saturating_sub(height + 1);
    let rect = Rect {
        x,
        y,
        width,
        height,
    };
    frame.render_widget(Clear, rect);
    frame.render_widget(
        Paragraph::new(Line::from(Span::styled(
            text.to_string(),
            Style::default().fg(ACCENT_2),
        )))
        .style(app_style())
        .block(
            Block::default()
                .borders(Borders::ALL)
                .title("mouse debug")
                .style(app_style()),
        )
        .style(app_style())
        .wrap(Wrap { trim: false }),
        rect,
    );
}

pub(crate) fn render_perf_hud(frame: &mut Frame<'_>, area: Rect, app: &mut App) {
    let agent_count = app.agents.len();
    let Some(text) = app.perf_stats.hud_line(agent_count) else {
        return;
    };
    let width = area.width.saturating_sub(4).min(120).max(20);
    let height = 3_u16.min(area.height);
    let x = area.right().saturating_sub(width + 2);
    let y = 1_u16.min(area.bottom().saturating_sub(height));
    let rect = Rect {
        x,
        y,
        width,
        height,
    };
    frame.render_widget(Clear, rect);
    frame.render_widget(
        Paragraph::new(Line::from(Span::styled(
            text,
            Style::default().fg(ACCENT_2),
        )))
        .style(app_style())
        .block(
            Block::default()
                .borders(Borders::ALL)
                .title("perf")
                .style(app_style()),
        )
        .style(app_style())
        .wrap(Wrap { trim: false }),
        rect,
    );
}

// Row spans recorded while the agents pane renders: (first_row, end_row, agent index).
// The TUI renders on one thread, so a thread-local recorder lets the deeply nested
// row-push walks tag their rows without threading a collector through every helper.
// render_agents clears it, the push helpers append, and render_agents harvests it
// into `app.agent_row_map`, which is what mouse clicks resolve against. This replaced
// a hardcoded "agents start at row 12" offset that silently broke whenever the pane
// header grew and never accounted for section headers or blank separator rows.
thread_local! {
    static AGENT_ROW_SPANS: std::cell::RefCell<Vec<(usize, usize, usize)>> =
        const { std::cell::RefCell::new(Vec::new()) };
}

fn record_agent_rows(start: usize, end: usize, agent_index: usize) {
    AGENT_ROW_SPANS.with(|spans| spans.borrow_mut().push((start, end, agent_index)));
}

// Same idea for the collapsed drawer headers: they are sidebar rows that resolve to a
// bucket rather than to an agent, so a click on "done 27" opens that drawer.
thread_local! {
    static DRAWER_ROW_SPANS: std::cell::RefCell<Vec<(usize, Bucket)>> =
        const { std::cell::RefCell::new(Vec::new()) };
}

fn record_drawer_row(row: usize, bucket: Bucket) {
    DRAWER_ROW_SPANS.with(|spans| spans.borrow_mut().push((row, bucket)));
}

/// Render one agent into the list: a task-label line, a status badge line, and an
/// optional diff line. `prefix` is prepended to the task-label line and
/// `cont_prefix` to the status/diff continuation lines. Both are empty in flat
/// mode (byte-for-byte unchanged output); nest mode passes jj-log connector glyphs.
#[allow(clippy::too_many_arguments)]
pub(crate) fn push_agent_row<'a>(
    lines: &mut Vec<Line<'a>>,
    app: &App,
    index: usize,
    agent: &AgentRun,
    diff: Option<String>,
    focused: bool,
    task_width: usize,
    prefix: &[Span<'a>],
    cont_prefix: &[Span<'a>],
) {
    push_agent_row_with_trailing(
        lines,
        app,
        index,
        agent,
        diff,
        focused,
        task_width,
        prefix,
        cont_prefix,
        &[],
    );
}

/// Like `push_agent_row` but appends `trailing` spans to the task-label line (used
/// for the faint "^dep" cross-section dependency hint).
#[allow(clippy::too_many_arguments)]
pub(crate) fn push_agent_row_with_trailing<'a>(
    lines: &mut Vec<Line<'a>>,
    app: &App,
    index: usize,
    agent: &AgentRun,
    diff: Option<String>,
    focused: bool,
    task_width: usize,
    prefix: &[Span<'a>],
    cont_prefix: &[Span<'a>],
    trailing: &[Span<'a>],
) {
    let row_start = lines.len();
    // Which card this is in the frame, for the staggered startup reveal.
    let ordinal = AGENT_ROW_SPANS.with(|spans| spans.borrow().len());
    let selected = index == app.selected_agent
        && (app.drawer_cursor.is_none() || app.drawer_open() == app.drawer_cursor);
    let p = pal();
    // The rename box comes first: a main row used to take its summary
    // before this check, so `r` opened an editor nobody could see.
    let task_label = if index == app.selected_agent && app.rename_input.is_some() {
        let buf = app.rename_input.clone().unwrap_or_default();
        format!("✎ {buf}")
    } else if agent.is_main() {
        agent.task_summary.clone()
    } else if agent.task_summary.trim().is_empty() {
        summarize_task(&agent.task)
    } else {
        agent.task_summary.clone()
    };
    // The reviewer half is not its own piece of work. bucket_members guarantees
    // it sits directly under its generator, and the hook says so: two rows for
    // one pair otherwise read as two unrelated agents that happen to be next to
    // each other.
    let task_label = if agent.is_gam_adversary() {
        format!("↳ {task_label}")
    } else {
        task_label
    };
    let task_label = if is_cloud_agent(agent) {
        format!("☁ {task_label}")
    } else {
        task_label
    };

    // Line 2: status glyph, status, elapsed, diff, and a tag for anything that
    // is not a plain run (main, plan, cloud, a plan node id).
    let (status_label, status_style): (String, Style) =
        if agent.is_main() && agent.terminal.is_none() {
            ("idle".to_string(), muted_style(focused))
        } else if app.launch_preparing(&agent.id) {
            ("preparing workspace".to_string(), agent_status_style(agent))
        } else {
            (agent_status_text(agent), agent_status_style(agent))
        };
    // Another terminal is mirroring this pane (`rudder attach`): say so, since
    // keys typed there land in the same agent and its pane is sized to that
    // terminal rather than to this layout.
    let status_label = if app.run_is_attached(&agent.id) {
        format!("{status_label} \u{b7} attached")
    } else {
        status_label
    };
    let status_color = status_style.fg.unwrap_or(p.muted);
    let glyph = if agent_is_busy(agent) {
        app.spinner_glyph()
    } else {
        match agent.status {
            _ if agent_waits_for_you(agent) => BADGE,
            AgentStatus::Done | AgentStatus::Merged => "\u{2713}",
            AgentStatus::Failed | AgentStatus::Orphaned => "\u{2715}",
            AgentStatus::Stopped | AgentStatus::Paused => "\u{25cb}",
            _ => BADGE,
        }
    };
    let dot = Span::styled(" \u{b7} ", Style::default().fg(p.faint));
    let mut status = vec![
        Span::styled(glyph, Style::default().fg(status_color)),
        Span::raw(" "),
    ];
    if agent_is_busy(agent) {
        // Working: a light band sweeps across the status, frame by frame.
        status.extend(shimmer_spans(&status_label, status_color, app.spinner_frame));
    } else {
        status.push(Span::styled(status_label, status_style));
    }
    if agent.status == AgentStatus::Running {
        if let Some(elapsed) = run_elapsed_label(agent) {
            status.push(dot.clone());
            status.push(Span::styled(elapsed, muted_style(focused)));
        }
    }
    if let Some(summary) = diff.as_deref() {
        let diff_spans = diff_stat_spans(summary, focused);
        if !diff_spans.is_empty() {
            status.push(dot.clone());
            status.extend(diff_spans);
        }
    }
    let tag = if agent.is_main() {
        Some("main".to_string())
    } else if is_cloud_agent(agent) {
        Some("cloud".to_string())
    } else if agent.mode == AgentMode::RudderPlan {
        Some("rudder-plan".to_string())
    } else if agent.mode == AgentMode::ReviewAll {
        Some("review-all".to_string())
    } else if agent.mode == AgentMode::Plan {
        Some("plan".to_string())
    } else {
        agent.node_id.clone()
    };
    if let Some(tag) = tag {
        status.push(dot.clone());
        status.push(Span::styled(tag, Style::default().fg(p.accent)));
    }

    let mut extra: Vec<Vec<Span<'a>>> = Vec::new();
    if selected {
        let summary = app.diff_panel.timeline.as_ref()
            .filter(|(id, _)| *id == agent.id)
            .map(|(_, summary)| summary.clone()).unwrap_or_default();
        let context = if is_cloud_agent(agent) || agent.backend == Backend::Opencode {
            vec!["Context: backend usage unavailable here".into()]
        } else {
            crate::timeline::context_lines(&summary)
        };
        for line in context {
            extra.push(vec![Span::styled(line, muted_style(focused))]);
        }
    }

    if let Some(detail) = integration_detail(agent) {
        extra.push(vec![Span::styled(detail, muted_style(focused))]);
    }
    if let Some(detail) = delivery_detail(agent) {
        extra.push(vec![Span::styled(detail, muted_style(focused))]);
    }

    let inner = task_width + 2;
    let prefix_w = spans_width(prefix);
    if selected {
        // The selected card: a rounded accent outline around title and status.
        let box_w = inner.saturating_sub(prefix_w).max(8);
        let body_w = box_w.saturating_sub(4);
        let border = Style::default().fg(p.accent);
        let edge = |left: &'static str, right: &'static str| -> Vec<Span<'a>> {
            let mut row = cont_prefix.to_vec();
            row.push(Span::styled(
                format!("{left}{}{right}", "─".repeat(box_w.saturating_sub(2))),
                border,
            ));
            row
        };
        let framed = |pre: &[Span<'a>], body: Vec<Span<'a>>| -> Line<'a> {
            let mut row = pre.to_vec();
            row.push(Span::styled("│ ", border));
            row.extend(fit_spans(body, body_w));
            row.push(Span::styled(" │", border));
            Line::from(row)
        };
        lines.push(Line::from(edge("╭", "╮")));
        let title_style = Style::default().fg(p.accent).add_modifier(Modifier::BOLD);
        let title_rows = wrap_title(&task_label, body_w, 2);
        let last = title_rows.len().saturating_sub(1);
        for (row_index, row) in title_rows.into_iter().enumerate() {
            let mut title = vec![Span::styled(row, title_style)];
            if row_index == last {
                title.extend(trailing.iter().cloned());
            }
            lines.push(framed(if row_index == 0 { prefix } else { cont_prefix }, title));
        }
        for row in wrap_spans(status, body_w, 2) {
            lines.push(framed(cont_prefix, row));
        }
        for row in extra {
            lines.push(framed(cont_prefix, row));
        }
        lines.push(Line::from(edge("╰", "╯")));
        apply_card_motion(&mut lines[row_start..], app, &agent.id, ordinal, inner);
        record_agent_rows(row_start, lines.len(), index);
    } else {
        // Every other card: no outline, two quiet lines and a gap.
        let body_w = inner.saturating_sub(prefix_w + 2).max(4);
        let finished = matches!(
            agent.status,
            AgentStatus::Merged | AgentStatus::Stopped | AgentStatus::Paused
        );
        let title_style = if finished || !focused {
            Style::default().fg(p.muted)
        } else {
            pane_text_style(true)
        };
        // A thin bar in the card's status color runs down its left edge.
        let bar = Span::styled(
            "\u{258e} ",
            Style::default().fg(match card_kind(agent) {
                CardKind::Idle => p.faint,
                kind if finished => blend(canvas(), card_kind_color(kind), 0.55),
                kind => card_kind_color(kind),
            }),
        );
        let title_rows = wrap_title(&task_label, body_w, 2);
        let last = title_rows.len().saturating_sub(1);
        for (row_index, row) in title_rows.into_iter().enumerate() {
            let mut title = if row_index == 0 { prefix.to_vec() } else { cont_prefix.to_vec() };
            title.push(bar.clone());
            title.push(Span::styled(row, title_style));
            if row_index == last {
                title.extend(trailing.iter().cloned());
            }
            lines.push(Line::from(title));
        }
        for status_row in wrap_spans(status, body_w, 2) {
            let mut row = cont_prefix.to_vec();
            row.push(bar.clone());
            row.extend(status_row);
            lines.push(Line::from(row));
        }
        for detail in extra {
            let mut row = cont_prefix.to_vec();
            row.push(bar.clone());
            row.extend(detail);
            lines.push(Line::from(row));
        }
        apply_card_motion(&mut lines[row_start..], app, &agent.id, ordinal, inner);
        record_agent_rows(row_start, lines.len(), index);
        lines.push(Line::default());
    }
}

/// How long a running agent has been going, from its creation stamp. Under a
/// minute says nothing: the spinner already shows it just started.
fn run_elapsed_label(agent: &AgentRun) -> Option<String> {
    let created: u128 = agent.created_at.trim().parse().ok()?;
    // Not a real clock reading (old rows, fixtures): say nothing.
    if created < 1_500_000_000_000 {
        return None;
    }
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .ok()?
        .as_millis();
    let secs = (now.checked_sub(created)? / 1000) as u64;
    if secs < 60 {
        return None;
    }
    let minutes = secs / 60;
    Some(if minutes < 60 {
        format!("{minutes}m")
    } else if minutes < 48 * 60 {
        format!("{}h", minutes / 60)
    } else {
        format!("{}d", minutes / (60 * 24))
    })
}

/// `+120 -8` from a `--shortstat` summary, additions green and deletions red.
fn diff_stat_spans(summary: &str, focused: bool) -> Vec<Span<'static>> {
    let p = pal();
    let mut ins = None;
    let mut del = None;
    for part in summary.split(',') {
        let part = part.trim();
        let Some(num) = part.split_whitespace().next() else {
            continue;
        };
        if part.contains("insertion") {
            ins = Some(num.to_string());
        } else if part.contains("deletion") {
            del = Some(num.to_string());
        }
    }
    if ins.is_none() && del.is_none() {
        let compact = compact_diff_stat(summary);
        return if compact.trim().is_empty() {
            Vec::new()
        } else {
            vec![Span::styled(compact, muted_style(focused))]
        };
    }
    let mut spans = Vec::new();
    if let Some(ins) = ins {
        spans.push(Span::styled(format!("+{ins}"), Style::default().fg(p.done)));
    }
    if let Some(del) = del {
        if !spans.is_empty() {
            spans.push(Span::raw(" "));
        }
        spans.push(Span::styled(format!("-{del}"), Style::default().fg(p.failed)));
    }
    spans
}

/// Clip `spans` to exactly `width` columns: cut with an ellipsis when too wide,
/// pad with spaces when short.
fn fit_spans<'a>(spans: Vec<Span<'a>>, width: usize) -> Vec<Span<'a>> {
    let mut out = Vec::new();
    let mut used = 0usize;
    let total = spans_width(&spans);
    let limit = if total > width { width.saturating_sub(1) } else { width };
    for span in spans {
        let w = span.width();
        if used + w <= limit {
            used += w;
            out.push(span);
            continue;
        }
        let mut cut = String::new();
        for ch in span.content.chars() {
            let cw = unicode_width::UnicodeWidthChar::width(ch).unwrap_or(0);
            if used + cw > limit {
                break;
            }
            used += cw;
            cut.push(ch);
        }
        out.push(Span::styled(cut, span.style));
        break;
    }
    if total > width && width > 0 {
        out.push(Span::styled("\u{2026}", Style::default().fg(pal().muted)));
        used += 1;
    }
    if used < width {
        out.push(Span::raw(" ".repeat(width - used)));
    }
    out
}

/// A card title on at most `max_rows` rows of `width` columns, broken at
/// words, with an ellipsis when it still does not fit.
fn wrap_title(text: &str, width: usize, max_rows: usize) -> Vec<String> {
    let width = width.max(4);
    let mut rows = wrap_text(text, width as u16);
    if rows.is_empty() {
        rows.push(String::new());
    }
    if rows.len() > max_rows {
        rows.truncate(max_rows);
        let last = rows.last_mut().expect("at least one row");
        let keep = width.saturating_sub(1);
        let mut cut: String = last.chars().take(keep).collect();
        while unicode_width::UnicodeWidthStr::width(cut.as_str()) > keep {
            cut.pop();
        }
        *last = format!("{}\u{2026}", cut.trim_end());
    }
    rows
}

/// Compress `git diff --shortstat` output ("34 files changed, 3162 insertions(+),
/// 120 deletions(-)") into a pane-friendly "34f +3162 -120" so every count stays
/// visible in the narrow agents pane instead of clipping mid-word. Falls back to
/// the original string if it does not parse.
fn compact_diff_stat(summary: &str) -> String {
    let (mut files, mut ins, mut del) = (None, None, None);
    for part in summary.split(',') {
        let part = part.trim();
        let Some(num) = part.split_whitespace().next() else {
            continue;
        };
        if part.contains("file") {
            files = Some(num);
        } else if part.contains("insertion") {
            ins = Some(num);
        } else if part.contains("deletion") {
            del = Some(num);
        }
    }
    let mut out = String::new();
    if let Some(files) = files {
        out.push_str(&format!("{files}f"));
    }
    if let Some(ins) = ins {
        out.push_str(&format!(" +{ins}"));
    }
    if let Some(del) = del {
        out.push_str(&format!(" -{del}"));
    }
    let out = out.trim();
    if out.is_empty() {
        summary.to_string()
    } else {
        out.to_string()
    }
}

/// Status section for the agents pane. Order here is the rendered/navigated order.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum Bucket {
    OneOff,
    Todo,
    InProgress,
    Review,
    Cloud,
    Done,
    Closed,
}

impl Bucket {
    /// Sections in display order; main agents + the orchestrator render above all of these.
    /// One-off agents lead (they are quick, transient, and what you most recently asked for).
    /// Cloud-owned rows sit after review: that is live remote work, and burying it
    /// in the collapsed closed drawer is exactly how a migrated agent got
    /// misread as dead and deleted while it was still running in Rudder Cloud.
    const ORDER: [Bucket; 7] = [
        Bucket::OneOff,
        Bucket::Todo,
        Bucket::InProgress,
        Bucket::Review,
        Bucket::Cloud,
        Bucket::Done,
        Bucket::Closed,
    ];

    /// Short header label (the left pane is only 34 cols wide).
    pub(crate) fn label(self) -> &'static str {
        match self {
            Bucket::OneOff => "one-off",
            Bucket::Todo => "todo",
            Bucket::InProgress => "in progress",
            Bucket::Review => "review",
            Bucket::Cloud => "cloud",
            Bucket::Done => "done",
            Bucket::Closed => "closed",
        }
    }

    /// Finished work collapses to a single sidebar row instead of one row per run.
    /// A long session ends with dozens of merged and failed rows; listing them all
    /// buries the two or three that are still moving.
    ///
    /// `done` and `closed` always collapse. `review` is the awkward one: it is
    /// finished work that still needs a merge decision, so a handful of rows belongs
    /// inline where the user can act on it — but a session that ends with 25 of them
    /// (which is what a long fleet run actually produces) is not scannable at four
    /// lines a row, and the section you must act on is the one whose shape you cannot
    /// see. Past the threshold it collapses like the rest.
    pub(crate) fn always_drawer(self) -> bool {
        matches!(self, Bucket::Done | Bucket::Closed)
    }
}

/// Review stays inline up to this many rows, then collapses. Sized so a normal plan
/// (a few nodes awaiting merge) reads exactly as before.
pub(crate) const REVIEW_INLINE_LIMIT: usize = 5;

/// Is this bucket currently drawn as a collapsed drawer? Depends on the fleet, because
/// `review` collapses only once it is too long to read.
pub(crate) fn bucket_is_drawer(agents: &[AgentRun], bucket: Bucket) -> bool {
    if bucket.always_drawer() {
        return true;
    }
    bucket == Bucket::Review && bucket_members(agents, bucket).len() > REVIEW_INLINE_LIMIT
}

/// The buckets currently collapsed, in sidebar (Bucket::ORDER) order, so navigation
/// and rendering agree on which header rows exist and where.
pub(crate) fn drawer_buckets(agents: &[AgentRun]) -> Vec<Bucket> {
    Bucket::ORDER
        .into_iter()
        .filter(|&bucket| bucket_is_drawer(agents, bucket))
        .collect()
}

/// Indices of the runs a drawer holds, in sidebar order.
pub(crate) fn drawer_members(agents: &[AgentRun], bucket: Bucket) -> Vec<usize> {
    bucket_members(agents, bucket)
}

/// Is this run tucked inside a collapsed drawer (and so not its own sidebar row)?
pub(crate) fn in_drawer(agents: &[AgentRun], agent: &AgentRun) -> bool {
    !agent.is_main()
        && !agent.is_pinned_planner()
        && bucket_is_drawer(agents, status_bucket_in(agents, agent))
}

/// Map an agent to its status section.
///
/// - todo:        planned (not-yet-launched) DAG nodes (rendered from the
///                `planned_nodes` queue, not from agents).
/// - in progress: Running (incl. steering/verifying via AgentStatus::Running).
/// - review:      completed-but-not-merged (Done), plus needs-permission/-input.
/// - cloud:       Migrated — the row's work lives in Rudder Cloud now. Rudder
///                owns the record but NOT the execution: it must stay visible
///                (live remote work) and must not be locally deleted, merged,
///                resumed, or reviewed as if the workspace were still local.
/// - done:        Merged.
/// - closed:      Failed / Stopped (cancelled).
///
/// Main agents are rendered in their own leading section and are not bucketed.
/// Agents never land in Todo: that section now holds planned nodes only.
/// Bucket for a row IN THE CONTEXT of the fleet.
///
/// A `/gam` reviewer follows its generator. The two halves are one piece of
/// work, and bucketing each by its own status split them across the sidebar:
/// the generator under "in progress" while the reviewer, idle between rounds,
/// sat under "review" -- which also reads as finished work awaiting a merge it
/// can never have, since the reviewer holds no change of its own.
pub(crate) fn status_bucket_in(agents: &[AgentRun], agent: &AgentRun) -> Bucket {
    if agent.is_gam_adversary() {
        if let Some(generator) = agent
            .gam
            .as_ref()
            .and_then(|gam| agents.iter().find(|run| run.id == gam.peer_run_id))
        {
            return status_bucket(generator);
        }
    }
    status_bucket(agent)
}

pub(crate) fn status_bucket(agent: &AgentRun) -> Bucket {
    // Bucket strictly by run status. A RUNNING agent stays "in progress" even when it is
    // waiting on a permission prompt or a question: it is still an active session, not
    // finished work. The "needs permission"/"needs input" state is surfaced on the row
    // itself (agent_status_label, in amber) and via a notification, so it is discoverable
    // without being mislabeled "review". "review" means DONE: the agent finished a turn
    // and is awaiting your review/merge. (Previously needs_permission/needs_user_input
    // forced a running agent into Review, which read as "finished" and was confusing.)
    // One-off conversational agents get their own leading section regardless of run status.
    if is_cloud_owned_agent(agent) {
        return Bucket::Cloud;
    }
    if agent.is_oneoff() {
        return Bucket::OneOff;
    }
    match agent.status {
        AgentStatus::Running => Bucket::InProgress,
        AgentStatus::Done => Bucket::Review,
        AgentStatus::Merged => Bucket::Done,
        AgentStatus::Migrated => Bucket::Cloud,
        AgentStatus::Failed
        | AgentStatus::Stopped
        | AgentStatus::Paused
        | AgentStatus::Orphaned => Bucket::Closed,
    }
}

/// Build the per-section nest: parent->children adjacency restricted to edges where
/// BOTH endpoints fall in `members` (the indices in one section). Also returns,
/// for each member, whether it has a parent that lives OUTSIDE this section (so we
/// render it as a section root with a dep hint rather than a connector).
/// Map every agent's identifiers to its index, for resolving dependency edges in the
/// agents-pane nest. Both the run id AND the plan node id are inserted, because a
/// launched plan worker records its deps as plan NODE ids (`node.deps`) while
/// manual/test runs reference the run id. `node_id` is inserted after the run id so it
/// wins on the (rare) collision; deps are node ids in production.
fn agent_id_index(agents: &[AgentRun]) -> std::collections::HashMap<&str, usize> {
    let mut map: std::collections::HashMap<&str, usize> =
        std::collections::HashMap::with_capacity(agents.len() * 2);
    for (index, agent) in agents.iter().enumerate() {
        map.insert(agent.id.as_str(), index);
        if let Some(node_id) = agent.node_id.as_deref() {
            map.insert(node_id, index);
        }
    }
    map
}

fn section_nest(
    agents: &[AgentRun],
    members: &[usize],
) -> (
    std::collections::HashMap<usize, Vec<(usize, EdgeType)>>,
    std::collections::HashSet<usize>,
    std::collections::HashSet<usize>,
) {
    use std::collections::{HashMap, HashSet};
    let member_set: HashSet<usize> = members.iter().copied().collect();
    // Index by BOTH the run id and the plan node id. A launched plan worker carries its
    // deps as plan NODE ids (`node.deps`), while manual/test setups reference the run id;
    // mapping both lets a child resolve its parent either way. `node_id` is inserted last
    // so it wins if it ever collides with a different run's id (deps are node ids in
    // production). Without the node_id key, launched workers never nest (run id != node id).
    let id_to_index: HashMap<&str, usize> = agent_id_index(agents);

    let mut children: HashMap<usize, Vec<(usize, EdgeType)>> = HashMap::new();
    let mut has_in_section_parent: HashSet<usize> = HashSet::new();
    let mut has_out_of_section_parent: HashSet<usize> = HashSet::new();

    for &child_index in members {
        let agent = &agents[child_index];
        // Hard parents bind before soft so the most-binding edge wins.
        for parent_id in &agent.deps {
            if let Some(&parent_index) = id_to_index.get(parent_id.as_str()) {
                if parent_index == child_index {
                    continue;
                }
                if member_set.contains(&parent_index) {
                    children
                        .entry(parent_index)
                        .or_default()
                        .push((child_index, EdgeType::Hard));
                    has_in_section_parent.insert(child_index);
                } else {
                    has_out_of_section_parent.insert(child_index);
                }
            }
        }
        for parent_id in &agent.soft_deps {
            if agent.deps.iter().any(|hard| hard == parent_id) {
                continue;
            }
            if let Some(&parent_index) = id_to_index.get(parent_id.as_str()) {
                if parent_index == child_index {
                    continue;
                }
                if member_set.contains(&parent_index) {
                    children
                        .entry(parent_index)
                        .or_default()
                        .push((child_index, EdgeType::Soft));
                    has_in_section_parent.insert(child_index);
                } else {
                    has_out_of_section_parent.insert(child_index);
                }
            }
        }
    }

    (children, has_in_section_parent, has_out_of_section_parent)
}

/// Depth-first walk of one section's nest, appending visited indices in render
/// order (parents before children). Cycle-guarded by `visited`.
#[allow(clippy::too_many_arguments)]
fn section_walk(
    out: &mut Vec<usize>,
    children: &std::collections::HashMap<usize, Vec<(usize, EdgeType)>>,
    index: usize,
    visited: &mut std::collections::HashSet<usize>,
) {
    if !visited.insert(index) {
        return;
    }
    out.push(index);
    if let Some(kids) = children.get(&index) {
        for (child_index, _) in kids {
            section_walk(out, children, *child_index, visited);
        }
    }
}

/// The members of one section in nested render order: section roots (no in-section
/// parent) in display order, each followed by its in-section subtree.
fn section_order(agents: &[AgentRun], members: &[usize]) -> Vec<usize> {
    use std::collections::HashSet;
    let (children, has_in_section_parent, _out) = section_nest(agents, members);
    let mut visited: HashSet<usize> = HashSet::new();
    let mut out: Vec<usize> = Vec::new();
    for &index in members {
        if !has_in_section_parent.contains(&index) {
            section_walk(&mut out, &children, index, &mut visited);
        }
    }
    // Any member left unvisited (e.g. inside an in-section cycle) renders at root.
    for &index in members {
        section_walk(&mut out, &children, index, &mut visited);
    }
    out
}

/// Indices that belong to `bucket`, in display (insertion) order, excluding main
/// and pinned orchestrators (which render above all sections).
#[cfg(test)]
pub(crate) fn bucket_members_for_test(agents: &[AgentRun], bucket: Bucket) -> Vec<usize> {
    bucket_members(agents, bucket)
}

fn bucket_members(agents: &[AgentRun], bucket: Bucket) -> Vec<usize> {
    let mut members: Vec<usize> = agents
        .iter()
        .enumerate()
        .filter(|(_, agent)| {
            !agent.is_main()
                && !agent.is_pinned_planner()
                && status_bucket_in(agents, agent) == bucket
        })
        .map(|(index, _)| index)
        .collect();

    // A `/gam` reviewer is pulled to sit directly under its generator. Roster
    // order puts them together at launch, but nothing KEPT them together: a
    // deletion, a reordering, or another agent landing between them split one
    // pair into two rows with no visible relationship. The "↳" hook on the
    // reviewer row promises adjacency, so it has to be guaranteed here.
    let mut ordered: Vec<usize> = Vec::with_capacity(members.len());
    let reviewers: Vec<usize> = members
        .iter()
        .copied()
        .filter(|&index| agents[index].is_gam_adversary())
        .collect();
    members.retain(|&index| !agents[index].is_gam_adversary());
    for index in members {
        ordered.push(index);
        let id = &agents[index].id;
        for &reviewer in &reviewers {
            let follows = agents[reviewer]
                .gam
                .as_ref()
                .is_some_and(|gam| &gam.peer_run_id == id);
            if follows {
                ordered.push(reviewer);
            }
        }
    }
    // A reviewer whose generator is not in this bucket (or is gone) still has to
    // appear somewhere rather than being dropped off the list entirely.
    for reviewer in reviewers {
        if !ordered.contains(&reviewer) {
            ordered.push(reviewer);
        }
    }
    ordered
}

/// Canonical agents-pane order used by BOTH the renderer and navigation so the
/// selection marker always lands on the row j/k move to: main agents first, then
/// each status section in `Bucket::ORDER`, nested within the section.
pub(crate) fn sectioned_agent_order(agents: &[AgentRun]) -> Vec<usize> {
    // Pinned planners (the orchestrator and the plan-mode front-end) render and
    // navigate first, above main and every status section, so j/k land on them
    // where they are drawn.
    let mut order: Vec<usize> = agents
        .iter()
        .enumerate()
        .filter(|(_, agent)| agent.is_pinned_planner())
        .map(|(index, _)| index)
        .collect();
    order.extend(
        agents
            .iter()
            .enumerate()
            .filter(|(_, agent)| agent.is_main() && !agent.is_pinned_planner())
            .map(|(index, _)| index),
    );
    for bucket in Bucket::ORDER {
        let members = bucket_members(agents, bucket);
        order.extend(section_order(agents, &members));
    }
    order
}

/// Faint "^dep" badge appended to a section root whose parent lives in another
/// section (so it has no connector to draw here). Short to fit the 34-col pane.
fn dep_hint_span(focused: bool) -> Span<'static> {
    Span::styled(
        "  ^dep",
        Style::default()
            .fg(if focused { FAINT } else { FAINT })
            .add_modifier(Modifier::DIM),
    )
}

/// Recursive section nest walk that pushes rendered rows (mirrors `nest_walk` but
/// scoped to one section's adjacency, and tags out-of-section roots with a dep hint).
#[allow(clippy::too_many_arguments)]
fn section_walk_rows<'a>(
    lines: &mut Vec<Line<'a>>,
    app: &App,
    focused: bool,
    task_width: usize,
    diff_summaries: &[Option<String>],
    children: &std::collections::HashMap<usize, Vec<(usize, EdgeType)>>,
    out_of_section: &std::collections::HashSet<usize>,
    index: usize,
    edge: Option<EdgeType>,
    is_last: bool,
    lanes: &mut Vec<bool>,
    visited: &mut std::collections::HashSet<usize>,
) {
    if !visited.insert(index) {
        return;
    }
    let agent = &app.agents[index];
    let is_root = lanes.is_empty();
    let mut prefix = nest_prefix(lanes, is_last, edge);
    // A section root whose parent is in another section gets a faint dep hint after
    // its label rather than a connector glyph (the parent isn't drawn here).
    let trailing = if is_root && out_of_section.contains(&index) {
        vec![dep_hint_span(focused)]
    } else {
        Vec::new()
    };

    let own_children = children.get(&index).map(Vec::as_slice).unwrap_or(&[]);
    let has_unvisited_children = own_children
        .iter()
        .any(|(child, _)| !visited.contains(child));
    let cont_prefix = nest_cont_prefix(lanes, has_unvisited_children, is_root);

    let summary = if agent.is_main() {
        None
    } else {
        diff_summaries.get(index).and_then(|opt| opt.clone())
    };
    push_agent_row_with_trailing(
        lines,
        app,
        index,
        agent,
        summary,
        focused,
        task_width,
        &prefix,
        &cont_prefix,
        &trailing,
    );
    prefix.clear();

    let pending: Vec<(usize, EdgeType)> = own_children
        .iter()
        .copied()
        .filter(|(child, _)| !visited.contains(child))
        .collect();
    for (position, (child_index, child_edge)) in pending.iter().enumerate() {
        let child_is_last = position + 1 == pending.len();
        lanes.push(!child_is_last);
        section_walk_rows(
            lines,
            app,
            focused,
            task_width,
            diff_summaries,
            children,
            out_of_section,
            *child_index,
            Some(*child_edge),
            child_is_last,
            lanes,
            visited,
        );
        lanes.pop();
    }
}

/// Render one status section: a header line with a count, then its members nested
/// by dependency. Returns true if anything was emitted (the section was non-empty).
#[allow(clippy::too_many_arguments)]
fn render_status_section<'a>(
    lines: &mut Vec<Line<'a>>,
    app: &App,
    focused: bool,
    task_width: usize,
    diff_summaries: &[Option<String>],
    bucket: Bucket,
    leading_blank: bool,
) -> bool {
    use std::collections::HashSet;
    let members = bucket_members(&app.agents, bucket);
    if members.is_empty() {
        return false;
    }
    if leading_blank {
        lines.push((Line::default()));
    }
    if bucket_is_drawer(&app.agents, bucket) {
        // One row for the whole section. The chevron is the affordance: `›` closed,
        // `⌄` open, and the row carries the selection marker when the sidebar cursor
        // is on it, exactly like an agent row.
        let selected = app.drawer_cursor == Some(bucket);
        let open = app.drawer_open() == Some(bucket);
        let marker = if selected { "▶ " } else { "  " };
        let chevron = if open { "⌄" } else { "›" };
        record_drawer_row(lines.len(), bucket);
        lines.push((Line::from(vec![
            Span::styled(
                marker,
                if selected {
                    accent_style(focused)
                } else {
                    muted_style(focused)
                },
            ),
            Span::styled(bucket.label(), header_style(focused)),
            Span::styled(format!(" {}", members.len()), muted_style(focused)),
            Span::styled(format!("  {chevron}"), muted_style(focused)),
            Span::styled(
                if open { "  click or Esc to collapse" } else { "  click or Enter to expand" },
                muted_style(focused),
            ),
        ])));
        if !open {
            return true;
        }
    } else {
        // The cloud section gets the ☁ glyph on the header so it reads as "live
        // in Rudder Cloud", not as another local status bucket.
        let header_label = if bucket == Bucket::Cloud {
            "☁ cloud"
        } else {
            bucket.label()
        };
        lines.push((Line::from(vec![
            Span::styled(header_label, header_style(focused)),
            Span::styled(format!(" {}", members.len()), muted_style(focused)),
        ])));
    }

    let (children, has_in_section_parent, out_of_section) = section_nest(&app.agents, &members);
    let mut visited: HashSet<usize> = HashSet::new();
    let mut lanes: Vec<bool> = Vec::new();
    // Roots: members with no in-section parent, in display order.
    for &index in &members {
        if !has_in_section_parent.contains(&index) {
            section_walk_rows(
                lines,
                app,
                focused,
                task_width,
                diff_summaries,
                &children,
                &out_of_section,
                index,
                None,
                true,
                &mut lanes,
                &mut visited,
            );
        }
    }
    // Any member trapped in an in-section cycle renders at root so nothing hides.
    for &index in &members {
        if !visited.contains(&index) {
            section_walk_rows(
                lines,
                app,
                focused,
                task_width,
                diff_summaries,
                &children,
                &out_of_section,
                index,
                None,
                true,
                &mut lanes,
                &mut visited,
            );
        }
    }
    true
}

/// Push one planned (not-yet-launched) node row into the Todo section: a single
/// line carrying the Todo-colored badge and the node title. The "todo"/"planned"
/// status text is omitted on purpose — the section is already titled "todo", so
/// repeating it per row is noise. `prefix` carries the nest glyphs (empty for a
/// root); `_cont_prefix` is unused now that there is no second line to bridge.
fn push_planned_row<'a>(
    lines: &mut Vec<Line<'a>>,
    node: &PlannedNode,
    focused: bool,
    task_width: usize,
    prefix: &[Span<'a>],
    _cont_prefix: &[Span<'a>],
    blocked_reason: Option<&str>,
) {
    let label = if node.title.trim().is_empty() {
        summarize_task(&node.prompt)
    } else {
        node.title.clone()
    };
    let mut line = prefix.to_vec();
    line.extend([
        Span::raw("  "),
        Span::styled(BADGE, Style::default().fg(ST_PLANNED)),
        Span::raw(" "),
        Span::styled(
            truncate_chars(&label, task_width.saturating_sub(2) * 2),
            pane_text_style(focused),
        ),
    ]);
    if let Some(dep) = blocked_reason {
        line.push(Span::styled(
            format!("  blocked: {dep} failed"),
            Style::default().fg(ST_FAILED),
        ));
    }
    lines.push((Line::from(line)));
}

/// Walk the planned-node forest depth-first, nesting a node under a parent that is
/// also still planned. Roots are nodes whose every dep id is not itself a pending
/// planned node (so its parent has already launched or never existed).
#[allow(clippy::too_many_arguments)]
fn planned_walk<'a>(
    lines: &mut Vec<Line<'a>>,
    nodes: &[&PlannedNode],
    children: &std::collections::HashMap<usize, Vec<(usize, EdgeType)>>,
    index: usize,
    edge: Option<EdgeType>,
    is_last: bool,
    focused: bool,
    task_width: usize,
    lanes: &mut Vec<bool>,
    visited: &mut Vec<bool>,
    blocked_reasons: &std::collections::HashMap<String, String>,
) {
    if visited[index] {
        return;
    }
    visited[index] = true;

    let is_root = lanes.is_empty();
    let prefix = nest_prefix(lanes, is_last, edge);
    let own_children = children.get(&index).map(Vec::as_slice).unwrap_or(&[]);
    let has_unvisited_children = own_children.iter().any(|(child, _)| !visited[*child]);
    let cont_prefix = nest_cont_prefix(lanes, has_unvisited_children, is_root);

    push_planned_row(
        lines,
        nodes[index],
        focused,
        task_width,
        &prefix,
        &cont_prefix,
        blocked_reasons.get(&nodes[index].id).map(String::as_str),
    );

    let pending: Vec<(usize, EdgeType)> = own_children
        .iter()
        .copied()
        .filter(|(child, _)| !visited[*child])
        .collect();
    for (position, (child_index, child_edge)) in pending.iter().enumerate() {
        let child_is_last = position + 1 == pending.len();
        lanes.push(!child_is_last);
        planned_walk(
            lines,
            nodes,
            children,
            *child_index,
            Some(*child_edge),
            child_is_last,
            focused,
            task_width,
            lanes,
            visited,
            blocked_reasons,
        );
        lanes.pop();
    }
}

/// Render the Todo section from the planned-node queue. A node nests under its
/// parent when that parent is also still a pending planned node; once the parent
/// launches it leaves the queue and the child becomes a section root. Returns
/// true when anything was emitted.
fn render_planned_section<'a>(
    lines: &mut Vec<Line<'a>>,
    nodes: &[&PlannedNode],
    focused: bool,
    task_width: usize,
    leading_blank: bool,
    awaiting_approval: bool,
    blocked_reasons: &std::collections::HashMap<String, String>,
) -> bool {
    use std::collections::HashMap;
    if nodes.is_empty() {
        return false;
    }
    if leading_blank {
        lines.push((Line::default()));
    }
    lines.push((Line::from(vec![
        Span::styled(Bucket::Todo.label(), header_style(focused)),
        Span::styled(format!(" {}", nodes.len()), muted_style(focused)),
    ])));
    // APPROVAL GATE hint: while the plan awaits approval nothing has launched yet;
    // the user refines in the task pane or approves with an empty Enter.
    if awaiting_approval {
        lines.push((Line::from(Span::styled(
            "  type below to refine  ·  empty Enter approves",
            Style::default().fg(ACCENT),
        ))));
    }

    let id_to_index: HashMap<&str, usize> = nodes
        .iter()
        .enumerate()
        .map(|(index, node)| (node.id.as_str(), index))
        .collect();

    // Parent (still-planned node) -> children among the planned set. Hard edges
    // bind before soft. A dep whose parent already launched is not in the map, so
    // such a node renders at root.
    let mut children: HashMap<usize, Vec<(usize, EdgeType)>> = HashMap::new();
    let mut has_planned_parent = vec![false; nodes.len()];
    for (child_index, node) in nodes.iter().enumerate() {
        for parent_id in &node.deps {
            if let Some(&parent_index) = id_to_index.get(parent_id.as_str()) {
                if parent_index == child_index {
                    continue;
                }
                children
                    .entry(parent_index)
                    .or_default()
                    .push((child_index, EdgeType::Hard));
                has_planned_parent[child_index] = true;
            }
        }
        for parent_id in &node.soft_deps {
            if node.deps.iter().any(|hard| hard == parent_id) {
                continue;
            }
            if let Some(&parent_index) = id_to_index.get(parent_id.as_str()) {
                if parent_index == child_index {
                    continue;
                }
                children
                    .entry(parent_index)
                    .or_default()
                    .push((child_index, EdgeType::Soft));
                has_planned_parent[child_index] = true;
            }
        }
    }

    let mut visited = vec![false; nodes.len()];
    let mut lanes: Vec<bool> = Vec::new();
    for index in 0..nodes.len() {
        if !has_planned_parent[index] {
            planned_walk(
                lines,
                nodes,
                &children,
                index,
                None,
                true,
                focused,
                task_width,
                &mut lanes,
                &mut visited,
                blocked_reasons,
            );
        }
    }
    // Anything trapped in a planned-node cycle renders at root so nothing hides.
    for index in 0..nodes.len() {
        if !visited[index] {
            planned_walk(
                lines,
                nodes,
                &children,
                index,
                None,
                true,
                focused,
                task_width,
                &mut lanes,
                &mut visited,
                blocked_reasons,
            );
        }
    }
    true
}

// --- Orchestrator view -----------------------------------------------------
//
// A RudderPlan agent is the pinned orchestrator: it owns the active plan. While it
// is still decomposing the task (Running and no parseable RUDDER_PLAN_TASKS block
// yet) the orchestrator is in the PLANNING phase and shows an animated spinner.
// Once a plan block parses it flips to PLAN-READY and renders a DAG tree of the
// parsed tasks, each row badged with that task's LIVE status (todo / in-progress /
// done / failed) derived from the planned-node queue and launched agents.

/// Diamond marker used to identify the orchestrator row and header.
const ORCH_MARK: &str = "\u{25C6}"; // ◆

/// Live status of a single plan task, derived from the planned-node queue (todo)
/// and any agent launched from that node id (running/done/failed).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum OrchTaskStatus {
    Todo,
    Running,
    /// The worker finished (AgentStatus::Done) but has NOT merged yet: it is
    /// awaiting review, exactly like the agents-pane Review bucket. Reserve Done
    /// for Merged so the orchestrator DAG matches the agents-pane buckets.
    Review,
    Done,
    Failed,
}

impl OrchTaskStatus {
    fn label(self) -> &'static str {
        match self {
            OrchTaskStatus::Todo => "todo",
            OrchTaskStatus::Running => "in-progress",
            OrchTaskStatus::Review => "review",
            OrchTaskStatus::Done => "done",
            OrchTaskStatus::Failed => "failed",
        }
    }

    fn color(self) -> Color {
        match self {
            OrchTaskStatus::Todo => ST_PLANNED,
            OrchTaskStatus::Running => ST_RUNNING,
            OrchTaskStatus::Review => ST_REVIEW,
            OrchTaskStatus::Done => ST_MERGED,
            OrchTaskStatus::Failed => ST_FAILED,
        }
    }
}

/// Derive a plan task's live status: a launched agent tagged with this node id
/// wins (Merged -> done, Done-not-merged -> review, Failed/Stopped -> failed,
/// otherwise in-progress); else if it is still a pending planned node it is todo.
/// Mirrors the agents-pane status buckets: Review is reserved for finished-but-
/// unmerged, Done for Merged.
pub(crate) fn orchestrator_task_status(app: &App, node_id: &str) -> OrchTaskStatus {
    // A node id can map to more than one run (e.g. a failed launch followed by a relaunch).
    // Pick the MOST SIGNIFICANT rather than the first match, so a live/merged relaunch is
    // not masked by a stale Failed/Stopped run; ties resolve to the most recent (later)
    // run since agents are appended in order.
    let rank = |s: &OrchTaskStatus| match s {
        OrchTaskStatus::Done => 4, // merged
        OrchTaskStatus::Running => 3,
        OrchTaskStatus::Review => 2,
        OrchTaskStatus::Failed => 1,
        OrchTaskStatus::Todo => 0,
    };
    let mut best: Option<OrchTaskStatus> = None;
    for run in app
        .agents
        .iter()
        .filter(|run| run.node_id.as_deref() == Some(node_id))
    {
        let status = match run.status {
            AgentStatus::Merged => OrchTaskStatus::Done,
            AgentStatus::Done => OrchTaskStatus::Review,
            AgentStatus::Failed | AgentStatus::Stopped | AgentStatus::Orphaned => {
                OrchTaskStatus::Failed
            }
            _ => OrchTaskStatus::Running,
        };
        if best.as_ref().is_none_or(|b| rank(&status) >= rank(b)) {
            best = Some(status);
        }
    }
    best.unwrap_or(OrchTaskStatus::Todo)
}

/// The orchestrator's current phase. `Planning` shows the spinner; `PlanReady`
/// carries the parsed tasks for the DAG tree.
pub(crate) enum OrchestratorPhase {
    Planning,
    PlanReady(Vec<RudderPlanTask>),
}

/// Compute the orchestrator phase from the agent's live output. A parseable
/// RUDDER_PLAN_TASKS block means the plan is ready (even while the planner process
/// is still winding down); otherwise it is still decomposing the task.
pub(crate) fn orchestrator_phase(agent: &AgentRun) -> OrchestratorPhase {
    rudder_plan_tasks_for_run(agent)
        .map(OrchestratorPhase::PlanReady)
        .unwrap_or(OrchestratorPhase::Planning)
}

fn orchestrator_phase_for_app(app: &App, agent: &AgentRun) -> OrchestratorPhase {
    if agent.is_orchestrator() && app.plan().planner_paused_for_input {
        OrchestratorPhase::Planning
    } else {
        orchestrator_phase(agent)
    }
}

/// Short phase label for the pinned orchestrator row: "planning" while
/// decomposing, then "plan N · running X" once tasks are parsed and launching.
fn orchestrator_phase_label(app: &App, agent: &AgentRun) -> String {
    match orchestrator_phase_for_app(app, agent) {
        OrchestratorPhase::Planning => "planning".to_string(),
        OrchestratorPhase::PlanReady(tasks) => {
            let running = tasks
                .iter()
                .filter(|t| orchestrator_task_status(app, &t.id) == OrchTaskStatus::Running)
                .count();
            format!("plan {} · running {}", tasks.len(), running)
        }
    }
}

/// Push the pinned orchestrator row: a diamond + selection marker, the task label,
/// then a phase line (spinner + "planning" while decomposing, or the plan summary).
fn push_orchestrator_row<'a>(
    lines: &mut Vec<Line<'a>>,
    app: &App,
    index: usize,
    agent: &AgentRun,
    focused: bool,
    task_width: usize,
) {
    let row_start = lines.len();
    let selected = index == app.selected_agent;
    let marker = if selected { "▶ " } else { "  " };
    let label = if selected && app.rename_input.is_some() {
        format!("✎ {}", app.rename_input.clone().unwrap_or_default())
    } else if agent.task_summary.trim().is_empty() {
        summarize_task(&agent.task)
    } else {
        agent.task_summary.clone()
    };
    // Match the agent rows: a slow-blinking accent arrow + a solid accent pill on the
    // selected title so the highlighted orchestrator reads at a glance.
    let marker_style = if selected {
        Style::default()
            .fg(ACCENT)
            .add_modifier(Modifier::BOLD | Modifier::SLOW_BLINK)
    } else {
        accent_style(focused)
    };
    let label_style = if selected {
        Style::default()
            .fg(PAPER)
            .bg(ACCENT)
            .add_modifier(Modifier::BOLD)
    } else {
        Style::default().fg(ACCENT)
    };
    lines.push((Line::from(vec![
        Span::styled(marker, marker_style),
        Span::styled(format!("{ORCH_MARK} "), label_style),
        Span::styled(
            truncate_chars(&label, task_width.saturating_sub(2) * 2),
            label_style,
        ),
    ])));

    // While the planner is still researching/decomposing, present it as the model's
    // PLAN MODE (Claude Code / Codex) so it reads as genuine plan mode; once the DAG is
    // parsed it becomes the orchestrator with its live status. Spin during planning.
    let planning = matches!(
        orchestrator_phase_for_app(app, agent),
        OrchestratorPhase::Planning
    ) && agent.status == AgentStatus::Running;
    // The plan is decomposed and waiting for the user to approve it before any
    // node launches. Surface that on the left panel in the attention color so
    // "it's asking for approval" is obvious without opening the plan.
    let awaiting = app.plan().awaiting_approval && agent.is_orchestrator() && !planning;
    let swarm = app.run_is_in_swarm(agent);
    let badge = if planning { app.spinner_glyph() } else { BADGE };
    let (role, phase) = if planning {
        let backend = match agent.backend {
            Backend::Claude => "Claude Code",
            Backend::Codex => "Codex",
            Backend::Opencode => "opencode",
        };
        ("plan mode", format!("{backend} · researching the plan"))
    } else if awaiting {
        (
            "awaiting approval",
            if swarm {
                "press Enter on this row to launch the experiments".to_string()
            } else {
                "press Enter on this row to launch the plan".to_string()
            },
        )
    } else if swarm {
        ("swarm", orchestrator_phase_label(app, agent))
    } else {
        ("orchestrator", orchestrator_phase_label(app, agent))
    };
    let row_accent = if awaiting { RUNNING_COLOR } else { ACCENT };
    let role_style = if awaiting {
        Style::default()
            .fg(RUNNING_COLOR)
            .add_modifier(Modifier::BOLD)
    } else {
        Style::default().fg(ACCENT)
    };
    let phase_style = if awaiting {
        Style::default()
            .fg(RUNNING_COLOR)
            .add_modifier(Modifier::BOLD)
    } else {
        muted_style(focused)
    };
    lines.push((Line::from(vec![
        Span::raw("  "),
        Span::styled(badge, Style::default().fg(row_accent)),
        Span::raw(" "),
        Span::styled(role, role_style),
        Span::raw("  "),
        Span::styled(phase, phase_style),
    ])));
    record_agent_rows(row_start, lines.len(), index);
}

pub(crate) fn render_agents(frame: &mut Frame<'_>, area: Rect, app: &mut App) {
    let focused = app.focus == FocusPane::Agents;
    // Fresh row-span recording for this frame; harvested into app.agent_row_map below.
    AGENT_ROW_SPANS.with(|spans| spans.borrow_mut().clear());
    DRAWER_ROW_SPANS.with(|spans| spans.borrow_mut().clear());
    app.normalize_drawer_state();
    note_card_changes(app, Instant::now());
    let diff_summaries: Vec<Option<String>> = {
        // Pinned planners (orchestrator + plan-mode front-end) run in the MAIN repo
        // and are rendered via push_orchestrator_row (no diff line), so a diff summary
        // there is both meaningless and never shown: skip it like main agents.
        // Only rows that are actually DRAWN. Each summary costs two git subprocesses
        // on a miss, and a row collapsed inside the done/closed/review drawer never
        // shows one — so asking for every agent made startup O(all rows ever) in
        // subprocess spawns. Measured at 200 seeded rows: ~2 git processes per row
        // before the first frame.
        let drawn: std::collections::HashSet<usize> =
            sidebar_agent_indices(&app.agents).into_iter().collect();
        let keys: Vec<(String, PathBuf, bool, bool)> = app
            .agents
            .iter()
            .enumerate()
            .map(|(index, a)| {
                (
                    a.id.clone(),
                    a.cwd.clone(),
                    a.is_main() || a.is_pinned_planner() || !drawn.contains(&index),
                    // Only a row whose workspace can still change needs re-asking.
                    matches!(a.status, AgentStatus::Running | AgentStatus::Paused),
                )
            })
            .collect();
        keys.iter()
            .map(|(id, cwd, skip, live)| {
                if *skip {
                    None
                } else {
                    app.cached_diff_summary(id, cwd, *live)
                }
            })
            .collect()
    };
    // Project, branch and version live in the header row now; the card list
    // starts with the cards.
    let mut lines: Vec<Line<'_>> = Vec::new();
    if app.nest_view {
        lines.push(Line::from(Span::styled("nest view", header_style(focused))));
        lines.push(Line::default());
    }
    // The board URL is intentionally NOT shown in the UI; the "o web ui" hint below
    // (and /web) opens this project's board in the browser. Keeping the URL off-screen
    // avoids clipping it in this narrow pane and keeps the dashboard uncluttered.
    if is_cloud_worker_session() {
        // Only surface cloud status when this dashboard is actually running
        // inside a cloud worker. Showing "cloud connected" in a plain local
        // rudder session is misleading: it reflects whether the user has saved
        // cloud auth, not whether anything is attached.
        lines.push(Line::from(vec![
            Span::styled("☁ ", cloud_style(app.cloud_connected, focused)),
            Span::styled(
                cloud_workspace_label(app.cloud_workspace.as_ref()),
                cloud_style(app.cloud_workspace.is_some(), focused),
            ),
        ]));
        lines.push(Line::default());
    }

    let task_width = area.width.saturating_sub(4).max(12) as usize;

    // Mirrors `sectioned_agent_order`: a pinned planner draws in the orchestrator
    // block above, so it must not also count as (or render in) the main section.
    let main_indices: Vec<usize> = app
        .agents
        .iter()
        .enumerate()
        .filter(|(_, a)| a.is_main() && !a.is_pinned_planner())
        .map(|(index, _)| index)
        .collect();
    let main_count = main_indices.len();
    let run_total = app.agents.iter().filter(|a| !a.is_main()).count();
    let orchestrator_count = app.agents.iter().filter(|a| a.is_pinned_planner()).count();

    // Pinned planners render at the very top of BOTH views: one distinct accent +
    // diamond row per orchestrator / plan-mode front-end showing its phase, above
    // main and every status section. There is normally one at a time.
    if orchestrator_count > 0 {
        lines.push((Line::from(Span::styled(
            "orchestrator",
            header_style(focused),
        ))));
        for (index, agent) in app.agents.iter().enumerate() {
            if agent.is_pinned_planner() {
                push_orchestrator_row(&mut lines, app, index, agent, focused, task_width);
            }
        }
        lines.push((Line::default()));
    }

    if app.nest_view {
        // Alternate full-DAG view (toggled with `g`): one topological tree across
        // every agent, ignoring status sections.
        render_agents_nest_into(&mut lines, app, focused, task_width, &diff_summaries);
        if main_count == 0 && run_total == 0 {
            push_empty_state(&mut lines, focused);
        }
    } else {
        // Default view: main agents first, then status sections (todo / in progress
        // / review / done / closed), each header carrying a count and omitted when
        // empty. Dependents nest under their parents within a section.
        if main_count > 0 {
            // Running several `/main` agents at once is supported, but they all edit
            // the SAME working copy, so the header states it: with only the per-row
            // "main" tag the shared-checkout risk had to be inferred by counting rows.
            let noun = if main_count == 1 { "agent" } else { "agents" };
            let (section_name, location) = if is_cloud_workspace_session() {
                ("☁ cloud", "persistent")
            } else if is_cloud_worker_session() {
                ("☁ cloud", "ephemeral")
            } else {
                ("main", "shared checkout")
            };
            lines.push((Line::from(vec![
                Span::styled(section_name, header_style(focused)),
                Span::styled(
                    format!(" {main_count} {noun} · {location}"),
                    muted_style(focused),
                ),
            ])));
            for &index in &main_indices {
                push_agent_row(
                    &mut lines,
                    app,
                    index,
                    &app.agents[index],
                    None,
                    focused,
                    task_width,
                    &[],
                    &[],
                );
            }
        }

        let mut emitted_any_section = false;
        for bucket in Bucket::ORDER {
            // Leave a blank line before any non-first block (after main or a prior
            // section) for visual separation.
            let leading_blank = main_count > 0 || emitted_any_section;
            // The Todo section is the planned-node queue (no agent ever buckets
            // there); every other section renders agents by status.
            let emitted = if bucket == Bucket::Todo {
                // A queued node whose HARD dep FAILED can never launch until the
                // user retries/deletes the failed node; say so ON the row (the
                // one-shot notice was easy to miss and the node then sat in todo
                // with no explanation forever).
                let failed_ids: std::collections::HashSet<&str> = app
                    .agents
                    .iter()
                    .filter(|run| run.status == AgentStatus::Failed)
                    .filter_map(|run| run.node_id.as_deref())
                    .collect();
                let blocked_reasons: std::collections::HashMap<String, String> = app
                    .plan()
                    .planned_nodes
                    .iter()
                    .filter_map(|node| {
                        node.deps
                            .iter()
                            .find(|dep| failed_ids.contains(dep.as_str()))
                            .map(|dep| (node.id.clone(), dep.clone()))
                    })
                    .collect();
                // EVERY plan's queue, not just the selected one's: the agents pane is
                // the whole fleet, so work waiting in another plan must not vanish from
                // it just because a different orchestrator is focused. Node ids are kept
                // unique across plans, so the flat list stays unambiguous.
                let queued: Vec<&PlannedNode> = app
                    .plans
                    .iter()
                    .flat_map(|plan| plan.planned_nodes.iter())
                    .collect();
                let awaiting = app
                    .plans
                    .iter()
                    .any(|plan| plan.awaiting_approval && !plan.planned_nodes.is_empty());
                render_planned_section(
                    &mut lines,
                    &queued,
                    focused,
                    task_width,
                    leading_blank,
                    awaiting,
                    &blocked_reasons,
                )
            } else {
                render_status_section(
                    &mut lines,
                    app,
                    focused,
                    task_width,
                    &diff_summaries,
                    bucket,
                    leading_blank,
                )
            };
            if emitted {
                emitted_any_section = true;
            }
        }

        if main_count == 0
            && run_total == 0
            && app.plans.iter().all(|plan| plan.planned_nodes.is_empty())
        {
            push_empty_state(&mut lines, focused);
        }
    }

    // Wrap anything wider than the pane onto indented continuation rows instead
    // of letting the list clip it: zoomed in (Cmd+), the 34-column sidebar cut
    // titles, statuses and the repo path mid-word. Every row stays one screen
    // row, so the recorded spans are shifted to the rows they now occupy.
    let inner = area.width.saturating_sub(2) as usize;
    let lines = {
        let mut wrapped = Vec::with_capacity(lines.len());
        let mut starts = Vec::with_capacity(lines.len() + 1);
        // Cards and sections each bring their own breathing room; two blank
        // rows in a row (a card's gap, then a section's) collapse into one, and
        // the list never starts with one.
        let mut last_blank = true;
        for line in lines {
            starts.push(wrapped.len());
            let blank = line.width() == 0;
            if blank && last_blank {
                continue;
            }
            last_blank = blank;
            wrapped.extend(wrap_sidebar_line(line, inner));
        }
        starts.push(wrapped.len());
        let moved = |row: usize| starts.get(row).copied().unwrap_or(wrapped.len());
        AGENT_ROW_SPANS.with(|spans| {
            for span in spans.borrow_mut().iter_mut() {
                *span = (moved(span.0), moved(span.1), span.2);
            }
        });
        DRAWER_ROW_SPANS.with(|spans| {
            for span in spans.borrow_mut().iter_mut() {
                span.0 = moved(span.0);
            }
        });
        wrapped
    };

    // Harvest the recorded row spans into the row -> agent map mouse clicks resolve
    // against. Built from what was ACTUALLY rendered this frame, so it stays correct
    // however the header, hints, sections, or per-row line counts evolve.
    let mut row_map: Vec<Option<usize>> = vec![None; lines.len()];
    AGENT_ROW_SPANS.with(|spans| {
        for &(start, end, agent) in spans.borrow().iter() {
            for row in row_map.iter_mut().take(end).skip(start) {
                *row = Some(agent);
            }
        }
    });
    app.agent_row_map = row_map;
    let mut drawer_map: Vec<Option<Bucket>> = vec![None; lines.len()];
    DRAWER_ROW_SPANS.with(|spans| {
        for &(row, bucket) in spans.borrow().iter() {
            if let Some(slot) = drawer_map.get_mut(row) {
                *slot = Some(bucket);
            }
        }
    });
    app.drawer_row_map = drawer_map;

    // Scroll the pane to follow the selection. The list was rendered stateless before,
    // so it always drew from the top: with more agents than fit, the selected row (and
    // the bottom of the tree) was clipped off-screen. Compute the minimal top offset
    // that keeps the selected agent's whole row group visible, persisted across frames.
    let view_h = area.height.saturating_sub(2) as usize; // minus the block's top+bottom border
    let total = lines.len();
    let mut offset = app.agents_scroll;
    if view_h > 0 {
        let mut first: Option<usize> = None;
        let mut last: Option<usize> = None;
        // The cursor is EITHER on an agent's row group or on a collapsed drawer
        // header. Following only the agent case left the drawer headers unreachable
        // whenever they sat below the fold: j walked onto them and the pane never
        // scrolled, so the marker vanished off-screen.
        if let Some(bucket) = app.drawer_cursor.filter(|bucket| app.drawer_open() != Some(*bucket)) {
            for (i, m) in app.drawer_row_map.iter().enumerate() {
                if *m == Some(bucket) {
                    first = Some(i);
                    last = Some(i);
                }
            }
        } else {
            for (i, m) in app.agent_row_map.iter().enumerate() {
                if *m == Some(app.selected_agent) {
                    if first.is_none() {
                        first = Some(i);
                    }
                    last = Some(i);
                }
            }
        }
        if let (Some(first), Some(last)) = (first, last) {
            if last + 1 <= view_h {
                // The whole selection fits on the first page: show from the very top so
                // the pane header (cwd, run count, hints) stays visible.
                offset = 0;
            } else if first < offset {
                // Selection is above the viewport: scroll up to reveal its top.
                offset = first;
            } else if last >= offset + view_h {
                // Selection is below the viewport: scroll down to reveal its bottom.
                offset = last + 1 - view_h;
            }
            // If the group alone is taller than the viewport, prefer showing its top.
            if last + 1 - first > view_h {
                offset = first;
            }
        }
        // Never scroll past the end (which would leave dead space at the bottom).
        offset = offset.min(total.saturating_sub(view_h));
    } else {
        offset = 0;
    }
    app.agents_scroll = offset;

    // Borderless: the cards are the structure. The list keeps a one-cell inset
    // (block_inner) so mouse hit-testing and scroll math stay unchanged.
    let mut list_state = ratatui::widgets::ListState::default().with_offset(offset);
    frame.render_widget(Block::default().style(app_style()), area);
    frame.render_stateful_widget(
        List::new(lines.into_iter().map(ListItem::new).collect::<Vec<_>>()).style(app_style()),
        block_inner(area),
        &mut list_state,
    );
}

// --- Nest / DAG view -------------------------------------------------------
//
// ratatui has no tree widget, so the dependency tree is drawn with manual jj-log
// glyphs: a colored status BADGE node marker plus connector glyphs for ancestry.
// HARD-edge connectors render solid in MUTED; SOFT-edge connectors render with a
// dashed glyph in FAINT + DIM. The walk is a depth-first topological pass from
// roots (nodes with no present parent), guarding cycles with a visited set so an
// orphaned cycle still renders (at depth 0) rather than looping forever.

const NEST_LANE_BAR: &str = "\u{2502} "; // "│ " ancestor lane continues
const NEST_LANE_BLANK: &str = "  "; // ancestor lane ended
const NEST_TEE_HARD: &str = "\u{251c}\u{2500}"; // "├─" child with a following sibling
const NEST_CORNER_HARD: &str = "\u{2570}\u{2500}"; // "╰─" last child
const NEST_TEE_SOFT: &str = "\u{251c}\u{2504}"; // "├┄" soft, dashed feel
const NEST_CORNER_SOFT: &str = "\u{2570}\u{2504}"; // "╰┄" soft, dashed feel

/// Build the parent->children adjacency for the displayed agents, plus the edge
/// type for each (child, parent) relation. Children are kept in display (index)
/// order so the tree renders deterministically.
pub(crate) fn nest_children_by_index(
    agents: &[AgentRun],
) -> (
    std::collections::HashMap<usize, Vec<(usize, EdgeType)>>,
    Vec<bool>,
) {
    use std::collections::HashMap;
    let id_to_index: HashMap<&str, usize> = agent_id_index(agents);

    let mut children: HashMap<usize, Vec<(usize, EdgeType)>> = HashMap::new();
    let mut has_parent = vec![false; agents.len()];

    for (child_index, agent) in agents.iter().enumerate() {
        // Hard parents first so the most-binding edge wins if a parent appears in
        // both lists; soft parents only contribute a soft edge when not already hard.
        for parent_id in &agent.deps {
            if let Some(&parent_index) = id_to_index.get(parent_id.as_str()) {
                if parent_index == child_index {
                    continue;
                }
                children
                    .entry(parent_index)
                    .or_default()
                    .push((child_index, EdgeType::Hard));
                has_parent[child_index] = true;
            }
        }
        for parent_id in &agent.soft_deps {
            if agent.deps.iter().any(|hard| hard == parent_id) {
                continue;
            }
            if let Some(&parent_index) = id_to_index.get(parent_id.as_str()) {
                if parent_index == child_index {
                    continue;
                }
                children
                    .entry(parent_index)
                    .or_default()
                    .push((child_index, EdgeType::Soft));
                has_parent[child_index] = true;
            }
        }
    }

    (children, has_parent)
}

/// Connector glyph + style for the first (task-label) line of a node, given the
/// ancestor lane state, whether this node is the last among its siblings, and the
/// edge type from its parent. `lanes[i]` is true when ancestor level `i` still has
/// a following sibling (draw a vertical bar) and false when its subtree ended.
fn nest_prefix(lanes: &[bool], is_last: bool, edge: Option<EdgeType>) -> Vec<Span<'static>> {
    let mut spans = Vec::new();
    for continues in lanes {
        spans.push(Span::styled(
            if *continues {
                NEST_LANE_BAR
            } else {
                NEST_LANE_BLANK
            },
            Style::default().fg(MUTED),
        ));
    }
    if let Some(edge) = edge {
        let (glyph, style) = match edge {
            EdgeType::Hard => (
                if is_last {
                    NEST_CORNER_HARD
                } else {
                    NEST_TEE_HARD
                },
                Style::default().fg(MUTED),
            ),
            EdgeType::Soft => (
                if is_last {
                    NEST_CORNER_SOFT
                } else {
                    NEST_TEE_SOFT
                },
                Style::default().fg(FAINT).add_modifier(Modifier::DIM),
            ),
        };
        spans.push(Span::styled(glyph, style));
    }
    spans
}

/// Continuation-line prefix (for the status/diff rows under a node): the ancestor
/// lanes plus this node's own lane, so child rows align beneath the task label.
fn nest_cont_prefix(lanes: &[bool], self_continues: bool, is_root: bool) -> Vec<Span<'static>> {
    let mut spans = Vec::new();
    for continues in lanes {
        spans.push(Span::styled(
            if *continues {
                NEST_LANE_BAR
            } else {
                NEST_LANE_BLANK
            },
            Style::default().fg(MUTED),
        ));
    }
    // The node's own subtree lane: a bar while it (or its later siblings) continue.
    if !is_root {
        spans.push(Span::styled(
            if self_continues {
                NEST_LANE_BAR
            } else {
                NEST_LANE_BLANK
            },
            Style::default().fg(MUTED),
        ));
    } else if self_continues {
        spans.push(Span::styled(NEST_LANE_BAR, Style::default().fg(MUTED)));
    }
    spans
}

#[allow(clippy::too_many_arguments)]
fn nest_walk<'a>(
    lines: &mut Vec<Line<'a>>,
    app: &App,
    focused: bool,
    task_width: usize,
    diff_summaries: &[Option<String>],
    children: &std::collections::HashMap<usize, Vec<(usize, EdgeType)>>,
    index: usize,
    edge: Option<EdgeType>,
    is_last: bool,
    lanes: &mut Vec<bool>,
    visited: &mut Vec<bool>,
) {
    if visited[index] {
        return;
    }
    visited[index] = true;

    let agent = &app.agents[index];
    let is_root = lanes.is_empty();
    let prefix = nest_prefix(lanes, is_last, edge);
    let own_children = children.get(&index).map(Vec::as_slice).unwrap_or(&[]);
    let has_unvisited_children = own_children.iter().any(|(child, _)| !visited[*child]);
    let cont_prefix = nest_cont_prefix(lanes, has_unvisited_children, is_root);

    let summary = if agent.is_main() {
        None
    } else {
        diff_summaries.get(index).and_then(|opt| opt.clone())
    };
    push_agent_row(
        lines,
        app,
        index,
        agent,
        summary,
        focused,
        task_width,
        &prefix,
        &cont_prefix,
    );

    // Recurse into children that have not yet been placed in the tree.
    let pending: Vec<(usize, EdgeType)> = own_children
        .iter()
        .copied()
        .filter(|(child, _)| !visited[*child])
        .collect();
    for (position, (child_index, child_edge)) in pending.iter().enumerate() {
        // In a diamond DAG an earlier sibling's subtree may have already drawn this child
        // (it has two parents); it would be skipped on entry, so skip it here too and do
        // not let it affect the is_last decision for the children that ARE drawn.
        if visited[*child_index] {
            continue;
        }
        // "Last" = no LATER pending child is still undrawn — computed dynamically so the
        // connector is a corner (not a tee) for the child that is actually drawn last,
        // even when later pending entries were already placed under an earlier sibling.
        let child_is_last = pending[position + 1..]
            .iter()
            .all(|(later, _)| visited[*later]);
        // Push THIS node's continuation (does it have a following sibling) as the ancestor
        // lane for its descendants — not the child's own flag, which double-counted and
        // misaligned the connectors.
        lanes.push(!is_last);
        nest_walk(
            lines,
            app,
            focused,
            task_width,
            diff_summaries,
            children,
            *child_index,
            Some(*child_edge),
            child_is_last,
            lanes,
            visited,
        );
        lanes.pop();
    }
}

/// Test-facing wrapper that materializes the nest view as its own vec.
#[cfg(test)]
pub(crate) fn render_agents_nest(
    app: &App,
    focused: bool,
    task_width: usize,
    diff_summaries: &[Option<String>],
) -> Vec<Line<'static>> {
    let mut lines: Vec<Line<'static>> = Vec::new();
    render_agents_nest_into(&mut lines, app, focused, task_width, diff_summaries);
    lines
}

/// Nest-view body pushed onto the CALLER's row vec, so the row positions the agent
/// row-span recorder captures are absolute agents-pane rows (mouse hit-testing
/// resolves against them).
fn render_agents_nest_into<'a>(
    lines: &mut Vec<Line<'a>>,
    app: &App,
    focused: bool,
    task_width: usize,
    diff_summaries: &[Option<String>],
) {
    let (children, has_parent) = nest_children_by_index(&app.agents);
    let mut visited = vec![false; app.agents.len()];

    // Pinned planners are drawn as a dedicated row above this tree, so mark them
    // visited to keep the nest walk (and its orphan sweep) from re-drawing them.
    for index in 0..app.agents.len() {
        if app.agents[index].is_pinned_planner() {
            visited[index] = true;
        }
    }

    // Roots: any node with no present parent, in display order. Main agents have no
    // deps and so are always roots, rendered first.
    let mut roots: Vec<usize> = (0..app.agents.len())
        .filter(|index| app.agents[*index].is_main() && !app.agents[*index].is_pinned_planner())
        .collect();
    roots.extend((0..app.agents.len()).filter(|index| {
        !app.agents[*index].is_main()
            && !app.agents[*index].is_pinned_planner()
            && !has_parent[*index]
    }));

    let mut lanes: Vec<bool> = Vec::new();
    for &root in &roots {
        nest_walk(
            lines,
            app,
            focused,
            task_width,
            diff_summaries,
            &children,
            root,
            None,
            true,
            &mut lanes,
            &mut visited,
        );
    }

    // Any node not reached from a root (e.g. part of a dependency cycle) is an
    // orphan; render it at depth 0 so a cycle never hides work or loops.
    for index in 0..app.agents.len() {
        if !visited[index] {
            nest_walk(
                lines,
                app,
                focused,
                task_width,
                diff_summaries,
                &children,
                index,
                None,
                true,
                &mut lanes,
                &mut visited,
            );
        }
    }
}

pub(crate) fn visible_agent_indices(agents: &[AgentRun]) -> Vec<usize> {
    // Navigation follows the rendered order: main agents first, then each status
    // section nested by dependency. Sharing `sectioned_agent_order` guarantees the
    // selection marker lands exactly where j/k move.
    //
    // Drawer members are drawn inside their drawer rather than as sidebar rows, so the
    // sectioned view's navigation skips them (see `App::visible_agent_indices`). This
    // free function stays the full rendered order: nest view (`g`) still draws every
    // run as its own row, and filtering here would desync that view's j/k.
    sectioned_agent_order(agents)
}

/// The sectioned view's navigable rows: everything except the runs that a collapsed
/// drawer holds.
pub(crate) fn sidebar_agent_indices(agents: &[AgentRun]) -> Vec<usize> {
    visible_agent_indices(agents)
        .into_iter()
        .filter(|&index| {
            agents
                .get(index)
                .is_none_or(|agent| !in_drawer(agents, agent))
        })
        .collect()
}

/// The navigation order for the NEST view (`g`), mirroring `render_agents_nest` exactly
/// so j/k land on the row that is actually drawn. The sectioned order cannot be reused
/// here because nest view walks the dependency tree globally (crossing status buckets)
/// rather than within each section. Order: pinned orchestrators (drawn above the tree),
/// then mains, then dependency roots, then the tree (DFS via the children map), then any
/// cycle-trapped orphans.
pub(crate) fn nest_agent_order(agents: &[AgentRun]) -> Vec<usize> {
    let (children, has_parent) = nest_children_by_index(agents);
    let mut order: Vec<usize> = Vec::new();
    let mut visited = vec![false; agents.len()];

    for index in 0..agents.len() {
        if agents[index].is_pinned_planner() {
            order.push(index);
            visited[index] = true;
        }
    }
    let mut roots: Vec<usize> = (0..agents.len())
        .filter(|&i| agents[i].is_main() && !agents[i].is_pinned_planner())
        .collect();
    roots.extend(
        (0..agents.len())
            .filter(|&i| !agents[i].is_main() && !agents[i].is_pinned_planner() && !has_parent[i]),
    );
    for &root in &roots {
        nest_order_walk(&mut order, &children, root, &mut visited);
    }
    for index in 0..agents.len() {
        if !visited[index] {
            nest_order_walk(&mut order, &children, index, &mut visited);
        }
    }
    order
}

fn nest_order_walk(
    order: &mut Vec<usize>,
    children: &std::collections::HashMap<usize, Vec<(usize, EdgeType)>>,
    index: usize,
    visited: &mut Vec<bool>,
) {
    if visited[index] {
        return;
    }
    visited[index] = true;
    order.push(index);
    if let Some(kids) = children.get(&index) {
        for (child, _) in kids {
            if !visited[*child] {
                nest_order_walk(order, children, *child, visited);
            }
        }
    }
}

/// Push one task row of the orchestrator DAG tree: nest glyphs, a live-status
/// BADGE, the task title, and the status label. Mirrors `push_planned_row` styling
/// but the badge color reflects the task's LIVE status.
fn push_orchestrator_task_row<'a>(
    lines: &mut Vec<Line<'a>>,
    task: &RudderPlanTask,
    status: OrchTaskStatus,
    prefix: &[Span<'a>],
) {
    let label = if task.title.trim().is_empty() {
        summarize_task(&task.prompt)
    } else {
        task.title.clone()
    };
    let mut spans = prefix.to_vec();
    spans.extend([
        Span::styled(BADGE, Style::default().fg(status.color())),
        Span::raw(" "),
        // The node id is the cross-reference key: the same id shows on the worker's
        // agents-pane row and in the worker pane title, so "n2 here" is findable there.
        Span::styled(format!("{} ", task.id), muted_style(true)),
        Span::styled(label, pane_text_style(true)),
        Span::raw("  "),
        Span::styled(status.label(), Style::default().fg(status.color())),
    ]);
    lines.push(Line::from(spans));
}

/// Depth-first walk of the parsed plan task forest for the orchestrator DAG tree.
/// Nests a task under a parent that is also in the plan (hard edges solid MUTED,
/// soft edges dashed/dimmed), reusing the same nest glyphs as the agents pane.
#[allow(clippy::too_many_arguments)]
/// Build the orchestrator DAG tree structure. Nesting follows HARD edges only:
/// soft edges are advisory "context" that never gate launch, so soft-linked tasks
/// run in parallel and must NOT be drawn as a sequential chain. A node nests under its
/// DEEPEST hard prerequisite (longest hard chain, ties by index) so an integrator
/// renders below its real inputs; a node with no hard parent is a top-level (parallel)
/// root. Returns
/// the children adjacency and which nodes have a hard parent. Pure, so the nesting
/// is unit-testable.
/// Longest hard-dependency chain depth for task `index` over the FULL hard-edge graph
/// (memoized, cycle-safe). A node with no hard prereqs is depth 0. Used so a JOIN nests
/// under its DEEPEST prerequisite rather than under whichever has the largest id.
fn hard_chain_depth(
    index: usize,
    tasks: &[RudderPlanTask],
    id_to_index: &std::collections::HashMap<&str, usize>,
    memo: &mut [Option<usize>],
    in_progress: &mut [bool],
) -> usize {
    if let Some(depth) = memo[index] {
        return depth;
    }
    if in_progress[index] {
        return 0; // cycle: break the recursion
    }
    in_progress[index] = true;
    let depth = tasks[index]
        .deps
        .iter()
        .filter(|edge| edge.edge == EdgeType::Hard)
        .filter_map(|edge| id_to_index.get(edge.on.as_str()).copied())
        .filter(|&parent| parent != index)
        .map(|parent| 1 + hard_chain_depth(parent, tasks, id_to_index, memo, in_progress))
        .max()
        .unwrap_or(0);
    in_progress[index] = false;
    memo[index] = Some(depth);
    depth
}

pub(crate) fn orchestrator_hard_tree(
    tasks: &[RudderPlanTask],
) -> (
    std::collections::HashMap<usize, Vec<(usize, EdgeType)>>,
    Vec<bool>,
) {
    use std::collections::HashMap;
    let id_to_index: HashMap<&str, usize> = tasks
        .iter()
        .enumerate()
        .map(|(index, task)| (task.id.as_str(), index))
        .collect();
    let mut memo = vec![None; tasks.len()];
    let mut in_progress = vec![false; tasks.len()];
    let depths: Vec<usize> = (0..tasks.len())
        .map(|i| hard_chain_depth(i, tasks, &id_to_index, &mut memo, &mut in_progress))
        .collect();
    let mut children: HashMap<usize, Vec<(usize, EdgeType)>> = HashMap::new();
    let mut has_parent = vec![false; tasks.len()];
    for (child_index, task) in tasks.iter().enumerate() {
        // Of all resolvable hard prerequisites, nest under the DEEPEST (longest hard
        // chain), ties broken by index. So an integrator that depends on several units
        // renders below its deepest input instead of under a shallow parallel root.
        let hard_parent = task
            .deps
            .iter()
            .filter(|edge| edge.edge == EdgeType::Hard)
            .filter_map(|edge| id_to_index.get(edge.on.as_str()).copied())
            .filter(|&parent_index| parent_index != child_index)
            .max_by_key(|&parent_index| (depths[parent_index], parent_index));
        if let Some(parent_index) = hard_parent {
            children
                .entry(parent_index)
                .or_default()
                .push((child_index, EdgeType::Hard));
            has_parent[child_index] = true;
        }
    }
    (children, has_parent)
}

/// Build the orchestrator DAG tree rows (no status/summary lines). Roots (no resolvable
/// hard parent) first, depth-first; any cycle-trapped node is then rendered at depth 0 so
/// work is never hidden. Extracted so the rendered structure is unit-testable.
pub(crate) fn orchestrator_dag_tree_lines(
    app: &App,
    tasks: &[RudderPlanTask],
) -> Vec<Line<'static>> {
    let mut dag: Vec<Line<'static>> = Vec::new();
    let (children, has_parent) = orchestrator_hard_tree(tasks);
    let mut visited = vec![false; tasks.len()];
    let mut lanes: Vec<bool> = Vec::new();
    for index in 0..tasks.len() {
        if !has_parent[index] {
            orchestrator_tree_walk(
                &mut dag,
                app,
                tasks,
                &children,
                index,
                None,
                true,
                &mut lanes,
                &mut visited,
            );
        }
    }
    for index in 0..tasks.len() {
        if !visited[index] {
            orchestrator_tree_walk(
                &mut dag,
                app,
                tasks,
                &children,
                index,
                None,
                true,
                &mut lanes,
                &mut visited,
            );
        }
    }
    dag
}

fn orchestrator_tree_walk<'a>(
    lines: &mut Vec<Line<'a>>,
    app: &App,
    tasks: &[RudderPlanTask],
    children: &std::collections::HashMap<usize, Vec<(usize, EdgeType)>>,
    index: usize,
    edge: Option<EdgeType>,
    is_last: bool,
    lanes: &mut Vec<bool>,
    visited: &mut Vec<bool>,
) {
    if visited[index] {
        return;
    }
    visited[index] = true;
    let prefix = nest_prefix(lanes, is_last, edge);
    let status = orchestrator_task_status(app, &tasks[index].id);
    push_orchestrator_task_row(lines, &tasks[index], status, &prefix);

    let own_children = children.get(&index).map(Vec::as_slice).unwrap_or(&[]);
    let pending: Vec<(usize, EdgeType)> = own_children
        .iter()
        .copied()
        .filter(|(child, _)| !visited[*child])
        .collect();
    for (position, (child_index, child_edge)) in pending.iter().enumerate() {
        let child_is_last = position + 1 == pending.len();
        // Push THIS node's continuation (does it have a following sibling), which becomes
        // the ancestor lane drawn to the LEFT of every descendant. (Pushing the child's
        // own flag added a spurious extra column and misaligned the connectors.)
        lanes.push(!is_last);
        orchestrator_tree_walk(
            lines,
            app,
            tasks,
            children,
            *child_index,
            Some(*child_edge),
            child_is_last,
            lanes,
            visited,
        );
        lanes.pop();
    }
}

/// Custom command-center view for a selected orchestrator (RudderPlan) agent,
/// rendered in place of the raw planner PTY. PLANNING shows an animated spinner;
/// PLAN-READY renders a DAG tree of the parsed tasks with live status badges.
pub(crate) fn render_orchestrator(frame: &mut Frame<'_>, area: Rect, app: &mut App) {
    use std::collections::HashMap;
    let focused = app.focus == FocusPane::Worker;
    let Some(agent) = app.agents.get(app.selected_agent) else {
        return;
    };

    let phase = orchestrator_phase_for_app(app, agent);
    let swarm = app.run_is_in_swarm(agent);
    let phase_label = match app.plan().final_gate_status {
        FinalGateStatus::Running => "all integrated · verifying".to_string(),
        FinalGateStatus::Passed => "all integrated · checks passed".to_string(),
        FinalGateStatus::Failed => "all integrated · checks failed".to_string(),
        FinalGateStatus::Idle => match &phase {
            OrchestratorPhase::Planning => "plan mode".to_string(),
            OrchestratorPhase::PlanReady(tasks) if swarm => {
                format!("swarm · {} experiments", tasks.len())
            }
            OrchestratorPhase::PlanReady(tasks) => format!("plan · {} tasks", tasks.len()),
        },
    };
    // The model + version doing the planning, shown on the header so it is always clear
    // which agent/version is in plan mode (e.g. "Claude Code · opus[1m]").
    let backend_label = match agent.backend {
        Backend::Claude => "Claude Code",
        Backend::Codex => "Codex",
        Backend::Opencode => "opencode",
    };
    let model_label = if agent.model.trim().is_empty() {
        backend_label.to_string()
    } else {
        format!("{backend_label} · {}", agent.model.trim())
    };

    // Draw the pane border first; content is laid out inside it. Splitting the
    // inner area into fixed (header + DAG) and scrollable (plan prose) regions lets
    // the DAG tree stay PINNED on top while only the prose plan scrolls.
    let block = pane_block("orchestrator", focused, app.nav_mode);
    let inner = block.inner(area);
    frame.render_widget(block, area);

    // Header: ◆ ORCHESTRATOR · phase · <backend · model>.
    let header_spans = vec![
        Span::styled(
            format!("{ORCH_MARK} {}", if swarm { "SWARM" } else { "ORCHESTRATOR" }),
            Style::default().fg(ACCENT),
        ),
        Span::styled("  ·  ", muted_style(focused)),
        Span::styled(phase_label, muted_style(focused)),
        Span::styled("  ·  ", muted_style(focused)),
        Span::styled(model_label, Style::default().fg(ACCENT)),
    ];
    let header: Vec<Line<'static>> = vec![Line::from(header_spans)];

    // Chat input (pinned at the bottom when focused): the follow-up being composed,
    // with a visible block cursor at worker_input_cursor so Left/Right/Home/End and
    // typing show where the insertion point is. Rendered inline (a reversed cell) so
    // it tracks the text through wrapping without any coordinate math.
    let draft = agent.worker_input_draft.clone();
    let cursor_col = agent.worker_input_cursor;
    let input: Vec<Line<'static>> = if focused {
        let mut spans = vec![Span::styled("› ", Style::default().fg(ACCENT))];
        let cursor_style = pane_text_style(focused).add_modifier(Modifier::REVERSED);
        let chars: Vec<char> = draft.chars().collect();
        if chars.is_empty() {
            // Cursor block, then the dimmed hint.
            spans.push(Span::styled(" ", cursor_style));
            spans.push(Span::styled(
                "chat to refine the plan, or press Enter to approve & launch",
                muted_style(focused),
            ));
        } else {
            let cursor = cursor_col.min(chars.len());
            let before: String = chars[..cursor].iter().collect();
            if !before.is_empty() {
                spans.push(Span::styled(before, pane_text_style(focused)));
            }
            if cursor < chars.len() {
                spans.push(Span::styled(chars[cursor].to_string(), cursor_style));
                let after: String = chars[cursor + 1..].iter().collect();
                if !after.is_empty() {
                    spans.push(Span::styled(after, pane_text_style(focused)));
                }
            } else {
                // Cursor at end of the draft: a trailing block.
                spans.push(Span::styled(" ", cursor_style));
            }
        }
        vec![Line::default(), Line::from(spans)]
    } else {
        Vec::new()
    };

    match phase {
        OrchestratorPhase::Planning => {
            // Smooth live stream: the model's reasoning (dim), the files it inspects,
            // and the plan as it writes it. No pinned region; the whole transcript
            // scrolls and sticks to the bottom so new output stays in view.
            let mut body: Vec<Line<'static>> = Vec::new();
            // If the planner process has EXITED without a parseable DAG (it asked a
            // clarifying question or needs more detail), show a terminal PAUSED state with
            // a static badge instead of an animated spinner that would imply it is still
            // working forever. A typed message resumes the session (planner_awaiting_input).
            let paused = agent.status != AgentStatus::Running;
            if !paused {
                let spinner_label = if app.plan().rebasing {
                    "rebasing the plan…"
                } else if app.plan().refining {
                    "refining the plan…"
                } else {
                    "decomposing the task…"
                };
                body.push(Line::from(vec![
                    Span::styled(app.spinner_glyph(), Style::default().fg(ACCENT)),
                    Span::raw(" "),
                    Span::styled(spinner_label, pane_text_style(true)),
                ]));
                body.push(Line::default());
            }
            // The planner's reasoning + the files it inspected, then (when paused) the
            // prose that leads into the questions. The RUDDER_QUESTIONS block is stripped
            // here because it is re-rendered as a clean numbered prompt just below, so the
            // reading order is: inspection → "what shapes the build" → the questions.
            let empty: &[PlanEntry] = &[];
            let transcript = agent
                .plan_stream
                .as_ref()
                .map(|stream| stream.transcript())
                .unwrap_or(empty);
            if transcript.is_empty() {
                body.push(Line::from(Span::styled(
                    "starting the planner…",
                    muted_style(focused),
                )));
            } else {
                // Only strip the questions block when we actually re-render it as a
                // separate prompt below; otherwise (no parsed questions) leave it in the
                // transcript so the "its question is above" pointer is truthful.
                let strip_questions = paused && !app.plan().pending_questions.is_empty();
                push_transcript_lines(&mut body, transcript, focused, strip_questions);
            }
            if paused {
                // Render the question/answer step as a bordered "plan mode" card with the
                // free-text answer field pulled INSIDE the box (native-plan-mode feel),
                // anchored at the BOTTOM of the body so it stays in view (the body sticks to
                // the bottom) instead of scrolling off above a long transcript. The answer is
                // `worker_input_draft`; Enter submits it (refine), Esc clears it.
                body.push(Line::default());
                if app.plan().pending_questions.is_empty() {
                    body.push(Line::from(Span::styled(
                        "❓ The planner needs your input",
                        Style::default().fg(ACCENT).add_modifier(Modifier::BOLD),
                    )));
                    body.push(Line::from(Span::styled(
                        "Its question is at the end of the transcript above.",
                        pane_text_style(focused),
                    )));
                } else {
                    push_question_card(
                        &mut body,
                        &app.plan().pending_questions,
                        &agent.worker_input_draft,
                        agent.worker_input_cursor,
                        inner.width as usize,
                        focused,
                    );
                }
            }
            // While paused, the answer is typed into the card's own field, so suppress the
            // separate chat-input region to avoid a duplicated cursor below the box.
            let input_region: &[Line<'static>] = if paused { &[] } else { &input };
            let follow_bottom = app.orch_follow_bottom;
            let max_scroll = render_orchestrator_layout(
                frame,
                inner,
                &header,
                &[],
                &body,
                input_region,
                &mut app.orch_dag_scroll,
                follow_bottom,
            );
            app.orch_dag_max_scroll = max_scroll;
        }
        OrchestratorPhase::PlanReady(mut tasks) => {
            // Fold in nodes the user RECONCILED into the plan after launch. They live
            // only in `planned_nodes` (never in the orchestrator's frozen plan block),
            // so without this they are invisible in the orchestrator DAG. The initial
            // nodes share ids with `tasks`, so only post-launch additions get appended
            // here, and the user sees a typed task show up in the DAG.
            for node in &app.plan().planned_nodes {
                if !tasks.iter().any(|task| task.id == node.id) {
                    tasks.push(node.to_task());
                }
            }
            // PINNED top region: the DAG tree + a live status line. The tree nests
            // by HARD edges only (see orchestrator_hard_tree) so parallel soft-linked
            // tasks render as top-level roots instead of a misleading chain.
            let mut dag: Vec<Line<'static>> = orchestrator_dag_tree_lines(app, &tasks);
            let (mut running, mut review, mut done, mut todo, mut failed) =
                (0usize, 0usize, 0usize, 0usize, 0usize);
            for task in &tasks {
                match orchestrator_task_status(app, &task.id) {
                    OrchTaskStatus::Running => running += 1,
                    OrchTaskStatus::Review => review += 1,
                    OrchTaskStatus::Done => done += 1,
                    OrchTaskStatus::Failed => failed += 1,
                    OrchTaskStatus::Todo => todo += 1,
                }
            }
            // Surface failed tasks too (they were silently dropped from the tally, hiding
            // that a node failed and is blocking its dependents). Only shown when > 0.
            let failed_suffix = if failed > 0 {
                format!(" · failed {failed}")
            } else {
                String::new()
            };
            dag.push(Line::from(Span::styled(
                format!("running {running} · review {review} · done {done} · todo {todo}{failed_suffix}"),
                muted_style(focused),
            )));
            if app.plan().awaiting_approval {
                dag.push(Line::from(Span::styled(
                    "type below to refine  ·  Enter to approve & launch",
                    Style::default().fg(ACCENT),
                )));
            }

            // SCROLLABLE body: the human summary, THEN a per-node breakdown. Showing
            // each task's goal, done-when, and dependencies surfaces the plan's full
            // depth and fills the pane instead of leaving it blank below a short
            // summary.
            let mut prose: Vec<Line<'static>> = Vec::new();
            prose.push(Line::from(Span::styled("Plan", header_style(focused))));
            // Read the cached LIVE summary rather than only the frozen app.plan().plan_summary
            // captured at exit-detection: final bytes can arrive a tick or two after the
            // process is marked done. The cache refreshes on semantic stream changes, so
            // the prose self-heals without reparsing the whole plan on every redraw.
            let summary = rudder_plan_summary_for_run(agent)
                .or_else(|| app.plan().plan_summary.clone())
                .map(|s| s.trim().to_string())
                .filter(|s| !s.is_empty());
            if let Some(summary) = summary {
                for raw in summary.lines() {
                    prose.push(Line::from(Span::styled(
                        raw.trim_end().to_string(),
                        pane_text_style(focused),
                    )));
                }
            }

            // Per-node breakdown: title (+ dependency note), goal, and done-when for
            // every task, so the plan reads in full without launching anything.
            prose.push(Line::default());
            prose.push(Line::from(Span::styled("Tasks", header_style(focused))));
            for task in &tasks {
                let hard: Vec<&str> = task
                    .deps
                    .iter()
                    .filter(|edge| edge.edge == EdgeType::Hard)
                    .map(|edge| edge.on.as_str())
                    .collect();
                let soft: Vec<&str> = task
                    .deps
                    .iter()
                    .filter(|edge| edge.edge == EdgeType::Soft)
                    .map(|edge| edge.on.as_str())
                    .collect();
                let note = if hard.is_empty() && soft.is_empty() {
                    "  ·  parallel".to_string()
                } else {
                    let mut parts = String::new();
                    if !hard.is_empty() {
                        parts.push_str(&format!("  ·  after {}", hard.join(", ")));
                    }
                    if !soft.is_empty() {
                        parts.push_str(&format!("  ·  with {}", soft.join(", ")));
                    }
                    parts
                };
                prose.push(Line::default());
                prose.push(Line::from(vec![
                    Span::styled(format!("{}  ", task.id), accent_style(focused)),
                    Span::styled(task.title.clone(), pane_text_style(focused)),
                    Span::styled(note, muted_style(focused)),
                ]));
                if let Some(goal) = task
                    .goal
                    .as_deref()
                    .map(str::trim)
                    .filter(|g| !g.is_empty())
                {
                    prose.push(Line::from(Span::styled(
                        format!("    goal: {goal}"),
                        muted_style(focused),
                    )));
                }
                if let Some(success) = task
                    .success
                    .as_deref()
                    .map(str::trim)
                    .filter(|s| !s.is_empty())
                {
                    prose.push(Line::from(Span::styled(
                        format!("    done when: {success}"),
                        muted_style(focused),
                    )));
                }
            }

            // Activity: the conductor's autonomous actions (auto-expand, steering),
            // so the user always sees what the brain did without a confirm gate.
            if !app.activity_log.is_empty() {
                prose.push(Line::default());
                prose.push(Line::from(Span::styled("Activity", header_style(focused))));
                let start = app.activity_log.len().saturating_sub(8);
                for entry in &app.activity_log[start..] {
                    prose.push(Line::from(Span::styled(
                        format!("· {entry}"),
                        muted_style(focused),
                    )));
                }
            }
            let max_scroll = render_orchestrator_layout(
                frame,
                inner,
                &header,
                &dag,
                &prose,
                &input,
                &mut app.orch_dag_scroll,
                false,
            );
            app.orch_dag_max_scroll = max_scroll;
        }
    }

    capture_orchestrator_visible_rows(frame, inner, app);
}

fn capture_orchestrator_visible_rows(frame: &mut Frame<'_>, inner: Rect, app: &mut App) {
    // Read the pane back from the rendered buffer as plain-text rows (post wrap +
    // scroll) so a mouse drag can select and copy exactly what is on screen — the
    // pane composes Lines, not the PTY, so this is the only faithful source for the
    // selection machinery. Then paint the active selection by reversing its cells.
    let selection = app.orch_selection.map(normalize_selection);
    let buf = frame.buffer_mut();
    let mut rows: Vec<String> = Vec::with_capacity(inner.height as usize);
    for (row_idx, y) in (inner.y..inner.bottom()).enumerate() {
        let sel_cols = selection_for_row(selection, row_idx);
        let mut text = String::new();
        for (col_idx, x) in (inner.x..inner.right()).enumerate() {
            let symbol = buf
                .cell((x, y))
                .map(|cell| cell.symbol().to_string())
                .unwrap_or_else(|| " ".to_string());
            text.push_str(&symbol);
            if sel_cols.is_some_and(|(start, end)| col_idx >= start && col_idx <= end) {
                if let Some(cell) = buf.cell_mut((x, y)) {
                    cell.set_style(Style::default().fg(INK).bg(SURFACE_SEL));
                }
            }
        }
        rows.push(text);
    }
    app.orch_visible_rows = rows;
}

/// Lay out the orchestrator pane inside `inner` as: header (fixed) · top/DAG
/// (PINNED, capped) · body (SCROLLABLE) · input (pinned bottom). Only the body
/// scrolls (via `scroll`); the DAG stays on top. `follow_bottom` keeps the body
/// pinned to its last line until the user scrolls away.
#[allow(clippy::too_many_arguments)]
fn render_orchestrator_layout(
    frame: &mut Frame<'_>,
    inner: Rect,
    header: &[Line<'static>],
    top: &[Line<'static>],
    body: &[Line<'static>],
    input: &[Line<'static>],
    scroll: &mut usize,
    follow_bottom: bool,
) -> usize {
    let width = inner.width.max(1) as usize;
    let rows = |lines: &[Line<'static>]| -> u16 {
        lines
            .iter()
            .map(|line| wrapped_row_count(line.width().max(1), width))
            .sum::<usize>()
            .min(u16::MAX as usize) as u16
    };
    let header_h = rows(header);
    let input_h = rows(input);
    let avail = inner
        .height
        .saturating_sub(header_h)
        .saturating_sub(input_h);
    // Pin the DAG but never let it eat more than half the body area, so the prose
    // plan always has room; it shows the top of the tree if it is very tall.
    let top_h = if top.is_empty() {
        0
    } else {
        rows(top).min((avail / 2).max(3)).min(avail)
    };
    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(header_h),
            Constraint::Length(top_h),
            Constraint::Min(1),
            Constraint::Length(input_h),
        ])
        .split(inner);

    frame.render_widget(
        Paragraph::new(header.to_vec())
            .style(app_style())
            .wrap(Wrap { trim: false }),
        chunks[0],
    );
    if top_h > 0 {
        frame.render_widget(
            Paragraph::new(top.to_vec())
                .style(app_style())
                .wrap(Wrap { trim: false }),
            chunks[1],
        );
    }
    let body_area = chunks[2];
    let body_visible = body_area.height.max(1) as usize;
    let body_rows: usize = body
        .iter()
        .map(|line| wrapped_row_count(line.width().max(1), width))
        .sum();
    let max_scroll = body_rows.saturating_sub(body_visible);
    let effective = orchestrator_scroll_offset(scroll, max_scroll, follow_bottom);
    frame.render_widget(
        Paragraph::new(body.to_vec())
            .style(app_style())
            .scroll((effective as u16, 0))
            .wrap(Wrap { trim: false }),
        body_area,
    );
    if input_h > 0 {
        frame.render_widget(
            Paragraph::new(input.to_vec())
                .style(app_style())
                .wrap(Wrap { trim: false }),
            chunks[3],
        );
    }
    max_scroll
}

/// A blank interior row of the plan-mode card: `│` + spaces + `│`, exactly `box_w` wide.
fn card_blank(box_w: usize, border: Style) -> Line<'static> {
    Line::from(vec![
        Span::styled("│", border),
        Span::raw(" ".repeat(box_w.saturating_sub(2))),
        Span::styled("│", border),
    ])
}

/// A top (`╭─ … ─╮`) or bottom (`╰─ … ─╯`) border row with an embedded label,
/// exactly `box_w` wide. The label is truncated with `…` if it would not fit.
fn card_border(
    top: bool,
    label: &str,
    box_w: usize,
    border: Style,
    label_style: Style,
) -> Line<'static> {
    let (lead, corner) = if top {
        ("╭─ ", "╮")
    } else {
        ("╰─ ", "╯")
    };
    // Reserve: lead(3) + label + trailing space(1) + dashes(>=1) + corner(1).
    let max_label = box_w.saturating_sub(6);
    let label_fit: String = if label.chars().count() > max_label {
        format!(
            "{}…",
            label
                .chars()
                .take(max_label.saturating_sub(1))
                .collect::<String>()
        )
    } else {
        label.to_string()
    };
    let used = 3 + label_fit.chars().count() + 1; // lead + label + trailing space
    let dashes = box_w.saturating_sub(used + 1); // +1 for the corner
    Line::from(vec![
        Span::styled(lead, border),
        Span::styled(label_fit, label_style),
        Span::styled(" ", border),
        Span::styled("─".repeat(dashes), border),
        Span::styled(corner, border),
    ])
}

/// Render the planner's clarifying questions as a bordered "plan mode" card with the
/// free-text answer field pulled INSIDE the box, for a native-plan-mode feel. `draft`
/// + `cursor` are the live `worker_input_draft`/`worker_input_cursor`: Enter submits
/// the answer (refine), Esc clears it. All width math is char-count based, which
/// matches how ratatui renders this (ASCII-ish) content, so the borders stay aligned.
pub(crate) fn push_question_card(
    body: &mut Vec<Line<'static>>,
    questions: &[String],
    draft: &str,
    cursor: usize,
    width: usize,
    focused: bool,
) {
    let border = Style::default().fg(ACCENT);
    let accent = Style::default().fg(ACCENT);
    let text = pane_text_style(focused);
    let box_w = width.max(1);

    // Narrow-pane fallback: a plain numbered list (a box would wrap and break).
    if box_w < 30 {
        body.push(Line::from(Span::styled(
            "❓ The planner needs your input",
            accent.add_modifier(Modifier::BOLD),
        )));
        for (i, q) in questions.iter().enumerate() {
            body.push(Line::from(vec![
                Span::styled(format!("  {}. ", i + 1), accent),
                Span::styled(q.clone(), text),
            ]));
        }
        body.push(Line::from(Span::styled(
            "↳ type your answer below and press Enter",
            muted_style(focused),
        )));
        return;
    }

    let content_w = box_w - 4; // "│ " (2) + content + " │" (2)

    body.push(card_border(
        true,
        "◆ Plan mode · a few quick questions",
        box_w,
        border,
        accent.add_modifier(Modifier::BOLD),
    ));
    body.push(card_blank(box_w, border));

    // Numbered, wrapped questions. The number label is a fixed 3-col gutter so wrapped
    // continuation lines hang-indent under the text.
    let label_w = 3usize;
    let q_avail = content_w.saturating_sub(label_w);
    for (i, q) in questions.iter().enumerate() {
        for (li, seg) in wrap_text(q, q_avail as u16).iter().enumerate() {
            let label = if li == 0 {
                format!("{:<width$}", i + 1, width = label_w)
            } else {
                " ".repeat(label_w)
            };
            let pad = content_w.saturating_sub(label_w + seg.chars().count());
            body.push(Line::from(vec![
                Span::styled("│ ", border),
                Span::styled(label, accent),
                Span::styled(seg.clone(), text),
                Span::raw(" ".repeat(pad)),
                Span::styled(" │", border),
            ]));
        }
    }

    body.push(card_blank(box_w, border));

    // The answer field, inside the box: "› " + the live draft with a block cursor.
    let inner_avail = content_w.saturating_sub(2); // after the "› " prompt
    let cursor_style = text.add_modifier(Modifier::REVERSED);
    let mut spans: Vec<Span<'static>> =
        vec![Span::styled("│ ", border), Span::styled("› ", accent)];
    let mut used = 2usize; // "› "
    let chars: Vec<char> = draft.chars().collect();
    if chars.is_empty() {
        if focused {
            spans.push(Span::styled(" ", cursor_style));
            used += 1;
            let ph: String = "type your answer, e.g. 1: reuse, 2: mock"
                .chars()
                .take(inner_avail.saturating_sub(1))
                .collect();
            used += ph.chars().count();
            spans.push(Span::styled(ph, muted_style(focused)));
        } else {
            let ph: String = "type to answer".chars().take(inner_avail).collect();
            used += ph.chars().count();
            spans.push(Span::styled(ph, muted_style(focused)));
        }
    } else if !focused {
        let shown: String = chars.iter().take(inner_avail).collect();
        used += shown.chars().count();
        spans.push(Span::styled(shown, text));
    } else if chars.len() + 1 <= inner_avail {
        // Whole draft fits: render before / cursor cell / after so mid-line editing shows.
        let cur = cursor.min(chars.len());
        let before: String = chars[..cur].iter().collect();
        let after: String = if cur < chars.len() {
            chars[cur + 1..].iter().collect()
        } else {
            String::new()
        };
        used += before.chars().count();
        spans.push(Span::styled(before, text));
        let cell = chars
            .get(cur)
            .map(|c| c.to_string())
            .unwrap_or_else(|| " ".to_string());
        used += 1;
        spans.push(Span::styled(cell, cursor_style));
        used += after.chars().count();
        spans.push(Span::styled(after, text));
    } else {
        // Overflow: tail-anchor with a leading ellipsis and an end cursor.
        let tail: String = chars
            .iter()
            .skip(chars.len().saturating_sub(inner_avail.saturating_sub(2)))
            .collect();
        used += 1;
        spans.push(Span::styled("…".to_string(), muted_style(focused)));
        used += tail.chars().count();
        spans.push(Span::styled(tail, text));
        used += 1;
        spans.push(Span::styled(" ", cursor_style));
    }
    let pad = content_w.saturating_sub(used);
    spans.push(Span::raw(" ".repeat(pad)));
    spans.push(Span::styled(" │", border));
    body.push(Line::from(spans));

    body.push(card_border(
        false,
        "↵ continue  ·  esc clear  ·  or just describe what you want",
        box_w,
        border,
        muted_style(focused),
    ));
}

/// Render a planner transcript (reasoning/tool/text/user turns) into `out`, with
/// dim italic thinking, muted tool/system lines, and accented user turns.
pub(crate) fn push_transcript_lines(
    out: &mut Vec<Line<'static>>,
    transcript: &[PlanEntry],
    focused: bool,
    strip_questions: bool,
) {
    // When the planner has paused for input, the RUDDER_QUESTIONS block is rendered
    // separately as a clean numbered prompt, so showing it again here (raw markers
    // and all) is noisy duplication. Skip the block — including its START/END marker
    // lines — wherever it appears in the streamed text. `skipping` persists across
    // entries because streamed deltas can split the block over several Text entries.
    let mut skipping = false;
    for entry in transcript {
        let (prefix, style) = match entry.kind {
            PlanEntryKind::Thinking => ("· ", muted_style(focused).add_modifier(Modifier::ITALIC)),
            PlanEntryKind::Tool => ("  ", muted_style(focused)),
            PlanEntryKind::System => ("", muted_style(focused)),
            PlanEntryKind::UserTurn => ("you: ", Style::default().fg(ACCENT)),
            PlanEntryKind::Text => ("", pane_text_style(focused)),
        };
        let mut first = true;
        for sub in entry.text.split('\n') {
            if strip_questions {
                let marker = sub.trim();
                if marker == "RUDDER_QUESTIONS_START" {
                    skipping = true;
                    first = false;
                    continue;
                }
                if marker == "RUDDER_QUESTIONS_END" {
                    skipping = false;
                    first = false;
                    continue;
                }
                if skipping {
                    first = false;
                    continue;
                }
            }
            let content = if first {
                format!("{prefix}{}", sub.trim_end())
            } else {
                sub.trim_end().to_string()
            };
            first = false;
            if content.is_empty() {
                continue;
            }
            out.push(Line::from(Span::styled(content, style)));
        }
    }
}

/// Render the INTERACTIVE orchestrator (opt-in): a live DAG pane on top (from the plan
/// the orchestrator wrote to its plan file → `planned_nodes`) and the raw interactive
/// backend PTY below, which the user converses with directly.
pub(crate) fn interactive_orchestrator_areas(area: Rect) -> (Rect, Rect) {
    let dag_h = (area.height / 3).clamp(6, area.height.saturating_sub(6).max(6));
    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([Constraint::Length(dag_h), Constraint::Min(1)])
        .split(area);
    (chunks[0], chunks[1])
}

pub(crate) fn render_interactive_orchestrator(frame: &mut Frame<'_>, area: Rect, app: &mut App) {
    let focused = app.focus == FocusPane::Worker;
    // DAG pane on top (capped to a third, leaving room for the conversation), PTY below.
    let (dag_area, term_area) = interactive_orchestrator_areas(area);
    let swarm = app
        .agents
        .get(app.selected_agent)
        .is_some_and(|run| app.run_is_in_swarm(run));

    // ---- DAG pane (top) ----
    // Render the WHOLE plan (queued + already-launched nodes), so the tree stays
    // intact after approval drains the queue into running workers. orchestrator_dag_tasks
    // reconstructs launched nodes from their agents; status badges come from live runs.
    let mut dag_lines: Vec<Line<'static>> = Vec::new();
    let tasks = app.orchestrator_dag_tasks();
    if tasks.is_empty() && swarm {
        dag_lines.push(Line::from(Span::styled(
            "Designing experiments… talk to the swarm below.",
            muted_style(focused),
        )));
        dag_lines.push(Line::from(Span::styled(
            "It writes the experiment list to RUDDER.md; the list shows here.",
            muted_style(focused),
        )));
    } else if tasks.is_empty() {
        dag_lines.push(Line::from(Span::styled(
            "Planning… talk to the orchestrator below.",
            muted_style(focused),
        )));
        dag_lines.push(Line::from(Span::styled(
            "It writes the task DAG to RUDDER.md; the DAG shows here.",
            muted_style(focused),
        )));
    } else {
        dag_lines = orchestrator_dag_tree_lines(app, &tasks);
        if app.plan().awaiting_approval && swarm {
            dag_lines.push(Line::from(Span::styled(
                "awaiting approval: press empty Enter in the task pane to launch the experiments",
                Style::default().fg(ACCENT),
            )));
        } else if app.plan().awaiting_approval {
            dag_lines.push(Line::from(Span::styled(
                "awaiting approval — press empty Enter in the task pane to launch",
                Style::default().fg(ACCENT),
            )));
        }
    }
    let dag_inner = block_inner(dag_area);
    let dag_rows: usize = dag_lines
        .iter()
        .map(|line| wrapped_row_count(line.width().max(1), dag_inner.width.max(1) as usize))
        .sum();
    let max_scroll = orchestrator_dag_max_scroll(dag_rows, dag_inner.height.max(1) as usize);
    let scroll = orchestrator_scroll_offset(&mut app.orch_dag_scroll, max_scroll, false);
    app.orch_dag_max_scroll = max_scroll;
    frame.render_widget(
        Paragraph::new(dag_lines)
            .style(app_style())
            .block(pane_block(if swarm { "experiments" } else { "dag" }, focused, app.nav_mode))
            .scroll((scroll as u16, 0))
            .wrap(Wrap { trim: false }),
        dag_area,
    );
    capture_orchestrator_visible_rows(frame, dag_inner, app);

    // ---- interactive PTY (bottom) ----
    let term_inner = block_inner(term_area);
    // Once the plan is approved, the orchestrator is Stopped (its PTY was killed): it
    // has handed off to the worker fleet and must never implement itself. Show a clear
    // hand-off banner instead of the dead terminal so the pane explains what happened
    // and points at the work above.
    let orchestrator_stopped = app
        .agents
        .get(app.selected_agent)
        .is_some_and(|run| run.is_orchestrator() && run.status == AgentStatus::Stopped);
    if orchestrator_stopped {
        let working = app
            .agents
            .iter()
            .filter(|run| run.node_id.is_some() && run.status == AgentStatus::Running)
            .count();
        let banner = vec![
            Line::from(""),
            Line::from(Span::styled(
                "Plan approved. Orchestrator stopped.",
                pane_text_style(focused),
            )),
            Line::from(Span::styled(
                format!("{working} worker(s) implementing the DAG above. Press ↑/↓ to watch them."),
                muted_style(focused),
            )),
        ];
        frame.render_widget(
            Paragraph::new(banner)
                .style(app_style())
                .block(pane_block("orchestrator · stopped", focused, app.nav_mode))
                .wrap(Wrap { trim: false }),
            term_area,
        );
        return;
    }
    let fit = app
        .transition_slot
        .map_or(term_inner, |slot| block_inner(interactive_orchestrator_areas(slot).1));
    if let Ok(size) = TerminalSize::new(fit.height.max(1), fit.width.max(1)) {
        app.fit_pane(app.selected_agent, size);
    }
    let lines = worker_lines(app, term_inner.height as usize, term_inner.width as usize);
    frame.render_widget(
        Paragraph::new(lines)
            .style(app_style())
            .block(pane_block("orchestrator", focused, app.nav_mode))
            .wrap(Wrap { trim: false }),
        term_area,
    );
    if focused {
        set_worker_cursor(frame, term_inner, app);
    }
}

/// Split for the finished-worker card view: card (objective + what-it-did) on
/// top, conversation below. Same shape as the interactive orchestrator's split so
/// the mouse handlers can derive the sub-areas from the pane rect alone.
pub(crate) fn done_card_areas(area: Rect) -> (Rect, Rect) {
    let card_h = (area.height / 3).clamp(6, area.height.saturating_sub(6).max(6));
    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([Constraint::Length(card_h), Constraint::Min(1)])
        .split(area);
    (chunks[0], chunks[1])
}

/// The finished-worker card content: status + title, the objective, and the
/// worker's own completion summary ("what it did"). Pure so tests can assert it.
pub(crate) fn done_worker_card_lines(run: &AgentRun, focused: bool) -> Vec<Line<'static>> {
    let title = if run.task_summary.trim().is_empty() {
        summarize_task(&run.task)
    } else {
        run.task_summary.clone()
    };
    let mut lines = vec![
        Line::from(Span::styled(
            format!("{} · {}", run.status.as_str(), title),
            pane_text_style(focused),
        )),
        Line::from(""),
        Line::from(Span::styled("Objective", muted_style(focused))),
    ];
    for text_line in run.task.lines().take(12) {
        lines.push(Line::from(Span::styled(
            text_line.to_string(),
            pane_text_style(focused),
        )));
    }
    lines.push(Line::from(""));
    lines.push(Line::from(Span::styled(
        "What it did",
        muted_style(focused),
    )));
    match run.done_summary.as_deref() {
        Some(summary) => {
            for text_line in summary.lines() {
                lines.push(Line::from(Span::styled(
                    text_line.to_string(),
                    pane_text_style(focused),
                )));
            }
        }
        None => lines.push(Line::from(Span::styled(
            "(no completion summary was recorded)",
            muted_style(focused),
        ))),
    }
    lines
}

/// Two-panel view for a FINISHED worker, mirroring the interactive orchestrator:
/// the card on top (objective + what it did), the conversation below. The session
/// stays conversable — typing below resumes it and the card refreshes when the
/// resumed turn completes.
/// One line per finished run in a drawer: node label (or `·` when it has none), the
/// task summary, and its diff size when the run recorded one. Pure so tests can
/// assert the list without a terminal.
pub(crate) fn drawer_list_lines(
    agents: &[AgentRun],
    members: &[usize],
    selected: usize,
    focused: bool,
    width: usize,
) -> Vec<Line<'static>> {
    members
        .iter()
        .enumerate()
        .filter_map(|(position, &index)| {
            let run = agents.get(index)?;
            let is_selected = position == selected;
            let label = run.node_id.clone().unwrap_or_else(|| "·".to_string());
            let title = if run.task_summary.trim().is_empty() {
                summarize_task(&run.task)
            } else {
                run.task_summary.clone()
            };
            // The label column is fixed so the titles line up into a readable column.
            let label_width = 4usize;
            // marker(2) + badge(2) + label + gap(2) + status label.
            let status_text = agent_status_text(run);
            let text_width = width
                .saturating_sub(label_width + 6 + status_text.chars().count())
                .max(8);
            Some(Line::from(vec![
                Span::styled(
                    if is_selected { "▶ " } else { "  " },
                    if is_selected {
                        accent_style(focused)
                    } else {
                        muted_style(focused)
                    },
                ),
                // The status badge keeps state legible at a glance inside the drawer,
                // the way it is on a sidebar row: merged green, failed red. Collapsing
                // the section must not cost the color coding.
                Span::styled(BADGE, status_style(run.status)),
                Span::raw(" "),
                Span::styled(
                    format!("{label:<label_width$}"),
                    if is_selected {
                        accent_style(focused)
                    } else {
                        muted_style(focused)
                    },
                ),
                Span::styled(truncate_chars(&title, text_width), pane_text_style(focused)),
                Span::raw("  "),
                Span::styled(status_text, status_style(run.status)),
            ]))
        })
        .collect()
}

/// The drawer view: the finished runs on top, the highlighted run's result card
/// below. Mirrors `render_done_worker_card`'s split so the two read the same, but the
/// top half is the LIST rather than one run's card.
pub(crate) fn render_drawer(frame: &mut Frame<'_>, area: Rect, app: &mut App) {
    let focused = app.focus == FocusPane::Worker || app.focus == FocusPane::Agents;
    let Some(bucket) = app.drawer_open() else {
        return;
    };
    let members = drawer_members(&app.agents, bucket);
    let selected = app.drawer_selection().min(members.len().saturating_sub(1));
    let (list_area, detail_area) = done_card_areas(area);

    let list_inner = block_inner(list_area);
    let mut lines = drawer_list_lines(
        &app.agents,
        &members,
        selected,
        focused,
        list_inner.width as usize,
    );
    // Keep the highlighted row on screen when the drawer holds more runs than fit.
    let view_h = list_inner.height as usize;
    if view_h > 0 && lines.len() > view_h {
        let start = selected.saturating_sub(view_h.saturating_sub(1));
        lines = lines.into_iter().skip(start).take(view_h).collect();
    }
    frame.render_widget(
        Paragraph::new(lines).style(app_style()).block(pane_block(
            &format!("{} · {} · enter reopens · esc back", bucket.label(), members.len()),
            focused,
            app.nav_mode,
        )),
        list_area,
    );

    let (title, detail) = match members
        .get(selected)
        .and_then(|&index| app.agents.get(index))
    {
        Some(run) => {
            let label = run.node_id.clone().unwrap_or_else(|| "run".to_string());
            let summary = if run.task_summary.trim().is_empty() {
                summarize_task(&run.task)
            } else {
                run.task_summary.clone()
            };
            (
                format!("{label} · {summary}"),
                done_worker_card_lines(run, focused),
            )
        }
        None => (
            format!("{} · nothing here", bucket.label()),
            vec![Line::from(Span::styled(
                "This drawer is empty.",
                muted_style(focused),
            ))],
        ),
    };
    frame.render_widget(
        Paragraph::new(detail)
            .style(app_style())
            .block(pane_block(&title, focused, app.nav_mode))
            .wrap(Wrap { trim: false }),
        detail_area,
    );
}

pub(crate) fn render_worker(frame: &mut Frame<'_>, area: Rect, app: &mut App) {
    let inner = block_inner(area);
    // Mid-move (⌥h) the pane is drawn bigger than the slot it is laid out for;
    // the agent keeps the slot's size until the move lands.
    let fit = block_inner(app.transition_slot.unwrap_or(area));
    let terminal_size = TerminalSize::new(fit.height.max(1), fit.width.max(1)).ok();
    let focused = app.focus == FocusPane::Worker;

    // The selected orchestrator gets the custom DAG command-center view ONLY once
    // its plan is ready. While it is still planning, show its raw PTY so the live
    // plan-mode session (clarifying questions, selection menus, approval) renders
    // cleanly through the terminal emulator and fills the pane. A custom extracted
    // -lines view mangled spacing, leaked OSC escape sequences, and broke menu
    // navigation. The diff/review view always uses the normal terminal path.
    let selected_is_orchestrator = app
        .agents
        .get(app.selected_agent)
        .is_some_and(|run| run.is_orchestrator());
    if app.worker_view == WorkerView::PlanReview {
        app.ensure_plan_review_state();
    }
    // The orchestrator ALWAYS gets the custom command-center view: a live streaming
    // transcript while planning (its raw PTY is now JSON events, not human text), and
    // the DAG tree once a plan is ready. Both phases are rendered by render_orchestrator.
    if app.worker_view == WorkerView::Terminal && selected_is_orchestrator {
        let selected_is_interactive_orchestrator = app
            .agents
            .get(app.selected_agent)
            .is_some_and(|run| app.is_interactive_orchestrator_run(run));
        if selected_is_interactive_orchestrator {
            // Opt-in: the orchestrator is a normal interactive backend PTY the user talks
            // to, with the DAG it wrote to its plan file rendered in a pane ABOVE it.
            render_interactive_orchestrator(frame, area, app);
        } else {
            render_orchestrator(frame, area, app);
        }
        return;
    }

    // A finished worker renders as its plain conversation. It used to get a
    // two-panel card (objective + what-it-did on top, conversation below), but the
    // card restated what the transcript already said and took half the pane to do
    // it. The same summary is still one keypress away in the done drawer.

    // The /gam split: a paired generator + adversarial reviewer render as two
    // live terminals side by side (generator left, reviewer right).
    if app.worker_view == WorkerView::Terminal && app.gam_pair_indices_for_selected().is_some() {
        render_gam_split(frame, area, app);
        return;
    }

    if let Some(size) = terminal_size {
        if app.worker_view == WorkerView::Terminal {
            app.fit_pane(app.selected_agent, size);
        }
    }

    let lines = match app.worker_view {
        WorkerView::Terminal => worker_lines(app, inner.height as usize, inner.width as usize),
        WorkerView::PlanReview => {
            plan_review_lines(app, inner.height as usize, inner.width as usize)
        }
    };
    // Identify WHICH agent this pane is showing: its id (the node id for plan
    // workers, matching the n0/n1 ids in the orchestrator DAG) plus a short task
    // label. Without this the pane just said "worker" and the user had to infer
    // the mapping from the agents-pane highlight.
    // The title is what runs in the pane: backend, model and the run's key.
    // The task itself is on the selected card; only when the card list is
    // hidden (narrow or solo) does the title repeat it.
    let sidebar_visible = app.agents_area.is_some();
    let identity = app.agents.get(app.selected_agent).map(|run| {
        let id = worker_title_id(app, run);
        let model = if run.model.trim().is_empty() {
            run.backend.as_str().to_string()
        } else {
            format!(
                "{} {}({})",
                run.backend.as_str(),
                run.model,
                effort_label(run.effort)
            )
        };
        if sidebar_visible {
            format!("{model} · {id}")
        } else {
            let label = if run.task_summary.trim().is_empty() {
                summarize_task(&run.task)
            } else {
                run.task_summary.clone()
            };
            format!("{} · {model} · {id}", truncate_chars(&label, 36))
        }
    });
    let title = match app.worker_view {
        WorkerView::Terminal => match &identity {
            Some(identity) if !selected_is_orchestrator => identity.clone(),
            _ if selected_is_orchestrator => "orchestrator".to_string(),
            _ => "terminal".to_string(),
        },
        WorkerView::PlanReview => format!(
            "plan review · {} · Ctrl+S save · Ctrl+Enter approve · Esc hide",
            app.plan().plan_review.field.label()
        ),
    };
    let paragraph = Paragraph::new(lines)
        .style(app_style())
        .block(pane_block(&title, focused, app.nav_mode))
        .wrap(Wrap { trim: false });

    frame.render_widget(paragraph, area);

    if focused {
        match app.worker_view {
            WorkerView::Terminal => set_worker_cursor(frame, inner, app),
            WorkerView::PlanReview => set_plan_review_cursor(frame, inner, app),
        }
    }
}

fn worker_title_id(app: &App, run: &AgentRun) -> String {
    if let Some(node_id) = run.node_id.as_ref().filter(|id| !id.trim().is_empty()) {
        return node_id.clone();
    }
    if run.is_oneoff() {
        return format!("q{}", oneoff_display_index(app, &run.id).unwrap_or(1));
    }
    if run.is_main() {
        return "main".to_string();
    }
    // The whole id repeats the task words the title already shows, so only its
    // key (`k3f9q`), the same one the PR branch and Remote Control name carry.
    run_short_key(&run.id)
}

fn oneoff_display_index(app: &App, run_id: &str) -> Option<usize> {
    app.agents
        .iter()
        .filter(|agent| agent.is_oneoff())
        .position(|agent| agent.id == run_id)
        .map(|index| index + 1)
}

pub(crate) fn plan_review_lines(app: &mut App, height: usize, width: usize) -> Vec<Line<'static>> {
    app.plan_mut().plan_review.cursor_row = None;
    app.plan_mut().plan_review.cursor_col = 0;
    let focused = app.focus == FocusPane::Worker;
    let mut lines: Vec<Line<'static>> = Vec::new();
    let content_width = width.max(1);
    if app.plan().plan_review.nodes.is_empty() {
        return vec![Line::from(Span::styled(
            "No approval-gated plan is available.",
            muted_style(focused),
        ))];
    }

    lines.push(Line::from(vec![
        Span::styled("Plan ready", accent_style(focused)),
        Span::styled("  edit fields inline before launch", muted_style(focused)),
    ]));
    lines.push(Line::from(vec![
        Span::styled("Tab", accent_style(focused)),
        Span::styled(" field  ", muted_style(focused)),
        Span::styled("\u{2191}/\u{2193}", accent_style(focused)),
        Span::styled(" task  ", muted_style(focused)),
        Span::styled("Ctrl+S", accent_style(focused)),
        Span::styled(" save  ", muted_style(focused)),
        Span::styled("Ctrl+Enter", accent_style(focused)),
        Span::styled(" approve  ", muted_style(focused)),
        Span::styled("just type", accent_style(focused)),
        Span::styled(" to refine with the planner", muted_style(focused)),
    ]));
    lines.push(Line::from(""));
    lines.push(Line::from(Span::styled("Tasks", muted_style(focused))));
    for (index, node) in app.plan().plan_review.nodes.iter().enumerate() {
        let selected = index == app.plan().plan_review.selected;
        let marker = if selected { "▶ " } else { "  " };
        let style = if selected {
            Style::default().fg(PAPER).bg(FOCUS_COLOR)
        } else {
            pane_text_style(focused)
        };
        let dep_count = plan_review_dep_count(node);
        let mut spans = vec![
            Span::styled(marker, accent_style(focused)),
            Span::styled(format!("{} ", node.id), accent_style(focused)),
            Span::styled(
                truncate_chars(&node.title, content_width.saturating_sub(12)),
                style,
            ),
        ];
        if dep_count > 0 {
            spans.push(Span::styled(
                format!("  deps {dep_count}"),
                muted_style(focused),
            ));
        }
        lines.push(Line::from(spans));
    }
    lines.push(Line::from(""));

    let selected = app.plan().plan_review.selected;
    if let Some(node) = app.plan().plan_review.nodes.get(selected).cloned() {
        lines.push(Line::from(vec![
            Span::styled(format!("Editing {} ", node.id), accent_style(focused)),
            Span::styled(
                format!("({}/{})", selected + 1, app.plan().plan_review.nodes.len()),
                muted_style(focused),
            ),
        ]));
        push_plan_review_field(
            app,
            &mut lines,
            PlanReviewField::Title,
            "title",
            &node.title,
            content_width,
            focused,
        );
        push_plan_review_field(
            app,
            &mut lines,
            PlanReviewField::Goal,
            "goal",
            &node.goal,
            content_width,
            focused,
        );
        push_plan_review_field(
            app,
            &mut lines,
            PlanReviewField::Success,
            "done",
            &node.success,
            content_width,
            focused,
        );
        push_plan_review_field(
            app,
            &mut lines,
            PlanReviewField::HardDeps,
            "hard deps",
            &node.hard_deps,
            content_width,
            focused,
        );
        push_plan_review_field(
            app,
            &mut lines,
            PlanReviewField::SoftDeps,
            "soft deps",
            &node.soft_deps,
            content_width,
            focused,
        );
        push_plan_review_field(
            app,
            &mut lines,
            PlanReviewField::Prompt,
            "prompt",
            &node.prompt,
            content_width,
            focused,
        );
    }

    if !app.plan().plan_review.errors.is_empty() {
        lines.push(Line::from(""));
        lines.push(Line::from(Span::styled("Errors", error_style())));
        for error in &app.plan().plan_review.errors {
            lines.push(Line::from(Span::styled(
                format!("  {error}"),
                error_style(),
            )));
        }
    }
    if !app.plan().plan_review.warnings.is_empty() {
        lines.push(Line::from(""));
        lines.push(Line::from(Span::styled(
            "Warnings",
            Style::default().fg(ACCENT_2),
        )));
        for warning in &app.plan().plan_review.warnings {
            lines.push(Line::from(Span::styled(
                format!("  {warning}"),
                Style::default().fg(ACCENT_2),
            )));
        }
    }

    let max_scroll = lines.len().saturating_sub(height.max(1));
    app.plan_mut().plan_review.scroll = app.plan().plan_review.scroll.min(max_scroll);
    lines
        .into_iter()
        .skip(app.plan().plan_review.scroll)
        .take(height.max(1))
        .collect()
}

fn plan_review_dep_count(node: &PlanReviewDraftNode) -> usize {
    App::parse_plan_review_deps(&node.hard_deps).len()
        + App::parse_plan_review_deps(&node.soft_deps).len()
}

fn push_plan_review_field(
    app: &mut App,
    lines: &mut Vec<Line<'static>>,
    field: PlanReviewField,
    label: &str,
    value: &str,
    width: usize,
    focused: bool,
) {
    let active = app.plan().plan_review.field == field;
    let label_text = format!("  {label}: ");
    let label_width = label_text.chars().count();
    let value_width = width.saturating_sub(label_width).max(1) as u16;
    let wrapped = wrap_input_text(value, value_width);
    if active {
        let (cursor_line, cursor_col) =
            task_cursor_position(value, app.plan().plan_review.cursor, value_width);
        app.plan_mut().plan_review.cursor_row = Some(lines.len() + cursor_line);
        app.plan_mut().plan_review.cursor_col = label_width + cursor_col;
    }
    let label_style = if active {
        accent_style(focused)
    } else {
        muted_style(focused)
    };
    let value_style = if active {
        Style::default().fg(INK).bg(SURFACE_SEL)
    } else {
        pane_text_style(focused)
    };
    for (index, line) in wrapped.iter().enumerate() {
        let prefix = if index == 0 {
            label_text.clone()
        } else {
            " ".repeat(label_width)
        };
        lines.push(Line::from(vec![
            Span::styled(prefix, label_style),
            Span::styled(line.clone(), value_style),
        ]));
    }
}

pub(crate) fn set_plan_review_cursor(frame: &mut Frame<'_>, inner: Rect, app: &App) {
    let Some(row) = app.plan().plan_review.cursor_row else {
        return;
    };
    if row < app.plan().plan_review.scroll {
        return;
    }
    let visible_row = row - app.plan().plan_review.scroll;
    if visible_row >= inner.height as usize {
        return;
    }
    let col = app
        .plan()
        .plan_review
        .cursor_col
        .min(inner.width.saturating_sub(1) as usize);
    frame.set_cursor_position((inner.x + col as u16, inner.y + visible_row as u16));
}

/// The ⌥d diff panel: header (stats, tokens, cost), file summary, hunks.
pub(crate) fn render_diff_panel(frame: &mut Frame<'_>, area: Rect, app: &mut App) {
    let focused = app.focus == FocusPane::Diff;
    let inner = block_inner(area);
    let title = {
        let files = app.diff_panel.files.len();
        if app.diff_panel.loaded && app.diff_panel.error.is_none() {
            format!(
                "diff · +{} −{} · {} file{}",
                app.diff_panel.additions(),
                app.diff_panel.deletions(),
                files,
                if files == 1 { "" } else { "s" }
            )
        } else {
            "diff".to_string()
        }
    };
    let viewport = inner.height as usize;
    // Mid-move (⌥h) the lines keep the width of the slot the panel moves from
    // or to, so they are not rewrapped on every frame.
    let wrap_width = app
        .transition_slot
        .map_or(inner.width, |slot| block_inner(slot).width.min(inner.width));
    let lines: Vec<Line<'static>> = match app.agents.get(app.selected_agent) {
        Some(run) => {
            let run_ref: &AgentRun = run;
            // Borrow dance: the panel caches by revision, the run is read-only.
            let (all, _) = app.diff_panel.lines(run_ref, wrap_width, focused);
            let all = all.to_vec();
            let max = all.len().saturating_sub(viewport);
            let scroll = app.diff_panel.scroll.min(max);
            app.diff_panel.scroll = scroll;
            all.into_iter().skip(scroll).take(viewport).collect()
        }
        None => vec![Line::from(Span::styled(
            "No agent selected.",
            muted_style(focused),
        ))],
    };
    let scrolled = app.diff_panel.scroll;
    let total = app.diff_panel.max_scroll(viewport) + viewport;
    let title = if scrolled > 0 {
        format!("{title} · {}%", (scrolled * 100) / total.max(1))
    } else {
        title
    };
    frame.render_widget(
        Paragraph::new(lines)
            .style(app_style())
            .block(pane_block(&title, focused, app.nav_mode)),
        area,
    );
}

pub(crate) fn set_worker_cursor(frame: &mut Frame<'_>, inner: Rect, app: &App) {
    let Some(run) = app.agents.get(app.selected_agent) else {
        return;
    };
    let Some(terminal) = run.terminal.as_ref() else {
        return;
    };
    if let Some(position) = worker_hardware_cursor(terminal, inner) {
        frame.set_cursor_position(position);
    }
}

/// Where the terminal's own cursor goes for a live agent pane, if anywhere.
///
/// The agent decides: Claude Code and Codex hide the cursor while they redraw
/// and show it again at the input caret. Showing it while hidden put it
/// wherever the agent happened to be drawing, so it jumped around and seemed
/// to highlight random text. Rudder sets the cursor shape (a thin bar) once
/// for the whole dashboard; panes never paint a cursor cell of their own.
pub(crate) fn worker_hardware_cursor(terminal: &TerminalPane, inner: Rect) -> Option<(u16, u16)> {
    if terminal.scrollback() > 0 {
        return None;
    }
    let cursor = terminal.cursor();
    if !cursor.visible || cursor.row >= inner.height || cursor.col >= inner.width {
        return None;
    }
    Some((inner.x + cursor.col, inner.y + cursor.row))
}

/// The selected run's `/gam` peer pair, ordered (generator, adversarial).
/// `None` unless the selected row participates in a pair AND its peer still
/// exists in the roster.
pub(crate) fn gam_split_halves(inner: Rect) -> (Rect, Rect) {
    let columns = ratatui::layout::Layout::horizontal([
        ratatui::layout::Constraint::Percentage(50),
        ratatui::layout::Constraint::Percentage(50),
    ])
    .split(inner);
    (columns[0], columns[1])
}

/// The one-row status bar over a `/gam` split, and what is left under it.
/// `None` on a pane too short to spare the row; the halves' own titles still
/// carry the per-side stage there.
pub(crate) fn gam_split_bar(
    area: Rect,
    app: &App,
    generator_index: usize,
) -> Option<(Rect, Rect)> {
    if area.height < 12 {
        return None;
    }
    // A settled pair's outcome and next step may take a second row rather than
    // be cut off: it is the one thing on the screen you need to act on.
    let rows = app
        .gam_peer_index_of(generator_index)
        .map(|peer| {
            gam_status_bar_lines(
                app,
                generator_index,
                peer,
                area.width.saturating_sub(2) as usize,
                2,
            )
            .len()
            .max(1) as u16
        })
        .unwrap_or(1);
    Some((
        Rect {
            height: rows,
            ..area
        },
        Rect {
            y: area.y + rows,
            height: area.height - rows,
            ..area
        },
    ))
}

/// The bar as up to `max_rows` lines: the one-row form, or for a settled
/// pair whose outcome does not fit, the outcome wrapped onto a second row.
pub(crate) fn gam_status_bar_lines(
    app: &App,
    generator_index: usize,
    adversarial_index: usize,
    width: usize,
    max_rows: usize,
) -> Vec<Line<'static>> {
    let line = gam_status_bar_line(app, generator_index, adversarial_index, width);
    let text = flatten_spans(&line);
    let settled = app
        .agents
        .get(generator_index)
        .and_then(|run| run.gam.as_ref())
        .is_some_and(|gam| matches!(gam.phase, crate::GamPhase::Settled(_)));
    if max_rows < 2 || !settled || !text.trim_end().ends_with("...") {
        return vec![line];
    }
    // Rebuild without truncation, wrapped to two rows.
    let wide = gam_status_bar_line(app, generator_index, adversarial_index, usize::MAX / 4);
    let lead_style = wide.spans.first().map(|span| span.style).unwrap_or_default();
    let lead = wide
        .spans
        .first()
        .map(|span| span.content.to_string())
        .unwrap_or_default();
    let sentence = wide
        .spans
        .get(2)
        .map(|span| span.content.to_string())
        .unwrap_or_default();
    let first_room = width.saturating_sub(lead.chars().count() + 3).max(8);
    let mut words = sentence.split(' ').peekable();
    let mut first = String::new();
    while let Some(word) = words.peek() {
        let next_len = first.chars().count() + usize::from(!first.is_empty()) + word.chars().count();
        if next_len > first_room {
            break;
        }
        if !first.is_empty() {
            first.push(' ');
        }
        first.push_str(word);
        words.next();
    }
    let second = truncate_chars(&words.collect::<Vec<_>>().join(" "), width.max(8));
    vec![
        Line::from(vec![
            Span::styled(lead, lead_style),
            Span::styled(" · ", muted_style(true)),
            Span::styled(first, pane_text_style(true)),
        ]),
        Line::from(Span::styled(second, pane_text_style(true))),
    ]
}

fn flatten_spans(line: &Line<'_>) -> String {
    line.spans.iter().map(|span| span.content.as_ref()).collect()
}

/// What the pair is doing, in one sentence, plus what to press. The halves'
/// titles each describe ONE side ("under review", "reviewing · #2"); this is
/// the pair as a whole: which round, who has the ball, the last thing said,
/// and once it settles, how it ended and what to do next.
pub(crate) fn gam_status_bar_line(
    app: &App,
    generator_index: usize,
    adversarial_index: usize,
    width: usize,
) -> Line<'static> {
    let Some(gam) = app
        .agents
        .get(generator_index)
        .and_then(|run| run.gam.as_ref())
    else {
        return Line::from("");
    };
    let who = |index: usize| {
        app.agents
            .get(index)
            .map(|run| {
                let model = run.model.trim();
                if model.is_empty() {
                    run.backend.as_str().to_string()
                } else {
                    model.to_string()
                }
            })
            .unwrap_or_else(|| "the reviewer".to_string())
    };
    let generator = who(generator_index);
    let reviewer = who(adversarial_index);
    let blocked = app
        .agents
        .get(generator_index)
        .is_some_and(|run| run.needs_user_input || run.needs_permission);
    let in_main = app
        .agents
        .get(generator_index)
        .is_some_and(|run| run.workspace_path.is_none());
    let rounds = |count: u32| format!("{count} round{}", if count == 1 { "" } else { "s" });

    let (lead, lead_style, sentence, hints): (String, Style, String, Vec<(&str, &str)>) =
        match &gam.phase {
            crate::GamPhase::Settled(outcome) => {
                let reviews = gam.round + 1;
                let (lead, style, sentence) = match outcome {
                    crate::GamOutcome::Accepted => {
                        let said = gam
                            .transcript
                            .iter()
                            .rev()
                            .find(|message| message.kind == crate::GamMessageKind::Accept)
                            .map(|message| format!(": {}", message.text.trim().replace('\n', " ")))
                            .unwrap_or_default();
                        let next = if in_main {
                            "the changes are in this checkout · start a new task for more work"
                        } else {
                            "next: m merges it · start a new task for more work"
                        };
                        (
                            "✓ accepted".to_string(),
                            accent_style(true),
                            format!("after {}{said} · {next}", rounds(reviews)),
                        )
                    }
                    crate::GamOutcome::Escalated(reason) => (
                        "needs you".to_string(),
                        Style::default().fg(RUNNING_COLOR),
                        format!(
                            "{} · answer in either pane",
                            if reason.trim().is_empty() {
                                "the pair ended without agreeing".to_string()
                            } else {
                                reason.trim().to_string()
                            }
                        ),
                    ),
                    crate::GamOutcome::Halted(reason) => (
                        "✗ the reviewer stopped it".to_string(),
                        error_style(),
                        format!(
                            "{} · ask it why in the reviewer pane",
                            if reason.trim().is_empty() {
                                "no reason recorded".to_string()
                            } else {
                                reason.trim().to_string()
                            }
                        ),
                    ),
                    crate::GamOutcome::Stalled(reason) => (
                        "stalled".to_string(),
                        Style::default().fg(RUNNING_COLOR),
                        format!(
                            "{} · send a follow-up in the generator pane",
                            if reason.trim().is_empty() {
                                "the generator stopped making progress".to_string()
                            } else {
                                reason.trim().to_string()
                            }
                        ),
                    ),
                    crate::GamOutcome::StoppedByUser => (
                        "stopped".to_string(),
                        muted_style(true),
                        format!(
                            "you ended the loop after {} · both panes stay live · {}",
                            rounds(gam.round),
                            if in_main {
                                "start a new task for more work"
                            } else {
                                "m merges what is there"
                            }
                        ),
                    ),
                };
                (
                    lead,
                    style,
                    sentence,
                    vec![("a", "swap"), ("t", "dialogue"), ("i", "tell reviewer")],
                )
            }
            phase => {
                let round = gam.round + 1;
                let doing = if blocked {
                    format!("{generator} needs you: a question or permission in its pane")
                } else {
                    match phase {
                        crate::GamPhase::GeneratorWorking if gam.round == 0 => {
                            format!("{generator} is writing the first draft")
                        }
                        crate::GamPhase::GeneratorWorking => {
                            format!("{generator} is answering objection {}", gam.round)
                        }
                        crate::GamPhase::AwaitingVerdict => {
                            format!("{reviewer} is reviewing turn {round}")
                        }
                        crate::GamPhase::AwaitingInterim => format!(
                            "{reviewer} is checking in while {generator} keeps writing"
                        ),
                        crate::GamPhase::Settled(_) => String::new(),
                    }
                };
                let mut sentence = doing;
                if gam.steers > 0 {
                    sentence.push_str(&format!(
                        " · {} steer{}",
                        gam.steers,
                        if gam.steers == 1 { "" } else { "s" }
                    ));
                }
                if !gam.user_notes.is_empty() {
                    sentence.push_str(&format!(
                        " · {} note{} queued for the reviewer",
                        gam.user_notes.len(),
                        if gam.user_notes.len() == 1 { "" } else { "s" }
                    ));
                }
                if let Some(last) = gam
                    .transcript
                    .iter()
                    .rev()
                    .find(|message| message.kind != crate::GamMessageKind::Note)
                {
                    let verb = match last.kind {
                        crate::GamMessageKind::Objection => "last verdict: revise",
                        crate::GamMessageKind::Reply => "generator replied",
                        crate::GamMessageKind::Steer => "last steer",
                        crate::GamMessageKind::Accept => "last verdict: accept",
                        crate::GamMessageKind::Escalate => "last verdict: escalate",
                        crate::GamMessageKind::Stop => "last verdict: stop",
                        crate::GamMessageKind::Note => "",
                    };
                    sentence.push_str(&format!(
                        " · {verb}: {}",
                        last.text.trim().replace('\n', " ")
                    ));
                }
                (
                    format!("gam · round {round}"),
                    accent_style(true),
                    sentence,
                    vec![
                        ("a", "swap"),
                        ("t", "dialogue"),
                        ("i", "brief"),
                        ("x", "stop"),
                    ],
                )
            }
        };

    let settled = matches!(gam.phase, crate::GamPhase::Settled(_));
    // `^w a swap · t dialogue · i brief · x stop`: one leader, its letters.
    let hint_width = 3
        + hints
            .iter()
            .map(|(key, label)| key.chars().count() + label.chars().count() + 1)
            .sum::<usize>()
        + 3 * hints.len().saturating_sub(1);
    let lead_width = lead.chars().count();
    let sentence_width = sentence.chars().count();
    // What happened outranks how to change it. A settled pair's sentence is
    // the outcome and the next step, so it never gives up room to the keys; a
    // running pair keeps its keys while the sentence still gets a fair share.
    let fits_whole = lead_width + 3 + sentence_width + 2 + hint_width <= width;
    let show_hints = fits_whole || (!settled && width >= lead_width + 3 + 32 + 2 + hint_width);
    let sentence_room = width
        .saturating_sub(lead_width + 3)
        .saturating_sub(if show_hints { hint_width + 2 } else { 0 })
        .max(8);
    let sentence = truncate_chars(&sentence, sentence_room);
    let mut spans = vec![
        Span::styled(lead.clone(), lead_style),
        Span::styled(" · ", muted_style(true)),
        Span::styled(sentence.clone(), pane_text_style(true)),
    ];
    if show_hints {
        let used = lead_width + 3 + sentence.chars().count();
        let pad = width.saturating_sub(used + hint_width);
        spans.push(Span::raw(" ".repeat(pad.clamp(2, 400))));
        spans.push(Span::styled("^w ", accent_style(true)));
        for (index, (key, label)) in hints.iter().enumerate() {
            if index > 0 {
                spans.push(Span::styled(" · ", muted_style(true)));
            }
            spans.push(Span::styled((*key).to_string(), accent_style(true)));
            spans.push(Span::styled(format!(" {label}"), muted_style(true)));
        }
    }
    Line::from(spans)
}

/// Split the worker area into (the two conversation panes, the dialogue strip).
///
/// The strip only appears once the two models have actually said something to
/// each other, and never when it would leave the panes too short to read: an
/// empty box explaining that nobody has spoken yet is worse than no box.
pub(crate) fn gam_split_rows(
    area: Rect,
    app: &App,
    generator_index: usize,
) -> (Rect, Option<Rect>) {
    let area = gam_split_bar(area, app, generator_index).map_or(area, |(_, rest)| rest);
    if !app.gam_transcript_visible {
        return (area, None);
    }
    let entries = app
        .agents
        .get(generator_index)
        .and_then(|run| run.gam.as_ref())
        .map(|gam| gam.transcript.len())
        .unwrap_or(0);
    if entries == 0 || area.height < 16 {
        return (area, None);
    }
    // Two border rows plus at least two lines of dialogue, capped so the live
    // panes always keep the majority of the height.
    let strip = (entries as u16 + 2).clamp(4, (area.height / 3).max(4));
    let panes = Rect {
        height: area.height.saturating_sub(strip),
        ..area
    };
    let strip = Rect {
        y: area.y + panes.height,
        height: strip,
        ..area
    };
    (panes, Some(strip))
}

/// The DIALOGUE between the two models: who said what, oldest first, newest
/// pinned to the bottom.
///
/// The split shows two terminals arguing past each other — each half holds only
/// its own side of the exchange, and the reviewer's half is a wall of packet
/// text it was sent, not the sentence that mattered. This strip is the
/// conversation itself: one line per turn, labelled by speaker, so the argument
/// can be followed without reading either pane.
pub(crate) fn render_gam_transcript(
    frame: &mut Frame<'_>,
    area: Rect,
    app: &App,
    generator_index: usize,
) {
    let Some(gam) = app
        .agents
        .get(generator_index)
        .and_then(|run| run.gam.as_ref())
    else {
        return;
    };
    let inner = block_inner(area);
    let rows = inner.height as usize;
    let width = inner.width as usize;
    let start = gam.transcript.len().saturating_sub(rows);
    let lines = gam.transcript[start..]
        .iter()
        .map(|message| {
            let label = format!(
                "{} · {}: ",
                message.round + 1,
                message.kind.label(message.speaker)
            );
            let body = message.text.trim().replace('\n', " ");
            let body = truncate_chars(
                &body,
                width.saturating_sub(label.chars().count() + 1).max(8),
            );
            Line::from(vec![
                Span::styled(label, gam_speaker_style(message)),
                Span::styled(body, pane_text_style(true)),
            ])
        })
        .collect::<Vec<_>>();
    let hidden = start;
    let title = if hidden > 0 {
        format!("dialogue · {hidden} earlier · ^w t hides")
    } else {
        "dialogue · ^w t hides".to_string()
    };
    let paragraph =
        Paragraph::new(lines)
            .style(app_style())
            .block(pane_block(&title, false, app.nav_mode));
    frame.render_widget(paragraph, area);
}

/// Colour carries the speaker (the theme emphasises by colour, not weight), and
/// a stop is the one line in the strip that should read as an alarm.
fn gam_speaker_style(message: &crate::GamMessage) -> Style {
    match message.kind {
        crate::GamMessageKind::Stop => error_style(),
        crate::GamMessageKind::Accept => accent_style(true),
        _ => match message.speaker {
            crate::GamRole::Generator => model_style(true),
            crate::GamRole::Adversarial => accent_style(true),
        },
    }
}

/// Terminal lines for ANY agent's pane — `worker_lines` parameterized by
/// index so the gam split can draw both conversations side by side. The
/// unfocused half gets no text selection or cursor.
fn worker_lines_at(app: &mut App, index: usize, height: usize, width: usize) -> Vec<Line<'static>> {
    let focused = app.focus == FocusPane::Worker && app.selected_agent == index;
    let Some(run) = app.agents.get_mut(index) else {
        return vec![Line::from("")];
    };
    let Some(terminal) = run.terminal.as_mut() else {
        // No live PTY (finished/reloaded): show the static conversation card,
        // same as the single-pane path does for a pane-less run.
        let mut lines = vec![
            Line::from(Span::styled(
                format!("{}  {}", run.status.as_str(), run.task_summary),
                pane_text_style(true),
            )),
            Line::from(""),
        ];
        for text_line in run.task.lines() {
            lines.push(Line::from(Span::styled(
                text_line.to_string(),
                pane_text_style(true),
            )));
        }
        lines.push(Line::from(Span::styled(
            run.worker_exit_note
                .clone()
                .unwrap_or_else(|| "This agent is not running.".to_string()),
            muted_style(true),
        )));
        return lines;
    };
    let selection = app
        .worker_selection
        .filter(|_| focused)
        .map(normalize_selection)
        .filter(|selection| !selection_is_empty(*selection));
    let (start_row, styled_rows) = terminal.styled_line_window_snapshot(height);
    // Selection rows are history line numbers (see `worker_selection_point`).
    let history_top = terminal.view_top_line();
    let lines = styled_rows
        .into_iter()
        .enumerate()
        .map(|(offset, cells)| {
            let row = start_row + offset;
            styled_terminal_line(
                cells,
                selection_for_row(selection, history_top + row),
                None,
            )
        })
        .collect::<Vec<_>>();
    lines
}

/// The `/gam` split: generator on the left, adversarial reviewer on the right,
/// each half a live terminal of one conversation in the pair.
pub(crate) fn render_gam_split(frame: &mut Frame<'_>, area: Rect, app: &mut App) {
    let Some((generator_index, adversarial_index)) = app.gam_pair_indices_for_selected() else {
        return;
    };
    if let Some((bar, _)) = gam_split_bar(area, app, generator_index) {
        // Inset to line up with the text inside the halves' borders.
        let bar = Rect {
            x: bar.x + 1,
            width: bar.width.saturating_sub(2),
            ..bar
        };
        let lines = gam_status_bar_lines(
            app,
            generator_index,
            adversarial_index,
            bar.width as usize,
            bar.height as usize,
        );
        frame.render_widget(Paragraph::new(lines).style(app_style()), bar);
    }
    let (panes, transcript) = gam_split_rows(area, app, generator_index);
    if let Some(strip) = transcript {
        render_gam_transcript(frame, strip, app, generator_index);
    }
    let (left, right) = gam_split_halves(panes);
    // Mid-move (⌥h) each half keeps the size it has in the slot it moves from or to.
    let (fit_left, fit_right) = app.transition_slot.map_or((left, right), |slot| {
        gam_split_halves(gam_split_rows(slot, app, generator_index).0)
    });
    for (half, fit_half, index, role_label) in [
        (left, fit_left, generator_index, "gen"),
        (right, fit_right, adversarial_index, "adv"),
    ] {
        // The PTY gets the half's INSIDE: sized to the outer rect, every full-width
        // line of agent output was two columns too wide and wrapped its tail onto
        // a line of its own.
        let half_inner_size = block_inner(fit_half);
        let size = TerminalSize::new(half_inner_size.height.max(1), half_inner_size.width.max(1)).ok();
        if let Some(size) = size {
            app.fit_pane(index, size);
        }
        let focused_half = app.focus == FocusPane::Worker && app.selected_agent == index;
        // A generator waiting on a question HALTS the whole pair: an "input"
        // signal never marks the run Done, and a packet is only built for a Done
        // generator, so the reviewer stands by forever with nothing to review.
        // Observed live in the libra session, where the badge said "writing" for
        // forty minutes while the answer was the thing everyone was waiting on.
        let blocked = app
            .agents
            .get(index)
            .is_some_and(|run| run.needs_user_input || run.needs_permission);
        let round_badge = app
            .agents
            .get(index)
            .and_then(|run| run.gam.as_ref())
            // Say what is HAPPENING, not which iteration of an internal counter
            // this is. "round 1/4" appeared before a single line had been
            // reviewed, so it read as a countdown that had already started, and
            // it never said what reaching 4 would do.
            //
            // `round` counts completed review cycles, so the number is only
            // meaningful once one has happened: at that point what the reader
            // wants is how many attempts are left before the pair stops and asks
            // them, which is max_rounds - 1 revisions after the first draft.
            .map(|gam| {
                let adversarial = gam.role == crate::GamRole::Adversarial;
                // Anything past a minute gets a clock. "standing by" for eighteen
                // minutes and "standing by" for eight seconds are the same three
                // words, and only one of them means something is wrong.
                let waited = gam
                    .phase_since
                    .map(|since| since.elapsed())
                    .filter(|elapsed| elapsed.as_secs() >= 60)
                    .map(|elapsed| format!(" · {}", humanize_quiet(elapsed)))
                    .unwrap_or_default();
                match &gam.phase {
                    // A blocked half is the whole pair's state, so it outranks
                    // whatever stage it would otherwise report.
                    _ if blocked && !adversarial => format!("needs you{waited}"),
                    crate::GamPhase::GeneratorWorking if gam.round == 0 => {
                        if adversarial {
                            format!("standing by{waited}")
                        } else {
                            format!("writing{waited}")
                        }
                    }
                    crate::GamPhase::GeneratorWorking => {
                        if adversarial {
                            format!(
                                "objected · {} review{}",
                                gam.round,
                                if gam.round == 1 { "" } else { "s" }
                            )
                        } else {
                            // No total: the pair runs until it converges, so
                            // a denominator here would be inventing a deadline.
                            format!("revision {}{waited}", gam.round)
                        }
                    }
                    // The two halves are doing different things here: one is
                    // being judged, the other is judging.
                    crate::GamPhase::AwaitingVerdict => {
                        if adversarial {
                            format!("reviewing · #{}{waited}", gam.round + 1)
                        } else {
                            format!("under review{waited}")
                        }
                    }
                    // The mid-turn check: the generator is NOT waiting on this,
                    // so its badge keeps saying what it is doing.
                    crate::GamPhase::AwaitingInterim => {
                        if adversarial {
                            format!("checking in{waited}")
                        } else if gam.round == 0 {
                            format!("writing · watched{waited}")
                        } else {
                            format!("revision {} · watched{waited}", gam.round)
                        }
                    }
                    crate::GamPhase::Settled(crate::GamOutcome::Accepted) => "accepted".to_string(),
                    crate::GamPhase::Settled(crate::GamOutcome::Escalated(_)) => {
                        "needs you".to_string()
                    }
                    crate::GamPhase::Settled(crate::GamOutcome::Halted(_)) => {
                        if adversarial {
                            "stopped it".to_string()
                        } else {
                            "stopped by reviewer".to_string()
                        }
                    }
                    crate::GamPhase::Settled(crate::GamOutcome::Stalled(_)) => "stalled".to_string(),
                    crate::GamPhase::Settled(crate::GamOutcome::StoppedByUser) => {
                        "stopped by you".to_string()
                    }
                }
            })
            .unwrap_or_default();

        let identity = app
            .agents
            .get(index)
            .map(|run| {
                // The reviewer is identified by WHO is arguing, not by the task
                // it shares with the generator. Its task_summary already carries
                // an "adv · " label for the agents list, and this line adds the
                // role prefix itself, so reusing it rendered "adv · adv · codex".
                let model = run.model.trim();
                let who = if model.is_empty() {
                    run.backend.as_str().to_string()
                } else {
                    format!("{} {}", run.backend.as_str(), model)
                };
                // Which road messages take: a live link, or typing into the pane.
                let link = app
                    .gam_links
                    .get(&run.id)
                    .filter(|link| link.can_send())
                    .map(|link| format!(" · ⇄ {}", link.label()))
                    .unwrap_or_default();
                if run.is_gam_adversary() {
                    format!("{} · {}{link}", truncate_chars(&who, 30), round_badge)
                } else {
                    // The generator names its model too: with `gen` the pair
                    // can be any two models, and "which one wrote this" was
                    // only answerable from the sidebar.
                    let summary = if run.task_summary.trim().is_empty() {
                        summarize_task(&run.task)
                    } else {
                        run.task_summary.clone()
                    };
                    // The task rides along only when it fits readably; the
                    // sidebar row and the status bar already carry it.
                    let room = (half.width as usize).saturating_sub(
                        who.chars().count().min(24) + round_badge.chars().count() + link.chars().count() + 22,
                    );
                    if room >= 16 {
                        format!(
                            "{} · {} · {}{link}",
                            truncate_chars(&who, 24),
                            truncate_chars(&summary, room),
                            round_badge
                        )
                    } else {
                        format!("{} · {}{link}", truncate_chars(&who, 24), round_badge)
                    }
                }
            })
            .unwrap_or_default();
        // The split is the only place two agent panes share the screen, and
        // nothing on it said the halves were reachable. Put the chord on the
        // half you are NOT in, which is the one you would want to move to.
        let title = if app.focus == FocusPane::Worker && !focused_half {
            format!("{role_label} · {identity} · ^w a")
        } else {
            format!("{role_label} · {identity}")
        };
        let lines = worker_lines_at(
            app,
            index,
            half_inner_size.height as usize,
            half_inner_size.width as usize,
        );
        let paragraph = Paragraph::new(lines)
            .style(app_style())
            .block(pane_block(&title, focused_half, app.nav_mode))
            .wrap(Wrap { trim: false });
        frame.render_widget(paragraph, half);
        if focused_half {
            let half_inner = block_inner(half);
            let Some(run) = app.agents.get(index) else {
                continue;
            };
            let Some(terminal) = run.terminal.as_ref() else {
                continue;
            };
            if let Some(position) = worker_hardware_cursor(terminal, half_inner) {
                frame.set_cursor_position(position);
            }
        }
    }
}

pub(crate) fn worker_lines(app: &mut App, height: usize, width: usize) -> Vec<Line<'static>> {
    let perf_start = Instant::now();
    let enter_resumes = app.enter_resumes_paneless_agent(app.selected_agent);
    // Some(keys typed so far) while the row's workspace is being prepared.
    let preparing = app
        .agents
        .get(app.selected_agent)
        .and_then(|run| app.pending_launches.get(&run.id))
        .map(|pending| pending.typed.len());
    let Some(run) = app.agents.get_mut(app.selected_agent) else {
        return vec![
            Line::from(""),
            Line::from(Span::styled("No worker is running yet.", muted_style(true))),
            Line::from(""),
            Line::from(Span::styled(
                "Enter a task below to start Claude Code or Codex in this pane.",
                pane_text_style(true),
            )),
        ];
    };

    if let Some(error) = &run.last_error {
        return vec![
            Line::from(vec![
                Span::styled("failed ", error_style()),
                Span::styled(run.cwd.display().to_string(), muted_style(true)),
            ]),
            Line::from(Span::styled(error.clone(), error_style())),
        ];
    }

    let backend = run.backend;
    let focused = app.focus == FocusPane::Worker;
    let run_id = run.id.clone();
    let Some(terminal) = run.terminal.as_mut() else {
        let title = if run.task_summary.trim().is_empty() {
            summarize_task(&run.task)
        } else {
            run.task_summary.clone()
        };
        let status = if preparing.is_some() {
            "preparing workspace"
        } else {
            run.status.as_str()
        };
        let mut lines = vec![
            Line::from(Span::styled(
                format!("{status}  {title}"),
                pane_text_style(true),
            )),
            Line::from(""),
        ];
        // The FULL objective/prompt, one Line per source line. The surrounding
        // Paragraph wraps long lines to the pane width, so a completed agent's
        // objective is fully readable here (it used to be short_task's 26 chars,
        // which made finished work unreviewable after its terminal was gone).
        for text_line in run.task.lines() {
            lines.push(Line::from(Span::styled(
                text_line.to_string(),
                pane_text_style(true),
            )));
        }
        lines.push(Line::from(""));
        if let Some(typed) = preparing {
            lines.push(Line::from(Span::styled(
                "Creating this agent's workspace. The agent starts here when it is ready.",
                muted_style(true),
            )));
            lines.push(Line::from(Span::styled(
                if typed == 0 {
                    "Anything you type here goes to the agent once it starts."
                } else {
                    "What you typed goes to the agent once it starts."
                },
                muted_style(true),
            )));
            return lines;
        }
        let idle = if matches!(run.mode, AgentMode::Plan | AgentMode::RudderPlan) {
            "This read-only planner is not running."
        } else {
            "This agent is not running."
        };
        lines.push(Line::from(Span::styled(
            if enter_resumes {
                format!("{idle} Press Enter to resume it.")
            } else {
                idle.to_string()
            },
            muted_style(true),
        )));
        lines.push(Line::from(Span::styled(
            run.cwd.display().to_string(),
            muted_style(true),
        )));
        return lines;
    };

    let selection = app
        .worker_selection
        .map(normalize_selection)
        .filter(|selection| !selection_is_empty(*selection));
    let (start_row, styled_rows) = terminal.styled_line_window_snapshot(height);
    let row_count = styled_rows.len();
    // Selection rows are history line numbers (see `worker_selection_point`).
    let history_top = terminal.view_top_line();
    let lines = styled_rows
        .into_iter()
        .enumerate()
        .map(|(offset, cells)| {
            let row = start_row + offset;
            styled_terminal_line(
                cells,
                selection_for_row(selection, history_top + row),
                None,
            )
        })
        .collect::<Vec<_>>();
    let duration = perf_start.elapsed();
    app.record_perf_duration("worker_lines", duration);
    app.log_perf_duration_over(
        "worker_lines",
        duration,
        SLOW_LINE_RENDER_THRESHOLD,
        serde_json::json!({
            "run_id": run_id,
            "rows": row_count,
            "height": height,
            "width": width,
            "start_row": start_row,
        }),
    );
    lines
}

/// The default task-pane hint shown when there is no transient notice. Centralised
/// so render_task, task_pane_height, and selection's height/hit-test calc all wrap
/// the SAME string: if these drift, the pane height and mouse selection bounds
/// disagree and clicks on valid input rows get rejected.
pub(crate) fn task_default_hint(app: &App) -> String {
    task_default_hint_static(app).to_string()
}

fn task_default_hint_static(app: &App) -> &'static str {
    // The moment a draft has two lines, "will Enter send this or add a line?" is
    // the only question worth answering, and it outranks every hint below.
    if app.task_input.contains('\n') {
        "Enter runs it  ·  ⌥⏎ / ⇧⏎ / ^J adds a line  ·  ↑↓ move between lines"
    } else if app.cloud_mode {
        // Cloud mode changes what the default route MEANS, so it outranks the
        // local hints: the user must never wonder where a typed task went.
        "cloud mode  ·  tasks start in Rudder Cloud and keep running while you're away  ·  /cloud off to go local"
    } else if app.plan().planner_paused_for_input {
        "↳ the planner asked a question — answer here or in the orchestrator pane"
    } else if app.plan().awaiting_approval {
        "type to talk to the orchestrator, or press Enter to approve  ·  Option-1/2/3/4 or ^W pane"
    } else if app.plan_is_active() {
        "type to talk to the live orchestrator; it can add, re-plan, merge, stop, or re-goal workers  ·  ^W pane"
    } else if app.agents.iter().any(|run| run.node_id.is_some())
        && app.has_running_interactive_orchestrator()
    {
        "type to talk to the live orchestrator for status, retro, or follow-up control  ·  ^W pane"
    } else {
        "describe a task; Rudder plans when needed; /main for this checkout  ·  Option-1/2/3/4 or ^W pane"
    }
}

/// What a notice asks of the user. Keyed off the message text so the ~60
/// notice call sites need no extra state to maintain.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum NoticeKind {
    Info,
    Error,
    /// A pending confirmation such as "press d again".
    Confirm,
}

pub(crate) fn notice_kind(text: &str) -> NoticeKind {
    const ERROR_MARKERS: &[&str] = &[
        "failed",
        "error",
        "conflict",
        "stopped",
        "timed out",
        "not found",
        "not logged in",
        "no longer exists",
        "cannot",
        "disabled",
    ];
    const CONFIRM_MARKERS: &[&str] = &[
        "press d again",
        "press q / ctrl+c again",
        HOME_UPDATE_CONFIRM_MARKER,
        "still running",
    ];
    let lower = text.to_ascii_lowercase();
    if CONFIRM_MARKERS.iter().any(|marker| lower.contains(marker)) {
        NoticeKind::Confirm
    } else if ERROR_MARKERS.iter().any(|marker| lower.contains(marker)) {
        NoticeKind::Error
    } else {
        NoticeKind::Info
    }
}

/// Style for a notice settled into the footer, by severity. Errors and pending
/// confirmations must not read like routine status: the notice is the only
/// surface many failures have, and an all-muted line buried them. Routine
/// status is muted, never faint: it is still something to read.
pub(crate) fn notice_style(text: &str) -> Style {
    let p = pal();
    match notice_kind(text) {
        NoticeKind::Confirm => Style::default().fg(p.waiting),
        NoticeKind::Error => Style::default().fg(p.failed),
        NoticeKind::Info => Style::default().fg(p.muted),
    }
}

/// Colors for the notice pill. The words are the terminal's own text color on
/// its own background, so they read in any theme, including one that did not
/// report its colors; the severity shows in the glyph and the border. A pending
/// confirmation is bold: it is about to delete or quit something.
#[derive(Clone, Copy, Debug)]
pub(crate) struct NoticePillColors {
    pub glyph: Color,
    pub text: Style,
    pub border: Color,
}

pub(crate) fn notice_pill_colors(kind: NoticeKind) -> NoticePillColors {
    let p = pal();
    let text = Style::default().fg(term_fg());
    match kind {
        NoticeKind::Info => NoticePillColors { glyph: p.accent, text, border: p.muted },
        NoticeKind::Error => NoticePillColors { glyph: p.failed, text, border: p.failed },
        NoticeKind::Confirm => NoticePillColors {
            glyph: p.waiting,
            text: text.add_modifier(Modifier::BOLD),
            border: p.waiting,
        },
    }
}

/// `task · claude opus(high)` — the backend, model and effort a new agent gets.
/// `/fast` is called out because it changes behaviour without changing the model.
pub(crate) fn task_pane_title(app: &App) -> String {
    // Cloud mode moves execution off this machine; the local model dial is not
    // the model that would run. Say where the task goes instead of implying a
    // local launch.
    if app.cloud_mode {
        return "task · Rudder Cloud".to_string();
    }
    let model = app.model.trim();
    if model.is_empty() {
        // opencode's legitimate "use your configured default".
        return format!("task · {}", app.backend.as_str());
    }
    let fast = if app.fast_mode { " · fast" } else { "" };
    format!(
        "task · {} {model}({}){fast}",
        app.backend.as_str(),
        effort_label(app.effort)
    )
}

/// The model tag on the prompt's border: what a new task runs on.
pub(crate) fn task_model_tag(app: &App) -> String {
    task_pane_title(app)
        .strip_prefix("task · ")
        .map(str::to_string)
        .unwrap_or_else(|| task_pane_title(app))
}

/// Width of the right-aligned "⏎ run" label inside the prompt pill.
fn task_run_label_width(inner: Rect) -> u16 {
    if inner.width >= 32 {
        8
    } else {
        0
    }
}

/// Columns before the draft: one cell of padding and the "› " mark.
const TASK_MARK_WIDTH: u16 = 3;

/// Where the draft text is drawn inside the prompt pill: after the "› " mark
/// and clear of the run label. Rendering, cursor placement, wrapping and
/// mouse selection all use this one rect.
pub(crate) fn task_text_rect(area: Rect) -> Rect {
    let inner = block_inner(area);
    let label = task_run_label_width(inner);
    Rect {
        x: inner.x.saturating_add(TASK_MARK_WIDTH),
        y: inner.y,
        width: inner.width.saturating_sub(TASK_MARK_WIDTH + label + 1).max(1),
        height: inner.height,
    }
}

/// Placeholder for an empty prompt: what typing here will do right now.
fn task_placeholder(app: &App) -> &'static str {
    if app.plan().planner_paused_for_input {
        "answer the planner's question"
    } else if app.plan().awaiting_approval {
        "refine the plan, or press enter to approve"
    } else if app.cloud_mode {
        "describe a task for Rudder Cloud\u{2026}"
    } else if app.plan_is_active() || app.has_running_interactive_orchestrator() {
        "talk to the orchestrator\u{2026}"
    } else {
        "describe a task\u{2026}"
    }
}

pub(crate) fn render_task(frame: &mut Frame<'_>, area: Rect, app: &App) {
    render_task_with_cursor(frame, area, app, true);
}

/// The task line; `cursor` false keeps the terminal cursor off it (while the
/// full-screen move draws a pane over it).
fn render_task_with_cursor(frame: &mut Frame<'_>, area: Rect, app: &App, cursor: bool) {
    let focused = app.focus == FocusPane::Task;
    let p = pal();
    let text = task_text_rect(area);
    let inner = block_inner(area);
    let width = text.width;
    let input_lines = task_input_lines(&app.task_input, app.task_cursor, width);
    let (cursor_line, cursor_column) =
        task_cursor_position(&app.task_input, app.task_cursor, width);
    let max_input_lines = text.height.max(1) as usize;
    let input_start = if input_lines.len() > max_input_lines {
        cursor_line.saturating_sub(max_input_lines.saturating_sub(1))
    } else {
        0
    };
    let input_start = input_start.min(input_lines.len().saturating_sub(1));
    let task_selection = if app.task_input.is_empty() {
        None
    } else {
        app.task_selection
            .map(normalize_selection)
            .filter(|selection| !selection_is_empty(*selection))
    };
    let lines = input_lines
        .iter()
        .skip(input_start)
        .take(max_input_lines)
        .enumerate()
        .map(|(offset, line)| {
            if app.task_input.is_empty() {
                return Line::from(Span::styled(
                    task_placeholder(app),
                    Style::default().fg(if focused { p.muted } else { p.faint }),
                ));
            }
            let row = input_start + offset;
            styled_plain_line(
                line.as_str(),
                pane_text_style(true),
                selection_for_row(task_selection, row),
            )
        })
        .collect::<Vec<_>>();

    let border = Style::default().fg(if focused { p.accent } else { p.faint });
    let block = Block::default()
        .borders(Borders::ALL)
        .border_type(BorderType::Rounded)
        .border_style(border)
        .title_top(
            Line::from(Span::styled(
                format!(" {} ", task_model_tag(app)),
                Style::default().fg(if focused { p.muted } else { p.faint }),
            ))
            .right_aligned(),
        )
        .style(app_style());
    frame.render_widget(block, area);
    if inner.width == 0 || inner.height == 0 {
        return;
    }
    let mark = Rect {
        x: inner.x.saturating_add(1),
        y: inner.y,
        width: inner.width.saturating_sub(1).min(2),
        height: 1,
    };
    frame.render_widget(
        Paragraph::new(Line::from(Span::styled(
            "\u{203a} ",
            Style::default()
                .fg(if focused { p.accent } else { p.muted })
                .add_modifier(Modifier::BOLD),
        ))),
        mark,
    );
    frame.render_widget(Paragraph::new(lines).style(app_style()), text);
    let label_w = task_run_label_width(inner);
    if label_w > 0 {
        let label = if app.plan().awaiting_approval { "refine" } else { "run" };
        let rect = Rect {
            x: inner.right().saturating_sub(label_w + 1),
            y: inner.y + inner.height.saturating_sub(1),
            width: label_w,
            height: 1,
        };
        frame.render_widget(
            Paragraph::new(Line::from(vec![
                Span::styled("\u{23ce} ", Style::default().fg(p.faint)),
                Span::styled(
                    label,
                    Style::default().fg(if focused { p.accent } else { p.muted }),
                ),
            ]))
            .right_aligned(),
            rect,
        );
    }

    if cursor && app.focus == FocusPane::Task {
        let visible_cursor_line = cursor_line.saturating_sub(input_start);
        let x = text.x + cursor_column as u16;
        let y = text.y + visible_cursor_line as u16;
        if x < inner.right() && y < text.bottom() {
            frame.set_cursor_position((x, y));
        }
    }
}

pub(crate) fn task_pane_height(app: &App, width: u16) -> u16 {
    // The prompt is the draft and nothing else: notices show as a pill and
    // then in the footer, so a notice coming or going never changes the
    // worker pane's height (a resize makes Claude and Codex repaint their
    // whole screen, the flicker, and over Cloud it cascades further).
    let text = task_text_rect(Rect::new(0, 0, width, 3));
    let input_lines = task_input_lines(&app.task_input, app.task_cursor, text.width)
        .len()
        .max(1) as u16;
    2_u16.saturating_add(input_lines).clamp(3, 9)
}

pub(crate) fn render_suggestions(frame: &mut Frame<'_>, task_area: Rect, app: &App) {
    let suggestions = suggestions_for(app);
    if suggestions.is_empty() {
        return;
    }

    let visible_count = suggestions
        .len()
        .min(task_area.y.saturating_sub(2).min(10) as usize);
    if visible_count == 0 {
        return;
    }
    let height = (visible_count as u16).saturating_add(2);
    if task_area.y < height {
        return;
    }
    let area = Rect {
        x: task_area.x,
        y: task_area.y - height,
        width: task_area.width,
        height,
    };

    let selected_index = app.picker_index.min(suggestions.len().saturating_sub(1));
    let offset = selected_index.saturating_sub(visible_count.saturating_sub(1));
    let max_label_width = area.width.saturating_sub(8).min(32) as usize;
    let label_width = suggestions
        .iter()
        .skip(offset)
        .take(visible_count)
        .map(|suggestion| suggestion.label.chars().count())
        .max()
        .unwrap_or(0)
        .min(max_label_width);
    let items = suggestions
        .iter()
        .skip(offset)
        .take(visible_count)
        .enumerate()
        .map(|(index, suggestion)| {
            let selected = index + offset == selected_index;
            let marker = if selected { "> " } else { "  " };
            let style = if selected {
                accent_style(true)
            } else {
                app_style()
            };
            let label = truncate_chars(&suggestion.label, label_width);
            ListItem::new(Line::from(vec![
                Span::styled(marker, style),
                Span::styled(format!("{label:<label_width$}"), style),
                Span::raw("  "),
                Span::styled(suggestion.detail.clone(), muted_style(true)),
            ]))
        })
        .collect::<Vec<_>>();

    let title = if app.task_input.starts_with("/model") {
        model_picker_title(
            &app.task_input,
            selected_index.saturating_add(1),
            suggestions.len(),
        )
    } else {
        format!(
            " commands · {}/{} ",
            selected_index.saturating_add(1),
            suggestions.len()
        )
    };
    let title = truncate_chars(&title, area.width.saturating_sub(4) as usize);
    let list = List::new(items).style(app_style()).block(
        Block::default()
            .title(title)
            .borders(Borders::ALL)
            .border_style(Style::default().fg(FOCUS_COLOR))
            .style(app_style()),
    );

    frame.render_widget(Clear, area);
    frame.render_widget(list, area);
}

pub(crate) fn model_picker_title(input: &str, selected: usize, total: usize) -> String {
    let rest = input
        .trim_start()
        .strip_prefix("/model")
        .unwrap_or_default();
    let trailing_space = rest.ends_with(char::is_whitespace);
    let parts = rest.split_whitespace().collect::<Vec<_>>();
    let step = match parts.as_slice() {
        [] => "provider".to_string(),
        [provider] if !trailing_space => format!("provider · {provider}"),
        [provider] => format!("{provider} · model"),
        [provider, model] if !trailing_space => format!("{provider} · model · {model}"),
        [provider, model] => format!("{provider} · {model} · effort"),
        [provider, model, ..] => format!("{provider} · {model} · effort"),
    };
    format!(" model · {step} · {selected}/{total} ")
}

pub(crate) fn render_merge_prompt(frame: &mut Frame<'_>, area: Rect, app: &App) {
    let (title, lines, border_color): (Span<'static>, Vec<Line<'static>>, Color) = if let Some(
        confirm,
    ) =
        &app.merge_confirm
    {
        let publishing = matches!(confirm.intent, MergeIntent::Publish { .. });
        let headline = match &confirm.intent {
            MergeIntent::Selected { task, .. } => format!("Merge:  {}", short_task(task)),
            MergeIntent::Publish { task, .. } => format!("Publish:  {}", short_task(task)),
        };
        // The ACTION comes first. This modal used to explain itself for three
        // wrapped rows and mention the keys only on the last one, which is also the
        // row that got clipped — so the question "which key confirms?" had no
        // answer on screen.
        let mut lines = vec![
            Line::from(Span::styled(headline, app_style())),
            Line::from(""),
            key_choice_line(&[
                (
                    "y",
                    if publishing {
                        "push and open PR"
                    } else {
                        "merge"
                    },
                ),
                ("Esc", "cancel"),
            ]),
            Line::from(""),
            Line::from(Span::styled(
                if publishing {
                    // The detail line below names the remote and branch. This one
                    // says what the answer commits the repo to, because the prompt
                    // is asked exactly once per repo.
                    "The PR opens as a draft. Rudder will not ask again for this repo; from here on m publishes instead of merging locally."
                } else {
                    "Dependent nodes unblock once it lands. A clean merge is mechanical (no AI); on conflict you can launch an AI resolver or resolve it yourself."
                },
                muted_style(true),
            )),
        ];
        if let Some(detail) = &confirm.detail {
            lines.push(Line::from(""));
            lines.push(Line::from(Span::styled(
                detail.clone(),
                Style::default().fg(RUNNING_COLOR),
            )));
        }

        (
            Span::styled(
                if publishing {
                    " First publish · y pushes and opens a draft PR · Esc cancels "
                } else {
                    " Confirm merge · y merges · Esc cancels "
                },
                Style::default().fg(ACCENT),
            ),
            lines,
            ACCENT,
        )
    } else if let Some(confirm) = &app.cloud_delete_confirm {
        let mut lines = vec![
            Line::from(Span::styled(
                format!("Delete from Rudder Cloud:  {}", confirm.label),
                app_style(),
            )),
            Line::from(""),
            key_choice_line(&[("y", "delete it"), ("n", "keep it")]),
            Line::from(""),
        ];
        let consequence = match &confirm.target {
            crate::CloudDeleteTarget::Sail(id) => format!(
                "Destroys cloud sail {id}: its machine and its snapshot. The conversation running there ends. This cannot be undone."
            ),
            crate::CloudDeleteTarget::HomeAgent(_) => {
                "Deletes it on the home machine: its conversation ends there and it disappears from the phone. This laptop's copy of its workspace goes too. This cannot be undone."
                    .to_string()
            }
            crate::CloudDeleteTarget::Workspace => {
                let count = confirm.run_ids.len();
                format!(
                    "This agent runs in this repo's shared cloud workspace, so deleting it deletes the workspace: its machine, volume and snapshot, and the {count} cloud agent{} running there. This cannot be undone.",
                    if count == 1 { "" } else { "s" }
                )
            }
        };
        lines.push(Line::from(Span::styled(consequence, Style::default().fg(RUNNING_COLOR))));
        lines.push(Line::from(""));
        lines.push(Line::from(Span::styled(
            "If Fly is slow to let go of anything, Rudder Cloud keeps retrying until it is all gone.",
            muted_style(true),
        )));
        (
            Span::styled(
                " Delete from Rudder Cloud? · y deletes · n keeps it ",
                Style::default().fg(FAILED_COLOR),
            ),
            lines,
            FAILED_COLOR,
        )
    } else if let Some(prompt) = &app.conflict_prompt {
        let operation_label = if prompt.operation == ConflictOperation::Rebase {
            "Rebase"
        } else {
            "Merge"
        };
        let count = prompt.conflicted_files.len();
        let mut lines = vec![
            Line::from(Span::styled(
                format!(
                    "{operation_label} stopped on {count} conflicted file{}.",
                    if count == 1 { "" } else { "s" }
                ),
                Style::default().fg(FAILED_COLOR),
            )),
            Line::from(""),
        ];
        lines.push(key_choice_line(&[
            ("y", "AI resolver"),
            ("n", "resolve it yourself"),
            ("Esc", "cancel"),
        ]));
        lines.push(Line::from(""));
        if prompt.conflicted_files.is_empty() {
            lines.push(Line::from(Span::styled(
                "No specific files were reported.",
                muted_style(true),
            )));
        } else {
            // Cap the list so a wide conflict cannot push the key hints out of
            // the fixed-height modal.
            const MAX_CONFLICT_ROWS: usize = 8;
            for file in prompt.conflicted_files.iter().take(MAX_CONFLICT_ROWS) {
                lines.push(Line::from(vec![
                    Span::styled("   • ", muted_style(true)),
                    Span::styled(file.clone(), app_style()),
                ]));
            }
            if count > MAX_CONFLICT_ROWS {
                lines.push(Line::from(Span::styled(
                    format!("   … and {} more", count - MAX_CONFLICT_ROWS),
                    muted_style(true),
                )));
            }
        }

        (
            Span::styled(
                if prompt.operation == ConflictOperation::Rebase {
                    " Rebase conflict "
                } else {
                    " Merge conflict "
                },
                Style::default().fg(FAILED_COLOR),
            ),
            lines,
            FAILED_COLOR,
        )
    } else {
        return;
    };

    // Height must count WRAPPED rows, not Line structs. The body holds a long
    // explanatory sentence and, when the checkout is dirty, a warning — each one
    // Line that renders as three or four. Sizing by `lines.len()` clipped the
    // bottom of the modal, and the bottom is exactly where the key hint lives, so
    // the dialogue asked for confirmation without ever saying which key confirms.
    let modal_width = 76;
    let inner_width = modal_width
        .min(area.width.saturating_sub(4))
        .max(24)
        .saturating_sub(2);
    let modal = centered_modal(
        area,
        modal_width,
        wrapped_line_count(&lines, inner_width).saturating_add(2),
    );
    let block = Block::default()
        .title(title)
        .borders(Borders::ALL)
        .border_style(Style::default().fg(border_color))
        .style(app_style());
    let paragraph = Paragraph::new(lines)
        .style(app_style())
        .block(block)
        .wrap(Wrap { trim: true });
    frame.render_widget(Clear, modal);
    frame.render_widget(paragraph, modal);
}

pub(crate) fn render_cloud_prompt(frame: &mut Frame<'_>, area: Rect, app: &App) {
    let Some(prompt) = &app.cloud_prompt else {
        return;
    };
    let selected = prompt
        .selected_task
        .as_deref()
        .map(|task| {
            format!(
                "onload current Rudder workspace to cloud: {}",
                short_task(task)
            )
        })
        .unwrap_or_else(|| "onload current Rudder workspace to cloud".to_string());
    let upload_selected = prompt.choice == CloudLaunchChoice::Upload;
    let scratch_selected = prompt.choice == CloudLaunchChoice::Scratch;
    let row_style = |selected: bool| {
        if selected {
            accent_style(true)
        } else {
            app_style()
        }
    };
    let marker = |selected: bool| if selected { "> " } else { "  " };
    let lines = vec![
        Line::from(Span::styled(
            "Move this Rudder workspace to the cloud, or start a fresh cloud worker.",
            app_style(),
        )),
        Line::from(""),
        Line::from(vec![
            Span::styled(marker(upload_selected), accent_style(true)),
            Span::styled(selected, row_style(upload_selected)),
        ]),
        Line::from(vec![
            Span::styled(marker(scratch_selected), accent_style(true)),
            Span::styled(
                "start scratch in a fresh cloud directory",
                row_style(scratch_selected),
            ),
        ]),
        Line::from(""),
        // Keys accent-colored like every other modal hint (emphasis by color,
        // not weight), so the available actions read at a glance.
        Line::from(vec![
            Span::styled("Up/Down", accent_style(true)),
            Span::styled(" choose  ·  ", muted_style(true)),
            Span::styled("Enter", accent_style(true)),
            Span::styled(" start  ·  ", muted_style(true)),
            Span::styled("Esc", accent_style(true)),
            Span::styled(" cancel", muted_style(true)),
        ]),
    ];
    // Same wrapping rule as the merge modal: size by rendered rows, or the last
    // line (which carries the keys) falls off the bottom.
    let modal_width = 78;
    let inner_width = modal_width
        .min(area.width.saturating_sub(4))
        .max(24)
        .saturating_sub(2);
    let modal = centered_modal(
        area,
        modal_width,
        wrapped_line_count(&lines, inner_width).saturating_add(2),
    );
    let block = Block::default()
        .title(" cloud launch ")
        .borders(Borders::ALL)
        .border_style(Style::default().fg(CLOUD_COLOR))
        .style(app_style());
    frame.render_widget(Clear, modal);
    frame.render_widget(
        Paragraph::new(lines)
            .style(app_style())
            .block(block)
            .wrap(Wrap { trim: true }),
        modal,
    );
}

/// The one row a confirm dialogue must never bury: `[y] merge   [Esc] cancel`,
/// keys bracketed and in the accent colour so they read as buttons rather than
/// prose. Rendered directly under the headline, before any explanation.
pub(crate) fn key_choice_line(choices: &[(&str, &str)]) -> Line<'static> {
    let mut spans = vec![Span::raw("  ")];
    for (index, (key, label)) in choices.iter().enumerate() {
        if index > 0 {
            spans.push(Span::raw("     "));
        }
        spans.push(Span::styled("[", muted_style(true)));
        spans.push(Span::styled((*key).to_string(), accent_style(true)));
        spans.push(Span::styled("] ", muted_style(true)));
        spans.push(Span::styled((*label).to_string(), app_style()));
    }
    Line::from(spans)
}

/// How many terminal rows these lines occupy once wrapped to `width` — what a
/// modal must be sized by, since `Paragraph::wrap` reflows every Line. Sizing by
/// `lines.len()` clipped the bottom of the box, and the bottom is where a
/// dialogue's keys used to live.
pub(crate) fn wrapped_line_count(lines: &[Line<'_>], width: u16) -> u16 {
    lines
        .iter()
        .map(|line| {
            let text: String = line
                .spans
                .iter()
                .map(|span| span.content.as_ref())
                .collect::<Vec<_>>()
                .join("");
            if text.trim().is_empty() {
                1
            } else {
                wrap_text(&text, width).len().max(1) as u16
            }
        })
        .sum()
}

pub(crate) fn centered_modal(area: Rect, desired_width: u16, desired_height: u16) -> Rect {
    let width = desired_width.min(area.width.saturating_sub(4)).max(24);
    let height = desired_height.min(area.height.saturating_sub(2)).max(5);
    Rect {
        x: area.x + area.width.saturating_sub(width) / 2,
        y: area.y + area.height.saturating_sub(height) / 2,
        width: width.min(area.width),
        height: height.min(area.height),
    }
}

pub(crate) fn pane_block(title: &str, focused: bool, nav_mode: bool) -> Block<'static> {
    let p = pal();
    let _ = nav_mode;
    // Rounded hairline, accent when focused. The title is a quiet label, not a
    // filled tab: the border color already says which pane has the keyboard.
    let border_style = Style::default().fg(if focused { p.accent } else { p.faint });
    let title_style = Style::default().fg(if focused { p.muted } else { p.faint });
    Block::default()
        .title(Line::from(Span::styled(format!(" {title} "), title_style)))
        .borders(Borders::ALL)
        .border_type(BorderType::Rounded)
        .border_style(border_style)
        .style(app_style())
}

pub(crate) fn task_input_lines(value: &str, cursor: usize, width: u16) -> Vec<String> {
    let mut lines = wrap_input_text(value, width);
    let (cursor_line, _) = task_cursor_position(value, cursor, width);
    while lines.len() <= cursor_line {
        lines.push(String::new());
    }
    lines
}

pub(crate) fn wrap_input_text(value: &str, width: u16) -> Vec<String> {
    let max_width = usize::from(width.max(1));
    let mut lines = vec![String::new()];

    for ch in value.chars() {
        if ch == '\r' {
            continue;
        }
        if ch == '\n' {
            lines.push(String::new());
            continue;
        }
        if lines
            .last()
            .is_some_and(|line| line.chars().count() == max_width)
        {
            lines.push(String::new());
        }
        if let Some(line) = lines.last_mut() {
            line.push(ch);
        }
    }

    lines
}

pub(crate) fn task_cursor_position(value: &str, cursor: usize, width: u16) -> (usize, usize) {
    let max_width = usize::from(width.max(1));
    let mut line = 0;
    let mut column = 0;

    for ch in value.chars().take(cursor) {
        if ch == '\r' {
            continue;
        }
        if ch == '\n' {
            line += 1;
            column = 0;
            continue;
        }
        column += 1;
        if column == max_width {
            line += 1;
            column = 0;
        }
    }

    (line, column)
}

pub(crate) fn wrap_text(value: &str, width: u16) -> Vec<String> {
    let max_width = usize::from(width.max(1));
    let mut lines = Vec::new();
    let mut current = String::new();

    for word in value.split_whitespace() {
        if current.is_empty() {
            push_wrapped_word(&mut lines, &mut current, word, max_width);
            continue;
        }

        if current.chars().count() + 1 + word.chars().count() <= max_width {
            current.push(' ');
            current.push_str(word);
        } else {
            lines.push(std::mem::take(&mut current));
            push_wrapped_word(&mut lines, &mut current, word, max_width);
        }
    }

    if !current.is_empty() {
        lines.push(current);
    }
    if lines.is_empty() {
        lines.push(String::new());
    }

    lines
}

pub(crate) fn push_wrapped_word(
    lines: &mut Vec<String>,
    current: &mut String,
    word: &str,
    max_width: usize,
) {
    if word.chars().count() <= max_width {
        current.push_str(word);
        return;
    }

    let mut chunk = String::new();
    for ch in word.chars() {
        if chunk.chars().count() == max_width {
            lines.push(std::mem::take(&mut chunk));
        }
        chunk.push(ch);
    }
    *current = chunk;
}

pub(crate) fn pane_text_style(focused: bool) -> Style {
    if focused {
        app_style()
    } else {
        Style::default().fg(pal().muted)
    }
}

pub(crate) fn muted_style(focused: bool) -> Style {
    let p = pal();
    Style::default().fg(if focused { p.muted } else { p.faint })
}

pub(crate) fn accent_style(focused: bool) -> Style {
    let p = pal();
    Style::default().fg(if focused { p.accent } else { p.muted })
}

pub(crate) fn model_style(focused: bool) -> Style {
    Style::default().fg(if focused { MODEL_COLOR } else { FAINT })
}

pub(crate) fn cloud_style(connected: bool, focused: bool) -> Style {
    let color = if connected { CLOUD_COLOR } else { FAINT };
    let _ = focused;
    Style::default().fg(color)
}

/// Base style for the whole UI. Terminal mode intentionally sets neither fg nor
/// bg so the user's terminal theme shows through; paper mode restores the old
/// ink-on-white canvas.
pub(crate) fn app_style() -> Style {
    if terminal_background_mode() {
        Style::default()
    } else {
        Style::default().fg(INK).bg(PAPER)
    }
}

pub(crate) fn error_style() -> Style {
    Style::default().fg(pal().failed)
}

pub(crate) fn block_inner(area: Rect) -> Rect {
    Rect {
        x: area.x.saturating_add(1),
        y: area.y.saturating_add(1),
        width: area.width.saturating_sub(2),
        height: area.height.saturating_sub(2),
    }
}

pub(crate) fn rect_contains(area: Rect, x: u16, y: u16) -> bool {
    x >= area.x && x < area.right() && y >= area.y && y < area.bottom()
}

pub(crate) fn is_scroll_mouse_event(kind: MouseEventKind) -> bool {
    matches!(
        kind,
        MouseEventKind::ScrollUp
            | MouseEventKind::ScrollDown
            | MouseEventKind::ScrollLeft
            | MouseEventKind::ScrollRight
    )
}

/// On-screen rows a single logical line occupies once wrapped to `width` columns.
/// A blank or sub-column line still takes one row.
pub(crate) fn wrapped_row_count(line_width: usize, width: usize) -> usize {
    line_width.div_ceil(width.max(1)).max(1)
}

/// Largest valid scroll offset (lines from the top) for a list of `content_rows`
/// drawn in a `visible_height`-row viewport. Zero when everything fits.
pub(crate) fn orchestrator_dag_max_scroll(content_rows: usize, visible_height: usize) -> usize {
    content_rows.saturating_sub(visible_height)
}

pub(crate) fn orchestrator_scroll_offset(
    scroll: &mut usize,
    max_scroll: usize,
    follow_bottom: bool,
) -> usize {
    if follow_bottom {
        *scroll = max_scroll;
    } else {
        *scroll = (*scroll).min(max_scroll);
    }
    *scroll
}

pub(crate) fn mouse_scrollback_delta(mouse: MouseEvent, viewport_height: u16) -> isize {
    let rows = wheel_scroll_rows(viewport_height, mouse.modifiers) as isize;
    match mouse.kind {
        MouseEventKind::ScrollUp => rows,
        MouseEventKind::ScrollDown => -rows,
        MouseEventKind::ScrollLeft | MouseEventKind::ScrollRight => 0,
        _ => 0,
    }
}

/// A wheel event this soon after the previous one starts a burst: a trackpad
/// flick (and a fast-spun wheel) arrives every ~8-16ms, a deliberate notch
/// much slower.
pub(crate) const WHEEL_BURST_START_GAP: Duration = Duration::from_millis(30);
/// A burst survives gaps up to this long, so a trackpad's momentum tail (its
/// events thin out as it coasts) keeps moving a row at a time instead of
/// lurching back to whole notches at the end of the gesture.
pub(crate) const WHEEL_BURST_HOLD_GAP: Duration = Duration::from_millis(150);

/// Whether a wheel event at `now` belongs to a burst, given the previous
/// event's time and whether IT was in one.
pub(crate) fn wheel_event_in_burst(previous: Option<(Instant, bool)>, now: Instant) -> bool {
    let Some((at, was_burst)) = previous else {
        return false;
    };
    let gap = now.saturating_duration_since(at);
    gap < WHEEL_BURST_START_GAP || (was_burst && gap < WHEEL_BURST_HOLD_GAP)
}

/// Rows one wheel event moves. A terminal turns every trackpad step into its
/// own wheel event, so moving a notch's worth of rows per event made a flick
/// leap in 3-row jumps; inside a burst each event moves one row, which is how
/// a native terminal scrolls. A lone notch still moves the configured rows,
/// and a modifier still pages.
pub(crate) fn burst_scrollback_delta(mouse: MouseEvent, viewport_height: u16, in_burst: bool) -> isize {
    let paging = mouse.modifiers.intersects(
        KeyModifiers::ALT | KeyModifiers::CONTROL | KeyModifiers::META | KeyModifiers::SUPER,
    );
    if !in_burst || paging {
        return mouse_scrollback_delta(mouse, viewport_height);
    }
    match mouse.kind {
        MouseEventKind::ScrollUp => 1,
        MouseEventKind::ScrollDown => -1,
        _ => 0,
    }
}

pub(crate) fn wheel_scroll_rows(viewport_height: u16, modifiers: KeyModifiers) -> u16 {
    let page = viewport_height.saturating_sub(1).max(1);
    if modifiers.intersects(
        KeyModifiers::ALT | KeyModifiers::CONTROL | KeyModifiers::META | KeyModifiers::SUPER,
    ) {
        return page;
    }

    wheel_scroll_rows_setting().min(page).max(1)
}

pub(crate) fn wheel_scroll_rows_setting() -> u16 {
    env::var("RUDDER_WHEEL_SCROLL_ROWS")
        .ok()
        .and_then(|value| value.trim().parse::<u16>().ok())
        .filter(|value| *value > 0)
        .unwrap_or(DEFAULT_WHEEL_SCROLL_ROWS)
}

pub(crate) fn page_scroll_rows(area: Option<Rect>) -> isize {
    let height = area.map(block_inner).map(|inner| inner.height).unwrap_or(1);
    height.saturating_sub(1).max(1) as isize
}

/// Scrollback rows for an Alt-modified key in the worker pane, or None when
/// the key is not an alt-scroll binding. Vim grammar, held behind Alt so the
/// bare letters still reach the agent when the pane is focused:
///   Alt+k / Alt+Up   → one line up        Alt+j / Alt+Down → one line down
///   Alt+u            → half a page up     Alt+n            → half a page down
/// (Alt+d used to be the half-page-down key; it now opens the diff panel from
/// every pane, and "n" for the next half page keeps the pair one-handed.)
/// Positive rows scroll toward history (up), matching scrollback_by.
pub(crate) fn alt_scroll_rows(key: KeyEvent, area: Option<Rect>) -> Option<isize> {
    if !key
        .modifiers
        .intersects(KeyModifiers::ALT | KeyModifiers::META)
    {
        return None;
    }
    let half = (page_scroll_rows(area) / 2).max(1);
    // Letters only. Alt+Up/Alt+Down used to scroll too, which meant a Codex
    // worker never received them, and Codex binds them itself (message
    // history). The arrows belong to the agent; ⌥k/⌥j scroll.
    match key.code {
        KeyCode::Char('k') => Some(1),
        KeyCode::Char('j') => Some(-1),
        KeyCode::Char('u') => Some(half),
        KeyCode::Char('n') => Some(-half),
        _ => None,
    }
}

pub(crate) fn status_style(status: AgentStatus) -> Style {
    Style::default().fg(status_color(status))
}

/// The status a row actually shows. A published row's PR outranks every local
/// label: once the work is a pull request, "done · press m to read the diff" is a
/// stale instruction and `PR #123 · draft` is where the work now lives.
pub(crate) fn agent_status_text(agent: &AgentRun) -> String {
    if let Some(label) = agent.publish.label() {
        return label;
    }
    let label = agent_status_label(agent).to_string();
    // A row that says "working" says exactly the same thing whether the agent
    // is mid-thought or has been parked for a day and a half. In ~/code/battery
    // an orchestrator sat at "running" for 32 HOURS with a live-but-idle
    // process and nothing on screen distinguished it from real work. Once a
    // running agent has been silent well past any normal pause, say how long.
    // (The clock restarts when a dashboard reloads a row, so this reports
    // silence within the current session, never a fabricated longer one.)
    if agent.status == AgentStatus::Running
        && !agent.needs_permission
        && !agent.needs_user_input
        && agent.turn_error().is_none()
    {
        let quiet = agent.last_output_at.elapsed();
        if quiet >= QUIET_AGENT_THRESHOLD {
            return format!("{label} · quiet {}", humanize_quiet(quiet));
        }
    }
    label
}

/// How long a running agent may go without output before the row says so.
/// Long enough that a thinking model or a slow build never trips it.
const QUIET_AGENT_THRESHOLD: Duration = Duration::from_secs(15 * 60);

fn humanize_quiet(quiet: Duration) -> String {
    let minutes = quiet.as_secs() / 60;
    if minutes < 90 {
        format!("{minutes}m")
    } else {
        let hours = minutes / 60;
        if hours < 48 {
            format!("{hours}h")
        } else {
            format!("{}d", hours / 24)
        }
    }
}

pub(crate) fn agent_status_label(agent: &AgentRun) -> &'static str {
    // Evidence first. A row that claims "merged" while jj says the change is no
    // longer in trunk is the most misleading thing the pane can show — the work
    // looks landed and is not. Same for a row still pointing at a workspace that
    // has been swept: every key you press against it will fail.
    if agent.status == AgentStatus::Merged && agent.integration.in_trunk == Some(false) {
        return "merged · NOT in main";
    }
    if agent.needs_permission {
        "needs permission"
    } else if agent.wait_signal == Some(WaitSignal::Idle) {
        // Silent at its composer for a minute with the row still "running":
        // the turn ended (or never ran) and nothing reported it.
        "idle at its prompt \u{b7} no turn running"
    } else if agent.wait_signal == Some(WaitSignal::Interrupted) {
        // You stopped the turn (Esc, or rejecting a tool call). Claude reports
        // nothing for that, so this row used to read "working" for as long as
        // you left it.
        "interrupted \u{b7} waiting for you"
    } else if agent.needs_user_input {
        "needs input"
    } else if agent.turn_error().is_some() {
        // A live worker whose last turn died on the API (rate limit, auth,
        // overloaded). It is not Failed — the process is fine — but it is not
        // working either, and "running" was a lie that hid every rate limit.
        "turn failed \u{b7} retry"
    } else if let Some(label) = gam_row_status_label(agent) {
        label
    } else if matches!(agent.mode, AgentMode::Plan | AgentMode::RudderPlan)
        && agent.status == AgentStatus::Running
    {
        "plan mode"
    } else if agent.status == AgentStatus::Merged
        && agent.delivery.required
        && !agent.delivery.is_verified()
    {
        // A "deploy/publish" node auto-merges within a tick, and the plain
        // "merged locally" label silently swallowed its unmet delivery proof.
        "merged · delivery proof needed"
    } else if agent.merged_through_pr() {
        "PR merged"
    } else if agent.status == AgentStatus::Merged && agent.integration.pushed {
        "pushed"
    } else if agent.status == AgentStatus::Merged {
        "merged locally"
    } else if agent.integration.phase == IntegrationPhase::Integrating {
        "integrating"
    } else if agent.has_merge_conflict() && agent.status == AgentStatus::Running {
        "resolving conflict"
    } else if agent.has_merge_conflict()
        && agent.status == AgentStatus::Done
        && agent.merge_conflict_files.is_empty()
    {
        "needs you · checks failed"
    } else if agent.has_merge_conflict() && agent.status == AgentStatus::Done {
        "needs you · conflict"
    } else if agent.status == AgentStatus::Done && agent.delivery.required {
        // A deploy/publish task's delivery state (deployed / delivery blocked /
        // delivery proof needed) outranks the generic merge hint — these labels
        // were unreachable for workspace runs because the awaits-merge branch
        // below always won.
        agent.lifecycle_label()
    } else if agent.swarm_experiment && agent.status == AgentStatus::Done {
        // A swarm experiment is a candidate, not a piece of the goal: it never
        // integrates on its own, so "auto-merging" would be a lie.
        "experiment done \u{b7} m merges it"
    } else if agent.status == AgentStatus::Done && agent_awaits_merge(agent) {
        // Two different futures, one bucket: planned DAG nodes integrate on
        // their own; everything else waits for the user. The old label
        // ("verifying · ready to integrate") claimed active work that wasn't
        // happening and never said WHO merges next.
        if agent.node_id.is_some() {
            "done · auto-merging"
        } else if agent.reviewed_at.is_some() {
            // The gate is only visible if the two sides look different. Without this
            // both a read and an unread row said "press m to merge", and the extra
            // keystroke on unread work read as the dashboard misbehaving.
            "reviewed · press m to merge"
        } else {
            "done · press m to read the diff"
        }
    } else {
        agent.lifecycle_label()
    }
}

/// What a `/gam` row is doing, in the sidebar's words. A generator whose turn
/// ended reads Done, and the generic label for that ("done · press m to read
/// the diff") told you to merge work the reviewer was still judging; a reviewer
/// between packets read "completed" as if the pair were over.
fn gam_row_status_label(agent: &AgentRun) -> Option<&'static str> {
    let gam = agent.gam.as_ref()?;
    let adversarial = gam.role == crate::GamRole::Adversarial;
    if agent.status == AgentStatus::Running {
        return None;
    }
    Some(match &gam.phase {
        crate::GamPhase::Settled(crate::GamOutcome::Accepted) => {
            if adversarial {
                "reviewer \u{b7} accepted"
            } else {
                // Accepted work is ordinary finished work: the merge hint is
                // exactly right for it.
                return None;
            }
        }
        crate::GamPhase::Settled(crate::GamOutcome::Escalated(_)) => "gam \u{b7} needs you",
        crate::GamPhase::Settled(crate::GamOutcome::Stalled(_)) => "gam \u{b7} stalled",
        crate::GamPhase::Settled(crate::GamOutcome::Halted(_)) => {
            if adversarial {
                "reviewer \u{b7} stopped it"
            } else {
                "stopped by reviewer"
            }
        }
        crate::GamPhase::Settled(crate::GamOutcome::StoppedByUser) => {
            if adversarial {
                "reviewer \u{b7} idle"
            } else {
                return None;
            }
        }
        _ if agent.status != AgentStatus::Done => return None,
        crate::GamPhase::AwaitingVerdict if !adversarial => "gam \u{b7} under review",
        _ if adversarial => "reviewer \u{b7} standing by",
        _ => return None,
    })
}

fn integration_detail(agent: &AgentRun) -> Option<String> {
    if agent.integration.phase == IntegrationPhase::Pending
        && agent.integration.bookmark.is_none()
        && agent.integration.merge_change_id.is_none()
    {
        return None;
    }
    let bookmark = agent.integration.bookmark.as_deref().unwrap_or("HEAD");
    let revision = agent
        .integration
        .git_commit
        .as_deref()
        .or(agent.integration.merge_change_id.as_deref())
        .map(short_hash)
        .unwrap_or_else(|| "pending".to_string());
    let remote = if agent.integration.pushed {
        "remote contains commit"
    } else {
        "not pushed"
    };
    Some(format!("{bookmark} @ {revision} · {remote}"))
}

fn delivery_detail(agent: &AgentRun) -> Option<String> {
    if !agent.delivery.required {
        return None;
    }
    let target = agent
        .delivery
        .target
        .as_deref()
        .unwrap_or("target not recorded");
    let revision = agent
        .delivery
        .revision
        .as_deref()
        .map(short_hash)
        .unwrap_or_else(|| "revision not recorded".to_string());
    Some(format!(
        "delivery {} · {} · {} · {} check{}",
        agent.delivery.status.as_str(),
        target,
        revision,
        agent.delivery.checks.len(),
        if agent.delivery.checks.len() == 1 {
            ""
        } else {
            "s"
        }
    ))
}

/// A finished run that still has a workspace to integrate: it sits in the review
/// bucket until merged (m / M for manual runs; automatic for DAG nodes), and hard-dependent nodes stay gated
/// on it. Main, one-off, and planner runs never merge.
pub(crate) fn agent_awaits_merge(agent: &AgentRun) -> bool {
    agent.status == AgentStatus::Done
        && !agent.is_main()
        && !agent.is_oneoff()
        && !matches!(agent.mode, AgentMode::Plan | AgentMode::RudderPlan)
        && agent.has_merge_source()
}

pub(crate) fn agent_status_style(agent: &AgentRun) -> Style {
    let p = pal();
    if agent.needs_permission
        || agent.needs_user_input
        || matches!(
            agent.wait_signal,
            Some(WaitSignal::Idle) | Some(WaitSignal::Interrupted)
        )
    {
        Style::default().fg(p.waiting)
    } else if agent.turn_error().is_some() {
        Style::default().fg(p.failed)
    } else if agent.has_merge_conflict() {
        Style::default().fg(p.failed)
    } else if agent.status == AgentStatus::Done
        && agent.delivery.required
        && matches!(
            agent.delivery.status,
            DeliveryStatus::Blocked | DeliveryStatus::Failed
        )
    {
        Style::default().fg(p.failed)
    } else if agent.status == AgentStatus::Done
        && agent.delivery.required
        && !agent.delivery.is_verified()
    {
        Style::default().fg(p.waiting)
    } else {
        status_style(agent.status)
    }
}

pub(crate) fn is_cloud_agent(agent: &AgentRun) -> bool {
    agent.task == "cloud"
        || agent.task.starts_with("cloud ")
        || agent.current_prompt == "cloud"
        || agent.current_prompt.starts_with("cloud ")
}

pub(crate) fn cloud_sail_id(agent: &AgentRun) -> Option<&str> {
    agent
        .task
        .strip_prefix("cloud sail ")
        .and_then(|rest| rest.split_once(" · ").map(|(id, _)| id).or(Some(rest)))
        .filter(|id| !id.trim().is_empty())
}

/// A CLI pane moving rows off this dashboard (to the home machine, or the
/// older snapshot-workspace and per-agent sail handoffs). Its
/// `review_source_ids` are the rows it carries.
pub(crate) fn is_cloud_move_pane(agent: &AgentRun) -> bool {
    agent.task.starts_with(crate::HOME_MOVE_LABEL_PREFIX)
        || agent.task.starts_with("cloud migrate ")
        || agent.task.starts_with("cloud onload ")
}

pub(crate) fn is_cloud_owned_agent(agent: &AgentRun) -> bool {
    agent.status == AgentStatus::Migrated || cloud_sail_id(agent).is_some()
}

// status_color now lives in theme.rs (re-exported via the crate root).

/// Smallest tile god's view draws: below this a tile is a title bar and
/// nothing you can read, so extra agents go to the next page instead.
pub(crate) const GODS_VIEW_MIN_TILE_WIDTH: u16 = 28;
pub(crate) const GODS_VIEW_MIN_TILE_HEIGHT: u16 = 7;

/// Columns and rows for `count` tiles in `width` x `height`. Picks the
/// layout whose tiles are closest to square on screen (a cell is about twice
/// as tall as it is wide) among those that fit every tile at the minimum size;
/// when none does, the most tiles that fit, and the rest are paged.
pub(crate) fn gods_view_grid(count: usize, width: u16, height: u16) -> (usize, usize) {
    let count = count.max(1);
    let max_cols = ((width / GODS_VIEW_MIN_TILE_WIDTH) as usize).max(1);
    let max_rows = ((height / GODS_VIEW_MIN_TILE_HEIGHT) as usize).max(1);
    let mut best: Option<(f64, usize, usize)> = None;
    for cols in 1..=max_cols.min(count) {
        let rows = count.div_ceil(cols);
        if rows > max_rows {
            continue;
        }
        let tile_w = width as f64 / cols as f64;
        let tile_h = height as f64 / rows as f64;
        let skew = (tile_w / (2.0 * tile_h)).ln().abs();
        // Empty slots are wasted screen: 4 agents tile 2x2, not 3x2 with a hole.
        let empty = (cols * rows - count) as f64 / (cols * rows) as f64;
        let score = skew + 1.5 * empty;
        if best.is_none_or(|(best_score, _, _)| score < best_score) {
            best = Some((score, cols, rows));
        }
    }
    match best {
        Some((_, cols, rows)) => (cols, rows),
        None => (max_cols, max_rows),
    }
}

/// The short status a tile carries, in the agents list's words.
fn gods_view_status(agent: &AgentRun) -> &'static str {
    if agent.needs_permission
        || agent.needs_user_input
        || matches!(
            agent.wait_signal,
            Some(WaitSignal::Idle) | Some(WaitSignal::Interrupted)
        )
    {
        "needs you"
    } else if agent.turn_error().is_some() {
        "turn failed"
    } else {
        match agent.status {
            AgentStatus::Running => "working",
            AgentStatus::Done => "done",
            AgentStatus::Paused => "paused",
            other => other.as_str(),
        }
    }
}

pub(crate) fn render_gods_view(frame: &mut Frame<'_>, area: Rect, app: &mut App) {
    let tiles = app.gods_view_agents();
    app.gods_view_tile_map.clear();
    if area.height < 2 || area.width < 4 {
        return;
    }
    let header_area = Rect { height: 1, ..area };
    let grid_area = Rect {
        y: area.y + 1,
        height: area.height - 1,
        ..area
    };
    let (cols, rows) = gods_view_grid(tiles.len(), grid_area.width, grid_area.height);
    let per_page = (cols * rows).max(1);
    app.gods_view_cols = cols;
    app.gods_view_per_page = per_page;
    if !tiles.is_empty() {
        app.gods_view_cursor = app.gods_view_cursor.min(tiles.len() - 1);
    }
    let page = app.gods_view_cursor / per_page;
    let pages = tiles.len().div_ceil(per_page).max(1);

    let mut header = format!(
        " god's view · {} agent{}",
        tiles.len(),
        if tiles.len() == 1 { "" } else { "s" }
    );
    if pages > 1 {
        header.push_str(&format!(" · page {}/{}", page + 1, pages));
    }
    header.push_str(" · arrows or h/j/k/l move · Enter or click focuses · Esc or ⌥g closes");
    frame.render_widget(
        Paragraph::new(Line::from(Span::styled(header, accent_style(true)))).style(app_style()),
        header_area,
    );

    if tiles.is_empty() {
        frame.render_widget(
            Paragraph::new(Line::from(Span::styled("No live agents.", muted_style(true))))
                .style(app_style()),
            grid_area,
        );
        return;
    }

    let row_rects = Layout::default()
        .direction(Direction::Vertical)
        .constraints(vec![Constraint::Ratio(1, rows as u32); rows])
        .split(grid_area);
    let page_tiles: Vec<usize> = tiles
        .iter()
        .copied()
        .skip(page * per_page)
        .take(per_page)
        .collect();
    for (slot, &agent_index) in page_tiles.iter().enumerate() {
        let row = slot / cols;
        let col = slot % cols;
        let Some(&row_rect) = row_rects.get(row) else {
            break;
        };
        let col_rects = Layout::default()
            .direction(Direction::Horizontal)
            .constraints(vec![Constraint::Ratio(1, cols as u32); cols])
            .split(row_rect);
        let Some(&tile) = col_rects.get(col) else {
            break;
        };
        let highlighted = page * per_page + slot == app.gods_view_cursor;
        render_gods_view_tile(frame, tile, app, agent_index, highlighted);
        app.gods_view_tile_map.push((tile, agent_index));
    }
}

fn render_gods_view_tile(
    frame: &mut Frame<'_>,
    tile: Rect,
    app: &mut App,
    index: usize,
    highlighted: bool,
) {
    let Some(run) = app.agents.get(index) else {
        return;
    };
    let id = worker_title_id(app, run);
    let label = if run.task_summary.trim().is_empty() {
        summarize_task(&run.task)
    } else {
        run.task_summary.clone()
    };
    let model = if run.model.trim().is_empty() {
        run.backend.as_str().to_string()
    } else {
        format!("{}/{}", run.backend.as_str(), run.model)
    };
    let status = gods_view_status(run);
    let status_style = agent_status_style(run);
    // Room for the label once the fixed parts and the border corners are paid.
    let fixed = id.chars().count() + model.chars().count() + status.chars().count() + 12;
    let room = (tile.width as usize).saturating_sub(fixed).max(6);
    let title_style = if highlighted {
        Style::default().fg(PAPER).bg(FOCUS_COLOR)
    } else {
        pane_text_style(true)
    };
    let title = Line::from(vec![
        Span::styled(
            format!(" {id} · {} ", truncate_chars(&label, room)),
            title_style,
        ),
        Span::styled(format!("{model} "), model_style(true)),
        Span::styled(format!("· {status} "), status_style),
    ]);
    let block = Block::default()
        .title(title)
        .borders(Borders::ALL)
        .border_type(if highlighted {
            ratatui::widgets::BorderType::Thick
        } else {
            ratatui::widgets::BorderType::Plain
        })
        .border_style(Style::default().fg(if highlighted {
            FOCUS_COLOR
        } else {
            INACTIVE_COLOR
        }))
        .style(app_style());
    let inner = block.inner(tile);
    let height = inner.height as usize;
    let width = inner.width as usize;

    // A cropped window of the pane exactly as the agent drew it. The PTY is
    // never resized to the tile: that makes the agent repaint and drops the
    // region scrollback (AGENTS.md §7).
    let terminal = app
        .agents
        .get_mut(index)
        .and_then(|run| run.terminal.as_mut());
    let lines: Vec<Line<'static>> = match terminal {
        Some(terminal) => terminal
            .styled_tail_window_snapshot(height)
            .into_iter()
            .map(|mut cells| {
                cells.truncate(width);
                styled_terminal_line(cells, None, None)
            })
            .collect(),
        None => {
            let note = app
                .agents
                .get(index)
                .and_then(|run| run.worker_exit_note.clone())
                .unwrap_or_else(|| "No live terminal yet.".to_string());
            vec![Line::from(Span::styled(note, muted_style(true)))]
        }
    };
    frame.render_widget(Paragraph::new(lines).style(app_style()).block(block), tile);
}

// --- 4.2 chrome: header, footer, notice pill, keys overlay --------------------

/// The Rudder version this binary was built with: the repo's package.json,
/// which `npm version` bumps before the release build.
fn rudder_version() -> Option<&'static str> {
    const PACKAGE_JSON: &str = include_str!("../../package.json");
    static VERSION: std::sync::OnceLock<Option<String>> = std::sync::OnceLock::new();
    VERSION
        .get_or_init(|| {
            serde_json::from_str::<serde_json::Value>(PACKAGE_JSON)
                .ok()?
                .get("version")?
                .as_str()
                .map(str::to_string)
        })
        .as_deref()
}

/// Display width of a run of spans.
fn spans_width(spans: &[Span<'_>]) -> usize {
    spans.iter().map(Span::width).sum()
}

/// `left` flush left and `right` flush right on one row; the right side is
/// dropped when both do not fit, never overlapped.
fn split_row<'a>(mut left: Vec<Span<'a>>, right: Vec<Span<'a>>, width: usize) -> Line<'a> {
    let lw = spans_width(&left);
    let rw = spans_width(&right);
    if rw > 0 && lw + rw + 2 <= width {
        left.push(Span::raw(" ".repeat(width - lw - rw)));
        left.extend(right);
    }
    Line::from(left)
}

/// Counts of the fleet by what it needs from you, for the header.
fn fleet_counts(app: &App) -> (usize, usize, usize) {
    let mut running = 0;
    let mut waiting = 0;
    let mut failed = 0;
    for agent in app.agents.iter().filter(|a| !a.is_main() || a.terminal.is_some()) {
        if agent_waits_for_you(agent) {
            waiting += 1;
        } else if agent.status == AgentStatus::Running {
            running += 1;
        } else if agent.status == AgentStatus::Failed {
            failed += 1;
        }
    }
    (running, waiting, failed)
}

/// True when the agent is blocked on the user: a question, a permission, an
/// interrupted turn or a conflict only the user can settle.
pub(crate) fn agent_waits_for_you(agent: &AgentRun) -> bool {
    agent.needs_permission
        || agent.needs_user_input
        || matches!(
            agent.wait_signal,
            Some(WaitSignal::Idle) | Some(WaitSignal::Interrupted)
        )
        || (agent.has_merge_conflict() && agent.status == AgentStatus::Done)
}

/// True while the agent is visibly busy, so its card gets the spinner.
pub(crate) fn agent_is_busy(agent: &AgentRun) -> bool {
    (agent.status == AgentStatus::Running
        && !agent_waits_for_you(agent)
        && agent.turn_error().is_none())
        || agent.integration.phase == IntegrationPhase::Integrating
}

pub(crate) fn render_header(frame: &mut Frame<'_>, area: Rect, app: &App) {
    if area.height == 0 {
        return;
    }
    let p = pal();
    let project = app
        .cwd
        .file_name()
        .map(|name| name.to_string_lossy().into_owned())
        .unwrap_or_else(|| home_relative(&app.cwd));
    let reveal = reveal_progress(app, Instant::now());
    let mut left = vec![Span::raw(" ")];
    match reveal {
        Some(progress) => {
            // Startup: the wordmark draws in letter by letter under a light
            // sweep; the rest of the row fades in behind it.
            let shown = ((progress / 0.55).min(1.0) * 6.0).ceil() as usize;
            let frame = (progress * 14.0) as usize;
            let mut word = shimmer_spans(&"rudder"[..shown.min(6)], p.accent, frame);
            for span in word.iter_mut() {
                span.style = span.style.add_modifier(Modifier::BOLD);
            }
            left.extend(word);
            left.push(Span::raw(" ".repeat(6 - shown.min(6))));
        }
        None => left.push(Span::styled(
            "rudder",
            Style::default().fg(p.accent).add_modifier(Modifier::BOLD),
        )),
    }
    let word_end = left.len();
    left.push(Span::raw("  "));
    left.push(Span::styled(project, app_style()));
    if let Some(branch) = app.branch.as_deref() {
        left.push(Span::styled(format!("  \u{2387} {branch}"), Style::default().fg(p.muted)));
    }
    if let Some(version) = rudder_version() {
        left.push(Span::styled(format!("  {version}"), Style::default().fg(p.muted)));
    }
    if is_cloud_worker_session() {
        let connected = app.cloud_connected;
        left.push(Span::styled(
            if connected { "  \u{2601} cloud connected" } else { "  \u{2601} cloud offline" },
            Style::default().fg(if connected { p.accent } else { p.muted }),
        ));
    }
    let mut right: Vec<Span<'static>> = Vec::new();
    if let Some((_, latest)) = read_update_notice() {
        right.push(Span::styled(
            format!("\u{2191} {latest} available  "),
            Style::default().fg(p.accent),
        ));
    }
    let (running, waiting, failed) = fleet_counts(app);
    for (count, label, color) in [
        (running, "running", p.running),
        (waiting, "need you", p.waiting),
        (failed, "failed", p.failed),
    ] {
        if count > 0 {
            right.push(Span::styled(BADGE, Style::default().fg(color)));
            right.push(Span::styled(format!(" {count} {label}  "), Style::default().fg(p.muted)));
        }
    }
    let mut row = split_row(left, right, area.width as usize);
    if let Some(progress) = reveal {
        let alpha = ((progress - 0.3) / 0.5).clamp(0.0, 1.0);
        for span in row.spans.iter_mut().skip(word_end) {
            let fg = span.style.fg.unwrap_or_else(ink);
            span.style = span.style.fg(blend(canvas(), fg, alpha));
        }
    }
    frame.render_widget(Paragraph::new(row).style(app_style()), area);
}

/// The four or five keys that matter in the current context.
pub(crate) fn footer_keys(app: &App) -> Vec<(&'static str, &'static str)> {
    let mut keys = context_footer_keys(app);
    // A newer image is ready on the home machine: offer it before the last
    // hint, wherever the chord works and the footer is not showing typing keys.
    let room = match app.focus {
        FocusPane::Agents => app.drawer_cursor.is_none() && app.drawer.is_none(),
        FocusPane::Worker => true,
        FocusPane::Task => app.task_input.is_empty(),
        FocusPane::Diff => false,
    };
    if room && !app.show_keys && app.rename_input.is_none() && app.home_update_offered() {
        keys.insert(keys.len().saturating_sub(1), (HOME_UPDATE_KEY_LABEL, "update"));
    }
    keys
}

fn context_footer_keys(app: &App) -> Vec<(&'static str, &'static str)> {
    if app.show_keys {
        return vec![("esc", "close")];
    }
    if app.rename_input.is_some() {
        return vec![("enter", "save"), ("esc", "cancel")];
    }
    match app.focus {
        FocusPane::Agents => {
            if app.drawer_cursor.is_some() || app.drawer.is_some() {
                return vec![("enter", "open"), ("j/k", "move"), ("esc", "close"), ("?", "keys")];
            }
            match app.agents.get(app.selected_agent) {
                Some(agent) if agent.status == AgentStatus::Running => vec![
                    ("enter", "open"),
                    ("x", "stop"),
                    ("b", "branch"),
                    ("r", "rename"),
                    ("?", "keys"),
                ],
                Some(agent) if agent.status == AgentStatus::Done => vec![
                    ("enter", "open"),
                    ("m", "merge"),
                    ("b", "branch"),
                    ("dd", "delete"),
                    ("?", "keys"),
                ],
                Some(_) => vec![
                    ("enter", "resume"),
                    ("b", "branch"),
                    ("dd", "delete"),
                    ("g", "nest"),
                    ("?", "keys"),
                ],
                None => vec![
                    ("\u{2325}3", "prompt"),
                    ("o", "web ui"),
                    ("P", "model"),
                    ("?", "keys"),
                ],
            }
        }
        FocusPane::Worker => vec![
            ("\u{2325}1", "agents"),
            ("\u{2325}3", "prompt"),
            ("\u{2325}d", "diff"),
            ("\u{2325}h", "full screen"),
            ("^W", "pane keys"),
        ],
        FocusPane::Diff => vec![
            ("j/k", "scroll"),
            ("\u{2325}d", "close"),
            ("\u{2325}2", "terminal"),
        ],
        FocusPane::Task => {
            if app.task_input.is_empty() {
                vec![
                    ("enter", "run"),
                    ("/", "commands"),
                    ("\u{2191}", "history"),
                    ("\u{2325}1", "agents"),
                    ("?", "keys"),
                ]
            } else if app.task_input.contains('\n') {
                vec![
                    ("enter", "runs it"),
                    ("\u{2325}\u{23ce} \u{21e7}\u{23ce} ^J", "adds a line"),
                    ("\u{2191}\u{2193}", "move between lines"),
                ]
            } else {
                vec![
                    ("enter", "run"),
                    ("\u{2325}\u{23ce}", "new line"),
                    ("\u{2325}1", "agents"),
                ]
            }
        }
    }
}

/// Footer key hints: the key in the terminal's own text color, what it does in
/// muted. Only the separator dots are faint.
fn key_spans(keys: &[(&str, &str)]) -> Vec<Span<'static>> {
    let p = pal();
    let mut spans = vec![Span::raw(" ")];
    for (index, (key, label)) in keys.iter().enumerate() {
        if index > 0 {
            spans.push(Span::styled("  \u{b7}  ", Style::default().fg(p.faint)));
        }
        spans.push(Span::styled((*key).to_string(), Style::default().fg(term_fg())));
        spans.push(Span::styled(format!(" {label}"), Style::default().fg(p.muted)));
    }
    spans
}

/// How long a notice shows as a pill before it settles into the footer.
pub(crate) const NOTICE_PILL_FOR: Duration = Duration::from_secs(4);
pub(crate) const NOTICE_ERROR_PILL_FOR: Duration = Duration::from_secs(10);

/// A notice that ends in an ellipsis is work in progress ("refining the
/// plan…"), so its pill carries the spinner.
pub(crate) fn notice_in_progress(text: &str) -> bool {
    let text = text.trim_end();
    text.ends_with('\u{2026}') || text.ends_with("...")
}

/// True while the current notice still shows as a pill. Remembers when each
/// distinct notice first appeared.
pub(crate) fn notice_pill_visible(app: &mut App, now: Instant) -> bool {
    let Some(text) = app.notice.as_deref() else {
        app.notice_seen = None;
        return false;
    };
    let fresh = match &app.notice_seen {
        Some((seen, _)) => seen != text,
        None => true,
    };
    if fresh {
        app.notice_seen = Some((text.to_string(), now));
    }
    let since = app.notice_seen.as_ref().map(|(_, at)| *at).unwrap_or(now);
    let kind = notice_kind(text);
    let ttl = if kind == NoticeKind::Error { NOTICE_ERROR_PILL_FOR } else { NOTICE_PILL_FOR };
    // A pending confirmation ("press d again") and a long notice (a /help
    // listing) are meant to be acted on or read: they stay up until Esc or the
    // next notice instead of shrinking into the footer.
    notice_in_progress(text)
        || kind == NoticeKind::Confirm
        || text.chars().count() > 140
        || now.duration_since(since) < ttl
}

pub(crate) fn render_footer(frame: &mut Frame<'_>, area: Rect, app: &mut App) {
    if area.height == 0 {
        return;
    }
    let keys = key_spans(&footer_keys(app));
    // A notice past its pill time stays readable here until Esc or the next one.
    let settled = match app.notice.clone() {
        Some(text) if !notice_pill_visible(app, Instant::now()) => {
            let room = (area.width as usize).saturating_sub(spans_width(&keys) + 4);
            vec![
                Span::styled(truncate_chars(&text, room), notice_style(&text)),
                Span::raw(" "),
            ]
        }
        _ => Vec::new(),
    };
    frame.render_widget(
        Paragraph::new(split_row(keys, settled, area.width as usize)).style(app_style()),
        area,
    );
}

/// The most rows of text a notice pill shows. More would cover the panes.
pub(crate) const NOTICE_PILL_MAX_ROWS: usize = 3;

/// A notice wrapped to `width` columns, at most `max_rows` rows. When rows
/// are left out, the last row shown ends in "…" and still fits the width.
pub(crate) fn notice_pill_rows(text: &str, width: usize, max_rows: usize) -> Vec<String> {
    let width = width.max(2);
    let mut rows = wrap_text(text, width as u16);
    if rows.len() > max_rows.max(1) {
        rows.truncate(max_rows.max(1));
        if let Some(last) = rows.last_mut() {
            let mut kept: String = last.chars().take(width - 1).collect();
            // Room for "…" cut into a word: cut at the word before it instead,
            // when one ends near the edge.
            if last.chars().count() > width - 1 {
                if let Some(at) = kept.rfind(' ').filter(|at| *at >= kept.len() / 2) {
                    kept.truncate(at);
                }
            }
            *last = format!("{}\u{2026}", kept.trim_end());
        }
    }
    rows
}

/// The notice as a small rounded pill at the bottom right of `area`.
pub(crate) fn render_notice_pill(frame: &mut Frame<'_>, area: Rect, app: &mut App) {
    if !notice_pill_visible(app, Instant::now()) || area.height < 3 || area.width < 16 {
        return;
    }
    let Some(text) = app.notice.clone() else {
        return;
    };
    let kind = notice_kind(&text);
    let colors = notice_pill_colors(kind);
    let glyph = if notice_in_progress(&text) {
        app.spinner_glyph()
    } else {
        match kind {
            NoticeKind::Error => "\u{2715}",
            NoticeKind::Confirm => "!",
            NoticeKind::Info => "\u{2713}",
        }
    };
    // The pill is as wide as its text: a short notice stays small, a long one
    // (a /help listing, an error with its cause) widens and wraps only when
    // it must, so a sentence is not broken across rows needlessly. At full
    // width it keeps two columns clear on each side.
    let max_w = (area.width as usize).saturating_sub(8).clamp(12, 120);
    // A notice is a glance, not a page: a few rows, the rest cut with "…".
    // /help is the exception, a listing the user asked to read.
    let max_rows = (area.height as usize).saturating_sub(3).max(1).min(
        if text == dashboard_help() { usize::MAX } else { NOTICE_PILL_MAX_ROWS },
    );
    let rows = notice_pill_rows(&text, max_w.saturating_sub(2), max_rows);
    let lines: Vec<Line<'static>> = rows
        .into_iter()
        .enumerate()
        .map(|(index, row)| {
            Line::from(vec![
                Span::styled(
                    if index == 0 { format!("{glyph} ") } else { "  ".to_string() },
                    Style::default().fg(colors.glyph),
                ),
                Span::styled(row, colors.text),
            ])
        })
        .collect();
    let inner_w = lines.iter().map(Line::width).max().unwrap_or(0);
    let w = (inner_w as u16 + 4).min(area.width);
    let h = (lines.len() as u16 + 2).min(area.height);
    // Bottom right, just above the prompt: the corner a toast is expected in,
    // clear of pane titles and the top of whatever the pane is showing.
    let rect = Rect {
        x: area.right().saturating_sub(w + 2).max(area.x),
        y: area.bottom().saturating_sub(h + 1).max(area.y),
        width: w,
        height: h,
    };
    frame.render_widget(Clear, rect);
    frame.render_widget(
        Paragraph::new(lines).style(app_style()).block(
            Block::default()
                .borders(Borders::ALL)
                .border_type(BorderType::Rounded)
                .border_style(Style::default().fg(colors.border))
                .padding(ratatui::widgets::Padding::horizontal(1))
                .style(app_style()),
        ),
        rect,
    );
}

/// Every key, grouped. `?` opens it from the agents list or an empty prompt.
pub(crate) const KEYS_OVERLAY: &[(&str, &[(&str, &str)])] = &[
    (
        "agents",
        &[
            ("j k", "move"),
            ("enter", "open or resume"),
            ("m", "merge"),
            ("b", "branch"),
            ("x", "stop"),
            ("dd", "delete"),
            ("r", "rename"),
            ("u", "undo merge"),
            ("cc", "clear merged"),
            ("g", "nest view"),
            ("v", "plan review"),
            ("P", "model"),
            ("o", "web ui"),
            ("U", "usage"),
            ("H", "move all home"),
            ("q", "quit"),
        ],
    ),
    (
        "anywhere",
        &[
            ("\u{2325}1 2 3 4", "agents, terminal, prompt, diff"),
            ("^W", "pane keys, then a key"),
            ("\u{2325}[ ]", "previous, next agent"),
            ("\u{2325}j k", "scroll"),
            ("\u{2325}v b", "previous, next message"),
            ("\u{2325}h", "hide panes"),
            ("\u{2325}d", "diff"),
            ("\u{2325}g", "god's view"),
            ("esc", "dismiss notice"),
            ("^C", "quit"),
        ],
    ),
    (
        "prompt",
        &[
            ("enter", "run"),
            ("\u{2325}\u{23ce} ^J", "new line"),
            ("/", "commands"),
            ("\u{2191} \u{2193}", "history"),
            ("?", "keys, when empty"),
        ],
    ),
];

pub(crate) fn render_keys_overlay(frame: &mut Frame<'_>, area: Rect, app: &App) {
    if !app.show_keys {
        return;
    }
    let p = pal();
    let group_lines = |group: &(&str, &[(&str, &str)])| -> Vec<Line<'static>> {
        let mut lines = vec![Line::from(Span::styled(
            group.0.to_string(),
            Style::default().fg(p.accent).add_modifier(Modifier::BOLD),
        ))];
        for (key, label) in group.1 {
            let pad = 10usize.saturating_sub(Span::raw(*key).width());
            lines.push(Line::from(vec![
                Span::styled(format!("{key}{}", " ".repeat(pad)), app_style()),
                Span::styled((*label).to_string(), Style::default().fg(p.muted)),
            ]));
        }
        lines.push(Line::default());
        lines
    };
    let mut left: Vec<Line<'static>> = group_lines(&KEYS_OVERLAY[0]);
    let mut right: Vec<Line<'static>> = Vec::new();
    for group in &KEYS_OVERLAY[1..] {
        right.extend(group_lines(group));
    }
    for column in [&mut left, &mut right] {
        while column.last().is_some_and(|line| line.width() == 0) {
            column.pop();
        }
    }
    let col_w = 30usize;
    let right_w = 42usize;
    let two_columns = area.width as usize >= col_w + right_w + 6;
    let mut lines: Vec<Line<'static>> = if two_columns {
        let rows = left.len().max(right.len());
        (0..rows)
            .map(|row| {
                let mut spans = left.get(row).cloned().unwrap_or_default().spans;
                let w = spans_width(&spans);
                spans.push(Span::raw(" ".repeat(col_w.saturating_sub(w))));
                spans.extend(right.get(row).cloned().unwrap_or_default().spans);
                Line::from(spans)
            })
            .collect()
    } else {
        left.into_iter().chain(right).collect()
    };
    while lines.last().is_some_and(|line| line.width() == 0) {
        lines.pop();
    }
    let width = if two_columns {
        (col_w + right_w + 4) as u16
    } else {
        right_w as u16 + 4
    };
    let height = lines.len() as u16 + 2;
    let rect = centered_modal(area, width, height);
    frame.render_widget(Clear, rect);
    frame.render_widget(
        Paragraph::new(lines).style(app_style()).block(
            Block::default()
                .title(Line::from(Span::styled(" keys ", Style::default().fg(p.muted))))
                .title_bottom(
                    Line::from(Span::styled(" esc close ", Style::default().fg(p.muted)))
                        .right_aligned(),
                )
                .borders(Borders::ALL)
                .border_type(BorderType::Rounded)
                .border_style(Style::default().fg(p.accent))
                .padding(ratatui::widgets::Padding::horizontal(1))
                .style(app_style()),
        ),
        rect,
    );
}

/// The card list before the first agent: what to do, in two quiet lines.
fn push_empty_state(lines: &mut Vec<Line<'_>>, focused: bool) {
    let p = pal();
    lines.push(Line::default());
    lines.push(Line::from(Span::styled(
        "  no agents yet",
        Style::default().fg(if focused { p.muted } else { p.faint }),
    )));
    lines.push(Line::from(Span::styled(
        "  describe a task below,",
        Style::default().fg(p.faint),
    )));
    lines.push(Line::from(Span::styled(
        "  or /main for this checkout",
        Style::default().fg(p.faint),
    )));
}

// --- Motion: startup reveal, working shimmer, state-change flash --------------
//
// Everything here is a pure function of the clock and App state, drawn by the
// normal render pass. The App loop asks for a frame (at most every ANIM_FRAME)
// only while something is actually moving; an idle dashboard draws nothing.

/// How long the startup reveal runs. Any key or click skips it.
pub(crate) const REVEAL_FOR: Duration = Duration::from_millis(600);
/// How long a card glows after its agent changes state.
pub(crate) const FLASH_FOR: Duration = Duration::from_millis(700);
/// Frame interval while animating: about 11 fps.
pub(crate) const ANIM_FRAME: Duration = Duration::from_millis(90);

/// What a card is showing, for color and change detection.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum CardKind {
    Busy,
    Waiting,
    Done,
    Merged,
    Failed,
    Idle,
}

pub(crate) fn card_kind(agent: &AgentRun) -> CardKind {
    if agent_waits_for_you(agent) {
        CardKind::Waiting
    } else if matches!(agent.status, AgentStatus::Failed | AgentStatus::Orphaned)
        || agent.turn_error().is_some()
    {
        CardKind::Failed
    } else if agent_is_busy(agent) {
        CardKind::Busy
    } else if agent.status == AgentStatus::Merged {
        CardKind::Merged
    } else if agent.status == AgentStatus::Done {
        CardKind::Done
    } else {
        CardKind::Idle
    }
}

pub(crate) fn card_kind_color(kind: CardKind) -> Color {
    let p = pal();
    match kind {
        CardKind::Busy => p.running,
        CardKind::Waiting => p.waiting,
        CardKind::Done => p.done,
        CardKind::Merged => p.merged,
        CardKind::Failed => p.failed,
        CardKind::Idle => p.faint,
    }
}

/// Startup reveal progress from 0 to 1, or None once it is over or skipped.
pub(crate) fn reveal_progress(app: &App, now: Instant) -> Option<f32> {
    let started = app.reveal_started?;
    let elapsed = now.duration_since(started);
    (elapsed < REVEAL_FOR).then(|| elapsed.as_secs_f32() / REVEAL_FOR.as_secs_f32())
}

/// Fade-in for the card drawn `ordinal`-th during the reveal: cards arrive one
/// after another, each over 150ms.
fn reveal_alpha(progress: Option<f32>, ordinal: usize) -> Option<f32> {
    let progress = progress?;
    let ms = progress * REVEAL_FOR.as_millis() as f32;
    let start = 120.0 + (ordinal.min(6) as f32) * 55.0;
    Some(((ms - start) / 150.0).clamp(0.0, 1.0))
}

/// Remember each agent's card kind and start a flash when it moves into a
/// state worth noticing (finished, merged, needs you, failed). The first sight
/// of an agent never flashes, so a dashboard reload is calm.
pub(crate) fn note_card_changes(app: &mut App, now: Instant) {
    let mut live = std::collections::HashSet::new();
    for agent in &app.agents {
        live.insert(agent.id.as_str());
        let kind = card_kind(agent);
        if let Some(previous) = app.card_kinds.insert(agent.id.clone(), kind) {
            if previous != kind
                && matches!(
                    kind,
                    CardKind::Waiting | CardKind::Done | CardKind::Merged | CardKind::Failed
                )
            {
                app.card_flashes
                    .insert(agent.id.clone(), (now, card_kind_color(kind)));
            }
        }
    }
    app.card_kinds.retain(|id, _| live.contains(id.as_str()));
    app.card_flashes
        .retain(|id, (at, _)| live.contains(id.as_str()) && now.duration_since(*at) < FLASH_FOR);
}

/// The flash on a card right now: its color and strength (1 at the start,
/// easing out to 0).
fn card_flash(app: &App, id: &str, now: Instant) -> Option<(Color, f32)> {
    let (at, color) = app.card_flashes.get(id)?;
    let age = now.duration_since(*at);
    if age >= FLASH_FOR {
        return None;
    }
    let left = 1.0 - age.as_secs_f32() / FLASH_FOR.as_secs_f32();
    Some((*color, left * left))
}

/// True while any reveal or flash still needs frames.
pub(crate) fn motion_pending(app: &App, now: Instant) -> bool {
    let tail = FLASH_FOR + ANIM_FRAME * 2;
    app.reveal_started
        .is_some_and(|started| now.duration_since(started) < REVEAL_FOR + ANIM_FRAME * 2)
        || app
            .card_flashes
            .values()
            .any(|(at, _)| now.duration_since(*at) < tail)
}

/// One step of the ⌥h move: about 30 fps, the App loop's own 33ms tick, so the
/// move needs no extra wakeups. The panes only move on this grid, so a frame
/// drawn between steps for another reason (agent output) shows the same rects
/// and sends no more than that output would (this matters over Rudder Cloud).
/// A step repaints the moving pane; 60 fps would double those bytes for
/// little gain, since motion is in whole cells anyway.
pub(crate) const SOLO_STEP: Duration = Duration::from_millis(33);
/// Steps in a whole move, split to full screen or back: about 200ms. A move
/// reversed part way takes its share.
pub(crate) const SOLO_STEPS: u32 = 7;

/// The ⌥h move between the split and full screen. `solo_pane` and focus flip
/// at once so keys go where they will land; only the drawing moves.
#[derive(Clone, Copy, Debug, PartialEq)]
pub(crate) struct SoloTransition {
    pub(crate) started: Instant,
    /// How full screen the panes were drawn when the move began: 0 is the
    /// split, 1 is full screen. A reversal starts from where they were.
    pub(crate) from: f32,
    /// Heading to full screen (true) or back to the split.
    pub(crate) entering: bool,
    /// The pane that shrinks when leaving full screen: the worker or the diff.
    /// Entering, the pane is whatever full screen will show.
    pub(crate) pane: FocusPane,
    /// The step the App loop last asked a frame for.
    pub(crate) step_shown: u32,
}

impl SoloTransition {
    fn target(&self) -> f32 {
        if self.entering {
            1.0
        } else {
            0.0
        }
    }

    /// Steps this move takes; 1 means there is nothing to animate.
    pub(crate) fn steps(&self) -> u32 {
        ((self.target() - self.from).abs() * SOLO_STEPS as f32).round().max(1.0) as u32
    }

    /// The step on screen at `now`, or None once the move has landed: the last
    /// step is the resting layout itself, drawn by the normal path.
    pub(crate) fn step_at(&self, now: Instant) -> Option<u32> {
        let elapsed = now.saturating_duration_since(self.started).as_millis();
        let step = u32::try_from(elapsed / SOLO_STEP.as_millis()).unwrap_or(u32::MAX);
        (step.saturating_add(1) < self.steps()).then_some(step)
    }

    /// How full screen the panes are on `step`, eased out: quick to start,
    /// settling gently. The first frame already moves.
    pub(crate) fn fullness(&self, step: u32) -> f32 {
        let progress = (step.saturating_add(1) as f32 / self.steps() as f32).min(1.0);
        let eased = 1.0 - (1.0 - progress) * (1.0 - progress);
        self.from + (self.target() - self.from) * eased
    }
}

/// The ⌥h move to draw at `now`: which pane moves and how full screen it is.
/// None when nothing is moving, including once the move has landed.
pub(crate) fn solo_transition_at(app: &App, now: Instant) -> Option<(FocusPane, f32)> {
    let transition = app.solo_transition?;
    if transition.entering != app.solo_pane {
        return None;
    }
    let fullness = transition.fullness(transition.step_at(now)?);
    let pane = if app.solo_pane {
        app.solo_shown_pane()
    } else {
        transition.pane
    };
    let pane = if pane == FocusPane::Diff && !app.diff_panel.open {
        FocusPane::Worker
    } else {
        pane
    };
    Some((pane, fullness))
}

/// Move a view's ⌥h transition along. True when it needs a frame: a new step,
/// or the landing frame (the transition is then cleared).
pub(crate) fn step_solo_transition(slot: &mut Option<SoloTransition>, now: Instant) -> bool {
    let Some(transition) = slot.as_mut() else {
        return false;
    };
    match transition.step_at(now) {
        None => {
            *slot = None;
            true
        }
        Some(step) if step != transition.step_shown => {
            transition.step_shown = step;
            true
        }
        Some(_) => false,
    }
}

/// `text` with a bright band sweeping left to right across it, one step per
/// animation frame: the working state, alive without being loud.
pub(crate) fn shimmer_spans(text: &str, base: Color, frame: usize) -> Vec<Span<'static>> {
    let chars: Vec<char> = text.chars().collect();
    let len = chars.len() as isize;
    if len == 0 {
        return Vec::new();
    }
    const BAND: f32 = 3.5;
    let span = len + 10;
    let center = (frame as isize * 2).rem_euclid(span) - 5;
    let dim = blend(canvas(), base, 0.72);
    let bright = blend(base, ink(), 0.65);
    chars
        .into_iter()
        .enumerate()
        .map(|(index, ch)| {
            let distance = (index as isize - center).abs() as f32;
            let glow = (1.0 - distance / BAND).clamp(0.0, 1.0);
            Span::styled(ch.to_string(), Style::default().fg(blend(dim, bright, glow)))
        })
        .collect()
}

/// Fade a line in from the canvas: alpha 0 is invisible, 1 is the final look.
fn fade_line(line: &mut Line<'_>, alpha: f32) {
    if alpha >= 1.0 {
        return;
    }
    let canvas = canvas();
    let ink = ink();
    for span in line.spans.iter_mut() {
        let fg = span.style.fg.unwrap_or(ink);
        span.style = span.style.fg(blend(canvas, fg, alpha));
    }
}

/// Wash a line's background with `bg` across the full card width.
fn tint_line(line: &mut Line<'_>, bg: Color, width: usize) {
    let used = line.width();
    if used < width {
        line.spans.push(Span::raw(" ".repeat(width - used)));
    }
    for span in line.spans.iter_mut() {
        span.style = span.style.bg(bg);
    }
}

/// The flash and the reveal fade for one card's rows.
fn apply_card_motion(rows: &mut [Line<'_>], app: &App, id: &str, ordinal: usize, width: usize) {
    let now = Instant::now();
    if let Some((color, strength)) = card_flash(app, id, now) {
        let bg = blend(canvas(), color, 0.42 * strength);
        for row in rows.iter_mut() {
            tint_line(row, bg, width);
        }
    }
    if let Some(alpha) = reveal_alpha(reveal_progress(app, now), ordinal) {
        for row in rows.iter_mut() {
            fade_line(row, alpha);
        }
    }
}

/// Styled spans laid out on at most `max_rows` rows of `width` columns,
/// broken between words; the last row is clipped with an ellipsis.
fn wrap_spans<'a>(spans: Vec<Span<'a>>, width: usize, max_rows: usize) -> Vec<Vec<Span<'a>>> {
    let width = width.max(4);
    let mut rows: Vec<Vec<Span<'a>>> = vec![Vec::new()];
    let mut used = 0usize;
    let mut overflow: Vec<Span<'a>> = Vec::new();
    for span in spans {
        for word in span.content.split_inclusive(' ') {
            let w = unicode_width::UnicodeWidthStr::width(word);
            let trimmed_w = unicode_width::UnicodeWidthStr::width(word.trim_end());
            if used > 0 && used + trimmed_w > width {
                if rows.len() == max_rows {
                    overflow.push(Span::styled(word.to_string(), span.style));
                    continue;
                }
                rows.push(Vec::new());
                used = 0;
            }
            if !overflow.is_empty() {
                overflow.push(Span::styled(word.to_string(), span.style));
                continue;
            }
            if used == 0 && word.trim().is_empty() {
                continue;
            }
            rows.last_mut().expect("a row").push(Span::styled(word.to_string(), span.style));
            used += w;
        }
    }
    if !overflow.is_empty() {
        let last = rows.pop().unwrap_or_default();
        let mut joined = last;
        joined.extend(overflow);
        rows.push(fit_spans(joined, width));
    }
    // A row may end on the space after its last word; that space is not
    // content and must not count against the width.
    for row in rows.iter_mut() {
        if let Some(last) = row.last_mut() {
            let trimmed = last.content.trim_end().to_string();
            if trimmed.len() != last.content.len() && !trimmed.is_empty() {
                last.content = trimmed.into();
            }
        }
    }
    rows
}

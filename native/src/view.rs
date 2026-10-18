//! Second dashboard views: `rudder` in another tab while a dashboard is
//! already running for the checkout.
//!
//! There is still exactly one `App` and one writer (AGENTS.md §7): the agents,
//! plans and PTYs live here, and the other tab is a thin client (attach.rs).
//! What a view owns is only what a person looking at the dashboard owns: the
//! selection, the focused pane, the task-bar draft, open popups and the
//! hit-test maps from its last frame. `ViewState` holds those fields for each
//! remote view, and `App::with_view` swaps them INTO `App` around handling that
//! view's input or rendering its frame, then swaps them back. Everything else,
//! every key handler and the whole of render.rs, is the dashboard's own code
//! run unchanged, so a second view can do anything the first can: start
//! agents, `/plan`, merge, type into a worker.
//!
//! Rendering goes through a ratatui `Terminal` whose backend writes into a
//! buffer instead of stdout, so a view gets ratatui's own cell diff and only
//! changed cells cross the socket.
//!
//! A pane has one size: the smallest any view showing it asks for
//! (`App::fit_pane`), so switching between tabs never resizes an agent.
//! Every frame goes out as one synchronized update and a repaint overwrites
//! cells in place rather than clearing the screen, so nothing flashes.

use super::*;
use ratatui::{TerminalOptions, Viewport};
use std::sync::{Arc, Mutex};

/// Swaps the listed `App` fields with a `ViewState`'s. Listing a field here is
/// the whole of making it per-view.
macro_rules! view_state {
    ($($field:ident : $ty:ty = $init:expr),* $(,)?) => {
        pub(crate) struct ViewState {
            $($field: $ty,)*
        }

        impl ViewState {
            fn fresh() -> Self {
                Self { $($field: $init,)* }
            }

            fn swap_with(&mut self, app: &mut App) {
                $(std::mem::swap(&mut self.$field, &mut app.$field);)*
            }
        }
    };
}

view_state! {
    focus: FocusPane = FocusPane::Task,
    nav_mode: bool = false,
    leader_pending: bool = false,
    worker_view: WorkerView = WorkerView::Terminal,
    gam_transcript_visible: bool = true,
    nest_view: bool = false,
    gods_view: bool = false,
    gods_view_cursor: usize = 0,
    gods_view_cols: usize = 1,
    gods_view_per_page: usize = 1,
    gods_view_tile_map: Vec<(Rect, usize)> = Vec::new(),
    task_input: String = String::new(),
    task_cursor: usize = 0,
    pasted_chunks: Vec<PastedChunk> = Vec::new(),
    task_history_index: Option<usize> = None,
    task_history_draft: String = String::new(),
    selected_agent: usize = 0,
    agents_scroll: usize = 0,
    notice: Option<String> = None,
    cloud_prompt: Option<CloudLaunchPrompt> = None,
    delete_pending: Option<String> = None,
    delete_armed_frame: u64 = 0,
    merge_confirm: Option<MergeConfirmation> = None,
    cloud_delete_confirm: Option<CloudDeleteConfirm> = None,
    conflict_prompt: Option<MergeConflictPrompt> = None,
    picker_index: usize = 0,
    picker_dismissed_input: Option<String> = None,
    worker_selection: Option<WorkerSelection> = None,
    worker_drag_edge: Option<(MouseEvent, Rect, Instant)> = None,
    task_selection: Option<WorkerSelection> = None,
    orch_selection: Option<WorkerSelection> = None,
    orch_visible_rows: Vec<String> = Vec::new(),
    agent_row_map: Vec<Option<usize>> = Vec::new(),
    drawer_row_map: Vec<Option<Bucket>> = Vec::new(),
    drawer_cursor: Option<Bucket> = None,
    drawer: Option<DrawerState> = None,
    orch_dag_scroll: usize = 0,
    orch_dag_max_scroll: usize = 0,
    orch_follow_bottom: bool = true,
    agents_area: Option<Rect> = None,
    worker_area: Option<Rect> = None,
    diff_area: Option<Rect> = None,
    diff_panel: DiffPanel = DiffPanel::new(),
    solo_pane: bool = false,
    solo_restore_focus: Option<FocusPane> = None,
    task_area: Option<Rect> = None,
    rename_input: Option<String> = None,
    rename_cursor: usize = 0,
    rename_prefilled: bool = false,
    quit_confirm_pending: bool = false,
    // Wheel timing is one viewer's hand on one trackpad.
    last_wheel: Option<(Instant, bool)> = None,
}

/// A `Write` the view's ratatui backend draws into; the bytes are taken after
/// each frame and sent to the client.
#[derive(Clone, Default)]
pub(crate) struct ViewOutput(Arc<Mutex<Vec<u8>>>);

impl ViewOutput {
    fn take(&self) -> Vec<u8> {
        self.0
            .lock()
            .map(|mut buf| std::mem::take(&mut *buf))
            .unwrap_or_default()
    }
}

impl Write for ViewOutput {
    fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
        if let Ok(mut buf) = self.0.lock() {
            buf.extend_from_slice(bytes);
        }
        Ok(bytes.len())
    }

    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

type ViewTerminal = Terminal<CrosstermBackend<ViewOutput>>;

pub(crate) struct DashboardView {
    state: ViewState,
    terminal: ViewTerminal,
    output: ViewOutput,
    /// This view's selection by run id: `selected_agent` is an index, and
    /// rows come and go between this view's turns.
    selected_run: Option<String>,
    /// `agents.len()` when this view last ran, so its click maps are dropped
    /// once rows were inserted or removed (the dashboard clears its own on
    /// every insert/remove; a stashed view never saw those).
    agents_len: usize,
    /// Something this view did needs a new frame.
    dirty: bool,
    rows: u16,
    cols: u16,
}

impl DashboardView {
    pub(crate) fn new(rows: u16, cols: u16) -> Self {
        let output = ViewOutput::default();
        Self {
            state: ViewState::fresh(),
            terminal: view_terminal(&output, rows, cols),
            output,
            selected_run: None,
            agents_len: 0,
            dirty: true,
            rows,
            cols,
        }
    }

    /// New size, or the client fell behind: start over from a blank screen.
    fn reset(&mut self, rows: u16, cols: u16) {
        self.output.take();
        self.terminal = view_terminal(&self.output, rows, cols);
        self.rows = rows;
        self.cols = cols;
        self.dirty = true;
    }
}

fn view_terminal(output: &ViewOutput, rows: u16, cols: u16) -> ViewTerminal {
    let mut output_writer = output.clone();
    let _ = output_writer.write_all(b"\x1b[0m");
    let mut terminal = Terminal::with_options(
        CrosstermBackend::new(output.clone()),
        TerminalOptions {
            viewport: Viewport::Fixed(Rect::new(0, 0, cols.max(1), rows.max(1))),
        },
    )
    .expect("a fixed viewport never queries the terminal");
    force_full_repaint(&mut terminal);
    terminal
}

/// Make the next frame overwrite every cell instead of clearing the screen
/// first. A fresh ratatui terminal diffs against a blank buffer, so it skips
/// blank cells and the client had to be wiped with `ESC[2J`: a whole blank
/// frame on every resize or catch-up, which is the flash people saw. Marking
/// the previous buffer with a glyph no frame ever draws makes every cell
/// differ, so the old picture is painted over in place.
pub(crate) fn force_full_repaint<B: ratatui::backend::Backend>(terminal: &mut Terminal<B>) {
    let buffer = terminal.current_buffer_mut();
    for cell in buffer.content.iter_mut() {
        cell.set_symbol(REPAINT_SENTINEL);
    }
    terminal.swap_buffers();
}

/// Private-use glyph: never drawn by Rudder, never produced by an agent pane.
const REPAINT_SENTINEL: &str = "\u{f8ff}";

/// Wrap a frame in DEC synchronized output (mode 2026): the terminal holds
/// the old picture until the whole frame has arrived and swaps it in at once,
/// so a big repaint never shows half-drawn. Terminals without the mode
/// ignore it.
pub(crate) fn synchronized_frame(bytes: &[u8]) -> String {
    let mut data = String::with_capacity(bytes.len() + 16);
    data.push_str(SYNC_BEGIN);
    data.push_str(&String::from_utf8_lossy(bytes));
    data.push_str(SYNC_END);
    data
}

pub(crate) const SYNC_BEGIN: &str = "\x1b[?2026h";
pub(crate) const SYNC_END: &str = "\x1b[?2026l";

/// Which view is acting: the dashboard's own terminal, or an attach client.
pub(crate) const LOCAL_VIEW: u64 = 0;

impl App {
    /// Run `f` with the remote view `client_id`'s state swapped into `self`.
    /// `None` when that client is gone or is not a dashboard view.
    pub(crate) fn with_view<R>(
        &mut self,
        client_id: u64,
        f: impl FnOnce(&mut App, &mut ViewTerminal) -> R,
    ) -> Option<R> {
        let pos = self
            .attach_clients
            .iter()
            .position(|client| client.id == client_id)?;
        let mut view = self.attach_clients[pos].dashboard.take()?;
        let local_run = self.agents.get(self.selected_agent).map(|run| run.id.clone());
        let local_len = self.agents.len();

        view.state.swap_with(self);
        self.selected_agent = self.index_for_view_selection(view.selected_run.as_deref());
        if view.agents_len != self.agents.len() {
            self.agent_row_map.clear();
            self.drawer_row_map.clear();
        }
        self.acting_view = client_id;
        let result = f(self, &mut view.terminal);
        self.acting_view = LOCAL_VIEW;
        view.selected_run = self.agents.get(self.selected_agent).map(|run| run.id.clone());
        view.agents_len = self.agents.len();
        view.state.swap_with(self);

        self.selected_agent = self.index_for_view_selection(local_run.as_deref());
        if local_len != self.agents.len() {
            self.agent_row_map.clear();
            self.drawer_row_map.clear();
        }
        if let Some(client) = self
            .attach_clients
            .iter_mut()
            .find(|client| client.id == client_id)
        {
            client.dashboard = Some(view);
        }
        Some(result)
    }

    /// The index a view's remembered run is at now; the same index clamped
    /// when that run is gone.
    fn index_for_view_selection(&self, run_id: Option<&str>) -> usize {
        run_id
            .and_then(|id| self.agents.iter().position(|run| run.id == id))
            .unwrap_or_else(|| self.selected_agent.min(self.agents.len().saturating_sub(1)))
    }

    /// Is this a second view acting right now (its keys, its frame)?
    pub(crate) fn acting_in_remote_view(&self) -> bool {
        self.acting_view != LOCAL_VIEW
    }

    /// Size the pane at `index` for the view drawing it now. Every view that
    /// shows a pane records the size it would like, and the pane takes the
    /// smallest of them (tmux's `window-size smallest`). It used to be the
    /// size of whichever view took input last, and two views a row apart
    /// (one had a notice, or a longer task-bar hint) resized the agent on
    /// every switch between them: each resize makes Claude or Codex repaint
    /// its whole screen, and the region scrollback is dropped. An attached
    /// single-pane terminal still owns its pane outright.
    pub(crate) fn fit_pane(&mut self, index: usize, size: TerminalSize) {
        let Some(run_id) = self.agents.get(index).map(|run| run.id.clone()) else {
            return;
        };
        if self.run_is_attached(&run_id) {
            return;
        }
        self.pane_size_wants
            .insert((self.acting_view, run_id.clone()), size);
        let fitted = self
            .pane_size_wants
            .iter()
            .filter(|((_, id), _)| *id == run_id)
            .map(|(_, want)| *want)
            .reduce(|a, b| TerminalSize {
                rows: a.rows.min(b.rows),
                cols: a.cols.min(b.cols),
            })
            .unwrap_or(size);
        let run = &mut self.agents[index];
        if run.terminal_size != Some(fitted) {
            if let Some(terminal) = run.terminal.as_mut() {
                if terminal.resize(fitted).is_ok() {
                    run.terminal_size = Some(fitted);
                }
            }
        }
    }

    /// A view is about to draw: what it showed last frame no longer counts,
    /// so a pane it stopped showing can grow back for the others.
    pub(crate) fn begin_view_frame(&mut self, view: u64) {
        self.pane_size_wants.retain(|(owner, _), _| *owner != view);
    }

    /// A view went away: its sizes stop holding panes small.
    pub(crate) fn forget_view_pane_sizes(&mut self, view: u64) {
        self.begin_view_frame(view);
        self.dirty = true;
    }

    pub(crate) fn open_dashboard_view(&mut self, client_id: u64, rows: u16, cols: u16) {
        if let Some(client) = self
            .attach_clients
            .iter_mut()
            .find(|client| client.id == client_id)
        {
            let mut view = DashboardView::new(rows, cols);
            // Start on the same row the dashboard is looking at.
            view.selected_run = self.agents.get(self.selected_agent).map(|run| run.id.clone());
            view.state.notice = Some(
                "second view of this dashboard · same agents · Ctrl+C closes this tab only".to_string(),
            );
            client.dashboard = Some(Box::new(view));
        }
    }

    pub(crate) fn resize_dashboard_view(&mut self, client_id: u64, rows: u16, cols: u16) {
        if let Some(view) = self
            .attach_clients
            .iter_mut()
            .find(|client| client.id == client_id)
            .and_then(|client| client.dashboard.as_mut())
        {
            view.reset(rows, cols);
        }
    }

    /// Apply one terminal event from a remote view. Returns true when that
    /// view asked to close (its quit keys close the tab, never the dashboard).
    pub(crate) fn handle_view_event(&mut self, client_id: u64, event: Event) -> bool {
        let closed = self
            .with_view(client_id, |app, _| match event {
                Event::Key(key) if key.kind == KeyEventKind::Press => app.handle_key(key),
                Event::Paste(text) => {
                    app.handle_paste(text);
                    false
                }
                Event::Mouse(mouse) => {
                    app.handle_mouse(mouse);
                    false
                }
                _ => false,
            })
            .unwrap_or(true);
        if let Some(view) = self
            .attach_clients
            .iter_mut()
            .find(|client| client.id == client_id)
            .and_then(|client| client.dashboard.as_mut())
        {
            view.dirty = true;
        }
        // Shared state may have moved (a new agent, a merge): the dashboard's
        // own terminal redraws too.
        self.dirty = true;
        closed
    }

    /// Draw every remote view that needs it and send the bytes. `changed`:
    /// shared state changed this tick, so every view redraws.
    pub(crate) fn render_dashboard_views(&mut self, changed: bool) {
        let ids: Vec<u64> = self
            .attach_clients
            .iter()
            .filter(|client| {
                client
                    .dashboard
                    .as_ref()
                    .is_some_and(|view| changed || view.dirty)
            })
            .map(|client| client.id)
            .collect();
        if ids.is_empty() {
            return;
        }
        let dirty_before = self.dirty;
        for id in ids {
            self.begin_view_frame(id);
            let drew = self
                .with_view(id, |app, terminal| {
                    terminal.draw(|frame| render(frame, app)).is_ok()
                })
                .unwrap_or(false);
            let Some(client) = self.attach_clients.iter_mut().find(|client| client.id == id) else {
                continue;
            };
            let Some(view) = client.dashboard.as_mut() else {
                continue;
            };
            view.dirty = false;
            let bytes = view.output.take();
            if !drew || bytes.is_empty() {
                continue;
            }
            client.send(&ServerMsg::Screen {
                data: synchronized_frame(&bytes),
            });
            if client.take_needs_full() {
                // Dropped: repaint the whole screen on the next pass.
                if let Some(view) = client.dashboard.as_mut() {
                    let (rows, cols) = (view.rows, view.cols);
                    view.reset(rows, cols);
                }
            }
        }
        // Rendering a view is not a change the dashboard's own terminal has
        // to show.
        self.dirty = dirty_before;
    }
}

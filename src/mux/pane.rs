use anyhow::Result;
use crossterm::event::{KeyCode, KeyEvent, MouseEvent, MouseEventKind};
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::Sender;
use std::sync::{Arc, Mutex};
use std::time::Instant;

use crate::events::hooks::{HookAttention, HookLifecycleEvent, HookMonitor};
use crate::events::lifecycle::{InputKind, LifecycleEvent, LifecycleMonitor};

use super::callbacks::{PaneCallbacks, PaneSignalEvent, PaneSignals};
use super::keys;
use super::pty::{pty_size, PtyChunk, PtySession};
use super::MuxEvent;

pub type PaneId = u64;

type PaneParser = Arc<Mutex<vt100::Parser<PaneCallbacks>>>;

#[derive(Debug, Clone, Copy)]
struct Viewport {
    x: u16,
    y: u16,
    rows: u16,
    cols: u16,
}

impl Viewport {
    fn coordinates(self, column: u16, row: u16) -> Option<(u16, u16)> {
        let column = column.checked_sub(self.x)?;
        let row = row.checked_sub(self.y)?;
        (column < self.cols && row < self.rows).then_some((column + 1, row + 1))
    }

    fn clamped_coordinates(self, column: u16, row: u16) -> (u16, u16) {
        let max_column = self.x.saturating_add(self.cols.saturating_sub(1));
        let max_row = self.y.saturating_add(self.rows.saturating_sub(1));
        (
            column.clamp(self.x, max_column) - self.x + 1,
            row.clamp(self.y, max_row) - self.y + 1,
        )
    }

    fn cell_coordinates(self, column: u16, row: u16) -> Option<(u16, u16)> {
        let column = column.checked_sub(self.x)?;
        let row = row.checked_sub(self.y)?;
        (column < self.cols && row < self.rows).then_some((row, column))
    }
}

/// Why a pane stopped running, if it has.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PaneStatus {
    Running,
    Exited(Option<u32>),
}

/// One Copilot session hosted inside CST: a PTY plus the terminal state it drives.
pub struct Pane {
    pub id: PaneId,
    /// Copilot session id. Known before the child starts: resumes are given one and new
    /// sessions are told theirs via `--session-id`, so a pane can always key per-session
    /// state (scratchpad, terminal, panel layout) on something stable and unique.
    pub session_id: String,
    pub title: String,
    pub cwd: PathBuf,
    pub status: PaneStatus,
    /// When the child was spawned, so the UI can say how long a slow start has taken.
    pub started_at: Instant,
    parser: PaneParser,
    has_visible_output: Arc<AtomicBool>,
    _lifecycle_monitor: Option<LifecycleMonitor>,
    _hook_monitor: Option<HookMonitor>,
    pty: PtySession,
    mouse_captured: bool,
    viewport: Viewport,
    /// Whether this pane's composer holds text that has not been consumed.
    ///
    /// CST cannot read Copilot's composer, so this is inferred: from the keys it
    /// forwards, and from anything it writes there itself. Both matter. Writing over a
    /// half-typed message mangles it and sends it early; writing over a wake-up that
    /// Copilot has not picked up yet stacks them into one unreadable message, which is
    /// what a busy thread produced.
    ///
    /// Deliberately sticky: set by anything that puts text there, cleared only by
    /// evidence it is gone — a submit, or a turn starting.
    composer_occupied: bool,

    working: bool,
    progress_state: crate::host_terminal::ProgressState,
    raw_progress_generation: u64,
    hook_state: Option<HookActivity>,
    hook_waiting: Option<HookAttention>,
    hook_timestamp: u64,
    hook_ready_pending: Option<u64>,
    hook_ready_raw_generation: u64,
    /// The working state came from progress sequences alone, with no hook to back it.
    ///
    /// Copilot animates its own progress for reasons that are not a turn — waiting on an
    /// attached background shell, most visibly — and that animation used to pin a pane to
    /// working for as long as the shell lived. A belief held on this evidence alone is
    /// given up when the session event log proves nothing is running.
    working_from_raw_progress: bool,
    /// Progress sequences have been caught claiming a turn the event log denies.
    ///
    /// Retiring the spinner once is not enough on its own: if the child re-emits its
    /// progress sequence — and a spinner being animated is exactly the sort of thing that
    /// does — the next one promotes the pane straight back to working, and the tab blinks
    /// every quiet period instead of settling. Once the log has proved the session idle,
    /// progress alone stops being grounds to start a turn until a hook vouches for one.
    raw_progress_discredited: bool,
    raw_completion_pending: bool,
    hook_completion_window: bool,
    attention_resolved_at: u64,
    pending_inputs: Vec<(String, InputKind)>,
    needs_attention: bool,
    ready_sent_in_cycle: bool,
    error_sent_in_cycle: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum HookActivity {
    Idle,
    Working,
    Error,
}

#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub struct PaneSignalOutcome {
    pub bell: bool,
}

#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub struct PaneHookOutcome {
    pub notification: Option<PaneNotification>,
    pub cycle_started: bool,
    pub ready_confirmation: Option<u64>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PaneNotification {
    Ready,
    Question,
    PlanApproval,
    Error,
}

/// Everything needed to start a pane, grouped so callers don't juggle a long argument list.
pub struct PaneSpec {
    pub id: PaneId,
    pub title: String,
    pub cwd: PathBuf,
    pub session_id: String,
    pub program: String,
    pub args: Vec<String>,
    pub events_path: Option<PathBuf>,
    pub terminal_light_mode: Option<bool>,
    /// Whether CST is managing lifecycle hooks, so this child will report its own state.
    ///
    /// Decides whether the pane may assume it is idle on attach, or has to believe
    /// whatever the terminal tells it.
    pub hooks_active: bool,
}

impl Drop for Pane {
    fn drop(&mut self) {
        if self.is_running() {
            let _ = self.shutdown();
        }
    }
}

impl Pane {
    pub fn spawn(spec: PaneSpec, rows: u16, cols: u16, events: Sender<MuxEvent>) -> Result<Self> {
        let PaneSpec {
            id,
            title,
            cwd,
            session_id,
            program,
            args,
            events_path,
            terminal_light_mode,
            hooks_active,
        } = spec;
        let size = pty_size(rows, cols);
        let lifecycle_baseline = events_path
            .as_deref()
            .map(crate::events::lifecycle::capture_file_baseline)
            .transpose()?;
        let hook_events_path = events_path
            .as_ref()
            .map(|path| path.with_file_name(".cst-lifecycle.jsonl"));
        let hook_baseline = hook_events_path
            .as_deref()
            .map(crate::events::hooks::capture_file_len)
            .unwrap_or_default();
        // A session whose child reports lifecycle state is idle at the moment we attach:
        // it has only just been launched, so it cannot be mid-turn. Without this the
        // pane starts with no lifecycle state at all and falls back to whatever the
        // terminal says — and Copilot announces itself as working for as long as a
        // background process it started is alive, which can be indefinitely. The first
        // `userPromptSubmitted` overwrites this within a keystroke if it is ever wrong.
        //
        // Keyed on hooks being installed now, rather than on this session having
        // reported before. The two are not the same claim: a session resumed but not yet
        // prompted has no journal and perfectly working hooks, while a user who has
        // since run `cst hooks uninstall` has journals that prove nothing. Claiming idle
        // where nothing will ever correct it would leave the pane with no progress
        // indication at all.
        let seeded_hook_state = hooks_active.then_some(HookActivity::Idle);

        let (chunk_tx, chunk_rx) = std::sync::mpsc::channel();
        // The session tells its own agent two things about itself.
        //
        // Which session it is: `thread post` runs as a separate process inside the pane
        // and has no other way to find out, and guessing from the working directory
        // would be wrong the moment two sessions share a checkout.
        //
        // And where this exact CST lives, for the same reason the lifecycle hooks are
        // materialized with an absolute path rather than a bare command: `cst` is a
        // shell function from the installer's profile integration, so it does not exist
        // in the non-interactive shell an agent runs commands in.
        let executable = crate::updater::invocation_executable()
            .map(|path| path.to_string_lossy().into_owned())
            .unwrap_or_default();
        let mut environment = vec![(crate::threads::SESSION_ID_ENV, session_id.as_str())];
        if !executable.is_empty() {
            environment.push((crate::threads::CLI_PATH_ENV, executable.as_str()));
        }
        let pty = PtySession::spawn(&program, &args, Some(&cwd), size, &environment, chunk_tx)?;

        // Device-status replies must reach the child, or ConPTY stalls on startup.
        let parser = Arc::new(Mutex::new(vt100::Parser::new_with_callbacks(
            size.rows,
            size.cols,
            // Scrollback keeps output above the viewport reachable once copy-mode lands.
            5000,
            PaneCallbacks::new(id, pty.writer_handle(), events.clone(), terminal_light_mode),
        )));

        let pump_parser = Arc::clone(&parser);
        let has_visible_output = Arc::new(AtomicBool::new(false));
        let pump_has_visible_output = Arc::clone(&has_visible_output);
        let lifecycle_events = events.clone();
        let hook_events = events.clone();
        std::thread::spawn(move || {
            while let Ok(chunk) = chunk_rx.recv() {
                match chunk {
                    PtyChunk::Output(bytes) => {
                        let signals = if let Ok(mut parser) = pump_parser.lock() {
                            parser.process(&bytes);
                            if !pump_has_visible_output.load(Ordering::Relaxed)
                                && Self::screen_has_visible_text(parser.screen())
                            {
                                pump_has_visible_output.store(true, Ordering::Release);
                            }
                            parser.callbacks_mut().take_signals()
                        } else {
                            PaneSignals::default()
                        };
                        // The UI thread coalesces these; one wakeup per chunk is fine.
                        if events.send(MuxEvent::Output(id, signals)).is_err() {
                            return;
                        }
                    }
                    PtyChunk::Exited(code) => {
                        let _ = events.send(MuxEvent::Exited(id, code));
                        return;
                    }
                }
            }
        });
        let lifecycle_monitor = events_path.zip(lifecycle_baseline).map(|(path, baseline)| {
            LifecycleMonitor::start_from_baseline(path, baseline, move |event| {
                lifecycle_events
                    .send(MuxEvent::SessionLifecycle(id, event))
                    .is_ok()
            })
        });
        let hook_monitor = hook_events_path.map(|path| {
            HookMonitor::start(path, hook_baseline, move |event| {
                hook_events.send(MuxEvent::HookLifecycle(id, event)).is_ok()
            })
        });

        Ok(Self {
            id,
            session_id,
            title,
            cwd,
            status: PaneStatus::Running,
            started_at: Instant::now(),
            parser,
            has_visible_output,
            _lifecycle_monitor: lifecycle_monitor,
            _hook_monitor: hook_monitor,
            pty,
            mouse_captured: false,
            viewport: Viewport {
                x: 0,
                y: 0,
                rows: size.rows,
                cols: size.cols,
            },
            composer_occupied: false,
            working: false,
            progress_state: crate::host_terminal::ProgressState::Clear,
            raw_progress_generation: 0,
            hook_state: seeded_hook_state,
            hook_waiting: None,
            hook_timestamp: 0,
            hook_ready_pending: None,
            hook_ready_raw_generation: 0,
            working_from_raw_progress: false,
            raw_progress_discredited: false,
            raw_completion_pending: false,
            hook_completion_window: false,
            attention_resolved_at: 0,
            pending_inputs: Vec::new(),
            needs_attention: false,
            ready_sent_in_cycle: false,
            error_sent_in_cycle: false,
        })
    }

    pub fn is_running(&self) -> bool {
        self.status == PaneStatus::Running
    }

    pub fn set_terminal_light_mode(&mut self, terminal_light_mode: Option<bool>) {
        if let Ok(mut parser) = self.parser.lock() {
            parser
                .callbacks_mut()
                .set_terminal_light_mode(terminal_light_mode);
        }
    }

    /// Whether this pane's composer holds text nobody has sent yet.
    pub fn has_draft(&self) -> bool {
        self.composer_occupied
    }

    /// Note that CST has written into the composer itself.
    ///
    /// Copilot does not always consume what is written the moment it is written — an
    /// autopilot run working through background agents will leave it sitting there. A
    /// second wake-up arriving then would stack onto the first, and the user would get
    /// one message containing the same notice several times over.
    pub fn note_injection(&mut self) {
        self.composer_occupied = true;
    }

    /// Note a key on its way to the child, to track whether a draft is being composed.
    ///
    /// Only a plain Enter clears it. Everything else that could be editing — backspace,
    /// arrows, a newline chord — leaves the draft standing, because the cost of the two
    /// mistakes is not symmetric. Believing in a draft that is not there delays a
    /// wake-up; believing there is none when there is loses the user's words.
    ///
    /// That asymmetry argues for *keeping* a draft, not for inventing one. Backspace and
    /// delete can only ever take text away, so they leave the flag exactly as they found
    /// it: they neither start a draft nor prove one has ended. Treating them as typing
    /// meant one stray backspace on an empty composer marked the tab `✎` and held any
    /// thread notice for that session until it timed out and went to the inbox instead.
    pub fn note_user_key(&mut self, key: &crossterm::event::KeyEvent) {
        use crossterm::event::{KeyCode, KeyModifiers};

        // Copilot submits on a bare Enter and inserts a newline for the chorded ones, so
        // only the bare one means the composer is now empty.
        if key.code == KeyCode::Enter && key.modifiers == KeyModifiers::NONE {
            self.composer_occupied = false;
            return;
        }
        let composes = match key.code {
            // A chord like Ctrl-C or Ctrl-L is a command, not typing.
            KeyCode::Char(_) => !key
                .modifiers
                .intersects(KeyModifiers::CONTROL | KeyModifiers::ALT),
            // Tab stays: Copilot completes on it, which puts text there.
            KeyCode::Enter | KeyCode::Tab => true,
            // Backspace, delete, arrows, everything else: no new text, so no change.
            _ => false,
        };
        if composes {
            self.composer_occupied = true;
        }
    }

    pub fn is_working(&self) -> bool {
        self.working
    }

    pub fn effective_progress_state(&self) -> crate::host_terminal::ProgressState {
        if self.requires_user_action() {
            crate::host_terminal::ProgressState::Clear
        } else {
            match self.hook_state {
                Some(HookActivity::Idle) => crate::host_terminal::ProgressState::Clear,
                Some(HookActivity::Working)
                    if matches!(
                        self.progress_state,
                        crate::host_terminal::ProgressState::Normal
                            | crate::host_terminal::ProgressState::Indeterminate
                    ) =>
                {
                    self.progress_state
                }
                Some(HookActivity::Working) => crate::host_terminal::ProgressState::Indeterminate,
                Some(HookActivity::Error) => crate::host_terminal::ProgressState::Error,
                None => self.progress_state,
            }
        }
    }

    pub fn should_forward_raw_progress(&self, state: crate::host_terminal::ProgressState) -> bool {
        if self.requires_user_action() {
            return state == crate::host_terminal::ProgressState::Clear;
        }
        match self.hook_state {
            None => true,
            Some(HookActivity::Idle) => state == crate::host_terminal::ProgressState::Clear,
            Some(HookActivity::Working) => matches!(
                state,
                crate::host_terminal::ProgressState::Normal
                    | crate::host_terminal::ProgressState::Indeterminate
            ),
            Some(HookActivity::Error) => state == crate::host_terminal::ProgressState::Error,
        }
    }

    pub fn record_progress_state(&mut self, state: crate::host_terminal::ProgressState) -> bool {
        self.raw_progress_generation = self.raw_progress_generation.wrapping_add(1);
        let mut cycle_started = false;
        let compatible_working = matches!(
            state,
            crate::host_terminal::ProgressState::Normal
                | crate::host_terminal::ProgressState::Indeterminate
        );
        let continuation_after_clear = self.hook_ready_pending.is_some()
            && self.hook_state == Some(HookActivity::Idle)
            && self.progress_state == crate::host_terminal::ProgressState::Clear
            && compatible_working;
        if (self.hook_ready_pending.is_none() || continuation_after_clear)
            && self.hook_state == Some(HookActivity::Idle)
            && compatible_working
            && !self.raw_progress_discredited
        {
            self.hook_ready_pending = None;
            self.hook_state = Some(HookActivity::Working);
            self.working = true;
            self.needs_attention = false;
            self.ready_sent_in_cycle = false;
            self.error_sent_in_cycle = false;
            self.raw_completion_pending = false;
            self.hook_completion_window = false;
            // Remembered as unconfirmed. The hooks have already called this turn over,
            // so the only thing saying otherwise is the child's own animation — which it
            // also runs while waiting on a background shell that may never exit.
            self.working_from_raw_progress = true;
            cycle_started = true;
        }
        self.progress_state = state;
        cycle_started
    }

    /// True while the child has produced nothing visible yet.
    ///
    /// Copilot takes a few seconds to draw its first frame, and ConPTY emits control
    /// sequences long before any text, so "has the screen got anything on it" is a more
    /// honest readiness signal than "have we received bytes".
    pub fn is_blank(&self) -> bool {
        !self.has_visible_output.load(Ordering::Acquire)
    }

    /// Feed bytes straight into the parser, standing in for child output in tests
    /// and in the generated documentation screenshots.
    #[cfg(any(test, feature = "screenshots"))]
    pub fn feed_synthetic(&mut self, bytes: &[u8]) {
        if let Ok(mut parser) = self.parser.lock() {
            parser.process(bytes);
            if Self::screen_has_visible_text(parser.screen()) {
                self.has_visible_output.store(true, Ordering::Release);
            }
        }
    }

    fn screen_has_visible_text(screen: &vt100::Screen) -> bool {
        let (rows, cols) = screen.size();
        (0..rows).any(|row| {
            (0..cols).any(|col| {
                screen.cell(row, col).is_some_and(|cell| {
                    cell.contents()
                        .chars()
                        .any(|character| !character.is_whitespace())
                })
            })
        })
    }

    pub fn mark_exited(&mut self, code: Option<u32>) {
        self.status = PaneStatus::Exited(code);
        self._lifecycle_monitor.take();
        self._hook_monitor.take();
        self.working = false;
        self.progress_state = crate::host_terminal::ProgressState::Clear;
        self.hook_state = Some(HookActivity::Idle);
        self.hook_waiting = None;
        self.hook_ready_pending = None;
        self.raw_completion_pending = false;
        self.hook_completion_window = false;
        self.pending_inputs.clear();
    }

    /// Run `f` against the current screen. The parser lock is held for the call, so
    /// keep it to rendering and cheap queries.
    pub fn with_screen<T>(&self, f: impl FnOnce(&vt100::Screen) -> T) -> Option<T> {
        let parser = self.parser.lock().ok()?;
        Some(f(parser.screen()))
    }

    pub fn application_cursor(&self) -> bool {
        self.with_screen(|screen| screen.application_cursor())
            .unwrap_or(false)
    }

    pub fn bracketed_paste(&self) -> bool {
        self.with_screen(|screen| screen.bracketed_paste())
            .unwrap_or(false)
    }

    /// GitHub issue/PR/discussion reference under an outer-terminal coordinate, if any.
    pub fn github_reference_at(&self, column: u16, row: u16) -> Option<u64> {
        let (row, column) = self.viewport.cell_coordinates(column, row)?;
        self.with_screen(|screen| github_reference_at(screen, row, column))
            .flatten()
    }

    /// Where the hyperlink under an outer-terminal coordinate points, if there is one.
    pub fn link_at(&self, column: u16, row: u16) -> Option<String> {
        let (row, column) = self.viewport.cell_coordinates(column, row)?;
        let parser = self.parser.lock().ok()?;
        parser.callbacks().link_at(parser.screen(), row, column)
    }

    /// The web links on screen, in outer-terminal coordinates, ready to be drawn as real
    /// hyperlinks.
    ///
    /// Web links only. A file link handed to a terminal is opened with whatever the file's
    /// type launches, so those stay with CST, which reveals rather than runs them.
    pub fn web_hyperlinks(&self) -> Vec<crate::ui::hyperlinks::HyperlinkRun> {
        let Ok(parser) = self.parser.lock() else {
            return Vec::new();
        };
        let screen = parser.screen();
        let (_, width) = screen.size();
        parser
            .callbacks()
            .links_on_screen(screen)
            .into_iter()
            .filter_map(|crate::mux::callbacks::LiveLink { start, end, target }| {
                let Some(crate::links::Target::Web(url)) = crate::links::classify(&target) else {
                    return None;
                };
                let mut cells = Vec::new();
                let (mut row, mut column) = start;
                while (row, column) < end {
                    let cell = screen.cell(row, column)?;
                    // The right half of a wide character is drawn by its left half.
                    if !cell.is_wide_continuation()
                        && row < self.viewport.rows
                        && column < self.viewport.cols
                    {
                        cells.push((
                            self.viewport.x + column,
                            self.viewport.y + row,
                            cell.contents().to_string(),
                        ));
                    }
                    column += 1;
                    if column >= width {
                        column = 0;
                        row += 1;
                    }
                }
                Some(crate::ui::hyperlinks::HyperlinkRun { target: url, cells })
            })
            .collect()
    }

    /// Every `#1234` currently on the pane's screen.
    ///
    /// Used to look up what those numbers point at, so they can be decorated.
    pub fn github_references(&self) -> Vec<u64> {
        self.with_screen(github_references).unwrap_or_default()
    }

    /// Apply title/progress/bell signals captured from one exact PTY output chunk.
    pub fn apply_signals(
        &mut self,
        signals: PaneSignals,
        attended: bool,
    ) -> (PaneSignalOutcome, Vec<PaneNotification>) {
        let mut outcome = PaneSignalOutcome::default();
        let mut notifications = Vec::new();
        if let Some(title) = signals.title {
            if let Some(title) = normalize_copilot_title(&title) {
                self.title = title;
            }
        }

        for event in signals.events {
            match event {
                PaneSignalEvent::Progress(progress) if progress.is_working() => {
                    self.progress_state = progress;
                    if self.hook_state.is_some() {
                        if self.hook_state == Some(HookActivity::Working) {
                            self.raw_completion_pending = false;
                        }
                        continue;
                    }
                    if !self.working {
                        self.ready_sent_in_cycle = false;
                        self.error_sent_in_cycle = false;
                    }
                    self.working = true;
                    if !self.requires_user_action() {
                        self.needs_attention = false;
                    }
                    if progress == crate::host_terminal::ProgressState::Error
                        && !self.error_sent_in_cycle
                    {
                        notifications.push(PaneNotification::Error);
                        self.error_sent_in_cycle = true;
                    }
                }
                PaneSignalEvent::Progress(_)
                    if self.hook_state == Some(HookActivity::Idle)
                        && self.hook_completion_window =>
                {
                    self.progress_state = crate::host_terminal::ProgressState::Clear;
                    self.hook_ready_pending = None;
                    self.hook_completion_window = false;
                    self.raw_completion_pending = false;
                    self.working = false;
                    self.needs_attention = !attended;
                    if !self.error_sent_in_cycle && !self.ready_sent_in_cycle {
                        self.ready_sent_in_cycle = true;
                        if !attended {
                            notifications.push(PaneNotification::Ready);
                        }
                    }
                }
                PaneSignalEvent::Progress(_) if self.working => {
                    self.progress_state = crate::host_terminal::ProgressState::Clear;
                    if matches!(
                        self.hook_state,
                        Some(HookActivity::Working | HookActivity::Error)
                    ) {
                        if self.hook_state == Some(HookActivity::Working) {
                            self.raw_completion_pending = true;
                        }
                        continue;
                    }
                    self.hook_ready_pending = None;
                    self.working = false;
                    if !self.requires_user_action() {
                        self.needs_attention = !attended;
                    }
                    if !self.requires_user_action()
                        && !self.error_sent_in_cycle
                        && !self.ready_sent_in_cycle
                    {
                        self.ready_sent_in_cycle = true;
                        if !attended {
                            notifications.push(PaneNotification::Ready);
                        }
                    }
                }
                PaneSignalEvent::Progress(_) => {
                    self.progress_state = crate::host_terminal::ProgressState::Clear;
                }
                PaneSignalEvent::Bell => {
                    outcome.bell = true;
                    if matches!(
                        self.hook_state,
                        Some(HookActivity::Working | HookActivity::Error)
                    ) {
                        if self.hook_state == Some(HookActivity::Working) {
                            self.raw_completion_pending = true;
                        }
                        continue;
                    }
                    if self.hook_state == Some(HookActivity::Idle) {
                        self.hook_ready_pending = None;
                        self.hook_completion_window = false;
                        self.raw_completion_pending = false;
                        self.working = false;
                    }
                    if !attended && !self.requires_user_action() {
                        self.needs_attention = true;
                    }
                    if !self.requires_user_action()
                        && !self.error_sent_in_cycle
                        && !self.ready_sent_in_cycle
                    {
                        self.ready_sent_in_cycle = true;
                        if !attended {
                            notifications.push(PaneNotification::Ready);
                        }
                    }
                }
            }
        }
        (outcome, notifications)
    }

    pub fn apply_lifecycle(&mut self, event: LifecycleEvent) -> Option<PaneNotification> {
        match event {
            LifecycleEvent::InputRequested { tool_call_id, kind } => {
                if self
                    .pending_inputs
                    .iter()
                    .any(|(pending_id, _)| pending_id == &tool_call_id)
                {
                    return None;
                }
                let already_waiting = self.requires_user_action();
                let hook_elicitation = self.hook_waiting == Some(HookAttention::Elicitation);
                self.pending_inputs.push((tool_call_id, kind));
                self.ready_sent_in_cycle = true;
                (!already_waiting || hook_elicitation || kind == InputKind::PlanApproval).then_some(
                    match kind {
                        InputKind::Question => PaneNotification::Question,
                        InputKind::PlanApproval => PaneNotification::PlanApproval,
                    },
                )
            }
            LifecycleEvent::InputResolved { tool_call_id, kind } => {
                let previous_len = self.pending_inputs.len();
                self.pending_inputs.retain(|(pending_id, pending_kind)| {
                    pending_id != &tool_call_id || *pending_kind != kind
                });
                if previous_len != self.pending_inputs.len() && self.pending_inputs.is_empty() {
                    self.attention_resolved_at = current_timestamp();
                    self.hook_waiting = None;
                    self.needs_attention = false;
                    self.ready_sent_in_cycle = false;
                    self.error_sent_in_cycle = false;
                }
                None
            }
            LifecycleEvent::Reset => {
                if !self.pending_inputs.is_empty() {
                    self.pending_inputs.clear();
                    self.needs_attention = false;
                    self.ready_sent_in_cycle = false;
                    self.error_sent_in_cycle = false;
                }
                None
            }
            // Copilot has written nothing for a while and has no tool call running, so
            // whatever its progress sequences are animating, it is not a turn. Only a
            // working state resting on those sequences alone is given up: a hook said so
            // outranks this, which is what keeps a long silent build spinning.
            LifecycleEvent::Quiet => {
                // Whatever the child is animating, it is not a turn. Progress alone stops
                // being grounds to start one until a hook vouches for activity again.
                self.raw_progress_discredited = true;
                if self.working_from_raw_progress {
                    self.working_from_raw_progress = false;
                    self.hook_state = Some(HookActivity::Idle);
                    self.working = false;
                    self.progress_state = crate::host_terminal::ProgressState::Clear;
                    self.hook_ready_pending = None;
                    self.hook_completion_window = false;
                    self.raw_completion_pending = false;
                }
                None
            }
        }
    }

    pub fn apply_hook(&mut self, event: HookLifecycleEvent, attended: bool) -> PaneHookOutcome {
        let timestamp = event.timestamp();
        let previous_timestamp = self.hook_timestamp;
        if timestamp < previous_timestamp {
            return PaneHookOutcome::default();
        }
        self.hook_timestamp = timestamp;
        // A hook has spoken, so the state no longer rests on progress sequences alone.
        // This is what keeps a genuinely working session spinning through a long silent
        // tool call: `Quiet` only ever gives up a belief nothing but the animation backed.
        self.working_from_raw_progress = false;
        self.raw_progress_discredited = false;
        match event {
            HookLifecycleEvent::SessionStarted { .. } => {
                // Dropping to `None` here means "no lifecycle authority yet, fall back
                // to raw OSC 9;4". That was safe when Copilot emitted progress
                // sequences; it no longer does, so `None` now means no progress signal
                // at all. Copilot also delivers this event *after* `userPromptSubmitted`
                // — several seconds into a turn that is already running — so resetting
                // unconditionally cancelled the working state mid-turn and left nothing
                // in its place. An in-flight turn keeps its state; anything else still
                // waits for a real working/ready transition.
                if self.hook_state != Some(HookActivity::Working) {
                    self.hook_state = None;
                }
                self.hook_waiting = None;
                self.hook_ready_pending = None;
                self.raw_completion_pending = false;
                self.hook_completion_window = false;
                PaneHookOutcome::default()
            }
            HookLifecycleEvent::Working { .. } => {
                let cycle_started = self.hook_state != Some(HookActivity::Working);
                // A turn started, so whatever was being composed has been sent. This is
                // the authoritative clear: it catches a message submitted by a route CST
                // never saw, such as a mouse click or a paste that ends in a newline.
                self.composer_occupied = false;
                self.hook_state = Some(HookActivity::Working);
                self.hook_waiting = None;
                self.hook_ready_pending = None;
                self.raw_completion_pending = false;
                self.hook_completion_window = false;
                if !matches!(
                    self.progress_state,
                    crate::host_terminal::ProgressState::Normal
                        | crate::host_terminal::ProgressState::Indeterminate
                ) {
                    self.progress_state = crate::host_terminal::ProgressState::Clear;
                }
                if cycle_started {
                    self.ready_sent_in_cycle = false;
                    self.error_sent_in_cycle = false;
                }
                self.working = true;
                self.needs_attention = false;
                PaneHookOutcome {
                    notification: None,
                    cycle_started,
                    ready_confirmation: None,
                }
            }
            HookLifecycleEvent::Ready { .. } => {
                let continuation_observed = self.hook_ready_pending.is_some()
                    && self.raw_progress_generation != self.hook_ready_raw_generation
                    && matches!(
                        self.progress_state,
                        crate::host_terminal::ProgressState::Normal
                            | crate::host_terminal::ProgressState::Indeterminate
                    );
                if self.hook_ready_pending.is_some() && !continuation_observed {
                    return PaneHookOutcome::default();
                }
                if continuation_observed {
                    self.ready_sent_in_cycle = false;
                    self.error_sent_in_cycle = false;
                }
                let was_working = continuation_observed
                    || self.hook_state == Some(HookActivity::Working)
                    || self.working;
                self.hook_state = Some(HookActivity::Idle);
                self.hook_waiting = None;
                self.needs_attention = false;
                let ready_confirmation =
                    (was_working && !self.error_sent_in_cycle && !self.ready_sent_in_cycle)
                        .then_some(timestamp);
                self.hook_ready_pending = ready_confirmation;
                self.hook_ready_raw_generation = self.raw_progress_generation;
                self.hook_completion_window = was_working;
                PaneHookOutcome {
                    notification: None,
                    cycle_started: false,
                    ready_confirmation,
                }
            }
            HookLifecycleEvent::Error { .. } => {
                self.hook_state = Some(HookActivity::Error);
                self.hook_waiting = None;
                self.hook_ready_pending = None;
                self.raw_completion_pending = false;
                self.hook_completion_window = false;
                self.working = false;
                self.needs_attention = !attended;
                let notification = (!self.error_sent_in_cycle).then_some(PaneNotification::Error);
                self.error_sent_in_cycle = true;
                PaneHookOutcome {
                    notification,
                    cycle_started: false,
                    ready_confirmation: None,
                }
            }
            HookLifecycleEvent::Awaiting { attention, .. } => {
                if timestamp <= self.attention_resolved_at
                    || (timestamp == previous_timestamp
                        && matches!(
                            self.hook_state,
                            Some(HookActivity::Idle | HookActivity::Error)
                        ))
                {
                    return PaneHookOutcome::default();
                }
                let changed = self.hook_waiting != Some(attention);
                self.hook_waiting = Some(attention);
                self.hook_ready_pending = None;
                self.raw_completion_pending = false;
                self.hook_completion_window = false;
                self.ready_sent_in_cycle = true;
                PaneHookOutcome {
                    notification: (changed && attention == HookAttention::Permission)
                        .then_some(PaneNotification::Question),
                    cycle_started: false,
                    ready_confirmation: None,
                }
            }
            HookLifecycleEvent::SessionEnded { .. } => {
                self.hook_state = Some(HookActivity::Idle);
                self.hook_waiting = None;
                self.hook_ready_pending = None;
                self.raw_completion_pending = false;
                self.hook_completion_window = false;
                self.working = false;
                PaneHookOutcome::default()
            }
        }
    }

    pub fn confirm_hook_ready(
        &mut self,
        timestamp: u64,
        attended: bool,
    ) -> Option<PaneNotification> {
        if self.hook_ready_pending != Some(timestamp) || self.hook_state != Some(HookActivity::Idle)
        {
            return None;
        }
        if self.raw_progress_generation != self.hook_ready_raw_generation
            && matches!(
                self.progress_state,
                crate::host_terminal::ProgressState::Normal
                    | crate::host_terminal::ProgressState::Indeterminate
            )
        {
            self.hook_ready_pending = None;
            self.hook_state = Some(HookActivity::Working);
            self.working = true;
            self.needs_attention = false;
            self.ready_sent_in_cycle = false;
            return None;
        }
        self.hook_ready_pending = None;
        self.working = false;
        if self.raw_completion_pending && !self.error_sent_in_cycle && !self.ready_sent_in_cycle {
            self.raw_completion_pending = false;
            self.hook_completion_window = false;
            self.needs_attention = !attended;
            self.ready_sent_in_cycle = true;
            // Nothing to tell you about a turn you watched finish. The marker and the
            // notification now answer the same question — "does this pane want you?" —
            // so a turn that leaves no `●` sends nothing either.
            (!attended).then_some(PaneNotification::Ready)
        } else {
            self.needs_attention = false;
            None
        }
    }

    #[cfg(test)]
    pub fn refresh_from_callbacks(
        &mut self,
        attended: bool,
    ) -> (PaneSignalOutcome, Vec<PaneNotification>) {
        let signals = self
            .parser
            .lock()
            .map(|mut parser| parser.callbacks_mut().take_signals())
            .unwrap_or_default();
        self.apply_signals(signals, attended)
    }

    pub fn needs_attention(&self) -> bool {
        self.needs_attention || self.requires_user_action()
    }

    /// A turn that finished while the user was looking somewhere else, and that they
    /// have not been back to since.
    ///
    /// `needs_attention` deliberately lumps this in with a live question, because both
    /// mean "this pane wants you". They do not want the same thing though: a question
    /// blocks the session until it is answered, while this is only a result waiting to
    /// be read. The tab strip tells them apart so a glance can say which pane is
    /// actually stuck.
    pub fn is_unread(&self) -> bool {
        self.needs_attention && !self.requires_user_action()
    }

    pub fn requires_user_action(&self) -> bool {
        !self.pending_inputs.is_empty() || self.hook_waiting.is_some()
    }

    pub fn acknowledge_attention(&mut self) {
        self.needs_attention = false;
    }

    pub fn clear_hook_permission_wait(&mut self) -> bool {
        self.attention_resolved_at = current_timestamp();
        if self.hook_waiting == Some(HookAttention::Permission) {
            self.hook_waiting = None;
            self.ready_sent_in_cycle = false;
            self.error_sent_in_cycle = false;
            true
        } else {
            false
        }
    }

    pub fn display_title(&self) -> String {
        if self.needs_attention() {
            format!("? {}", self.title)
        } else {
            self.title.clone()
        }
    }

    pub fn send_key(&mut self, key: &KeyEvent) -> Result<()> {
        if !self.is_running() {
            return Ok(());
        }
        // DECCKM only changes cursor/navigation key encodings. Reading it for every
        // ordinary character needlessly contended with the parser thread that is
        // producing Copilot's echo.
        let application_cursor = if matches!(
            key.code,
            KeyCode::Up
                | KeyCode::Down
                | KeyCode::Left
                | KeyCode::Right
                | KeyCode::Home
                | KeyCode::End
        ) {
            self.application_cursor()
        } else {
            false
        };
        if let Some(bytes) = keys::encode(key, application_cursor) {
            self.pty.write(&bytes)?;
        }
        Ok(())
    }

    pub fn send_paste(&mut self, text: &str) -> Result<()> {
        if !self.is_running() {
            return Ok(());
        }
        let bytes = keys::encode_paste(text, self.bracketed_paste());
        self.pty.write(&bytes)
    }

    /// Paste a snippet without allowing a line break to become an Enter keypress.
    pub fn send_prompt_snippet(&mut self, text: &str) -> Result<()> {
        if !self.is_running() {
            anyhow::bail!("the focused session is no longer running");
        }
        if text.is_empty() {
            anyhow::bail!("the snippet prompt is empty");
        }
        if text
            .chars()
            .any(|character| character.is_control() && !matches!(character, '\r' | '\n' | '\t'))
        {
            anyhow::bail!("snippet prompts cannot contain terminal control characters");
        }
        let bracketed = self.bracketed_paste();
        if (text.contains('\r') || text.contains('\n')) && !bracketed {
            anyhow::bail!(
                "Copilot is not ready for multiline paste yet; wait for startup to finish"
            );
        }
        let bytes = keys::encode_paste(text, bracketed);
        self.pty.write(&bytes)
    }

    pub fn handle_mouse(&mut self, event: MouseEvent) -> Result<bool> {
        if !self.is_running() {
            return Ok(false);
        }
        let Some((mode, encoding)) = self.with_screen(|screen| {
            (
                screen.mouse_protocol_mode(),
                screen.mouse_protocol_encoding(),
            )
        }) else {
            return Ok(false);
        };
        let coordinates = self.viewport.coordinates(event.column, event.row);

        if mode == vt100::MouseProtocolMode::None {
            if coordinates.is_some() {
                match event.kind {
                    MouseEventKind::ScrollUp => self.scroll(3)?,
                    MouseEventKind::ScrollDown => self.scroll(-3)?,
                    _ => {}
                }
            }
            Ok(false)
        } else {
            if matches!(event.kind, MouseEventKind::Down(_)) && coordinates.is_some() {
                self.mouse_captured = true;
            }
            let coordinates = coordinates.or_else(|| {
                (self.mouse_captured
                    && matches!(event.kind, MouseEventKind::Drag(_) | MouseEventKind::Up(_)))
                .then(|| self.viewport.clamped_coordinates(event.column, event.row))
            });
            let sequence = coordinates
                .and_then(|position| keys::encode_mouse(event, position, mode, encoding));
            if matches!(event.kind, MouseEventKind::Up(_)) {
                self.mouse_captured = false;
            }
            if let Some(sequence) = sequence {
                self.pty.write(&sequence)?;
                return Ok(true);
            }
            Ok(false)
        }
    }

    fn scroll(&mut self, rows: isize) -> Result<()> {
        let mut parser = self
            .parser
            .lock()
            .map_err(|_| anyhow::anyhow!("Terminal parser lock was poisoned"))?;
        let current = parser.screen().scrollback();
        let next = if rows.is_negative() {
            current.saturating_sub(rows.unsigned_abs())
        } else {
            current.saturating_add(rows as usize)
        };
        parser.screen_mut().set_scrollback(next);
        Ok(())
    }

    pub fn resize(&mut self, rows: u16, cols: u16) -> Result<()> {
        self.resize_at(0, 0, rows, cols)
    }

    pub fn resize_at(&mut self, x: u16, y: u16, rows: u16, cols: u16) -> Result<()> {
        let size = pty_size(rows, cols);
        if let Ok(mut parser) = self.parser.lock() {
            parser.screen_mut().set_size(size.rows, size.cols);
        }
        self.pty.resize(size)?;
        self.viewport = Viewport {
            x,
            y,
            rows: size.rows,
            cols: size.cols,
        };
        Ok(())
    }

    pub fn kill(&self) -> Result<()> {
        self.pty.kill()
    }

    pub fn shutdown(&mut self) -> Result<bool> {
        if !self.is_running() {
            return Ok(false);
        }
        let outcome = self
            .pty
            .terminate_and_wait(std::time::Duration::from_secs(3))?;
        self.mark_exited(outcome.code);
        Ok(outcome.terminated)
    }

    /// Pick up an exit that happened without the reader thread noticing yet.
    pub fn poll_exit(&mut self) {
        if self.is_running() {
            if let Some(code) = self.pty.try_wait() {
                self.mark_exited(Some(code));
            }
        }
    }
}

/// Text of one screen row, as characters, for reference scanning.
fn current_timestamp() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis()
        .min(u128::from(u64::MAX)) as u64
}

fn row_characters(screen: &vt100::Screen, row: u16, cols: u16) -> Vec<char> {
    (0..cols)
        .map(|col| {
            screen
                .cell(row, col)
                .and_then(|cell| cell.contents().chars().next())
                .unwrap_or(' ')
        })
        .collect()
}

/// Every distinct `#1234` on screen, in no particular order.
fn github_references(screen: &vt100::Screen) -> Vec<u64> {
    let (rows, cols) = screen.size();
    let mut found = std::collections::BTreeSet::new();
    for row in 0..rows {
        let characters = row_characters(screen, row, cols);
        for (index, character) in characters.iter().enumerate() {
            if *character != '#' {
                continue;
            }
            let start = index + 1;
            let mut end = start;
            while characters.get(end).is_some_and(char::is_ascii_digit) {
                end += 1;
            }
            if end == start {
                continue;
            }
            // A digit immediately before the hash means this is part of a
            // longer token such as a colour code, not a reference.
            if index > 0 && characters[index - 1].is_ascii_alphanumeric() {
                continue;
            }
            if let Ok(number) = characters[start..end]
                .iter()
                .collect::<String>()
                .parse::<u64>()
            {
                if number > 0 {
                    found.insert(number);
                }
            }
        }
    }
    found.into_iter().collect()
}

fn github_reference_at(screen: &vt100::Screen, row: u16, column: u16) -> Option<u64> {
    let (_, cols) = screen.size();
    let characters = row_characters(screen, row, cols);
    let clicked = *characters.get(column as usize)?;
    if clicked != '#' && !clicked.is_ascii_digit() {
        return None;
    }

    let mut hash = column as usize;
    if clicked.is_ascii_digit() {
        while hash > 0 && characters[hash - 1].is_ascii_digit() {
            hash -= 1;
        }
        hash = hash.checked_sub(1)?;
    }
    if characters.get(hash) != Some(&'#') {
        return None;
    }

    let start = hash + 1;
    let mut end = start;
    while characters.get(end).is_some_and(char::is_ascii_digit) {
        end += 1;
    }
    if end == start || column as usize >= end {
        return None;
    }
    characters[start..end]
        .iter()
        .collect::<String>()
        .parse::<u64>()
        .ok()
        .filter(|number| *number > 0)
}

fn normalize_copilot_title(title: &str) -> Option<String> {
    let title = title.trim();
    if title == "GitHub Copilot" {
        return None;
    }
    let title = [
        " - GitHub Copilot",
        " \u{2013} GitHub Copilot",
        " \u{2014} GitHub Copilot",
    ]
    .into_iter()
    .find_map(|suffix| title.strip_suffix(suffix))
    .unwrap_or(title)
    .trim();
    (!title.is_empty()).then(|| title.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crossterm::event::{KeyCode, KeyModifiers};
    use std::io::Write;
    use std::sync::mpsc;
    use std::time::{Duration, Instant};

    #[test]
    fn viewport_translates_outer_mouse_coordinates() {
        let viewport = Viewport {
            x: 2,
            y: 1,
            rows: 10,
            cols: 20,
        };

        assert_eq!(viewport.coordinates(2, 1), Some((1, 1)));
        assert_eq!(viewport.coordinates(21, 10), Some((20, 10)));
        assert_eq!(viewport.coordinates(1, 1), None);
        assert_eq!(viewport.clamped_coordinates(99, 99), (20, 10));
        assert_eq!(viewport.cell_coordinates(2, 1), Some((0, 0)));
        assert_eq!(viewport.cell_coordinates(21, 10), Some((9, 19)));
    }

    #[test]
    fn github_reference_is_detected_on_hash_or_digits() {
        let mut parser = vt100::Parser::new(4, 40, 0);
        parser.process(b"Findings posted to #2029 and #77.");
        let screen = parser.screen();

        for column in 19..=23 {
            assert_eq!(github_reference_at(screen, 0, column), Some(2029));
        }
        assert_eq!(github_reference_at(screen, 0, 18), None);
        assert_eq!(github_reference_at(screen, 0, 24), None);
        assert_eq!(github_reference_at(screen, 0, 30), Some(77));
    }

    #[test]
    fn ordinary_numbers_are_not_github_references() {
        let mut parser = vt100::Parser::new(2, 30, 0);
        parser.process(b"room 376 still 126");

        assert_eq!(github_reference_at(parser.screen(), 0, 6), None);
        assert_eq!(github_reference_at(parser.screen(), 0, 16), None);
    }

    #[test]
    fn screen_scan_finds_every_reference_once() {
        let mut parser = vt100::Parser::new(4, 40, 0);
        parser.process(b"Opened #2029, see #77 and #2029 again.\r\nColour #ff00aa is not one.");

        let mut found = github_references(parser.screen());
        found.sort_unstable();

        // `#ff00aa` yields no digits before the letters, so it never becomes a
        // reference; the duplicate is asked about only once.
        assert_eq!(found, vec![77, 2029]);
    }

    #[test]
    fn screen_scan_ignores_hashes_glued_to_a_word() {
        let mut parser = vt100::Parser::new(2, 40, 0);
        parser.process(b"abc#12 x#9");

        assert!(github_references(parser.screen()).is_empty());
    }

    fn shell_command(script: &str) -> (String, Vec<String>) {
        if cfg!(windows) {
            (
                "cmd.exe".to_string(),
                vec!["/c".to_string(), script.to_string()],
            )
        } else {
            (
                "/bin/sh".to_string(),
                vec!["-c".to_string(), script.to_string()],
            )
        }
    }

    fn test_spec(id: PaneId, program: String, args: Vec<String>) -> PaneSpec {
        PaneSpec {
            id,
            title: "test".to_string(),
            cwd: std::env::temp_dir(),
            session_id: format!("test-session-{id}"),
            program,
            args,
            events_path: None,
            terminal_light_mode: Some(false),
            hooks_active: false,
        }
    }

    #[test]
    fn copilot_branding_is_removed_only_from_the_end_of_titles() {
        assert_eq!(
            normalize_copilot_title("TRenderer - GitHub Copilot").as_deref(),
            Some("TRenderer")
        );
        assert_eq!(
            normalize_copilot_title("GitHub Copilot investigation").as_deref(),
            Some("GitHub Copilot investigation")
        );
        assert_eq!(normalize_copilot_title("GitHub Copilot"), None);
    }

    fn wait_for_exit(rx: &mpsc::Receiver<MuxEvent>, pane: &mut Pane) -> Option<u32> {
        let deadline = Instant::now() + Duration::from_secs(20);
        while Instant::now() < deadline {
            match rx.recv_timeout(Duration::from_millis(500)) {
                Ok(MuxEvent::Exited(_, code)) => {
                    pane.mark_exited(code);
                    return code;
                }
                Ok(_) => continue,
                Err(mpsc::RecvTimeoutError::Timeout) => continue,
                Err(mpsc::RecvTimeoutError::Disconnected) => break,
            }
        }
        None
    }

    fn wait_for_lifecycle(rx: &mpsc::Receiver<MuxEvent>) -> LifecycleEvent {
        let deadline = Instant::now() + Duration::from_secs(3);
        while Instant::now() < deadline {
            match rx.recv_timeout(Duration::from_millis(250)) {
                Ok(MuxEvent::SessionLifecycle(_, event)) => return event,
                Ok(_) | Err(mpsc::RecvTimeoutError::Timeout) => {}
                Err(mpsc::RecvTimeoutError::Disconnected) => break,
            }
        }
        panic!("timed out waiting for session lifecycle event");
    }

    fn wait_for_hook(rx: &mpsc::Receiver<MuxEvent>) -> HookLifecycleEvent {
        let deadline = Instant::now() + Duration::from_secs(3);
        while Instant::now() < deadline {
            match rx.recv_timeout(Duration::from_millis(250)) {
                Ok(MuxEvent::HookLifecycle(_, event)) => return event,
                Ok(_) | Err(mpsc::RecvTimeoutError::Timeout) => {}
                Err(mpsc::RecvTimeoutError::Disconnected) => break,
            }
        }
        panic!("timed out waiting for hook lifecycle event");
    }

    #[test]
    fn pane_monitor_forwards_lifecycle_appends_after_spawn() {
        let directory = tempfile::tempdir().unwrap();
        let events_path = directory.path().join("events.jsonl");
        std::fs::write(&events_path, "{\"type\":\"session.start\"}\n").unwrap();
        let (tx, rx) = mpsc::channel();
        let (program, args) = shell_command(if cfg!(windows) {
            "ping -n 30 127.0.0.1 > nul"
        } else {
            "sleep 30"
        });
        let mut spec = test_spec(39, program, args);
        spec.events_path = Some(events_path.clone());
        let mut pane = Pane::spawn(spec, 24, 80, tx).unwrap();

        let mut events = std::fs::OpenOptions::new()
            .append(true)
            .open(&events_path)
            .unwrap();
        writeln!(
            events,
            r#"{{"type":"tool.execution_start","data":{{"toolName":"ask_user","toolCallId":"question-1"}}}}"#
        )
        .unwrap();
        events.flush().unwrap();
        assert_eq!(
            wait_for_lifecycle(&rx),
            LifecycleEvent::InputRequested {
                tool_call_id: "question-1".into(),
                kind: InputKind::Question,
            }
        );

        writeln!(
            events,
            r#"{{"type":"tool.execution_complete","data":{{"toolCallId":"question-1","success":true}}}}"#
        )
        .unwrap();
        events.flush().unwrap();
        assert_eq!(
            wait_for_lifecycle(&rx),
            LifecycleEvent::InputResolved {
                tool_call_id: "question-1".into(),
                kind: InputKind::Question,
            }
        );
        assert!(pane.shutdown().unwrap());
    }

    #[test]
    fn pane_renders_child_output_into_the_screen() {
        let (tx, rx) = mpsc::channel();
        let (program, args) = shell_command("echo pane-hello");
        let mut pane = Pane::spawn(test_spec(1, program, args), 24, 80, tx).unwrap();

        wait_for_exit(&rx, &mut pane);

        let contents = pane.with_screen(|screen| screen.contents()).unwrap();
        assert!(
            contents.contains("pane-hello"),
            "screen did not contain the output: {contents:?}"
        );
        assert!(!pane.is_running());
    }

    #[test]
    fn a_child_that_enables_bracketed_paste_is_detected() {
        // Copilot CLI turns the mode on at startup; the paste path relies on seeing it
        // so pasted newlines travel as text instead of Enter keystrokes.
        let (tx, rx) = mpsc::channel();
        let (program, args) = shell_command("echo done");
        let mut pane = Pane::spawn(test_spec(1, program, args), 24, 80, tx).unwrap();

        assert!(!pane.bracketed_paste());
        pane.feed_synthetic(b"\x1b[?2004h");
        assert!(pane.bracketed_paste());
        pane.send_prompt_snippet("line one\nline two").unwrap();
        pane.feed_synthetic(b"\x1b[?2004l");
        assert!(!pane.bracketed_paste());

        wait_for_exit(&rx, &mut pane);
    }

    #[test]
    fn startup_readiness_is_cached_without_rebuilding_screen_contents() {
        let (tx, _) = mpsc::channel();
        let (program, args) = shell_command(if cfg!(windows) {
            "ping -n 30 127.0.0.1 > nul"
        } else {
            "sleep 30"
        });
        let mut pane = Pane::spawn(test_spec(1, program, args), 24, 80, tx).unwrap();

        assert!(pane.is_blank());
        pane.feed_synthetic(b"\x1b[?2004h\x1b[6n");
        assert!(pane.is_blank(), "control sequences are not visible output");
        pane.feed_synthetic(b"Copilot ready");
        assert!(!pane.is_blank());
        assert!(!pane.is_blank(), "subsequent checks are atomic reads");

        pane.shutdown().unwrap();
    }

    #[test]
    fn pane_monitor_forwards_authoritative_hook_appends_after_spawn() {
        let directory = tempfile::tempdir().unwrap();
        let events_path = directory.path().join("events.jsonl");
        std::fs::write(&events_path, "{\"type\":\"session.start\"}\n").unwrap();
        let (tx, rx) = mpsc::channel();
        let (program, args) = shell_command(if cfg!(windows) {
            "ping -n 30 127.0.0.1 > nul"
        } else {
            "sleep 30"
        });
        let mut spec = test_spec(44, program, args);
        spec.events_path = Some(events_path);
        let mut pane = Pane::spawn(spec, 24, 80, tx).unwrap();
        let hook_path = directory.path().join(".cst-lifecycle.jsonl");

        std::fs::write(
            &hook_path,
            format!(
                "{}\n",
                serde_json::to_string(&HookLifecycleEvent::Working { timestamp: 1 }).unwrap()
            ),
        )
        .unwrap();

        assert_eq!(
            wait_for_hook(&rx),
            HookLifecycleEvent::Working { timestamp: 1 }
        );
        pane.shutdown().unwrap();
    }

    /// Reproduces the stale spinner on a re-attached session.
    ///
    /// A session that finished its turn hours ago, with a background process Copilot
    /// still counts as "working", so it re-announces progress over OSC on resume. Before
    /// this was fixed the pane started with no lifecycle state at all — it reads the
    /// journal from end-of-file, so nothing arrives until the next turn — and the raw
    /// OSC won by default. The spinner then never stopped, because the background
    /// process never ends.
    #[test]
    fn a_session_that_finished_its_turn_does_not_spin_when_reattached() {
        let directory = tempfile::tempdir().unwrap();
        let events_path = directory.path().join("events.jsonl");
        std::fs::write(&events_path, "{\"type\":\"session.start\"}\n").unwrap();
        // The turn ended long before this pane existed, which is the whole point: the
        // record is already on disk when the pane opens, so nothing will be replayed.
        std::fs::write(
            directory.path().join(".cst-lifecycle.jsonl"),
            format!(
                "{}\n{}\n",
                serde_json::to_string(&HookLifecycleEvent::Working { timestamp: 1 }).unwrap(),
                serde_json::to_string(&HookLifecycleEvent::Ready { timestamp: 2 }).unwrap()
            ),
        )
        .unwrap();

        let (tx, _rx) = mpsc::channel();
        let (program, args) = shell_command(if cfg!(windows) {
            "ping -n 30 127.0.0.1 > nul"
        } else {
            "sleep 30"
        });
        let mut spec = test_spec(45, program, args);
        spec.events_path = Some(events_path);
        spec.hooks_active = true;
        let mut pane = Pane::spawn(spec, 24, 80, tx).unwrap();

        // Copilot re-announcing progress on resume, because its own idea of "working"
        // counts the background process it started before the turn ended.
        pane.feed_synthetic(b"\x1b]9;4;3;0\x1b\\");
        pane.refresh_from_callbacks(false);

        assert_eq!(
            pane.effective_progress_state(),
            crate::host_terminal::ProgressState::Clear,
            "a child that reports its own state was not mid-turn when we attached"
        );
        pane.shutdown().unwrap();
    }

    fn key(code: crossterm::event::KeyCode) -> crossterm::event::KeyEvent {
        crossterm::event::KeyEvent::new(code, crossterm::event::KeyModifiers::NONE)
    }

    #[test]
    fn typing_marks_a_draft_and_sending_it_clears_it() {
        // A wake-up that lands between these two is the bug this prevents: it pastes
        // onto the half-written message and submits the pair.
        use crossterm::event::KeyCode;
        let (tx, _rx) = mpsc::channel();
        let (program, args) = shell_command(if cfg!(windows) {
            "ping -n 30 127.0.0.1 > nul"
        } else {
            "sleep 30"
        });
        let mut pane = Pane::spawn(test_spec(49, program, args), 24, 80, tx).unwrap();

        assert!(!pane.has_draft(), "nothing typed yet");
        pane.note_user_key(&key(KeyCode::Char('h')));
        assert!(pane.has_draft());

        pane.note_user_key(&key(KeyCode::Enter));
        assert!(!pane.has_draft(), "sending empties the composer");
        pane.shutdown().unwrap();
    }

    #[test]
    fn editing_keeps_the_draft_and_a_newline_chord_does_not_send_it() {
        // Backspace could be clearing the last character or the whole message; CST
        // cannot see which. It keeps the draft, because delaying a wake-up costs a
        // little and overwriting what somebody wrote costs a lot.
        use crossterm::event::{KeyCode, KeyModifiers};
        let (tx, _rx) = mpsc::channel();
        let (program, args) = shell_command(if cfg!(windows) {
            "ping -n 30 127.0.0.1 > nul"
        } else {
            "sleep 30"
        });
        let mut pane = Pane::spawn(test_spec(50, program, args), 24, 80, tx).unwrap();

        // Backspace on an empty composer deleted nothing, so there is nothing to keep.
        // It used to count as typing, which marked the tab and held that session's
        // thread notices until they timed out into the inbox.
        pane.note_user_key(&key(KeyCode::Backspace));
        assert!(
            !pane.has_draft(),
            "backspace cannot put text into an empty composer"
        );
        pane.note_user_key(&key(KeyCode::Delete));
        assert!(!pane.has_draft(), "nor can delete");

        pane.note_user_key(&key(KeyCode::Char('h')));
        pane.note_user_key(&key(KeyCode::Backspace));
        assert!(pane.has_draft(), "backspace is editing, not sending");

        pane.note_user_key(&crossterm::event::KeyEvent::new(
            KeyCode::Enter,
            KeyModifiers::CONTROL,
        ));
        assert!(
            pane.has_draft(),
            "a chorded Enter inserts a newline rather than submitting"
        );
        pane.shutdown().unwrap();
    }

    #[test]
    fn a_control_chord_is_a_command_and_not_the_start_of_a_message() {
        use crossterm::event::{KeyCode, KeyModifiers};
        let (tx, _rx) = mpsc::channel();
        let (program, args) = shell_command(if cfg!(windows) {
            "ping -n 30 127.0.0.1 > nul"
        } else {
            "sleep 30"
        });
        let mut pane = Pane::spawn(test_spec(51, program, args), 24, 80, tx).unwrap();

        pane.note_user_key(&crossterm::event::KeyEvent::new(
            KeyCode::Char('l'),
            KeyModifiers::CONTROL,
        ));
        assert!(!pane.has_draft());
        pane.shutdown().unwrap();
    }

    #[test]
    fn a_turn_starting_clears_a_draft_submitted_by_a_route_cst_never_saw() {
        // A mouse click, or a paste ending in a newline, sends the message without CST
        // seeing an Enter. The turn beginning is the authoritative evidence.
        let (tx, _rx) = mpsc::channel();
        let (program, args) = shell_command(if cfg!(windows) {
            "ping -n 30 127.0.0.1 > nul"
        } else {
            "sleep 30"
        });
        let mut pane = Pane::spawn(test_spec(52, program, args), 24, 80, tx).unwrap();
        pane.note_user_key(&key(crossterm::event::KeyCode::Char('h')));
        assert!(pane.has_draft());

        pane.apply_hook(HookLifecycleEvent::Working { timestamp: 1 }, false);

        assert!(!pane.has_draft());
        pane.shutdown().unwrap();
    }

    #[test]
    fn a_pane_tells_its_agent_which_session_it_is_and_where_cst_lives() {
        // Both are the answer to the same failure: an agent inside the pane is a
        // separate process that cannot ask the TUI anything. Without the session id it
        // does not know who it is; without the path it reports the whole feature as
        // unavailable, because `cst` is a shell function that its shell never loaded.
        let (tx, _rx) = mpsc::channel();
        let (program, args) = shell_command(if cfg!(windows) {
            "echo SEEN-%CST_SESSION_ID%-%CST_BIN%"
        } else {
            "echo SEEN-$CST_SESSION_ID-$CST_BIN"
        });
        let mut pane = Pane::spawn(test_spec(48, program, args), 24, 200, tx).unwrap();

        let deadline = std::time::Instant::now() + Duration::from_secs(20);
        let mut screen = String::new();
        while std::time::Instant::now() < deadline {
            pane.refresh_from_callbacks(false);
            screen = pane
                .with_screen(|screen| screen.contents())
                .unwrap_or_default();
            if screen.contains("SEEN-") && screen.contains("copilot-session-tui") {
                break;
            }
            std::thread::sleep(Duration::from_millis(50));
        }

        assert!(
            screen.contains("test-session-48"),
            "the pane must name its session, got: {screen}"
        );
        assert!(
            screen.contains("copilot-session-tui"),
            "the pane must say where CST lives, got: {screen}"
        );
        pane.shutdown().unwrap();
    }

    #[test]
    fn a_session_resumed_but_not_yet_prompted_is_also_covered() {
        // The journal only appears once something is submitted, so keying the fix on
        // "has this session reported before" would have missed every session that was
        // resumed and left alone — which is exactly the state a stale spinner sits in.
        let directory = tempfile::tempdir().unwrap();
        let events_path = directory.path().join("events.jsonl");
        std::fs::write(&events_path, "{\"type\":\"session.start\"}\n").unwrap();

        let (tx, _rx) = mpsc::channel();
        let (program, args) = shell_command(if cfg!(windows) {
            "ping -n 30 127.0.0.1 > nul"
        } else {
            "sleep 30"
        });
        let mut spec = test_spec(47, program, args);
        spec.events_path = Some(events_path);
        spec.hooks_active = true;
        let mut pane = Pane::spawn(spec, 24, 80, tx).unwrap();

        pane.feed_synthetic(b"\x1b]9;4;3;0\x1b\\");
        pane.refresh_from_callbacks(false);

        assert_eq!(
            pane.effective_progress_state(),
            crate::host_terminal::ProgressState::Clear,
            "no journal yet, but the hooks will still report"
        );
        pane.shutdown().unwrap();
    }

    #[test]
    fn a_pane_without_hooks_installed_still_shows_raw_progress() {
        // The guard on the fix above. With nothing to report lifecycle state, the raw
        // sequence is the only signal there is, and suppressing it would leave anyone
        // who has not installed the plugin with no progress indication at all.
        let directory = tempfile::tempdir().unwrap();
        let events_path = directory.path().join("events.jsonl");
        std::fs::write(&events_path, "{\"type\":\"session.start\"}\n").unwrap();
        // A journal left behind by hooks that have since been uninstalled. It proves
        // they once worked, which is not a reason to believe they work now.
        std::fs::write(
            directory.path().join(".cst-lifecycle.jsonl"),
            format!(
                "{}\n",
                serde_json::to_string(&HookLifecycleEvent::Ready { timestamp: 2 }).unwrap()
            ),
        )
        .unwrap();

        let (tx, _rx) = mpsc::channel();
        let (program, args) = shell_command(if cfg!(windows) {
            "ping -n 30 127.0.0.1 > nul"
        } else {
            "sleep 30"
        });
        let mut spec = test_spec(46, program, args);
        spec.events_path = Some(events_path);
        spec.hooks_active = false;
        let mut pane = Pane::spawn(spec, 24, 80, tx).unwrap();

        pane.feed_synthetic(b"\x1b]9;4;3;0\x1b\\");
        pane.refresh_from_callbacks(false);

        assert_eq!(
            pane.effective_progress_state(),
            crate::host_terminal::ProgressState::Indeterminate,
            "without hooks the raw sequence is all there is"
        );
        pane.shutdown().unwrap();
    }

    #[test]
    fn background_work_completion_sets_attention_until_acknowledged() {
        let (tx, _) = mpsc::channel();
        let (program, args) = shell_command(if cfg!(windows) {
            "ping -n 30 127.0.0.1 > nul"
        } else {
            "sleep 30"
        });
        let mut pane = Pane::spawn(test_spec(31, program, args), 24, 80, tx).unwrap();

        pane.feed_synthetic(b"\x1b]9;4;3;0\x1b\\");
        pane.refresh_from_callbacks(false);
        assert!(
            !pane.needs_attention(),
            "starting work clears old attention"
        );

        pane.feed_synthetic(b"\x1b]9;4;0;0\x1b\\");
        let (_, notifications) = pane.refresh_from_callbacks(false);
        assert_eq!(notifications, vec![PaneNotification::Ready]);
        assert!(pane.needs_attention());
        assert!(pane.display_title().starts_with("? "));

        pane.acknowledge_attention();
        assert!(!pane.needs_attention());
        assert_eq!(pane.display_title(), pane.title);
        pane.shutdown().unwrap();
    }

    #[test]
    fn an_unwatched_finish_reads_as_unread_but_a_question_does_not() {
        let (tx, _) = mpsc::channel();
        let (program, args) = shell_command(if cfg!(windows) {
            "ping -n 30 127.0.0.1 > nul"
        } else {
            "sleep 30"
        });
        let mut pane = Pane::spawn(test_spec(41, program, args), 24, 80, tx).unwrap();

        pane.feed_synthetic(b"\x1b]9;4;3;0\x1b\\");
        pane.refresh_from_callbacks(false);
        assert!(!pane.is_unread(), "work in flight is not a result to read");

        pane.feed_synthetic(b"\x1b]9;4;0;0\x1b\\");
        pane.refresh_from_callbacks(false);
        assert!(pane.is_unread(), "finished while the user was elsewhere");

        // Going back to the tab is what reads it.
        pane.acknowledge_attention();
        assert!(!pane.is_unread());

        // A live question also wants the user, but it is not an unread result: it
        // blocks the session, and the strip has to be able to say so.
        pane.apply_lifecycle(LifecycleEvent::InputRequested {
            tool_call_id: "question-41".into(),
            kind: InputKind::Question,
        });
        assert!(pane.needs_attention());
        assert!(pane.requires_user_action());
        assert!(!pane.is_unread());
        pane.shutdown().unwrap();
    }

    /// A turn you watched finish neither marks the tab nor notifies you.
    ///
    /// This used to send the notification anyway, on the reasoning that a phone's
    /// notification list is a log of what your agents did and should not depend on where
    /// you happened to be looking. That holds for a phone you pick up later; it does not
    /// hold once the same channel is being read on the machine you are sitting at, where
    /// it is just an alert about something already on screen. The marker and the
    /// notification now answer the same question.
    #[test]
    fn a_completion_you_watched_notifies_nobody_and_marks_nothing() {
        let (tx, _) = mpsc::channel();
        let (program, args) = shell_command(if cfg!(windows) {
            "ping -n 30 127.0.0.1 > nul"
        } else {
            "sleep 30"
        });
        let mut pane = Pane::spawn(test_spec(32, program, args), 24, 80, tx).unwrap();

        pane.feed_synthetic(b"\x1b]9;4;3;0\x1b\\");
        pane.refresh_from_callbacks(true);
        pane.feed_synthetic(b"\x1b]9;4;0;0\x1b\\");
        let (_, notifications) = pane.refresh_from_callbacks(true);

        assert!(!pane.needs_attention());
        assert!(
            notifications.is_empty(),
            "a turn finished under your nose is not news, got: {notifications:?}"
        );
        pane.shutdown().unwrap();
    }

    #[test]
    fn question_state_overrides_progress_until_matching_completion() {
        let (tx, _) = mpsc::channel();
        let (program, args) = shell_command(if cfg!(windows) {
            "ping -n 30 127.0.0.1 > nul"
        } else {
            "sleep 30"
        });
        let mut pane = Pane::spawn(test_spec(37, program, args), 24, 80, tx).unwrap();
        pane.apply_signals(
            PaneSignals {
                events: vec![PaneSignalEvent::Progress(
                    crate::host_terminal::ProgressState::Indeterminate,
                )],
                ..Default::default()
            },
            true,
        );

        assert_eq!(
            pane.apply_lifecycle(LifecycleEvent::InputRequested {
                tool_call_id: "question-1".into(),
                kind: InputKind::Question,
            }),
            Some(PaneNotification::Question)
        );
        assert!(pane.requires_user_action());
        assert_eq!(
            pane.effective_progress_state(),
            crate::host_terminal::ProgressState::Clear
        );
        pane.acknowledge_attention();
        assert!(
            pane.needs_attention(),
            "focus cannot dismiss a live question"
        );
        assert!(pane.display_title().starts_with("? "));

        let (_, notifications) = pane.apply_signals(
            PaneSignals {
                events: vec![PaneSignalEvent::Progress(
                    crate::host_terminal::ProgressState::Indeterminate,
                )],
                ..Default::default()
            },
            true,
        );
        assert!(notifications.is_empty());
        assert_eq!(
            pane.effective_progress_state(),
            crate::host_terminal::ProgressState::Clear
        );

        pane.apply_lifecycle(LifecycleEvent::InputResolved {
            tool_call_id: "another-question".into(),
            kind: InputKind::Question,
        });
        assert!(pane.requires_user_action());
        pane.apply_lifecycle(LifecycleEvent::InputResolved {
            tool_call_id: "question-1".into(),
            kind: InputKind::Question,
        });
        assert!(!pane.requires_user_action());
        assert_eq!(
            pane.effective_progress_state(),
            crate::host_terminal::ProgressState::Indeterminate
        );
        assert_eq!(pane.display_title(), pane.title);
        pane.shutdown().unwrap();
    }

    #[test]
    fn all_pending_inputs_must_resolve_and_reset_clears_stale_waits() {
        let (tx, _) = mpsc::channel();
        let (program, args) = shell_command(if cfg!(windows) {
            "ping -n 30 127.0.0.1 > nul"
        } else {
            "sleep 30"
        });
        let mut pane = Pane::spawn(test_spec(38, program, args), 24, 80, tx).unwrap();

        assert_eq!(
            pane.apply_lifecycle(LifecycleEvent::InputRequested {
                tool_call_id: "question".into(),
                kind: InputKind::Question,
            }),
            Some(PaneNotification::Question)
        );
        assert_eq!(
            pane.apply_lifecycle(LifecycleEvent::InputRequested {
                tool_call_id: "plan".into(),
                kind: InputKind::PlanApproval,
            }),
            Some(PaneNotification::PlanApproval)
        );
        pane.apply_lifecycle(LifecycleEvent::InputResolved {
            tool_call_id: "question".into(),
            kind: InputKind::Question,
        });
        assert!(pane.requires_user_action());

        pane.apply_lifecycle(LifecycleEvent::Reset);
        assert!(!pane.requires_user_action());
        assert!(!pane.needs_attention());
        pane.shutdown().unwrap();
    }

    #[test]
    fn hook_lifecycle_overrides_patchy_raw_progress_until_next_turn() {
        let (tx, _) = mpsc::channel();
        let (program, args) = shell_command(if cfg!(windows) {
            "ping -n 30 127.0.0.1 > nul"
        } else {
            "sleep 30"
        });
        let mut pane = Pane::spawn(test_spec(42, program, args), 24, 80, tx).unwrap();

        let working = pane.apply_hook(HookLifecycleEvent::Working { timestamp: 1 }, false);
        assert!(working.cycle_started);
        assert_eq!(
            pane.effective_progress_state(),
            crate::host_terminal::ProgressState::Indeterminate
        );
        assert!(!pane.should_forward_raw_progress(crate::host_terminal::ProgressState::Clear));
        assert!(
            pane.should_forward_raw_progress(crate::host_terminal::ProgressState::Indeterminate)
        );

        let ready = pane.apply_hook(HookLifecycleEvent::Ready { timestamp: 2 }, false);
        assert!(ready.notification.is_none());
        assert_eq!(ready.ready_confirmation, Some(2));
        assert!(!pane.needs_attention());
        assert_eq!(
            pane.effective_progress_state(),
            crate::host_terminal::ProgressState::Clear
        );
        assert!(
            !pane.should_forward_raw_progress(crate::host_terminal::ProgressState::Indeterminate)
        );
        assert_eq!(pane.confirm_hook_ready(2, false), None);
        assert!(!pane.needs_attention());

        pane.record_progress_state(crate::host_terminal::ProgressState::Error);
        pane.apply_hook(HookLifecycleEvent::Working { timestamp: 3 }, false);
        assert_eq!(
            pane.effective_progress_state(),
            crate::host_terminal::ProgressState::Indeterminate,
            "a previous raw error cannot leak into a new authoritative cycle"
        );
        pane.shutdown().unwrap();
    }

    #[test]
    fn late_async_attention_cannot_override_newer_ready_state() {
        let (tx, _) = mpsc::channel();
        let (program, args) = shell_command(if cfg!(windows) {
            "ping -n 30 127.0.0.1 > nul"
        } else {
            "sleep 30"
        });
        let mut pane = Pane::spawn(test_spec(45, program, args), 24, 80, tx).unwrap();
        pane.apply_hook(HookLifecycleEvent::Working { timestamp: 10 }, false);
        pane.apply_hook(HookLifecycleEvent::Ready { timestamp: 30 }, false);

        let stale = pane.apply_hook(
            HookLifecycleEvent::Awaiting {
                timestamp: 20,
                attention: HookAttention::Permission,
            },
            false,
        );

        assert_eq!(stale, PaneHookOutcome::default());
        assert!(!pane.requires_user_action());
        assert_eq!(
            pane.effective_progress_state(),
            crate::host_terminal::ProgressState::Clear
        );
        pane.shutdown().unwrap();
    }

    /// The reported bug: a spinner that outlived its turn by over an hour.
    ///
    /// Copilot finished, `agentStop` fired, and CST went idle — and then Copilot went on
    /// animating progress because an attached background shell (a web server, in the
    /// report) was still alive. That animation flipped the pane back to working, where it
    /// stayed for 72 minutes: nothing returns a pane to idle except another ready hook,
    /// and no further hook fires when no turn is running.
    #[test]
    fn a_background_shell_animating_progress_does_not_pin_the_pane_to_working() {
        let (tx, _) = mpsc::channel();
        let (program, args) = shell_command(if cfg!(windows) {
            "ping -n 30 127.0.0.1 > nul"
        } else {
            "sleep 30"
        });
        let mut pane = Pane::spawn(test_spec(53, program, args), 24, 80, tx).unwrap();

        pane.apply_hook(HookLifecycleEvent::Working { timestamp: 1 }, false);
        pane.apply_hook(HookLifecycleEvent::Ready { timestamp: 2 }, false);
        pane.confirm_hook_ready(2, false);
        assert_eq!(
            pane.effective_progress_state(),
            crate::host_terminal::ProgressState::Clear,
            "the turn is over"
        );

        // The shell is still attached, so Copilot keeps drawing its spinner.
        pane.record_progress_state(crate::host_terminal::ProgressState::Indeterminate);
        assert_eq!(
            pane.effective_progress_state(),
            crate::host_terminal::ProgressState::Indeterminate,
            "progress after a stop is still taken as a turn resuming, which it may be"
        );

        // The event log settles it: Copilot has written nothing and is running no tool.
        pane.apply_lifecycle(LifecycleEvent::Quiet);

        assert_eq!(
            pane.effective_progress_state(),
            crate::host_terminal::ProgressState::Clear,
            "a session writing nothing at all is not running a turn"
        );
        assert!(!pane.is_working());

        // And it stays retired. Clearing once would not have been enough: the shell is
        // still attached and the child goes on animating, so the next sequence would
        // promote the pane straight back and the tab would blink once a minute forever.
        pane.record_progress_state(crate::host_terminal::ProgressState::Indeterminate);
        pane.record_progress_state(crate::host_terminal::ProgressState::Indeterminate);
        assert_eq!(
            pane.effective_progress_state(),
            crate::host_terminal::ProgressState::Clear,
            "progress the log has already contradicted does not get a second hearing"
        );

        // A real turn starts. The hook is authority and restores normal service.
        pane.apply_hook(HookLifecycleEvent::Working { timestamp: 3 }, false);
        assert!(pane.is_working(), "a hook is still believed immediately");
        pane.shutdown().unwrap();
    }

    /// The guard on the fix: silence is not idleness while a tool is running.
    ///
    /// A build or a test run writes nothing to the event log for minutes — I measured
    /// gaps up to 52 minutes with a tool call outstanding — so quiet must never cancel a
    /// working state that a hook put there. Only a belief resting on progress sequences
    /// alone is given up.
    #[test]
    fn quiet_does_not_stop_the_spinner_on_a_turn_the_hooks_vouched_for() {
        let (tx, _) = mpsc::channel();
        let (program, args) = shell_command(if cfg!(windows) {
            "ping -n 30 127.0.0.1 > nul"
        } else {
            "sleep 30"
        });
        let mut pane = Pane::spawn(test_spec(54, program, args), 24, 80, tx).unwrap();

        // Get the pane into the state the fix targets first, so the clear is what is
        // under test and not merely the initial value of the flag.
        pane.apply_hook(HookLifecycleEvent::Working { timestamp: 1 }, false);
        pane.apply_hook(HookLifecycleEvent::Ready { timestamp: 2 }, false);
        pane.confirm_hook_ready(2, false);
        pane.record_progress_state(crate::host_terminal::ProgressState::Indeterminate);

        // Now a real turn starts. The hook is authority, and it outranks the guess the
        // progress sequence produced a moment ago.
        pane.apply_hook(HookLifecycleEvent::Working { timestamp: 3 }, false);
        assert!(pane.is_working());

        pane.apply_lifecycle(LifecycleEvent::Quiet);

        assert!(
            pane.is_working(),
            "a long silent tool call is still a running turn"
        );
        assert_eq!(
            pane.effective_progress_state(),
            crate::host_terminal::ProgressState::Indeterminate
        );
        pane.shutdown().unwrap();
    }

    #[test]
    fn raw_work_after_agent_stop_cancels_premature_ready_confirmation() {
        let (tx, _) = mpsc::channel();
        let (program, args) = shell_command(if cfg!(windows) {
            "ping -n 30 127.0.0.1 > nul"
        } else {
            "sleep 30"
        });
        let mut pane = Pane::spawn(test_spec(47, program, args), 24, 80, tx).unwrap();
        pane.apply_hook(HookLifecycleEvent::Working { timestamp: 1 }, false);
        assert!(!pane.record_progress_state(crate::host_terminal::ProgressState::Indeterminate));
        let pending = pane.apply_hook(HookLifecycleEvent::Ready { timestamp: 2 }, false);
        assert_eq!(pending.ready_confirmation, Some(2));

        assert!(!pane.record_progress_state(crate::host_terminal::ProgressState::Indeterminate));
        let final_ready = pane.apply_hook(HookLifecycleEvent::Ready { timestamp: 3 }, false);
        assert_eq!(final_ready.ready_confirmation, Some(3));
        assert_eq!(pane.confirm_hook_ready(2, false), None);
        assert_eq!(pane.confirm_hook_ready(3, false), None);
        pane.shutdown().unwrap();
    }

    #[test]
    fn raw_work_after_confirmed_ready_reenters_authoritative_working() {
        let (tx, _) = mpsc::channel();
        let (program, args) = shell_command(if cfg!(windows) {
            "ping -n 30 127.0.0.1 > nul"
        } else {
            "sleep 30"
        });
        let mut pane = Pane::spawn(test_spec(48, program, args), 24, 80, tx).unwrap();
        pane.apply_hook(HookLifecycleEvent::Working { timestamp: 1 }, false);
        pane.apply_hook(HookLifecycleEvent::Ready { timestamp: 2 }, false);
        assert_eq!(pane.confirm_hook_ready(2, false), None);

        pane.record_progress_state(crate::host_terminal::ProgressState::Indeterminate);

        assert!(pane.is_working());
        assert!(!pane.needs_attention());
        assert_eq!(
            pane.effective_progress_state(),
            crate::host_terminal::ProgressState::Indeterminate
        );
        let final_ready = pane.apply_hook(HookLifecycleEvent::Ready { timestamp: 3 }, false);
        assert_eq!(final_ready.ready_confirmation, Some(3));
        pane.shutdown().unwrap();
    }

    #[test]
    fn bell_finalizes_hook_ready_candidate() {
        let (tx, _) = mpsc::channel();
        let (program, args) = shell_command(if cfg!(windows) {
            "ping -n 30 127.0.0.1 > nul"
        } else {
            "sleep 30"
        });
        let mut pane = Pane::spawn(test_spec(49, program, args), 24, 80, tx).unwrap();
        pane.apply_hook(HookLifecycleEvent::Working { timestamp: 1 }, false);
        pane.apply_hook(HookLifecycleEvent::Ready { timestamp: 2 }, false);

        let (outcome, notifications) = pane.apply_signals(
            PaneSignals {
                events: vec![PaneSignalEvent::Bell],
                ..Default::default()
            },
            false,
        );

        assert!(outcome.bell);
        assert_eq!(notifications, vec![PaneNotification::Ready]);
        assert!(pane.needs_attention());
        assert_eq!(pane.confirm_hook_ready(2, false), None);
        pane.shutdown().unwrap();
    }

    #[test]
    fn raw_clear_before_delayed_ready_hook_is_preserved_as_completion_evidence() {
        let (tx, _) = mpsc::channel();
        let (program, args) = shell_command(if cfg!(windows) {
            "ping -n 30 127.0.0.1 > nul"
        } else {
            "sleep 30"
        });
        let mut pane = Pane::spawn(test_spec(51, program, args), 24, 80, tx).unwrap();
        pane.apply_hook(HookLifecycleEvent::Working { timestamp: 1 }, false);

        let (_, notifications) = pane.apply_signals(
            PaneSignals {
                events: vec![PaneSignalEvent::Progress(
                    crate::host_terminal::ProgressState::Clear,
                )],
                ..Default::default()
            },
            false,
        );
        assert!(notifications.is_empty());
        let ready = pane.apply_hook(HookLifecycleEvent::Ready { timestamp: 2 }, false);

        assert_eq!(ready.ready_confirmation, Some(2));
        assert_eq!(
            pane.confirm_hook_ready(2, false),
            Some(PaneNotification::Ready)
        );
        assert!(pane.needs_attention());
        pane.shutdown().unwrap();
    }

    /// The same completion, watched: no marker and no notification.
    ///
    /// The twin of the test above, and the only cover for this path — sessions running
    /// the lifecycle plugin finish through `confirm_hook_ready` rather than through raw
    /// progress signals, so gating one without the other would leave the common case
    /// notifying exactly as before. Verified by reverting the gate and watching this
    /// fail on its own.
    #[test]
    fn a_watched_completion_confirmed_by_the_hooks_notifies_nobody() {
        let (tx, _) = mpsc::channel();
        let (program, args) = shell_command(if cfg!(windows) {
            "ping -n 30 127.0.0.1 > nul"
        } else {
            "sleep 30"
        });
        let mut pane = Pane::spawn(test_spec(52, program, args), 24, 80, tx).unwrap();
        pane.apply_hook(HookLifecycleEvent::Working { timestamp: 1 }, true);

        pane.apply_signals(
            PaneSignals {
                events: vec![PaneSignalEvent::Progress(
                    crate::host_terminal::ProgressState::Clear,
                )],
                ..Default::default()
            },
            true,
        );
        let ready = pane.apply_hook(HookLifecycleEvent::Ready { timestamp: 2 }, true);

        assert_eq!(
            ready.ready_confirmation,
            Some(2),
            "the turn still completes; only the telling changes"
        );
        assert_eq!(pane.confirm_hook_ready(2, true), None);
        assert!(!pane.needs_attention());
        pane.shutdown().unwrap();
    }

    #[test]
    fn raw_clear_after_ready_confirmation_still_finalizes_completion() {
        let (tx, _) = mpsc::channel();
        let (program, args) = shell_command(if cfg!(windows) {
            "ping -n 30 127.0.0.1 > nul"
        } else {
            "sleep 30"
        });
        let mut pane = Pane::spawn(test_spec(52, program, args), 24, 80, tx).unwrap();
        pane.apply_hook(HookLifecycleEvent::Working { timestamp: 1 }, false);
        pane.apply_hook(HookLifecycleEvent::Ready { timestamp: 2 }, false);
        assert_eq!(pane.confirm_hook_ready(2, false), None);

        let (_, notifications) = pane.apply_signals(
            PaneSignals {
                events: vec![PaneSignalEvent::Progress(
                    crate::host_terminal::ProgressState::Clear,
                )],
                ..Default::default()
            },
            false,
        );

        assert_eq!(notifications, vec![PaneNotification::Ready]);
        assert!(pane.needs_attention());
        pane.shutdown().unwrap();
    }

    #[test]
    fn coalesced_clear_then_working_cancels_hook_ready_candidate() {
        let (tx, _) = mpsc::channel();
        let (program, args) = shell_command(if cfg!(windows) {
            "ping -n 30 127.0.0.1 > nul"
        } else {
            "sleep 30"
        });
        let mut pane = Pane::spawn(test_spec(50, program, args), 24, 80, tx).unwrap();
        pane.apply_hook(HookLifecycleEvent::Working { timestamp: 1 }, false);
        pane.record_progress_state(crate::host_terminal::ProgressState::Indeterminate);
        pane.apply_hook(HookLifecycleEvent::Ready { timestamp: 2 }, false);
        assert!(!pane.record_progress_state(crate::host_terminal::ProgressState::Clear));
        assert!(pane.record_progress_state(crate::host_terminal::ProgressState::Indeterminate));

        let (_, notifications) = pane.apply_signals(
            PaneSignals {
                events: vec![
                    PaneSignalEvent::Progress(crate::host_terminal::ProgressState::Clear),
                    PaneSignalEvent::Progress(crate::host_terminal::ProgressState::Indeterminate),
                ],
                ..Default::default()
            },
            false,
        );

        assert!(notifications.is_empty());
        assert!(pane.is_working());
        assert_eq!(
            pane.effective_progress_state(),
            crate::host_terminal::ProgressState::Indeterminate
        );
        pane.shutdown().unwrap();
    }

    #[test]
    fn resolved_attention_watermark_rejects_delayed_hook_notification() {
        let (tx, _) = mpsc::channel();
        let (program, args) = shell_command(if cfg!(windows) {
            "ping -n 30 127.0.0.1 > nul"
        } else {
            "sleep 30"
        });
        let mut pane = Pane::spawn(test_spec(46, program, args), 24, 80, tx).unwrap();
        pane.apply_hook(HookLifecycleEvent::Working { timestamp: 1 }, false);
        pane.apply_lifecycle(LifecycleEvent::InputRequested {
            tool_call_id: "question-1".into(),
            kind: InputKind::Question,
        });
        pane.apply_lifecycle(LifecycleEvent::InputResolved {
            tool_call_id: "question-1".into(),
            kind: InputKind::Question,
        });

        pane.apply_hook(
            HookLifecycleEvent::Awaiting {
                timestamp: 2,
                attention: HookAttention::Elicitation,
            },
            false,
        );

        assert!(!pane.requires_user_action());
        pane.shutdown().unwrap();
    }

    #[test]
    fn hook_attention_and_structured_question_events_do_not_duplicate_notifications() {
        let (tx, _) = mpsc::channel();
        let (program, args) = shell_command(if cfg!(windows) {
            "ping -n 30 127.0.0.1 > nul"
        } else {
            "sleep 30"
        });
        let mut pane = Pane::spawn(test_spec(43, program, args), 24, 80, tx).unwrap();
        pane.apply_hook(HookLifecycleEvent::Working { timestamp: 1 }, false);

        let hook = pane.apply_hook(
            HookLifecycleEvent::Awaiting {
                timestamp: 2,
                attention: HookAttention::Elicitation,
            },
            false,
        );
        assert!(hook.notification.is_none());
        assert!(pane.requires_user_action());
        assert_eq!(
            pane.apply_lifecycle(LifecycleEvent::InputRequested {
                tool_call_id: "question-1".into(),
                kind: InputKind::Question,
            }),
            Some(PaneNotification::Question)
        );
        pane.apply_lifecycle(LifecycleEvent::InputResolved {
            tool_call_id: "question-1".into(),
            kind: InputKind::Question,
        });
        assert!(!pane.requires_user_action());

        let permission_timestamp = current_timestamp().saturating_add(1);
        let permission = pane.apply_hook(
            HookLifecycleEvent::Awaiting {
                timestamp: permission_timestamp,
                attention: HookAttention::Permission,
            },
            false,
        );
        assert_eq!(permission.notification, Some(PaneNotification::Question));
        assert!(pane.clear_hook_permission_wait());
        assert!(!pane.requires_user_action());
        assert_eq!(
            pane.effective_progress_state(),
            crate::host_terminal::ProgressState::Indeterminate
        );
        let ready = pane.apply_hook(
            HookLifecycleEvent::Ready {
                timestamp: permission_timestamp.saturating_add(1),
            },
            false,
        );
        assert_eq!(
            ready.ready_confirmation,
            Some(permission_timestamp.saturating_add(1))
        );
        pane.shutdown().unwrap();
    }

    #[test]
    fn working_and_clear_in_one_output_chunk_still_requests_attention() {
        let (tx, _) = mpsc::channel();
        let (program, args) = shell_command(if cfg!(windows) {
            "ping -n 30 127.0.0.1 > nul"
        } else {
            "sleep 30"
        });
        let mut pane = Pane::spawn(test_spec(33, program, args), 24, 80, tx).unwrap();

        pane.feed_synthetic(b"\x1b]9;4;3;0\x1b\\answer\x1b]9;4;0;0\x1b\\");
        assert_eq!(
            pane.refresh_from_callbacks(false).1,
            vec![PaneNotification::Ready]
        );

        assert!(pane.needs_attention());
        pane.shutdown().unwrap();
    }

    #[test]
    fn background_bell_requests_attention() {
        let (tx, _) = mpsc::channel();
        let (program, args) = shell_command(if cfg!(windows) {
            "ping -n 30 127.0.0.1 > nul"
        } else {
            "sleep 30"
        });
        let mut pane = Pane::spawn(test_spec(34, program, args), 24, 80, tx).unwrap();

        pane.feed_synthetic(b"\x07");
        let (outcome, notifications) = pane.refresh_from_callbacks(false);
        assert!(outcome.bell);
        assert_eq!(notifications, vec![PaneNotification::Ready]);
        assert!(pane.needs_attention());

        pane.acknowledge_attention();
        pane.feed_synthetic(b"\x07");
        let (_, notifications) = pane.refresh_from_callbacks(true);
        assert!(
            notifications.is_empty(),
            "bell is deduplicated until new work starts"
        );
        assert!(!pane.needs_attention());
        pane.shutdown().unwrap();
    }

    #[test]
    fn error_notifies_once_and_suppresses_ready_until_the_next_cycle() {
        let (tx, _) = mpsc::channel();
        let (program, args) = shell_command(if cfg!(windows) {
            "ping -n 30 127.0.0.1 > nul"
        } else {
            "sleep 30"
        });
        let mut pane = Pane::spawn(test_spec(35, program, args), 24, 80, tx).unwrap();

        // Unattended throughout: this is about one cycle not reporting itself twice,
        // and a watched pane now sends nothing at all, which would hide the bug.
        pane.feed_synthetic(b"\x1b]9;4;2;0\x1b\\");
        let (_, error) = pane.refresh_from_callbacks(false);
        assert_eq!(error, vec![PaneNotification::Error]);
        pane.feed_synthetic(b"\x1b]9;4;2;0\x1b\\\x1b]9;4;0;0\x1b\\");
        let (_, duplicate_and_clear) = pane.refresh_from_callbacks(false);
        assert!(duplicate_and_clear.is_empty());

        pane.feed_synthetic(b"\x1b]9;4;3;0\x1b\\\x1b]9;4;0;0\x1b\\");
        let (_, next_cycle) = pane.refresh_from_callbacks(false);
        assert_eq!(next_cycle, vec![PaneNotification::Ready]);
        pane.shutdown().unwrap();
    }

    #[test]
    fn ordered_chunk_preserves_multiple_cycles_and_bell_before_error() {
        let (tx, _) = mpsc::channel();
        let (program, args) = shell_command(if cfg!(windows) {
            "ping -n 30 127.0.0.1 > nul"
        } else {
            "sleep 30"
        });
        let mut pane = Pane::spawn(test_spec(36, program, args), 24, 80, tx).unwrap();

        pane.feed_synthetic(
            b"\x1b]9;4;3;0\x1b\\\x07\x1b]9;4;2;0\x1b\\\x1b]9;4;0;0\x1b\\\
              \x1b]9;4;3;0\x1b\\\x1b]9;4;0;0\x1b\\",
        );
        let (_, notifications) = pane.refresh_from_callbacks(false);

        assert_eq!(
            notifications,
            vec![
                PaneNotification::Ready,
                PaneNotification::Error,
                PaneNotification::Ready,
            ]
        );
        pane.shutdown().unwrap();
    }

    #[test]
    fn keys_sent_to_an_exited_pane_are_ignored() {
        let (tx, rx) = mpsc::channel();
        let (program, args) = shell_command("echo done");
        let mut pane = Pane::spawn(test_spec(2, program, args), 24, 80, tx).unwrap();

        wait_for_exit(&rx, &mut pane);

        // Must not error even though the child is gone.
        pane.send_key(&KeyEvent::new(KeyCode::Char('x'), KeyModifiers::NONE))
            .unwrap();
        pane.send_paste("text").unwrap();
        assert!(pane.send_prompt_snippet("text").is_err());
    }

    #[test]
    fn multiline_snippet_is_rejected_until_bracketed_paste_is_enabled() {
        let (tx, _) = mpsc::channel();
        let (program, args) = shell_command(if cfg!(windows) {
            "ping -n 3 127.0.0.1 > nul"
        } else {
            "sleep 2"
        });
        let mut pane = Pane::spawn(test_spec(3, program, args), 24, 80, tx).unwrap();

        assert!(pane
            .send_prompt_snippet("")
            .unwrap_err()
            .to_string()
            .contains("empty"));
        assert!(pane
            .send_prompt_snippet("safe\x1b[201~unsafe")
            .unwrap_err()
            .to_string()
            .contains("control characters"));
        pane.send_prompt_snippet("single line").unwrap();
        let error = pane
            .send_prompt_snippet("line one\nline two")
            .unwrap_err()
            .to_string();
        assert!(error.contains("not ready for multiline paste"));

        let _ = pane.kill();
    }

    #[test]
    fn resize_updates_the_parser_dimensions() {
        let (tx, rx) = mpsc::channel();
        // Keep the child alive briefly so the resize targets a live PTY.
        let (program, args) = shell_command(if cfg!(windows) {
            "ping -n 3 127.0.0.1 > nul"
        } else {
            "sleep 2"
        });
        let mut pane = Pane::spawn(test_spec(3, program, args), 24, 80, tx).unwrap();

        pane.resize(40, 120).unwrap();
        let size = pane.with_screen(|screen| screen.size()).unwrap();
        assert_eq!(size, (40, 120));

        pane.kill().unwrap();
        wait_for_exit(&rx, &mut pane);
    }

    #[test]
    fn shutdown_waits_until_the_child_is_really_gone() {
        let (tx, _) = mpsc::channel();
        let (program, args) = shell_command(if cfg!(windows) {
            "ping -n 30 127.0.0.1 > nul"
        } else {
            "sleep 30"
        });
        let mut pane = Pane::spawn(test_spec(30, program, args), 24, 80, tx).unwrap();
        assert!(pane.is_running());

        assert!(pane.shutdown().unwrap());

        assert!(!pane.is_running());
        assert!(matches!(pane.status, PaneStatus::Exited(_)));
    }

    #[test]
    fn shutdown_report_excludes_a_child_that_finished_naturally() {
        let (tx, rx) = mpsc::channel();
        let (program, args) = shell_command(if cfg!(windows) { "exit /b 0" } else { "exit 0" });
        let mut pane = Pane::spawn(test_spec(40, program, args), 24, 80, tx).unwrap();
        let deadline = Instant::now() + Duration::from_secs(3);
        while Instant::now() < deadline {
            if matches!(
                rx.recv_timeout(Duration::from_millis(100)),
                Ok(MuxEvent::Exited(..))
            ) {
                break;
            }
        }

        assert!(!pane.shutdown().unwrap());
        assert!(!pane.is_running());
    }

    #[test]
    fn dropping_a_live_pane_terminates_and_reaps_its_child() {
        let (tx, rx) = mpsc::channel();
        let (program, args) = shell_command(if cfg!(windows) {
            "ping -n 30 127.0.0.1 > nul"
        } else {
            "sleep 30"
        });
        let pane = Pane::spawn(test_spec(41, program, args), 24, 80, tx).unwrap();

        drop(pane);

        let deadline = Instant::now() + Duration::from_secs(3);
        let mut exited = false;
        while Instant::now() < deadline {
            if matches!(
                rx.recv_timeout(Duration::from_millis(100)),
                Ok(MuxEvent::Exited(..))
            ) {
                exited = true;
                break;
            }
        }
        assert!(exited);
    }

    /// The full loop that makes the multiplexer usable: keystrokes are encoded, written to
    /// the PTY, echoed by the child, parsed, and land on the screen we render.
    #[test]
    fn typed_keys_round_trip_through_the_child() {
        let (tx, rx) = mpsc::channel();
        let (program, args) = if cfg!(windows) {
            (
                "cmd.exe".to_string(),
                vec!["/q".to_string(), "/k".to_string(), "@echo off".to_string()],
            )
        } else {
            ("/bin/sh".to_string(), vec!["-i".to_string()])
        };
        let mut pane = Pane::spawn(test_spec(4, program, args), 24, 80, tx).unwrap();

        // Give the shell time to draw its prompt before typing into it.
        std::thread::sleep(Duration::from_millis(1500));
        for ch in "echo round-trip".chars() {
            pane.send_key(&KeyEvent::new(KeyCode::Char(ch), KeyModifiers::NONE))
                .unwrap();
        }
        pane.send_key(&KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE))
            .unwrap();

        let deadline = Instant::now() + Duration::from_secs(20);
        let mut contents = String::new();
        while Instant::now() < deadline {
            let _ = rx.recv_timeout(Duration::from_millis(250));
            contents = pane.with_screen(|screen| screen.contents()).unwrap();
            if contents.matches("round-trip").count() >= 2 {
                break;
            }
        }

        // Twice: once as the echoed command line, once as the command's output.
        assert!(
            contents.matches("round-trip").count() >= 2,
            "child never echoed the typed command: {contents:?}"
        );

        pane.kill().unwrap();
        wait_for_exit(&rx, &mut pane);
    }
}

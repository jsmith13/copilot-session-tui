use crate::config::{self, EffectiveWorktreeConfig, ProjectSettings, UserConfig};
use crate::github::{GithubError, GithubItem};
use crate::mux::{KeyChord, MuxState, Pane, PaneSpec, PrefixState};
use crate::notifications::{NotificationKind, NotificationRequest, NotificationWorker};
use crate::scratchpad::Scratchpad;
use crate::session::manager;
use crate::session::tmux::{self, TmuxSessionRef};
use crate::session::worktree::ManagedWorktree;
use crate::session::Session;
use crate::snippets::{SnippetModal, SnippetUpdate};
use crate::terminal_pane::TerminalManager;
use crate::theme::{Theme, ThemeName};
use crate::updater::{UpdateCheckResult, UpdateInfo, UpdateInstallOutcome, UpdateInstallResult};
use crate::workspace_state;
use anyhow::{Context, Result};
use fuzzy_matcher::skim::SkimMatcherV2;
use fuzzy_matcher::FuzzyMatcher;
use ratatui::layout::Rect;
use ratatui::text::Line;
use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc;
use std::sync::Arc;
use std::time::{Duration, Instant};

pub use crate::config::GithubCommentFilter;

fn path_has_prefix(path: &str, prefix: &str) -> bool {
    #[cfg(windows)]
    {
        let path = path.replace('/', "\\").to_lowercase();
        let prefix = prefix.replace('/', "\\").to_lowercase();
        path == prefix
            || if prefix.ends_with('\\') {
                path.starts_with(&prefix)
            } else {
                path.strip_prefix(&prefix)
                    .is_some_and(|remainder| remainder.starts_with('\\'))
            }
    }
    #[cfg(not(windows))]
    {
        Path::new(path).starts_with(Path::new(prefix))
    }
}

/// Which surface the user is looking at: the session list, or a live session pane.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum View {
    List,
    Attached(crate::mux::PaneId),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WorkspaceFocus {
    Chat,
    Scratchpad,
    Terminal,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WorkspaceHelp {
    Scratchpad,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct WorkspaceAreas {
    pub chat: Rect,
    pub tabs: Rect,
    pub scratchpad: Option<Rect>,
    pub terminal: Option<Rect>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Mode {
    Normal,
    Search,
    Rename,
    ConfirmDelete,
    ConfirmForceDelete,
    ConfirmTakeover,
    /// Asks how the inactive favorites should be opened.
    FavoriteOpen,
    FilterProject,
    Help,
    Settings,
    ProjectSettings,
    BranchName,
    /// Pane switcher opened with `prefix w`.
    PaneList,
    Scratchpad,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SortField {
    LastUsed,
    Created,
    Name,
    Project,
}

#[derive(Debug, Clone)]
pub enum NewSessionRequest {
    Normal {
        cwd: String,
    },
    Tmux {
        cwd: String,
        title: String,
    },
    TmuxResume {
        session_id: String,
        cwd: String,
        title: String,
    },
    Worktree {
        source_project: String,
        branch: String,
        config: EffectiveWorktreeConfig,
    },
    TmuxWorktree {
        source_project: String,
        branch: String,
        config: EffectiveWorktreeConfig,
    },
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum WorktreeLaunchTarget {
    #[default]
    Standard,
    Tmux,
}

/// A worktree creation deferred to the main loop so a progress notice can be drawn first.
#[derive(Debug, Clone)]
pub struct PendingWorktree {
    pub project: String,
    pub branch: String,
    pub config: EffectiveWorktreeConfig,
    pub target: WorktreeLaunchTarget,
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct UpdateRestartRequest {
    pub panes: Vec<UpdateRestartPane>,
    pub focused_session_id: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UpdateRestartPane {
    pub pane_id: Option<crate::mux::PaneId>,
    pub copilot_running: bool,
    pub terminal_generation: Option<u64>,
    pub session_id: String,
    pub cwd: PathBuf,
    pub title: String,
}

#[derive(Debug, Clone)]
pub enum DeleteTarget {
    SessionOnly,
    Managed { entry: ManagedWorktree, dirty: bool },
}

#[derive(Debug, Clone)]
pub struct TakeoverTarget {
    pub id: String,
    pub cwd: String,
    pub title: String,
    pub dir_path: PathBuf,
    pub pids: Vec<u32>,
    pub resume_in_tmux: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SettingsEditField {
    Model,
    MaxAutopilotContinues,
    ThreadTrustedAuthors,
    ThreadWakeupsPerHour,
    HiddenTitlePrefixes,
    HiddenPathPrefixes,
    BranchPrefix,
    WorktreeRoot,
    MuxPrefix,
    TerminalShell,
    NtfyServer,
    NtfyTopic,
    NtfyAccessToken,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SettingsSection {
    General,
    Filters,
    Worktrees,
    Terminal,
    Notifications,
    Threads,
}

impl SettingsSection {
    pub const ALL: [Self; 6] = [
        Self::General,
        Self::Worktrees,
        Self::Terminal,
        Self::Notifications,
        Self::Threads,
        Self::Filters,
    ];

    pub const fn label(self) -> &'static str {
        match self {
            Self::General => "General",
            Self::Filters => "Filters",
            Self::Worktrees => "Worktrees",
            Self::Terminal => "Terminal",
            Self::Notifications => "Notifications",
            Self::Threads => "Threads",
        }
    }

    pub const fn rows(self) -> &'static [usize] {
        match self {
            Self::General => &[0, 1, 2, 18, 3],
            Self::Filters => &[16, 17, 19],
            Self::Worktrees => &[4, 5],
            Self::Terminal => &[6, 7, 8],
            Self::Notifications => &[9, 10, 11, 12, 13, 14, 15],
            // Numbered after everything else rather than renumbering: the indices are
            // keys, not positions.
            Self::Threads => &[20, 21, 22, 23],
        }
    }

    pub fn next(self, forward: bool) -> Self {
        let index = Self::ALL
            .iter()
            .position(|section| *section == self)
            .unwrap_or(0);
        let next = if forward {
            (index + 1) % Self::ALL.len()
        } else {
            (index + Self::ALL.len() - 1) % Self::ALL.len()
        };
        Self::ALL[next]
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ThemePicker {
    pub original: ThemeName,
    pub selected: usize,
}

impl ThemePicker {
    pub fn selected_theme(self) -> ThemeName {
        ThemeName::ALL[self.selected.min(ThemeName::ALL.len() - 1)]
    }

    pub fn move_by(&mut self, amount: isize) {
        let selected = (self.selected as isize).saturating_add(amount);
        self.selected = selected.clamp(0, ThemeName::ALL.len() as isize - 1) as usize;
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum GithubTab {
    Overview,
    Comments,
    Files,
}

impl GithubTab {
    pub fn index(self) -> usize {
        match self {
            Self::Overview => 0,
            Self::Comments => 1,
            Self::Files => 2,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FilesPane {
    Tree,
    Diff,
}

/// One of the inspector's scrollbars, named so a drag stays attached to the bar it
/// grabbed instead of re-deciding on every pointer move — which would hand the
/// gesture to a neighbour the moment the pointer strayed off the track.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum GithubScrollbar {
    /// The Overview and Comments text.
    Body,
    /// The changed-file tree on the Files tab.
    Tree,
    /// The diff beside it.
    Diff,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum GithubInspectorScreen {
    NumberPrompt,
    Loading,
    Choose {
        issue_or_pull_request: Box<GithubItem>,
        discussion: Box<GithubItem>,
    },
    Ready(GithubItem),
    Error(String),
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub enum CopilotReviewStatus {
    #[default]
    Idle,
    Requesting,
    Requested,
    Failed(String),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GithubInspector {
    pub screen: GithubInspectorScreen,
    pub copilot_review_status: CopilotReviewStatus,
    pub comment_filter: GithubCommentFilter,
    pub input: String,
    pub prompt_error: Option<String>,
    pub tab: GithubTab,
    pub scroll_offsets: [usize; 3],
    pub max_scroll: usize,
    pub selected_file: usize,
    /// Which half of the Files tab receives keys.
    pub files_pane: FilesPane,
    /// Selected row of the flattened changed-file tree.
    pub tree_selected: usize,
    pub tree_offset: usize,
    pub visible_tree_rows: usize,
    /// Directories the user collapsed; everything else stays expanded.
    pub collapsed_dirs: std::collections::BTreeSet<String>,
    /// Pane rectangles from the last draw, so the mouse can target them.
    pub tree_area: Rect,
    pub diff_area: Rect,
    /// Area the Overview and Comments text scrolls in, so its scrollbar can be found.
    pub body_area: Rect,
    /// Scrollbar the pointer grabbed, held until the button is released so the
    /// gesture survives the pointer wandering off the track.
    pub scrollbar_drag: Option<GithubScrollbar>,
    pub diff_scroll: usize,
    pub diff_horizontal: usize,
    pub max_diff_scroll: usize,
    pub max_diff_horizontal: usize,
    pub diff_render_cache: Option<DiffRenderCache>,
    pub request_id: u64,
    pub request_cwd: Option<PathBuf>,
    pub number: Option<u64>,
    pub lookup_kind: crate::github::GithubLookupKind,
}

impl GithubInspector {
    #[cfg(any(test, feature = "screenshots"))]
    pub fn number_prompt() -> Self {
        Self::number_prompt_with_filter(GithubCommentFilter::All)
    }

    pub fn number_prompt_with_filter(comment_filter: GithubCommentFilter) -> Self {
        Self {
            screen: GithubInspectorScreen::NumberPrompt,
            copilot_review_status: CopilotReviewStatus::Idle,
            comment_filter,
            input: String::new(),
            prompt_error: None,
            tab: GithubTab::Overview,
            scroll_offsets: [0; 3],
            max_scroll: 0,
            selected_file: 0,
            files_pane: FilesPane::Tree,
            tree_selected: 0,
            tree_offset: 0,
            visible_tree_rows: 0,
            collapsed_dirs: std::collections::BTreeSet::new(),
            tree_area: Rect::default(),
            diff_area: Rect::default(),
            body_area: Rect::default(),
            scrollbar_drag: None,
            diff_scroll: 0,
            diff_horizontal: 0,
            max_diff_scroll: 0,
            max_diff_horizontal: 0,
            diff_render_cache: None,
            request_id: 0,
            request_cwd: None,
            number: None,
            lookup_kind: crate::github::GithubLookupKind::Auto,
        }
    }

    pub fn ready_item(&self) -> Option<&GithubItem> {
        match &self.screen {
            GithubInspectorScreen::Ready(item) => Some(item),
            _ => None,
        }
    }

    pub fn can_request_copilot_review(&self) -> bool {
        matches!(
            self.ready_item(),
            Some(GithubItem::PullRequest(pull))
                if !pull.merged && pull.common.state.eq_ignore_ascii_case("open")
        )
    }

    pub fn can_filter_comments(&self) -> bool {
        self.tab == GithubTab::Comments
            && matches!(self.ready_item(), Some(GithubItem::PullRequest(_)))
    }

    pub fn cycle_comment_filter(&mut self) {
        if !self.can_filter_comments() {
            return;
        }
        self.comment_filter = self.comment_filter.next();
        self.scroll_offsets[GithubTab::Comments.index()] = 0;
        self.max_scroll = 0;
        self.scrollbar_drag = None;
    }

    pub fn choose_item(&mut self, kind: crate::github::GithubLookupKind) -> Option<GithubItem> {
        let GithubInspectorScreen::Choose {
            issue_or_pull_request,
            discussion,
        } = &self.screen
        else {
            return None;
        };
        let item = match kind {
            crate::github::GithubLookupKind::IssueOrPullRequest => {
                issue_or_pull_request.as_ref().clone()
            }
            crate::github::GithubLookupKind::Discussion => discussion.as_ref().clone(),
            crate::github::GithubLookupKind::Auto => return None,
        };
        self.lookup_kind = kind;
        self.screen = GithubInspectorScreen::Ready(item.clone());
        self.reset_navigation();
        Some(item)
    }

    pub fn tab_count(&self) -> usize {
        self.ready_item()
            .map(|item| if item.is_pull_request() { 3 } else { 2 })
            .unwrap_or(0)
    }

    pub fn cycle_tab(&mut self, forward: bool) {
        let count = self.tab_count();
        if count == 0 {
            return;
        }
        let current = self.tab.index().min(count - 1);
        let next = if forward {
            (current + 1) % count
        } else if current == 0 {
            count - 1
        } else {
            current - 1
        };
        self.tab = match next {
            0 => GithubTab::Overview,
            1 => GithubTab::Comments,
            _ => GithubTab::Files,
        };
        self.max_scroll = 0;
        self.files_pane = FilesPane::Tree;
    }

    pub fn active_scroll(&self) -> usize {
        self.scroll_offsets[self.tab.index()]
    }

    pub fn set_active_scroll(&mut self, offset: usize) {
        self.scroll_offsets[self.tab.index()] = offset.min(self.max_scroll);
    }

    pub fn scroll_active_by(&mut self, amount: isize) {
        let current = self.active_scroll();
        let next = if amount < 0 {
            current.saturating_sub(amount.unsigned_abs())
        } else {
            current.saturating_add(amount as usize)
        };
        self.set_active_scroll(next);
    }

    /// Put the tree cursor on the first file so the diff pane has something to
    /// show as soon as a pull request opens.
    pub fn select_first_tree_file(&mut self) {
        let Some(item) = self.ready_item() else {
            return;
        };
        let rows = crate::ui::file_tree::build_rows(item.files(), &self.collapsed_dirs);
        if let Some(row) = rows.iter().position(|row| row.file_index().is_some()) {
            self.tree_selected = row;
            self.selected_file = rows[row].file_index().unwrap_or(0);
        }
    }

    fn reset_navigation(&mut self) {
        self.tab = GithubTab::Overview;
        self.scroll_offsets = [0; 3];
        self.max_scroll = 0;
        self.selected_file = 0;
        self.files_pane = FilesPane::Tree;
        self.tree_selected = 0;
        self.tree_offset = 0;
        self.visible_tree_rows = 0;
        self.collapsed_dirs.clear();
        self.tree_area = Rect::default();
        self.diff_area = Rect::default();
        self.body_area = Rect::default();
        self.scrollbar_drag = None;
        self.diff_scroll = 0;
        self.diff_horizontal = 0;
        self.max_diff_scroll = 0;
        self.max_diff_horizontal = 0;
        self.diff_render_cache = None;
    }
}

pub struct GithubLoadResult {
    request_id: u64,
    result: std::result::Result<crate::github::FetchedItem, GithubError>,
    lookup_kind: crate::github::GithubLookupKind,
    /// A background refresh of something already on screen.
    ///
    /// These must never replace the view with a spinner or an error: the user
    /// is reading a perfectly good copy.
    revalidation: bool,
    reference_generation: Option<u64>,
}

pub struct GithubPatchResult {
    request_id: u64,
    result: std::result::Result<Vec<(String, Option<String>)>, GithubError>,
}

struct GithubReviewResult {
    request_id: u64,
    result: std::result::Result<(), GithubError>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DiffRenderCache {
    pub file_index: usize,
    pub line_count: usize,
    pub max_width: usize,
    pub lines: Vec<Line<'static>>,
}

/// An item kept around so reopening it is instant.
struct CachedGithubItem {
    repository: crate::github::RepositoryRef,
    number: u64,
    lookup_kind: crate::github::GithubLookupKind,
    item: GithubItem,
}

/// How many recently viewed items stay in memory.
///
/// Items are small next to the cost of refetching them, and the working set is
/// however many pull requests one conversation refers to.
const GITHUB_CACHE_LIMIT: usize = 12;

/// How often the focused pane is scanned for `#1234` references.
const REFERENCE_SCAN_INTERVAL: Duration = Duration::from_millis(750);
const REFERENCE_REVALIDATE_INTERVAL: Duration = Duration::from_secs(5 * 60);

/// Slowest the scan backs off to after repeated lookup failures.
const REFERENCE_SCAN_BACKOFF_LIMIT: Duration = Duration::from_secs(60);

/// What each `#1234` seen in a repository turned out to be.
type ReferenceLookup = std::collections::HashMap<
    (crate::github::RepositoryRef, u64),
    Option<crate::github::ReferenceStatus>,
>;

/// One batch of answers coming back from a lookup.
struct ResolvedReferences {
    repository: crate::github::RepositoryRef,
    statuses: Vec<(u64, Option<crate::github::ReferenceStatus>)>,
    periodic: bool,
    periodic_batch_len: usize,
    generations: HashMap<u64, u64>,
}

struct PendingExternalSession {
    next_attempt: Instant,
    deadline: Instant,
}

pub struct App {
    pub sessions: Vec<Session>,
    pub filtered_indices: Vec<usize>,
    pub selected: usize,
    pub scroll_offset: usize,
    /// Session id currently being dragged through the favorite order, if any.
    ///
    /// Held by id rather than row index so re-sorting after each move cannot lose
    /// track of what the user grabbed.
    pub grabbed_favorite: Option<String>,
    pub mode: Mode,
    pub search_query: String,
    pub show_hidden_sessions: bool,
    pub rename_input: String,
    pub project_filter: Option<String>,
    /// Project root of the directory `cst` was launched from (git-aware).
    /// `None` when the launch directory is not inside a Git repository.
    pub cwd_project: Option<String>,
    /// Raw directory `cst` was launched from.
    pub cwd: Option<String>,
    pub unique_projects: Vec<String>,
    pub project_selected: usize,
    pub project_scroll_offset: usize,
    pub project_visible_rows: usize,
    pub project_search_query: String,
    pub sort_field: SortField,
    pub detail_loaded_for: Option<String>,
    /// Selection waiting out a settle delay before its (large) event log is read.
    pub detail_pending: Option<(String, std::time::Instant)>,
    pub should_quit: bool,
    /// First visible line of the help popup, which is taller than it can draw.
    pub help_scroll: usize,
    pub should_resume: Option<(String, String)>, // (session_id, cwd)
    pub should_attach_tmux: Option<TmuxSessionRef>,
    pub should_new_session: Option<NewSessionRequest>,
    pub status_message: Option<String>,
    pub visible_rows: usize,
    pub update_info: Option<UpdateInfo>,
    pub update_receiver: Option<mpsc::Receiver<UpdateCheckResult>>,
    pub update_install_receiver: Option<mpsc::Receiver<UpdateInstallResult>>,
    session_load_receiver: Option<mpsc::Receiver<crate::session::loader::SessionLoadResult>>,
    pub update_check_requested: bool,
    pub installed_update_version: Option<String>,
    pub update_notice: Option<String>,
    pub confirm_update_restart: bool,
    /// Release notes to show once after an update, or on demand from the palette.
    pub whats_new: Option<crate::ui::whats_new::WhatsNewScreen>,

    /// Where thread subscriptions and held messages live.
    ///
    /// A field rather than a call to `state_root()` at each use, so a test can point it
    /// at a temporary directory. Without that, anything exercising delivery writes into
    /// the running user's real state — which is how this field came to exist.
    pub thread_state_root: PathBuf,

    /// The open thread inbox, if the user has asked to see what is waiting.
    ///
    /// Holds only a cursor: the items themselves live in `thread_pending`, so acting on
    /// one cannot leave the modal showing a stale copy of what it just changed.
    pub thread_inbox: Option<usize>,

    /// Notices waiting on the user, cached so drawing a frame is not a file read.
    ///
    /// Refreshed whenever the watcher records something and whenever the user acts on a
    /// notice. The file behind it is the truth; this is a copy for the renderer.
    pub thread_pending: Vec<crate::threads::Notice>,

    /// How many notices are still CST's to deal with rather than the user's.
    ///
    /// Only here so the idle tick can decide it has nothing to do without reading the
    /// state file five times a minute forever. Safe to cache because the one writer
    /// outside this thread — the watcher — says so on the mux channel.
    thread_live_notices: usize,

    /// The last reason the thread watcher gave for not working.
    ///
    /// Kept so it can be said once. The watcher already backs off; a status line
    /// repeating the same failure every minute would only train the user to ignore it.
    pub thread_watch_notice: Option<String>,
    pub restart_after_update: Option<UpdateRestartRequest>,
    update_restart_requested: bool,
    notification_worker: Option<NotificationWorker>,
    notification_pending: usize,
    notification_drain_started: Option<Instant>,
    notification_cycle_offsets: HashMap<String, u64>,
    config_reload_pending: bool,
    global_config_path: PathBuf,
    applied_config_revision: Option<config::ConfigRevision>,
    config_watch_revision: Arc<std::sync::Mutex<Option<config::ConfigRevision>>>,
    settings_config_revision: Option<config::ConfigRevision>,
    last_config_reload_attempt: Instant,
    #[cfg(test)]
    pub notification_requests: Vec<NotificationRequest>,
    /// Links a click asked to open, recorded instead of launching a browser or Explorer.
    #[cfg(test)]
    pub opened_links: Vec<crate::links::Target>,

    /// CST is running on a remote host the user reached over SSH.
    ///
    /// Anything CST launches then starts on that host, not on the machine the user is
    /// looking at, so it must not try to open files on their behalf.
    pub over_ssh: bool,
    #[cfg(test)]
    pub update_install_requested_for: Option<String>,
    pub config: UserConfig,
    pub copilot_home: PathBuf,
    pub settings_selected: usize,
    pub settings_section: SettingsSection,
    pub settings_editing: Option<SettingsEditField>,
    pub settings_input: String,
    pub theme_picker: Option<ThemePicker>,
    pub theme_picker_hits: Vec<(Rect, usize)>,
    theme_picker_last_click: Option<(usize, Instant)>,
    theme_save_reloaded_external: bool,
    pub project_settings: Option<ProjectSettings>,
    pub project_settings_selected: usize,
    pub project_settings_editing: bool,
    pub project_settings_input: String,
    pub branch_input: String,
    pub branch_config: Option<EffectiveWorktreeConfig>,
    pub worktree_launch_target: WorktreeLaunchTarget,
    pub pending_delete: Option<DeleteTarget>,
    pub pending_takeover: Option<TakeoverTarget>,
    /// Present only when multiplexing is enabled; owns every live pane.
    pub mux: Option<MuxState>,
    tmux_sessions: HashMap<String, TmuxSessionRef>,
    next_tmux_refresh: Instant,
    pub confirm_end_tmux: Option<TmuxSessionRef>,
    pub view: View,
    /// Rows/cols available to a pane, kept in sync with the terminal size.
    pub pane_size: (u16, u16),
    pub pane_origin: (u16, u16),
    /// Set when the user tries to quit while panes are still running.
    pub confirm_quit: bool,
    /// Highlighted row in the pane switcher.
    pub pane_selected: usize,
    /// The `mux` value stored on disk, so a `--mux` / `--no-mux` override for this
    /// invocation is never accidentally persisted by the settings popup.
    pub mux_on_disk: bool,
    /// Directory of the last focused pane, captured before panes are torn down so the
    /// shell wrapper can still auto-`cd` there.
    pub exit_dir: Option<String>,
    /// A worktree the main loop should create on its next iteration.
    pub pending_worktree: Option<PendingWorktree>,
    pub scratchpad: Option<Scratchpad>,
    pub scratchpad_owner: Option<crate::mux::PaneId>,
    pub scratchpad_open: HashSet<crate::mux::PaneId>,
    pub terminal: TerminalManager,
    pub terminal_owner: Option<crate::mux::PaneId>,
    pub terminal_open: HashSet<crate::mux::PaneId>,
    pub workspace_focus: WorkspaceFocus,
    pub terminal_focused: bool,
    pub workspace_help: Option<WorkspaceHelp>,
    pub snippet_modal: Option<SnippetModal>,
    pub command_palette: Option<crate::command_palette::CommandPalette>,
    pub workspace_areas: WorkspaceAreas,
    /// The pane a tab drag is carrying, from the press on its tab until release.
    pub dragging_tab: Option<crate::mux::PaneId>,
    pub host_sequences: Vec<Vec<u8>>,
    pub github_inspector: Option<GithubInspector>,
    github_request_receiver: Option<mpsc::Receiver<GithubLoadResult>>,
    github_request_cancel: Option<Arc<AtomicBool>>,
    github_patch_receiver: Option<mpsc::Receiver<GithubPatchResult>>,
    github_patch_cancel: Option<Arc<AtomicBool>>,
    github_review_receiver: Option<mpsc::Receiver<GithubReviewResult>>,
    github_review_cancel: Option<Arc<AtomicBool>>,
    github_repo_receiver: Option<mpsc::Receiver<(PathBuf, Option<crate::github::RepositoryRef>)>>,
    github_reference_receiver: Option<mpsc::Receiver<ResolvedReferences>>,
    /// What each seen `#1234` points at; `None` means "not a reference".
    ///
    /// Negative answers are kept deliberately: a terminal is full of numbers
    /// that merely look like references, and re-asking about them every second
    /// would be worse than useless.
    github_references: ReferenceLookup,
    github_reference_generations: HashMap<(crate::github::RepositoryRef, u64), u64>,
    next_github_reference_generation: u64,
    github_reference_repo: Option<crate::github::RepositoryRef>,
    github_reference_scan: Option<Instant>,
    /// Grows when lookups fail, so a broken network is not hammered.
    github_reference_interval: Duration,
    github_reference_refreshed_at: HashMap<(crate::github::RepositoryRef, u64), Instant>,
    github_reference_periodic_remaining: HashMap<crate::github::RepositoryRef, Vec<u64>>,
    /// Repositories already resolved, keyed by working directory.
    github_repositories: std::collections::HashMap<PathBuf, Option<crate::github::RepositoryRef>>,
    /// Most recently viewed items, newest last.
    github_cache: Vec<CachedGithubItem>,
    next_github_request_id: u64,
    pending_external_sessions: HashMap<String, PendingExternalSession>,
    workspace_state_enabled: bool,
    /// Disabled in tests so reordering favorites never rewrites the real user config.
    config_persistence_enabled: bool,
    workspace_state_root: PathBuf,
}

impl App {
    pub fn new(sessions: Vec<Session>, config: UserConfig) -> Self {
        let unique_projects = extract_unique_projects(&sessions);
        let filtered_indices: Vec<usize> = (0..sessions.len()).collect();
        let mux_on_disk = config.mux;

        // An unparseable prefix falls back to the default rather than refusing to start.
        let mux = config.mux.then(|| {
            let prefix = KeyChord::parse(&config.mux_prefix)
                .or_else(|| KeyChord::parse(crate::config::DEFAULT_MUX_PREFIX))
                .expect("the default mux prefix must parse");
            MuxState::new(prefix)
        });

        let mut app = App {
            sessions,
            filtered_indices,
            selected: 0,
            scroll_offset: 0,
            grabbed_favorite: None,
            mode: Mode::Normal,
            search_query: String::new(),
            show_hidden_sessions: false,
            rename_input: String::new(),
            project_filter: None,
            cwd_project: None,
            cwd: None,
            unique_projects,
            project_selected: 0,
            project_scroll_offset: 0,
            project_visible_rows: 10,
            project_search_query: String::new(),
            sort_field: SortField::LastUsed,
            detail_loaded_for: None,
            detail_pending: None,
            should_quit: false,
            help_scroll: 0,
            should_resume: None,
            should_attach_tmux: None,
            should_new_session: None,
            status_message: None,
            visible_rows: 20,
            update_info: None,
            update_receiver: None,
            update_install_receiver: None,
            session_load_receiver: None,
            update_check_requested: false,
            installed_update_version: None,
            update_notice: None,
            confirm_update_restart: false,
            whats_new: None,
            thread_state_root: crate::threads::store::state_root(),
            thread_inbox: None,
            thread_pending: Vec::new(),
            thread_live_notices: 0,
            thread_watch_notice: None,
            restart_after_update: None,
            update_restart_requested: false,
            notification_worker: None,
            notification_pending: 0,
            notification_drain_started: None,
            notification_cycle_offsets: HashMap::new(),
            config_reload_pending: false,
            global_config_path: config::config_path(),
            applied_config_revision: None,
            config_watch_revision: Arc::new(std::sync::Mutex::new(None)),
            settings_config_revision: None,
            last_config_reload_attempt: Instant::now() - Duration::from_secs(1),
            #[cfg(test)]
            notification_requests: Vec::new(),
            #[cfg(test)]
            opened_links: Vec::new(),
            over_ssh: crate::links::running_over_ssh(|name| std::env::var_os(name).is_some()),
            #[cfg(test)]
            update_install_requested_for: None,
            config,
            copilot_home: crate::session::loader::copilot_home(),
            settings_selected: 0,
            settings_section: SettingsSection::General,
            settings_editing: None,
            settings_input: String::new(),
            theme_picker: None,
            theme_picker_hits: Vec::new(),
            theme_picker_last_click: None,
            theme_save_reloaded_external: false,
            project_settings: None,
            project_settings_selected: 0,
            project_settings_editing: false,
            project_settings_input: String::new(),
            branch_input: String::new(),
            branch_config: None,
            worktree_launch_target: WorktreeLaunchTarget::Standard,
            pending_delete: None,
            pending_takeover: None,
            mux,
            tmux_sessions: HashMap::new(),
            next_tmux_refresh: Instant::now(),
            confirm_end_tmux: None,
            view: View::List,
            pane_size: (24, 80),
            pane_origin: (0, 0),
            confirm_quit: false,
            pane_selected: 0,
            mux_on_disk,
            exit_dir: None,
            pending_worktree: None,
            scratchpad: None,
            scratchpad_owner: None,
            scratchpad_open: HashSet::new(),
            terminal: TerminalManager::default(),
            terminal_owner: None,
            terminal_open: HashSet::new(),
            workspace_focus: WorkspaceFocus::Chat,
            // DECSET 1004 reports future focus transitions, not initial state. Treat a
            // newly launched tab as unattended until FocusGained or real input proves
            // otherwise; favorite tabs often finish startup after becoming background.
            terminal_focused: false,
            workspace_help: None,
            snippet_modal: None,
            command_palette: None,
            workspace_areas: WorkspaceAreas::default(),
            dragging_tab: None,
            host_sequences: Vec::new(),
            github_inspector: None,
            github_request_receiver: None,
            github_request_cancel: None,
            github_patch_receiver: None,
            github_patch_cancel: None,
            github_review_receiver: None,
            github_review_cancel: None,
            github_repositories: std::collections::HashMap::new(),
            github_repo_receiver: None,
            github_reference_receiver: None,
            github_references: ReferenceLookup::new(),
            github_reference_generations: HashMap::new(),
            next_github_reference_generation: 1,
            github_reference_repo: None,
            github_reference_scan: None,
            github_reference_interval: REFERENCE_SCAN_INTERVAL,
            github_reference_refreshed_at: HashMap::new(),
            github_reference_periodic_remaining: HashMap::new(),
            github_cache: Vec::new(),
            next_github_request_id: 1,
            pending_external_sessions: HashMap::new(),
            workspace_state_enabled: true,
            config_persistence_enabled: true,
            workspace_state_root: workspace_state::workspace_root(),
        };
        app.apply_filter();
        app
    }

    /// Config as it should be written to disk, with any per-invocation override undone.
    pub fn persistable_config(&self) -> UserConfig {
        let mut config = self.config.clone();
        config.mux = self.mux_on_disk;
        config
    }

    /// Open the pane switcher, pre-selecting the currently focused pane.
    pub fn open_pane_list(&mut self) {
        let Some(mux) = self.mux.as_ref() else {
            return;
        };
        if mux.panes.is_empty() {
            self.status_message = Some("No running sessions".to_string());
            return;
        }
        // New Copilot sessions do not exist on disk until the child has started. Refresh
        // only the live pane ids here: walking the user's entire Copilot history would
        // make opening this tiny switcher noticeably slow.
        self.refresh_live_pane_sessions();
        let mux = self.mux.as_ref().expect("multiplexer checked above");
        self.pane_selected = mux
            .focused
            .and_then(|id| mux.panes.iter().position(|pane| pane.id == id))
            .unwrap_or(0);
        self.workspace_focus = WorkspaceFocus::Chat;
        self.terminal.unfocus();
        self.view = View::List;
        self.mode = Mode::PaneList;
    }

    pub fn mux_enabled(&self) -> bool {
        self.mux.is_some()
    }

    pub fn pane_is_tmux_backed(&self, pane: &Pane) -> bool {
        self.tmux_sessions.contains_key(&pane.session_id)
    }

    pub fn tmux_session_for(&self, session_id: &str) -> Option<&TmuxSessionRef> {
        self.tmux_sessions.get(session_id)
    }

    pub fn session_is_tmux_backed(&self, session_id: &str) -> bool {
        self.tmux_sessions.contains_key(session_id)
    }

    pub fn forget_tmux_session(&mut self, session_id: &str) {
        self.tmux_sessions.remove(session_id);
        if self
            .confirm_end_tmux
            .as_ref()
            .is_some_and(|reference| reference.session_id == session_id)
        {
            self.confirm_end_tmux = None;
        }
    }

    pub fn replace_tmux_session(&mut self, reference: TmuxSessionRef) {
        self.tmux_sessions
            .insert(reference.session_id.clone(), reference);
    }

    pub fn refresh_tmux_sessions(&mut self) -> bool {
        let now = Instant::now();
        if now < self.next_tmux_refresh {
            return false;
        }
        self.next_tmux_refresh = now + Duration::from_secs(2);
        match tmux::list_live() {
            Ok(references) => {
                let next: HashMap<_, _> = references
                    .into_iter()
                    .map(|reference| (reference.session_id.clone(), reference))
                    .collect();
                if next == self.tmux_sessions {
                    false
                } else {
                    self.tmux_sessions = next;
                    true
                }
            }
            Err(error) => {
                self.status_message = Some(format!("Cannot refresh tmux sessions: {error}"));
                false
            }
        }
    }

    pub fn open_snippets(&mut self) {
        let (global, global_error) = if self.config_persistence_enabled {
            match config::load_checked_from(&self.global_config_path) {
                Ok(persisted) => {
                    let snippets = persisted.snippets.clone();
                    self.adopt_persisted_config(persisted);
                    (snippets, None)
                }
                Err(error) => (
                    self.config.snippets.clone(),
                    Some(format!("Cannot refresh global snippets: {error}")),
                ),
            }
        } else {
            (self.config.snippets.clone(), None)
        };
        let project_root = self
            .mux
            .as_ref()
            .and_then(|mux| mux.focused_pane())
            .and_then(|pane| {
                crate::session::loader::detect_project_root(&pane.cwd.to_string_lossy())
            })
            .map(PathBuf::from);
        let (project, project_error, project_root) = match project_root {
            Some(root) => match ProjectSettings::load(&root, &self.config) {
                Ok(settings) => (settings.snippets().to_vec(), None, Some(root)),
                Err(error) => (
                    Vec::new(),
                    Some(format!("Cannot load project snippets: {error}")),
                    None,
                ),
            },
            None => (Vec::new(), None, None),
        };
        let mut modal = SnippetModal::new(global, project, project_root);
        modal.error = global_error.or(project_error);
        self.snippet_modal = Some(modal);
    }

    pub fn persist_snippets(&mut self, update: &SnippetUpdate) -> Result<()> {
        if update.project_root.is_none() && !update.project.is_empty() {
            anyhow::bail!("Project-scoped snippets require a Git project");
        }
        let mut project_settings = if update.project_dirty {
            update
                .project_root
                .as_deref()
                .map(|root| ProjectSettings::load(root, &self.config))
                .transpose()?
        } else {
            None
        };
        let previous_project = project_settings
            .as_ref()
            .map(|settings| settings.snippets().to_vec())
            .unwrap_or_default();

        if !self.config_persistence_enabled {
            self.config.snippets = update.global.clone();
            return Ok(());
        }

        if update.project_dirty && previous_project != update.original_project {
            anyhow::bail!("Project snippets changed on disk; close and reopen the snippet manager");
        }
        if update.project_dirty {
            let settings = project_settings
                .as_mut()
                .context("Project settings disappeared while saving snippets")?;
            settings.set_snippets(update.project.clone());
            settings.save()?;
        }

        if update.global_dirty {
            match config::save_global_snippets_if_unchanged(&update.original_global, &update.global)
            {
                Ok(persisted) => self.adopt_persisted_config(persisted),
                Err(error) => {
                    if update.project_dirty {
                        let settings = project_settings
                            .as_mut()
                            .context("Project settings disappeared while rolling back snippets")?;
                        settings.set_snippets(previous_project);
                        if let Err(rollback) = settings.save() {
                            return Err(error).context(format!(
                                "Failed to save global snippets; project rollback also failed: {rollback}"
                            ));
                        }
                    }
                    return Err(error).context("Failed to save global snippets");
                }
            }
        } else {
            self.config.snippets = update.global.clone();
        }
        Ok(())
    }

    fn adopt_persisted_config(&mut self, mut persisted: UserConfig) {
        // Mux is fixed for the lifetime of this App, including a CLI-only override.
        // Remember the refreshed disk value for future saves but keep the live mode.
        let selected_id = self.selected_session().map(|session| session.id.clone());
        let favorites_changed = self.config.favorites != persisted.favorites;
        let hidden_title_prefixes_changed =
            self.config.hidden_title_prefixes != persisted.hidden_title_prefixes;
        let hidden_path_prefixes_changed =
            self.config.hidden_path_prefixes != persisted.hidden_path_prefixes;
        let theme_changed = self.config.theme != persisted.theme;
        let runtime_mux = self.config.mux;
        self.mux_on_disk = persisted.mux;
        persisted.mux = runtime_mux;
        match KeyChord::parse(&persisted.mux_prefix) {
            Some(prefix) => {
                if let Some(mux) = self.mux.as_mut() {
                    mux.prefix = prefix;
                }
            }
            None => {
                self.status_message = Some(format!(
                    "Cannot apply invalid mux prefix '{}' from global settings",
                    persisted.mux_prefix
                ));
                persisted.mux_prefix = self.config.mux_prefix.clone();
            }
        }
        self.config = persisted;
        if theme_changed {
            self.refresh_theme_dependents();
        }
        if let Some(settings) = self.project_settings.as_mut() {
            settings.refresh_global(&self.config);
        }
        if favorites_changed || hidden_title_prefixes_changed || hidden_path_prefixes_changed {
            self.apply_filter();
            if let Some(session_id) = selected_id {
                self.focus_session(&session_id);
            }
        }
    }

    pub fn request_config_reload(&mut self) -> bool {
        if !self.config_persistence_enabled {
            return false;
        }
        if self.mode == Mode::Settings {
            if config::config_revision(&self.global_config_path)
                .ok()
                .as_ref()
                == self.settings_config_revision.as_ref()
            {
                return false;
            }
            self.config_reload_pending = true;
            return false;
        }
        self.config_reload_pending = false;
        self.last_config_reload_attempt = Instant::now();
        match config::load_existing_base_config_with_revision(&self.global_config_path) {
            Ok(Some((mut persisted, revision))) => {
                // Global snippets have an authoritative locked sidecar. Reloading the
                // base config must not replace that live snapshot with a legacy field.
                persisted.snippets = self.config.snippets.clone();
                self.adopt_persisted_config(persisted);
                self.set_applied_config_revision(revision);
                true
            }
            Ok(None) => {
                if self.applied_config_revision.is_none() {
                    self.set_applied_config_revision(config::ConfigRevision::Missing);
                } else {
                    self.config_reload_pending = true;
                }
                false
            }
            Err(error) => {
                self.config_reload_pending = true;
                self.status_message = Some(format!("Cannot reload global settings: {error}"));
                true
            }
        }
    }

    pub fn poll_config_reload(&mut self) -> bool {
        if !self.config_reload_pending
            || self.mode == Mode::Settings
            || self.last_config_reload_attempt.elapsed() < Duration::from_secs(1)
        {
            return false;
        }
        self.request_config_reload()
    }

    fn set_applied_config_revision(&mut self, revision: config::ConfigRevision) {
        self.applied_config_revision = Some(revision);
        if let Ok(mut watched) = self.config_watch_revision.lock() {
            *watched = Some(revision);
        }
    }

    pub(crate) fn config_revision_handle(
        &self,
    ) -> Arc<std::sync::Mutex<Option<config::ConfigRevision>>> {
        Arc::clone(&self.config_watch_revision)
    }

    pub(crate) fn config_revision_is_applied(&self, revision: config::ConfigRevision) -> bool {
        self.applied_config_revision == Some(revision)
    }

    pub fn begin_global_settings(&mut self) {
        self.request_config_reload();
        self.settings_selected = 0;
        self.settings_section = SettingsSection::General;
        self.settings_editing = None;
        self.settings_input.clear();
        self.theme_picker = None;
        let current = config::config_revision(&self.global_config_path).ok();
        self.settings_config_revision = (current == self.applied_config_revision)
            .then_some(current)
            .flatten();
        self.config_reload_pending = false;
        self.mode = Mode::Settings;
    }

    pub fn theme_name(&self) -> ThemeName {
        self.theme_picker
            .map(ThemePicker::selected_theme)
            .unwrap_or(self.config.theme)
    }

    pub fn theme(&self) -> Theme {
        self.theme_name().theme()
    }

    pub fn open_theme_picker(&mut self) {
        let original = self.config.theme;
        let selected = ThemeName::ALL
            .iter()
            .position(|theme| *theme == original)
            .unwrap_or_default();
        self.theme_picker = Some(ThemePicker { original, selected });
        self.theme_picker_hits.clear();
        self.theme_picker_last_click = None;
        self.theme_save_reloaded_external = false;
        self.refresh_theme_dependents();
    }

    pub fn move_theme_picker(&mut self, amount: isize) {
        if let Some(picker) = self.theme_picker.as_mut() {
            picker.move_by(amount);
            self.refresh_theme_dependents();
        }
    }

    pub fn cancel_theme_picker(&mut self) {
        if self.theme_picker.take().is_some() {
            self.theme_picker_hits.clear();
            self.theme_picker_last_click = None;
            self.refresh_theme_dependents();
        }
    }

    pub fn confirm_theme_picker(&mut self) -> anyhow::Result<()> {
        let Some(picker) = self.theme_picker else {
            return Ok(());
        };
        let selected = picker.selected_theme();
        let previous = self.config.theme;
        self.config.theme = selected;
        self.theme_save_reloaded_external = false;
        match self.save_global_settings() {
            Ok(()) => {
                self.theme_picker = None;
                self.theme_picker_hits.clear();
                self.theme_picker_last_click = None;
                self.refresh_theme_dependents();
                Ok(())
            }
            Err(error) => {
                if self.theme_save_reloaded_external {
                    if let Some(picker) = self.theme_picker.as_mut() {
                        picker.original = self.config.theme;
                    }
                } else {
                    self.config.theme = previous;
                }
                self.refresh_theme_dependents();
                Err(error)
            }
        }
    }

    fn refresh_theme_dependents(&mut self) {
        if let Some(inspector) = self.github_inspector.as_mut() {
            inspector.diff_render_cache = None;
        }
        let terminal_light_mode = self.theme_name().terminal_light_mode();
        if let Some(mux) = self.mux.as_mut() {
            for pane in &mut mux.panes {
                pane.set_terminal_light_mode(terminal_light_mode);
            }
        }
    }

    pub fn set_theme_picker_hits(&mut self, hits: Vec<(Rect, usize)>) {
        self.theme_picker_hits = hits;
    }

    pub fn select_theme_picker_at(&mut self, column: u16, row: u16) -> bool {
        let selected = self
            .theme_picker_hits
            .iter()
            .find(|(area, _)| {
                column >= area.x && column < area.right() && row >= area.y && row < area.bottom()
            })
            .map(|(_, selected)| *selected);
        if let (Some(picker), Some(selected)) = (self.theme_picker.as_mut(), selected) {
            picker.selected = selected.min(ThemeName::ALL.len() - 1);
            self.refresh_theme_dependents();
            let now = Instant::now();
            let confirm = self
                .theme_picker_last_click
                .is_some_and(|(previous, clicked)| {
                    previous == selected && clicked.elapsed() <= Duration::from_millis(500)
                });
            self.theme_picker_last_click = Some((selected, now));
            return confirm;
        }
        false
    }

    /// Status message that also warns when the prefix key had to be defaulted.
    pub fn prefix_label(&self) -> Option<String> {
        self.mux.as_ref().map(|mux| mux.prefix.label())
    }

    /// True while the prefix has been pressed and a command key is awaited.
    pub fn prefix_pending(&self) -> bool {
        self.mux
            .as_ref()
            .is_some_and(|mux| mux.prefix_state != PrefixState::Idle)
    }

    pub fn help_pending(&self) -> bool {
        self.mux
            .as_ref()
            .is_some_and(|mux| mux.prefix_state == PrefixState::Help)
    }

    pub fn github_prefix_pending(&self) -> bool {
        self.mux
            .as_ref()
            .is_some_and(|mux| mux.prefix_state == PrefixState::Github)
    }

    /// Live panes owned by this instance, for the session list footer.
    pub fn running_pane_count(&self) -> usize {
        self.mux
            .as_ref()
            .map(|mux| mux.panes.iter().filter(|pane| pane.is_running()).count())
            .unwrap_or(0)
    }

    pub fn focused_pane_title(&self) -> Option<String> {
        self.mux
            .as_ref()
            .and_then(|mux| mux.focused_pane())
            .map(Pane::display_title)
    }

    pub fn any_pane_needs_attention(&self) -> bool {
        self.mux
            .as_ref()
            .is_some_and(|mux| mux.panes.iter().any(Pane::needs_attention))
    }

    pub fn acknowledge_focused_pane(&mut self) {
        if let Some(pane) = self.mux.as_mut().and_then(|mux| mux.focused_pane_mut()) {
            pane.acknowledge_attention();
        }
    }

    pub fn open_command_palette(&mut self) {
        self.release_favorite_grab();
        self.command_palette = Some(crate::command_palette::CommandPalette::default());
        if let Some(mux) = self.mux.as_mut() {
            mux.prefix_state = PrefixState::Idle;
        }
    }

    pub fn close_command_palette(&mut self) {
        self.command_palette = None;
    }

    pub fn open_github_inspector(&mut self) {
        if !matches!(self.view, View::Attached(_)) {
            self.status_message =
                Some("Attach to a session before inspecting a GitHub item".to_string());
            return;
        }
        self.cancel_github_request();
        self.cancel_copilot_review();
        self.github_inspector = Some(GithubInspector::number_prompt_with_filter(
            self.config.github_comment_filter,
        ));
        // Resolving the repository is a network round trip that does not depend
        // on the number, so it can happen while the user is still typing it.
        self.prefetch_github_repository();
    }

    fn prefetch_github_repository(&mut self) {
        let Some(cwd) = self
            .mux
            .as_ref()
            .and_then(|mux| mux.focused_pane())
            .map(|pane| pane.cwd.clone())
        else {
            return;
        };
        if self.github_repositories.contains_key(&cwd) || self.github_repo_receiver.is_some() {
            return;
        }
        let (sender, receiver) = mpsc::channel();
        let cancelled = Arc::new(AtomicBool::new(false));
        std::thread::spawn(move || {
            // A failure is recorded too: without it, a session outside any
            // GitHub repository would keep spawning `gh` forever.
            let resolved = crate::github::resolve_repository_for(&cwd, cancelled).ok();
            let _ = sender.send((cwd, resolved));
        });
        self.github_repo_receiver = Some(receiver);
    }

    fn poll_github_repository(&mut self) {
        let Some(receiver) = self.github_repo_receiver.as_ref() else {
            return;
        };
        match receiver.try_recv() {
            Ok((cwd, repository)) => {
                self.github_repo_receiver = None;
                self.github_repositories.insert(cwd, repository);
            }
            Err(mpsc::TryRecvError::Empty) => {}
            Err(mpsc::TryRecvError::Disconnected) => self.github_repo_receiver = None,
        }
    }

    /// Status of a `#1234` seen in the focused pane, once it is known.
    pub fn github_reference_status(&self, number: u64) -> Option<crate::github::ReferenceStatus> {
        let repository = self.github_reference_repo.as_ref()?;
        self.github_references
            .get(&(repository.clone(), number))
            .copied()
            .flatten()
    }

    /// Look up any references on screen that have not been looked up yet.
    ///
    /// One query covers every new number, so a busy screen still costs a single
    /// round trip.
    pub fn refresh_github_references(&mut self) {
        let Some(cwd) = self
            .mux
            .as_ref()
            .and_then(|mux| mux.focused_pane())
            .map(|pane| pane.cwd.clone())
        else {
            self.github_reference_repo = None;
            return;
        };
        let Some(repository) = self.github_repositories.get(&cwd).cloned().flatten() else {
            self.github_reference_repo = None;
            // Resolving the repository is what makes decoration possible at all,
            // and it is needed for the inspector anyway.
            self.prefetch_github_repository();
            return;
        };
        self.github_reference_repo = Some(repository.clone());
        if self.github_reference_receiver.is_some() {
            return;
        }
        // Scanning the whole screen every frame would be wasteful for something
        // that changes on human timescales.
        let now = Instant::now();
        if self
            .github_reference_scan
            .is_some_and(|last| now.duration_since(last) < self.github_reference_interval)
        {
            return;
        }
        self.github_reference_scan = Some(now);

        let Some(pane) = self.mux.as_ref().and_then(|mux| mux.focused_pane()) else {
            return;
        };
        let visible = pane.github_references();
        if visible.is_empty() {
            return;
        }
        let stale: Vec<u64> = visible
            .iter()
            .copied()
            .filter(|number| {
                self.github_reference_refreshed_at
                    .get(&(repository.clone(), *number))
                    .is_none_or(|last| now.duration_since(*last) >= REFERENCE_REVALIDATE_INTERVAL)
            })
            .collect();
        let numbers: Vec<u64> = self
            .github_reference_periodic_remaining
            .entry(repository.clone())
            .or_insert(stale)
            .iter()
            .copied()
            .take(crate::github::REFERENCE_BATCH)
            .collect();
        if numbers.is_empty() {
            self.github_reference_periodic_remaining.remove(&repository);
            return;
        }
        let periodic = true;
        let periodic_batch_len = numbers.len();
        let generations: HashMap<u64, u64> = numbers
            .iter()
            .map(|number| {
                (
                    *number,
                    self.issue_github_reference_generation(repository.clone(), *number),
                )
            })
            .collect();

        let (sender, receiver) = mpsc::channel();
        let cancelled = Arc::new(AtomicBool::new(false));
        let worker_repository = repository.clone();
        std::thread::spawn(move || {
            if let Ok(statuses) = crate::github::resolve_references(
                cwd,
                worker_repository.clone(),
                numbers,
                cancelled,
            ) {
                let _ = sender.send(ResolvedReferences {
                    repository: worker_repository,
                    statuses,
                    periodic,
                    periodic_batch_len,
                    generations,
                });
            }
        });
        self.github_reference_receiver = Some(receiver);
    }

    fn poll_github_references(&mut self) {
        let Some(receiver) = self.github_reference_receiver.as_ref() else {
            return;
        };
        match receiver.try_recv() {
            Ok(resolved) => {
                self.github_reference_receiver = None;
                self.github_reference_interval = REFERENCE_SCAN_INTERVAL;
                if resolved.periodic {
                    let completed = self
                        .github_reference_periodic_remaining
                        .get_mut(&resolved.repository)
                        .is_none_or(|remaining| {
                            let consumed = resolved.periodic_batch_len.min(remaining.len());
                            remaining.drain(..consumed);
                            remaining.is_empty()
                        });
                    if completed {
                        self.github_reference_periodic_remaining
                            .remove(&resolved.repository);
                    }
                }
                for (number, status) in resolved.statuses {
                    let key = (resolved.repository.clone(), number);
                    let captured = resolved.generations.get(&number).copied().unwrap_or(0);
                    let current = self
                        .github_reference_generations
                        .get(&key)
                        .copied()
                        .unwrap_or(0);
                    if current == captured {
                        self.github_references.insert(key, status);
                        self.github_reference_refreshed_at
                            .insert((resolved.repository.clone(), number), Instant::now());
                    }
                }
            }
            Err(mpsc::TryRecvError::Empty) => {}
            Err(mpsc::TryRecvError::Disconnected) => {
                self.github_reference_receiver = None;
                // The numbers stay unknown, so without a backoff a persistent
                // failure would mean a `gh` process every scan.
                self.github_reference_interval =
                    (self.github_reference_interval * 2).min(REFERENCE_SCAN_BACKOFF_LIMIT);
            }
        }
    }

    /// Open a link a pane showed, within what a click is allowed to do.
    ///
    /// See [`crate::links`] for why a file is revealed rather than launched and why every
    /// scheme but the web is refused.
    pub fn open_link(&mut self, raw: &str) {
        let target = match crate::links::classify(raw) {
            None => {
                self.status_message =
                    Some("Not opened: CST only follows web links and local files".to_string());
                return;
            }
            // Already open. Web links are drawn as real hyperlinks, so the terminal opened
            // this one itself — on the machine the user is sitting at, which over SSH is
            // the only right one. Windows Terminal also forwards the same click, so
            // opening it here as well would give every such click two browser tabs.
            Some(crate::links::Target::Web(_)) => return,
            Some(crate::links::Target::File(path)) if self.over_ssh => {
                self.status_message = Some(format!(
                    "Not opened: {} is on the machine CST is running on, not this one",
                    path.display()
                ));
                return;
            }
            Some(crate::links::Target::File(path)) => path,
        };
        #[cfg(test)]
        {
            self.opened_links.push(crate::links::Target::File(target));
        }
        #[cfg(not(test))]
        if let Err(error) = crate::links::reveal(&target) {
            self.status_message = Some(format!("Could not open link: {error}"));
        }
    }

    /// Web links in whatever pane is showing, to be drawn as real hyperlinks.
    pub fn visible_hyperlinks(&self) -> Vec<crate::ui::hyperlinks::HyperlinkRun> {
        match self.view {
            View::Attached(id) => self
                .mux
                .as_ref()
                .and_then(|mux| mux.pane(id))
                .map(crate::mux::Pane::web_hyperlinks)
                .unwrap_or_default(),
            _ => Vec::new(),
        }
    }

    pub fn inspect_github_item(&mut self, number: u64) {
        let Some(cwd) = self
            .mux
            .as_ref()
            .and_then(|mux| mux.focused_pane())
            .map(|pane| pane.cwd.clone())
        else {
            self.status_message = Some("The focused session has no working directory".to_string());
            return;
        };
        self.cancel_copilot_review();
        self.github_inspector = Some(GithubInspector::number_prompt_with_filter(
            self.config.github_comment_filter,
        ));
        self.start_github_request(cwd, number);
    }

    pub fn close_github_inspector(&mut self) {
        self.cancel_github_request();
        self.cancel_github_patches();
        self.cancel_copilot_review();
        self.github_inspector = None;
    }

    pub fn github_loading(&self) -> bool {
        self.github_inspector.as_ref().is_some_and(|inspector| {
            matches!(inspector.screen, GithubInspectorScreen::Loading)
                || inspector.copilot_review_status == CopilotReviewStatus::Requesting
        })
    }

    pub fn request_copilot_review(&mut self) {
        let Some((cwd, repository, number, request_id)) =
            self.github_inspector.as_ref().and_then(|inspector| {
                if !inspector.can_request_copilot_review()
                    || matches!(
                        inspector.copilot_review_status,
                        CopilotReviewStatus::Requesting | CopilotReviewStatus::Requested
                    )
                {
                    return None;
                }
                let item = inspector.ready_item()?;
                Some((
                    inspector.request_cwd.clone(),
                    item.common().repository.clone(),
                    item.common().number,
                    inspector.request_id,
                ))
            })
        else {
            return;
        };
        let Some(cwd) = cwd else {
            if let Some(inspector) = self.github_inspector.as_mut() {
                inspector.copilot_review_status = CopilotReviewStatus::Failed(
                    "The inspected pull request has no working directory".to_string(),
                );
            }
            return;
        };

        self.cancel_copilot_review();
        if let Some(inspector) = self.github_inspector.as_mut() {
            inspector.copilot_review_status = CopilotReviewStatus::Requesting;
        }
        let (sender, receiver) = mpsc::channel();
        let cancelled = Arc::new(AtomicBool::new(false));
        let worker_cancelled = Arc::clone(&cancelled);
        std::thread::spawn(move || {
            let result =
                crate::github::request_copilot_review(cwd, repository, number, worker_cancelled);
            let _ = sender.send(GithubReviewResult { request_id, result });
        });
        self.github_review_receiver = Some(receiver);
        self.github_review_cancel = Some(cancelled);
    }

    pub fn submit_github_number(&mut self) {
        let spec = {
            let Some(inspector) = self.github_inspector.as_mut() else {
                return;
            };
            match crate::github::parse_item_spec(&inspector.input) {
                Ok(spec) => spec,
                Err(error) => {
                    inspector.prompt_error = Some(error);
                    return;
                }
            }
        };
        let Some(cwd) = self
            .mux
            .as_ref()
            .and_then(|mux| mux.focused_pane())
            .map(|pane| pane.cwd.clone())
        else {
            if let Some(inspector) = self.github_inspector.as_mut() {
                inspector.prompt_error =
                    Some("The focused session has no working directory".to_string());
            }
            return;
        };
        self.start_github_request_with_kind(cwd, spec.number, spec.kind);
    }

    pub fn retry_github_request(&mut self) {
        let Some((cwd, number, lookup_kind)) =
            self.github_inspector.as_ref().and_then(|inspector| {
                let lookup_kind = if inspector.lookup_kind == crate::github::GithubLookupKind::Auto
                {
                    inspector
                        .ready_item()
                        .map(GithubItem::cache_kind)
                        .unwrap_or(inspector.lookup_kind)
                } else {
                    inspector.lookup_kind
                };
                Some((
                    inspector.request_cwd.clone()?,
                    inspector.number?,
                    lookup_kind,
                ))
            })
        else {
            return;
        };
        // An explicit retry should not hand back the copy the user is trying to
        // get away from.
        self.forget_cached_github_item(&cwd, number, lookup_kind);
        self.start_github_request_with_kind(cwd, number, lookup_kind);
    }

    fn forget_cached_github_item(
        &mut self,
        cwd: &Path,
        number: u64,
        lookup_kind: crate::github::GithubLookupKind,
    ) {
        let Some(repository) = self.github_repositories.get(cwd).cloned().flatten() else {
            return;
        };
        self.github_cache.retain(|entry| {
            entry.number != number
                || entry.repository != repository
                || (lookup_kind != crate::github::GithubLookupKind::Auto
                    && entry.lookup_kind != lookup_kind)
        });
    }

    fn cached_github_item(
        &self,
        cwd: &Path,
        number: u64,
        lookup_kind: crate::github::GithubLookupKind,
    ) -> Option<&GithubItem> {
        let repository = self.github_repositories.get(cwd)?.as_ref()?;
        self.github_cache
            .iter()
            .rev()
            .find(|entry| {
                entry.number == number
                    && &entry.repository == repository
                    && (entry.lookup_kind == lookup_kind
                        || (lookup_kind == crate::github::GithubLookupKind::Auto
                            && entry.lookup_kind
                                == crate::github::GithubLookupKind::IssueOrPullRequest))
            })
            .map(|entry| &entry.item)
    }

    fn store_github_item(&mut self, repository: crate::github::RepositoryRef, item: GithubItem) {
        let lookup_kind = item.cache_kind();
        self.store_github_item_for(repository, item, lookup_kind);
    }

    fn store_github_item_for(
        &mut self,
        repository: crate::github::RepositoryRef,
        item: GithubItem,
        lookup_kind: crate::github::GithubLookupKind,
    ) {
        let number = item.common().number;
        self.github_cache.retain(|entry| {
            entry.number != number
                || entry.repository != repository
                || entry.lookup_kind != lookup_kind
        });
        self.github_cache.push(CachedGithubItem {
            repository,
            number,
            lookup_kind,
            item,
        });
        if self.github_cache.len() > GITHUB_CACHE_LIMIT {
            self.github_cache.remove(0);
        }
    }

    fn store_fetched_github_item(
        &mut self,
        repository: crate::github::RepositoryRef,
        item: GithubItem,
        lookup_kind: crate::github::GithubLookupKind,
        request_generation: Option<u64>,
    ) {
        let number = item.common().number;
        let key = (repository.clone(), number);
        let generation = request_generation
            .unwrap_or_else(|| self.issue_github_reference_generation(repository.clone(), number));
        if self.github_reference_generations.get(&key) == Some(&generation) {
            let status = item.reference_status();
            let authoritative = match lookup_kind {
                crate::github::GithubLookupKind::Auto => Some(status),
                crate::github::GithubLookupKind::IssueOrPullRequest
                | crate::github::GithubLookupKind::Discussion => {
                    match self.github_references.get(&key).copied().flatten() {
                        Some(existing)
                            if existing.kind == crate::github::ReferenceKind::Ambiguous =>
                        {
                            Some(existing)
                        }
                        Some(existing) if existing.kind != status.kind => {
                            Some(crate::github::ReferenceStatus {
                                kind: crate::github::ReferenceKind::Ambiguous,
                                state: existing.state,
                            })
                        }
                        Some(_) => Some(status),
                        None => None,
                    }
                }
            };
            if let Some(status) = authoritative {
                self.github_references.insert(key, Some(status));
                self.github_reference_refreshed_at
                    .insert((repository.clone(), number), Instant::now());
            }
        }
        self.store_github_item_for(repository, item, lookup_kind);
    }

    fn issue_github_reference_generation(
        &mut self,
        repository: crate::github::RepositoryRef,
        number: u64,
    ) -> u64 {
        let generation = self.next_github_reference_generation;
        self.next_github_reference_generation = generation.wrapping_add(1).max(1);
        self.github_reference_generations
            .insert((repository, number), generation);
        generation
    }

    fn start_github_request(&mut self, cwd: PathBuf, number: u64) {
        self.start_github_request_with_kind(cwd, number, crate::github::GithubLookupKind::Auto);
    }

    fn start_github_request_with_kind(
        &mut self,
        cwd: PathBuf,
        number: u64,
        lookup_kind: crate::github::GithubLookupKind,
    ) {
        if lookup_kind == crate::github::GithubLookupKind::Auto {
            let issue_or_pull_request = self
                .cached_github_item(
                    &cwd,
                    number,
                    crate::github::GithubLookupKind::IssueOrPullRequest,
                )
                .cloned();
            let discussion = self
                .cached_github_item(&cwd, number, crate::github::GithubLookupKind::Discussion)
                .cloned();
            if let (Some(issue_or_pull_request), Some(discussion)) =
                (issue_or_pull_request, discussion)
            {
                let comment_filter = self.config.github_comment_filter;
                let inspector = self.github_inspector.get_or_insert_with(|| {
                    GithubInspector::number_prompt_with_filter(comment_filter)
                });
                inspector.request_cwd = Some(cwd.clone());
                inspector.number = Some(number);
                inspector.lookup_kind = lookup_kind;
                inspector.screen = GithubInspectorScreen::Choose {
                    issue_or_pull_request: Box::new(issue_or_pull_request),
                    discussion: Box::new(discussion),
                };
                inspector.reset_navigation();
                self.spawn_github_request(cwd, number, lookup_kind, true);
                return;
            }
        }
        // A cached copy goes on screen straight away, and is checked for
        // staleness in the background rather than making the user wait.
        if let Some(item) = self.cached_github_item(&cwd, number, lookup_kind).cloned() {
            self.show_github_item(cwd, number, lookup_kind, item);
            return;
        }
        self.spawn_github_request(cwd, number, lookup_kind, false);
    }

    /// Put an already-known item on screen and quietly check it is current.
    fn show_github_item(
        &mut self,
        cwd: PathBuf,
        number: u64,
        lookup_kind: crate::github::GithubLookupKind,
        item: GithubItem,
    ) {
        let comment_filter = self.config.github_comment_filter;
        let inspector = self
            .github_inspector
            .get_or_insert_with(|| GithubInspector::number_prompt_with_filter(comment_filter));
        inspector.prompt_error = None;
        inspector.request_cwd = Some(cwd.clone());
        inspector.number = Some(number);
        inspector.lookup_kind = lookup_kind;
        inspector.screen = GithubInspectorScreen::Ready(item);
        inspector.reset_navigation();
        inspector.select_first_tree_file();
        self.spawn_github_request(cwd, number, lookup_kind, true);
    }

    fn spawn_github_request(
        &mut self,
        cwd: PathBuf,
        number: u64,
        lookup_kind: crate::github::GithubLookupKind,
        revalidation: bool,
    ) {
        self.cancel_github_request();
        let request_id = self.next_github_request_id;
        self.next_github_request_id = self.next_github_request_id.wrapping_add(1).max(1);
        let (sender, receiver) = mpsc::channel();
        let cancelled = Arc::new(AtomicBool::new(false));
        let worker_cancelled = Arc::clone(&cancelled);
        let worker_cwd = cwd.clone();
        let known_repository = self.github_repositories.get(&cwd).cloned().flatten();
        let reference_generation = known_repository
            .as_ref()
            .map(|repository| self.issue_github_reference_generation(repository.clone(), number));
        std::thread::spawn(move || {
            let result = crate::github::fetch_item_with_kind(
                worker_cwd,
                crate::github::GithubItemSpec {
                    number,
                    kind: lookup_kind,
                },
                known_repository,
                worker_cancelled,
            );
            let _ = sender.send(GithubLoadResult {
                request_id,
                result,
                lookup_kind,
                revalidation,
                reference_generation,
            });
        });

        let comment_filter = self.config.github_comment_filter;
        let inspector = self
            .github_inspector
            .get_or_insert_with(|| GithubInspector::number_prompt_with_filter(comment_filter));
        inspector.request_id = request_id;
        if !revalidation {
            inspector.screen = GithubInspectorScreen::Loading;
            inspector.prompt_error = None;
            inspector.request_cwd = Some(cwd);
            inspector.number = Some(number);
            inspector.lookup_kind = lookup_kind;
            inspector.reset_navigation();
        }
        self.github_request_receiver = Some(receiver);
        self.github_request_cancel = Some(cancelled);
    }

    fn cancel_github_request(&mut self) {
        if let Some(cancelled) = self.github_request_cancel.take() {
            cancelled.store(true, Ordering::Release);
        }
        self.github_request_receiver = None;
    }

    pub fn poll_github(&mut self) {
        self.poll_github_repository();
        self.poll_github_references();
        self.poll_github_item();
        self.poll_github_patches();
        self.poll_copilot_review();
    }

    fn poll_github_item(&mut self) {
        let Some(receiver) = self.github_request_receiver.as_ref() else {
            return;
        };
        match receiver.try_recv() {
            Ok(result) => {
                self.github_request_receiver = None;
                self.github_request_cancel = None;
                self.apply_github_result(result);
            }
            Err(mpsc::TryRecvError::Disconnected) => {
                self.github_request_receiver = None;
                self.github_request_cancel = None;
                if let Some(inspector) = self.github_inspector.as_mut() {
                    if matches!(inspector.screen, GithubInspectorScreen::Loading) {
                        inspector.screen = GithubInspectorScreen::Error(
                            "GitHub loader stopped before returning a result".to_string(),
                        );
                    }
                }
            }
            Err(mpsc::TryRecvError::Empty) => {}
        }
    }

    fn apply_github_result(&mut self, result: GithubLoadResult) {
        let stale = self
            .github_inspector
            .as_ref()
            .is_none_or(|inspector| inspector.request_id != result.request_id);
        if stale {
            return;
        }

        let fetched = match result.result {
            Ok(fetched) => fetched,
            Err(error) => {
                // A failed background check leaves the copy on screen alone; the
                // user did not ask for it and cannot act on it.
                if !result.revalidation {
                    if let Some(inspector) = self.github_inspector.as_mut() {
                        inspector.screen = GithubInspectorScreen::Error(error.to_string());
                    }
                }
                return;
            }
        };

        if let Some(cwd) = self
            .github_inspector
            .as_ref()
            .and_then(|inspector| inspector.request_cwd.clone())
        {
            self.github_repositories
                .insert(cwd, Some(fetched.repository.clone()));
        }

        if result.revalidation
            && result.lookup_kind == crate::github::GithubLookupKind::Auto
            && !fetched.discussion_checked
        {
            self.store_fetched_github_item(
                fetched.repository,
                fetched.item,
                crate::github::GithubLookupKind::IssueOrPullRequest,
                result.reference_generation,
            );
            return;
        }

        if let GithubItem::Ambiguous {
            issue_or_pull_request,
            discussion,
        } = &fetched.item
        {
            let number = issue_or_pull_request.common().number;
            let key = (fetched.repository.clone(), number);
            let generation = result.reference_generation.unwrap_or_else(|| {
                self.issue_github_reference_generation(fetched.repository.clone(), number)
            });
            if self.github_reference_generations.get(&key) == Some(&generation) {
                self.github_references.insert(
                    key,
                    Some(crate::github::ReferenceStatus {
                        kind: crate::github::ReferenceKind::Ambiguous,
                        state: issue_or_pull_request.reference_status().state,
                    }),
                );
            }
            self.store_github_item_for(
                fetched.repository.clone(),
                issue_or_pull_request.as_ref().clone(),
                crate::github::GithubLookupKind::IssueOrPullRequest,
            );
            self.store_github_item_for(
                fetched.repository,
                discussion.as_ref().clone(),
                crate::github::GithubLookupKind::Discussion,
            );
            let refreshed_selection = result
                .revalidation
                .then(|| {
                    self.github_inspector.as_ref().and_then(|inspector| {
                        let shown = inspector.ready_item()?;
                        let refreshed = match inspector.lookup_kind {
                            crate::github::GithubLookupKind::IssueOrPullRequest => {
                                issue_or_pull_request.as_ref()
                            }
                            crate::github::GithubLookupKind::Discussion => discussion.as_ref(),
                            crate::github::GithubLookupKind::Auto => return None,
                        };
                        (shown.common().updated_at != refreshed.common().updated_at)
                            .then(|| (*refreshed).clone())
                    })
                })
                .flatten();
            if let Some(refreshed) = refreshed_selection {
                if let Some(inspector) = self.github_inspector.as_mut() {
                    inspector.screen = GithubInspectorScreen::Ready(refreshed);
                    inspector.reset_navigation();
                }
            } else if !result.revalidation
                || self.github_inspector.as_ref().is_some_and(|inspector| {
                    inspector.lookup_kind == crate::github::GithubLookupKind::Auto
                        || !matches!(inspector.screen, GithubInspectorScreen::Ready(_))
                })
            {
                if let Some(inspector) = self.github_inspector.as_mut() {
                    inspector.screen = GithubInspectorScreen::Choose {
                        issue_or_pull_request: issue_or_pull_request.clone(),
                        discussion: discussion.clone(),
                    };
                    inspector.reset_navigation();
                }
            }
            return;
        }

        if result.revalidation {
            let unchanged = self
                .github_inspector
                .as_ref()
                .and_then(|inspector| inspector.ready_item())
                .is_some_and(|shown| shown.common().updated_at == fetched.item.common().updated_at);
            self.store_fetched_github_item(
                fetched.repository,
                fetched.item.clone(),
                result.lookup_kind,
                result.reference_generation,
            );
            // Redrawing an identical item would throw away the user's scroll
            // position and any diffs already fetched.
            if unchanged {
                return;
            }
        } else {
            self.store_fetched_github_item(
                fetched.repository,
                fetched.item.clone(),
                result.lookup_kind,
                result.reference_generation,
            );
        }

        let Some(inspector) = self.github_inspector.as_mut() else {
            return;
        };
        inspector.screen = GithubInspectorScreen::Ready(fetched.item);
        inspector.select_first_tree_file();
        inspector.reset_navigation();
    }

    pub fn choose_github_item(&mut self, kind: crate::github::GithubLookupKind) {
        let Some(item) = self
            .github_inspector
            .as_mut()
            .and_then(|inspector| inspector.choose_item(kind))
        else {
            return;
        };
        if let Some(repository) = self
            .github_inspector
            .as_ref()
            .and_then(|inspector| inspector.request_cwd.as_ref())
            .and_then(|cwd| self.github_repositories.get(cwd))
            .cloned()
            .flatten()
        {
            self.store_github_item_for(repository, item, kind);
        }
    }

    /// True while the inspector is showing a pull request's Files tab.
    pub fn github_files_tab_active(&self) -> bool {
        self.github_inspector.as_ref().is_some_and(|inspector| {
            inspector.tab == GithubTab::Files
                && inspector
                    .ready_item()
                    .is_some_and(|item| item.is_pull_request())
        })
    }

    /// True while the diffs for the pull request on screen are still arriving.
    #[cfg(test)]
    pub fn github_patches_loading(&self) -> bool {
        self.github_patch_receiver.is_some()
    }

    /// Fetch diffs for the pull request on screen, unless that is already done.    ///
    /// The single-call loader lists changed files without their patches, so the
    /// Files tab asks for them the first time it is opened.
    pub fn ensure_github_patches(&mut self) {
        let Some(inspector) = self.github_inspector.as_ref() else {
            return;
        };
        let (Some(cwd), Some(number)) = (inspector.request_cwd.clone(), inspector.number) else {
            return;
        };
        let needed = inspector.ready_item().is_some_and(|item| match item {
            GithubItem::PullRequest(pull) => !pull.patches_loaded && !pull.files.is_empty(),
            GithubItem::Issue(_) | GithubItem::Discussion(_) | GithubItem::Ambiguous { .. } => {
                false
            }
        });
        if !needed || self.github_patch_receiver.is_some() {
            return;
        }
        let Some(repository) = self.github_repositories.get(&cwd).cloned().flatten() else {
            return;
        };

        let request_id = inspector.request_id;
        let (sender, receiver) = mpsc::channel();
        let cancelled = Arc::new(AtomicBool::new(false));
        let worker_cancelled = Arc::clone(&cancelled);
        std::thread::spawn(move || {
            let result = crate::github::fetch_patches(cwd, repository, number, worker_cancelled);
            let _ = sender.send(GithubPatchResult { request_id, result });
        });
        self.github_patch_receiver = Some(receiver);
        self.github_patch_cancel = Some(cancelled);
    }

    fn poll_github_patches(&mut self) {
        let Some(receiver) = self.github_patch_receiver.as_ref() else {
            return;
        };
        let result = match receiver.try_recv() {
            Ok(result) => result,
            Err(mpsc::TryRecvError::Empty) => return,
            Err(mpsc::TryRecvError::Disconnected) => {
                self.github_patch_receiver = None;
                self.github_patch_cancel = None;
                return;
            }
        };
        self.github_patch_receiver = None;
        self.github_patch_cancel = None;

        let Ok(patches) = result.result else {
            // Leaving `patches_loaded` false lets opening the tab try again.
            return;
        };
        let Some(inspector) = self.github_inspector.as_mut() else {
            return;
        };
        if inspector.request_id != result.request_id {
            return;
        }
        let GithubInspectorScreen::Ready(GithubItem::PullRequest(pull)) = &mut inspector.screen
        else {
            return;
        };
        let patches: std::collections::HashMap<String, Option<String>> =
            patches.into_iter().collect();
        for file in &mut pull.files {
            if let Some(patch) = patches.get(&file.path) {
                file.patch = patch.clone();
            }
        }
        pull.patches_loaded = true;
        inspector.diff_render_cache = None;

        let repository = pull.common.repository.clone();
        let item = GithubItem::PullRequest(pull.clone());
        self.store_github_item(repository, item);
    }

    fn cancel_github_patches(&mut self) {
        if let Some(cancelled) = self.github_patch_cancel.take() {
            cancelled.store(true, Ordering::Release);
        }
        self.github_patch_receiver = None;
    }

    fn poll_copilot_review(&mut self) {
        let Some(receiver) = self.github_review_receiver.as_ref() else {
            return;
        };
        let result = match receiver.try_recv() {
            Ok(result) => result,
            Err(mpsc::TryRecvError::Empty) => return,
            Err(mpsc::TryRecvError::Disconnected) => {
                self.github_review_receiver = None;
                self.github_review_cancel = None;
                if let Some(inspector) = self.github_inspector.as_mut() {
                    if inspector.copilot_review_status == CopilotReviewStatus::Requesting {
                        inspector.copilot_review_status = CopilotReviewStatus::Failed(
                            "Copilot review requester stopped before returning a result"
                                .to_string(),
                        );
                    }
                }
                return;
            }
        };
        self.github_review_receiver = None;
        self.github_review_cancel = None;
        self.apply_copilot_review_result(result);
    }

    fn apply_copilot_review_result(&mut self, result: GithubReviewResult) {
        let Some(inspector) = self.github_inspector.as_mut() else {
            return;
        };
        if inspector.request_id != result.request_id {
            return;
        }
        inspector.copilot_review_status = match result.result {
            Ok(()) => CopilotReviewStatus::Requested,
            Err(error) => CopilotReviewStatus::Failed(error.to_string()),
        };
    }

    fn cancel_copilot_review(&mut self) {
        if let Some(cancelled) = self.github_review_cancel.take() {
            cancelled.store(true, Ordering::Release);
        }
        self.github_review_receiver = None;
    }

    pub fn attached_scratchpad_visible(&self) -> bool {
        matches!(self.view, View::Attached(id) if self.scratchpad_owner == Some(id)
            && self.scratchpad_open.contains(&id))
            && self.scratchpad.is_some()
    }

    pub fn attached_terminal_visible(&self) -> bool {
        matches!(self.view, View::Attached(id) if self.terminal_owner == Some(id)
            && self.terminal_open.contains(&id))
            && self.terminal.is_visible()
    }

    pub fn collapse_stopped_terminals(&mut self) {
        let stopped: HashSet<String> = self.terminal.stopped_session_ids().into_iter().collect();
        if stopped.is_empty() {
            return;
        }

        let open_stopped_panels: Vec<(crate::mux::PaneId, String)> = self
            .mux
            .as_ref()
            .map(|mux| {
                mux.panes
                    .iter()
                    .filter_map(|pane| {
                        let session_id = &pane.session_id;
                        (self.terminal_open.contains(&pane.id) && stopped.contains(session_id))
                            .then(|| (pane.id, session_id.clone()))
                    })
                    .collect()
            })
            .unwrap_or_default();

        for (pane_id, session_id) in &open_stopped_panels {
            self.remember_terminal_panel(*pane_id, session_id, false);
        }

        let active_stopped = self
            .terminal
            .active_session_id()
            .is_some_and(|session_id| stopped.contains(session_id));
        if active_stopped {
            self.terminal.hide();
            self.terminal_owner = None;
            if self.workspace_focus == WorkspaceFocus::Terminal {
                self.workspace_focus = WorkspaceFocus::Chat;
            }
        }
    }

    pub fn forget_workspace_panels(&mut self, pane_id: crate::mux::PaneId) -> bool {
        if self.scratchpad_owner == Some(pane_id) {
            if let Some(error) = self
                .scratchpad
                .as_mut()
                .and_then(|scratchpad| scratchpad.save().err())
            {
                self.status_message = Some(format!("Scratchpad save failed: {error}"));
                return false;
            }
            self.scratchpad = None;
            self.scratchpad_owner = None;
        }
        self.scratchpad_open.remove(&pane_id);

        if self.terminal_owner == Some(pane_id) {
            self.terminal.hide();
            self.terminal_owner = None;
        }
        self.terminal_open.remove(&pane_id);
        true
    }

    pub fn remember_scratchpad_panel(
        &mut self,
        pane_id: crate::mux::PaneId,
        session_id: &str,
        open: bool,
    ) {
        if open {
            self.scratchpad_open.insert(pane_id);
        } else {
            self.scratchpad_open.remove(&pane_id);
        }
        if self.workspace_state_enabled {
            if let Err(error) = workspace_state::set_scratchpad_open_in(
                &self.workspace_state_root,
                session_id,
                open,
            ) {
                self.status_message = Some(format!("Cannot save workspace state: {error}"));
            }
        }
    }

    pub fn remember_terminal_panel(
        &mut self,
        pane_id: crate::mux::PaneId,
        session_id: &str,
        open: bool,
    ) {
        if open {
            self.terminal_open.insert(pane_id);
        } else {
            self.terminal_open.remove(&pane_id);
        }
        if self.workspace_state_enabled {
            if let Err(error) =
                workspace_state::set_terminal_open_in(&self.workspace_state_root, session_id, open)
            {
                self.status_message = Some(format!("Cannot save workspace state: {error}"));
            }
        }
    }

    fn restore_workspace_panels(&mut self, pane_id: crate::mux::PaneId, session_id: &str) {
        if !self.workspace_state_enabled {
            return;
        }
        match workspace_state::load_in(&self.workspace_state_root, session_id) {
            Ok(state) => {
                if state.scratchpad_open {
                    self.scratchpad_open.insert(pane_id);
                }
                if state.terminal_open {
                    self.terminal_open.insert(pane_id);
                }
            }
            Err(error) => {
                self.status_message = Some(format!("Cannot restore workspace state: {error}"));
            }
        }
    }

    #[cfg(test)]
    pub fn disable_workspace_state_persistence(&mut self) {
        self.workspace_state_enabled = false;
    }

    /// Write the user config, unless a test has opted out of touching real files.
    fn save_config(&mut self) -> anyhow::Result<()> {
        if !self.config_persistence_enabled {
            return Ok(());
        }
        let Some(expected) = self.applied_config_revision else {
            anyhow::bail!(
                "Global settings have no valid loaded revision; current file was not overwritten"
            );
        };
        match config::save_if_revision(
            &self.global_config_path,
            Some(expected),
            &self.persistable_config(),
        )? {
            config::ConfigSaveOutcome::Saved(revision) => {
                self.set_applied_config_revision(revision);
                Ok(())
            }
            config::ConfigSaveOutcome::Changed => {
                self.config_reload_pending = true;
                anyhow::bail!("Global settings changed in another instance; reload and retry")
            }
        }
    }

    pub(crate) fn save_global_settings(&mut self) -> anyhow::Result<()> {
        if !self.config_persistence_enabled {
            return Ok(());
        }
        let Some(expected) = self.settings_config_revision else {
            match config::load_existing_base_config_with_revision(&self.global_config_path)? {
                Some((mut persisted, revision)) => {
                    persisted.snippets = self.config.snippets.clone();
                    self.adopt_persisted_config(persisted);
                    self.set_applied_config_revision(revision);
                    self.settings_config_revision = Some(revision);
                    self.config_reload_pending = false;
                    self.theme_save_reloaded_external = true;
                    anyhow::bail!(
                        "Reloaded restored global settings instead of overwriting them; review and save again"
                    );
                }
                None => anyhow::bail!(
                    "Global settings file is temporarily unavailable; current values were not overwritten"
                ),
            }
        };
        match config::save_if_revision(
            &self.global_config_path,
            Some(expected),
            &self.persistable_config(),
        )? {
            config::ConfigSaveOutcome::Saved(revision) => {
                self.set_applied_config_revision(revision);
                self.settings_config_revision = Some(revision);
                self.config_reload_pending = false;
                Ok(())
            }
            config::ConfigSaveOutcome::Changed => {
                let Some((mut persisted, revision)) =
                    config::load_existing_base_config_with_revision(&self.global_config_path)?
                else {
                    self.config_reload_pending = true;
                    self.settings_config_revision = None;
                    anyhow::bail!(
                        "Global settings file is temporarily unavailable; current values were not overwritten"
                    );
                };
                persisted.snippets = self.config.snippets.clone();
                self.adopt_persisted_config(persisted);
                self.set_applied_config_revision(revision);
                self.settings_config_revision = Some(revision);
                self.config_reload_pending = false;
                self.theme_save_reloaded_external = true;
                anyhow::bail!(
                    "Global settings changed in another instance; reloaded them instead of overwriting"
                );
            }
        }
    }

    #[cfg(test)]
    pub fn disable_config_persistence(&mut self) {
        self.config_persistence_enabled = false;
    }

    #[cfg(test)]
    fn set_global_config_path(&mut self, path: PathBuf) {
        self.global_config_path = path;
    }

    #[cfg(test)]
    fn set_workspace_state_root(&mut self, root: PathBuf) {
        self.workspace_state_root = root;
    }

    pub fn selected_session(&self) -> Option<&Session> {
        self.filtered_indices
            .get(self.selected)
            .and_then(|&idx| self.sessions.get(idx))
    }

    pub fn selected_real_index(&self) -> Option<usize> {
        self.filtered_indices.get(self.selected).copied()
    }

    fn refresh_live_pane_sessions(&mut self) {
        let session_ids: Vec<String> = self
            .mux
            .as_ref()
            .map(|mux| {
                mux.panes
                    .iter()
                    .map(|pane| pane.session_id.clone())
                    .collect()
            })
            .unwrap_or_default();
        let mut refreshed = Vec::new();
        for session_id in session_ids {
            match crate::session::loader::load_session(&self.copilot_home, &session_id) {
                Ok(Some(session)) => refreshed.push(session),
                Ok(None) => {}
                Err(error) => {
                    self.status_message =
                        Some(format!("Cannot refresh session {session_id}: {error}"));
                }
            }
        }
        self.merge_session_metadata(refreshed);
    }

    fn merge_session_metadata(&mut self, sessions: Vec<Session>) {
        if sessions.is_empty() {
            return;
        }

        let selected_id = self.selected_session().map(|session| session.id.clone());
        for session in sessions {
            if let Some(index) = self
                .sessions
                .iter()
                .position(|existing| existing.id == session.id)
            {
                let old = std::mem::replace(&mut self.sessions[index], session);
                carry_session_details(&mut self.sessions[index], old);
            } else {
                self.sessions.push(session);
            }
        }

        self.rebuild_unique_projects();
        self.sort_sessions();
        if let Some(session_id) = selected_id {
            self.focus_session(&session_id);
        }
    }

    pub fn track_external_session(&mut self, session_id: String) {
        let now = Instant::now();
        self.pending_external_sessions.insert(
            session_id,
            PendingExternalSession {
                next_attempt: now,
                deadline: now + Duration::from_secs(60),
            },
        );
    }

    pub fn poll_external_sessions(&mut self) {
        let now = Instant::now();
        let due: Vec<String> = self
            .pending_external_sessions
            .iter()
            .filter(|(_, pending)| pending.next_attempt <= now)
            .map(|(session_id, _)| session_id.clone())
            .collect();
        for session_id in due {
            match crate::session::loader::load_session(&self.copilot_home, &session_id) {
                Ok(Some(session)) => {
                    self.pending_external_sessions.remove(&session_id);
                    self.merge_session_metadata(vec![session]);
                    self.focus_session(&session_id);
                }
                Ok(None) => {
                    let expired = self
                        .pending_external_sessions
                        .get(&session_id)
                        .is_some_and(|pending| pending.deadline <= now);
                    if expired {
                        self.pending_external_sessions.remove(&session_id);
                    } else if let Some(pending) =
                        self.pending_external_sessions.get_mut(&session_id)
                    {
                        pending.next_attempt = now + Duration::from_millis(500);
                    }
                }
                Err(error) => {
                    self.pending_external_sessions.remove(&session_id);
                    self.status_message = Some(format!(
                        "Cannot load new tmux session {session_id}: {error}"
                    ));
                }
            }
        }
    }

    pub fn begin_session_load(
        &mut self,
        receiver: mpsc::Receiver<crate::session::loader::SessionLoadResult>,
    ) {
        self.session_load_receiver = Some(receiver);
    }

    pub fn sessions_loading(&self) -> bool {
        self.session_load_receiver.is_some()
    }

    pub fn background_work_pending(&self) -> bool {
        self.session_load_receiver.is_some()
            || self.update_receiver.is_some()
            || self.update_install_receiver.is_some()
            || self.update_restart_ready()
            || self.notification_pending > 0
            || self.github_request_receiver.is_some()
            || self.github_patch_receiver.is_some()
            || self.github_review_receiver.is_some()
            || self.github_repo_receiver.is_some()
            || self.github_reference_receiver.is_some()
            || self
                .scratchpad
                .as_ref()
                .is_some_and(Scratchpad::autosave_pending)
    }

    pub fn detail_load_pending(&self) -> bool {
        self.detail_pending.is_some()
    }

    pub fn begin_notification_cycle(&mut self, session_id: &str) {
        if session_id.is_empty() {
            return;
        }
        let length = std::fs::metadata(self.notification_events_path(session_id))
            .map(|metadata| metadata.len())
            .unwrap_or(0);
        self.notification_cycle_offsets
            .insert(session_id.to_string(), length);
    }

    #[cfg(test)]
    pub fn notification_cycle_offset(&self, session_id: &str) -> Option<u64> {
        self.notification_cycle_offsets.get(session_id).copied()
    }

    fn notification_events_path(&self, session_id: &str) -> PathBuf {
        self.copilot_home
            .join("session-state")
            .join(session_id)
            .join("events.jsonl")
    }

    pub fn enqueue_notification(
        &mut self,
        kind: NotificationKind,
        session_title: String,
        session_id: Option<&str>,
    ) {
        if self.notification_drain_started.is_some() {
            return;
        }
        // Notification routing and credentials are side-effect-critical. While
        // Settings is open, read a separate disk snapshot so unsaved UI edits remain
        // untouched but another instance can still revoke or redirect delivery.
        let effective = if self.mode == Mode::Settings {
            match config::load_existing_base_config(&self.global_config_path) {
                Ok(Some(config)) => config,
                Ok(None) => return,
                Err(error) => {
                    self.status_message = Some(format!(
                        "Notification skipped: cannot read global settings: {error}"
                    ));
                    return;
                }
            }
        } else {
            self.request_config_reload();
            self.config.clone()
        };
        let configured = &effective.notifications;
        let event_enabled = match kind {
            NotificationKind::Ready
            | NotificationKind::Question
            | NotificationKind::PlanApproval => configured.ready,
            NotificationKind::Error => configured.error,
        };
        if !configured.enabled || !event_enabled {
            return;
        }
        if let Err(error) = config::validate_user_notification_config(&effective) {
            self.status_message = Some(format!("Notifications disabled: {error}"));
            return;
        }
        let events_path = session_id
            .filter(|session_id| effective.ntfy_verbose && !session_id.is_empty())
            .map(|session_id| self.notification_events_path(session_id));
        let events_start = session_id
            .and_then(|session_id| self.notification_cycle_offsets.get(session_id))
            .copied();
        let events_end = events_path
            .as_ref()
            .and_then(|path| std::fs::metadata(path).ok())
            .map(|metadata| metadata.len());
        let request = NotificationRequest {
            config: configured.clone(),
            access_token: effective.ntfy_access_token,
            verbose: effective.ntfy_verbose,
            session_title,
            kind,
            events_path,
            events_start,
            events_end,
        };
        #[cfg(test)]
        {
            self.notification_requests.push(request);
        }
        #[cfg(not(test))]
        {
            let worker = self
                .notification_worker
                .get_or_insert_with(NotificationWorker::start);
            match worker.enqueue(request) {
                Ok(()) => self.notification_pending += 1,
                Err(error) => {
                    self.status_message = Some(format!("Notification failed: {error}"));
                }
            }
        }
    }

    pub fn poll_notifications(&mut self) {
        let Some(worker) = self.notification_worker.as_ref() else {
            return;
        };
        while let Some(result) = worker.try_result() {
            self.notification_pending = self.notification_pending.saturating_sub(1);
            if let Err(error) = result.result {
                self.status_message = Some(format!("Notification failed: {error}"));
            }
        }
    }

    pub fn exit_waits_for_notifications(&mut self) -> bool {
        const DRAIN_TIMEOUT: Duration = Duration::from_secs(2);
        if self.notification_pending == 0 {
            return false;
        }
        let started = *self
            .notification_drain_started
            .get_or_insert_with(Instant::now);
        if started.elapsed() < DRAIN_TIMEOUT {
            self.status_message =
                Some("Sending final phone notification before leaving...".to_string());
            true
        } else {
            false
        }
    }

    pub fn cancel_notification_drain(&mut self) {
        self.notification_drain_started = None;
    }

    pub fn poll_session_load(&mut self) {
        let Some(receiver) = self.session_load_receiver.as_ref() else {
            return;
        };
        match receiver.try_recv() {
            Ok(Ok(sessions)) => {
                self.apply_loaded_catalog(sessions);
                self.session_load_receiver = None;
            }
            Ok(Err(error)) => {
                self.status_message = Some(format!("Cannot load sessions: {error}"));
                self.session_load_receiver = None;
            }
            Err(mpsc::TryRecvError::Disconnected) => {
                self.status_message =
                    Some("Cannot load sessions: background worker stopped".to_string());
                self.session_load_receiver = None;
            }
            Err(mpsc::TryRecvError::Empty) => {}
        }
    }

    fn apply_loaded_catalog(&mut self, sessions: Vec<Session>) {
        let selected_id = self.selected_session().map(|session| session.id.clone());
        let mut previous: HashMap<String, Session> = std::mem::take(&mut self.sessions)
            .into_iter()
            .map(|session| (session.id.clone(), session))
            .collect();
        let mut catalog = Vec::with_capacity(sessions.len() + previous.len());
        for session in sessions {
            if let Some(current) = previous.remove(&session.id) {
                // Favorites and newly created panes can be refreshed while the worker is
                // scanning. Their in-memory metadata is newer than its snapshot.
                catalog.push(current);
            } else if session.dir_path.join("workspace.yaml").is_file() {
                // A session deleted after the worker read it must not come back as a ghost.
                catalog.push(session);
            }
        }
        // A session can be created while the worker is scanning. Keep anything merged
        // after its snapshot rather than making it briefly disappear at completion.
        catalog.extend(previous.into_values());
        self.sessions = catalog;
        self.rebuild_unique_projects();
        self.sort_sessions();
        if let Some(session_id) = selected_id {
            self.focus_session(&session_id);
        }
    }

    fn rebuild_unique_projects(&mut self) {
        let selected_project = self.unique_projects.get(self.project_selected).cloned();
        self.unique_projects = extract_unique_projects(&self.sessions);
        if let Some(project) = self.cwd_project.as_ref() {
            if !self
                .unique_projects
                .iter()
                .any(|known| known.eq_ignore_ascii_case(project))
            {
                self.unique_projects.insert(0, project.clone());
            }
        }
        self.project_selected = selected_project
            .as_ref()
            .and_then(|selected| {
                self.unique_projects
                    .iter()
                    .position(|project| project.eq_ignore_ascii_case(selected))
            })
            .unwrap_or_else(|| {
                self.project_selected
                    .min(self.unique_projects.len().saturating_sub(1))
            });
    }

    /// Persisted Copilot names are more useful in the pane switcher than OSC window
    /// titles, which can lag behind `/rename` and generated-name updates.
    pub fn pane_session_title<'a>(&'a self, session_id: &str, fallback: &'a str) -> &'a str {
        self.sessions
            .iter()
            .find(|session| session.id == session_id)
            .map(Session::display_name)
            .filter(|title| *title != "(unnamed)")
            .unwrap_or(fallback)
    }

    pub fn move_up(&mut self) {
        if self.selected > 0 {
            self.selected -= 1;
            if self.selected < self.scroll_offset {
                self.scroll_offset = self.selected;
            }
        }
    }

    pub fn move_down(&mut self) {
        if self.selected + 1 < self.filtered_indices.len() {
            self.selected += 1;
            if self.selected >= self.scroll_offset + self.visible_rows {
                self.scroll_offset = self.selected - self.visible_rows + 1;
            }
        }
    }

    pub fn apply_filter(&mut self) {
        let matcher = SkimMatcherV2::default();

        self.filtered_indices = self
            .sessions
            .iter()
            .enumerate()
            .filter(|(_, s)| {
                if !self.show_hidden_sessions
                    && (self.session_title_is_hidden(s) || self.session_path_is_hidden(s))
                {
                    return false;
                }
                // Project filter
                if let Some(ref proj) = self.project_filter {
                    if !s.project_root.eq_ignore_ascii_case(proj) {
                        return false;
                    }
                }
                // Search filter
                if !self.search_query.is_empty() {
                    // Match fields independently. Concatenating them let a short query
                    // start in a title/path and finish in the UUID, so nearly every
                    // favorite in the same project appeared to match.
                    return [
                        s.display_name(),
                        s.project_root.as_str(),
                        s.cwd.as_str(),
                        s.id.as_str(),
                    ]
                    .into_iter()
                    .any(|field| matcher.fuzzy_match(field, &self.search_query).is_some());
                }
                true
            })
            .map(|(i, _)| i)
            .collect();

        if self.project_filter.is_none() && self.search_query.is_empty() {
            // Favorites lead in the order the user arranged; everything else keeps the
            // selected sort, which the stable sort preserves.
            let mut ordered = std::mem::take(&mut self.filtered_indices);
            ordered.sort_by_key(|&index| {
                self.favorite_rank(&self.sessions[index].id)
                    .unwrap_or(usize::MAX)
            });
            self.filtered_indices = ordered;
        }

        // Reset selection if out of bounds
        if self.selected >= self.filtered_indices.len() {
            self.selected = self.filtered_indices.len().saturating_sub(1);
        }
        self.scroll_offset = 0;
    }

    fn session_title_is_hidden(&self, session: &Session) -> bool {
        let title = session.display_name().to_lowercase();
        self.config
            .hidden_title_prefixes
            .iter()
            .any(|prefix| !prefix.is_empty() && title.starts_with(&prefix.to_lowercase()))
    }

    fn session_path_is_hidden(&self, session: &Session) -> bool {
        self.config
            .hidden_path_prefixes
            .iter()
            .any(|prefix| path_has_prefix(&session.cwd, prefix))
    }

    pub fn toggle_hidden_sessions(&mut self) -> bool {
        let selected_id = self.selected_session().map(|session| session.id.clone());
        self.show_hidden_sessions = !self.show_hidden_sessions;
        self.apply_filter();
        if let Some(session_id) = selected_id {
            self.focus_session(&session_id);
        }
        self.show_hidden_sessions
    }

    pub fn is_favorite(&self, session_id: &str) -> bool {
        self.favorite_rank(session_id).is_some()
    }

    /// Position of a session within the user's favorite order, if it is one.
    pub fn favorite_rank(&self, session_id: &str) -> Option<usize> {
        self.config.favorites.iter().position(|id| id == session_id)
    }

    /// True when the list is showing the arranged favorites group.
    ///
    /// Grouping only applies to the unfiltered view, so a search or project filter
    /// leaves the chosen sort untouched.
    pub fn favorites_section_active(&self) -> bool {
        self.project_filter.is_none()
            && self.search_query.is_empty()
            && self
                .filtered_indices
                .iter()
                .any(|&index| self.is_favorite(&self.sessions[index].id))
    }

    /// Lines the list spends on group headers.
    ///
    /// Both the renderer and the scroll maths need this, and they must agree or the
    /// selection drifts out of view, so it is computed in exactly one place.
    pub fn list_header_lines(&self) -> usize {
        if !self.favorites_section_active() {
            return 0;
        }
        // `apply_filter` sorts favorites to the front, so counting the leading run is
        // exact and costs the size of the group rather than the whole list — this
        // runs on every frame.
        let favorites = self
            .filtered_indices
            .iter()
            .take_while(|&&index| self.is_favorite(&self.sessions[index].id))
            .count();
        // The trailing header only exists when ordinary sessions follow.
        1 + usize::from(favorites < self.filtered_indices.len())
    }

    pub fn toggle_selected_favorite(&mut self) -> anyhow::Result<Option<bool>> {
        let Some(session_id) = self.selected_session().map(|session| session.id.clone()) else {
            return Ok(None);
        };
        let previous = self.config.favorites.clone();
        let was_favorite = self.is_favorite(&session_id);

        if was_favorite {
            self.config.favorites.retain(|id| id != &session_id);
        } else {
            // New favorites land at the end so starring one never silently
            // rearranges the order the user already set.
            self.config.favorites.push(session_id.clone());
        }

        if let Err(error) = self.save_config() {
            self.config.favorites = previous;
            return Err(error);
        }

        self.apply_filter();
        self.focus_session(&session_id);

        Ok(Some(!was_favorite))
    }

    pub fn forget_favorite(&mut self, session_id: &str) -> anyhow::Result<bool> {
        let Some(rank) = self.favorite_rank(session_id) else {
            return Ok(false);
        };
        let removed = self.config.favorites.remove(rank);
        if let Err(error) = self.save_config() {
            self.config.favorites.insert(rank, removed);
            return Err(error);
        }
        Ok(true)
    }

    /// Start or stop dragging the selected favorite through the order.
    ///
    /// Returns the message to show, or `None` when nothing was grabbed.
    pub fn toggle_favorite_grab(&mut self) -> Option<String> {
        if self.grabbed_favorite.take().is_some() {
            return Some("Order saved".to_string());
        }
        let session_id = self.selected_session()?.id.clone();
        if !self.is_favorite(&session_id) {
            return Some(
                "Only favorites can be reordered — press Space to add this one".to_string(),
            );
        }
        if !self.favorites_section_active() {
            return Some("Clear the filter and search to reorder favorites".to_string());
        }
        self.grabbed_favorite = Some(session_id);
        Some("Moving favorite — ↑/↓ to move, Enter or Esc to drop".to_string())
    }

    pub fn release_favorite_grab(&mut self) -> bool {
        self.grabbed_favorite.take().is_some()
    }

    /// Move the grabbed favorite one place up (`-1`) or down (`1`).
    ///
    /// Persists immediately, so the arrangement survives however the session ends —
    /// which is the whole point of arranging it.
    pub fn move_grabbed_favorite(&mut self, offset: isize) -> Option<String> {
        let session_id = self.grabbed_favorite.clone()?;
        let from = self.favorite_rank(&session_id)?;
        // Favorites whose session no longer exists stay in the config but never
        // appear in the list, so movement is measured in visible steps. Otherwise a
        // keypress could swap the row past an invisible neighbour and look stuck.
        let visible: Vec<usize> = self
            .config
            .favorites
            .iter()
            .enumerate()
            .filter(|(_, id)| self.sessions.iter().any(|session| &session.id == *id))
            .map(|(rank, _)| rank)
            .collect();
        let Some(to) = visible
            .iter()
            .position(|&rank| rank == from)
            .and_then(|position| position.checked_add_signed(offset))
            .and_then(|position| visible.get(position).copied())
        else {
            // Already at the end of the run; staying put is friendlier than wrapping.
            return None;
        };

        let previous = self.config.favorites.clone();
        let moved = self.config.favorites.remove(from);
        self.config.favorites.insert(to, moved);

        if let Err(error) = self.save_config() {
            self.config.favorites = previous;
            self.grabbed_favorite = None;
            return Some(format!("Failed to save order: {error}"));
        }

        self.apply_filter();
        self.focus_session(&session_id);
        None
    }

    /// Put the cursor on a session and scroll it into view.
    fn focus_session(&mut self, session_id: &str) {
        let Some(display_index) = self
            .filtered_indices
            .iter()
            .position(|&index| self.sessions[index].id == session_id)
        else {
            return;
        };
        self.selected = display_index;
        if self.selected < self.scroll_offset {
            self.scroll_offset = self.selected;
        } else if self.visible_rows > 0 && self.selected >= self.scroll_offset + self.visible_rows {
            self.scroll_offset = self.selected + 1 - self.visible_rows;
        }
    }

    pub fn cycle_sort(&mut self) {
        self.sort_field = match self.sort_field {
            SortField::LastUsed => SortField::Created,
            SortField::Created => SortField::Name,
            SortField::Name => SortField::Project,
            SortField::Project => SortField::LastUsed,
        };
        self.sort_sessions();
    }

    fn sort_sessions(&mut self) {
        match self.sort_field {
            SortField::LastUsed => {
                self.sessions.sort_by(|a, b| {
                    let at = a.updated_at.or(a.created_at);
                    let bt = b.updated_at.or(b.created_at);
                    bt.cmp(&at)
                });
            }
            SortField::Created => {
                self.sessions
                    .sort_by(|a, b| b.created_at.cmp(&a.created_at));
            }
            SortField::Name => {
                self.sessions.sort_by(|a, b| {
                    a.display_name()
                        .to_lowercase()
                        .cmp(&b.display_name().to_lowercase())
                });
            }
            SortField::Project => {
                self.sessions
                    .sort_by(|a, b| a.project_root.cmp(&b.project_root));
            }
        }
        self.apply_filter();
    }

    /// Returns indices into `unique_projects` that match the current project search query.
    pub fn filtered_project_indices(&self) -> Vec<usize> {
        if self.project_search_query.is_empty() {
            return (0..self.unique_projects.len()).collect();
        }
        let matcher = SkimMatcherV2::default();
        self.unique_projects
            .iter()
            .enumerate()
            .filter(|(_, project)| {
                let short_name = std::path::Path::new(project)
                    .file_name()
                    .and_then(|n| n.to_str())
                    .unwrap_or(project);
                matcher
                    .fuzzy_match(short_name, &self.project_search_query)
                    .is_some()
            })
            .map(|(i, _)| i)
            .collect()
    }

    pub fn set_project_filter(&mut self, project: Option<String>) {
        self.project_filter = project;
        self.apply_filter();
    }

    /// Record the directory `cst` was launched from. When that directory belongs to a
    /// Git project it becomes selectable in the project filter even if it has no
    /// sessions yet, and is auto-selected as the current filter.
    pub fn set_cwd_context(&mut self, cwd: String, auto_filter: bool) {
        let project = crate::session::loader::detect_project_root(&cwd);
        self.cwd = Some(cwd);

        let Some(project) = project else {
            return;
        };

        let known = self
            .unique_projects
            .iter()
            .any(|p| p.eq_ignore_ascii_case(&project));
        if !known {
            self.unique_projects.insert(0, project.clone());
        }
        self.cwd_project = Some(project.clone());

        if auto_filter {
            self.set_project_filter(Some(project));
        }
    }

    /// Project used by project-scoped actions: the active filter, falling back to the
    /// project of the launch directory.
    pub fn active_project(&self) -> Option<String> {
        self.project_filter
            .clone()
            .or_else(|| self.cwd_project.clone())
    }

    /// Project context for a command invoked from any surface.
    ///
    /// While attached, the focused pane is authoritative; the hidden session-list
    /// filter may still point at a different repository.
    pub fn command_project(&self) -> Option<String> {
        match self.view {
            View::Attached(_) => self
                .mux
                .as_ref()
                .and_then(|mux| mux.focused_pane())
                .map(|pane| pane.cwd.to_string_lossy().to_string()),
            View::List => self.active_project(),
        }
    }

    /// Directory to start a plain new session in.
    pub fn new_session_dir(&self) -> Option<String> {
        match self.view {
            View::Attached(_) => self
                .mux
                .as_ref()
                .and_then(|mux| mux.focused_pane())
                .map(|pane| pane.cwd.to_string_lossy().to_string()),
            View::List => self.active_project().or_else(|| self.cwd.clone()),
        }
    }

    /// Attach an existing Copilot session as a pane, or focus it if already attached.
    pub fn attach_session(&mut self, session_id: &str, cwd: &str, title: String) -> Result<()> {
        // A pane that already exited is not worth re-focusing; drop it and resume afresh.
        if let Some(mux) = self.mux.as_mut() {
            if let Some(existing) = mux.pane_for_session(session_id) {
                let running = mux.pane(existing).is_some_and(|pane| pane.is_running());
                if running {
                    mux.focused = Some(existing);
                    self.view = View::Attached(existing);
                    return Ok(());
                }
                mux.remove(existing);
            }
        }
        if let Some(reference) = tmux::find_live(session_id)? {
            return self.attach_tmux_session(reference, title);
        }
        self.tmux_sessions.remove(session_id);
        let (program, args) = manager::resume_command(session_id, &self.config, Path::new(cwd))?;
        self.spawn_pane(
            title,
            PathBuf::from(cwd),
            session_id.to_string(),
            program,
            args,
        )
    }

    /// The tab strip only earns its two rows once there is something to switch between.
    pub fn tab_bar_visible(&self) -> bool {
        self.mux.as_ref().is_some_and(|mux| mux.panes.len() > 1)
    }

    /// Open every inactive favorite as a pane, in the user's configured order.
    ///
    /// Focus lands on the first favorite opened rather than the last, so the arranged
    /// order is what the user sees when the panes finish spawning.
    pub fn open_favorites_as_panes(&mut self) -> String {
        let plan = crate::windows_terminal::build_launch_plan(&self.sessions, &self.config);
        let configured = self.config.favorites.len();
        let mut failed: Vec<String> = Vec::new();
        let mut launched = 0;
        let mut first: Option<crate::mux::PaneId> = None;

        for tab in plan.tabs {
            // The session's own cwd is used rather than the plan's validated directory,
            // matching how resuming a single session behaves when the path has moved.
            let cwd = self
                .sessions
                .iter()
                .find(|session| session.id == tab.session_id)
                .map(|session| session.cwd.clone())
                .unwrap_or_default();
            match self.attach_session(&tab.session_id, &cwd, tab.title.clone()) {
                Ok(()) => {
                    launched += 1;
                    if first.is_none() {
                        first = self.mux.as_ref().and_then(|mux| mux.focused);
                    }
                }
                Err(error) => failed.push(format!("{}: {error}", tab.title)),
            }
        }

        if let Some(id) = first {
            if let Some(mux) = self.mux.as_mut() {
                mux.focused = Some(id);
            }
            self.view = View::Attached(id);
        }

        let report = crate::windows_terminal::FavoriteLaunchReport::new(
            configured,
            launched,
            plan.active,
            plan.stale,
        );
        let message = report.status_message_for("pane");
        if failed.is_empty() {
            message
        } else {
            format!("{message}; {} failed to open", failed.len())
        }
    }

    /// Start a brand new Copilot session as a pane.
    pub fn attach_new_session(&mut self, cwd: &str, title: String) -> Result<()> {
        let (program, args, session_id) =
            manager::new_session_command(&self.config, Path::new(cwd))?;
        self.spawn_pane(title, PathBuf::from(cwd), session_id, program, args)
    }

    pub fn attach_tmux_session(&mut self, reference: TmuxSessionRef, title: String) -> Result<()> {
        let (program, args) = tmux::attach_command(&reference);
        let session_id = reference.session_id.clone();
        self.spawn_pane(
            title,
            reference.cwd.clone(),
            session_id.clone(),
            program,
            args,
        )?;
        self.tmux_sessions.insert(session_id, reference);
        Ok(())
    }

    fn spawn_pane(
        &mut self,
        title: String,
        cwd: PathBuf,
        session_id: String,
        program: String,
        args: Vec<String>,
    ) -> Result<()> {
        let (rows, cols) = self.pane_size;
        let events_path = self.notification_events_path(&session_id);
        let terminal_light_mode = self.theme_name().terminal_light_mode();
        // Read now rather than cached at startup: `cst hooks install` from another
        // terminal should take effect on the next pane, not the next CST.
        let hooks_active = crate::hook_plugin::is_managed(&self.copilot_home);
        let Some(mux) = self.mux.as_mut() else {
            anyhow::bail!("Multiplexing is disabled");
        };
        let id = mux.allocate_id();
        // Sessions with no name yet would otherwise render as a blank tab and produce
        // messages like "Session '' exited".
        let title = match title.trim() {
            "" => format!("session {id}"),
            trimmed => trimmed.to_string(),
        };
        let workspace_session_id = session_id.clone();
        let pane = Pane::spawn(
            PaneSpec {
                id,
                title,
                cwd,
                session_id,
                program,
                args,
                events_path: Some(events_path),
                terminal_light_mode,
                hooks_active,
            },
            rows,
            cols,
            mux.events.clone(),
        )?;
        mux.push(pane);
        self.view = View::Attached(id);
        self.restore_workspace_panels(id, &workspace_session_id);
        Ok(())
    }

    /// Leave the focused pane running and return to the session list.
    pub fn detach(&mut self) {
        if let Some(error) = self
            .scratchpad
            .as_mut()
            .and_then(|scratchpad| scratchpad.save().err())
        {
            self.status_message = Some(format!("Scratchpad save failed: {error}"));
        }
        self.terminal.unfocus();
        self.workspace_focus = WorkspaceFocus::Chat;
        self.view = View::List;
        if let Some(mux) = self.mux.as_mut() {
            mux.prefix_state = PrefixState::Idle;
        }
        self.refresh_live_pane_sessions();
    }

    /// `prefix q` — quit CST, ending direct panes and detaching tmux-backed panes.
    ///
    /// A live focused session may still be working, so quitting always confirms when
    /// any pane would be terminated. Exited panes can be dismissed immediately.
    pub fn request_quit_from_pane(&mut self) {
        let running = self
            .mux
            .as_mut()
            .map(|mux| {
                mux.reap();
                mux.running_count()
            })
            .unwrap_or(0);
        if running > 0 {
            self.confirm_quit = true;
        } else {
            self.should_quit = true;
        }
    }

    /// Panes owned by this instance, for marking rows in the session list.
    pub fn pane_for_session(&self, session_id: &str) -> Option<crate::mux::PaneId> {
        self.mux
            .as_ref()
            .and_then(|mux| mux.pane_for_session(session_id))
    }

    /// Whether a session is live in one of our panes. Exited panes are not marked, since
    /// the session is resumable again.
    pub fn has_running_pane_for(&self, session_id: &str) -> bool {
        self.mux.as_ref().is_some_and(|mux| {
            mux.pane_for_session(session_id)
                .and_then(|id| mux.pane(id))
                .is_some_and(|pane| pane.is_running())
        })
    }

    /// How long a wake-up waits for a pane to become writable before the user is asked.
    ///
    /// The wait is usually seconds — a turn finishing, or a half-typed message being
    /// sent. A draft left on screen while somebody goes to lunch would otherwise hold the
    /// message indefinitely, and a thread nobody hears about is the exact failure this
    /// feature exists to prevent.
    const WAKE_PATIENCE_SECS: i64 = 300;

    /// Act on the notices the watcher wrote down, now that pane state is knowable.
    ///
    /// The watcher settles *whether* a comment should reach a session and records that
    /// decision on disk; only the UI thread can see whether the session has a running
    /// pane, whether it is busy, and whether somebody is typing into it. Splitting it
    /// this way is also what makes a restart harmless: the decision outlives the process
    /// that made it, so a notice planned just before a crash is still delivered after.
    pub fn apply_thread_notices(&mut self) -> bool {
        self.refresh_thread_pending();
        self.flush_thread_wakes()
    }

    /// Deliver what can be delivered, and give up on what has waited too long.
    ///
    /// Called on every idle tick as well as on events, because a pane holding an
    /// abandoned draft emits nothing at all. The counter is what keeps that affordable:
    /// an install with no notices never touches the disk, and the only thing that can
    /// add one behind CST's back — the watcher thread — announces itself.
    pub fn flush_thread_wakes(&mut self) -> bool {
        if self.thread_live_notices == 0 {
            return false;
        }
        let live: Vec<(String, crate::threads::ThreadRef, bool, i64)> =
            crate::threads::store::load_in(&self.thread_state_root)
                .notices
                .iter()
                .filter_map(|notice| {
                    let sent = match notice.status {
                        crate::threads::NoticeStatus::Planned => false,
                        crate::threads::NoticeStatus::Sent => true,
                        // Already in front of the user; nothing to do without them.
                        crate::threads::NoticeStatus::Waiting { .. } => return None,
                    };
                    Some((
                        notice.session_id.clone(),
                        notice.thread.clone(),
                        sent,
                        notice.age_seconds(),
                    ))
                })
                .collect();
        if live.is_empty() {
            return false;
        }

        let mut changed = false;
        for (session_id, thread, sent, age) in live {
            if sent {
                // Written into the composer already. It keeps holding this session's
                // slot until the composer is empty again, which is the evidence that a
                // turn actually took it — otherwise the next notice would be typed on
                // top of this one and the two would arrive as one unreadable message.
                if self.composer_is_free(&session_id) {
                    self.update_threads(|state| state.drop_notice(&session_id, &thread));
                    changed = true;
                }
                continue;
            }
            if !self.has_running_pane_for(&session_id) {
                // Never start a session unasked. It becomes something the user can
                // approve, with an age, so a stalled correspondence stays visible.
                self.hold_thread_message(
                    &session_id,
                    &thread,
                    crate::threads::PendingReason::SessionClosed,
                );
                changed = true;
                continue;
            }
            // Readiness is re-asked per notice rather than decided once up front:
            // delivering one occupies that pane's composer, so a second notice for the
            // same session has to wait for the next pass.
            if self.session_ready_for_wake(&session_id) {
                match self.deliver_thread_pointer(&session_id, &thread) {
                    Ok(()) => self.update_threads(|state| {
                        if let Some(notice) = state.notice_mut(&session_id, &thread) {
                            notice.status = crate::threads::NoticeStatus::Sent;
                        }
                    }),
                    Err(error) => {
                        // The pane went away between the check and the write. Hand it to
                        // the user rather than dropping it silently.
                        self.hold_thread_message(
                            &session_id,
                            &thread,
                            crate::threads::PendingReason::SessionClosed,
                        );
                        self.status_message = Some(format!("Could not wake the session: {error}"));
                    }
                }
                changed = true;
            } else if age >= Self::WAKE_PATIENCE_SECS {
                // Waited too long and still nowhere to put it. A pane that never becomes
                // writable — a draft left on screen over lunch, a turn that never ends —
                // must not swallow the message in silence.
                self.hold_thread_message(
                    &session_id,
                    &thread,
                    crate::threads::PendingReason::SessionClosed,
                );
                changed = true;
            }
        }
        if changed {
            self.refresh_thread_pending();
        }
        changed
    }

    fn update_threads(&self, change: impl FnOnce(&mut crate::threads::store::ThreadState)) {
        let _ = crate::threads::store::update_in(&self.thread_state_root, change);
    }

    /// Whether a session's composer has been emptied since a notice was written into it.
    ///
    /// A pane that has gone away counts as free, or the notice would hold that session's
    /// slot forever against a thread that may well move again.
    fn composer_is_free(&self, session_id: &str) -> bool {
        self.mux
            .as_ref()
            .and_then(|mux| mux.pane_for_session(session_id).and_then(|id| mux.pane(id)))
            .is_none_or(|pane| !pane.has_draft())
    }

    /// Whether a session can be written to without corrupting what it is doing.
    fn session_ready_for_wake(&self, session_id: &str) -> bool {
        self.mux
            .as_ref()
            .and_then(|mux| mux.pane_for_session(session_id).and_then(|id| mux.pane(id)))
            .is_some_and(|pane| {
                pane.is_running()
                    && !pane.is_working()
                    && !pane.requires_user_action()
                    // An idle agent is not the same as an idle pane. Writing while the
                    // user has a half-typed message in the composer appends to it and
                    // then submits the pair, which both mangles what they wrote and
                    // sends it before they meant to.
                    && !pane.has_draft()
            })
    }

    /// Hand a session the pointer, and press Enter so it actually starts a turn.
    ///
    /// `send_prompt_snippet` pastes without submitting, which is right for a snippet the
    /// user is about to edit and wrong here — nobody is sitting in front of this pane.
    fn deliver_thread_pointer(
        &mut self,
        session_id: &str,
        thread: &crate::threads::ThreadRef,
    ) -> Result<()> {
        let pointer = crate::threads::wake_pointer(thread);
        let Some(pane_id) = self.pane_for_session(session_id) else {
            anyhow::bail!("the session is no longer open");
        };
        let Some(mux) = self.mux.as_mut() else {
            anyhow::bail!("multiplexing is disabled");
        };
        let Some(pane) = mux.pane_mut(pane_id) else {
            anyhow::bail!("the session is no longer open");
        };
        pane.send_prompt_snippet(&pointer)?;
        pane.send_key(&crossterm::event::KeyEvent::new(
            crossterm::event::KeyCode::Enter,
            crossterm::event::KeyModifiers::NONE,
        ))?;
        // Pressing Enter is not the same as the message being taken. Copilot leaves it in
        // the composer while it is busy in a way the hooks do not report — an autopilot
        // run working through background agents, say. The composer counts as occupied
        // until a turn actually starts, so the next notice waits rather than stacking.
        pane.note_injection();
        Ok(())
    }

    /// Re-read the notices from disk: what the user is asked about, and what is in hand.
    pub fn refresh_thread_pending(&mut self) {
        let notices = crate::threads::store::load_in(&self.thread_state_root).notices;
        self.thread_live_notices = notices
            .iter()
            .filter(|notice| !notice.is_waiting_for_user())
            .count();
        self.thread_pending = notices
            .into_iter()
            .filter(crate::threads::Notice::is_waiting_for_user)
            .collect();
    }

    /// What is waiting on the user for one session, if anything.
    pub fn thread_pending_for(&self, session_id: &str) -> Option<&crate::threads::Notice> {
        self.thread_pending
            .iter()
            .filter(|notice| notice.session_id == session_id)
            .min_by_key(|notice| notice.planned_at)
    }

    /// Stop trying to deliver a notice, and put it in front of the user instead.
    fn hold_thread_message(
        &mut self,
        session_id: &str,
        thread: &crate::threads::ThreadRef,
        reason: crate::threads::PendingReason,
    ) {
        self.update_threads(|state| {
            // Only ever changes a notice that is still there. If it went while this pass
            // was deciding — `cst thread leave` running in the agent's own shell — then
            // the agent has said the thread no longer concerns it, and re-creating the
            // notice would put back exactly what it asked to be rid of.
            if let Some(notice) = state.notice_mut(session_id, thread) {
                notice.status = crate::threads::NoticeStatus::Waiting { reason };
            }
        });
        self.refresh_thread_pending();
    }

    /// Open the list of messages waiting on the user.
    pub fn open_thread_inbox(&mut self) {
        self.refresh_thread_pending();
        self.thread_inbox = Some(0);
    }

    pub fn thread_inbox_move(&mut self, delta: isize) {
        let Some(selected) = self.thread_inbox else {
            return;
        };
        if self.thread_pending.is_empty() {
            return;
        }
        let last = self.thread_pending.len() - 1;
        let next = (selected as isize + delta).clamp(0, last as isize) as usize;
        self.thread_inbox = Some(next);
    }

    /// Accept the selected message: open its session if need be, and hand it the pointer.
    ///
    /// This is the only path that starts a session because of a GitHub comment, and it
    /// exists so that starting one is always something the user chose.
    pub fn thread_inbox_accept(&mut self) -> Result<()> {
        let Some(selected) = self.thread_inbox else {
            return Ok(());
        };
        let Some(pending) = self.thread_pending.get(selected).cloned() else {
            return Ok(());
        };

        if !self.has_running_pane_for(&pending.session_id) {
            let session = self
                .sessions
                .iter()
                .find(|session| session.id == pending.session_id)
                .cloned();
            let Some(session) = session else {
                anyhow::bail!("That session no longer exists");
            };
            let title = session.display_name().to_string();
            self.attach_session(&session.id, &session.cwd, title)?;
        }

        // Put back in the queue rather than written straight out: a session that has
        // only just been resumed is still starting up, and the queue already knows how
        // to wait for it. The clock restarts here, because the patience deadline should
        // measure how long *this* attempt has been waiting, not how long the message sat
        // in the inbox before the user got to it.
        self.update_threads(|state| {
            if let Some(notice) = state.notice_mut(&pending.session_id, &pending.thread) {
                notice.status = crate::threads::NoticeStatus::Planned;
                notice.planned_at = chrono::Utc::now();
            }
        });
        self.refresh_thread_pending();
        self.reselect_inbox();
        self.flush_thread_wakes();
        Ok(())
    }

    /// Drop the selected message without running anything.
    pub fn thread_inbox_dismiss(&mut self) {
        if let Some(selected) = self.thread_inbox {
            self.thread_dismiss(selected);
        }
    }

    fn thread_dismiss(&mut self, index: usize) {
        let Some(pending) = self.thread_pending.get(index).cloned() else {
            return;
        };
        self.update_threads(|state| {
            state.drop_notice(&pending.session_id, &pending.thread);
        });
        self.refresh_thread_pending();
        self.reselect_inbox();
    }

    /// Keep the inbox cursor pointing at something after a row leaves the list.
    fn reselect_inbox(&mut self) {
        if self.thread_pending.is_empty() {
            self.thread_inbox = None;
        } else if let Some(selected) = self.thread_inbox {
            self.thread_inbox = Some(selected.min(self.thread_pending.len() - 1));
        }
    }

    /// Say why the thread watcher is not working, once per distinct reason.
    pub fn report_thread_watch_failure(&mut self, reason: String) -> bool {
        if self.thread_watch_notice.as_deref() == Some(reason.as_str()) {
            return false;
        }
        self.status_message = Some(format!("Thread watching paused: {reason}"));
        self.thread_watch_notice = Some(reason);
        true
    }

    pub fn sort_label(&self) -> &str {
        match self.sort_field {
            SortField::LastUsed => "Last Used",
            SortField::Created => "Created",
            SortField::Name => "Name",
            SortField::Project => "Project",
        }
    }

    pub fn poll_update(&mut self) {
        self.poll_update_install();
        if self.update_info.is_some() && !self.update_check_requested {
            return;
        }
        let Some(rx) = self.update_receiver.as_ref() else {
            return;
        };
        match rx.try_recv() {
            Ok(Ok(result)) => {
                self.update_info = result;
                if self.update_check_requested {
                    if self.update_info.is_some() {
                        self.prepare_update_restart();
                    } else {
                        self.set_update_notice("No update available");
                    }
                }
                self.update_receiver = None;
                self.update_check_requested = false;
            }
            Ok(Err(error)) => {
                if self.update_check_requested {
                    self.set_update_notice(format!("Update check failed: {error}"));
                }
                self.update_receiver = None;
                self.update_check_requested = false;
            }
            Err(mpsc::TryRecvError::Disconnected) => {
                if self.update_check_requested {
                    self.set_update_notice("Update check failed: background worker stopped");
                }
                self.update_receiver = None;
                self.update_check_requested = false;
            }
            Err(mpsc::TryRecvError::Empty) => {}
        }
    }

    pub fn request_update(&mut self) {
        if self.confirm_update_restart {
            return;
        }
        if self.update_install_receiver.is_some() {
            self.set_update_notice("Update installation is already running...");
        } else if self.installed_update_version.is_some() || self.update_info.is_some() {
            self.prepare_update_restart();
        } else if !self.update_check_requested {
            self.update_receiver = Some(crate::updater::force_check_for_updates_async());
            self.update_check_requested = true;
            self.set_update_notice("Checking for updates...");
        } else {
            self.set_update_notice("Already checking for updates...");
        }
    }

    fn prepare_update_restart(&mut self) {
        let request = self.running_sessions_for_restart();
        if request.panes.is_empty() {
            self.begin_confirmed_update_restart(request);
        } else {
            let count = request.panes.len();
            self.confirm_update_restart = true;
            self.set_update_notice(format!(
                "Update ready; confirm restart of {count} running session{}",
                if count == 1 { "" } else { "s" }
            ));
        }
    }

    fn running_sessions_for_restart(&mut self) -> UpdateRestartRequest {
        if let Some(mux) = self.mux.as_mut() {
            mux.reap();
        }
        self.restart_resources()
    }

    fn restart_resources(&self) -> UpdateRestartRequest {
        let mut terminals: HashMap<String, (String, String, u64)> = self
            .terminal
            .running_sessions()
            .into_iter()
            .map(|(session_id, cwd, title, generation)| (session_id, (cwd, title, generation)))
            .collect();
        let mut panes = Vec::new();
        if let Some(mux) = self.mux.as_ref() {
            for pane in &mux.panes {
                let terminal = terminals.remove(&pane.session_id);
                if pane.is_running() || terminal.is_some() {
                    panes.push(UpdateRestartPane {
                        pane_id: Some(pane.id),
                        copilot_running: pane.is_running(),
                        terminal_generation: terminal.as_ref().map(|(_, _, value)| *value),
                        session_id: pane.session_id.clone(),
                        cwd: pane.cwd.clone(),
                        title: pane.title.clone(),
                    });
                }
            }
        }
        let mut terminal_only: Vec<_> = terminals.into_iter().collect();
        terminal_only.sort_by(|left, right| left.0.cmp(&right.0));
        panes.extend(
            terminal_only
                .into_iter()
                .map(|(session_id, (cwd, title, generation))| UpdateRestartPane {
                    pane_id: None,
                    copilot_running: false,
                    terminal_generation: Some(generation),
                    session_id,
                    cwd: PathBuf::from(cwd),
                    title,
                }),
        );

        let focused_session_id = self
            .mux
            .as_ref()
            .and_then(|mux| mux.focused_pane())
            .map(|pane| pane.session_id.as_str())
            .filter(|session_id| panes.iter().any(|pane| pane.session_id == *session_id))
            .or_else(|| {
                self.terminal
                    .active_session_id()
                    .filter(|session_id| panes.iter().any(|pane| pane.session_id == *session_id))
            })
            .map(str::to_string);
        UpdateRestartRequest {
            panes,
            focused_session_id,
        }
    }

    pub fn update_restart_titles(&self) -> Vec<String> {
        self.restart_resources()
            .panes
            .into_iter()
            .map(|pane| pane.title)
            .collect()
    }

    pub fn confirm_update_and_restart(&mut self) {
        if !self.confirm_update_restart {
            return;
        }
        self.confirm_update_restart = false;
        let request = self.running_sessions_for_restart();
        self.begin_confirmed_update_restart(request);
    }

    pub fn cancel_update_restart(&mut self) {
        self.confirm_update_restart = false;
        self.restart_after_update = None;
        self.update_restart_requested = false;
        self.cancel_notification_drain();
        self.set_update_notice("Update cancelled");
    }

    pub fn cancel_update_restart_after_failure(&mut self, message: impl Into<String>) {
        self.confirm_update_restart = false;
        self.restart_after_update = None;
        self.update_restart_requested = false;
        self.cancel_notification_drain();
        self.set_update_notice(message);
    }

    pub fn cancel_update_restart_for_user_action(&mut self) {
        self.confirm_update_restart = false;
        self.restart_after_update = None;
        self.update_restart_requested = false;
        self.cancel_notification_drain();
    }

    fn begin_confirmed_update_restart(&mut self, request: UpdateRestartRequest) {
        let session_count = request.panes.len();
        self.restart_after_update = Some(request);
        self.update_restart_requested = true;
        if let Some(version) = self.installed_update_version.clone() {
            self.should_quit = false;
            self.set_update_notice(format!("Restarting CST into v{version}..."));
            return;
        }
        self.begin_update_install();
        if session_count > 0 {
            self.set_update_notice(format!(
                "Installing update; {session_count} running session{} will reopen after restart...",
                if session_count == 1 { "" } else { "s" }
            ));
        }
    }

    pub fn clear_update_notice(&mut self) {
        self.update_notice = None;
    }

    fn set_update_notice(&mut self, message: impl Into<String>) {
        let message = message.into();
        self.status_message = Some(message.clone());
        self.update_notice = Some(message);
    }

    pub fn update_installing(&self) -> bool {
        self.update_install_receiver.is_some()
    }

    pub fn update_restart_ready(&self) -> bool {
        self.restart_after_update.is_some()
            && self.installed_update_version.is_some()
            && self.update_install_receiver.is_none()
    }

    pub fn update_restart_deferred_by_editor(&self) -> bool {
        self.mode != Mode::Normal
            || self.workspace_focus == WorkspaceFocus::Scratchpad
            || self.snippet_modal.is_some()
            || self.command_palette.is_some()
            || self.confirm_end_tmux.is_some()
            || self.pending_worktree.is_some()
            || self.github_inspector.as_ref().is_some_and(|inspector| {
                matches!(
                    &inspector.screen,
                    GithubInspectorScreen::NumberPrompt | GithubInspectorScreen::Choose { .. }
                )
            })
    }

    pub fn note_deferred_update_restart(&mut self) {
        self.set_update_notice(
            "Update installed; close the current editor or prompt to restart CST",
        );
    }

    pub fn validate_update_restart_sessions(&mut self) -> bool {
        if !self.update_restart_ready() {
            return false;
        }
        let current = self.running_sessions_for_restart();
        let approved_panes = self
            .restart_after_update
            .as_ref()
            .map(|request| request.panes.as_slice())
            .unwrap_or_default();
        let same_panes = current.panes.len() == approved_panes.len()
            && current
                .panes
                .iter()
                .zip(approved_panes)
                .all(|(current, approved)| {
                    current.pane_id == approved.pane_id
                        && current.copilot_running == approved.copilot_running
                        && current.terminal_generation == approved.terminal_generation
                        && current.session_id == approved.session_id
                });
        if same_panes {
            self.restart_after_update = Some(current);
            return true;
        }
        if current.panes.is_empty() {
            self.restart_after_update = Some(current);
            return true;
        }

        self.restart_after_update = None;
        self.update_restart_requested = false;
        self.confirm_update_restart = true;
        self.cancel_notification_drain();
        self.set_update_notice(
            "Running sessions changed during installation; confirm the updated restart set",
        );
        false
    }

    pub fn recover_update_restart_sessions(&mut self, reason: &str) {
        let Some(request) = self.restart_after_update.clone() else {
            self.cancel_update_restart_after_failure(format!(
                "Update installed, but sessions could not be restarted: {reason}"
            ));
            return;
        };
        let mut recovery_errors = Vec::new();
        for pane in &request.panes {
            if !pane.copilot_running {
                continue;
            }
            let still_running = self
                .mux
                .as_ref()
                .and_then(|mux| mux.pane_for_session(&pane.session_id))
                .and_then(|pane_id| self.mux.as_ref()?.pane(pane_id))
                .is_some_and(|existing| existing.is_running());
            if still_running {
                continue;
            }
            if let Err(error) = self.attach_session(
                &pane.session_id,
                &pane.cwd.to_string_lossy(),
                pane.title.clone(),
            ) {
                recovery_errors.push(format!("'{}': {error}", pane.title));
            }
        }
        if let Some(session_id) = request.focused_session_id.as_deref() {
            if let Some(pane_id) = self
                .mux
                .as_ref()
                .and_then(|mux| mux.pane_for_session(session_id))
            {
                if let Some(mux) = self.mux.as_mut() {
                    mux.focused = Some(pane_id);
                }
                self.view = View::Attached(pane_id);
            }
        }
        let message = if recovery_errors.is_empty() {
            format!(
                "Update installed, but restart was cancelled: {reason}. Stopped Copilot sessions \
                 were reopened."
            )
        } else {
            format!(
                "Update installed, but restart and session recovery were incomplete: \
                 {reason}; {}",
                recovery_errors.join("; ")
            )
        };
        self.cancel_update_restart_after_failure(message);
    }

    pub fn retain_terminated_restart_panes(&mut self, terminated: &[crate::mux::PaneId]) {
        let Some(request) = self.restart_after_update.as_mut() else {
            return;
        };
        request.panes.retain(|pane| {
            pane.copilot_running
                && pane
                    .pane_id
                    .is_some_and(|pane_id| terminated.contains(&pane_id))
        });
        if request
            .focused_session_id
            .as_ref()
            .is_some_and(|focused| !request.panes.iter().any(|pane| &pane.session_id == focused))
        {
            request.focused_session_id = request.panes.last().map(|pane| pane.session_id.clone());
        }
    }

    pub fn wait_for_update_install(&mut self) {
        let result = self.update_install_receiver.as_ref().map(|receiver| {
            receiver.recv().unwrap_or_else(|_| {
                Err("background worker stopped before reporting a result".to_string())
            })
        });
        if let Some(result) = result {
            self.apply_update_install_result(result);
        }
    }

    fn begin_update_install(&mut self) {
        let Some(info) = self.update_info.as_ref() else {
            return;
        };
        let version = info.latest_version.clone();
        #[cfg(not(test))]
        {
            self.update_install_receiver =
                Some(crate::updater::install_update_async(version.clone()));
        }
        #[cfg(test)]
        {
            self.update_install_requested_for = Some(version.clone());
        }
        self.set_update_notice(format!(
            "Installing v{version}; CST will restart automatically..."
        ));
    }

    fn poll_update_install(&mut self) {
        let Some(receiver) = self.update_install_receiver.as_ref() else {
            return;
        };
        match receiver.try_recv() {
            Ok(result) => self.apply_update_install_result(result),
            Err(mpsc::TryRecvError::Disconnected) => self.apply_update_install_result(Err(
                "background worker stopped before reporting a result".to_string(),
            )),
            Err(mpsc::TryRecvError::Empty) => {}
        }
    }

    fn apply_update_install_result(&mut self, result: UpdateInstallResult) {
        self.update_install_receiver = None;
        match result {
            Ok(UpdateInstallOutcome::Installed(version)) => {
                self.update_info = None;
                self.installed_update_version = Some(version.clone());
                if self.update_restart_requested {
                    self.set_update_notice(format!("Installed v{version}; restarting CST..."));
                } else {
                    self.set_update_notice(format!(
                        "Installed v{version}; restart CST to use the update"
                    ));
                }
            }
            Ok(UpdateInstallOutcome::AlreadyInstalled(version)) => {
                self.update_info = None;
                self.installed_update_version = Some(version.clone());
                if self.update_restart_requested {
                    self.set_update_notice(format!("v{version} is installed; restarting CST..."));
                } else {
                    self.set_update_notice(format!("v{version} is already installed"));
                }
            }
            Err(error) => {
                self.update_restart_requested = false;
                self.restart_after_update = None;
                self.set_update_notice(format!("Update installation failed: {error}"));
            }
        }
    }
}

fn extract_unique_projects(sessions: &[Session]) -> Vec<String> {
    use chrono::{DateTime, Utc};
    // Track the most recent updated_at per project (using resolved project_root)
    let mut latest: std::collections::HashMap<String, DateTime<Utc>> =
        std::collections::HashMap::new();
    for s in sessions {
        if s.project_root.is_empty() {
            continue;
        }

        if let Some(updated) = s.updated_at {
            let entry = latest.entry(s.project_root.clone()).or_insert(updated);
            if updated > *entry {
                *entry = updated;
            }
        } else {
            latest
                .entry(s.project_root.clone())
                .or_insert_with(|| DateTime::<Utc>::MIN_UTC);
        }
    }
    let mut projects: Vec<String> = latest.keys().cloned().collect();
    projects.sort_by(|a, b| latest[b].cmp(&latest[a])); // most recent first
    projects
}

fn carry_session_details(session: &mut Session, old: Session) {
    session.edited_files = old.edited_files;
    session.last_user_message = old.last_user_message;
    session.turn_count = old.turn_count;
    session.tool_call_count = old.tool_call_count;
    session.details_parsed_len = old.details_parsed_len;
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::{DateTime, Utc};

    fn app_with(mux: bool) -> App {
        let config = UserConfig {
            mux,
            ..UserConfig::default()
        };
        App::new(Vec::new(), config)
    }

    #[test]
    fn cli_override_is_not_persisted_by_settings() {
        // Simulates `--mux` on a machine whose config file says mux is off.
        let mut app = app_with(true);
        app.mux_on_disk = false;

        assert!(app.mux_enabled());
        assert!(!app.persistable_config().mux);
    }

    #[test]
    fn settings_toggle_is_persisted() {
        let mut app = app_with(false);
        app.mux_on_disk = true;
        assert!(app.persistable_config().mux);
    }

    #[test]
    fn project_snippet_save_uses_the_project_config_without_touching_global_config() {
        let project = tempfile::tempdir().unwrap();
        let mut app = app_with(false);
        let snippet = crate::config::PromptSnippet {
            name: "Repository review".to_string(),
            prompt: "Review this repository.".to_string(),
        };

        app.persist_snippets(&SnippetUpdate {
            global: Vec::new(),
            project: vec![snippet.clone()],
            original_global: Vec::new(),
            original_project: Vec::new(),
            project_root: Some(project.path().to_path_buf()),
            global_dirty: false,
            project_dirty: true,
        })
        .unwrap();

        assert!(app.config.snippets.is_empty());
        let settings = ProjectSettings::load(project.path(), &app.config).unwrap();
        assert_eq!(settings.snippets(), &[snippet]);
    }

    #[test]
    fn project_snippet_save_refuses_to_overwrite_an_external_change() {
        let project = tempfile::tempdir().unwrap();
        let original = crate::config::PromptSnippet {
            name: "Original".to_string(),
            prompt: "original".to_string(),
        };
        let external = crate::config::PromptSnippet {
            name: "External".to_string(),
            prompt: "changed elsewhere".to_string(),
        };
        let mut settings = ProjectSettings::load(project.path(), &UserConfig::default()).unwrap();
        settings.set_snippets(vec![external.clone()]);
        settings.save().unwrap();
        let mut app = app_with(false);

        let error = app
            .persist_snippets(&SnippetUpdate {
                global: Vec::new(),
                project: vec![crate::config::PromptSnippet {
                    name: "Mine".to_string(),
                    prompt: "my edit".to_string(),
                }],
                original_global: Vec::new(),
                original_project: vec![original],
                project_root: Some(project.path().to_path_buf()),
                global_dirty: false,
                project_dirty: true,
            })
            .unwrap_err()
            .to_string();

        assert!(error.contains("changed on disk"));
        let reloaded = ProjectSettings::load(project.path(), &app.config).unwrap();
        assert_eq!(reloaded.snippets(), &[external]);
    }

    #[test]
    fn adopting_fresh_global_config_keeps_runtime_mux_override() {
        let mut app = app_with(true);
        app.mux_on_disk = false;
        app.adopt_persisted_config(UserConfig {
            mux: false,
            model: Some("fresh-model".to_string()),
            snippets: vec![crate::config::PromptSnippet {
                name: "Fresh".to_string(),
                prompt: "fresh prompt".to_string(),
            }],
            ..UserConfig::default()
        });

        assert!(app.config.mux, "the live CLI override remains active");
        assert!(!app.mux_on_disk, "future saves retain the disk value");
        assert_eq!(app.config.model.as_deref(), Some("fresh-model"));
        assert_eq!(app.config.snippets[0].name, "Fresh");
    }

    #[test]
    fn live_reload_adopts_notification_changes_and_runtime_prefix() {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("config.json");
        std::fs::write(
            &path,
            serde_json::to_vec(&UserConfig {
                mux: false,
                mux_prefix: "C-a".to_string(),
                theme: ThemeName::SolarizedLight,
                notifications: crate::config::NotificationConfig {
                    enabled: true,
                    server: "https://ntfy.example.test".to_string(),
                    topic: "new_topic".to_string(),
                    ..Default::default()
                },
                ntfy_access_token: "tk_new_token".to_string(),
                ..Default::default()
            })
            .unwrap(),
        )
        .unwrap();
        let mut app = app_with(true);
        app.mux_on_disk = true;
        app.set_global_config_path(path.clone());

        assert!(app.request_config_reload());

        assert!(app.config.mux, "the running mux cannot change in place");
        assert!(!app.mux_on_disk);
        assert_eq!(
            app.mux.as_ref().unwrap().prefix.label(),
            "C-a",
            "safe runtime settings update immediately"
        );
        assert_eq!(app.config.notifications.server, "https://ntfy.example.test");
        assert_eq!(app.config.ntfy_access_token, "tk_new_token");
        assert_eq!(app.theme_name(), ThemeName::SolarizedLight);
    }

    #[test]
    fn live_reload_waits_until_the_settings_modal_closes() {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("config.json");
        std::fs::write(
            &path,
            serde_json::to_vec(&UserConfig {
                model: Some("new-model".to_string()),
                ..Default::default()
            })
            .unwrap(),
        )
        .unwrap();
        let mut app = app_with(false);
        app.set_global_config_path(path);
        app.mode = Mode::Settings;
        app.settings_input = "unsaved text".to_string();

        assert!(!app.request_config_reload());
        assert_eq!(app.settings_input, "unsaved text");
        assert!(app.config.model.is_none());

        app.mode = Mode::Normal;
        assert!(app.poll_config_reload());
        assert_eq!(app.config.model.as_deref(), Some("new-model"));
    }

    #[test]
    fn opening_settings_loads_the_revision_used_as_its_save_baseline() {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("config.json");
        std::fs::write(
            &path,
            serde_json::to_vec(&UserConfig {
                model: Some("external".to_string()),
                ..Default::default()
            })
            .unwrap(),
        )
        .unwrap();
        let mut app = app_with(false);
        app.config.model = Some("stale".to_string());
        app.set_global_config_path(path);

        app.begin_global_settings();

        assert_eq!(app.config.model.as_deref(), Some("external"));
        assert!(app.settings_config_revision.is_some());
    }

    #[test]
    fn invalid_config_cannot_be_overwritten_from_the_settings_modal() {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("config.json");
        std::fs::write(&path, "{invalid").unwrap();
        let mut app = app_with(false);
        app.set_global_config_path(path.clone());

        app.begin_global_settings();
        let error = app.save_global_settings().unwrap_err().to_string();

        assert!(error.contains("Invalid global settings"));
        assert_eq!(std::fs::read_to_string(path).unwrap(), "{invalid");
    }

    #[test]
    fn invalid_reloaded_prefix_keeps_the_working_runtime_prefix() {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("config.json");
        std::fs::write(
            &path,
            serde_json::to_vec(&UserConfig {
                mux: true,
                mux_prefix: "not-a-chord".to_string(),
                ..Default::default()
            })
            .unwrap(),
        )
        .unwrap();
        let mut app = app_with(true);
        app.set_global_config_path(path);

        assert!(app.request_config_reload());

        assert_eq!(app.mux.as_ref().unwrap().prefix.label(), "C-b");
        assert_eq!(app.config.mux_prefix, "C-b");
        assert!(app
            .status_message
            .as_deref()
            .unwrap()
            .contains("invalid mux prefix"));
    }

    #[test]
    fn invalid_external_config_keeps_the_last_known_good_runtime_values() {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("config.json");
        std::fs::write(&path, "{invalid").unwrap();
        let mut app = app_with(false);
        app.config.model = Some("known-good".to_string());
        app.set_global_config_path(path.clone());

        assert!(app.request_config_reload());

        assert_eq!(app.config.model.as_deref(), Some("known-good"));
        assert!(app
            .status_message
            .as_deref()
            .unwrap()
            .contains("Cannot reload global settings"));
        let save_error = app.save_config().unwrap_err().to_string();
        assert!(save_error.contains("no valid loaded revision"));
        assert_eq!(std::fs::read_to_string(path).unwrap(), "{invalid");
    }

    #[test]
    fn unknown_external_theme_keeps_the_last_known_good_theme() {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("config.json");
        std::fs::write(
            &path,
            serde_json::to_vec(&UserConfig {
                theme: ThemeName::Nord,
                ..Default::default()
            })
            .unwrap(),
        )
        .unwrap();
        let mut app = app_with(false);
        app.set_global_config_path(path.clone());
        assert!(app.request_config_reload());
        assert_eq!(app.theme_name(), ThemeName::Nord);

        std::fs::write(&path, r#"{"theme":"unknown-theme"}"#).unwrap();
        assert!(app.request_config_reload());

        assert_eq!(app.theme_name(), ThemeName::Nord);
        assert!(app
            .status_message
            .as_deref()
            .unwrap()
            .contains("Cannot reload global settings"));
    }

    #[test]
    fn temporarily_missing_config_keeps_the_last_known_good_runtime_values() {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("config.json");
        std::fs::write(
            &path,
            serde_json::to_vec(&UserConfig {
                model: Some("known-good".to_string()),
                ..Default::default()
            })
            .unwrap(),
        )
        .unwrap();
        let mut app = app_with(false);
        app.set_global_config_path(path.clone());
        assert!(app.request_config_reload());
        std::fs::remove_file(path).unwrap();

        assert!(!app.request_config_reload());
        assert_eq!(app.config.model.as_deref(), Some("known-good"));
        assert!(app.config_reload_pending);
    }

    #[test]
    fn first_run_missing_config_can_be_created_from_explicit_defaults() {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("config.json");
        let mut app = app_with(false);
        app.set_global_config_path(path.clone());
        assert!(!app.request_config_reload());
        app.config.model = Some("first-save".to_string());

        app.save_config().unwrap();

        let saved: UserConfig =
            serde_json::from_str(&std::fs::read_to_string(path).unwrap()).unwrap();
        assert_eq!(saved.model.as_deref(), Some("first-save"));
    }

    #[test]
    fn external_favorite_changes_resort_without_losing_selection() {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("config.json");
        std::fs::write(
            &path,
            serde_json::to_vec(&UserConfig {
                favorites: vec!["beta".to_string()],
                ..Default::default()
            })
            .unwrap(),
        )
        .unwrap();
        let mut app = App::new(
            vec![
                session("alpha", "project", "2026-08-21T10:00:00Z"),
                session("beta", "project", "2026-08-20T10:00:00Z"),
            ],
            UserConfig::default(),
        );
        app.focus_session("alpha");
        app.set_global_config_path(path);

        assert!(app.request_config_reload());

        assert_eq!(visible_ids(&app), vec!["beta", "alpha"]);
        assert_eq!(app.selected_session().unwrap().id, "alpha");
    }

    #[test]
    fn live_reload_refreshes_open_project_settings_global_defaults() {
        let temp = tempfile::tempdir().unwrap();
        let project = tempfile::tempdir().unwrap();
        let path = temp.path().join("config.json");
        std::fs::write(
            &path,
            serde_json::to_vec(&UserConfig {
                worktree: crate::config::WorktreeConfig {
                    branch_prefix: "fresh/".to_string(),
                    ..Default::default()
                },
                ..Default::default()
            })
            .unwrap(),
        )
        .unwrap();
        let mut app = app_with(false);
        app.project_settings = Some(ProjectSettings::load(project.path(), &app.config).unwrap());
        app.mode = Mode::ProjectSettings;
        app.set_global_config_path(path);

        assert!(app.request_config_reload());

        assert_eq!(
            app.project_settings
                .as_ref()
                .unwrap()
                .effective_branch_prefix(),
            "fresh/"
        );
    }

    #[test]
    fn redirected_config_reads_and_writes_the_same_file() {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("config.json");
        let mut app = app_with(false);
        app.set_global_config_path(path.clone());
        app.begin_global_settings();
        app.config.model = Some("saved-model".to_string());

        app.save_global_settings().unwrap();

        let saved: UserConfig =
            serde_json::from_str(&std::fs::read_to_string(path).unwrap()).unwrap();
        assert_eq!(saved.model.as_deref(), Some("saved-model"));
    }

    #[test]
    fn settings_save_refuses_to_overwrite_another_instances_change() {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("config.json");
        std::fs::write(
            &path,
            serde_json::to_vec(&UserConfig {
                model: Some("initial".to_string()),
                ..Default::default()
            })
            .unwrap(),
        )
        .unwrap();
        let mut app = app_with(false);
        app.set_global_config_path(path.clone());
        app.begin_global_settings();
        std::fs::write(
            &path,
            serde_json::to_vec(&UserConfig {
                model: Some("external".to_string()),
                ..Default::default()
            })
            .unwrap(),
        )
        .unwrap();
        app.config.model = Some("mine".to_string());

        let error = app.save_global_settings().unwrap_err().to_string();

        assert!(error.contains("changed in another instance"));
        assert_eq!(app.config.model.as_deref(), Some("external"));
        let saved: UserConfig =
            serde_json::from_str(&std::fs::read_to_string(path).unwrap()).unwrap();
        assert_eq!(saved.model.as_deref(), Some("external"));
    }

    #[test]
    fn theme_save_conflict_keeps_the_theme_reloaded_from_another_instance() {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("config.json");
        std::fs::write(&path, serde_json::to_vec(&UserConfig::default()).unwrap()).unwrap();
        let mut app = app_with(false);
        app.set_global_config_path(path.clone());
        app.begin_global_settings();
        app.open_theme_picker();
        app.theme_picker.as_mut().unwrap().selected = ThemeName::ALL
            .iter()
            .position(|name| *name == ThemeName::Gruvbox)
            .unwrap();
        std::fs::write(
            &path,
            serde_json::to_vec(&UserConfig {
                theme: ThemeName::Nord,
                model: Some("external".to_string()),
                ..Default::default()
            })
            .unwrap(),
        )
        .unwrap();

        assert!(app.confirm_theme_picker().is_err());

        assert_eq!(app.config.theme, ThemeName::Nord);
        assert_eq!(app.config.model.as_deref(), Some("external"));
        assert_eq!(app.theme_picker.unwrap().original, ThemeName::Nord);
        assert_eq!(app.theme_name(), ThemeName::Gruvbox);
        app.cancel_theme_picker();
        assert_eq!(app.theme_name(), ThemeName::Nord);
    }

    #[test]
    fn settings_save_updates_its_revision_before_the_watcher_echoes() {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("config.json");
        std::fs::write(&path, serde_json::to_vec(&UserConfig::default()).unwrap()).unwrap();
        let mut app = app_with(false);
        app.set_global_config_path(path);
        app.begin_global_settings();
        app.config.model = Some("mine".to_string());

        app.save_global_settings().unwrap();

        assert!(
            !app.request_config_reload(),
            "the watcher echo for this instance's own save must not look external"
        );
        assert!(!app.config_reload_pending);
    }

    #[test]
    fn notification_enqueue_refreshes_external_routing_without_restart() {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("config.json");
        std::fs::write(
            &path,
            serde_json::to_vec(&UserConfig {
                notifications: crate::config::NotificationConfig {
                    enabled: true,
                    server: "https://ntfy.example.test".to_string(),
                    topic: "fresh_topic".to_string(),
                    ..Default::default()
                },
                ntfy_access_token: "tk_fresh_token".to_string(),
                ..Default::default()
            })
            .unwrap(),
        )
        .unwrap();
        let mut app = app_with(false);
        app.set_global_config_path(path);

        app.enqueue_notification(NotificationKind::Ready, "Fresh route".to_string(), None);

        assert_eq!(app.notification_requests.len(), 1);
        assert_eq!(
            app.notification_requests[0].config.server,
            "https://ntfy.example.test"
        );
        assert_eq!(app.notification_requests[0].config.topic, "fresh_topic");
        assert_eq!(app.notification_requests[0].access_token, "tk_fresh_token");
    }

    #[test]
    fn notification_enqueue_honors_external_revocation_while_settings_are_open() {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("config.json");
        std::fs::write(&path, serde_json::to_vec(&UserConfig::default()).unwrap()).unwrap();
        let mut app = app_with(false);
        app.set_global_config_path(path);
        app.config.notifications.enabled = true;
        app.config.notifications.topic = "stale_topic".to_string();
        app.mode = Mode::Settings;

        app.enqueue_notification(NotificationKind::Ready, "Revoked".to_string(), None);

        assert!(
            app.notification_requests.is_empty(),
            "the current disk snapshot disabled notifications"
        );
    }

    #[test]
    fn pane_list_reports_when_there_is_nothing_to_switch_to() {
        let mut app = app_with(true);
        app.open_pane_list();
        assert_eq!(app.mode, Mode::Normal);
        assert!(app.status_message.is_some());
    }

    #[test]
    fn pane_list_is_inert_without_the_multiplexer() {
        let mut app = app_with(false);
        app.open_pane_list();
        assert_eq!(app.mode, Mode::Normal);
        assert!(app.status_message.is_none());
    }

    #[test]
    fn panel_preferences_restore_for_a_new_pane_after_restart() {
        let temp = tempfile::tempdir().unwrap();
        let session_id = "workspace-restart-session";

        let mut first_run = app_with(true);
        first_run.set_workspace_state_root(temp.path().to_path_buf());
        first_run.remember_scratchpad_panel(1, session_id, true);
        first_run.remember_terminal_panel(1, session_id, true);

        let mut restarted = app_with(true);
        restarted.set_workspace_state_root(temp.path().to_path_buf());
        restarted.restore_workspace_panels(42, session_id);

        assert!(restarted.scratchpad_open.contains(&42));
        assert!(restarted.terminal_open.contains(&42));
        assert!(!restarted.scratchpad_open.contains(&1));
        assert!(!restarted.terminal_open.contains(&1));
    }

    #[test]
    fn stopped_attached_terminal_collapses_and_clears_persisted_open_state() {
        let directory = tempfile::tempdir().unwrap();
        let state_root = directory.path().join("state");
        let session_id = "terminal-exit-session";
        let mut app = app_with(true);
        app.set_workspace_state_root(state_root.clone());

        let (program, args) = if cfg!(windows) {
            (
                "cmd.exe".to_string(),
                vec!["/c".to_string(), "ping -n 30 127.0.0.1 >nul".to_string()],
            )
        } else {
            (
                "/bin/sh".to_string(),
                vec!["-c".to_string(), "sleep 30".to_string()],
            )
        };
        let events = app.mux.as_ref().unwrap().events.clone();
        let pane = crate::mux::Pane::spawn(
            crate::mux::PaneSpec {
                id: 42,
                title: "Session".to_string(),
                cwd: directory.path().to_path_buf(),
                session_id: session_id.to_string(),
                program,
                args,
                events_path: None,
                terminal_light_mode: Some(false),
                hooks_active: false,
            },
            24,
            80,
            events,
        )
        .unwrap();
        app.mux.as_mut().unwrap().push(pane);
        app.view = View::Attached(42);

        app.terminal
            .activate(
                session_id.to_string(),
                "Terminal".to_string(),
                directory.path().to_string_lossy().to_string(),
                &app.config.terminal,
            )
            .unwrap();
        app.terminal_owner = Some(42);
        app.remember_terminal_panel(42, session_id, true);
        app.workspace_focus = WorkspaceFocus::Terminal;
        app.terminal.exit_active_for_test().unwrap();

        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(3);
        while std::time::Instant::now() < deadline && app.terminal.stopped_session_ids().is_empty()
        {
            std::thread::sleep(std::time::Duration::from_millis(20));
        }
        app.collapse_stopped_terminals();

        assert!(!app.terminal.is_visible());
        assert_eq!(app.terminal_owner, None);
        assert!(!app.terminal_open.contains(&42));
        assert_eq!(app.workspace_focus, WorkspaceFocus::Chat);
        assert!(
            !crate::workspace_state::load_in(&state_root, session_id)
                .unwrap()
                .terminal_open
        );
        assert_eq!(
            app.terminal
                .activate(
                    session_id.to_string(),
                    "Terminal".to_string(),
                    directory.path().to_string_lossy().to_string(),
                    &app.config.terminal,
                )
                .unwrap(),
            crate::terminal_pane::Activation::Restarted
        );

        let _ = app.terminal.shutdown();
        let _ = app.mux.as_mut().unwrap().shutdown();
    }

    fn session(id: &str, project: &str, updated_at: &str) -> Session {
        Session {
            id: id.to_string(),
            cwd: project.to_string(),
            project_root: project.to_string(),
            summary: Some(format!("Session {id}")),
            created_at: None,
            updated_at: Some(
                DateTime::parse_from_rfc3339(updated_at)
                    .unwrap()
                    .with_timezone(&Utc),
            ),
            is_active: false,
            dir_path: PathBuf::from(id),
            edited_files: Vec::new(),
            last_user_message: None,
            turn_count: 0,
            tool_call_count: 0,
            details_parsed_len: 0,
        }
    }

    fn visible_ids(app: &App) -> Vec<&str> {
        app.filtered_indices
            .iter()
            .map(|&index| app.sessions[index].id.as_str())
            .collect()
    }

    #[test]
    fn refreshing_sessions_adds_new_metadata_without_losing_cached_details_or_selection() {
        let mut existing = session("existing", "project-a", "2026-08-21T10:00:00Z");
        existing.edited_files = vec!["src/main.rs".to_string()];
        existing.last_user_message = Some("keep me".to_string());
        existing.turn_count = 7;
        existing.tool_call_count = 11;
        existing.details_parsed_len = 1234;
        let mut app = App::new(vec![existing], UserConfig::default());

        let renamed = Session {
            summary: Some("Current persisted name".to_string()),
            ..session("existing", "project-a", "2026-08-21T11:00:00Z")
        };
        let new = Session {
            summary: Some("Brand new session".to_string()),
            ..session("new", "project-b", "2026-08-21T12:00:00Z")
        };
        app.merge_session_metadata(vec![renamed, new]);

        assert_eq!(app.selected_session().unwrap().id, "existing");
        let existing = app
            .sessions
            .iter()
            .find(|session| session.id == "existing")
            .unwrap();
        assert_eq!(existing.display_name(), "Current persisted name");
        assert_eq!(existing.edited_files, ["src/main.rs"]);
        assert_eq!(existing.last_user_message.as_deref(), Some("keep me"));
        assert_eq!(existing.turn_count, 7);
        assert_eq!(existing.tool_call_count, 11);
        assert_eq!(existing.details_parsed_len, 1234);
        assert!(app.sessions.iter().any(|session| session.id == "new"));
        assert!(app
            .unique_projects
            .iter()
            .any(|project| project == "project-b"));
    }

    #[test]
    fn externally_launched_tmux_session_is_added_and_selected_when_metadata_appears() {
        let home = tempfile::tempdir().unwrap();
        let session_dir = home.path().join("session-state").join("external-session");
        std::fs::create_dir_all(&session_dir).unwrap();
        std::fs::write(
            session_dir.join("workspace.yaml"),
            format!(
                "id: external-session\ncwd: {}\nname: External tmux session\n",
                home.path().display()
            ),
        )
        .unwrap();

        let mut app = App::new(Vec::new(), UserConfig::default());
        app.copilot_home = home.path().to_path_buf();
        app.track_external_session("external-session".to_string());
        app.poll_external_sessions();

        assert!(app.pending_external_sessions.is_empty());
        assert_eq!(app.selected_session().unwrap().id, "external-session");
        assert_eq!(
            app.selected_session().unwrap().display_name(),
            "External tmux session"
        );
    }

    #[test]
    fn pane_switcher_prefers_the_refreshed_session_name_to_an_old_window_title() {
        let persisted = Session {
            summary: Some("EmbViz".to_string()),
            ..session("new-session", "project", "2026-08-21T12:00:00Z")
        };
        let app = App::new(vec![persisted], UserConfig::default());

        assert_eq!(
            app.pane_session_title(
                "new-session",
                "Create Embedding Space Visualizations - GitHub Copilot"
            ),
            "EmbViz"
        );
        assert_eq!(
            app.pane_session_title("not-on-disk-yet", "Starting Copilot"),
            "Starting Copilot"
        );
    }

    #[test]
    fn a_session_created_after_startup_is_discovered_from_disk() {
        let home = tempfile::tempdir().unwrap();
        let session_dir = home.path().join("session-state").join("fresh-session");
        std::fs::create_dir_all(&session_dir).unwrap();
        std::fs::write(
            session_dir.join("workspace.yaml"),
            format!(
                "id: fresh-session\ncwd: {}\nname: Fresh name\n\
                 created_at: 2026-08-21T12:00:00Z\nupdated_at: 2026-08-21T12:00:01Z\n",
                home.path().display()
            ),
        )
        .unwrap();

        let mut app = App::new(Vec::new(), UserConfig::default());
        app.copilot_home = home.path().to_path_buf();
        let fresh = crate::session::loader::load_session(&app.copilot_home, "fresh-session")
            .unwrap()
            .expect("new session metadata");
        app.merge_session_metadata(vec![fresh]);

        assert_eq!(app.sessions.len(), 1);
        assert_eq!(app.sessions[0].id, "fresh-session");
        assert_eq!(app.sessions[0].display_name(), "Fresh name");
    }

    #[test]
    fn background_catalog_completion_keeps_selection_details_and_concurrent_sessions() {
        let mut favorite = session("favorite", "project-a", "2026-08-21T10:00:00Z");
        favorite.summary = Some("Current favorite name".to_string());
        favorite.turn_count = 9;
        favorite.details_parsed_len = 456;
        let concurrent = session("created-during-scan", "project-c", "2026-08-21T12:30:00Z");
        let mut app = App::new(vec![favorite, concurrent], UserConfig::default());
        app.focus_session("favorite");

        let loaded_favorite = Session {
            summary: Some("Stale worker name".to_string()),
            ..session("favorite", "project-a", "2026-08-21T12:00:00Z")
        };
        let temp = tempfile::tempdir().unwrap();
        let historical_dir = temp.path().join("historical");
        std::fs::create_dir(&historical_dir).unwrap();
        std::fs::write(historical_dir.join("workspace.yaml"), "id: historical\n").unwrap();
        let historical = Session {
            dir_path: historical_dir,
            ..session("historical", "project-b", "2026-08-20T12:00:00Z")
        };
        let (sender, receiver) = mpsc::channel();
        app.begin_session_load(receiver);
        assert!(app.sessions_loading());
        sender.send(Ok(vec![loaded_favorite, historical])).unwrap();

        app.poll_session_load();

        assert!(!app.sessions_loading());
        assert_eq!(app.selected_session().unwrap().id, "favorite");
        let favorite = app
            .sessions
            .iter()
            .find(|session| session.id == "favorite")
            .unwrap();
        assert_eq!(favorite.display_name(), "Current favorite name");
        assert_eq!(favorite.turn_count, 9);
        assert_eq!(favorite.details_parsed_len, 456);
        assert!(app
            .sessions
            .iter()
            .any(|session| session.id == "historical"));
        assert!(
            app.sessions
                .iter()
                .any(|session| session.id == "created-during-scan"),
            "a session created after the worker's directory snapshot must survive"
        );
    }

    #[test]
    fn background_catalog_does_not_resurrect_deleted_sessions() {
        let temp = tempfile::tempdir().unwrap();
        let deleted = Session {
            dir_path: temp.path().join("already-deleted"),
            ..session("deleted", "project", "2026-08-21T12:00:00Z")
        };
        let mut app = App::new(Vec::new(), UserConfig::default());

        app.apply_loaded_catalog(vec![deleted]);

        assert!(app.sessions.is_empty());
    }

    #[test]
    fn background_catalog_keeps_a_launch_project_with_no_sessions() {
        let temp = tempfile::tempdir().unwrap();
        std::fs::create_dir(temp.path().join(".git")).unwrap();
        let mut app = App::new(Vec::new(), UserConfig::default());
        app.set_cwd_context(temp.path().to_string_lossy().to_string(), false);
        let launch_project = app.cwd_project.clone().unwrap();

        app.apply_loaded_catalog(Vec::new());

        assert!(app
            .unique_projects
            .iter()
            .any(|project| project.eq_ignore_ascii_case(&launch_project)));
    }

    #[test]
    fn favorites_lead_only_in_fully_unfiltered_view() {
        let sessions = vec![
            session("newest", "project-a", "2026-08-14T12:00:00Z"),
            session("favorite", "project-a", "2026-08-13T12:00:00Z"),
            session("oldest", "project-b", "2026-08-12T12:00:00Z"),
        ];
        let mut config = UserConfig::default();
        config.favorites.push("favorite".to_string());
        let mut app = App::new(sessions, config);

        assert_eq!(visible_ids(&app), vec!["favorite", "newest", "oldest"]);

        app.set_project_filter(Some("project-a".to_string()));
        assert_eq!(visible_ids(&app), vec!["newest", "favorite"]);

        app.set_project_filter(None);
        app.search_query = "Session".to_string();
        app.apply_filter();
        assert_eq!(visible_ids(&app), vec!["newest", "favorite", "oldest"]);
    }

    #[test]
    fn search_filters_favorites_without_crossing_metadata_boundaries() {
        let project = "D:/Workspace/SpeakingBigMapsIntoExistence";
        let mut intended = session("intended-deadbeef", project, "2026-08-14T12:00:00Z");
        intended.summary = Some("Standup Cleanup".to_string());
        let mut unrelated = session("unrelated-deadbeef", project, "2026-08-13T12:00:00Z");
        // "stand" can be assembled across "Start Benchmark" + UUID when every
        // field is concatenated, but no individual field actually matches.
        unrelated.summary = Some("Start Benchmark".to_string());
        let config = UserConfig {
            favorites: vec![intended.id.clone(), unrelated.id.clone()],
            ..UserConfig::default()
        };
        let mut app = App::new(vec![intended, unrelated], config);

        app.search_query = "stand".to_string();
        app.apply_filter();

        assert_eq!(visible_ids(&app), vec!["intended-deadbeef"]);
        assert!(!app.favorites_section_active(), "search results are flat");
    }

    #[test]
    fn search_can_still_match_each_metadata_field() {
        let project = "D:/Workspace/SpeakingBigMapsIntoExistence";
        let item = session("abc1234", project, "2026-08-14T12:00:00Z");
        let mut app = App::new(vec![item], UserConfig::default());

        for query in ["Session", "SpeakingBig", "Workspace", "abc1234"] {
            app.search_query = query.to_string();
            app.apply_filter();
            assert_eq!(visible_ids(&app), vec!["abc1234"], "query {query:?}");
        }
    }

    #[test]
    fn hidden_title_prefixes_filter_sessions_and_can_be_temporarily_shown() {
        let mut hidden = session("hidden", "project", "2026-08-14T12:00:00Z");
        hidden.summary = Some("Your objective: investigate the issue".to_string());
        let mut differently_cased = session("hidden-case", "project", "2026-08-13T12:00:00Z");
        differently_cased.summary = Some("YOUR OBJECTIVE: test another path".to_string());
        let visible = session("visible", "project", "2026-08-12T12:00:00Z");
        let config = UserConfig {
            hidden_title_prefixes: vec!["Your objective:".to_string()],
            ..UserConfig::default()
        };
        let mut app = App::new(vec![hidden, differently_cased, visible], config);

        assert_eq!(visible_ids(&app), vec!["visible"]);

        assert!(app.toggle_hidden_sessions());
        assert_eq!(visible_ids(&app), vec!["hidden", "hidden-case", "visible"]);

        assert!(!app.toggle_hidden_sessions());
        assert_eq!(visible_ids(&app), vec!["visible"]);
    }

    #[test]
    fn hidden_path_prefixes_match_path_components() {
        let in_tmp = Session {
            cwd: "/tmp/cst-work/project".to_string(),
            ..session("tmp-session", "project", "2026-08-14T12:00:00Z")
        };
        let tmp_exact = Session {
            cwd: "/tmp".to_string(),
            ..session("tmp-exact", "project", "2026-08-13T12:00:00Z")
        };
        let similar = Session {
            cwd: "/tmp2/project".to_string(),
            ..session("tmp2-session", "project", "2026-08-12T12:00:00Z")
        };
        let config = UserConfig {
            hidden_path_prefixes: vec!["/tmp".to_string()],
            ..UserConfig::default()
        };
        let mut app = App::new(vec![in_tmp, tmp_exact, similar], config);

        assert_eq!(visible_ids(&app), vec!["tmp2-session"]);

        app.toggle_hidden_sessions();
        assert_eq!(
            visible_ids(&app),
            vec!["tmp-session", "tmp-exact", "tmp2-session"]
        );
    }

    #[test]
    fn favorites_keep_their_arranged_order_regardless_of_sort() {
        let sessions = vec![
            session("zebra", "project", "2026-08-14T12:00:00Z"),
            session("beta", "project", "2026-08-13T12:00:00Z"),
            session("alpha", "project", "2026-08-12T12:00:00Z"),
        ];
        let mut config = UserConfig::default();
        // Deliberately not alphabetical: the arrangement is the user's, and a sort
        // change must not quietly rewrite it.
        config.favorites.push("beta".to_string());
        config.favorites.push("alpha".to_string());
        let mut app = App::new(sessions, config);

        assert_eq!(visible_ids(&app), vec!["beta", "alpha", "zebra"]);

        app.cycle_sort();
        app.cycle_sort();
        assert_eq!(visible_ids(&app), vec!["beta", "alpha", "zebra"]);
    }

    /// Three favorites arranged `beta, alpha, zebra`, with the cursor on `alpha`.
    fn reorderable_app() -> App {
        let sessions = vec![
            session("zebra", "project", "2026-08-14T12:00:00Z"),
            session("beta", "project", "2026-08-13T12:00:00Z"),
            session("alpha", "project", "2026-08-12T12:00:00Z"),
        ];
        let mut config = UserConfig::default();
        for id in ["beta", "alpha", "zebra"] {
            config.favorites.push(id.to_string());
        }
        let mut app = App::new(sessions, config);
        app.disable_config_persistence();
        app.selected = 1;
        app
    }

    #[test]
    fn grabbing_a_favorite_and_moving_it_rewrites_the_order() {
        let mut app = reorderable_app();

        app.toggle_favorite_grab();
        assert_eq!(app.grabbed_favorite.as_deref(), Some("alpha"));

        app.move_grabbed_favorite(-1);

        assert_eq!(app.config.favorites, vec!["alpha", "beta", "zebra"]);
        assert_eq!(visible_ids(&app), vec!["alpha", "beta", "zebra"]);
    }

    #[test]
    fn opening_command_palette_releases_an_active_favorite_grab() {
        let mut app = reorderable_app();
        app.toggle_favorite_grab();
        assert!(app.grabbed_favorite.is_some());

        app.open_command_palette();

        assert!(app.grabbed_favorite.is_none());
        assert!(app.command_palette.is_some());
    }

    #[test]
    fn reorder_favorite_palette_command_is_disabled_while_filtering() {
        let mut app = reorderable_app();
        app.search_query = "alpha".to_string();
        app.apply_filter();
        app.open_command_palette();

        let reorder = crate::command_palette::filtered_commands(&app)
            .into_iter()
            .find(|command| command.id == crate::command_palette::CommandId::ReorderFavorite)
            .unwrap();

        assert!(!reorder.enabled);
        assert!(reorder
            .unavailable_reason
            .is_some_and(|reason| reason.contains("unfiltered favorites")));
    }

    #[test]
    fn the_cursor_follows_the_item_being_moved() {
        let mut app = reorderable_app();
        app.toggle_favorite_grab();

        app.move_grabbed_favorite(-1);

        // Losing the cursor mid-move would make a second press move a different row.
        assert_eq!(app.selected, 0);
        assert_eq!(
            app.selected_session().map(|session| session.id.as_str()),
            Some("alpha")
        );
    }

    #[test]
    fn moving_stops_at_the_ends_instead_of_wrapping() {
        let mut app = reorderable_app();
        app.selected = 0;
        app.toggle_favorite_grab();

        app.move_grabbed_favorite(-1);
        assert_eq!(app.config.favorites, vec!["beta", "alpha", "zebra"]);

        app.selected = 2;
        app.release_favorite_grab();
        app.toggle_favorite_grab();
        app.move_grabbed_favorite(1);
        assert_eq!(app.config.favorites, vec!["beta", "alpha", "zebra"]);
    }

    #[test]
    fn a_favorite_cannot_be_moved_below_the_ordinary_sessions() {
        let sessions = vec![
            session("plain", "project", "2026-08-14T12:00:00Z"),
            session("beta", "project", "2026-08-13T12:00:00Z"),
            session("alpha", "project", "2026-08-12T12:00:00Z"),
        ];
        let config = UserConfig {
            favorites: vec!["beta".to_string(), "alpha".to_string()],
            ..Default::default()
        };
        let mut app = App::new(sessions, config);
        app.disable_config_persistence();
        // The last favorite, sitting directly above the ordinary sessions.
        app.selected = 1;
        app.toggle_favorite_grab();

        app.move_grabbed_favorite(1);

        // Moving down must not push it out of the group or drag `plain` in.
        assert_eq!(app.config.favorites, vec!["beta", "alpha"]);
        assert_eq!(visible_ids(&app), vec!["beta", "alpha", "plain"]);
    }

    #[test]
    fn moving_steps_over_favorites_whose_sessions_are_gone() {
        let sessions = vec![
            session("beta", "project", "2026-08-13T12:00:00Z"),
            session("alpha", "project", "2026-08-12T12:00:00Z"),
        ];
        // `ghost` is a favorite whose session directory no longer exists, so it is
        // kept in the config but never listed.
        let config = UserConfig {
            favorites: vec!["beta".to_string(), "ghost".to_string(), "alpha".to_string()],
            ..Default::default()
        };
        let mut app = App::new(sessions, config);
        app.disable_config_persistence();
        app.selected = 1;
        app.toggle_favorite_grab();

        app.move_grabbed_favorite(-1);

        // One press moves one *visible* row; swapping with `ghost` would look stuck.
        assert_eq!(visible_ids(&app), vec!["alpha", "beta"]);
        assert_eq!(app.config.favorites, vec!["alpha", "beta", "ghost"]);
    }

    #[test]
    fn only_favorites_can_be_grabbed() {
        let sessions = vec![
            session("plain", "project", "2026-08-14T12:00:00Z"),
            session("starred", "project", "2026-08-13T12:00:00Z"),
        ];
        let mut config = UserConfig::default();
        config.favorites.push("starred".to_string());
        let mut app = App::new(sessions, config);
        app.disable_config_persistence();
        // Row 1 is the unstarred session, since the favorite leads the list.
        app.selected = 1;

        let message = app.toggle_favorite_grab();

        assert!(app.grabbed_favorite.is_none());
        assert!(message.is_some_and(|text| text.contains("Only favorites")));
    }

    #[test]
    fn reordering_is_refused_while_the_list_is_filtered() {
        let mut app = reorderable_app();
        app.search_query = "session".to_string();
        app.apply_filter();

        let message = app.toggle_favorite_grab();

        // The group is not shown while filtering, so moving within it would be
        // rearranging something the user cannot see.
        assert!(app.grabbed_favorite.is_none());
        assert!(message.is_some_and(|text| text.contains("Clear the filter")));
    }

    #[test]
    fn newly_starred_sessions_are_appended_rather_than_sorted_in() {
        let mut app = reorderable_app();
        app.config.favorites = vec!["beta".to_string()];
        app.apply_filter();
        app.selected = visible_ids(&app)
            .iter()
            .position(|&id| id == "alpha")
            .expect("alpha is listed");

        app.toggle_selected_favorite().expect("save is disabled");

        assert_eq!(app.config.favorites, vec!["beta", "alpha"]);
    }

    #[test]
    fn headers_are_only_budgeted_when_a_favorites_group_is_shown() {
        let mut app = reorderable_app();
        // Every session is a favorite here, so there is no trailing group to label.
        assert_eq!(app.list_header_lines(), 1);

        app.config.favorites = vec!["beta".to_string()];
        app.apply_filter();
        assert_eq!(app.list_header_lines(), 2);

        app.config.favorites.clear();
        app.apply_filter();
        assert_eq!(app.list_header_lines(), 0);
    }

    /// A pull request with one changed file and no patch yet, as the fast path
    /// produces it.
    fn cached_pull(number: u64, updated_at: &str) -> GithubItem {
        use crate::github::{Author, ChangedFile, ItemCommon, PullRequest, RepositoryRef};
        GithubItem::PullRequest(PullRequest {
            common: ItemCommon {
                repository: RepositoryRef {
                    host: "github.com".to_string(),
                    owner: "octo".to_string(),
                    name: "widgets".to_string(),
                },
                number,
                title: format!("Change {number}"),
                state: "open".to_string(),
                author: Author {
                    login: "monalisa".to_string(),
                },
                labels: Vec::new(),
                created_at: "2026-01-01T00:00:00Z".to_string(),
                updated_at: updated_at.to_string(),
                url: String::new(),
                body: String::new(),
            },
            draft: false,
            merged: false,
            mergeable_state: None,
            base_ref: "main".to_string(),
            head_ref: "feature".to_string(),
            additions: 1,
            deletions: 0,
            changed_files: 1,
            discussion: Vec::new(),
            files: vec![ChangedFile {
                path: "src/lib.rs".to_string(),
                status: "modified".to_string(),
                additions: 1,
                deletions: 0,
                changes: 1,
                patch: None,
            }],
            patches_loaded: false,
        })
    }

    fn cached_discussion(number: u64, updated_at: &str) -> GithubItem {
        use crate::github::{Author, ItemCommon, RepositoryDiscussion, RepositoryRef};
        GithubItem::Discussion(RepositoryDiscussion {
            common: ItemCommon {
                repository: RepositoryRef {
                    host: "github.com".to_string(),
                    owner: "octo".to_string(),
                    name: "widgets".to_string(),
                },
                number,
                title: format!("Discussion {number}"),
                state: "open".to_string(),
                author: Author {
                    login: "monalisa".to_string(),
                },
                labels: Vec::new(),
                created_at: "2026-01-01T00:00:00Z".to_string(),
                updated_at: updated_at.to_string(),
                url: String::new(),
                body: String::new(),
            },
            category: "General".to_string(),
            answerable: false,
            answered: false,
            answer_chosen_at: None,
            upvote_count: 0,
            reactions: Vec::new(),
            comments: Vec::new(),
        })
    }

    #[test]
    fn ambiguous_number_is_cached_by_kind_and_can_choose_discussion() {
        let mut app = App::new(Vec::new(), UserConfig::default());
        let cwd = PathBuf::from("C:/Workspace/widgets");
        app.github_repositories
            .insert(cwd.clone(), Some(repository()));
        app.github_inspector = Some(GithubInspector::number_prompt());
        {
            let inspector = app.github_inspector.as_mut().unwrap();
            inspector.request_id = 42;
            inspector.request_cwd = Some(cwd.clone());
            inspector.number = Some(7);
        }
        app.apply_github_result(GithubLoadResult {
            request_id: 42,
            result: Ok(crate::github::FetchedItem {
                repository: repository(),
                item: GithubItem::Ambiguous {
                    issue_or_pull_request: Box::new(cached_pull(7, "2026-01-02T00:00:00Z")),
                    discussion: Box::new(cached_discussion(7, "2026-01-03T00:00:00Z")),
                },
                discussion_checked: true,
            }),
            lookup_kind: crate::github::GithubLookupKind::Auto,
            revalidation: false,
            reference_generation: None,
        });

        assert!(matches!(
            app.github_inspector.as_ref().unwrap().screen,
            GithubInspectorScreen::Choose { .. }
        ));
        assert!(app
            .cached_github_item(&cwd, 7, crate::github::GithubLookupKind::IssueOrPullRequest)
            .is_some());
        assert!(app
            .cached_github_item(&cwd, 7, crate::github::GithubLookupKind::Discussion)
            .is_some());

        app.choose_github_item(crate::github::GithubLookupKind::Discussion);
        assert!(matches!(
            app.github_inspector.as_ref().unwrap().ready_item().unwrap(),
            GithubItem::Discussion(_)
        ));
    }

    #[test]
    fn ambiguous_revalidation_replaces_an_auto_cached_candidate_with_chooser() {
        let mut app = App::new(Vec::new(), UserConfig::default());
        let cwd = PathBuf::from("C:/Workspace/widgets");
        app.github_repositories
            .insert(cwd.clone(), Some(repository()));
        let pull = cached_pull(7, "2026-01-02T00:00:00Z");
        app.store_github_item(repository(), pull.clone());
        app.github_inspector = Some(GithubInspector::number_prompt());
        {
            let inspector = app.github_inspector.as_mut().unwrap();
            inspector.request_id = 42;
            inspector.request_cwd = Some(cwd);
            inspector.number = Some(7);
            inspector.lookup_kind = crate::github::GithubLookupKind::Auto;
            inspector.screen = GithubInspectorScreen::Ready(pull.clone());
        }

        app.apply_github_result(GithubLoadResult {
            request_id: 42,
            result: Ok(crate::github::FetchedItem {
                repository: repository(),
                item: GithubItem::Ambiguous {
                    issue_or_pull_request: Box::new(pull),
                    discussion: Box::new(cached_discussion(7, "2026-01-03T00:00:00Z")),
                },
                discussion_checked: true,
            }),
            lookup_kind: crate::github::GithubLookupKind::Auto,
            revalidation: true,
            reference_generation: None,
        });

        assert!(matches!(
            app.github_inspector.as_ref().unwrap().screen,
            GithubInspectorScreen::Choose { .. }
        ));
    }

    #[test]
    fn copilot_review_results_update_only_the_pull_request_that_requested_them() {
        let mut app = App::new(Vec::new(), UserConfig::default());
        let mut inspector = GithubInspector::number_prompt();
        inspector.request_id = 42;
        inspector.screen = GithubInspectorScreen::Ready(cached_pull(7, "2026-01-02T00:00:00Z"));
        inspector.copilot_review_status = CopilotReviewStatus::Requesting;
        app.github_inspector = Some(inspector);

        app.apply_copilot_review_result(GithubReviewResult {
            request_id: 41,
            result: Ok(()),
        });
        assert_eq!(
            app.github_inspector.as_ref().unwrap().copilot_review_status,
            CopilotReviewStatus::Requesting
        );

        app.apply_copilot_review_result(GithubReviewResult {
            request_id: 42,
            result: Ok(()),
        });
        assert_eq!(
            app.github_inspector.as_ref().unwrap().copilot_review_status,
            CopilotReviewStatus::Requested
        );
    }

    #[test]
    fn copilot_review_failure_remains_visible_for_retry() {
        let mut app = App::new(Vec::new(), UserConfig::default());
        let mut inspector = GithubInspector::number_prompt();
        inspector.request_id = 42;
        inspector.screen = GithubInspectorScreen::Ready(cached_pull(7, "2026-01-02T00:00:00Z"));
        inspector.copilot_review_status = CopilotReviewStatus::Requesting;
        app.github_inspector = Some(inspector);

        app.apply_copilot_review_result(GithubReviewResult {
            request_id: 42,
            result: Err(GithubError {
                kind: crate::github::GithubErrorKind::Cli,
                message: "review unavailable".to_string(),
            }),
        });

        assert_eq!(
            app.github_inspector.as_ref().unwrap().copilot_review_status,
            CopilotReviewStatus::Failed("review unavailable".to_string())
        );
    }

    #[test]
    fn pull_request_comment_filter_cycles_and_returns_to_the_top() {
        let mut inspector = GithubInspector::number_prompt();
        inspector.screen = GithubInspectorScreen::Ready(cached_pull(7, "2026-01-02T00:00:00Z"));
        inspector.tab = GithubTab::Comments;
        inspector.scroll_offsets[GithubTab::Comments.index()] = 12;
        inspector.max_scroll = 30;

        inspector.cycle_comment_filter();
        assert_eq!(inspector.comment_filter, GithubCommentFilter::Unresolved);
        assert_eq!(inspector.active_scroll(), 0);
        assert_eq!(inspector.max_scroll, 0);

        inspector.cycle_comment_filter();
        assert_eq!(inspector.comment_filter, GithubCommentFilter::Resolved);
        inspector.cycle_comment_filter();
        assert_eq!(inspector.comment_filter, GithubCommentFilter::All);
    }

    #[test]
    fn navigation_resets_preserve_the_inspectors_configured_comment_filter() {
        let mut inspector =
            GithubInspector::number_prompt_with_filter(GithubCommentFilter::Resolved);
        inspector.tab = GithubTab::Comments;
        inspector.scroll_offsets[GithubTab::Comments.index()] = 12;

        inspector.reset_navigation();

        assert_eq!(inspector.comment_filter, GithubCommentFilter::Resolved);
        assert_eq!(inspector.tab, GithubTab::Overview);
        assert_eq!(inspector.active_scroll(), 0);
    }

    #[test]
    fn issue_only_rest_revalidation_cannot_erase_known_collision() {
        let mut app = App::new(Vec::new(), UserConfig::default());
        let cwd = PathBuf::from("C:/Workspace/widgets");
        app.github_repositories
            .insert(cwd.clone(), Some(repository()));
        app.github_inspector = Some(GithubInspector::number_prompt());
        {
            let inspector = app.github_inspector.as_mut().unwrap();
            inspector.request_id = 42;
            inspector.request_cwd = Some(cwd);
            inspector.number = Some(7);
            inspector.lookup_kind = crate::github::GithubLookupKind::Auto;
            inspector.screen = GithubInspectorScreen::Choose {
                issue_or_pull_request: Box::new(cached_pull(7, "2026-01-02T00:00:00Z")),
                discussion: Box::new(cached_discussion(7, "2026-01-03T00:00:00Z")),
            };
        }

        app.apply_github_result(GithubLoadResult {
            request_id: 42,
            result: Ok(crate::github::FetchedItem {
                repository: repository(),
                item: cached_pull(7, "2026-01-04T00:00:00Z"),
                discussion_checked: false,
            }),
            lookup_kind: crate::github::GithubLookupKind::Auto,
            revalidation: true,
            reference_generation: None,
        });

        assert!(matches!(
            app.github_inspector.as_ref().unwrap().screen,
            GithubInspectorScreen::Choose { .. }
        ));
    }

    #[test]
    fn explicit_discussion_fetch_preserves_known_ambiguous_reference_kind() {
        use crate::github::{ReferenceKind, ReferenceState, ReferenceStatus};

        let mut app = App::new(Vec::new(), UserConfig::default());
        let repository = repository();
        app.github_references.insert(
            (repository.clone(), 7),
            Some(ReferenceStatus {
                kind: ReferenceKind::Ambiguous,
                state: ReferenceState::Open,
            }),
        );

        app.store_fetched_github_item(
            repository.clone(),
            cached_discussion(7, "2026-01-03T00:00:00Z"),
            crate::github::GithubLookupKind::Discussion,
            None,
        );

        assert_eq!(
            app.github_references[&(repository, 7)].unwrap().kind,
            ReferenceKind::Ambiguous
        );
    }

    #[test]
    fn explicit_fetch_without_combined_lookup_does_not_claim_unique_reference_kind() {
        let mut app = App::new(Vec::new(), UserConfig::default());
        let repository = repository();

        app.store_fetched_github_item(
            repository.clone(),
            cached_discussion(7, "2026-01-03T00:00:00Z"),
            crate::github::GithubLookupKind::Discussion,
            None,
        );

        assert!(!app.github_references.contains_key(&(repository, 7)));
    }

    #[test]
    fn reference_styling_is_scoped_to_the_repository_that_was_asked() {
        use crate::github::{ReferenceKind, ReferenceState, ReferenceStatus};

        let mut app = App::new(Vec::new(), UserConfig::default());
        let open_issue = ReferenceStatus {
            kind: ReferenceKind::Issue,
            state: ReferenceState::Open,
        };
        app.github_reference_repo = Some(repository());
        app.github_references
            .insert((repository(), 11), Some(open_issue));
        // A number that turned out not to be a reference is remembered as such,
        // so it is never asked about again.
        app.github_references.insert((repository(), 314), None);

        assert_eq!(app.github_reference_status(11), Some(open_issue));
        assert_eq!(app.github_reference_status(314), None);
        assert_eq!(app.github_reference_status(99), None);
        assert!(app.github_references.contains_key(&(repository(), 314)));

        // The same number in a different repository is a different thing.
        let other = crate::github::RepositoryRef {
            host: "github.com".to_string(),
            owner: "octo".to_string(),
            name: "gadgets".to_string(),
        };
        app.github_reference_repo = Some(other);
        assert_eq!(app.github_reference_status(11), None);
    }

    #[test]
    fn pane_switch_updates_reference_repository_even_while_a_request_is_running() {
        let directory = tempfile::tempdir().unwrap();
        let mut app = app_with(true);
        let current = repository();
        app.github_repositories
            .insert(directory.path().to_path_buf(), Some(current.clone()));
        app.github_reference_repo = Some(crate::github::RepositoryRef {
            host: "github.com".to_string(),
            owner: "old".to_string(),
            name: "repository".to_string(),
        });
        let (_sender, receiver) = mpsc::channel();
        app.github_reference_receiver = Some(receiver);

        let (program, args) = if cfg!(windows) {
            (
                "cmd.exe".to_string(),
                vec!["/c".to_string(), "ping -n 3 127.0.0.1 >nul".to_string()],
            )
        } else {
            (
                "/bin/sh".to_string(),
                vec!["-c".to_string(), "sleep 2".to_string()],
            )
        };
        let events = app.mux.as_ref().unwrap().events.clone();
        let pane = Pane::spawn(
            PaneSpec {
                id: 1,
                title: "Current".to_string(),
                cwd: directory.path().to_path_buf(),
                session_id: "current-session".to_string(),
                program,
                args,
                events_path: None,
                terminal_light_mode: Some(false),
                hooks_active: false,
            },
            24,
            80,
            events,
        )
        .unwrap();
        app.mux.as_mut().unwrap().push(pane);
        app.view = View::Attached(1);

        app.refresh_github_references();

        assert_eq!(app.github_reference_repo.as_ref(), Some(&current));
        let _ = app.mux.as_mut().unwrap().shutdown();
    }

    #[test]
    fn inspecting_an_item_backfills_its_fresh_state_into_chat_decoration() {
        use crate::github::{ReferenceKind, ReferenceState, ReferenceStatus};

        let mut app = App::new(Vec::new(), UserConfig::default());
        app.github_reference_repo = Some(repository());
        app.github_references.insert(
            (repository(), 2175),
            Some(ReferenceStatus {
                kind: ReferenceKind::PullRequest,
                state: ReferenceState::Open,
            }),
        );
        let mut item = cached_pull(2175, "2026-08-21T20:50:27Z");
        if let GithubItem::PullRequest(pull) = &mut item {
            pull.common.state = "closed".to_string();
            pull.merged = true;
        }

        app.store_fetched_github_item(
            repository(),
            item,
            crate::github::GithubLookupKind::IssueOrPullRequest,
            None,
        );

        assert_eq!(
            app.github_reference_status(2175),
            Some(ReferenceStatus {
                kind: ReferenceKind::PullRequest,
                state: ReferenceState::Merged,
            }),
            "closing the inspector must leave the chat with the fetched state"
        );

        let (sender, receiver) = mpsc::channel();
        app.github_reference_receiver = Some(receiver);
        sender
            .send(ResolvedReferences {
                repository: repository(),
                statuses: vec![(
                    2175,
                    Some(ReferenceStatus {
                        kind: ReferenceKind::PullRequest,
                        state: ReferenceState::Open,
                    }),
                )],
                periodic: false,
                periodic_batch_len: 0,
                generations: HashMap::from([(2175, 0)]),
            })
            .unwrap();
        app.poll_github_references();
        assert_eq!(
            app.github_reference_status(2175).unwrap().state,
            ReferenceState::Merged,
            "a batch started before the inspector fetch must not overwrite it"
        );

        app.store_github_item(repository(), cached_pull(2175, "2026-08-21T20:49:00Z"));
        assert_eq!(
            app.github_reference_status(2175).unwrap().state,
            ReferenceState::Merged,
            "patch/cache enrichment is not authoritative for reference state"
        );
    }

    #[test]
    fn periodic_reference_batches_update_their_repository_and_finish_the_cycle() {
        use crate::github::{ReferenceKind, ReferenceState, ReferenceStatus};

        let mut app = App::new(Vec::new(), UserConfig::default());
        let repository = repository();
        app.github_reference_periodic_remaining
            .insert(repository.clone(), (1..=41).collect());
        let closed = ReferenceStatus {
            kind: ReferenceKind::Issue,
            state: ReferenceState::Closed,
        };

        let (sender, receiver) = mpsc::channel();
        app.github_reference_receiver = Some(receiver);
        sender
            .send(ResolvedReferences {
                repository: repository.clone(),
                statuses: (1..=40).map(|number| (number, Some(closed))).collect(),
                periodic: true,
                periodic_batch_len: 40,
                generations: (1..=40).map(|number| (number, 0)).collect(),
            })
            .unwrap();
        app.poll_github_references();

        assert_eq!(
            app.github_reference_periodic_remaining[&repository],
            vec![41]
        );
        assert!(app
            .github_reference_refreshed_at
            .contains_key(&(repository.clone(), 1)));
        assert!(!app
            .github_reference_refreshed_at
            .contains_key(&(repository.clone(), 41)));
        assert_eq!(
            app.github_references[&(repository.clone(), 1)],
            Some(closed)
        );

        let (sender, receiver) = mpsc::channel();
        app.github_reference_receiver = Some(receiver);
        sender
            .send(ResolvedReferences {
                repository: repository.clone(),
                statuses: vec![(41, Some(closed))],
                periodic: true,
                periodic_batch_len: 1,
                generations: HashMap::from([(41, 0)]),
            })
            .unwrap();
        app.poll_github_references();

        assert!(!app
            .github_reference_periodic_remaining
            .contains_key(&repository));
        assert!(app
            .github_reference_refreshed_at
            .contains_key(&(repository.clone(), 41)));
        assert_eq!(app.github_references[&(repository, 41)], Some(closed));
    }

    #[test]
    fn newer_periodic_request_beats_an_older_slow_inspector_request() {
        use crate::github::{ReferenceKind, ReferenceState, ReferenceStatus};

        let mut app = App::new(Vec::new(), UserConfig::default());
        let repository = repository();
        app.github_reference_repo = Some(repository.clone());
        let inspector_generation = app.issue_github_reference_generation(repository.clone(), 7);
        let periodic_generation = app.issue_github_reference_generation(repository.clone(), 7);
        let closed = ReferenceStatus {
            kind: ReferenceKind::PullRequest,
            state: ReferenceState::Closed,
        };
        let (sender, receiver) = mpsc::channel();
        app.github_reference_receiver = Some(receiver);
        sender
            .send(ResolvedReferences {
                repository: repository.clone(),
                statuses: vec![(7, Some(closed))],
                periodic: false,
                periodic_batch_len: 0,
                generations: HashMap::from([(7, periodic_generation)]),
            })
            .unwrap();
        app.poll_github_references();

        app.store_fetched_github_item(
            repository,
            cached_pull(7, "2026-08-21T20:00:00Z"),
            crate::github::GithubLookupKind::IssueOrPullRequest,
            Some(inspector_generation),
        );

        assert_eq!(app.github_reference_status(7), Some(closed));
    }

    fn repository() -> crate::github::RepositoryRef {
        crate::github::RepositoryRef {
            host: "github.com".to_string(),
            owner: "octo".to_string(),
            name: "widgets".to_string(),
        }
    }

    #[test]
    fn a_second_look_at_the_same_item_skips_the_spinner() {
        let mut app = App::new(Vec::new(), UserConfig::default());
        let cwd = PathBuf::from("C:/Workspace/widgets");
        app.github_repositories
            .insert(cwd.clone(), Some(repository()));
        app.store_github_item(repository(), cached_pull(7, "2026-01-02T00:00:00Z"));

        app.start_github_request(cwd, 7);

        // The whole point: no Loading screen on the way back to something the
        // user just looked at.
        let inspector = app.github_inspector.as_ref().unwrap();
        assert!(matches!(inspector.screen, GithubInspectorScreen::Ready(_)));
        assert_eq!(inspector.ready_item().unwrap().common().number, 7);
    }

    #[test]
    fn an_unchanged_revalidation_leaves_the_open_item_alone() {
        let mut app = App::new(Vec::new(), UserConfig::default());
        let cwd = PathBuf::from("C:/Workspace/widgets");
        app.github_repositories
            .insert(cwd.clone(), Some(repository()));
        let item = cached_pull(7, "2026-01-02T00:00:00Z");
        app.github_inspector = Some(GithubInspector::number_prompt());
        {
            let inspector = app.github_inspector.as_mut().unwrap();
            inspector.request_id = 42;
            inspector.request_cwd = Some(cwd);
            inspector.number = Some(7);
            inspector.screen = GithubInspectorScreen::Ready(item.clone());
            inspector.scroll_offsets[0] = 25;
        }

        app.apply_github_result(GithubLoadResult {
            request_id: 42,
            result: Ok(crate::github::FetchedItem {
                repository: repository(),
                item,
                discussion_checked: true,
            }),
            lookup_kind: crate::github::GithubLookupKind::IssueOrPullRequest,
            revalidation: true,
            reference_generation: None,
        });

        // Replacing an identical item would throw away where the user had
        // scrolled to.
        assert_eq!(app.github_inspector.as_ref().unwrap().scroll_offsets[0], 25);
    }

    #[test]
    fn a_revalidation_that_moved_on_replaces_what_is_shown() {
        let mut app = App::new(Vec::new(), UserConfig::default());
        let cwd = PathBuf::from("C:/Workspace/widgets");
        app.github_repositories
            .insert(cwd.clone(), Some(repository()));
        app.github_inspector = Some(GithubInspector::number_prompt());
        {
            let inspector = app.github_inspector.as_mut().unwrap();
            inspector.request_id = 42;
            inspector.request_cwd = Some(cwd);
            inspector.number = Some(7);
            inspector.screen = GithubInspectorScreen::Ready(cached_pull(7, "2026-01-02T00:00:00Z"));
        }

        let mut newer = cached_pull(7, "2026-03-03T00:00:00Z");
        if let GithubItem::PullRequest(pull) = &mut newer {
            pull.common.title = "Reworked".to_string();
        }
        app.apply_github_result(GithubLoadResult {
            request_id: 42,
            result: Ok(crate::github::FetchedItem {
                repository: repository(),
                item: newer,
                discussion_checked: true,
            }),
            lookup_kind: crate::github::GithubLookupKind::IssueOrPullRequest,
            revalidation: true,
            reference_generation: None,
        });

        let shown = app.github_inspector.as_ref().unwrap();
        assert_eq!(shown.ready_item().unwrap().common().title, "Reworked");
    }

    #[test]
    fn a_failed_background_check_keeps_the_copy_on_screen() {
        let mut app = App::new(Vec::new(), UserConfig::default());
        app.github_inspector = Some(GithubInspector::number_prompt());
        {
            let inspector = app.github_inspector.as_mut().unwrap();
            inspector.request_id = 42;
            inspector.screen = GithubInspectorScreen::Ready(cached_pull(7, "2026-01-02T00:00:00Z"));
        }

        app.apply_github_result(GithubLoadResult {
            request_id: 42,
            result: Err(crate::github::GithubError {
                kind: crate::github::GithubErrorKind::Cli,
                message: "network is down".to_string(),
            }),
            lookup_kind: crate::github::GithubLookupKind::IssueOrPullRequest,
            revalidation: true,
            reference_generation: None,
        });

        // The user did not ask for the refresh and cannot act on its failure.
        let inspector = app.github_inspector.as_ref().unwrap();
        assert!(
            inspector.ready_item().is_some(),
            "item was replaced by an error"
        );
    }

    #[test]
    fn a_failed_first_load_is_reported() {
        let mut app = App::new(Vec::new(), UserConfig::default());
        app.github_inspector = Some(GithubInspector::number_prompt());
        app.github_inspector.as_mut().unwrap().request_id = 42;

        app.apply_github_result(GithubLoadResult {
            request_id: 42,
            result: Err(crate::github::GithubError {
                kind: crate::github::GithubErrorKind::NotFound,
                message: "no such item".to_string(),
            }),
            lookup_kind: crate::github::GithubLookupKind::IssueOrPullRequest,
            revalidation: false,
            reference_generation: None,
        });

        let screen = &app.github_inspector.as_ref().unwrap().screen;
        assert!(
            matches!(screen, GithubInspectorScreen::Error(message) if message.contains("no such item"))
        );
    }

    #[test]
    fn the_cache_keeps_only_the_most_recent_items() {
        let mut app = App::new(Vec::new(), UserConfig::default());
        for number in 1..=(GITHUB_CACHE_LIMIT as u64 + 3) {
            app.store_github_item(repository(), cached_pull(number, "2026-01-01T00:00:00Z"));
        }

        assert_eq!(app.github_cache.len(), GITHUB_CACHE_LIMIT);
        // The oldest are dropped, not the newest.
        assert!(app.github_cache.iter().all(|entry| entry.number > 3));
    }

    #[test]
    fn reopening_an_item_refreshes_its_place_in_the_cache() {
        let mut app = App::new(Vec::new(), UserConfig::default());
        app.store_github_item(repository(), cached_pull(1, "2026-01-01T00:00:00Z"));
        app.store_github_item(repository(), cached_pull(2, "2026-01-01T00:00:00Z"));
        app.store_github_item(repository(), cached_pull(1, "2026-02-01T00:00:00Z"));

        // Storing twice must update in place rather than keep two copies.
        assert_eq!(app.github_cache.len(), 2);
        assert_eq!(app.github_cache.last().unwrap().number, 1);
    }

    #[test]
    fn retrying_bypasses_the_cached_copy() {
        let mut app = App::new(Vec::new(), UserConfig::default());
        let cwd = PathBuf::from("C:/Workspace/widgets");
        app.github_repositories
            .insert(cwd.clone(), Some(repository()));
        app.store_github_item(repository(), cached_pull(7, "2026-01-02T00:00:00Z"));
        app.github_inspector = Some(GithubInspector::number_prompt());
        {
            let inspector = app.github_inspector.as_mut().unwrap();
            inspector.request_cwd = Some(cwd.clone());
            inspector.number = Some(7);
        }

        app.retry_github_request();

        // Handing back the copy the user is trying to get away from would make
        // the retry key do nothing.
        assert!(app
            .cached_github_item(&cwd, 7, crate::github::GithubLookupKind::IssueOrPullRequest)
            .is_none());
    }

    #[test]
    fn diffs_are_only_requested_for_a_pull_request_that_lacks_them() {
        let mut app = App::new(Vec::new(), UserConfig::default());
        let cwd = PathBuf::from("C:/Workspace/widgets");
        app.github_repositories
            .insert(cwd.clone(), Some(repository()));
        app.github_inspector = Some(GithubInspector::number_prompt());
        {
            let inspector = app.github_inspector.as_mut().unwrap();
            inspector.request_cwd = Some(cwd);
            inspector.number = Some(7);
            inspector.screen = GithubInspectorScreen::Ready(cached_pull(7, "2026-01-02T00:00:00Z"));
        }

        app.ensure_github_patches();
        assert!(
            app.github_patches_loading(),
            "diffs should have been requested"
        );

        // Already loaded: opening the tab again must not refetch.
        app.cancel_github_patches();
        if let Some(GithubItem::PullRequest(pull)) =
            app.github_inspector
                .as_mut()
                .and_then(|inspector| match &mut inspector.screen {
                    GithubInspectorScreen::Ready(item) => Some(item),
                    _ => None,
                })
        {
            pull.patches_loaded = true;
        }
        app.ensure_github_patches();
        assert!(!app.github_patches_loading());
    }

    #[test]
    fn arriving_patches_invalidate_the_rendered_diff_cache() {
        let mut app = App::new(Vec::new(), UserConfig::default());
        app.github_inspector = Some(GithubInspector::number_prompt());
        {
            let inspector = app.github_inspector.as_mut().unwrap();
            inspector.request_id = 42;
            inspector.screen = GithubInspectorScreen::Ready(cached_pull(7, "2026-01-02T00:00:00Z"));
            inspector.diff_render_cache = Some(DiffRenderCache {
                file_index: 0,
                line_count: 1,
                max_width: 10,
                lines: vec![Line::from("old render")],
            });
        }
        let (sender, receiver) = mpsc::channel();
        app.github_patch_receiver = Some(receiver);
        sender
            .send(GithubPatchResult {
                request_id: 42,
                result: Ok(vec![(
                    "src/lib.rs".to_string(),
                    Some("@@ -1 +1 @@\n-old\n+new".to_string()),
                )]),
            })
            .unwrap();

        app.poll_github_patches();

        assert!(app
            .github_inspector
            .as_ref()
            .unwrap()
            .diff_render_cache
            .is_none());
    }

    #[test]
    fn requested_update_check_starts_install_and_prepares_restart() {
        let mut app = App::new(Vec::new(), UserConfig::default());
        let (tx, rx) = mpsc::channel();
        app.update_receiver = Some(rx);
        app.update_check_requested = true;
        tx.send(Ok(Some(UpdateInfo {
            current_version: "0.9.0".to_string(),
            latest_version: "0.10.0".to_string(),
        })))
        .unwrap();

        app.poll_update();

        assert_eq!(app.update_install_requested_for.as_deref(), Some("0.10.0"));
        assert!(!app.should_quit);
        assert!(app.update_info.is_some());
        assert!(!app.update_check_requested);
        assert!(app.update_receiver.is_none());
        assert_eq!(
            app.restart_after_update,
            Some(UpdateRestartRequest::default())
        );
    }

    #[test]
    fn requested_update_check_reports_when_current() {
        let mut app = App::new(Vec::new(), UserConfig::default());
        let (tx, rx) = mpsc::channel();
        app.update_receiver = Some(rx);
        app.update_check_requested = true;
        tx.send(Ok(None)).unwrap();

        app.poll_update();

        assert_eq!(app.status_message.as_deref(), Some("No update available"));
        assert!(!app.update_check_requested);
        assert!(app.update_receiver.is_none());
    }

    #[test]
    fn running_embedded_terminal_requires_update_restart_confirmation() {
        let directory = tempfile::tempdir().unwrap();
        let mut app = App::new(Vec::new(), UserConfig::default());
        app.terminal
            .activate(
                "terminal-session".to_string(),
                "Build shell".to_string(),
                directory.path().to_string_lossy().to_string(),
                &app.config.terminal,
            )
            .unwrap();
        app.update_info = Some(UpdateInfo {
            current_version: "0.9.0".to_string(),
            latest_version: "0.10.0".to_string(),
        });

        app.request_update();

        assert!(app.confirm_update_restart);
        assert!(app.update_install_requested_for.is_none());
        assert_eq!(app.update_restart_titles(), ["Build shell"]);
        let _ = app.terminal.shutdown();
    }

    #[test]
    fn restarted_embedded_terminal_requires_fresh_update_confirmation() {
        let directory = tempfile::tempdir().unwrap();
        let mut app = App::new(Vec::new(), UserConfig::default());
        let activate = |app: &mut App| {
            app.terminal
                .activate(
                    "terminal-session".to_string(),
                    "Build shell".to_string(),
                    directory.path().to_string_lossy().to_string(),
                    &app.config.terminal,
                )
                .unwrap();
        };
        activate(&mut app);
        app.update_info = Some(UpdateInfo {
            current_version: "0.9.0".to_string(),
            latest_version: "0.10.0".to_string(),
        });
        app.request_update();
        app.confirm_update_and_restart();

        app.terminal.exit_active_for_test().unwrap();
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(3);
        while app.terminal.stopped_session_ids().is_empty() && std::time::Instant::now() < deadline
        {
            std::thread::sleep(std::time::Duration::from_millis(20));
        }
        activate(&mut app);
        app.installed_update_version = Some("0.10.0".to_string());

        assert!(app.update_restart_ready());
        assert!(!app.validate_update_restart_sessions());
        assert!(app.confirm_update_restart);
        let _ = app.terminal.shutdown();
    }

    #[test]
    fn completed_explicit_install_is_ready_to_restart() {
        let mut app = App::new(Vec::new(), UserConfig::default());
        app.update_info = Some(UpdateInfo {
            current_version: env!("CARGO_PKG_VERSION").to_string(),
            latest_version: "99.0.0".to_string(),
        });
        app.request_update();
        let (sender, receiver) = mpsc::channel();
        app.update_install_receiver = Some(receiver);
        sender
            .send(Ok(UpdateInstallOutcome::Installed("99.0.0".to_string())))
            .unwrap();

        app.poll_update();

        assert_eq!(app.installed_update_version.as_deref(), Some("99.0.0"));
        assert!(app.update_info.is_none());
        assert!(!app.should_quit);
        assert!(app.should_resume.is_none());
        assert!(app.update_restart_ready());
        assert!(app
            .status_message
            .as_deref()
            .unwrap()
            .contains("restarting CST"));
    }

    #[test]
    fn exit_uses_a_bounded_notification_drain_and_stops_new_enqueues() {
        let mut app = App::new(Vec::new(), UserConfig::default());
        app.disable_config_persistence();
        app.notification_pending = 1;

        assert!(app.exit_waits_for_notifications());
        assert!(app
            .status_message
            .as_deref()
            .unwrap()
            .contains("final phone notification"));

        app.config.notifications.enabled = true;
        app.config.notifications.topic = "private_topic".to_string();
        app.enqueue_notification(NotificationKind::Ready, "Late event".to_string(), None);
        assert!(
            app.notification_requests.is_empty(),
            "draining rejects events that were not already accepted"
        );

        app.cancel_notification_drain();
        app.enqueue_notification(NotificationKind::Ready, "Recovered event".to_string(), None);
        assert_eq!(
            app.notification_requests.len(),
            1,
            "notifications resume when an exit request is abandoned"
        );

        assert!(app.exit_waits_for_notifications());
        app.notification_drain_started = Some(Instant::now() - Duration::from_secs(3));
        assert!(!app.exit_waits_for_notifications());
    }

    #[test]
    fn abandoned_update_restart_reenables_notifications() {
        let mut app = App::new(Vec::new(), UserConfig::default());
        app.disable_config_persistence();
        app.notification_pending = 1;
        assert!(app.exit_waits_for_notifications());
        assert!(app.notification_drain_started.is_some());

        app.cancel_update_restart_after_failure("restart postponed");

        assert!(app.notification_drain_started.is_none());
    }

    #[test]
    fn finalized_restart_keeps_only_panes_cst_actually_terminated() {
        let mut app = App::new(Vec::new(), UserConfig::default());
        app.restart_after_update = Some(UpdateRestartRequest {
            panes: vec![
                UpdateRestartPane {
                    pane_id: Some(1),
                    copilot_running: true,
                    terminal_generation: None,
                    session_id: "finished-naturally".to_string(),
                    cwd: PathBuf::from("one"),
                    title: "One".to_string(),
                },
                UpdateRestartPane {
                    pane_id: Some(2),
                    copilot_running: true,
                    terminal_generation: None,
                    session_id: "terminated-by-cst".to_string(),
                    cwd: PathBuf::from("two"),
                    title: "Two".to_string(),
                },
            ],
            focused_session_id: Some("finished-naturally".to_string()),
        });

        app.retain_terminated_restart_panes(&[2]);

        let request = app.restart_after_update.unwrap();
        assert_eq!(request.panes.len(), 1);
        assert_eq!(request.panes[0].session_id, "terminated-by-cst");
        assert_eq!(
            request.focused_session_id.as_deref(),
            Some("terminated-by-cst")
        );
    }

    #[test]
    fn update_restart_waits_for_open_editing_flows() {
        let mut app = App::new(Vec::new(), UserConfig::default());
        app.restart_after_update = Some(UpdateRestartRequest::default());
        app.installed_update_version = Some("99.0.0".to_string());
        app.mode = Mode::Settings;

        assert!(app.update_restart_ready());
        assert!(app.update_restart_deferred_by_editor());
        app.note_deferred_update_restart();
        assert!(app
            .status_message
            .as_deref()
            .unwrap()
            .contains("close the current editor"));

        app.mode = Mode::Normal;
        assert!(!app.update_restart_deferred_by_editor());

        app.workspace_focus = WorkspaceFocus::Scratchpad;
        assert!(app.update_restart_deferred_by_editor());
    }

    #[test]
    fn every_work_cycle_refreshes_the_context_offset_even_when_detailed_is_disabled() {
        let temp = tempfile::tempdir().unwrap();
        let events = temp
            .path()
            .join("session-state")
            .join("session")
            .join("events.jsonl");
        std::fs::create_dir_all(events.parent().unwrap()).unwrap();
        std::fs::write(&events, "old").unwrap();
        let mut app = App::new(Vec::new(), UserConfig::default());
        app.copilot_home = temp.path().to_path_buf();
        app.config.notifications.enabled = true;
        app.config.ntfy_verbose = true;
        app.begin_notification_cycle("session");
        assert_eq!(
            app.notification_cycle_offsets.get("session"),
            Some(&3),
            "the first cycle starts after existing events"
        );

        std::fs::write(&events, "old-new").unwrap();
        app.config.ntfy_verbose = false;
        app.begin_notification_cycle("session");
        assert_eq!(
            app.notification_cycle_offsets.get("session"),
            Some(&7),
            "a later detailed-mode change cannot reuse the previous cycle"
        );
    }
}

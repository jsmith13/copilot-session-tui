use crate::app::{App, View, WorkspaceFocus};
use crate::mux::pane::PaneStatus;
use fuzzy_matcher::skim::SkimMatcherV2;
use fuzzy_matcher::FuzzyMatcher;
use ratatui::layout::Rect;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum CommandId {
    FocusChat,
    ToggleScratchpad,
    ToggleTerminal,
    OpenSnippets,
    OpenScratchpadHelp,
    BackToSessionList,
    SwitchSession,
    MoveTab,
    NextSession,
    PreviousSession,
    ResumeSelected,
    ResumeSelectedInTmux,
    NewSession,
    NewTmuxSession,
    NewWorktreeSession,
    NewTmuxWorktreeSession,
    OpenSelectedScratchpad,
    ToggleFavorite,
    ReorderFavorite,
    OpenFavoriteTabs,
    RenameSelected,
    DeleteSelected,
    SearchSessions,
    FilterProject,
    ClearProjectFilter,
    CycleSort,
    ToggleHiddenSessions,
    InspectGithub,
    GlobalSettings,
    ProjectSettings,
    CheckForUpdates,
    ThreadInbox,
    WhatsNew,
    OpenHelp,
    SendLiteralPrefix,
    EndSession,
    EndPersistentSession,
    Quit,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CommandGroup {
    Workspace,
    Sessions,
    GitHub,
    View,
    Settings,
    Lifecycle,
}

impl CommandGroup {
    pub fn label(self) -> &'static str {
        match self {
            Self::Workspace => "Workspace",
            Self::Sessions => "Sessions",
            Self::GitHub => "GitHub",
            Self::View => "View",
            Self::Settings => "Settings",
            Self::Lifecycle => "Lifecycle",
        }
    }
}

#[derive(Debug, Clone)]
pub struct CommandEntry {
    pub id: CommandId,
    pub group: CommandGroup,
    pub title: &'static str,
    pub description: &'static str,
    pub shortcut: String,
    pub enabled: bool,
    pub unavailable_reason: Option<&'static str>,
}

#[derive(Debug, Default)]
pub struct CommandPalette {
    pub query: String,
    pub selected: usize,
    pub scroll: usize,
    pub visible_rows: usize,
    pub hits: Vec<(Rect, CommandId)>,
    pub error: Option<String>,
}

impl CommandPalette {
    pub fn move_selection(&mut self, delta: isize, count: usize) {
        self.error = None;
        if count == 0 {
            self.selected = 0;
            self.scroll = 0;
            return;
        }
        self.selected = if delta < 0 {
            self.selected.saturating_sub(delta.unsigned_abs())
        } else {
            self.selected.saturating_add(delta as usize).min(count - 1)
        };
        self.keep_selected_visible();
    }

    pub fn set_selected(&mut self, selected: usize, count: usize) {
        self.error = None;
        self.selected = selected.min(count.saturating_sub(1));
        self.keep_selected_visible();
    }

    pub fn reset_filter(&mut self) {
        self.selected = 0;
        self.scroll = 0;
        self.error = None;
    }

    pub fn keep_selected_visible(&mut self) {
        let visible = self.visible_rows.max(1);
        if self.selected < self.scroll {
            self.scroll = self.selected;
        } else if self.selected >= self.scroll + visible {
            self.scroll = self.selected + 1 - visible;
        }
    }
}

pub fn filtered_commands(app: &App) -> Vec<CommandEntry> {
    let query = app
        .command_palette
        .as_ref()
        .map(|palette| palette.query.trim())
        .unwrap_or_default();
    let commands = commands(app);
    if query.is_empty() {
        return commands;
    }

    let matcher = SkimMatcherV2::default();
    let mut scored: Vec<(i64, usize, CommandEntry)> = commands
        .into_iter()
        .enumerate()
        .filter_map(|(index, command)| {
            let search = format!(
                "{} {} {} {} {}",
                command.group.label(),
                command.title,
                command.description,
                command.shortcut,
                command_keywords(command.id)
            );
            matcher
                .fuzzy_match(&search, query)
                .map(|score| (score, index, command))
        })
        .collect();
    scored.sort_by(|left, right| right.0.cmp(&left.0).then(left.1.cmp(&right.1)));
    scored.into_iter().map(|(_, _, command)| command).collect()
}

fn commands(app: &App) -> Vec<CommandEntry> {
    use CommandGroup as Group;
    use CommandId as Id;

    let attached = matches!(app.view, View::Attached(_));
    let list = matches!(app.view, View::List);
    let running_pane = app
        .mux
        .as_ref()
        .and_then(|mux| mux.focused_pane())
        .is_some_and(|pane| pane.status == PaneStatus::Running);
    let has_panes = app.mux.as_ref().is_some_and(|mux| !mux.panes.is_empty());
    let selected = app.selected_session().is_some();
    let favorite_selected = app
        .selected_session()
        .is_some_and(|session| app.is_favorite(&session.id));
    let project = app.command_project().is_some();
    let session_dir = app.new_session_dir().is_some();
    let tmux_supported = crate::session::tmux::supported_platform();
    let tmux_new_shortcut = app.config.tmux_keys.new_session.clone();
    let tmux_resume_shortcut = app.config.tmux_keys.resume_session.clone();
    let tmux_worktree_shortcut = app.config.tmux_keys.new_worktree.clone();
    let tmux_end_shortcut = format!(
        "{} / C-b {}",
        app.config.tmux_keys.end_session, app.config.tmux_keys.end_session
    );
    let tmux_target = match app.view {
        View::Attached(_) => app
            .mux
            .as_ref()
            .and_then(|mux| mux.focused_pane())
            .is_some_and(|pane| app.pane_is_tmux_backed(pane)),
        View::List => app
            .selected_session()
            .is_some_and(|session| app.session_is_tmux_backed(&session.id)),
    };

    let mut commands = vec![
        entry(
            Id::FocusChat,
            Group::Workspace,
            "Focus chat",
            "Move input focus to Copilot",
            "C-b c",
            attached && running_pane,
            "Requires an attached running session",
        ),
        entry(
            Id::ToggleScratchpad,
            Group::Workspace,
            "Toggle scratchpad",
            "Open or focus the session scratchpad",
            "C-b e",
            attached,
            "Requires an attached session",
        ),
        entry(
            Id::ToggleTerminal,
            Group::Workspace,
            "Toggle terminal",
            "Open or focus the session terminal",
            "C-b t",
            attached,
            "Requires an attached session",
        ),
        entry(
            Id::OpenSnippets,
            Group::Workspace,
            "Prompt snippets",
            "Open reusable prompt snippets",
            "C-b s",
            attached && running_pane,
            "Requires an attached running session",
        ),
        entry(
            Id::OpenScratchpadHelp,
            Group::Workspace,
            "Scratchpad help",
            "Show scratchpad editing shortcuts",
            "C-b h e",
            attached,
            "Requires an attached session",
        ),
        entry(
            Id::BackToSessionList,
            Group::Sessions,
            "Session list",
            "Return to the picker without ending panes",
            "C-b d",
            attached,
            "Already on the session list",
        ),
        entry(
            Id::SwitchSession,
            Group::Sessions,
            "Switch session",
            "Open the running-pane switcher",
            "C-b w",
            has_panes,
            "No panes are open",
        ),
        entry(
            Id::MoveTab,
            Group::Sessions,
            "Move tab",
            "Slide this tab along the strip with the arrows",
            "C-b m",
            attached && has_panes,
            "Requires an attached session",
        ),
        entry(
            Id::NextSession,
            Group::Sessions,
            "Next session",
            "Focus the next running pane",
            "C-b n",
            has_panes,
            "No panes are open",
        ),
        entry(
            Id::PreviousSession,
            Group::Sessions,
            "Previous session",
            "Focus the previous running pane",
            "C-b p",
            has_panes,
            "No panes are open",
        ),
        entry(
            Id::ResumeSelected,
            Group::Sessions,
            "Resume selected session",
            "Open the selected picker item",
            "Enter",
            list && selected,
            "Available from the session list",
        ),
        entry(
            Id::ResumeSelectedInTmux,
            Group::Sessions,
            "Resume selected session in tmux",
            "Open the selected conversation in a persistent tmux session",
            &tmux_resume_shortcut,
            list && selected && tmux_supported,
            if tmux_supported {
                "Available for a selected session in the list"
            } else {
                "tmux requires a Unix-like operating system"
            },
        ),
        entry(
            Id::NewSession,
            Group::Sessions,
            "New session",
            "Start Copilot in the active project, asking as-is or worktree",
            "n",
            session_dir,
            "No session directory is available",
        ),
        entry(
            Id::NewTmuxSession,
            Group::Sessions,
            "New tmux session",
            "Start a persistent Copilot session backed by tmux",
            &tmux_new_shortcut,
            session_dir && tmux_supported,
            if tmux_supported {
                "No session directory is available"
            } else {
                "tmux requires a Unix-like operating system"
            },
        ),
        entry(
            Id::NewWorktreeSession,
            Group::Sessions,
            "New worktree session",
            "Create an isolated worktree session",
            "N",
            project,
            "No Git project is available",
        ),
        entry(
            Id::NewTmuxWorktreeSession,
            Group::Sessions,
            "New tmux worktree session",
            "Create an isolated persistent session backed by tmux",
            &tmux_worktree_shortcut,
            project && tmux_supported,
            if tmux_supported {
                "No Git project is available"
            } else {
                "tmux requires a Unix-like operating system"
            },
        ),
        entry(
            Id::OpenSelectedScratchpad,
            Group::Workspace,
            "Selected session scratchpad",
            "Open the picker item's scratchpad",
            "e",
            list && selected,
            "Available for a selected session in the list",
        ),
        entry(
            Id::ToggleFavorite,
            Group::Sessions,
            "Toggle favorite",
            "Favorite or unfavorite the selected session",
            "Space",
            list && selected,
            "Available for a selected session in the list",
        ),
        entry(
            Id::ReorderFavorite,
            Group::Sessions,
            "Reorder favorite",
            "Grab the selected favorite for arrow-key reordering",
            "g",
            list && favorite_selected && app.favorites_section_active(),
            "Show the unfiltered favorites group before reordering",
        ),
        entry(
            Id::OpenFavoriteTabs,
            Group::Sessions,
            "Open favorites",
            "Open inactive favorites as panes or terminal tabs",
            "T",
            true,
            "",
        ),
        entry(
            Id::RenameSelected,
            Group::Sessions,
            "Rename selected session",
            "Edit the selected session name",
            "r",
            list && selected,
            "Available for a selected session in the list",
        ),
        entry(
            Id::DeleteSelected,
            Group::Lifecycle,
            "Delete selected session",
            "Delete session data and managed worktree",
            "d",
            list && selected,
            "Available for a selected session in the list",
        ),
        entry(
            Id::SearchSessions,
            Group::View,
            "Search sessions",
            "Fuzzy-filter the session catalog",
            "/",
            true,
            "",
        ),
        entry(
            Id::FilterProject,
            Group::View,
            "Filter by project",
            "Choose a project filter",
            "f / p",
            true,
            "",
        ),
        entry(
            Id::ClearProjectFilter,
            Group::View,
            "Clear project filter",
            "Show sessions from all projects",
            "c",
            app.project_filter.is_some(),
            "No project filter is active",
        ),
        entry(
            Id::CycleSort,
            Group::View,
            "Cycle sort order",
            "Change session list ordering",
            "s",
            true,
            "",
        ),
        entry(
            Id::ToggleHiddenSessions,
            Group::View,
            if app.show_hidden_sessions {
                "Hide configured sessions"
            } else {
                "Show hidden sessions"
            },
            "Temporarily toggle configured hidden-session filters",
            "H",
            !app.config.hidden_title_prefixes.is_empty()
                || !app.config.hidden_path_prefixes.is_empty(),
            "No hidden session filters are configured",
        ),
        entry(
            Id::InspectGithub,
            Group::GitHub,
            "Inspect GitHub item",
            "Open an issue, pull request, or discussion",
            "C-b g i",
            attached,
            "Requires an attached session",
        ),
        entry(
            Id::GlobalSettings,
            Group::Settings,
            "Global settings",
            "Edit CST behavior, theme, terminal, and notifications",
            ",",
            true,
            "",
        ),
        entry(
            Id::ProjectSettings,
            Group::Settings,
            "Project settings",
            "Edit worktree settings for the active project",
            ".",
            project,
            "No Git project is available",
        ),
        entry(
            Id::CheckForUpdates,
            Group::Settings,
            "Check for updates",
            "Install and restart into the latest CST",
            "u / C-b u",
            true,
            "",
        ),
        entry(
            Id::ThreadInbox,
            Group::GitHub,
            "Waiting for you",
            "GitHub thread messages held for your approval",
            "",
            !app.thread_pending.is_empty(),
            "Nothing is waiting",
        ),
        entry(
            Id::WhatsNew,
            Group::Settings,
            "What's new",
            "Release notes for this version and recent updates",
            "",
            true,
            "",
        ),
        entry(
            Id::OpenHelp,
            Group::Settings,
            "Keyboard help",
            "Open the complete shortcut reference",
            "?",
            true,
            "",
        ),
        entry(
            Id::SendLiteralPrefix,
            Group::Workspace,
            "Send literal prefix",
            "Forward the configured prefix key to Copilot",
            "",
            attached && running_pane && app.workspace_focus == WorkspaceFocus::Chat,
            "Requires chat focus in a running session",
        ),
        entry(
            Id::EndSession,
            Group::Lifecycle,
            "Close focused session tab",
            "End a CST-owned session or detach a tmux-backed session",
            "C-b x",
            attached && running_pane,
            "Requires an attached running session",
        ),
        entry(
            Id::EndPersistentSession,
            Group::Lifecycle,
            "End persistent tmux session",
            "Stop Copilot and remove its tmux ownership",
            &tmux_end_shortcut,
            tmux_target,
            "Requires a tmux-backed session",
        ),
        entry(
            Id::Quit,
            Group::Lifecycle,
            "Quit CST",
            "Exit, confirming any resources that would stop",
            "q / C-b q",
            true,
            "",
        ),
    ];
    let prefix = app
        .mux
        .as_ref()
        .map(|mux| mux.prefix.label())
        .unwrap_or_else(|| "C-b".to_string());
    for command in &mut commands {
        command.shortcut = command.shortcut.replace("C-b", &prefix);
    }
    commands
}

fn entry(
    id: CommandId,
    group: CommandGroup,
    title: &'static str,
    description: &'static str,
    shortcut: impl Into<String>,
    enabled: bool,
    unavailable_reason: &'static str,
) -> CommandEntry {
    CommandEntry {
        id,
        group,
        title,
        description,
        shortcut: shortcut.into(),
        enabled,
        unavailable_reason: (!enabled).then_some(unavailable_reason),
    }
}

fn command_keywords(id: CommandId) -> &'static str {
    use CommandId::*;
    match id {
        FocusChat => "copilot conversation input",
        ToggleScratchpad | OpenSelectedScratchpad | OpenScratchpadHelp => "notes editor",
        ToggleTerminal => "shell console",
        OpenSnippets => "prompt reusable",
        BackToSessionList => "detach picker catalog",
        SwitchSession | NextSession | PreviousSession => "pane tab mux",
        MoveTab => "reorder move tab drag arrange",
        ResumeSelected => "attach open",
        ResumeSelectedInTmux => "attach open persistent tmux tab",
        NewSession => "create copilot",
        NewTmuxSession => "create copilot persistent tmux tab",
        NewWorktreeSession => "create branch isolated git",
        NewTmuxWorktreeSession => "create branch isolated git persistent tmux tab",
        ToggleFavorite | ReorderFavorite | OpenFavoriteTabs => "star pin windows terminal",
        RenameSelected => "name edit",
        DeleteSelected => "remove worktree",
        SearchSessions => "find filter fuzzy",
        FilterProject | ClearProjectFilter => "repository scope",
        CycleSort => "order",
        ToggleHiddenSessions => "exclude hidden title path prefix visibility",
        InspectGithub => "issue pr pull request discussion",
        GlobalSettings | ProjectSettings => "config preferences theme",
        CheckForUpdates => "upgrade version release restart",
        ThreadInbox => "threads github messages waiting agents inbox comments",
        WhatsNew => "changelog release notes updates version history",
        OpenHelp => "shortcuts documentation",
        SendLiteralPrefix => "raw control key",
        EndSession => "close detach tab",
        EndPersistentSession => "kill stop terminate persistent tmux",
        Quit => "exit close",
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::UserConfig;

    #[test]
    fn list_context_keeps_attached_commands_visible_but_disabled() {
        let app = App::new(Vec::new(), UserConfig::default());
        let commands = filtered_commands(&app);

        let chat = commands
            .iter()
            .find(|command| command.id == CommandId::FocusChat)
            .unwrap();
        let settings = commands
            .iter()
            .find(|command| command.id == CommandId::GlobalSettings)
            .unwrap();
        assert!(!chat.enabled);
        assert_eq!(
            chat.unavailable_reason,
            Some("Requires an attached running session")
        );
        assert!(settings.enabled);
    }

    #[test]
    fn fuzzy_search_matches_descriptions_and_keywords() {
        let mut app = App::new(Vec::new(), UserConfig::default());
        app.command_palette = Some(CommandPalette {
            query: "github discussion".to_string(),
            ..CommandPalette::default()
        });

        let commands = filtered_commands(&app);

        assert_eq!(commands[0].id, CommandId::InspectGithub);
    }

    #[test]
    fn command_shortcuts_follow_the_configured_prefix() {
        let app = App::new(
            Vec::new(),
            UserConfig {
                mux: true,
                mux_prefix: "C-a".to_string(),
                ..UserConfig::default()
            },
        );

        let commands = filtered_commands(&app);
        let chat = commands
            .iter()
            .find(|command| command.id == CommandId::FocusChat)
            .unwrap();
        let update = commands
            .iter()
            .find(|command| command.id == CommandId::CheckForUpdates)
            .unwrap();

        assert_eq!(chat.shortcut, "C-a c");
        assert_eq!(update.shortcut, "u / C-a u");
    }

    #[test]
    fn selection_stays_inside_the_visible_window() {
        let mut palette = CommandPalette {
            visible_rows: 3,
            ..CommandPalette::default()
        };

        palette.move_selection(5, 10);
        assert_eq!(palette.selected, 5);
        assert_eq!(palette.scroll, 3);
        palette.move_selection(-4, 10);
        assert_eq!(palette.selected, 1);
        assert_eq!(palette.scroll, 1);
    }
}

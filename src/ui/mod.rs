pub mod command_palette;
pub mod diff;
pub mod file_tree;
pub mod github_inspector;
pub mod hyperlinks;
pub mod pane;
pub mod popups;
pub mod scratchpad;
pub mod session_detail;
pub mod session_list;
pub mod snippets;
pub mod status_bar;
pub mod tabs;
pub mod terminal_pane;
pub mod thread_inbox;
pub mod whats_new;

use ratatui::layout::{Constraint, Direction, Layout, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::Paragraph;
use ratatui::Frame;

use crate::app::{App, Mode, View, WorkspaceAreas, WorkspaceFocus, WorkspaceHelp};
use crate::theme::{fill_area, Theme, ThemeName};

const SPINNER_FRAMES: [&str; 8] = ["⠋", "⠙", "⠹", "⠸", "⠼", "⠴", "⠦", "⠧"];

pub(crate) fn spinner_frame() -> &'static str {
    let tick = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|duration| duration.as_millis() / 120)
        .unwrap_or_default();
    spinner_frame_at(tick)
}

fn spinner_frame_at(tick: u128) -> &'static str {
    SPINNER_FRAMES[tick as usize % SPINNER_FRAMES.len()]
}

pub(crate) fn foreground_on(theme: Theme, background: Color) -> Color {
    if theme.name == ThemeName::Classic {
        theme.text
    } else {
        theme.contrast_text(background)
    }
}

pub(crate) fn badge_foreground(theme: Theme, background: Color) -> Color {
    if theme.name == ThemeName::Classic {
        theme.selection_fg
    } else {
        theme.contrast_text(background)
    }
}

pub(crate) fn semantic_foreground_on(theme: Theme, semantic: Color, background: Color) -> Color {
    if theme.is_light {
        foreground_on(theme, background)
    } else {
        semantic
    }
}

pub(crate) fn row_selection_style(theme: Theme) -> Style {
    if theme.name == ThemeName::Classic {
        Style::default()
            .fg(Color::White)
            .bg(Color::DarkGray)
            .add_modifier(Modifier::BOLD)
    } else {
        Style::default()
            .fg(theme.selection_fg)
            .bg(theme.selection_bg)
            .add_modifier(Modifier::BOLD)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct AttachedLayout {
    pub tabs: Rect,
    pub chat: Rect,
    pub scratchpad: Option<Rect>,
    pub terminal: Option<Rect>,
    pub status: Rect,
}

impl AttachedLayout {
    /// Where the session pane's own cells go: inside the chat box's border.
    ///
    /// One definition, because two things must agree on it exactly — where panes are
    /// sized to, and where anything that addresses a pane's cells from outside thinks they
    /// are. A click, or a link drawn over the frame, lands a cell off otherwise.
    pub fn chat_pane(&self) -> Rect {
        self.chat.inner(ratatui::layout::Margin {
            horizontal: 1,
            vertical: 1,
        })
    }
}

/// Padding row, the labels, then the rule that underlines the focused tab.
///
/// The leading blank row keeps the tabs off the top edge of the window, matching the
/// gap the rule leaves below them.
pub const TAB_BAR_HEIGHT: u16 = 3;

pub fn draw(f: &mut Frame, app: &mut App) {
    let size = f.area();
    let theme = app.theme();
    fill_area(f.buffer_mut(), size, theme.background);

    if app.github_inspector.is_some() && !github_inspector::is_prompt(app) {
        github_inspector::draw(f, app);
        draw_context_overlays(f, app, theme);
        command_palette::draw_overlays(f, app);
        thread_inbox::draw(f, app);
        whats_new::draw(f, app);
        if app.confirm_update_restart {
            popups::draw_update_restart_confirm(f, app);
        }
        if app.confirm_end_tmux.is_some() {
            popups::draw_end_tmux_confirm(f, app);
        }
        if app.confirm_quit {
            popups::draw_quit_confirm(f, app);
        }
        return;
    }

    if app.mode == Mode::Scratchpad {
        if let Some(scratchpad) = app.scratchpad.as_mut() {
            scratchpad::draw_with_theme(f, scratchpad, theme);
        }
        draw_portable_mode_popup(f, app);
        command_palette::draw_overlays(f, app);
        thread_inbox::draw(f, app);
        whats_new::draw(f, app);
        if app.confirm_update_restart {
            popups::draw_update_restart_confirm(f, app);
        }
        if app.confirm_end_tmux.is_some() {
            popups::draw_end_tmux_confirm(f, app);
        }
        if app.confirm_quit {
            popups::draw_quit_confirm(f, app);
        }
        return;
    }

    if matches!(app.view, View::Attached(_)) {
        let layout = attached_layout(
            size,
            app.attached_scratchpad_visible(),
            app.attached_terminal_visible(),
            app.tab_bar_visible(),
        );
        app.workspace_areas = WorkspaceAreas {
            chat: layout.chat,
            tabs: layout.tabs,
            scratchpad: layout.scratchpad,
            terminal: layout.terminal,
        };
        pane::draw_chat(f, app, layout.chat);
        if let (Some(area), Some(scratchpad)) = (layout.scratchpad, app.scratchpad.as_mut()) {
            scratchpad::draw_in_with_theme(
                f,
                scratchpad,
                area,
                app.workspace_focus == WorkspaceFocus::Scratchpad,
                theme,
            );
        }
        if let (Some(area), Some(terminal)) = (layout.terminal, app.terminal.active()) {
            terminal_pane::draw_with_theme(
                f,
                terminal,
                app.workspace_focus == WorkspaceFocus::Terminal,
                area,
                theme,
            );
        }
        pane::draw_tabs(f, app, layout.tabs);
        pane::draw_status(f, app, layout.status);
        if github_inspector::is_prompt(app) {
            github_inspector::draw(f, app);
        }
        if app.snippet_modal.is_some() {
            snippets::draw(f, app);
        }
        if matches!(app.workspace_help, Some(WorkspaceHelp::Scratchpad)) {
            scratchpad::draw_help_with_theme(f, size, theme);
        }
        draw_portable_mode_popup(f, app);
        if app.mode == Mode::FavoriteOpen {
            popups::draw_favorite_open(f, app);
        }
        if app.mode == Mode::NewSessionKind {
            popups::draw_new_session_kind(f, app);
        }
        // `prefix q` can raise this without leaving the pane, so it has to be drawn
        // here too — the list view below is never reached while attached.
        command_palette::draw_overlays(f, app);
        thread_inbox::draw(f, app);
        whats_new::draw(f, app);
        if app.confirm_update_restart {
            popups::draw_update_restart_confirm(f, app);
        }
        if app.confirm_end_tmux.is_some() {
            popups::draw_end_tmux_confirm(f, app);
        }
        if app.confirm_quit {
            popups::draw_quit_confirm(f, app);
        }
        return;
    }

    let main_layout = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(1), // title bar
            Constraint::Min(5),    // main content
            Constraint::Length(2), // status bar
        ])
        .split(size);

    // Title bar
    let filter_text = match &app.project_filter {
        Some(p) => {
            let name = std::path::Path::new(p)
                .file_name()
                .and_then(|n| n.to_str())
                .unwrap_or(p);
            format!(" Filter: {} ", name)
        }
        None => " All Projects ".to_string(),
    };

    let sort_text = format!(" Sort: {} ", app.sort_label());

    let title = Paragraph::new(Line::from(vec![
        Span::styled(
            " Copilot Session Manager ",
            Style::default()
                .fg(theme.selection_fg)
                .bg(theme.selection_bg)
                .add_modifier(Modifier::BOLD),
        ),
        Span::raw("  "),
        Span::styled(
            filter_text,
            Style::default()
                .fg(badge_foreground(theme, theme.warning))
                .bg(theme.warning),
        ),
        Span::raw("  "),
        Span::styled(
            sort_text,
            Style::default()
                .fg(badge_foreground(theme, theme.accent))
                .bg(theme.accent),
        ),
        Span::raw(format!("  {} sessions", app.filtered_indices.len())),
        Span::styled(
            if app.sessions_loading() {
                format!(" · {} loading remaining sessions…", spinner_frame())
            } else {
                String::new()
            },
            Style::default()
                .fg(semantic_foreground_on(theme, theme.info, theme.background))
                .add_modifier(Modifier::BOLD),
        ),
    ]))
    .style(Style::default().fg(theme.text).bg(theme.background));

    f.render_widget(title, main_layout[0]);

    // Main content: session list + detail pane
    let content_layout = Layout::default()
        .direction(Direction::Horizontal)
        .constraints([Constraint::Percentage(45), Constraint::Percentage(55)])
        .split(main_layout[1]);

    session_list::draw(f, app, content_layout[0]);
    session_detail::draw(f, app, content_layout[1]);

    // Status bar
    status_bar::draw(f, app, main_layout[2]);

    // Popups overlay
    match app.mode {
        Mode::ConfirmDelete => popups::draw_delete_confirm(f, app),
        Mode::ConfirmForceDelete => popups::draw_force_delete_confirm(f, app),
        Mode::ConfirmTakeover => popups::draw_takeover_confirm(f, app),
        Mode::FavoriteOpen => popups::draw_favorite_open(f, app),
        Mode::NewSessionKind => popups::draw_new_session_kind(f, app),
        Mode::FilterProject => popups::draw_project_filter(f, app),
        Mode::Rename => popups::draw_rename(f, app),
        _ => {}
    }
    draw_portable_mode_popup(f, app);
    if matches!(app.workspace_help, Some(WorkspaceHelp::Scratchpad)) {
        scratchpad::draw_help_with_theme(f, size, theme);
    }

    command_palette::draw_overlays(f, app);
    thread_inbox::draw(f, app);
    whats_new::draw(f, app);
    if app.confirm_update_restart {
        popups::draw_update_restart_confirm(f, app);
    }
    if app.confirm_end_tmux.is_some() {
        popups::draw_end_tmux_confirm(f, app);
    }
    if app.confirm_quit {
        popups::draw_quit_confirm(f, app);
    }

    // Drawn last so it covers everything: the next loop iteration blocks on Git, and
    // this frame is the only feedback the user gets until it returns.
    if let Some(pending) = app.pending_worktree.as_ref() {
        popups::draw_busy(
            f,
            "Creating worktree",
            &format!(
                "Branch '{}' — copying files and checking out…",
                pending.branch
            ),
            theme,
        );
    }
}

fn draw_context_overlays(f: &mut Frame, app: &mut App, theme: Theme) {
    if app.snippet_modal.is_some() {
        snippets::draw(f, app);
    }
    if matches!(app.workspace_help, Some(WorkspaceHelp::Scratchpad)) {
        scratchpad::draw_help_with_theme(f, f.area(), theme);
    }
    draw_portable_mode_popup(f, app);
}

fn draw_portable_mode_popup(f: &mut Frame, app: &mut App) {
    match app.mode {
        Mode::Help => popups::draw_help(f, app),
        Mode::Settings => popups::draw_settings(f, app),
        Mode::ProjectSettings => popups::draw_project_settings(f, app),
        Mode::BranchName => popups::draw_branch_name(f, app),
        Mode::PaneList => popups::draw_pane_list(f, app),
        _ => {}
    }
}

pub fn terminal_panel_height(content_height: u16) -> u16 {
    if content_height < 10 {
        content_height / 2
    } else {
        (content_height * 2 / 5).clamp(7, 16)
    }
}

pub fn attached_layout(
    area: Rect,
    scratchpad_visible: bool,
    terminal_visible: bool,
    tabs_visible: bool,
) -> AttachedLayout {
    // A lone session has nothing to switch to, so the strip collapses to nothing rather
    // than spending two rows of Copilot's output on a single tab.
    let tab_height = if tabs_visible { TAB_BAR_HEIGHT } else { 0 };
    let vertical = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(tab_height),
            Constraint::Min(1),
            Constraint::Length(1),
        ])
        .split(area);
    let tabs = vertical[0];
    let content = vertical[1];
    let status = vertical[2];

    let (top, terminal) = if terminal_visible {
        let sections = Layout::default()
            .direction(Direction::Vertical)
            .constraints([
                Constraint::Min(5),
                Constraint::Length(terminal_panel_height(content.height)),
            ])
            .split(content);
        (sections[0], Some(sections[1]))
    } else {
        (content, None)
    };

    let (chat, scratchpad) = if scratchpad_visible {
        let sections = Layout::default()
            .direction(Direction::Horizontal)
            .constraints([Constraint::Percentage(65), Constraint::Percentage(35)])
            .split(top);
        (sections[0], Some(sections[1]))
    } else {
        (top, None)
    };

    AttachedLayout {
        tabs,
        chat,
        scratchpad,
        terminal,
        status,
    }
}

/// A vertical scrollbar down the right edge of `area`, drawn only when the content
/// actually overflows.
///
/// Shared rather than per-panel: the GitHub inspector and the What's New screen scroll
/// the same way, and a second copy would be the first step to them drifting apart.
pub(crate) fn draw_scrollbar(
    f: &mut Frame,
    area: Rect,
    line_count: usize,
    viewport_height: usize,
    offset: usize,
    theme: Theme,
) {
    use ratatui::widgets::{Scrollbar, ScrollbarOrientation, ScrollbarState};

    if viewport_height == 0 || line_count <= viewport_height {
        return;
    }
    let scrollbar = Scrollbar::new(ScrollbarOrientation::VerticalRight)
        .begin_symbol(None)
        .end_symbol(None)
        .track_symbol(Some("│"))
        .track_style(Style::default().fg(theme.inactive))
        .thumb_symbol("█")
        .thumb_style(Style::default().fg(theme.accent));
    // With an explicit viewport length Ratatui expects the number of possible
    // positions, not the total line count. Passing `line_count` makes the thumb stop
    // early even after the text has reached its real maximum offset.
    let positions = line_count.saturating_sub(viewport_height).saturating_add(1);
    let mut state = ScrollbarState::new(positions)
        .position(offset)
        .viewport_content_length(viewport_height);
    f.render_stateful_widget(scrollbar, area, &mut state);
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::UserConfig;
    use crate::theme::ThemeName;
    use ratatui::backend::TestBackend;
    use ratatui::buffer::Buffer;
    use ratatui::Terminal;

    #[test]
    fn attached_workspace_places_scratchpad_right_and_terminal_below() {
        let layout = attached_layout(Rect::new(0, 0, 120, 40), true, true, true);
        let scratchpad = layout.scratchpad.unwrap();
        let terminal = layout.terminal.unwrap();

        assert_eq!(layout.status.y, 39);
        assert_eq!(scratchpad.x, layout.chat.right());
        assert_eq!(scratchpad.y, layout.chat.y);
        assert_eq!(terminal.y, layout.chat.bottom());
        assert_eq!(terminal.width, 120);
        assert_eq!(layout.chat.width + scratchpad.width, 120);
    }

    #[test]
    fn hidden_tools_give_the_chat_the_full_content_area() {
        let layout = attached_layout(Rect::new(0, 0, 100, 30), false, false, true);

        assert_eq!(layout.tabs, Rect::new(0, 0, 100, TAB_BAR_HEIGHT));
        assert_eq!(
            layout.chat,
            Rect::new(0, TAB_BAR_HEIGHT, 100, 30 - TAB_BAR_HEIGHT - 1)
        );
        assert_eq!(layout.status.y, 29);
        assert!(layout.scratchpad.is_none());
        assert!(layout.terminal.is_none());
    }

    #[test]
    fn the_tab_bar_sits_above_the_chat_and_never_overlaps_it() {
        let layout = attached_layout(Rect::new(0, 0, 120, 40), true, true, true);

        assert_eq!(layout.tabs.y, 0);
        assert_eq!(layout.tabs.height, TAB_BAR_HEIGHT);
        assert_eq!(layout.chat.y, layout.tabs.bottom());
        assert!(layout.tabs.bottom() <= layout.chat.y);
    }

    #[test]
    fn first_frame_says_the_rest_of_the_catalog_is_loading() {
        let mut app = App::new(Vec::new(), UserConfig::default());
        let (_sender, receiver) = std::sync::mpsc::channel();
        app.begin_session_load(receiver);
        let mut terminal = Terminal::new(TestBackend::new(120, 30)).unwrap();

        terminal.draw(|frame| draw(frame, &mut app)).unwrap();

        let text = terminal
            .backend()
            .buffer()
            .content()
            .iter()
            .map(|cell| cell.symbol())
            .collect::<String>();
        assert!(text.contains("0 sessions"), "got:\n{text}");
        assert!(text.contains("loading remaining sessions…"), "got:\n{text}");
        assert!(
            SPINNER_FRAMES.iter().any(|frame| text.contains(frame)),
            "got:\n{text}"
        );
    }

    #[test]
    fn update_confirmation_renders_above_full_screen_github_inspector() {
        let mut app = App::new(Vec::new(), UserConfig::default());
        let mut inspector = crate::app::GithubInspector::number_prompt();
        inspector.screen = crate::app::GithubInspectorScreen::Loading;
        app.github_inspector = Some(inspector);
        app.confirm_update_restart = true;
        let mut terminal = Terminal::new(TestBackend::new(120, 30)).unwrap();

        terminal.draw(|frame| draw(frame, &mut app)).unwrap();

        let text = terminal
            .backend()
            .buffer()
            .content()
            .iter()
            .map(|cell| cell.symbol())
            .collect::<String>();
        assert!(text.contains("Update & Restart"), "got:\n{text}");
    }

    #[test]
    fn global_settings_render_above_full_screen_github_inspector() {
        let mut app = App::new(Vec::new(), UserConfig::default());
        let mut inspector = crate::app::GithubInspector::number_prompt();
        inspector.screen = crate::app::GithubInspectorScreen::Loading;
        app.github_inspector = Some(inspector);
        app.mode = Mode::Settings;
        let mut terminal = Terminal::new(TestBackend::new(120, 30)).unwrap();

        terminal.draw(|frame| draw(frame, &mut app)).unwrap();

        let text = terminal
            .backend()
            .buffer()
            .content()
            .iter()
            .map(|cell| cell.symbol())
            .collect::<String>();
        assert!(text.contains("Global Settings"), "got:\n{text}");
        assert!(text.contains("Theme"), "got:\n{text}");
    }

    #[test]
    fn spinner_advances_through_all_frames() {
        for (tick, frame) in SPINNER_FRAMES.iter().enumerate() {
            assert_eq!(spinner_frame_at(tick as u128), *frame);
        }
        assert_eq!(
            spinner_frame_at(SPINNER_FRAMES.len() as u128),
            SPINNER_FRAMES[0]
        );
    }

    #[test]
    fn classic_row_selection_preserves_the_legacy_white_on_dark_gray_style() {
        let style = row_selection_style(ThemeName::Classic.theme());
        assert_eq!(style.fg, Some(Color::White));
        assert_eq!(style.bg, Some(Color::DarkGray));
        assert!(style.add_modifier.contains(Modifier::BOLD));
    }

    fn find_text(buffer: &Buffer, needle: &str) -> (u16, u16) {
        let width = needle.chars().count() as u16;
        for y in buffer.area.top()..buffer.area.bottom() {
            for x in buffer.area.left()..=buffer.area.right().saturating_sub(width) {
                let rendered = (x..x + width)
                    .map(|column| buffer[(column, y)].symbol())
                    .collect::<String>();
                if rendered == needle {
                    return (x, y);
                }
            }
        }
        panic!("{needle:?} was not rendered");
    }

    fn contrast_ratio(foreground: Color, background: Color) -> f64 {
        fn luminance(color: Color) -> f64 {
            let Color::Rgb(red, green, blue) = color else {
                panic!("expected an RGB color, got {color:?}");
            };
            let channel = |value: u8| {
                let value = f64::from(value) / 255.0;
                if value <= 0.04045 {
                    value / 12.92
                } else {
                    ((value + 0.055) / 1.055).powf(2.4)
                }
            };
            0.2126 * channel(red) + 0.7152 * channel(green) + 0.0722 * channel(blue)
        }

        let foreground = luminance(foreground);
        let background = luminance(background);
        (foreground.max(background) + 0.05) / (foreground.min(background) + 0.05)
    }

    #[test]
    fn light_themes_paint_blank_cells_and_render_primary_text_with_contrast() {
        for theme_name in [ThemeName::CatppuccinLatte, ThemeName::SolarizedLight] {
            let mut app = App::new(
                Vec::new(),
                UserConfig {
                    theme: theme_name,
                    ..UserConfig::default()
                },
            );
            let mut terminal = Terminal::new(TestBackend::new(100, 24)).unwrap();

            terminal.draw(|frame| draw(frame, &mut app)).unwrap();

            let buffer = terminal.backend().buffer();
            assert!(
                buffer.content().iter().all(|cell| cell.bg != Color::Reset),
                "{} left terminal-default background cells",
                theme_name.label()
            );
            let theme = theme_name.theme();
            let blank = &buffer[(70, 10)];
            assert_eq!(blank.symbol(), " ");
            assert_eq!(blank.bg, theme.background);

            let (x, y) = find_text(buffer, "0 sessions");
            let text = &buffer[(x, y)];
            assert_eq!(text.fg, theme.text);
            assert_eq!(text.bg, theme.background);
            assert!(
                contrast_ratio(text.fg, text.bg) >= 4.5,
                "{} rendered primary text at insufficient contrast",
                theme_name.label()
            );
        }
    }
}

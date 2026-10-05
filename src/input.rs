#[cfg(test)]
use crate::app::SettingsSection;
use crate::app::{
    App, DeleteTarget, Mode, NewSessionRequest, PendingWorktree, SettingsEditField, TakeoverTarget,
    View, WorktreeLaunchTarget,
};
use crate::config;
use crate::session::loader;
use crate::session::manager;
use crate::session::tmux;
use crate::session::worktree;
use crossterm::event::{
    self, Event, KeyCode, KeyEventKind, KeyModifiers, MouseButton, MouseEventKind,
};
use std::path::{Path, PathBuf};

pub fn handle_input(app: &mut App) -> anyhow::Result<bool> {
    if !event::poll(std::time::Duration::from_millis(100))? {
        return Ok(false);
    }

    let event = event::read()?;
    handle_terminal_event(app, event)?;
    Ok(true)
}

pub(crate) fn handle_end_tmux_confirm(app: &mut App, key: KeyCode) {
    let Some(reference) = app.confirm_end_tmux.clone() else {
        return;
    };
    if !matches!(
        key,
        KeyCode::Char('y') | KeyCode::Char('Y') | KeyCode::Enter
    ) {
        app.confirm_end_tmux = None;
        app.status_message = Some("Persistent session kept running".to_string());
        return;
    }

    let pane_id = app
        .mux
        .as_ref()
        .and_then(|mux| mux.pane_for_session(&reference.session_id));
    if let Some(pane_id) = pane_id {
        if !app.forget_workspace_panels(pane_id) {
            app.confirm_end_tmux = None;
            return;
        }
    }
    match tmux::kill(&reference) {
        Ok(()) => {
            if let (Some(mux), Some(pane_id)) = (app.mux.as_mut(), pane_id) {
                mux.remove(pane_id);
            }
            app.forget_tmux_session(&reference.session_id);
            if let Some(session) = app
                .sessions
                .iter_mut()
                .find(|session| session.id == reference.session_id)
            {
                session.is_active = false;
            }
            app.status_message = Some(format!(
                "Ended persistent tmux session '{}'",
                reference.tmux_session
            ));
            crate::mux_input::sync_workspace_panels(app);
            crate::mux_input::sync_view(app);
        }
        Err(error) => {
            app.confirm_end_tmux = None;
            app.status_message = Some(format!("Cannot end persistent tmux session: {error}"));
        }
    }
}

pub(crate) fn request_end_tmux_session(app: &mut App, session_id: &str) {
    let Some(reference) = app.tmux_session_for(session_id).cloned() else {
        app.status_message = Some("This session is not backed by tmux".to_string());
        return;
    };
    app.confirm_end_tmux = Some(reference);
}

fn request_end_selected_tmux_session(app: &mut App) {
    let Some(session_id) = app.selected_session().map(|session| session.id.clone()) else {
        return;
    };
    request_end_tmux_session(app, &session_id);
}

/// Apply an already-read terminal event to the session list UI.
///
/// Split out from `handle_input` so the multiplexer's event thread — which owns the
/// only reader of the terminal — can route events here instead of polling separately.
pub fn handle_terminal_event(app: &mut App, event: Event) -> anyhow::Result<()> {
    if app.confirm_end_tmux.is_some() {
        if let Event::Key(key) = event {
            if key.kind == KeyEventKind::Press {
                handle_end_tmux_confirm(app, key.code);
            }
        }
        return Ok(());
    }

    if app.confirm_update_restart {
        if let Event::Key(key) = event {
            if key.kind == KeyEventKind::Press {
                handle_update_restart_confirm(app, key.code);
            }
        }
        return Ok(());
    }

    if app.confirm_quit {
        if let Event::Key(key) = event {
            if key.kind == KeyEventKind::Press {
                handle_quit_confirm(app, key.code);
            }
        }
        return Ok(());
    }

    if thread_inbox_active(app) {
        handle_thread_inbox_event(app, event);
        return Ok(());
    }

    // Ahead of everything else it can cover, so a keystroke meant to dismiss it
    // cannot reach the view underneath.
    if whats_new_active(app) {
        handle_whats_new_event(app, event);
        return Ok(());
    }

    if app.command_palette.is_some() {
        crate::mux_input::handle_command_palette_event(app, event);
        return Ok(());
    }

    if portable_modal_active(app) {
        handle_portable_modal_event(app, event);
        return Ok(());
    }

    if app.workspace_help.is_some() {
        if matches!(
            event,
            Event::Key(crossterm::event::KeyEvent {
                code: KeyCode::Esc,
                kind: KeyEventKind::Press,
                ..
            })
        ) {
            app.workspace_help = None;
        }
        return Ok(());
    }

    if app.mode == Mode::Scratchpad {
        handle_scratchpad(app, event);
        return Ok(());
    }

    // Replay a paste as typing, but only into a prompt that is actually collecting
    // text — in Normal mode every character is a command, so pasting must not run one.
    // These prompts are all single-line, so line breaks are dropped rather than sent.
    if let Event::Paste(text) = &event {
        if matches!(app.mode, Mode::Search | Mode::Rename | Mode::FilterProject) {
            for character in text.chars().filter(|character| !character.is_control()) {
                let key =
                    crossterm::event::KeyEvent::new(KeyCode::Char(character), KeyModifiers::NONE);
                handle_terminal_event(app, Event::Key(key))?;
            }
        }
        return Ok(());
    }

    let Event::Key(key) = event else {
        return Ok(());
    };

    if key.kind != KeyEventKind::Press {
        return Ok(());
    }

    if key.code == KeyCode::Char('c') && key.modifiers.contains(KeyModifiers::CONTROL) {
        request_quit(app);
        return Ok(());
    }

    // The prefix works from the session list too, so panes stay reachable after
    // detaching without having to re-resume a session from the list.
    if matches!(app.mode, Mode::Normal) && crate::mux_input::handle_list_prefix(app, key) {
        return Ok(());
    }

    match app.mode {
        Mode::Normal => handle_normal(app, key.code),
        Mode::Search => handle_search(app, key.code),
        Mode::Rename => handle_rename(app, key.code),
        Mode::ConfirmDelete => handle_confirm_delete(app, key.code),
        Mode::ConfirmForceDelete => handle_confirm_force_delete(app, key.code),
        Mode::ConfirmTakeover => handle_confirm_takeover(app, key.code),
        Mode::FavoriteOpen => handle_favorite_open(app, key.code),
        Mode::NewSessionKind => handle_new_session_kind(app, key.code),
        Mode::FilterProject => handle_filter_project(app, key.code),
        Mode::Help => handle_help(app, key.code),
        Mode::Settings => handle_settings(app, key.code),
        Mode::ProjectSettings => handle_project_settings(app, key.code),
        Mode::BranchName => handle_branch_name(app, key.code),
        Mode::PaneList => handle_pane_list(app, key.code),
        Mode::Scratchpad => unreachable!(),
    }

    Ok(())
}

/// Quitting kills every pane, so confirm while sessions are still running.
pub(crate) fn request_quit(app: &mut App) {
    let running = app
        .mux
        .as_mut()
        .map(|mux| {
            mux.reap();
            mux.running_count()
        })
        .unwrap_or(0);
    if running > 0 {
        app.confirm_quit = true;
    } else {
        app.should_quit = true;
    }
}

pub(crate) fn handle_quit_confirm(app: &mut App, key: KeyCode) {
    match key {
        KeyCode::Char('y') | KeyCode::Char('Y') => {
            app.confirm_quit = false;
            app.should_quit = true;
        }
        _ => {
            app.confirm_quit = false;
            app.status_message = Some("Quit cancelled".to_string());
        }
    }
}

pub(crate) fn handle_update_restart_confirm(app: &mut App, key: KeyCode) {
    match key {
        KeyCode::Char('y') | KeyCode::Char('Y') | KeyCode::Enter => {
            app.confirm_update_and_restart();
        }
        KeyCode::Char('n') | KeyCode::Char('N') | KeyCode::Esc => {
            app.cancel_update_restart();
        }
        _ => {}
    }
}

pub(crate) fn whats_new_active(app: &App) -> bool {
    app.whats_new.is_some()
}

pub(crate) fn thread_inbox_active(app: &App) -> bool {
    app.thread_inbox.is_some()
}

/// Route the thread inbox.
///
/// Swallows every key it does not use, for the same reason as the What's New screen:
/// this can be open over a workspace full of live Copilot panes, and a stray keystroke
/// reaching one of them would be worse than a keystroke that does nothing. Accepting an
/// item is the only thing here that starts anything, and it takes a deliberate Enter.
pub(crate) fn handle_thread_inbox_event(app: &mut App, event: Event) {
    match event {
        Event::Key(key) if key.kind == KeyEventKind::Press => match key.code {
            KeyCode::Esc | KeyCode::Char('q') => app.thread_inbox = None,
            KeyCode::Up | KeyCode::Char('k') => app.thread_inbox_move(-1),
            KeyCode::Down | KeyCode::Char('j') => app.thread_inbox_move(1),
            KeyCode::Char('d') => app.thread_inbox_dismiss(),
            KeyCode::Enter => {
                if let Err(error) = app.thread_inbox_accept() {
                    app.status_message = Some(format!("Could not open the session: {error}"));
                }
            }
            _ => {}
        },
        Event::Mouse(mouse) => match mouse.kind {
            crossterm::event::MouseEventKind::ScrollUp => app.thread_inbox_move(-1),
            crossterm::event::MouseEventKind::ScrollDown => app.thread_inbox_move(1),
            _ => {}
        },
        _ => {}
    }
}

/// Route the What's New screen.
///
/// Every key it does not use is swallowed rather than passed along. This can be on
/// screen over a workspace that a post-update restart has just reopened, and a stray
/// keystroke reaching a Copilot pane would be worse than one that does nothing. The
/// footer says how to close it.
pub(crate) fn handle_whats_new_event(app: &mut App, event: Event) {
    let page = app
        .whats_new
        .as_ref()
        // A page is the whole remaining scroll, capped so a very long changelog still
        // pages rather than jumping to the end.
        .map(|screen| screen.max_scroll.clamp(1, 10) as isize)
        .unwrap_or(1);

    match event {
        Event::Key(key) if key.kind == KeyEventKind::Press => {
            if key.code == KeyCode::Char('c') && key.modifiers.contains(KeyModifiers::CONTROL) {
                request_quit(app);
                return;
            }
            match key.code {
                KeyCode::Esc | KeyCode::Enter | KeyCode::Char('q') | KeyCode::Char(' ') => {
                    app.whats_new = None;
                }
                KeyCode::Up | KeyCode::Char('k') => scroll_whats_new(app, -1),
                KeyCode::Down | KeyCode::Char('j') => scroll_whats_new(app, 1),
                KeyCode::PageUp => scroll_whats_new(app, -page),
                KeyCode::PageDown => scroll_whats_new(app, page),
                KeyCode::Home => scroll_whats_new(app, isize::MIN / 2),
                KeyCode::End => {
                    if let Some(screen) = app.whats_new.as_mut() {
                        screen.scroll_to_end();
                    }
                }
                _ => {}
            }
        }
        Event::Mouse(mouse) => match mouse.kind {
            crossterm::event::MouseEventKind::ScrollUp => scroll_whats_new(app, -3),
            crossterm::event::MouseEventKind::ScrollDown => scroll_whats_new(app, 3),
            _ => {}
        },
        _ => {}
    }
}

fn scroll_whats_new(app: &mut App, amount: isize) {
    if let Some(screen) = app.whats_new.as_mut() {
        screen.scroll_by(amount);
    }
}

pub(crate) fn portable_modal_active(app: &App) -> bool {
    matches!(
        app.mode,
        Mode::Help | Mode::Settings | Mode::ProjectSettings | Mode::BranchName | Mode::PaneList
    )
}

/// Route a list-style modal without changing the underlying list, pane, or inspector.
pub(crate) fn handle_portable_modal_event(app: &mut App, event: Event) {
    if !portable_modal_active(app) {
        return;
    }

    if let Event::Key(key) = &event {
        if key.kind != KeyEventKind::Press {
            return;
        }
        if key.code == KeyCode::Char('c') && key.modifiers.contains(KeyModifiers::CONTROL) {
            request_quit(app);
            return;
        }
    }

    if let Event::Mouse(mouse) = event {
        if app.mode == Mode::Help {
            match mouse.kind {
                MouseEventKind::ScrollUp => handle_help(app, KeyCode::Up),
                MouseEventKind::ScrollDown => handle_help(app, KeyCode::Down),
                _ => {}
            }
        } else if app.mode == Mode::Settings && app.theme_picker.is_some() {
            match mouse.kind {
                MouseEventKind::ScrollUp => app.move_theme_picker(-1),
                MouseEventKind::ScrollDown => app.move_theme_picker(1),
                MouseEventKind::Down(MouseButton::Left) => {
                    if app.select_theme_picker_at(mouse.column, mouse.row) {
                        if let Err(error) = app.confirm_theme_picker() {
                            app.status_message = Some(format!("Failed to save theme: {error}"));
                        }
                    }
                }
                _ => {}
            }
        }
        return;
    }

    if let Event::Paste(text) = event {
        let text = text
            .chars()
            .filter(|character| !character.is_control())
            .collect::<String>();
        match app.mode {
            Mode::Settings if app.settings_editing.is_some() => app.settings_input.push_str(&text),
            Mode::ProjectSettings if app.project_settings_editing => {
                app.project_settings_input.push_str(&text);
            }
            Mode::BranchName => app.branch_input.push_str(&text),
            _ => {}
        }
        return;
    }

    let Event::Key(key) = event else {
        return;
    };
    if key.kind != KeyEventKind::Press {
        return;
    }
    match app.mode {
        Mode::Help => handle_help(app, key.code),
        Mode::Settings => handle_settings(app, key.code),
        Mode::ProjectSettings => handle_project_settings(app, key.code),
        Mode::BranchName => handle_branch_name(app, key.code),
        Mode::PaneList => handle_pane_list(app, key.code),
        _ => {}
    }
}

/// Pane switcher: attach, kill, or dismiss without touching the underlying session list.
fn handle_pane_list(app: &mut App, key: KeyCode) {
    let Some(count) = app.mux.as_ref().map(|mux| mux.panes.len()) else {
        app.mode = Mode::Normal;
        return;
    };
    if count == 0 {
        app.mode = Mode::Normal;
        return;
    }

    match key {
        KeyCode::Up | KeyCode::Char('k') => {
            app.pane_selected = app.pane_selected.saturating_sub(1);
        }
        KeyCode::Down | KeyCode::Char('j') => {
            app.pane_selected = (app.pane_selected + 1).min(count - 1);
        }
        KeyCode::Char(digit @ '1'..='9') => {
            let index = digit as usize - '1' as usize;
            if index < count {
                app.pane_selected = index;
            }
        }
        KeyCode::Enter => {
            let index = app.pane_selected.min(count - 1);
            let mux = app.mux.as_mut().expect("mux checked above");
            mux.select_index(index);
            let id = mux.panes[index].id;
            app.mode = Mode::Normal;
            app.view = View::Attached(id);
            crate::mux_input::sync_workspace_panels(app);
        }
        KeyCode::Char('x') => {
            let index = app.pane_selected.min(count - 1);
            let (id, title, persistent) = app
                .mux
                .as_ref()
                .and_then(|mux| mux.panes.get(index))
                .map(|pane| (pane.id, pane.title.clone(), app.pane_is_tmux_backed(pane)))
                .expect("pane index checked above");
            if !app.forget_workspace_panels(id) {
                return;
            }
            let mux = app.mux.as_mut().expect("mux checked above");
            mux.remove(id);
            app.pane_selected = app.pane_selected.min(mux.panes.len().saturating_sub(1));
            if mux.panes.is_empty() {
                app.mode = Mode::Normal;
            }
            app.view = View::List;
            app.status_message = Some(if persistent {
                format!("Detached '{title}'; tmux session is still running")
            } else {
                format!("Ended '{title}'")
            });
        }
        key if app.config.tmux_keys.matches_end_session(key) => {
            let index = app.pane_selected.min(count - 1);
            let session_id = app
                .mux
                .as_ref()
                .and_then(|mux| mux.panes.get(index))
                .map(|pane| pane.session_id.clone())
                .expect("pane index checked above");
            request_end_tmux_session(app, &session_id);
        }
        KeyCode::Esc | KeyCode::Char('q') => {
            app.mode = Mode::Normal;
        }
        _ => {}
    }
}

fn handle_normal(app: &mut App, key: KeyCode) {
    // A grabbed favorite takes over the arrow keys, so this runs first.
    if app.grabbed_favorite.is_some() {
        match key {
            KeyCode::Up | KeyCode::Char('k') => {
                if let Some(message) = app.move_grabbed_favorite(-1) {
                    app.status_message = Some(message);
                }
                return;
            }
            KeyCode::Down | KeyCode::Char('j') => {
                if let Some(message) = app.move_grabbed_favorite(1) {
                    app.status_message = Some(message);
                }
                return;
            }
            KeyCode::Enter | KeyCode::Esc | KeyCode::Char('g') | KeyCode::Char(' ') => {
                app.release_favorite_grab();
                app.status_message = Some("Order saved".to_string());
                return;
            }
            // Anything else drops the item first, so no command runs while the
            // list is in a half-rearranged state.
            _ => {
                app.release_favorite_grab();
            }
        }
    }

    if app.config.tmux_keys.matches_new_session(key) {
        start_tmux_session(app);
        return;
    }
    if app.config.tmux_keys.matches_resume_session(key) {
        resume_selected_in_tmux(app);
        return;
    }
    if app.config.tmux_keys.matches_new_worktree(key) {
        begin_tmux_worktree_session(app);
        return;
    }
    if app.config.tmux_keys.matches_end_session(key) {
        request_end_selected_tmux_session(app);
        return;
    }

    match key {
        KeyCode::Char('q') | KeyCode::Esc => {
            request_quit(app);
        }
        KeyCode::Up | KeyCode::Char('k') => app.move_up(),
        KeyCode::Down | KeyCode::Char('j') => app.move_down(),
        KeyCode::Home => {
            app.selected = 0;
            app.scroll_offset = 0;
        }
        KeyCode::End => {
            if !app.filtered_indices.is_empty() {
                app.selected = app.filtered_indices.len() - 1;
                if app.selected >= app.visible_rows {
                    app.scroll_offset = app.selected - app.visible_rows + 1;
                }
            }
        }
        KeyCode::Enter => resume_selected(app),
        KeyCode::Char('/') => {
            app.mode = Mode::Search;
            app.search_query.clear();
        }
        KeyCode::Char('r') => {
            if let Some(session) = app.selected_session() {
                app.rename_input = session.summary.clone().unwrap_or_default();
                app.mode = Mode::Rename;
            }
        }
        KeyCode::Char('e') => open_scratchpad(app),
        KeyCode::Char('g') => {
            if let Some(message) = app.toggle_favorite_grab() {
                app.status_message = Some(message);
            }
        }
        KeyCode::Char('T') => open_favorite_tabs(app),
        KeyCode::Char(' ') => match app.toggle_selected_favorite() {
            Ok(Some(true)) => app.status_message = Some("Added to favorites".to_string()),
            Ok(Some(false)) => app.status_message = Some("Removed from favorites".to_string()),
            Ok(None) => {}
            Err(error) => {
                app.status_message = Some(format!("Failed to save favorite: {error}"));
            }
        },
        KeyCode::Char('d') => begin_delete(app),
        KeyCode::Char('f') | KeyCode::Char('p') => {
            app.project_selected = 0;
            app.project_scroll_offset = 0;
            app.project_search_query.clear();
            app.mode = Mode::FilterProject;
        }
        KeyCode::Char('s') => {
            app.cycle_sort();
            app.status_message = Some(format!("Sorted by: {}", app.sort_label()));
        }
        KeyCode::Char('H') => {
            if app.config.hidden_title_prefixes.is_empty()
                && app.config.hidden_path_prefixes.is_empty()
            {
                app.status_message =
                    Some("Configure hidden session filters in Global Settings".to_string());
            } else if app.toggle_hidden_sessions() {
                app.status_message = Some("Showing hidden sessions for this run".to_string());
            } else {
                app.status_message = Some("Hidden session filters reapplied".to_string());
            }
        }
        KeyCode::Char('c') => {
            app.set_project_filter(None);
            app.status_message = Some("Filter cleared".to_string());
        }
        KeyCode::Char('n') => begin_new_session_choice(app),
        KeyCode::Char('N') => begin_worktree_session(app),
        KeyCode::Char('?') => {
            app.help_scroll = 0;
            app.mode = Mode::Help;
        }
        KeyCode::Char(',') => {
            app.begin_global_settings();
        }
        KeyCode::Char('.') => begin_project_settings(app),
        KeyCode::Char('u') => app.request_update(),
        _ => {}
    }
}

pub(crate) fn execute_palette_list_command(
    app: &mut App,
    command: crate::command_palette::CommandId,
) {
    use crate::command_palette::CommandId;
    match command {
        CommandId::NewTmuxSession => {
            start_tmux_session(app);
            return;
        }
        CommandId::ResumeSelectedInTmux => {
            resume_selected_in_tmux(app);
            return;
        }
        CommandId::NewTmuxWorktreeSession => {
            begin_tmux_worktree_session(app);
            return;
        }
        CommandId::EndPersistentSession => {
            request_end_selected_tmux_session(app);
            return;
        }
        _ => {}
    }
    let key = match command {
        CommandId::ResumeSelected => KeyCode::Enter,
        CommandId::NewSession => KeyCode::Char('n'),
        CommandId::NewWorktreeSession => KeyCode::Char('N'),
        CommandId::OpenSelectedScratchpad => KeyCode::Char('e'),
        CommandId::ToggleFavorite => KeyCode::Char(' '),
        CommandId::ReorderFavorite => KeyCode::Char('g'),
        CommandId::OpenFavoriteTabs => KeyCode::Char('T'),
        CommandId::RenameSelected => KeyCode::Char('r'),
        CommandId::DeleteSelected => KeyCode::Char('d'),
        CommandId::SearchSessions => KeyCode::Char('/'),
        CommandId::FilterProject => KeyCode::Char('f'),
        CommandId::ClearProjectFilter => KeyCode::Char('c'),
        CommandId::CycleSort => KeyCode::Char('s'),
        CommandId::ToggleHiddenSessions => KeyCode::Char('H'),
        CommandId::GlobalSettings => KeyCode::Char(','),
        CommandId::ProjectSettings => KeyCode::Char('.'),
        CommandId::OpenHelp => KeyCode::Char('?'),
        _ => return,
    };
    handle_normal(app, key);
}

fn open_favorite_tabs(app: &mut App) {
    app.mode = Mode::FavoriteOpen;
}

/// Choose where the favorites open. Both destinations are always offered so the key
/// behaves the same regardless of which terminal CST was launched from.
pub(crate) fn handle_favorite_open(app: &mut App, key: KeyCode) {
    match key {
        KeyCode::Char('p') | KeyCode::Char('P') => {
            app.mode = Mode::Normal;
            if app.mux.is_none() {
                app.status_message =
                    Some("Panes need mux mode; restart CST with --mux".to_string());
                return;
            }
            app.status_message = Some(app.open_favorites_as_panes());
            // The same follow-up resuming a single session does: move the scratchpad
            // and terminal panels onto the newly focused pane and acknowledge it.
            if matches!(app.view, crate::app::View::Attached(_)) {
                crate::mux_input::sync_workspace_panels(app);
            }
        }
        KeyCode::Char('t') | KeyCode::Char('T') => {
            app.mode = Mode::Normal;
            app.status_message = Some(
                match crate::windows_terminal::launch_favorites(
                    &app.sessions,
                    &app.config,
                    &app.copilot_home,
                    None,
                ) {
                    Ok(report) => report.status_message(),
                    Err(error) => format!("Cannot open favorite tabs: {error}"),
                },
            );
        }
        KeyCode::Esc => {
            app.mode = Mode::Normal;
        }
        _ => {}
    }
}

fn resume_selected(app: &mut App) {
    resume_selected_with_host(app, false);
}

fn resume_selected_in_tmux(app: &mut App) {
    if let Err(error) = tmux::check_available() {
        app.status_message = Some(format!("Cannot resume in tmux: {error}"));
        return;
    }
    resume_selected_with_host(app, true);
}

fn resume_selected_with_host(app: &mut App, resume_in_tmux: bool) {
    let Some(session) = app.selected_session() else {
        return;
    };
    let (id, cwd, title, dir_path) = (
        session.id.clone(),
        session.cwd.clone(),
        session.display_name().to_string(),
        session.dir_path.clone(),
    );

    if app.mux_enabled() {
        // A pane we own is not "busy elsewhere" — re-focus it instead of refusing.
        if app.pane_for_session(&id).is_some() {
            if resume_in_tmux && !app.session_is_tmux_backed(&id) {
                app.status_message =
                    Some("Close the direct session tab before resuming it in tmux".to_string());
                return;
            }
            resume_target(app, id, cwd, title);
            return;
        }
    }

    match tmux::find_live(&id) {
        Ok(Some(reference)) => {
            if app.mux_enabled() {
                match app.attach_tmux_session(reference, title) {
                    Ok(()) => crate::mux_input::sync_workspace_panels(app),
                    Err(error) => {
                        app.status_message = Some(format!("Cannot attach tmux session: {error}"))
                    }
                }
            } else {
                app.should_attach_tmux = Some(reference);
            }
            return;
        }
        Ok(None) => {}
        Err(error) => {
            app.status_message = Some(format!("Cannot inspect tmux session ownership: {error}"));
            return;
        }
    }

    let pids = crate::session::process::active_session_pids(&dir_path);
    if !pids.is_empty() {
        app.pending_takeover = Some(TakeoverTarget {
            id,
            cwd,
            title,
            dir_path,
            pids,
            resume_in_tmux,
        });
        app.mode = Mode::ConfirmTakeover;
        return;
    }
    mark_session_inactive(app, &id);
    if resume_in_tmux {
        resume_target_in_tmux(app, id, cwd, title);
    } else {
        resume_target(app, id, cwd, title);
    }
}

fn resume_target(app: &mut App, id: String, cwd: String, title: String) {
    if app.mux_enabled() {
        match app.attach_session(&id, &cwd, title) {
            Ok(()) => crate::mux_input::sync_workspace_panels(app),
            Err(error) => app.status_message = Some(format!("Cannot attach session: {error}")),
        }
    } else {
        app.should_resume = Some((id, cwd));
    }
}

fn resume_target_in_tmux(app: &mut App, id: String, cwd: String, title: String) {
    if app.mux_enabled() {
        app.status_message = Some(
            match manager::resume_tmux_session(&id, Path::new(&cwd), &title, &app.config) {
                Ok(launched) => complete_tmux_launch(app, launched, title, "Resumed session"),
                Err(error) => format!("Cannot resume in tmux: {error}"),
            },
        );
    } else {
        app.should_new_session = Some(NewSessionRequest::TmuxResume {
            session_id: id,
            cwd,
            title,
        });
    }
}

fn mark_session_inactive(app: &mut App, id: &str) {
    if let Some(session) = app.sessions.iter_mut().find(|session| session.id == id) {
        session.is_active = false;
    }
}

fn handle_confirm_takeover(app: &mut App, key: KeyCode) {
    match key {
        KeyCode::Char('y') | KeyCode::Char('Y') | KeyCode::Enter => {
            let Some(target) = app.pending_takeover.take() else {
                app.mode = Mode::Normal;
                return;
            };
            app.mode = Mode::Normal;
            match crate::session::process::terminate_session(&target.dir_path, &target.pids) {
                Ok(_) if !loader::session_is_active(&target.dir_path) => {
                    mark_session_inactive(app, &target.id);
                    if target.resume_in_tmux {
                        resume_target_in_tmux(app, target.id, target.cwd, target.title);
                    } else {
                        resume_target(app, target.id, target.cwd, target.title);
                    }
                }
                Ok(_) => {
                    app.status_message = Some(
                        "Takeover failed: the previous Copilot process is still active".into(),
                    );
                }
                Err(error) => {
                    app.status_message = Some(format!("Takeover failed: {error}"));
                }
            }
        }
        KeyCode::Char('n') | KeyCode::Char('N') | KeyCode::Esc => {
            app.pending_takeover = None;
            app.mode = Mode::Normal;
            app.status_message = Some("Takeover cancelled".to_string());
        }
        _ => {}
    }
}

/// Short, human-readable pane label derived from a project directory.
fn project_title(cwd: &str) -> String {
    Path::new(cwd)
        .file_name()
        .and_then(|name| name.to_str())
        .unwrap_or(cwd)
        .to_string()
}

fn begin_delete(app: &mut App) {
    let Some(session) = app.selected_session() else {
        return;
    };
    if app.session_is_tmux_backed(&session.id) {
        app.status_message =
            Some("End the persistent tmux session first with X, then delete it".to_string());
        return;
    }
    if session.is_active || loader::session_is_active(&session.dir_path) {
        app.status_message = Some("Cannot delete: session is currently active".to_string());
        return;
    }

    let cwd = session.cwd.clone();

    match worktree::managed_worktree_for_cwd(Path::new(&cwd)) {
        Ok(Some(entry)) => match worktree::is_dirty(&entry) {
            Ok(dirty) => {
                app.pending_delete = Some(DeleteTarget::Managed { entry, dirty });
                app.mode = Mode::ConfirmDelete;
            }
            Err(error) => {
                app.status_message = Some(format!("Cannot inspect worktree: {error}"));
            }
        },
        Ok(None) => {
            app.pending_delete = Some(DeleteTarget::SessionOnly);
            app.mode = Mode::ConfirmDelete;
        }
        Err(error) => {
            app.status_message = Some(format!("Cannot inspect worktree registry: {error}"));
        }
    }
}

fn open_scratchpad(app: &mut App) {
    let Some(session) = app.selected_session() else {
        return;
    };
    let session_id = session.id.clone();
    match crate::scratchpad::Scratchpad::open(&session_id) {
        Ok(scratchpad) => {
            app.scratchpad = Some(scratchpad);
            app.scratchpad_owner = None;
            app.mode = Mode::Scratchpad;
        }
        Err(error) => {
            app.status_message = Some(format!("Cannot open scratchpad: {error}"));
        }
    }
}

fn handle_scratchpad(app: &mut App, event: Event) {
    let Some(scratchpad) = app.scratchpad.as_mut() else {
        app.mode = Mode::Normal;
        return;
    };
    match scratchpad.handle_event(event) {
        Ok(crate::scratchpad::InputOutcome::Continue) => {}
        Ok(crate::scratchpad::InputOutcome::Close) => {
            app.scratchpad = None;
            app.scratchpad_owner = None;
            app.mode = Mode::Normal;
            app.status_message = Some("Scratchpad saved".to_string());
        }
        Err(error) => {
            scratchpad.status_message = Some(format!("Scratchpad save failed: {error}"));
        }
    }
}

pub(crate) fn start_tmux_session(app: &mut App) {
    if let Err(error) = tmux::check_available() {
        app.status_message = Some(format!("Cannot start tmux session: {error}"));
        return;
    }
    let Some(cwd) = app.new_session_dir() else {
        app.status_message =
            Some("Filter by a project first (f) to start a tmux session".to_string());
        return;
    };
    let title = project_title(&cwd);
    if !app.mux_enabled() {
        app.should_new_session = Some(NewSessionRequest::Tmux { cwd, title });
        return;
    }
    app.status_message = Some(
        match manager::start_tmux_session(Path::new(&cwd), &title, &app.config) {
            Ok(launched) => complete_tmux_launch(app, launched, title, "Started new session"),
            Err(error) => format!("Cannot start tmux session: {error}"),
        },
    );
}

fn complete_tmux_launch(
    app: &mut App,
    launched: manager::TmuxSessionRef,
    title: String,
    description: &str,
) -> String {
    app.track_external_session(launched.session_id.clone());
    let base = format!("{description} in tmux '{}'", launched.tmux_session);
    match app.attach_tmux_session(launched, title) {
        Ok(()) => format!("{base}; opened as a CST tab"),
        Err(error) => format!("{base}; could not open CST tab: {error}"),
    }
}

/// Ask whether the new session should live in the project as-is or in a worktree.
///
/// `N` used to carry the worktree variant on its own, but a second, shift-modified key
/// for a close sibling of `n` was easy to miss in the footer and easy to forget. One
/// key that asks is cheaper to discover than two keys to memorise; `N` still works as
/// an undocumented-in-the-footer shortcut that skips the question.
fn begin_new_session_choice(app: &mut App) {
    if app.new_session_dir().is_none() {
        app.status_message =
            Some("Filter by a project first (f) to start a new session".to_string());
        return;
    }
    app.mode = Mode::NewSessionKind;
}

pub(crate) fn handle_new_session_kind(app: &mut App, key: KeyCode) {
    match key {
        KeyCode::Char('n') | KeyCode::Enter => {
            app.mode = Mode::Normal;
            start_plain_new_session(app);
        }
        KeyCode::Char('w') | KeyCode::Char('W') => {
            app.mode = Mode::Normal;
            begin_worktree_session(app);
        }
        KeyCode::Esc | KeyCode::Char('q') => {
            app.mode = Mode::Normal;
        }
        _ => {}
    }
}

fn start_plain_new_session(app: &mut App) {
    let Some(cwd) = app.new_session_dir() else {
        app.status_message =
            Some("Filter by a project first (f) to start a new session".to_string());
        return;
    };
    if app.mux_enabled() {
        let title = project_title(&cwd);
        match app.attach_new_session(&cwd, title) {
            Ok(()) => crate::mux_input::sync_workspace_panels(app),
            Err(error) => app.status_message = Some(format!("Cannot start session: {error}")),
        }
    } else {
        app.should_new_session = Some(NewSessionRequest::Normal { cwd });
    }
}

pub(crate) fn begin_worktree_session(app: &mut App) {
    begin_worktree_session_for(app, WorktreeLaunchTarget::Standard);
}

pub(crate) fn begin_tmux_worktree_session(app: &mut App) {
    if let Err(error) = tmux::check_available() {
        app.status_message = Some(format!("Cannot start tmux session: {error}"));
        return;
    }
    begin_worktree_session_for(app, WorktreeLaunchTarget::Tmux);
}

fn begin_worktree_session_for(app: &mut App, target: WorktreeLaunchTarget) {
    let Some(project) = app.command_project() else {
        app.status_message =
            Some("Filter by a project first (f) to create an isolated session".to_string());
        return;
    };

    let repository = match worktree::resolve_repository_root(Path::new(&project)) {
        Ok(repository) => repository,
        Err(error) => {
            app.status_message = Some(format!("Cannot create worktree: {error}"));
            return;
        }
    };
    let effective = match config::effective_worktree(&app.config, &repository) {
        Ok(effective) => effective,
        Err(error) => {
            app.status_message = Some(error.to_string());
            return;
        }
    };

    app.branch_input = worktree::generated_branch_name(&effective.branch_prefix);
    app.branch_config = Some(effective);
    app.worktree_launch_target = target;
    app.mode = Mode::BranchName;
}

pub(crate) fn begin_project_settings(app: &mut App) {
    let Some(project) = app.command_project() else {
        app.status_message =
            Some("Filter by a project first (f) to edit project settings".to_string());
        return;
    };
    let repository = match worktree::resolve_repository_root(Path::new(&project)) {
        Ok(repository) => repository,
        Err(error) => {
            app.status_message = Some(format!(
                "Project settings require a Git repository: {error}"
            ));
            return;
        }
    };

    match config::ProjectSettings::load(&repository, &app.config) {
        Ok(settings) => {
            app.project_settings = Some(settings);
            app.project_settings_selected = 0;
            app.project_settings_editing = false;
            app.project_settings_input.clear();
            app.mode = Mode::ProjectSettings;
        }
        Err(error) => app.status_message = Some(error.to_string()),
    }
}

fn handle_search(app: &mut App, key: KeyCode) {
    match key {
        KeyCode::Esc => {
            app.mode = Mode::Normal;
            app.search_query.clear();
            app.apply_filter();
        }
        KeyCode::Enter => app.mode = Mode::Normal,
        KeyCode::Backspace => {
            app.search_query.pop();
            app.apply_filter();
        }
        KeyCode::Char(c) => {
            app.search_query.push(c);
            app.apply_filter();
        }
        KeyCode::Up => app.move_up(),
        KeyCode::Down => app.move_down(),
        _ => {}
    }
}

fn handle_rename(app: &mut App, key: KeyCode) {
    match key {
        KeyCode::Esc => app.mode = Mode::Normal,
        KeyCode::Enter => {
            if let Some(idx) = app.selected_real_index() {
                let dir = app.sessions[idx].dir_path.clone();
                let session_id = app.sessions[idx].id.clone();
                let new_name = app.rename_input.clone();
                let renamed_tmux = match app.tmux_session_for(&session_id).cloned() {
                    Some(reference) => match tmux::rename(&reference, &new_name) {
                        Ok(renamed) => Some((reference, renamed)),
                        Err(error) => {
                            app.status_message = Some(format!("Rename failed: {error}"));
                            app.mode = Mode::Normal;
                            return;
                        }
                    },
                    None => None,
                };
                match manager::rename_session(&dir, &new_name) {
                    Ok(()) => {
                        app.sessions[idx].summary = Some(new_name);
                        if let Some((_, renamed)) = renamed_tmux {
                            app.replace_tmux_session(renamed);
                        }
                        app.status_message = Some("Session renamed".to_string());
                    }
                    Err(error) => {
                        let rollback_error = renamed_tmux.and_then(|(original, renamed)| {
                            tmux::restore_name(&renamed, &original).err()
                        });
                        app.status_message = Some(match rollback_error {
                            Some(rollback) => format!(
                                "Rename failed: {error}; tmux rename rollback also failed: {rollback}"
                            ),
                            None => format!("Rename failed: {error}"),
                        });
                    }
                }
            }
            app.mode = Mode::Normal;
        }
        KeyCode::Backspace => {
            app.rename_input.pop();
        }
        KeyCode::Char(c) => app.rename_input.push(c),
        _ => {}
    }
}

fn handle_confirm_delete(app: &mut App, key: KeyCode) {
    if matches!(key, KeyCode::Char('y') | KeyCode::Char('Y')) {
        if matches!(
            app.pending_delete,
            Some(DeleteTarget::Managed { dirty: true, .. })
        ) {
            app.mode = Mode::ConfirmForceDelete;
        } else {
            perform_delete(app, false);
        }
    } else {
        cancel_delete(app);
    }
}

fn handle_confirm_force_delete(app: &mut App, key: KeyCode) {
    if key == KeyCode::Char('Y') {
        perform_delete(app, true);
    } else {
        cancel_delete(app);
    }
}

fn perform_delete(app: &mut App, force: bool) {
    let Some(idx) = app.selected_real_index() else {
        cancel_delete(app);
        return;
    };
    if app.session_is_tmux_backed(&app.sessions[idx].id) {
        app.mode = Mode::Normal;
        app.pending_delete = None;
        app.status_message =
            Some("Cannot delete while the persistent tmux session is running".to_string());
        return;
    }
    if app.sessions[idx].is_active || loader::session_is_active(&app.sessions[idx].dir_path) {
        app.mode = Mode::Normal;
        app.pending_delete = None;
        app.status_message = Some("Cannot delete: session became active".to_string());
        return;
    }

    let dir = app.sessions[idx].dir_path.clone();
    let session_id = app.sessions[idx].id.clone();
    let target = app
        .pending_delete
        .clone()
        .unwrap_or(DeleteTarget::SessionOnly);
    if let Err(error) = app.terminal.remove(&session_id) {
        app.mode = Mode::Normal;
        app.pending_delete = None;
        app.status_message = Some(format!(
            "Cannot delete: the session terminal could not be stopped: {error}"
        ));
        return;
    }
    if app.terminal.active_session_id().is_none() {
        app.terminal_owner = None;
    }
    let result = match target {
        DeleteTarget::SessionOnly => {
            manager::delete_session(&dir).map(|_| "Session deleted".to_string())
        }
        DeleteTarget::Managed { entry, .. } => manager::delete_managed_session(&dir, &entry, force),
    };

    match result {
        Ok(message) => {
            app.sessions.remove(idx);
            let favorite_error = app.forget_favorite(&session_id).err();
            let scratchpad_error = crate::scratchpad::delete(&session_id).err();
            let workspace_error = crate::workspace_state::delete(&session_id).err();
            app.apply_filter();
            let mut errors = Vec::new();
            if let Some(error) = favorite_error {
                errors.push(format!("favorites: {error}"));
            }
            if let Some(error) = scratchpad_error {
                errors.push(format!("scratchpad: {error}"));
            }
            if let Some(error) = workspace_error {
                errors.push(format!("workspace state: {error}"));
            }
            app.status_message = Some(if errors.is_empty() {
                message
            } else {
                format!("{message}; cleanup failed for {}", errors.join(", "))
            });
        }
        Err(error) => {
            app.status_message = Some(format!("Delete failed: {error}"));
        }
    }
    app.pending_delete = None;
    app.mode = Mode::Normal;
}

fn cancel_delete(app: &mut App) {
    app.pending_delete = None;
    app.mode = Mode::Normal;
    app.status_message = Some("Delete cancelled".to_string());
}

fn handle_filter_project(app: &mut App, key: KeyCode) {
    let filtered = app.filtered_project_indices();
    let has_all_option = app.project_search_query.is_empty();
    let total = filtered.len() + usize::from(has_all_option);

    match key {
        KeyCode::Esc => app.mode = Mode::Normal,
        KeyCode::Up => {
            if app.project_selected > 0 {
                app.project_selected -= 1;
                if app.project_selected < app.project_scroll_offset {
                    app.project_scroll_offset = app.project_selected;
                }
            }
        }
        KeyCode::Down => {
            if app.project_selected + 1 < total {
                app.project_selected += 1;
                if app.project_selected >= app.project_scroll_offset + app.project_visible_rows {
                    app.project_scroll_offset = app.project_selected - app.project_visible_rows + 1;
                }
            }
        }
        KeyCode::Enter => {
            if total == 0 {
                return;
            }
            if has_all_option && app.project_selected == 0 {
                app.set_project_filter(None);
                app.status_message = Some("Showing all projects".to_string());
            } else {
                let list_idx = if has_all_option {
                    app.project_selected - 1
                } else {
                    app.project_selected
                };
                if let Some(&project_index) = filtered.get(list_idx) {
                    let project = app.unique_projects[project_index].clone();
                    app.status_message = Some(format!("Filtered to: {project}"));
                    app.set_project_filter(Some(project));
                }
            }
            app.mode = Mode::Normal;
        }
        KeyCode::Backspace => {
            app.project_search_query.pop();
            app.project_selected = 0;
            app.project_scroll_offset = 0;
        }
        KeyCode::Char(c) => {
            app.project_search_query.push(c);
            app.project_selected = 0;
            app.project_scroll_offset = 0;
        }
        _ => {}
    }
}

fn handle_help(app: &mut App, key: KeyCode) {
    // Scrolling has to come first, or the keys that move through a help screen
    // taller than the popup would close it instead.
    match key {
        KeyCode::Up => {
            app.help_scroll = app.help_scroll.saturating_sub(1);
            return;
        }
        KeyCode::Down => {
            // The draw clamps this against the real content height.
            app.help_scroll = app.help_scroll.saturating_add(1);
            return;
        }
        KeyCode::PageUp => {
            app.help_scroll = app.help_scroll.saturating_sub(10);
            return;
        }
        KeyCode::PageDown => {
            app.help_scroll = app.help_scroll.saturating_add(10);
            return;
        }
        KeyCode::Home => {
            app.help_scroll = 0;
            return;
        }
        KeyCode::End => {
            // Clamped by the draw, which is the only place that knows the height.
            app.help_scroll = usize::MAX;
            return;
        }
        _ => {}
    }
    if matches!(
        key,
        KeyCode::Esc | KeyCode::Char('?') | KeyCode::Char('q') | KeyCode::Enter
    ) {
        app.mode = Mode::Normal;
    }
}

fn handle_settings(app: &mut App, key: KeyCode) {
    if app.theme_picker.is_some() {
        match key {
            KeyCode::Up => app.move_theme_picker(-1),
            KeyCode::Down => app.move_theme_picker(1),
            KeyCode::PageUp => app.move_theme_picker(-5),
            KeyCode::PageDown => app.move_theme_picker(5),
            KeyCode::Home => app.move_theme_picker(isize::MIN),
            KeyCode::End => app.move_theme_picker(isize::MAX),
            KeyCode::Enter => {
                if let Err(error) = app.confirm_theme_picker() {
                    app.status_message = Some(format!("Failed to save theme: {error}"));
                }
            }
            KeyCode::Esc => app.cancel_theme_picker(),
            _ => {}
        }
        return;
    }

    if let Some(field) = app.settings_editing {
        match key {
            KeyCode::Esc => {
                app.settings_editing = None;
                app.settings_input.clear();
            }
            KeyCode::Enter => commit_global_setting(app, field),
            KeyCode::Backspace => {
                app.settings_input.pop();
            }
            KeyCode::Char(character) => app.settings_input.push(character),
            _ => {}
        }
        return;
    }

    match key {
        KeyCode::Esc | KeyCode::Char(',') => {
            if let Err(error) = config::validate_tmux_keys(&app.config.tmux_keys) {
                app.status_message = Some(error.to_string());
                return;
            }
            if let Err(error) = config::validate_user_notification_config(&app.config) {
                app.status_message = Some(error.to_string());
                return;
            }
            match app.save_global_settings() {
                Ok(()) => {
                    app.mode = Mode::Normal;
                    app.status_message = Some("Global settings saved".to_string());
                }
                Err(error) => {
                    app.status_message = Some(format!("Failed to save global settings: {error}"));
                }
            }
        }
        KeyCode::Up | KeyCode::Char('k') => {
            let rows = app.settings_section.rows();
            let position = rows
                .iter()
                .position(|row| *row == app.settings_selected)
                .unwrap_or(0);
            app.settings_selected = rows[position.saturating_sub(1)];
        }
        KeyCode::Down | KeyCode::Char('j') => {
            let rows = app.settings_section.rows();
            let position = rows
                .iter()
                .position(|row| *row == app.settings_selected)
                .unwrap_or(0);
            app.settings_selected = rows[(position + 1).min(rows.len() - 1)];
        }
        KeyCode::Tab | KeyCode::Right => {
            app.settings_section = app.settings_section.next(true);
            app.settings_selected = app.settings_section.rows()[0];
        }
        KeyCode::BackTab | KeyCode::Left => {
            app.settings_section = app.settings_section.next(false);
            app.settings_selected = app.settings_section.rows()[0];
        }
        KeyCode::Enter | KeyCode::Char(' ') => match app.settings_selected {
            0 => {
                app.config.yolo = !app.config.yolo;
            }
            1 => begin_global_edit(
                app,
                SettingsEditField::Model,
                app.config.model.clone().unwrap_or_default(),
            ),
            2 => cycle_reasoning_effort(app),
            18 => begin_global_edit(
                app,
                SettingsEditField::MaxAutopilotContinues,
                app.config
                    .max_autopilot_continues
                    .map(|limit| limit.to_string())
                    .unwrap_or_default(),
            ),
            3 => app.open_theme_picker(),
            20 => begin_global_edit(
                app,
                SettingsEditField::ThreadTrustedAuthors,
                app.config.thread_trusted_authors.join(", "),
            ),
            21 => app.config.threads_enabled = !app.config.threads_enabled,
            22 => begin_global_edit(
                app,
                SettingsEditField::ThreadWakeupsPerHour,
                app.config
                    .thread_wakeups_per_hour
                    .map(|limit| limit.to_string())
                    .unwrap_or_default(),
            ),
            23 => app.config.thread_stall_detection = !app.config.thread_stall_detection,
            16 => begin_global_edit(
                app,
                SettingsEditField::HiddenTitlePrefixes,
                app.config.hidden_title_prefixes.join(", "),
            ),
            17 => begin_global_edit(
                app,
                SettingsEditField::HiddenPathPrefixes,
                app.config.hidden_path_prefixes.join(", "),
            ),
            19 => {
                app.config.github_comment_filter = app.config.github_comment_filter.next();
            }
            4 => begin_global_edit(
                app,
                SettingsEditField::BranchPrefix,
                app.config.worktree.branch_prefix.clone(),
            ),
            5 => begin_global_edit(
                app,
                SettingsEditField::WorktreeRoot,
                app.config.worktree.root.to_string_lossy().to_string(),
            ),
            6 => {
                // The row reflects the persisted value; a `--mux` override only lasts
                // for this invocation and must not be flipped by editing settings.
                app.mux_on_disk = !app.mux_on_disk;
                app.status_message =
                    Some("Multiplexer setting applies the next time CST starts".to_string());
            }
            7 => begin_global_edit(
                app,
                SettingsEditField::MuxPrefix,
                app.config.mux_prefix.clone(),
            ),
            8 => begin_global_edit(
                app,
                SettingsEditField::TerminalShell,
                app.config.terminal.shell.clone().unwrap_or_default(),
            ),
            9 => {
                app.config.notifications.enabled = !app.config.notifications.enabled;
            }
            10 => begin_global_edit(
                app,
                SettingsEditField::NtfyServer,
                app.config.notifications.server.clone(),
            ),
            11 => begin_global_edit(
                app,
                SettingsEditField::NtfyTopic,
                app.config.notifications.topic.clone(),
            ),
            12 => begin_global_edit(
                app,
                SettingsEditField::NtfyAccessToken,
                app.config.ntfy_access_token.clone(),
            ),
            13 => {
                app.config.ntfy_verbose = !app.config.ntfy_verbose;
            }
            14 => {
                app.config.notifications.ready = !app.config.notifications.ready;
            }
            15 => {
                app.config.notifications.error = !app.config.notifications.error;
            }
            _ => {}
        },
        _ => {}
    }
}

fn begin_global_edit(app: &mut App, field: SettingsEditField, value: String) {
    app.settings_editing = Some(field);
    app.settings_input = value;
}

fn commit_global_setting(app: &mut App, field: SettingsEditField) {
    let value = app.settings_input.trim().to_string();
    match field {
        SettingsEditField::Model => {
            app.config.model = (!value.is_empty()).then_some(value);
        }
        SettingsEditField::ThreadTrustedAuthors => {
            // Same shape as the prefix lists: comma-separated, trimmed, de-duplicated.
            // A login is not a path, but the parsing is identical and reusing it keeps
            // one place where "what does a comma-separated setting mean" is answered.
            app.config.thread_trusted_authors = config::parse_title_prefixes(&value);
        }
        SettingsEditField::ThreadWakeupsPerHour => {
            let trimmed = value.trim();
            if trimmed.is_empty() {
                app.config.thread_wakeups_per_hour = None;
            } else {
                match trimmed.parse::<u32>() {
                    Ok(limit) if limit > 0 => app.config.thread_wakeups_per_hour = Some(limit),
                    _ => {
                        // Left in the field rather than discarded, so a typo is a
                        // correction instead of silently reverting to the default.
                        app.status_message =
                            Some("Wakes per hour must be a positive number".to_string());
                        return;
                    }
                }
            }
        }
        SettingsEditField::HiddenTitlePrefixes => {
            app.config.hidden_title_prefixes = config::parse_title_prefixes(&value);
            app.show_hidden_sessions = false;
            app.apply_filter();
        }
        SettingsEditField::HiddenPathPrefixes => {
            app.config.hidden_path_prefixes = config::parse_path_prefixes(&value);
            app.show_hidden_sessions = false;
            app.apply_filter();
        }
        SettingsEditField::BranchPrefix => {
            if let Err(error) = worktree::validate_branch_prefix(&value) {
                app.status_message = Some(error.to_string());
                return;
            }
            app.config.worktree.branch_prefix = value;
        }
        SettingsEditField::WorktreeRoot => {
            if value.is_empty() {
                app.status_message = Some("Worktree root cannot be empty".to_string());
                return;
            }
            app.config.worktree.root = PathBuf::from(value);
        }
        SettingsEditField::MuxPrefix => {
            // Reject unparseable chords here rather than silently defaulting at startup.
            let Some(chord) = crate::mux::KeyChord::parse(&value) else {
                app.status_message = Some(format!(
                    "'{value}' is not a valid prefix (try C-b, C-g, C-a)"
                ));
                return;
            };
            app.config.mux_prefix = chord.label();
            if let Some(mux) = app.mux.as_mut() {
                mux.prefix = chord;
            }
        }
        SettingsEditField::TerminalShell => {
            app.config.terminal.shell = (!value.is_empty()).then_some(value);
        }
        SettingsEditField::NtfyServer => match config::normalize_ntfy_server(&value) {
            Ok(server) => app.config.notifications.server = server,
            Err(error) => {
                app.status_message = Some(error.to_string());
                return;
            }
        },
        SettingsEditField::NtfyTopic => {
            if !value.is_empty() {
                if let Err(error) = config::validate_ntfy_topic(&value) {
                    app.status_message = Some(error.to_string());
                    return;
                }
            }
            app.config.notifications.topic = value;
        }
        SettingsEditField::NtfyAccessToken => {
            if let Err(error) = config::validate_ntfy_access_token(&value) {
                app.status_message = Some(error.to_string());
                return;
            }
            app.config.ntfy_access_token = value;
        }
        SettingsEditField::MaxAutopilotContinues => {
            // Blank clears the override rather than meaning zero, which would pin
            // autopilot to never continuing at all.
            if value.is_empty() {
                app.config.max_autopilot_continues = None;
            } else {
                match value.parse::<u32>() {
                    Ok(limit) => app.config.max_autopilot_continues = Some(limit),
                    // Copilot rejects anything that is not a non-negative integer, so
                    // catching it here keeps a typo from breaking every launch until
                    // the user works out why sessions stopped starting.
                    Err(_) => {
                        app.status_message = Some(
                            "Max autopilot continues must be a whole number, or blank for Copilot's default"
                                .to_string(),
                        );
                        return;
                    }
                }
            }
        }
    }
    app.settings_editing = None;
    app.settings_input.clear();
    if let Err(error) = app.save_global_settings() {
        app.status_message = Some(format!("Failed to save global settings: {error}"));
    }
}

fn cycle_reasoning_effort(app: &mut App) {
    let efforts = config::REASONING_EFFORTS;
    app.config.reasoning_effort = match &app.config.reasoning_effort {
        None => Some(efforts[0].to_string()),
        Some(current) => match efforts.iter().position(|effort| *effort == current) {
            Some(index) if index + 1 < efforts.len() => Some(efforts[index + 1].to_string()),
            _ => None,
        },
    };
}

fn handle_project_settings(app: &mut App, key: KeyCode) {
    if app.project_settings_editing {
        match key {
            KeyCode::Esc => {
                app.project_settings_editing = false;
                app.project_settings_input.clear();
            }
            KeyCode::Enter => commit_project_setting(app),
            KeyCode::Backspace => {
                app.project_settings_input.pop();
            }
            KeyCode::Char(character) => app.project_settings_input.push(character),
            _ => {}
        }
        return;
    }

    match key {
        KeyCode::Esc | KeyCode::Char('.') => save_and_close_project_settings(app),
        KeyCode::Up | KeyCode::Char('k') => {
            app.project_settings_selected = app.project_settings_selected.saturating_sub(1);
        }
        KeyCode::Down | KeyCode::Char('j') => {
            // Bumped with every row added below; the rows here are positional, unlike
            // the keyed rows in global settings.
            if app.project_settings_selected < 4 {
                app.project_settings_selected += 1;
            }
        }
        KeyCode::Char(' ') => toggle_project_override(app),
        KeyCode::Enter => begin_project_edit(app),
        _ => {}
    }
}

fn toggle_project_override(app: &mut App) {
    let Some(settings) = app.project_settings.as_mut() else {
        return;
    };
    match app.project_settings_selected {
        0 => {
            if settings.branch_prefix_override().is_some() {
                settings.set_branch_prefix_override(None);
            } else {
                settings.set_branch_prefix_override(Some(
                    settings.effective_branch_prefix().to_string(),
                ));
            }
        }
        1 => {
            if settings.root_override().is_some() {
                settings.set_root_override(None);
            } else {
                settings.set_root_override(Some(settings.effective_root()));
            }
        }
        // Three states rather than the other rows' two: a repository needs to be able
        // to force the prompts back on for someone whose global setting is yolo, not
        // only to opt itself in.
        2 => {
            let next = match settings.yolo_override() {
                None => Some(true),
                Some(true) => Some(false),
                Some(false) => None,
            };
            settings.set_yolo_override(next);
        }
        // Two states here, unlike yolo: a number has no third meaning, so Space is
        // "stop inheriting, starting from whatever is currently in effect" and back.
        3 => {
            if settings.max_autopilot_continues_override().is_some() {
                settings.set_max_autopilot_continues_override(None);
            } else {
                settings.set_max_autopilot_continues_override(
                    settings
                        .effective_max_autopilot_continues()
                        .or(Some(DEFAULT_MAX_AUTOPILOT_CONTINUES)),
                );
            }
        }
        // Three states like yolo, and for the same reason: a repository doing sensitive
        // work needs to be able to refuse thread wakes even when they are on globally.
        4 => {
            let next = match settings.threads_enabled_override() {
                None => Some(true),
                Some(true) => Some(false),
                Some(false) => None,
            };
            settings.set_threads_enabled_override(next);
        }
        _ => {}
    }
}

/// What Copilot uses when nothing overrides it, as of CLI 1.0.84.
///
/// Only ever used as the starting point when a project first takes the setting over,
/// so drifting out of date makes an override start from a stale number rather than
/// changing what an inheriting project does.
const DEFAULT_MAX_AUTOPILOT_CONTINUES: u32 = 5;

fn begin_project_edit(app: &mut App) {
    let Some(settings) = app.project_settings.as_ref() else {
        return;
    };
    app.project_settings_input = match app.project_settings_selected {
        0 => settings.effective_branch_prefix().to_string(),
        1 => settings.effective_root().to_string_lossy().to_string(),
        3 => settings
            .max_autopilot_continues_override()
            .map(|limit| limit.to_string())
            .unwrap_or_default(),
        _ => return,
    };
    app.project_settings_editing = true;
}

fn commit_project_setting(app: &mut App) {
    let value = app.project_settings_input.trim().to_string();
    let Some(settings) = app.project_settings.as_mut() else {
        return;
    };
    match app.project_settings_selected {
        0 => {
            if let Err(error) = worktree::validate_branch_prefix(&value) {
                app.status_message = Some(error.to_string());
                return;
            }
            settings.set_branch_prefix_override(Some(value));
        }
        1 => {
            if value.is_empty() {
                app.status_message = Some("Worktree root cannot be empty".to_string());
                return;
            }
            settings.set_root_override(Some(PathBuf::from(value)));
        }
        3 => {
            // Blank clears the override rather than meaning zero, which would stop
            // autopilot continuing at all.
            if value.is_empty() {
                settings.set_max_autopilot_continues_override(None);
            } else {
                match value.parse::<u32>() {
                    Ok(limit) => settings.set_max_autopilot_continues_override(Some(limit)),
                    Err(_) => {
                        app.status_message = Some(
                            "Max autopilot continues must be a whole number, or blank to inherit"
                                .to_string(),
                        );
                        return;
                    }
                }
            }
        }
        _ => return,
    }
    app.project_settings_editing = false;
    app.project_settings_input.clear();
}

fn save_and_close_project_settings(app: &mut App) {
    let Some(settings) = app.project_settings.as_ref() else {
        app.mode = Mode::Normal;
        return;
    };
    match settings.save() {
        Ok(()) => {
            app.project_settings = None;
            app.mode = Mode::Normal;
            app.status_message = Some("Project settings saved".to_string());
        }
        Err(error) => {
            app.status_message = Some(format!("Failed to save project settings: {error}"));
        }
    }
}

/// Create the worktree and launch its session, rolling back if launch fails.
///
/// Runs from the main loop rather than the key handler so the "creating…" notice is
/// already on screen before this blocks.
pub fn run_pending_worktree(app: &mut App, pending: PendingWorktree) {
    let PendingWorktree {
        project,
        branch,
        config,
        target,
    } = pending;
    let created = match worktree::create_managed_worktree(Path::new(&project), &branch, &config) {
        Ok(created) => created,
        Err(error) => {
            app.branch_config = None;
            app.worktree_launch_target = WorktreeLaunchTarget::Standard;
            app.status_message = Some(format!("Cannot create worktree: {error}"));
            return;
        }
    };

    let path = created.entry.path.clone();
    let result = match target {
        WorktreeLaunchTarget::Standard => app
            .attach_new_session(&path.to_string_lossy(), branch.clone())
            .map(|_| match created.notice.as_deref() {
                Some(notice) => format!("Isolated session on '{branch}' — {notice}"),
                None => format!("Isolated session on '{branch}'"),
            }),
        WorktreeLaunchTarget::Tmux => {
            manager::start_tmux_session(&path, &branch, &app.config).map(|launched| {
                let mut message = complete_tmux_launch(
                    app,
                    launched,
                    branch.clone(),
                    &format!("Started '{branch}'"),
                );
                if let Some(notice) = created.notice.as_deref() {
                    message.push_str(&format!(" — {notice}"));
                }
                message
            })
        }
    };
    match result {
        Ok(message) => {
            app.branch_config = None;
            app.worktree_launch_target = WorktreeLaunchTarget::Standard;
            app.status_message = Some(message);
        }
        Err(error) => {
            // The worktree exists but has no session; undo it rather than leaking one.
            let rollback = worktree::rollback_created_worktree(&created.entry);
            app.branch_config = None;
            app.worktree_launch_target = WorktreeLaunchTarget::Standard;
            app.status_message = Some(match rollback {
                Ok(()) => format!("Cannot start session: {error}; worktree rolled back"),
                Err(rollback_error) => format!(
                    "Cannot start session: {error}; worktree rollback also failed: {rollback_error}"
                ),
            });
        }
    }
}

fn handle_branch_name(app: &mut App, key: KeyCode) {
    match key {
        KeyCode::Esc => {
            app.branch_config = None;
            app.worktree_launch_target = WorktreeLaunchTarget::Standard;
            app.mode = Mode::Normal;
            app.status_message = Some("Isolated session cancelled".to_string());
        }
        KeyCode::Enter => {
            let Some(project) = app.command_project() else {
                app.worktree_launch_target = WorktreeLaunchTarget::Standard;
                app.mode = Mode::Normal;
                app.status_message = Some("Project context is no longer available".to_string());
                return;
            };
            if let Err(error) = worktree::validate_branch(Path::new(&project), &app.branch_input) {
                app.status_message = Some(error.to_string());
                return;
            }
            let Some(config) = app.branch_config.clone() else {
                app.worktree_launch_target = WorktreeLaunchTarget::Standard;
                app.mode = Mode::Normal;
                app.status_message = Some("Worktree configuration is unavailable".to_string());
                return;
            };
            let target = app.worktree_launch_target;
            if app.mux_enabled() {
                // Creating a worktree copies files and talks to Git, which can take a
                // few seconds. Hand it to the main loop so a progress notice is painted
                // before we block.
                app.pending_worktree = Some(PendingWorktree {
                    project,
                    branch: app.branch_input.clone(),
                    config,
                    target,
                });
                app.mode = Mode::Normal;
                app.status_message = Some(format!("Creating worktree for '{}'…", app.branch_input));
                return;
            }
            app.should_new_session = Some(match target {
                WorktreeLaunchTarget::Standard => NewSessionRequest::Worktree {
                    source_project: project,
                    branch: app.branch_input.clone(),
                    config,
                },
                WorktreeLaunchTarget::Tmux => NewSessionRequest::TmuxWorktree {
                    source_project: project,
                    branch: app.branch_input.clone(),
                    config,
                },
            });
        }
        KeyCode::Backspace => {
            app.branch_input.pop();
        }
        KeyCode::Char(character) => app.branch_input.push(character),
        _ => {}
    }
}

/// Unread event bytes below which a session is summarized immediately.
///
/// Reading this much is imperceptible, so the common case keeps its instant details.
const DETAIL_INSTANT_BYTES: u64 = 4 * 1024 * 1024;

/// How long the selection must hold still before a large event log is read.
const DETAIL_SETTLE: std::time::Duration = std::time::Duration::from_millis(90);

/// Load details for the currently selected session if not already loaded.
pub fn maybe_load_details(app: &mut App) {
    let Some(id) = app.selected_session().map(|session| session.id.clone()) else {
        app.detail_pending = None;
        return;
    };
    if app.detail_loaded_for.as_deref() == Some(&id) {
        app.detail_pending = None;
        return;
    }
    let Some(index) = app.selected_real_index() else {
        return;
    };

    // Holding an arrow key sweeps the selection across sessions that are never read.
    // Summarizing a multi-hundred-megabyte event log for each one is what makes that
    // scrolling drag, so only expensive sessions wait for the selection to settle.
    if loader::pending_detail_bytes(&app.sessions[index]) > DETAIL_INSTANT_BYTES {
        match &app.detail_pending {
            Some((pending, since)) if *pending == id => {
                if since.elapsed() < DETAIL_SETTLE {
                    return;
                }
            }
            _ => {
                app.detail_pending = Some((id, std::time::Instant::now()));
                return;
            }
        }
    }

    let _ = loader::load_session_details(&mut app.sessions[index]);
    app.detail_loaded_for = Some(id);
    app.detail_pending = None;
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pasting_into_the_search_prompt_types_the_text() {
        let mut app = App::new(Vec::new(), config::UserConfig::default());
        app.mode = Mode::Search;

        handle_terminal_event(&mut app, Event::Paste("my-project\nrest".to_string())).unwrap();

        // Single-line prompt: the line break is dropped rather than typed or submitted.
        assert_eq!(app.search_query, "my-projectrest");
        assert_eq!(app.mode, Mode::Search);
    }

    #[test]
    fn pasting_in_normal_mode_never_runs_the_characters_as_commands() {
        let mut app = App::new(Vec::new(), config::UserConfig::default());
        assert_eq!(app.mode, Mode::Normal);

        // 'd' opens the delete confirmation and '/' opens search when typed.
        handle_terminal_event(&mut app, Event::Paste("d/rn".to_string())).unwrap();

        assert_eq!(app.mode, Mode::Normal);
        assert!(app.search_query.is_empty());
    }

    #[test]
    fn pasting_a_uuid_into_the_ntfy_topic_editor_keeps_it_editable() {
        let mut app = App::new(Vec::new(), config::UserConfig::default());
        app.mode = Mode::Settings;
        app.settings_selected = 11;
        app.settings_editing = Some(SettingsEditField::NtfyTopic);
        let topic = "f7dd1a49-c978-455a-a170-136c29ed39a4";

        handle_terminal_event(&mut app, Event::Paste(format!("{topic}\r\n"))).unwrap();

        assert_eq!(app.settings_input, topic);
        assert_eq!(app.settings_editing, Some(SettingsEditField::NtfyTopic));
        assert_eq!(app.mode, Mode::Settings);
    }

    #[test]
    fn arrow_keys_scroll_the_help_instead_of_closing_it() {
        let mut app = App::new(Vec::new(), config::UserConfig::default());
        handle_normal(&mut app, KeyCode::Char('?'));
        assert_eq!(app.mode, Mode::Help);
        assert_eq!(app.help_scroll, 0);

        handle_help(&mut app, KeyCode::Down);
        handle_help(&mut app, KeyCode::Down);
        assert_eq!(app.mode, Mode::Help);
        assert_eq!(app.help_scroll, 2);

        handle_help(&mut app, KeyCode::Up);
        assert_eq!(app.help_scroll, 1);

        handle_help(&mut app, KeyCode::Home);
        assert_eq!(app.help_scroll, 0);
        assert_eq!(app.mode, Mode::Help);

        handle_help(&mut app, KeyCode::Esc);
        assert_eq!(app.mode, Mode::Normal);
    }

    #[test]
    fn reopening_the_help_starts_at_the_top() {
        let mut app = App::new(Vec::new(), config::UserConfig::default());
        app.mode = Mode::Help;
        handle_help(&mut app, KeyCode::PageDown);
        assert_eq!(app.help_scroll, 10);
        handle_help(&mut app, KeyCode::Char('q'));
        assert_eq!(app.mode, Mode::Normal);

        handle_normal(&mut app, KeyCode::Char('?'));

        assert_eq!(app.help_scroll, 0);
    }

    #[test]
    fn notification_settings_toggle_and_require_a_topic_before_save() {
        let mut app = App::new(Vec::new(), config::UserConfig::default());
        app.mode = Mode::Settings;
        app.settings_section = SettingsSection::Notifications;
        app.settings_selected = 9;

        handle_settings(&mut app, KeyCode::Enter);
        assert!(app.config.notifications.enabled);
        handle_settings(&mut app, KeyCode::Esc);

        assert_eq!(app.mode, Mode::Settings, "invalid settings stay open");
        assert!(app
            .status_message
            .as_deref()
            .unwrap()
            .contains("topic is required"));

        app.settings_selected = 14;
        handle_settings(&mut app, KeyCode::Enter);
        assert!(!app.config.notifications.ready);
        app.settings_selected = 15;
        handle_settings(&mut app, KeyCode::Enter);
        assert!(!app.config.notifications.error);
    }

    #[test]
    fn global_settings_tabs_keep_selection_inside_the_active_section() {
        let mut app = App::new(Vec::new(), config::UserConfig::default());
        app.mode = Mode::Settings;
        // Row indices are keys, not positions: a section may carry an index that does
        // not sit numerically between its neighbours' — appending a row to an early
        // section is otherwise a renumbering of everything below it. What has to hold
        // is that every index belongs to exactly one section and none is skipped.
        let mut rows: Vec<usize> = SettingsSection::ALL
            .into_iter()
            .flat_map(|section| section.rows().iter().copied())
            .collect();
        let listed = rows.len();
        rows.sort_unstable();
        rows.dedup();
        assert_eq!(
            rows.len(),
            listed,
            "a settings row is claimed by more than one section"
        );
        // Derived from what the sections actually claim, so adding a row does not date
        // the test. What is being checked is the shape: a contiguous run from zero.
        let highest = rows.last().copied().unwrap_or_default();
        assert_eq!(
            rows,
            (0..=highest).collect::<Vec<_>>(),
            "every settings row must belong to exactly one section, with no gaps"
        );

        handle_settings(&mut app, KeyCode::Tab);
        assert_eq!(app.settings_section, SettingsSection::Worktrees);
        assert_eq!(app.settings_selected, 4);
        handle_settings(&mut app, KeyCode::Down);
        assert_eq!(app.settings_selected, 5);
        handle_settings(&mut app, KeyCode::Down);
        assert_eq!(app.settings_selected, 5);

        handle_settings(&mut app, KeyCode::BackTab);
        assert_eq!(app.settings_section, SettingsSection::General);
        assert_eq!(app.settings_selected, 0);
    }

    #[test]
    fn theme_picker_previews_cancels_and_confirms_without_hjkl() {
        let mut app = App::new(Vec::new(), config::UserConfig::default());
        app.disable_config_persistence();
        app.mode = Mode::Settings;
        app.settings_section = SettingsSection::General;
        app.settings_selected = 3;

        handle_settings(&mut app, KeyCode::Enter);
        assert!(app.theme_picker.is_some());
        handle_settings(&mut app, KeyCode::Down);
        assert_eq!(app.theme_name(), crate::theme::ThemeName::Gruvbox);
        assert_eq!(app.config.theme, crate::theme::ThemeName::Classic);
        handle_settings(&mut app, KeyCode::Esc);
        assert!(app.theme_picker.is_none());
        assert_eq!(app.theme_name(), crate::theme::ThemeName::Classic);

        handle_settings(&mut app, KeyCode::Enter);
        handle_settings(&mut app, KeyCode::Down);
        handle_settings(&mut app, KeyCode::Enter);
        assert!(app.theme_picker.is_none());
        assert_eq!(app.config.theme, crate::theme::ThemeName::Gruvbox);
    }

    #[test]
    fn theme_picker_supports_mouse_wheel_and_click_preview() {
        let mut app = App::new(Vec::new(), config::UserConfig::default());
        app.disable_config_persistence();
        app.mode = Mode::Settings;
        app.open_theme_picker();
        app.set_theme_picker_hits(vec![(ratatui::layout::Rect::new(10, 5, 20, 1), 4)]);

        handle_terminal_event(
            &mut app,
            Event::Mouse(crossterm::event::MouseEvent {
                kind: MouseEventKind::ScrollDown,
                column: 0,
                row: 0,
                modifiers: KeyModifiers::NONE,
            }),
        )
        .unwrap();
        assert_eq!(app.theme_name(), crate::theme::ThemeName::Gruvbox);

        handle_terminal_event(
            &mut app,
            Event::Mouse(crossterm::event::MouseEvent {
                kind: MouseEventKind::Down(MouseButton::Left),
                column: 12,
                row: 5,
                modifiers: KeyModifiers::NONE,
            }),
        )
        .unwrap();
        assert_eq!(app.theme_name(), crate::theme::ThemeName::CatppuccinMocha);

        handle_terminal_event(
            &mut app,
            Event::Mouse(crossterm::event::MouseEvent {
                kind: MouseEventKind::Down(MouseButton::Left),
                column: 12,
                row: 5,
                modifiers: KeyModifiers::NONE,
            }),
        )
        .unwrap();
        assert!(app.theme_picker.is_none());
        assert_eq!(app.config.theme, crate::theme::ThemeName::CatppuccinMocha);
    }

    #[test]
    fn terminal_shortcut_is_inert_on_session_list() {
        let mut app = App::new(Vec::new(), config::UserConfig::default());

        handle_normal(&mut app, KeyCode::Char('t'));

        assert!(app.terminal.active_session_id().is_none());
        assert!(!app.terminal.is_visible());
        assert!(app.status_message.is_none());
    }

    #[test]
    fn list_update_without_running_sessions_prepares_an_automatic_restart() {
        let mut app = App::new(Vec::new(), config::UserConfig::default());
        app.update_info = Some(crate::updater::UpdateInfo {
            current_version: "0.18.0".to_string(),
            latest_version: "0.19.0".to_string(),
        });

        handle_normal(&mut app, KeyCode::Char('u'));

        assert_eq!(app.update_install_requested_for.as_deref(), Some("0.19.0"));
        assert!(!app.should_quit);
        assert_eq!(
            app.restart_after_update,
            Some(crate::app::UpdateRestartRequest::default())
        );
        assert!(app
            .status_message
            .as_deref()
            .unwrap()
            .contains("restart automatically"));
    }

    #[test]
    fn update_restart_confirmation_is_modal_over_full_screen_tools() {
        let mut app = App::new(Vec::new(), config::UserConfig::default());
        app.mode = Mode::Scratchpad;
        app.confirm_update_restart = true;

        handle_terminal_event(
            &mut app,
            Event::Key(crossterm::event::KeyEvent::new(
                KeyCode::Esc,
                KeyModifiers::NONE,
            )),
        )
        .unwrap();

        assert!(!app.confirm_update_restart);
        assert_eq!(
            app.mode,
            Mode::Scratchpad,
            "the covered scratchpad must not consume confirmation input"
        );
    }

    #[test]
    fn favorite_shortcut_asks_where_to_open_before_launching_anything() {
        let mut app = App::new(Vec::new(), config::UserConfig::default());

        handle_normal(&mut app, KeyCode::Char('T'));

        assert_eq!(app.mode, Mode::FavoriteOpen);
        assert!(
            app.status_message.is_none(),
            "nothing should be launched until a destination is chosen"
        );
    }

    #[test]
    fn choosing_terminal_tabs_reports_its_result_in_the_status_bar() {
        let mut app = App::new(Vec::new(), config::UserConfig::default());

        handle_normal(&mut app, KeyCode::Char('T'));
        handle_favorite_open(&mut app, KeyCode::Char('t'));

        assert_eq!(app.mode, Mode::Normal);
        let message = app.status_message.unwrap();
        if cfg!(windows) {
            assert_eq!(message, "No favorite sessions configured");
        } else {
            assert!(message.contains("supported only on Windows"));
        }
    }

    #[test]
    fn choosing_panes_without_mux_says_so_instead_of_failing_silently() {
        let mut app = App::new(Vec::new(), config::UserConfig::default());

        handle_normal(&mut app, KeyCode::Char('T'));
        handle_favorite_open(&mut app, KeyCode::Char('p'));

        assert_eq!(app.mode, Mode::Normal);
        assert!(app.status_message.unwrap().contains("mux mode"));
    }

    #[test]
    fn cancelling_the_favorite_modal_launches_nothing() {
        let mut app = App::new(Vec::new(), config::UserConfig::default());

        handle_normal(&mut app, KeyCode::Char('T'));
        handle_favorite_open(&mut app, KeyCode::Esc);

        assert_eq!(app.mode, Mode::Normal);
        assert!(app.status_message.is_none());
    }

    #[test]
    fn cancelling_inherited_project_edits_does_not_create_overrides() {
        let temp = tempfile::tempdir().unwrap();
        let mut global = config::UserConfig::default();
        global.worktree.branch_prefix = "global/".to_string();
        global.worktree.root = temp.path().join("global-worktrees");
        let mut app = App::new(Vec::new(), global.clone());
        app.project_settings = Some(config::ProjectSettings::load(temp.path(), &global).unwrap());

        for selected in 0..=1 {
            app.project_settings_selected = selected;
            begin_project_edit(&mut app);
            assert!(app.project_settings_editing);
            handle_project_settings(&mut app, KeyCode::Esc);
        }

        let settings = app.project_settings.as_ref().unwrap();
        assert!(settings.branch_prefix_override().is_none());
        assert!(settings.root_override().is_none());
        settings.save().unwrap();
        assert!(!temp.path().join(".cst.json").exists());
    }

    #[test]
    fn new_session_uses_cwd_project_without_a_filter() {
        let temp = tempfile::tempdir().unwrap();
        std::fs::create_dir(temp.path().join(".git")).unwrap();
        let mut app = App::new(Vec::new(), config::UserConfig::default());
        app.set_cwd_context(temp.path().to_string_lossy().to_string(), false);
        assert!(app.project_filter.is_none());

        handle_normal(&mut app, KeyCode::Char('n'));
        handle_new_session_kind(&mut app, KeyCode::Char('n'));

        match app.should_new_session {
            Some(NewSessionRequest::Normal { ref cwd }) => {
                assert_eq!(cwd, app.cwd_project.as_ref().unwrap());
            }
            other => panic!("expected a normal new session request, got {other:?}"),
        }
    }

    #[test]
    fn configured_resume_key_queues_existing_session_for_foreground_tmux() {
        let session = crate::session::Session {
            id: "existing-session".to_string(),
            cwd: "/tmp/project".to_string(),
            project_root: "/tmp/project".to_string(),
            summary: Some("Existing".to_string()),
            created_at: None,
            updated_at: None,
            is_active: false,
            dir_path: PathBuf::from("/tmp/nonexistent-existing-session"),
            edited_files: Vec::new(),
            last_user_message: None,
            turn_count: 0,
            tool_call_count: 0,
            details_parsed_len: 0,
        };
        let mut config = config::UserConfig::default();
        config.tmux_keys.resume_session = "a".to_string();
        let mut app = App::new(vec![session], config);

        handle_normal(&mut app, KeyCode::Char('a'));

        match app.should_new_session.as_ref() {
            Some(NewSessionRequest::TmuxResume {
                session_id,
                cwd,
                title,
            }) => {
                assert_eq!(session_id, "existing-session");
                assert_eq!(cwd, "/tmp/project");
                assert_eq!(title, "Existing");
            }
            other => panic!("expected a tmux resume request, got {other:?}"),
        }
    }

    #[test]
    fn uppercase_m_queues_a_foreground_tmux_worktree_launch_without_mux() {
        let temp = tempfile::tempdir().unwrap();
        assert!(std::process::Command::new("git")
            .args(["init", "--quiet"])
            .current_dir(temp.path())
            .status()
            .unwrap()
            .success());
        let mut config = config::UserConfig::default();
        config.worktree.root = temp.path().join("worktrees");
        let mut app = App::new(Vec::new(), config);
        app.set_cwd_context(temp.path().to_string_lossy().to_string(), false);

        handle_normal(&mut app, KeyCode::Char('M'));
        assert_eq!(app.mode, Mode::BranchName);
        assert_eq!(app.worktree_launch_target, WorktreeLaunchTarget::Tmux);

        app.branch_input = "copilot/tmux-probe".to_string();
        handle_branch_name(&mut app, KeyCode::Enter);

        match app.should_new_session.as_ref() {
            Some(NewSessionRequest::TmuxWorktree {
                source_project,
                branch,
                ..
            }) => {
                assert_eq!(source_project, &temp.path().to_string_lossy());
                assert_eq!(branch, "copilot/tmux-probe");
            }
            other => panic!("expected a tmux worktree request, got {other:?}"),
        }
        assert!(app.pending_worktree.is_none());
    }

    #[test]
    fn lowercase_m_without_a_project_reports_what_is_missing() {
        let mut app = App::new(Vec::new(), config::UserConfig::default());

        handle_normal(&mut app, KeyCode::Char('m'));

        assert!(app
            .status_message
            .as_deref()
            .is_some_and(|message| message.contains("project")));
    }

    #[test]
    fn configured_end_key_targets_the_selected_tmux_session() {
        let session = crate::session::Session {
            id: "tmux-session".to_string(),
            cwd: "/tmp/project".to_string(),
            project_root: "/tmp/project".to_string(),
            summary: Some("Persistent".to_string()),
            created_at: None,
            updated_at: None,
            is_active: true,
            dir_path: PathBuf::from("/tmp/session"),
            edited_files: Vec::new(),
            last_user_message: None,
            turn_count: 0,
            tool_call_count: 0,
            details_parsed_len: 0,
        };
        let mut config = config::UserConfig::default();
        config.tmux_keys.end_session = "!".to_string();
        let mut app = App::new(vec![session], config);
        app.replace_tmux_session(crate::session::tmux::TmuxSessionRef {
            session_id: "tmux-session".to_string(),
            tmux_session: "cst-persistent-tmux-sess".to_string(),
            cwd: PathBuf::from("/tmp/project"),
            server_socket: None,
        });

        handle_normal(&mut app, KeyCode::Char('!'));

        assert_eq!(
            app.confirm_end_tmux
                .as_ref()
                .map(|reference| reference.session_id.as_str()),
            Some("tmux-session")
        );
    }

    #[test]
    fn new_session_shortcut_asks_for_the_kind_before_starting_anything() {
        let temp = tempfile::tempdir().unwrap();
        std::fs::create_dir(temp.path().join(".git")).unwrap();
        let mut app = App::new(Vec::new(), config::UserConfig::default());
        app.set_cwd_context(temp.path().to_string_lossy().to_string(), false);

        handle_normal(&mut app, KeyCode::Char('n'));

        assert_eq!(app.mode, Mode::NewSessionKind);
        assert!(
            app.should_new_session.is_none(),
            "nothing may start until a kind is chosen"
        );
    }

    #[test]
    fn cancelling_the_new_session_question_starts_nothing() {
        let temp = tempfile::tempdir().unwrap();
        std::fs::create_dir(temp.path().join(".git")).unwrap();
        let mut app = App::new(Vec::new(), config::UserConfig::default());
        app.set_cwd_context(temp.path().to_string_lossy().to_string(), false);

        handle_normal(&mut app, KeyCode::Char('n'));
        handle_new_session_kind(&mut app, KeyCode::Esc);

        assert_eq!(app.mode, Mode::Normal);
        assert!(app.should_new_session.is_none());
        assert!(app.pending_worktree.is_none());
    }

    #[test]
    fn choosing_worktree_in_the_new_session_question_reaches_the_worktree_flow() {
        let temp = tempfile::tempdir().unwrap();
        std::fs::create_dir(temp.path().join(".git")).unwrap();
        let mut app = App::new(Vec::new(), config::UserConfig::default());
        app.set_cwd_context(temp.path().to_string_lossy().to_string(), false);

        handle_normal(&mut app, KeyCode::Char('n'));
        handle_new_session_kind(&mut app, KeyCode::Char('w'));

        // A bare `.git` directory is not a usable repository, so reaching the
        // worktree flow shows either its branch prompt or its error — never the
        // plain-session path.
        let reached_worktree_flow = app.mode == Mode::BranchName
            || app
                .status_message
                .as_deref()
                .is_some_and(|message| message.contains("worktree"));
        assert!(
            reached_worktree_flow,
            "mode: {:?}, status: {:?}",
            app.mode, app.status_message
        );
        assert!(app.should_new_session.is_none());
    }

    #[test]
    fn cwd_project_is_offered_in_the_project_list_and_auto_filtered() {
        let temp = tempfile::tempdir().unwrap();
        std::fs::create_dir(temp.path().join(".git")).unwrap();
        let mut app = App::new(Vec::new(), config::UserConfig::default());

        app.set_cwd_context(temp.path().to_string_lossy().to_string(), true);

        let project = app.cwd_project.clone().unwrap();
        assert_eq!(app.project_filter.as_ref(), Some(&project));
        assert!(app.unique_projects.contains(&project));
    }

    #[test]
    fn active_session_opens_takeover_confirmation_instead_of_refusing() {
        let temp = tempfile::tempdir().unwrap();
        let pid = std::process::id();
        std::fs::write(
            temp.path().join(format!("inuse.{pid}.lock")),
            pid.to_string(),
        )
        .unwrap();
        let session = crate::session::Session {
            id: "active-session".to_string(),
            cwd: temp.path().to_string_lossy().to_string(),
            project_root: temp.path().to_string_lossy().to_string(),
            summary: Some("Active elsewhere".to_string()),
            created_at: None,
            updated_at: None,
            is_active: true,
            dir_path: temp.path().to_path_buf(),
            edited_files: Vec::new(),
            last_user_message: None,
            turn_count: 0,
            tool_call_count: 0,
            details_parsed_len: 0,
        };
        let mut app = App::new(vec![session], config::UserConfig::default());

        resume_selected(&mut app);

        assert_eq!(app.mode, Mode::ConfirmTakeover);
        let takeover = app.pending_takeover.as_ref().unwrap();
        assert_eq!(takeover.id, "active-session");
        assert_eq!(takeover.pids, vec![pid]);
        assert!(app.should_resume.is_none());

        handle_confirm_takeover(&mut app, KeyCode::Esc);
        assert_eq!(app.mode, Mode::Normal);
        assert!(app.pending_takeover.is_none());
        assert!(app.should_resume.is_none());
    }

    fn favorites_app() -> App {
        let sessions: Vec<crate::session::Session> = (0..3)
            .map(|index| crate::session::Session {
                id: format!("id-{index}"),
                cwd: "C:/Workspace/zazen".to_string(),
                project_root: "C:/Workspace/zazen".to_string(),
                summary: Some(format!("Session {index}")),
                created_at: None,
                updated_at: None,
                is_active: false,
                dir_path: std::path::PathBuf::from("."),
                edited_files: Vec::new(),
                last_user_message: None,
                turn_count: 0,
                tool_call_count: 0,
                details_parsed_len: 0,
            })
            .collect();
        let config = config::UserConfig {
            favorites: vec!["id-0".to_string(), "id-1".to_string()],
            ..Default::default()
        };
        let mut app = App::new(sessions, config);
        app.disable_config_persistence();
        app
    }

    #[test]
    fn grabbing_then_pressing_down_reorders_instead_of_moving_the_cursor() {
        let mut app = favorites_app();
        app.selected = 0;

        handle_normal(&mut app, KeyCode::Char('g'));
        assert_eq!(app.grabbed_favorite.as_deref(), Some("id-0"));

        handle_normal(&mut app, KeyCode::Down);

        assert_eq!(app.config.favorites, vec!["id-1", "id-0"]);
        // The cursor travels with the row, so a second press keeps moving it.
        assert_eq!(app.selected, 1);
        assert_eq!(app.grabbed_favorite.as_deref(), Some("id-0"));
    }

    #[test]
    fn enter_drops_the_grabbed_favorite_without_resuming_a_session() {
        let mut app = favorites_app();
        app.selected = 0;
        handle_normal(&mut app, KeyCode::Char('g'));

        handle_normal(&mut app, KeyCode::Enter);

        assert!(app.grabbed_favorite.is_none());
        assert!(app.should_resume.is_none(), "Enter must not attach here");
    }

    #[test]
    fn an_unrelated_key_drops_the_grab_before_acting() {
        let mut app = favorites_app();
        app.selected = 0;
        handle_normal(&mut app, KeyCode::Char('g'));

        handle_normal(&mut app, KeyCode::Char('/'));

        // The item is dropped first, so the command never runs mid-rearrangement.
        assert!(app.grabbed_favorite.is_none());
        assert_eq!(app.mode, Mode::Search);
        assert_eq!(app.config.favorites, vec!["id-0", "id-1"]);
    }
    #[test]
    fn space_cycles_the_project_yolo_setting_through_all_three_states() {
        let temp = tempfile::tempdir().unwrap();
        let global = config::UserConfig {
            yolo: true,
            ..config::UserConfig::default()
        };
        let mut app = App::new(Vec::new(), global.clone());
        app.project_settings = Some(config::ProjectSettings::load(temp.path(), &global).unwrap());
        app.mode = Mode::ProjectSettings;
        app.project_settings_selected = 2;

        let state = |app: &App| app.project_settings.as_ref().unwrap().yolo_override();
        assert_eq!(state(&app), None, "starts inherited");

        handle_project_settings(&mut app, KeyCode::Char(' '));
        assert_eq!(state(&app), Some(true));
        handle_project_settings(&mut app, KeyCode::Char(' '));
        assert_eq!(
            state(&app),
            Some(false),
            "a repository must be able to say no to a global yes"
        );
        handle_project_settings(&mut app, KeyCode::Char(' '));
        assert_eq!(state(&app), None, "and back to inheriting");
    }

    #[test]
    fn the_yolo_row_is_reachable_and_has_nothing_to_type_into() {
        let temp = tempfile::tempdir().unwrap();
        let global = config::UserConfig::default();
        let mut app = App::new(Vec::new(), global.clone());
        app.project_settings = Some(config::ProjectSettings::load(temp.path(), &global).unwrap());
        app.mode = Mode::ProjectSettings;

        handle_project_settings(&mut app, KeyCode::Down);
        handle_project_settings(&mut app, KeyCode::Down);
        assert_eq!(app.project_settings_selected, 2);

        // Enter opens a text editor on the other rows; a tri-state has no text.
        handle_project_settings(&mut app, KeyCode::Enter);
        assert!(!app.project_settings_editing);

        handle_project_settings(&mut app, KeyCode::Down);
        assert_eq!(app.project_settings_selected, 3);
        handle_project_settings(&mut app, KeyCode::Down);
        assert_eq!(app.project_settings_selected, 4);
        handle_project_settings(&mut app, KeyCode::Down);
        assert_eq!(app.project_settings_selected, 4, "stops at the last row");
    }

    #[test]
    fn a_repository_can_refuse_thread_wakes_even_when_they_are_on_globally() {
        // Three states, like yolo: inheriting is not the same as agreeing, and a
        // repository doing sensitive work has to be able to say no outright.
        let temp = tempfile::tempdir().unwrap();
        let global = config::UserConfig::default();
        let mut app = App::new(Vec::new(), global.clone());
        app.project_settings = Some(config::ProjectSettings::load(temp.path(), &global).unwrap());
        app.mode = Mode::ProjectSettings;
        app.project_settings_selected = 4;

        assert!(global.threads_enabled, "the global default is on");
        let states: Vec<Option<bool>> = (0..4)
            .map(|_| {
                toggle_project_override(&mut app);
                app.project_settings
                    .as_ref()
                    .unwrap()
                    .threads_enabled_override()
            })
            .collect();

        assert_eq!(
            states,
            vec![Some(true), Some(false), None, Some(true)],
            "inherit -> on -> off -> inherit"
        );
    }
    fn key_event(code: KeyCode) -> Event {
        Event::Key(crossterm::event::KeyEvent::new(code, KeyModifiers::NONE))
    }

    fn whats_new_app() -> App {
        let mut app = App::new(Vec::new(), config::UserConfig::default());
        app.whats_new = Some(crate::ui::whats_new::WhatsNewScreen::new(
            crate::changelog::whats_new_for_current_version(),
        ));
        if let Some(screen) = app.whats_new.as_mut() {
            screen.max_scroll = 20;
        }
        app
    }

    #[test]
    fn the_whats_new_screen_swallows_a_stray_keystroke_instead_of_sending_it_to_copilot() {
        // It is most likely on screen right after a restart reopened panes, so a key
        // that reached the chat underneath would be typed into someone's conversation.
        let mut app = whats_new_app();

        for code in [KeyCode::Char('a'), KeyCode::Tab, KeyCode::Backspace] {
            handle_whats_new_event(&mut app, key_event(code));
            assert!(
                app.whats_new.is_some(),
                "{code:?} must neither close it nor fall through"
            );
        }
    }

    #[test]
    fn whats_new_closes_on_the_keys_its_footer_advertises() {
        for code in [
            KeyCode::Esc,
            KeyCode::Enter,
            KeyCode::Char('q'),
            KeyCode::Char(' '),
        ] {
            let mut app = whats_new_app();
            handle_whats_new_event(&mut app, key_event(code));
            assert!(app.whats_new.is_none(), "{code:?} should close it");
        }
    }

    #[test]
    fn whats_new_scrolls_with_the_same_keys_as_the_help_popup() {
        let mut app = whats_new_app();
        let scroll = |app: &App| app.whats_new.as_ref().unwrap().scroll;

        handle_whats_new_event(&mut app, key_event(KeyCode::Down));
        assert_eq!(scroll(&app), 1);
        handle_whats_new_event(&mut app, key_event(KeyCode::End));
        assert_eq!(scroll(&app), 20, "End reaches the bottom");
        handle_whats_new_event(&mut app, key_event(KeyCode::Home));
        assert_eq!(scroll(&app), 0, "and Home comes back");
    }

    #[test]
    fn ctrl_c_still_quits_from_the_whats_new_screen() {
        // Every other key is swallowed, so without this the screen would be a trap for
        // anyone whose reflex is Ctrl+C.
        let mut app = whats_new_app();
        app.disable_config_persistence();

        handle_whats_new_event(
            &mut app,
            Event::Key(crossterm::event::KeyEvent::new(
                KeyCode::Char('c'),
                KeyModifiers::CONTROL,
            )),
        );

        assert!(app.should_quit || app.confirm_quit);
    }
    #[test]
    fn a_typo_in_the_autopilot_cap_is_refused_rather_than_breaking_every_launch() {
        let mut app = App::new(Vec::new(), config::UserConfig::default());
        app.disable_config_persistence();
        app.mode = Mode::Settings;
        app.settings_section = SettingsSection::General;
        app.settings_selected = 18;

        handle_settings(&mut app, KeyCode::Enter);
        assert_eq!(
            app.settings_editing,
            Some(SettingsEditField::MaxAutopilotContinues)
        );
        app.settings_input = "lots".to_string();
        handle_settings(&mut app, KeyCode::Enter);

        // Copilot rejects a non-numeric value, so letting it through would stop every
        // session starting until the user worked out why.
        assert_eq!(app.config.max_autopilot_continues, None);
        assert!(app
            .status_message
            .as_deref()
            .unwrap()
            .contains("whole number"));
        assert!(app.settings_editing.is_some(), "the editor stays open");
    }

    #[test]
    fn clearing_the_autopilot_cap_hands_the_limit_back_to_copilot() {
        let mut app = App::new(
            Vec::new(),
            config::UserConfig {
                max_autopilot_continues: Some(9),
                ..config::UserConfig::default()
            },
        );
        app.disable_config_persistence();
        app.mode = Mode::Settings;
        app.settings_selected = 18;

        handle_settings(&mut app, KeyCode::Enter);
        assert_eq!(
            app.settings_input, "9",
            "editing starts from the current value"
        );
        app.settings_input.clear();
        handle_settings(&mut app, KeyCode::Enter);

        // Blank means "no opinion", not zero -- zero would stop autopilot entirely.
        assert_eq!(app.config.max_autopilot_continues, None);
        assert!(app.settings_editing.is_none());
    }

    #[test]
    fn the_autopilot_row_is_reachable_from_the_general_section() {
        let mut app = App::new(Vec::new(), config::UserConfig::default());
        app.mode = Mode::Settings;
        app.settings_section = SettingsSection::General;
        app.settings_selected = SettingsSection::General.rows()[0];

        let mut seen = vec![app.settings_selected];
        for _ in 0..6 {
            handle_settings(&mut app, KeyCode::Down);
            seen.push(app.settings_selected);
        }
        assert!(
            seen.contains(&18),
            "arrowing through General reaches it: {seen:?}"
        );
    }

    #[test]
    fn github_comment_filter_setting_cycles_through_each_default() {
        let mut app = App::new(Vec::new(), config::UserConfig::default());
        app.mode = Mode::Settings;
        app.settings_section = SettingsSection::Filters;
        app.settings_selected = 19;

        handle_settings(&mut app, KeyCode::Enter);
        assert_eq!(
            app.config.github_comment_filter,
            config::GithubCommentFilter::Unresolved
        );
        handle_settings(&mut app, KeyCode::Char(' '));
        assert_eq!(
            app.config.github_comment_filter,
            config::GithubCommentFilter::Resolved
        );
        handle_settings(&mut app, KeyCode::Enter);
        assert_eq!(
            app.config.github_comment_filter,
            config::GithubCommentFilter::All
        );
    }

    #[test]
    fn a_project_takes_the_autopilot_cap_over_from_whatever_is_in_effect() {
        let temp = tempfile::tempdir().unwrap();
        let global = config::UserConfig {
            max_autopilot_continues: Some(12),
            ..config::UserConfig::default()
        };
        let mut app = App::new(Vec::new(), global.clone());
        app.project_settings = Some(config::ProjectSettings::load(temp.path(), &global).unwrap());
        app.mode = Mode::ProjectSettings;
        app.project_settings_selected = 3;

        let state = |app: &App| {
            app.project_settings
                .as_ref()
                .unwrap()
                .max_autopilot_continues_override()
        };
        assert_eq!(state(&app), None, "starts inherited");

        // Taking it over starts from what was already in effect, so Space alone never
        // silently changes the number.
        handle_project_settings(&mut app, KeyCode::Char(' '));
        assert_eq!(state(&app), Some(12));
        handle_project_settings(&mut app, KeyCode::Char(' '));
        assert_eq!(state(&app), None, "and back to inheriting");
    }
}

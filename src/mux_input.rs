use crate::app::{App, GithubScrollbar, Mode, View, WorkspaceFocus, WorkspaceHelp};
use crate::command_palette::CommandId;
use crate::input::{
    handle_end_tmux_confirm, handle_quit_confirm, handle_update_restart_confirm,
    request_end_tmux_session, request_quit,
};
use crate::mux::pane::PaneNotification;
use crate::mux::{
    resolve_github_command, resolve_help_command, resolve_layout_command,
    resolve_prefix_command_with_tmux_keys, resolve_transient_command, GithubCommand, HelpCommand,
    MuxEvent, MuxState, PrefixCommand, PrefixState, TransientCommand, TransientMode,
};
use crate::notifications::NotificationKind;
use crate::snippets::{SnippetEditorField, SnippetModal, SnippetScope, SnippetScreen};
use crossterm::event::{
    Event, KeyCode, KeyEvent, KeyEventKind, KeyModifiers, MouseButton, MouseEventKind,
};
use ratatui::layout::Rect;

const HOOK_READY_GRACE: std::time::Duration = std::time::Duration::from_millis(750);

/// Route a terminal event while a pane is focused.
///
/// Everything except the prefix key is forwarded to the child, because Copilot wants
/// nearly every keystroke for itself.
pub fn handle_attached_event(app: &mut App, event: Event) {
    // A message in the attached status bar answers the last thing the user did, so
    // the next keystroke retires it — whatever that keystroke goes on to set is the
    // new answer. Without this a message would sit over the prefix reminder forever.
    if matches!(&event, Event::Key(key) if key.kind == KeyEventKind::Press) {
        app.status_message = None;
    }
    if app.confirm_end_tmux.is_some() {
        if let Event::Key(key) = &event {
            if key.kind == KeyEventKind::Press {
                handle_end_tmux_confirm(app, key.code);
            }
        }
        return;
    }

    if app.confirm_update_restart {
        if let Event::Key(key) = &event {
            if key.kind == KeyEventKind::Press {
                handle_update_restart_confirm(app, key.code);
            }
        }
        return;
    }

    if app.confirm_quit {
        if let Event::Key(key) = &event {
            if key.kind == KeyEventKind::Press {
                handle_quit_confirm(app, key.code);
                // Cancelling is answer enough on the attached screen, where the
                // shared "Quit cancelled" notice would only cover the prefix reminder.
                if !app.should_quit {
                    app.status_message = None;
                }
            }
        }
        return;
    }

    // Same reasoning as What's New below: this can be open over live Copilot panes, so
    // it has to take every keystroke rather than let one through to a chat.
    if crate::input::thread_inbox_active(app) {
        crate::input::handle_thread_inbox_event(app, event);
        return;
    }

    // First, and before the prefix gate below: this is most likely to be on screen
    // right after a restart reopened these panes, and a keystroke aimed at dismissing
    // it must not land in a Copilot chat.
    if crate::input::whats_new_active(app) {
        crate::input::handle_whats_new_event(app, event);
        return;
    }

    if app.command_palette.is_some() {
        handle_command_palette_event(app, event);
        return;
    }

    if crate::input::portable_modal_active(app) {
        crate::input::handle_portable_modal_event(app, event);
        return;
    }

    // Choosing the project for a new session started from the split picker. The list
    // view's own project filter never opens here.
    if app.mode == Mode::FilterProject {
        if let Event::Key(key) = &event {
            if key.kind == KeyEventKind::Press {
                crate::input::handle_filter_project(app, key.code);
            }
        }
        return;
    }

    if let Event::Key(key) = &event {
        if key.kind == KeyEventKind::Press {
            let is_prefix = app.mux.as_ref().is_some_and(|mux| {
                mux.prefix_state != PrefixState::Idle || mux.prefix.matches(key)
            });
            if is_prefix {
                handle_attached_key(app, *key);
                return;
            }
            // Browser-style: whichever panel has the keyboard, these switch tabs.
            if let Some(forward) = tab_switch_key(key) {
                close_context_overlays(app);
                if let Some(mux) = app.mux.as_mut() {
                    mux.cycle(forward);
                }
                sync_workspace_panels(app);
                sync_view(app);
                return;
            }
        }
    }

    if app.workspace_help.is_some() {
        if matches!(
            event,
            Event::Key(KeyEvent {
                code: KeyCode::Esc,
                kind: KeyEventKind::Press,
                ..
            })
        ) {
            app.workspace_help = None;
        }
        return;
    }

    // Reachable from the command palette while attached, so it has to take keys here
    // too — otherwise the pane swallows them and the mode can never be left.
    if app.mode == Mode::FavoriteOpen {
        if let Event::Key(key) = &event {
            if key.kind == KeyEventKind::Press {
                crate::input::handle_favorite_open(app, key.code);
            }
        }
        return;
    }

    // Same situation: the palette's "New session" lands here while attached.
    if app.mode == Mode::NewSessionKind {
        if let Event::Key(key) = &event {
            if key.kind == KeyEventKind::Press {
                crate::input::handle_new_session_kind(app, key.code);
            }
        }
        return;
    }

    if app.snippet_modal.is_some() {
        handle_snippet_event(app, event);
        return;
    }

    if app.github_inspector.is_some() {
        handle_github_inspector_event(app, event);
        return;
    }

    let attended = matches!(
        &event,
        Event::Key(KeyEvent {
            kind: KeyEventKind::Press,
            ..
        }) | Event::Paste(_)
            | Event::Mouse(crossterm::event::MouseEvent {
                kind: MouseEventKind::Down(_),
                ..
            })
    );
    if attended {
        app.terminal_focused = true;
        app.acknowledge_focused_pane();
    }

    if let Event::Key(key) = &event {
        if key.kind != KeyEventKind::Press {
            return;
        }
        app.clear_update_notice();
    }

    if let Event::Mouse(mouse) = &event {
        if app.swallow_mouse_until_up {
            match mouse.kind {
                MouseEventKind::Drag(_) => return,
                MouseEventKind::Up(_) => {
                    app.swallow_mouse_until_up = false;
                    return;
                }
                // A release can go missing (outside the window, say); anything else
                // means that gesture is over.
                _ => app.swallow_mouse_until_up = false,
            }
        }
        // Ctrl+click, as in any terminal. A plain click still belongs to the child, which
        // uses it for its own interface. Opening a link needs nothing from the session it
        // is in, so this works in any split, focused or not.
        if matches!(mouse.kind, MouseEventKind::Down(MouseButton::Left))
            && mouse.modifiers.contains(KeyModifiers::CONTROL)
        {
            let under = app.workspace_areas.chat_at(mouse.column, mouse.row);
            let link = app
                .mux
                .as_ref()
                .and_then(|mux| under.and_then(|id| mux.pane(id)).or(mux.focused_pane()))
                .and_then(|pane| pane.link_at(mouse.column, mouse.row));
            if let Some(link) = link {
                app.open_link(&link);
                return;
            }
        }
        if matches!(mouse.kind, MouseEventKind::Down(MouseButton::Left)) {
            let reference = app
                .mux
                .as_ref()
                .and_then(|mux| mux.focused_pane())
                .and_then(|pane| pane.github_reference_at(mouse.column, mouse.row));
            if let Some(number) = reference {
                app.inspect_github_item(number);
                return;
            }
        }
        // The tab strip sits above the workspace, so it claims the click before the
        // panes below get a chance to take focus.
        if matches!(mouse.kind, MouseEventKind::Down(MouseButton::Left))
            && focus_clicked_tab(app, mouse.column, mouse.row)
        {
            return;
        }
        // A press that landed on a tab owns the rest of the gesture. Nothing below
        // may see it, or the drag would double as a selection in whatever pane the
        // pointer happens to be over.
        if app.dragging_tab.is_some() {
            match mouse.kind {
                MouseEventKind::Drag(MouseButton::Left) => {
                    drag_tab_to(app, mouse.column, mouse.row);
                    return;
                }
                MouseEventKind::Up(_) => {
                    app.dragging_tab = None;
                    return;
                }
                _ => {}
            }
        }
        // Same rule as a tab drag: once a border is picked up, the whole gesture is
        // the resize's, or the pointer passing over a chat would select text in it.
        if let Some(divider) = app.dragging_divider.clone() {
            match mouse.kind {
                MouseEventKind::Drag(MouseButton::Left) => {
                    drag_divider_to(app, divider, mouse.column, mouse.row);
                    return;
                }
                MouseEventKind::Up(_) => {
                    app.dragging_divider = None;
                    return;
                }
                _ => app.dragging_divider = None,
            }
        }
        if matches!(mouse.kind, MouseEventKind::Down(MouseButton::Left)) {
            if let Some(divider) = app.workspace_areas.divider_at(mouse.column, mouse.row) {
                app.dragging_divider = Some(divider);
                return;
            }
        }
        // The wheel scrolls whichever split it is over, without taking focus: reading
        // back through a session being watched should not pull the keyboard away from
        // the one being typed into.
        if matches!(
            mouse.kind,
            MouseEventKind::ScrollUp | MouseEventKind::ScrollDown
        ) {
            let under = app.workspace_areas.chat_at(mouse.column, mouse.row);
            if let Some(mux) = app.mux.as_mut() {
                if let Some(pane) = under
                    .filter(|id| mux.focused != Some(*id))
                    .and_then(|id| mux.pane_mut(id))
                {
                    let _ = pane.handle_mouse(*mouse);
                    return;
                }
            }
        }
        if matches!(mouse.kind, MouseEventKind::Down(_))
            && focus_clicked_workspace(app, mouse.column, mouse.row)
        {
            // The click was to choose a session, not to act in it.
            app.swallow_mouse_until_up = true;
            return;
        }
    }

    match app.workspace_focus {
        WorkspaceFocus::Chat => handle_chat_event(app, event),
        WorkspaceFocus::Scratchpad => handle_scratchpad_event(app, event),
        WorkspaceFocus::Terminal => handle_terminal_event(app, event),
    }
}

fn handle_chat_event(app: &mut App, event: Event) {
    match event {
        Event::Key(key) => {
            if key.kind != KeyEventKind::Press {
                return;
            }
            handle_attached_key(app, key);
        }
        Event::Paste(text) => {
            if let Some(pane) = app.mux.as_mut().and_then(|mux| mux.focused_pane_mut()) {
                let _ = pane.send_paste(&text);
            }
        }
        Event::Mouse(mouse) => {
            if let Some(pane) = app.mux.as_mut().and_then(|mux| mux.focused_pane_mut()) {
                let _ = pane.handle_mouse(mouse);
            }
        }
        _ => {}
    }
}

fn handle_snippet_event(app: &mut App, event: Event) {
    let screen = match app.snippet_modal.as_ref() {
        Some(modal) => modal.screen,
        None => return,
    };
    match (screen, event) {
        (SnippetScreen::List, Event::Key(key)) if key.kind == KeyEventKind::Press => {
            handle_snippet_list_key(app, key.code);
        }
        (SnippetScreen::Editor, Event::Key(key)) if key.kind == KeyEventKind::Press => {
            handle_snippet_editor_key(app, key);
        }
        (SnippetScreen::Editor, Event::Paste(text)) => {
            if let Some(editor) = app
                .snippet_modal
                .as_mut()
                .and_then(SnippetModal::focused_editor)
            {
                editor.handle_event(Event::Paste(text));
            }
        }
        (SnippetScreen::ConfirmDelete, Event::Key(key)) if key.kind == KeyEventKind::Press => {
            match key.code {
                KeyCode::Char('y') | KeyCode::Enter => delete_selected_snippet(app),
                KeyCode::Char('n') | KeyCode::Esc => {
                    if let Some(modal) = app.snippet_modal.as_mut() {
                        modal.cancel_subscreen();
                    }
                }
                _ => {}
            }
        }
        _ => {}
    }
}

fn handle_snippet_list_key(app: &mut App, key: KeyCode) {
    match key {
        KeyCode::Up => {
            if let Some(modal) = app.snippet_modal.as_mut() {
                modal.select_previous();
            }
        }
        KeyCode::Down => {
            if let Some(modal) = app.snippet_modal.as_mut() {
                modal.select_next();
            }
        }
        KeyCode::Char('a') => {
            if let Some(modal) = app.snippet_modal.as_mut() {
                modal.begin_add();
            }
        }
        KeyCode::Char('e') => {
            if let Some(modal) = app.snippet_modal.as_mut() {
                modal.begin_edit();
            }
        }
        KeyCode::Char('d') => {
            if let Some(modal) = app.snippet_modal.as_mut() {
                modal.begin_delete();
            }
        }
        KeyCode::Enter => use_selected_snippet(app),
        KeyCode::Char(digit) if digit.is_ascii_digit() => {
            if let Some(index) = crate::snippets::quick_use_index(digit) {
                use_snippet_at(app, index);
            }
        }
        KeyCode::Char('q') | KeyCode::Esc => app.snippet_modal = None,
        _ => {}
    }
}

fn handle_snippet_editor_key(app: &mut App, key: KeyEvent) {
    let control = key
        .modifiers
        .contains(crossterm::event::KeyModifiers::CONTROL);
    if control && matches!(key.code, KeyCode::Char('s')) {
        save_snippet_editor(app);
        return;
    }
    if control && matches!(key.code, KeyCode::Char('g')) {
        toggle_snippet_scope(app);
        return;
    }
    let Some(modal) = app.snippet_modal.as_mut() else {
        return;
    };
    // The form claims the keys that move between fields; everything else is the
    // editor's, so a chord that works in the scratchpad works here too.
    match key.code {
        KeyCode::Esc => {
            modal.cancel_subscreen();
            return;
        }
        // Terminals disagree on whether Shift+Tab arrives as BackTab or as Tab
        // carrying the modifier; both mean "the field before this one".
        KeyCode::Tab | KeyCode::BackTab => {
            let forward = key.code == KeyCode::Tab
                && !key
                    .modifiers
                    .contains(crossterm::event::KeyModifiers::SHIFT);
            modal.editor_field = modal.editor_field.next(forward);
            return;
        }
        KeyCode::Enter if modal.editor_field == SnippetEditorField::Name => {
            modal.editor_field = SnippetEditorField::Scope;
            return;
        }
        KeyCode::Enter | KeyCode::Char(' ') if modal.editor_field == SnippetEditorField::Scope => {
            toggle_snippet_scope(app);
            return;
        }
        _ => {}
    }

    if let Some(editor) = modal.focused_editor() {
        if editor.handle_event(Event::Key(key)) {
            modal.error = None;
        }
    }
}

fn toggle_snippet_scope(app: &mut App) {
    let Some(modal) = app.snippet_modal.as_mut() else {
        return;
    };
    modal.error = None;
    modal.editor_scope = match modal.editor_scope {
        SnippetScope::Global if modal.project_root.is_some() => SnippetScope::Project,
        SnippetScope::Global => {
            modal.error = Some("No Git project detected for this session".to_string());
            SnippetScope::Global
        }
        SnippetScope::Project => SnippetScope::Global,
    };
}

fn save_snippet_editor(app: &mut App) {
    let Some(modal) = app.snippet_modal.as_ref() else {
        return;
    };
    let name = modal.editor_name.text().trim().to_string();
    let prompt = modal
        .editor_prompt
        .text()
        .replace("\r\n", "\n")
        .replace('\r', "\n");
    if name.is_empty() || prompt.trim().is_empty() {
        if let Some(modal) = app.snippet_modal.as_mut() {
            modal.error = Some("Name and prompt are both required".to_string());
        }
        return;
    }
    if name.chars().any(char::is_control)
        || prompt
            .chars()
            .any(|character| character.is_control() && !matches!(character, '\n' | '\t'))
    {
        if let Some(modal) = app.snippet_modal.as_mut() {
            modal.error = Some("Snippets cannot contain terminal control characters".to_string());
        }
        return;
    }
    if modal.editor_scope == SnippetScope::Project && modal.project_root.is_none() {
        if let Some(modal) = app.snippet_modal.as_mut() {
            modal.error = Some("No Git project detected for project scope".to_string());
        }
        return;
    }

    let editing = modal.editing;
    let target_scope = modal.editor_scope;
    let project_root = modal.project_root.clone();
    let original_global = modal.original_global.clone();
    let original_project = modal.original_project.clone();
    let mut global = modal.global.clone();
    let mut project = modal.project.clone();
    let snippet = crate::config::PromptSnippet { name, prompt };
    let selected = match editing {
        Some((old_scope, index)) if old_scope == target_scope => match target_scope {
            SnippetScope::Global => {
                global[index] = snippet;
                index
            }
            SnippetScope::Project => {
                project[index] = snippet;
                global.len() + index
            }
        },
        Some((old_scope, index)) => {
            match old_scope {
                SnippetScope::Global => {
                    global.remove(index);
                }
                SnippetScope::Project => {
                    project.remove(index);
                }
            }
            match target_scope {
                SnippetScope::Global => {
                    global.push(snippet);
                    global.len() - 1
                }
                SnippetScope::Project => {
                    project.push(snippet);
                    global.len() + project.len() - 1
                }
            }
        }
        None => match target_scope {
            SnippetScope::Global => {
                global.push(snippet);
                global.len() - 1
            }
            SnippetScope::Project => {
                project.push(snippet);
                global.len() + project.len() - 1
            }
        },
    };

    let (global_dirty, project_dirty) = match editing {
        Some((old_scope, _)) if old_scope != target_scope => (true, true),
        _ => match target_scope {
            SnippetScope::Global => (true, false),
            SnippetScope::Project => (false, true),
        },
    };
    let update = crate::snippets::SnippetUpdate {
        global: global.clone(),
        project: project.clone(),
        original_global,
        original_project,
        project_root,
        global_dirty,
        project_dirty,
    };
    match app.persist_snippets(&update) {
        Ok(()) => {
            let modal = app.snippet_modal.as_mut().expect("modal remains open");
            modal.global = global;
            modal.project = project;
            modal.original_global = modal.global.clone();
            modal.original_project = modal.project.clone();
            modal.selected = selected;
            modal.cancel_subscreen();
        }
        Err(error) => {
            if let Some(modal) = app.snippet_modal.as_mut() {
                modal.error = Some(format!("Cannot save snippet: {error}"));
            }
        }
    }
}

fn delete_selected_snippet(app: &mut App) {
    let Some(modal) = app.snippet_modal.as_ref() else {
        return;
    };
    let Some((scope, index, _)) = modal.selected_entry() else {
        return;
    };
    let project_root = modal.project_root.clone();
    let original_global = modal.original_global.clone();
    let original_project = modal.original_project.clone();
    let mut global = modal.global.clone();
    let mut project = modal.project.clone();
    match scope {
        SnippetScope::Global => {
            global.remove(index);
        }
        SnippetScope::Project => {
            project.remove(index);
        }
    }
    let update = crate::snippets::SnippetUpdate {
        global: global.clone(),
        project: project.clone(),
        original_global,
        original_project,
        project_root,
        global_dirty: scope == SnippetScope::Global,
        project_dirty: scope == SnippetScope::Project,
    };
    match app.persist_snippets(&update) {
        Ok(()) => {
            let modal = app.snippet_modal.as_mut().expect("modal remains open");
            modal.global = global;
            modal.project = project;
            modal.original_global = modal.global.clone();
            modal.original_project = modal.project.clone();
            modal.selected = modal.selected.min(modal.len().saturating_sub(1));
            modal.cancel_subscreen();
        }
        Err(error) => {
            if let Some(modal) = app.snippet_modal.as_mut() {
                modal.screen = SnippetScreen::List;
                modal.error = Some(format!("Cannot delete snippet: {error}"));
            }
        }
    }
}

fn use_selected_snippet(app: &mut App) {
    let selected = app.snippet_modal.as_ref().map(|modal| modal.selected);
    if let Some(selected) = selected {
        use_snippet_at(app, selected);
    }
}

fn use_snippet_at(app: &mut App, index: usize) {
    let prompt = app
        .snippet_modal
        .as_ref()
        .and_then(|modal| modal.entry(index))
        .map(|(_, _, snippet)| snippet.prompt.clone());
    let Some(prompt) = prompt else {
        return;
    };
    if let Some(pane) = app.mux.as_mut().and_then(|mux| mux.focused_pane_mut()) {
        if let Err(error) = pane.send_prompt_snippet(&prompt) {
            if let Some(modal) = app.snippet_modal.as_mut() {
                modal.error = Some(format!("Cannot paste snippet: {error}"));
            }
            return;
        }
    }
    app.snippet_modal = None;
    app.close_github_inspector();
    app.workspace_help = None;
    focus_chat(app);
}

pub fn handle_command_palette_event(app: &mut App, event: Event) {
    let commands = crate::command_palette::filtered_commands(app);
    match event {
        Event::Paste(text) => {
            if let Some(palette) = app.command_palette.as_mut() {
                palette
                    .query
                    .extend(text.chars().filter(|character| !character.is_control()));
                palette.reset_filter();
            }
        }
        Event::Mouse(mouse) => match mouse.kind {
            MouseEventKind::ScrollUp => move_command_palette(app, -3),
            MouseEventKind::ScrollDown => move_command_palette(app, 3),
            MouseEventKind::Down(MouseButton::Left) => {
                let clicked = app.command_palette.as_ref().and_then(|palette| {
                    palette
                        .hits
                        .iter()
                        .find(|(area, _)| contains(*area, mouse.column, mouse.row))
                        .map(|(_, command)| *command)
                });
                if let Some(command) = clicked {
                    if let Some(index) = commands.iter().position(|entry| entry.id == command) {
                        if let Some(palette) = app.command_palette.as_mut() {
                            palette.set_selected(index, commands.len());
                        }
                        execute_command_palette_selection(app);
                    }
                }
            }
            _ => {}
        },
        Event::Key(key) if key.kind == KeyEventKind::Press => {
            if key.code == KeyCode::Esc {
                app.close_command_palette();
                return;
            }
            match key.code {
                KeyCode::Enter => execute_command_palette_selection(app),
                KeyCode::Up => move_command_palette(app, -1),
                KeyCode::Down => move_command_palette(app, 1),
                KeyCode::PageUp => {
                    let page = app
                        .command_palette
                        .as_ref()
                        .map(|palette| palette.visible_rows.max(1))
                        .unwrap_or(1);
                    move_command_palette(app, -(page as isize));
                }
                KeyCode::PageDown => {
                    let page = app
                        .command_palette
                        .as_ref()
                        .map(|palette| palette.visible_rows.max(1))
                        .unwrap_or(1);
                    move_command_palette(app, page as isize);
                }
                KeyCode::Home => {
                    if let Some(palette) = app.command_palette.as_mut() {
                        palette.set_selected(0, commands.len());
                    }
                }
                KeyCode::End => {
                    if let Some(palette) = app.command_palette.as_mut() {
                        palette.set_selected(commands.len().saturating_sub(1), commands.len());
                    }
                }
                KeyCode::Backspace => {
                    if let Some(palette) = app.command_palette.as_mut() {
                        palette.query.pop();
                        palette.reset_filter();
                    }
                }
                KeyCode::Char(character)
                    if !key.modifiers.intersects(
                        crossterm::event::KeyModifiers::CONTROL
                            | crossterm::event::KeyModifiers::ALT,
                    ) =>
                {
                    if let Some(palette) = app.command_palette.as_mut() {
                        palette.query.push(character);
                        palette.reset_filter();
                    }
                }
                _ => {}
            }
        }
        _ => {}
    }
}

fn move_command_palette(app: &mut App, delta: isize) {
    let count = crate::command_palette::filtered_commands(app).len();
    if let Some(palette) = app.command_palette.as_mut() {
        palette.move_selection(delta, count);
    }
}

fn execute_command_palette_selection(app: &mut App) {
    let commands = crate::command_palette::filtered_commands(app);
    let selected = app
        .command_palette
        .as_ref()
        .map(|palette| palette.selected)
        .unwrap_or_default();
    let Some(command) = commands.get(selected) else {
        return;
    };
    if !command.enabled {
        if let Some(palette) = app.command_palette.as_mut() {
            palette.error = command.unavailable_reason.map(str::to_string);
        }
        return;
    }
    let command = command.id;
    app.close_command_palette();
    execute_palette_command(app, command);
}

fn execute_palette_command(app: &mut App, command: CommandId) {
    use CommandId::*;
    match command {
        FocusChat => {
            close_context_overlays(app);
            focus_chat(app);
        }
        ToggleScratchpad => {
            close_context_overlays(app);
            toggle_attached_scratchpad(app);
        }
        ToggleTerminal => {
            close_context_overlays(app);
            toggle_attached_terminal(app);
        }
        OpenSnippets => app.open_snippets(),
        MoveTab => {
            close_context_overlays(app);
            enter_transient_mode(app, TransientMode::MoveTab);
        }
        SplitSideBySide | SplitStacked | ZoomSplit | UnsplitSession | ResizeSplit
        | EqualizeSplits => {
            close_context_overlays(app);
            run_split_command(
                app,
                match command {
                    SplitSideBySide => PrefixCommand::Split(crate::mux::SplitDirection::Columns),
                    SplitStacked => PrefixCommand::Split(crate::mux::SplitDirection::Rows),
                    ZoomSplit => PrefixCommand::ZoomSplit,
                    ResizeSplit => PrefixCommand::ResizeSplit,
                    EqualizeSplits => PrefixCommand::EqualizeSplits,
                    _ => PrefixCommand::UnsplitFocused,
                },
            );
        }
        OpenScratchpadHelp => app.workspace_help = Some(WorkspaceHelp::Scratchpad),
        BackToSessionList => {
            close_context_overlays(app);
            app.detach();
        }
        SwitchSession => {
            close_context_overlays(app);
            app.open_pane_list();
        }
        NextSession => {
            close_context_overlays(app);
            let from_list = matches!(app.view, View::List);
            if let Some(mux) = app.mux.as_mut() {
                mux.cycle(true);
            }
            if from_list {
                attach_focused(app);
            } else {
                sync_workspace_panels(app);
            }
        }
        PreviousSession => {
            close_context_overlays(app);
            let from_list = matches!(app.view, View::List);
            if let Some(mux) = app.mux.as_mut() {
                mux.cycle(false);
            }
            if from_list {
                attach_focused(app);
            } else {
                sync_workspace_panels(app);
            }
        }
        InspectGithub => app.open_github_inspector(),
        GlobalSettings => app.begin_global_settings(),
        ProjectSettings => crate::input::begin_project_settings(app),
        OpenHelp => {
            app.help_scroll = 0;
            app.mode = crate::app::Mode::Help;
        }
        CheckForUpdates => app.request_update(),
        ThreadInbox => app.open_thread_inbox(),
        WhatsNew => {
            app.whats_new = Some(crate::ui::whats_new::WhatsNewScreen::new(
                crate::changelog::whats_new_for_current_version(),
            ));
        }
        SendLiteralPrefix => {
            let key = app.mux.as_ref().map(|mux| mux.prefix.literal_key_event());
            if let (Some(key), Some(pane)) =
                (key, app.mux.as_mut().and_then(|mux| mux.focused_pane_mut()))
            {
                if let Err(error) = pane.send_key(&key) {
                    app.status_message = Some(format!("Cannot send prefix: {error}"));
                }
            }
        }
        EndSession => {
            close_context_overlays(app);
            kill_focused(app);
        }
        EndPersistentSession => {
            close_context_overlays(app);
            end_focused_tmux(app);
        }
        Quit => request_quit(app),
        NewSession
        | NewTmuxSession
        | NewWorktreeSession
        | NewTmuxWorktreeSession
        | ResumeSelectedInTmux => {
            close_context_overlays(app);
            crate::input::execute_palette_list_command(app, command);
        }
        SearchSessions | FilterProject | ClearProjectFilter | CycleSort | ToggleHiddenSessions
        | RecoverWorkspace => {
            close_context_overlays(app);
            if matches!(app.view, View::Attached(_)) {
                app.detach();
            }
            crate::input::execute_palette_list_command(app, command);
        }
        ResumeSelected
        | OpenSelectedScratchpad
        | ToggleFavorite
        | ReorderFavorite
        | OpenFavoriteTabs
        | RenameSelected
        | DeleteSelected => crate::input::execute_palette_list_command(app, command),
    }
    sync_view(app);
}

fn close_context_overlays(app: &mut App) {
    app.close_github_inspector();
    app.snippet_modal = None;
    app.workspace_help = None;
}

fn handle_attached_key(app: &mut App, key: KeyEvent) {
    let Some((prefix_state, prefix)) = app.mux.as_ref().map(|mux| (mux.prefix_state, mux.prefix))
    else {
        return;
    };

    if prefix_state == PrefixState::Help {
        if let Some(mux) = app.mux.as_mut() {
            mux.prefix_state = PrefixState::Idle;
        }
        if matches!(resolve_help_command(&key), Some(HelpCommand::Scratchpad)) {
            app.workspace_help = Some(WorkspaceHelp::Scratchpad);
        }
        return;
    }

    if prefix_state == PrefixState::Github {
        if let Some(mux) = app.mux.as_mut() {
            mux.prefix_state = PrefixState::Idle;
        }
        match resolve_github_command(&key) {
            Some(GithubCommand::Inspect) => app.open_github_inspector(),
            Some(GithubCommand::Messages) => app.open_thread_inbox(),
            _ => {}
        }
        return;
    }

    if prefix_state == PrefixState::Layout {
        if let Some(mux) = app.mux.as_mut() {
            mux.prefix_state = PrefixState::Idle;
        }
        if let Some(command) = resolve_layout_command(&key) {
            close_context_overlays(app);
            run_split_command(app, command);
        }
        return;
    }

    // A transient mode keeps acting until it is dismissed, so it is checked before
    // the one-shot menus and only leaves on its own terms.
    if let PrefixState::Transient(mode) = prefix_state {
        match resolve_transient_command(mode, &key) {
            TransientCommand::Step(forward) => {
                step_transient_mode(app, mode, forward);
                return;
            }
            TransientCommand::Resize(axis, grow) => {
                app.status_message = (!resize_focused_split(app, axis, grow)).then(|| {
                    if grow {
                        "No room left to grow this split that way"
                    } else {
                        "This split cannot shrink that way"
                    }
                    .to_string()
                });
                return;
            }
            TransientCommand::Leave => {
                leave_transient_mode(app);
                return;
            }
            TransientCommand::Passthrough => {
                leave_transient_mode(app);
            }
        }
    }

    if prefix_state == PrefixState::Root {
        let command = resolve_prefix_command_with_tmux_keys(
            &key,
            &prefix,
            app.tmux_support
                .is_available()
                .then_some(&app.config.tmux_keys),
        );
        if let Some(mux) = app.mux.as_mut() {
            mux.prefix_state = PrefixState::Idle;
        }
        match command {
            Some(PrefixCommand::CommandPalette) => app.open_command_palette(),
            Some(PrefixCommand::Detach) => {
                close_context_overlays(app);
                app.detach();
            }
            Some(PrefixCommand::NextPane) => {
                close_context_overlays(app);
                if let Some(mux) = app.mux.as_mut() {
                    mux.cycle(true);
                }
                sync_workspace_panels(app);
            }
            Some(PrefixCommand::PreviousPane) => {
                close_context_overlays(app);
                if let Some(mux) = app.mux.as_mut() {
                    mux.cycle(false);
                }
                sync_workspace_panels(app);
            }
            Some(PrefixCommand::KillPane) => {
                close_context_overlays(app);
                kill_focused(app);
            }
            Some(PrefixCommand::EndPersistentSession) => {
                close_context_overlays(app);
                end_focused_tmux(app);
            }
            Some(PrefixCommand::Quit) => {
                app.request_quit_from_pane();
                return;
            }
            Some(PrefixCommand::PaneList) => {
                close_context_overlays(app);
                app.open_pane_list();
                return;
            }
            Some(PrefixCommand::Chat) => {
                close_context_overlays(app);
                focus_chat(app);
            }
            Some(PrefixCommand::Scratchpad) => {
                close_context_overlays(app);
                toggle_attached_scratchpad(app);
            }
            Some(PrefixCommand::Terminal) => {
                close_context_overlays(app);
                toggle_attached_terminal(app);
            }
            Some(PrefixCommand::Snippets) => app.open_snippets(),
            Some(PrefixCommand::Update) => app.request_update(),
            Some(PrefixCommand::Help) => {
                if let Some(mux) = app.mux.as_mut() {
                    mux.prefix_state = PrefixState::Help;
                }
            }
            Some(PrefixCommand::Layout) => {
                if let Some(mux) = app.mux.as_mut() {
                    mux.prefix_state = PrefixState::Layout;
                }
            }
            Some(PrefixCommand::Github) => {
                if let Some(mux) = app.mux.as_mut() {
                    mux.prefix_state = PrefixState::Github;
                }
            }
            Some(PrefixCommand::MoveTab) => {
                close_context_overlays(app);
                enter_transient_mode(app, TransientMode::MoveTab);
                return;
            }
            Some(PrefixCommand::SelectIndex(index)) => {
                close_context_overlays(app);
                // Panes are labelled from 1 in the UI.
                if let Some(mux) = app.mux.as_mut() {
                    mux.select_index(index.saturating_sub(1));
                }
                sync_workspace_panels(app);
            }
            Some(
                command @ (PrefixCommand::Split(_)
                | PrefixCommand::FocusSplit(_)
                | PrefixCommand::ZoomSplit
                | PrefixCommand::UnsplitFocused
                | PrefixCommand::ResizeSplit
                | PrefixCommand::EqualizeSplits),
            ) => {
                close_context_overlays(app);
                run_split_command(app, command);
                return;
            }
            Some(PrefixCommand::Cancel) | None => {}
        }
        sync_view(app);
        return;
    }

    if prefix.matches(&key) {
        if let Some(mux) = app.mux.as_mut() {
            mux.prefix_state = PrefixState::Root;
        }
        return;
    }

    if key_resolves_permission(&key) {
        clear_focused_permission_wait(app);
    }
    if let Some(pane) = app.mux.as_mut().and_then(|mux| mux.focused_pane_mut()) {
        if pane.is_running() {
            // Noted before sending: a thread wake-up must not paste itself onto a
            // half-written message and submit the pair.
            pane.note_user_key(&key);
            let _ = pane.send_key(&key);
        } else if matches!(
            key.code,
            crossterm::event::KeyCode::Enter | crossterm::event::KeyCode::Esc
        ) {
            // A dead pane keeps its final screen until dismissed.
            kill_focused(app);
        } else if matches!(key.code, crossterm::event::KeyCode::Char('r' | 'R')) {
            // Only reachable once the session is dead, so this cannot swallow an
            // ordinary keystroke meant for Copilot.
            restart_focused(app);
        }
    }
}

fn clear_focused_permission_wait(app: &mut App) {
    let changed = app
        .mux
        .as_mut()
        .and_then(|mux| mux.focused_pane_mut())
        .is_some_and(|pane| pane.clear_hook_permission_wait());
    if changed {
        sync_outer_progress(app);
    }
}

fn key_resolves_permission(key: &KeyEvent) -> bool {
    if key.modifiers.intersects(
        KeyModifiers::CONTROL
            | KeyModifiers::ALT
            | KeyModifiers::META
            | KeyModifiers::SUPER
            | KeyModifiers::HYPER,
    ) {
        return false;
    }
    match key.code {
        KeyCode::Enter | KeyCode::Esc => !key.modifiers.contains(KeyModifiers::SHIFT),
        KeyCode::Char('y' | 'n') => key.modifiers.is_empty(),
        KeyCode::Char('Y' | 'N') => {
            key.modifiers.is_empty() || key.modifiers == KeyModifiers::SHIFT
        }
        _ => false,
    }
}

fn handle_github_inspector_event(app: &mut App, event: Event) {
    match event {
        Event::Paste(text) => {
            let Some(inspector) = app.github_inspector.as_mut() else {
                return;
            };
            if matches!(
                inspector.screen,
                crate::app::GithubInspectorScreen::NumberPrompt
            ) {
                inspector
                    .input
                    .extend(text.chars().filter(|character| !character.is_control()));
                inspector.prompt_error = None;
            }
        }
        Event::Mouse(mouse) => match mouse.kind {
            MouseEventKind::ScrollUp => scroll_github_at(app, mouse.column, mouse.row, -3),
            MouseEventKind::ScrollDown => scroll_github_at(app, mouse.column, mouse.row, 3),
            MouseEventKind::Down(MouseButton::Left) => {
                // A press on a scrollbar owns the gesture; without this it would fall
                // through and be read as a click on the pane behind the track.
                if grab_github_scrollbar(app, mouse.column, mouse.row) {
                    return;
                }
                click_github_inspector(app, mouse.column, mouse.row)
            }
            MouseEventKind::Drag(MouseButton::Left) => {
                drag_github_scrollbar(app, mouse.row);
            }
            MouseEventKind::Up(_) => {
                if let Some(inspector) = app.github_inspector.as_mut() {
                    inspector.scrollbar_drag = None;
                }
            }
            _ => {}
        },
        Event::Key(key) if key.kind == KeyEventKind::Press => handle_github_inspector_key(app, key),
        _ => {}
    }
}

fn handle_github_inspector_key(app: &mut App, key: KeyEvent) {
    let Some(inspector) = app.github_inspector.as_ref() else {
        return;
    };
    match inspector.screen {
        crate::app::GithubInspectorScreen::NumberPrompt => match key.code {
            KeyCode::Esc => app.close_github_inspector(),
            KeyCode::Enter => app.submit_github_number(),
            KeyCode::Backspace => {
                if let Some(inspector) = app.github_inspector.as_mut() {
                    inspector.input.pop();
                    inspector.prompt_error = None;
                }
            }
            KeyCode::Char(character)
                if !key.modifiers.intersects(
                    crossterm::event::KeyModifiers::CONTROL | crossterm::event::KeyModifiers::ALT,
                ) =>
            {
                if let Some(inspector) = app.github_inspector.as_mut() {
                    inspector.input.push(character);
                    inspector.prompt_error = None;
                }
            }
            _ => {}
        },
        crate::app::GithubInspectorScreen::Loading => {
            if matches!(key.code, KeyCode::Esc | KeyCode::Char('q')) {
                app.close_github_inspector();
            }
        }
        crate::app::GithubInspectorScreen::Choose { .. } => match key.code {
            KeyCode::Char('i') => {
                app.choose_github_item(crate::github::GithubLookupKind::IssueOrPullRequest)
            }
            KeyCode::Char('d') => {
                app.choose_github_item(crate::github::GithubLookupKind::Discussion)
            }
            KeyCode::Esc | KeyCode::Char('q') => app.close_github_inspector(),
            _ => {}
        },
        crate::app::GithubInspectorScreen::Error(_) => match key.code {
            KeyCode::Esc | KeyCode::Char('q') => app.close_github_inspector(),
            KeyCode::Char('r') => app.retry_github_request(),
            _ => {}
        },
        crate::app::GithubInspectorScreen::Ready(_) => handle_ready_github_key(app, key),
    }
}

fn handle_ready_github_key(app: &mut App, key: KeyEvent) {
    let files_tab = app.github_inspector.as_ref().is_some_and(|inspector| {
        inspector.tab == crate::app::GithubTab::Files
            && inspector
                .ready_item()
                .is_some_and(|item| item.is_pull_request())
    });
    match key.code {
        KeyCode::Tab
            if key
                .modifiers
                .contains(crossterm::event::KeyModifiers::SHIFT) =>
        {
            if let Some(inspector) = app.github_inspector.as_mut() {
                inspector.cycle_tab(false);
            }
        }
        KeyCode::Tab => {
            if let Some(inspector) = app.github_inspector.as_mut() {
                inspector.cycle_tab(true);
            }
        }
        KeyCode::BackTab => {
            if let Some(inspector) = app.github_inspector.as_mut() {
                inspector.cycle_tab(false);
            }
        }
        // `q` leaves the inspector outright, wherever you are in it; Esc only
        // steps back out of the diff.
        KeyCode::Char('q') => app.close_github_inspector(),
        KeyCode::Esc => {
            let in_diff = app
                .github_inspector
                .as_ref()
                .is_some_and(|inspector| inspector.files_pane == crate::app::FilesPane::Diff);
            if files_tab && in_diff {
                if let Some(inspector) = app.github_inspector.as_mut() {
                    inspector.files_pane = crate::app::FilesPane::Tree;
                }
            } else {
                app.close_github_inspector();
            }
        }
        KeyCode::Up => scroll_github_inspector(app, -1),
        KeyCode::Down => scroll_github_inspector(app, 1),
        KeyCode::PageUp => scroll_github_inspector(app, -10),
        KeyCode::PageDown => scroll_github_inspector(app, 10),
        KeyCode::Home => set_github_scroll_boundary(app, false),
        KeyCode::End => set_github_scroll_boundary(app, true),
        KeyCode::Left if files_tab => collapse_or_ascend_github_tree(app),
        KeyCode::Right if files_tab => expand_or_enter_github_tree(app),
        KeyCode::Left => scroll_github_diff_horizontal(app, -4),
        KeyCode::Right => scroll_github_diff_horizontal(app, 4),
        KeyCode::Enter if files_tab => activate_github_tree_row(app),
        KeyCode::Char('r') => app.request_copilot_review(),
        KeyCode::Char('f') => {
            if let Some(inspector) = app.github_inspector.as_mut() {
                inspector.cycle_comment_filter();
            }
        }
        _ => {}
    }
}

/// Scroll whichever pane the pointer is over, so the wheel works without
/// changing focus first.
fn scroll_github_at(app: &mut App, column: u16, row: u16, amount: isize) {
    let target = app.github_inspector.as_ref().and_then(|inspector| {
        if inspector.tab != crate::app::GithubTab::Files {
            return None;
        }
        if contains(inspector.diff_area, column, row) {
            Some(crate::app::FilesPane::Diff)
        } else if contains(inspector.tree_area, column, row) {
            Some(crate::app::FilesPane::Tree)
        } else {
            None
        }
    });

    match target {
        Some(crate::app::FilesPane::Diff) => scroll_github_diff(app, amount),
        // The tree scrolls under the pointer without dragging the selection along.
        Some(crate::app::FilesPane::Tree) => scroll_github_tree_view(app, amount),
        None => scroll_github_inspector(app, amount),
    }
}

fn scroll_github_tree_view(app: &mut App, amount: isize) {
    let row_count = github_tree_rows(app).len();
    let Some(inspector) = app.github_inspector.as_mut() else {
        return;
    };
    let max_offset = row_count.saturating_sub(inspector.visible_tree_rows.max(1));
    let next = if amount < 0 {
        inspector.tree_offset.saturating_sub(amount.unsigned_abs())
    } else {
        inspector.tree_offset.saturating_add(amount as usize)
    };
    inspector.tree_offset = next.min(max_offset);
}

/// Clicking a pane focuses it, and clicking a tree row selects that row.
/// Takes hold of the scrollbar under the pointer, if there is one, and jumps to the
/// grabbed position. Returns whether the press was claimed.
fn grab_github_scrollbar(app: &mut App, column: u16, row: u16) -> bool {
    let Some(bar) = github_scrollbar_at(app, column, row) else {
        return false;
    };
    if let Some(inspector) = app.github_inspector.as_mut() {
        inspector.scrollbar_drag = Some(bar);
        // Grabbing a pane's bar is also a claim on that pane, the same as clicking
        // into it, so the keys follow the scrollbar the user just reached for.
        match bar {
            GithubScrollbar::Tree => inspector.files_pane = crate::app::FilesPane::Tree,
            GithubScrollbar::Diff => inspector.files_pane = crate::app::FilesPane::Diff,
            GithubScrollbar::Body => {}
        }
    }
    drag_github_scrollbar(app, row);
    true
}

/// The scrollbar covering `column`/`row`, if one is drawn there.
///
/// A bar only exists while its content overflows, which is exactly when its maximum
/// offset is non-zero, so that doubles as the "is it on screen" test.
fn github_scrollbar_at(app: &App, column: u16, row: u16) -> Option<GithubScrollbar> {
    let inspector = app.github_inspector.as_ref()?;
    let files = inspector.tab == crate::app::GithubTab::Files;
    let candidates = [
        (GithubScrollbar::Body, inspector.body_area, !files),
        (GithubScrollbar::Tree, inspector.tree_area, files),
        (GithubScrollbar::Diff, inspector.diff_area, files),
    ];
    candidates
        .into_iter()
        .find(|(bar, area, on_this_tab)| {
            *on_this_tab
                && github_scroll_range(app, *bar) > 0
                && area.height > 0
                && area.width > 0
                // Ratatui draws a vertical-right scrollbar in the last column.
                && column == area.right() - 1
                && row >= area.y
                && row < area.bottom()
        })
        .map(|(bar, _, _)| bar)
}

/// Scroll the held bar to `row`, clamped to its track.
///
/// The row maps linearly across the whole track rather than tracking the thumb's own
/// travel, which is what makes the two ends reachable: the last row of the track is
/// the last line of the content, however tall the thumb happens to be.
fn drag_github_scrollbar(app: &mut App, row: u16) {
    let Some(bar) = app
        .github_inspector
        .as_ref()
        .and_then(|inspector| inspector.scrollbar_drag)
    else {
        return;
    };
    let max = github_scroll_range(app, bar);
    let Some(area) = github_scrollbar_area(app, bar) else {
        return;
    };
    let travel = area.height.saturating_sub(1);
    let offset = if travel == 0 {
        // A one-row track has nowhere to aim, so either end is the whole range.
        max
    } else {
        let position = row.clamp(area.y, area.bottom() - 1) - area.y;
        // Round to the nearest line rather than truncating, so the thumb follows the
        // pointer instead of lagging half a line behind it all the way down.
        (position as usize * max + travel as usize / 2) / travel as usize
    };
    set_github_scroll(app, bar, offset.min(max));
}

fn github_scrollbar_area(app: &App, bar: GithubScrollbar) -> Option<Rect> {
    let inspector = app.github_inspector.as_ref()?;
    Some(match bar {
        GithubScrollbar::Body => inspector.body_area,
        GithubScrollbar::Tree => inspector.tree_area,
        GithubScrollbar::Diff => inspector.diff_area,
    })
}

fn github_scroll_range(app: &App, bar: GithubScrollbar) -> usize {
    let Some(inspector) = app.github_inspector.as_ref() else {
        return 0;
    };
    match bar {
        GithubScrollbar::Body => inspector.max_scroll,
        GithubScrollbar::Tree => github_tree_rows(app)
            .len()
            .saturating_sub(inspector.visible_tree_rows.max(1)),
        GithubScrollbar::Diff => inspector.max_diff_scroll,
    }
}

fn set_github_scroll(app: &mut App, bar: GithubScrollbar, offset: usize) {
    let Some(inspector) = app.github_inspector.as_mut() else {
        return;
    };
    match bar {
        GithubScrollbar::Body => inspector.set_active_scroll(offset),
        GithubScrollbar::Tree => inspector.tree_offset = offset,
        GithubScrollbar::Diff => inspector.diff_scroll = offset,
    }
}

fn click_github_inspector(app: &mut App, column: u16, row: u16) {
    let Some(inspector) = app.github_inspector.as_ref() else {
        return;
    };
    if inspector.tab != crate::app::GithubTab::Files {
        return;
    }
    if contains(inspector.diff_area, column, row) {
        if let Some(inspector) = app.github_inspector.as_mut() {
            inspector.files_pane = crate::app::FilesPane::Diff;
        }
        return;
    }
    if !contains(inspector.tree_area, column, row) {
        return;
    }

    let clicked = inspector.tree_offset + (row - inspector.tree_area.y) as usize;
    if let Some(inspector) = app.github_inspector.as_mut() {
        inspector.files_pane = crate::app::FilesPane::Tree;
    }
    select_github_tree_row(app, clicked);
}

/// Rows of the changed-file tree as currently displayed.
fn github_tree_rows(app: &App) -> Vec<crate::ui::file_tree::TreeRow> {
    app.github_inspector
        .as_ref()
        .and_then(|inspector| {
            inspector.ready_item().map(|item| {
                crate::ui::file_tree::build_rows(item.files(), &inspector.collapsed_dirs)
            })
        })
        .unwrap_or_default()
}

/// Move the tree cursor, keeping the diff pane showing the selected file.
fn select_github_tree_row(app: &mut App, row: usize) {
    let rows = github_tree_rows(app);
    let Some(inspector) = app.github_inspector.as_mut() else {
        return;
    };
    if rows.is_empty() {
        return;
    }
    let row = row.min(rows.len() - 1);
    inspector.tree_selected = row;
    if let Some(index) = rows[row].file_index() {
        if inspector.selected_file != index {
            inspector.selected_file = index;
            inspector.diff_scroll = 0;
            inspector.diff_horizontal = 0;
        }
    }
}

fn collapse_or_ascend_github_tree(app: &mut App) {
    let in_diff = app
        .github_inspector
        .as_ref()
        .is_some_and(|inspector| inspector.files_pane == crate::app::FilesPane::Diff);
    if in_diff {
        scroll_github_diff_horizontal(app, -4);
        return;
    }

    let rows = github_tree_rows(app);
    let Some(inspector) = app.github_inspector.as_ref() else {
        return;
    };
    let selected = inspector.tree_selected;
    let Some(row) = rows.get(selected) else {
        return;
    };

    // An open directory folds; anything else steps out to its parent.
    if let crate::ui::file_tree::RowKind::Directory { path, expanded, .. } = &row.kind {
        if *expanded {
            let path = path.clone();
            if let Some(inspector) = app.github_inspector.as_mut() {
                inspector.collapsed_dirs.insert(path);
            }
            return;
        }
    }
    if let Some(parent) = crate::ui::file_tree::parent_row(&rows, selected) {
        select_github_tree_row(app, parent);
    }
}

fn expand_or_enter_github_tree(app: &mut App) {
    let in_diff = app
        .github_inspector
        .as_ref()
        .is_some_and(|inspector| inspector.files_pane == crate::app::FilesPane::Diff);
    if in_diff {
        scroll_github_diff_horizontal(app, 4);
        return;
    }

    let rows = github_tree_rows(app);
    let Some(inspector) = app.github_inspector.as_ref() else {
        return;
    };
    let selected = inspector.tree_selected;
    let Some(row) = rows.get(selected) else {
        return;
    };

    match &row.kind {
        crate::ui::file_tree::RowKind::Directory { path, expanded, .. } => {
            if *expanded {
                select_github_tree_row(app, selected + 1);
            } else {
                let path = path.clone();
                if let Some(inspector) = app.github_inspector.as_mut() {
                    inspector.collapsed_dirs.remove(&path);
                }
            }
        }
        crate::ui::file_tree::RowKind::File { .. } => {
            if let Some(inspector) = app.github_inspector.as_mut() {
                inspector.files_pane = crate::app::FilesPane::Diff;
            }
        }
    }
}

/// Enter folds a directory or moves focus into the diff.
fn activate_github_tree_row(app: &mut App) {
    let in_diff = app
        .github_inspector
        .as_ref()
        .is_some_and(|inspector| inspector.files_pane == crate::app::FilesPane::Diff);
    if in_diff {
        return;
    }

    let rows = github_tree_rows(app);
    let Some(inspector) = app.github_inspector.as_ref() else {
        return;
    };
    let Some(row) = rows.get(inspector.tree_selected) else {
        return;
    };
    match &row.kind {
        crate::ui::file_tree::RowKind::Directory { path, expanded, .. } => {
            let (path, expanded) = (path.clone(), *expanded);
            if let Some(inspector) = app.github_inspector.as_mut() {
                if expanded {
                    inspector.collapsed_dirs.insert(path);
                } else {
                    inspector.collapsed_dirs.remove(&path);
                }
            }
        }
        crate::ui::file_tree::RowKind::File { .. } => {
            if let Some(inspector) = app.github_inspector.as_mut() {
                inspector.files_pane = crate::app::FilesPane::Diff;
            }
        }
    }
}

fn scroll_github_inspector(app: &mut App, amount: isize) {
    let Some(inspector) = app.github_inspector.as_ref() else {
        return;
    };
    let files_tab = inspector.tab == crate::app::GithubTab::Files
        && inspector
            .ready_item()
            .is_some_and(|item| item.is_pull_request());

    if files_tab {
        if inspector.files_pane == crate::app::FilesPane::Diff {
            scroll_github_diff(app, amount);
        } else {
            let selected = inspector.tree_selected;
            let next = if amount < 0 {
                selected.saturating_sub(amount.unsigned_abs())
            } else {
                selected.saturating_add(amount as usize)
            };
            select_github_tree_row(app, next);
        }
        return;
    }

    if let Some(inspector) = app.github_inspector.as_mut() {
        inspector.scroll_active_by(amount);
    }
}

fn scroll_github_diff(app: &mut App, amount: isize) {
    let Some(inspector) = app.github_inspector.as_mut() else {
        return;
    };
    let next = if amount < 0 {
        inspector.diff_scroll.saturating_sub(amount.unsigned_abs())
    } else {
        inspector.diff_scroll.saturating_add(amount as usize)
    };
    inspector.diff_scroll = next.min(inspector.max_diff_scroll);
}

fn set_github_scroll_boundary(app: &mut App, end: bool) {
    let Some(inspector) = app.github_inspector.as_ref() else {
        return;
    };
    let files_tab = inspector.tab == crate::app::GithubTab::Files
        && inspector
            .ready_item()
            .is_some_and(|item| item.is_pull_request());

    if files_tab {
        if inspector.files_pane == crate::app::FilesPane::Diff {
            if let Some(inspector) = app.github_inspector.as_mut() {
                inspector.diff_scroll = if end { inspector.max_diff_scroll } else { 0 };
            }
        } else {
            let rows = github_tree_rows(app);
            if !rows.is_empty() {
                select_github_tree_row(app, if end { rows.len() - 1 } else { 0 });
            }
        }
        return;
    }

    if let Some(inspector) = app.github_inspector.as_mut() {
        inspector.set_active_scroll(if end { inspector.max_scroll } else { 0 });
    }
}

fn scroll_github_diff_horizontal(app: &mut App, amount: isize) {
    let Some(inspector) = app.github_inspector.as_mut() else {
        return;
    };
    let next = if amount < 0 {
        inspector
            .diff_horizontal
            .saturating_sub(amount.unsigned_abs())
    } else {
        inspector.diff_horizontal.saturating_add(amount as usize)
    };
    inspector.diff_horizontal = next.min(inspector.max_diff_horizontal);
}

fn handle_scratchpad_event(app: &mut App, event: Event) {
    let Some(scratchpad) = app.scratchpad.as_mut() else {
        app.workspace_focus = WorkspaceFocus::Chat;
        return;
    };
    let outcome = scratchpad.handle_event(event);
    match outcome {
        Ok(crate::scratchpad::InputOutcome::Continue) => {}
        Ok(crate::scratchpad::InputOutcome::Close) => {
            let context = focused_workspace_context(app);
            app.scratchpad = None;
            app.scratchpad_owner = None;
            if let Some((pane_id, session_id, _, _)) = context {
                app.remember_scratchpad_panel(pane_id, &session_id, false);
            }
            app.workspace_focus = WorkspaceFocus::Chat;
        }
        Err(error) => {
            scratchpad.status_message = Some(format!("Scratchpad save failed: {error}"));
        }
    }
}

fn handle_terminal_event(app: &mut App, event: Event) {
    if let Some(terminal) = app.terminal.active_mut() {
        if let Err(error) = terminal.handle_event(event) {
            app.status_message = Some(format!("Terminal input failed: {error}"));
        }
    } else {
        app.workspace_focus = WorkspaceFocus::Chat;
    }
}

fn focus_chat(app: &mut App) {
    app.workspace_focus = WorkspaceFocus::Chat;
    app.terminal.unfocus();
}

/// Ctrl+Tab and Ctrl+Shift+Tab, and Ctrl+PageDown and Ctrl+PageUp, as in a browser:
/// `Some(true)` for the next tab, `Some(false)` for the previous one.
///
/// Both pairs, because Windows Terminal claims Ctrl+Tab for its own tabs unless the
/// binding is removed, while the page keys reach the application there. Copilot uses
/// neither, so taking them costs it nothing.
pub(crate) fn tab_switch_key(key: &KeyEvent) -> Option<bool> {
    if !key.modifiers.contains(KeyModifiers::CONTROL) || key.modifiers.contains(KeyModifiers::ALT) {
        return None;
    }
    let shift = key.modifiers.contains(KeyModifiers::SHIFT);
    match key.code {
        KeyCode::Tab => Some(!shift),
        // Terminals report Shift+Tab as its own key, with or without the modifier.
        KeyCode::BackTab => Some(false),
        KeyCode::PageDown if !shift => Some(true),
        KeyCode::PageUp if !shift => Some(false),
        _ => None,
    }
}

/// The other sessions sharing the screen, with their session ids.
///
/// In a split the scratchpad and terminal are one dock, kept open while any session
/// on screen has that panel. Closing it therefore closes it for all of them: closing
/// only the focused session's left the dock up with a placeholder in it, and there
/// was no way to make it go away short of visiting every session in turn.
fn other_sessions_on_screen(
    app: &App,
    except: crate::mux::PaneId,
) -> Vec<(crate::mux::PaneId, String)> {
    let Some(mux) = app.mux.as_ref() else {
        return Vec::new();
    };
    app.workspace_areas
        .chats
        .iter()
        .filter(|slot| slot.pane != except)
        .filter_map(|slot| mux.pane(slot.pane))
        .map(|pane| (pane.id, pane.session_id.clone()))
        .collect()
}

fn toggle_attached_scratchpad(app: &mut App) {
    let Some((pane_id, session_id, _, _)) = focused_workspace_context(app) else {
        return;
    };
    if app.scratchpad_owner == Some(pane_id) && app.scratchpad.is_some() {
        if app.workspace_focus == WorkspaceFocus::Scratchpad {
            if let Some(scratchpad) = app.scratchpad.as_mut() {
                if let Err(error) = scratchpad.save() {
                    scratchpad.status_message = Some(format!("Scratchpad save failed: {error}"));
                    return;
                }
            }
            app.scratchpad = None;
            app.scratchpad_owner = None;
            app.remember_scratchpad_panel(pane_id, &session_id, false);
            for (id, session_id) in other_sessions_on_screen(app, pane_id) {
                if app.scratchpad_open.contains(&id) {
                    app.remember_scratchpad_panel(id, &session_id, false);
                }
            }
            app.workspace_focus = WorkspaceFocus::Chat;
        } else {
            app.workspace_focus = WorkspaceFocus::Scratchpad;
            app.terminal.unfocus();
        }
        return;
    }

    if !close_scratchpad(app) {
        return;
    }
    match crate::scratchpad::Scratchpad::open(&session_id) {
        Ok(scratchpad) => {
            app.scratchpad = Some(scratchpad);
            app.scratchpad_owner = Some(pane_id);
            app.remember_scratchpad_panel(pane_id, &session_id, true);
            app.workspace_focus = WorkspaceFocus::Scratchpad;
            app.terminal.unfocus();
        }
        Err(error) => {
            app.status_message = Some(format!("Cannot open scratchpad: {error}"));
        }
    }
}

fn toggle_attached_terminal(app: &mut App) {
    let Some((pane_id, session_id, title, cwd)) = focused_workspace_context(app) else {
        return;
    };
    if app.attached_terminal_visible()
        && app.terminal.active_session_id() == Some(session_id.as_str())
        && app.workspace_focus == WorkspaceFocus::Terminal
    {
        app.terminal.hide();
        app.terminal_owner = None;
        app.remember_terminal_panel(pane_id, &session_id, false);
        // Shells keep running; only the dock goes, as for the focused session.
        for (id, session_id) in other_sessions_on_screen(app, pane_id) {
            if app.terminal_open.contains(&id) {
                app.remember_terminal_panel(id, &session_id, false);
            }
        }
        app.workspace_focus = WorkspaceFocus::Chat;
        return;
    }

    match app
        .terminal
        .activate(session_id.clone(), title, cwd, &app.config.terminal)
    {
        Ok(_) => {
            app.terminal_owner = Some(pane_id);
            app.remember_terminal_panel(pane_id, &session_id, true);
            app.workspace_focus = WorkspaceFocus::Terminal;
        }
        Err(error) => app.status_message = Some(format!("Cannot open terminal: {error}")),
    }
}

fn sync_outer_progress(app: &mut App) {
    let state = app
        .mux
        .as_ref()
        .and_then(|mux| mux.focused_pane())
        .map(|pane| pane.effective_progress_state())
        .unwrap_or(crate::host_terminal::ProgressState::Clear);
    app.host_sequences
        .push(crate::host_terminal::progress_sequence_for_state(state));
}

pub fn sync_workspace_panels(app: &mut App) {
    app.acknowledge_focused_pane();
    sync_outer_progress(app);
    let Some((pane_id, session_id, title, cwd)) = focused_workspace_context(app) else {
        let _ = close_scratchpad(app);
        app.terminal.hide();
        app.terminal_owner = None;
        app.scratchpad_open.clear();
        app.terminal_open.clear();
        app.workspace_focus = WorkspaceFocus::Chat;
        return;
    };

    // With the scratchpad column already up beside a split, a session without one gets
    // its own, empty if it has never had notes, rather than a placeholder telling the
    // user to open it. The terminal dock keeps its placeholder: opening one starts a
    // shell, which clicking between splits should not do.
    if !app.scratchpad_open.contains(&pane_id) && app.scratchpad_dock_visible() {
        app.remember_scratchpad_panel(pane_id, &session_id, true);
    }

    if app.scratchpad_owner != Some(pane_id) || app.scratchpad.is_none() {
        if !close_scratchpad(app) {
            return;
        }

        if app.scratchpad_open.contains(&pane_id) {
            match crate::scratchpad::Scratchpad::open(&session_id) {
                Ok(scratchpad) => {
                    app.scratchpad = Some(scratchpad);
                    app.scratchpad_owner = Some(pane_id);
                }
                Err(error) => {
                    app.scratchpad_open.remove(&pane_id);
                    app.status_message = Some(format!("Cannot switch scratchpad: {error}"));
                }
            }
        }
    }

    if app.terminal_open.contains(&pane_id) {
        let activated = match app
            .terminal
            .activate(session_id, title, cwd, &app.config.terminal)
        {
            Ok(_) => {
                if app.workspace_focus != WorkspaceFocus::Terminal {
                    app.terminal.unfocus();
                }
                true
            }
            Err(error) => {
                app.terminal.hide();
                app.terminal_owner = None;
                app.terminal_open.remove(&pane_id);
                app.status_message = Some(format!("Cannot switch terminal: {error}"));
                false
            }
        };
        if activated {
            app.terminal_owner = Some(pane_id);
        }
    } else {
        app.terminal.hide();
        app.terminal_owner = None;
    }

    if (!app.attached_scratchpad_visible() && app.workspace_focus == WorkspaceFocus::Scratchpad)
        || (!app.attached_terminal_visible() && app.workspace_focus == WorkspaceFocus::Terminal)
    {
        app.workspace_focus = WorkspaceFocus::Chat;
    }
}

fn close_scratchpad(app: &mut App) -> bool {
    if let Some(scratchpad) = app.scratchpad.as_mut() {
        if let Err(error) = scratchpad.save() {
            app.status_message = Some(format!("Scratchpad save failed: {error}"));
            return false;
        }
    }
    app.scratchpad = None;
    app.scratchpad_owner = None;
    true
}

fn enter_transient_mode(app: &mut App, mode: TransientMode) {
    // A mode whose every key would be a no-op is worse than a message saying so:
    // the arrows would silently stop reaching Copilot for no benefit.
    if let Some(reason) = transient_mode_unavailable(app, mode) {
        app.status_message = Some(reason.to_string());
        return;
    }
    if let Some(mux) = app.mux.as_mut() {
        mux.prefix_state = PrefixState::Transient(mode);
    }
    app.status_message = None;
    sync_view(app);
}

fn transient_mode_unavailable(app: &App, mode: TransientMode) -> Option<&'static str> {
    match mode {
        TransientMode::MoveTab => {
            // A lone session has no tab strip to move anything along.
            let panes = app.mux.as_ref().map_or(0, |mux| mux.panes.len());
            (panes < 2).then_some("Only one session is open — nothing to reorder")
        }
        TransientMode::ResizeSplit => (app.workspace_areas.chats.len() < 2)
            .then_some("Only one session is on screen — nothing to resize"),
    }
}

fn leave_transient_mode(app: &mut App) {
    if let Some(mux) = app.mux.as_mut() {
        mux.prefix_state = PrefixState::Idle;
    }
}

/// Applies one step of `mode`. The mode stays open so the next step is one key away.
fn step_transient_mode(app: &mut App, mode: TransientMode, forward: bool) {
    match mode {
        TransientMode::MoveTab => {
            let moved = app
                .mux
                .as_mut()
                .is_some_and(|mux| mux.move_focused_pane(forward));
            if !moved {
                app.status_message = Some(
                    if forward {
                        "Already the last tab"
                    } else {
                        "Already the first tab"
                    }
                    .to_string(),
                );
            } else {
                app.status_message = None;
            }
        }
        // Its keys resolve to `Resize`, which carries the axis a plain step cannot.
        TransientMode::ResizeSplit => {}
    }
}

fn focused_workspace_context(app: &App) -> Option<(u64, String, String, String)> {
    let pane = app.mux.as_ref()?.focused_pane()?;
    Some((
        pane.id,
        pane.session_id.clone(),
        pane.title.clone(),
        pane.cwd.to_string_lossy().to_string(),
    ))
}

/// Switch to the tab under the pointer. Returns whether the click landed on one.
fn focus_clicked_tab(app: &mut App, column: u16, row: u16) -> bool {
    let area = app.workspace_areas.tabs;
    let Some(id) = app
        .mux
        .as_ref()
        .and_then(|mux| crate::ui::pane::tab_at(mux, area, column, row))
    else {
        return false;
    };
    // Every press on a tab arms a drag; it only reorders once the pointer actually
    // moves to another tab, so a plain click still just switches.
    app.dragging_tab = Some(id);
    if app.mux.as_ref().and_then(|mux| mux.focused) == Some(id) {
        return true;
    }
    close_context_overlays(app);
    if let Some(mux) = app.mux.as_mut() {
        mux.focus(id);
    }
    app.view = View::Attached(id);
    // The same bookkeeping the prefix pane-switch does: acknowledges the newly focused
    // pane's unread marker, moves the scratchpad and terminal panels over to it, and
    // refreshes the outer terminal's progress. Switching by click must not be a
    // second-class way of doing it.
    sync_workspace_panels(app);
    true
}

/// Carry the dragged tab to the slot under the pointer.
///
/// Reordering as the pointer crosses each tab rather than only on release means the
/// strip shows the outcome while the button is still down.
///
/// When there are more sessions than fit, the pointer runs out of strip long before
/// the tab runs out of places to go: the strip is windowed around the focused tab,
/// which is the one being dragged, so it is pinned to the edge and the column under
/// the pointer stops changing. Holding against that edge therefore means "one more
/// step that way" rather than "stay put" — each step slides the window, bringing the
/// next hidden tab into reach.
fn drag_tab_to(app: &mut App, column: u16, row: u16) {
    let Some(id) = app.dragging_tab else {
        return;
    };
    let area = app.workspace_areas.tabs;
    let Some(mux) = app.mux.as_mut() else {
        return;
    };
    // Off the strip vertically means the gesture has wandered into a pane; hold the
    // tab where it is rather than snapping it to whatever column is underneath.
    if area.height == 0 || row < area.y || row >= area.bottom() {
        return;
    }
    let Some(from) = mux.window_index_of(id) else {
        return;
    };
    let last = mux.windows.len().saturating_sub(1);
    let under = crate::ui::pane::tab_index_at(mux, area, column, row);
    let (first_shown, last_shown) =
        crate::ui::pane::visible_tab_bounds(mux, area).unwrap_or((0, 0));

    let target = match under {
        // Pinned against an edge of the window with more order beyond it.
        Some(index) if index == from && index == last_shown => (from + 1).min(last),
        Some(index) if index == from && index == first_shown => from.saturating_sub(1),
        Some(index) => index,
        // Past the last tab's right edge, in the strip's empty tail.
        None if column >= area.x => last,
        None => 0,
    };
    mux.move_pane_to(id, target);
}

/// Move focus to whatever panel was clicked. Returns true when that meant switching
/// to another split's session, so the caller can keep the click from reaching it.
fn focus_clicked_workspace(app: &mut App, column: u16, row: u16) -> bool {
    let areas = &app.workspace_areas;
    let chat = areas.chat_at(column, row);
    // A placeholder dock has nothing to type into, so a click on one is ignored rather
    // than leaving the keyboard on an empty box.
    if areas
        .scratchpad
        .is_some_and(|area| contains(area, column, row))
    {
        if app.attached_scratchpad_visible() {
            app.workspace_focus = WorkspaceFocus::Scratchpad;
            app.terminal.unfocus();
        }
    } else if areas
        .terminal
        .is_some_and(|area| contains(area, column, row))
    {
        if app.attached_terminal_visible() {
            app.workspace_focus = WorkspaceFocus::Terminal;
            app.terminal.focus();
        }
    } else if let Some(id) = chat {
        app.workspace_focus = WorkspaceFocus::Chat;
        app.terminal.unfocus();
        if app.mux.as_ref().and_then(|mux| mux.focused) != Some(id) {
            close_context_overlays(app);
            if let Some(mux) = app.mux.as_mut() {
                mux.focus(id);
            }
            sync_workspace_panels(app);
            sync_view(app);
            return true;
        }
    }
    false
}

fn contains(area: Rect, column: u16, row: u16) -> bool {
    column >= area.x && column < area.right() && row >= area.y && row < area.bottom()
}

/// Route a key from the session list through the prefix state machine.
///
/// Returns true when the key was consumed, so panes remain reachable from the list
/// instead of only from inside an attached pane.
pub fn handle_list_prefix(app: &mut App, key: KeyEvent) -> bool {
    let Some((state, prefix)) = app.mux.as_ref().map(|mux| (mux.prefix_state, mux.prefix)) else {
        return false;
    };

    if state == PrefixState::Help {
        if let Some(mux) = app.mux.as_mut() {
            mux.prefix_state = PrefixState::Idle;
        }
        if matches!(resolve_help_command(&key), Some(HelpCommand::Scratchpad)) {
            app.workspace_help = Some(WorkspaceHelp::Scratchpad);
        }
        return true;
    }

    if state == PrefixState::Github {
        if let Some(mux) = app.mux.as_mut() {
            mux.prefix_state = PrefixState::Idle;
        }
        match resolve_github_command(&key) {
            Some(GithubCommand::Inspect) => {
                app.status_message =
                    Some("Attach to a session before inspecting a GitHub item".to_string());
            }
            // Unlike inspecting, this is about every session, so the list is a natural
            // place to ask from.
            Some(GithubCommand::Messages) => app.open_thread_inbox(),
            _ => {}
        }
        return true;
    }

    // Splits only exist on the attached screen, so a layout command goes there first,
    // the same way `prefix c` and `prefix e` do.
    if state == PrefixState::Layout {
        if let Some(mux) = app.mux.as_mut() {
            mux.prefix_state = PrefixState::Idle;
        }
        if let Some(command) = resolve_layout_command(&key) {
            if app.mux.as_ref().is_none_or(|mux| mux.panes.is_empty()) {
                app.status_message = Some("No sessions are running".to_string());
            } else {
                attach_focused(app);
                run_split_command(app, command);
            }
        }
        return true;
    }

    if state == PrefixState::Idle {
        if prefix.matches(&key) {
            if let Some(mux) = app.mux.as_mut() {
                mux.prefix_state = PrefixState::Root;
            }
            return true;
        }
        return false;
    }

    if let Some(mux) = app.mux.as_mut() {
        mux.prefix_state = PrefixState::Idle;
    }
    let command = resolve_prefix_command_with_tmux_keys(
        &key,
        &prefix,
        app.tmux_support
            .is_available()
            .then_some(&app.config.tmux_keys),
    );
    if matches!(command, Some(PrefixCommand::Help)) {
        if let Some(mux) = app.mux.as_mut() {
            mux.prefix_state = PrefixState::Help;
        }
        return true;
    }
    if matches!(command, Some(PrefixCommand::Github)) {
        if let Some(mux) = app.mux.as_mut() {
            mux.prefix_state = PrefixState::Github;
        }
        return true;
    }
    // Quitting is meaningful with or without panes, so it is resolved before the
    // "nothing is running" guard below.
    if matches!(command, Some(PrefixCommand::Quit)) {
        request_quit(app);
        return true;
    }
    if matches!(command, Some(PrefixCommand::Update)) {
        app.request_update();
        return true;
    }
    if matches!(command, Some(PrefixCommand::CommandPalette)) {
        app.open_command_palette();
        return true;
    }
    if app.mux.as_ref().is_none_or(|mux| mux.panes.is_empty()) {
        app.status_message = Some("No sessions are running".to_string());
        return true;
    }

    match command {
        // There is no child to re-attach to and nothing to detach from, so the only
        // sensible reading of these from the list is "show me what's running".
        Some(PrefixCommand::Detach) => app.open_pane_list(),
        Some(PrefixCommand::CommandPalette) => {
            unreachable!("handled before pane availability")
        }
        Some(PrefixCommand::PaneList) => app.open_pane_list(),
        Some(PrefixCommand::Chat) => {
            attach_focused(app);
            focus_chat(app);
        }
        Some(PrefixCommand::Scratchpad) => {
            attach_focused(app);
            toggle_attached_scratchpad(app);
        }
        Some(PrefixCommand::Terminal) => {
            attach_focused(app);
            toggle_attached_terminal(app);
        }
        Some(PrefixCommand::Snippets) => {
            attach_focused(app);
            app.open_snippets();
        }
        // The strip only exists once attached, so reordering from the list means
        // going there first — the same thing `prefix c` and `prefix e` do.
        Some(PrefixCommand::MoveTab) => {
            attach_focused(app);
            enter_transient_mode(app, TransientMode::MoveTab);
        }
        Some(PrefixCommand::Update) => unreachable!("handled before pane availability"),
        Some(PrefixCommand::Help) => unreachable!("handled before pane availability"),
        Some(PrefixCommand::Github) => unreachable!("handled before pane availability"),
        Some(PrefixCommand::Quit) => unreachable!("handled before pane availability"),
        Some(PrefixCommand::NextPane) => {
            if let Some(mux) = app.mux.as_mut() {
                mux.cycle(true);
            }
            attach_focused(app);
        }
        Some(PrefixCommand::PreviousPane) => {
            if let Some(mux) = app.mux.as_mut() {
                mux.cycle(false);
            }
            attach_focused(app);
        }
        Some(PrefixCommand::SelectIndex(index)) => {
            // Panes are labelled from 1 in the UI.
            if let Some(mux) = app.mux.as_mut() {
                mux.select_index(index.saturating_sub(1));
            }
            attach_focused(app);
        }
        Some(PrefixCommand::KillPane) => kill_focused(app),
        Some(PrefixCommand::EndPersistentSession) => end_focused_tmux(app),
        // Splits only exist on the attached screen, so these go there first, the same
        // way `prefix c` and `prefix e` do.
        Some(
            command @ (PrefixCommand::Split(_)
            | PrefixCommand::FocusSplit(_)
            | PrefixCommand::ZoomSplit
            | PrefixCommand::UnsplitFocused
            | PrefixCommand::ResizeSplit
            | PrefixCommand::EqualizeSplits),
        ) => {
            attach_focused(app);
            run_split_command(app, command);
        }
        Some(PrefixCommand::Layout) => {
            if let Some(mux) = app.mux.as_mut() {
                mux.prefix_state = PrefixState::Layout;
            }
        }
        Some(PrefixCommand::Cancel) | None => {}
    }
    true
}

/// Put the dragged border under the pointer.
fn drag_divider_to(app: &mut App, divider: crate::app::Divider, column: u16, row: u16) {
    use crate::app::Divider;
    match divider {
        Divider::Split { path, index } => drag_split_divider(app, &path, index, column, row),
        Divider::ScratchpadDock => {
            let areas = &app.workspace_areas;
            let (Some(dock), Some(first)) = (areas.scratchpad, areas.chats.first()) else {
                return;
            };
            let left = areas
                .chats
                .iter()
                .map(|slot| slot.area.x)
                .min()
                .unwrap_or(first.area.x);
            let total = dock.right().saturating_sub(left);
            if total == 0 {
                return;
            }
            let width = dock.right().saturating_sub(column);
            let percent = (u32::from(width) * 100 / u32::from(total)) as u16;
            // One percent short of the exact limit, because the layout rounds and a
            // cell too many would hide the split after all.
            let room = total.saturating_sub(split_needs(app).0);
            let cap = (u32::from(room) * 100 / u32::from(total)) as u16;
            let range = crate::ui::SCRATCHPAD_PERCENT_RANGE;
            app.dock_sizes.scratchpad_percent = percent
                .min(cap.saturating_sub(1))
                .clamp(*range.start(), *range.end());
        }
        Divider::TerminalDock => {
            let areas = &app.workspace_areas;
            let (Some(dock), Some(top)) = (
                areas.terminal,
                areas.chats.iter().map(|slot| slot.area.y).min(),
            ) else {
                return;
            };
            let room = dock
                .bottom()
                .saturating_sub(top)
                .saturating_sub(split_needs(app).1);
            // The layout also caps this so a lone chat keeps some rows.
            let rows = dock
                .bottom()
                .saturating_sub(row)
                .min(room)
                .max(crate::ui::MIN_TERMINAL_ROWS);
            app.dock_sizes.terminal_rows = Some(rows);
        }
    }
    app.refresh_workspace_areas();
}

/// Columns and rows the sessions on screen need to stay on screen, borders included.
///
/// A dock dragged past this would make every split vanish under the pointer, which
/// reads as the sessions closing; the drag stops here instead, the way a divider
/// between two splits stops at their minimum.
fn split_needs(app: &App) -> (u16, u16) {
    let Some(layout) = app
        .mux
        .as_ref()
        .and_then(MuxState::visible_layout)
        .filter(|_| app.workspace_areas.chats.len() > 1)
    else {
        return (0, 0);
    };
    (
        crate::ui::minimum_extent(layout, crate::mux::SplitDirection::Columns, false),
        crate::ui::minimum_extent(layout, crate::mux::SplitDirection::Rows, false),
    )
}

/// Give the split at `path` these member lengths. Weights in cells reproduce the frame
/// exactly, so nothing outside the two members being resized shifts by a rounding cell.
fn set_split_lengths(app: &mut App, path: &[usize], lengths: Vec<u16>) {
    if let Some(crate::mux::LayoutNode::Split { weights, .. }) = app
        .mux
        .as_mut()
        .and_then(MuxState::current_layout_mut)
        .and_then(|layout| layout.node_at_mut(path))
    {
        if weights.len() == lengths.len() {
            *weights = lengths;
        }
    }
    app.refresh_workspace_areas();
}

fn drag_split_divider(app: &mut App, path: &[usize], index: usize, column: u16, row: u16) {
    let Some(split) = app
        .workspace_areas
        .splits
        .iter()
        .find(|split| split.path == path)
        .cloned()
    else {
        return;
    };
    let Some(before) = index.checked_sub(1) else {
        return;
    };
    if index >= split.lengths.len() {
        return;
    }
    let at = match split.direction {
        crate::mux::SplitDirection::Columns => column,
        crate::mux::SplitDirection::Rows => row,
    };
    let pair = split.lengths[before] + split.lengths[index];
    let (low, high) = (
        split.minimums[before],
        pair.saturating_sub(split.minimums[index]),
    );
    if low > high {
        return;
    }
    let first = at.saturating_sub(split.starts[before]).clamp(low, high);
    let mut lengths = split.lengths;
    lengths[before] = first;
    lengths[index] = pair - first;
    set_split_lengths(app, &split.path, lengths);
}

/// One keyboard step of resizing along `axis`: the focused session's member of the
/// nearest split running that way takes cells from, or gives them to, the member
/// after it — or before it, for the last one. Left and right act on columns, up and
/// down on rows, so in a column holding a stack every arrow does something.
fn resize_focused_split(app: &mut App, axis: crate::mux::SplitDirection, grow: bool) -> bool {
    const STEP: u16 = 2;
    let Some(path) = app.mux.as_ref().and_then(|mux| {
        let focused = mux.focused?;
        mux.current_window()?.layout.path_to(focused)
    }) else {
        return false;
    };
    // The deepest split on the way to the focused session that runs along `axis`.
    let Some(split) = app
        .workspace_areas
        .splits
        .iter()
        .filter(|split| split.direction == axis && path.starts_with(&split.path))
        .max_by_key(|split| split.path.len())
        .cloned()
    else {
        return false;
    };
    let index = path[split.path.len()];
    let neighbour = if index + 1 < split.lengths.len() {
        index + 1
    } else {
        index - 1
    };
    let (from, to) = if grow {
        (neighbour, index)
    } else {
        (index, neighbour)
    };
    let moved = split.lengths[from]
        .saturating_sub(split.minimums[from])
        .min(STEP);
    if moved == 0 {
        return false;
    }
    let mut lengths = split.lengths;
    lengths[from] -= moved;
    lengths[to] += moved;
    set_split_lengths(app, &split.path, lengths);
    true
}

/// The session on screen next to the focused one in `step`'s direction: of those
/// lying wholly that way and overlapping it across, the nearest, then the one sharing
/// most of its edge. Found from what is drawn rather than the layout tree, because
/// "left" means left on the screen however the splits happen to be nested.
fn split_neighbour(app: &App, step: crate::mux::SplitStep) -> Option<crate::mux::PaneId> {
    use crate::mux::SplitStep;
    let focused = app.mux.as_ref()?.focused?;
    let chats = &app.workspace_areas.chats;
    let current = chats.iter().find(|slot| slot.pane == focused)?.area;
    let overlap = |a: (u16, u16), b: (u16, u16)| a.1.min(b.1).saturating_sub(a.0.max(b.0));
    chats
        .iter()
        .filter(|slot| slot.pane != focused)
        .filter_map(|slot| {
            let area = slot.area;
            let horizontal = (area.x, area.right());
            let vertical = (area.y, area.bottom());
            let (distance, shared) = match step {
                SplitStep::Left if area.right() <= current.x => (
                    current.x - area.right(),
                    overlap(vertical, (current.y, current.bottom())),
                ),
                SplitStep::Right if area.x >= current.right() => (
                    area.x - current.right(),
                    overlap(vertical, (current.y, current.bottom())),
                ),
                SplitStep::Up if area.bottom() <= current.y => (
                    current.y - area.bottom(),
                    overlap(horizontal, (current.x, current.right())),
                ),
                SplitStep::Down if area.y >= current.bottom() => (
                    area.y - current.bottom(),
                    overlap(horizontal, (current.x, current.right())),
                ),
                _ => return None,
            };
            (shared > 0).then_some((distance, std::cmp::Reverse(shared), slot.pane))
        })
        .min()
        .map(|(_, _, pane)| pane)
}

/// The split commands, which behave the same from the list and from a session.
fn run_split_command(app: &mut App, command: PrefixCommand) {
    match command {
        PrefixCommand::Split(direction) => app.open_split_picker(direction),
        PrefixCommand::ResizeSplit => {
            enter_transient_mode(app, TransientMode::ResizeSplit);
            return;
        }
        PrefixCommand::FocusSplit(step) => {
            if let Some(target) = split_neighbour(app, step) {
                if let Some(mux) = app.mux.as_mut() {
                    mux.focus(target);
                }
                app.workspace_focus = WorkspaceFocus::Chat;
                sync_workspace_panels(app);
            }
        }
        PrefixCommand::EqualizeSplits => {
            if !app.mux.as_mut().is_some_and(MuxState::equalize_split) {
                app.status_message =
                    Some("Nothing to equalize: only one session is on screen".to_string());
            }
        }
        PrefixCommand::ZoomSplit => {
            if !app.mux.as_mut().is_some_and(MuxState::toggle_split_zoom) {
                app.status_message =
                    Some("Nothing to zoom: only one session is on screen".to_string());
            }
        }
        PrefixCommand::UnsplitFocused => {
            if app.mux.as_mut().is_some_and(MuxState::unsplit_focused) {
                sync_workspace_panels(app);
            } else {
                app.status_message = Some("This session is not in a split".to_string());
            }
        }
        _ => {}
    }
    sync_view(app);
    app.refresh_workspace_areas();
}

/// Bring the focused pane back on screen after the list changed it.
fn attach_focused(app: &mut App) {
    if let Some(id) = app.mux.as_ref().and_then(|mux| mux.focused) {
        app.view = View::Attached(id);
        sync_workspace_panels(app);
    }
}

fn kill_focused(app: &mut App) {
    let Some((id, title, persistent)) = app.mux.as_ref().and_then(|mux| {
        let id = mux.focused?;
        let pane = mux.pane(id)?;
        Some((id, pane.title.clone(), app.pane_is_tmux_backed(pane)))
    }) else {
        return;
    };
    if !app.forget_workspace_panels(id) {
        return;
    }
    if let Some(mux) = app.mux.as_mut() {
        mux.remove(id);
    }
    app.status_message = Some(if persistent {
        format!("Detached '{title}'; tmux session is still running")
    } else {
        format!("Ended '{title}'")
    });
    sync_workspace_panels(app);
    sync_view(app);
}

fn end_focused_tmux(app: &mut App) {
    let Some(session_id) = app
        .mux
        .as_ref()
        .and_then(|mux| mux.focused_pane())
        .map(|pane| pane.session_id.clone())
    else {
        return;
    };
    request_end_tmux_session(app, &session_id);
}

/// Everything needed to resurrect the focused pane as a fresh resume of its session.
struct RestartTarget {
    pane_id: crate::mux::PaneId,
    session_id: String,
    cwd: String,
    title: String,
}

/// Restart is only offered on a pane whose session has exited: while the child is
/// alive every key belongs to Copilot, and `r` is ordinary typing.
fn restart_target(app: &App) -> Option<RestartTarget> {
    let mux = app.mux.as_ref()?;
    let pane = mux.focused_pane()?;
    if pane.is_running() {
        return None;
    }
    Some(RestartTarget {
        pane_id: pane.id,
        session_id: pane.session_id.clone(),
        cwd: pane.cwd.to_string_lossy().into_owned(),
        title: pane.title.clone(),
    })
}

/// Resume the focused dead pane's session in place, instead of making the user close
/// the pane, find the session in the list, and open it again.
fn restart_focused(app: &mut App) {
    let Some(target) = restart_target(app) else {
        return;
    };
    // Same contract as closing: a scratchpad that cannot be saved keeps its pane.
    if !app.forget_workspace_panels(target.pane_id) {
        return;
    }
    // In the dead pane's place — its slot in a split, or its tab — so nothing
    // reshuffles under the user.
    match app.attach_session_replacing(
        &target.session_id,
        &target.cwd,
        target.title,
        Some(target.pane_id),
    ) {
        Ok(()) => {
            sync_workspace_panels(app);
        }
        Err(error) => {
            app.status_message = Some(format!("Cannot restart session: {error}"));
        }
    }
    sync_view(app);
}

/// Keep the visible view consistent with the focused pane, falling back to the list.
pub fn sync_view(app: &mut App) {
    let focused = app.mux.as_ref().and_then(|mux| mux.focused);
    app.view = match (app.view, focused) {
        (View::Attached(_), Some(id)) => View::Attached(id),
        (View::Attached(_), None) => View::List,
        (View::List, _) => View::List,
    };
}

/// A turn finishing in another session that shares the screen in a split, in a window
/// the user is looking at.
///
/// Such a session is not *attended* — it does not have the keyboard, so it still
/// raises its attention marker, and its border shows it. But the user watched it
/// finish, and a notification saying so is noise. Only `Ready` is held back, which is
/// exactly what attending the focused session holds back: questions, plan approvals
/// and errors notify even from the focused session, because they block until someone
/// acts, and a session merely being watched has no better claim to silence.
fn watched_in_split(app: &App, id: crate::mux::PaneId, kind: NotificationKind) -> bool {
    kind == NotificationKind::Ready
        && app.terminal_focused
        && !matches!(app.view, View::Attached(attached) if attached == id)
        && app.pane_on_screen(id)
}

/// Apply a PTY event. Returns true when the UI needs a repaint.
pub fn handle_mux_event(app: &mut App, event: MuxEvent) -> bool {
    match event {
        MuxEvent::Output(id, signals) => {
            let attended = app.terminal_focused
                && matches!(app.view, View::Attached(focused) if focused == id);
            let (
                outcome,
                notifications,
                attention_changed,
                title_changed,
                draft_changed,
                cycle_started,
                title,
                session_id,
            ) = app
                .mux
                .as_mut()
                .and_then(|mux| mux.pane_mut(id))
                .map(|pane| {
                    let before = pane.needs_attention();
                    let title_before = pane.title.clone();
                    let cycle_started = signals_start_cycle(pane.is_working(), &signals.events);
                    let (outcome, notifications) = pane.apply_signals(signals, attended);
                    let draft_changed = pane.clear_draft_if_composer_empty();
                    (
                        outcome,
                        notifications,
                        before != pane.needs_attention(),
                        title_before != pane.title,
                        draft_changed,
                        cycle_started,
                        pane.title.clone(),
                        pane.session_id.clone(),
                    )
                })
                .unwrap_or_default();
            if cycle_started {
                app.begin_notification_cycle(&session_id);
            }
            let notification_changed = !notifications.is_empty();
            for notification in notifications {
                let kind = match notification {
                    PaneNotification::Ready => NotificationKind::Ready,
                    PaneNotification::Question => NotificationKind::Question,
                    PaneNotification::PlanApproval => NotificationKind::PlanApproval,
                    PaneNotification::Error => NotificationKind::Error,
                };
                if !watched_in_split(app, id, kind) {
                    app.enqueue_notification(kind, title.clone(), Some(&session_id));
                }
            }
            if outcome.bell {
                // Numbered like its tab, since several sessions often share a title.
                if let Some(title) = app.mux.as_ref().and_then(|mux| {
                    let pane = mux.pane(id)?;
                    Some(format!("{} {}", mux.tab_number(id)?, pane.title))
                }) {
                    app.status_message = Some(format!("🔔 {title}"));
                }
            }
            app.pane_on_screen(id)
                || outcome.bell
                || attention_changed
                || title_changed
                || draft_changed
                || notification_changed
        }
        MuxEvent::WorktreeReady(id, outcome) => {
            crate::input::finish_worktree_launch(app, id, *outcome);
            true
        }
        MuxEvent::Exited(id, code) => {
            let was_focused = app.mux.as_ref().is_some_and(|mux| mux.focused == Some(id));
            // A tab the user just closed reports its process exiting a moment later.
            // There is nothing to say about it, and saying "Session '' exited" would
            // replace the message that closing it left, such as that a tmux session
            // is still running.
            let Some(title) = app
                .mux
                .as_mut()
                .and_then(|mux| mux.pane_mut(id))
                .map(|pane| {
                    pane.mark_exited(code);
                    pane.title.clone()
                })
            else {
                return false;
            };
            app.status_message = Some(match code {
                Some(0) | None => format!("Session '{title}' finished"),
                Some(code) => format!("Session '{title}' exited with code {code}"),
            });
            if was_focused {
                app.host_sequences
                    .push(crate::host_terminal::CLEAR_PROGRESS.to_vec());
            }
            true
        }
        MuxEvent::SessionLifecycle(id, event) => {
            let focused = app.mux.as_ref().and_then(|mux| mux.focused) == Some(id);
            let (notification, attention_changed, action_or_progress_changed, title, session_id) =
                app.mux
                    .as_mut()
                    .and_then(|mux| mux.pane_mut(id))
                    .and_then(|pane| {
                        if !pane.is_running() {
                            return None;
                        }
                        let attention_before = pane.needs_attention();
                        let action_before = pane.requires_user_action();
                        // A quiet report can retire a spinner without touching either of
                        // those, and a stale spinner on an unfocused tab is exactly what this
                        // is here to clear — so the marker is compared too.
                        let progress_before = pane.effective_progress_state();
                        let notification = pane.apply_lifecycle(event);
                        Some((
                            notification,
                            attention_before != pane.needs_attention(),
                            action_before != pane.requires_user_action()
                                || progress_before != pane.effective_progress_state(),
                            pane.title.clone(),
                            pane.session_id.clone(),
                        ))
                    })
                    .unwrap_or_default();
            if let Some(notification) = notification {
                let kind = match notification {
                    PaneNotification::Question => NotificationKind::Question,
                    PaneNotification::PlanApproval => NotificationKind::PlanApproval,
                    PaneNotification::Ready => NotificationKind::Ready,
                    PaneNotification::Error => NotificationKind::Error,
                };
                if !watched_in_split(app, id, kind) {
                    app.enqueue_notification(kind, title, Some(&session_id));
                }
            }
            if focused && action_or_progress_changed {
                sync_outer_progress(app);
            }
            app.pane_on_screen(id)
                || attention_changed
                || action_or_progress_changed
                || notification.is_some()
        }
        MuxEvent::HookLifecycle(id, event) => {
            let attended = app.terminal_focused
                && matches!(app.view, View::Attached(focused) if focused == id);
            let focused = app.mux.as_ref().and_then(|mux| mux.focused) == Some(id);
            let (outcome, attention_changed, action_changed, title, session_id) = app
                .mux
                .as_mut()
                .and_then(|mux| mux.pane_mut(id))
                .and_then(|pane| {
                    if !pane.is_running() {
                        return None;
                    }
                    let attention_before = pane.needs_attention();
                    let action_before = pane.requires_user_action();
                    let outcome = pane.apply_hook(event, attended);
                    Some((
                        outcome,
                        attention_before != pane.needs_attention(),
                        action_before != pane.requires_user_action(),
                        pane.title.clone(),
                        pane.session_id.clone(),
                    ))
                })
                .unwrap_or_default();
            if outcome.cycle_started {
                app.begin_notification_cycle(&session_id);
            }
            if let Some(timestamp) = outcome.ready_confirmation {
                if let Some(events) = app.mux.as_ref().map(|mux| mux.events.clone()) {
                    std::thread::spawn(move || {
                        std::thread::sleep(HOOK_READY_GRACE);
                        let _ = events.send(MuxEvent::HookReadyConfirmed(id, timestamp));
                    });
                }
            }
            if let Some(notification) = outcome.notification {
                let kind = match notification {
                    PaneNotification::Question => NotificationKind::Question,
                    PaneNotification::PlanApproval => NotificationKind::PlanApproval,
                    PaneNotification::Ready => NotificationKind::Ready,
                    PaneNotification::Error => NotificationKind::Error,
                };
                if !watched_in_split(app, id, kind) {
                    app.enqueue_notification(kind, title, Some(&session_id));
                }
            }
            if focused {
                sync_outer_progress(app);
            }
            focused
                || attention_changed
                || action_changed
                || outcome.notification.is_some()
                || outcome.cycle_started
        }
        MuxEvent::HookReadyConfirmed(id, timestamp) => {
            let attended = app.terminal_focused
                && matches!(app.view, View::Attached(focused) if focused == id);
            let focused = app.mux.as_ref().and_then(|mux| mux.focused) == Some(id);
            let (notification, title, session_id) = app
                .mux
                .as_mut()
                .and_then(|mux| mux.pane_mut(id))
                .and_then(|pane| {
                    if !pane.is_running() {
                        return None;
                    }
                    Some((
                        pane.confirm_hook_ready(timestamp, attended),
                        pane.title.clone(),
                        pane.session_id.clone(),
                    ))
                })
                .unwrap_or_default();
            if let Some(notification) = notification {
                let kind = match notification {
                    PaneNotification::Ready => NotificationKind::Ready,
                    PaneNotification::Question => NotificationKind::Question,
                    PaneNotification::PlanApproval => NotificationKind::PlanApproval,
                    PaneNotification::Error => NotificationKind::Error,
                };
                if !watched_in_split(app, id, kind) {
                    app.enqueue_notification(kind, title, Some(&session_id));
                }
            }
            if focused {
                sync_outer_progress(app);
            }
            // The turn this pane just finished may be what a queued wake was waiting
            // for, so this is the earliest safe moment to hand it over.
            let woken = app.flush_thread_wakes();
            focused || notification.is_some() || woken
        }
        MuxEvent::HostSequence(id, sequence) => {
            let progress = crate::host_terminal::progress_state_from_sequence(&sequence);
            let mut cycle_started = None;
            if let Some(progress) = progress {
                if let Some(pane) = app.mux.as_mut().and_then(|mux| mux.pane_mut(id)) {
                    if pane.is_running() && pane.record_progress_state(progress) {
                        cycle_started = Some(pane.session_id.clone());
                    }
                }
            }
            if let Some(session_id) = cycle_started {
                app.begin_notification_cycle(&session_id);
            }
            let focused = app.mux.as_ref().and_then(|mux| mux.focused);
            let forward_progress = progress.is_some_and(|state| {
                app.mux
                    .as_ref()
                    .and_then(|mux| mux.pane(id))
                    .is_some_and(|pane| pane.should_forward_raw_progress(state))
            });
            if progress.is_none() || (focused == Some(id) && forward_progress) {
                app.host_sequences.push(sequence);
                true
            } else {
                // The sequence is not going to the outer terminal, but the pane's own
                // tab still shows this state, so the frame is now stale.
                progress.is_some() && app.tab_bar_visible()
            }
        }
        MuxEvent::ConfigChanged => app.request_config_reload(),
        MuxEvent::ThreadNoticesChanged => app.apply_thread_notices(),
        MuxEvent::ThreadStalled(notice) => {
            // Reported only. Nothing was changed, so there is nothing to undo.
            app.status_message = Some(notice);
            true
        }
        MuxEvent::ThreadWatchFailed(reason) => {
            // Said once rather than every minute: the watcher already backs off, and a
            // status line that keeps repeating the same failure is noise.
            app.report_thread_watch_failure(reason)
        }
        MuxEvent::Term(_) => true,
    }
}

fn signals_start_cycle(
    initially_working: bool,
    events: &[crate::mux::callbacks::PaneSignalEvent],
) -> bool {
    let mut working = initially_working;
    let mut started = false;
    for event in events {
        if let crate::mux::callbacks::PaneSignalEvent::Progress(progress) = event {
            if progress.is_working() {
                started |= !working;
                working = true;
            } else {
                working = false;
            }
        }
    }
    started
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::UserConfig;
    use crate::mux::{Pane, PaneSpec};
    use crossterm::event::{KeyCode, KeyModifiers};

    fn mux_app() -> App {
        let config = UserConfig {
            mux: true,
            ..UserConfig::default()
        };
        let mut app = App::new(Vec::new(), config);
        app.disable_workspace_state_persistence();
        app.disable_config_persistence();
        app
    }

    /// Sessions `1..=count`, with 1 and 2 side by side and 2 focused, and one frame's
    /// layout recorded the way the main loop records it.
    fn split_app(count: u64) -> App {
        let mut app = mux_app();
        for id in 1..=count {
            push_test_pane(&mut app, id, &format!("split-{id}"));
        }
        app.mux.as_mut().unwrap().focused = Some(1);
        app.view = View::Attached(1);
        assert!(app
            .mux
            .as_mut()
            .unwrap()
            .split_with(crate::mux::SplitDirection::Columns, 2));
        sync_view(&mut app);
        record_frame(&mut app);
        app
    }

    fn record_frame(app: &mut App) {
        let layout = app.attached_layout(Rect::new(0, 0, 160, 40));
        app.workspace_areas = crate::app::WorkspaceAreas::from_layout(&layout);
    }

    fn mouse(kind: MouseEventKind, column: u16, row: u16) -> Event {
        Event::Mouse(crossterm::event::MouseEvent {
            kind,
            column,
            row,
            modifiers: KeyModifiers::NONE,
        })
    }

    fn inside(app: &App, id: u64) -> (u16, u16) {
        let slot = app
            .workspace_areas
            .chats
            .iter()
            .find(|slot| slot.pane == id)
            .expect("session on screen");
        let area = slot.pane_area();
        (area.x + 2, area.y + 2)
    }

    #[test]
    fn output_from_a_split_that_is_not_focused_repaints_or_that_split_would_freeze() {
        let mut app = split_app(3);
        assert!(app.pane_on_screen(1));
        assert!(handle_mux_event(
            &mut app,
            MuxEvent::Output(1, Default::default())
        ));
        assert!(
            !handle_mux_event(&mut app, MuxEvent::Output(3, Default::default())),
            "a session off screen still costs no frame"
        );
        let _ = app.mux.as_mut().unwrap().shutdown();
    }

    #[test]
    fn a_turn_finishing_in_a_watched_split_marks_it_without_notifying() {
        let mut app = split_app(3);
        app.config.notifications.enabled = true;
        app.config.notifications.topic = "private_topic".to_string();
        app.terminal_focused = true;
        let finished = crate::mux::callbacks::PaneSignals {
            events: vec![crate::mux::callbacks::PaneSignalEvent::Bell],
            ..Default::default()
        };

        handle_mux_event(&mut app, MuxEvent::Output(1, finished.clone()));
        assert!(app.notification_requests.is_empty());
        assert!(
            app.mux.as_ref().unwrap().pane(1).unwrap().needs_attention(),
            "the marker is how a watched split says it is done"
        );

        handle_mux_event(&mut app, MuxEvent::Output(3, finished));
        assert_eq!(
            app.notification_requests.len(),
            1,
            "a session off screen notifies exactly as before"
        );
        let _ = app.mux.as_mut().unwrap().shutdown();
    }

    #[test]
    fn a_question_in_a_watched_split_still_notifies_like_one_in_the_focused_session() {
        use crate::events::lifecycle::{InputKind, LifecycleEvent};
        let mut app = split_app(2);
        app.config.notifications.enabled = true;
        app.config.notifications.topic = "private_topic".to_string();
        app.terminal_focused = true;

        handle_mux_event(
            &mut app,
            MuxEvent::SessionLifecycle(
                1,
                LifecycleEvent::InputRequested {
                    tool_call_id: "question-1".into(),
                    kind: InputKind::Question,
                },
            ),
        );
        assert_eq!(app.notification_requests.len(), 1);
        assert_eq!(
            app.notification_requests[0].kind,
            NotificationKind::Question
        );
        let _ = app.mux.as_mut().unwrap().shutdown();
    }

    #[test]
    fn clicking_another_split_focuses_it_and_keeps_the_rest_of_that_click_from_it() {
        let mut app = split_app(2);
        let (column, row) = inside(&app, 1);

        handle_attached_event(
            &mut app,
            mouse(MouseEventKind::Down(MouseButton::Left), column, row),
        );
        assert_eq!(app.mux.as_ref().unwrap().focused, Some(1));
        assert_eq!(app.view, View::Attached(1));
        assert!(app.swallow_mouse_until_up);

        handle_attached_event(
            &mut app,
            mouse(MouseEventKind::Up(MouseButton::Left), column, row),
        );
        assert!(!app.swallow_mouse_until_up);
        let _ = app.mux.as_mut().unwrap().shutdown();
    }

    #[test]
    fn the_wheel_scrolls_the_split_under_it_without_taking_focus() {
        let mut app = split_app(2);
        let (column, row) = inside(&app, 1);
        handle_attached_event(&mut app, mouse(MouseEventKind::ScrollUp, column, row));
        assert_eq!(app.mux.as_ref().unwrap().focused, Some(2));
        assert!(!app.swallow_mouse_until_up);
        let _ = app.mux.as_mut().unwrap().shutdown();
    }

    #[test]
    fn the_split_picker_offers_only_sessions_off_screen_and_never_leaves_the_split() {
        let mut app = split_app(4);
        app.open_split_picker(crate::mux::SplitDirection::Columns);
        assert_eq!(app.mode, Mode::PaneList);
        assert_eq!(
            app.view,
            View::Attached(2),
            "the list view would resize every session on screen and back"
        );
        assert_eq!(app.pane_list_ids(), vec![3, 4]);

        handle_attached_event(
            &mut app,
            Event::Key(KeyEvent::new(KeyCode::Down, KeyModifiers::NONE)),
        );
        handle_attached_event(
            &mut app,
            Event::Key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE)),
        );
        assert_eq!(app.mode, Mode::Normal);
        assert_eq!(app.split_picker, None);
        let mux = app.mux.as_ref().unwrap();
        assert_eq!(mux.current_window().unwrap().layout.panes(), vec![1, 2, 4]);
        assert_eq!(mux.focused, Some(4));
        assert_eq!(app.view, View::Attached(4));
        let _ = app.mux.as_mut().unwrap().shutdown();
    }

    /// Found once messages could be seen while attached: closing a tab printed what it
    /// meant to, and the exit of its process a moment later replaced it with
    /// "Session '' exited with code 1".
    #[test]
    fn the_exit_of_a_session_whose_tab_was_just_closed_says_nothing() {
        let mut app = split_app(2);
        handle_attached_event(
            &mut app,
            Event::Key(KeyEvent::new(KeyCode::Char('b'), KeyModifiers::CONTROL)),
        );
        handle_attached_event(
            &mut app,
            Event::Key(KeyEvent::new(KeyCode::Char('x'), KeyModifiers::NONE)),
        );
        let closed = app.status_message.clone();
        assert!(closed
            .as_deref()
            .is_some_and(|message| message.starts_with("Ended")));

        assert!(!handle_mux_event(&mut app, MuxEvent::Exited(2, Some(1))));
        assert_eq!(app.status_message, closed);
        let _ = app.mux.as_mut().unwrap().shutdown();
    }

    #[test]
    fn splitting_the_other_way_with_every_session_on_screen_turns_the_split() {
        let mut app = split_app(2);
        app.open_split_picker(crate::mux::SplitDirection::Rows);
        assert_eq!(app.mode, Mode::Normal, "nothing to pick, so no picker");
        let layout = app
            .mux
            .as_ref()
            .unwrap()
            .current_window()
            .unwrap()
            .layout
            .clone();
        assert!(matches!(
            layout,
            crate::mux::LayoutNode::Split {
                direction: crate::mux::SplitDirection::Rows,
                ..
            }
        ));
        assert_eq!(layout.panes(), vec![1, 2]);
        assert_eq!(
            app.workspace_areas.chats[0].area.x, app.workspace_areas.chats[1].area.x,
            "laid out stacked straight away"
        );

        // Already that way: nothing to turn, so the picker opens, offering only a new
        // session.
        app.open_split_picker(crate::mux::SplitDirection::Rows);
        assert_eq!(app.mode, Mode::PaneList);
        assert!(app.pane_list_ids().is_empty());
        assert_eq!(app.pane_selected, 0, "on the new-session row");
        let _ = app.mux.as_mut().unwrap().shutdown();
    }

    #[test]
    fn the_split_prefix_keys_open_the_picker_and_move_focus_between_splits() {
        let mut app = split_app(3);
        let prefix = KeyEvent::new(KeyCode::Char('b'), KeyModifiers::CONTROL);
        handle_attached_event(&mut app, Event::Key(prefix));
        handle_attached_event(
            &mut app,
            Event::Key(KeyEvent::new(KeyCode::Left, KeyModifiers::NONE)),
        );
        assert_eq!(app.mux.as_ref().unwrap().focused, Some(1));
        assert_eq!(app.view, View::Attached(1));

        handle_attached_event(&mut app, Event::Key(prefix));
        handle_attached_event(
            &mut app,
            Event::Key(KeyEvent::new(KeyCode::Char('l'), KeyModifiers::NONE)),
        );
        assert_eq!(
            app.mux.as_ref().unwrap().prefix_state,
            PrefixState::Layout,
            "waiting for a layout key, which the status bar lists"
        );
        handle_attached_event(
            &mut app,
            Event::Key(KeyEvent::new(KeyCode::Char('s'), KeyModifiers::NONE)),
        );
        assert_eq!(app.mode, Mode::PaneList);
        assert_eq!(app.split_picker, Some(crate::mux::SplitDirection::Rows));
        assert_eq!(app.mux.as_ref().unwrap().prefix_state, PrefixState::Idle);
        let _ = app.mux.as_mut().unwrap().shutdown();
    }

    #[test]
    fn layout_equalize_undoes_resizing_and_esc_leaves_the_menu_without_acting() {
        let mut app = split_app(2);
        let before = chat_widths(&app);
        if let Some(crate::mux::LayoutNode::Split { weights, .. }) =
            app.mux.as_mut().unwrap().current_layout_mut()
        {
            *weights = vec![300, 100];
        }
        app.refresh_workspace_areas();
        assert_ne!(chat_widths(&app), before);

        let prefix = KeyEvent::new(KeyCode::Char('b'), KeyModifiers::CONTROL);
        for code in [KeyCode::Char('l'), KeyCode::Esc] {
            handle_attached_event(&mut app, Event::Key(prefix));
            handle_attached_event(
                &mut app,
                Event::Key(KeyEvent::new(code, KeyModifiers::NONE)),
            );
        }
        assert_eq!(app.mux.as_ref().unwrap().prefix_state, PrefixState::Idle);
        assert_ne!(chat_widths(&app), before);

        for code in [KeyCode::Char('l'), KeyCode::Char('=')] {
            if code == KeyCode::Char('l') {
                handle_attached_event(&mut app, Event::Key(prefix));
            }
            handle_attached_event(
                &mut app,
                Event::Key(KeyEvent::new(code, KeyModifiers::NONE)),
            );
        }
        assert_eq!(chat_widths(&app), before);
        let _ = app.mux.as_mut().unwrap().shutdown();
    }

    fn chat_widths(app: &App) -> Vec<u16> {
        app.workspace_areas
            .chats
            .iter()
            .map(|slot| slot.pane_area().width)
            .collect()
    }

    #[test]
    fn dragging_a_split_divider_moves_it_with_the_pointer_and_stops_at_a_usable_width() {
        let mut app = split_app(3);
        app.mux
            .as_mut()
            .unwrap()
            .split_with(crate::mux::SplitDirection::Columns, 3);
        record_frame(&mut app);
        let before = chat_widths(&app);
        let divider = app.workspace_areas.chats[1].area;
        let row = divider.y + 3;
        assert_eq!(
            app.workspace_areas.divider_at(divider.x, row),
            Some(crate::app::Divider::Split {
                path: Vec::new(),
                index: 1
            })
        );

        handle_attached_event(
            &mut app,
            mouse(MouseEventKind::Down(MouseButton::Left), divider.x, row),
        );
        assert_eq!(
            app.mux.as_ref().unwrap().focused,
            Some(3),
            "grabbing a border is not a click in either split"
        );
        handle_attached_event(
            &mut app,
            mouse(MouseEventKind::Drag(MouseButton::Left), divider.x + 5, row),
        );
        record_frame(&mut app);
        let after = chat_widths(&app);
        assert_eq!(after[0], before[0] + 5);
        assert_eq!(after[1], before[1] - 5);
        assert_eq!(after[2], before[2], "the split beyond the pair stays put");

        // Far past the next divider: the middle split keeps its minimum.
        handle_attached_event(
            &mut app,
            mouse(MouseEventKind::Drag(MouseButton::Left), 159, row),
        );
        record_frame(&mut app);
        assert_eq!(chat_widths(&app)[1], crate::ui::MIN_SPLIT_COLS);

        handle_attached_event(
            &mut app,
            mouse(MouseEventKind::Up(MouseButton::Left), 159, row),
        );
        assert_eq!(app.dragging_divider, None);
        let _ = app.mux.as_mut().unwrap().shutdown();
    }

    #[test]
    fn dragging_the_scratchpad_edge_resizes_the_dock_within_its_limits() {
        let mut app = split_app(2);
        app.scratchpad_open.insert(1);
        record_frame(&mut app);
        let dock = app.workspace_areas.scratchpad.expect("dock on screen");
        let row = dock.y + 2;

        handle_attached_event(
            &mut app,
            mouse(MouseEventKind::Down(MouseButton::Left), dock.x, row),
        );
        handle_attached_event(
            &mut app,
            mouse(MouseEventKind::Drag(MouseButton::Left), dock.x - 16, row),
        );
        assert_eq!(app.dock_sizes.scratchpad_percent, 45);

        // All the way to the left edge: the dock stops where both splits still fit,
        // short of its own maximum, rather than hiding them under the pointer.
        handle_attached_event(
            &mut app,
            mouse(MouseEventKind::Drag(MouseButton::Left), 0, row),
        );
        record_frame(&mut app);
        assert!(app.dock_sizes.scratchpad_percent < *crate::ui::SCRATCHPAD_PERCENT_RANGE.end());
        assert!(!app.workspace_areas.split_collapsed);
        assert_eq!(app.workspace_areas.chats.len(), 2);
        assert!(
            app.workspace_areas
                .chats
                .iter()
                .all(|slot| slot.pane_area().width >= crate::ui::MIN_SPLIT_COLS),
            "{:?}",
            chat_widths(&app)
        );
        handle_attached_event(
            &mut app,
            mouse(MouseEventKind::Up(MouseButton::Left), 0, row),
        );
        let _ = app.mux.as_mut().unwrap().shutdown();
    }

    #[test]
    fn the_resize_mode_trades_cells_with_a_neighbour_until_one_reaches_its_minimum() {
        let mut app = split_app(2);
        let before = chat_widths(&app);
        let prefix = KeyEvent::new(KeyCode::Char('b'), KeyModifiers::CONTROL);
        handle_attached_event(&mut app, Event::Key(prefix));
        for code in [KeyCode::Char('l'), KeyCode::Char('r')] {
            handle_attached_event(
                &mut app,
                Event::Key(KeyEvent::new(code, KeyModifiers::NONE)),
            );
        }
        assert_eq!(
            app.mux.as_ref().unwrap().prefix_state,
            PrefixState::Transient(TransientMode::ResizeSplit)
        );

        // The focused split is the last one, so it trades with the one before it.
        handle_attached_event(
            &mut app,
            Event::Key(KeyEvent::new(KeyCode::Right, KeyModifiers::NONE)),
        );
        record_frame(&mut app);
        assert_eq!(chat_widths(&app), vec![before[0] - 2, before[1] + 2]);

        for _ in 0..100 {
            handle_attached_event(
                &mut app,
                Event::Key(KeyEvent::new(KeyCode::Right, KeyModifiers::NONE)),
            );
            record_frame(&mut app);
        }
        assert_eq!(chat_widths(&app)[0], crate::ui::MIN_SPLIT_COLS);
        assert!(app.status_message.is_some(), "says why the key did nothing");

        handle_attached_event(
            &mut app,
            Event::Key(KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE)),
        );
        assert_eq!(app.mux.as_ref().unwrap().prefix_state, PrefixState::Idle);
        let _ = app.mux.as_mut().unwrap().shutdown();
    }

    /// Keys arrive faster than frames. Found running CST for real: three presses before
    /// the next draw each started from the same stale sizes, and moved one step.
    #[test]
    fn resize_steps_that_arrive_before_the_next_frame_still_add_up() {
        let mut app = split_app(2);
        let before = chat_widths(&app);
        app.mux.as_mut().unwrap().prefix_state = PrefixState::Transient(TransientMode::ResizeSplit);
        for _ in 0..3 {
            handle_attached_event(
                &mut app,
                Event::Key(KeyEvent::new(KeyCode::Right, KeyModifiers::NONE)),
            );
        }
        assert_eq!(chat_widths(&app), vec![before[0] - 6, before[1] + 6]);
        record_frame(&mut app);
        assert_eq!(chat_widths(&app), vec![before[0] - 6, before[1] + 6]);
        let _ = app.mux.as_mut().unwrap().shutdown();
    }

    /// `split_app`, with names and projects that share no characters with tab numbers,
    /// so a typed filter matches only what each test means it to.
    fn named_split_app(names: &[&str]) -> App {
        let mut app = split_app(names.len() as u64);
        for (pane, name) in app.mux.as_mut().unwrap().panes.iter_mut().zip(names) {
            pane.title = name.to_string();
            pane.cwd = std::path::PathBuf::from("/work/project");
        }
        app
    }

    fn type_text(app: &mut App, text: &str) {
        for character in text.chars() {
            press_attached(app, KeyCode::Char(character));
        }
    }

    #[test]
    fn the_split_picker_finds_a_session_by_the_tab_number_its_row_is_labelled_with() {
        let mut app = named_split_app(&["alpha", "beta", "gamma", "delta"]);
        app.open_split_picker(crate::mux::SplitDirection::Columns);
        assert_eq!(app.pane_list_ids(), vec![3, 4]);
        type_text(&mut app, "3");
        assert_eq!(
            app.pane_list_ids(),
            vec![4],
            "session 4 is in tab 3, now that 1 and 2 share tab 1"
        );
        assert_eq!(app.pane_selected, 1, "the match, not the new-session row");
        press_attached(&mut app, KeyCode::Enter);
        assert_eq!(app.mux.as_ref().unwrap().focused, Some(4));
        let _ = app.mux.as_mut().unwrap().shutdown();
    }

    /// The handler used to index every pane while the popup drew a filtered list, so
    /// Enter on the only match opened whichever session happened to be first.
    #[test]
    fn enter_in_a_filtered_switcher_opens_the_highlighted_match_not_the_row_at_that_index() {
        let mut app = named_split_app(&["api-server", "web-client", "docs-site"]);
        app.open_pane_list();
        type_text(&mut app, "docs");
        assert_eq!(app.pane_list_ids(), vec![3]);
        assert_eq!(app.pane_selected, 0);
        press_attached(&mut app, KeyCode::Enter);
        assert_eq!(app.mode, Mode::Normal);
        assert_eq!(app.mux.as_ref().unwrap().focused, Some(3));
        assert_eq!(app.view, View::Attached(3));
        let _ = app.mux.as_mut().unwrap().shutdown();
    }

    #[test]
    fn letters_that_used_to_be_switcher_commands_are_filter_text_and_never_close_a_session() {
        let mut app = named_split_app(&["fix-queue", "xkjq-tool"]);
        app.open_pane_list();
        type_text(&mut app, "xkjq");
        assert_eq!(app.mode, Mode::PaneList, "q no longer dismisses");
        assert_eq!(app.pane_filter, "xkjq");
        assert_eq!(app.pane_list_ids(), vec![2]);
        assert_eq!(
            app.mux.as_ref().unwrap().panes.len(),
            2,
            "x no longer closes the highlighted session"
        );
        let _ = app.mux.as_mut().unwrap().shutdown();
    }

    #[test]
    fn a_filter_matching_nothing_keeps_the_switcher_open_until_backspace_widens_it() {
        let mut app = named_split_app(&["alpha", "beta"]);
        app.open_pane_list();
        type_text(&mut app, "betz");
        assert!(app.pane_list_ids().is_empty());
        press_attached(&mut app, KeyCode::Enter);
        press_attached(&mut app, KeyCode::Down);
        press_attached(&mut app, KeyCode::Delete);
        assert_eq!(
            app.mode,
            Mode::PaneList,
            "Enter with nothing to open is a no-op"
        );
        assert_eq!(app.mux.as_ref().unwrap().panes.len(), 2);

        press_attached(&mut app, KeyCode::Backspace);
        assert_eq!(app.pane_list_ids(), vec![2]);
        press_attached(&mut app, KeyCode::Enter);
        assert_eq!(app.mux.as_ref().unwrap().focused, Some(2));
        let _ = app.mux.as_mut().unwrap().shutdown();
    }

    #[test]
    fn delete_closes_the_highlighted_match_and_the_next_opening_starts_unfiltered() {
        let mut app = named_split_app(&["alpha", "beta", "gamma"]);
        app.open_pane_list();
        type_text(&mut app, "gam");
        press_attached(&mut app, KeyCode::Delete);
        let mux = app.mux.as_ref().unwrap();
        assert_eq!(
            mux.panes.iter().map(|pane| pane.id).collect::<Vec<_>>(),
            vec![1, 2],
            "the match went, not the first session"
        );
        assert!(app.pane_list_ids().is_empty(), "still filtered to it");

        app.open_pane_list();
        assert_eq!(app.pane_filter, "");
        assert_eq!(app.pane_list_ids(), vec![1, 2]);
        let _ = app.mux.as_mut().unwrap().shutdown();
    }

    #[test]
    fn the_split_pickers_new_session_row_survives_any_filter() {
        let mut app = named_split_app(&["alpha", "beta", "gamma"]);
        app.open_split_picker(crate::mux::SplitDirection::Columns);
        type_text(&mut app, "nothing like it");
        assert!(app.pane_list_ids().is_empty());
        assert_eq!(app.pane_selected, 0, "on the new-session row");
        press_attached(&mut app, KeyCode::Esc);
        assert_eq!(app.mode, Mode::Normal);
        assert_eq!(app.split_picker, None);
        let _ = app.mux.as_mut().unwrap().shutdown();
    }

    /// Found by using it: choosing a session that shares a tab with others went to
    /// whichever split the tab last had focused, or nowhere from another tab.
    #[test]
    fn the_session_switcher_focuses_the_chosen_session_even_inside_a_split() {
        let mut app = split_app(3);
        // Tab 1 holds 1 and 2; tab 2 holds 3.
        app.mux.as_mut().unwrap().focus(1);
        app.open_pane_list();
        assert_eq!(app.pane_list_ids(), vec![1, 2, 3]);
        press_attached(&mut app, KeyCode::Down);
        press_attached(&mut app, KeyCode::Enter);
        assert_eq!(
            app.mux.as_ref().unwrap().focused,
            Some(2),
            "the other split"
        );
        assert_eq!(app.view, View::Attached(2));

        // From the other tab, straight to a session that is part of a split.
        app.mux.as_mut().unwrap().focus(3);
        app.open_pane_list();
        press_attached(&mut app, KeyCode::Up);
        press_attached(&mut app, KeyCode::Enter);
        let mux = app.mux.as_ref().unwrap();
        assert_eq!(mux.focused, Some(2));
        assert!(
            mux.current_window().unwrap().layout.contains(1),
            "its tab came back"
        );
        let _ = app.mux.as_mut().unwrap().shutdown();
    }

    #[test]
    fn ctrl_tab_and_ctrl_page_keys_switch_tabs_like_a_browser_from_any_panel() {
        let mut app = split_app(3);
        let key = |code, modifiers| Event::Key(KeyEvent::new(code, modifiers));
        let ctrl = KeyModifiers::CONTROL;
        assert_eq!(app.mux.as_ref().unwrap().focused, Some(2));

        handle_attached_event(&mut app, key(KeyCode::Tab, ctrl));
        assert_eq!(app.mux.as_ref().unwrap().focused, Some(3), "next tab");
        handle_attached_event(&mut app, key(KeyCode::BackTab, ctrl | KeyModifiers::SHIFT));
        assert_eq!(
            app.mux.as_ref().unwrap().focused,
            Some(2),
            "back, to who had focus"
        );
        handle_attached_event(&mut app, key(KeyCode::PageDown, ctrl));
        assert_eq!(app.mux.as_ref().unwrap().focused, Some(3));
        app.workspace_focus = WorkspaceFocus::Scratchpad;
        handle_attached_event(&mut app, key(KeyCode::PageUp, ctrl));
        assert_eq!(
            app.mux.as_ref().unwrap().focused,
            Some(2),
            "even from the scratchpad"
        );

        // Plain Tab still belongs to Copilot.
        assert_eq!(
            tab_switch_key(&KeyEvent::new(KeyCode::Tab, KeyModifiers::NONE)),
            None
        );
        assert_eq!(
            tab_switch_key(&KeyEvent::new(KeyCode::BackTab, KeyModifiers::SHIFT)),
            None
        );
        let _ = app.mux.as_mut().unwrap().shutdown();
    }

    #[test]
    fn deleting_all_prompt_text_clears_the_draft_before_switching_tabs() {
        let mut app = split_app(3);
        assert_eq!(app.mux.as_ref().unwrap().focused, Some(2));

        handle_attached_event(
            &mut app,
            Event::Key(KeyEvent::new(KeyCode::Char('x'), KeyModifiers::NONE)),
        );
        assert!(app.mux.as_ref().unwrap().pane(2).unwrap().has_draft());

        handle_attached_event(
            &mut app,
            Event::Key(KeyEvent::new(KeyCode::Backspace, KeyModifiers::NONE)),
        );
        handle_attached_event(
            &mut app,
            Event::Key(KeyEvent::new(KeyCode::Tab, KeyModifiers::CONTROL)),
        );

        let mux = app.mux.as_ref().unwrap();
        assert_eq!(mux.focused, Some(3), "the user navigated away");
        assert!(
            !mux.pane(2).unwrap().has_draft(),
            "the abandoned empty prompt must not keep its pencil"
        );
        let _ = app.mux.as_mut().unwrap().shutdown();
    }

    #[test]
    fn deleting_all_pasted_prompt_text_clears_the_draft_before_switching_tabs() {
        let mut app = split_app(3);
        assert_eq!(app.mux.as_ref().unwrap().focused, Some(2));

        handle_attached_event(&mut app, Event::Paste("x👨‍👩‍👧‍👦".to_string()));
        assert!(app.mux.as_ref().unwrap().pane(2).unwrap().has_draft());

        for _ in 0..2 {
            handle_attached_event(
                &mut app,
                Event::Key(KeyEvent::new(KeyCode::Backspace, KeyModifiers::NONE)),
            );
        }
        handle_attached_event(
            &mut app,
            Event::Key(KeyEvent::new(KeyCode::Tab, KeyModifiers::CONTROL)),
        );

        let mux = app.mux.as_ref().unwrap();
        assert_eq!(mux.focused, Some(3), "the user navigated away");
        assert!(
            !mux.pane(2).unwrap().has_draft(),
            "deleting the entire paste must clear the pencil"
        );
        let _ = app.mux.as_mut().unwrap().shutdown();
    }

    fn assert_control_clear_repaints_offscreen(code: char) {
        let mut app = split_app(3);
        handle_attached_event(&mut app, Event::Paste("one word".to_string()));
        assert!(app.mux.as_ref().unwrap().pane(2).unwrap().has_draft());

        handle_attached_event(
            &mut app,
            Event::Key(KeyEvent::new(KeyCode::Char(code), KeyModifiers::CONTROL)),
        );
        handle_attached_event(
            &mut app,
            Event::Key(KeyEvent::new(KeyCode::Tab, KeyModifiers::CONTROL)),
        );
        app.mux
            .as_mut()
            .unwrap()
            .pane_mut(2)
            .unwrap()
            .feed_synthetic(
                "────────────────────\r\n❯ \r\n────────────────────\x1b[2;3H".as_bytes(),
            );

        assert!(
            handle_mux_event(&mut app, MuxEvent::Output(2, Default::default())),
            "clearing an offscreen marker must repaint the tab strip"
        );
        let mux = app.mux.as_ref().unwrap();
        assert_eq!(mux.focused, Some(3), "the user already navigated away");
        assert!(
            !mux.pane(2).unwrap().has_draft(),
            "the rendered empty composer is authoritative after deletion"
        );
        let _ = app.mux.as_mut().unwrap().shutdown();
    }

    #[test]
    fn copilot_redrawing_an_empty_prompt_after_control_backspace_clears_an_offscreen_draft() {
        assert_control_clear_repaints_offscreen('w');
    }

    #[test]
    fn copilot_redrawing_an_empty_prompt_after_control_c_clears_an_offscreen_draft() {
        assert_control_clear_repaints_offscreen('c');
    }

    fn press_attached(app: &mut App, code: KeyCode) {
        handle_attached_event(app, Event::Key(KeyEvent::new(code, KeyModifiers::NONE)));
    }

    #[test]
    fn the_split_pickers_first_row_starts_a_new_session_by_asking_for_its_project_first() {
        let mut app = split_app(3);
        app.unique_projects = vec!["/work/alpha".to_string(), "/work/beta".to_string()];
        app.open_split_picker(crate::mux::SplitDirection::Rows);
        assert_eq!(app.pane_selected, 1, "an existing session is the default");
        press_attached(&mut app, KeyCode::Up);
        press_attached(&mut app, KeyCode::Enter);
        assert_eq!(app.mode, Mode::FilterProject);
        assert_eq!(
            app.new_split.as_ref().map(|new_split| new_split.anchor),
            Some(2)
        );
        // The focused session's own directory is offered first, and starts selected.
        let here = app
            .mux
            .as_ref()
            .unwrap()
            .focused_pane()
            .unwrap()
            .cwd
            .clone();
        assert_eq!(app.unique_projects[0], here.to_string_lossy());
        assert_eq!(app.project_selected, 0);

        press_attached(&mut app, KeyCode::Down);
        press_attached(&mut app, KeyCode::Down);
        press_attached(&mut app, KeyCode::Enter);
        assert_eq!(app.mode, Mode::NewSessionKind, "then as-is or worktree");
        assert_eq!(app.new_session_dir().as_deref(), Some("/work/beta"));
        assert_eq!(app.command_project().as_deref(), Some("/work/beta"));
        assert_eq!(app.project_filter, None, "the list filter is not touched");

        press_attached(&mut app, KeyCode::Esc);
        assert_eq!(app.new_split, None, "leaving the flow forgets the split");
        assert_ne!(app.new_session_dir().as_deref(), Some("/work/beta"));
        let _ = app.mux.as_mut().unwrap().shutdown();
    }

    #[test]
    fn a_session_opened_from_the_split_picker_joins_the_split_instead_of_a_new_tab() {
        let mut app = split_app(3);
        app.new_split = Some(crate::app::NewSplit {
            direction: crate::mux::SplitDirection::Rows,
            anchor: 2,
            project: Some("/work".to_string()),
        });
        // Stands in for the freshly spawned session, which opened in a tab of its own.
        app.mux.as_mut().unwrap().focus(3);
        app.join_pending_split(3);
        let mux = app.mux.as_ref().unwrap();
        assert_eq!(mux.windows.len(), 1);
        assert_eq!(mux.windows[0].layout.panes(), vec![1, 2, 3]);
        assert_eq!(mux.focused, Some(3));
        assert_eq!(app.new_split, None);
        let _ = app.mux.as_mut().unwrap().shutdown();
    }

    /// Session 1 in a tall column beside a stack of 2 over 3, focused on 3.
    fn nested_app() -> App {
        let mut app = split_app(3);
        assert!(app
            .mux
            .as_mut()
            .unwrap()
            .split_with(crate::mux::SplitDirection::Rows, 3));
        record_frame(&mut app);
        app
    }

    fn slot(app: &App, id: u64) -> Rect {
        app.workspace_areas
            .chats
            .iter()
            .find(|slot| slot.pane == id)
            .expect("on screen")
            .area
    }

    #[test]
    fn a_column_can_hold_a_stack_beside_one_tall_session() {
        let mut app = nested_app();
        let (one, two, three) = (slot(&app, 1), slot(&app, 2), slot(&app, 3));
        assert_eq!(
            one.height,
            two.height + three.height,
            "1 is as tall as the stack"
        );
        assert_eq!(two.x, three.x);
        assert_eq!(two.bottom(), three.y);
        assert!(one.right() <= two.x);
        let _ = app.mux.as_mut().unwrap().shutdown();
    }

    #[test]
    fn arrows_move_to_whatever_is_that_way_on_screen_however_the_splits_nest() {
        let mut app = nested_app();
        let step = |app: &mut App, step| {
            run_split_command(app, PrefixCommand::FocusSplit(step));
            app.mux.as_ref().unwrap().focused
        };
        use crate::mux::SplitStep::{Down, Left, Right, Up};
        assert_eq!(step(&mut app, Up), Some(2));
        assert_eq!(step(&mut app, Left), Some(1));
        assert_eq!(step(&mut app, Left), Some(1), "nothing further left");
        assert!(matches!(step(&mut app, Right), Some(2 | 3)));
        assert_eq!(step(&mut app, Down), Some(3));
        let _ = app.mux.as_mut().unwrap().shutdown();
    }

    #[test]
    fn dragging_the_border_inside_a_stack_trades_height_between_its_two_sessions_only() {
        let mut app = nested_app();
        let (one, three) = (slot(&app, 1), slot(&app, 3));
        let (column, row) = (three.x + 5, three.y);
        assert_eq!(
            app.workspace_areas.divider_at(column, row),
            Some(crate::app::Divider::Split {
                path: vec![1],
                index: 1
            })
        );
        handle_attached_event(
            &mut app,
            mouse(MouseEventKind::Down(MouseButton::Left), column, row),
        );
        handle_attached_event(
            &mut app,
            mouse(MouseEventKind::Drag(MouseButton::Left), column, row - 4),
        );
        handle_attached_event(
            &mut app,
            mouse(MouseEventKind::Up(MouseButton::Left), column, row - 4),
        );
        assert_eq!(slot(&app, 3).y, three.y - 4);
        assert_eq!(slot(&app, 3).height, three.height + 4);
        assert_eq!(slot(&app, 1), one, "the tall session is untouched");
        let _ = app.mux.as_mut().unwrap().shutdown();
    }

    #[test]
    fn resize_arrows_work_on_their_own_axis_in_a_column_holding_a_stack() {
        let mut app = nested_app();
        let (one, three) = (slot(&app, 1), slot(&app, 3));
        assert!(resize_focused_split(
            &mut app,
            crate::mux::SplitDirection::Rows,
            true
        ));
        assert_eq!(
            slot(&app, 3).height,
            three.height + 2,
            "down makes it taller"
        );
        assert_eq!(slot(&app, 1), one);
        assert!(resize_focused_split(
            &mut app,
            crate::mux::SplitDirection::Columns,
            true
        ));
        assert_eq!(
            slot(&app, 3).width,
            three.width + 2,
            "right widens the column"
        );
        assert_eq!(slot(&app, 1).width, one.width - 2);
        let _ = app.mux.as_mut().unwrap().shutdown();
    }

    /// Found by using it: the dock stayed while any session on screen had a scratchpad,
    /// so closing the focused one left a placeholder and the dock could not be shut.
    #[test]
    fn closing_the_scratchpad_in_a_split_closes_the_shared_dock_for_every_session() {
        let mut app = split_app(2);
        let temp = tempfile::tempdir().unwrap();
        app.scratchpad =
            Some(crate::scratchpad::Scratchpad::open_in(temp.path(), "split-2").unwrap());
        app.scratchpad_owner = Some(2);
        app.scratchpad_open.insert(1);
        app.scratchpad_open.insert(2);
        app.workspace_focus = WorkspaceFocus::Scratchpad;
        assert!(app.scratchpad_dock_visible());

        toggle_attached_scratchpad(&mut app);

        assert!(app.scratchpad_open.is_empty());
        assert!(!app.scratchpad_dock_visible());
        assert_eq!(app.workspace_focus, WorkspaceFocus::Chat);
        let _ = app.mux.as_mut().unwrap().shutdown();
    }

    /// Found by using it: focusing a split beside an open scratchpad showed "No
    /// scratchpad for …", when the user plainly wanted that session's notes there.
    #[test]
    fn focusing_a_split_beside_an_open_scratchpad_gives_that_session_its_own() {
        let mut app = split_app(2);
        app.mux.as_mut().unwrap().focus(1);
        app.view = View::Attached(1);
        app.scratchpad_open.insert(2);
        // Only the scratchpad: a terminal dock means a shell, so it is never implied.
        app.terminal_open.insert(2);

        sync_workspace_panels(&mut app);

        assert!(app.scratchpad_open.contains(&1));
        assert!(
            app.attached_scratchpad_visible(),
            "its own, not a placeholder"
        );
        assert!(!app.terminal_open.contains(&1));
        // Dropped unsaved, so the test leaves no empty notes file behind.
        app.scratchpad = None;
        let _ = app.mux.as_mut().unwrap().shutdown();
    }

    #[test]
    fn a_dock_stays_while_any_split_has_that_panel_so_moving_focus_reflows_nobody() {
        let mut app = split_app(2);
        app.scratchpad_open.insert(1);
        assert!(
            !app.attached_scratchpad_visible(),
            "focused session has none"
        );
        assert!(app.scratchpad_dock_visible());

        let layout = app.attached_layout(Rect::new(0, 0, 160, 40));
        assert!(layout.scratchpad.is_some());

        app.mux.as_mut().unwrap().toggle_split_zoom();
        assert!(
            !app.scratchpad_dock_visible(),
            "zoomed, only the focused session's own panels count"
        );
        let _ = app.mux.as_mut().unwrap().shutdown();
    }

    /// Switching by click must do the same bookkeeping as `prefix n`, or the new pane
    /// keeps its unread marker and the side panels stay on the old one.
    #[test]
    fn clicking_a_tab_switches_focus_and_acknowledges_the_pane() {
        let mut app = mux_app();
        push_test_pane(&mut app, 1, "alpha");
        push_test_pane(&mut app, 2, "beta");
        app.mux.as_mut().unwrap().focused = Some(1);
        app.view = View::Attached(1);
        // Give the target pane an unread marker to prove focusing clears it.
        app.mux.as_mut().unwrap().pane_mut(2).unwrap().apply_hook(
            crate::events::hooks::HookLifecycleEvent::Error { timestamp: 1 },
            false,
        );
        assert!(app.mux.as_ref().unwrap().pane(2).unwrap().needs_attention());

        // Column 0 is inside the first tab; a column past it lands in the second.
        let area = app.workspace_areas.tabs;
        assert_eq!(area.height, 0, "areas are only set once a frame is drawn");

        // Drive the layout the way a frame would, then click the second tab.
        let layout = crate::ui::attached_layout(
            ratatui::layout::Rect::new(0, 0, 80, 24),
            false,
            false,
            app.tab_bar_visible(),
        );
        app.workspace_areas.tabs = layout.tabs;
        let mux = app.mux.as_ref().unwrap();
        let sources = crate::ui::pane::tab_sources(mux);
        let (tab_list, _) = crate::ui::tabs::layout(&sources, 0, layout.tabs.width as usize);
        let second_tab_column = crate::text::display_width(&tab_list[0].label) as u16 + 1;

        handle_attached_event(
            &mut app,
            Event::Mouse(crossterm::event::MouseEvent {
                kind: MouseEventKind::Down(MouseButton::Left),
                column: second_tab_column,
                row: 0,
                modifiers: KeyModifiers::NONE,
            }),
        );

        assert_eq!(app.mux.as_ref().unwrap().focused, Some(2));
        assert_eq!(app.view, View::Attached(2));
        assert!(
            !app.mux.as_ref().unwrap().pane(2).unwrap().needs_attention(),
            "focusing a pane by click must acknowledge its unread marker"
        );
        let _ = app.mux.as_mut().unwrap().shutdown();
    }

    /// The command palette can raise the favorites modal without leaving the pane, so
    /// the attached path must answer its keys rather than forwarding them to the child.
    #[test]
    fn the_favorites_modal_takes_keys_while_attached_instead_of_the_pane() {
        let mut app = mux_app();
        app.view = View::Attached(1);
        app.mode = Mode::FavoriteOpen;

        handle_attached_event(
            &mut app,
            Event::Key(KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE)),
        );

        assert_eq!(
            app.mode,
            Mode::Normal,
            "Esc must close the modal from the attached view"
        );
    }

    #[test]
    fn choosing_panes_from_an_attached_pane_reports_a_result() {
        let mut app = mux_app();
        app.view = View::Attached(1);
        app.mode = Mode::FavoriteOpen;

        handle_attached_event(
            &mut app,
            Event::Key(KeyEvent::new(KeyCode::Char('p'), KeyModifiers::NONE)),
        );

        assert_eq!(app.mode, Mode::Normal);
        assert_eq!(
            app.status_message.as_deref(),
            Some("No favorite sessions configured")
        );
    }

    #[test]
    fn a_clear_then_working_batch_starts_a_fresh_notification_cycle() {
        use crate::host_terminal::ProgressState::{Clear, Indeterminate};
        use crate::mux::callbacks::PaneSignalEvent::Progress;

        assert!(!signals_start_cycle(true, &[Progress(Clear)]));
        assert!(signals_start_cycle(
            true,
            &[Progress(Clear), Progress(Indeterminate)]
        ));
        assert!(signals_start_cycle(
            false,
            &[Progress(Indeterminate), Progress(Clear)]
        ));
    }

    fn attached_mux_app(session_id: &str) -> App {
        let mut app = mux_app();
        push_test_pane(&mut app, 1, session_id);
        app.view = View::Attached(1);
        app
    }

    fn push_test_pane(app: &mut App, id: u64, session_id: &str) {
        push_test_pane_at(app, id, session_id, std::env::temp_dir());
    }

    fn push_test_pane_at(app: &mut App, id: u64, session_id: &str, cwd: std::path::PathBuf) {
        let events = app.mux.as_ref().unwrap().events.clone();
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
        let pane = Pane::spawn(
            PaneSpec {
                id,
                title: format!("Test session {id}"),
                cwd,
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
    }

    fn test_thread(number: u64) -> crate::threads::ThreadRef {
        crate::threads::ThreadRef {
            host: "github.com".to_string(),
            owner: "o".to_string(),
            repo: "r".to_string(),
            number,
            kind: crate::threads::ThreadKind::Issue,
        }
    }

    /// Write down a notice the way the watcher does, then act on it the way CST does.
    ///
    /// The split is the point of the design: the watcher only records, and everything
    /// about panes happens later, on whatever process is running at the time.
    fn plan_and_apply(app: &mut App, session_id: &str, thread: &crate::threads::ThreadRef) -> bool {
        crate::threads::store::update_in(&app.thread_state_root, |state| {
            state.record_notice(crate::threads::Notice {
                session_id: session_id.to_string(),
                thread: thread.clone(),
                comment_url: thread.url(),
                author: None,
                status: crate::threads::NoticeStatus::Planned,
                planned_at: chrono::Utc::now(),
            });
        })
        .expect("recording a notice");
        app.apply_thread_notices()
    }

    fn notices_in(app: &App) -> Vec<crate::threads::Notice> {
        crate::threads::store::load_in(&app.thread_state_root).notices
    }

    fn count_with_status(app: &App, status: crate::threads::NoticeStatus) -> usize {
        notices_in(app)
            .iter()
            .filter(|notice| notice.status == status)
            .count()
    }

    /// An instance that has not got the session open, while Copilot's own lock file says
    /// it is running somewhere — in another CST, most likely. The lock names this test's
    /// own process, which is as live as a pid gets.
    fn bystander_to_open_session(home: &std::path::Path, id: &str) -> App {
        let dir = home.join("session-state").join(id);
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join(format!("inuse.{}.lock", std::process::id())), "").unwrap();
        let mut app = App::new(Vec::new(), crate::config::UserConfig::default());
        app.mux = Some(crate::mux::MuxState::new(crate::mux::KeyChord {
            code: KeyCode::Char('b'),
            modifiers: KeyModifiers::CONTROL,
        }));
        app.copilot_home = home.to_path_buf();
        app.thread_state_root = home.to_path_buf();
        app
    }

    #[test]
    fn an_instance_without_the_session_open_leaves_its_reply_for_the_one_that_has_it() {
        // Every CST runs a watcher over the same threads.json, so the instance that
        // happens to see a reply first is often not the one with the session open. It
        // used to hold the reply as "its session is closed" on the spot, and the session
        // that was open and waiting never heard of it.
        let temp = tempfile::tempdir().unwrap();
        let mut other = bystander_to_open_session(temp.path(), "session-a");

        plan_and_apply(&mut other, "session-a", &test_thread(12));

        assert_eq!(
            count_with_status(&other, crate::threads::NoticeStatus::Planned),
            1,
            "got: {:?}",
            notices_in(&other)
        );
        assert!(other.thread_pending.is_empty());
    }

    #[test]
    fn a_tab_another_instance_opened_but_has_not_used_yet_still_counts_as_open() {
        // Copilot only takes its lock once a session runs a turn, so right after a CST
        // restarts, every tab it resumed looks closed by that measure. Found in the
        // end-to-end run: the other instance's workspace record is what still shows it.
        let temp = tempfile::tempdir().unwrap();
        let mut other = App::new(Vec::new(), crate::config::UserConfig::default());
        other.mux = Some(crate::mux::MuxState::new(crate::mux::KeyChord {
            code: KeyCode::Char('b'),
            modifiers: KeyModifiers::CONTROL,
        }));
        other.copilot_home = temp.path().to_path_buf();
        other.thread_state_root = temp.path().to_path_buf();
        let records = temp.path().join("workspaces");
        other.workspace_record_root = Some(records.clone());
        // Another instance: this test's pid stands in for a running process, and a
        // different start time makes it somebody else's record.
        crate::recovery::write_in(
            &records,
            &crate::recovery::WorkspaceRecord {
                pid: std::process::id(),
                started_at: other.instance_started_at - chrono::Duration::minutes(5),
                saved_at: chrono::Utc::now(),
                launch_dir: None,
                sessions: vec![crate::recovery::RecordedSession {
                    session_id: "session-a".to_string(),
                    cwd: temp.path().display().to_string(),
                    title: "session-a".to_string(),
                }],
                focused_session_id: None,
                windows: Vec::new(),
            },
        )
        .unwrap();

        plan_and_apply(&mut other, "session-a", &test_thread(12));

        assert_eq!(
            count_with_status(&other, crate::threads::NoticeStatus::Planned),
            1,
            "got: {:?}",
            notices_in(&other)
        );
    }

    #[test]
    fn a_reply_left_for_another_instance_still_reaches_the_user_if_nobody_delivers_it() {
        // The session may be open in a bare terminal, or in a CST too old to look. Standing
        // aside forever would lose the reply exactly the way the bug did.
        let temp = tempfile::tempdir().unwrap();
        let mut other = bystander_to_open_session(temp.path(), "session-a");
        let thread = test_thread(12);
        plan_and_apply(&mut other, "session-a", &thread);

        crate::threads::store::update_in(temp.path(), |state| {
            if let Some(notice) = state.notice_mut("session-a", &thread) {
                notice.planned_at = chrono::Utc::now() - chrono::Duration::hours(1);
            }
        })
        .unwrap();
        other.flush_thread_wakes();

        assert_eq!(
            count_with_status(
                &other,
                crate::threads::NoticeStatus::Waiting {
                    reason: crate::threads::PendingReason::SessionClosed
                }
            ),
            1,
            "got: {:?}",
            notices_in(&other)
        );
    }

    #[test]
    fn an_instance_without_the_session_open_does_not_free_a_slot_another_one_is_holding() {
        // Only the instance with the pane can see whether its composer has emptied. One
        // that has no pane would count the composer as free and drop the marker, and the
        // owner, which now reads the file, would lose its own bookkeeping.
        let temp = tempfile::tempdir().unwrap();
        let mut other = bystander_to_open_session(temp.path(), "session-a");
        let thread = test_thread(12);
        crate::threads::store::update_in(temp.path(), |state| {
            state.record_notice(crate::threads::Notice {
                session_id: "session-a".to_string(),
                thread: thread.clone(),
                comment_url: thread.url(),
                author: None,
                status: crate::threads::NoticeStatus::Sent,
                planned_at: chrono::Utc::now(),
            });
        })
        .unwrap();

        other.flush_thread_wakes();

        assert_eq!(
            count_with_status(&other, crate::threads::NoticeStatus::Sent),
            1,
            "got: {:?}",
            notices_in(&other)
        );
    }

    #[test]
    fn a_reply_left_unread_in_a_composer_that_closed_reaches_the_inbox_instead_of_vanishing() {
        // The CST that typed the link quit, or its tab was closed, before any turn took
        // it. Every other CST now reads the file and would count a composer it never had
        // as empty, which used to drop the reply without a word to anyone.
        let temp = tempfile::tempdir().unwrap();
        let mut other = App::new(Vec::new(), crate::config::UserConfig::default());
        other.mux = Some(crate::mux::MuxState::new(crate::mux::KeyChord {
            code: KeyCode::Char('b'),
            modifiers: KeyModifiers::CONTROL,
        }));
        other.copilot_home = temp.path().to_path_buf();
        other.thread_state_root = temp.path().to_path_buf();
        let thread = test_thread(12);
        crate::threads::store::update_in(temp.path(), |state| {
            state.record_notice(crate::threads::Notice {
                session_id: "session-a".to_string(),
                thread: thread.clone(),
                comment_url: thread.url(),
                author: None,
                status: crate::threads::NoticeStatus::Sent,
                planned_at: chrono::Utc::now(),
            });
        })
        .unwrap();

        other.flush_thread_wakes();
        other.flush_thread_wakes();

        assert_eq!(
            other
                .thread_pending_for("session-a")
                .and_then(crate::threads::Notice::reason),
            Some(crate::threads::PendingReason::SessionClosed),
            "got: {:?}",
            notices_in(&other)
        );
    }

    #[test]
    fn the_instance_with_the_session_open_delivers_a_reply_another_instance_planned() {
        // The other half. The poll cursors are shared too, so once one instance has seen
        // a comment, every other one is told by GitHub that nothing changed. The file is
        // the only place the owner can learn about it.
        let temp = tempfile::tempdir().unwrap();
        let mut owner = App::new(Vec::new(), crate::config::UserConfig::default());
        owner.mux = Some(crate::mux::MuxState::new(crate::mux::KeyChord {
            code: KeyCode::Char('b'),
            modifiers: KeyModifiers::CONTROL,
        }));
        push_test_pane_at(&mut owner, 1, "session-a", temp.path().to_path_buf());
        owner.thread_state_root = temp.path().to_path_buf();
        owner.refresh_thread_pending();

        // Written by another process; this one was never told.
        let thread = test_thread(12);
        crate::threads::store::update_in(temp.path(), |state| {
            state.record_notice(crate::threads::Notice {
                session_id: "session-a".to_string(),
                thread: thread.clone(),
                comment_url: thread.url(),
                author: None,
                status: crate::threads::NoticeStatus::Planned,
                planned_at: chrono::Utc::now(),
            });
        })
        .unwrap();

        assert!(owner.flush_thread_wakes(), "got: {:?}", notices_in(&owner));
        assert_eq!(
            count_with_status(&owner, crate::threads::NoticeStatus::Sent),
            1
        );
    }

    /// The reported bug, end to end: typing, a wake-up arriving, and what happens next.
    ///
    /// Deliberately driven through `handle_attached_event` rather than by calling the
    /// draft tracker directly. The unit tests cover the rules; this covers the wiring,
    /// which is where the mistake actually was — a gate that never asked the question.
    #[test]
    fn a_wake_up_waits_for_a_half_typed_message_and_arrives_once_it_is_sent() {
        let temp = tempfile::tempdir().unwrap();
        let mut app = App::new(Vec::new(), crate::config::UserConfig::default());
        app.mux = Some(crate::mux::MuxState::new(crate::mux::KeyChord {
            code: KeyCode::Char('b'),
            modifiers: KeyModifiers::CONTROL,
        }));
        push_test_pane_at(&mut app, 1, "session-a", temp.path().to_path_buf());
        app.view = crate::app::View::Attached(1);
        // Never the real state directory: delivery writes notices to disk, and a test
        // must not put one into the running user's inbox.
        app.thread_state_root = temp.path().to_path_buf();

        let thread = test_thread(12);

        // The user starts typing a message, through the real event path.
        for character in "you probably saw it".chars() {
            handle_attached_event(
                &mut app,
                Event::Key(KeyEvent::new(KeyCode::Char(character), KeyModifiers::NONE)),
            );
        }
        assert!(
            app.mux.as_ref().unwrap().pane(1).unwrap().has_draft(),
            "typing through the real key path must register a draft"
        );

        // A reply lands mid-sentence. This is the moment that mangled the message.
        plan_and_apply(&mut app, "session-a", &thread);
        assert_eq!(
            count_with_status(&app, crate::threads::NoticeStatus::Planned),
            1,
            "the wake-up must wait rather than paste onto a half-written message"
        );

        // The user finishes and sends. Now the pane is free.
        handle_attached_event(
            &mut app,
            Event::Key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE)),
        );
        assert!(
            !app.mux.as_ref().unwrap().pane(1).unwrap().has_draft(),
            "sending clears the draft"
        );

        assert!(app.flush_thread_wakes(), "the wake-up is delivered now");
        assert_eq!(
            count_with_status(&app, crate::threads::NoticeStatus::Sent),
            1,
            "nothing is left planned; the notice that remains has been written and is \
             held only until a turn takes it"
        );
    }

    /// A busy thread must not stack its notices into one message.
    ///
    /// Reported from a live autopilot run: the same notice appeared four times in a
    /// single message. Each was a genuine wake-up for a different comment, but Copilot
    /// had not consumed the previous one, so they piled up in the composer.
    #[test]
    fn a_second_wake_up_waits_until_the_first_has_been_taken() {
        let temp = tempfile::tempdir().unwrap();
        let mut app = App::new(Vec::new(), crate::config::UserConfig::default());
        app.mux = Some(crate::mux::MuxState::new(crate::mux::KeyChord {
            code: KeyCode::Char('b'),
            modifiers: KeyModifiers::CONTROL,
        }));
        push_test_pane_at(&mut app, 1, "session-a", temp.path().to_path_buf());
        app.view = crate::app::View::Attached(1);
        app.thread_state_root = temp.path().to_path_buf();

        plan_and_apply(&mut app, "session-a", &test_thread(12));
        assert_eq!(
            count_with_status(&app, crate::threads::NoticeStatus::Sent),
            1,
            "the first wake-up goes straight through"
        );

        // Copilot has not started a turn, so the notice is still sitting there.
        plan_and_apply(&mut app, "session-a", &test_thread(13));
        assert_eq!(
            count_with_status(&app, crate::threads::NoticeStatus::Planned),
            1,
            "a second notice must not stack onto one that has not been taken"
        );

        // The turn runs and finishes. Starting it is what empties the composer, but a
        // pane mid-turn is not writable either, so the wake-up waits for the end.
        {
            let pane = app.mux.as_mut().unwrap().pane_mut(1).unwrap();
            pane.apply_hook(
                crate::events::hooks::HookLifecycleEvent::Working { timestamp: 1 },
                false,
            );
            assert!(!pane.has_draft(), "the turn took what was in the composer");
            pane.apply_hook(
                crate::events::hooks::HookLifecycleEvent::Ready { timestamp: 2 },
                false,
            );
            pane.confirm_hook_ready(2, false);
        }

        assert!(app.flush_thread_wakes(), "now the second one is delivered");
        assert_eq!(
            count_with_status(&app, crate::threads::NoticeStatus::Planned),
            0,
            "nothing is left waiting once the pane is free again"
        );
    }

    #[test]
    fn a_thread_reply_waits_for_a_long_running_turn_and_arrives_when_it_finishes() {
        let temp = tempfile::tempdir().unwrap();
        let mut app = App::new(Vec::new(), crate::config::UserConfig::default());
        app.mux = Some(crate::mux::MuxState::new(crate::mux::KeyChord {
            code: KeyCode::Char('b'),
            modifiers: KeyModifiers::CONTROL,
        }));
        push_test_pane_at(&mut app, 1, "session-a", temp.path().to_path_buf());
        app.view = crate::app::View::Attached(1);
        app.thread_state_root = temp.path().to_path_buf();

        app.mux.as_mut().unwrap().pane_mut(1).unwrap().apply_hook(
            crate::events::hooks::HookLifecycleEvent::Working { timestamp: 1 },
            false,
        );

        let thread = test_thread(12);
        plan_and_apply(&mut app, "session-a", &thread);
        crate::threads::store::update_in(&app.thread_state_root, |state| {
            state
                .notice_mut("session-a", &thread)
                .expect("the reply is queued")
                .planned_at = chrono::Utc::now() - chrono::Duration::minutes(37);
        })
        .expect("backdating the reply");

        app.flush_thread_wakes();
        assert_eq!(
            count_with_status(&app, crate::threads::NoticeStatus::Planned),
            1,
            "a long working turn must not strand a reply as a closed-session notice"
        );
        assert!(app.thread_pending.is_empty());
        assert!(
            !app.mux.as_ref().unwrap().pane(1).unwrap().has_draft(),
            "the reply must not be injected while the turn is working"
        );

        {
            let pane = app.mux.as_mut().unwrap().pane_mut(1).unwrap();
            pane.apply_hook(
                crate::events::hooks::HookLifecycleEvent::Ready { timestamp: 2 },
                false,
            );
            pane.confirm_hook_ready(2, false);
        }
        assert!(
            app.flush_thread_wakes(),
            "finishing the turn delivers the reply"
        );
        assert_eq!(
            count_with_status(&app, crate::threads::NoticeStatus::Sent),
            1
        );
        assert_eq!(
            count_with_status(&app, crate::threads::NoticeStatus::Planned),
            0
        );
        assert!(
            !app.flush_thread_wakes(),
            "an unread reply must not be delivered twice"
        );
    }

    #[test]
    fn a_long_turn_that_ends_with_a_draft_still_gives_the_reply_the_full_five_minutes() {
        // Patience exists for a pane blocked by the user, so it has to be measured from
        // when the user became the obstacle. Counted from when the reply arrived, a
        // 37-minute turn would leave it overdue the moment the turn ended, and a draft
        // typed in the meantime would send it straight to the inbox.
        let temp = tempfile::tempdir().unwrap();
        let mut app = App::new(Vec::new(), crate::config::UserConfig::default());
        app.mux = Some(crate::mux::MuxState::new(crate::mux::KeyChord {
            code: KeyCode::Char('b'),
            modifiers: KeyModifiers::CONTROL,
        }));
        push_test_pane_at(&mut app, 1, "session-a", temp.path().to_path_buf());
        app.view = crate::app::View::Attached(1);
        app.thread_state_root = temp.path().to_path_buf();
        app.mux.as_mut().unwrap().pane_mut(1).unwrap().apply_hook(
            crate::events::hooks::HookLifecycleEvent::Working { timestamp: 1 },
            false,
        );

        let thread = test_thread(12);
        plan_and_apply(&mut app, "session-a", &thread);
        crate::threads::store::update_in(&app.thread_state_root, |state| {
            state
                .notice_mut("session-a", &thread)
                .expect("the reply is queued")
                .planned_at = chrono::Utc::now() - chrono::Duration::minutes(37);
        })
        .expect("backdating the reply");
        app.flush_thread_wakes();

        {
            let pane = app.mux.as_mut().unwrap().pane_mut(1).unwrap();
            pane.apply_hook(
                crate::events::hooks::HookLifecycleEvent::Ready { timestamp: 2 },
                false,
            );
            pane.confirm_hook_ready(2, false);
        }
        handle_attached_event(
            &mut app,
            Event::Key(KeyEvent::new(KeyCode::Char('h'), KeyModifiers::NONE)),
        );
        assert!(app.mux.as_ref().unwrap().pane(1).unwrap().has_draft());

        app.flush_thread_wakes();
        assert_eq!(
            count_with_status(&app, crate::threads::NoticeStatus::Planned),
            1,
            "the turn just ended; the draft has not had five minutes yet"
        );
        assert!(app.thread_pending.is_empty());
    }

    /// A notice planned by one run is still delivered by the next.
    ///
    /// This is why the status lives on disk rather than in memory. The comment behind a
    /// notice is marked seen the moment it is planned, so anything lost at a restart is
    /// a message the session is never told about again — which is the exact failure the
    /// whole feature exists to prevent.
    #[test]
    fn a_notice_planned_before_a_restart_is_delivered_after_it() {
        let temp = tempfile::tempdir().unwrap();
        let thread = test_thread(12);

        // The run that hears about the comment. It records the notice and then dies
        // before any pane is available to receive it.
        crate::threads::store::update_in(temp.path(), |state| {
            state.record_notice(crate::threads::Notice {
                session_id: "session-a".to_string(),
                thread: thread.clone(),
                comment_url: thread.url(),
                author: None,
                status: crate::threads::NoticeStatus::Planned,
                planned_at: chrono::Utc::now(),
            });
        })
        .expect("recording a notice");

        // A fresh process, with nothing carried over but the file.
        let mut app = App::new(Vec::new(), crate::config::UserConfig::default());
        app.mux = Some(crate::mux::MuxState::new(crate::mux::KeyChord {
            code: KeyCode::Char('b'),
            modifiers: KeyModifiers::CONTROL,
        }));
        app.thread_state_root = temp.path().to_path_buf();
        crate::threads::store::resume_in(temp.path(), |_| false).expect("resume");
        // Exactly what startup does, in the same order.
        app.refresh_thread_pending();
        push_test_pane_at(&mut app, 1, "session-a", temp.path().to_path_buf());
        app.view = crate::app::View::Attached(1);

        app.flush_thread_wakes();

        assert_eq!(
            count_with_status(&app, crate::threads::NoticeStatus::Sent),
            1,
            "the new run picks up what the old one planned"
        );
    }

    /// A notice written into a composer that a restart threw away is written again.
    ///
    /// `Sent` means "sitting in a composer waiting for a turn". That composer does not
    /// survive the process, so on the way back up the notice has to return to the queue
    /// or it would be dropped having never reached anyone.
    #[test]
    fn a_notice_left_in_a_composer_returns_to_the_queue_on_restart() {
        let temp = tempfile::tempdir().unwrap();
        let thread = test_thread(12);
        crate::threads::store::update_in(temp.path(), |state| {
            state.record_notice(crate::threads::Notice {
                session_id: "session-a".to_string(),
                thread: thread.clone(),
                comment_url: thread.url(),
                author: None,
                status: crate::threads::NoticeStatus::Sent,
                planned_at: chrono::Utc::now(),
            });
        })
        .expect("recording a notice");

        crate::threads::store::resume_in(temp.path(), |_| false).expect("resume");

        let notices = crate::threads::store::load_in(temp.path()).notices;
        assert_eq!(notices.len(), 1);
        assert_eq!(
            notices[0].status,
            crate::threads::NoticeStatus::Planned,
            "a notice that was in a composer that no longer exists is queued again"
        );
    }

    /// A wake-up really reaches the child, not just the queue.
    ///
    /// Everything else about delivery is asserted on CST's own state, which would look
    /// identical if the write never left the process. Here the child echoes what it is
    /// given, so its screen is the evidence.
    ///
    /// It does **not** cover the stacking fix, though it was written to. With an echoing
    /// child the second delivery is blocked by the working state as well, so the test
    /// passed with the fix removed — proving nothing. `a_second_wake_up_waits_until_the
    /// _first_has_been_taken` is the one that isolates it, verified by removing the fix
    /// and watching it fail.
    #[test]
    fn a_wake_up_reaches_the_child_process_and_not_only_the_queue() {
        let temp = tempfile::tempdir().unwrap();
        let mut app = App::new(Vec::new(), crate::config::UserConfig::default());
        app.mux = Some(crate::mux::MuxState::new(crate::mux::KeyChord {
            code: KeyCode::Char('b'),
            modifiers: KeyModifiers::CONTROL,
        }));
        // A child that prints whatever is typed at it, so the write is observable.
        let events = app.mux.as_ref().unwrap().events.clone();
        let (program, args) = if cfg!(windows) {
            (
                "cmd.exe".to_string(),
                vec!["/c".to_string(), "findstr \"^\"".to_string()],
            )
        } else {
            ("/bin/cat".to_string(), Vec::new())
        };
        let pane = crate::mux::Pane::spawn(
            crate::mux::PaneSpec {
                id: 1,
                title: "echo".to_string(),
                cwd: temp.path().to_path_buf(),
                session_id: "session-a".to_string(),
                program,
                args,
                events_path: None,
                terminal_light_mode: Some(false),
                hooks_active: false,
            },
            24,
            200,
            events,
        )
        .unwrap();
        app.mux.as_mut().unwrap().push(pane);
        app.view = crate::app::View::Attached(1);
        app.thread_state_root = temp.path().to_path_buf();

        plan_and_apply(&mut app, "session-a", &test_thread(12));
        plan_and_apply(&mut app, "session-a", &test_thread(13));

        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
        let mut screen = String::new();
        while std::time::Instant::now() < deadline {
            if let Some(pane) = app.mux.as_mut().and_then(|mux| mux.pane_mut(1)) {
                pane.refresh_from_callbacks(false);
                screen = pane
                    .with_screen(|screen| screen.contents())
                    .unwrap_or_default();
            }
            if screen.contains("issues/12") {
                break;
            }
            std::thread::sleep(std::time::Duration::from_millis(50));
        }

        assert!(
            screen.contains("issues/12"),
            "the notice should have reached the child, got: {screen}"
        );
        assert!(
            screen.contains("thread leave"),
            "and in full, not truncated on the way, got: {screen}"
        );
    }

    /// A wake-up that can never be delivered ends up with the user, not nowhere.
    ///
    /// The pane here holds an unsent draft and nothing will clear it, which is what an
    /// abandoned half-typed message looks like. Waiting forever would swallow the
    /// message in silence — the exact failure the whole feature exists to prevent.
    #[test]
    fn a_wake_up_that_waits_too_long_is_handed_to_the_user() {
        let temp = tempfile::tempdir().unwrap();
        let mut app = App::new(Vec::new(), crate::config::UserConfig::default());
        app.mux = Some(crate::mux::MuxState::new(crate::mux::KeyChord {
            code: KeyCode::Char('b'),
            modifiers: KeyModifiers::CONTROL,
        }));
        push_test_pane_at(&mut app, 1, "session-a", temp.path().to_path_buf());
        app.view = crate::app::View::Attached(1);
        app.thread_state_root = temp.path().to_path_buf();

        // Somebody typed and walked away.
        handle_attached_event(
            &mut app,
            Event::Key(KeyEvent::new(KeyCode::Char('h'), KeyModifiers::NONE)),
        );

        let thread = test_thread(12);
        plan_and_apply(&mut app, "session-a", &thread);
        assert_eq!(
            count_with_status(&app, crate::threads::NoticeStatus::Planned),
            1,
            "waiting on the draft"
        );

        // Backdate it rather than waiting five real minutes. This is also the assertion
        // that the deadline is measured from a persisted timestamp: an in-memory clock
        // could not be moved from here.
        crate::threads::store::update_in(app.thread_state_root.clone().as_path(), |state| {
            if let Some(notice) = state.notice_mut("session-a", &thread) {
                notice.planned_at = chrono::Utc::now() - chrono::Duration::hours(1);
            }
        })
        .expect("backdating");

        assert!(app.flush_thread_wakes());
        assert_eq!(
            count_with_status(&app, crate::threads::NoticeStatus::Planned),
            0,
            "it stops waiting rather than holding the message forever"
        );
        assert_eq!(
            app.thread_pending.len(),
            1,
            "and becomes something the user is asked about"
        );
    }

    /// Two notices never arrive back to back, whether or not they are the same thread.
    ///
    /// The earlier fix stopped a second notice being *written* while the first sat
    /// unconsumed, but the queue could still hold several and release them together:
    /// readiness was decided once before the loop, so every entry was delivered on that
    /// one decision. This covers both halves of that.
    #[test]
    fn a_queue_of_notices_is_released_one_at_a_time() {
        let temp = tempfile::tempdir().unwrap();
        let mut app = App::new(Vec::new(), crate::config::UserConfig::default());
        app.mux = Some(crate::mux::MuxState::new(crate::mux::KeyChord {
            code: KeyCode::Char('b'),
            modifiers: KeyModifiers::CONTROL,
        }));
        push_test_pane_at(&mut app, 1, "session-a", temp.path().to_path_buf());
        app.view = crate::app::View::Attached(1);
        app.thread_state_root = temp.path().to_path_buf();

        // First goes straight through. It stays on the list, marked sent, because being
        // written is not the same as being read.
        plan_and_apply(&mut app, "session-a", &test_thread(12));
        assert_eq!(notices_in(&app).len(), 1);
        assert_eq!(
            count_with_status(&app, crate::threads::NoticeStatus::Sent),
            1
        );

        // The same thread moving again adds nothing: the notice is the same link, and
        // the first one has not been read yet.
        plan_and_apply(&mut app, "session-a", &test_thread(12));
        plan_and_apply(&mut app, "session-a", &test_thread(12));
        assert_eq!(
            notices_in(&app).len(),
            1,
            "a repeat notice for one thread should not be queued behind the first"
        );

        // Two other threads do queue, because each names somewhere different to look.
        plan_and_apply(&mut app, "session-a", &test_thread(13));
        plan_and_apply(&mut app, "session-a", &test_thread(14));
        assert_eq!(notices_in(&app).len(), 3);

        // Freeing the pane must release exactly one of them, not both at once.
        // Timestamps have to keep climbing: the pane drops lifecycle events older than
        // the last one it saw, so reusing them makes the second turn a no-op.
        let mut clock = 0u64;
        let mut free = |app: &mut App| {
            let pane = app.mux.as_mut().unwrap().pane_mut(1).unwrap();
            clock += 1;
            pane.apply_hook(
                crate::events::hooks::HookLifecycleEvent::Working { timestamp: clock },
                false,
            );
            clock += 1;
            pane.apply_hook(
                crate::events::hooks::HookLifecycleEvent::Ready { timestamp: clock },
                false,
            );
            pane.confirm_hook_ready(clock, false);
        };

        // Drained a turn at a time, asserting the invariant at every step rather than
        // counting cycles. How many turns it takes is not fixed: when a cycle produced
        // no ready notification, `apply_hook`'s Ready arm leaves `hook_ready_pending`
        // unset, `confirm_hook_ready` returns early without clearing `working`, and the
        // next notice waits one more turn. That slip is harmless — nothing is lost or
        // doubled — but pinning an exact count made this fail about once in a dozen
        // full-suite runs. What must never happen is two outstanding at once.
        let mut turns = 0;
        while !notices_in(&app).is_empty() {
            assert!(
                count_with_status(&app, crate::threads::NoticeStatus::Sent) <= 1,
                "only one notice may be unread at a time, got: {:?}",
                notices_in(&app)
            );
            free(&mut app);
            app.flush_thread_wakes();
            turns += 1;
            assert!(
                turns <= 10,
                "the list should drain as turns are taken, got: {:?}",
                notices_in(&app)
            );
        }
        assert!(
            turns >= 3,
            "three notices cannot be delivered in fewer than three turns"
        );
    }

    /// Put a named link on an attached pane's screen, with the pane where the event loop
    /// would put it in a 100×30 terminal.
    ///
    /// Positioned by the production layout rather than by hand. A hand-picked origin is
    /// how this test first went wrong: the pane was sized to one place and drawn in
    /// another, and only the frame test noticed. Returns the pane's origin in outer
    /// coordinates, which is never (0, 0) — the chat box's border sits there — so every
    /// click below also proves the outer-to-pane translation.
    fn app_showing_a_link(session: &str, target: &str) -> (App, u16, u16) {
        let mut app = attached_mux_app(session);
        // Read from the environment in real use; pinned so these tests mean the same thing
        // however they are run, including from an SSH session.
        app.over_ssh = false;
        let pane_area = crate::ui::attached_layout(
            ratatui::layout::Rect::new(0, 0, 100, 30),
            app.attached_scratchpad_visible(),
            app.attached_terminal_visible(),
            app.tab_bar_visible(),
        )
        .chat_pane();
        app.mux.as_mut().unwrap().resize_all_at(
            pane_area.x,
            pane_area.y,
            pane_area.height,
            pane_area.width,
        );
        // The child is a real process, and ConPTY clears the screen as it starts and as it
        // is resized. Let that land first, or it can wipe the link before it is used.
        std::thread::sleep(std::time::Duration::from_millis(300));
        app.mux
            .as_mut()
            .unwrap()
            .pane_mut(1)
            .unwrap()
            .feed_synthetic(
                format!("\x1b[1;1Hsee \x1b]8;;{target}\x1b\\Open the local gallery\x1b]8;;\x1b\\.")
                    .as_bytes(),
            );
        (app, pane_area.x, pane_area.y)
    }

    fn mouse_down(column: u16, row: u16, modifiers: KeyModifiers) -> Event {
        Event::Mouse(crossterm::event::MouseEvent {
            kind: MouseEventKind::Down(MouseButton::Left),
            column,
            row,
            modifiers,
        })
    }

    /// The report, through the real input path.
    ///
    /// A coworker clicked "Open the local gallery" and nothing happened. The link's target
    /// lived only in the escape sequence, which CST used to drop, so no click could ever
    /// have reached it.
    #[test]
    fn ctrl_clicking_a_named_link_opens_where_it_points() {
        let (mut app, x, y) = app_showing_a_link("links", "file:///D:/gallery/index.html");
        // The link's text runs from pane column 4 to 25; this is a letter in the middle.
        let on_link = x + 10;

        handle_attached_event(&mut app, mouse_down(on_link, y, KeyModifiers::NONE));
        assert!(
            app.opened_links.is_empty(),
            "a plain click still belongs to Copilot, which uses it for its own interface"
        );

        handle_attached_event(&mut app, mouse_down(on_link, y, KeyModifiers::CONTROL));
        assert_eq!(app.opened_links.len(), 1, "got: {:?}", app.status_message);
        assert!(matches!(
            &app.opened_links[0],
            crate::links::Target::File(_)
        ));

        handle_attached_event(&mut app, mouse_down(x + 26, y, KeyModifiers::CONTROL));
        assert_eq!(
            app.opened_links.len(),
            1,
            "the full stop after the link is not part of it"
        );
        let _ = app.mux.as_mut().unwrap().shutdown();
    }

    /// Draw one real frame of `app` through the production path, returning the bytes a
    /// terminal would receive. `links` false leaves hyperlinks out, for comparison.
    fn frame_bytes(app: &mut App, links: bool) -> Vec<u8> {
        #[derive(Clone, Default)]
        struct Recorder(std::sync::Arc<std::sync::Mutex<Vec<u8>>>);
        impl std::io::Write for Recorder {
            fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
                self.0.lock().unwrap().extend_from_slice(bytes);
                Ok(bytes.len())
            }
            fn flush(&mut self) -> std::io::Result<()> {
                Ok(())
            }
        }
        let recorder = Recorder::default();
        let mut terminal = ratatui::Terminal::with_options(
            ratatui::backend::CrosstermBackend::new(recorder.clone()),
            ratatui::TerminalOptions {
                viewport: ratatui::Viewport::Fixed(ratatui::layout::Rect::new(0, 0, 100, 30)),
            },
        )
        .unwrap();
        crate::synchronized_frame(&mut terminal, |frame| {
            crate::ui::draw(frame, app);
            if links {
                app.visible_hyperlinks()
            } else {
                Vec::new()
            }
        })
        .unwrap();
        let bytes = recorder.0.lock().unwrap().clone();
        bytes
    }

    fn screen_after(bytes: &[u8]) -> (String, Vec<String>) {
        let mut parser = vt100::Parser::new(30, 100, 0);
        parser.process(bytes);
        let screen = parser.screen();
        let styles = (0..30u16)
            .flat_map(|row| (0..100u16).map(move |col| (row, col)))
            .filter_map(|(row, col)| screen.cell(row, col))
            .map(|cell| {
                format!(
                    "{:?}{:?}{}",
                    cell.fgcolor(),
                    cell.bgcolor(),
                    cell.underline()
                )
            })
            .collect();
        (screen.contents(), styles)
    }

    /// The SSH requirement, end to end through a real frame.
    ///
    /// Over SSH, CST runs on the remote host and cannot reach the local browser — except
    /// through what it writes. So the link has to leave CST as a real hyperlink, and the
    /// terminal on the user's machine opens it. This draws a frame the way the event loop
    /// does and checks both halves: the hyperlink is there around the right text, and
    /// adding it changed nothing a person could see.
    #[test]
    fn a_web_link_leaves_cst_as_a_real_hyperlink_without_changing_the_picture() {
        let url = "https://example.com/gallery?a=1&b=2";
        let (mut app, _, _) = app_showing_a_link("hyperlink-frame", url);

        let with = frame_bytes(&mut app, true);
        let text = String::from_utf8_lossy(&with);
        let open = text
            .find(&format!("\x1b]8;;{url}\x1b\\"))
            .unwrap_or_else(|| panic!("no hyperlink in the frame: {text:?}"));
        let close = open + text[open..].find("\x1b]8;;\x1b\\").expect("closed");
        assert!(
            text[open..close].contains("Open"),
            "the hyperlink wraps the link's own text, got {:?}",
            &text[open..close]
        );

        let without = frame_bytes(&mut app, false);
        assert_eq!(
            screen_after(&with),
            screen_after(&without),
            "reprinting the link must land exactly on itself — text and colours both"
        );
        let _ = app.mux.as_mut().unwrap().shutdown();
    }

    #[test]
    fn a_file_link_is_never_handed_to_the_terminal() {
        // A terminal opens a file link with whatever the file type launches, which for an
        // .exe means running it. CST keeps those to itself.
        let (mut app, _, _) =
            app_showing_a_link("file-frame", "file:///C:/Windows/System32/calc.exe");
        let text = String::from_utf8_lossy(&frame_bytes(&mut app, true)).into_owned();
        assert!(!text.contains("\x1b]8;"), "got {text:?}");
        let _ = app.mux.as_mut().unwrap().shutdown();
    }

    /// The terminal already opened a web link, so CST must not open it too.
    ///
    /// Windows Terminal opens the hyperlink *and* forwards the same Ctrl+click to the
    /// program underneath. Opening it here as well is how other tools ended up giving
    /// every click two browser tabs.
    #[test]
    fn ctrl_clicking_a_web_link_leaves_the_opening_to_the_terminal() {
        let (mut app, x, y) = app_showing_a_link("web-click", "https://example.com/report");

        handle_attached_event(&mut app, mouse_down(x + 10, y, KeyModifiers::CONTROL));

        assert!(
            app.opened_links.is_empty(),
            "the terminal opened it already"
        );
        assert_eq!(app.status_message, None, "and there is nothing to report");
        let _ = app.mux.as_mut().unwrap().shutdown();
    }

    #[test]
    fn over_ssh_a_file_link_says_where_the_file_is_instead_of_opening_it() {
        // Opened here it would appear on the remote host's desktop, where nobody is looking.
        let (mut app, x, y) = app_showing_a_link("ssh-file", "file:///D:/gallery/index.html");
        app.over_ssh = true;

        handle_attached_event(&mut app, mouse_down(x + 10, y, KeyModifiers::CONTROL));

        assert!(app.opened_links.is_empty());
        assert!(
            app.status_message
                .as_deref()
                .is_some_and(|message| message.contains("machine CST is running on")),
            "got {:?}",
            app.status_message
        );
        let _ = app.mux.as_mut().unwrap().shutdown();
    }

    /// A link to a handler that acts on a click is shown, and refused when clicked.
    #[test]
    fn a_link_to_anything_but_the_web_or_a_file_is_refused() {
        let (mut app, x, y) = app_showing_a_link("refused", "ms-settings:privacy");

        handle_attached_event(&mut app, mouse_down(x + 10, y, KeyModifiers::CONTROL));

        assert!(app.opened_links.is_empty());
        assert!(
            app.status_message
                .as_deref()
                .is_some_and(|message| message.starts_with("Not opened")),
            "the user is told why nothing happened, got: {:?}",
            app.status_message
        );
        let _ = app.mux.as_mut().unwrap().shutdown();
    }

    fn send_prefix_command(app: &mut App, command: char) {
        handle_attached_key(
            app,
            KeyEvent::new(KeyCode::Char('b'), KeyModifiers::CONTROL),
        );
        handle_attached_key(
            app,
            KeyEvent::new(KeyCode::Char(command), KeyModifiers::NONE),
        );
    }

    #[test]
    fn prefix_key_is_swallowed_and_arms_the_state_machine() {
        let mut app = mux_app();
        let prefix = KeyEvent::new(KeyCode::Char('b'), KeyModifiers::CONTROL);

        handle_attached_key(&mut app, prefix);

        assert_eq!(app.mux.as_ref().unwrap().prefix_state, PrefixState::Root);
    }

    #[test]
    fn prefix_then_d_detaches_to_the_list() {
        let mut app = mux_app();
        app.view = View::Attached(1);
        handle_attached_key(
            &mut app,
            KeyEvent::new(KeyCode::Char('b'), KeyModifiers::CONTROL),
        );

        handle_attached_key(
            &mut app,
            KeyEvent::new(KeyCode::Char('d'), KeyModifiers::NONE),
        );

        assert_eq!(app.view, View::List);
        assert_eq!(app.mux.as_ref().unwrap().prefix_state, PrefixState::Idle);
    }

    #[test]
    fn prefix_q_confirms_before_ending_a_lone_active_session() {
        let mut app = attached_mux_app("quit-lone-session");

        send_prefix_command(&mut app, 'q');

        assert!(!app.should_quit);
        assert!(app.confirm_quit);
    }

    #[test]
    fn prefix_q_dismisses_an_exited_session_without_confirming() {
        let mut app = attached_mux_app("quit-exited-session");
        app.mux
            .as_mut()
            .unwrap()
            .focused_pane_mut()
            .unwrap()
            .mark_exited(Some(0));

        send_prefix_command(&mut app, 'q');

        assert!(app.should_quit);
        assert!(!app.confirm_quit);
    }

    #[test]
    fn prefix_q_confirms_when_other_sessions_would_die_unseen() {
        let mut app = attached_mux_app("quit-visible-session");
        // Pushing focuses the new pane, so the first one becomes the background
        // session the user cannot see.
        push_test_pane(&mut app, 2, "quit-background-session");
        app.view = View::Attached(2);

        send_prefix_command(&mut app, 'q');

        assert!(app.confirm_quit);
        assert!(!app.should_quit);
    }

    #[test]
    fn confirming_the_quit_prompt_from_a_pane_quits() {
        let mut app = mux_app();
        app.view = View::Attached(1);
        app.confirm_quit = true;

        handle_attached_event(
            &mut app,
            Event::Key(KeyEvent::new(KeyCode::Char('y'), KeyModifiers::NONE)),
        );

        assert!(app.should_quit);
        assert!(!app.confirm_quit);
    }

    #[test]
    fn declining_the_quit_prompt_returns_to_the_pane_without_a_stale_notice() {
        let mut app = mux_app();
        app.view = View::Attached(1);
        app.confirm_quit = true;

        handle_attached_event(
            &mut app,
            Event::Key(KeyEvent::new(KeyCode::Char('n'), KeyModifiers::NONE)),
        );

        assert!(!app.should_quit);
        assert!(!app.confirm_quit);
        assert_eq!(app.view, View::Attached(1));
        assert_eq!(app.status_message, None);
    }

    #[test]
    fn the_quit_prompt_swallows_keys_instead_of_forwarding_them_to_the_child() {
        let mut app = mux_app();
        app.view = View::Attached(1);
        app.confirm_quit = true;

        handle_attached_event(
            &mut app,
            Event::Key(KeyEvent::new(KeyCode::Char('b'), KeyModifiers::CONTROL)),
        );

        assert_eq!(app.mux.as_ref().unwrap().prefix_state, PrefixState::Idle);
        assert!(!app.confirm_quit);
        assert!(!app.should_quit);
    }

    #[test]
    fn github_namespace_opens_the_number_prompt_while_attached() {
        let mut app = attached_mux_app("github-inspector-test");

        for key in [
            KeyEvent::new(KeyCode::Char('b'), KeyModifiers::CONTROL),
            KeyEvent::new(KeyCode::Char('g'), KeyModifiers::CONTROL),
            KeyEvent::new(KeyCode::Char('i'), KeyModifiers::NONE),
        ] {
            handle_attached_key(&mut app, key);
        }

        assert!(matches!(
            app.github_inspector
                .as_ref()
                .map(|inspector| &inspector.screen),
            Some(crate::app::GithubInspectorScreen::NumberPrompt)
        ));
        let _ = app.mux.as_mut().unwrap().shutdown();
    }

    #[test]
    fn github_inspection_from_the_list_explains_that_attachment_is_required() {
        let mut app = mux_app();
        app.view = View::List;

        for key in [
            KeyEvent::new(KeyCode::Char('b'), KeyModifiers::CONTROL),
            KeyEvent::new(KeyCode::Char('g'), KeyModifiers::CONTROL),
            KeyEvent::new(KeyCode::Char('i'), KeyModifiers::NONE),
        ] {
            assert!(handle_list_prefix(&mut app, key));
        }

        assert!(app.github_inspector.is_none());
        assert_eq!(
            app.status_message.as_deref(),
            Some("Attach to a session before inspecting a GitHub item")
        );
    }

    #[test]
    fn waiting_thread_messages_open_from_a_key_on_the_list_and_inside_a_session() {
        // Command search was the only way in, which meant knowing the inbox existed and
        // what it was called. It concerns every session, so it works from both places.
        let mut app = mux_app();
        app.view = View::List;
        for key in [
            KeyEvent::new(KeyCode::Char('b'), KeyModifiers::CONTROL),
            KeyEvent::new(KeyCode::Char('g'), KeyModifiers::NONE),
            KeyEvent::new(KeyCode::Char('m'), KeyModifiers::NONE),
        ] {
            assert!(handle_list_prefix(&mut app, key));
        }
        assert_eq!(app.thread_inbox, Some(0));

        let mut app = attached_mux_app("thread-inbox-key-test");
        for key in [
            KeyEvent::new(KeyCode::Char('b'), KeyModifiers::CONTROL),
            KeyEvent::new(KeyCode::Char('g'), KeyModifiers::NONE),
            KeyEvent::new(KeyCode::Char('m'), KeyModifiers::NONE),
        ] {
            handle_attached_key(&mut app, key);
        }
        assert_eq!(app.thread_inbox, Some(0));
        let _ = app.mux.as_mut().unwrap().shutdown();
    }

    #[test]
    fn github_item_prompt_accepts_discussion_selectors_and_backspace() {
        let mut app = attached_mux_app("github-number-test");
        app.open_github_inspector();

        for key in [
            KeyEvent::new(KeyCode::Char('d'), KeyModifiers::NONE),
            KeyEvent::new(KeyCode::Char(' '), KeyModifiers::NONE),
            KeyEvent::new(KeyCode::Char('1'), KeyModifiers::NONE),
            KeyEvent::new(KeyCode::Char('2'), KeyModifiers::NONE),
            KeyEvent::new(KeyCode::Backspace, KeyModifiers::NONE),
        ] {
            handle_github_inspector_event(&mut app, Event::Key(key));
        }

        assert_eq!(
            app.github_inspector
                .as_ref()
                .map(|inspector| inspector.input.as_str()),
            Some("d 1")
        );
        handle_github_inspector_event(
            &mut app,
            Event::Key(KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE)),
        );
        assert!(app.github_inspector.is_none());
        let _ = app.mux.as_mut().unwrap().shutdown();
    }

    #[test]
    fn github_inspector_opens_with_the_configured_comment_filter() {
        let mut app = attached_mux_app("github-filter-default");
        app.config.github_comment_filter = crate::config::GithubCommentFilter::Resolved;

        app.open_github_inspector();

        assert_eq!(
            app.github_inspector.as_ref().unwrap().comment_filter,
            crate::config::GithubCommentFilter::Resolved
        );
        let _ = app.mux.as_mut().unwrap().shutdown();
    }

    /// A pull request whose tree is `src/{ui/pane.rs, lib.rs}`, so there is a
    /// nested directory to fold and two files to move between.
    fn pull_request_app() -> App {
        use crate::github::{Author, ChangedFile, ItemCommon, Label, PullRequest, RepositoryRef};

        let changed = |path: &str| ChangedFile {
            path: path.to_string(),
            status: "modified".to_string(),
            additions: 1,
            deletions: 1,
            changes: 2,
            patch: Some(format!("@@ -1 +1 @@\n-old {path}\n+new {path}")),
        };
        let item = crate::github::GithubItem::PullRequest(PullRequest {
            common: ItemCommon {
                repository: RepositoryRef {
                    host: "github.com".to_string(),
                    owner: "octo".to_string(),
                    name: "widgets".to_string(),
                },
                number: 7,
                title: "Tree navigation".to_string(),
                state: "open".to_string(),
                author: Author {
                    login: "monalisa".to_string(),
                },
                labels: Vec::<Label>::new(),
                created_at: "2026-01-01T00:00:00Z".to_string(),
                updated_at: "2026-01-02T00:00:00Z".to_string(),
                url: "https://github.com/octo/widgets/pull/7".to_string(),
                body: String::new(),
            },
            draft: false,
            merged: false,
            mergeable_state: None,
            base_ref: "main".to_string(),
            head_ref: "feature".to_string(),
            additions: 2,
            deletions: 2,
            changed_files: 2,
            discussion: Vec::new(),
            files: vec![changed("src/ui/pane.rs"), changed("src/lib.rs")],
            patches_loaded: true,
        });

        let mut app = App::new(Vec::new(), UserConfig::default());
        let mut inspector = crate::app::GithubInspector::number_prompt();
        inspector.screen = crate::app::GithubInspectorScreen::Ready(item);
        inspector.tab = crate::app::GithubTab::Files;
        inspector.select_first_tree_file();
        app.github_inspector = Some(inspector);
        app
    }

    fn press(app: &mut App, code: KeyCode) {
        handle_github_inspector_event(app, Event::Key(KeyEvent::new(code, KeyModifiers::NONE)));
    }

    fn tree_labels(app: &App) -> Vec<String> {
        github_tree_rows(app)
            .iter()
            .map(|row| row.label.clone())
            .collect()
    }

    #[test]
    fn the_files_tree_opens_on_the_first_file_not_a_directory() {
        let app = pull_request_app();
        let inspector = app.github_inspector.as_ref().unwrap();

        // rows: [src, ui, pane.rs, lib.rs] — the cursor skips to `pane.rs`.
        assert_eq!(tree_labels(&app), vec!["src", "ui", "pane.rs", "lib.rs"]);
        assert_eq!(inspector.tree_selected, 2);
        assert_eq!(inspector.selected_file, 0);
    }

    #[test]
    fn moving_through_the_tree_updates_the_diff_without_pressing_enter() {
        let mut app = pull_request_app();

        press(&mut app, KeyCode::Down);

        let inspector = app.github_inspector.as_ref().unwrap();
        assert_eq!(inspector.tree_selected, 3);
        assert_eq!(inspector.selected_file, 1, "the diff follows the cursor");
        assert_eq!(inspector.files_pane, crate::app::FilesPane::Tree);
    }

    #[test]
    fn r_requests_a_copilot_review_from_an_open_pull_request() {
        let mut app = pull_request_app();

        press(&mut app, KeyCode::Char('r'));

        assert_eq!(
            app.github_inspector.as_ref().unwrap().copilot_review_status,
            crate::app::CopilotReviewStatus::Failed(
                "The inspected pull request has no working directory".to_string()
            )
        );
    }

    #[test]
    fn f_cycles_the_pull_request_comment_filter() {
        let mut app = pull_request_app();
        app.github_inspector.as_mut().unwrap().tab = crate::app::GithubTab::Comments;

        press(&mut app, KeyCode::Char('f'));

        assert_eq!(
            app.github_inspector.as_ref().unwrap().comment_filter,
            crate::app::GithubCommentFilter::Unresolved
        );
    }

    #[test]
    fn left_folds_a_directory_and_right_unfolds_it() {
        let mut app = pull_request_app();

        // From `pane.rs`, Left steps out to `ui`, then Left folds it.
        press(&mut app, KeyCode::Left);
        assert_eq!(app.github_inspector.as_ref().unwrap().tree_selected, 1);
        press(&mut app, KeyCode::Left);
        assert_eq!(tree_labels(&app), vec!["src", "ui", "lib.rs"]);

        press(&mut app, KeyCode::Right);
        assert_eq!(tree_labels(&app), vec!["src", "ui", "pane.rs", "lib.rs"]);
    }

    #[test]
    fn enter_moves_focus_to_the_diff_and_esc_brings_it_back() {
        let mut app = pull_request_app();

        press(&mut app, KeyCode::Enter);
        assert_eq!(
            app.github_inspector.as_ref().unwrap().files_pane,
            crate::app::FilesPane::Diff
        );

        // Arrows now scroll the patch instead of moving the tree cursor.
        press(&mut app, KeyCode::Down);
        let inspector = app.github_inspector.as_ref().unwrap();
        assert_eq!(inspector.tree_selected, 2, "the tree cursor stays put");

        press(&mut app, KeyCode::Esc);
        assert_eq!(
            app.github_inspector.as_ref().unwrap().files_pane,
            crate::app::FilesPane::Tree,
            "Esc returns to the tree before closing the inspector"
        );

        press(&mut app, KeyCode::Esc);
        assert!(app.github_inspector.is_none());
    }

    #[test]
    fn q_leaves_the_inspector_from_anywhere_in_it() {
        let mut app = pull_request_app();

        press(&mut app, KeyCode::Char('q'));
        assert!(app.github_inspector.is_none(), "q closes from a tab");

        // From inside a diff, where Esc only steps back to the tree.
        let mut app = pull_request_app();
        press(&mut app, KeyCode::Enter);
        assert_eq!(
            app.github_inspector.as_ref().unwrap().files_pane,
            crate::app::FilesPane::Diff
        );

        press(&mut app, KeyCode::Char('q'));
        assert!(
            app.github_inspector.is_none(),
            "q closes outright rather than returning to the tree"
        );
    }

    #[test]
    fn q_closes_the_loading_and_error_screens() {
        let mut app = pull_request_app();
        app.github_inspector.as_mut().unwrap().screen = crate::app::GithubInspectorScreen::Loading;
        press(&mut app, KeyCode::Char('q'));
        assert!(app.github_inspector.is_none());

        let mut app = pull_request_app();
        app.github_inspector.as_mut().unwrap().screen =
            crate::app::GithubInspectorScreen::Error("nope".to_string());
        press(&mut app, KeyCode::Char('q'));
        assert!(app.github_inspector.is_none());
    }

    #[test]
    fn enter_on_a_directory_folds_it_instead_of_focusing_the_diff() {
        let mut app = pull_request_app();
        app.github_inspector.as_mut().unwrap().tree_selected = 0;

        press(&mut app, KeyCode::Enter);

        assert_eq!(tree_labels(&app), vec!["src"]);
        assert_eq!(
            app.github_inspector.as_ref().unwrap().files_pane,
            crate::app::FilesPane::Tree
        );
    }

    #[test]
    fn the_wheel_scrolls_whichever_pane_it_is_over() {
        let mut app = pull_request_app();
        {
            let inspector = app.github_inspector.as_mut().unwrap();
            inspector.tree_area = Rect::new(0, 4, 30, 10);
            inspector.diff_area = Rect::new(31, 4, 60, 10);
            inspector.visible_tree_rows = 2;
            // Normally set while drawing; the wheel clamps against it.
            inspector.max_diff_scroll = 10;
        }

        // Over the diff: the patch scrolls and the tree cursor is untouched.
        scroll_github_at(&mut app, 40, 6, 3);
        let inspector = app.github_inspector.as_ref().unwrap();
        assert_eq!(inspector.diff_scroll, 3);
        assert_eq!(inspector.tree_selected, 2);

        // Over the tree: the view scrolls without dragging the selection.
        scroll_github_at(&mut app, 5, 6, 3);
        let inspector = app.github_inspector.as_ref().unwrap();
        assert_eq!(inspector.tree_offset, 2, "clamped to the last full page");
        assert_eq!(inspector.tree_selected, 2);
    }

    #[test]
    fn clicking_a_tree_row_selects_it_and_focuses_the_tree() {
        let mut app = pull_request_app();
        {
            let inspector = app.github_inspector.as_mut().unwrap();
            inspector.tree_area = Rect::new(0, 4, 30, 10);
            inspector.diff_area = Rect::new(31, 4, 60, 10);
            inspector.files_pane = crate::app::FilesPane::Diff;
        }

        click_github_inspector(&mut app, 5, 7);

        let inspector = app.github_inspector.as_ref().unwrap();
        assert_eq!(inspector.files_pane, crate::app::FilesPane::Tree);
        assert_eq!(inspector.tree_selected, 3, "row 3 of the tree");
        assert_eq!(inspector.selected_file, 1);
    }

    #[test]
    fn the_prefix_is_recognised_from_the_session_list() {
        let mut app = mux_app();
        app.view = View::List;

        let consumed = handle_list_prefix(
            &mut app,
            KeyEvent::new(KeyCode::Char('b'), KeyModifiers::CONTROL),
        );

        assert!(
            consumed,
            "the prefix must not fall through to list bindings"
        );
        assert_eq!(app.mux.as_ref().unwrap().prefix_state, PrefixState::Root);
    }

    #[test]
    fn double_prefix_opens_command_search_from_list_without_panes() {
        let mut app = mux_app();
        app.view = View::List;
        let prefix = KeyEvent::new(KeyCode::Char('b'), KeyModifiers::CONTROL);

        assert!(handle_list_prefix(&mut app, prefix));
        assert!(handle_list_prefix(&mut app, prefix));

        assert!(app.command_palette.is_some());
        assert_eq!(app.mux.as_ref().unwrap().prefix_state, PrefixState::Idle);
    }

    #[test]
    fn double_prefix_opens_command_search_without_sending_to_copilot() {
        let mut app = attached_mux_app("command-search");
        let prefix = KeyEvent::new(KeyCode::Char('b'), KeyModifiers::CONTROL);

        handle_attached_key(&mut app, prefix);
        handle_attached_key(&mut app, prefix);

        assert!(app.command_palette.is_some());
        assert_eq!(app.mux.as_ref().unwrap().prefix_state, PrefixState::Idle);
        let _ = app.mux.as_mut().unwrap().shutdown();
    }

    #[test]
    fn command_search_fuzzy_executes_list_commands() {
        let mut app = mux_app();
        app.view = View::List;
        app.open_command_palette();
        for character in "global settings".chars() {
            handle_command_palette_event(
                &mut app,
                Event::Key(KeyEvent::new(KeyCode::Char(character), KeyModifiers::NONE)),
            );
        }

        handle_command_palette_event(
            &mut app,
            Event::Key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE)),
        );

        assert!(app.command_palette.is_none());
        assert_eq!(app.mode, crate::app::Mode::Settings);
    }

    #[test]
    fn tmux_palette_commands_grey_out_with_a_reason_where_tmux_cannot_work() {
        let mut app = attached_mux_app("tmux-greyed");
        app.tmux_support =
            crate::session::tmux::TmuxSupport::Unavailable("no tmux here".to_string());

        let commands = crate::command_palette::filtered_commands(&app);

        for id in [
            CommandId::NewTmuxSession,
            CommandId::NewTmuxWorktreeSession,
            CommandId::ResumeSelectedInTmux,
            CommandId::EndPersistentSession,
        ] {
            let command = commands
                .iter()
                .find(|command| command.id == id)
                .unwrap_or_else(|| panic!("{id:?} should stay listed so it is discoverable"));
            assert!(!command.enabled, "{id:?} must be greyed out");
        }
        // The greyed entries explain themselves instead of silently failing.
        assert!(commands
            .iter()
            .filter(|command| command.id == CommandId::NewTmuxSession)
            .all(|command| command
                .unavailable_reason
                .is_some_and(|r| r.contains("tmux"))));
    }

    #[test]
    fn portable_commands_are_enabled_while_attached() {
        let mut app = attached_mux_app("portable-commands");
        app.config.hidden_title_prefixes = vec!["Your objective:".to_string()];
        app.tmux_support = crate::session::tmux::TmuxSupport::Available;
        let commands = crate::command_palette::filtered_commands(&app);

        for id in [
            CommandId::NewSession,
            CommandId::NewTmuxSession,
            CommandId::NewWorktreeSession,
            CommandId::NewTmuxWorktreeSession,
            CommandId::OpenFavoriteTabs,
            CommandId::SearchSessions,
            CommandId::FilterProject,
            CommandId::CycleSort,
            CommandId::ToggleHiddenSessions,
            CommandId::GlobalSettings,
            CommandId::ProjectSettings,
            CommandId::OpenHelp,
        ] {
            assert!(
                commands
                    .iter()
                    .find(|command| command.id == id)
                    .is_some_and(|command| command.enabled),
                "{id:?} should be available while attached"
            );
        }

        for id in [
            CommandId::ResumeSelected,
            CommandId::OpenSelectedScratchpad,
            CommandId::ToggleFavorite,
            CommandId::RenameSelected,
            CommandId::DeleteSelected,
        ] {
            assert!(
                commands
                    .iter()
                    .find(|command| command.id == id)
                    .is_some_and(|command| !command.enabled),
                "{id:?} requires a visible selected list row"
            );
        }
        let _ = app.mux.as_mut().unwrap().shutdown();
    }

    #[test]
    fn command_search_and_theme_settings_work_over_github_inspector() {
        let mut app = attached_mux_app("github-settings");
        app.github_inspector = Some(crate::app::GithubInspector::number_prompt());
        let prefix = Event::Key(KeyEvent::new(KeyCode::Char('b'), KeyModifiers::CONTROL));

        handle_attached_event(&mut app, prefix.clone());
        handle_attached_event(&mut app, prefix);
        assert!(app.command_palette.is_some());
        app.command_palette.as_mut().unwrap().query = "global settings".to_string();

        handle_attached_event(
            &mut app,
            Event::Key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE)),
        );
        assert_eq!(app.mode, crate::app::Mode::Settings);
        assert!(app.github_inspector.is_some());

        app.settings_selected = 3;
        handle_attached_event(
            &mut app,
            Event::Key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE)),
        );
        assert!(app.theme_picker.is_some());
        handle_attached_event(
            &mut app,
            Event::Key(KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE)),
        );
        handle_attached_event(
            &mut app,
            Event::Key(KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE)),
        );

        assert_eq!(app.mode, crate::app::Mode::Normal);
        assert!(app.github_inspector.is_some());
        assert_eq!(app.view, View::Attached(1));
        let _ = app.mux.as_mut().unwrap().shutdown();
    }

    #[test]
    fn raw_prefix_navigation_closes_the_previous_panes_inspector() {
        let mut app = attached_mux_app("first-inspector");
        push_test_pane(&mut app, 2, "second-pane");
        app.mux.as_mut().unwrap().focused = Some(1);
        app.github_inspector = Some(crate::app::GithubInspector::number_prompt());
        let prefix = KeyEvent::new(KeyCode::Char('b'), KeyModifiers::CONTROL);

        handle_attached_event(&mut app, Event::Key(prefix));
        handle_attached_event(
            &mut app,
            Event::Key(KeyEvent::new(KeyCode::Char('n'), KeyModifiers::NONE)),
        );

        assert_eq!(app.mux.as_ref().unwrap().focused, Some(2));
        assert_eq!(app.view, View::Attached(2));
        assert!(app.github_inspector.is_none());
        let _ = app.mux.as_mut().unwrap().shutdown();
    }

    #[test]
    fn workspace_modals_receive_input_above_github_inspector() {
        let mut app = attached_mux_app("inspector-overlays");
        app.github_inspector = Some(crate::app::GithubInspector::number_prompt());

        execute_palette_command(&mut app, CommandId::OpenScratchpadHelp);
        handle_attached_event(
            &mut app,
            Event::Key(KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE)),
        );
        assert!(app.workspace_help.is_none());
        assert!(app.github_inspector.is_some());

        execute_palette_command(&mut app, CommandId::OpenSnippets);
        assert!(app.snippet_modal.is_some());
        handle_attached_event(
            &mut app,
            Event::Key(KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE)),
        );
        assert!(app.snippet_modal.is_none());
        assert!(app.github_inspector.is_some());
        let _ = app.mux.as_mut().unwrap().shutdown();
    }

    #[test]
    fn list_navigation_commands_detach_without_stopping_the_pane() {
        let mut app = attached_mux_app("search-from-chat");

        execute_palette_command(&mut app, CommandId::SearchSessions);

        assert_eq!(app.view, View::List);
        assert_eq!(app.mode, crate::app::Mode::Search);
        assert!(app.mux.as_ref().unwrap().pane(1).unwrap().is_running());
        let _ = app.mux.as_mut().unwrap().shutdown();
    }

    #[test]
    fn attached_commands_use_the_focused_pane_project_context() {
        let mut app = attached_mux_app("first");
        let focused = std::env::temp_dir().join("focused-command-project");
        app.mux.as_mut().unwrap().pane_mut(1).unwrap().cwd = focused.clone();
        app.project_filter = Some("hidden-list-project".to_string());

        assert_eq!(
            app.command_project(),
            Some(focused.to_string_lossy().to_string())
        );
        let _ = app.mux.as_mut().unwrap().shutdown();
    }

    #[test]
    fn disabled_command_stays_open_and_explains_why() {
        let mut app = mux_app();
        app.view = View::List;
        app.open_command_palette();
        app.command_palette.as_mut().unwrap().query = "focus chat".to_string();

        handle_command_palette_event(
            &mut app,
            Event::Key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE)),
        );

        assert!(app.command_palette.is_some());
        assert!(app
            .command_palette
            .as_ref()
            .unwrap()
            .error
            .as_deref()
            .unwrap()
            .contains("attached running session"));
    }

    #[test]
    fn clicking_a_palette_result_executes_it() {
        let mut app = mux_app();
        app.view = View::List;
        app.open_command_palette();
        app.command_palette
            .as_mut()
            .unwrap()
            .hits
            .push((Rect::new(10, 10, 30, 1), CommandId::GlobalSettings));

        handle_command_palette_event(
            &mut app,
            Event::Mouse(crossterm::event::MouseEvent {
                kind: MouseEventKind::Down(MouseButton::Left),
                column: 12,
                row: 10,
                modifiers: KeyModifiers::NONE,
            }),
        );

        assert!(app.command_palette.is_none());
        assert_eq!(app.mode, crate::app::Mode::Settings);
    }

    #[test]
    fn next_session_from_list_attaches_the_newly_focused_pane() {
        let mut app = attached_mux_app("session-one");
        push_test_pane(&mut app, 2, "session-two");
        app.mux.as_mut().unwrap().focused = Some(1);
        app.view = View::List;
        app.open_command_palette();
        app.command_palette.as_mut().unwrap().query = "next session".to_string();

        handle_command_palette_event(
            &mut app,
            Event::Key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE)),
        );

        assert_eq!(app.mux.as_ref().unwrap().focused, Some(2));
        assert_eq!(app.view, View::Attached(2));
        let _ = app.mux.as_mut().unwrap().shutdown();
    }

    #[test]
    fn ordinary_list_keys_are_left_alone_when_no_prefix_is_pending() {
        let mut app = mux_app();
        app.view = View::List;

        let consumed = handle_list_prefix(
            &mut app,
            KeyEvent::new(KeyCode::Char('n'), KeyModifiers::NONE),
        );

        assert!(
            !consumed,
            "'n' must still create a new session from the list"
        );
    }

    #[test]
    fn a_prefix_command_from_the_list_reports_when_nothing_is_running() {
        let mut app = mux_app();
        app.view = View::List;
        handle_list_prefix(
            &mut app,
            KeyEvent::new(KeyCode::Char('b'), KeyModifiers::CONTROL),
        );

        let consumed = handle_list_prefix(
            &mut app,
            KeyEvent::new(KeyCode::Char('w'), KeyModifiers::NONE),
        );

        assert!(consumed);
        assert_eq!(app.mux.as_ref().unwrap().prefix_state, PrefixState::Idle);
        assert_eq!(app.mode, crate::app::Mode::Normal, "no panes, no switcher");
        assert!(app.status_message.is_some());
    }

    #[test]
    fn per_session_state_is_keyed_on_the_session_not_the_pane_number() {
        // Pane numbering restarts at 1 every run, so deriving a scratchpad or
        // terminal key from it made two unrelated new sessions share one file.
        let first_run = attached_mux_app("session-alpha");
        let (first_pane, alpha, _, _) = focused_workspace_context(&first_run).unwrap();

        let second_run = attached_mux_app("session-beta");
        let (second_pane, beta, _, _) = focused_workspace_context(&second_run).unwrap();

        assert_eq!(first_pane, second_pane, "the same pane number, as in life");
        assert_eq!(alpha, "session-alpha");
        assert_eq!(beta, "session-beta");
        assert_ne!(alpha, beta, "but state must not be shared between them");
    }

    fn snippet_key(app: &mut App, code: KeyCode) {
        handle_snippet_event(app, Event::Key(KeyEvent::new(code, KeyModifiers::NONE)));
    }

    #[test]
    fn prefix_s_opens_snippets_and_new_snippets_default_to_global() {
        let mut app = attached_mux_app("snippet-session");
        app.disable_config_persistence();

        send_prefix_command(&mut app, 's');
        assert!(app.snippet_modal.is_some());

        snippet_key(&mut app, KeyCode::Char('a'));
        {
            let modal = app.snippet_modal.as_mut().unwrap();
            assert_eq!(modal.editor_scope, SnippetScope::Global);
            modal.editor_name.set_text("Review");
            modal.editor_prompt.set_text("Review this carefully.");
        }

        handle_snippet_event(
            &mut app,
            Event::Key(KeyEvent::new(KeyCode::Char('s'), KeyModifiers::CONTROL)),
        );

        assert_eq!(app.config.snippets.len(), 1);
        assert_eq!(app.config.snippets[0].name, "Review");
        assert_eq!(
            app.snippet_modal.as_ref().unwrap().screen,
            SnippetScreen::List
        );
    }

    #[test]
    fn prefix_u_confirms_before_restarting_an_attached_pane() {
        let mut app = attached_mux_app("update-session");
        app.update_info = Some(crate::updater::UpdateInfo {
            current_version: "0.18.0".to_string(),
            latest_version: "0.19.0".to_string(),
        });

        send_prefix_command(&mut app, 'u');

        assert!(app.confirm_update_restart);
        assert!(app.update_install_requested_for.is_none());
        assert!(matches!(app.view, View::Attached(1)));
        assert!(app.mux.as_ref().unwrap().pane(1).unwrap().is_running());
        assert!(!app.should_quit);

        handle_attached_event(
            &mut app,
            Event::Key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE)),
        );
        assert!(!app.confirm_update_restart);
        assert_eq!(app.update_install_requested_for.as_deref(), Some("0.19.0"));
        let restart = app.restart_after_update.as_ref().unwrap();
        assert_eq!(
            restart
                .panes
                .iter()
                .map(|pane| pane.session_id.as_str())
                .collect::<Vec<_>>(),
            ["update-session"]
        );
        assert_eq!(
            restart.focused_session_id.as_deref(),
            Some("update-session")
        );
        assert!(app.mux.as_ref().unwrap().pane(1).unwrap().is_running());
        let _ = app.mux.as_mut().unwrap().shutdown();
    }

    #[test]
    fn attached_update_confirmation_can_be_cancelled_without_installing() {
        let mut app = attached_mux_app("update-session");
        app.update_info = Some(crate::updater::UpdateInfo {
            current_version: "0.18.0".to_string(),
            latest_version: "0.19.0".to_string(),
        });
        send_prefix_command(&mut app, 'u');

        handle_attached_event(
            &mut app,
            Event::Key(KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE)),
        );

        assert!(!app.confirm_update_restart);
        assert!(app.update_install_requested_for.is_none());
        assert!(app.restart_after_update.is_none());
        assert!(app.mux.as_ref().unwrap().pane(1).unwrap().is_running());
        let _ = app.mux.as_mut().unwrap().shutdown();
    }

    #[test]
    fn update_restart_preserves_pane_order_and_focus() {
        let mut app = attached_mux_app("session-one");
        push_test_pane(&mut app, 2, "session-two");
        push_test_pane(&mut app, 3, "session-three");
        app.mux.as_mut().unwrap().focused = Some(2);
        app.update_info = Some(crate::updater::UpdateInfo {
            current_version: "0.18.0".to_string(),
            latest_version: "0.19.0".to_string(),
        });

        app.request_update();
        app.confirm_update_and_restart();

        let restart = app.restart_after_update.as_ref().unwrap();
        assert_eq!(
            restart
                .panes
                .iter()
                .map(|pane| (pane.pane_id, pane.session_id.as_str()))
                .collect::<Vec<_>>(),
            [
                (Some(1), "session-one"),
                (Some(2), "session-two"),
                (Some(3), "session-three"),
            ]
        );
        assert_eq!(restart.focused_session_id.as_deref(), Some("session-two"));
        let _ = app.mux.as_mut().unwrap().shutdown();
    }

    #[test]
    fn sessions_started_during_install_require_fresh_restart_confirmation() {
        let mut app = attached_mux_app("session-one");
        app.update_info = Some(crate::updater::UpdateInfo {
            current_version: "0.18.0".to_string(),
            latest_version: "0.19.0".to_string(),
        });
        app.request_update();
        app.confirm_update_and_restart();

        push_test_pane(&mut app, 2, "session-two");
        let (sender, receiver) = std::sync::mpsc::channel();
        app.update_install_receiver = Some(receiver);
        sender
            .send(Ok(crate::updater::UpdateInstallOutcome::Installed(
                "0.19.0".to_string(),
            )))
            .unwrap();
        app.poll_update();

        assert!(app.update_restart_ready());
        assert!(!app.validate_update_restart_sessions());
        assert!(app.confirm_update_restart);
        assert!(app.restart_after_update.is_none());

        app.confirm_update_and_restart();
        assert!(app.update_restart_ready());
        assert_eq!(
            app.restart_after_update
                .as_ref()
                .unwrap()
                .panes
                .iter()
                .map(|pane| pane.session_id.as_str())
                .collect::<Vec<_>>(),
            ["session-one", "session-two"]
        );
        let _ = app.mux.as_mut().unwrap().shutdown();
    }

    #[test]
    fn replacement_pane_for_the_same_session_requires_fresh_confirmation() {
        let mut app = attached_mux_app("session-one");
        app.update_info = Some(crate::updater::UpdateInfo {
            current_version: "0.18.0".to_string(),
            latest_version: "0.19.0".to_string(),
        });
        app.request_update();
        app.confirm_update_and_restart();
        app.installed_update_version = Some("0.19.0".to_string());
        app.restart_after_update.as_mut().unwrap().panes[0].pane_id = Some(999);

        assert!(app.update_restart_ready());
        assert!(!app.validate_update_restart_sessions());
        assert!(app.confirm_update_restart);
        assert!(app.restart_after_update.is_none());
        let _ = app.mux.as_mut().unwrap().shutdown();
    }

    #[test]
    fn snippet_editor_navigation_follows_name_scope_prompt() {
        let mut app = attached_mux_app("snippet-navigation-session");
        app.open_snippets();
        snippet_key(&mut app, KeyCode::Char('a'));
        assert_eq!(
            app.snippet_modal.as_ref().unwrap().editor_field,
            SnippetEditorField::Name
        );

        snippet_key(&mut app, KeyCode::Tab);
        assert_eq!(
            app.snippet_modal.as_ref().unwrap().editor_field,
            SnippetEditorField::Scope
        );
        snippet_key(&mut app, KeyCode::Tab);
        assert_eq!(
            app.snippet_modal.as_ref().unwrap().editor_field,
            SnippetEditorField::Prompt
        );
        snippet_key(&mut app, KeyCode::BackTab);
        assert_eq!(
            app.snippet_modal.as_ref().unwrap().editor_field,
            SnippetEditorField::Scope
        );

        app.snippet_modal.as_mut().unwrap().editor_field = SnippetEditorField::Name;
        snippet_key(&mut app, KeyCode::Enter);
        assert_eq!(
            app.snippet_modal.as_ref().unwrap().editor_field,
            SnippetEditorField::Scope
        );
    }

    #[test]
    fn opening_snippets_loads_the_focused_sessions_project_collection() {
        let project = tempfile::tempdir().unwrap();
        std::fs::create_dir(project.path().join(".git")).unwrap();
        let project_snippet = crate::config::PromptSnippet {
            name: "Local".to_string(),
            prompt: "Only in this repository".to_string(),
        };
        let mut settings =
            crate::config::ProjectSettings::load(project.path(), &UserConfig::default()).unwrap();
        settings.set_snippets(vec![project_snippet.clone()]);
        settings.save().unwrap();

        let mut app = mux_app();
        push_test_pane_at(
            &mut app,
            1,
            "project-snippet-session",
            project.path().to_path_buf(),
        );
        app.view = View::Attached(1);

        send_prefix_command(&mut app, 's');

        let modal = app.snippet_modal.as_ref().unwrap();
        assert_eq!(modal.project, vec![project_snippet]);
        assert_eq!(
            modal.project_root.as_deref(),
            Some(project.path()),
            "scope follows the focused pane, not CST's launch directory"
        );
    }

    #[test]
    fn using_a_snippet_closes_the_modal_and_focuses_chat_without_submitting() {
        let mut app = attached_mux_app("snippet-use-session");
        app.config.snippets = vec![crate::config::PromptSnippet {
            name: "Plan".to_string(),
            prompt: "Make a plan first; do not execute it yet.".to_string(),
        }];
        app.workspace_focus = WorkspaceFocus::Scratchpad;
        app.open_snippets();

        snippet_key(&mut app, KeyCode::Enter);

        assert!(app.snippet_modal.is_none());
        assert_eq!(app.workspace_focus, WorkspaceFocus::Chat);
        // Pane::send_prompt_snippet owns paste-safety checks; the next test covers
        // multiline text before Copilot has enabled bracketed paste.
    }

    #[test]
    fn a_digit_uses_that_numbered_snippet_without_selecting_it_first() {
        let mut app = attached_mux_app("snippet-quick-use-session");
        app.config.snippets = (0..3)
            .map(|index| crate::config::PromptSnippet {
                name: format!("Snippet {index}"),
                prompt: format!("prompt {index}"),
            })
            .collect();
        app.open_snippets();
        assert_eq!(app.snippet_modal.as_ref().unwrap().selected, 0);

        snippet_key(&mut app, KeyCode::Char('3'));

        assert!(
            app.snippet_modal.is_none(),
            "the third snippet was pasted and the modal closed"
        );
    }

    #[test]
    fn a_digit_past_the_end_of_the_list_is_ignored() {
        let mut app = attached_mux_app("snippet-quick-use-empty-slot-session");
        app.config.snippets = vec![crate::config::PromptSnippet {
            name: "Only".to_string(),
            prompt: "only prompt".to_string(),
        }];
        app.open_snippets();

        snippet_key(&mut app, KeyCode::Char('7'));
        snippet_key(&mut app, KeyCode::Char('0'));

        assert!(
            app.snippet_modal.is_some(),
            "an unassigned digit must not close the modal"
        );
    }

    #[test]
    fn the_snippet_editor_answers_the_scratchpad_shortcuts() {
        let mut app = attached_mux_app("snippet-shared-editor-session");
        app.open_snippets();
        snippet_key(&mut app, KeyCode::Char('a'));
        snippet_key(&mut app, KeyCode::Tab);
        snippet_key(&mut app, KeyCode::Tab);
        for character in "review the diff".chars() {
            snippet_key(&mut app, KeyCode::Char(character));
        }
        let prompt = |app: &App| {
            app.snippet_modal
                .as_ref()
                .unwrap()
                .editor_prompt
                .text()
                .clone()
        };
        assert_eq!(
            app.snippet_modal.as_ref().unwrap().editor_field,
            SnippetEditorField::Prompt
        );

        handle_snippet_event(
            &mut app,
            Event::Key(KeyEvent::new(KeyCode::Char('w'), KeyModifiers::CONTROL)),
        );
        assert_eq!(prompt(&app), "review the ", "Ctrl+W deletes a word");

        handle_snippet_event(
            &mut app,
            Event::Key(KeyEvent::new(KeyCode::Char('z'), KeyModifiers::CONTROL)),
        );
        assert_eq!(prompt(&app), "review the diff", "Ctrl+Z undoes it");
    }

    #[test]
    fn shift_tab_walks_back_a_field_however_the_terminal_spells_it() {
        let mut app = attached_mux_app("snippet-shift-tab-session");
        app.open_snippets();
        snippet_key(&mut app, KeyCode::Char('a'));
        let field = |app: &App| app.snippet_modal.as_ref().unwrap().editor_field;

        handle_snippet_event(
            &mut app,
            Event::Key(KeyEvent::new(KeyCode::BackTab, KeyModifiers::NONE)),
        );
        assert_eq!(field(&app), SnippetEditorField::Prompt);

        handle_snippet_event(
            &mut app,
            Event::Key(KeyEvent::new(KeyCode::Tab, KeyModifiers::SHIFT)),
        );
        assert_eq!(field(&app), SnippetEditorField::Scope, "not a dedent");
    }

    #[test]
    fn a_pasted_escape_sequence_never_reaches_the_snippet_buffer() {
        let mut app = attached_mux_app("snippet-paste-control-session");
        app.open_snippets();
        snippet_key(&mut app, KeyCode::Char('a'));
        snippet_key(&mut app, KeyCode::Tab);
        snippet_key(&mut app, KeyCode::Tab);

        handle_snippet_event(&mut app, Event::Paste("safe\u{1b}[2Jtext".to_string()));

        assert_eq!(
            app.snippet_modal.as_ref().unwrap().editor_prompt.text(),
            "safe[2Jtext"
        );
    }

    #[test]
    fn ctrl_w_deletes_a_word_in_the_snippet_editor() {
        let mut app = attached_mux_app("snippet-ctrl-w-session");
        app.open_snippets();
        snippet_key(&mut app, KeyCode::Char('a'));
        for character in "review this".chars() {
            snippet_key(&mut app, KeyCode::Char(character));
        }

        handle_snippet_event(
            &mut app,
            Event::Key(KeyEvent::new(KeyCode::Char('w'), KeyModifiers::CONTROL)),
        );

        assert_eq!(
            app.snippet_modal.as_ref().unwrap().editor_name.text(),
            "review "
        );

        handle_snippet_event(
            &mut app,
            Event::Key(KeyEvent::new(KeyCode::Backspace, KeyModifiers::CONTROL)),
        );

        assert_eq!(app.snippet_modal.as_ref().unwrap().editor_name.text(), "");
    }

    #[test]
    fn multiline_snippet_waits_for_bracketed_paste_instead_of_pressing_enter() {
        let mut app = attached_mux_app("snippet-safe-paste-session");
        app.config.snippets = vec![crate::config::PromptSnippet {
            name: "Multiline".to_string(),
            prompt: "line one\nline two".to_string(),
        }];
        app.open_snippets();

        snippet_key(&mut app, KeyCode::Enter);

        let modal = app
            .snippet_modal
            .as_ref()
            .expect("unsafe paste keeps the modal open");
        assert!(modal
            .error
            .as_deref()
            .unwrap()
            .contains("not ready for multiline paste"));
    }

    #[test]
    fn deleting_a_snippet_requires_confirmation() {
        let mut app = attached_mux_app("snippet-delete-session");
        app.disable_config_persistence();
        app.config.snippets = vec![crate::config::PromptSnippet {
            name: "Disposable".to_string(),
            prompt: "Delete me".to_string(),
        }];
        app.open_snippets();

        snippet_key(&mut app, KeyCode::Char('d'));
        assert_eq!(
            app.snippet_modal.as_ref().unwrap().screen,
            SnippetScreen::ConfirmDelete
        );
        snippet_key(&mut app, KeyCode::Esc);
        assert_eq!(app.config.snippets.len(), 1, "cancel keeps the snippet");

        snippet_key(&mut app, KeyCode::Char('d'));
        snippet_key(&mut app, KeyCode::Char('y'));
        assert!(app.config.snippets.is_empty());
        assert_eq!(
            app.snippet_modal.as_ref().unwrap().screen,
            SnippetScreen::List
        );
    }

    #[test]
    fn editor_supports_multiline_prompt_input_and_project_scope_guard() {
        let mut app = attached_mux_app("snippet-editor-session");
        app.disable_config_persistence();
        app.open_snippets();
        snippet_key(&mut app, KeyCode::Char('a'));
        {
            let modal = app.snippet_modal.as_mut().unwrap();
            modal.editor_field = SnippetEditorField::Prompt;
        }

        snippet_key(&mut app, KeyCode::Char('o'));
        snippet_key(&mut app, KeyCode::Char('n'));
        snippet_key(&mut app, KeyCode::Char('e'));
        snippet_key(&mut app, KeyCode::Enter);
        handle_snippet_event(&mut app, Event::Paste("two\nthree".to_string()));
        assert_eq!(
            app.snippet_modal.as_ref().unwrap().editor_prompt.text(),
            "one\ntwo\nthree"
        );

        handle_snippet_event(
            &mut app,
            Event::Key(KeyEvent::new(KeyCode::Char('g'), KeyModifiers::CONTROL)),
        );
        let modal = app.snippet_modal.as_ref().unwrap();
        assert_eq!(modal.editor_scope, SnippetScope::Global);
        assert!(modal.error.as_deref().unwrap().contains("No Git project"));
    }

    #[test]
    fn editor_rejects_terminal_controls_before_persisting() {
        let mut app = attached_mux_app("snippet-control-session");
        app.open_snippets();
        snippet_key(&mut app, KeyCode::Char('a'));
        {
            let modal = app.snippet_modal.as_mut().unwrap();
            modal.editor_name.set_text("Unsafe");
            modal.editor_prompt.set_text("text\u{1b}[201~");
        }

        handle_snippet_event(
            &mut app,
            Event::Key(KeyEvent::new(KeyCode::Char('s'), KeyModifiers::CONTROL)),
        );

        assert!(app.config.snippets.is_empty());
        assert!(app
            .snippet_modal
            .as_ref()
            .unwrap()
            .error
            .as_deref()
            .unwrap()
            .contains("control characters"));
    }

    #[test]
    fn editing_can_move_a_global_snippet_into_project_scope() {
        let mut app = attached_mux_app("snippet-move-session");
        app.disable_config_persistence();
        app.config.snippets = vec![crate::config::PromptSnippet {
            name: "Move me".to_string(),
            prompt: "Project-specific prompt".to_string(),
        }];
        app.open_snippets();
        app.snippet_modal.as_mut().unwrap().project_root =
            Some(std::env::temp_dir().join("snippet-project"));

        snippet_key(&mut app, KeyCode::Char('e'));
        handle_snippet_event(
            &mut app,
            Event::Key(KeyEvent::new(KeyCode::Char('g'), KeyModifiers::CONTROL)),
        );
        handle_snippet_event(
            &mut app,
            Event::Key(KeyEvent::new(KeyCode::Char('s'), KeyModifiers::CONTROL)),
        );

        assert!(app.config.snippets.is_empty());
        let modal = app.snippet_modal.as_ref().unwrap();
        assert_eq!(modal.project.len(), 1);
        assert_eq!(modal.project[0].name, "Move me");
        assert_eq!(modal.screen, SnippetScreen::List);
    }

    #[test]
    fn editing_in_place_does_not_reorder_the_list() {
        let mut app = attached_mux_app("snippet-order-session");
        app.disable_config_persistence();
        app.config.snippets = vec![
            crate::config::PromptSnippet {
                name: "First".to_string(),
                prompt: "one".to_string(),
            },
            crate::config::PromptSnippet {
                name: "Second".to_string(),
                prompt: "two".to_string(),
            },
        ];
        app.open_snippets();
        snippet_key(&mut app, KeyCode::Char('e'));
        app.snippet_modal
            .as_mut()
            .unwrap()
            .editor_prompt
            .set_text("updated");
        handle_snippet_event(
            &mut app,
            Event::Key(KeyEvent::new(KeyCode::Char('s'), KeyModifiers::CONTROL)),
        );

        assert_eq!(app.config.snippets[0].name, "First");
        assert_eq!(app.config.snippets[0].prompt, "updated");
        assert_eq!(app.config.snippets[1].name, "Second");
    }

    #[test]
    fn scratchpad_help_is_available_from_the_list_without_running_sessions() {
        let mut app = mux_app();
        app.view = View::List;

        for key in [
            KeyEvent::new(KeyCode::Char('b'), KeyModifiers::CONTROL),
            KeyEvent::new(KeyCode::Char('h'), KeyModifiers::CONTROL),
            KeyEvent::new(KeyCode::Char('e'), KeyModifiers::NONE),
        ] {
            assert!(handle_list_prefix(&mut app, key));
        }

        assert_eq!(app.workspace_help, Some(WorkspaceHelp::Scratchpad));
    }

    #[test]
    fn an_unknown_prefix_command_clears_the_pending_state() {
        let mut app = mux_app();
        handle_attached_key(
            &mut app,
            KeyEvent::new(KeyCode::Char('b'), KeyModifiers::CONTROL),
        );

        handle_attached_key(
            &mut app,
            KeyEvent::new(KeyCode::Char('z'), KeyModifiers::NONE),
        );

        assert!(
            app.mux.as_ref().unwrap().prefix_state == PrefixState::Idle,
            "a stray prefix must not swallow every later keystroke"
        );
    }

    #[test]
    fn prefix_e_opens_the_attached_scratchpad() {
        let mut app = attached_mux_app("prefix-scratchpad-test");

        send_prefix_command(&mut app, 'e');

        assert!(app.scratchpad.is_some());
        assert_eq!(app.scratchpad_owner, Some(1));
        assert_eq!(app.workspace_focus, WorkspaceFocus::Scratchpad);
        app.scratchpad = None;
        let _ = app.mux.as_mut().unwrap().shutdown();
    }

    #[test]
    fn prefix_ctrl_h_e_opens_scratchpad_help() {
        let mut app = attached_mux_app("prefix-scratchpad-help-test");

        handle_attached_key(
            &mut app,
            KeyEvent::new(KeyCode::Char('b'), KeyModifiers::CONTROL),
        );
        handle_attached_key(
            &mut app,
            KeyEvent::new(KeyCode::Char('h'), KeyModifiers::CONTROL),
        );
        assert_eq!(app.mux.as_ref().unwrap().prefix_state, PrefixState::Help);

        handle_attached_key(
            &mut app,
            KeyEvent::new(KeyCode::Char('e'), KeyModifiers::NONE),
        );

        assert_eq!(app.workspace_help, Some(WorkspaceHelp::Scratchpad));
        assert_eq!(app.mux.as_ref().unwrap().prefix_state, PrefixState::Idle);

        handle_attached_event(
            &mut app,
            Event::Key(KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE)),
        );
        assert_eq!(app.workspace_help, None);
        let _ = app.mux.as_mut().unwrap().shutdown();
    }

    #[test]
    fn prefix_t_opens_and_hides_the_attached_terminal() {
        let mut app = attached_mux_app("prefix-terminal-test");

        send_prefix_command(&mut app, 't');
        assert!(app.terminal.is_visible());
        assert_eq!(app.terminal_owner, Some(1));
        assert_eq!(app.workspace_focus, WorkspaceFocus::Terminal);

        send_prefix_command(&mut app, 't');
        assert!(!app.terminal.is_visible());
        assert_eq!(app.workspace_focus, WorkspaceFocus::Chat);
        let _ = app.terminal.shutdown();
        let _ = app.mux.as_mut().unwrap().shutdown();
    }

    #[test]
    fn attached_panels_are_hidden_behind_the_pane_list_and_restored_on_reattach() {
        let mut app = attached_mux_app("panel-reattach-test");
        send_prefix_command(&mut app, 'e');
        send_prefix_command(&mut app, 't');

        app.open_pane_list();

        assert_eq!(app.mode, crate::app::Mode::PaneList);
        assert!(!app.attached_scratchpad_visible());
        assert!(!app.attached_terminal_visible());
        assert!(app.scratchpad_open.contains(&1));
        assert!(app.terminal_open.contains(&1));

        app.view = View::Attached(1);
        sync_workspace_panels(&mut app);

        assert!(app.attached_scratchpad_visible());
        assert!(app.attached_terminal_visible());
        app.scratchpad = None;
        let _ = app.terminal.shutdown();
        let _ = app.mux.as_mut().unwrap().shutdown();
    }

    #[test]
    fn attached_panels_are_not_inherited_by_another_session() {
        let mut app = attached_mux_app("panel-owner-a");
        push_test_pane(&mut app, 2, "panel-owner-b");
        app.mux.as_mut().unwrap().select_index(0);
        send_prefix_command(&mut app, 'e');
        send_prefix_command(&mut app, 't');

        app.mux.as_mut().unwrap().select_index(1);
        app.view = View::Attached(2);
        sync_workspace_panels(&mut app);

        assert!(!app.attached_scratchpad_visible());
        assert!(!app.attached_terminal_visible());

        app.mux.as_mut().unwrap().select_index(0);
        app.view = View::Attached(1);
        sync_workspace_panels(&mut app);

        assert!(app.attached_scratchpad_visible());
        assert!(app.attached_terminal_visible());
        app.scratchpad = None;
        let _ = app.terminal.shutdown();
        let _ = app.mux.as_mut().unwrap().shutdown();
    }

    #[test]
    fn prefix_c_returns_focus_to_chat_without_hiding_panels() {
        let mut app = attached_mux_app("prefix-chat-test");
        send_prefix_command(&mut app, 'e');
        assert!(app.scratchpad.is_some());

        send_prefix_command(&mut app, 'c');

        assert_eq!(app.workspace_focus, WorkspaceFocus::Chat);
        assert!(app.scratchpad.is_some());
        app.scratchpad = None;
        let _ = app.mux.as_mut().unwrap().shutdown();
    }

    #[test]
    fn attached_view_falls_back_to_the_list_when_no_panes_remain() {
        let mut app = mux_app();
        app.view = View::Attached(7);

        sync_view(&mut app);

        assert_eq!(app.view, View::List);
    }

    #[test]
    fn output_for_an_unfocused_pane_does_not_force_a_repaint() {
        let mut app = mux_app();
        app.view = View::Attached(1);

        assert!(handle_mux_event(
            &mut app,
            MuxEvent::Output(1, Default::default())
        ));
        assert!(!handle_mux_event(
            &mut app,
            MuxEvent::Output(2, Default::default())
        ));
    }

    #[test]
    fn host_progress_sequences_still_request_the_outer_terminal_flush() {
        let mut app = attached_mux_app("foreground");
        let sequence = b"\x1b]9;4;3;0\x1b\\".to_vec();

        assert!(handle_mux_event(
            &mut app,
            MuxEvent::HostSequence(1, sequence.clone())
        ));
        assert_eq!(app.host_sequences, vec![sequence]);
    }

    #[test]
    fn background_pane_progress_cannot_override_the_focused_tab_spinner() {
        let mut app = attached_mux_app("foreground");
        push_test_pane(&mut app, 2, "background");
        app.mux.as_mut().unwrap().focused = Some(1);
        let sequence = b"\x1b]9;4;0;0\x1b\\".to_vec();

        // The frame is repainted because the background pane's own tab shows this
        // state, but the sequence itself must not reach the outer terminal.
        assert!(handle_mux_event(
            &mut app,
            MuxEvent::HostSequence(2, sequence)
        ));
        assert!(app.host_sequences.is_empty());
    }

    /// Without a tab strip there is nothing on screen showing a background pane's
    /// progress, so it must not wake the renderer.
    #[test]
    fn background_progress_does_not_repaint_when_no_tab_strip_is_showing() {
        let mut app = attached_mux_app("only");
        app.mux.as_mut().unwrap().focused = Some(1);
        assert!(!app.tab_bar_visible());

        assert!(!handle_mux_event(
            &mut app,
            MuxEvent::HostSequence(2, b"\x1b]9;4;3;0\x1b\\".to_vec())
        ));
        assert!(app.host_sequences.is_empty());
    }

    #[test]
    fn progress_sequence_updates_background_state_before_a_focus_switch() {
        let mut app = attached_mux_app("foreground");
        push_test_pane(&mut app, 2, "background");
        app.mux.as_mut().unwrap().focused = Some(1);

        // Repaints for the background pane's tab marker, without forwarding upstream.
        assert!(handle_mux_event(
            &mut app,
            MuxEvent::HostSequence(2, b"\x1b]9;4;3;0\x1b\\".to_vec())
        ));
        assert!(app.host_sequences.is_empty());
        handle_mux_event(
            &mut app,
            MuxEvent::Output(
                2,
                crate::mux::callbacks::PaneSignals {
                    events: vec![crate::mux::callbacks::PaneSignalEvent::Progress(
                        crate::host_terminal::ProgressState::Indeterminate,
                    )],
                    ..Default::default()
                },
            ),
        );
        assert!(handle_mux_event(
            &mut app,
            MuxEvent::HostSequence(2, b"\x1b]9;4;0;0\x1b\\".to_vec())
        ));
        assert!(app.host_sequences.is_empty());

        app.mux.as_mut().unwrap().focused = Some(2);
        app.view = View::Attached(2);
        sync_workspace_panels(&mut app);
        assert_eq!(
            app.host_sequences.last(),
            Some(&crate::host_terminal::CLEAR_PROGRESS.to_vec()),
            "the clear sequence must be recorded before its later Output event"
        );
    }

    #[test]
    fn focused_question_clears_and_suppresses_progress_until_resolved() {
        use crate::events::lifecycle::{InputKind, LifecycleEvent};

        let mut app = attached_mux_app("foreground-question");
        app.config.notifications.enabled = true;
        app.config.notifications.topic = "private_topic".to_string();
        app.terminal_focused = true;

        let working = crate::mux::callbacks::PaneSignals {
            events: vec![crate::mux::callbacks::PaneSignalEvent::Progress(
                crate::host_terminal::ProgressState::Indeterminate,
            )],
            ..Default::default()
        };
        handle_mux_event(&mut app, MuxEvent::Output(1, working.clone()));
        app.host_sequences.clear();

        assert!(handle_mux_event(
            &mut app,
            MuxEvent::SessionLifecycle(
                1,
                LifecycleEvent::InputRequested {
                    tool_call_id: "question-1".into(),
                    kind: InputKind::Question,
                },
            )
        ));
        assert_eq!(
            app.host_sequences,
            vec![crate::host_terminal::CLEAR_PROGRESS.to_vec()]
        );
        assert_eq!(app.notification_requests.len(), 1);
        assert_eq!(
            app.notification_requests[0].kind,
            NotificationKind::Question
        );
        assert!(app.mux.as_ref().unwrap().pane(1).unwrap().needs_attention());
        app.acknowledge_focused_pane();
        assert!(
            app.mux.as_ref().unwrap().pane(1).unwrap().needs_attention(),
            "a focused question remains visible until it is answered"
        );

        app.host_sequences.clear();
        assert!(!handle_mux_event(
            &mut app,
            MuxEvent::HostSequence(1, b"\x1b]9;4;3;0\x1b\\".to_vec())
        ));
        assert!(app.host_sequences.is_empty());
        handle_mux_event(&mut app, MuxEvent::Output(1, working));

        handle_mux_event(
            &mut app,
            MuxEvent::SessionLifecycle(
                1,
                LifecycleEvent::InputResolved {
                    tool_call_id: "not-the-question".into(),
                    kind: InputKind::Question,
                },
            ),
        );
        assert!(app.mux.as_ref().unwrap().pane(1).unwrap().needs_attention());
        assert!(app.host_sequences.is_empty());

        handle_mux_event(
            &mut app,
            MuxEvent::SessionLifecycle(
                1,
                LifecycleEvent::InputResolved {
                    tool_call_id: "question-1".into(),
                    kind: InputKind::Question,
                },
            ),
        );
        assert!(!app.mux.as_ref().unwrap().pane(1).unwrap().needs_attention());
        assert_eq!(
            app.host_sequences,
            vec![crate::host_terminal::progress_sequence_for_state(
                crate::host_terminal::ProgressState::Indeterminate
            )]
        );

        let complete = crate::mux::callbacks::PaneSignals {
            events: vec![crate::mux::callbacks::PaneSignalEvent::Progress(
                crate::host_terminal::ProgressState::Clear,
            )],
            ..Default::default()
        };
        handle_mux_event(&mut app, MuxEvent::Output(1, complete));
        // The question was worth telling you about even though you were watching: it
        // marks the tab `?` whether or not you are looking, because it blocks the
        // session. The completion behind it is not, because it leaves no marker either.
        assert_eq!(
            app.notification_requests.len(),
            1,
            "got: {:?}",
            app.notification_requests
        );
        assert_eq!(
            app.notification_requests[0].kind,
            NotificationKind::Question
        );
    }

    #[test]
    fn focused_hook_lifecycle_drives_outer_spinner_without_raw_osc() {
        use crate::events::hooks::HookLifecycleEvent;

        let mut app = attached_mux_app("hook-status");
        app.config.notifications.enabled = true;
        app.config.notifications.topic = "private_topic".to_string();

        assert!(handle_mux_event(
            &mut app,
            MuxEvent::HookLifecycle(1, HookLifecycleEvent::Working { timestamp: 1 })
        ));
        assert_eq!(
            app.host_sequences,
            vec![crate::host_terminal::progress_sequence_for_state(
                crate::host_terminal::ProgressState::Indeterminate
            )]
        );

        app.host_sequences.clear();
        assert!(!handle_mux_event(
            &mut app,
            MuxEvent::HostSequence(1, crate::host_terminal::CLEAR_PROGRESS.to_vec())
        ));
        assert!(app.host_sequences.is_empty());

        assert!(handle_mux_event(
            &mut app,
            MuxEvent::HookLifecycle(1, HookLifecycleEvent::Ready { timestamp: 2 })
        ));
        assert_eq!(
            app.host_sequences,
            vec![crate::host_terminal::CLEAR_PROGRESS.to_vec()]
        );
        assert!(app.notification_requests.is_empty());
        assert!(handle_mux_event(
            &mut app,
            MuxEvent::HookReadyConfirmed(1, 2)
        ));
        assert!(app.notification_requests.is_empty());
        let _ = app.mux.as_mut().unwrap().shutdown();
    }

    #[test]
    fn raw_reentry_after_hook_ready_starts_a_fresh_notification_boundary() {
        use crate::events::hooks::HookLifecycleEvent;

        let home = tempfile::tempdir().unwrap();
        let session_id = "hook-raw-reentry";
        let session_dir = home.path().join("session-state").join(session_id);
        std::fs::create_dir_all(&session_dir).unwrap();
        let events = session_dir.join("events.jsonl");
        std::fs::write(&events, "first cycle").unwrap();
        let mut app = attached_mux_app(session_id);
        app.copilot_home = home.path().to_path_buf();
        handle_mux_event(
            &mut app,
            MuxEvent::HookLifecycle(1, HookLifecycleEvent::Working { timestamp: 1 }),
        );
        handle_mux_event(
            &mut app,
            MuxEvent::HookLifecycle(1, HookLifecycleEvent::Ready { timestamp: 2 }),
        );
        handle_mux_event(&mut app, MuxEvent::HookReadyConfirmed(1, 2));

        std::fs::write(&events, "second cycle is longer").unwrap();
        handle_mux_event(
            &mut app,
            MuxEvent::HostSequence(1, b"\x1b]9;4;3;0\x1b\\".to_vec()),
        );

        assert_eq!(
            app.notification_cycle_offset(session_id),
            Some(std::fs::metadata(events).unwrap().len())
        );
        let _ = app.mux.as_mut().unwrap().shutdown();
    }

    #[test]
    fn background_hook_lifecycle_never_controls_outer_spinner() {
        use crate::events::hooks::HookLifecycleEvent;

        let mut app = attached_mux_app("foreground");
        push_test_pane(&mut app, 2, "background-hook");
        app.mux.as_mut().unwrap().focused = Some(1);

        assert!(handle_mux_event(
            &mut app,
            MuxEvent::HookLifecycle(2, HookLifecycleEvent::Working { timestamp: 1 })
        ));
        assert!(app.host_sequences.is_empty());
        assert!(app.mux.as_ref().unwrap().pane(2).unwrap().is_working());
        let _ = app.mux.as_mut().unwrap().shutdown();
    }

    #[test]
    fn forwarding_permission_response_restores_authoritative_working_spinner() {
        use crate::events::hooks::{HookAttention, HookLifecycleEvent};

        let mut app = attached_mux_app("permission-hook");
        handle_mux_event(
            &mut app,
            MuxEvent::HookLifecycle(1, HookLifecycleEvent::Working { timestamp: 1 }),
        );
        handle_mux_event(
            &mut app,
            MuxEvent::HookLifecycle(
                1,
                HookLifecycleEvent::Awaiting {
                    timestamp: 2,
                    attention: HookAttention::Permission,
                },
            ),
        );
        app.host_sequences.clear();

        handle_attached_event(
            &mut app,
            Event::Key(KeyEvent::new(KeyCode::Char('y'), KeyModifiers::NONE)),
        );

        assert!(!app
            .mux
            .as_ref()
            .unwrap()
            .pane(1)
            .unwrap()
            .requires_user_action());
        assert_eq!(
            app.host_sequences,
            vec![crate::host_terminal::progress_sequence_for_state(
                crate::host_terminal::ProgressState::Indeterminate
            )]
        );
        let _ = app.mux.as_mut().unwrap().shutdown();
    }

    #[test]
    fn local_mouse_event_does_not_clear_permission_wait() {
        use crate::events::hooks::{HookAttention, HookLifecycleEvent};

        let mut app = attached_mux_app("permission-mouse");
        handle_mux_event(
            &mut app,
            MuxEvent::HookLifecycle(1, HookLifecycleEvent::Working { timestamp: 1 }),
        );
        handle_mux_event(
            &mut app,
            MuxEvent::HookLifecycle(
                1,
                HookLifecycleEvent::Awaiting {
                    timestamp: 2,
                    attention: HookAttention::Permission,
                },
            ),
        );

        handle_attached_event(
            &mut app,
            Event::Mouse(crossterm::event::MouseEvent {
                kind: MouseEventKind::Down(MouseButton::Left),
                column: 5,
                row: 5,
                modifiers: KeyModifiers::NONE,
            }),
        );

        assert!(app
            .mux
            .as_ref()
            .unwrap()
            .pane(1)
            .unwrap()
            .requires_user_action());
        let _ = app.mux.as_mut().unwrap().shutdown();
    }

    #[test]
    fn permission_navigation_key_does_not_clear_wait() {
        use crate::events::hooks::{HookAttention, HookLifecycleEvent};

        let mut app = attached_mux_app("permission-navigation");
        handle_mux_event(
            &mut app,
            MuxEvent::HookLifecycle(1, HookLifecycleEvent::Working { timestamp: 1 }),
        );
        handle_mux_event(
            &mut app,
            MuxEvent::HookLifecycle(
                1,
                HookLifecycleEvent::Awaiting {
                    timestamp: 2,
                    attention: HookAttention::Permission,
                },
            ),
        );

        handle_attached_event(
            &mut app,
            Event::Key(KeyEvent::new(KeyCode::Down, KeyModifiers::NONE)),
        );
        handle_attached_event(
            &mut app,
            Event::Key(KeyEvent::new(KeyCode::Char('n'), KeyModifiers::CONTROL)),
        );
        handle_attached_event(
            &mut app,
            Event::Key(KeyEvent::new(KeyCode::Enter, KeyModifiers::SHIFT)),
        );

        assert!(app
            .mux
            .as_ref()
            .unwrap()
            .pane(1)
            .unwrap()
            .requires_user_action());
        let _ = app.mux.as_mut().unwrap().shutdown();
    }

    #[test]
    fn permission_resolution_keys_require_literal_unmodified_input() {
        assert!(key_resolves_permission(&KeyEvent::new(
            KeyCode::Enter,
            KeyModifiers::NONE
        )));
        assert!(key_resolves_permission(&KeyEvent::new(
            KeyCode::Char('Y'),
            KeyModifiers::SHIFT
        )));
        assert!(!key_resolves_permission(&KeyEvent::new(
            KeyCode::Char('n'),
            KeyModifiers::CONTROL
        )));
        assert!(!key_resolves_permission(&KeyEvent::new(
            KeyCode::Enter,
            KeyModifiers::SHIFT
        )));
    }

    #[test]
    fn background_plan_approval_marks_attention_without_changing_outer_progress() {
        use crate::events::lifecycle::{InputKind, LifecycleEvent};

        let mut app = attached_mux_app("foreground");
        app.config.notifications.enabled = true;
        app.config.notifications.topic = "private_topic".to_string();
        push_test_pane(&mut app, 2, "background-plan");
        app.mux.as_mut().unwrap().focused = Some(1);
        app.host_sequences.clear();

        assert!(handle_mux_event(
            &mut app,
            MuxEvent::SessionLifecycle(
                2,
                LifecycleEvent::InputRequested {
                    tool_call_id: "plan-1".into(),
                    kind: InputKind::PlanApproval,
                },
            )
        ));
        assert!(app.host_sequences.is_empty());
        assert!(app.mux.as_ref().unwrap().pane(2).unwrap().needs_attention());
        assert_eq!(
            app.notification_requests[0].kind,
            NotificationKind::PlanApproval
        );
    }

    #[test]
    fn title_change_for_an_unfocused_pane_repaints_the_tab_strip() {
        let mut app = attached_mux_app("foreground");
        push_test_pane(&mut app, 2, "background");
        app.mux.as_mut().unwrap().focused = Some(1);
        app.view = View::Attached(1);
        let signals = crate::mux::callbacks::PaneSignals {
            title: Some("Renamed background pane - GitHub Copilot".to_string()),
            events: Vec::new(),
        };

        assert!(handle_mux_event(&mut app, MuxEvent::Output(2, signals)));
        assert_eq!(
            app.mux.as_ref().unwrap().pane(2).unwrap().title,
            "Renamed background pane"
        );
    }

    #[test]
    fn background_completion_requests_attention_and_switching_clears_it() {
        let mut app = attached_mux_app("foreground");
        app.config.notifications.enabled = true;
        app.config.notifications.topic = "private_topic".to_string();
        push_test_pane(&mut app, 2, "background");
        app.mux.as_mut().unwrap().focused = Some(1);
        app.view = View::Attached(1);
        app.terminal_focused = true;

        app.mux
            .as_mut()
            .unwrap()
            .pane_mut(2)
            .unwrap()
            .feed_synthetic(b"\x1b]9;4;3;0\x1b\\");
        let working = crate::mux::callbacks::PaneSignals {
            events: vec![crate::mux::callbacks::PaneSignalEvent::Progress(
                crate::host_terminal::ProgressState::Indeterminate,
            )],
            ..Default::default()
        };
        handle_mux_event(&mut app, MuxEvent::Output(2, working));
        app.mux
            .as_mut()
            .unwrap()
            .pane_mut(2)
            .unwrap()
            .feed_synthetic(b"\x1b]9;4;0;0\x1b\\");

        let complete = crate::mux::callbacks::PaneSignals {
            events: vec![crate::mux::callbacks::PaneSignalEvent::Progress(
                crate::host_terminal::ProgressState::Clear,
            )],
            ..Default::default()
        };
        assert!(
            handle_mux_event(&mut app, MuxEvent::Output(2, complete)),
            "attention transition must repaint the internal tab strip"
        );
        assert!(app.mux.as_ref().unwrap().pane(2).unwrap().needs_attention());
        assert_eq!(app.notification_requests.len(), 1);
        assert_eq!(app.notification_requests[0].kind, NotificationKind::Ready);
        assert_eq!(app.notification_requests[0].session_title, "Test session 2");
        assert!(app
            .mux
            .as_ref()
            .unwrap()
            .pane(2)
            .unwrap()
            .display_title()
            .starts_with("? "));

        app.mux.as_mut().unwrap().focused = Some(2);
        app.view = View::Attached(2);
        sync_workspace_panels(&mut app);

        assert!(!app.mux.as_ref().unwrap().pane(2).unwrap().needs_attention());
        let _ = app.mux.as_mut().unwrap().shutdown();
    }

    #[test]
    fn completion_in_the_focused_terminal_needs_attention_only_when_unfocused() {
        let mut app = attached_mux_app("focus-test");
        app.config.notifications.enabled = true;
        app.config.notifications.topic = "private_topic".to_string();
        app.terminal_focused = false;
        app.mux
            .as_mut()
            .unwrap()
            .pane_mut(1)
            .unwrap()
            .feed_synthetic(b"\x1b]9;4;3;0\x1b\\\x1b]9;4;0;0\x1b\\");

        let signals = crate::mux::callbacks::PaneSignals {
            events: vec![
                crate::mux::callbacks::PaneSignalEvent::Progress(
                    crate::host_terminal::ProgressState::Indeterminate,
                ),
                crate::mux::callbacks::PaneSignalEvent::Progress(
                    crate::host_terminal::ProgressState::Clear,
                ),
            ],
            ..Default::default()
        };
        handle_mux_event(&mut app, MuxEvent::Output(1, signals));
        assert!(app.mux.as_ref().unwrap().pane(1).unwrap().needs_attention());
        assert_eq!(app.notification_requests.len(), 1);
        assert_eq!(app.notification_requests[0].kind, NotificationKind::Ready);

        app.terminal_focused = true;
        app.acknowledge_focused_pane();
        assert!(app.terminal_focused);
        assert!(!app.mux.as_ref().unwrap().pane(1).unwrap().needs_attention());
        let _ = app.mux.as_mut().unwrap().shutdown();
    }

    #[test]
    fn a_watched_pane_reports_an_error_but_not_a_plain_completion() {
        let mut app = attached_mux_app("notification-history");
        app.config.notifications.enabled = true;
        app.config.notifications.topic = "private_topic".to_string();
        app.config.ntfy_verbose = true;
        let copilot_home = tempfile::tempdir().unwrap();
        app.copilot_home = copilot_home.path().to_path_buf();
        app.terminal_focused = true;

        let ready = crate::mux::callbacks::PaneSignals {
            events: vec![
                crate::mux::callbacks::PaneSignalEvent::Progress(
                    crate::host_terminal::ProgressState::Indeterminate,
                ),
                crate::mux::callbacks::PaneSignalEvent::Progress(
                    crate::host_terminal::ProgressState::Clear,
                ),
            ],
            ..Default::default()
        };
        handle_mux_event(&mut app, MuxEvent::Output(1, ready));
        assert!(!app.mux.as_ref().unwrap().pane(1).unwrap().needs_attention());
        assert!(
            app.notification_requests.is_empty(),
            "a turn that finished in front of you is already on your screen, got: {:?}",
            app.notification_requests
        );

        let error = crate::mux::callbacks::PaneSignals {
            events: vec![crate::mux::callbacks::PaneSignalEvent::Progress(
                crate::host_terminal::ProgressState::Error,
            )],
            ..Default::default()
        };
        handle_mux_event(&mut app, MuxEvent::Output(1, error));
        // An error is worth knowing about wherever you are looking, and it has its own
        // `notifications.error` switch for people who disagree.
        assert_eq!(app.notification_requests[0].kind, NotificationKind::Error);
        assert_eq!(
            app.notification_requests[0].events_path.as_deref(),
            Some(
                copilot_home
                    .path()
                    .join("session-state")
                    .join("notification-history")
                    .join("events.jsonl")
                    .as_path()
            ),
            "verbose mode still attaches the event tail"
        );
        let clear = crate::mux::callbacks::PaneSignals {
            events: vec![crate::mux::callbacks::PaneSignalEvent::Progress(
                crate::host_terminal::ProgressState::Clear,
            )],
            ..Default::default()
        };
        handle_mux_event(&mut app, MuxEvent::Output(1, clear));
        assert_eq!(
            app.notification_requests.len(),
            1,
            "error suppresses duplicate ready in the same work cycle"
        );
        let _ = app.mux.as_mut().unwrap().shutdown();
    }

    #[test]
    fn exit_events_report_the_status() {
        use crate::events::lifecycle::{InputKind, LifecycleEvent};

        let mut app = attached_mux_app("exiting");
        app.config.notifications.enabled = true;
        app.config.notifications.topic = "private_topic".to_string();
        handle_mux_event(
            &mut app,
            MuxEvent::HostSequence(1, b"\x1b]9;4;3;0\x1b\\".to_vec()),
        );
        app.host_sequences.clear();

        assert!(handle_mux_event(&mut app, MuxEvent::Exited(1, Some(2))));

        let message = app.status_message.as_deref().unwrap();
        assert!(message.contains("code 2"), "unexpected message: {message}");
        assert_eq!(
            app.host_sequences,
            vec![crate::host_terminal::CLEAR_PROGRESS.to_vec()]
        );
        let pane = app.mux.as_ref().unwrap().pane(1).unwrap();
        assert_eq!(
            pane.effective_progress_state(),
            crate::host_terminal::ProgressState::Clear
        );
        assert!(!pane.is_working());

        app.host_sequences.clear();
        assert!(handle_mux_event(
            &mut app,
            MuxEvent::SessionLifecycle(
                1,
                LifecycleEvent::InputRequested {
                    tool_call_id: "late-question".into(),
                    kind: InputKind::Question,
                }
            )
        ));
        assert!(!app.mux.as_ref().unwrap().pane(1).unwrap().needs_attention());
        assert!(app.notification_requests.is_empty());
        assert!(app.host_sequences.is_empty());
    }

    #[test]
    fn background_exit_does_not_clear_the_focused_pane_progress() {
        let mut app = attached_mux_app("foreground");
        push_test_pane(&mut app, 2, "background");
        app.mux.as_mut().unwrap().focused = Some(1);

        assert!(handle_mux_event(&mut app, MuxEvent::Exited(2, Some(0))));
        assert!(app.host_sequences.is_empty());
    }

    #[test]
    fn a_running_pane_never_offers_a_restart_target_because_r_there_is_ordinary_typing() {
        let mut app = attached_mux_app("alive");
        assert!(restart_target(&app).is_none());
        let _ = app.mux.as_mut().unwrap().shutdown();
    }

    /// The whole journey from the bug report, against the real binary: Copilot crashes,
    /// the dead pane offers `r`, and pressing it brings the same session back in place.
    /// Everything between the keystroke and the resumed child is real — the PTY, the
    /// key routing, `attach_session`, the PATH lookup, the spawn.
    ///
    /// Hermetic despite being live: `--copilot-home` points into a sandbox and a
    /// compiled stand-in `copilot` sits first on PATH, so nothing reads the real
    /// session list and no AI credit is spent. The stand-in crashes its first launch
    /// with code 134 — the OOM exit this feature was built for — and stays alive when
    /// resumed. PATH is changed process-wide, which is safe here because every other
    /// ignored test gates itself off unless its own env var is set.
    ///
    /// One known leak: the What's new marker lives in the real local data dir, not
    /// under the Copilot home, so a run on a machine whose marker is older than this
    /// build advances it. The screen stays reachable from the command palette.
    #[test]
    #[ignore = "drives the real cst binary in a PTY; run with CST_RESTART_E2E=1"]
    fn a_crashed_session_really_restarts_in_place_when_r_is_pressed() {
        use std::sync::mpsc::{self, Receiver};
        use std::time::{Duration, Instant};

        if std::env::var_os("CST_RESTART_E2E").is_none() {
            return;
        }

        // target/debug/deps/<test>-<hash>.exe -> target/debug/<bin>.
        let mut dir = std::env::current_exe().expect("test executable path");
        dir.pop();
        if dir.ends_with("deps") {
            dir.pop();
        }
        let cst = dir.join(format!(
            "copilot-session-tui{}",
            std::env::consts::EXE_SUFFIX
        ));
        assert!(
            cst.exists(),
            "the cst binary must exist at {} (cargo test builds it)",
            cst.display()
        );

        let sandbox = tempfile::tempdir().expect("sandbox directory");
        let home = sandbox.path().join("home");
        std::fs::create_dir_all(home.join("session-state")).unwrap();
        let bin = sandbox.path().join("bin");
        std::fs::create_dir_all(&bin).unwrap();
        let log = sandbox.path().join("invocations.log");

        let shim_src = sandbox.path().join("copilot_shim.rs");
        std::fs::write(
            &shim_src,
            r#"
use std::io::Write;
fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    if let Ok(log) = std::env::var("CST_E2E_LOG") {
        let mut file = std::fs::OpenOptions::new().create(true).append(true).open(log).unwrap();
        writeln!(file, "{}", args.join(" ")).unwrap();
    }
    if args.iter().any(|arg| arg == "--version") {
        println!("1.99.0 (fake copilot for the CST restart e2e)");
        return;
    }
    if args.iter().any(|arg| arg.starts_with("--session-id=")) {
        println!("FAKE COPILOT STARTED");
        std::io::stdout().flush().unwrap();
        std::thread::sleep(std::time::Duration::from_secs(2));
        std::process::exit(134);
    }
    if let Some(id) = args.iter().find_map(|arg| arg.strip_prefix("--resume=")) {
        println!("FAKE COPILOT RESUMED {id}");
        std::io::stdout().flush().unwrap();
        std::thread::sleep(std::time::Duration::from_secs(600));
        return;
    }
    std::process::exit(1);
}
"#,
        )
        .unwrap();
        let compiled = std::process::Command::new("rustc")
            .arg(&shim_src)
            .arg("-o")
            .arg(bin.join(format!("copilot{}", std::env::consts::EXE_SUFFIX)))
            .status()
            .expect("rustc must be available to build the stand-in Copilot");
        assert!(compiled.success(), "stand-in Copilot failed to compile");

        let path = std::env::join_paths(std::iter::once(bin.clone()).chain(std::env::split_paths(
            &std::env::var_os("PATH").unwrap_or_default(),
        )))
        .unwrap();
        std::env::set_var("PATH", &path);
        std::env::set_var("CST_E2E_LOG", &log);

        let (tx, rx) = mpsc::channel();
        let mut pane = Pane::spawn(
            PaneSpec {
                id: 1,
                title: "e2e driver".to_string(),
                cwd: sandbox.path().to_path_buf(),
                session_id: "e2e-driver".to_string(),
                program: cst.to_string_lossy().into_owned(),
                args: vec![
                    "--mux".to_string(),
                    "--copilot-home".to_string(),
                    home.to_string_lossy().into_owned(),
                ],
                events_path: None,
                terminal_light_mode: Some(false),
                hooks_active: false,
            },
            35,
            120,
            tx,
        )
        .expect("cst must start inside the PTY");

        fn wait_for(pane: &Pane, rx: &Receiver<MuxEvent>, needle: &str) -> String {
            let deadline = Instant::now() + Duration::from_secs(30);
            let mut contents = String::new();
            while Instant::now() < deadline {
                let _ = rx.recv_timeout(Duration::from_millis(200));
                contents = pane.with_screen(|screen| screen.contents()).unwrap();
                if contents.contains(needle) {
                    return contents;
                }
            }
            panic!("never saw {needle:?} on screen; last frame:\n{contents}");
        }

        wait_for(&pane, &rx, "Start a new session here");
        // A machine whose last-ran marker is older than this build gets the What's
        // new screen on top of the list; it eats every key until dismissed.
        if pane
            .with_screen(|screen| screen.contents())
            .unwrap()
            .contains("What's new")
        {
            pane.send_key(&KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE))
                .unwrap();
            let deadline = Instant::now() + Duration::from_secs(10);
            while Instant::now() < deadline {
                let _ = rx.recv_timeout(Duration::from_millis(200));
                let contents = pane.with_screen(|screen| screen.contents()).unwrap();
                if !contents.contains("What's new") {
                    break;
                }
            }
        }
        // `n` asks whether to start as-is or in a worktree; the second `n` answers.
        pane.send_key(&KeyEvent::new(KeyCode::Char('n'), KeyModifiers::NONE))
            .unwrap();
        wait_for(&pane, &rx, "in the project as-is");
        pane.send_key(&KeyEvent::new(KeyCode::Char('n'), KeyModifiers::NONE))
            .unwrap();
        wait_for(&pane, &rx, "FAKE COPILOT STARTED");
        wait_for(&pane, &rx, "exited with code 134");
        wait_for(&pane, &rx, "r restart");
        pane.send_key(&KeyEvent::new(KeyCode::Char('r'), KeyModifiers::NONE))
            .unwrap();
        wait_for(&pane, &rx, "FAKE COPILOT RESUMED");

        let invocations = std::fs::read_to_string(&log).unwrap();
        let crashed = invocations
            .lines()
            .find_map(|line| {
                line.split_whitespace()
                    .find_map(|arg| arg.strip_prefix("--session-id="))
            })
            .expect("the crashed launch must have been given a session id")
            .to_string();
        assert!(
            invocations.contains(&format!("--resume={crashed}")),
            "r must resume the very session that crashed: {invocations}"
        );

        let _ = pane.kill();
    }

    /// The text inside the popup box titled `title`, one string per row, read cell by
    /// cell so the session list drawn behind it — which shows the same names — is not
    /// mistaken for the popup's rows.
    fn popup_rows(screen: &vt100::Screen, title: &str) -> Option<Vec<String>> {
        let (rows, cols) = screen.size();
        let cell = |row: u16, col: u16| {
            screen
                .cell(row, col)
                .map(|cell| cell.contents().to_string())
                .unwrap_or_default()
        };
        // The list behind is titled "Sessions" too, and only sometimes follows it with
        // more text, such as a workspace to recover. Its border is always at the left
        // edge, where a centred popup never is.
        let heading = format!("┌ {title} ─");
        let width = heading.chars().count() as u16;
        let (top, left) = (0..rows).find_map(|row| {
            (1..cols.saturating_sub(width)).find_map(|col| {
                let text: String = (col..col + width).map(|col| cell(row, col)).collect();
                (text == heading).then_some((row, col))
            })
        })?;
        let right = (left + 1..cols).find(|col| cell(top, *col) == "┐")?;
        Some(
            (top + 1..rows)
                .take_while(|row| cell(*row, left) != "└")
                .map(|row| {
                    (left + 1..right)
                        .map(|col| cell(row, col))
                        .collect::<String>()
                        .trim_end()
                        .to_string()
                })
                .collect(),
        )
    }

    /// The `prefix w` switcher, typed into for real: the actual binary in a PTY, three
    /// sessions resumed under a stand-in Copilot, and every assertion made on what the
    /// popup draws. Hermetic in the same way as the restart e2e above, with the same
    /// What's new leak; it also closes every session it opened before leaving, so its
    /// workspace record is gone and nothing is offered for recovery afterwards.
    #[test]
    #[ignore = "drives the real cst binary in a PTY; run with CST_SWITCHER_E2E=1"]
    fn the_session_switcher_filters_as_you_type_in_the_real_binary() {
        use std::sync::mpsc::{self, Receiver};
        use std::time::{Duration, Instant};

        if std::env::var_os("CST_SWITCHER_E2E").is_none() {
            return;
        }

        let mut dir = std::env::current_exe().expect("test executable path");
        dir.pop();
        if dir.ends_with("deps") {
            dir.pop();
        }
        let cst = dir.join(format!(
            "copilot-session-tui{}",
            std::env::consts::EXE_SUFFIX
        ));
        assert!(
            cst.exists(),
            "the cst binary must exist at {}",
            cst.display()
        );

        let sandbox = tempfile::tempdir().expect("sandbox directory");
        let home = sandbox.path().join("home");
        let sessions = [
            (
                "11111111-1111-4111-8111-111111111111",
                "Refactor payment api",
                "payments",
            ),
            (
                "22222222-2222-4222-8222-222222222222",
                "Polish landing page",
                "website",
            ),
            (
                "33333333-3333-4333-8333-333333333333",
                "Write release notes",
                "handbook",
            ),
        ];
        for (index, (id, name, project)) in sessions.iter().enumerate() {
            let state = home.join("session-state").join(id);
            std::fs::create_dir_all(&state).unwrap();
            let cwd = sandbox.path().join("projects").join(project);
            std::fs::create_dir_all(&cwd).unwrap();
            std::fs::write(
                state.join("workspace.yaml"),
                format!(
                    "id: {id}\ncwd: '{}'\nname: {name}\ncreated_at: 2026-08-0{day}T12:00:00Z\nupdated_at: 2026-08-0{day}T12:00:00Z\n",
                    cwd.display(),
                    day = 7 - index,
                ),
            )
            .unwrap();
        }
        let bin = sandbox.path().join("bin");
        std::fs::create_dir_all(&bin).unwrap();
        let log = sandbox.path().join("invocations.log");
        let shim_src = sandbox.path().join("copilot_shim.rs");
        std::fs::write(
            &shim_src,
            r#"
use std::io::Write;
fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    if let Ok(log) = std::env::var("CST_E2E_LOG") {
        let mut file = std::fs::OpenOptions::new().create(true).append(true).open(log).unwrap();
        writeln!(file, "{}", args.join(" ")).unwrap();
    }
    if args.iter().any(|arg| arg == "--version") {
        println!("1.99.0 (fake copilot for the CST switcher e2e)");
        return;
    }
    if let Some(id) = args.iter().find_map(|arg| arg.strip_prefix("--resume=")) {
        println!("FAKE COPILOT RESUMED {id}");
        std::io::stdout().flush().unwrap();
        std::thread::sleep(std::time::Duration::from_secs(600));
        return;
    }
    std::process::exit(1);
}
"#,
        )
        .unwrap();
        let compiled = std::process::Command::new("rustc")
            .arg(&shim_src)
            .arg("-o")
            .arg(bin.join(format!("copilot{}", std::env::consts::EXE_SUFFIX)))
            .status()
            .expect("rustc must be available to build the stand-in Copilot");
        assert!(compiled.success(), "stand-in Copilot failed to compile");
        let path = std::env::join_paths(std::iter::once(bin.clone()).chain(std::env::split_paths(
            &std::env::var_os("PATH").unwrap_or_default(),
        )))
        .unwrap();
        std::env::set_var("PATH", &path);
        std::env::set_var("CST_E2E_LOG", &log);

        let (tx, rx) = mpsc::channel();
        let mut pane = Pane::spawn(
            PaneSpec {
                id: 1,
                title: "e2e driver".to_string(),
                cwd: sandbox.path().to_path_buf(),
                session_id: "e2e-driver".to_string(),
                program: cst.to_string_lossy().into_owned(),
                args: vec![
                    "--mux".to_string(),
                    "--copilot-home".to_string(),
                    home.to_string_lossy().into_owned(),
                ],
                events_path: None,
                terminal_light_mode: Some(false),
                hooks_active: false,
            },
            35,
            120,
            tx,
        )
        .expect("cst must start inside the PTY");

        fn wait_until(
            pane: &Pane,
            rx: &Receiver<MuxEvent>,
            what: &str,
            done: impl Fn(&vt100::Screen) -> bool,
        ) {
            let deadline = Instant::now() + Duration::from_secs(30);
            while Instant::now() < deadline {
                let _ = rx.recv_timeout(Duration::from_millis(200));
                if pane.with_screen(&done).unwrap() {
                    return;
                }
            }
            let frame = pane.with_screen(|screen| screen.contents()).unwrap();
            panic!("never saw {what}; last frame:\n{frame}");
        }
        fn switcher(screen: &vt100::Screen) -> Vec<String> {
            popup_rows(screen, "Sessions").unwrap_or_default()
        }
        fn show(pane: &Pane, step: &str) {
            println!("── {step}");
            for row in pane.with_screen(switcher).unwrap() {
                println!("│{row}");
            }
        }
        fn shows(rows: &[String], text: &str) -> bool {
            rows.iter().any(|row| row.contains(text))
        }
        let key = |code| KeyEvent::new(code, KeyModifiers::NONE);
        let prefix = KeyEvent::new(KeyCode::Char('b'), KeyModifiers::CONTROL);
        let resumed = |log: &std::path::Path| {
            std::fs::read_to_string(log)
                .unwrap_or_default()
                .lines()
                .filter(|line| line.contains("--resume="))
                .count()
        };
        let type_text = |pane: &mut Pane, text: &str| {
            for character in text.chars() {
                pane.send_key(&key(KeyCode::Char(character))).unwrap();
            }
        };

        wait_until(&pane, &rx, "the session list", |screen| {
            screen.contents().contains("Refactor payment api")
        });
        if pane
            .with_screen(|screen| screen.contents())
            .unwrap()
            .contains("What's new")
        {
            pane.send_key(&key(KeyCode::Esc)).unwrap();
            wait_until(&pane, &rx, "What's new close", |screen| {
                !screen.contents().contains("What's new")
            });
        }

        // Resume each session from the list, coming back to it in between.
        for index in 0..sessions.len() {
            for _ in 0..sessions.len() {
                pane.send_key(&key(KeyCode::Up)).unwrap();
            }
            for _ in 0..index {
                pane.send_key(&key(KeyCode::Down)).unwrap();
            }
            pane.send_key(&key(KeyCode::Enter)).unwrap();
            let deadline = Instant::now() + Duration::from_secs(30);
            while resumed(&log) <= index {
                assert!(Instant::now() < deadline, "session {index} never resumed");
                let _ = rx.recv_timeout(Duration::from_millis(200));
            }
            wait_until(&pane, &rx, "the resumed session", |screen| {
                screen.contents().contains("FAKE COPILOT RESUMED")
            });
            if index + 1 < sessions.len() {
                pane.send_key(&prefix).unwrap();
                pane.send_key(&key(KeyCode::Char('d'))).unwrap();
                wait_until(&pane, &rx, "the list again", |screen| {
                    !screen.contents().contains("FAKE COPILOT RESUMED")
                });
            }
        }

        pane.send_key(&prefix).unwrap();
        pane.send_key(&key(KeyCode::Char('w'))).unwrap();
        wait_until(&pane, &rx, "the switcher with every session", |screen| {
            let rows = switcher(screen);
            sessions.iter().all(|(_, name, _)| shows(&rows, name))
        });
        show(&pane, "prefix w");

        type_text(&mut pane, "notes");
        wait_until(&pane, &rx, "only the release notes", |screen| {
            let rows = switcher(screen);
            shows(&rows, "Filter: notes")
                && shows(&rows, "Write release notes")
                && !shows(&rows, "Refactor payment api")
                && !shows(&rows, "Polish landing page")
        });
        show(&pane, "typed 'notes'");

        // x and q were commands here; now they are letters that match nothing.
        for _ in 0.."notes".len() {
            pane.send_key(&key(KeyCode::Backspace)).unwrap();
        }
        type_text(&mut pane, "xq");
        wait_until(&pane, &rx, "no matches", |screen| {
            let rows = switcher(screen);
            shows(&rows, "Filter: xq") && shows(&rows, "No matching sessions")
        });
        show(&pane, "typed 'xq'");

        // Narrow by project rather than title, then open the match.
        pane.send_key(&key(KeyCode::Backspace)).unwrap();
        pane.send_key(&key(KeyCode::Backspace)).unwrap();
        type_text(&mut pane, "websi");
        wait_until(&pane, &rx, "only the website session", |screen| {
            let rows = switcher(screen);
            shows(&rows, "Polish landing page") && !shows(&rows, "Write release notes")
        });
        show(&pane, "typed 'websi'");
        pane.send_key(&key(KeyCode::Enter)).unwrap();
        wait_until(&pane, &rx, "the website session attached", |screen| {
            screen
                .contents()
                .contains(&format!("FAKE COPILOT RESUMED {}", sessions[1].0))
        });
        println!("── Enter opened {}", sessions[1].1);
        assert_eq!(resumed(&log), 3, "nothing was started or ended by typing");

        // Down moves within the matches; Delete closes the highlighted one.
        pane.send_key(&prefix).unwrap();
        pane.send_key(&key(KeyCode::Char('w'))).unwrap();
        type_text(&mut pane, "a");
        pane.send_key(&key(KeyCode::Down)).unwrap();
        wait_until(&pane, &rx, "the switcher filtered to 'a'", |screen| {
            shows(&switcher(screen), "Filter: a█")
        });
        show(&pane, "typed 'a', pressed Down");
        pane.send_key(&key(KeyCode::Backspace)).unwrap();
        type_text(&mut pane, "pay");
        pane.send_key(&key(KeyCode::Delete)).unwrap();
        wait_until(&pane, &rx, "the payments session closed", |screen| {
            let rows = switcher(screen);
            shows(&rows, "Filter: pay") && shows(&rows, "No matching sessions")
        });
        show(&pane, "typed 'pay', pressed Delete");

        pane.send_key(&key(KeyCode::Esc)).unwrap();
        // ESC followed at once by the prefix's 0x02 reads as one Alt+Ctrl+B, and the
        // switcher never sees an Esc at all; no person types them that close together.
        wait_until(&pane, &rx, "the switcher closed by Esc", |screen| {
            popup_rows(screen, "Sessions").is_none()
        });
        pane.send_key(&prefix).unwrap();
        pane.send_key(&key(KeyCode::Char('w'))).unwrap();
        wait_until(&pane, &rx, "two sessions left, unfiltered", |screen| {
            let rows = switcher(screen);
            shows(&rows, "Filter: █")
                && !shows(&rows, "Refactor payment api")
                && shows(&rows, "Polish landing page")
                && shows(&rows, "Write release notes")
        });
        show(&pane, "reopened after Esc");

        // Close the rest, so this CST leaves no workspace to recover.
        pane.send_key(&key(KeyCode::Delete)).unwrap();
        pane.send_key(&key(KeyCode::Delete)).unwrap();
        wait_until(
            &pane,
            &rx,
            "the switcher gone with the last session",
            |screen| popup_rows(screen, "Sessions").is_none(),
        );
        std::thread::sleep(Duration::from_secs(2));
        let _ = pane.kill();
        let sandbox_path = sandbox.path().to_string_lossy().replace('\\', "\\\\");
        if let Ok(entries) = std::fs::read_dir(crate::recovery::root()) {
            for entry in entries.flatten() {
                let leaked = std::fs::read_to_string(entry.path())
                    .is_ok_and(|record| record.contains(&sandbox_path));
                assert!(
                    !leaked,
                    "left a workspace record: {}",
                    entry.path().display()
                );
            }
        }
    }

    #[test]
    fn a_dead_pane_offers_its_session_for_restart_keeping_its_slot_in_the_tab_strip() {
        let mut app = attached_mux_app("one");
        push_test_pane(&mut app, 2, "crashed");
        push_test_pane(&mut app, 3, "three");
        app.mux.as_mut().unwrap().focused = Some(2);

        handle_mux_event(&mut app, MuxEvent::Exited(2, Some(134)));

        let target = restart_target(&app).expect("an exited pane is restartable");
        assert_eq!(target.session_id, "crashed");
        assert_eq!(target.title, "Test session 2");
        assert_eq!(
            target.pane_id, 2,
            "the dead pane is the one replaced, so the new one takes its place"
        );
        let _ = app.mux.as_mut().unwrap().shutdown();
    }
    fn tab_order(app: &App) -> Vec<String> {
        app.mux
            .as_ref()
            .unwrap()
            .panes
            .iter()
            .map(|pane| pane.session_id.clone())
            .collect()
    }

    fn three_tab_app() -> App {
        let mut app = attached_mux_app("one");
        push_test_pane(&mut app, 2, "two");
        push_test_pane(&mut app, 3, "three");
        app.mux.as_mut().unwrap().focused = Some(1);
        app.view = View::Attached(1);
        app
    }

    #[test]
    fn move_tab_mode_stays_open_so_repeated_steps_cost_one_key() {
        let mut app = three_tab_app();
        assert_eq!(tab_order(&app), ["one", "two", "three"]);

        send_prefix_command(&mut app, 'm');
        assert_eq!(
            app.mux.as_ref().unwrap().prefix_state,
            PrefixState::Transient(TransientMode::MoveTab)
        );

        // Two steps, no second prefix — the point of the mode.
        handle_attached_key(&mut app, KeyEvent::new(KeyCode::Right, KeyModifiers::NONE));
        assert_eq!(tab_order(&app), ["two", "one", "three"]);
        handle_attached_key(&mut app, KeyEvent::new(KeyCode::Right, KeyModifiers::NONE));
        assert_eq!(tab_order(&app), ["two", "three", "one"]);

        handle_attached_key(&mut app, KeyEvent::new(KeyCode::Left, KeyModifiers::NONE));
        assert_eq!(tab_order(&app), ["two", "one", "three"]);

        handle_attached_key(&mut app, KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE));
        assert_eq!(app.mux.as_ref().unwrap().prefix_state, PrefixState::Idle);
    }

    #[test]
    fn the_moved_tab_keeps_focus_and_stops_at_the_ends() {
        let mut app = three_tab_app();
        send_prefix_command(&mut app, 'm');

        handle_attached_key(&mut app, KeyEvent::new(KeyCode::Left, KeyModifiers::NONE));
        assert_eq!(
            tab_order(&app),
            ["one", "two", "three"],
            "the first tab does not wrap around to the end"
        );
        assert!(app.status_message.as_deref().unwrap().contains("first"));

        for _ in 0..5 {
            handle_attached_key(&mut app, KeyEvent::new(KeyCode::Right, KeyModifiers::NONE));
        }
        assert_eq!(tab_order(&app), ["two", "three", "one"]);
        assert_eq!(
            app.mux.as_ref().unwrap().focused,
            Some(1),
            "the tab that moved is still the focused one"
        );
    }

    #[test]
    fn a_key_the_mode_does_not_claim_closes_it_rather_than_being_eaten() {
        let mut app = three_tab_app();
        send_prefix_command(&mut app, 'm');

        handle_attached_key(
            &mut app,
            KeyEvent::new(KeyCode::Char('z'), KeyModifiers::NONE),
        );

        assert_eq!(app.mux.as_ref().unwrap().prefix_state, PrefixState::Idle);
        assert_eq!(tab_order(&app), ["one", "two", "three"]);
    }

    #[test]
    fn dragging_a_tab_reorders_it_as_the_pointer_crosses_its_neighbours() {
        let mut app = three_tab_app();
        app.workspace_areas.tabs = Rect::new(0, 0, 80, 1);
        let column_of = |app: &App, index: usize| {
            let mux = app.mux.as_ref().unwrap();
            (0..80u16)
                .find(|column| {
                    crate::ui::pane::tab_index_at(mux, app.workspace_areas.tabs, *column, 0)
                        == Some(index)
                })
                .expect("tab is on the strip")
        };

        let first = column_of(&app, 0);
        handle_attached_mouse(&mut app, MouseEventKind::Down(MouseButton::Left), first, 0);
        assert_eq!(app.dragging_tab, Some(1));

        let third = column_of(&app, 2);
        handle_attached_mouse(&mut app, MouseEventKind::Drag(MouseButton::Left), third, 0);
        assert_eq!(tab_order(&app), ["two", "three", "one"]);

        handle_attached_mouse(&mut app, MouseEventKind::Up(MouseButton::Left), third, 0);
        assert_eq!(app.dragging_tab, None);
    }

    #[test]
    fn a_click_on_a_tab_switches_without_reordering_anything() {
        let mut app = three_tab_app();
        app.workspace_areas.tabs = Rect::new(0, 0, 80, 1);
        let mux = app.mux.as_ref().unwrap();
        let second = (0..80u16)
            .find(|column| {
                crate::ui::pane::tab_index_at(mux, app.workspace_areas.tabs, *column, 0) == Some(1)
            })
            .expect("second tab is on the strip");

        handle_attached_mouse(&mut app, MouseEventKind::Down(MouseButton::Left), second, 0);
        handle_attached_mouse(&mut app, MouseEventKind::Up(MouseButton::Left), second, 0);

        assert_eq!(tab_order(&app), ["one", "two", "three"]);
        assert_eq!(app.mux.as_ref().unwrap().focused, Some(2));
    }

    fn handle_attached_mouse(app: &mut App, kind: MouseEventKind, column: u16, row: u16) {
        handle_attached_event(
            app,
            Event::Mouse(crossterm::event::MouseEvent {
                kind,
                column,
                row,
                modifiers: KeyModifiers::NONE,
            }),
        );
    }
    #[test]
    fn dragging_against_the_edge_scrolls_a_strip_too_narrow_to_show_every_tab() {
        let mut app = attached_mux_app("s1");
        for id in 2..=10u64 {
            push_test_pane(&mut app, id, &format!("s{id}"));
        }
        app.mux.as_mut().unwrap().focused = Some(1);
        // Narrow enough that the strip windows: only two tabs are ever drawn, so the
        // pointer runs out of room long before the tab runs out of places to go.
        app.workspace_areas.tabs = Rect::new(0, 0, 40, 1);
        let right_edge = app.workspace_areas.tabs.right() - 1;

        handle_attached_mouse(&mut app, MouseEventKind::Down(MouseButton::Left), 0, 0);
        for _ in 0..12 {
            handle_attached_mouse(
                &mut app,
                MouseEventKind::Drag(MouseButton::Left),
                right_edge,
                0,
            );
        }

        assert_eq!(
            tab_order(&app),
            ["s2", "s3", "s4", "s5", "s6", "s7", "s8", "s9", "s10", "s1"],
            "holding at the edge walks the tab past the tabs it could never see"
        );
        assert_eq!(
            app.mux.as_ref().unwrap().focused,
            Some(1),
            "and it is still the tab the user grabbed"
        );
        let _ = app.mux.as_mut().unwrap().shutdown();
    }
    #[test]
    fn move_mode_refuses_to_open_when_there_is_only_one_session() {
        let mut app = attached_mux_app("alone");

        send_prefix_command(&mut app, 'm');

        assert_eq!(
            app.mux.as_ref().unwrap().prefix_state,
            PrefixState::Idle,
            "the arrows must keep reaching Copilot when the mode could do nothing"
        );
        assert!(app
            .status_message
            .as_deref()
            .unwrap()
            .contains("one session"));
    }

    #[test]
    fn the_prefix_chord_still_reaches_the_command_menu_from_inside_move_mode() {
        let mut app = three_tab_app();
        send_prefix_command(&mut app, 'm');

        handle_attached_key(
            &mut app,
            KeyEvent::new(KeyCode::Char('b'), KeyModifiers::CONTROL),
        );

        assert_eq!(app.mux.as_ref().unwrap().prefix_state, PrefixState::Root);
        assert_eq!(tab_order(&app), ["one", "two", "three"]);
    }
}

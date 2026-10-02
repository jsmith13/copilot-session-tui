use ratatui::layout::{Constraint, Direction, Layout, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Clear, List, ListItem, Paragraph, Wrap};
use ratatui::Frame;

use crate::app::{App, DeleteTarget, SettingsEditField, SettingsSection};
use crate::theme::{fill_area, Theme, ThemeName};

pub(crate) fn surface_style(theme: Theme) -> Style {
    Style::default().fg(theme.text).bg(theme.surface)
}

pub(crate) fn prepare_popup(f: &mut Frame, area: Rect, theme: Theme) {
    f.render_widget(Clear, area);
    fill_area(f.buffer_mut(), area, theme.surface);
}

pub(crate) fn centered_rect(percent_x: u16, percent_y: u16, area: Rect) -> Rect {
    let popup_layout = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Percentage((100 - percent_y) / 2),
            Constraint::Percentage(percent_y),
            Constraint::Percentage((100 - percent_y) / 2),
        ])
        .split(area);

    Layout::default()
        .direction(Direction::Horizontal)
        .constraints([
            Constraint::Percentage((100 - percent_x) / 2),
            Constraint::Percentage(percent_x),
            Constraint::Percentage((100 - percent_x) / 2),
        ])
        .split(popup_layout[1])[1]
}

/// Warn before quitting while sessions are still running.
pub fn draw_quit_confirm(f: &mut Frame, app: &App) {
    let theme = app.theme();
    let running: Vec<(String, bool)> = app
        .mux
        .as_ref()
        .map(|mux| {
            mux.panes
                .iter()
                .filter(|pane| pane.is_running())
                .map(|pane| (pane.title.clone(), app.pane_is_tmux_backed(pane)))
                .collect()
        })
        .unwrap_or_default();
    let persistent = running.iter().filter(|(_, tmux)| *tmux).count();
    let ending = running.len().saturating_sub(persistent);

    let height = (running.len() + 8).min(20) as u16;
    let percent_y = ((height as f32 / f.area().height as f32) * 100.0).min(70.0) as u16;
    let area = centered_rect(60, percent_y.max(30), f.area());
    prepare_popup(f, area, theme);

    let mut text = vec![
        Line::from(""),
        Line::from(Span::styled(
            format!("  Quit with {} running session(s)?", running.len()),
            Style::default().fg(theme.text).add_modifier(Modifier::BOLD),
        )),
        Line::from(""),
    ];
    for (title, tmux) in running.iter().take(8) {
        let suffix = if *tmux { " (tmux: keeps running)" } else { "" };
        text.push(Line::from(Span::styled(
            format!("    • {title}{suffix}"),
            Style::default().fg(theme.accent_alt),
        )));
    }

    if running.len() > 8 {
        text.push(Line::from(Span::styled(
            format!("    … and {} more", running.len() - 8),
            Style::default().fg(theme.muted),
        )));
    }
    text.push(Line::from(""));
    text.push(Line::from(Span::styled(
        format!(
            "  {ending} CST-owned session(s) will end; {persistent} tmux session(s) will detach."
        ),
        Style::default().fg(theme.warning),
    )));
    text.push(Line::from(""));
    text.push(Line::from(vec![
        Span::raw("  "),
        Span::styled(
            "y",
            Style::default()
                .fg(theme.error)
                .add_modifier(Modifier::BOLD),
        ),
        Span::raw(" quit and end them    "),
        Span::styled(
            "n",
            Style::default()
                .fg(theme.success)
                .add_modifier(Modifier::BOLD),
        ),
        Span::raw(" keep working"),
    ]));

    let block = Block::default()
        .title(" Quit ")
        .borders(Borders::ALL)
        .style(surface_style(theme))
        .border_style(Style::default().fg(theme.error));
    let paragraph = Paragraph::new(text)
        .style(surface_style(theme))
        .block(block)
        .wrap(Wrap { trim: false });
    f.render_widget(paragraph, area);
}

pub fn draw_end_tmux_confirm(f: &mut Frame, app: &App) {
    let Some(reference) = app.confirm_end_tmux.as_ref() else {
        return;
    };
    let theme = app.theme();
    let area = centered_rect(62, 38, f.area());
    prepare_popup(f, area, theme);
    let title = app
        .sessions
        .iter()
        .find(|session| session.id == reference.session_id)
        .map(|session| session.display_name())
        .unwrap_or("(unnamed)");
    let text = vec![
        Line::from(""),
        Line::from(Span::styled(
            "  End this persistent tmux session?",
            Style::default()
                .fg(theme.error)
                .add_modifier(Modifier::BOLD),
        )),
        Line::from(""),
        Line::from(format!("  {title}")),
        Line::from(Span::styled(
            format!("  tmux: {}", reference.tmux_session),
            Style::default().fg(theme.accent_alt),
        )),
        Line::from(""),
        Line::from(Span::styled(
            "  Copilot will stop. Session history and any managed worktree remain.",
            Style::default().fg(theme.warning),
        )),
        Line::from(""),
        Line::from(vec![
            Span::raw("  "),
            Span::styled(
                "y",
                Style::default()
                    .fg(theme.error)
                    .add_modifier(Modifier::BOLD),
            ),
            Span::raw(" end permanently    "),
            Span::styled("n/Esc", Style::default().fg(theme.accent_alt)),
            Span::raw(" keep running"),
        ]),
    ];
    f.render_widget(
        Paragraph::new(text)
            .style(surface_style(theme))
            .block(
                Block::default()
                    .title(" End Persistent Session ")
                    .borders(Borders::ALL)
                    .border_style(Style::default().fg(theme.error)),
            )
            .wrap(Wrap { trim: true }),
        area,
    );
}

pub fn draw_update_restart_confirm(f: &mut Frame, app: &App) {
    let theme = app.theme();
    let running = app.update_restart_titles();
    let height = (running.len() + 11).min(22) as u16;
    let percent_y = ((height as f32 / f.area().height as f32) * 100.0).min(75.0) as u16;
    let area = centered_rect(66, percent_y.max(38), f.area());
    prepare_popup(f, area, theme);

    let mut text = vec![
        Line::from(""),
        Line::from(Span::styled(
            format!(
                "  Update CST and restart {} running session{}?",
                running.len(),
                if running.len() == 1 { "" } else { "s" }
            ),
            Style::default().fg(theme.text).add_modifier(Modifier::BOLD),
        )),
        Line::from(""),
    ];
    for title in running.iter().take(6) {
        text.push(Line::from(Span::styled(
            format!("    • {title}"),
            Style::default().fg(theme.accent_alt),
        )));
    }
    if running.len() > 6 {
        text.push(Line::from(Span::styled(
            format!("    … and {} more", running.len() - 6),
            Style::default().fg(theme.muted),
        )));
    }
    text.extend([
        Line::from(""),
        Line::from(Span::styled(
            "  CST installs the update first, then stops only these sessions.",
            Style::default().fg(theme.warning),
        )),
        Line::from(Span::styled(
            "  Running Copilot chats reopen in the same order and focus.",
            Style::default().fg(theme.muted),
        )),
        Line::from(Span::styled(
            "  Copilot work and embedded shell jobs are interrupted on restart.",
            Style::default().fg(theme.muted),
        )),
        Line::from(""),
        Line::from(vec![
            Span::raw("  "),
            Span::styled(
                "y/Enter",
                Style::default()
                    .fg(theme.warning)
                    .add_modifier(Modifier::BOLD),
            ),
            Span::raw(" update and restart    "),
            Span::styled(
                "n/Esc",
                Style::default()
                    .fg(theme.success)
                    .add_modifier(Modifier::BOLD),
            ),
            Span::raw(" cancel"),
        ]),
    ]);

    let block = Block::default()
        .title(" Update & Restart ")
        .borders(Borders::ALL)
        .style(surface_style(theme))
        .border_style(Style::default().fg(theme.warning));
    f.render_widget(
        Paragraph::new(text)
            .style(surface_style(theme))
            .block(block)
            .wrap(Wrap { trim: false }),
        area,
    );
}

/// Switcher over the panes this CST instance owns, opened with `prefix w`.
/// Last path component of a pane's working directory, to disambiguate same-named
/// sessions living in different projects.
fn project_label(cwd: &std::path::Path) -> String {
    cwd.file_name()
        .map(|name| name.to_string_lossy().to_string())
        .unwrap_or_default()
}

/// Blocking-work notice, drawn on the frame before the work actually starts.
pub fn draw_busy(f: &mut Frame, title: &str, detail: &str, theme: Theme) {
    let area = centered_rect(50, 20, f.area());
    prepare_popup(f, area, theme);

    let block = Block::default()
        .title(format!(" {title} "))
        .borders(Borders::ALL)
        .style(surface_style(theme))
        .border_style(Style::default().fg(theme.accent_alt));
    let inner = block.inner(area);
    f.render_widget(block, area);

    let text = vec![
        Line::from(""),
        Line::from(Span::styled(
            format!("  ⠿  {detail}"),
            Style::default()
                .fg(theme.accent_alt)
                .add_modifier(Modifier::BOLD),
        )),
        Line::from(""),
        Line::from(Span::styled(
            "  This can take a few seconds.",
            Style::default().fg(theme.muted),
        )),
    ];
    f.render_widget(
        Paragraph::new(text)
            .style(surface_style(theme))
            .wrap(Wrap { trim: true }),
        inner,
    );
}

pub fn draw_pane_list(f: &mut Frame, app: &App) {
    let Some(mux) = app.mux.as_ref() else {
        return;
    };
    let theme = app.theme();

    let height = (mux.panes.len() + 5).min(20) as u16;
    let percent_y = ((height as f32 / f.area().height as f32) * 100.0).min(70.0) as u16;
    let area = centered_rect(60, percent_y.max(30), f.area());
    prepare_popup(f, area, theme);

    let block = Block::default()
        .title(" Sessions ")
        .borders(Borders::ALL)
        .style(surface_style(theme))
        .border_style(Style::default().fg(theme.accent));
    let inner = block.inner(area);
    f.render_widget(block, area);

    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([Constraint::Min(1), Constraint::Length(1)])
        .split(inner);

    let items: Vec<ListItem> = mux
        .panes
        .iter()
        .enumerate()
        .map(|(index, pane)| {
            let title = app.pane_session_title(&pane.session_id, &pane.title);
            let mut title = if pane.needs_attention() {
                format!("? {title}")
            } else {
                title.to_string()
            };
            if app.pane_is_tmux_backed(pane) {
                title.push_str(" [tmux]");
            }
            let selected = index == app.pane_selected;
            let base = if selected {
                super::row_selection_style(theme)
            } else {
                Style::default().fg(theme.text).bg(theme.surface)
            };
            let (marker, marker_color) = if !pane.is_running() {
                ("✖", theme.error)
            } else if mux.focused == Some(pane.id) {
                ("●", theme.success)
            } else {
                ("○", theme.inactive)
            };
            ListItem::new(Line::from(vec![
                Span::styled(
                    format!(" {} ", index + 1),
                    if selected {
                        base
                    } else {
                        base.fg(theme.accent_alt)
                    },
                ),
                Span::styled(
                    marker,
                    if selected {
                        base
                    } else {
                        base.fg(marker_color)
                    },
                ),
                Span::styled(format!(" {title}"), base),
                // Panes routinely span several projects, so the title alone is ambiguous.
                Span::styled(
                    format!("  {}", project_label(&pane.cwd)),
                    if selected { base } else { base.fg(theme.muted) },
                ),
            ]))
        })
        .collect();

    f.render_widget(List::new(items).style(surface_style(theme)), chunks[0]);

    let hint = Line::from(vec![
        Span::raw(" "),
        Span::styled("↑↓", Style::default().fg(theme.accent_alt)),
        Span::raw(" select  "),
        Span::styled("Enter", Style::default().fg(theme.accent_alt)),
        Span::raw(" attach  "),
        Span::styled("x", Style::default().fg(theme.accent_alt)),
        Span::raw(" close  "),
        Span::styled(
            app.config.tmux_keys.end_session.clone(),
            Style::default().fg(theme.accent_alt),
        ),
        Span::raw(" end tmux  "),
        Span::styled("Esc", Style::default().fg(theme.accent_alt)),
        Span::raw(" close"),
    ]);
    f.render_widget(Paragraph::new(hint).style(surface_style(theme)), chunks[1]);
}

pub fn draw_delete_confirm(f: &mut Frame, app: &App) {
    let theme = app.theme();
    let area = centered_rect(55, 36, f.area());
    prepare_popup(f, area, theme);

    let name = app
        .selected_session()
        .map(|s| s.display_name().to_string())
        .unwrap_or_default();

    let managed = matches!(app.pending_delete, Some(DeleteTarget::Managed { .. }));
    let dirty = matches!(
        app.pending_delete,
        Some(DeleteTarget::Managed { dirty: true, .. })
    );
    let action = if managed {
        "  Delete this session and its TUI-managed worktree?"
    } else {
        "  Delete this session?"
    };

    let mut text = vec![
        Line::from(""),
        Line::from(Span::styled(
            action,
            Style::default()
                .fg(theme.error)
                .add_modifier(Modifier::BOLD),
        )),
        Line::from(""),
        Line::from(format!("  {}", name)),
        Line::from(""),
    ];
    if managed {
        text.push(Line::from(Span::styled(
            "  The worktree will be removed before session metadata.",
            Style::default().fg(theme.warning),
        )));
    }
    if dirty {
        text.push(Line::from(Span::styled(
            "  This worktree is dirty; another force confirmation follows.",
            Style::default().fg(theme.error),
        )));
    }
    text.extend([
        Line::from(""),
        Line::from(vec![
            Span::raw("  "),
            Span::styled(
                "y",
                Style::default()
                    .fg(theme.success)
                    .add_modifier(Modifier::BOLD),
            ),
            Span::raw(" Yes  "),
            Span::styled(
                "any key",
                Style::default()
                    .fg(theme.error)
                    .add_modifier(Modifier::BOLD),
            ),
            Span::raw(" Cancel"),
        ]),
    ]);

    let block = Block::default()
        .title(" Confirm Delete ")
        .borders(Borders::ALL)
        .style(surface_style(theme))
        .border_style(Style::default().fg(theme.error));

    let paragraph = Paragraph::new(text)
        .style(surface_style(theme))
        .block(block)
        .wrap(Wrap { trim: false });
    f.render_widget(paragraph, area);
}

pub fn draw_force_delete_confirm(f: &mut Frame, app: &App) {
    let theme = app.theme();
    let area = centered_rect(60, 36, f.area());
    prepare_popup(f, area, theme);
    let path = match &app.pending_delete {
        Some(DeleteTarget::Managed { entry, .. }) => entry.path.display().to_string(),
        _ => String::new(),
    };
    let text = vec![
        Line::from(""),
        Line::from(Span::styled(
            "  FORCE REMOVE DIRTY WORKTREE?",
            Style::default()
                .fg(theme.error)
                .add_modifier(Modifier::BOLD),
        )),
        Line::from(""),
        Line::from(format!("  {path}")),
        Line::from(""),
        Line::from(Span::styled(
            "  Modified, staged, and untracked files will be permanently lost.",
            Style::default().fg(theme.error),
        )),
        Line::from(""),
        Line::from(vec![
            Span::raw("  Press "),
            Span::styled(
                "Shift+Y",
                Style::default()
                    .fg(theme.error)
                    .add_modifier(Modifier::BOLD),
            ),
            Span::raw(" to force delete; any other key cancels"),
        ]),
    ];
    let block = Block::default()
        .title(" Destructive Confirmation ")
        .borders(Borders::ALL)
        .style(surface_style(theme))
        .border_style(Style::default().fg(theme.error));
    f.render_widget(
        Paragraph::new(text)
            .style(surface_style(theme))
            .block(block)
            .wrap(Wrap { trim: false }),
        area,
    );
}

pub fn draw_takeover_confirm(f: &mut Frame, app: &App) {
    let Some(target) = app.pending_takeover.as_ref() else {
        return;
    };
    let theme = app.theme();
    let area = centered_rect(62, 50, f.area());
    prepare_popup(f, area, theme);
    let block = Block::default()
        .title(" Take Over Active Session? ")
        .borders(Borders::ALL)
        .style(surface_style(theme))
        .border_style(Style::default().fg(theme.warning));
    let inner = block.inner(area);
    f.render_widget(block, area);
    let owners = if target.pids.len() == 1 {
        format!("Copilot PID {}", target.pids[0])
    } else {
        format!("{} Copilot processes", target.pids.len())
    };
    f.render_widget(
        Paragraph::new(vec![
            Line::from(""),
            Line::from(vec![
                Span::raw("  "),
                Span::styled(
                    &target.title,
                    Style::default().fg(theme.text).add_modifier(Modifier::BOLD),
                ),
            ]),
            Line::from(""),
            Line::from(Span::styled(
                format!("  This session is active in another process ({owners})."),
                Style::default().fg(theme.warning),
            )),
            Line::from(Span::styled(
                "  Taking over ends that exact Copilot process, then resumes the",
                Style::default().fg(theme.muted),
            )),
            Line::from(Span::styled(
                "  session in this CST instance. In-flight work will be interrupted.",
                Style::default().fg(theme.muted),
            )),
            Line::from(""),
            Line::from(vec![
                Span::raw("  "),
                Span::styled(
                    "y/Enter",
                    Style::default()
                        .fg(theme.error)
                        .add_modifier(Modifier::BOLD),
                ),
                Span::raw(" take over    "),
                Span::styled(
                    "n/Esc",
                    Style::default()
                        .fg(theme.success)
                        .add_modifier(Modifier::BOLD),
                ),
                Span::raw(" cancel"),
            ]),
        ])
        .style(surface_style(theme))
        .wrap(Wrap { trim: false }),
        inner,
    );
}

pub fn draw_rename(f: &mut Frame, app: &App) {
    let theme = app.theme();
    let area = centered_rect(50, 20, f.area());
    prepare_popup(f, area, theme);

    let text = vec![
        Line::from(""),
        Line::from(Span::styled(
            "  Enter new name:",
            Style::default()
                .fg(theme.warning)
                .add_modifier(Modifier::BOLD),
        )),
        Line::from(""),
        Line::from(vec![
            Span::raw("  "),
            Span::styled(&app.rename_input, Style::default().fg(theme.text)),
            Span::styled("█", Style::default().fg(theme.warning)),
        ]),
        Line::from(""),
        Line::from(vec![
            Span::raw("  "),
            Span::styled("Enter", Style::default().fg(theme.accent_alt)),
            Span::raw(" Save  "),
            Span::styled("Esc", Style::default().fg(theme.accent_alt)),
            Span::raw(" Cancel"),
        ]),
    ];

    let block = Block::default()
        .title(" Rename Session ")
        .borders(Borders::ALL)
        .style(surface_style(theme))
        .border_style(Style::default().fg(theme.warning));

    let paragraph = Paragraph::new(text)
        .style(surface_style(theme))
        .block(block);
    f.render_widget(paragraph, area);
}

pub fn draw_project_filter(f: &mut Frame, app: &App) {
    let theme = app.theme();
    let filtered = app.filtered_project_indices();
    let item_count = filtered.len() + 1; // +1 for "All Projects"
    let height = (item_count + 5).min(20) as u16; // +5 for search input, borders, padding
    let percent_y = ((height as f32 / f.area().height as f32) * 100.0).min(80.0) as u16;
    let area = centered_rect(50, percent_y.max(25), f.area());
    prepare_popup(f, area, theme);

    let block = Block::default()
        .title(" Select Project ")
        .borders(Borders::ALL)
        .style(surface_style(theme))
        .border_style(Style::default().fg(theme.accent_alt));

    let inner = block.inner(area);
    f.render_widget(block, area);

    // Split inner area: search input at top, list below
    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(1),
            Constraint::Length(1),
            Constraint::Min(1),
        ])
        .split(inner);

    // Search input
    let search_line = Line::from(vec![
        Span::styled(" 🔍 ", Style::default().fg(theme.warning)),
        Span::styled(&app.project_search_query, Style::default().fg(theme.text)),
        Span::styled("█", Style::default().fg(theme.warning)),
    ]);
    f.render_widget(
        Paragraph::new(search_line).style(surface_style(theme)),
        chunks[0],
    );

    // Separator
    let sep = Line::from(Span::styled(
        "─".repeat(chunks[1].width as usize),
        Style::default().fg(theme.muted),
    ));
    f.render_widget(Paragraph::new(sep).style(surface_style(theme)), chunks[1]);

    // Project list
    let has_all_option = app.project_search_query.is_empty();
    let visible_rows = chunks[2].height as usize;

    // Build all logical items with their indices
    let mut all_items: Vec<(usize, ListItem)> = Vec::new();

    // "All Projects" option (only shown when search is empty)
    if has_all_option {
        let all_style = if app.project_selected == 0 {
            super::row_selection_style(theme)
        } else {
            surface_style(theme)
        };
        all_items.push((
            0,
            ListItem::new(Line::from(Span::styled("  All Projects", all_style))),
        ));
    }

    for (list_i, &proj_idx) in filtered.iter().enumerate() {
        let project = &app.unique_projects[proj_idx];
        let item_index = if has_all_option { list_i + 1 } else { list_i };
        let is_selected = app.project_selected == item_index;
        let is_active = app.project_filter.as_deref() == Some(project.as_str());

        let prefix = if is_active { "● " } else { "  " };
        let name = std::path::Path::new(project)
            .file_name()
            .and_then(|n| n.to_str())
            .unwrap_or(project);

        let display = if app
            .cwd_project
            .as_deref()
            .is_some_and(|p| p.eq_ignore_ascii_case(project))
        {
            format!("{}{} (current dir)", prefix, name)
        } else {
            format!("{}{}", prefix, name)
        };

        let style = if is_selected {
            super::row_selection_style(theme)
        } else if is_active {
            surface_style(theme).fg(theme.accent_alt)
        } else {
            surface_style(theme)
        };

        all_items.push((
            item_index,
            ListItem::new(Line::from(Span::styled(display, style))),
        ));
    }

    let items: Vec<ListItem> = all_items
        .into_iter()
        .skip(app.project_scroll_offset)
        .take(visible_rows)
        .map(|(_, item)| item)
        .collect();

    let list = List::new(items).style(surface_style(theme));
    f.render_widget(list, chunks[2]);
}

pub fn draw_help(f: &mut Frame, app: &mut App) {
    let theme = app.theme();
    let area = centered_rect(55, 70, f.area());
    prepare_popup(f, area, theme);

    let mut text = vec![
        Line::from(""),
        Line::from(Span::styled(
            "  Copilot Session Manager - Keyboard Shortcuts",
            Style::default()
                .fg(theme.accent_alt)
                .add_modifier(Modifier::BOLD),
        )),
        Line::from(Span::styled(
            format!("  CST v{}", env!("CARGO_PKG_VERSION")),
            Style::default().fg(theme.muted),
        )),
        Line::from(""),
        help_line(theme, "↑/k ↓/j", "Navigate sessions"),
        help_line(theme, "Home/End", "Jump to first/last"),
        help_line(theme, "Enter", "Resume selected session"),
        help_line(
            theme,
            &app.config.tmux_keys.resume_session,
            "Resume selected session persistently via tmux",
        ),
        help_line(theme, "n", "New session in current project"),
        help_line(
            theme,
            &app.config.tmux_keys.new_session,
            "New persistent current-project session via tmux",
        ),
        help_line(theme, "N", "New isolated worktree session"),
        help_line(
            theme,
            &app.config.tmux_keys.new_worktree,
            "New persistent worktree session via tmux",
        ),
        help_line(
            theme,
            &app.config.tmux_keys.end_session,
            "End the selected persistent tmux session",
        ),
        help_line(theme, "Space", "Toggle selected session favorite"),
        help_line(theme, "g", "Grab a favorite, then ↑/↓ to reorder"),
        help_line(theme, "T", "Open favorites as panes or terminal tabs"),
        help_line(theme, "e", "Open selected session scratchpad"),
        help_line(theme, "r", "Rename selected session"),
        help_line(theme, "d", "Delete selected session"),
        help_line(theme, "◇ / ◆", "Persistent tmux session detached / open"),
        Line::from(""),
        help_line(theme, "/", "Search / fuzzy filter"),
        help_line(theme, "f/p", "Filter by project (type to search)"),
        help_line(theme, "c", "Clear project filter"),
        help_line(theme, "s", "Cycle sort order"),
        help_line(theme, "H", "Temporarily show/hide filtered sessions"),
        Line::from(""),
        help_line(theme, ",", "Global settings"),
        help_line(theme, ".", "Filtered-project settings"),
        help_line(theme, "?", "Toggle this help"),
        help_line(theme, "u", "Update (when available)"),
        help_line(theme, "q/Esc", "Quit"),
        help_line(theme, "Ctrl+C", "Force quit"),
        Line::from(""),
    ];

    if let Some(prefix) = app.prefix_label() {
        text.push(Line::from(Span::styled(
            format!("  Multiplexer (prefix {prefix})"),
            Style::default()
                .fg(theme.accent)
                .add_modifier(Modifier::BOLD),
        )));
        text.push(help_line(
            theme,
            &format!("{prefix} d"),
            "Back to the list (session keeps running)",
        ));
        text.push(help_line(theme, &format!("{prefix} w"), "Session switcher"));
        text.push(help_line(
            theme,
            &format!("{prefix} c"),
            "Focus the session chat",
        ));
        text.push(help_line(
            theme,
            &format!("{prefix} e"),
            "Toggle/focus the session scratchpad",
        ));
        text.push(help_line(
            theme,
            &format!("{prefix} t"),
            "Toggle/focus the session terminal",
        ));
        text.push(help_line(
            theme,
            &format!("{prefix} s"),
            "Open prompt snippets",
        ));
        text.push(help_line(
            theme,
            &format!("{prefix} u"),
            "Install update without stopping sessions",
        ));
        text.push(help_line(
            theme,
            &format!("{prefix} g i"),
            "Inspect a GitHub issue, pull request, or discussion",
        ));
        text.push(help_line(
            theme,
            &format!("{prefix} h e"),
            "Open scratchpad shortcut help",
        ));
        text.push(help_line(
            theme,
            &format!("{prefix} n/p"),
            "Next / previous session",
        ));
        text.push(help_line(
            theme,
            &format!("{prefix} 1-9"),
            "Jump to session by number",
        ));
        text.push(help_line(
            theme,
            &format!("{prefix} {}", app.config.tmux_keys.end_session),
            "End the persistent tmux session",
        ));
        text.push(help_line(
            theme,
            &format!("{prefix} x"),
            "End a CST session or detach a tmux-backed tab",
        ));
        text.push(help_line(
            theme,
            &format!("{prefix} q"),
            "Quit CST (persistent tmux sessions detach)",
        ));
        text.push(help_line(
            theme,
            &format!("{prefix} {prefix}"),
            "Search every CST command",
        ));
        text.push(help_line(
            theme,
            "?",
            "Copilot is waiting for a response or has completed work",
        ));
        text.push(help_line(theme, "", "These work from this list too"));
        text.push(Line::from(""));
        text.push(Line::from(Span::styled(
            "  GitHub references in the chat",
            Style::default()
                .fg(theme.accent)
                .add_modifier(Modifier::BOLD),
        )));
        text.extend(reference_legend(theme));
        text.push(Line::from(""));
    }

    text.push(Line::from(Span::styled(
        "  ↑/↓ PageUp/PageDown scroll · Esc/q/Enter/? close",
        Style::default().fg(theme.muted),
    )));

    // The list has outgrown the popup, so without scrolling the tail of it --
    // including the multiplexer bindings -- simply cannot be read.
    let viewport = area.height.saturating_sub(2) as usize;
    let max_scroll = text.len().saturating_sub(viewport);
    app.help_scroll = app.help_scroll.min(max_scroll);

    let title = if max_scroll > 0 {
        format!(" Help ({}/{}) ", app.help_scroll + 1, max_scroll + 1)
    } else {
        " Help ".to_string()
    };
    let block = Block::default()
        .title(title)
        .borders(Borders::ALL)
        .style(surface_style(theme))
        .border_style(Style::default().fg(theme.accent_alt));

    let paragraph = Paragraph::new(text)
        .style(surface_style(theme))
        .block(block)
        .scroll((u16::try_from(app.help_scroll).unwrap_or(u16::MAX), 0));
    f.render_widget(paragraph, area);
}

/// Shows what the colours on a `#1234` in the chat mean.
///
/// The kind lives in the hash and the state in the number, which is not
/// guessable, so the help screen spells it out.
fn reference_legend(theme: Theme) -> Vec<Line<'static>> {
    let sample = |hash: Color, number: Color, label: &str| -> Vec<Span<'static>> {
        let link = Modifier::UNDERLINED | Modifier::BOLD;
        vec![
            Span::styled("#", Style::default().fg(hash).add_modifier(link)),
            Span::styled("12", Style::default().fg(number).add_modifier(link)),
            Span::styled(format!(" {label}"), Style::default().fg(theme.text)),
        ]
    };

    let mut kinds = vec![Span::raw("  ")];
    kinds.extend(sample(theme.warning, theme.success, "issue     "));
    kinds.extend(sample(theme.accent_alt, theme.success, "pull request"));

    let mut states = vec![Span::raw("  ")];
    states.extend(sample(theme.accent_alt, theme.success, "open  "));
    states.extend(sample(theme.accent_alt, theme.error, "closed  "));
    states.extend(sample(theme.accent_alt, theme.accent, "merged  "));
    states.extend(sample(theme.accent_alt, theme.muted, "draft"));

    vec![Line::from(kinds), Line::from(states)]
}

fn help_line(theme: Theme, key: &str, desc: &str) -> Line<'static> {
    Line::from(vec![
        Span::raw("  "),
        Span::styled(
            format!("{:<12}", key),
            Style::default()
                .fg(theme.accent_alt)
                .add_modifier(Modifier::BOLD),
        ),
        Span::styled(desc.to_string(), Style::default().fg(theme.text)),
    ])
}

pub fn draw_settings(f: &mut Frame, app: &mut App) {
    let theme = app.theme();
    let area = centered_rect(65, 78, f.area());
    prepare_popup(f, area, theme);

    let block = Block::default()
        .title(" Global Settings ")
        .borders(Borders::ALL)
        .style(surface_style(theme))
        .border_style(Style::default().fg(theme.accent));

    let inner = block.inner(area);
    f.render_widget(block, area);
    let sections = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(1),
            Constraint::Length(1),
            Constraint::Min(1),
            Constraint::Length(1),
        ])
        .split(inner);
    let mut tab_spans = vec![Span::raw("  ")];
    for section in SettingsSection::ALL {
        let selected = section == app.settings_section;
        tab_spans.push(Span::styled(
            format!(" {} ", section.label()),
            if selected {
                Style::default()
                    .fg(theme.selection_fg)
                    .bg(theme.selection_bg)
                    .add_modifier(Modifier::BOLD)
            } else {
                Style::default().fg(theme.muted).bg(theme.surface)
            },
        ));
        tab_spans.push(Span::raw(" "));
    }
    f.render_widget(
        Paragraph::new(Line::from(tab_spans)).style(surface_style(theme)),
        sections[0],
    );

    let mut lines: Vec<Line> = Vec::new();
    let mut setting_lines = Vec::new();
    let general_start = lines.len();

    lines.push(Line::from(""));

    // Row 0: Yolo mode
    let yolo_value = if app.config.yolo { "ON" } else { "OFF" };
    let yolo_color = if app.config.yolo {
        theme.success
    } else {
        theme.error
    };
    setting_lines.push(lines.len());
    lines.push(settings_row(
        theme,
        "Yolo Mode",
        yolo_value,
        yolo_color,
        app.settings_selected == 0,
        false,
    ));
    lines.push(Line::from(Span::styled(
        "    Pass --yolo flag (allow all permissions)",
        Style::default().fg(theme.muted),
    )));

    lines.push(Line::from(""));

    // Row 1: Model
    let model_editing = app.settings_editing == Some(SettingsEditField::Model);
    let model_display = if model_editing {
        format!("{}█", app.settings_input)
    } else {
        app.config
            .model
            .as_deref()
            .unwrap_or("(default)")
            .to_string()
    };
    let model_color = if app.config.model.is_some() {
        theme.accent_alt
    } else {
        theme.muted
    };
    setting_lines.push(lines.len());
    lines.push(settings_row(
        theme,
        "Model",
        &model_display,
        model_color,
        app.settings_selected == 1,
        model_editing,
    ));
    lines.push(Line::from(Span::styled(
        "    Default for new sessions only (e.g. gpt-5.2, claude-sonnet-4)",
        Style::default().fg(theme.muted),
    )));

    lines.push(Line::from(""));

    // Row 2: Reasoning effort
    let effort_display = app
        .config
        .reasoning_effort
        .as_deref()
        .unwrap_or("(default)");
    let effort_color = if app.config.reasoning_effort.is_some() {
        theme.warning
    } else {
        theme.muted
    };
    setting_lines.push(lines.len());
    lines.push(settings_row(
        theme,
        "Reasoning Effort",
        effort_display,
        effort_color,
        app.settings_selected == 2,
        false,
    ));
    lines.push(Line::from(Span::styled(
        "    Default for new sessions only (low/medium/high/xhigh)",
        Style::default().fg(theme.muted),
    )));

    lines.push(Line::from(""));

    // Row 18: Max autopilot continues. Numbered after the Filters rows rather than
    // renumbering everything below it — the indices are keys, not positions.
    let continues_editing = app.settings_editing == Some(SettingsEditField::MaxAutopilotContinues);
    let continues_display = if continues_editing {
        format!("{}█", app.settings_input)
    } else {
        match app.config.max_autopilot_continues {
            Some(limit) => limit.to_string(),
            None => "(Copilot default)".to_string(),
        }
    };
    let continues_color = if app.config.max_autopilot_continues.is_some() {
        theme.accent_alt
    } else {
        theme.muted
    };
    setting_lines.push(lines.len());
    lines.push(settings_row(
        theme,
        "Max Autopilot Continues",
        &continues_display,
        continues_color,
        app.settings_selected == 18,
        continues_editing,
    ));
    lines.push(Line::from(Span::styled(
        "    Blank leaves Copilot to apply its own limit",
        Style::default().fg(theme.muted),
    )));

    lines.push(Line::from(""));

    // Row 3: Theme
    setting_lines.push(lines.len());
    lines.push(settings_row(
        theme,
        "Theme",
        app.theme_name().label(),
        theme.accent_alt,
        app.settings_selected == 3,
        false,
    ));
    lines.push(Line::from(Span::styled(
        "    Preview and choose CST colors",
        Style::default().fg(theme.muted),
    )));

    lines.push(Line::from(""));
    let general_end = lines.len();
    let worktrees_start = lines.len();
    let prefix_editing = app.settings_editing == Some(SettingsEditField::BranchPrefix);
    let prefix_display = if prefix_editing {
        format!("{}█", app.settings_input)
    } else {
        app.config.worktree.branch_prefix.clone()
    };
    setting_lines.push(lines.len());
    lines.push(settings_row(
        theme,
        "Branch Prefix",
        &prefix_display,
        theme.accent_alt,
        app.settings_selected == 4,
        prefix_editing,
    ));
    lines.push(Line::from(Span::styled(
        "    Default branch prefix for isolated sessions",
        Style::default().fg(theme.muted),
    )));

    lines.push(Line::from(""));
    let root_editing = app.settings_editing == Some(SettingsEditField::WorktreeRoot);
    let root_display = if root_editing {
        format!("{}█", app.settings_input)
    } else {
        app.config.worktree.root.to_string_lossy().to_string()
    };
    setting_lines.push(lines.len());
    lines.push(settings_row(
        theme,
        "Worktree Root",
        &root_display,
        theme.accent_alt,
        app.settings_selected == 5,
        root_editing,
    ));
    lines.push(Line::from(Span::styled(
        "    Relative paths resolve from the global config directory",
        Style::default().fg(theme.muted),
    )));

    lines.push(Line::from(""));
    let worktrees_end = lines.len();
    let terminal_start = lines.len();
    let mux_value = if app.mux_on_disk { "ON" } else { "OFF" };
    let mux_color = if app.mux_on_disk {
        theme.success
    } else {
        theme.error
    };
    setting_lines.push(lines.len());
    lines.push(settings_row(
        theme,
        "Multiplexer",
        mux_value,
        mux_color,
        app.settings_selected == 6,
        false,
    ));
    lines.push(Line::from(Span::styled(
        "    Run sessions inside CST as panes (applies on restart)",
        Style::default().fg(theme.muted),
    )));

    lines.push(Line::from(""));
    let mux_prefix_editing = app.settings_editing == Some(SettingsEditField::MuxPrefix);
    let mux_prefix_display = if mux_prefix_editing {
        format!("{}█", app.settings_input)
    } else {
        app.config.mux_prefix.clone()
    };
    setting_lines.push(lines.len());
    lines.push(settings_row(
        theme,
        "Mux Prefix",
        &mux_prefix_display,
        theme.accent_alt,
        app.settings_selected == 7,
        mux_prefix_editing,
    ));
    lines.push(Line::from(Span::styled(
        "    Prefix key for pane commands, e.g. C-b, C-g, C-a",
        Style::default().fg(theme.muted),
    )));
    lines.push(Line::from(Span::styled(
        format!(
            "    tmux keys: {} resume · {} new · {} worktree · {} end (config.json: tmux_keys)",
            app.config.tmux_keys.resume_session,
            app.config.tmux_keys.new_session,
            app.config.tmux_keys.new_worktree,
            app.config.tmux_keys.end_session
        ),
        Style::default().fg(theme.muted),
    )));

    lines.push(Line::from(""));
    let shell_editing = app.settings_editing == Some(SettingsEditField::TerminalShell);
    let shell_display = if shell_editing {
        format!("{}█", app.settings_input)
    } else {
        app.config
            .terminal
            .shell
            .as_deref()
            .unwrap_or("(platform default)")
            .to_string()
    };
    let shell_color = if app.config.terminal.shell.is_some() {
        theme.accent_alt
    } else {
        theme.muted
    };
    setting_lines.push(lines.len());
    lines.push(settings_row(
        theme,
        "Terminal Shell",
        &shell_display,
        shell_color,
        app.settings_selected == 8,
        shell_editing,
    ));
    lines.push(Line::from(Span::styled(
        "    Executable or path; blank uses the platform default",
        Style::default().fg(theme.muted),
    )));

    lines.push(Line::from(""));
    let terminal_end = lines.len();
    let notifications_start = lines.len();
    lines.push(Line::from(Span::styled(
        "  Notifications (ntfy HTTP)",
        Style::default()
            .fg(theme.accent)
            .add_modifier(Modifier::BOLD),
    )));
    lines.push(Line::from(""));

    setting_lines.push(lines.len());
    let notifications_enabled = app.config.notifications.enabled;
    lines.push(settings_row(
        theme,
        "Notifications",
        if notifications_enabled { "ON" } else { "OFF" },
        if notifications_enabled {
            theme.success
        } else {
            theme.error
        },
        app.settings_selected == 9,
        false,
    ));
    lines.push(Line::from(Span::styled(
        "    Publish ready/error history directly over HTTP",
        Style::default().fg(theme.muted),
    )));
    lines.push(Line::from(""));

    setting_lines.push(lines.len());
    let server_editing = app.settings_editing == Some(SettingsEditField::NtfyServer);
    let server_display = if server_editing {
        format!("{}█", app.settings_input)
    } else {
        app.config.notifications.server.clone()
    };
    lines.push(settings_row(
        theme,
        "ntfy Server",
        &server_display,
        theme.accent_alt,
        app.settings_selected == 10,
        server_editing,
    ));
    lines.push(Line::from(Span::styled(
        "    Defaults to https://ntfy.sh; self-hosted URLs are supported",
        Style::default().fg(theme.muted),
    )));
    lines.push(Line::from(""));

    setting_lines.push(lines.len());
    let topic_editing = app.settings_editing == Some(SettingsEditField::NtfyTopic);
    let topic_display = if topic_editing {
        format!("{}█", app.settings_input)
    } else if app.config.notifications.topic.is_empty() {
        "(not configured)".to_string()
    } else {
        "•••••••••••• (configured)".to_string()
    };
    lines.push(settings_row(
        theme,
        "ntfy Topic",
        &topic_display,
        if app.config.notifications.topic.is_empty() && app.settings_input.is_empty() {
            theme.muted
        } else {
            theme.accent_alt
        },
        app.settings_selected == 11,
        topic_editing,
    ));
    lines.push(Line::from(Span::styled(
        "    Secret topic; never shown outside edit mode",
        Style::default().fg(theme.muted),
    )));
    lines.push(Line::from(""));

    setting_lines.push(lines.len());
    let token_editing = app.settings_editing == Some(SettingsEditField::NtfyAccessToken);
    let token_display = if token_editing {
        format!("{}█", app.settings_input)
    } else if app.config.ntfy_access_token.is_empty() {
        "(not configured)".to_string()
    } else {
        "•••••••••••• (configured)".to_string()
    };
    lines.push(settings_row(
        theme,
        "Access Token",
        &token_display,
        if app.config.ntfy_access_token.is_empty() && app.settings_input.is_empty() {
            theme.muted
        } else {
            theme.accent_alt
        },
        app.settings_selected == 12,
        token_editing,
    ));
    lines.push(Line::from(Span::styled(
        "    Optional Bearer token; stored in config, so prefer HTTPS",
        Style::default().fg(theme.muted),
    )));
    lines.push(Line::from(""));

    setting_lines.push(lines.len());
    lines.push(settings_row(
        theme,
        "Detailed Content",
        if app.config.ntfy_verbose {
            "LATEST RESPONSE"
        } else {
            "STATUS ONLY"
        },
        if app.config.ntfy_verbose {
            theme.warning
        } else {
            theme.success
        },
        app.settings_selected == 13,
        false,
    ));
    lines.push(Line::from(Span::styled(
        "    May send sensitive assistant text; use a private authenticated server",
        Style::default().fg(if app.config.ntfy_verbose {
            theme.warning
        } else {
            theme.muted
        }),
    )));
    lines.push(Line::from(""));

    setting_lines.push(lines.len());
    lines.push(settings_row(
        theme,
        "Ready Events",
        if app.config.notifications.ready {
            "ON"
        } else {
            "OFF"
        },
        if app.config.notifications.ready {
            theme.success
        } else {
            theme.error
        },
        app.settings_selected == 14,
        false,
    ));
    lines.push(Line::from(Span::styled(
        "    Questions, approvals, or completed work ready for review",
        Style::default().fg(theme.muted),
    )));
    lines.push(Line::from(""));

    setting_lines.push(lines.len());
    lines.push(settings_row(
        theme,
        "Error Events",
        if app.config.notifications.error {
            "ON"
        } else {
            "OFF"
        },
        if app.config.notifications.error {
            theme.success
        } else {
            theme.error
        },
        app.settings_selected == 15,
        false,
    ));
    lines.push(Line::from(Span::styled(
        "    Copilot OSC progress error state",
        Style::default().fg(theme.muted),
    )));
    lines.push(Line::from(""));
    let notifications_end = lines.len();
    let filters_start = lines.len();
    lines.push(Line::from(""));
    let prefixes_editing = app.settings_editing == Some(SettingsEditField::HiddenTitlePrefixes);
    let prefixes_display = if prefixes_editing {
        format!("{}█", app.settings_input)
    } else if app.config.hidden_title_prefixes.is_empty() {
        "(none)".to_string()
    } else {
        app.config.hidden_title_prefixes.join(", ")
    };
    setting_lines.push(lines.len());
    lines.push(settings_row(
        theme,
        "Hidden Title Prefixes",
        &prefixes_display,
        if app.config.hidden_title_prefixes.is_empty() {
            theme.muted
        } else {
            theme.accent_alt
        },
        app.settings_selected == 16,
        prefixes_editing,
    ));
    lines.push(Line::from(Span::styled(
        "    Comma-separated, case-insensitive; press H to show temporarily",
        Style::default().fg(theme.muted),
    )));
    lines.push(Line::from(""));
    let paths_editing = app.settings_editing == Some(SettingsEditField::HiddenPathPrefixes);
    let paths_display = if paths_editing {
        format!("{}█", app.settings_input)
    } else if app.config.hidden_path_prefixes.is_empty() {
        "(none)".to_string()
    } else {
        app.config.hidden_path_prefixes.join(", ")
    };
    setting_lines.push(lines.len());
    lines.push(settings_row(
        theme,
        "Hidden Path Prefixes",
        &paths_display,
        if app.config.hidden_path_prefixes.is_empty() {
            theme.muted
        } else {
            theme.accent_alt
        },
        app.settings_selected == 17,
        paths_editing,
    ));
    lines.push(Line::from(Span::styled(
        "    Comma-separated working-directory roots; path boundaries are respected",
        Style::default().fg(theme.muted),
    )));
    lines.push(Line::from(""));

    setting_lines.push(lines.len());
    lines.push(settings_row(
        theme,
        "GitHub Comment Filter",
        app.config.github_comment_filter.settings_label(),
        theme.accent_alt,
        app.settings_selected == 19,
        false,
    ));
    lines.push(Line::from(Span::styled(
        "    Initial pull-request comment view; press f in the inspector to cycle",
        Style::default().fg(theme.muted),
    )));
    lines.push(Line::from(""));
    let filters_end = lines.len();

    let threads_start = lines.len();
    // The trusted list comes first because it is the only one here that changes what
    // somebody else can cause to happen on this machine.
    let trusted_editing = app.settings_editing == Some(SettingsEditField::ThreadTrustedAuthors);
    let trusted_display = if trusted_editing {
        format!("{}█", app.settings_input)
    } else if app.config.thread_trusted_authors.is_empty() {
        "(only you)".to_string()
    } else {
        app.config.thread_trusted_authors.join(", ")
    };
    setting_lines.push(lines.len());
    lines.push(settings_row(
        theme,
        "Trusted Authors",
        &trusted_display,
        if app.config.thread_trusted_authors.is_empty() {
            theme.muted
        } else {
            theme.warning
        },
        app.settings_selected == 20,
        trusted_editing,
    ));
    lines.push(Line::from(Span::styled(
        "    Comma-separated GitHub logins whose comments may wake a session",
        Style::default().fg(theme.muted),
    )));
    lines.push(Line::from(""));

    setting_lines.push(lines.len());
    lines.push(settings_row(
        theme,
        "Thread Wakes",
        if app.config.threads_enabled {
            "On"
        } else {
            "Off"
        },
        if app.config.threads_enabled {
            theme.success
        } else {
            theme.muted
        },
        app.settings_selected == 21,
        false,
    ));
    lines.push(Line::from(Span::styled(
        "    Costs nothing until a session takes part in a thread",
        Style::default().fg(theme.muted),
    )));
    lines.push(Line::from(""));

    let wakeups_editing = app.settings_editing == Some(SettingsEditField::ThreadWakeupsPerHour);
    let wakeups_display = if wakeups_editing {
        format!("{}█", app.settings_input)
    } else {
        match app.config.thread_wakeups_per_hour {
            Some(limit) => limit.to_string(),
            None => format!("{} (default)", crate::threads::DEFAULT_WAKEUPS_PER_HOUR),
        }
    };
    setting_lines.push(lines.len());
    lines.push(settings_row(
        theme,
        "Wakes Per Hour",
        &wakeups_display,
        if app.config.thread_wakeups_per_hour.is_some() {
            theme.accent_alt
        } else {
            theme.muted
        },
        app.settings_selected == 22,
        wakeups_editing,
    ));
    lines.push(Line::from(Span::styled(
        "    Beyond this, a thread's messages wait for you instead",
        Style::default().fg(theme.muted),
    )));
    lines.push(Line::from(""));

    setting_lines.push(lines.len());
    lines.push(settings_row(
        theme,
        "Stall Detection",
        if app.config.thread_stall_detection {
            "On"
        } else {
            "Off"
        },
        if app.config.thread_stall_detection {
            theme.success
        } else {
            theme.muted
        },
        app.settings_selected == 23,
        false,
    ));
    lines.push(Line::from(Span::styled(
        "    Says when two agents look like they are going in circles",
        Style::default().fg(theme.muted),
    )));
    lines.push(Line::from(""));
    let threads_end = lines.len();

    let (section_start, section_end) = match app.settings_section {
        SettingsSection::General => (general_start, general_end),
        SettingsSection::Filters => (filters_start, filters_end),
        SettingsSection::Worktrees => (worktrees_start, worktrees_end),
        SettingsSection::Terminal => (terminal_start, terminal_end),
        SettingsSection::Notifications => (notifications_start, notifications_end),
        SettingsSection::Threads => (threads_start, threads_end),
    };
    let visible_lines = lines[section_start..section_end].to_vec();
    f.render_widget(
        Paragraph::new(Line::from(vec![
            Span::raw("  "),
            Span::styled("Tab/Shift+Tab", Style::default().fg(theme.accent)),
            Span::raw(" Section  "),
            Span::styled("↑/↓", Style::default().fg(theme.accent)),
            Span::raw(" Select  "),
            Span::styled("Enter/Space", Style::default().fg(theme.accent)),
            Span::raw(" Edit/Toggle  "),
            Span::styled("Esc/,", Style::default().fg(theme.accent)),
            Span::raw(" Save"),
        ]))
        .style(surface_style(theme)),
        sections[3],
    );

    let viewport = sections[2].height as usize;
    let selected_line = setting_lines
        .get(app.settings_selected)
        .copied()
        .unwrap_or(section_start)
        .saturating_sub(section_start);
    let max_scroll = visible_lines.len().saturating_sub(viewport);
    let scroll = selected_line
        .saturating_sub(1)
        .min(max_scroll)
        .min(u16::MAX as usize) as u16;
    let paragraph = Paragraph::new(visible_lines)
        .style(surface_style(theme))
        .scroll((scroll, 0));
    f.render_widget(paragraph, sections[2]);

    if app.theme_picker.is_some() {
        draw_theme_picker(f, app);
    } else {
        app.set_theme_picker_hits(Vec::new());
    }
}

fn draw_theme_picker(f: &mut Frame, app: &mut App) {
    let Some(picker) = app.theme_picker else {
        return;
    };
    let theme = app.theme();
    let area = centered_rect(54, 68, f.area());
    f.render_widget(Clear, area);
    fill_area(f.buffer_mut(), area, theme.chrome_bg);
    let picker_style = Style::default().fg(theme.text).bg(theme.chrome_bg);
    let block = Block::default()
        .title(format!(
            " Theme Picker · Preview: {} ",
            app.theme_name().label()
        ))
        .borders(Borders::ALL)
        .style(picker_style)
        .border_style(Style::default().fg(theme.accent_alt));
    let inner = block.inner(area);
    f.render_widget(block, area);

    let classic = [ThemeName::Classic];
    let groups: [(&str, &[ThemeName]); 3] = [
        ("Classic", &classic),
        ("Dark", &ThemeName::DARK),
        ("Light", &ThemeName::LIGHT),
    ];
    let mut entries: Vec<(Line<'static>, Option<usize>)> = Vec::new();
    for (group_index, (label, names)) in groups.into_iter().enumerate() {
        if group_index > 0 {
            entries.push((Line::from(""), None));
        }
        entries.push((
            Line::from(Span::styled(
                format!("  {label}"),
                Style::default()
                    .fg(theme.accent)
                    .bg(theme.chrome_bg)
                    .add_modifier(Modifier::BOLD),
            )),
            None,
        ));
        for name in names {
            let theme_index = ThemeName::ALL
                .iter()
                .position(|candidate| candidate == name)
                .unwrap_or_default();
            let selected = picker.selected_theme() == *name;
            let saved = picker.original == *name;
            let row_style = if selected {
                Style::default()
                    .fg(theme.selection_fg)
                    .bg(theme.selection_bg)
                    .add_modifier(Modifier::BOLD)
            } else {
                picker_style
            };
            let marker = if selected { "▸" } else { " " };
            let saved_label = if saved { " ● saved" } else { "" };
            let preview_label = if selected { " ◀ preview" } else { "" };
            let saved_style = if selected {
                row_style
            } else {
                picker_style.fg(theme.success)
            };
            entries.push((
                Line::from(vec![
                    Span::styled(format!("  {marker} {:<20}", name.label()), row_style),
                    Span::styled(saved_label, saved_style),
                    Span::styled(preview_label, row_style),
                ]),
                Some(theme_index),
            ));
        }
    }
    let sections = Layout::default()
        .direction(Direction::Vertical)
        .constraints([Constraint::Min(1), Constraint::Length(3)])
        .split(inner);
    let visible_rows = sections[0].height as usize;
    let selected_row = entries
        .iter()
        .position(|(_, index)| *index == Some(picker.selected))
        .unwrap_or_default();
    let max_offset = entries.len().saturating_sub(visible_rows);
    let offset = selected_row
        .saturating_sub(visible_rows.saturating_sub(1))
        .min(max_offset);
    let visible: Vec<Line<'static>> = entries
        .iter()
        .skip(offset)
        .take(visible_rows)
        .map(|(line, _)| line.clone())
        .collect();
    let hits = entries
        .iter()
        .enumerate()
        .skip(offset)
        .take(visible_rows)
        .filter_map(|(entry_row, (_, theme_index))| {
            theme_index.map(|theme_index| {
                let row = sections[0].y + u16::try_from(entry_row - offset).unwrap_or(u16::MAX);
                (
                    Rect::new(sections[0].x, row, sections[0].width, 1),
                    theme_index,
                )
            })
        })
        .collect();
    f.render_widget(Paragraph::new(visible).style(picker_style), sections[0]);
    let hints = vec![
        Line::from(""),
        Line::from(vec![
            Span::styled(
                "  ↑/↓",
                Style::default()
                    .fg(theme.accent_alt)
                    .bg(theme.chrome_bg)
                    .add_modifier(Modifier::BOLD),
            ),
            Span::styled(" select · PageUp/PageDown/Home/End jump", picker_style),
        ]),
        Line::from(vec![
            Span::styled(
                "  Enter",
                Style::default()
                    .fg(theme.success)
                    .bg(theme.chrome_bg)
                    .add_modifier(Modifier::BOLD),
            ),
            Span::styled(" apply · ", picker_style),
            Span::styled(
                "Esc",
                Style::default()
                    .fg(theme.error)
                    .bg(theme.chrome_bg)
                    .add_modifier(Modifier::BOLD),
            ),
            Span::styled(
                format!(" cancel · restore {}", picker.original.label()),
                picker_style,
            ),
        ]),
    ];
    f.render_widget(Paragraph::new(hints).style(picker_style), sections[1]);
    app.set_theme_picker_hits(hits);
}

pub fn draw_project_settings(f: &mut Frame, app: &App) {
    let theme = app.theme();
    let area = centered_rect(70, 50, f.area());
    prepare_popup(f, area, theme);
    let block = Block::default()
        .title(" Project Settings (.cst.json) ")
        .borders(Borders::ALL)
        .style(surface_style(theme))
        .border_style(Style::default().fg(theme.directory));
    let inner = block.inner(area);
    f.render_widget(block, area);

    let Some(settings) = app.project_settings.as_ref() else {
        return;
    };
    let prefix_override = settings.branch_prefix_override().is_some();
    let root_override = settings.root_override().is_some();
    let prefix_value = if app.project_settings_editing && app.project_settings_selected == 0 {
        format!("{}█", app.project_settings_input)
    } else {
        settings.effective_branch_prefix().to_string()
    };
    let root_value = if app.project_settings_editing && app.project_settings_selected == 1 {
        format!("{}█", app.project_settings_input)
    } else {
        settings.effective_root().to_string_lossy().to_string()
    };
    let continues_value = if app.project_settings_editing && app.project_settings_selected == 3 {
        format!("{}█", app.project_settings_input)
    } else {
        match settings.effective_max_autopilot_continues() {
            Some(limit) => limit.to_string(),
            None => "(Copilot default)".to_string(),
        }
    };

    let lines = vec![
        Line::from(""),
        Line::from(Span::styled(
            format!("  Repository: {}", settings.repository_root.display()),
            Style::default().fg(theme.muted),
        )),
        Line::from(""),
        project_settings_row(
            theme,
            "Branch Prefix",
            &prefix_value,
            prefix_override,
            app.project_settings_selected == 0,
            app.project_settings_editing && app.project_settings_selected == 0,
        ),
        Line::from(Span::styled(
            "    Effective prefix used to prepopulate isolated branches",
            Style::default().fg(theme.muted),
        )),
        Line::from(""),
        project_settings_row(
            theme,
            "Worktree Root",
            &root_value,
            root_override,
            app.project_settings_selected == 1,
            app.project_settings_editing && app.project_settings_selected == 1,
        ),
        Line::from(Span::styled(
            "    Relative overrides resolve from the repository root",
            Style::default().fg(theme.muted),
        )),
        Line::from(""),
        project_settings_row(
            theme,
            "Yolo Mode",
            if settings.effective_yolo() {
                "ON"
            } else {
                "OFF"
            },
            settings.yolo_override().is_some(),
            app.project_settings_selected == 2,
            false,
        ),
        Line::from(Span::styled(
            "    Space cycles inherit / ON / OFF for sessions in this repository",
            Style::default().fg(theme.muted),
        )),
        Line::from(""),
        project_settings_row(
            theme,
            "Max Autopilot Continues",
            &continues_value,
            settings.max_autopilot_continues_override().is_some(),
            app.project_settings_selected == 3,
            app.project_settings_editing && app.project_settings_selected == 3,
        ),
        Line::from(Span::styled(
            "    Blank inherits; Space overrides from whatever is in effect",
            Style::default().fg(theme.muted),
        )),
        Line::from(""),
        project_settings_row(
            theme,
            "Thread Wakes",
            if settings.effective_threads_enabled() {
                "On"
            } else {
                "Off"
            },
            settings.threads_enabled_override().is_some(),
            app.project_settings_selected == 4,
            false,
        ),
        Line::from(Span::styled(
            "    Whether GitHub threads may wake sessions in this repository",
            Style::default().fg(theme.muted),
        )),
        Line::from(""),
        Line::from(vec![
            Span::raw("  "),
            Span::styled("Space", Style::default().fg(theme.directory)),
            Span::raw(" Inherit/Override  "),
            Span::styled("Enter", Style::default().fg(theme.directory)),
            Span::raw(" Edit  "),
            Span::styled("Esc/.", Style::default().fg(theme.directory)),
            Span::raw(" Save & Close"),
        ]),
    ];
    f.render_widget(
        Paragraph::new(lines)
            .style(surface_style(theme))
            .wrap(Wrap { trim: false }),
        inner,
    );
}

pub fn draw_branch_name(f: &mut Frame, app: &App) {
    let theme = app.theme();
    let area = centered_rect(65, 28, f.area());
    prepare_popup(f, area, theme);
    let tmux = app.worktree_launch_target == crate::app::WorktreeLaunchTarget::Tmux;
    let prompt = if tmux {
        "  Branch for persistent tmux worktree session:"
    } else {
        "  Branch for isolated worktree session:"
    };
    let title = if tmux {
        " New Persistent tmux Session "
    } else {
        " New Isolated Session "
    };
    let text = vec![
        Line::from(""),
        Line::from(Span::styled(
            prompt,
            Style::default()
                .fg(theme.accent_alt)
                .add_modifier(Modifier::BOLD),
        )),
        Line::from(""),
        Line::from(vec![
            Span::raw("  "),
            Span::styled(&app.branch_input, Style::default().fg(theme.text)),
            Span::styled("█", Style::default().fg(theme.accent_alt)),
        ]),
        Line::from(""),
        Line::from(vec![
            Span::raw("  "),
            Span::styled("Enter", Style::default().fg(theme.accent_alt)),
            Span::raw(" Create  "),
            Span::styled("Esc", Style::default().fg(theme.accent_alt)),
            Span::raw(" Cancel"),
        ]),
    ];
    let block = Block::default()
        .title(title)
        .borders(Borders::ALL)
        .style(surface_style(theme))
        .border_style(Style::default().fg(theme.accent_alt));
    f.render_widget(
        Paragraph::new(text)
            .style(surface_style(theme))
            .block(block),
        area,
    );
}

fn settings_row<'a>(
    theme: Theme,
    label: &str,
    value: &str,
    value_color: Color,
    is_selected: bool,
    is_editing: bool,
) -> Line<'a> {
    let pointer = if is_selected { "▸ " } else { "  " };
    let label_style = if is_selected {
        if theme.name == ThemeName::Classic {
            Style::default()
                .fg(theme.text)
                .bg(theme.surface)
                .add_modifier(Modifier::BOLD)
        } else {
            super::row_selection_style(theme)
        }
    } else {
        surface_style(theme)
    };
    let pointer_style = if is_selected {
        if theme.name == ThemeName::Classic {
            Style::default().fg(theme.accent).bg(theme.surface)
        } else {
            super::row_selection_style(theme)
        }
    } else {
        Style::default().fg(theme.accent).bg(theme.surface)
    };
    let value_style = if is_selected && theme.name != ThemeName::Classic {
        super::row_selection_style(theme)
    } else if is_editing {
        Style::default().fg(theme.warning).bg(theme.surface)
    } else {
        Style::default().fg(value_color).bg(theme.surface)
    };

    Line::from(vec![
        Span::styled(pointer.to_string(), pointer_style),
        Span::styled(format!("{:<24}", label), label_style),
        Span::styled(value.to_string(), value_style),
    ])
}

fn project_settings_row<'a>(
    theme: Theme,
    label: &str,
    value: &str,
    is_override: bool,
    is_selected: bool,
    is_editing: bool,
) -> Line<'a> {
    let pointer = if is_selected { "▸ " } else { "  " };
    let state = if is_override { "Override" } else { "Inherited" };
    let state_color = if is_override {
        theme.warning
    } else {
        theme.muted
    };
    let value_color = if is_editing {
        theme.warning
    } else {
        theme.accent_alt
    };
    let row_style = |color| {
        if is_selected && theme.name != ThemeName::Classic {
            super::row_selection_style(theme)
        } else {
            Style::default().fg(color).bg(theme.surface)
        }
    };
    Line::from(vec![
        Span::styled(pointer.to_string(), row_style(theme.directory)),
        Span::styled(format!("{:<18}", label), row_style(theme.text)),
        Span::styled(format!("[{state:<9}] "), row_style(state_color)),
        Span::styled(value.to_string(), row_style(value_color)),
    ])
}

/// Ask how the inactive favorites should be opened.
///
/// Both destinations are always offered rather than inferred from the host terminal, so
/// the same keystroke means the same thing on every machine.
pub fn draw_favorite_open(f: &mut Frame, app: &App) {
    let theme = app.theme();
    let plan = crate::windows_terminal::build_launch_plan(&app.sessions, &app.config);
    let ready = plan.tabs.len();
    let skipped = plan.active.len() + plan.stale.len();

    let area = centered_rect(58, 42, f.area());
    prepare_popup(f, area, theme);
    let block = Block::default()
        .title(" Open Favorite Sessions ")
        .borders(Borders::ALL)
        .style(surface_style(theme))
        .border_style(Style::default().fg(theme.accent));
    let inner = block.inner(area);
    f.render_widget(block, area);

    let summary = match (ready, skipped) {
        (0, 0) => "  No favorite sessions configured.".to_string(),
        (0, _) => format!("  No inactive favorites to open ({skipped} active or missing)."),
        (_, 0) => format!("  {ready} inactive favorite(s) ready."),
        _ => format!("  {ready} ready, {skipped} skipped as active or missing."),
    };

    let mut lines = vec![
        Line::from(""),
        Line::from(Span::styled(summary, Style::default().fg(theme.text))),
        Line::from(""),
    ];
    if ready > 0 {
        lines.push(Line::from(vec![
            Span::raw("  "),
            Span::styled(
                "p",
                Style::default()
                    .fg(theme.accent_alt)
                    .add_modifier(Modifier::BOLD),
            ),
            Span::raw("  as panes in this CST window"),
        ]));
        lines.push(Line::from(vec![
            Span::raw("  "),
            Span::styled(
                "t",
                Style::default()
                    .fg(theme.accent_alt)
                    .add_modifier(Modifier::BOLD),
            ),
            Span::raw("  as Windows Terminal tabs"),
        ]));
        lines.push(Line::from(""));
    }
    lines.push(Line::from(vec![
        Span::raw("  "),
        Span::styled(
            "Esc",
            Style::default()
                .fg(theme.muted)
                .add_modifier(Modifier::BOLD),
        ),
        Span::raw(" cancel"),
    ]));

    f.render_widget(
        Paragraph::new(lines)
            .style(surface_style(theme))
            .wrap(Wrap { trim: false }),
        inner,
    );
}

#[cfg(test)]
mod help_tests {
    use super::*;
    use crate::app::App;
    use crate::config::UserConfig;
    use ratatui::backend::TestBackend;
    use ratatui::buffer::Buffer;
    use ratatui::Terminal;

    fn buffer_text(buffer: &Buffer) -> String {
        buffer.content().iter().map(|cell| cell.symbol()).collect()
    }

    fn buffer_rows(buffer: &Buffer) -> Vec<String> {
        (0..buffer.area.height)
            .map(|y| {
                (0..buffer.area.width)
                    .map(|x| buffer[(x, y)].symbol())
                    .collect()
            })
            .collect()
    }

    fn rendered_settings(app: &mut App, width: u16, height: u16) -> Buffer {
        let mut terminal = Terminal::new(TestBackend::new(width, height)).unwrap();
        terminal.draw(|frame| draw_settings(frame, app)).unwrap();
        terminal.backend().buffer().clone()
    }

    fn rendered_with(
        config: UserConfig,
        width: u16,
        height: u16,
        scroll: usize,
    ) -> (String, usize) {
        let mut app = App::new(Vec::new(), config);
        app.mode = crate::app::Mode::Help;
        app.help_scroll = scroll;
        let backend = TestBackend::new(width, height);
        let mut terminal = Terminal::new(backend).expect("terminal");
        terminal.draw(|f| draw_help(f, &mut app)).expect("draw");
        let text = buffer_text(terminal.backend().buffer());
        (text, app.help_scroll)
    }

    fn rendered(width: u16, height: u16, scroll: usize) -> (String, usize) {
        rendered_with(UserConfig::default(), width, height, scroll)
    }

    const FOOTER: &str = "Esc/q/Enter/? close";

    #[test]
    fn a_help_screen_taller_than_its_popup_can_be_scrolled_to_the_end() {
        let (top, _) = rendered(120, 40, 0);
        assert!(top.contains("Keyboard Shortcuts"), "got:\n{top}");
        assert!(
            !top.contains(FOOTER),
            "the help fits after all, so scrolling is pointless:\n{top}"
        );

        let (bottom, clamped) = rendered(120, 40, usize::MAX);
        assert!(bottom.contains(FOOTER), "got:\n{bottom}");
        assert!(
            clamped < usize::MAX,
            "End should be clamped to the last page, got {clamped}"
        );
    }

    #[test]
    fn a_help_screen_that_fits_is_not_scrolled() {
        let (text, clamped) = rendered(200, 100, usize::MAX);

        assert!(text.contains("Keyboard Shortcuts"), "got:\n{text}");
        assert!(
            text.contains(&format!("CST v{}", env!("CARGO_PKG_VERSION"))),
            "running version missing:\n{text}"
        );
        assert!(text.contains(FOOTER), "got:\n{text}");
        assert_eq!(clamped, 0);
    }

    #[test]
    fn the_reference_legend_is_reachable_under_the_multiplexer_section() {
        let config = UserConfig {
            mux: true,
            ..UserConfig::default()
        };
        let (text, _) = rendered_with(config, 200, 100, 0);

        assert!(
            text.contains("GitHub references in the chat"),
            "got:\n{text}"
        );
        assert!(text.contains("C-b g i"), "got:\n{text}");
        assert!(text.contains("Inspect a GitHub issue"), "got:\n{text}");
        assert!(text.contains("pull request"), "got:\n{text}");
    }

    #[test]
    fn takeover_popup_names_the_session_and_interruption_risk() {
        let mut app = App::new(Vec::new(), UserConfig::default());
        app.pending_takeover = Some(crate::app::TakeoverTarget {
            id: "session-id".to_string(),
            cwd: "C:/repo".to_string(),
            title: "Important work".to_string(),
            dir_path: std::path::PathBuf::from("session-id"),
            pids: vec![1234],
            resume_in_tmux: false,
        });
        let mut terminal = Terminal::new(TestBackend::new(100, 30)).unwrap();

        terminal
            .draw(|frame| draw_takeover_confirm(frame, &app))
            .unwrap();

        let text: String = terminal
            .backend()
            .buffer()
            .content()
            .iter()
            .map(|cell| cell.symbol())
            .collect();
        assert!(text.contains("Take Over Active Session"), "got:\n{text}");
        assert!(text.contains("Important work"), "got:\n{text}");
        assert!(text.contains("Copilot PID"), "got:\n{text}");
        assert!(text.contains("1234"), "got:\n{text}");
        assert!(text.contains("In-flight work will be"), "got:\n{text}");
        assert!(text.contains("interrupted."), "got:\n{text}");
        assert!(text.contains("y/Enter take over"), "got:\n{text}");
    }

    #[test]
    fn notification_settings_mask_the_saved_topic_but_reveal_it_while_editing() {
        let mut app = App::new(Vec::new(), UserConfig::default());
        app.settings_section = SettingsSection::Notifications;
        app.settings_selected = 15;
        app.config.notifications.enabled = true;
        app.config.notifications.topic = "super_secret_phone_topic".to_string();
        app.config.ntfy_access_token = "tk_super_secret_token".to_string();
        let mut terminal = Terminal::new(TestBackend::new(100, 35)).unwrap();

        terminal
            .draw(|frame| draw_settings(frame, &mut app))
            .unwrap();

        let text: String = terminal
            .backend()
            .buffer()
            .content()
            .iter()
            .map(|cell| cell.symbol())
            .collect();
        assert!(text.contains("General   Worktrees   Terminal   Notifications"));
        assert!(text.contains("Error Events"), "got:\n{text}");
        assert!(text.contains("Tab/Shift+Tab"), "got:\n{text}");
        assert!(text.contains("(configured)"), "got:\n{text}");
        assert!(
            !text.contains("super_secret_phone_topic"),
            "topic leaked outside edit mode:\n{text}"
        );
        assert!(
            !text.contains("tk_super_secret_token"),
            "token leaked outside edit mode:\n{text}"
        );

        app.settings_selected = 11;
        app.settings_editing = Some(SettingsEditField::NtfyTopic);
        app.settings_input = "super_secret_phone_topic".to_string();
        terminal
            .draw(|frame| draw_settings(frame, &mut app))
            .unwrap();
        let editing: String = terminal
            .backend()
            .buffer()
            .content()
            .iter()
            .map(|cell| cell.symbol())
            .collect();
        assert!(
            editing.contains("super_secret_phone_topic█"),
            "editable topic and cursor missing:\n{editing}"
        );

        app.settings_selected = 12;
        app.settings_editing = Some(SettingsEditField::NtfyAccessToken);
        app.settings_input = "tk_super_secret_token".to_string();
        terminal
            .draw(|frame| draw_settings(frame, &mut app))
            .unwrap();
        let editing: String = terminal
            .backend()
            .buffer()
            .content()
            .iter()
            .map(|cell| cell.symbol())
            .collect();
        assert!(
            editing.contains("tk_super_secret_token█"),
            "editable token and cursor missing:\n{editing}"
        );
    }

    #[test]
    fn settings_rendering_maps_all_rows_to_the_shared_section_indexes() {
        let cases = [
            (SettingsSection::General, 0, "Yolo Mode"),
            (SettingsSection::General, 1, "Model"),
            (SettingsSection::General, 2, "Reasoning Effort"),
            (SettingsSection::General, 3, "Theme"),
            (SettingsSection::General, 18, "Max Autopilot Continues"),
            (SettingsSection::Filters, 16, "Hidden Title Prefixes"),
            (SettingsSection::Filters, 17, "Hidden Path Prefixes"),
            (SettingsSection::Filters, 19, "GitHub Comment Filter"),
            (SettingsSection::Threads, 20, "Trusted Authors"),
            (SettingsSection::Threads, 21, "Thread Wakes"),
            (SettingsSection::Threads, 22, "Wakes Per Hour"),
            (SettingsSection::Threads, 23, "Stall Detection"),
            (SettingsSection::Worktrees, 4, "Branch Prefix"),
            (SettingsSection::Worktrees, 5, "Worktree Root"),
            (SettingsSection::Terminal, 6, "Multiplexer"),
            (SettingsSection::Terminal, 7, "Mux Prefix"),
            (SettingsSection::Terminal, 8, "Terminal Shell"),
            (SettingsSection::Notifications, 9, "Notifications"),
            (SettingsSection::Notifications, 10, "ntfy Server"),
            (SettingsSection::Notifications, 11, "ntfy Topic"),
            (SettingsSection::Notifications, 12, "Access Token"),
            (SettingsSection::Notifications, 13, "Detailed Content"),
            (SettingsSection::Notifications, 14, "Ready Events"),
            (SettingsSection::Notifications, 15, "Error Events"),
        ];

        for (section, selected, label) in cases {
            let mut app = App::new(Vec::new(), UserConfig::default());
            app.settings_section = section;
            app.settings_selected = selected;
            let rows = buffer_rows(&rendered_settings(&mut app, 120, 40));
            assert!(
                rows.iter()
                    .any(|row| row.contains('▸') && row.contains(label)),
                "row {selected} did not select {label}:\n{}",
                rows.join("\n")
            );
        }

        let mut app = App::new(Vec::new(), UserConfig::default());
        app.settings_section = SettingsSection::Filters;
        let rows = buffer_rows(&rendered_settings(&mut app, 120, 40));
        assert!(
            rows.iter()
                .any(|row| row.contains("Hidden Title Prefixes   (none)")),
            "filter label and value columns are not separated:\n{}",
            rows.join("\n")
        );
        assert!(
            rows.iter()
                .any(|row| row.contains("GitHub Comment Filter   ALL")),
            "GitHub comment filter default is missing:\n{}",
            rows.join("\n")
        );
    }

    #[test]
    fn theme_picker_groups_themes_and_marks_saved_and_previewed_choices() {
        let mut app = App::new(Vec::new(), UserConfig::default());
        app.settings_selected = 3;
        app.open_theme_picker();
        app.theme_picker.as_mut().unwrap().selected = ThemeName::ALL
            .iter()
            .position(|name| *name == ThemeName::CatppuccinLatte)
            .unwrap();

        let rows = buffer_rows(&rendered_settings(&mut app, 120, 44));
        let text = rows.join("\n");
        assert!(rows.iter().any(|row| row.contains("│  Classic")), "{text}");
        assert!(rows.iter().any(|row| row.contains("│  Dark")), "{text}");
        assert!(rows.iter().any(|row| row.contains("│  Light")), "{text}");
        assert!(
            rows.iter().any(|row| {
                row.contains('▸') && row.contains("Catppuccin Latte") && row.contains("◀ preview")
            }),
            "{text}"
        );
        assert!(
            rows.iter()
                .any(|row| row.contains("CST Classic") && row.contains("● saved")),
            "{text}"
        );
        assert!(text.contains("PageUp/PageDown/Home/End"), "{text}");
        assert!(text.contains("Enter apply"), "{text}");
        assert!(text.contains("Esc cancel"), "{text}");
    }

    #[test]
    fn theme_picker_registers_one_hit_target_per_theme_and_clears_stale_targets() {
        let mut app = App::new(Vec::new(), UserConfig::default());
        app.settings_selected = 3;
        app.open_theme_picker();

        let buffer = rendered_settings(&mut app, 120, 44);
        let rows = buffer_rows(&buffer);
        assert_eq!(app.theme_picker_hits.len(), ThemeName::ALL.len());
        for (expected_index, (area, theme_index)) in app.theme_picker_hits.iter().enumerate() {
            assert_eq!(*theme_index, expected_index);
            assert_eq!(area.height, 1);
            assert!(area.width > 0);
            assert!(
                rows[area.y as usize].contains(ThemeName::ALL[*theme_index].label()),
                "hit target {theme_index} does not cover its rendered row:\n{}",
                rows.join("\n")
            );
        }

        app.theme_picker = None;
        app.set_theme_picker_hits(vec![(Rect::new(1, 1, 1, 1), usize::MAX)]);
        rendered_settings(&mut app, 120, 44);
        assert!(app.theme_picker_hits.is_empty());
    }

    #[test]
    fn theme_picker_keeps_the_last_light_theme_visible_in_a_short_terminal() {
        let mut app = App::new(Vec::new(), UserConfig::default());
        app.settings_selected = 3;
        app.open_theme_picker();
        app.theme_picker.as_mut().unwrap().selected = ThemeName::ALL.len() - 1;

        let text = buffer_text(&rendered_settings(&mut app, 90, 24));

        assert!(text.contains("Solarized Light"), "{text}");
        assert!(text.contains("Enter"), "{text}");
        assert!(text.contains("Esc"), "{text}");
    }

    #[test]
    fn narrow_theme_picker_keeps_rows_and_mouse_targets_one_to_one() {
        let mut app = App::new(Vec::new(), UserConfig::default());
        app.settings_selected = 3;
        app.open_theme_picker();
        app.theme_picker.as_mut().unwrap().selected = ThemeName::ALL.len() - 1;

        rendered_settings(&mut app, 60, 24);

        assert!(app
            .theme_picker_hits
            .iter()
            .any(|(_, index)| *index == ThemeName::ALL.len() - 1));
        let mut rows: Vec<u16> = app
            .theme_picker_hits
            .iter()
            .map(|(area, _)| area.y)
            .collect();
        rows.sort_unstable();
        rows.dedup();
        assert_eq!(rows.len(), app.theme_picker_hits.len());
    }

    #[test]
    fn classic_settings_selection_keeps_the_legacy_unfilled_row() {
        let theme = ThemeName::Classic.theme();
        let line = settings_row(theme, "Theme", "CST Classic", theme.accent_alt, true, false);

        assert_eq!(line.spans[0].style.fg, Some(theme.accent));
        assert_eq!(line.spans[0].style.bg, Some(Color::Reset));
        assert_eq!(line.spans[1].style.fg, Some(Color::White));
        assert_eq!(line.spans[1].style.bg, Some(Color::Reset));
        assert!(line.spans[1].style.add_modifier.contains(Modifier::BOLD));
        assert_eq!(line.spans[2].style.fg, Some(theme.accent_alt));
        assert_eq!(line.spans[2].style.bg, Some(Color::Reset));
    }

    #[test]
    fn theme_picker_preview_colors_the_settings_surface_and_overlay() {
        let mut app = App::new(Vec::new(), UserConfig::default());
        app.settings_selected = 3;
        app.open_theme_picker();
        app.theme_picker.as_mut().unwrap().selected = ThemeName::ALL
            .iter()
            .position(|name| *name == ThemeName::Nord)
            .unwrap();
        let preview = ThemeName::Nord.theme();
        let width = 120;
        let height = 44;
        let buffer = rendered_settings(&mut app, width, height);
        let frame = Rect::new(0, 0, width, height);
        let settings = centered_rect(65, 78, frame);
        let picker = centered_rect(54, 68, frame);

        assert_eq!(buffer[(settings.x, settings.y)].fg, preview.accent);
        assert_eq!(buffer[(settings.x, settings.y)].bg, preview.surface);
        assert_eq!(buffer[(picker.x, picker.y)].fg, preview.accent_alt);
        assert_eq!(buffer[(picker.x, picker.y)].bg, preview.chrome_bg);
        let mut selected = None;
        for y in picker.top()..picker.bottom() {
            for x in picker.left()..picker.right() {
                if buffer[(x, y)].symbol() == "▸" {
                    selected = Some((x, y));
                }
            }
        }
        let selected = selected.expect("selected theme marker");
        assert_eq!(buffer[selected].fg, preview.selection_fg);
        assert_eq!(buffer[selected].bg, preview.selection_bg);
    }

    #[test]
    fn picker_cancel_hint_names_the_saved_theme_and_cancel_restores_its_presentation() {
        let config = UserConfig {
            theme: ThemeName::Gruvbox,
            ..UserConfig::default()
        };
        let mut app = App::new(Vec::new(), config.clone());
        app.settings_selected = 3;
        app.open_theme_picker();
        app.theme_picker.as_mut().unwrap().selected = ThemeName::ALL
            .iter()
            .position(|name| *name == ThemeName::SolarizedLight)
            .unwrap();

        let preview = buffer_text(&rendered_settings(&mut app, 120, 44));
        assert!(preview.contains("restore Gruvbox"), "{preview}");
        assert_eq!(app.config.theme, ThemeName::Gruvbox);

        app.cancel_theme_picker();
        let restored = buffer_text(&rendered_settings(&mut app, 120, 44));
        assert!(!restored.contains("Theme Picker"), "{restored}");
        assert!(
            restored.contains("Theme                   Gruvbox"),
            "{restored}"
        );
        assert_eq!(app.theme_name(), ThemeName::Gruvbox);
    }

    #[test]
    fn light_theme_fills_help_and_settings_popups_without_reset_backgrounds() {
        let config = UserConfig {
            theme: ThemeName::SolarizedLight,
            ..UserConfig::default()
        };
        let width = 120;
        let height = 44;
        let frame = Rect::new(0, 0, width, height);

        let mut help_app = App::new(Vec::new(), config.clone());
        let mut help_terminal = Terminal::new(TestBackend::new(width, height)).unwrap();
        help_terminal.draw(|f| draw_help(f, &mut help_app)).unwrap();
        let help = help_terminal.backend().buffer();
        let help_area = centered_rect(55, 70, frame);
        assert!((help_area.top()..help_area.bottom()).all(|y| {
            (help_area.left()..help_area.right()).all(|x| help[(x, y)].bg != Color::Reset)
        }));

        let mut settings_app = App::new(Vec::new(), config);
        let settings = rendered_settings(&mut settings_app, width, height);
        let settings_area = centered_rect(65, 78, frame);
        assert!((settings_area.top()..settings_area.bottom()).all(|y| {
            (settings_area.left()..settings_area.right())
                .all(|x| settings[(x, y)].bg != Color::Reset)
        }));
    }

    #[test]
    fn busy_popup_infers_and_fills_the_rendered_light_theme() {
        let theme = ThemeName::CatppuccinLatte.theme();
        let width = 100;
        let height = 30;
        let mut terminal = Terminal::new(TestBackend::new(width, height)).unwrap();
        terminal
            .draw(|frame| {
                let area = frame.area();
                fill_area(frame.buffer_mut(), area, theme.background);
                draw_busy(frame, "Working", "Applying changes", theme);
            })
            .unwrap();

        let area = centered_rect(50, 20, Rect::new(0, 0, width, height));
        let buffer = terminal.backend().buffer();
        assert_eq!(buffer[(area.x, area.y)].fg, theme.accent_alt);
        assert_eq!(buffer[(area.x, area.y)].bg, theme.surface);
        assert!((area.top()..area.bottom())
            .all(|y| { (area.left()..area.right()).all(|x| buffer[(x, y)].bg != Color::Reset) }));
    }
}

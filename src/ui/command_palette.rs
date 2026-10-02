use crate::app::App;
use crate::command_palette::CommandGroup;
use crate::mux::PrefixState;
use crate::text;
use crate::theme::{fill_area, Theme};
use ratatui::layout::{Constraint, Direction, Layout, Rect};
use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Clear, Paragraph};
use ratatui::Frame;

pub fn draw_overlays(f: &mut Frame, app: &mut App) {
    if app.command_palette.is_some() {
        draw_command_palette(f, app);
    } else if app
        .mux
        .as_ref()
        .is_some_and(|mux| mux.prefix_state == PrefixState::Root)
    {
        draw_prefix_menu(f, app);
    }
}

fn draw_prefix_menu(f: &mut Frame, app: &App) {
    let theme = app.theme();
    let area = bottom_overlay(f.area(), 112, 11);
    prepare(f, area, theme);
    let prefix = app
        .mux
        .as_ref()
        .map(|mux| mux.prefix.label())
        .unwrap_or_else(|| "C-b".to_string());
    let tmux_end = app.config.tmux_keys.end_session.as_str();
    let block = Block::default()
        .title(format!(" {prefix} · Commands "))
        .borders(Borders::ALL)
        .style(surface(theme))
        .border_style(Style::default().fg(theme.accent));
    let inner = block.inner(area);
    f.render_widget(block, area);

    let columns = Layout::default()
        .direction(Direction::Horizontal)
        .constraints([
            Constraint::Percentage(25),
            Constraint::Percentage(25),
            Constraint::Percentage(25),
            Constraint::Percentage(25),
        ])
        .split(inner);
    let groups = vec![
        (
            "Workspace",
            vec![
                ("c", "Chat"),
                ("e", "Scratchpad"),
                ("t", "Terminal"),
                ("s", "Snippets"),
                (prefix.as_str(), "Command search"),
            ],
        ),
        (
            "Sessions",
            vec![
                ("w", "Switch"),
                ("n / p", "Next / previous"),
                ("m", "Move tab"),
                ("1-9", "Jump to tab"),
                ("d", "Session list"),
            ],
        ),
        (
            "Tools",
            vec![
                ("g i", "GitHub inspector"),
                ("h e", "Scratchpad help"),
                ("u", "Update CST"),
                ("", ""),
                ("", ""),
            ],
        ),
        (
            "Lifecycle",
            vec![
                ("x", "Close session tab"),
                (tmux_end, "End persistent session"),
                ("q", "Quit CST"),
                ("", ""),
                ("", ""),
            ],
        ),
    ];
    for (column, (title, commands)) in columns.iter().zip(groups) {
        let mut lines = vec![
            Line::from(Span::styled(
                format!(" {title}"),
                Style::default()
                    .fg(theme.warning)
                    .add_modifier(Modifier::BOLD),
            )),
            Line::from(""),
        ];
        for (key, label) in commands {
            if key.is_empty() {
                lines.push(Line::from(""));
            } else {
                lines.push(command_hint(key, label, theme));
            }
        }
        f.render_widget(Paragraph::new(lines).style(surface(theme)), *column);
    }
}

fn draw_command_palette(f: &mut Frame, app: &mut App) {
    let theme = app.theme();
    let commands = crate::command_palette::filtered_commands(app);
    let prefix = app
        .mux
        .as_ref()
        .map(|mux| mux.prefix.label())
        .unwrap_or_else(|| "C-b".to_string());
    let area = bottom_overlay(f.area(), 118, 22);
    prepare(f, area, theme);
    let block = Block::default()
        .title(format!(" Command Search · {prefix} {prefix} "))
        .borders(Borders::ALL)
        .style(surface(theme))
        .border_style(Style::default().fg(theme.accent_alt));
    let inner = block.inner(area);
    f.render_widget(block, area);
    let sections = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(2),
            Constraint::Min(1),
            Constraint::Length(1),
        ])
        .split(inner);

    let query = app
        .command_palette
        .as_ref()
        .map(|palette| palette.query.clone())
        .unwrap_or_default();
    f.render_widget(
        Paragraph::new(Line::from(vec![
            Span::styled(
                " Search: ",
                Style::default()
                    .fg(theme.warning)
                    .add_modifier(Modifier::BOLD),
            ),
            Span::styled(query, Style::default().fg(theme.text)),
            Span::styled("█", Style::default().fg(theme.accent)),
            Span::styled(
                format!("  {} commands", commands.len()),
                Style::default().fg(theme.muted),
            ),
        ]))
        .style(surface(theme)),
        sections[0],
    );

    let visible_rows = sections[1].height as usize;
    let Some(palette) = app.command_palette.as_mut() else {
        return;
    };
    palette.visible_rows = visible_rows;
    palette.set_selected(palette.selected, commands.len());
    palette.hits.clear();

    if commands.is_empty() {
        f.render_widget(
            Paragraph::new(Line::from(Span::styled(
                " No matching commands",
                Style::default().fg(theme.muted),
            )))
            .style(surface(theme)),
            sections[1],
        );
    } else {
        for (row, command) in commands
            .iter()
            .enumerate()
            .skip(palette.scroll)
            .take(visible_rows)
        {
            let y = sections[1].y + (row - palette.scroll) as u16;
            let row_area = Rect::new(sections[1].x, y, sections[1].width, 1);
            palette.hits.push((row_area, command.id));
            let selected = row == palette.selected;
            let base = if selected {
                Style::default()
                    .fg(theme.selection_fg)
                    .bg(theme.selection_bg)
                    .add_modifier(Modifier::BOLD)
            } else if command.enabled {
                Style::default().fg(theme.text).bg(theme.surface)
            } else {
                Style::default().fg(theme.muted).bg(theme.surface)
            };
            let group_width = 13;
            let shortcut_width = 12;
            let title_width = 28;
            let description_width = row_area
                .width
                .saturating_sub(group_width + shortcut_width + title_width + 4)
                as usize;
            let group = format!("{:<12}", command.group.label());
            let shortcut = format!("{:<11}", command.shortcut);
            let title = format!(
                "{:<27}",
                text::truncate_to_width(command.title, title_width as usize - 1)
            );
            let description = text::truncate_to_width(command.description, description_width);
            let group_color = group_color(command.group, theme);
            f.render_widget(
                Paragraph::new(Line::from(vec![
                    Span::styled(
                        format!(" {group}"),
                        if selected {
                            base
                        } else {
                            base.fg(group_color).add_modifier(Modifier::BOLD)
                        },
                    ),
                    Span::styled(shortcut, base),
                    Span::styled(title, base),
                    Span::styled(description, base),
                ]))
                .style(base),
                row_area,
            );
        }
    }

    let footer = palette.error.as_deref().or_else(|| {
        commands
            .get(palette.selected)
            .and_then(|command| command.unavailable_reason)
    });
    let footer = footer
        .map(|message| {
            Line::from(Span::styled(
                format!(" {message}"),
                Style::default().fg(theme.warning),
            ))
        })
        .unwrap_or_else(|| {
            Line::from(vec![
                Span::styled(" ↑↓", key_style(theme)),
                Span::raw(" select  "),
                Span::styled("Enter/click", key_style(theme)),
                Span::raw(" run  "),
                Span::styled("Esc", key_style(theme)),
                Span::raw(" close"),
            ])
        });
    f.render_widget(Paragraph::new(footer).style(surface(theme)), sections[2]);
}

fn command_hint(key: &str, label: &str, theme: Theme) -> Line<'static> {
    Line::from(vec![
        Span::raw(" "),
        Span::styled(format!("{key:<7}"), key_style(theme)),
        Span::styled(label.to_string(), Style::default().fg(theme.text)),
    ])
}

fn group_color(group: CommandGroup, theme: Theme) -> ratatui::style::Color {
    match group {
        CommandGroup::Workspace => theme.accent_alt,
        CommandGroup::Sessions => theme.info,
        CommandGroup::GitHub => theme.success,
        CommandGroup::View => theme.directory,
        CommandGroup::Settings => theme.warning,
        CommandGroup::Lifecycle => theme.error,
    }
}

fn key_style(theme: Theme) -> Style {
    Style::default()
        .fg(theme.accent_alt)
        .add_modifier(Modifier::BOLD)
}

fn surface(theme: Theme) -> Style {
    Style::default().fg(theme.text).bg(theme.surface)
}

fn prepare(f: &mut Frame, area: Rect, theme: Theme) {
    f.render_widget(Clear, area);
    fill_area(f.buffer_mut(), area, theme.surface);
}

fn bottom_overlay(frame: Rect, max_width: u16, preferred_height: u16) -> Rect {
    let width = max_width.min(frame.width.saturating_sub(2)).max(1);
    let height = preferred_height.min(frame.height.saturating_sub(2)).max(1);
    Rect::new(
        frame.x + frame.width.saturating_sub(width) / 2,
        frame.bottom().saturating_sub(height + 1),
        width,
        height,
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::UserConfig;
    use ratatui::backend::TestBackend;
    use ratatui::Terminal;

    fn render(app: &mut App) -> String {
        let mut terminal = Terminal::new(TestBackend::new(120, 30)).unwrap();
        terminal
            .draw(|frame| {
                let area = frame.area();
                fill_area(frame.buffer_mut(), area, app.theme().background);
                draw_overlays(frame, app);
            })
            .unwrap();
        terminal
            .backend()
            .buffer()
            .content()
            .iter()
            .map(|cell| cell.symbol())
            .collect()
    }

    #[test]
    fn prefix_menu_is_grouped_instead_of_one_long_hint() {
        let mut app = App::new(
            Vec::new(),
            UserConfig {
                mux: true,
                ..UserConfig::default()
            },
        );
        app.mux.as_mut().unwrap().prefix_state = PrefixState::Root;

        let text = render(&mut app);

        for heading in ["Workspace", "Sessions", "Tools", "Lifecycle"] {
            assert!(text.contains(heading), "missing {heading}:\n{text}");
        }
        assert!(text.contains("Command search"), "got:\n{text}");
        assert!(text.contains("GitHub inspector"), "got:\n{text}");
    }

    #[test]
    fn command_palette_shows_search_results_and_disabled_reason() {
        let mut app = App::new(Vec::new(), UserConfig::default());
        app.open_command_palette();

        let text = render(&mut app);

        assert!(text.contains("Command Search"), "got:\n{text}");
        assert!(text.contains("Focus chat"), "got:\n{text}");
        assert!(
            text.contains("Requires an attached running session"),
            "got:\n{text}"
        );
        assert!(!app.command_palette.as_ref().unwrap().hits.is_empty());
    }
}

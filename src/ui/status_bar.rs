use ratatui::layout::Rect;
use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::Paragraph;
use ratatui::Frame;

use crate::app::{App, Mode};

pub fn draw(f: &mut Frame, app: &App, area: Rect) {
    let theme = app.theme();
    let chrome_text = super::foreground_on(theme, theme.chrome_bg);
    let line1 = match app.mode {
        Mode::Search => Line::from(vec![
            Span::styled(
                " / ",
                Style::default()
                    .fg(super::badge_foreground(theme, theme.warning))
                    .bg(theme.warning),
            ),
            Span::raw(" "),
            Span::styled(
                &app.search_query,
                Style::default().fg(super::semantic_foreground_on(
                    theme,
                    theme.warning,
                    theme.chrome_bg,
                )),
            ),
            Span::styled(
                "█",
                Style::default().fg(super::semantic_foreground_on(
                    theme,
                    theme.warning,
                    theme.chrome_bg,
                )),
            ),
            Span::raw("  "),
            key_span("Enter", theme),
            Span::raw(" confirm  "),
            key_span("Esc", theme),
            Span::raw(" cancel"),
        ]),
        // A grabbed favorite remaps most keys, so the usual hints would lie.
        Mode::Normal if app.grabbed_favorite.is_some() => Line::from(vec![
            Span::raw(" "),
            key_span("↑↓", theme),
            Span::raw(" Move favorite  "),
            key_span("Enter/g", theme),
            Span::raw(" Drop & save"),
        ]),
        Mode::Normal => {
            let mut spans = vec![
                Span::raw(" "),
                key_span("↑↓", theme),
                Span::raw(" Navigate  "),
                key_span("Enter", theme),
                Span::raw(" Resume  "),
                key_span("n", theme),
                Span::raw(" New  "),
                key_span("r", theme),
                Span::raw(" Rename  "),
                key_span("d", theme),
                Span::raw(" Delete  "),
                key_span("/", theme),
                Span::raw(" Search  "),
                key_span("f", theme),
                Span::raw(" Filter  "),
                key_span("s", theme),
                Span::raw(" Sort"),
            ];
            if let Some(prefix) = app.prefix_label() {
                spans.push(Span::raw("  │  "));
                if app.help_pending() {
                    spans.push(Span::styled(
                        "Help: e scratchpad  Esc cancel",
                        Style::default()
                            .fg(super::badge_foreground(theme, theme.accent))
                            .bg(theme.accent)
                            .add_modifier(Modifier::BOLD),
                    ));
                } else if app.github_prefix_pending() {
                    spans.push(Span::styled(
                        "GitHub: i inspect  Esc cancel",
                        Style::default()
                            .fg(super::badge_foreground(theme, theme.accent))
                            .bg(theme.accent)
                            .add_modifier(Modifier::BOLD),
                    ));
                } else if app.prefix_pending() {
                    spans.push(Span::styled(
                        format!("{prefix} …"),
                        Style::default()
                            .fg(super::badge_foreground(theme, theme.accent))
                            .bg(theme.accent)
                            .add_modifier(Modifier::BOLD),
                    ));
                } else {
                    let running = app.running_pane_count();
                    let label = if running > 0 {
                        format!("{prefix} w  {running} running")
                    } else {
                        format!("mux {prefix}")
                    };
                    spans.push(Span::styled(
                        label,
                        Style::default().fg(super::semantic_foreground_on(
                            theme,
                            theme.accent,
                            theme.chrome_bg,
                        )),
                    ));
                }
            }
            Line::from(spans)
        }
        _ => Line::from(""),
    };

    let line2 = match app.mode {
        Mode::Normal if app.grabbed_favorite.is_none() => {
            let mut spans = vec![
                Span::raw(" "),
                key_span("Space", theme),
                Span::raw(" Favorite  "),
            ];
            // Keys that cannot do anything right now stay out of the bar; the full
            // list lives behind `?`.
            if app.selected_favorite_reorderable() {
                spans.push(key_span("g", theme));
                spans.push(Span::raw(" Reorder  "));
            }
            if app.project_filter.is_some() {
                spans.push(key_span("c", theme));
                spans.push(Span::raw(" Clear filter  "));
            }
            spans.extend([
                key_span("T", theme),
                Span::raw(" Favorite tabs  "),
                key_span(",", theme),
                Span::raw(" Global settings  "),
                key_span(".", theme),
                Span::raw(" Project settings  "),
                key_span("?", theme),
                Span::raw(" Help  "),
                key_span("q", theme),
                Span::raw(" Quit"),
            ]);
            if let Some(info) = app
                .update_info
                .as_ref()
                .filter(|_| app.update_install_receiver.is_none())
            {
                spans.push(Span::raw("  │  "));
                spans.push(Span::styled(
                    format!("⬆ v{} → v{} ", info.current_version, info.latest_version),
                    Style::default()
                        .fg(super::semantic_foreground_on(
                            theme,
                            theme.success,
                            theme.chrome_bg,
                        ))
                        .add_modifier(Modifier::BOLD),
                ));
                spans.push(key_span("u", theme));
                spans.push(Span::raw(" Update"));
            }
            if let Some(ref msg) = app.status_message {
                spans.push(Span::raw("  │  "));
                spans.push(Span::styled(
                    msg.as_str(),
                    Style::default()
                        .fg(super::semantic_foreground_on(
                            theme,
                            theme.warning,
                            theme.chrome_bg,
                        ))
                        .add_modifier(Modifier::BOLD),
                ));
            }
            Line::from(spans)
        }
        _ => {
            if let Some(ref msg) = app.status_message {
                Line::from(Span::styled(
                    format!(" {}", msg),
                    Style::default()
                        .fg(super::semantic_foreground_on(
                            theme,
                            theme.warning,
                            theme.chrome_bg,
                        ))
                        .add_modifier(Modifier::BOLD),
                ))
            } else {
                Line::from("")
            }
        }
    };

    let paragraph = Paragraph::new(vec![line1, line2])
        .style(Style::default().fg(chrome_text).bg(theme.chrome_bg));

    f.render_widget(paragraph, area);
}

fn key_span(key: &str, theme: crate::theme::Theme) -> Span<'_> {
    Span::styled(
        key,
        Style::default()
            .fg(super::semantic_foreground_on(
                theme,
                theme.accent_alt,
                theme.chrome_bg,
            ))
            .add_modifier(Modifier::BOLD),
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::UserConfig;
    use crate::theme::ThemeName;
    use ratatui::backend::TestBackend;
    use ratatui::Terminal;

    fn rendered(app: &App) -> String {
        let backend = TestBackend::new(180, 2);
        let mut terminal = Terminal::new(backend).unwrap();
        terminal
            .draw(|frame| draw(frame, app, frame.area()))
            .unwrap();
        terminal
            .backend()
            .buffer()
            .content()
            .iter()
            .map(|cell| cell.symbol())
            .collect()
    }

    fn session(id: &str) -> crate::session::Session {
        crate::session::Session {
            id: id.to_string(),
            cwd: "C:/Workspace/zazen".to_string(),
            project_root: "C:/Workspace/zazen".to_string(),
            summary: Some(id.to_string()),
            created_at: None,
            updated_at: None,
            is_active: false,
            dir_path: std::path::PathBuf::from("."),
            edited_files: Vec::new(),
            last_user_message: None,
            turn_count: 0,
            tool_call_count: 0,
            details_parsed_len: 0,
        }
    }

    #[test]
    fn session_list_footer_advertises_favorite_tabs_shortcut() {
        let app = App::new(Vec::new(), UserConfig::default());
        let text = rendered(&app);
        assert!(text.contains("T Favorite tabs"), "got:\n{text}");
    }

    #[test]
    fn footer_keeps_rare_keys_behind_the_help_screen() {
        let app = App::new(Vec::new(), UserConfig::default());
        let text = rendered(&app);
        assert!(text.contains("n New"), "got:\n{text}");
        assert!(
            !text.contains("Scratchpad") && !text.contains("N Worktree"),
            "rarely used keys belong in ? instead of the footer:\n{text}"
        );
    }

    #[test]
    fn clear_filter_hint_appears_only_while_a_filter_is_active() {
        let mut app = App::new(vec![session("a")], UserConfig::default());
        app.disable_config_persistence();
        assert!(
            !rendered(&app).contains("c Clear filter"),
            "there is nothing to clear yet"
        );

        app.set_project_filter(Some("C:/Workspace/zazen".to_string()));
        assert!(rendered(&app).contains("c Clear filter"));
    }

    #[test]
    fn reorder_hint_appears_only_while_a_reorderable_favorite_is_selected() {
        let config = UserConfig {
            favorites: vec!["fav".to_string()],
            ..UserConfig::default()
        };
        let mut app = App::new(vec![session("fav"), session("plain")], config);
        app.disable_config_persistence();

        // Favorites sort first, so selection 0 is the favorite.
        app.selected = 0;
        assert!(rendered(&app).contains("g Reorder"));

        app.selected = 1;
        assert!(
            !rendered(&app).contains("g Reorder"),
            "g does nothing on a non-favorite, so it must not be advertised"
        );
    }

    #[test]
    fn footer_swaps_to_move_hints_while_a_favorite_is_grabbed() {
        let config = UserConfig {
            favorites: vec!["fav".to_string()],
            ..UserConfig::default()
        };
        let mut app = App::new(vec![session("fav")], config);
        app.disable_config_persistence();
        app.selected = 0;
        app.toggle_favorite_grab();
        assert!(app.grabbed_favorite.is_some());

        let text = rendered(&app);
        assert!(text.contains("Move favorite"), "got:\n{text}");
        assert!(
            !text.contains("Enter Resume"),
            "Enter drops the favorite here, it does not resume:\n{text}"
        );
    }

    #[test]
    fn solarized_light_status_bar_paints_chrome_and_readable_default_text() {
        let app = App::new(
            Vec::new(),
            UserConfig {
                theme: ThemeName::SolarizedLight,
                ..UserConfig::default()
            },
        );
        let mut terminal = Terminal::new(TestBackend::new(180, 2)).unwrap();

        terminal
            .draw(|frame| draw(frame, &app, frame.area()))
            .unwrap();

        let theme = app.theme();
        let buffer = terminal.backend().buffer();
        assert!(buffer
            .content()
            .iter()
            .all(|cell| cell.bg == theme.chrome_bg));
        let navigate = &buffer[(4, 0)];
        assert_eq!(navigate.symbol(), "N");
        assert_eq!(
            navigate.fg,
            crate::ui::foreground_on(theme, theme.chrome_bg)
        );
    }
}

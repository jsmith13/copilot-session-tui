use ratatui::layout::Rect;
use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, List, ListItem};
use ratatui::Frame;

use crate::app::App;
use crate::text;

pub fn draw(f: &mut Frame, app: &App, area: Rect) {
    let theme = app.theme();
    let selection_style = super::row_selection_style(theme);
    let selection_fg = selection_style.fg.unwrap_or(theme.selection_fg);
    let block = Block::default()
        .title(" Sessions ")
        .borders(Borders::ALL)
        .style(Style::default().fg(theme.text).bg(theme.background))
        .border_style(Style::default().fg(theme.muted));

    let inner = block.inner(area);
    f.render_widget(block, area);

    if app.filtered_indices.is_empty() {
        let hint = if app.new_session_dir().is_some() {
            "  No sessions found\n\n  n  Start a new session here\n  N  Start an isolated worktree session"
        } else {
            "  No sessions found"
        };
        let empty = ratatui::widgets::Paragraph::new(hint).style(
            Style::default()
                .fg(super::semantic_foreground_on(
                    theme,
                    theme.muted,
                    theme.background,
                ))
                .bg(theme.background),
        );
        f.render_widget(empty, inner);
        return;
    }

    let has_project_filter = app.project_filter.is_some();
    // The mail column only exists while something is waiting, so the usual list keeps
    // its full width for names. Most people will never see it.
    let mail_column = !app.thread_pending.is_empty();
    let lines_per_item = if has_project_filter { 1 } else { 2 };
    let grouped = app.favorites_section_active();
    let budget = (inner.height as usize).saturating_sub(app.list_header_lines());
    let visible_items = budget / lines_per_item;

    let mut items: Vec<ListItem> = Vec::new();
    for (display_idx, &real_idx) in app
        .filtered_indices
        .iter()
        .enumerate()
        .skip(app.scroll_offset)
        .take(visible_items)
    {
        {
            let session = &app.sessions[real_idx];
            let is_selected = display_idx == app.selected;
            let is_favorite = app.is_favorite(&session.id);
            let is_grabbed = app.grabbed_favorite.as_deref() == Some(session.id.as_str());
            let tmux_backed = app.session_is_tmux_backed(&session.id);

            // Panes we own read differently from a foreign `inuse` lock: ours can be
            // re-attached, theirs cannot.
            let active_indicator = if app.has_running_pane_for(&session.id) {
                Span::styled(
                    if tmux_backed { "◆ " } else { "▶ " },
                    Style::default().fg(if is_selected {
                        selection_fg
                    } else {
                        super::semantic_foreground_on(theme, theme.accent, theme.background)
                    }),
                )
            } else if tmux_backed {
                Span::styled(
                    "◇ ",
                    Style::default().fg(if is_selected {
                        selection_fg
                    } else {
                        super::semantic_foreground_on(theme, theme.accent_alt, theme.background)
                    }),
                )
            } else if session.is_active {
                Span::styled(
                    "● ",
                    Style::default().fg(if is_selected {
                        selection_fg
                    } else {
                        super::semantic_foreground_on(theme, theme.success, theme.background)
                    }),
                )
            } else {
                Span::raw("  ")
            };
            // While an item is grabbed it replaces the star, so the row being moved
            // is obvious even as it travels past other favorites.
            let favorite_indicator = if is_grabbed {
                Span::styled(
                    "⇕ ",
                    Style::default()
                        .fg(super::badge_foreground(theme, theme.warning))
                        .bg(theme.warning)
                        .add_modifier(Modifier::BOLD),
                )
            } else if is_favorite {
                Span::styled(
                    "★ ",
                    Style::default().fg(if is_selected {
                        selection_fg
                    } else {
                        super::semantic_foreground_on(theme, theme.warning, theme.background)
                    }),
                )
            } else {
                Span::raw("  ")
            };

            // A message waiting on the user, marked with `@` because it came from
            // somebody. ASCII on purpose: the smoke harness reads this screen back
            // through a PTY, where non-ASCII glyphs do not survive the trip.
            let pending = app.thread_pending_for(&session.id);
            let mail_indicator = if !mail_column {
                None
            } else if pending.is_some() {
                Some(Span::styled(
                    "@ ",
                    Style::default()
                        .fg(if is_selected {
                            selection_fg
                        } else {
                            super::semantic_foreground_on(theme, theme.warning, theme.background)
                        })
                        .add_modifier(Modifier::BOLD),
                ))
            } else {
                Some(Span::raw("  "))
            };

            let name = session.display_name();
            // 4 columns for indicators, 1 space + 8 columns for the time column, and
            // two more only while some session is actually holding a message.
            let max_name_width = (inner.width as usize)
                .saturating_sub(13)
                .saturating_sub(if mail_column { 2 } else { 0 });
            let truncated_name = text::truncate_to_width(name, max_name_width);

            let name_style = if is_selected {
                Style::default()
                    .fg(selection_fg)
                    .add_modifier(Modifier::BOLD)
            } else {
                Style::default().fg(theme.text)
            };

            // How long something has been waiting matters more than when the session
            // was last used, and it is the thing that turns a silent stall into
            // something visible.
            let time = match pending {
                Some(pending) => waiting_for(pending.planned_at),
                None => session.relative_time(),
            };
            let time_style = Style::default().fg(if is_selected {
                selection_fg
            } else if pending.is_some() {
                super::semantic_foreground_on(theme, theme.warning, theme.background)
            } else {
                super::semantic_foreground_on(theme, theme.muted, theme.background)
            });

            let mut spans = vec![active_indicator, favorite_indicator];
            spans.extend(mail_indicator);
            spans.push(Span::styled(
                text::pad_to_width(&truncated_name, max_name_width),
                name_style,
            ));
            spans.push(Span::styled(format!(" {:>8}", time), time_style));
            let line = Line::from(spans);

            let lines = if has_project_filter {
                vec![line]
            } else {
                let project = session.project_name();
                // The project line has no time column, so it can use everything the
                // indent leaves rather than a fixed budget that clips common names.
                let max_project_width = (inner.width as usize).saturating_sub(5);
                let truncated_project = text::truncate_to_width(project, max_project_width);
                let project_line = Line::from(vec![
                    Span::raw("    "),
                    Span::styled(
                        truncated_project,
                        Style::default()
                            .fg(if is_selected {
                                selection_fg
                            } else {
                                super::semantic_foreground_on(
                                    theme,
                                    theme.directory,
                                    theme.background,
                                )
                            })
                            .add_modifier(if theme.is_light {
                                Modifier::empty()
                            } else {
                                Modifier::DIM
                            }),
                    ),
                ]);
                vec![line, project_line]
            };

            // Headers ride on the first row of each group rather than occupying list
            // entries of their own, so every entry still maps to exactly one session
            // and the selection index needs no translation.
            if grouped {
                if display_idx == 0 && is_favorite {
                    items.push(header(
                        "★ Favorites",
                        super::semantic_foreground_on(theme, theme.warning, theme.background),
                    ));
                } else if !is_favorite
                    && app
                        .filtered_indices
                        .get(display_idx.wrapping_sub(1))
                        .is_some_and(|&previous| app.is_favorite(&app.sessions[previous].id))
                {
                    items.push(header(
                        "Sessions",
                        super::semantic_foreground_on(theme, theme.muted, theme.background),
                    ));
                }
            }

            if is_selected {
                items.push(ListItem::new(lines).style(selection_style));
            } else {
                items.push(ListItem::new(lines));
            }
        }
    }

    let list = List::new(items).style(Style::default().fg(theme.text).bg(theme.background));
    f.render_widget(list, inner);
}

/// A one-line group label separating the arranged favorites from everything else.
/// How long a message has been waiting, in the eight columns the time slot allows.
///
/// Says "waiting" rather than "ago" because it is not a record of something that
/// happened, it is a thing still undone. A correspondence that quietly stalls for days
/// is the failure this whole feature exists to prevent, so the number has to be visible
/// without opening anything.
fn waiting_for(arrived_at: chrono::DateTime<chrono::Utc>) -> String {
    let waited = chrono::Utc::now().signed_duration_since(arrived_at);
    if waited.num_minutes() < 1 {
        "now".to_string()
    } else if waited.num_minutes() < 60 {
        format!("{}m wait", waited.num_minutes())
    } else if waited.num_hours() < 24 {
        format!("{}h wait", waited.num_hours())
    } else {
        format!("{}d wait", waited.num_days())
    }
}

fn header(label: &str, color: ratatui::style::Color) -> ListItem<'static> {
    ListItem::new(Line::from(Span::styled(
        format!("  {label}"),
        Style::default()
            .fg(color)
            .add_modifier(Modifier::BOLD | Modifier::UNDERLINED),
    )))
}

#[cfg(test)]
pub mod tests {
    use crate::app::App;
    use crate::config::UserConfig;
    use crate::session::Session;
    use crate::theme::ThemeName;
    use ratatui::backend::TestBackend;
    use ratatui::Terminal;
    use std::path::PathBuf;

    fn session_named(name: &str) -> Session {
        Session {
            id: "abcdef123456".to_string(),
            cwd: "C:/Workspace/zazen".to_string(),
            project_root: "C:/Workspace/zazen".to_string(),
            summary: Some(name.to_string()),
            created_at: None,
            updated_at: None,
            is_active: false,
            dir_path: PathBuf::from("."),
            edited_files: Vec::new(),
            last_user_message: None,
            turn_count: 0,
            tool_call_count: 0,
            details_parsed_len: 0,
        }
    }

    pub fn render_with(name: &str, width: u16) -> String {
        let mut app = App::new(vec![session_named(name)], UserConfig::default());
        let backend = TestBackend::new(width, 12);
        let mut terminal = Terminal::new(backend).expect("terminal");
        terminal
            .draw(|f| crate::ui::draw(f, &mut app))
            .expect("draw succeeds");
        let buffer = terminal.backend().buffer().clone();
        buffer
            .content()
            .iter()
            .map(|cell| cell.symbol())
            .collect::<String>()
    }

    #[test]
    fn an_emoji_name_renders_without_panicking() {
        let text = render_with("🚀 Ship it", 60);
        assert!(text.contains("🚀"), "emoji must survive rendering:\n{text}");
    }

    #[test]
    fn a_long_emoji_name_is_truncated_on_a_character_boundary() {
        // Long enough to force truncation, with the emoji straddling the cut.
        let name = "Publishing to f-droid 🚀 and elsewhere too, at length";
        let text = render_with(name, 40);
        assert!(text.contains("Publishing"), "got:\n{text}");
    }

    #[test]
    fn tmux_owned_sessions_have_a_persistent_marker() {
        let mut app = App::new(
            vec![session_named("Persistent session")],
            UserConfig::default(),
        );
        app.replace_tmux_session(crate::session::tmux::TmuxSessionRef {
            session_id: "abcdef123456".to_string(),
            tmux_session: "cst-persistent-abcdef12".to_string(),
            cwd: PathBuf::from("C:/Workspace/zazen"),
            server_socket: None,
        });
        let mut terminal = Terminal::new(TestBackend::new(60, 12)).unwrap();

        terminal
            .draw(|frame| crate::ui::draw(frame, &mut app))
            .unwrap();

        let text = terminal
            .backend()
            .buffer()
            .content()
            .iter()
            .map(|cell| cell.symbol())
            .collect::<String>();
        assert!(text.contains('◇'), "got:\n{text}");
    }

    /// Rows as grids of cell symbols, so positions can be compared in terminal columns
    /// rather than bytes — an emoji is one cell but four bytes.
    pub fn render_cells(name: &str, width: u16) -> Vec<Vec<String>> {
        let mut app = App::new(vec![session_named(name)], UserConfig::default());
        let backend = TestBackend::new(width, 12);
        let mut terminal = Terminal::new(backend).expect("terminal");
        terminal
            .draw(|f| crate::ui::draw(f, &mut app))
            .expect("draw succeeds");
        let buffer = terminal.backend().buffer().clone();
        (0..buffer.area.height)
            .map(|y| {
                (0..buffer.area.width)
                    .map(|x| buffer[(x, y)].symbol().to_string())
                    .collect()
            })
            .collect()
    }

    /// Render one session that has a message waiting, as a grid of cells.
    fn render_cells_with_pending(
        name: &str,
        width: u16,
        waited: chrono::Duration,
    ) -> Vec<Vec<String>> {
        let mut app = App::new(vec![session_named(name)], UserConfig::default());
        app.thread_pending = vec![crate::threads::Notice {
            session_id: "abcdef123456".to_string(),
            thread: crate::threads::ThreadRef {
                host: "github.com".to_string(),
                owner: "o".to_string(),
                repo: "r".to_string(),
                number: 12,
                kind: crate::threads::ThreadKind::Issue,
            },
            comment_url: "https://github.com/o/r/issues/12".to_string(),
            status: crate::threads::NoticeStatus::Waiting {
                reason: crate::threads::PendingReason::SessionClosed,
            },
            author: None,
            planned_at: chrono::Utc::now() - waited,
        }];
        let backend = TestBackend::new(width, 12);
        let mut terminal = Terminal::new(backend).expect("terminal");
        terminal
            .draw(|f| crate::ui::draw(f, &mut app))
            .expect("draw succeeds");
        let buffer = terminal.backend().buffer().clone();
        (0..buffer.area.height)
            .map(|y| {
                (0..buffer.area.width)
                    .map(|x| buffer[(x, y)].symbol().to_string())
                    .collect()
            })
            .collect()
    }

    /// Column at which `needle` starts, counted in cells.
    fn column_of(rows: &[Vec<String>], needle: &str) -> usize {
        for row in rows {
            for start in 0..row.len() {
                if row[start..].concat().starts_with(needle) {
                    return start;
                }
            }
        }
        panic!("{needle:?} was not on screen");
    }

    #[test]
    fn emoji_names_do_not_panic_at_any_terminal_width() {
        // Byte-slicing a name used to abort here; the emoji straddles every cut point.
        let name = "🚀🚀🚀🚀🚀🚀🚀🚀🚀🚀🚀🚀🚀🚀🚀🚀🚀🚀🚀🚀 rocket session";
        for width in 20u16..=140 {
            let rendered = render_with(name, width);
            assert!(!rendered.is_empty(), "width {width} produced no output");
        }
    }

    #[test]
    fn an_emoji_name_keeps_the_timestamp_column_aligned() {
        // The emoji occupies two columns; if it is counted as one char the whole row
        // shifts and the right-hand time column no longer lines up.
        let plain = render_cells("ab plain name", 60);
        let emoji = render_cells("🚀 plain name", 60);

        assert_eq!(
            column_of(&plain, "unknown"),
            column_of(&emoji, "unknown"),
            "the emoji shifted the time column"
        );
    }

    #[test]
    fn a_waiting_message_keeps_the_right_hand_column_aligned() {
        // Same hazard as the emoji test above. The mail column changes the name budget,
        // so getting the arithmetic wrong here would break every row's alignment for
        // anyone using this feature.
        let rows = render_cells_with_pending("plain name", 60, chrono::Duration::days(3));
        let widths: Vec<usize> = rows
            .iter()
            .map(|row| row.concat().chars().count())
            .collect();

        assert!(
            widths.windows(2).all(|pair| pair[0] == pair[1]),
            "rows must stay the same width: {widths:?}"
        );
    }

    #[test]
    fn a_message_waiting_for_days_says_so_where_the_time_would_be() {
        // The whole point: a correspondence that quietly stalls has to be visible
        // without opening anything, or the feature has reinvented its own problem.
        let rows = render_cells_with_pending("plain name", 60, chrono::Duration::days(3));
        let screen: String = rows.iter().map(|row| row.concat()).collect();

        assert!(screen.contains("3d wait"), "got: {screen}");
        assert!(
            screen.contains('@'),
            "the row must be marked as holding a message: {screen}"
        );
    }

    #[test]
    fn a_session_with_nothing_waiting_still_lines_up_beside_one_that_has() {
        // The mail column is reserved for the whole list once anything is waiting, so a
        // row that pads it differently from one that fills it would leave the list
        // visibly ragged — the same failure the emoji test above guards.
        let mut app = App::new(numbered_sessions(3), UserConfig::default());
        app.thread_pending = vec![crate::threads::Notice {
            session_id: "id-1".to_string(),
            thread: crate::threads::ThreadRef {
                host: "github.com".to_string(),
                owner: "o".to_string(),
                repo: "r".to_string(),
                number: 12,
                kind: crate::threads::ThreadKind::Issue,
            },
            comment_url: "https://github.com/o/r/issues/12".to_string(),
            status: crate::threads::NoticeStatus::Waiting {
                reason: crate::threads::PendingReason::SessionClosed,
            },
            author: None,
            planned_at: chrono::Utc::now(),
        }];

        let backend = TestBackend::new(60, 14);
        let mut terminal = Terminal::new(backend).expect("terminal");
        terminal
            .draw(|f| crate::ui::draw(f, &mut app))
            .expect("draw succeeds");
        let buffer = terminal.backend().buffer().clone();
        let rows: Vec<String> = (0..buffer.area.height)
            .map(|y| {
                (0..buffer.area.width)
                    .map(|x| buffer[(x, y)].symbol().to_string())
                    .collect()
            })
            .collect();

        let widths: Vec<usize> = rows.iter().map(|row| row.chars().count()).collect();
        assert!(
            widths.windows(2).all(|pair| pair[0] == pair[1]),
            "rows must stay the same width: {widths:?}"
        );
        assert_eq!(
            rows.iter().filter(|row| row.contains('@')).count(),
            1,
            "only the session holding a message is marked"
        );
    }

    /// A list of `count` sessions with predictable ids and names.
    fn numbered_sessions(count: usize) -> Vec<Session> {
        (0..count)
            .map(|index| {
                let mut session = session_named(&format!("Session {index}"));
                session.id = format!("id-{index}");
                session
            })
            .collect()
    }

    fn render_app(app: &mut App, width: u16, height: u16) -> String {
        let backend = TestBackend::new(width, height);
        let mut terminal = Terminal::new(backend).expect("terminal");
        terminal
            .draw(|f| crate::ui::draw(f, app))
            .expect("draw succeeds");
        terminal
            .backend()
            .buffer()
            .content()
            .iter()
            .map(|cell| cell.symbol())
            .collect()
    }

    fn config_with_favorites(ids: &[&str]) -> UserConfig {
        UserConfig {
            favorites: ids.iter().map(|id| id.to_string()).collect(),
            ..Default::default()
        }
    }

    #[test]
    fn both_group_headers_are_drawn_when_favorites_lead_the_list() {
        let mut app = App::new(
            numbered_sessions(4),
            config_with_favorites(&["id-2", "id-0"]),
        );

        let text = render_app(&mut app, 60, 20);

        assert!(text.contains("★ Favorites"), "got:\n{text}");
        // Once for the surrounding block title, once for the group label.
        assert_eq!(text.matches("Sessions").count(), 2, "got:\n{text}");
    }

    #[test]
    fn the_trailing_header_is_omitted_when_every_session_is_a_favorite() {
        let mut app = App::new(
            numbered_sessions(2),
            config_with_favorites(&["id-0", "id-1"]),
        );

        let text = render_app(&mut app, 60, 20);

        assert!(text.contains("★ Favorites"), "got:\n{text}");
        // Only the surrounding block title, never a group label with nothing under it.
        assert_eq!(text.matches("Sessions").count(), 1, "got:\n{text}");
    }

    #[test]
    fn no_headers_are_drawn_without_favorites() {
        let mut app = App::new(numbered_sessions(3), UserConfig::default());

        let text = render_app(&mut app, 60, 20);

        assert!(!text.contains("Favorites"), "got:\n{text}");
    }

    #[test]
    fn the_grabbed_row_is_marked_in_place_of_its_star() {
        let mut app = App::new(
            numbered_sessions(3),
            config_with_favorites(&["id-0", "id-1"]),
        );

        let before = render_app(&mut app, 60, 20);
        assert_eq!(before.matches('★').count(), 3, "got:\n{before}");

        app.grabbed_favorite = Some("id-1".to_string());
        let after = render_app(&mut app, 60, 20);

        // The marker replaces that row's star rather than adding a column.
        assert!(after.contains('⇕'), "got:\n{after}");
        assert_eq!(after.matches('★').count(), 2, "got:\n{after}");
    }

    #[test]
    fn light_theme_selected_row_keeps_markers_and_text_readable() {
        let mut session = session_named("Selected session");
        session.is_active = true;
        let app = App::new(
            vec![session],
            UserConfig {
                theme: ThemeName::CatppuccinLatte,
                favorites: vec!["abcdef123456".to_string()],
                ..UserConfig::default()
            },
        );
        let mut terminal = Terminal::new(TestBackend::new(60, 12)).unwrap();

        terminal
            .draw(|frame| super::draw(frame, &app, frame.area()))
            .unwrap();

        let theme = app.theme();
        let buffer = terminal.backend().buffer();
        for symbol in ["●", "★", "S"] {
            let cell = buffer
                .content()
                .iter()
                .find(|cell| cell.symbol() == symbol && cell.bg == theme.selection_bg)
                .unwrap_or_else(|| panic!("{symbol:?} was not rendered in the selected row"));
            assert_eq!(cell.fg, theme.selection_fg);
        }
    }
}

use ratatui::layout::Rect;
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Paragraph};
use ratatui::Frame;
use std::time::Duration;
use tui_term::widget::{Cursor, PseudoTerminal};

use crate::app::App;
use crate::mux::{PaneStatus, PrefixState};
use crate::text;
use crate::theme::{apply_terminal_theme, fill_area, Theme, ThemeName};
use crate::ui::tabs;

/// Frames of the startup spinner. Braille dots read as motion even in a plain terminal.
const SPINNER: [&str; 8] = ["⠋", "⠙", "⠹", "⠸", "⠼", "⠴", "⠦", "⠧"];

/// Progress indicator shown while a freshly spawned session has yet to draw anything.
fn draw_starting(f: &mut Frame, area: Rect, elapsed: Duration, theme: Theme) {
    let frame = SPINNER[(elapsed.as_millis() / 120) as usize % SPINNER.len()];
    let seconds = elapsed.as_secs();

    let mut lines = vec![
        Line::from(""),
        Line::from(Span::styled(
            format!("  {frame}  Starting Copilot…"),
            Style::default()
                .fg(theme.accent_alt)
                .add_modifier(Modifier::BOLD),
        )),
    ];
    // Only mention the wait once it is long enough to be worth reassuring about.
    if seconds >= 3 {
        lines.push(Line::from(Span::styled(
            format!("     {seconds}s — the CLI is still booting"),
            Style::default().fg(theme.muted),
        )));
    }

    let height = lines.len() as u16;
    let box_area = Rect {
        x: area.x,
        y: area.y,
        width: area.width,
        height: height.min(area.height),
    };
    f.render_widget(Paragraph::new(lines), box_area);
}

pub fn draw_chat(f: &mut Frame, app: &App, area: Rect) {
    let theme = app.theme();
    let Some(mux) = app.mux.as_ref() else {
        return;
    };
    let Some(pane) = mux.focused_pane() else {
        return;
    };

    let border_color = if app.workspace_focus == crate::app::WorkspaceFocus::Chat {
        theme.accent_alt
    } else {
        theme.inactive
    };
    let block = Block::default()
        .title(" Chat ")
        .borders(Borders::ALL)
        .style(panel_style(theme))
        .border_style(Style::default().fg(border_color));
    let terminal_area = block.inner(area);
    fill_area(f.buffer_mut(), area, theme.background);
    f.render_widget(block, area);

    // Copilot needs a few seconds before it draws anything; without this the pane just
    // looks frozen.
    let starting = pane.is_running() && pane.is_blank();

    // Paint the cursor into the frame rather than moving the terminal's real one.
    // The real cursor is drawn by the terminal on its own schedule, so while a frame
    // was being written it darted between every cell the diff touched; parking it for
    // the write traded that for a cursor that was hidden more often than not. A painted
    // cursor is just part of the frame, which is how the terminal and scratchpad panes
    // have always drawn theirs.
    let cursor = Cursor::default()
        .visibility(app.workspace_focus == crate::app::WorkspaceFocus::Chat && !starting);
    pane.with_screen(|screen| {
        let widget = PseudoTerminal::new(screen).cursor(cursor);
        f.render_widget(widget, terminal_area);
        apply_terminal_theme(f.buffer_mut(), terminal_area, theme);
    });
    decorate_references(f, app, terminal_area, theme);

    if starting {
        draw_starting(f, terminal_area, pane.started_at.elapsed(), theme);
    }
}

/// Colour and underline `#1234` in the rendered pane according to what it is.
///
/// This works on the already-drawn cells rather than the child's output, so it
/// stays correct however the terminal widget chose to lay the text out, and a
/// reference that is not yet resolved simply stays plain.
fn decorate_references(f: &mut Frame, app: &App, area: Rect, theme: Theme) {
    restyle_references(f.buffer_mut(), area, theme, &|number| {
        app.github_reference_status(number)
    });
}

fn restyle_references(
    buffer: &mut ratatui::buffer::Buffer,
    area: Rect,
    theme: Theme,
    lookup: &dyn Fn(u64) -> Option<crate::github::ReferenceStatus>,
) {
    if area.width == 0 || area.height == 0 {
        return;
    }
    for y in area.top()..area.bottom() {
        let mut x = area.left();
        while x < area.right() {
            if buffer[(x, y)].symbol() != "#" {
                x += 1;
                continue;
            }
            let mut end = x + 1;
            let mut digits = String::new();
            while end < area.right() {
                let symbol = buffer[(end, y)].symbol();
                match symbol.chars().next() {
                    Some(character) if character.is_ascii_digit() => digits.push(character),
                    _ => break,
                }
                end += 1;
            }
            if digits.is_empty() {
                x += 1;
                continue;
            }
            // Mirror the scanner: a hash glued to a word is part of that word,
            // not a reference.
            let glued = x > area.left()
                && buffer[(x - 1, y)]
                    .symbol()
                    .chars()
                    .next()
                    .is_some_and(|character| character.is_ascii_alphanumeric());
            if glued {
                x = end;
                continue;
            }
            if let Some(status) = digits.parse::<u64>().ok().and_then(lookup) {
                buffer[(x, y)].set_style(reference_marker_style(theme, status));
                for cell in (x + 1)..end {
                    buffer[(cell, y)].set_style(reference_style(theme, status));
                }
            }
            x = end;
        }
    }
}

/// Colour of the leading `#`, which carries the kind.
///
/// State alone cannot say this: GitHub shows an open issue and an open pull
/// request in the same green, so without a second channel the two are
/// indistinguishable. Tinting the hash leaves the layout untouched, which
/// swapping in an icon glyph would not — a wide character would shift the rest
/// of the child's line.
fn reference_marker_style(theme: Theme, status: crate::github::ReferenceStatus) -> Style {
    use crate::github::ReferenceKind;
    let color = match status.kind {
        ReferenceKind::Issue => theme.warning,
        ReferenceKind::PullRequest => theme.accent_alt,
        ReferenceKind::Discussion => theme.info,
        ReferenceKind::Ambiguous => theme.warning,
    };
    Style::default()
        .fg(color)
        .add_modifier(Modifier::UNDERLINED | Modifier::BOLD)
}

/// Colour of the number, which carries the state; the underline says "this is a link".
fn reference_style(theme: Theme, status: crate::github::ReferenceStatus) -> Style {
    use crate::github::{ReferenceKind, ReferenceState};
    let color = match status.state {
        ReferenceState::Open => theme.success,
        ReferenceState::Closed => match status.kind {
            // A closed issue and a closed pull request mean different things,
            // and GitHub itself colours them differently.
            ReferenceKind::Issue => theme.accent,
            ReferenceKind::PullRequest => theme.error,
            ReferenceKind::Discussion => theme.accent,
            ReferenceKind::Ambiguous => theme.warning,
        },
        ReferenceState::Merged => theme.accent,
        ReferenceState::Draft if theme.name == ThemeName::Classic => Color::Gray,
        ReferenceState::Draft => theme.muted,
    };
    Style::default()
        .fg(color)
        .add_modifier(Modifier::UNDERLINED | Modifier::BOLD)
}

pub fn draw_status(f: &mut Frame, app: &App, area: Rect) {
    let theme = app.theme();
    let Some(mux) = app.mux.as_ref() else {
        return;
    };
    let Some(pane) = mux.focused_pane() else {
        return;
    };
    let prefix = mux.prefix.label();

    // The hint is fixed-width and reserved first; tabs take whatever is left, so a long
    // session name can never push the prefix reminder off screen.
    let hint: Vec<Span> = match pane.status {
        // A transient mode stays open across keystrokes, so unlike the one-shot menus
        // its reminder is the only thing telling the user why arrows have stopped
        // reaching Copilot.
        PaneStatus::Running if matches!(mux.prefix_state, PrefixState::Transient(_)) => {
            let PrefixState::Transient(mode) = mux.prefix_state else {
                unreachable!("guarded by the arm above")
            };
            vec![
                Span::styled(mode.badge(), badge_style(theme, theme.accent_alt)),
                Span::raw(mode.hint()),
            ]
        }
        PaneStatus::Running if mux.prefix_state == PrefixState::Help => vec![
            Span::styled(" Help ", badge_style(theme, theme.warning)),
            Span::raw(" e scratchpad  Esc cancel "),
        ],
        PaneStatus::Running if mux.prefix_state == PrefixState::Github => vec![
            Span::styled(" GitHub ", badge_style(theme, theme.accent)),
            Span::raw(" i inspect  Esc cancel "),
        ],
        PaneStatus::Running if mux.prefix_state == PrefixState::Root => vec![
            Span::styled(format!(" {prefix} "), badge_style(theme, theme.warning)),
            Span::raw(format!(" choose a command · {prefix} search · Esc close ")),
        ],
        PaneStatus::Running
            if mux.prefix_state == PrefixState::Idle && app.update_notice.is_some() =>
        {
            vec![
                Span::styled(" Update ", badge_style(theme, theme.accent_alt)),
                Span::raw(format!(
                    " {} ",
                    text::truncate_to_width(
                        app.update_notice.as_deref().unwrap_or_default(),
                        area.width.saturating_sub(11) as usize,
                    )
                )),
            ]
        }
        PaneStatus::Running => vec![
            Span::raw(" "),
            Span::styled(prefix.clone(), Style::default().fg(theme.accent_alt)),
            Span::raw(" for commands "),
        ],
        PaneStatus::Exited(code) => {
            let text = match code {
                Some(0) | None => "exited".to_string(),
                Some(code) => format!("exited with code {code}"),
            };
            vec![Span::styled(
                format!(" {text} — r restart · Enter close "),
                Style::default().fg(theme.warning),
            )]
        }
    };
    fill_area(f.buffer_mut(), area, theme.chrome_bg);
    let status = Paragraph::new(Line::from(hint)).style(status_style(theme));
    f.render_widget(status, area);
}

/// Two-column activity cell shown at the head of every tab.
///
/// The width is fixed whatever the state, so a tab's text never shifts sideways when a
/// turn starts or finishes — and click hit-testing stays valid between frames even
/// though the spinner glyph changes underneath it.
fn tab_marker(pane: &crate::mux::Pane) -> String {
    use crate::host_terminal::ProgressState;
    if !pane.is_running() {
        return "× ".to_string();
    }
    // A waiting question outranks progress: the same rule the outer terminal follows.
    if pane.requires_user_action() {
        return "? ".to_string();
    }
    // A turn that finished while the user was elsewhere. This keeps the precedence the
    // combined attention flag always had, and only splits the glyph: a question is
    // blocked on the user, whereas this is a result they have not read yet.
    if pane.is_unread() {
        return "● ".to_string();
    }
    // Something typed here and never sent. Ranked below anything the session wants from
    // the user, because a draft is not waiting on them — it is waiting on them to come
    // back, which is a quieter thing.
    if pane.has_draft() {
        return "✎ ".to_string();
    }
    match pane.effective_progress_state() {
        ProgressState::Normal | ProgressState::Indeterminate => {
            format!("{} ", crate::ui::spinner_frame())
        }
        ProgressState::Error => "! ".to_string(),
        ProgressState::Warning => "▲ ".to_string(),
        ProgressState::Clear => "  ".to_string(),
    }
}

/// Tab titles exactly as the bar draws them.
///
/// Shared with click hit-testing so the two can never disagree about where a tab starts
/// and ends.
pub fn tab_sources(mux: &crate::mux::MuxState) -> Vec<tabs::TabSource> {
    mux.panes
        .iter()
        .map(|pane| tabs::TabSource {
            marker: tab_marker(pane),
            title: pane.title.clone(),
            running: pane.is_running(),
        })
        .collect()
}

/// The pane whose tab covers `column`, for click-to-switch.
pub fn tab_at(
    mux: &crate::mux::MuxState,
    area: Rect,
    column: u16,
    row: u16,
) -> Option<crate::mux::PaneId> {
    tab_index_at(mux, area, column, row).map(|index| mux.panes[index].id)
}

/// Position in `mux.panes` of the tab covering `column`, for a drag that has to know
/// where it is going rather than only which pane it is over.
///
/// Strict about the strip's empty tail: a click out there is not a click on the last
/// tab. A drag that wants to treat an overshoot as "park it at the end" applies that
/// itself, because click-to-focus must not.
pub fn tab_index_at(
    mux: &crate::mux::MuxState,
    area: Rect,
    column: u16,
    row: u16,
) -> Option<usize> {
    if area.height == 0 || row < area.y || row >= area.bottom() {
        return None;
    }
    let (start, widths) = strip(mux, area);
    let mut x = area.x;
    for (offset, width) in widths.iter().enumerate() {
        if column >= x && column < x + width {
            let index = start + offset;
            return (index < mux.panes.len()).then_some(index);
        }
        x += width;
    }
    None
}

/// Indices of the first and last tabs currently drawn, or `None` when none are.
///
/// A drag needs this to tell "the pointer is over the tab it is already dragging"
/// apart from "the pointer has run out of strip", which are the same column but
/// mean opposite things.
pub fn visible_tab_bounds(mux: &crate::mux::MuxState, area: Rect) -> Option<(usize, usize)> {
    if area.height == 0 {
        return None;
    }
    let (start, widths) = strip(mux, area);
    let last = start + widths.len().checked_sub(1)?;
    Some((start, last.min(mux.panes.len().saturating_sub(1))))
}

/// The rendered strip as `(index of the first tab, width of each tab)`.
///
/// The strip is windowed around the focused tab when it overflows, so every caller
/// mapping a column back to a pane has to apply that same offset. Sharing one
/// computation keeps hit-testing from drifting away from what was drawn.
fn strip(mux: &crate::mux::MuxState, area: Rect) -> (usize, Vec<u16>) {
    let focused_index = mux
        .focused_pane()
        .and_then(|pane| mux.panes.iter().position(|other| other.id == pane.id))
        .unwrap_or(0);
    let sessions = tab_sources(mux);
    let (tab_list, _) = tabs::layout(&sessions, focused_index, area.width as usize);
    let start = tabs::window_start_for(sessions.len(), tab_list.len(), focused_index);
    let widths = tab_list
        .iter()
        .map(|tab| text::display_width(&tab.label) as u16)
        .collect();
    (start, widths)
}

/// Draw the browser-style tab bar: a row of labels over a rule that runs heavy beneath
/// the focused tab and light beneath the rest.
///
/// The two rows are laid out from the same widths, so the underline can never drift out
/// of alignment with the label above it.
pub fn draw_tabs(f: &mut Frame, app: &App, area: Rect) {
    // Collapsed to nothing for a lone session. Without this the rows below would be
    // forced to height 1 and paint over the chat's first line.
    if area.height == 0 || area.width == 0 {
        return;
    }
    let theme = app.theme();
    fill_area(f.buffer_mut(), area, theme.chrome_bg);
    let Some(mux) = app.mux.as_ref() else {
        return;
    };
    let Some(pane) = mux.focused_pane() else {
        return;
    };

    let focused_index = mux
        .panes
        .iter()
        .position(|candidate| candidate.id == pane.id)
        .unwrap_or(0);
    let sessions = tab_sources(mux);
    let (tab_list, hidden) = tabs::layout(&sessions, focused_index, area.width as usize);
    let start = tabs::window_start_for(sessions.len(), tab_list.len(), focused_index);

    let mut labels: Vec<Span> = Vec::new();
    // Widths are collected alongside the labels so the rule below is built from the same
    // arithmetic rather than re-measuring the rendered text.
    let mut rule: Vec<Span> = Vec::new();
    for (offset, tab) in tab_list.iter().enumerate() {
        let width = text::display_width(&tab.label);
        // A drag reorders as the pointer moves, so without this the strip would
        // rearrange itself under a pointer with nothing to show it is the cause.
        let held = app.dragging_tab.is_some()
            && mux.panes.get(start + offset).map(|pane| pane.id) == app.dragging_tab;
        let (label_style, rule_style, glyph) = if held {
            (
                // Filled rather than merely recoloured: the tab should read as picked
                // up off the strip, not as a third kind of session state.
                Style::default()
                    .fg(theme.contrast_text(theme.selection_bg))
                    .bg(theme.selection_bg)
                    .add_modifier(Modifier::BOLD),
                Style::default().fg(theme.accent_alt),
                // Broken rather than solid: this tab has not landed anywhere yet.
                "╍",
            )
        } else if tab.active {
            (
                Style::default()
                    .fg(theme.accent_alt)
                    .add_modifier(Modifier::BOLD),
                Style::default().fg(theme.accent_alt),
                "━",
            )
        } else if tab.running {
            (
                Style::default().fg(theme.ansi[7]),
                Style::default().fg(theme.muted),
                "─",
            )
        } else {
            (
                Style::default().fg(theme.error),
                Style::default().fg(theme.muted),
                "─",
            )
        };
        labels.push(Span::styled(tab.label.clone(), label_style));
        rule.push(Span::styled(glyph.repeat(width), rule_style));
    }

    let mut used: usize = tab_list
        .iter()
        .map(|tab| text::display_width(&tab.label))
        .sum();
    if hidden > 0 {
        let marker = format!(" +{hidden} ");
        let width = text::display_width(&marker);
        used += width;
        labels.push(Span::styled(marker, Style::default().fg(theme.muted)));
        rule.push(Span::styled(
            "─".repeat(width),
            Style::default().fg(theme.muted),
        ));
    }
    // Carry the rule to the edge so the tab bar reads as one continuous baseline.
    if let Some(remainder) = (area.width as usize).checked_sub(used) {
        rule.push(Span::styled(
            "─".repeat(remainder),
            Style::default().fg(theme.muted),
        ));
    }

    // Anchored to the bottom of the strip: the rule closes it off against the chat, the
    // labels sit directly above it, and whatever is left over becomes breathing room at
    // the top so the tabs are not flush against the edge of the window.
    let style = status_style(theme);
    f.render_widget(
        Paragraph::new(Line::from(labels)).style(style),
        Rect {
            y: area.y + area.height.saturating_sub(2),
            height: 1,
            ..area
        },
    );
    if area.height > 1 {
        f.render_widget(
            Paragraph::new(Line::from(rule)).style(style),
            Rect {
                y: area.y + area.height - 1,
                height: 1,
                ..area
            },
        );
    }
}

fn panel_style(theme: Theme) -> Style {
    if theme.name == ThemeName::Classic {
        Style::default()
    } else {
        Style::default().fg(theme.text).bg(theme.background)
    }
}

fn status_style(theme: Theme) -> Style {
    let style = Style::default().bg(theme.chrome_bg);
    if theme.name == ThemeName::Classic {
        style
    } else {
        style.fg(theme.text)
    }
}

fn badge_style(theme: Theme, background: Color) -> Style {
    let foreground = if theme.name == ThemeName::Classic {
        Color::Black
    } else {
        theme.contrast_text(background)
    };
    Style::default()
        .fg(foreground)
        .bg(background)
        .add_modifier(Modifier::BOLD)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::UserConfig;
    use crate::github::{ReferenceKind, ReferenceState, ReferenceStatus};
    use crate::mux::{Pane, PaneSpec};
    use ratatui::backend::TestBackend;
    use ratatui::buffer::Buffer;
    use ratatui::Terminal;
    use std::sync::mpsc;

    #[test]
    fn known_references_are_styled_and_others_left_alone() {
        let area = Rect::new(0, 0, 40, 2);
        let mut buffer = Buffer::empty(area);
        for (index, character) in "Fixed #11 in #12, room 314, v#9".chars().enumerate() {
            buffer[(index as u16, 0)].set_symbol(&character.to_string());
        }

        restyle_references(
            &mut buffer,
            area,
            ThemeName::Classic.theme(),
            &|number| match number {
                11 => Some(ReferenceStatus {
                    kind: ReferenceKind::Issue,
                    state: ReferenceState::Open,
                }),
                12 => Some(ReferenceStatus {
                    kind: ReferenceKind::PullRequest,
                    state: ReferenceState::Merged,
                }),
                // Resolvable on its own, but here it only appears glued to `v#`.
                9 => Some(ReferenceStatus {
                    kind: ReferenceKind::Issue,
                    state: ReferenceState::Open,
                }),
                _ => None,
            },
        );

        // The number carries the state...
        for x in 7..9 {
            assert_eq!(buffer[(x, 0)].style().fg, Some(Color::Green), "cell {x}");
        }
        for x in 14..16 {
            assert_eq!(buffer[(x, 0)].style().fg, Some(Color::Magenta), "cell {x}");
        }
        // ...and the hash carries the kind, which the state alone cannot: both of
        // these would be green if they were open.
        assert_eq!(buffer[(6, 0)].style().fg, Some(Color::Yellow), "issue hash");
        assert_eq!(buffer[(13, 0)].style().fg, Some(Color::Cyan), "pull hash");
        assert!(buffer[(6, 0)]
            .style()
            .add_modifier
            .contains(Modifier::UNDERLINED));
        // A number nobody could resolve, and a hash glued to a word, stay untouched.
        assert_eq!(buffer[(23, 0)].style().fg, Some(Color::Reset));
        assert_eq!(buffer[(30, 0)].style().fg, Some(Color::Reset));
    }

    #[test]
    fn an_open_issue_and_an_open_pull_request_are_distinguishable() {
        let area = Rect::new(0, 0, 20, 1);
        let mut buffer = Buffer::empty(area);
        for (index, character) in "#11 #12".chars().enumerate() {
            buffer[(index as u16, 0)].set_symbol(&character.to_string());
        }

        restyle_references(&mut buffer, area, ThemeName::Classic.theme(), &|number| {
            Some(ReferenceStatus {
                kind: if number == 11 {
                    ReferenceKind::Issue
                } else {
                    ReferenceKind::PullRequest
                },
                state: ReferenceState::Open,
            })
        });

        // Both are open, so both numbers are green; only the hash tells them apart.
        assert_eq!(buffer[(1, 0)].style().fg, Some(Color::Green));
        assert_eq!(buffer[(5, 0)].style().fg, Some(Color::Green));
        assert_ne!(buffer[(0, 0)].style().fg, buffer[(4, 0)].style().fg);
    }

    /// Render the whole UI into an off-screen buffer and return it as plain text.
    fn render(app: &mut App) -> String {
        render_buffer(app, 80, 24)
            .content()
            .iter()
            .map(|cell| cell.symbol())
            .collect::<String>()
    }

    fn render_buffer(app: &mut App, width: u16, height: u16) -> Buffer {
        let backend = TestBackend::new(width, height);
        let mut terminal = Terminal::new(backend).expect("test terminal");
        terminal
            .draw(|f| crate::ui::draw(f, app))
            .expect("draw succeeds");
        terminal.backend().buffer().clone()
    }

    /// A child that stays alive without printing anything, standing in for Copilot's
    /// several-second boot.
    fn silent_pane(events: mpsc::Sender<crate::mux::MuxEvent>) -> Pane {
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
        Pane::spawn(
            PaneSpec {
                id: 1,
                title: "booting".to_string(),
                cwd: std::env::temp_dir(),
                session_id: "booting-session".to_string(),
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
        .expect("pane spawns")
    }

    fn mux_app() -> App {
        let config = UserConfig {
            mux: true,
            ..UserConfig::default()
        };
        App::new(Vec::new(), config)
    }

    fn mux_app_with_theme(theme: ThemeName) -> App {
        let config = UserConfig {
            mux: true,
            theme,
            ..UserConfig::default()
        };
        App::new(Vec::new(), config)
    }

    fn rgb_components(color: Color) -> (u8, u8, u8) {
        match color {
            Color::Rgb(r, g, b) => (r, g, b),
            other => panic!("expected RGB color, got {other:?}"),
        }
    }

    fn contrast_ratio(foreground: Color, background: Color) -> f64 {
        let luminance = |color| {
            let (r, g, b) = rgb_components(color);
            let channel = |value: u8| {
                let value = f64::from(value) / 255.0;
                if value <= 0.04045 {
                    value / 12.92
                } else {
                    ((value + 0.055) / 1.055).powf(2.4)
                }
            };
            0.2126 * channel(r) + 0.7152 * channel(g) + 0.0722 * channel(b)
        };
        let foreground = luminance(foreground);
        let background = luminance(background);
        (foreground.max(background) + 0.05) / (foreground.min(background) + 0.05)
    }

    #[test]
    fn light_theme_styles_attached_chrome_and_startup_spinner() {
        let mut app = mux_app_with_theme(ThemeName::SolarizedLight);
        let theme = app.theme();
        let events = app.mux.as_ref().expect("mux").events.clone();
        let pane = silent_pane(events);
        let id = pane.id;
        app.mux.as_mut().expect("mux").push(pane);
        app.view = crate::app::View::Attached(id);

        let buffer = render_buffer(&mut app, 80, 24);

        // A lone session shows no tab bar, so the chat still starts at the top row.
        assert_eq!(buffer[(0, 0)].fg, theme.accent_alt);
        assert_eq!(buffer[(0, 0)].bg, theme.background);
        assert_eq!(buffer[(6, 2)].symbol(), "S");
        assert_eq!(buffer[(6, 2)].fg, theme.accent_alt);
        assert_eq!(buffer[(6, 2)].bg, theme.background);
        let _ = app.mux.as_mut().expect("mux").shutdown();
    }

    #[test]
    fn light_themes_keep_copilot_edit_output_readable_and_terminal_faithful() {
        let transcript = concat!(
            "ordinary \x1b[1;3mstyled\x1b[0m \x1b[38;5;42mhigh\x1b[0m ",
            "\x1b[38;2;1;2;3mtrue\x1b[0m\r\n",
            "\x1b[1;36mEdit\x1b[0m src/lib.rs\r\n",
            "\x1b[90m 12 │ \x1b[35mfn\x1b[39m demo() {\x1b[0m\r\n",
            "\x1b[48;2;18;52;31m\x1b[32m+\x1b[39m \x1b[90m13 │ ",
            "\x1b[39m\x1b[36mlet\x1b[39m added = \x1b[33mtrue\x1b[39m;",
            "\x1b[0m\r\n",
            "\x1b[48;2;62;25;31m\x1b[31m-\x1b[39m \x1b[90m13 │ ",
            "\x1b[39m\x1b[36mlet\x1b[39m deleted = \x1b[33mfalse\x1b[39m;",
            "\x1b[0m"
        );

        for name in ThemeName::LIGHT {
            let mut app = mux_app_with_theme(name);
            let theme = app.theme();
            let events = app.mux.as_ref().expect("mux").events.clone();
            let mut pane = silent_pane(events);
            let id = pane.id;
            pane.feed_synthetic(transcript.as_bytes());
            app.mux.as_mut().expect("mux").push(pane);
            app.view = crate::app::View::Attached(id);

            let buffer = render_buffer(&mut app, 80, 24);
            let x = 1;
            // One pane means no tab bar, so this is just past the chat's own border.
            let y = 1;
            let add_bg = Color::Rgb(18, 52, 31);
            let delete_bg = Color::Rgb(62, 25, 31);

            assert_eq!(buffer[(x, y)].fg, theme.text, "{}", name.label());
            assert_eq!(buffer[(x, y)].bg, theme.background, "{}", name.label());
            assert!(
                buffer[(x + 9, y)].modifier.contains(Modifier::BOLD),
                "{}",
                name.label()
            );
            assert!(
                buffer[(x + 9, y)].modifier.contains(Modifier::ITALIC),
                "{}",
                name.label()
            );
            assert_eq!(
                buffer[(x + 16, y)].fg,
                Color::Indexed(42),
                "{}",
                name.label()
            );
            assert_eq!(
                buffer[(x + 21, y)].fg,
                Color::Rgb(1, 2, 3),
                "{}",
                name.label()
            );

            assert_eq!(buffer[(x + 1, y + 2)].fg, theme.ansi[8]);
            assert_eq!(buffer[(x + 6, y + 2)].fg, theme.ansi[5]);
            assert!(contrast_ratio(theme.ansi[8], theme.background) >= 2.4);

            assert_eq!(buffer[(x, y + 3)].fg, theme.ansi[2]);
            assert_eq!(buffer[(x, y + 3)].bg, add_bg);
            assert_eq!(buffer[(x + 7, y + 3)].fg, theme.ansi[6]);
            assert_eq!(buffer[(x + 11, y + 3)].fg, theme.contrast_text(add_bg));
            assert_eq!(buffer[(x + 11, y + 3)].bg, add_bg);
            assert!(contrast_ratio(theme.ansi[2], add_bg) >= 2.8);
            assert!(contrast_ratio(theme.ansi[6], add_bg) >= 2.8);

            assert_eq!(buffer[(x, y + 4)].fg, theme.ansi[1]);
            assert_eq!(buffer[(x, y + 4)].bg, delete_bg);
            assert_eq!(buffer[(x + 7, y + 4)].fg, theme.ansi[6]);
            assert_eq!(buffer[(x + 11, y + 4)].fg, theme.contrast_text(delete_bg));
            assert_eq!(buffer[(x + 11, y + 4)].bg, delete_bg);
            assert!(contrast_ratio(theme.ansi[1], delete_bg) >= 2.8);
            assert!(contrast_ratio(theme.ansi[6], delete_bg) >= 2.8);

            let _ = app.mux.as_mut().expect("mux").shutdown();
        }
    }

    #[test]
    fn themed_references_keep_terminal_backgrounds_and_modifiers() {
        let area = Rect::new(0, 0, 4, 1);
        let mut buffer = Buffer::empty(area);
        let theme = ThemeName::CatppuccinLatte.theme();
        for (index, character) in "#123".chars().enumerate() {
            buffer[(index as u16, 0)]
                .set_symbol(&character.to_string())
                .set_style(
                    Style::default()
                        .fg(theme.text)
                        .bg(theme.diff_add_bg)
                        .add_modifier(Modifier::ITALIC),
                );
        }

        restyle_references(&mut buffer, area, theme, &|_| {
            Some(ReferenceStatus {
                kind: ReferenceKind::Issue,
                state: ReferenceState::Open,
            })
        });

        assert_eq!(buffer[(0, 0)].fg, theme.warning);
        assert_eq!(buffer[(1, 0)].fg, theme.success);
        for x in 0..4 {
            assert_eq!(buffer[(x, 0)].bg, theme.diff_add_bg);
            assert!(buffer[(x, 0)].modifier.contains(Modifier::ITALIC));
            assert!(buffer[(x, 0)].modifier.contains(Modifier::UNDERLINED));
            assert!(buffer[(x, 0)].modifier.contains(Modifier::BOLD));
        }
    }

    #[test]
    fn a_session_that_has_drawn_nothing_shows_the_startup_spinner() {
        let mut app = mux_app();
        let events = app.mux.as_ref().expect("mux").events.clone();
        let pane = silent_pane(events);
        let id = pane.id;
        app.mux.as_mut().expect("mux").push(pane);
        app.view = crate::app::View::Attached(id);

        let text = render(&mut app);

        assert!(
            text.contains("Starting Copilot"),
            "a blank pane must show the spinner, got:\n{text}"
        );
        let _ = app.mux.as_mut().expect("mux").shutdown();
    }

    #[test]
    fn the_spinner_clears_once_the_session_paints_something() {
        let mut app = mux_app();
        let events = app.mux.as_ref().expect("mux").events.clone();
        let pane = silent_pane(events);
        let id = pane.id;
        app.mux.as_mut().expect("mux").push(pane);
        app.view = crate::app::View::Attached(id);

        // Stand in for Copilot's first frame arriving.
        app.mux
            .as_mut()
            .expect("mux")
            .focused_pane_mut()
            .expect("pane")
            .feed_synthetic(b"hello from copilot");

        let text = render(&mut app);

        assert!(
            !text.contains("Starting Copilot"),
            "the spinner must disappear as soon as the child draws, got:\n{text}"
        );
        assert!(text.contains("hello from copilot"));
        let _ = app.mux.as_mut().expect("mux").shutdown();
    }

    /// Where the chat paints its cursor. Over text it reverses the cell; on the empty
    /// cell past the end of a line it draws a block, which is the case that matters
    /// here because that is where a composer's cursor sits.
    fn painted_cursors(app: &mut App) -> Vec<(u16, u16)> {
        let buffer = render_buffer(app, 80, 24);
        let width = buffer.area.width;
        buffer
            .content()
            .iter()
            .enumerate()
            .filter(|(_, cell)| {
                cell.modifier.contains(Modifier::REVERSED) || cell.symbol() == "\u{2588}"
            })
            .map(|(index, _)| (index as u16 % width, index as u16 / width))
            .collect()
    }

    #[test]
    fn a_tab_that_finished_unwatched_is_marked_unread_not_as_a_question() {
        let (tx, _) = mpsc::channel();
        let mut pane = silent_pane(tx);

        pane.feed_synthetic(b"\x1b]9;4;3;0\x1b\\");
        pane.refresh_from_callbacks(false);
        assert_eq!(
            tab_marker(&pane),
            format!("{} ", crate::ui::spinner_frame()),
            "a working tab keeps its spinner"
        );

        pane.feed_synthetic(b"\x1b]9;4;0;0\x1b\\");
        pane.refresh_from_callbacks(false);
        assert_eq!(tab_marker(&pane), "● ", "finished while looking elsewhere");

        pane.acknowledge_attention();
        assert_eq!(tab_marker(&pane), "  ", "reading it clears the mark");

        // The question glyph stays reserved for a session that is actually blocked.
        pane.apply_lifecycle(crate::events::lifecycle::LifecycleEvent::InputRequested {
            tool_call_id: "question-1".into(),
            kind: crate::events::lifecycle::InputKind::Question,
        });
        assert_eq!(tab_marker(&pane), "? ");

        // Every state keeps the cell the same width, so titles never shift sideways.
        assert_eq!(crate::text::display_width(&tab_marker(&pane)), 2);
        let _ = pane.shutdown();
    }

    #[test]
    fn an_unsent_draft_is_marked_without_shifting_the_tab_title() {
        // A marker wider than two columns pushes every title along and invalidates click
        // hit-testing, which is why the cell is fixed. A new glyph is the way that gets
        // broken, so it is measured rather than assumed.
        let (tx, _) = mpsc::channel();
        let mut pane = silent_pane(tx);

        pane.note_user_key(&crossterm::event::KeyEvent::new(
            crossterm::event::KeyCode::Char('h'),
            crossterm::event::KeyModifiers::NONE,
        ));

        assert_eq!(tab_marker(&pane), "✎ ", "something typed and not sent");
        assert_eq!(
            crate::text::display_width(&tab_marker(&pane)),
            2,
            "the draft glyph must occupy the same cell as every other marker"
        );

        // And it yields to anything the session actually wants from the user: a draft is
        // waiting on them to come back, not asking them for something.
        pane.apply_lifecycle(crate::events::lifecycle::LifecycleEvent::InputRequested {
            tool_call_id: "question-1".into(),
            kind: crate::events::lifecycle::InputKind::Question,
        });
        assert_eq!(tab_marker(&pane), "? ");
        let _ = pane.shutdown();
    }

    #[test]
    fn the_chat_paints_its_own_cursor_only_while_it_has_focus() {
        let mut app = mux_app();
        let events = app.mux.as_ref().expect("mux").events.clone();
        let pane = silent_pane(events);
        let id = pane.id;
        app.mux.as_mut().expect("mux").push(pane);
        app.view = crate::app::View::Attached(id);
        app.mux
            .as_mut()
            .expect("mux")
            .focused_pane_mut()
            .expect("pane")
            .feed_synthetic(b"hi");

        app.workspace_focus = crate::app::WorkspaceFocus::Chat;
        let focused = painted_cursors(&mut app);
        app.workspace_focus = crate::app::WorkspaceFocus::Scratchpad;
        let unfocused = painted_cursors(&mut app);

        // Exactly one cell carries the cursor, and it sits just past the text the child
        // drew rather than anywhere the terminal's own cursor happened to be left.
        assert_eq!(focused.len(), 1, "expected one painted cursor: {focused:?}");
        let (x, y) = focused[0];
        let text_start = x - 2;
        assert_eq!(
            render_buffer(&mut app, 80, 24)[(text_start, y)].symbol(),
            "h",
            "cursor should follow \"hi\""
        );
        // A pane that paints a cursor without input focus reads as if it has it.
        assert!(
            unfocused.is_empty(),
            "unfocused chat still painted a cursor: {unfocused:?}"
        );
        let _ = app.mux.as_mut().expect("mux").shutdown();
    }

    #[test]
    fn attached_status_shows_update_restart_progress() {
        let mut app = mux_app();
        let events = app.mux.as_ref().expect("mux").events.clone();
        let pane = silent_pane(events);
        let id = pane.id;
        app.mux.as_mut().expect("mux").push(pane);
        app.view = crate::app::View::Attached(id);
        app.update_notice =
            Some("Installing v0.19.0; 1 running session will reopen after restart...".to_string());

        let text = render(&mut app);

        assert!(text.contains("Update"), "got:\n{text}");
        assert!(text.contains("Installing v0.19.0"), "got:\n{text}");
        assert!(text.contains("reopen after resta"), "got:\n{text}");
        let _ = app.mux.as_mut().expect("mux").shutdown();
    }

    #[test]
    fn update_restart_prompt_is_visible_without_leaving_the_attached_pane() {
        let mut app = mux_app();
        let events = app.mux.as_ref().expect("mux").events.clone();
        let pane = silent_pane(events);
        let id = pane.id;
        app.mux.as_mut().expect("mux").push(pane);
        app.view = crate::app::View::Attached(id);
        app.confirm_update_restart = true;

        let text = render(&mut app);

        assert!(text.contains("Update & Restart"), "got:\n{text}");
        assert!(
            text.contains("Update CST and restart 1 running session?"),
            "got:\n{text}"
        );
        assert!(text.contains("Copilot chats reopen"), "got:\n{text}");
        let _ = app.mux.as_mut().expect("mux").shutdown();
    }

    #[test]
    fn the_quit_prompt_is_visible_without_leaving_the_attached_pane() {
        let mut app = mux_app();
        let events = app.mux.as_ref().expect("mux").events.clone();
        let pane = silent_pane(events);
        let id = pane.id;
        app.mux.as_mut().expect("mux").push(pane);
        app.view = crate::app::View::Attached(id);
        app.confirm_quit = true;

        let text = render(&mut app);

        assert!(
            text.contains("Quit with 1 running session(s)?"),
            "prefix q must be answerable from the pane it was pressed in, got:\n{text}"
        );
        // No tmux-backed pane is open, so the warning must not talk about tmux.
        assert!(
            text.contains("Sessions do not survive CST exiting."),
            "got:\n{text}"
        );
        let _ = app.mux.as_mut().expect("mux").shutdown();
    }

    #[test]
    fn attached_workspace_renders_chat_scratchpad_and_terminal_together() {
        let mut app = mux_app();
        let events = app.mux.as_ref().expect("mux").events.clone();
        let pane = silent_pane(events);
        let id = pane.id;
        app.mux.as_mut().expect("mux").push(pane);
        app.view = crate::app::View::Attached(id);
        app.scratchpad =
            Some(crate::scratchpad::Scratchpad::open("workspace-render-test").unwrap());
        app.scratchpad
            .as_mut()
            .unwrap()
            .handle_event(crossterm::event::Event::Key(
                crossterm::event::KeyEvent::new(
                    crossterm::event::KeyCode::Char('x'),
                    crossterm::event::KeyModifiers::NONE,
                ),
            ))
            .unwrap();
        app.scratchpad_owner = Some(id);
        app.scratchpad_open.insert(id);
        let directory = tempfile::tempdir().unwrap();
        app.terminal
            .activate(
                "workspace-render-test".to_string(),
                "Shell".to_string(),
                directory.path().to_string_lossy().to_string(),
                &crate::config::TerminalConfig::default(),
            )
            .unwrap();
        app.terminal_owner = Some(id);
        app.terminal_open.insert(id);
        app.workspace_focus = crate::app::WorkspaceFocus::Terminal;

        let text = render(&mut app);

        assert!(text.contains("Scratchpad"), "got:\n{text}");
        assert!(!text.contains("[modified]"), "got:\n{text}");
        assert!(text.contains("Terminal"), "got:\n{text}");
        assert!(!text.contains("Terminal ["), "got:\n{text}");
        assert!(text.contains("Starting Copilot"), "got:\n{text}");
        assert!(!text.contains("Alt+H"), "got:\n{text}");
        assert!(!text.contains("Save/close"), "got:\n{text}");
        assert!(!text.contains("focused"), "got:\n{text}");

        app.workspace_help = Some(crate::app::WorkspaceHelp::Scratchpad);
        let text = render(&mut app);
        assert!(text.contains("Scratchpad Help"), "got:\n{text}");
        assert!(text.contains("Ctrl/Alt+L"), "got:\n{text}");
        assert!(text.contains("Shift+Tab"), "got:\n{text}");
        assert!(text.contains("Ctrl+Shift+K"), "got:\n{text}");

        let _ = app.terminal.shutdown();
        let _ = app.mux.as_mut().expect("mux").shutdown();
    }

    fn row(buffer: &ratatui::buffer::Buffer, y: u16, width: u16) -> String {
        (0..width)
            .map(|x| buffer[(x, y)].symbol().to_string())
            .collect()
    }

    /// The strip is anchored to its bottom edge, leaving a blank padding row above the
    /// labels so the tabs are not flush against the top of the window.
    #[test]
    fn the_tab_strip_leaves_a_blank_padding_row_above_the_labels() {
        let mut app = mux_app_with_theme(ThemeName::Classic);
        let events = app.mux.as_ref().expect("mux").events.clone();
        for (id, title) in [(1u64, "alpha"), (2, "beta")] {
            app.mux
                .as_mut()
                .expect("mux")
                .push(named_pane(events.clone(), id, title));
        }
        app.view = crate::app::View::Attached(1);

        let buffer = render_buffer(&mut app, 80, 24);

        for y in 0..crate::ui::TAB_BAR_HEIGHT - 2 {
            let padding = row(&buffer, y, 80);
            assert!(
                padding.trim().is_empty(),
                "row {y} must be blank padding, got {padding:?}"
            );
        }
        // The labels still land immediately above the rule.
        assert!(row(&buffer, crate::ui::TAB_BAR_HEIGHT - 2, 80).contains("alpha"));
        let _ = app.mux.as_mut().expect("mux").shutdown();
    }

    #[test]
    fn the_rule_underlines_exactly_the_focused_tab() {
        let mut app = mux_app_with_theme(ThemeName::Classic);
        let events = app.mux.as_ref().expect("mux").events.clone();
        for (id, title) in [(1u64, "cst-work"), (2, "map-parse"), (3, "api-fix")] {
            let pane = named_pane(events.clone(), id, title);
            app.mux.as_mut().expect("mux").push(pane);
        }
        app.mux.as_mut().expect("mux").focused = Some(2);
        app.view = crate::app::View::Attached(2);

        let buffer = render_buffer(&mut app, 80, 24);
        let labels = row(&buffer, crate::ui::TAB_BAR_HEIGHT - 2, 80);
        let rule = row(&buffer, crate::ui::TAB_BAR_HEIGHT - 1, 80);

        // Both rows are compared in columns, not bytes: the rule glyphs are 3 bytes each
        // and would otherwise never line up with the ASCII labels above them.
        let rule_columns: Vec<char> = rule.chars().collect();
        let heavy_start = rule_columns
            .iter()
            .position(|glyph| *glyph == '━')
            .expect("focused tab is underlined");
        let heavy_end = rule_columns
            .iter()
            .rposition(|glyph| *glyph == '━')
            .expect("focused tab is underlined")
            + 1;
        let label_start = labels
            .find("2   map-parse")
            .map(|byte| labels[..byte].chars().count())
            .expect("focused label is drawn");

        assert!(
            heavy_start <= label_start && label_start < heavy_end,
            "the heavy rule must sit under the focused label\nlabels: {labels}\nrule:   {rule}"
        );
        assert!(
            rule_columns[heavy_start..heavy_end]
                .iter()
                .all(|glyph| *glyph == '━'),
            "the focused tab's underline must be one unbroken run\nrule: {rule}"
        );
        let _ = app.mux.as_mut().expect("mux").shutdown();
    }

    #[test]
    fn a_lone_session_spends_no_rows_on_a_tab_bar() {
        let mut app = mux_app_with_theme(ThemeName::Classic);
        let events = app.mux.as_ref().expect("mux").events.clone();
        app.mux
            .as_mut()
            .expect("mux")
            .push(named_pane(events.clone(), 1, "only-session"));
        app.view = crate::app::View::Attached(1);
        assert!(!app.tab_bar_visible());

        // A second session gives the strip something to switch between.
        app.mux
            .as_mut()
            .expect("mux")
            .push(named_pane(events, 2, "second-session"));
        assert!(app.tab_bar_visible());

        let _ = app.mux.as_mut().expect("mux").shutdown();
    }

    #[test]
    fn a_working_pane_shows_a_spinner_and_a_waiting_one_shows_the_question_marker() {
        use crate::host_terminal::ProgressState;

        let mut app = mux_app_with_theme(ThemeName::Classic);
        let events = app.mux.as_ref().expect("mux").events.clone();
        for (id, title) in [(1u64, "alpha"), (2, "beta")] {
            app.mux
                .as_mut()
                .expect("mux")
                .push(named_pane(events.clone(), id, title));
        }
        app.mux
            .as_mut()
            .expect("mux")
            .pane_mut(1)
            .expect("pane")
            .record_progress_state(ProgressState::Indeterminate);
        app.view = crate::app::View::Attached(1);

        let sources = tab_sources(app.mux.as_ref().expect("mux"));

        assert!(
            SPINNER.contains(&sources[0].marker.trim()),
            "a working pane must carry a spinner frame, got {:?}",
            sources[0].marker
        );
        assert_eq!(
            crate::text::display_width(&sources[0].marker),
            crate::text::display_width(&sources[1].marker),
            "every status cell must be the same width so labels never shift"
        );
        let _ = app.mux.as_mut().expect("mux").shutdown();
    }

    /// Replays the exact hook order Copilot CLI 1.0.83 writes to `.cst-lifecycle.jsonl`,
    /// with the real timestamps: `working` first, then `session_started` 4.3 seconds
    /// later, then `ready`.
    #[test]
    fn a_turn_stays_lit_through_the_session_started_copilot_sends_after_working() {
        use crate::events::hooks::HookLifecycleEvent;
        use crate::host_terminal::ProgressState;

        let mut app = mux_app_with_theme(ThemeName::Classic);
        let events = app.mux.as_ref().expect("mux").events.clone();
        for (id, title) in [(1u64, "alpha"), (2, "beta")] {
            app.mux
                .as_mut()
                .expect("mux")
                .push(named_pane(events.clone(), id, title));
        }
        let pane = app.mux.as_mut().expect("mux").pane_mut(1).expect("pane");

        pane.apply_hook(
            HookLifecycleEvent::Working {
                timestamp: 1788469434959,
            },
            false,
        );
        assert_eq!(
            pane.effective_progress_state(),
            ProgressState::Indeterminate
        );

        pane.apply_hook(
            HookLifecycleEvent::SessionStarted {
                timestamp: 1788469439241,
            },
            false,
        );
        assert_eq!(
            pane.effective_progress_state(),
            ProgressState::Indeterminate,
            "an in-flight turn must survive a late session_started"
        );

        let sources = tab_sources(app.mux.as_ref().expect("mux"));
        assert!(
            SPINNER.contains(&sources[0].marker.trim()),
            "expected a spinner mid-turn, got {:?}",
            sources[0].marker
        );

        let pane = app.mux.as_mut().expect("mux").pane_mut(1).expect("pane");
        pane.apply_hook(
            HookLifecycleEvent::Ready {
                timestamp: 1788469452830,
            },
            false,
        );
        assert_eq!(
            pane.effective_progress_state(),
            ProgressState::Clear,
            "and the turn still ends when the hook says it did"
        );
        let _ = app.mux.as_mut().expect("mux").shutdown();
    }

    /// A session_started with no turn under way must still yield authority, so a pane
    /// that has never reported anything does not claim to be busy.
    #[test]
    fn session_started_outside_a_turn_still_clears_the_hook_state() {
        use crate::events::hooks::HookLifecycleEvent;
        use crate::host_terminal::ProgressState;

        let mut app = mux_app_with_theme(ThemeName::Classic);
        let events = app.mux.as_ref().expect("mux").events.clone();
        app.mux
            .as_mut()
            .expect("mux")
            .push(named_pane(events, 1, "alpha"));
        let pane = app.mux.as_mut().expect("mux").pane_mut(1).expect("pane");

        pane.apply_hook(HookLifecycleEvent::Ready { timestamp: 10 }, false);
        pane.apply_hook(HookLifecycleEvent::SessionStarted { timestamp: 20 }, false);

        assert_eq!(pane.effective_progress_state(), ProgressState::Clear);
        let _ = app.mux.as_mut().expect("mux").shutdown();
    }

    /// Click hit-testing recomputes the markers, so every variant must occupy exactly
    /// the same columns the drawn frame did — otherwise a state change between frames
    /// silently shifts every tab boundary.
    #[test]
    fn every_marker_variant_is_exactly_two_columns() {
        for marker in ["  ", "? ", "! ", "▲ ", "× "] {
            assert_eq!(
                crate::text::display_width(marker),
                2,
                "marker {marker:?} is not two columns"
            );
        }
        for frame in SPINNER {
            assert_eq!(
                crate::text::display_width(&format!("{frame} ")),
                2,
                "spinner frame {frame:?} is not two columns"
            );
        }
    }

    #[test]
    fn the_status_cell_survives_a_title_too_long_for_the_strip() {
        let sources = vec![
            tabs::TabSource {
                marker: "⠋ ".to_string(),
                title: "an-extremely-long-session-title-that-cannot-fit".to_string(),
                running: true,
            },
            tabs::TabSource {
                marker: "? ".to_string(),
                title: "another-very-long-session-title-here".to_string(),
                running: true,
            },
        ];

        let (tab_list, _) = tabs::layout(&sources, 0, 30);

        assert!(
            tab_list[0].label.contains('⠋'),
            "truncation must not eat the spinner: {:?}",
            tab_list[0].label
        );
        assert!(
            tab_list[1].label.contains('?'),
            "truncation must not eat the attention marker: {:?}",
            tab_list[1].label
        );
    }

    #[test]
    fn clicking_a_tab_reports_the_pane_it_covers() {
        let mut app = mux_app_with_theme(ThemeName::Classic);
        let events = app.mux.as_ref().expect("mux").events.clone();
        for (id, title) in [(1u64, "alpha"), (2, "beta"), (3, "gamma")] {
            app.mux
                .as_mut()
                .expect("mux")
                .push(named_pane(events.clone(), id, title));
        }
        app.view = crate::app::View::Attached(1);
        let area = Rect::new(0, 0, 80, 2);

        let mux = app.mux.as_ref().expect("mux");
        let sources = tab_sources(mux);
        let (tab_list, _) = tabs::layout(&sources, 0, area.width as usize);
        let first_width = text::display_width(&tab_list[0].label) as u16;

        // A column inside the first tab resolves to the first pane, and one inside the
        // second resolves to the second.
        assert_eq!(tab_at(mux, area, 1, 0), Some(1));
        assert_eq!(tab_at(mux, area, first_width + 1, 0), Some(2));
        // Below the strip is not the strip.
        assert_eq!(tab_at(mux, area, 1, 5), None);
        // Past the last tab is empty rule, not a tab.
        assert_eq!(tab_at(mux, area, 79, 0), None);

        let _ = app.mux.as_mut().expect("mux").shutdown();
    }

    #[test]
    fn the_rule_spans_the_full_width_so_the_baseline_is_continuous() {
        let mut app = mux_app_with_theme(ThemeName::Classic);
        let events = app.mux.as_ref().expect("mux").events.clone();
        for (id, title) in [(1u64, "first"), (2, "second")] {
            let pane = named_pane(events.clone(), id, title);
            app.mux.as_mut().expect("mux").push(pane);
        }
        app.view = crate::app::View::Attached(1);

        let buffer = render_buffer(&mut app, 80, 24);
        let rule = row(&buffer, crate::ui::TAB_BAR_HEIGHT - 1, 80);

        assert_eq!(rule.chars().count(), 80);
        assert!(
            rule.chars().all(|c| c == '━' || c == '─'),
            "the rule row must be drawn edge to edge: {rule}"
        );
        let _ = app.mux.as_mut().expect("mux").shutdown();
    }

    fn named_pane(
        events: std::sync::mpsc::Sender<crate::mux::MuxEvent>,
        id: u64,
        title: &str,
    ) -> Pane {
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
        Pane::spawn(
            PaneSpec {
                id,
                title: title.to_string(),
                cwd: std::env::temp_dir(),
                session_id: format!("{title}-session"),
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
        .expect("pane spawns")
    }

    #[test]
    fn move_tab_mode_says_so_in_the_status_bar_for_as_long_as_it_is_open() {
        let mut app = mux_app();
        let events = app.mux.as_ref().expect("mux").events.clone();
        let pane = silent_pane(events);
        let id = pane.id;
        app.mux.as_mut().expect("mux").push(pane);
        app.view = crate::app::View::Attached(id);
        app.mux.as_mut().expect("mux").prefix_state =
            PrefixState::Transient(crate::mux::TransientMode::MoveTab);

        let text = render(&mut app);

        assert!(text.contains("Move tab"), "got:\n{text}");
        assert!(
            text.contains("move") && text.contains("Esc done"),
            "the arrows stop reaching Copilot, so the way out has to be on screen, got:\n{text}"
        );
        let _ = app.mux.as_mut().expect("mux").shutdown();
    }

    #[test]
    fn a_held_tab_is_drawn_lifted_so_the_strip_is_not_rearranging_itself() {
        let mut app = mux_app_with_theme(ThemeName::CatppuccinLatte);
        let events = app.mux.as_ref().expect("mux").events.clone();
        for (id, title) in [(1u64, "cst-work"), (2, "map-parse"), (3, "api-fix")] {
            let pane = named_pane(events.clone(), id, title);
            app.mux.as_mut().expect("mux").push(pane);
        }
        app.mux.as_mut().expect("mux").focused = Some(2);
        app.view = crate::app::View::Attached(2);
        let theme = app.theme();

        let settled = render_buffer(&mut app, 80, 24);
        app.dragging_tab = Some(2);
        let held = render_buffer(&mut app, 80, 24);

        let label_row = crate::ui::TAB_BAR_HEIGHT - 2;
        let rule_row = crate::ui::TAB_BAR_HEIGHT - 1;
        let labels = row(&held, label_row, 80);
        let column = labels
            .find("map-parse")
            .map(|byte| labels[..byte].chars().count())
            .expect("held label is drawn") as u16;

        assert_ne!(
            settled[(column, label_row)].bg,
            theme.selection_bg,
            "a tab nobody is holding stays flat"
        );
        assert_eq!(
            held[(column, label_row)].bg,
            theme.selection_bg,
            "the held tab is filled\nlabels: {labels}"
        );
        assert_eq!(
            held[(column, rule_row)].symbol(),
            "╍",
            "and its baseline is broken, because it has not landed yet\nrule: {}",
            row(&held, rule_row, 80)
        );
        assert_eq!(
            settled[(column, rule_row)].symbol(),
            "━",
            "which is a different glyph from merely being focused"
        );
        let _ = app.mux.as_mut().expect("mux").shutdown();
    }
}

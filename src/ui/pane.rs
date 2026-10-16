use ratatui::layout::Rect;
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Paragraph};
use ratatui::Frame;
use std::time::Duration;
use tui_term::widget::{Cursor, PseudoTerminal};

use crate::app::App;
use crate::mux::{PaneId, PaneStatus, PrefixState};
use crate::text;
use crate::theme::{apply_terminal_theme, fill_area, Theme, ThemeName};
use crate::ui::{tabs, ChatSlot};

/// Frames of the startup spinner. Braille dots read as motion even in a plain terminal.
const SPINNER: [&str; 8] = ["⠋", "⠙", "⠹", "⠸", "⠼", "⠴", "⠦", "⠧"];

/// Progress indicator shown while a freshly spawned session has yet to draw anything.
fn draw_starting(f: &mut Frame, area: Rect, elapsed: Duration, theme: Theme) {
    let seconds = elapsed.as_secs();
    // Only mention the wait once it is long enough to be worth reassuring about.
    let detail = (seconds >= 3).then(|| format!("{seconds}s — the CLI is still booting"));
    draw_waiting(
        f,
        area,
        elapsed,
        "Starting Copilot…",
        detail.into_iter().collect(),
        theme,
    );
}

/// Progress shown in a tab whose worktree is still being created.
///
/// The count matters more than the spinner: copying a virtualenv named in
/// `.worktreeinclude` can run for an hour, and only a number that moves tells that
/// apart from a hang.
fn draw_preparing(
    f: &mut Frame,
    area: Rect,
    elapsed: Duration,
    preparing: &crate::app::PreparingWorktree,
    theme: Theme,
) {
    use crate::session::worktree::CreationStage;
    let stage = match preparing.progress.stage() {
        CreationStage::CheckingOut => "Checking out the branch".to_string(),
        CreationStage::FindingIncludes => "Finding the files .worktreeinclude names".to_string(),
        CreationStage::CopyingIncludes { copied, total } => format!(
            "Copying files named in .worktreeinclude: {} of {}",
            grouped(copied),
            grouped(total)
        ),
    };
    draw_waiting(
        f,
        area,
        elapsed,
        &format!("Creating worktree for '{}'…", preparing.branch),
        vec![
            stage,
            format!(
                "{} so far — other tabs work as usual; closing this one cancels it",
                wait_label(elapsed)
            ),
        ],
        theme,
    );
}

/// `48219` as `48,219`: counts this size are read at a glance, not digit by digit.
fn grouped(count: usize) -> String {
    let digits = count.to_string();
    let mut out = String::with_capacity(digits.len() + digits.len() / 3);
    for (index, digit) in digits.chars().enumerate() {
        if index > 0 && (digits.len() - index) % 3 == 0 {
            out.push(',');
        }
        out.push(digit);
    }
    out
}

fn wait_label(elapsed: Duration) -> String {
    let seconds = elapsed.as_secs();
    match seconds {
        0..60 => format!("{seconds}s"),
        60..3600 => format!("{}m {:02}s", seconds / 60, seconds % 60),
        _ => format!("{}h {:02}m", seconds / 3600, seconds % 3600 / 60),
    }
}

fn draw_waiting(
    f: &mut Frame,
    area: Rect,
    elapsed: Duration,
    headline: &str,
    details: Vec<String>,
    theme: Theme,
) {
    let frame = SPINNER[(elapsed.as_millis() / 120) as usize % SPINNER.len()];
    let mut lines = vec![
        Line::from(""),
        Line::from(Span::styled(
            format!("  {frame}  {headline}"),
            Style::default()
                .fg(theme.accent_alt)
                .add_modifier(Modifier::BOLD),
        )),
    ];
    lines.extend(details.into_iter().map(|detail| {
        Line::from(Span::styled(
            format!("     {detail}"),
            Style::default().fg(theme.muted),
        ))
    }));

    let height = lines.len() as u16;
    let box_area = Rect {
        x: area.x,
        y: area.y,
        width: area.width,
        height: height.min(area.height),
    };
    f.render_widget(Paragraph::new(lines), box_area);
}

/// Draw every session on screen, then settle the borders they share.
pub fn draw_chats(f: &mut Frame, app: &App, slots: &[ChatSlot]) {
    let in_split = slots.len() > 1;
    for slot in slots {
        draw_chat(f, app, slot, in_split);
    }
    if !in_split {
        return;
    }
    join_shared_borders(f.buffer_mut(), slots);
    // A boundary belongs to the later slot, so a slot's trailing edge is drawn in its
    // neighbour's colour. Recolouring afterwards gives each highlighted slot its whole
    // frame; the focused one goes last so it wins the edge it shares with a slot that
    // is asking for attention.
    let theme = app.theme();
    let focused = app.mux.as_ref().and_then(|mux| mux.focused);
    let mut highlighted: Vec<(&ChatSlot, Color)> = slots
        .iter()
        .filter_map(|slot| chat_border_color(app, slot.pane, in_split, theme).map(|c| (slot, c)))
        .collect();
    highlighted.sort_by_key(|(slot, _)| Some(slot.pane) == focused);
    for (slot, color) in highlighted {
        recolor_frame(f.buffer_mut(), full_frame(slot), color);
    }
}

/// The colour a chat's border stands out in, or `None` for the resting colour.
fn chat_border_color(app: &App, id: PaneId, in_split: bool, theme: Theme) -> Option<Color> {
    let mux = app.mux.as_ref()?;
    if mux.focused == Some(id) {
        return (app.workspace_focus == crate::app::WorkspaceFocus::Chat)
            .then_some(theme.accent_alt);
    }
    if !in_split {
        return None;
    }
    // Only a split shows an unfocused chat at all. There the status bar describes
    // someone else, so the border is what says this session has died or wants the
    // user — it raises no notification while visible.
    let pane = mux.pane(id)?;
    if !pane.is_running() {
        Some(theme.error)
    } else {
        pane.needs_attention().then_some(theme.warning)
    }
}

/// What a split's border is titled with.
///
/// The tab number leads because titles alone do not tell sessions apart: several
/// started in one project all carry its name until Copilot renames them.
///
/// Fitted to the border's `width`: Copilot names sessions after their task, often at
/// sentence length, and left alone the name ran into the corner with no sign it was
/// cut — and pushed "exited" off the end, which is the part that matters.
fn split_title(
    mux: &crate::mux::MuxState,
    pane: &crate::mux::Pane,
    width: u16,
    zoomed: bool,
) -> String {
    let number = mux
        .number_in_window(pane.id)
        .map(|number| format!("{number} "))
        .unwrap_or_default();
    let state = format!(
        "{}{}",
        if pane.is_running() { "" } else { " · exited" },
        if zoomed { " · zoomed" } else { "" }
    );
    // Both corners, the padding space either side, and one column of border showing
    // after the title so it does not butt against the corner.
    let room = usize::from(width)
        .saturating_sub(5)
        .saturating_sub(text::display_width(&number) + text::display_width(&state));
    format!(
        " {number}{}{state} ",
        text::truncate_to_width(&pane.title, room)
    )
}

/// A slot's frame including the trailing edge its neighbour draws.
fn full_frame(slot: &ChatSlot) -> Rect {
    let mut area = slot.area;
    if !slot.borders.contains(Borders::RIGHT) {
        area.width += 1;
    }
    if !slot.borders.contains(Borders::BOTTOM) {
        area.height += 1;
    }
    area
}

fn is_border_symbol(symbol: &str) -> bool {
    matches!(
        symbol,
        "─" | "│" | "┌" | "┐" | "└" | "┘" | "├" | "┤" | "┬" | "┴" | "┼"
    )
}

/// Recolour the box-drawing cells around `area`, leaving any title text alone.
fn recolor_frame(buffer: &mut ratatui::buffer::Buffer, area: Rect, color: Color) {
    let area = area.intersection(buffer.area);
    if area.is_empty() {
        return;
    }
    let (left, right, top, bottom) = (area.left(), area.right() - 1, area.top(), area.bottom() - 1);
    for x in left..=right {
        for y in [top, bottom] {
            recolor_border_cell(buffer, x, y, color);
        }
    }
    for y in top..=bottom {
        for x in [left, right] {
            recolor_border_cell(buffer, x, y, color);
        }
    }
}

fn recolor_border_cell(buffer: &mut ratatui::buffer::Buffer, x: u16, y: u16, color: Color) {
    if let Some(cell) = buffer.cell_mut((x, y)) {
        if is_border_symbol(cell.symbol()) {
            cell.set_fg(color);
        }
    }
}

/// Turn the corners and edges where borders meet into tees and crosses, so each
/// boundary reads as one divider rather than a box butted against a line.
///
/// Worked out per cell from its neighbours — which of them have a line reaching
/// towards it — rather than from where splits are, so it is right at any depth of
/// nesting: a stack inside a column meets the column's edge with ├ and ┤, and a
/// boundary running into another meets it with ┬ or ┴.
fn join_shared_borders(buffer: &mut ratatui::buffer::Buffer, slots: &[ChatSlot]) {
    // Arms as up, down, left, right.
    fn arms(symbol: &str) -> Option<[bool; 4]> {
        Some(match symbol {
            "─" => [false, false, true, true],
            "│" => [true, true, false, false],
            "┌" => [false, true, false, true],
            "┐" => [false, true, true, false],
            "└" => [true, false, false, true],
            "┘" => [true, false, true, false],
            "├" => [true, true, false, true],
            "┤" => [true, true, true, false],
            "┬" => [false, true, true, true],
            "┴" => [true, false, true, true],
            "┼" => [true, true, true, true],
            _ => return None,
        })
    }
    fn symbol(arms: [bool; 4]) -> Option<&'static str> {
        Some(match arms {
            [true, true, false, true] => "├",
            [true, true, true, false] => "┤",
            [false, true, true, true] => "┬",
            [true, false, true, true] => "┴",
            [true, true, true, true] => "┼",
            _ => return None,
        })
    }
    // Only the frames' own cells count, on both sides. Copilot draws boxes and rules
    // of its own inside a chat, and a rule running up to the border would otherwise
    // join it and put a tee in the middle of a plain edge.
    let mut frame_cells = std::collections::HashSet::new();
    for frame in slots.iter().map(full_frame) {
        let frame = frame.intersection(buffer.area);
        if frame.is_empty() {
            continue;
        }
        for x in frame.left()..frame.right() {
            frame_cells.insert((x, frame.top()));
            frame_cells.insert((x, frame.bottom() - 1));
        }
        for y in frame.top()..frame.bottom() {
            frame_cells.insert((frame.left(), y));
            frame_cells.insert((frame.right() - 1, y));
        }
    }
    let mut joins = Vec::new();
    for &(x, y) in &frame_cells {
        let Some(own) = arms(buffer[(x, y)].symbol()) else {
            continue;
        };
        let reaches = |dx: i32, dy: i32, arm: usize| {
            let (nx, ny) = (i32::from(x) + dx, i32::from(y) + dy);
            let (Ok(nx), Ok(ny)) = (u16::try_from(nx), u16::try_from(ny)) else {
                return false;
            };
            frame_cells.contains(&(nx, ny))
                && arms(buffer[(nx, ny)].symbol()).is_some_and(|a| a[arm])
        };
        // A neighbour reaching towards this cell: up's down arm, and so on.
        let joined = [
            own[0] || reaches(0, -1, 1),
            own[1] || reaches(0, 1, 0),
            own[2] || reaches(-1, 0, 3),
            own[3] || reaches(1, 0, 2),
        ];
        if joined != own {
            if let Some(junction) = symbol(joined) {
                joins.push((x, y, junction));
            }
        }
    }
    for (x, y, junction) in joins {
        buffer[(x, y)].set_symbol(junction);
    }
}

/// The scratchpad or terminal dock in a split, when the focused session has none of
/// its own. Kept on screen rather than removed so the other sessions keep their size.
pub fn draw_empty_dock(f: &mut Frame, area: Rect, title: &str, message: &str, theme: Theme) {
    let block = Block::default()
        .title(title)
        .borders(Borders::ALL)
        .style(panel_style(theme))
        .border_style(Style::default().fg(theme.inactive));
    let inner = block.inner(area);
    fill_area(f.buffer_mut(), area, theme.background);
    f.render_widget(block, area);
    f.render_widget(
        Paragraph::new(Line::from(Span::styled(
            message.to_string(),
            Style::default().fg(theme.muted),
        )))
        .wrap(ratatui::widgets::Wrap { trim: true }),
        inner,
    );
}

fn draw_chat(f: &mut Frame, app: &App, slot: &ChatSlot, in_split: bool) {
    let theme = app.theme();
    let Some(mux) = app.mux.as_ref() else {
        return;
    };
    let Some(pane) = mux.pane(slot.pane) else {
        return;
    };
    let focused = mux.focused == Some(slot.pane);

    let border_color = chat_border_color(app, slot.pane, in_split, theme).unwrap_or(theme.inactive);
    // A view scrolled into history looks exactly like a session that has stopped
    // producing output, which is what it was taken for in testing. Say so on the
    // border, where it cannot cover the history being read. Worked out before the
    // title, which gets whatever room the label leaves rather than being drawn over.
    let scrolled_back = pane
        .with_screen(|screen| screen.scrollback())
        .unwrap_or_default();
    let label = (scrolled_back > 0).then(|| {
        let full = format!(" ↑ {scrolled_back} lines back · type to return ");
        // Long enough to explain itself only when that still leaves the title room.
        if text::display_width(&full) + 24 <= usize::from(slot.area.width) {
            full
        } else {
            format!(" ↑ {scrolled_back} back ")
        }
    });
    let label_width = label
        .as_deref()
        .map_or(0, |label| text::display_width(label) as u16);
    // Each split is titled with its session, since there is no single "the chat" to
    // name any more and the tab strip only marks one of them.
    // A zoomed chat fills the screen exactly as an unsplit one does, so it says so
    // three ways: a heavier frame, the word in the title, and a colour of its own —
    // the theme's other accent, since the usual one means focus and yellow and red
    // already mean attention and exit.
    let zoomed = focused && mux.zoomed();
    let border_color = if zoomed { theme.accent } else { border_color };
    let title = if in_split || zoomed {
        split_title(
            mux,
            pane,
            slot.area.width.saturating_sub(label_width),
            zoomed,
        )
    } else {
        " Chat ".to_string()
    };
    let block = Block::default()
        .title(title)
        .border_type(if zoomed {
            ratatui::widgets::BorderType::Thick
        } else {
            ratatui::widgets::BorderType::Plain
        })
        .borders(slot.borders)
        .style(panel_style(theme))
        .border_style(Style::default().fg(border_color));
    let area = slot.area;
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
    let cursor = Cursor::default().visibility(
        focused && app.workspace_focus == crate::app::WorkspaceFocus::Chat && !starting,
    );
    pane.with_screen(|screen| {
        let widget = PseudoTerminal::new(screen).cursor(cursor);
        f.render_widget(widget, terminal_area);
        apply_terminal_theme(f.buffer_mut(), terminal_area, theme);
    });
    if let Some(label) = label.filter(|_| area.height > 0) {
        let width = label_width.min(area.width.saturating_sub(4));
        let label_area = Rect {
            x: area.right().saturating_sub(width + 2),
            y: area.y,
            width,
            height: 1,
        };
        f.render_widget(
            Paragraph::new(Span::styled(label, Style::default().fg(theme.warning))),
            label_area,
        );
    }
    // Reference statuses are resolved against the focused session's repository. Another
    // split may be in a different one, where the same number is a different item, so
    // its references stay plain rather than risk a wrong colour.
    if focused {
        decorate_references(f, app, terminal_area, theme);
    }

    if let Some(preparing) = app.preparing_worktree(pane.id) {
        draw_preparing(
            f,
            terminal_area,
            pane.started_at.elapsed(),
            preparing,
            theme,
        );
    } else if starting {
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
            let mut spans = vec![
                Span::styled(mode.badge(), badge_style(theme, theme.accent_alt)),
                Span::raw(mode.hint()),
            ];
            // Why the last step did nothing, e.g. a split already at its minimum.
            if let Some(message) = app.status_message.as_deref() {
                spans.push(Span::styled(
                    format!(" {message} "),
                    Style::default().fg(theme.warning),
                ));
            }
            spans
        }
        PaneStatus::Running if mux.prefix_state == PrefixState::Help => vec![
            Span::styled(" Help ", badge_style(theme, theme.warning)),
            Span::raw(" e scratchpad  Esc cancel "),
        ],
        PaneStatus::Running if mux.prefix_state == PrefixState::Github => vec![
            Span::styled(" GitHub ", badge_style(theme, theme.accent)),
            Span::raw(" i inspect  m messages  Esc cancel "),
        ],
        PaneStatus::Running if mux.prefix_state == PrefixState::Layout => vec![
            Span::styled(" Layout ", badge_style(theme, theme.accent_alt)),
            Span::raw(format!(" {} ", crate::mux::LAYOUT_HINT)),
        ],
        PaneStatus::Running if mux.prefix_state == PrefixState::Root => vec![
            Span::styled(format!(" {prefix} "), badge_style(theme, theme.warning)),
            Span::raw(format!(" choose a command · {prefix} search · Esc close ")),
        ],
        // The answer to the last command — "nothing to zoom", a bell in another tab —
        // until the next key. This bar used to show none of them, so a command that
        // could not act looked like a key that had not registered.
        PaneStatus::Running
            if mux.prefix_state == PrefixState::Idle && app.status_message.is_some() =>
        {
            vec![Span::styled(
                format!(
                    " {} ",
                    text::truncate_to_width(
                        app.status_message.as_deref().unwrap_or_default(),
                        area.width.saturating_sub(2) as usize,
                    )
                ),
                Style::default().fg(theme.warning),
            )]
        }
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
        // A split that is not being drawn is otherwise indistinguishable from no split,
        // and the user would have no idea why the other sessions vanished.
        PaneStatus::Running if mux.prefix_state == PrefixState::Idle && mux.zoomed() => {
            vec![
                Span::styled(" Zoomed ", badge_style(theme, theme.accent_alt)),
                Span::raw(format!(" {prefix} l z shows the split again ")),
            ]
        }
        PaneStatus::Running
            if mux.prefix_state == PrefixState::Idle && app.workspace_areas.split_collapsed =>
        {
            vec![
                Span::styled(" Split hidden ", badge_style(theme, theme.warning)),
                Span::raw(
                    " too little room for every session; enlarge the window or close a panel ",
                ),
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

/// Tab titles exactly as the bar draws them, one per tab.
///
/// Shared with click hit-testing so the two can never disagree about where a tab starts
/// and ends.
pub fn tab_sources(mux: &crate::mux::MuxState) -> Vec<tabs::TabSource> {
    mux.windows
        .iter()
        .filter_map(|window| {
            let shown = mux.pane(window.last_focused)?;
            let members: Vec<&crate::mux::Pane> = window
                .layout
                .panes()
                .into_iter()
                .filter_map(|id| mux.pane(id))
                .collect();
            // A tab of several sessions is titled by the one last focused there, marked
            // as holding more. Every title would be legible only one at a time: the
            // strip gives a tab a couple of dozen columns, and Copilot names sessions
            // with whole sentences.
            let title = if window.is_split() {
                format!("⧉ {}", shown.title)
            } else {
                shown.title.clone()
            };
            Some(tabs::TabSource {
                marker: window_marker(&members),
                title,
                running: members.iter().any(|pane| pane.is_running()),
            })
        })
        .collect()
}

/// The marker for a tab: whichever of its sessions most wants the user. A question in
/// one session of three must not hide behind a spinner in another.
fn window_marker(members: &[&crate::mux::Pane]) -> String {
    let markers: Vec<String> = members.iter().map(|pane| tab_marker(pane)).collect();
    let rank = |marker: &str| match marker {
        "? " => 0,
        "● " => 1,
        "! " => 2,
        "▲ " => 3,
        "✎ " => 5,
        "× " => 6,
        "  " => 7,
        // Anything else is a spinner frame: a turn in progress.
        _ => 4,
    };
    markers
        .into_iter()
        .min_by_key(|marker| rank(marker))
        .unwrap_or_else(|| "  ".to_string())
}

/// The session a click on the tab covering `column` brings back: whichever had the
/// keyboard last in that tab.
pub fn tab_at(
    mux: &crate::mux::MuxState,
    area: Rect,
    column: u16,
    row: u16,
) -> Option<crate::mux::PaneId> {
    tab_index_at(mux, area, column, row).map(|index| mux.windows[index].last_focused)
}

/// Position in the strip of the tab covering `column`, for a drag that has to know
/// where it is going rather than only which tab it is over.
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
            return (index < mux.windows.len()).then_some(index);
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
    Some((start, last.min(mux.windows.len().saturating_sub(1))))
}

/// The rendered strip as `(index of the first tab, width of each tab)`.
///
/// The strip is windowed around the focused tab when it overflows, so every caller
/// mapping a column back to a tab has to apply that same offset. Sharing one
/// computation keeps hit-testing from drifting away from what was drawn.
fn strip(mux: &crate::mux::MuxState, area: Rect) -> (usize, Vec<u16>) {
    let focused_index = mux
        .focused
        .and_then(|id| mux.window_index_of(id))
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
    let Some(focused_index) = mux.focused.and_then(|id| mux.window_index_of(id)) else {
        return;
    };
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
        let held = app.dragging_tab.is_some_and(|dragged| {
            mux.windows
                .get(start + offset)
                .is_some_and(|window| window.layout.contains(dragged))
        });
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
    fn an_unsent_draft_is_marked_and_deleting_it_clears_the_fixed_width_marker() {
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

        pane.note_user_key(&crossterm::event::KeyEvent::new(
            crossterm::event::KeyCode::Backspace,
            crossterm::event::KeyModifiers::NONE,
        ));
        assert_eq!(
            tab_marker(&pane),
            "  ",
            "removing all prompt text clears the pencil immediately"
        );

        pane.note_user_key(&crossterm::event::KeyEvent::new(
            crossterm::event::KeyCode::Char('h'),
            crossterm::event::KeyModifiers::NONE,
        ));
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
    fn the_attached_status_bar_answers_a_command_that_could_not_act_until_the_next_key() {
        let mut app = mux_app();
        let events = app.mux.as_ref().expect("mux").events.clone();
        let pane = silent_pane(events);
        let id = pane.id;
        app.mux.as_mut().expect("mux").push(pane);
        app.view = crate::app::View::Attached(id);
        app.status_message = Some("Nothing to zoom: only one session is on screen".to_string());

        assert!(
            render(&mut app).contains("Nothing to zoom"),
            "without this a refused command looks like a key that did not register"
        );

        crate::mux_input::handle_attached_event(
            &mut app,
            crossterm::event::Event::Key(crossterm::event::KeyEvent::new(
                crossterm::event::KeyCode::Char('a'),
                crossterm::event::KeyModifiers::NONE,
            )),
        );
        assert!(!render(&mut app).contains("Nothing to zoom"));
        assert!(render(&mut app).contains("for commands"));
        let _ = app.mux.as_mut().expect("mux").shutdown();
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

    #[test]
    fn side_by_side_sessions_share_one_divider_lit_for_whichever_has_focus() {
        let mut app = mux_app();
        let events = app.mux.as_ref().expect("mux").events.clone();
        app.mux
            .as_mut()
            .expect("mux")
            .push(named_pane(events.clone(), 1, "left"));
        app.mux
            .as_mut()
            .expect("mux")
            .push(named_pane(events, 2, "right"));
        app.mux.as_mut().expect("mux").focus(1);
        app.mux
            .as_mut()
            .expect("mux")
            .split_with(crate::mux::SplitDirection::Columns, 2);
        app.view = crate::app::View::Attached(2);

        let buffer = render_buffer(&mut app, 100, 30);
        let slots = app.workspace_areas.chats.clone();
        assert_eq!(slots.len(), 2);
        let divider = slots[1].area.x;
        let (top, bottom) = (slots[1].area.y, slots[1].area.bottom() - 1);
        assert_eq!(
            buffer[(divider, top)].symbol(),
            "┬",
            "{}",
            row(&buffer, top, 100)
        );
        assert_eq!(buffer[(divider, bottom)].symbol(), "┴");
        assert_eq!(
            buffer[(divider - 1, top + 1)].symbol(),
            " ",
            "one column between the two chats, not two"
        );

        let accent = app.theme().accent_alt;
        assert_eq!(buffer[(divider, top + 1)].style().fg, Some(accent));
        assert_ne!(
            buffer[(slots[0].area.x, top + 1)].style().fg,
            Some(accent),
            "the unfocused split's own edge stays at rest"
        );

        // Focusing the left session moves the lit frame, divider included.
        app.mux.as_mut().expect("mux").focus(1);
        app.view = crate::app::View::Attached(1);
        let buffer = render_buffer(&mut app, 100, 30);
        assert_eq!(buffer[(slots[0].area.x, top + 1)].style().fg, Some(accent));
        assert_eq!(buffer[(divider, top + 1)].style().fg, Some(accent));
        assert!(
            row(&buffer, top, 100).contains(" 1 left "),
            "titled with the tab number, since sessions in one project share a name"
        );
        assert!(row(&buffer, top, 100).contains(" 2 right "));

        // A watched split that dies says so itself: the status bar is describing the
        // focused one.
        app.mux
            .as_mut()
            .expect("mux")
            .pane_mut(2)
            .expect("pane")
            .mark_exited(Some(1));
        let buffer = render_buffer(&mut app, 100, 30);
        assert!(row(&buffer, top, 100).contains(" 2 right · exited "));
        assert_eq!(
            buffer[(slots[1].area.right() - 1, top + 1)].style().fg,
            Some(app.theme().error)
        );
        let _ = app.mux.as_mut().expect("mux").shutdown();
    }

    /// Found by running CST: the label was drawn over the end of a long title, which
    /// then read "1 Refactor the a ↑ 3 lines back".
    #[test]
    fn a_scrolled_back_split_shows_its_label_beside_the_title_rather_than_over_it() {
        let mut app = mux_app();
        let events = app.mux.as_ref().expect("mux").events.clone();
        app.mux.as_mut().expect("mux").push(named_pane(
            events.clone(),
            1,
            "Refactor the authentication middleware to support OAuth device flow",
        ));
        app.mux
            .as_mut()
            .expect("mux")
            .push(named_pane(events.clone(), 2, "two"));
        app.mux
            .as_mut()
            .expect("mux")
            .push(named_pane(events, 3, "three"));
        app.mux.as_mut().expect("mux").focus(1);
        for id in [2, 3] {
            app.mux
                .as_mut()
                .expect("mux")
                .split_with(crate::mux::SplitDirection::Columns, id);
        }
        app.view = crate::app::View::Attached(3);
        let history: String = (1..=100).map(|line| format!("line {line}\r\n")).collect();
        let pane = app.mux.as_mut().expect("mux").pane_mut(1).expect("pane");
        pane.feed(history.as_bytes());
        pane.handle_mouse(crossterm::event::MouseEvent {
            kind: crossterm::event::MouseEventKind::ScrollUp,
            column: 2,
            row: 2,
            modifiers: crossterm::event::KeyModifiers::NONE,
        })
        .expect("scrolls");

        let buffer = render_buffer(&mut app, 160, 30);
        let slot = app.workspace_areas.chats[0];
        let top: String = (slot.area.x..slot.area.right())
            .map(|x| buffer[(x, slot.area.y)].symbol().to_string())
            .collect();
        let label = top.find('↑').expect("the label is there");
        let ellipsis = top.find('…').expect("the title is cut to fit");
        assert!(ellipsis < label, "{top}");
        assert!(top.contains(" 1 Refactor"), "{top}");
        let _ = app.mux.as_mut().expect("mux").shutdown();
    }

    /// A zoomed split fills the screen like an unsplit chat; without a mark of its own
    /// there was no telling the other sessions were still there.
    #[test]
    fn a_zoomed_split_has_a_heavier_frame_and_says_zoomed() {
        let mut app = mux_app();
        let events = app.mux.as_ref().expect("mux").events.clone();
        app.mux
            .as_mut()
            .expect("mux")
            .push(named_pane(events.clone(), 1, "left"));
        app.mux
            .as_mut()
            .expect("mux")
            .push(named_pane(events, 2, "right"));
        app.mux.as_mut().expect("mux").focus(1);
        app.mux
            .as_mut()
            .expect("mux")
            .split_with(crate::mux::SplitDirection::Columns, 2);
        app.mux.as_mut().expect("mux").toggle_split_zoom();
        app.view = crate::app::View::Attached(2);

        let buffer = render_buffer(&mut app, 100, 30);
        let chat = app.workspace_areas.chat;
        assert_eq!(buffer[(chat.x, chat.y)].symbol(), "┏");
        assert_eq!(
            buffer[(chat.x, chat.y + 1)].style().fg,
            Some(app.theme().accent),
            "a colour of its own, not the focus accent"
        );
        assert_ne!(app.theme().accent, app.theme().accent_alt);
        assert!(row(&buffer, chat.y, 100).contains("right · zoomed"));

        app.mux.as_mut().expect("mux").toggle_split_zoom();
        let buffer = render_buffer(&mut app, 100, 30);
        let chat = app.workspace_areas.chats[0].area;
        assert_eq!(buffer[(chat.x, chat.y)].symbol(), "┌");
        assert!(!row(&buffer, chat.y, 100).contains("zoomed"));
        let _ = app.mux.as_mut().expect("mux").shutdown();
    }

    #[test]
    fn a_long_split_title_is_cut_with_an_ellipsis_and_never_hides_that_it_exited() {
        let mut app = mux_app();
        let events = app.mux.as_ref().expect("mux").events.clone();
        app.mux
            .as_mut()
            .expect("mux")
            .push(named_pane(events.clone(), 1, "short"));
        app.mux.as_mut().expect("mux").push(named_pane(
            events,
            2,
            "Refactor the authentication middleware to support OAuth device flow",
        ));
        app.mux.as_mut().expect("mux").focus(1);
        app.mux
            .as_mut()
            .expect("mux")
            .split_with(crate::mux::SplitDirection::Columns, 2);
        app.mux
            .as_mut()
            .expect("mux")
            .pane_mut(2)
            .expect("pane")
            .mark_exited(Some(1));
        app.view = crate::app::View::Attached(2);

        let buffer = render_buffer(&mut app, 100, 30);
        let slot = app.workspace_areas.chats[1];
        let top = row(&buffer, slot.area.y, 100);
        assert!(top.contains("… · exited "), "{top}");
        assert_eq!(
            buffer[(slot.area.right() - 2, slot.area.y)].symbol(),
            "─",
            "the title stops short of the corner: {top}"
        );
        let _ = app.mux.as_mut().expect("mux").shutdown();
    }

    #[test]
    fn a_stack_inside_a_column_joins_the_borders_around_it_with_tees() {
        let mut app = mux_app();
        let events = app.mux.as_ref().expect("mux").events.clone();
        for (id, title) in [(1, "tall"), (2, "upper"), (3, "lower"), (4, "elsewhere")] {
            app.mux
                .as_mut()
                .expect("mux")
                .push(named_pane(events.clone(), id, title));
        }
        let mux = app.mux.as_mut().expect("mux");
        mux.focus(1);
        mux.split_with(crate::mux::SplitDirection::Columns, 2);
        mux.split_with(crate::mux::SplitDirection::Rows, 3);
        app.view = crate::app::View::Attached(3);

        let buffer = render_buffer(&mut app, 120, 36);
        let areas = app.workspace_areas.clone();
        let find = |id| {
            areas
                .chats
                .iter()
                .find(|slot| slot.pane == id)
                .unwrap()
                .area
        };
        let (tall, upper, lower) = (find(1), find(2), find(3));
        // The column boundary runs the full height; the stack's boundary meets it.
        assert_eq!(buffer[(upper.x, tall.y)].symbol(), "┬");
        assert_eq!(buffer[(upper.x, tall.bottom() - 1)].symbol(), "┴");
        assert_eq!(buffer[(lower.x, lower.y)].symbol(), "├");
        assert_eq!(buffer[(lower.right() - 1, lower.y)].symbol(), "┤");
        assert_eq!(upper.x, lower.x);

        // One tab for the three, marked as holding more than one, and one for the rest.
        let strip = row(&buffer, areas.tabs.y + 1, 120);
        assert!(strip.contains("⧉ lower"), "{strip}");
        assert!(strip.contains("elsewhere"), "{strip}");
        assert!(!strip.contains(" tall"), "{strip}");
        let _ = app.mux.as_mut().expect("mux").shutdown();
    }

    #[test]
    fn stacked_sessions_share_a_titled_divider_lit_down_to_it_for_the_upper_one() {
        let mut app = mux_app();
        let events = app.mux.as_ref().expect("mux").events.clone();
        app.mux
            .as_mut()
            .expect("mux")
            .push(named_pane(events.clone(), 1, "upper"));
        app.mux
            .as_mut()
            .expect("mux")
            .push(named_pane(events, 2, "lower"));
        app.mux.as_mut().expect("mux").focus(1);
        app.mux
            .as_mut()
            .expect("mux")
            .split_with(crate::mux::SplitDirection::Rows, 2);
        app.mux.as_mut().expect("mux").focus(1);
        app.view = crate::app::View::Attached(1);

        let buffer = render_buffer(&mut app, 100, 40);
        let slots = app.workspace_areas.chats.clone();
        let divider = slots[1].area.y;
        let (left, right) = (slots[1].area.x, slots[1].area.right() - 1);
        assert_eq!(buffer[(left, divider)].symbol(), "├");
        assert_eq!(buffer[(right, divider)].symbol(), "┤");
        assert!(
            row(&buffer, divider, 100).contains("lower"),
            "the shared line carries the lower session's title"
        );
        assert_eq!(
            buffer[(left + 1, divider - 1)].symbol(),
            " ",
            "one row between the two chats, not two"
        );

        // The upper split is focused, so its frame is lit down to the shared line,
        // while the lower one's own bottom edge stays at rest.
        let accent = app.theme().accent_alt;
        assert_eq!(buffer[(left, divider)].style().fg, Some(accent));
        assert_eq!(buffer[(left, divider - 1)].style().fg, Some(accent));
        assert_ne!(
            buffer[(left, slots[1].area.bottom() - 1)].style().fg,
            Some(accent)
        );
        let _ = app.mux.as_mut().expect("mux").shutdown();
    }
    /// The count is what tells an hour-long copy apart from a hang; a tab that said
    /// "Starting Copilot" instead would be both wrong and the same silence as before.
    #[test]
    fn a_tab_waiting_on_its_worktree_counts_the_files_copied_instead_of_saying_copilot_is_starting()
    {
        let mut app = mux_app();
        let events = app.mux.as_ref().expect("mux").events.clone();
        app.mux.as_mut().expect("mux").push(Pane::unlaunched(
            1,
            "feature".to_string(),
            std::env::temp_dir(),
            "waiting-session".to_string(),
            24,
            80,
            Some(false),
            events,
        ));
        app.view = crate::app::View::Attached(1);
        let progress = std::sync::Arc::new(crate::session::worktree::CreationProgress::default());
        progress.plan_copies(48219);
        progress.note_copied(31209);
        app.preparing_worktrees.push(crate::app::PreparingWorktree {
            pane: 1,
            branch: "feature".to_string(),
            progress,
            favorite: false,
        });

        let text = render_buffer(&mut app, 120, 24)
            .content()
            .iter()
            .map(|cell| cell.symbol())
            .collect::<String>();

        assert!(
            text.contains("Creating worktree for 'feature'"),
            "got:\n{text}"
        );
        assert!(text.contains("31,209 of 48,219"), "got:\n{text}");
        assert!(!text.contains("Starting Copilot"), "got:\n{text}");
    }

    #[test]
    fn counts_are_grouped_in_thousands_and_waits_read_in_the_largest_useful_unit() {
        assert_eq!(grouped(7), "7");
        assert_eq!(grouped(48219), "48,219");
        assert_eq!(grouped(1234567), "1,234,567");
        assert_eq!(wait_label(Duration::from_secs(42)), "42s");
        assert_eq!(wait_label(Duration::from_secs(605)), "10m 05s");
        assert_eq!(wait_label(Duration::from_secs(5520)), "1h 32m");
    }
}

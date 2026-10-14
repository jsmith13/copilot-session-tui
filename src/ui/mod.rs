pub mod command_palette;
pub mod diff;
pub mod file_tree;
pub mod github_inspector;
pub mod hyperlinks;
pub mod markdown;
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
use ratatui::widgets::{Block, Borders, Paragraph};
use ratatui::Frame;

use crate::app::{App, Mode, View, WorkspaceAreas, WorkspaceFocus, WorkspaceHelp};
use crate::mux::{LayoutNode, PaneId, SplitDirection};
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

/// Smallest chat, inside its border, that a split may leave a session with. Copilot's
/// own layout falls apart below this, so a split that cannot give every session this
/// much hides itself rather than squeezing them. Three columns fit on 120.
pub const MIN_SPLIT_COLS: u16 = 32;
pub const MIN_SPLIT_ROWS: u16 = 8;

/// One session's chat on screen.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ChatSlot {
    pub pane: PaneId,
    pub area: Rect,
    /// Edges this slot draws itself. A slot followed by another leaves its trailing
    /// edge to that neighbour, so a boundary costs one column rather than two — which
    /// is also what gives a divider a single cell to grab.
    pub borders: Borders,
}

impl ChatSlot {
    /// Where the session's own cells go; see [`AttachedLayout::chat_pane`].
    pub fn pane_area(&self) -> Rect {
        Block::default().borders(self.borders).inner(self.area)
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AttachedLayout {
    pub tabs: Rect,
    /// The focused session's chat box.
    pub chat: Rect,
    pub chat_borders: Borders,
    /// Every session on screen, in split order. Exactly one outside a split.
    pub chats: Vec<ChatSlot>,
    /// Where each split's members went, outermost first.
    pub splits: Vec<SplitGeometry>,
    pub scratchpad: Option<Rect>,
    pub terminal: Option<Rect>,
    pub status: Rect,
    /// A split exists but the window is too small to show it, so only the focused
    /// session is drawn.
    pub split_collapsed: bool,
    /// The whole screen this was laid out for.
    pub screen: Rect,
}

/// Where one split in a tab's layout put its members, kept for dragging the borders
/// between them and resizing from the keyboard — both need the cells as drawn.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SplitGeometry {
    /// Child indices from the root of the layout to this split.
    pub path: Vec<usize>,
    pub direction: SplitDirection,
    /// Where each member starts along the split's direction.
    pub starts: Vec<u16>,
    /// Each member's length along it, borders included.
    pub lengths: Vec<u16>,
    /// The least each member can be along it.
    pub minimums: Vec<u16>,
    /// Where the split starts across its direction, and how far it reaches.
    pub across: (u16, u16),
}

impl AttachedLayout {
    /// Where the focused session's own cells go: inside the chat box's border.
    ///
    /// One definition, because two things must agree on it exactly — where panes are
    /// sized to, and where anything that addresses a pane's cells from outside thinks they
    /// are. A click, or a link drawn over the frame, lands a cell off otherwise.
    pub fn chat_pane(&self) -> Rect {
        Block::default().borders(self.chat_borders).inner(self.chat)
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
        let layout = app.attached_layout(size);
        app.workspace_areas = WorkspaceAreas::from_layout(&layout);
        pane::draw_chats(f, app, &layout.chats);
        let focused_title = app
            .mux
            .as_ref()
            .and_then(|mux| {
                let pane = mux.focused_pane()?;
                Some(match mux.number_in_window(pane.id) {
                    Some(number) => format!("{number} {}", pane.title),
                    None => pane.title.clone(),
                })
            })
            .unwrap_or_default();
        let own_scratchpad = app.attached_scratchpad_visible();
        if let Some(area) = layout.scratchpad {
            match app.scratchpad.as_mut() {
                Some(scratchpad) if own_scratchpad => {
                    scratchpad::draw_in_with_theme(
                        f,
                        scratchpad,
                        area,
                        app.workspace_focus == WorkspaceFocus::Scratchpad,
                        theme,
                    );
                }
                _ => pane::draw_empty_dock(
                    f,
                    area,
                    " Scratchpad ",
                    &format!("No scratchpad for {focused_title} — prefix e opens one"),
                    theme,
                ),
            }
        }
        if let Some(area) = layout.terminal {
            match app.terminal.active() {
                Some(terminal) if app.attached_terminal_visible() => {
                    terminal_pane::draw_with_theme(
                        f,
                        terminal,
                        app.workspace_focus == WorkspaceFocus::Terminal,
                        area,
                        theme,
                    );
                }
                _ => pane::draw_empty_dock(
                    f,
                    area,
                    " Terminal ",
                    &format!("No terminal for {focused_title} — prefix t opens one"),
                    theme,
                ),
            }
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
        // Asked while attached only when starting a new session into a split.
        if app.mode == Mode::FilterProject {
            popups::draw_project_filter(f, app);
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
        Mode::RecoverWorkspace => popups::draw_recover_workspace(f, app),
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

#[cfg(test)]
pub fn attached_layout(
    area: Rect,
    scratchpad_visible: bool,
    terminal_visible: bool,
    tabs_visible: bool,
) -> AttachedLayout {
    attached_layout_sized(
        area,
        scratchpad_visible,
        terminal_visible,
        tabs_visible,
        DockSizes::default(),
    )
}

/// How much room the docks take. Starts at the built-in proportions; dragging a
/// dock's edge changes it for as long as CST runs.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct DockSizes {
    pub scratchpad_percent: u16,
    /// `None` until someone drags it, so until then the height follows the window.
    pub terminal_rows: Option<u16>,
}

impl Default for DockSizes {
    fn default() -> Self {
        Self {
            scratchpad_percent: 35,
            terminal_rows: None,
        }
    }
}

/// How far the scratchpad column can be dragged: never so narrow it is lost under the
/// pointer, never so wide the chats get less than a third.
pub const SCRATCHPAD_PERCENT_RANGE: std::ops::RangeInclusive<u16> = 15..=65;
pub const MIN_TERMINAL_ROWS: u16 = 4;
/// The chats keep at least this many rows above the terminal, however far it is dragged.
const MIN_ROWS_ABOVE_TERMINAL: u16 = 5;

pub fn attached_layout_sized(
    area: Rect,
    scratchpad_visible: bool,
    terminal_visible: bool,
    tabs_visible: bool,
    sizes: DockSizes,
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
                Constraint::Min(MIN_ROWS_ABOVE_TERMINAL),
                Constraint::Length(
                    sizes
                        .terminal_rows
                        .unwrap_or_else(|| terminal_panel_height(content.height))
                        .min(content.height.saturating_sub(MIN_ROWS_ABOVE_TERMINAL)),
                ),
            ])
            .split(content);
        (sections[0], Some(sections[1]))
    } else {
        (content, None)
    };

    let (chat, scratchpad) = if scratchpad_visible {
        let sections = Layout::default()
            .direction(Direction::Horizontal)
            .constraints([
                Constraint::Percentage(100 - sizes.scratchpad_percent),
                Constraint::Percentage(sizes.scratchpad_percent),
            ])
            .split(top);
        (sections[0], Some(sections[1]))
    } else {
        (top, None)
    };

    AttachedLayout {
        tabs,
        chat,
        chat_borders: Borders::ALL,
        chats: Vec::new(),
        splits: Vec::new(),
        scratchpad,
        terminal,
        status,
        split_collapsed: false,
        screen: area,
    }
}

/// The attached layout with a tab's sessions sharing the chat area by `layout`, or
/// `None` when the window is too small to give each of them a usable chat.
///
/// The docks are laid out first and the sessions get what is left, so the scratchpad
/// and terminal are the same size however many sessions share the screen.
pub fn attached_split_layout(
    area: Rect,
    scratchpad_visible: bool,
    terminal_visible: bool,
    tabs_visible: bool,
    sizes: DockSizes,
    layout: &LayoutNode,
    focused: PaneId,
) -> Option<AttachedLayout> {
    let mut attached = attached_layout_sized(
        area,
        scratchpad_visible,
        terminal_visible,
        tabs_visible,
        sizes,
    );
    let mut chats = Vec::new();
    let mut splits = Vec::new();
    lay_out(
        layout,
        attached.chat,
        (false, false),
        &mut Vec::new(),
        &mut chats,
        &mut splits,
    )?;
    let focused = chats.iter().find(|slot| slot.pane == focused)?;
    attached.chat = focused.area;
    attached.chat_borders = focused.borders;
    attached.chats = chats;
    attached.splits = splits;
    Some(attached)
}

/// The inner length a chat needs along `axis`: width for columns, height for rows.
fn chat_minimum(axis: SplitDirection) -> u16 {
    match axis {
        SplitDirection::Columns => MIN_SPLIT_COLS,
        SplitDirection::Rows => MIN_SPLIT_ROWS,
    }
}

/// The least room `node` needs along `axis`, borders included.
///
/// `drop_trailing` is whether its trailing edge on that axis is drawn by a neighbour:
/// a boundary between two members is drawn once, by the later one, so everything but
/// the last member of a split saves a line.
pub fn minimum_extent(node: &LayoutNode, axis: SplitDirection, drop_trailing: bool) -> u16 {
    match node {
        LayoutNode::Pane(_) => chat_minimum(axis) + 1 + u16::from(!drop_trailing),
        LayoutNode::Split {
            direction,
            children,
            ..
        } if *direction == axis => {
            let last = children.len().saturating_sub(1);
            children
                .iter()
                .enumerate()
                .map(|(index, child)| minimum_extent(child, axis, index < last || drop_trailing))
                .sum()
        }
        LayoutNode::Split { children, .. } => children
            .iter()
            .map(|child| minimum_extent(child, axis, drop_trailing))
            .max()
            .unwrap_or(0),
    }
}

/// Lay `node` out in `area`. `drops` says whether its right and bottom edges belong
/// to a neighbour; see [`minimum_extent`]. The rule holds at every depth only because
/// a split never sits directly inside one running the same way.
fn lay_out(
    node: &LayoutNode,
    area: Rect,
    drops: (bool, bool),
    path: &mut Vec<usize>,
    chats: &mut Vec<ChatSlot>,
    splits: &mut Vec<SplitGeometry>,
) -> Option<()> {
    let (drop_right, drop_bottom) = drops;
    match node {
        LayoutNode::Pane(pane) => {
            let mut borders = Borders::ALL;
            if drop_right {
                borders -= Borders::RIGHT;
            }
            if drop_bottom {
                borders -= Borders::BOTTOM;
            }
            let inner = Block::default().borders(borders).inner(area);
            if inner.width < MIN_SPLIT_COLS || inner.height < MIN_SPLIT_ROWS {
                return None;
            }
            chats.push(ChatSlot {
                pane: *pane,
                area,
                borders,
            });
            Some(())
        }
        LayoutNode::Split {
            direction,
            children,
            weights,
        } => {
            let (length, start, drop_along, across) = match direction {
                SplitDirection::Columns => (area.width, area.x, drop_right, (area.y, area.height)),
                SplitDirection::Rows => (area.height, area.y, drop_bottom, (area.x, area.width)),
            };
            let last = children.len().saturating_sub(1);
            let drops_along: Vec<bool> = (0..children.len())
                .map(|index| index < last || drop_along)
                .collect();
            let minimums: Vec<u16> = children
                .iter()
                .zip(&drops_along)
                .map(|(child, drop)| minimum_extent(child, *direction, *drop))
                .collect();
            let lengths = share_with_minimums(length, weights, &minimums)?;
            let mut starts = Vec::with_capacity(children.len());
            let mut offset = start;
            for (index, child) in children.iter().enumerate() {
                starts.push(offset);
                let (child_area, child_drops) = match direction {
                    SplitDirection::Columns => (
                        Rect::new(offset, area.y, lengths[index], area.height),
                        (drops_along[index], drop_bottom),
                    ),
                    SplitDirection::Rows => (
                        Rect::new(area.x, offset, area.width, lengths[index]),
                        (drop_right, drops_along[index]),
                    ),
                };
                path.push(index);
                let placed = lay_out(child, child_area, child_drops, path, chats, splits);
                path.pop();
                placed?;
                offset += lengths[index];
            }
            splits.push(SplitGeometry {
                path: path.clone(),
                direction: *direction,
                starts,
                lengths,
                minimums,
                across,
            });
            Some(())
        }
    }
}

/// Dock sizes shrunk, as far as their minimums, so that `layout` fits beside them.
///
/// Docks give way before sessions do. Opening the terminal under three stacked
/// sessions at its usual height left them too few rows, and the whole split vanished
/// the moment the terminal appeared. These sizes are only for laying this screen out;
/// the ones the user dragged to are kept, and come back when there is room.
pub fn docks_fitted_to_split(
    area: Rect,
    tabs_visible: bool,
    scratchpad_visible: bool,
    terminal_visible: bool,
    sizes: DockSizes,
    layout: &LayoutNode,
) -> DockSizes {
    let needed_cols = minimum_extent(layout, SplitDirection::Columns, false);
    let needed_rows = minimum_extent(layout, SplitDirection::Rows, false);
    let mut fitted = sizes;
    if terminal_visible {
        let tab_height = if tabs_visible { TAB_BAR_HEIGHT } else { 0 };
        let content = area.height.saturating_sub(tab_height + 1);
        let current = sizes
            .terminal_rows
            .unwrap_or_else(|| terminal_panel_height(content));
        let room = content.saturating_sub(needed_rows);
        fitted.terminal_rows = Some(current.min(room).max(MIN_TERMINAL_ROWS));
    }
    if scratchpad_visible && area.width > 0 {
        let room = area.width.saturating_sub(needed_cols);
        // A percent short of the exact fit, because the layout rounds.
        let cap = (u32::from(room) * 100 / u32::from(area.width)) as u16;
        fitted.scratchpad_percent = sizes
            .scratchpad_percent
            .min(cap.saturating_sub(1))
            .max(*SCRATCHPAD_PERCENT_RANGE.start());
    }
    fitted
}

/// Split `total` cells in proportion to `weights`, giving every share at least
/// `minimum`. `None` when there are not enough cells for that.
#[cfg(test)]
pub(crate) fn share_cells(total: u16, weights: &[u16], minimum: u16) -> Option<Vec<u16>> {
    share_with_minimums(total, weights, &vec![minimum; weights.len()])
}

/// Split `total` cells in proportion to `weights`, giving share `i` at least
/// `minimums[i]`. `None` when there are not enough cells for that.
///
/// Rounding can leave each share a cell short, and those cells go to the leading
/// shares so the sizes always add up to exactly `total` — a gap would show the
/// background through between two chats.
pub(crate) fn share_with_minimums(
    total: u16,
    weights: &[u16],
    minimums: &[u16],
) -> Option<Vec<u16>> {
    let count = weights.len();
    let needed: u32 = minimums.iter().map(|minimum| u32::from(*minimum)).sum();
    if count == 0 || minimums.len() != count || u32::from(total) < needed {
        return None;
    }
    let sum: u32 = weights
        .iter()
        .map(|weight| u32::from((*weight).max(1)))
        .sum();
    let mut sizes: Vec<u16> = weights
        .iter()
        .map(|weight| (u32::from(total) * u32::from((*weight).max(1)) / sum) as u16)
        .collect();
    let mut remainder = total - sizes.iter().sum::<u16>();
    for size in &mut sizes {
        if remainder == 0 {
            break;
        }
        *size += 1;
        remainder -= 1;
    }
    // Lift anything under its minimum, taking from whichever share has most to spare.
    while let Some(short) = (0..count).find(|&index| sizes[index] < minimums[index]) {
        let donor = (0..count).max_by_key(|&index| sizes[index].saturating_sub(minimums[index]))?;
        let spare = sizes[donor].saturating_sub(minimums[donor]);
        let take = (minimums[short] - sizes[short]).min(spare);
        if take == 0 {
            return None;
        }
        sizes[donor] -= take;
        sizes[short] += take;
    }
    Some(sizes)
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

    fn split_of(direction: SplitDirection, panes: &[PaneId]) -> LayoutNode {
        LayoutNode::Split {
            direction,
            children: panes.iter().map(|id| LayoutNode::Pane(*id)).collect(),
            weights: vec![crate::mux::split::DEFAULT_WEIGHT; panes.len()],
        }
    }

    fn three_columns() -> LayoutNode {
        split_of(SplitDirection::Columns, &[1, 2, 3])
    }

    #[test]
    fn three_columns_tile_the_chat_area_sharing_one_border_column_per_boundary() {
        let area = Rect::new(0, 0, 120, 40);
        let layout = attached_split_layout(
            area,
            false,
            false,
            true,
            DockSizes::default(),
            &three_columns(),
            2,
        )
        .expect("three columns fit on 120");
        let chats = &layout.chats;

        assert_eq!(chats.len(), 3);
        assert_eq!(chats[0].area.x, 0);
        assert_eq!(chats[2].area.right(), 120);
        for pair in chats.windows(2) {
            assert_eq!(
                pair[0].area.right(),
                pair[1].area.x,
                "no gap and no overlap between neighbours"
            );
            assert!(
                !pair[0].borders.contains(Borders::RIGHT),
                "the boundary is drawn once, by the later slot"
            );
            assert!(pair[0].pane_area().right() < pair[1].pane_area().x);
        }
        for slot in chats {
            assert!(slot.pane_area().width >= MIN_SPLIT_COLS);
            assert_eq!(slot.area.y, layout.tabs.bottom());
        }
        // 120 columns less four border columns, shared evenly.
        let widths: Vec<u16> = chats.iter().map(|slot| slot.pane_area().width).collect();
        assert_eq!(widths.iter().sum::<u16>(), 116);
        assert!(widths.iter().max().unwrap() - widths.iter().min().unwrap() <= 1);
    }

    #[test]
    fn the_focused_chat_is_the_focused_sessions_slot_so_its_size_drives_new_panes() {
        let layout = attached_split_layout(
            Rect::new(0, 0, 120, 40),
            false,
            false,
            true,
            DockSizes::default(),
            &three_columns(),
            3,
        )
        .unwrap();
        let third = layout.chats[2];
        assert_eq!(layout.chat, third.area);
        assert_eq!(layout.chat_pane(), third.pane_area());
    }

    #[test]
    fn a_window_too_narrow_for_every_split_refuses_rather_than_squeezing_them() {
        // Three columns of 32 plus four borders need 100.
        assert!(attached_split_layout(
            Rect::new(0, 0, 99, 40),
            false,
            false,
            true,
            DockSizes::default(),
            &three_columns(),
            1
        )
        .is_none());
        assert!(attached_split_layout(
            Rect::new(0, 0, 100, 40),
            false,
            false,
            true,
            DockSizes::default(),
            &three_columns(),
            1
        )
        .is_some());
    }

    #[test]
    fn the_docks_keep_their_size_whatever_the_split_so_moving_focus_reflows_nothing() {
        let area = Rect::new(0, 0, 200, 50);
        let single = attached_layout(area, true, true, true);
        let split = attached_split_layout(
            area,
            true,
            true,
            true,
            DockSizes::default(),
            &three_columns(),
            1,
        )
        .unwrap();
        assert_eq!(split.scratchpad, single.scratchpad);
        assert_eq!(split.terminal, single.terminal);
        assert_eq!(
            split.chats.last().unwrap().area.right(),
            single.scratchpad.unwrap().x,
            "the split fills exactly the room the single chat had"
        );
    }

    #[test]
    fn stacked_splits_keep_each_top_edge_so_every_session_has_its_title() {
        let split = split_of(SplitDirection::Rows, &[1, 2, 3]);
        let layout = attached_split_layout(
            Rect::new(0, 0, 100, 50),
            false,
            false,
            true,
            DockSizes::default(),
            &split,
            1,
        )
        .unwrap();
        for pair in layout.chats.windows(2) {
            assert_eq!(pair[0].area.bottom(), pair[1].area.y);
            assert!(!pair[0].borders.contains(Borders::BOTTOM));
            assert!(pair[1].borders.contains(Borders::TOP));
        }
    }

    #[test]
    fn dragged_dock_sizes_are_used_but_the_terminal_never_takes_the_chats_last_rows() {
        let area = Rect::new(0, 0, 100, 40);
        let wide = attached_layout_sized(
            area,
            true,
            true,
            true,
            DockSizes {
                scratchpad_percent: 50,
                terminal_rows: Some(10),
            },
        );
        assert_eq!(wide.scratchpad.unwrap().width, 50);
        assert_eq!(wide.terminal.unwrap().height, 10);

        let greedy = attached_layout_sized(
            area,
            false,
            true,
            true,
            DockSizes {
                terminal_rows: Some(500),
                ..DockSizes::default()
            },
        );
        assert_eq!(greedy.chat.height, MIN_ROWS_ABOVE_TERMINAL);
    }

    #[test]
    fn docks_shrink_to_keep_a_split_on_screen_before_the_split_gives_way() {
        let area = Rect::new(0, 0, 160, 45);
        let stacked = split_of(SplitDirection::Rows, &[1, 2, 3]);
        let sizes = DockSizes::default();
        assert!(
            attached_split_layout(area, false, true, true, sizes, &stacked, 1).is_none(),
            "the terminal at its usual height leaves three rows too little room"
        );
        let fitted = docks_fitted_to_split(area, true, false, true, sizes, &stacked);
        let layout = attached_split_layout(area, false, true, true, fitted, &stacked, 1)
            .expect("a shorter terminal makes room");
        assert!(layout.terminal.unwrap().height >= MIN_TERMINAL_ROWS);

        let four = split_of(SplitDirection::Columns, &[1, 2, 3, 4]);
        assert!(attached_split_layout(area, true, false, true, sizes, &four, 1).is_none());
        let fitted = docks_fitted_to_split(area, true, true, false, sizes, &four);
        assert!(attached_split_layout(area, true, false, true, fitted, &four, 1).is_some());

        // Too small even with both docks at their minimum: the split still gives way.
        let tiny = Rect::new(0, 0, 80, 20);
        let fitted = docks_fitted_to_split(tiny, true, false, true, sizes, &stacked);
        assert!(attached_split_layout(tiny, false, true, true, fitted, &stacked, 1).is_none());
    }

    #[test]
    fn shares_add_up_exactly_and_honour_both_the_weights_and_the_minimum() {
        assert_eq!(
            share_cells(100, &[100, 100, 100], 10),
            Some(vec![34, 33, 33])
        );
        assert_eq!(share_cells(100, &[300, 100], 10), Some(vec![75, 25]));
        // A tiny weight is lifted to the minimum at the expense of the largest share.
        assert_eq!(share_cells(100, &[990, 10], 32), Some(vec![68, 32]));
        assert_eq!(share_cells(63, &[100, 100], 32), None);
    }

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

    /// Said once, in the list being filled. It used to sit in the header while the
    /// list itself said "No sessions found", which read as a contradiction.
    #[test]
    fn first_frame_says_the_list_is_loading_instead_of_that_there_are_no_sessions() {
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
        assert!(text.contains("Loading sessions…"), "got:\n{text}");
        assert!(!text.contains("No sessions found"), "got:\n{text}");
        assert_eq!(text.matches("oading").count(), 1, "said once:\n{text}");
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

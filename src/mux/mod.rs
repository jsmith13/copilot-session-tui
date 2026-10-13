pub mod callbacks;
pub mod keys;
pub mod pane;
pub mod pty;
pub mod split;

use anyhow::Result;
use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use std::path::PathBuf;
use std::sync::mpsc::{Receiver, Sender};

pub use pane::{Pane, PaneId, PaneSpec, PaneStatus};
pub use split::{LayoutNode, SplitDirection, SplitStep, Window, WindowId};

/// Events the UI loop must wake up for, from PTYs and from the terminal.
pub enum MuxEvent {
    Output(PaneId, callbacks::PaneSignals),
    Exited(PaneId, Option<u32>),
    SessionLifecycle(PaneId, crate::events::lifecycle::LifecycleEvent),
    HookLifecycle(PaneId, crate::events::hooks::HookLifecycleEvent),
    HookReadyConfirmed(PaneId, u64),
    HostSequence(PaneId, Vec<u8>),
    ConfigChanged,
    /// Notices on disk have changed and the UI thread should act on them.
    ///
    /// Carries nothing: the notices are already persisted, and only the UI thread knows
    /// which panes can take one. Sending the decision instead would mean two places
    /// holding the same list.
    ThreadNoticesChanged,
    /// The thread watcher could not do its job, with a sentence saying why.
    ThreadWatchFailed(String),
    /// A watched thread looks like it is going in circles. Reported, never acted on.
    ThreadStalled(String),
    Term(crossterm::event::Event),
}

/// A parsed prefix key such as `C-b`, `M-x` or `C-Space`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct KeyChord {
    pub code: KeyCode,
    pub modifiers: KeyModifiers,
}

impl KeyChord {
    pub fn matches(&self, key: &KeyEvent) -> bool {
        // Compare chars case-insensitively: Ctrl-b and Ctrl-B are the same chord.
        let same_code = match (self.code, key.code) {
            (KeyCode::Char(a), KeyCode::Char(b)) => a.eq_ignore_ascii_case(&b),
            (a, b) => a == b,
        };
        same_code && self.modifiers == relevant_modifiers(key.modifiers)
    }

    /// Parse `C-b`, `M-x`, `C-M-a`, `C-Space`, `F5`… Returns `None` on bad input so
    /// callers can fall back to the default rather than failing startup.
    pub fn parse(text: &str) -> Option<Self> {
        let text = text.trim();
        if text.is_empty() {
            return None;
        }

        let mut modifiers = KeyModifiers::NONE;
        let mut rest = text;
        loop {
            let lower = rest.to_ascii_lowercase();
            if lower.starts_with("c-") || lower.starts_with("ctrl-") {
                modifiers |= KeyModifiers::CONTROL;
                rest = &rest[rest.find('-')? + 1..];
            } else if lower.starts_with("m-") || lower.starts_with("alt-") {
                modifiers |= KeyModifiers::ALT;
                rest = &rest[rest.find('-')? + 1..];
            } else if lower.starts_with("s-") || lower.starts_with("shift-") {
                modifiers |= KeyModifiers::SHIFT;
                rest = &rest[rest.find('-')? + 1..];
            } else {
                break;
            }
        }

        let code = match rest.to_ascii_lowercase().as_str() {
            "space" => KeyCode::Char(' '),
            "tab" => KeyCode::Tab,
            "enter" | "return" => KeyCode::Enter,
            "esc" | "escape" => KeyCode::Esc,
            other => {
                if let Some(number) = other.strip_prefix('f') {
                    match number.parse::<u8>() {
                        Ok(n) if (1..=12).contains(&n) => KeyCode::F(n),
                        _ => return None,
                    }
                } else {
                    let mut chars = other.chars();
                    let c = chars.next()?;
                    if chars.next().is_some() {
                        return None;
                    }
                    KeyCode::Char(c)
                }
            }
        };

        // A bare character with no modifiers would swallow ordinary typing.
        if modifiers.is_empty() && matches!(code, KeyCode::Char(_)) {
            return None;
        }

        Some(Self { code, modifiers })
    }

    pub fn label(&self) -> String {
        let mut label = String::new();
        if self.modifiers.contains(KeyModifiers::CONTROL) {
            label.push_str("C-");
        }
        if self.modifiers.contains(KeyModifiers::ALT) {
            label.push_str("M-");
        }
        if self.modifiers.contains(KeyModifiers::SHIFT) {
            label.push_str("S-");
        }
        match self.code {
            KeyCode::Char(' ') => label.push_str("Space"),
            KeyCode::Char(c) => label.push(c),
            other => label.push_str(&format!("{other:?}")),
        }
        label
    }

    pub fn literal_key_event(self) -> KeyEvent {
        let mut modifiers = self.modifiers;
        let code = match self.code {
            KeyCode::Char(character) if modifiers.contains(KeyModifiers::SHIFT) => {
                modifiers.remove(KeyModifiers::SHIFT);
                KeyCode::Char(shifted_character(character))
            }
            code => code,
        };
        KeyEvent::new(code, modifiers)
    }
}

fn shifted_character(character: char) -> char {
    match character {
        'a'..='z' => character.to_ascii_uppercase(),
        '1' => '!',
        '2' => '@',
        '3' => '#',
        '4' => '$',
        '5' => '%',
        '6' => '^',
        '7' => '&',
        '8' => '*',
        '9' => '(',
        '0' => ')',
        '-' => '_',
        '=' => '+',
        '[' => '{',
        ']' => '}',
        '\\' => '|',
        ';' => ':',
        '\'' => '"',
        ',' => '<',
        '.' => '>',
        '/' => '?',
        '`' => '~',
        character => character,
    }
}

/// Ignore modifiers that terminals report inconsistently (e.g. KEYPAD/SUPER on Windows).
fn relevant_modifiers(modifiers: KeyModifiers) -> KeyModifiers {
    modifiers & (KeyModifiers::CONTROL | KeyModifiers::ALT | KeyModifiers::SHIFT)
}

/// What the prefix key sequence resolved to.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PrefixCommand {
    Detach,
    NextPane,
    PreviousPane,
    KillPane,
    EndPersistentSession,
    PaneList,
    Chat,
    Scratchpad,
    Terminal,
    Snippets,
    Update,
    Help,
    Github,
    /// `prefix q` — quit CST and close its pane clients.
    Quit,
    /// `prefix m` — enter the sticky mode that slides the focused tab.
    MoveTab,
    SelectIndex(usize),
    /// `prefix l` — the layout menu; see [`LayoutCommand`].
    Layout,
    /// `prefix` then an arrow — move to the neighbouring split. Kept at the top level
    /// rather than in the layout menu because it is by far the most frequent.
    FocusSplit(SplitStep),
    // What the layout menu's keys resolve to; never produced by the top level.
    /// Pick a session to show beside or below the focused one.
    Split(SplitDirection),
    /// Give the focused split the whole screen, or put the split back.
    ZoomSplit,
    /// Take the focused session out of the split, keeping its tab.
    UnsplitFocused,
    /// Enter the sticky mode that resizes the focused split.
    ResizeSplit,
    /// Give every split the same share again.
    EqualizeSplits,
    /// `prefix prefix` — search every CST command.
    CommandPalette,
    Cancel,
}

#[cfg(test)]
pub fn resolve_prefix_command(key: &KeyEvent, prefix: &KeyChord) -> Option<PrefixCommand> {
    let tmux_keys = crate::config::TmuxKeyConfig::default();
    resolve_prefix_command_with_tmux_keys(key, prefix, Some(&tmux_keys))
}

/// Every character the built-in match below claims. The configured tmux end key is
/// validated against this and only consulted after the built-ins, so drift here makes
/// a configured key inert — it can never steal a multiplexer command.
pub(crate) const PREFIX_COMMAND_KEYS: &[char] = &[
    'd', 'n', 'p', 'x', 'w', 'c', 'e', 't', 's', 'u', 'q', 'm', 'h', 'g', 'l', '0', '1', '2', '3',
    '4', '5', '6', '7', '8', '9',
];

/// `tmux_keys` is `None` when tmux-backed sessions cannot work here, which keeps the
/// configured end key as inert after the prefix as it would be on a build without
/// the feature.
pub fn resolve_prefix_command_with_tmux_keys(
    key: &KeyEvent,
    prefix: &KeyChord,
    tmux_keys: Option<&crate::config::TmuxKeyConfig>,
) -> Option<PrefixCommand> {
    if prefix.matches(key) {
        return Some(PrefixCommand::CommandPalette);
    }
    let builtin = match key.code {
        KeyCode::Char('d') => Some(PrefixCommand::Detach),
        KeyCode::Char('n') => Some(PrefixCommand::NextPane),
        KeyCode::Char('p') => Some(PrefixCommand::PreviousPane),
        KeyCode::Char('x') => Some(PrefixCommand::KillPane),
        KeyCode::Char('w') => Some(PrefixCommand::PaneList),
        KeyCode::Char('c') => Some(PrefixCommand::Chat),
        KeyCode::Char('e') => Some(PrefixCommand::Scratchpad),
        KeyCode::Char('t') => Some(PrefixCommand::Terminal),
        KeyCode::Char('s') => Some(PrefixCommand::Snippets),
        KeyCode::Char('u') => Some(PrefixCommand::Update),
        KeyCode::Char('q') => Some(PrefixCommand::Quit),
        KeyCode::Char('m') => Some(PrefixCommand::MoveTab),
        KeyCode::Char('h') => Some(PrefixCommand::Help),
        KeyCode::Char('g') => Some(PrefixCommand::Github),
        KeyCode::Char('l') => Some(PrefixCommand::Layout),
        KeyCode::Left => Some(PrefixCommand::FocusSplit(SplitStep::Left)),
        KeyCode::Right => Some(PrefixCommand::FocusSplit(SplitStep::Right)),
        KeyCode::Up => Some(PrefixCommand::FocusSplit(SplitStep::Up)),
        KeyCode::Down => Some(PrefixCommand::FocusSplit(SplitStep::Down)),
        KeyCode::Char(character)
            if character.eq_ignore_ascii_case(&'h')
                && key.modifiers.contains(KeyModifiers::CONTROL) =>
        {
            Some(PrefixCommand::Help)
        }
        KeyCode::Char(character)
            if character.eq_ignore_ascii_case(&'g')
                && key.modifiers.contains(KeyModifiers::CONTROL) =>
        {
            Some(PrefixCommand::Github)
        }
        KeyCode::Backspace => Some(PrefixCommand::Help),
        KeyCode::Char(c) if c.is_ascii_digit() => {
            Some(PrefixCommand::SelectIndex(c.to_digit(10)? as usize))
        }
        KeyCode::Esc => Some(PrefixCommand::Cancel),
        _ => None,
    };
    if builtin.is_some() {
        return builtin;
    }
    // Only a key no built-in claims can reach the configured tmux end shortcut, so a
    // bad configuration is at worst inert.
    if tmux_keys.is_some_and(|keys| keys.matches_end_session(key.code)) {
        return Some(PrefixCommand::EndPersistentSession);
    }
    None
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HelpCommand {
    Scratchpad,
    Cancel,
}

pub fn resolve_help_command(key: &KeyEvent) -> Option<HelpCommand> {
    match key.code {
        KeyCode::Char('e') => Some(HelpCommand::Scratchpad),
        KeyCode::Esc => Some(HelpCommand::Cancel),
        _ => None,
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum GithubCommand {
    Inspect,
    /// The thread messages waiting on the user.
    Messages,
    Cancel,
}

pub fn resolve_github_command(key: &KeyEvent) -> Option<GithubCommand> {
    match key.code {
        KeyCode::Char('i') => Some(GithubCommand::Inspect),
        KeyCode::Char('m') => Some(GithubCommand::Messages),
        KeyCode::Esc => Some(GithubCommand::Cancel),
        _ => None,
    }
}

/// What the layout menu offers, for the status bar while it waits for a key.
pub const LAYOUT_HINT: &str =
    "v beside · s below · z zoom · d remove · r resize · = equal · Esc cancel";

/// `prefix l`: everything about splits except moving between them, grouped so the
/// top level of the prefix stays small. The letters follow Doom Emacs's window keys;
/// the tmux-style `|` and `-` work too.
pub fn resolve_layout_command(key: &KeyEvent) -> Option<PrefixCommand> {
    match key.code {
        KeyCode::Char('v' | '|') => Some(PrefixCommand::Split(SplitDirection::Columns)),
        KeyCode::Char('s' | '-') => Some(PrefixCommand::Split(SplitDirection::Rows)),
        KeyCode::Char('z') => Some(PrefixCommand::ZoomSplit),
        KeyCode::Char('d') => Some(PrefixCommand::UnsplitFocused),
        KeyCode::Char('r') => Some(PrefixCommand::ResizeSplit),
        KeyCode::Char('=') => Some(PrefixCommand::EqualizeSplits),
        _ => None,
    }
}

/// A sticky sub-mode entered from the prefix menu.
///
/// Unlike the one-shot `Help` and `Github` menus, a transient mode survives the key
/// that acts on it, so a repeated adjustment costs one keystroke instead of three.
/// Every key goes to CST while one is active, because the capture gate already
/// claims everything when `PrefixState` is not `Idle`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TransientMode {
    /// Left/Right slide the focused tab along the strip.
    MoveTab,
    /// The arrows shrink and grow the focused split.
    ResizeSplit,
}

impl TransientMode {
    /// Badge shown in the status bar while the mode is active.
    pub fn badge(self) -> &'static str {
        match self {
            Self::MoveTab => " Move tab ",
            Self::ResizeSplit => " Resize ",
        }
    }

    pub fn hint(self) -> &'static str {
        match self {
            Self::MoveTab => " ←/→ move  Esc done ",
            Self::ResizeSplit => " ←/→ narrower/wider  ↑/↓ shorter/taller  Esc done ",
        }
    }
}

/// What a key means inside a transient mode.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TransientCommand {
    /// Act in the mode's own direction; `true` is rightwards/forwards.
    Step(bool),
    /// Resize along this axis; `true` grows the focused split.
    Resize(SplitDirection, bool),
    Leave,
    /// Not ours — leave the mode and let the key be handled normally.
    Passthrough,
}

pub fn resolve_transient_command(mode: TransientMode, key: &KeyEvent) -> TransientCommand {
    // The vim-style letters are a convenience for the bare keys only. Ctrl+L clears a
    // screen and Ctrl+H is a backspace; a mode that swallowed those to nudge a tab
    // sideways would be surprising, and the prefix chord has to stay reachable so
    // there is always a way back to the command menu.
    let plain = !key
        .modifiers
        .intersects(KeyModifiers::CONTROL | KeyModifiers::ALT);
    match mode {
        TransientMode::MoveTab => match key.code {
            KeyCode::Left => TransientCommand::Step(false),
            KeyCode::Right => TransientCommand::Step(true),
            KeyCode::Char('h') if plain => TransientCommand::Step(false),
            KeyCode::Char('l') if plain => TransientCommand::Step(true),
            KeyCode::Esc | KeyCode::Enter => TransientCommand::Leave,
            _ => TransientCommand::Passthrough,
        },
        // Both axes, so the same keys work whichever way the split runs.
        TransientMode::ResizeSplit => match key.code {
            // Each pair of arrows works on its own axis, so in a column holding a
            // stack the left and right arrows widen it and the up and down arrows
            // trade height inside it.
            KeyCode::Left => TransientCommand::Resize(SplitDirection::Columns, false),
            KeyCode::Right => TransientCommand::Resize(SplitDirection::Columns, true),
            KeyCode::Up => TransientCommand::Resize(SplitDirection::Rows, false),
            KeyCode::Down => TransientCommand::Resize(SplitDirection::Rows, true),
            KeyCode::Char('h') if plain => TransientCommand::Resize(SplitDirection::Columns, false),
            KeyCode::Char('l') if plain => TransientCommand::Resize(SplitDirection::Columns, true),
            KeyCode::Char('k') if plain => TransientCommand::Resize(SplitDirection::Rows, false),
            KeyCode::Char('j') if plain => TransientCommand::Resize(SplitDirection::Rows, true),
            KeyCode::Esc | KeyCode::Enter => TransientCommand::Leave,
            _ => TransientCommand::Passthrough,
        },
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PrefixState {
    Idle,
    Root,
    Help,
    Github,
    /// `prefix l`, waiting for a key from [`resolve_layout_command`].
    Layout,
    /// A sticky mode; see [`TransientMode`].
    Transient(TransientMode),
}

/// All panes owned by this CST instance.
pub struct MuxState {
    /// Every session, kept in tab order and, within a tab, in reading order, so that
    /// anything listing them meets them in the order the screen shows.
    pub panes: Vec<Pane>,
    /// The session with the keyboard. The focused tab is whichever one holds it; no
    /// second record of that exists to disagree with this one.
    pub focused: Option<PaneId>,
    /// The tabs, in strip order. Every session is in exactly one.
    pub windows: Vec<Window>,
    next_window_id: WindowId,
    pub prefix: KeyChord,
    pub prefix_state: PrefixState,
    next_id: PaneId,
    pub events: Sender<MuxEvent>,
    pub receiver: Receiver<MuxEvent>,
}

impl MuxState {
    pub fn new(prefix: KeyChord) -> Self {
        let (events, receiver) = std::sync::mpsc::channel();
        Self {
            panes: Vec::new(),
            focused: None,
            windows: Vec::new(),
            next_window_id: 1,
            prefix,
            prefix_state: PrefixState::Idle,
            next_id: 1,
            events,
            receiver,
        }
    }

    pub fn allocate_id(&mut self) -> PaneId {
        let id = self.next_id;
        self.next_id += 1;
        id
    }

    pub fn pane(&self, id: PaneId) -> Option<&Pane> {
        self.panes.iter().find(|pane| pane.id == id)
    }

    pub fn pane_mut(&mut self, id: PaneId) -> Option<&mut Pane> {
        self.panes.iter_mut().find(|pane| pane.id == id)
    }

    pub fn focused_pane(&self) -> Option<&Pane> {
        self.focused.and_then(|id| self.pane(id))
    }

    pub fn focused_pane_mut(&mut self) -> Option<&mut Pane> {
        let id = self.focused?;
        self.pane_mut(id)
    }

    pub fn window_index_of(&self, id: PaneId) -> Option<usize> {
        self.windows
            .iter()
            .position(|window| window.layout.contains(id))
    }

    pub fn window_of(&self, id: PaneId) -> Option<&Window> {
        self.windows
            .iter()
            .find(|window| window.layout.contains(id))
    }

    /// The tab on screen: the one holding the focused session.
    pub fn current_window(&self) -> Option<&Window> {
        self.window_of(self.focused?)
    }

    fn current_window_mut(&mut self) -> Option<&mut Window> {
        let focused = self.focused?;
        self.windows
            .iter_mut()
            .find(|window| window.layout.contains(focused))
    }

    /// The layout to draw: the current tab's, unless it holds one session or one of
    /// them is zoomed, in which case only the focused session is drawn.
    pub fn visible_layout(&self) -> Option<&LayoutNode> {
        self.current_window()
            .filter(|window| window.is_split() && !window.zoomed)
            .map(|window| &window.layout)
    }

    /// Whether the current tab holds more than one session, zoomed or not.
    pub fn in_split(&self) -> bool {
        self.current_window().is_some_and(Window::is_split)
    }

    pub fn zoomed(&self) -> bool {
        self.current_window()
            .is_some_and(|window| window.is_split() && window.zoomed)
    }

    /// The number this session's tab is labelled with, counting from 1 as the strip does.
    pub fn tab_number(&self, id: PaneId) -> Option<usize> {
        self.window_index_of(id).map(|index| index + 1)
    }

    /// Where a session sits among those sharing its tab, from 1, or `None` when it
    /// has the tab to itself. Splits are titled with this: several sessions started in
    /// one project all carry its name, and the tab number is the same for all of them.
    pub fn number_in_window(&self, id: PaneId) -> Option<usize> {
        let window = self.window_of(id).filter(|window| window.is_split())?;
        window
            .layout
            .panes()
            .iter()
            .position(|pane| *pane == id)
            .map(|index| index + 1)
    }

    /// Existing pane for a Copilot session id, so Enter re-focuses instead of duplicating.
    pub fn pane_for_session(&self, session_id: &str) -> Option<PaneId> {
        self.panes
            .iter()
            .find(|pane| pane.session_id == session_id)
            .map(|pane| pane.id)
    }

    /// A new session, in a tab of its own at the end of the strip.
    pub fn push(&mut self, pane: Pane) -> PaneId {
        let id = pane.id;
        self.panes.push(pane);
        let window = self.new_window(id);
        self.windows.push(window);
        self.focus(id);
        id
    }

    fn new_window(&mut self, pane: PaneId) -> Window {
        let id = self.next_window_id;
        self.next_window_id += 1;
        Window::single(id, pane)
    }

    /// Give `id` the keyboard, which brings its tab on screen.
    pub fn focus(&mut self, id: PaneId) {
        if self.pane(id).is_none() {
            return;
        }
        if let Some(window) = self
            .windows
            .iter_mut()
            .find(|window| window.layout.contains(id))
        {
            window.last_focused = id;
        }
        self.focused = Some(id);
    }

    pub fn remove(&mut self, id: PaneId) {
        let Some(index) = self.panes.iter().position(|pane| pane.id == id) else {
            return;
        };
        let pane = self.panes.remove(index);
        let _ = pane.kill();
        let was_focused = self.focused == Some(id);
        let mut successor = None;
        if let Some(window_index) = self.window_index_of(id) {
            let window = &mut self.windows[window_index];
            match window.layout.remove(id) {
                // Others share the tab: the one that closes over the gap takes over.
                Some(next) => {
                    if window.last_focused == id {
                        window.last_focused = next;
                    }
                    // The zoom was on the session that left. Staying zoomed would put
                    // a different one full screen in its place, so the split comes
                    // back instead, as tmux does when a zoomed pane closes.
                    if was_focused {
                        window.zoomed = false;
                    }
                    successor = Some(window.last_focused);
                }
                // It had the tab to itself, so the tab goes and the next one along
                // comes on screen, as closing a tab always has.
                None => {
                    self.windows.remove(window_index);
                    successor = self
                        .windows
                        .get(window_index)
                        .or_else(|| self.windows.last())
                        .map(|window| window.last_focused);
                }
            }
        }
        if was_focused {
            self.focused = successor;
        }
        self.sort_panes();
    }

    pub fn cycle(&mut self, forward: bool) {
        let len = self.windows.len();
        if len < 2 {
            return;
        }
        let Some(current) = self.focused.and_then(|id| self.window_index_of(id)) else {
            let first = self.windows[0].last_focused;
            self.focus(first);
            return;
        };
        let next = if forward {
            (current + 1) % len
        } else {
            (current + len - 1) % len
        };
        let target = self.windows[next].last_focused;
        self.focus(target);
    }

    /// Bring tab `index` (from 0) on screen.
    pub fn select_index(&mut self, index: usize) {
        if let Some(target) = self.windows.get(index).map(|window| window.last_focused) {
            self.focus(target);
        }
    }

    /// Bring `id` into the current tab beside the focused session, and focus it.
    ///
    /// The session leaves the tab it was in, which closes if it is left empty — tmux's
    /// join-pane. Splitting the way the focused session's own split already runs adds
    /// to it; splitting across nests, which is how a column comes to hold a stack.
    pub fn split_with(&mut self, direction: SplitDirection, id: PaneId) -> bool {
        let Some(focused) = self.focused else {
            return false;
        };
        if id == focused || self.pane(id).is_none() {
            return false;
        }
        let (Some(target), Some(source)) =
            (self.window_index_of(focused), self.window_index_of(id))
        else {
            return false;
        };
        if target == source {
            return false;
        }
        if self.windows[source].layout.remove(id).is_none() {
            // It had its tab to itself.
            self.windows.remove(source);
        } else {
            let window = &mut self.windows[source];
            if window.last_focused == id {
                window.last_focused = window.layout.first_pane();
            }
        }
        if let Some(window) = self.current_window_mut() {
            window.layout.insert_beside(focused, id, direction);
            window.zoomed = false;
        }
        self.focus(id);
        self.sort_panes();
        true
    }

    /// Move the focused session out into a tab of its own, placed right after this
    /// one; focus stays in this tab on the session that closes over the gap. Returns
    /// false when it already has a tab to itself.
    pub fn unsplit_focused(&mut self) -> bool {
        let Some(focused) = self.focused else {
            return false;
        };
        let Some(index) = self.window_index_of(focused) else {
            return false;
        };
        let Some(next) = self.windows[index].layout.remove(focused) else {
            return false;
        };
        let window = &mut self.windows[index];
        window.zoomed = false;
        window.last_focused = next;
        let broken_out = self.new_window(focused);
        self.windows.insert(index + 1, broken_out);
        self.focused = Some(next);
        self.sort_panes();
        true
    }

    /// Put `new` — just opened in a tab of its own — exactly where `old` is, and close
    /// `old`. Restarting a session that died in a split brings it back in its place
    /// rather than in a new tab at the end of the strip.
    pub fn replace_in_place(&mut self, old: PaneId, new: PaneId) -> bool {
        if old == new || self.pane(old).is_none() || self.pane(new).is_none() {
            return false;
        }
        let Some(own) = self.window_index_of(new) else {
            return false;
        };
        if self.windows[own].is_split() {
            return false;
        }
        self.windows.remove(own);
        if let Some(window) = self
            .windows
            .iter_mut()
            .find(|window| window.layout.contains(old))
        {
            window.layout.replace(old, new);
            if window.last_focused == old {
                window.last_focused = new;
            }
        }
        if let Some(index) = self.panes.iter().position(|pane| pane.id == old) {
            let _ = self.panes.remove(index).kill();
        }
        self.focus(new);
        self.sort_panes();
        true
    }

    /// Change the direction of the split the focused session sits directly in.
    pub fn turn_split(&mut self, direction: SplitDirection) -> bool {
        let Some(focused) = self.focused else {
            return false;
        };
        self.current_window_mut().is_some_and(|window| {
            let turned = window.layout.turn_parent(focused, direction);
            if turned {
                window.zoomed = false;
            }
            turned
        })
    }

    /// Undo any resizing in this tab: every split gets equal shares again.
    pub fn equalize_split(&mut self) -> bool {
        match self.current_window_mut().filter(|window| window.is_split()) {
            Some(window) => {
                window.layout.equalize();
                true
            }
            None => false,
        }
    }

    pub fn toggle_split_zoom(&mut self) -> bool {
        match self.current_window_mut().filter(|window| window.is_split()) {
            Some(window) => {
                window.zoomed = !window.zoomed;
                true
            }
            None => false,
        }
    }

    /// The current tab's layout, for resizing it.
    pub fn current_layout_mut(&mut self) -> Option<&mut LayoutNode> {
        self.current_window_mut().map(|window| &mut window.layout)
    }

    /// Every tab with more than one session, by session id, for the update restart.
    /// Tabs of one need nothing saying: that is what a reopened session gets anyway.
    pub fn save_windows(&self) -> Vec<split::SavedWindow> {
        let session_of = |id: PaneId| self.pane(id).map(|pane| pane.session_id.clone());
        self.windows
            .iter()
            .filter(|window| window.is_split())
            .filter_map(|window| {
                Some(split::SavedWindow {
                    layout: window.layout.save(&session_of)?,
                    zoomed: window.zoomed,
                    focused_session_id: session_of(window.last_focused),
                })
            })
            .collect()
    }

    /// Regroup reopened sessions into the tabs they were saved in. Each saved tab
    /// takes the place of its first session's tab; sessions that did not come back
    /// are left out, and a tab left with one session is simply that session's tab.
    pub fn restore_windows(&mut self, saved: &[split::SavedWindow]) {
        for saved_window in saved {
            let pane_of = |session: &str| self.pane_for_session(session);
            let Some(layout) = saved_window.layout.restore(&pane_of) else {
                continue;
            };
            let members = layout.panes();
            if members.len() < 2 {
                continue;
            }
            let Some(position) = self
                .windows
                .iter()
                .position(|window| window.layout.contains(members[0]))
            else {
                continue;
            };
            let last_focused = saved_window
                .focused_session_id
                .as_deref()
                .and_then(|session| self.pane_for_session(session))
                .filter(|id| members.contains(id))
                .unwrap_or(members[0]);
            let id = self.windows[position].id;
            self.windows[position] = Window {
                id,
                layout,
                zoomed: saved_window.zoomed,
                last_focused,
            };
            for member in &members[1..] {
                if let Some(index) = self.windows.iter().enumerate().position(|(index, window)| {
                    index != position && window.layout == LayoutNode::Pane(*member)
                }) {
                    self.windows.remove(index);
                }
            }
        }
        self.sort_panes();
    }

    /// Moves the tab holding `id` to `index`, keeping every other tab in its order.
    ///
    /// Returns whether anything moved, so a caller can tell a no-op at the end of
    /// the strip apart from a real reorder and leave the user a hint either way.
    pub fn move_pane_to(&mut self, id: PaneId, index: usize) -> bool {
        let Some(from) = self.window_index_of(id) else {
            return false;
        };
        let to = index.min(self.windows.len().saturating_sub(1));
        if from == to {
            return false;
        }
        let window = self.windows.remove(from);
        self.windows.insert(to, window);
        self.sort_panes();
        true
    }

    /// Moves the focused tab one place along the strip. Deliberately stops at the
    /// ends rather than wrapping: dragging a tab off one edge and having it appear
    /// at the other is disorienting, and `cycle` already exists for going around.
    pub fn move_focused_pane(&mut self, forward: bool) -> bool {
        let Some(from) = self.focused.and_then(|id| self.window_index_of(id)) else {
            return false;
        };
        let to = if forward {
            from + 1
        } else {
            match from.checked_sub(1) {
                Some(to) => to,
                None => return false,
            }
        };
        if to >= self.windows.len() {
            return false;
        }
        self.windows.swap(from, to);
        self.sort_panes();
        true
    }

    /// Put `panes` back in tab order, then reading order within each tab.
    fn sort_panes(&mut self) {
        let order: Vec<PaneId> = self
            .windows
            .iter()
            .flat_map(|window| window.layout.panes())
            .collect();
        self.panes.sort_by_key(|pane| {
            order
                .iter()
                .position(|id| *id == pane.id)
                .unwrap_or(usize::MAX)
        });
    }

    pub fn running_count(&self) -> usize {
        self.panes.iter().filter(|pane| pane.is_running()).count()
    }

    /// Catch exits whose notification never reached the event loop, so a quit
    /// confirmation never lists sessions that are already gone.
    pub fn reap(&mut self) {
        for pane in &mut self.panes {
            pane.poll_exit();
        }
    }

    /// Directory of the focused pane, used for the shell auto-`cd` on exit.
    pub fn focused_cwd(&self) -> Option<PathBuf> {
        self.focused_pane().map(|pane| pane.cwd.clone())
    }

    #[cfg(test)]
    pub fn resize_all_at(&mut self, x: u16, y: u16, rows: u16, cols: u16) {
        for pane in &mut self.panes {
            let _ = pane.resize_at(x, y, rows, cols);
        }
    }

    pub fn shutdown(&mut self) -> Result<()> {
        self.shutdown_with_report().map(|_| ())
    }

    pub fn shutdown_with_report(&mut self) -> Result<Vec<PaneId>> {
        let mut failures = Vec::new();
        let mut terminated = Vec::new();
        for pane in &mut self.panes {
            match pane.shutdown() {
                Ok(true) => terminated.push(pane.id),
                Ok(false) => {}
                Err(error) => failures.push(format!("'{}': {error}", pane.title)),
            }
        }
        if !failures.is_empty() {
            anyhow::bail!("Could not end all sessions: {}", failures.join("; "));
        }
        self.panes.clear();
        self.focused = None;
        self.windows.clear();
        Ok(terminated)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn key(code: KeyCode, modifiers: KeyModifiers) -> KeyEvent {
        KeyEvent::new(code, modifiers)
    }

    /// A multiplexer holding real, idle panes `1..=count`, focused on the first.
    fn mux_with_panes(count: PaneId) -> MuxState {
        let mut mux = MuxState::new(KeyChord::parse("C-b").unwrap());
        for id in 1..=count {
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
                    title: format!("Session {id}"),
                    cwd: std::env::temp_dir(),
                    session_id: format!("session-{id}"),
                    program,
                    args,
                    events_path: None,
                    terminal_light_mode: Some(false),
                    hooks_active: false,
                },
                24,
                80,
                mux.events.clone(),
            )
            .unwrap();
            mux.push(pane);
        }
        mux.focus(1);
        mux
    }

    fn tabs(mux: &MuxState) -> Vec<Vec<PaneId>> {
        mux.windows
            .iter()
            .map(|window| window.layout.panes())
            .collect()
    }

    /// The reason for windows: before them, starting a session while a split was on
    /// screen put it in the focused split's place.
    #[test]
    fn a_new_session_opens_in_a_tab_of_its_own_whatever_is_on_screen() {
        let mut mux = mux_with_panes(2);
        mux.split_with(SplitDirection::Columns, 2);
        assert_eq!(tabs(&mux), vec![vec![1, 2]]);

        let _ = mux.shutdown();
        let mut mux = mux_with_panes(3);
        mux.split_with(SplitDirection::Columns, 2);
        assert_eq!(tabs(&mux), vec![vec![1, 2], vec![3]]);
        mux.focus(3);
        assert_eq!(
            tabs(&mux),
            vec![vec![1, 2], vec![3]],
            "focusing another tab's session switches tabs and leaves the split alone"
        );
        let _ = mux.shutdown();
    }

    #[test]
    fn splitting_pulls_a_session_out_of_its_tab_and_a_tab_left_empty_closes() {
        let mut mux = mux_with_panes(3);
        assert!(mux.split_with(SplitDirection::Columns, 3));
        assert_eq!(tabs(&mux), vec![vec![1, 3], vec![2]]);
        assert_eq!(mux.focused, Some(3));
        assert!(
            !mux.split_with(SplitDirection::Columns, 1),
            "already in this tab"
        );
        assert_eq!(
            mux.panes.iter().map(|pane| pane.id).collect::<Vec<_>>(),
            vec![1, 3, 2]
        );
        let _ = mux.shutdown();
    }

    #[test]
    fn splitting_across_the_focused_sessions_split_nests_so_a_column_can_hold_a_stack() {
        let mut mux = mux_with_panes(3);
        mux.split_with(SplitDirection::Columns, 2);
        mux.split_with(SplitDirection::Rows, 3);
        let LayoutNode::Split {
            direction,
            children,
            ..
        } = &mux.windows[0].layout
        else {
            panic!("a split");
        };
        assert_eq!(*direction, SplitDirection::Columns);
        assert_eq!(children[0], LayoutNode::Pane(1));
        assert_eq!(children[1].panes(), vec![2, 3]);
        let _ = mux.shutdown();
    }

    #[test]
    fn ending_a_split_session_hands_focus_to_the_one_that_closes_over_it() {
        let mut mux = mux_with_panes(3);
        mux.split_with(SplitDirection::Columns, 2);
        mux.split_with(SplitDirection::Columns, 3);
        mux.focus(2);
        mux.remove(2);
        assert_eq!(tabs(&mux), vec![vec![1, 3]]);
        assert_eq!(mux.focused, Some(3));

        mux.remove(3);
        assert_eq!(tabs(&mux), vec![vec![1]]);
        assert_eq!(mux.focused, Some(1));
        let _ = mux.shutdown();
    }

    #[test]
    fn taking_a_session_out_gives_it_the_next_tab_and_keeps_the_keyboard_here() {
        let mut mux = mux_with_panes(4);
        mux.split_with(SplitDirection::Rows, 2);
        mux.split_with(SplitDirection::Rows, 3);
        mux.focus(2);

        assert!(mux.unsplit_focused());
        assert_eq!(tabs(&mux), vec![vec![1, 3], vec![2], vec![4]]);
        assert_eq!(mux.focused, Some(3));

        assert!(mux.unsplit_focused());
        assert_eq!(tabs(&mux), vec![vec![1], vec![3], vec![2], vec![4]]);
        assert!(
            !mux.unsplit_focused(),
            "a tab of one has nothing to take out"
        );
        let _ = mux.shutdown();
    }

    #[test]
    fn a_restarted_session_takes_the_dead_ones_place_in_its_split() {
        let mut mux = mux_with_panes(4);
        mux.split_with(SplitDirection::Columns, 2);
        mux.split_with(SplitDirection::Rows, 3);
        // 4 stands in for the restarted session, opened in a tab of its own.
        mux.focus(4);
        assert!(mux.replace_in_place(2, 4));
        assert_eq!(tabs(&mux), vec![vec![1, 4, 3]]);
        assert!(mux.pane(2).is_none());
        assert_eq!(mux.focused, Some(4));
        let _ = mux.shutdown();
    }

    #[test]
    fn tabs_are_cycled_and_numbered_whole_and_return_to_whoever_had_focus() {
        let mut mux = mux_with_panes(3);
        mux.split_with(SplitDirection::Columns, 2);
        mux.focus(1);
        assert_eq!(mux.tab_number(2), Some(1));
        assert_eq!(mux.tab_number(3), Some(2));
        assert_eq!(mux.number_in_window(2), Some(2));
        assert_eq!(mux.number_in_window(3), None);

        mux.cycle(true);
        assert_eq!(mux.focused, Some(3));
        mux.select_index(0);
        assert_eq!(mux.focused, Some(1), "the session last focused in that tab");
        let _ = mux.shutdown();
    }

    #[test]
    fn closing_the_zoomed_session_brings_the_split_back_instead_of_zooming_another() {
        let mut mux = mux_with_panes(4);
        mux.split_with(SplitDirection::Columns, 2);
        mux.split_with(SplitDirection::Columns, 3);
        mux.toggle_split_zoom();
        mux.remove(3);
        assert!(
            mux.visible_layout().is_some(),
            "1 and 2 are back side by side"
        );

        // A session leaving that was not the zoomed one leaves the zoom alone.
        mux.split_with(SplitDirection::Columns, 4);
        mux.focus(1);
        mux.toggle_split_zoom();
        mux.remove(4);
        assert!(mux.zoomed());

        // Taking the zoomed session out of the split brings the rest back too.
        mux.toggle_split_zoom();
        mux.focus(2);
        mux.toggle_split_zoom();
        mux.unsplit_focused();
        assert!(!mux.zoomed());
        let _ = mux.shutdown();
    }

    #[test]
    fn a_saved_tab_comes_back_by_session_even_though_pane_ids_change() {
        let mut before = mux_with_panes(3);
        before.split_with(SplitDirection::Columns, 3);
        before.split_with(SplitDirection::Rows, 2);
        before.focus(3);
        let saved = before.save_windows();
        assert_eq!(
            saved.len(),
            1,
            "only tabs holding several sessions need saving"
        );
        assert_eq!(
            saved[0].layout.sessions(),
            vec!["session-1", "session-3", "session-2"]
        );
        let _ = before.shutdown();

        let mut after = mux_with_panes(3);
        after.restore_windows(&saved);
        assert_eq!(tabs(&after), vec![vec![1, 3, 2]]);
        assert_eq!(after.windows[0].last_focused, 3);
        let _ = after.shutdown();
    }

    #[test]
    fn split_keys_live_under_l_except_the_arrows_which_stay_one_key_away() {
        let chord = KeyChord::parse("C-b").unwrap();
        let none = KeyModifiers::NONE;
        assert_eq!(
            resolve_prefix_command(&key(KeyCode::Char('l'), none), &chord),
            Some(PrefixCommand::Layout)
        );
        assert!(PREFIX_COMMAND_KEYS.contains(&'l'));
        assert_eq!(
            resolve_prefix_command(&key(KeyCode::Left, none), &chord),
            Some(PrefixCommand::FocusSplit(SplitStep::Left))
        );
        // The old top-level keys are free again for the configurable tmux end key.
        for free in ['|', '-', 'z', 'b', 'r'] {
            assert_eq!(
                resolve_prefix_command(&key(KeyCode::Char(free), none), &chord),
                None,
                "{free}"
            );
            assert!(!PREFIX_COMMAND_KEYS.contains(&free), "{free}");
        }

        let cases = [
            ('v', PrefixCommand::Split(SplitDirection::Columns)),
            ('|', PrefixCommand::Split(SplitDirection::Columns)),
            ('s', PrefixCommand::Split(SplitDirection::Rows)),
            ('-', PrefixCommand::Split(SplitDirection::Rows)),
            ('z', PrefixCommand::ZoomSplit),
            ('d', PrefixCommand::UnsplitFocused),
            ('r', PrefixCommand::ResizeSplit),
            ('=', PrefixCommand::EqualizeSplits),
        ];
        for (character, command) in cases {
            assert_eq!(
                resolve_layout_command(&key(KeyCode::Char(character), none)),
                Some(command),
                "{character}"
            );
        }
        // `|` usually needs Shift; the terminal reporting it must not stop it matching.
        assert_eq!(
            resolve_layout_command(&key(KeyCode::Char('|'), KeyModifiers::SHIFT)),
            Some(PrefixCommand::Split(SplitDirection::Columns))
        );
        assert_eq!(resolve_layout_command(&key(KeyCode::Esc, none)), None);
    }

    #[test]
    fn parses_common_chord_spellings() {
        let ctrl_b = KeyChord::parse("C-b").unwrap();
        assert_eq!(ctrl_b.code, KeyCode::Char('b'));
        assert_eq!(ctrl_b.modifiers, KeyModifiers::CONTROL);

        assert_eq!(KeyChord::parse("ctrl-b"), Some(ctrl_b));
        assert_eq!(KeyChord::parse("C-B"), KeyChord::parse("C-b"));
        assert_eq!(KeyChord::parse("C-Space").unwrap().code, KeyCode::Char(' '));
        assert_eq!(KeyChord::parse("M-x").unwrap().modifiers, KeyModifiers::ALT);
        assert_eq!(KeyChord::parse("F5").unwrap().code, KeyCode::F(5));
    }

    #[test]
    fn literal_shifted_character_chords_preserve_the_shifted_character() {
        let shifted = KeyChord::parse("S-x").unwrap().literal_key_event();
        assert_eq!(shifted.code, KeyCode::Char('X'));
        assert!(!shifted.modifiers.contains(KeyModifiers::SHIFT));

        let alt_shifted = KeyChord::parse("M-S-1").unwrap().literal_key_event();
        assert_eq!(alt_shifted.code, KeyCode::Char('!'));
        assert_eq!(alt_shifted.modifiers, KeyModifiers::ALT);
    }

    #[test]
    fn rejects_chords_that_would_swallow_typing() {
        assert!(KeyChord::parse("b").is_none());
        assert!(KeyChord::parse("").is_none());
        assert!(KeyChord::parse("C-").is_none());
        assert!(KeyChord::parse("C-nope").is_none());
        assert!(KeyChord::parse("F42").is_none());
    }

    #[test]
    fn matching_ignores_case_and_irrelevant_modifiers() {
        let chord = KeyChord::parse("C-b").unwrap();
        assert!(chord.matches(&key(KeyCode::Char('b'), KeyModifiers::CONTROL)));
        assert!(chord.matches(&key(KeyCode::Char('B'), KeyModifiers::CONTROL)));
        assert!(!chord.matches(&key(KeyCode::Char('b'), KeyModifiers::NONE)));
        assert!(!chord.matches(&key(
            KeyCode::Char('b'),
            KeyModifiers::CONTROL | KeyModifiers::ALT
        )));
    }

    #[test]
    fn label_round_trips() {
        for text in ["C-b", "M-x", "C-Space"] {
            let chord = KeyChord::parse(text).unwrap();
            assert_eq!(KeyChord::parse(&chord.label()), Some(chord));
        }
    }

    #[test]
    fn double_prefix_sends_a_literal() {
        let chord = KeyChord::parse("C-b").unwrap();
        let command =
            resolve_prefix_command(&key(KeyCode::Char('b'), KeyModifiers::CONTROL), &chord);
        assert_eq!(command, Some(PrefixCommand::CommandPalette));
    }

    #[test]
    fn prefix_commands_map_to_actions() {
        let chord = KeyChord::parse("C-b").unwrap();
        let none = KeyModifiers::NONE;
        assert_eq!(
            resolve_prefix_command(&key(KeyCode::Char('d'), none), &chord),
            Some(PrefixCommand::Detach)
        );
        assert_eq!(
            resolve_prefix_command(&key(KeyCode::Char('x'), none), &chord),
            Some(PrefixCommand::KillPane)
        );
        assert_eq!(
            resolve_prefix_command(&key(KeyCode::Char('c'), none), &chord),
            Some(PrefixCommand::Chat)
        );
        assert_eq!(
            resolve_prefix_command(&key(KeyCode::Char('e'), none), &chord),
            Some(PrefixCommand::Scratchpad)
        );
        assert_eq!(
            resolve_prefix_command(&key(KeyCode::Char('t'), none), &chord),
            Some(PrefixCommand::Terminal)
        );
        assert_eq!(
            resolve_prefix_command(&key(KeyCode::Char('s'), none), &chord),
            Some(PrefixCommand::Snippets)
        );
        assert_eq!(
            resolve_prefix_command(&key(KeyCode::Char('m'), none), &chord),
            Some(PrefixCommand::MoveTab)
        );
        assert_eq!(
            resolve_prefix_command(&key(KeyCode::Char('M'), KeyModifiers::SHIFT), &chord),
            None
        );
        assert_eq!(
            resolve_prefix_command(&key(KeyCode::Char('u'), none), &chord),
            Some(PrefixCommand::Update)
        );
        assert_eq!(
            resolve_prefix_command(&key(KeyCode::Char('h'), KeyModifiers::CONTROL), &chord),
            Some(PrefixCommand::Help)
        );
        assert_eq!(
            resolve_prefix_command(&key(KeyCode::Char('g'), KeyModifiers::CONTROL), &chord),
            Some(PrefixCommand::Github)
        );
        assert_eq!(
            resolve_prefix_command(&key(KeyCode::Char('h'), none), &chord),
            Some(PrefixCommand::Help)
        );
        assert_eq!(
            resolve_prefix_command(&key(KeyCode::Char('g'), none), &chord),
            Some(PrefixCommand::Github)
        );
        assert_eq!(
            resolve_prefix_command(&key(KeyCode::Char('q'), none), &chord),
            Some(PrefixCommand::Quit)
        );
        assert_eq!(
            resolve_prefix_command(&key(KeyCode::Backspace, none), &chord),
            Some(PrefixCommand::Help)
        );
        assert_eq!(
            resolve_prefix_command(&key(KeyCode::Char('2'), none), &chord),
            Some(PrefixCommand::SelectIndex(2))
        );
        assert_eq!(
            resolve_prefix_command(&key(KeyCode::Char('y'), none), &chord),
            None
        );
    }

    #[test]
    fn persistent_end_prefix_follows_configured_key_without_shadowing_move_mode() {
        let chord = KeyChord::parse("C-b").unwrap();
        let keys = crate::config::TmuxKeyConfig {
            resume_session: "t".to_string(),
            new_session: "v".to_string(),
            new_worktree: "V".to_string(),
            end_session: "!".to_string(),
        };

        assert_eq!(
            resolve_prefix_command_with_tmux_keys(
                &key(KeyCode::Char('v'), KeyModifiers::NONE),
                &chord,
                Some(&keys),
            ),
            None
        );
        assert_eq!(
            resolve_prefix_command_with_tmux_keys(
                &key(KeyCode::Char('V'), KeyModifiers::SHIFT),
                &chord,
                Some(&keys),
            ),
            None
        );
        assert_eq!(
            resolve_prefix_command_with_tmux_keys(
                &key(KeyCode::Char('!'), KeyModifiers::SHIFT),
                &chord,
                Some(&keys),
            ),
            Some(PrefixCommand::EndPersistentSession)
        );
        assert_eq!(
            resolve_prefix_command_with_tmux_keys(
                &key(KeyCode::Char('m'), KeyModifiers::NONE),
                &chord,
                Some(&keys),
            ),
            Some(PrefixCommand::MoveTab)
        );
    }

    #[test]
    fn end_prefix_stays_inert_where_tmux_sessions_cannot_work() {
        let chord = KeyChord::parse("C-b").unwrap();

        assert_eq!(
            resolve_prefix_command_with_tmux_keys(
                &key(KeyCode::Char('X'), KeyModifiers::SHIFT),
                &chord,
                None,
            ),
            None
        );
    }

    #[test]
    fn group_namespaces_remain_reachable_when_control_key_is_the_prefix() {
        let prefix = KeyChord::parse("C-g").unwrap();
        assert_eq!(
            resolve_prefix_command(&key(KeyCode::Char('g'), KeyModifiers::CONTROL), &prefix),
            Some(PrefixCommand::CommandPalette)
        );
        assert_eq!(
            resolve_prefix_command(&key(KeyCode::Char('g'), KeyModifiers::NONE), &prefix),
            Some(PrefixCommand::Github)
        );
        assert_eq!(
            resolve_prefix_command(&key(KeyCode::Char('h'), KeyModifiers::NONE), &prefix),
            Some(PrefixCommand::Help)
        );
    }

    #[test]
    fn help_commands_map_to_topics() {
        assert_eq!(
            resolve_help_command(&key(KeyCode::Char('e'), KeyModifiers::NONE)),
            Some(HelpCommand::Scratchpad)
        );
        assert_eq!(
            resolve_help_command(&key(KeyCode::Esc, KeyModifiers::NONE)),
            Some(HelpCommand::Cancel)
        );
    }

    #[test]
    fn github_commands_map_to_inspection_and_waiting_messages() {
        assert_eq!(
            resolve_github_command(&key(KeyCode::Char('i'), KeyModifiers::NONE)),
            Some(GithubCommand::Inspect)
        );
        assert_eq!(
            resolve_github_command(&key(KeyCode::Char('m'), KeyModifiers::NONE)),
            Some(GithubCommand::Messages)
        );
        assert_eq!(
            resolve_github_command(&key(KeyCode::Esc, KeyModifiers::NONE)),
            Some(GithubCommand::Cancel)
        );
    }
}

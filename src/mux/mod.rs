pub mod callbacks;
pub mod keys;
pub mod pane;
pub mod pty;

use anyhow::Result;
use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use std::path::PathBuf;
use std::sync::mpsc::{Receiver, Sender};

pub use pane::{Pane, PaneId, PaneSpec, PaneStatus};

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
    'd', 'n', 'p', 'x', 'w', 'c', 'e', 't', 's', 'u', 'q', 'm', 'h', 'g', '0', '1', '2', '3', '4',
    '5', '6', '7', '8', '9',
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
    Cancel,
}

pub fn resolve_github_command(key: &KeyEvent) -> Option<GithubCommand> {
    match key.code {
        KeyCode::Char('i') => Some(GithubCommand::Inspect),
        KeyCode::Esc => Some(GithubCommand::Cancel),
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
}

impl TransientMode {
    /// Badge shown in the status bar while the mode is active.
    pub fn badge(self) -> &'static str {
        match self {
            Self::MoveTab => " Move tab ",
        }
    }

    pub fn hint(self) -> &'static str {
        match self {
            Self::MoveTab => " ←/→ move  Esc done ",
        }
    }
}

/// What a key means inside a transient mode.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TransientCommand {
    /// Act in the mode's own direction; `true` is rightwards/forwards.
    Step(bool),
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
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PrefixState {
    Idle,
    Root,
    Help,
    Github,
    /// A sticky mode; see [`TransientMode`].
    Transient(TransientMode),
}

/// All panes owned by this CST instance.
pub struct MuxState {
    pub panes: Vec<Pane>,
    pub focused: Option<PaneId>,
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

    /// Existing pane for a Copilot session id, so Enter re-focuses instead of duplicating.
    pub fn pane_for_session(&self, session_id: &str) -> Option<PaneId> {
        self.panes
            .iter()
            .find(|pane| pane.session_id == session_id)
            .map(|pane| pane.id)
    }

    pub fn push(&mut self, pane: Pane) -> PaneId {
        let id = pane.id;
        self.panes.push(pane);
        self.focused = Some(id);
        id
    }

    pub fn remove(&mut self, id: PaneId) {
        if let Some(index) = self.panes.iter().position(|pane| pane.id == id) {
            let pane = self.panes.remove(index);
            let _ = pane.kill();
            if self.focused == Some(id) {
                self.focused = self
                    .panes
                    .get(index)
                    .or_else(|| self.panes.last())
                    .map(|pane| pane.id);
            }
        }
    }

    pub fn cycle(&mut self, forward: bool) {
        if self.panes.len() < 2 {
            return;
        }
        let Some(current) = self
            .focused
            .and_then(|id| self.panes.iter().position(|pane| pane.id == id))
        else {
            self.focused = self.panes.first().map(|pane| pane.id);
            return;
        };
        let len = self.panes.len();
        let next = if forward {
            (current + 1) % len
        } else {
            (current + len - 1) % len
        };
        self.focused = Some(self.panes[next].id);
    }

    pub fn select_index(&mut self, index: usize) {
        if let Some(pane) = self.panes.get(index) {
            self.focused = Some(pane.id);
        }
    }

    /// Moves `id` to `index`, keeping every other pane in its relative order.
    ///
    /// Returns whether anything moved, so a caller can tell a no-op at the end of
    /// the strip apart from a real reorder and leave the user a hint either way.
    pub fn move_pane_to(&mut self, id: PaneId, index: usize) -> bool {
        let Some(from) = self.panes.iter().position(|pane| pane.id == id) else {
            return false;
        };
        let to = index.min(self.panes.len().saturating_sub(1));
        if from == to {
            return false;
        }
        let pane = self.panes.remove(from);
        self.panes.insert(to, pane);
        true
    }

    /// Moves the focused pane one slot along the strip. Deliberately stops at the
    /// ends rather than wrapping: dragging a tab off one edge and having it appear
    /// at the other is disorienting, and `cycle` already exists for going around.
    pub fn move_focused_pane(&mut self, forward: bool) -> bool {
        let Some(id) = self.focused else {
            return false;
        };
        let Some(from) = self.panes.iter().position(|pane| pane.id == id) else {
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
        if to >= self.panes.len() {
            return false;
        }
        self.panes.swap(from, to);
        true
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

    pub fn resize_all(&mut self, rows: u16, cols: u16) {
        for pane in &mut self.panes {
            let _ = pane.resize(rows, cols);
        }
    }

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
        Ok(terminated)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn key(code: KeyCode, modifiers: KeyModifiers) -> KeyEvent {
        KeyEvent::new(code, modifiers)
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
            resolve_prefix_command(&key(KeyCode::Char('z'), none), &chord),
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
    fn github_commands_map_to_inspection() {
        assert_eq!(
            resolve_github_command(&key(KeyCode::Char('i'), KeyModifiers::NONE)),
            Some(GithubCommand::Inspect)
        );
        assert_eq!(
            resolve_github_command(&key(KeyCode::Esc, KeyModifiers::NONE)),
            Some(GithubCommand::Cancel)
        );
    }
}

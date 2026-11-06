use crate::config::TerminalConfig;
use crate::mux::keys::{encode_mouse, encode_paste};
use anyhow::{Context, Result};
use crossterm::event::{
    Event, KeyCode, KeyEvent, KeyEventKind, KeyModifiers, MouseEvent, MouseEventKind,
};
use portable_pty::{native_pty_system, Child, CommandBuilder, MasterPty, PtySize};
use std::collections::HashMap;
use std::io::{Read, Write};
use std::path::Path;
use std::sync::{mpsc, Arc, Mutex, RwLock};
use std::time::{Duration, Instant};

const INITIAL_ROWS: u16 = 12;
const INITIAL_COLS: u16 = 80;
const SCROLLBACK_ROWS: usize = 10_000;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct Viewport {
    x: u16,
    y: u16,
    rows: u16,
    cols: u16,
}

impl Viewport {
    fn coordinates(self, column: u16, row: u16) -> Option<(u16, u16)> {
        let relative_column = column.checked_sub(self.x)?;
        let relative_row = row.checked_sub(self.y)?;
        if relative_column >= self.cols || relative_row >= self.rows {
            return None;
        }
        Some((relative_column + 1, relative_row + 1))
    }

    fn clamped_coordinates(self, column: u16, row: u16) -> (u16, u16) {
        let max_column = self.x.saturating_add(self.cols.saturating_sub(1));
        let max_row = self.y.saturating_add(self.rows.saturating_sub(1));
        (
            column.clamp(self.x, max_column) - self.x + 1,
            row.clamp(self.y, max_row) - self.y + 1,
        )
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TerminalStatus {
    Running,
    Exited(u32),
    Failed(String),
}

impl std::fmt::Display for TerminalStatus {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Running => write!(formatter, "running"),
            Self::Exited(code) => write!(formatter, "exited ({code})"),
            Self::Failed(error) => write!(formatter, "error: {error}"),
        }
    }
}

struct TerminalProcess {
    parser: Arc<RwLock<TerminalParser>>,
    master: Option<Box<dyn MasterPty + Send>>,
    input: Option<mpsc::Sender<Vec<u8>>>,
    child: Arc<Mutex<Box<dyn Child + Send + Sync>>>,
    status: Arc<Mutex<TerminalStatus>>,
    host_sequences: mpsc::Receiver<Vec<u8>>,
    size: PtySize,
}

type TerminalParser = vt100::Parser<TerminalCallbacks>;

pub(crate) struct TerminalCallbacks {
    input: mpsc::Sender<Vec<u8>>,
    host_sequences: mpsc::Sender<Vec<u8>>,
}

impl vt100::Callbacks for TerminalCallbacks {
    fn copy_to_clipboard(&mut self, _screen: &mut vt100::Screen, selector: &[u8], data: &[u8]) {
        let mut sequence = Vec::with_capacity(selector.len() + data.len() + 10);
        sequence.extend_from_slice(b"\x1b]52;");
        sequence.extend_from_slice(selector);
        sequence.push(b';');
        sequence.extend_from_slice(data);
        sequence.extend_from_slice(b"\x1b\\");
        let _ = self.host_sequences.send(sequence);
    }

    fn unhandled_osc(&mut self, _screen: &mut vt100::Screen, params: &[&[u8]]) {
        if let Some(sequence) = crate::host_terminal::progress_sequence(params) {
            let _ = self.host_sequences.send(sequence);
        }
    }

    fn unhandled_csi(
        &mut self,
        screen: &mut vt100::Screen,
        first_intermediate: Option<u8>,
        second_intermediate: Option<u8>,
        params: &[&[u16]],
        command: char,
    ) {
        if second_intermediate.is_some() {
            return;
        }

        let response = match (first_intermediate, params, command) {
            (None, [[5]], 'n') => Some(b"\x1b[0n".to_vec()),
            (None, [[6]], 'n') => {
                let (row, column) = screen.cursor_position();
                Some(format!("\x1b[{};{}R", row + 1, column + 1).into_bytes())
            }
            (Some(b'?'), [[6]], 'n') => {
                let (row, column) = screen.cursor_position();
                Some(format!("\x1b[?{};{}R", row + 1, column + 1).into_bytes())
            }
            (None, [] | [[0]], 'c') => Some(b"\x1b[?1;2c".to_vec()),
            _ => None,
        };
        if let Some(response) = response {
            let _ = self.input.send(response);
        }
    }
}

pub struct TerminalPane {
    pub session_name: String,
    pub cwd: String,
    generation: u64,
    process: TerminalProcess,
    viewport: Viewport,
    mouse_captured: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Activation {
    Opened,
    Focused,
    Restarted,
}

#[derive(Default)]
pub struct TerminalManager {
    panes: HashMap<String, TerminalPane>,
    active_id: Option<String>,
    visible: bool,
    focused: bool,
    next_generation: u64,
}

impl TerminalProcess {
    fn spawn(cwd: &Path, config: &TerminalConfig) -> Result<Self> {
        if !cwd.is_dir() {
            anyhow::bail!("Session directory does not exist: {}", cwd.display());
        }

        let size = PtySize {
            rows: INITIAL_ROWS,
            cols: INITIAL_COLS,
            pixel_width: 0,
            pixel_height: 0,
        };
        let pair = native_pty_system()
            .openpty(size)
            .context("Failed to create pseudo-terminal")?;

        let mut command = match config.shell.as_deref().map(str::trim) {
            Some(shell) if !shell.is_empty() => CommandBuilder::new(shell),
            _ => CommandBuilder::new_default_prog(),
        };
        command.cwd(cwd);

        let child = pair
            .slave
            .spawn_command(command)
            .context("Failed to start terminal shell")?;
        let child = Arc::new(Mutex::new(child));
        drop(pair.slave);

        let mut reader = pair
            .master
            .try_clone_reader()
            .context("Failed to open terminal output")?;
        let mut writer = pair
            .master
            .take_writer()
            .context("Failed to open terminal input")?;
        let (input, receiver) = mpsc::channel::<Vec<u8>>();
        let (host_sequence_sender, host_sequences) = mpsc::channel::<Vec<u8>>();
        let parser = Arc::new(RwLock::new(vt100::Parser::new_with_callbacks(
            size.rows,
            size.cols,
            SCROLLBACK_ROWS,
            TerminalCallbacks {
                input: input.clone(),
                host_sequences: host_sequence_sender,
            },
        )));
        let status = Arc::new(Mutex::new(TerminalStatus::Running));

        {
            let parser = Arc::clone(&parser);
            let status = Arc::clone(&status);
            std::thread::spawn(move || {
                let mut buffer = [0_u8; 8192];
                loop {
                    match reader.read(&mut buffer) {
                        Ok(0) => break,
                        Ok(read) => match parser.write() {
                            Ok(mut parser) => parser.process(&buffer[..read]),
                            Err(_) => {
                                set_failed(&status, "terminal parser lock was poisoned");
                                break;
                            }
                        },
                        Err(error) => {
                            set_failed(&status, format!("terminal read failed: {error}"));
                            break;
                        }
                    }
                }
            });
        }

        {
            let status = Arc::clone(&status);
            std::thread::spawn(move || {
                while let Ok(bytes) = receiver.recv() {
                    if let Err(error) = writer.write_all(&bytes).and_then(|_| writer.flush()) {
                        set_failed(&status, format!("terminal write failed: {error}"));
                        break;
                    }
                }
            });
        }

        Ok(Self {
            parser,
            master: Some(pair.master),
            input: Some(input),
            child,
            status,
            host_sequences,
            size,
        })
    }

    fn status(&self) -> TerminalStatus {
        let current = self.status.lock().map_or_else(
            |_| TerminalStatus::Failed("terminal status lock was poisoned".to_string()),
            |status| status.clone(),
        );
        if !matches!(current, TerminalStatus::Running) {
            return current;
        }
        let next = match self.child.lock() {
            Ok(mut child) => match child.try_wait() {
                Ok(Some(exit)) => Some(TerminalStatus::Exited(exit.exit_code())),
                Ok(None) => None,
                Err(error) => Some(TerminalStatus::Failed(format!(
                    "shell status check failed: {error}"
                ))),
            },
            Err(_) => Some(TerminalStatus::Failed(
                "terminal child lock was poisoned".to_string(),
            )),
        };
        if let Some(next) = next {
            if let Ok(mut status) = self.status.lock() {
                *status = next.clone();
            }
            next
        } else {
            current
        }
    }

    fn send(&self, bytes: Vec<u8>) -> Result<()> {
        if !matches!(self.status(), TerminalStatus::Running) {
            anyhow::bail!("Shell is not running");
        }
        self.input
            .as_ref()
            .context("Terminal input is closed")?
            .send(bytes)
            .context("Terminal input is closed")
    }

    fn resize(&mut self, rows: u16, cols: u16) -> Result<()> {
        let size = PtySize {
            rows: rows.max(1),
            cols: cols.max(1),
            pixel_width: 0,
            pixel_height: 0,
        };
        if size == self.size {
            return Ok(());
        }
        self.master
            .as_ref()
            .context("Pseudo-terminal is closed")?
            .resize(size)
            .context("Failed to resize pseudo-terminal")?;
        self.parser
            .write()
            .map_err(|_| anyhow::anyhow!("Terminal parser lock was poisoned"))?
            .screen_mut()
            .set_size(size.rows, size.cols);
        self.size = size;
        Ok(())
    }

    fn drain_host_sequences(&self, sequences: &mut Vec<Vec<u8>>) {
        sequences.extend(self.host_sequences.try_iter());
    }

    fn shutdown(&mut self, timeout: Duration) -> Result<()> {
        let terminated = match self.terminate_process() {
            Ok(terminated) => terminated,
            Err(error) => {
                if let Some(code) = self.try_wait_child()? {
                    self.record_exit(code);
                    false
                } else {
                    return Err(error);
                }
            }
        };
        if !terminated {
            self.input.take();
            self.master.take();
            return Ok(());
        }
        let deadline = Instant::now() + timeout;
        loop {
            if let Some(code) = self.try_wait_child()? {
                self.record_exit(code);
                self.input.take();
                self.master.take();
                return Ok(());
            }
            if Instant::now() >= deadline {
                anyhow::bail!(
                    "terminal shell did not exit within {:.1}s",
                    timeout.as_secs_f32()
                );
            }
            std::thread::sleep(Duration::from_millis(20));
        }
    }

    fn try_wait_child(&self) -> Result<Option<u32>> {
        self.child
            .lock()
            .map_err(|_| anyhow::anyhow!("Terminal child handle is unavailable"))?
            .try_wait()
            .context("Failed to check terminal shell status")
            .map(|status| status.map(|status| status.exit_code()))
    }

    fn child_is_running(&self) -> bool {
        match self.try_wait_child() {
            Ok(Some(code)) => {
                self.record_exit(code);
                false
            }
            Ok(None) | Err(_) => true,
        }
    }

    fn record_exit(&self, code: u32) {
        if let Ok(mut status) = self.status.lock() {
            *status = TerminalStatus::Exited(code);
        }
    }

    #[cfg(windows)]
    fn terminate_process(&self) -> Result<bool> {
        use windows_sys::Win32::System::Threading::TerminateProcess;

        let mut child = self
            .child
            .lock()
            .map_err(|_| anyhow::anyhow!("Terminal child handle is unavailable"))?;
        if let Some(status) = child.try_wait()? {
            self.record_exit(status.exit_code());
            return Ok(false);
        }
        let handle = child
            .as_raw_handle()
            .context("Terminal shell has no Windows handle")?;
        // SAFETY: `handle` belongs to the child protected by the mutex guard.
        if unsafe { TerminateProcess(handle.cast(), 1) } == 0 {
            return Err(std::io::Error::last_os_error())
                .context("Failed to terminate terminal shell");
        }
        Ok(true)
    }

    #[cfg(not(windows))]
    fn terminate_process(&self) -> Result<bool> {
        let mut child = self
            .child
            .lock()
            .map_err(|_| anyhow::anyhow!("Terminal child handle is unavailable"))?;
        if let Some(status) = child.try_wait()? {
            self.record_exit(status.exit_code());
            return Ok(false);
        }
        child.kill().context("Failed to terminate terminal shell")?;
        Ok(true)
    }
}

impl Drop for TerminalProcess {
    fn drop(&mut self) {
        let _ = self.shutdown(Duration::from_secs(3));
    }
}

impl TerminalPane {
    pub fn open(
        session_name: String,
        cwd: String,
        generation: u64,
        config: &TerminalConfig,
    ) -> Result<Self> {
        let process = TerminalProcess::spawn(Path::new(&cwd), config)?;
        Ok(Self {
            session_name,
            cwd,
            generation,
            process,
            viewport: Viewport {
                x: 0,
                y: 0,
                rows: INITIAL_ROWS,
                cols: INITIAL_COLS,
            },
            mouse_captured: false,
        })
    }

    pub fn restart(&mut self, generation: u64, config: &TerminalConfig) -> Result<()> {
        let process = TerminalProcess::spawn(Path::new(&self.cwd), config)?;
        self.process = process;
        self.generation = generation;
        self.mouse_captured = false;
        Ok(())
    }

    #[cfg(test)]
    pub fn status(&self) -> TerminalStatus {
        self.process.status()
    }

    pub fn is_running(&self) -> bool {
        self.process.child_is_running()
    }

    pub(crate) fn parser(&self) -> &RwLock<TerminalParser> {
        &self.process.parser
    }

    pub fn resize(&mut self, x: u16, y: u16, rows: u16, cols: u16) -> Result<()> {
        let rows = rows.max(1);
        let cols = cols.max(1);
        self.process.resize(rows, cols)?;
        self.viewport = Viewport { x, y, rows, cols };
        Ok(())
    }

    pub fn handle_event(&mut self, event: Event) -> Result<()> {
        match event {
            Event::Key(key) if key.kind == KeyEventKind::Press => {
                let application_cursor = self
                    .process
                    .parser
                    .read()
                    .map_err(|_| anyhow::anyhow!("Terminal parser lock was poisoned"))?
                    .screen()
                    .application_cursor();
                if let Some(bytes) = encode_key(key, application_cursor) {
                    self.process.send(bytes)?;
                }
            }
            Event::Paste(text) => {
                let bracketed = self
                    .process
                    .parser
                    .read()
                    .map_err(|_| anyhow::anyhow!("Terminal parser lock was poisoned"))?
                    .screen()
                    .bracketed_paste();
                self.process.send(encode_paste(&text, bracketed))?;
            }
            Event::Mouse(mouse) => self.handle_mouse(mouse)?,
            _ => {}
        }
        Ok(())
    }

    fn handle_mouse(&mut self, event: MouseEvent) -> Result<()> {
        let (mode, encoding) = {
            let parser = self
                .process
                .parser
                .read()
                .map_err(|_| anyhow::anyhow!("Terminal parser lock was poisoned"))?;
            (
                parser.screen().mouse_protocol_mode(),
                parser.screen().mouse_protocol_encoding(),
            )
        };

        if mode == vt100::MouseProtocolMode::None {
            if self.viewport.coordinates(event.column, event.row).is_some() {
                match event.kind {
                    MouseEventKind::ScrollUp => self.scroll(3)?,
                    MouseEventKind::ScrollDown => self.scroll(-3)?,
                    _ => {}
                }
            }
            return Ok(());
        }

        let coordinates = self.viewport.coordinates(event.column, event.row);
        if matches!(event.kind, MouseEventKind::Down(_)) && coordinates.is_some() {
            self.mouse_captured = true;
        }
        let coordinates = coordinates.or_else(|| {
            (self.mouse_captured
                && matches!(event.kind, MouseEventKind::Drag(_) | MouseEventKind::Up(_)))
            .then(|| self.viewport.clamped_coordinates(event.column, event.row))
        });
        let sequence =
            coordinates.and_then(|coordinates| encode_mouse(event, coordinates, mode, encoding));
        if matches!(event.kind, MouseEventKind::Up(_)) {
            self.mouse_captured = false;
        }
        if let Some(sequence) = sequence {
            self.process.send(sequence)?;
        }
        Ok(())
    }

    fn scroll(&mut self, rows: isize) -> Result<()> {
        let mut parser = self
            .process
            .parser
            .write()
            .map_err(|_| anyhow::anyhow!("Terminal parser lock was poisoned"))?;
        let current = parser.screen().scrollback();
        let next = if rows.is_negative() {
            current.saturating_sub(rows.unsigned_abs())
        } else {
            current.saturating_add(rows as usize)
        };
        parser.screen_mut().set_scrollback(next);
        Ok(())
    }

    #[cfg(test)]
    fn contents(&self) -> String {
        self.process.parser.read().unwrap().screen().contents()
    }
}

impl TerminalManager {
    fn allocate_generation(&mut self) -> u64 {
        self.next_generation = self.next_generation.saturating_add(1);
        self.next_generation
    }

    pub fn activate(
        &mut self,
        session_id: String,
        session_name: String,
        cwd: String,
        config: &TerminalConfig,
    ) -> Result<Activation> {
        let needs_generation = self
            .panes
            .get(&session_id)
            .is_none_or(|pane| !pane.is_running());
        let generation = needs_generation.then(|| self.allocate_generation());
        let activation = if let Some(pane) = self.panes.get_mut(&session_id) {
            pane.session_name = session_name;
            pane.cwd = cwd;
            if pane.is_running() {
                Activation::Focused
            } else {
                pane.restart(
                    generation.expect("stopped panes receive a generation"),
                    config,
                )?;
                Activation::Restarted
            }
        } else {
            let pane = TerminalPane::open(
                session_name,
                cwd,
                generation.expect("new panes receive a generation"),
                config,
            )?;
            self.panes.insert(session_id.clone(), pane);
            Activation::Opened
        };
        self.active_id = Some(session_id);
        self.visible = true;
        self.focused = true;
        Ok(activation)
    }

    pub fn active(&self) -> Option<&TerminalPane> {
        self.active_id
            .as_ref()
            .and_then(|session_id| self.panes.get(session_id))
    }

    pub fn active_mut(&mut self) -> Option<&mut TerminalPane> {
        self.active_id
            .as_ref()
            .and_then(|session_id| self.panes.get_mut(session_id))
    }

    pub fn active_session_id(&self) -> Option<&str> {
        self.active_id.as_deref()
    }

    pub fn stopped_session_ids(&self) -> Vec<String> {
        self.panes
            .iter()
            .filter(|(_, pane)| !pane.is_running())
            .map(|(session_id, _)| session_id.clone())
            .collect()
    }

    pub fn running_sessions(&self) -> Vec<(String, String, String, u64)> {
        self.panes
            .iter()
            .filter(|(_, pane)| pane.is_running())
            .map(|(session_id, pane)| {
                (
                    session_id.clone(),
                    pane.cwd.clone(),
                    pane.session_name.clone(),
                    pane.generation,
                )
            })
            .collect()
    }

    #[cfg(test)]
    pub fn exit_active_for_test(&mut self) -> Result<()> {
        self.active_mut()
            .context("No active terminal")?
            .process
            .send(if cfg!(windows) {
                b"exit\r\n".to_vec()
            } else {
                b"exit\n".to_vec()
            })
    }

    pub fn is_visible(&self) -> bool {
        self.visible && self.active().is_some()
    }

    pub fn hide(&mut self) {
        self.visible = false;
        self.focused = false;
    }

    pub fn unfocus(&mut self) {
        self.focused = false;
    }

    pub fn focus(&mut self) {
        if self.is_visible() {
            self.focused = true;
        }
    }

    pub fn remove(&mut self, session_id: &str) -> Result<()> {
        if let Some(pane) = self.panes.get_mut(session_id) {
            pane.process.shutdown(Duration::from_secs(3))?;
        }
        self.panes.remove(session_id);
        if self.active_id.as_deref() == Some(session_id) {
            self.active_id = None;
            self.visible = false;
            self.focused = false;
        }
        Ok(())
    }

    pub fn shutdown(&mut self) -> Result<()> {
        let mut failures = Vec::new();
        for (session_id, pane) in &mut self.panes {
            if let Err(error) = pane.process.shutdown(Duration::from_secs(3)) {
                failures.push(format!("'{}': {error}", session_id));
            }
        }
        if !failures.is_empty() {
            anyhow::bail!("Could not end all terminal shells: {}", failures.join("; "));
        }
        self.panes.clear();
        self.active_id = None;
        self.visible = false;
        self.focused = false;
        Ok(())
    }

    pub fn drain_host_sequences(&self) -> Vec<Vec<u8>> {
        let mut sequences = Vec::new();
        for pane in self.panes.values() {
            pane.process.drain_host_sequences(&mut sequences);
        }
        sequences
    }
}

fn set_failed(status: &Mutex<TerminalStatus>, error: impl Into<String>) {
    if let Ok(mut status) = status.lock() {
        if matches!(*status, TerminalStatus::Running) {
            *status = TerminalStatus::Failed(error.into());
        }
    }
}

fn encode_key(key: KeyEvent, application_cursor: bool) -> Option<Vec<u8>> {
    let mut bytes = match key.code {
        KeyCode::Char(character) => {
            if key.modifiers.contains(KeyModifiers::CONTROL) {
                control_byte(character).map(|byte| vec![byte])?
            } else {
                character.to_string().into_bytes()
            }
        }
        KeyCode::Enter => {
            #[cfg(windows)]
            {
                vec![b'\r', b'\n']
            }
            #[cfg(not(windows))]
            {
                vec![b'\r']
            }
        }
        KeyCode::Backspace => vec![0x7f],
        KeyCode::Tab => vec![b'\t'],
        KeyCode::BackTab => b"\x1b[Z".to_vec(),
        KeyCode::Esc => vec![0x1b],
        KeyCode::Up => cursor_sequence(b'A', key.modifiers, application_cursor),
        KeyCode::Down => cursor_sequence(b'B', key.modifiers, application_cursor),
        KeyCode::Right => cursor_sequence(b'C', key.modifiers, application_cursor),
        KeyCode::Left => cursor_sequence(b'D', key.modifiers, application_cursor),
        KeyCode::Home => cursor_sequence(b'H', key.modifiers, application_cursor),
        KeyCode::End => cursor_sequence(b'F', key.modifiers, application_cursor),
        KeyCode::PageUp => modified_tilde_sequence(5, key.modifiers),
        KeyCode::PageDown => modified_tilde_sequence(6, key.modifiers),
        KeyCode::Insert => modified_tilde_sequence(2, key.modifiers),
        KeyCode::Delete => modified_tilde_sequence(3, key.modifiers),
        KeyCode::F(number) => function_key_sequence(number, key.modifiers)?,
        _ => return None,
    };

    if key
        .modifiers
        .intersects(KeyModifiers::ALT | KeyModifiers::META | KeyModifiers::SUPER)
        && matches!(
            key.code,
            KeyCode::Char(_) | KeyCode::Enter | KeyCode::Backspace | KeyCode::Tab | KeyCode::Esc
        )
    {
        bytes.insert(0, 0x1b);
    }
    Some(bytes)
}

fn control_byte(character: char) -> Option<u8> {
    let upper = character.to_ascii_uppercase();
    if ('@'..='_').contains(&upper) {
        Some(upper as u8 - b'@')
    } else if upper == '?' {
        Some(0x7f)
    } else {
        None
    }
}

fn modifier_parameter(modifiers: KeyModifiers) -> u8 {
    1 + u8::from(modifiers.contains(KeyModifiers::SHIFT))
        + 2 * u8::from(
            modifiers.intersects(KeyModifiers::ALT | KeyModifiers::META | KeyModifiers::SUPER),
        )
        + 4 * u8::from(modifiers.contains(KeyModifiers::CONTROL))
}

fn cursor_sequence(final_byte: u8, modifiers: KeyModifiers, application: bool) -> Vec<u8> {
    let parameter = modifier_parameter(modifiers);
    if parameter > 1 {
        format!("\x1b[1;{parameter}{}", final_byte as char).into_bytes()
    } else if application {
        vec![0x1b, b'O', final_byte]
    } else {
        vec![0x1b, b'[', final_byte]
    }
}

fn modified_tilde_sequence(code: u8, modifiers: KeyModifiers) -> Vec<u8> {
    let parameter = modifier_parameter(modifiers);
    if parameter > 1 {
        format!("\x1b[{code};{parameter}~").into_bytes()
    } else {
        format!("\x1b[{code}~").into_bytes()
    }
}

fn function_key_sequence(number: u8, modifiers: KeyModifiers) -> Option<Vec<u8>> {
    let code = match number {
        1..=4 => {
            let final_byte = b'P' + number - 1;
            return Some(cursor_sequence(final_byte, modifiers, true));
        }
        5 => 15,
        6 => 17,
        7 => 18,
        8 => 19,
        9 => 20,
        10 => 21,
        11 => 23,
        12 => 24,
        _ => return None,
    };
    Some(modified_tilde_sequence(code, modifiers))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crossterm::event::MouseButton;
    use std::time::{Duration, Instant};

    #[test]
    fn key_encoding_preserves_shell_control_keys() {
        let control_c = KeyEvent::new(KeyCode::Char('c'), KeyModifiers::CONTROL);
        let control_v = KeyEvent::new(KeyCode::Char('v'), KeyModifiers::CONTROL);
        let control_z = KeyEvent::new(KeyCode::Char('z'), KeyModifiers::CONTROL);
        let alt_x = KeyEvent::new(KeyCode::Char('x'), KeyModifiers::ALT);
        let meta_v = KeyEvent::new(KeyCode::Char('v'), KeyModifiers::META);
        let shifted_up = KeyEvent::new(KeyCode::Up, KeyModifiers::SHIFT);

        assert_eq!(encode_key(control_c, false), Some(vec![3]));
        assert_eq!(encode_key(control_v, false), Some(vec![0x16]));
        assert_eq!(encode_key(control_z, false), Some(vec![0x1a]));
        assert_eq!(encode_key(alt_x, false), Some(b"\x1bx".to_vec()));
        assert_eq!(encode_key(meta_v, false), Some(b"\x1bv".to_vec()));
        assert_eq!(encode_key(shifted_up, false), Some(b"\x1b[1;2A".to_vec()));
    }

    #[test]
    fn image_and_text_paste_sequences_are_preserved() {
        assert_eq!(encode_paste("", false), vec![0x16]);
        assert_eq!(encode_paste("text", false), b"text");
        assert_eq!(encode_paste("text", true), b"\x1b[200~text\x1b[201~");
    }

    #[test]
    fn viewport_translates_outer_mouse_coordinates() {
        let viewport = Viewport {
            x: 1,
            y: 20,
            rows: 10,
            cols: 100,
        };

        assert_eq!(viewport.coordinates(10, 25), Some((10, 6)));
        assert_eq!(viewport.coordinates(0, 25), None);
        assert_eq!(viewport.coordinates(10, 30), None);
        assert_eq!(viewport.clamped_coordinates(0, 99), (1, 10));
    }

    #[test]
    fn sgr_mouse_encoding_forwards_scroll_drag_and_release() {
        use vt100::{MouseProtocolEncoding, MouseProtocolMode};

        let scroll = MouseEvent {
            kind: MouseEventKind::ScrollUp,
            column: 10,
            row: 25,
            modifiers: KeyModifiers::NONE,
        };
        let drag = MouseEvent {
            kind: MouseEventKind::Drag(MouseButton::Left),
            column: 10,
            row: 25,
            modifiers: KeyModifiers::CONTROL,
        };
        let release = MouseEvent {
            kind: MouseEventKind::Up(MouseButton::Left),
            column: 10,
            row: 25,
            modifiers: KeyModifiers::NONE,
        };

        assert_eq!(
            encode_mouse(
                scroll,
                (10, 6),
                MouseProtocolMode::PressRelease,
                MouseProtocolEncoding::Sgr,
            ),
            Some(b"\x1b[<64;10;6M".to_vec())
        );
        assert_eq!(
            encode_mouse(
                drag,
                (10, 6),
                MouseProtocolMode::ButtonMotion,
                MouseProtocolEncoding::Sgr,
            ),
            Some(b"\x1b[<48;10;6M".to_vec())
        );
        assert_eq!(
            encode_mouse(
                release,
                (10, 6),
                MouseProtocolMode::PressRelease,
                MouseProtocolEncoding::Sgr,
            ),
            Some(b"\x1b[<0;10;6m".to_vec())
        );
        assert_eq!(
            encode_mouse(
                drag,
                (10, 6),
                MouseProtocolMode::PressRelease,
                MouseProtocolEncoding::Sgr,
            ),
            None
        );
    }

    #[test]
    fn parser_answers_cursor_position_queries() {
        let (input, receiver) = mpsc::channel();
        let (host_sequences, _) = mpsc::channel();
        let mut parser = vt100::Parser::new_with_callbacks(
            12,
            80,
            0,
            TerminalCallbacks {
                input,
                host_sequences,
            },
        );

        parser.process(b"\x1b[6n");

        assert_eq!(
            receiver.recv_timeout(Duration::from_millis(100)).unwrap(),
            b"\x1b[1;1R"
        );
    }

    #[test]
    fn parser_forwards_clipboard_copy_to_the_outer_terminal() {
        let (input, _) = mpsc::channel();
        let (host_sequences, receiver) = mpsc::channel();
        let mut parser = vt100::Parser::new_with_callbacks(
            12,
            80,
            0,
            TerminalCallbacks {
                input,
                host_sequences,
            },
        );

        parser.process(b"\x1b]52;c;Q29waWVkIHRleHQ=\x07");

        assert_eq!(
            receiver.recv_timeout(Duration::from_millis(100)).unwrap(),
            b"\x1b]52;c;Q29waWVkIHRleHQ=\x1b\\"
        );
    }

    #[test]
    fn parser_forwards_progress_to_the_outer_terminal() {
        let (input, _) = mpsc::channel();
        let (host_sequences, receiver) = mpsc::channel();
        let mut parser = vt100::Parser::new_with_callbacks(
            12,
            80,
            0,
            TerminalCallbacks {
                input,
                host_sequences,
            },
        );

        parser.process(b"\x1b]9;4;3;0\x07");

        assert_eq!(
            receiver.recv_timeout(Duration::from_millis(100)).unwrap(),
            b"\x1b]9;4;3;0\x1b\\"
        );
    }

    #[test]
    fn terminals_are_kept_per_session_until_shutdown() {
        let directory = tempfile::tempdir().unwrap();
        let cwd = directory.path().to_string_lossy().to_string();
        let config = TerminalConfig::default();
        let mut manager = TerminalManager::default();

        assert_eq!(
            manager
                .activate(
                    "session-a".to_string(),
                    "A".to_string(),
                    cwd.clone(),
                    &config,
                )
                .unwrap(),
            Activation::Opened
        );
        manager.hide();
        assert_eq!(
            manager
                .activate(
                    "session-a".to_string(),
                    "A".to_string(),
                    cwd.clone(),
                    &config,
                )
                .unwrap(),
            Activation::Focused
        );
        assert_eq!(
            manager
                .activate("session-b".to_string(), "B".to_string(), cwd, &config,)
                .unwrap(),
            Activation::Opened
        );

        assert_eq!(manager.panes.len(), 2);
        assert_eq!(manager.active_session_id(), Some("session-b"));
        let _ = manager.shutdown();
        assert!(manager.panes.is_empty());
        assert!(!manager.is_visible());
    }

    #[test]
    fn stopped_terminal_is_detected_and_restarts_when_reopened() {
        let directory = tempfile::tempdir().unwrap();
        let cwd = directory.path().to_string_lossy().to_string();
        let config = TerminalConfig::default();
        let mut manager = TerminalManager::default();

        assert_eq!(
            manager
                .activate(
                    "session-a".to_string(),
                    "A".to_string(),
                    cwd.clone(),
                    &config,
                )
                .unwrap(),
            Activation::Opened
        );
        manager
            .active_mut()
            .unwrap()
            .process
            .send(if cfg!(windows) {
                b"exit\r\n".to_vec()
            } else {
                b"exit\n".to_vec()
            })
            .unwrap();

        // A real shell has to start and run a user profile before it will act on
        // input; that alone was measured at over a second, and these tests run in
        // parallel with others that spawn their own PTYs. The loop exits as soon as
        // the shell reacts, so a generous ceiling only costs time when genuinely broken.
        let deadline = Instant::now() + Duration::from_secs(30);
        while Instant::now() < deadline && manager.active().unwrap().is_running() {
            std::thread::sleep(Duration::from_millis(20));
        }

        assert_eq!(manager.stopped_session_ids(), vec!["session-a"]);
        manager.hide();
        assert!(!manager.is_visible());
        assert_eq!(
            manager
                .activate("session-a".to_string(), "A".to_string(), cwd, &config,)
                .unwrap(),
            Activation::Restarted
        );
        assert!(manager.active().unwrap().is_running());
        let _ = manager.shutdown();
    }

    #[test]
    fn manager_shutdown_waits_for_shell_exit() {
        let directory = tempfile::tempdir().unwrap();
        let mut manager = TerminalManager::default();
        manager
            .activate(
                "session-a".to_string(),
                "Terminal".to_string(),
                directory.path().to_string_lossy().to_string(),
                &TerminalConfig::default(),
            )
            .unwrap();

        manager.shutdown().unwrap();

        assert!(manager.panes.is_empty());
        assert!(manager.active_id.is_none());
    }

    #[test]
    fn manager_remove_waits_for_shell_exit_before_forgetting_it() {
        let directory = tempfile::tempdir().unwrap();
        let mut manager = TerminalManager::default();
        manager
            .activate(
                "session-a".to_string(),
                "Terminal".to_string(),
                directory.path().to_string_lossy().to_string(),
                &TerminalConfig::default(),
            )
            .unwrap();

        manager.remove("session-a").unwrap();

        assert!(manager.panes.is_empty());
        assert!(manager.active_id.is_none());
    }

    #[test]
    fn manager_remove_stops_child_even_after_terminal_io_failure() {
        let directory = tempfile::tempdir().unwrap();
        let mut manager = TerminalManager::default();
        manager
            .activate(
                "session-a".to_string(),
                "Terminal".to_string(),
                directory.path().to_string_lossy().to_string(),
                &TerminalConfig::default(),
            )
            .unwrap();
        let pane = manager.panes.get("session-a").unwrap();
        *pane.process.status.lock().unwrap() =
            TerminalStatus::Failed("simulated parser failure".to_string());

        manager.remove("session-a").unwrap();

        assert!(manager.panes.is_empty());
    }

    #[test]
    fn configured_shell_runs_in_session_directory() {
        let directory = tempfile::tempdir().unwrap();
        let config = TerminalConfig {
            shell: Some(if cfg!(windows) {
                "powershell.exe".to_string()
            } else {
                "/bin/sh".to_string()
            }),
        };
        let pane = TerminalPane::open(
            "Test".to_string(),
            directory.path().to_string_lossy().to_string(),
            1,
            &config,
        )
        .unwrap();
        pane.process
            .send(if cfg!(windows) {
                b"Write-Output (Get-Location).Path; Write-Output ('CST_' + 'PTY_OK')\r\n".to_vec()
            } else {
                b"pwd; echo CST_PTY_OK\n".to_vec()
            })
            .unwrap();

        // See the note on the other shell deadline in this module: profile loading
        // makes a few seconds far too tight once these run alongside other PTY tests.
        let deadline = Instant::now() + Duration::from_secs(30);
        while Instant::now() < deadline && !pane.contents().contains("CST_PTY_OK") {
            std::thread::sleep(Duration::from_millis(20));
        }

        assert!(
            pane.contents().contains("CST_PTY_OK"),
            "terminal status: {:?}, contents: {:?}",
            pane.status(),
            pane.contents()
        );
        let canonical = std::fs::canonicalize(directory.path()).unwrap();
        let expected = canonical
            .to_string_lossy()
            .trim_start_matches(r"\\?\")
            .to_lowercase();
        assert!(
            pane.contents().to_lowercase().contains(&expected),
            "expected cwd {expected:?}, contents: {:?}",
            pane.contents()
        );
    }
}

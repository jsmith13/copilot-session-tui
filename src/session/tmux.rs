use anyhow::{Context, Result};
use fs4::{FileExt, TryLockError};
use serde::{Deserialize, Serialize};
use std::fs::{self, File, OpenOptions};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::OnceLock;
use std::thread;
use std::time::{Duration, Instant};

const REGISTRY_VERSION: u32 = 1;
const REGISTRY_LOCK_TIMEOUT: Duration = Duration::from_secs(5);
const REGISTRY_LOCK_RETRY: Duration = Duration::from_millis(25);
const STARTUP_GRACE: Duration = Duration::from_millis(350);

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TmuxSessionRef {
    pub session_id: String,
    pub tmux_session: String,
    pub cwd: PathBuf,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub server_socket: Option<PathBuf>,
}

#[derive(Debug, Default, Serialize, Deserialize)]
struct Registry {
    #[serde(default = "registry_version")]
    version: u32,
    #[serde(default)]
    sessions: Vec<TmuxSessionRef>,
}

struct RegistryLock {
    _file: File,
}

impl RegistryLock {
    fn acquire(path: &Path) -> Result<Self> {
        let parent = path
            .parent()
            .context("tmux registry path has no parent directory")?;
        fs::create_dir_all(parent).with_context(|| {
            format!(
                "Failed to create tmux registry directory: {}",
                parent.display()
            )
        })?;
        let lock_path = path.with_extension("lock");
        let file = OpenOptions::new()
            .create(true)
            .read(true)
            .write(true)
            .truncate(false)
            .open(&lock_path)
            .with_context(|| format!("Failed to open {}", lock_path.display()))?;
        let started = Instant::now();
        loop {
            match FileExt::try_lock(&file) {
                Ok(()) => return Ok(Self { _file: file }),
                Err(TryLockError::WouldBlock) if started.elapsed() < REGISTRY_LOCK_TIMEOUT => {
                    thread::sleep(
                        REGISTRY_LOCK_RETRY
                            .min(REGISTRY_LOCK_TIMEOUT.saturating_sub(started.elapsed())),
                    );
                }
                Err(TryLockError::WouldBlock) => {
                    anyhow::bail!(
                        "Timed out waiting for tmux registry lock {}",
                        lock_path.display()
                    );
                }
                Err(TryLockError::Error(error)) => {
                    return Err(error)
                        .with_context(|| format!("Failed to lock {}", lock_path.display()));
                }
            }
        }
    }
}

pub fn launch(
    cwd: &Path,
    title: &str,
    program: &str,
    args: &[String],
    session_id: &str,
) -> Result<TmuxSessionRef> {
    check_available()?;
    let mut reference = TmuxSessionRef {
        session_id: session_id.to_string(),
        tmux_session: session_name(title, session_id),
        cwd: cwd.to_path_buf(),
        // Clipboard policy is server-wide, so isolate it from the user's own tmux server.
        server_socket: Some(dedicated_server_socket()),
    };
    prepare_server_socket(&reference)?;
    let mut created = false;
    let mut last_error = None;
    for suffix_len in [8, 12, 32] {
        reference.tmux_session = session_name_with_suffix(title, session_id, suffix_len);
        let output = tmux_command(&reference)
            .args(["new-session", "-d", "-s"])
            .arg(&reference.tmux_session)
            .arg("-c")
            .arg(cwd)
            .output()
            .context("Failed to run tmux; make sure tmux is installed and on PATH")?;
        if output.status.success() {
            created = true;
            break;
        }
        let detail = String::from_utf8_lossy(&output.stderr).trim().to_string();
        if !detail.to_ascii_lowercase().contains("duplicate session") {
            return Err(tmux_error(&reference, "create", &detail));
        }
        last_error = Some(detail);
    }
    if !created {
        return Err(tmux_error(
            &reference,
            "create",
            last_error
                .as_deref()
                .unwrap_or("all generated names collided"),
        ));
    }
    if let Err(error) = configure_clipboard(&reference) {
        let _ = kill_unregistered(&reference);
        return Err(error).context("tmux session was stopped because clipboard setup failed");
    }
    if let Err(error) = start_program(&reference, cwd, program, args) {
        let _ = kill_unregistered(&reference);
        return Err(error).context("tmux session was stopped because Copilot could not start");
    }
    thread::sleep(STARTUP_GRACE);
    if !is_live(&reference)? {
        anyhow::bail!(
            "Copilot exited before tmux session '{}' finished starting",
            reference.tmux_session
        );
    }
    if let Err(error) = register(&reference) {
        let _ = kill(&reference);
        return Err(error).context("tmux session was stopped because ownership could not be saved");
    }
    Ok(reference)
}

pub fn find_live(session_id: &str) -> Result<Option<TmuxSessionRef>> {
    let path = registry_path();
    find_live_in(&path, session_id)
}

pub fn list_live() -> Result<Vec<TmuxSessionRef>> {
    let path = registry_path();
    list_live_in(&path)
}

fn list_live_in(path: &Path) -> Result<Vec<TmuxSessionRef>> {
    let _lock = RegistryLock::acquire(path)?;
    let mut registry = load_unlocked(path)?;
    if registry.sessions.is_empty() {
        return Ok(Vec::new());
    }
    let mut live = Vec::new();
    for reference in &registry.sessions {
        if is_live(reference)? {
            live.push(reference.clone());
        }
    }
    if live.len() != registry.sessions.len() {
        registry.sessions = live.clone();
        save_unlocked(path, &registry)?;
    }
    Ok(live)
}

fn find_live_in(path: &Path, session_id: &str) -> Result<Option<TmuxSessionRef>> {
    find_live_in_with(path, session_id, is_live)
}

fn find_live_in_with<F>(
    path: &Path,
    session_id: &str,
    mut live: F,
) -> Result<Option<TmuxSessionRef>>
where
    F: FnMut(&TmuxSessionRef) -> Result<bool>,
{
    let _lock = RegistryLock::acquire(path)?;
    let mut registry = load_unlocked(path)?;
    let Some(reference) = registry
        .sessions
        .iter()
        .find(|reference| reference.session_id == session_id)
        .cloned()
    else {
        return Ok(None);
    };
    if live(&reference)? {
        return Ok(Some(reference));
    }
    registry
        .sessions
        .retain(|candidate| candidate.session_id != session_id);
    save_unlocked(path, &registry)?;
    Ok(None)
}

pub fn attach_command(reference: &TmuxSessionRef) -> (String, Vec<String>) {
    let mut args = vec!["-u".to_string(), "TMUX".to_string(), "tmux".to_string()];
    append_socket_args(&mut args, reference);
    args.extend([
        "attach-session".to_string(),
        "-t".to_string(),
        reference.tmux_session.clone(),
    ]);
    ("env".to_string(), args)
}

pub fn attach_foreground(reference: &TmuxSessionRef) -> Result<()> {
    let mut command = tmux_command(reference);
    let status = command
        .args(["attach-session", "-t"])
        .arg(&reference.tmux_session)
        .status()
        .with_context(|| format!("Failed to attach tmux session '{}'", reference.tmux_session))?;
    if !status.success() {
        anyhow::bail!(
            "tmux session '{}' exited with status {status}",
            reference.tmux_session
        );
    }
    Ok(())
}

pub fn kill(reference: &TmuxSessionRef) -> Result<()> {
    let output = tmux_command(reference)
        .args(["kill-session", "-t"])
        .arg(&reference.tmux_session)
        .output()
        .with_context(|| format!("Failed to stop tmux session '{}'", reference.tmux_session))?;
    if !output.status.success() {
        if !is_live(reference)? {
            return unregister(&reference.session_id);
        }
        return Err(tmux_error(
            reference,
            "stop",
            &String::from_utf8_lossy(&output.stderr),
        ));
    }
    unregister(&reference.session_id)
}

pub fn rename(reference: &TmuxSessionRef, title: &str) -> Result<TmuxSessionRef> {
    let mut last_error = None;
    for suffix_len in [8, 12, 32] {
        let tmux_session = session_name_with_suffix(title, &reference.session_id, suffix_len);
        match rename_to(reference, tmux_session) {
            Ok(renamed) => return Ok(renamed),
            Err(error)
                if error
                    .to_string()
                    .to_ascii_lowercase()
                    .contains("duplicate session") =>
            {
                last_error = Some(error);
            }
            Err(error) => return Err(error),
        }
    }
    Err(last_error.unwrap_or_else(|| anyhow::anyhow!("all generated tmux names collided")))
}

pub fn restore_name(current: &TmuxSessionRef, original: &TmuxSessionRef) -> Result<TmuxSessionRef> {
    rename_to(current, original.tmux_session.clone())
}

fn rename_to(reference: &TmuxSessionRef, tmux_session: String) -> Result<TmuxSessionRef> {
    if tmux_session == reference.tmux_session {
        return Ok(reference.clone());
    }
    let output = tmux_command(reference)
        .args(["rename-session", "-t"])
        .arg(&reference.tmux_session)
        .arg(&tmux_session)
        .output()
        .with_context(|| format!("Failed to rename tmux session '{}'", reference.tmux_session))?;
    if !output.status.success() {
        return Err(tmux_error(
            reference,
            "rename",
            &String::from_utf8_lossy(&output.stderr),
        ));
    }
    let mut renamed = reference.clone();
    renamed.tmux_session = tmux_session;
    if let Err(error) = register(&renamed) {
        let _ = tmux_command(&renamed)
            .args(["rename-session", "-t"])
            .arg(&renamed.tmux_session)
            .arg(&reference.tmux_session)
            .output();
        return Err(error)
            .context("tmux rename was rolled back because ownership could not be saved");
    }
    Ok(renamed)
}

pub fn supported_platform() -> bool {
    cfg!(unix)
}

fn register(reference: &TmuxSessionRef) -> Result<()> {
    let path = registry_path();
    let _lock = RegistryLock::acquire(&path)?;
    let mut registry = load_unlocked(&path)?;
    registry
        .sessions
        .retain(|candidate| candidate.session_id != reference.session_id);
    registry.sessions.push(reference.clone());
    save_unlocked(&path, &registry)
}

fn unregister(session_id: &str) -> Result<()> {
    let path = registry_path();
    let _lock = RegistryLock::acquire(&path)?;
    let mut registry = load_unlocked(&path)?;
    registry
        .sessions
        .retain(|candidate| candidate.session_id != session_id);
    save_unlocked(&path, &registry)
}

fn is_live(reference: &TmuxSessionRef) -> Result<bool> {
    check_available()?;
    let output = tmux_command(reference)
        .args(["has-session", "-t"])
        .arg(&reference.tmux_session)
        .output()
        .context("Failed to inspect tmux session; make sure tmux is installed and on PATH")?;
    Ok(output.status.success())
}

pub fn check_available() -> Result<()> {
    if !supported_platform() {
        anyhow::bail!("tmux-backed sessions require a Unix-like operating system");
    }
    static AVAILABLE: OnceLock<()> = OnceLock::new();
    if AVAILABLE.get().is_some() {
        return Ok(());
    }
    let output = Command::new("tmux")
        .arg("-V")
        .output()
        .map_err(|error| anyhow::anyhow!("tmux is not installed or not on PATH: {error}"))?;
    if !output.status.success() {
        let detail = String::from_utf8_lossy(&output.stderr).trim().to_string();
        if detail.is_empty() {
            anyhow::bail!("tmux is installed but `tmux -V` failed");
        }
        anyhow::bail!("tmux is unavailable: {detail}");
    }
    let _ = AVAILABLE.set(());
    Ok(())
}

fn tmux_command(reference: &TmuxSessionRef) -> Command {
    let mut command = Command::new("tmux");
    command.env_remove("TMUX");
    if let Some(socket) = &reference.server_socket {
        command.arg("-S").arg(socket);
    }
    command
}

fn prepare_server_socket(reference: &TmuxSessionRef) -> Result<()> {
    let Some(parent) = reference.server_socket.as_deref().and_then(Path::parent) else {
        return Ok(());
    };
    fs::create_dir_all(parent).with_context(|| {
        format!(
            "Failed to create CST tmux socket directory: {}",
            parent.display()
        )
    })
}

fn configure_clipboard(reference: &TmuxSessionRef) -> Result<()> {
    let output = tmux_command(reference)
        .args(["set-option", "-s", "set-clipboard", "on"])
        .output()
        .context("Failed to configure tmux clipboard forwarding")?;
    if !output.status.success() {
        return Err(tmux_error(
            reference,
            "configure clipboard forwarding for",
            &String::from_utf8_lossy(&output.stderr),
        ));
    }
    Ok(())
}

fn start_program(
    reference: &TmuxSessionRef,
    cwd: &Path,
    program: &str,
    args: &[String],
) -> Result<()> {
    let output = tmux_command(reference)
        .args(["respawn-pane", "-k", "-t"])
        .arg(format!("{}:0.0", reference.tmux_session))
        .arg("-c")
        .arg(cwd)
        // Copilot changes clipboard backends when TMUX is present. It is still owned by
        // tmux, but hiding the variable keeps the direct OSC 52 path that CST forwards.
        .arg("env")
        .args(["-u", "TMUX"])
        // Multiple shell-command arguments are executed directly by tmux.
        .arg(program)
        .args(args)
        .output()
        .with_context(|| {
            format!(
                "Failed to start Copilot in tmux session '{}'",
                reference.tmux_session
            )
        })?;
    if !output.status.success() {
        return Err(tmux_error(
            reference,
            "start Copilot in",
            &String::from_utf8_lossy(&output.stderr),
        ));
    }
    Ok(())
}

fn kill_unregistered(reference: &TmuxSessionRef) -> Result<()> {
    let output = tmux_command(reference)
        .args(["kill-session", "-t"])
        .arg(&reference.tmux_session)
        .output()
        .with_context(|| format!("Failed to stop tmux session '{}'", reference.tmux_session))?;
    if output.status.success() || !is_live(reference)? {
        return Ok(());
    }
    Err(tmux_error(
        reference,
        "stop",
        &String::from_utf8_lossy(&output.stderr),
    ))
}

fn append_socket_args(args: &mut Vec<String>, reference: &TmuxSessionRef) {
    if let Some(socket) = &reference.server_socket {
        args.push("-S".to_string());
        args.push(socket.to_string_lossy().to_string());
    }
}

fn dedicated_server_socket() -> PathBuf {
    dirs::data_local_dir()
        .unwrap_or_else(|| {
            dirs::home_dir()
                .unwrap_or_else(|| PathBuf::from("."))
                .join(".local")
                .join("share")
        })
        .join("cst")
        .join("tmux.sock")
}

fn session_name(title: &str, session_id: &str) -> String {
    session_name_with_suffix(title, session_id, 8)
}

fn session_name_with_suffix(title: &str, session_id: &str, suffix_len: usize) -> String {
    let mut slug = String::new();
    let mut separated = false;
    for character in title.chars() {
        if character.is_ascii_alphanumeric() {
            slug.push(character.to_ascii_lowercase());
            separated = false;
        } else if !slug.is_empty() && !separated {
            slug.push('-');
            separated = true;
        }
        if slug.len() >= 32 {
            break;
        }
    }
    while slug.ends_with('-') {
        slug.pop();
    }
    if slug.is_empty() {
        slug.push_str("session");
    }
    let short_id: String = session_id
        .chars()
        .filter(|character| character.is_ascii_alphanumeric())
        .take(suffix_len)
        .collect();
    format!("cst-{slug}-{short_id}")
}

fn registry_path() -> PathBuf {
    dirs::data_local_dir()
        .unwrap_or_else(|| {
            dirs::home_dir()
                .unwrap_or_else(|| PathBuf::from("."))
                .join(".local")
                .join("share")
        })
        .join("cst")
        .join("tmux-sessions.json")
}

/// A registry that cannot be read must never block resuming sessions: the worst
/// correct outcome of losing it is that CST forgets which tmux sessions it owned,
/// which `tmux attach` can recover from by hand. A corrupt or future-version file is
/// therefore moved aside for inspection and treated as empty, not surfaced as an
/// error — an error here would propagate into every resume path, tmux-backed or not.
fn load_unlocked(path: &Path) -> Result<Registry> {
    let empty = Registry {
        version: REGISTRY_VERSION,
        sessions: Vec::new(),
    };
    match fs::read_to_string(path) {
        Ok(content) => {
            let registry = match serde_json::from_str::<Registry>(&content) {
                Ok(registry) if registry.version == REGISTRY_VERSION => registry,
                Ok(_) | Err(_) => {
                    quarantine_registry(path);
                    empty
                }
            };
            Ok(registry)
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(empty),
        Err(error) => {
            Err(error).with_context(|| format!("Failed to read tmux registry: {}", path.display()))
        }
    }
}

/// Best effort: keep the bad bytes next to the registry for debugging. If even the
/// rename fails the next save overwrites the file, which is still the right outcome.
fn quarantine_registry(path: &Path) {
    let _ = fs::rename(path, path.with_extension("json.bad"));
}

fn save_unlocked(path: &Path, registry: &Registry) -> Result<()> {
    let parent = path
        .parent()
        .context("tmux registry path has no parent directory")?;
    fs::create_dir_all(parent)?;
    let content = serde_json::to_vec_pretty(registry)?;
    let mut temporary = tempfile::NamedTempFile::new_in(parent)?;
    temporary.as_file_mut().write_all(&content)?;
    temporary.as_file_mut().sync_all()?;
    temporary
        .persist(path)
        .map_err(|error| error.error)
        .with_context(|| format!("Failed to replace tmux registry: {}", path.display()))?;
    Ok(())
}

fn tmux_error(reference: &TmuxSessionRef, action: &str, stderr: &str) -> anyhow::Error {
    let detail = stderr.trim();
    if detail.is_empty() {
        anyhow::anyhow!(
            "tmux could not {action} session '{}'",
            reference.tmux_session
        )
    } else {
        anyhow::anyhow!(
            "tmux could not {action} session '{}': {detail}",
            reference.tmux_session
        )
    }
}

const fn registry_version() -> u32 {
    REGISTRY_VERSION
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tmux_session_names_are_safe_readable_and_collision_resistant() {
        assert_eq!(
            session_name("copilot/Copy parser & tests", "12345678-abcd"),
            "cst-copilot-copy-parser-tests-12345678"
        );
        assert_eq!(session_name("🚀", "abcdef12-abcd"), "cst-session-abcdef12");
        assert_eq!(
            session_name_with_suffix("copy parser", "12345678-abcd-ef00", 12),
            "cst-copy-parser-12345678abcd"
        );
    }

    #[test]
    fn pane_attach_clears_nested_tmux_and_preserves_server_socket() {
        let reference = TmuxSessionRef {
            session_id: "session".to_string(),
            tmux_session: "cst-session-12345678".to_string(),
            cwd: PathBuf::from("/tmp"),
            server_socket: Some(PathBuf::from("/tmp/tmux.sock")),
        };

        let (program, args) = attach_command(&reference);

        assert_eq!(program, "env");
        assert_eq!(
            args,
            vec![
                "-u",
                "TMUX",
                "tmux",
                "-S",
                "/tmp/tmux.sock",
                "attach-session",
                "-t",
                "cst-session-12345678"
            ]
        );
    }

    #[test]
    fn registry_round_trips_tmux_session_ownership() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("tmux-sessions.json");
        let reference = TmuxSessionRef {
            session_id: "session".to_string(),
            tmux_session: "cst-session-12345678".to_string(),
            cwd: PathBuf::from("/tmp/project"),
            server_socket: Some(PathBuf::from("/tmp/tmux.sock")),
        };
        let registry = Registry {
            version: REGISTRY_VERSION,
            sessions: vec![reference.clone()],
        };

        save_unlocked(&path, &registry).unwrap();
        let loaded = load_unlocked(&path).unwrap();

        assert_eq!(loaded.sessions, vec![reference]);
    }

    #[test]
    fn a_damaged_registry_is_quarantined_instead_of_blocking_every_resume() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("tmux-sessions.json");
        fs::write(&path, "{ not json").unwrap();

        let loaded = load_unlocked(&path).unwrap();

        assert!(loaded.sessions.is_empty());
        let quarantined = path.with_extension("json.bad");
        assert_eq!(fs::read_to_string(&quarantined).unwrap(), "{ not json");
        assert!(!path.exists(), "the bad file must be moved, not kept");
    }

    #[test]
    fn a_future_registry_version_is_set_aside_rather_than_erroring_after_a_downgrade() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("tmux-sessions.json");
        fs::write(&path, r#"{"version": 2, "sessions": []}"#).unwrap();

        let loaded = load_unlocked(&path).unwrap();

        assert!(loaded.sessions.is_empty());
        assert!(path.with_extension("json.bad").exists());
    }

    #[test]
    fn lookup_prunes_a_stale_tmux_mapping() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("tmux-sessions.json");
        let reference = TmuxSessionRef {
            session_id: "stale-session".to_string(),
            tmux_session: format!("cst-test-missing-{}", uuid::Uuid::new_v4()),
            cwd: PathBuf::from("/tmp/project"),
            server_socket: None,
        };
        save_unlocked(
            &path,
            &Registry {
                version: REGISTRY_VERSION,
                sessions: vec![reference],
            },
        )
        .unwrap();

        assert!(find_live_in_with(&path, "stale-session", |_| Ok(false))
            .unwrap()
            .is_none());
        assert!(load_unlocked(&path).unwrap().sessions.is_empty());
    }
}

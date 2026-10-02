use anyhow::{Context, Result};
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::OnceLock;

use crate::config::{EffectiveWorktreeConfig, UserConfig};

use super::tmux;
pub use super::tmux::TmuxSessionRef;
use super::worktree::{self, ManagedWorktree};

fn apply_args(cmd: &mut Command, args: Vec<String>) {
    for arg in args {
        cmd.arg(arg);
    }
}

/// The settings that apply to one launch, after the repository has had its say.
///
/// Resolved once and passed around, so adding the next per-launch setting does not mean
/// threading another parameter through every command builder.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct LaunchPolicy {
    pub yolo: bool,
    /// `None` leaves Copilot's own default alone rather than pinning a number.
    pub max_autopilot_continues: Option<u32>,
}

impl LaunchPolicy {
    /// Work out what applies for work in `cwd`.
    ///
    /// A repository's `.cst.json` wins over the global setting when it says anything, so
    /// a project can opt itself in or — more usefully — force the permission prompts
    /// back on for everyone working in it, whatever their own default is.
    ///
    /// A directory outside a Git project has no project file to consult. A project file
    /// that cannot be read falls back to the global settings too: it is reported where
    /// it is edited, and refusing to start a session over an unparseable preference
    /// would be a worse failure than starting with the user's own defaults.
    pub fn resolve(config: &UserConfig, cwd: &Path) -> Self {
        let project = crate::session::loader::detect_project_root(&cwd.to_string_lossy())
            .and_then(|root| crate::config::ProjectSettings::load(Path::new(&root), config).ok());
        match project {
            Some(project) => Self {
                yolo: project.effective_yolo(),
                max_autopilot_continues: project.effective_max_autopilot_continues(),
            },
            None => Self {
                yolo: config.yolo,
                max_autopilot_continues: config.max_autopilot_continues,
            },
        }
    }
}

/// Launch policy that applies every time Copilot starts.
///
/// `--max-autopilot-continues` belongs here rather than with the new-session defaults
/// below: Copilot does not persist it in the session, so passing it only on creation
/// would silently do nothing for every resumed session, which is most of them.
fn runtime_args(policy: &LaunchPolicy) -> Vec<String> {
    let mut args = Vec::new();
    if policy.yolo {
        args.push("--yolo".to_string());
    }
    if let Some(limit) = policy.max_autopilot_continues {
        args.push(format!("--max-autopilot-continues={limit}"));
    }
    args
}

/// Defaults applied only while creating a new session.
///
/// Copilot persists model and effort in the session. Passing them again on resume would
/// overwrite a model the user selected inside that conversation with CST's current
/// defaults.
fn new_session_config_args(config: &UserConfig, policy: &LaunchPolicy) -> Vec<String> {
    let mut args = runtime_args(policy);
    if let Some(ref model) = config.model {
        args.push(format!("--model={}", model));
    }
    if let Some(ref effort) = config.reasoning_effort {
        args.push(format!("--reasoning-effort={}", effort));
    }
    args
}

/// Program plus arguments for resuming an existing session inside a pane.
///
/// `cwd` is where the session will run, which is what decides whether the project it
/// belongs to has an opinion about `--yolo`.
pub fn resume_command(
    session_id: &str,
    config: &UserConfig,
    cwd: &Path,
) -> Result<(String, Vec<String>)> {
    let copilot = find_copilot()?;
    Ok((
        copilot,
        resume_args(session_id, &LaunchPolicy::resolve(config, cwd)),
    ))
}

fn resume_args(session_id: &str, policy: &LaunchPolicy) -> Vec<String> {
    let mut args = vec![format!("--resume={}", session_id)];
    args.extend(runtime_args(policy));
    args
}

/// Program plus arguments for starting a fresh session inside a pane, and the id that
/// session will have.
///
/// The id is ours to choose: `--session-id` names a new session rather than resuming one.
/// Deciding it up front is what lets a pane bind its scratchpad and terminal to the real
/// session from the moment it spawns, instead of waiting for Copilot to invent an id and
/// having nothing stable to key on in the meantime.
pub fn new_session_command(
    config: &UserConfig,
    cwd: &Path,
) -> Result<(String, Vec<String>, String)> {
    let copilot = find_copilot()?;
    let (args, session_id) = new_session_args(config, &LaunchPolicy::resolve(config, cwd));
    Ok((copilot, args, session_id))
}

/// The argument half of [`new_session_command`], split out so it can be tested without
/// a Copilot binary on PATH.
fn new_session_args(config: &UserConfig, policy: &LaunchPolicy) -> (Vec<String>, String) {
    let session_id = uuid::Uuid::new_v4().to_string();
    let mut args = vec![format!("--session-id={session_id}")];
    args.extend(new_session_config_args(config, policy));
    (args, session_id)
}

/// Rename a session using the current `name` field while preserving legacy metadata.
pub fn rename_session(session_dir: &Path, new_name: &str) -> Result<()> {
    let workspace_path = session_dir.join("workspace.yaml");
    let content = fs::read_to_string(&workspace_path)
        .with_context(|| format!("Failed to read {}", workspace_path.display()))?;

    let mut new_lines = Vec::new();
    let has_name = content.lines().any(|line| line.starts_with("name:"));
    let mut found_title = false;

    for line in content.lines() {
        if line.starts_with("name:") {
            new_lines.push(format!("name: {}", new_name));
            found_title = true;
        } else if !has_name && line.starts_with("summary:") && !line.starts_with("summary_count:") {
            new_lines.push(format!("summary: {}", new_name));
            found_title = true;
        } else {
            new_lines.push(line.to_string());
        }
    }

    if !found_title {
        // New Copilot CLI versions use `name`; older `summary` files remain
        // supported by the replacement path above.
        let mut inserted = Vec::new();
        for line in &new_lines {
            inserted.push(line.clone());
            if line.starts_with("id:") {
                inserted.push(format!("name: {}", new_name));
            }
        }
        new_lines = inserted;
    }

    let new_content = new_lines.join("\n") + "\n";
    fs::write(&workspace_path, new_content)
        .with_context(|| format!("Failed to write {}", workspace_path.display()))?;

    Ok(())
}

/// Delete a session by removing its directory
pub fn delete_session(session_dir: &Path) -> Result<()> {
    fs::remove_dir_all(session_dir)
        .with_context(|| format!("Failed to delete {}", session_dir.display()))?;
    Ok(())
}

pub fn delete_managed_session(
    session_dir: &Path,
    entry: &ManagedWorktree,
    force: bool,
) -> Result<String> {
    let outcome = worktree::remove_managed_worktree(entry, force)?;

    fs::remove_dir_all(session_dir)
        .with_context(|| format!("Failed to delete {}", session_dir.display()))?;

    let registry_warning = worktree::unregister(entry).err().map(|error| {
        format!("Registry cleanup will be pruned automatically on next load: {error}")
    });

    let mut messages = vec!["Session and worktree deleted".to_string()];
    if let Some(notice) = outcome.branch_notice {
        messages.push(notice);
    } else if outcome.branch_removed {
        messages.push(format!("Branch '{}' deleted", entry.branch));
    }
    if let Some(warning) = registry_warning {
        messages.push(warning);
    }
    Ok(messages.join(". "))
}

/// Resume a session by launching `copilot --resume=<id>` in the session's working directory
pub fn resume_session(session_id: &str, cwd: &str, config: &UserConfig) -> Result<()> {
    let copilot = find_copilot()?;

    let mut cmd = Command::new(copilot);
    cmd.arg(format!("--resume={}", session_id));
    apply_args(
        &mut cmd,
        runtime_args(&LaunchPolicy::resolve(config, Path::new(cwd))),
    );

    // Set the working directory to the session's original cwd
    if !cwd.is_empty() {
        let cwd_path = Path::new(cwd);
        if cwd_path.exists() {
            cmd.current_dir(cwd_path);
        }
    }

    cmd.status().context("Failed to launch copilot")?;

    Ok(())
}

/// Start a new session by launching `copilot` in the given working directory
pub fn start_new_session(cwd: &str, config: &UserConfig) -> Result<()> {
    let copilot = find_copilot()?;

    let mut cmd = Command::new(copilot);
    let cwd_path = Path::new(cwd);
    apply_args(
        &mut cmd,
        new_session_config_args(config, &LaunchPolicy::resolve(config, cwd_path)),
    );
    if cwd_path.exists() {
        cmd.current_dir(cwd_path);
    }

    cmd.status().context("Failed to launch copilot")?;

    Ok(())
}

/// Start a new Copilot conversation owned by a persistent tmux session.
pub fn start_tmux_session(cwd: &Path, title: &str, config: &UserConfig) -> Result<TmuxSessionRef> {
    let (program, args, session_id) = new_session_command(config, cwd)?;
    tmux::launch(cwd, title, &program, &args, &session_id)
}

/// Resume an existing Copilot conversation owned by a persistent tmux session.
pub fn resume_tmux_session(
    session_id: &str,
    cwd: &Path,
    title: &str,
    config: &UserConfig,
) -> Result<TmuxSessionRef> {
    let (program, args) = resume_command(session_id, config, cwd)?;
    tmux::launch(cwd, title, &program, &args, session_id)
}

pub fn attach_tmux_session(reference: &TmuxSessionRef) -> Result<()> {
    tmux::attach_foreground(reference)
}

pub fn start_worktree_session(
    project: &str,
    branch: &str,
    worktree_config: &EffectiveWorktreeConfig,
    config: &UserConfig,
) -> Result<PathBuf> {
    let copilot = find_copilot()?;
    let created = worktree::create_managed_worktree(Path::new(project), branch, worktree_config)?;

    if let Some(ref notice) = created.notice {
        eprintln!("Notice: {notice}");
    }
    eprintln!(
        "Starting isolated session on '{}' in {}...",
        branch,
        created.entry.path.display()
    );

    let mut cmd = Command::new(copilot);
    // The worktree carries the repository's own `.cst.json`, so it answers for the
    // project setting exactly as the main checkout would.
    let policy = LaunchPolicy::resolve(config, &created.entry.path);
    apply_args(&mut cmd, new_session_config_args(config, &policy));
    cmd.current_dir(&created.entry.path);

    if let Err(error) = cmd.status() {
        let rollback = worktree::rollback_created_worktree(&created.entry);
        return Err(match rollback {
            Ok(()) => anyhow::Error::new(error)
                .context("Failed to launch copilot; worktree creation was rolled back"),
            Err(rollback_error) => anyhow::Error::new(error).context(format!(
                "Failed to launch copilot, and worktree rollback also failed: {rollback_error}"
            )),
        });
    }

    Ok(created.entry.path)
}

pub fn start_worktree_tmux_session(
    project: &str,
    branch: &str,
    worktree_config: &EffectiveWorktreeConfig,
    config: &UserConfig,
) -> Result<TmuxSessionRef> {
    let created = worktree::create_managed_worktree(Path::new(project), branch, worktree_config)?;

    if let Some(ref notice) = created.notice {
        eprintln!("Notice: {notice}");
    }
    eprintln!(
        "Starting isolated tmux session on '{}' in {}...",
        branch,
        created.entry.path.display()
    );

    match start_tmux_session(&created.entry.path, branch, config) {
        Ok(reference) => Ok(reference),
        Err(error) => {
            let rollback = worktree::rollback_created_worktree(&created.entry);
            Err(match rollback {
                Ok(()) => error.context(
                    "Failed to launch Copilot in tmux; worktree creation was rolled back",
                ),
                Err(rollback_error) => error.context(format!(
                    "Failed to launch Copilot in tmux, and worktree rollback also failed: {rollback_error}"
                )),
            })
        }
    }
}

/// Locate the Copilot binary, caching the result.
///
/// The probe spawns `copilot --version`, which boots the whole Node CLI and costs
/// ~400ms. That ran on the UI thread for every session create and resume, freezing the
/// TUI before the pane could even show its startup spinner. The location cannot
/// meaningfully change while CST is running, so resolve it once.
fn find_copilot() -> Result<String> {
    copilot_probe().map(|probe| probe.program).ok_or_else(|| {
        anyhow::anyhow!("Could not find copilot CLI. Make sure it's installed and in PATH.")
    })
}

/// What the Copilot lookup found, for reporting rather than launching.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CopilotProbe {
    /// The program CST will actually spawn.
    pub program: String,
    /// First line of `copilot --version`, absent when it could not be read.
    pub version: Option<String>,
    /// Whether `--version` exited successfully. A binary that spawns but fails this
    /// is still the one sessions launch with, so `cst doctor` can say so.
    pub version_ok: bool,
}

/// The resolved Copilot CLI, sharing the cache [`find_copilot`] uses.
pub fn copilot_probe() -> Option<CopilotProbe> {
    RESOLVED.get_or_init(locate_copilot).clone()
}

static RESOLVED: OnceLock<Option<CopilotProbe>> = OnceLock::new();

/// Populate the Copilot lookup cache off the UI thread at startup.
pub fn warm_copilot_lookup() {
    std::thread::spawn(|| {
        let _ = find_copilot();
    });
}

fn locate_copilot() -> Option<CopilotProbe> {
    // Check common locations
    let candidates = ["copilot", "copilot.exe"];

    for candidate in &candidates {
        // Deliberately still keyed on the process starting rather than on its exit
        // status: tightening that would change which binary sessions launch with, on
        // machines this cannot be tested against. The status is reported instead.
        if let Ok(output) = Command::new(candidate).arg("--version").output() {
            return Some(CopilotProbe {
                program: candidate.to_string(),
                version: version_line(&String::from_utf8_lossy(&output.stdout)),
                version_ok: output.status.success(),
            });
        }
    }

    // Check npm global
    if let Ok(output) = Command::new("npm").args(["root", "-g"]).output() {
        let npm_root = String::from_utf8_lossy(&output.stdout).trim().to_string();
        let copilot_path = format!("{}/@github/copilot/bin/copilot", npm_root);
        if Path::new(&copilot_path).exists() {
            let probe = Command::new(&copilot_path).arg("--version").output().ok();
            return Some(CopilotProbe {
                version: probe
                    .as_ref()
                    .and_then(|out| version_line(&String::from_utf8_lossy(&out.stdout))),
                version_ok: probe.is_some_and(|out| out.status.success()),
                program: copilot_path,
            });
        }
    }

    None
}

/// First meaningful line of a `--version` banner.
///
/// Copilot prints two lines — the version, then an update hint — so anything past the
/// first is noise. Control bytes are dropped because the banner reaches a plain
/// `println!` in `cst doctor`, where an escape sequence would be executed.
fn version_line(stdout: &str) -> Option<String> {
    let line: String = stdout
        .lines()
        .map(str::trim)
        .find(|line| !line.is_empty())?
        .chars()
        .filter(|character| !character.is_control())
        .take(80)
        .collect();
    let line = line.trim_end_matches('.').trim();
    (!line.is_empty()).then(|| line.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The policy for a directory with no project, so the test is about arguments
    /// rather than about where they came from.
    fn policy_of(config: &UserConfig) -> LaunchPolicy {
        LaunchPolicy {
            yolo: config.yolo,
            max_autopilot_continues: config.max_autopilot_continues,
        }
    }

    #[test]
    fn a_new_session_is_told_the_id_it_will_have() {
        let config = UserConfig::default();

        let (args, session_id) = new_session_args(&config, &policy_of(&config));

        assert!(
            uuid::Uuid::parse_str(&session_id).is_ok(),
            "Copilot expects a UUID, got {session_id}"
        );
        assert_eq!(
            args.first().map(String::as_str),
            Some(format!("--session-id={session_id}").as_str()),
            "the pane and the child must agree on the id"
        );
    }

    #[test]
    fn every_new_session_gets_its_own_id() {
        let config = UserConfig::default();

        let (_, first) = new_session_args(&config, &policy_of(&config));
        let (_, second) = new_session_args(&config, &policy_of(&config));

        // Two new sessions sharing an id would share a scratchpad.
        assert_ne!(first, second);
    }

    #[test]
    fn naming_a_new_session_does_not_drop_the_configured_arguments() {
        let config = UserConfig {
            yolo: true,
            model: Some("claude-opus-5".to_string()),
            reasoning_effort: Some("high".to_string()),
            ..UserConfig::default()
        };

        let (args, _) = new_session_args(&config, &policy_of(&config));

        assert!(args.iter().any(|arg| arg == "--yolo"), "got {args:?}");
        assert!(
            args.iter().any(|arg| arg == "--model=claude-opus-5"),
            "got {args:?}"
        );
        assert!(
            args.iter().any(|arg| arg == "--reasoning-effort=high"),
            "got {args:?}"
        );
    }

    #[test]
    fn resuming_preserves_the_sessions_model_and_effort() {
        let config = UserConfig {
            yolo: true,
            model: Some("new-default-model".to_string()),
            reasoning_effort: Some("xhigh".to_string()),
            ..UserConfig::default()
        };

        let args = resume_args("existing-session", &policy_of(&config));

        assert_eq!(args[0], "--resume=existing-session");
        assert!(args.iter().any(|arg| arg == "--yolo"), "got {args:?}");
        assert!(
            !args.iter().any(|arg| arg.starts_with("--model=")),
            "a CST default must not replace the session's model: {args:?}"
        );
        assert!(
            !args
                .iter()
                .any(|arg| arg.starts_with("--reasoning-effort=")),
            "a CST default must not replace the session's effort: {args:?}"
        );
    }

    /// Proves against the real Copilot binary that the id CST picks is the id the
    /// session actually gets. Ignored by default: it needs Copilot installed and
    /// authenticated, and it spends a few AI credits.
    ///
    /// ```text
    /// cargo test -- --ignored a_new_session_really_is_created_under_the_id_we_chose
    /// ```
    #[test]
    #[ignore = "requires a real, authenticated Copilot CLI and spends AI credits"]
    fn a_new_session_really_is_created_under_the_id_we_chose() {
        let (program, args, session_id) =
            new_session_command(&UserConfig::default(), &std::env::temp_dir())
                .expect("Copilot CLI must be installed for this probe");

        let workdir = tempfile::tempdir().unwrap();
        let status = Command::new(&program)
            .args(&args)
            .args(["-p", "reply with exactly: ok"])
            .current_dir(workdir.path())
            .status()
            .expect("failed to launch copilot");
        assert!(status.success(), "copilot rejected {args:?}");

        let session_dir = dirs::home_dir()
            .expect("home directory")
            .join(".copilot")
            .join("session-state")
            .join(&session_id);
        let found = session_dir.is_dir();
        if found {
            let _ = fs::remove_dir_all(&session_dir);
        }
        assert!(
            found,
            "expected the session at {}, so per-session state keyed on {session_id} would find it",
            session_dir.display()
        );
    }

    #[test]
    fn rename_updates_current_name_field() {
        let temp = tempfile::tempdir().unwrap();
        let workspace = temp.path().join("workspace.yaml");
        fs::write(
            &workspace,
            "id: test\nname: Generated title\nsummary_count: 0\n",
        )
        .unwrap();

        rename_session(temp.path(), "My title").unwrap();

        let content = fs::read_to_string(workspace).unwrap();
        assert!(content.contains("name: My title\n"));
        assert!(!content.contains("summary: My title\n"));
    }

    #[test]
    fn rename_preserves_legacy_summary_field() {
        let temp = tempfile::tempdir().unwrap();
        let workspace = temp.path().join("workspace.yaml");
        fs::write(
            &workspace,
            "id: test\nsummary: Old title\nsummary_count: 1\n",
        )
        .unwrap();

        rename_session(temp.path(), "My title").unwrap();

        let content = fs::read_to_string(workspace).unwrap();
        assert!(content.contains("summary: My title\n"));
        assert!(content.contains("summary_count: 1\n"));
    }
    /// A directory that looks like a Git repository with the given project settings.
    fn repo_with(project_json: Option<&str>) -> tempfile::TempDir {
        let temp = tempfile::tempdir().unwrap();
        fs::create_dir(temp.path().join(".git")).unwrap();
        if let Some(json) = project_json {
            fs::write(temp.path().join(".cst.json"), json).unwrap();
        }
        temp
    }

    #[test]
    fn a_project_decides_yolo_for_sessions_started_in_it() {
        let careful = UserConfig::default();
        let reckless = UserConfig {
            yolo: true,
            ..UserConfig::default()
        };

        let opted_in = repo_with(Some(r#"{"yolo":true}"#));
        assert!(LaunchPolicy::resolve(&careful, opted_in.path()).yolo);

        // The direction that matters most: a repository can hold the prompts on for
        // someone whose own default is yolo.
        let opted_out = repo_with(Some(r#"{"yolo":false}"#));
        assert!(!LaunchPolicy::resolve(&reckless, opted_out.path()).yolo);

        // Silent project, and a project with other settings but no opinion here.
        let silent = repo_with(None);
        assert!(LaunchPolicy::resolve(&reckless, silent.path()).yolo);
        assert!(!LaunchPolicy::resolve(&careful, silent.path()).yolo);
        let unrelated = repo_with(Some(r#"{"worktree":{"branch_prefix":"x/"}}"#));
        assert!(LaunchPolicy::resolve(&reckless, unrelated.path()).yolo);
    }

    #[test]
    fn a_directory_with_no_project_falls_back_to_the_global_setting() {
        let reckless = UserConfig {
            yolo: true,
            ..UserConfig::default()
        };
        let loose = tempfile::tempdir().unwrap();

        assert!(LaunchPolicy::resolve(&reckless, loose.path()).yolo);
        assert!(!LaunchPolicy::resolve(&UserConfig::default(), loose.path()).yolo);
    }

    #[test]
    fn an_unreadable_project_file_starts_the_session_rather_than_failing_it() {
        let broken = repo_with(Some("{not json"));
        let reckless = UserConfig {
            yolo: true,
            ..UserConfig::default()
        };

        // Reported where it is edited; a launch is the wrong place to refuse over it.
        assert!(LaunchPolicy::resolve(&reckless, broken.path()).yolo);
        assert!(!LaunchPolicy::resolve(&UserConfig::default(), broken.path()).yolo);
    }

    #[test]
    fn the_project_answer_is_what_reaches_copilots_arguments() {
        let reckless = UserConfig {
            yolo: true,
            ..UserConfig::default()
        };
        let opted_out = repo_with(Some(r#"{"yolo":false}"#));
        let policy = LaunchPolicy::resolve(&reckless, opted_out.path());

        let (new_args, _) = new_session_args(&reckless, &policy);
        let resumed = resume_args("existing", &policy);

        assert!(
            !new_args.iter().any(|arg| arg == "--yolo"),
            "a global yolo must not leak past the project's no, got {new_args:?}"
        );
        assert!(
            !resumed.iter().any(|arg| arg == "--yolo"),
            "and resuming into that project is the same session, got {resumed:?}"
        );
    }
    #[test]
    fn a_copilot_version_banner_is_reduced_to_its_first_line() {
        // Real output on a working install: the version, then an upsell line.
        let banner = "GitHub Copilot CLI 1.0.84-3.\nRun 'copilot update' to check for updates.\n";
        assert_eq!(
            version_line(banner).as_deref(),
            Some("GitHub Copilot CLI 1.0.84-3")
        );
    }

    #[test]
    fn a_version_banner_cannot_smuggle_an_escape_sequence_into_the_doctor_report() {
        // The banner is printed raw by `cst doctor`, so a control byte would be
        // executed by the terminal rather than shown.
        let banner = "\u{1b}[2JGitHub Copilot CLI 1.0.84";
        let line = version_line(banner).expect("a version remains");
        assert!(!line.contains('\u{1b}'), "got {line:?}");
    }

    #[test]
    fn a_copilot_that_printed_nothing_yields_no_version_text() {
        assert_eq!(version_line(""), None);
        assert_eq!(version_line("\n  \n"), None);
    }
    #[test]
    fn the_autopilot_cap_reaches_copilot_on_new_and_resumed_sessions_alike() {
        // Copilot does not persist this one, so a cap passed only at creation would
        // quietly do nothing for every resume -- which is most launches.
        let capped = UserConfig {
            max_autopilot_continues: Some(25),
            ..UserConfig::default()
        };
        let policy = policy_of(&capped);

        let (new_args, _) = new_session_args(&capped, &policy);
        let resumed = resume_args("existing", &policy);

        assert!(
            new_args
                .iter()
                .any(|arg| arg == "--max-autopilot-continues=25"),
            "got {new_args:?}"
        );
        assert!(
            resumed
                .iter()
                .any(|arg| arg == "--max-autopilot-continues=25"),
            "got {resumed:?}"
        );
    }

    #[test]
    fn no_configured_cap_leaves_copilots_own_default_alone() {
        // Passing a number CST invented would pin the limit to whatever Copilot's
        // default happened to be when this was written.
        let (args, _) =
            new_session_args(&UserConfig::default(), &policy_of(&UserConfig::default()));
        assert!(
            !args
                .iter()
                .any(|arg| arg.starts_with("--max-autopilot-continues")),
            "got {args:?}"
        );
    }

    #[test]
    fn a_cap_of_zero_is_passed_rather_than_treated_as_unset() {
        // Copilot accepts 0, and it means something: never continue on its own.
        let never = UserConfig {
            max_autopilot_continues: Some(0),
            ..UserConfig::default()
        };
        let (args, _) = new_session_args(&never, &policy_of(&never));
        assert!(
            args.iter().any(|arg| arg == "--max-autopilot-continues=0"),
            "got {args:?}"
        );
    }

    #[test]
    fn a_project_can_set_its_own_autopilot_cap_over_the_global_one() {
        let global = UserConfig {
            max_autopilot_continues: Some(50),
            ..UserConfig::default()
        };

        let restrained = repo_with(Some(r#"{"max_autopilot_continues":2}"#));
        assert_eq!(
            LaunchPolicy::resolve(&global, restrained.path()).max_autopilot_continues,
            Some(2),
            "a repository that wants autopilot on a short leash must be able to say so"
        );

        let silent = repo_with(None);
        assert_eq!(
            LaunchPolicy::resolve(&global, silent.path()).max_autopilot_continues,
            Some(50),
            "a project with no opinion inherits"
        );

        let loose = tempfile::tempdir().unwrap();
        assert_eq!(
            LaunchPolicy::resolve(&global, loose.path()).max_autopilot_continues,
            Some(50),
            "and so does a directory that is not a project at all"
        );
    }
}

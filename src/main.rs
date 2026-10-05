mod app;
mod app_state;
mod changelog;
mod command_palette;
mod config;
mod debug_keys;
mod doctor;
mod editor;
mod events;
mod github;
mod hook_plugin;
mod host_terminal;
mod input;
mod links;
mod mux;
mod mux_input;
mod notifications;
mod paste;
mod scratchpad;
#[cfg(feature = "screenshots")]
mod screenshots;
mod session;
mod snippets;
mod terminal_pane;
mod text;
mod theme;
mod threads;
mod ui;
mod updater;
mod windows_terminal;
mod workspace_state;

use anyhow::{Context, Result};
use clap::{Parser, Subcommand};
use crossterm::{
    event::{
        DisableBracketedPaste, DisableFocusChange, DisableMouseCapture, EnableBracketedPaste,
        EnableFocusChange, EnableMouseCapture,
    },
    execute,
    terminal::{
        disable_raw_mode, enable_raw_mode, BeginSynchronizedUpdate, EndSynchronizedUpdate,
        EnterAlternateScreen, LeaveAlternateScreen, SetTitle,
    },
};
use ratatui::backend::CrosstermBackend;
use ratatui::Terminal;
use serde::{Deserialize, Serialize};
use std::ffi::OsString;
use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

use app::{App, NewSessionRequest};
use session::loader;
use session::manager;
use session::tmux;

#[derive(Parser)]
#[command(name = "copilot-session-tui")]
#[command(about = "A TUI for managing GitHub Copilot CLI sessions")]
#[command(version)]
struct Cli {
    /// Path to the Copilot config directory (default: ~/.copilot)
    #[arg(long)]
    copilot_home: Option<PathBuf>,

    /// Auto-filter to sessions from the current directory
    #[arg(
        long,
        default_value = "true",
        action = clap::ArgAction::Set,
        num_args = 0..=1,
        default_missing_value = "true"
    )]
    auto_filter: bool,

    /// Write the session's project directory to this file on exit (for shell cd wrapper)
    #[arg(long)]
    last_dir_file: Option<PathBuf>,

    /// Run sessions as panes inside CST for this invocation, overriding the `mux` config
    #[arg(long, overrides_with = "no_mux")]
    mux: bool,

    /// Launch sessions in the terminal and exit, overriding the `mux` config
    #[arg(long, overrides_with = "mux")]
    no_mux: bool,

    /// Open an exact session directly instead of showing the session list
    #[arg(long, value_name = "ID", conflicts_with = "open_favorites")]
    session: Option<String>,

    /// Open each inactive favorite in a Windows Terminal tab
    #[arg(long, conflicts_with = "session")]
    open_favorites: bool,

    /// Readiness gate used by the old CST process during an update handoff
    #[arg(
        long,
        value_name = "PATH",
        hide = true,
        requires = "restart_parent_pid"
    )]
    restart_gate: Option<PathBuf>,

    /// Parent CST process supervising an update handoff
    #[arg(long, value_name = "PID", hide = true, requires = "restart_gate")]
    restart_parent_pid: Option<u32>,

    /// Internal helper used to replace the installed executable without stopping panes
    #[arg(long, value_name = "VERSION", hide = true)]
    install_update_helper: Option<String>,

    #[command(subcommand)]
    command: Option<Commands>,
}

#[derive(Subcommand)]
enum Commands {
    /// Output shell integration script (add to your shell config for auto-cd on exit)
    Init {
        /// Shell type
        #[arg(value_parser = ["bash", "zsh", "powershell"])]
        shell: String,
    },
    /// Check that everything CST needs is installed and configured
    Doctor,
    /// Report how this terminal delivers key presses (used to pick a mux prefix key)
    #[command(hide = true)]
    DebugKeys,
    /// Check GitHub Releases and install the latest CST
    Update,
    /// Manage the optional Copilot lifecycle hook plugin
    Hooks {
        #[command(subcommand)]
        action: HookPluginCommand,
    },
    /// Take part in a GitHub thread and be woken when it moves
    Thread {
        #[command(subcommand)]
        action: ThreadCommand,
    },
    /// Receive one lifecycle event from the Copilot hook plugin
    #[command(hide = true)]
    HookEvent {
        #[arg(value_name = "EVENT")]
        event: String,
    },
    /// Regenerate the README screenshots from invented sessions
    #[cfg(feature = "screenshots")]
    #[command(hide = true)]
    Screenshots {
        /// Directory to write the SVG files into
        #[arg(default_value = "docs/img")]
        out_dir: PathBuf,
    },
}

#[derive(Subcommand)]
enum ThreadCommand {
    /// Watch a thread without posting to it
    Watch {
        /// URL of a GitHub issue, pull request, or discussion
        url: String,
        /// Which session to act for, when not running inside one
        #[arg(long, value_name = "ID")]
        session: Option<String>,
    },
    /// Comment on a thread and start watching it
    Post {
        /// URL of a GitHub issue, pull request, or discussion
        url: String,
        /// Comment text
        #[arg(long, conflicts_with = "body_file")]
        body: Option<String>,
        /// Read the comment text from a file, or from stdin with `-`
        #[arg(long, value_name = "PATH")]
        body_file: Option<String>,
        /// Which session to act for, when not running inside one
        #[arg(long, value_name = "ID")]
        session: Option<String>,
    },
    /// End a correspondence for every session watching it
    Close {
        /// URL of a GitHub issue, pull request, or discussion
        url: String,
    },
    /// Stop being woken by a thread, leaving other sessions watching it
    Leave {
        /// URL of a GitHub issue, pull request, or discussion
        url: String,
        /// Which session to act for, when not running inside one
        #[arg(long, value_name = "ID")]
        session: Option<String>,
    },
    /// Show what this session is watching
    List {
        /// Which session to act for, when not running inside one
        #[arg(long, value_name = "ID")]
        session: Option<String>,
    },
}

#[derive(Subcommand)]
enum HookPluginCommand {
    /// Install or refresh authoritative Copilot lifecycle reporting
    Install,
    /// Show whether the lifecycle plugin is installed
    Status,
    /// Remove the lifecycle plugin
    Uninstall,
}

#[derive(Debug, Default, Serialize, Deserialize)]
struct RestartManifest {
    panes: Vec<RestartManifestPane>,
    focused_session_id: Option<String>,
}

#[derive(Debug, Serialize, Deserialize)]
struct RestartManifestPane {
    session_id: String,
    cwd: PathBuf,
}

fn main() -> Result<()> {
    let cli = Cli::parse();
    let copilot_home = cli
        .copilot_home
        .clone()
        .unwrap_or_else(loader::copilot_home);
    if cli.copilot_home.is_some() {
        std::env::set_var("COPILOT_HOME", &copilot_home);
    }
    if let Some(Commands::HookEvent { event }) = &cli.command {
        let command = events::hooks::HookCommand::parse(event)
            .with_context(|| format!("Unknown CST lifecycle hook event: {event}"))?;
        events::hooks::record_stdin(command, &copilot_home)?;
        return Ok(());
    }

    let restart_manifest = match (cli.restart_gate.as_deref(), cli.restart_parent_pid) {
        (Some(gate), Some(parent_pid)) => {
            let Some(manifest) = wait_for_restart_release(gate, parent_pid)? else {
                return Ok(());
            };
            manifest
        }
        _ => RestartManifest::default(),
    };

    if let Some(version) = cli.install_update_helper.as_deref() {
        updater::install_update_helper(version)?;
        return Ok(());
    }
    updater::cleanup_old_update_files();

    // Handle subcommands
    match &cli.command {
        Some(Commands::Init { shell }) => {
            print_shell_init(shell);
            return Ok(());
        }
        Some(Commands::DebugKeys) => {
            return debug_keys::run();
        }
        Some(Commands::Update) => {
            match updater::update_now()? {
                updater::UpdateCommandOutcome::Current(version) => {
                    println!("CST v{version} is already current.");
                }
                updater::UpdateCommandOutcome::Installed(version) => {
                    println!("Updated CST to v{version}.");
                }
                updater::UpdateCommandOutcome::AlreadyInstalled(version) => {
                    println!("CST v{version} is already installed.");
                }
            }
            return Ok(());
        }
        Some(Commands::Doctor) => {
            let report = doctor::gather(&copilot_home);
            let mut stdout = io::stdout();
            doctor::render(&report, &mut stdout)?;
            // `process::exit` skips the flush that dropping stdout would do, and the
            // whole point of this command is the text.
            stdout.flush()?;
            if report.has_required_failure() {
                // Deliberately an exit code rather than an `Err`: the report is already
                // a complete explanation, and returning an error would print `Error: …`
                // underneath it. Safe here because this runs before raw mode, the
                // alternate screen, and any PTY or lock.
                std::process::exit(1);
            }
            return Ok(());
        }
        Some(Commands::Hooks { action }) => {
            match action {
                HookPluginCommand::Install => {
                    hook_plugin::install(&copilot_home)?;
                    println!(
                        "Installed CST lifecycle hooks. Restart active Copilot sessions to load them."
                    );
                }
                HookPluginCommand::Status => match hook_plugin::status(&copilot_home)? {
                    hook_plugin::PluginStatus::Installed => {
                        println!("CST lifecycle hooks are installed.");
                    }
                    hook_plugin::PluginStatus::NotInstalled => {
                        println!("CST lifecycle hooks are not installed.");
                    }
                },
                HookPluginCommand::Uninstall => {
                    hook_plugin::uninstall(&copilot_home)?;
                    println!(
                        "Uninstalled CST lifecycle hooks. Restart active Copilot sessions to unload them."
                    );
                }
            }
            return Ok(());
        }
        Some(Commands::Thread { action }) => {
            let root = threads::cli::root();
            let report = match action {
                ThreadCommand::Watch { url, session } => {
                    let session_id = threads::cli::resolve_session(session.as_deref())?;
                    threads::cli::watch(&root, &session_id, url)?
                }
                ThreadCommand::Post {
                    url,
                    body,
                    body_file,
                    session,
                } => {
                    let session_id = threads::cli::resolve_session(session.as_deref())?;
                    let text = threads::cli::read_body(body.as_deref(), body_file.as_deref())?;
                    threads::cli::post(&root, &session_id, url, &text)?
                }
                ThreadCommand::Close { url } => threads::cli::close(&root, url)?,
                ThreadCommand::Leave { url, session } => {
                    let session_id = threads::cli::resolve_session(session.as_deref())?;
                    threads::cli::leave(&root, &session_id, url)?
                }
                ThreadCommand::List { session } => {
                    let session_id = threads::cli::resolve_session(session.as_deref())?;
                    threads::cli::list(&root, &session_id)?
                }
            };
            let mut stdout = std::io::stdout();
            threads::cli::emit(&mut stdout, &report)?;
            stdout.flush()?;
            return Ok(());
        }
        Some(Commands::HookEvent { .. }) => unreachable!("handled before normal startup"),
        #[cfg(feature = "screenshots")]
        Some(Commands::Screenshots { out_dir }) => {
            return screenshots::run(out_dir);
        }
        None => {}
    }

    let hook_refresh_notice = match hook_plugin::refresh_if_needed(&copilot_home) {
        Ok(hook_plugin::RefreshOutcome::Refreshed) => Some(format!(
            "CST lifecycle hooks refreshed for v{}; restart other active Copilot sessions to load them.",
            env!("CARGO_PKG_VERSION")
        )),
        Ok(
            hook_plugin::RefreshOutcome::NotManaged
            | hook_plugin::RefreshOutcome::Current
            | hook_plugin::RefreshOutcome::Removed,
        ) => None,
        Err(error) => Some(format!(
            "Could not refresh installed CST lifecycle hooks: {error}. Run `cst hooks install` to retry."
        )),
    };

    // The path controls both CST's session loader and the Copilot process it resumes.
    // Generated favorite tabs receive an explicit path before any child processes start.

    let mut user_config = config::load();
    // CLI flags win for this invocation only; they are never written back to disk.
    let mux_on_disk = user_config.mux;
    if cli.mux {
        user_config.mux = true;
    } else if cli.no_mux {
        user_config.mux = false;
    }

    // Direct resume is an exact id and favorites are already ids, so neither path needs
    // to enumerate the complete catalog. Normal startup paints favorites, then discovers
    // the rest in the background.
    let mut session_load_receiver = None;
    let sessions = if let Some(session_id) = cli.session.as_deref() {
        vec![loader::load_session(&copilot_home, session_id)
            .context("Failed to load Copilot session")?
            .with_context(|| format!("Session '{session_id}' was not found"))?]
    } else if !restart_manifest.panes.is_empty() {
        session_load_receiver = Some(loader::load_sessions_async(copilot_home.clone()));
        let session_ids = restart_manifest
            .panes
            .iter()
            .map(|pane| pane.session_id.clone())
            .collect::<Vec<_>>();
        loader::load_sessions_by_ids(&copilot_home, &session_ids)
            .context("Failed to load sessions after updating CST")?
    } else {
        let favorites = match loader::load_sessions_by_ids(&copilot_home, &user_config.favorites) {
            Ok(sessions) => sessions,
            Err(error) if cli.open_favorites => {
                return Err(error).context("Failed to load favorite Copilot sessions");
            }
            Err(_) => Vec::new(),
        };
        if !cli.open_favorites {
            session_load_receiver = Some(loader::load_sessions_async(copilot_home.clone()));
        }
        favorites
    };
    let startup_session = cli
        .session
        .as_deref()
        .map(|id| resolve_startup_session(&sessions, id))
        .transpose()?;
    let restored_startup_sessions = restart_manifest
        .panes
        .iter()
        .map(|pane| resolve_restored_startup_session(&sessions, &pane.session_id, &pane.cwd))
        .collect::<Vec<_>>();

    if cli.open_favorites {
        if let Some(notice) = hook_refresh_notice.as_deref() {
            eprintln!("{notice}");
        }
        let mux_override = if cli.mux {
            Some(true)
        } else if cli.no_mux {
            Some(false)
        } else {
            None
        };
        return windows_terminal::open_favorites(
            &sessions,
            &user_config,
            &copilot_home,
            mux_override,
        );
    }

    if let Some(session) = startup_session.as_ref().filter(|_| !user_config.mux) {
        if let Some(notice) = hook_refresh_notice.as_deref() {
            eprintln!("{notice}");
        }
        if let Some(reference) = tmux::find_live(&session.id)? {
            eprintln!(
                "Attaching persistent tmux session '{}'...",
                reference.tmux_session
            );
            manager::attach_tmux_session(&reference)?;
        } else {
            eprintln!(
                "Resuming session {} in {}...",
                short_session_id(&session.id),
                session.cwd
            );
            manager::resume_session(&session.id, &session.cwd, &user_config)?;
        }
        if let Some(path) = &cli.last_dir_file {
            let _ = std::fs::write(path, &session.cwd);
        }
        return Ok(());
    }

    let mut app = App::new(sessions, user_config);
    // Deliberately here rather than earlier. Every subcommand and the direct
    // `--session` resume return before this point, and none of them shows a UI, so
    // none of them may burn the notes by advancing the marker without displaying
    // anything. A `--restart-gate` child does reach this line, which is the case that
    // matters most: it is the process that came up after an update.
    if let Some(whats_new) = changelog::whats_new_on_startup() {
        app.whats_new = Some(ui::whats_new::WhatsNewScreen::new(whats_new));
    }
    // Anything a previous run planned but never got into a composer is picked back up,
    // and anything held while CST was closed is still held — so the list shows both from
    // the first frame rather than only after the next comment arrives. The comment behind
    // a notice is marked seen as soon as it is planned, so a notice dropped here would be
    // a message nobody is ever told about again.
    if let Err(error) = threads::store::resume_in(&app.thread_state_root) {
        eprintln!("Could not resume thread notices: {error}");
    }
    app.refresh_thread_pending();
    app.mux_on_disk = mux_on_disk;
    app.copilot_home = copilot_home;
    if let Some(receiver) = session_load_receiver {
        app.begin_session_load(receiver);
    }

    // Start background update check
    app.update_receiver = Some(updater::check_for_updates_async());

    // Resolving the Copilot binary costs ~400ms; do it off-thread so the first session
    // does not pay for it on the UI thread.
    manager::warm_copilot_lookup();

    // A direct session should detach into its own project list even when invoked from
    // elsewhere. Normal startup continues to use the process launch directory.
    let cwd_context = startup_session
        .as_ref()
        .map(|session| existing_or_current_dir(&session.cwd))
        .or_else(|| {
            let focused = restart_manifest.focused_session_id.as_deref();
            restored_startup_sessions
                .iter()
                .find(|session| Some(session.id.as_str()) == focused)
                .or_else(|| restored_startup_sessions.last())
                .map(|session| existing_or_current_dir(&session.cwd))
        })
        .or_else(|| {
            std::env::current_dir()
                .ok()
                .map(|cwd| cwd.to_string_lossy().to_string())
        });
    if let Some(cwd) = cwd_context {
        app.set_cwd_context(cwd, cli.auto_filter);
    }

    if let Some(session) = startup_session {
        let cwd = existing_or_current_dir(&session.cwd);
        app.attach_session(&session.id, &cwd, session.title)?;
        mux_input::sync_workspace_panels(&mut app);
    }
    if !restored_startup_sessions.is_empty() {
        for session in &restored_startup_sessions {
            let cwd = existing_or_current_dir(&session.cwd);
            app.attach_session(&session.id, &cwd, session.title.clone())?;
            if restart_startup_aborted(&cli) {
                return Ok(());
            }
        }
        if let Some(session_id) = restart_manifest.focused_session_id.as_deref() {
            if let Some(pane_id) = app
                .mux
                .as_ref()
                .and_then(|mux| mux.pane_for_session(session_id))
            {
                if let Some(mux) = app.mux.as_mut() {
                    mux.focused = Some(pane_id);
                }
                app.view = app::View::Attached(pane_id);
            }
        }
        mux_input::sync_workspace_panels(&mut app);
    }
    if restart_startup_aborted(&cli) {
        return Ok(());
    }
    if let Some(notice) = hook_refresh_notice {
        app.status_message = Some(notice);
    }

    loop {
        let Some(mut restart) = run_tui_cycle(&mut app, &cli)? else {
            break;
        };
        match restart.release_until_started() {
            Ok(()) => {
                drop(app);
                return restart.wait();
            }
            Err(error) => {
                drop(restart);
                app.recover_update_restart_sessions(&format!(
                    "the updated CST failed before opening its workspace: {error}"
                ));
                mux_input::sync_workspace_panels(&mut app);
            }
        }
    }

    // Track the directory to write to --last-dir-file
    let mut last_dir: Option<String> = None;

    if let Some(reference) = app.should_attach_tmux.take() {
        last_dir = Some(reference.cwd.to_string_lossy().to_string());
        manager::attach_tmux_session(&reference)?;
    }

    // Resume session if requested
    if let Some((session_id, cwd)) = app.should_resume {
        eprintln!(
            "Resuming session {} in {}...",
            short_session_id(&session_id),
            &cwd
        );
        last_dir = Some(cwd.clone());
        manager::resume_session(&session_id, &cwd, &app.config)?;
    }

    // Start new session if requested
    if let Some(request) = app.should_new_session.take() {
        match request {
            NewSessionRequest::Normal { cwd } => {
                eprintln!("Starting new session in {}...", &cwd);
                last_dir = Some(cwd.clone());
                manager::start_new_session(&cwd, &app.config)?;
            }
            NewSessionRequest::Tmux { cwd, title } => {
                eprintln!("Starting persistent tmux session in {}...", cwd);
                let reference = manager::start_tmux_session(Path::new(&cwd), &title, &app.config)?;
                last_dir = Some(cwd);
                manager::attach_tmux_session(&reference)?;
            }
            NewSessionRequest::TmuxResume {
                session_id,
                cwd,
                title,
            } => {
                eprintln!(
                    "Resuming session {} in persistent tmux...",
                    short_session_id(&session_id)
                );
                let reference = manager::resume_tmux_session(
                    &session_id,
                    Path::new(&cwd),
                    &title,
                    &app.config,
                )?;
                last_dir = Some(cwd);
                manager::attach_tmux_session(&reference)?;
            }
            NewSessionRequest::Worktree {
                source_project,
                branch,
                config,
            } => {
                let worktree = manager::start_worktree_session(
                    &source_project,
                    &branch,
                    &config,
                    &app.config,
                )?;
                last_dir = Some(worktree.to_string_lossy().to_string());
            }
            NewSessionRequest::TmuxWorktree {
                source_project,
                branch,
                config,
            } => {
                let reference = manager::start_worktree_tmux_session(
                    &source_project,
                    &branch,
                    &config,
                    &app.config,
                )?;
                last_dir = Some(reference.cwd.to_string_lossy().to_string());
                manager::attach_tmux_session(&reference)?;
            }
        }
    }

    // If user quit without entering a session but has an active project filter, use that
    // (skip when the filter is just the project we were launched from — no cd needed)
    if last_dir.is_none() {
        // In mux mode the session ran inside CST, so the last focused pane's directory is
        // the most useful place for the shell wrapper to land.
        last_dir = app.exit_dir.take();
    }
    if last_dir.is_none() {
        if let Some(ref project) = app.project_filter {
            let is_launch_project = app
                .cwd_project
                .as_ref()
                .is_some_and(|p| p.eq_ignore_ascii_case(project));
            if !is_launch_project {
                last_dir = Some(project.clone());
            }
        }
    }

    // Write last directory to file if --last-dir-file was provided
    if let (Some(ref path), Some(ref dir)) = (&cli.last_dir_file, &last_dir) {
        let _ = std::fs::write(path, dir);
    }
    Ok(())
}

fn run_tui_cycle(app: &mut App, cli: &Cli) -> Result<Option<PreparedRestart>> {
    enable_raw_mode()?;
    let mut stdout = io::stdout();
    execute!(
        stdout,
        EnterAlternateScreen,
        EnableMouseCapture,
        EnableBracketedPaste,
        EnableFocusChange
    )?;
    let backend = CrosstermBackend::new(stdout);
    let mut terminal = Terminal::new(backend)?;
    if let Some(gate) = cli.restart_gate.as_deref() {
        std::fs::write(restart_gate_path(gate, "started"), b"started")
            .context("Failed to confirm the updated CST workspace started")?;
    }

    let result = run_app(&mut terminal, app, cli);
    app.wait_for_update_install();

    disable_raw_mode()?;
    terminal
        .backend_mut()
        .write_all(host_terminal::CLEAR_PROGRESS)?;
    terminal.backend_mut().flush()?;
    execute!(
        terminal.backend_mut(),
        LeaveAlternateScreen,
        DisableMouseCapture,
        DisableBracketedPaste,
        DisableFocusChange,
        SetTitle("")
    )?;
    terminal.show_cursor()?;
    app.terminal.shutdown()?;
    result
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct StartupSession {
    id: String,
    cwd: String,
    title: String,
}

fn resolve_startup_session(sessions: &[session::Session], id: &str) -> Result<StartupSession> {
    let session = sessions
        .iter()
        .find(|session| session.id == id)
        .with_context(|| format!("Session '{id}' was not found"))?;
    if session.is_active {
        anyhow::bail!("Session '{}' is already active", session.display_name());
    }
    Ok(StartupSession {
        id: session.id.clone(),
        cwd: session.cwd.clone(),
        title: session.display_name().to_string(),
    })
}

fn resolve_restored_startup_session(
    sessions: &[session::Session],
    id: &str,
    fallback_cwd: &Path,
) -> StartupSession {
    sessions
        .iter()
        .find(|session| session.id == id)
        .map(|session| StartupSession {
            id: session.id.clone(),
            cwd: session.cwd.clone(),
            title: session.display_name().to_string(),
        })
        .unwrap_or_else(|| StartupSession {
            id: id.to_string(),
            cwd: fallback_cwd.to_string_lossy().to_string(),
            title: format!("session {}", short_session_id(id)),
        })
}

fn short_session_id(id: &str) -> &str {
    id.get(..8).unwrap_or(id)
}

fn existing_or_current_dir(cwd: &str) -> String {
    if Path::new(cwd).is_dir() {
        return cwd.to_string();
    }
    std::env::current_dir()
        .map(|path| path.to_string_lossy().to_string())
        .unwrap_or_else(|_| cwd.to_string())
}

fn restart_startup_aborted(cli: &Cli) -> bool {
    cli.restart_gate
        .as_deref()
        .is_some_and(|gate| restart_gate_path(gate, "abort").is_file())
}

struct PreparedRestart {
    child: Option<std::process::Child>,
    gate: tempfile::TempDir,
    released: bool,
}

impl PreparedRestart {
    fn write_manifest(&self, request: &app::UpdateRestartRequest) -> Result<()> {
        let manifest = RestartManifest {
            panes: request
                .panes
                .iter()
                .filter(|pane| pane.copilot_running)
                .map(|pane| RestartManifestPane {
                    session_id: pane.session_id.clone(),
                    cwd: pane.cwd.clone(),
                })
                .collect(),
            focused_session_id: request.focused_session_id.clone().filter(|session_id| {
                request
                    .panes
                    .iter()
                    .any(|pane| pane.copilot_running && &pane.session_id == session_id)
            }),
        };
        let path = restart_gate_path(self.gate.path(), "manifest");
        let temporary = restart_gate_path(self.gate.path(), "manifest.tmp");
        std::fs::write(&temporary, serde_json::to_vec(&manifest)?)
            .context("Failed to write the finalized CST restart manifest")?;
        std::fs::rename(&temporary, &path)
            .context("Failed to publish the finalized CST restart manifest")
    }

    fn release_until_started(&mut self) -> Result<()> {
        std::fs::write(restart_gate_path(self.gate.path(), "go"), b"go")
            .context("Failed to release the updated CST process")?;
        self.released = true;
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(15);
        while std::time::Instant::now() < deadline {
            if restart_gate_path(self.gate.path(), "started").is_file() {
                return Ok(());
            }
            if let Some(status) = self
                .child
                .as_mut()
                .context("Updated CST process is unavailable")?
                .try_wait()
                .context("Failed to inspect the updated CST process")?
            {
                anyhow::bail!("Updated CST exited during startup with status {status}");
            }
            std::thread::sleep(std::time::Duration::from_millis(25));
        }
        anyhow::bail!("Updated CST did not open its workspace within 15 seconds")
    }

    fn wait(mut self) -> Result<()> {
        let status = self
            .child
            .take()
            .context("Updated CST process is unavailable")?
            .wait()
            .context("Failed to wait for the updated CST process")?;
        self.cleanup();
        if !status.success() {
            anyhow::bail!("Updated CST exited with status {status}");
        }
        Ok(())
    }

    fn cleanup(&self) {
        for suffix in [
            "ready",
            "go",
            "abort",
            "started",
            "manifest",
            "manifest.tmp",
        ] {
            let _ = std::fs::remove_file(restart_gate_path(self.gate.path(), suffix));
        }
    }
}

impl Drop for PreparedRestart {
    fn drop(&mut self) {
        if let Some(mut child) = self.child.take() {
            let _ = std::fs::write(restart_gate_path(self.gate.path(), "abort"), b"abort");
            if self.released {
                let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
                while std::time::Instant::now() < deadline {
                    if child.try_wait().ok().flatten().is_some() {
                        self.cleanup();
                        return;
                    }
                    std::thread::sleep(std::time::Duration::from_millis(25));
                }
            }
            let _ = child.kill();
            let _ = child.wait();
        }
        self.cleanup();
    }
}

fn prepare_updated_cst(
    cli: &Cli,
    copilot_home: &Path,
    mux_enabled: bool,
) -> Result<PreparedRestart> {
    let executable = updater::invocation_executable()?;
    let gate = tempfile::Builder::new()
        .prefix("cst-restart-")
        .tempdir()
        .context("Failed to create a private restart handoff directory")?;
    let mut args = restart_arguments(cli, copilot_home, mux_enabled);
    args.push(OsString::from("--restart-gate"));
    args.push(gate.path().as_os_str().to_owned());
    args.push(OsString::from("--restart-parent-pid"));
    args.push(OsString::from(std::process::id().to_string()));
    let child = std::process::Command::new(&executable)
        .args(args)
        .spawn()
        .with_context(|| format!("Failed to restart updated CST {}", executable.display()))?;
    let mut prepared = PreparedRestart {
        child: Some(child),
        gate,
        released: false,
    };
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
    while std::time::Instant::now() < deadline {
        if restart_gate_path(prepared.gate.path(), "ready").is_file() {
            return Ok(prepared);
        }
        if let Some(status) = prepared
            .child
            .as_mut()
            .expect("prepared child")
            .try_wait()
            .context("Failed to inspect the updated CST process")?
        {
            anyhow::bail!("Updated CST exited before handoff with status {status}");
        }
        std::thread::sleep(std::time::Duration::from_millis(25));
    }
    anyhow::bail!("Updated CST did not become ready for restart within 5 seconds")
}

fn restart_gate_path(gate: &Path, suffix: &str) -> PathBuf {
    gate.join(suffix)
}

fn wait_for_restart_release(gate: &Path, parent_pid: u32) -> Result<Option<RestartManifest>> {
    let manifest_path = restart_gate_path(gate, "manifest");
    let ready = restart_gate_path(gate, "ready");
    std::fs::write(&ready, b"ready")
        .with_context(|| format!("Failed to signal update readiness at {}", ready.display()))?;
    loop {
        let go = restart_gate_path(gate, "go");
        if go.is_file() {
            let manifest =
                serde_json::from_slice(&std::fs::read(&manifest_path).with_context(|| {
                    format!(
                        "Failed to read restart manifest {}",
                        manifest_path.display()
                    )
                })?)
                .context("Failed to parse the restart manifest")?;
            let _ = std::fs::remove_file(go);
            let _ = std::fs::remove_file(ready);
            let _ = std::fs::remove_file(&manifest_path);
            return Ok(Some(manifest));
        }
        if restart_gate_path(gate, "abort").is_file()
            || !session::process::process_is_running(parent_pid)
        {
            let _ = std::fs::remove_file(ready);
            return Ok(None);
        }
        std::thread::sleep(std::time::Duration::from_millis(25));
    }
}

fn restart_arguments(cli: &Cli, copilot_home: &Path, mux_enabled: bool) -> Vec<OsString> {
    let mut args = vec![
        OsString::from("--copilot-home"),
        copilot_home.as_os_str().to_owned(),
        OsString::from("--auto-filter"),
        OsString::from(cli.auto_filter.to_string()),
        OsString::from(if mux_enabled { "--mux" } else { "--no-mux" }),
    ];
    if let Some(path) = cli.last_dir_file.as_ref() {
        args.push(OsString::from("--last-dir-file"));
        args.push(path.as_os_str().to_owned());
    }
    args
}

struct ConfigWatcher {
    stop: Arc<AtomicBool>,
    worker: Option<std::thread::JoinHandle<()>>,
}

impl ConfigWatcher {
    fn start(
        events: std::sync::mpsc::Sender<mux::MuxEvent>,
        path: PathBuf,
        applied: std::sync::Arc<std::sync::Mutex<Option<config::ConfigRevision>>>,
        interval: std::time::Duration,
    ) -> Self {
        let stop = Arc::new(AtomicBool::new(false));
        let worker_stop = Arc::clone(&stop);
        let worker = std::thread::spawn(move || loop {
            std::thread::park_timeout(interval);
            if worker_stop.load(Ordering::Acquire) {
                return;
            }
            let Ok(next) = config::config_revision(&path) else {
                continue;
            };
            if applied
                .lock()
                .ok()
                .is_some_and(|revision| *revision == Some(next))
            {
                continue;
            }
            if events.send(mux::MuxEvent::ConfigChanged).is_err() {
                return;
            }
        });
        Self {
            stop,
            worker: Some(worker),
        }
    }
}

impl Drop for ConfigWatcher {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Release);
        if let Some(worker) = self.worker.take() {
            worker.thread().unpark();
            let _ = worker.join();
        }
    }
}

struct TerminalEventReader {
    stop: Arc<AtomicBool>,
    worker: Option<std::thread::JoinHandle<()>>,
}

impl TerminalEventReader {
    fn start(sender: std::sync::mpsc::Sender<mux::MuxEvent>) -> Self {
        let stop = Arc::new(AtomicBool::new(false));
        let worker_stop = Arc::clone(&stop);
        let worker = std::thread::spawn(move || {
            while !worker_stop.load(Ordering::Acquire) {
                match crossterm::event::poll(std::time::Duration::from_millis(100)) {
                    Ok(false) => continue,
                    Ok(true) => match paste::read_events() {
                        Ok(events) => {
                            for event in events {
                                if sender.send(mux::MuxEvent::Term(event)).is_err() {
                                    return;
                                }
                            }
                        }
                        Err(_) => return,
                    },
                    Err(_) => return,
                }
            }
        });
        Self {
            stop,
            worker: Some(worker),
        }
    }
}

impl Drop for TerminalEventReader {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Release);
        if let Some(worker) = self.worker.take() {
            let _ = worker.join();
        }
    }
}

fn run_app(
    terminal: &mut Terminal<CrosstermBackend<io::Stdout>>,
    app: &mut App,
    cli: &Cli,
) -> Result<Option<PreparedRestart>> {
    let config_path = config::config_path();
    // The watcher compares against the revision actually applied by App, so a quick
    // A -> B -> A reversion cannot be hidden by a stale watcher-local baseline.
    app.request_config_reload();
    let _config_watcher = app.mux.as_ref().map(|mux| {
        ConfigWatcher::start(
            mux.events.clone(),
            config_path.clone(),
            app.config_revision_handle(),
            std::time::Duration::from_secs(1),
        )
    });
    // Started here rather than at construction so it only exists when there are panes to
    // wake. The login is read once: it is what decides whether a comment came from one
    // of our own agents, and asking GitHub every poll would be a round trip for an
    // answer that does not change.
    let _thread_watcher = app
        .mux
        .as_ref()
        .filter(|_| app.config.threads_enabled)
        .and_then(|mux| {
            let login = threads::doorbell::current_login("github.com").ok()?;
            Some(threads::watcher::ThreadWatcher::start(
                mux.events.clone(),
                threads::store::state_root(),
                login,
                threads::watcher::WatchSettings {
                    poll_interval: std::time::Duration::from_secs(
                        threads::doorbell::DEFAULT_POLL_SECONDS,
                    ),
                    stall_detection: app.config.thread_stall_detection,
                    trusted_authors: app.config.thread_trusted_authors.clone(),
                    wakeups_per_hour: app
                        .config
                        .thread_wakeups_per_hour
                        .unwrap_or(threads::DEFAULT_WAKEUPS_PER_HOUR),
                },
            ))
        });
    let mut last_non_mux_config_check = std::time::Instant::now();
    // In mux mode a dedicated thread feeds terminal events into the same channel as PTY
    // output, so the loop can wait on both at once instead of polling. Reading through
    // `paste` rebuilds the paste events Windows delivers as bare keystrokes; because the
    // thread does nothing but read, a queued burst can only have come from a paste.
    let _terminal_events = app
        .mux
        .as_ref()
        .map(|mux| TerminalEventReader::start(mux.events.clone()));
    let mut terminal_title = String::new();
    let mut repaint = true;
    let mut last_paint = std::time::Instant::now();
    let mut prepared_restart = None;

    loop {
        repaint |= app.poll_config_reload();
        if app.mux.is_none()
            && last_non_mux_config_check.elapsed() >= std::time::Duration::from_secs(1)
        {
            if let Ok(next) = config::config_revision(&config_path) {
                if !app.config_revision_is_applied(next) {
                    repaint |= app.request_config_reload();
                }
            }
            last_non_mux_config_check = std::time::Instant::now();
        }
        app.collapse_stopped_terminals();

        let desired_title = desired_terminal_title(app);
        if terminal_title != desired_title {
            terminal_title.clear();
            terminal_title.push_str(&desired_title);
            execute!(terminal.backend_mut(), SetTitle(&terminal_title))?;
        }

        let size = terminal.size()?;
        let attached_layout = matches!(app.view, app::View::Attached(_)).then(|| {
            ui::attached_layout(
                ratatui::layout::Rect::new(0, 0, size.width, size.height),
                app.attached_scratchpad_visible(),
                app.attached_terminal_visible(),
                app.tab_bar_visible(),
            )
        });
        update_layout_metrics(app, size.height);

        if let Some(layout) = attached_layout {
            app.workspace_areas = app::WorkspaceAreas {
                chat: layout.chat,
                tabs: layout.tabs,
                scratchpad: layout.scratchpad,
                terminal: layout.terminal,
            };
            if let (Some(area), Some(terminal_pane)) = (layout.terminal, app.terminal.active_mut())
            {
                if let Err(error) = terminal_pane.resize(
                    area.x.saturating_add(1),
                    area.y.saturating_add(1),
                    area.height.saturating_sub(2),
                    area.width.saturating_sub(2),
                ) {
                    app.status_message = Some(format!("Terminal resize failed: {error}"));
                }
            }

            let chat = layout.chat_pane();
            let rows = chat.height.max(1);
            let cols = chat.width.max(1);
            if app.pane_size != (rows, cols) || app.pane_origin != (chat.x, chat.y) {
                app.pane_size = (rows, cols);
                app.pane_origin = (chat.x, chat.y);
                if let Some(mux) = app.mux.as_mut() {
                    mux.resize_all_at(chat.x, chat.y, rows, cols);
                }
            }
        } else {
            if app.mux.is_some() {
                let rows = size.height.saturating_sub(1).max(1);
                let cols = size.width.max(1);
                if app.pane_size != (rows, cols) || app.pane_origin != (0, 0) {
                    app.pane_size = (rows, cols);
                    app.pane_origin = (0, 0);
                    if let Some(mux) = app.mux.as_mut() {
                        mux.resize_all(rows, cols);
                    }
                }
            }
        }

        input::maybe_load_details(app);
        app.poll_session_load();
        app.poll_external_sessions();
        repaint |= app.refresh_tmux_sessions();
        app.poll_update();
        app.poll_notifications();
        app.poll_github();
        // Also on the tick, not only when a pane reports a turn ending: a wake-up can be
        // waiting on a half-typed message, and a pane holding an abandoned draft emits
        // no events at all. Cheap — the queue is almost always empty.
        repaint |= app.flush_thread_wakes();
        // Covers every route into the Files tab — keys, mouse, or opening
        // straight onto it — rather than each one separately.
        if app.github_files_tab_active() {
            app.ensure_github_patches();
        }
        if matches!(app.view, app::View::Attached(_)) {
            app.refresh_github_references();
        }

        if let Some(scratchpad) = app.scratchpad.as_mut() {
            if let Err(error) = scratchpad.autosave_if_due() {
                scratchpad.status_message = Some(format!("Autosave failed: {error}"));
            }
        }

        let mut host_sequences = app.terminal.drain_host_sequences();
        host_sequences.append(&mut app.host_sequences);
        if !host_sequences.is_empty() {
            for sequence in host_sequences {
                terminal.backend_mut().write_all(&sequence)?;
            }
            terminal.backend_mut().flush()?;
        }

        if mux_paint_due(app, repaint, last_paint) {
            draw_frame(terminal, app)?;
            last_paint = std::time::Instant::now();
        }

        // Runs after the draw above so the "creating worktree…" notice is already on
        // screen before this blocks on Git.
        if let Some(pending) = app.pending_worktree.take() {
            input::run_pending_worktree(app, pending);
            continue;
        }

        // Same contract: the tmux server start and its startup grace block for long
        // enough that the launch notice must be painted first.
        if let Some(pending) = app.pending_tmux_launch.take() {
            input::run_pending_tmux_launch(app, pending);
            continue;
        }

        if app.mux.is_some() {
            repaint = pump_mux(app)?;
        } else {
            input::handle_input(app)?;
            repaint = true;
        }

        if exit_waits_for_update(app) {
            app.status_message =
                Some("Finishing the update before leaving; running sessions stay open...".into());
            continue;
        }
        let explicit_exit_requested = prioritize_explicit_exit(app);
        if !explicit_exit_requested
            && app.update_restart_ready()
            && app.update_restart_deferred_by_editor()
        {
            app.note_deferred_update_restart();
            continue;
        }
        let exit_requested = explicit_exit_requested || app.update_restart_ready();
        if exit_requested && app.exit_waits_for_notifications() {
            continue;
        }

        if app.update_restart_ready() {
            if !app.validate_update_restart_sessions() {
                continue;
            }
            if let Some(scratchpad) = app.scratchpad.as_mut() {
                if let Err(error) = scratchpad.save() {
                    app.cancel_update_restart_after_failure(format!(
                        "Update installed, but restart was cancelled because the scratchpad \
                         could not be saved: {error}"
                    ));
                    continue;
                }
            }
            let prepared = match prepare_updated_cst(cli, &app.copilot_home, app.mux.is_some()) {
                Ok(prepared) => prepared,
                Err(error) => {
                    app.cancel_update_restart_after_failure(format!(
                        "Update installed, but the new CST could not start: {error}"
                    ));
                    continue;
                }
            };
            if let Err(error) = app.terminal.shutdown() {
                drop(prepared);
                app.cancel_update_restart_after_failure(format!(
                    "Update installed, but restart was cancelled because terminal shells could \
                     not stop: {error}"
                ));
                continue;
            }
            let terminated = match app
                .mux
                .as_mut()
                .map(|mux| mux.shutdown_with_report())
                .transpose()
            {
                Ok(terminated) => terminated.unwrap_or_default(),
                Err(error) => {
                    drop(prepared);
                    app.recover_update_restart_sessions(&error.to_string());
                    mux_input::sync_workspace_panels(app);
                    continue;
                }
            };
            app.retain_terminated_restart_panes(&terminated);
            if let Err(error) = prepared.write_manifest(
                app.restart_after_update
                    .as_ref()
                    .expect("ready restart has a finalized request"),
            ) {
                drop(prepared);
                app.recover_update_restart_sessions(&error.to_string());
                mux_input::sync_workspace_panels(app);
                continue;
            }
            prepared_restart = Some(prepared);
            break;
        }

        if app.should_quit {
            if app.exit_dir.is_none() {
                app.exit_dir = app
                    .mux
                    .as_ref()
                    .and_then(|mux| mux.focused_cwd())
                    .map(|path| path.to_string_lossy().to_string());
            }
            let shutdown = app.mux.as_mut().map(|mux| mux.shutdown()).transpose();
            if let Err(error) = shutdown {
                app.should_quit = false;
                app.cancel_notification_drain();
                app.detach();
                app.status_message = Some(format!("Cannot quit yet: {error}"));
                continue;
            }
            break;
        }

        if app.should_resume.is_some()
            || app.should_attach_tmux.is_some()
            || app.should_new_session.is_some()
        {
            break;
        }
    }

    if app.restart_after_update.is_none() {
        if let Some(scratchpad) = app.scratchpad.as_mut() {
            scratchpad.save()?;
        }
    }

    // Without a daemon, panes are children of this process and must be reaped. Capture the
    // exit directory first — shutdown drops the panes that hold it.
    if let Some(mux) = app.mux.as_mut() {
        if app.exit_dir.is_none() {
            app.exit_dir = mux
                .focused_cwd()
                .map(|path| path.to_string_lossy().to_string());
        }
        if !mux.panes.is_empty() {
            mux.shutdown()?;
        }
    }

    Ok(prepared_restart)
}

fn update_layout_metrics(app: &mut App, height: u16) {
    // visible_rows must match the session_list take() count:
    // inner height = total height - 6 (title + borders + status), minus the group
    // headers, which the list asks the app for so the two cannot disagree.
    let lines_per_item = if app.project_filter.is_some() { 1 } else { 2 };
    app.visible_rows = (height as usize)
        .saturating_sub(6)
        .saturating_sub(app.list_header_lines())
        / lines_per_item;

    // Project popup visible rows: popup is ~25-80% height, minus borders (2), search (1), separator (1)
    let popup_percent = 80u16
        .min(25u16.max(
            (((app.unique_projects.len() + 6).min(20) as f32 / height as f32) * 100.0) as u16,
        ));
    let popup_height = (height as usize * popup_percent as usize) / 100;
    app.project_visible_rows = popup_height.saturating_sub(4); // borders + search + separator
}

/// Wait for the next terminal or PTY event and apply it.
///
/// Output arriving in many small chunks is drained in one go so a chatty child cannot
/// force a repaint per chunk.
fn pump_mux(app: &mut App) -> Result<bool> {
    let Some(mux) = app.mux.as_ref() else {
        return Ok(false);
    };

    // A blank pane is showing the startup spinner, which needs frequent repaints; an
    // established pane can idle until something actually happens.
    let animating = mux_animating(app);
    let timeout = mux_wait_timeout(app, animating);

    let first = match mux
        .receiver
        .recv_timeout(std::time::Duration::from_millis(timeout))
    {
        Ok(event) => event,
        // Maintenance, animations, and background work use this timeout as their
        // redraw clock.
        Err(std::sync::mpsc::RecvTimeoutError::Timeout) => return Ok(true),
        Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => {
            app.should_quit = true;
            return Ok(true);
        }
    };

    let mut pending = vec![first];
    while let Ok(event) = app.mux.as_ref().expect("mux present").receiver.try_recv() {
        pending.push(event);
        if pending.len() >= 256 {
            break;
        }
    }

    let mut repaint = false;
    for event in pending {
        match event {
            mux::MuxEvent::Term(event) => {
                repaint |= terminal_event_needs_repaint(app, &event);
                apply_terminal_event(app, event)?;
            }
            other => {
                repaint |= mux_input::handle_mux_event(app, other);
            }
        }
    }

    Ok(repaint)
}

/// Paint one frame as a single terminal update.
///
/// Copilot redraws its whole composer on every update, so a frame can rewrite most of
/// the chat pane. Without these markers a terminal is free to present that halfway
/// through, which is what made Alacritty tear and flicker. Terminals that do not
/// implement synchronized updates ignore the private mode.
///
/// No view asks for the terminal's real cursor — panes paint their own — so ratatui
/// leaves it hidden for the whole session and nothing drags it across the screen.
fn draw_frame(terminal: &mut Terminal<CrosstermBackend<io::Stdout>>, app: &mut App) -> Result<()> {
    synchronized_frame(terminal, |frame| {
        ui::draw(frame, app);
        // Read after drawing, so the pane's position is the one this frame used.
        app.visible_hyperlinks()
    })
}

/// Draw a frame, then turn the links on it into real hyperlinks — one terminal update.
///
/// The links go in the same update as the frame, so the terminal never shows a moment
/// where the text is there and the link behind it is not.
fn synchronized_frame<W: io::Write>(
    terminal: &mut Terminal<CrosstermBackend<W>>,
    render: impl FnOnce(&mut ratatui::Frame) -> Vec<ui::hyperlinks::HyperlinkRun>,
) -> Result<()> {
    execute!(terminal.backend_mut(), BeginSynchronizedUpdate)?;
    let mut links = Vec::new();
    let drawn = terminal
        .draw(|frame| links = render(frame))
        .map(|frame| ui::hyperlinks::overlay(&links, frame.buffer))
        .and_then(|bytes| {
            if !bytes.is_empty() {
                terminal.backend_mut().write_all(&bytes)?;
                terminal.backend_mut().flush()?;
            }
            Ok(())
        });
    // Closing the update matters even when the draw failed: leaving it open would freeze
    // the terminal on a half-written frame.
    execute!(terminal.backend_mut(), EndSynchronizedUpdate)?;
    drawn?;
    Ok(())
}

fn mux_animating(app: &App) -> bool {
    app.mux.as_ref().is_some_and(|mux| {
        mux.panes
            .iter()
            .any(|pane| pane.is_running() && pane.is_blank())
    }) || tab_spinner_animating(app)
        || app.github_loading()
        || app.sessions_loading()
}

/// Whether a tab spinner is on screen and needs frames.
///
/// Deliberately gated on the strip being visible: a lone session has no tab to spin, so
/// a working pane must not pull the idle cadence down to 100 ms for nothing.
fn tab_spinner_animating(app: &App) -> bool {
    if !matches!(app.view, app::View::Attached(_)) || !app.tab_bar_visible() {
        return false;
    }
    app.mux.as_ref().is_some_and(|mux| {
        mux.panes.iter().any(|pane| {
            pane.is_running()
                && !pane.needs_attention()
                && matches!(
                    pane.effective_progress_state(),
                    host_terminal::ProgressState::Normal
                        | host_terminal::ProgressState::Indeterminate
                )
        })
    })
}

fn mux_paint_due(app: &App, repaint: bool, last_paint: std::time::Instant) -> bool {
    repaint
        || (app.mux.is_some()
            && last_paint.elapsed()
                >= std::time::Duration::from_millis(mux_wait_timeout(app, mux_animating(app))))
}

/// Ordinary chat input changes the child, not CST's current frame. Waiting for the
/// child's output avoids rendering the stale pre-echo screen while holding its parser.
fn prioritize_explicit_exit(app: &mut App) -> bool {
    let requested = app.should_quit
        || app.should_resume.is_some()
        || app.should_attach_tmux.is_some()
        || app.should_new_session.is_some();
    if requested && app.update_restart_ready() {
        app.cancel_update_restart_for_user_action();
    }
    requested
}

fn terminal_event_needs_repaint(app: &App, event: &crossterm::event::Event) -> bool {
    if !matches!(app.view, app::View::Attached(_))
        || app.workspace_focus != app::WorkspaceFocus::Chat
        || app.confirm_quit
        || app.confirm_update_restart
        || app.command_palette.is_some()
        || app.github_inspector.is_some()
        || app.snippet_modal.is_some()
        || app.workspace_help.is_some()
        || !app.terminal_focused
    {
        return true;
    }

    let Some(mux) = app.mux.as_ref() else {
        return true;
    };
    let pane_is_ready = mux
        .focused_pane()
        .is_some_and(|pane| pane.is_running() && !pane.needs_attention());
    if !pane_is_ready {
        return true;
    }

    match event {
        crossterm::event::Event::Paste(_) => false,
        crossterm::event::Event::Key(key) if key.kind == crossterm::event::KeyEventKind::Press => {
            app.update_notice.is_some()
                || mux.prefix_state != mux::PrefixState::Idle
                || mux.prefix.matches(key)
        }
        crossterm::event::Event::Key(_) => false,
        _ => true,
    }
}

fn mux_wait_timeout(app: &App, animating: bool) -> u64 {
    if animating || app.attached_terminal_visible() || app.detail_load_pending() {
        100
    } else if app.background_work_pending() {
        250
    } else {
        // Input and PTY output wake the channel immediately. This is only the idle
        // maintenance cadence; redrawing every 250 ms across many CST instances was
        // consuming a meaningful fraction of a CPU core for no visible change.
        5_000
    }
}

fn desired_terminal_title(app: &App) -> String {
    match app.view {
        app::View::Attached(_) => {
            let title = app
                .focused_pane_title()
                .unwrap_or_else(|| "Copilot Session Manager".to_string());
            if app.any_pane_needs_attention() && !title.starts_with("? ") {
                format!("? {title}")
            } else {
                title
            }
        }
        app::View::List if app.any_pane_needs_attention() => {
            "? Copilot Session Manager".to_string()
        }
        app::View::List => "Copilot Session Manager".to_string(),
    }
}

fn exit_waits_for_update(app: &App) -> bool {
    app.update_installing()
        && (app.should_quit
            || app.should_resume.is_some()
            || app.should_attach_tmux.is_some()
            || app.should_new_session.is_some())
}

fn apply_terminal_event(app: &mut App, event: crossterm::event::Event) -> Result<()> {
    match &event {
        crossterm::event::Event::FocusGained => {
            app.terminal_focused = true;
            if matches!(app.view, app::View::Attached(_)) {
                app.acknowledge_focused_pane();
            }
            return Ok(());
        }
        crossterm::event::Event::FocusLost => {
            app.terminal_focused = false;
            return Ok(());
        }
        crossterm::event::Event::Key(crossterm::event::KeyEvent {
            kind: crossterm::event::KeyEventKind::Press,
            ..
        })
        | crossterm::event::Event::Paste(_)
        | crossterm::event::Event::Mouse(crossterm::event::MouseEvent {
            kind: crossterm::event::MouseEventKind::Down(_),
            ..
        }) => {
            app.terminal_focused = true;
            if matches!(app.view, app::View::Attached(_)) {
                app.acknowledge_focused_pane();
            }
        }
        _ => {}
    }
    match app.view {
        app::View::Attached(_) => {
            mux_input::handle_attached_event(app, event);
            Ok(())
        }
        app::View::List => input::handle_terminal_event(app, event),
    }
}

fn print_shell_init(shell: &str) {
    match shell {
        "bash" | "zsh" => {
            print!(
                r#"cst() {{
    local tmpfile
    tmpfile=$(mktemp)
    command copilot-session-tui --last-dir-file="$tmpfile" "$@"
    local last_dir
    last_dir=$(cat "$tmpfile" 2>/dev/null)
    rm -f "$tmpfile"
    if [ -n "$last_dir" ] && [ -d "$last_dir" ]; then
        cd "$last_dir" || true
    fi
}}
"#
            );
        }
        "powershell" => {
            print!(
                r#"function cst {{
    $tmpfile = [System.IO.Path]::GetTempFileName()
    copilot-session-tui --last-dir-file="$tmpfile" @args
    $lastDir = Get-Content $tmpfile -ErrorAction SilentlyContinue
    Remove-Item $tmpfile -ErrorAction SilentlyContinue
    if ($lastDir -and (Test-Path $lastDir)) {{
        Set-Location $lastDir
    }}
}}
"#
            );
        }
        _ => unreachable!(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::mux::{Pane, PaneSpec};
    use chrono::{TimeZone, Utc};
    use std::sync::{Arc, Mutex};

    /// Writer that keeps everything written where a test can read it back.
    #[derive(Clone, Default)]
    struct Recorder(Arc<Mutex<Vec<u8>>>);

    impl io::Write for Recorder {
        fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
            self.0.lock().unwrap().extend_from_slice(buf);
            Ok(buf.len())
        }

        fn flush(&mut self) -> io::Result<()> {
            Ok(())
        }
    }

    #[test]
    fn a_frame_is_one_synchronized_update_and_never_shows_the_real_cursor() {
        let recorder = Recorder::default();
        let mut terminal = Terminal::with_options(
            CrosstermBackend::new(recorder.clone()),
            ratatui::TerminalOptions {
                viewport: ratatui::Viewport::Fixed(ratatui::layout::Rect::new(0, 0, 8, 2)),
            },
        )
        .expect("fixed viewport needs no terminal");

        synchronized_frame(&mut terminal, |frame| {
            frame.render_widget(ratatui::widgets::Paragraph::new("hello"), frame.area());
            Vec::new()
        })
        .expect("recording writer cannot fail");

        let written = String::from_utf8_lossy(&recorder.0.lock().unwrap()).into_owned();
        let begin = written.find("\x1b[?2026h");
        let text = written.find("hello");
        let end = written.rfind("\x1b[?2026l");

        // Cells only ever reach the terminal between the two markers, so it can never
        // present a half-written frame.
        assert!(
            begin < text && text < end && begin.is_some() && end.is_some(),
            "unexpected frame order in {written:?}"
        );
        // Panes paint their own cursor, so the terminal's real one stays off.
        assert!(
            !written.contains("\x1b[?25h"),
            "frame turned the real cursor on: {written:?}"
        );
    }

    fn session(id: &str, name: &str, active: bool) -> session::Session {
        session::Session {
            id: id.to_string(),
            cwd: r"C:\work\project".to_string(),
            project_root: r"C:\work\project".to_string(),
            summary: Some(name.to_string()),
            created_at: None,
            updated_at: Some(Utc.timestamp_opt(1, 0).unwrap()),
            is_active: active,
            dir_path: PathBuf::new(),
            edited_files: Vec::new(),
            last_user_message: None,
            turn_count: 0,
            tool_call_count: 0,
            details_parsed_len: 0,
        }
    }

    #[test]
    fn direct_session_options_parse_and_conflict() {
        let direct = Cli::try_parse_from(["cst", "--session", "session-1", "--mux"]).unwrap();
        assert_eq!(direct.session.as_deref(), Some("session-1"));
        assert!(direct.mux);

        let favorites = Cli::try_parse_from(["cst", "--open-favorites", "--no-mux"]).unwrap();
        assert!(favorites.open_favorites);
        assert!(favorites.no_mux);

        assert!(
            Cli::try_parse_from(["cst", "--session", "session-1", "--open-favorites"]).is_err()
        );

        let handoff = Cli::try_parse_from([
            "cst",
            "--restart-gate",
            "gate",
            "--restart-parent-pid",
            "42",
        ])
        .unwrap();
        assert_eq!(handoff.restart_parent_pid, Some(42));
        assert_eq!(handoff.restart_gate, Some(PathBuf::from("gate")));
        assert!(Cli::try_parse_from(["cst", "--restart-gate", "gate"]).is_err());
    }

    #[test]
    fn lifecycle_hook_subcommands_parse_without_starting_the_tui() {
        let install = Cli::try_parse_from(["cst", "hooks", "install"]).unwrap();
        assert!(matches!(
            install.command,
            Some(Commands::Hooks {
                action: HookPluginCommand::Install
            })
        ));

        let event = Cli::try_parse_from(["cst", "hook-event", "working"]).unwrap();
        assert!(matches!(
            event.command,
            Some(Commands::HookEvent { ref event }) if event == "working"
        ));
    }

    #[test]
    fn update_subcommand_parses_without_starting_the_tui() {
        let cli = Cli::try_parse_from(["cst", "update"]).unwrap();
        assert!(matches!(cli.command, Some(Commands::Update)));
    }

    #[test]
    fn doctor_parses_even_though_the_shell_wrapper_prepends_last_dir_file() {
        // `cst` is a shell function that always passes --last-dir-file, so the
        // subcommand never arrives as the first argument in real use.
        let cli = Cli::try_parse_from(["cst", "--last-dir-file", "tmp", "doctor"]).unwrap();
        assert!(matches!(cli.command, Some(Commands::Doctor)));
    }

    #[test]
    fn restart_handoff_preserves_runtime_options_session_order_and_focus() {
        let cli = Cli::try_parse_from([
            "cst",
            "--copilot-home",
            r"C:\copilot-home",
            "--auto-filter",
            "false",
            "--last-dir-file",
            r"C:\temp\last-dir",
            "--mux",
        ])
        .unwrap();
        let request = app::UpdateRestartRequest {
            panes: vec![
                app::UpdateRestartPane {
                    pane_id: Some(2),
                    copilot_running: true,
                    terminal_generation: None,
                    session_id: "session-2".to_string(),
                    cwd: PathBuf::from(r"C:\work\two"),
                    title: "Second".to_string(),
                },
                app::UpdateRestartPane {
                    pane_id: Some(1),
                    copilot_running: true,
                    terminal_generation: None,
                    session_id: "session-1".to_string(),
                    cwd: PathBuf::from(r"C:\work\one"),
                    title: "First".to_string(),
                },
            ],
            focused_session_id: Some("session-2".to_string()),
        };

        let args = restart_arguments(&cli, Path::new(r"C:\copilot-home"), true);
        let reparsed =
            Cli::try_parse_from(std::iter::once(OsString::from("cst")).chain(args.clone()))
                .unwrap();

        assert_eq!(
            reparsed.copilot_home,
            Some(PathBuf::from(r"C:\copilot-home"))
        );
        assert!(!reparsed.auto_filter);
        assert!(reparsed.mux);
        assert_eq!(
            reparsed.last_dir_file,
            Some(PathBuf::from(r"C:\temp\last-dir"))
        );

        let prepared = PreparedRestart {
            child: None,
            gate: tempfile::tempdir().unwrap(),
            released: false,
        };
        prepared.write_manifest(&request).unwrap();
        let manifest: RestartManifest = serde_json::from_slice(
            &std::fs::read(restart_gate_path(prepared.gate.path(), "manifest")).unwrap(),
        )
        .unwrap();
        assert_eq!(
            manifest
                .panes
                .iter()
                .map(|pane| pane.session_id.as_str())
                .collect::<Vec<_>>(),
            ["session-2", "session-1"]
        );
        assert_eq!(manifest.focused_session_id.as_deref(), Some("session-2"));
    }

    #[test]
    fn explicit_actions_supersede_automatic_restart_after_installation() {
        let ready_app = || {
            let mut app = App::new(Vec::new(), config::UserConfig::default());
            app.restart_after_update = Some(app::UpdateRestartRequest::default());
            app.installed_update_version = Some("99.0.0".to_string());
            app
        };

        let mut quitting = ready_app();
        quitting.should_quit = true;
        assert!(prioritize_explicit_exit(&mut quitting));
        assert!(quitting.restart_after_update.is_none());
        assert!(quitting.should_quit);

        let mut resuming = ready_app();
        resuming.should_resume = Some(("session-1".to_string(), "cwd".to_string()));
        assert!(prioritize_explicit_exit(&mut resuming));
        assert!(resuming.restart_after_update.is_none());
        assert!(resuming.should_resume.is_some());

        let mut creating = ready_app();
        creating.should_new_session = Some(NewSessionRequest::Normal {
            cwd: "cwd".to_string(),
        });
        assert!(prioritize_explicit_exit(&mut creating));
        assert!(creating.restart_after_update.is_none());
        assert!(creating.should_new_session.is_some());
    }

    #[test]
    fn restore_handoff_can_resume_a_session_missing_from_the_visible_catalog() {
        let restored = resolve_restored_startup_session(
            &[],
            "fresh-session-id",
            Path::new(r"C:\work\brand-new"),
        );

        assert_eq!(restored.id, "fresh-session-id");
        assert_eq!(restored.cwd, r"C:\work\brand-new");
        assert_eq!(restored.title, "session fresh-se");
    }

    #[test]
    fn terminal_only_resource_does_not_resume_a_stopped_copilot_chat() {
        let request = app::UpdateRestartRequest {
            panes: vec![app::UpdateRestartPane {
                pane_id: Some(7),
                copilot_running: false,
                terminal_generation: Some(3),
                session_id: "stopped-chat".to_string(),
                cwd: PathBuf::from(r"C:\work\project"),
                title: "Shell".to_string(),
            }],
            focused_session_id: Some("stopped-chat".to_string()),
        };

        let prepared = PreparedRestart {
            child: None,
            gate: tempfile::tempdir().unwrap(),
            released: false,
        };
        prepared.write_manifest(&request).unwrap();
        let manifest: RestartManifest = serde_json::from_slice(
            &std::fs::read(restart_gate_path(prepared.gate.path(), "manifest")).unwrap(),
        )
        .unwrap();
        assert!(manifest.panes.is_empty());
        assert!(manifest.focused_session_id.is_none());
    }

    #[test]
    fn restart_gate_waits_for_release_from_the_supervising_process() {
        let temp = tempfile::tempdir().unwrap();
        let gate = temp.path().to_path_buf();
        let manifest_path = restart_gate_path(&gate, "manifest");
        std::fs::write(
            &manifest_path,
            serde_json::to_vec(&RestartManifest::default()).unwrap(),
        )
        .unwrap();
        let worker_gate = gate.clone();
        let worker = std::thread::spawn(move || {
            wait_for_restart_release(&worker_gate, std::process::id()).unwrap()
        });
        let ready = restart_gate_path(&gate, "ready");
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(1);
        while !ready.is_file() && std::time::Instant::now() < deadline {
            std::thread::sleep(std::time::Duration::from_millis(5));
        }
        assert!(ready.is_file());

        std::fs::write(restart_gate_path(&gate, "go"), b"go").unwrap();
        assert!(worker.join().unwrap().is_some());
    }

    #[test]
    fn version_flag_reports_the_crate_version() {
        // Asking a binary what it is should not require byte-scanning it.
        let Err(error) = Cli::try_parse_from(["cst", "--version"]) else {
            panic!("--version should short-circuit parsing");
        };
        assert_eq!(error.kind(), clap::error::ErrorKind::DisplayVersion);
        assert!(error.to_string().contains(env!("CARGO_PKG_VERSION")));
    }

    #[test]
    fn idle_mux_waits_for_events_instead_of_redrawing_four_times_per_second() {
        let mut app = App::new(Vec::new(), config::UserConfig::default());
        assert_eq!(mux_wait_timeout(&app, false), 5_000);
        assert_eq!(mux_wait_timeout(&app, true), 100);

        let (_sender, receiver) = std::sync::mpsc::channel();
        app.begin_session_load(receiver);
        assert_eq!(mux_wait_timeout(&app, false), 250);

        app.detail_pending = Some(("large-session".to_string(), std::time::Instant::now()));
        assert_eq!(mux_wait_timeout(&app, false), 100);
    }

    #[test]
    fn config_watcher_wakes_the_mux_only_after_content_changes() {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("config.json");
        std::fs::write(&path, r#"{"model":"first"}"#).unwrap();
        let initial = config::config_revision(&path).unwrap();
        let applied = std::sync::Arc::new(std::sync::Mutex::new(Some(initial)));
        let (sender, receiver) = std::sync::mpsc::channel();
        let watcher = ConfigWatcher::start(
            sender,
            path.clone(),
            std::sync::Arc::clone(&applied),
            std::time::Duration::from_millis(5),
        );

        assert!(receiver
            .recv_timeout(std::time::Duration::from_millis(30))
            .is_err());
        std::fs::write(&path, r#"{"model":"second"}"#).unwrap();
        assert!(matches!(
            receiver.recv_timeout(std::time::Duration::from_secs(1)),
            Ok(mux::MuxEvent::ConfigChanged)
        ));
        let second = config::config_revision(&path).unwrap();
        *applied.lock().unwrap() = Some(second);
        std::thread::sleep(std::time::Duration::from_millis(20));
        while receiver.try_recv().is_ok() {}

        std::fs::write(&path, r#"{"model":"first"}"#).unwrap();
        assert!(matches!(
            receiver.recv_timeout(std::time::Duration::from_secs(1)),
            Ok(mux::MuxEvent::ConfigChanged)
        ));

        drop(watcher);
        std::fs::write(&path, r#"{"model":"third"}"#).unwrap();
        assert!(receiver
            .recv_timeout(std::time::Duration::from_millis(30))
            .is_err());
    }

    #[test]
    fn repaint_deadline_bounds_staleness_when_non_visual_events_keep_arriving() {
        let config = config::UserConfig {
            mux: true,
            ..config::UserConfig::default()
        };
        let mut app = App::new(Vec::new(), config);
        assert!(mux_paint_due(
            &app,
            false,
            std::time::Instant::now() - std::time::Duration::from_secs(6)
        ));

        app.detail_pending = Some(("large-session".to_string(), std::time::Instant::now()));
        assert!(mux_paint_due(
            &app,
            false,
            std::time::Instant::now() - std::time::Duration::from_millis(150)
        ));
        assert!(mux_paint_due(&app, true, std::time::Instant::now()));
    }

    #[test]
    fn ordinary_chat_input_waits_for_the_child_before_repainting() {
        let config = config::UserConfig {
            mux: true,
            ..config::UserConfig::default()
        };
        let mut app = App::new(Vec::new(), config);
        let events = app.mux.as_ref().unwrap().events.clone();
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
                id: 1,
                title: "Latency test".to_string(),
                cwd: std::env::temp_dir(),
                session_id: "latency-test".to_string(),
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
        .unwrap();
        app.mux.as_mut().unwrap().push(pane);
        app.view = app::View::Attached(1);
        app.terminal_focused = true;

        let character = crossterm::event::Event::Key(crossterm::event::KeyEvent::new(
            crossterm::event::KeyCode::Char('a'),
            crossterm::event::KeyModifiers::NONE,
        ));
        assert!(!terminal_event_needs_repaint(&app, &character));
        assert!(!terminal_event_needs_repaint(
            &app,
            &crossterm::event::Event::Paste("large prompt".to_string())
        ));

        let prefix = crossterm::event::Event::Key(crossterm::event::KeyEvent::new(
            crossterm::event::KeyCode::Char('b'),
            crossterm::event::KeyModifiers::CONTROL,
        ));
        assert!(terminal_event_needs_repaint(&app, &prefix));

        app.open_command_palette();
        assert!(terminal_event_needs_repaint(&app, &character));
        app.close_command_palette();

        app.update_notice = Some("Update ready".to_string());
        assert!(terminal_event_needs_repaint(&app, &character));
    }

    #[test]
    fn outer_title_carries_attention_in_attached_and_list_views() {
        let config = config::UserConfig {
            mux: true,
            ..config::UserConfig::default()
        };
        let mut app = App::new(Vec::new(), config);
        let events = app.mux.as_ref().unwrap().events.clone();
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
        let mut pane = Pane::spawn(
            PaneSpec {
                id: 1,
                title: "Plan review".to_string(),
                cwd: std::env::temp_dir(),
                session_id: "attention-title".to_string(),
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
        .unwrap();
        pane.feed_synthetic(b"\x1b]9;4;3;0\x1b\\\x1b]9;4;0;0\x1b\\");
        pane.refresh_from_callbacks(false);
        app.mux.as_mut().unwrap().push(pane);
        app.view = app::View::Attached(1);

        assert_eq!(desired_terminal_title(&app), "? Plan review");
        app.terminal_focused = false;
        apply_terminal_event(&mut app, crossterm::event::Event::FocusGained).unwrap();
        assert_eq!(desired_terminal_title(&app), "Plan review");

        app.terminal_focused = false;
        app.mux
            .as_mut()
            .unwrap()
            .pane_mut(1)
            .unwrap()
            .feed_synthetic(b"\x1b]9;4;3;0\x1b\\\x1b]9;4;0;0\x1b\\");
        app.mux
            .as_mut()
            .unwrap()
            .pane_mut(1)
            .unwrap()
            .refresh_from_callbacks(false);
        app.view = app::View::List;
        assert_eq!(desired_terminal_title(&app), "? Copilot Session Manager");
        apply_terminal_event(&mut app, crossterm::event::Event::FocusGained).unwrap();
        assert_eq!(
            desired_terminal_title(&app),
            "? Copilot Session Manager",
            "focusing the list must not acknowledge a pane that was not viewed"
        );

        let _ = app.mux.as_mut().unwrap().shutdown();
    }

    #[test]
    fn newly_created_app_is_unattended_until_focus_or_input_arrives() {
        let mut app = App::new(Vec::new(), config::UserConfig::default());
        assert!(!app.terminal_focused);

        apply_terminal_event(
            &mut app,
            crossterm::event::Event::Key(crossterm::event::KeyEvent::new(
                crossterm::event::KeyCode::Char('x'),
                crossterm::event::KeyModifiers::NONE,
            )),
        )
        .unwrap();

        assert!(app.terminal_focused);
    }

    #[test]
    fn natural_exit_waits_for_background_installation() {
        let mut app = App::new(Vec::new(), config::UserConfig::default());
        let (_sender, receiver) = std::sync::mpsc::channel();
        app.update_install_receiver = Some(receiver);
        app.should_quit = true;

        assert!(exit_waits_for_update(&app));

        app.update_install_receiver = None;
        assert!(!exit_waits_for_update(&app));
    }

    #[test]
    fn startup_session_requires_an_exact_inactive_match() {
        let sessions = vec![
            session("session-1", "First", false),
            session("session-2", "Second", true),
        ];

        assert_eq!(
            resolve_startup_session(&sessions, "session-1").unwrap(),
            StartupSession {
                id: "session-1".to_string(),
                cwd: r"C:\work\project".to_string(),
                title: "First".to_string(),
            }
        );
        assert!(resolve_startup_session(&sessions, "session").is_err());
        assert!(resolve_startup_session(&sessions, "session-2")
            .unwrap_err()
            .to_string()
            .contains("already active"));
    }

    #[test]
    fn short_session_ids_do_not_panic() {
        assert_eq!(short_session_id("short"), "short");
        assert_eq!(short_session_id("123456789"), "12345678");
    }

    #[test]
    fn direct_mux_startup_falls_back_when_saved_directory_is_missing() {
        let directory = tempfile::tempdir().unwrap();
        assert_eq!(
            existing_or_current_dir(&directory.path().to_string_lossy()),
            directory.path().to_string_lossy()
        );

        let fallback = existing_or_current_dir(r"Z:\definitely\missing\cst-session");
        assert!(Path::new(&fallback).is_dir());
    }
}

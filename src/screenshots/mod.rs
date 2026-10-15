//! Generates the README screenshots from the real widgets.
//!
//! Hand-captured terminal shots go stale the moment a pane moves, and capturing
//! them from a live install would leak whatever sessions happen to be on disk.
//! This renders the same `ui::draw` the app runs, against sessions invented here,
//! so the images can be regenerated after any UI change and never contain private
//! data.
//!
//! ```text
//! cargo run --features screenshots -- screenshots docs/img
//! ```

mod svg;

use anyhow::{Context, Result};
use chrono::{Duration, Utc};
use ratatui::backend::TestBackend;
use ratatui::layout::Rect;
use ratatui::Terminal;
use std::path::Path;

use crate::app::{App, View};
use crate::config::UserConfig;
use crate::mux::{Pane, PaneSpec};
use crate::session::Session;
use crate::theme::ThemeName;

/// Wide enough for the split panes to breathe and for the two-row key hints to fit,
/// short enough to stay readable when GitHub scales the image to the README column.
const WIDTH: u16 = 124;
const HEIGHT: u16 = 34;

/// Renders one screenshot's worth of app state.
type Scene = Box<dyn Fn() -> Result<String>>;

pub fn run(out_dir: &Path) -> Result<()> {
    // Nothing here should ever read the real session directory. Redirecting the
    // whole Copilot home at the start makes that a property of the process rather
    // than a promise each scene has to keep.
    let sandbox = tempfile::tempdir().context("Failed to create a sandbox directory")?;
    std::env::set_var("COPILOT_HOME", sandbox.path());

    std::fs::create_dir_all(out_dir)
        .with_context(|| format!("Failed to create {}", out_dir.display()))?;

    let scenes: Vec<(&str, Scene)> = vec![
        ("session-list", Box::new(session_list)),
        ("theme-picker", Box::new(theme_picker)),
        ("workspace", {
            let root = sandbox.path().to_path_buf();
            Box::new(move || workspace(&root))
        }),
        ("github-inspector", Box::new(github_inspector)),
    ];

    for (name, scene) in scenes {
        let svg = scene()?;
        let path = out_dir.join(format!("{name}.svg"));
        std::fs::write(&path, svg)
            .with_context(|| format!("Failed to write {}", path.display()))?;
        println!("wrote {}", path.display());
    }
    Ok(())
}

/// Render one app state and return the SVG for it.
fn capture(app: &mut App, width: u16, height: u16) -> Result<String> {
    let theme = app.theme();
    let mut terminal = Terminal::new(TestBackend::new(width, height))
        .context("Failed to create the offscreen terminal")?;
    terminal
        .draw(|frame| crate::ui::draw(frame, app))
        .context("Failed to render the frame")?;
    Ok(svg::render(terminal.backend().buffer(), theme))
}

fn session_list() -> Result<String> {
    let sessions = demo_sessions();
    let mut config = UserConfig {
        theme: ThemeName::CatppuccinMocha,
        ..UserConfig::default()
    };
    config.favorites.push(sessions[0].id.clone());
    config.favorites.push(sessions[1].id.clone());

    let mut app = App::new(sessions, config);
    app.selected = 0;
    capture(&mut app, WIDTH, HEIGHT)
}

fn github_inspector() -> Result<String> {
    // Keep one generated scene light so both sides of the theme system are covered
    // whenever the README images are regenerated.
    let config = UserConfig {
        theme: ThemeName::CatppuccinLatte,
        ..UserConfig::default()
    };
    let mut app = App::new(demo_sessions(), config);
    crate::ui::github_inspector::install_demo_pull_request(&mut app);
    capture(&mut app, WIDTH, 26)
}

fn theme_picker() -> Result<String> {
    let mut app = App::new(
        demo_sessions(),
        UserConfig {
            theme: ThemeName::CatppuccinMocha,
            ..UserConfig::default()
        },
    );
    app.mode = crate::app::Mode::Settings;
    app.settings_section = crate::app::SettingsSection::General;
    app.settings_selected = 3;
    app.open_theme_picker();
    app.theme_picker.as_mut().unwrap().selected = ThemeName::ALL
        .iter()
        .position(|name| *name == ThemeName::CatppuccinLatte)
        .unwrap();
    capture(&mut app, WIDTH, HEIGHT)
}

/// The attached workspace: a Copilot pane beside its scratchpad.
fn workspace(sandbox: &Path) -> Result<String> {
    let session_id = "9f1c8e42-demo-4a77-9c31-5b6d0e2a4f18";
    let config = UserConfig {
        mux: true,
        theme: ThemeName::TokyoNight,
        ..Default::default()
    };
    let mut app = App::new(demo_sessions(), config);

    // Size the pane to the area it will actually occupy, so the transcript wraps
    // exactly as it will on screen.
    let layout = crate::ui::attached_layout_sized(
        Rect::new(0, 0, WIDTH, HEIGHT),
        true,
        false,
        false,
        crate::ui::DockSizes::default(),
    );
    let chat = layout.chat_pane();
    let (rows, cols) = (chat.height, chat.width);

    let events = app.mux.as_ref().expect("mux is enabled").events.clone();
    // A pane needs a live child to report itself as running. This one only has to
    // outlive the render, and the mux kills it again below.
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
            title: "Add a tree view to the PR inspector".to_string(),
            cwd: sandbox.to_path_buf(),
            session_id: session_id.to_string(),
            program,
            args,
            events_path: None,
            terminal_light_mode: Some(false),
            hooks_active: false,
        },
        rows,
        cols,
        events,
    )?;
    pane.feed_synthetic(TRANSCRIPT.replace('\n', "\r\n").as_bytes());
    // Park the cursor inside the prompt box rather than wherever the transcript
    // happened to end, so the pane looks like it is waiting for input.
    pane.feed_synthetic(b"\x1b[2A\r\x1b[4C");
    let pane_id = app.mux.as_mut().expect("mux is enabled").push(pane);

    let scratchpad = crate::scratchpad::Scratchpad::synthetic(sandbox, session_id, SCRATCHPAD)?;
    app.scratchpad = Some(scratchpad);
    app.scratchpad_owner = Some(pane_id);
    app.scratchpad_open.insert(pane_id);
    app.view = View::Attached(pane_id);

    let svg = capture(&mut app, WIDTH, HEIGHT);
    // Always reap the child, even if rendering failed.
    if let Some(mux) = app.mux.as_mut() {
        let _ = mux.shutdown();
    }
    svg
}

/// Invented sessions across a few invented projects.
fn demo_sessions() -> Vec<Session> {
    let entries = [
        (
            "9f1c8e42-demo-4a77-9c31-5b6d0e2a4f18",
            "copilot-session-tui",
            "Add a tree view to the PR inspector",
            4,
            true,
        ),
        (
            "2b7d5a10-demo-4c02-8e4f-1a9c7b3d6e05",
            "copilot-session-tui",
            "Speed up session detail parsing",
            72,
            false,
        ),
        (
            "c4e90f38-demo-4b61-9d27-6f2a8c1e4b93",
            "copilot-session-tui",
            "Make scratchpad mouse selection reliable",
            190,
            false,
        ),
        (
            "7a3f6c21-demo-4e58-b0c9-3d8e5f7a2c14",
            "copilot-session-tui",
            "Forward OSC 52 clipboard writes through the mux",
            1_500,
            false,
        ),
        (
            "e58b2d47-demo-49af-8c13-7b0d6e4a9f26",
            "acme-api",
            "Add a --json flag to the status command",
            2_900,
            false,
        ),
        (
            "1d6a9b53-demo-4f70-a284-9c5e3b7d0a61",
            "acme-api",
            "Migrate CI to the slim runner image",
            4_300,
            false,
        ),
        (
            "b0c37e94-demo-4a15-9f68-2e4d8a6c5b07",
            "dotfiles",
            "Tidy shell aliases and the prompt",
            7_200,
            false,
        ),
        (
            "58e1f4a6-demo-4d93-b7c0-8a2f6e9d3b45",
            "notes",
            "Draft the v0.12 release notes",
            10_080,
            false,
        ),
        (
            "3f8c0d29-demo-4b46-85a7-1e9b2f6c4d80",
            "acme-web",
            "Replace the settings modal with a route",
            13_400,
            false,
        ),
        (
            "a71e5b08-demo-4c3d-92f4-6b0a8d1e7c53",
            "acme-web",
            "Audit bundle size after the router upgrade",
            17_900,
            false,
        ),
        (
            "6c2b9e74-demo-4a81-b3d5-0f7c4a2e8b19",
            "infra-terraform",
            "Split the staging workspace out of prod",
            21_600,
            false,
        ),
    ];

    entries
        .into_iter()
        .map(|(id, project, summary, minutes_ago, is_active)| {
            let updated = Utc::now() - Duration::minutes(minutes_ago);
            let root = format!("/home/dev/src/{project}");
            Session {
                id: id.to_string(),
                cwd: root.clone(),
                project_root: root,
                summary: Some(summary.to_string()),
                created_at: Some(updated - Duration::minutes(45)),
                updated_at: Some(updated),
                is_active,
                dir_path: std::path::PathBuf::from("/tmp").join(id),
                edited_files: Vec::new(),
                last_user_message: None,
                turn_count: 0,
                tool_call_count: 0,
                details_parsed_len: 0,
            }
        })
        .enumerate()
        .map(|(index, mut session)| {
            // Only the selected session has its details loaded in the real app, so
            // only the first one carries stats here.
            if index == 0 {
                session.edited_files = vec![
                    "src/ui/file_tree.rs".to_string(),
                    "src/ui/github_inspector.rs".to_string(),
                    "src/mux_input.rs".to_string(),
                    "src/app.rs".to_string(),
                    "README.md".to_string(),
                ];
                session.last_user_message = Some(
                    "The Files tab is hard to scan when a PR touches a dozen paths. Could the \
                     changed files render as a tree on the left, with the selected file's diff \
                     in a pane on the right?"
                        .to_string(),
                );
                session.turn_count = 47;
                session.tool_call_count = 312;
            }
            session
        })
        .collect()
}

const TRANSCRIPT: &str = "\x1b[38;5;212m●\x1b[0m \x1b[1mGitHub Copilot CLI\x1b[0m
\x1b[90m──────────────────────────────────────────────────────\x1b[0m

\x1b[36m›\x1b[0m Show the changed files as a tree and put the diff
  next to it.

\x1b[38;5;212m●\x1b[0m Splitting the Files tab now. The tree collapses
  single-child directories so deep paths stay narrow.

  \x1b[32m●\x1b[0m \x1b[1mCreate\x1b[0m src/ui/file_tree.rs
  \x1b[32m●\x1b[0m \x1b[1mEdit\x1b[0m   src/ui/github_inspector.rs \x1b[32m+218\x1b[0m \x1b[31m-96\x1b[0m
  \x1b[32m●\x1b[0m \x1b[1mEdit\x1b[0m   src/mux_input.rs \x1b[32m+164\x1b[0m \x1b[31m-31\x1b[0m
  \x1b[32m●\x1b[0m \x1b[1mBash\x1b[0m   cargo test --quiet
    \x1b[32m✓\x1b[0m 210 passed in 4.1s

\x1b[38;5;212m●\x1b[0m Done. The diff follows the tree cursor, so opening a
  file no longer needs \x1b[1mEnter\x1b[0m.

\x1b[36m›\x1b[0m Nice. Fold single-child chains too?

\x1b[38;5;212m●\x1b[0m Already in: \x1b[1msrc/ui\x1b[0m renders as one row.

\x1b[36m›\x1b[0m Great. Ship it and cut a release.

\x1b[38;5;212m●\x1b[0m Bumped to \x1b[1m0.12.0\x1b[0m, tagged, and the release workflow
  is green on all three targets.

\x1b[90m╭────────────────────────────────────────────────────╮\x1b[0m
\x1b[90m│\x1b[0m \x1b[36m>\x1b[0m                                                  \x1b[90m│\x1b[0m
\x1b[90m╰────────────────────────────────────────────────────╯\x1b[0m
";

const SCRATCHPAD: &str = "Files tab rework
- [x] tree with folded single-child dirs
- [x] diff follows the cursor
- [ ] ask about huge PRs (300+ files)

Perf: events.jsonl for the big session
is 385 MB. Keep the incremental parse
on the selection path or the list
stutters again.

Ship note: bump to 0.12.0 before the
tag, the workflow builds from it.
";

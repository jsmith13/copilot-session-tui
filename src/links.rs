//! What a link in a pane may do when clicked.
//!
//! A link's target is written by the child — which for a Copilot pane means by an agent,
//! from text it may have read anywhere, including a stranger's GitHub comment. So the
//! question here is not only "how do I open this" but "what may a click do at all".
//!
//! Web links are not opened here. They are drawn as real hyperlinks (see
//! [`crate::ui::hyperlinks`]) and the terminal opens them, on the machine the user is
//! sitting at — the only way that works when CST runs on a remote host over SSH. CST must
//! not open them as well: Windows Terminal opens a hyperlink *and* forwards the same click
//! to the program underneath, so doing both gives every click two browser tabs.
//!
//! A file link is CST's to handle, and it never launches the file: it opens the folder with
//! the file selected. That keeps a link to an `.exe` or a `.ps1` from running anything
//! while leaving the user a double-click away from the `.html` they wanted. It is never
//! handed to the terminal, which would open it with whatever the file type launches.
//!
//! Every other scheme is refused outright — `ms-settings:`, `vscode:` and friends are
//! handlers that act on a click, and nothing an agent printed should be able to reach them.
//!
//! Nothing here goes through a shell. The one place Windows forces a hand-built command line
//! — Explorer's `/select,` — is guarded by refusing any path containing a quote.

// Under test nothing is launched — `App::open_link` records the target instead — so the
// launching half of this module is unused there. Same arrangement as `notifications`.
#![cfg_attr(test, allow(dead_code))]

use anyhow::{bail, Context, Result};
use std::path::{Path, PathBuf};
use std::process::Command;

/// What a link may be opened as.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Target {
    /// An `http` or `https` address, drawn as a hyperlink for the terminal to open.
    Web(String),
    /// A file or folder, revealed in the file manager and never launched.
    File(PathBuf),
}

/// Decide what a link's target is, or refuse it.
pub fn classify(raw: &str) -> Option<Target> {
    let raw = raw.trim();
    if raw.is_empty() || raw.chars().any(char::is_control) {
        return None;
    }
    let lower = raw.to_ascii_lowercase();
    if lower.starts_with("https://") || lower.starts_with("http://") {
        // A well-formed URL has any space or quote percent-encoded. One that does not is
        // either malformed or trying to smuggle something into a command line.
        if raw.chars().any(|c| c.is_whitespace() || c == '"') {
            return None;
        }
        return Some(Target::Web(raw.to_string()));
    }
    let path = if lower.starts_with("file:") {
        file_url_path(&raw["file:".len()..])?
    } else {
        // A bare path as the target is still an explicit link, just without a scheme.
        PathBuf::from(raw)
    };
    (path.is_absolute() && !path.as_os_str().to_string_lossy().contains('"'))
        .then_some(Target::File(path))
}

/// Turn the part of a `file:` URL after the scheme into a local path.
fn file_url_path(rest: &str) -> Option<PathBuf> {
    let (authority, path) = match rest.strip_prefix("//") {
        Some(after) => match after.find('/') {
            Some(slash) => (&after[..slash], &after[slash..]),
            None => (after, ""),
        },
        None => ("", rest),
    };
    let path = percent_decode(path)?;
    let local = authority.is_empty() || authority.eq_ignore_ascii_case("localhost");

    if cfg!(windows) {
        let path = path.replace('/', "\\");
        if !local {
            // file://server/share/x is a UNC path.
            return Some(PathBuf::from(format!("\\\\{authority}{path}")));
        }
        // file:///C:/x arrives as \C:\x; the drive letter is the real start.
        let bytes = path.as_bytes();
        if bytes.len() >= 3 && bytes[0] == b'\\' && bytes[2] == b':' {
            return Some(PathBuf::from(&path[1..]));
        }
        Some(PathBuf::from(path))
    } else {
        local.then(|| PathBuf::from(path))
    }
}

fn percent_decode(text: &str) -> Option<String> {
    let bytes = text.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'%' {
            let hex = std::str::from_utf8(bytes.get(i + 1..i + 3)?).ok()?;
            out.push(u8::from_str_radix(hex, 16).ok()?);
            i += 3;
        } else {
            out.push(bytes[i]);
            i += 1;
        }
    }
    String::from_utf8(out).ok()
}

/// Whether this process was started inside an SSH session.
///
/// OpenSSH sets these for the remote shell on Linux and on Windows, and anything started
/// from it — tmux included — inherits them. Takes a lookup rather than reading the
/// environment itself so tests do not depend on how they were launched.
pub fn running_over_ssh(is_set: impl Fn(&str) -> bool) -> bool {
    ["SSH_CONNECTION", "SSH_CLIENT", "SSH_TTY"]
        .into_iter()
        .any(is_set)
}

/// Show a file selected in its folder, without ever launching it.
pub fn reveal(path: &Path) -> Result<()> {
    if !path.exists() {
        match path.parent().filter(|parent| parent.is_dir()) {
            // The file is gone but its folder is not: showing the folder is still the
            // most useful thing a click can do.
            Some(parent) => return open_folder(parent),
            None => bail!("{} does not exist", path.display()),
        }
    }
    select_in_folder(path)
}

#[cfg(windows)]
fn select_in_folder(path: &Path) -> Result<()> {
    use std::os::windows::process::CommandExt;
    // Explorer parses its own command line and only understands `/select,"path"` in
    // exactly that shape, which the standard argument quoting does not produce. The path
    // was checked for quotes when it was classified, so it cannot break out of them.
    Command::new("explorer.exe")
        .raw_arg(format!("/select,\"{}\"", path.display()))
        .spawn()
        .context("Could not open Explorer")?;
    Ok(())
}

#[cfg(target_os = "macos")]
fn select_in_folder(path: &Path) -> Result<()> {
    Command::new("open")
        .arg("-R")
        .arg(path)
        .spawn()
        .context("Could not open Finder")?;
    Ok(())
}

#[cfg(not(any(windows, target_os = "macos")))]
fn select_in_folder(path: &Path) -> Result<()> {
    // There is no portable way to select a file, so show the folder it lives in.
    let folder = if path.is_dir() {
        path
    } else {
        path.parent().unwrap_or(path)
    };
    open_folder(folder)
}

fn open_folder(folder: &Path) -> Result<()> {
    let program = if cfg!(windows) {
        "explorer.exe"
    } else if cfg!(target_os = "macos") {
        "open"
    } else {
        "xdg-open"
    };
    Command::new(program)
        .arg(folder)
        .spawn()
        .with_context(|| format!("Could not open {}", folder.display()))?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn web_links_are_followed() {
        assert_eq!(
            classify("https://example.com/a?b=1&c=2"),
            Some(Target::Web("https://example.com/a?b=1&c=2".into()))
        );
        assert!(matches!(
            classify("HTTP://example.com"),
            Some(Target::Web(_))
        ));
    }

    #[test]
    fn a_scheme_that_acts_on_a_click_is_refused() {
        // Each of these is a handler that does something when invoked. Nothing an agent
        // printed should be able to reach one.
        for raw in [
            "javascript:alert(1)",
            "ms-settings:privacy",
            "vscode://file/c:/x",
            "cmd:/c calc",
            "mailto:someone@example.com",
        ] {
            assert_eq!(classify(raw), None, "{raw} must not be opened");
        }
    }

    #[test]
    fn a_url_that_could_escape_its_argument_is_refused() {
        assert_eq!(classify("https://example.com/\" & calc"), None);
        assert_eq!(classify("https://example.com/a b"), None);
        assert_eq!(classify("https://example.com/\u{7}"), None);
    }

    #[test]
    fn a_relative_path_is_not_a_file_link() {
        assert_eq!(classify("index.html"), None);
        assert_eq!(classify("file:index.html"), None);
    }

    #[cfg(windows)]
    #[test]
    fn a_file_url_becomes_the_path_it_names() {
        assert_eq!(
            classify("file:///D:/code/My%20Views/index.html"),
            Some(Target::File(PathBuf::from(r"D:\code\My Views\index.html")))
        );
        assert_eq!(
            classify("file://localhost/C:/x.html"),
            Some(Target::File(PathBuf::from(r"C:\x.html")))
        );
        assert_eq!(
            classify("file://server/share/report.html"),
            Some(Target::File(PathBuf::from(r"\\server\share\report.html")))
        );
    }

    #[cfg(windows)]
    #[test]
    fn a_bare_windows_path_is_a_file_link() {
        // What the agent in the report fell back to printing once clicking failed.
        assert_eq!(
            classify(r"D:\code\VesperPiecewise_20260924\STREET_SCALE_CITY_20260930\index.html"),
            Some(Target::File(PathBuf::from(
                r"D:\code\VesperPiecewise_20260924\STREET_SCALE_CITY_20260930\index.html"
            )))
        );
    }

    #[cfg(windows)]
    #[test]
    fn an_executable_is_still_only_ever_revealed() {
        // Classified as a file like any other — and files are revealed, never launched.
        // The distinction lives in `open`, which has no path that runs a file.
        assert_eq!(
            classify("file:///C:/Windows/System32/calc.exe"),
            Some(Target::File(PathBuf::from(r"C:\Windows\System32\calc.exe")))
        );
    }

    #[cfg(not(windows))]
    #[test]
    fn a_file_url_becomes_the_path_it_names() {
        assert_eq!(
            classify("file:///home/me/My%20Views/index.html"),
            Some(Target::File(PathBuf::from("/home/me/My Views/index.html")))
        );
        assert_eq!(classify("file://elsewhere/x.html"), None);
    }

    /// Hands a real file to the real file manager.
    ///
    /// Everything else here runs with launching compiled out, so this is the only proof the
    /// command line itself is right — Explorer's `/select,` in particular, which has to be
    /// built by hand. Aim it at a script that leaves a marker behind if it ever runs: the
    /// point of revealing rather than opening is that the marker never appears.
    #[test]
    #[ignore = "opens a file manager window; run with CST_LINKS_LIVE_FILE=<path>"]
    fn a_file_link_really_reveals_the_file() {
        let Ok(path) = std::env::var("CST_LINKS_LIVE_FILE") else {
            return;
        };
        let Some(Target::File(path)) = classify(&path) else {
            panic!("{path} is not a file link");
        };
        reveal(&path).expect("the file manager should start");
    }

    #[test]
    fn malformed_percent_escapes_are_refused() {
        assert_eq!(classify("file:///C:/a%2"), None);
        assert_eq!(classify("file:///C:/a%zz"), None);
    }
}

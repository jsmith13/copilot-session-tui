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
//! the file selected. That keeps a link to an `.exe` or a `.ps1` from running anything. It
//! is never handed to the terminal, which would open it with whatever the file type
//! launches.
//!
//! The one exception is an HTML page, which opens in the browser. Agents write reports and
//! previews as `.html` and link to them, and revealing those only made the user
//! double-click the file CST had just selected. A page runs inside the browser's sandbox,
//! the same place a web link already lands, so opening one gives an agent nothing a web
//! link does not. The test is the exact extension, so `.hta` — which Windows runs as a
//! program — is still only revealed.
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
    /// A file or folder, revealed in the file manager — or, for an HTML page, opened in
    /// the browser. Never otherwise launched.
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

/// Whether a file link may be opened in the browser rather than revealed.
pub fn is_web_page(path: &Path) -> bool {
    path.extension()
        .and_then(|extension| extension.to_str())
        .is_some_and(|extension| {
            extension.eq_ignore_ascii_case("html") || extension.eq_ignore_ascii_case("htm")
        })
}

/// Whether this file link opens in the browser — an existing page, not a folder that
/// happens to be called `x.html`, which a browser would only list.
fn opens_in_browser(path: &Path) -> bool {
    is_web_page(path) && path.is_file()
}

/// Open a file link: an HTML page in the browser, anything else revealed in its folder.
pub fn open_file(path: &Path) -> Result<()> {
    if opens_in_browser(path) {
        open_with_default_app(path)
    } else {
        reveal(path)
    }
}

/// Open a file with whatever its type is associated with.
///
/// Only ever called for an HTML page; the association is the user's browser unless they
/// chose otherwise, and what they chose is what they expect a page to open in.
///
/// Not through Explorer, as revealing is: Explorer splits its command line on commas even
/// inside quotes, so a page under `a b,c\` silently never opened. `ShellExecuteW` takes the
/// path as a string with nothing to parse. It does block until the handler has started.
#[cfg(windows)]
fn open_with_default_app(path: &Path) -> Result<()> {
    use std::os::windows::ffi::OsStrExt;
    use windows_sys::Win32::System::Com::{
        CoInitializeEx, CoUninitialize, COINIT_APARTMENTTHREADED, COINIT_DISABLE_OLE1DDE,
    };
    use windows_sys::Win32::UI::Shell::ShellExecuteW;
    use windows_sys::Win32::UI::WindowsAndMessaging::SW_SHOWNORMAL;

    let wide = |text: &std::ffi::OsStr| -> Vec<u16> { text.encode_wide().chain([0]).collect() };
    let verb = wide("open".as_ref());
    let file = wide(path.as_os_str());
    // A handler may be a COM shell extension, which needs COM set up on the calling thread.
    // A thread of its own keeps that from changing the UI thread's apartment under
    // anything else that uses COM there, like the clipboard.
    let result = std::thread::spawn(move || unsafe {
        let initialized = CoInitializeEx(
            std::ptr::null(),
            (COINIT_APARTMENTTHREADED | COINIT_DISABLE_OLE1DDE) as u32,
        );
        let instance = ShellExecuteW(
            std::ptr::null_mut(),
            verb.as_ptr(),
            file.as_ptr(),
            std::ptr::null(),
            std::ptr::null(),
            SW_SHOWNORMAL,
        );
        if initialized >= 0 {
            CoUninitialize();
        }
        // Anything above 32 is success; at or below it is an error code.
        instance as isize
    })
    .join()
    .map_err(|_| anyhow::anyhow!("Could not open the browser"))?;
    if result <= 32 {
        bail!("Could not open the browser (error {result})");
    }
    Ok(())
}

/// Open a file with whatever its type is associated with. See the Windows version.
#[cfg(not(windows))]
fn open_with_default_app(path: &Path) -> Result<()> {
    let program = if cfg!(target_os = "macos") {
        "open"
    } else {
        "xdg-open"
    };
    Command::new(program)
        .arg(path)
        .spawn()
        .context("Could not open the browser")?;
    Ok(())
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
        // Classified as a file like any other, and not a web page — so `open_file` reveals
        // it, the only thing it does with a file that is not one.
        assert_eq!(
            classify("file:///C:/Windows/System32/calc.exe"),
            Some(Target::File(PathBuf::from(r"C:\Windows\System32\calc.exe")))
        );
        assert!(!opens_in_browser(Path::new(
            r"C:\Windows\System32\calc.exe"
        )));
    }

    #[test]
    fn an_html_page_opens_in_the_browser_because_revealing_it_only_added_a_double_click() {
        for name in ["report.html", "index.htm", "PREVIEW.HTML", "My View.Htm"] {
            assert!(is_web_page(Path::new(name)), "{name} should open");
        }
    }

    #[test]
    fn only_the_exact_html_extensions_open_because_anything_else_could_run_as_a_program() {
        // `.hta` is HTML that Windows runs as a trusted application, outside any browser.
        for name in [
            "app.hta",
            "run.exe",
            "script.ps1",
            "shortcut.lnk",
            "page.html.exe",
            "page.xhtml",
            "page.shtml",
            "html",
            ".html",
        ] {
            assert!(
                !is_web_page(Path::new(name)),
                "{name} must only be revealed"
            );
        }
    }

    #[test]
    fn a_folder_named_like_a_page_is_revealed_because_a_browser_would_only_list_it() {
        let folder = tempfile::Builder::new()
            .suffix(".html")
            .tempdir()
            .expect("temporary folder");
        assert!(!opens_in_browser(folder.path()));

        let page = folder.path().join("report.html");
        std::fs::write(&page, "<p>hi</p>").expect("page");
        assert!(opens_in_browser(&page));
    }

    #[test]
    fn a_missing_page_is_revealed_so_the_click_still_shows_its_folder() {
        let folder = tempfile::tempdir().expect("temporary folder");
        assert!(!opens_in_browser(&folder.path().join("gone.html")));
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

    /// Hands a real file to the real file manager, or a real page to the real browser.
    ///
    /// Everything else here runs with launching compiled out, so this is the only proof the
    /// launches themselves are right — Explorer's `/select,` in particular, which has to be
    /// built by hand. Aim it at a script that leaves a marker behind if it ever runs: the
    /// point of revealing rather than opening is that the marker never appears. Aim it at an
    /// `.html` in a folder whose name has a space and a comma, and the page itself should
    /// open — Explorer could not do that, which is why pages do not go through it.
    #[test]
    #[ignore = "opens a file manager or browser window; run with CST_LINKS_LIVE_FILE=<path>"]
    fn a_file_link_really_opens_a_page_or_reveals_anything_else() {
        let Ok(path) = std::env::var("CST_LINKS_LIVE_FILE") else {
            return;
        };
        let Some(Target::File(path)) = classify(&path) else {
            panic!("{path} is not a file link");
        };
        open_file(&path).expect("the file manager or browser should start");
    }

    #[test]
    fn malformed_percent_escapes_are_refused() {
        assert_eq!(classify("file:///C:/a%2"), None);
        assert_eq!(classify("file:///C:/a%zz"), None);
    }
}

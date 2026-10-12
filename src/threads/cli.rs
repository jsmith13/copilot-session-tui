//! The `cst thread` commands an agent runs to take part in a conversation.
//!
//! These run in the agent's own shell, not in the TUI, so they follow the `cst doctor`
//! convention: every line of wording comes from a pure function, plain sentences to
//! stdout, no colour and no non-ASCII glyphs. The agent reads this output as tool
//! output, and a human reads it when running the same command to check on things.

use anyhow::Result;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::Duration;

use super::store::{self, ThreadState};
use super::{current_session_id, parse_thread_url, Subscription, ThreadKind, ThreadRef};

/// Shorten a Copilot session UUID for display, matching what the TUI shows in titles.
fn short(session_id: &str) -> &str {
    session_id.get(..8).unwrap_or(session_id)
}

/// Subscribe without posting.
///
/// The agent that *created* an item with `gh issue create` needs this: it is already a
/// participant as far as the conversation is concerned, but it has posted nothing
/// through CST, so nothing has recorded its interest.
pub fn watch(root: &Path, session_id: &str, url: &str) -> Result<String> {
    let thread = parse_thread_url(url).map_err(anyhow::Error::msg)?;
    let added = store::update_in(root, |state| state.subscribe(session_id, thread.clone()))?;
    Ok(render_watch(&thread, session_id, added))
}

fn render_watch(thread: &ThreadRef, session_id: &str, added: bool) -> String {
    if added {
        format!(
            "Watching {} as session {}.\nYou will be woken when someone comments on it.",
            thread.url(),
            short(session_id)
        )
    } else {
        format!(
            "Session {} was already watching {}.",
            short(session_id),
            thread.url()
        )
    }
}

/// How long to wait for GitHub to accept a comment before giving up.
///
/// Generous, because failing here is worse than waiting: the agent has already composed
/// the message and has no good way to retry without risking a double post.
const POST_TIMEOUT: Duration = Duration::from_secs(30);

/// Comment on a thread, and start watching it.
///
/// Posting is what joins a conversation — there is no separate "join" step and no
/// addressing. That is the whole reason a human can introduce two agents just by
/// pointing the second one at a URL.
pub fn post(root: &Path, session_id: &str, url: &str, body: &str) -> Result<String> {
    let thread = parse_thread_url(url).map_err(anyhow::Error::msg)?;
    let cwd = std::env::current_dir().unwrap_or_else(|_| PathBuf::from("."));

    let cancelled = Arc::new(AtomicBool::new(false));
    let watchdog = Arc::clone(&cancelled);
    std::thread::spawn(move || {
        std::thread::sleep(POST_TIMEOUT);
        watchdog.store(true, Ordering::Release);
    });

    let posted = crate::github::post_comment(
        cwd,
        crate::github::CommentTarget {
            host: &thread.host,
            owner: &thread.owner,
            repo: &thread.repo,
            number: thread.number,
            discussion: thread.kind == ThreadKind::Discussion,
        },
        &with_footer(body, session_id),
        cancelled,
    )
    .map_err(|error| anyhow::anyhow!("{error}"))?;

    // Recorded after the post succeeds, never before: a subscription that claims to
    // have written a comment that does not exist would silently swallow somebody
    // else's comment that later happens to share the id.
    remember_own_comment(root, session_id, &thread, &posted.id)?;
    Ok(render_post(&thread, session_id, &posted.url))
}

/// Sign a comment with the session that wrote it.
///
/// Every CST agent posts as the user's own account, so on GitHub an agent's comment
/// and one the user typed in a browser look identical, and two agents in one thread
/// cannot be told apart. The full id rather than the short one, because it is what
/// `--session` takes and what finds the session again.
///
/// For people reading the thread only. CST never reads it back: anybody can type this
/// line into a comment, and the login is what decides who is trusted.
fn with_footer(body: &str, session_id: &str) -> String {
    // A blank line first. Without it the footer joins the last paragraph, and a line
    // of dashes as a separator would turn that paragraph into a heading instead.
    format!(
        "{}\n\n<sub>Posted by CST from session `{session_id}`</sub>",
        body.trim_end()
    )
}

fn render_post(thread: &ThreadRef, session_id: &str, comment_url: &str) -> String {
    format!(
        "Posted to {}.\n{}\nSession {} is now watching this thread and will be woken \
         when someone replies.",
        thread.url(),
        comment_url,
        short(session_id)
    )
}

/// Stop being woken by a thread, without affecting anyone else watching it.
pub fn leave(root: &Path, session_id: &str, url: &str) -> Result<String> {
    let thread = parse_thread_url(url).map_err(anyhow::Error::msg)?;
    let removed = store::update_in(root, |state| state.leave(session_id, &thread))?;
    Ok(render_leave(&thread, session_id, removed))
}

fn render_leave(thread: &ThreadRef, session_id: &str, removed: bool) -> String {
    if removed {
        format!(
            "Session {} has left {}.\nOther sessions watching it are unaffected.",
            short(session_id),
            thread.url()
        )
    } else {
        format!(
            "Session {} was not watching {}, so nothing changed.",
            short(session_id),
            thread.url()
        )
    }
}

/// End a correspondence for everyone watching it locally.
///
/// Distinct from `leave`, which is one session stepping back from a conversation that
/// carries on without it. This says the whole thing is finished. It does not close the
/// GitHub item — that is a separate decision a human usually wants to make.
pub fn close(root: &Path, url: &str) -> Result<String> {
    let thread = parse_thread_url(url).map_err(anyhow::Error::msg)?;
    let dropped = store::update_in(root, |state| state.close(&thread))?;
    Ok(render_close(&thread, dropped))
}

fn render_close(thread: &ThreadRef, dropped: usize) -> String {
    if dropped == 0 {
        return format!("No session was watching {}.", thread.url());
    }
    format!(
        "Closed {}.\n{dropped} session{} will no longer be woken by it. The GitHub item \
         itself is untouched.",
        thread.url(),
        if dropped == 1 { "" } else { "s" }
    )
}

/// What this session is watching, and anything it has been unable to receive.
pub fn list(root: &Path, session_id: &str) -> Result<String> {
    let state = store::load_in(root);
    let subscriptions = state.subscriptions_for(session_id);
    let pending: Vec<&super::Notice> = state
        .waiting_for_user()
        .into_iter()
        .filter(|notice| notice.session_id == session_id)
        .collect();
    Ok(render_list(&subscriptions, &pending, session_id))
}

fn render_list(
    subscriptions: &[&Subscription],
    pending: &[&super::Notice],
    session_id: &str,
) -> String {
    if subscriptions.is_empty() && pending.is_empty() {
        return format!(
            "Session {} is not watching any threads.\nUse `{cli} thread watch <url>` or \
             `{cli} thread post <url>` to start.",
            short(session_id),
            cli = super::AGENT_CLI
        );
    }

    let mut out = format!(
        "Session {} is watching {} thread{}:",
        short(session_id),
        subscriptions.len(),
        if subscriptions.len() == 1 { "" } else { "s" }
    );
    for subscription in subscriptions {
        // Naming the state only when it is not the ordinary one keeps the common case
        // scannable.
        let state = if subscription.is_active() {
            String::new()
        } else {
            " (paused)".to_string()
        };
        out.push_str(&format!(
            "\n  {} [{}]{}",
            subscription.thread.url(),
            subscription.thread.kind.label(),
            state
        ));
    }

    // Said here as well as in the TUI, because an agent asking what it is watching is
    // usually asking because it is wondering whether an answer arrived.
    if !pending.is_empty() {
        out.push_str(&format!(
            "\n\n{} message{} could not be delivered and {} waiting for the user:",
            pending.len(),
            if pending.len() == 1 { "" } else { "s" },
            if pending.len() == 1 { "is" } else { "are" }
        ));
        for held in pending {
            out.push_str(&format!(
                "\n  {} ({})",
                held.thread.url(),
                held.reason().map_or("waiting", |reason| reason.describe())
            ));
        }
    }
    out
}

/// Render a report and print it, so the commands share one output path.
pub fn emit(out: &mut impl Write, report: &str) -> std::io::Result<()> {
    writeln!(out, "{report}")
}

/// Resolve which session a command is acting for, turning the failure into an error
/// with a sentence the person reading it can act on.
pub fn resolve_session(explicit: Option<&str>) -> Result<String> {
    current_session_id(explicit).map_err(anyhow::Error::msg)
}

/// The state directory every command works against.
pub fn root() -> std::path::PathBuf {
    store::state_root()
}

/// Read a comment body from a file, or from stdin when the path is `-`.
///
/// Stdin is the reason this exists: comment bodies are multi-line prose, and making an
/// agent embed one in a shell argument invites quoting bugs that corrupt the message
/// on the way out.
pub fn read_body(body: Option<&str>, body_file: Option<&str>) -> Result<String> {
    let text = match (body, body_file) {
        (Some(_), Some(_)) => {
            anyhow::bail!("Use either --body or --body-file, not both")
        }
        (Some(body), None) => body.to_string(),
        (None, Some("-")) => {
            let mut buffer = String::new();
            std::io::Read::read_to_string(&mut std::io::stdin(), &mut buffer)?;
            buffer
        }
        (None, Some(path)) => std::fs::read_to_string(path)?,
        (None, None) => anyhow::bail!("Give the comment text with --body or --body-file"),
    };
    if text.trim().is_empty() {
        anyhow::bail!("The comment body is empty");
    }
    Ok(text)
}

/// Record that this session wrote a comment, so it is never woken by its own words.
pub fn remember_own_comment(
    root: &Path,
    session_id: &str,
    thread: &ThreadRef,
    comment_id: &str,
) -> Result<()> {
    store::update_in(root, |state: &mut ThreadState| {
        state.subscribe(session_id, thread.clone());
        if let Some(subscription) = state.subscription_mut(session_id, thread) {
            subscription.record_authored(comment_id);
        }
    })
}

#[cfg(test)]
mod tests {
    use super::super::ThreadKind;
    use super::*;

    fn thread() -> ThreadRef {
        ThreadRef {
            host: "github.com".to_string(),
            owner: "o".to_string(),
            repo: "r".to_string(),
            number: 12,
            kind: ThreadKind::Issue,
        }
    }

    #[test]
    fn watching_a_thread_says_what_will_happen_next_not_that_a_row_was_written() {
        let report = render_watch(&thread(), "0a1b2c3d-4e5f", true);

        assert!(report.contains("https://github.com/o/r/issues/12"));
        assert!(report.contains("woken"), "got: {report}");
        assert!(
            report.is_ascii(),
            "output must survive any console: {report}"
        );
    }

    #[test]
    fn watching_twice_says_so_rather_than_pretending_something_changed() {
        let report = render_watch(&thread(), "0a1b2c3d", false);

        assert!(report.contains("already watching"), "got: {report}");
    }

    #[test]
    fn leaving_a_thread_spells_out_that_other_sessions_keep_theirs() {
        // An agent needs to know this is not the same as closing the conversation, or
        // it will hesitate to tidy up after itself.
        let report = render_leave(&thread(), "0a1b2c3d", true);

        assert!(report.contains("unaffected"), "got: {report}");
    }

    #[test]
    fn an_empty_list_points_at_the_command_that_would_fill_it() {
        let report = render_list(&[], &[], "0a1b2c3d");

        assert!(report.contains("not watching any"), "got: {report}");
        assert!(
            report.contains("copilot-session-tui thread watch"),
            "got: {report}"
        );
    }

    #[test]
    fn the_list_names_every_watched_thread_with_its_kind() {
        let subscription = Subscription::new("session-a", thread());
        let report = render_list(&[&subscription], &[], "session-a");

        assert!(report.contains("https://github.com/o/r/issues/12"));
        assert!(report.contains("issue"), "got: {report}");
    }

    #[test]
    fn a_body_may_come_from_stdin_so_prose_is_never_mangled_by_shell_quoting() {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("body.md");
        std::fs::write(&path, "Line one\n\nLine two with \"quotes\" and $vars").unwrap();

        let body = read_body(None, Some(path.to_str().unwrap())).unwrap();

        assert!(body.contains("\"quotes\""));
        assert!(body.contains("$vars"));
    }

    #[test]
    fn an_empty_comment_is_refused_rather_than_posted() {
        assert!(read_body(Some("   "), None).is_err());
        assert!(read_body(None, None).is_err());
        assert!(read_body(Some("x"), Some("y")).is_err());
    }

    #[test]
    fn a_posted_comment_names_the_full_session_so_a_reader_can_tell_agents_from_the_user() {
        let session = "d6a86c02-d466-4f34-af86-db04c324d06c";

        let posted = with_footer("Here is the answer.\n\n", session);

        assert!(
            posted.starts_with("Here is the answer.\n\n<sub>"),
            "got: {posted:?}"
        );
        assert!(posted.contains(&format!("`{session}`")), "got: {posted:?}");
        assert!(posted.contains("CST"), "got: {posted:?}");
    }

    #[test]
    fn the_footer_is_its_own_paragraph_so_it_cannot_restyle_the_last_line_of_the_body() {
        // Glued to the last line it would render as part of that paragraph, and a
        // trailing list item or table row would swallow it outright.
        let posted = with_footer("- one\n- two", "session-a");

        let (body, footer) = posted.rsplit_once("\n\n").unwrap();
        assert_eq!(body, "- one\n- two");
        assert!(footer.starts_with("<sub>") && footer.ends_with("</sub>"));
        assert!(!footer.contains('\n'), "got: {footer:?}");
    }

    #[test]
    fn posting_records_the_comment_id_so_the_author_is_never_woken_by_itself() {
        let temp = tempfile::tempdir().unwrap();
        remember_own_comment(temp.path(), "session-a", &thread(), "4242").unwrap();

        let state = store::load_in(temp.path());
        let subscription = &state.subscriptions_for("session-a")[0];
        assert!(subscription.has_seen("4242"));
        // Posting also subscribes: taking part in a thread is what joins it.
        assert!(subscription.is_active());
    }

    #[test]
    fn a_command_run_outside_a_session_says_which_flag_to_pass() {
        let error = super::super::current_session_id(None).unwrap_err();

        assert!(error.contains("--session"), "got: {error}");
    }
}

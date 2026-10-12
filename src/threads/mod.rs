//! Agent-to-agent conversation over GitHub threads.
//!
//! An agent that files an issue asking another agent for something has no way to hear
//! the answer: it finishes its turn and nothing is listening when the reply lands. The
//! conversation itself already works — issues, pull requests and discussions carry it,
//! humans can read it, and it survives restarts. What is missing is a doorbell.
//!
//! A session subscribes to a thread by taking part in it. When the thread moves, CST
//! wakes that session with a *pointer* to the new comment and nothing else. The comment
//! body never becomes prompt text: it is written by whoever can comment on the thread,
//! and a session may be running with `--yolo` in a real repository.
//!
//! This module is the model and the rules. Every function here is pure, so the wording
//! and the security decisions can be tested without a network, a GitHub account, or a
//! running session. The I/O lives in the sibling modules.

pub mod cli;
pub mod doorbell;
pub mod judge;
pub mod store;
pub mod watcher;

use chrono::{DateTime, Duration as ChronoDuration, Utc};
use serde::{Deserialize, Serialize};

/// How a pane tells the agent running inside it which session it is.
///
/// `cst thread post` is a separate short-lived process; it cannot ask the TUI, and
/// inferring the session from the working directory would be wrong the moment two
/// sessions share a checkout.
pub const SESSION_ID_ENV: &str = "CST_SESSION_ID";

/// Absolute path to the CST an agent should call.
///
/// `cst` is a shell function the installer writes into a profile, so it does not exist in
/// the non-interactive shell an agent runs commands in — an agent told to run `cst` gets
/// "command not found" and reports the feature as unavailable. `copilot-session-tui` is a
/// real binary on `PATH`, and this is the belt to that braces: the exact executable of
/// the CST that opened the pane, which is also what the lifecycle hooks are materialized
/// with and for the same reason.
pub const CLI_PATH_ENV: &str = "CST_BIN";

/// Which session the calling process belongs to.
///
/// `explicit` wins so the commands stay usable outside a CST pane — running them by
/// hand is how you inspect or repair state. When neither is available the error names
/// the flag rather than the environment variable, because a human reading it is the one
/// who has to act.
pub fn current_session_id(explicit: Option<&str>) -> Result<String, String> {
    if let Some(session_id) = explicit.map(str::trim).filter(|value| !value.is_empty()) {
        return Ok(session_id.to_string());
    }
    std::env::var(SESSION_ID_ENV)
        .ok()
        .filter(|value| !value.trim().is_empty())
        .ok_or_else(|| {
            "This is not running inside a CST session. Pass --session <id> to say which \
             session to act for."
                .to_string()
        })
}

/// Which GitHub item a thread is, because the three are read and posted differently.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ThreadKind {
    Issue,
    PullRequest,
    Discussion,
}

impl ThreadKind {
    /// The path segment GitHub uses for this kind, so a canonical URL can be rebuilt.
    pub fn url_segment(self) -> &'static str {
        match self {
            Self::Issue => "issues",
            Self::PullRequest => "pull",
            Self::Discussion => "discussions",
        }
    }

    pub fn label(self) -> &'static str {
        match self {
            Self::Issue => "issue",
            Self::PullRequest => "pull request",
            Self::Discussion => "discussion",
        }
    }
}

/// A thread, identified the way a human would paste it.
///
/// The host is kept because CST already threads `--hostname` through every `gh` call,
/// and an Enterprise thread must not be confused with one on github.com.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ThreadRef {
    pub host: String,
    pub owner: String,
    pub repo: String,
    pub number: u64,
    pub kind: ThreadKind,
}

impl ThreadRef {
    /// Rebuild the canonical URL rather than storing whatever was pasted.
    ///
    /// Two subscriptions to the same thread must compare equal even when one was added
    /// from a link carrying a `#issuecomment-…` anchor and the other from a bare URL.
    pub fn url(&self) -> String {
        format!(
            "https://{}/{}/{}/{}/{}",
            self.host,
            self.owner,
            self.repo,
            self.kind.url_segment(),
            self.number
        )
    }
}

/// Parse a GitHub thread URL.
///
/// Deliberately stricter than [`crate::github::parse_item_spec`], which only needs a
/// number because the repository comes from the session's working directory. A thread
/// is introduced by a human pasting a link to somewhere else, so the repository has to
/// come from the link itself.
pub fn parse_thread_url(input: &str) -> Result<ThreadRef, String> {
    let trimmed = input.trim();
    if trimmed.is_empty() {
        return Err("Give the URL of a GitHub issue, pull request, or discussion".to_string());
    }

    let without_scheme = trimmed
        .strip_prefix("https://")
        .or_else(|| trimmed.strip_prefix("http://"))
        .ok_or_else(|| format!("Not a GitHub URL: {trimmed}"))?;

    // Anything after the item number is GitHub's own decoration — a comment anchor, a
    // `/files` tab, a query string. Dropping it is what makes two links to one thread
    // land on a single subscription.
    let path = without_scheme
        .split(['#', '?'])
        .next()
        .unwrap_or(without_scheme);
    let mut parts = path.split('/').filter(|part| !part.is_empty());

    let host = parts
        .next()
        .ok_or_else(|| format!("Not a GitHub URL: {trimmed}"))?;
    let owner = parts
        .next()
        .ok_or_else(|| format!("That URL names no repository: {trimmed}"))?;
    let repo = parts
        .next()
        .ok_or_else(|| format!("That URL names no repository: {trimmed}"))?;
    let segment = parts.next().ok_or_else(|| {
        format!("That URL names no issue, pull request, or discussion: {trimmed}")
    })?;

    let kind = match segment {
        "issues" => ThreadKind::Issue,
        "pull" | "pulls" => ThreadKind::PullRequest,
        "discussions" => ThreadKind::Discussion,
        other => {
            return Err(format!(
                "Expected an issue, pull request, or discussion URL, found `{other}`"
            ))
        }
    };

    let number: u64 = parts
        .next()
        .ok_or_else(|| format!("That URL has no item number: {trimmed}"))?
        .parse()
        .map_err(|_| format!("That URL has no item number: {trimmed}"))?;
    if number == 0 {
        return Err("Item numbers start at 1".to_string());
    }

    Ok(ThreadRef {
        host: host.to_ascii_lowercase(),
        owner: owner.to_string(),
        repo: repo.to_string(),
        number,
        kind,
    })
}

/// A comment as the doorbell sees it: enough to decide, and nothing more.
///
/// The body is absent on purpose. Nothing downstream is allowed to put comment text
/// into a prompt, and the surest way to guarantee that is to never carry it.
///
/// Comment ids are strings because the three kinds do not agree on what an id is:
/// issue and pull request comments carry a numeric REST id, while discussion comments
/// only have an opaque GraphQL node id. Stringifying the numeric one costs nothing and
/// lets one set of rules cover all three.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CommentSighting {
    pub id: String,
    pub author: String,
    pub url: String,
    pub created_at: DateTime<Utc>,
}

/// What should happen because of one newly seen comment.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Verdict {
    /// One of our own posts coming back to us.
    SkipOwn,
    /// Another CST agent, or the user commenting from a browser. Wake the session.
    Wake,
    /// Written by somebody who is not us. Nothing runs, and the user is not asked.
    Ignore,
}

/// Why a message is waiting on the user instead of going straight to a session.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PendingReason {
    /// The session has no running pane, and CST does not start one unasked.
    SessionClosed,
    /// Written by a login that is not the one our agents post as.
    ///
    /// No longer produced, because such comments are now ignored. It stays so that a
    /// `threads.json` written by an older CST still loads: `load_in` reads a file it
    /// cannot parse as empty, so dropping a variant a file still holds would drop every
    /// subscription with it.
    ForeignAuthor,
    /// This thread has woken this session too many times in the last hour.
    Throttled,
}

impl PendingReason {
    /// Said to the user, so it names the consequence rather than the rule.
    pub fn describe(self) -> &'static str {
        match self {
            Self::SessionClosed => "its session is closed",
            Self::ForeignAuthor => "it was written by someone else",
            Self::Throttled => "this thread has woken it repeatedly",
        }
    }
}

/// Decide what a newly seen comment means for one subscription.
///
/// Every CST agent posts as the same GitHub account, so the API cannot say which agent
/// wrote a comment — a login tells us only whether it came from outside. Recognising
/// our own writing is therefore local: [`Subscription::authored_comment_ids`] records
/// what this session posted, at the moment it posted it.
///
/// The middle case does more work than it looks. A comment from our own login that this
/// session did not write is either another agent or the user typing into GitHub in a
/// browser, and both should wake the session. That is why a human can steer a
/// conversation just by commenting on it, with no mechanism of its own.
pub fn classify(
    comment: &CommentSighting,
    our_login: &str,
    trusted: &[String],
    subscription: &Subscription,
) -> Verdict {
    if !comment.author.eq_ignore_ascii_case(our_login) {
        // A named collaborator is treated like one of our own agents, because that is
        // what working with somebody's agent means. It is a real widening of the trust
        // boundary — a comment from a trusted login starts work on this machine — so it
        // is a list somebody types out, never inferred from who happens to be talking.
        if trusted
            .iter()
            .any(|login| login.eq_ignore_ascii_case(&comment.author))
        {
            return Verdict::Wake;
        }
        // Ignored rather than raised with the user. Asking about every stranger's
        // comment turned a busy public thread into a queue of questions whose only safe
        // answer was "no". Somebody whose comments should start work goes on the
        // trusted list instead.
        return Verdict::Ignore;
    }
    if subscription
        .authored_comment_ids
        .iter()
        .any(|id| id == &comment.id)
    {
        return Verdict::SkipOwn;
    }
    Verdict::Wake
}

/// How long a wake-up counts against the per-thread rate limit.
const THROTTLE_WINDOW_HOURS: i64 = 1;

/// One session's interest in one thread.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Subscription {
    pub session_id: String,
    pub thread: ThreadRef,
    #[serde(default = "default_active")]
    pub state: SubscriptionState,
    /// Comment ids already accounted for, so a restart does not replay a whole thread.
    ///
    /// A set rather than a high-water mark because discussion comment ids are opaque
    /// and cannot be ordered, and because a comment edited after the fact keeps its id.
    #[serde(default)]
    pub seen_comment_ids: Vec<String>,
    /// Comment ids this session posted. The only way to recognise our own writing.
    #[serde(default)]
    pub authored_comment_ids: Vec<String>,
    #[serde(default = "Utc::now")]
    pub subscribed_at: DateTime<Utc>,
    /// When this thread woke this session, pruned to the throttle window.
    #[serde(default)]
    pub wakeups: Vec<DateTime<Utc>>,
}

fn default_active() -> SubscriptionState {
    SubscriptionState::Active
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SubscriptionState {
    Active,
    /// The user stopped the wake-ups but kept the record, so it can be resumed.
    Paused,
}

impl Subscription {
    pub fn new(session_id: impl Into<String>, thread: ThreadRef) -> Self {
        Self {
            session_id: session_id.into(),
            thread,
            state: SubscriptionState::Active,
            seen_comment_ids: Vec::new(),
            authored_comment_ids: Vec::new(),
            subscribed_at: Utc::now(),
            wakeups: Vec::new(),
        }
    }

    pub fn is_active(&self) -> bool {
        self.state == SubscriptionState::Active
    }

    /// Whether another wake-up is allowed right now, pruning the window as it goes.
    ///
    /// A rate rather than a total: an agent cannot estimate how many exchanges a piece
    /// of work needs, so a budget would either cut off real conversations or be set so
    /// high it never binds. A rate asks nobody to estimate anything.
    pub fn may_wake(&mut self, now: DateTime<Utc>, per_hour: u32) -> bool {
        let cutoff = now - ChronoDuration::hours(THROTTLE_WINDOW_HOURS);
        self.wakeups.retain(|moment| *moment > cutoff);
        (self.wakeups.len() as u32) < per_hour
    }

    pub fn record_wakeup(&mut self, now: DateTime<Utc>) {
        self.wakeups.push(now);
    }

    pub fn record_authored(&mut self, comment_id: &str) {
        if !self.authored_comment_ids.iter().any(|id| id == comment_id) {
            self.authored_comment_ids.push(comment_id.to_string());
        }
        self.mark_seen(comment_id);
    }

    /// Whether this comment has already been accounted for.
    pub fn has_seen(&self, comment_id: &str) -> bool {
        self.seen_comment_ids.iter().any(|id| id == comment_id)
    }

    /// Remember a comment so it is never processed twice.
    ///
    /// Pruned from the front: an old id can only cause a repeat if the thread has since
    /// gained [`SEEN_HISTORY`] newer comments, by which point replaying it would be the
    /// lesser problem than an unbounded file.
    pub fn mark_seen(&mut self, comment_id: &str) {
        if self.has_seen(comment_id) {
            return;
        }
        self.seen_comment_ids.push(comment_id.to_string());
        if self.seen_comment_ids.len() > SEEN_HISTORY {
            let excess = self.seen_comment_ids.len() - SEEN_HISTORY;
            self.seen_comment_ids.drain(..excess);
        }
    }
}

/// How many comment ids to remember per subscription.
const SEEN_HISTORY: usize = 200;

/// Wake-ups one thread may give one session per hour before the rest wait for the user.
///
/// Twelve is deliberately loose. It is a runaway guard, not a conversation limit: a real
/// exchange settling something takes a handful of turns, and a pair of agents talking in
/// circles will blow past this within minutes.
pub const DEFAULT_WAKEUPS_PER_HOUR: u32 = 12;

/// How far a notice has got.
///
/// On disk rather than in memory, because the interesting states are exactly the ones a
/// restart used to lose: a notice decided on but not yet written, and one written into a
/// composer that no turn has taken. Both were held in memory while the comment that
/// caused them was already recorded as seen, so stopping CST in that window meant the
/// message was never mentioned again.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case", tag = "state")]
pub enum NoticeStatus {
    /// Decided on; nothing has been written to the session yet.
    Planned,
    /// Written into the composer, still waiting for a turn to take it.
    Sent,
    /// Not going anywhere without the user saying so.
    Waiting { reason: PendingReason },
}

/// One notice about one thread, for one session.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Notice {
    pub session_id: String,
    pub thread: ThreadRef,
    pub comment_url: String,
    /// Who wrote it, when that is why the user is being asked.
    #[serde(default)]
    pub author: Option<String>,
    /// Flattened, so the file reads as a status column — `"state": "planned"` on the
    /// notice itself — rather than a nested object nobody would recognise at a glance.
    #[serde(flatten)]
    pub status: NoticeStatus,
    /// When this was decided on.
    ///
    /// Drives both the age shown in the session list and the deadline after which a
    /// notice stops waiting for a busy pane. Persisted so neither is reset by a restart.
    #[serde(default = "Utc::now")]
    pub planned_at: DateTime<Utc>,
}

impl Notice {
    pub fn is_waiting_for_user(&self) -> bool {
        matches!(self.status, NoticeStatus::Waiting { .. })
    }

    pub fn reason(&self) -> Option<PendingReason> {
        match self.status {
            NoticeStatus::Waiting { reason } => Some(reason),
            _ => None,
        }
    }

    /// How long ago this was decided on, never negative.
    ///
    /// A clock that moved backwards between runs would otherwise read as a notice from
    /// the future, and the patience deadline would never fire.
    pub fn age_seconds(&self) -> i64 {
        Utc::now()
            .signed_duration_since(self.planned_at)
            .num_seconds()
            .max(0)
    }
}

/// A message that arrived but was not delivered, waiting on the user.
///
/// Superseded by [`Notice`]; kept so a file written by the version that used it still
/// loads, with its entries folded in as notices already waiting for the user.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PendingDelivery {
    pub session_id: String,
    pub thread: ThreadRef,
    pub comment_url: String,
    pub reason: PendingReason,
    /// Who wrote it, when that is the reason it is here.
    ///
    /// The decision being asked of the user is whether to let this person's words start
    /// work on their machine, and "someone else" is not enough to answer that. It is also
    /// the name they would add to the trusted list if the answer is yes.
    #[serde(default)]
    pub author: Option<String>,
    /// Drives the age shown in the session list, so a stalled correspondence becomes
    /// visible instead of waiting silently forever.
    #[serde(default = "Utc::now")]
    pub arrived_at: DateTime<Utc>,
}

/// The entire text a woken session is given.
///
/// A pointer, never the comment. The body of a GitHub comment is written by anyone who
/// can reach the thread, and handing it to an agent as prompt text hands an outsider a
/// prompt for a process that can edit files and run commands. Fetching it through the
/// agent's own tools makes it data the agent chose to read.
///
/// One line, which also keeps it clear of `Pane::send_prompt_snippet`'s refusal to
/// paste multiline text before the child has enabled bracketed paste.
/// The command name given to agents, which is deliberately not `cst`.
///
/// `cst` is a shell function written into a profile by the installer, so it is absent
/// from the non-interactive shell an agent runs commands in. Telling an agent to use it
/// makes the whole feature look unavailable — which is exactly how this was found.
pub const AGENT_CLI: &str = "copilot-session-tui";

pub fn wake_pointer(thread: &ThreadRef) -> String {
    let url = thread.url();
    format!(
        "A new comment arrived on {url} — read it and decide whether it changes your \
         work. If this thread no longer concerns you, run: {AGENT_CLI} thread leave {url}"
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    fn thread() -> ThreadRef {
        ThreadRef {
            host: "github.com".to_string(),
            owner: "microsoft".to_string(),
            repo: "SpeakingBigMapsIntoExistence".to_string(),
            number: 2366,
            kind: ThreadKind::Issue,
        }
    }

    fn sighting(id: &str, author: &str) -> CommentSighting {
        CommentSighting {
            id: id.to_string(),
            author: author.to_string(),
            url: format!("{}#issuecomment-{id}", thread().url()),
            created_at: Utc::now(),
        }
    }

    #[test]
    fn a_comment_this_session_posted_never_wakes_it_back_up() {
        let mut subscription = Subscription::new("session-a", thread());
        subscription.record_authored("99");

        assert_eq!(
            classify(&sighting("99", "tupini07"), "tupini07", &[], &subscription),
            Verdict::SkipOwn
        );
    }

    #[test]
    fn a_comment_from_another_agent_on_the_same_account_wakes_the_session() {
        // Every CST agent posts as the same login, so this is the ordinary case: the
        // author matches, but this session did not write it.
        let subscription = Subscription::new("session-a", thread());

        assert_eq!(
            classify(&sighting("100", "tupini07"), "tupini07", &[], &subscription),
            Verdict::Wake
        );
    }

    #[test]
    fn the_user_commenting_from_a_browser_wakes_the_session_like_any_other_agent() {
        // This is the whole human-intervention story: commenting on the thread is how
        // you redirect two agents, and it needs no mechanism of its own.
        let mut subscription = Subscription::new("session-a", thread());
        subscription.record_authored("1");

        assert_eq!(
            classify(&sighting("2", "TUPINI07"), "tupini07", &[], &subscription),
            Verdict::Wake
        );
    }

    #[test]
    fn a_comment_from_an_outsider_is_ignored_and_never_wakes_anything() {
        // The security property. A stranger commenting on a public thread must not be
        // able to start work on this machine.
        let subscription = Subscription::new("session-a", thread());

        assert_eq!(
            classify(
                &sighting("101", "drive-by-contributor"),
                "tupini07",
                &[],
                &subscription
            ),
            Verdict::Ignore
        );
    }

    #[test]
    fn a_named_collaborator_wakes_the_session_like_one_of_our_own_agents() {
        // What working with somebody else's agent means. Their comment has to be able to
        // start a turn, or the conversation only ever runs in one direction.
        let subscription = Subscription::new("session-a", thread());
        let trusted = vec!["Coworker".to_string()];

        assert_eq!(
            classify(
                &sighting("101", "coworker"),
                "tupini07",
                &trusted,
                &subscription
            ),
            Verdict::Wake,
            "GitHub logins are case-insensitive and the list is typed by hand"
        );
    }

    #[test]
    fn trusting_one_person_does_not_trust_everybody_else() {
        // The whole point of a list. A stranger on a public thread stays a stranger
        // however many colleagues have been added.
        let subscription = Subscription::new("session-a", thread());
        let trusted = vec!["coworker".to_string()];

        assert_eq!(
            classify(
                &sighting("102", "drive-by-contributor"),
                "tupini07",
                &trusted,
                &subscription
            ),
            Verdict::Ignore
        );
    }

    #[test]
    fn nobody_is_trusted_until_somebody_is_named() {
        // The default has to stay closed: a fresh install must not let any commenter
        // start work, and an empty list is what a fresh install has.
        let subscription = Subscription::new("session-a", thread());

        assert_eq!(
            classify(&sighting("103", "anyone"), "tupini07", &[], &subscription),
            Verdict::Ignore
        );
    }

    #[test]
    fn the_prompt_handed_to_a_woken_agent_carries_no_comment_text() {
        let pointer = wake_pointer(&thread());

        assert!(pointer.contains("/microsoft/SpeakingBigMapsIntoExistence/issues/2366"));
        assert!(pointer.contains("copilot-session-tui thread leave"));
        // Never the bare `cst`: that is a shell function from a human's profile, absent
        // from the shell an agent runs commands in. An agent told to use it gets
        // "command not found" and reports the whole feature as unavailable.
        assert!(
            !pointer.contains(" cst "),
            "the pointer must not name a command agents cannot run: {pointer}"
        );
        // One line, or `send_prompt_snippet` refuses it before bracketed paste is on.
        assert!(!pointer.contains('\n'), "got: {pointer}");
    }

    #[test]
    fn two_links_to_the_same_thread_become_one_subscription() {
        let bare = parse_thread_url("https://github.com/o/r/issues/12").unwrap();
        let anchored =
            parse_thread_url("https://github.com/o/r/issues/12#issuecomment-55").unwrap();
        let queried = parse_thread_url("https://github.com/o/r/issues/12?foo=bar").unwrap();

        assert_eq!(bare, anchored);
        assert_eq!(bare, queried);
        assert_eq!(bare.url(), "https://github.com/o/r/issues/12");
    }

    #[test]
    fn all_three_item_kinds_are_accepted_because_all_three_carry_conversations() {
        assert_eq!(
            parse_thread_url("https://github.com/o/r/issues/1")
                .unwrap()
                .kind,
            ThreadKind::Issue
        );
        assert_eq!(
            parse_thread_url("https://github.com/o/r/pull/2")
                .unwrap()
                .kind,
            ThreadKind::PullRequest
        );
        assert_eq!(
            parse_thread_url("https://github.com/o/r/discussions/3")
                .unwrap()
                .kind,
            ThreadKind::Discussion
        );
    }

    #[test]
    fn an_enterprise_host_is_kept_so_it_is_not_confused_with_github_com() {
        let thread = parse_thread_url("https://github.mycorp.example/o/r/issues/4").unwrap();

        assert_eq!(thread.host, "github.mycorp.example");
        assert_eq!(thread.url(), "https://github.mycorp.example/o/r/issues/4");
    }

    #[test]
    fn a_url_that_names_no_thread_is_refused_rather_than_guessed_at() {
        for input in [
            "",
            "not a url",
            "https://github.com/owner",
            "https://github.com/o/r",
            "https://github.com/o/r/releases/tag/v1",
            "https://github.com/o/r/issues/0",
            "https://github.com/o/r/issues/none",
        ] {
            assert!(
                parse_thread_url(input).is_err(),
                "{input:?} should not parse as a thread"
            );
        }
    }

    #[test]
    fn the_throttle_counts_the_last_hour_rather_than_capping_a_conversation_forever() {
        let mut subscription = Subscription::new("session-a", thread());
        let now = Utc::now();

        for _ in 0..3 {
            assert!(subscription.may_wake(now, 3));
            subscription.record_wakeup(now);
        }
        assert!(
            !subscription.may_wake(now, 3),
            "the fourth wake within an hour is held"
        );

        // An hour later the same thread may wake it again: a long correspondence that
        // keeps producing work is not the thing being limited.
        let later = now + ChronoDuration::hours(2);
        assert!(subscription.may_wake(later, 3));
        assert!(
            subscription.wakeups.is_empty(),
            "the stale window is pruned"
        );
    }
}

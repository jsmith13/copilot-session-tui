use serde::de::DeserializeOwned;
use serde::Deserialize;
use std::fmt;
use std::io::Read;
use std::path::{Path, PathBuf};
use std::process::{Command, ExitStatus, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::Duration;

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct RepositoryRef {
    pub host: String,
    pub owner: String,
    pub name: String,
}

fn discussion_reference_alias(number: u64) -> String {
    format!("d{number}")
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct GraphDiscussion {
    number: u64,
    title: String,
    body: Option<String>,
    url: String,
    created_at: String,
    updated_at: String,
    closed: bool,
    is_answered: Option<bool>,
    answer_chosen_at: Option<String>,
    upvote_count: u64,
    #[serde(default)]
    reaction_groups: Vec<GraphReactionGroup>,
    author: Option<GraphActor>,
    category: GraphDiscussionCategory,
    comments: GraphConnection<GraphDiscussionComment>,
}

#[derive(Debug, Deserialize)]
struct GraphDiscussionCategory {
    name: String,
    #[serde(rename = "isAnswerable", default)]
    is_answerable: bool,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct GraphDiscussionComment {
    id: String,
    body: Option<String>,
    created_at: String,
    upvote_count: u64,
    is_answer: bool,
    #[serde(default)]
    reaction_groups: Vec<GraphReactionGroup>,
    author: Option<GraphActor>,
    replies: GraphConnection<GraphDiscussionReply>,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct GraphDiscussionReply {
    body: Option<String>,
    created_at: String,
    upvote_count: u64,
    #[serde(default)]
    reaction_groups: Vec<GraphReactionGroup>,
    author: Option<GraphActor>,
}

#[derive(Debug, Deserialize)]
struct GraphReactionGroup {
    content: String,
    reactors: GraphTotalCount,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct GraphTotalCount {
    total_count: u64,
}

#[derive(Debug, Deserialize)]
struct GraphDiscussionCommentsData {
    repository: Option<GraphDiscussionCommentsRepository>,
}

#[derive(Debug, Deserialize)]
struct GraphDiscussionCommentsRepository {
    discussion: Option<GraphDiscussionCommentsPage>,
}

#[derive(Debug, Deserialize)]
struct GraphDiscussionCommentsPage {
    comments: GraphConnection<GraphDiscussionComment>,
}

#[derive(Debug, Deserialize)]
struct GraphDiscussionRepliesData {
    node: Option<GraphDiscussionRepliesNode>,
}

#[derive(Debug, Deserialize)]
struct GraphDiscussionRepliesNode {
    replies: GraphConnection<GraphDiscussionReply>,
}

impl RepositoryRef {
    pub fn name_with_owner(&self) -> String {
        format!("{}/{}", self.owner, self.name)
    }

    fn endpoint(&self, suffix: &str) -> String {
        format!("repos/{}/{}/{}", self.owner, self.name, suffix)
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Author {
    pub login: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Label {
    pub name: String,
    pub color: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ItemCommon {
    pub repository: RepositoryRef,
    pub number: u64,
    pub title: String,
    pub state: String,
    pub author: Author,
    pub labels: Vec<Label>,
    pub created_at: String,
    pub updated_at: String,
    pub url: String,
    pub body: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DiscussionKind {
    Comment,
    Review,
    InlineReview,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DiscussionEntry {
    pub kind: DiscussionKind,
    pub author: Author,
    pub body: String,
    pub created_at: String,
    pub review_state: Option<String>,
    /// Resolution belongs to the review thread, so every inline reply in it shares
    /// this value. Other comment kinds, and REST fallback data, leave it unknown.
    pub thread_resolved: Option<bool>,
    pub path: Option<String>,
    pub line: Option<u64>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ChangedFile {
    pub path: String,
    pub status: String,
    pub additions: u64,
    pub deletions: u64,
    pub changes: u64,
    pub patch: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Issue {
    pub common: ItemCommon,
    pub comments: Vec<DiscussionEntry>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DiscussionComment {
    pub author: Author,
    pub body: String,
    pub created_at: String,
    pub upvote_count: u64,
    pub reactions: Vec<ReactionCount>,
    pub is_answer: bool,
    pub replies: Vec<DiscussionComment>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ReactionCount {
    pub content: String,
    pub count: u64,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RepositoryDiscussion {
    pub common: ItemCommon,
    pub category: String,
    pub answerable: bool,
    pub answered: bool,
    pub answer_chosen_at: Option<String>,
    pub upvote_count: u64,
    pub reactions: Vec<ReactionCount>,
    pub comments: Vec<DiscussionComment>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PullRequest {
    pub common: ItemCommon,
    pub draft: bool,
    pub merged: bool,
    pub mergeable_state: Option<String>,
    pub base_ref: String,
    pub head_ref: String,
    pub additions: u64,
    pub deletions: u64,
    pub changed_files: u64,
    pub discussion: Vec<DiscussionEntry>,
    pub files: Vec<ChangedFile>,
    /// False while the changed-file list is known but the patches are not.
    ///
    /// The fast path lists files without their diffs, so `patch: None` alone
    /// cannot distinguish "not fetched yet" from "GitHub omitted it".
    pub patches_loaded: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum GithubItem {
    Issue(Issue),
    PullRequest(PullRequest),
    Discussion(RepositoryDiscussion),
    Ambiguous {
        issue_or_pull_request: Box<GithubItem>,
        discussion: Box<GithubItem>,
    },
}

impl GithubItem {
    pub fn common(&self) -> &ItemCommon {
        match self {
            Self::Issue(issue) => &issue.common,
            Self::PullRequest(pull) => &pull.common,
            Self::Discussion(discussion) => &discussion.common,
            Self::Ambiguous {
                issue_or_pull_request,
                ..
            } => issue_or_pull_request.common(),
        }
    }

    pub fn discussion(&self) -> &[DiscussionEntry] {
        match self {
            Self::Issue(issue) => &issue.comments,
            Self::PullRequest(pull) => &pull.discussion,
            Self::Discussion(_) => &[],
            Self::Ambiguous { .. } => &[],
        }
    }

    pub fn files(&self) -> &[ChangedFile] {
        match self {
            Self::Issue(_) => &[],
            Self::PullRequest(pull) => &pull.files,
            Self::Discussion(_) => &[],
            Self::Ambiguous { .. } => &[],
        }
    }

    pub fn is_pull_request(&self) -> bool {
        matches!(self, Self::PullRequest(_))
    }

    pub fn is_discussion(&self) -> bool {
        matches!(self, Self::Discussion(_))
    }

    pub fn cache_kind(&self) -> GithubLookupKind {
        match self {
            Self::Discussion(_) => GithubLookupKind::Discussion,
            Self::Issue(_) | Self::PullRequest(_) => GithubLookupKind::IssueOrPullRequest,
            Self::Ambiguous { .. } => GithubLookupKind::Auto,
        }
    }

    pub fn reference_status(&self) -> ReferenceStatus {
        match self {
            Self::Issue(issue) => ReferenceStatus {
                kind: ReferenceKind::Issue,
                state: if issue.common.state.eq_ignore_ascii_case("closed") {
                    ReferenceState::Closed
                } else {
                    ReferenceState::Open
                },
            },
            Self::PullRequest(pull) => ReferenceStatus {
                kind: ReferenceKind::PullRequest,
                state: if pull.merged || pull.common.state.eq_ignore_ascii_case("merged") {
                    ReferenceState::Merged
                } else if pull.common.state.eq_ignore_ascii_case("closed") {
                    ReferenceState::Closed
                } else if pull.draft {
                    ReferenceState::Draft
                } else {
                    ReferenceState::Open
                },
            },
            Self::Discussion(discussion) => ReferenceStatus {
                kind: ReferenceKind::Discussion,
                state: if discussion.common.state.eq_ignore_ascii_case("closed") {
                    ReferenceState::Closed
                } else {
                    ReferenceState::Open
                },
            },
            Self::Ambiguous { .. } => ReferenceStatus {
                kind: ReferenceKind::Ambiguous,
                state: ReferenceState::Open,
            },
        }
    }
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum GithubLookupKind {
    #[default]
    Auto,
    IssueOrPullRequest,
    Discussion,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct GithubItemSpec {
    pub number: u64,
    pub kind: GithubLookupKind,
}

impl From<u64> for GithubItemSpec {
    fn from(number: u64) -> Self {
        Self {
            number,
            kind: GithubLookupKind::Auto,
        }
    }
}

pub fn parse_item_spec(input: &str) -> Result<GithubItemSpec, String> {
    let input = input.trim();
    if input.is_empty() {
        return Err("Enter an issue, pull request, or discussion number".to_string());
    }
    let lower = input.to_ascii_lowercase();
    if let Some(number) = number_from_url(&lower, "/discussions/") {
        return positive_spec(number, GithubLookupKind::Discussion);
    }
    if let Some(number) =
        number_from_url(&lower, "/issues/").or_else(|| number_from_url(&lower, "/pull/"))
    {
        return positive_spec(number, GithubLookupKind::IssueOrPullRequest);
    }

    let parts: Vec<&str> = lower.split_whitespace().collect();
    let (kind, number) = match parts.as_slice() {
        [number] => (GithubLookupKind::Auto, *number),
        ["d" | "discussion" | "discussions", number] => (GithubLookupKind::Discussion, *number),
        ["i" | "issue" | "issues" | "pr" | "pull", number] => {
            (GithubLookupKind::IssueOrPullRequest, *number)
        }
        _ => {
            return Err("Use a number, `d NUMBER`, or a GitHub issue/PR/discussion URL".to_string())
        }
    };
    let number = number
        .trim_start_matches('#')
        .parse::<u64>()
        .map_err(|_| "Enter a positive GitHub item number".to_string())?;
    positive_spec(number, kind)
}

fn number_from_url(input: &str, marker: &str) -> Option<u64> {
    let suffix = input.split_once(marker)?.1;
    suffix
        .split(|character: char| !character.is_ascii_digit())
        .next()?
        .parse()
        .ok()
}

fn positive_spec(number: u64, kind: GithubLookupKind) -> Result<GithubItemSpec, String> {
    if number == 0 {
        Err("Item numbers start at 1".to_string())
    } else {
        Ok(GithubItemSpec { number, kind })
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum GithubErrorKind {
    MissingCli,
    Repository,
    Authentication,
    NotFound,
    Incomplete,
    InvalidResponse,
    Cancelled,
    Cli,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GithubError {
    pub kind: GithubErrorKind,
    pub message: String,
}

impl GithubError {
    fn new(kind: GithubErrorKind, message: impl Into<String>) -> Self {
        Self {
            kind,
            message: message.into(),
        }
    }

    fn invalid(context: &str, error: impl fmt::Display) -> Self {
        Self::new(
            GithubErrorKind::InvalidResponse,
            format!("GitHub returned invalid {context} data: {error}"),
        )
    }

    /// True when retrying the request a different way cannot help.
    ///
    /// Missing credentials or a missing item fail identically on every
    /// endpoint, so falling back would only double the time spent failing.
    fn is_fatal(&self) -> bool {
        matches!(
            self.kind,
            GithubErrorKind::MissingCli
                | GithubErrorKind::Authentication
                | GithubErrorKind::NotFound
                | GithubErrorKind::Incomplete
                | GithubErrorKind::Repository
        )
    }
}

impl fmt::Display for GithubError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.message)
    }
}

impl std::error::Error for GithubError {}

trait GhRunner {
    fn run(&self, cwd: &Path, args: &[String]) -> Result<Vec<u8>, GithubError>;

    /// Run, keeping stdout even when `gh` reports failure.
    ///
    /// GraphQL answers a partially invalid request with usable data *and* an
    /// error, which `gh` surfaces as a non-zero exit.
    fn run_partial(&self, cwd: &Path, args: &[String]) -> Result<Vec<u8>, GithubError> {
        self.run(cwd, args)
    }
}

struct ProcessGhRunner {
    cancelled: Arc<AtomicBool>,
}

impl ProcessGhRunner {
    fn execute(
        &self,
        cwd: &Path,
        args: &[String],
    ) -> Result<(ExitStatus, Vec<u8>, Vec<u8>), GithubError> {
        if self.cancelled.load(Ordering::Acquire) {
            return Err(cancelled_error());
        }

        let mut child = Command::new("gh")
            .args(args)
            .current_dir(cwd)
            .env("GH_PROMPT_DISABLED", "1")
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .map_err(|error| {
                if error.kind() == std::io::ErrorKind::NotFound {
                    GithubError::new(
                        GithubErrorKind::MissingCli,
                        "GitHub inspection requires the `gh` CLI in PATH",
                    )
                } else {
                    GithubError::new(GithubErrorKind::Cli, format!("Failed to run `gh`: {error}"))
                }
            })?;

        let stdout = child.stdout.take().ok_or_else(|| {
            GithubError::new(GithubErrorKind::Cli, "Failed to capture `gh` output")
        })?;
        let stderr = child.stderr.take().ok_or_else(|| {
            GithubError::new(GithubErrorKind::Cli, "Failed to capture `gh` errors")
        })?;
        let stdout_reader = read_pipe(stdout);
        let stderr_reader = read_pipe(stderr);

        let status = loop {
            if self.cancelled.load(Ordering::Acquire) {
                let _ = child.kill();
                let _ = child.wait();
                let _ = stdout_reader.join();
                let _ = stderr_reader.join();
                return Err(cancelled_error());
            }
            match child.try_wait() {
                Ok(Some(status)) => break status,
                Ok(None) => std::thread::sleep(Duration::from_millis(40)),
                Err(error) => {
                    let _ = child.kill();
                    let _ = child.wait();
                    let _ = stdout_reader.join();
                    let _ = stderr_reader.join();
                    return Err(GithubError::new(
                        GithubErrorKind::Cli,
                        format!("Failed while waiting for `gh`: {error}"),
                    ));
                }
            }
        };
        let stdout = stdout_reader.join().map_err(|_| {
            GithubError::new(
                GithubErrorKind::Cli,
                "Failed to collect `gh` output: reader stopped",
            )
        })?;
        let stdout = stdout.map_err(|error| {
            GithubError::new(
                GithubErrorKind::Cli,
                format!("Failed to read `gh` output: {error}"),
            )
        })?;
        let stderr = stderr_reader.join().map_err(|_| {
            GithubError::new(
                GithubErrorKind::Cli,
                "Failed to collect `gh` errors: reader stopped",
            )
        })?;
        let stderr = stderr.map_err(|error| {
            GithubError::new(
                GithubErrorKind::Cli,
                format!("Failed to read `gh` errors: {error}"),
            )
        })?;

        Ok((status, stdout, stderr))
    }
}

impl GhRunner for ProcessGhRunner {
    fn run(&self, cwd: &Path, args: &[String]) -> Result<Vec<u8>, GithubError> {
        let (status, stdout, stderr) = self.execute(cwd, args)?;
        if status.success() {
            return Ok(stdout);
        }
        cli_error(status, &stderr)
    }

    fn run_partial(&self, cwd: &Path, args: &[String]) -> Result<Vec<u8>, GithubError> {
        let (status, stdout, stderr) = self.execute(cwd, args)?;
        if status.success() || !stdout.is_empty() {
            return Ok(stdout);
        }
        cli_error(status, &stderr)
    }
}

fn read_pipe(
    mut pipe: impl Read + Send + 'static,
) -> std::thread::JoinHandle<std::io::Result<Vec<u8>>> {
    std::thread::spawn(move || {
        let mut bytes = Vec::new();
        pipe.read_to_end(&mut bytes)?;
        Ok(bytes)
    })
}

fn cli_error(status: ExitStatus, stderr: &[u8]) -> Result<Vec<u8>, GithubError> {
    let stderr = String::from_utf8_lossy(stderr).trim().to_string();
    let lower = stderr.to_ascii_lowercase();
    let (kind, guidance) = if lower.contains("http 401")
        || lower.contains("http 403")
        || lower.contains("authentication")
        || lower.contains("not logged")
        || lower.contains("gh auth login")
    {
        (
            GithubErrorKind::Authentication,
            "GitHub authentication failed; run `gh auth login` for this repository host",
        )
    } else if lower.contains("http 404")
        || lower.contains("not found")
        || lower.contains("could not resolve to a")
    {
        (
            GithubErrorKind::NotFound,
            "The GitHub repository or item was not found",
        )
    } else {
        (GithubErrorKind::Cli, "`gh` could not load the GitHub item")
    };
    let message = if stderr.is_empty() {
        format!("{guidance} ({status})")
    } else {
        format!("{guidance}: {stderr}")
    };
    Err(GithubError::new(kind, message))
}

fn cancelled_error() -> GithubError {
    GithubError::new(GithubErrorKind::Cancelled, "GitHub request cancelled")
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct RepoView {
    name_with_owner: String,
    url: String,
}

#[derive(Debug, Deserialize)]
struct ApiUser {
    login: String,
}

#[derive(Debug, Deserialize)]
struct ApiLabel {
    name: String,
    color: Option<String>,
}

#[derive(Debug, Deserialize)]
struct ApiIssue {
    number: u64,
    title: String,
    state: String,
    user: Option<ApiUser>,
    #[serde(default)]
    labels: Vec<ApiLabel>,
    created_at: String,
    updated_at: String,
    html_url: String,
    body: Option<String>,
    pull_request: Option<serde_json::Value>,
}

#[derive(Debug, Deserialize)]
struct ApiRef {
    #[serde(rename = "ref")]
    name: String,
}

#[derive(Debug, Deserialize)]
struct ApiPullRequest {
    number: u64,
    title: String,
    state: String,
    user: Option<ApiUser>,
    #[serde(default)]
    labels: Vec<ApiLabel>,
    created_at: String,
    updated_at: String,
    html_url: String,
    body: Option<String>,
    #[serde(default)]
    draft: bool,
    merged_at: Option<String>,
    mergeable_state: Option<String>,
    base: ApiRef,
    head: ApiRef,
    additions: u64,
    deletions: u64,
    changed_files: u64,
}

#[derive(Debug, Deserialize)]
struct ApiComment {
    user: Option<ApiUser>,
    body: Option<String>,
    created_at: String,
}

#[derive(Debug, Deserialize)]
struct ApiReview {
    user: Option<ApiUser>,
    body: Option<String>,
    state: Option<String>,
    submitted_at: Option<String>,
}

#[derive(Debug, Deserialize)]
struct ApiInlineComment {
    user: Option<ApiUser>,
    body: Option<String>,
    created_at: String,
    path: Option<String>,
    line: Option<u64>,
    original_line: Option<u64>,
}

#[derive(Debug, Deserialize)]
struct ApiFile {
    filename: String,
    status: String,
    additions: u64,
    deletions: u64,
    changes: u64,
    patch: Option<String>,
}

/// An item plus the repository it came from, so callers can cache the lookup.
pub struct FetchedItem {
    pub repository: RepositoryRef,
    pub item: GithubItem,
    pub discussion_checked: bool,
}

pub fn fetch_item_with_kind(
    cwd: PathBuf,
    spec: GithubItemSpec,
    known_repository: Option<RepositoryRef>,
    cancelled: Arc<AtomicBool>,
) -> Result<FetchedItem, GithubError> {
    let runner = ProcessGhRunner { cancelled };
    // Resolving the repository is a network round trip of its own, and it can
    // never change for a given directory while CST is running.
    let repository = match known_repository {
        Some(repository) => repository,
        None => resolve_repository(&runner, &cwd)?,
    };
    let outcome = fetch_item_outcome(&runner, &cwd, &repository, spec)?;
    Ok(FetchedItem {
        repository,
        item: outcome.item,
        discussion_checked: outcome.discussion_checked,
    })
}

/// Resolve the repository for a working directory.
///
/// Exposed so callers can warm this up before it is needed: it is a network
/// round trip, and repository identity never changes while CST is running.
pub fn resolve_repository_for(
    cwd: &Path,
    cancelled: Arc<AtomicBool>,
) -> Result<RepositoryRef, GithubError> {
    resolve_repository(&ProcessGhRunner { cancelled }, cwd)
}

/// Which GitHub hosts `gh` is authenticated to, or why that could not be answered.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum GhAuth {
    NotInstalled,
    /// `gh` runs but no host is logged in.
    NoHosts,
    LoggedIn(Vec<GhAuthHost>),
    /// `gh` answered with something neither parser understood, or not at all.
    Unknown(String),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GhAuthHost {
    pub host: String,
    pub account: Option<String>,
    pub healthy: bool,
}

#[derive(Deserialize)]
struct GhAuthStatusJson {
    #[serde(default)]
    hosts: std::collections::BTreeMap<String, Vec<GhAuthAccountJson>>,
}

/// Only `state` and `login` are relied on. The shape of a *failed* account was not
/// observable while writing this, so every field is optional and anything that is not
/// literally `"success"` counts as unhealthy.
#[derive(Deserialize)]
struct GhAuthAccountJson {
    #[serde(default)]
    state: String,
    #[serde(default)]
    login: Option<String>,
}

/// Ask `gh` which hosts it is logged in to.
///
/// This lives beside the fetch paths rather than in `doctor` so it can reuse the
/// private runner — the only place that returns exit status, stdout and stderr
/// together — along with its cancellation loop, which is what gives us a timeout.
///
/// A timeout is not optional: `gh auth status` validates tokens over the network and
/// hangs behind a VPN or captive portal, and `cst doctor` runs at the end of the
/// installer, where a hang reads as a broken install.
pub fn probe_auth(cwd: &Path, timeout: Duration) -> GhAuth {
    let cancelled = Arc::new(AtomicBool::new(false));
    let watchdog = Arc::clone(&cancelled);
    std::thread::spawn(move || {
        std::thread::sleep(timeout);
        watchdog.store(true, Ordering::Release);
    });
    let runner = ProcessGhRunner { cancelled };

    // `--json` always exits zero, so the payload is the only source of truth. Absence
    // of the flag on older `gh` is detected by the output failing to parse rather than
    // by sniffing stderr for "unknown flag", which is the brittleness `cli_error`
    // already suffers from and should not be extended.
    let json = runner.execute(cwd, &to_args(&["auth", "status", "--json", "hosts"]));
    match &json {
        Err(error) if error.kind == GithubErrorKind::MissingCli => return GhAuth::NotInstalled,
        Err(error) if error.kind == GithubErrorKind::Cancelled => {
            return GhAuth::Unknown(format!(
                "`gh auth status` did not answer within {} seconds",
                timeout.as_secs()
            ));
        }
        _ => {}
    }
    if let Ok((_, stdout, _)) = &json {
        if let Some(hosts) = parse_auth_status_json(stdout) {
            return classify_hosts(hosts);
        }
    }

    let text = runner.execute(cwd, &to_args(&["auth", "status"]));
    match text {
        Err(error) if error.kind == GithubErrorKind::MissingCli => GhAuth::NotInstalled,
        Err(error) => GhAuth::Unknown(error.message),
        Ok((_, stdout, stderr)) => {
            // Older `gh` printed this to stderr and newer prints to stdout; reading
            // both makes the parser version-independent.
            let combined = format!(
                "{}\n{}",
                String::from_utf8_lossy(&stdout),
                String::from_utf8_lossy(&stderr)
            );
            match parse_auth_status_text(&combined) {
                Some(hosts) => classify_hosts(hosts),
                None => GhAuth::Unknown(first_line(&combined)),
            }
        }
    }
}

fn classify_hosts(hosts: Vec<GhAuthHost>) -> GhAuth {
    if hosts.is_empty() {
        GhAuth::NoHosts
    } else {
        GhAuth::LoggedIn(hosts)
    }
}

fn to_args(args: &[&str]) -> Vec<String> {
    args.iter().map(|arg| (*arg).to_string()).collect()
}

fn first_line(text: &str) -> String {
    text.lines()
        .map(str::trim)
        .find(|line| !line.is_empty())
        .unwrap_or("`gh` produced no output")
        .chars()
        .filter(|character| !character.is_control())
        .take(120)
        .collect()
}

fn parse_auth_status_json(stdout: &[u8]) -> Option<Vec<GhAuthHost>> {
    let parsed: GhAuthStatusJson = serde_json::from_slice(stdout).ok()?;
    Some(
        parsed
            .hosts
            .into_iter()
            .flat_map(|(host, accounts)| {
                accounts.into_iter().map(move |account| GhAuthHost {
                    host: host.clone(),
                    account: account.login,
                    healthy: account.state == "success",
                })
            })
            .collect(),
    )
}

/// Fallback for `gh` too old to know `--json`.
///
/// Keyed on the English literals rather than on the check and cross glyphs, which
/// `gh` substitutes in its accessibility mode and Windows consoles mangle.
fn parse_auth_status_text(text: &str) -> Option<Vec<GhAuthHost>> {
    let mut hosts = Vec::new();
    let mut understood = false;
    for line in text.lines() {
        let line = line.trim();
        let (rest, healthy) = if let Some(rest) = line.split_once("Logged in to ") {
            (rest.1, true)
        } else if let Some(rest) = line.split_once("Failed to log in to ") {
            (rest.1, false)
        } else {
            continue;
        };
        understood = true;
        let mut words = rest.split_whitespace();
        let Some(host) = words.next() else { continue };
        let host = host.trim_end_matches([',', '.']).to_string();
        // `account <login>` on gh 2.40+, `as <login>` before it.
        let account = rest
            .split_once(" account ")
            .or_else(|| rest.split_once(" as "))
            .and_then(|(_, tail)| tail.split_whitespace().next())
            .map(|login| login.trim_end_matches([',', '.']).to_string());
        hosts.push(GhAuthHost {
            host,
            account,
            healthy,
        });
    }
    understood.then_some(hosts)
}

/// Fetch a pull request's diffs, which the fast path deliberately skips.
///
/// Returns patches keyed by path, for merging into an already-listed set of
/// changed files.
pub fn fetch_patches(
    cwd: PathBuf,
    repository: RepositoryRef,
    number: u64,
    cancelled: Arc<AtomicBool>,
) -> Result<Vec<(String, Option<String>)>, GithubError> {
    let runner = ProcessGhRunner { cancelled };
    let endpoint = repository.endpoint(&format!("pulls/{number}/files"));
    let files: Vec<ApiFile> = api_pages(&runner, &cwd, &repository, &endpoint, "changed files")?;
    Ok(files
        .into_iter()
        .map(|file| (file.filename, file.patch))
        .collect())
}

const COPILOT_REVIEWER: &str = "copilot-pull-request-reviewer[bot]";

/// Ask GitHub Copilot to review a pull request.
///
/// This is intentionally a single attempt. A cancelled or ambiguous mutating request
/// must not be retried automatically, because GitHub may already have accepted it.
pub fn request_copilot_review(
    cwd: PathBuf,
    repository: RepositoryRef,
    number: u64,
    cancelled: Arc<AtomicBool>,
) -> Result<(), GithubError> {
    let runner = ProcessGhRunner { cancelled };
    request_copilot_review_with(&runner, &cwd, &repository, number)
}

fn request_copilot_review_with(
    runner: &impl GhRunner,
    cwd: &Path,
    repository: &RepositoryRef,
    number: u64,
) -> Result<(), GithubError> {
    let mut args = strings(&["api", "--hostname"]);
    args.push(repository.host.clone());
    args.extend(strings(&["--method", "POST"]));
    args.push(repository.endpoint(&format!("pulls/{number}/requested_reviewers")));
    args.extend(strings(&["-f", &format!("reviewers[]={COPILOT_REVIEWER}")]));

    runner.run(cwd, &args).map(|_| ()).map_err(|error| {
        if error.kind == GithubErrorKind::Cancelled {
            error
        } else if error.kind == GithubErrorKind::MissingCli {
            GithubError::new(
                error.kind,
                "Requesting a Copilot review requires the `gh` CLI in PATH",
            )
        } else {
            GithubError::new(
                error.kind,
                format!("Could not request Copilot review: {}", error.message),
            )
        }
    })
}

/// Which thread to comment on.
///
/// Primitives rather than a `threads::ThreadRef` so this module keeps knowing nothing
/// about subscriptions; it only knows how to talk to `gh`.
#[derive(Debug, Clone, Copy)]
pub struct CommentTarget<'a> {
    pub host: &'a str,
    pub owner: &'a str,
    pub repo: &'a str,
    pub number: u64,
    /// Discussions have no REST endpoint and need a GraphQL mutation instead.
    pub discussion: bool,
}

/// A comment that now exists on GitHub.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PostedComment {
    pub id: String,
    pub url: String,
}

#[derive(Deserialize)]
struct ApiCreatedComment {
    id: u64,
    html_url: String,
}

/// Unwrap a GraphQL envelope, turning a pure-error response into a `GithubError`.
///
/// `data` wins when both are present: GraphQL reports a partially fulfilled request as
/// usable data alongside an error, and discarding it would lose the answer.
fn graph_payload<T: DeserializeOwned>(stdout: &[u8]) -> Result<T, GithubError> {
    let response: GraphResponse<T> = serde_json::from_slice(stdout).map_err(|error| {
        GithubError::new(
            GithubErrorKind::InvalidResponse,
            format!("GitHub returned an unreadable response: {error}"),
        )
    })?;
    if let Some(data) = response.data {
        return Ok(data);
    }
    let message = response
        .errors
        .into_iter()
        .map(|error| error.message)
        .collect::<Vec<_>>()
        .join("; ");
    Err(GithubError::new(
        GithubErrorKind::Cli,
        if message.is_empty() {
            "GitHub returned no data".to_string()
        } else {
            message
        },
    ))
}

/// Add a comment to an issue, pull request, or discussion.
///
/// The first mutating GitHub call in CST. Everything else here reads, so the failure
/// modes are different: a retry after an ambiguous failure can double-post, which is
/// why there is no retry and the error is handed straight back to the caller.
pub fn post_comment(
    cwd: PathBuf,
    target: CommentTarget<'_>,
    body: &str,
    cancelled: Arc<AtomicBool>,
) -> Result<PostedComment, GithubError> {
    let runner = ProcessGhRunner { cancelled };
    if target.discussion {
        post_discussion_comment(&runner, &cwd, target, body)
    } else {
        post_issue_comment(&runner, &cwd, target, body)
    }
}

/// Issues and pull requests share one REST endpoint.
///
/// `gh api` rather than `gh issue comment`/`gh pr comment` for two reasons: the REST
/// endpoint treats both alike so there is no branch to get wrong, and it answers with
/// the created comment as JSON, which is where the id comes from. The `gh` porcelain
/// prints only a URL.
fn post_issue_comment(
    runner: &ProcessGhRunner,
    cwd: &Path,
    target: CommentTarget<'_>,
    body: &str,
) -> Result<PostedComment, GithubError> {
    let mut args = strings(&["api", "--hostname", target.host, "--method", "POST"]);
    args.push(format!(
        "repos/{}/{}/issues/{}/comments",
        target.owner, target.repo, target.number
    ));
    // `-f` and not `-F`: `-F` type-converts, so a body that happens to start with `@`
    // would be read as a filename and a body of `123` would be sent as a number.
    args.push("-f".to_string());
    args.push(format!("body={body}"));

    let stdout = runner.run(cwd, &args)?;
    let created: ApiCreatedComment = serde_json::from_slice(&stdout).map_err(|error| {
        GithubError::new(
            GithubErrorKind::InvalidResponse,
            format!("GitHub did not describe the comment it created: {error}"),
        )
    })?;
    Ok(PostedComment {
        id: created.id.to_string(),
        url: created.html_url,
    })
}

const DISCUSSION_ID_QUERY: &str = "query($owner:String!,$repo:String!,$number:Int!){repository(owner:$owner,name:$repo){discussion(number:$number){id}}}";

const ADD_DISCUSSION_COMMENT: &str = "mutation($discussionId:ID!,$body:String!){addDiscussionComment(input:{discussionId:$discussionId,body:$body}){comment{id url}}}";

#[derive(Deserialize)]
struct GraphDiscussionIdData {
    repository: Option<GraphDiscussionIdRepository>,
}

#[derive(Deserialize)]
struct GraphDiscussionIdRepository {
    discussion: Option<GraphNodeId>,
}

#[derive(Deserialize)]
struct GraphNodeId {
    id: String,
}

#[derive(Deserialize)]
struct GraphAddCommentData {
    #[serde(rename = "addDiscussionComment")]
    add_discussion_comment: Option<GraphAddCommentPayload>,
}

#[derive(Deserialize)]
struct GraphAddCommentPayload {
    comment: Option<GraphPostedComment>,
}

#[derive(Deserialize)]
struct GraphPostedComment {
    id: String,
    url: String,
}

/// Two round trips, because the mutation wants a node id and a URL only has a number.
fn post_discussion_comment(
    runner: &ProcessGhRunner,
    cwd: &Path,
    target: CommentTarget<'_>,
    body: &str,
) -> Result<PostedComment, GithubError> {
    let lookup = graphql_args(
        target.host,
        DISCUSSION_ID_QUERY,
        &[
            ("owner", target.owner.to_string()),
            ("repo", target.repo.to_string()),
            ("number", target.number.to_string()),
        ],
    );
    let stdout = runner.run(cwd, &lookup)?;
    let discussion_id = graph_payload::<GraphDiscussionIdData>(&stdout)?
        .repository
        .and_then(|repository| repository.discussion)
        .ok_or_else(|| {
            GithubError::new(
                GithubErrorKind::NotFound,
                format!(
                    "Discussion #{} was not found in {}/{}",
                    target.number, target.owner, target.repo
                ),
            )
        })?
        .id;

    // Built by hand rather than through `graphql_args`, which sends every variable
    // with `-F`. A comment body must go as `-f` or `gh` would try to type-convert it.
    let mut args = strings(&["api", "graphql", "--hostname", target.host]);
    args.push("-f".to_string());
    args.push(format!("discussionId={discussion_id}"));
    args.push("-f".to_string());
    args.push(format!("body={body}"));
    args.push("-f".to_string());
    args.push(format!("query={ADD_DISCUSSION_COMMENT}"));

    let stdout = runner.run(cwd, &args)?;
    let comment = graph_payload::<GraphAddCommentData>(&stdout)?
        .add_discussion_comment
        .and_then(|payload| payload.comment)
        .ok_or_else(|| {
            GithubError::new(
                GithubErrorKind::InvalidResponse,
                "GitHub did not describe the discussion comment it created",
            )
        })?;
    Ok(PostedComment {
        id: comment.id,
        url: comment.url,
    })
}

/// One comment, reduced to what deciding whether to wake a session needs.
///
/// No body. The comment text is written by whoever can reach the thread, and CST never
/// puts it in front of an agent as prompt text — carrying it here would be the first
/// step towards doing so by accident.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ThreadComment {
    pub id: String,
    pub author: String,
    pub url: String,
    pub created_at: String,
}

#[derive(Deserialize)]
struct ApiThreadComment {
    id: u64,
    html_url: String,
    created_at: String,
    user: Option<ApiCommentUser>,
}

#[derive(Deserialize)]
struct ApiCommentUser {
    login: String,
}

/// Recent comments on a thread, newest activity first being irrelevant — order is not
/// relied on anywhere, because a comment is new when its id has not been seen before.
///
/// `since` keeps this cheap on a long-running conversation: without it, a thread with
/// hundreds of comments would be refetched in full every time it moved.
pub fn fetch_thread_comments(
    cwd: PathBuf,
    target: CommentTarget<'_>,
    since: Option<&str>,
    cancelled: Arc<AtomicBool>,
) -> Result<Vec<ThreadComment>, GithubError> {
    let runner = ProcessGhRunner { cancelled };
    if target.discussion {
        fetch_discussion_comments(&runner, &cwd, target)
    } else {
        fetch_issue_comments(&runner, &cwd, target, since)
    }
}

/// The comment list for an issue or pull request, from `since` on.
///
/// `since` is an RFC 3339 time, and a `+00:00` offset in it has to be escaped. Left bare,
/// the `+` reaches GitHub as a space, the offset is lost, and the time is read as US
/// Pacific: seven or eight hours later than meant. Every comment from the first hours of
/// a subscription then fell outside the window, and since the thread's cursor had
/// already moved on, nothing ever asked for them again.
fn issue_comments_endpoint(target: &CommentTarget<'_>, since: Option<&str>) -> String {
    let mut endpoint = format!(
        "repos/{}/{}/issues/{}/comments?per_page=100",
        target.owner, target.repo, target.number
    );
    if let Some(since) = since {
        endpoint.push_str(&format!("&since={}", since.replace('+', "%2B")));
    }
    endpoint
}

fn fetch_issue_comments(
    runner: &ProcessGhRunner,
    cwd: &Path,
    target: CommentTarget<'_>,
    since: Option<&str>,
) -> Result<Vec<ThreadComment>, GithubError> {
    let mut args = strings(&["api", "--hostname", target.host, "--paginate", "--slurp"]);
    args.push(issue_comments_endpoint(&target, since));

    let stdout = runner.run(cwd, &args)?;
    // `--slurp` wraps each page in an outer array; flattening is what `api_pages` does.
    let pages: Vec<Vec<ApiThreadComment>> = serde_json::from_slice(&stdout).map_err(|error| {
        GithubError::new(
            GithubErrorKind::InvalidResponse,
            format!("GitHub sent an unreadable comment list: {error}"),
        )
    })?;

    Ok(pages
        .into_iter()
        .flatten()
        .map(|comment| ThreadComment {
            id: comment.id.to_string(),
            author: comment.user.map(|user| user.login).unwrap_or_default(),
            url: comment.html_url,
            created_at: comment.created_at,
        })
        .collect())
}

const DISCUSSION_UPDATED_QUERY: &str = "query($owner:String!,$repo:String!,$number:Int!){repository(owner:$owner,name:$repo){discussion(number:$number){updatedAt}}}";

#[derive(Deserialize)]
struct GraphUpdatedData {
    repository: Option<GraphUpdatedRepository>,
}

#[derive(Deserialize)]
struct GraphUpdatedRepository {
    discussion: Option<GraphUpdatedDiscussion>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct GraphUpdatedDiscussion {
    updated_at: String,
}

/// When a discussion last changed, as an opaque cursor.
///
/// GraphQL has no conditional requests, so a discussion cannot be polled for free the
/// way an issue can. Asking only for `updatedAt` keeps the response tiny and the query
/// cost at one point, which is the next best thing.
pub fn fetch_discussion_updated_at(
    cwd: PathBuf,
    target: CommentTarget<'_>,
    cancelled: Arc<AtomicBool>,
) -> Result<String, GithubError> {
    let runner = ProcessGhRunner { cancelled };
    let args = graphql_args(
        target.host,
        DISCUSSION_UPDATED_QUERY,
        &[
            ("owner", target.owner.to_string()),
            ("repo", target.repo.to_string()),
            ("number", target.number.to_string()),
        ],
    );
    let stdout = runner.run(&cwd, &args)?;
    graph_payload::<GraphUpdatedData>(&stdout)?
        .repository
        .and_then(|repository| repository.discussion)
        .map(|discussion| discussion.updated_at)
        .ok_or_else(|| {
            GithubError::new(
                GithubErrorKind::NotFound,
                format!(
                    "Discussion #{} was not found in {}/{}",
                    target.number, target.owner, target.repo
                ),
            )
        })
}

/// Comment text, for the one caller allowed to look at it.
///
/// Separate from [`fetch_thread_comments`] and named for its purpose, because carrying
/// bodies around is exactly what the rest of the thread pipeline refuses to do. The only
/// consumer is the stall check, which feeds them to a model that has no tools and no
/// repository — see `threads::judge`.
pub fn fetch_recent_bodies_for_review(
    cwd: PathBuf,
    target: CommentTarget<'_>,
    limit: usize,
    cancelled: Arc<AtomicBool>,
) -> Result<Vec<String>, GithubError> {
    if target.discussion {
        // Discussions would need a second GraphQL query for bodies. The stall check is
        // advisory, so skipping them loses a hint rather than breaking anything.
        return Ok(Vec::new());
    }
    let runner = ProcessGhRunner { cancelled };
    let mut args = strings(&["api", "--hostname", target.host]);
    args.push(format!(
        "repos/{}/{}/issues/{}/comments?per_page={limit}",
        target.owner, target.repo, target.number
    ));

    let stdout = runner.run(&cwd, &args)?;
    let comments: Vec<ApiCommentBody> = serde_json::from_slice(&stdout).map_err(|error| {
        GithubError::new(
            GithubErrorKind::InvalidResponse,
            format!("GitHub sent an unreadable comment list: {error}"),
        )
    })?;
    Ok(comments
        .into_iter()
        .rev()
        .take(limit)
        .map(|comment| comment.body.unwrap_or_default())
        .collect())
}

#[derive(Deserialize)]
struct ApiCommentBody {
    body: Option<String>,
}

/// Discussions need the last page, not the first: their comments cannot be filtered by
/// time through GraphQL the way the REST endpoint allows.
const DISCUSSION_RECENT_QUERY: &str = "query($owner:String!,$repo:String!,$number:Int!){repository(owner:$owner,name:$repo){discussion(number:$number){comments(last:30){nodes{id url createdAt author{login} replies(last:10){nodes{id url createdAt author{login}}}}}}}}";

#[derive(Deserialize)]
struct GraphRecentData {
    repository: Option<GraphRecentRepository>,
}

#[derive(Deserialize)]
struct GraphRecentRepository {
    discussion: Option<GraphRecentDiscussion>,
}

#[derive(Deserialize)]
struct GraphRecentDiscussion {
    comments: GraphRecentComments,
}

#[derive(Deserialize)]
struct GraphRecentComments {
    #[serde(default)]
    nodes: Vec<GraphRecentComment>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct GraphRecentComment {
    id: String,
    url: String,
    created_at: String,
    author: Option<GraphRecentAuthor>,
    #[serde(default)]
    replies: Option<GraphRecentReplies>,
}

#[derive(Deserialize)]
struct GraphRecentReplies {
    #[serde(default)]
    nodes: Vec<GraphRecentComment>,
}

#[derive(Deserialize)]
struct GraphRecentAuthor {
    login: String,
}

impl GraphRecentComment {
    /// Flatten a comment and its replies, because a reply is just as much something
    /// somebody may be waiting on as a top-level comment.
    fn flatten(self, out: &mut Vec<ThreadComment>) {
        let replies = self
            .replies
            .map(|replies| replies.nodes)
            .unwrap_or_default();
        out.push(ThreadComment {
            id: self.id,
            author: self.author.map(|author| author.login).unwrap_or_default(),
            url: self.url,
            created_at: self.created_at,
        });
        for reply in replies {
            reply.flatten(out);
        }
    }
}

fn fetch_discussion_comments(
    runner: &ProcessGhRunner,
    cwd: &Path,
    target: CommentTarget<'_>,
) -> Result<Vec<ThreadComment>, GithubError> {
    let args = graphql_args(
        target.host,
        DISCUSSION_RECENT_QUERY,
        &[
            ("owner", target.owner.to_string()),
            ("repo", target.repo.to_string()),
            ("number", target.number.to_string()),
        ],
    );
    let stdout = runner.run(cwd, &args)?;
    let discussion = graph_payload::<GraphRecentData>(&stdout)?
        .repository
        .and_then(|repository| repository.discussion);

    let mut comments = Vec::new();
    if let Some(discussion) = discussion {
        for node in discussion.comments.nodes {
            node.flatten(&mut comments);
        }
    }
    Ok(comments)
}

#[cfg(test)]
fn fetch_item_from(
    runner: &dyn GhRunner,
    cwd: &Path,
    repository: &RepositoryRef,
    spec: impl Into<GithubItemSpec>,
) -> Result<GithubItem, GithubError> {
    fetch_item_outcome(runner, cwd, repository, spec.into()).map(|outcome| outcome.item)
}

struct FetchOutcome {
    item: GithubItem,
    discussion_checked: bool,
}

fn fetch_item_outcome(
    runner: &dyn GhRunner,
    cwd: &Path,
    repository: &RepositoryRef,
    spec: GithubItemSpec,
) -> Result<FetchOutcome, GithubError> {
    // One GraphQL round trip replaces up to six REST ones. It only covers items
    // whose comment, review and file lists fit in a single page, so anything
    // larger falls back to the paginated REST loader rather than truncating.
    match fetch_item_graphql(runner, cwd, repository, spec) {
        Ok(GraphFetch::Item(item)) => {
            return Ok(FetchOutcome {
                item,
                discussion_checked: spec.kind != GithubLookupKind::IssueOrPullRequest,
            })
        }
        Ok(GraphFetch::NeedsRest { discussion }) => {
            let issue_endpoint = repository.endpoint(&format!("issues/{}", spec.number));
            let issue: ApiIssue = match api_object(runner, cwd, repository, &issue_endpoint, "item")
            {
                Ok(issue) => issue,
                Err(error) if error.kind == GithubErrorKind::NotFound && discussion.is_some() => {
                    return Ok(FetchOutcome {
                        item: discussion.expect("checked above"),
                        discussion_checked: true,
                    });
                }
                Err(error) => return Err(error),
            };
            return match if issue.pull_request.is_some() {
                load_pull_request(runner, cwd, repository.clone(), spec.number)
            } else {
                load_issue(runner, cwd, repository.clone(), issue)
            } {
                Ok(issue_or_pull) => Ok(FetchOutcome {
                    item: match discussion {
                        Some(discussion) => GithubItem::Ambiguous {
                            issue_or_pull_request: Box::new(issue_or_pull),
                            discussion: Box::new(discussion),
                        },
                        None => issue_or_pull,
                    },
                    discussion_checked: true,
                }),
                Err(error) => Err(error),
            };
        }
        Err(error) if error.kind == GithubErrorKind::Cancelled => return Err(error),
        Err(error) if spec.kind == GithubLookupKind::Discussion => return Err(error),
        Err(error) if error.is_fatal() => return Err(error),
        // A GraphQL-specific failure (an unsupported schema on an old
        // Enterprise server, say) must not make the inspector unusable.
        Err(_) => {}
    }
    if spec.kind == GithubLookupKind::Discussion {
        Err(GithubError::new(
            GithubErrorKind::NotFound,
            format!(
                "No discussion #{} in {}",
                spec.number,
                repository.name_with_owner()
            ),
        ))
    } else {
        fetch_item_rest(runner, cwd, repository, spec.number).map(|item| FetchOutcome {
            item,
            discussion_checked: false,
        })
    }
}

enum GraphFetch {
    Item(GithubItem),
    NeedsRest { discussion: Option<GithubItem> },
}

fn fetch_item_rest(
    runner: &dyn GhRunner,
    cwd: &Path,
    repository: &RepositoryRef,
    number: u64,
) -> Result<GithubItem, GithubError> {
    let issue_endpoint = repository.endpoint(&format!("issues/{number}"));
    let issue: ApiIssue = api_object(runner, cwd, repository, &issue_endpoint, "item")?;

    if issue.pull_request.is_some() {
        load_pull_request(runner, cwd, repository.clone(), number)
    } else {
        load_issue(runner, cwd, repository.clone(), issue)
    }
}

fn resolve_repository(runner: &dyn GhRunner, cwd: &Path) -> Result<RepositoryRef, GithubError> {
    let args = strings(&["repo", "view", "--json", "nameWithOwner,url"]);
    let output = runner.run(cwd, &args).map_err(|mut error| {
        if error.kind == GithubErrorKind::Cli || error.kind == GithubErrorKind::NotFound {
            error.kind = GithubErrorKind::Repository;
            error.message = format!(
                "Cannot resolve a GitHub repository from {}: {}",
                cwd.display(),
                error.message
            );
        }
        error
    })?;
    let view: RepoView = serde_json::from_slice(&output)
        .map_err(|error| GithubError::invalid("repository", error))?;
    repository_from_view(view)
}

fn repository_from_view(view: RepoView) -> Result<RepositoryRef, GithubError> {
    let (owner, name) = view.name_with_owner.split_once('/').ok_or_else(|| {
        GithubError::new(
            GithubErrorKind::InvalidResponse,
            "GitHub returned a repository name without an owner",
        )
    })?;
    let without_scheme = view
        .url
        .split_once("://")
        .map(|(_, rest)| rest)
        .unwrap_or(view.url.as_str());
    let host = without_scheme
        .split('/')
        .next()
        .filter(|host| !host.is_empty())
        .ok_or_else(|| {
            GithubError::new(
                GithubErrorKind::InvalidResponse,
                "GitHub returned a repository URL without a hostname",
            )
        })?;
    Ok(RepositoryRef {
        host: host.to_string(),
        owner: owner.to_string(),
        name: name.to_string(),
    })
}

fn load_issue(
    runner: &dyn GhRunner,
    cwd: &Path,
    repository: RepositoryRef,
    issue: ApiIssue,
) -> Result<GithubItem, GithubError> {
    let comments_endpoint = repository.endpoint(&format!("issues/{}/comments", issue.number));
    let comments: Vec<ApiComment> = api_pages(
        runner,
        cwd,
        &repository,
        &comments_endpoint,
        "issue comments",
    )?;
    let comments = comments
        .into_iter()
        .map(|comment| DiscussionEntry {
            kind: DiscussionKind::Comment,
            author: author(comment.user),
            body: comment.body.unwrap_or_default(),
            created_at: comment.created_at,
            review_state: None,
            thread_resolved: None,
            path: None,
            line: None,
        })
        .collect();

    Ok(GithubItem::Issue(Issue {
        common: common_from_issue(repository, issue),
        comments,
    }))
}

fn load_pull_request(
    runner: &dyn GhRunner,
    cwd: &Path,
    repository: RepositoryRef,
    number: u64,
) -> Result<GithubItem, GithubError> {
    let pull_endpoint = repository.endpoint(&format!("pulls/{number}"));
    let pull: ApiPullRequest =
        api_object(runner, cwd, &repository, &pull_endpoint, "pull request")?;
    let issue_comments_endpoint = repository.endpoint(&format!("issues/{number}/comments"));
    let reviews_endpoint = repository.endpoint(&format!("pulls/{number}/reviews"));
    let inline_endpoint = repository.endpoint(&format!("pulls/{number}/comments"));
    let files_endpoint = repository.endpoint(&format!("pulls/{number}/files"));

    let comments: Vec<ApiComment> = api_pages(
        runner,
        cwd,
        &repository,
        &issue_comments_endpoint,
        "pull request comments",
    )?;
    let reviews: Vec<ApiReview> = api_pages(
        runner,
        cwd,
        &repository,
        &reviews_endpoint,
        "pull request reviews",
    )?;
    let inline_comments: Vec<ApiInlineComment> = api_pages(
        runner,
        cwd,
        &repository,
        &inline_endpoint,
        "inline review comments",
    )?;
    let files: Vec<ApiFile> =
        api_pages(runner, cwd, &repository, &files_endpoint, "changed files")?;

    let mut discussion = Vec::with_capacity(comments.len() + reviews.len() + inline_comments.len());
    discussion.extend(comments.into_iter().map(|comment| DiscussionEntry {
        kind: DiscussionKind::Comment,
        author: author(comment.user),
        body: comment.body.unwrap_or_default(),
        created_at: comment.created_at,
        review_state: None,
        thread_resolved: None,
        path: None,
        line: None,
    }));
    discussion.extend(reviews.into_iter().map(|review| DiscussionEntry {
        kind: DiscussionKind::Review,
        author: author(review.user),
        body: review.body.unwrap_or_default(),
        created_at: review.submitted_at.unwrap_or_default(),
        review_state: review.state,
        thread_resolved: None,
        path: None,
        line: None,
    }));
    discussion.extend(inline_comments.into_iter().map(|comment| DiscussionEntry {
        kind: DiscussionKind::InlineReview,
        author: author(comment.user),
        body: comment.body.unwrap_or_default(),
        created_at: comment.created_at,
        review_state: None,
        thread_resolved: None,
        path: comment.path,
        line: comment.line.or(comment.original_line),
    }));
    discussion.sort_by(|left, right| left.created_at.cmp(&right.created_at));

    let common = ItemCommon {
        repository: repository.clone(),
        number: pull.number,
        title: pull.title,
        state: pull.state,
        author: author(pull.user),
        labels: labels(pull.labels),
        created_at: pull.created_at,
        updated_at: pull.updated_at,
        url: pull.html_url,
        body: pull.body.unwrap_or_default(),
    };
    let files = files
        .into_iter()
        .map(|file| ChangedFile {
            path: file.filename,
            status: file.status,
            additions: file.additions,
            deletions: file.deletions,
            changes: file.changes,
            patch: file.patch,
        })
        .collect();

    Ok(GithubItem::PullRequest(PullRequest {
        common,
        draft: pull.draft,
        merged: pull.merged_at.is_some(),
        mergeable_state: pull.mergeable_state,
        base_ref: pull.base.name,
        head_ref: pull.head.name,
        additions: pull.additions,
        deletions: pull.deletions,
        changed_files: pull.changed_files,
        discussion,
        files,
        patches_loaded: true,
    }))
}

fn common_from_issue(repository: RepositoryRef, issue: ApiIssue) -> ItemCommon {
    ItemCommon {
        repository,
        number: issue.number,
        title: issue.title,
        state: issue.state,
        author: author(issue.user),
        labels: labels(issue.labels),
        created_at: issue.created_at,
        updated_at: issue.updated_at,
        url: issue.html_url,
        body: issue.body.unwrap_or_default(),
    }
}

fn author(user: Option<ApiUser>) -> Author {
    Author {
        login: user
            .map(|user| user.login)
            .unwrap_or_else(|| "ghost".to_string()),
    }
}

fn labels(input: Vec<ApiLabel>) -> Vec<Label> {
    input
        .into_iter()
        .map(|label| Label {
            name: label.name,
            color: label.color,
        })
        .collect()
}

fn api_object<T: DeserializeOwned>(
    runner: &dyn GhRunner,
    cwd: &Path,
    repository: &RepositoryRef,
    endpoint: &str,
    context: &str,
) -> Result<T, GithubError> {
    let output = runner.run(cwd, &api_args(repository, endpoint, false))?;
    serde_json::from_slice(&output).map_err(|error| GithubError::invalid(context, error))
}

fn api_pages<T: DeserializeOwned>(
    runner: &dyn GhRunner,
    cwd: &Path,
    repository: &RepositoryRef,
    endpoint: &str,
    context: &str,
) -> Result<Vec<T>, GithubError> {
    let output = runner.run(cwd, &api_args(repository, endpoint, true))?;
    let pages: Vec<Vec<T>> =
        serde_json::from_slice(&output).map_err(|error| GithubError::invalid(context, error))?;
    Ok(pages.into_iter().flatten().collect())
}

/// What a `#1234` in the conversation actually points at.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ReferenceKind {
    Issue,
    PullRequest,
    Discussion,
    Ambiguous,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ReferenceState {
    Open,
    Closed,
    Merged,
    Draft,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ReferenceStatus {
    pub kind: ReferenceKind,
    pub state: ReferenceState,
}

/// Most references resolved in one query.
///
/// Bounded so a screen full of numbers cannot build a request large enough for
/// GitHub to reject on node count.
pub const REFERENCE_BATCH: usize = 40;

/// Look up several issue/pull-request numbers in a single round trip.
///
/// A `None` status means the number is not a reference at all — plenty of
/// `#123` in a terminal are version numbers or line numbers — and callers are
/// expected to remember that so they stop asking.
pub fn resolve_references(
    cwd: PathBuf,
    repository: RepositoryRef,
    numbers: Vec<u64>,
    cancelled: Arc<AtomicBool>,
) -> Result<Vec<(u64, Option<ReferenceStatus>)>, GithubError> {
    resolve_references_with(&ProcessGhRunner { cancelled }, &cwd, &repository, &numbers)
}

fn resolve_references_with(
    runner: &dyn GhRunner,
    cwd: &Path,
    repository: &RepositoryRef,
    numbers: &[u64],
) -> Result<Vec<(u64, Option<ReferenceStatus>)>, GithubError> {
    if numbers.is_empty() {
        return Ok(Vec::new());
    }
    let numbers: Vec<u64> = numbers.iter().copied().take(REFERENCE_BATCH).collect();
    let args = graphql_args(
        &repository.host,
        &reference_query(&numbers),
        &[
            ("owner", repository.owner.clone()),
            ("name", repository.name.clone()),
        ],
    );
    // Unknown numbers make GitHub report an error alongside perfectly good data
    // for every other alias, and `gh` exits non-zero for it, so the body has to
    // be read regardless of the exit status.
    let output = runner.run_partial(cwd, &args)?;
    let response: GraphResponse<GraphReferenceData> = serde_json::from_slice(&output)
        .map_err(|error| GithubError::invalid("references", error))?;
    let discussions_unsupported = response
        .errors
        .iter()
        .any(|error| graphql_discussions_unsupported(&error.message));
    let mut alias_errors = response.errors;
    let (mut nodes, discussions_supported) = match response.data.and_then(|data| data.repository) {
        Some(nodes) => (nodes, true),
        None if discussions_unsupported => {
            let args = graphql_args(
                &repository.host,
                &reference_query_legacy(&numbers),
                &[
                    ("owner", repository.owner.clone()),
                    ("name", repository.name.clone()),
                ],
            );
            let output = runner.run_partial(cwd, &args)?;
            let fallback: GraphResponse<GraphReferenceData> = serde_json::from_slice(&output)
                .map_err(|error| GithubError::invalid("references", error))?;
            alias_errors = fallback.errors;
            let nodes = fallback
                .data
                .and_then(|data| data.repository)
                .ok_or_else(|| GithubError::invalid("references", "missing repository data"))?;
            (nodes, false)
        }
        None => {
            return Err(GithubError::invalid(
                "references",
                "missing repository data",
            ))
        }
    };
    for number in &numbers {
        let aliases = if discussions_supported {
            vec![
                reference_alias(*number),
                discussion_reference_alias(*number),
            ]
        } else {
            vec![reference_alias(*number)]
        };
        for alias in aliases {
            if let Some(error) = candidate_graph_error(&alias_errors, &alias) {
                return Err(error);
            }
        }
    }

    Ok(numbers
        .into_iter()
        .map(|number| {
            let issue_or_pull = nodes
                .remove(&reference_alias(number))
                .flatten()
                .map(|node| node.into_status());
            let discussion = nodes
                .remove(&discussion_reference_alias(number))
                .flatten()
                .map(|node| node.into_status());
            let status = match (issue_or_pull, discussion) {
                (Some(item), Some(_)) => Some(ReferenceStatus {
                    kind: ReferenceKind::Ambiguous,
                    state: item.state,
                }),
                (Some(item), None) | (None, Some(item)) => Some(item),
                (None, None) => None,
            };
            (number, status)
        })
        .collect())
}

fn reference_alias(number: u64) -> String {
    format!("r{number}")
}

fn reference_query(numbers: &[u64]) -> String {
    let mut query =
        String::from("query($owner:String!,$name:String!){repository(owner:$owner,name:$name){");
    for number in numbers {
        query.push_str(&format!(
            "{}:issueOrPullRequest(number:{number}){{__typename ... on Issue{{state}} ... on PullRequest{{state isDraft}}}} ",
            reference_alias(*number)
        ));
        query.push_str(&format!(
            "{}:discussion(number:{number}){{__typename closed}} ",
            discussion_reference_alias(*number)
        ));
    }
    query.push_str("}}");
    query
}

fn reference_query_legacy(numbers: &[u64]) -> String {
    let mut query =
        String::from("query($owner:String!,$name:String!){repository(owner:$owner,name:$name){");
    for number in numbers {
        query.push_str(&format!(
            "{}:issueOrPullRequest(number:{number}){{__typename ... on Issue{{state}} ... on PullRequest{{state isDraft}}}} ",
            reference_alias(*number)
        ));
    }
    query.push_str("}}");
    query
}

fn graphql_discussions_unsupported(message: &str) -> bool {
    let lower = message.to_ascii_lowercase();
    lower.contains("discussion")
        && (lower.contains("field")
            || lower.contains("undefined")
            || lower.contains("doesn't exist")
            || lower.contains("does not exist"))
}

#[derive(Debug, Deserialize)]
struct GraphReferenceData {
    repository: Option<std::collections::HashMap<String, Option<GraphReferenceNode>>>,
}

#[derive(Debug, Deserialize)]
#[serde(tag = "__typename")]
enum GraphReferenceNode {
    Issue {
        state: String,
    },
    PullRequest {
        state: String,
        #[serde(rename = "isDraft", default)]
        is_draft: bool,
    },
    Discussion {
        closed: bool,
    },
}

impl GraphReferenceNode {
    fn into_status(self) -> ReferenceStatus {
        match self {
            Self::Issue { state } => ReferenceStatus {
                kind: ReferenceKind::Issue,
                state: if state.eq_ignore_ascii_case("closed") {
                    ReferenceState::Closed
                } else {
                    ReferenceState::Open
                },
            },
            Self::PullRequest { state, is_draft } => ReferenceStatus {
                kind: ReferenceKind::PullRequest,
                state: if state.eq_ignore_ascii_case("merged") {
                    ReferenceState::Merged
                } else if state.eq_ignore_ascii_case("closed") {
                    ReferenceState::Closed
                } else if is_draft {
                    ReferenceState::Draft
                } else {
                    ReferenceState::Open
                },
            },
            Self::Discussion { closed } => ReferenceStatus {
                kind: ReferenceKind::Discussion,
                state: if closed {
                    ReferenceState::Closed
                } else {
                    ReferenceState::Open
                },
            },
        }
    }
}

fn api_args(repository: &RepositoryRef, endpoint: &str, paginate: bool) -> Vec<String> {
    let mut args = strings(&["api", "--hostname", &repository.host]);
    if paginate {
        args.extend(strings(&["--paginate", "--slurp"]));
    }
    args.push(endpoint.to_string());
    args
}

/// How many entries of each connection the single-call query asks for.
///
/// An item that exceeds this in any list falls back to the REST loader, so the
/// figure trades how often that happens against the size of one response.
const GRAPH_PAGE: usize = 100;

const ITEM_QUERY: &str = r#"
query($owner:String!,$name:String!,$number:Int!,$page:Int!){
  repository(owner:$owner,name:$name){
    issueOrPullRequest(number:$number){
      __typename
      ... on Issue {
        number title state body url createdAt updatedAt
        author{login}
        labels(first:$page){nodes{name color} pageInfo{hasNextPage}}
        comments(first:$page){nodes{body createdAt author{login}} pageInfo{hasNextPage}}
      }
      ... on PullRequest {
        number title state body url createdAt updatedAt
        isDraft merged mergeable additions deletions changedFiles
        baseRefName headRefName
        author{login}
        labels(first:$page){nodes{name color} pageInfo{hasNextPage}}
        comments(first:$page){nodes{body createdAt author{login}} pageInfo{hasNextPage}}
        reviews(first:$page){nodes{body state submittedAt author{login}} pageInfo{hasNextPage}}
        reviewThreads(first:$page){nodes{isResolved comments(first:20){nodes{body path line originalLine createdAt author{login}} pageInfo{hasNextPage}}} pageInfo{hasNextPage}}
        files(first:$page){nodes{path additions deletions changeType} pageInfo{hasNextPage}}
      }
    }
    discussion(number:$number){
      number title body url createdAt updatedAt closed isAnswered answerChosenAt upvoteCount
      reactionGroups{content reactors{totalCount}}
      author{login}
      category{name isAnswerable}
      comments(first:$page){
        nodes{
          id body createdAt upvoteCount isAnswer author{login}
          reactionGroups{content reactors{totalCount}}
          replies(first:$page){
            nodes{body createdAt upvoteCount author{login} reactionGroups{content reactors{totalCount}}}
            pageInfo{hasNextPage endCursor}
          }
        }
        pageInfo{hasNextPage endCursor}
      }
    }
  }
}
"#;

const DISCUSSION_COMMENTS_QUERY: &str = r#"
query($owner:String!,$name:String!,$number:Int!,$page:Int!,$after:String!){
  repository(owner:$owner,name:$name){
    discussion(number:$number){
      comments(first:$page,after:$after){
        nodes{
          id body createdAt upvoteCount isAnswer author{login}
          reactionGroups{content reactors{totalCount}}
          replies(first:$page){
            nodes{body createdAt upvoteCount author{login} reactionGroups{content reactors{totalCount}}}
            pageInfo{hasNextPage endCursor}
          }
        }
        pageInfo{hasNextPage endCursor}
      }
    }
  }
}
"#;

const DISCUSSION_REPLIES_QUERY: &str = r#"
query($id:ID!,$page:Int!,$after:String!){
  node(id:$id){
    ... on DiscussionComment {
      replies(first:$page,after:$after){
        nodes{body createdAt upvoteCount author{login} reactionGroups{content reactors{totalCount}}}
        pageInfo{hasNextPage endCursor}
      }
    }
  }
}
"#;

fn graphql_args(host: &str, query: &str, variables: &[(&str, String)]) -> Vec<String> {
    let mut args = strings(&["api", "graphql", "--hostname", host]);
    for (name, value) in variables {
        // -F types numbers and booleans; -f would send them as strings and the
        // Int! variables would be rejected.
        args.push("-F".to_string());
        args.push(format!("{name}={value}"));
    }
    args.push("-f".to_string());
    args.push(format!("query={query}"));
    args
}

/// Fetch issue/PR and discussion candidates in one round trip.
///
/// Oversized issues and pull requests ask the caller for the paginated REST
/// fallback; discussion connections are paginated here through GraphQL.
fn fetch_item_graphql(
    runner: &dyn GhRunner,
    cwd: &Path,
    repository: &RepositoryRef,
    spec: GithubItemSpec,
) -> Result<GraphFetch, GithubError> {
    let args = graphql_args(
        &repository.host,
        ITEM_QUERY,
        &[
            ("owner", repository.owner.clone()),
            ("name", repository.name.clone()),
            ("number", spec.number.to_string()),
            ("page", GRAPH_PAGE.to_string()),
        ],
    );
    let output = runner.run_partial(cwd, &args)?;
    let response: GraphResponse<GraphItemData> =
        serde_json::from_slice(&output).map_err(|error| GithubError::invalid("item", error))?;
    let issue_error = candidate_graph_error(&response.errors, "issueOrPullRequest");
    let discussion_error = candidate_graph_error(&response.errors, "discussion");
    let mut repository_data = response
        .data
        .and_then(|data| data.repository)
        .ok_or_else(|| GithubError::invalid("item", "missing repository data"))?;
    let item_present = repository_data.item.is_some();
    let issue_or_pull = repository_data
        .item
        .take()
        .and_then(|item| convert_graph_item(repository, item));
    let discussion = if spec.kind == GithubLookupKind::IssueOrPullRequest {
        None
    } else {
        if let Some(error) = discussion_error {
            return Err(error);
        }
        repository_data
            .discussion
            .map(|discussion| {
                hydrate_discussion(runner, cwd, repository, spec.number, discussion).map(
                    |discussion| GithubItem::Discussion(graph_discussion(repository, discussion)),
                )
            })
            .transpose()?
    };
    match spec.kind {
        GithubLookupKind::IssueOrPullRequest if issue_error.is_some() => {
            Ok(GraphFetch::NeedsRest { discussion: None })
        }
        GithubLookupKind::IssueOrPullRequest => issue_or_pull
            .map(GraphFetch::Item)
            .or_else(|| item_present.then_some(GraphFetch::NeedsRest { discussion: None }))
            .ok_or_else(|| {
                GithubError::new(
                    GithubErrorKind::NotFound,
                    format!(
                        "No issue or pull request #{} in {}",
                        spec.number,
                        repository.name_with_owner()
                    ),
                )
            }),
        GithubLookupKind::Discussion => discussion.map(GraphFetch::Item).ok_or_else(|| {
            GithubError::new(
                GithubErrorKind::NotFound,
                format!(
                    "No discussion #{} in {}",
                    spec.number,
                    repository.name_with_owner()
                ),
            )
        }),
        GithubLookupKind::Auto if issue_error.is_some() => Ok(GraphFetch::NeedsRest { discussion }),
        GithubLookupKind::Auto => match (issue_or_pull, item_present, discussion) {
            (Some(issue_or_pull), _, Some(discussion)) => {
                Ok(GraphFetch::Item(GithubItem::Ambiguous {
                    issue_or_pull_request: Box::new(issue_or_pull),
                    discussion: Box::new(discussion),
                }))
            }
            (Some(item), _, None) | (None, false, Some(item)) => Ok(GraphFetch::Item(item)),
            (None, true, discussion) => Ok(GraphFetch::NeedsRest { discussion }),
            (None, false, None) => Err(GithubError::new(
                GithubErrorKind::NotFound,
                format!(
                    "No issue, pull request, or discussion #{} in {}",
                    spec.number,
                    repository.name_with_owner()
                ),
            )),
        },
    }
}

fn candidate_graph_error(errors: &[GraphError], field: &str) -> Option<GithubError> {
    errors
        .iter()
        .find(|error| {
            error.path.iter().any(|part| part.as_str() == Some(field))
                && error.error_type.as_deref() != Some("NOT_FOUND")
        })
        .map(|error| {
            GithubError::new(
                GithubErrorKind::Incomplete,
                format!("GitHub could not fully resolve {field}: {}", error.message),
            )
        })
}

fn convert_graph_item(repository: &RepositoryRef, item: GraphItem) -> Option<GithubItem> {
    match item {
        GraphItem::Issue(issue) => {
            if issue.labels.truncated() || issue.comments.truncated() {
                return None;
            }
            let comments = issue
                .comments
                .nodes
                .into_iter()
                .map(|comment| DiscussionEntry {
                    kind: DiscussionKind::Comment,
                    author: graph_author(comment.author),
                    body: comment.body.unwrap_or_default(),
                    created_at: comment.created_at,
                    review_state: None,
                    thread_resolved: None,
                    path: None,
                    line: None,
                })
                .collect();
            Some(GithubItem::Issue(Issue {
                common: ItemCommon {
                    repository: repository.clone(),
                    number: issue.number,
                    title: issue.title,
                    state: issue.state.to_ascii_lowercase(),
                    author: graph_author(issue.author),
                    labels: graph_labels(issue.labels.nodes),
                    created_at: issue.created_at,
                    updated_at: issue.updated_at,
                    url: issue.url,
                    body: issue.body.unwrap_or_default(),
                },
                comments,
            }))
        }
        GraphItem::PullRequest(pull) => {
            if pull.labels.truncated()
                || pull.comments.truncated()
                || pull.reviews.truncated()
                || pull.review_threads.truncated()
                || pull.files.truncated()
                || pull
                    .review_threads
                    .nodes
                    .iter()
                    .any(|thread| thread.comments.truncated())
            {
                return None;
            }

            let mut discussion = Vec::new();
            discussion.extend(
                pull.comments
                    .nodes
                    .into_iter()
                    .map(|comment| DiscussionEntry {
                        kind: DiscussionKind::Comment,
                        author: graph_author(comment.author),
                        body: comment.body.unwrap_or_default(),
                        created_at: comment.created_at,
                        review_state: None,
                        thread_resolved: None,
                        path: None,
                        line: None,
                    }),
            );
            discussion.extend(
                pull.reviews
                    .nodes
                    .into_iter()
                    .map(|review| DiscussionEntry {
                        kind: DiscussionKind::Review,
                        author: graph_author(review.author),
                        body: review.body.unwrap_or_default(),
                        created_at: review.submitted_at.unwrap_or_default(),
                        review_state: review.state,
                        thread_resolved: None,
                        path: None,
                        line: None,
                    }),
            );
            for thread in pull.review_threads.nodes {
                let resolved = thread.is_resolved;
                discussion.extend(thread.comments.nodes.into_iter().map(|comment| {
                    DiscussionEntry {
                        kind: DiscussionKind::InlineReview,
                        author: graph_author(comment.author),
                        body: comment.body.unwrap_or_default(),
                        created_at: comment.created_at,
                        review_state: None,
                        thread_resolved: Some(resolved),
                        path: comment.path,
                        line: comment.line.or(comment.original_line),
                    }
                }));
            }
            discussion.sort_by(|left, right| left.created_at.cmp(&right.created_at));

            let files = pull
                .files
                .nodes
                .into_iter()
                .map(|file| ChangedFile {
                    path: file.path,
                    status: graph_file_status(&file.change_type),
                    additions: file.additions,
                    deletions: file.deletions,
                    changes: file.additions + file.deletions,
                    patch: None,
                })
                .collect();

            // REST reports a merged pull request as closed and flags the merge
            // separately; matching that keeps the rest of the app unaware of
            // which loader produced the item.
            let merged = pull.merged;
            let state = if pull.state.eq_ignore_ascii_case("merged") {
                "closed".to_string()
            } else {
                pull.state.to_ascii_lowercase()
            };

            Some(GithubItem::PullRequest(PullRequest {
                common: ItemCommon {
                    repository: repository.clone(),
                    number: pull.number,
                    title: pull.title,
                    state,
                    author: graph_author(pull.author),
                    labels: graph_labels(pull.labels.nodes),
                    created_at: pull.created_at,
                    updated_at: pull.updated_at,
                    url: pull.url,
                    body: pull.body.unwrap_or_default(),
                },
                draft: pull.is_draft,
                merged,
                mergeable_state: pull.mergeable.map(|state| state.to_ascii_lowercase()),
                base_ref: pull.base_ref_name,
                head_ref: pull.head_ref_name,
                additions: pull.additions,
                deletions: pull.deletions,
                changed_files: pull.changed_files,
                discussion,
                files,
                patches_loaded: false,
            }))
        }
    }
}

fn hydrate_discussion(
    runner: &dyn GhRunner,
    cwd: &Path,
    repository: &RepositoryRef,
    number: u64,
    mut discussion: GraphDiscussion,
) -> Result<GraphDiscussion, GithubError> {
    hydrate_comment_replies(runner, cwd, repository, &mut discussion.comments.nodes)?;
    while discussion.comments.page_info.has_next_page {
        let after = discussion
            .comments
            .page_info
            .end_cursor
            .clone()
            .ok_or_else(|| GithubError::invalid("discussion comments", "missing page cursor"))?;
        let args = graphql_args(
            &repository.host,
            DISCUSSION_COMMENTS_QUERY,
            &[
                ("owner", repository.owner.clone()),
                ("name", repository.name.clone()),
                ("number", number.to_string()),
                ("page", GRAPH_PAGE.to_string()),
                ("after", after),
            ],
        );
        let output = runner.run(cwd, &args)?;
        let response: GraphResponse<GraphDiscussionCommentsData> = serde_json::from_slice(&output)
            .map_err(|error| GithubError::invalid("discussion comments", error))?;
        let mut page = response
            .data
            .and_then(|data| data.repository)
            .and_then(|repository| repository.discussion)
            .map(|discussion| discussion.comments)
            .ok_or_else(|| {
                GithubError::invalid("discussion comments", "missing discussion page")
            })?;
        hydrate_comment_replies(runner, cwd, repository, &mut page.nodes)?;
        discussion.comments.nodes.extend(page.nodes);
        discussion.comments.page_info = page.page_info;
    }
    Ok(discussion)
}

fn hydrate_comment_replies(
    runner: &dyn GhRunner,
    cwd: &Path,
    repository: &RepositoryRef,
    comments: &mut [GraphDiscussionComment],
) -> Result<(), GithubError> {
    for comment in comments {
        while comment.replies.page_info.has_next_page {
            let after = comment
                .replies
                .page_info
                .end_cursor
                .clone()
                .ok_or_else(|| GithubError::invalid("discussion replies", "missing page cursor"))?;
            let args = graphql_args(
                &repository.host,
                DISCUSSION_REPLIES_QUERY,
                &[
                    ("id", comment.id.clone()),
                    ("page", GRAPH_PAGE.to_string()),
                    ("after", after),
                ],
            );
            let output = runner.run(cwd, &args)?;
            let response: GraphResponse<GraphDiscussionRepliesData> =
                serde_json::from_slice(&output)
                    .map_err(|error| GithubError::invalid("discussion replies", error))?;
            let page = response
                .data
                .and_then(|data| data.node)
                .map(|node| node.replies)
                .ok_or_else(|| {
                    GithubError::invalid("discussion replies", "missing replies page")
                })?;
            comment.replies.nodes.extend(page.nodes);
            comment.replies.page_info = page.page_info;
        }
    }
    Ok(())
}

fn graph_discussion(
    repository: &RepositoryRef,
    discussion: GraphDiscussion,
) -> RepositoryDiscussion {
    RepositoryDiscussion {
        common: ItemCommon {
            repository: repository.clone(),
            number: discussion.number,
            title: discussion.title,
            state: if discussion.closed {
                "closed".to_string()
            } else {
                "open".to_string()
            },
            author: graph_author(discussion.author),
            labels: Vec::new(),
            created_at: discussion.created_at,
            updated_at: discussion.updated_at,
            url: discussion.url,
            body: discussion.body.unwrap_or_default(),
        },
        category: discussion.category.name,
        answerable: discussion.category.is_answerable,
        answered: discussion.is_answered.unwrap_or(false),
        answer_chosen_at: discussion.answer_chosen_at,
        upvote_count: discussion.upvote_count,
        reactions: graph_reactions(discussion.reaction_groups),
        comments: discussion
            .comments
            .nodes
            .into_iter()
            .map(graph_discussion_comment)
            .collect(),
    }
}

fn graph_discussion_comment(comment: GraphDiscussionComment) -> DiscussionComment {
    DiscussionComment {
        author: graph_author(comment.author),
        body: comment.body.unwrap_or_default(),
        created_at: comment.created_at,
        upvote_count: comment.upvote_count,
        reactions: graph_reactions(comment.reaction_groups),
        is_answer: comment.is_answer,
        replies: comment
            .replies
            .nodes
            .into_iter()
            .map(|reply| DiscussionComment {
                author: graph_author(reply.author),
                body: reply.body.unwrap_or_default(),
                created_at: reply.created_at,
                upvote_count: reply.upvote_count,
                reactions: graph_reactions(reply.reaction_groups),
                is_answer: false,
                replies: Vec::new(),
            })
            .collect(),
    }
}

fn graph_author(actor: Option<GraphActor>) -> Author {
    Author {
        login: actor
            .map(|actor| actor.login)
            .unwrap_or_else(|| "ghost".to_string()),
    }
}

fn graph_reactions(groups: Vec<GraphReactionGroup>) -> Vec<ReactionCount> {
    groups
        .into_iter()
        .filter(|group| group.reactors.total_count > 0)
        .map(|group| ReactionCount {
            content: group.content,
            count: group.reactors.total_count,
        })
        .collect()
}

fn graph_labels(nodes: Vec<GraphLabel>) -> Vec<Label> {
    nodes
        .into_iter()
        .map(|label| Label {
            name: label.name,
            color: label.color,
        })
        .collect()
}

/// Translate GraphQL's change vocabulary into the REST one the UI already uses.
fn graph_file_status(change_type: &str) -> String {
    match change_type.to_ascii_lowercase().as_str() {
        "added" => "added",
        "deleted" => "removed",
        "renamed" => "renamed",
        "copied" => "copied",
        _ => "modified",
    }
    .to_string()
}

#[derive(Debug, Deserialize)]
struct GraphResponse<T> {
    data: Option<T>,
    #[serde(default)]
    errors: Vec<GraphError>,
}

#[derive(Debug, Deserialize)]
struct GraphError {
    message: String,
    #[serde(rename = "type")]
    error_type: Option<String>,
    #[serde(default)]
    path: Vec<serde_json::Value>,
}

#[derive(Debug, Deserialize)]
struct GraphItemData {
    repository: Option<GraphItemRepository>,
}

#[derive(Debug, Deserialize)]
struct GraphItemRepository {
    #[serde(rename = "issueOrPullRequest")]
    item: Option<GraphItem>,
    discussion: Option<GraphDiscussion>,
}

#[derive(Debug, Deserialize)]
#[serde(tag = "__typename")]
enum GraphItem {
    Issue(Box<GraphIssue>),
    PullRequest(Box<GraphPull>),
}

#[derive(Debug, Deserialize)]
struct GraphConnection<T> {
    #[serde(default = "Vec::new")]
    nodes: Vec<T>,
    #[serde(rename = "pageInfo")]
    page_info: GraphPageInfo,
}

impl<T> GraphConnection<T> {
    fn truncated(&self) -> bool {
        self.page_info.has_next_page
    }
}

#[derive(Debug, Deserialize)]
struct GraphPageInfo {
    #[serde(rename = "hasNextPage", default)]
    has_next_page: bool,
    #[serde(rename = "endCursor")]
    end_cursor: Option<String>,
}

#[derive(Debug, Deserialize)]
struct GraphActor {
    login: String,
}

#[derive(Debug, Deserialize)]
struct GraphLabel {
    name: String,
    color: Option<String>,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct GraphIssue {
    number: u64,
    title: String,
    state: String,
    body: Option<String>,
    url: String,
    created_at: String,
    updated_at: String,
    author: Option<GraphActor>,
    labels: GraphConnection<GraphLabel>,
    comments: GraphConnection<GraphComment>,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct GraphPull {
    number: u64,
    title: String,
    state: String,
    body: Option<String>,
    url: String,
    created_at: String,
    updated_at: String,
    is_draft: bool,
    merged: bool,
    mergeable: Option<String>,
    additions: u64,
    deletions: u64,
    changed_files: u64,
    base_ref_name: String,
    head_ref_name: String,
    author: Option<GraphActor>,
    labels: GraphConnection<GraphLabel>,
    comments: GraphConnection<GraphComment>,
    reviews: GraphConnection<GraphReview>,
    review_threads: GraphConnection<GraphThread>,
    files: GraphConnection<GraphFile>,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct GraphComment {
    body: Option<String>,
    created_at: String,
    author: Option<GraphActor>,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct GraphReview {
    body: Option<String>,
    state: Option<String>,
    submitted_at: Option<String>,
    author: Option<GraphActor>,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct GraphThread {
    is_resolved: bool,
    comments: GraphConnection<GraphThreadComment>,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct GraphThreadComment {
    body: Option<String>,
    path: Option<String>,
    line: Option<u64>,
    original_line: Option<u64>,
    created_at: String,
    author: Option<GraphActor>,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct GraphFile {
    path: String,
    additions: u64,
    deletions: u64,
    change_type: String,
}

fn strings(values: &[&str]) -> Vec<String> {
    values.iter().map(|value| (*value).to_string()).collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::cell::RefCell;

    /// A runner that replays canned `gh` output and records what was asked for.
    struct FakeRunner {
        responses: RefCell<Vec<Result<Vec<u8>, GithubError>>>,
        calls: RefCell<Vec<Vec<String>>>,
    }

    impl FakeRunner {
        fn new(responses: Vec<Result<Vec<u8>, GithubError>>) -> Self {
            Self {
                responses: RefCell::new(responses),
                calls: RefCell::new(Vec::new()),
            }
        }

        fn ok<S: AsRef<str>>(bodies: &[S]) -> Self {
            Self::new(
                bodies
                    .iter()
                    .map(|body| Ok(body.as_ref().as_bytes().to_vec()))
                    .collect(),
            )
        }
    }

    impl GhRunner for FakeRunner {
        fn run(&self, _cwd: &Path, args: &[String]) -> Result<Vec<u8>, GithubError> {
            self.calls.borrow_mut().push(args.to_vec());
            if self.responses.borrow().is_empty() {
                return Err(GithubError::new(
                    GithubErrorKind::Cli,
                    format!("unexpected call: {args:?}"),
                ));
            }
            self.responses.borrow_mut().remove(0)
        }
    }

    #[test]
    fn resolves_a_mixed_batch_of_references() {
        let runner = FakeRunner::ok(&[r#"{"data":{"repository":{
            "r1":{"__typename":"Issue","state":"OPEN"},
            "r2":{"__typename":"Issue","state":"CLOSED"},
            "r3":{"__typename":"PullRequest","state":"MERGED","isDraft":false},
            "r4":{"__typename":"PullRequest","state":"OPEN","isDraft":true},
            "r5":{"__typename":"PullRequest","state":"CLOSED","isDraft":false},
            "r6":null}},"errors":[{"message":"Could not resolve"}]}"#]);

        let resolved = resolve_references_with(
            &runner,
            Path::new("/repo"),
            &repository(),
            &[1, 2, 3, 4, 5, 6],
        )
        .expect("resolve");

        assert_eq!(
            resolved,
            vec![
                (
                    1,
                    Some(ReferenceStatus {
                        kind: ReferenceKind::Issue,
                        state: ReferenceState::Open
                    })
                ),
                (
                    2,
                    Some(ReferenceStatus {
                        kind: ReferenceKind::Issue,
                        state: ReferenceState::Closed
                    })
                ),
                (
                    3,
                    Some(ReferenceStatus {
                        kind: ReferenceKind::PullRequest,
                        state: ReferenceState::Merged
                    })
                ),
                (
                    4,
                    Some(ReferenceStatus {
                        kind: ReferenceKind::PullRequest,
                        state: ReferenceState::Draft
                    })
                ),
                (
                    5,
                    Some(ReferenceStatus {
                        kind: ReferenceKind::PullRequest,
                        state: ReferenceState::Closed
                    })
                ),
                // A number that is not a reference at all still gets an answer,
                // so the caller can stop asking about it.
                (6, None),
            ]
        );
    }

    #[test]
    fn reference_lookup_uses_one_call_and_respects_the_batch_limit() {
        let aliases: String = (1..=REFERENCE_BATCH + 10)
            .map(|number| format!("\"r{number}\":{{\"__typename\":\"Issue\",\"state\":\"OPEN\"}},"))
            .collect();
        let body = format!(
            "{{\"data\":{{\"repository\":{{{}\"unused\":null}}}}}}",
            aliases
        );
        let runner = FakeRunner::ok(&[body]);
        let numbers: Vec<u64> = (1..=(REFERENCE_BATCH + 10) as u64).collect();

        let resolved =
            resolve_references_with(&runner, Path::new("/repo"), &repository(), &numbers)
                .expect("resolve");

        assert_eq!(resolved.len(), REFERENCE_BATCH);
        let calls = runner.calls.borrow();
        assert_eq!(calls.len(), 1, "one round trip for the whole batch");
        let query = calls[0]
            .iter()
            .find(|argument| argument.starts_with("query="))
            .expect("query argument");
        assert!(query.contains(&format!("r{REFERENCE_BATCH}:issueOrPullRequest")));
        assert!(!query.contains(&format!("r{}:issueOrPullRequest", REFERENCE_BATCH + 1)));
    }

    #[test]
    fn missing_repository_data_is_an_error_not_a_batch_of_negative_answers() {
        let runner = FakeRunner::ok(&[r#"{"data":null,"errors":[{"message":"temporary"}]}"#]);

        let error = resolve_references_with(&runner, Path::new("/repo"), &repository(), &[1, 2])
            .unwrap_err();

        assert_eq!(error.kind, GithubErrorKind::InvalidResponse);
        assert!(error.message.contains("missing repository data"));
    }

    #[test]
    fn resolving_no_references_makes_no_calls() {
        let runner = FakeRunner::ok::<String>(&[]);

        let resolved =
            resolve_references_with(&runner, Path::new("/repo"), &repository(), &[]).expect("ok");

        assert!(resolved.is_empty());
        assert!(runner.calls.borrow().is_empty());
    }

    #[test]
    fn copilot_review_uses_the_requested_reviewers_endpoint() {
        let runner = FakeRunner::ok(&["{}"]);

        request_copilot_review_with(&runner, Path::new("/repo"), &repository(), 7)
            .expect("request succeeds");

        assert_eq!(
            runner.calls.borrow()[0],
            strings(&[
                "api",
                "--hostname",
                "github.com",
                "--method",
                "POST",
                "repos/octo/widgets/pulls/7/requested_reviewers",
                "-f",
                "reviewers[]=copilot-pull-request-reviewer[bot]",
            ])
        );
    }

    #[test]
    fn copilot_review_errors_name_the_action_that_failed() {
        let runner = FakeRunner::new(vec![Err(GithubError::new(
            GithubErrorKind::Cli,
            "GitHub rejected the reviewer",
        ))]);

        let error =
            request_copilot_review_with(&runner, Path::new("/repo"), &repository(), 7).unwrap_err();

        assert_eq!(error.kind, GithubErrorKind::Cli);
        assert_eq!(
            error.message,
            "Could not request Copilot review: GitHub rejected the reviewer"
        );
    }

    fn repository() -> RepositoryRef {
        RepositoryRef {
            host: "github.com".to_string(),
            owner: "octo".to_string(),
            name: "widgets".to_string(),
        }
    }

    #[test]
    fn parses_discussion_selectors_and_github_urls() {
        assert_eq!(
            parse_item_spec("2291").unwrap(),
            GithubItemSpec {
                number: 2291,
                kind: GithubLookupKind::Auto,
            }
        );
        assert_eq!(
            parse_item_spec("d 2291").unwrap().kind,
            GithubLookupKind::Discussion
        );
        assert_eq!(
            parse_item_spec("discussion #2291").unwrap().kind,
            GithubLookupKind::Discussion
        );
        assert_eq!(
            parse_item_spec("https://github.com/octo/widgets/discussions/2291?sort=top").unwrap(),
            GithubItemSpec {
                number: 2291,
                kind: GithubLookupKind::Discussion,
            }
        );
        assert_eq!(
            parse_item_spec("https://github.com/octo/widgets/pull/42")
                .unwrap()
                .kind,
            GithubLookupKind::IssueOrPullRequest
        );
        assert!(parse_item_spec("d 0").is_err());
    }

    fn discussion_response(
        issue: &str,
        comments: &str,
        has_next: bool,
        cursor: Option<&str>,
    ) -> String {
        let cursor = cursor
            .map(|cursor| format!(r#""{cursor}""#))
            .unwrap_or_else(|| "null".to_string());
        format!(
            r#"{{"data":{{"repository":{{"issueOrPullRequest":{issue},"discussion":{{
                "number":2291,"title":"Coordinate agents","body":"Top-level context",
                "url":"https://github.com/octo/widgets/discussions/2291",
                "createdAt":"2026-01-01T00:00:00Z","updatedAt":"2026-01-03T00:00:00Z",
                "closed":false,"isAnswered":true,"answerChosenAt":"2026-01-03T00:00:00Z",
                "upvoteCount":7,"author":{{"login":"lead"}},
                "category":{{"name":"Agents","isAnswerable":true}},
                "reactionGroups":[{{"content":"THUMBS_UP","reactors":{{"totalCount":3}}}}],
                "comments":{{"nodes":[{comments}],"pageInfo":{{"hasNextPage":{has_next},"endCursor":{cursor}}}}}
            }}}}}}}}"#
        )
    }

    fn discussion_comment(
        id: &str,
        body: &str,
        is_answer: bool,
        replies: &str,
        has_next: bool,
        cursor: Option<&str>,
    ) -> String {
        let cursor = cursor
            .map(|cursor| format!(r#""{cursor}""#))
            .unwrap_or_else(|| "null".to_string());
        format!(
            r#"{{"id":"{id}","body":"{body}","createdAt":"2026-01-02T00:00:00Z",
                "upvoteCount":2,"isAnswer":{is_answer},"author":{{"login":"agent"}},
                "reactionGroups":[],
                "replies":{{"nodes":[{replies}],"pageInfo":{{"hasNextPage":{has_next},"endCursor":{cursor}}}}}}}"#
        )
    }

    fn discussion_reply(body: &str) -> String {
        format!(
            r#"{{"body":"{body}","createdAt":"2026-01-02T01:00:00Z","upvoteCount":1,
                "author":{{"login":"reply-agent"}},"reactionGroups":[]}}"#
        )
    }

    #[test]
    fn loads_a_discussion_when_no_issue_or_pull_request_uses_the_number() {
        let comment = discussion_comment(
            "DC_1",
            "Accepted coordination",
            true,
            &discussion_reply("Nested reply"),
            false,
            None,
        );
        let runner = FakeRunner::ok(&[discussion_response("null", &comment, false, None)]);

        let item = fetch_item_from(&runner, Path::new("."), &repository(), 2291).unwrap();

        let GithubItem::Discussion(discussion) = item else {
            panic!("expected discussion");
        };
        assert_eq!(discussion.category, "Agents");
        assert!(discussion.answered);
        assert_eq!(discussion.upvote_count, 7);
        assert_eq!(discussion.reactions[0].content, "THUMBS_UP");
        assert!(discussion.comments[0].is_answer);
        assert_eq!(discussion.comments[0].replies[0].body, "Nested reply");
    }

    #[test]
    fn nullable_answer_state_loads_for_non_answerable_categories() {
        let response = discussion_response("null", "", false, None)
            .replace(r#""isAnswered":true"#, r#""isAnswered":null"#)
            .replace(r#""isAnswerable":true"#, r#""isAnswerable":false"#);
        let runner = FakeRunner::ok(&[response]);

        let item = fetch_item_from(&runner, Path::new("."), &repository(), 2291).unwrap();

        let GithubItem::Discussion(discussion) = item else {
            panic!("expected discussion");
        };
        assert!(!discussion.answerable);
        assert!(!discussion.answered);
    }

    #[test]
    fn bare_number_returns_a_choice_when_discussion_and_issue_collide() {
        let issue = r#"{"__typename":"Issue","number":2291,"title":"Issue title","state":"OPEN",
            "body":"issue","url":"https://github.com/octo/widgets/issues/2291",
            "createdAt":"2026-01-01T00:00:00Z","updatedAt":"2026-01-02T00:00:00Z",
            "author":{"login":"owner"},"labels":{"nodes":[],"pageInfo":{"hasNextPage":false}},
            "comments":{"nodes":[],"pageInfo":{"hasNextPage":false}}}"#;
        let runner = FakeRunner::ok(&[discussion_response(issue, "", false, None)]);

        let item = fetch_item_from(&runner, Path::new("."), &repository(), 2291).unwrap();

        let GithubItem::Ambiguous {
            issue_or_pull_request,
            discussion,
        } = item
        else {
            panic!("expected collision");
        };
        assert!(matches!(*issue_or_pull_request, GithubItem::Issue(_)));
        assert!(matches!(*discussion, GithubItem::Discussion(_)));
    }

    #[test]
    fn operational_issue_alias_error_uses_rest_without_losing_valid_discussion() {
        let mut response: serde_json::Value =
            serde_json::from_str(&discussion_response("null", "", false, None)).unwrap();
        response["errors"] = serde_json::json!([{
            "type": "INTERNAL",
            "path": ["repository", "issueOrPullRequest"],
            "message": "temporary resolver failure"
        }]);
        let issue = r#"{"number":2291,"title":"REST issue","state":"open",
            "user":{"login":"octo"},"labels":[],"created_at":"2026-01-01T00:00:00Z",
            "updated_at":"2026-01-02T00:00:00Z",
            "html_url":"https://github.com/octo/widgets/issues/2291","body":"text"}"#;
        let runner = FakeRunner::ok(&[response.to_string(), issue.to_string(), "[[]]".to_string()]);

        let item = fetch_item_from(&runner, Path::new("."), &repository(), 2291).unwrap();

        assert!(matches!(item, GithubItem::Ambiguous { .. }));
        assert_eq!(runner.calls.borrow().len(), 3);
    }

    #[test]
    fn rest_not_found_confirms_retained_discussion_is_unique() {
        let mut response: serde_json::Value =
            serde_json::from_str(&discussion_response("null", "", false, None)).unwrap();
        response["errors"] = serde_json::json!([{
            "type": "INTERNAL",
            "path": ["repository", "issueOrPullRequest"],
            "message": "temporary resolver failure"
        }]);
        let runner = FakeRunner::new(vec![
            Ok(response.to_string().into_bytes()),
            Err(GithubError::new(
                GithubErrorKind::NotFound,
                "no issue with that number",
            )),
        ]);

        let item = fetch_item_from(&runner, Path::new("."), &repository(), 2291).unwrap();

        assert!(matches!(item, GithubItem::Discussion(_)));
        assert_eq!(runner.calls.borrow().len(), 2);
    }

    #[test]
    fn not_found_after_rest_item_probe_does_not_discard_collision_uncertainty() {
        let mut response: serde_json::Value =
            serde_json::from_str(&discussion_response("null", "", false, None)).unwrap();
        response["errors"] = serde_json::json!([{
            "type": "INTERNAL",
            "path": ["repository", "issueOrPullRequest"],
            "message": "temporary resolver failure"
        }]);
        let issue = r#"{"number":2291,"title":"REST issue","state":"open",
            "user":{"login":"octo"},"labels":[],"created_at":"2026-01-01T00:00:00Z",
            "updated_at":"2026-01-02T00:00:00Z",
            "html_url":"https://github.com/octo/widgets/issues/2291","body":"text"}"#;
        let runner = FakeRunner::new(vec![
            Ok(response.to_string().into_bytes()),
            Ok(issue.as_bytes().to_vec()),
            Err(GithubError::new(
                GithubErrorKind::NotFound,
                "comments endpoint disappeared",
            )),
        ]);

        let error = fetch_item_from(&runner, Path::new("."), &repository(), 2291).unwrap_err();

        assert_eq!(error.kind, GithubErrorKind::NotFound);
        assert_eq!(runner.calls.borrow().len(), 3);
    }

    #[test]
    fn operational_discussion_alias_error_is_not_cached_as_issue_only() {
        let issue = r#"{"__typename":"Issue","number":2291,"title":"Issue title","state":"OPEN",
            "body":"issue","url":"https://github.com/octo/widgets/issues/2291",
            "createdAt":"2026-01-01T00:00:00Z","updatedAt":"2026-01-02T00:00:00Z",
            "author":{"login":"owner"},"labels":{"nodes":[],"pageInfo":{"hasNextPage":false}},
            "comments":{"nodes":[],"pageInfo":{"hasNextPage":false}}}"#;
        let mut response: serde_json::Value =
            serde_json::from_str(&discussion_response(issue, "", false, None)).unwrap();
        response["data"]["repository"]["discussion"] = serde_json::Value::Null;
        response["errors"] = serde_json::json!([{
            "type": "INTERNAL",
            "path": ["repository", "discussion"],
            "message": "temporary resolver failure"
        }]);
        let runner = FakeRunner::ok(&[response.to_string()]);

        let error = fetch_item_from(&runner, Path::new("."), &repository(), 2291).unwrap_err();

        assert_eq!(error.kind, GithubErrorKind::Incomplete);
        assert_eq!(runner.calls.borrow().len(), 1);
    }

    #[test]
    fn paginates_discussion_comments_and_replies() {
        let first_comment =
            discussion_comment("DC_1", "First", false, "", true, Some("reply-next"));
        let first = discussion_response("null", &first_comment, true, Some("comment-next"));
        let reply_page = format!(
            r#"{{"data":{{"node":{{"replies":{{"nodes":[{}],
                "pageInfo":{{"hasNextPage":false,"endCursor":null}}}}}}}}}}"#,
            discussion_reply("Late reply")
        );
        let second_comment = discussion_comment("DC_2", "Second", true, "", false, None);
        let comment_page = format!(
            r#"{{"data":{{"repository":{{"discussion":{{"comments":{{"nodes":[{second_comment}],
                "pageInfo":{{"hasNextPage":false,"endCursor":null}}}}}}}}}}}}"#
        );
        let runner = FakeRunner::ok(&[first, reply_page, comment_page]);

        let item = fetch_item_from(
            &runner,
            Path::new("."),
            &repository(),
            GithubItemSpec {
                number: 2291,
                kind: GithubLookupKind::Discussion,
            },
        )
        .unwrap();

        let GithubItem::Discussion(discussion) = item else {
            panic!("expected discussion");
        };
        assert_eq!(discussion.comments.len(), 2);
        assert_eq!(discussion.comments[0].replies[0].body, "Late reply");
        assert!(discussion.comments[1].is_answer);
        assert_eq!(runner.calls.borrow().len(), 3);
    }

    #[test]
    fn reference_lookup_marks_discussions_and_number_collisions() {
        let runner = FakeRunner::ok(&[r#"{"data":{"repository":{
            "r7":null,"d7":{"__typename":"Discussion","closed":false},
            "r8":{"__typename":"Issue","state":"OPEN"},
            "d8":{"__typename":"Discussion","closed":true}
        }}}"#]);

        let resolved =
            resolve_references_with(&runner, Path::new("."), &repository(), &[7, 8]).unwrap();

        assert_eq!(
            resolved,
            vec![
                (
                    7,
                    Some(ReferenceStatus {
                        kind: ReferenceKind::Discussion,
                        state: ReferenceState::Open,
                    })
                ),
                (
                    8,
                    Some(ReferenceStatus {
                        kind: ReferenceKind::Ambiguous,
                        state: ReferenceState::Open,
                    })
                ),
            ]
        );
    }

    #[test]
    fn older_enterprise_schema_falls_back_to_issue_only_reference_query() {
        let runner = FakeRunner::ok(&[
            r#"{"data":null,"errors":[{"message":"Field 'discussion' doesn't exist on type 'Repository'"}]}"#,
            r#"{"data":{"repository":{"r7":{"__typename":"Issue","state":"OPEN"}}}}"#,
        ]);

        let resolved =
            resolve_references_with(&runner, Path::new("."), &repository(), &[7]).unwrap();

        assert_eq!(
            resolved,
            vec![(
                7,
                Some(ReferenceStatus {
                    kind: ReferenceKind::Issue,
                    state: ReferenceState::Open,
                })
            )]
        );
        let calls = runner.calls.borrow();
        assert_eq!(calls.len(), 2);
        assert!(calls[0].iter().any(|arg| arg.contains(":discussion(")));
        assert!(!calls[1].iter().any(|arg| arg.contains(":discussion(")));
    }

    #[test]
    fn operational_reference_alias_error_is_not_cached_as_absence() {
        let runner = FakeRunner::ok(&[r#"{"data":{"repository":{
            "r7":null,"d7":{"__typename":"Discussion","closed":false}
        }},"errors":[{
            "type":"INTERNAL","path":["repository","r7"],"message":"temporary resolver failure"
        }]}"#]);

        let error =
            resolve_references_with(&runner, Path::new("."), &repository(), &[7]).unwrap_err();

        assert_eq!(error.kind, GithubErrorKind::Incomplete);
    }

    #[test]
    fn legacy_reference_fallback_validates_its_own_alias_errors() {
        let runner = FakeRunner::ok(&[
            r#"{"data":null,"errors":[{"message":"Field 'discussion' doesn't exist on type 'Repository'"}]}"#,
            r#"{"data":{"repository":{"r7":null}},"errors":[{
                "type":"INTERNAL","path":["repository","r7"],"message":"temporary resolver failure"
            }]}"#,
        ]);

        let error =
            resolve_references_with(&runner, Path::new("."), &repository(), &[7]).unwrap_err();

        assert_eq!(error.kind, GithubErrorKind::Incomplete);
        assert_eq!(runner.calls.borrow().len(), 2);
    }

    fn graph_pull(page_info: &str, files: &str) -> String {
        format!(
            r#"{{"data":{{"repository":{{"issueOrPullRequest":{{
                "__typename":"PullRequest",
                "number":7,"title":"A change","state":"MERGED","body":"details",
                "url":"https://github.com/octo/widgets/pull/7",
                "createdAt":"2026-01-01T00:00:00Z","updatedAt":"2026-01-02T00:00:00Z",
                "isDraft":false,"merged":true,"mergeable":"MERGEABLE",
                "additions":10,"deletions":3,"changedFiles":2,
                "baseRefName":"main","headRefName":"feature",
                "author":{{"login":"monalisa"}},
                "labels":{{"nodes":[{{"name":"bug","color":"ff0000"}}],"pageInfo":{{"hasNextPage":false}}}},
                "comments":{{"nodes":[{{"body":"looks good","createdAt":"2026-01-01T02:00:00Z","author":{{"login":"octo"}}}}],"pageInfo":{{"hasNextPage":false}}}},
                "reviews":{{"nodes":[{{"body":"approved","state":"APPROVED","submittedAt":"2026-01-01T03:00:00Z","author":{{"login":"hubot"}}}}],"pageInfo":{{"hasNextPage":false}}}},
                "reviewThreads":{{"nodes":[{{"isResolved":false,"comments":{{"nodes":[{{"body":"nit","path":"src/lib.rs","line":12,"originalLine":null,"createdAt":"2026-01-01T01:00:00Z","author":{{"login":"hubot"}}}}],"pageInfo":{{"hasNextPage":false}}}}}}],"pageInfo":{{"hasNextPage":{page_info}}}}},
                "files":{{"nodes":{files},"pageInfo":{{"hasNextPage":false}}}}
            }}}}}}}}"#
        )
    }

    const GRAPH_FILES: &str = r#"[
        {"path":"src/lib.rs","additions":8,"deletions":2,"changeType":"MODIFIED"},
        {"path":"old.rs","additions":0,"deletions":1,"changeType":"DELETED"}
    ]"#;

    #[test]
    fn a_pull_request_arrives_from_a_single_graphql_call() {
        let runner = FakeRunner::ok(&[graph_pull("false", GRAPH_FILES)]);

        let item = fetch_item_from(&runner, Path::new("."), &repository(), 7).unwrap();

        assert_eq!(runner.calls.borrow().len(), 1, "one round trip only");
        let GithubItem::PullRequest(pull) = item else {
            panic!("expected a pull request");
        };
        assert_eq!(pull.common.title, "A change");
        // REST reports a merged pull request as closed, and the rest of the app
        // relies on that, so the GraphQL MERGED state has to be translated.
        assert_eq!(pull.common.state, "closed");
        assert!(pull.merged);
        assert_eq!(pull.base_ref, "main");
        assert_eq!(pull.common.labels[0].name, "bug");
        assert!(!pull.patches_loaded, "diffs are deliberately deferred");
        assert!(pull.files.iter().all(|file| file.patch.is_none()));
    }

    #[test]
    fn graphql_file_changes_use_the_rest_vocabulary() {
        let runner = FakeRunner::ok(&[graph_pull("false", GRAPH_FILES)]);

        let item = fetch_item_from(&runner, Path::new("."), &repository(), 7).unwrap();

        let statuses: Vec<&str> = item
            .files()
            .iter()
            .map(|file| file.status.as_str())
            .collect();
        // The file-tree markers switch on these strings.
        assert_eq!(statuses, vec!["modified", "removed"]);
        assert_eq!(item.files()[0].changes, 10);
    }

    #[test]
    fn every_kind_of_pull_request_comment_lands_in_one_timeline() {
        let runner = FakeRunner::ok(&[graph_pull("false", GRAPH_FILES)]);

        let item = fetch_item_from(&runner, Path::new("."), &repository(), 7).unwrap();

        let kinds: Vec<DiscussionKind> = item.discussion().iter().map(|entry| entry.kind).collect();
        // Sorted by timestamp, so the inline comment at 01:00 comes first.
        assert_eq!(
            kinds,
            vec![
                DiscussionKind::InlineReview,
                DiscussionKind::Comment,
                DiscussionKind::Review
            ]
        );
        assert_eq!(item.discussion()[0].path.as_deref(), Some("src/lib.rs"));
        assert_eq!(item.discussion()[0].thread_resolved, Some(false));
    }

    #[test]
    fn an_item_larger_than_one_page_falls_back_to_the_rest_loader() {
        // `hasNextPage` on the review threads means the single call would have
        // silently dropped comments.
        let runner = FakeRunner::new(vec![
            Ok(graph_pull("true", GRAPH_FILES).into_bytes()),
            Err(GithubError::new(GithubErrorKind::Cli, "rest reached")),
        ]);

        let error = fetch_item_from(&runner, Path::new("."), &repository(), 7).unwrap_err();

        assert_eq!(error.message, "rest reached");
        let calls = runner.calls.borrow();
        assert_eq!(calls.len(), 2);
        assert_eq!(calls[1][1], "--hostname", "the retry is a REST call");
    }

    #[test]
    fn a_graphql_outage_still_serves_the_item_over_rest() {
        let issue = r#"{"number":7,"title":"Broken","state":"open","user":{"login":"octo"},
            "labels":[],"created_at":"2026-01-01T00:00:00Z","updated_at":"2026-01-01T00:00:00Z",
            "html_url":"https://github.com/octo/widgets/issues/7","body":"text"}"#;
        let runner = FakeRunner::new(vec![
            Err(GithubError::new(GithubErrorKind::Cli, "graphql disabled")),
            Ok(issue.as_bytes().to_vec()),
            Ok(b"[[]]".to_vec()),
        ]);

        let item = fetch_item_from(&runner, Path::new("."), &repository(), 7).unwrap();

        assert_eq!(item.common().title, "Broken");
    }

    #[test]
    fn a_missing_item_is_not_retried_over_rest() {
        let runner = FakeRunner::new(vec![Err(GithubError::new(
            GithubErrorKind::NotFound,
            "no such item",
        ))]);

        let error = fetch_item_from(&runner, Path::new("."), &repository(), 7).unwrap_err();

        // REST would fail identically, so retrying only doubles the wait.
        assert_eq!(error.kind, GithubErrorKind::NotFound);
        assert_eq!(runner.calls.borrow().len(), 1);
    }

    #[test]
    fn graphql_arguments_keep_numbers_typed() {
        let args = graphql_args(
            "github.com",
            "query{}",
            &[("owner", "octo".to_string()), ("number", "7".to_string())],
        );

        // `-F` types values; `-f` would send 7 as a string and the Int! variable
        // would be rejected.
        assert_eq!(
            args[0..4],
            strings(&["api", "graphql", "--hostname", "github.com"])[..]
        );
        assert!(args.contains(&"-F".to_string()));
        assert!(args.contains(&"number=7".to_string()));
        assert!(args.contains(&"query=query{}".to_string()));
    }

    #[test]
    fn a_deleted_account_reads_as_ghost_rather_than_blank() {
        assert_eq!(graph_author(None).login, "ghost");
    }

    #[test]
    fn repository_view_preserves_enterprise_hostname() {
        let repository = repository_from_view(RepoView {
            name_with_owner: "octo/widgets".to_string(),
            url: "https://github.example.com/octo/widgets".to_string(),
        })
        .unwrap();

        assert_eq!(repository.host, "github.example.com");
        assert_eq!(repository.name_with_owner(), "octo/widgets");
    }

    #[test]
    fn paginated_api_arguments_request_slurped_pages() {
        let repository = RepositoryRef {
            host: "github.com".to_string(),
            owner: "octo".to_string(),
            name: "widgets".to_string(),
        };

        assert_eq!(
            api_args(&repository, "repos/octo/widgets/issues/1/comments", true),
            strings(&[
                "api",
                "--hostname",
                "github.com",
                "--paginate",
                "--slurp",
                "repos/octo/widgets/issues/1/comments"
            ])
        );
    }

    #[test]
    fn paginated_arrays_flatten_in_page_order() {
        let pages: Vec<Vec<ApiComment>> = serde_json::from_str(
            r#"[
                [{"user":{"login":"one"},"body":"first","created_at":"2026-01-01T00:00:00Z"}],
                [{"user":{"login":"two"},"body":"second","created_at":"2026-01-02T00:00:00Z"}]
            ]"#,
        )
        .unwrap();
        let comments: Vec<ApiComment> = pages.into_iter().flatten().collect();

        assert_eq!(comments.len(), 2);
        assert_eq!(comments[1].body.as_deref(), Some("second"));
    }

    #[test]
    fn issue_pull_request_marker_classifies_items() {
        let issue: ApiIssue = serde_json::from_str(
            r#"{
                "number":7,
                "title":"A change",
                "state":"open",
                "user":{"login":"octo"},
                "labels":[],
                "created_at":"2026-01-01T00:00:00Z",
                "updated_at":"2026-01-02T00:00:00Z",
                "html_url":"https://github.com/octo/widgets/pull/7",
                "body":"body",
                "pull_request":{"url":"https://api.github.com/repos/octo/widgets/pulls/7"}
            }"#,
        )
        .unwrap();

        assert!(issue.pull_request.is_some());
    }
    #[test]
    fn gh_auth_status_json_lists_every_host_the_user_is_logged_into() {
        // The single-host payload is the exact shape gh 2.89 produced; the two-host
        // one extends it, because an enterprise host is the case that matters.
        let single = br#"{"hosts":{"github.com":[{"state":"success","active":true,"host":"github.com","login":"tupini07","tokenSource":"keyring","scopes":"repo","gitProtocol":"https"}]}}"#;
        let hosts = parse_auth_status_json(single).expect("payload parses");
        assert_eq!(hosts.len(), 1);
        assert_eq!(hosts[0].host, "github.com");
        assert_eq!(hosts[0].account.as_deref(), Some("tupini07"));
        assert!(hosts[0].healthy);

        let two = br#"{"hosts":{"github.com":[{"state":"success","login":"a"}],"github.corp.example":[{"state":"success","login":"b"}]}}"#;
        let hosts = parse_auth_status_json(two).expect("payload parses");
        assert_eq!(hosts.len(), 2);
        assert!(hosts.iter().any(|host| host.host == "github.corp.example"));
    }

    #[test]
    fn an_account_whose_state_is_not_success_is_reported_as_unhealthy() {
        // The failure payload could not be observed, so anything that is not literally
        // "success" has to count as broken rather than being guessed at.
        let payload = br#"{"hosts":{"github.com":[{"state":"error","login":"a"}]}}"#;
        let hosts = parse_auth_status_json(payload).expect("payload parses");
        assert!(!hosts[0].healthy);
    }

    #[test]
    fn a_json_payload_without_the_hosts_key_is_not_mistaken_for_being_logged_out() {
        // An older gh rejecting --json prints an error, not JSON. Returning None sends
        // the caller to the text parser instead of reporting "no hosts".
        assert!(parse_auth_status_json(b"unknown flag: --json").is_none());
    }

    #[test]
    fn gh_versions_without_json_support_fall_back_to_the_human_readable_hosts() {
        let text = "github.com\n  \u{2713} Logged in to github.com account tupini07 (keyring)\n  - Active account: true\n";
        let hosts = parse_auth_status_text(text).expect("text parses");
        assert_eq!(hosts.len(), 1);
        assert_eq!(hosts[0].host, "github.com");
        assert_eq!(hosts[0].account.as_deref(), Some("tupini07"));
        assert!(hosts[0].healthy);
    }

    #[test]
    fn the_modern_account_wording_and_the_older_as_wording_both_yield_the_host() {
        let modern =
            parse_auth_status_text("Logged in to github.com account me (keyring)").unwrap();
        let legacy = parse_auth_status_text("Logged in to github.com as me (oauth_token)").unwrap();
        assert_eq!(modern[0].account.as_deref(), Some("me"));
        assert_eq!(legacy[0].account.as_deref(), Some("me"));
        assert_eq!(modern[0].host, legacy[0].host);
    }

    #[test]
    fn a_host_gh_could_not_log_into_is_parsed_as_unhealthy_rather_than_skipped() {
        let hosts =
            parse_auth_status_text("X Failed to log in to github.corp.example account me").unwrap();
        assert_eq!(hosts[0].host, "github.corp.example");
        assert!(!hosts[0].healthy);
    }

    #[test]
    fn output_that_neither_parser_understands_is_reported_as_unknown_instead_of_guessing() {
        // Claiming "not logged in" for output we simply failed to read would send the
        // user off to run `gh auth login` for no reason.
        assert!(parse_auth_status_text("something entirely unexpected").is_none());
    }

    #[test]
    fn no_logged_in_hosts_is_reported_as_unauthenticated_rather_than_an_error() {
        let hosts = parse_auth_status_json(br#"{"hosts":{}}"#).expect("payload parses");
        assert_eq!(classify_hosts(hosts), GhAuth::NoHosts);
    }

    #[test]
    fn a_subscription_time_reaches_github_with_its_offset_intact() {
        // A bare + arrives as a space, GitHub drops the offset and reads the time as
        // Pacific, and every reply from the first hours of a subscription is skipped.
        let target = CommentTarget {
            host: "github.com",
            owner: "tupini07",
            repo: "copilot-session-tui",
            number: 2,
            discussion: false,
        };
        let since = chrono::DateTime::parse_from_rfc3339("2026-10-13T18:34:16.927104400Z")
            .unwrap()
            .with_timezone(&chrono::Utc)
            .to_rfc3339();

        let endpoint = issue_comments_endpoint(&target, Some(&since));

        assert_eq!(
            endpoint,
            concat!(
                "repos/tupini07/copilot-session-tui/issues/2/comments?per_page=100",
                "&since=2026-10-13T18:34:16.927104400%2B00:00"
            )
        );
    }
}

use anyhow::{Context, Result};
use fs4::{FileExt, TryLockError};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use sha2::{Digest, Sha256};
use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};

pub const DEFAULT_BRANCH_PREFIX: &str = "copilot/";
pub const REASONING_EFFORTS: &[&str] = &["low", "medium", "high", "xhigh"];
/// Plain control bytes like Ctrl-b travel reliably through every terminal we target,
/// unlike chords that depend on the keyboard-enhancement protocol.
pub const DEFAULT_MUX_PREFIX: &str = "C-b";
pub const DEFAULT_NTFY_SERVER: &str = "https://ntfy.sh";

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum GithubCommentFilter {
    #[default]
    All,
    Unresolved,
    Resolved,
}

impl GithubCommentFilter {
    pub const fn label(self) -> &'static str {
        match self {
            Self::All => "all",
            Self::Unresolved => "unresolved",
            Self::Resolved => "resolved",
        }
    }

    pub const fn settings_label(self) -> &'static str {
        match self {
            Self::All => "ALL",
            Self::Unresolved => "UNRESOLVED",
            Self::Resolved => "RESOLVED",
        }
    }

    pub const fn next(self) -> Self {
        match self {
            Self::All => Self::Unresolved,
            Self::Unresolved => Self::Resolved,
            Self::Resolved => Self::All,
        }
    }

    pub const fn includes(self, thread_resolved: Option<bool>) -> bool {
        match self {
            Self::All => true,
            Self::Unresolved => matches!(thread_resolved, Some(false)),
            Self::Resolved => matches!(thread_resolved, Some(true)),
        }
    }

    fn is_all(&self) -> bool {
        *self == Self::All
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PromptSnippet {
    pub name: String,
    pub prompt: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct NotificationConfig {
    #[serde(default)]
    pub enabled: bool,
    #[serde(default = "default_ntfy_server")]
    pub server: String,
    #[serde(default)]
    pub topic: String,
    #[serde(default = "default_true")]
    pub ready: bool,
    #[serde(default = "default_true")]
    pub error: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct UserConfig {
    #[serde(default)]
    pub yolo: bool,

    #[serde(default)]
    pub theme: crate::theme::ThemeName,

    /// Starred sessions, in the order the user arranged them.
    ///
    /// Deliberately a list rather than a set: the order is the feature, and it drives
    /// both the list grouping and the order favorite tabs open in. On disk this is a
    /// JSON array exactly as the previous set was, so older configs load unchanged.
    #[serde(
        default,
        deserialize_with = "deduplicated",
        skip_serializing_if = "Vec::is_empty"
    )]
    pub favorites: Vec<String>,

    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub snippets: Vec<PromptSnippet>,

    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub model: Option<String>,

    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reasoning_effort: Option<String>,

    /// Cap on how many times autopilot may continue on its own.
    ///
    /// `None` means CST passes nothing and Copilot applies its own default, which is
    /// currently 5. Storing the number here instead would pin the value to whatever
    /// that default happened to be on the day this was written.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_autopilot_continues: Option<u32>,

    /// Whether sessions may be woken by the GitHub threads they take part in.
    ///
    /// On by default, which costs nothing: the watcher never touches the network until
    /// some session has actually subscribed to a thread.
    #[serde(default = "default_true", skip_serializing_if = "is_true")]
    pub threads_enabled: bool,

    /// How many times in an hour one thread may wake one session.
    ///
    /// A rate and not a total. An agent cannot estimate how many exchanges a piece of
    /// work needs, so a budget would either cut off real conversations or be set so high
    /// it never binds; a rate asks nobody to estimate anything.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub thread_wakeups_per_hour: Option<u32>,

    /// Whether a busy thread is checked for going in circles.
    #[serde(default = "default_true", skip_serializing_if = "is_true")]
    pub thread_stall_detection: bool,

    /// GitHub logins whose comments may wake a session, besides your own.
    ///
    /// For working with a colleague's agent. Empty by default and deliberately explicit:
    /// a login in here can cause work to start on this machine, so it is something you
    /// write down rather than something CST infers from who has been talking.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub thread_trusted_authors: Vec<String>,

    #[serde(
        default,
        deserialize_with = "normalized_title_prefixes",
        skip_serializing_if = "Vec::is_empty"
    )]
    pub hidden_title_prefixes: Vec<String>,

    #[serde(
        default,
        deserialize_with = "normalized_path_prefixes",
        skip_serializing_if = "Vec::is_empty"
    )]
    pub hidden_path_prefixes: Vec<String>,

    /// Initial filter for pull-request comments in newly opened GitHub inspectors.
    #[serde(default, skip_serializing_if = "GithubCommentFilter::is_all")]
    pub github_comment_filter: GithubCommentFilter,

    /// Run sessions inside CST as multiplexed panes instead of launching and exiting.
    #[serde(default)]
    pub mux: bool,

    /// Key that escapes back to CST while a pane is focused.
    #[serde(default = "default_mux_prefix")]
    pub mux_prefix: String,

    #[serde(default)]
    pub tmux_keys: TmuxKeyConfig,

    #[serde(default)]
    pub worktree: WorktreeConfig,

    #[serde(default)]
    pub terminal: TerminalConfig,

    #[serde(default)]
    pub notifications: NotificationConfig,

    /// Kept at the root so older CST versions preserve it through `extra` when saving.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub ntfy_access_token: String,

    /// Kept at the root so older CST versions preserve it through `extra` when saving.
    #[serde(default)]
    pub ntfy_verbose: bool,

    #[serde(flatten)]
    pub(crate) extra: BTreeMap<String, Value>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct TerminalConfig {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub shell: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TmuxKeyConfig {
    #[serde(default = "default_tmux_resume_key")]
    pub resume_session: String,
    #[serde(default = "default_tmux_new_key")]
    pub new_session: String,
    #[serde(default = "default_tmux_worktree_key")]
    pub new_worktree: String,
    #[serde(default = "default_tmux_end_key")]
    pub end_session: String,
}

impl TmuxKeyConfig {
    pub fn matches_resume_session(&self, code: crossterm::event::KeyCode) -> bool {
        matches_key(code, &self.resume_session)
    }

    pub fn matches_new_session(&self, code: crossterm::event::KeyCode) -> bool {
        matches_key(code, &self.new_session)
    }

    pub fn matches_new_worktree(&self, code: crossterm::event::KeyCode) -> bool {
        matches_key(code, &self.new_worktree)
    }

    pub fn matches_end_session(&self, code: crossterm::event::KeyCode) -> bool {
        matches_key(code, &self.end_session)
    }
}

impl Default for TmuxKeyConfig {
    fn default() -> Self {
        Self {
            resume_session: default_tmux_resume_key(),
            new_session: default_tmux_new_key(),
            new_worktree: default_tmux_worktree_key(),
            end_session: default_tmux_end_key(),
        }
    }
}

/// Keeps the default out of a written `config.json`, so the file stays a record of what
/// the user actually chose rather than a snapshot of every default.
fn is_true(value: &bool) -> bool {
    *value
}

fn default_mux_prefix() -> String {
    DEFAULT_MUX_PREFIX.to_string()
}

fn default_tmux_resume_key() -> String {
    "t".to_string()
}

fn default_tmux_new_key() -> String {
    "m".to_string()
}

fn default_tmux_worktree_key() -> String {
    "M".to_string()
}

fn default_tmux_end_key() -> String {
    "X".to_string()
}

fn matches_key(code: crossterm::event::KeyCode, configured: &str) -> bool {
    configured_key_code(configured) == Some(code)
}

fn configured_key_code(configured: &str) -> Option<crossterm::event::KeyCode> {
    let mut characters = configured.chars();
    let expected = characters.next()?;
    characters
        .next()
        .is_none()
        .then_some(crossterm::event::KeyCode::Char(expected))
}

fn default_ntfy_server() -> String {
    DEFAULT_NTFY_SERVER.to_string()
}

const fn default_true() -> bool {
    true
}

impl Default for NotificationConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            server: default_ntfy_server(),
            topic: String::new(),
            ready: true,
            error: true,
        }
    }
}

/// Drop repeated favorites while preserving first-seen order.
///
/// The file is hand-editable and a duplicate would otherwise show the same session
/// twice in the list and open it twice as a tab.
fn deduplicated<'de, D>(deserializer: D) -> std::result::Result<Vec<String>, D::Error>
where
    D: serde::Deserializer<'de>,
{
    let mut seen = BTreeSet::new();
    Ok(Vec::<String>::deserialize(deserializer)?
        .into_iter()
        .filter(|id| seen.insert(id.clone()))
        .collect())
}

fn normalized_title_prefixes<'de, D>(deserializer: D) -> std::result::Result<Vec<String>, D::Error>
where
    D: serde::Deserializer<'de>,
{
    Ok(normalize_title_prefixes(Vec::<String>::deserialize(
        deserializer,
    )?))
}

fn normalized_path_prefixes<'de, D>(deserializer: D) -> std::result::Result<Vec<String>, D::Error>
where
    D: serde::Deserializer<'de>,
{
    Ok(normalize_path_prefixes(Vec::<String>::deserialize(
        deserializer,
    )?))
}

pub fn parse_title_prefixes(input: &str) -> Vec<String> {
    normalize_title_prefixes(input.split(',').map(str::to_string))
}

pub fn parse_path_prefixes(input: &str) -> Vec<String> {
    normalize_path_prefixes(input.split(',').map(str::to_string))
}

fn normalize_title_prefixes(prefixes: impl IntoIterator<Item = String>) -> Vec<String> {
    let mut seen = BTreeSet::new();
    prefixes
        .into_iter()
        .filter_map(|prefix| {
            let prefix = prefix.trim().to_string();
            (!prefix.is_empty() && seen.insert(prefix.to_lowercase())).then_some(prefix)
        })
        .collect()
}

fn normalize_path_prefixes(prefixes: impl IntoIterator<Item = String>) -> Vec<String> {
    let mut seen = BTreeSet::new();
    prefixes
        .into_iter()
        .filter_map(|prefix| {
            let prefix = prefix.trim().to_string();
            (!prefix.is_empty() && seen.insert(path_prefix_key(&prefix))).then_some(prefix)
        })
        .collect()
}

fn path_prefix_key(prefix: &str) -> String {
    #[cfg(windows)]
    {
        prefix.replace('/', "\\").to_lowercase()
    }
    #[cfg(not(windows))]
    {
        prefix.to_string()
    }
}

impl Default for UserConfig {
    fn default() -> Self {
        Self {
            yolo: false,
            theme: crate::theme::ThemeName::Classic,
            favorites: Vec::new(),
            snippets: Vec::new(),
            model: None,
            reasoning_effort: None,
            max_autopilot_continues: None,
            threads_enabled: true,
            thread_wakeups_per_hour: None,
            thread_stall_detection: true,
            thread_trusted_authors: Vec::new(),
            hidden_title_prefixes: Vec::new(),
            hidden_path_prefixes: Vec::new(),
            github_comment_filter: GithubCommentFilter::All,
            mux: false,
            mux_prefix: default_mux_prefix(),
            tmux_keys: TmuxKeyConfig::default(),
            worktree: WorktreeConfig::default(),
            terminal: TerminalConfig::default(),
            notifications: NotificationConfig::default(),
            ntfy_access_token: String::new(),
            ntfy_verbose: false,
            extra: BTreeMap::new(),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct WorktreeConfig {
    #[serde(default = "default_branch_prefix")]
    pub branch_prefix: String,

    #[serde(default = "default_worktree_root")]
    pub root: PathBuf,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EffectiveWorktreeConfig {
    pub branch_prefix: String,
    pub root: PathBuf,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct ProjectConfig {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub worktree: Option<ProjectWorktreeConfig>,

    /// Whether sessions in this repository start with `--yolo`.
    ///
    /// Tri-state on purpose: absent means "whatever the user's global setting says",
    /// so a repository that wants to force the permission prompts back on can say
    /// `false` rather than only being able to opt in.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub yolo: Option<bool>,

    /// Cap on autopilot continuations for sessions in this repository. Absent means
    /// "whatever the global setting says".
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_autopilot_continues: Option<u32>,

    /// Whether sessions in this repository may be woken by GitHub threads. Absent means
    /// "whatever the global setting says", so a repository can opt out on its own.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub threads_enabled: Option<bool>,

    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub snippets: Vec<PromptSnippet>,

    #[serde(flatten)]
    extra: BTreeMap<String, Value>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct ProjectWorktreeConfig {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub branch_prefix: Option<String>,

    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub root: Option<PathBuf>,

    #[serde(flatten)]
    extra: BTreeMap<String, Value>,
}

#[derive(Debug, Clone)]
pub struct ProjectSettings {
    pub repository_root: PathBuf,
    config: ProjectConfig,
    global: EffectiveWorktreeConfig,
    global_yolo: bool,
    global_max_autopilot_continues: Option<u32>,
    global_threads_enabled: bool,
}

impl Default for WorktreeConfig {
    fn default() -> Self {
        Self {
            branch_prefix: default_branch_prefix(),
            root: default_worktree_root(),
        }
    }
}

impl ProjectSettings {
    pub fn load(repository_root: &Path, global: &UserConfig) -> Result<Self> {
        let repository_root = canonicalize_or_absolute(repository_root);
        let mut config = load_project_config(&repository_root)?;
        if config
            .worktree
            .as_ref()
            .is_some_and(ProjectWorktreeConfig::is_empty)
        {
            config.worktree = None;
        }
        Ok(Self {
            repository_root,
            config,
            global: global_worktree(global),
            global_yolo: global.yolo,
            global_max_autopilot_continues: global.max_autopilot_continues,
            global_threads_enabled: global.threads_enabled,
        })
    }

    pub fn yolo_override(&self) -> Option<bool> {
        self.config.yolo
    }

    pub fn effective_yolo(&self) -> bool {
        self.config.yolo.unwrap_or(self.global_yolo)
    }

    pub fn set_yolo_override(&mut self, value: Option<bool>) {
        self.config.yolo = value;
    }

    pub fn max_autopilot_continues_override(&self) -> Option<u32> {
        self.config.max_autopilot_continues
    }

    /// The cap to use here, or `None` to leave Copilot's own default alone.
    pub fn effective_max_autopilot_continues(&self) -> Option<u32> {
        self.config
            .max_autopilot_continues
            .or(self.global_max_autopilot_continues)
    }

    pub fn set_max_autopilot_continues_override(&mut self, value: Option<u32>) {
        self.config.max_autopilot_continues = value;
    }

    pub fn threads_enabled_override(&self) -> Option<bool> {
        self.config.threads_enabled
    }

    pub fn effective_threads_enabled(&self) -> bool {
        self.config
            .threads_enabled
            .unwrap_or(self.global_threads_enabled)
    }

    pub fn set_threads_enabled_override(&mut self, value: Option<bool>) {
        self.config.threads_enabled = value;
    }

    pub fn branch_prefix_override(&self) -> Option<&str> {
        self.config
            .worktree
            .as_ref()
            .and_then(|worktree| worktree.branch_prefix.as_deref())
    }

    pub fn root_override(&self) -> Option<&Path> {
        self.config
            .worktree
            .as_ref()
            .and_then(|worktree| worktree.root.as_deref())
    }

    pub fn effective_branch_prefix(&self) -> &str {
        self.branch_prefix_override()
            .unwrap_or(&self.global.branch_prefix)
    }

    pub fn effective_root(&self) -> PathBuf {
        match self.root_override() {
            Some(root) => resolve_path(root, &self.repository_root),
            None => self.global.root.clone(),
        }
    }

    pub fn set_branch_prefix_override(&mut self, value: Option<String>) {
        self.worktree_mut().branch_prefix = value;
        self.remove_empty_worktree();
    }

    pub fn set_root_override(&mut self, value: Option<PathBuf>) {
        self.worktree_mut().root = value;
        self.remove_empty_worktree();
    }

    pub fn save(&self) -> Result<()> {
        save_project_config(&self.repository_root, &self.config)
    }

    pub fn effective(&self) -> EffectiveWorktreeConfig {
        EffectiveWorktreeConfig {
            branch_prefix: self.effective_branch_prefix().to_string(),
            root: self.effective_root(),
        }
    }

    pub fn snippets(&self) -> &[PromptSnippet] {
        &self.config.snippets
    }

    pub fn set_snippets(&mut self, snippets: Vec<PromptSnippet>) {
        self.config.snippets = snippets;
    }

    pub fn refresh_global(&mut self, global: &UserConfig) {
        self.global = global_worktree(global);
        self.global_yolo = global.yolo;
        self.global_max_autopilot_continues = global.max_autopilot_continues;
        self.global_threads_enabled = global.threads_enabled;
    }

    fn worktree_mut(&mut self) -> &mut ProjectWorktreeConfig {
        self.config
            .worktree
            .get_or_insert_with(ProjectWorktreeConfig::default)
    }

    fn remove_empty_worktree(&mut self) {
        let empty = self
            .config
            .worktree
            .as_ref()
            .is_some_and(ProjectWorktreeConfig::is_empty);
        if empty {
            self.config.worktree = None;
        }
    }
}

impl ProjectConfig {
    /// Whether there is nothing left worth keeping a `.cst.json` for.
    ///
    /// Every field has to be listed: a config that looks empty is deleted from disk,
    /// so forgetting one here silently throws that setting away on the next save.
    fn is_empty(&self) -> bool {
        self.worktree.is_none()
            && self.yolo.is_none()
            && self.max_autopilot_continues.is_none()
            && self.threads_enabled.is_none()
            && self.snippets.is_empty()
            && self.extra.is_empty()
    }
}

impl ProjectWorktreeConfig {
    fn is_empty(&self) -> bool {
        self.branch_prefix.is_none() && self.root.is_none() && self.extra.is_empty()
    }
}

fn default_branch_prefix() -> String {
    DEFAULT_BRANCH_PREFIX.to_string()
}

pub fn default_worktree_root() -> PathBuf {
    dirs::data_local_dir()
        .unwrap_or_else(|| {
            dirs::home_dir()
                .unwrap_or_else(|| PathBuf::from("."))
                .join(".local")
                .join("share")
        })
        .join("cst")
        .join("wt")
}

pub fn config_path() -> PathBuf {
    dirs::config_dir()
        .unwrap_or_else(|| {
            dirs::home_dir()
                .unwrap_or_else(|| PathBuf::from("."))
                .join(".config")
        })
        .join("copilot-session-tui")
        .join("config.json")
}

fn global_snippets_path() -> PathBuf {
    config_path()
        .parent()
        .map(Path::to_path_buf)
        .unwrap_or_else(|| PathBuf::from("."))
        .join("snippets.json")
}

pub fn project_config_path(repository_root: &Path) -> PathBuf {
    repository_root.join(".cst.json")
}

pub fn global_worktree(config: &UserConfig) -> EffectiveWorktreeConfig {
    let base = config_path()
        .parent()
        .map(Path::to_path_buf)
        .unwrap_or_else(|| PathBuf::from("."));
    EffectiveWorktreeConfig {
        branch_prefix: config.worktree.branch_prefix.clone(),
        root: resolve_path(&config.worktree.root, &base),
    }
}

pub fn effective_worktree(
    config: &UserConfig,
    repository_root: &Path,
) -> Result<EffectiveWorktreeConfig> {
    Ok(ProjectSettings::load(repository_root, config)?.effective())
}

pub fn load() -> UserConfig {
    let config_path = config_path();
    let snippets_path = global_snippets_path();
    let mut config = load_base_config(&config_path).unwrap_or_default();
    if let Err(error) = validate_tmux_keys(&config.tmux_keys) {
        eprintln!("Warning: invalid tmux shortcut configuration: {error}; using defaults");
        config.tmux_keys = TmuxKeyConfig::default();
    }
    // A damaged sidecar must not make unrelated settings/favorites fall back to
    // defaults and later overwrite a valid config.json.
    let _ = overlay_global_snippets(&mut config, &snippets_path);
    config
}

pub(crate) fn load_checked_from(config_path: &Path) -> Result<UserConfig> {
    let snippets_path = config_path
        .parent()
        .map(Path::to_path_buf)
        .unwrap_or_else(|| PathBuf::from("."))
        .join("snippets.json");
    load_checked_in(config_path, &snippets_path)
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ConfigRevision {
    Missing,
    Present([u8; 32]),
}

pub(crate) fn config_revision(path: &Path) -> Result<ConfigRevision> {
    match fs::read(path) {
        Ok(content) => Ok(content_revision(&content)),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(ConfigRevision::Missing),
        Err(error) => Err(error)
            .with_context(|| format!("Failed to inspect global settings: {}", path.display())),
    }
}

fn content_revision(content: &[u8]) -> ConfigRevision {
    ConfigRevision::Present(Sha256::digest(content).into())
}

fn load_checked_in(config_path: &Path, snippets_path: &Path) -> Result<UserConfig> {
    let mut config = load_base_config(config_path)?;
    validate_tmux_keys(&config.tmux_keys)?;
    overlay_global_snippets(&mut config, snippets_path)?;
    Ok(config)
}

pub(crate) fn load_base_config(config_path: &Path) -> Result<UserConfig> {
    Ok(load_existing_base_config(config_path)?.unwrap_or_default())
}

pub(crate) fn load_existing_base_config(config_path: &Path) -> Result<Option<UserConfig>> {
    Ok(load_existing_base_config_with_revision(config_path)?.map(|(config, _revision)| config))
}

pub(crate) fn load_existing_base_config_with_revision(
    config_path: &Path,
) -> Result<Option<(UserConfig, ConfigRevision)>> {
    match fs::read_to_string(config_path) {
        Ok(content) => {
            let revision = content_revision(content.as_bytes());
            serde_json::from_str(&content)
                .map(|config| Some((config, revision)))
                .with_context(|| format!("Invalid global settings in {}", config_path.display()))
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(error) => Err(error)
            .with_context(|| format!("Failed to read global settings: {}", config_path.display())),
    }
}

fn overlay_global_snippets(config: &mut UserConfig, snippets_path: &Path) -> Result<()> {
    let lock_path = snippets_path.with_extension("lock");
    let _lock = SnippetLock::acquire(&lock_path)?;
    overlay_global_snippets_locked(config, snippets_path)
}

fn overlay_global_snippets_locked(config: &mut UserConfig, snippets_path: &Path) -> Result<()> {
    match fs::read_to_string(snippets_path) {
        Ok(content) => {
            config.snippets = serde_json::from_str(&content).with_context(|| {
                format!("Invalid global snippets in {}", snippets_path.display())
            })?;
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            // One-time migration from the original config.json field. The sidecar is
            // authoritative afterward, so a long-running pre-snippet CST can rewrite
            // config.json without erasing prompts.
            if !config.snippets.is_empty() {
                write_json_atomic(snippets_path, &config.snippets)?;
            }
        }
        Err(error) => {
            return Err(error).with_context(|| {
                format!(
                    "Failed to read global snippets: {}",
                    snippets_path.display()
                )
            });
        }
    }
    Ok(())
}

#[cfg(test)]
pub(crate) fn save_to(path: &Path, config: &UserConfig) -> Result<()> {
    let _lock = ConfigLock::acquire(path)?;
    write_json_atomic(path, config)
}

pub(crate) enum ConfigSaveOutcome {
    Saved(ConfigRevision),
    Changed,
}

pub(crate) fn save_if_revision(
    path: &Path,
    expected: Option<ConfigRevision>,
    config: &UserConfig,
) -> Result<ConfigSaveOutcome> {
    let _lock = ConfigLock::acquire(path)?;
    let current = config_revision(path)?;
    if expected.is_some_and(|expected| expected != current) {
        return Ok(ConfigSaveOutcome::Changed);
    }
    Ok(ConfigSaveOutcome::Saved(write_json_atomic_with_revision(
        path, config,
    )?))
}

pub fn validate_ntfy_topic(topic: &str) -> Result<()> {
    if topic.is_empty() {
        anyhow::bail!("ntfy topic is required when notifications are enabled");
    }
    if topic.len() > 64
        || !topic
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_'))
    {
        anyhow::bail!("ntfy topic must be 1-64 letters, numbers, dashes, or underscores");
    }
    Ok(())
}

pub fn validate_ntfy_access_token(token: &str) -> Result<()> {
    if token.len() > 512
        || token.chars().any(|character| {
            character.is_control() || character.is_whitespace() || !character.is_ascii()
        })
    {
        anyhow::bail!("ntfy access token must be printable ASCII without spaces");
    }
    Ok(())
}

pub fn normalize_ntfy_server(server: &str) -> Result<String> {
    let server = server.trim().trim_end_matches('/');
    let uri: ureq::http::Uri = server
        .parse()
        .map_err(|_| anyhow::anyhow!("ntfy server is not a valid URL"))?;
    if !matches!(uri.scheme_str(), Some("http" | "https")) {
        anyhow::bail!("ntfy server must start with http:// or https://");
    }
    if uri.host().is_none_or(str::is_empty)
        || uri
            .authority()
            .is_none_or(|authority| !valid_http_authority(authority.as_str()))
        || !matches!(uri.path(), "" | "/")
        || uri.query().is_some()
        || server.contains('#')
    {
        anyhow::bail!("ntfy server must contain a valid host and no path, query, or fragment");
    }
    Ok(server.to_string())
}

fn valid_http_authority(authority: &str) -> bool {
    if authority.is_empty() || authority.contains([' ', '@']) {
        return false;
    }
    if let Some(rest) = authority.strip_prefix('[') {
        let Some(end) = rest.find(']') else {
            return false;
        };
        if end == 0 {
            return false;
        }
        let suffix = &rest[end + 1..];
        return suffix.is_empty() || suffix.strip_prefix(':').is_some_and(valid_port);
    }
    if authority.matches(':').count() > 1 {
        return false;
    }
    match authority.rsplit_once(':') {
        Some((host, port)) => !host.is_empty() && valid_port(port),
        None => true,
    }
}

fn valid_port(port: &str) -> bool {
    !port.is_empty() && port.parse::<u16>().is_ok_and(|port| port > 0)
}

pub fn validate_notification_config(config: &NotificationConfig) -> Result<()> {
    if !config.enabled {
        return Ok(());
    }
    let _ = normalize_ntfy_server(&config.server)?;
    validate_ntfy_topic(config.topic.trim())?;
    Ok(())
}

pub fn validate_user_notification_config(config: &UserConfig) -> Result<()> {
    validate_notification_config(&config.notifications)?;
    if config.notifications.enabled {
        validate_ntfy_access_token(config.ntfy_access_token.trim())?;
    }
    Ok(())
}

pub fn validate_tmux_keys(keys: &TmuxKeyConfig) -> Result<()> {
    let configured = [
        ("resume_session", keys.resume_session.as_str(), false),
        ("new_session", keys.new_session.as_str(), false),
        ("new_worktree", keys.new_worktree.as_str(), false),
        ("end_session", keys.end_session.as_str(), true),
    ];
    let mut seen = BTreeSet::new();
    for (name, value, used_after_prefix) in configured {
        let mut characters = value.chars();
        let Some(character) = characters.next() else {
            anyhow::bail!("tmux_keys.{name} must be one printable character");
        };
        if characters.next().is_some()
            || character.is_control()
            || character.is_whitespace()
            || !character.is_ascii()
        {
            anyhow::bail!("tmux_keys.{name} must be one printable ASCII character");
        }
        if !seen.insert(character) {
            anyhow::bail!("tmux shortcut '{character}' is assigned more than once");
        }
        if !used_after_prefix
            && matches!(
                character,
                'q' | 'k'
                    | 'j'
                    | 'r'
                    | 'e'
                    | 'g'
                    | 'T'
                    | ' '
                    | 'd'
                    | 'f'
                    | 'p'
                    | 's'
                    | 'c'
                    | 'n'
                    | 'N'
                    | '?'
                    | ','
                    | '.'
                    | 'u'
            )
        {
            anyhow::bail!(
                "tmux shortcut '{character}' conflicts with an existing session-list command"
            );
        }
        if used_after_prefix
            && matches!(
                character,
                'd' | 'n'
                    | 'p'
                    | 'x'
                    | 'w'
                    | 'c'
                    | 'e'
                    | 't'
                    | 's'
                    | 'u'
                    | 'q'
                    | 'm'
                    | 'h'
                    | 'g'
                    | '0'..='9'
            )
        {
            anyhow::bail!(
                "tmux shortcut '{character}' conflicts with an existing multiplexer command"
            );
        }
    }
    Ok(())
}

pub fn save_global_snippets_if_unchanged(
    original: &[PromptSnippet],
    snippets: &[PromptSnippet],
) -> Result<UserConfig> {
    let snippets_path = global_snippets_path();
    let lock_path = snippets_path.with_extension("lock");
    save_global_snippets_if_unchanged_in(
        &config_path(),
        &snippets_path,
        &lock_path,
        original,
        snippets,
    )
}

fn save_global_snippets_if_unchanged_in(
    config_path: &Path,
    snippets_path: &Path,
    lock_path: &Path,
    original: &[PromptSnippet],
    snippets: &[PromptSnippet],
) -> Result<UserConfig> {
    let _lock = SnippetLock::acquire(lock_path)?;
    let mut config = load_base_config(config_path)?;
    overlay_global_snippets_locked(&mut config, snippets_path)?;
    if config.snippets != original {
        anyhow::bail!("Global snippets changed on disk; close and reopen the snippet manager");
    }
    write_json_atomic(snippets_path, snippets)?;
    config.snippets = snippets.to_vec();
    Ok(config)
}

struct SnippetLock {
    file: fs::File,
}

impl SnippetLock {
    fn acquire(path: &Path) -> Result<Self> {
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent)?;
        }
        let file = fs::OpenOptions::new()
            .create(true)
            .truncate(false)
            .read(true)
            .write(true)
            .open(path)
            .with_context(|| format!("Failed to open snippet lock {}", path.display()))?;
        match FileExt::try_lock(&file) {
            Ok(()) => Ok(Self { file }),
            Err(TryLockError::WouldBlock) => {
                anyhow::bail!("Another CST instance is saving global snippets")
            }
            Err(TryLockError::Error(error)) => {
                Err(error).with_context(|| format!("Failed to lock {}", path.display()))
            }
        }
    }
}

impl Drop for SnippetLock {
    fn drop(&mut self) {
        let _ = FileExt::unlock(&self.file);
    }
}

struct ConfigLock {
    file: fs::File,
}

impl ConfigLock {
    fn acquire(config_path: &Path) -> Result<Self> {
        let lock_path = config_path.with_extension("lock");
        if let Some(parent) = lock_path.parent() {
            fs::create_dir_all(parent)?;
        }
        let file = fs::OpenOptions::new()
            .create(true)
            .truncate(false)
            .read(true)
            .write(true)
            .open(&lock_path)
            .with_context(|| format!("Failed to open config lock {}", lock_path.display()))?;
        match FileExt::try_lock(&file) {
            Ok(()) => Ok(Self { file }),
            Err(TryLockError::WouldBlock) => {
                anyhow::bail!("Another CST instance is saving global settings")
            }
            Err(TryLockError::Error(error)) => {
                Err(error).with_context(|| format!("Failed to lock {}", lock_path.display()))
            }
        }
    }
}

impl Drop for ConfigLock {
    fn drop(&mut self) {
        let _ = FileExt::unlock(&self.file);
    }
}

fn load_project_config(repository_root: &Path) -> Result<ProjectConfig> {
    let path = project_config_path(repository_root);
    if !path.exists() {
        return Ok(ProjectConfig::default());
    }
    let content = fs::read_to_string(&path)
        .with_context(|| format!("Failed to read project settings: {}", path.display()))?;
    serde_json::from_str(&content).with_context(|| {
        format!(
            "Invalid project settings in {}. Fix the JSON before editing it in the TUI",
            path.display()
        )
    })
}

fn save_project_config(repository_root: &Path, config: &ProjectConfig) -> Result<()> {
    let path = project_config_path(repository_root);
    if config.is_empty() {
        if path.exists() {
            fs::remove_file(&path).with_context(|| {
                format!("Failed to remove project settings: {}", path.display())
            })?;
        }
        return Ok(());
    }
    write_json_atomic(&path, config)
}

fn write_json_atomic<T: Serialize + ?Sized>(path: &Path, value: &T) -> Result<()> {
    write_json_atomic_with_revision(path, value).map(|_| ())
}

fn write_json_atomic_with_revision<T: Serialize + ?Sized>(
    path: &Path,
    value: &T,
) -> Result<ConfigRevision> {
    let parent = path
        .parent()
        .context("Configuration path has no parent directory")?;
    fs::create_dir_all(parent)
        .with_context(|| format!("Failed to create config directory: {}", parent.display()))?;

    let mut content = serde_json::to_vec_pretty(value).context("Failed to serialize config")?;
    content.push(b'\n');
    let revision = content_revision(&content);
    let mut temp = tempfile::NamedTempFile::new_in(parent)
        .with_context(|| format!("Failed to create temporary config in {}", parent.display()))?;
    temp.as_file_mut().write_all(&content)?;
    temp.as_file_mut().sync_all()?;
    temp.persist(path)
        .map_err(|error| error.error)
        .with_context(|| format!("Failed to replace config atomically: {}", path.display()))?;
    Ok(revision)
}

fn resolve_path(path: &Path, base: &Path) -> PathBuf {
    if path.is_absolute() {
        path.to_path_buf()
    } else {
        base.join(path)
    }
}

fn canonicalize_or_absolute(path: &Path) -> PathBuf {
    let resolved = fs::canonicalize(path).unwrap_or_else(|_| {
        if path.is_absolute() {
            path.to_path_buf()
        } else {
            std::env::current_dir()
                .unwrap_or_else(|_| PathBuf::from("."))
                .join(path)
        }
    });
    let text = resolved.to_string_lossy();
    if let Some(stripped) = text.strip_prefix(r"\\?\UNC\") {
        return PathBuf::from(format!(r"\\{stripped}"));
    }
    match text.strip_prefix(r"\\?\") {
        Some(stripped) => PathBuf::from(stripped),
        None => resolved,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn old_global_config_gets_worktree_defaults() {
        let config: UserConfig =
            serde_json::from_str(r#"{"yolo":true,"model":"gpt-5","reasoning_effort":"high"}"#)
                .unwrap();

        assert!(config.yolo);
        assert!(config.favorites.is_empty());
        assert!(config.hidden_title_prefixes.is_empty());
        assert!(config.hidden_path_prefixes.is_empty());
        assert_eq!(config.github_comment_filter, GithubCommentFilter::All);
        assert_eq!(config.worktree.branch_prefix, DEFAULT_BRANCH_PREFIX);
        assert_eq!(config.worktree.root, default_worktree_root());
        assert!(config.terminal.shell.is_none());
    }

    #[test]
    fn github_comment_filter_round_trips_and_the_default_stays_sparse() {
        let default_json = serde_json::to_value(UserConfig::default()).unwrap();
        assert!(default_json.get("github_comment_filter").is_none());

        let config = UserConfig {
            github_comment_filter: GithubCommentFilter::Unresolved,
            ..UserConfig::default()
        };
        let json = serde_json::to_string(&config).unwrap();
        assert!(json.contains(r#""github_comment_filter":"unresolved""#));

        let loaded: UserConfig = serde_json::from_str(&json).unwrap();
        assert_eq!(
            loaded.github_comment_filter,
            GithubCommentFilter::Unresolved
        );
    }

    #[test]
    fn favorites_round_trip_in_global_config() {
        let mut config = UserConfig::default();
        config.favorites.push("session-b".to_string());
        config.favorites.push("session-a".to_string());

        let json = serde_json::to_string(&config).unwrap();
        let loaded: UserConfig = serde_json::from_str(&json).unwrap();

        // Order is the point, so round-tripping must not sort them.
        assert_eq!(loaded.favorites, vec!["session-b", "session-a"]);
    }

    #[test]
    fn favorites_written_by_older_versions_still_load() {
        // Older releases stored a set, which serialised to the same JSON array.
        let loaded: UserConfig =
            serde_json::from_str(r#"{"favorites":["session-a","session-b"]}"#).unwrap();

        assert_eq!(loaded.favorites, vec!["session-a", "session-b"]);
    }

    #[test]
    fn hand_edited_duplicate_favorites_are_dropped() {
        let loaded: UserConfig =
            serde_json::from_str(r#"{"favorites":["a","b","a","b","c"]}"#).unwrap();

        assert_eq!(loaded.favorites, vec!["a", "b", "c"]);
    }

    #[test]
    fn hidden_title_prefixes_are_trimmed_and_deduplicated_case_insensitively() {
        let loaded: UserConfig = serde_json::from_str(
            r#"{"hidden_title_prefixes":[" Your objective: ","","your OBJECTIVE:","CLIO:"]}"#,
        )
        .unwrap();

        assert_eq!(
            loaded.hidden_title_prefixes,
            vec!["Your objective:", "CLIO:"]
        );
        assert_eq!(
            parse_title_prefixes(" Your objective: , CLIO: , clio: "),
            vec!["Your objective:", "CLIO:"]
        );
    }

    #[test]
    fn hidden_path_prefixes_are_trimmed_and_deduplicated() {
        let loaded: UserConfig =
            serde_json::from_str(r#"{"hidden_path_prefixes":[" /tmp ","","/tmp","/var/tmp"]}"#)
                .unwrap();

        assert_eq!(loaded.hidden_path_prefixes, vec!["/tmp", "/var/tmp"]);
        assert_eq!(
            parse_path_prefixes(" /tmp, /tmp, /var/tmp "),
            vec!["/tmp", "/var/tmp"]
        );
    }

    #[test]
    fn terminal_shell_round_trips_in_global_config() {
        let mut config = UserConfig::default();
        config.terminal.shell = Some("pwsh".to_string());

        let json = serde_json::to_string(&config).unwrap();
        let loaded: UserConfig = serde_json::from_str(&json).unwrap();

        assert_eq!(loaded.terminal.shell.as_deref(), Some("pwsh"));
    }

    #[test]
    fn global_config_round_trip_preserves_future_fields() {
        let json = r#"{"yolo":true,"future_setting":{"enabled":7}}"#;
        let loaded: UserConfig = serde_json::from_str(json).unwrap();
        let saved: Value = serde_json::to_value(loaded).unwrap();

        assert_eq!(saved["future_setting"]["enabled"], 7);
    }

    #[test]
    fn config_revision_detects_atomic_replacement_even_at_the_same_path() {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("config.json");
        write_json_atomic(&path, &serde_json::json!({"model": "first"})).unwrap();
        let first = config_revision(&path).unwrap();

        write_json_atomic(&path, &serde_json::json!({"model": "second"})).unwrap();
        let second = config_revision(&path).unwrap();

        assert_ne!(first, second);
    }

    #[test]
    fn compare_and_save_refuses_to_overwrite_a_newer_config() {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("config.json");
        save_to(
            &path,
            &UserConfig {
                model: Some("initial".to_string()),
                ..Default::default()
            },
        )
        .unwrap();
        let expected = config_revision(&path).unwrap();
        save_to(
            &path,
            &UserConfig {
                model: Some("external".to_string()),
                ..Default::default()
            },
        )
        .unwrap();

        let outcome = save_if_revision(
            &path,
            Some(expected),
            &UserConfig {
                model: Some("stale".to_string()),
                ..Default::default()
            },
        )
        .unwrap();

        assert!(matches!(outcome, ConfigSaveOutcome::Changed));
        assert_eq!(
            load_existing_base_config(&path)
                .unwrap()
                .unwrap()
                .model
                .as_deref(),
            Some("external")
        );
    }

    #[test]
    fn config_writers_share_a_cross_process_lock() {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("config.json");
        let _held = ConfigLock::acquire(&path).unwrap();

        let error = save_to(&path, &UserConfig::default())
            .unwrap_err()
            .to_string();

        assert!(error.contains("Another CST instance"));
    }

    #[test]
    fn notification_defaults_are_disabled_and_private() {
        let config: UserConfig = serde_json::from_str("{}").unwrap();

        assert_eq!(config.theme, crate::theme::ThemeName::Classic);
        assert!(!config.notifications.enabled);
        assert_eq!(config.notifications.server, DEFAULT_NTFY_SERVER);
        assert!(config.notifications.topic.is_empty());
        assert!(config.ntfy_access_token.is_empty());
        assert!(!config.ntfy_verbose);
        assert!(config.notifications.ready);
        assert!(config.notifications.error);
    }

    #[test]
    fn older_root_flattening_preserves_new_ntfy_credentials() {
        #[derive(Serialize, Deserialize)]
        struct OlderConfig {
            #[serde(default)]
            notifications: NotificationConfig,
            #[serde(flatten)]
            extra: BTreeMap<String, Value>,
        }

        let current = UserConfig {
            theme: crate::theme::ThemeName::SolarizedLight,
            ntfy_access_token: "tk_private_token".to_string(),
            ntfy_verbose: true,
            ..UserConfig::default()
        };
        let json = serde_json::to_string(&current).unwrap();
        let old: OlderConfig = serde_json::from_str(&json).unwrap();
        let rewritten = serde_json::to_string(&old).unwrap();
        let restored: UserConfig = serde_json::from_str(&rewritten).unwrap();

        assert_eq!(restored.ntfy_access_token, "tk_private_token");
        assert!(restored.ntfy_verbose);
        assert_eq!(restored.theme, crate::theme::ThemeName::SolarizedLight);
    }

    #[test]
    fn ntfy_configuration_validates_server_and_topic() {
        let mut notifications = NotificationConfig {
            enabled: true,
            topic: "private_CST-topic_123".to_string(),
            server: "https://ntfy.example.test/".to_string(),
            ..NotificationConfig::default()
        };
        assert!(validate_notification_config(&notifications).is_ok());
        assert_eq!(
            normalize_ntfy_server(&notifications.server).unwrap(),
            "https://ntfy.example.test"
        );

        notifications.topic = "not/a/topic".to_string();
        assert!(validate_notification_config(&notifications).is_err());
        notifications.topic = "valid".to_string();
        notifications.server = "file:///tmp/ntfy".to_string();
        assert!(validate_notification_config(&notifications).is_err());
        for invalid in ["https://not a host", "https://:443", "https://host:bad"] {
            notifications.server = invalid.to_string();
            assert!(
                validate_notification_config(&notifications).is_err(),
                "accepted {invalid:?}"
            );
        }
        notifications.server = "https://ntfy.example.test/unintended-topic".to_string();
        assert!(
            validate_notification_config(&notifications).is_err(),
            "a server path would route JSON to the wrong ntfy topic"
        );

        notifications.enabled = false;
        notifications.server.clear();
        notifications.topic.clear();
        assert!(
            validate_notification_config(&notifications).is_ok(),
            "disabled integration must not block unrelated settings saves"
        );

        let mut config = UserConfig {
            notifications: NotificationConfig {
                enabled: true,
                server: "https://ntfy.example.test".to_string(),
                topic: "private_topic".to_string(),
                ..NotificationConfig::default()
            },
            ntfy_access_token: "token with spaces".to_string(),
            ..UserConfig::default()
        };
        assert!(validate_user_notification_config(&config).is_err());
        config.ntfy_access_token = "tk_valid-token_123".to_string();
        assert!(validate_user_notification_config(&config).is_ok());
    }

    #[test]
    fn tmux_shortcuts_are_single_distinct_printable_characters() {
        assert!(validate_tmux_keys(&TmuxKeyConfig::default()).is_ok());

        let duplicate = TmuxKeyConfig {
            resume_session: "t".to_string(),
            new_session: "z".to_string(),
            new_worktree: "z".to_string(),
            end_session: "X".to_string(),
        };
        assert!(validate_tmux_keys(&duplicate).is_err());

        let invalid = TmuxKeyConfig {
            new_session: "mm".to_string(),
            ..TmuxKeyConfig::default()
        };
        assert!(validate_tmux_keys(&invalid).is_err());
    }

    #[test]
    fn project_overrides_layer_over_global_values() {
        let temp = tempfile::tempdir().unwrap();
        fs::write(
            temp.path().join(".cst.json"),
            r#"{"worktree":{"branch_prefix":"task/","root":"local-wt"}}"#,
        )
        .unwrap();
        let mut global = UserConfig::default();
        global.worktree.branch_prefix = "global/".to_string();
        global.worktree.root = temp.path().join("global-wt");

        let settings = ProjectSettings::load(temp.path(), &global).unwrap();
        assert_eq!(settings.effective_branch_prefix(), "task/");
        assert_eq!(
            settings.effective_root(),
            canonicalize_or_absolute(temp.path()).join("local-wt")
        );
    }

    #[test]
    fn inherited_transitions_remove_empty_project_file() {
        let temp = tempfile::tempdir().unwrap();
        let mut settings = ProjectSettings::load(temp.path(), &UserConfig::default()).unwrap();
        settings.set_branch_prefix_override(Some("feature/".to_string()));
        settings.save().unwrap();
        assert!(temp.path().join(".cst.json").exists());

        settings.set_branch_prefix_override(None);
        settings.save().unwrap();
        assert!(!temp.path().join(".cst.json").exists());
    }

    #[test]
    fn project_save_preserves_other_fields() {
        let temp = tempfile::tempdir().unwrap();
        fs::write(
            temp.path().join(".cst.json"),
            r#"{"future_setting":true,"worktree":{"future_field":7}}"#,
        )
        .unwrap();
        let mut settings = ProjectSettings::load(temp.path(), &UserConfig::default()).unwrap();
        settings.set_branch_prefix_override(Some("feature/".to_string()));
        settings.save().unwrap();

        let saved: Value =
            serde_json::from_str(&fs::read_to_string(temp.path().join(".cst.json")).unwrap())
                .unwrap();
        assert_eq!(saved["future_setting"], true);
        assert_eq!(saved["worktree"]["future_field"], 7);
        assert_eq!(saved["worktree"]["branch_prefix"], "feature/");
    }

    #[test]
    fn global_and_project_snippets_round_trip_without_losing_unknown_fields() {
        let global = UserConfig {
            snippets: vec![PromptSnippet {
                name: "Review".to_string(),
                prompt: "Review this carefully.".to_string(),
            }],
            ..UserConfig::default()
        };
        let json = serde_json::to_string(&global).unwrap();
        let loaded: UserConfig = serde_json::from_str(&json).unwrap();
        assert_eq!(loaded.snippets, global.snippets);

        let temp = tempfile::tempdir().unwrap();
        fs::write(temp.path().join(".cst.json"), r#"{"future_setting":true}"#).unwrap();
        let mut settings = ProjectSettings::load(temp.path(), &global).unwrap();
        settings.set_snippets(vec![PromptSnippet {
            name: "Project plan".to_string(),
            prompt: "Use this repository's plan.".to_string(),
        }]);
        settings.save().unwrap();

        let reloaded = ProjectSettings::load(temp.path(), &global).unwrap();
        assert_eq!(reloaded.snippets()[0].name, "Project plan");
        let saved: Value =
            serde_json::from_str(&fs::read_to_string(temp.path().join(".cst.json")).unwrap())
                .unwrap();
        assert_eq!(saved["future_setting"], true);
    }

    #[test]
    fn legacy_global_snippets_migrate_to_an_authoritative_sidecar() {
        let temp = tempfile::tempdir().unwrap();
        let config_path = temp.path().join("config.json");
        let snippets_path = temp.path().join("snippets.json");
        let snippet = PromptSnippet {
            name: "Quick check in".to_string(),
            prompt: "Anything else to work on?".to_string(),
        };
        fs::write(
            &config_path,
            serde_json::to_vec(&UserConfig {
                snippets: vec![snippet.clone()],
                ..UserConfig::default()
            })
            .unwrap(),
        )
        .unwrap();

        let migrated = load_checked_in(&config_path, &snippets_path).unwrap();
        assert_eq!(migrated.snippets, vec![snippet.clone()]);
        assert!(snippets_path.exists());

        // Simulate a long-running old CST rewriting config.json with no snippet field.
        fs::write(
            &config_path,
            serde_json::to_vec(&UserConfig::default()).unwrap(),
        )
        .unwrap();
        let protected = load_checked_in(&config_path, &snippets_path).unwrap();
        assert_eq!(protected.snippets, vec![snippet]);
    }

    #[test]
    fn sidecar_parse_errors_are_actionable_instead_of_silently_empty() {
        let temp = tempfile::tempdir().unwrap();
        let config_path = temp.path().join("config.json");
        let snippets_path = temp.path().join("snippets.json");
        fs::write(&config_path, r#"{"yolo":true,"future_setting":7}"#).unwrap();
        fs::write(&snippets_path, "{invalid").unwrap();

        let error = load_checked_in(&config_path, &snippets_path)
            .unwrap_err()
            .to_string();

        assert!(error.contains("Invalid global snippets"));
        assert!(error.contains("snippets.json"));

        let preserved = load_base_config(&config_path).unwrap();
        assert!(preserved.yolo);
        assert_eq!(preserved.extra["future_setting"], 7);
    }

    #[test]
    fn sidecar_compare_and_write_rejects_a_stale_second_editor() {
        let temp = tempfile::tempdir().unwrap();
        let config_path = temp.path().join("config.json");
        let snippets_path = temp.path().join("snippets.json");
        let lock_path = temp.path().join("snippets.lock");
        let original = PromptSnippet {
            name: "Original".to_string(),
            prompt: "old".to_string(),
        };
        let first_update = PromptSnippet {
            name: "Original".to_string(),
            prompt: "first".to_string(),
        };
        fs::write(
            &config_path,
            serde_json::to_vec(&UserConfig {
                yolo: true,
                snippets: vec![original.clone()],
                ..UserConfig::default()
            })
            .unwrap(),
        )
        .unwrap();
        write_json_atomic(&snippets_path, std::slice::from_ref(&original)).unwrap();

        let saved = save_global_snippets_if_unchanged_in(
            &config_path,
            &snippets_path,
            &lock_path,
            std::slice::from_ref(&original),
            std::slice::from_ref(&first_update),
        )
        .unwrap();
        assert!(saved.yolo, "fresh unrelated settings are retained");
        let compatibility: UserConfig =
            serde_json::from_str(&fs::read_to_string(&config_path).unwrap()).unwrap();
        assert_eq!(
            compatibility.snippets,
            vec![original.clone()],
            "sidecar edits never rewrite unrelated config.json snapshots"
        );

        let error = save_global_snippets_if_unchanged_in(
            &config_path,
            &snippets_path,
            &lock_path,
            std::slice::from_ref(&original),
            &[PromptSnippet {
                name: "Original".to_string(),
                prompt: "second".to_string(),
            }],
        )
        .unwrap_err()
        .to_string();
        assert!(error.contains("changed on disk"));
        let current: Vec<PromptSnippet> =
            serde_json::from_str(&fs::read_to_string(snippets_path).unwrap()).unwrap();
        assert_eq!(current, vec![first_update]);
    }

    #[test]
    fn migration_cannot_race_a_locked_snippet_writer() {
        let temp = tempfile::tempdir().unwrap();
        let config_path = temp.path().join("config.json");
        let snippets_path = temp.path().join("snippets.json");
        let lock_path = temp.path().join("snippets.lock");
        let legacy = PromptSnippet {
            name: "Legacy".to_string(),
            prompt: "migrate me".to_string(),
        };
        fs::write(
            &config_path,
            serde_json::to_vec(&UserConfig {
                snippets: vec![legacy.clone()],
                ..UserConfig::default()
            })
            .unwrap(),
        )
        .unwrap();
        let held = SnippetLock::acquire(&lock_path).unwrap();

        let error = load_checked_in(&config_path, &snippets_path)
            .unwrap_err()
            .to_string();
        assert!(error.contains("saving global snippets"));
        assert!(
            !snippets_path.exists(),
            "an unlocked migrator must not write while the lock is held"
        );

        drop(held);
        assert_eq!(
            load_checked_in(&config_path, &snippets_path)
                .unwrap()
                .snippets,
            vec![legacy]
        );
    }

    #[test]
    fn malformed_project_config_is_actionable_and_unchanged() {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join(".cst.json");
        fs::write(&path, "{invalid").unwrap();

        let error = ProjectSettings::load(temp.path(), &UserConfig::default())
            .unwrap_err()
            .to_string();
        assert!(error.contains("Invalid project settings"));
        assert_eq!(fs::read_to_string(path).unwrap(), "{invalid");
    }
    #[test]
    fn a_project_can_override_yolo_in_either_direction_or_stay_out_of_it() {
        let temp = tempfile::tempdir().unwrap();
        let yolo_global = UserConfig {
            yolo: true,
            ..UserConfig::default()
        };
        let careful_global = UserConfig::default();

        // Nothing said: whatever the user's own setting is.
        let inherited = ProjectSettings::load(temp.path(), &yolo_global).unwrap();
        assert_eq!(inherited.yolo_override(), None);
        assert!(inherited.effective_yolo());
        assert!(!ProjectSettings::load(temp.path(), &careful_global)
            .unwrap()
            .effective_yolo());

        // A repository that wants the prompts back on can say so, and it wins over a
        // global yolo — the direction a two-state opt-in could not express.
        let mut settings = ProjectSettings::load(temp.path(), &yolo_global).unwrap();
        settings.set_yolo_override(Some(false));
        settings.save().unwrap();
        let reloaded = ProjectSettings::load(temp.path(), &yolo_global).unwrap();
        assert_eq!(reloaded.yolo_override(), Some(false));
        assert!(!reloaded.effective_yolo());

        // And the other way round.
        let mut settings = ProjectSettings::load(temp.path(), &careful_global).unwrap();
        settings.set_yolo_override(Some(true));
        settings.save().unwrap();
        assert!(ProjectSettings::load(temp.path(), &careful_global)
            .unwrap()
            .effective_yolo());
    }

    #[test]
    fn clearing_the_yolo_override_leaves_the_key_out_of_the_file_entirely() {
        let temp = tempfile::tempdir().unwrap();
        let global = UserConfig::default();
        let mut settings = ProjectSettings::load(temp.path(), &global).unwrap();

        settings.set_yolo_override(Some(true));
        settings.save().unwrap();
        assert!(fs::read_to_string(temp.path().join(".cst.json"))
            .unwrap()
            .contains("yolo"));

        settings.set_yolo_override(None);
        settings.save().unwrap();
        // Absent rather than `false`: the two mean different things, and writing one
        // for the other would pin the repository to a setting nobody chose. With
        // nothing else in it the file goes altogether, which says the same thing.
        let left_behind = fs::read_to_string(temp.path().join(".cst.json")).unwrap_or_default();
        assert!(!left_behind.contains("yolo"), "got: {left_behind}");
        assert_eq!(
            ProjectSettings::load(temp.path(), &global)
                .unwrap()
                .yolo_override(),
            None
        );
    }
    #[test]
    fn a_project_autopilot_cap_round_trips_and_clears_back_to_inheriting() {
        let temp = tempfile::tempdir().unwrap();
        let global = UserConfig {
            max_autopilot_continues: Some(10),
            ..UserConfig::default()
        };

        let inherited = ProjectSettings::load(temp.path(), &global).unwrap();
        assert_eq!(inherited.max_autopilot_continues_override(), None);
        assert_eq!(inherited.effective_max_autopilot_continues(), Some(10));

        let mut settings = ProjectSettings::load(temp.path(), &global).unwrap();
        settings.set_max_autopilot_continues_override(Some(2));
        settings.save().unwrap();
        let reloaded = ProjectSettings::load(temp.path(), &global).unwrap();
        assert_eq!(reloaded.max_autopilot_continues_override(), Some(2));
        assert_eq!(reloaded.effective_max_autopilot_continues(), Some(2));

        let mut settings = ProjectSettings::load(temp.path(), &global).unwrap();
        settings.set_max_autopilot_continues_override(None);
        settings.save().unwrap();
        assert_eq!(
            ProjectSettings::load(temp.path(), &global)
                .unwrap()
                .effective_max_autopilot_continues(),
            Some(10),
            "clearing the override goes back to inheriting, not to nothing"
        );
    }

    #[test]
    fn a_project_whose_only_setting_is_the_autopilot_cap_still_gets_a_file() {
        // `is_empty` decides whether the file is written or deleted, so a field missing
        // from it is thrown away on the next save.
        let temp = tempfile::tempdir().unwrap();
        let global = UserConfig::default();
        let mut settings = ProjectSettings::load(temp.path(), &global).unwrap();

        settings.set_max_autopilot_continues_override(Some(3));
        settings.save().unwrap();

        assert!(temp.path().join(".cst.json").exists());
        assert_eq!(
            ProjectSettings::load(temp.path(), &global)
                .unwrap()
                .max_autopilot_continues_override(),
            Some(3)
        );
    }

    #[test]
    fn a_project_that_only_opts_out_of_thread_wakes_still_gets_a_file() {
        // Same hazard as the autopilot cap above: a field missing from `is_empty` makes
        // the file look empty, and a project whose only setting is this one would have
        // its opt-out deleted on the next save.
        let temp = tempfile::tempdir().unwrap();
        let global = UserConfig::default();
        let mut settings = ProjectSettings::load(temp.path(), &global).unwrap();

        settings.set_threads_enabled_override(Some(false));
        settings.save().unwrap();

        assert!(temp.path().join(".cst.json").exists());
        let reloaded = ProjectSettings::load(temp.path(), &global).unwrap();
        assert_eq!(reloaded.threads_enabled_override(), Some(false));
        assert!(
            !reloaded.effective_threads_enabled(),
            "a repository must be able to opt out even when the global default is on"
        );
    }

    #[test]
    fn a_repository_with_no_opinion_follows_the_global_thread_setting() {
        let temp = tempfile::tempdir().unwrap();
        let global = UserConfig {
            threads_enabled: false,
            ..Default::default()
        };

        let settings = ProjectSettings::load(temp.path(), &global).unwrap();

        assert_eq!(settings.threads_enabled_override(), None);
        assert!(!settings.effective_threads_enabled());
    }

    #[test]
    fn an_unknown_global_setting_survives_a_save_alongside_the_autopilot_cap() {
        let config: UserConfig =
            serde_json::from_str(r#"{"max_autopilot_continues":7,"future_setting":true}"#).unwrap();
        assert_eq!(config.max_autopilot_continues, Some(7));

        let written = serde_json::to_string(&config).unwrap();
        assert!(
            written.contains("\"max_autopilot_continues\":7"),
            "{written}"
        );
        assert!(written.contains("future_setting"), "{written}");
    }
}

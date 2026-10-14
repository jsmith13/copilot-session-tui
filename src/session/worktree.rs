use crate::config::EffectiveWorktreeConfig;
use anyhow::{bail, Context, Result};
use fs4::{FileExt, TryLockError};
use ignore::gitignore::GitignoreBuilder;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::collections::{HashMap, HashSet};
use std::ffi::OsStr;
use std::fs::{self, File, OpenOptions};
use std::io::{Read, Write};
use std::path::{Component, Path, PathBuf};
use std::process::{Command, Output};
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::thread;
use std::time::{Duration, Instant};
use uuid::Uuid;

const REGISTRY_VERSION: u32 = 2;
const MAX_DERIVED_PATH_CHARS: usize = 240;
const REGISTRY_LOCK_TIMEOUT: Duration = Duration::from_secs(5);
const REGISTRY_LOCK_RETRY: Duration = Duration::from_millis(25);
const IDENTITY_FILE: &str = "cst-worktree.json";
const ZERO_OID: &str = "0000000000000000000000000000000000000000";

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct ManagedWorktree {
    pub path: PathBuf,
    pub source_repository: PathBuf,
    pub common_git_dir: PathBuf,
    pub branch: String,
    identity: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    include_patterns: Option<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    included_files: Vec<IncludedFileState>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
struct IncludedFileState {
    path: PathBuf,
    sha256: String,
}

#[derive(Debug, Clone, Default)]
struct WorktreeIncludeState {
    patterns: Option<String>,
    files: Vec<IncludedFileState>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
struct WorktreeReservation {
    identity: String,
    path: PathBuf,
    source_repository: PathBuf,
    common_git_dir: PathBuf,
    branch: String,
    base_oid: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
struct WorktreeIdentity {
    identity: String,
    source_repository: PathBuf,
    common_git_dir: PathBuf,
    branch: String,
}

#[derive(Debug)]
pub struct CreatedWorktree {
    pub entry: ManagedWorktree,
    pub notice: Option<String>,
}

/// How far a worktree creation has got, readable from another thread while it runs.
///
/// Exists because the slow part is unbounded and silent: a `.worktreeinclude` naming a
/// virtualenv copies tens of thousands of files, and a wait of an hour with no count
/// looks exactly like a hang.
#[derive(Debug, Default)]
pub struct CreationProgress {
    /// Listing what `.worktreeinclude` selects has begun. Its own stage because it
    /// walks every ignored file in the source, which can take far longer than the
    /// checkout before it.
    finding: AtomicBool,
    /// Files `.worktreeinclude` selected; zero until they have been listed.
    to_copy: AtomicUsize,
    copied: AtomicUsize,
    cancelled: AtomicBool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CreationStage {
    CheckingOut,
    FindingIncludes,
    CopyingIncludes { copied: usize, total: usize },
}

impl CreationProgress {
    /// Stop copying at the next file, so the creation fails and rolls itself back.
    pub fn cancel(&self) {
        self.cancelled.store(true, Ordering::Release);
    }

    pub fn is_cancelled(&self) -> bool {
        self.cancelled.load(Ordering::Acquire)
    }

    pub(crate) fn plan_copies(&self, total: usize) {
        self.to_copy.store(total, Ordering::Release);
    }

    pub(crate) fn note_copied(&self, copied: usize) {
        self.copied.store(copied, Ordering::Release);
    }

    pub fn stage(&self) -> CreationStage {
        match self.to_copy.load(Ordering::Acquire) {
            0 if self.finding.load(Ordering::Acquire) => CreationStage::FindingIncludes,
            0 => CreationStage::CheckingOut,
            total => CreationStage::CopyingIncludes {
                copied: self.copied.load(Ordering::Acquire),
                total,
            },
        }
    }
}

#[derive(Debug)]
pub struct RemovalOutcome {
    pub branch_removed: bool,
    pub branch_notice: Option<String>,
}

#[derive(Debug)]
struct Repository {
    main_worktree: PathBuf,
    common_git_dir: PathBuf,
}

#[derive(Debug, Default, Serialize, Deserialize)]
struct WorktreeRegistry {
    #[serde(default = "registry_version")]
    version: u32,
    #[serde(default)]
    worktrees: Vec<ManagedWorktree>,
    #[serde(default)]
    reservations: Vec<WorktreeReservation>,
}

struct RegistryLock {
    _file: File,
}

pub fn registry_path() -> PathBuf {
    dirs::data_local_dir()
        .unwrap_or_else(|| {
            dirs::home_dir()
                .unwrap_or_else(|| PathBuf::from("."))
                .join(".local")
                .join("share")
        })
        .join("cst")
        .join("worktrees.json")
}

pub fn resolve_repository_root(path: &Path) -> Result<PathBuf> {
    Ok(resolve_repository(path)?.main_worktree)
}

pub fn validate_branch(repository: &Path, branch: &str) -> Result<()> {
    validate_branch_syntax(branch)?;
    let repository = resolve_repository(repository)?;
    reject_branch_collision(&repository.main_worktree, branch)
}

pub fn validate_branch_prefix(prefix: &str) -> Result<()> {
    validate_branch_syntax(&format!("{prefix}cst-test"))
        .context("Branch prefix would produce an invalid Git branch")
}

fn validate_branch_syntax(branch: &str) -> Result<()> {
    if branch.trim() != branch || branch.is_empty() {
        bail!("Branch name cannot be empty or have leading/trailing whitespace");
    }
    let output = Command::new("git")
        .args(["check-ref-format", "--branch", branch])
        .output()
        .context("Failed to run Git branch validation")?;
    if !output.status.success() {
        bail!(
            "Invalid branch name '{}': {}",
            branch,
            command_error(&output)
        );
    }
    Ok(())
}

pub fn create_managed_worktree(
    project: &Path,
    branch: &str,
    config: &EffectiveWorktreeConfig,
) -> Result<CreatedWorktree> {
    create_managed_worktree_reporting(project, branch, config, &CreationProgress::default())
}

/// [`create_managed_worktree`], counting its progress into `progress` as it goes.
pub fn create_managed_worktree_reporting(
    project: &Path,
    branch: &str,
    config: &EffectiveWorktreeConfig,
    progress: &CreationProgress,
) -> Result<CreatedWorktree> {
    create_managed_worktree_with(
        project,
        branch,
        config,
        &registry_path(),
        |source, worktree| copy_worktree_includes_reporting(source, worktree, progress),
    )
}

pub fn managed_worktree_for_cwd(cwd: &Path) -> Result<Option<ManagedWorktree>> {
    managed_worktree_for_cwd_in(cwd, &registry_path())
}

pub fn is_dirty(entry: &ManagedWorktree) -> Result<bool> {
    if !entry.path.exists() {
        bail!(
            "Managed worktree no longer exists: {}",
            entry.path.display()
        );
    }
    let output = git_output(
        &entry.path,
        ["status", "--porcelain=v1", "--untracked-files=all"],
    )?;
    if !output.status.success() {
        bail!("Failed to inspect worktree: {}", command_error(&output));
    }
    if !output.stdout.is_empty() {
        return Ok(true);
    }

    included_files_are_dirty(entry)
}

pub fn remove_managed_worktree(entry: &ManagedWorktree, force: bool) -> Result<RemovalOutcome> {
    remove_managed_worktree_in(entry, force, &registry_path())
}

pub fn unregister(entry: &ManagedWorktree) -> Result<()> {
    unregister_in(entry, &registry_path())
}

pub fn rollback_created_worktree(entry: &ManagedWorktree) -> Result<()> {
    rollback_registered_worktree_in(entry, &registry_path())
}

pub fn generated_branch_name(prefix: &str) -> String {
    format!("{}{}", prefix, chrono::Local::now().format("%Y%m%d-%H%M%S"))
}

fn create_managed_worktree_with<F>(
    project: &Path,
    branch: &str,
    config: &EffectiveWorktreeConfig,
    registry_path: &Path,
    initialize: F,
) -> Result<CreatedWorktree>
where
    F: Fn(&Path, &Path) -> Result<WorktreeIncludeState>,
{
    let repository = resolve_repository(project)?;
    validate_branch(&repository.main_worktree, branch)?;

    let path = derive_worktree_path(
        &config.root,
        &repository.main_worktree,
        &repository.common_git_dir,
        branch,
    )?;
    let (base, notice) = resolve_base(&repository.main_worktree)?;
    let reservation = WorktreeReservation {
        identity: Uuid::new_v4().to_string(),
        path: canonicalize_or_absolute(&path),
        source_repository: repository.main_worktree,
        common_git_dir: repository.common_git_dir,
        branch: branch.to_string(),
        base_oid: base,
    };
    reserve_creation_in(&reservation, registry_path)?;

    if let Some(parent) = path.parent() {
        if let Err(error) = fs::create_dir_all(parent)
            .with_context(|| format!("Failed to create worktree root: {}", parent.display()))
        {
            return Err(with_reservation_release_error(
                error,
                &reservation,
                registry_path,
            ));
        }
    }

    let branch_ref = format!("refs/heads/{branch}");
    let branch_output = match git_output(
        &reservation.source_repository,
        [
            OsStr::new("update-ref"),
            OsStr::new(&branch_ref),
            OsStr::new(&reservation.base_oid),
            OsStr::new(ZERO_OID),
        ],
    ) {
        Ok(output) => output,
        Err(error) => {
            return Err(with_reservation_release_error(
                error,
                &reservation,
                registry_path,
            ));
        }
    };
    if !branch_output.status.success() {
        let error = anyhow::anyhow!(
            "Failed to reserve branch '{}': {}",
            branch,
            command_error(&branch_output)
        );
        return Err(with_reservation_release_error(
            error,
            &reservation,
            registry_path,
        ));
    }

    let add_output = match git_output(
        &reservation.source_repository,
        [
            OsStr::new("worktree"),
            OsStr::new("add"),
            OsStr::new("--no-checkout"),
            reservation.path.as_os_str(),
            OsStr::new(&reservation.branch),
        ],
    ) {
        Ok(output) => output,
        Err(error) => {
            return Err(with_creation_rollback_error(
                error,
                &reservation,
                registry_path,
            ));
        }
    };
    if !add_output.status.success() {
        let error = anyhow::anyhow!("Failed to create worktree: {}", command_error(&add_output));
        return Err(with_creation_rollback_error(
            error,
            &reservation,
            registry_path,
        ));
    }

    if let Err(error) = write_worktree_identity(&reservation) {
        return Err(with_creation_rollback_error(
            error.context("Failed to persist worktree ownership identity"),
            &reservation,
            registry_path,
        ));
    }

    let checkout = match git_output(&reservation.path, ["reset", "--hard", "HEAD"]) {
        Ok(output) => output,
        Err(error) => {
            return Err(with_creation_rollback_error(
                error,
                &reservation,
                registry_path,
            ));
        }
    };
    if !checkout.status.success() {
        let error = anyhow::anyhow!("Failed to populate worktree: {}", command_error(&checkout));
        return Err(with_creation_rollback_error(
            error,
            &reservation,
            registry_path,
        ));
    }

    let included = match initialize(&reservation.source_repository, &reservation.path) {
        Ok(included) => included,
        Err(error) => {
            return Err(with_creation_rollback_error(
                error.context("Worktree initialization failed"),
                &reservation,
                registry_path,
            ));
        }
    };

    let entry = ManagedWorktree {
        path: reservation.path.clone(),
        source_repository: reservation.source_repository.clone(),
        common_git_dir: reservation.common_git_dir.clone(),
        branch: reservation.branch.clone(),
        identity: reservation.identity.clone(),
        include_patterns: included.patterns,
        included_files: included.files,
    };

    if let Err(error) = finalize_creation_in(&reservation, &entry, registry_path) {
        return Err(with_creation_rollback_error(
            error.context("Failed to register worktree"),
            &reservation,
            registry_path,
        ));
    }

    Ok(CreatedWorktree { entry, notice })
}

fn resolve_repository(path: &Path) -> Result<Repository> {
    let top_level = git_stdout(path, ["rev-parse", "--show-toplevel"])
        .with_context(|| format!("{} is not a Git worktree", path.display()))?;
    let current_top = canonicalize_or_absolute(Path::new(&top_level));

    let common = git_stdout(path, ["rev-parse", "--git-common-dir"])?;
    let common_path = PathBuf::from(common);
    let resolved_common = if common_path.is_absolute() {
        common_path
    } else {
        current_top.join(common_path)
    };
    let common_git_dir = canonicalize_or_absolute(&resolved_common);

    let list = git_stdout(path, ["worktree", "list", "--porcelain"])?;
    let main = list
        .lines()
        .find_map(|line| line.strip_prefix("worktree "))
        .map(PathBuf::from)
        .unwrap_or(current_top);

    Ok(Repository {
        main_worktree: canonicalize_or_absolute(&main),
        common_git_dir,
    })
}

fn resolve_base(repository: &Path) -> Result<(String, Option<String>)> {
    let remote = git_output(
        repository,
        [
            "rev-parse",
            "--verify",
            "--quiet",
            "refs/remotes/origin/HEAD^{commit}",
        ],
    )?;
    if remote.status.success() {
        return Ok((
            String::from_utf8_lossy(&remote.stdout).trim().to_string(),
            None,
        ));
    }

    let head = git_stdout(repository, ["rev-parse", "--verify", "HEAD^{commit}"])
        .context("Unable to resolve either cached origin/HEAD or current HEAD")?;
    Ok((
        head,
        Some(
            "Cached origin/HEAD was unavailable; the worktree was based on the project's current HEAD"
                .to_string(),
        ),
    ))
}

fn reject_branch_collision(repository: &Path, branch: &str) -> Result<()> {
    if branch_exists(repository, branch)? {
        bail!("Branch already exists: {branch}");
    }
    Ok(())
}

fn branch_exists(repository: &Path, branch: &str) -> Result<bool> {
    let reference = format!("refs/heads/{branch}");
    let output = git_output(
        repository,
        ["show-ref", "--verify", "--quiet", reference.as_str()],
    )?;
    if output.status.success() {
        return Ok(true);
    }
    if output.status.code() != Some(1) {
        bail!(
            "Failed to check whether branch '{}' exists: {}",
            branch,
            command_error(&output)
        );
    }
    Ok(false)
}

fn reject_path_collision(
    repository: &Path,
    path: &Path,
    registry: &WorktreeRegistry,
) -> Result<()> {
    if path.exists() {
        bail!("Worktree path already exists: {}", path.display());
    }

    let target_key = path_key(path);
    let worktrees = git_stdout(repository, ["worktree", "list", "--porcelain"])?;
    for listed in worktrees
        .lines()
        .filter_map(|line| line.strip_prefix("worktree "))
    {
        if path_key(Path::new(listed)) == target_key {
            bail!("Git already has a worktree at {}", path.display());
        }
    }

    if registry
        .worktrees
        .iter()
        .any(|entry| path_key(&entry.path) == target_key)
        || registry
            .reservations
            .iter()
            .any(|entry| path_key(&entry.path) == target_key)
    {
        bail!("The worktree registry already contains {}", path.display());
    }
    Ok(())
}

fn derive_worktree_path(
    root: &Path,
    repository: &Path,
    common_git_dir: &Path,
    branch: &str,
) -> Result<PathBuf> {
    if root.as_os_str().is_empty() {
        bail!("Worktree root cannot be empty");
    }

    let repository_name = repository
        .file_name()
        .and_then(OsStr::to_str)
        .unwrap_or("repo");
    let repository_component = format!(
        "{}-{}",
        filesystem_slug(repository_name, 20),
        short_hash(&common_git_dir.to_string_lossy(), 10)
    );
    let branch_component = format!("{}-{}", filesystem_slug(branch, 32), short_hash(branch, 12));
    let path = root.join(repository_component).join(branch_component);

    if path.to_string_lossy().encode_utf16().count() > MAX_DERIVED_PATH_CHARS {
        bail!(
            "Derived worktree path is too long (>{MAX_DERIVED_PATH_CHARS} characters). Configure a shorter worktree root"
        );
    }
    Ok(path)
}

fn filesystem_slug(value: &str, max_len: usize) -> String {
    let mut slug = String::with_capacity(max_len);
    let mut separator = false;
    for character in value.chars() {
        if slug.len() >= max_len {
            break;
        }
        if character.is_ascii_alphanumeric() {
            slug.push(character.to_ascii_lowercase());
            separator = false;
        } else if !separator && !slug.is_empty() {
            slug.push('-');
            separator = true;
        }
    }
    while slug.ends_with('-') {
        slug.pop();
    }
    if slug.is_empty() {
        "item".to_string()
    } else {
        slug
    }
}

fn short_hash(value: &str, length: usize) -> String {
    let digest = Sha256::digest(value.as_bytes());
    format!("{digest:x}")[..length].to_string()
}

#[cfg(test)]
fn copy_worktree_includes(
    source_repository: &Path,
    worktree: &Path,
) -> Result<WorktreeIncludeState> {
    copy_worktree_includes_reporting(source_repository, worktree, &CreationProgress::default())
}

fn copy_worktree_includes_reporting(
    source_repository: &Path,
    worktree: &Path,
    progress: &CreationProgress,
) -> Result<WorktreeIncludeState> {
    let include_path = source_repository.join(".worktreeinclude");
    if !include_path.exists() {
        return Ok(WorktreeIncludeState::default());
    }
    progress.finding.store(true, Ordering::Release);

    let patterns = fs::read_to_string(&include_path).with_context(|| {
        format!(
            "Failed to read .worktreeinclude at {}",
            include_path.display()
        )
    })?;
    let matcher = build_include_matcher(source_repository, &include_path, &patterns)?;

    let output = git_output(
        source_repository,
        [
            "ls-files",
            "--others",
            "--ignored",
            "--exclude-standard",
            "-z",
            "--",
        ],
    )?;
    if !output.status.success() {
        bail!(
            "Failed to enumerate gitignored files: {}",
            command_error(&output)
        );
    }

    let tracked = git_output(worktree, ["ls-files", "-z", "--"])?;
    if !tracked.status.success() {
        bail!(
            "Failed to enumerate tracked target files: {}",
            command_error(&tracked)
        );
    }
    let tracked: HashSet<String> = nul_paths(&tracked.stdout)?
        .into_iter()
        .map(|path| relative_path_key(&path))
        .collect();

    let mut candidates = Vec::new();
    for raw_path in output.stdout.split(|byte| *byte == 0) {
        if raw_path.is_empty() {
            continue;
        }
        let relative_text =
            std::str::from_utf8(raw_path).context("Git returned a non-UTF-8 ignored file path")?;
        let relative = Path::new(relative_text);
        if !is_safe_relative_path(relative) {
            bail!(
                "Git returned an unsafe ignored path: {}",
                relative.display()
            );
        }
        if !matcher
            .matched_path_or_any_parents(relative, false)
            .is_ignore()
        {
            continue;
        }

        let source = source_repository.join(relative);
        if !source.is_file() {
            continue;
        }
        if tracked.contains(&relative_path_key(relative)) {
            bail!(
                "Refusing to overwrite tracked target file from .worktreeinclude: {}",
                relative.display()
            );
        }
        let destination = worktree.join(relative);
        if destination.exists() {
            bail!(
                "Refusing to overwrite existing target file from .worktreeinclude: {}",
                destination.display()
            );
        }
        candidates.push((relative.to_path_buf(), source, destination));
    }

    progress.plan_copies(candidates.len());
    let mut copied = Vec::with_capacity(candidates.len());
    for (relative, source, destination) in candidates {
        if progress.is_cancelled() {
            bail!("Cancelled");
        }
        if let Some(parent) = destination.parent() {
            fs::create_dir_all(parent).with_context(|| {
                format!(
                    "Failed to create included file directory: {}",
                    parent.display()
                )
            })?;
        }
        fs::copy(&source, &destination).with_context(|| {
            format!(
                "Failed to copy included file {} to {}",
                source.display(),
                destination.display()
            )
        })?;
        copied.push(IncludedFileState {
            path: relative,
            sha256: hash_file(&destination)?,
        });
        progress.note_copied(copied.len());
    }
    Ok(WorktreeIncludeState {
        patterns: Some(patterns),
        files: copied,
    })
}

fn build_include_matcher(
    repository: &Path,
    include_path: &Path,
    patterns: &str,
) -> Result<ignore::gitignore::Gitignore> {
    let mut builder = GitignoreBuilder::new(repository);
    for line in patterns.lines() {
        builder
            .add_line(Some(include_path.to_path_buf()), line)
            .with_context(|| {
                format!(
                    "Failed to parse .worktreeinclude at {}",
                    include_path.display()
                )
            })?;
    }
    builder.build().with_context(|| {
        format!(
            "Failed to build .worktreeinclude matcher for {}",
            include_path.display()
        )
    })
}

fn is_safe_relative_path(path: &Path) -> bool {
    !path.is_absolute()
        && path
            .components()
            .all(|component| matches!(component, Component::Normal(_) | Component::CurDir))
}

fn remove_managed_worktree_in(
    entry: &ManagedWorktree,
    force: bool,
    registry_path: &Path,
) -> Result<RemovalOutcome> {
    let _lock = RegistryLock::acquire(registry_path)?;
    let registry = load_registry_unlocked(registry_path)?;
    let registered = registry
        .worktrees
        .iter()
        .find(|registered| path_key(&registered.path) == path_key(&entry.path))
        .cloned()
        .context("Refusing to remove a worktree that is not registered as TUI-managed")?;
    if registered != *entry {
        bail!("Managed worktree metadata changed; reload before deleting");
    }

    let dirty = is_dirty(entry)?;
    if dirty && !force {
        bail!("Worktree has uncommitted changes and requires explicit force confirmation");
    }

    let branch_oid = validate_worktree_identity(entry)?;
    let mut args = vec![OsStr::new("worktree"), OsStr::new("remove")];
    if force {
        args.push(OsStr::new("--force"));
    }
    args.push(entry.path.as_os_str());
    let output = git_output(&entry.source_repository, args)?;
    if !output.status.success() {
        bail!("Failed to remove worktree: {}", command_error(&output));
    }

    match delete_branch_if_owned(&entry.source_repository, &entry.branch, &branch_oid, true)? {
        BranchCleanup::Removed => Ok(RemovalOutcome {
            branch_removed: true,
            branch_notice: None,
        }),
        BranchCleanup::Missing => Ok(RemovalOutcome {
            branch_removed: false,
            branch_notice: Some(format!(
                "Worktree removed, but branch '{}' was already absent",
                entry.branch
            )),
        }),
        BranchCleanup::Preserved(reason) => Ok(RemovalOutcome {
            branch_removed: false,
            branch_notice: Some(format!(
                "Worktree removed, but branch '{}' was preserved: {}",
                entry.branch, reason
            )),
        }),
    }
}

fn rollback_registered_worktree_in(entry: &ManagedWorktree, registry_path: &Path) -> Result<()> {
    let _lock = RegistryLock::acquire(registry_path)?;
    let mut registry = load_registry_unlocked(registry_path)?;
    let registered = registry
        .worktrees
        .iter()
        .find(|registered| path_key(&registered.path) == path_key(&entry.path))
        .cloned()
        .context("Refusing to roll back a worktree that is not registered as TUI-managed")?;
    if registered != *entry {
        bail!("Managed worktree metadata changed; refusing rollback");
    }

    let branch_oid = validate_worktree_identity(entry)?;
    let remove = git_output(
        &entry.source_repository,
        [
            OsStr::new("worktree"),
            OsStr::new("remove"),
            OsStr::new("--force"),
            entry.path.as_os_str(),
        ],
    )?;
    if !remove.status.success() {
        bail!("Failed to roll back worktree: {}", command_error(&remove));
    }

    let mut errors = Vec::new();
    if let BranchCleanup::Preserved(reason) =
        delete_branch_if_owned(&entry.source_repository, &entry.branch, &branch_oid, false)?
    {
        errors.push(format!("branch was preserved: {reason}"));
    }

    let old_len = registry.worktrees.len();
    registry.worktrees.retain(|registered| registered != entry);
    if registry.worktrees.len() == old_len {
        errors.push("registry ownership changed during rollback".to_string());
    } else if let Err(error) = save_registry_unlocked(registry_path, &registry) {
        errors.push(format!("registry cleanup failed: {error}"));
    }

    if errors.is_empty() {
        Ok(())
    } else {
        bail!("{}", errors.join("; "))
    }
}

fn rollback_reserved_creation_in(
    reservation: &WorktreeReservation,
    registry_path: &Path,
) -> Result<()> {
    let _lock = RegistryLock::acquire(registry_path)?;
    let mut registry = load_registry_unlocked(registry_path)?;
    if !registry
        .reservations
        .iter()
        .any(|registered| registered == reservation)
    {
        bail!("Creation reservation ownership changed; refusing rollback");
    }

    let mut errors = Vec::new();
    let mut owned_worktree_remains = false;
    if reservation.path.exists() && validate_reservation_identity(reservation).is_ok() {
        match git_output(
            &reservation.source_repository,
            [
                OsStr::new("worktree"),
                OsStr::new("remove"),
                OsStr::new("--force"),
                reservation.path.as_os_str(),
            ],
        ) {
            Ok(remove) if remove.status.success() => {}
            Ok(remove) => {
                owned_worktree_remains = true;
                errors.push(format!(
                    "Failed to remove reserved worktree: {}",
                    command_error(&remove)
                ));
            }
            Err(error) => {
                owned_worktree_remains = true;
                errors.push(format!("Failed to remove reserved worktree: {error}"));
            }
        }
    }

    let branch_remains = match delete_branch_if_owned(
        &reservation.source_repository,
        &reservation.branch,
        &reservation.base_oid,
        false,
    ) {
        Ok(BranchCleanup::Removed | BranchCleanup::Missing) => false,
        Ok(BranchCleanup::Preserved(reason)) => {
            errors.push(format!("Reserved branch was preserved: {reason}"));
            true
        }
        Err(error) => {
            errors.push(format!("Failed to clean up reserved branch: {error}"));
            true
        }
    };

    if !owned_worktree_remains && !branch_remains {
        let old_len = registry.reservations.len();
        registry
            .reservations
            .retain(|registered| registered != reservation);
        if registry.reservations.len() == old_len {
            errors.push("creation reservation ownership changed during rollback".to_string());
        } else if let Err(error) = save_registry_unlocked(registry_path, &registry) {
            errors.push(format!("creation reservation cleanup failed: {error}"));
        }
    }

    if errors.is_empty() {
        Ok(())
    } else {
        bail!("{}", errors.join("; "))
    }
}

fn reserve_creation_in(reservation: &WorktreeReservation, path: &Path) -> Result<()> {
    let _lock = RegistryLock::acquire(path)?;
    let mut registry = load_registry_unlocked(path)?;
    reject_path_collision(&reservation.source_repository, &reservation.path, &registry)?;
    if registry.worktrees.iter().any(|entry| {
        path_key(&entry.common_git_dir) == path_key(&reservation.common_git_dir)
            && entry.branch == reservation.branch
    }) || registry.reservations.iter().any(|existing| {
        path_key(&existing.common_git_dir) == path_key(&reservation.common_git_dir)
            && existing.branch == reservation.branch
    }) {
        bail!(
            "The worktree registry already reserves branch '{}'",
            reservation.branch
        );
    }
    registry.reservations.push(reservation.clone());
    save_registry_unlocked(path, &registry)
}

fn finalize_creation_in(
    reservation: &WorktreeReservation,
    entry: &ManagedWorktree,
    path: &Path,
) -> Result<()> {
    let _lock = RegistryLock::acquire(path)?;
    let mut registry = load_registry_unlocked(path)?;
    if !registry
        .reservations
        .iter()
        .any(|registered| registered == reservation)
    {
        bail!("Creation reservation ownership changed; refusing registration");
    }
    if registry
        .worktrees
        .iter()
        .any(|existing| path_key(&existing.path) == path_key(&entry.path))
    {
        bail!("Worktree is already registered: {}", entry.path.display());
    }

    validate_worktree_identity(entry)?;
    registry
        .reservations
        .retain(|registered| registered != reservation);
    registry.worktrees.push(entry.clone());
    save_registry_unlocked(path, &registry)
}

fn release_reservation_in(reservation: &WorktreeReservation, path: &Path) -> Result<()> {
    let _lock = RegistryLock::acquire(path)?;
    let mut registry = load_registry_unlocked(path)?;
    let old_len = registry.reservations.len();
    registry
        .reservations
        .retain(|registered| registered != reservation);
    if registry.reservations.len() == old_len {
        bail!("Creation reservation ownership changed; refusing cleanup");
    }
    save_registry_unlocked(path, &registry)
}

fn unregister_in(entry: &ManagedWorktree, path: &Path) -> Result<()> {
    let _lock = RegistryLock::acquire(path)?;
    let mut registry = load_registry_unlocked(path)?;
    let old_len = registry.worktrees.len();
    registry.worktrees.retain(|existing| existing != entry);
    if registry.worktrees.len() != old_len {
        save_registry_unlocked(path, &registry)?;
    }
    Ok(())
}

fn managed_worktree_for_cwd_in(cwd: &Path, path: &Path) -> Result<Option<ManagedWorktree>> {
    let _lock = RegistryLock::acquire(path)?;
    let registry = load_registry_unlocked(path)?;
    let key = path_key(cwd);
    Ok(registry
        .worktrees
        .into_iter()
        .find(|entry| path_key(&entry.path) == key))
}

#[cfg(test)]
fn load_registry(path: &Path) -> Result<WorktreeRegistry> {
    let _lock = RegistryLock::acquire(path)?;
    load_registry_unlocked(path)
}

fn load_registry_unlocked(path: &Path) -> Result<WorktreeRegistry> {
    if !path.exists() {
        return Ok(WorktreeRegistry {
            version: REGISTRY_VERSION,
            worktrees: Vec::new(),
            reservations: Vec::new(),
        });
    }

    let content = fs::read_to_string(path)
        .with_context(|| format!("Failed to read worktree registry: {}", path.display()))?;
    let mut registry: WorktreeRegistry = serde_json::from_str(&content)
        .with_context(|| format!("Failed to parse worktree registry: {}", path.display()))?;
    if registry.version != REGISTRY_VERSION {
        bail!(
            "Unsupported worktree registry version {} in {}",
            registry.version,
            path.display()
        );
    }

    let old_len = registry.worktrees.len();
    registry
        .worktrees
        .retain(|entry| entry.path.try_exists().unwrap_or(true));
    if registry.worktrees.len() != old_len {
        save_registry_unlocked(path, &registry)?;
    }
    Ok(registry)
}

#[cfg(test)]
fn save_registry(path: &Path, registry: &WorktreeRegistry) -> Result<()> {
    let _lock = RegistryLock::acquire(path)?;
    save_registry_unlocked(path, registry)
}

fn save_registry_unlocked(path: &Path, registry: &WorktreeRegistry) -> Result<()> {
    let parent = path
        .parent()
        .context("Worktree registry path has no parent")?;
    fs::create_dir_all(parent)
        .with_context(|| format!("Failed to create registry directory: {}", parent.display()))?;
    let mut temp = tempfile::NamedTempFile::new_in(parent).with_context(|| {
        format!(
            "Failed to create registry temporary file in {}",
            parent.display()
        )
    })?;
    serde_json::to_writer_pretty(temp.as_file_mut(), registry)
        .context("Failed to serialize worktree registry")?;
    temp.as_file_mut().write_all(b"\n")?;
    temp.as_file_mut().sync_all()?;
    temp.persist(path)
        .map_err(|error| error.error)
        .with_context(|| format!("Failed to replace worktree registry: {}", path.display()))?;
    Ok(())
}

impl RegistryLock {
    fn acquire(registry_path: &Path) -> Result<Self> {
        Self::acquire_with_timeout(registry_path, REGISTRY_LOCK_TIMEOUT)
    }

    fn acquire_with_timeout(registry_path: &Path, timeout: Duration) -> Result<Self> {
        let lock_path = registry_path.with_extension("lock");
        let parent = lock_path
            .parent()
            .context("Worktree registry lock path has no parent")?;
        fs::create_dir_all(parent).with_context(|| {
            format!(
                "Failed to create worktree registry directory: {}",
                parent.display()
            )
        })?;
        let file = OpenOptions::new()
            .create(true)
            .truncate(false)
            .read(true)
            .write(true)
            .open(&lock_path)
            .with_context(|| {
                format!(
                    "Failed to open worktree registry lock: {}",
                    lock_path.display()
                )
            })?;
        let started = Instant::now();
        loop {
            match FileExt::try_lock(&file) {
                Ok(()) => return Ok(Self { _file: file }),
                Err(TryLockError::WouldBlock) if started.elapsed() < timeout => {
                    thread::sleep(
                        REGISTRY_LOCK_RETRY.min(timeout.saturating_sub(started.elapsed())),
                    );
                }
                Err(TryLockError::WouldBlock) => {
                    bail!(
                        "Timed out after {:.1}s waiting for worktree registry lock {}. OS file locks are released when a process exits; another live process is still using the registry",
                        timeout.as_secs_f64(),
                        lock_path.display()
                    );
                }
                Err(TryLockError::Error(error)) => {
                    return Err(error).with_context(|| {
                        format!("Failed to lock worktree registry: {}", lock_path.display())
                    });
                }
            }
        }
    }
}

#[derive(Debug)]
enum BranchCleanup {
    Removed,
    Missing,
    Preserved(String),
}

fn with_reservation_release_error(
    error: anyhow::Error,
    reservation: &WorktreeReservation,
    registry_path: &Path,
) -> anyhow::Error {
    match release_reservation_in(reservation, registry_path) {
        Ok(()) => error,
        Err(cleanup_error) => error.context(format!(
            "Creation reservation cleanup also failed: {cleanup_error}"
        )),
    }
}

fn with_creation_rollback_error(
    error: anyhow::Error,
    reservation: &WorktreeReservation,
    registry_path: &Path,
) -> anyhow::Error {
    match rollback_reserved_creation_in(reservation, registry_path) {
        Ok(()) => error.context("Creation was rolled back"),
        Err(rollback_error) => error.context(format!(
            "Creation rollback refused or failed: {rollback_error}"
        )),
    }
}

fn write_worktree_identity(reservation: &WorktreeReservation) -> Result<()> {
    let marker = WorktreeIdentity {
        identity: reservation.identity.clone(),
        source_repository: reservation.source_repository.clone(),
        common_git_dir: reservation.common_git_dir.clone(),
        branch: reservation.branch.clone(),
    };
    let marker_path = worktree_identity_path(&reservation.path)?;
    let mut file = OpenOptions::new()
        .create_new(true)
        .write(true)
        .open(&marker_path)
        .with_context(|| {
            format!(
                "Failed to create worktree identity marker: {}",
                marker_path.display()
            )
        })?;
    serde_json::to_writer_pretty(&mut file, &marker)
        .context("Failed to serialize worktree identity")?;
    file.write_all(b"\n")?;
    file.sync_all()?;
    Ok(())
}

fn worktree_identity_path(worktree: &Path) -> Result<PathBuf> {
    let git_dir = git_stdout(worktree, ["rev-parse", "--absolute-git-dir"])
        .context("Failed to resolve worktree Git directory")?;
    Ok(canonicalize_or_absolute(Path::new(&git_dir)).join(IDENTITY_FILE))
}

fn validate_reservation_identity(reservation: &WorktreeReservation) -> Result<String> {
    validate_worktree_identity_fields(
        &reservation.path,
        &reservation.source_repository,
        &reservation.common_git_dir,
        &reservation.branch,
        &reservation.identity,
    )
}

fn validate_worktree_identity(entry: &ManagedWorktree) -> Result<String> {
    validate_worktree_identity_fields(
        &entry.path,
        &entry.source_repository,
        &entry.common_git_dir,
        &entry.branch,
        &entry.identity,
    )
}

fn validate_worktree_identity_fields(
    path: &Path,
    source_repository: &Path,
    common_git_dir: &Path,
    branch: &str,
    identity: &str,
) -> Result<String> {
    let actual_repository = resolve_repository(path)
        .context("Refusing removal because the registered path is not the expected worktree")?;
    if path_key(&actual_repository.main_worktree) != path_key(source_repository)
        || path_key(&actual_repository.common_git_dir) != path_key(common_git_dir)
    {
        bail!("Refusing removal because the worktree repository identity changed");
    }

    let marker_path = worktree_identity_path(path)?;
    let marker_content = fs::read_to_string(&marker_path).with_context(|| {
        format!(
            "Refusing removal because the worktree identity marker is unavailable: {}",
            marker_path.display()
        )
    })?;
    let marker: WorktreeIdentity = serde_json::from_str(&marker_content).with_context(|| {
        format!(
            "Refusing removal because the worktree identity marker is invalid: {}",
            marker_path.display()
        )
    })?;
    let expected_marker = WorktreeIdentity {
        identity: identity.to_string(),
        source_repository: source_repository.to_path_buf(),
        common_git_dir: common_git_dir.to_path_buf(),
        branch: branch.to_string(),
    };
    if marker != expected_marker {
        bail!("Refusing removal because the durable worktree identity changed");
    }

    let actual_branch = git_stdout(path, ["symbolic-ref", "--quiet", "--short", "HEAD"]).context(
        "Refusing removal because the worktree is detached or its branch is unavailable",
    )?;
    if actual_branch != branch {
        bail!(
            "Refusing removal because the worktree branch changed from '{}' to '{}'",
            branch,
            actual_branch
        );
    }

    let head = git_stdout(path, ["rev-parse", "--verify", "HEAD^{commit}"])?;
    let tip = branch_tip(source_repository, branch)?
        .context("Refusing removal because the registered branch no longer exists")?;
    if head != tip {
        bail!("Refusing removal because the worktree HEAD and registered branch differ");
    }
    Ok(head)
}

fn delete_branch_if_owned(
    repository: &Path,
    branch: &str,
    expected_oid: &str,
    require_merged: bool,
) -> Result<BranchCleanup> {
    let Some(actual_oid) = branch_tip(repository, branch)? else {
        return Ok(BranchCleanup::Missing);
    };
    if actual_oid != expected_oid {
        return Ok(BranchCleanup::Preserved(format!(
            "branch tip changed from the expected commit {expected_oid}"
        )));
    }
    if branch_is_checked_out(repository, branch)? {
        return Ok(BranchCleanup::Preserved(
            "branch is checked out by another worktree".to_string(),
        ));
    }
    if require_merged {
        let merged = git_output(
            repository,
            ["merge-base", "--is-ancestor", expected_oid, "HEAD"],
        )?;
        if !merged.status.success() {
            if merged.status.code() == Some(1) {
                return Ok(BranchCleanup::Preserved(
                    "branch contains commits not merged into the source repository HEAD"
                        .to_string(),
                ));
            }
            bail!(
                "Failed to determine whether branch '{}' is merged: {}",
                branch,
                command_error(&merged)
            );
        }
    }

    let reference = format!("refs/heads/{branch}");
    let output = git_output(
        repository,
        [
            OsStr::new("update-ref"),
            OsStr::new("-d"),
            OsStr::new(&reference),
            OsStr::new(expected_oid),
        ],
    )?;
    if output.status.success() {
        Ok(BranchCleanup::Removed)
    } else {
        Ok(BranchCleanup::Preserved(format!(
            "atomic branch deletion failed: {}",
            command_error(&output)
        )))
    }
}

fn branch_tip(repository: &Path, branch: &str) -> Result<Option<String>> {
    let reference = format!("refs/heads/{branch}^{{commit}}");
    let output = git_output(
        repository,
        ["rev-parse", "--verify", "--quiet", reference.as_str()],
    )?;
    if output.status.success() {
        return Ok(Some(
            String::from_utf8_lossy(&output.stdout).trim().to_string(),
        ));
    }
    if output.status.code() == Some(1) {
        return Ok(None);
    }
    bail!(
        "Failed to resolve branch '{}': {}",
        branch,
        command_error(&output)
    )
}

fn branch_is_checked_out(repository: &Path, branch: &str) -> Result<bool> {
    let branch_line = format!("branch refs/heads/{branch}");
    let worktrees = git_stdout(repository, ["worktree", "list", "--porcelain"])?;
    Ok(worktrees.lines().any(|line| line == branch_line))
}

fn included_files_are_dirty(entry: &ManagedWorktree) -> Result<bool> {
    if entry.include_patterns.is_none() && entry.included_files.is_empty() {
        return Ok(false);
    }

    let tracked_output = git_output(&entry.path, ["ls-files", "-z", "--"])?;
    if !tracked_output.status.success() {
        bail!(
            "Failed to enumerate tracked worktree files: {}",
            command_error(&tracked_output)
        );
    }
    let tracked: HashSet<String> = nul_paths(&tracked_output.stdout)?
        .into_iter()
        .map(|path| relative_path_key(&path))
        .collect();
    let initial: HashMap<String, &IncludedFileState> = entry
        .included_files
        .iter()
        .map(|state| (relative_path_key(&state.path), state))
        .collect();

    for state in &entry.included_files {
        if tracked.contains(&relative_path_key(&state.path)) {
            continue;
        }
        let path = entry.path.join(&state.path);
        if !path.is_file() || hash_file(&path)? != state.sha256 {
            return Ok(true);
        }
    }

    let Some(patterns) = entry.include_patterns.as_deref() else {
        return Ok(false);
    };
    let matcher =
        build_include_matcher(&entry.path, &entry.path.join(".worktreeinclude"), patterns)?;
    let ignored = git_output(
        &entry.path,
        [
            "ls-files",
            "--others",
            "--ignored",
            "--exclude-standard",
            "-z",
            "--",
        ],
    )?;
    if !ignored.status.success() {
        bail!(
            "Failed to enumerate ignored worktree files: {}",
            command_error(&ignored)
        );
    }
    for path in nul_paths(&ignored.stdout)? {
        if !is_safe_relative_path(&path) {
            bail!("Git returned an unsafe ignored path: {}", path.display());
        }
        if matcher
            .matched_path_or_any_parents(&path, false)
            .is_ignore()
            && !initial.contains_key(&relative_path_key(&path))
        {
            return Ok(true);
        }
    }
    Ok(false)
}

fn nul_paths(output: &[u8]) -> Result<Vec<PathBuf>> {
    output
        .split(|byte| *byte == 0)
        .filter(|path| !path.is_empty())
        .map(|path| {
            std::str::from_utf8(path)
                .map(PathBuf::from)
                .context("Git returned a non-UTF-8 file path")
        })
        .collect()
}

fn relative_path_key(path: &Path) -> String {
    let value = path.to_string_lossy().replace('\\', "/");
    if cfg!(windows) {
        value.to_lowercase()
    } else {
        value
    }
}

fn hash_file(path: &Path) -> Result<String> {
    let mut file = File::open(path)
        .with_context(|| format!("Failed to read included file: {}", path.display()))?;
    let mut digest = Sha256::new();
    let mut buffer = [0_u8; 64 * 1024];
    loop {
        let read = file
            .read(&mut buffer)
            .with_context(|| format!("Failed to hash included file: {}", path.display()))?;
        if read == 0 {
            break;
        }
        digest.update(&buffer[..read]);
    }
    Ok(format!("{:x}", digest.finalize()))
}

fn registry_version() -> u32 {
    REGISTRY_VERSION
}

/// The installed Git version, or `None` when Git cannot be run at all.
///
/// Deliberately not routed through [`git_output`], which needs a repository for
/// `git -C`; the point here is whether the binary exists before any repository is in
/// play. It lives in this module so the program name stays where Git is actually used.
pub fn git_version() -> Option<String> {
    let output = Command::new("git").arg("--version").output().ok()?;
    if !output.status.success() {
        return None;
    }
    let line = String::from_utf8_lossy(&output.stdout)
        .lines()
        .map(str::trim)
        .find(|line| !line.is_empty())?
        .chars()
        .filter(|character| !character.is_control())
        .take(80)
        .collect::<String>();
    (!line.is_empty()).then_some(line)
}

fn git_stdout<I, S>(repository: &Path, args: I) -> Result<String>
where
    I: IntoIterator<Item = S>,
    S: AsRef<OsStr>,
{
    let output = git_output(repository, args)?;
    if !output.status.success() {
        bail!("{}", command_error(&output));
    }
    Ok(String::from_utf8_lossy(&output.stdout).trim().to_string())
}

fn git_output<I, S>(repository: &Path, args: I) -> Result<Output>
where
    I: IntoIterator<Item = S>,
    S: AsRef<OsStr>,
{
    Command::new("git")
        .arg("-C")
        .arg(repository)
        .args(args)
        .output()
        .with_context(|| format!("Failed to run Git in {}", repository.display()))
}

fn command_error(output: &Output) -> String {
    let stderr = String::from_utf8_lossy(&output.stderr).trim().to_string();
    if stderr.is_empty() {
        String::from_utf8_lossy(&output.stdout).trim().to_string()
    } else {
        stderr
    }
}

fn canonicalize_or_absolute(path: &Path) -> PathBuf {
    let canonical = fs::canonicalize(path).unwrap_or_else(|_| {
        if path.is_absolute() {
            path.to_path_buf()
        } else {
            std::env::current_dir()
                .unwrap_or_else(|_| PathBuf::from("."))
                .join(path)
        }
    });
    strip_windows_prefix(canonical)
}

fn strip_windows_prefix(path: PathBuf) -> PathBuf {
    let text = path.to_string_lossy();
    if let Some(stripped) = text.strip_prefix(r"\\?\UNC\") {
        return PathBuf::from(format!(r"\\{stripped}"));
    }
    match text.strip_prefix(r"\\?\") {
        Some(stripped) => PathBuf::from(stripped),
        None => path,
    }
}

fn path_key(path: &Path) -> String {
    let value = canonicalize_or_absolute(path)
        .to_string_lossy()
        .replace('/', "\\");
    if cfg!(windows) {
        value.to_lowercase()
    } else {
        value
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::{Arc, Barrier};

    fn git(repo: &Path, args: &[&str]) {
        let output = Command::new("git")
            .arg("-C")
            .arg(repo)
            .args(args)
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "git {:?}: {}",
            args,
            command_error(&output)
        );
    }

    fn repository() -> tempfile::TempDir {
        let temp = tempfile::tempdir().unwrap();
        git(temp.path(), &["init"]);
        git(temp.path(), &["config", "user.email", "tests@example.com"]);
        git(temp.path(), &["config", "user.name", "CST Tests"]);
        fs::write(temp.path().join("tracked.txt"), "main").unwrap();
        git(temp.path(), &["add", "."]);
        git(temp.path(), &["commit", "-m", "initial"]);
        temp
    }

    fn test_config(temp: &tempfile::TempDir) -> EffectiveWorktreeConfig {
        EffectiveWorktreeConfig {
            branch_prefix: "copilot/".to_string(),
            root: temp.path().join("wt"),
        }
    }

    #[test]
    fn generated_paths_are_safe_bounded_and_collision_resistant() {
        let temp = tempfile::tempdir().unwrap();
        let first = derive_worktree_path(
            temp.path(),
            Path::new("C:\\repo"),
            Path::new("C:\\repo\\.git"),
            "feature/a:b*long-name",
        )
        .unwrap();
        let second = derive_worktree_path(
            temp.path(),
            Path::new("C:\\repo"),
            Path::new("C:\\repo\\.git"),
            "feature/a?b*long-name",
        )
        .unwrap();

        assert_ne!(first, second);
        assert!(first.to_string_lossy().encode_utf16().count() <= MAX_DERIVED_PATH_CHARS);
        let derived = first.strip_prefix(temp.path()).unwrap().to_string_lossy();
        assert!(!derived.contains(':'));
        assert!(!derived.contains('*'));
    }

    #[test]
    fn generated_branch_uses_configured_prefix_and_valid_git_syntax() {
        let branch = generated_branch_name("copilot/");
        assert!(branch.starts_with("copilot/"));
        validate_branch_syntax(&branch).unwrap();
        validate_branch_prefix("feature/").unwrap();
        assert!(validate_branch_prefix("bad..prefix/").is_err());
    }

    #[test]
    fn creates_from_cached_origin_head() {
        let repo = repository();
        let temp = tempfile::tempdir().unwrap();
        git(repo.path(), &["branch", "-M", "main"]);
        git(
            repo.path(),
            &["update-ref", "refs/remotes/origin/main", "HEAD"],
        );
        git(
            repo.path(),
            &[
                "symbolic-ref",
                "refs/remotes/origin/HEAD",
                "refs/remotes/origin/main",
            ],
        );
        let registry = temp.path().join("registry.json");

        let created = create_managed_worktree_with(
            repo.path(),
            "copilot/remote",
            &test_config(&temp),
            &registry,
            |_, _| Ok(WorktreeIncludeState::default()),
        )
        .unwrap();

        assert!(created.notice.is_none());
        assert!(created.entry.path.join("tracked.txt").exists());
    }

    #[test]
    fn falls_back_to_current_head_with_notice() {
        let repo = repository();
        let temp = tempfile::tempdir().unwrap();
        let created = create_managed_worktree_with(
            repo.path(),
            "copilot/fallback",
            &test_config(&temp),
            &temp.path().join("registry.json"),
            |_, _| Ok(WorktreeIncludeState::default()),
        )
        .unwrap();

        assert!(created.notice.unwrap().contains("current HEAD"));
    }

    #[test]
    fn rejects_existing_branch_collision() {
        let repo = repository();
        let temp = tempfile::tempdir().unwrap();
        git(repo.path(), &["branch", "copilot/existing"]);

        let error = create_managed_worktree_with(
            repo.path(),
            "copilot/existing",
            &test_config(&temp),
            &temp.path().join("registry.json"),
            |_, _| Ok(WorktreeIncludeState::default()),
        )
        .unwrap_err()
        .to_string();

        assert!(error.contains("Branch already exists"));
    }

    #[test]
    fn initialization_failure_rolls_back_worktree_and_branch() {
        let repo = repository();
        let temp = tempfile::tempdir().unwrap();
        let repository_info = resolve_repository(repo.path()).unwrap();
        let expected_path = derive_worktree_path(
            &test_config(&temp).root,
            &repository_info.main_worktree,
            &repository_info.common_git_dir,
            "copilot/fail",
        )
        .unwrap();
        let result = create_managed_worktree_with(
            repo.path(),
            "copilot/fail",
            &test_config(&temp),
            &temp.path().join("registry.json"),
            |_, _| bail!("injected failure"),
        );

        assert!(result.is_err());
        let branch = git_output(
            repo.path(),
            ["show-ref", "--verify", "--quiet", "refs/heads/copilot/fail"],
        )
        .unwrap();
        assert!(!branch.status.success());
        assert!(!expected_path.exists());
    }

    #[test]
    fn concurrent_creation_keeps_the_successful_worktree_and_branch() {
        let repo = repository();
        let temp = tempfile::tempdir().unwrap();
        let registry = temp.path().join("registry.json");
        let config = test_config(&temp);
        let barrier = Arc::new(Barrier::new(2));

        let created = thread::scope(|scope| {
            let first_barrier = Arc::clone(&barrier);
            let first_repo = repo.path();
            let first_config = &config;
            let first_registry = &registry;
            let first = scope.spawn(move || {
                first_barrier.wait();
                create_managed_worktree_with(
                    first_repo,
                    "copilot/concurrent",
                    first_config,
                    first_registry,
                    |_, _| Ok(WorktreeIncludeState::default()),
                )
            });
            let second_barrier = Arc::clone(&barrier);
            let second_repo = repo.path();
            let second_config = &config;
            let second_registry = &registry;
            let second = scope.spawn(move || {
                second_barrier.wait();
                create_managed_worktree_with(
                    second_repo,
                    "copilot/concurrent",
                    second_config,
                    second_registry,
                    |_, _| Ok(WorktreeIncludeState::default()),
                )
            });

            [first.join().unwrap(), second.join().unwrap()]
                .into_iter()
                .filter_map(Result::ok)
                .collect::<Vec<_>>()
        });

        assert_eq!(created.len(), 1);
        assert!(created[0].entry.path.exists());
        assert!(branch_exists(repo.path(), "copilot/concurrent").unwrap());
        let registry = load_registry(&registry).unwrap();
        assert_eq!(registry.worktrees.len(), 1);
        assert!(registry.reservations.is_empty());
    }

    #[test]
    fn dirty_worktree_requires_force_and_clean_deletion_removes_branch() {
        let repo = repository();
        let temp = tempfile::tempdir().unwrap();
        let registry = temp.path().join("registry.json");
        let created = create_managed_worktree_with(
            repo.path(),
            "copilot/dirty",
            &test_config(&temp),
            &registry,
            |_, _| Ok(WorktreeIncludeState::default()),
        )
        .unwrap();
        fs::write(created.entry.path.join("tracked.txt"), "dirty").unwrap();

        assert!(remove_managed_worktree_in(&created.entry, false, &registry).is_err());
        assert!(created.entry.path.exists());
        let outcome = remove_managed_worktree_in(&created.entry, true, &registry).unwrap();
        assert!(outcome.branch_removed);
        assert!(!created.entry.path.exists());
    }

    #[test]
    fn reused_path_with_different_identity_is_never_deleted() {
        let repo = repository();
        let temp = tempfile::tempdir().unwrap();
        let registry = temp.path().join("registry.json");
        let created = create_managed_worktree_with(
            repo.path(),
            "copilot/original",
            &test_config(&temp),
            &registry,
            |_, _| Ok(WorktreeIncludeState::default()),
        )
        .unwrap();
        let reused_path = created.entry.path.clone();

        git(
            repo.path(),
            &[
                "worktree",
                "remove",
                "--force",
                reused_path.to_str().unwrap(),
            ],
        );
        git(repo.path(), &["branch", "-D", "copilot/original"]);
        git(
            repo.path(),
            &[
                "worktree",
                "add",
                "-b",
                "replacement",
                reused_path.to_str().unwrap(),
                "HEAD",
            ],
        );

        let error = remove_managed_worktree_in(&created.entry, true, &registry)
            .unwrap_err()
            .to_string();
        assert!(error.contains("identity marker"));
        assert!(reused_path.exists());
        assert!(branch_exists(repo.path(), "replacement").unwrap());
    }

    #[test]
    fn unmerged_branch_is_preserved_after_worktree_removal() {
        let repo = repository();
        let temp = tempfile::tempdir().unwrap();
        let registry = temp.path().join("registry.json");
        let created = create_managed_worktree_with(
            repo.path(),
            "copilot/unmerged",
            &test_config(&temp),
            &registry,
            |_, _| Ok(WorktreeIncludeState::default()),
        )
        .unwrap();
        fs::write(created.entry.path.join("new.txt"), "branch only").unwrap();
        git(&created.entry.path, &["add", "."]);
        git(&created.entry.path, &["commit", "-m", "branch commit"]);

        let outcome = remove_managed_worktree_in(&created.entry, false, &registry).unwrap();
        assert!(!outcome.branch_removed);
        assert!(outcome.branch_notice.unwrap().contains("preserved"));
        let branch = git_output(
            repo.path(),
            [
                "show-ref",
                "--verify",
                "--quiet",
                "refs/heads/copilot/unmerged",
            ],
        )
        .unwrap();
        assert!(branch.status.success());
    }

    #[test]
    fn registry_prunes_stale_entries_and_does_not_own_manual_worktrees() {
        let repo = repository();
        let temp = tempfile::tempdir().unwrap();
        let registry_path = temp.path().join("registry.json");
        let manual = temp.path().join("manual");
        git(
            repo.path(),
            &[
                "worktree",
                "add",
                "-b",
                "manual",
                manual.to_str().unwrap(),
                "HEAD",
            ],
        );
        assert!(managed_worktree_for_cwd_in(&manual, &registry_path)
            .unwrap()
            .is_none());

        let stale = ManagedWorktree {
            path: temp.path().join("missing"),
            source_repository: repo.path().to_path_buf(),
            common_git_dir: repo.path().join(".git"),
            branch: "stale".to_string(),
            identity: "stale-identity".to_string(),
            include_patterns: None,
            included_files: Vec::new(),
        };
        save_registry(
            &registry_path,
            &WorktreeRegistry {
                version: REGISTRY_VERSION,
                worktrees: vec![stale],
                reservations: Vec::new(),
            },
        )
        .unwrap();
        assert!(load_registry(&registry_path).unwrap().worktrees.is_empty());
    }

    #[test]
    fn registry_lock_times_out_and_recovers_after_owner_exit() {
        let temp = tempfile::tempdir().unwrap();
        let registry = temp.path().join("registry.json");
        let first = RegistryLock::acquire_with_timeout(&registry, Duration::from_secs(1)).unwrap();
        let error = match RegistryLock::acquire_with_timeout(&registry, Duration::from_millis(60)) {
            Ok(_) => panic!("second registry lock unexpectedly succeeded"),
            Err(error) => error.to_string(),
        };
        assert!(error.contains("Timed out"));

        drop(first);
        RegistryLock::acquire_with_timeout(&registry, Duration::from_secs(1)).unwrap();
    }

    #[test]
    fn concurrent_registry_mutations_preserve_both_reservations() {
        let repo = repository();
        let temp = tempfile::tempdir().unwrap();
        let registry_path = temp.path().join("registry.json");
        let repository = resolve_repository(repo.path()).unwrap();
        let base_oid = git_stdout(repo.path(), ["rev-parse", "HEAD"]).unwrap();
        let reservation = |identity: &str, branch: &str| WorktreeReservation {
            identity: identity.to_string(),
            path: temp.path().join(identity),
            source_repository: repository.main_worktree.clone(),
            common_git_dir: repository.common_git_dir.clone(),
            branch: branch.to_string(),
            base_oid: base_oid.clone(),
        };
        let first = reservation("first", "copilot/first");
        let second = reservation("second", "copilot/second");
        let barrier = Arc::new(Barrier::new(2));

        thread::scope(|scope| {
            let first_barrier = Arc::clone(&barrier);
            let first_registry = &registry_path;
            let first_reservation = &first;
            let first_thread = scope.spawn(move || {
                first_barrier.wait();
                reserve_creation_in(first_reservation, first_registry)
            });
            let second_barrier = Arc::clone(&barrier);
            let second_registry = &registry_path;
            let second_reservation = &second;
            let second_thread = scope.spawn(move || {
                second_barrier.wait();
                reserve_creation_in(second_reservation, second_registry)
            });

            first_thread.join().unwrap().unwrap();
            second_thread.join().unwrap().unwrap();
        });

        let registry = load_registry(&registry_path).unwrap();
        assert_eq!(registry.reservations.len(), 2);
        assert!(registry.reservations.contains(&first));
        assert!(registry.reservations.contains(&second));
    }

    #[test]
    fn included_ignored_files_are_clean_until_changed_or_added() {
        let repo = repository();
        let temp = tempfile::tempdir().unwrap();
        let registry = temp.path().join("registry.json");
        fs::write(repo.path().join(".gitignore"), ".env\ncache/\n").unwrap();
        fs::write(repo.path().join(".worktreeinclude"), ".env\ncache/\n").unwrap();
        git(repo.path(), &["add", ".gitignore", ".worktreeinclude"]);
        git(repo.path(), &["commit", "-m", "add worktree includes"]);
        fs::write(repo.path().join(".env"), "secret").unwrap();

        let created = create_managed_worktree_with(
            repo.path(),
            "copilot/includes",
            &test_config(&temp),
            &registry,
            copy_worktree_includes,
        )
        .unwrap();
        let entry = managed_worktree_for_cwd_in(&created.entry.path, &registry)
            .unwrap()
            .unwrap();

        assert_eq!(entry.included_files.len(), 1);
        assert!(!is_dirty(&entry).unwrap());

        fs::write(entry.path.join(".env"), "changed").unwrap();
        assert!(is_dirty(&entry).unwrap());

        fs::write(entry.path.join(".env"), "secret").unwrap();
        assert!(!is_dirty(&entry).unwrap());
        fs::create_dir_all(entry.path.join("cache")).unwrap();
        fs::write(entry.path.join("cache").join("new.txt"), "new").unwrap();
        assert!(is_dirty(&entry).unwrap());
    }

    #[test]
    fn tracked_include_destination_fails_setup_without_overwriting() {
        let repo = repository();
        let temp = tempfile::tempdir().unwrap();
        let registry = temp.path().join("registry.json");
        fs::write(repo.path().join("collision.txt"), "tracked target").unwrap();
        git(repo.path(), &["add", "collision.txt"]);
        git(repo.path(), &["commit", "-m", "track collision"]);
        git(repo.path(), &["branch", "-M", "main"]);
        git(
            repo.path(),
            &["update-ref", "refs/remotes/origin/main", "HEAD"],
        );
        git(
            repo.path(),
            &[
                "symbolic-ref",
                "refs/remotes/origin/HEAD",
                "refs/remotes/origin/main",
            ],
        );
        git(repo.path(), &["checkout", "-b", "source"]);
        git(repo.path(), &["rm", "collision.txt"]);
        fs::write(repo.path().join(".gitignore"), "collision.txt\n").unwrap();
        fs::write(repo.path().join(".worktreeinclude"), "collision.txt\n").unwrap();
        git(repo.path(), &["add", ".gitignore", ".worktreeinclude"]);
        git(repo.path(), &["commit", "-m", "ignore source collision"]);
        fs::write(repo.path().join("collision.txt"), "ignored source").unwrap();

        let repository_info = resolve_repository(repo.path()).unwrap();
        let expected_path = derive_worktree_path(
            &test_config(&temp).root,
            &repository_info.main_worktree,
            &repository_info.common_git_dir,
            "copilot/tracked-collision",
        )
        .unwrap();
        let error = create_managed_worktree_with(
            repo.path(),
            "copilot/tracked-collision",
            &test_config(&temp),
            &registry,
            copy_worktree_includes,
        )
        .unwrap_err();

        assert!(format!("{error:#}").contains("tracked target file"));
        assert!(!expected_path.exists());
        assert!(!branch_exists(repo.path(), "copilot/tracked-collision").unwrap());
    }

    #[test]
    fn worktreeinclude_copies_only_matching_ignored_files() {
        let repo = repository();
        let destination = tempfile::tempdir().unwrap();
        git(destination.path(), &["init"]);
        fs::write(
            repo.path().join(".gitignore"),
            ".env\ncache/\nignored.txt\n",
        )
        .unwrap();
        fs::write(
            repo.path().join(".worktreeinclude"),
            ".env\ncache/\n!cache/skip.txt\ntracked.txt\n",
        )
        .unwrap();
        fs::write(repo.path().join(".env"), "secret").unwrap();
        fs::create_dir_all(repo.path().join("cache")).unwrap();
        fs::write(repo.path().join("cache").join("keep.txt"), "keep").unwrap();
        fs::write(repo.path().join("cache").join("skip.txt"), "skip").unwrap();
        fs::write(repo.path().join("ignored.txt"), "not included").unwrap();

        copy_worktree_includes(repo.path(), destination.path()).unwrap();

        assert_eq!(
            fs::read_to_string(destination.path().join(".env")).unwrap(),
            "secret"
        );
        assert!(destination.path().join("cache").join("keep.txt").exists());
        assert!(!destination.path().join("cache").join("skip.txt").exists());
        assert!(!destination.path().join("ignored.txt").exists());
        assert!(!destination.path().join("tracked.txt").exists());
    }
    /// The count is what tells a long copy apart from a hang; without it a worktree
    /// copying a virtualenv sat silent for over an hour and looked stuck.
    #[test]
    fn worktreeinclude_progress_counts_the_files_it_copies_out_of_those_selected() {
        let repo = repository();
        let destination = tempfile::tempdir().unwrap();
        git(destination.path(), &["init"]);
        fs::write(repo.path().join(".gitignore"), "cache/\n").unwrap();
        fs::write(repo.path().join(".worktreeinclude"), "cache/\n").unwrap();
        fs::create_dir_all(repo.path().join("cache")).unwrap();
        for name in ["a.txt", "b.txt", "c.txt"] {
            fs::write(repo.path().join("cache").join(name), name).unwrap();
        }
        let progress = CreationProgress::default();
        assert_eq!(progress.stage(), CreationStage::CheckingOut);

        copy_worktree_includes_reporting(repo.path(), destination.path(), &progress).unwrap();

        assert_eq!(
            progress.stage(),
            CreationStage::CopyingIncludes {
                copied: 3,
                total: 3
            }
        );
    }
    /// Listing the ignored files is its own wait: in a repository with a large ignored
    /// tree it ran for tens of seconds while the tab still claimed to be checking out.
    #[test]
    fn worktreeinclude_progress_reports_finding_files_once_listing_has_begun() {
        let repo = repository();
        let destination = tempfile::tempdir().unwrap();
        git(destination.path(), &["init"]);
        fs::write(repo.path().join(".worktreeinclude"), "nothing-matches/\n").unwrap();
        let progress = CreationProgress::default();

        copy_worktree_includes_reporting(repo.path(), destination.path(), &progress).unwrap();

        assert_eq!(progress.stage(), CreationStage::FindingIncludes);
    }
}

//! Recovering the tabs a CST instance had open, after it quits or crashes.
//!
//! Each instance writes its workspace down — which sessions, in which tabs, laid out
//! how — whenever that changes, in a file of its own. Copilot keeps sessions on disk,
//! so all a later instance needs to bring a workspace back is that list: it resumes each
//! session and regroups the tabs, the same way the update restart does.
//!
//! One file per instance rather than one shared record, because several CSTs are often
//! open at once and a shared record would only ever hold whichever wrote last.
//!
//! Nothing here resumes a session that is running somewhere else. That is decided by
//! Copilot's own lock files, which every running `copilot` holds whoever started it, so
//! a session open in another CST, or in a terminal of its own, is never offered.

use std::path::{Path, PathBuf};
use std::time::Duration;

use anyhow::{Context, Result};
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

use crate::mux::split::SavedWindow;

/// How many past workspaces are kept to choose from.
const KEEP: usize = 10;

/// A live instance rewrites its record at least this often, changed or not.
pub const HEARTBEAT: Duration = Duration::from_secs(60);

/// A record not rewritten for this long belongs to an instance that is gone, even when
/// its process id is still running — ids are reused, and a crashed CST's id can belong
/// to something else a minute later.
const STALE_AFTER: Duration = Duration::from_secs(5 * 60);

/// Past this, a workspace is not worth offering any more.
const MAX_AGE_DAYS: i64 = 30;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RecordedSession {
    pub session_id: String,
    pub cwd: String,
    pub title: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct WorkspaceRecord {
    pub pid: u32,
    /// When the instance started; with the pid, what names its file.
    pub started_at: DateTime<Utc>,
    pub saved_at: DateTime<Utc>,
    #[serde(default)]
    pub launch_dir: Option<String>,
    /// In tab order, and within a tab in reading order.
    pub sessions: Vec<RecordedSession>,
    #[serde(default)]
    pub focused_session_id: Option<String>,
    /// Tabs holding more than one session; every other session had a tab of its own.
    #[serde(default)]
    pub windows: Vec<SavedWindow>,
}

impl WorkspaceRecord {
    /// Whether two records describe the same workspace, whenever they were written.
    pub fn same_workspace(&self, other: &Self) -> bool {
        self.sessions == other.sessions
            && self.focused_session_id == other.focused_session_id
            && self.windows == other.windows
            && self.launch_dir == other.launch_dir
    }

    /// How many tabs these sessions were in.
    pub fn tab_count(&self) -> usize {
        let grouped: usize = self
            .windows
            .iter()
            .map(|window| window.layout.sessions().len().saturating_sub(1))
            .sum();
        self.sessions.len().saturating_sub(grouped)
    }
}

pub fn root() -> PathBuf {
    crate::app_state::state_root().join("workspaces")
}

fn path_in(root: &Path, pid: u32, started_at: DateTime<Utc>) -> PathBuf {
    root.join(format!("{pid}-{}.json", started_at.timestamp_millis()))
}

/// Written whole or not at all, so a crash mid-write cannot leave a record that loses
/// the workspace it was meant to save.
pub fn write_in(root: &Path, record: &WorkspaceRecord) -> Result<()> {
    use std::io::Write;
    std::fs::create_dir_all(root)
        .with_context(|| format!("Failed to create {}", root.display()))?;
    let mut file = tempfile::NamedTempFile::new_in(root)?;
    file.write_all(&serde_json::to_vec_pretty(record)?)?;
    file.as_file().sync_all()?;
    file.persist(path_in(root, record.pid, record.started_at))
        .context("Failed to save the workspace record")?;
    Ok(())
}

pub fn remove_in(root: &Path, pid: u32, started_at: DateTime<Utc>) {
    let _ = std::fs::remove_file(path_in(root, pid, started_at));
}

fn load_all_in(root: &Path) -> Vec<(PathBuf, WorkspaceRecord)> {
    let Ok(entries) = std::fs::read_dir(root) else {
        return Vec::new();
    };
    entries
        .filter_map(Result::ok)
        .map(|entry| entry.path())
        .filter(|path| {
            path.extension()
                .is_some_and(|extension| extension == "json")
        })
        // A record that cannot be read is skipped rather than deleted: it may be a newer
        // CST's, and it is not this one's to throw away.
        .filter_map(|path| {
            let record = serde_json::from_slice(&std::fs::read(&path).ok()?).ok()?;
            Some((path, record))
        })
        .collect()
}

/// A workspace that could be brought back now.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Recoverable {
    pub path: PathBuf,
    pub record: WorkspaceRecord,
    /// The sessions free to resume: not running anywhere, and not already open here.
    pub sessions: Vec<RecordedSession>,
}

/// What a later instance would offer to recover, newest first. Records past their
/// use are cleaned up on the way.
///
/// `me` is this instance, whose own record is never offered back to it.
pub fn recoverable_in(
    root: &Path,
    me: (u32, DateTime<Utc>),
    now: DateTime<Utc>,
    process_running: impl Fn(u32) -> bool,
    session_running: impl Fn(&str) -> bool,
    open_here: impl Fn(&str) -> bool,
) -> Vec<Recoverable> {
    let mut records = load_all_in(root);
    records.sort_by_key(|(_, record)| std::cmp::Reverse(record.saved_at));
    let mut offered = Vec::new();
    for (kept, (path, record)) in records.into_iter().enumerate() {
        if (record.pid, record.started_at) == me || is_live(&record, now, &process_running) {
            continue;
        }
        if kept >= KEEP || now - record.saved_at > chrono::Duration::days(MAX_AGE_DAYS) {
            let _ = std::fs::remove_file(&path);
            continue;
        }
        let sessions: Vec<RecordedSession> = record
            .sessions
            .iter()
            .filter(|session| {
                !session_running(&session.session_id) && !open_here(&session.session_id)
            })
            .cloned()
            .collect();
        if !sessions.is_empty() {
            offered.push(Recoverable {
                path,
                record,
                sessions,
            });
        }
    }
    offered
}

/// Whether the instance that wrote a record is still running.
fn is_live(
    record: &WorkspaceRecord,
    now: DateTime<Utc>,
    process_running: impl Fn(u32) -> bool,
) -> bool {
    let stale_after = chrono::Duration::from_std(STALE_AFTER).unwrap_or_default();
    process_running(record.pid) && now - record.saved_at < stale_after
}

/// The sessions open right now in a CST other than `me`.
///
/// Copilot's lock file says the same thing only once a session has run a turn since it
/// was opened, so a session resumed into another CST's tab and not used yet looks closed
/// by that measure. These records are written as soon as a tab opens, whether anything
/// has happened in it or not.
pub fn hosted_elsewhere_in(
    root: &Path,
    me: (u32, DateTime<Utc>),
    now: DateTime<Utc>,
    process_running: impl Fn(u32) -> bool,
) -> std::collections::HashSet<String> {
    load_all_in(root)
        .into_iter()
        .map(|(_, record)| record)
        .filter(|record| {
            (record.pid, record.started_at) != me && is_live(record, now, &process_running)
        })
        .flat_map(|record| record.sessions)
        .map(|session| session.session_id)
        .collect()
}

/// "12m ago", "3h ago", "2d ago".
pub fn ago(when: DateTime<Utc>, now: DateTime<Utc>) -> String {
    let elapsed = now.signed_duration_since(when);
    if elapsed.num_minutes() < 1 {
        "just now".to_string()
    } else if elapsed.num_minutes() < 60 {
        format!("{}m ago", elapsed.num_minutes())
    } else if elapsed.num_hours() < 24 {
        format!("{}h ago", elapsed.num_hours())
    } else {
        format!("{}d ago", elapsed.num_days())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::mux::split::SavedNode;

    fn session(id: &str) -> RecordedSession {
        RecordedSession {
            session_id: id.to_string(),
            cwd: "/work".to_string(),
            title: format!("title {id}"),
        }
    }

    fn record(
        pid: u32,
        saved_minutes_ago: i64,
        ids: &[&str],
        now: DateTime<Utc>,
    ) -> WorkspaceRecord {
        WorkspaceRecord {
            pid,
            started_at: now - chrono::Duration::hours(1),
            saved_at: now - chrono::Duration::minutes(saved_minutes_ago),
            launch_dir: Some("/work".to_string()),
            sessions: ids.iter().map(|id| session(id)).collect(),
            focused_session_id: ids.first().map(|id| id.to_string()),
            windows: Vec::new(),
        }
    }

    #[test]
    fn a_closed_instances_workspace_is_offered_newest_first() {
        let root = tempfile::tempdir().unwrap();
        let now = Utc::now();
        write_in(root.path(), &record(10, 30, &["a", "b"], now)).unwrap();
        write_in(root.path(), &record(11, 5, &["c"], now)).unwrap();
        let me = (99, now);
        let offered = recoverable_in(root.path(), me, now, |_| false, |_| false, |_| false);
        assert_eq!(offered.len(), 2);
        assert_eq!(offered[0].record.pid, 11);
        assert_eq!(offered[1].sessions, vec![session("a"), session("b")]);
    }

    #[test]
    fn a_running_instances_workspace_and_this_ones_own_are_not_offered() {
        let root = tempfile::tempdir().unwrap();
        let now = Utc::now();
        let mine = record(1, 0, &["mine"], now);
        write_in(root.path(), &mine).unwrap();
        write_in(root.path(), &record(2, 1, &["live"], now)).unwrap();
        // Same pid as a running process, but silent for too long: a reused id.
        write_in(root.path(), &record(3, 20, &["gone"], now)).unwrap();
        let offered = recoverable_in(
            root.path(),
            (1, mine.started_at),
            now,
            |pid| pid == 2 || pid == 3,
            |_| false,
            |_| false,
        );
        assert_eq!(offered.len(), 1);
        assert_eq!(offered[0].record.pid, 3);
    }

    #[test]
    fn sessions_running_anywhere_or_open_here_are_never_offered_for_resuming() {
        let root = tempfile::tempdir().unwrap();
        let now = Utc::now();
        write_in(
            root.path(),
            &record(10, 30, &["free", "elsewhere", "here"], now),
        )
        .unwrap();
        write_in(root.path(), &record(11, 30, &["elsewhere"], now)).unwrap();
        let offered = recoverable_in(
            root.path(),
            (99, now),
            now,
            |_| false,
            |id| id == "elsewhere",
            |id| id == "here",
        );
        assert_eq!(
            offered.len(),
            1,
            "a workspace with nothing left to resume is not offered"
        );
        assert_eq!(offered[0].sessions, vec![session("free")]);
    }

    #[test]
    fn old_and_surplus_workspaces_are_cleaned_up() {
        let root = tempfile::tempdir().unwrap();
        let now = Utc::now();
        write_in(root.path(), &record(1, 60 * 24 * 40, &["ancient"], now)).unwrap();
        for pid in 100..(100 + KEEP as u32 + 2) {
            write_in(root.path(), &record(pid, i64::from(pid), &["s"], now)).unwrap();
        }
        let offered = recoverable_in(root.path(), (9, now), now, |_| false, |_| false, |_| false);
        assert_eq!(offered.len(), KEEP);
        assert_eq!(std::fs::read_dir(root.path()).unwrap().count(), KEEP);
    }

    #[test]
    fn a_workspace_counts_a_tab_of_several_sessions_once() {
        let now = Utc::now();
        let mut workspace = record(1, 0, &["a", "b", "c"], now);
        assert_eq!(workspace.tab_count(), 3);
        workspace.windows = vec![SavedWindow {
            layout: SavedNode::Split {
                direction: crate::mux::SplitDirection::Columns,
                children: vec![SavedNode::Pane("a".into()), SavedNode::Pane("b".into())],
                weights: vec![100, 100],
            },
            zoomed: false,
            focused_session_id: None,
        }];
        assert_eq!(workspace.tab_count(), 2);
    }

    #[test]
    fn only_sessions_in_another_running_instance_count_as_open_elsewhere() {
        // A tab opened and not used yet has no Copilot lock, and another CST that took
        // that for "closed" would send the session's replies to the inbox.
        let root = tempfile::tempdir().unwrap();
        let now = Utc::now();
        let mine = record(1, 0, &["mine"], now);
        write_in(root.path(), &mine).unwrap();
        write_in(root.path(), &record(2, 1, &["other-a", "other-b"], now)).unwrap();
        write_in(root.path(), &record(3, 0, &["crashed"], now)).unwrap();
        write_in(root.path(), &record(4, 20, &["reused-pid"], now)).unwrap();

        let open = hosted_elsewhere_in(root.path(), (1, mine.started_at), now, |pid| pid != 3);

        let mut open: Vec<_> = open.into_iter().collect();
        open.sort();
        assert_eq!(open, vec!["other-a", "other-b"]);
    }
}

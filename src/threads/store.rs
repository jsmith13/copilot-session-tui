//! Where thread subscriptions live on disk.
//!
//! Unlike every other piece of CST state, this file has **two writers**: the running TUI
//! and the short-lived `cst thread post` an agent runs in its own shell. Every
//! read-modify-write therefore goes through [`update_in`], which holds an `fs4` advisory
//! lock across the whole cycle — the same approach `events::hooks::append_event` uses for
//! the lifecycle journal. Without it, an agent posting while the doorbell is recording a
//! watermark would silently drop one of the two changes.
//!
//! The lock is a separate `threads.lock` file rather than the state file itself, because
//! saving replaces the state file by rename and a lock held on the old inode would
//! protect nothing.
//!
//! Kept beside `app-state.json` and out of `config.json` for the reasons spelled out in
//! [`crate::app_state`]: the config file is hand-edited and polled every second, so
//! machine-managed bookkeeping does not belong there.

use anyhow::{Context, Result};
use fs4::FileExt;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::collections::BTreeMap;
use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};

use super::{Notice, NoticeStatus, PendingDelivery, Subscription, ThreadRef};

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct ThreadState {
    #[serde(default)]
    pub subscriptions: Vec<Subscription>,

    /// Every notice CST still has something to do about.
    ///
    /// Covers the whole life of one: decided on, written into a composer, or waiting for
    /// the user. Keeping all three here rather than only the last is what lets a restart
    /// resume instead of forgetting — the comment behind a notice is marked seen as soon
    /// as it is planned, so anything lost at that point is never mentioned again.
    #[serde(default)]
    pub notices: Vec<Notice>,

    /// Written by the version before notices had a status. Read, never written.
    #[serde(default, skip_serializing)]
    pending: Vec<PendingDelivery>,

    /// Per-thread `ETag`, keyed by canonical URL.
    ///
    /// What makes polling every watched thread affordable: a request carrying one of
    /// these comes back `304` without being charged against the rate limit. Losing the
    /// map costs one full fetch per thread, not correctness, which is why it is fine
    /// for a corrupt file to start over.
    #[serde(default)]
    pub cursors: BTreeMap<String, String>,

    /// Anything a newer CST wrote that this one does not understand, so downgrading does
    /// not silently discard it. Same approach as `UserConfig::extra`.
    #[serde(flatten)]
    extra: BTreeMap<String, Value>,
}

impl ThreadState {
    /// Every distinct thread that some session is actively subscribed to.
    ///
    /// The doorbell asks for this to decide whether it has any reason to poll at all.
    pub fn active_threads(&self) -> Vec<ThreadRef> {
        let mut threads: Vec<ThreadRef> = Vec::new();
        for subscription in self.subscriptions.iter().filter(|s| s.is_active()) {
            if !threads.contains(&subscription.thread) {
                threads.push(subscription.thread.clone());
            }
        }
        threads
    }

    /// Whether anything at all is subscribed, so an unused install never polls GitHub.
    pub fn has_active_subscription(&self) -> bool {
        self.subscriptions.iter().any(Subscription::is_active)
    }

    pub fn subscription_mut(
        &mut self,
        session_id: &str,
        thread: &ThreadRef,
    ) -> Option<&mut Subscription> {
        self.subscriptions
            .iter_mut()
            .find(|s| s.session_id == session_id && &s.thread == thread)
    }

    /// Subscribe a session, returning whether this was new.
    ///
    /// Idempotent: taking part in a thread twice is the normal case, and it must not
    /// create a second row that would then wake the session twice per comment.
    pub fn subscribe(&mut self, session_id: &str, thread: ThreadRef) -> bool {
        if let Some(existing) = self.subscription_mut(session_id, &thread) {
            // Re-subscribing is how a paused thread is resumed.
            existing.state = super::SubscriptionState::Active;
            return false;
        }
        self.subscriptions
            .push(Subscription::new(session_id, thread));
        true
    }

    /// Drop one session's interest, leaving every other subscriber untouched.
    ///
    /// This is `leave`, not `close`: an agent deciding a thread no longer concerns it
    /// must not silence the thread for the agent still working on it.
    pub fn leave(&mut self, session_id: &str, thread: &ThreadRef) -> bool {
        let before = self.subscriptions.len();
        self.subscriptions
            .retain(|s| !(s.session_id == session_id && &s.thread == thread));
        self.notices
            .retain(|n| !(n.session_id == session_id && &n.thread == thread));
        self.subscriptions.len() != before
    }

    /// Drop every local subscription to a thread, for when the correspondence is over.
    pub fn close(&mut self, thread: &ThreadRef) -> usize {
        let before = self.subscriptions.len();
        self.subscriptions.retain(|s| &s.thread != thread);
        self.notices.retain(|n| &n.thread != thread);
        before - self.subscriptions.len()
    }

    pub fn subscriptions_for(&self, session_id: &str) -> Vec<&Subscription> {
        self.subscriptions
            .iter()
            .filter(|s| s.session_id == session_id)
            .collect()
    }

    /// Record a notice, or update the one already tracking this thread.
    ///
    /// One notice per session per thread. A thread that moves again while its notice is
    /// still unread has nothing new to say — the notice is the same link — so the newer
    /// arrival refreshes what it points at without resetting when it was planned. That
    /// keeps a busy thread from pushing out its own deadline indefinitely.
    pub fn record_notice(&mut self, notice: Notice) {
        if let Some(existing) = self
            .notices
            .iter_mut()
            .find(|n| n.session_id == notice.session_id && n.thread == notice.thread)
        {
            existing.comment_url = notice.comment_url;
            existing.author = notice.author.or(existing.author.take());
            // A queued or delivered notice keeps its status: it already says "go and
            // read this thread", and walking it back to planned would send the same
            // link a second time.
            //
            // A held notice is different. Being unreachable or throttled describes the
            // moment it was held, and a fresh wake is evidence that moment has passed —
            // left alone, one held notice would silently absorb every later wake on the
            // thread for as long as nobody opened the inbox. The clock restarts with
            // it, or a notice held days ago would already be past the patience deadline
            // and be held again on the first pass that found the pane busy.
            //
            // That includes a question about an outsider's comment left by an older
            // CST. Those comments are ignored now, so the question no longer needs an
            // answer, and it must not stand between the session and our own reply.
            let promote = notice.status == NoticeStatus::Planned
                && matches!(existing.status, NoticeStatus::Waiting { .. });
            if promote {
                existing.status = NoticeStatus::Planned;
                existing.planned_at = notice.planned_at;
            }
            return;
        }
        self.notices.push(notice);
    }

    pub fn notice_mut(&mut self, session_id: &str, thread: &ThreadRef) -> Option<&mut Notice> {
        self.notices
            .iter_mut()
            .find(|n| n.session_id == session_id && &n.thread == thread)
    }

    pub fn drop_notice(&mut self, session_id: &str, thread: &ThreadRef) {
        self.notices
            .retain(|n| !(n.session_id == session_id && &n.thread == thread));
    }

    /// Everything the user is being asked about.
    pub fn waiting_for_user(&self) -> Vec<&Notice> {
        self.notices
            .iter()
            .filter(|n| n.is_waiting_for_user())
            .collect()
    }

    /// Fold in anything written before notices had a status, and recover from a restart.
    ///
    /// A notice recorded as sent was sitting in a composer that no longer exists, so it
    /// goes back to planned and is written again. Without this the comment behind it is
    /// already marked seen and would never be mentioned a second time.
    fn adopt_legacy(&mut self) {
        for legacy in std::mem::take(&mut self.pending) {
            self.record_notice(Notice {
                session_id: legacy.session_id,
                thread: legacy.thread,
                comment_url: legacy.comment_url,
                author: legacy.author,
                status: NoticeStatus::Waiting {
                    reason: legacy.reason,
                },
                planned_at: legacy.arrived_at,
            });
        }
    }

    /// Put sent notices back in the queue, because the composer holding them is gone.
    ///
    /// Only correct at startup. Running it on every read would undo the sent marking
    /// within a single session and write the same notice over and over, which is the
    /// bug this whole mechanism exists to prevent.
    fn resume_after_restart(&mut self) {
        for notice in &mut self.notices {
            if notice.status == NoticeStatus::Sent {
                notice.status = NoticeStatus::Planned;
            }
        }
    }
}

/// Read the state, treating every failure as "nothing subscribed yet".
///
/// Deliberately infallible, for the same reason as [`crate::app_state::load_in`]: a
/// corrupt file must never stop CST from starting. The cost is that a damaged file loses
/// subscriptions, which the agents can recreate by posting again.
pub fn load_in(root: &Path) -> ThreadState {
    let mut state: ThreadState = fs::read_to_string(state_path(root))
        .ok()
        .and_then(|content| serde_json::from_str(&content).ok())
        .unwrap_or_default();
    state.adopt_legacy();
    state
}

/// Load once at startup, putting anything a previous run left mid-flight back in hand.
pub fn resume_in(root: &Path) -> Result<()> {
    update_in(root, |state| state.resume_after_restart())
}

/// Read, change and save under one lock held for the whole cycle.
///
/// The only sanctioned way to modify the state. Taking the lock around just the write
/// would still lose changes: the losing writer would have read a copy that predates the
/// winner's change and then written it back wholesale.
pub fn update_in<T>(root: &Path, change: impl FnOnce(&mut ThreadState) -> T) -> Result<T> {
    let _guard = StateLock::acquire(root)?;
    let mut state = load_in(root);
    let outcome = change(&mut state);
    write_in(root, &state)?;
    Ok(outcome)
}

fn write_in(root: &Path, state: &ThreadState) -> Result<()> {
    let path = state_path(root);
    fs::create_dir_all(root)
        .with_context(|| format!("Failed to create state directory: {}", root.display()))?;
    let content = serde_json::to_vec_pretty(state)?;
    let mut temp = tempfile::NamedTempFile::new_in(root)?;
    temp.as_file_mut().write_all(&content)?;
    temp.as_file_mut().sync_all()?;
    temp.persist(&path)
        .map_err(|error| error.error)
        .with_context(|| format!("Failed to replace thread state: {}", path.display()))?;
    Ok(())
}

/// Blocking, unlike `ConfigLock`.
///
/// Refusing with "another process is saving" would be wrong here: `cst thread post` is
/// run by an agent mid-turn with no way to retry sensibly, and the contention window is
/// a single small file write.
struct StateLock {
    file: fs::File,
}

impl StateLock {
    fn acquire(root: &Path) -> Result<Self> {
        fs::create_dir_all(root)
            .with_context(|| format!("Failed to create state directory: {}", root.display()))?;
        let path = lock_path(root);
        let file = fs::OpenOptions::new()
            .create(true)
            .truncate(false)
            .read(true)
            .write(true)
            .open(&path)
            .with_context(|| format!("Failed to open thread lock {}", path.display()))?;
        FileExt::lock(&file).with_context(|| format!("Failed to lock {}", path.display()))?;
        Ok(Self { file })
    }
}

impl Drop for StateLock {
    fn drop(&mut self) {
        let _ = FileExt::unlock(&self.file);
    }
}

fn state_path(root: &Path) -> PathBuf {
    root.join("threads.json")
}

fn lock_path(root: &Path) -> PathBuf {
    root.join("threads.lock")
}

/// Beside `app-state.json`, and reusing its root so the two never drift apart.
pub fn state_root() -> PathBuf {
    crate::app_state::state_root()
}

#[cfg(test)]
mod tests {
    use super::super::{PendingReason, ThreadKind};
    use super::*;
    use chrono::{DateTime, Utc};

    fn thread(number: u64) -> ThreadRef {
        ThreadRef {
            host: "github.com".to_string(),
            owner: "o".to_string(),
            repo: "r".to_string(),
            number,
            kind: ThreadKind::Issue,
        }
    }

    #[test]
    fn a_machine_with_no_subscriptions_never_gives_the_doorbell_a_reason_to_poll() {
        let temp = tempfile::tempdir().unwrap();
        let state = load_in(temp.path());

        assert!(!state.has_active_subscription());
        assert!(state.active_threads().is_empty());
    }

    #[test]
    fn taking_part_in_a_thread_twice_does_not_wake_the_session_twice_per_comment() {
        let mut state = ThreadState::default();

        assert!(state.subscribe("session-a", thread(1)));
        assert!(!state.subscribe("session-a", thread(1)));
        assert_eq!(state.subscriptions.len(), 1);
    }

    #[test]
    fn leaving_a_thread_silences_it_for_one_session_and_not_for_the_other() {
        // An agent deciding a thread no longer concerns it must not cut off the agent
        // still working on it.
        let mut state = ThreadState::default();
        state.subscribe("session-a", thread(1));
        state.subscribe("session-b", thread(1));

        assert!(state.leave("session-a", &thread(1)));

        assert_eq!(state.subscriptions_for("session-a").len(), 0);
        assert_eq!(state.subscriptions_for("session-b").len(), 1);
        assert_eq!(
            state.active_threads().len(),
            1,
            "the thread is still watched"
        );
    }

    #[test]
    fn closing_a_thread_ends_it_for_everyone_unlike_leaving_it() {
        let mut state = ThreadState::default();
        state.subscribe("session-a", thread(1));
        state.subscribe("session-b", thread(1));
        state.subscribe("session-a", thread(2));

        assert_eq!(state.close(&thread(1)), 2);

        assert!(state.active_threads().contains(&thread(2)));
        assert!(!state.active_threads().contains(&thread(1)));
    }

    #[test]
    fn one_thread_that_keeps_moving_stays_one_ageing_item_rather_than_a_pile() {
        let mut state = ThreadState::default();
        let planned = chrono::Utc::now() - chrono::Duration::days(3);
        for id in 1..=4 {
            state.record_notice(Notice {
                session_id: "session-a".to_string(),
                thread: thread(1),
                comment_url: format!("https://github.com/o/r/issues/1#issuecomment-{id}"),
                status: NoticeStatus::Waiting {
                    reason: PendingReason::SessionClosed,
                },
                author: None,
                planned_at: planned,
            });
        }

        let pending = state.waiting_for_user();
        assert_eq!(pending.len(), 1);
        // The age must survive the collapse, or a long wait would look brand new.
        assert_eq!(pending[0].planned_at, planned);
        assert!(
            pending[0].comment_url.ends_with("-4"),
            "newest comment wins"
        );
    }

    fn notice(id: u64, status: NoticeStatus, planned_at: DateTime<Utc>) -> Notice {
        Notice {
            session_id: "session-a".to_string(),
            thread: thread(1),
            comment_url: format!("https://github.com/o/r/issues/1#issuecomment-{id}"),
            author: Some("tupini07".to_string()),
            status,
            planned_at,
        }
    }

    #[test]
    fn a_wake_arriving_while_a_notice_is_held_puts_it_back_in_the_queue_with_a_fresh_clock() {
        // The reported failure: a notice held three days earlier absorbed every later
        // reply on the thread, so a session that was open and waiting never heard of
        // any of them.
        for reason in [PendingReason::SessionClosed, PendingReason::Throttled] {
            let mut state = ThreadState::default();
            let held_at = Utc::now() - chrono::Duration::days(3);
            let woken_at = Utc::now();
            state.record_notice(notice(1, NoticeStatus::Waiting { reason }, held_at));

            state.record_notice(notice(2, NoticeStatus::Planned, woken_at));

            let current = state.notice_mut("session-a", &thread(1)).unwrap();
            assert_eq!(current.status, NoticeStatus::Planned, "held as {reason:?}");
            // An old clock would put it straight past the patience deadline and have
            // it held again on the first pass that found the pane busy.
            assert_eq!(current.planned_at, woken_at, "held as {reason:?}");
            assert!(current.comment_url.ends_with("-2"));
            assert!(state.waiting_for_user().is_empty());
        }
    }

    #[test]
    fn a_question_about_an_outsider_left_by_an_older_cst_does_not_swallow_our_own_reply() {
        // Outsiders' comments are ignored now, but a threads.json written before that
        // still holds the questions asked about them. Left alone, one would absorb
        // every later wake on its thread, which is the bug this store was fixed for.
        let mut state = ThreadState::default();
        let mut legacy = notice(
            1,
            NoticeStatus::Waiting {
                reason: PendingReason::ForeignAuthor,
            },
            Utc::now() - chrono::Duration::days(3),
        );
        legacy.author = Some("a-stranger".to_string());
        state.record_notice(legacy);

        state.record_notice(notice(2, NoticeStatus::Planned, Utc::now()));

        let current = state.notice_mut("session-a", &thread(1)).unwrap();
        assert_eq!(current.status, NoticeStatus::Planned);
        assert_eq!(current.author.as_deref(), Some("tupini07"));
        assert!(current.comment_url.ends_with("-2"));
        assert!(state.waiting_for_user().is_empty());
    }

    #[test]
    fn a_wake_arriving_while_a_notice_is_queued_or_delivered_does_not_send_the_link_twice() {
        for status in [NoticeStatus::Planned, NoticeStatus::Sent] {
            let mut state = ThreadState::default();
            let first = Utc::now() - chrono::Duration::minutes(2);
            state.record_notice(notice(1, status, first));

            state.record_notice(notice(2, NoticeStatus::Planned, Utc::now()));

            let current = state.notice_mut("session-a", &thread(1)).unwrap();
            assert_eq!(current.status, status);
            assert_eq!(current.planned_at, first, "{status:?} keeps its deadline");
        }
    }

    #[test]
    fn a_change_is_readable_by_the_next_process_to_open_the_file() {
        let temp = tempfile::tempdir().unwrap();
        update_in(temp.path(), |state| {
            state.subscribe("session-a", thread(7));
        })
        .unwrap();

        let reloaded = load_in(temp.path());
        assert_eq!(reloaded.subscriptions.len(), 1);
        assert_eq!(reloaded.subscriptions[0].thread, thread(7));
    }

    /// The shape on disk, pinned.
    ///
    /// Asserted on the literal JSON rather than a round trip, because the point of the
    /// status being persisted is that another build — and a human reading the file —
    /// can tell what is still owed to a session. A round trip would keep passing while
    /// the field quietly renamed itself.
    #[test]
    fn a_notice_is_saved_with_its_status_as_a_readable_column() {
        let temp = tempfile::tempdir().unwrap();
        update_in(temp.path(), |state| {
            state.record_notice(Notice {
                session_id: "session-a".to_string(),
                thread: thread(1),
                comment_url: "https://github.com/o/r/issues/1".to_string(),
                author: None,
                status: NoticeStatus::Planned,
                planned_at: chrono::Utc::now(),
            });
        })
        .unwrap();

        let saved = fs::read_to_string(state_path(temp.path())).unwrap();
        assert!(saved.contains(r#""state": "planned""#), "got: {saved}");

        update_in(temp.path(), |state| {
            state.notice_mut("session-a", &thread(1)).unwrap().status = NoticeStatus::Waiting {
                reason: PendingReason::Throttled,
            };
        })
        .unwrap();

        let saved = fs::read_to_string(state_path(temp.path())).unwrap();
        assert!(saved.contains(r#""state": "waiting""#), "got: {saved}");
        assert!(saved.contains(r#""reason": "throttled""#), "got: {saved}");
        assert_eq!(
            load_in(temp.path()).notices[0].reason(),
            Some(PendingReason::Throttled)
        );
    }

    #[test]
    fn a_corrupt_file_is_treated_as_a_fresh_start_rather_than_failing_startup() {
        let temp = tempfile::tempdir().unwrap();
        fs::write(state_path(temp.path()), "{not json").unwrap();

        assert_eq!(load_in(temp.path()), ThreadState::default());

        // And it repairs itself on the next write rather than staying broken.
        update_in(temp.path(), |state| state.subscribe("session-a", thread(1))).unwrap();
        assert_eq!(load_in(temp.path()).subscriptions.len(), 1);
    }

    #[test]
    fn a_field_written_by_a_newer_cst_survives_a_save_by_an_older_one() {
        let temp = tempfile::tempdir().unwrap();
        fs::create_dir_all(temp.path()).unwrap();
        fs::write(
            state_path(temp.path()),
            r#"{"subscriptions":[],"something_newer":{"kept":true}}"#,
        )
        .unwrap();

        update_in(temp.path(), |state| state.subscribe("session-a", thread(1))).unwrap();

        let written = fs::read_to_string(state_path(temp.path())).unwrap();
        assert!(written.contains("something_newer"), "got: {written}");
    }

    #[test]
    fn a_thread_cursor_survives_a_restart_so_the_first_poll_is_still_free() {
        let temp = tempfile::tempdir().unwrap();
        update_in(temp.path(), |state| {
            state
                .cursors
                .insert(thread(1).url(), "W/\"abc\"".to_string());
        })
        .unwrap();

        assert_eq!(
            load_in(temp.path()).cursors.get(&thread(1).url()),
            Some(&"W/\"abc\"".to_string())
        );
    }
}

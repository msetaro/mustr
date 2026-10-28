//! Reviews of agents' work, kept on this machine: comments on an agent's diff never go to a
//! forge, so mustr is where they live (`mustr-docs/PLAN.md` §7). One JSON file per thread and
//! scope in `<data dir>/agent-reviews/`, holding the pending review and the comments already
//! sent to the agent, replaced atomically on every change. `reviewed.json` next to them records
//! when each thread's changes were last opened, so the inbox can tell new work from seen work.

use anyhow::{Context as _, Result, bail};
use gpui::{App, AppContext as _};
use parking_lot::Mutex;
use review::{DiffSide, PendingReview, ReviewScope, ReviewThread, ThreadComment};
use serde::{Deserialize, Serialize};
use std::{
    collections::HashMap,
    path::{Path, PathBuf},
    sync::Arc,
};
use time::{OffsetDateTime, format_description::well_known::Rfc3339};

const FORMAT_VERSION: u32 = 1;
const REVIEWED_FILE: &str = "reviewed.json";

pub fn agent_reviews_dir() -> PathBuf {
    paths::data_dir().join("agent-reviews")
}

/// A comment that was sent to the agent, shown afterwards as a local thread.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SentComment {
    pub id: String,
    pub path: String,
    pub side: DiffSide,
    pub line: u32,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub start_line: Option<u32>,
    pub body: String,
    /// RFC 3339.
    pub sent_at: String,
}

impl SentComment {
    pub fn to_thread(&self) -> ReviewThread {
        ReviewThread {
            id: self.id.clone(),
            path: self.path.clone(),
            line: Some(self.line),
            side: self.side,
            is_resolved: false,
            is_outdated: false,
            can_resolve: false,
            comments: vec![ThreadComment {
                id: self.id.clone(),
                author: Some("You".to_string()),
                body: self.body.clone(),
                created_at: OffsetDateTime::parse(&self.sent_at, &Rfc3339).ok(),
                edited: false,
                can_edit: false,
            }],
            comment_count: 1,
            sent_to_agent: true,
        }
    }
}

#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
struct AgentReviewFile {
    version: u32,
    thread_id: String,
    scope: String,
    #[serde(default)]
    pending: PendingReview,
    #[serde(default)]
    sent: Vec<SentComment>,
}

impl AgentReviewFile {
    fn is_empty(&self) -> bool {
        self.pending.is_empty() && self.sent.is_empty()
    }
}

#[derive(Default)]
struct StoreState {
    contents: Option<AgentReviewFile>,
    /// The number of the latest change, and of the latest one written. An older write that
    /// finishes after a newer one is dropped.
    latest_change: u64,
    latest_written: u64,
}

/// The review of one scope of one agent thread.
#[derive(Clone)]
pub struct AgentReviewStore {
    path: PathBuf,
    thread_id: String,
    scope: String,
    state: Arc<Mutex<StoreState>>,
}

impl AgentReviewStore {
    pub fn new(dir: &Path, thread_id: &str, scope: ReviewScope) -> Self {
        let scope = scope.key();
        Self {
            path: dir.join(format!("{}~{scope}.json", escape(thread_id))),
            thread_id: thread_id.to_string(),
            scope,
            state: Arc::default(),
        }
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    /// Reads the file, once. Blocks; call it off the main thread. A file mustr can't read is
    /// moved aside rather than overwritten, so its comments can still be recovered by hand.
    pub fn load(&self) -> Result<(PendingReview, Vec<SentComment>)> {
        let mut state = self.state.lock();
        if let Some(contents) = &state.contents {
            return Ok((contents.pending.clone(), contents.sent.clone()));
        }
        let empty = AgentReviewFile {
            version: FORMAT_VERSION,
            thread_id: self.thread_id.clone(),
            scope: self.scope.clone(),
            ..AgentReviewFile::default()
        };
        let result = match std::fs::read(&self.path) {
            Ok(bytes) => match self.parse(&bytes) {
                Ok(contents) => Ok(contents),
                Err(error) => {
                    let aside = self.path.with_extension(format!(
                        "unreadable-{}.json",
                        OffsetDateTime::now_utc().unix_timestamp()
                    ));
                    std::fs::rename(&self.path, &aside).with_context(|| {
                        format!("moving the unreadable {} aside", self.path.display())
                    })?;
                    Err(error.context(format!(
                        "the saved review couldn't be read; it was kept in {}",
                        aside.display()
                    )))
                }
            },
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(empty.clone()),
            Err(error) => Err(error).with_context(|| format!("reading {}", self.path.display())),
        };
        let contents = match result {
            Ok(contents) => contents,
            Err(error) => {
                state.contents = Some(empty);
                return Err(error);
            }
        };
        let loaded = (contents.pending.clone(), contents.sent.clone());
        state.contents = Some(contents);
        Ok(loaded)
    }

    fn parse(&self, bytes: &[u8]) -> Result<AgentReviewFile> {
        let file: AgentReviewFile =
            serde_json::from_slice(bytes).context("not a mustr agent review file")?;
        if file.version != FORMAT_VERSION {
            bail!("agent review file version {} isn't supported", file.version);
        }
        if file.thread_id != self.thread_id || file.scope != self.scope {
            bail!("the file holds the review of another thread");
        }
        Ok(file)
    }

    pub fn sent(&self) -> Vec<SentComment> {
        self.state
            .lock()
            .contents
            .as_ref()
            .map(|contents| contents.sent.clone())
            .unwrap_or_default()
    }

    pub fn save_pending(&self, pending: PendingReview, cx: &mut App) {
        self.change(cx, move |contents| contents.pending = pending);
    }

    pub fn record_sent(&self, sent: Vec<SentComment>, cx: &mut App) {
        self.change(cx, move |contents| contents.sent.extend(sent));
    }

    /// Applies a change in memory, then writes the file in the background. The latest change
    /// always wins, even if an earlier write finishes later.
    fn change(&self, cx: &mut App, change: impl FnOnce(&mut AgentReviewFile)) {
        let (number, contents) = {
            let mut state = self.state.lock();
            let contents = state.contents.get_or_insert_with(|| AgentReviewFile {
                version: FORMAT_VERSION,
                thread_id: self.thread_id.clone(),
                scope: self.scope.clone(),
                ..AgentReviewFile::default()
            });
            change(contents);
            let contents = contents.clone();
            state.latest_change += 1;
            (state.latest_change, contents)
        };
        let store = self.clone();
        cx.background_spawn(async move {
            let mut state = store.state.lock();
            if state.latest_written > number {
                return;
            }
            match write_atomically(&store.path, (!contents.is_empty()).then_some(&contents)) {
                Ok(()) => state.latest_written = number,
                Err(error) => log::error!("couldn't save the agent review: {error:#}"),
            }
        })
        .detach();
    }

    pub fn sent_threads(&self) -> Vec<ReviewThread> {
        self.sent().iter().map(SentComment::to_thread).collect()
    }
}

/// Replaces `path` with `value` as JSON, or removes it for `None`. Writes a sibling and renames
/// it over the file, so a crash mid-write never leaves half a file behind.
fn write_atomically<T: Serialize>(path: &Path, value: Option<&T>) -> Result<()> {
    let Some(value) = value else {
        return match std::fs::remove_file(path) {
            Err(error) if error.kind() != std::io::ErrorKind::NotFound => {
                Err(error).with_context(|| format!("removing {}", path.display()))
            }
            _ => Ok(()),
        };
    };
    let bytes = serde_json::to_vec_pretty(value)?;
    let dir = path.parent().context("the file has no directory")?;
    std::fs::create_dir_all(dir).with_context(|| format!("creating {}", dir.display()))?;
    let temporary = path.with_extension("json.tmp");
    std::fs::write(&temporary, bytes)
        .with_context(|| format!("writing {}", temporary.display()))?;
    std::fs::rename(&temporary, path).with_context(|| format!("replacing {}", path.display()))
}

/// Letters, digits, `-` and `.` are kept, and every other byte is written as `_xx`, so thread ids
/// (some hold `:` and `%`) make file names that are safe and unique on every platform.
fn escape(part: &str) -> String {
    part.bytes()
        .map(|byte| match byte {
            b'a'..=b'z' | b'A'..=b'Z' | b'0'..=b'9' | b'-' | b'.' => (byte as char).to_string(),
            _ => format!("_{byte:02x}"),
        })
        .collect()
}

/// When each thread's changes were last opened in mustr: the thread's latest completion at the
/// time, by thread id.
#[derive(Clone)]
pub struct ReviewedMarks {
    path: PathBuf,
    writer: Arc<Mutex<()>>,
}

impl ReviewedMarks {
    pub fn new(dir: &Path) -> Self {
        Self {
            path: dir.join(REVIEWED_FILE),
            writer: Arc::default(),
        }
    }

    /// Blocks; call it off the main thread. Unreadable marks count as none.
    pub fn load(&self) -> HashMap<String, String> {
        std::fs::read(&self.path)
            .ok()
            .and_then(|bytes| serde_json::from_slice(&bytes).ok())
            .unwrap_or_default()
    }

    pub fn mark(&self, thread_id: String, completed_at: String, cx: &mut App) {
        let marks = self.clone();
        cx.background_spawn(async move {
            let _writer = marks.writer.lock();
            let mut all = marks.load();
            if all
                .get(&thread_id)
                .is_some_and(|marked| *marked >= completed_at)
            {
                return;
            }
            all.insert(thread_id, completed_at);
            if let Err(error) = write_atomically(&marks.path, Some(&all)) {
                log::error!("couldn't save which agent threads were reviewed: {error:#}");
            }
        })
        .detach();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use gpui::TestAppContext;
    use pretty_assertions::assert_eq;
    use review::{CommentPosition, DraftComment};

    fn pending(body: &str) -> PendingReview {
        PendingReview {
            comments: vec![DraftComment {
                id: "d1".to_string(),
                path: "src/main.rs".to_string(),
                old_path: None,
                position: CommentPosition::Added { new_line: 3 },
                start_line: None,
                body: body.to_string(),
                added: true,
            }],
            ..PendingReview::default()
        }
    }

    fn sent(id: &str) -> SentComment {
        SentComment {
            id: id.to_string(),
            path: "src/main.rs".to_string(),
            side: DiffSide::Right,
            line: 3,
            start_line: None,
            body: "Use the logger.".to_string(),
            sent_at: "2026-10-27T10:00:00Z".to_string(),
        }
    }

    #[gpui::test]
    async fn test_pending_and_sent_comments_survive_a_restart(cx: &mut TestAppContext) {
        let dir = tempfile::tempdir().expect("a temporary directory");
        let thread = "thread:delegated-task:command%3Amcp%3A1";
        let store = AgentReviewStore::new(dir.path(), thread, ReviewScope::Turn(2));
        assert_eq!(
            store.path().file_name().and_then(|name| name.to_str()),
            Some("thread_3adelegated-task_3acommand_253Amcp_253A1~turn-2.json")
        );
        assert_eq!(
            store.load().unwrap(),
            (PendingReview::default(), Vec::new())
        );

        cx.update(|cx| {
            store.save_pending(pending("one"), cx);
            store.save_pending(pending("two"), cx);
            store.record_sent(vec![sent("s1")], cx);
        });
        cx.run_until_parked();

        let reopened = AgentReviewStore::new(dir.path(), thread, ReviewScope::Turn(2));
        assert_eq!(reopened.load().unwrap(), (pending("two"), vec![sent("s1")]));
        assert_eq!(
            reopened.sent_threads()[0].comments[0].author.as_deref(),
            Some("You")
        );
        assert!(reopened.sent_threads()[0].sent_to_agent);
        // Another scope of the same thread is its own review.
        let changes = AgentReviewStore::new(dir.path(), thread, ReviewScope::Changes);
        assert_eq!(
            changes.load().unwrap(),
            (PendingReview::default(), Vec::new())
        );

        // Unreadable files are kept aside, and the review starts over.
        std::fs::write(changes.path(), "{ not json").unwrap();
        let broken = AgentReviewStore::new(dir.path(), thread, ReviewScope::Changes);
        assert!(format!("{:#}", broken.load().unwrap_err()).contains("it was kept in"));
        assert!(!changes.path().exists());
    }

    #[gpui::test]
    async fn test_reviewed_marks_only_move_forward(cx: &mut TestAppContext) {
        let dir = tempfile::tempdir().expect("a temporary directory");
        let marks = ReviewedMarks::new(dir.path());
        assert!(marks.load().is_empty());
        cx.update(|cx| {
            marks.mark("t1".into(), "2026-10-27T10:00:00.000Z".into(), cx);
        });
        cx.run_until_parked();
        cx.update(|cx| {
            marks.mark("t1".into(), "2026-10-27T09:00:00.000Z".into(), cx);
            marks.mark("t2".into(), "2026-10-26T09:00:00.000Z".into(), cx);
        });
        cx.run_until_parked();
        assert_eq!(
            marks.load(),
            HashMap::from_iter([
                ("t1".to_string(), "2026-10-27T10:00:00.000Z".to_string()),
                ("t2".to_string(), "2026-10-26T09:00:00.000Z".to_string()),
            ])
        );
    }
}

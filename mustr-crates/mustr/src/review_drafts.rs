//! Pending reviews on disk, so drafts are never lost (`mustr-docs/PLAN.md` §7): one small JSON
//! file per pull request in `<data dir>/review-drafts/`, replaced atomically on every save.
//!
//! T3 doesn't keep pending review drafts (its web app holds them in the browser), and GitHub
//! allows one pending review per user per pull request, so mustr keeps its drafts locally and
//! sends them all at once with `pullRequests.submitReview`.

use anyhow::{Context as _, Result, bail};
use gpui::{App, AppContext as _};
use parking_lot::Mutex;
use review::PendingReview;
use serde::{Deserialize, Serialize};
use std::{
    path::{Path, PathBuf},
    sync::Arc,
};
use t3_client::PullRequestRef;

const FORMAT_VERSION: u32 = 1;

/// Where pending reviews are kept.
pub fn drafts_dir() -> PathBuf {
    paths::data_dir().join("review-drafts")
}

/// The pull request a file belongs to, saved with it as a check.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
struct DraftsKey {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    host: Option<String>,
    repository: String,
    number: i64,
}

#[derive(Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
struct DraftsFile {
    version: u32,
    pull_request: DraftsKey,
    review: PendingReview,
}

#[derive(Default)]
struct WriterState {
    /// The number of the latest save, and of the latest one written. An older save that
    /// finishes after a newer one is dropped.
    latest_save: u64,
    latest_written: u64,
}

/// The pending review of one pull request.
#[derive(Clone)]
pub struct DraftStore {
    path: PathBuf,
    key: DraftsKey,
    writer: Arc<Mutex<WriterState>>,
}

impl DraftStore {
    pub fn new(dir: &Path, pull_request: &PullRequestRef) -> Self {
        let key = DraftsKey {
            host: pull_request.host.clone(),
            repository: pull_request.repository.clone(),
            number: pull_request.number,
        };
        Self {
            path: dir.join(file_name(&key)),
            key,
            writer: Arc::default(),
        }
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    /// The saved pending review, or an empty one. Blocks; call it off the main thread.
    ///
    /// A file mustr can't read is moved aside rather than overwritten, so its drafts can still be
    /// recovered by hand; the error says where it went.
    pub fn load(&self) -> Result<PendingReview> {
        let bytes = match std::fs::read(&self.path) {
            Ok(bytes) => bytes,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                return Ok(PendingReview::default());
            }
            Err(error) => {
                return Err(error).with_context(|| format!("reading {}", self.path.display()));
            }
        };
        match parse(&bytes, &self.key) {
            Ok(review) => Ok(review),
            Err(error) => {
                let aside = self.path.with_extension(format!(
                    "unreadable-{}.json",
                    time::OffsetDateTime::now_utc().unix_timestamp()
                ));
                std::fs::rename(&self.path, &aside).with_context(|| {
                    format!("moving the unreadable {} aside", self.path.display())
                })?;
                Err(error.context(format!(
                    "the saved drafts couldn't be read; they were kept in {}",
                    aside.display()
                )))
            }
        }
    }

    /// Saves in the background. The latest save always wins, even if an earlier one finishes
    /// later.
    pub fn save(&self, review: PendingReview, cx: &mut App) {
        let save = {
            let mut writer = self.writer.lock();
            writer.latest_save += 1;
            writer.latest_save
        };
        let store = self.clone();
        cx.background_spawn(async move {
            let mut writer = store.writer.lock();
            if writer.latest_written > save {
                return;
            }
            match store.write(&review) {
                Ok(()) => writer.latest_written = save,
                Err(error) => log::error!("couldn't save the pending review: {error:#}"),
            }
        })
        .detach();
    }

    /// Replaces the file, or removes it when the review is empty. Blocks.
    pub fn write(&self, review: &PendingReview) -> Result<()> {
        if review.is_empty() {
            return match std::fs::remove_file(&self.path) {
                Err(error) if error.kind() != std::io::ErrorKind::NotFound => {
                    Err(error).with_context(|| format!("removing {}", self.path.display()))
                }
                _ => Ok(()),
            };
        }
        let file = DraftsFile {
            version: FORMAT_VERSION,
            pull_request: self.key.clone(),
            review: review.clone(),
        };
        let bytes = serde_json::to_vec_pretty(&file)?;
        let dir = self
            .path
            .parent()
            .context("the drafts file has no directory")?;
        std::fs::create_dir_all(dir).with_context(|| format!("creating {}", dir.display()))?;
        // Write a sibling and rename it over the file, so a crash mid-write never leaves half a
        // file behind.
        let temporary = self.path.with_extension("json.tmp");
        std::fs::write(&temporary, bytes)
            .with_context(|| format!("writing {}", temporary.display()))?;
        std::fs::rename(&temporary, &self.path)
            .with_context(|| format!("replacing {}", self.path.display()))
    }
}

fn parse(bytes: &[u8], key: &DraftsKey) -> Result<PendingReview> {
    let file: DraftsFile = serde_json::from_slice(bytes).context("not a mustr drafts file")?;
    if file.version != FORMAT_VERSION {
        bail!("drafts file version {} isn't supported", file.version);
    }
    if file.pull_request != *key {
        bail!(
            "the file holds drafts for {}#{}",
            file.pull_request.repository,
            file.pull_request.number
        );
    }
    Ok(file.review)
}

/// A file name that's unique per pull request and safe on every platform: letters, digits, `-`
/// and `.` are kept, and every other byte is written as `_xx`.
fn file_name(key: &DraftsKey) -> String {
    let escape = |part: &str| {
        part.bytes()
            .map(|byte| match byte {
                b'a'..=b'z' | b'A'..=b'Z' | b'0'..=b'9' | b'-' | b'.' => (byte as char).to_string(),
                _ => format!("_{byte:02x}"),
            })
            .collect::<String>()
    };
    format!(
        "{}~{}~{}.json",
        escape(key.host.as_deref().unwrap_or("default-host")),
        escape(&key.repository),
        key.number
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use gpui::TestAppContext;
    use pretty_assertions::assert_eq;
    use review::{CommentPosition, DraftComment, ReviewVerdict};

    fn pull_request(repository: &str, number: i64) -> PullRequestRef {
        PullRequestRef {
            allow_stale: None,
            expected_account_id: None,
            project_id: "project-1".to_string(),
            host: Some("github.com".to_string()),
            repository: repository.to_string(),
            number,
        }
    }

    fn review(body: &str) -> PendingReview {
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
            summary: "Summary".to_string(),
            verdict: Some(ReviewVerdict::Approve),
            ..PendingReview::default()
        }
    }

    #[test]
    fn file_names_are_safe_and_unique() {
        let key = |host: Option<&str>, repository: &str| DraftsKey {
            host: host.map(str::to_string),
            repository: repository.to_string(),
            number: 4,
        };
        assert_eq!(
            file_name(&key(Some("github.com"), "msetaro/mustr")),
            "github.com~msetaro_2fmustr~4.json"
        );
        assert_eq!(
            file_name(&key(None, "a_b/../c")),
            "default-host~a_5fb_2f.._2fc~4.json"
        );
        assert_ne!(file_name(&key(None, "a_2fb")), file_name(&key(None, "a/b")));
    }

    #[test]
    fn round_trips_and_empty_reviews_remove_the_file() -> Result<()> {
        let dir = tempfile::tempdir()?;
        let store = DraftStore::new(dir.path(), &pull_request("acme/hello", 7));
        assert_eq!(store.load()?, PendingReview::default());

        store.write(&review("Why?"))?;
        assert_eq!(store.load()?, review("Why?"));
        // Another pull request doesn't see it.
        let other = DraftStore::new(dir.path(), &pull_request("acme/hello", 8));
        assert_eq!(other.load()?, PendingReview::default());

        store.write(&PendingReview::default())?;
        assert!(!store.path().exists());
        assert_eq!(store.load()?, PendingReview::default());
        Ok(())
    }

    #[test]
    fn unreadable_files_are_kept_aside() -> Result<()> {
        let dir = tempfile::tempdir()?;
        let store = DraftStore::new(dir.path(), &pull_request("acme/hello", 7));
        std::fs::write(store.path(), "{ not json")?;
        let error = store.load().expect_err("the file is corrupt");
        assert!(format!("{error:#}").contains("they were kept in"));
        assert!(!store.path().exists());
        let kept = std::fs::read_dir(dir.path())?
            .filter_map(|entry| entry.ok())
            .map(|entry| std::fs::read_to_string(entry.path()))
            .collect::<Result<Vec<_>, _>>()?;
        assert_eq!(kept, ["{ not json"]);
        // And the next save starts a fresh file.
        store.write(&review("Again"))?;
        assert_eq!(store.load()?, review("Again"));
        Ok(())
    }

    #[gpui::test]
    async fn test_the_latest_save_wins(cx: &mut TestAppContext) {
        let dir = tempfile::tempdir().expect("a temporary directory");
        let store = DraftStore::new(dir.path(), &pull_request("acme/hello", 7));
        cx.update(|cx| {
            store.save(review("one"), cx);
            store.save(review("two"), cx);
            store.save(review("three"), cx);
        });
        cx.run_until_parked();
        assert_eq!(store.load().expect("drafts load"), review("three"));
        assert!(!store.path().with_extension("json.tmp").exists());
    }
}

//! The data behind mustr's home view: open pull requests and recent agent threads from T3,
//! and turning a pull request's files into review contents. Agent threads with work to review
//! come from `agent_threads`.
//!
//! Only the fields mustr shows are decoded, and every entry is decoded on its own, so one
//! malformed or newer-shaped entry doesn't hide the rest.

use collections::HashMap;
use review::{CommentableLines, FileChange, HunkLine, ReviewFileContents, UnavailableFile};
use serde::Deserialize;
use serde_json::Value;
use std::path::Path;
use t3_client::{ChangeType, PatchHunk, PatchLine, PullRequestFile, PullRequestRef};
use util::{paths::PathStyle, rel_path::RelPath};

#[derive(Clone, Debug, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PullRequestEntry {
    pub project_id: String,
    #[serde(default)]
    pub host: Option<String>,
    pub repository: String,
    pub number: i64,
    pub title: String,
    #[serde(default)]
    pub author: Option<PullRequestActor>,
    #[serde(default)]
    pub is_draft: bool,
    /// T3 reports 0 until the forge has been asked for counts (always, for GitHub listings).
    #[serde(default)]
    pub additions: u64,
    #[serde(default)]
    pub deletions: u64,
    #[serde(default)]
    pub viewer_review_requested: bool,
    /// ISO 8601, for ordering the inbox.
    #[serde(default)]
    pub updated_at: Option<String>,
    #[serde(default)]
    pub project_title: Option<String>,
    #[serde(default)]
    pub head_branch: Option<String>,
    #[serde(default)]
    pub base_branch: Option<String>,
    /// The checks' state; T3 leaves it out when the forge didn't say.
    #[serde(default, deserialize_with = "lenient")]
    pub checks_state: Option<ChecksState>,
    #[serde(default, deserialize_with = "lenient")]
    pub review_decision: Option<ReviewDecision>,
}

/// A pull request's checks, as T3 lists them (`PullRequestChecksState`).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum ChecksState {
    Passing,
    Failing,
    Pending,
}

/// A pull request's reviews, as T3 lists them (`PullRequestReviewDecision`).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum ReviewDecision {
    Approved,
    ChangesRequested,
    ReviewRequired,
}

/// Decodes a value a newer T3 may send more kinds of: one mustr doesn't know is `None`.
fn lenient<'de, D, T>(deserializer: D) -> Result<Option<T>, D::Error>
where
    D: serde::Deserializer<'de>,
    T: serde::de::DeserializeOwned,
{
    let value = Option::<Value>::deserialize(deserializer)?;
    Ok(value.and_then(|value| serde_json::from_value(value).ok()))
}

#[derive(Clone, Debug, PartialEq, Eq, Deserialize)]
pub struct PullRequestActor {
    pub login: String,
}

impl PullRequestEntry {
    pub fn reference(&self) -> PullRequestRef {
        PullRequestRef {
            allow_stale: None,
            expected_account_id: None,
            project_id: self.project_id.clone(),
            host: self.host.clone(),
            repository: self.repository.clone(),
            number: self.number,
        }
    }

    pub fn display_ref(&self) -> String {
        format!("{}#{}", self.repository, self.number)
    }

    pub fn has_line_counts(&self) -> bool {
        self.additions > 0 || self.deletions > 0
    }

    /// Whether the signed-in account on the pull request's host opened it.
    pub fn is_authored_by_viewer(&self, viewers: &HashMap<String, String>) -> bool {
        let viewer = self
            .host
            .as_deref()
            .and_then(|host| viewers.get(host))
            .map(|login| login.trim().to_lowercase())
            .filter(|login| !login.is_empty());
        let author = self
            .author
            .as_ref()
            .map(|author| author.login.trim().to_lowercase());
        viewer.is_some() && viewer == author
    }

    /// Whether the pull request matches a search, on the fields T3's search looks at
    /// (`matchesPullRequestQuery`): its number, title, repository, branch and author.
    pub fn matches(&self, query: &str) -> bool {
        let query = query.trim().to_lowercase();
        if query.is_empty() {
            return true;
        }
        format!(
            "#{} {} {} {} {}",
            self.number,
            self.title,
            self.repository,
            self.head_branch.as_deref().unwrap_or_default(),
            self.author
                .as_ref()
                .map(|author| author.login.as_str())
                .unwrap_or_default()
        )
        .to_lowercase()
        .contains(&query)
    }
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct PullRequestList {
    pub entries: Vec<PullRequestEntry>,
    /// Projects T3 couldn't list, as "project: reason".
    pub errors: Vec<String>,
    /// The signed-in account's login on each host, which says which pull requests are yours.
    pub viewers: HashMap<String, String>,
}

pub fn parse_pull_request_list(list: Value) -> PullRequestList {
    #[derive(Deserialize)]
    #[serde(rename_all = "camelCase")]
    struct ProjectError {
        project_title: String,
        message: String,
    }

    let entries = decode_each::<PullRequestEntry>(list.get("entries"), "pull request");
    let errors = decode_each::<ProjectError>(list.get("errors"), "pull request list error")
        .into_iter()
        .map(|error| format!("{}: {}", error.project_title, error.message))
        .collect();
    let viewers = list
        .get("viewers")
        .and_then(|viewers| serde_json::from_value::<HashMap<String, String>>(viewers.clone()).ok())
        .unwrap_or_default();
    PullRequestList {
        entries,
        errors,
        viewers,
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ThreadRow {
    pub id: String,
    pub title: String,
    pub status: ThreadStatus,
    pub branch: Option<String>,
    pub project: Option<String>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ThreadStatus {
    /// The agent is blocked on an approval or a question.
    NeedsInput,
    /// A run is preparing, queued, starting, running, or waiting.
    Working(String),
    Failed,
    /// Idle, or the last run completed, was interrupted, cancelled, or rolled back.
    Settled(String),
}

impl ThreadStatus {
    /// A thread's status from its record in T3's shell, as the threads sidebar and the
    /// thread's header read it (`thread_chat::ShellStatus`): a thread whose agent still waits on
    /// background work reads "waiting", not its last run's "completed".
    pub(crate) fn of(thread: &t3_client::t3_api::types::OrchestrationV2ThreadShell) -> Self {
        use thread_chat::ShellStatus;
        match ShellStatus::from_shell(thread) {
            Some(ShellStatus::PendingApproval | ShellStatus::AwaitingInput) => Self::NeedsInput,
            Some(ShellStatus::Failed) => Self::Failed,
            Some(
                status @ (ShellStatus::Working | ShellStatus::Connecting | ShellStatus::Waiting),
            ) => Self::Working(status.label().to_lowercase()),
            Some(status) => Self::Settled(status.label().to_lowercase()),
            None => Self::Settled(match serde_json::to_value(&thread.status) {
                Ok(Value::String(status)) => status.replace('_', " "),
                _ => String::new(),
            }),
        }
    }

    pub(crate) fn from_shell(status: String, has_pending_request: bool) -> Self {
        if has_pending_request {
            return Self::NeedsInput;
        }
        match status.as_str() {
            "preparing" | "queued" | "starting" | "running" | "waiting" => Self::Working(status),
            "failed" => Self::Failed,
            _ => Self::Settled(status.replace('_', " ")),
        }
    }

    pub fn label(&self) -> &str {
        match self {
            Self::NeedsInput => "needs input",
            Self::Failed => "failed",
            Self::Working(status) | Self::Settled(status) => status,
        }
    }
}

/// A project in T3's shell snapshot: a folder agents work in.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ProjectRow {
    pub id: String,
    pub title: String,
    pub workspace_root: Option<String>,
}

/// Decodes each element of a JSON array on its own, skipping (and logging) the ones mustr can't
/// read.
pub(crate) fn decode_each<T: serde::de::DeserializeOwned>(
    values: Option<&Value>,
    what: &str,
) -> Vec<T> {
    let Some(Value::Array(values)) = values else {
        return Vec::new();
    };
    values
        .iter()
        .filter_map(|value| match serde_json::from_value(value.clone()) {
            Ok(decoded) => Some(decoded),
            Err(error) => {
                log::warn!("skipping a {what} mustr can't read: {error}");
                None
            }
        })
        .collect()
}

#[derive(Default)]
pub struct ReviewFiles {
    pub files: Vec<ReviewFileContents>,
    pub unavailable: Vec<UnavailableFile>,
}

/// Splits a pull request's files into the ones the review view can show and the ones it can't
/// (binary, too large for the forge to expand, or with a path mustr won't trust).
pub fn review_files(files: Vec<PullRequestFile>) -> ReviewFiles {
    let mut review = ReviewFiles::default();
    for file in files {
        let change = match file.file.change_type {
            ChangeType::Change => FileChange::Modified,
            ChangeType::New => FileChange::Added,
            ChangeType::Deleted => FileChange::Deleted,
            ChangeType::RenamePure | ChangeType::RenameChanged => FileChange::Renamed,
        };
        let old_path = (change == FileChange::Renamed).then(|| file.file.old_path.clone());
        let path = match file.file.change_type {
            ChangeType::Deleted => file.file.old_path,
            _ => file.file.new_path,
        };
        let unavailable = |path: String, reason: String| UnavailableFile {
            path,
            old_path: old_path.clone(),
            change,
            reason,
        };
        if let Some(reason) = file.unavailable {
            review.unavailable.push(unavailable(path, reason));
            continue;
        }
        if file.old_text.is_none() && file.new_text.is_none() {
            review
                .unavailable
                .push(unavailable(path, "No contents".to_string()));
            continue;
        }
        // Paths come from the pull request, so they're untrusted: no absolute paths, and no
        // `..` that climbs out of the repository.
        let relative_path = RelPath::new(Path::new(&path), PathStyle::Unix)
            .ok()
            .filter(|relative_path| !relative_path.is_empty());
        let Some(relative_path) = relative_path else {
            review
                .unavailable
                .push(unavailable(path, "Unsupported path".to_string()));
            continue;
        };
        review.files.push(ReviewFileContents {
            path: relative_path.as_ref().into_arc(),
            old_path,
            base_text: file.old_text,
            head_text: file.new_text,
            commentable: Some(commentable_lines(&file.file.hunks)),
        });
    }
    review
}

/// The lines the forge accepts comments on: the lines of its patch's hunks (`PLAN.md` §14).
pub fn commentable_lines(hunks: &[PatchHunk]) -> CommentableLines {
    CommentableLines::from_hunks(hunks.iter().map(|hunk| {
        let lines = hunk.lines.iter().map(|line| match line {
            PatchLine::Context => HunkLine::Context,
            PatchLine::Added => HunkLine::Added,
            PatchLine::Deleted => HunkLine::Deleted,
        });
        (hunk.old_start, hunk.new_start, lines)
    }))
}

const LISTED_UNAVAILABLE_FILES: usize = 5;

/// A short note naming the files a review doesn't show.
pub fn unavailable_note(pull_request: &str, unavailable: &[UnavailableFile]) -> Option<String> {
    if unavailable.is_empty() {
        return None;
    }
    let count = unavailable.len();
    let mut note = format!(
        "{pull_request}: {count} {} not shown: ",
        if count == 1 { "file is" } else { "files are" }
    );
    let listed = unavailable
        .iter()
        .take(LISTED_UNAVAILABLE_FILES)
        .map(|file| format!("{} ({})", file.path, file.reason))
        .collect::<Vec<_>>()
        .join(", ");
    note.push_str(&listed);
    if count > LISTED_UNAVAILABLE_FILES {
        note.push_str(&format!(", and {} more", count - LISTED_UNAVAILABLE_FILES));
    }
    Some(note)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use t3_client::ChangedFile;

    fn pull_request_file(
        old_path: &str,
        new_path: &str,
        change_type: ChangeType,
        old_text: Option<&str>,
        new_text: Option<&str>,
        unavailable: Option<&str>,
    ) -> PullRequestFile {
        PullRequestFile {
            file: ChangedFile {
                old_path: old_path.to_string(),
                new_path: new_path.to_string(),
                change_type,
                binary: unavailable == Some("Binary file"),
                hunks: vec![PatchHunk {
                    old_start: 1,
                    new_start: 1,
                    lines: vec![PatchLine::Deleted, PatchLine::Added],
                }],
            },
            old_text: old_text.map(str::to_string),
            new_text: new_text.map(str::to_string),
            unavailable: unavailable.map(str::to_string),
        }
    }

    #[test]
    fn maps_pull_request_files_to_review_contents() {
        let review = review_files(vec![
            pull_request_file(
                "src/main.rs",
                "src/main.rs",
                ChangeType::Change,
                Some("old"),
                Some("new"),
                None,
            ),
            pull_request_file(
                "added.md",
                "added.md",
                ChangeType::New,
                None,
                Some("hi"),
                None,
            ),
            pull_request_file(
                "gone.txt",
                "gone.txt",
                ChangeType::Deleted,
                Some("bye"),
                None,
                None,
            ),
            pull_request_file(
                "lib/util.rs",
                "lib/helpers.rs",
                ChangeType::RenameChanged,
                Some("a"),
                Some("b"),
                None,
            ),
            pull_request_file(
                "logo.png",
                "logo.png",
                ChangeType::Change,
                None,
                None,
                Some("Binary file"),
            ),
            pull_request_file(
                "big.json",
                "big.json",
                ChangeType::Change,
                None,
                None,
                Some("Too large to expand"),
            ),
            pull_request_file(
                "../escape",
                "../escape",
                ChangeType::New,
                None,
                Some("x"),
                None,
            ),
            pull_request_file(
                "/etc/passwd",
                "/etc/passwd",
                ChangeType::New,
                None,
                Some("x"),
                None,
            ),
            pull_request_file("empty", "empty", ChangeType::Change, None, None, None),
        ]);

        assert_eq!(
            review.files[0]
                .commentable
                .as_ref()
                .and_then(|lines| lines.position(review::DiffSide::Right, 1)),
            Some(review::CommentPosition::Added { new_line: 1 })
        );
        let shown = review
            .files
            .iter()
            .map(|file| {
                (
                    file.path.as_unix_str().to_string(),
                    file.old_path.clone(),
                    file.change(),
                    file.base_text.clone(),
                    file.head_text.clone(),
                )
            })
            .collect::<Vec<_>>();
        assert_eq!(
            shown,
            vec![
                (
                    "src/main.rs".into(),
                    None,
                    FileChange::Modified,
                    Some("old".into()),
                    Some("new".into())
                ),
                (
                    "added.md".into(),
                    None,
                    FileChange::Added,
                    None,
                    Some("hi".into())
                ),
                (
                    "gone.txt".into(),
                    None,
                    FileChange::Deleted,
                    Some("bye".into()),
                    None
                ),
                (
                    "lib/helpers.rs".into(),
                    Some("lib/util.rs".into()),
                    FileChange::Renamed,
                    Some("a".into()),
                    Some("b".into())
                ),
            ]
        );
        let unavailable = review
            .unavailable
            .iter()
            .map(|file| (file.path.as_str(), file.change, file.reason.as_str()))
            .collect::<Vec<_>>();
        assert_eq!(
            unavailable,
            vec![
                ("logo.png", FileChange::Modified, "Binary file"),
                ("big.json", FileChange::Modified, "Too large to expand"),
                ("../escape", FileChange::Added, "Unsupported path"),
                ("/etc/passwd", FileChange::Added, "Unsupported path"),
                ("empty", FileChange::Modified, "No contents"),
            ]
        );
    }

    #[test]
    fn unavailable_note_lists_a_few_files() {
        assert_eq!(unavailable_note("o/r#1", &[]), None);
        let one = [UnavailableFile {
            path: "logo.png".into(),
            old_path: None,
            change: FileChange::Modified,
            reason: "Binary file".into(),
        }];
        assert_eq!(
            unavailable_note("o/r#1", &one).unwrap(),
            "o/r#1: 1 file is not shown: logo.png (Binary file)"
        );
        let many = (0..7)
            .map(|index| UnavailableFile {
                path: format!("f{index}"),
                old_path: None,
                change: FileChange::Modified,
                reason: "Binary file".into(),
            })
            .collect::<Vec<_>>();
        let note = unavailable_note("o/r#1", &many).unwrap();
        assert!(note.starts_with("o/r#1: 7 files are not shown: f0 (Binary file), "));
        assert!(note.ends_with("f4 (Binary file), and 2 more"));
    }

    #[test]
    fn parses_pull_request_lists_entry_by_entry() {
        let list = parse_pull_request_list(json!({
            "viewers": {},
            "providers": [],
            "entries": [
                {
                    "provider": "github", "host": "github.com", "projectId": "p1",
                    "projectTitle": "mustr", "repository": "msetaro/mustr", "number": 7,
                    "title": "Add the review view", "url": "https://github.com/msetaro/mustr/pull/7",
                    "author": { "login": "msetaro", "name": null, "avatarUrl": null },
                    "headBranch": "m0/review", "baseBranch": "main", "state": "open",
                    "isDraft": true, "mergeability": "mergeable", "additions": 12, "deletions": 3,
                    "createdAt": "2026-10-27T00:00:00.000Z", "updatedAt": "2026-10-27T00:00:00.000Z",
                    "viewerReviewRequested": false, "labels": []
                },
                { "number": "not a number" },
                {
                    "projectId": "p2", "repository": "acme/api", "number": 482,
                    "title": "Retry backoff", "author": null, "additions": 0, "deletions": 0
                }
            ],
            "errors": [{ "projectId": "p3", "projectTitle": "infra", "message": "No GitHub account" }],
            "truncated": false,
            "nextCursors": {}
        }));

        assert_eq!(list.entries.len(), 2);
        let first = &list.entries[0];
        assert_eq!(first.display_ref(), "msetaro/mustr#7");
        assert_eq!(first.author.as_ref().unwrap().login, "msetaro");
        assert!(first.is_draft);
        assert!(first.has_line_counts());
        assert_eq!(
            first.updated_at.as_deref(),
            Some("2026-10-27T00:00:00.000Z")
        );
        assert!(!first.viewer_review_requested);
        assert_eq!(
            first.reference(),
            PullRequestRef {
                allow_stale: None,
                expected_account_id: None,
                project_id: "p1".into(),
                host: Some("github.com".into()),
                repository: "msetaro/mustr".into(),
                number: 7,
            }
        );
        assert!(!list.entries[1].has_line_counts());
        assert_eq!(list.errors, vec!["infra: No GitHub account".to_string()]);
        assert_eq!(
            parse_pull_request_list(json!(null)),
            PullRequestList::default()
        );
    }
}

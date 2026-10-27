//! Loading a pull request's changed files, with old and new contents, from T3.
//!
//! T3 serves the unified patch over HTTP (`POST /api/pull-requests/diff`, paged by cursor)
//! and each file's full old/new text over RPC (`pullRequests.diffFileContents`). mustr's
//! review view needs the full texts, so it can build real diffs with syntax highlighting.

use anyhow::{Context as _, Result, anyhow, bail};
use futures::{AsyncReadExt as _, StreamExt as _};
use http_client::{AsyncBody, HttpClient, Method, Request, Url};
use serde::{Deserialize, Serialize};
use serde_json::json;

use crate::{AccessToken, RpcConnection, RpcError};

const CONTENT_REQUESTS_IN_FLIGHT: usize = 6;

/// Identifies a pull request the way T3's `PullRequestRef` does.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PullRequestRef {
    pub project_id: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub host: Option<String>,
    pub repository: String,
    pub number: u64,
}

/// T3's `changeType` values for `pullRequests.diffFileContents`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ChangeType {
    Change,
    New,
    Deleted,
    RenamePure,
    RenameChanged,
}

impl ChangeType {
    pub fn as_str(self) -> &'static str {
        match self {
            ChangeType::Change => "change",
            ChangeType::New => "new",
            ChangeType::Deleted => "deleted",
            ChangeType::RenamePure => "rename-pure",
            ChangeType::RenameChanged => "rename-changed",
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ChangedFile {
    pub old_path: String,
    pub new_path: String,
    pub change_type: ChangeType,
    pub binary: bool,
}

/// A changed file with its texts. `old_text` is `None` for added files and `new_text` is
/// `None` for deleted ones. Both are `None` when the texts can't be shown, and
/// `unavailable` says why (a binary file, or one too large for the forge to expand).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PullRequestFile {
    pub file: ChangedFile,
    pub old_text: Option<String>,
    pub new_text: Option<String>,
    pub unavailable: Option<String>,
}

/// The pull request's complete unified patch, following T3's cursor pages.
pub async fn pull_request_patch(
    http: &dyn HttpClient,
    base_url: &Url,
    access_token: &AccessToken,
    pull_request: &PullRequestRef,
) -> Result<String> {
    #[derive(Deserialize)]
    #[serde(rename_all = "camelCase")]
    struct DiffSlice {
        patch: String,
        next_cursor: Option<String>,
    }

    let url = base_url
        .join("/api/pull-requests/diff")
        .with_context(|| format!("invalid T3 server URL {base_url}"))?;
    let mut patch = String::new();
    let mut cursor: Option<String> = None;
    loop {
        let mut body = serde_json::to_value(pull_request)?;
        if let Some(cursor) = &cursor {
            body["cursor"] = json!(cursor);
        }
        let request = Request::builder()
            .method(Method::POST)
            .uri(url.as_str())
            .header("Content-Type", "application/json")
            .header(
                "Authorization",
                format!("Bearer {}", access_token.access_token),
            )
            .body(AsyncBody::from(body.to_string()))?;
        let mut response = http.send(request).await?;
        let mut text = String::new();
        response.body_mut().read_to_string(&mut text).await?;
        if !response.status().is_success() {
            bail!("{url} returned {}: {text}", response.status());
        }
        let slice: DiffSlice = serde_json::from_str(&text)
            .with_context(|| format!("unexpected pull request diff from {url}"))?;
        if !patch.is_empty() && !patch.ends_with('\n') {
            patch.push('\n');
        }
        patch.push_str(&slice.patch);
        match slice.next_cursor {
            Some(next) if Some(&next) != cursor.as_ref() => cursor = Some(next),
            _ => return Ok(patch),
        }
    }
}

/// Lists the files in a `git diff` style patch. Uses the `---`/`+++` and rename lines when
/// present, since the `diff --git` header alone is ambiguous for paths containing " b/".
pub fn changed_files(patch: &str) -> Vec<ChangedFile> {
    let mut files = Vec::new();
    let mut lines = patch.lines().peekable();
    while let Some(line) = lines.next() {
        let Some(header) = line.strip_prefix("diff --git ") else {
            continue;
        };
        let mut old_path: Option<String> = None;
        let mut new_path: Option<String> = None;
        let mut new_file = false;
        let mut deleted_file = false;
        let mut renamed = false;
        let mut has_hunks = false;
        let mut binary = false;
        while let Some(next) = lines.peek() {
            if next.starts_with("diff --git ") {
                break;
            }
            let next = lines.next().unwrap_or_default();
            if next.starts_with("new file mode") {
                new_file = true;
            } else if next.starts_with("deleted file mode") {
                deleted_file = true;
            } else if let Some(path) = next.strip_prefix("rename from ") {
                renamed = true;
                old_path = Some(unquote(path));
            } else if let Some(path) = next.strip_prefix("rename to ") {
                renamed = true;
                new_path = Some(unquote(path));
            } else if let Some(path) = next.strip_prefix("--- ") {
                if old_path.is_none() {
                    old_path = strip_side_prefix(&unquote(path), "a/");
                }
            } else if let Some(path) = next.strip_prefix("+++ ") {
                if new_path.is_none() {
                    new_path = strip_side_prefix(&unquote(path), "b/");
                }
            } else if next.starts_with("@@") {
                has_hunks = true;
            } else if next.starts_with("Binary files ") || next == "GIT binary patch" {
                binary = true;
            }
        }
        let (header_old, header_new) = split_header(header);
        let old_path = old_path.or(header_old.clone()).or(header_new.clone());
        let new_path = new_path.or(header_new).or(header_old);
        let (Some(old_path), Some(new_path)) = (old_path, new_path) else {
            log::warn!("skipping a patch section with an unreadable header: {header}");
            continue;
        };
        let change_type = if new_file {
            ChangeType::New
        } else if deleted_file {
            ChangeType::Deleted
        } else if renamed && has_hunks {
            ChangeType::RenameChanged
        } else if renamed {
            ChangeType::RenamePure
        } else {
            ChangeType::Change
        };
        files.push(ChangedFile {
            old_path,
            new_path,
            change_type,
            binary,
        });
    }
    files
}

/// Fetches old and new texts for every non-binary file, a few requests at a time.
pub async fn load_pull_request_files(
    http: &dyn HttpClient,
    base_url: &Url,
    access_token: &AccessToken,
    connection: &RpcConnection,
    pull_request: &PullRequestRef,
) -> Result<Vec<PullRequestFile>> {
    let patch = pull_request_patch(http, base_url, access_token, pull_request).await?;
    let files = changed_files(&patch);
    let results = futures::stream::iter(files.into_iter().map(|file| async move {
        if file.binary {
            return Ok(PullRequestFile {
                file,
                old_text: None,
                new_text: None,
                unavailable: Some("Binary file".to_string()),
            });
        }
        let mut payload = serde_json::to_value(pull_request)?;
        payload["changeType"] = json!(file.change_type.as_str());
        payload["oldPath"] = json!(file.old_path);
        payload["newPath"] = json!(file.new_path);
        let contents = match connection
            .request("pullRequests.diffFileContents", payload)
            .await
        {
            Ok(contents) => contents,
            // A typed failure is about this file (e.g. over the forge's expansion limit),
            // so show the rest of the pull request. Connection failures still fail the load.
            Err(error @ RpcError::Failed { .. }) => {
                let reason = error
                    .error_detail()
                    .unwrap_or_else(|| "This file's contents couldn't be loaded".to_string());
                log::info!("{} unavailable: {reason}", file.new_path);
                return Ok(PullRequestFile {
                    file,
                    old_text: None,
                    new_text: None,
                    unavailable: Some(reason),
                });
            }
            Err(error) => {
                return Err(anyhow!(error)).with_context(|| format!("loading {}", file.new_path));
            }
        };
        let text = |key: &str| {
            contents
                .get(key)
                .and_then(|value| value.as_str())
                .map(str::to_string)
        };
        let (old_text, new_text) = match file.change_type {
            ChangeType::New => (None, text("newContents")),
            ChangeType::Deleted => (text("oldContents"), None),
            _ => (text("oldContents"), text("newContents")),
        };
        anyhow::Ok(PullRequestFile {
            file,
            old_text,
            new_text,
            unavailable: None,
        })
    }))
    .buffered(CONTENT_REQUESTS_IN_FLIGHT)
    .collect::<Vec<_>>()
    .await;
    results.into_iter().collect()
}

fn strip_side_prefix(path: &str, prefix: &str) -> Option<String> {
    if path == "/dev/null" {
        return None;
    }
    Some(path.strip_prefix(prefix).unwrap_or(path).to_string())
}

/// Splits `a/<old> b/<new>`. Exact for quoted paths and for unrenamed files, where both
/// sides are the same path; renames are resolved from the `rename from/to` lines instead.
fn split_header(header: &str) -> (Option<String>, Option<String>) {
    if header.starts_with('"') {
        let (old, rest) = take_quoted(header);
        let new = rest.trim_start();
        let new = if new.starts_with('"') {
            take_quoted(new).0
        } else {
            new.to_string()
        };
        return (strip_side_prefix(&old, "a/"), strip_side_prefix(&new, "b/"));
    }
    let length = header.len();
    if length > 5 && (length - 5).is_multiple_of(2) {
        let path_length = (length - 5) / 2;
        if let (Some(old), Some(new)) = (
            header.get(2..2 + path_length),
            header.get(path_length + 5..),
        ) && header.starts_with("a/")
            && header.get(path_length + 2..path_length + 5) == Some(" b/")
            && old == new
        {
            return (Some(old.to_string()), Some(new.to_string()));
        }
    }
    match header.split_once(" b/") {
        Some((old, new)) => (strip_side_prefix(old, "a/"), Some(new.to_string())),
        None => (None, None),
    }
}

/// Undoes git's C-style path quoting (`"a/caf\303\251.txt"`).
fn unquote(path: &str) -> String {
    if path.starts_with('"') {
        take_quoted(path).0
    } else {
        path.to_string()
    }
}

fn take_quoted(input: &str) -> (String, &str) {
    let mut bytes = Vec::new();
    let mut characters = input.char_indices().skip(1);
    while let Some((index, character)) = characters.next() {
        match character {
            '"' => {
                let rest = input.get(index + 1..).unwrap_or_default();
                return (String::from_utf8_lossy(&bytes).into_owned(), rest);
            }
            '\\' => match characters.next() {
                Some((_, 'n')) => bytes.push(b'\n'),
                Some((_, 't')) => bytes.push(b'\t'),
                Some((_, digit @ '0'..='7')) => {
                    let mut value = digit.to_digit(8).unwrap_or_default();
                    for _ in 0..2 {
                        match characters.clone().next() {
                            Some((_, next @ '0'..='7')) => {
                                characters.next();
                                value = value * 8 + next.to_digit(8).unwrap_or_default();
                            }
                            _ => break,
                        }
                    }
                    bytes.push(value as u8);
                }
                Some((_, other)) => {
                    let mut buffer = [0; 4];
                    bytes.extend_from_slice(other.encode_utf8(&mut buffer).as_bytes());
                }
                None => break,
            },
            other => {
                let mut buffer = [0; 4];
                bytes.extend_from_slice(other.encode_utf8(&mut buffer).as_bytes());
            }
        }
    }
    (String::from_utf8_lossy(&bytes).into_owned(), "")
}

#[cfg(test)]
mod tests {
    use super::*;

    const PATCH: &str = r#"diff --git a/src/main.rs b/src/main.rs
index 1111111..2222222 100644
--- a/src/main.rs
+++ b/src/main.rs
@@ -1,3 +1,3 @@
 fn main() {
-    println!("old");
+    println!("new");
 }
diff --git a/docs/new file.md b/docs/new file.md
new file mode 100644
index 0000000..3333333
--- /dev/null
+++ b/docs/new file.md
@@ -0,0 +1 @@
+hello
diff --git a/old.txt b/old.txt
deleted file mode 100644
index 4444444..0000000
--- a/old.txt
+++ /dev/null
@@ -1 +0,0 @@
-bye
diff --git a/a b/x.rs b/a b/y.rs
similarity index 100%
rename from a b/x.rs
rename to a b/y.rs
diff --git a/lib/util.rs b/lib/helpers.rs
similarity index 90%
rename from lib/util.rs
rename to lib/helpers.rs
index 5555555..6666666 100644
--- a/lib/util.rs
+++ b/lib/helpers.rs
@@ -1 +1 @@
-old
+new
diff --git a/logo.png b/logo.png
index 7777777..8888888 100644
Binary files a/logo.png and b/logo.png differ
diff --git "a/caf\303\251.txt" "b/caf\303\251.txt"
index 9999999..aaaaaaa 100644
--- "a/caf\303\251.txt"
+++ "b/caf\303\251.txt"
@@ -1 +1 @@
-x
+y
"#;

    fn file(old: &str, new: &str, change_type: ChangeType, binary: bool) -> ChangedFile {
        ChangedFile {
            old_path: old.to_string(),
            new_path: new.to_string(),
            change_type,
            binary,
        }
    }

    #[test]
    fn parses_every_kind_of_change() {
        assert_eq!(
            changed_files(PATCH),
            vec![
                file("src/main.rs", "src/main.rs", ChangeType::Change, false),
                file(
                    "docs/new file.md",
                    "docs/new file.md",
                    ChangeType::New,
                    false
                ),
                file("old.txt", "old.txt", ChangeType::Deleted, false),
                file("a b/x.rs", "a b/y.rs", ChangeType::RenamePure, false),
                file(
                    "lib/util.rs",
                    "lib/helpers.rs",
                    ChangeType::RenameChanged,
                    false
                ),
                file("logo.png", "logo.png", ChangeType::Change, true),
                file("café.txt", "café.txt", ChangeType::Change, false),
            ]
        );
    }

    #[test]
    fn header_split_handles_b_slash_inside_paths() {
        assert_eq!(
            split_header("a/x b/y.rs b/x b/y.rs"),
            (Some("x b/y.rs".to_string()), Some("x b/y.rs".to_string()))
        );
    }

    #[test]
    fn pull_request_ref_matches_t3_field_names() -> Result<()> {
        let pull_request = PullRequestRef {
            project_id: "project".to_string(),
            host: Some("github.com".to_string()),
            repository: "msetaro/differ".to_string(),
            number: 7,
        };
        assert_eq!(
            serde_json::to_value(&pull_request)?,
            json!({ "projectId": "project", "host": "github.com", "repository": "msetaro/differ", "number": 7 })
        );
        Ok(())
    }
}

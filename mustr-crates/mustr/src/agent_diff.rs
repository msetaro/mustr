//! T3's thread diffs (`orchestration.getTurnDiff`, `getFullThreadDiff`) are unified patches.
//! When the full files can't be read (the thread's worktree isn't on this machine), the review
//! is built from the patch alone: each side's text holds the patch's lines at their line numbers,
//! with blank lines in between. Line numbers, and so comment positions, stay right, and the
//! review's excerpts (a hunk plus a couple of context lines) stay inside what the patch shows.

use review::{FileChange, ReviewFileContents, UnavailableFile};
use std::path::Path;
use t3_client::PatchLine;
use util::{paths::PathStyle, rel_path::RelPath};

/// T3 cuts long patches short and appends this (`CheckpointDiffQuery.ts`).
const TRUNCATED_MARKER: &str = "\n\n[truncated]";

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PatchFile {
    /// `None` for an added file.
    pub old_path: Option<String>,
    /// `None` for a deleted file.
    pub new_path: Option<String>,
    pub binary: bool,
    pub hunks: Vec<TextHunk>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct TextHunk {
    pub old_start: u32,
    pub new_start: u32,
    pub lines: Vec<(PatchLine, String)>,
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Patch {
    pub files: Vec<PatchFile>,
    /// T3 cut the patch short, so later files (or the end of the last one) are missing.
    pub truncated: bool,
}

impl PatchFile {
    /// The path the review shows the file under: the new path, or the old one when deleted.
    pub fn path(&self) -> &str {
        self.new_path
            .as_deref()
            .or(self.old_path.as_deref())
            .unwrap_or_default()
    }

    pub fn change(&self) -> FileChange {
        match (&self.old_path, &self.new_path) {
            (None, _) => FileChange::Added,
            (_, None) => FileChange::Deleted,
            (Some(old), Some(new)) if old != new => FileChange::Renamed,
            _ => FileChange::Modified,
        }
    }

    /// Each side's text, with the patch's lines at their line numbers and blank lines between
    /// hunks. `None` for the side a file doesn't have.
    pub fn partial_texts(&self) -> (Option<String>, Option<String>) {
        fn place<'a>(lines: &mut Vec<Option<&'a str>>, line: u32, text: &'a str) {
            let index = line.saturating_sub(1) as usize;
            if lines.len() <= index {
                lines.resize(index + 1, None);
            }
            lines[index] = Some(text);
        }
        let mut old_lines = Vec::<Option<&str>>::new();
        let mut new_lines = Vec::<Option<&str>>::new();
        for hunk in &self.hunks {
            let (mut old_line, mut new_line) = (hunk.old_start.max(1), hunk.new_start.max(1));
            for (kind, text) in &hunk.lines {
                match kind {
                    PatchLine::Context => {
                        place(&mut old_lines, old_line, text);
                        place(&mut new_lines, new_line, text);
                        old_line += 1;
                        new_line += 1;
                    }
                    PatchLine::Deleted => {
                        place(&mut old_lines, old_line, text);
                        old_line += 1;
                    }
                    PatchLine::Added => {
                        place(&mut new_lines, new_line, text);
                        new_line += 1;
                    }
                }
            }
        }
        let join = |lines: Vec<Option<&str>>| {
            let mut text = lines
                .into_iter()
                .map(Option::unwrap_or_default)
                .collect::<Vec<_>>()
                .join("\n");
            if !text.is_empty() {
                text.push('\n');
            }
            text
        };
        (
            self.old_path.as_ref().map(|_| join(old_lines)),
            self.new_path.as_ref().map(|_| join(new_lines)),
        )
    }
}

/// Splits a unified patch (as `git diff` writes it) into files with their hunks' lines.
pub fn parse_patch(patch: &str) -> Patch {
    let (patch, truncated) = match patch.strip_suffix(TRUNCATED_MARKER) {
        Some(patch) => (patch, true),
        None => (patch, false),
    };
    let mut files = Vec::new();
    let mut lines = patch.lines().peekable();
    while let Some(line) = lines.next() {
        let Some(header) = line.strip_prefix("diff --git ") else {
            continue;
        };
        let (mut old_path, mut new_path) = split_git_header(header);
        let mut binary = false;
        let mut hunks = Vec::new();
        while let Some(line) = lines.peek().copied() {
            if line.starts_with("diff --git ") {
                break;
            }
            lines.next();
            if let Some(path) = line.strip_prefix("--- ") {
                old_path = side_path(path, "a/");
            } else if let Some(path) = line.strip_prefix("+++ ") {
                new_path = side_path(path, "b/");
            } else if line.starts_with("new file mode") {
                old_path = None;
            } else if line.starts_with("deleted file mode") {
                new_path = None;
            } else if let Some(path) = line.strip_prefix("rename from ") {
                old_path = Some(unquote(path));
            } else if let Some(path) = line.strip_prefix("rename to ") {
                new_path = Some(unquote(path));
            } else if line.starts_with("Binary files ") || line == "GIT binary patch" {
                binary = true;
            } else if line.starts_with("@@ ")
                && let Some(hunk) = parse_hunk(line, &mut lines)
            {
                hunks.push(hunk);
            }
        }
        files.push(PatchFile {
            old_path,
            new_path,
            binary,
            hunks,
        });
    }
    Patch { files, truncated }
}

/// `a/old b/new` from a `diff --git` line. Quoted paths and paths with spaces are best effort;
/// the `---`/`+++` and `rename` lines that follow say exactly.
fn split_git_header(header: &str) -> (Option<String>, Option<String>) {
    if let Some(rest) = header.strip_prefix('"') {
        if let Some(end) = rest.find("\" ") {
            let old = unquote(&format!("\"{}\"", &rest[..end]));
            let new = unquote(&rest[end + 2..]);
            return (
                old.strip_prefix("a/").map(str::to_string),
                new.strip_prefix("b/").map(str::to_string),
            );
        }
    }
    match header.split_once(" b/") {
        Some((old, new)) => (
            old.strip_prefix("a/").map(str::to_string),
            Some(new.to_string()),
        ),
        None => (None, None),
    }
}

fn side_path(path: &str, prefix: &str) -> Option<String> {
    let path = unquote(path.trim_end_matches('\t'));
    if path == "/dev/null" {
        return None;
    }
    Some(path.strip_prefix(prefix).unwrap_or(&path).to_string())
}

/// Git's C-style quoting of unusual paths: `"a/caf\303\251.txt"`.
fn unquote(path: &str) -> String {
    let Some(inner) = path
        .strip_prefix('"')
        .and_then(|path| path.strip_suffix('"'))
    else {
        return path.to_string();
    };
    let mut bytes = Vec::with_capacity(inner.len());
    let mut chars = inner.bytes().peekable();
    while let Some(byte) = chars.next() {
        if byte != b'\\' {
            bytes.push(byte);
            continue;
        }
        match chars.next() {
            Some(b'n') => bytes.push(b'\n'),
            Some(b't') => bytes.push(b'\t'),
            Some(digit @ b'0'..=b'7') => {
                let mut value = u32::from(digit - b'0');
                for _ in 0..2 {
                    if let Some(next @ b'0'..=b'7') = chars.peek().copied() {
                        chars.next();
                        value = value * 8 + u32::from(next - b'0');
                    }
                }
                bytes.push(value as u8);
            }
            Some(other) => bytes.push(other),
            None => {}
        }
    }
    String::from_utf8_lossy(&bytes).into_owned()
}

/// One hunk, read by the counts its header gives, so lines such as `--- x` inside it aren't
/// mistaken for headers.
fn parse_hunk<'a>(
    header: &str,
    lines: &mut std::iter::Peekable<impl Iterator<Item = &'a str>>,
) -> Option<TextHunk> {
    let ranges = header.strip_prefix("@@ ")?.split(" @@").next()?;
    let (old, new) = ranges.split_once(' ')?;
    let range = |range: &str, sign: char| -> Option<(u32, u32)> {
        let range = range.strip_prefix(sign)?;
        Some(match range.split_once(',') {
            Some((start, count)) => (start.parse().ok()?, count.parse().ok()?),
            None => (range.parse().ok()?, 1),
        })
    };
    let (old_start, mut old_left) = range(old, '-')?;
    let (new_start, mut new_left) = range(new, '+')?;
    let mut hunk = TextHunk {
        old_start,
        new_start,
        lines: Vec::new(),
    };
    while old_left > 0 || new_left > 0 {
        let Some(line) = lines.peek().copied() else {
            break;
        };
        let (kind, text) = match line.as_bytes().first() {
            Some(b'+') => (PatchLine::Added, &line[1..]),
            Some(b'-') => (PatchLine::Deleted, &line[1..]),
            Some(b' ') => (PatchLine::Context, &line[1..]),
            // Some tools trim the space off empty context lines.
            None => (PatchLine::Context, ""),
            Some(b'\\') => {
                lines.next();
                continue;
            }
            _ => break,
        };
        lines.next();
        if kind != PatchLine::Added {
            old_left = old_left.saturating_sub(1);
        }
        if kind != PatchLine::Deleted {
            new_left = new_left.saturating_sub(1);
        }
        hunk.lines.push((kind, text.to_string()));
    }
    // `\ No newline at end of file` after the last line.
    while lines.peek().is_some_and(|line| line.starts_with('\\')) {
        lines.next();
    }
    Some(hunk)
}

/// The review's files from a patch alone: partial texts (see the module docs), any line open to
/// comments since the comments stay local.
pub fn review_files_from_patch(patch: &Patch) -> (Vec<ReviewFileContents>, Vec<UnavailableFile>) {
    let mut files = Vec::new();
    let mut unavailable = Vec::new();
    for file in &patch.files {
        let path = file.path().to_string();
        let change = file.change();
        let old_path = (change == FileChange::Renamed)
            .then(|| file.old_path.clone())
            .flatten();
        let mut skip = |reason: &str| {
            unavailable.push(UnavailableFile {
                path: path.clone(),
                old_path: old_path.clone(),
                change,
                reason: reason.to_string(),
            })
        };
        if file.binary {
            skip("Binary file");
            continue;
        }
        if file.hunks.is_empty() {
            skip("No changes to its text");
            continue;
        }
        // Paths come from the agent's worktree, so they're untrusted.
        let Some(relative_path) = RelPath::new(Path::new(&path), PathStyle::Unix)
            .ok()
            .filter(|relative_path| !relative_path.is_empty())
        else {
            skip("Unsupported path");
            continue;
        };
        let (base_text, head_text) = file.partial_texts();
        files.push(ReviewFileContents {
            path: relative_path.as_ref().into_arc(),
            old_path,
            base_text,
            head_text,
            commentable: None,
        });
    }
    (files, unavailable)
}

#[cfg(test)]
mod tests {
    use super::*;
    use pretty_assertions::assert_eq;

    const PATCH: &str = "\
diff --git a/src/main.rs b/src/main.rs
index 1111111..2222222 100644
--- a/src/main.rs
+++ b/src/main.rs
@@ -2,3 +2,4 @@ fn main() {
     let a = 1;
-    let b = 2;
+    let b = 3;
+    let c = 4;
 }
@@ -10,2 +11,1 @@
--- a comment that looks like a header
-old
+new
diff --git a/added.md b/added.md
new file mode 100644
--- /dev/null
+++ b/added.md
@@ -0,0 +1,2 @@
+# Hi
+there
\\ No newline at end of file
diff --git a/gone.txt b/gone.txt
deleted file mode 100644
--- a/gone.txt
+++ /dev/null
@@ -1 +0,0 @@
-bye
diff --git a/lib/old.rs b/lib/new.rs
similarity index 90%
rename from lib/old.rs
rename to lib/new.rs
diff --git a/logo.png b/logo.png
Binary files a/logo.png and b/logo.png differ
diff --git \"a/caf\\303\\251.txt\" \"b/caf\\303\\251.txt\"
--- \"a/caf\\303\\251.txt\"
+++ \"b/caf\\303\\251.txt\"
@@ -1 +1 @@
-x
+y
";

    #[test]
    fn parses_files_hunks_and_their_lines() {
        let patch = parse_patch(PATCH);
        assert!(!patch.truncated);
        let summary = patch
            .files
            .iter()
            .map(|file| {
                (
                    file.old_path.clone(),
                    file.new_path.clone(),
                    file.change(),
                    file.binary,
                    file.hunks.len(),
                )
            })
            .collect::<Vec<_>>();
        let some = |path: &str| Some(path.to_string());
        assert_eq!(
            summary,
            vec![
                (
                    some("src/main.rs"),
                    some("src/main.rs"),
                    FileChange::Modified,
                    false,
                    2
                ),
                (None, some("added.md"), FileChange::Added, false, 1),
                (some("gone.txt"), None, FileChange::Deleted, false, 1),
                (
                    some("lib/old.rs"),
                    some("lib/new.rs"),
                    FileChange::Renamed,
                    false,
                    0
                ),
                (
                    some("logo.png"),
                    some("logo.png"),
                    FileChange::Modified,
                    true,
                    0
                ),
                (
                    some("café.txt"),
                    some("café.txt"),
                    FileChange::Modified,
                    false,
                    1
                ),
            ]
        );
        assert_eq!(
            patch.files[0].hunks[1].lines,
            vec![
                (
                    PatchLine::Deleted,
                    "-- a comment that looks like a header".to_string()
                ),
                (PatchLine::Deleted, "old".to_string()),
                (PatchLine::Added, "new".to_string()),
            ]
        );
    }

    #[test]
    fn partial_texts_keep_line_numbers() {
        let patch = parse_patch(PATCH);
        let (base, head) = patch.files[0].partial_texts();
        assert_eq!(
            base.as_deref(),
            Some(
                "\n    let a = 1;\n    let b = 2;\n}\n\n\n\n\n\n-- a comment that looks like a header\nold\n"
            )
        );
        assert_eq!(
            head.as_deref(),
            Some("\n    let a = 1;\n    let b = 3;\n    let c = 4;\n}\n\n\n\n\n\nnew\n")
        );
        assert_eq!(head.unwrap().lines().nth(10), Some("new"), "line 11");
        assert_eq!(
            patch.files[1].partial_texts(),
            (None, Some("# Hi\nthere\n".to_string()))
        );
        assert_eq!(
            patch.files[2].partial_texts(),
            (Some("bye\n".to_string()), None)
        );
    }

    #[test]
    fn review_files_skip_what_a_patch_cant_show() {
        let mut text = PATCH.to_string();
        text.push_str(
            "diff --git a/../escape b/../escape\n--- a/../escape\n+++ b/../escape\n@@ -1 +1 @@\n-a\n+b\n",
        );
        text.push_str(TRUNCATED_MARKER);
        let patch = parse_patch(&text);
        assert!(patch.truncated);
        let (files, unavailable) = review_files_from_patch(&patch);
        assert_eq!(
            files
                .iter()
                .map(|file| (file.path.as_unix_str().to_string(), file.change()))
                .collect::<Vec<_>>(),
            vec![
                ("src/main.rs".to_string(), FileChange::Modified),
                ("added.md".to_string(), FileChange::Added),
                ("gone.txt".to_string(), FileChange::Deleted),
                ("café.txt".to_string(), FileChange::Modified),
            ]
        );
        assert!(files.iter().all(|file| file.commentable.is_none()));
        assert_eq!(
            unavailable
                .iter()
                .map(|file| (file.path.as_str(), file.reason.as_str()))
                .collect::<Vec<_>>(),
            vec![
                ("lib/new.rs", "No changes to its text"),
                ("logo.png", "Binary file"),
                ("../escape", "Unsupported path"),
            ]
        );
        assert_eq!(parse_patch(""), Patch::default());
    }
}

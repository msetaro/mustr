//! Reading an agent thread's worktree with git, for its "Changes": the branch against its base,
//! including changes not committed yet, as T3's own diff panel shows them
//! (`GitVcsDriverCore.ts`, `getReviewDiffPreview`).
//!
//! Only read commands run, and none of them writes: `--no-optional-locks` keeps `git diff` from
//! refreshing the index, and hooks, fsmonitor, external diff and textconv drivers are off, so a
//! worktree's config can't make mustr run anything.

use anyhow::{Context as _, Result, anyhow, bail};
use std::path::{Path, PathBuf};

/// Files larger than this aren't shown.
pub const MAX_FILE_BYTES: usize = 2 * 1024 * 1024;

/// The comparison bases T3 falls back to after the branch's configured base
/// (`DEFAULT_BASE_BRANCH_CANDIDATES`).
const DEFAULT_BASES: [&str; 4] = [
    "refs/remotes/origin/main",
    "refs/remotes/origin/master",
    "refs/heads/main",
    "refs/heads/master",
];

/// A git checkout on this machine.
#[derive(Clone, Debug)]
pub struct LocalGit {
    root: PathBuf,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ChangeKind {
    Modified,
    Added,
    Deleted,
    Renamed { old_path: String },
}

/// A file that differs between the merge base and the working tree.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ChangedPath {
    pub path: String,
    pub kind: ChangeKind,
}

/// What the branch is compared against.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Comparison {
    /// The base's name, such as `origin/main`, or `None` when there's no base and only
    /// uncommitted changes are shown.
    pub base_name: Option<String>,
    /// The commit the branch left the base at.
    pub merge_base: String,
}

/// A file's bytes at a revision, or why they can't be shown.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum FileText {
    Text(String),
    Missing,
    Unavailable(String),
}

impl LocalGit {
    /// The checkout `path` is in, if it's on this machine and git knows it.
    pub async fn open(path: &Path) -> Option<Self> {
        if !path.is_dir() {
            return None;
        }
        let output = run_git(path, &["rev-parse", "--show-toplevel"])
            .await
            .ok()?;
        let root = PathBuf::from(output.trim_end_matches(['\n', '\r']));
        root.is_dir().then_some(Self { root })
    }

    pub fn root(&self) -> &Path {
        &self.root
    }

    async fn git(&self, args: &[&str]) -> Result<String> {
        run_git(&self.root, args).await
    }

    async fn git_succeeds(&self, args: &[&str]) -> bool {
        self.git(args).await.is_ok()
    }

    /// The checked-out branch, or `None` when HEAD is detached.
    pub async fn current_branch(&self) -> Option<String> {
        let branch = self
            .git(&["symbolic-ref", "--quiet", "--short", "HEAD"])
            .await
            .ok()?;
        let branch = branch.trim();
        (!branch.is_empty()).then(|| branch.to_string())
    }

    async fn config(&self, key: &str) -> Option<String> {
        let value = self.git(&["config", "--get", key]).await.ok()?;
        let value = value.trim();
        (!value.is_empty()).then(|| value.to_string())
    }

    async fn commit_exists(&self, rev: &str) -> bool {
        // A ref name, not an option: `--end-of-options` keeps a hostile one from being read as
        // a flag.
        self.git_succeeds(&[
            "rev-parse",
            "--verify",
            "--quiet",
            "--end-of-options",
            &format!("{rev}^{{commit}}"),
        ])
        .await
    }

    /// The ref the branch is compared against, following T3's order: the branch's configured
    /// base (`branch.<name>.t3-merge-base-ref`, then `t3-merge-base` or `gh-merge-base`), the
    /// remote's default branch, then `main` or `master`.
    pub async fn base_ref(&self, branch: Option<&str>) -> Option<String> {
        if let Some(branch) = branch {
            if let Some(configured) = self
                .config(&format!("branch.{branch}.t3-merge-base-ref"))
                .await
                .filter(|base| base.starts_with("refs/heads/") || base.starts_with("refs/remotes/"))
                && self.commit_exists(&configured).await
            {
                return Some(configured);
            }
            let configured = match self.config(&format!("branch.{branch}.t3-merge-base")).await {
                Some(base) => Some(base),
                None => self.config(&format!("branch.{branch}.gh-merge-base")).await,
            };
            if let Some(configured) = configured {
                let name = configured.trim_start_matches("origin/");
                for candidate in [
                    format!("refs/heads/{configured}"),
                    format!("refs/remotes/origin/{name}"),
                    configured.clone(),
                ] {
                    if self.commit_exists(&candidate).await {
                        return Some(candidate);
                    }
                }
            }
        }
        if let Some(default) = self
            .git(&["symbolic-ref", "--quiet", "refs/remotes/origin/HEAD"])
            .await
            .ok()
            .map(|default| default.trim().to_string())
            .filter(|default| !default.is_empty())
            && self.commit_exists(&default).await
        {
            return Some(default);
        }
        for candidate in DEFAULT_BASES {
            if self.commit_exists(candidate).await {
                return Some(candidate.to_string());
            }
        }
        None
    }

    /// Where the branch left its base. Without a base, the comparison is against HEAD, which
    /// shows only uncommitted changes.
    pub async fn comparison(&self, branch: Option<&str>) -> Result<Comparison> {
        let Some(base) = self.base_ref(branch).await else {
            let head = self.git(&["rev-parse", "--verify", "HEAD"]).await?;
            return Ok(Comparison {
                base_name: None,
                merge_base: head.trim().to_string(),
            });
        };
        let merge_base = self
            .git(&["merge-base", "--end-of-options", &base, "HEAD"])
            .await
            .with_context(|| format!("finding where the branch left {base}"))?;
        Ok(Comparison {
            base_name: Some(short_ref_name(&base)),
            merge_base: merge_base.trim().to_string(),
        })
    }

    /// The files that differ between `merge_base` and the working tree, untracked files
    /// included, in git's order.
    pub async fn changed_paths(&self, merge_base: &str) -> Result<Vec<ChangedPath>> {
        let tracked = self
            .git(&[
                "diff",
                "--name-status",
                "-z",
                "-M",
                "--no-ext-diff",
                "--no-textconv",
                "--end-of-options",
                merge_base,
                "--",
            ])
            .await?;
        let mut paths = parse_name_status(&tracked);
        let untracked = self
            .git(&["ls-files", "--others", "--exclude-standard", "-z"])
            .await?;
        paths.extend(
            untracked
                .split('\0')
                .filter(|path| !path.is_empty())
                .map(|path| ChangedPath {
                    path: path.to_string(),
                    kind: ChangeKind::Added,
                }),
        );
        Ok(paths)
    }

    /// A file's text at `rev`.
    pub async fn show(&self, rev: &str, path: &str) -> Result<FileText> {
        let output = run_git_bytes(
            &self.root,
            &["show", "--no-textconv", &format!("{rev}:{path}")],
        )
        .await;
        match output {
            Ok(bytes) => Ok(text_or_reason(bytes)),
            // `git show` fails for a path the revision doesn't have.
            Err(_) => Ok(FileText::Missing),
        }
    }

    /// A file's text in the working tree. Paths come from git and are relative to the
    /// checkout; one that leaves it (through `..` or a symlink) isn't read.
    pub fn read_working_file(&self, path: &str) -> Result<FileText> {
        let full_path = self.root.join(path);
        let canonical = match full_path.canonicalize() {
            Ok(canonical) => canonical,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                return Ok(FileText::Missing);
            }
            Err(error) => return Err(error).with_context(|| format!("reading {path}")),
        };
        let root = self.root.canonicalize()?;
        if !canonical.starts_with(&root) {
            return Ok(FileText::Unavailable("Outside the worktree".to_string()));
        }
        let metadata = std::fs::metadata(&canonical)?;
        if !metadata.is_file() {
            return Ok(FileText::Unavailable("Not a file".to_string()));
        }
        if metadata.len() as usize > MAX_FILE_BYTES {
            return Ok(FileText::Unavailable("Too large to show".to_string()));
        }
        Ok(text_or_reason(std::fs::read(&canonical)?))
    }

    /// How many files the branch changed against its base, for the inbox.
    pub async fn changed_file_count(&self, branch: Option<&str>) -> Result<usize> {
        let comparison = self.comparison(branch).await?;
        Ok(self.changed_paths(&comparison.merge_base).await?.len())
    }
}

fn text_or_reason(bytes: Vec<u8>) -> FileText {
    if bytes.len() > MAX_FILE_BYTES {
        return FileText::Unavailable("Too large to show".to_string());
    }
    // Git's own test for binary content: a NUL in the first 8000 bytes.
    if bytes.iter().take(8000).any(|byte| *byte == 0) {
        return FileText::Unavailable("Binary file".to_string());
    }
    match String::from_utf8(bytes) {
        Ok(text) => FileText::Text(text),
        Err(_) => FileText::Unavailable("Not UTF-8 text".to_string()),
    }
}

/// `refs/remotes/origin/main` → `origin/main`, `refs/heads/main` → `main`.
pub fn short_ref_name(reference: &str) -> String {
    reference
        .strip_prefix("refs/remotes/")
        .or_else(|| reference.strip_prefix("refs/heads/"))
        .unwrap_or(reference)
        .to_string()
}

/// `git diff --name-status -z`: a status, then one path (two for renames and copies), each
/// NUL-terminated.
fn parse_name_status(output: &str) -> Vec<ChangedPath> {
    let mut fields = output.split('\0').filter(|field| !field.is_empty());
    let mut paths = Vec::new();
    while let Some(status) = fields.next() {
        let kind = status.chars().next().unwrap_or('M');
        match kind {
            'R' | 'C' => {
                let (Some(old_path), Some(path)) = (fields.next(), fields.next()) else {
                    break;
                };
                paths.push(ChangedPath {
                    path: path.to_string(),
                    kind: if kind == 'R' {
                        ChangeKind::Renamed {
                            old_path: old_path.to_string(),
                        }
                    } else {
                        ChangeKind::Added
                    },
                });
            }
            _ => {
                let Some(path) = fields.next() else {
                    break;
                };
                paths.push(ChangedPath {
                    path: path.to_string(),
                    kind: match kind {
                        'A' => ChangeKind::Added,
                        'D' => ChangeKind::Deleted,
                        _ => ChangeKind::Modified,
                    },
                });
            }
        }
    }
    paths
}

async fn run_git(cwd: &Path, args: &[&str]) -> Result<String> {
    let bytes = run_git_bytes(cwd, args).await?;
    String::from_utf8(bytes).map_err(|_| anyhow!("git printed something that isn't UTF-8"))
}

async fn run_git_bytes(cwd: &Path, args: &[&str]) -> Result<Vec<u8>> {
    let mut command = util::command::new_command("git");
    command
        .current_dir(cwd)
        .args([
            "--no-optional-locks",
            "--no-pager",
            "-c",
            "core.fsmonitor=false",
            "-c",
            "core.hooksPath=/dev/null",
            "-c",
            "diff.external=",
            "-c",
            "core.quotePath=false",
        ])
        .args(args)
        .env("GIT_OPTIONAL_LOCKS", "0")
        .env("GIT_TERMINAL_PROMPT", "0")
        .stdin(util::command::Stdio::null())
        .kill_on_drop(true);
    let output = command
        .output()
        .await
        .context("running git (is it installed?)")?;
    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        bail!(
            "git {} failed: {}",
            args.first().copied().unwrap_or_default(),
            stderr.trim()
        );
    }
    Ok(output.stdout)
}

#[cfg(test)]
mod tests {
    use super::*;
    use pretty_assertions::assert_eq;

    #[test]
    fn name_status_output_is_parsed() {
        let output = "M\0src/main.rs\0A\0new file.md\0D\0gone.txt\0R087\0lib/old.rs\0lib/new.rs\0C100\0a.rs\0b.rs\0T\0link\0";
        let changed = |path: &str, kind| ChangedPath {
            path: path.to_string(),
            kind,
        };
        assert_eq!(
            parse_name_status(output),
            vec![
                changed("src/main.rs", ChangeKind::Modified),
                changed("new file.md", ChangeKind::Added),
                changed("gone.txt", ChangeKind::Deleted),
                changed(
                    "lib/new.rs",
                    ChangeKind::Renamed {
                        old_path: "lib/old.rs".to_string()
                    }
                ),
                changed("b.rs", ChangeKind::Added),
                changed("link", ChangeKind::Modified),
            ]
        );
        assert_eq!(parse_name_status(""), Vec::new());
    }

    #[test]
    fn texts_binaries_and_ref_names() {
        assert_eq!(
            text_or_reason(b"hi\n".to_vec()),
            FileText::Text("hi\n".to_string())
        );
        assert_eq!(
            text_or_reason(vec![0x89, b'P', 0, 1]),
            FileText::Unavailable("Binary file".to_string())
        );
        assert_eq!(short_ref_name("refs/remotes/origin/main"), "origin/main");
        assert_eq!(short_ref_name("refs/heads/main"), "main");
        assert_eq!(short_ref_name("HEAD"), "HEAD");
    }

    /// Runs real git on a temporary repository: the branch against its base, with a commit,
    /// an uncommitted change, a new untracked file, and a deletion. Nothing it runs writes.
    // Setting up the repository blocks on git, which is fine in a test.
    #[allow(clippy::disallowed_methods)]
    #[test]
    fn changes_against_the_base() {
        let dir = tempfile::tempdir().expect("a temporary directory");
        let root = dir.path();
        let git = |args: &[&str]| {
            let status = std::process::Command::new("git")
                .current_dir(root)
                .args([
                    "-c",
                    "user.name=mustr",
                    "-c",
                    "user.email=mustr@example.com",
                ])
                .args(args)
                .output()
                .expect("git runs");
            assert!(status.status.success(), "git {args:?}: {status:?}");
        };
        git(&["init", "-q", "-b", "main"]);
        std::fs::write(root.join("kept.rs"), "fn a() {}\n").unwrap();
        std::fs::write(root.join("gone.txt"), "bye\n").unwrap();
        git(&["add", "."]);
        git(&["commit", "-q", "-m", "base"]);
        git(&["checkout", "-q", "-b", "agent/work"]);
        std::fs::write(root.join("kept.rs"), "fn a() {}\nfn b() {}\n").unwrap();
        git(&["commit", "-q", "-am", "agent"]);
        std::fs::write(root.join("kept.rs"), "fn a() {}\nfn b() {}\nfn c() {}\n").unwrap();
        std::fs::remove_file(root.join("gone.txt")).unwrap();
        std::fs::write(root.join("new.md"), "# New\n").unwrap();
        let before = std::fs::read(root.join(".git/index")).unwrap();

        let (comparison, changed, base, head, missing) = futures::executor::block_on({
            let root = root.to_path_buf();
            async move {
                let local = LocalGit::open(&root.join("."))
                    .await
                    .expect("a git checkout");
                let branch = local.current_branch().await;
                assert_eq!(branch.as_deref(), Some("agent/work"));
                let comparison = local.comparison(branch.as_deref()).await.unwrap();
                let changed = local.changed_paths(&comparison.merge_base).await.unwrap();
                let base = local.show(&comparison.merge_base, "kept.rs").await.unwrap();
                let head = local.read_working_file("kept.rs").unwrap();
                let missing = local.show(&comparison.merge_base, "new.md").await.unwrap();
                assert_eq!(
                    local.changed_file_count(branch.as_deref()).await.unwrap(),
                    3
                );
                (comparison, changed, base, head, missing)
            }
        });
        assert_eq!(comparison.base_name.as_deref(), Some("main"));
        assert_eq!(
            changed,
            vec![
                ChangedPath {
                    path: "gone.txt".to_string(),
                    kind: ChangeKind::Deleted
                },
                ChangedPath {
                    path: "kept.rs".to_string(),
                    kind: ChangeKind::Modified
                },
                ChangedPath {
                    path: "new.md".to_string(),
                    kind: ChangeKind::Added
                },
            ]
        );
        assert_eq!(base, FileText::Text("fn a() {}\n".to_string()));
        assert_eq!(
            head,
            FileText::Text("fn a() {}\nfn b() {}\nfn c() {}\n".to_string())
        );
        assert_eq!(missing, FileText::Missing);
        assert_eq!(
            std::fs::read(root.join(".git/index")).unwrap(),
            before,
            "reading the changes doesn't touch the index"
        );
    }
}

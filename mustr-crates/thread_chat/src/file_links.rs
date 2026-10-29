//! Links in the timeline's Markdown, as T3's `ChatMarkdown` opens them
//! (`packages/shared/src/markdownLinks.ts`, `fileLinks.ts` and `chat/MarkdownFileLink.tsx` in
//! T3):
//! - A link to a file (`[main.rs](src/main.rs:12)`, `/abs/path#L12C3`, `file://` URLs) or a
//!   path in inline code (`` `src/main.rs:12` ``) opens in mustr's editor at that line, when the
//!   thread's folder is on this machine, or the thread's review when it's on T3's.
//! - A web link opens in the system browser.
//! - A quote of an agent's message (`t3-citation:`) shows that message.
//! - A context chip (`t3-context:`) opens nothing: it's the record the message carried.
//!
//! What counts as a file is T3's rule exactly, so the same text links in both apps: model ids
//! like `z-ai/glm-5.3`, branches like `origin/main` and hostnames stay plain code.

use crate::{actions::ReviewThreadChanges, mentions::Folder, thread_view::ThreadView};
use anyhow::Context as _;
use editor::Editor;
use gpui::{Context, TaskExt as _, Window};
use language::Point;
use std::path::{Component, Path, PathBuf};
use util::rel_path::RelPath;
use workspace::{OpenOptions, OpenVisible, Workspace};

/// A file a link names, with where to go in it. Lines and columns count from 1.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct FileTarget {
    pub path: String,
    pub line: Option<u32>,
    pub column: Option<u32>,
}

/// What a link in the timeline is.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum Link {
    File(FileTarget),
    /// A web page, or anything else with a scheme the system opens.
    External(String),
    /// A quote of an agent's message, by the message's id.
    Citation {
        message_id: String,
    },
    /// A context chip's reference.
    Context,
    /// Nothing mustr can open, such as an anchor.
    None,
}

const CITATION_HREF_PREFIX: &str = "t3-citation://v1/";

const POSIX_FILE_ROOT_PREFIXES: [&str; 24] = [
    "/Users/",
    "/home/",
    "/tmp/",
    "/var/",
    "/etc/",
    "/opt/",
    "/mnt/",
    "/Volumes/",
    "/private/",
    "/root/",
    "/usr/",
    "/bin/",
    "/sbin/",
    "/lib/",
    "/lib64/",
    "/srv/",
    "/dev/",
    "/proc/",
    "/sys/",
    "/run/",
    "/boot/",
    "/media/",
    "/workspace/",
    "/workspaces/",
];

const EXTENSIONLESS_FILE_NAMES: [&str; 27] = [
    "Makefile",
    "makefile",
    "GNUmakefile",
    "Dockerfile",
    "Containerfile",
    "Justfile",
    "justfile",
    "Rakefile",
    "Gemfile",
    "Procfile",
    "Brewfile",
    "Caddyfile",
    "Vagrantfile",
    "Jenkinsfile",
    "Podfile",
    "Fastfile",
    "BUILD",
    "WORKSPACE",
    "LICENSE",
    "LICENCE",
    "COPYING",
    "NOTICE",
    "AUTHORS",
    "CONTRIBUTORS",
    "CHANGELOG",
    "README",
    "CODEOWNERS",
];

const GENERIC_HOSTNAME_TLDS: [&str; 25] = [
    "com", "net", "org", "io", "dev", "app", "ai", "co", "edu", "gov", "mil", "info", "biz", "xyz",
    "me", "tv", "cc", "gg", "chat", "cloud", "site", "online", "tech", "store", "link",
];

const COUNTRY_HOSTNAME_TLDS: [&str; 31] = [
    "uk", "de", "fr", "nl", "se", "no", "fi", "dk", "pl", "ch", "at", "be", "es", "it", "pt", "eu",
    "us", "ca", "au", "nz", "jp", "kr", "cn", "br", "ru", "mx", "ie", "cz", "tr", "sg", "hk",
];

/// `%XX` decoded, or `text` itself when it isn't valid encoding (`safeDecodeURIComponent`).
fn decode(text: &str) -> String {
    crate::mentions::percent_decode(text).unwrap_or_else(|| text.to_string())
}

fn is_digits(text: &str) -> bool {
    !text.is_empty() && text.bytes().all(|byte| byte.is_ascii_digit())
}

/// `text` without a trailing `:<digits>`, and the digits.
fn strip_number_suffix(text: &str) -> Option<(&str, &str)> {
    let (rest, number) = text.rsplit_once(':')?;
    is_digits(number).then_some((rest, number))
}

/// `text` without up to two trailing `:<digits>` (`(?::\d+){0,2}$`).
fn strip_positions(text: &str) -> &str {
    let mut text = text;
    for _ in 0..2 {
        match strip_number_suffix(text) {
            Some((rest, _)) => text = rest,
            None => break,
        }
    }
    text
}

/// Whether `text` ends in `:line` or `:line:column` (`POSITION_SUFFIX_PATTERN`).
fn has_position_suffix(text: &str) -> bool {
    strip_number_suffix(text).is_some()
}

/// `text` without its `:line[:column]` (`POSITION_SUFFIX_PATTERN` replaced).
fn without_position_suffix(text: &str) -> &str {
    let Some((rest, _)) = strip_number_suffix(text) else {
        return text;
    };
    strip_number_suffix(rest).map_or(rest, |(rest, _)| rest)
}

fn is_segment_char(c: char) -> bool {
    c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '-')
}

/// Words of `[A-Za-z0-9._-]`, with spaces between them (`[A-Za-z0-9._-]+(?: +[…]+)*`).
fn is_segment_words(text: &str) -> bool {
    !text.is_empty()
        && !text.starts_with(' ')
        && !text.ends_with(' ')
        && text.chars().all(|c| is_segment_char(c) || c == ' ')
}

fn has_extension(name: &str) -> bool {
    name.rsplit_once('.').is_some_and(|(_, extension)| {
        !extension.is_empty()
            && extension
                .bytes()
                .all(|byte| byte.is_ascii_alphanumeric() || byte == b'_' || byte == b'-')
    })
}

fn has_relative_prefix(path: &str) -> bool {
    path.starts_with("~/") || path.starts_with("./") || path.starts_with("../")
}

/// `dir/…/name`, maybe with a position, or a folder ending in `/` (`RELATIVE_FILE_PATH_PATTERN`).
fn is_relative_file_path(path: &str) -> bool {
    let Some((dirs, last)) = path.rsplit_once('/') else {
        return false;
    };
    dirs.split('/').all(is_segment_words)
        && (last.is_empty() || is_segment_words(strip_positions(last)))
}

/// `name.ext`, maybe with a position (`RELATIVE_FILE_NAME_PATTERN`).
fn is_relative_file_name(path: &str) -> bool {
    let name = strip_positions(path);
    is_segment_words(name)
        && name
            .rsplit_once('.')
            .is_some_and(|(stem, _)| !stem.is_empty() && has_extension(name))
}

fn looks_like_posix_path(path: &str) -> bool {
    if !path.starts_with('/') {
        return false;
    }
    if POSIX_FILE_ROOT_PREFIXES
        .iter()
        .any(|prefix| path.starts_with(prefix))
        || has_position_suffix(path)
    {
        return true;
    }
    let name = path.rsplit('/').next().unwrap_or(path);
    EXTENSIONLESS_FILE_NAMES.contains(&name) || has_extension(name)
}

fn looks_like_file_path(path: &str, authored: &str) -> bool {
    if has_relative_prefix(path) {
        return true;
    }
    if path.starts_with('/') {
        return looks_like_posix_path(authored);
    }
    EXTENSIONLESS_FILE_NAMES.contains(&path)
        || is_relative_file_path(authored)
        || is_relative_file_name(path)
}

/// Whether `path` starts with a URL scheme, which `name.rs:12` doesn't (`hasExternalScheme`).
fn has_external_scheme(path: &str) -> bool {
    let Some((scheme, rest)) = path.split_once(':') else {
        return false;
    };
    let mut chars = scheme.chars();
    let is_scheme = chars.next().is_some_and(|c| c.is_ascii_alphabetic())
        && chars.all(|c| c.is_ascii_alphanumeric() || matches!(c, '+' | '.' | '-'));
    if !is_scheme {
        return false;
    }
    if rest.starts_with("//") {
        return true;
    }
    let position_only = match rest.split_once(':') {
        Some((line, column)) => is_digits(line) && is_digits(column),
        None => is_digits(rest),
    };
    !position_only
}

fn positive(number: &str) -> Option<u32> {
    number.parse::<u32>().ok().filter(|number| *number > 0)
}

/// The path and its `:line[:column]`, or its `#L12C3` anchor (`splitFilePathPosition`).
fn split_position(path: &str, hash: &str) -> FileTarget {
    if let Some((rest, last)) = strip_number_suffix(path) {
        let (path, line, column) = match strip_number_suffix(rest) {
            Some((path, line)) => (path, line, Some(last)),
            None => (rest, last, None),
        };
        return FileTarget {
            path: path.to_string(),
            line: positive(line),
            column: column.and_then(positive),
        };
    }
    let anchor = hash
        .strip_prefix("#L")
        .or_else(|| hash.strip_prefix("#l"))
        .and_then(|anchor| {
            let (line, column) = match anchor.find(['C', 'c']) {
                Some(index) => (&anchor[..index], Some(&anchor[index + 1..])),
                None => (anchor, None),
            };
            (is_digits(line) && column.is_none_or(is_digits)).then_some((line, column))
        });
    match anchor {
        Some((line, column)) => FileTarget {
            path: path.to_string(),
            line: positive(line),
            column: column.and_then(positive),
        },
        None => FileTarget {
            path: path.to_string(),
            line: None,
            column: None,
        },
    }
}

/// A `file:` URL's path (still encoded) and anchor, for this machine's files only: T3 makes
/// other hosts Windows shares, which mustr doesn't open (`parseFileUrlHref`).
fn parse_file_url(href: &str) -> Option<(String, String)> {
    let rest = &href["file:".len()..];
    let (path, hash) = match rest.find('#') {
        Some(index) => (&rest[..index], &rest[index..]),
        None => (rest, ""),
    };
    let path = path.split('?').next().unwrap_or_default();
    let path = match path.strip_prefix("//") {
        Some(authority_and_path) => {
            let (host, path) = match authority_and_path.find('/') {
                Some(index) => authority_and_path.split_at(index),
                None => (authority_and_path, ""),
            };
            if !host.is_empty() && !host.eq_ignore_ascii_case("localhost") {
                return None;
            }
            path
        }
        None => path,
    };
    (!path.is_empty()).then(|| (path.to_string(), hash.to_string()))
}

/// The file a link's destination names, if it names one (`parseMarkdownFileLink`).
pub(crate) fn parse_file_link(href: &str) -> Option<FileTarget> {
    let trimmed = href.trim();
    let normalized = trimmed
        .strip_prefix('<')
        .and_then(|inner| inner.strip_suffix('>'))
        .unwrap_or(trimmed);
    if normalized.is_empty() || normalized.starts_with('#') || normalized.starts_with("//") {
        return None;
    }
    let is_file_url = normalized
        .get(..5)
        .is_some_and(|scheme| scheme.eq_ignore_ascii_case("file:"));
    let (path, hash) = if is_file_url {
        parse_file_url(normalized)?
    } else {
        let (path, hash) = match normalized.find('#') {
            Some(index) => (&normalized[..index], &normalized[index..]),
            None => (normalized, ""),
        };
        let path = path.split('?').next().unwrap_or_default();
        (path.to_string(), hash.to_string())
    };
    let path = decode(path.trim());
    let hash = decode(hash.trim());
    if path.is_empty() || has_external_scheme(&path) {
        return None;
    }
    let target = split_position(&path, &hash);
    looks_like_file_path(&target.path, &path).then_some(target)
}

fn looks_like_hostname(segment: &str, has_position: bool) -> bool {
    if segment.starts_with('.') {
        return false;
    }
    let lowered = segment.to_ascii_lowercase();
    if lowered == "localhost" {
        return true;
    }
    let labels = lowered.split('.').collect::<Vec<_>>();
    if labels.len() >= 2 && labels.iter().all(|label| is_digits(label)) {
        return true;
    }
    let Some(last) = labels.last().filter(|_| labels.len() >= 2) else {
        return false;
    };
    GENERIC_HOSTNAME_TLDS.contains(last) || (!has_position && COUNTRY_HOSTNAME_TLDS.contains(last))
}

/// Whether a file name ends like a version, as `glm-5.3` does (`\d\.\d[^.]*$`).
fn has_version_suffix(name: &str) -> bool {
    let bytes = name.as_bytes();
    (0..bytes.len().saturating_sub(2)).any(|index| {
        bytes[index].is_ascii_digit()
            && bytes[index + 1] == b'.'
            && bytes[index + 2].is_ascii_digit()
            && !bytes[index + 3..].contains(&b'.')
    })
}

/// The path inline code names, if it reads as one (`inlineCodeFilePathCandidate`), as the
/// link's destination.
pub(crate) fn inline_code_link(code: &str) -> Option<String> {
    let trimmed = code.trim();
    if trimmed.is_empty() || trimmed.contains(|c: char| c.is_whitespace() || c == '`') {
        return None;
    }
    let candidate = trimmed.replace('\\', "/");
    let has_position = has_position_suffix(&candidate);
    if !has_position && !candidate.contains('/') {
        return None;
    }
    let explicit = has_relative_prefix(&candidate) || candidate.starts_with('/');
    if !explicit {
        let without_position = without_position_suffix(&candidate);
        if without_position.ends_with('/') {
            return None;
        }
        let first = without_position
            .split('/')
            .next()
            .unwrap_or(without_position);
        if looks_like_hostname(first, has_position) {
            return None;
        }
        let name = without_position
            .trim_end_matches('/')
            .rsplit('/')
            .next()
            .unwrap_or_default();
        if has_version_suffix(name) || (!has_position && !has_extension(name)) {
            return None;
        }
    }
    parse_file_link(&candidate).map(|_| candidate)
}

/// What a link's destination is.
pub(crate) fn classify(url: &str) -> Link {
    if let Some(path) = url.strip_prefix(CITATION_HREF_PREFIX) {
        let path = path.split(['?', '#']).next().unwrap_or_default();
        let parts = path.split('/').collect::<Vec<_>>();
        return match parts.as_slice() {
            [_, _, message_id] if !message_id.is_empty() => Link::Citation {
                message_id: decode(message_id),
            },
            _ => Link::None,
        };
    }
    if url.starts_with("t3-context:") {
        return Link::Context;
    }
    if let Some(target) = parse_file_link(url) {
        return Link::File(target);
    }
    if has_external_scheme(url.trim()) {
        return Link::External(url.trim().to_string());
    }
    Link::None
}

/// `path` with `.` and `..` resolved, without touching the disk.
fn normalize(path: &Path) -> PathBuf {
    let mut normalized = PathBuf::new();
    for component in path.components() {
        match component {
            Component::CurDir => {}
            Component::ParentDir => {
                normalized.pop();
            }
            other => normalized.push(other),
        }
    }
    normalized
}

/// The absolute path a link names, from the thread's folder (`resolvePathLinkTarget`): `~/` is
/// the home folder, and relative paths are in `folder`.
pub(crate) fn resolve(path: &str, folder: &Path, home: Option<&Path>) -> PathBuf {
    let path = path.trim_end_matches('/');
    let resolved = if let Some(rest) = path.strip_prefix("~/") {
        match home {
            Some(home) => home.join(rest),
            None => PathBuf::from(path),
        }
    } else if Path::new(path).is_absolute() {
        PathBuf::from(path)
    } else {
        folder.join(path)
    };
    normalize(&resolved)
}

/// What opening a file link does.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum FileOpen {
    /// Opens the file in mustr's editor: `folder` is the thread's, which the project gets as a
    /// visible worktree so Zed's Restricted Mode applies, when the file is in it. `position`
    /// counts from 0.
    Editor {
        folder: Option<PathBuf>,
        path: PathBuf,
        position: Option<Point>,
    },
    /// Adds a folder to the project, and shows Zed's file finder over it.
    Folder(PathBuf),
    /// The thread's folder is on T3's machine: its review shows the agent's changes.
    Review,
    /// Nothing is at the path.
    Missing(String),
}

/// Where a link to `target` goes, for a thread working in `folder`.
pub(crate) fn file_open(target: &FileTarget, folder: &Folder, home: Option<&Path>) -> FileOpen {
    let root: &Path = match folder {
        Folder::Local(root) => root,
        Folder::Remote(_) => return FileOpen::Review,
        Folder::Unknown if Path::new(&target.path).is_absolute() => Path::new("/"),
        Folder::Unknown => return FileOpen::Missing(target.path.clone()),
    };
    let path = resolve(&target.path, root, home);
    if path.is_dir() {
        return FileOpen::Folder(path);
    }
    if !path.is_file() {
        return FileOpen::Missing(target.path.clone());
    }
    let position = target.line.map(|line| {
        Point::new(
            line.saturating_sub(1),
            target.column.unwrap_or(1).saturating_sub(1),
        )
    });
    FileOpen::Editor {
        folder: matches!(folder, Folder::Local(_))
            .then(|| root.to_path_buf())
            .filter(|root| path.starts_with(root)),
        path,
        position,
    }
}

impl ThreadView {
    /// Opens a link the timeline's Markdown was clicked on.
    pub(crate) fn open_link(&mut self, url: &str, window: &mut Window, cx: &mut Context<Self>) {
        match classify(url) {
            Link::File(target) => {
                let folder = self.mention_folder(cx);
                let open = file_open(&target, &folder, Some(util::paths::home_dir().as_path()));
                self.open_file(open, window, cx);
            }
            Link::External(url) => cx.open_url(&url),
            Link::Citation { message_id } => self.reveal_message(&message_id, cx),
            Link::Context | Link::None => {}
        }
    }

    /// Scrolls the timeline to a message, by its message id.
    pub(crate) fn reveal_message(&mut self, message_id: &str, cx: &mut Context<Self>) {
        let item_id = self.thread_state().and_then(|state| {
            state
                .items()
                .iter()
                .find(|item| item.entry_id == message_id)
                .map(|item| item.id.clone())
        });
        if let Some(row) = item_id.and_then(|item_id| self.row_of_item(&item_id)) {
            self.list_state.scroll_to_reveal_item(row);
            cx.notify();
        }
    }

    fn open_file(&mut self, open: FileOpen, window: &mut Window, cx: &mut Context<Self>) {
        let Some(workspace) = Workspace::for_window(window, cx) else {
            return;
        };
        match open {
            FileOpen::Editor {
                folder,
                path,
                position,
            } => open_in_editor(&workspace, folder, path, position, window, cx),
            FileOpen::Folder(folder) => workspace.update(cx, |workspace, cx| {
                let add = workspace.project().update(cx, |project, cx| {
                    project.find_or_create_worktree(&folder, true, cx)
                });
                cx.spawn_in(window, async move |workspace, cx| {
                    add.await
                        .with_context(|| format!("opening {}", folder.display()))?;
                    workspace.update_in(cx, |_, window, cx| {
                        window.dispatch_action(Box::new(workspace::ToggleFileFinder::default()), cx)
                    })
                })
                .detach_and_log_err(cx);
            }),
            FileOpen::Review => {
                if let Some(thread_id) = self.thread_id.clone() {
                    window.dispatch_action(Box::new(ReviewThreadChanges { thread_id }), cx);
                }
            }
            FileOpen::Missing(path) => {
                self.error = Some(format!("There's no {path} in the thread's folder.").into());
                cx.notify();
            }
        }
    }
}

/// Opens `path` in mustr's editor, at `position`. A file in the thread's `folder` opens in that
/// folder's worktree, added visible as the review adds it, so Zed's Restricted Mode applies;
/// another one opens on its own, also visible.
fn open_in_editor(
    workspace: &gpui::Entity<Workspace>,
    folder: Option<PathBuf>,
    path: PathBuf,
    position: Option<Point>,
    window: &mut Window,
    cx: &mut Context<ThreadView>,
) {
    workspace.update(cx, |workspace, cx| {
        let opened = match folder {
            Some(folder) => {
                let add = workspace.project().update(cx, |project, cx| {
                    project.find_or_create_worktree(&folder, true, cx)
                });
                let relative = path.strip_prefix(&folder).map(Path::to_path_buf);
                cx.spawn_in(window, async move |workspace, cx| {
                    let (worktree, _) = add
                        .await
                        .with_context(|| format!("opening {}", folder.display()))?;
                    let relative =
                        RelPath::new(&relative?, util::paths::PathStyle::local())?.into_arc();
                    let worktree_id = cx.update(|_, cx| worktree.read(cx).id())?;
                    workspace
                        .update_in(cx, |workspace, window, cx| {
                            workspace.open_path(
                                project::ProjectPath {
                                    worktree_id,
                                    path: relative,
                                },
                                None,
                                true,
                                window,
                                cx,
                            )
                        })?
                        .await
                })
            }
            None => workspace.open_abs_path(
                path,
                OpenOptions {
                    visible: Some(OpenVisible::All),
                    focus: Some(true),
                    ..Default::default()
                },
                window,
                cx,
            ),
        };
        cx.spawn_in(window, async move |_, cx| {
            let item = opened.await?;
            if let (Some(position), Some(editor)) = (position, item.downcast::<Editor>()) {
                editor.update_in(cx, |editor, window, cx| {
                    editor.go_to_singleton_buffer_point(position, window, cx)
                })?;
            }
            anyhow::Ok(())
        })
        .detach_and_log_err(cx);
    });
}

#[cfg(test)]
mod tests {
    use super::*;
    use pretty_assertions::assert_eq;

    fn target(path: &str, line: Option<u32>, column: Option<u32>) -> FileTarget {
        FileTarget {
            path: path.into(),
            line,
            column,
        }
    }

    #[test]
    fn inline_code_links_only_what_t3_links() {
        // T3's `inlineCodeFilePathCandidate` cases.
        for (code, link) in [
            ("src\\main.ts", Some("src/main.ts")),
            ("origin/main", None),
            ("z-ai/glm-5.3", None),
            ("share/man/ls.1", Some("share/man/ls.1")),
            ("src/main.ts/", None),
            ("./patches/", Some("./patches/")),
            ("src/main.rs:12", Some("src/main.rs:12")),
            ("Cargo.toml:3", Some("Cargo.toml:3")),
            ("github.com/zed-industries/zed", None),
            ("localhost:3000", None),
            ("withRetry", None),
            ("npm test", None),
            (
                "/Users/matt/work/app/src/lib.rs",
                Some("/Users/matt/work/app/src/lib.rs"),
            ),
        ] {
            assert_eq!(inline_code_link(code).as_deref(), link, "{code}");
        }
    }

    #[test]
    fn file_links_are_found_as_t3_finds_them() {
        assert_eq!(
            parse_file_link("file://localhost/home/me/notes.md"),
            Some(target("/home/me/notes.md", None, None))
        );
        assert_eq!(
            parse_file_link("</home/me/project/ChatMarkdown.tsx:1>"),
            Some(target("/home/me/project/ChatMarkdown.tsx", Some(1), None))
        );
        for href in [
            "https://example.com/docs",
            "/chat/settings#L3",
            "TODO:12",
            "patches",
            "#heading",
            "//cdn.example.com/clip.mp4",
            "mailto:me@example.com",
            "file://server/share/src/main.ts",
        ] {
            assert_eq!(parse_file_link(href), None, "{href}");
        }
        assert_eq!(
            parse_file_link("patches/"),
            Some(target("patches/", None, None))
        );
        assert_eq!(
            parse_file_link("docs/My%20Folder/"),
            Some(target("docs/My Folder/", None, None))
        );
        assert_eq!(
            parse_file_link("Makefile"),
            Some(target("Makefile", None, None))
        );
        assert_eq!(
            parse_file_link("Dockerfile:8"),
            Some(target("Dockerfile", Some(8), None))
        );
        assert_eq!(
            parse_file_link("/srv/app/Makefile"),
            Some(target("/srv/app/Makefile", None, None))
        );
        assert_eq!(
            parse_file_link("/Users/julius/project/src/main.ts#L42C7"),
            Some(target(
                "/Users/julius/project/src/main.ts",
                Some(42),
                Some(7)
            ))
        );
        assert_eq!(
            parse_file_link("src/webhooks/dispatcher.ts:40:9"),
            Some(target("src/webhooks/dispatcher.ts", Some(40), Some(9)))
        );
        assert_eq!(
            parse_file_link("script.ts:10"),
            Some(target("script.ts", Some(10), None))
        );
        // Encoded once, decoded once.
        assert_eq!(
            parse_file_link("file:///Users/julius/project/file%2520name.md"),
            Some(target("/Users/julius/project/file%20name.md", None, None))
        );
    }

    #[test]
    fn links_are_files_pages_quotes_or_chips() {
        assert_eq!(
            classify("https://github.com/pingdotgg/t3code/pull/12"),
            Link::External("https://github.com/pingdotgg/t3code/pull/12".into())
        );
        assert_eq!(
            classify("src/main.rs:3"),
            Link::File(target("src/main.rs", Some(3), None))
        );
        assert_eq!(
            classify(
                "t3-citation://v1/env-1/thread-1/message-i5?text=Done&start=0&end=4&prefix=&suffix="
            ),
            Link::Citation {
                message_id: "message-i5".into()
            }
        );
        assert_eq!(classify("t3-context://v1/thread/thread_abc"), Link::Context);
        assert_eq!(classify("#usage"), Link::None);
    }

    #[test]
    fn links_open_in_the_editor_the_review_or_say_theyre_missing() {
        let folder =
            std::env::temp_dir().join(format!("mustr-file-links-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(folder.join("src")).unwrap();
        std::fs::write(folder.join("src/main.rs"), "fn main() {}\n").unwrap();
        let local = Folder::Local(folder.clone());

        assert_eq!(
            file_open(&target("src/main.rs", Some(3), Some(5)), &local, None),
            FileOpen::Editor {
                folder: Some(folder.clone()),
                path: folder.join("src/main.rs"),
                position: Some(Point::new(2, 4)),
            }
        );
        assert_eq!(
            file_open(&target("./src/../src/main.rs", None, None), &local, None),
            FileOpen::Editor {
                folder: Some(folder.clone()),
                path: folder.join("src/main.rs"),
                position: None,
            }
        );
        let absolute = folder.join("src/main.rs").display().to_string();
        assert_eq!(
            file_open(&target(&absolute, Some(1), None), &Folder::Unknown, None),
            FileOpen::Editor {
                folder: None,
                path: folder.join("src/main.rs"),
                position: Some(Point::new(0, 0)),
            }
        );
        assert_eq!(
            file_open(&target("~/src/main.rs", None, None), &local, Some(&folder)),
            FileOpen::Editor {
                folder: Some(folder.clone()),
                path: folder.join("src/main.rs"),
                position: None,
            }
        );
        assert_eq!(
            file_open(&target("src/", None, None), &local, None),
            FileOpen::Folder(folder.join("src"))
        );
        assert_eq!(
            file_open(&target("src/gone.rs", Some(1), None), &local, None),
            FileOpen::Missing("src/gone.rs".into())
        );
        assert_eq!(
            file_open(
                &target("src/main.rs", Some(1), None),
                &Folder::Remote("/srv/checkout".into()),
                None
            ),
            FileOpen::Review
        );
        std::fs::remove_dir_all(folder).ok();
    }
}

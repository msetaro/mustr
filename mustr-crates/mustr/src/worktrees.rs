//! The worktrees page: the git worktrees T3 made for agent threads, by project, with the threads
//! that use each one and whether it's safe to remove. It follows T3's own rules
//! (`worktreeCleanup.ts`, `hooks/useThreadActions.ts`, `settings/StorageSettings.tsx`, and the
//! server's `storageCleanup.ts`).
//!
//! - The worktrees are the `worktreePath`s of T3's threads: the active ones ([`ThreadShells`])
//!   and the archived ones (`orchestration.getArchivedShellSnapshot`). Threads without a project
//!   work in folders of T3's scratch root (`server.getConfig`'s `scratchWorkspaceRoot`), which
//!   aren't worktrees, so they're left out, as T3's `isScratchProject` leaves them out.
//! - A worktree is shared when its threads aren't one thread and its subagents (T3's
//!   `worktreeOwner`). Shared worktrees, and those an agent is working in, aren't removed here.
//! - Removing one sends `vcs.removeWorktree` with `force`, after a confirm step, as T3's app does
//!   when it deletes a thread. That needs `source-control:write`.
//! - When T3 runs on this machine, the page checks that each worktree's folder exists and is a
//!   linked git worktree (its `.git` is a file), and runs `git status` in it (read-only), a few
//!   at a time. Only linked worktrees can be removed. So about 150 worktrees list at once, and
//!   their states fill in as they're checked. The list only lays out the rows on screen.
//! - Opening a worktree adds it to the window's project, so Zed's file finder, project search,
//!   and language servers cover it.
//!
//! - Above the list, T3's automatic cleanup rules, and Delete now (`worktree_cleanup.rs`).
//!
//! [`RemoveThreadWorktree`], which archiving a thread sends, offers to remove the thread's
//! worktree when no other active thread uses it, as T3's app does when it deletes a thread.

use crate::{
    t3_connection::{self, T3Connection},
    worktree_cleanup::WorktreeCleanup,
};
use anyhow::{Context as _, Result, ensure};
use futures::StreamExt as _;
use gpui::{
    AnyElement, App, AppContext as _, Context, Entity, EventEmitter, FocusHandle, Focusable,
    FontWeight, Global, ListAlignment, ListState, PromptLevel, SharedString, Subscription, Task,
    WeakEntity, Window, actions, list,
};
use mustr_ui::{Pill, SegmentedControl, TextButton, palette, tokens, tokens::scaled};
use std::{
    collections::{HashMap, HashSet},
    path::{Path, PathBuf},
    sync::Arc,
};
use t3_client::{
    RpcMethod as _,
    t3_api::{
        rpc,
        types::{
            OrchestrationGetArchivedShellSnapshotPayload, OrchestrationProjectShell,
            OrchestrationV2AppThreadLineageRelationshipToParent, OrchestrationV2ThreadShell,
            ServerGetConfigPayload, VcsRemoveWorktreeInput,
        },
    },
};
use thread_chat::{
    Access, RemoveThreadWorktree, ThreadBackend, ThreadBackendExt as _, ThreadShells,
};
use ui::{Color, Icon, IconName, IconSize, Label, LabelCommon as _, LabelSize, prelude::*};
use workspace::{
    Toast, Workspace,
    item::{Item, ItemEvent},
    notifications::NotificationId,
};

actions!(
    mustr,
    [
        /// Opens the worktrees page: the worktrees T3 made for agent threads, by project.
        ShowWorktrees,
    ]
);

/// How many `git status` checks run at once.
const STATUS_CHECKS_IN_FLIGHT: usize = 6;
/// How many of a worktree's threads its row names.
const THREADS_SHOWN: usize = 3;
const CONTENT_MAX_WIDTH: f32 = 920.;

pub fn init(cx: &mut App) {
    cx.set_global(LocalFolders(t3_runs_here));
    cx.observe_new(|workspace: &mut Workspace, _, _| {
        workspace.register_action(|workspace, _: &ShowWorktrees, window, cx| {
            WorktreesView::show(workspace, window, cx);
        });
        workspace.register_action(|workspace, action: &RemoveThreadWorktree, window, cx| {
            offer_thread_worktree_removal(action.thread_id.clone(), workspace, window, cx);
        });
    })
    .detach();
}

/// Whether T3's folders are on this machine, so mustr can read them, run terminals in them, and
/// open them in the editor. Tests replace it.
#[derive(Clone, Copy)]
pub(crate) struct LocalFolders(pub fn(&App) -> bool);

impl Global for LocalFolders {}

pub(crate) fn worktrees_are_local(cx: &App) -> bool {
    cx.try_global::<LocalFolders>()
        .is_none_or(|local| (local.0)(cx))
}

/// T3 runs on this machine, and `MUSTR_IGNORE_LOCAL_WORKTREES` (which agent reviews honor too)
/// doesn't ask mustr to act as if it didn't.
fn t3_runs_here(cx: &App) -> bool {
    let ignored =
        std::env::var_os("MUSTR_IGNORE_LOCAL_WORKTREES").is_some_and(|value| !value.is_empty());
    !ignored
        && T3Connection::global(cx)
            .read(cx)
            .server_url()
            .is_some_and(t3_connection::server_is_local)
}

/// A thread's worktree path as T3 compares them: trimmed, without a trailing separator.
pub(crate) fn normalize_worktree_path(path: Option<&str>) -> Option<&str> {
    let path = path?.trim();
    let path = match path.trim_end_matches(['/', '\\']) {
        "" => path,
        trimmed => trimmed,
    };
    (!path.is_empty()).then_some(path)
}

/// Whether removing `path` can't take the project's own checkout with it: it isn't the
/// checkout, or a folder that holds it.
pub(crate) fn is_removable_path(path: &str, project_root: &str) -> bool {
    let path = Path::new(path);
    let root = Path::new(project_root.trim());
    path.is_absolute()
        && path.parent().is_some()
        && !project_root.trim().is_empty()
        && !root.starts_with(path)
}

/// Whether `path` is `root` or inside it, compared by components.
fn is_within(path: &str, root: &str) -> bool {
    Path::new(path).starts_with(Path::new(root.trim()))
}

/// Whether a thread works in T3's scratch root instead of a project's worktree: its project is
/// the scratch project, or its folder is in the scratch root (`isScratchProject` in T3's
/// `packages/client-runtime/src/state/projects.ts`).
fn is_scratch(path: &str, project_root: Option<&str>, scratch_root: Option<&str>) -> bool {
    let Some(scratch_root) = normalize_worktree_path(scratch_root) else {
        return false;
    };
    project_root.and_then(|root| normalize_worktree_path(Some(root))) == Some(scratch_root)
        || is_within(path, scratch_root)
}

/// T3's scratch root, from `server.getConfig`, read leniently: only that field is needed, and
/// `None` when T3 doesn't say.
async fn scratch_root(backend: &Arc<dyn ThreadBackend>) -> Option<String> {
    let payload = serde_json::to_value(ServerGetConfigPayload(Default::default())).ok()?;
    let config = backend
        .call_raw(rpc::ServerGetConfig::TAG, payload, Access::Read)
        .await
        .map_err(|error| log::warn!("couldn't read T3's config: {error:#}"))
        .ok()?;
    let root = config.get("scratchWorkspaceRoot")?.as_str()?.trim();
    (!root.is_empty()).then(|| root.to_string())
}

/// Whether the folder is a linked git worktree: its `.git` is a file that points into the main
/// checkout's repository, whose own `.git` is a folder. T3's cleanup checks `.git` too.
fn is_linked_worktree(path: &Path) -> bool {
    path.join(".git").is_file()
}

/// `/home/me/x` as `~/x`.
pub(crate) fn display_path(path: &Path) -> String {
    match dirs::home_dir().and_then(|home| path.strip_prefix(home).ok().map(Path::to_path_buf)) {
        Some(relative) if relative.as_os_str().is_empty() => "~".to_string(),
        Some(relative) => format!("~/{}", relative.display()),
        None => path.display().to_string(),
    }
}

/// What `vcs.removeWorktree` gets, as T3's app sends it when it deletes a thread with its
/// worktree: the project's checkout as `cwd`, and `force`, since the user confirmed it.
pub(crate) fn remove_worktree_input(project_root: &str, path: &str) -> VcsRemoveWorktreeInput {
    VcsRemoveWorktreeInput {
        cwd: project_root.to_string(),
        path: path.to_string(),
        force: Some(true),
    }
}

/// Removes a worktree through T3. Refused before it's sent in dry runs, or when the pairing
/// lacks `source-control:write`.
fn remove_worktree(
    backend: &Arc<dyn ThreadBackend>,
    project_root: &str,
    path: &str,
) -> futures::future::BoxFuture<'static, Result<()>> {
    backend.call::<rpc::VcsRemoveWorktree>(
        remove_worktree_input(project_root, path),
        Access::SourceControlWrite,
    )
}

/// Whether the worktree has anything removing it would lose: uncommitted changes or untracked
/// files. Only reads, with `worktree_git.rs`'s hardening: no optional locks, so the index isn't
/// refreshed, and no hooks or fsmonitor.
async fn has_uncommitted_changes(path: &Path) -> Result<bool> {
    let mut command = util::command::new_command("git");
    command
        .current_dir(path)
        .args([
            "--no-optional-locks",
            "--no-pager",
            "-c",
            "core.fsmonitor=false",
            "-c",
            "core.hooksPath=/dev/null",
            "status",
            "--porcelain",
            "-z",
            "--untracked-files=normal",
        ])
        .env("GIT_OPTIONAL_LOCKS", "0")
        .env("GIT_TERMINAL_PROMPT", "0")
        .stdin(util::command::Stdio::null())
        .kill_on_drop(true);
    let output = command
        .output()
        .await
        .context("running git (is it installed?)")?;
    ensure!(
        output.status.success(),
        "git status failed: {}",
        String::from_utf8_lossy(&output.stderr).trim()
    );
    Ok(!output.stdout.is_empty())
}

// The worktrees, from T3's threads.

/// A thread that uses a worktree.
#[derive(Clone, Debug, PartialEq)]
pub(crate) struct WorktreeThread {
    pub id: String,
    pub title: String,
    pub archived: bool,
    pub settled: bool,
    /// T3's run status, such as `running` or `completed`.
    pub status: String,
    /// It waits on the user: an approval or a question.
    pub needs_input: bool,
    /// When T3 created it, which orders the list: unlike its last update, it doesn't change
    /// while an agent works, so rows don't move under the pointer.
    pub created_at: String,
    parent_id: Option<String>,
    root_id: String,
    subagent: bool,
}

impl WorktreeThread {
    fn from_shell(thread: &OrchestrationV2ThreadShell, archived: bool) -> Self {
        let status = serde_json::to_value(&thread.status)
            .ok()
            .and_then(|status| status.as_str().map(str::to_string))
            .unwrap_or_default();
        Self {
            id: thread.id.clone(),
            title: if thread.title.trim().is_empty() {
                "Untitled thread".to_string()
            } else {
                thread.title.clone()
            },
            archived: archived || thread.archived_at.is_some(),
            settled: thread.settled_at.is_some(),
            status,
            needs_input: thread.pending_runtime_request.is_some(),
            created_at: thread.created_at.clone(),
            parent_id: thread.lineage.parent_thread_id.clone(),
            root_id: thread.lineage.root_thread_id.clone(),
            subagent: thread.lineage.relationship_to_parent
                == Some(OrchestrationV2AppThreadLineageRelationshipToParent::Subagent),
        }
    }

    /// An agent works in the worktree, or waits on the user there.
    pub fn is_busy(&self) -> bool {
        !self.archived
            && !self.settled
            && (self.needs_input
                || matches!(
                    self.status.as_str(),
                    "preparing" | "queued" | "starting" | "running" | "waiting"
                ))
    }

    pub fn state_label(&self) -> String {
        if self.archived {
            "archived".to_string()
        } else if self.settled {
            "settled".to_string()
        } else if self.needs_input {
            "needs input".to_string()
        } else {
            self.status.replace('_', " ")
        }
    }
}

/// One worktree and the threads that use it.
#[derive(Clone, Debug, PartialEq)]
pub(crate) struct WorktreeEntry {
    pub path: String,
    pub branch: Option<String>,
    /// Newest first, by creation.
    pub threads: Vec<WorktreeThread>,
}

impl WorktreeEntry {
    /// The folder's name.
    pub fn name(&self) -> &str {
        Path::new(&self.path)
            .file_name()
            .and_then(|name| name.to_str())
            .unwrap_or(&self.path)
    }

    /// The thread that owns the worktree: its only thread, or the one the others are subagents
    /// of, as T3's `worktreeOwner` (`storageCleanup.ts`) decides.
    pub fn owner(&self) -> Option<&WorktreeThread> {
        if let [only] = self.threads.as_slice() {
            return Some(only);
        }
        let by_id: HashMap<&str, &WorktreeThread> = self
            .threads
            .iter()
            .map(|thread| (thread.id.as_str(), thread))
            .collect();
        let mut tops = self.threads.iter().filter(|thread| {
            thread
                .parent_id
                .as_deref()
                .is_none_or(|parent| !by_id.contains_key(parent))
        });
        let (Some(owner), None) = (tops.next(), tops.next()) else {
            return None;
        };
        for thread in &self.threads {
            let mut current = thread;
            let mut seen = HashSet::new();
            while current.id != owner.id {
                if !seen.insert(current.id.as_str())
                    || !current.subagent
                    || current.root_id != owner.root_id
                {
                    return None;
                }
                current = by_id.get(current.parent_id.as_deref()?)?;
            }
        }
        Some(owner)
    }

    /// Threads other than one thread and its subagents use it.
    pub fn is_shared(&self) -> bool {
        self.owner().is_none()
    }

    pub fn is_busy(&self) -> bool {
        self.threads.iter().any(WorktreeThread::is_busy)
    }

    /// Every thread in it is settled or archived, so its work is done.
    pub fn is_done(&self) -> bool {
        self.threads
            .iter()
            .all(|thread| thread.archived || thread.settled)
    }

    fn newest_thread(&self) -> &str {
        self.threads
            .iter()
            .map(|thread| thread.created_at.as_str())
            .max()
            .unwrap_or_default()
    }
}

/// A project's worktrees.
#[derive(Clone, Debug, PartialEq)]
pub(crate) struct ProjectWorktrees {
    pub id: String,
    pub title: String,
    /// The project's own checkout, `vcs.removeWorktree`'s `cwd`.
    pub workspace_root: Option<String>,
    /// Newest first, by their threads' creation.
    pub worktrees: Vec<WorktreeEntry>,
}

/// T3's threads' worktrees, by project. A path that is its project's own checkout, or a folder
/// of T3's scratch root, isn't a worktree, and isn't listed.
pub(crate) fn group_worktrees<'a>(
    active: impl IntoIterator<Item = &'a OrchestrationV2ThreadShell>,
    archived: impl IntoIterator<Item = &'a OrchestrationV2ThreadShell>,
    projects: impl IntoIterator<Item = &'a OrchestrationProjectShell>,
    scratch_root: Option<&str>,
) -> Vec<ProjectWorktrees> {
    let projects: HashMap<&str, &OrchestrationProjectShell> = projects
        .into_iter()
        .map(|project| (project.id.as_str(), project))
        .collect();
    // By path: the project of its newest thread, and the threads.
    let mut by_path: HashMap<String, (String, Vec<(&OrchestrationV2ThreadShell, bool)>)> =
        HashMap::new();
    let threads = active
        .into_iter()
        .map(|thread| (thread, false))
        .chain(archived.into_iter().map(|thread| (thread, true)));
    for (thread, archived) in threads {
        if thread.deleted_at.is_some() {
            continue;
        }
        let Some(path) = normalize_worktree_path(thread.worktree_path.as_deref()) else {
            continue;
        };
        let (_, members) = by_path
            .entry(path.to_string())
            .or_insert_with(|| (thread.project_id.clone(), Vec::new()));
        if !members.iter().any(|(member, _)| member.id == thread.id) {
            members.push((thread, archived));
        }
    }

    let mut grouped: HashMap<String, ProjectWorktrees> = HashMap::new();
    for (path, (_, mut members)) in by_path {
        members.sort_by(|(left, _), (right, _)| {
            right
                .created_at
                .cmp(&left.created_at)
                .then_with(|| left.id.cmp(&right.id))
        });
        let project_id = members[0].0.project_id.clone();
        let project = projects.get(project_id.as_str());
        let workspace_root = project.map(|project| project.workspace_root.clone());
        if workspace_root
            .as_deref()
            .and_then(|root| normalize_worktree_path(Some(root)))
            == Some(path.as_str())
            || is_scratch(&path, workspace_root.as_deref(), scratch_root)
        {
            continue;
        }
        let branch = members.iter().find_map(|(thread, _)| thread.branch.clone());
        let entry = WorktreeEntry {
            path,
            branch,
            threads: members
                .iter()
                .map(|(thread, archived)| WorktreeThread::from_shell(thread, *archived))
                .collect(),
        };
        grouped
            .entry(project_id.clone())
            .or_insert_with(|| ProjectWorktrees {
                id: project_id.clone(),
                title: project
                    .map(|project| project.title.clone())
                    .unwrap_or_else(|| "Unknown project".to_string()),
                workspace_root,
                worktrees: Vec::new(),
            })
            .worktrees
            .push(entry);
    }
    let mut grouped: Vec<ProjectWorktrees> = grouped.into_values().collect();
    for project in &mut grouped {
        project.worktrees.sort_by(|left, right| {
            right
                .newest_thread()
                .cmp(left.newest_thread())
                .then_with(|| left.path.cmp(&right.path))
        });
    }
    grouped.sort_by(|left, right| {
        left.title
            .to_lowercase()
            .cmp(&right.title.to_lowercase())
            .then_with(|| left.id.cmp(&right.id))
    });
    grouped
}

/// The worktree to offer removing once `thread_id` is archived: its own, when no other active
/// thread uses it (`getOrphanedWorktreePathForThread` in T3's `worktreeCleanup.ts`).
#[derive(Clone, Debug, PartialEq)]
pub(crate) struct OrphanedWorktree {
    pub path: String,
    pub project_root: String,
    pub thread_title: String,
}

pub(crate) fn orphaned_worktree(
    thread_id: &str,
    active: &[OrchestrationV2ThreadShell],
    archived: &[OrchestrationV2ThreadShell],
    projects: &[OrchestrationProjectShell],
    scratch_root: Option<&str>,
) -> Option<OrphanedWorktree> {
    let thread = active
        .iter()
        .chain(archived)
        .find(|thread| thread.id == thread_id)?;
    let path = normalize_worktree_path(thread.worktree_path.as_deref())?;
    let shared = active.iter().any(|other| {
        other.id != thread_id
            && other.archived_at.is_none()
            && other.deleted_at.is_none()
            && normalize_worktree_path(other.worktree_path.as_deref()) == Some(path)
    });
    if shared {
        return None;
    }
    let project = projects
        .iter()
        .find(|project| project.id == thread.project_id)?;
    if is_scratch(path, Some(&project.workspace_root), scratch_root) {
        return None;
    }
    is_removable_path(path, &project.workspace_root).then(|| OrphanedWorktree {
        path: path.to_string(),
        project_root: project.workspace_root.clone(),
        thread_title: thread.title.clone(),
    })
}

/// Offers to remove an archived thread's worktree, when no other active thread uses it and the
/// pairing may remove worktrees, as T3's app does when it deletes a thread
/// (`hooks/useThreadActions.ts`).
fn offer_thread_worktree_removal(
    thread_id: String,
    _: &mut Workspace,
    window: &mut Window,
    cx: &mut Context<Workspace>,
) {
    let Some(backend) = thread_chat::backend(cx) else {
        return;
    };
    if let Some(reason) = backend.blocked_reason(Access::SourceControlWrite) {
        log::info!("not offering to remove thread {thread_id}'s worktree: {reason}");
        return;
    }
    let local = worktrees_are_local(cx);
    cx.spawn_in(window, async move |workspace, cx| {
        // A fresh shell: the thread was just archived, and other threads may have moved.
        let snapshot = backend.shell_snapshot().await?;
        let mut archived = snapshot.archived_threads;
        let mut projects = snapshot.projects;
        let known = snapshot
            .threads
            .iter()
            .chain(&archived)
            .any(|thread| thread.id == thread_id);
        if !known {
            let archived_shell = backend
                .call::<rpc::OrchestrationGetArchivedShellSnapshot>(
                    OrchestrationGetArchivedShellSnapshotPayload(Default::default()),
                    Access::Read,
                )
                .await?;
            archived.extend(archived_shell.threads);
            projects.extend(archived_shell.projects);
        }
        let scratch_root = scratch_root(&backend).await;
        let Some(orphan) = orphaned_worktree(
            &thread_id,
            &snapshot.threads,
            &archived,
            &projects,
            scratch_root.as_deref(),
        ) else {
            return Ok(());
        };
        let path = PathBuf::from(&orphan.path);
        let dirty = if local {
            if !path.is_dir() || !is_linked_worktree(&path) {
                return Ok(());
            }
            has_uncommitted_changes(&path).await.ok()
        } else {
            None
        };
        let detail = format!(
            "“{}” was the only thread using {}. Removing it deletes the folder{}.",
            orphan.thread_title,
            display_path(&path),
            match dirty {
                Some(true) => ", and the uncommitted changes in it",
                Some(false) => "; it has no uncommitted changes",
                None => "",
            }
        );
        let answer = workspace.update_in(cx, |_, window, cx| {
            window.prompt(
                PromptLevel::Warning,
                "Remove the thread's worktree too?",
                Some(&detail),
                &["Remove worktree", "Keep it"],
                cx,
            )
        })?;
        if answer.await.ok() != Some(0) {
            return Ok(());
        }
        let result = remove_worktree(&backend, &orphan.project_root, &orphan.path).await;
        workspace.update(cx, |workspace, cx| {
            if result.is_ok() {
                let pages: Vec<_> = workspace
                    .panes()
                    .iter()
                    .flat_map(|pane| pane.read(cx).items_of_type::<WorktreesView>())
                    .collect();
                for page in pages {
                    page.update(cx, |page, cx| page.removed(&orphan.path, cx));
                }
            }
            let id =
                NotificationId::composite::<WorktreesView>(SharedString::from(orphan.path.clone()));
            let message = match &result {
                Ok(()) => format!("Removed the worktree {}", display_path(&path)),
                Err(error) => format!("Couldn't remove {}: {error:#}", display_path(&path)),
            };
            workspace.show_toast(Toast::new(id, message), cx);
        })?;
        result
    })
    .detach_and_log_err(cx);
}

// The page.

/// What's known of a worktree's folder.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
struct FolderState {
    /// `None` until it's checked, and on a T3 on another machine.
    exists: Option<bool>,
    /// Whether it's a linked git worktree, once checked.
    linked_worktree: Option<bool>,
    /// Uncommitted changes or untracked files, once `git status` has read them.
    dirty: Option<bool>,
    /// `git status` has finished, or won't run.
    status_checked: bool,
}

impl FolderState {
    fn gone() -> Self {
        Self {
            exists: Some(false),
            linked_worktree: None,
            dirty: None,
            status_checked: true,
        }
    }
}

/// A row's removal, while it's being confirmed or sent.
#[derive(Clone, Debug, PartialEq)]
enum Removal {
    Confirming,
    Removing,
    Failed(SharedString),
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
enum Filter {
    #[default]
    All,
    Done,
    InUse,
}

impl Filter {
    const ALL: [Self; 3] = [Self::All, Self::Done, Self::InUse];

    fn includes(self, worktree: &WorktreeEntry) -> bool {
        match self {
            Self::All => true,
            Self::Done => worktree.is_done(),
            Self::InUse => !worktree.is_done(),
        }
    }

    fn label(self) -> &'static str {
        match self {
            Self::All => "All",
            Self::Done => "Done",
            Self::InUse => "In use",
        }
    }

    fn tooltip(self) -> &'static str {
        match self {
            Self::All => "Every worktree",
            Self::Done => "Worktrees whose threads are all settled or archived",
            Self::InUse => "Worktrees with a thread that isn't settled or archived",
        }
    }
}

/// One row of the list.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum ListEntry {
    Project(usize),
    /// `last` closes the project's card.
    Worktree {
        project: usize,
        worktree: usize,
        last: bool,
    },
}

enum Archived {
    Loading,
    Loaded {
        threads: Vec<OrchestrationV2ThreadShell>,
        projects: Vec<OrchestrationProjectShell>,
    },
    Failed(SharedString),
}

pub struct WorktreesView {
    focus_handle: FocusHandle,
    workspace: WeakEntity<Workspace>,
    shells: Entity<ThreadShells>,
    archived: Archived,
    /// T3's scratch root, whose folders aren't worktrees.
    scratch_root: Option<String>,
    projects: Vec<ProjectWorktrees>,
    entries: Vec<ListEntry>,
    list_state: ListState,
    filter: Filter,
    /// Whether worktrees whose folders are gone are listed.
    show_missing: bool,
    folders: HashMap<String, FolderState>,
    removals: HashMap<String, Removal>,
    /// The active threads that had worktrees, to notice when one is archived.
    active_with_worktrees: HashSet<String>,
    /// T3's automatic cleanup rules, above the list.
    cleanup: Entity<WorktreeCleanup>,
    _load_archived: Task<()>,
    _load_scratch_root: Task<()>,
    _check_folders: Vec<Task<()>>,
    _subscriptions: Vec<Subscription>,
}

impl WorktreesView {
    /// Activates the worktrees page, or opens it.
    pub fn show(workspace: &mut Workspace, window: &mut Window, cx: &mut Context<Workspace>) {
        let existing = workspace
            .panes()
            .iter()
            .find_map(|pane| pane.read(cx).items_of_type::<Self>().next());
        if let Some(existing) = existing {
            workspace.activate_item(&existing, true, true, window, cx);
            return;
        }
        let workspace_handle = workspace.weak_handle();
        let view = cx.new(|cx| Self::new(workspace_handle, window, cx));
        workspace.add_item_to_active_pane(Box::new(view), None, true, window, cx);
    }

    fn new(workspace: WeakEntity<Workspace>, window: &mut Window, cx: &mut Context<Self>) -> Self {
        let shells = ThreadShells::global(cx);
        let subscription = cx.observe_in(&shells, window, |this, _, _, cx| this.shell_changed(cx));
        let page = cx.weak_entity();
        let cleanup = cx.new(|cx| {
            WorktreeCleanup::new(cx).on_ran(move |_, cx| {
                page.update(cx, |page, cx| page.refresh(cx)).ok();
            })
        });
        let mut this = Self {
            cleanup,
            focus_handle: cx.focus_handle(),
            workspace,
            shells,
            archived: Archived::Loading,
            scratch_root: None,
            projects: Vec::new(),
            entries: Vec::new(),
            list_state: ListState::new(0, ListAlignment::Top, px(800.)),
            filter: Filter::default(),
            show_missing: false,
            folders: HashMap::new(),
            removals: HashMap::new(),
            active_with_worktrees: HashSet::new(),
            _load_archived: Task::ready(()),
            _load_scratch_root: Task::ready(()),
            _check_folders: Vec::new(),
            _subscriptions: vec![subscription],
        };
        this.load_archived(cx);
        this.load_scratch_root(cx);
        this.regroup(cx);
        this
    }

    fn load_scratch_root(&mut self, cx: &mut Context<Self>) {
        let Some(backend) = thread_chat::backend(cx) else {
            return;
        };
        self._load_scratch_root = cx.spawn(async move |this, cx| {
            let root = scratch_root(&backend).await;
            this.update(cx, |this, cx| {
                this.scratch_root = root;
                this.regroup(cx);
            })
            .ok();
        });
    }

    /// Reloads the archived threads, and checks every folder again.
    fn refresh(&mut self, cx: &mut Context<Self>) {
        self.folders.clear();
        self._check_folders.clear();
        self.removals
            .retain(|_, removal| *removal == Removal::Removing);
        self.load_archived(cx);
        self.load_scratch_root(cx);
        self.regroup(cx);
    }

    fn load_archived(&mut self, cx: &mut Context<Self>) {
        let Some(backend) = thread_chat::backend(cx) else {
            self.archived = Archived::Failed("mustr isn't set up to read T3's threads".into());
            return;
        };
        let load = backend.call::<rpc::OrchestrationGetArchivedShellSnapshot>(
            OrchestrationGetArchivedShellSnapshotPayload(Default::default()),
            Access::Read,
        );
        self._load_archived = cx.spawn(async move |this, cx| {
            let result = load.await;
            this.update(cx, |this, cx| {
                this.archived = match result {
                    Ok(snapshot) => Archived::Loaded {
                        threads: snapshot.threads,
                        projects: snapshot.projects,
                    },
                    Err(error) => {
                        log::warn!("couldn't load T3's archived threads: {error:#}");
                        Archived::Failed(format!("{error:#}").into())
                    }
                };
                this.regroup(cx);
            })
            .ok();
        });
    }

    fn shell_changed(&mut self, cx: &mut Context<Self>) {
        // A thread that leaves the active shell with a worktree was most likely archived.
        let active_with_worktrees: HashSet<String> = self
            .shells
            .read(cx)
            .shell()
            .map(|shell| {
                shell
                    .threads
                    .values()
                    .filter(|thread| {
                        normalize_worktree_path(thread.worktree_path.as_deref()).is_some()
                    })
                    .map(|thread| thread.id.clone())
                    .collect()
            })
            .unwrap_or_default();
        let left = self
            .active_with_worktrees
            .iter()
            .any(|id| !active_with_worktrees.contains(id));
        self.active_with_worktrees = active_with_worktrees;
        if left && matches!(self.archived, Archived::Loaded { .. }) {
            self.load_archived(cx);
        }
        self.regroup(cx);
    }

    fn regroup(&mut self, cx: &mut Context<Self>) {
        let shells = self.shells.read(cx);
        let Some(shell) = shells.shell() else {
            cx.notify();
            return;
        };
        let (archived_threads, archived_projects): (&[_], &[_]) = match &self.archived {
            Archived::Loaded { threads, projects } => (threads, projects),
            _ => (&[], &[]),
        };
        let projects = shell.projects.values().chain(
            archived_projects
                .iter()
                .filter(|project| !shell.projects.contains_key(&project.id)),
        );
        self.projects = group_worktrees(
            shell.threads.values(),
            archived_threads,
            projects,
            self.scratch_root.as_deref(),
        );
        self.check_new_folders(cx);
        self.rebuild_entries();
        cx.notify();
    }

    fn folder(&self, path: &str) -> FolderState {
        self.folders.get(path).copied().unwrap_or_default()
    }

    fn is_listed(&self, worktree: &WorktreeEntry) -> bool {
        self.filter.includes(worktree)
            && (self.show_missing || self.folder(&worktree.path).exists != Some(false))
    }

    fn rebuild_entries(&mut self) {
        let mut entries = Vec::new();
        for (project_index, project) in self.projects.iter().enumerate() {
            let listed: Vec<usize> = project
                .worktrees
                .iter()
                .enumerate()
                .filter(|(_, worktree)| self.is_listed(worktree))
                .map(|(index, _)| index)
                .collect();
            if listed.is_empty() {
                continue;
            }
            entries.push(ListEntry::Project(project_index));
            let count = listed.len();
            entries.extend(listed.into_iter().enumerate().map(|(position, worktree)| {
                ListEntry::Worktree {
                    project: project_index,
                    worktree,
                    last: position + 1 == count,
                }
            }));
        }
        if entries != self.entries {
            self.entries = entries;
            self.list_state.reset(self.entries.len());
        } else {
            self.list_state.remeasure();
        }
    }

    /// Checks the folders of worktrees not checked yet: whether they exist (all at once, which is
    /// quick), then `git status` in the ones that do, a few at a time.
    fn check_new_folders(&mut self, cx: &mut Context<Self>) {
        if !worktrees_are_local(cx) {
            return;
        }
        let unchecked: Vec<String> = self
            .projects
            .iter()
            .flat_map(|project| &project.worktrees)
            .map(|worktree| worktree.path.clone())
            .filter(|path| !self.folders.contains_key(path))
            .collect();
        if unchecked.is_empty() {
            return;
        }
        for path in &unchecked {
            self.folders.insert(path.clone(), FolderState::default());
        }
        let check = cx.spawn(async move |this, cx| {
            let checked = cx
                .background_spawn(async move {
                    unchecked
                        .into_iter()
                        .map(|path| {
                            let folder = Path::new(&path);
                            let exists = folder.is_dir();
                            let linked = exists && is_linked_worktree(folder);
                            (path, exists, linked)
                        })
                        .collect::<Vec<_>>()
                })
                .await;
            let Ok(worktrees) = this.update(cx, |this, cx| {
                for (path, exists, linked) in &checked {
                    let folder = this.folders.entry(path.clone()).or_default();
                    folder.exists = Some(*exists);
                    folder.linked_worktree = Some(*linked);
                    folder.status_checked = !linked;
                }
                this.rebuild_entries();
                cx.notify();
                checked
                    .into_iter()
                    .filter_map(|(path, _, linked)| linked.then_some(path))
                    .collect::<Vec<_>>()
            }) else {
                return;
            };
            let mut statuses = futures::stream::iter(worktrees)
                .map(|path| async move {
                    let dirty = has_uncommitted_changes(Path::new(&path)).await;
                    (path, dirty)
                })
                .buffer_unordered(STATUS_CHECKS_IN_FLIGHT);
            while let Some((path, dirty)) = statuses.next().await {
                let dirty = match dirty {
                    Ok(dirty) => Some(dirty),
                    Err(error) => {
                        log::warn!("couldn't read the status of {path}: {error:#}");
                        None
                    }
                };
                let updated = this.update(cx, |this, cx| {
                    let folder = this.folders.entry(path).or_default();
                    folder.dirty = dirty;
                    folder.status_checked = true;
                    cx.notify();
                });
                if updated.is_err() {
                    return;
                }
            }
        });
        self._check_folders.push(check);
    }

    fn set_filter(&mut self, filter: Filter, cx: &mut Context<Self>) {
        self.filter = filter;
        self.rebuild_entries();
        cx.notify();
    }

    fn toggle_missing(&mut self, cx: &mut Context<Self>) {
        self.show_missing = !self.show_missing;
        self.rebuild_entries();
        cx.notify();
    }

    fn find(&self, path: &str) -> Option<(&ProjectWorktrees, &WorktreeEntry)> {
        self.projects.iter().find_map(|project| {
            project
                .worktrees
                .iter()
                .find(|worktree| worktree.path == path)
                .map(|worktree| (project, worktree))
        })
    }

    /// Why the worktree can't be removed here, if it can't.
    fn removal_blocker(
        &self,
        project: &ProjectWorktrees,
        worktree: &WorktreeEntry,
        cx: &App,
    ) -> Option<String> {
        let Some(root) = project.workspace_root.as_deref() else {
            return Some("T3 doesn't say where the project's checkout is".to_string());
        };
        if !is_removable_path(&worktree.path, root) {
            return Some("It holds the project's own checkout".to_string());
        }
        if worktree.is_shared() {
            return Some(format!(
                "Shared by {} threads that aren't one thread and its subagents",
                worktree.threads.len()
            ));
        }
        if worktree.is_busy() {
            return Some("An agent is working in it".to_string());
        }
        let folder = self.folder(&worktree.path);
        if folder.exists == Some(false) {
            return Some("The folder is already gone".to_string());
        }
        if folder.linked_worktree == Some(false) {
            return Some("It isn't a git worktree".to_string());
        }
        match thread_chat::backend(cx) {
            Some(backend) => backend.blocked_reason(Access::SourceControlWrite),
            None => Some("mustr isn't connected to T3".to_string()),
        }
    }

    fn set_removal(&mut self, path: &str, removal: Option<Removal>, cx: &mut Context<Self>) {
        match removal {
            Some(removal) => self.removals.insert(path.to_string(), removal),
            None => self.removals.remove(path),
        };
        self.list_state.remeasure();
        cx.notify();
    }

    fn confirm_removal(&mut self, path: &str, cx: &mut Context<Self>) {
        self.set_removal(path, Some(Removal::Confirming), cx);
    }

    fn remove(&mut self, path: &str, cx: &mut Context<Self>) {
        let Some((project, worktree)) = self.find(path) else {
            return;
        };
        if let Some(blocker) = self.removal_blocker(project, worktree, cx) {
            self.set_removal(path, Some(Removal::Failed(blocker.into())), cx);
            return;
        }
        let (Some(root), Some(backend)) =
            (project.workspace_root.clone(), thread_chat::backend(cx))
        else {
            return;
        };
        let path = path.to_string();
        let remove = remove_worktree(&backend, &root, &path);
        self.set_removal(&path, Some(Removal::Removing), cx);
        cx.spawn(async move |this, cx| {
            let result = remove.await;
            this.update(cx, |this, cx| match result {
                Ok(()) => {
                    this.removed(&path, cx);
                    let message =
                        format!("Removed the worktree {}", display_path(Path::new(&path)));
                    this.workspace
                        .update(cx, |workspace, cx| {
                            let id = NotificationId::composite::<Self>(SharedString::from(path));
                            workspace.show_toast(Toast::new(id, message), cx);
                        })
                        .ok();
                }
                Err(error) => this.set_removal(
                    &path,
                    Some(Removal::Failed(format!("{error:#}").into())),
                    cx,
                ),
            })
            .ok();
        })
        .detach();
    }

    /// Notes that T3 removed the worktree, which hides it unless gone folders are listed.
    fn removed(&mut self, path: &str, cx: &mut Context<Self>) {
        self.folders.insert(path.to_string(), FolderState::gone());
        self.removals.remove(path);
        self.rebuild_entries();
        cx.notify();
    }

    /// Adds the worktree to the window's project, and shows Zed's file finder over it.
    fn open_in_editor(&mut self, path: &str, window: &mut Window, cx: &mut Context<Self>) {
        let path = PathBuf::from(path);
        self.workspace
            .update(cx, |workspace, cx| {
                let project = workspace.project().clone();
                // Visible, so Zed's Restricted Mode applies to it, as for reviews (`review.rs`).
                let add = project.update(cx, |project, cx| {
                    project.find_or_create_worktree(&path, true, cx)
                });
                cx.spawn_in(window, async move |workspace, cx| {
                    add.await
                        .with_context(|| format!("opening {}", path.display()))?;
                    workspace.update_in(cx, |_, window, cx| {
                        window
                            .dispatch_action(Box::new(workspace::ToggleFileFinder::default()), cx);
                    })
                })
                .detach_and_log_err(cx);
            })
            .ok();
    }

    fn open_thread(&self, thread: &WorktreeThread, window: &mut Window, cx: &mut App) {
        thread_chat::open_thread(
            thread.id.clone(),
            thread.title.clone().into(),
            self.workspace.clone(),
            window,
            cx,
        );
    }

    // Rendering.

    fn counts(&self) -> (usize, usize, usize) {
        let visible = |worktree: &&WorktreeEntry| {
            self.show_missing || self.folder(&worktree.path).exists != Some(false)
        };
        let worktrees = || {
            self.projects
                .iter()
                .flat_map(|project| &project.worktrees)
                .filter(visible)
        };
        let all = worktrees().count();
        let done = worktrees().filter(|worktree| worktree.is_done()).count();
        (all, done, all - done)
    }

    fn render_header(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let palette = palette(cx);
        let (all, done, in_use) = self.counts();
        let local = worktrees_are_local(cx);
        let missing = self
            .projects
            .iter()
            .flat_map(|project| &project.worktrees)
            .filter(|worktree| self.folder(&worktree.path).exists == Some(false))
            .count();
        let dirty = self
            .projects
            .iter()
            .flat_map(|project| &project.worktrees)
            .filter(|worktree| self.folder(&worktree.path).dirty == Some(true))
            .count();
        let checking = self.folders.values().any(|folder| !folder.status_checked);
        let filters = Filter::ALL.iter().fold(
            SegmentedControl::new("worktrees-filter"),
            |control, filter| {
                let count = match filter {
                    Filter::All => all,
                    Filter::Done => done,
                    Filter::InUse => in_use,
                };
                control
                    .segment_with_tooltip(format!("{} {count}", filter.label()), filter.tooltip())
            },
        );
        let selected = Filter::ALL
            .iter()
            .position(|filter| *filter == self.filter)
            .unwrap_or(0);
        let this = cx.entity().downgrade();
        let filters = filters.selected(selected).on_select(move |index, _, cx| {
            this.update(cx, |this, cx| this.set_filter(Filter::ALL[index], cx))
                .ok();
        });
        let mut summary = Vec::new();
        if dirty > 0 {
            summary.push(format!("{dirty} with uncommitted changes"));
        }
        if checking {
            summary.push("checking folders…".to_string());
        }
        let shells = self.shells.read(cx);
        let shell_error = shells
            .shell()
            .is_none()
            .then(|| shells.error().cloned())
            .flatten();
        v_flex()
            .w_full()
            .max_w(scaled(px(CONTENT_MAX_WIDTH)))
            .mx_auto()
            .px_6()
            .pt_6()
            .pb_3()
            .gap_3()
            .child(
                v_flex()
                    .gap_0p5()
                    .child(Headline::new("Worktrees").size(HeadlineSize::Large))
                    .child(
                        Label::new(
                            "The worktrees T3 made for agent threads, by project. Remove the ones \
                             whose work is done.",
                        )
                        .color(Color::Muted),
                    ),
            )
            .child(
                // In a narrow window, the summary and Refresh go to the next line.
                h_flex()
                    .flex_wrap()
                    .gap_3()
                    .child(filters)
                    .child(
                        Label::new(summary.join(" · "))
                            .size(LabelSize::Small)
                            .color(Color::Muted),
                    )
                    .child(
                        div().ml_auto().child(
                            TextButton::new("refresh-worktrees", "Refresh")
                                .tooltip("Read T3's threads and check every folder again")
                                .on_click(cx.listener(|this, _, _, cx| this.refresh(cx))),
                        ),
                    ),
            )
            .child(self.cleanup.clone())
            .when(!local, |this| {
                this.child(notice(
                    "T3 runs on another machine, so mustr can't check these folders or open \
                     them here. Removing still works, through T3.",
                    palette.text_muted.hsla(),
                ))
            })
            .when_some(shell_error, |this, error| {
                this.child(notice(
                    format!("Couldn't read T3's threads: {error}"),
                    cx.theme().status().error,
                ))
            })
            .when_some(
                match &self.archived {
                    Archived::Failed(error) => Some(error.clone()),
                    _ => None,
                },
                |this, error| {
                    this.child(notice(
                        format!("Couldn't read T3's archived threads: {error}"),
                        cx.theme().status().error,
                    ))
                },
            )
            .when(missing > 0, |this| {
                let label = if self.show_missing {
                    format!(
                        "Showing {missing} worktree{} whose folders are gone.",
                        plural(missing)
                    )
                } else {
                    format!(
                        "{missing} more worktree{} whose folders are gone.",
                        plural(missing)
                    )
                };
                this.child(
                    h_flex()
                        .gap_2()
                        .child(Label::new(label).size(LabelSize::Small).color(Color::Muted))
                        .child(
                            div()
                                .id("toggle-missing-worktrees")
                                .cursor_pointer()
                                .text_size(tokens::text_size(tokens::TEXT_SIZE_SMALL))
                                .text_color(palette.accent.hsla())
                                .child(if self.show_missing {
                                    "Hide them"
                                } else {
                                    "Show them"
                                })
                                .on_click(cx.listener(|this, _, _, cx| this.toggle_missing(cx))),
                        ),
                )
            })
    }

    fn render_entry(&mut self, index: usize, cx: &mut Context<Self>) -> AnyElement {
        let Some(entry) = self.entries.get(index).copied() else {
            return div().into_any_element();
        };
        let content = match entry {
            ListEntry::Project(project) => self.render_project(project, cx).into_any_element(),
            ListEntry::Worktree {
                project,
                worktree,
                last,
            } => self.render_worktree(project, worktree, last, cx),
        };
        div()
            .w_full()
            .child(
                div()
                    .w_full()
                    .max_w(scaled(px(CONTENT_MAX_WIDTH)))
                    .mx_auto()
                    .px_6()
                    .child(content),
            )
            .into_any_element()
    }

    fn render_project(&self, index: usize, cx: &App) -> impl IntoElement {
        let palette = palette(cx);
        let project = &self.projects[index];
        let listed = project
            .worktrees
            .iter()
            .filter(|worktree| self.is_listed(worktree))
            .count();
        h_flex()
            .mt_3()
            .w_full()
            .gap_2()
            .px_3()
            .py_2()
            .rounded_t(scaled(tokens::CORNER_RADIUS))
            .border(tokens::BORDER_WIDTH)
            .border_color(palette.border.hsla())
            .bg(palette.surface.hsla())
            .child(
                Icon::new(IconName::Folder)
                    .size(IconSize::Small)
                    .color(Color::Muted),
            )
            .child(Label::new(project.title.clone()).weight(FontWeight::SEMIBOLD))
            .child(
                Label::new(listed.to_string())
                    .size(LabelSize::Small)
                    .color(Color::Muted),
            )
            .when_some(project.workspace_root.as_deref(), |this, root| {
                this.child(
                    Label::new(display_path(Path::new(root)))
                        .size(LabelSize::Small)
                        .color(Color::Muted)
                        .truncate(),
                )
            })
    }

    fn render_worktree(
        &self,
        project_index: usize,
        worktree_index: usize,
        last: bool,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let palette = palette(cx);
        let project = &self.projects[project_index];
        let worktree = &project.worktrees[worktree_index];
        let folder = self.folder(&worktree.path);
        let local = worktrees_are_local(cx);
        let removal = self.removals.get(&worktree.path).cloned();
        let blocker = self.removal_blocker(project, worktree, cx);
        let path = worktree.path.clone();

        let mut pills: Vec<AnyElement> = Vec::new();
        if folder.exists == Some(false) {
            pills.push(Pill::tinted("folder gone", palette.text_muted).into_any_element());
        }
        if folder.exists == Some(true) && folder.linked_worktree == Some(false) {
            pills.push(Pill::tinted("not a git worktree", palette.text_muted).into_any_element());
        }
        if folder.dirty == Some(true) {
            pills.push(Pill::tinted("uncommitted changes", palette.warning).into_any_element());
        }
        if worktree.is_shared() {
            pills.push(
                Pill::tinted(
                    format!("shared by {}", worktree.threads.len()),
                    palette.pending,
                )
                .into_any_element(),
            );
        }
        if worktree.is_busy() {
            pills.push(Pill::tinted("working", palette.accent).into_any_element());
        } else if worktree.is_done() {
            let label = if worktree.threads.iter().all(|thread| thread.archived) {
                "archived"
            } else {
                "done"
            };
            pills.push(Pill::tinted(label, palette.ok).into_any_element());
        }

        let details = [
            worktree.branch.clone(),
            Some(display_path(Path::new(&worktree.path))),
        ]
        .into_iter()
        .flatten()
        .collect::<Vec<_>>()
        .join(" · ");

        let threads = h_flex()
            .gap_3()
            .min_w_0()
            .overflow_hidden()
            .children(worktree.threads.iter().take(THREADS_SHOWN).enumerate().map(
                |(index, thread)| {
                    let color = if thread.is_busy() {
                        palette.accent.hsla()
                    } else {
                        palette.text_muted.hsla()
                    };
                    let thread_for_click = thread.clone();
                    h_flex()
                        .id(SharedString::from(format!(
                            "worktree-thread-{}-{index}",
                            worktree.path
                        )))
                        .flex_shrink(1.)
                        .min_w_0()
                        .gap_1()
                        .cursor_pointer()
                        .text_size(tokens::text_size(tokens::TEXT_SIZE_SMALL))
                        .child(
                            div()
                                .flex_none()
                                .size(scaled(px(6.)))
                                .rounded_full()
                                .bg(color),
                        )
                        .child(
                            div()
                                .min_w_0()
                                .truncate()
                                .text_color(palette.text.hsla())
                                .hover(|style| style.text_color(palette.accent.hsla()))
                                .child(thread.title.clone()),
                        )
                        .child(
                            div()
                                .flex_none()
                                .text_color(palette.text_muted.hsla())
                                .child(thread.state_label()),
                        )
                        .on_click(cx.listener(move |this, _, window, cx| {
                            this.open_thread(&thread_for_click, window, cx)
                        }))
                },
            ))
            .when(worktree.threads.len() > THREADS_SHOWN, |this| {
                this.child(
                    Label::new(format!("+{} more", worktree.threads.len() - THREADS_SHOWN))
                        .size(LabelSize::Small)
                        .color(Color::Muted),
                )
            });

        let actions = match &removal {
            Some(Removal::Confirming) => None,
            Some(Removal::Removing) => Some(
                Label::new("Removing…")
                    .size(LabelSize::Small)
                    .color(Color::Muted)
                    .into_any_element(),
            ),
            _ => {
                let can_open = local && folder.exists != Some(false);
                let open_path = path.clone();
                let remove_path = path.clone();
                Some(
                    h_flex()
                        .flex_none()
                        .gap_2()
                        .child(
                            TextButton::new(
                                SharedString::from(format!("open-worktree-{path}")),
                                "Open",
                            )
                            .disabled(!can_open)
                            .tooltip(if can_open {
                                "Add it to this window's project, for the file finder and search"
                            } else {
                                "The folder isn't on this machine"
                            })
                            .on_click(cx.listener(
                                move |this, _, window, cx| {
                                    if can_open {
                                        this.open_in_editor(&open_path, window, cx)
                                    }
                                },
                            )),
                        )
                        .child(
                            TextButton::new(
                                SharedString::from(format!("remove-worktree-{path}")),
                                "Remove…",
                            )
                            .disabled(blocker.is_some())
                            .tooltip(
                                blocker
                                    .unwrap_or_else(|| "Delete the worktree's folder".to_string()),
                            )
                            .on_click(cx.listener(
                                move |this, _, _, cx| {
                                    if this.find(&remove_path).is_some_and(|(project, worktree)| {
                                        this.removal_blocker(project, worktree, cx).is_none()
                                    }) {
                                        this.confirm_removal(&remove_path, cx)
                                    }
                                },
                            )),
                        )
                        .into_any_element(),
                )
            }
        };

        let confirm = matches!(removal, Some(Removal::Confirming)).then(|| {
            let warning = match folder.dirty {
                Some(true) => "It has uncommitted changes, which will be lost.",
                Some(false) => "It has no uncommitted changes.",
                None => "Anything not pushed will be lost.",
            };
            let confirm_path = path.clone();
            let cancel_path = path.clone();
            h_flex()
                .w_full()
                .flex_wrap()
                .gap_2()
                .mt_2()
                .px_2()
                .py_1p5()
                .rounded(scaled(tokens::CONTROL_RADIUS))
                .bg(palette.warning.hsla().opacity(0.1))
                .child(
                    Label::new(format!(
                        "Remove {} and delete its folder? {warning}",
                        worktree.name()
                    ))
                    .size(LabelSize::Small),
                )
                .child(
                    h_flex()
                        .ml_auto()
                        .gap_2()
                        .child(
                            TextButton::new(
                                SharedString::from(format!("cancel-remove-{path}")),
                                "Cancel",
                            )
                            .on_click(cx.listener(
                                move |this, _, _, cx| this.set_removal(&cancel_path, None, cx),
                            )),
                        )
                        .child(
                            TextButton::new(
                                SharedString::from(format!("confirm-remove-{path}")),
                                "Remove worktree",
                            )
                            .style(mustr_ui::ButtonStyle::Primary)
                            .on_click(
                                cx.listener(move |this, _, _, cx| this.remove(&confirm_path, cx)),
                            ),
                        ),
                )
        });

        let failure = match &removal {
            Some(Removal::Failed(error)) => Some(error.clone()),
            _ => None,
        };

        v_flex()
            .w_full()
            .px_3()
            .py_2()
            .border_x(tokens::BORDER_WIDTH)
            .border_b(tokens::BORDER_WIDTH)
            .border_color(palette.border.hsla())
            .bg(palette.surface.hsla())
            .when(last, |this| this.rounded_b(scaled(tokens::CORNER_RADIUS)))
            .when(folder.exists == Some(false), |this| this.opacity(0.6))
            .child(
                // Open and Remove go under the worktree when there's no room beside it.
                h_flex()
                    .w_full()
                    .flex_wrap()
                    .gap_3()
                    .items_start()
                    .child(
                        div().pt(scaled(px(2.))).child(
                            Icon::new(IconName::GitWorktree)
                                .size(IconSize::Small)
                                .color(Color::Muted),
                        ),
                    )
                    .child(
                        v_flex()
                            .flex_1()
                            .min_w(scaled(px(120.)))
                            .gap_0p5()
                            .child(
                                // The pills go under the name when there's no room for both.
                                h_flex()
                                    .flex_wrap()
                                    .gap_2()
                                    .min_w_0()
                                    .child(
                                        Label::new(worktree.name().to_string())
                                            .weight(FontWeight::MEDIUM)
                                            .truncate(),
                                    )
                                    .children(pills),
                            )
                            .child(
                                Label::new(details)
                                    .size(LabelSize::Small)
                                    .color(Color::Muted)
                                    .truncate(),
                            )
                            .child(threads),
                    )
                    .children(actions.map(|actions| div().ml_auto().child(actions))),
            )
            .children(confirm)
            .when_some(failure, |this, error| {
                this.child(
                    div()
                        .pt_1()
                        .child(Label::new(error).size(LabelSize::Small).color(Color::Error)),
                )
            })
            .into_any_element()
    }
}

fn plural(count: usize) -> &'static str {
    if count == 1 { "" } else { "s" }
}

fn notice(text: impl Into<SharedString>, color: gpui::Hsla) -> impl IntoElement {
    div()
        .text_size(tokens::text_size(tokens::TEXT_SIZE_SMALL))
        .text_color(color)
        .child(text.into())
}

impl EventEmitter<ItemEvent> for WorktreesView {}

impl Focusable for WorktreesView {
    fn focus_handle(&self, _: &App) -> FocusHandle {
        self.focus_handle.clone()
    }
}

impl Item for WorktreesView {
    type Event = ItemEvent;

    fn tab_content_text(&self, _detail: usize, _cx: &App) -> SharedString {
        "Worktrees".into()
    }

    fn show_toolbar(&self) -> bool {
        false
    }

    fn to_item_events(event: &Self::Event, f: &mut dyn FnMut(ItemEvent)) {
        f(*event)
    }
}

impl Render for WorktreesView {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let loading = self.shells.read(cx).shell().is_none();
        let empty = !loading && self.entries.is_empty();
        let empty_text = if self.projects.is_empty() {
            "T3 hasn't made worktrees for any active or archived thread."
        } else {
            "No worktrees match."
        };
        v_flex()
            .key_context("MustrWorktrees")
            .track_focus(&self.focus_handle)
            .size_full()
            .bg(cx.theme().colors().editor_background)
            .child(self.render_header(cx))
            .when(loading, |this| {
                this.child(
                    div()
                        .w_full()
                        .max_w(scaled(px(CONTENT_MAX_WIDTH)))
                        .mx_auto()
                        .px_6()
                        .child(
                            Label::new("Loading T3's threads…")
                                .size(LabelSize::Small)
                                .color(Color::Muted),
                        ),
                )
            })
            .when(empty, |this| {
                this.child(
                    div()
                        .w_full()
                        .max_w(scaled(px(CONTENT_MAX_WIDTH)))
                        .mx_auto()
                        .px_6()
                        .child(
                            Label::new(empty_text)
                                .size(LabelSize::Small)
                                .color(Color::Muted),
                        ),
                )
            })
            .child(
                list(
                    self.list_state.clone(),
                    cx.processor(|this, index: usize, _window, cx| this.render_entry(index, cx)),
                )
                .flex_1()
                .size_full()
                .pb_6(),
            )
    }
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use anyhow::anyhow;
    use futures::{FutureExt as _, future::BoxFuture, stream::BoxStream};
    use gpui::{TestAppContext, VisualTestContext};
    use parking_lot::Mutex;
    use pretty_assertions::assert_eq;
    use serde_json::{Value, json};
    use settings::SettingsStore;
    use t3_client::t3_api::{
        tagged::{
            OrchestrationV2Command, OrchestrationV2ShellStreamItem, OrchestrationV2ThreadStreamItem,
        },
        types::{
            OrchestrationV2DispatchCommandResult, OrchestrationV2GetTurnItemInput,
            OrchestrationV2GetTurnItemResult, OrchestrationV2ShellSnapshot,
            OrchestrationV2SubscribeShellInput, OrchestrationV2SubscribeThreadInput,
            OrchestrationV2ThreadDetailSnapshot, OrchestrationV2ThreadLaunchInput,
            OrchestrationV2ThreadLaunchResult, ServerProvider,
        },
    };
    use thread_chat::{AttachmentUpload, ReadOnlyBackend};
    use workspace::MultiWorkspace;

    const NOW: &str = "2026-10-28T12:00:00.000Z";

    /// A thread of T3's shell, with every field the contracts require.
    pub(crate) fn thread(id: &str, worktree: Option<&str>) -> Value {
        json!({
            "id": id, "projectId": "project-1", "title": format!("Thread {id}"),
            "status": "completed", "createdAt": NOW, "updatedAt": NOW, "createdBy": "user",
            "creationSource": "web", "interactionMode": "default",
            "runtimeMode": "full-access",
            "modelSelection": { "instanceId": "claudeAgent", "model": "claude-sonnet-4-5" },
            "providerInstanceId": "claudeAgent", "activeProviderThreadId": null,
            "activeRunId": null, "archivedAt": null, "branch": null, "deletedAt": null,
            "forkedFrom": null, "hasActionableProposedPlan": false, "itemCount": 3,
            "latestRunId": null, "latestUserMessageAt": null, "latestVisibleMessage": null,
            "lineage": { "rootThreadId": id, "parentThreadId": null, "relationshipToParent": null },
            "pendingRuntimeRequest": null, "settledAt": null, "settledOverride": null,
            "visibleItemCount": 3, "worktreePath": worktree
        })
    }

    pub(crate) fn project(id: &str, title: &str, root: &str) -> Value {
        json!({
            "id": id, "title": title, "workspaceRoot": root, "scripts": [],
            "createdAt": NOW, "updatedAt": NOW, "defaultModelSelection": null
        })
    }

    fn decode<T: serde::de::DeserializeOwned>(value: Value) -> T {
        serde_json::from_value(value).expect("T3's JSON decodes")
    }

    fn with(mut value: Value, fields: Value) -> Value {
        util::merge_json_value_into(fields, &mut value);
        value
    }

    /// One call of `call_raw`.
    #[derive(Clone, Debug, PartialEq)]
    pub(crate) struct RawCall {
        pub tag: String,
        pub payload: Value,
        pub access: Access,
    }

    /// Plays T3: a shell, archived threads, and a record of every raw call. Writes succeed.
    #[derive(Default)]
    pub(crate) struct FakeBackend {
        pub shell: Mutex<Value>,
        pub archived: Mutex<Value>,
        pub raw_calls: Mutex<Vec<RawCall>>,
    }

    impl FakeBackend {
        pub(crate) fn new(threads: Vec<Value>, archived: Vec<Value>) -> Arc<Self> {
            Arc::new(Self {
                shell: Mutex::new(json!({
                    "schemaVersion": 1, "snapshotSequence": 7, "threads": threads,
                    "archivedThreads": [],
                    "projects": [
                        project("project-1", "api", "/src/api"),
                        project("scratch", "Scratch", "/scratch"),
                    ]
                })),
                archived: Mutex::new(json!({
                    "schemaVersion": 1, "snapshotSequence": 7, "threads": archived,
                    "projects": [project("project-1", "api", "/src/api")]
                })),
                raw_calls: Mutex::default(),
            })
        }

        /// The calls that aren't reads.
        pub(crate) fn writes(&self) -> Vec<RawCall> {
            self.raw_calls
                .lock()
                .iter()
                .filter(|call| call.access.is_write())
                .cloned()
                .collect()
        }
    }

    fn ready<T: Send + 'static>(value: Result<T>) -> BoxFuture<'static, Result<T>> {
        futures::future::ready(value).boxed()
    }

    impl ThreadBackend for FakeBackend {
        fn shell_snapshot(&self) -> BoxFuture<'static, Result<OrchestrationV2ShellSnapshot>> {
            ready(serde_json::from_value(self.shell.lock().clone()).map_err(Into::into))
        }

        fn subscribe_shell(
            &self,
            _: OrchestrationV2SubscribeShellInput,
        ) -> Result<BoxStream<'static, Result<OrchestrationV2ShellStreamItem>>> {
            Ok(futures::stream::pending().boxed())
        }

        fn thread_snapshot(
            &self,
            _: String,
        ) -> BoxFuture<'static, Result<OrchestrationV2ThreadDetailSnapshot>> {
            ready(Err(anyhow!("no thread")))
        }

        fn subscribe_thread(
            &self,
            _: OrchestrationV2SubscribeThreadInput,
        ) -> Result<BoxStream<'static, Result<OrchestrationV2ThreadStreamItem>>> {
            Ok(futures::stream::pending().boxed())
        }

        fn turn_item(
            &self,
            _: OrchestrationV2GetTurnItemInput,
        ) -> BoxFuture<'static, Result<OrchestrationV2GetTurnItemResult>> {
            ready(Err(anyhow!("no item")))
        }

        fn providers(&self) -> BoxFuture<'static, Result<Vec<ServerProvider>>> {
            ready(Ok(Vec::new()))
        }

        fn dispatch(
            &self,
            _: OrchestrationV2Command,
        ) -> BoxFuture<'static, Result<OrchestrationV2DispatchCommandResult>> {
            ready(Err(anyhow!("not in these tests")))
        }

        fn launch_thread(
            &self,
            _: OrchestrationV2ThreadLaunchInput,
        ) -> BoxFuture<'static, Result<OrchestrationV2ThreadLaunchResult>> {
            ready(Err(anyhow!("not in these tests")))
        }

        fn call_raw(
            &self,
            tag: &str,
            payload: Value,
            access: Access,
        ) -> BoxFuture<'static, Result<Value>> {
            self.raw_calls.lock().push(RawCall {
                tag: tag.to_string(),
                payload,
                access,
            });
            if tag == rpc::OrchestrationGetArchivedShellSnapshot::TAG {
                return ready(Ok(self.archived.lock().clone()));
            }
            if tag == rpc::ServerGetConfig::TAG {
                return ready(Ok(json!({ "scratchWorkspaceRoot": "/scratch" })));
            }
            ready(Ok(Value::Null))
        }

        fn subscribe_raw(
            &self,
            _: &str,
            _: Value,
            _: Access,
        ) -> Result<BoxStream<'static, Result<Value>>> {
            Err(anyhow!("not in these tests"))
        }

        fn upload(&self, _: AttachmentUpload) -> BoxFuture<'static, Result<()>> {
            ready(Err(anyhow!("not in these tests")))
        }
    }

    pub(crate) async fn workspace_with(
        backend: Arc<dyn ThreadBackend>,
        local: bool,
        cx: &mut TestAppContext,
    ) -> (Entity<Workspace>, &mut VisualTestContext) {
        cx.update(|cx| {
            let settings_store = SettingsStore::test(cx);
            cx.set_global(settings_store);
            theme_settings::init(theme::LoadThemes::JustBase, cx);
            thread_chat::set_backend(backend, cx);
            init(cx);
            crate::terminal_panel::init(cx);
            cx.set_global(LocalFolders(if local { |_| true } else { |_| false }));
        });
        let fs = project::FakeFs::new(cx.executor());
        let project = project::Project::test(fs, [], cx).await;
        let (multi_workspace, cx) =
            cx.add_window_view(|window, cx| MultiWorkspace::test_new(project.clone(), window, cx));
        let workspace =
            multi_workspace.read_with(cx, |multi_workspace, _| multi_workspace.workspace().clone());
        (workspace, cx)
    }

    fn shells(threads: &[Value]) -> Vec<OrchestrationV2ThreadShell> {
        threads.iter().cloned().map(decode).collect()
    }

    #[test]
    fn worktrees_group_by_project_and_skip_the_checkout() {
        let active = shells(&[
            with(
                thread("a", Some("/wt/api/a/")),
                json!({ "createdAt": "2026-10-28T10:00:00.000Z", "branch": "t3/a" }),
            ),
            thread("b", Some("/wt/api/b")),
            // Root threads have no worktree, and one on the checkout isn't one.
            thread("root", None),
            thread("checkout", Some("/src/api")),
            with(
                thread("web", Some("/wt/web/x")),
                json!({ "projectId": "project-2", "status": "running" }),
            ),
            // T3's scratch folders, for threads without a project, aren't worktrees.
            with(
                thread("scratch", Some("/scratch/2026-10-28-notes")),
                json!({ "projectId": "scratch" }),
            ),
            thread("scratch-2", Some("/scratch/2026-10-27-ideas")),
        ]);
        let archived = shells(&[with(
            thread("old", Some("/wt/api/a")),
            json!({ "archivedAt": NOW, "createdAt": "2026-10-01T00:00:00.000Z" }),
        )]);
        let projects: Vec<OrchestrationProjectShell> = [
            project("project-1", "api", "/src/api"),
            project("project-2", "Web", "/src/web"),
            project("scratch", "Scratch", "/scratch"),
        ]
        .into_iter()
        .map(decode)
        .collect();
        let grouped = group_worktrees(&active, &archived, &projects, Some("/scratch/"));
        let summary: Vec<(&str, Vec<(&str, Vec<&str>)>)> = grouped
            .iter()
            .map(|project| {
                (
                    project.title.as_str(),
                    project
                        .worktrees
                        .iter()
                        .map(|worktree| {
                            (
                                worktree.path.as_str(),
                                worktree
                                    .threads
                                    .iter()
                                    .map(|thread| thread.id.as_str())
                                    .collect(),
                            )
                        })
                        .collect(),
                )
            })
            .collect();
        assert_eq!(
            summary,
            [
                (
                    "api",
                    vec![("/wt/api/b", vec!["b"]), ("/wt/api/a", vec!["a", "old"])]
                ),
                ("Web", vec![("/wt/web/x", vec!["web"])]),
            ]
        );
        let shared = &grouped[0].worktrees[1];
        assert_eq!(shared.branch.as_deref(), Some("t3/a"));
        assert!(shared.is_shared(), "two unrelated threads share it");
        assert!(!shared.is_done(), "a isn't settled");
        assert!(shared.threads[1].archived);
        assert!(grouped[1].worktrees[0].is_busy());
    }

    #[test]
    fn subagents_dont_share_their_parents_worktree() {
        let subagent = |id: &str, parent: &str| {
            with(
                thread(id, Some("/wt/a")),
                json!({ "lineage": {
                    "rootThreadId": "parent", "parentThreadId": parent,
                    "relationshipToParent": "subagent"
                }}),
            )
        };
        let entry = |threads: &[Value]| WorktreeEntry {
            path: "/wt/a".into(),
            branch: None,
            threads: shells(threads)
                .iter()
                .map(|thread| WorktreeThread::from_shell(thread, false))
                .collect(),
        };
        let family = entry(&[
            thread("parent", Some("/wt/a")),
            subagent("child", "parent"),
            subagent("grandchild", "child"),
        ]);
        assert_eq!(
            family.owner().map(|owner| owner.id.as_str()),
            Some("parent")
        );
        assert!(!family.is_shared());

        // A fork isn't a subagent, and neither is a thread of another family.
        let fork = with(
            thread("fork", Some("/wt/a")),
            json!({ "lineage": {
                "rootThreadId": "parent", "parentThreadId": "parent",
                "relationshipToParent": "fork"
            }}),
        );
        assert!(entry(&[thread("parent", Some("/wt/a")), fork]).is_shared());
        assert!(
            entry(&[
                thread("parent", Some("/wt/a")),
                thread("other", Some("/wt/a"))
            ])
            .is_shared()
        );
        // A subagent whose parent isn't in the worktree is a top of its own.
        assert!(entry(&[subagent("child", "elsewhere")]).owner().is_some());
    }

    #[test]
    fn done_means_every_thread_is_settled_or_archived() {
        let entry = |threads: &[Value]| WorktreeEntry {
            path: "/wt/a".into(),
            branch: None,
            threads: shells(threads)
                .iter()
                .map(|thread| WorktreeThread::from_shell(thread, false))
                .collect(),
        };
        let settled = with(thread("a", Some("/wt/a")), json!({ "settledAt": NOW }));
        let archived = with(thread("b", Some("/wt/a")), json!({ "archivedAt": NOW }));
        assert!(entry(&[settled.clone(), archived]).is_done());
        assert!(!entry(&[settled, thread("c", Some("/wt/a"))]).is_done());
        let asking = with(
            thread("d", Some("/wt/a")),
            json!({ "pendingRuntimeRequest": { "id": "r1", "kind": "command", "createdAt": NOW } }),
        );
        let asking: OrchestrationV2ThreadShell = decode(asking);
        let asking = WorktreeThread::from_shell(&asking, false);
        assert!(asking.is_busy());
        assert_eq!(asking.state_label(), "needs input");
    }

    #[test]
    fn removal_never_takes_the_projects_checkout() {
        assert!(is_removable_path(
            "/home/me/.t3/worktrees/api/a",
            "/src/api"
        ));
        assert!(!is_removable_path("/src/api", "/src/api"));
        assert!(!is_removable_path("/src", "/src/api"));
        assert!(!is_removable_path("/", "/src/api"));
        assert!(!is_removable_path("relative/path", "/src/api"));
        assert!(!is_removable_path("/wt/a", " "));
        assert_eq!(normalize_worktree_path(Some(" /wt/a/ ")), Some("/wt/a"));
        assert_eq!(normalize_worktree_path(Some("/")), Some("/"));
        assert_eq!(normalize_worktree_path(Some("  ")), None);
    }

    #[test]
    fn an_archived_threads_worktree_is_orphaned_when_no_active_thread_uses_it() {
        let projects: Vec<OrchestrationProjectShell> =
            vec![decode(project("project-1", "api", "/src/api"))];
        let active = shells(&[
            thread("a", Some("/wt/a")),
            thread("b", Some("/wt/b")),
            thread("b2", Some("/wt/b/")),
        ]);
        let archived = shells(&[with(
            thread("gone", Some("/wt/c")),
            json!({ "archivedAt": NOW }),
        )]);
        assert_eq!(
            orphaned_worktree("a", &active, &archived, &projects, None),
            Some(OrphanedWorktree {
                path: "/wt/a".into(),
                project_root: "/src/api".into(),
                thread_title: "Thread a".into(),
            })
        );
        assert_eq!(
            orphaned_worktree("b", &active, &archived, &projects, None),
            None
        );
        assert_eq!(
            orphaned_worktree("gone", &active, &archived, &projects, None)
                .map(|orphan| orphan.path),
            Some("/wt/c".into())
        );
        assert_eq!(
            orphaned_worktree("missing", &active, &archived, &projects, None),
            None
        );
        // A scratch folder isn't a worktree.
        let scratch = shells(&[thread("notes", Some("/scratch/2026-10-28-notes"))]);
        assert_eq!(
            orphaned_worktree("notes", &scratch, &[], &projects, Some("/scratch")),
            None
        );
    }

    #[gpui::test]
    async fn archiving_offers_to_remove_an_unshared_worktree(cx: &mut TestAppContext) {
        let backend = FakeBackend::new(
            vec![thread("b", Some("/wt/b"))],
            vec![with(
                thread("a", Some("/wt/a")),
                json!({ "archivedAt": NOW }),
            )],
        );
        let (_workspace, cx) = workspace_with(backend.clone(), false, cx).await;
        cx.dispatch_action(RemoveThreadWorktree {
            thread_id: "a".into(),
        });
        cx.run_until_parked();
        let (message, detail) = cx.pending_prompt().expect("mustr asks first");
        assert_eq!(message, "Remove the thread's worktree too?");
        assert_eq!(
            detail,
            "“Thread a” was the only thread using /wt/a. Removing it deletes the folder."
        );
        cx.simulate_prompt_answer("Remove worktree");
        cx.run_until_parked();
        assert_eq!(
            backend.writes(),
            [RawCall {
                tag: "vcs.removeWorktree".into(),
                payload: json!({ "cwd": "/src/api", "path": "/wt/a", "force": true }),
                access: Access::SourceControlWrite,
            }]
        );
    }

    #[gpui::test]
    async fn scratch_folders_arent_offered(cx: &mut TestAppContext) {
        let backend = FakeBackend::new(
            vec![with(
                thread("notes", Some("/scratch/2026-10-28-notes")),
                json!({ "projectId": "scratch" }),
            )],
            Vec::new(),
        );
        let (_workspace, cx) = workspace_with(backend.clone(), false, cx).await;
        cx.dispatch_action(RemoveThreadWorktree {
            thread_id: "notes".into(),
        });
        cx.run_until_parked();
        assert!(!cx.has_pending_prompt());
        assert_eq!(backend.writes(), []);
    }

    #[gpui::test]
    async fn keeping_the_worktree_sends_nothing(cx: &mut TestAppContext) {
        let backend = FakeBackend::new(vec![thread("a", Some("/wt/a"))], Vec::new());
        let (_workspace, cx) = workspace_with(backend.clone(), false, cx).await;
        cx.dispatch_action(RemoveThreadWorktree {
            thread_id: "a".into(),
        });
        cx.run_until_parked();
        cx.simulate_prompt_answer("Keep it");
        cx.run_until_parked();
        assert_eq!(backend.writes(), []);
    }

    #[gpui::test]
    async fn shared_worktrees_and_dry_runs_arent_offered(cx: &mut TestAppContext) {
        let backend = FakeBackend::new(
            vec![thread("a", Some("/wt/a")), thread("b", Some("/wt/a"))],
            Vec::new(),
        );
        let (_workspace, cx) = workspace_with(backend.clone(), false, cx).await;
        cx.dispatch_action(RemoveThreadWorktree {
            thread_id: "a".into(),
        });
        cx.run_until_parked();
        assert!(!cx.has_pending_prompt());

        // A dry run can't remove it, so it doesn't ask.
        let backend = FakeBackend::new(vec![thread("a", Some("/wt/a"))], Vec::new());
        cx.update(|_, cx| {
            thread_chat::set_backend(
                Arc::new(ReadOnlyBackend::new(backend.clone(), "a dry run")),
                cx,
            )
        });
        cx.dispatch_action(RemoveThreadWorktree {
            thread_id: "a".into(),
        });
        cx.run_until_parked();
        assert!(!cx.has_pending_prompt());
        assert_eq!(backend.raw_calls.lock().len(), 0);
    }

    #[gpui::test]
    async fn the_page_lists_worktrees_and_removes_one_after_confirming(cx: &mut TestAppContext) {
        let backend = FakeBackend::new(
            vec![
                with(
                    thread("a", Some("/wt/api/a")),
                    json!({ "settledAt": NOW, "branch": "t3/a" }),
                ),
                with(
                    thread("b", Some("/wt/api/b")),
                    json!({ "status": "running" }),
                ),
                thread("c1", Some("/wt/api/c")),
                thread("c2", Some("/wt/api/c")),
            ],
            vec![with(
                thread("d", Some("/wt/api/d")),
                json!({ "archivedAt": NOW }),
            )],
        );
        let (workspace, cx) = workspace_with(backend.clone(), false, cx).await;
        cx.dispatch_action(ShowWorktrees);
        cx.run_until_parked();
        let view = workspace
            .read_with(cx, |workspace, cx| {
                workspace.active_item_as::<WorktreesView>(cx)
            })
            .expect("the page opened");
        assert_eq!(
            backend.raw_calls.lock()[0],
            RawCall {
                tag: "orchestration.getArchivedShellSnapshot".into(),
                payload: json!({}),
                access: Access::Read,
            }
        );
        view.update(cx, |view, cx| {
            let paths: Vec<&str> = view.projects[0]
                .worktrees
                .iter()
                .map(|worktree| worktree.path.as_str())
                .collect();
            assert_eq!(paths.len(), 4);
            assert_eq!(view.entries.len(), 5, "a header and four rows");
            let blocker = |path: &str| {
                let (project, worktree) = view.find(path).unwrap();
                view.removal_blocker(project, worktree, cx)
            };
            assert_eq!(blocker("/wt/api/a"), None);
            assert_eq!(blocker("/wt/api/d"), None);
            assert_eq!(
                blocker("/wt/api/b").as_deref(),
                Some("An agent is working in it")
            );
            assert!(blocker("/wt/api/c").unwrap().starts_with("Shared by 2"));

            view.set_filter(Filter::Done, cx);
            assert_eq!(view.entries.len(), 3, "a header, a and d");
            view.set_filter(Filter::All, cx);

            // Shared worktrees can't even be confirmed.
            view.remove("/wt/api/c", cx);
            view.confirm_removal("/wt/api/a", cx);
            view.remove("/wt/api/a", cx);
        });
        cx.run_until_parked();
        assert_eq!(
            backend.writes(),
            [RawCall {
                tag: "vcs.removeWorktree".into(),
                payload: json!({ "cwd": "/src/api", "path": "/wt/api/a", "force": true }),
                access: Access::SourceControlWrite,
            }]
        );
        view.read_with(cx, |view, _| {
            assert_eq!(view.folder("/wt/api/a").exists, Some(false));
            assert_eq!(view.removals.get("/wt/api/a"), None);
            assert!(matches!(
                view.removals.get("/wt/api/c"),
                Some(Removal::Failed(_))
            ));
            assert_eq!(view.entries.len(), 4, "a is gone from the list");
        });
    }

    #[gpui::test]
    async fn local_folders_are_checked_with_git(cx: &mut TestAppContext) {
        cx.executor().allow_parking();
        let root = tempfile::tempdir().unwrap();
        let repo = root.path().join("repo");
        let worktree = root.path().join("worktree");
        async fn run(args: &[&str], cwd: &Path) {
            let output = util::command::new_command("git")
                .args(args)
                .current_dir(cwd)
                .env("GIT_CONFIG_GLOBAL", "/dev/null")
                .env("GIT_CONFIG_NOSYSTEM", "1")
                .output()
                .await
                .unwrap();
            assert!(output.status.success(), "git {args:?}: {output:?}");
        }
        std::fs::create_dir(&repo).unwrap();
        run(&["init", "-q", "-b", "main"], &repo).await;
        std::fs::write(repo.join("a.txt"), "a\n").unwrap();
        run(&["add", "a.txt"], &repo).await;
        run(
            &[
                "-c",
                "user.name=t",
                "-c",
                "user.email=t@example.com",
                "commit",
                "-q",
                "-m",
                "a",
            ],
            &repo,
        )
        .await;
        run(
            &[
                "worktree",
                "add",
                "-q",
                "-b",
                "t3/x",
                worktree.to_str().unwrap(),
            ],
            &repo,
        )
        .await;
        // Only linked worktrees can be removed: not the main checkout, nor a plain folder.
        assert!(is_linked_worktree(&worktree));
        assert!(!is_linked_worktree(&repo));
        assert!(!is_linked_worktree(root.path()));
        assert!(!has_uncommitted_changes(&worktree).await.unwrap());
        std::fs::write(worktree.join("new.txt"), "new\n").unwrap();
        assert!(has_uncommitted_changes(&worktree).await.unwrap());
        assert!(has_uncommitted_changes(root.path()).await.is_err());
    }
}

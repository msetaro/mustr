//! A thread's checkout in git, through T3: its status (`subscribeVcsStatus`), its branches
//! (`vcs.listRefs`, `vcs.switchRef`, `vcs.createRef`), committing, pushing, and opening a pull
//! request in one go (`git.runStackedAction`, a stream of progress), pulling (`vcs.pull`), and
//! opening the checkout in an editor (`shell.openInEditor`, with the editors from
//! `server.getConfig`).
//!
//! The checkout is the thread's worktree, or its project's checkout when it has none, as in T3's
//! `GitActionsControl`. Which action the main button runs, and what the menu offers, follow T3's
//! `GitActionsControl.logic.ts`. T3 writes the commit message and the pull request's text unless
//! the user gives a message.
//!
//! Every call goes through the installed [`ThreadBackend`] with the scope T3 checks for it, so a
//! dry run refuses the writes and tests assert the exact payloads.
//!
//! [`ThreadGit`] is one thread's state, shared by every view of that thread: the thread
//! toolbar (`thread_toolbar.rs`) and the review of the thread's work (`agent_review.rs`).
//! [`ThreadGitControls`] draws its Open in ▾ and Commit ▾ buttons; [`GitProgressRow`] the steps of
//! a running action; and [`BranchPickerDelegate`] and [`GitActionDialog`] are their modals.

use anyhow::{Context as _, Result, anyhow};
use editor::Editor;
use futures::{FutureExt as _, StreamExt as _, future::BoxFuture};
use gpui::{
    AnyElement, App, AppContext as _, AsyncApp, ClickEvent, Context, DismissEvent, Entity,
    EventEmitter, FocusHandle, Focusable, Global, IntoElement, ParentElement as _, Pixels, Point,
    Render, SharedString, Styled as _, Subscription, Task, WeakEntity, Window, anchored, deferred,
    div, px, rems,
};
use mustr_ui::{
    ButtonStyle, Chip, OverflowItem, OverflowRow, SplitButton, TextButton, overflow, palette,
    tokens, tokens::scaled,
};
use picker::{Picker, PickerDelegate};
use serde_json::json;
use std::{collections::HashMap, sync::Arc, time::Duration};
use t3_client::t3_api::{
    RpcMethod as _, rpc,
    tagged::{
        GitActionProgressEvent, GitRunStackedActionResultToastCta, OrchestrationV2Command,
        VcsStatusStreamEvent,
    },
    types::{
        EditorId, GitActionProgressPhase, GitRunStackedActionInput, GitRunStackedActionResult,
        GitRunStackedActionResultBranchStatus, GitRunStackedActionResultPrStatus, GitStackedAction,
        LaunchEditorInput, OrchestrationV2CommandThreadMetadataUpdate,
        OrchestrationV2CommandThreadMetadataUpdateType, VcsCreateRefInput,
        VcsCreateWorktreePayload, VcsListRefsInput, VcsPullInput, VcsRef, VcsRefreshStatusPayload,
        VcsRemoveWorktreeInput, VcsStatusLocalResult, VcsStatusRemoteResult,
        VcsStatusRemoteResultPrState, VcsStatusSubscriptionInput, VcsSwitchRefInput,
    },
};
use thread_chat::{Access, Shell, ThreadBackend, ThreadBackendExt as _, ThreadShells};
use ui::{
    Color, ContextMenu, ContextMenuEntry, Icon, IconName, IconSize, Label, LabelCommon as _,
    LabelSize, ListItem, ListItemSpacing, Tooltip, prelude::*,
};
use workspace::{ModalView, Workspace};

/// How many branches the picker lists, as T3's branch picker asks for.
const REFS_LIMIT: i64 = 100;

/// How long a finished action's result stays next to the buttons, as T3 shows its toast.
const RESULT_VISIBLE: Duration = Duration::from_secs(10);

// ---------------------------------------------------------------------------------------------
// The checkout and its status

/// Where a thread works: its worktree, or its project's checkout.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Checkout {
    pub thread_id: String,
    pub project_id: String,
    pub cwd: String,
    /// The branch T3 recorded for the thread.
    pub branch: Option<String>,
    /// The thread's own worktree, when it has one; `None` when it works in the project's
    /// checkout.
    pub worktree_path: Option<String>,
    /// The project's checkout (`workspaceRoot`), while T3 lists the project.
    pub project_root: Option<String>,
}

impl Checkout {
    /// The thread's checkout, from T3's shell: `worktreePath`, or else the project's
    /// `workspaceRoot` (as T3's `gitCwd`).
    pub fn of_thread(shell: &Shell, thread_id: &str) -> Option<Self> {
        let thread = shell.threads.get(thread_id)?;
        let worktree_path = thread
            .worktree_path
            .clone()
            .filter(|path| !path.trim().is_empty());
        let project_root = shell
            .projects
            .get(&thread.project_id)
            .map(|project| project.workspace_root.clone());
        let cwd = worktree_path.clone().or_else(|| project_root.clone())?;
        Some(Self {
            thread_id: thread.id.clone(),
            project_id: thread.project_id.clone(),
            cwd,
            branch: thread.branch.clone().filter(|branch| !branch.is_empty()),
            worktree_path,
            project_root,
        })
    }
}

/// The checkout's git status, as `subscribeVcsStatus` sends it: what git sees locally, and what
/// T3 knows of the remote (`None` until it has looked).
#[derive(Clone, Debug, Default, PartialEq)]
pub struct GitStatus {
    pub local: Option<VcsStatusLocalResult>,
    pub remote: Option<VcsStatusRemoteResult>,
}

impl GitStatus {
    /// Applies one event of `subscribeVcsStatus`. Returns whether anything changed.
    pub fn apply(&mut self, event: VcsStatusStreamEvent) -> bool {
        match event {
            VcsStatusStreamEvent::Snapshot(snapshot) => {
                self.local = Some(snapshot.local);
                self.remote = snapshot.remote;
            }
            VcsStatusStreamEvent::LocalUpdated(update) => self.local = Some(update.local),
            VcsStatusStreamEvent::RemoteUpdated(update) => self.remote = update.remote,
            VcsStatusStreamEvent::Unrecognized(_) => return false,
        }
        true
    }

    pub fn is_repo(&self) -> bool {
        self.local.as_ref().is_some_and(|local| local.is_repo)
    }

    /// The checked-out branch, or `None` with a detached head.
    pub fn branch(&self) -> Option<&str> {
        self.local.as_ref()?.ref_name.as_deref()
    }

    pub fn is_dirty(&self) -> bool {
        self.local
            .as_ref()
            .is_some_and(|local| local.has_working_tree_changes)
    }

    /// How many files have uncommitted changes.
    pub fn changed_files(&self) -> usize {
        self.local
            .as_ref()
            .map_or(0, |local| local.working_tree.files.len())
    }

    pub fn is_default_ref(&self) -> bool {
        self.local
            .as_ref()
            .is_some_and(|local| local.is_default_ref)
    }

    pub fn has_primary_remote(&self) -> bool {
        self.local
            .as_ref()
            .is_some_and(|local| local.has_primary_remote)
    }

    pub fn has_upstream(&self) -> bool {
        self.remote
            .as_ref()
            .is_some_and(|remote| remote.has_upstream)
    }

    pub fn ahead(&self) -> i64 {
        self.remote.as_ref().map_or(0, |remote| remote.ahead_count)
    }

    pub fn behind(&self) -> i64 {
        self.remote.as_ref().map_or(0, |remote| remote.behind_count)
    }

    /// Commits ahead of the default branch, which a pull request would carry.
    fn ahead_of_default(&self) -> i64 {
        self.remote.as_ref().map_or(0, |remote| {
            remote.ahead_of_default_count.unwrap_or(remote.ahead_count)
        })
    }

    pub fn has_open_pull_request(&self) -> bool {
        self.remote
            .as_ref()
            .and_then(|remote| remote.pr.as_ref())
            .is_some_and(|pr| pr.state == VcsStatusRemoteResultPrState::Open)
    }

    /// What the forge calls a pull request: `MR` on GitLab, `PR` elsewhere (T3's
    /// `changeRequestTerminologyFor`).
    pub fn change_request(&self) -> &'static str {
        let kind = self
            .local
            .as_ref()
            .and_then(|local| local.source_control_provider.as_ref())
            .map(|provider| provider.kind.as_str());
        match kind {
            Some("gitlab") => "MR",
            _ => "PR",
        }
    }

    /// A short summary for the toolbar: `3 changed · ↑2 ↓1`, or `Up to date`.
    pub fn summary(&self) -> String {
        let mut parts = Vec::new();
        match self.changed_files() {
            0 => {}
            1 => parts.push("1 changed".to_string()),
            files => parts.push(format!("{files} changed")),
        }
        let arrows = [(self.ahead(), "↑"), (self.behind(), "↓")]
            .into_iter()
            .filter(|(count, _)| *count > 0)
            .map(|(count, arrow)| format!("{arrow}{count}"))
            .collect::<Vec<_>>();
        if !arrows.is_empty() {
            parts.push(arrows.join(" "));
        }
        if parts.is_empty() {
            if self.has_upstream() {
                "Up to date".to_string()
            } else {
                "Not pushed".to_string()
            }
        } else {
            parts.join(" · ")
        }
    }
}

// ---------------------------------------------------------------------------------------------
// What can be done

/// What the main button does.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum QuickKind {
    Run(GitStackedAction),
    Pull,
    /// Nothing to do; the hint says why.
    Unavailable,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct QuickAction {
    pub label: String,
    pub kind: QuickKind,
    pub hint: Option<String>,
}

impl QuickAction {
    fn run(label: impl Into<String>, action: GitStackedAction) -> Self {
        Self {
            label: label.into(),
            kind: QuickKind::Run(action),
            hint: None,
        }
    }

    fn unavailable(label: impl Into<String>, hint: impl Into<String>) -> Self {
        Self {
            label: label.into(),
            kind: QuickKind::Unavailable,
            hint: Some(hint.into()),
        }
    }
}

/// The main button's action, as T3's `resolveQuickAction`. mustr has no publish flow, so a
/// repository without a remote says so instead.
pub fn quick_action(status: Option<&GitStatus>, busy: bool) -> QuickAction {
    if busy {
        return QuickAction::unavailable("Commit", "A git action is running.");
    }
    let Some(status) = status.filter(|status| status.local.is_some()) else {
        return QuickAction::unavailable("Commit", "Git status isn't available yet.");
    };
    if !status.is_repo() {
        return QuickAction::unavailable("Commit", "This folder isn't a git repository.");
    }
    let pr = status.change_request();
    if status.branch().is_none() {
        return QuickAction::unavailable(
            "Commit",
            format!("Check out a branch before pushing or opening a {pr}."),
        );
    }
    let has_open_pr = status.has_open_pull_request();
    let is_default = status.is_default_ref();
    let has_remote = status.has_primary_remote();
    let ahead = status.ahead() > 0;
    let behind = status.behind() > 0;
    let push = || {
        if is_default {
            GitStackedAction::CommitPush
        } else {
            GitStackedAction::Push
        }
    };

    if status.is_dirty() {
        if !status.has_upstream() && !has_remote {
            return QuickAction::run("Commit", GitStackedAction::Commit);
        }
        if has_open_pr || is_default {
            return QuickAction::run("Commit & push", GitStackedAction::CommitPush);
        }
        return QuickAction::run(
            format!("Commit, push & {pr}"),
            GitStackedAction::CommitPushPr,
        );
    }
    if !status.has_upstream() {
        if !has_remote {
            return QuickAction::unavailable("Commit", "This repository has no remote to push to.");
        }
        if !ahead {
            if has_open_pr {
                return QuickAction::unavailable("Commit", "The branch is up to date.");
            }
            return QuickAction::unavailable("Push", "There are no local commits to push.");
        }
        if has_open_pr || is_default {
            return QuickAction::run("Push", push());
        }
        return QuickAction::run(format!("Push & create {pr}"), GitStackedAction::CreatePr);
    }
    if ahead && behind {
        return QuickAction::unavailable(
            "Sync branch",
            "The branch has diverged from its upstream. Rebase or merge first.",
        );
    }
    if behind {
        return QuickAction {
            label: "Pull".to_string(),
            kind: QuickKind::Pull,
            hint: None,
        };
    }
    if ahead {
        if has_open_pr || is_default {
            return QuickAction::run("Push", push());
        }
        return QuickAction::run(format!("Push & create {pr}"), GitStackedAction::CreatePr);
    }
    if !has_open_pr && status.ahead_of_default() > 0 && !is_default {
        return QuickAction::run(format!("Create {pr}"), GitStackedAction::CreatePr);
    }
    QuickAction::unavailable("Commit", "The branch is up to date.")
}

/// One entry of the Commit ▾ menu.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct MenuAction {
    pub label: String,
    pub kind: QuickKind,
    pub enabled: bool,
    /// Whether it asks for a commit message first.
    pub asks_for_message: bool,
}

/// The Commit ▾ menu: T3's commit, push, and create-PR entries (`buildMenuItems`), with
/// commit-and-push and commit-push-and-PR spelled out, and Pull when the branch is behind.
pub fn menu_actions(status: Option<&GitStatus>, busy: bool) -> Vec<MenuAction> {
    let Some(status) = status.filter(|status| status.is_repo()) else {
        return Vec::new();
    };
    let pr = status.change_request();
    let has_branch = status.branch().is_some();
    let has_open_pr = status.has_open_pull_request();
    let behind = status.behind() > 0;
    let has_remote = status.has_primary_remote();
    let can_reach_upstream = status.has_upstream() || has_remote;
    let can_commit = !busy && status.is_dirty();
    let can_push = !busy && has_branch && !behind && status.ahead() > 0 && can_reach_upstream;
    let can_create_pr = !busy
        && has_branch
        && !has_open_pr
        && status.ahead_of_default() > 0
        && !behind
        && can_reach_upstream;
    let commit_then = !busy && can_commit && has_branch && has_remote && !behind;

    let entry = |label: String, action: GitStackedAction, enabled: bool, asks: bool| MenuAction {
        label,
        kind: QuickKind::Run(action),
        enabled,
        asks_for_message: asks,
    };
    let mut actions = vec![entry(
        "Commit…".to_string(),
        GitStackedAction::Commit,
        can_commit,
        true,
    )];
    if !has_remote {
        return actions;
    }
    actions.push(entry(
        "Commit & push…".to_string(),
        GitStackedAction::CommitPush,
        commit_then,
        true,
    ));
    if !has_open_pr {
        actions.push(entry(
            format!("Commit, push & create {pr}…"),
            GitStackedAction::CommitPushPr,
            commit_then,
            true,
        ));
    }
    actions.push(entry(
        "Push".to_string(),
        GitStackedAction::Push,
        can_push,
        false,
    ));
    if !has_open_pr {
        actions.push(entry(
            format!("Create {pr}"),
            GitStackedAction::CreatePr,
            can_create_pr,
            false,
        ));
    }
    actions.push(MenuAction {
        label: "Pull".to_string(),
        kind: QuickKind::Pull,
        enabled: !busy && behind && status.has_upstream(),
        asks_for_message: false,
    });
    actions
}

/// Whether the action pushes, which on the default branch asks first (T3's
/// `requiresDefaultBranchConfirmation`).
pub fn pushes(action: &GitStackedAction) -> bool {
    matches!(
        action,
        GitStackedAction::Push
            | GitStackedAction::CreatePr
            | GitStackedAction::CommitPush
            | GitStackedAction::CommitPushPr
    )
}

pub fn commits(action: &GitStackedAction) -> bool {
    matches!(
        action,
        GitStackedAction::Commit | GitStackedAction::CommitPush | GitStackedAction::CommitPushPr
    )
}

/// The action's name in buttons and progress: `Commit, push & PR`.
pub fn action_label(action: &GitStackedAction, pr: &str) -> String {
    match action {
        GitStackedAction::Commit => "Commit".to_string(),
        GitStackedAction::Push => "Push".to_string(),
        GitStackedAction::CreatePr => format!("Create {pr}"),
        GitStackedAction::CommitPush => "Commit & push".to_string(),
        GitStackedAction::CommitPushPr => format!("Commit, push & {pr}"),
        GitStackedAction::Unrecognized(action) => action.clone(),
    }
}

// ---------------------------------------------------------------------------------------------
// Payloads

/// What the user chose for a stacked action.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct ActionOptions {
    /// The commit message; T3 writes one when it's empty.
    pub commit_message: Option<String>,
    /// Commit on a new branch that T3 names, instead of the checked-out one.
    pub feature_branch: bool,
}

/// `git.runStackedAction`'s input, as T3's `GitActionsControl` sends it for a thread.
pub fn stacked_action_input(
    checkout: &Checkout,
    action: GitStackedAction,
    options: ActionOptions,
    action_id: String,
) -> GitRunStackedActionInput {
    GitRunStackedActionInput {
        action,
        action_id,
        commit_message: options
            .commit_message
            .map(|message| message.trim().to_string())
            .filter(|message| !message.is_empty()),
        cwd: checkout.cwd.clone(),
        feature_branch: options.feature_branch.then_some(true),
        file_paths: None,
        project_id: None,
        thread_id: Some(checkout.thread_id.clone()),
    }
}

/// Records the thread's new branch (`thread.metadata.update`), with the one T3 had, so T3 can
/// refuse it if the thread moved on meanwhile.
pub fn branch_update(
    thread_id: &str,
    branch: String,
    expected_branch: Option<String>,
    command_id: String,
) -> OrchestrationV2Command {
    OrchestrationV2Command::ThreadMetadataUpdate(OrchestrationV2CommandThreadMetadataUpdate {
        branch: Some(branch),
        command_id,
        expected_branch,
        expected_empty: None,
        expected_worktree_path: None,
        limit_recovery: None,
        linked_pull_request: None,
        regenerate_title: None,
        thread_id: thread_id.to_string(),
        title: None,
        type_: OrchestrationV2CommandThreadMetadataUpdateType::ThreadMetadataUpdate,
        worktree_path: None,
    })
}

/// Where picking a branch takes the thread (`resolveBranchSelectionTarget` in T3's
/// `BranchToolbar.logic.ts`): a branch checked out in a worktree moves the thread into that
/// worktree, or back to the project's checkout when that's where it's checked out; the default
/// branch picked in a worktree moves the thread back to the project's checkout; any other branch
/// is checked out where the thread works.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct BranchTarget {
    /// Where the branch is checked out (`vcs.switchRef`), unless it already is.
    pub checkout_cwd: String,
    /// The thread's worktree afterwards; `None` for the project's checkout.
    pub worktree_path: Option<String>,
    /// Whether the branch is already checked out there, so only the thread moves.
    pub reuse_existing_worktree: bool,
}

pub fn branch_selection_target(
    project_root: &str,
    worktree_path: Option<&str>,
    git_ref: &VcsRef,
) -> BranchTarget {
    if let Some(ref_worktree) = git_ref.worktree_path.as_deref() {
        return BranchTarget {
            checkout_cwd: ref_worktree.to_string(),
            worktree_path: (ref_worktree != project_root).then(|| ref_worktree.to_string()),
            reuse_existing_worktree: true,
        };
    }
    let worktree_path = worktree_path.filter(|_| !git_ref.is_default);
    BranchTarget {
        checkout_cwd: worktree_path.unwrap_or(project_root).to_string(),
        worktree_path: worktree_path.map(str::to_string),
        reuse_existing_worktree: false,
    }
}

/// Moves a thread to another worktree, or to its project's checkout (`worktree_path` `None`),
/// on `branch` (`thread.metadata.update`), refused if the thread moved meanwhile
/// (`expectedWorktreePath`). It's raw JSON because moving to the project's checkout sends
/// `worktreePath: null`, which the typed command leaves out. T3 detaches the thread's agent
/// sessions itself when its worktree changes, so its next message starts in the new folder.
pub fn move_thread_command(
    thread_id: &str,
    branch: Option<&str>,
    worktree_path: Option<&str>,
    expected_worktree_path: Option<&str>,
    command_id: String,
) -> serde_json::Value {
    json!({
        "type": "thread.metadata.update",
        "commandId": command_id,
        "threadId": thread_id,
        "branch": branch,
        "worktreePath": worktree_path,
        "expectedWorktreePath": expected_worktree_path,
    })
}

/// Creates a worktree for the new branch `branch`, from `base`, in the project's checkout
/// (`vcs.createWorktree`, where T3 picks the folder), as T3's `t3_worktree_handoff` does.
pub fn create_worktree_input(
    project_root: &str,
    base: &str,
    branch: &str,
) -> VcsCreateWorktreePayload {
    VcsCreateWorktreePayload {
        base_ref_name: Some(base.to_string()),
        cwd: project_root.to_string(),
        new_ref_name: Some(branch.to_string()),
        path: (),
        ref_name: base.to_string(),
    }
}

pub fn list_refs_input(cwd: &str, query: &str) -> VcsListRefsInput {
    let query = query.trim();
    VcsListRefsInput {
        cursor: None,
        cwd: cwd.to_string(),
        include_matching_remote_refs: None,
        limit: Some(REFS_LIMIT),
        query: (!query.is_empty()).then(|| query.chars().take(256).collect()),
        ref_kind: None,
        refresh: None,
    }
}

/// The local name to switch to: a remote ref (`origin/feature`) checks out as `feature`, as in
/// T3's branch picker.
pub fn local_ref_name(git_ref: &VcsRef) -> String {
    if git_ref.is_remote == Some(true) {
        let prefix = git_ref
            .remote_name
            .as_ref()
            .map(|remote| format!("{remote}/"));
        if let Some(name) = prefix.and_then(|prefix| git_ref.name.strip_prefix(&prefix)) {
            return name.to_string();
        }
        if let Some((_, name)) = git_ref.name.split_once('/') {
            return name.to_string();
        }
    }
    git_ref.name.clone()
}

// ---------------------------------------------------------------------------------------------
// A running action's progress

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum StepState {
    Pending,
    Running,
    Done,
    Failed,
}

#[derive(Clone, Debug, PartialEq)]
pub enum RunOutcome {
    Finished(Box<GitRunStackedActionResult>),
    Failed {
        phase: Option<GitActionProgressPhase>,
        message: String,
    },
}

/// One stacked action, as its progress events arrive.
#[derive(Clone, Debug, PartialEq)]
pub struct ActionRun {
    pub action_id: String,
    pub action: GitStackedAction,
    pub phases: Vec<GitActionProgressPhase>,
    pub current: Option<GitActionProgressPhase>,
    /// T3's label for the current phase, such as `Pushing feature/x...`.
    pub label: Option<String>,
    /// The last line a hook printed.
    pub output: Option<String>,
    pub outcome: Option<RunOutcome>,
}

impl ActionRun {
    pub fn new(action_id: String, action: GitStackedAction) -> Self {
        Self {
            action_id,
            action,
            phases: Vec::new(),
            current: None,
            label: None,
            output: None,
            outcome: None,
        }
    }

    /// Applies one progress event. Events of other actions are ignored.
    pub fn apply(&mut self, event: GitActionProgressEvent) {
        match event {
            GitActionProgressEvent::ActionStarted(started)
                if started.action_id == self.action_id =>
            {
                self.phases = started.phases;
            }
            GitActionProgressEvent::PhaseStarted(phase) if phase.action_id == self.action_id => {
                if !self.phases.contains(&phase.phase) {
                    self.phases.push(phase.phase.clone());
                }
                self.current = Some(phase.phase);
                self.label = Some(phase.label);
                self.output = None;
            }
            GitActionProgressEvent::HookStarted(hook) if hook.action_id == self.action_id => {
                self.label = Some(format!("Running {}…", hook.hook_name));
            }
            GitActionProgressEvent::HookOutput(output) if output.action_id == self.action_id => {
                if let Some(line) = output
                    .text
                    .lines()
                    .rev()
                    .find(|line| !line.trim().is_empty())
                {
                    self.output = Some(line.trim().to_string());
                }
            }
            GitActionProgressEvent::ActionFinished(finished)
                if finished.action_id == self.action_id =>
            {
                self.current = None;
                self.outcome = Some(RunOutcome::Finished(Box::new(finished.result)));
            }
            GitActionProgressEvent::ActionFailed(failed) if failed.action_id == self.action_id => {
                self.outcome = Some(RunOutcome::Failed {
                    phase: failed.phase.or_else(|| self.current.clone()),
                    message: failed.message,
                });
            }
            _ => {}
        }
    }

    /// Ends the run with an error that came from the call itself, unless T3 already said how it
    /// ended.
    pub fn fail(&mut self, message: String) {
        if self.outcome.is_none() {
            self.outcome = Some(RunOutcome::Failed {
                phase: self.current.clone(),
                message,
            });
        }
    }

    pub fn is_running(&self) -> bool {
        self.outcome.is_none()
    }

    /// Each phase and how far it got.
    pub fn steps(&self) -> Vec<(GitActionProgressPhase, StepState)> {
        let current = self
            .current
            .as_ref()
            .and_then(|current| self.phases.iter().position(|phase| phase == current));
        let failed = match &self.outcome {
            Some(RunOutcome::Failed { phase, .. }) => Some(
                phase
                    .as_ref()
                    .and_then(|failed| self.phases.iter().position(|phase| phase == failed)),
            ),
            _ => None,
        };
        let finished = matches!(self.outcome, Some(RunOutcome::Finished(_)));
        self.phases
            .iter()
            .enumerate()
            .map(|(index, phase)| {
                let state = if finished {
                    StepState::Done
                } else if let Some(failed) = failed {
                    match failed {
                        Some(failed) if index < failed => StepState::Done,
                        Some(failed) if index == failed => StepState::Failed,
                        None if current.is_some_and(|current| index < current) => StepState::Done,
                        _ => StepState::Pending,
                    }
                } else {
                    match current {
                        Some(current) if index < current => StepState::Done,
                        Some(current) if index == current => StepState::Running,
                        _ => StepState::Pending,
                    }
                };
                (phase.clone(), state)
            })
            .collect()
    }
}

pub fn phase_label(phase: &GitActionProgressPhase, pr: &str) -> String {
    match phase {
        GitActionProgressPhase::Branch => "Branch".to_string(),
        GitActionProgressPhase::Commit => "Commit".to_string(),
        GitActionProgressPhase::Push => "Push".to_string(),
        GitActionProgressPhase::Pr => pr.to_string(),
        GitActionProgressPhase::Unrecognized(phase) => phase.clone(),
    }
}

/// The branch a finished action created, if it made one (T3's `resolveThreadBranchUpdate`).
pub fn created_branch(result: &GitRunStackedActionResult) -> Option<String> {
    (result.branch.status == GitRunStackedActionResultBranchStatus::Created)
        .then(|| result.branch.name.clone())
        .flatten()
}

/// The pull request a finished action created or found, by URL and number.
pub fn result_pull_request(result: &GitRunStackedActionResult) -> Option<(String, i64)> {
    match result.pr.status {
        GitRunStackedActionResultPrStatus::Created
        | GitRunStackedActionResultPrStatus::OpenedExisting => {
            Some((result.pr.url.clone()?, result.pr.number?))
        }
        _ => None,
    }
}

// ---------------------------------------------------------------------------------------------
// Editors

/// T3's editors in the order its Open in picker lists them (`EDITORS` in T3's
/// `packages/contracts/src/editor.ts`), with their names.
const EDITOR_ORDER: &[(&str, &str)] = &[
    ("cursor", "Cursor"),
    ("trae", "Trae"),
    ("kiro", "Kiro"),
    ("vscode", "VS Code"),
    ("vscode-insiders", "VS Code Insiders"),
    ("vscodium", "VSCodium"),
    ("zed", "Zed"),
    ("antigravity", "Antigravity"),
    ("idea", "IntelliJ IDEA"),
    ("aqua", "Aqua"),
    ("clion", "CLion"),
    ("datagrip", "DataGrip"),
    ("dataspell", "DataSpell"),
    ("goland", "GoLand"),
    ("phpstorm", "PhpStorm"),
    ("pycharm", "PyCharm"),
    ("rider", "Rider"),
    ("rubymine", "RubyMine"),
    ("rustrover", "RustRover"),
    ("webstorm", "WebStorm"),
    ("file-manager", "File Manager"),
];

pub fn editor_label(editor: &EditorId) -> String {
    if editor.as_str() == "file-manager" {
        return if cfg!(target_os = "macos") {
            "Finder".to_string()
        } else if cfg!(target_os = "windows") {
            "File Explorer".to_string()
        } else {
            "Files".to_string()
        };
    }
    EDITOR_ORDER
        .iter()
        .find(|(id, _)| *id == editor.as_str())
        .map_or_else(
            || editor.as_str().to_string(),
            |(_, label)| label.to_string(),
        )
}

/// The editors T3 found, in its order.
pub fn sorted_editors(available: &[EditorId]) -> Vec<EditorId> {
    let mut editors = available.to_vec();
    editors.sort_by_key(|editor| {
        EDITOR_ORDER
            .iter()
            .position(|(id, _)| *id == editor.as_str())
            .unwrap_or(EDITOR_ORDER.len())
    });
    editors.dedup();
    editors
}

pub fn open_in_editor_input(cwd: &str, editor: EditorId) -> LaunchEditorInput {
    LaunchEditorInput {
        cwd: cwd.to_string(),
        editor,
        reveal: None,
    }
}

/// The editors T3 can open, from `server.getConfig`, and the one last used (T3 remembers it
/// as `t3code:last-editor`).
pub struct Editors {
    available: Option<Vec<EditorId>>,
    preferred: Option<EditorId>,
    error: Option<SharedString>,
    _load: Option<Task<()>>,
}

struct GlobalEditors(Entity<Editors>);

impl Global for GlobalEditors {}

impl Editors {
    pub fn global(cx: &mut App) -> Entity<Self> {
        if let Some(global) = cx.try_global::<GlobalEditors>() {
            return global.0.clone();
        }
        let editors = cx.new(|cx| {
            let mut editors = Self {
                available: None,
                preferred: None,
                error: None,
                _load: None,
            };
            editors.load(cx);
            editors
        });
        cx.set_global(GlobalEditors(editors.clone()));
        editors
    }

    fn load(&mut self, cx: &mut Context<Self>) {
        let Some(backend) = thread_chat::backend(cx) else {
            self.error = Some("mustr isn't connected to T3".into());
            return;
        };
        self._load = Some(cx.spawn(async move |this, cx| {
            let mut attempt = 0;
            loop {
                // Only the editors are read, so a config this build can't decode in full
                // still lists them.
                let config = backend
                    .call_raw(rpc::ServerGetConfig::TAG, json!({}), Access::Read)
                    .await
                    .and_then(|config| {
                        serde_json::from_value::<Vec<EditorId>>(
                            config.get("availableEditors").cloned().unwrap_or_default(),
                        )
                        .context("T3 listed editors mustr can't read")
                    });
                let loaded = this.update(cx, |this, cx| {
                    let loaded = match config {
                        Ok(available) => {
                            this.available = Some(sorted_editors(&available));
                            this.error = None;
                            true
                        }
                        Err(error) => {
                            log::log!(
                                thread_chat::retry_log_level(&error, log::Level::Warn),
                                "couldn't load T3's editors: {error:#}"
                            );
                            this.error = Some(format!("{error:#}").into());
                            false
                        }
                    };
                    cx.notify();
                    loaded
                });
                if !matches!(loaded, Ok(false)) || attempt >= 4 {
                    return;
                }
                cx.background_executor()
                    .timer(Duration::from_secs(2u64 << attempt))
                    .await;
                attempt += 1;
            }
        }));
    }

    pub fn available(&self) -> &[EditorId] {
        self.available.as_deref().unwrap_or_default()
    }

    /// The editor Open uses: the last one used if T3 still has it, or else T3's first.
    pub fn preferred(&self) -> Option<EditorId> {
        let available = self.available();
        self.preferred
            .clone()
            .filter(|editor| available.contains(editor))
            .or_else(|| available.first().cloned())
    }

    pub fn set_preferred(&mut self, editor: EditorId, cx: &mut Context<Self>) {
        self.preferred = Some(editor);
        cx.notify();
    }

    /// Opens `cwd` in `editor`, and remembers it.
    pub fn open(
        &mut self,
        cwd: &str,
        editor: EditorId,
        cx: &mut Context<Self>,
    ) -> Task<Result<()>> {
        let Some(backend) = thread_chat::backend(cx) else {
            return Task::ready(Err(anyhow!("mustr isn't connected to T3")));
        };
        self.set_preferred(editor.clone(), cx);
        let call = backend.call::<rpc::ShellOpenInEditor>(
            open_in_editor_input(cwd, editor.clone()),
            Access::Operate,
        );
        let label = editor_label(&editor);
        cx.background_spawn(async move {
            call.await
                .with_context(|| format!("opening the checkout in {label}"))
        })
    }
}

// ---------------------------------------------------------------------------------------------
// One thread's git

/// What happened last, next to the buttons.
#[derive(Clone, Debug, PartialEq)]
pub enum Notice {
    Error(SharedString),
    Done(SharedString),
}

/// One thread's checkout: its status, followed from T3, and the git actions on it.
pub struct ThreadGit {
    thread_id: String,
    backend: Option<Arc<dyn ThreadBackend>>,
    checkout: Option<Checkout>,
    status: Option<GitStatus>,
    status_error: Option<SharedString>,
    run: Option<ActionRun>,
    /// A branch switch, a new branch, or a pull in flight, by what it says.
    busy: Option<SharedString>,
    notice: Option<Notice>,
    /// When the agent last finished working in the checkout.
    run_completed_at: Option<String>,
    _shells: Option<Subscription>,
    _status: Option<Task<()>>,
    _run: Option<Task<()>>,
    _clear_notice: Option<Task<()>>,
    _tasks: Vec<Task<()>>,
}

#[derive(Default)]
struct ThreadGits {
    gits: HashMap<String, WeakEntity<ThreadGit>>,
    /// Threads with an action running, or a result still to show, are kept even while no view
    /// shows them, such as while the review that started from the toolbar opens.
    pinned: HashMap<String, Entity<ThreadGit>>,
}

impl Global for ThreadGits {}

impl ThreadGit {
    /// The thread's git, shared with every other view of the same thread.
    pub fn for_thread(thread_id: &str, cx: &mut App) -> Entity<Self> {
        if let Some(existing) = cx
            .try_global::<ThreadGits>()
            .and_then(|gits| gits.gits.get(thread_id))
            .and_then(WeakEntity::upgrade)
        {
            return existing;
        }
        let git = cx.new(|cx| Self::new(thread_id.to_string(), cx));
        let gits = cx.default_global::<ThreadGits>();
        gits.gits.retain(|_, git| git.upgrade().is_some());
        gits.gits.insert(thread_id.to_string(), git.downgrade());
        git
    }

    fn pin(&self, cx: &mut Context<Self>) {
        let entity = cx.entity();
        cx.default_global::<ThreadGits>()
            .pinned
            .insert(self.thread_id.clone(), entity);
    }

    /// Lets the thread's git go with its last view once there's nothing left to show.
    fn unpin_if_idle(&self, cx: &mut Context<Self>) {
        if self.is_busy() || self.notice.is_some() || self.run.is_some() {
            return;
        }
        cx.default_global::<ThreadGits>()
            .pinned
            .remove(&self.thread_id);
    }

    fn new(thread_id: String, cx: &mut Context<Self>) -> Self {
        let backend = thread_chat::backend(cx);
        let mut this = Self {
            thread_id,
            backend,
            checkout: None,
            status: None,
            status_error: None,
            run: None,
            busy: None,
            notice: None,
            run_completed_at: None,
            _shells: None,
            _status: None,
            _run: None,
            _clear_notice: None,
            _tasks: Vec::new(),
        };
        if this.backend.is_some() {
            let shells = ThreadShells::global(cx);
            this._shells = Some(cx.observe(&shells, |this, shells, cx| {
                this.update_checkout(&shells, cx)
            }));
            this.update_checkout(&shells, cx);
        } else {
            this.status_error = Some("mustr isn't connected to T3".into());
        }
        this
    }

    pub fn checkout(&self) -> Option<&Checkout> {
        self.checkout.as_ref()
    }

    pub fn status(&self) -> Option<&GitStatus> {
        self.status.as_ref()
    }

    pub fn status_error(&self) -> Option<&SharedString> {
        self.status_error.as_ref()
    }

    pub fn run(&self) -> Option<&ActionRun> {
        self.run.as_ref()
    }

    pub fn notice(&self) -> Option<&Notice> {
        self.notice.as_ref()
    }

    pub fn busy_label(&self) -> Option<&SharedString> {
        self.busy.as_ref()
    }

    pub fn is_busy(&self) -> bool {
        self.busy.is_some() || self.run.as_ref().is_some_and(ActionRun::is_running)
    }

    /// Why writes to the checkout are refused before they're sent, if they are.
    pub fn write_blocked_reason(&self) -> Option<String> {
        match &self.backend {
            Some(backend) => backend.blocked_reason(Access::SourceControlWrite),
            None => Some("mustr isn't connected to T3".to_string()),
        }
    }

    pub fn change_request(&self) -> &'static str {
        self.status.as_ref().map_or("PR", GitStatus::change_request)
    }

    pub fn quick_action(&self) -> QuickAction {
        quick_action(self.status.as_ref(), self.is_busy())
    }

    pub fn menu_actions(&self) -> Vec<MenuAction> {
        menu_actions(self.status.as_ref(), self.is_busy())
    }

    fn update_checkout(&mut self, shells: &Entity<ThreadShells>, cx: &mut Context<Self>) {
        let shells = shells.read(cx);
        let checkout = shells
            .shell()
            .and_then(|shell| Checkout::of_thread(shell, &self.thread_id));
        // T3 doesn't watch the checkout, so look again once the agent has worked in it.
        let run_completed_at = shells
            .thread(&self.thread_id)
            .and_then(|thread| thread.latest_run_completed_at.clone());
        if run_completed_at != self.run_completed_at {
            let first = self.run_completed_at.is_none();
            self.run_completed_at = run_completed_at;
            if !first && checkout == self.checkout {
                self.refresh(cx);
            }
        }
        if checkout == self.checkout {
            return;
        }
        let cwd_changed = checkout.as_ref().map(|checkout| &checkout.cwd)
            != self.checkout.as_ref().map(|checkout| &checkout.cwd);
        self.checkout = checkout;
        if cwd_changed {
            self.status = None;
            self.follow_status(cx);
        }
        cx.notify();
    }

    /// Follows `subscribeVcsStatus` for the checkout, resubscribing when the stream ends.
    fn follow_status(&mut self, cx: &mut Context<Self>) {
        let (Some(backend), Some(checkout)) = (self.backend.clone(), self.checkout.clone()) else {
            self._status = None;
            return;
        };
        self._status = Some(cx.spawn(async move |this, cx| {
            let mut attempt = 0;
            loop {
                match follow_status_once(&backend, &checkout.cwd, &this, cx).await {
                    Ok(true) => attempt = 0,
                    Ok(false) => {}
                    Err(error) => {
                        log::warn!("couldn't follow git status of {}: {error:#}", checkout.cwd);
                        let updated = this.update(cx, |this, cx| {
                            this.status_error = Some(format!("{error:#}").into());
                            cx.notify();
                        });
                        if updated.is_err() {
                            return;
                        }
                    }
                }
                cx.background_executor().timer(retry_delay(attempt)).await;
                attempt += 1;
            }
        }));
    }

    /// Asks T3 to look at the checkout again; the status stream brings the result.
    pub fn refresh(&mut self, cx: &mut Context<Self>) {
        let (Some(backend), Some(checkout)) = (self.backend.clone(), self.checkout.clone()) else {
            return;
        };
        let refresh = backend.call::<rpc::VcsRefreshStatus>(
            VcsRefreshStatusPayload {
                cwd: checkout.cwd,
                local_only: None,
            },
            Access::Read,
        );
        self._tasks.push(cx.spawn(async move |_, _| {
            if let Err(error) = refresh.await {
                log::warn!("couldn't refresh git status: {error:#}");
            }
        }));
    }

    /// Runs a stacked action on the checkout, following its progress.
    pub fn run_action(
        &mut self,
        action: GitStackedAction,
        options: ActionOptions,
        cx: &mut Context<Self>,
    ) {
        if self.is_busy() {
            return;
        }
        let (Some(backend), Some(checkout)) = (self.backend.clone(), self.checkout.clone()) else {
            self.set_notice(
                Notice::Error("This thread has no checkout to commit in.".into()),
                cx,
            );
            return;
        };
        let action_id = thread_chat::commands::new_id();
        let input = stacked_action_input(&checkout, action.clone(), options, action_id.clone());
        self.pin(cx);
        self.notice = None;
        self.run = Some(ActionRun::new(action_id, action));
        cx.notify();
        let stream = backend.stream::<rpc::GitRunStackedAction>(input, Access::SourceControlWrite);
        self._run = Some(cx.spawn(async move |this, cx| {
            let mut stream = match stream {
                Ok(stream) => stream,
                Err(error) => {
                    this.update(cx, |this, cx| this.end_run(Err(error), cx))
                        .ok();
                    return;
                }
            };
            while let Some(event) = stream.next().await {
                let applied = this.update(cx, |this, cx| {
                    match event {
                        Ok(event) => {
                            if let Some(run) = &mut this.run {
                                run.apply(event);
                            }
                        }
                        // An event this build can't read: the stream goes on.
                        Err(error) => log::warn!("git.runStackedAction: {error:#}"),
                    }
                    cx.notify();
                    this.run.as_ref().is_some_and(|run| !run.is_running())
                });
                match applied {
                    Ok(true) => break,
                    Ok(false) => {}
                    Err(_) => return,
                }
            }
            this.update(cx, |this, cx| this.end_run(Ok(()), cx)).ok();
        }));
    }

    fn end_run(&mut self, result: Result<()>, cx: &mut Context<Self>) {
        let pr = self.change_request();
        let Some(run) = &mut self.run else {
            return;
        };
        match result {
            Err(error) => run.fail(format!("{error:#}")),
            Ok(()) => run.fail("T3 stopped reporting the action's progress.".to_string()),
        }
        match run.outcome.clone() {
            Some(RunOutcome::Finished(result)) => {
                if let Some(branch) = created_branch(&result) {
                    self.record_branch(branch, cx);
                }
                let title = result.toast.title.clone();
                self.set_notice(Notice::Done(title.into()), cx);
            }
            Some(RunOutcome::Failed { phase, message }) => {
                let what = phase.map_or_else(
                    || action_label(&run.action, pr),
                    |phase| phase_label(&phase, pr),
                );
                self.set_notice(
                    Notice::Error(format!("{what} failed: {message}").into()),
                    cx,
                );
            }
            None => {}
        }
        self.refresh(cx);
        cx.notify();
    }

    /// Shows what happened. Successes go away on their own, as T3's toasts do; failures stay
    /// until they're dismissed.
    fn set_notice(&mut self, notice: Notice, cx: &mut Context<Self>) {
        let clears = matches!(notice, Notice::Done(_));
        self.notice = Some(notice.clone());
        self._clear_notice = clears.then(|| {
            cx.spawn(async move |this, cx| {
                cx.background_executor().timer(RESULT_VISIBLE).await;
                this.update(cx, |this, cx| {
                    if this.notice.as_ref() == Some(&notice) {
                        this.dismiss(cx);
                    }
                })
                .ok();
            })
        });
        cx.notify();
    }

    pub fn dismiss(&mut self, cx: &mut Context<Self>) {
        self.notice = None;
        if self.run.as_ref().is_some_and(|run| !run.is_running()) {
            self.run = None;
        }
        self.unpin_if_idle(cx);
        cx.notify();
    }

    /// Tells T3 the thread is on another branch now.
    fn record_branch(&mut self, branch: String, cx: &mut Context<Self>) {
        let Some(backend) = self.backend.clone() else {
            return;
        };
        let expected = self
            .checkout
            .as_ref()
            .and_then(|checkout| checkout.branch.clone());
        if expected.as_deref() == Some(branch.as_str()) {
            return;
        }
        if backend.blocked_reason(Access::Operate).is_some() {
            return;
        }
        let dispatch = backend.dispatch(branch_update(
            &self.thread_id,
            branch,
            expected,
            thread_chat::commands::new_id(),
        ));
        self._tasks.push(cx.spawn(async move |_, _| {
            if let Err(error) = dispatch.await {
                log::warn!("couldn't record the thread's branch: {error:#}");
            }
        }));
    }

    /// Pulls the branch's upstream (`vcs.pull`).
    pub fn pull(&mut self, cx: &mut Context<Self>) {
        let Some((backend, checkout)) = self.start_busy("Pulling…", cx) else {
            return;
        };
        let pull = backend.call::<rpc::VcsPull>(
            VcsPullInput { cwd: checkout.cwd },
            Access::SourceControlWrite,
        );
        self._tasks.push(cx.spawn(async move |this, cx| {
            let result = pull.await;
            this.update(cx, |this, cx| {
                let result = result.map(|pulled| format!("Pulled {}", pulled.ref_name));
                this.end_busy(result.context("pulling"), cx)
            })
            .ok();
        }));
    }

    /// Switches the checkout to `name` (`vcs.switchRef`), then records it on the thread.
    pub fn switch_ref(&mut self, name: String, cx: &mut Context<Self>) {
        let Some((backend, checkout)) = self.start_busy(format!("Switching to {name}…"), cx)
        else {
            return;
        };
        let switch = backend.call::<rpc::VcsSwitchRef>(
            VcsSwitchRefInput {
                cwd: checkout.cwd,
                ref_name: name.clone(),
            },
            Access::SourceControlWrite,
        );
        self._tasks.push(cx.spawn(async move |this, cx| {
            let result = switch.await;
            this.update(cx, |this, cx| match result {
                Ok(switched) => {
                    let branch = switched.ref_name.unwrap_or(name);
                    this.record_branch(branch.clone(), cx);
                    this.end_busy(Ok(format!("Switched to {branch}")), cx);
                }
                Err(error) => this.end_busy(Err(error.context("switching the branch")), cx),
            })
            .ok();
        }));
    }

    /// Creates the branch `name` and switches to it (`vcs.createRef` with `switchRef`).
    pub fn create_ref(&mut self, name: String, cx: &mut Context<Self>) {
        let name = name.trim().to_string();
        if name.is_empty() {
            return;
        }
        let Some((backend, checkout)) = self.start_busy(format!("Creating {name}…"), cx) else {
            return;
        };
        let create = backend.call::<rpc::VcsCreateRef>(
            VcsCreateRefInput {
                cwd: checkout.cwd,
                ref_name: name,
                switch_ref: Some(true),
            },
            Access::SourceControlWrite,
        );
        self._tasks.push(cx.spawn(async move |this, cx| {
            let result = create.await;
            this.update(cx, |this, cx| match result {
                Ok(created) => {
                    this.record_branch(created.ref_name.clone(), cx);
                    this.end_busy(Ok(format!("Created {}", created.ref_name)), cx);
                }
                Err(error) => this.end_busy(Err(error.context("creating the branch")), cx),
            })
            .ok();
        }));
    }

    /// Why the thread can't move to another folder now, if it can't: its agent is working, or
    /// mustr can't change the thread or its checkout.
    pub fn move_blocked_reason(&self, cx: &mut App) -> Option<String> {
        let backend = self.backend.as_ref()?;
        let working = ThreadShells::global(cx)
            .read(cx)
            .thread(&self.thread_id)
            .is_some_and(|thread| thread.active_run_id.is_some());
        if working {
            return Some("Stop the agent's run before moving the thread.".to_string());
        }
        backend
            .blocked_reason(Access::Operate)
            .or_else(|| backend.blocked_reason(Access::SourceControlWrite))
    }

    /// Picks `git_ref` in the branch picker, as T3's branch picker does: a branch checked out in
    /// a worktree moves the thread there, the default branch picked in a worktree checks it out
    /// in the project's checkout and moves the thread back there, and any other branch is checked
    /// out where the thread works ([`branch_selection_target`]).
    pub fn select_ref(&mut self, git_ref: VcsRef, cx: &mut Context<Self>) {
        let name = local_ref_name(&git_ref);
        let Some(checkout) = self.checkout.clone() else {
            self.switch_ref(name, cx);
            return;
        };
        let project_root = checkout
            .project_root
            .clone()
            .unwrap_or_else(|| checkout.cwd.clone());
        let target =
            branch_selection_target(&project_root, checkout.worktree_path.as_deref(), &git_ref);
        if target.worktree_path == checkout.worktree_path {
            if !target.reuse_existing_worktree {
                self.switch_ref(name, cx);
            }
            return;
        }
        if let Some(reason) = self.move_blocked_reason(cx) {
            self.set_notice(Notice::Error(reason.into()), cx);
            return;
        }
        let destination = match &target.worktree_path {
            Some(_) => "its worktree",
            None => "the project's checkout",
        };
        let Some((backend, checkout)) = self.start_busy(format!("Moving to {destination}…"), cx)
        else {
            return;
        };
        let switch = (!target.reuse_existing_worktree).then(|| {
            backend.call::<rpc::VcsSwitchRef>(
                VcsSwitchRefInput {
                    cwd: target.checkout_cwd.clone(),
                    ref_name: name.clone(),
                },
                Access::SourceControlWrite,
            )
        });
        let thread_id = self.thread_id.clone();
        self._tasks.push(cx.spawn(async move |this, cx| {
            let result = async {
                let branch = match switch {
                    Some(switch) => switch
                        .await
                        .context("switching the branch")?
                        .ref_name
                        .unwrap_or(name),
                    None => name,
                };
                backend
                    .call_raw(
                        rpc::OrchestrationDispatchCommand::TAG,
                        move_thread_command(
                            &thread_id,
                            Some(&branch),
                            target.worktree_path.as_deref(),
                            checkout.worktree_path.as_deref(),
                            thread_chat::commands::new_id(),
                        ),
                        Access::Operate,
                    )
                    .await
                    .context("moving the thread")?;
                anyhow::Ok(format!("Moved to {destination}, on {branch}"))
            }
            .await;
            this.update(cx, |this, cx| this.end_busy(result, cx)).ok();
        }));
    }

    /// Moves a thread that works in its project's checkout to a new worktree, on the new branch
    /// `name` from the checkout's branch, as T3's `t3_worktree_handoff` does: creates the
    /// worktree (`vcs.createWorktree`), then moves the thread there (`thread.metadata.update`),
    /// removing the worktree again if T3 refuses the move.
    pub fn move_to_new_worktree(&mut self, name: String, cx: &mut Context<Self>) {
        let name = name.trim().to_string();
        let Some(checkout) = self.checkout.clone().filter(|_| !name.is_empty()) else {
            return;
        };
        if checkout.worktree_path.is_some() {
            self.set_notice(
                Notice::Error("The thread already works in a worktree.".into()),
                cx,
            );
            return;
        }
        let base = self
            .status
            .as_ref()
            .and_then(GitStatus::branch)
            .map(str::to_string)
            .or_else(|| checkout.branch.clone());
        let Some(base) = base else {
            self.set_notice(
                Notice::Error("The project's checkout isn't on a branch to start from.".into()),
                cx,
            );
            return;
        };
        if let Some(reason) = self.move_blocked_reason(cx) {
            self.set_notice(Notice::Error(reason.into()), cx);
            return;
        }
        let Some((backend, checkout)) =
            self.start_busy(format!("Creating a worktree for {name}…"), cx)
        else {
            return;
        };
        let project_root = checkout.project_root.clone().unwrap_or(checkout.cwd);
        let create = backend.call::<rpc::VcsCreateWorktree>(
            create_worktree_input(&project_root, &base, &name),
            Access::SourceControlWrite,
        );
        let thread_id = self.thread_id.clone();
        self._tasks.push(cx.spawn(async move |this, cx| {
            let result = async {
                let created = create.await.context("creating the worktree")?.worktree;
                let moved = backend
                    .call_raw(
                        rpc::OrchestrationDispatchCommand::TAG,
                        move_thread_command(
                            &thread_id,
                            Some(&created.ref_name),
                            Some(&created.path),
                            None,
                            thread_chat::commands::new_id(),
                        ),
                        Access::Operate,
                    )
                    .await;
                if let Err(error) = moved {
                    let remove = backend.call::<rpc::VcsRemoveWorktree>(
                        VcsRemoveWorktreeInput {
                            cwd: project_root,
                            force: Some(true),
                            path: created.path,
                        },
                        Access::SourceControlWrite,
                    );
                    if let Err(error) = remove.await {
                        log::warn!(
                            "couldn't remove the worktree the thread didn't move to: {error:#}"
                        );
                    }
                    return Err(error.context("moving the thread"));
                }
                anyhow::Ok(format!("Moved to a new worktree on {}", created.ref_name))
            }
            .await;
            this.update(cx, |this, cx| this.end_busy(result, cx)).ok();
        }));
    }

    fn start_busy(
        &mut self,
        label: impl Into<SharedString>,
        cx: &mut Context<Self>,
    ) -> Option<(Arc<dyn ThreadBackend>, Checkout)> {
        if self.is_busy() {
            return None;
        }
        let (Some(backend), Some(checkout)) = (self.backend.clone(), self.checkout.clone()) else {
            self.set_notice(Notice::Error("This thread has no checkout.".into()), cx);
            return None;
        };
        self.busy = Some(label.into());
        self.notice = None;
        self.run = None;
        self.pin(cx);
        cx.notify();
        Some((backend, checkout))
    }

    fn end_busy(&mut self, result: Result<String>, cx: &mut Context<Self>) {
        self.busy = None;
        match result {
            Ok(done) => self.set_notice(Notice::Done(done.into()), cx),
            Err(error) => {
                log::warn!("git action failed: {error:#}");
                self.set_notice(Notice::Error(format!("{error:#}").into()), cx)
            }
        }
        self.refresh(cx);
    }

    /// Opens the checkout in the preferred editor (or `editor`).
    pub fn open_in_editor(&mut self, editor: Option<EditorId>, cx: &mut Context<Self>) {
        let Some(checkout) = self.checkout.clone() else {
            self.set_notice(
                Notice::Error("This thread has no checkout to open.".into()),
                cx,
            );
            return;
        };
        let editors = Editors::global(cx);
        let Some(editor) = editor.or_else(|| editors.read(cx).preferred()) else {
            self.set_notice(
                Notice::Error("T3 found no editors on its machine.".into()),
                cx,
            );
            return;
        };
        let open = editors.update(cx, |editors, cx| editors.open(&checkout.cwd, editor, cx));
        self._tasks.push(cx.spawn(async move |this, cx| {
            if let Err(error) = open.await {
                this.update(cx, |this, cx| {
                    this.set_notice(Notice::Error(format!("{error:#}").into()), cx)
                })
                .ok();
            }
        }));
    }

    /// Lists the checkout's branches matching `query`.
    pub fn list_refs(&self, query: &str) -> Option<BoxFuture<'static, Result<Vec<VcsRef>>>> {
        let backend = self.backend.clone()?;
        let checkout = self.checkout.as_ref()?;
        let list =
            backend.call::<rpc::VcsListRefs>(list_refs_input(&checkout.cwd, query), Access::Read);
        Some(async move { Ok(list.await?.refs) }.boxed())
    }
}

/// Follows the status until the stream ends. Returns whether anything arrived.
async fn follow_status_once(
    backend: &Arc<dyn ThreadBackend>,
    cwd: &str,
    this: &WeakEntity<ThreadGit>,
    cx: &mut AsyncApp,
) -> Result<bool> {
    let mut stream = backend.stream::<rpc::SubscribeVcsStatus>(
        VcsStatusSubscriptionInput {
            cwd: cwd.to_string(),
            include_remote: None,
        },
        Access::Read,
    )?;
    let mut received = false;
    while let Some(event) = stream.next().await {
        let event = match event {
            Ok(event) => event,
            Err(error) => {
                log::warn!("subscribeVcsStatus: {error:#}");
                continue;
            }
        };
        received = true;
        this.update(cx, |this, cx| {
            let status = this.status.get_or_insert_default();
            if status.apply(event) {
                this.status_error = None;
                cx.notify();
            }
        })?;
    }
    Ok(received)
}

pub(crate) fn retry_delay(attempt: usize) -> Duration {
    const DELAYS: [Duration; 4] = [
        Duration::from_secs(1),
        Duration::from_secs(2),
        Duration::from_secs(5),
        Duration::from_secs(10),
    ];
    DELAYS[attempt.min(DELAYS.len() - 1)]
}

// ---------------------------------------------------------------------------------------------
// Menus anchored where they were opened

/// A context menu open at a point, for views that open menus from mustr's buttons and chips.
#[derive(Default)]
pub struct MenuSlot(Option<(Entity<ContextMenu>, Point<Pixels>, Subscription)>);

impl MenuSlot {
    pub fn deploy<V: 'static>(
        &mut self,
        position: Point<Pixels>,
        menu: Entity<ContextMenu>,
        window: &mut Window,
        cx: &mut Context<V>,
        slot: fn(&mut V) -> &mut MenuSlot,
    ) {
        let subscription =
            cx.subscribe_in(&menu, window, move |this, _, _: &DismissEvent, _, cx| {
                slot(this).0 = None;
                cx.notify();
            });
        window.focus(&menu.focus_handle(cx), cx);
        self.0 = Some((menu, position, subscription));
        cx.notify();
    }

    /// Whether a menu is open. The buttons that open it leave out their tooltips while it is,
    /// since GPUI shows a tooltip that was pending when the click came, over the menu.
    pub fn is_open(&self) -> bool {
        self.0.is_some()
    }

    pub fn render(&self) -> Option<AnyElement> {
        let (menu, position, _) = self.0.as_ref()?;
        Some(
            deferred(anchored().position(*position).child(menu.clone()))
                .with_priority(1)
                .into_any_element(),
        )
    }
}

/// Below the element that was clicked, for menus opened by a click.
pub fn menu_position(event: &ClickEvent, window: &Window) -> Point<Pixels> {
    match event {
        ClickEvent::Keyboard(event) => event.bounds.bottom_left(),
        event => {
            let rem_size = window.rem_size();
            event.position()
                + Point::new(
                    scaled(px(-8.)).to_pixels(rem_size),
                    scaled(px(10.)).to_pixels(rem_size),
                )
        }
    }
}

// ---------------------------------------------------------------------------------------------
// Views

/// Where the controls are drawn, which decides what they show.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ControlsPlacement {
    /// The thread toolbar, whose progress row shows a running action's steps.
    Toolbar,
    /// A review's header, which says what's running on the button itself.
    ReviewHeader,
}

/// Which of its buttons a [`ThreadGitControls`] draws. The thread toolbar draws each on its own,
/// so that Open in can go into the toolbar's "…" menu when the window is narrow.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ControlsParts {
    Both,
    /// Open in ▾.
    OpenIn,
    /// Commit ▾ (or whatever the branch needs next), with what the last action did.
    GitAction,
}

/// A thread's Open in ▾ and Commit ▾ buttons, with what the last action did.
pub struct ThreadGitControls {
    git: Entity<ThreadGit>,
    editors: Entity<Editors>,
    workspace: WeakEntity<Workspace>,
    placement: ControlsPlacement,
    parts: ControlsParts,
    /// Measures the buttons, when drawn together in a review's header.
    overflow: OverflowRow,
    /// Whether Open in didn't fit when last drawn, so Commit ▾'s menu offers the editors.
    open_in_hidden: bool,
    menu: MenuSlot,
    _subscriptions: [Subscription; 2],
}

impl ThreadGitControls {
    pub fn new(
        git: Entity<ThreadGit>,
        workspace: WeakEntity<Workspace>,
        placement: ControlsPlacement,
        cx: &mut Context<Self>,
    ) -> Self {
        let editors = Editors::global(cx);
        Self {
            _subscriptions: [
                cx.observe(&git, |_, _, cx| cx.notify()),
                cx.observe(&editors, |_, _, cx| cx.notify()),
            ],
            git,
            editors,
            workspace,
            placement,
            parts: ControlsParts::Both,
            overflow: OverflowRow::new(),
            open_in_hidden: false,
            menu: MenuSlot::default(),
        }
    }

    /// Draws only some of the buttons.
    pub fn parts(mut self, parts: ControlsParts) -> Self {
        self.parts = parts;
        self
    }

    /// Runs the main button's action, asking first when it would push to the default branch.
    pub fn run_quick_action(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let quick = self.git.read(cx).quick_action();
        match quick.kind {
            QuickKind::Run(action) => self.start_action(action, false, window, cx),
            QuickKind::Pull => self.git.update(cx, |git, cx| git.pull(cx)),
            QuickKind::Unavailable => {}
        }
    }

    /// Starts `action`: straight away, or through the dialog when it asks for a message or would
    /// push to the default branch.
    pub fn start_action(
        &mut self,
        action: GitStackedAction,
        asks_for_message: bool,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let git = self.git.read(cx);
        let status = git.status().cloned().unwrap_or_default();
        let default_branch = (status.is_default_ref() && pushes(&action))
            .then(|| status.branch().map(str::to_string))
            .flatten();
        if !asks_for_message && default_branch.is_none() {
            self.git.update(cx, |git, cx| {
                git.run_action(action, ActionOptions::default(), cx)
            });
            return;
        }
        let git = self.git.clone();
        let opened = self.workspace.update(cx, |workspace, cx| {
            workspace.toggle_modal(window, cx, |window, cx| {
                GitActionDialog::new(git, action, asks_for_message, default_branch, window, cx)
            })
        });
        if opened.is_err() {
            log::error!("couldn't open the git action dialog");
        }
    }

    fn deploy_commit_menu(
        &mut self,
        event: &ClickEvent,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let actions = self.git.read(cx).menu_actions();
        let this = cx.weak_entity();
        let git = self.git.clone();
        let editors = self
            .open_in_hidden
            .then(|| {
                let editors = self.editors.read(cx);
                (editors.available().to_vec(), editors.preferred())
            })
            .filter(|(available, _)| !available.is_empty());
        let menu = ContextMenu::build(window, cx, move |mut menu, _, _| {
            // Open in, when there was no room for it next to the button.
            if let Some((available, preferred)) = editors {
                for editor in available {
                    let git = git.clone();
                    let checked = preferred.as_ref() == Some(&editor);
                    menu = menu.toggleable_entry(
                        format!("Open in {}", editor_label(&editor)),
                        checked,
                        IconPosition::Start,
                        None,
                        move |_, cx| {
                            let editor = editor.clone();
                            git.update(cx, |git, cx| git.open_in_editor(Some(editor), cx))
                        },
                    );
                }
                menu = menu.separator();
            }
            if actions.is_empty() {
                return menu.label("Git status isn't available yet");
            }
            for (index, action) in actions.into_iter().enumerate() {
                if index == 3
                    || (index == 1 && action.kind == QuickKind::Run(GitStackedAction::Push))
                {
                    menu = menu.separator();
                }
                let this = this.clone();
                let git = git.clone();
                let kind = action.kind.clone();
                let asks = action.asks_for_message;
                menu = menu.item(
                    ContextMenuEntry::new(action.label)
                        .disabled(!action.enabled)
                        .handler(move |window, cx| match kind.clone() {
                            QuickKind::Run(action) => {
                                this.update(cx, |this, cx| {
                                    this.start_action(action, asks, window, cx)
                                })
                                .ok();
                            }
                            QuickKind::Pull => git.update(cx, |git, cx| git.pull(cx)),
                            QuickKind::Unavailable => {}
                        }),
                );
            }
            menu.separator()
                .entry("Refresh status", None, move |_, cx| {
                    git.update(cx, |git, cx| git.refresh(cx))
                })
        });
        self.menu
            .deploy(menu_position(event, window), menu, window, cx, |this| {
                &mut this.menu
            });
    }

    fn deploy_editor_menu(
        &mut self,
        event: &ClickEvent,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let editors = self.editors.read(cx);
        let available = editors.available().to_vec();
        let preferred = editors.preferred();
        let git = self.git.clone();
        // mustr's own editor, when the checkout is on this machine (`edit_area.rs`).
        let here = git
            .read(cx)
            .checkout()
            .and_then(|checkout| crate::edit_area::local_folder(&checkout.cwd, cx));
        let menu = ContextMenu::build(window, cx, move |mut menu, _, _| {
            if let Some(path) = here {
                menu = menu
                    .entry("Open in mustr's editor", None, move |window, cx| {
                        window.dispatch_action(
                            Box::new(crate::edit_area::OpenFolderInEdit { path: path.clone() }),
                            cx,
                        )
                    })
                    .separator();
            }
            if available.is_empty() {
                return menu.label("T3 found no editors");
            }
            for editor in available {
                let git = git.clone();
                let label = editor_label(&editor);
                let checked = preferred.as_ref() == Some(&editor);
                menu = menu.toggleable_entry(
                    label,
                    checked,
                    IconPosition::Start,
                    None,
                    move |_, cx| {
                        let editor = editor.clone();
                        git.update(cx, |git, cx| git.open_in_editor(Some(editor), cx))
                    },
                );
            }
            menu
        });
        self.menu
            .deploy(menu_position(event, window), menu, window, cx, |this| {
                &mut this.menu
            });
    }

    fn render_notice(&self, cx: &mut Context<Self>) -> Option<AnyElement> {
        let git = self.git.read(cx);
        let palette = palette(cx);
        let error_color = cx.theme().status().error;
        if self.placement == ControlsPlacement::Toolbar && git.run().is_some() {
            // The progress row says how the action went.
            return None;
        }
        let (label, color, tooltip) = match git.notice()? {
            Notice::Error(error) => ("Failed".to_string(), error_color, error.clone()),
            Notice::Done(done) => (done.to_string(), palette.ok.hsla(), done.clone()),
        };
        let git = self.git.clone();
        Some(
            Chip::new("thread-git-notice", label)
                .dot(color)
                .tooltip(tooltip)
                .on_click(move |_, _, cx| git.update(cx, |git, cx| git.dismiss(cx)))
                .into_any_element(),
        )
    }
}

impl Render for ThreadGitControls {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let git = self.git.read(cx);
        let has_checkout = git.checkout().is_some();
        let quick = git.quick_action();
        let blocked = git.write_blocked_reason();
        let running_label = git
            .run()
            .filter(|run| run.is_running())
            .map(|run| {
                run.label.clone().unwrap_or_else(|| {
                    format!("{}…", action_label(&run.action, git.change_request()))
                })
            })
            .or_else(|| git.busy_label().map(ToString::to_string));
        let editors = self.editors.read(cx);
        let preferred = editors.preferred();
        let open_label = match &preferred {
            Some(editor) => format!("Open in {}", editor_label(editor)),
            None => "Open in".to_string(),
        };
        let main_disabled = quick.kind == QuickKind::Unavailable || blocked.is_some();
        let main_tooltip =
            blocked
                .or_else(|| quick.hint.clone())
                .unwrap_or_else(|| match &quick.kind {
                    QuickKind::Run(action) if commits(action) => {
                        "T3 writes the commit message and the pull request".to_string()
                    }
                    _ => quick.label.clone(),
                });
        let notice = self.render_notice(cx);
        let this = cx.weak_entity();
        let menu_this = this.clone();
        let menu_open = self.menu.is_open();
        let editor_this = this.clone();
        let git = self.git.clone();

        let parts = self.parts;
        let open_in = (has_checkout && parts != ControlsParts::GitAction).then(|| {
            SplitButton::new("thread-open-in", open_label)
                .disabled(preferred.is_none())
                .tooltip(
                    preferred
                        .as_ref()
                        .map_or("T3 found no editors".to_string(), |_| {
                            "Open the thread's checkout in an editor".to_string()
                        }),
                )
                .menu_tooltip("Choose an editor")
                .menu_open(menu_open)
                .on_click(move |_, _, cx| git.update(cx, |git, cx| git.open_in_editor(None, cx)))
                .on_menu_click(move |event, window, cx| {
                    editor_this
                        .update(cx, |this, cx| this.deploy_editor_menu(event, window, cx))
                        .ok();
                })
                .into_any_element()
        });
        let action = (parts != ControlsParts::OpenIn).then(|| {
            SplitButton::new(
                "thread-git-action",
                running_label.unwrap_or_else(|| quick.label.clone()),
            )
            .style(if main_disabled {
                ButtonStyle::Default
            } else {
                ButtonStyle::Primary
            })
            .disabled(!has_checkout || (main_disabled && self.git.read(cx).is_busy()))
            .tooltip(main_tooltip)
            .menu_tooltip("More git actions")
            .menu_open(menu_open)
            .on_click(move |_, window, cx| {
                if main_disabled {
                    return;
                }
                this.update(cx, |this, cx| this.run_quick_action(window, cx))
                    .ok();
            })
            .on_menu_click(move |event, window, cx| {
                menu_this
                    .update(cx, |this, cx| this.deploy_commit_menu(event, window, cx))
                    .ok();
            })
            .into_any_element()
        });
        let notice = notice.filter(|_| parts != ControlsParts::OpenIn);
        let gap = scaled(px(6.));
        if parts != ControlsParts::Both {
            return h_flex()
                .id("thread-git-controls")
                .flex_none()
                .gap(gap)
                .children(notice)
                .children(open_in)
                .children(action)
                .children(self.menu.render())
                .into_any_element();
        }

        // Both, in a review's header: with no room for Open in, its editors go into Commit ▾'s
        // menu, and the notice goes before them.
        let children = [
            (CONTROLS_NOTICE, 2, notice),
            (CONTROLS_OPEN_IN, 1, open_in),
            (CONTROLS_ACTION, overflow::ALWAYS, action),
        ]
        .into_iter()
        .filter_map(|(key, priority, child)| Some((OverflowItem::new(key, priority), child?)))
        .collect::<Vec<_>>();
        let items = children
            .iter()
            .map(|(item, _)| item.clone())
            .collect::<Vec<_>>();
        let gap_pixels = gap.to_pixels(window.rem_size());
        let visible = self.overflow.visible(&items, gap_pixels, px(0.));
        self.open_in_hidden = items
            .iter()
            .zip(&visible)
            .any(|(item, visible)| item.key == CONTROLS_OPEN_IN && !visible);
        let natural = self.overflow.natural_width(&items, gap_pixels);
        let min = self.overflow.min_width(&items, gap_pixels, px(0.));
        let children = children
            .into_iter()
            .zip(visible)
            .filter(|(_, visible)| *visible)
            .map(|((item, child), _)| (Some(item.key), child))
            .chain(self.menu.render().map(|menu| (None, menu)));
        self.overflow
            .render(children, window)
            .gap(gap)
            .map(|row| match natural {
                Some(natural) => row.w(natural),
                None => row.w_auto(),
            })
            .when_some(min, |row, min| row.min_w(min))
            .flex_shrink_1()
            .into_any_element()
    }
}

const CONTROLS_NOTICE: &str = "thread-git-notice";
const CONTROLS_OPEN_IN: &str = "thread-open-in";
const CONTROLS_ACTION: &str = "thread-git-action";

/// A running or finished action's steps, under the thread toolbar: `✓ Commit › Push… › PR`,
/// with the error under the step that failed.
pub struct GitProgressRow {
    git: Entity<ThreadGit>,
    on_open_pull_request: Option<Arc<dyn Fn(String, i64, &mut Window, &mut App)>>,
    _subscription: Subscription,
}

impl GitProgressRow {
    pub fn new(
        git: Entity<ThreadGit>,
        on_open_pull_request: Option<Arc<dyn Fn(String, i64, &mut Window, &mut App)>>,
        cx: &mut Context<Self>,
    ) -> Self {
        Self {
            _subscription: cx.observe(&git, |_, _, cx| cx.notify()),
            git,
            on_open_pull_request,
        }
    }

    pub fn is_visible(&self, cx: &App) -> bool {
        self.git.read(cx).run().is_some()
    }
}

impl Render for GitProgressRow {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let git = self.git.read(cx);
        let Some(run) = git.run().cloned() else {
            return div().into_any_element();
        };
        let pr = git.change_request();
        let palette = palette(cx);
        let status = cx.theme().status();
        let mut steps = h_flex().gap(scaled(px(6.))).flex_wrap();
        for (index, (phase, state)) in run.steps().into_iter().enumerate() {
            if index > 0 {
                steps = steps.child(Label::new("›").size(LabelSize::Small).color(Color::Muted));
            }
            let (icon, color) = match state {
                StepState::Done => (IconName::Check, palette.ok.hsla()),
                StepState::Running => (IconName::ArrowCircle, palette.accent.hsla()),
                StepState::Failed => (IconName::XCircle, status.error),
                StepState::Pending => (IconName::Circle, palette.text_muted.hsla()),
            };
            steps = steps.child(
                h_flex()
                    .gap_1()
                    .child(
                        Icon::new(icon)
                            .size(IconSize::XSmall)
                            .color(Color::Custom(color)),
                    )
                    .child(
                        Label::new(phase_label(&phase, pr))
                            .size(LabelSize::Small)
                            .color(if state == StepState::Pending {
                                Color::Muted
                            } else {
                                Color::Default
                            }),
                    ),
            );
        }
        let detail: Option<(SharedString, Color)> = match &run.outcome {
            None => run
                .output
                .clone()
                .or_else(|| run.label.clone())
                .map(|text| (text.into(), Color::Muted)),
            Some(RunOutcome::Failed { message, .. }) => {
                Some((message.clone().into(), Color::Error))
            }
            Some(RunOutcome::Finished(result)) => Some((
                match &result.toast.description {
                    Some(description) => format!("{} · {description}", result.toast.title),
                    None => result.toast.title.clone(),
                }
                .into(),
                Color::Success,
            )),
        };
        let open_pr = match &run.outcome {
            Some(RunOutcome::Finished(result)) => result_pull_request(result),
            _ => None,
        };
        let cta = match &run.outcome {
            Some(RunOutcome::Finished(result)) => match &result.toast.cta {
                GitRunStackedActionResultToastCta::RunAction(cta) => {
                    Some((cta.label.clone(), cta.action.kind.clone()))
                }
                _ => None,
            },
            _ => None,
        };
        let git = self.git.clone();
        let cta_git = self.git.clone();
        let on_open = self.on_open_pull_request.clone();

        h_flex()
            .id("thread-git-progress")
            .w_full()
            .gap(scaled(px(10.)))
            .pt(scaled(px(6.)))
            .mt(scaled(px(2.)))
            .border_t(tokens::BORDER_WIDTH)
            .border_color(palette.border.hsla())
            .child(
                Label::new(action_label(&run.action, pr))
                    .size(LabelSize::Small)
                    .weight(gpui::FontWeight::SEMIBOLD),
            )
            .child(steps)
            .child(
                div()
                    .id("thread-git-progress-detail")
                    .flex_1()
                    .min_w_0()
                    .when_some(detail.clone(), |this, (text, _)| {
                        this.tooltip(Tooltip::text(text))
                    })
                    .children(detail.map(|(text, color)| {
                        Label::new(text)
                            .size(LabelSize::Small)
                            .color(color)
                            .truncate()
                    })),
            )
            .when_some(open_pr.zip(on_open), |this, ((url, number), on_open)| {
                this.child(
                    TextButton::new("thread-git-open-pr", format!("Review {pr} #{number}"))
                        .on_click(move |_, window, cx| on_open(url.clone(), number, window, cx)),
                )
            })
            .when_some(cta, |this, (label, action)| {
                this.child(
                    TextButton::new("thread-git-cta", label).on_click(move |_, _, cx| {
                        cta_git.update(cx, |git, cx| {
                            git.dismiss(cx);
                            git.run_action(action.clone(), ActionOptions::default(), cx)
                        })
                    }),
                )
            })
            .when(!run.is_running(), |this| {
                this.child(
                    IconButton::new("thread-git-progress-dismiss", IconName::Close)
                        .icon_size(IconSize::XSmall)
                        .tooltip(Tooltip::text("Dismiss"))
                        .on_click(move |_, _, cx| git.update(cx, |git, cx| git.dismiss(cx))),
                )
            })
            .into_any_element()
    }
}

// ---------------------------------------------------------------------------------------------
// The action dialog: a commit message, and the default-branch question

/// Asks for a commit message (T3 writes one if it's left empty), and before pushing to the
/// default branch, whether to stay on it or let T3 start a feature branch.
pub struct GitActionDialog {
    git: Entity<ThreadGit>,
    action: GitStackedAction,
    message: Option<Entity<Editor>>,
    /// The default branch the action would push to.
    default_branch: Option<String>,
    focus_handle: FocusHandle,
}

impl GitActionDialog {
    fn new(
        git: Entity<ThreadGit>,
        action: GitStackedAction,
        asks_for_message: bool,
        default_branch: Option<String>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        let message = (asks_for_message && commits(&action)).then(|| {
            cx.new(|cx| {
                let mut editor = Editor::auto_height(3, 10, window, cx);
                editor.set_placeholder_text(
                    "Leave empty and T3 writes the commit message",
                    window,
                    cx,
                );
                editor
            })
        });
        let focus_handle = match &message {
            Some(message) => message.focus_handle(cx),
            None => cx.focus_handle(),
        };
        Self {
            git,
            action,
            message,
            default_branch,
            focus_handle,
        }
    }

    fn confirm(&mut self, feature_branch: bool, cx: &mut Context<Self>) {
        let commit_message = self
            .message
            .as_ref()
            .map(|message| message.read(cx).text(cx));
        let action = self.action.clone();
        self.git.update(cx, |git, cx| {
            git.run_action(
                action,
                ActionOptions {
                    commit_message,
                    feature_branch,
                },
                cx,
            )
        });
        cx.emit(DismissEvent);
    }

    fn title(&self, pr: &str) -> String {
        let label = action_label(&self.action, pr);
        match &self.default_branch {
            Some(branch) => format!("{label} on {branch}?"),
            None if self.action == GitStackedAction::Commit => "Commit changes".to_string(),
            None => label,
        }
    }
}

impl EventEmitter<DismissEvent> for GitActionDialog {}

impl ModalView for GitActionDialog {}

impl Focusable for GitActionDialog {
    fn focus_handle(&self, _cx: &App) -> FocusHandle {
        self.focus_handle.clone()
    }
}

impl Render for GitActionDialog {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let palette = palette(cx);
        let git = self.git.read(cx);
        let pr = git.change_request();
        let status = git.status().cloned().unwrap_or_default();
        let branch = status.branch().unwrap_or("this branch").to_string();
        let files = status.changed_files();
        let label = action_label(&self.action, pr);
        let can_branch = status.is_dirty();
        let details = match files {
            0 => format!("On {branch}"),
            1 => format!("1 changed file on {branch}"),
            files => format!("{files} changed files on {branch}"),
        };
        let warning = self.default_branch.as_ref().map(|default| {
            format!(
                "{default} is the default branch. Continue on it, or let T3 start a feature \
                 branch and run the same action there."
            )
        });

        v_flex()
            .key_context("GitActionDialog")
            .on_action(cx.listener(|_, _: &menu::Cancel, _, cx| cx.emit(DismissEvent)))
            .w(tokens::modal_width(rems(32.), window))
            .p_4()
            .gap_3()
            .rounded(scaled(tokens::CORNER_RADIUS))
            .border(tokens::BORDER_WIDTH)
            .border_color(palette.border.hsla())
            .bg(palette.surface.hsla())
            .shadow_lg()
            .child(
                v_flex()
                    .gap_0p5()
                    .child(
                        Label::new(self.title(pr))
                            .size(LabelSize::Large)
                            .weight(gpui::FontWeight::SEMIBOLD),
                    )
                    .child(
                        Label::new(details)
                            .size(LabelSize::Small)
                            .color(Color::Muted),
                    ),
            )
            .when_some(self.message.clone(), |this, message| {
                this.child(
                    div()
                        .px_2()
                        .py_1p5()
                        .rounded(scaled(tokens::CONTROL_RADIUS))
                        .border(tokens::BORDER_WIDTH)
                        .border_color(palette.border.hsla())
                        .bg(palette.background.hsla())
                        .child(message),
                )
            })
            .when_some(warning, |this, warning| {
                this.child(
                    h_flex()
                        .gap_1p5()
                        .items_start()
                        .child(
                            Icon::new(IconName::Warning)
                                .size(IconSize::Small)
                                .color(Color::Warning),
                        )
                        .child(Label::new(warning).size(LabelSize::Small)),
                )
            })
            .child(
                h_flex()
                    .justify_end()
                    .gap_2()
                    .child(
                        TextButton::new("git-dialog-cancel", "Cancel")
                            .on_click(cx.listener(|_, _, _, cx| cx.emit(DismissEvent))),
                    )
                    .when(self.default_branch.is_some(), |this| {
                        this.child(
                            TextButton::new("git-dialog-feature-branch", "On a new branch")
                                .disabled(!can_branch)
                                .tooltip(if can_branch {
                                    "T3 names a branch after the changes".to_string()
                                } else {
                                    "There's nothing to commit, so T3 can't name a branch"
                                        .to_string()
                                })
                                .on_click(cx.listener(|this, _, _, cx| this.confirm(true, cx))),
                        )
                    })
                    .child(
                        TextButton::new(
                            "git-dialog-confirm",
                            match &self.default_branch {
                                Some(default) => format!("{label} on {default}"),
                                None => label,
                            },
                        )
                        .style(ButtonStyle::Primary)
                        .on_click(cx.listener(|this, _, _, cx| this.confirm(false, cx))),
                    ),
            )
    }
}

// ---------------------------------------------------------------------------------------------
// The branch picker

#[derive(Clone, Debug, PartialEq)]
enum BranchEntry {
    Ref(VcsRef),
    Create(String),
    /// A new branch in a new worktree, which the thread moves to.
    CreateInWorktree(String),
}

/// Picks a branch to switch the checkout to, or names a new one (T3's branch picker). Picking
/// a branch checked out in a worktree moves the thread there, and a thread in the project's
/// checkout can move to a new worktree on a new branch ([`ThreadGit::select_ref`],
/// [`ThreadGit::move_to_new_worktree`]).
pub struct BranchPickerDelegate {
    git: Entity<ThreadGit>,
    /// The thread's checkout when the picker opened.
    checkout: Option<Checkout>,
    entries: Vec<BranchEntry>,
    selected: usize,
    error: Option<SharedString>,
}

impl BranchPickerDelegate {
    pub fn new(git: Entity<ThreadGit>, cx: &App) -> Self {
        let checkout = git.read(cx).checkout().cloned();
        Self {
            git,
            checkout,
            entries: Vec::new(),
            selected: 0,
            error: None,
        }
    }

    /// Whether `entry` can be picked: not the branch the thread is on.
    fn selectable(&self, entry: &BranchEntry) -> bool {
        match entry {
            BranchEntry::Ref(git_ref) => {
                !git_ref.current
                    && git_ref.worktree_path.as_deref()
                        != self.checkout.as_ref().map(|checkout| checkout.cwd.as_str())
            }
            BranchEntry::Create(_) | BranchEntry::CreateInWorktree(_) => true,
        }
    }

    /// What picking `git_ref` does besides switching branches, if it moves the thread.
    fn moves_thread(&self, git_ref: &VcsRef) -> Option<&'static str> {
        let checkout = self.checkout.as_ref()?;
        let project_root = checkout.project_root.as_deref().unwrap_or(&checkout.cwd);
        let target =
            branch_selection_target(project_root, checkout.worktree_path.as_deref(), git_ref);
        if target.worktree_path == checkout.worktree_path {
            return None;
        }
        Some(match target.worktree_path {
            Some(_) => "moves the thread to its worktree",
            None => "moves the thread to the project's checkout",
        })
    }

    /// Opens the picker over `workspace`.
    pub fn open(
        git: Entity<ThreadGit>,
        workspace: &mut Workspace,
        window: &mut Window,
        cx: &mut Context<Workspace>,
    ) {
        git.update(cx, |git, cx| git.refresh(cx));
        workspace.toggle_modal(window, cx, |window, cx| {
            let delegate = Self::new(git, cx);
            Picker::uniform_list(delegate, window, cx)
                .initial_width(tokens::modal_width(rems(32.), window))
        });
    }
}

impl PickerDelegate for BranchPickerDelegate {
    type ListItem = ListItem;

    fn name() -> &'static str {
        "thread branch picker"
    }

    fn match_count(&self) -> usize {
        self.entries.len()
    }

    fn selected_index(&self) -> usize {
        self.selected
    }

    fn set_selected_index(&mut self, ix: usize, _: &mut Window, _: &mut Context<Picker<Self>>) {
        self.selected = ix;
    }

    fn can_select(&self, ix: usize, _: &mut Window, _: &mut Context<Picker<Self>>) -> bool {
        self.entries
            .get(ix)
            .is_some_and(|entry| self.selectable(entry))
    }

    fn placeholder_text(&self, _: &mut Window, _: &mut App) -> Arc<str> {
        "Switch to a branch, or name a new one…".into()
    }

    fn no_matches_text(&self, _: &mut Window, _: &mut App) -> Option<SharedString> {
        Some(self.error.clone().unwrap_or_else(|| "No branches".into()))
    }

    fn update_matches(
        &mut self,
        query: String,
        window: &mut Window,
        cx: &mut Context<Picker<Self>>,
    ) -> Task<()> {
        let Some(list) = self.git.read(cx).list_refs(&query) else {
            self.entries.clear();
            self.error = Some("This thread has no checkout".into());
            return Task::ready(());
        };
        cx.spawn_in(window, async move |picker, cx| {
            let refs = list.await;
            picker
                .update(cx, |picker, cx| {
                    let delegate = &mut picker.delegate;
                    let query = query.trim();
                    match refs {
                        Ok(refs) => {
                            delegate.error = None;
                            let exact = refs.iter().any(|git_ref| git_ref.name == query);
                            delegate.entries = refs.into_iter().map(BranchEntry::Ref).collect();
                            if !query.is_empty() && !exact {
                                delegate
                                    .entries
                                    .push(BranchEntry::Create(query.to_string()));
                                let local = delegate
                                    .checkout
                                    .as_ref()
                                    .is_some_and(|checkout| checkout.worktree_path.is_none());
                                if local {
                                    delegate
                                        .entries
                                        .push(BranchEntry::CreateInWorktree(query.to_string()));
                                }
                            }
                        }
                        Err(error) => {
                            delegate.entries.clear();
                            delegate.error = Some(format!("{error:#}").into());
                        }
                    }
                    delegate.selected = delegate
                        .entries
                        .iter()
                        .position(|entry| delegate.selectable(entry))
                        .unwrap_or(0);
                    cx.notify();
                })
                .ok();
        })
    }

    fn confirm(&mut self, _secondary: bool, _: &mut Window, cx: &mut Context<Picker<Self>>) {
        let Some(entry) = self
            .entries
            .get(self.selected)
            .cloned()
            .filter(|entry| self.selectable(entry))
        else {
            return;
        };
        match entry {
            BranchEntry::Ref(git_ref) => {
                self.git.update(cx, |git, cx| git.select_ref(git_ref, cx));
            }
            BranchEntry::Create(name) => {
                self.git.update(cx, |git, cx| git.create_ref(name, cx));
            }
            BranchEntry::CreateInWorktree(name) => {
                self.git
                    .update(cx, |git, cx| git.move_to_new_worktree(name, cx));
            }
        }
        cx.emit(DismissEvent);
    }

    fn dismissed(&mut self, _: &mut Window, cx: &mut Context<Picker<Self>>) {
        cx.emit(DismissEvent);
    }

    fn render_match(
        &self,
        ix: usize,
        selected: bool,
        _: &mut Window,
        _: &mut Context<Picker<Self>>,
    ) -> Option<Self::ListItem> {
        let entry = self.entries.get(ix)?;
        let (icon, label, badge) = match entry {
            BranchEntry::Ref(git_ref) => {
                let kind = if git_ref.current {
                    Some("current")
                } else if git_ref.worktree_path.is_some() {
                    Some("worktree")
                } else if git_ref.is_default {
                    Some("default")
                } else if git_ref.is_remote == Some(true) {
                    Some("remote")
                } else {
                    None
                };
                let badge = match (kind, self.moves_thread(git_ref)) {
                    (Some(kind), Some(moves)) => Some(format!("{kind} · {moves}")),
                    (kind, moves) => kind.or(moves).map(str::to_string),
                };
                (IconName::GitBranch, git_ref.name.clone(), badge)
            }
            BranchEntry::Create(name) => (IconName::Plus, format!("Create branch “{name}”"), None),
            BranchEntry::CreateInWorktree(name) => (
                IconName::GitBranchPlus,
                format!("Create “{name}” in a new worktree"),
                Some("moves the thread there".to_string()),
            ),
        };
        let muted = !self.selectable(entry);
        Some(
            ListItem::new(ix)
                .inset(true)
                .spacing(ListItemSpacing::Sparse)
                .toggle_state(selected)
                .start_slot(Icon::new(icon).size(IconSize::Small).color(Color::Muted))
                .child(
                    h_flex()
                        .w_full()
                        .gap_2()
                        .child(Label::new(label).truncate().color(if muted {
                            Color::Muted
                        } else {
                            Color::Default
                        }))
                        .children(badge.map(|badge| {
                            Label::new(badge)
                                .size(LabelSize::XSmall)
                                .color(Color::Muted)
                        })),
                ),
        )
    }
}

#[cfg(test)]
#[path = "thread_git_tests.rs"]
pub(crate) mod tests;

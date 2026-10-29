//! The thread toolbar: what ships an agent thread's work, above its timeline (`mustr-docs/PARITY.md`
//! §2.9–§2.12).
//!
//! - The checkout's branch, which opens the branch picker, and its git status
//!   (`subscribeVcsStatus`, through [`ThreadGit`]).
//! - The thread's pull request, `● #80 ✗ ▾`: its state as a dot and its checks as an icon
//!   (`pullRequests.summary`, `pullRequests.checks`), the rest in its tooltip. Its menu reviews
//!   it, opens it on the web, marks it ready (`pullRequests.runAction`), watches it, links and
//!   unlinks it (`thread.pull-request.link`, `unlink`, `watch`), and lists the thread's other
//!   linked pull requests.
//! - The thread's local dev servers (`local_servers.rs`), each opening in the system browser.
//! - **Review changes** (handles `thread_chat::ReviewThreadChanges`), **Merge back** on a fork
//!   (`merge_back.rs`), and **Commit ▾**, with a row of a running action's steps; then two icon
//!   buttons, **Terminal** (dispatches `thread_chat::OpenThreadTerminal`, which the terminal panel
//!   handles) and **Open in ▾** with the editor's icon (handles `thread_chat::OpenInEditor`), and
//!   "…" for what doesn't fit.
//!
//! The left side shows, the right side acts: the branch and the pull request are quiet buttons
//! with muted details, the actions outlined buttons, all one height.
//!
//! The pull request a thread shows is chosen as T3's `useLinkedThreadPullRequest` does: from the
//! thread's linked pull requests when T3 sends them, then its `linkedPullRequest`, then the pull
//! request of its branch.
//!
//! The toolbar's actions work on the thread in focus from the keyboard and the command palette
//! too, which offers them only while a thread is open: [`ReviewChanges`] (`secondary-d`, T3's
//! `mod+d`), [`OpenThreadInEditor`] (`secondary-o`), [`SwitchThreadBranch`] (`secondary-shift-g`),
//! [`CommitPushAndPr`], and [`LinkThreadPullRequest`].

use crate::{
    agent_review,
    agent_threads::{AgentThread, agent_threads},
    local_servers::{self, LocalServers},
    merge_back::MergeBack,
    pull_request_review::{self, checks_summary},
    thread_git::{
        BranchPickerDelegate, ControlsParts, ControlsPlacement, Editors, GitProgressRow, MenuSlot,
        ThreadGit, ThreadGitControls, editor_choices, menu_position, open_in_entries,
    },
};
use anyhow::{Context as _, Result, anyhow};
use editor::Editor;
use gpui::{
    AnyElement, App, AppContext as _, ClickEvent, Context, DismissEvent, Entity, EventEmitter,
    FocusHandle, Focusable, Hsla, IntoElement, KeyBinding, ParentElement as _, Render,
    SharedString, Styled as _, Subscription, Task, TaskExt as _, WeakEntity, Window, actions, div,
    px, rems,
};
use mustr_ui::{
    ButtonStyle, IconOnlyButton, MustrIcon, OverflowItem, OverflowRow, QuietButton, TextButton,
    overflow, palette, tokens, tokens::scaled,
};
use serde_json::json;
use std::sync::Arc;
use t3_client::{
    PullRequestRef,
    t3_api::{
        rpc,
        tagged::OrchestrationV2Command,
        types::{
            GitPullRequestRefInput, OrchestrationV2CommandThreadPullRequestLink,
            OrchestrationV2CommandThreadPullRequestLinkType,
            OrchestrationV2CommandThreadPullRequestUnlink,
            OrchestrationV2CommandThreadPullRequestUnlinkType,
            OrchestrationV2CommandThreadPullRequestWatch,
            OrchestrationV2CommandThreadPullRequestWatchType, OrchestrationV2ThreadShell,
            PullRequestAction, PullRequestChecks, PullRequestChecksState, PullRequestState,
            PullRequestSummary, ThreadPullRequestLink, ThreadPullRequestLinkSource,
        },
    },
};
use thread_chat::{
    Access, OpenInEditor, OpenThreadTerminal, ReviewThreadChanges, Shell, ThreadBackendExt as _,
    ThreadShells, ThreadView,
};
use ui::{
    Color, ContextMenu, ContextMenuEntry, Icon, IconName, IconSize, Label, LabelCommon as _,
    LabelSize, Tooltip, prelude::*,
};
use workspace::{
    ItemHandle, ModalView, Toast, ToolbarItemEvent, ToolbarItemLocation, ToolbarItemView,
    Workspace, notifications::NotificationId,
};

actions!(
    thread_chat,
    [
        /// Reviews the changes of the agent thread in focus: its branch, or its latest turn.
        ReviewChanges,
        /// Opens the agent thread's folder in your editor, the one Open in last used.
        OpenThreadInEditor,
        /// Switches the agent thread's checkout to another branch, or creates one.
        SwitchThreadBranch,
        /// Commits the agent thread's changes, pushes them, and creates the pull request, as far
        /// as its branch needs: the thread toolbar's Commit button.
        CommitPushAndPr,
        /// Links a pull request to the agent thread in focus.
        LinkThreadPullRequest,
    ]
);

/// Handles the thread actions this toolbar owns on every workspace. Call once at startup.
pub fn init(cx: &mut App) {
    cx.observe_new(|workspace: &mut Workspace, _, _| {
        workspace.register_action(|workspace, action: &ReviewThreadChanges, window, cx| {
            review_thread_changes(workspace, &action.thread_id, window, cx);
        });
        workspace.register_action(|_, action: &OpenInEditor, _, cx| {
            let git = ThreadGit::for_thread(&action.thread_id, cx);
            git.update(cx, |git, cx| git.open_in_editor(None, cx));
        });
        // Offered only while a thread is open, so the palette lists them only then.
        workspace.register_action_renderer(|div, workspace, _, cx| {
            match focused_thread(workspace, cx) {
                Some(thread_id) => thread_actions(div, thread_id, cx),
                None => div,
            }
        });
    })
    .detach();
}

/// The toolbar's keys, as T3 binds them (`packages/shared/src/keybindings.ts`). The composer's
/// own editor binds `secondary-d` (select next), so they're bound for editors in a thread view
/// too, which mustr binds after Zed's.
pub fn key_bindings() -> Vec<KeyBinding> {
    ["ThreadView", "ThreadView > Editor"]
        .into_iter()
        .flat_map(|context| {
            [
                KeyBinding::new("secondary-d", ReviewChanges, Some(context)),
                KeyBinding::new("secondary-o", OpenThreadInEditor, Some(context)),
                KeyBinding::new("secondary-shift-g", SwitchThreadBranch, Some(context)),
            ]
        })
        .collect()
}

/// The thread of the agent thread view in focus, once it isn't a draft.
fn focused_thread(workspace: &Workspace, cx: &App) -> Option<String> {
    let view = workspace.active_item(cx)?.downcast::<ThreadView>()?;
    view.read(cx).thread_id().map(str::to_string)
}

fn thread_actions(div: gpui::Div, thread_id: String, cx: &mut Context<Workspace>) -> gpui::Div {
    let review = thread_id.clone();
    let editor = thread_id.clone();
    let branch = thread_id.clone();
    let commit = thread_id.clone();
    div.on_action(
        cx.listener(move |workspace, _: &ReviewChanges, window, cx| {
            review_thread_changes(workspace, &review, window, cx);
        }),
    )
    .on_action(cx.listener(move |_, _: &OpenThreadInEditor, _, cx| {
        let git = ThreadGit::for_thread(&editor, cx);
        git.update(cx, |git, cx| git.open_in_editor(None, cx));
    }))
    .on_action(
        cx.listener(move |workspace, _: &SwitchThreadBranch, window, cx| {
            let git = ThreadGit::for_thread(&branch, cx);
            BranchPickerDelegate::open(git, workspace, window, cx);
        }),
    )
    .on_action(cx.listener(move |_, _: &CommitPushAndPr, window, cx| {
        let git = ThreadGit::for_thread(&commit, cx);
        let workspace = cx.weak_entity();
        let controls =
            cx.new(|cx| ThreadGitControls::new(git, workspace, ControlsPlacement::Toolbar, cx));
        // Its dialog opens on the workspace, which this handler is updating.
        window.defer(cx, move |window, cx| {
            controls.update(cx, |controls, cx| controls.run_quick_action(window, cx));
        });
    }))
    .on_action(
        cx.listener(move |workspace, _: &LinkThreadPullRequest, window, cx| {
            let git = ThreadGit::for_thread(&thread_id, cx);
            let thread_id = thread_id.clone();
            workspace.toggle_modal(window, cx, |window, cx| {
                LinkPullRequestDialog::new(thread_id, git, window, cx)
            });
        }),
    )
}

// ---------------------------------------------------------------------------------------------
// Review changes

/// The thread as the review knows it, from T3's shell.
fn agent_thread(shell: &Shell, thread_id: &str) -> Option<AgentThread> {
    let thread = shell.threads.get(thread_id)?;
    let project = shell.projects.get(&thread.project_id);
    let snapshot = json!({
        "threads": [thread],
        "projects": project.into_iter().collect::<Vec<_>>(),
    });
    agent_threads(&snapshot).into_iter().next()
}

/// Opens the review of the thread's work: its whole changes, or its latest turn when the
/// branch has none.
fn review_thread_changes(
    workspace: &mut Workspace,
    thread_id: &str,
    window: &mut Window,
    cx: &mut Context<Workspace>,
) {
    let thread = ThreadShells::global(cx)
        .read(cx)
        .shell()
        .and_then(|shell| agent_thread(shell, thread_id));
    let Some(thread) = thread else {
        show_error(
            workspace,
            thread_id,
            "T3 hasn't listed this thread yet, so its changes can't be reviewed",
            cx,
        );
        return;
    };
    let review = agent_review::review_thread_work(workspace, thread, window, cx);
    let thread_id = thread_id.to_string();
    cx.spawn_in(window, async move |workspace, cx| {
        if let Err(error) = review.await {
            log::error!("couldn't review the thread's changes: {error:#}");
            workspace.update(cx, |workspace, cx| {
                show_error(workspace, &thread_id, &format!("{error:#}"), cx)
            })?;
        }
        anyhow::Ok(())
    })
    .detach_and_log_err(cx);
}

fn show_error(workspace: &mut Workspace, key: &str, message: &str, cx: &mut Context<Workspace>) {
    let id = NotificationId::composite::<ThreadToolbar>(key.to_string());
    workspace.show_toast(Toast::new(id, message.to_string()), cx);
}

// ---------------------------------------------------------------------------------------------
// The thread's pull request

/// How the pull request came to be the thread's.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum PullRequestOrigin {
    /// One of the thread's linked pull requests, with who linked it.
    Linked(ThreadPullRequestLinkSource),
    /// The thread's `linkedPullRequest`.
    LinkedField,
    /// The pull request of the thread's branch.
    Branch,
}

/// The pull request a thread shows.
#[derive(Clone, Debug, PartialEq)]
pub struct ThreadPullRequest {
    pub reference: PullRequestRef,
    pub url: String,
    pub origin: PullRequestOrigin,
    pub watching: bool,
    /// What T3 last saw of it, for linked pull requests.
    pub state: Option<PullRequestState>,
    pub is_draft: bool,
    pub title: Option<String>,
    pub checks: Option<PullRequestChecksState>,
    /// The other pull requests linked to the thread, newest first (the toolbar used to show
    /// them as `+2`; its menu lists them now).
    pub others: Vec<OtherPullRequest>,
}

/// Another pull request linked to the thread, as T3 last saw it.
#[derive(Clone, Debug, PartialEq)]
pub struct OtherPullRequest {
    pub reference: PullRequestRef,
    pub title: Option<String>,
    pub state: Option<PullRequestState>,
}

impl OtherPullRequest {
    /// `#81 Retry webhooks (merged)`.
    pub fn label(&self) -> String {
        let mut label = format!("#{}", self.reference.number);
        if let Some(title) = self
            .title
            .as_deref()
            .filter(|title| !title.trim().is_empty())
        {
            label.push(' ');
            label.push_str(title.trim());
        }
        match &self.state {
            Some(PullRequestState::Merged) => label.push_str(" (merged)"),
            Some(PullRequestState::Closed) => label.push_str(" (closed)"),
            _ => {}
        }
        label
    }
}

impl ThreadPullRequest {
    pub fn is_linked(&self) -> bool {
        matches!(self.origin, PullRequestOrigin::Linked(_))
    }

    fn same_pull_request(&self, other: &Self) -> bool {
        self.reference == other.reference
    }
}

/// `github.com` from `https://github.com/acme/api/pull/7`.
fn url_host(url: &str) -> Option<String> {
    let rest = url.split_once("://").map_or(url, |(_, rest)| rest);
    let host = rest.split(['/', '?', '#']).next()?.trim();
    (!host.is_empty()).then(|| host.to_lowercase())
}

/// The host, repository, and number of a pull request URL: GitHub's `/pull/7`, GitLab's
/// `/-/merge_requests/7`, and Bitbucket's `/pull-requests/7`.
pub fn parse_pull_request_url(url: &str) -> Option<(String, String, i64)> {
    let url = url.trim();
    let (scheme, rest) = url.split_once("://")?;
    if scheme != "https" && scheme != "http" {
        return None;
    }
    let host = url_host(url)?;
    let path = rest.split(['?', '#']).next()?;
    let segments = path
        .split('/')
        .skip(1)
        .filter(|segment| !segment.is_empty())
        .collect::<Vec<_>>();
    let marker = segments.iter().position(|segment| {
        matches!(
            *segment,
            "pull" | "pulls" | "merge_requests" | "pull-requests"
        )
    })?;
    let number = segments
        .get(marker + 1)?
        .parse::<i64>()
        .ok()
        .filter(|n| *n > 0)?;
    let repository = segments[..marker]
        .iter()
        .filter(|segment| **segment != "-")
        .copied()
        .collect::<Vec<_>>()
        .join("/");
    (repository.contains('/')).then_some((host, repository, number))
}

/// The pull request the thread shows, as T3's `useLinkedThreadPullRequest` picks it.
pub fn thread_pull_request(thread: &OrchestrationV2ThreadShell) -> Option<ThreadPullRequest> {
    let links = thread
        .pull_requests
        .iter()
        .flatten()
        .filter(|link| link.source != ThreadPullRequestLinkSource::StackDismissed)
        .collect::<Vec<_>>();
    if !links.is_empty() {
        let updated = |link: &&t3_client::t3_api::types::ThreadPullRequestLink| {
            link.snapshot
                .as_ref()
                .and_then(|snapshot| snapshot.updated_at.clone())
                .unwrap_or_else(|| link.linked_at.clone())
        };
        let open = links
            .iter()
            .filter(|link| {
                link.snapshot
                    .as_ref()
                    .is_some_and(|snapshot| snapshot.state == PullRequestState::Open)
            })
            .collect::<Vec<_>>();
        let chosen = if open.is_empty() {
            links.iter().max_by_key(|link| updated(link))
        } else {
            open.into_iter().max_by_key(|link| updated(link))
        }?;
        let snapshot = chosen.snapshot.as_ref();
        let mut others = links
            .iter()
            .filter(|link| !std::ptr::eq(**link, *chosen))
            .collect::<Vec<_>>();
        others.sort_by_key(|link| std::cmp::Reverse(updated(link)));
        let others = others
            .into_iter()
            .map(|link| other_pull_request(link, &thread.project_id))
            .collect();
        return Some(ThreadPullRequest {
            reference: PullRequestRef {
                allow_stale: None,
                expected_account_id: None,
                host: Some(chosen.host.clone()),
                number: chosen.number,
                project_id: thread.project_id.clone(),
                repository: chosen.repository.clone(),
            },
            url: chosen.url.clone(),
            origin: PullRequestOrigin::Linked(chosen.source.clone()),
            watching: chosen.watch.is_some(),
            state: snapshot.map(|snapshot| snapshot.state.clone()),
            is_draft: snapshot.is_some_and(|snapshot| snapshot.is_draft),
            title: snapshot.map(|snapshot| snapshot.title.clone()),
            checks: snapshot.and_then(|snapshot| snapshot.checks_state.clone()),
            others,
        });
    }
    let (linked, origin) = match (&thread.linked_pull_request, &thread.branch_pull_request) {
        (Some(linked), _) => (linked, PullRequestOrigin::LinkedField),
        (None, Some(branch)) => (branch, PullRequestOrigin::Branch),
        (None, None) => return None,
    };
    Some(ThreadPullRequest {
        reference: PullRequestRef {
            allow_stale: None,
            expected_account_id: None,
            host: url_host(&linked.url),
            number: linked.number,
            project_id: linked.project_id.clone(),
            repository: linked.repository.clone(),
        },
        url: linked.url.clone(),
        origin,
        watching: false,
        state: None,
        is_draft: false,
        title: None,
        checks: None,
        others: Vec::new(),
    })
}

fn other_pull_request(link: &ThreadPullRequestLink, project_id: &str) -> OtherPullRequest {
    let snapshot = link.snapshot.as_ref();
    OtherPullRequest {
        reference: PullRequestRef {
            allow_stale: None,
            expected_account_id: None,
            host: Some(link.host.clone()),
            number: link.number,
            project_id: project_id.to_string(),
            repository: link.repository.clone(),
        },
        title: snapshot.map(|snapshot| snapshot.title.clone()),
        state: snapshot.map(|snapshot| snapshot.state.clone()),
    }
}

fn host_of(pull_request: &ThreadPullRequest) -> String {
    pull_request
        .reference
        .host
        .clone()
        .or_else(|| url_host(&pull_request.url))
        .unwrap_or_else(|| "github.com".to_string())
}

/// Links a pull request to the thread, as the user (`thread.pull-request.link`).
pub fn link_command(
    thread_id: &str,
    host: String,
    repository: String,
    number: i64,
    url: String,
    command_id: String,
) -> OrchestrationV2Command {
    OrchestrationV2Command::ThreadPullRequestLink(OrchestrationV2CommandThreadPullRequestLink {
        command_id,
        host,
        number,
        repository,
        repository_web_url: None,
        source: ThreadPullRequestLinkSource::Manual,
        thread_id: thread_id.to_string(),
        type_: OrchestrationV2CommandThreadPullRequestLinkType::ThreadPullRequestLink,
        url,
    })
}

pub fn unlink_command(
    thread_id: &str,
    pull_request: &ThreadPullRequest,
    command_id: String,
) -> OrchestrationV2Command {
    OrchestrationV2Command::ThreadPullRequestUnlink(OrchestrationV2CommandThreadPullRequestUnlink {
        command_id,
        host: host_of(pull_request),
        number: pull_request.reference.number,
        repository: pull_request.reference.repository.clone(),
        thread_id: thread_id.to_string(),
        type_: OrchestrationV2CommandThreadPullRequestUnlinkType::ThreadPullRequestUnlink,
    })
}

/// Starts or stops the agent reacting to the pull request's CI and comments.
pub fn watch_command(
    thread_id: &str,
    pull_request: &ThreadPullRequest,
    watching: bool,
    command_id: String,
) -> OrchestrationV2Command {
    OrchestrationV2Command::ThreadPullRequestWatch(OrchestrationV2CommandThreadPullRequestWatch {
        command_id,
        host: host_of(pull_request),
        link: None,
        number: pull_request.reference.number,
        repository: pull_request.reference.repository.clone(),
        thread_id: thread_id.to_string(),
        type_: OrchestrationV2CommandThreadPullRequestWatchType::ThreadPullRequestWatch,
        watching,
    })
}

/// The thread's pull request, with what `pullRequests.summary` and `pullRequests.checks` say.
pub struct PullRequestStatus {
    thread_id: String,
    /// The thread's project, for pull requests T3 just made from it.
    project_id: Option<String>,
    pull_request: Option<ThreadPullRequest>,
    summary: Option<PullRequestSummary>,
    checks: Option<PullRequestChecks>,
    error: Option<SharedString>,
    _shells: Subscription,
    _load: Option<Task<()>>,
    _tasks: Vec<Task<()>>,
}

impl PullRequestStatus {
    pub fn new(thread_id: String, cx: &mut Context<Self>) -> Self {
        let shells = ThreadShells::global(cx);
        let mut this = Self {
            thread_id,
            project_id: None,
            pull_request: None,
            summary: None,
            checks: None,
            error: None,
            _shells: cx.observe(&shells, |this, shells, cx| this.update(&shells, cx)),
            _load: None,
            _tasks: Vec::new(),
        };
        this.update(&shells, cx);
        this
    }

    pub fn pull_request(&self) -> Option<&ThreadPullRequest> {
        self.pull_request.as_ref()
    }

    fn update(&mut self, shells: &Entity<ThreadShells>, cx: &mut Context<Self>) {
        let thread = shells.read(cx).thread(&self.thread_id);
        self.project_id = thread.map(|thread| thread.project_id.clone());
        let pull_request = thread.and_then(thread_pull_request);
        if pull_request == self.pull_request {
            return;
        }
        let changed = match (&pull_request, &self.pull_request) {
            (Some(new), Some(old)) => !new.same_pull_request(old),
            _ => true,
        };
        self.pull_request = pull_request;
        if changed {
            self.summary = None;
            self.checks = None;
            self.load(cx);
        }
        cx.notify();
    }

    fn load(&mut self, cx: &mut Context<Self>) {
        let (Some(backend), Some(pull_request)) =
            (thread_chat::backend(cx), self.pull_request.clone())
        else {
            self._load = None;
            return;
        };
        let summary =
            backend.call::<rpc::PullRequestsSummary>(pull_request.reference.clone(), Access::Read);
        let checks = backend.call::<rpc::PullRequestsChecks>(pull_request.reference, Access::Read);
        self._load = Some(cx.spawn(async move |this, cx| {
            let (summary, checks) = futures::join!(summary, checks);
            this.update(cx, |this, cx| {
                match summary {
                    Ok(summary) => this.summary = Some(summary),
                    Err(error) => {
                        log::warn!("couldn't load the thread's pull request: {error:#}");
                        this.error = Some(format!("{error:#}").into());
                    }
                }
                match checks {
                    Ok(checks) => this.checks = checks.map(Into::into),
                    Err(error) => log::warn!("couldn't load the pull request's checks: {error:#}"),
                }
                cx.notify();
            })
            .ok();
        }));
    }

    fn state(&self) -> Option<(PullRequestState, bool)> {
        if let Some(summary) = &self.summary {
            return Some((summary.state.clone(), summary.is_draft.unwrap_or(false)));
        }
        let pull_request = self.pull_request.as_ref()?;
        Some((pull_request.state.clone()?, pull_request.is_draft))
    }

    fn title(&self) -> Option<String> {
        self.summary
            .as_ref()
            .map(|summary| summary.title.clone())
            .or_else(|| self.pull_request.as_ref()?.title.clone())
    }

    /// The checks' rollup, and a label such as `2 of 5 checks failing`.
    fn checks(&self) -> Option<(PullRequestChecksState, String)> {
        if let Some(checks) = &self.checks {
            let statuses = checks
                .checks
                .iter()
                .map(|check| check.status.as_str().to_string())
                .collect::<Vec<_>>();
            if let Some(summary) = checks_summary(&statuses) {
                let state = match summary.state {
                    review::ChecksState::Passing => PullRequestChecksState::Passing,
                    review::ChecksState::Failing => PullRequestChecksState::Failing,
                    review::ChecksState::Pending => PullRequestChecksState::Pending,
                };
                return Some((state, summary.label()));
            }
        }
        let state = self
            .summary
            .as_ref()
            .and_then(|summary| summary.checks_state.clone())
            .or_else(|| self.pull_request.as_ref()?.checks.clone())?;
        let label = match state {
            PullRequestChecksState::Passing => "Checks passing",
            PullRequestChecksState::Failing => "Checks failing",
            PullRequestChecksState::Pending => "Checks pending",
            PullRequestChecksState::Unrecognized(_) => return None,
        };
        Some((state, label.to_string()))
    }

    fn dispatch(
        &mut self,
        command: OrchestrationV2Command,
        what: &'static str,
        cx: &mut Context<Self>,
    ) {
        let Some(backend) = thread_chat::backend(cx) else {
            return;
        };
        let dispatch = backend.dispatch(command);
        self._tasks.push(cx.spawn(async move |this, cx| {
            if let Err(error) = dispatch.await {
                log::error!("couldn't {what}: {error:#}");
                this.update(cx, |this, cx| {
                    this.error = Some(format!("Couldn't {what}: {error:#}").into());
                    cx.notify();
                })
                .ok();
            }
        }));
    }

    pub fn unlink(&mut self, cx: &mut Context<Self>) {
        let Some(pull_request) = self
            .pull_request
            .clone()
            .filter(ThreadPullRequest::is_linked)
        else {
            return;
        };
        let command = unlink_command(
            &self.thread_id,
            &pull_request,
            thread_chat::commands::new_id(),
        );
        self.dispatch(command, "unlink the pull request", cx);
    }

    pub fn set_watching(&mut self, watching: bool, cx: &mut Context<Self>) {
        let Some(pull_request) = self.pull_request.clone() else {
            return;
        };
        let command = watch_command(
            &self.thread_id,
            &pull_request,
            watching,
            thread_chat::commands::new_id(),
        );
        self.dispatch(command, "watch the pull request", cx);
    }

    /// Marks the draft pull request ready for review (`pullRequests.runAction`), as its review's
    /// Ready for review does.
    pub fn mark_ready(&mut self, cx: &mut Context<Self>) {
        let (Some(backend), Some(pull_request)) =
            (thread_chat::backend(cx), self.pull_request.clone())
        else {
            return;
        };
        let call = backend.call::<rpc::PullRequestsRunAction>(
            pull_request_review::run_action_input(
                &pull_request.reference,
                PullRequestAction::Ready,
                None,
            ),
            Access::SourceControlWrite,
        );
        self._tasks.push(cx.spawn(async move |this, cx| {
            let result = call.await;
            this.update(cx, |this, cx| {
                match result {
                    Ok(()) => {
                        if let Some(summary) = &mut this.summary {
                            summary.is_draft = Some(false);
                        }
                        if let Some(pull_request) = &mut this.pull_request {
                            pull_request.is_draft = false;
                        }
                    }
                    Err(error) => {
                        log::error!("couldn't mark the pull request ready: {error:#}");
                        this.error = Some(format!("Couldn't mark it ready: {error:#}").into());
                    }
                }
                cx.notify();
            })
            .ok();
        }));
    }

    /// Links the pull request of the thread's branch, so it can be watched.
    pub fn link_shown(&mut self, cx: &mut Context<Self>) {
        let Some(pull_request) = self.pull_request.clone().filter(|pr| !pr.is_linked()) else {
            return;
        };
        let command = link_command(
            &self.thread_id,
            host_of(&pull_request),
            pull_request.reference.repository.clone(),
            pull_request.reference.number,
            pull_request.url.clone(),
            thread_chat::commands::new_id(),
        );
        self.dispatch(command, "link the pull request", cx);
    }
}

fn state_label(state: &PullRequestState, is_draft: bool) -> &'static str {
    match state {
        PullRequestState::Open if is_draft => "Draft",
        PullRequestState::Open => "Open",
        PullRequestState::Closed => "Closed",
        PullRequestState::Merged => "Merged",
        PullRequestState::Unrecognized(_) => "Open",
    }
}

fn state_color(state: &PullRequestState, is_draft: bool, cx: &App) -> Hsla {
    let palette = palette(cx);
    match state {
        PullRequestState::Open if is_draft => palette.text_muted.hsla(),
        PullRequestState::Closed => cx.theme().status().error,
        PullRequestState::Merged => palette.pending.hsla(),
        _ => palette.ok.hsla(),
    }
}

// ---------------------------------------------------------------------------------------------
// The link dialog

/// What the user typed to link: a pull request's URL, or its number in the thread's repository.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum LinkInput {
    Url {
        host: String,
        repository: String,
        number: i64,
        url: String,
    },
    /// Resolved in the thread's checkout (`git.resolvePullRequest`).
    Reference(String),
}

pub fn parse_link_input(input: &str) -> Result<LinkInput> {
    let input = input.trim();
    if input.is_empty() {
        return Err(anyhow!("Paste a pull request URL, or enter its number."));
    }
    if input.contains("://") {
        let (host, repository, number) = parse_pull_request_url(input)
            .ok_or_else(|| anyhow!("That URL isn't a pull request's."))?;
        return Ok(LinkInput::Url {
            host,
            repository,
            number,
            url: input.to_string(),
        });
    }
    // A checkout command such as `gh pr checkout 42` ends with the reference.
    let reference = input.split_whitespace().last().unwrap_or(input);
    let number = reference.trim_start_matches('#');
    match number.parse::<i64>() {
        Ok(number) if number > 0 => Ok(LinkInput::Reference(number.to_string())),
        _ => Err(anyhow!(
            "Use a pull request URL, a checkout command, 123, or #123."
        )),
    }
}

/// Links a pull request to the thread by its URL or number (T3's `LinkPullRequestDialog`).
pub struct LinkPullRequestDialog {
    thread_id: String,
    git: Entity<ThreadGit>,
    editor: Entity<Editor>,
    error: Option<SharedString>,
    linking: bool,
    _task: Option<Task<()>>,
}

impl LinkPullRequestDialog {
    pub fn new(
        thread_id: String,
        git: Entity<ThreadGit>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        let editor = cx.new(|cx| {
            let mut editor = Editor::single_line(window, cx);
            editor.set_placeholder_text("Pull request URL or #42", window, cx);
            editor
        });
        Self {
            thread_id,
            git,
            editor,
            error: None,
            linking: false,
            _task: None,
        }
    }

    fn confirm(&mut self, _: &menu::Confirm, _: &mut Window, cx: &mut Context<Self>) {
        self.link(cx);
    }

    fn link(&mut self, cx: &mut Context<Self>) {
        if self.linking {
            return;
        }
        let Some(backend) = thread_chat::backend(cx) else {
            self.error = Some("mustr isn't connected to T3".into());
            cx.notify();
            return;
        };
        let input = match parse_link_input(&self.editor.read(cx).text(cx)) {
            Ok(input) => input,
            Err(error) => {
                self.error = Some(error.to_string().into());
                cx.notify();
                return;
            }
        };
        let cwd = self
            .git
            .read(cx)
            .checkout()
            .map(|checkout| checkout.cwd.clone());
        let thread_id = self.thread_id.clone();
        self.linking = true;
        self.error = None;
        cx.notify();
        self._task = Some(cx.spawn(async move |this, cx| {
            let result = async {
                let (host, repository, number, url) = match input {
                    LinkInput::Url {
                        host,
                        repository,
                        number,
                        url,
                    } => (host, repository, number, url),
                    LinkInput::Reference(reference) => {
                        let cwd = cwd.context("This thread has no checkout to find it in.")?;
                        let resolved = backend
                            .call::<rpc::GitResolvePullRequest>(
                                GitPullRequestRefInput { cwd, reference },
                                Access::Read,
                            )
                            .await
                            .context("finding the pull request")?
                            .pull_request;
                        let (host, repository, number) = parse_pull_request_url(&resolved.url)
                            .with_context(|| {
                                format!("T3 found {}, which mustr can't link", resolved.url)
                            })?;
                        (host, repository, number, resolved.url)
                    }
                };
                backend
                    .dispatch(link_command(
                        &thread_id,
                        host,
                        repository,
                        number,
                        url,
                        thread_chat::commands::new_id(),
                    ))
                    .await
                    .context("linking the pull request")?;
                anyhow::Ok(())
            }
            .await;
            this.update(cx, |this, cx| {
                this.linking = false;
                match result {
                    Ok(()) => cx.emit(DismissEvent),
                    Err(error) => {
                        this.error = Some(format!("{error:#}").into());
                        cx.notify();
                    }
                }
            })
            .ok();
        }));
    }
}

impl EventEmitter<DismissEvent> for LinkPullRequestDialog {}

impl ModalView for LinkPullRequestDialog {}

impl Focusable for LinkPullRequestDialog {
    fn focus_handle(&self, cx: &App) -> FocusHandle {
        self.editor.focus_handle(cx)
    }
}

impl Render for LinkPullRequestDialog {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let palette = palette(cx);
        v_flex()
            .key_context("LinkPullRequestDialog")
            .on_action(cx.listener(Self::confirm))
            .on_action(cx.listener(|_, _: &menu::Cancel, _, cx| cx.emit(DismissEvent)))
            .w(tokens::modal_width(rems(30.), window))
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
                        Label::new("Link pull request")
                            .size(LabelSize::Large)
                            .weight(gpui::FontWeight::SEMIBOLD),
                    )
                    .child(
                        Label::new("The thread shows it, and its agent can watch it.")
                            .size(LabelSize::Small)
                            .color(Color::Muted),
                    ),
            )
            .child(
                div()
                    .px_2()
                    .py_1p5()
                    .rounded(scaled(tokens::CONTROL_RADIUS))
                    .border(tokens::BORDER_WIDTH)
                    .border_color(palette.border.hsla())
                    .bg(palette.background.hsla())
                    .child(self.editor.clone()),
            )
            .when_some(self.error.clone(), |this, error| {
                this.child(Label::new(error).size(LabelSize::Small).color(Color::Error))
            })
            .child(
                h_flex()
                    .justify_end()
                    .gap_2()
                    .child(
                        TextButton::new("link-pull-request-cancel", "Cancel")
                            .on_click(cx.listener(|_, _, _, cx| cx.emit(DismissEvent))),
                    )
                    .child(
                        TextButton::new(
                            "link-pull-request-confirm",
                            if self.linking { "Linking…" } else { "Link" },
                        )
                        .style(ButtonStyle::Primary)
                        .disabled(self.linking)
                        .on_click(cx.listener(|this, _, _, cx| this.link(cx))),
                    ),
            )
    }
}

// ---------------------------------------------------------------------------------------------
// The toolbar

/// One thread's parts of the toolbar.
struct ToolbarThread {
    thread_id: String,
    git: Entity<ThreadGit>,
    /// Open in ▾.
    open_in: Entity<ThreadGitControls>,
    /// Commit ▾, or whatever the branch needs next.
    controls: Entity<ThreadGitControls>,
    progress: Entity<GitProgressRow>,
    pull_request: Entity<PullRequestStatus>,
    _subscriptions: [Subscription; 2],
}

/// The toolbar over an agent thread's view.
pub struct ThreadToolbar {
    workspace: WeakEntity<Workspace>,
    thread: Option<ToolbarThread>,
    /// The thread view it's over, which may be a draft that becomes a thread.
    view: Option<WeakEntity<ThreadView>>,
    menu: MenuSlot,
    /// Whose menu `menu` is.
    menu_owner: Option<ToolbarItem>,
    location: ToolbarItemLocation,
    /// Measures the toolbar, to decide which items fit.
    overflow: OverflowRow,
    /// The items that didn't fit when it was last drawn, which its "…" menu offers.
    hidden: Vec<ToolbarItem>,
    /// While a merge back is being sent.
    merging: bool,
    /// T3's local dev servers, once the toolbar shows a thread.
    servers: Option<(Entity<LocalServers>, Subscription)>,
    _view: Option<Subscription>,
    _activation: Subscription,
}

impl ThreadToolbar {
    pub fn new(
        workspace: WeakEntity<Workspace>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        // T3 doesn't watch checkouts; like T3's app, look again when the window comes back.
        let activation = cx.observe_window_activation(window, |this, window, cx| {
            if window.is_window_active() {
                this.refresh_status(cx);
            }
        });
        Self {
            workspace,
            thread: None,
            view: None,
            menu: MenuSlot::default(),
            menu_owner: None,
            location: ToolbarItemLocation::Hidden,
            overflow: OverflowRow::new(),
            hidden: Vec::new(),
            merging: false,
            servers: None,
            _view: None,
            _activation: activation,
        }
    }

    fn refresh_status(&self, cx: &mut Context<Self>) {
        if self.location == ToolbarItemLocation::Hidden {
            return;
        }
        if let Some(thread) = &self.thread {
            thread.git.update(cx, |git, cx| git.refresh(cx));
        }
    }

    /// The thread the toolbar shows, while it shows one.
    #[cfg(test)]
    pub fn thread_id(&self) -> Option<&str> {
        if self.location == ToolbarItemLocation::Hidden {
            return None;
        }
        self.thread.as_ref().map(|thread| thread.thread_id.as_str())
    }

    /// Shows the toolbar for the thread the view shows, or hides it for a draft.
    fn sync_thread(&mut self, cx: &mut Context<Self>) -> ToolbarItemLocation {
        let thread_id = self
            .view
            .as_ref()
            .and_then(WeakEntity::upgrade)
            .and_then(|view| view.read(cx).thread_id().map(str::to_string));
        // The last thread's parts stay while something else shows, so its git keeps going
        // while, say, the review of its changes opens.
        let Some(thread_id) = thread_id else {
            self.location = ToolbarItemLocation::Hidden;
            return self.location;
        };
        let kept = self.thread.as_ref().map(|thread| thread.thread_id.as_str());
        if kept != Some(thread_id.as_str()) {
            let git = ThreadGit::for_thread(&thread_id, cx);
            let workspace = self.workspace.clone();
            let open_in = cx.new(|cx| {
                ThreadGitControls::new(
                    git.clone(),
                    workspace.clone(),
                    ControlsPlacement::Toolbar,
                    cx,
                )
                .parts(ControlsParts::OpenIn)
            });
            let controls = cx.new(|cx| {
                ThreadGitControls::new(
                    git.clone(),
                    workspace.clone(),
                    ControlsPlacement::Toolbar,
                    cx,
                )
                .parts(ControlsParts::GitAction)
            });
            let pull_request = cx.new(|cx| PullRequestStatus::new(thread_id.clone(), cx));
            let progress = cx.new(|cx| {
                let workspace = workspace.clone();
                let project_pull_request = pull_request.downgrade();
                GitProgressRow::new(
                    git.clone(),
                    Some(Arc::new(
                        move |url: String, number: i64, window: &mut Window, cx: &mut App| {
                            let reference = project_pull_request.upgrade().and_then(|status| {
                                let project_id = status.read(cx).project_id.clone()?;
                                let (host, repository, _) = parse_pull_request_url(&url)?;
                                Some(PullRequestRef {
                                    allow_stale: None,
                                    expected_account_id: None,
                                    host: Some(host),
                                    number,
                                    project_id,
                                    repository,
                                })
                            });
                            if let Some(reference) = reference {
                                open_pull_request_review(workspace.clone(), reference, window, cx);
                            }
                        },
                    )),
                    cx,
                )
            });
            self.thread = Some(ToolbarThread {
                _subscriptions: [
                    cx.observe(&git, |_, _, cx| cx.notify()),
                    cx.observe(&pull_request, |_, _, cx| cx.notify()),
                ],
                thread_id,
                git,
                open_in,
                controls,
                progress,
                pull_request,
            });
        }
        if self.servers.is_none() {
            let servers = LocalServers::global(cx);
            let subscription = cx.observe(&servers, |_, _, cx| cx.notify());
            self.servers = Some((servers, subscription));
        }
        self.location = ToolbarItemLocation::PrimaryLeft;
        self.location
    }

    /// The thread's local dev servers.
    fn local_servers(
        &self,
        thread: &ToolbarThread,
        cx: &App,
    ) -> Vec<t3_client::t3_api::types::DiscoveredLocalServer> {
        self.servers
            .as_ref()
            .map(|(servers, _)| servers.read(cx).of_thread(&thread.thread_id))
            .unwrap_or_default()
    }

    fn deploy_servers_menu(
        &mut self,
        event: &ClickEvent,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(thread) = &self.thread else {
            return;
        };
        let servers = self.local_servers(thread, cx);
        let here = crate::worktrees::worktrees_are_local(cx);
        let menu = ContextMenu::build(window, cx, move |mut menu, _, _| {
            menu = menu.header(if here {
                "The thread's local servers"
            } else {
                "The thread's servers, on T3's machine"
            });
            for server in &servers {
                let url = server.url.clone();
                let label = match &server.process_name {
                    Some(process) => {
                        format!("Open {} · {process}", local_servers::server_label(server))
                    }
                    None => format!("Open {}", local_servers::server_label(server)),
                };
                menu = menu.item(
                    ContextMenuEntry::new(label)
                        .disabled(!here)
                        .handler(move |_, cx| cx.open_url(&url)),
                );
            }
            menu
        });
        self.deploy_menu(ToolbarItem::Servers, event, menu, window, cx);
    }

    fn render_servers(&self, thread: &ToolbarThread, cx: &mut Context<Self>) -> AnyElement {
        let servers = self.local_servers(thread, cx);
        let label = local_servers::chip_label(&servers).unwrap_or_default();
        let palette = palette(cx);
        let tooltip = if crate::worktrees::worktrees_are_local(cx) {
            "The thread's terminals run these servers: open one in your browser"
        } else {
            "The thread's terminals run these servers on T3's machine"
        };
        QuietButton::new("thread-local-servers", label)
            .muted(true)
            .start(status_dot(palette.ok.hsla()))
            .menu_indicator(true)
            .tooltip(Tooltip::text(tooltip))
            .menu_open(self.menu_open_for(ToolbarItem::Servers))
            .on_click(
                cx.listener(|this, event, window, cx| this.deploy_servers_menu(event, window, cx)),
            )
            .into_any_element()
    }

    /// What merging the thread back would do, when it's a fork (`merge_back.rs`).
    fn merge_back(&self, thread: &ToolbarThread, cx: &mut App) -> Option<MergeBack> {
        let shells = ThreadShells::global(cx);
        let view = self.view.as_ref().and_then(WeakEntity::upgrade);
        let state = view.as_ref().and_then(|view| view.read(cx).thread_state());
        MergeBack::of_thread(&thread.thread_id, state, shells.read(cx))
    }

    fn run_merge_back(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(merge) = self
            .thread
            .as_ref()
            .and_then(|thread| self.merge_back(thread, cx))
        else {
            return;
        };
        if self.merging || merge.run_id.is_none() {
            return;
        }
        self.merging = true;
        cx.notify();
        let task = merge.run(self.workspace.clone(), window, cx);
        cx.spawn(async move |this, cx| {
            task.await;
            this.update(cx, |this, cx| {
                this.merging = false;
                cx.notify();
            })
            .ok();
        })
        .detach();
    }

    fn render_merge_back(&self, merge: &MergeBack, cx: &mut Context<Self>) -> AnyElement {
        let label = if self.merging {
            "Merging back…".into()
        } else {
            merge.label()
        };
        TextButton::new("thread-merge-back", label)
            .disabled(self.merging || merge.run_id.is_none())
            .tooltip(merge.tooltip())
            .on_click(cx.listener(|this, _, window, cx| this.run_merge_back(window, cx)))
            .into_any_element()
    }

    fn open_branch_picker(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(thread) = &self.thread else {
            return;
        };
        let git = thread.git.clone();
        self.workspace
            .update(cx, |workspace, cx| {
                BranchPickerDelegate::open(git, workspace, window, cx)
            })
            .ok();
    }

    fn open_link_dialog(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(thread) = &self.thread else {
            return;
        };
        let thread_id = thread.thread_id.clone();
        let git = thread.git.clone();
        self.workspace
            .update(cx, |workspace, cx| {
                workspace.toggle_modal(window, cx, |window, cx| {
                    LinkPullRequestDialog::new(thread_id, git, window, cx)
                })
            })
            .ok();
    }

    fn deploy_pull_request_menu(
        &mut self,
        event: &ClickEvent,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(entries) = self.pull_request_entries(cx) else {
            return;
        };
        let menu = ContextMenu::build(window, cx, move |menu, _, _| entries(menu));
        self.deploy_menu(ToolbarItem::PullRequest, event, menu, window, cx);
    }

    fn deploy_menu(
        &mut self,
        item: ToolbarItem,
        event: &ClickEvent,
        menu: Entity<ContextMenu>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.menu_owner = Some(item);
        self.menu
            .deploy(menu_position(event, window), menu, window, cx, |this| {
                &mut this.menu
            });
    }

    /// Whether the menu open is `item`'s.
    fn menu_open_for(&self, item: ToolbarItem) -> bool {
        self.menu.is_open() && self.menu_owner == Some(item)
    }

    /// The pull request's menu: review it, open it on the web, mark a draft ready, merge or close
    /// it (in its review), watch it, link or unlink it; the thread's other linked pull requests;
    /// and link another.
    fn pull_request_menu(&self, cx: &App) -> Option<Vec<PullRequestMenuRow>> {
        let thread = self.thread.as_ref()?;
        let status = thread.pull_request.read(cx);
        let mut rows = Vec::new();
        if let Some(pull_request) = status.pull_request() {
            let state = status.state();
            let is_open = state
                .as_ref()
                .is_none_or(|(state, _)| *state == PullRequestState::Open);
            let is_draft = state.as_ref().is_some_and(|(_, draft)| *draft);
            let number = pull_request.reference.number;
            let repository = &pull_request.reference.repository;
            rows.push(PullRequestMenuRow::Header(match &state {
                Some((state, is_draft)) => {
                    format!("{repository}#{number} · {}", state_label(state, *is_draft))
                }
                None => format!("{repository}#{number}"),
            }));
            if let Some((_, checks)) = status.checks() {
                rows.push(PullRequestMenuRow::Label(checks));
            }
            rows.push(PullRequestMenuRow::Review(
                format!("Review #{number}"),
                pull_request.reference.clone(),
            ));
            rows.push(PullRequestMenuRow::OpenOnWeb(
                web_label(&host_of(pull_request)),
                pull_request.url.clone(),
            ));
            if is_open && is_draft {
                let can_write = thread_chat::backend(cx).is_some_and(|backend| {
                    backend.blocked_reason(Access::SourceControlWrite).is_none()
                });
                rows.push(PullRequestMenuRow::MarkReady { enabled: can_write });
            }
            if is_open {
                // Merging and closing ask first, and merging asks how, in the review's header.
                rows.push(PullRequestMenuRow::Review(
                    "Merge or close…".to_string(),
                    pull_request.reference.clone(),
                ));
            }
            rows.push(PullRequestMenuRow::Separator);
            if pull_request.is_linked() {
                if is_open {
                    rows.push(PullRequestMenuRow::Watch(pull_request.watching));
                }
                rows.push(PullRequestMenuRow::Unlink);
            } else {
                rows.push(PullRequestMenuRow::LinkShown);
            }
            if !pull_request.others.is_empty() {
                rows.push(PullRequestMenuRow::Separator);
                rows.push(PullRequestMenuRow::Header(
                    "Also linked to this thread".to_string(),
                ));
                for other in &pull_request.others {
                    rows.push(PullRequestMenuRow::Review(
                        other.label(),
                        other.reference.clone(),
                    ));
                }
            }
            rows.push(PullRequestMenuRow::Separator);
        }
        rows.push(PullRequestMenuRow::LinkAnother);
        Some(rows)
    }

    /// Adds the pull request's menu to a menu.
    fn pull_request_entries(
        &self,
        cx: &mut Context<Self>,
    ) -> Option<impl FnOnce(ContextMenu) -> ContextMenu + 'static> {
        let rows = self.pull_request_menu(cx)?;
        let status = self.thread.as_ref()?.pull_request.clone();
        let this = cx.weak_entity();
        Some(move |mut menu: ContextMenu| {
            for row in rows {
                menu = match row {
                    PullRequestMenuRow::Header(header) => menu.header(header),
                    PullRequestMenuRow::Label(label) => menu.label(label),
                    PullRequestMenuRow::Separator => menu.separator(),
                    PullRequestMenuRow::Review(label, reference) => {
                        let this = this.clone();
                        menu.entry(label, None, move |window, cx| {
                            let reference = reference.clone();
                            this.update(cx, |this, cx| {
                                open_pull_request_review(
                                    this.workspace.clone(),
                                    reference,
                                    window,
                                    cx,
                                )
                            })
                            .ok();
                        })
                    }
                    PullRequestMenuRow::OpenOnWeb(label, url) => {
                        menu.entry(label, None, move |_, cx| cx.open_url(&url))
                    }
                    PullRequestMenuRow::MarkReady { enabled } => {
                        let status = status.clone();
                        menu.item(
                            ContextMenuEntry::new("Mark ready for review")
                                .disabled(!enabled)
                                .handler(move |_, cx| {
                                    status.update(cx, |status, cx| status.mark_ready(cx))
                                }),
                        )
                    }
                    PullRequestMenuRow::Watch(watching) => {
                        let status = status.clone();
                        menu.toggleable_entry(
                            "Watch for CI and comments",
                            watching,
                            IconPosition::End,
                            None,
                            move |_, cx| {
                                status.update(cx, |status, cx| status.set_watching(!watching, cx))
                            },
                        )
                    }
                    PullRequestMenuRow::Unlink => {
                        let status = status.clone();
                        menu.entry("Unlink from thread", None, move |_, cx| {
                            status.update(cx, |status, cx| status.unlink(cx))
                        })
                    }
                    PullRequestMenuRow::LinkShown => {
                        let status = status.clone();
                        menu.entry("Link to thread", None, move |_, cx| {
                            status.update(cx, |status, cx| status.link_shown(cx))
                        })
                    }
                    PullRequestMenuRow::LinkAnother => {
                        let this = this.clone();
                        menu.entry("Link a pull request…", None, move |window, cx| {
                            this.update(cx, |this, cx| this.open_link_dialog(window, cx))
                                .ok();
                        })
                    }
                };
            }
            menu
        })
    }

    /// The "…" menu: the toolbar's items that didn't fit, in their order.
    fn deploy_overflow_menu(
        &mut self,
        event: &ClickEvent,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(thread) = &self.thread else {
            return;
        };
        let thread_id = thread.thread_id.clone();
        let git = thread.git.clone();
        let merge = self.merge_back(thread, cx);
        let entries = self.overflow_entries(merge.as_ref(), cx);
        let pull_request_entries = entries
            .contains(&OverflowEntry::PullRequest)
            .then(|| self.pull_request_entries(cx))
            .flatten();
        let servers = self.local_servers(thread, cx);
        let here = crate::worktrees::worktrees_are_local(cx);
        let editors = entries
            .contains(&OverflowEntry::OpenIn)
            .then(|| editor_choices(&Editors::global(cx), cx))
            .unwrap_or_default();
        let this = cx.weak_entity();
        let menu = ContextMenu::build(window, cx, move |mut menu, _, _| {
            let mut pull_request_entries = pull_request_entries;
            let mut editors = Some(editors);
            for entry in entries {
                match entry {
                    OverflowEntry::ReviewChanges => {
                        let thread_id = thread_id.clone();
                        menu = menu.entry(
                            "Review changes",
                            Some(Box::new(ReviewChanges)),
                            move |window, cx| {
                                window.dispatch_action(
                                    Box::new(ReviewThreadChanges {
                                        thread_id: thread_id.clone(),
                                    }),
                                    cx,
                                )
                            },
                        );
                    }
                    OverflowEntry::Servers => {
                        for server in &servers {
                            let url = server.url.clone();
                            menu = menu.item(
                                ContextMenuEntry::new(format!(
                                    "Open {}",
                                    local_servers::server_label(server)
                                ))
                                .disabled(!here)
                                .handler(move |_, cx| cx.open_url(&url)),
                            );
                        }
                    }
                    OverflowEntry::MergeBack(enabled) => {
                        let this = this.clone();
                        menu = menu.item(
                            ContextMenuEntry::new("Merge back")
                                .disabled(!enabled)
                                .handler(move |window, cx| {
                                    this.update(cx, |this, cx| this.run_merge_back(window, cx))
                                        .ok();
                                }),
                        );
                    }
                    OverflowEntry::Terminal => {
                        let thread_id = thread_id.clone();
                        menu = menu.item(
                            ContextMenuEntry::new("Open a terminal")
                                .custom_icon_path(MustrIcon::Terminal.path())
                                .icon_color(Color::Muted)
                                .handler(move |window, cx| {
                                    window.dispatch_action(
                                        Box::new(OpenThreadTerminal {
                                            thread_id: thread_id.clone(),
                                        }),
                                        cx,
                                    )
                                }),
                        );
                    }
                    OverflowEntry::OpenIn => {
                        menu =
                            open_in_entries(menu, editors.take().unwrap_or_default(), true, &git);
                    }
                    OverflowEntry::LinkPullRequest => {
                        let this = this.clone();
                        menu = menu.entry("Link a pull request…", None, move |window, cx| {
                            this.update(cx, |this, cx| this.open_link_dialog(window, cx))
                                .ok();
                        });
                    }
                    OverflowEntry::PullRequest => {
                        if let Some(entries) = pull_request_entries.take() {
                            menu = entries(menu.separator());
                        }
                    }
                }
            }
            menu
        });
        self.deploy_menu(ToolbarItem::More, event, menu, window, cx);
    }

    /// What the "…" menu offers for the items that didn't fit when the toolbar was last drawn:
    /// the actions first, in the toolbar's order, then the pull request's menu.
    fn overflow_entries(&self, merge: Option<&MergeBack>, _cx: &App) -> Vec<OverflowEntry> {
        if self.thread.is_none() {
            return Vec::new();
        }
        let mut actions = Vec::new();
        let mut pull_request = Vec::new();
        for item in &self.hidden {
            match item {
                ToolbarItem::ReviewChanges => actions.push(OverflowEntry::ReviewChanges),
                ToolbarItem::Terminal => actions.push(OverflowEntry::Terminal),
                ToolbarItem::Servers => actions.push(OverflowEntry::Servers),
                ToolbarItem::MergeBack => actions.push(OverflowEntry::MergeBack(
                    !self.merging && merge.is_some_and(|merge| merge.run_id.is_some()),
                )),
                ToolbarItem::OpenIn => actions.push(OverflowEntry::OpenIn),
                ToolbarItem::LinkPullRequest => actions.push(OverflowEntry::LinkPullRequest),
                ToolbarItem::PullRequest => pull_request.push(OverflowEntry::PullRequest),
                ToolbarItem::Branch | ToolbarItem::GitAction | ToolbarItem::More => {}
            }
        }
        actions.extend(pull_request);
        actions
    }

    /// `● main 3 ▾`: the branch, with a dot for its state (changed, pushed, or not yet) and how
    /// many files have changed; the rest is in its tooltip and the branch picker.
    fn render_branch(&self, thread: &ToolbarThread, cx: &mut Context<Self>) -> AnyElement {
        let git = thread.git.read(cx);
        if git.checkout().is_none() {
            let waiting = git
                .status_error()
                .cloned()
                .unwrap_or_else(|| "Waiting for T3…".into());
            return Label::new(waiting)
                .size(LabelSize::Small)
                .color(Color::Muted)
                .truncate()
                .into_any_element();
        }
        let palette = palette(cx);
        let status = git.status();
        let branch = status
            .and_then(|status| status.branch().map(str::to_string))
            .or_else(|| git.checkout().and_then(|checkout| checkout.branch.clone()))
            .unwrap_or_else(|| "No branch".to_string());
        let dot = match status {
            Some(status) if status.is_dirty() => palette.warning.hsla(),
            Some(status) if status.has_upstream() && status.ahead() == 0 => palette.ok.hsla(),
            _ => palette.text_muted.hsla(),
        };
        let changed = status.map_or(0, |status| status.changed_files());
        let mut tooltip = git
            .checkout()
            .map(|checkout| {
                format!(
                    "{branch}\n{} · switch or create a branch, or move the thread",
                    checkout.cwd
                )
            })
            .unwrap_or_else(|| format!("{branch}\nSwitch or create a branch"));
        if let Some(summary) = git_summary(&thread.git, cx) {
            tooltip.push_str(&format!("\n{summary}"));
        }
        QuietButton::new("thread-branch", branch)
            .start(status_dot(dot))
            .when(changed > 0, |this| this.count(changed.to_string()))
            .menu_indicator(true)
            .truncate(true)
            .tooltip(Tooltip::text(tooltip))
            .on_click(cx.listener(|this, _, window, cx| this.open_branch_picker(window, cx)))
            .into_any_element()
    }

    /// `● #80 ✗ ▾`: the pull request's number, a dot for its state, and an icon for its checks;
    /// the rest is in its tooltip and its menu.
    fn render_pull_request(&self, thread: &ToolbarThread, cx: &mut Context<Self>) -> AnyElement {
        let status = thread.pull_request.read(cx);
        let Some(pull_request) = status.pull_request().cloned() else {
            return div().into_any_element();
        };
        let number = pull_request.reference.number;
        let (state, is_draft) = status
            .state()
            .unwrap_or((PullRequestState::Open, pull_request.is_draft));
        let origin = match &pull_request.origin {
            PullRequestOrigin::Linked(ThreadPullRequestLinkSource::Created) => {
                "Created from this thread"
            }
            PullRequestOrigin::Linked(ThreadPullRequestLinkSource::Agent) => "Linked by the agent",
            PullRequestOrigin::Linked(ThreadPullRequestLinkSource::Stack) => "Found in the stack",
            PullRequestOrigin::Linked(_) | PullRequestOrigin::LinkedField => "Linked by you",
            PullRequestOrigin::Branch => "The pull request of the thread's branch",
        };
        let mut tooltip = format!("{}#{number}", pull_request.reference.repository);
        if let Some(title) = status.title() {
            tooltip.push_str(&format!(": {title}"));
        }
        tooltip.push_str(&format!("\n{} · {origin}", state_label(&state, is_draft)));
        let checks = status.checks();
        if let Some((_, label)) = &checks {
            tooltip.push_str(&format!("\n{label}"));
        }
        if pull_request.watching {
            tooltip.push_str("\nThe agent watches its CI and comments");
        }
        match pull_request.others.len() {
            0 => {}
            1 => tooltip.push_str("\n1 more pull request linked to this thread"),
            others => tooltip.push_str(&format!(
                "\n{others} more pull requests linked to this thread"
            )),
        }
        if let Some(error) = &status.error {
            tooltip.push_str(&format!("\n{error}"));
        }
        let checks_icon = checks.map(|(state, _)| {
            let (icon, color) = match state {
                PullRequestChecksState::Passing => (IconName::Check, Color::Success),
                PullRequestChecksState::Failing => (IconName::XCircle, Color::Error),
                _ => (IconName::LoadCircle, Color::Muted),
            };
            Icon::new(icon)
                .size(IconSize::Custom(scaled(px(12.))))
                .color(color)
        });
        QuietButton::new("thread-pull-request", format!("#{number}"))
            .start(status_dot(state_color(&state, is_draft, cx)))
            .when_some(checks_icon, |this, icon| this.end(icon))
            .menu_indicator(true)
            .tooltip(Tooltip::text(tooltip))
            .menu_open(self.menu_open_for(ToolbarItem::PullRequest))
            .on_click(cx.listener(|this, event, window, cx| {
                this.deploy_pull_request_menu(event, window, cx)
            }))
            .into_any_element()
    }

    fn render_link_pull_request(&self, cx: &mut Context<Self>) -> AnyElement {
        QuietButton::new("thread-link-pull-request", "Link PR")
            .muted(true)
            .start(
                MustrIcon::PullRequest
                    .icon()
                    .size(IconSize::Custom(scaled(px(12.))))
                    .color(Color::Muted),
            )
            .tooltip(Tooltip::text("Link a pull request to this thread"))
            .on_click(cx.listener(|this, _, window, cx| this.open_link_dialog(window, cx)))
            .into_any_element()
    }

    fn render_terminal(&self, thread_id: String) -> AnyElement {
        IconOnlyButton::new(
            "thread-terminal",
            MustrIcon::Terminal
                .icon()
                .size(IconSize::Custom(scaled(TOOLBAR_ICON_SIZE))),
        )
        .tooltip(Tooltip::for_action_title(
            "Terminal",
            &crate::terminal_panel::ToggleThreadTerminal,
        ))
        .on_click(move |_, window, cx| {
            window.dispatch_action(
                Box::new(OpenThreadTerminal {
                    thread_id: thread_id.clone(),
                }),
                cx,
            )
        })
        .into_any_element()
    }

    /// The items the toolbar would show if it were wide enough, in their order, with their
    /// priorities: Commit ▾ and the branch always show (the branch shrinks), then Review
    /// changes, Merge back and the pull request; Link PR, Terminal, and Open in go into the "…"
    /// menu first.
    fn items(
        &self,
        thread: &ToolbarThread,
        merge: Option<&MergeBack>,
        cx: &App,
    ) -> Vec<(ToolbarItem, OverflowItem)> {
        let status = thread.pull_request.read(cx);
        let listed = status.project_id.is_some();
        let has_pull_request = listed && status.pull_request().is_some();
        let git = thread.git.read(cx);
        let mut items = vec![ToolbarItem::Branch];
        if has_pull_request {
            items.push(ToolbarItem::PullRequest);
        } else if listed {
            items.push(ToolbarItem::LinkPullRequest);
        }
        if !self.local_servers(thread, cx).is_empty() {
            items.push(ToolbarItem::Servers);
        }
        items.push(ToolbarItem::ReviewChanges);
        if merge.is_some() {
            items.push(ToolbarItem::MergeBack);
        }
        items.push(ToolbarItem::GitAction);
        items.push(ToolbarItem::Terminal);
        if git.checkout().is_some() {
            items.push(ToolbarItem::OpenIn);
        }
        let branch_min_width = tokens::scaled_pixels(BRANCH_MIN_WIDTH, cx);
        items
            .into_iter()
            .map(|item| {
                let overflow = OverflowItem::new(item.key(), item.priority());
                let overflow = match item {
                    ToolbarItem::Branch => overflow.shrinks_to(branch_min_width),
                    _ => overflow,
                };
                (item, overflow)
            })
            .collect()
    }
}

/// The narrowest the branch gets, at mustr's design size, before items go into the "…" menu.
const BRANCH_MIN_WIDTH: Pixels = px(100.);

/// The gap between the toolbar's items, at mustr's design size.
const ITEM_GAP: Pixels = px(8.);

/// The size of the toolbar's icons, at mustr's design size (Zed's `IconSize`s assume 16px rems).
const TOOLBAR_ICON_SIZE: Pixels = px(15.);

/// The width of the "…" button (an outlined icon button), at mustr's design size, without its
/// 1px borders.
const MORE_WIDTH: Pixels = px(27.);

/// A small status dot, as the branch's and the pull request's.
fn status_dot(color: Hsla) -> impl IntoElement {
    div()
        .flex_none()
        .size(scaled(px(6.)))
        .rounded_full()
        .bg(color)
}

/// `Open on GitHub`, or on the pull request's host.
fn web_label(host: &str) -> String {
    match host {
        "github.com" => "Open on GitHub".to_string(),
        "gitlab.com" => "Open on GitLab".to_string(),
        "bitbucket.org" => "Open on Bitbucket".to_string(),
        host => format!("Open on {host}"),
    }
}

/// The toolbar's items, each of which shows only when there's room (see
/// [`ThreadToolbar::items`]).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum ToolbarItem {
    Branch,
    PullRequest,
    LinkPullRequest,
    Servers,
    ReviewChanges,
    MergeBack,
    GitAction,
    Terminal,
    OpenIn,
    /// The "…" button, for its menu.
    More,
}

impl ToolbarItem {
    fn key(self) -> &'static str {
        match self {
            Self::Branch => "thread-toolbar-branch",
            Self::PullRequest => "thread-toolbar-pull-request",
            Self::LinkPullRequest => "thread-toolbar-link-pull-request",
            Self::Servers => "thread-toolbar-servers",
            Self::ReviewChanges => "thread-toolbar-review-changes",
            Self::MergeBack => "thread-toolbar-merge-back",
            Self::GitAction => "thread-toolbar-git-action",
            Self::Terminal => "thread-toolbar-terminal",
            Self::OpenIn => "thread-toolbar-open-in",
            Self::More => "thread-toolbar-more",
        }
    }

    /// Whether it's one of the actions at the toolbar's right.
    fn is_action(self) -> bool {
        matches!(
            self,
            Self::ReviewChanges | Self::MergeBack | Self::GitAction | Self::Terminal | Self::OpenIn
        )
    }

    fn priority(self) -> u32 {
        match self {
            Self::Branch | Self::GitAction | Self::More => overflow::ALWAYS,
            Self::ReviewChanges => 90,
            Self::MergeBack => 70,
            Self::PullRequest => 60,
            Self::Servers => 35,
            Self::OpenIn => 30,
            Self::Terminal => 20,
            Self::LinkPullRequest => 10,
        }
    }
}

/// A row of the pull request's menu.
#[derive(Clone, Debug, PartialEq)]
enum PullRequestMenuRow {
    Header(String),
    /// Plain text, such as the checks.
    Label(String),
    Separator,
    /// Opens mustr's review of a pull request.
    Review(String, PullRequestRef),
    OpenOnWeb(String, String),
    MarkReady {
        enabled: bool,
    },
    /// Watch for CI and comments, checked while watching.
    Watch(bool),
    Unlink,
    /// Links the pull request of the thread's branch.
    LinkShown,
    LinkAnother,
}

/// An entry of the toolbar's "…" menu.
#[derive(Clone, Debug, PartialEq, Eq)]
enum OverflowEntry {
    ReviewChanges,
    /// Merge back, and whether it can run now.
    MergeBack(bool),
    /// The thread's local servers.
    Servers,
    Terminal,
    /// Open in each of T3's editors.
    OpenIn,
    LinkPullRequest,
    /// The pull request menu's entries.
    PullRequest,
}

/// The checkout's git status, such as "2 changed" or "Up to date", once T3 has sent it.
fn git_summary(git: &Entity<ThreadGit>, cx: &App) -> Option<SharedString> {
    let git = git.read(cx);
    git.checkout()?;
    match (git.status(), git.status_error()) {
        (Some(status), _) if status.local.is_some() => Some(status.summary().into()),
        (_, Some(_)) => Some("Git status unavailable".into()),
        _ => Some("Reading git status…".into()),
    }
}

impl ThreadPullRequest {
    #[cfg(test)]
    pub fn for_tests(reference: PullRequestRef, url: &str, origin: PullRequestOrigin) -> Self {
        Self {
            reference,
            url: url.to_string(),
            origin,
            watching: false,
            state: None,
            is_draft: false,
            title: None,
            checks: None,
            others: Vec::new(),
        }
    }
}

/// Opens mustr's review of a pull request, saying in a toast why it couldn't.
fn open_pull_request_review(
    workspace: WeakEntity<Workspace>,
    reference: PullRequestRef,
    window: &mut Window,
    cx: &mut App,
) {
    let open = pull_request_review::open_review(workspace.clone(), reference.clone(), window, cx);
    window
        .spawn(cx, async move |cx| {
            if let Err(error) = open.await {
                log::error!("couldn't open the pull request's review: {error:#}");
                workspace.update(cx, |workspace, cx| {
                    show_error(
                        workspace,
                        &pull_request_review::display_ref(&reference),
                        &format!("{error:#}"),
                        cx,
                    )
                })?;
            }
            anyhow::Ok(())
        })
        .detach_and_log_err(cx);
}

impl EventEmitter<ToolbarItemEvent> for ThreadToolbar {}

impl ToolbarItemView for ThreadToolbar {
    fn set_active_pane_item(
        &mut self,
        active_pane_item: Option<&dyn ItemHandle>,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) -> ToolbarItemLocation {
        let view = active_pane_item.and_then(|item| item.downcast::<ThreadView>());
        self.menu = MenuSlot::default();
        self._view = view.as_ref().map(|view| {
            // A draft becomes a thread once it launches.
            cx.observe(view, |this, _, cx| {
                let location = this.location;
                if this.sync_thread(cx) != location {
                    cx.emit(ToolbarItemEvent::ChangeLocation(this.location));
                }
                cx.notify();
            })
        });
        self.view = view.map(|view| view.downgrade());
        let location = self.sync_thread(cx);
        self.refresh_status(cx);
        cx.notify();
        location
    }
}

impl Render for ThreadToolbar {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let Some(thread) = self
            .thread
            .as_ref()
            .filter(|_| self.location != ToolbarItemLocation::Hidden)
        else {
            return div().into_any_element();
        };
        let merge = self.merge_back(thread, cx);
        let items = self.items(thread, merge.as_ref(), cx);
        let rem_size = window.rem_size();
        let gap = scaled(ITEM_GAP);
        let visible = self.overflow.visible(
            &items
                .iter()
                .map(|(_, overflow)| overflow.clone())
                .collect::<Vec<_>>(),
            gap.to_pixels(rem_size),
            scaled(MORE_WIDTH).to_pixels(rem_size) + tokens::BORDER_WIDTH * 2.,
        );
        let hidden = items
            .iter()
            .zip(&visible)
            .filter(|(_, visible)| !**visible)
            .map(|((item, _), _)| *item)
            .collect::<Vec<_>>();
        let thread_id = thread.thread_id.clone();
        let mut children = Vec::new();
        let mut spaced = false;
        for ((item, _), _) in items.iter().zip(&visible).filter(|(_, visible)| **visible) {
            if !spaced && item.is_action() {
                // The actions go to the right. The spacer cancels its own gap, which the fit
                // doesn't count.
                children.push((None, div().flex_1().ml(-gap).into_any_element()));
                spaced = true;
            }
            let element = match item {
                ToolbarItem::Branch => self.render_branch(thread, cx),
                ToolbarItem::PullRequest => self.render_pull_request(thread, cx),
                ToolbarItem::LinkPullRequest => self.render_link_pull_request(cx),
                ToolbarItem::ReviewChanges => {
                    let thread_id = thread_id.clone();
                    TextButton::new("thread-review-changes", "Review changes")
                        .tooltip("Review the thread's changes, with comments for the agent")
                        .on_click(move |_, window, cx| {
                            window.dispatch_action(
                                Box::new(ReviewThreadChanges {
                                    thread_id: thread_id.clone(),
                                }),
                                cx,
                            )
                        })
                        .into_any_element()
                }
                ToolbarItem::Servers => self.render_servers(thread, cx),
                ToolbarItem::MergeBack => match &merge {
                    Some(merge) => self.render_merge_back(merge, cx),
                    None => div().into_any_element(),
                },
                ToolbarItem::GitAction => thread.controls.clone().into_any_element(),
                ToolbarItem::Terminal => self.render_terminal(thread_id.clone()),
                ToolbarItem::OpenIn => thread.open_in.clone().into_any_element(),
                ToolbarItem::More => div().into_any_element(),
            };
            children.push((Some(SharedString::from(item.key())), element));
        }
        if !hidden.is_empty() {
            children.push((
                Some(overflow::MORE_KEY.into()),
                div()
                    .debug_selector(|| ToolbarItem::More.key().into())
                    .child(
                        IconOnlyButton::new(
                            ToolbarItem::More.key(),
                            Icon::new(IconName::Ellipsis)
                                .size(IconSize::Custom(scaled(TOOLBAR_ICON_SIZE))),
                        )
                        .tooltip(Tooltip::text("More of the thread's actions"))
                        .menu_open(self.menu_open_for(ToolbarItem::More))
                        .on_click(cx.listener(
                            |this, event, window, cx| this.deploy_overflow_menu(event, window, cx),
                        )),
                    )
                    .into_any_element(),
            ));
        }
        let progress = thread
            .progress
            .read(cx)
            .is_visible(cx)
            .then(|| thread.progress.clone());
        self.hidden = hidden;
        v_flex()
            .id("thread-toolbar")
            .w_full()
            .gap(scaled(px(4.)))
            .child(self.overflow.render(children, window).gap(gap))
            .children(progress)
            .children(self.menu.render())
            .into_any_element()
    }
}

#[cfg(test)]
#[path = "thread_toolbar_tests.rs"]
mod tests;

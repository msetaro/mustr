//! The thread toolbar: what ships an agent thread's work, above its timeline (`mustr-docs/PARITY.md`
//! §2.9–§2.12).
//!
//! - The checkout's branch, which opens the branch picker, and its git status
//!   (`subscribeVcsStatus`, through [`ThreadGit`]).
//! - The thread's pull request, with its state and checks (`pullRequests.summary`,
//!   `pullRequests.checks`). Clicking it opens mustr's review of it; its menu links, unlinks,
//!   and watches pull requests (`thread.pull-request.link`, `unlink`, `watch`).
//! - **Review changes** (handles `thread_chat::ReviewThreadChanges`), **Terminal** (dispatches
//!   `thread_chat::OpenThreadTerminal`, which the terminal panel handles), **Open in ▾** (handles
//!   `thread_chat::OpenInEditor`), and **Commit ▾**, with a row of a running action's steps.
//!
//! The pull request a thread shows is chosen as T3's `useLinkedThreadPullRequest` does: from the
//! thread's linked pull requests when T3 sends them, then its `linkedPullRequest`, then the pull
//! request of its branch.

use crate::{
    agent_review,
    agent_threads::{AgentThread, agent_threads},
    pull_request_review::{self, checks_summary},
    thread_git::{
        BranchPickerDelegate, ControlsPlacement, GitProgressRow, MenuSlot, ThreadGit,
        ThreadGitControls, menu_position,
    },
};
use anyhow::{Context as _, Result, anyhow};
use editor::Editor;
use gpui::{
    AnyElement, App, AppContext as _, ClickEvent, Context, DismissEvent, Entity, EventEmitter,
    FocusHandle, Focusable, Hsla, IntoElement, ParentElement as _, Render, SharedString,
    Styled as _, Subscription, Task, TaskExt as _, WeakEntity, Window, div, px, rems,
};
use mustr_ui::{ButtonStyle, Chip, TextButton, palette, tokens, tokens::scaled};
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
            PullRequestChecks, PullRequestChecksState, PullRequestState, PullRequestSummary,
            ThreadPullRequestLinkSource,
        },
    },
};
use thread_chat::{
    Access, OpenInEditor, OpenThreadTerminal, ReviewThreadChanges, Shell, ThreadBackendExt as _,
    ThreadShells, ThreadView,
};
use ui::{
    Color, ContextMenu, Icon, IconButton, IconName, IconSize, Label, LabelCommon as _, LabelSize,
    Tooltip, prelude::*,
};
use workspace::{
    ItemHandle, ModalView, Toast, ToolbarItemEvent, ToolbarItemLocation, ToolbarItemView,
    Workspace, notifications::NotificationId,
};

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
    })
    .detach();
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
    /// How many other pull requests are linked to the thread.
    pub others: usize,
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
            others: links.len() - 1,
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
        others: 0,
    })
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
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let palette = palette(cx);
        v_flex()
            .key_context("LinkPullRequestDialog")
            .on_action(cx.listener(Self::confirm))
            .on_action(cx.listener(|_, _: &menu::Cancel, _, cx| cx.emit(DismissEvent)))
            .w(rems(30.))
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
    location: ToolbarItemLocation,
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
            location: ToolbarItemLocation::Hidden,
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
            let controls = cx.new(|cx| {
                ThreadGitControls::new(
                    git.clone(),
                    workspace.clone(),
                    ControlsPlacement::Toolbar,
                    cx,
                )
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
                controls,
                progress,
                pull_request,
            });
        }
        self.location = ToolbarItemLocation::PrimaryLeft;
        self.location
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
        let Some(thread) = &self.thread else {
            return;
        };
        let status = thread.pull_request.clone();
        let pull_request = status.read(cx).pull_request().cloned();
        let is_open = status
            .read(cx)
            .state()
            .is_none_or(|(state, _)| state == PullRequestState::Open);
        let this = cx.weak_entity();
        let menu = ContextMenu::build(window, cx, move |mut menu, _, _| {
            if let Some(pull_request) = &pull_request {
                let review = this.clone();
                let reference = pull_request.reference.clone();
                menu = menu.entry(
                    format!("Review #{}", pull_request.reference.number),
                    None,
                    move |window, cx| {
                        let reference = reference.clone();
                        review
                            .update(cx, |this, cx| {
                                open_pull_request_review(
                                    this.workspace.clone(),
                                    reference,
                                    window,
                                    cx,
                                )
                            })
                            .ok();
                    },
                );
                if pull_request.is_linked() {
                    if is_open {
                        let watching = pull_request.watching;
                        let status = status.clone();
                        menu = menu.entry(
                            if watching {
                                "Stop watching"
                            } else {
                                "Watch for CI and comments"
                            },
                            None,
                            move |_, cx| {
                                status.update(cx, |status, cx| status.set_watching(!watching, cx))
                            },
                        );
                    }
                    let status = status.clone();
                    menu = menu.entry("Unlink from thread", None, move |_, cx| {
                        status.update(cx, |status, cx| status.unlink(cx))
                    });
                } else {
                    let status = status.clone();
                    menu = menu.entry("Link to thread", None, move |_, cx| {
                        status.update(cx, |status, cx| status.link_shown(cx))
                    });
                }
                menu = menu.separator();
            }
            let link = this;
            menu.entry("Link a pull request…", None, move |window, cx| {
                link.update(cx, |this, cx| this.open_link_dialog(window, cx))
                    .ok();
            })
        });
        self.menu
            .deploy(menu_position(event, window), menu, window, cx, |this| {
                &mut this.menu
            });
    }

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
        let summary = match (status, git.status_error()) {
            (Some(status), _) if status.local.is_some() => Some(status.summary()),
            (_, Some(_)) => Some("Git status unavailable".to_string()),
            _ if git.checkout().is_none() => None,
            _ => Some("Reading git status…".to_string()),
        };
        let tooltip = git
            .checkout()
            .map(|checkout| format!("{} · switch or create a branch", checkout.cwd))
            .unwrap_or_else(|| "Switch or create a branch".to_string());
        h_flex()
            .gap(scaled(px(8.)))
            .child(
                Chip::new("thread-branch", branch)
                    .dot(dot)
                    .menu_indicator(true)
                    .tooltip(tooltip)
                    .on_click(
                        cx.listener(|this, _, window, cx| this.open_branch_picker(window, cx)),
                    ),
            )
            .children(summary.map(|summary| {
                Label::new(summary)
                    .size(LabelSize::Small)
                    .color(Color::Muted)
            }))
            .into_any_element()
    }

    fn render_pull_request(&self, thread: &ToolbarThread, cx: &mut Context<Self>) -> AnyElement {
        let status = thread.pull_request.read(cx);
        let menu_button = IconButton::new("thread-pull-request-menu", IconName::ChevronDown)
            .icon_size(IconSize::XSmall)
            .tooltip(Tooltip::text("Link, unlink, or watch pull requests"))
            .on_click(cx.listener(|this, event, window, cx| {
                this.deploy_pull_request_menu(event, window, cx)
            }));
        if status.project_id.is_none() {
            // T3 hasn't listed the thread yet.
            return div().into_any_element();
        }
        let Some(pull_request) = status.pull_request().cloned() else {
            return h_flex()
                .child(
                    Chip::new("thread-link-pull-request", "Link PR")
                        .tooltip("Link a pull request to this thread")
                        .on_click(
                            cx.listener(|this, _, window, cx| this.open_link_dialog(window, cx)),
                        ),
                )
                .into_any_element();
        };
        let number = pull_request.reference.number;
        let (state, is_draft) = status
            .state()
            .unwrap_or((PullRequestState::Open, pull_request.is_draft));
        let mut label = format!("PR #{number} · {}", state_label(&state, is_draft));
        if pull_request.others > 0 {
            label.push_str(&format!(" +{}", pull_request.others));
        }
        let title = status.title();
        let origin = match &pull_request.origin {
            PullRequestOrigin::Linked(ThreadPullRequestLinkSource::Created) => {
                "Created from this thread"
            }
            PullRequestOrigin::Linked(ThreadPullRequestLinkSource::Agent) => "Linked by the agent",
            PullRequestOrigin::Linked(ThreadPullRequestLinkSource::Stack) => "Found in the stack",
            PullRequestOrigin::Linked(_) | PullRequestOrigin::LinkedField => "Linked by you",
            PullRequestOrigin::Branch => "The pull request of the thread's branch",
        };
        let tooltip = match &title {
            Some(title) => format!(
                "{}#{number}: {title}\n{origin}",
                pull_request.reference.repository
            ),
            None => format!("{}#{number}\n{origin}", pull_request.reference.repository),
        };
        let checks = status.checks();
        let palette = palette(cx);
        let error_color = cx.theme().status().error;
        let reference = pull_request.reference.clone();
        h_flex()
            .gap(scaled(px(4.)))
            .child(
                Chip::new("thread-pull-request", label)
                    .dot(state_color(&state, is_draft, cx))
                    .tooltip(tooltip)
                    .on_click(cx.listener(move |this, _, window, cx| {
                        open_pull_request_review(
                            this.workspace.clone(),
                            reference.clone(),
                            window,
                            cx,
                        )
                    })),
            )
            .when(pull_request.watching, |this| {
                this.child(
                    div()
                        .id("thread-pull-request-watching")
                        .tooltip(Tooltip::text("The agent reacts to its CI and comments"))
                        .child(
                            Icon::new(IconName::Eye)
                                .size(IconSize::XSmall)
                                .color(Color::Accent),
                        ),
                )
            })
            .when_some(checks, |this, (state, label)| {
                let (icon, color) = match state {
                    PullRequestChecksState::Passing => (IconName::Check, palette.ok.hsla()),
                    PullRequestChecksState::Failing => (IconName::XCircle, error_color),
                    _ => (IconName::Circle, palette.warning.hsla()),
                };
                this.child(
                    h_flex()
                        .gap_1()
                        .child(
                            Icon::new(icon)
                                .size(IconSize::XSmall)
                                .color(Color::Custom(color)),
                        )
                        .child(
                            Label::new(label)
                                .size(LabelSize::Small)
                                .color(Color::Custom(color)),
                        ),
                )
            })
            .child(menu_button)
            .into_any_element()
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
            others: 0,
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
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let Some(thread) = self
            .thread
            .as_ref()
            .filter(|_| self.location != ToolbarItemLocation::Hidden)
        else {
            return div().into_any_element();
        };
        let thread_id = thread.thread_id.clone();
        let terminal_thread_id = thread_id.clone();
        let branch = self.render_branch(thread, cx);
        let pull_request = self.render_pull_request(thread, cx);
        let controls = thread.controls.clone();
        let progress = thread
            .progress
            .read(cx)
            .is_visible(cx)
            .then(|| thread.progress.clone());
        v_flex()
            .id("thread-toolbar")
            .w_full()
            .gap(scaled(px(4.)))
            .child(
                h_flex()
                    .w_full()
                    .gap(scaled(px(10.)))
                    .child(branch)
                    .child(pull_request)
                    .child(div().flex_1())
                    .child(
                        TextButton::new("thread-review-changes", "Review changes")
                            .tooltip("Review the thread's changes, with comments for the agent")
                            .on_click(move |_, window, cx| {
                                window.dispatch_action(
                                    Box::new(ReviewThreadChanges {
                                        thread_id: thread_id.clone(),
                                    }),
                                    cx,
                                )
                            }),
                    )
                    .child(
                        TextButton::new("thread-terminal", "Terminal")
                            .tooltip("Open a terminal in the thread's checkout")
                            .on_click(move |_, window, cx| {
                                window.dispatch_action(
                                    Box::new(OpenThreadTerminal {
                                        thread_id: terminal_thread_id.clone(),
                                    }),
                                    cx,
                                )
                            }),
                    )
                    .child(controls),
            )
            .children(progress)
            .children(self.menu.render())
            .into_any_element()
    }
}

#[cfg(test)]
#[path = "thread_toolbar_tests.rs"]
mod tests;

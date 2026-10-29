//! How the user finds out an agent needs them while they look elsewhere (`mustr-docs/PARITY.md`
//! §2.16): a toast, an OS notification and a sound when a thread completes, needs approval or
//! input, fails, or hits a usage limit, and a count of the threads waiting on the user in the
//! status bar and on the macOS Dock.
//!
//! [`Transitions`] follows T3's shell ([`ThreadShells`]) the way T3's own app does
//! (`components/ThreadNotificationCoordinator.tsx`): a thread's status is resolved as T3's
//! sidebar resolves it (`resolveSidebarThreadStatus`), a subagent's request is rolled up to its
//! parents (`pendingSubagentRequestsByThread`), and subagents never notify on their own.
//! [`Notifier`] delivers what changed: nothing for the thread the user is looking at in a focused
//! window, a toast in the focused window for any other thread, and an OS notification when no
//! mustr window has focus. Clicking the OS notification focuses mustr and opens the thread.
//!
//! Which events notify, whether they play a sound, and whether the Dock shows the count are set
//! in the Settings page's Agents and threads ([`set_settings`]), which keeps them in mustr's
//! settings file:
//!
//! ```json
//! "notifications": {
//!   "completed": true, "needs_approval": true, "needs_input": true,
//!   "failed": false, "usage_limit": false, "sound": false, "dock_badge": true
//! }
//! ```
//!
//! Unread marks are the threads sidebar's (`thread.visit` and `lastVisitedAt`, in
//! `thread_chat/src/sidebar.rs`); opening a thread from a notification records the visit there.

#[cfg(debug_assertions)]
pub mod replay;
#[cfg(test)]
mod tests;

use anyhow::{Context as _, Result};
use futures::StreamExt as _;
use gpui::{
    App, AppContext as _, Context, DismissEvent, Entity, EventEmitter, FocusHandle, Focusable,
    FontWeight, Global, Hsla, InteractiveElement as _, IntoElement, ParentElement as _, Render,
    SharedString, StatefulInteractiveElement as _, Styled as _, Subscription, SystemNotification,
    SystemNotificationResponse, Task, WeakEntity, Window, div, px,
};
use mustr_ui::{ButtonStyle, TextButton, tokens, tokens::scaled};
use serde::{Deserialize, Serialize};
use std::collections::{HashMap, HashSet};
use std::time::Duration;
use t3_client::t3_api::types::{
    OrchestrationV2AppThreadLineageRelationshipToParent, OrchestrationV2CreationSource,
    OrchestrationV2ProviderFailureClass, OrchestrationV2ThreadShell,
    OrchestrationV2ThreadShellSettledOverride,
};
use theme::ActiveTheme as _;
use thread_chat::{Shell, ThreadShells, ThreadView};
use time::{OffsetDateTime, format_description::well_known::Rfc3339};
use ui::{
    ButtonCommon as _, Clickable as _, Color, FluentBuilder as _, Icon, IconButton, IconName,
    IconSize, Tooltip, h_flex, v_flex,
};
use workspace::{
    AppState, HideStatusItem, ItemHandle, MultiWorkspace, StatusItemView, Workspace,
    notifications::{Notification, NotificationId, SuppressEvent},
};

/// The app ID the OS shows notifications under. It matches the `.desktop` file and the bundle.
const APP_ID: &str = "dev.mustr.mustr";
/// What starts the tag of a thread's OS notification; the thread's ID follows.
const TAG_PREFIX: &str = "mustr-thread:";
/// How long a toast stays when the pointer isn't on it.
const TOAST_DURATION: Duration = Duration::from_secs(8);

pub fn init(cx: &mut App) {
    cx.set_app_identity(APP_ID, "mustr");
    watch_settings(cx);
    init_workspaces(cx);
}

/// Opens threads from OS notifications, and gives every workspace the status bar's count.
fn init_workspaces(cx: &mut App) {
    cx.on_system_notification_response(|response: SystemNotificationResponse, cx| {
        if let Some(thread_id) = response.tag.strip_prefix(TAG_PREFIX) {
            open_thread_from_notification(thread_id.to_string(), cx);
        }
    });
    cx.observe_new(|workspace: &mut Workspace, window, cx| {
        let Some(window) = window else {
            return;
        };
        // The first workspace starts the notifier, once the app has installed the threads'
        // backend.
        let notifier = Notifier::global(cx);
        let workspace_handle = cx.entity().downgrade();
        let status = cx.new(|cx| AttentionStatus::new(notifier, workspace_handle, cx));
        workspace.status_bar().update(cx, |status_bar, cx| {
            status_bar.add_left_item(status, window, cx);
        });
        // T3 clears its OS notifications when its window gets focus.
        cx.observe_window_activation(window, |_, window, cx| {
            if window.is_window_active()
                && let Some(notifier) = Notifier::try_global(cx)
            {
                notifier.update(cx, |notifier, cx| notifier.dismiss_delivered(cx));
            }
        })
        .detach();
    })
    .detach();
}

// Settings.

/// Which events notify, from `"notifications"` in mustr's settings file. T3 has every event off
/// until the user turns notifications on; mustr has the three that need the user, or that they
/// wait for, on.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Deserialize, Serialize)]
#[serde(default)]
pub struct NotificationSettings {
    /// A run finished.
    pub completed: bool,
    /// The agent asks to run a command, change files, or use a tool.
    pub needs_approval: bool,
    /// The agent asks a question, or asks for a secret.
    pub needs_input: bool,
    /// A run failed.
    pub failed: bool,
    /// A run stopped at the agent's usage limit.
    pub usage_limit: bool,
    /// Whether notifications play a sound (on macOS).
    pub sound: bool,
    /// Whether the Dock's icon shows how many threads wait on the user (on macOS).
    pub dock_badge: bool,
}

impl Default for NotificationSettings {
    fn default() -> Self {
        Self {
            completed: true,
            needs_approval: true,
            needs_input: true,
            failed: false,
            usage_limit: false,
            sound: false,
            dock_badge: true,
        }
    }
}

impl Global for NotificationSettings {}

impl NotificationSettings {
    /// The settings in mustr's settings file (JSON with comments). A file without
    /// `"notifications"` has the defaults.
    pub fn from_settings_text(text: &str) -> Result<Self> {
        if text.trim().is_empty() {
            return Ok(Self::default());
        }
        let settings: serde_json::Value = settings::parse_json_with_comments(text)?;
        match settings.get("notifications") {
            None | Some(serde_json::Value::Null) => Ok(Self::default()),
            Some(notifications) => serde_json::from_value(notifications.clone())
                .context("\"notifications\" in mustr's settings"),
        }
    }

    pub fn allows(&self, kind: Kind) -> bool {
        match kind {
            Kind::Completed => self.completed,
            Kind::NeedsApproval => self.needs_approval,
            Kind::NeedsInput => self.needs_input,
            Kind::Failed => self.failed,
            Kind::UsageLimit => self.usage_limit,
        }
    }

    pub fn get(cx: &App) -> Self {
        cx.try_global::<Self>().copied().unwrap_or_default()
    }
}

/// Changes the notification settings, at once and in the settings file, for the Settings page.
pub fn set_settings(settings: NotificationSettings, cx: &mut App) {
    let old = NotificationSettings::get(cx);
    apply_settings(settings, cx);
    let (Ok(serde_json::Value::Object(old)), Ok(serde_json::Value::Object(new))) =
        (serde_json::to_value(old), serde_json::to_value(settings))
    else {
        return;
    };
    for (key, value) in new {
        if old.get(&key) != Some(&value) {
            let path: &'static [&'static str] = match key.as_str() {
                "completed" => &["notifications", "completed"],
                "needs_approval" => &["notifications", "needs_approval"],
                "needs_input" => &["notifications", "needs_input"],
                "failed" => &["notifications", "failed"],
                "usage_limit" => &["notifications", "usage_limit"],
                "sound" => &["notifications", "sound"],
                "dock_badge" => &["notifications", "dock_badge"],
                _ => continue,
            };
            crate::app_settings::update_mustr_setting(path, Some(value), cx);
        }
    }
}

fn apply_settings(settings: NotificationSettings, cx: &mut App) {
    cx.set_global(settings);
    if let Some(notifier) = Notifier::try_global(cx) {
        notifier.update(cx, |notifier, cx| notifier.refresh_badge(cx));
    }
}

/// Keeps [`NotificationSettings`] current with the settings file. An invalid file keeps the
/// last settings that parsed.
fn watch_settings(cx: &mut App) {
    let (mut contents, watcher) = settings::watch_config_file(
        cx.background_executor(),
        <dyn fs::Fs>::global(cx),
        paths::settings_file().clone(),
    );
    cx.spawn(async move |cx| {
        let _watcher = watcher;
        while let Some(text) = contents.next().await {
            match NotificationSettings::from_settings_text(&text) {
                Ok(settings) => cx.update(|cx| apply_settings(settings, cx)),
                Err(error) => log::error!("couldn't read the notification settings: {error:#}"),
            }
        }
    })
    .detach();
}

// What changed.

/// What a thread does, as T3's sidebar says (`SidebarThreadStatus`), plus the coordinator's own
/// "failed" for a ready thread whose last run failed.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Status {
    Approval,
    Input,
    Waiting,
    Working,
    Failed,
    Limited,
    Ready,
}

/// What the user is told about.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Kind {
    Completed,
    NeedsApproval,
    NeedsInput,
    Failed,
    UsageLimit,
}

impl Kind {
    /// The notification's headline, T3's words.
    pub fn title(self) -> &'static str {
        match self {
            Self::Completed => "Thread completed",
            Self::NeedsApproval => "Approval needed",
            Self::NeedsInput => "Input needed",
            Self::Failed => "Thread failed",
            Self::UsageLimit => "Usage limit reached",
        }
    }

    fn for_status(status: Status) -> Option<Self> {
        match status {
            Status::Approval => Some(Self::NeedsApproval),
            Status::Input => Some(Self::NeedsInput),
            Status::Failed => Some(Self::Failed),
            Status::Limited => Some(Self::UsageLimit),
            Status::Waiting | Status::Working | Status::Ready => None,
        }
    }

    fn icon(self) -> IconName {
        match self {
            Self::Completed => IconName::Check,
            Self::NeedsApproval => IconName::Warning,
            Self::NeedsInput => IconName::CircleHelp,
            Self::Failed => IconName::XCircle,
            Self::UsageLimit => IconName::CountdownTimer,
        }
    }

    fn color(self, cx: &App) -> Hsla {
        let palette = mustr_ui::palette(cx);
        match self {
            Self::Completed => palette.ok.hsla(),
            Self::NeedsApproval => palette.warning.hsla(),
            Self::NeedsInput => palette.pending.hsla(),
            Self::Failed | Self::UsageLimit => cx.theme().status().error,
        }
    }
}

/// A subagent's request, rolled up to its parents (`PendingSubagentRequest`).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum SubagentRequest {
    Approval,
    UserInput,
}

/// One thing to tell the user.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Event {
    pub thread_id: String,
    pub title: String,
    pub kind: Kind,
}

/// What [`Transitions`] remembers of a thread between shells (`NotificationState`).
#[derive(Clone, Debug, PartialEq, Eq)]
struct Seen {
    /// The run and status that need the user, such as `run-2:Approval`.
    attention: Option<String>,
    /// When the last completed run finished, in milliseconds.
    completion: Option<i128>,
}

/// Turns the shell's changes into the events T3 notifies for. Nothing notifies for the shell
/// first seen, for a thread first seen, or for archived threads and subagents.
#[derive(Default)]
pub struct Transitions {
    seen: HashMap<String, Seen>,
}

impl Transitions {
    /// What changed since the last shell, oldest thread first. `None` is a shell that isn't
    /// loaded, which forgets every thread, as T3 does while its shell isn't live.
    pub fn update(&mut self, shell: Option<&Shell>) -> Vec<Event> {
        let Some(shell) = shell else {
            self.seen.clear();
            return Vec::new();
        };
        let requests = subagent_requests(shell);
        let mut seen = HashMap::default();
        let mut events = Vec::new();
        for thread in shell.threads.values() {
            if is_subagent(thread) {
                continue;
            }
            let prior = self.seen.get(&thread.id);
            let rolled_up = requests.get(&thread.id);
            let status = status(thread, rolled_up.map(|rolled_up| rolled_up.request));
            let attention = Kind::for_status(status).map(|_| {
                format!(
                    "{}:{status:?}",
                    thread.latest_run_id.as_deref().unwrap_or_default()
                )
            });
            let completion = if status == Status::Ready
                && latest_run_status(thread).as_deref() == Some("completed")
                && let Some(completed_at) = millis(thread.latest_run_completed_at.as_deref())
            {
                Some(completed_at)
            } else {
                prior.and_then(|prior| prior.completion)
            };
            let next = Seen {
                attention,
                completion,
            };
            let kind = prior
                .filter(|_| thread.archived_at.is_none())
                .and_then(|prior| {
                    if next.attention.is_some() && next.attention != prior.attention {
                        Kind::for_status(status)
                    } else if next.completion.is_some()
                        && (prior.completion.is_none() || next.completion > prior.completion)
                    {
                        Some(Kind::Completed)
                    } else {
                        None
                    }
                });
            if let Some(kind) = kind {
                events.push((
                    thread.created_at.clone(),
                    Event {
                        thread_id: thread.id.clone(),
                        title: display_title(&thread.title),
                        kind,
                    },
                ));
            }
            seen.insert(thread.id.clone(), next);
        }
        self.seen = seen;
        events.sort_by(|(a_created, a), (b_created, b)| {
            (a_created, &a.thread_id).cmp(&(b_created, &b.thread_id))
        });
        events.into_iter().map(|(_, event)| event).collect()
    }
}

/// A thread waiting on the user: for an approval or an answer, or with a failure the user
/// hasn't seen.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Waiting {
    pub thread_id: String,
    pub title: String,
    pub status: Status,
}

/// The threads waiting on the user: requests first, then failures, each oldest first.
/// Subagents count through their parents.
pub fn waiting(shell: &Shell) -> Vec<Waiting> {
    let requests = subagent_requests(shell);
    let mut waiting = shell
        .threads
        .values()
        .filter(|thread| {
            !is_subagent(thread) && thread.archived_at.is_none() && thread.deleted_at.is_none()
        })
        .filter_map(|thread| {
            let rolled_up = requests.get(&thread.id);
            let status = status(thread, rolled_up.map(|rolled_up| rolled_up.request));
            let (rank, since) = match status {
                // Since the agent asked, or a subagent asked for it.
                Status::Approval | Status::Input => (
                    0,
                    thread
                        .pending_runtime_request
                        .as_ref()
                        .map(|request| request.created_at.clone())
                        .or_else(|| rolled_up.map(|rolled_up| rolled_up.since.clone()))
                        .unwrap_or_else(|| thread.updated_at.clone()),
                ),
                Status::Failed | Status::Limited
                    if !is_settled(thread) && has_unseen_failure(thread) =>
                {
                    (
                        1,
                        thread
                            .latest_run_completed_at
                            .clone()
                            .unwrap_or_else(|| thread.updated_at.clone()),
                    )
                }
                _ => return None,
            };
            let since = millis(Some(&since)).unwrap_or_default();
            Some((
                (rank, since, thread.id.clone()),
                Waiting {
                    thread_id: thread.id.clone(),
                    title: display_title(&thread.title),
                    status,
                },
            ))
        })
        .collect::<Vec<_>>();
    waiting.sort_by(|(a, _), (b, _)| a.cmp(b));
    waiting.into_iter().map(|(_, waiting)| waiting).collect()
}

/// A thread's status (`resolveSidebarThreadStatus` over `presentThreadShell`), with the
/// coordinator's "failed" for a ready thread whose last run failed.
///
/// T3 0.1.1 always sends `activityRunStatus` and `latestRunCompletedAt`, as `null` when there's
/// none, so a missing one here is a `null` (T3 falls back to other fields for servers that omit
/// them).
fn status(thread: &OrchestrationV2ThreadShell, subagent: Option<SubagentRequest>) -> Status {
    let runtime = runtime_status(thread);
    let working = matches!(
        runtime.as_deref(),
        Some("preparing" | "starting" | "running")
    );
    let request = request_kind(thread);
    let approval = request
        .as_deref()
        .is_some_and(|kind| kind != "user_input" && kind != "auth_refresh");
    let blocking = thread
        .pending_runtime_request
        .as_ref()
        .and_then(|request| request.blocking);
    let input = request.as_deref() == Some("user_input") && (blocking != Some(false) || !working);
    if approval || subagent == Some(SubagentRequest::Approval) {
        return Status::Approval;
    }
    if input || subagent == Some(SubagentRequest::UserInput) {
        return Status::Input;
    }
    let status = match runtime.as_deref() {
        Some("running") if thread.blocked_on_thread_wait == Some(true) => Status::Waiting,
        Some("preparing" | "queued" | "starting" | "running" | "waiting") => Status::Working,
        Some("idle") => Status::Waiting,
        Some("failed")
            if thread.last_error_class == Some(OrchestrationV2ProviderFailureClass::UsageLimit) =>
        {
            Status::Limited
        }
        Some("failed") => Status::Failed,
        _ => Status::Ready,
    };
    if status == Status::Ready && latest_run_status(thread).as_deref() == Some("failed") {
        Status::Failed
    } else {
        status
    }
}

/// The runtime's status (`shellRuntime`): `None` for a thread that never ran, `idle` while
/// background work the agent waits on holds its completion.
fn runtime_status(thread: &OrchestrationV2ThreadShell) -> Option<String> {
    let status = wire(&thread.status);
    let park_at_idle = background_work_holds_completion(thread) && status != "failed";
    if thread.latest_run_id.is_none() && thread.active_provider_thread_id.is_none() && !park_at_idle
    {
        return None;
    }
    if park_at_idle {
        return Some("idle".to_string());
    }
    Some(match &thread.activity_run_status {
        Some(activity) => activity.as_str().to_string(),
        None => status,
    })
}

/// The latest run's status (`latestRun.status` in `presentThreadShell`).
fn latest_run_status(thread: &OrchestrationV2ThreadShell) -> Option<String> {
    thread.latest_run_id.as_ref()?;
    let status = wire(&thread.status);
    Some(if status == "idle" {
        "completed".to_string()
    } else {
        status
    })
}

/// Whether the agent waits on a subagent, a monitor or a background task
/// (`backgroundWorkHoldsCompletion` in T3 0.1.1); a command it left running, such as a dev
/// server, doesn't count.
fn background_work_holds_completion(thread: &OrchestrationV2ThreadShell) -> bool {
    thread
        .pending_background_tasks
        .iter()
        .flatten()
        .any(|task| {
            let task = serde_json::to_value(task).unwrap_or_default();
            matches!(
                task["kind"].as_str(),
                Some("subagent" | "monitor" | "background_task")
            )
        })
}

/// The kind of the thread's pending request, as T3 sends it. T3 sends a secret request as a
/// blocking `user_input` request.
fn request_kind(thread: &OrchestrationV2ThreadShell) -> Option<String> {
    thread
        .pending_runtime_request
        .as_ref()
        .map(|request| wire(&request.kind))
}

/// A subagent's request, as its parents show it.
#[derive(Clone, Debug, PartialEq, Eq)]
struct RolledUp {
    request: SubagentRequest,
    /// When the subagent asked.
    since: String,
}

/// The requests subagents roll up to their parents and their parents' parents
/// (`pendingSubagentRequestsByThread`). An approval wins over a question.
fn subagent_requests(shell: &Shell) -> HashMap<String, RolledUp> {
    let mut requests: HashMap<String, RolledUp> = HashMap::default();
    for child in shell.threads.values() {
        let (Some(request), Some(pending)) = (
            subagent_request(child),
            child.pending_runtime_request.as_ref(),
        ) else {
            continue;
        };
        let mut visited: HashSet<String> = HashSet::from_iter([child.id.clone()]);
        let mut current = Some(child);
        while let Some(thread) = current.filter(|thread| is_subagent(thread)) {
            let Some(parent_id) = thread.lineage.parent_thread_id.clone() else {
                break;
            };
            if !visited.insert(parent_id.clone()) {
                break;
            }
            let shown = requests.get(&parent_id).map(|rolled_up| rolled_up.request);
            if shown != Some(SubagentRequest::Approval) {
                let rolled_up = RolledUp {
                    request,
                    since: pending.created_at.clone(),
                };
                requests.insert(parent_id.clone(), rolled_up);
            }
            current = shell.threads.get(&parent_id);
        }
    }
    requests
}

/// What a subagent asks its parents to show (`subagentRequest`). Subagents native to the
/// provider ask on their parent already, and a request outliving the subagent's run doesn't
/// hold the parent up.
fn subagent_request(thread: &OrchestrationV2ThreadShell) -> Option<SubagentRequest> {
    let kind = request_kind(thread)?;
    let native = thread.creation_source == OrchestrationV2CreationSource::Provider;
    let live = thread.activity_run_status.is_some();
    if kind == "auth_refresh" || !is_subagent(thread) || native || !live {
        return None;
    }
    Some(if kind == "user_input" {
        SubagentRequest::UserInput
    } else {
        SubagentRequest::Approval
    })
}

fn is_subagent(thread: &OrchestrationV2ThreadShell) -> bool {
    thread.lineage.relationship_to_parent
        == Some(OrchestrationV2AppThreadLineageRelationshipToParent::Subagent)
}

fn is_settled(thread: &OrchestrationV2ThreadShell) -> bool {
    thread.settled_override == Some(OrchestrationV2ThreadShellSettledOverride::Settled)
}

/// Whether the last run ended after the user last looked, as the sidebar's Failed pill counts
/// it: a thread the user never opened hasn't been seen.
fn has_unseen_failure(thread: &OrchestrationV2ThreadShell) -> bool {
    match (
        millis(thread.latest_run_completed_at.as_deref()),
        millis(thread.last_visited_at.as_deref()),
    ) {
        (Some(ended), Some(visited)) => ended > visited,
        _ => true,
    }
}

fn display_title(title: &str) -> String {
    if title.trim().is_empty() {
        "Untitled thread".to_string()
    } else {
        title.to_string()
    }
}

/// A generated enum's value as T3 sends it.
fn wire(value: &impl Serialize) -> String {
    match serde_json::to_value(value) {
        Ok(serde_json::Value::String(text)) => text,
        Ok(other) => other.to_string(),
        Err(_) => String::new(),
    }
}

/// Milliseconds since the epoch of one of T3's timestamps.
fn millis(timestamp: Option<&str>) -> Option<i128> {
    let timestamp = OffsetDateTime::parse(timestamp?, &Rfc3339).ok()?;
    Some(timestamp.unix_timestamp_nanos() / 1_000_000)
}

// Delivery.

/// What the user is looking at: the focused mustr window's workspace, and the threads its panes
/// show.
#[derive(Default)]
struct Focus {
    workspace: Option<Entity<Workspace>>,
    threads: HashSet<String>,
}

impl Focus {
    /// Asks each window whether it has focus, as Zed's agent panel does before notifying: on
    /// macOS that's the key window, which an app in the background doesn't have
    /// (`App::active_window` is the main window there, which it keeps).
    fn current(cx: &mut App) -> Self {
        for window in cx.windows() {
            let Some(window) = window.downcast::<MultiWorkspace>() else {
                continue;
            };
            let focus = window.update(cx, |multi_workspace, window, cx| {
                if !window.is_window_active() {
                    return None;
                }
                let workspace = multi_workspace.workspace().clone();
                let threads = workspace
                    .read(cx)
                    .panes()
                    .iter()
                    .filter_map(|pane| {
                        let view = pane.read(cx).active_item()?.downcast::<ThreadView>()?;
                        view.read(cx).thread_id().map(str::to_string)
                    })
                    .collect();
                Some(Self {
                    workspace: Some(workspace),
                    threads,
                })
            });
            if let Ok(Some(focus)) = focus {
                return focus;
            }
        }
        Self::default()
    }
}

/// How a notification reached the user, for tests and logs.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Delivery {
    /// The user is looking at the thread, so nothing was shown.
    Skipped,
    Toast,
    System,
}

/// The app's notifier: it follows the shell, notifies, and keeps the count of threads waiting
/// on the user for the status bar and the Dock.
pub struct Notifier {
    transitions: Transitions,
    waiting: Vec<Waiting>,
    /// Tags of the OS notifications shown since mustr last had focus.
    delivered: Vec<SharedString>,
    /// The count last put on the Dock.
    badge: usize,
    #[cfg(test)]
    pub(crate) deliveries: Vec<(Event, Delivery)>,
    #[cfg(test)]
    pub(crate) sounds: Vec<Kind>,
    _observe: Subscription,
}

struct GlobalNotifier(Entity<Notifier>);

impl Global for GlobalNotifier {}

impl Notifier {
    /// The app's notifier, which starts following the shell on first use. Call it once the
    /// threads' backend is installed.
    pub fn global(cx: &mut App) -> Entity<Self> {
        if let Some(notifier) = Self::try_global(cx) {
            return notifier;
        }
        let shells = ThreadShells::global(cx);
        let notifier = cx.new(|cx| {
            let observe = cx.observe(&shells, |this: &mut Self, shells, cx| {
                this.shell_changed(&shells, cx)
            });
            let mut notifier = Self {
                transitions: Transitions::default(),
                waiting: Vec::new(),
                delivered: Vec::new(),
                badge: 0,
                #[cfg(test)]
                deliveries: Vec::new(),
                #[cfg(test)]
                sounds: Vec::new(),
                _observe: observe,
            };
            notifier.shell_changed(&shells, cx);
            notifier
        });
        cx.set_global(GlobalNotifier(notifier.clone()));
        notifier
    }

    pub fn try_global(cx: &App) -> Option<Entity<Self>> {
        cx.try_global::<GlobalNotifier>()
            .map(|global| global.0.clone())
    }

    /// The threads waiting on the user.
    pub fn waiting(&self) -> &[Waiting] {
        &self.waiting
    }

    fn shell_changed(&mut self, shells: &Entity<ThreadShells>, cx: &mut Context<Self>) {
        let (events, waiting) = self.track(shells.read(cx).shell());
        self.deliver(events, waiting, cx);
    }

    /// Plays a shell to the notifier as if [`ThreadShells`] had it.
    #[cfg(test)]
    pub(crate) fn apply(&mut self, shell: Option<&Shell>, cx: &mut Context<Self>) {
        let (events, waiting) = self.track(shell);
        self.deliver(events, waiting, cx);
    }

    fn track(&mut self, shell: Option<&Shell>) -> (Vec<Event>, Vec<Waiting>) {
        let events = self.transitions.update(shell);
        (events, shell.map(waiting).unwrap_or_default())
    }

    fn deliver(&mut self, events: Vec<Event>, waiting: Vec<Waiting>, cx: &mut Context<Self>) {
        if waiting != self.waiting {
            self.waiting = waiting;
            self.refresh_badge(cx);
            cx.notify();
        }
        let settings = NotificationSettings::get(cx);
        for event in events {
            if settings.allows(event.kind) {
                self.notify(event, settings.sound, cx);
            }
        }
    }

    fn notify(&mut self, event: Event, sound: bool, cx: &mut Context<Self>) {
        let focus = Focus::current(cx);
        let delivery = match &focus.workspace {
            Some(_) if focus.threads.contains(&event.thread_id) => Delivery::Skipped,
            Some(workspace) => {
                show_toast(workspace, &event, cx);
                Delivery::Toast
            }
            None => {
                let tag = SharedString::from(format!("{TAG_PREFIX}{}", event.thread_id));
                cx.show_system_notification(SystemNotification {
                    tag: tag.clone(),
                    title: event.kind.title().into(),
                    body: event.title.clone().into(),
                    actions: Vec::new(),
                });
                if !self.delivered.contains(&tag) {
                    self.delivered.push(tag);
                }
                Delivery::System
            }
        };
        log::info!(
            "{} for thread {}: {delivery:?}",
            event.kind.title(),
            event.thread_id
        );
        if sound && delivery != Delivery::Skipped {
            self.play_sound(event.kind);
        }
        #[cfg(test)]
        self.deliveries.push((event, delivery));
    }

    /// Removes the OS notifications shown since mustr last had focus.
    fn dismiss_delivered(&mut self, cx: &mut Context<Self>) {
        for tag in self.delivered.drain(..) {
            cx.dismiss_system_notification(&tag);
        }
    }

    /// Shows the count of waiting threads on the Dock, unless the settings turn it off.
    fn refresh_badge(&mut self, cx: &App) {
        let shown = if NotificationSettings::get(cx).dock_badge {
            self.waiting.len()
        } else {
            0
        };
        self.set_badge(shown);
    }

    fn set_badge(&mut self, count: usize) {
        if self.badge != count {
            self.badge = count;
            platform::set_badge(count);
        }
    }

    fn play_sound(&mut self, kind: Kind) {
        #[cfg(test)]
        self.sounds.push(kind);
        platform::play_sound(kind == Kind::Completed);
    }

    /// The count on the Dock.
    #[cfg(test)]
    pub(crate) fn badge(&self) -> usize {
        self.badge
    }
}

/// Opens the thread an OS notification was about, in the focused window or the first one, and
/// brings mustr to the front. With every window closed (macOS keeps running), it opens one.
fn open_thread_from_notification(thread_id: String, cx: &mut App) {
    cx.activate(true);
    let title: SharedString = ThreadShells::global(cx)
        .read(cx)
        .thread(&thread_id)
        .map(|thread| display_title(&thread.title))
        .unwrap_or_else(|| "Agent thread".to_string())
        .into();
    let window = cx
        .active_window()
        .and_then(|window| window.downcast::<MultiWorkspace>())
        .or_else(|| {
            cx.windows()
                .into_iter()
                .find_map(|window| window.downcast::<MultiWorkspace>())
        });
    match window {
        Some(window) => {
            window
                .update(cx, |multi_workspace, window, cx| {
                    window.activate_window();
                    let workspace = multi_workspace.workspace().downgrade();
                    thread_chat::open_thread(thread_id, title, workspace, window, cx);
                })
                .ok();
        }
        None => {
            if let Some(app_state) = AppState::try_global(cx) {
                cx.set_global(crate::window::StartupThreads {
                    open_thread: Some(thread_id),
                    new_thread: false,
                });
                crate::window::open_home_window(app_state, cx);
            }
        }
    }
}

fn show_toast(workspace: &Entity<Workspace>, event: &Event, cx: &mut App) {
    let id = NotificationId::composite::<ThreadToast>(SharedString::from(event.thread_id.clone()));
    let event = event.clone();
    workspace.update(cx, |workspace, cx| {
        let workspace_handle = cx.entity().downgrade();
        workspace.show_notification(id, cx, |cx| {
            cx.new(|cx| ThreadToast::new(event, workspace_handle, cx))
        });
    });
}

/// The in-app toast: what happened, the thread's title, and Open.
pub struct ThreadToast {
    event: Event,
    workspace: WeakEntity<Workspace>,
    focus_handle: FocusHandle,
    hovered: bool,
    _dismiss: Task<()>,
}

impl ThreadToast {
    fn new(event: Event, workspace: WeakEntity<Workspace>, cx: &mut Context<Self>) -> Self {
        let dismiss = cx.spawn(async move |this, cx| {
            loop {
                cx.background_executor().timer(TOAST_DURATION).await;
                let dismissed = this.update(cx, |this, cx| {
                    if !this.hovered {
                        cx.emit(DismissEvent);
                    }
                    !this.hovered
                });
                if dismissed.unwrap_or(true) {
                    return;
                }
            }
        });
        Self {
            event,
            workspace,
            focus_handle: cx.focus_handle(),
            hovered: false,
            _dismiss: dismiss,
        }
    }

    #[cfg(test)]
    pub(crate) fn event(&self) -> &Event {
        &self.event
    }

    pub(crate) fn open(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        thread_chat::open_thread(
            self.event.thread_id.clone(),
            self.event.title.clone().into(),
            self.workspace.clone(),
            window,
            cx,
        );
        cx.emit(DismissEvent);
    }
}

impl EventEmitter<DismissEvent> for ThreadToast {}
impl EventEmitter<SuppressEvent> for ThreadToast {}

impl Focusable for ThreadToast {
    fn focus_handle(&self, _: &App) -> FocusHandle {
        self.focus_handle.clone()
    }
}

impl Notification for ThreadToast {}

impl Render for ThreadToast {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let palette = mustr_ui::palette(cx);
        let kind = self.event.kind;
        h_flex()
            .id("thread-toast")
            .occlude()
            .w_full()
            .gap(scaled(px(10.)))
            .px(scaled(px(12.)))
            .py(scaled(px(10.)))
            .rounded(scaled(tokens::CORNER_RADIUS))
            .border(tokens::BORDER_WIDTH)
            .border_color(palette.border.hsla())
            .bg(palette.surface_2.hsla())
            .shadow_md()
            .on_hover(cx.listener(|this, hovered: &bool, _, _| this.hovered = *hovered))
            .child(
                Icon::new(kind.icon())
                    .size(IconSize::Small)
                    .color(Color::Custom(kind.color(cx))),
            )
            .child(
                v_flex()
                    .flex_1()
                    .min_w_0()
                    .gap(scaled(px(2.)))
                    .child(
                        div()
                            .text_size(tokens::text_size(tokens::TEXT_SIZE_MEDIUM))
                            .font_weight(FontWeight::SEMIBOLD)
                            .text_color(palette.text.hsla())
                            .child(kind.title()),
                    )
                    .child(
                        div()
                            .text_size(tokens::text_size(tokens::TEXT_SIZE_SMALL))
                            .text_color(palette.text_muted.hsla())
                            .truncate()
                            .child(self.event.title.clone()),
                    ),
            )
            .child(
                TextButton::new("open-thread", "Open")
                    .style(ButtonStyle::Primary)
                    .on_click(cx.listener(|this, _, window, cx| this.open(window, cx))),
            )
            .child(
                IconButton::new("dismiss", IconName::Close)
                    .icon_size(IconSize::Small)
                    .tooltip(Tooltip::text("Dismiss"))
                    .on_click(cx.listener(|_, _, _, cx| cx.emit(DismissEvent))),
            )
    }
}

/// The status bar's count of threads waiting on the user. Clicking it opens the next one.
pub struct AttentionStatus {
    notifier: Entity<Notifier>,
    workspace: WeakEntity<Workspace>,
    /// The thread the workspace shows, so a click moves on from it.
    active_thread: Option<String>,
    _observe: Subscription,
}

impl AttentionStatus {
    fn new(
        notifier: Entity<Notifier>,
        workspace: WeakEntity<Workspace>,
        cx: &mut Context<Self>,
    ) -> Self {
        let observe = cx.observe(&notifier, |_, _, cx| cx.notify());
        Self {
            notifier,
            workspace,
            active_thread: None,
            _observe: observe,
        }
    }

    pub fn label(count: usize) -> String {
        match count {
            1 => "1 thread needs you".to_string(),
            count => format!("{count} threads need you"),
        }
    }

    /// The waiting thread after the one the workspace shows, or the first.
    fn next(&self, cx: &App) -> Option<Waiting> {
        let waiting = self.notifier.read(cx).waiting();
        let current = self.active_thread.as_ref().and_then(|active| {
            waiting
                .iter()
                .position(|thread| &thread.thread_id == active)
        });
        match current {
            Some(index) => waiting.get((index + 1) % waiting.len()).cloned(),
            None => waiting.first().cloned(),
        }
    }

    pub(crate) fn open_next(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if let Some(next) = self.next(cx) {
            thread_chat::open_thread(
                next.thread_id,
                next.title.into(),
                self.workspace.clone(),
                window,
                cx,
            );
        }
    }
}

impl Render for AttentionStatus {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let palette = mustr_ui::palette(cx);
        let count = self.notifier.read(cx).waiting().len();
        h_flex().when(count > 0, |this| {
            this.child(
                h_flex()
                    .id("threads-waiting")
                    .cursor_pointer()
                    .px(scaled(px(5.)))
                    .gap(scaled(px(5.)))
                    .rounded(scaled(tokens::KBD_RADIUS))
                    .hover(|style| style.bg(palette.surface_2.hsla()))
                    .child(
                        div()
                            .flex_none()
                            .size(scaled(px(6.)))
                            .rounded_full()
                            .bg(palette.warning.hsla()),
                    )
                    .child(
                        div()
                            .text_size(tokens::text_size(tokens::TEXT_SIZE_XSMALL))
                            .text_color(palette.text.hsla())
                            .whitespace_nowrap()
                            .child(Self::label(count)),
                    )
                    .tooltip(Tooltip::text("Open the next thread waiting on you"))
                    .on_click(cx.listener(|this, _, window, cx| this.open_next(window, cx))),
            )
        })
    }
}

impl StatusItemView for AttentionStatus {
    fn set_active_pane_item(
        &mut self,
        item: Option<&dyn ItemHandle>,
        _: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.active_thread = item
            .and_then(|item| item.downcast::<ThreadView>())
            .and_then(|view| view.read(cx).thread_id().map(str::to_string));
    }

    fn hide_setting(&self, _: &App) -> Option<HideStatusItem> {
        None
    }
}

/// The Dock's badge and the notification sounds, on macOS. Linux has no app badge Zed or GPUI
/// can set, and GPUI plays no sounds there.
#[cfg(target_os = "macos")]
mod platform {
    use objc2::MainThreadMarker;
    use objc2_app_kit::{NSApplication, NSSound};
    use objc2_foundation::NSString;

    pub fn set_badge(count: usize) {
        let Some(main_thread) = MainThreadMarker::new() else {
            log::error!("the Dock badge can only be set on the main thread");
            return;
        };
        let label = (count > 0).then(|| NSString::from_str(&count.to_string()));
        NSApplication::sharedApplication(main_thread)
            .dockTile()
            .setBadgeLabel(label.as_deref());
    }

    /// One of macOS's own sounds: Glass when a run finishes, Ping when the agent needs the user.
    /// A sound that's still playing isn't played again, so threads finishing together sound once.
    pub fn play_sound(completed: bool) {
        let name = NSString::from_str(if completed { "Glass" } else { "Ping" });
        if let Some(sound) = NSSound::soundNamed(&name)
            && !sound.isPlaying()
        {
            sound.play();
        }
    }
}

#[cfg(not(target_os = "macos"))]
mod platform {
    pub fn set_badge(_count: usize) {}

    pub fn play_sound(_completed: bool) {}
}

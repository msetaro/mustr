//! The scheduled tasks page (`mustr-docs/PARITY.md` §2.14): every task T3 runs on its own, on a
//! schedule or when a webhook is called, whether the user made it or an agent did with T3's
//! `schedule_task` tool. Each shows when it runs and ran, how the last run went, and where it
//! runs, and can be edited, paused, run now, or deleted. Webhook triggers are shown, with their
//! URL masked and copyable, but are edited in T3 Code.
//!
//! The list follows `scheduledTasks.subscribe`; changes go through `scheduledTasks.upsert`,
//! `setEnabled`, `runNow`, and `delete`, on the installed thread backend, so dry runs and the
//! pairing's scopes apply. It's opened from the rail.

mod draft;
mod editor;
#[cfg(test)]
mod tests;

use crate::{nav_panel::register_rail_entry, t3_connection::T3Connection};
use draft::{
    Draft, delivery_label, mask_webhook_address, next_run_label, relative, schedule_label,
};
use editor::{Choices, ProjectChoice, TaskEditor, TaskEditorEvent, ThreadChoice};
use futures::StreamExt as _;
use gpui::{
    AnyElement, App, AppContext as _, ClipboardItem, Context, Entity, EventEmitter, FocusHandle,
    Focusable, FontWeight, Hsla, InteractiveElement as _, IntoElement, ParentElement as _, Render,
    SharedString, Styled as _, Subscription, Task, Window, div, px,
};
use mustr_ui::{ButtonStyle, Card, Pill, TextButton, palette, tokens, tokens::scaled};
use std::{
    collections::{HashMap, HashSet},
    sync::Arc,
    time::Duration,
};
use t3_client::t3_api::{
    rpc,
    tagged::{OrchestrationV2ThreadLaunchWorkspaceStrategy, ScheduledTaskSchedule},
    types::{
        ScheduledTask, ScheduledTaskDeleteInput, ScheduledTaskListInput,
        ScheduledTaskListWebhookDeliveriesInput, ScheduledTaskRunNowInput, ScheduledTaskRunStatus,
        ScheduledTaskSetEnabledInput, ScheduledTaskWebhookDeliverySummary,
        ScheduledTaskWebhookDeliverySummaryOutcome,
    },
};
use thread_chat::{Access, AgentChoice, ThreadBackend, ThreadBackendExt as _, ThreadShells};
use time::OffsetDateTime;
use ui::{
    Color, Headline, HeadlineSize, Icon, IconName, Label, LabelCommon as _, LabelSize, Switch,
    ToggleState, Tooltip, prelude::*,
};
use url::Url;
use workspace::{
    Workspace,
    item::{Item, ItemEvent},
};

const CONTENT_MAX_WIDTH: f32 = 880.;
/// Webhook deliveries shown at once, newest first.
const DELIVERIES_SHOWN: usize = 10;
/// How often relative times ("in 3h") are brought up to date.
const CLOCK_TICK: Duration = Duration::from_secs(30);

gpui::actions!(
    mustr,
    [
        /// Opens the scheduled tasks.
        OpenScheduledTasks,
    ]
);

/// Adds the page's rail entry and action. Call once at startup, before windows open.
pub fn init(cx: &mut App) {
    register_rail_entry(
        "schedules",
        Icon::new(IconName::Clock),
        "Scheduled tasks",
        OpenScheduledTasks,
        |item| item.downcast::<SchedulesView>().is_some(),
        cx,
    );
    cx.observe_new(|workspace: &mut Workspace, _, _| {
        workspace.register_action(|workspace, _: &OpenScheduledTasks, window, cx| {
            show(workspace, window, cx)
        });
    })
    .detach();
}

/// Activates the scheduled tasks page, or opens it.
pub fn show(workspace: &mut Workspace, window: &mut Window, cx: &mut Context<Workspace>) {
    let existing = workspace
        .panes()
        .iter()
        .find_map(|pane| pane.read(cx).items_of_type::<SchedulesView>().next());
    if let Some(existing) = existing {
        workspace.activate_item(&existing, true, true, window, cx);
        return;
    }
    let backend = thread_chat::backend(cx);
    let view = cx.new(|cx| {
        SchedulesView::new(
            backend,
            Box::new(|cx| T3Connection::global(cx).read(cx).server_url().cloned()),
            window,
            cx,
        )
    });
    workspace.add_item_to_active_pane(Box::new(view), None, true, window, cx);
}

enum Loadable<T> {
    Loading,
    Failed(SharedString),
    Loaded(T),
}

/// T3's server, for webhook addresses that are a path on it.
type ServerUrl = Box<dyn Fn(&App) -> Option<Url>>;

pub struct SchedulesView {
    focus_handle: FocusHandle,
    backend: Option<Arc<dyn ThreadBackend>>,
    server_url: ServerUrl,
    shells: Entity<ThreadShells>,
    tasks: Loadable<Vec<ScheduledTask>>,
    agents: Vec<AgentChoice>,
    editor: Option<Entity<TaskEditor>>,
    /// Tasks with a change in flight, by id.
    busy: HashSet<String>,
    /// The task whose Delete asks to confirm.
    confirming_delete: Option<String>,
    /// The task whose webhook URL was just copied.
    copied: Option<String>,
    /// Webhook delivery logs shown, by task id.
    deliveries: HashMap<String, Loadable<Vec<ScheduledTaskWebhookDeliverySummary>>>,
    error: Option<SharedString>,
    _follow: Option<Task<()>>,
    _tasks: Vec<Task<()>>,
    _subscriptions: Vec<Subscription>,
}

impl SchedulesView {
    fn new(
        backend: Option<Arc<dyn ThreadBackend>>,
        server_url: ServerUrl,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        let shells = ThreadShells::global(cx);
        let subscriptions = vec![cx.observe(&shells, |_, _, cx| cx.notify())];
        let mut this = Self {
            focus_handle: cx.focus_handle(),
            backend,
            server_url,
            shells,
            tasks: Loadable::Loading,
            agents: Vec::new(),
            editor: None,
            busy: HashSet::default(),
            confirming_delete: None,
            copied: None,
            deliveries: HashMap::default(),
            error: None,
            _follow: None,
            _tasks: Vec::new(),
            _subscriptions: subscriptions,
        };
        this.follow(cx);
        this.load_agents(cx);
        this._tasks.push(cx.spawn_in(window, async move |this, cx| {
            loop {
                cx.background_executor().timer(CLOCK_TICK).await;
                if this.update(cx, |_, cx| cx.notify()).is_err() {
                    return;
                }
            }
        }));
        this
    }

    /// Follows `scheduledTasks.subscribe`, which sends the whole list whenever it changes, and
    /// subscribes again when the stream ends, as it does when mustr reconnects.
    fn follow(&mut self, cx: &mut Context<Self>) {
        let Some(backend) = self.backend.clone() else {
            self.tasks = Loadable::Failed("mustr isn't connected to T3".into());
            return;
        };
        self._follow = Some(cx.spawn(async move |this, cx| {
            let mut attempt = 0;
            loop {
                let stream = backend.stream::<rpc::ScheduledTasksSubscribe>(
                    ScheduledTaskListInput(Default::default()),
                    Access::Read,
                );
                let error = match stream {
                    Ok(mut stream) => loop {
                        match stream.next().await {
                            Some(Ok(list)) => {
                                attempt = 0;
                                let updated = this.update(cx, |this, cx| {
                                    this.tasks = Loadable::Loaded(sorted(list.tasks));
                                    cx.notify();
                                });
                                if updated.is_err() {
                                    return;
                                }
                            }
                            Some(Err(error)) => break Some(error),
                            None => break None,
                        }
                    },
                    Err(error) => Some(error),
                };
                if let Some(error) = error {
                    log::warn!("couldn't follow T3's scheduled tasks: {error:#}");
                    let updated = this.update(cx, |this, cx| {
                        if !matches!(this.tasks, Loadable::Loaded(_)) {
                            this.tasks = Loadable::Failed(format!("{error:#}").into());
                            cx.notify();
                        }
                    });
                    if updated.is_err() {
                        return;
                    }
                }
                cx.background_executor().timer(retry_delay(attempt)).await;
                attempt += 1;
            }
        }));
    }

    fn load_agents(&mut self, cx: &mut Context<Self>) {
        let Some(backend) = self.backend.clone() else {
            return;
        };
        let providers = backend.providers();
        self._tasks
            .push(cx.spawn(async move |this, cx| match providers.await {
                Ok(providers) => {
                    this.update(cx, |this, cx| {
                        this.agents = thread_chat::agent_choices(&providers);
                        cx.notify();
                    })
                    .ok();
                }
                Err(error) => log::warn!("couldn't list T3's agents: {error:#}"),
            }));
    }

    #[cfg(test)]
    fn tasks(&self) -> &[ScheduledTask] {
        match &self.tasks {
            Loadable::Loaded(tasks) => tasks,
            _ => &[],
        }
    }

    fn choices(&self, cx: &App) -> Choices {
        let shell = self.shells.read(cx).shell();
        let mut projects = shell
            .into_iter()
            .flat_map(|shell| shell.projects.values())
            .map(|project| ProjectChoice {
                id: project.id.clone(),
                title: project.title.clone(),
            })
            .collect::<Vec<_>>();
        projects.sort_by_key(|project| project.title.to_lowercase());
        let mut threads = shell
            .into_iter()
            .flat_map(|shell| shell.threads.values())
            .filter(|thread| thread.archived_at.is_none() && thread.deleted_at.is_none())
            .map(|thread| {
                (
                    thread.updated_at.clone(),
                    ThreadChoice {
                        id: thread.id.clone(),
                        title: if thread.title.trim().is_empty() {
                            "Untitled thread".to_string()
                        } else {
                            thread.title.clone()
                        },
                        project_id: thread.project_id.clone(),
                    },
                )
            })
            .collect::<Vec<_>>();
        // ISO timestamps sort as text; newest first.
        threads.sort_by(|left, right| right.0.cmp(&left.0));
        Choices {
            projects,
            threads: threads.into_iter().map(|(_, thread)| thread).collect(),
            agents: self.agents.clone(),
        }
    }

    fn open_editor(&mut self, draft: Draft, window: &mut Window, cx: &mut Context<Self>) {
        let Some(backend) = self.backend.clone() else {
            return;
        };
        let choices = self.choices(cx);
        let editor = cx.new(|cx| TaskEditor::new(backend, draft, choices, window, cx));
        self._subscriptions.push(
            cx.subscribe(&editor, |this, _, event: &TaskEditorEvent, cx| {
                if let TaskEditorEvent::Saved(task) = event {
                    this.task_changed((**task).clone());
                }
                this.editor = None;
                cx.notify();
            }),
        );
        let title = editor.read(cx).title.clone();
        window.focus(&title.focus_handle(cx), cx);
        self.editor = Some(editor);
        self.confirming_delete = None;
        cx.notify();
    }

    /// Shows a task T3 sent back after a change, before the subscription catches up.
    fn task_changed(&mut self, task: ScheduledTask) {
        if let Loadable::Loaded(tasks) = &mut self.tasks {
            match tasks.iter_mut().find(|existing| existing.id == task.id) {
                Some(existing) => *existing = task,
                None => tasks.push(task),
            }
            *tasks = sorted(std::mem::take(tasks));
        }
    }

    fn set_enabled(&mut self, id: String, enabled: bool, cx: &mut Context<Self>) {
        self.change(id.clone(), cx, move |backend| {
            let call = backend.call::<rpc::ScheduledTasksSetEnabled>(
                ScheduledTaskSetEnabledInput { enabled, id },
                Access::Operate,
            );
            Box::pin(async move { call.await.map(|result| Change::Updated(result.task)) })
        });
    }

    /// Runs a task now (`scheduledTasks.runNow`), which starts an agent run.
    fn run_now(&mut self, id: String, cx: &mut Context<Self>) {
        self.change(id.clone(), cx, move |backend| {
            let call = backend.call::<rpc::ScheduledTasksRunNow>(
                ScheduledTaskRunNowInput { id },
                Access::Operate,
            );
            Box::pin(async move { call.await.map(|result| Change::Updated(result.task)) })
        });
    }

    fn delete(&mut self, id: String, cx: &mut Context<Self>) {
        self.confirming_delete = None;
        self.change(id.clone(), cx, move |backend| {
            let call = backend.call::<rpc::ScheduledTasksDelete>(
                ScheduledTaskDeleteInput { id },
                Access::Operate,
            );
            Box::pin(async move { call.await.map(|result| Change::Deleted(result.id)) })
        });
    }

    fn change(
        &mut self,
        id: String,
        cx: &mut Context<Self>,
        call: impl FnOnce(
            &Arc<dyn ThreadBackend>,
        ) -> futures::future::BoxFuture<'static, anyhow::Result<Change>>,
    ) {
        let Some(backend) = self.backend.clone() else {
            return;
        };
        if !self.busy.insert(id.clone()) {
            return;
        }
        self.error = None;
        let call = call(&backend);
        cx.notify();
        self._tasks.push(cx.spawn(async move |this, cx| {
            let result = call.await;
            this.update(cx, |this, cx| {
                this.busy.remove(&id);
                match result {
                    Ok(Change::Updated(task)) => this.task_changed(task),
                    Ok(Change::Deleted(id)) => {
                        if let Loadable::Loaded(tasks) = &mut this.tasks {
                            tasks.retain(|task| task.id != id);
                        }
                    }
                    Err(error) => {
                        log::warn!("couldn't change scheduled task {id}: {error:#}");
                        this.error = Some(format!("{error:#}").into());
                    }
                }
                cx.notify();
            })
            .ok();
        }));
    }

    fn webhook_address(&self, task: &ScheduledTask, cx: &App) -> Option<String> {
        let endpoint = task.webhook.as_ref()?;
        if let Some(url) = &endpoint.url {
            return Some(url.clone());
        }
        match (self.server_url)(cx) {
            Some(server) => server.join(&endpoint.path).ok().map(String::from),
            None => Some(endpoint.path.clone()),
        }
    }

    /// Shows or hides a webhook's delivery log (`scheduledTasks.listWebhookDeliveries`). T3
    /// guards it like the URL, since it holds request bodies, so dry runs refuse it.
    fn toggle_deliveries(&mut self, id: String, cx: &mut Context<Self>) {
        if self.deliveries.remove(&id).is_some() {
            cx.notify();
            return;
        }
        let Some(backend) = self.backend.clone() else {
            return;
        };
        let call = backend.call::<rpc::ScheduledTasksListWebhookDeliveries>(
            ScheduledTaskListWebhookDeliveriesInput {
                accept_any_outcome: None,
                id: id.clone(),
            },
            Access::Operate,
        );
        self.deliveries.insert(id.clone(), Loadable::Loading);
        cx.notify();
        self._tasks.push(cx.spawn(async move |this, cx| {
            let result = call.await;
            this.update(cx, |this, cx| {
                if let Some(shown) = this.deliveries.get_mut(&id) {
                    *shown = match result {
                        Ok(result) => Loadable::Loaded(result.deliveries),
                        Err(error) => Loadable::Failed(format!("{error:#}").into()),
                    };
                    cx.notify();
                }
            })
            .ok();
        }));
    }

    fn copy_webhook(&mut self, task: &ScheduledTask, cx: &mut Context<Self>) {
        if let Some(address) = self.webhook_address(task, cx) {
            cx.write_to_clipboard(ClipboardItem::new_string(address));
            self.copied = Some(task.id.clone());
            cx.notify();
        }
    }
}

enum Change {
    Updated(ScheduledTask),
    Deleted(String),
}

/// Running tasks first, then by when they run next, then by name.
fn sorted(mut tasks: Vec<ScheduledTask>) -> Vec<ScheduledTask> {
    tasks.sort_by(|left, right| {
        right
            .enabled
            .cmp(&left.enabled)
            .then_with(|| match (&left.next_run_at, &right.next_run_at) {
                (Some(left), Some(right)) => left.cmp(right),
                (Some(_), None) => std::cmp::Ordering::Less,
                (None, Some(_)) => std::cmp::Ordering::Greater,
                (None, None) => std::cmp::Ordering::Equal,
            })
            .then_with(|| left.title.to_lowercase().cmp(&right.title.to_lowercase()))
    });
    tasks
}

fn retry_delay(attempt: usize) -> Duration {
    const DELAYS: [u64; 4] = [1, 2, 5, 10];
    Duration::from_secs(DELAYS[attempt.min(DELAYS.len() - 1)])
}

fn status_color(status: &ScheduledTaskRunStatus, cx: &App) -> Hsla {
    let palette = palette(cx);
    match status {
        ScheduledTaskRunStatus::Succeeded => palette.ok.hsla(),
        ScheduledTaskRunStatus::Failed => cx.theme().status().error,
        ScheduledTaskRunStatus::Running => palette.accent.hsla(),
        _ => palette.text_muted.hsla(),
    }
}

fn render_deliveries(
    deliveries: &Loadable<Vec<ScheduledTaskWebhookDeliverySummary>>,
    now: OffsetDateTime,
    cx: &App,
) -> AnyElement {
    let palette = palette(cx);
    let body = match deliveries {
        Loadable::Loading => muted("Loading deliveries…")
            .size(LabelSize::XSmall)
            .into_any_element(),
        Loadable::Failed(error) => Label::new(error.clone())
            .size(LabelSize::XSmall)
            .color(Color::Error)
            .into_any_element(),
        Loadable::Loaded(deliveries) if deliveries.is_empty() => muted("No requests yet.")
            .size(LabelSize::XSmall)
            .into_any_element(),
        Loadable::Loaded(deliveries) => v_flex()
            .children(deliveries.iter().take(DELIVERIES_SHOWN).map(|delivery| {
                let outcome = match &delivery.outcome {
                    ScheduledTaskWebhookDeliverySummaryOutcome::ScheduledTaskWebhookDeliveryOutcome(
                        outcome,
                    ) => outcome.as_str().to_string(),
                    ScheduledTaskWebhookDeliverySummaryOutcome::String(outcome) => outcome.clone(),
                };
                let failed = matches!(outcome.as_str(), "dispatch_failed" | "rejected_signature");
                let label = delivery_label(&outcome, delivery.pending_window.unwrap_or(false));
                let when = relative(&delivery.received_at, now)
                    .unwrap_or_else(|| delivery.received_at.clone());
                h_flex()
                    .gap_2()
                    .child(muted(when).size(LabelSize::XSmall))
                    .child(muted(delivery.method.clone()).size(LabelSize::XSmall))
                    .child(Label::new(label).size(LabelSize::XSmall).color(if failed {
                        Color::Error
                    } else {
                        Color::Default
                    }))
                    .when_some(delivery.error.clone(), |this, error| {
                        this.child(
                            div().min_w_0().child(
                                Label::new(error)
                                    .size(LabelSize::XSmall)
                                    .color(Color::Error)
                                    .truncate(),
                            ),
                        )
                    })
            }))
            .into_any_element(),
    };
    div()
        .mt_1()
        .px_2()
        .py_1()
        .rounded(scaled(tokens::CONTROL_RADIUS))
        .border(tokens::BORDER_WIDTH)
        .border_color(palette.border.hsla())
        .child(body)
        .into_any_element()
}

fn muted(text: impl Into<SharedString>) -> Label {
    Label::new(text.into())
        .size(LabelSize::Small)
        .color(Color::Muted)
}

impl SchedulesView {
    /// Where a task runs: its project, the thread it messages or a new one each run, and the
    /// agent.
    fn target_line(&self, task: &ScheduledTask, cx: &App) -> String {
        let shell = self.shells.read(cx).shell();
        let project = shell
            .and_then(|shell| shell.projects.get(&task.project_id))
            .map_or_else(
                || "A removed project".to_string(),
                |project| project.title.clone(),
            );
        let thread = match &task.thread_id {
            Some(thread_id) => format!(
                "messages {}",
                shell
                    .and_then(|shell| shell.threads.get(thread_id))
                    .map_or_else(
                        || "a thread T3 no longer lists".to_string(),
                        |thread| { format!("“{}”", thread.title) }
                    )
            ),
            None => match &task.workspace_strategy {
                OrchestrationV2ThreadLaunchWorkspaceStrategy::Worktree(worktree) => {
                    format!(
                        "a new thread each run, in a worktree from {}",
                        worktree.base_ref
                    )
                }
                OrchestrationV2ThreadLaunchWorkspaceStrategy::Root(_) => {
                    "a new thread each run, in the project's checkout".to_string()
                }
                OrchestrationV2ThreadLaunchWorkspaceStrategy::ExistingWorktree(existing) => {
                    format!("a new thread each run, in {}", existing.worktree_path)
                }
                OrchestrationV2ThreadLaunchWorkspaceStrategy::Unrecognized(_) => {
                    "a new thread each run".to_string()
                }
            },
        };
        let instance = task
            .model_selection
            .instance_id
            .as_ref()
            .and_then(|id| id.as_str())
            .unwrap_or_default();
        let agent = self
            .agents
            .iter()
            .find(|agent| agent.instance_id == instance);
        let slug = task.model_selection.model.as_str().unwrap_or_default();
        let model = agent
            .and_then(|agent| agent.models.iter().find(|model| model.slug == *slug))
            .map_or_else(|| slug.to_string(), |model| model.name.clone());
        let agent = agent.map_or_else(|| instance.to_string(), |agent| agent.name.clone());
        let mode = draft::RUNTIME_MODES
            .iter()
            .find(|(mode, _)| *mode == task.runtime_mode)
            .map(|(_, label)| label.to_string())
            .unwrap_or_default();
        [project, thread, agent, model, mode]
            .into_iter()
            .filter(|part| !part.is_empty())
            .collect::<Vec<_>>()
            .join(" · ")
    }

    fn render_task(
        &self,
        index: usize,
        task: &ScheduledTask,
        now: OffsetDateTime,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let palette = palette(cx);
        let id = task.id.clone();
        let busy = self.busy.contains(&task.id);
        let blocked = self
            .backend
            .as_ref()
            .and_then(|backend| backend.blocked_reason(Access::Operate));
        let is_webhook = matches!(task.schedule, ScheduledTaskSchedule::Webhook(_));
        let enabled = task.enabled;
        let this = cx.entity().downgrade();

        let mut details = vec![schedule_label(&task.schedule), next_run_label(task, now)];
        if let Some(when) = task.last_run_at.as_deref().and_then(|at| relative(at, now)) {
            details.push(format!("last ran {when}"));
        }
        if task.run_count > 0 {
            details.push(format!(
                "{} {}",
                task.run_count,
                if task.run_count == 1 { "run" } else { "runs" }
            ));
        }

        let switch = div()
            .id(("schedule-enabled", index))
            .debug_selector(|| format!("schedule-toggle-{id}"))
            .pt(scaled(px(2.)))
            .when_some(blocked.clone(), |this, reason| {
                this.tooltip(Tooltip::text(reason))
            })
            .child(
                Switch::new(
                    ("schedule-switch", index),
                    if enabled {
                        ToggleState::Selected
                    } else {
                        ToggleState::Unselected
                    },
                )
                .disabled(busy || blocked.is_some())
                .on_click({
                    let (this, id) = (this.clone(), id.clone());
                    move |_, _, cx| {
                        let id = id.clone();
                        this.update(cx, |this, cx| this.set_enabled(id, !enabled, cx))
                            .ok();
                    }
                }),
            );

        let button =
            |label: &'static str,
             selector: String,
             tooltip: Option<String>,
             style: ButtonStyle,
             on_click: Box<dyn Fn(&mut Self, &mut Window, &mut Context<Self>)>| {
                let this = this.clone();
                // A dry run can still look at a task in the form; it can't save it.
                let disabled = busy || (blocked.is_some() && !matches!(label, "Cancel" | "Edit"));
                let id_selector = selector.clone();
                div().debug_selector(|| id_selector).child(
                    TextButton::new(SharedString::from(selector), label)
                        .style(style)
                        .disabled(disabled)
                        .when_some(
                            blocked.clone().filter(|_| disabled).or(tooltip),
                            |button, tooltip| button.tooltip(tooltip),
                        )
                        .on_click(move |_, window, cx| {
                            this.update(cx, |this, cx| on_click(this, window, cx)).ok();
                        }),
                )
            };
        let actions = if self.confirming_delete.as_deref() == Some(task.id.as_str()) {
            h_flex()
                .gap_1()
                .child(muted("Delete this task?"))
                .child(button(
                    "Delete",
                    format!("schedule-confirm-delete-{id}"),
                    None,
                    ButtonStyle::Primary,
                    Box::new({
                        let id = id.clone();
                        move |this, _, cx| this.delete(id.clone(), cx)
                    }),
                ))
                .child(button(
                    "Cancel",
                    format!("schedule-cancel-delete-{id}"),
                    None,
                    ButtonStyle::Default,
                    Box::new(|this, _, cx| {
                        this.confirming_delete = None;
                        cx.notify();
                    }),
                ))
        } else {
            h_flex()
                .gap_1()
                .child(button(
                    "Edit",
                    format!("schedule-edit-{id}"),
                    None,
                    ButtonStyle::Default,
                    Box::new({
                        let task = task.clone();
                        move |this, window, cx| {
                            this.open_editor(Draft::from_task(&task), window, cx)
                        }
                    }),
                ))
                .when(!is_webhook, |this| {
                    this.child(button(
                        "Run now",
                        format!("schedule-run-{id}"),
                        Some("Start a run now, which starts the agent".to_string()),
                        ButtonStyle::Default,
                        Box::new({
                            let id = id.clone();
                            move |this, _, cx| this.run_now(id.clone(), cx)
                        }),
                    ))
                })
                .child(button(
                    "Delete",
                    format!("schedule-delete-{id}"),
                    None,
                    ButtonStyle::Default,
                    Box::new({
                        let id = id.clone();
                        move |this, _, cx| {
                            this.confirming_delete = Some(id.clone());
                            cx.notify();
                        }
                    }),
                ))
        };

        let webhook = task.webhook.as_ref().map(|endpoint| {
            let address = self.webhook_address(task, cx).unwrap_or_default();
            let copied = self.copied.as_deref() == Some(task.id.as_str());
            let task = task.clone();
            let copy = this.clone();
            h_flex()
                .gap_2()
                .child(
                    div()
                        .min_w_0()
                        .px_1p5()
                        .rounded(scaled(tokens::CONTROL_RADIUS))
                        .bg(palette.surface_2.hsla())
                        .font_family(tokens::CODE_FONT_FAMILY)
                        .text_size(tokens::text_size(tokens::TEXT_SIZE_XSMALL))
                        .text_color(palette.text_muted.hsla())
                        .child(mask_webhook_address(&address)),
                )
                .child(
                    div()
                        .debug_selector(|| format!("schedule-copy-{}", task.id))
                        .child(
                            TextButton::new(
                                SharedString::from(format!("schedule-copy-{}", task.id)),
                                if copied { "Copied" } else { "Copy URL" },
                            )
                            .tooltip("Copy the webhook URL, with its secret token")
                            .on_click(move |_, _, cx| {
                                copy.update(cx, |this, cx| this.copy_webhook(&task, cx))
                                    .ok();
                            }),
                        ),
                )
                .child({
                    let this = this.clone();
                    let id = id.clone();
                    let shown = self.deliveries.contains_key(&id);
                    let selector = format!("schedule-deliveries-{id}");
                    div().debug_selector(|| selector.clone()).child(
                        TextButton::new(
                            SharedString::from(selector),
                            if shown {
                                "Hide deliveries"
                            } else {
                                "Deliveries"
                            },
                        )
                        .tooltip("The requests this webhook received")
                        .on_click(move |_, _, cx| {
                            let id = id.clone();
                            this.update(cx, |this, cx| this.toggle_deliveries(id, cx))
                                .ok();
                        }),
                    )
                })
                .when(endpoint.has_secret, |this| {
                    this.child(muted("signed requests only").size(LabelSize::XSmall))
                })
        });
        let deliveries = self
            .deliveries
            .get(&task.id)
            .map(|deliveries| render_deliveries(deliveries, now, cx));

        h_flex()
            .id(("schedule-task", index))
            .debug_selector(|| format!("schedule-task-{}", task.id))
            .w_full()
            .items_start()
            .gap_3()
            .px_2()
            .py(scaled(px(10.)))
            .when(index > 0, |this| {
                this.border_t(tokens::BORDER_WIDTH)
                    .border_color(palette.border.hsla())
            })
            .child(switch)
            .child(
                v_flex()
                    .flex_1()
                    .min_w_0()
                    .gap_0p5()
                    .child(
                        h_flex()
                            .gap_2()
                            .child(
                                div().min_w_0().child(
                                    Label::new(task.title.clone())
                                        .weight(FontWeight::SEMIBOLD)
                                        .truncate(),
                                ),
                            )
                            .when(
                                task.last_run_status != ScheduledTaskRunStatus::Never,
                                |this| {
                                    this.child(Pill::tinted(
                                        task.last_run_status.as_str().to_string(),
                                        status_color(&task.last_run_status, cx),
                                    ))
                                },
                            ),
                    )
                    .child(muted(details.join(" · ")))
                    .when_some(task.last_run_error.clone(), |this, error| {
                        this.child(
                            Label::new(error)
                                .size(LabelSize::Small)
                                .color(Color::Error)
                                .truncate(),
                        )
                    })
                    .child(
                        muted(self.target_line(task, cx))
                            .size(LabelSize::XSmall)
                            .truncate(),
                    )
                    .child(
                        muted(task.prompt.lines().next().unwrap_or_default().to_string())
                            .size(LabelSize::XSmall)
                            .italic()
                            .truncate(),
                    )
                    .children(webhook)
                    .children(deliveries),
            )
            .child(actions)
            .into_any_element()
    }

    fn render_list(&self, cx: &mut Context<Self>) -> AnyElement {
        let body = match &self.tasks {
            Loadable::Loading => muted("Loading scheduled tasks…").into_any_element(),
            Loadable::Failed(error) => Label::new(error.clone())
                .size(LabelSize::Small)
                .color(Color::Error)
                .into_any_element(),
            Loadable::Loaded(tasks) if tasks.is_empty() => v_flex()
                .gap_0p5()
                .child(Label::new("No scheduled tasks yet.").size(LabelSize::Small))
                .child(muted(
                    "Create one here, or ask an agent to schedule one with T3's schedule_task tool.",
                ))
                .into_any_element(),
            Loadable::Loaded(tasks) => {
                let now = OffsetDateTime::now_utc();
                v_flex()
                    .children(
                        tasks
                            .iter()
                            .enumerate()
                            .map(|(index, task)| self.render_task(index, task, now, cx)),
                    )
                    .into_any_element()
            }
        };
        Card::new()
            .px(scaled(px(10.)))
            .py(scaled(px(4.)))
            .child(body)
            .into_any_element()
    }
}

impl EventEmitter<ItemEvent> for SchedulesView {}

impl Focusable for SchedulesView {
    fn focus_handle(&self, _: &App) -> FocusHandle {
        self.focus_handle.clone()
    }
}

impl Item for SchedulesView {
    type Event = ItemEvent;

    fn tab_content_text(&self, _detail: usize, _cx: &App) -> SharedString {
        "Scheduled tasks".into()
    }

    fn tab_icon(&self, _window: &Window, _cx: &App) -> Option<Icon> {
        Some(Icon::new(IconName::Clock))
    }

    fn show_toolbar(&self) -> bool {
        false
    }

    fn to_item_events(event: &Self::Event, f: &mut dyn FnMut(ItemEvent)) {
        f(*event)
    }
}

impl Render for SchedulesView {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let palette = palette(cx);
        let count = match &self.tasks {
            Loadable::Loaded(tasks) => Some(tasks.len()),
            _ => None,
        };
        let blocked = self
            .backend
            .as_ref()
            .and_then(|backend| backend.blocked_reason(Access::Operate));
        let header = h_flex()
            .w_full()
            .items_end()
            .gap_3()
            .child(
                v_flex()
                    .flex_1()
                    .min_w_0()
                    .gap_0p5()
                    .child(
                        h_flex()
                            .gap_2()
                            .items_baseline()
                            .child(Headline::new("Scheduled tasks").size(HeadlineSize::Large))
                            .children(count.map(|count| muted(count.to_string()))),
                    )
                    .child(muted(
                        "Prompts T3 runs on its own: at a time, on an interval, or when a webhook is called.",
                    )),
            )
            .when(self.editor.is_none(), |this| {
                this.child(
                    div()
                        .id("schedule-new-wrapper")
                        .debug_selector(|| "schedule-new".into())
                        .when_some(blocked.clone(), |this, reason| {
                            this.tooltip(Tooltip::text(reason))
                        })
                        .child(
                            TextButton::new("schedule-new", "New task")
                                .style(ButtonStyle::Primary)
                                .disabled(self.backend.is_none() || blocked.is_some())
                                .on_click(cx.listener(|this, _, window, cx| {
                                    this.open_editor(Draft::new(None, None), window, cx)
                                })),
                        ),
                )
            });
        v_flex()
            .key_context("MustrSchedules")
            .track_focus(&self.focus_handle)
            .size_full()
            .bg(palette.background.hsla())
            .child(
                v_flex()
                    .id("schedules-scroll")
                    .size_full()
                    .overflow_y_scroll()
                    .child(
                        v_flex()
                            .w_full()
                            .max_w(scaled(px(CONTENT_MAX_WIDTH)))
                            .mx_auto()
                            .px_6()
                            .py_6()
                            .gap_4()
                            .child(header)
                            .children(self.editor.clone())
                            .when_some(self.error.clone(), |this, error| {
                                this.child(div().debug_selector(|| "schedule-error".into()).child(
                                    Label::new(error).size(LabelSize::Small).color(Color::Error),
                                ))
                            })
                            .child(self.render_list(cx)),
                    ),
            )
    }
}

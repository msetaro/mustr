//! A thread's family (`mustr-docs/PARITY.md` §2.7 and §2.14): in a child, the way back to its
//! parent (a link in the thread's header); in a parent, its subagents and delegated tasks with
//! Open and Stop, and its workflows with their agents, script, and Stop, in a drawer from the
//! right edge of the thread view that the header's subagents button opens; and, above the
//! composer as T3 shows them at the end of its timeline (`chat/ThreadNow.tsx`), the subagents
//! waiting for the user and the background work the agent left running.
//!
//! The family comes from the shell's threads and their `lineage`, which [`crate::ThreadShells`]
//! keeps for the whole app, and from the thread's own projection (`family_bar/family.rs`).
//! T3 0.1.3 adds `orchestration.getThreadFamily`: shells that opt into `omitIdleSubagents` (as
//! T3's own client does) leave idle subagent children out, and the family read has them. So the
//! bar reads the family once per thread and shell generation and merges it behind the shell's
//! rows, which win, as T3's client does (`threadFamilies.ts`). T3 0.1.1 doesn't have the call,
//! and its shell holds every child, so there the shell alone is the family.

mod family;
#[cfg(test)]
mod tests;

use family::{
    Activity, BackgroundWork, Family, StopTarget, SubagentTask, Workflow, WorkflowMember,
};
pub(crate) use family::{Relationship, Relative, format_subagent_title};

use crate::{
    backend::{Access, ThreadBackend, ThreadBackendExt as _, is_unsupported_rpc},
    commands,
    shell::{Shell, ThreadShells},
    thread_view::ThreadView,
};
use collections::{HashMap, HashSet};
use gpui::{
    AnyElement, App, Context, Entity, FocusHandle, Focusable as _, FontWeight, Hsla,
    InteractiveElement as _, IntoElement, MouseButton, ParentElement as _, Render, SharedString,
    Styled as _, Subscription, Task, WeakEntity, Window, div, px,
};
use mustr_ui::{TextButton, palette, tokens, tokens::scaled};
use std::sync::Arc;
use t3_api::{
    rpc,
    types::{
        OrchestrationV2GetThreadFamilyInput, OrchestrationV2GetWorkflowScriptInput,
        OrchestrationV2StopWorkflowInput, OrchestrationV2ThreadFamily,
    },
};
use ui::{
    Color, Icon, IconButton, IconName, IconSize, Label, LabelCommon as _, LabelSize, Tooltip,
    prelude::*,
};
use workspace::Workspace;

/// The subagents drawer's width at mustr's design size; a narrow view caps it.
const DRAWER_WIDTH: f32 = 360.;
/// Background subagents with a button of their own; the rest are in the title's count.
const BACKGROUND_THREADS: usize = 2;
/// Lines of a workflow script shown at once.
const SCRIPT_MAX_LINES: usize = 400;

/// A workflow's script, once asked for.
#[derive(Clone, Debug, PartialEq)]
enum Script {
    Loading,
    Loaded {
        contents: SharedString,
        truncated: bool,
    },
    Failed(SharedString),
}

/// What `orchestration.getThreadFamily` said about the thread's family.
#[derive(Clone, Debug, Default, PartialEq)]
enum FamilyRead {
    /// Not read yet, being read, or the read failed: the shell alone is the family.
    #[default]
    None,
    Loaded(OrchestrationV2ThreadFamily),
    /// The server is older than 0.1.3 and has no such call. Its shell has every child.
    Unsupported,
}

/// The family of one [`ThreadView`]'s thread.
pub struct ThreadFamilyBar {
    thread_view: WeakEntity<ThreadView>,
    shells: Entity<ThreadShells>,
    backend: Option<Arc<dyn ThreadBackend>>,
    thread_id: Option<String>,
    /// The family read and the thread and shell generation it was asked for.
    family_read: FamilyRead,
    family_read_for: Option<(String, usize)>,
    family: Family,
    /// Whether the subagents drawer is open.
    open: bool,
    focus_handle: FocusHandle,
    /// Workflow scripts shown, by subagent id.
    scripts: HashMap<String, Script>,
    /// Stops in flight, by [`StopTarget::key`] or workflow subagent id.
    stopping: HashSet<String>,
    error: Option<SharedString>,
    _subscriptions: Vec<Subscription>,
    _tasks: Vec<Task<()>>,
}

impl ThreadFamilyBar {
    pub fn new(
        thread_view: WeakEntity<ThreadView>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        let shells = ThreadShells::global(cx);
        let subscriptions = vec![cx.observe(&shells, |this, _, cx| this.refresh(cx))];
        // The view is still being built, so it can only be observed once it exists.
        cx.defer_in(window, |this, _, cx| {
            if let Some(view) = this.thread_view.upgrade() {
                this._subscriptions
                    .push(cx.observe(&view, |this, _, cx| this.refresh(cx)));
                this.refresh(cx);
            }
        });
        Self {
            thread_view,
            shells,
            backend: None,
            thread_id: None,
            family_read: FamilyRead::None,
            family_read_for: None,
            family: Family::default(),
            open: false,
            focus_handle: cx.focus_handle(),
            scripts: HashMap::default(),
            stopping: HashSet::default(),
            error: None,
            _subscriptions: subscriptions,
            _tasks: Vec::new(),
        }
    }

    /// Works the family out again from the thread, the shell, and the family read.
    fn refresh(&mut self, cx: &mut Context<Self>) {
        let Some(view) = self.thread_view.upgrade() else {
            return;
        };
        let view = view.read(cx);
        let backend = view.backend.clone();
        let shells = self.shells.read(cx);
        let generation = shells.generation();
        let (thread_id, family) = match view.thread_state() {
            Some(state) => {
                let merged = match &self.family_read {
                    FamilyRead::Loaded(read) => {
                        Some(merge_family(shells.shell(), read, &state.thread))
                    }
                    FamilyRead::None | FamilyRead::Unsupported => None,
                };
                let shell = merged.as_ref().or(shells.shell());
                (
                    Some(state.thread_id().to_string()),
                    Family::derive(&state.thread, shell, state.subagents()),
                )
            }
            None => (None, Family::default()),
        };
        if let Some(thread_id) = &thread_id {
            self.read_family(&backend, thread_id, generation, cx);
        }
        self.backend = Some(backend);
        if thread_id != self.thread_id {
            self.thread_id = thread_id;
            self.open = false;
            self.scripts.clear();
            self.error = None;
            cx.notify();
        }
        if family != self.family {
            self.family = family;
            cx.notify();
        }
    }

    /// Reads the thread's family (`orchestration.getThreadFamily`) once per thread and shell
    /// generation, once the shell has loaded. On a server without the call (T3 0.1.1) it stays
    /// with the shell.
    fn read_family(
        &mut self,
        backend: &Arc<dyn ThreadBackend>,
        thread_id: &str,
        generation: usize,
        cx: &mut Context<Self>,
    ) {
        let key = (thread_id.to_string(), generation);
        if generation == 0 || self.family_read_for.as_ref() == Some(&key) {
            return;
        }
        let thread_changed = self
            .family_read_for
            .as_ref()
            .is_none_or(|(read_for, _)| read_for != thread_id);
        self.family_read_for = Some(key.clone());
        if thread_changed && self.family_read != FamilyRead::Unsupported {
            self.family_read = FamilyRead::None;
        }
        let read = backend.call::<rpc::OrchestrationGetThreadFamily>(
            OrchestrationV2GetThreadFamilyInput {
                thread_id: thread_id.to_string(),
            },
            Access::Read,
        );
        self._tasks.push(cx.spawn(async move |this, cx| {
            let result = read.await;
            this.update(cx, |this, cx| {
                // A newer read was asked for in the meantime.
                if this.family_read_for.as_ref() != Some(&key) {
                    return;
                }
                this.family_read = match result {
                    Ok(family) => FamilyRead::Loaded(family),
                    Err(error) if is_unsupported_rpc(&error) => {
                        log::debug!("{error:#}; the family comes from the shell");
                        FamilyRead::Unsupported
                    }
                    Err(error) => {
                        log::warn!("couldn't read thread {}'s family: {error:#}", key.0);
                        FamilyRead::None
                    }
                };
                this.refresh(cx);
            })
            .ok();
        }));
    }

    fn blocked_reason(&self, access: Access) -> Option<String> {
        self.backend.as_ref()?.blocked_reason(access)
    }

    fn open(&mut self, thread_id: String, title: String, window: &mut Window, cx: &mut App) {
        let Some(workspace) = Workspace::for_window(window, cx) else {
            log::error!("the thread isn't in a workspace, so its relative can't open");
            return;
        };
        crate::open_thread(thread_id, title.into(), workspace.downgrade(), window, cx);
    }

    /// Stops a running child or subagent: `subagent.stop` for Claude's own, `thread.stop` on
    /// the child for T3's delegated tasks.
    fn stop(&mut self, target: StopTarget, cx: &mut Context<Self>) {
        let key = target.key().to_string();
        self.send(key, cx, move |backend| {
            let dispatch = backend.dispatch(target.command(commands::new_id()));
            Box::pin(async move { dispatch.await.map(drop) })
        });
    }

    /// Stops the background work the thread's agent left running (`thread.stop`), as T3's
    /// "Stop" on its background-work line does.
    fn stop_background(&mut self, cx: &mut Context<Self>) {
        let Some(thread_id) = self.thread_id.clone() else {
            return;
        };
        self.send(format!("background:{thread_id}"), cx, move |backend| {
            let dispatch = backend.dispatch(commands::stop(&thread_id, commands::new_id()));
            Box::pin(async move { dispatch.await.map(drop) })
        });
    }

    /// Stops a whole workflow (`orchestration.stopWorkflow`).
    fn stop_workflow(&mut self, subagent_id: String, cx: &mut Context<Self>) {
        let Some(thread_id) = self.thread_id.clone() else {
            return;
        };
        self.send(subagent_id.clone(), cx, move |backend| {
            backend.call::<rpc::OrchestrationStopWorkflow>(
                OrchestrationV2StopWorkflowInput {
                    subagent_id,
                    thread_id,
                },
                Access::Operate,
            )
        });
    }

    fn send(
        &mut self,
        key: String,
        cx: &mut Context<Self>,
        write: impl FnOnce(
            &Arc<dyn ThreadBackend>,
        ) -> futures::future::BoxFuture<'static, anyhow::Result<()>>,
    ) {
        let Some(backend) = self.backend.clone() else {
            return;
        };
        if !self.stopping.insert(key.clone()) {
            return;
        }
        self.error = None;
        let write = write(&backend);
        cx.notify();
        self._tasks.push(cx.spawn(async move |this, cx| {
            let result = write.await;
            this.update(cx, |this, cx| {
                this.stopping.remove(&key);
                if let Err(error) = result {
                    log::warn!("couldn't stop it: {error:#}");
                    this.error = Some(format!("{error:#}").into());
                }
                cx.notify();
            })
            .ok();
        }));
    }

    /// Shows or hides a workflow's script, loading it the first time
    /// (`orchestration.getWorkflowScript`).
    fn toggle_script(&mut self, subagent_id: String, script_path: String, cx: &mut Context<Self>) {
        if self.scripts.remove(&subagent_id).is_some() {
            cx.notify();
            return;
        }
        let (Some(backend), Some(thread_id)) = (self.backend.clone(), self.thread_id.clone())
        else {
            return;
        };
        self.scripts.insert(subagent_id.clone(), Script::Loading);
        cx.notify();
        let call = backend.call::<rpc::OrchestrationGetWorkflowScript>(
            OrchestrationV2GetWorkflowScriptInput {
                script_path,
                thread_id,
            },
            Access::Read,
        );
        self._tasks.push(cx.spawn(async move |this, cx| {
            let script = match call.await {
                Ok(result) => Script::Loaded {
                    contents: script_text(&result.contents).into(),
                    truncated: result.truncated,
                },
                Err(error) => Script::Failed(format!("{error:#}").into()),
            };
            this.update(cx, |this, cx| {
                // Hidden again while it loaded.
                if let Some(shown) = this.scripts.get_mut(&subagent_id) {
                    *shown = script;
                    cx.notify();
                }
            })
            .ok();
        }));
    }

    /// How many subagents, delegated tasks and workflows the drawer lists, and how many work.
    pub(crate) fn summary(&self) -> (usize, usize) {
        let family = &self.family;
        (
            family.children.len() + family.tasks.len() + family.workflows.len(),
            family.working_count(),
        )
    }

    /// The thread this one is a subagent or fork of, for the header's link.
    pub(crate) fn parent(&self) -> Option<&Relative> {
        self.family.parent.as_ref()
    }

    pub(crate) fn is_open(&self) -> bool {
        self.open
    }

    /// Opens or closes the subagents drawer, focusing it when it opens, so Escape closes it.
    pub(crate) fn toggle(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.open = !self.open;
        if self.open {
            window.focus(&self.focus_handle, cx);
        }
        cx.notify();
    }

    /// Closes the drawer, saying whether it was open.
    pub(crate) fn close(&mut self, cx: &mut Context<Self>) -> bool {
        let was_open = std::mem::take(&mut self.open);
        if was_open {
            cx.notify();
        }
        was_open
    }

    /// Opens the parent thread.
    pub(crate) fn open_parent(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if let Some(parent) = self.family.parent.clone().filter(|parent| parent.available) {
            self.open(parent.thread_id, parent.title, window, cx);
        }
    }
}

/// The script as the bar shows it: its first lines, so a long one doesn't stall drawing.
fn script_text(contents: &str) -> String {
    let mut lines = contents.lines();
    let shown = lines
        .by_ref()
        .take(SCRIPT_MAX_LINES)
        .collect::<Vec<_>>()
        .join("\n");
    let rest = lines.count();
    if rest == 0 {
        shown
    } else {
        format!("{shown}\n… {rest} more lines")
    }
}

fn activity_color(activity: &Activity, cx: &App) -> Hsla {
    let palette = palette(cx);
    match activity {
        Activity::Working(_) => palette.accent.hsla(),
        Activity::NeedsInput => palette.warning.hsla(),
        Activity::Failed => cx.theme().status().error,
        Activity::Done(status) if status == "completed" => palette.ok.hsla(),
        Activity::Done(_) => palette.text_muted.hsla(),
    }
}

fn status_label(activity: &Activity, cx: &App) -> impl IntoElement {
    Label::new(activity.label().to_string())
        .size(LabelSize::XSmall)
        .color(Color::Custom(activity_color(activity, cx)))
}

fn agent_label(provider: Option<&str>, model: Option<&str>) -> Option<String> {
    let parts = [
        provider.map(family::provider_name),
        model.map(str::to_string),
    ]
    .into_iter()
    .flatten()
    .collect::<Vec<_>>();
    (!parts.is_empty()).then(|| parts.join(" · "))
}

/// The status a relative's badge shows, in T3's subagent statuses.
fn badge_status(activity: &Activity) -> &str {
    match activity {
        Activity::Working(status) => status,
        Activity::NeedsInput => "waiting",
        Activity::Failed => "failed",
        Activity::Done(status) => status,
    }
}

impl ThreadFamilyBar {
    /// A Stop button, disabled while its stop is in flight or when writes are refused.
    fn stop_button(
        &self,
        id: String,
        key: &str,
        tooltip: &'static str,
        cx: &mut Context<Self>,
        on_click: impl Fn(&mut Self, &mut Context<Self>) + 'static,
    ) -> AnyElement {
        let stopping = self.stopping.contains(key);
        let blocked = self.blocked_reason(Access::Operate);
        let this = cx.entity().downgrade();
        let selector = id.clone();
        div()
            .debug_selector(|| selector)
            // A disabled button lets clicks through, and Stop sits on rows that open on click.
            .on_mouse_down(MouseButton::Left, |_, _, cx| cx.stop_propagation())
            .child(
                TextButton::new(
                    SharedString::from(id),
                    if stopping { "Stopping…" } else { "Stop" },
                )
                .disabled(stopping || blocked.is_some())
                .tooltip(blocked.unwrap_or_else(|| tooltip.to_string()))
                .on_click(move |_, _, cx| {
                    this.update(cx, |this, cx| on_click(this, cx)).ok();
                }),
            )
            .into_any_element()
    }

    /// The driver of the thread's own agent, which its workflows' agents run on.
    fn thread_driver(&self, cx: &App) -> Option<String> {
        let view = self.thread_view.upgrade()?;
        let instance = view
            .read(cx)
            .thread_state()?
            .thread
            .provider_instance_id
            .clone();
        Some(crate::agent_badge::instance_driver(&instance, cx))
    }

    /// One row of the drawer, as T3's subagent rows (`SubagentTimelineLink`): the agent's badge,
    /// the title over the agent and its status, Stop while it runs, and a chevron when it opens.
    #[allow(clippy::too_many_arguments)]
    fn drawer_row(
        &self,
        id: impl Into<gpui::ElementId>,
        selector: String,
        driver: Option<String>,
        activity: &Activity,
        title: String,
        detail: Option<String>,
        opens: bool,
        cx: &mut Context<Self>,
    ) -> gpui::Stateful<gpui::Div> {
        let palette = palette(cx);
        let status = badge_status(activity).to_string();
        let mut line = vec![
            Label::new(activity.label().to_string())
                .size(LabelSize::XSmall)
                .color(Color::Custom(activity_color(activity, cx)))
                .into_any_element(),
        ];
        if let Some(detail) = detail {
            line.push(
                Label::new(format!("· {detail}"))
                    .size(LabelSize::XSmall)
                    .color(Color::Muted)
                    .truncate()
                    .into_any_element(),
            );
        }
        h_flex()
            .id(id)
            .debug_selector({
                let selector = selector.clone();
                move || selector
            })
            .flex_none()
            .w_full()
            .px_2()
            .py(scaled(px(5.)))
            .gap(scaled(px(10.)))
            .rounded(scaled(tokens::CONTROL_RADIUS))
            .when(opens, |this| {
                this.cursor_pointer()
                    .hover(|style| style.bg(palette.surface_2.hsla()))
            })
            .child(crate::agent_badge::subagent_badge(
                format!("{selector}-badge"),
                driver.as_deref(),
                Some(&status),
                cx,
            ))
            .child(
                v_flex()
                    .flex_1()
                    .min_w_0()
                    .child(Label::new(title).size(LabelSize::Small).truncate())
                    .child(h_flex().min_w_0().gap_1().children(line)),
            )
    }

    fn chevron(opens: bool) -> Option<Icon> {
        opens.then(|| {
            Icon::new(IconName::ChevronRight)
                .size(IconSize::XSmall)
                .color(Color::Muted)
        })
    }

    fn render_child(&self, index: usize, child: &Relative, cx: &mut Context<Self>) -> AnyElement {
        let thread_id = child.thread_id.clone();
        let child_title = child.title.clone();
        let driver = child
            .provider
            .as_deref()
            .map(|instance| crate::agent_badge::instance_driver(instance, cx));
        let agent = agent_label(child.provider.as_deref(), child.model.as_deref());
        self.drawer_row(
            ("family-child", index),
            format!("family-child-{}", child.thread_id),
            driver,
            &child.activity,
            child.title.clone(),
            agent,
            child.available,
            cx,
        )
        .when(child.available, |this| {
            this.tooltip(Tooltip::text("Open this thread"))
                .on_click(cx.listener(move |this, _, window, cx| {
                    this.open(thread_id.clone(), child_title.clone(), window, cx)
                }))
        })
        .when(child.relationship == Relationship::Fork, |this| {
            this.child(
                Icon::new(IconName::GitBranch)
                    .size(IconSize::XSmall)
                    .color(Color::Muted),
            )
        })
        .children(child.stop.clone().map(|target| {
            let key = target.key().to_string();
            self.stop_button(
                format!("family-stop-{}", child.thread_id),
                &key,
                "Stop this subagent",
                cx,
                move |this, cx| this.stop(target.clone(), cx),
            )
        }))
        .children(Self::chevron(child.available))
        .into_any_element()
    }

    fn render_task(&self, index: usize, task: &SubagentTask, cx: &mut Context<Self>) -> AnyElement {
        let driver = task
            .provider
            .as_deref()
            .map(|instance| crate::agent_badge::instance_driver(instance, cx));
        let agent = agent_label(task.provider.as_deref(), task.model.as_deref());
        self.drawer_row(
            ("family-task", index),
            format!("family-task-{}", task.id),
            driver,
            &task.activity,
            task.title.clone(),
            agent,
            false,
            cx,
        )
        .children(task.stop.clone().map(|target| {
            let key = target.key().to_string();
            self.stop_button(
                format!("family-stop-task-{}", task.id),
                &key,
                "Stop this subagent",
                cx,
                move |this, cx| this.stop(target.clone(), cx),
            )
        }))
        .into_any_element()
    }

    /// A workflow (`WorkflowOpeningBlock.tsx`): its name, its agents' tally and status, Script,
    /// Open and Stop, then its phases with their agents.
    fn render_workflow(
        &self,
        index: usize,
        workflow: &Workflow,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let palette = palette(cx);
        let driver = self.thread_driver(cx);
        let header = h_flex()
            .id(("family-workflow", index))
            .w_full()
            .px_2()
            .py(scaled(px(5.)))
            .gap_1p5()
            .child(
                Icon::new(IconName::ListTree)
                    .size(IconSize::Small)
                    .color(Color::Muted),
            )
            .child(
                v_flex()
                    .flex_1()
                    .min_w_0()
                    .child(
                        Label::new(workflow.name.clone())
                            .size(LabelSize::Small)
                            .weight(FontWeight::MEDIUM)
                            .truncate(),
                    )
                    .child(
                        h_flex()
                            .gap_1()
                            .child(status_label(&workflow.activity, cx))
                            .child(
                                Label::new(format!("· {}", workflow.summary()))
                                    .size(LabelSize::XSmall)
                                    .color(Color::Muted)
                                    .truncate(),
                            ),
                    ),
            )
            .children(workflow.script_path.clone().map(|script_path| {
                let subagent_id = workflow.subagent_id.clone();
                let shown = self.scripts.contains_key(&workflow.subagent_id);
                let this = cx.entity().downgrade();
                let id = format!("family-workflow-script-{}", workflow.subagent_id);
                let selector = id.clone();
                div().debug_selector(|| selector).child(
                    TextButton::new(
                        SharedString::from(id),
                        if shown { "Hide script" } else { "Script" },
                    )
                    .tooltip("The script that runs this workflow")
                    .on_click(move |_, _, cx| {
                        let (subagent_id, script_path) = (subagent_id.clone(), script_path.clone());
                        this.update(cx, |this, cx| {
                            this.toggle_script(subagent_id, script_path, cx)
                        })
                        .ok();
                    }),
                )
            }))
            .when_some(
                workflow.thread_id.clone().filter(|_| workflow.available),
                |this, thread_id| {
                    let name = workflow.name.clone();
                    let bar = cx.entity().downgrade();
                    this.child(
                        TextButton::new(
                            SharedString::from(format!("family-workflow-open-{index}")),
                            "Open",
                        )
                        .tooltip("Open the workflow's coordinator thread")
                        .on_click(move |_, window, cx| {
                            let (thread_id, name) = (thread_id.clone(), name.clone());
                            bar.update(cx, |this, cx| this.open(thread_id, name, window, cx))
                                .ok();
                        }),
                    )
                },
            )
            .when(workflow.can_stop, |this| {
                let subagent_id = workflow.subagent_id.clone();
                this.child(self.stop_button(
                    format!("family-workflow-stop-{}", workflow.subagent_id),
                    &workflow.subagent_id,
                    "Stop the whole workflow",
                    cx,
                    move |this, cx| this.stop_workflow(subagent_id.clone(), cx),
                ))
            });
        let script = self.scripts.get(&workflow.subagent_id).map(|script| {
            let body = match script {
                Script::Loading => Label::new("Loading the script…")
                    .size(LabelSize::XSmall)
                    .color(Color::Muted)
                    .into_any_element(),
                Script::Failed(error) => Label::new(error.clone())
                    .size(LabelSize::XSmall)
                    .color(Color::Error)
                    .into_any_element(),
                Script::Loaded {
                    contents,
                    truncated,
                } => v_flex()
                    .debug_selector(|| "family-workflow-script-loaded".into())
                    .child(
                        div()
                            .font_family(tokens::CODE_FONT_FAMILY)
                            .text_size(tokens::text_size(tokens::TEXT_SIZE_SMALL))
                            .text_color(palette.text.hsla())
                            .child(contents.clone()),
                    )
                    .when(*truncated, |this| {
                        this.child(
                            Label::new("T3 sent only the start of the script.")
                                .size(LabelSize::XSmall)
                                .color(Color::Muted),
                        )
                    })
                    .into_any_element(),
            };
            div()
                .id(("family-workflow-script", index))
                .mx_2()
                .mb_1()
                .max_h(scaled(px(240.)))
                .overflow_y_scroll()
                .px_2()
                .py_1p5()
                .rounded(scaled(tokens::CONTROL_RADIUS))
                .border(tokens::BORDER_WIDTH)
                .border_color(palette.border.hsla())
                .bg(palette.background.hsla())
                .child(body)
        });
        v_flex()
            .w_full()
            .child(header)
            .children(script)
            .children(
                workflow
                    .phases
                    .iter()
                    .enumerate()
                    .map(|(phase_index, phase)| {
                        v_flex()
                            .w_full()
                            .pl(scaled(px(12.)))
                            .when_some(phase.title.clone(), |this, phase_title| {
                                this.child(
                                    h_flex()
                                        .px_2()
                                        .pt_1()
                                        .gap_2()
                                        .child(
                                            Label::new(phase_title)
                                                .size(LabelSize::XSmall)
                                                .weight(FontWeight::SEMIBOLD)
                                                .color(Color::Muted),
                                        )
                                        .child(
                                            Label::new(format!(
                                                "{}/{}",
                                                phase.settled(),
                                                phase.members.len()
                                            ))
                                            .size(LabelSize::XSmall)
                                            .color(Color::Muted),
                                        ),
                                )
                            })
                            .children(phase.members.iter().enumerate().map(
                                |(member_index, member)| {
                                    self.render_member(
                                        (index * 1000 + phase_index) * 1000 + member_index,
                                        member,
                                        driver.clone(),
                                        cx,
                                    )
                                },
                            ))
                    }),
            )
            .when(workflow.phases.is_empty(), |this| {
                this.child(
                    div().px_2().pl(scaled(px(20.))).child(
                        Label::new("No agents yet")
                            .size(LabelSize::XSmall)
                            .color(Color::Muted),
                    ),
                )
            })
            .into_any_element()
    }

    fn render_member(
        &self,
        index: usize,
        member: &WorkflowMember,
        driver: Option<String>,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let label = member.label.clone();
        let opens = member.thread_id.is_some() && member.available;
        self.drawer_row(
            ("family-workflow-member", index),
            format!("family-workflow-member-{}", member.label),
            driver,
            &member.activity,
            member.label.clone(),
            None,
            opens,
            cx,
        )
        .when_some(
            member.thread_id.clone().filter(|_| member.available),
            |this, thread_id| {
                this.tooltip(Tooltip::text("Open this agent's thread"))
                    .on_click(cx.listener(move |this, _, window, cx| {
                        this.open(thread_id.clone(), label.clone(), window, cx)
                    }))
            },
        )
        .children(Self::chevron(opens))
        .into_any_element()
    }

    /// The lines above the composer, as T3's `ThreadNow` shows them at the end of its timeline:
    /// subagents waiting for the user, the background work the agent left running with Stop,
    /// and that the agent waits on another thread.
    pub(crate) fn render_now(&mut self, cx: &mut Context<Self>) -> Vec<AnyElement> {
        let family = self.family.clone();
        let mut lines = Vec::new();
        let asking = family
            .children
            .iter()
            .filter(|child| child.activity == Activity::NeedsInput && child.available)
            .collect::<Vec<_>>();
        if let Some(first) = asking.first() {
            let title = if asking.len() == 1 {
                "A subagent is waiting for you".to_string()
            } else {
                format!("{} subagents are waiting for you", asking.len())
            };
            let (thread_id, child_title) = (first.thread_id.clone(), first.title.clone());
            lines.push(
                now_line(
                    "family-now-input",
                    Icon::new(IconName::CircleHelp)
                        .size(IconSize::Small)
                        .color(Color::Accent)
                        .into_any_element(),
                    title,
                    Some(
                        asking
                            .iter()
                            .map(|child| child.title.clone())
                            .collect::<Vec<_>>()
                            .join(", "),
                    ),
                    cx,
                )
                .child(
                    TextButton::new("family-now-input-open", "Open")
                        .tooltip("Open the subagent that's asking")
                        .on_click(cx.listener(move |this, _, window, cx| {
                            this.open(thread_id.clone(), child_title.clone(), window, cx)
                        })),
                )
                .into_any_element(),
            );
        }
        if let Some(work) = &family.background {
            lines.push(self.render_background(work, cx));
        }
        if family.waiting_on_thread {
            lines.push(
                now_line(
                    "family-now-waiting",
                    Icon::new(IconName::Clock)
                        .size(IconSize::Small)
                        .color(Color::Muted)
                        .into_any_element(),
                    "Waiting on another thread".to_string(),
                    None,
                    cx,
                )
                .into_any_element(),
            );
        }
        if let Some(error) = self.error.clone().filter(|_| !self.open) {
            lines.push(
                div()
                    .debug_selector(|| "family-error".into())
                    .child(
                        Label::new(error)
                            .size(LabelSize::XSmall)
                            .color(Color::Error),
                    )
                    .into_any_element(),
            );
        }
        lines
    }

    fn render_background(&self, work: &BackgroundWork, cx: &mut Context<Self>) -> AnyElement {
        let key = format!(
            "background:{}",
            self.thread_id.as_deref().unwrap_or_default()
        );
        let palette = palette(cx);
        now_line(
            "family-now-background",
            div()
                .size(scaled(px(6.)))
                .rounded_full()
                .bg(palette.text_muted.hsla())
                .into_any_element(),
            work.title.clone(),
            None,
            cx,
        )
        .children(
            work.threads
                .iter()
                .take(BACKGROUND_THREADS)
                .enumerate()
                .map(|(index, (label, thread_id))| {
                    let this = cx.entity().downgrade();
                    let (label, thread_id) = (label.clone(), thread_id.clone());
                    TextButton::new(
                        SharedString::from(format!("family-background-open-{index}")),
                        if work.threads.len() == 1 {
                            "Show".to_string()
                        } else {
                            label.clone()
                        },
                    )
                    .tooltip("Open the subagent's thread")
                    .on_click(move |_, window, cx| {
                        let (thread_id, label) = (thread_id.clone(), label.clone());
                        this.update(cx, |this, cx| this.open(thread_id, label, window, cx))
                            .ok();
                    })
                }),
        )
        .child(self.stop_button(
            "family-background-stop".to_string(),
            &key,
            "Stop the agent's background work",
            cx,
            |this, cx| this.stop_background(cx),
        ))
        .into_any_element()
    }
}

/// One line above the composer: an icon, what's going on and its detail, then its actions
/// (`ThreadNowLine` in T3).
fn now_line(
    selector: &'static str,
    icon: AnyElement,
    title: String,
    detail: Option<String>,
    cx: &App,
) -> gpui::Div {
    let palette = palette(cx);
    h_flex()
        .debug_selector(move || selector.into())
        .w_full()
        .min_w_0()
        .gap_1p5()
        .px_1()
        .child(
            div()
                .flex_none()
                .size(scaled(px(14.)))
                .flex()
                .items_center()
                .justify_center()
                .child(icon),
        )
        .child(
            div()
                .flex_1()
                .min_w_0()
                .overflow_hidden()
                .whitespace_nowrap()
                .text_ellipsis()
                .text_size(tokens::text_size(tokens::TEXT_SIZE_SMALL))
                .text_color(palette.text_muted.hsla())
                .child(match detail {
                    Some(detail) => format!("{title} · {detail}"),
                    None => title,
                }),
        )
}

impl Render for ThreadFamilyBar {
    /// The subagents drawer, over the right of the timeline while it's open.
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let (count, working) = self.summary();
        if !self.open || count == 0 {
            // With nothing to show, `hidden` keeps the drawer out of the view's layout.
            return div().hidden().into_any_element();
        }
        let palette = palette(cx);
        let family = self.family.clone();
        let settled = count - working.min(count);
        let summary = match (working, settled) {
            (0, settled) => format!("{settled} done"),
            (working, 0) => format!("{working} running"),
            (working, settled) => format!("{working} running · {settled} done"),
        };
        v_flex()
            .id("subagents-drawer")
            .debug_selector(|| "subagents-drawer".into())
            .key_context("SubagentsDrawer")
            .track_focus(&self.focus_handle)
            .on_action(cx.listener(|this, _: &menu::Cancel, window, cx| {
                this.close(cx);
                if let Some(view) = this.thread_view.upgrade() {
                    window.focus(&view.focus_handle(cx), cx);
                }
            }))
            .occlude()
            .absolute()
            .top_0()
            .right_0()
            .bottom_0()
            .w(scaled(px(DRAWER_WIDTH)))
            .max_w(gpui::relative(0.92))
            .border_l(tokens::BORDER_WIDTH)
            .border_color(palette.border.hsla())
            .bg(palette.surface.hsla())
            .shadow_lg()
            .child(
                h_flex()
                    .flex_none()
                    .px_3()
                    .py_2()
                    .gap_2()
                    .border_b(tokens::BORDER_WIDTH)
                    .border_color(palette.border.hsla())
                    .child(
                        Label::new("Subagents")
                            .size(LabelSize::Small)
                            .weight(FontWeight::SEMIBOLD),
                    )
                    .child(
                        div().flex_1().min_w_0().child(
                            Label::new(summary)
                                .size(LabelSize::XSmall)
                                .color(Color::Muted)
                                .truncate(),
                        ),
                    )
                    .child(
                        div()
                            .debug_selector(|| "subagents-drawer-close".into())
                            .child(
                                IconButton::new("subagents-drawer-close", IconName::Close)
                                    .icon_size(IconSize::XSmall)
                                    .icon_color(Color::Muted)
                                    .tooltip(Tooltip::text("Close"))
                                    .on_click(cx.listener(|this, _, window, cx| {
                                        this.close(cx);
                                        if let Some(view) = this.thread_view.upgrade() {
                                            window.focus(&view.focus_handle(cx), cx);
                                        }
                                    })),
                            ),
                    ),
            )
            .when_some(self.error.clone(), |this, error| {
                this.child(
                    div()
                        .debug_selector(|| "family-error".into())
                        .px_3()
                        .py_1()
                        .child(
                            Label::new(error)
                                .size(LabelSize::XSmall)
                                .color(Color::Error),
                        ),
                )
            })
            .child(
                v_flex()
                    .id("subagents-drawer-list")
                    .flex_1()
                    .min_h_0()
                    .overflow_y_scroll()
                    .p_1p5()
                    .gap_0p5()
                    .children(
                        family
                            .tasks
                            .iter()
                            .enumerate()
                            .map(|(index, task)| self.render_task(index, task, cx)),
                    )
                    .children(
                        family
                            .children
                            .iter()
                            .enumerate()
                            .map(|(index, child)| self.render_child(index, child, cx)),
                    )
                    .children(
                        family
                            .workflows
                            .iter()
                            .enumerate()
                            .map(|(index, workflow)| self.render_workflow(index, workflow, cx)),
                    ),
            )
            .into_any_element()
    }
}

/// The shell, with the family's rows that it doesn't have: the threads sharing the thread's root
/// that a shell omitting idle subagents leaves out. The shell's rows win, since its changes are
/// newer, and a row the shell dropped after the read stays dropped.
fn merge_family(
    shell: Option<&Shell>,
    read: &OrchestrationV2ThreadFamily,
    thread: &t3_api::types::OrchestrationV2AppThread,
) -> Shell {
    let root = &thread.lineage.root_thread_id;
    let mut merged = Shell::default();
    for row in &read.threads {
        let dropped = shell
            .and_then(|shell| shell.removed.get(&row.id))
            .is_some_and(|sequence| *sequence > read.snapshot_sequence);
        if !dropped {
            merged.threads.insert(row.id.clone(), row.clone());
        }
    }
    if let Some(shell) = shell {
        merged.sequence = shell.sequence;
        for (id, row) in &shell.threads {
            if merged.threads.contains_key(id)
                || &row.lineage.root_thread_id == root
                || row.id == thread.id
            {
                merged.threads.insert(id.clone(), row.clone());
            }
        }
    }
    merged
}

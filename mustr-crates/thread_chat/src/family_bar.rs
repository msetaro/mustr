//! A thread's family bar, between the header and the timeline (`mustr-docs/PARITY.md` §2.7 and
//! §2.14): in a child, the way back to its parent; in a parent, its subagents and delegated
//! tasks with Open and Stop, its workflows with their agents, script, and Stop, and the
//! background work its agent left running.
//!
//! On T3 0.1.1 the family comes from the shell's threads and their `lineage`, which
//! [`crate::ThreadShells`] keeps for the whole app, and from the thread's own projection
//! (`family_bar/family.rs`). 0.1.3 adds `orchestration.getThreadFamily`, whose snapshots leave
//! out idle subagent children; switch to it when the pin moves.

mod family;
#[cfg(test)]
mod tests;

use family::{
    Activity, BackgroundWork, Family, Relationship, Relative, StopTarget, SubagentTask, Workflow,
    WorkflowMember,
};

use crate::{
    backend::{Access, ThreadBackend, ThreadBackendExt as _},
    commands,
    shell::ThreadShells,
    thread_view::ThreadView,
};
use collections::{HashMap, HashSet};
use gpui::{
    AnyElement, App, Context, Entity, FontWeight, Hsla, InteractiveElement as _, IntoElement,
    MouseButton, ParentElement as _, Render, SharedString, Styled as _, Subscription, Task,
    WeakEntity, Window, div, px,
};
use mustr_ui::{TextButton, palette, tokens};
use std::sync::Arc;
use t3_api::{
    rpc,
    types::{OrchestrationV2GetWorkflowScriptInput, OrchestrationV2StopWorkflowInput},
};
use ui::{
    Color, Icon, IconName, IconSize, Indicator, Label, LabelCommon as _, LabelSize, Tooltip,
    prelude::*,
};
use workspace::Workspace;

/// Children listed before "Show more".
const VISIBLE_CHILDREN: usize = 5;
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

/// The family of one [`ThreadView`]'s thread.
pub struct ThreadFamilyBar {
    thread_view: WeakEntity<ThreadView>,
    shells: Entity<ThreadShells>,
    backend: Option<Arc<dyn ThreadBackend>>,
    thread_id: Option<String>,
    family: Family,
    /// Whether the children are listed; `None` until the user says, which lists them while any
    /// works.
    expanded: Option<bool>,
    show_all_children: bool,
    /// Workflows whose agents are listed, by subagent id.
    expanded_workflows: HashSet<String>,
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
            family: Family::default(),
            expanded: None,
            show_all_children: false,
            expanded_workflows: HashSet::default(),
            scripts: HashMap::default(),
            stopping: HashSet::default(),
            error: None,
            _subscriptions: subscriptions,
            _tasks: Vec::new(),
        }
    }

    /// Works the family out again from the thread and the shell.
    fn refresh(&mut self, cx: &mut Context<Self>) {
        let Some(view) = self.thread_view.upgrade() else {
            return;
        };
        let view = view.read(cx);
        let backend = view.backend.clone();
        let (thread_id, family) = match view.thread_state() {
            Some(state) => (
                Some(state.thread_id().to_string()),
                Family::derive(
                    &state.thread,
                    self.shells.read(cx).shell(),
                    state.subagents(),
                ),
            ),
            None => (None, Family::default()),
        };
        self.backend = Some(backend);
        if thread_id != self.thread_id {
            self.thread_id = thread_id;
            self.expanded = None;
            self.show_all_children = false;
            self.expanded_workflows.clear();
            self.scripts.clear();
            self.error = None;
            cx.notify();
        }
        if family != self.family {
            self.family = family;
            cx.notify();
        }
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

    fn is_expanded(&self) -> bool {
        self.expanded
            .unwrap_or_else(|| self.family.working_count() > 0 || self.family.children.len() <= 3)
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

fn section_label(text: &'static str) -> impl IntoElement {
    Label::new(text)
        .size(LabelSize::XSmall)
        .weight(FontWeight::SEMIBOLD)
        .color(Color::Muted)
}

/// A title that gives way to what follows it.
fn title(text: String) -> impl IntoElement {
    div()
        .flex_1()
        .min_w_0()
        .child(Label::new(text).size(LabelSize::Small).truncate())
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

    fn render_parent(&self, parent: &Relative, cx: &mut Context<Self>) -> AnyElement {
        let this = cx.entity().downgrade();
        let thread_id = parent.thread_id.clone();
        let parent_title = parent.title.clone();
        h_flex()
            .w_full()
            .h(px(26.))
            .gap_2()
            .child(
                Icon::new(IconName::ArrowUp)
                    .size(IconSize::Small)
                    .color(Color::Muted),
            )
            .child(
                Label::new(match parent.relationship {
                    Relationship::Subagent => "Subagent of",
                    Relationship::Fork => "Forked from",
                })
                .size(LabelSize::Small)
                .color(Color::Muted),
            )
            .child(title(parent.title.clone()))
            .when(parent.available, |this| {
                this.child(status_label(&parent.activity, cx))
            })
            .child(
                div().debug_selector(|| "family-open-parent".into()).child(
                    TextButton::new("family-open-parent", "Open parent")
                        .disabled(!parent.available)
                        .tooltip(if parent.available {
                            "Open the thread that started this one"
                        } else {
                            "T3 no longer lists the parent thread"
                        })
                        .on_click(move |_, window, cx| {
                            let (thread_id, parent_title) =
                                (thread_id.clone(), parent_title.clone());
                            this.update(cx, |this, cx| {
                                this.open(thread_id, parent_title, window, cx)
                            })
                            .ok();
                        }),
                ),
            )
            .into_any_element()
    }

    fn render_children(&self, cx: &mut Context<Self>) -> Option<AnyElement> {
        let children = &self.family.children;
        if children.is_empty() && self.family.tasks.is_empty() {
            return None;
        }
        let palette = palette(cx);
        let working = children
            .iter()
            .filter(|child| child.activity.is_working())
            .count()
            + self.family.tasks.len();
        let label = if children
            .iter()
            .all(|child| child.relationship == Relationship::Subagent)
        {
            "Subagents"
        } else {
            "Related threads"
        };
        let summary = match (working, children.len() + self.family.tasks.len() - working) {
            (0, settled) => format!("{settled}"),
            (working, 0) => format!("{working} running"),
            (working, settled) => format!("{working} running · {settled} done"),
        };
        let expanded = self.is_expanded();
        let header = h_flex()
            .id("family-children-toggle")
            .w_full()
            .h(px(24.))
            .gap_1p5()
            .cursor_pointer()
            .on_click(cx.listener(move |this, _, _, cx| {
                this.expanded = Some(!expanded);
                cx.notify();
            }))
            .child(
                Icon::new(if expanded {
                    IconName::ChevronDown
                } else {
                    IconName::ChevronRight
                })
                .size(IconSize::XSmall)
                .color(Color::Muted),
            )
            .child(section_label(label))
            .child(
                Label::new(summary)
                    .size(LabelSize::XSmall)
                    .color(Color::Muted),
            );
        let shown = if self.show_all_children {
            children.len()
        } else {
            VISIBLE_CHILDREN.min(children.len())
        };
        let hidden = children.len() - shown;
        Some(
            v_flex()
                .w_full()
                .child(header)
                .when(expanded, |this| {
                    this.children(
                        self.family
                            .tasks
                            .iter()
                            .enumerate()
                            .map(|(index, task)| self.render_task(index, task, cx)),
                    )
                    .children(
                        children
                            .iter()
                            .take(shown)
                            .enumerate()
                            .map(|(index, child)| self.render_child(index, child, cx)),
                    )
                    .when(hidden > 0, |this| {
                        this.child(
                            h_flex()
                                .id("family-show-more")
                                .h(px(24.))
                                .pl(px(20.))
                                .cursor_pointer()
                                .text_color(palette.text_muted.hsla())
                                .hover(|style| style.text_color(palette.text.hsla()))
                                .on_click(cx.listener(|this, _, _, cx| {
                                    this.show_all_children = true;
                                    cx.notify();
                                }))
                                .child(
                                    Label::new(format!("Show {hidden} more"))
                                        .size(LabelSize::XSmall)
                                        .color(Color::Muted),
                                ),
                        )
                    })
                })
                .into_any_element(),
        )
    }

    fn row(&self, id: impl Into<gpui::ElementId>, selector: String) -> gpui::Stateful<gpui::Div> {
        h_flex()
            .id(id)
            .debug_selector(|| selector)
            .w_full()
            .h(px(26.))
            .pl(px(20.))
            .pr_1()
            .gap_2()
            .rounded(tokens::CONTROL_RADIUS)
    }

    fn render_child(&self, index: usize, child: &Relative, cx: &mut Context<Self>) -> AnyElement {
        let palette = palette(cx);
        let color = activity_color(&child.activity, cx);
        let thread_id = child.thread_id.clone();
        let child_title = child.title.clone();
        self.row(
            ("family-child", index),
            format!("family-child-{}", child.thread_id),
        )
        .when(child.available, |this| {
            this.cursor_pointer()
                .hover(|style| style.bg(palette.surface_2.hsla()))
                .tooltip(Tooltip::text("Open this thread"))
                .on_click(cx.listener(move |this, _, window, cx| {
                    this.open(thread_id.clone(), child_title.clone(), window, cx)
                }))
        })
        .child(Indicator::dot().color(Color::Custom(color)))
        .when(child.relationship == Relationship::Fork, |this| {
            this.child(
                Icon::new(IconName::GitBranch)
                    .size(IconSize::XSmall)
                    .color(Color::Muted),
            )
        })
        .child(title(child.title.clone()))
        .children(
            agent_label(child.provider.as_deref(), child.model.as_deref()).map(|agent| {
                Label::new(agent)
                    .size(LabelSize::XSmall)
                    .color(Color::Muted)
            }),
        )
        .child(status_label(&child.activity, cx))
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
        .into_any_element()
    }

    fn render_task(&self, index: usize, task: &SubagentTask, cx: &mut Context<Self>) -> AnyElement {
        let color = activity_color(&task.activity, cx);
        self.row(("family-task", index), format!("family-task-{}", task.id))
            .child(Indicator::dot().color(Color::Custom(color)))
            .child(title(task.title.clone()))
            .children(
                agent_label(task.provider.as_deref(), task.model.as_deref()).map(|agent| {
                    Label::new(agent)
                        .size(LabelSize::XSmall)
                        .color(Color::Muted)
                }),
            )
            .child(status_label(&task.activity, cx))
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

    fn render_workflow(
        &self,
        index: usize,
        workflow: &Workflow,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let palette = palette(cx);
        let expanded = self.expanded_workflows.contains(&workflow.subagent_id);
        let subagent_id = workflow.subagent_id.clone();
        let header = h_flex()
            .id(("family-workflow", index))
            .w_full()
            .h(px(26.))
            .gap_1p5()
            .child(
                h_flex()
                    .id(("family-workflow-toggle", index))
                    .debug_selector(|| format!("family-workflow-toggle-{subagent_id}"))
                    .flex_1()
                    .min_w_0()
                    .gap_1p5()
                    .cursor_pointer()
                    .on_click(cx.listener(move |this, _, _, cx| {
                        if !this.expanded_workflows.remove(&subagent_id) {
                            this.expanded_workflows.insert(subagent_id.clone());
                        }
                        cx.notify();
                    }))
                    .child(
                        Icon::new(if expanded {
                            IconName::ChevronDown
                        } else {
                            IconName::ChevronRight
                        })
                        .size(IconSize::XSmall)
                        .color(Color::Muted),
                    )
                    .child(section_label("Workflow"))
                    .child(
                        div().min_w_0().child(
                            Label::new(workflow.name.clone())
                                .size(LabelSize::Small)
                                .truncate(),
                        ),
                    )
                    .child(
                        Label::new(workflow.summary())
                            .size(LabelSize::XSmall)
                            .color(Color::Muted),
                    )
                    .child(div().flex_1()),
            )
            .child(status_label(&workflow.activity, cx))
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
                .ml(px(20.))
                .mb_1()
                .max_h(px(240.))
                .overflow_y_scroll()
                .px_2()
                .py_1p5()
                .rounded(tokens::CONTROL_RADIUS)
                .border(tokens::BORDER_WIDTH)
                .border_color(palette.border.hsla())
                .bg(palette.background.hsla())
                .child(body)
        });
        v_flex()
            .w_full()
            .child(header)
            .children(script)
            .when(expanded, |this| {
                this.children(
                    workflow
                        .phases
                        .iter()
                        .enumerate()
                        .map(|(phase_index, phase)| {
                            v_flex()
                                .w_full()
                                .when_some(phase.title.clone(), |this, phase_title| {
                                    this.child(
                                        h_flex()
                                            .h(px(22.))
                                            .pl(px(20.))
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
                                            cx,
                                        )
                                    },
                                ))
                        }),
                )
                .when(workflow.phases.is_empty(), |this| {
                    this.child(
                        div().h(px(22.)).pl(px(20.)).child(
                            Label::new("No agents yet")
                                .size(LabelSize::XSmall)
                                .color(Color::Muted),
                        ),
                    )
                })
            })
            .into_any_element()
    }

    fn render_member(
        &self,
        index: usize,
        member: &WorkflowMember,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let palette = palette(cx);
        let color = activity_color(&member.activity, cx);
        let label = member.label.clone();
        self.row(
            ("family-workflow-member", index),
            format!("family-workflow-member-{}", member.label),
        )
        .pl(px(32.))
        .when_some(
            member.thread_id.clone().filter(|_| member.available),
            |this, thread_id| {
                this.cursor_pointer()
                    .hover(|style| style.bg(palette.surface_2.hsla()))
                    .tooltip(Tooltip::text("Open this agent's thread"))
                    .on_click(cx.listener(move |this, _, window, cx| {
                        this.open(thread_id.clone(), label.clone(), window, cx)
                    }))
            },
        )
        .child(Indicator::dot().color(Color::Custom(color)))
        .child(title(member.label.clone()))
        .child(status_label(&member.activity, cx))
        .into_any_element()
    }

    fn render_background(&self, work: &BackgroundWork, cx: &mut Context<Self>) -> AnyElement {
        let key = format!(
            "background:{}",
            self.thread_id.as_deref().unwrap_or_default()
        );
        h_flex()
            .w_full()
            .h(px(26.))
            .gap_1p5()
            .child(Indicator::dot().color(Color::Custom(palette(cx).text_muted.hsla())))
            .child(section_label("Background"))
            .child(title(work.title.clone()))
            .children(
                work.threads
                    .iter()
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

impl Render for ThreadFamilyBar {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        if self.family.is_empty() && self.error.is_none() {
            // With nothing to show, `hidden` keeps the bar out of the view's layout.
            return div().hidden().into_any_element();
        }
        let palette = palette(cx);
        let family = self.family.clone();
        v_flex()
            .flex_none()
            .w_full()
            .px_4()
            .py(px(4.))
            .border_b(tokens::BORDER_WIDTH)
            .border_color(palette.border.hsla())
            .bg(palette.surface.hsla())
            .children(
                family
                    .parent
                    .as_ref()
                    .map(|parent| self.render_parent(parent, cx)),
            )
            .children(self.render_children(cx))
            .children(
                family
                    .workflows
                    .iter()
                    .enumerate()
                    .map(|(index, workflow)| self.render_workflow(index, workflow, cx)),
            )
            .children(
                family
                    .background
                    .as_ref()
                    .map(|work| self.render_background(work, cx)),
            )
            .when(family.waiting_on_thread, |this| {
                this.child(
                    h_flex()
                        .h(px(24.))
                        .gap_1p5()
                        .child(
                            Icon::new(IconName::Clock)
                                .size(IconSize::XSmall)
                                .color(Color::Muted),
                        )
                        .child(
                            Label::new("Waiting on another thread")
                                .size(LabelSize::XSmall)
                                .color(Color::Muted),
                        ),
                )
            })
            .when_some(self.error.clone(), |this, error| {
                this.child(
                    div()
                        .debug_selector(|| "family-error".into())
                        .py_0p5()
                        .child(
                            Label::new(error)
                                .size(LabelSize::XSmall)
                                .color(Color::Error),
                        ),
                )
            })
            .into_any_element()
    }
}

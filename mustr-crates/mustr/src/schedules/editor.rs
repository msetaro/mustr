//! The form for a new scheduled task, or for editing one: its name, prompt, project, agent and
//! model, where it runs, which thread it messages, its permission mode, and when it runs.
//! Saving sends `scheduledTasks.upsert`. A webhook task's trigger is shown, not edited.

use super::draft::{Draft, RUNTIME_MODES, ScheduleMode, WEEKDAYS, WorkspaceMode};
use editor::Editor;
use gpui::{
    AnyElement, App, AppContext as _, Context, Entity, EventEmitter, FontWeight,
    InteractiveElement as _, IntoElement, ParentElement as _, Render, SharedString,
    StatefulInteractiveElement as _, Styled as _, Task, WeakEntity, Window, div, px,
};
use mustr_ui::{ButtonStyle, Card, SegmentedControl, TextButton, palette, tokens, tokens::scaled};
use serde_json::json;
use std::sync::Arc;
use t3_client::t3_api::{
    rpc,
    types::{ModelSelection, ScheduledTask},
};
use thread_chat::{
    Access, AgentChoice, ThreadBackend, ThreadBackendExt as _, thread_settings::ModelChoice,
};
use ui::{
    Color, ContextMenu, DropdownMenu, DropdownStyle, IconPosition, Label, LabelCommon as _,
    LabelSize, Switch, ToggleState, Tooltip, prelude::*,
};

/// Threads offered as a task's target, newest first.
const THREAD_CHOICES: usize = 30;

#[derive(Clone, Debug, PartialEq)]
pub struct ProjectChoice {
    pub id: String,
    pub title: String,
}

#[derive(Clone, Debug, PartialEq)]
pub struct ThreadChoice {
    pub id: String,
    pub title: String,
    pub project_id: String,
}

/// What the form's pickers offer.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Choices {
    pub projects: Vec<ProjectChoice>,
    /// Newest first.
    pub threads: Vec<ThreadChoice>,
    pub agents: Vec<AgentChoice>,
}

pub enum TaskEditorEvent {
    Saved(Box<ScheduledTask>),
    Cancelled,
}

pub struct TaskEditor {
    backend: Arc<dyn ThreadBackend>,
    pub(super) draft: Draft,
    choices: Choices,
    pub(super) title: Entity<Editor>,
    pub(super) prompt: Entity<Editor>,
    base_ref: Entity<Editor>,
    worktree_path: Entity<Editor>,
    pub(super) time_of_day: Entity<Editor>,
    pub(super) interval: Entity<Editor>,
    saving: bool,
    error: Option<SharedString>,
    _save: Option<Task<()>>,
}

impl EventEmitter<TaskEditorEvent> for TaskEditor {}

fn single_line(
    text: &str,
    placeholder: &str,
    window: &mut Window,
    cx: &mut Context<TaskEditor>,
) -> Entity<Editor> {
    cx.new(|cx| {
        let mut editor = Editor::single_line(window, cx);
        editor.set_placeholder_text(placeholder, window, cx);
        editor.set_text(text, window, cx);
        editor
    })
}

pub fn model_selection(instance_id: &str, model: &str) -> ModelSelection {
    ModelSelection {
        instance_id: Some(json!(instance_id)),
        model: json!(model),
        options: None,
        provider: None,
    }
}

fn selection_ids(selection: &ModelSelection) -> (Option<&str>, Option<&str>) {
    (
        selection.instance_id.as_ref().and_then(|id| id.as_str()),
        selection.model.as_str(),
    )
}

impl TaskEditor {
    pub fn new(
        backend: Arc<dyn ThreadBackend>,
        draft: Draft,
        choices: Choices,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        let mut draft = draft;
        if draft.project_id.is_none() {
            draft.project_id = choices.projects.first().map(|project| project.id.clone());
        }
        if draft.model.is_none()
            && let Some(agent) = choices.agents.first()
            && let Some(ModelChoice { slug, .. }) = agent
                .default_model
                .and_then(|index| agent.models.get(index))
                .or(agent.models.first())
        {
            draft.model = Some(model_selection(&agent.instance_id, slug));
        }
        let prompt = cx.new(|cx| {
            let mut editor = Editor::auto_height(3, 12, window, cx);
            editor.set_placeholder_text(
                "What should the agent do each time this runs?",
                window,
                cx,
            );
            editor.set_text(draft.prompt.as_str(), window, cx);
            editor
        });
        Self {
            title: single_line(&draft.title, "e.g. Check for new Sentry issues", window, cx),
            prompt,
            base_ref: single_line(&draft.base_ref, "main", window, cx),
            worktree_path: single_line(&draft.existing_worktree, "/path/to/checkout", window, cx),
            time_of_day: single_line(&draft.time_of_day, "09:00", window, cx),
            interval: single_line(&draft.interval_minutes, "15", window, cx),
            backend,
            draft,
            choices,
            saving: false,
            error: None,
            _save: None,
        }
    }

    pub fn is_editing(&self) -> bool {
        self.draft.editing.is_some()
    }

    /// The draft with what's typed in the text fields.
    pub(super) fn current_draft(&self, cx: &App) -> Draft {
        let mut draft = self.draft.clone();
        draft.title = self.title.read(cx).text(cx);
        draft.prompt = self.prompt.read(cx).text(cx);
        draft.base_ref = self.base_ref.read(cx).text(cx);
        draft.existing_worktree = self.worktree_path.read(cx).text(cx);
        draft.time_of_day = self.time_of_day.read(cx).text(cx);
        draft.interval_minutes = self.interval.read(cx).text(cx);
        draft
    }

    pub(super) fn save(&mut self, cx: &mut Context<Self>) {
        if self.saving {
            return;
        }
        let input = match self.current_draft(cx).upsert_input() {
            Ok(input) => input,
            Err(error) => {
                self.error = Some(error.into());
                cx.notify();
                return;
            }
        };
        let save = self
            .backend
            .call::<rpc::ScheduledTasksUpsert>(input, Access::Operate);
        self.saving = true;
        self.error = None;
        cx.notify();
        self._save = Some(cx.spawn(async move |this, cx| {
            let result = save.await;
            this.update(cx, |this, cx| {
                this.saving = false;
                match result {
                    Ok(saved) => cx.emit(TaskEditorEvent::Saved(Box::new(saved.task))),
                    Err(error) => this.error = Some(format!("{error:#}").into()),
                }
                cx.notify();
            })
            .ok();
        }));
    }

    fn agent_index(&self) -> Option<usize> {
        let (instance_id, _) = selection_ids(self.draft.model.as_ref()?);
        self.choices
            .agents
            .iter()
            .position(|agent| Some(agent.instance_id.as_str()) == instance_id)
    }

    fn select_project(&mut self, index: usize, cx: &mut Context<Self>) {
        let Some(project) = self.choices.projects.get(index) else {
            return;
        };
        if self.draft.project_id.as_deref() != Some(project.id.as_str()) {
            self.draft.project_id = Some(project.id.clone());
            // A thread belongs to its project.
            self.draft.thread_id = None;
        }
        cx.notify();
    }

    fn select_agent(&mut self, index: usize, cx: &mut Context<Self>) {
        let Some(agent) = self.choices.agents.get(index) else {
            return;
        };
        let slug = agent
            .default_model
            .and_then(|index| agent.models.get(index))
            .or(agent.models.first())
            .map(|model| model.slug.clone())
            .unwrap_or_default();
        self.draft.model = Some(model_selection(&agent.instance_id, &slug));
        cx.notify();
    }

    fn select_model(&mut self, index: usize, cx: &mut Context<Self>) {
        let Some(agent) = self
            .agent_index()
            .and_then(|index| self.choices.agents.get(index))
        else {
            return;
        };
        if let Some(ModelChoice { slug, .. }) = agent.models.get(index) {
            let unchanged = self.draft.model.as_ref().is_some_and(|model| {
                selection_ids(model) == (Some(agent.instance_id.as_str()), Some(slug.as_str()))
            });
            // Re-picking the model keeps its options, as T3's form does.
            if !unchanged {
                self.draft.model = Some(model_selection(&agent.instance_id, slug));
            }
        }
        cx.notify();
    }

    fn project_threads(&self) -> Vec<&ThreadChoice> {
        self.choices
            .threads
            .iter()
            .filter(|thread| Some(thread.project_id.as_str()) == self.draft.project_id.as_deref())
            .take(THREAD_CHOICES)
            .collect()
    }

    fn select_thread(&mut self, index: usize, cx: &mut Context<Self>) {
        // The first entry is "A new thread each run".
        self.draft.thread_id = index.checked_sub(1).and_then(|index| {
            self.project_threads()
                .get(index)
                .map(|thread| thread.id.clone())
        });
        cx.notify();
    }

    fn dropdown(
        &self,
        id: &'static str,
        label: String,
        entries: Vec<String>,
        selected: Option<usize>,
        on_select: fn(&mut Self, usize, &mut Context<Self>),
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let this: WeakEntity<Self> = cx.entity().downgrade();
        let key = SharedString::from(format!(
            "{id}-{}-{}",
            entries.len(),
            selected.unwrap_or(usize::MAX)
        ));
        let menu = window.use_keyed_state(key, cx, move |window, cx| {
            ContextMenu::new(window, cx, move |mut menu, _, _| {
                for (index, entry) in entries.into_iter().enumerate() {
                    let this = this.clone();
                    menu = menu.toggleable_entry(
                        entry,
                        selected == Some(index),
                        IconPosition::End,
                        None,
                        move |_, cx| {
                            this.update(cx, |this, cx| on_select(this, index, cx)).ok();
                        },
                    );
                }
                menu
            })
        });
        DropdownMenu::new(id, label, menu)
            .style(DropdownStyle::Outlined)
            .into_any_element()
    }
}

/// A labeled row of the editor. A control too wide to go beside its label (a big `ui_font_size`
/// in a narrow window) goes below it.
fn field(label: &'static str, control: impl IntoElement) -> impl IntoElement {
    h_flex()
        .w_full()
        .flex_wrap()
        .gap_3()
        .items_start()
        .child(
            div()
                .w(scaled(px(96.)))
                .flex_none()
                .pt(scaled(px(3.)))
                .child(Label::new(label).size(LabelSize::Small).color(Color::Muted)),
        )
        .child(
            h_flex()
                .flex_auto()
                .min_w_0()
                .flex_wrap()
                .gap_2()
                .child(control),
        )
}

fn text_box(editor: &Entity<Editor>, width: Option<f32>, cx: &App) -> impl IntoElement {
    let palette = palette(cx);
    div()
        .when_some(width, |this, width| this.w(scaled(px(width))))
        .when(width.is_none(), |this| this.flex_1().min_w_0())
        .px_2()
        .py_0p5()
        .rounded(scaled(tokens::CONTROL_RADIUS))
        .border(tokens::BORDER_WIDTH)
        .border_color(palette.border.hsla())
        .bg(palette.background.hsla())
        .child(editor.clone())
}

fn note(text: impl Into<SharedString>) -> impl IntoElement {
    Label::new(text.into())
        .size(LabelSize::XSmall)
        .color(Color::Muted)
}

impl Render for TaskEditor {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let palette = palette(cx);
        let this = cx.entity().downgrade();

        let projects = &self.choices.projects;
        let project_index = self
            .draft
            .project_id
            .as_deref()
            .and_then(|id| projects.iter().position(|project| project.id == id));
        let project_control = if projects.is_empty() {
            note("T3 has no projects yet. Add one from Home.").into_any_element()
        } else {
            self.dropdown(
                "schedule-project",
                project_index
                    .and_then(|index| projects.get(index))
                    .map_or_else(
                        || "Pick a project".to_string(),
                        |project| project.title.clone(),
                    ),
                projects
                    .iter()
                    .map(|project| project.title.clone())
                    .collect(),
                project_index,
                Self::select_project,
                window,
                cx,
            )
        };

        let agent_index = self.agent_index();
        let agent = agent_index.and_then(|index| self.choices.agents.get(index));
        let (_, model_slug) = self
            .draft
            .model
            .as_ref()
            .map(selection_ids)
            .unwrap_or_default();
        let model_index = agent.and_then(|agent| {
            agent
                .models
                .iter()
                .position(|model| Some(model.slug.as_str()) == model_slug)
        });
        let agent_control = if self.choices.agents.is_empty() {
            note(match &self.draft.model {
                Some(model) => match selection_ids(model) {
                    (Some(instance), Some(model)) => format!("{instance} · {model}"),
                    _ => "No agent is set up in T3.".to_string(),
                },
                None => "No agent is set up in T3.".to_string(),
            })
            .into_any_element()
        } else {
            h_flex()
                .gap_2()
                .child(
                    self.dropdown(
                        "schedule-agent",
                        agent.map_or_else(
                            || "Pick an agent".to_string(),
                            |agent| agent.name.clone(),
                        ),
                        self.choices
                            .agents
                            .iter()
                            .map(|agent| agent.name.clone())
                            .collect(),
                        agent_index,
                        Self::select_agent,
                        window,
                        cx,
                    ),
                )
                .children(agent.filter(|agent| !agent.models.is_empty()).map(|agent| {
                    self.dropdown(
                        "schedule-model",
                        model_index
                            .and_then(|index| agent.models.get(index))
                            .map(|model| model.name.clone())
                            .or_else(|| model_slug.map(str::to_string))
                            .unwrap_or_else(|| "Pick a model".to_string()),
                        agent
                            .models
                            .iter()
                            .map(|model| model.name.clone())
                            .collect(),
                        model_index,
                        Self::select_model,
                        window,
                        cx,
                    )
                }))
                .into_any_element()
        };

        let workspace = self.draft.workspace;
        let workspace_control = if self.draft.unknown_workspace.is_some() {
            note("Where T3 Code set it to run.").into_any_element()
        } else {
            h_flex()
                .gap_2()
                .child(
                    SegmentedControl::new("schedule-workspace")
                        .segment("New worktree")
                        .segment("Project checkout")
                        .segment("Existing checkout")
                        .selected(match workspace {
                            WorkspaceMode::NewWorktree => 0,
                            WorkspaceMode::Checkout => 1,
                            WorkspaceMode::ExistingWorktree => 2,
                        })
                        .on_select({
                            let this = this.clone();
                            move |index, _, cx| {
                                this.update(cx, |this, cx| {
                                    this.draft.workspace = match index {
                                        0 => WorkspaceMode::NewWorktree,
                                        1 => WorkspaceMode::Checkout,
                                        _ => WorkspaceMode::ExistingWorktree,
                                    };
                                    cx.notify();
                                })
                                .ok();
                            }
                        }),
                )
                .when(workspace == WorkspaceMode::NewWorktree, |this| {
                    this.child(note("from"))
                        .child(text_box(&self.base_ref, Some(140.), cx))
                })
                .when(workspace == WorkspaceMode::ExistingWorktree, |this| {
                    this.child(text_box(&self.worktree_path, Some(260.), cx))
                })
                .into_any_element()
        };

        let threads = self.project_threads();
        let thread_index = self
            .draft
            .thread_id
            .as_deref()
            .and_then(|id| threads.iter().position(|thread| thread.id == id))
            .map(|index| index + 1);
        let thread_label = match (thread_index, &self.draft.thread_id) {
            (Some(index), _) => threads[index - 1].title.clone(),
            // A thread the shell no longer lists.
            (None, Some(thread_id)) => thread_id.clone(),
            (None, None) => "A new thread each run".to_string(),
        };
        let thread_entries = std::iter::once("A new thread each run".to_string())
            .chain(threads.iter().map(|thread| thread.title.clone()))
            .collect();
        let thread_control = self.dropdown(
            "schedule-thread",
            thread_label,
            thread_entries,
            Some(thread_index.unwrap_or(0)),
            Self::select_thread,
            window,
            cx,
        );

        let runtime_index = RUNTIME_MODES
            .iter()
            .position(|(mode, _)| *mode == self.draft.runtime_mode)
            .unwrap_or(RUNTIME_MODES.len() - 1);
        let mut runtime_control = SegmentedControl::new("schedule-runtime-mode")
            .selected(runtime_index)
            .on_select({
                let this = this.clone();
                move |index, _, cx| {
                    this.update(cx, |this, cx| {
                        if let Some((mode, _)) = RUNTIME_MODES.get(index) {
                            this.draft.runtime_mode = mode.clone();
                            cx.notify();
                        }
                    })
                    .ok();
                }
            });
        for (_, label) in RUNTIME_MODES {
            runtime_control = runtime_control.segment(label);
        }

        let schedule = self.draft.schedule;
        let schedule_control: AnyElement = if schedule == ScheduleMode::Webhook {
            note("On webhook. Its URL, signature check, and filters are edited in T3 Code.")
                .into_any_element()
        } else {
            v_flex()
                .items_start()
                .gap_2()
                .child(
                    SegmentedControl::new("schedule-kind")
                        .segment("At a time")
                        .segment("Every interval")
                        .selected(if schedule == ScheduleMode::AtTime {
                            0
                        } else {
                            1
                        })
                        .on_select({
                            let this = this.clone();
                            move |index, _, cx| {
                                this.update(cx, |this, cx| {
                                    this.draft.schedule = if index == 0 {
                                        ScheduleMode::AtTime
                                    } else {
                                        ScheduleMode::Every
                                    };
                                    cx.notify();
                                })
                                .ok();
                            }
                        }),
                )
                .child(if schedule == ScheduleMode::AtTime {
                    h_flex()
                        .gap_2()
                        .child(note("at"))
                        .child(text_box(&self.time_of_day, Some(64.), cx))
                        .child(note("on"))
                        .children(WEEKDAYS.iter().map(|(day, label)| {
                            let selected = self.draft.weekdays.contains(day);
                            let day = *day;
                            let this = this.clone();
                            div()
                                .id(("schedule-weekday", day as usize))
                                .debug_selector(move || format!("schedule-weekday-{day}"))
                                .w(scaled(px(26.)))
                                .py(scaled(px(1.)))
                                .text_center()
                                .rounded(scaled(tokens::CONTROL_RADIUS))
                                .border(tokens::BORDER_WIDTH)
                                .text_size(tokens::text_size(tokens::TEXT_SIZE_SMALL))
                                .cursor_pointer()
                                .map(|this| {
                                    if selected {
                                        this.border_color(palette.accent.hsla().opacity(0.5))
                                            .bg(palette.accent_soft.hsla())
                                            .text_color(palette.accent.hsla())
                                    } else {
                                        this.border_color(palette.border.hsla())
                                            .text_color(palette.text_muted.hsla())
                                            .hover(|style| style.bg(palette.surface_2.hsla()))
                                    }
                                })
                                .on_click(move |_, _, cx| {
                                    this.update(cx, |this, cx| {
                                        if !this.draft.weekdays.remove(&day) {
                                            this.draft.weekdays.insert(day);
                                        }
                                        cx.notify();
                                    })
                                    .ok();
                                })
                                .child(*label)
                        }))
                        .into_any_element()
                } else {
                    h_flex()
                        .gap_2()
                        .child(note("every"))
                        .child(text_box(&self.interval, Some(64.), cx))
                        .child(note("minutes"))
                        .into_any_element()
                })
                .child(note("Times are on the clock of the machine T3 runs on."))
                .into_any_element()
        };

        let enabled = self.draft.enabled;
        let enabled_control = Switch::new(
            "schedule-enabled",
            if enabled {
                ToggleState::Selected
            } else {
                ToggleState::Unselected
            },
        )
        .label(if enabled {
            "Runs on schedule"
        } else {
            "Saved, but doesn't run"
        })
        .on_click(move |_, _, cx| {
            this.update(cx, |this, cx| {
                this.draft.enabled = !this.draft.enabled;
                cx.notify();
            })
            .ok();
        });

        let blocked = self.backend.blocked_reason(Access::Operate);
        let editing = self.is_editing();
        Card::new()
            .gap_3()
            .px(scaled(px(14.)))
            .py(scaled(px(12.)))
            .child(
                v_flex()
                    .gap_0p5()
                    .child(
                        Label::new(if editing { "Edit task" } else { "New task" })
                            .weight(FontWeight::SEMIBOLD),
                    )
                    .child(note(
                        "T3 runs the prompt on its own: at a time, on an interval, or when a webhook is called.",
                    )),
            )
            .child(field("Name", text_box(&self.title, None, cx)))
            .child(field("Prompt", text_box(&self.prompt, None, cx)))
            .child(field("Project", project_control))
            .child(field("Agent", agent_control))
            .child(field("Works in", workspace_control))
            .child(field("Messages", thread_control))
            .child(field("Permissions", runtime_control))
            .child(field("Runs", schedule_control))
            .child(field("Enabled", enabled_control))
            .when_some(self.error.clone(), |this, error| {
                this.child(
                    div()
                        .debug_selector(|| "schedule-editor-error".into())
                        .child(Label::new(error).size(LabelSize::Small).color(Color::Error)),
                )
            })
            .child(
                h_flex()
                    .w_full()
                    .justify_end()
                    .gap_2()
                    .child(
                        div().debug_selector(|| "schedule-cancel".into()).child(
                            TextButton::new("schedule-cancel", "Cancel")
                                .disabled(self.saving)
                                .on_click(cx.listener(|_, _, _, cx| {
                                    cx.emit(TaskEditorEvent::Cancelled)
                                })),
                        ),
                    )
                    .child(
                        div()
                            .id("schedule-save-wrapper")
                            .debug_selector(|| "schedule-save".into())
                            .when_some(blocked.clone(), |this, reason| {
                                this.tooltip(Tooltip::text(reason))
                            })
                            .child(
                                TextButton::new(
                                    "schedule-save",
                                    match (self.saving, editing) {
                                        (true, _) => "Saving…",
                                        (false, true) => "Save task",
                                        (false, false) => "Create task",
                                    },
                                )
                                .style(ButtonStyle::Primary)
                                .disabled(self.saving || blocked.is_some())
                                .on_click(cx.listener(|this, _, _, cx| this.save(cx))),
                            ),
                    ),
            )
    }
}

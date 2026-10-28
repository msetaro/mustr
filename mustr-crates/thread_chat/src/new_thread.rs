//! The form of a draft thread: which project, which agent and model, and whether the agent works
//! in the project's checkout or in a new worktree. The project list comes from T3's shell, and the
//! agents and their models from T3's provider catalog (`server.getConfig`).

use crate::{
    backend::ThreadBackend,
    commands::{NewThread, Workspace as ThreadWorkspace},
};
use editor::Editor;
use gpui::{
    App, AppContext as _, Context, Entity, EventEmitter, FontWeight, IntoElement,
    ParentElement as _, Render, SharedString, Styled as _, Task, WeakEntity, Window, div, px,
};
use mustr_ui::{Card, SegmentedControl, palette, tokens};
use serde_json::json;
use std::sync::Arc;
use t3_api::types::{
    ModelSelection, ProviderInteractionMode, RuntimeMode, ServerProvider, ServerProviderState,
};
use ui::{
    Color, ContextMenu, DropdownMenu, DropdownStyle, IconPosition, Label, LabelCommon as _,
    LabelSize, prelude::*,
};

pub enum NewThreadFormEvent {
    Changed,
}

#[derive(Clone, Debug, PartialEq)]
pub struct ProjectChoice {
    pub id: String,
    pub title: String,
    pub default_model: Option<ModelSelection>,
}

#[derive(Clone, Debug, PartialEq)]
pub struct AgentChoice {
    pub instance_id: String,
    pub name: String,
    /// `(name, slug)`.
    pub models: Vec<(String, String)>,
    pub default_model: Option<usize>,
    pub supported_runtime_modes: Option<Vec<RuntimeMode>>,
}

enum Loadable<T> {
    Loading,
    Failed(SharedString),
    Loaded(T),
}

pub struct NewThreadForm {
    projects: Loadable<Vec<ProjectChoice>>,
    agents: Loadable<Vec<AgentChoice>>,
    project: Option<usize>,
    agent: Option<usize>,
    model: Option<usize>,
    use_worktree: bool,
    base_ref: Entity<Editor>,
    _tasks: Vec<Task<()>>,
}

impl EventEmitter<NewThreadFormEvent> for NewThreadForm {}

/// The agents a thread can start with: enabled, installed, and not reporting an error.
pub fn agent_choices(providers: &[ServerProvider]) -> Vec<AgentChoice> {
    providers
        .iter()
        .filter(|provider| {
            provider.enabled
                && provider.installed
                && !matches!(
                    provider.status,
                    ServerProviderState::Error | ServerProviderState::Disabled
                )
        })
        .map(|provider| {
            let models = provider
                .models
                .iter()
                .map(|model| (model.name.clone(), model.slug.clone()))
                .collect::<Vec<_>>();
            AgentChoice {
                instance_id: provider.instance_id.clone(),
                name: provider
                    .display_name
                    .clone()
                    .filter(|name| !name.trim().is_empty())
                    .unwrap_or_else(|| provider.driver.clone()),
                default_model: provider
                    .models
                    .iter()
                    .position(|model| model.is_default == Some(true))
                    .or((!models.is_empty()).then_some(0)),
                models,
                supported_runtime_modes: provider.supported_runtime_modes.clone(),
            }
        })
        .collect()
}

/// New threads ask before the agent runs commands or edits files (`mustr-docs/PLAN.md` §8),
/// unless the agent doesn't support that.
pub fn runtime_mode_for(agent: &AgentChoice) -> RuntimeMode {
    match &agent.supported_runtime_modes {
        Some(modes) if !modes.contains(&RuntimeMode::ApprovalRequired) => modes
            .first()
            .cloned()
            .unwrap_or(RuntimeMode::ApprovalRequired),
        _ => RuntimeMode::ApprovalRequired,
    }
}

impl NewThreadForm {
    pub fn new(
        backend: Arc<dyn ThreadBackend>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        let base_ref = cx.new(|cx| {
            let mut editor = Editor::single_line(window, cx);
            editor.set_placeholder_text("Base branch", window, cx);
            editor.set_text("main", window, cx);
            editor
        });
        let load_projects = cx.spawn({
            let backend = backend.clone();
            async move |this, cx| {
                let mut attempt = 0;
                loop {
                    let shell = backend.shell_snapshot().await;
                    let loaded = this.update(cx, |this, cx| this.projects_loaded(shell, cx));
                    if !matches!(loaded, Ok(false)) {
                        return;
                    }
                    cx.background_executor()
                        .timer(crate::backend::retry_delay(attempt))
                        .await;
                    attempt += 1;
                }
            }
        });
        let load_agents = cx.spawn(async move |this, cx| {
            let mut attempt = 0;
            loop {
                let providers = backend.providers().await;
                let loaded = this.update(cx, |this, cx| this.agents_loaded(providers, cx));
                if !matches!(loaded, Ok(false)) {
                    return;
                }
                cx.background_executor()
                    .timer(crate::backend::retry_delay(attempt))
                    .await;
                attempt += 1;
            }
        });
        Self {
            projects: Loadable::Loading,
            agents: Loadable::Loading,
            project: None,
            agent: None,
            model: None,
            use_worktree: true,
            base_ref,
            _tasks: vec![load_projects, load_agents],
        }
    }

    /// Returns whether the projects loaded.
    fn projects_loaded(
        &mut self,
        shell: anyhow::Result<t3_api::types::OrchestrationV2ShellSnapshot>,
        cx: &mut Context<Self>,
    ) -> bool {
        let loaded = shell.is_ok();
        self.projects = match shell {
            Ok(shell) => {
                let mut projects = shell
                    .projects
                    .into_iter()
                    .map(|project| ProjectChoice {
                        id: project.id,
                        title: project.title,
                        default_model: project.default_model_selection,
                    })
                    .collect::<Vec<_>>();
                projects.sort_by_key(|project| project.title.to_lowercase());
                Loadable::Loaded(projects)
            }
            Err(error) => Loadable::Failed(format!("{error:#}").into()),
        };
        if self.project.is_none() && self.projects().is_some_and(|p| !p.is_empty()) {
            self.select_project(0, cx);
        }
        cx.emit(NewThreadFormEvent::Changed);
        cx.notify();
        loaded
    }

    /// Returns whether the agents loaded.
    fn agents_loaded(
        &mut self,
        providers: anyhow::Result<Vec<ServerProvider>>,
        cx: &mut Context<Self>,
    ) -> bool {
        let loaded = providers.is_ok();
        self.agents = match providers {
            Ok(providers) => Loadable::Loaded(agent_choices(&providers)),
            Err(error) => Loadable::Failed(format!("{error:#}").into()),
        };
        if self.agent.is_none() && self.agents().is_some_and(|a| !a.is_empty()) {
            self.select_agent(0, cx);
        }
        self.apply_project_default(cx);
        cx.emit(NewThreadFormEvent::Changed);
        cx.notify();
        loaded
    }

    fn projects(&self) -> Option<&[ProjectChoice]> {
        match &self.projects {
            Loadable::Loaded(projects) => Some(projects),
            _ => None,
        }
    }

    fn agents(&self) -> Option<&[AgentChoice]> {
        match &self.agents {
            Loadable::Loaded(agents) => Some(agents),
            _ => None,
        }
    }

    fn selected_agent(&self) -> Option<&AgentChoice> {
        self.agents()?.get(self.agent?)
    }

    pub fn select_project(&mut self, index: usize, cx: &mut Context<Self>) {
        self.project = Some(index);
        self.apply_project_default(cx);
        cx.emit(NewThreadFormEvent::Changed);
        cx.notify();
    }

    pub fn select_agent(&mut self, index: usize, cx: &mut Context<Self>) {
        self.agent = Some(index);
        self.model = self
            .agents()
            .and_then(|agents| agents.get(index))
            .and_then(|agent| agent.default_model);
        cx.emit(NewThreadFormEvent::Changed);
        cx.notify();
    }

    pub fn select_model(&mut self, index: usize, cx: &mut Context<Self>) {
        self.model = Some(index);
        cx.emit(NewThreadFormEvent::Changed);
        cx.notify();
    }

    pub fn set_use_worktree(&mut self, use_worktree: bool, cx: &mut Context<Self>) {
        self.use_worktree = use_worktree;
        cx.emit(NewThreadFormEvent::Changed);
        cx.notify();
    }

    /// Picks the agent and model the project starts threads with, when T3 has one for it.
    fn apply_project_default(&mut self, cx: &mut Context<Self>) {
        let Some(default) = self
            .projects()
            .and_then(|projects| projects.get(self.project?))
            .and_then(|project| project.default_model.clone())
        else {
            return;
        };
        let Some(agents) = self.agents() else {
            return;
        };
        let instance_id = default.instance_id.as_ref().and_then(|id| id.as_str());
        let Some(agent_index) = agents
            .iter()
            .position(|agent| Some(agent.instance_id.as_str()) == instance_id)
        else {
            return;
        };
        let agent = &agents[agent_index];
        let model = agent
            .models
            .iter()
            .position(|(_, slug)| default.model.as_str() == Some(slug.as_str()))
            .or(agent.default_model);
        self.agent = Some(agent_index);
        self.model = model;
        cx.notify();
    }

    /// The thread to launch with `text` as its first message, or why it can't be launched yet.
    pub fn new_thread(&self, text: &str, cx: &App) -> Result<NewThread, SharedString> {
        let project = self
            .projects()
            .and_then(|projects| projects.get(self.project?))
            .ok_or_else(|| SharedString::from("Pick a project for the thread."))?;
        let agent = self
            .selected_agent()
            .ok_or_else(|| SharedString::from("Pick an agent for the thread."))?;
        let (_, slug) = self
            .model
            .and_then(|model| agent.models.get(model))
            .ok_or_else(|| SharedString::from("Pick a model for the thread."))?;
        let workspace = if self.use_worktree {
            let base_ref = self.base_ref.read(cx).text(cx).trim().to_string();
            if base_ref.is_empty() {
                return Err("Name the branch the worktree starts from.".into());
            }
            ThreadWorkspace::NewWorktree {
                base_ref,
                start_from_origin: true,
            }
        } else {
            ThreadWorkspace::ProjectRoot { branch: None }
        };
        Ok(NewThread {
            project_id: project.id.clone(),
            model_selection: ModelSelection {
                instance_id: Some(json!(agent.instance_id)),
                model: json!(slug),
                options: None,
                provider: None,
            },
            runtime_mode: runtime_mode_for(agent),
            interaction_mode: ProviderInteractionMode::Default,
            workspace,
            text: text.to_string(),
        })
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
    ) -> impl IntoElement + use<> {
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
        DropdownMenu::new(id, label, menu).style(DropdownStyle::Outlined)
    }
}

fn field(label: &'static str, control: impl IntoElement) -> impl IntoElement {
    h_flex()
        .gap_3()
        .child(
            div()
                .w(px(88.))
                .flex_none()
                .child(Label::new(label).size(LabelSize::Small).color(Color::Muted)),
        )
        .child(h_flex().flex_1().min_w_0().child(control))
}

impl Render for NewThreadForm {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let palette = palette(cx);
        let project_control = match &self.projects {
            Loadable::Loading => Label::new("Loading projects…")
                .size(LabelSize::Small)
                .color(Color::Muted)
                .into_any_element(),
            Loadable::Failed(error) => Label::new(error.clone())
                .size(LabelSize::Small)
                .color(Color::Error)
                .into_any_element(),
            Loadable::Loaded(projects) if projects.is_empty() => {
                Label::new("T3 has no projects. Add one in T3 Code first.")
                    .size(LabelSize::Small)
                    .color(Color::Muted)
                    .into_any_element()
            }
            Loadable::Loaded(projects) => {
                let label = self
                    .project
                    .and_then(|index| projects.get(index))
                    .map_or_else(
                        || "Pick a project".to_string(),
                        |project| project.title.clone(),
                    );
                let entries = projects
                    .iter()
                    .map(|project| project.title.clone())
                    .collect();
                self.dropdown(
                    "new-thread-project",
                    label,
                    entries,
                    self.project,
                    Self::select_project,
                    window,
                    cx,
                )
                .into_any_element()
            }
        };
        let (agent_control, model_control) = match &self.agents {
            Loadable::Loading => (
                Label::new("Loading agents…")
                    .size(LabelSize::Small)
                    .color(Color::Muted)
                    .into_any_element(),
                div().into_any_element(),
            ),
            Loadable::Failed(error) => (
                Label::new(error.clone())
                    .size(LabelSize::Small)
                    .color(Color::Error)
                    .into_any_element(),
                div().into_any_element(),
            ),
            Loadable::Loaded(agents) if agents.is_empty() => (
                Label::new("No agent is set up in T3. Install and sign in to one in T3 Code.")
                    .size(LabelSize::Small)
                    .color(Color::Muted)
                    .into_any_element(),
                div().into_any_element(),
            ),
            Loadable::Loaded(agents) => {
                let agent = self.agent.and_then(|index| agents.get(index));
                let agent_label =
                    agent.map_or_else(|| "Pick an agent".to_string(), |agent| agent.name.clone());
                let agent_entries = agents.iter().map(|agent| agent.name.clone()).collect();
                let model_entries = agent
                    .map(|agent| {
                        agent
                            .models
                            .iter()
                            .map(|(name, _)| name.clone())
                            .collect::<Vec<_>>()
                    })
                    .unwrap_or_default();
                let model_label = agent
                    .and_then(|agent| agent.models.get(self.model?))
                    .map_or_else(|| "Pick a model".to_string(), |(name, _)| name.clone());
                let has_models = !model_entries.is_empty();
                (
                    self.dropdown(
                        "new-thread-agent",
                        agent_label,
                        agent_entries,
                        self.agent,
                        Self::select_agent,
                        window,
                        cx,
                    )
                    .into_any_element(),
                    if has_models {
                        self.dropdown(
                            "new-thread-model",
                            model_label,
                            model_entries,
                            self.model,
                            Self::select_model,
                            window,
                            cx,
                        )
                        .into_any_element()
                    } else {
                        Label::new("This agent lists no models.")
                            .size(LabelSize::Small)
                            .color(Color::Muted)
                            .into_any_element()
                    },
                )
            }
        };
        let runtime_note = match self.selected_agent().map(runtime_mode_for) {
            Some(RuntimeMode::ApprovalRequired) | None => {
                "The agent asks before it runs commands or edits files."
            }
            Some(_) => "This agent can't ask for approvals, so it runs without asking.",
        };
        let this = cx.entity().downgrade();
        Card::new()
            .gap_3()
            .px(px(14.))
            .py(px(12.))
            .child(
                v_flex()
                    .gap_0p5()
                    .child(Label::new("New agent thread").weight(FontWeight::SEMIBOLD))
                    .child(
                        Label::new("T3 runs the agent on this machine with your own CLI.")
                            .size(LabelSize::Small)
                            .color(Color::Muted),
                    ),
            )
            .child(field("Project", project_control))
            .child(field("Agent", agent_control))
            .child(field("Model", model_control))
            .child(field(
                "Works in",
                h_flex()
                    .gap_2()
                    .child(
                        SegmentedControl::new("new-thread-workspace")
                            .segment("New worktree")
                            .segment("Project checkout")
                            .selected(if self.use_worktree { 0 } else { 1 })
                            .on_select(move |index, _, cx| {
                                this.update(cx, |this, cx| this.set_use_worktree(index == 0, cx))
                                    .ok();
                            }),
                    )
                    .when(self.use_worktree, |this| {
                        this.child(
                            h_flex()
                                .gap_1()
                                .child(
                                    Label::new("from")
                                        .size(LabelSize::Small)
                                        .color(Color::Muted),
                                )
                                .child(
                                    div()
                                        .w(px(160.))
                                        .px_2()
                                        .py_0p5()
                                        .rounded(tokens::CONTROL_RADIUS)
                                        .border(tokens::BORDER_WIDTH)
                                        .border_color(palette.border.hsla())
                                        .bg(palette.background.hsla())
                                        .child(self.base_ref.clone()),
                                ),
                        )
                    }),
            ))
            .child(
                Label::new(runtime_note)
                    .size(LabelSize::XSmall)
                    .color(Color::Muted),
            )
    }
}

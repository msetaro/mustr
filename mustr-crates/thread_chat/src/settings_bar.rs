//! A thread's settings bar, at the start of the composer's bottom row: the model (and its
//! agent), the model's options (effort, fast mode, thinking, context window), the permission
//! mode, and plan mode, as T3's composer footer has them (`ProviderModelPicker.tsx`,
//! `TraitsPicker.tsx`, `ComposerFooterControls.tsx` in T3). On an open thread a pick changes the
//! thread at once (`thread.model-selection.set`, `thread.runtime-mode.set`,
//! `thread.interaction-mode.set`); on a draft it changes what the thread launches with. On a
//! draft in a git project, shift-clicking models picks several, to start a thread with each
//! (`launch.rs`), as T3's model picker does.

use crate::{
    backend::ThreadBackend,
    model::ThreadState,
    new_thread::NewThreadForm,
    thread_settings::{
        self, AgentChoice, ModelOption, ModelOptionKind, RUNTIME_MODES, agent_choices,
        model_selection, parse_options, resolve_options, runtime_mode_description,
        runtime_mode_label, selection_instance_id, selections_from, with_option,
    },
    thread_view::ThreadView,
};
use gpui::{
    Anchor, AnyElement, App, ClickEvent, Context, CursorStyle, DismissEvent, ElementId, Entity,
    IntoElement, MouseButton, ParentElement as _, Render, RenderOnce, SharedString, Styled as _,
    Task, WeakEntity, Window, actions, div, px,
};
use mustr_ui::{palette, tokens, tokens::scaled};
use std::{rc::Rc, sync::Arc};
use t3_api::{
    tagged::OrchestrationV2Command,
    types::{
        ProviderInteractionMode, ProviderOptionSelection, ProviderOptionSelectionValue, RuntimeMode,
    },
};
use ui::{
    Clickable, Color, ContextMenu, Icon, IconName, IconSize, Label, LabelCommon as _, LabelSize,
    PopoverMenu, PopoverMenuHandle, Toggleable, Tooltip, prelude::*,
};
use workspace::Workspace;

actions!(
    thread_chat,
    [
        /// Opens the model picker of the agent thread in focus.
        ToggleModelPicker,
        /// Opens the options of the thread's model: effort, fast mode, thinking, and context
        /// window.
        ToggleModelOptions,
        /// Opens the permission mode picker of the agent thread in focus.
        TogglePermissionMode,
        /// Switches the agent thread in focus between plan mode and building.
        TogglePlanMode,
    ]
);

enum Catalog {
    NotLoaded,
    Loading,
    Loaded(Vec<AgentChoice>),
    Failed,
}

/// The pickers for a thread's agent, model, and modes. It belongs to one [`ThreadView`], which
/// may still be a draft for a new thread.
pub struct ThreadSettingsBar {
    thread_view: WeakEntity<ThreadView>,
    /// The agents, for an open thread. A draft's form has its own.
    catalog: Catalog,
    model_menu: PopoverMenuHandle<ContextMenu>,
    options_menu: PopoverMenuHandle<ContextMenu>,
    access_menu: PopoverMenuHandle<ContextMenu>,
    _tasks: Vec<Task<()>>,
}

/// What the bar acts on.
#[derive(Clone)]
enum Target {
    Draft(Entity<NewThreadForm>),
    Thread {
        thread_id: String,
        backend: Arc<dyn ThreadBackend>,
    },
}

/// What the bar shows: an open thread's settings, or a draft's picks.
struct Settings {
    target: Target,
    agents: Vec<AgentChoice>,
    /// The agents with the models T3's settings show in pickers.
    shown_agents: Vec<AgentChoice>,
    agent: Option<AgentChoice>,
    instance_id: Option<String>,
    model_slug: Option<String>,
    model_name: String,
    /// The model's options, with their values.
    options: Vec<ModelOption>,
    /// The options an open thread's model selection names, which a new model keeps.
    picked: Vec<ProviderOptionSelection>,
    runtime_mode: RuntimeMode,
    runtime_modes: Vec<RuntimeMode>,
    interaction_mode: ProviderInteractionMode,
    /// Why the thread can't switch to another agent now, if it can't.
    agent_locked: Option<&'static str>,
    /// Why the thread can't switch to another model of its agent, if it can't.
    model_locked: Option<&'static str>,
    /// Why changes can't be sent, such as a dry run.
    blocked: Option<String>,
}

impl ThreadSettingsBar {
    pub fn new(
        thread_view: WeakEntity<ThreadView>,
        _window: &mut Window,
        _cx: &mut Context<Self>,
    ) -> Self {
        Self {
            thread_view,
            catalog: Catalog::NotLoaded,
            model_menu: PopoverMenuHandle::default(),
            options_menu: PopoverMenuHandle::default(),
            access_menu: PopoverMenuHandle::default(),
            _tasks: Vec::new(),
        }
    }

    /// Loads the agents for an open thread, once, retrying while T3 can't be reached.
    fn load_catalog(&mut self, backend: Arc<dyn ThreadBackend>, cx: &mut Context<Self>) {
        if !matches!(self.catalog, Catalog::NotLoaded) {
            return;
        }
        self.catalog = Catalog::Loading;
        self._tasks.push(cx.spawn(async move |this, cx| {
            let mut attempt = 0;
            loop {
                let providers = backend.providers().await;
                let loaded = this.update(cx, |this, cx| {
                    let agents = providers.as_deref().map(agent_choices);
                    // Until T3 has checked its agents there may be none; ask again.
                    let loaded = agents.as_ref().is_ok_and(|agents| !agents.is_empty());
                    this.catalog = match agents {
                        Ok(agents) => Catalog::Loaded(agents),
                        Err(error) => {
                            log::warn!("couldn't read T3's agents: {error:#}");
                            Catalog::Failed
                        }
                    };
                    cx.notify();
                    loaded
                });
                if !matches!(loaded, Ok(false)) {
                    return;
                }
                cx.background_executor()
                    .timer(crate::backend::retry_delay(attempt))
                    .await;
                attempt += 1;
            }
        }));
    }

    fn settings(&mut self, cx: &mut Context<Self>) -> Option<Settings> {
        let mut settings = self.unfiltered_settings(cx)?;
        // T3's settings as the server config keeps them current, or else as a draft read them.
        let server_settings = crate::usage::ServerConfigs::try_global(cx)
            .and_then(|configs| configs.read(cx).settings().cloned())
            .or_else(|| match &settings.target {
                Target::Draft(form) => form.read(cx).server_settings().cloned(),
                Target::Thread { .. } => None,
            });
        let keep = settings
            .instance_id
            .as_deref()
            .zip(settings.model_slug.as_deref());
        settings.shown_agents =
            thread_settings::shown_agents(&settings.agents, server_settings.as_ref(), keep);
        Some(settings)
    }

    /// The settings, with every agent's models.
    fn unfiltered_settings(&mut self, cx: &mut Context<Self>) -> Option<Settings> {
        let view = self.thread_view.upgrade()?;
        if let Some(form) = view.read(cx).new_thread.clone() {
            return Some(draft_settings(form, cx));
        }
        let backend = view.read(cx).backend.clone();
        if matches!(self.catalog, Catalog::NotLoaded) {
            self.load_catalog(backend.clone(), cx);
        }
        let agents = match &self.catalog {
            Catalog::Loaded(agents) => agents.clone(),
            _ => Vec::new(),
        };
        let state = view.read(cx).thread_state()?;
        Some(thread_settings_for(state, agents, backend))
    }

    /// Why the thread can't switch agents or models, and why changes can't be sent. For tests.
    #[cfg(test)]
    pub(crate) fn locks(
        &mut self,
        cx: &mut Context<Self>,
    ) -> (Option<&'static str>, Option<&'static str>, Option<String>) {
        self.settings(cx)
            .map(|settings| {
                (
                    settings.agent_locked,
                    settings.model_locked,
                    settings.blocked,
                )
            })
            .unwrap_or_default()
    }

    /// The models the model picker shows, by agent. For tests.
    #[cfg(test)]
    pub(crate) fn shown_models(&mut self, cx: &mut Context<Self>) -> Vec<(String, Vec<String>)> {
        self.settings(cx)
            .map(|settings| {
                settings
                    .shown_agents
                    .iter()
                    .map(|agent| {
                        let models = agent.models.iter().map(|model| model.slug.clone());
                        (agent.instance_id.clone(), models.collect())
                    })
                    .collect()
            })
            .unwrap_or_default()
    }

    /// Whether the model, options, and permission menus are open. For tests.
    #[cfg(test)]
    pub(crate) fn open_menus(&self) -> [bool; 3] {
        [
            self.model_menu.is_deployed(),
            self.options_menu.is_deployed(),
            self.access_menu.is_deployed(),
        ]
    }

    fn act(&mut self, cx: &mut Context<Self>, act: impl FnOnce(&Settings) -> Option<Action>) {
        let Some(settings) = self.settings(cx) else {
            return;
        };
        let Some(action) = act(&settings) else {
            return;
        };
        match (&settings.target, action) {
            (Target::Draft(form), Action::Model { instance_id, slug }) => {
                form.update(cx, |form, cx| form.pick_model(instance_id, slug, cx));
            }
            (Target::Draft(form), Action::Option { id, value }) => {
                form.update(cx, |form, cx| form.pick_option(&id, value, cx));
            }
            (Target::Draft(form), Action::RuntimeMode(mode)) => {
                form.update(cx, |form, cx| form.pick_runtime_mode(mode, cx));
            }
            (Target::Draft(form), Action::InteractionMode(mode)) => {
                form.update(cx, |form, cx| form.set_interaction_mode(mode, cx));
            }
            (Target::Thread { thread_id, backend }, action) => {
                let command = action.command(thread_id, &settings);
                if let Some(command) = command {
                    self.dispatch(backend.clone(), command, cx);
                }
            }
        }
        cx.notify();
    }

    fn dispatch(
        &mut self,
        backend: Arc<dyn ThreadBackend>,
        command: OrchestrationV2Command,
        cx: &mut Context<Self>,
    ) {
        let dispatch = backend.dispatch(command);
        let view = self.thread_view.clone();
        self._tasks.push(cx.spawn(async move |_, cx| {
            if let Err(error) = dispatch.await {
                view.update(cx, |view, cx| {
                    view.error = Some(format!("{error:#}").into());
                    cx.notify();
                })
                .ok();
            }
        }));
    }

    pub(crate) fn pick_model(&mut self, instance_id: String, slug: String, cx: &mut Context<Self>) {
        self.act(cx, |settings| {
            let same = settings.instance_id.as_deref() == Some(instance_id.as_str())
                && settings.model_slug.as_deref() == Some(slug.as_str());
            (!same).then_some(Action::Model { instance_id, slug })
        });
    }

    pub(crate) fn pick_option(
        &mut self,
        id: String,
        value: ProviderOptionSelectionValue,
        cx: &mut Context<Self>,
    ) {
        self.act(cx, |settings| {
            let current = settings
                .options
                .iter()
                .find(|option| option.id == id)
                .and_then(ModelOption::value);
            (current.as_ref() != Some(&value)).then_some(Action::Option { id, value })
        });
    }

    pub(crate) fn pick_runtime_mode(&mut self, mode: RuntimeMode, cx: &mut Context<Self>) {
        self.act(cx, |settings| {
            (settings.runtime_mode != mode).then_some(Action::RuntimeMode(mode))
        });
    }

    pub(crate) fn toggle_plan_mode(&mut self, cx: &mut Context<Self>) {
        self.act(cx, |settings| {
            if !settings.agent.as_ref().is_some_and(|agent| agent.plan_mode) {
                return None;
            }
            Some(Action::InteractionMode(
                if settings.interaction_mode == ProviderInteractionMode::Plan {
                    ProviderInteractionMode::Default
                } else {
                    ProviderInteractionMode::Plan
                },
            ))
        });
    }
}

/// One change picked in the bar.
enum Action {
    Model {
        instance_id: String,
        slug: String,
    },
    Option {
        id: String,
        value: ProviderOptionSelectionValue,
    },
    RuntimeMode(RuntimeMode),
    InteractionMode(ProviderInteractionMode),
}

impl Action {
    /// The command that makes the change to an open thread.
    fn command(self, thread_id: &str, settings: &Settings) -> Option<OrchestrationV2Command> {
        let command_id = crate::commands::new_id();
        Some(match self {
            Self::Model { instance_id, slug } => {
                // Keep the options that were picked and that the new model takes too.
                let explicit = &settings.picked;
                let options = settings
                    .agents
                    .iter()
                    .find(|agent| agent.instance_id == instance_id)
                    .and_then(|agent| agent.model(&slug))
                    .map(|model| {
                        selections_from(&resolve_options(&model.options, explicit))
                            .into_iter()
                            .filter(|selection| {
                                explicit.iter().any(|explicit| explicit.id == selection.id)
                            })
                            .collect::<Vec<_>>()
                    })
                    .unwrap_or_default();
                thread_settings::set_model_selection(
                    thread_id,
                    model_selection(&instance_id, &slug, &options),
                    command_id,
                )
            }
            Self::Option { id, value } => {
                let instance_id = settings.instance_id.as_deref()?;
                let slug = settings.model_slug.as_deref()?;
                let options = with_option(&settings.options, &id, value);
                thread_settings::set_model_selection(
                    thread_id,
                    model_selection(instance_id, slug, &options),
                    command_id,
                )
            }
            Self::RuntimeMode(mode) => {
                thread_settings::set_runtime_mode(thread_id, mode, command_id)
            }
            Self::InteractionMode(mode) => {
                thread_settings::set_interaction_mode(thread_id, mode, command_id)
            }
        })
    }
}

fn draft_settings(form: Entity<NewThreadForm>, cx: &App) -> Settings {
    let draft = form.read(cx);
    let agents = draft.agents().map(<[_]>::to_vec).unwrap_or_default();
    let selected = draft.model();
    let several = draft.several_models().map(|picks| {
        picks
            .iter()
            .map(|pick| {
                agents
                    .iter()
                    .find(|agent| agent.instance_id == pick.instance_id)
                    .and_then(|agent| agent.model(&pick.slug))
                    .map_or_else(|| pick.slug.clone(), |model| model.name.clone())
            })
            .collect::<Vec<_>>()
    });
    let agent = selected.map(|(agent, _)| agent.clone());
    let runtime_modes = agent
        .as_ref()
        .map(AgentChoice::runtime_modes)
        .unwrap_or_else(|| RUNTIME_MODES.to_vec());
    Settings {
        instance_id: selected.map(|(agent, _)| agent.instance_id.clone()),
        model_slug: selected.map(|(_, model)| model.slug.clone()),
        model_name: match &several {
            // T3's label for several: the first two, and how many more.
            Some(names) if names.len() > 2 => {
                format!("{}, {} more", names[..2].join(", "), names.len() - 2)
            }
            Some(names) => names.join(", "),
            None => selected.map_or_else(
                || match draft.agents() {
                    None => "Loading models…".to_string(),
                    Some(_) => "No model".to_string(),
                },
                |(_, model)| model.name.clone(),
            ),
        },
        options: draft.resolved_options(),
        picked: Vec::new(),
        runtime_mode: draft.runtime_mode(),
        runtime_modes,
        interaction_mode: draft.interaction_mode(),
        agent,
        agents,
        shown_agents: Vec::new(),
        agent_locked: None,
        model_locked: None,
        blocked: None,
        target: Target::Draft(form),
    }
}

fn thread_settings_for(
    state: &ThreadState,
    agents: Vec<AgentChoice>,
    backend: Arc<dyn ThreadBackend>,
) -> Settings {
    let thread = &state.thread;
    let selection = &thread.model_selection;
    let instance_id = selection_instance_id(selection)
        .unwrap_or(thread.provider_instance_id.as_str())
        .to_string();
    let slug = thread_settings::selection_model(selection).map(str::to_string);
    let agent = agents
        .iter()
        .find(|agent| agent.instance_id == instance_id)
        .cloned();
    let model = agent
        .as_ref()
        .zip(slug.as_deref())
        .and_then(|(agent, slug)| agent.model(slug));
    let picked = parse_options(selection.options.as_ref());
    let options = model
        .map(|model| resolve_options(&model.options, &picked))
        .unwrap_or_default();
    let started = !state.items().is_empty();
    let running = state.is_running();
    Settings {
        model_name: model
            .map(|model| model.name.clone())
            .or_else(|| slug.clone())
            .unwrap_or_else(|| "Model".to_string()),
        instance_id: Some(instance_id),
        model_slug: slug,
        options,
        picked,
        runtime_mode: thread.runtime_mode.clone(),
        runtime_modes: agent
            .as_ref()
            .map(AgentChoice::runtime_modes)
            .unwrap_or_else(|| RUNTIME_MODES.to_vec()),
        interaction_mode: thread.interaction_mode.clone(),
        agent_locked: running.then_some("Stop the agent's run to switch agents."),
        model_locked: (started && agent.as_ref().is_some_and(|agent| agent.fixed_model))
            .then_some("This agent can't change models once a thread has started."),
        blocked: backend.blocked_reason(crate::Access::Operate),
        agent,
        agents,
        shown_agents: Vec::new(),
        target: Target::Thread {
            thread_id: thread.id.clone(),
            backend,
        },
    }
}

fn agent_icon(agent: Option<&AgentChoice>) -> IconName {
    match agent.map(|agent| agent.driver.as_str()) {
        Some("claudeAgent") => IconName::AiClaude,
        Some("codex") => IconName::AiOpenAi,
        _ => IconName::ZedAgent,
    }
}

fn runtime_mode_icon(mode: &RuntimeMode) -> IconName {
    match mode {
        RuntimeMode::ApprovalRequired => IconName::Lock,
        RuntimeMode::AutoAcceptEdits => IconName::Pencil,
        RuntimeMode::Auto => IconName::Sparkle,
        RuntimeMode::FullAccess | RuntimeMode::Unrecognized(_) => IconName::LockOff,
    }
}

impl Render for ThreadSettingsBar {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let Some(settings) = self.settings(cx) else {
            // With nothing to show, `hidden` keeps the bar out of the row's layout.
            return div().hidden().into_any_element();
        };
        let settings = Rc::new(settings);
        let this = cx.entity().downgrade();
        let disabled = settings.blocked.clone();

        let model = {
            let label = settings.model_name.clone();
            let tooltip = match &settings.agent {
                Some(agent) => format!("{} · {}", agent.name, settings.model_name),
                None => settings.model_name.clone(),
            };
            let control = ComposerControl::new("thread-settings-model", label)
                .icon(agent_icon(settings.agent.as_ref()))
                .chevron(true)
                .tooltip(disabled.clone().unwrap_or(tooltip));
            if disabled.is_some() || settings.agents.is_empty() {
                control.disabled(true).into_any_element()
            } else {
                let settings = settings.clone();
                let this = this.clone();
                PopoverMenu::new("thread-settings-model-menu")
                    .trigger(control)
                    .with_handle(self.model_menu.clone())
                    .anchor(Anchor::BottomLeft)
                    .menu(move |window, cx| Some(model_menu(&settings, this.clone(), window, cx)))
                    .into_any_element()
            }
        };

        let options = thread_settings::options_label(&settings.options).map(|label| {
            let control = ComposerControl::new("thread-settings-options", label.clone())
                .icon(IconName::ToolThink)
                .chevron(true)
                .tooltip(disabled.clone().unwrap_or(label));
            if disabled.is_some() {
                control.disabled(true).into_any_element()
            } else {
                let settings = settings.clone();
                let this = this.clone();
                PopoverMenu::new("thread-settings-options-menu")
                    .trigger(control)
                    .with_handle(self.options_menu.clone())
                    .anchor(Anchor::BottomLeft)
                    .menu(move |window, cx| Some(options_menu(&settings, this.clone(), window, cx)))
                    .into_any_element()
            }
        });

        let access = {
            let mode = settings.runtime_mode.clone();
            let control = ComposerControl::new(
                "thread-settings-access",
                runtime_mode_label(&mode).to_string(),
            )
            .icon(runtime_mode_icon(&mode))
            .chevron(true)
            .tooltip(
                disabled
                    .clone()
                    .unwrap_or_else(|| runtime_mode_description(&mode).to_string()),
            );
            if disabled.is_some() {
                control.disabled(true).into_any_element()
            } else {
                let settings = settings.clone();
                let this = this.clone();
                PopoverMenu::new("thread-settings-access-menu")
                    .trigger(control)
                    .with_handle(self.access_menu.clone())
                    .anchor(Anchor::BottomLeft)
                    .menu(move |window, cx| Some(access_menu(&settings, this.clone(), window, cx)))
                    .into_any_element()
            }
        };

        let plan = settings
            .agent
            .as_ref()
            .is_some_and(|agent| agent.plan_mode)
            .then(|| {
                let planning = settings.interaction_mode == ProviderInteractionMode::Plan;
                let tooltip = disabled.clone().unwrap_or_else(|| {
                    if planning {
                        "Plan mode. Click to return to normal build mode".to_string()
                    } else {
                        "Default mode. Click to enter plan mode".to_string()
                    }
                });
                ComposerControl::new(
                    "thread-settings-plan",
                    if planning { "Plan" } else { "Build" },
                )
                .icon(if planning {
                    IconName::ListTodo
                } else {
                    IconName::ToolHammer
                })
                .active(planning)
                .tooltip(tooltip)
                .disabled(disabled.is_some())
                .on_click(move |_, _, cx| {
                    this.update(cx, |this, cx| this.toggle_plan_mode(cx)).ok();
                })
            });

        h_flex()
            .id("thread-settings-bar")
            .gap_0p5()
            .min_w_0()
            .flex_wrap()
            .child(model)
            .children(options)
            .child(access)
            .children(plan)
            .into_any_element()
    }
}

/// The models of every agent, under the agent's name, as T3's model picker groups them. On a
/// draft that can start several threads, a shift-click adds a model or takes it out, and the
/// menu stays open.
fn model_menu(
    settings: &Settings,
    bar: WeakEntity<ThreadSettingsBar>,
    window: &mut Window,
    cx: &mut App,
) -> Entity<ContextMenu> {
    let agents = settings.shown_agents.clone();
    let current_agent = settings.instance_id.clone();
    let current_model = settings.model_slug.clone();
    let agent_locked = settings.agent_locked;
    let model_locked = settings.model_locked;
    let draft = match &settings.target {
        Target::Draft(form) => Some(form.clone()),
        Target::Thread { .. } => None,
    };
    let is_draft = draft.is_some();
    let build = move |mut menu: ContextMenu, _: &mut Window, cx: &mut Context<ContextMenu>| {
        // Read again on every rebuild, as shift-clicks change them.
        let several = draft.as_ref().and_then(|form| {
            form.read(cx).several_models().map(|picks| {
                picks
                    .iter()
                    .map(|pick| (pick.instance_id.clone(), pick.slug.clone()))
                    .collect::<Vec<_>>()
            })
        });
        let can_start_several = draft
            .as_ref()
            .is_some_and(|form| form.read(cx).can_start_several());
        let (current_agent, current_model) = match &draft {
            Some(form) => form
                .read(cx)
                .model()
                .map(|(agent, model)| (Some(agent.instance_id.clone()), Some(model.slug.clone())))
                .unwrap_or_default(),
            None => (current_agent.clone(), current_model.clone()),
        };
        let this_menu = cx.weak_entity();
        // At the top, as the list of models is long.
        if can_start_several {
            menu = menu
                .label("Shift-click to start a thread with each of several models.")
                .separator();
        }
        for (index, agent) in agents.iter().enumerate() {
            if index > 0 {
                menu = menu.separator();
            }
            let same_agent = current_agent.as_deref() == Some(agent.instance_id.as_str());
            menu = menu.header(agent.name.clone());
            for model in &agent.models {
                let selected = match &several {
                    Some(picks) => picks.iter().any(|(instance_id, slug)| {
                        *instance_id == agent.instance_id && *slug == model.slug
                    }),
                    None => same_agent && current_model.as_deref() == Some(model.slug.as_str()),
                };
                let disabled = !selected
                    && if same_agent {
                        model_locked.is_some()
                    } else {
                        agent_locked.is_some()
                    };
                let bar = bar.clone();
                let draft = draft.clone();
                let this_menu = this_menu.clone();
                let instance_id = agent.instance_id.clone();
                let slug = model.slug.clone();
                menu = menu.toggleable_entry_disabled_when(
                    model.name.clone(),
                    selected,
                    disabled,
                    ui::IconPosition::End,
                    None,
                    move |window, cx| {
                        let (instance_id, slug) = (instance_id.clone(), slug.clone());
                        let Some(form) = &draft else {
                            bar.update(cx, |bar, cx| bar.pick_model(instance_id, slug, cx))
                                .ok();
                            return;
                        };
                        if can_start_several && window.modifiers().shift {
                            form.update(cx, |form, cx| form.toggle_model(instance_id, slug, cx));
                            return;
                        }
                        form.update(cx, |form, cx| form.pick_model(instance_id, slug, cx));
                        // The menu stays open for shift-clicks; a click closes it.
                        let this_menu = this_menu.clone();
                        cx.defer(move |cx| {
                            this_menu.update(cx, |_, cx| cx.emit(DismissEvent)).ok();
                        });
                    },
                );
            }
        }
        for reason in [agent_locked, model_locked].into_iter().flatten() {
            menu = menu.separator().label(reason);
        }
        menu
    };
    if is_draft {
        ContextMenu::build_persistent(window, cx, build)
    } else {
        ContextMenu::build(window, cx, build)
    }
}

/// The model's options: each choice under its label, and each switch as On and Off, as T3's
/// traits menu lists them.
fn options_menu(
    settings: &Settings,
    bar: WeakEntity<ThreadSettingsBar>,
    window: &mut Window,
    cx: &mut App,
) -> Entity<ContextMenu> {
    let options = settings.options.clone();
    ContextMenu::build(window, cx, move |mut menu, _, _| {
        for (index, option) in options.iter().enumerate() {
            if index > 0 {
                menu = menu.separator();
            }
            menu = menu.header(option.label.clone());
            // A switch without a value is off, as T3's traits menu shows it.
            let current = match &option.kind {
                ModelOptionKind::Switch { .. } => Some(
                    option
                        .value()
                        .unwrap_or(ProviderOptionSelectionValue::Boolean(false)),
                ),
                ModelOptionKind::Choice { .. } => option.value(),
            };
            let entries: Vec<(
                SharedString,
                Option<SharedString>,
                ProviderOptionSelectionValue,
            )> = match &option.kind {
                ModelOptionKind::Choice { .. } => option
                    .pickable_choices()
                    .into_iter()
                    .map(|choice| {
                        let label = if choice.is_default {
                            format!("{} (default)", choice.label)
                        } else {
                            choice.label.clone()
                        };
                        (
                            label.into(),
                            choice.description.clone().map(Into::into),
                            ProviderOptionSelectionValue::String(choice.id.clone()),
                        )
                    })
                    .collect(),
                ModelOptionKind::Switch { .. } => vec![
                    (
                        "On".into(),
                        None,
                        ProviderOptionSelectionValue::Boolean(true),
                    ),
                    (
                        "Off".into(),
                        None,
                        ProviderOptionSelectionValue::Boolean(false),
                    ),
                ],
            };
            for (label, description, value) in entries {
                let selected = current.as_ref() == Some(&value);
                let bar = bar.clone();
                let id = option.id.clone();
                menu = menu.custom_entry(
                    move |_, cx| choice_row(None, label.clone(), description.clone(), selected, cx),
                    move |_, cx| {
                        bar.update(cx, |bar, cx| bar.pick_option(id.clone(), value.clone(), cx))
                            .ok();
                    },
                );
            }
        }
        menu
    })
}

/// The permission modes the agent supports, with T3's names and descriptions.
fn access_menu(
    settings: &Settings,
    bar: WeakEntity<ThreadSettingsBar>,
    window: &mut Window,
    cx: &mut App,
) -> Entity<ContextMenu> {
    let modes = settings.runtime_modes.clone();
    let current = settings.runtime_mode.clone();
    ContextMenu::build(window, cx, move |mut menu, _, _| {
        for mode in &modes {
            let selected = *mode == current;
            let label: SharedString = runtime_mode_label(mode).to_string().into();
            let description: SharedString = runtime_mode_description(mode).into();
            let icon = runtime_mode_icon(mode);
            let bar = bar.clone();
            let mode = mode.clone();
            menu = menu.custom_entry(
                move |_, cx| {
                    choice_row(
                        Some(icon),
                        label.clone(),
                        Some(description.clone()),
                        selected,
                        cx,
                    )
                },
                move |_, cx| {
                    bar.update(cx, |bar, cx| bar.pick_runtime_mode(mode.clone(), cx))
                        .ok();
                },
            );
        }
        menu
    })
}

/// A menu row with a label, a muted description under it, and a check when it's the current
/// choice.
fn choice_row(
    icon: Option<IconName>,
    label: SharedString,
    description: Option<SharedString>,
    selected: bool,
    _cx: &mut App,
) -> AnyElement {
    h_flex()
        .w_full()
        .min_w(scaled(px(220.)))
        .gap_2()
        .py_0p5()
        .when_some(icon, |this, icon| {
            this.child(Icon::new(icon).size(IconSize::Small).color(Color::Muted))
        })
        .child(
            v_flex()
                .flex_1()
                .min_w_0()
                .child(Label::new(label).size(LabelSize::Small))
                .when_some(description, |this, description| {
                    this.child(
                        Label::new(description)
                            .size(LabelSize::XSmall)
                            .color(Color::Muted),
                    )
                }),
        )
        .child(div().w(scaled(px(14.))).flex_none().when(selected, |this| {
            this.child(
                Icon::new(IconName::Check)
                    .size(IconSize::Small)
                    .color(Color::Accent),
            )
        }))
        .into_any_element()
}

type ClickHandler = Box<dyn Fn(&ClickEvent, &mut Window, &mut App) + 'static>;

/// A compact control of the composer's bottom row: an icon and a label, with `▾` when it opens
/// a menu. It's quiet until hovered, like the composer's other controls.
#[derive(IntoElement)]
struct ComposerControl {
    id: ElementId,
    label: SharedString,
    icon: Option<IconName>,
    chevron: bool,
    /// Accented, for a mode that's on (plan mode).
    active: bool,
    /// Its menu is open.
    open: bool,
    disabled: bool,
    tooltip: Option<SharedString>,
    on_click: Option<ClickHandler>,
}

impl ComposerControl {
    fn new(id: impl Into<ElementId>, label: impl Into<SharedString>) -> Self {
        Self {
            id: id.into(),
            label: label.into(),
            icon: None,
            chevron: false,
            active: false,
            open: false,
            disabled: false,
            tooltip: None,
            on_click: None,
        }
    }

    fn icon(mut self, icon: IconName) -> Self {
        self.icon = Some(icon);
        self
    }

    fn chevron(mut self, chevron: bool) -> Self {
        self.chevron = chevron;
        self
    }

    fn active(mut self, active: bool) -> Self {
        self.active = active;
        self
    }

    fn disabled(mut self, disabled: bool) -> Self {
        self.disabled = disabled;
        self
    }

    fn tooltip(mut self, tooltip: impl Into<SharedString>) -> Self {
        self.tooltip = Some(tooltip.into());
        self
    }
}

impl Clickable for ComposerControl {
    fn on_click(mut self, handler: impl Fn(&ClickEvent, &mut Window, &mut App) + 'static) -> Self {
        self.on_click = Some(Box::new(handler));
        self
    }

    fn cursor_style(self, _cursor_style: CursorStyle) -> Self {
        self
    }
}

impl Toggleable for ComposerControl {
    fn toggle_state(mut self, selected: bool) -> Self {
        self.open = selected;
        self
    }
}

impl RenderOnce for ComposerControl {
    fn render(self, _window: &mut Window, cx: &mut App) -> impl IntoElement {
        let palette = palette(cx);
        let text = if self.active {
            palette.accent.hsla()
        } else if self.open {
            palette.text.hsla()
        } else {
            palette.text_muted.hsla()
        };
        let background = if self.active {
            palette.accent_soft.hsla()
        } else if self.open {
            palette.surface_2.hsla()
        } else {
            gpui::transparent_black()
        };
        let hover_background = if self.active {
            palette.accent.hsla().opacity(0.22)
        } else {
            palette.surface_2.hsla()
        };
        let hover_text = if self.active {
            palette.accent.hsla()
        } else {
            palette.text.hsla()
        };
        let icon_color = Color::Custom(text);
        let selector = self.id.to_string();
        h_flex()
            .debug_selector(|| selector)
            .id(self.id)
            .flex_none()
            .gap_1()
            .px(scaled(px(6.)))
            .py(scaled(px(2.)))
            .rounded(scaled(tokens::CONTROL_RADIUS))
            .bg(background)
            .text_color(text)
            .text_size(tokens::text_size(tokens::TEXT_SIZE_SMALL))
            .whitespace_nowrap()
            .when(self.disabled, |this| this.opacity(0.5))
            .when_some(self.icon, |this, icon| {
                this.child(Icon::new(icon).size(IconSize::XSmall).color(icon_color))
            })
            .child(self.label)
            .when(self.chevron, |this| {
                this.child(
                    Icon::new(IconName::ChevronDown)
                        .size(IconSize::XSmall)
                        .color(icon_color),
                )
            })
            .when_some(self.tooltip.filter(|_| !self.open), |this, tooltip| {
                this.tooltip(Tooltip::text(tooltip))
            })
            .when(!self.disabled, |this| {
                this.when_some(self.on_click, |this, on_click| {
                    this.cursor_pointer()
                        .hover(move |style| style.bg(hover_background).text_color(hover_text))
                        .on_mouse_down(MouseButton::Left, |_, window, _| window.prevent_default())
                        .on_click(move |event, window, cx| {
                            cx.stop_propagation();
                            on_click(event, window, cx);
                        })
                })
            })
    }
}

/// The settings bar of the thread or draft in focus.
fn active_bar(workspace: &Workspace, cx: &App) -> Option<Entity<ThreadSettingsBar>> {
    let view = workspace.active_item_as::<ThreadView>(cx)?;
    Some(view.read(cx).settings_bar.clone())
}

/// Registers the bar's keyboard actions, which open its menus as T3's `mod+shift+m`,
/// `mod+shift+e` and `mod+shift+a` do.
pub(crate) fn register(workspace: &mut Workspace) {
    workspace.register_action(|workspace, _: &ToggleModelPicker, window, cx| {
        if let Some(bar) = active_bar(workspace, cx) {
            let handle = bar.read(cx).model_menu.clone();
            handle.toggle(window, cx);
        }
    });
    workspace.register_action(|workspace, _: &ToggleModelOptions, window, cx| {
        if let Some(bar) = active_bar(workspace, cx) {
            let handle = bar.read(cx).options_menu.clone();
            handle.toggle(window, cx);
        }
    });
    workspace.register_action(|workspace, _: &TogglePermissionMode, window, cx| {
        if let Some(bar) = active_bar(workspace, cx) {
            let handle = bar.read(cx).access_menu.clone();
            handle.toggle(window, cx);
        }
    });
    workspace.register_action(|workspace, _: &TogglePlanMode, _, cx| {
        if let Some(bar) = active_bar(workspace, cx) {
            bar.update(cx, |bar, cx| bar.toggle_plan_mode(cx));
        }
    });
}

//! How an agent runs a thread: its agent and model, the model's options (effort, fast mode,
//! thinking, context window), its permission mode, and plan mode (`mustr-docs/PARITY.md` §2.3).
//! This is the logic behind the settings bar and the new-thread form, without any UI: the agents
//! T3 offers, the defaults T3's settings give a new thread, and the commands that change an open
//! thread, built the way T3's web app builds them (`packages/client-runtime/src/operations/
//! commands.ts`, `packages/shared/src/model.ts` and `projectSettings.ts` in T3).

use serde_json::{Value, json};
use t3_api::{
    tagged::{OrchestrationV2Command, ProviderOptionDescriptor},
    types::{
        ModelSelection, OrchestrationProjectShell, OrchestrationV2CommandThreadInteractionModeSet,
        OrchestrationV2CommandThreadInteractionModeSetType,
        OrchestrationV2CommandThreadModelSelectionSet,
        OrchestrationV2CommandThreadModelSelectionSetType,
        OrchestrationV2CommandThreadRuntimeModeSet, OrchestrationV2CommandThreadRuntimeModeSetType,
        ProviderInteractionMode, ProviderOptionSelection, ProviderOptionSelectionValue,
        RuntimeMode, ServerProvider, ServerProviderState, ServerSettings, ThreadEnvMode,
        VcsListRefsResult,
    },
};

// Permission modes.

/// The permission modes in the order T3's picker lists them, with T3's names and descriptions
/// (`apps/web/src/components/chat/runtimeModeConfig.ts`).
pub const RUNTIME_MODES: [RuntimeMode; 4] = [
    RuntimeMode::ApprovalRequired,
    RuntimeMode::AutoAcceptEdits,
    RuntimeMode::Auto,
    RuntimeMode::FullAccess,
];

/// T3's name for a permission mode.
pub fn runtime_mode_label(mode: &RuntimeMode) -> &str {
    match mode {
        RuntimeMode::ApprovalRequired => "Supervised",
        RuntimeMode::AutoAcceptEdits => "Auto-accept edits",
        RuntimeMode::Auto => "Auto",
        RuntimeMode::FullAccess => "Full access",
        RuntimeMode::Unrecognized(mode) => mode,
    }
}

/// What a permission mode lets the agent do, in T3's words.
pub fn runtime_mode_description(mode: &RuntimeMode) -> &'static str {
    match mode {
        RuntimeMode::ApprovalRequired => "Ask before commands and file changes.",
        RuntimeMode::AutoAcceptEdits => "Auto-approve edits, ask before other actions.",
        RuntimeMode::Auto => "Supported providers approve routine actions; others still ask.",
        RuntimeMode::FullAccess => "Allow commands and edits without prompts.",
        RuntimeMode::Unrecognized(_) => "A mode this version of mustr doesn't know.",
    }
}

/// The modes an agent supports, in T3's order: all of them when it doesn't say.
pub fn runtime_mode_choices(supported: Option<&[RuntimeMode]>) -> Vec<RuntimeMode> {
    match supported {
        Some(supported) if !supported.is_empty() => RUNTIME_MODES
            .iter()
            .filter(|mode| supported.contains(mode))
            .cloned()
            .collect(),
        _ => RUNTIME_MODES.to_vec(),
    }
}

/// `mode` if the agent supports it, or else the first mode it does, as T3's composer picks.
pub fn compatible_runtime_mode(mode: RuntimeMode, choices: &[RuntimeMode]) -> RuntimeMode {
    if choices.contains(&mode) {
        mode
    } else {
        choices.first().cloned().unwrap_or(mode)
    }
}

// Agents and models.

/// An agent T3 can run, with its models.
#[derive(Clone, Debug, PartialEq)]
pub struct AgentChoice {
    pub instance_id: String,
    /// The kind of agent, such as `claudeAgent` or `codex`.
    pub driver: String,
    pub name: String,
    pub models: Vec<ModelChoice>,
    pub default_model: Option<usize>,
    pub supported_runtime_modes: Option<Vec<RuntimeMode>>,
    /// Whether the agent has a plan mode (T3's `showInteractionModeToggle`).
    pub plan_mode: bool,
    /// Whether a started thread can't change models (T3's `requiresNewThreadForModelChange`).
    pub fixed_model: bool,
}

/// One model of an agent, with the options it takes.
#[derive(Clone, Debug, PartialEq)]
pub struct ModelChoice {
    pub name: String,
    pub slug: String,
    pub options: Vec<ModelOption>,
}

impl AgentChoice {
    pub fn model(&self, slug: &str) -> Option<&ModelChoice> {
        self.models.iter().find(|model| model.slug == slug)
    }

    /// The permission modes the agent supports.
    pub fn runtime_modes(&self) -> Vec<RuntimeMode> {
        runtime_mode_choices(self.supported_runtime_modes.as_deref())
    }
}

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
                .map(|model| ModelChoice {
                    name: model.name.clone(),
                    slug: model.slug.clone(),
                    options: model
                        .capabilities
                        .as_ref()
                        .and_then(|capabilities| capabilities.option_descriptors.as_ref())
                        .map(|descriptors| {
                            descriptors
                                .iter()
                                .filter_map(ModelOption::from_descriptor)
                                .collect()
                        })
                        .unwrap_or_default(),
                })
                .collect::<Vec<_>>();
            AgentChoice {
                instance_id: provider.instance_id.clone(),
                driver: provider.driver.clone(),
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
                plan_mode: provider.show_interaction_mode_toggle != Some(false),
                fixed_model: provider.requires_new_thread_for_model_change == Some(true),
            }
        })
        .collect()
}

/// The instance a model selection names (T3 falls back to the legacy `provider`).
pub fn selection_instance_id(selection: &ModelSelection) -> Option<&str> {
    selection
        .instance_id
        .as_ref()
        .and_then(Value::as_str)
        .or_else(|| selection.provider.as_ref().and_then(Value::as_str))
}

/// The model slug a model selection names.
pub fn selection_model(selection: &ModelSelection) -> Option<&str> {
    selection.model.as_str()
}

/// A model selection in T3's wire shape: `options` only when there are any.
pub fn model_selection(
    instance_id: &str,
    model: &str,
    options: &[ProviderOptionSelection],
) -> ModelSelection {
    ModelSelection {
        instance_id: Some(json!(instance_id)),
        model: json!(model),
        options: (!options.is_empty()).then(|| json!(options)),
        provider: None,
    }
}

// Model options.

/// One option a model takes, from its capabilities in `server.getConfig`: a choice (effort,
/// context window, service tier) or an on/off switch (fast mode, thinking).
#[derive(Clone, Debug, PartialEq)]
pub struct ModelOption {
    pub id: String,
    pub label: String,
    pub kind: ModelOptionKind,
}

#[derive(Clone, Debug, PartialEq)]
pub enum ModelOptionKind {
    Choice {
        choices: Vec<OptionChoice>,
        /// The value, once resolved against a selection; or the model's own current value.
        value: Option<String>,
        /// Values the agent takes from the prompt instead (Claude's `ultrathink`), which the
        /// pickers leave out.
        prompt_values: Vec<String>,
    },
    Switch {
        value: Option<bool>,
    },
}

#[derive(Clone, Debug, PartialEq)]
pub struct OptionChoice {
    pub id: String,
    pub label: String,
    pub description: Option<String>,
    pub is_default: bool,
}

/// The option ids T3 treats as reasoning effort (`REASONING_EFFORT_OPTION_IDS` in T3's
/// `threadExecution.ts`, plus `thinking`).
const REASONING_OPTION_IDS: [&str; 6] = [
    "reasoningEffort",
    "effort",
    "reasoning",
    "variant",
    "reasoning_effort",
    "thinking",
];

impl ModelOption {
    fn from_descriptor(descriptor: &ProviderOptionDescriptor) -> Option<Self> {
        match descriptor {
            ProviderOptionDescriptor::Select(select) => Some(Self {
                id: select.id.clone(),
                label: select.label.clone(),
                kind: ModelOptionKind::Choice {
                    choices: select
                        .options
                        .iter()
                        .map(|choice| OptionChoice {
                            id: choice.id.clone(),
                            label: choice.label.clone(),
                            description: choice.description.clone(),
                            is_default: choice.is_default == Some(true),
                        })
                        .collect(),
                    value: select.current_value.clone(),
                    prompt_values: select.prompt_injected_values.clone().unwrap_or_default(),
                },
            }),
            ProviderOptionDescriptor::Boolean(boolean) => Some(Self {
                id: boolean.id.clone(),
                label: boolean.label.clone(),
                kind: ModelOptionKind::Switch {
                    value: boolean.current_value,
                },
            }),
            ProviderOptionDescriptor::Unrecognized(_) => None,
        }
    }

    /// The option's value: its resolved or current value, or else its default choice.
    pub fn value(&self) -> Option<ProviderOptionSelectionValue> {
        match &self.kind {
            ModelOptionKind::Choice { choices, value, .. } => value
                .clone()
                .or_else(|| {
                    choices
                        .iter()
                        .find(|choice| choice.is_default)
                        .map(|choice| choice.id.clone())
                })
                .map(ProviderOptionSelectionValue::String),
            ModelOptionKind::Switch { value } => value.map(ProviderOptionSelectionValue::Boolean),
        }
    }

    /// The label of the option's value: the choice's label, or On or Off.
    pub fn value_label(&self) -> Option<String> {
        match (&self.kind, self.value()?) {
            (ModelOptionKind::Choice { choices, .. }, ProviderOptionSelectionValue::String(id)) => {
                choices
                    .iter()
                    .find(|choice| choice.id == id)
                    .map(|choice| choice.label.clone())
            }
            (ModelOptionKind::Switch { .. }, ProviderOptionSelectionValue::Boolean(on)) => {
                Some(if on { "On" } else { "Off" }.to_string())
            }
            _ => None,
        }
    }

    /// The choices a picker offers, without the ones the agent takes from the prompt.
    pub fn pickable_choices(&self) -> Vec<&OptionChoice> {
        match &self.kind {
            ModelOptionKind::Choice {
                choices,
                prompt_values,
                ..
            } => choices
                .iter()
                .filter(|choice| !prompt_values.contains(&choice.id))
                .collect(),
            ModelOptionKind::Switch { .. } => Vec::new(),
        }
    }

    pub fn is_reasoning(&self) -> bool {
        matches!(self.kind, ModelOptionKind::Choice { .. })
            && REASONING_OPTION_IDS.contains(&self.id.as_str())
    }

    /// Resolves the option against a selected value, the way T3's `withDescriptorCurrentValue`
    /// does: a value the model doesn't offer falls back to its current or default choice.
    fn resolved(&self, selected: Option<&ProviderOptionSelectionValue>) -> Self {
        let kind = match (&self.kind, selected) {
            (ModelOptionKind::Switch { .. }, Some(ProviderOptionSelectionValue::Boolean(on))) => {
                ModelOptionKind::Switch { value: Some(*on) }
            }
            (ModelOptionKind::Switch { value }, _) => ModelOptionKind::Switch { value: *value },
            (
                ModelOptionKind::Choice {
                    choices,
                    value,
                    prompt_values,
                },
                selected,
            ) => {
                let default = || {
                    value.clone().or_else(|| {
                        choices
                            .iter()
                            .find(|choice| choice.is_default)
                            .map(|choice| choice.id.clone())
                    })
                };
                let raw = match selected {
                    Some(ProviderOptionSelectionValue::String(raw)) => Some(raw.trim()),
                    _ => value.as_deref().map(str::trim),
                }
                .filter(|raw| !raw.is_empty());
                let resolved = match raw {
                    None => default(),
                    Some(raw) if choices.is_empty() => Some(raw.to_string()),
                    Some(raw) if prompt_values.iter().any(|value| value == raw) => choices
                        .iter()
                        .find(|choice| choice.is_default)
                        .map(|choice| choice.id.clone()),
                    Some(raw) if choices.iter().any(|choice| choice.id == raw) => {
                        Some(raw.to_string())
                    }
                    Some(_) => default(),
                };
                ModelOptionKind::Choice {
                    choices: choices.clone(),
                    value: resolved,
                    prompt_values: prompt_values.clone(),
                }
            }
        };
        Self {
            id: self.id.clone(),
            label: self.label.clone(),
            kind,
        }
    }
}

/// A model's options with the values `selections` give them (T3's
/// `getProviderOptionDescriptors`).
pub fn resolve_options(
    options: &[ModelOption],
    selections: &[ProviderOptionSelection],
) -> Vec<ModelOption> {
    options
        .iter()
        .map(|option| {
            let selected = selections
                .iter()
                .find(|selection| selection.id == option.id)
                .map(|selection| &selection.value);
            option.resolved(selected)
        })
        .collect()
}

/// Every option with a value, as T3 sends them once one has been picked
/// (`buildProviderOptionSelectionsFromDescriptors`).
pub fn selections_from(options: &[ModelOption]) -> Vec<ProviderOptionSelection> {
    options
        .iter()
        .filter_map(|option| {
            Some(ProviderOptionSelection {
                id: option.id.clone(),
                value: option.value()?,
            })
        })
        .collect()
}

/// The selections after picking `value` for the option `id`, from the resolved `options`.
pub fn with_option(
    options: &[ModelOption],
    id: &str,
    value: ProviderOptionSelectionValue,
) -> Vec<ProviderOptionSelection> {
    let picked = ProviderOptionSelection {
        id: id.to_string(),
        value,
    };
    let options = options
        .iter()
        .map(|option| {
            if option.id == id {
                option.resolved(Some(&picked.value))
            } else {
                option.clone()
            }
        })
        .collect::<Vec<_>>();
    selections_from(&options)
}

/// The options a new thread launches with: the ones picked, normalized for the model, plus fast
/// mode off when the model has it and it wasn't picked, as T3's composer sends
/// (`buildExplicitProviderOptionSelectionsFromDescriptors`, `withImplicitFastModeDefault`).
pub fn launch_options(
    options: &[ModelOption],
    picked: &[ProviderOptionSelection],
) -> Vec<ProviderOptionSelection> {
    let mut selections = selections_from(&resolve_options(options, picked))
        .into_iter()
        .filter(|selection| picked.iter().any(|picked| picked.id == selection.id))
        .collect::<Vec<_>>();
    let has_fast_mode = options.iter().any(|option| {
        option.id == "fastMode" && matches!(option.kind, ModelOptionKind::Switch { .. })
    });
    if has_fast_mode
        && !selections
            .iter()
            .any(|selection| selection.id == "fastMode")
    {
        selections.push(ProviderOptionSelection {
            id: "fastMode".into(),
            value: ProviderOptionSelectionValue::Boolean(false),
        });
    }
    selections
}

/// The options a model selection carries, in T3's array shape or its legacy object shape.
pub fn parse_options(options: Option<&Value>) -> Vec<ProviderOptionSelection> {
    match options {
        Some(Value::Array(_)) => {
            serde_json::from_value(options.cloned().unwrap_or_default()).unwrap_or_default()
        }
        Some(Value::Object(object)) => object
            .iter()
            .filter_map(|(id, value)| {
                let value = match value {
                    Value::String(text) if !text.trim().is_empty() => {
                        ProviderOptionSelectionValue::String(text.trim().to_string())
                    }
                    Value::Bool(on) => ProviderOptionSelectionValue::Boolean(*on),
                    _ => return None,
                };
                Some(ProviderOptionSelection {
                    id: id.trim().to_string(),
                    value,
                })
            })
            .filter(|selection| !selection.id.is_empty())
            .collect(),
        _ => Vec::new(),
    }
}

/// A short label for a model's options, such as `High · Fast` or `Medium · 1M`, built the way
/// T3's `buildTraitsTriggerDisplay` builds its trigger: fast mode or a fast service tier joins
/// the effort label.
pub fn options_label(options: &[ModelOption]) -> Option<String> {
    let mut labels: Vec<String> = Vec::new();
    let mut speed: Option<String> = None;
    let mut speed_fallback: Option<String> = None;
    let mut reasoning_index = None;
    for option in options {
        if option.id == "fastMode" && matches!(option.kind, ModelOptionKind::Switch { .. }) {
            let fast = option.value() == Some(ProviderOptionSelectionValue::Boolean(true));
            speed = fast.then(|| "Fast".to_string());
            speed_fallback = Some(if fast { "Fast" } else { "Normal" }.to_string());
            continue;
        }
        if option.id == "serviceTier"
            && let ModelOptionKind::Choice { choices, .. } = &option.kind
        {
            let current = option.value();
            let current = choices.iter().find(|choice| {
                current == Some(ProviderOptionSelectionValue::String(choice.id.clone()))
            });
            if let Some(current) = current {
                speed = matches!(current.label.as_str(), "Fast" | "Ultrafast")
                    .then(|| current.label.clone());
                speed_fallback = Some(current.label.clone());
                continue;
            }
        }
        let label = match &option.kind {
            ModelOptionKind::Switch { .. } => option
                .value_label()
                .map(|value| format!("{} {value}", option.label)),
            ModelOptionKind::Choice { .. } => option.value_label(),
        };
        if let Some(label) = label.filter(|label| !label.is_empty()) {
            if reasoning_index.is_none() && option.is_reasoning() {
                reasoning_index = Some(labels.len());
            }
            labels.push(label);
        }
    }
    if labels.is_empty() {
        return speed_fallback;
    }
    if let Some(speed) = speed {
        match reasoning_index {
            Some(index) => labels[index] = format!("{} {speed}", labels[index]),
            None => labels.push(speed),
        }
    }
    Some(labels.join(" · "))
}

// Commands.

/// Changes a thread's permission mode (`thread.runtime-mode.set`).
pub fn set_runtime_mode(
    thread_id: &str,
    runtime_mode: RuntimeMode,
    command_id: String,
) -> OrchestrationV2Command {
    OrchestrationV2Command::ThreadRuntimeModeSet(OrchestrationV2CommandThreadRuntimeModeSet {
        command_id,
        runtime_mode,
        thread_id: thread_id.to_string(),
        type_: OrchestrationV2CommandThreadRuntimeModeSetType::ThreadRuntimeModeSet,
    })
}

/// Switches a thread between plan mode and building (`thread.interaction-mode.set`).
pub fn set_interaction_mode(
    thread_id: &str,
    interaction_mode: ProviderInteractionMode,
    command_id: String,
) -> OrchestrationV2Command {
    OrchestrationV2Command::ThreadInteractionModeSet(
        OrchestrationV2CommandThreadInteractionModeSet {
            command_id,
            interaction_mode,
            thread_id: thread_id.to_string(),
            type_: OrchestrationV2CommandThreadInteractionModeSetType::ThreadInteractionModeSet,
        },
    )
}

/// Changes a thread's model, its options, or its agent (`thread.model-selection.set`). mustr's
/// T3 resolves command context on the server, so like T3's web app on such a server, a change
/// of agent is this command too: T3 plans the switch itself.
pub fn set_model_selection(
    thread_id: &str,
    model_selection: ModelSelection,
    command_id: String,
) -> OrchestrationV2Command {
    OrchestrationV2Command::ThreadModelSelectionSet(OrchestrationV2CommandThreadModelSelectionSet {
        command_id,
        model_selection,
        thread_id: thread_id.to_string(),
        type_: OrchestrationV2CommandThreadModelSelectionSetType::ThreadModelSelectionSet,
    })
}

// Defaults for new threads.

/// How a new thread starts unless the user picks otherwise, from T3's settings.
#[derive(Clone, Debug, PartialEq)]
pub struct ThreadDefaults {
    pub runtime_mode: RuntimeMode,
    pub model_selection: Option<ModelSelection>,
    /// Whether new threads get a worktree.
    pub env_mode: ThreadEnvMode,
    /// Whether a new worktree starts from the base branch's remote.
    pub start_from_origin: bool,
}

/// T3's own default permission mode (`DEFAULT_RUNTIME_MODE`), for when its settings say none.
pub const DEFAULT_RUNTIME_MODE: RuntimeMode = RuntimeMode::FullAccess;

/// The defaults for a new thread in `project`, resolved the way T3's `resolveProjectSettings`
/// does: the project's overrides in T3's settings, then the project's own legacy fields (until
/// T3 folds them into the overrides), then the settings themselves, then T3's defaults. T3 also
/// reads a repository's `t3.json` when neither sets the workspace mode; mustr doesn't yet, so
/// that falls back to the project's checkout, as T3's does without one.
pub fn thread_defaults(
    settings: Option<&ServerSettings>,
    project_id: Option<&str>,
    project: Option<&OrchestrationProjectShell>,
) -> ThreadDefaults {
    let overrides = settings.zip(project_id).and_then(|(settings, project_id)| {
        settings
            .project_settings_overrides
            .as_ref()?
            .get(project_id)
    });
    let legacy = project.filter(|_| {
        !settings
            .and_then(|settings| settings.project_settings_folded)
            .unwrap_or(false)
    });
    ThreadDefaults {
        runtime_mode: overrides
            .and_then(|overrides| overrides.default_runtime_mode.clone())
            .or_else(|| settings.and_then(|settings| settings.default_runtime_mode.clone()))
            .unwrap_or(DEFAULT_RUNTIME_MODE),
        model_selection: overrides
            .and_then(|overrides| overrides.default_model_selection.clone())
            .or_else(|| legacy.and_then(|project| project.default_model_selection.clone()))
            .or_else(|| settings.and_then(|settings| settings.default_model_selection.clone())),
        env_mode: overrides
            .and_then(|overrides| overrides.default_thread_env_mode.clone())
            .or_else(|| legacy.and_then(|project| project.default_thread_env_mode.clone()))
            .or_else(|| settings.and_then(|settings| settings.default_thread_env_mode.clone()))
            .unwrap_or(ThreadEnvMode::Local),
        start_from_origin: overrides
            .and_then(|overrides| overrides.new_worktrees_start_from_origin)
            .or_else(|| settings.and_then(|settings| settings.new_worktrees_start_from_origin))
            .unwrap_or(true),
    }
}

// Where a new thread works.

/// A branch or other ref of a project's repository, from `vcs.listRefs`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RefChoice {
    pub name: String,
    pub current: bool,
    pub is_default: bool,
    pub is_remote: bool,
    /// The worktree it's checked out in, if any.
    pub worktree_path: Option<String>,
}

impl RefChoice {
    /// T3's badge for a ref in its branch picker: `current`, `worktree`, `remote` or `default`.
    pub fn badge(&self, project_root: &str) -> Option<&'static str> {
        if self.current {
            Some("current")
        } else if self
            .worktree_path
            .as_deref()
            .is_some_and(|path| path != project_root)
        {
            Some("worktree")
        } else if self.is_remote {
            Some("remote")
        } else if self.is_default {
            Some("default")
        } else {
            None
        }
    }
}

pub fn ref_choices(result: &VcsListRefsResult) -> Vec<RefChoice> {
    result
        .refs
        .iter()
        .map(|vcs_ref| RefChoice {
            name: vcs_ref.name.clone(),
            current: vcs_ref.current,
            is_default: vcs_ref.is_default,
            is_remote: vcs_ref.is_remote == Some(true),
            worktree_path: vcs_ref.worktree_path.clone(),
        })
        .collect()
}

/// The ref a new worktree starts from unless the user picks another: the repository's default
/// branch, or else the checked-out one.
pub fn default_base_ref(refs: &[RefChoice]) -> Option<&RefChoice> {
    refs.iter()
        .find(|vcs_ref| vcs_ref.is_default && !vcs_ref.is_remote)
        .or_else(|| refs.iter().find(|vcs_ref| vcs_ref.is_default))
        .or_else(|| refs.iter().find(|vcs_ref| vcs_ref.current))
}

/// The project's other worktrees, by the branches checked out in them.
pub fn worktrees<'a>(refs: &'a [RefChoice], project_root: &str) -> Vec<&'a RefChoice> {
    refs.iter()
        .filter(|vcs_ref| {
            !vcs_ref.is_remote
                && vcs_ref
                    .worktree_path
                    .as_deref()
                    .is_some_and(|path| path != project_root)
        })
        .collect()
}

#[cfg(test)]
#[path = "thread_settings_tests.rs"]
mod tests;

//! Tests of thread settings: agents, model options, permission modes, defaults, and commands.

use super::*;
use pretty_assertions::assert_eq;
use t3_api::{tagged::OrchestrationV2Command, types::OrchestrationV2ShellSnapshot};

const NOW: &str = "2026-10-27T12:00:00.000Z";
const THREAD: &str = "thread-1";

// Fixtures: T3's wire JSON.

fn providers_json() -> Value {
    json!([
        {
            "instanceId": "claudeAgent", "driver": "claudeAgent", "displayName": "Claude Code",
            "enabled": true, "installed": true, "status": "ready",
            "auth": { "status": "authenticated" }, "checkedAt": NOW, "version": "2.1.0",
            "supportedRuntimeModes": ["approval-required", "auto-accept-edits", "auto", "full-access"],
            "showInteractionModeToggle": true,
            "models": [
                {
                    "slug": "claude-opus-4-7", "name": "Claude Opus 4.7", "isCustom": false,
                    "isDefault": true,
                    "capabilities": { "optionDescriptors": [
                        {
                            "type": "select", "id": "effort", "label": "Effort",
                            "options": [
                                { "id": "low", "label": "Low" },
                                { "id": "medium", "label": "Medium" },
                                { "id": "high", "label": "High", "isDefault": true },
                                { "id": "max", "label": "Max" },
                                { "id": "ultrathink", "label": "Ultrathink" }
                            ],
                            "promptInjectedValues": ["ultrathink"]
                        },
                        { "type": "boolean", "id": "fastMode", "label": "Fast mode" },
                        {
                            "type": "select", "id": "contextWindow", "label": "Context window",
                            "options": [
                                { "id": "200k", "label": "200k", "isDefault": true },
                                { "id": "1m", "label": "1M", "description": "Bills as long context" }
                            ]
                        }
                    ] }
                },
                {
                    "slug": "claude-sonnet-4-6", "name": "Claude Sonnet 4.6", "isCustom": false,
                    "capabilities": { "optionDescriptors": [
                        {
                            "type": "select", "id": "effort", "label": "Effort",
                            "options": [
                                { "id": "low", "label": "Low" },
                                { "id": "high", "label": "High", "isDefault": true }
                            ]
                        },
                        { "type": "boolean", "id": "thinking", "label": "Thinking", "currentValue": true }
                    ] }
                }
            ]
        },
        {
            "instanceId": "codex", "driver": "codex", "displayName": "Codex",
            "enabled": true, "installed": true, "status": "ready",
            "auth": { "status": "authenticated" }, "checkedAt": NOW, "version": "0.60.0",
            "supportedRuntimeModes": ["approval-required", "full-access"],
            "models": [
                {
                    "slug": "gpt-5.5", "name": "GPT-5.5", "isCustom": false, "isDefault": true,
                    "capabilities": { "optionDescriptors": [
                        {
                            "type": "select", "id": "reasoningEffort", "label": "Reasoning",
                            "currentValue": "medium",
                            "options": [
                                { "id": "low", "label": "Low" },
                                { "id": "medium", "label": "Medium" },
                                { "id": "high", "label": "High" }
                            ]
                        },
                        {
                            "type": "select", "id": "serviceTier", "label": "Service Tier",
                            "currentValue": "default",
                            "options": [
                                { "id": "default", "label": "Standard", "isDefault": true },
                                { "id": "fast", "label": "Fast" }
                            ]
                        }
                    ] }
                }
            ]
        },
        {
            "instanceId": "opencode", "driver": "opencode", "enabled": true, "installed": false,
            "status": "warning", "auth": { "status": "unknown" }, "checkedAt": NOW,
            "version": null, "models": []
        }
    ])
}

fn providers() -> Vec<t3_api::types::ServerProvider> {
    serde_json::from_value(providers_json()).expect("the providers fixture decodes")
}

fn project_json(id: &str, title: &str, root: &str) -> Value {
    json!({
        "id": id, "title": title, "workspaceRoot": root, "scripts": [],
        "createdAt": NOW, "updatedAt": NOW, "defaultModelSelection": null
    })
}

fn shell_json() -> Value {
    json!({
        "schemaVersion": 1,
        "snapshotSequence": 7,
        "threads": [],
        "archivedThreads": [],
        "projects": [
            project_json("project-web", "web", "/src/web"),
            project_json("project-api", "api", "/src/api"),
            project_json("scratch-1", "No project", "/home/me/.t3/scratch"),
        ]
    })
}

fn refs_json() -> Value {
    json!({
        "isRepo": true, "hasPrimaryRemote": true, "nextCursor": null, "totalCount": 4,
        "refs": [
            { "name": "feature/retries", "current": true, "isDefault": false, "worktreePath": "/src/api" },
            { "name": "main", "current": false, "isDefault": true, "worktreePath": null },
            { "name": "t3/fix-flaky-test", "current": false, "isDefault": false,
              "worktreePath": "/src/api-worktrees/fix-flaky-test" },
            { "name": "origin/main", "current": false, "isDefault": true, "isRemote": true,
              "remoteName": "origin", "worktreePath": null }
        ]
    })
}

fn settings_json() -> Value {
    json!({
        "defaultRuntimeMode": "auto-accept-edits",
        "defaultThreadEnvMode": "worktree",
        "newWorktreesStartFromOrigin": false,
        "defaultModelSelection": {
            "instanceId": "claudeAgent", "model": "claude-opus-4-7",
            "options": [{ "id": "effort", "value": "max" }]
        },
        "projectSettingsOverrides": {
            "project-web": { "defaultRuntimeMode": "approval-required", "defaultThreadEnvMode": "local" }
        }
    })
}

// The pure logic.

#[test]
fn permission_modes_use_t3s_names() {
    assert_eq!(
        RUNTIME_MODES
            .iter()
            .map(|mode| (runtime_mode_label(mode), runtime_mode_description(mode)))
            .collect::<Vec<_>>(),
        [
            ("Supervised", "Ask before commands and file changes."),
            (
                "Auto-accept edits",
                "Auto-approve edits, ask before other actions."
            ),
            (
                "Auto",
                "Supported providers approve routine actions; others still ask."
            ),
            ("Full access", "Allow commands and edits without prompts."),
        ]
    );
    let codex = [RuntimeMode::FullAccess, RuntimeMode::ApprovalRequired];
    let choices = runtime_mode_choices(Some(&codex));
    // T3's order, whatever order the agent lists them in.
    assert_eq!(
        choices,
        [RuntimeMode::ApprovalRequired, RuntimeMode::FullAccess]
    );
    assert_eq!(runtime_mode_choices(Some(&[])), RUNTIME_MODES.to_vec());
    assert_eq!(
        compatible_runtime_mode(RuntimeMode::AutoAcceptEdits, &choices),
        RuntimeMode::ApprovalRequired
    );
    assert_eq!(
        compatible_runtime_mode(RuntimeMode::FullAccess, &choices),
        RuntimeMode::FullAccess
    );
}

#[test]
fn agents_come_with_their_models_options_and_modes() {
    let agents = agent_choices(&providers());
    assert_eq!(
        agents
            .iter()
            .map(|agent| (agent.name.as_str(), agent.models.len(), agent.plan_mode))
            .collect::<Vec<_>>(),
        [("Claude Code", 2, true), ("Codex", 1, true)]
    );
    let claude = &agents[0];
    assert_eq!(claude.default_model, Some(0));
    assert_eq!(claude.runtime_modes(), RUNTIME_MODES.to_vec());
    let opus = claude.model("claude-opus-4-7").unwrap();
    assert_eq!(
        opus.options
            .iter()
            .map(|option| option.id.as_str())
            .collect::<Vec<_>>(),
        ["effort", "fastMode", "contextWindow"]
    );
    // Claude's `ultrathink` comes from the prompt, so the pickers leave it out.
    assert_eq!(
        opus.options[0]
            .pickable_choices()
            .iter()
            .map(|choice| choice.id.as_str())
            .collect::<Vec<_>>(),
        ["low", "medium", "high", "max"]
    );
}

fn string(value: &str) -> ProviderOptionSelectionValue {
    ProviderOptionSelectionValue::String(value.into())
}

fn selection(id: &str, value: ProviderOptionSelectionValue) -> ProviderOptionSelection {
    ProviderOptionSelection {
        id: id.into(),
        value,
    }
}

#[test]
fn options_resolve_the_way_t3s_do() {
    let agents = agent_choices(&providers());
    let opus = &agents[0].models[0].options;

    // Unpicked options take their defaults; a switch with no value has none.
    let resolved = resolve_options(opus, &[]);
    assert_eq!(
        resolved.iter().map(ModelOption::value).collect::<Vec<_>>(),
        [Some(string("high")), None, Some(string("200k"))]
    );
    assert_eq!(options_label(&resolved).as_deref(), Some("High · 200k"));

    // A value the model doesn't offer falls back to the default; a prompt-injected one too.
    let resolved = resolve_options(
        opus,
        &[
            selection("effort", string("ultrathink")),
            selection("contextWindow", string("2m")),
            selection("fastMode", ProviderOptionSelectionValue::Boolean(true)),
        ],
    );
    assert_eq!(
        selections_from(&resolved),
        [
            selection("effort", string("high")),
            selection("fastMode", ProviderOptionSelectionValue::Boolean(true)),
            selection("contextWindow", string("200k")),
        ]
    );
    // Fast mode joins the effort label.
    assert_eq!(
        options_label(&resolved).as_deref(),
        Some("High Fast · 200k")
    );

    // Picking one option sends them all, as T3's traits menu does.
    assert_eq!(
        with_option(&resolve_options(opus, &[]), "contextWindow", string("1m")),
        [
            selection("effort", string("high")),
            selection("contextWindow", string("1m")),
        ]
    );

    // Codex: a fast service tier is a speed, and the standard one isn't shown.
    let codex = &agents[1].models[0].options;
    assert_eq!(
        options_label(&resolve_options(codex, &[])).as_deref(),
        Some("Medium")
    );
    assert_eq!(
        options_label(&resolve_options(
            codex,
            &[selection("serviceTier", string("fast"))]
        ))
        .as_deref(),
        Some("Medium Fast")
    );
    // Sonnet's thinking switch is on by default.
    let sonnet = &agents[0].models[1].options;
    assert_eq!(
        options_label(&resolve_options(sonnet, &[])).as_deref(),
        Some("High · Thinking On")
    );
}

#[test]
fn new_threads_launch_with_the_options_picked() {
    let agents = agent_choices(&providers());
    let opus = &agents[0].models[0].options;
    // Nothing picked: only fast mode off, as T3's composer adds.
    assert_eq!(
        launch_options(opus, &[]),
        [selection(
            "fastMode",
            ProviderOptionSelectionValue::Boolean(false)
        )]
    );
    assert_eq!(
        launch_options(opus, &[selection("effort", string("max"))]),
        [
            selection("effort", string("max")),
            selection("fastMode", ProviderOptionSelectionValue::Boolean(false)),
        ]
    );
    // Codex has no fast mode switch.
    assert_eq!(launch_options(&agents[1].models[0].options, &[]), []);
    assert_eq!(
        serde_json::to_value(model_selection("codex", "gpt-5.5", &[])).unwrap(),
        json!({ "instanceId": "codex", "model": "gpt-5.5" })
    );
}

#[test]
fn model_options_parse_in_either_shape() {
    assert_eq!(
        parse_options(Some(&json!([{ "id": "effort", "value": "high" }]))),
        [selection("effort", string("high"))]
    );
    // T3's legacy shape, an object.
    assert_eq!(
        parse_options(Some(
            &json!({ "fastMode": true, "effort": " low ", "x": 3 })
        )),
        [
            selection("fastMode", ProviderOptionSelectionValue::Boolean(true)),
            selection("effort", string("low")),
        ]
    );
    assert_eq!(parse_options(None), []);
}

#[test]
fn settings_commands_are_t3s() {
    let wire = |command: OrchestrationV2Command| serde_json::to_value(command).unwrap();
    assert_eq!(
        wire(set_runtime_mode(
            THREAD,
            RuntimeMode::AutoAcceptEdits,
            "command-1".into()
        )),
        json!({
            "type": "thread.runtime-mode.set", "commandId": "command-1", "threadId": THREAD,
            "runtimeMode": "auto-accept-edits"
        })
    );
    assert_eq!(
        wire(set_interaction_mode(
            THREAD,
            ProviderInteractionMode::Plan,
            "command-2".into()
        )),
        json!({
            "type": "thread.interaction-mode.set", "commandId": "command-2", "threadId": THREAD,
            "interactionMode": "plan"
        })
    );
    assert_eq!(
        wire(set_model_selection(
            THREAD,
            model_selection(
                "claudeAgent",
                "claude-opus-4-7",
                &[selection("effort", string("max"))]
            ),
            "command-3".into()
        )),
        json!({
            "type": "thread.model-selection.set", "commandId": "command-3", "threadId": THREAD,
            "modelSelection": {
                "instanceId": "claudeAgent", "model": "claude-opus-4-7",
                "options": [{ "id": "effort", "value": "max" }]
            }
        })
    );
}

#[test]
fn defaults_come_from_t3s_settings() {
    let settings: ServerSettings = serde_json::from_value(settings_json()).unwrap();
    let shell: OrchestrationV2ShellSnapshot = serde_json::from_value(shell_json()).unwrap();
    let web = &shell.projects[0];
    let api = &shell.projects[1];

    // Nothing to go on: T3's own defaults.
    assert_eq!(
        thread_defaults(None, None, None),
        ThreadDefaults {
            runtime_mode: RuntimeMode::FullAccess,
            model_selection: None,
            env_mode: ThreadEnvMode::Local,
            start_from_origin: true,
        }
    );
    let defaults = thread_defaults(Some(&settings), Some(&api.id), Some(api));
    assert_eq!(defaults.runtime_mode, RuntimeMode::AutoAcceptEdits);
    assert_eq!(defaults.env_mode, ThreadEnvMode::Worktree);
    assert!(!defaults.start_from_origin);
    assert_eq!(
        defaults.model_selection.map(|selection| selection.model),
        Some(json!("claude-opus-4-7"))
    );
    // A project's overrides win.
    let defaults = thread_defaults(Some(&settings), Some(&web.id), Some(web));
    assert_eq!(defaults.runtime_mode, RuntimeMode::ApprovalRequired);
    assert_eq!(defaults.env_mode, ThreadEnvMode::Local);

    // A project's own default model counts until T3 folds it into the overrides.
    let mut legacy = api.clone();
    legacy.default_model_selection = Some(model_selection("codex", "gpt-5.5", &[]));
    let defaults = thread_defaults(Some(&settings), Some(&legacy.id), Some(&legacy));
    assert_eq!(
        defaults.model_selection.map(|selection| selection.model),
        Some(json!("gpt-5.5"))
    );
    let mut folded = settings;
    folded.project_settings_folded = Some(true);
    let defaults = thread_defaults(Some(&folded), Some(&legacy.id), Some(&legacy));
    assert_eq!(
        defaults.model_selection.map(|selection| selection.model),
        Some(json!("claude-opus-4-7"))
    );
}

#[test]
fn refs_give_the_base_branch_and_the_worktrees() {
    let refs = ref_choices(&serde_json::from_value(refs_json()).unwrap());
    assert_eq!(
        default_base_ref(&refs).map(|r| r.name.as_str()),
        Some("main")
    );
    assert_eq!(
        refs.iter()
            .map(|vcs_ref| vcs_ref.badge("/src/api"))
            .collect::<Vec<_>>(),
        [
            Some("current"),
            Some("default"),
            Some("worktree"),
            Some("remote")
        ]
    );
    assert_eq!(
        worktrees(&refs, "/src/api")
            .iter()
            .map(|vcs_ref| vcs_ref.worktree_path.as_deref())
            .collect::<Vec<_>>(),
        [Some("/src/api-worktrees/fix-flaky-test")]
    );
}

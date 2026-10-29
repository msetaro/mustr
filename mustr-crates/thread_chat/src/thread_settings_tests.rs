//! Tests of thread settings and new threads: the pure logic, then the settings bar and the
//! new-thread form against a recording backend that asserts the exact payloads T3 would get.

use super::*;
use crate::{
    SendMessage, ThreadView,
    actions::NewThreadInWorktree,
    backend::{Access, AttachmentUpload, ReadOnlyBackend, ThreadBackend},
    new_thread::form_of,
};
use anyhow::{Result, anyhow};
use collections::HashMap;
use futures::{
    FutureExt as _, StreamExt as _, channel::mpsc, future::BoxFuture, stream::BoxStream,
};
use gpui::{Entity, TestAppContext, VisualContext as _, VisualTestContext, Window};
use parking_lot::Mutex;
use pretty_assertions::assert_eq;
use settings::SettingsStore;
use std::sync::Arc;
use t3_api::{
    tagged::{
        OrchestrationV2Command, OrchestrationV2ShellStreamItem, OrchestrationV2ThreadStreamItem,
    },
    types::{
        OrchestrationV2DispatchCommandResult, OrchestrationV2GetTurnItemInput,
        OrchestrationV2GetTurnItemResult, OrchestrationV2ShellSnapshot,
        OrchestrationV2SubscribeShellInput, OrchestrationV2SubscribeThreadInput,
        OrchestrationV2ThreadDetailSnapshot, OrchestrationV2ThreadLaunchInput,
        OrchestrationV2ThreadLaunchResult,
    },
};

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

fn thread_json(model_selection: Value) -> Value {
    json!({
        "id": THREAD, "projectId": "project-api", "title": "Add retries",
        "createdAt": NOW, "updatedAt": NOW, "createdBy": "user", "creationSource": "web",
        "interactionMode": "default", "runtimeMode": "full-access",
        "modelSelection": model_selection, "providerInstanceId": "claudeAgent",
        "activeProviderThreadId": null, "archivedAt": null, "deletedAt": null,
        "forkedFrom": null, "branch": "t3/retries", "worktreePath": "/src/api-worktrees/retries",
        "lineage": { "rootThreadId": THREAD, "parentThreadId": null, "relationshipToParent": null }
    })
}

fn user_message(id: &str, run_id: &str) -> Value {
    json!({
        "type": "user_message", "id": id, "threadId": THREAD, "runId": run_id, "nodeId": null,
        "providerThreadId": null, "providerTurnId": null, "parentItemId": null,
        "nativeItemRef": null, "ordinal": 1, "status": "completed", "title": null,
        "startedAt": null, "completedAt": null, "updatedAt": NOW,
        "messageId": format!("message-{id}"), "text": "Add retries", "attachments": [],
        "createdBy": "user", "creationSource": "web", "inputIntent": "turn_start"
    })
}

fn run_json(id: &str, status: &str) -> Value {
    json!({
        "id": id, "threadId": THREAD, "ordinal": 1, "status": status, "userMessageId": "m1",
        "modelSelection": { "instanceId": "claudeAgent", "model": "claude-opus-4-7" },
        "providerInstanceId": "claudeAgent", "providerThreadId": null, "activeAttemptId": null,
        "checkpointId": null, "completedAt": null, "contextHandoffId": null,
        "requestedAt": NOW, "rootNodeId": null, "startedAt": null
    })
}

fn projection_json(model_selection: Value, run_status: &str) -> Value {
    let item = user_message("i1", "run-1");
    json!({
        "thread": thread_json(model_selection),
        "attempts": [], "checkpointScopes": [], "checkpoints": [], "contextHandoffs": [],
        "contextTransfers": [], "messages": [], "nodes": [], "plans": [], "providerSessions": [],
        "providerThreads": [], "providerTurns": [], "runs": [run_json("run-1", run_status)],
        "runtimeRequests": [], "subagents": [], "turnItems": [item],
        "visibleTurnItems": [{
            "position": 0, "visibility": "local", "sourceThreadId": THREAD,
            "sourceItemId": "i1", "item": item
        }],
        "updatedAt": NOW
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

// A recording backend.

#[derive(Clone, Debug, PartialEq)]
struct RawCall {
    tag: String,
    payload: Value,
    access: Access,
}

#[derive(Default)]
struct Recorder {
    projection: Mutex<Option<Value>>,
    /// What `call_raw` answers, by tag; anything else fails.
    responses: Mutex<HashMap<String, Value>>,
    /// RPCs the server doesn't have, as a newer or older T3 than mustr's lacks some.
    unsupported: Mutex<Vec<String>>,
    raw_calls: Mutex<Vec<RawCall>>,
    commands: Mutex<Vec<OrchestrationV2Command>>,
    launches: Mutex<Vec<OrchestrationV2ThreadLaunchInput>>,
    events: Mutex<Option<mpsc::UnboundedSender<Result<OrchestrationV2ThreadStreamItem>>>>,
}

impl Recorder {
    fn new() -> Arc<Self> {
        let recorder = Self::default();
        recorder.respond("server.getSettings", settings_json());
        recorder.respond("vcs.listRefs", refs_json());
        recorder.respond(
            "projects.ensureScratch",
            json!({ "projectId": "scratch-1" }),
        );
        Arc::new(recorder)
    }

    fn with_thread(model_selection: Value, run_status: &str) -> Arc<Self> {
        let recorder = Self::new();
        *recorder.projection.lock() = Some(projection_json(model_selection, run_status));
        recorder
    }

    fn respond(&self, tag: &str, response: Value) {
        self.responses.lock().insert(tag.into(), response);
    }

    fn raw_calls(&self, tag: &str) -> Vec<RawCall> {
        self.raw_calls
            .lock()
            .iter()
            .filter(|call| call.tag == tag)
            .cloned()
            .collect()
    }

    /// The commands sent, without their fresh ids.
    fn commands(&self) -> Vec<Value> {
        self.commands
            .lock()
            .iter()
            .map(|command| {
                let mut command = serde_json::to_value(command).unwrap();
                let id = command
                    .as_object_mut()
                    .unwrap()
                    .remove("commandId")
                    .unwrap();
                assert!(uuid::Uuid::parse_str(id.as_str().unwrap()).is_ok());
                command
            })
            .collect()
    }

    /// The launches, without their fresh ids.
    fn launches(&self) -> Vec<Value> {
        self.launches
            .lock()
            .iter()
            .map(|launch| {
                let mut launch = serde_json::to_value(launch).unwrap();
                let object = launch.as_object_mut().unwrap();
                for id in ["commandId", "threadId"] {
                    let id = object.remove(id).unwrap();
                    assert!(uuid::Uuid::parse_str(id.as_str().unwrap()).is_ok());
                }
                object["initialMessage"]
                    .as_object_mut()
                    .unwrap()
                    .remove("messageId");
                launch
            })
            .collect()
    }
}

fn ready<T: Send + 'static>(value: Result<T>) -> BoxFuture<'static, Result<T>> {
    futures::future::ready(value).boxed()
}

impl ThreadBackend for Recorder {
    fn shell_snapshot(&self) -> BoxFuture<'static, Result<OrchestrationV2ShellSnapshot>> {
        ready(serde_json::from_value(shell_json()).map_err(Into::into))
    }

    fn subscribe_shell(
        &self,
        _input: OrchestrationV2SubscribeShellInput,
    ) -> Result<BoxStream<'static, Result<OrchestrationV2ShellStreamItem>>> {
        Ok(futures::stream::pending().boxed())
    }

    fn thread_snapshot(
        &self,
        _thread_id: String,
    ) -> BoxFuture<'static, Result<OrchestrationV2ThreadDetailSnapshot>> {
        let projection = self.projection.lock().clone();
        ready(
            projection
                .ok_or_else(|| anyhow!("no such thread"))
                .and_then(|projection| {
                    serde_json::from_value(
                        json!({ "snapshotSequence": 10, "projection": projection }),
                    )
                    .map_err(Into::into)
                }),
        )
    }

    fn subscribe_thread(
        &self,
        _input: OrchestrationV2SubscribeThreadInput,
    ) -> Result<BoxStream<'static, Result<OrchestrationV2ThreadStreamItem>>> {
        let (sender, receiver) = mpsc::unbounded();
        *self.events.lock() = Some(sender);
        Ok(receiver.boxed())
    }

    fn turn_item(
        &self,
        _input: OrchestrationV2GetTurnItemInput,
    ) -> BoxFuture<'static, Result<OrchestrationV2GetTurnItemResult>> {
        ready(Err(anyhow!("no turn items here")))
    }

    fn providers(&self) -> BoxFuture<'static, Result<Vec<t3_api::types::ServerProvider>>> {
        ready(Ok(providers()))
    }

    fn dispatch(
        &self,
        command: OrchestrationV2Command,
    ) -> BoxFuture<'static, Result<OrchestrationV2DispatchCommandResult>> {
        self.commands.lock().push(command);
        ready(Ok(OrchestrationV2DispatchCommandResult { sequence: 100 }))
    }

    fn launch_thread(
        &self,
        input: OrchestrationV2ThreadLaunchInput,
    ) -> BoxFuture<'static, Result<OrchestrationV2ThreadLaunchResult>> {
        self.launches.lock().push(input);
        ready(
            serde_json::from_value(json!({
                "threadId": THREAD,
                "resumed": false,
                "projection": projection_json(json!({ "instanceId": "claudeAgent", "model": "claude-opus-4-7" }), "preparing")
            }))
            .map_err(Into::into),
        )
    }

    fn call_raw(
        &self,
        tag: &str,
        payload: Value,
        access: Access,
    ) -> BoxFuture<'static, Result<Value>> {
        self.raw_calls.lock().push(RawCall {
            tag: tag.into(),
            payload,
            access,
        });
        if self
            .unsupported
            .lock()
            .iter()
            .any(|unsupported| unsupported == tag)
        {
            let error = crate::backend::UnsupportedRpc { tag: tag.into() };
            return ready(Err(error.into()));
        }
        ready(
            self.responses
                .lock()
                .get(tag)
                .cloned()
                .ok_or_else(|| anyhow!("{tag} isn't answered here")),
        )
    }

    fn subscribe_raw(
        &self,
        tag: &str,
        _payload: Value,
        _access: Access,
    ) -> Result<BoxStream<'static, Result<Value>>> {
        Err(anyhow!("{tag} isn't followed here"))
    }

    fn upload(&self, _upload: AttachmentUpload) -> BoxFuture<'static, Result<()>> {
        ready(Err(anyhow!("no uploads here")))
    }
}

// The views.

fn init_test(cx: &mut TestAppContext) {
    cx.update(|cx| {
        let settings_store = SettingsStore::test(cx);
        cx.set_global(settings_store);
        theme_settings::init(theme::LoadThemes::JustBase, cx);
        cx.bind_keys(crate::key_bindings());
    });
}

fn open_view<'a>(
    backend: Arc<dyn ThreadBackend>,
    thread_id: Option<&str>,
    cx: &'a mut TestAppContext,
) -> (Entity<ThreadView>, &'a mut VisualTestContext) {
    init_test(cx);
    let thread_id = thread_id.map(str::to_string);
    let (view, cx) = cx.add_window_view(|window: &mut Window, cx| {
        ThreadView::new(backend, None, thread_id, "Loading".into(), window, cx)
    });
    cx.simulate_resize(gpui::size(gpui::px(1000.), gpui::px(900.)));
    cx.run_until_parked();
    (view, cx)
}

fn launch(view: &Entity<ThreadView>, text: &str, cx: &mut VisualTestContext) {
    let composer = view.read_with(cx, |view, _| view.composer().clone());
    composer.update_in(cx, |editor, window, cx| editor.set_text(text, window, cx));
    cx.focus(&composer);
    cx.dispatch_action(SendMessage);
    cx.run_until_parked();
    view.read_with(cx, |view, _| assert_eq!(view.error(), None));
}

fn bar(view: &Entity<ThreadView>, cx: &mut VisualTestContext) -> Entity<ThreadSettingsBar> {
    view.read_with(cx, |view, _| view.settings_bar.clone())
}

use crate::settings_bar::ThreadSettingsBar;

#[gpui::test]
async fn drafts_start_with_t3s_defaults(cx: &mut TestAppContext) {
    let backend = Recorder::new();
    let (view, cx) = open_view(backend.clone(), None, cx);

    // The form asked T3 for its settings and for the first project's branches, read-only.
    assert_eq!(
        backend.raw_calls("server.getSettings"),
        [RawCall {
            tag: "server.getSettings".into(),
            payload: json!({}),
            access: Access::Read,
        }]
    );
    assert_eq!(
        backend.raw_calls("vcs.listRefs"),
        [RawCall {
            tag: "vcs.listRefs".into(),
            payload: json!({ "cwd": "/src/api", "limit": 100 }),
            access: Access::Read,
        }]
    );
    for selector in [
        "thread-settings-model",
        "thread-settings-options",
        "thread-settings-access",
        "thread-settings-plan",
    ] {
        assert!(
            cx.debug_bounds(selector).is_some(),
            "{selector} isn't rendered"
        );
    }

    launch(&view, "Add retries to the webhook dispatcher", cx);
    assert_eq!(
        backend.launches(),
        [json!({
            "creationSource": "web",
            // The first project by name; T3's scratch project isn't listed as one.
            "projectId": "project-api",
            "title": "Add retries to the webhook dispatcher",
            "generateTitle": true,
            // T3's default model, with its options, and fast mode off.
            "modelSelection": {
                "instanceId": "claudeAgent", "model": "claude-opus-4-7",
                "options": [
                    { "id": "effort", "value": "max" },
                    { "id": "fastMode", "value": false }
                ]
            },
            // T3's default permission mode, not mustr's own.
            "runtimeMode": "auto-accept-edits",
            "interactionMode": "default",
            // T3's default is a worktree, from the default branch, without fetching first.
            "workspaceStrategy": { "type": "worktree", "baseRef": "main", "startFromOrigin": false },
            "initialMessage": { "text": "Add retries to the webhook dispatcher", "attachments": [] }
        })]
    );
}

#[gpui::test]
async fn without_get_settings_drafts_read_the_settings_from_the_config(cx: &mut TestAppContext) {
    // T3's 0.1.4 nightlies dropped `server.getSettings`; their config carries the settings.
    let backend = Recorder::new();
    backend.unsupported.lock().push("server.getSettings".into());
    backend.respond(
        "server.getConfig",
        json!({
            "environment": {
                "environmentId": "env-1", "label": "workstation", "serverVersion": "0.1.4",
                "platform": { "os": "linux", "arch": "x64" }, "orchestrationProtocolVersion": 2,
                "capabilities": {}
            },
            "auth": {
                "policy": "desktop-managed-local", "bootstrapMethods": ["desktop-bootstrap"],
                "sessionMethods": ["bearer-access-token"], "sessionCookieName": "t3_session"
            },
            "cwd": "/srv/t3", "keybindingsConfigPath": "/srv/t3/keybindings.json",
            "keybindings": [], "issues": [], "providers": [], "availableEditors": [],
            "observability": {
                "logsDirectoryPath": "/srv/t3/logs", "localTracingEnabled": false,
                "otlpTracesEnabled": false, "otlpMetricsEnabled": false
            },
            "settings": settings_json()
        }),
    );
    let (view, cx) = open_view(backend.clone(), None, cx);
    assert_eq!(
        backend.raw_calls("server.getConfig"),
        [RawCall {
            tag: "server.getConfig".into(),
            payload: json!({}),
            access: Access::Read,
        }]
    );
    launch(&view, "Add retries to the webhook dispatcher", cx);
    let launches = backend.launches();
    // T3's default permission mode and worktree, from the config's settings.
    assert_eq!(launches[0]["runtimeMode"], "auto-accept-edits");
    assert_eq!(launches[0]["workspaceStrategy"]["type"], "worktree");
}

#[gpui::test]
async fn drafts_launch_with_what_the_settings_bar_picks(cx: &mut TestAppContext) {
    let backend = Recorder::new();
    let (view, cx) = open_view(backend.clone(), None, cx);
    let bar = bar(&view, cx);
    bar.update(cx, |bar, cx| {
        bar.pick_model("codex".into(), "gpt-5.5".into(), cx);
        bar.pick_option("reasoningEffort".into(), string("high"), cx);
        // Codex can't auto-accept edits, so the draft starts supervised until picked.
        bar.pick_runtime_mode(RuntimeMode::FullAccess, cx);
        bar.toggle_plan_mode(cx);
    });
    // No command goes to T3 for a draft.
    assert!(backend.commands().is_empty());

    let form = cx.update(|_, cx| form_of(&view, cx)).unwrap();
    form.update(cx, |form, cx| {
        form.pick_ref(
            crate::new_thread::RefPickerMode::BaseRef,
            ref_choices(&serde_json::from_value(refs_json()).unwrap())[3].clone(),
            cx,
        );
        form.set_start_from_origin(true, cx);
    });

    launch(&view, "Plan the retry policy", cx);
    let launch = &backend.launches()[0];
    assert_eq!(
        (
            &launch["modelSelection"],
            &launch["runtimeMode"],
            &launch["interactionMode"],
            &launch["workspaceStrategy"],
        ),
        (
            &json!({
                "instanceId": "codex", "model": "gpt-5.5",
                "options": [
                    { "id": "reasoningEffort", "value": "high" },
                    { "id": "serviceTier", "value": "default" }
                ]
            }),
            &json!("full-access"),
            &json!("plan"),
            &json!({ "type": "worktree", "baseRef": "origin/main", "startFromOrigin": true }),
        )
    );
}

#[gpui::test]
async fn the_composers_controls_wrap_rather_than_overlap(cx: &mut TestAppContext) {
    let backend = Recorder::new();
    let (_view, cx) = open_view(backend, None, cx);
    // Too narrow for the controls to fit on one line, as with a big ui_font_size in a small
    // window.
    cx.simulate_resize(gpui::size(gpui::px(360.), gpui::px(900.)));
    cx.run_until_parked();

    let composer = cx.debug_bounds("thread-composer").unwrap();
    let controls = [
        "thread-settings-model",
        "thread-settings-access",
        "thread-settings-plan",
        "thread-attach",
        "thread-send",
    ]
    .map(|selector| {
        let bounds = cx
            .debug_bounds(selector)
            .unwrap_or_else(|| panic!("{selector} isn't shown"));
        (selector, bounds)
    });
    for (index, (selector, bounds)) in controls.iter().enumerate() {
        assert!(
            bounds.left() >= composer.left() && bounds.right() <= composer.right(),
            "{selector} sticks out of the composer"
        );
        for (other_selector, other) in &controls[index + 1..] {
            assert!(
                !bounds.intersects(other),
                "{selector} overlaps {other_selector}"
            );
        }
    }
}

#[gpui::test]
async fn agents_without_a_mode_fall_back_to_one_they_have(cx: &mut TestAppContext) {
    let backend = Recorder::new();
    let (view, cx) = open_view(backend.clone(), None, cx);
    bar(&view, cx).update(cx, |bar, cx| {
        bar.pick_model("codex".into(), "gpt-5.5".into(), cx)
    });
    launch(&view, "Go", cx);
    // T3's default is auto-accept edits, which Codex doesn't have.
    assert_eq!(
        backend.launches()[0]["runtimeMode"],
        json!("approval-required")
    );
}

#[gpui::test]
async fn no_project_opens_t3s_scratch_project(cx: &mut TestAppContext) {
    let backend = Recorder::new();
    let (view, cx) = open_view(backend.clone(), None, cx);
    let form = cx.update(|_, cx| form_of(&view, cx)).unwrap();
    form.update(cx, |form, cx| form.select_scratch(cx));
    cx.run_until_parked();
    assert_eq!(
        backend.raw_calls("projects.ensureScratch"),
        [RawCall {
            tag: "projects.ensureScratch".into(),
            payload: json!({}),
            access: Access::Operate,
        }]
    );
    launch(&view, "What's using port 3000?", cx);
    let launch = &backend.launches()[0];
    // T3 gives each scratch thread a folder of its own from `root`.
    assert_eq!(
        (&launch["projectId"], &launch["workspaceStrategy"]),
        (&json!("scratch-1"), &json!({ "type": "root" }))
    );
}

#[gpui::test]
async fn scratch_fails_on_a_dry_run(cx: &mut TestAppContext) {
    let recorder = Recorder::new();
    let backend = Arc::new(ReadOnlyBackend::new(
        recorder.clone(),
        "MUSTR_DRY_RUN_WRITES is set",
    ));
    let (view, cx) = open_view(backend, None, cx);
    let form = cx.update(|_, cx| form_of(&view, cx)).unwrap();
    form.update(cx, |form, cx| form.select_scratch(cx));
    cx.run_until_parked();
    assert!(recorder.raw_calls("projects.ensureScratch").is_empty());
    let error = form.read_with(cx, |form, cx| form.new_thread("Hi", cx).unwrap_err());
    assert_eq!(
        error,
        "Couldn't start without a project: Not sent: MUSTR_DRY_RUN_WRITES is set."
    );
}

/// Opens a draft filled in for `action`, launches it, and returns what T3 got.
fn launch_prefilled(action: NewThreadInWorktree, cx: &mut TestAppContext) -> Value {
    let backend = Recorder::new();
    let (view, cx) = open_view(backend.clone(), None, cx);
    let form = cx.update(|_, cx| form_of(&view, cx)).unwrap();
    form.update(cx, |form, cx| form.prefill(&action, cx));
    cx.run_until_parked();
    launch(&view, "Go", cx);
    backend.launches().remove(0)
}

#[gpui::test]
async fn drafts_open_in_an_existing_worktree(cx: &mut TestAppContext) {
    let launch = launch_prefilled(
        NewThreadInWorktree {
            project_id: "project-api".into(),
            worktree_path: Some("/src/api-worktrees/fix-flaky-test".into()),
            branch: Some("t3/fix-flaky-test".into()),
        },
        cx,
    );
    assert_eq!(
        (&launch["projectId"], &launch["workspaceStrategy"]),
        (
            &json!("project-api"),
            &json!({
                "type": "existing_worktree",
                "worktreePath": "/src/api-worktrees/fix-flaky-test",
                "branch": "t3/fix-flaky-test"
            })
        )
    );
}

#[gpui::test]
async fn drafts_open_on_a_branch_of_the_checkout(cx: &mut TestAppContext) {
    let launch = launch_prefilled(
        NewThreadInWorktree {
            project_id: "project-web".into(),
            worktree_path: None,
            branch: Some("release/2.4".into()),
        },
        cx,
    );
    assert_eq!(
        (&launch["projectId"], &launch["workspaceStrategy"]),
        (
            &json!("project-web"),
            &json!({ "type": "root", "branch": "release/2.4" })
        )
    );
    // The web project's own defaults apply: supervised.
    assert_eq!(launch["runtimeMode"], json!("approval-required"));
}

#[gpui::test]
async fn the_bar_changes_an_open_thread(cx: &mut TestAppContext) {
    let backend = Recorder::with_thread(
        json!({
            "instanceId": "claudeAgent", "model": "claude-opus-4-7",
            "options": [{ "id": "effort", "value": "max" }]
        }),
        "completed",
    );
    let (view, cx) = open_view(backend.clone(), Some(THREAD), cx);
    for selector in [
        "thread-settings-model",
        "thread-settings-options",
        "thread-settings-access",
        "thread-settings-plan",
    ] {
        assert!(
            cx.debug_bounds(selector).is_some(),
            "{selector} isn't rendered"
        );
    }
    let bar = bar(&view, cx);
    bar.update(cx, |bar, cx| {
        bar.pick_runtime_mode(RuntimeMode::ApprovalRequired, cx);
        // The current mode is no change.
        bar.pick_runtime_mode(RuntimeMode::FullAccess, cx);
        bar.pick_option(
            "fastMode".into(),
            ProviderOptionSelectionValue::Boolean(true),
            cx,
        );
        bar.pick_model("claudeAgent".into(), "claude-sonnet-4-6".into(), cx);
        bar.pick_model("codex".into(), "gpt-5.5".into(), cx);
    });
    let plan = cx.debug_bounds("thread-settings-plan").unwrap();
    cx.simulate_click(plan.center(), gpui::Modifiers::none());
    cx.run_until_parked();

    assert_eq!(
        backend.commands(),
        [
            json!({
                "type": "thread.runtime-mode.set", "threadId": THREAD,
                "runtimeMode": "approval-required"
            }),
            // One option changes; T3 gets them all.
            json!({
                "type": "thread.model-selection.set", "threadId": THREAD,
                "modelSelection": {
                    "instanceId": "claudeAgent", "model": "claude-opus-4-7",
                    "options": [
                        { "id": "effort", "value": "max" },
                        { "id": "fastMode", "value": true },
                        { "id": "contextWindow", "value": "200k" }
                    ]
                }
            }),
            // Another model keeps the options it takes too: Sonnet has no `max`, so its
            // default effort.
            json!({
                "type": "thread.model-selection.set", "threadId": THREAD,
                "modelSelection": {
                    "instanceId": "claudeAgent", "model": "claude-sonnet-4-6",
                    "options": [{ "id": "effort", "value": "high" }]
                }
            }),
            // Another agent: T3 plans the switch from the same command.
            json!({
                "type": "thread.model-selection.set", "threadId": THREAD,
                "modelSelection": { "instanceId": "codex", "model": "gpt-5.5" }
            }),
            json!({
                "type": "thread.interaction-mode.set", "threadId": THREAD,
                "interactionMode": "plan"
            }),
        ]
    );
}

#[gpui::test]
async fn running_threads_keep_their_agent(cx: &mut TestAppContext) {
    let backend = Recorder::with_thread(
        json!({ "instanceId": "claudeAgent", "model": "claude-opus-4-7" }),
        "running",
    );
    let (view, cx) = open_view(backend, Some(THREAD), cx);
    let locks = bar(&view, cx).update(cx, |bar, cx| bar.locks(cx));
    assert_eq!(
        locks,
        (Some("Stop the agent's run to switch agents."), None, None)
    );
}

#[gpui::test]
async fn a_dry_run_disables_the_bar(cx: &mut TestAppContext) {
    let recorder = Recorder::with_thread(
        json!({ "instanceId": "claudeAgent", "model": "claude-opus-4-7" }),
        "completed",
    );
    let backend = Arc::new(ReadOnlyBackend::new(
        recorder.clone(),
        "MUSTR_DRY_RUN_WRITES is set",
    ));
    let (view, cx) = open_view(backend, Some(THREAD), cx);
    // Still shown, with what the thread runs.
    assert!(cx.debug_bounds("thread-settings-model").is_some());
    let plan = cx.debug_bounds("thread-settings-plan").unwrap();
    cx.simulate_click(plan.center(), gpui::Modifiers::none());
    cx.run_until_parked();
    let (_, _, blocked) = bar(&view, cx).update(cx, |bar, cx| bar.locks(cx));
    assert_eq!(blocked.as_deref(), Some("MUSTR_DRY_RUN_WRITES is set"));
    assert!(recorder.commands.lock().is_empty());
}

#[gpui::test]
async fn new_thread_in_worktree_opens_a_filled_in_draft(cx: &mut TestAppContext) {
    init_test(cx);
    let backend = Recorder::new();
    cx.update(|cx| {
        crate::set_backend(backend.clone(), cx);
        crate::init(cx);
    });
    let fs = project::FakeFs::new(cx.executor());
    let project = project::Project::test(fs, [], cx).await;
    let (multi_workspace, cx) = cx.add_window_view(|window, cx| {
        workspace::MultiWorkspace::test_new(project.clone(), window, cx)
    });
    let workspace =
        multi_workspace.read_with(cx, |multi_workspace, _| multi_workspace.workspace().clone());
    cx.simulate_resize(gpui::size(gpui::px(1200.), gpui::px(900.)));

    workspace.update_in(cx, |workspace, window, cx| {
        window.dispatch_action(
            Box::new(NewThreadInWorktree {
                project_id: "project-api".into(),
                worktree_path: Some("/src/api-worktrees/fix-flaky-test".into()),
                branch: Some("t3/fix-flaky-test".into()),
            }),
            cx,
        );
        let _ = workspace;
    });
    cx.run_until_parked();
    let view = workspace
        .read_with(cx, |workspace, cx| {
            workspace.active_item_as::<ThreadView>(cx)
        })
        .expect("a draft opened");
    let new_thread = cx
        .update(|_, cx| form_of(&view, cx))
        .unwrap()
        .read_with(cx, |form, cx| form.new_thread("Fix it", cx))
        .unwrap();
    assert_eq!(new_thread.project_id, "project-api");
    assert_eq!(
        new_thread.workspace,
        crate::commands::Workspace::ExistingWorktree {
            worktree_path: "/src/api-worktrees/fix-flaky-test".into(),
            branch: Some("t3/fix-flaky-test".into()),
        }
    );

    // T3's `mod+shift+a` opens the permission modes; `mod+shift+m` the models.
    let composer = view.read_with(cx, |view, _| view.composer().clone());
    cx.focus(&composer);
    cx.simulate_keystrokes("ctrl-shift-a");
    cx.run_until_parked();
    let bar = bar(&view, cx);
    assert_eq!(
        bar.read_with(cx, |bar, _| bar.open_menus()),
        [false, false, true]
    );
    cx.focus(&composer);
    cx.simulate_keystrokes("ctrl-shift-m");
    cx.run_until_parked();
    assert!(bar.read_with(cx, |bar, _| bar.open_menus()[0]));
}

#[gpui::test]
fn follow_ups_queue_or_steer_while_the_agent_works(cx: &mut TestAppContext) {
    use crate::commands::Delivery;
    use crate::thread_settings::FollowUpBehavior;
    cx.update(|cx| {
        // mustr queues by default, as it always has.
        let behavior = FollowUpBehavior::get(cx);
        assert_eq!(behavior, FollowUpBehavior::Queue);
        assert_eq!(
            (
                behavior.delivery(),
                behavior.other_delivery(),
                behavior.hint()
            ),
            (Delivery::Queue, Delivery::Steer, "queue")
        );
        // Without the app's database (as in tests), the choice still holds for the session.
        FollowUpBehavior::Steer.set(cx);
        let behavior = FollowUpBehavior::get(cx);
        assert_eq!(
            (
                behavior.delivery(),
                behavior.other_delivery(),
                behavior.hint()
            ),
            (Delivery::Steer, Delivery::Queue, "steer")
        );
    });
}

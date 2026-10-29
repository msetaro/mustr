//! Tests of starting threads (WP17): the palette's new threads, starting in the background, and
//! one prompt for several models, against a recording backend that asserts the exact payloads T3
//! would get. Nothing here launches a real thread.

use crate::{
    NewThreadInProject, NewThreadWithoutProject, SendMessage, StartInBackground, ThreadView,
    backend::{Access, AttachmentUpload, ThreadBackend},
    launch::StartedToast,
    shell::ThreadShells,
};
use anyhow::{Result, anyhow};
use collections::{HashMap, HashSet};
use futures::{
    FutureExt as _, StreamExt as _, channel::mpsc, future::BoxFuture, stream::BoxStream,
};
use gpui::{Entity, TestAppContext, VisualContext as _, VisualTestContext};
use parking_lot::Mutex;
use pretty_assertions::assert_eq;
use serde_json::{Value, json};
use settings::SettingsStore;
use std::{sync::Arc, time::Duration};
use t3_api::{
    tagged::{
        OrchestrationV2Command, OrchestrationV2ShellStreamItem, OrchestrationV2ThreadStreamItem,
    },
    types::{
        OrchestrationV2DispatchCommandResult, OrchestrationV2GetTurnItemInput,
        OrchestrationV2GetTurnItemResult, OrchestrationV2ShellSnapshot,
        OrchestrationV2SubscribeShellInput, OrchestrationV2SubscribeThreadInput,
        OrchestrationV2ThreadDetailSnapshot, OrchestrationV2ThreadLaunchInput,
        OrchestrationV2ThreadLaunchResult, ServerProvider,
    },
};
use workspace::{MultiWorkspace, Workspace, notifications::NotificationId};

const NOW: &str = "2026-10-27T12:00:00.000Z";

// Fixtures: T3's wire JSON.

pub(crate) fn providers_json() -> Value {
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
                                { "id": "high", "label": "High", "isDefault": true }
                            ]
                        }
                    ] }
                },
                { "slug": "claude-sonnet-4-6", "name": "Claude Sonnet 4.6", "isCustom": false, "capabilities": null },
                { "slug": "claude-haiku-4-5", "name": "Claude Haiku 4.5", "isCustom": false, "capabilities": null },
                { "slug": "my-fine-tune", "name": "My fine-tune", "isCustom": true, "capabilities": null }
            ]
        },
        {
            "instanceId": "codex", "driver": "codex", "displayName": "Codex",
            "enabled": true, "installed": true, "status": "ready",
            "auth": { "status": "authenticated" }, "checkedAt": NOW, "version": "0.60.0",
            "supportedRuntimeModes": ["approval-required", "full-access"],
            "showInteractionModeToggle": false,
            "models": [
                { "slug": "gpt-5.5", "name": "GPT-5.5", "isCustom": false, "isDefault": true, "capabilities": null },
                { "slug": "gpt-5.5-mini", "name": "GPT-5.5 mini", "isCustom": false, "capabilities": null }
            ]
        }
    ])
}

fn project_json(id: &str, title: &str, root: &str) -> Value {
    json!({
        "id": id, "title": title, "workspaceRoot": root, "scripts": [],
        "createdAt": NOW, "updatedAt": NOW, "defaultModelSelection": null
    })
}

pub(crate) fn shell_thread(id: &str, project_id: &str, title: &str) -> Value {
    json!({
        "id": id, "projectId": project_id, "title": title, "status": "completed",
        "createdAt": NOW, "updatedAt": NOW, "createdBy": "user", "creationSource": "web",
        "interactionMode": "default", "runtimeMode": "full-access",
        "modelSelection": { "instanceId": "claudeAgent", "model": "claude-opus-4-7" },
        "providerInstanceId": "claudeAgent", "activeProviderThreadId": null, "activeRunId": null,
        "archivedAt": null, "branch": "t3/retries", "deletedAt": null, "forkedFrom": null,
        "hasActionableProposedPlan": false, "itemCount": 3, "latestRunId": null,
        "latestUserMessageAt": null, "latestVisibleMessage": null,
        "lineage": { "rootThreadId": id, "parentThreadId": null, "relationshipToParent": null },
        "pendingRuntimeRequest": null, "settledAt": null, "settledOverride": null,
        "visibleItemCount": 3, "worktreePath": null
    })
}

fn shell_json() -> Value {
    json!({
        "schemaVersion": 1,
        "snapshotSequence": 7,
        "threads": [shell_thread("thread-web", "project-web", "Fix the header")],
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
        "isRepo": true, "hasPrimaryRemote": true, "nextCursor": null, "totalCount": 2,
        "refs": [
            { "name": "feature/retries", "current": true, "isDefault": false, "worktreePath": "/src/api" },
            { "name": "main", "current": false, "isDefault": true, "worktreePath": null }
        ]
    })
}

fn settings_json() -> Value {
    json!({
        "defaultRuntimeMode": "auto-accept-edits",
        "defaultThreadEnvMode": "local",
        "newWorktreesStartFromOrigin": false,
        "defaultModelSelection": { "instanceId": "claudeAgent", "model": "claude-opus-4-7" }
    })
}

fn projection_json(thread_id: &str) -> Value {
    let mut thread = shell_thread(thread_id, "project-api", "Add retries");
    thread["worktreePath"] = json!("/src/api-worktrees/retries");
    json!({
        "thread": thread,
        "attempts": [], "checkpointScopes": [], "checkpoints": [], "contextHandoffs": [],
        "contextTransfers": [], "messages": [], "nodes": [], "plans": [], "providerSessions": [],
        "providerThreads": [], "providerTurns": [], "runs": [], "runtimeRequests": [],
        "subagents": [], "turnItems": [], "visibleTurnItems": [], "updatedAt": NOW
    })
}

/// Records every call, and answers the ones the tests need.
#[derive(Default)]
pub(crate) struct Recorder {
    shell: Mutex<Value>,
    /// What `call_raw` answers, by tag; anything else fails.
    responses: Mutex<HashMap<String, Value>>,
    calls: Mutex<Vec<(String, Value, Access)>>,
    commands: Mutex<Vec<OrchestrationV2Command>>,
    launches: Mutex<Vec<OrchestrationV2ThreadLaunchInput>>,
    /// Models whose launches T3 refuses.
    failing_models: Mutex<HashSet<String>>,
    /// What streaming RPCs send, by tag. `$PROJECT` is the project of the latest clone.
    pub(crate) streams: Mutex<HashMap<String, Vec<Value>>>,
    read_only: bool,
}

impl Recorder {
    pub(crate) fn new() -> Arc<Self> {
        Self::with(false)
    }

    fn with(read_only: bool) -> Arc<Self> {
        let recorder = Self {
            shell: Mutex::new(shell_json()),
            read_only,
            ..Default::default()
        };
        recorder.respond("server.getSettings", settings_json());
        recorder.respond("vcs.listRefs", refs_json());
        recorder.respond(
            "projects.ensureScratch",
            json!({ "projectId": "scratch-1" }),
        );
        Arc::new(recorder)
    }

    /// Puts `threads` in the shell, before it's followed.
    pub(crate) fn set_threads(&self, threads: Vec<Value>) {
        self.shell.lock()["threads"] = Value::Array(threads);
    }

    pub(crate) fn respond(&self, tag: &str, response: Value) {
        self.responses.lock().insert(tag.into(), response);
    }

    pub(crate) fn calls(&self, tag: &str) -> Vec<(Value, Access)> {
        self.calls
            .lock()
            .iter()
            .filter(|(called, _, _)| called == tag)
            .map(|(_, payload, access)| (payload.clone(), *access))
            .collect()
    }

    /// The commands sent, without their fresh ids.
    pub(crate) fn commands(&self) -> Vec<Value> {
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
        ready(serde_json::from_value(self.shell.lock().clone()).map_err(Into::into))
    }

    fn subscribe_shell(
        &self,
        _: OrchestrationV2SubscribeShellInput,
    ) -> Result<BoxStream<'static, Result<OrchestrationV2ShellStreamItem>>> {
        Ok(futures::stream::pending().boxed())
    }

    fn thread_snapshot(
        &self,
        thread_id: String,
    ) -> BoxFuture<'static, Result<OrchestrationV2ThreadDetailSnapshot>> {
        ready(
            serde_json::from_value(json!({
                "snapshotSequence": 10, "projection": projection_json(&thread_id)
            }))
            .map_err(Into::into),
        )
    }

    fn subscribe_thread(
        &self,
        _: OrchestrationV2SubscribeThreadInput,
    ) -> Result<BoxStream<'static, Result<OrchestrationV2ThreadStreamItem>>> {
        let (_sender, receiver) = mpsc::unbounded();
        Ok(receiver.boxed())
    }

    fn turn_item(
        &self,
        _: OrchestrationV2GetTurnItemInput,
    ) -> BoxFuture<'static, Result<OrchestrationV2GetTurnItemResult>> {
        ready(Err(anyhow!("no turn items here")))
    }

    fn providers(&self) -> BoxFuture<'static, Result<Vec<ServerProvider>>> {
        ready(serde_json::from_value(providers_json()).map_err(Into::into))
    }

    fn dispatch(
        &self,
        command: OrchestrationV2Command,
    ) -> BoxFuture<'static, Result<OrchestrationV2DispatchCommandResult>> {
        if self.read_only {
            return ready(Err(anyhow!("Not sent: read only.")));
        }
        self.commands.lock().push(command);
        ready(Ok(OrchestrationV2DispatchCommandResult { sequence: 100 }))
    }

    fn launch_thread(
        &self,
        input: OrchestrationV2ThreadLaunchInput,
    ) -> BoxFuture<'static, Result<OrchestrationV2ThreadLaunchResult>> {
        if self.read_only {
            return ready(Err(anyhow!("Not sent: read only.")));
        }
        let model = input
            .model_selection
            .model
            .as_str()
            .unwrap_or_default()
            .to_string();
        let thread_id = input.thread_id.clone().unwrap_or_default();
        self.launches.lock().push(input);
        if self.failing_models.lock().contains(&model) {
            return ready(Err(anyhow!("{model} is signed out")));
        }
        ready(
            serde_json::from_value(json!({
                "threadId": thread_id, "resumed": false, "projection": projection_json(&thread_id)
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
        self.calls.lock().push((tag.to_string(), payload, access));
        if self.read_only && access.is_write() {
            return ready(Err(anyhow!("Not sent: read only.")));
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
        payload: Value,
        access: Access,
    ) -> Result<BoxStream<'static, Result<Value>>> {
        self.calls.lock().push((tag.to_string(), payload, access));
        let items = self
            .streams
            .lock()
            .get(tag)
            .cloned()
            .ok_or_else(|| anyhow!("{tag} isn't followed here"))?;
        let project = self
            .calls("projectClone.start")
            .last()
            .and_then(|(payload, _)| payload["projectId"].as_str().map(str::to_string))
            .unwrap_or_default();
        let items = items
            .into_iter()
            .map(|item| {
                let text = item.to_string().replace("$PROJECT", &project);
                Ok(serde_json::from_str(&text).unwrap())
            })
            .collect::<Vec<_>>();
        Ok(futures::stream::iter(items).boxed())
    }

    fn upload(&self, _: AttachmentUpload) -> BoxFuture<'static, Result<()>> {
        ready(Err(anyhow!("no uploads here")))
    }

    fn write_blocked_reason(&self) -> Option<String> {
        self.read_only.then(|| "read only".to_string())
    }
}

pub(crate) async fn workspace(
    backend: Arc<Recorder>,
    cx: &mut TestAppContext,
) -> (Entity<Workspace>, &mut VisualTestContext) {
    cx.update(|cx| {
        let settings_store = SettingsStore::test(cx);
        cx.set_global(settings_store);
        theme_settings::init(theme::LoadThemes::JustBase, cx);
        editor::init(cx);
        crate::init(cx);
        cx.bind_keys(crate::key_bindings());
        crate::set_backend(backend, cx);
    });
    let fs = project::FakeFs::new(cx.executor());
    let project = project::Project::test(fs, [], cx).await;
    let (multi_workspace, cx) =
        cx.add_window_view(|window, cx| MultiWorkspace::test_new(project.clone(), window, cx));
    let workspace =
        multi_workspace.read_with(cx, |multi_workspace, _| multi_workspace.workspace().clone());
    cx.update(|_, cx| ThreadShells::global(cx));
    cx.run_until_parked();
    (workspace, cx)
}

fn active_view(workspace: &Entity<Workspace>, cx: &mut VisualTestContext) -> Entity<ThreadView> {
    workspace.read_with(cx, |workspace, cx| {
        workspace
            .active_item(cx)
            .and_then(|item| item.downcast::<ThreadView>())
            .expect("a thread view is open")
    })
}

fn new_draft(workspace: &Entity<Workspace>, cx: &mut VisualTestContext) -> Entity<ThreadView> {
    let view = workspace
        .update_in(cx, |workspace, window, cx| {
            ThreadView::open_new(workspace, window, cx)
        })
        .unwrap();
    cx.run_until_parked();
    view
}

/// Types `text` in the view's composer and focuses it.
fn type_message(view: &Entity<ThreadView>, text: &str, cx: &mut VisualTestContext) {
    let composer = view.read_with(cx, |view, _| view.composer().clone());
    composer.update_in(cx, |editor, window, cx| editor.set_text(text, window, cx));
    cx.focus(&composer);
    cx.run_until_parked();
}

fn composer_text(view: &Entity<ThreadView>, cx: &mut VisualTestContext) -> String {
    view.read_with(cx, |view, cx| view.composer().read(cx).text(cx))
}

fn has_started_toast(workspace: &Entity<Workspace>, cx: &mut VisualTestContext) -> bool {
    workspace.read_with(cx, |workspace, _| {
        workspace.has_notification(&NotificationId::unique::<StartedToast>())
    })
}

// The palette.

fn palette(
    query: &str,
    workspace: &Entity<Workspace>,
    cx: &mut VisualTestContext,
) -> Vec<(String, Value, Vec<usize>)> {
    let workspace = workspace.downgrade();
    cx.update(|_, cx| {
        crate::palette_results(query, &workspace, cx)
            .into_iter()
            .map(|result| {
                assert_eq!(result.action.name(), "thread_chat::NewThreadInProject");
                let action = result
                    .action
                    .as_any()
                    .downcast_ref::<NewThreadInProject>()
                    .unwrap();
                (
                    result.label,
                    json!({ "project_id": action.project_id }),
                    result.positions,
                )
            })
            .collect()
    })
}

fn labels(results: &[(String, Value, Vec<usize>)]) -> Vec<&str> {
    results.iter().map(|(label, _, _)| label.as_str()).collect()
}

#[gpui::test]
async fn the_palette_offers_a_new_thread_in_each_project(cx: &mut TestAppContext) {
    let backend = Recorder::new();
    let (workspace, cx) = workspace(backend, cx).await;

    // Every project but T3's scratch one, by name.
    let results = palette("new thread", &workspace, cx);
    assert_eq!(
        labels(&results),
        [
            "thread chat: new thread in api",
            "thread chat: new thread in web"
        ]
    );
    assert_eq!(results[0].1, json!({ "project_id": "project-api" }));
    // The matched characters, to highlight: "new" and "thread" after the namespace.
    assert_eq!(results[0].2, [13, 14, 15, 17, 18, 19, 20, 21, 22]);
    // A project's name alone finds it.
    assert_eq!(
        labels(&palette("WEB", &workspace, cx)),
        ["thread chat: new thread in web"]
    );
    assert_eq!(
        labels(&palette("new th a", &workspace, cx)),
        ["thread chat: new thread in api"]
    );
    // The namespace, or a first keystroke, lists nothing, so Zed's commands stay on top.
    for query in ["thread", "thread chat", "n", "", "new thread in docs"] {
        assert!(palette(query, &workspace, cx).is_empty(), "{query:?} lists");
    }

    // The open thread's project comes first, as T3 lists it.
    workspace.update_in(cx, |workspace, window, cx| {
        ThreadView::open("thread-web".into(), "Fix".into(), workspace, window, cx)
    });
    cx.run_until_parked();
    assert_eq!(
        labels(&palette("new", &workspace, cx)),
        [
            "thread chat: new thread in web",
            "thread chat: new thread in api"
        ]
    );
}

#[gpui::test]
async fn new_threads_from_the_palette_start_where_t3_says(cx: &mut TestAppContext) {
    let backend = Recorder::new();
    let (workspace, cx) = workspace(backend.clone(), cx).await;

    cx.dispatch_action(NewThreadInProject {
        project_id: "project-web".into(),
    });
    cx.run_until_parked();
    let draft = active_view(&workspace, cx);
    type_message(&draft, "Fix the header", cx);
    cx.dispatch_action(SendMessage);
    cx.run_until_parked();

    cx.dispatch_action(NewThreadWithoutProject);
    cx.run_until_parked();
    let draft = active_view(&workspace, cx);
    type_message(&draft, "Sketch a CLI", cx);
    cx.dispatch_action(SendMessage);
    cx.run_until_parked();

    // T3's scratch folder, found or made with `orchestration:operate`.
    assert_eq!(
        backend.calls("projects.ensureScratch"),
        [(json!({}), Access::Operate)]
    );
    let launches = backend.launches();
    assert_eq!(
        launches
            .iter()
            .map(|launch| (&launch["projectId"], &launch["workspaceStrategy"]))
            .collect::<Vec<_>>(),
        [
            // T3's default for new threads is the project's checkout here.
            (&json!("project-web"), &json!({ "type": "root" })),
            (&json!("scratch-1"), &json!({ "type": "root" })),
        ]
    );
}

#[gpui::test]
async fn the_palette_copies_the_thread_id_and_finds_in_it(cx: &mut TestAppContext) {
    let backend = Recorder::new();
    let (workspace, cx) = workspace(backend, cx).await;
    // On a draft there's no thread to copy.
    new_draft(&workspace, cx);
    cx.dispatch_action(crate::CopyThreadId);
    assert_eq!(cx.read_from_clipboard(), None);

    workspace.update_in(cx, |workspace, window, cx| {
        ThreadView::open("thread-web".into(), "Fix".into(), workspace, window, cx)
    });
    cx.run_until_parked();
    cx.dispatch_action(crate::CopyThreadId);
    assert_eq!(
        cx.read_from_clipboard().and_then(|item| item.text()),
        Some("thread-web".to_string())
    );
}

// Starting in the background.

#[gpui::test]
async fn starting_in_the_background_keeps_the_draft(cx: &mut TestAppContext) {
    let backend = Recorder::new();
    let (workspace, cx) = workspace(backend.clone(), cx).await;
    let draft = new_draft(&workspace, cx);
    type_message(&draft, "Add retries to the webhook dispatcher", cx);
    cx.simulate_keystrokes("ctrl-enter");
    cx.run_until_parked();

    assert_eq!(
        backend.launches(),
        [json!({
            "creationSource": "web",
            "projectId": "project-api",
            "title": "Add retries to the webhook dispatcher",
            "generateTitle": true,
            // T3's default model, with its options' defaults left to T3.
            "modelSelection": { "instanceId": "claudeAgent", "model": "claude-opus-4-7" },
            "runtimeMode": "auto-accept-edits",
            "interactionMode": "default",
            "workspaceStrategy": { "type": "root" },
            "initialMessage": { "text": "Add retries to the webhook dispatcher", "attachments": [] }
        })]
    );
    // The draft is still open, empty, for the next thread; a toast offers the started one.
    assert_eq!(active_view(&workspace, cx), draft);
    assert!(draft.read_with(cx, |view, _| view.new_thread.is_some()
        && view.error().is_none()));
    assert_eq!(composer_text(&draft, cx), "");
    assert!(has_started_toast(&workspace, cx));

    // The same keys on a draft without a message send nothing.
    cx.simulate_keystrokes("ctrl-enter");
    cx.run_until_parked();
    assert_eq!(backend.launches().len(), 1);

    // On a thread that has started, they send the message, as before.
    workspace.update_in(cx, |workspace, window, cx| {
        ThreadView::open("thread-web".into(), "Fix".into(), workspace, window, cx)
    });
    cx.run_until_parked();
    let thread = active_view(&workspace, cx);
    type_message(&thread, "Also cap the delay", cx);
    cx.simulate_keystrokes("ctrl-enter");
    cx.run_until_parked();
    let commands = backend.commands();
    assert_eq!(commands.len(), 1);
    assert_eq!(commands[0]["type"], "message.dispatch");
    assert_eq!(commands[0]["text"], "Also cap the delay");
    assert_eq!(backend.launches().len(), 1);
}

#[gpui::test]
async fn a_refused_background_start_keeps_the_prompt(cx: &mut TestAppContext) {
    let backend = Recorder::with(true);
    let (workspace, cx) = workspace(backend.clone(), cx).await;
    let draft = new_draft(&workspace, cx);
    type_message(&draft, "Add retries", cx);
    cx.dispatch_action(StartInBackground);
    cx.run_until_parked();
    assert_eq!(composer_text(&draft, cx), "Add retries");
    assert_eq!(
        draft.read_with(cx, |view, _| view.error().cloned()),
        Some("Not sent: read only.".into())
    );
    assert!(!has_started_toast(&workspace, cx));
    // The next Enter is an ordinary send again, not a background one.
    let form = draft
        .read_with(cx, |view, _| view.new_thread.clone())
        .unwrap();
    assert!(!form.read_with(cx, |form, _| form.background));
}

// One prompt for several models.

#[gpui::test]
async fn several_models_start_a_thread_each_in_a_worktree(cx: &mut TestAppContext) {
    let backend = Recorder::new();
    let (workspace, cx) = workspace(backend.clone(), cx).await;
    let draft = new_draft(&workspace, cx);
    let form = draft
        .read_with(cx, |view, _| view.new_thread.clone())
        .unwrap();
    form.update(cx, |form, cx| {
        form.pick_option(
            "effort",
            t3_api::types::ProviderOptionSelectionValue::String("low".into()),
            cx,
        );
        // Shift-clicks in the model picker.
        form.toggle_model("codex".into(), "gpt-5.5".into(), cx);
        form.toggle_model("claudeAgent".into(), "claude-sonnet-4-6".into(), cx);
        form.toggle_model("claudeAgent".into(), "claude-sonnet-4-6".into(), cx);
        form.toggle_model("codex".into(), "gpt-5.5-mini".into(), cx);
    });
    let picked = |cx: &mut VisualTestContext| {
        form.read_with(cx, |form, _| {
            form.several_models().map(|picks| {
                picks
                    .iter()
                    .map(|pick| pick.slug.clone())
                    .collect::<Vec<_>>()
            })
        })
    };
    assert_eq!(
        picked(cx),
        Some(vec![
            "claude-opus-4-7".to_string(),
            "gpt-5.5".to_string(),
            "gpt-5.5-mini".to_string()
        ])
    );
    type_message(&draft, "Make the build faster", cx);
    cx.dispatch_action(SendMessage);
    cx.run_until_parked();

    let launches = backend.launches();
    assert_eq!(
        launches
            .iter()
            .map(|launch| (
                &launch["modelSelection"],
                &launch["runtimeMode"],
                &launch["workspaceStrategy"],
                &launch["initialMessage"]["text"],
            ))
            .collect::<Vec<_>>(),
        [
            (
                // The model picked first keeps its options.
                &json!({
                    "instanceId": "claudeAgent", "model": "claude-opus-4-7",
                    "options": [{ "id": "effort", "value": "low" }]
                }),
                &json!("auto-accept-edits"),
                // Each in a worktree of its own from the default branch, though the project's
                // threads work in its checkout by default.
                &json!({ "type": "worktree", "baseRef": "main", "startFromOrigin": false }),
                &json!("Make the build faster"),
            ),
            (
                &json!({ "instanceId": "codex", "model": "gpt-5.5" }),
                // Codex can't auto-accept edits.
                &json!("approval-required"),
                &json!({ "type": "worktree", "baseRef": "main", "startFromOrigin": false }),
                &json!("Make the build faster"),
            ),
            (
                &json!({ "instanceId": "codex", "model": "gpt-5.5-mini" }),
                &json!("approval-required"),
                &json!({ "type": "worktree", "baseRef": "main", "startFromOrigin": false }),
                &json!("Make the build faster"),
            ),
        ]
    );
    // The draft stays, with the models picked, for the next prompt.
    assert_eq!(active_view(&workspace, cx), draft);
    assert_eq!(composer_text(&draft, cx), "");
    assert_eq!(picked(cx).map(|picks| picks.len()), Some(3));
    assert!(has_started_toast(&workspace, cx));

    // Down to one model, the draft starts one thread again, with that model.
    form.update(cx, |form, cx| {
        form.toggle_model("claudeAgent".into(), "claude-opus-4-7".into(), cx);
        form.toggle_model("codex".into(), "gpt-5.5-mini".into(), cx);
    });
    assert_eq!(picked(cx), None);
    assert_eq!(
        form.read_with(cx, |form, _| form
            .model()
            .map(|(_, model)| model.slug.clone())),
        Some("gpt-5.5".to_string())
    );
}

#[gpui::test]
async fn a_model_that_fails_stays_picked_with_the_prompt(cx: &mut TestAppContext) {
    let backend = Recorder::new();
    backend.failing_models.lock().insert("gpt-5.5".into());
    let (workspace, cx) = workspace(backend.clone(), cx).await;
    let draft = new_draft(&workspace, cx);
    let form = draft
        .read_with(cx, |view, _| view.new_thread.clone())
        .unwrap();
    form.update(cx, |form, cx| {
        form.toggle_model("codex".into(), "gpt-5.5".into(), cx)
    });
    type_message(&draft, "Make the build faster", cx);
    cx.dispatch_action(SendMessage);
    cx.run_until_parked();

    assert_eq!(backend.launches().len(), 2);
    assert_eq!(
        draft.read_with(cx, |view, _| view.error().cloned()),
        Some("Couldn't start GPT-5.5: gpt-5.5 is signed out".into())
    );
    // Only the one that failed is left picked, and the prompt stays to try again.
    assert_eq!(composer_text(&draft, cx), "Make the build faster");
    assert_eq!(
        form.read_with(cx, |form, _| (
            form.several_models().is_none(),
            form.model().map(|(_, model)| model.slug.clone())
        )),
        (true, Some("gpt-5.5".to_string()))
    );
    // The one that started is offered in a toast.
    assert!(has_started_toast(&workspace, cx));
}

#[gpui::test]
async fn several_models_need_a_git_project(cx: &mut TestAppContext) {
    let backend = Recorder::new();
    let (workspace, cx) = workspace(backend.clone(), cx).await;
    let draft = new_draft(&workspace, cx);
    let form = draft
        .read_with(cx, |view, _| view.new_thread.clone())
        .unwrap();
    form.update(cx, |form, cx| form.select_scratch(cx));
    cx.run_until_parked();
    // Without a repository, a shift-click picks the model alone.
    form.update(cx, |form, cx| {
        assert!(!form.can_start_several());
        form.toggle_model("codex".into(), "gpt-5.5".into(), cx);
    });
    assert!(form.read_with(cx, |form, _| form.several_models().is_none()));
    type_message(&draft, "Sketch a CLI", cx);
    cx.dispatch_action(SendMessage);
    cx.run_until_parked();
    let launches = backend.launches();
    assert_eq!(launches.len(), 1);
    assert_eq!(launches[0]["modelSelection"]["model"], "gpt-5.5");
}

// Model visibility.

#[gpui::test]
async fn pickers_hide_the_models_t3_hides(cx: &mut TestAppContext) {
    let backend = Recorder::new();
    let mut settings = settings_json();
    settings["providerModelPreferences"] = json!({
        "claudeAgent": {
            "hiddenModels": ["claude-haiku-4-5", "claude-opus-4-7", "my-fine-tune"],
            "modelOrder": ["claude-sonnet-4-6"]
        },
        "codex": { "hiddenModels": [], "modelOrder": ["gpt-5.5-mini", "gpt-5.5"] }
    });
    backend.respond("server.getSettings", settings);
    let (workspace, cx) = workspace(backend.clone(), cx).await;
    let draft = new_draft(&workspace, cx);
    let bar = draft.read_with(cx, |view, _| view.settings_bar.clone());
    assert_eq!(
        bar.update(cx, |bar, cx| bar.shown_models(cx)),
        [
            (
                "claudeAgent".to_string(),
                vec![
                    // In T3's order: first the ones it orders.
                    "claude-sonnet-4-6".to_string(),
                    // Hidden, but T3's default and picked now, so it shows checked.
                    "claude-opus-4-7".to_string(),
                    // Custom models never hide.
                    "my-fine-tune".to_string(),
                ]
            ),
            (
                "codex".to_string(),
                vec!["gpt-5.5-mini".to_string(), "gpt-5.5".to_string()]
            ),
        ]
    );
    // Once another model is picked, the hidden one goes.
    let form = draft
        .read_with(cx, |view, _| view.new_thread.clone())
        .unwrap();
    form.update(cx, |form, cx| {
        form.pick_model("codex".into(), "gpt-5.5".into(), cx)
    });
    assert_eq!(
        bar.update(cx, |bar, cx| bar.shown_models(cx))[0].1,
        ["claude-sonnet-4-6", "my-fine-tune"]
    );
}

// Cloning a repository.

fn clone_snapshot(phase: &str, stage: &str, percent: Option<i64>, error: Option<&str>) -> Value {
    json!([{
        "projectId": "$PROJECT", "remoteUrl": "https://example.com/acme/widgets.git",
        "destinationPath": "/home/me/widgets", "repository": null, "phase": phase,
        "stage": stage, "percent": percent, "detail": "1.2 MiB/s", "error": error,
        "startedAt": NOW, "endedAt": null, "sequence": 1
    }])
}

fn clone_modal(
    workspace: &Entity<Workspace>,
    cx: &mut VisualTestContext,
) -> Option<Entity<crate::clone::CloneRepositoryModal>> {
    workspace.read_with(cx, |workspace, cx| {
        workspace.active_modal::<crate::clone::CloneRepositoryModal>(cx)
    })
}

#[test]
fn clones_are_named_and_sent_as_t3_does() {
    use crate::clone::{default_destination, directory_name, is_shorthand, start_input};
    for (repository, name) in [
        ("https://github.com/acme/widgets.git", "widgets"),
        ("git@github.com:acme/widgets.git", "widgets"),
        ("https://gitlab.example.com/group/sub/tool?ref=main", "tool"),
        ("acme/widgets", "widgets"),
        ("/srv/git/local-repo", "local-repo"),
        ("https://github.com", ""),
    ] {
        assert_eq!(directory_name(repository), name, "{repository}");
    }
    assert!(is_shorthand("acme/widgets"));
    assert!(!is_shorthand("https://github.com/acme/widgets"));
    assert!(!is_shorthand("/srv/git/widgets"));
    assert_eq!(default_destination(None, "acme/widgets"), "~/widgets");
    assert_eq!(
        default_destination(Some("/work/src"), "acme/widgets"),
        "/work/src/widgets"
    );

    // A URL goes as T3's `remoteUrl`; GitHub's `owner/name` for T3 to look up.
    let wire = |input| serde_json::to_value(input).unwrap();
    assert_eq!(
        wire(start_input(
            "project-1".into(),
            " https://example.com/acme/widgets.git ",
            "~/src/widgets/",
            NOW.into(),
        )),
        json!({
            "projectId": "project-1", "title": "widgets", "createdAt": NOW,
            "remoteUrl": "https://example.com/acme/widgets.git", "destinationPath": "~/src/widgets"
        })
    );
    assert_eq!(
        wire(start_input(
            "project-1".into(),
            "acme/widgets",
            "~/widgets",
            NOW.into()
        )),
        json!({
            "projectId": "project-1", "title": "widgets", "createdAt": NOW,
            "provider": "github", "repository": "acme/widgets", "destinationPath": "~/widgets"
        })
    );
}

#[gpui::test]
async fn cloning_adds_the_project_and_opens_a_thread_in_it(cx: &mut TestAppContext) {
    let backend = Recorder::new();
    backend.respond(
        "projectClone.start",
        json!({
            "projectId": "ignored", "cwd": "/home/me/widgets",
            "remoteUrl": "https://example.com/acme/widgets.git", "repository": null
        }),
    );
    backend.streams.lock().insert(
        "subscribeProjectClones".into(),
        vec![
            clone_snapshot("running", "receiving", Some(45), None),
            clone_snapshot("done", "checkout", Some(100), None),
        ],
    );
    let (workspace, cx) = workspace(backend.clone(), cx).await;
    cx.dispatch_action(crate::CloneRepository);
    cx.run_until_parked();
    let modal = clone_modal(&workspace, cx).expect("the clone modal is open");
    let (repository, destination) = modal.read_with(cx, |modal, _| modal.editors());
    repository.update_in(cx, |editor, window, cx| {
        editor.set_text("https://example.com/acme/widgets.git", window, cx)
    });
    // The folder follows the repository, in `~/` without T3's base folder.
    assert_eq!(
        destination.read_with(cx, |editor, cx| editor.text(cx)),
        "~/widgets"
    );
    modal.update_in(cx, |modal, window, cx| modal.clone_repository(window, cx));
    cx.run_until_parked();

    let calls = backend.calls("projectClone.start");
    assert_eq!(calls.len(), 1);
    let (mut payload, access) = calls[0].clone();
    assert_eq!(access, Access::SourceControlWrite);
    for id in ["projectId", "createdAt"] {
        assert!(payload[id].take().is_string());
    }
    assert_eq!(
        payload,
        json!({
            "projectId": null, "createdAt": null, "title": "widgets",
            "remoteUrl": "https://example.com/acme/widgets.git", "destinationPath": "~/widgets"
        })
    );
    assert_eq!(
        backend.calls("subscribeProjectClones"),
        [(json!({}), Access::Read)]
    );
    // Once it's done, the modal closes and a new thread opens in the project.
    cx.executor().advance_clock(Duration::from_secs(4));
    cx.run_until_parked();
    assert!(clone_modal(&workspace, cx).is_none());
    assert!(active_view(&workspace, cx).read_with(cx, |view, _| view.new_thread.is_some()));
}

#[gpui::test]
async fn a_failed_clone_says_why_and_retries(cx: &mut TestAppContext) {
    let backend = Recorder::new();
    backend.respond(
        "projectClone.start",
        json!({
            "projectId": "ignored", "cwd": "/home/me/widgets",
            "remoteUrl": "acme/widgets", "repository": null
        }),
    );
    backend.respond("projectClone.retry", json!({ "applied": true }));
    backend.streams.lock().insert(
        "subscribeProjectClones".into(),
        vec![clone_snapshot(
            "failed",
            "connecting",
            None,
            Some("Repository not found."),
        )],
    );
    let (workspace, cx) = workspace(backend.clone(), cx).await;
    cx.dispatch_action(crate::CloneRepository);
    cx.run_until_parked();
    let modal = clone_modal(&workspace, cx).unwrap();
    let (repository, _) = modal.read_with(cx, |modal, _| modal.editors());
    repository.update_in(cx, |editor, window, cx| {
        editor.set_text("acme/widgets", window, cx)
    });
    modal.update_in(cx, |modal, window, cx| modal.clone_repository(window, cx));
    cx.run_until_parked();
    assert_eq!(
        modal.read_with(cx, |modal, _| modal.status()),
        Some("Repository not found.".into())
    );
    // Clone again is Retry, for the same project.
    modal.update_in(cx, |modal, window, cx| modal.clone_repository(window, cx));
    cx.run_until_parked();
    let project = backend.calls("projectClone.start")[0].0["projectId"].clone();
    assert_eq!(
        backend.calls("projectClone.retry"),
        [(json!({ "projectId": project }), Access::SourceControlWrite)]
    );
    assert_eq!(backend.calls("projectClone.start").len(), 1);
}

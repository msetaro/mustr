//! The scheduled tasks page against a recording backend: how tasks read, and the exact
//! `scheduledTasks.*` payloads its buttons and form send. Nothing here talks to T3, and nothing
//! runs a task.

use super::{
    SchedulesView,
    draft::{Draft, ScheduleMode, WorkspaceMode, mask_webhook_address, relative, schedule_label},
    editor::model_selection,
};
use anyhow::{Result, anyhow};
use futures::{FutureExt as _, StreamExt as _, future::BoxFuture, stream::BoxStream};
use gpui::{Entity, TestAppContext, VisualTestContext, Window};
use parking_lot::Mutex;
use pretty_assertions::assert_eq;
use serde_json::{Value, json};
use settings::SettingsStore;
use std::sync::Arc;
use t3_client::t3_api::{
    tagged::{
        OrchestrationV2Command, OrchestrationV2ShellStreamItem, OrchestrationV2ThreadStreamItem,
    },
    types::{
        OrchestrationV2DispatchCommandResult, OrchestrationV2GetTurnItemInput,
        OrchestrationV2GetTurnItemResult, OrchestrationV2ShellSnapshot,
        OrchestrationV2SubscribeShellInput, OrchestrationV2SubscribeThreadInput,
        OrchestrationV2ThreadDetailSnapshot, OrchestrationV2ThreadLaunchInput,
        OrchestrationV2ThreadLaunchResult, RuntimeMode, ScheduledTask, ServerProvider,
    },
};
use thread_chat::{Access, AttachmentUpload, ReadOnlyBackend, ThreadBackend};
use time::macros::datetime;
use url::Url;

const NOW: &str = "2026-10-27T12:00:00.000Z";
const HOOK_PATH: &str = "/api/scheduled-tasks/webhooks/task-hook/secret-token-123";

// Fixtures: T3's wire JSON.

fn task_json(id: &str, title: &str, schedule: Value) -> Value {
    json!({
        "id": id,
        "title": title,
        "prompt": "Look at last night's Sentry issues and open a PR for the worst one.",
        "enabled": true,
        "schedule": schedule,
        "projectId": "project-1",
        "threadId": null,
        "workspaceStrategy": { "type": "worktree", "baseRef": "main", "startFromOrigin": true },
        "modelSelection": {
            "instanceId": "claudeAgent",
            "model": "claude-sonnet",
            "options": [{ "id": "effort", "value": "high" }]
        },
        "runtimeMode": "full-access",
        "interactionMode": "default",
        "createdBy": "user",
        "creationSource": "web",
        "createdAt": NOW,
        "updatedAt": NOW,
        "nextRunAt": "2026-10-28T09:00:00.000Z",
        "lastRunAt": "2026-10-27T09:00:00.000Z",
        "lastRunStatus": "succeeded",
        "lastRunError": null,
        "runCount": 3
    })
}

fn weekday_task() -> Value {
    task_json(
        "task-1",
        "Triage Sentry",
        json!({ "type": "fixed_time", "timeOfDay": "09:00", "weekdays": [1, 2, 3, 4, 5] }),
    )
}

fn webhook_task() -> Value {
    let mut task = task_json(
        "task-hook",
        "Fix failing CI",
        json!({
            "type": "webhook",
            "signature": { "header": "x-hub-signature-256", "encoding": "hex", "prefix": "sha256=" },
            "maxDeliveryAgeMinutes": 30,
            "dedupe": { "replayKey": "{{headers.x-github-delivery}}" }
        }),
    );
    task["webhook"] = json!({ "path": HOOK_PATH, "url": null, "hasSecret": true });
    task["nextRunAt"] = Value::Null;
    task["lastRunStatus"] = json!("failed");
    task["lastRunError"] = json!("The agent couldn't start: no worktree");
    task
}

fn decode<T: serde::de::DeserializeOwned>(value: Value) -> T {
    serde_json::from_value(value).expect("the fixture decodes")
}

fn upsert_json(draft: &Draft) -> Value {
    serde_json::to_value(draft.upsert_input().expect("the draft saves")).unwrap()
}

// Drafts and labels.

#[test]
fn new_tasks_save_with_t3s_defaults() {
    let mut draft = Draft::new(
        Some("project-1".into()),
        Some(model_selection("claudeAgent", "claude-sonnet")),
    );
    draft.title = "  Triage Sentry ".into();
    draft.prompt = "Look at last night's issues.".into();
    assert_eq!(
        upsert_json(&draft),
        json!({
            "title": "Triage Sentry",
            "prompt": "Look at last night's issues.",
            "enabled": true,
            "schedule": { "type": "fixed_time", "timeOfDay": "09:00", "weekdays": [1, 2, 3, 4, 5] },
            "projectId": "project-1",
            "workspaceStrategy": { "type": "worktree", "baseRef": "main", "startFromOrigin": true },
            "modelSelection": { "instanceId": "claudeAgent", "model": "claude-sonnet" },
            "runtimeMode": "full-access",
            "interactionMode": "default",
            "creationSource": "web"
        })
    );

    // Every day leaves the weekdays out; an interval is in milliseconds.
    draft.weekdays = (0..7).collect();
    draft.time_of_day = "7:5".into();
    assert_eq!(
        upsert_json(&draft)["schedule"],
        json!({ "type": "fixed_time", "timeOfDay": "07:05" })
    );
    draft.schedule = ScheduleMode::Every;
    draft.interval_minutes = "90".into();
    draft.workspace = WorkspaceMode::Checkout;
    draft.thread_id = Some("thread-7".into());
    draft.runtime_mode = RuntimeMode::ApprovalRequired;
    draft.enabled = false;
    let input = upsert_json(&draft);
    assert_eq!(
        input["schedule"],
        json!({ "type": "interval", "everyMs": 5400000 })
    );
    assert_eq!(input["workspaceStrategy"], json!({ "type": "root" }));
    assert_eq!(input["threadId"], "thread-7");
    assert_eq!(input["runtimeMode"], "approval-required");
    assert_eq!(input["enabled"], false);

    draft.workspace = WorkspaceMode::ExistingWorktree;
    draft.existing_worktree = " /src/api-hotfix ".into();
    assert_eq!(
        upsert_json(&draft)["workspaceStrategy"],
        json!({ "type": "existing_worktree", "worktreePath": "/src/api-hotfix" })
    );
}

#[test]
fn drafts_say_what_is_missing() {
    let draft = Draft::new(Some("project-1".into()), None);
    let error = |change: fn(&mut Draft)| {
        let mut draft = draft.clone();
        draft.title = "Triage".into();
        draft.prompt = "Do it".into();
        draft.model = Some(model_selection("codex", "gpt-5"));
        change(&mut draft);
        draft.upsert_input().unwrap_err()
    };
    assert_eq!(
        error(|draft| draft.title.clear()),
        "Add a name and a prompt."
    );
    assert_eq!(error(|draft| draft.project_id = None), "Pick a project.");
    assert_eq!(
        error(|draft| draft.model = None),
        "Pick an agent and a model."
    );
    assert_eq!(
        error(|draft| draft.time_of_day = "25:00".into()),
        "Enter the time as HH:MM, such as 09:00."
    );
    assert_eq!(
        error(|draft| draft.weekdays.clear()),
        "Pick at least one day."
    );
    assert_eq!(
        error(|draft| {
            draft.schedule = ScheduleMode::Every;
            draft.interval_minutes = "0.5".into();
        }),
        "Enter an interval of at least one minute."
    );
    assert_eq!(
        error(|draft| draft.workspace = WorkspaceMode::ExistingWorktree),
        "Enter the path of the checkout to run in."
    );
    // New webhook triggers are made in T3 Code.
    assert_eq!(
        error(|draft| draft.schedule = ScheduleMode::Webhook),
        "Edit this task's trigger in T3 Code."
    );
}

#[test]
fn edits_keep_what_the_form_does_not_show() {
    let task: ScheduledTask = decode(weekday_task());
    let mut draft = Draft::from_task(&task);
    assert_eq!(draft.schedule, ScheduleMode::AtTime);
    assert_eq!(draft.weekdays, [1, 2, 3, 4, 5].into());
    draft.title = "Triage Sentry daily".into();
    let input = upsert_json(&draft);
    assert_eq!(input["id"], "task-1");
    assert_eq!(input["requireExisting"], true);
    assert_eq!(input["title"], "Triage Sentry daily");
    // The model keeps its options.
    assert_eq!(
        input["modelSelection"],
        json!({
            "instanceId": "claudeAgent",
            "model": "claude-sonnet",
            "options": [{ "id": "effort", "value": "high" }]
        })
    );
    assert_eq!(
        input["workspaceStrategy"],
        json!({ "type": "worktree", "baseRef": "main", "startFromOrigin": true })
    );

    // A webhook task saves its trigger as it was: T3 keeps the stored secret, dedupe, and
    // filters when they're left out.
    let task: ScheduledTask = decode(webhook_task());
    let mut draft = Draft::from_task(&task);
    assert_eq!(draft.schedule, ScheduleMode::Webhook);
    draft.prompt = "Fix the failing check.".into();
    assert_eq!(
        upsert_json(&draft)["schedule"],
        json!({
            "type": "webhook",
            "signature": { "header": "x-hub-signature-256", "encoding": "hex", "prefix": "sha256=" },
            "maxDeliveryAgeMinutes": 30
        })
    );
}

#[test]
fn tasks_read_like_t3s() {
    let label = |schedule: Value| schedule_label(&decode(schedule));
    assert_eq!(
        label(json!({ "type": "fixed_time", "timeOfDay": "09:00", "weekdays": [1, 2, 3, 4, 5] })),
        "Weekdays at 09:00"
    );
    assert_eq!(
        label(json!({ "type": "fixed_time", "timeOfDay": "18:30" })),
        "Daily at 18:30"
    );
    assert_eq!(
        label(json!({ "type": "fixed_time", "timeOfDay": "08:00", "weekdays": [0, 6, 3] })),
        "Wed, Sat, Sun at 08:00"
    );
    assert_eq!(
        label(json!({ "type": "interval", "everyMs": 900000 })),
        "Every 15 min"
    );
    assert_eq!(
        label(json!({ "type": "interval", "everyMs": 90000 })),
        "Every 90 sec"
    );
    assert_eq!(
        label(json!({ "type": "webhook", "signature": null })),
        "On webhook"
    );

    let now = datetime!(2026-10-27 12:00 UTC);
    let at = |at: &str| relative(at, now).unwrap();
    assert_eq!(at("2026-10-27T12:00:30.000Z"), "in under a minute");
    assert_eq!(at("2026-10-27T12:14:00.000Z"), "in 14m");
    assert_eq!(at("2026-10-27T15:00:00.000Z"), "in 3h");
    assert_eq!(at("2026-10-30T12:00:00.000Z"), "in 3d");
    assert_eq!(at("2026-10-27T11:59:50.000Z"), "just now");
    assert_eq!(at("2026-10-27T11:30:00.000Z"), "30m ago");
    assert_eq!(at("2026-10-27T09:00:00.000Z"), "3h ago");

    let task: ScheduledTask = decode(weekday_task());
    assert_eq!(super::draft::next_run_label(&task, now), "Next run in 21h");
    let task: ScheduledTask = decode(webhook_task());
    assert_eq!(super::draft::next_run_label(&task, now), "Listening");

    assert_eq!(
        mask_webhook_address("http://127.0.0.1:3773/api/scheduled-tasks/webhooks/task-hook/tok"),
        "http://127.0.0.1:3773/api/scheduled-tasks/webhooks/task-hook/••••••••"
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
struct FakeBackend {
    raw_calls: Mutex<Vec<RawCall>>,
}

impl FakeBackend {
    fn calls(&self) -> Vec<RawCall> {
        self.raw_calls
            .lock()
            .iter()
            .filter(|call| call.tag != "scheduledTasks.subscribe")
            .cloned()
            .collect()
    }
}

fn ready<T: Send + 'static>(value: Result<T>) -> BoxFuture<'static, Result<T>> {
    futures::future::ready(value).boxed()
}

fn shell_json() -> Value {
    json!({
        "schemaVersion": 1,
        "snapshotSequence": 7,
        "threads": [{
            "id": "thread-7", "projectId": "project-1", "title": "Nightly triage", "status": "idle",
            "createdAt": NOW, "updatedAt": NOW, "createdBy": "user", "creationSource": "web",
            "interactionMode": "default", "runtimeMode": "full-access",
            "modelSelection": { "instanceId": "claudeAgent", "model": "claude-sonnet" },
            "providerInstanceId": "claudeAgent", "activeProviderThreadId": null,
            "activeRunId": null, "archivedAt": null, "branch": null, "deletedAt": null,
            "forkedFrom": null, "hasActionableProposedPlan": false, "itemCount": 3,
            "latestRunId": null, "latestUserMessageAt": null, "latestVisibleMessage": null,
            "lineage": { "rootThreadId": "thread-7", "parentThreadId": null,
                         "relationshipToParent": null },
            "pendingRuntimeRequest": null, "settledAt": null, "settledOverride": null,
            "visibleItemCount": 3, "worktreePath": null
        }],
        "archivedThreads": [],
        "projects": [{
            "id": "project-1", "title": "api", "workspaceRoot": "/src/api", "scripts": [],
            "createdAt": NOW, "updatedAt": NOW, "defaultModelSelection": null
        }]
    })
}

impl ThreadBackend for FakeBackend {
    fn shell_snapshot(&self) -> BoxFuture<'static, Result<OrchestrationV2ShellSnapshot>> {
        ready(serde_json::from_value(shell_json()).map_err(Into::into))
    }

    fn subscribe_shell(
        &self,
        _: OrchestrationV2SubscribeShellInput,
    ) -> Result<BoxStream<'static, Result<OrchestrationV2ShellStreamItem>>> {
        Ok(futures::stream::pending().boxed())
    }

    fn thread_snapshot(
        &self,
        _: String,
    ) -> BoxFuture<'static, Result<OrchestrationV2ThreadDetailSnapshot>> {
        ready(Err(anyhow!("no threads here")))
    }

    fn subscribe_thread(
        &self,
        _: OrchestrationV2SubscribeThreadInput,
    ) -> Result<BoxStream<'static, Result<OrchestrationV2ThreadStreamItem>>> {
        Err(anyhow!("no threads here"))
    }

    fn turn_item(
        &self,
        _: OrchestrationV2GetTurnItemInput,
    ) -> BoxFuture<'static, Result<OrchestrationV2GetTurnItemResult>> {
        ready(Err(anyhow!("no threads here")))
    }

    fn providers(&self) -> BoxFuture<'static, Result<Vec<ServerProvider>>> {
        ready(
            serde_json::from_value(json!([{
                "instanceId": "claudeAgent", "driver": "claudeAgent",
                "displayName": "Claude Code", "enabled": true, "installed": true,
                "status": "ready", "auth": { "status": "authenticated" },
                "checkedAt": NOW, "version": "2.0.0",
                "models": [
                    { "slug": "claude-opus", "name": "Opus", "isCustom": false, "capabilities": null },
                    { "slug": "claude-sonnet", "name": "Sonnet", "isCustom": false,
                      "isDefault": true, "capabilities": null }
                ]
            }]))
            .map_err(Into::into),
        )
    }

    fn dispatch(
        &self,
        _: OrchestrationV2Command,
    ) -> BoxFuture<'static, Result<OrchestrationV2DispatchCommandResult>> {
        ready(Err(anyhow!("the schedules page dispatches no commands")))
    }

    fn launch_thread(
        &self,
        _: OrchestrationV2ThreadLaunchInput,
    ) -> BoxFuture<'static, Result<OrchestrationV2ThreadLaunchResult>> {
        ready(Err(anyhow!("the schedules page launches no threads")))
    }

    fn call_raw(
        &self,
        tag: &str,
        payload: Value,
        access: Access,
    ) -> BoxFuture<'static, Result<Value>> {
        self.raw_calls.lock().push(RawCall {
            tag: tag.to_string(),
            payload: payload.clone(),
            access,
        });
        // Answer as T3 would, with the task as it is after the change.
        let mut task = weekday_task();
        let response = match tag {
            "scheduledTasks.setEnabled" => {
                task["enabled"] = payload["enabled"].clone();
                json!({ "task": task })
            }
            "scheduledTasks.runNow" => {
                task["lastRunStatus"] = json!("running");
                json!({ "task": task })
            }
            "scheduledTasks.delete" => json!({ "id": payload["id"] }),
            "scheduledTasks.listWebhookDeliveries" => json!({ "deliveries": [{
                "id": "delivery-1", "taskId": "task-hook", "method": "POST",
                "receivedAt": "2026-10-27T11:00:00.000Z", "outcome": "rejected_signature",
                "error": "The signature didn't match", "bodyBytes": 512,
                "contentType": "application/json", "missingFields": [],
                "signatureVerified": false
            }] }),
            "scheduledTasks.upsert" => {
                task["id"] = payload.get("id").cloned().unwrap_or(json!("task-new"));
                task["title"] = payload["title"].clone();
                json!({ "task": task })
            }
            _ => Value::Null,
        };
        ready(Ok(response))
    }

    fn subscribe_raw(
        &self,
        tag: &str,
        payload: Value,
        access: Access,
    ) -> Result<BoxStream<'static, Result<Value>>> {
        self.raw_calls.lock().push(RawCall {
            tag: tag.to_string(),
            payload,
            access,
        });
        let list = json!({ "tasks": [weekday_task(), webhook_task()] });
        Ok(futures::stream::iter([Ok(list)])
            .chain(futures::stream::pending())
            .boxed())
    }

    fn upload(&self, _: AttachmentUpload) -> BoxFuture<'static, Result<()>> {
        ready(Err(anyhow!("the schedules page uploads nothing")))
    }
}

// The page.

fn open_page(
    backend: Arc<dyn ThreadBackend>,
    cx: &mut TestAppContext,
) -> (Entity<SchedulesView>, &mut VisualTestContext) {
    cx.update(|cx| {
        let settings_store = SettingsStore::test(cx);
        cx.set_global(settings_store);
        theme_settings::init(theme::LoadThemes::JustBase, cx);
        thread_chat::set_backend(backend.clone(), cx);
    });
    let (view, cx) = cx.add_window_view(|window: &mut Window, cx| {
        SchedulesView::new(
            Some(backend),
            Box::new(|_| Url::parse("http://127.0.0.1:3773").ok()),
            window,
            cx,
        )
    });
    cx.simulate_resize(gpui::size(gpui::px(1200.), gpui::px(1600.)));
    cx.run_until_parked();
    (view, cx)
}

fn click(selector: &'static str, cx: &mut VisualTestContext) {
    let bounds = cx
        .debug_bounds(selector)
        .unwrap_or_else(|| panic!("{selector} isn't rendered"));
    cx.simulate_click(bounds.center(), gpui::Modifiers::none());
    cx.run_until_parked();
}

fn operate(tag: &str, payload: Value) -> RawCall {
    RawCall {
        tag: tag.to_string(),
        payload,
        access: Access::Operate,
    }
}

#[gpui::test]
async fn the_page_follows_t3s_tasks(cx: &mut TestAppContext) {
    let backend = Arc::new(FakeBackend::default());
    let (view, cx) = open_page(backend.clone(), cx);
    assert_eq!(
        backend.raw_calls.lock()[0],
        RawCall {
            tag: "scheduledTasks.subscribe".into(),
            payload: json!({}),
            access: Access::Read,
        }
    );
    view.read_with(cx, |view, _| {
        let ids = view
            .tasks()
            .iter()
            .map(|task| task.id.as_str())
            .collect::<Vec<_>>();
        // The one with a next run comes first.
        assert_eq!(ids, ["task-1", "task-hook"]);
    });
    assert!(cx.debug_bounds("schedule-task-task-1").is_some());
    // A webhook task can't run now; its URL is copied whole.
    assert!(cx.debug_bounds("schedule-run-task-1").is_some());
    assert!(cx.debug_bounds("schedule-run-task-hook").is_none());
    click("schedule-copy-task-hook", cx);
    assert_eq!(
        cx.read_from_clipboard().and_then(|item| item.text()),
        Some(format!("http://127.0.0.1:3773{HOOK_PATH}"))
    );
    assert!(backend.calls().is_empty());
}

#[gpui::test]
async fn rows_pause_run_and_delete_tasks(cx: &mut TestAppContext) {
    let backend = Arc::new(FakeBackend::default());
    let (view, cx) = open_page(backend.clone(), cx);

    click("schedule-toggle-task-1", cx);
    click("schedule-run-task-1", cx);
    // Delete asks first.
    click("schedule-delete-task-1", cx);
    assert_eq!(backend.calls().len(), 2);
    click("schedule-cancel-delete-task-1", cx);
    click("schedule-delete-task-1", cx);
    click("schedule-confirm-delete-task-1", cx);
    assert_eq!(
        backend.calls(),
        [
            operate(
                "scheduledTasks.setEnabled",
                json!({ "id": "task-1", "enabled": false })
            ),
            operate("scheduledTasks.runNow", json!({ "id": "task-1" })),
            operate("scheduledTasks.delete", json!({ "id": "task-1" })),
        ]
    );
    view.read_with(cx, |view, _| {
        let ids = view
            .tasks()
            .iter()
            .map(|task| task.id.as_str())
            .collect::<Vec<_>>();
        assert_eq!(ids, ["task-hook"]);
    });
}

#[gpui::test]
async fn the_form_creates_and_edits_tasks(cx: &mut TestAppContext) {
    let backend = Arc::new(FakeBackend::default());
    let (view, cx) = open_page(backend.clone(), cx);

    click("schedule-new", cx);
    let editor = view.read_with(cx, |view, _| view.editor.clone().expect("the form is open"));
    // Saving an empty form says what's missing, and sends nothing.
    click("schedule-save", cx);
    assert!(cx.debug_bounds("schedule-editor-error").is_some());
    assert!(backend.calls().is_empty());

    let (title, prompt) = editor.read_with(cx, |editor, _| {
        (editor.title.clone(), editor.prompt.clone())
    });
    title.update_in(cx, |title, window, cx| {
        title.set_text("Weekly deps", window, cx)
    });
    prompt.update_in(cx, |prompt, window, cx| {
        prompt.set_text("Update the dependencies.", window, cx)
    });
    // Saturday too.
    click("schedule-weekday-6", cx);
    click("schedule-save", cx);
    assert_eq!(
        backend.calls(),
        [operate(
            "scheduledTasks.upsert",
            json!({
                "title": "Weekly deps",
                "prompt": "Update the dependencies.",
                "enabled": true,
                "schedule": { "type": "fixed_time", "timeOfDay": "09:00",
                              "weekdays": [1, 2, 3, 4, 5, 6] },
                "projectId": "project-1",
                "workspaceStrategy": { "type": "worktree", "baseRef": "main",
                                       "startFromOrigin": true },
                // The default model of the first agent.
                "modelSelection": { "instanceId": "claudeAgent", "model": "claude-sonnet" },
                "runtimeMode": "full-access",
                "interactionMode": "default",
                "creationSource": "web"
            })
        )]
    );
    view.read_with(cx, |view, _| {
        assert!(view.editor.is_none());
        assert!(view.tasks().iter().any(|task| task.id == "task-new"));
    });

    click("schedule-edit-task-1", cx);
    let editor = view.read_with(cx, |view, _| view.editor.clone().expect("the form is open"));
    let title = editor.read_with(cx, |editor, _| editor.title.clone());
    title.update_in(cx, |title, window, cx| {
        title.set_text("Triage Sentry daily", window, cx)
    });
    click("schedule-weekday-0", cx);
    click("schedule-save", cx);
    let calls = backend.calls();
    assert_eq!(calls.len(), 2);
    let edit = &calls[1].payload;
    assert_eq!(edit["id"], "task-1");
    assert_eq!(edit["requireExisting"], true);
    assert_eq!(edit["title"], "Triage Sentry daily");
    assert_eq!(
        edit["schedule"],
        json!({ "type": "fixed_time", "timeOfDay": "09:00", "weekdays": [0, 1, 2, 3, 4, 5] })
    );
    assert_eq!(
        edit["modelSelection"]["options"],
        json!([{ "id": "effort", "value": "high" }])
    );
}

#[gpui::test]
async fn read_only_backends_change_nothing(cx: &mut TestAppContext) {
    let backend = Arc::new(FakeBackend::default());
    let read_only = Arc::new(ReadOnlyBackend::new(
        backend.clone(),
        "writes are off for a dry run",
    ));
    let (view, cx) = open_page(read_only, cx);
    // The list still loads, and its URL still copies.
    view.read_with(cx, |view, _| assert_eq!(view.tasks().len(), 2));
    click("schedule-toggle-task-1", cx);
    click("schedule-run-task-1", cx);
    click("schedule-delete-task-1", cx);
    click("schedule-new", cx);
    assert!(cx.debug_bounds("schedule-confirm-delete-task-1").is_none());
    view.read_with(cx, |view, _| assert!(view.editor.is_none()));
    // A task can still be looked at in the form, but not saved.
    click("schedule-edit-task-1", cx);
    view.read_with(cx, |view, _| assert!(view.editor.is_some()));
    click("schedule-save", cx);
    assert!(backend.calls().is_empty());
}

#[gpui::test]
async fn webhooks_show_their_deliveries(cx: &mut TestAppContext) {
    let backend = Arc::new(FakeBackend::default());
    let (_view, cx) = open_page(backend.clone(), cx);
    click("schedule-deliveries-task-hook", cx);
    assert_eq!(
        backend.calls(),
        [operate(
            "scheduledTasks.listWebhookDeliveries",
            json!({ "id": "task-hook" })
        )]
    );
    // Hiding them asks for nothing.
    click("schedule-deliveries-task-hook", cx);
    assert_eq!(backend.calls().len(), 1);
}

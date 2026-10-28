//! Tests of the thread model, the thread view, the sidebar's list, and the public API, against
//! a recording backend. Nothing here talks to a T3 server: every write is captured as the typed
//! payload it would send, and the tests assert those payloads exactly.

use crate::{
    backend::{Access, AttachmentUpload, ReadOnlyBackend, ThreadBackend, ThreadBackendExt as _},
    composer::{SendMessage, SteerMessage},
    model::{ItemKind, ItemStatus, Row, ThreadActivity, ThreadState},
    shell::{Shell, ThreadShells},
    sidebar::{ShellStatus, ThreadList},
    thread_view::ThreadView,
};
use anyhow::{Result, anyhow};
use futures::{
    FutureExt as _, StreamExt as _, channel::mpsc, future::BoxFuture, stream::BoxStream,
};
use gpui::{Entity, TestAppContext, VisualContext as _, VisualTestContext, Window};
use parking_lot::Mutex;
use pretty_assertions::assert_eq;
use serde_json::{Value, json};
use settings::SettingsStore;
use std::sync::Arc;
use t3_api::{
    rpc,
    tagged::{
        OrchestrationV2Command, OrchestrationV2ShellStreamItem, OrchestrationV2ThreadStreamItem,
    },
    types::{
        OrchestrationV2DispatchCommandResult, OrchestrationV2GetTurnItemInput,
        OrchestrationV2GetTurnItemResult, OrchestrationV2ShellSnapshot,
        OrchestrationV2SubscribeShellInput, OrchestrationV2SubscribeThreadInput,
        OrchestrationV2ThreadDetailSnapshot, OrchestrationV2ThreadLaunchInput,
        OrchestrationV2ThreadLaunchResult, OrchestrationV2ThreadProjection,
        ProviderApprovalDecision, ServerGetConfigPayload, ServerProvider,
    },
};

const THREAD: &str = "thread-1";
const NOW: &str = "2026-10-27T12:00:00.000Z";

// Fixtures: T3's wire JSON, with every field the contracts require.

fn model_selection() -> Value {
    json!({ "instanceId": "claudeAgent", "model": "claude-sonnet-4-5" })
}

fn thread_json(title: &str) -> Value {
    json!({
        "id": THREAD,
        "projectId": "project-1",
        "title": title,
        "createdAt": NOW,
        "updatedAt": NOW,
        "createdBy": "user",
        "creationSource": "web",
        "interactionMode": "default",
        "runtimeMode": "approval-required",
        "modelSelection": model_selection(),
        "providerInstanceId": "claudeAgent",
        "activeProviderThreadId": null,
        "archivedAt": null,
        "deletedAt": null,
        "forkedFrom": null,
        "branch": "t3/retry-backoff",
        "worktreePath": "/tmp/worktrees/retry-backoff",
        "lineage": { "rootThreadId": THREAD, "parentThreadId": null, "relationshipToParent": null }
    })
}

/// A turn item of `kind` with T3's base fields, plus `fields`.
fn item(
    kind: &str,
    id: &str,
    ordinal: i64,
    run_id: Option<&str>,
    status: &str,
    fields: Value,
) -> Value {
    let mut item = json!({
        "type": kind,
        "id": id,
        "threadId": THREAD,
        "runId": run_id,
        "nodeId": null,
        "providerThreadId": null,
        "providerTurnId": null,
        "parentItemId": null,
        "nativeItemRef": null,
        "ordinal": ordinal,
        "status": status,
        "title": null,
        "startedAt": null,
        "completedAt": null,
        "updatedAt": NOW
    });
    if let (Value::Object(item), Value::Object(fields)) = (&mut item, fields) {
        item.extend(fields);
    }
    item
}

fn user_message(id: &str, ordinal: i64, run_id: &str, text: &str) -> Value {
    item(
        "user_message",
        id,
        ordinal,
        Some(run_id),
        "completed",
        json!({
            "messageId": format!("message-{id}"),
            "text": text,
            "attachments": [],
            "createdBy": "user",
            "creationSource": "web",
            "inputIntent": "turn_start"
        }),
    )
}

fn assistant_message(id: &str, ordinal: i64, run_id: &str, text: &str, streaming: bool) -> Value {
    item(
        "assistant_message",
        id,
        ordinal,
        Some(run_id),
        if streaming { "running" } else { "completed" },
        json!({ "messageId": format!("message-{id}"), "text": text, "streaming": streaming }),
    )
}

fn command(id: &str, ordinal: i64, run_id: &str, input: &str, status: &str) -> Value {
    item(
        "command_execution",
        id,
        ordinal,
        Some(run_id),
        status,
        json!({ "input": input, "outputOmitted": true }),
    )
}

fn file_change(id: &str, ordinal: i64, run_id: &str, path: &str) -> Value {
    item(
        "file_change",
        id,
        ordinal,
        Some(run_id),
        "completed",
        json!({ "fileName": path, "additions": 12, "deletions": 3 }),
    )
}

fn approval(id: &str, ordinal: i64, run_id: &str, request_id: &str) -> Value {
    item(
        "approval_request",
        id,
        ordinal,
        Some(run_id),
        "waiting",
        json!({
            "requestId": request_id,
            "requestKind": "command",
            "prompt": "cargo test -p webhooks"
        }),
    )
}

fn run(id: &str, ordinal: i64, status: &str, user_message_id: &str) -> Value {
    json!({
        "id": id,
        "threadId": THREAD,
        "ordinal": ordinal,
        "status": status,
        "userMessageId": user_message_id,
        "modelSelection": model_selection(),
        "providerInstanceId": "claudeAgent",
        "providerThreadId": null,
        "activeAttemptId": null,
        "checkpointId": null,
        "completedAt": null,
        "contextHandoffId": null,
        "requestedAt": NOW,
        "rootNodeId": null,
        "startedAt": null
    })
}

fn message(id: &str, run_id: &str, text: &str) -> Value {
    json!({
        "id": id,
        "threadId": THREAD,
        "runId": run_id,
        "nodeId": null,
        "role": "user",
        "text": text,
        "attachments": [],
        "streaming": false,
        "createdBy": "user",
        "creationSource": "web",
        "createdAt": NOW,
        "updatedAt": NOW
    })
}

fn runtime_request(id: &str, status: &str) -> Value {
    json!({
        "id": id,
        "kind": "command",
        "status": status,
        "nodeId": "node-1",
        "createdAt": NOW,
        "resolvedAt": null,
        "providerTurnId": null,
        "nativeRequestRef": null,
        "responseCapability": { "type": "live", "providerSessionId": "session-1" }
    })
}

fn provider_turn(id: &str, ordinal: i64, used: i64, updated_at: &str) -> Value {
    json!({
        "id": id,
        "nodeId": "node-1",
        "ordinal": ordinal,
        "providerThreadId": "provider-thread-1",
        "runAttemptId": null,
        "status": "completed",
        "startedAt": null,
        "completedAt": null,
        "nativeTurnRef": null,
        "tokenUsage": { "usedTokens": used, "maxTokens": 200000, "updatedAt": updated_at }
    })
}

#[derive(Default)]
struct Projection {
    items: Vec<Value>,
    runs: Vec<Value>,
    messages: Vec<Value>,
    runtime_requests: Vec<Value>,
    provider_turns: Vec<Value>,
}

impl Projection {
    fn json(self) -> Value {
        let visible = self
            .items
            .iter()
            .enumerate()
            .map(|(position, item)| {
                json!({
                    "position": position,
                    "visibility": "local",
                    "sourceThreadId": THREAD,
                    "sourceItemId": item["id"],
                    "item": item
                })
            })
            .collect::<Vec<_>>();
        json!({
            "thread": thread_json("Add retries to the webhook dispatcher"),
            "attempts": [],
            "checkpointScopes": [],
            "checkpoints": [],
            "contextHandoffs": [],
            "contextTransfers": [],
            "messages": self.messages,
            "nodes": [],
            "plans": [],
            "providerSessions": [],
            "providerThreads": [],
            "providerTurns": self.provider_turns,
            "runs": self.runs,
            "runtimeRequests": self.runtime_requests,
            "subagents": [],
            "turnItems": self.items,
            "visibleTurnItems": visible,
            "updatedAt": NOW
        })
    }

    fn decode(self) -> OrchestrationV2ThreadProjection {
        serde_json::from_value(self.json()).expect("the projection fixture decodes")
    }
}

/// A finished first run: a message, some work, an answer.
fn finished_thread() -> Projection {
    Projection {
        items: vec![
            user_message("i1", 1, "run-1", "Add retries with backoff"),
            command("i2", 2, "run-1", "rg \"withRetry\" src", "completed"),
            file_change("i3", 3, "run-1", "src/webhooks/dispatcher.ts"),
            command("i4", 4, "run-1", "npm test", "failed"),
            assistant_message(
                "i5",
                5,
                "run-1",
                "Done. `withRetry` now wraps `post()`.",
                false,
            ),
        ],
        runs: vec![run("run-1", 1, "completed", "m1")],
        messages: vec![message("m1", "run-1", "Add retries with backoff")],
        provider_turns: vec![
            provider_turn("turn-1", 1, 30_000, "2026-10-27T11:00:00.000Z"),
            provider_turn("turn-2", 2, 42_000, "2026-10-27T11:30:00.000Z"),
        ],
        ..Default::default()
    }
}

/// A second run in progress, waiting on an approval, with a message queued behind it.
fn running_thread() -> Projection {
    let mut projection = finished_thread();
    projection.items.extend([
        user_message("i6", 6, "run-2", "Now handle 429s"),
        command("i7", 7, "run-2", "cargo test -p webhooks", "running"),
        approval("i8", 8, "run-2", "request-1"),
    ]);
    projection.runs.extend([
        run("run-2", 2, "running", "m2"),
        run("run-3", 3, "queued", "m3"),
    ]);
    projection.messages.extend([
        message("m2", "run-2", "Now handle 429s"),
        message("m3", "run-3", "Then update the docs"),
    ]);
    projection
        .runtime_requests
        .push(runtime_request("request-1", "pending"));
    projection
}

fn event(sequence: i64, kind: &str, payload: Value) -> OrchestrationV2ThreadStreamItem {
    serde_json::from_value(json!({
        "kind": "event",
        "sequence": sequence,
        "event": {
            "id": format!("event-{sequence}"),
            "type": kind,
            "threadId": THREAD,
            "occurredAt": NOW,
            "payload": payload
        }
    }))
    .unwrap_or_else(|error| panic!("the {kind} event fixture decodes: {error}"))
}

// The model.

fn kinds(state: &ThreadState) -> Vec<String> {
    state
        .rows()
        .iter()
        .map(|row| match row {
            Row::Item(index) => match &state.items()[*index].kind {
                ItemKind::UserMessage { text, .. } => format!("user: {text}"),
                ItemKind::AssistantMessage { text, .. } => format!("agent: {text}"),
                ItemKind::Approval { request_id, .. } => format!("approval {request_id}"),
                other => format!("{other:?}"),
            },
            Row::Work(indices) => format!(
                "work: {}",
                indices
                    .iter()
                    .map(|index| state.items()[*index].id.as_str())
                    .collect::<Vec<_>>()
                    .join(",")
            ),
        })
        .collect()
}

#[test]
fn projections_become_rows_with_work_grouped() {
    let state = ThreadState::new(finished_thread().decode(), 10);
    assert_eq!(
        kinds(&state),
        [
            "user: Add retries with backoff",
            "work: i2,i3,i4",
            "agent: Done. `withRetry` now wraps `post()`."
        ]
    );
    let command = &state.items()[1];
    assert!(matches!(
        &command.kind,
        ItemKind::Tool { summary, output_omitted: true, .. } if summary == "rg \"withRetry\" src"
    ));
    assert_eq!(state.items()[3].status, ItemStatus::Failed);
    assert_eq!(
        state.activity(),
        ThreadActivity::Idle(Some("completed".into()))
    );
    assert!(!state.is_running());
    // The newest provider turn's usage, by when it was reported.
    let usage = state.context_usage().unwrap();
    assert_eq!(
        (usage.used_tokens, usage.max_tokens),
        (42_000, Some(200_000))
    );
}

#[test]
fn running_threads_have_requests_and_a_queue() {
    let state = ThreadState::new(running_thread().decode(), 20);
    assert_eq!(state.activity(), ThreadActivity::NeedsInput);
    assert_eq!(state.active_run().map(|run| run.id.as_str()), Some("run-2"));
    assert!(state.is_running());
    let pending = state.pending_requests();
    assert_eq!(pending.len(), 1);
    assert_eq!(pending[0].request_id, "request-1");
    assert!(pending[0].can_respond);
    assert!(!pending[0].is_question);
    assert!(matches!(
        pending[0].item.as_ref().map(|item| &item.kind),
        Some(ItemKind::Approval { prompt: Some(prompt), .. }) if prompt == "cargo test -p webhooks"
    ));
    let queued = state.queued_messages();
    assert_eq!(queued.len(), 1);
    assert_eq!(queued[0].run_id, "run-3");
    assert_eq!(queued[0].text, "Then update the docs");
}

#[test]
fn events_replace_items_by_id_and_insert_new_ones_in_order() {
    let mut state = ThreadState::new(finished_thread().decode(), 10);
    let rows_before = state.rows();

    // Streaming: each update carries the whole text so far.
    let streaming = assistant_message("i9", 9, "run-2", "Looking", true);
    assert!(
        state
            .apply(event(11, "turn-item.updated", streaming))
            .changed
    );
    let streamed = assistant_message("i9", 9, "run-2", "Looking at the dispatcher", true);
    assert!(
        state
            .apply(event(12, "turn-item.updated", streamed))
            .changed
    );
    // An item that sorts before the new one, arriving after it.
    let earlier = user_message("i6", 6, "run-2", "Now handle 429s");
    assert!(state.apply(event(13, "turn-item.updated", earlier)).changed);

    assert_eq!(
        kinds(&state)[3..],
        ["user: Now handle 429s", "agent: Looking at the dispatcher"]
    );
    // The rows before the new ones didn't change.
    assert_eq!(state.rows()[..3], rows_before[..]);
    assert_eq!(state.sequence, 13);

    // Old and repeated events are dropped.
    let stale = assistant_message("i9", 9, "run-2", "stale", false);
    assert!(!state.apply(event(12, "turn-item.updated", stale)).changed);
    assert_eq!(kinds(&state)[4], "agent: Looking at the dispatcher");
}

#[test]
fn run_and_request_events_change_the_activity() {
    let mut state = ThreadState::new(finished_thread().decode(), 10);
    state.apply(event(11, "run.created", run("run-2", 2, "running", "m2")));
    assert_eq!(state.activity(), ThreadActivity::Working("running".into()));
    state.apply(event(
        12,
        "runtime-request.updated",
        runtime_request("request-1", "pending"),
    ));
    assert_eq!(state.activity(), ThreadActivity::NeedsInput);
    state.apply(event(
        13,
        "runtime-request.updated",
        runtime_request("request-1", "resolved"),
    ));
    state.apply(event(14, "run.updated", run("run-2", 2, "failed", "m2")));
    assert_eq!(state.activity(), ThreadActivity::Failed);
    assert!(!state.is_running());

    let mut renamed = thread_json("Retry with backoff");
    renamed["updatedAt"] = json!("2026-10-27T13:00:00.000Z");
    state.apply(event(15, "thread.metadata-updated", renamed));
    assert_eq!(state.thread.title, "Retry with backoff");
}

#[test]
fn rolled_back_runs_and_cancelled_queued_messages_are_hidden() {
    let mut projection = finished_thread();
    projection.items.push(item(
        "user_message",
        "i6",
        6,
        Some("run-2"),
        "completed",
        json!({
            "messageId": "m2", "text": "never mind", "attachments": [], "createdBy": "user",
            "creationSource": "web", "inputIntent": "queued_turn"
        }),
    ));
    projection
        .items
        .push(assistant_message("i7", 7, "run-3", "undone", false));
    projection.runs.push(run("run-2", 2, "cancelled", "m2"));
    projection.runs.push(run("run-3", 3, "rolled_back", "m3"));
    let state = ThreadState::new(projection.decode(), 10);
    assert_eq!(state.items().len(), 7);
    assert_eq!(state.rows().len(), 3);
}

#[test]
fn unknown_events_keep_their_place() {
    let mut state = ThreadState::new(finished_thread().decode(), 10);
    let unknown = serde_json::from_value::<OrchestrationV2ThreadStreamItem>(json!({
        "kind": "event",
        "sequence": 11,
        "event": { "type": "something.new", "payload": { "x": 1 } }
    }))
    .unwrap();
    assert!(!state.apply(unknown).changed);
    assert_eq!(state.sequence, 11);
}

#[test]
fn commands_lose_their_shell_wrapper() {
    use crate::model::display_command;
    assert_eq!(display_command("/usr/bin/zsh -lc \"npm test\""), "npm test");
    assert_eq!(display_command("bash -c 'rg foo src'"), "rg foo src");
    assert_eq!(
        display_command("  cargo test -p webhooks "),
        "cargo test -p webhooks"
    );
    assert_eq!(
        display_command("/usr/bin/python3 -c 'print(1)'"),
        "/usr/bin/python3 -c 'print(1)'"
    );
}

// The sidebar's list.

fn shell_thread(id: &str, title: &str, status: &str, updated_at: &str) -> Value {
    json!({
        "id": id, "projectId": "project-1", "title": title, "status": status,
        "createdAt": NOW, "updatedAt": updated_at, "createdBy": "user", "creationSource": "web",
        "interactionMode": "default", "runtimeMode": "approval-required",
        "modelSelection": model_selection(), "providerInstanceId": "claudeAgent",
        "activeProviderThreadId": null, "activeRunId": null, "archivedAt": null, "branch": null,
        "deletedAt": null, "forkedFrom": null, "hasActionableProposedPlan": false, "itemCount": 3,
        "latestRunId": null, "latestUserMessageAt": null, "latestVisibleMessage": null,
        "lineage": { "rootThreadId": id, "parentThreadId": null, "relationshipToParent": null },
        "pendingRuntimeRequest": null, "settledAt": null, "settledOverride": null,
        "visibleItemCount": 3, "worktreePath": null
    })
}

fn shell_json() -> Value {
    json!({
        "schemaVersion": 1,
        "snapshotSequence": 7,
        "threads": [
            shell_thread("old", "Old thread", "completed", "2026-10-20T00:00:00.000Z"),
            shell_thread(THREAD, "Add retries to the webhook dispatcher", "running", NOW),
        ],
        "archivedThreads": [],
        "projects": [{
            "id": "project-1", "title": "api", "workspaceRoot": "/src/api", "scripts": [],
            "createdAt": NOW, "updatedAt": NOW, "defaultModelSelection": null
        }]
    })
}

#[test]
fn the_thread_list_follows_the_shell() {
    let snapshot: OrchestrationV2ShellSnapshot = serde_json::from_value(shell_json()).unwrap();
    let mut list = ThreadList::from_snapshot(&snapshot);
    assert_eq!(
        list.threads
            .iter()
            .map(|thread| thread.id.as_str())
            .collect::<Vec<_>>(),
        [THREAD, "old"]
    );
    assert_eq!(
        list.threads[0].status,
        ShellStatus::Working("running".into())
    );
    assert_eq!(list.projects["project-1"], "api");

    let mut waiting = shell_thread("old", "Old thread", "running", "2026-10-28T00:00:00.000Z");
    waiting["pendingRuntimeRequest"] =
        json!({ "id": "request-1", "kind": "command", "createdAt": NOW });
    let updated: OrchestrationV2ShellStreamItem = serde_json::from_value(json!({
        "kind": "thread.updated", "sequence": 8, "location": "active", "thread": waiting
    }))
    .unwrap();
    assert!(list.apply(updated));
    assert_eq!(list.threads[0].id, "old");
    assert_eq!(list.threads[0].status, ShellStatus::NeedsInput);
    assert_eq!(list.threads[0].status.label(), "needs input");

    // On a resumed subscription T3 first sends the projects alone; the threads stay.
    let mut projects_only = shell_json();
    projects_only["threads"] = json!([]);
    projects_only["projects"][0]["title"] = json!("api (renamed)");
    let enrichment: OrchestrationV2ShellStreamItem = serde_json::from_value(json!({
        "kind": "snapshot", "snapshot": projects_only, "resolvedRepositoryIdentityRoots": []
    }))
    .unwrap();
    assert!(list.apply(enrichment));
    assert_eq!(list.threads.len(), 2);
    assert_eq!(list.projects["project-1"], "api (renamed)");

    let removed: OrchestrationV2ShellStreamItem = serde_json::from_value(json!({
        "kind": "thread.removed", "sequence": 9, "location": "active", "threadId": THREAD
    }))
    .unwrap();
    assert!(list.apply(removed));
    assert_eq!(list.threads.len(), 1);
    assert_eq!(list.sequence, 9);
}

// The app's shell.

fn shell_update(sequence: i64, location: &str, thread: Value) -> OrchestrationV2ShellStreamItem {
    serde_json::from_value(json!({
        "kind": "thread.updated", "sequence": sequence, "location": location, "thread": thread
    }))
    .unwrap()
}

fn shell_removal(sequence: i64, thread_id: &str) -> OrchestrationV2ShellStreamItem {
    serde_json::from_value(json!({
        "kind": "thread.removed", "sequence": sequence, "location": "active", "threadId": thread_id
    }))
    .unwrap()
}

#[test]
fn the_shell_keeps_t3s_records_of_active_threads() {
    let snapshot: OrchestrationV2ShellSnapshot = serde_json::from_value(shell_json()).unwrap();
    let mut shell = Shell::from_snapshot(&snapshot);
    assert_eq!(shell.sequence, 7);
    assert_eq!(shell.projects["project-1"].title, "api");

    // A subagent names its parent in its lineage.
    let mut child = shell_thread("child", "Write the tests", "running", NOW);
    child["lineage"] = json!({
        "rootThreadId": THREAD, "parentThreadId": THREAD, "relationshipToParent": "subagent"
    });
    assert!(shell.apply(shell_update(8, "active", child)));
    assert_eq!(
        shell.threads["child"].lineage.parent_thread_id.as_deref(),
        Some(THREAD)
    );

    // Events the shell already has change nothing.
    assert!(!shell.apply(shell_removal(8, THREAD)));
    assert!(shell.threads.contains_key(THREAD));

    // Archived threads leave it, as do removed ones.
    let archived = shell_thread("old", "Old thread", "completed", NOW);
    assert!(shell.apply(shell_update(9, "archive", archived)));
    assert!(shell.apply(shell_removal(10, "child")));
    assert_eq!(shell.threads.keys().collect::<Vec<_>>(), [THREAD]);
    assert_eq!(shell.sequence, 10);
}

#[gpui::test]
async fn the_app_follows_the_shell_once(cx: &mut TestAppContext) {
    let backend = Arc::new(FakeBackend::default());
    cx.update(|cx| crate::set_backend(backend, cx));
    let shells = cx.update(ThreadShells::global);
    cx.run_until_parked();
    shells.read_with(cx, |shells, _| {
        assert_eq!(
            shells.thread(THREAD).map(|thread| thread.title.as_str()),
            Some("Add retries to the webhook dispatcher")
        );
        assert_eq!(shells.error(), None);
    });
    let again = cx.update(ThreadShells::global);
    assert_eq!(again.entity_id(), shells.entity_id());
}

// A recording backend.

/// One call of [`ThreadBackend::call_raw`] or [`ThreadBackend::subscribe_raw`].
#[derive(Clone, Debug, PartialEq)]
struct RawCall {
    tag: String,
    payload: Value,
    access: Access,
}

/// One [`ThreadBackend::upload`], without its progress callback.
#[derive(Clone, Debug, PartialEq)]
struct Upload {
    relative_url: String,
    mime_type: String,
    bytes: Vec<u8>,
}

#[derive(Default)]
struct FakeBackend {
    snapshot: Mutex<Option<Value>>,
    commands: Mutex<Vec<OrchestrationV2Command>>,
    launches: Mutex<Vec<OrchestrationV2ThreadLaunchInput>>,
    subscriptions: Mutex<Vec<OrchestrationV2SubscribeThreadInput>>,
    events: Mutex<Option<mpsc::UnboundedSender<Result<OrchestrationV2ThreadStreamItem>>>>,
    turn_items: Mutex<Vec<OrchestrationV2GetTurnItemInput>>,
    raw_calls: Mutex<Vec<RawCall>>,
    /// What `call_raw` answers, in order; `null` once they run out.
    raw_responses: Mutex<Vec<Value>>,
    /// The items each `subscribe_raw` stream yields.
    raw_stream_items: Mutex<Vec<Value>>,
    uploads: Mutex<Vec<Upload>>,
}

impl FakeBackend {
    fn with_thread(projection: Projection, sequence: i64) -> Arc<Self> {
        let backend = Self::default();
        *backend.snapshot.lock() = Some(json!({
            "snapshotSequence": sequence,
            "projection": projection.json()
        }));
        Arc::new(backend)
    }

    fn push(&self, item: OrchestrationV2ThreadStreamItem) {
        self.events
            .lock()
            .as_ref()
            .expect("the view subscribed")
            .unbounded_send(Ok(item))
            .expect("the subscription is open");
    }

    fn commands(&self) -> Vec<Value> {
        self.commands
            .lock()
            .iter()
            .map(|command| serde_json::to_value(command).unwrap())
            .collect()
    }

    fn record_raw(&self, tag: &str, payload: Value, access: Access) {
        self.raw_calls.lock().push(RawCall {
            tag: tag.to_string(),
            payload,
            access,
        });
    }
}

fn ready<T: Send + 'static>(value: Result<T>) -> BoxFuture<'static, Result<T>> {
    futures::future::ready(value).boxed()
}

impl ThreadBackend for FakeBackend {
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
        let snapshot = self.snapshot.lock().clone();
        ready(
            snapshot
                .ok_or_else(|| anyhow!("no such thread"))
                .and_then(|snapshot| serde_json::from_value(snapshot).map_err(Into::into)),
        )
    }

    fn subscribe_thread(
        &self,
        input: OrchestrationV2SubscribeThreadInput,
    ) -> Result<BoxStream<'static, Result<OrchestrationV2ThreadStreamItem>>> {
        self.subscriptions.lock().push(input);
        let (sender, receiver) = mpsc::unbounded();
        *self.events.lock() = Some(sender);
        Ok(receiver.boxed())
    }

    fn turn_item(
        &self,
        input: OrchestrationV2GetTurnItemInput,
    ) -> BoxFuture<'static, Result<OrchestrationV2GetTurnItemResult>> {
        let item_id = input.item_id.clone();
        self.turn_items.lock().push(input);
        let mut item = command(&item_id, 2, "run-1", "rg \"withRetry\" src", "completed");
        item["outputOmitted"] = json!(false);
        item["output"] = json!("src/webhooks/dispatcher.ts:40: withRetry(");
        ready(serde_json::from_value(json!({ "item": item })).map_err(Into::into))
    }

    fn providers(&self) -> BoxFuture<'static, Result<Vec<ServerProvider>>> {
        ready(
            serde_json::from_value(json!([
                {
                    "instanceId": "claudeAgent", "driver": "claudeAgent",
                    "displayName": "Claude Code", "enabled": true, "installed": true,
                    "status": "ready", "auth": { "status": "authenticated" },
                    "checkedAt": NOW, "version": "2.0.0",
                    "supportedRuntimeModes": ["approval-required", "full-access"],
                    "models": [
                        { "slug": "claude-opus", "name": "Opus", "isCustom": false, "capabilities": null },
                        { "slug": "claude-sonnet", "name": "Sonnet", "isCustom": false, "isDefault": true, "capabilities": null }
                    ]
                },
                {
                    "instanceId": "codex", "driver": "codex", "enabled": true, "installed": false,
                    "status": "warning", "auth": { "status": "unknown" }, "checkedAt": NOW,
                    "version": null, "models": []
                }
            ]))
            .map_err(Into::into),
        )
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
        let projection = Projection {
            items: vec![user_message(
                "i1",
                1,
                "run-1",
                "Add retries to the webhook dispatcher",
            )],
            runs: vec![run("run-1", 1, "preparing", "m1")],
            messages: vec![message(
                "m1",
                "run-1",
                "Add retries to the webhook dispatcher",
            )],
            ..Default::default()
        };
        ready(
            serde_json::from_value(json!({
                "threadId": THREAD,
                "resumed": false,
                "projection": projection.json()
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
        self.record_raw(tag, payload, access);
        let mut responses = self.raw_responses.lock();
        ready(Ok(if responses.is_empty() {
            Value::Null
        } else {
            responses.remove(0)
        }))
    }

    fn subscribe_raw(
        &self,
        tag: &str,
        payload: Value,
        access: Access,
    ) -> Result<BoxStream<'static, Result<Value>>> {
        self.record_raw(tag, payload, access);
        let items = self.raw_stream_items.lock().clone();
        Ok(futures::stream::iter(items.into_iter().map(Ok)).boxed())
    }

    fn upload(&self, upload: AttachmentUpload) -> BoxFuture<'static, Result<()>> {
        if let Some(on_progress) = &upload.on_progress {
            on_progress(upload.bytes.len() as u64);
        }
        self.uploads.lock().push(Upload {
            relative_url: upload.relative_url,
            mime_type: upload.mime_type,
            bytes: upload.bytes,
        });
        ready(Ok(()))
    }
}

// The view.

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
    cx.run_until_parked();
    (view, cx)
}

fn type_message(view: &Entity<ThreadView>, text: &str, cx: &mut VisualTestContext) {
    let composer = view.read_with(cx, |view, _| view.composer().clone());
    composer.update_in(cx, |editor, window, cx| editor.set_text(text, window, cx));
    cx.focus(&composer);
}

fn composer_text(view: &Entity<ThreadView>, cx: &mut VisualTestContext) -> String {
    view.read_with(cx, |view, cx| view.composer().read(cx).text(cx))
}

#[gpui::test]
async fn the_view_shows_the_snapshot_then_follows_the_thread(cx: &mut TestAppContext) {
    let backend = FakeBackend::with_thread(finished_thread(), 10);
    let (view, cx) = open_view(backend.clone(), Some(THREAD), cx);

    view.read_with(cx, |view, _| {
        assert_eq!(view.title(), "Add retries to the webhook dispatcher");
        assert_eq!(view.rows().len(), 3);
    });
    // It follows from the snapshot's sequence, with full bodies.
    assert_eq!(
        serde_json::to_value(&*backend.subscriptions.lock()).unwrap(),
        json!([{ "threadId": THREAD, "afterSequence": 10 }])
    );

    backend.push(event(11, "run.created", run("run-2", 2, "running", "m2")));
    backend.push(event(
        12,
        "turn-item.updated",
        user_message("i6", 6, "run-2", "Now 429s"),
    ));
    backend.push(event(
        13,
        "turn-item.updated",
        assistant_message("i7", 7, "run-2", "On it", true),
    ));
    cx.run_until_parked();
    view.read_with(cx, |view, _| {
        let state = view.thread_state().unwrap();
        assert_eq!(state.sequence, 13);
        assert!(state.is_running());
        assert_eq!(view.rows().len(), 5);
    });

    // A collapsed tool call fetches its output when expanded.
    view.update(cx, |view, cx| view.toggle_tool("i2".into(), cx));
    cx.run_until_parked();
    assert_eq!(backend.turn_items.lock()[0].item_id, "i2");
    view.read_with(cx, |view, _| {
        let item = &view.thread_state().unwrap().items()[1];
        assert!(matches!(
            &item.kind,
            ItemKind::Tool { output: Some(output), output_omitted: false, .. }
                if output.contains("withRetry")
        ));
    });
    cx.run_until_parked();
}

#[gpui::test]
async fn sending_starts_a_run_when_idle(cx: &mut TestAppContext) {
    let backend = FakeBackend::with_thread(finished_thread(), 10);
    let (view, cx) = open_view(backend.clone(), Some(THREAD), cx);

    type_message(&view, "Now handle 429s too", cx);
    cx.dispatch_action(SendMessage);
    cx.run_until_parked();

    let commands = backend.commands();
    assert_eq!(commands.len(), 1);
    let command = &commands[0];
    assert_eq!(command["type"], "message.dispatch");
    assert_eq!(command["threadId"], THREAD);
    assert_eq!(command["text"], "Now handle 429s too");
    assert_eq!(command["deliveryIntent"], "auto");
    assert_eq!(
        command["dispatchMode"],
        json!({ "type": "start_immediately" })
    );
    assert_eq!(command["createdBy"], "user");
    assert_eq!(command["creationSource"], "web");
    assert!(uuid::Uuid::parse_str(command["commandId"].as_str().unwrap()).is_ok());
    assert!(uuid::Uuid::parse_str(command["messageId"].as_str().unwrap()).is_ok());
    // The composer clears once T3 takes the message.
    assert_eq!(composer_text(&view, cx), "");

    // Blank messages aren't sent.
    type_message(&view, "   ", cx);
    cx.dispatch_action(SendMessage);
    cx.run_until_parked();
    assert_eq!(backend.commands().len(), 1);
}

#[gpui::test]
async fn while_running_messages_queue_or_steer(cx: &mut TestAppContext) {
    let backend = FakeBackend::with_thread(running_thread(), 20);
    let (view, cx) = open_view(backend.clone(), Some(THREAD), cx);

    type_message(&view, "Then bump the version", cx);
    cx.dispatch_action(SendMessage);
    cx.run_until_parked();
    type_message(&view, "Use exponential backoff", cx);
    cx.dispatch_action(SteerMessage);
    cx.run_until_parked();

    let commands = backend.commands();
    assert_eq!(
        commands
            .iter()
            .map(|command| (
                command["text"].clone(),
                command["dispatchMode"].clone(),
                command.get("deliveryIntent").cloned()
            ))
            .collect::<Vec<_>>(),
        [
            (
                json!("Then bump the version"),
                json!({ "type": "queue_after_active" }),
                None
            ),
            (
                json!("Use exponential backoff"),
                json!({ "type": "start_immediately" }),
                Some(json!("steer"))
            ),
        ]
    );
}

#[gpui::test]
async fn stopping_approving_and_managing_the_queue(cx: &mut TestAppContext) {
    let backend = FakeBackend::with_thread(running_thread(), 20);
    let (view, cx) = open_view(backend.clone(), Some(THREAD), cx);

    view.update_in(cx, |view, window, cx| {
        view.respond(
            "request-1".into(),
            ProviderApprovalDecision::Accept,
            window,
            cx,
        );
        view.steer_with_queued("run-3".into(), window, cx);
        view.cancel_queued("run-3".into(), window, cx);
        view.stop(window, cx);
    });
    cx.run_until_parked();

    let commands = backend
        .commands()
        .into_iter()
        .map(|mut command| {
            // Fresh UUIDs; checked above.
            command.as_object_mut().unwrap().remove("commandId");
            command
        })
        .collect::<Vec<_>>();
    assert_eq!(
        commands,
        [
            json!({
                "type": "runtime-request.respond", "threadId": THREAD,
                "requestId": "request-1", "decision": "accept"
            }),
            json!({
                "type": "queued-message.promote-to-steer", "threadId": THREAD,
                "queuedRunId": "run-3", "targetRunId": "run-2"
            }),
            json!({ "type": "queued-run.cancel", "threadId": THREAD, "runId": "run-3" }),
            json!({ "type": "thread.stop", "threadId": THREAD }),
        ]
    );
}

#[gpui::test]
async fn a_read_only_backend_sends_nothing(cx: &mut TestAppContext) {
    let recording = FakeBackend::with_thread(running_thread(), 20);
    let backend = Arc::new(ReadOnlyBackend::new(
        recording.clone(),
        "MUSTR_DRY_RUN_WRITES is set",
    ));
    let (view, cx) = open_view(backend, Some(THREAD), cx);

    // Reads go through.
    view.read_with(cx, |view, _| assert!(view.thread_state().is_some()));
    view.update_in(cx, |view, window, cx| {
        view.respond(
            "request-1".into(),
            ProviderApprovalDecision::Accept,
            window,
            cx,
        );
        view.stop(window, cx);
    });
    type_message(&view, "hello", cx);
    cx.dispatch_action(SendMessage);
    cx.run_until_parked();

    assert!(recording.commands.lock().is_empty());
    view.read_with(cx, |view, _| {
        assert_eq!(
            view.error().map(|error| error.to_string()),
            Some("Not sent: MUSTR_DRY_RUN_WRITES is set.".to_string())
        );
    });
    // The message stays in the composer, so nothing typed is lost.
    assert_eq!(composer_text(&view, cx), "hello");
}

#[gpui::test]
async fn new_threads_launch_with_the_form_choices(cx: &mut TestAppContext) {
    let backend = Arc::new(FakeBackend::default());
    let (view, cx) = open_view(backend.clone(), None, cx);

    type_message(&view, "Add retries to the webhook dispatcher", cx);
    cx.dispatch_action(SendMessage);
    cx.run_until_parked();

    let launches = backend.launches.lock().clone();
    view.read_with(cx, |view, _| assert_eq!(view.error(), None));
    assert_eq!(launches.len(), 1);
    let mut launch = serde_json::to_value(&launches[0]).unwrap();
    let object = launch.as_object_mut().unwrap();
    for id in ["commandId", "threadId"] {
        assert!(uuid::Uuid::parse_str(object.remove(id).unwrap().as_str().unwrap()).is_ok());
    }
    object["initialMessage"]
        .as_object_mut()
        .unwrap()
        .remove("messageId");
    assert_eq!(
        launch,
        json!({
            "creationSource": "web",
            "projectId": "project-1",
            "title": "Add retries to the webhook dispatcher",
            "generateTitle": true,
            // The default model of the first agent that's installed.
            "modelSelection": { "instanceId": "claudeAgent", "model": "claude-sonnet" },
            // This backend has no settings for new threads, so T3's own defaults apply
            // (`thread_settings_tests.rs` covers the settings).
            "runtimeMode": "full-access",
            "interactionMode": "default",
            "workspaceStrategy": { "type": "root" },
            "initialMessage": { "text": "Add retries to the webhook dispatcher", "attachments": [] }
        })
    );
    // The draft became the thread, and follows it from a snapshot.
    view.read_with(cx, |view, _| {
        assert_eq!(view.thread_id(), Some(THREAD));
        assert!(view.thread_state().is_some());
    });
    assert_eq!(
        serde_json::to_value(&*backend.subscriptions.lock()).unwrap(),
        json!([{ "threadId": THREAD }])
    );
}

#[gpui::test]
async fn send_message_is_the_seam_for_send_to_agent(cx: &mut TestAppContext) {
    let backend = Arc::new(FakeBackend::default());
    cx.update(|cx| crate::set_backend(backend.clone(), cx));
    cx.update(|cx| crate::send_message(THREAD, "Please address these comments", cx))
        .await
        .unwrap();
    let commands = backend.commands();
    assert_eq!(commands.len(), 1);
    assert_eq!(commands[0]["text"], "Please address these comments");
    assert_eq!(commands[0]["deliveryIntent"], "auto");
    assert_eq!(
        commands[0]["dispatchMode"],
        json!({ "type": "start_immediately" })
    );

    let empty = cx.update(|cx| crate::send_message(THREAD, " ", cx)).await;
    assert!(empty.is_err());
    assert_eq!(backend.commands().len(), 1);
}

#[gpui::test]
async fn open_activates_an_open_thread(cx: &mut TestAppContext) {
    init_test(cx);
    let backend = FakeBackend::with_thread(finished_thread(), 10);
    cx.update(|cx| crate::set_backend(backend, cx));
    let fs = project::FakeFs::new(cx.executor());
    let project = project::Project::test(fs, [], cx).await;
    let (multi_workspace, cx) = cx.add_window_view(|window, cx| {
        workspace::MultiWorkspace::test_new(project.clone(), window, cx)
    });
    let workspace =
        multi_workspace.read_with(cx, |multi_workspace, _| multi_workspace.workspace().clone());

    let first = workspace
        .update_in(cx, |workspace, window, cx| {
            ThreadView::open(THREAD.into(), "Retry".into(), workspace, window, cx)
        })
        .unwrap();
    let second = workspace
        .update_in(cx, |workspace, window, cx| {
            ThreadView::open(THREAD.into(), "Retry".into(), workspace, window, cx)
        })
        .unwrap();
    cx.run_until_parked();
    assert_eq!(first.entity_id(), second.entity_id());
    workspace.read_with(cx, |workspace, cx| {
        assert_eq!(workspace.active_pane().read(cx).items_len(), 1);
    });
}

#[gpui::test]
async fn the_timeline_renders_and_its_buttons_send_commands(cx: &mut TestAppContext) {
    let backend = FakeBackend::with_thread(running_thread(), 20);
    let (_view, cx) = open_view(backend.clone(), Some(THREAD), cx);
    cx.simulate_resize(gpui::size(gpui::px(1000.), gpui::px(1200.)));
    cx.run_until_parked();

    for selector in [
        "thread-row-i1",
        "thread-work-i2",
        "thread-row-i5",
        "thread-row-i6",
        "thread-work-i7",
        "thread-row-i8",
        "thread-pending-request-1",
        "thread-queued-run-3",
        "thread-queue",
        "thread-steer",
        "thread-stop",
    ] {
        assert!(
            cx.debug_bounds(selector).is_some(),
            "{selector} isn't rendered"
        );
    }
    // While the agent works there's no plain Send.
    assert!(cx.debug_bounds("thread-send").is_none());
    // Work is collapsed by default.
    assert!(cx.debug_bounds("thread-work-item-i4").is_none());

    let approve = cx.debug_bounds("thread-respond-request-1-0").unwrap();
    cx.simulate_click(approve.center(), gpui::Modifiers::none());
    cx.run_until_parked();
    let commands = backend.commands();
    assert_eq!(commands.len(), 1);
    assert_eq!(commands[0]["type"], "runtime-request.respond");
    assert_eq!(commands[0]["requestId"], "request-1");
    assert_eq!(commands[0]["decision"], "accept");

    let header = cx.debug_bounds("thread-work-header-i2").unwrap();
    cx.simulate_click(header.center(), gpui::Modifiers::none());
    cx.run_until_parked();
    assert!(cx.debug_bounds("thread-work-item-i4").is_some());

    let stop = cx.debug_bounds("thread-stop").unwrap();
    cx.simulate_click(stop.center(), gpui::Modifiers::none());
    cx.run_until_parked();
    assert_eq!(backend.commands()[1]["type"], "thread.stop");
}

#[gpui::test]
async fn idle_threads_offer_send(cx: &mut TestAppContext) {
    let backend = FakeBackend::with_thread(finished_thread(), 10);
    let (_view, cx) = open_view(backend, Some(THREAD), cx);
    cx.simulate_resize(gpui::size(gpui::px(1000.), gpui::px(900.)));
    cx.run_until_parked();
    assert!(cx.debug_bounds("thread-send").is_some());
    assert!(cx.debug_bounds("thread-stop").is_none());
    assert!(cx.debug_bounds("thread-queue").is_none());
}

// RPCs without a method of their own on the backend.

#[gpui::test]
async fn typed_calls_and_streams_go_through_the_raw_ones(_cx: &mut TestAppContext) {
    let recording = Arc::new(FakeBackend::default());
    let backend: Arc<dyn ThreadBackend> = recording.clone();

    recording.raw_responses.lock().push(json!({
        "attachmentId": "attachment-1",
        "expiresAt": 1761566400000_i64,
        "relativeUrl": "/api/attachments/upload/token.signature"
    }));
    let minted = backend
        .call::<rpc::AttachmentsCreateUploadUrl>(
            serde_json::from_value(json!({
                "type": "image", "name": "screenshot.png", "mimeType": "image/png", "sizeBytes": 9
            }))
            .unwrap(),
            Access::Operate,
        )
        .await
        .unwrap();
    assert_eq!(
        minted.relative_url,
        "/api/attachments/upload/token.signature"
    );
    assert_eq!(
        *recording.raw_calls.lock(),
        [RawCall {
            tag: "attachments.createUploadUrl".into(),
            payload: json!({
                "type": "image", "name": "screenshot.png", "mimeType": "image/png", "sizeBytes": 9
            }),
            access: Access::Operate,
        }]
    );

    // Streams decode each item with the method's types.
    recording.raw_stream_items.lock().push(json!({
        "kind": "thread.removed", "sequence": 9, "location": "active", "threadId": THREAD
    }));
    let shell_input = || OrchestrationV2SubscribeShellInput {
        after_sequence: Some(8),
        request_completion_marker: None,
    };
    let items = backend
        .stream::<rpc::OrchestrationSubscribeShell>(shell_input(), Access::Read)
        .unwrap()
        .collect::<Vec<_>>()
        .await;
    assert!(matches!(
        items.as_slice(),
        [Ok(OrchestrationV2ShellStreamItem::ThreadRemoved(removed))] if removed.thread_id == THREAD
    ));
    assert_eq!(
        recording.raw_calls.lock()[1],
        RawCall {
            tag: "orchestration.subscribeShell".into(),
            payload: json!({ "afterSequence": 8 }),
            access: Access::Read,
        }
    );

    // The tables say which way a method is called.
    assert!(
        backend
            .call::<rpc::OrchestrationSubscribeShell>(shell_input(), Access::Read)
            .await
            .is_err()
    );
    assert!(
        backend
            .stream::<rpc::ServerGetConfig>(
                ServerGetConfigPayload(Default::default()),
                Access::Read
            )
            .is_err()
    );
    assert_eq!(recording.raw_calls.lock().len(), 2);
}

#[gpui::test]
async fn a_read_only_backend_refuses_raw_writes_and_uploads(_cx: &mut TestAppContext) {
    let recording = Arc::new(FakeBackend::default());
    let backend = ReadOnlyBackend::new(recording.clone(), "MUSTR_DRY_RUN_WRITES is set");

    // Reads go through.
    backend
        .call_raw("server.getSettings", json!({}), Access::Read)
        .await
        .unwrap();
    let _status = backend
        .subscribe_raw("subscribeVcsStatus", json!({}), Access::Read)
        .unwrap();

    let progress = Arc::new(Mutex::new(Vec::new()));
    let refused = [
        backend
            .call_raw(
                "shell.openInEditor",
                json!({ "cwd": "/tmp/worktrees/retry-backoff", "editor": "zed" }),
                Access::Operate,
            )
            .await,
        backend
            .call_raw(
                "vcs.switchRef",
                json!({ "cwd": "/tmp/worktrees/retry-backoff", "ref": "main" }),
                Access::SourceControlWrite,
            )
            .await,
        backend
            .subscribe_raw(
                "git.runStackedAction",
                json!({}),
                Access::SourceControlWrite,
            )
            .map(|_| Value::Null),
        backend
            .upload(AttachmentUpload {
                relative_url: "/api/attachments/upload/token.signature".into(),
                mime_type: "image/png".into(),
                bytes: b"png bytes".to_vec(),
                on_progress: Some(Arc::new({
                    let progress = progress.clone();
                    move |sent| progress.lock().push(sent)
                })),
            })
            .await
            .map(|()| Value::Null),
    ];
    for result in refused {
        assert_eq!(
            result.unwrap_err().to_string(),
            "Not sent: MUSTR_DRY_RUN_WRITES is set."
        );
    }
    assert_eq!(
        recording
            .raw_calls
            .lock()
            .iter()
            .map(|call| call.tag.as_str())
            .collect::<Vec<_>>(),
        ["server.getSettings", "subscribeVcsStatus"]
    );
    assert!(recording.uploads.lock().is_empty());
    assert!(progress.lock().is_empty());

    assert_eq!(backend.blocked_reason(Access::Read), None);
    for access in [Access::Operate, Access::SourceControlWrite] {
        assert_eq!(
            backend.blocked_reason(access).as_deref(),
            Some("MUSTR_DRY_RUN_WRITES is set")
        );
    }
}

// Actions between packages.

#[gpui::test]
fn cross_package_actions_build_by_name(cx: &mut TestAppContext) {
    cx.update(|cx| {
        for (name, fields) in [
            (
                "thread_chat::OpenThreadTerminal",
                json!({ "thread_id": THREAD }),
            ),
            (
                "thread_chat::ReviewThreadChanges",
                json!({ "thread_id": THREAD }),
            ),
            ("thread_chat::OpenInEditor", json!({ "thread_id": THREAD })),
            (
                "thread_chat::RemoveThreadWorktree",
                json!({ "thread_id": THREAD }),
            ),
            (
                "thread_chat::NewThreadInWorktree",
                json!({
                    "project_id": "project-1",
                    "worktree_path": "/tmp/worktrees/retry-backoff",
                    "branch": "t3/retry-backoff"
                }),
            ),
            (
                "thread_chat::NewThreadInWorktree",
                json!({ "project_id": "project-1" }),
            ),
        ] {
            let action = cx
                .build_action(name, Some(fields))
                .unwrap_or_else(|error| panic!("{name}: {error}"));
            assert_eq!(action.name(), name);
        }
        let action = cx
            .build_action(
                "thread_chat::NewThreadInWorktree",
                Some(json!({ "project_id": "project-1", "branch": "main" })),
            )
            .unwrap();
        assert_eq!(
            action.as_any().downcast_ref::<crate::NewThreadInWorktree>(),
            Some(&crate::NewThreadInWorktree {
                project_id: "project-1".into(),
                worktree_path: None,
                branch: Some("main".into()),
            })
        );
    });
}

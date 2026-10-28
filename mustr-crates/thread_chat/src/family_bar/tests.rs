//! The family bar against a recording backend: the family it works out from T3's shell and
//! projection, and the exact payloads its Open, Stop, and Script buttons send.

use super::{Activity, BackgroundWork, Family, Relationship, StopTarget, family};
use crate::{
    backend::{Access, AttachmentUpload, ReadOnlyBackend, ThreadBackend},
    shell::Shell,
    thread_view::ThreadView,
};
use anyhow::{Result, anyhow};
use collections::HashMap;
use futures::{FutureExt as _, StreamExt as _, future::BoxFuture, stream::BoxStream};
use gpui::{Entity, TestAppContext, VisualTestContext};
use parking_lot::Mutex;
use pretty_assertions::assert_eq;
use serde_json::{Value, json};
use settings::SettingsStore;
use std::sync::Arc;
use t3_api::{
    tagged::{
        OrchestrationV2Command, OrchestrationV2ShellStreamItem, OrchestrationV2ThreadStreamItem,
    },
    types::{
        OrchestrationV2AppThread, OrchestrationV2DispatchCommandResult,
        OrchestrationV2GetTurnItemInput, OrchestrationV2GetTurnItemResult,
        OrchestrationV2ShellSnapshot, OrchestrationV2Subagent, OrchestrationV2SubscribeShellInput,
        OrchestrationV2SubscribeThreadInput, OrchestrationV2ThreadDetailSnapshot,
        OrchestrationV2ThreadLaunchInput, OrchestrationV2ThreadLaunchResult, ServerProvider,
    },
};

const NOW: &str = "2026-10-27T12:00:00.000Z";
const PARENT: &str = "parent-1";
const SCRIPT_PATH: &str = "/home/dev/.claude/workflows/review-pipeline.js";

// Fixtures: T3's wire JSON, with every field the contracts require.

fn lineage(id: &str, parent: Option<&str>, relationship: Option<&str>) -> Value {
    json!({
        "rootThreadId": parent.unwrap_or(id),
        "parentThreadId": parent,
        "relationshipToParent": relationship
    })
}

fn shell_thread(
    id: &str,
    title: &str,
    status: &str,
    created_at: &str,
    lineage: Value,
    provider: &str,
) -> Value {
    json!({
        "id": id, "projectId": "project-1", "title": title, "status": status,
        "createdAt": created_at, "updatedAt": created_at, "createdBy": "user",
        "creationSource": "web", "interactionMode": "default", "runtimeMode": "full-access",
        "modelSelection": { "instanceId": provider, "model": "claude-sonnet-4-5" },
        "providerInstanceId": provider, "activeProviderThreadId": null, "activeRunId": null,
        "archivedAt": null, "branch": null, "deletedAt": null, "forkedFrom": null,
        "hasActionableProposedPlan": false, "itemCount": 3, "latestRunId": "run-x",
        "latestUserMessageAt": null, "latestVisibleMessage": null, "lineage": lineage,
        "pendingRuntimeRequest": null, "settledAt": null, "settledOverride": null,
        "visibleItemCount": 3, "worktreePath": null
    })
}

fn app_thread(id: &str, title: &str, lineage: Value) -> Value {
    json!({
        "id": id,
        "projectId": "project-1",
        "title": title,
        "createdAt": NOW,
        "updatedAt": NOW,
        "createdBy": "user",
        "creationSource": "web",
        "interactionMode": "default",
        "runtimeMode": "full-access",
        "modelSelection": { "instanceId": "claudeAgent", "model": "claude-opus-4-1" },
        "providerInstanceId": "claudeAgent",
        "activeProviderThreadId": null,
        "archivedAt": null,
        "deletedAt": null,
        "forkedFrom": null,
        "branch": null,
        "worktreePath": null,
        "lineage": lineage
    })
}

struct Subagent<'a> {
    id: &'a str,
    origin: &'a str,
    driver: &'a str,
    status: &'a str,
    child: Option<&'a str>,
    title: &'a str,
    native_id: Option<&'a str>,
}

fn subagent(subagent: Subagent) -> Value {
    json!({
        "id": subagent.id,
        "threadId": PARENT,
        "runId": "run-1",
        "parentNodeId": "node-1",
        "origin": subagent.origin,
        "driver": subagent.driver,
        "providerInstanceId": subagent.driver,
        "providerThreadId": null,
        "createdBy": "agent",
        "model": "claude-sonnet-4-5",
        "prompt": format!("Do the {} part", subagent.title),
        "title": subagent.title,
        "status": subagent.status,
        "result": null,
        "childThreadId": subagent.child,
        "nativeTaskRef": subagent.native_id.map(|native_id| json!({
            "driver": subagent.driver, "nativeId": native_id, "strength": "strong"
        })),
        "startedAt": NOW,
        "completedAt": null,
        "updatedAt": NOW
    })
}

fn workflow_subagent() -> Value {
    let mut value = subagent(Subagent {
        id: "sub-workflow",
        origin: "provider_native",
        driver: "claudeAgent",
        status: "running",
        child: Some("workflow-thread"),
        title: "Review the branch",
        native_id: Some("toolu_wf"),
    });
    value["workflow"] = json!({
        "name": "review-pipeline",
        "phases": [{ "index": 0, "title": "Scan" }, { "index": 1, "title": "Fix" }],
        "agents": [
            { "index": 0, "label": "security", "state": "completed", "phaseIndex": 0,
              "childThreadId": "member-security" },
            { "index": 1, "label": "style", "state": "running", "phaseIndex": 0 },
            { "index": 2, "label": "fixer", "state": "queued", "phaseIndex": 1 }
        ],
        "runHandles": { "scriptPath": SCRIPT_PATH }
    });
    value
}

fn parent_subagents() -> Vec<Value> {
    vec![
        subagent(Subagent {
            id: "sub-native",
            origin: "provider_native",
            driver: "claudeAgent",
            status: "running",
            child: Some("child-native"),
            title: "Subagent: Explore the API",
            native_id: Some("toolu_1"),
        }),
        subagent(Subagent {
            id: "sub-delegated",
            origin: "app_owned",
            driver: "codex",
            status: "running",
            child: Some("child-delegated"),
            title: "Write the migration",
            native_id: None,
        }),
        subagent(Subagent {
            id: "sub-done",
            origin: "app_owned",
            driver: "codex",
            status: "completed",
            child: Some("child-done"),
            title: "Read the docs",
            native_id: None,
        }),
        subagent(Subagent {
            id: "sub-task",
            origin: "provider_native",
            driver: "claudeAgent",
            status: "running",
            child: None,
            title: "Grep for callers",
            native_id: Some("toolu_2"),
        }),
        workflow_subagent(),
    ]
}

fn shell_json() -> Value {
    let mut parent = shell_thread(
        PARENT,
        "Plan the M3 work",
        "running",
        NOW,
        lineage(PARENT, None, None),
        "claudeAgent",
    );
    parent["pendingBackgroundTasks"] = json!([
        { "kind": "command", "taskId": "task-1", "description": "npm run dev" }
    ]);
    let child = |id: &str, title: &str, status: &str, created_at: &str, provider: &str| {
        shell_thread(
            id,
            title,
            status,
            created_at,
            lineage(id, Some(PARENT), Some("subagent")),
            provider,
        )
    };
    let mut native = child(
        "child-native",
        "Subagent: Explore the API",
        "idle",
        "2026-10-27T11:00:00.000Z",
        "claudeAgent",
    );
    // A mirror of Claude's own subagent never runs on its own.
    native["latestRunId"] = Value::Null;
    let mut fork = shell_thread(
        "fork-1",
        "Try another approach",
        "completed",
        "2026-10-27T09:00:00.000Z",
        lineage("fork-1", Some(PARENT), Some("fork")),
        "claudeAgent",
    );
    fork["forkedFrom"] = json!({ "type": "run", "threadId": PARENT, "runId": "run-1" });
    json!({
        "schemaVersion": 1,
        "snapshotSequence": 7,
        "threads": [
            parent,
            native,
            child("child-delegated", "Write the migration", "running",
                "2026-10-27T11:30:00.000Z", "codex"),
            child("child-done", "Read the docs", "completed",
                "2026-10-27T10:00:00.000Z", "codex"),
            child("workflow-thread", "Review the branch", "running",
                "2026-10-27T11:45:00.000Z", "claudeAgent"),
            child("member-security", "security", "completed",
                "2026-10-27T11:50:00.000Z", "claudeAgent"),
            fork,
            shell_thread("unrelated", "Something else", "running", NOW,
                lineage("unrelated", None, None), "codex"),
        ],
        "archivedThreads": [],
        "projects": []
    })
}

fn projection(thread: Value, subagents: Vec<Value>) -> Value {
    json!({
        "thread": thread,
        "attempts": [], "checkpointScopes": [], "checkpoints": [], "contextHandoffs": [],
        "contextTransfers": [], "messages": [], "nodes": [], "plans": [],
        "providerSessions": [], "providerThreads": [], "providerTurns": [], "runs": [],
        "runtimeRequests": [], "subagents": subagents, "turnItems": [],
        "visibleTurnItems": [], "updatedAt": NOW
    })
}

fn parent_projection() -> Value {
    projection(
        app_thread(PARENT, "Plan the M3 work", lineage(PARENT, None, None)),
        parent_subagents(),
    )
}

fn child_projection() -> Value {
    projection(
        app_thread(
            "child-delegated",
            "Write the migration",
            lineage("child-delegated", Some(PARENT), Some("subagent")),
        ),
        Vec::new(),
    )
}

fn decode<T: serde::de::DeserializeOwned>(value: Value) -> T {
    serde_json::from_value(value).expect("the fixture decodes")
}

fn shell() -> Shell {
    Shell::from_snapshot(&decode::<OrchestrationV2ShellSnapshot>(shell_json()))
}

fn parent_family() -> Family {
    let thread: OrchestrationV2AppThread = decode(parent_projection()["thread"].clone());
    let subagents: Vec<OrchestrationV2Subagent> = decode(json!(parent_subagents()));
    Family::derive(&thread, Some(&shell()), &subagents)
}

// The family.

#[test]
fn a_parent_lists_its_children_running_first() {
    let family = parent_family();
    assert_eq!(family.parent, None);
    let children = family
        .children
        .iter()
        .map(|child| {
            (
                child.thread_id.as_str(),
                child.title.as_str(),
                child.relationship,
                child.activity.label(),
                child.available,
            )
        })
        .collect::<Vec<_>>();
    assert_eq!(
        children,
        [
            // Running, newest first. The native child goes by its subagent record, and loses
            // the `Subagent: ` prefix.
            (
                "child-delegated",
                "Write the migration",
                Relationship::Subagent,
                "running",
                true
            ),
            (
                "child-native",
                "Explore the API",
                Relationship::Subagent,
                "running",
                true
            ),
            (
                "child-done",
                "Read the docs",
                Relationship::Subagent,
                "completed",
                true
            ),
            (
                "fork-1",
                "Try another approach",
                Relationship::Fork,
                "completed",
                true
            ),
        ]
    );
    assert_eq!(family.working_count(), 4);
    assert!(!family.waiting_on_thread);
}

#[test]
fn children_stop_the_way_t3_can_stop_them() {
    let family = parent_family();
    let stop = |id: &str| {
        family
            .children
            .iter()
            .find(|child| child.thread_id == id)
            .unwrap()
            .stop
            .clone()
    };
    // Claude's own subagent is interrupted in the parent's run.
    assert_eq!(
        stop("child-native"),
        Some(StopTarget::NativeSubagent {
            thread_id: PARENT.into(),
            run_id: "run-1".into(),
            subagent_id: "sub-native".into(),
        })
    );
    // 0.1.1's `subagent.stop` refuses delegated tasks, so their thread is stopped.
    assert_eq!(
        stop("child-delegated"),
        Some(StopTarget::Thread {
            thread_id: "child-delegated".into()
        })
    );
    assert_eq!(stop("child-done"), None);
    assert_eq!(stop("fork-1"), None);

    let payload = |target: StopTarget| serde_json::to_value(target.command("command-1".into()));
    assert_eq!(
        payload(stop("child-native").unwrap()).unwrap(),
        json!({
            "type": "subagent.stop",
            "commandId": "command-1",
            "threadId": PARENT,
            "runId": "run-1",
            "subagentId": "sub-native"
        })
    );
    assert_eq!(
        payload(stop("child-delegated").unwrap()).unwrap(),
        json!({ "type": "thread.stop", "commandId": "command-1", "threadId": "child-delegated" })
    );
}

#[test]
fn subagents_without_a_thread_and_workflows_get_their_own_rows() {
    let family = parent_family();
    assert_eq!(family.tasks.len(), 1);
    assert_eq!(family.tasks[0].title, "Grep for callers");
    assert_eq!(
        family.tasks[0].stop,
        Some(StopTarget::NativeSubagent {
            thread_id: PARENT.into(),
            run_id: "run-1".into(),
            subagent_id: "sub-task".into(),
        })
    );

    // The coordinator's thread is the workflow's row, not a child of its own.
    assert_eq!(family.workflows.len(), 1);
    let workflow = &family.workflows[0];
    assert_eq!(workflow.name, "review-pipeline");
    assert_eq!(workflow.subagent_id, "sub-workflow");
    assert_eq!(workflow.thread_id.as_deref(), Some("workflow-thread"));
    assert!(workflow.available);
    assert!(workflow.can_stop);
    assert_eq!(workflow.script_path.as_deref(), Some(SCRIPT_PATH));
    assert_eq!(workflow.summary(), "3 agents · 2 working · 1 done");
    let phases = workflow
        .phases
        .iter()
        .map(|phase| {
            (
                phase.title.clone().unwrap(),
                phase
                    .members
                    .iter()
                    .map(|member| {
                        (
                            member.label.as_str(),
                            member.activity.label(),
                            member.available,
                        )
                    })
                    .collect::<Vec<_>>(),
                phase.settled(),
            )
        })
        .collect::<Vec<_>>();
    assert_eq!(
        phases,
        [
            (
                "Scan".to_string(),
                vec![("security", "completed", true), ("style", "running", false)],
                1
            ),
            ("Fix".to_string(), vec![("fixer", "pending", false)], 0),
        ]
    );

    assert_eq!(
        family.background,
        Some(BackgroundWork {
            title: "Running: npm run dev".into(),
            threads: Vec::new(),
        })
    );
}

#[test]
fn a_finished_workflow_ends_its_agents_and_cant_be_stopped() {
    let thread: OrchestrationV2AppThread = decode(parent_projection()["thread"].clone());
    let mut workflow = workflow_subagent();
    workflow["status"] = json!("cancelled");
    let subagents: Vec<OrchestrationV2Subagent> = decode(json!([workflow]));
    let family = Family::derive(&thread, Some(&shell()), &subagents);
    let workflow = &family.workflows[0];
    assert!(!workflow.can_stop);
    assert_eq!(workflow.summary(), "3 agents · 1 done · 2 stopped");
}

#[test]
fn a_child_shows_its_parent() {
    let thread: OrchestrationV2AppThread = decode(child_projection()["thread"].clone());
    let family = Family::derive(&thread, Some(&shell()), &[]);
    let parent = family.parent.expect("the child has a parent");
    assert_eq!(parent.thread_id, PARENT);
    assert_eq!(parent.title, "Plan the M3 work");
    assert_eq!(parent.relationship, Relationship::Subagent);
    assert_eq!(parent.activity, Activity::Working("running".into()));
    assert!(parent.available);
    assert!(family.children.is_empty());

    // Without the shell, the parent is still named, but can't be opened.
    let family = Family::derive(&thread, None, &[]);
    let parent = family.parent.unwrap();
    assert_eq!(parent.title, "Parent thread");
    assert!(!parent.available);
}

#[test]
fn threads_without_relatives_have_no_family() {
    let thread: OrchestrationV2AppThread = decode(app_thread(
        "unrelated",
        "Something else",
        lineage("unrelated", None, None),
    ));
    let family = Family::derive(&thread, Some(&shell()), &[]);
    assert!(family.is_empty());
}

#[test]
fn background_work_reads_like_t3s() {
    let titles = |tasks: Value| {
        let mut thread = shell_thread(
            PARENT,
            "Plan",
            "running",
            NOW,
            lineage(PARENT, None, None),
            "claudeAgent",
        );
        thread["pendingBackgroundTasks"] = tasks;
        thread["blockedOnThreadWait"] = json!(true);
        let mut snapshot = shell_json();
        snapshot["threads"] = json!([thread]);
        let shell = Shell::from_snapshot(&decode(snapshot));
        let app: OrchestrationV2AppThread =
            decode(app_thread(PARENT, "Plan", lineage(PARENT, None, None)));
        let family = Family::derive(&app, Some(&shell), &[]);
        assert!(family.waiting_on_thread);
        family.background.map(|work| (work.title, work.threads))
    };
    assert_eq!(titles(json!([])), None);
    assert_eq!(
        titles(json!([{ "kind": "monitor", "taskId": "t1" }])),
        Some(("Running a monitor".into(), vec![]))
    );
    assert_eq!(
        titles(json!([
            { "kind": "command", "taskId": "t1" },
            { "kind": "subagent", "taskId": "t2", "description": "Subagent: Explore",
              "childThreadId": "child-native" },
            { "kind": "command", "taskId": "t3" }
        ])),
        Some((
            "Running 1 subagent and 2 commands".into(),
            vec![("Explore".into(), "child-native".into())]
        ))
    );
}

#[test]
fn subagent_titles_drop_t3s_prefixes() {
    assert_eq!(family::provider_name("claudeAgent"), "Claude");
    assert_eq!(family::provider_name("codex-work"), "codex-work");
    let thread: OrchestrationV2AppThread = decode(child_projection()["thread"].clone());
    let mut codex = subagent(Subagent {
        id: "sub-codex",
        origin: "provider_native",
        driver: "codex",
        status: "running",
        child: None,
        title: "/root/review_agent",
        native_id: None,
    });
    let subagents: Vec<OrchestrationV2Subagent> = decode(json!([codex.clone()]));
    let family = Family::derive(&thread, None, &subagents);
    assert_eq!(family.tasks[0].title, "Review Agent");
    // Codex's subagents can't be stopped on their own.
    assert_eq!(family.tasks[0].stop, None);

    codex["title"] = Value::Null;
    codex["prompt"] = json!("x".repeat(100));
    let subagents: Vec<OrchestrationV2Subagent> = decode(json!([codex]));
    let family = Family::derive(&thread, None, &subagents);
    assert_eq!(family.tasks[0].title, format!("{}…", "x".repeat(77)));
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
    projections: HashMap<String, Value>,
    commands: Mutex<Vec<OrchestrationV2Command>>,
    raw_calls: Mutex<Vec<RawCall>>,
    raw_responses: Mutex<Vec<Value>>,
}

impl FakeBackend {
    fn new() -> Arc<Self> {
        let mut backend = Self::default();
        backend
            .projections
            .insert(PARENT.into(), parent_projection());
        backend
            .projections
            .insert("child-delegated".into(), child_projection());
        Arc::new(backend)
    }

    fn commands(&self) -> Vec<Value> {
        self.commands
            .lock()
            .iter()
            .map(|command| serde_json::to_value(command).unwrap())
            .collect()
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
        thread_id: String,
    ) -> BoxFuture<'static, Result<OrchestrationV2ThreadDetailSnapshot>> {
        let projection = self.projections.get(&thread_id).cloned();
        ready(
            projection
                .ok_or_else(|| anyhow!("no thread {thread_id}"))
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
        Ok(futures::stream::pending().boxed())
    }

    fn turn_item(
        &self,
        _input: OrchestrationV2GetTurnItemInput,
    ) -> BoxFuture<'static, Result<OrchestrationV2GetTurnItemResult>> {
        ready(Err(anyhow!("no turn items here")))
    }

    fn providers(&self) -> BoxFuture<'static, Result<Vec<ServerProvider>>> {
        ready(Ok(Vec::new()))
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
        _input: OrchestrationV2ThreadLaunchInput,
    ) -> BoxFuture<'static, Result<OrchestrationV2ThreadLaunchResult>> {
        ready(Err(anyhow!("the family bar never launches threads")))
    }

    fn call_raw(
        &self,
        tag: &str,
        payload: Value,
        access: Access,
    ) -> BoxFuture<'static, Result<Value>> {
        self.raw_calls.lock().push(RawCall {
            tag: tag.to_string(),
            payload,
            access,
        });
        let mut responses = self.raw_responses.lock();
        ready(Ok(if responses.is_empty() {
            Value::Null
        } else {
            responses.remove(0)
        }))
    }

    fn subscribe_raw(
        &self,
        _tag: &str,
        _payload: Value,
        _access: Access,
    ) -> Result<BoxStream<'static, Result<Value>>> {
        Ok(futures::stream::empty().boxed())
    }

    fn upload(&self, _upload: AttachmentUpload) -> BoxFuture<'static, Result<()>> {
        ready(Err(anyhow!("the family bar never uploads")))
    }
}

// The bar.

fn init_test(cx: &mut TestAppContext) {
    cx.update(|cx| {
        let settings_store = SettingsStore::test(cx);
        cx.set_global(settings_store);
        theme_settings::init(theme::LoadThemes::JustBase, cx);
    });
}

/// Opens `thread_id` in a workspace, so the bar's Open buttons have one to open threads in.
async fn open_thread<'a>(
    backend: Arc<dyn ThreadBackend>,
    thread_id: &str,
    cx: &'a mut TestAppContext,
) -> (
    Entity<workspace::Workspace>,
    Entity<ThreadView>,
    &'a mut VisualTestContext,
) {
    init_test(cx);
    cx.update(|cx| crate::set_backend(backend, cx));
    let fs = project::FakeFs::new(cx.executor());
    let project = project::Project::test(fs, [], cx).await;
    let (multi_workspace, cx) = cx.add_window_view(|window, cx| {
        workspace::MultiWorkspace::test_new(project.clone(), window, cx)
    });
    let workspace =
        multi_workspace.read_with(cx, |multi_workspace, _| multi_workspace.workspace().clone());
    let view = workspace
        .update_in(cx, |workspace, window, cx| {
            ThreadView::open(thread_id.into(), "Loading".into(), workspace, window, cx)
        })
        .expect("a backend is installed");
    cx.simulate_resize(gpui::size(gpui::px(1200.), gpui::px(900.)));
    cx.run_until_parked();
    (workspace, view, cx)
}

fn click(selector: &'static str, cx: &mut VisualTestContext) {
    let bounds = cx
        .debug_bounds(selector)
        .unwrap_or_else(|| panic!("{selector} isn't rendered"));
    cx.simulate_click(bounds.center(), gpui::Modifiers::none());
    cx.run_until_parked();
}

fn open_thread_ids(
    workspace: &Entity<workspace::Workspace>,
    cx: &VisualTestContext,
) -> Vec<String> {
    workspace.read_with(cx, |workspace, cx| {
        workspace
            .active_pane()
            .read(cx)
            .items_of_type::<ThreadView>()
            .filter_map(|view| view.read(cx).thread_id().map(str::to_string))
            .collect()
    })
}

#[gpui::test]
async fn a_child_opens_its_parent(cx: &mut TestAppContext) {
    let backend = FakeBackend::new();
    let (workspace, _view, cx) = open_thread(backend.clone(), "child-delegated", cx).await;
    click("family-open-parent", cx);
    assert_eq!(open_thread_ids(&workspace, cx), ["child-delegated", PARENT]);
    assert!(backend.commands().is_empty());
}

#[gpui::test]
async fn a_parent_opens_and_stops_its_children(cx: &mut TestAppContext) {
    let backend = FakeBackend::new();
    let (workspace, _view, cx) = open_thread(backend.clone(), PARENT, cx).await;

    // The running children are listed, the first one opens.
    click("family-child-child-delegated", cx);
    assert_eq!(open_thread_ids(&workspace, cx), [PARENT, "child-delegated"]);

    workspace.update_in(cx, |workspace, window, cx| {
        ThreadView::open(PARENT.into(), "Plan".into(), workspace, window, cx);
    });
    cx.run_until_parked();
    click("family-stop-child-native", cx);
    click("family-stop-child-delegated", cx);
    click("family-stop-task-sub-task", cx);
    let commands = backend
        .commands()
        .into_iter()
        .map(|mut command| {
            // Fresh for every command.
            assert!(
                command["commandId"]
                    .as_str()
                    .is_some_and(|id| !id.is_empty())
            );
            command.as_object_mut().unwrap().remove("commandId");
            command
        })
        .collect::<Vec<_>>();
    assert_eq!(
        commands,
        [
            json!({ "type": "subagent.stop", "threadId": PARENT, "runId": "run-1",
                    "subagentId": "sub-native" }),
            json!({ "type": "thread.stop", "threadId": "child-delegated" }),
            json!({ "type": "subagent.stop", "threadId": PARENT, "runId": "run-1",
                    "subagentId": "sub-task" }),
        ]
    );
    assert!(backend.raw_calls.lock().is_empty());
}

#[gpui::test]
async fn workflows_show_their_script_and_stop(cx: &mut TestAppContext) {
    let backend = FakeBackend::new();
    backend.raw_responses.lock().push(json!({
        "scriptPath": SCRIPT_PATH,
        "contents": "export const meta = { name: \"review-pipeline\" };\nawait phase(\"Scan\");",
        "truncated": false
    }));
    let (_workspace, _view, cx) = open_thread(backend.clone(), PARENT, cx).await;

    click("family-workflow-script-sub-workflow", cx);
    click("family-workflow-stop-sub-workflow", cx);
    assert_eq!(
        backend.raw_calls.lock().clone(),
        [
            RawCall {
                tag: "orchestration.getWorkflowScript".into(),
                payload: json!({ "threadId": PARENT, "scriptPath": SCRIPT_PATH }),
                access: Access::Read,
            },
            RawCall {
                tag: "orchestration.stopWorkflow".into(),
                payload: json!({ "threadId": PARENT, "subagentId": "sub-workflow" }),
                access: Access::Operate,
            },
        ]
    );
    assert!(cx.debug_bounds("family-workflow-script-loaded").is_some());
    assert!(cx.debug_bounds("family-error").is_none());

    // Its agents are listed on demand, and those with a thread open.
    assert!(cx.debug_bounds("family-workflow-member-security").is_none());
    click("family-workflow-toggle-sub-workflow", cx);
    assert!(cx.debug_bounds("family-workflow-member-style").is_some());
    click("family-workflow-member-security", cx);
    assert!(backend.commands().is_empty());
}

#[gpui::test]
async fn background_work_stops_the_thread(cx: &mut TestAppContext) {
    let backend = FakeBackend::new();
    let (_workspace, _view, cx) = open_thread(backend.clone(), PARENT, cx).await;
    click("family-background-stop", cx);
    let commands = backend.commands();
    assert_eq!(commands.len(), 1);
    assert_eq!(commands[0]["type"], "thread.stop");
    assert_eq!(commands[0]["threadId"], PARENT);
}

#[gpui::test]
async fn read_only_backends_disable_stop(cx: &mut TestAppContext) {
    let backend = FakeBackend::new();
    let read_only = Arc::new(ReadOnlyBackend::new(
        backend.clone(),
        "writes are off for a dry run",
    ));
    let (workspace, _view, cx) = open_thread(read_only, PARENT, cx).await;
    click("family-stop-child-native", cx);
    click("family-workflow-stop-sub-workflow", cx);
    click("family-background-stop", cx);
    assert!(backend.commands().is_empty());
    assert!(backend.raw_calls.lock().is_empty());
    // Reads and opening still work.
    click("family-child-child-done", cx);
    assert_eq!(open_thread_ids(&workspace, cx), [PARENT, "child-done"]);
}

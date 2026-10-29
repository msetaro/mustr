//! Tests of the threads sidebar's sections and of organizing threads (`sidebar.rs`,
//! `organize.rs`), against a recording backend: every command is captured as the payload T3
//! would get, and the tests assert those payloads exactly.

use crate::{
    actions::RemoveThreadWorktree,
    backend::{Access, AttachmentUpload, ThreadBackend},
    organize::{
        ArchiveThread, MarkThreadUnread, PinThread, RenameThread, RenameThreadModal, SettleThread,
        UnsettleThread,
    },
    shell::{Shell, ThreadShells},
    sidebar::{Section, ShellStatus, ThreadList, ThreadListFilter, ThreadsSidebar},
    thread_view::ThreadView,
};
use anyhow::{Result, anyhow};
use collections::{HashMap, HashSet};
use futures::{
    FutureExt as _, StreamExt as _, channel::mpsc, future::BoxFuture, stream::BoxStream,
};
use gpui::{Entity, Focusable as _, TestAppContext, VisualContext as _, VisualTestContext};
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
use time::{OffsetDateTime, macros::datetime};
use workspace::{MultiWorkspace, Workspace};

const NOW: OffsetDateTime = datetime!(2026-10-27 12:00 UTC);

/// A thread as T3's shell sends it, with every field the contracts require, plus `fields`.
fn thread(id: &str, title: &str, fields: Value) -> Value {
    let mut thread = json!({
        "id": id, "projectId": "project-1", "title": title, "status": "completed",
        "createdAt": "2026-10-27T09:00:00.000Z", "updatedAt": "2026-10-27T09:30:00.000Z",
        "createdBy": "user", "creationSource": "web", "interactionMode": "default",
        "runtimeMode": "full-access",
        "modelSelection": { "instanceId": "claudeAgent", "model": "claude-sonnet-4-5" },
        "providerInstanceId": "claudeAgent", "activeProviderThreadId": null, "activeRunId": null,
        "archivedAt": null, "branch": null, "deletedAt": null, "forkedFrom": null,
        "hasActionableProposedPlan": false, "itemCount": 3, "latestRunId": null,
        "latestUserMessageAt": null, "latestVisibleMessage": null,
        "lineage": { "rootThreadId": id, "parentThreadId": null, "relationshipToParent": null },
        "pendingRuntimeRequest": null, "settledAt": null, "settledOverride": null,
        "visibleItemCount": 3, "worktreePath": null
    });
    if let (Value::Object(thread), Value::Object(fields)) = (&mut thread, fields) {
        thread.extend(fields);
    }
    thread
}

fn project(id: &str, title: &str) -> Value {
    json!({
        "id": id, "title": title, "workspaceRoot": format!("/src/{title}"), "scripts": [],
        "createdAt": "2026-10-01T00:00:00.000Z", "updatedAt": "2026-10-01T00:00:00.000Z",
        "defaultModelSelection": null
    })
}

fn snapshot(threads: Vec<Value>) -> Value {
    json!({
        "schemaVersion": 1,
        "snapshotSequence": 7,
        "threads": threads,
        "archivedThreads": [],
        "projects": [project("project-1", "api"), project("project-2", "web")]
    })
}

fn shell(threads: Vec<Value>) -> Shell {
    let snapshot: OrchestrationV2ShellSnapshot =
        serde_json::from_value(snapshot(threads)).expect("the shell fixture decodes");
    Shell::from_snapshot(&snapshot)
}

fn ids(threads: &[crate::sidebar::ThreadEntry]) -> Vec<&str> {
    threads.iter().map(|thread| thread.id.as_str()).collect()
}

/// A shell with a thread in every section, and some that have no row.
fn every_section() -> Vec<Value> {
    vec![
        thread(
            "new",
            "Newest",
            json!({ "createdAt": "2026-10-27T11:00:00.000Z" }),
        ),
        thread(
            "older",
            "Older",
            json!({ "createdAt": "2026-10-26T11:00:00.000Z" }),
        ),
        // Un-settling counts as new.
        thread(
            "unsettled",
            "Back again",
            json!({
                "createdAt": "2026-10-20T11:00:00.000Z",
                "unsettledAt": "2026-10-27T11:30:00.000Z",
                "settledOverride": "active"
            }),
        ),
        // Ordered by the user, after the rest.
        thread(
            "ordered",
            "Ordered",
            json!({ "createdAt": "2026-10-27T11:59:00.000Z", "activeOrderKey": "a0" }),
        ),
        thread(
            "pin-b",
            "Pinned second",
            json!({ "pinnedAt": NOW_ISO, "pinOrderKey": "b0" }),
        ),
        thread(
            "pin-a",
            "Pinned first",
            json!({ "pinnedAt": NOW_ISO, "pinOrderKey": "a0" }),
        ),
        thread(
            "snoozed",
            "Snoozed",
            json!({
                "snoozedAt": "2026-10-27T10:00:00.000Z",
                "snoozedUntil": "2026-10-27T15:00:00.000Z"
            }),
        ),
        // Its snooze is over.
        thread(
            "woke",
            "Woke up",
            json!({
                "createdAt": "2026-10-25T11:00:00.000Z",
                "snoozedAt": "2026-10-27T10:00:00.000Z",
                "snoozedUntil": "2026-10-27T11:00:00.000Z"
            }),
        ),
        // A request ends a snooze early.
        thread(
            "raised-hand",
            "Asks for approval",
            json!({
                "createdAt": "2026-10-24T11:00:00.000Z",
                "status": "running",
                "snoozedAt": "2026-10-27T10:00:00.000Z",
                "snoozedUntil": "2026-10-28T10:00:00.000Z",
                "pendingRuntimeRequest": { "id": "r1", "kind": "command", "createdAt": NOW_ISO }
            }),
        ),
        thread(
            "settled-old",
            "Settled long ago",
            json!({ "settledOverride": "settled", "settledAt": "2026-10-01T00:00:00.000Z" }),
        ),
        thread(
            "settled-new",
            "Settled today",
            json!({ "settledOverride": "settled", "settledAt": "2026-10-27T08:00:00.000Z" }),
        ),
        // Settled by T3 before settling was stamped: by its last activity.
        thread(
            "settled-unstamped",
            "Settled unstamped",
            json!({
                "settledOverride": "settled",
                "latestRunCompletedAt": "2026-10-20T00:00:00.000Z"
            }),
        ),
        thread("archived", "Archived", json!({ "archivedAt": NOW_ISO })),
        thread(
            "child",
            "A subagent",
            json!({
                "lineage": {
                    "rootThreadId": "new", "parentThreadId": "new",
                    "relationshipToParent": "subagent"
                }
            }),
        ),
        thread(
            "other-project",
            "Elsewhere",
            json!({ "projectId": "project-2", "createdAt": "2026-10-23T11:00:00.000Z" }),
        ),
    ]
}

const NOW_ISO: &str = "2026-10-27T12:00:00.000Z";

#[test]
fn threads_sort_into_t3s_sections() {
    let list = ThreadList::from_shell(&shell(every_section()), &Default::default(), NOW);
    assert_eq!(ids(&list.pinned), ["pin-a", "pin-b"]);
    assert_eq!(
        ids(&list.active),
        [
            "unsettled",
            "new",
            "older",
            "woke",
            "raised-hand",
            "other-project",
            "ordered"
        ]
    );
    assert_eq!(ids(&list.snoozed), ["snoozed"]);
    assert_eq!(
        ids(&list.settled),
        ["settled-new", "settled-unstamped", "settled-old"]
    );
    assert_eq!(list.len(), 13);
    let entry = list.find("other-project").unwrap();
    assert_eq!(entry.project.as_deref(), Some("web"));
    assert_eq!(entry.workspace_root.as_deref(), Some("/src/web"));
    assert_eq!(entry.section, Section::Active);

    // A project filter keeps that project's threads.
    let filter = ThreadListFilter {
        project_id: Some("project-2".into()),
    };
    let list = ThreadList::from_shell(&shell(every_section()), &filter, NOW);
    assert_eq!(ids(&list.active), ["other-project"]);
    assert_eq!(list.len(), 1);
}

#[test]
fn rows_show_t3s_status_pills() {
    let status = |fields: Value| {
        let list = ThreadList::from_shell(
            &shell(vec![thread("t", "T", fields)]),
            &Default::default(),
            NOW,
        );
        let entry = list.find("t").unwrap().clone();
        (entry.status, entry.unread)
    };
    assert_eq!(status(json!({})), (None, false));
    assert_eq!(
        status(json!({ "status": "running" })),
        (Some(ShellStatus::Working), false)
    );
    assert_eq!(
        status(json!({ "status": "starting" })),
        (Some(ShellStatus::Connecting), false)
    );
    assert_eq!(
        status(json!({ "status": "running", "blockedOnThreadWait": true })),
        (Some(ShellStatus::Waiting), false)
    );
    assert_eq!(
        status(json!({
            "status": "running",
            "pendingRuntimeRequest": { "id": "r", "kind": "user_input", "createdAt": NOW_ISO }
        })),
        (Some(ShellStatus::AwaitingInput), false)
    );
    assert_eq!(
        status(json!({
            "pendingRuntimeRequest": { "id": "r", "kind": "file-change", "createdAt": NOW_ISO }
        })),
        (Some(ShellStatus::PendingApproval), false)
    );
    // Finished since the last visit: unread.
    let finished = json!({
        "latestRunCompletedAt": "2026-10-27T11:00:00.000Z",
        "lastVisitedAt": "2026-10-27T10:00:00.000Z"
    });
    assert_eq!(status(finished), (Some(ShellStatus::Completed), true));
    assert_eq!(
        status(json!({
            "latestRunCompletedAt": "2026-10-27T11:00:00.000Z",
            "lastVisitedAt": "2026-10-27T11:30:00.000Z"
        })),
        (None, false)
    );
    // A failure counts until seen, even if the thread was never opened.
    assert_eq!(
        status(json!({ "status": "failed", "latestRunCompletedAt": "2026-10-27T11:00:00.000Z" })),
        (Some(ShellStatus::Failed), true)
    );
    assert_eq!(
        status(json!({ "interactionMode": "plan", "hasActionableProposedPlan": true })),
        (Some(ShellStatus::PlanReady), false)
    );
}

#[test]
fn settling_moves_to_the_next_open_thread() {
    let list = ThreadList::from_shell(&shell(every_section()), &Default::default(), NOW);
    // Pinned, then active, skipping what's snoozed or settled, and around the end.
    assert_eq!(list.next_after("pin-b").unwrap().id, "unsettled");
    assert_eq!(list.next_after("ordered").unwrap().id, "pin-a");
    assert_eq!(list.next_after("snoozed").unwrap().id, "pin-a");
    assert_eq!(list.next_after("settled-old").unwrap().id, "pin-a");
    assert!(list.next_after("child").is_none());
    let alone = ThreadList::from_shell(
        &shell(vec![thread("t", "T", json!({}))]),
        &Default::default(),
        NOW,
    );
    assert!(alone.next_after("t").is_none());
}

#[test]
fn search_finds_threads_by_title_branch_project_and_pull_request() {
    let mut threads = every_section();
    threads.push(thread(
        "pr",
        "Fix the flaky webhook test",
        json!({
            "branch": "t3/webhook-retry",
            "linkedPullRequest": {
                "number": 4182, "projectId": "project-1", "repository": "acme/api",
                "url": "https://github.com/acme/api/pull/4182"
            }
        }),
    ));
    let list = ThreadList::from_shell(&shell(threads), &Default::default(), NOW);
    let found = |query: &str| {
        list.search(query)
            .into_iter()
            .map(|thread| thread.id.clone())
            .collect::<Vec<_>>()
    };
    assert_eq!(found("FLAKY webhook"), ["pr"]);
    assert_eq!(found("webhook-retry"), ["pr"]);
    assert_eq!(found("#4182"), ["pr"]);
    // Every section, settled included.
    assert_eq!(found("settled today"), ["settled-new"]);
    assert_eq!(found("web elsewhere"), ["other-project"]);
    assert!(found("nothing like it").is_empty());
}

// A recording backend.

#[derive(Default)]
struct Recorder {
    shell: Mutex<Value>,
    updates: Mutex<Option<mpsc::UnboundedSender<Result<OrchestrationV2ShellStreamItem>>>>,
    commands: Mutex<Vec<OrchestrationV2Command>>,
    /// What happened, in order: commands by type, and other events the tests log.
    log: Arc<Mutex<Vec<String>>>,
    read_only: bool,
    /// Answers to RPCs, by tag: a result, or an error message. Other calls fail.
    answers: Mutex<HashMap<String, Result<Value, String>>>,
    /// RPCs this server doesn't have.
    unsupported: Mutex<HashSet<String>>,
    /// Every RPC called: its tag, payload and access level.
    calls: Mutex<Vec<(String, Value, Access)>>,
    /// RPCs whose answers wait for the test (`answer_held`).
    held: Mutex<HashSet<String>>,
    holds: Mutex<Vec<futures::channel::oneshot::Sender<Result<Value>>>>,
}

impl Recorder {
    fn new(threads: Vec<Value>) -> Arc<Self> {
        Arc::new(Self {
            shell: Mutex::new(snapshot(threads)),
            ..Default::default()
        })
    }

    fn commands(&self) -> Vec<Value> {
        self.commands
            .lock()
            .iter()
            .map(|command| serde_json::to_value(command).unwrap())
            .collect()
    }

    /// The commands without their fresh ids.
    fn commands_without_ids(&self) -> Vec<Value> {
        self.commands()
            .into_iter()
            .map(|mut command| {
                let id = command["commandId"].take();
                assert!(uuid::Uuid::parse_str(id.as_str().unwrap()).is_ok());
                command.as_object_mut().unwrap().remove("commandId");
                command
            })
            .collect()
    }

    fn push(&self, item: Value) {
        let item = serde_json::from_value(item).expect("the shell item decodes");
        self.updates
            .lock()
            .as_ref()
            .expect("the shell is followed")
            .unbounded_send(Ok(item))
            .unwrap();
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
        let (sender, receiver) = mpsc::unbounded();
        *self.updates.lock() = Some(sender);
        Ok(receiver.boxed())
    }

    fn thread_snapshot(
        &self,
        _: String,
    ) -> BoxFuture<'static, Result<OrchestrationV2ThreadDetailSnapshot>> {
        ready(Err(anyhow!("these tests don't load threads")))
    }

    fn subscribe_thread(
        &self,
        _: OrchestrationV2SubscribeThreadInput,
    ) -> Result<BoxStream<'static, Result<OrchestrationV2ThreadStreamItem>>> {
        Ok(futures::stream::pending().boxed())
    }

    fn turn_item(
        &self,
        _: OrchestrationV2GetTurnItemInput,
    ) -> BoxFuture<'static, Result<OrchestrationV2GetTurnItemResult>> {
        ready(Err(anyhow!("no turn items")))
    }

    fn providers(&self) -> BoxFuture<'static, Result<Vec<ServerProvider>>> {
        ready(Ok(Vec::new()))
    }

    fn dispatch(
        &self,
        command: OrchestrationV2Command,
    ) -> BoxFuture<'static, Result<OrchestrationV2DispatchCommandResult>> {
        if self.read_only {
            return ready(Err(anyhow!("Not sent: read only.")));
        }
        self.log
            .lock()
            .push(command.tag().unwrap_or_default().to_string());
        self.commands.lock().push(command);
        ready(Ok(OrchestrationV2DispatchCommandResult { sequence: 100 }))
    }

    fn launch_thread(
        &self,
        _: OrchestrationV2ThreadLaunchInput,
    ) -> BoxFuture<'static, Result<OrchestrationV2ThreadLaunchResult>> {
        ready(Err(anyhow!("these tests don't launch threads")))
    }

    fn call_raw(
        &self,
        tag: &str,
        payload: Value,
        access: Access,
    ) -> BoxFuture<'static, Result<Value>> {
        self.calls.lock().push((tag.to_string(), payload, access));
        if self.unsupported.lock().contains(tag) {
            return ready(Err(anyhow::Error::new(crate::UnsupportedRpc {
                tag: tag.to_string(),
            })));
        }
        if self.read_only && access.is_write() {
            return ready(Err(anyhow!("Not sent: read only.")));
        }
        if self.held.lock().contains(tag) {
            let (sender, receiver) = futures::channel::oneshot::channel();
            self.holds.lock().push(sender);
            return async move { receiver.await? }.boxed();
        }
        match self.answers.lock().get(tag) {
            Some(Ok(answer)) => ready(Ok(answer.clone())),
            Some(Err(error)) => ready(Err(anyhow!("{error}"))),
            None => ready(Err(anyhow!("unexpected call to {tag}"))),
        }
    }

    fn subscribe_raw(
        &self,
        tag: &str,
        _: Value,
        _: Access,
    ) -> Result<BoxStream<'static, Result<Value>>> {
        Err(anyhow!("unexpected stream {tag}"))
    }

    fn upload(&self, _: AttachmentUpload) -> BoxFuture<'static, Result<()>> {
        ready(Err(anyhow!("no uploads")))
    }

    fn write_blocked_reason(&self) -> Option<String> {
        self.read_only.then(|| "read only".to_string())
    }
}

fn init(backend: Arc<Recorder>, cx: &mut TestAppContext) {
    cx.update(|cx| {
        let settings_store = SettingsStore::test(cx);
        cx.set_global(settings_store);
        theme_settings::init(theme::LoadThemes::JustBase, cx);
        // For the pickers' query editors.
        editor::init(cx);
        crate::init(cx);
        cx.bind_keys(crate::key_bindings());
        crate::set_backend(backend, cx);
    });
}

async fn workspace(
    backend: Arc<Recorder>,
    cx: &mut TestAppContext,
) -> (Entity<Workspace>, &mut VisualTestContext) {
    init(backend, cx);
    let fs = project::FakeFs::new(cx.executor());
    let project = project::Project::test(fs, [], cx).await;
    let (multi_workspace, cx) =
        cx.add_window_view(|window, cx| MultiWorkspace::test_new(project.clone(), window, cx));
    let workspace =
        multi_workspace.read_with(cx, |multi_workspace, _| multi_workspace.workspace().clone());
    // The app follows the shell from the start.
    cx.update(|_, cx| ThreadShells::global(cx));
    cx.run_until_parked();
    (workspace, cx)
}

fn open(
    workspace: &Entity<Workspace>,
    thread_id: &str,
    cx: &mut VisualTestContext,
) -> Entity<ThreadView> {
    let view = workspace
        .update_in(cx, |workspace, window, cx| {
            ThreadView::open(thread_id.into(), thread_id.into(), workspace, window, cx)
        })
        .unwrap();
    cx.run_until_parked();
    view
}

fn active_thread(
    workspace: &Entity<Workspace>,
    cx: &mut VisualTestContext,
) -> Option<Option<String>> {
    workspace.read_with(cx, |workspace, cx| {
        let view = workspace.active_item(cx)?.downcast::<ThreadView>()?;
        Some(view.read(cx).thread_id().map(str::to_string))
    })
}

fn tab_count(workspace: &Entity<Workspace>, cx: &mut VisualTestContext) -> usize {
    workspace.read_with(cx, |workspace, cx| {
        workspace.active_pane().read(cx).items_len()
    })
}

#[gpui::test]
async fn settling_the_open_thread_moves_to_the_next(cx: &mut TestAppContext) {
    let backend = Recorder::new(vec![
        thread(
            "a",
            "First",
            json!({ "createdAt": "2026-10-27T11:00:00.000Z" }),
        ),
        thread(
            "b",
            "Second",
            json!({ "createdAt": "2026-10-27T10:00:00.000Z" }),
        ),
        thread("c", "Settled", json!({ "settledOverride": "settled" })),
    ]);
    let (workspace, cx) = workspace(backend.clone(), cx).await;
    open(&workspace, "a", cx);

    cx.dispatch_action(SettleThread);
    cx.run_until_parked();
    assert_eq!(
        backend.commands_without_ids(),
        [json!({ "type": "thread.settle", "threadId": "a" })]
    );
    // The next thread replaces the settled one.
    assert_eq!(active_thread(&workspace, cx), Some(Some("b".into())));
    assert_eq!(tab_count(&workspace, cx), 1);

    // With nothing left to move to, a new thread's draft takes its place.
    backend.push(json!({
        "kind": "thread.updated", "sequence": 8, "location": "active",
        "thread": thread("a", "First", json!({ "settledOverride": "settled" }))
    }));
    cx.run_until_parked();
    cx.dispatch_action(SettleThread);
    cx.run_until_parked();
    assert_eq!(backend.commands().len(), 2);
    assert_eq!(backend.commands()[1]["threadId"], "b");
    assert_eq!(active_thread(&workspace, cx), Some(None));
    assert_eq!(tab_count(&workspace, cx), 1);

    // A thread that's already settled stays put.
    open(&workspace, "c", cx);
    cx.dispatch_action(SettleThread);
    cx.run_until_parked();
    assert_eq!(backend.commands().len(), 2);
    assert_eq!(active_thread(&workspace, cx), Some(Some("c".into())));
}

#[gpui::test]
async fn a_draft_in_the_composer_keeps_the_settled_thread_open(cx: &mut TestAppContext) {
    let backend = Recorder::new(vec![
        thread(
            "a",
            "First",
            json!({ "createdAt": "2026-10-27T11:00:00.000Z" }),
        ),
        thread(
            "b",
            "Second",
            json!({ "createdAt": "2026-10-27T10:00:00.000Z" }),
        ),
    ]);
    let (workspace, cx) = workspace(backend.clone(), cx).await;
    let view = open(&workspace, "a", cx);
    let composer = view.read_with(cx, |view, _| view.composer().clone());
    composer.update_in(cx, |editor, window, cx| {
        editor.set_text("half a thought", window, cx)
    });
    cx.dispatch_action(SettleThread);
    cx.run_until_parked();
    assert_eq!(active_thread(&workspace, cx), Some(Some("b".into())));
    assert_eq!(tab_count(&workspace, cx), 2);
}

#[gpui::test]
async fn the_palette_organizes_the_open_thread(cx: &mut TestAppContext) {
    let backend = Recorder::new(vec![thread(
        "a",
        "First",
        json!({ "latestRunCompletedAt": "2026-10-27T11:00:00.000Z" }),
    )]);
    let (workspace, cx) = workspace(backend.clone(), cx).await;
    open(&workspace, "a", cx);
    cx.dispatch_action(UnsettleThread);
    cx.dispatch_action(PinThread);
    cx.dispatch_action(crate::UnpinThread);
    cx.dispatch_action(MarkThreadUnread);
    cx.run_until_parked();
    assert_eq!(
        backend.commands_without_ids(),
        [
            json!({ "type": "thread.unsettle", "threadId": "a", "reason": "user" }),
            json!({ "type": "thread.pin", "threadId": "a" }),
            json!({ "type": "thread.unpin", "threadId": "a" }),
            json!({ "type": "thread.mark-unread", "threadId": "a" }),
        ]
    );

    cx.dispatch_action(RenameThread);
    cx.run_until_parked();
    let modal = workspace
        .read_with(cx, |workspace, cx| {
            workspace.active_modal::<RenameThreadModal>(cx)
        })
        .expect("the rename modal is open");
    let editor = modal.read_with(cx, |modal, _| modal.editor().clone());
    editor.update_in(cx, |editor, window, cx| {
        editor.set_text("  Retry with backoff ", window, cx)
    });
    cx.dispatch_action(menu::Confirm);
    cx.run_until_parked();
    assert_eq!(
        backend.commands_without_ids()[4],
        json!({ "type": "thread.metadata.update", "threadId": "a", "title": "Retry with backoff" })
    );
    assert!(
        workspace
            .read_with(cx, |workspace, cx| workspace
                .active_modal::<RenameThreadModal>(cx))
            .is_none()
    );
}

#[gpui::test]
async fn archiving_offers_to_remove_an_unshared_worktree(cx: &mut TestAppContext) {
    let backend = Recorder::new(vec![
        thread("a", "Alone", json!({ "worktreePath": "/tmp/worktrees/a" })),
        thread(
            "b",
            "Sharing",
            json!({ "worktreePath": "/tmp/worktrees/shared" }),
        ),
        thread(
            "c",
            "Sharing too",
            json!({ "worktreePath": "/tmp/worktrees/shared" }),
        ),
        thread(
            "d",
            "Running",
            json!({ "status": "running", "worktreePath": "/tmp/worktrees/d" }),
        ),
    ]);
    let (workspace, cx) = workspace(backend.clone(), cx).await;
    // The worktrees view's handler, standing in for WP6's.
    let log = backend.log.clone();
    workspace.update(cx, |workspace, _| {
        workspace.register_action(move |_, action: &RemoveThreadWorktree, _, _| {
            log.lock()
                .push(format!("remove worktree of {}", action.thread_id))
        });
    });
    open(&workspace, "a", cx);

    cx.dispatch_action(ArchiveThread);
    cx.run_until_parked();
    assert!(cx.has_pending_prompt());
    cx.simulate_prompt_answer("Archive and Remove Worktree");
    cx.run_until_parked();
    // Removed first, while the shell still has the thread's worktree.
    assert_eq!(
        *backend.log.lock(),
        ["remove worktree of a", "thread.archive"]
    );
    assert_eq!(
        backend.commands_without_ids(),
        [json!({ "type": "thread.archive", "threadId": "a" })]
    );
    // A draft takes the archived thread's place.
    assert_eq!(active_thread(&workspace, cx), Some(None));

    // Another thread uses that worktree, so there's nothing to ask.
    open(&workspace, "b", cx);
    cx.dispatch_action(ArchiveThread);
    cx.run_until_parked();
    assert!(!cx.has_pending_prompt());
    assert_eq!(backend.log.lock().len(), 3);
    assert_eq!(backend.commands()[1]["threadId"], "b");

    // Cancelling sends nothing.
    backend.push(
        json!({ "kind": "thread.removed", "sequence": 8, "location": "active", "threadId": "b" }),
    );
    cx.run_until_parked();
    open(&workspace, "c", cx);
    cx.dispatch_action(ArchiveThread);
    cx.run_until_parked();
    cx.simulate_prompt_answer("Cancel");
    cx.run_until_parked();
    assert_eq!(backend.log.lock().len(), 3);

    // T3 won't archive a running thread.
    open(&workspace, "d", cx);
    cx.dispatch_action(ArchiveThread);
    cx.run_until_parked();
    assert!(!cx.has_pending_prompt());
    assert_eq!(backend.commands().len(), 2);
}

#[gpui::test]
async fn archiving_without_the_worktrees_view_asks_nothing(cx: &mut TestAppContext) {
    let backend = Recorder::new(vec![thread(
        "a",
        "Alone",
        json!({ "worktreePath": "/tmp/worktrees/a" }),
    )]);
    let (workspace, cx) = workspace(backend.clone(), cx).await;
    open(&workspace, "a", cx);
    cx.dispatch_action(ArchiveThread);
    cx.run_until_parked();
    assert!(!cx.has_pending_prompt());
    assert_eq!(
        backend.commands_without_ids(),
        [json!({ "type": "thread.archive", "threadId": "a" })]
    );
}

fn sidebar(workspace: &Entity<Workspace>, cx: &mut VisualTestContext) -> Entity<ThreadsSidebar> {
    let workspace = workspace.downgrade();
    let sidebar = cx.new_window_entity(|window, cx| ThreadsSidebar::new(workspace, window, cx));
    cx.run_until_parked();
    sidebar
}

#[gpui::test]
async fn opening_a_thread_records_a_visit(cx: &mut TestAppContext) {
    let backend = Recorder::new(vec![
        thread(
            "a",
            "Unseen",
            json!({
                "updatedAt": "2026-10-27T11:00:00.000Z",
                "latestRunCompletedAt": "2026-10-27T11:00:00.000Z",
                "lastVisitedAt": "2026-10-27T10:00:00.000Z"
            }),
        ),
        thread(
            "b",
            "Seen",
            json!({
                "updatedAt": "2026-10-27T11:00:00.000Z",
                "lastVisitedAt": "2026-10-27T11:00:00.000Z"
            }),
        ),
    ]);
    let (workspace, cx) = workspace(backend.clone(), cx).await;
    let sidebar = sidebar(&workspace, cx);
    let activate = |thread_id: Option<&str>, cx: &mut VisualTestContext| {
        sidebar.update(cx, |sidebar, cx| {
            sidebar.set_active_thread(thread_id.map(str::to_string), cx)
        });
        cx.run_until_parked();
    };
    activate(Some("a"), cx);
    activate(Some("b"), cx);
    // Once per visit: coming back to it sends nothing new.
    activate(Some("a"), cx);
    assert_eq!(
        backend.commands_without_ids(),
        [json!({
            "type": "thread.visit",
            "threadId": "a",
            "visitedAt": "2026-10-27T11:00:00.000Z"
        })]
    );

    // A run that finishes while the thread is open counts as seen.
    backend.push(json!({
        "kind": "thread.updated", "sequence": 8, "location": "active",
        "thread": thread("a", "Unseen", json!({
            "updatedAt": "2026-10-27T11:20:00.000Z",
            "latestRunCompletedAt": "2026-10-27T11:20:00.000Z",
            "lastVisitedAt": "2026-10-27T11:00:00.000Z"
        }))
    }));
    cx.run_until_parked();
    assert_eq!(
        backend.commands_without_ids()[1],
        json!({ "type": "thread.visit", "threadId": "a", "visitedAt": "2026-10-27T11:20:00.000Z" })
    );
    // While it streams, nothing.
    backend.push(json!({
        "kind": "thread.updated", "sequence": 9, "location": "active",
        "thread": thread("a", "Unseen", json!({
            "status": "running",
            "updatedAt": "2026-10-27T11:21:00.000Z",
            "latestRunCompletedAt": "2026-10-27T11:20:00.000Z",
            "lastVisitedAt": "2026-10-27T11:20:00.000Z"
        }))
    }));
    cx.run_until_parked();
    assert_eq!(backend.commands().len(), 2);
    activate(None, cx);
}

#[gpui::test]
async fn read_only_backends_record_no_visits(cx: &mut TestAppContext) {
    let backend = Arc::new(Recorder {
        shell: Mutex::new(snapshot(vec![thread(
            "a",
            "Unseen",
            json!({ "lastVisitedAt": "2026-10-27T08:00:00.000Z" }),
        )])),
        read_only: true,
        ..Default::default()
    });
    let (workspace, cx) = workspace(backend.clone(), cx).await;
    let sidebar = sidebar(&workspace, cx);
    sidebar.update(cx, |sidebar, cx| {
        sidebar.set_active_thread(Some("a".into()), cx)
    });
    cx.run_until_parked();
    assert!(backend.commands().is_empty());
    assert!(
        cx.update(|_, cx| crate::organize::blocked_reason(cx))
            .is_some()
    );
}

#[gpui::test]
async fn the_sidebar_folds_settled_threads_and_pages_them(cx: &mut TestAppContext) {
    let mut threads = vec![thread("open", "Open", json!({}))];
    for index in 0..40 {
        threads.push(thread(
            &format!("settled-{index:02}"),
            "Settled",
            json!({
                "settledOverride": "settled",
                "settledAt": format!("2026-10-{:02}T00:00:00.000Z", 1 + index % 27)
            }),
        ));
    }
    let backend = Recorder::new(threads);
    let (workspace, cx) = workspace(backend.clone(), cx).await;
    let sidebar = sidebar(&workspace, cx);
    let rows = |cx: &mut VisualTestContext| sidebar.read_with(cx, |sidebar, _| sidebar.row_count());
    // The Active header and its thread, and Settled's header.
    assert_eq!(rows(cx), 3);
    sidebar.update(cx, |sidebar, cx| sidebar.toggle_fold(Section::Settled, cx));
    assert_eq!(rows(cx), 3 + 10 + 1);
    sidebar.update(cx, |sidebar, cx| sidebar.show_more_settled(cx));
    assert_eq!(rows(cx), 3 + 35 + 1);
    sidebar.update(cx, |sidebar, cx| sidebar.show_more_settled(cx));
    assert_eq!(rows(cx), 3 + 40);
    sidebar.update(cx, |sidebar, cx| sidebar.toggle_fold(Section::Settled, cx));
    assert_eq!(rows(cx), 3);

    // The open thread shows in a folded section.
    sidebar.update(cx, |sidebar, cx| {
        sidebar.set_active_thread(Some("settled-05".into()), cx)
    });
    assert_eq!(rows(cx), 4);
    assert_eq!(
        sidebar.read_with(cx, |sidebar, _| sidebar.thread_list().settled.len()),
        40
    );
    cx.update(|_, cx| ThreadListFilter::set_project(Some("project-2".into()), cx));
    cx.run_until_parked();
    assert_eq!(rows(cx), 0);
}

// Ordering, searching, moving between threads, snoozing, undo and the archived list (WP13).

impl Recorder {
    fn answer(&self, tag: &str, answer: Value) {
        self.answers.lock().insert(tag.to_string(), Ok(answer));
    }

    /// Answers the oldest held call.
    fn answer_held(&self, answer: Value) {
        let sender = self.holds.lock().remove(0);
        sender.send(Ok(answer)).ok();
    }

    fn calls_to(&self, tag: &str) -> Vec<(Value, Access)> {
        self.calls
            .lock()
            .iter()
            .filter(|(called, _, _)| called == tag)
            .map(|(_, payload, access)| (payload.clone(), *access))
            .collect()
    }
}

fn rows(sidebar: &Entity<ThreadsSidebar>, cx: &mut VisualTestContext) -> Vec<String> {
    sidebar.read_with(cx, |sidebar, _| sidebar.rows())
}

/// Drops `thread_id` on the row described as `row`, as a drag and drop would.
fn drop_on(
    sidebar: &Entity<ThreadsSidebar>,
    thread_id: &str,
    row: &str,
    cx: &mut VisualTestContext,
) {
    sidebar.update_in(cx, |sidebar, window, cx| {
        let row = sidebar.row(row);
        sidebar.drop_thread(thread_id, row, window, cx)
    });
    cx.run_until_parked();
}

/// Two pinned threads with T3's keys, two active ones without, and a settled one.
fn ordered_threads() -> Vec<Value> {
    vec![
        thread(
            "p1",
            "Pinned first",
            json!({ "pinnedAt": NOW_ISO, "pinOrderKey": "h" }),
        ),
        thread(
            "p2",
            "Pinned second",
            json!({ "pinnedAt": NOW_ISO, "pinOrderKey": "p" }),
        ),
        thread(
            "a1",
            "Newest",
            json!({ "createdAt": "2026-10-27T11:00:00.000Z" }),
        ),
        thread(
            "a2",
            "Older",
            json!({ "createdAt": "2026-10-27T10:00:00.000Z" }),
        ),
        thread(
            "s1",
            "Settled",
            json!({ "settledOverride": "settled", "settledAt": "2026-10-27T08:00:00.000Z" }),
        ),
    ]
}

#[gpui::test]
async fn dragging_reorders_pinned_threads(cx: &mut TestAppContext) {
    let backend = Recorder::new(ordered_threads());
    let (workspace, cx) = workspace(backend.clone(), cx).await;
    let sidebar = sidebar(&workspace, cx);
    assert_eq!(
        rows(&sidebar, cx),
        [
            "Pinned", "t:p1", "t:p2", "Active", "t:a1", "t:a2", "Settled"
        ]
    );
    // Onto the thread above it: it goes before, with one key between the top and `h`.
    drop_on(&sidebar, "p2", "t:p1", cx);
    assert_eq!(
        backend.commands_without_ids(),
        [json!({ "type": "thread.pin.reorder", "threadId": "p2", "orderKey": "e" })]
    );
    // The new order shows before T3's shell has it…
    assert_eq!(rows(&sidebar, cx)[1..3], ["t:p2", "t:p1"]);
    // …and stays once it does.
    backend.push(json!({
        "kind": "thread.updated", "sequence": 100, "location": "active",
        "thread": thread("p2", "Pinned second", json!({ "pinnedAt": NOW_ISO, "pinOrderKey": "e" }))
    }));
    cx.run_until_parked();
    assert_eq!(rows(&sidebar, cx)[1..3], ["t:p2", "t:p1"]);

    // Dropping where it already is does nothing.
    sidebar.read_with(cx, |sidebar, _| {
        assert!(!sidebar.can_drop_on("p1", sidebar.row("t:p1")));
        assert!(sidebar.can_drop_on("a1", sidebar.row("t:p1")));
    });
}

#[gpui::test]
async fn dragging_pins_unpins_and_settles(cx: &mut TestAppContext) {
    let backend = Recorder::new(ordered_threads());
    let (workspace, cx) = workspace(backend.clone(), cx).await;
    let sidebar = sidebar(&workspace, cx);

    // An active thread onto Pinned's header: pinned at the top.
    drop_on(&sidebar, "a2", "Pinned", cx);
    assert_eq!(
        backend.commands_without_ids(),
        [json!({ "type": "thread.pin", "threadId": "a2", "orderKey": "e" })]
    );
    assert_eq!(
        rows(&sidebar, cx),
        [
            "Pinned", "t:a2", "t:p1", "t:p2", "Active", "t:a1", "Settled"
        ]
    );
    cx.executor().advance_clock(Duration::from_secs(3));
    cx.run_until_parked();

    // A pinned thread onto Active's header, from above: unpinned, at the top of Active. The
    // active threads have no keys yet, so they all get one, as in T3.
    drop_on(&sidebar, "p2", "Active", cx);
    assert_eq!(
        backend.commands_without_ids()[1..],
        [
            json!({ "type": "thread.unpin", "threadId": "p2" }),
            json!({ "type": "thread.active.reorder", "threadId": "p2", "orderKey": "gn" }),
            json!({ "type": "thread.active.reorder", "threadId": "a1", "orderKey": "nb" }),
            json!({ "type": "thread.active.reorder", "threadId": "a2", "orderKey": "tn" }),
        ]
    );
    cx.executor().advance_clock(Duration::from_secs(3));
    cx.run_until_parked();

    // Onto Settled's header: settled, with Undo.
    drop_on(&sidebar, "a1", "Settled", cx);
    assert_eq!(
        backend.commands_without_ids().last().unwrap(),
        &json!({ "type": "thread.settle", "threadId": "a1" })
    );
    cx.dispatch_action(crate::UndoThreadAction);
    cx.run_until_parked();
    assert_eq!(
        backend.commands_without_ids().last().unwrap(),
        &json!({ "type": "thread.unsettle", "threadId": "a1", "reason": "user" })
    );
    // A settled thread can't go among the settled ones again.
    sidebar.read_with(cx, |sidebar, _| {
        assert!(!sidebar.can_drop_on("s1", sidebar.row("Settled")));
    });
}

#[gpui::test]
async fn dragging_wakes_and_pins_into_an_empty_section(cx: &mut TestAppContext) {
    let backend = Recorder::new(vec![
        thread(
            "a1",
            "Active",
            json!({ "createdAt": "2026-10-27T11:00:00.000Z" }),
        ),
        thread(
            "z1",
            "Snoozed",
            json!({
                "snoozedAt": "2026-10-27T10:00:00.000Z",
                // Far off, so the test's clock can't wake it.
                "snoozedUntil": "2099-01-01T00:00:00.000Z"
            }),
        ),
    ]);
    let (workspace, cx) = workspace(backend.clone(), cx).await;
    let sidebar = sidebar(&workspace, cx);
    sidebar.update(cx, |sidebar, cx| sidebar.toggle_fold(Section::Snoozed, cx));
    assert_eq!(rows(&sidebar, cx), ["Active", "t:a1", "Snoozed", "t:z1"]);
    // Snoozed threads need a wake time, so nothing drops among them.
    sidebar.read_with(cx, |sidebar, _| {
        assert!(!sidebar.can_drop_on("a1", sidebar.row("t:z1")));
    });

    // A snoozed thread onto an active one: woken, and ordered.
    drop_on(&sidebar, "z1", "t:a1", cx);
    assert_eq!(
        backend.commands_without_ids(),
        [
            json!({ "type": "thread.unsnooze", "threadId": "z1", "reason": "user" }),
            json!({ "type": "thread.active.reorder", "threadId": "z1", "orderKey": "ir" }),
            json!({ "type": "thread.active.reorder", "threadId": "a1", "orderKey": "rj" }),
        ]
    );
    cx.executor().advance_clock(Duration::from_secs(3));
    cx.run_until_parked();

    // With nothing pinned, a drag shows where to pin.
    sidebar.update(cx, |sidebar, cx| sidebar.begin_drag("a1", cx));
    assert_eq!(
        rows(&sidebar, cx),
        [
            "Pinned",
            "empty:Pinned",
            "Active",
            "t:a1",
            "Snoozed",
            "t:z1"
        ]
    );
    drop_on(&sidebar, "a1", "empty:Pinned", cx);
    assert_eq!(
        backend.commands_without_ids().last().unwrap(),
        &json!({ "type": "thread.pin", "threadId": "a1", "orderKey": "n" })
    );
}

#[gpui::test]
async fn a_refused_drop_puts_the_order_back(cx: &mut TestAppContext) {
    let backend = Arc::new(Recorder {
        shell: Mutex::new(snapshot(ordered_threads())),
        read_only: true,
        ..Default::default()
    });
    let (workspace, cx) = workspace(backend.clone(), cx).await;
    let sidebar = sidebar(&workspace, cx);
    let before = rows(&sidebar, cx);
    drop_on(&sidebar, "p2", "t:p1", cx);
    assert!(backend.commands().is_empty());
    assert_eq!(rows(&sidebar, cx), before);
}

/// Threads for searching, and T3's answer for "webhook retry".
fn search_fixture() -> Arc<Recorder> {
    let backend = Recorder::new(vec![
        thread("a", "Fix the webhook retry", json!({})),
        thread("b", "Billing export", json!({})),
        thread(
            "c",
            "Release notes",
            json!({ "settledOverride": "settled", "settledAt": "2026-10-27T08:00:00.000Z" }),
        ),
    ]);
    let found = |thread_id: &str, snippet: &str, quality: f64, hits: i64, at: &str| {
        json!({
            "threadId": thread_id, "projectId": "project-1", "source": "assistant",
            "snippet": snippet, "messageCreatedAt": at, "quality": quality, "hits": hits
        })
    };
    backend.answer(
        "orchestration.searchThreads",
        json!({ "matches": [
            found("b", "…the  webhook retry loop backs off…", 0.8, 2, "2026-10-27T10:00:00.000Z"),
            found("a", "already found by its title", 1.0, 1, "2026-10-27T10:00:00.000Z"),
            found("c", "retry the webhook once", 1.0, 1, "2026-10-27T09:00:00.000Z"),
            found("gone", "an archived thread", 1.0, 1, "2026-10-27T09:00:00.000Z"),
            found("b", "a weaker match of the same thread", 0.3, 1, "2026-10-27T11:00:00.000Z"),
        ]}),
    );
    backend
}

fn search(sidebar: &Entity<ThreadsSidebar>, query: &str, cx: &mut VisualTestContext) {
    let editor = sidebar.read_with(cx, |sidebar, _| sidebar.search_editor().clone());
    editor.update_in(cx, |editor, window, cx| editor.set_text(query, window, cx));
    cx.run_until_parked();
}

#[gpui::test]
async fn search_lists_message_matches_after_title_matches(cx: &mut TestAppContext) {
    let backend = search_fixture();
    let (workspace, cx) = workspace(backend.clone(), cx).await;
    let sidebar = sidebar(&workspace, cx);
    search(&sidebar, "webhook retry", cx);
    // Title matches at once; messages once the typing stops.
    assert_eq!(rows(&sidebar, cx), ["found:a", "searching"]);
    assert!(backend.calls_to("orchestration.searchThreads").is_empty());
    cx.executor().advance_clock(Duration::from_millis(150));
    cx.run_until_parked();
    assert_eq!(
        backend.calls_to("orchestration.searchThreads"),
        [(json!({ "query": "webhook retry" }), Access::Read)]
    );
    // Best first, each thread once, and none the title search found or the list doesn't have.
    assert_eq!(
        rows(&sidebar, cx),
        ["found:a", "In messages", "message:c", "message:b"]
    );

    // Words under three letters can't match messages, so T3 isn't asked.
    search(&sidebar, "fi  x", cx);
    cx.executor().advance_clock(Duration::from_millis(150));
    cx.run_until_parked();
    assert_eq!(backend.calls_to("orchestration.searchThreads").len(), 1);
    assert_eq!(rows(&sidebar, cx), ["found:a"]);
}

#[gpui::test]
async fn search_without_message_search_finds_titles(cx: &mut TestAppContext) {
    let backend = search_fixture();
    backend
        .unsupported
        .lock()
        .insert("orchestration.searchThreads".into());
    let (workspace, cx) = workspace(backend.clone(), cx).await;
    let sidebar = sidebar(&workspace, cx);
    search(&sidebar, "webhook", cx);
    cx.executor().advance_clock(Duration::from_millis(150));
    cx.run_until_parked();
    // An older T3 without it: just the title matches, no error.
    assert_eq!(rows(&sidebar, cx), ["found:a"]);

    // A failure says so.
    backend.unsupported.lock().clear();
    backend.answers.lock().insert(
        "orchestration.searchThreads".into(),
        Err("the database is busy".into()),
    );
    search(&sidebar, "billing", cx);
    cx.executor().advance_clock(Duration::from_millis(150));
    cx.run_until_parked();
    assert_eq!(rows(&sidebar, cx), ["found:b", "search failed"]);
}

#[test]
fn snippets_highlight_the_searched_words() {
    let text = "Retry the WEBHOOK once, then retry";
    let marked = crate::sidebar::highlights(text, "webhook retry");
    let highlighted = marked
        .iter()
        .map(|index| &text[*index..*index + 1])
        .collect::<String>();
    assert_eq!(highlighted, "RetryWEBHOOKretry");
}

#[test]
fn snippets_start_near_their_match() {
    use crate::sidebar::focus_snippet;
    assert_eq!(
        focus_snippet("…we kept the old worktree around", "worktree"),
        "…we kept the old worktree around"
    );
    assert_eq!(
        focus_snippet(
            "…excerpts and review comments, then the agent removed the worktree and pushed",
            "worktree"
        ),
        "…removed the worktree and pushed"
    );
    assert_eq!(focus_snippet("no match here", "worktree"), "no match here");
}

#[gpui::test]
async fn previous_and_next_follow_the_sidebar(cx: &mut TestAppContext) {
    let backend = Recorder::new(ordered_threads());
    let (workspace, cx) = workspace(backend.clone(), cx).await;
    let _sidebar = sidebar(&workspace, cx);
    open(&workspace, "a1", cx);
    assert_eq!(tab_count(&workspace, cx), 1);
    let next = |cx: &mut VisualTestContext| step(&workspace, crate::NextThread, cx);
    let previous = |cx: &mut VisualTestContext| step(&workspace, crate::PreviousThread, cx);
    assert_eq!(next(cx).as_deref(), Some("a2"));
    // Settled is folded, so a2 is the last.
    assert_eq!(next(cx).as_deref(), Some("a2"));
    assert_eq!(previous(cx).as_deref(), Some("a1"));
    assert_eq!(previous(cx).as_deref(), Some("p2"));
    assert_eq!(previous(cx).as_deref(), Some("p1"));
    assert_eq!(previous(cx).as_deref(), Some("p1"));
    // Each step replaced the thread's tab.
    assert_eq!(tab_count(&workspace, cx), 1);
    // From a draft, the first.
    workspace.update_in(cx, |workspace, window, cx| {
        ThreadView::open_new(workspace, window, cx);
    });
    cx.run_until_parked();
    assert_eq!(next(cx).as_deref(), Some("p1"));
}

/// Dispatches `action`, and returns the thread the workspace then shows.
fn step<A: gpui::Action>(
    workspace: &Entity<Workspace>,
    action: A,
    cx: &mut VisualTestContext,
) -> Option<String> {
    cx.dispatch_action(action);
    cx.run_until_parked();
    active_thread(workspace, cx).flatten()
}

/// Binds Zed's default keymap, then mustr's, as the app does.
fn bind_zed_then_mustr_keys(cx: &mut VisualTestContext) {
    cx.update(|_, cx| {
        let content = util::asset_str::<settings::SettingsAssets>(settings::DEFAULT_KEYMAP_PATH);
        let bindings = match settings::KeymapFile::load(&content, cx) {
            settings::KeymapFileLoadResult::Success { key_bindings }
            | settings::KeymapFileLoadResult::SomeFailedToLoad { key_bindings, .. } => key_bindings,
            settings::KeymapFileLoadResult::JsonParseFailure { error } => {
                panic!("Zed's keymap doesn't parse: {error}")
            }
        };
        cx.clear_key_bindings();
        cx.bind_keys(bindings);
        cx.bind_keys(crate::key_bindings());
    });
}

#[gpui::test]
async fn the_list_keys_win_in_a_thread(cx: &mut TestAppContext) {
    let backend = Recorder::new(vec![
        thread(
            "a",
            "First",
            json!({ "createdAt": "2026-10-27T11:00:00.000Z" }),
        ),
        thread(
            "b",
            "Second",
            json!({
                "createdAt": "2026-10-27T10:00:00.000Z",
                "updatedAt": "2026-10-27T10:30:00.000Z",
                "worktreePath": "/wt/second", "branch": "t3/second"
            }),
        ),
    ]);
    let (workspace, cx) = workspace(backend.clone(), cx).await;
    let _sidebar = sidebar(&workspace, cx);
    bind_zed_then_mustr_keys(cx);
    let view = open(&workspace, "a", cx);
    // In the composer, over the editor's Fold and Unfold.
    let composer = view.read_with(cx, |view, _| view.composer().clone());
    cx.update(|window, cx| window.focus(&composer.focus_handle(cx), cx));
    cx.simulate_keystrokes("ctrl-}");
    cx.run_until_parked();
    assert_eq!(active_thread(&workspace, cx), Some(Some("b".into())));
    let view = workspace.read_with(cx, |workspace, cx| {
        workspace
            .active_item(cx)
            .unwrap()
            .downcast::<ThreadView>()
            .unwrap()
    });
    cx.update(|window, cx| window.focus(&view.focus_handle(cx), cx));
    cx.simulate_keystrokes("ctrl-{");
    cx.run_until_parked();
    assert_eq!(active_thread(&workspace, cx), Some(Some("a".into())));

    // On a draft, ctrl-shift-l takes the previous worktree, and ctrl-shift-x opens the menu.
    let draft = workspace
        .update_in(cx, |workspace, window, cx| {
            ThreadView::open_new(workspace, window, cx)
        })
        .unwrap();
    cx.run_until_parked();
    let form = draft
        .read_with(cx, |view, _| view.new_thread.clone())
        .unwrap();
    let composer = draft.read_with(cx, |view, _| view.composer().clone());
    cx.update(|window, cx| window.focus(&composer.focus_handle(cx), cx));
    cx.simulate_keystrokes("ctrl-shift-l");
    cx.run_until_parked();
    assert_eq!(
        form.read_with(cx, |form, _| form.picked_worktree()),
        Some((
            crate::new_thread::WorkspaceKind::ExistingWorktree,
            Some(("/wt/second".into(), Some("t3/second".into())))
        ))
    );
    cx.simulate_keystrokes("ctrl-shift-x");
    cx.run_until_parked();
    assert!(form.read_with(cx, |form, _| form.workspace_menu_is_open()));
}

#[gpui::test]
async fn snoozing_the_open_thread_moves_on_and_undoes(cx: &mut TestAppContext) {
    let backend = Recorder::new(ordered_threads());
    let (workspace, cx) = workspace(backend.clone(), cx).await;
    open(&workspace, "a1", cx);
    let weak = workspace.downgrade();
    cx.update(|window, cx| {
        crate::snooze::snooze_thread(
            "a1".into(),
            "Newest".into(),
            "2099-01-01T09:00:00.000Z".into(),
            weak,
            window,
            cx,
        )
    });
    cx.run_until_parked();
    assert_eq!(
        backend.commands_without_ids(),
        [json!({
            "type": "thread.snooze",
            "threadId": "a1",
            "snoozedUntil": "2099-01-01T09:00:00.000Z"
        })]
    );
    // The next thread that isn't snoozed or settled takes its place.
    assert_eq!(active_thread(&workspace, cx), Some(Some("a2".into())));
    // mod+z undoes it, once.
    cx.dispatch_action(crate::UndoThreadAction);
    cx.dispatch_action(crate::UndoThreadAction);
    cx.run_until_parked();
    assert_eq!(
        backend.commands_without_ids()[1..],
        [json!({ "type": "thread.unsnooze", "threadId": "a1", "reason": "user" })]
    );
}

#[gpui::test]
async fn the_custom_snooze_dialog_checks_its_time(cx: &mut TestAppContext) {
    use chrono::TimeZone as _;
    let backend = Recorder::new(ordered_threads());
    let (workspace, cx) = workspace(backend.clone(), cx).await;
    open(&workspace, "a1", cx);
    cx.dispatch_action(crate::SnoozeThread);
    cx.run_until_parked();
    let dialog = workspace
        .read_with(cx, |workspace, cx| {
            workspace.active_modal::<crate::snooze::SnoozeDialog>(cx)
        })
        .expect("the snooze dialog is open");
    let [date, time, _] = dialog.read_with(cx, |dialog, _| dialog.editors().map(Entity::clone));
    let set = |editor: &Entity<editor::Editor>, text: &str, cx: &mut VisualTestContext| {
        editor.update_in(cx, |editor, window, cx| editor.set_text(text, window, cx))
    };
    // In the past: refused, with a reason.
    set(&date, "2020-01-01", cx);
    cx.dispatch_action(menu::Confirm);
    cx.run_until_parked();
    assert!(backend.commands().is_empty());
    assert_eq!(
        dialog.read_with(cx, |dialog, _| dialog.error().cloned()),
        Some("Choose a valid date and time in the future.".into())
    );
    set(&date, "2099-03-04", cx);
    set(&time, "08:30", cx);
    cx.dispatch_action(menu::Confirm);
    cx.run_until_parked();
    let expected = chrono::Local
        .with_ymd_and_hms(2099, 3, 4, 8, 30, 0)
        .earliest()
        .unwrap()
        .with_timezone(&chrono::Utc)
        .format("%Y-%m-%dT%H:%M:%S%.3fZ")
        .to_string();
    assert_eq!(
        backend.commands_without_ids(),
        [json!({ "type": "thread.snooze", "threadId": "a1", "snoozedUntil": expected })]
    );
    assert!(
        workspace
            .read_with(cx, |workspace, cx| workspace
                .active_modal::<crate::snooze::SnoozeDialog>(cx))
            .is_none()
    );

    // A duration.
    open(&workspace, "a1", cx);
    cx.dispatch_action(crate::SnoozeThread);
    cx.run_until_parked();
    let dialog = workspace
        .read_with(cx, |workspace, cx| {
            workspace.active_modal::<crate::snooze::SnoozeDialog>(cx)
        })
        .unwrap();
    let [_, _, amount] = dialog.read_with(cx, |dialog, _| dialog.editors().map(Entity::clone));
    dialog.update_in(cx, |dialog, window, cx| {
        dialog.set_mode(
            crate::snooze::CustomMode::Duration,
            crate::snooze::DurationUnit::Minutes,
            window,
            cx,
        )
    });
    set(&amount, "90", cx);
    let before = chrono::Utc::now();
    cx.dispatch_action(menu::Confirm);
    cx.run_until_parked();
    let until = backend.commands()[1]["snoozedUntil"]
        .as_str()
        .unwrap()
        .parse::<chrono::DateTime<chrono::Utc>>()
        .unwrap();
    let minutes = (until - before).num_minutes();
    assert!((89..=91).contains(&minutes), "{minutes} minutes");
}

#[gpui::test]
async fn settling_and_unpinning_offer_undo_for_five_seconds(cx: &mut TestAppContext) {
    let backend = Recorder::new(vec![
        thread(
            "p",
            "Pinned and snoozed",
            json!({
                "pinnedAt": NOW_ISO, "pinOrderKey": "h",
                "createdAt": "2026-10-27T11:00:00.000Z"
            }),
        ),
        thread(
            "b",
            "Other",
            json!({ "createdAt": "2026-10-27T10:00:00.000Z" }),
        ),
    ]);
    let (workspace, cx) = workspace(backend.clone(), cx).await;
    open(&workspace, "p", cx);
    cx.dispatch_action(SettleThread);
    cx.run_until_parked();
    cx.dispatch_action(crate::UndoThreadAction);
    cx.run_until_parked();
    // Un-settled, and pinned back in its place.
    assert_eq!(
        backend.commands_without_ids(),
        [
            json!({ "type": "thread.settle", "threadId": "p" }),
            json!({ "type": "thread.unsettle", "threadId": "p", "reason": "user" }),
            json!({ "type": "thread.pin", "threadId": "p", "orderKey": "h" }),
        ]
    );

    open(&workspace, "p", cx);
    cx.dispatch_action(crate::UnpinThread);
    cx.run_until_parked();
    cx.dispatch_action(crate::UndoThreadAction);
    cx.run_until_parked();
    assert_eq!(
        backend.commands_without_ids()[3..],
        [
            json!({ "type": "thread.unpin", "threadId": "p" }),
            json!({ "type": "thread.pin", "threadId": "p", "orderKey": "h" }),
        ]
    );

    // After five seconds, there's nothing to undo.
    cx.dispatch_action(crate::UnpinThread);
    cx.run_until_parked();
    cx.executor().advance_clock(Duration::from_secs(6));
    cx.run_until_parked();
    cx.dispatch_action(crate::UndoThreadAction);
    cx.run_until_parked();
    assert_eq!(backend.commands().len(), 6);
}

#[gpui::test]
async fn auto_settle_and_the_pull_request_link(cx: &mut TestAppContext) {
    let link = |number: i64, state: &str, updated: &str| {
        json!({
            "host": "github.com", "repository": "acme/api", "number": number,
            "url": format!("https://github.com/acme/api/pull/{number}"),
            "linkedAt": "2026-10-20T00:00:00.000Z", "source": "manual", "stack": null,
            "snapshot": {
                "baseBranch": "main", "headBranch": "t3/x", "isDraft": false, "state": state,
                "title": format!("PR {number}"), "syncedAt": updated, "updatedAt": updated
            }
        })
    };
    let backend = Recorder::new(vec![thread(
        "a",
        "With pull requests",
        json!({
            "pullRequests": [
                link(41, "merged", "2026-10-27T11:00:00.000Z"),
                link(42, "open", "2026-10-26T11:00:00.000Z")
            ]
        }),
    )]);
    let (workspace, cx) = workspace(backend.clone(), cx).await;
    let sidebar = sidebar(&workspace, cx);
    // The open one, though the merged one is newer.
    let pull_request = sidebar.read_with(cx, |sidebar, _| {
        sidebar
            .thread_list()
            .find("a")
            .unwrap()
            .pull_request
            .clone()
    });
    assert_eq!(
        pull_request.map(|pull_request| (pull_request.number, pull_request.state)),
        Some((42, Some(t3_api::types::PullRequestState::Open)))
    );
    open(&workspace, "a", cx);
    cx.dispatch_action(crate::CopyPullRequestLink);
    cx.run_until_parked();
    assert_eq!(
        cx.read_from_clipboard().and_then(|item| item.text()),
        Some("https://github.com/acme/api/pull/42".into())
    );
    cx.dispatch_action(crate::ToggleThreadAutoSettle);
    cx.run_until_parked();
    assert_eq!(
        backend.commands_without_ids(),
        [json!({ "type": "thread.auto-settle.set", "threadId": "a", "enabled": false })]
    );
}

#[gpui::test]
async fn the_archived_list_brings_threads_back(cx: &mut TestAppContext) {
    let backend = Recorder::new(vec![thread("a", "Active", json!({}))]);
    backend.answer(
        "orchestration.getArchivedShellSnapshot",
        json!({
            "schemaVersion": 1, "snapshotSequence": 9, "projects": [],
            "threads": [
                thread("old", "Archived last week", json!({ "archivedAt": "2026-10-20T12:00:00.000Z" })),
                thread("new", "Archived today", json!({ "archivedAt": "2026-10-27T11:00:00.000Z" })),
                thread("child", "A subagent", json!({
                    "archivedAt": "2026-10-27T11:00:00.000Z",
                    "lineage": {
                        "rootThreadId": "new", "parentThreadId": "new",
                        "relationshipToParent": "subagent"
                    }
                })),
            ]
        }),
    );
    let (workspace, cx) = workspace(backend.clone(), cx).await;
    cx.dispatch_action(crate::ShowArchivedThreads);
    cx.run_until_parked();
    assert_eq!(
        backend.calls_to("orchestration.getArchivedShellSnapshot"),
        [(json!({}), Access::Read)]
    );
    let picker = workspace
        .read_with(cx, |workspace, cx| {
            workspace.active_modal::<picker::Picker<crate::organize::ArchivedThreadsDelegate>>(cx)
        })
        .expect("the archived list is open");
    let listed = |cx: &mut VisualTestContext| {
        picker.read_with(cx, |picker, _| {
            picker
                .delegate
                .listed()
                .into_iter()
                .map(|thread| (thread.title.clone(), thread.project.clone()))
                .collect::<Vec<_>>()
        })
    };
    assert_eq!(
        listed(cx),
        [
            ("Archived today".to_string(), Some("api".to_string())),
            ("Archived last week".to_string(), Some("api".to_string()))
        ]
    );
    cx.dispatch_action(menu::Confirm);
    cx.run_until_parked();
    assert_eq!(
        backend.commands_without_ids(),
        [json!({ "type": "thread.unarchive", "threadId": "new" })]
    );
    assert_eq!(listed(cx).len(), 1);
}

#[gpui::test]
async fn a_read_only_archived_list_unarchives_nothing(cx: &mut TestAppContext) {
    let backend = Arc::new(Recorder {
        shell: Mutex::new(snapshot(vec![])),
        read_only: true,
        ..Default::default()
    });
    backend.answer(
        "orchestration.getArchivedShellSnapshot",
        json!({
            "schemaVersion": 1, "snapshotSequence": 9, "projects": [],
            "threads": [thread("old", "Archived", json!({ "archivedAt": NOW_ISO }))]
        }),
    );
    let (_workspace, cx) = workspace(backend.clone(), cx).await;
    cx.dispatch_action(crate::ShowArchivedThreads);
    cx.run_until_parked();
    cx.dispatch_action(menu::Confirm);
    cx.run_until_parked();
    assert!(backend.commands().is_empty());
}

#[gpui::test]
async fn new_threads_reuse_the_previous_worktree(cx: &mut TestAppContext) {
    let backend = Recorder::new(vec![
        thread(
            "one",
            "One",
            json!({
                "updatedAt": "2026-10-27T10:00:00.000Z",
                "worktreePath": "/wt/one", "branch": "t3/one"
            }),
        ),
        thread(
            "two",
            "Two",
            json!({
                "updatedAt": "2026-10-27T11:00:00.000Z",
                "worktreePath": "/wt/two", "branch": "t3/two"
            }),
        ),
        // Newer, but in another project.
        thread(
            "elsewhere",
            "Elsewhere",
            json!({
                "projectId": "project-2", "updatedAt": "2026-10-27T11:30:00.000Z",
                "worktreePath": "/wt/web", "branch": "t3/web"
            }),
        ),
    ]);
    let (workspace, cx) = workspace(backend.clone(), cx).await;
    // On a thread that isn't a draft, nothing changes.
    open(&workspace, "one", cx);
    cx.dispatch_action(crate::UsePreviousWorktree);
    cx.run_until_parked();
    let draft = workspace
        .update_in(cx, |workspace, window, cx| {
            ThreadView::open_new(workspace, window, cx)
        })
        .unwrap();
    cx.run_until_parked();
    let form = draft
        .read_with(cx, |view, _| view.new_thread.clone())
        .unwrap();
    cx.dispatch_action(crate::UsePreviousWorktree);
    cx.run_until_parked();
    let picked = |cx: &mut VisualTestContext| form.read_with(cx, |form, _| form.picked_worktree());
    use crate::new_thread::WorkspaceKind;
    assert_eq!(
        picked(cx),
        Some((
            WorkspaceKind::ExistingWorktree,
            Some(("/wt/two".into(), Some("t3/two".into())))
        ))
    );
    // Again: the one before it, since that one's now picked.
    cx.dispatch_action(crate::UsePreviousWorktree);
    cx.run_until_parked();
    assert_eq!(
        picked(cx),
        Some((
            WorkspaceKind::ExistingWorktree,
            Some(("/wt/one".into(), Some("t3/one".into())))
        ))
    );
    // The menu picks where it works.
    cx.dispatch_action(crate::ToggleWorkspacePicker);
    cx.run_until_parked();
    assert!(form.read_with(cx, |form, _| form.workspace_menu_is_open()));
    cx.dispatch_action(crate::ToggleWorkspacePicker);
    cx.run_until_parked();
    assert!(!form.read_with(cx, |form, _| form.workspace_menu_is_open()));
}

#[gpui::test]
async fn one_message_search_runs_at_a_time(cx: &mut TestAppContext) {
    let backend = search_fixture();
    backend
        .held
        .lock()
        .insert("orchestration.searchThreads".into());
    let (workspace, cx) = workspace(backend.clone(), cx).await;
    let sidebar = sidebar(&workspace, cx);
    let queries = |backend: &Recorder| {
        backend
            .calls_to("orchestration.searchThreads")
            .into_iter()
            .map(|(payload, _)| payload["query"].as_str().unwrap().to_string())
            .collect::<Vec<_>>()
    };
    search(&sidebar, "billing", cx);
    cx.executor().advance_clock(Duration::from_millis(150));
    cx.run_until_parked();
    // T3 is still searching: the next queries wait for it.
    search(&sidebar, "billing exp", cx);
    cx.executor().advance_clock(Duration::from_millis(150));
    search(&sidebar, "billing export", cx);
    cx.executor().advance_clock(Duration::from_millis(150));
    cx.run_until_parked();
    assert_eq!(queries(&backend), ["billing"]);
    assert_eq!(rows(&sidebar, cx), ["found:b", "searching"]);
    // Its answer shows while the latest query goes.
    let found = json!({ "matches": [{
        "threadId": "c", "projectId": "project-1", "source": "user",
        "snippet": "the billing export", "messageCreatedAt": NOW_ISO
    }]});
    backend.answer_held(found);
    cx.run_until_parked();
    assert_eq!(queries(&backend), ["billing", "billing export"]);
    assert_eq!(rows(&sidebar, cx), ["found:b", "In messages", "message:c"]);
    backend.answer_held(json!({ "matches": [] }));
    cx.run_until_parked();
    assert_eq!(rows(&sidebar, cx), ["found:b"]);
}

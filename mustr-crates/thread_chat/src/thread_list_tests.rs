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
use futures::{
    FutureExt as _, StreamExt as _, channel::mpsc, future::BoxFuture, stream::BoxStream,
};
use gpui::{Entity, TestAppContext, VisualContext as _, VisualTestContext};
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

    fn call_raw(&self, tag: &str, _: Value, _: Access) -> BoxFuture<'static, Result<Value>> {
        ready(Err(anyhow!("unexpected call to {tag}")))
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

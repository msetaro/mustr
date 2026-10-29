use super::*;
use crate::backend::AttachmentUpload;
use anyhow::{Result, anyhow};
use futures::{FutureExt as _, channel::mpsc, future::BoxFuture, stream::BoxStream};
use gpui::{Entity, TestAppContext, VisualTestContext, px};
use parking_lot::Mutex;
use pretty_assertions::assert_eq;
use serde_json::{Value, json};
use settings::SettingsStore;
use t3_api::{
    tagged::{OrchestrationV2ShellStreamItem, OrchestrationV2ThreadStreamItem},
    types::{
        OrchestrationV2DispatchCommandResult, OrchestrationV2GetTurnItemInput,
        OrchestrationV2GetTurnItemResult, OrchestrationV2ShellSnapshot,
        OrchestrationV2SubscribeShellInput, OrchestrationV2SubscribeThreadInput,
        OrchestrationV2ThreadDetailSnapshot, OrchestrationV2ThreadLaunchInput,
        OrchestrationV2ThreadLaunchResult, ServerProvider,
    },
};

const THREAD: &str = "thread-1";
const NOW: &str = "2026-10-28T12:00:00.000Z";

fn stage(
    id: &str,
    status: &str,
    detail: Option<&str>,
    percent: Option<i64>,
    tail: &[&str],
) -> Value {
    let started = (status != "pending").then_some(NOW);
    let ended =
        matches!(status, "done" | "failed" | "skipped").then_some("2026-10-28T12:00:04.000Z");
    json!({
        "id": id, "status": status, "startedAt": started, "endedAt": ended,
        "percent": percent, "detail": detail, "tail": tail
    })
}

fn setup(phase: &str, sequence: i64, stages: Vec<Value>, error: Option<&str>) -> Value {
    json!({
        "threadId": THREAD, "phase": phase, "startedAt": NOW,
        "endedAt": (phase != "running").then_some("2026-10-28T12:01:05.000Z"),
        "branch": "t3/retry-backoff", "baseRef": "origin/main",
        "worktreePath": "/tmp/worktrees/retry-backoff",
        "setupScript": { "name": "Install", "command": "pnpm install", "terminalId": "setup-install" },
        "stages": stages, "error": error, "sequence": sequence
    })
}

fn snapshot(value: Value) -> WorktreeSetupSnapshot {
    serde_json::from_value(value).expect("the setup fixture decodes")
}

#[test]
fn the_card_says_what_t3s_does() {
    let running = snapshot(setup(
        "running",
        1,
        vec![
            stage("fetch", "skipped", None, None, &[]),
            stage("checkout", "running", None, Some(42), &[]),
            stage("setup-script", "pending", None, None, &[]),
            stage("agent", "pending", None, None, &[]),
        ],
        None,
    ));
    assert!(is_visible(&running));
    assert_eq!(headline(&running), "Setting up worktree…");
    let details = running.stages.iter().map(stage_detail).collect::<Vec<_>>();
    assert_eq!(
        details,
        [Some("skipped".into()), Some("42%".into()), None, None]
    );
    let labels = running
        .stages
        .iter()
        .map(|stage| stage_label(&running, stage))
        .collect::<Vec<_>>();
    assert_eq!(
        labels,
        [
            "Fetch base branch",
            "Check out files",
            "Install",
            "Start agent"
        ]
    );
    assert_eq!(shown_tail(&running), None);

    // A setup script that fails after the agent started leaves the worktree ready.
    let script_failed = snapshot(setup(
        "done",
        5,
        vec![
            stage(
                "setup-script",
                "failed",
                Some("exited with 1"),
                None,
                &["a", "b", "c", "d", "e"],
            ),
            stage("agent", "done", None, None, &[]),
        ],
        None,
    ));
    assert!(is_visible(&script_failed));
    assert_eq!(
        headline(&script_failed),
        "Worktree ready, setup script failed"
    );
    assert_eq!(
        shown_tail(&script_failed),
        Some(vec!["b".into(), "c".into(), "d".into(), "e".into()])
    );
    // One that succeeds hides the card.
    let done = snapshot(setup(
        "done",
        6,
        vec![stage("agent", "done", None, None, &[])],
        None,
    ));
    assert!(!is_visible(&done));
    assert_eq!(headline(&done), "Worktree ready");
    let running_tail = snapshot(setup(
        "running",
        2,
        vec![stage(
            "setup-script",
            "running",
            None,
            None,
            &["installing"],
        )],
        None,
    ));
    assert_eq!(
        shown_tail(&running_tail),
        Some(vec![
            "installing".into(),
            String::new(),
            String::new(),
            String::new()
        ])
    );

    assert_eq!(format_elapsed(9), "9s");
    assert_eq!(format_elapsed(65), "1m 05s");
    assert_eq!(format_elapsed(3720), "1h 02m");
}

/// Plays T3 for one thread: its projection, a `subscribeWorktreeSetup` stream the test feeds,
/// and a record of every call.
struct FakeBackend {
    runs: Vec<Value>,
    calls: Mutex<Vec<(String, Value, Access)>>,
    commands: Mutex<Vec<Value>>,
    setups: Mutex<Vec<mpsc::UnboundedSender<Result<Value>>>>,
}

impl FakeBackend {
    fn new(runs: Vec<Value>) -> Arc<Self> {
        Arc::new(Self {
            runs,
            calls: Mutex::default(),
            commands: Mutex::default(),
            setups: Mutex::default(),
        })
    }

    fn calls(&self, tag: &str) -> Vec<(Value, Access)> {
        self.calls
            .lock()
            .iter()
            .filter(|(called, _, _)| called == tag)
            .map(|(_, payload, access)| (payload.clone(), *access))
            .collect()
    }

    /// Sends to every card that follows the setup: the view's own and the test's.
    fn send(&self, value: Value) {
        let setups = self.setups.lock();
        assert!(!setups.is_empty(), "the card follows the setup");
        for setup in setups.iter() {
            setup.unbounded_send(Ok(value.clone())).unwrap();
        }
    }
}

fn ready<T: Send + 'static>(value: Result<T>) -> BoxFuture<'static, Result<T>> {
    futures::future::ready(value).boxed()
}

fn run(id: &str, status: &str, preparation: bool) -> Value {
    json!({
        "id": id, "threadId": THREAD, "ordinal": 1, "status": status, "userMessageId": "m1",
        "modelSelection": { "instanceId": "claudeAgent", "model": "claude-opus-4-5" },
        "providerInstanceId": "claudeAgent", "providerThreadId": null, "activeAttemptId": null,
        "checkpointId": null, "completedAt": null, "contextHandoffId": null,
        "requestedAt": NOW, "rootNodeId": null, "startedAt": null,
        "workspacePreparation": preparation.then(|| json!({
            "type": "worktree", "baseRef": "origin/main", "branch": "t3/retry-backoff"
        }))
    })
}

impl ThreadBackend for FakeBackend {
    fn shell_snapshot(&self) -> BoxFuture<'static, Result<OrchestrationV2ShellSnapshot>> {
        ready(Err(anyhow!("no shell here")))
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
        let thread = json!({
            "id": THREAD, "projectId": "project-1", "title": "Add retries",
            "createdAt": NOW, "updatedAt": NOW, "createdBy": "user", "creationSource": "web",
            "interactionMode": "default", "runtimeMode": "full-access",
            "modelSelection": { "instanceId": "claudeAgent", "model": "claude-opus-4-5" },
            "providerInstanceId": "claudeAgent", "activeProviderThreadId": null,
            "archivedAt": null, "deletedAt": null, "forkedFrom": null, "branch": null,
            "worktreePath": null,
            "lineage": { "rootThreadId": THREAD, "parentThreadId": null, "relationshipToParent": null }
        });
        let projection = json!({
            "thread": thread, "attempts": [], "checkpointScopes": [], "checkpoints": [],
            "contextHandoffs": [], "contextTransfers": [], "messages": [], "nodes": [],
            "plans": [], "providerSessions": [], "providerThreads": [], "providerTurns": [],
            "runs": self.runs, "runtimeRequests": [], "subagents": [], "turnItems": [],
            "visibleTurnItems": [], "updatedAt": NOW
        });
        ready(
            serde_json::from_value(json!({ "snapshotSequence": 10, "projection": projection }))
                .map_err(Into::into),
        )
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
        ready(Err(anyhow!("no turn items here")))
    }

    fn providers(&self) -> BoxFuture<'static, Result<Vec<ServerProvider>>> {
        ready(Ok(Vec::new()))
    }

    fn dispatch(
        &self,
        command: OrchestrationV2Command,
    ) -> BoxFuture<'static, Result<OrchestrationV2DispatchCommandResult>> {
        self.commands
            .lock()
            .push(serde_json::to_value(command).unwrap());
        ready(Ok(OrchestrationV2DispatchCommandResult { sequence: 11 }))
    }

    fn launch_thread(
        &self,
        _: OrchestrationV2ThreadLaunchInput,
    ) -> BoxFuture<'static, Result<OrchestrationV2ThreadLaunchResult>> {
        ready(Err(anyhow!("the card never launches threads")))
    }

    fn call_raw(
        &self,
        tag: &str,
        payload: Value,
        access: Access,
    ) -> BoxFuture<'static, Result<Value>> {
        self.calls.lock().push((tag.to_string(), payload, access));
        ready(Ok(json!({ "cancelled": true })))
    }

    fn subscribe_raw(
        &self,
        tag: &str,
        payload: Value,
        access: Access,
    ) -> Result<BoxStream<'static, Result<Value>>> {
        self.calls.lock().push((tag.to_string(), payload, access));
        let (tx, rx) = mpsc::unbounded();
        self.setups.lock().push(tx);
        Ok(rx.boxed())
    }

    fn upload(&self, _: AttachmentUpload) -> BoxFuture<'static, Result<()>> {
        ready(Err(anyhow!("no uploads here")))
    }
}

fn open_card(
    backend: Arc<FakeBackend>,
    cx: &mut TestAppContext,
) -> (Entity<WorktreeSetupCard>, &mut VisualTestContext) {
    cx.update(|cx| {
        let settings_store = SettingsStore::test(cx);
        cx.set_global(settings_store);
        theme_settings::init(theme::LoadThemes::JustBase, cx);
    });
    let (view, cx) = cx.add_window_view(|window, cx| {
        ThreadView::new(
            backend,
            None,
            Some(THREAD.into()),
            "Add retries".into(),
            window,
            cx,
        )
    });
    cx.run_until_parked();
    let card = view.update_in(cx, |_, window, cx| {
        let weak = cx.entity().downgrade();
        cx.new(|cx| WorktreeSetupCard::new(weak, window, cx))
    });
    cx.run_until_parked();
    (card, cx)
}

fn shows(card: &Entity<WorktreeSetupCard>, cx: &mut VisualTestContext) -> bool {
    card.update(cx, |card, cx| card.render_card(px(400.), cx).is_some())
}

#[gpui::test]
async fn a_preparing_run_shows_its_setup_and_can_cancel_it(cx: &mut TestAppContext) {
    let backend = FakeBackend::new(vec![run("run-1", "preparing", true)]);
    let (card, cx) = open_card(backend.clone(), cx);
    // The view's own card and the test's.
    assert_eq!(
        backend.calls("subscribeWorktreeSetup"),
        vec![(json!({ "threadId": THREAD }), Access::Read); 2]
    );
    // T3 sends `null` when nothing is being set up.
    backend.send(Value::Null);
    cx.run_until_parked();
    assert!(!shows(&card, cx));

    backend.send(setup(
        "running",
        3,
        vec![
            stage("fetch", "done", None, None, &[]),
            stage("checkout", "running", None, Some(42), &[]),
        ],
        None,
    ));
    cx.run_until_parked();
    assert!(shows(&card, cx));
    // An older snapshot doesn't replace a newer one.
    backend.send(setup("cancelled", 2, Vec::new(), None));
    cx.run_until_parked();
    card.read_with(cx, |card, _| {
        assert_eq!(card.setup.as_ref().unwrap().sequence, 3)
    });

    card.update(cx, |card, cx| card.cancel(cx));
    cx.run_until_parked();
    assert_eq!(
        backend.calls("worktreeSetup.cancel"),
        [(json!({ "threadId": THREAD }), Access::Operate)]
    );

    // Done, with nothing failed: the card goes.
    backend.send(setup(
        "done",
        9,
        vec![stage("agent", "done", None, None, &[])],
        None,
    ));
    cx.run_until_parked();
    assert!(!shows(&card, cx));
    assert!(backend.commands.lock().is_empty());
}

#[gpui::test]
async fn a_failed_preparation_can_be_retried(cx: &mut TestAppContext) {
    // T3 forgot the setup itself (it keeps one 30 seconds), but the run's preparation failed.
    let backend = FakeBackend::new(vec![run("run-1", "failed", true)]);
    let (card, cx) = open_card(backend.clone(), cx);
    assert!(backend.calls.lock().is_empty(), "nothing is being prepared");
    assert!(shows(&card, cx));

    card.update(cx, |card, cx| card.retry(cx));
    cx.run_until_parked();
    let commands = backend.commands.lock().clone();
    assert_eq!(commands.len(), 1);
    let mut command = commands[0].clone();
    let command_id = command
        .as_object_mut()
        .unwrap()
        .remove("commandId")
        .expect("a command id");
    assert!(command_id.as_str().is_some_and(|id| !id.is_empty()));
    assert_eq!(
        command,
        json!({ "type": "prepared-run.retry", "threadId": THREAD, "runId": "run-1" })
    );
}

#[gpui::test]
async fn other_failed_runs_show_no_card(cx: &mut TestAppContext) {
    let backend = FakeBackend::new(vec![run("run-1", "failed", false)]);
    let (card, cx) = open_card(backend.clone(), cx);
    assert!(!shows(&card, cx));
    assert!(backend.calls.lock().is_empty());
}

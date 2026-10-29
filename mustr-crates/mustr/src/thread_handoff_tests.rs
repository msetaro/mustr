use super::*;
use crate::thread_git::tests::{NOW, RecordingBackend, THREAD, init_test, shell, shell_thread};
use gpui::{Entity, TestAppContext, VisualTestContext};
use pretty_assertions::assert_eq;
use serde_json::{Value, json};
use workspace::MultiWorkspace;

const FORK: &str = "thread-2";

fn fork_thread(forked_from: Value) -> Value {
    shell_thread(json!({
        "id": FORK, "title": "Try a queue instead",
        "lineage": { "rootThreadId": THREAD, "parentThreadId": THREAD, "relationshipToParent": "fork" },
        "forkedFrom": forked_from,
    }))
}

fn run(id: &str, ordinal: i64, status: &str) -> Value {
    json!({
        "id": id, "threadId": FORK, "ordinal": ordinal, "status": status, "userMessageId": "m1",
        "modelSelection": { "instanceId": "claudeAgent", "model": "claude-opus-5-5" },
        "providerInstanceId": "claudeAgent", "providerThreadId": null, "activeAttemptId": null,
        "checkpointId": null, "completedAt": null, "contextHandoffId": null,
        "requestedAt": NOW, "rootNodeId": null, "startedAt": null, "workspacePreparation": null
    })
}

fn runs(runs: Vec<Value>) -> Vec<OrchestrationV2Run> {
    serde_json::from_value(Value::Array(runs)).unwrap()
}

/// The fork's view state, with `runs`.
fn fork_state(runs: Vec<Value>) -> ThreadState {
    let projection = json!({
        "thread": fork_thread(json!({ "type": "run", "threadId": THREAD, "runId": "run-0" })),
        "attempts": [], "checkpointScopes": [], "checkpoints": [], "contextHandoffs": [],
        "contextTransfers": [], "messages": [], "nodes": [], "plans": [], "providerSessions": [],
        "providerThreads": [], "providerTurns": [], "runs": runs, "runtimeRequests": [],
        "subagents": [], "turnItems": [], "visibleTurnItems": [], "updatedAt": NOW
    });
    ThreadState::new(serde_json::from_value(projection).unwrap(), 3)
}

#[test]
fn forks_merge_back_into_the_thread_they_came_from() {
    let shell_of =
        |value: Value| -> OrchestrationV2ThreadShell { serde_json::from_value(value).unwrap() };
    // Forked from a run: into that run's thread.
    let fork = shell_of(fork_thread(
        json!({ "type": "run", "threadId": "thread-0", "runId": "run-0" }),
    ));
    assert_eq!(merge_back_target(&fork).as_deref(), Some("thread-0"));
    // Otherwise into its parent.
    let fork = shell_of(fork_thread(Value::Null));
    assert_eq!(merge_back_target(&fork).as_deref(), Some(THREAD));
    // Subagents and threads of their own don't merge back.
    let subagent = shell_of(shell_thread(json!({
        "lineage": { "rootThreadId": THREAD, "parentThreadId": THREAD, "relationshipToParent": "subagent" }
    })));
    assert_eq!(merge_back_target(&subagent), None);
    assert_eq!(merge_back_target(&shell_of(shell_thread(json!({})))), None);
}

#[test]
fn the_latest_finished_run_merges_back_unless_a_newer_one_runs() {
    let id = |runs: &[OrchestrationV2Run]| latest_merge_back_run(runs).map(|run| run.id.clone());
    // The newest finished run, or one capturing its checkpoint.
    assert_eq!(
        id(&runs(vec![
            run("run-1", 1, "completed"),
            run("run-2", 2, "waiting")
        ]))
        .as_deref(),
        Some("run-2")
    );
    // Failed and interrupted runs don't count, and don't block an older finished one.
    assert_eq!(
        id(&runs(vec![
            run("run-1", 1, "completed"),
            run("run-2", 2, "failed")
        ]))
        .as_deref(),
        Some("run-1")
    );
    // A newer run under way blocks it; a queued one doesn't.
    assert_eq!(
        id(&runs(vec![
            run("run-1", 1, "completed"),
            run("run-2", 2, "running")
        ])),
        None
    );
    assert_eq!(
        id(&runs(vec![
            run("run-1", 1, "completed"),
            run("run-2", 2, "queued")
        ]))
        .as_deref(),
        Some("run-1")
    );
    assert_eq!(id(&runs(vec![run("run-1", 1, "running")])), None);
    assert_eq!(id(&[]), None);
}

#[test]
fn merge_back_sends_t3s_command() {
    assert_eq!(
        serde_json::to_value(merge_back_command(
            FORK,
            THREAD,
            "run-1",
            "command-1".into()
        ))
        .unwrap(),
        json!({
            "type": "thread.merge_back", "commandId": "command-1", "createdBy": "user",
            "creationSource": "web", "sourceThreadId": FORK, "targetThreadId": THREAD,
            "sourcePoint": { "type": "run", "runId": "run-1" }
        })
    );
}

async fn workspace(cx: &mut TestAppContext) -> (Entity<Workspace>, &mut VisualTestContext) {
    let fs = project::FakeFs::new(cx.executor());
    let project = project::Project::test(fs, [], cx).await;
    let (multi_workspace, cx) =
        cx.add_window_view(|window, cx| MultiWorkspace::test_new(project.clone(), window, cx));
    let workspace =
        multi_workspace.read_with(cx, |multi_workspace, _| multi_workspace.workspace().clone());
    (workspace, cx)
}

#[gpui::test]
async fn a_fork_merges_back_through_t3(cx: &mut TestAppContext) {
    let backend = RecordingBackend::with_shell(shell(vec![
        shell_thread(json!({})),
        fork_thread(json!({ "type": "run", "threadId": THREAD, "runId": "run-0" })),
    ]));
    init_test(backend.clone(), cx);
    let (workspace, cx) = workspace(cx).await;
    let shells = cx.update(|_, cx| ThreadShells::global(cx));
    cx.run_until_parked();

    // Before the fork has finished a run, there's nothing to merge.
    let state = fork_state(vec![run("run-1", 1, "running")]);
    let merge = shells
        .read_with(cx, |shells, _| {
            MergeBack::of_thread(FORK, Some(&state), shells)
        })
        .unwrap();
    assert_eq!(merge.run_id, None);
    assert_eq!(
        merge.tooltip(),
        "Complete a run in this fork before merging it back"
    );
    // The parent isn't a fork, so it has no merge back.
    assert_eq!(
        shells.read_with(cx, |shells, _| MergeBack::of_thread(THREAD, None, shells)),
        None
    );

    let state = fork_state(vec![run("run-1", 1, "completed")]);
    let merge = shells
        .read_with(cx, |shells, _| {
            MergeBack::of_thread(FORK, Some(&state), shells)
        })
        .unwrap();
    assert_eq!(
        merge,
        MergeBack {
            source_thread_id: FORK.to_string(),
            target_thread_id: THREAD.to_string(),
            target_title: Some("Add retries".to_string()),
            run_id: Some("run-1".to_string()),
        }
    );
    assert!(merge.tooltip().contains("“Add retries”"));

    let task = cx.update(|window, cx| merge.clone().run(workspace.downgrade(), window, cx));
    task.await;
    cx.run_until_parked();
    let commands = backend.commands();
    assert_eq!(
        commands,
        [json!({
            "type": "thread.merge_back", "commandId": commands[0]["commandId"],
            "createdBy": "user", "creationSource": "web", "sourceThreadId": FORK,
            "targetThreadId": THREAD, "sourcePoint": { "type": "run", "runId": "run-1" }
        })]
    );

    // Refused writes are refused before they're sent.
    backend.blocked.lock().insert(
        Access::Operate,
        "mustr is in a dry run, so it sends nothing to agents".to_string(),
    );
    let task = cx.update(|window, cx| merge.run(workspace.downgrade(), window, cx));
    task.await;
    assert_eq!(backend.commands().len(), 1);
}

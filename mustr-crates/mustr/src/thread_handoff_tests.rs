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

fn provider(instance: &str, driver: &str, name: &str, model: &str, extra: Value) -> Value {
    let mut provider = json!({
        "instanceId": instance, "driver": driver, "displayName": name,
        "enabled": true, "installed": true, "status": "ready",
        "auth": { "status": "authenticated" }, "checkedAt": NOW, "version": "1.0.0",
        "models": [{
            "slug": model, "name": model, "isCustom": false, "isDefault": true,
            "capabilities": { "optionDescriptors": [] }
        }]
    });
    provider
        .as_object_mut()
        .unwrap()
        .extend(extra.as_object().unwrap().clone());
    provider
}

fn providers(extra_codex: Value) -> Vec<ServerProvider> {
    serde_json::from_value(json!([
        provider(
            "claudeAgent",
            "claudeAgent",
            "Claude Code",
            "claude-opus-5-5",
            json!({})
        ),
        provider("codex", "codex", "Codex", "gpt-5.5", extra_codex),
    ]))
    .unwrap()
}

fn user_message(run_id: &str) -> Value {
    json!({
        "type": "user_message", "id": "i1", "threadId": FORK, "runId": run_id, "nodeId": null,
        "providerThreadId": null, "providerTurnId": null, "parentItemId": null,
        "nativeItemRef": null, "ordinal": 1, "status": "completed", "title": null,
        "startedAt": null, "completedAt": null, "updatedAt": NOW,
        "messageId": "message-1", "text": "Add retries", "attachments": [],
        "createdBy": "user", "creationSource": "web", "inputIntent": "turn_start"
    })
}

/// A thread whose agent is now `selection`, with `runs` and, unless `empty`, a message.
fn thread_state(selection: Value, runs: Vec<Value>, empty: bool) -> ThreadState {
    let mut thread = fork_thread(Value::Null);
    thread["modelSelection"] = selection;
    let items = if empty {
        vec![]
    } else {
        vec![user_message("run-1")]
    };
    let visible = items
        .iter()
        .enumerate()
        .map(|(position, item)| {
            json!({ "position": position, "visibility": "local", "sourceThreadId": FORK,
                    "sourceItemId": item["id"], "item": item })
        })
        .collect::<Vec<_>>();
    let projection = json!({
        "thread": thread,
        "attempts": [], "checkpointScopes": [], "checkpoints": [], "contextHandoffs": [],
        "contextTransfers": [], "messages": [], "nodes": [], "plans": [], "providerSessions": [],
        "providerThreads": [], "providerTurns": [], "runs": runs, "runtimeRequests": [],
        "subagents": [], "turnItems": items, "visibleTurnItems": visible, "updatedAt": NOW
    });
    ThreadState::new(serde_json::from_value(projection).unwrap(), 3)
}

fn started(id: &str, status: &str) -> Value {
    let mut run = run(id, 1, status);
    run["startedAt"] = json!(NOW);
    run
}

#[test]
fn switching_agents_makes_the_next_message_hand_off() {
    let codex = json!({ "instanceId": "codex", "model": "gpt-5.5" });
    let claude = json!({ "instanceId": "claudeAgent", "model": "claude-opus-5-5" });
    // The last run was Claude's; the thread is set to Codex now.
    let state = thread_state(codex.clone(), vec![started("run-1", "completed")], false);
    assert_eq!(
        pending_handoff(&state, &providers(json!({}))),
        Some(PendingHandoff {
            from: "Claude Code · claude-opus-5-5".to_string(),
            to: "Codex · gpt-5.5".to_string(),
        })
    );
    // Nothing to hand over yet, or the same agent: no handoff.
    let empty = thread_state(codex.clone(), vec![started("run-1", "completed")], true);
    assert_eq!(pending_handoff(&empty, &providers(json!({}))), None);
    let same = thread_state(claude.clone(), vec![started("run-1", "completed")], false);
    assert_eq!(pending_handoff(&same, &providers(json!({}))), None);
    // Another model of an agent that can't switch in place hands off too.
    let other_model = json!({ "instanceId": "claudeAgent", "model": "claude-sonnet-5" });
    let fixed = providers(json!({}))
        .into_iter()
        .map(|mut provider| {
            provider.requires_new_thread_for_model_change = Some(true);
            provider
        })
        .collect::<Vec<_>>();
    let state = thread_state(other_model, vec![started("run-1", "completed")], false);
    assert!(pending_handoff(&state, &fixed).is_some());
    assert_eq!(pending_handoff(&state, &providers(json!({}))), None);

    // The thread's agent can hand off when nothing runs; the others are where it can go.
    let idle = thread_state(claude.clone(), vec![], false);
    assert!(supports_handoff(&idle));
    let running = thread_state(claude, vec![started("run-1", "running")], false);
    assert!(!supports_handoff(&running));
    let targets = handoff_targets(&idle, &providers(json!({})));
    assert_eq!(
        targets
            .iter()
            .map(|agent| agent.name.as_str())
            .collect::<Vec<_>>(),
        ["Codex"]
    );
    assert_eq!(
        serde_json::to_value(handoff_command(FORK, &targets[0], "command-1".into()).unwrap())
            .unwrap(),
        json!({
            "type": "thread.model-selection.set", "commandId": "command-1", "threadId": FORK,
            "modelSelection": { "instanceId": "codex", "model": "gpt-5.5" }
        })
    );
}

#[gpui::test]
async fn hand_off_switches_the_threads_agent_through_t3(cx: &mut TestAppContext) {
    let backend = RecordingBackend::with_shell(shell(vec![]));
    init_test(backend.clone(), cx);
    let (workspace, cx) = workspace(cx).await;
    let idle = thread_state(
        json!({ "instanceId": "claudeAgent", "model": "claude-opus-5-5" }),
        vec![],
        false,
    );
    let codex = handoff_targets(&idle, &providers(json!({}))).remove(0);
    cx.update(|_, cx| hand_off(FORK.to_string(), codex.clone(), workspace.downgrade(), cx));
    cx.run_until_parked();
    let commands = backend.commands();
    assert_eq!(
        commands,
        [json!({
            "type": "thread.model-selection.set", "commandId": commands[0]["commandId"],
            "threadId": FORK, "modelSelection": { "instanceId": "codex", "model": "gpt-5.5" }
        })]
    );
    // A dry run refuses it before it's sent.
    backend
        .blocked
        .lock()
        .insert(Access::Operate, "mustr is in a dry run".to_string());
    cx.update(|_, cx| hand_off(FORK.to_string(), codex, workspace.downgrade(), cx));
    cx.run_until_parked();
    assert_eq!(backend.commands().len(), 1);
}

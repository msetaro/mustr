//! Reordering the queue (`queued-run.reorder`), Send now (`queued-run.start`, T3 0.1.3), and the
//! goal line, against the recording backend.

use super::*;
use crate::{mentions::version_at_least, model::GoalLine};
use pretty_assertions::assert_eq;

/// An agent's capabilities, all off but `turns` ones the test names.
fn capabilities(turns: &[&str]) -> Value {
    let group = |names: &[&str], on: &[&str]| {
        Value::Object(
            names
                .iter()
                .map(|name| (name.to_string(), json!(on.contains(name))))
                .collect(),
        )
    };
    let mut turn_capabilities = group(
        &[
            "emitsTurnCompleted",
            "emitsTurnStarted",
            "exposesNativeTurnId",
            "supportsActiveSteering",
            "supportsInterrupt",
            "supportsQueuedMessages",
            "supportsSteeringByInterruptRestart",
        ],
        turns,
    );
    turn_capabilities["terminalStatusQuality"] = json!("strong");
    let mut context = group(
        &[
            "acceptsDeveloperContext",
            "acceptsSyntheticUserContext",
            "acceptsSystemContext",
            "canConsumeHandoffSummaries",
            "canGenerateSummaries",
            "supportsDeltaHandoff",
            "supportsFullThreadHandoff",
        ],
        &[],
    );
    context["maxRecommendedHandoffChars"] = Value::Null;
    json!({
        "approvals": group(&[
            "approvalCallbacksAreLiveOnly", "approvalsCanOriginateFromSubagents",
            "approvalsHaveNativeRequestIds", "supportsApplyPatchApproval",
            "supportsCommandApproval", "supportsFileChangeApproval", "supportsFileReadApproval",
        ], &[]),
        "checkpointing": group(&[
            "appCanCheckpointFilesystem", "providerCanReadConversationSnapshot",
            "providerCanRollbackConversation", "providerRollbackReturnsSnapshot",
            "supportsNestedCheckpointScopes",
        ], &[]),
        "context": context,
        "identity": {
            "nativeItemIds": "strong", "nativeRequestIds": "strong",
            "nativeThreadIds": "strong", "nativeTurnIds": "strong"
        },
        "planning": group(&[
            "emitsPlanUpdated", "emitsProposedPlan", "emitsTodoList", "planDeltasHaveItemIds",
            "supportsStructuredQuestions",
        ], &[]),
        "sessions": group(&[
            "pendingRequestsSurviveRestart", "supportsModelSwitchInSession",
            "supportsMultipleProviderThreadsPerSession", "supportsProviderSwitchingViaHandoff",
            "supportsRuntimeModeSwitchInSession",
        ], &[]),
        "streaming": group(&[
            "emitsMessageCompleted", "streamsAssistantText", "streamsPlanText",
            "streamsReasoning", "streamsToolOutput",
        ], &[]),
        "subagents": group(&[
            "canCloseSubagents", "canForkSubagentThread", "canWaitForSubagents",
            "emitsSubagentLifecycle", "exposesSubagentThreadIds", "supportsSubagents",
        ], &[]),
        "threads": group(&[
            "canCreateEmptyThread", "canForkFromSubagentThread", "canReadThreadSnapshot",
            "canRollbackThread", "exposesNativeThreadId", "canForkThread", "canForkFromTurn",
        ], &[]),
        "tools": group(&[
            "emitsToolCompleted", "emitsToolOutput", "emitsToolStarted", "exposesToolItemIds",
            "supportsDynamicToolCallbacks", "supportsMcpTools",
        ], &[]),
        "turns": turn_capabilities
    })
}

/// A projection whose thread has an agent session with `turns` capabilities, and a goal when
/// given.
fn with_session(projection: Projection, turns: &[&str], goal: Option<Value>) -> Value {
    let mut json = projection.json();
    json["thread"]["activeProviderThreadId"] = json!("provider-thread-1");
    let mut provider_thread = json!({
        "id": "provider-thread-1", "appThreadId": THREAD, "driver": "claudeAgent",
        "providerInstanceId": "claudeAgent", "providerSessionId": "session-1", "status": "idle",
        "createdAt": NOW, "updatedAt": NOW, "firstRunOrdinal": 1, "lastRunOrdinal": 2,
        "forkedFrom": null, "handoffIds": [], "nativeConversationHeadRef": null,
        "nativeThreadRef": null, "ownerNodeId": null
    });
    if let Some(goal) = goal {
        provider_thread["goal"] = goal;
    }
    json["providerThreads"] = json!([provider_thread]);
    json["providerSessions"] = json!([{
        "id": "session-1", "driver": "claudeAgent", "providerInstanceId": "claudeAgent",
        "cwd": "/tmp/worktrees/retry-backoff", "model": "claude-sonnet-4-5", "status": "ready",
        "lastError": null, "capabilities": capabilities(turns), "createdAt": NOW, "updatedAt": NOW
    }]);
    json
}

fn backend_for(projection: Value, sequence: i64) -> Arc<FakeBackend> {
    let backend = FakeBackend::default();
    *backend.snapshot.lock() = Some(json!({
        "snapshotSequence": sequence,
        "projection": projection
    }));
    Arc::new(backend)
}

/// A running thread with two messages queued behind it.
fn two_queued() -> Projection {
    let mut projection = running_thread();
    projection.runtime_requests.clear();
    projection.items.pop();
    let mut first = run("run-3", 3, "queued", "m3");
    first["queuePosition"] = json!(1);
    let mut second = run("run-4", 4, "queued", "m4");
    second["queuePosition"] = json!(2);
    projection.runs.truncate(2);
    projection.runs.extend([first, second]);
    projection
        .messages
        .push(message("m4", "run-4", "And the changelog"));
    projection
}

fn click(selector: &'static str, cx: &mut VisualTestContext) {
    let bounds = cx
        .debug_bounds(selector)
        .unwrap_or_else(|| panic!("{selector} shows"));
    cx.simulate_click(bounds.center(), gpui::Modifiers::none());
    cx.run_until_parked();
}

#[gpui::test]
async fn the_queue_moves_with_its_arrows(cx: &mut TestAppContext) {
    let backend = backend_for(
        with_session(two_queued(), &["supportsQueuedMessages"], None),
        20,
    );
    let (view, cx) = open_view(backend.clone(), Some(THREAD), cx);
    cx.simulate_resize(gpui::size(gpui::px(1200.), gpui::px(900.)));
    cx.run_until_parked();
    view.read_with(cx, |view, _| {
        let queued = view.thread_state().unwrap().queued_messages();
        assert_eq!(
            queued
                .iter()
                .map(|queued| queued.run_id.as_str())
                .collect::<Vec<_>>(),
            ["run-3", "run-4"]
        );
    });

    // Later goes after the next one: here, to the end.
    click("thread-queued-down-run-3", cx);
    // Earlier goes before the one above.
    click("thread-queued-up-run-4", cx);
    assert_eq!(
        backend
            .commands()
            .into_iter()
            .map(without_ids)
            .collect::<Vec<_>>(),
        [
            json!({
                "type": "queued-run.reorder",
                "threadId": THREAD,
                "runId": "run-3",
                "beforeRunId": null
            }),
            json!({
                "type": "queued-run.reorder",
                "threadId": THREAD,
                "runId": "run-4",
                "beforeRunId": "run-3"
            }),
        ]
    );

    // A drop lands where it's dropped: dragging the first onto the second puts it after it.
    view.update_in(cx, |view, window, cx| view.move_queued(0, 1, window, cx));
    cx.run_until_parked();
    assert_eq!(backend.commands()[2]["beforeRunId"], Value::Null);
    // Running, a queued message can still steer the agent.
    assert!(cx.debug_bounds("thread-start-queued-run-3").is_none());
}

#[gpui::test]
async fn agents_without_a_queue_order_show_no_arrows(cx: &mut TestAppContext) {
    let backend = backend_for(with_session(two_queued(), &[], None), 20);
    let (_, cx) = open_view(backend, Some(THREAD), cx);
    cx.simulate_resize(gpui::size(gpui::px(1200.), gpui::px(900.)));
    cx.run_until_parked();
    assert!(cx.debug_bounds("thread-queued-run-3").is_some());
    assert!(cx.debug_bounds("thread-queued-up-run-4").is_none());
}

/// A thread whose run stopped, with a held message queued.
fn held_queue() -> Projection {
    let mut projection = finished_thread();
    let mut held = run("run-2", 2, "queued", "m2");
    held["queueHeld"] = json!(true);
    projection.runs.push(held);
    projection
        .messages
        .push(message("m2", "run-2", "Then update the docs"));
    projection
}

#[gpui::test]
async fn send_now_starts_a_queued_message_on_servers_that_have_it(cx: &mut TestAppContext) {
    let backend = FakeBackend::with_thread(held_queue(), 10);
    let (view, cx) = open_view(backend.clone(), Some(THREAD), cx);
    cx.simulate_resize(gpui::size(gpui::px(1200.), gpui::px(900.)));
    view.update(cx, |view, cx| {
        view.composer_state.menu.set_server_version("0.1.3");
        cx.notify();
    });
    cx.run_until_parked();

    click("thread-start-queued-run-2", cx);
    assert_eq!(
        backend
            .commands()
            .into_iter()
            .map(without_ids)
            .collect::<Vec<_>>(),
        [json!({ "type": "queued-run.start", "threadId": THREAD, "runId": "run-2" })]
    );
}

#[gpui::test]
async fn older_servers_have_no_send_now(cx: &mut TestAppContext) {
    let backend = FakeBackend::with_thread(held_queue(), 10);
    let (view, cx) = open_view(backend.clone(), Some(THREAD), cx);
    cx.simulate_resize(gpui::size(gpui::px(1200.), gpui::px(900.)));
    cx.run_until_parked();
    // It reads T3's config to know.
    assert_eq!(backend.raw_calls.lock()[0].tag, "server.getConfig");
    assert!(cx.debug_bounds("thread-start-queued-run-2").is_none());
    view.update(cx, |view, cx| {
        view.composer_state.menu.set_server_version("0.1.1");
        cx.notify();
    });
    cx.run_until_parked();
    assert!(cx.debug_bounds("thread-queued-run-2").is_some());
    assert!(cx.debug_bounds("thread-start-queued-run-2").is_none());
}

#[test]
fn versions_compare_as_releases() {
    assert!(version_at_least("0.1.3", (0, 1, 3)));
    assert!(version_at_least("0.1.4-nightly.20261028.3841", (0, 1, 3)));
    assert!(version_at_least("v0.2.0", (0, 1, 3)));
    assert!(!version_at_least("0.1.1", (0, 1, 3)));
    assert!(!version_at_least("next", (0, 1, 3)));
}

// The goal line.

fn goal(status: &str, fields: Value) -> Value {
    let mut goal = json!({ "objective": "Get the webhook tests green", "status": status });
    goal.as_object_mut()
        .unwrap()
        .extend(fields.as_object().cloned().unwrap_or_default());
    goal
}

#[test]
fn the_goal_line_says_what_t3s_does() {
    let line =
        |goal: Value, working: bool| GoalLine::new(&serde_json::from_value(goal).unwrap(), working);
    assert_eq!(
        line(
            goal(
                "active",
                json!({ "tokensUsed": 12_400, "tokenBudget": 1_500_000, "timeUsedSeconds": 3_725, "checks": 1 })
            ),
            true
        ),
        GoalLine {
            title: "Pursuing goal".into(),
            objective: "Get the webhook tests green".into(),
            usage: Some("12k / 1.5m tokens · 1h 2m 5s · 1 check".into()),
            can_resume: false,
        }
    );
    // Idle, an active goal is only set; paused, it can resume; done, it can't.
    assert_eq!(line(goal("active", json!({})), false).title, "Goal set");
    let paused = line(goal("paused", json!({ "timeUsedSeconds": 30 })), false);
    assert_eq!(
        (paused.title.as_str(), paused.usage, paused.can_resume),
        ("Goal paused", None, true)
    );
    assert!(!line(goal("complete", json!({})), false).can_resume);
    assert_eq!(
        line(
            goal("budget_limited", json!({ "tokensUsed": 2_000_000 })),
            false
        )
        .usage,
        Some("2m tokens".into())
    );
}

#[gpui::test]
async fn the_goal_line_resumes_and_clears_the_goal(cx: &mut TestAppContext) {
    let backend = backend_for(
        with_session(
            finished_thread(),
            &[],
            Some(goal("paused", json!({ "checks": 2 }))),
        ),
        10,
    );
    let (view, cx) = open_view(backend.clone(), Some(THREAD), cx);
    cx.simulate_resize(gpui::size(gpui::px(1200.), gpui::px(900.)));
    cx.run_until_parked();
    assert!(cx.debug_bounds("thread-goal").is_some());
    view.read_with(cx, |view, _| {
        let state = view.thread_state().unwrap();
        assert_eq!(
            GoalLine::new(state.goal().unwrap(), false).usage.as_deref(),
            Some("2 checks")
        );
    });

    click("thread-goal-resume", cx);
    click("thread-goal-clear", cx);
    let goal_command = |text: &str| {
        json!({
            "type": "message.dispatch",
            "createdBy": "user",
            "creationSource": "web",
            "threadId": THREAD,
            "text": text,
            "attachments": [],
            "modelSelection": model_selection(),
            "deliveryIntent": "auto",
            "dispatchMode": { "type": "start_immediately" }
        })
    };
    assert_eq!(
        backend
            .commands()
            .into_iter()
            .map(without_ids)
            .collect::<Vec<_>>(),
        [goal_command("/goal resume"), goal_command("/goal clear")]
    );
}

#[gpui::test]
async fn threads_without_a_goal_have_no_goal_line(cx: &mut TestAppContext) {
    let backend = FakeBackend::with_thread(finished_thread(), 10);
    let (_, cx) = open_view(backend, Some(THREAD), cx);
    cx.simulate_resize(gpui::size(gpui::px(1200.), gpui::px(900.)));
    cx.run_until_parked();
    assert!(cx.debug_bounds("thread-goal").is_none());
}

#[gpui::test]
async fn what_waits_is_above_the_composer_in_t3s_order(cx: &mut TestAppContext) {
    // A run waiting on an approval with a message queued behind it, a goal, background work,
    // and a usage limit.
    let backend = backend_for(
        with_session(running_thread(), &[], Some(goal("active", json!({})))),
        20,
    );
    let mut shell = shell_json();
    let thread = &mut shell["threads"][1];
    thread["status"] = json!("failed");
    thread["lastErrorClass"] = json!("usage_limit");
    thread["latestRunId"] = json!("run-2");
    thread["latestRunCompletedAt"] = json!(NOW);
    thread["usageLimitResetAt"] = json!("2099-01-01T15:40:00.000Z");
    thread["pendingBackgroundTasks"] = json!([{
        "taskId": "task-1", "kind": "background_task",
        "description": "Watching pull request #80"
    }]);
    *backend.shell.lock() = Some(shell);
    // The app's shell follows the installed backend.
    cx.update(|cx| crate::set_backend(backend.clone(), cx));
    let (_, cx) = open_view(backend, Some(THREAD), cx);
    cx.simulate_resize(gpui::size(gpui::px(1200.), gpui::px(1400.)));
    cx.run_until_parked();
    let order = [
        "family-now-background",
        "thread-goal",
        "thread-edit-queued-run-3",
        "thread-limit-recovery",
        "thread-pending-request-1",
        "thread-composer",
    ];
    let tops = order
        .iter()
        .map(|selector| {
            cx.debug_bounds(selector)
                .unwrap_or_else(|| panic!("{selector} isn't rendered"))
                .top()
        })
        .collect::<Vec<_>>();
    for (pair, tops) in order.windows(2).zip(tops.windows(2)) {
        assert!(tops[0] < tops[1], "{} should be above {}", pair[0], pair[1]);
    }
    // None of it is at the top of the page, under the header.
    let above = cx.debug_bounds("thread-above-composer").unwrap();
    assert!(above.top() <= tops[0]);
    assert!(above.top() > gpui::px(400.));
}

//! Tests of going back in a thread (`history.rs`) and of its agent's status and usage
//! (`usage.rs`), against a recording backend: every write is captured as the payload T3 would
//! get, and the tests assert those payloads exactly.

use crate::{
    backend::{Access, AttachmentUpload, ReadOnlyBackend, ThreadBackend},
    history::{self, CompactContext, RestartAgentSession},
    model::{RevertTarget, ThreadState},
    thread_view::ThreadView,
    usage::{self, LimitRecovery, LimitWindow, ProviderBanner, RecoveryChange, UsageLimits},
};
use anyhow::{Result, anyhow};
use futures::{
    FutureExt as _, StreamExt as _, channel::mpsc, future::BoxFuture, stream::BoxStream,
};
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
        OrchestrationV2DispatchCommandResult, OrchestrationV2GetTurnItemInput,
        OrchestrationV2GetTurnItemResult, OrchestrationV2ShellSnapshot,
        OrchestrationV2SubscribeShellInput, OrchestrationV2SubscribeThreadInput,
        OrchestrationV2ThreadDetailSnapshot, OrchestrationV2ThreadLaunchInput,
        OrchestrationV2ThreadLaunchResult, OrchestrationV2ThreadShell, ServerProvider,
    },
};
use time::macros::datetime;
use workspace::{MultiWorkspace, Workspace};

const THREAD: &str = "thread-1";
const NOW: &str = "2026-10-27T12:00:00.000Z";
const WORKTREE: &str = "/tmp/worktrees/retry-backoff";

// Fixtures: T3's wire JSON, with every field the contracts require.

fn model_selection() -> Value {
    json!({ "instanceId": "claudeAgent", "model": "claude-sonnet-4-5" })
}

fn thread_json(fields: Value) -> Value {
    let mut thread = json!({
        "id": THREAD,
        "projectId": "project-1",
        "title": "Add retries to the webhook dispatcher",
        "createdAt": NOW,
        "updatedAt": NOW,
        "createdBy": "user",
        "creationSource": "web",
        "interactionMode": "default",
        "runtimeMode": "full-access",
        "modelSelection": model_selection(),
        "providerInstanceId": "claudeAgent",
        "activeProviderThreadId": "provider-thread-1",
        "archivedAt": null,
        "deletedAt": null,
        "forkedFrom": null,
        "branch": "t3/retry-backoff",
        "worktreePath": WORKTREE,
        "lineage": { "rootThreadId": THREAD, "parentThreadId": null, "relationshipToParent": null }
    });
    extend(&mut thread, fields);
    thread
}

fn extend(value: &mut Value, fields: Value) {
    if let (Value::Object(value), Value::Object(fields)) = (value, fields) {
        value.extend(fields);
    }
}

fn item(kind: &str, id: &str, ordinal: i64, run_id: &str, fields: Value) -> Value {
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
        "status": "completed",
        "title": null,
        "startedAt": null,
        "completedAt": null,
        "updatedAt": NOW
    });
    extend(&mut item, fields);
    item
}

fn user_message(id: &str, ordinal: i64, run_id: &str, text: &str, intent: &str) -> Value {
    item(
        "user_message",
        id,
        ordinal,
        run_id,
        json!({
            "messageId": format!("message-{id}"),
            "text": text,
            "attachments": [],
            "createdBy": "user",
            "creationSource": "web",
            "inputIntent": intent
        }),
    )
}

fn assistant_message(id: &str, ordinal: i64, run_id: &str, text: &str) -> Value {
    item(
        "assistant_message",
        id,
        ordinal,
        run_id,
        json!({ "messageId": format!("message-{id}"), "text": text, "streaming": false }),
    )
}

fn run(id: &str, ordinal: i64, status: &str) -> Value {
    json!({
        "id": id,
        "threadId": THREAD,
        "ordinal": ordinal,
        "status": status,
        "userMessageId": format!("m{ordinal}"),
        "modelSelection": model_selection(),
        "providerInstanceId": "claudeAgent",
        "providerThreadId": "provider-thread-1",
        "activeAttemptId": format!("attempt-{ordinal}"),
        "checkpointId": null,
        "completedAt": if status == "running" { Value::Null } else { json!(NOW) },
        "contextHandoffId": null,
        "requestedAt": NOW,
        "rootNodeId": null,
        "startedAt": NOW
    })
}

fn attempt(ordinal: i64) -> Value {
    json!({
        "id": format!("attempt-{ordinal}"),
        "runId": format!("run-{ordinal}"),
        "attemptOrdinal": 1,
        "reason": "initial",
        "status": "completed",
        "providerInstanceId": "claudeAgent",
        "providerThreadId": "provider-thread-1",
        "providerTurnId": format!("turn-{ordinal}"),
        "rootNodeId": "node-1",
        "startedAt": NOW,
        "completedAt": NOW
    })
}

fn provider_turn(ordinal: i64, provider_thread: &str) -> Value {
    json!({
        "id": format!("turn-{ordinal}"),
        "nodeId": "node-1",
        "ordinal": ordinal,
        "providerThreadId": provider_thread,
        "runAttemptId": format!("attempt-{ordinal}"),
        "status": "completed",
        "startedAt": NOW,
        "completedAt": NOW,
        "nativeTurnRef": null,
        "tokenUsage": { "usedTokens": 42_000, "maxTokens": 200_000, "updatedAt": NOW }
    })
}

/// A checkpoint of the thread's one scope: the thread's start without a run, then one per run.
fn checkpoint(id: &str, run_ordinal: Option<i64>) -> Value {
    json!({
        "id": id,
        "threadId": THREAD,
        "scopeId": "scope-1",
        "nodeId": "node-1",
        "runId": run_ordinal.map(|ordinal| format!("run-{ordinal}")),
        "appRunOrdinal": run_ordinal,
        "ordinalWithinScope": run_ordinal.unwrap_or(0),
        "parentCheckpointId": null,
        "ref": format!("refs/t3/checkpoints/{id}"),
        "status": "ready",
        "capturedAt": NOW,
        "files": []
    })
}

fn provider_thread(instance: &str) -> Value {
    json!({
        "id": "provider-thread-1",
        "appThreadId": THREAD,
        "driver": "claudeAgent",
        "providerInstanceId": instance,
        "providerSessionId": "session-1",
        "status": "idle",
        "createdAt": NOW,
        "updatedAt": NOW,
        "firstRunOrdinal": 1,
        "lastRunOrdinal": 2,
        "forkedFrom": null,
        "handoffIds": [],
        "nativeConversationHeadRef": null,
        "nativeThreadRef": null,
        "ownerNodeId": null
    })
}

/// An agent's capabilities, all off but forking, which `fork` turns on.
fn capabilities(fork: bool) -> Value {
    let off = |names: &[&str]| {
        Value::Object(
            names
                .iter()
                .map(|name| (name.to_string(), json!(false)))
                .collect(),
        )
    };
    let mut threads = off(&[
        "canCreateEmptyThread",
        "canForkFromSubagentThread",
        "canReadThreadSnapshot",
        "canRollbackThread",
        "exposesNativeThreadId",
    ]);
    extend(
        &mut threads,
        json!({ "canForkThread": fork, "canForkFromTurn": fork }),
    );
    let mut turns = off(&[
        "emitsTurnCompleted",
        "emitsTurnStarted",
        "exposesNativeTurnId",
        "supportsActiveSteering",
        "supportsInterrupt",
        "supportsQueuedMessages",
        "supportsSteeringByInterruptRestart",
    ]);
    extend(&mut turns, json!({ "terminalStatusQuality": "strong" }));
    let mut context = off(&[
        "acceptsDeveloperContext",
        "acceptsSyntheticUserContext",
        "acceptsSystemContext",
        "canConsumeHandoffSummaries",
        "canGenerateSummaries",
        "supportsDeltaHandoff",
        "supportsFullThreadHandoff",
    ]);
    extend(&mut context, json!({ "maxRecommendedHandoffChars": null }));
    json!({
        "approvals": off(&[
            "approvalCallbacksAreLiveOnly", "approvalsCanOriginateFromSubagents",
            "approvalsHaveNativeRequestIds", "supportsApplyPatchApproval",
            "supportsCommandApproval", "supportsFileChangeApproval", "supportsFileReadApproval",
        ]),
        "checkpointing": off(&[
            "appCanCheckpointFilesystem", "providerCanReadConversationSnapshot",
            "providerCanRollbackConversation", "providerRollbackReturnsSnapshot",
            "supportsNestedCheckpointScopes",
        ]),
        "context": context,
        "identity": {
            "nativeItemIds": "strong", "nativeRequestIds": "strong",
            "nativeThreadIds": "strong", "nativeTurnIds": "strong"
        },
        "planning": off(&[
            "emitsPlanUpdated", "emitsProposedPlan", "emitsTodoList", "planDeltasHaveItemIds",
            "supportsStructuredQuestions",
        ]),
        "sessions": off(&[
            "pendingRequestsSurviveRestart", "supportsModelSwitchInSession",
            "supportsMultipleProviderThreadsPerSession", "supportsProviderSwitchingViaHandoff",
            "supportsRuntimeModeSwitchInSession",
        ]),
        "streaming": off(&[
            "emitsMessageCompleted", "streamsAssistantText", "streamsPlanText",
            "streamsReasoning", "streamsToolOutput",
        ]),
        "subagents": off(&[
            "canCloseSubagents", "canForkSubagentThread", "canWaitForSubagents",
            "emitsSubagentLifecycle", "exposesSubagentThreadIds", "supportsSubagents",
        ]),
        "threads": threads,
        "tools": off(&[
            "emitsToolCompleted", "emitsToolOutput", "emitsToolStarted", "exposesToolItemIds",
            "supportsDynamicToolCallbacks", "supportsMcpTools",
        ]),
        "turns": turns
    })
}

fn provider_session(id: &str, fork: bool) -> Value {
    json!({
        "id": id,
        "driver": "claudeAgent",
        "providerInstanceId": "claudeAgent",
        "cwd": WORKTREE,
        "model": "claude-sonnet-4-5",
        "status": "ready",
        "lastError": null,
        "capabilities": capabilities(fork),
        "createdAt": NOW,
        "updatedAt": NOW
    })
}

struct Projection {
    thread: Value,
    items: Vec<Value>,
    runs: Vec<Value>,
    attempts: Vec<Value>,
    provider_turns: Vec<Value>,
    provider_threads: Vec<Value>,
    provider_sessions: Vec<Value>,
    checkpoints: Vec<Value>,
}

impl Projection {
    fn json(&self) -> Value {
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
            "thread": self.thread,
            "attempts": self.attempts,
            "checkpointScopes": [],
            "checkpoints": self.checkpoints,
            "contextHandoffs": [],
            "contextTransfers": [],
            "messages": [],
            "nodes": [],
            "plans": [],
            "providerSessions": self.provider_sessions,
            "providerThreads": self.provider_threads,
            "providerTurns": self.provider_turns,
            "runs": self.runs,
            "runtimeRequests": [],
            "subagents": [],
            "turnItems": self.items,
            "visibleTurnItems": visible,
            "updatedAt": NOW
        })
    }

    fn state(&self) -> ThreadState {
        ThreadState::new(
            serde_json::from_value(self.json()).expect("the projection fixture decodes"),
            10,
        )
    }
}

/// Two finished runs, each a message and an answer, with checkpoints at the thread's start and
/// after each run.
fn two_runs() -> Projection {
    Projection {
        thread: thread_json(json!({})),
        items: vec![
            user_message("u1", 1, "run-1", "Add retries with backoff", "turn_start"),
            assistant_message("a1", 2, "run-1", "Done. `withRetry` now wraps `post()`."),
            user_message("u2", 3, "run-2", "Now handle 429s", "queued_turn"),
            assistant_message("a2", 4, "run-2", "Handled: 429s back off too."),
        ],
        runs: vec![run("run-1", 1, "completed"), run("run-2", 2, "completed")],
        attempts: vec![attempt(1), attempt(2)],
        provider_turns: vec![
            provider_turn(1, "provider-thread-1"),
            provider_turn(2, "provider-thread-1"),
        ],
        provider_threads: vec![provider_thread("claudeAgent")],
        provider_sessions: vec![provider_session("session-1", true)],
        checkpoints: vec![
            checkpoint("checkpoint-0", None),
            checkpoint("checkpoint-1", Some(1)),
            checkpoint("checkpoint-2", Some(2)),
        ],
    }
}

fn target(turn_count: i64, checkpoint: &str) -> RevertTarget {
    RevertTarget {
        turn_count,
        checkpoint_id: checkpoint.to_string(),
        scope_id: "scope-1".to_string(),
    }
}

fn sorted_targets(state: &ThreadState) -> Vec<(String, RevertTarget)> {
    let mut targets = state.revert_targets().into_iter().collect::<Vec<_>>();
    targets.sort_by(|left, right| left.0.cmp(&right.0));
    targets
}

fn provider(fields: Value) -> Value {
    let mut provider = json!({
        "instanceId": "claudeAgent", "driver": "claudeAgent", "displayName": "Claude Code",
        "enabled": true, "installed": true, "status": "ready",
        "auth": { "status": "authenticated", "label": "Max" },
        "checkedAt": NOW, "version": "2.1.0", "models": [],
        "slashCommands": [{ "name": "compact", "description": "Compact the conversation" }]
    });
    extend(&mut provider, fields);
    provider
}

fn decode_provider(fields: Value) -> ServerProvider {
    serde_json::from_value(provider(fields)).expect("the provider fixture decodes")
}

fn wire(command: &OrchestrationV2Command) -> Value {
    serde_json::to_value(command).unwrap()
}

// The model.

#[test]
fn messages_rewind_to_the_checkpoint_before_their_run() {
    let state = two_runs().state();
    assert_eq!(
        sorted_targets(&state),
        [
            ("u1".to_string(), target(0, "checkpoint-0")),
            ("u2".to_string(), target(1, "checkpoint-1")),
        ]
    );
    // The latest settled run after the target is the one whose rolling back shows it worked.
    assert_eq!(
        state.rewind_witness(1).map(|run| run.id.as_str()),
        Some("run-2")
    );
    assert_eq!(
        state.rewind_witness(0).map(|run| run.id.as_str()),
        Some("run-2")
    );
    assert!(state.rewind_witness(2).is_none());
}

#[test]
fn some_messages_cant_be_rewound_to() {
    // A run that hasn't settled can't be rewound into.
    let mut running = two_runs();
    running.runs[1] = run("run-2", 2, "running");
    assert_eq!(
        sorted_targets(&running.state()),
        [("u1".to_string(), target(0, "checkpoint-0"))]
    );

    // Steering doesn't start a run, so there's nothing to rewind to before it.
    let mut steered = two_runs();
    steered.items[2] = user_message("u2", 3, "run-2", "Now handle 429s", "steer");
    assert_eq!(sorted_targets(&steered.state()).len(), 1);

    // Without the thread's start checkpoint, the first message has nowhere to go.
    let mut no_start = two_runs();
    no_start.checkpoints.remove(0);
    assert_eq!(
        sorted_targets(&no_start.state()),
        [("u2".to_string(), target(1, "checkpoint-1"))]
    );

    // The agent's conversation must be the thread's current agent's to follow the rewind.
    let mut other_agent = two_runs();
    other_agent.provider_threads = vec![provider_thread("codex")];
    assert!(other_agent.state().revert_targets().is_empty());

    // A run whose turn ran in another conversation can't be rewound to.
    let mut other_conversation = two_runs();
    other_conversation.provider_turns[0] = provider_turn(1, "provider-thread-0");
    assert_eq!(
        sorted_targets(&other_conversation.state()),
        [("u1".to_string(), target(0, "checkpoint-0"))]
    );

    // Older history left out: the first message shown may not be the thread's first.
    let mut paged = two_runs().state();
    paged.has_more_history = true;
    assert_eq!(
        sorted_targets(&paged),
        [("u2".to_string(), target(1, "checkpoint-1"))]
    );
}

#[test]
fn finished_answers_can_be_forked_when_the_agent_can() {
    let state = two_runs().state();
    let answer = state.items().iter().find(|item| item.id == "a1").unwrap();
    let question = state.items().iter().find(|item| item.id == "u1").unwrap();
    assert!(state.can_fork(answer));
    assert!(!state.can_fork(question));

    let mut no_fork = two_runs();
    no_fork.provider_sessions = vec![provider_session("session-1", false)];
    let state = no_fork.state();
    assert!(!state.can_fork(state.items().iter().find(|item| item.id == "a1").unwrap()));

    // Without a session to ask, T3 forks on the server.
    let mut no_session = two_runs();
    no_session.provider_sessions.clear();
    no_session.provider_threads.clear();
    let state = no_session.state();
    assert!(state.can_fork(state.items().iter().find(|item| item.id == "a1").unwrap()));
    assert!(state.has_compactable_conversation());
}

#[test]
fn session_events_keep_the_sessions_current() {
    let mut state = two_runs().state();
    let event = |sequence: i64, kind: &str, payload: Value| {
        serde_json::from_value::<OrchestrationV2ThreadStreamItem>(json!({
            "kind": "event",
            "sequence": sequence,
            "event": {
                "id": format!("event-{sequence}"), "type": kind, "threadId": THREAD,
                "occurredAt": NOW, "payload": payload
            }
        }))
        .unwrap()
    };
    state.apply(event(
        11,
        "provider-session.attached",
        provider_session("session-2", true),
    ));
    assert_eq!(state.provider_sessions().len(), 2);
    state.apply(event(
        12,
        "provider-session.detached",
        json!({ "providerSessionId": "session-1", "detachedAt": NOW, "reason": "client-requested" }),
    ));
    assert_eq!(
        state
            .provider_sessions()
            .iter()
            .map(|session| session.id.as_str())
            .collect::<Vec<_>>(),
        ["session-2"]
    );
    // A new checkpoint makes a new target.
    state.apply(event(
        13,
        "checkpoint.captured",
        checkpoint("checkpoint-0b", None),
    ));
    assert_eq!(state.revert_targets()["u1"], target(0, "checkpoint-0b"));
}

// The commands.

#[test]
fn history_commands_are_t3s() {
    assert_eq!(
        wire(&history::rollback(
            THREAD,
            &target(1, "checkpoint-1"),
            true,
            "command-1".into()
        )),
        json!({
            "type": "checkpoint.rollback",
            "commandId": "command-1",
            "threadId": THREAD,
            "scopeId": "scope-1",
            "checkpointId": "checkpoint-1",
            "restoreFiles": true
        })
    );
    assert_eq!(
        wire(&history::fork(
            THREAD,
            "thread-2",
            "run-1",
            Some("Retries fork".into()),
            "command-2".into()
        )),
        json!({
            "type": "thread.fork",
            "commandId": "command-2",
            "createdBy": "user",
            "creationSource": "web",
            "sourceThreadId": THREAD,
            "targetThreadId": "thread-2",
            "sourcePoint": { "type": "run", "runId": "run-1" },
            "title": "Retries fork"
        })
    );
    let state = two_runs().state();
    assert_eq!(
        history::restart_session(&state, "command-3")
            .iter()
            .map(wire)
            .collect::<Vec<_>>(),
        [json!({
            "type": "provider-session.detach",
            "commandId": "command-3:detach:session-1",
            "threadId": THREAD,
            "providerSessionId": "session-1",
            "reason": "client-requested"
        })]
    );
    assert_eq!(
        wire(&history::compact(
            THREAD,
            crate::commands::CommandIds {
                command_id: "command-4".into(),
                message_id: "message-4".into(),
            }
        )),
        json!({
            "type": "message.dispatch",
            "commandId": "command-4",
            "createdBy": "user",
            "creationSource": "web",
            "threadId": THREAD,
            "messageId": "message-4",
            "text": "/compact",
            "attachments": [],
            "deliveryIntent": "auto",
            "dispatchMode": { "type": "start_immediately" }
        })
    );
    assert_eq!(
        wire(&usage::update_limit_recovery(
            THREAD,
            "run-2",
            "2099-01-01T15:40:00.000Z",
            RecoveryChange::AutoResume(true),
            "command-5".into()
        )),
        json!({
            "type": "thread.metadata.update",
            "commandId": "command-5",
            "threadId": THREAD,
            "limitRecovery": {
                "runId": "run-2", "resetAt": "2099-01-01T15:40:00.000Z", "autoResume": true
            }
        })
    );
    assert_eq!(
        wire(&usage::update_limit_recovery(
            THREAD,
            "run-2",
            "2099-01-01T15:40:00.000Z",
            RecoveryChange::Snooze(false),
            "command-6".into()
        ))["limitRecovery"],
        json!({ "runId": "run-2", "resetAt": "2099-01-01T15:40:00.000Z", "snooze": false })
    );
}

// The agent's status and usage.

#[test]
fn provider_banners_say_what_is_wrong() {
    assert_eq!(usage::provider_banner(&decode_provider(json!({}))), None);
    assert_eq!(
        usage::provider_banner(&decode_provider(json!({ "status": "disabled" }))),
        None
    );
    let signed_out = usage::provider_banner(&decode_provider(json!({
        "status": "error", "auth": { "status": "unauthenticated" }
    })))
    .unwrap();
    assert_eq!(
        (
            signed_out.title.as_str(),
            signed_out.message.as_str(),
            signed_out.error
        ),
        (
            "Claude Code is signed out",
            "Sign in via the CLI to authenticate again.",
            true
        )
    );
    let broken = usage::provider_banner(&decode_provider(json!({
        "version": "2.0.1",
        "compatibilityAdvisory": {
            "status": "broken", "message": "2.0.1 drops tool results; update to 2.0.3.",
            "recommendedRange": null, "recommendedVersion": null, "latestVersionStatus": null
        }
    })))
    .unwrap();
    assert_eq!(
        broken,
        ProviderBanner {
            key: "claudeAgent\u{0}broken\u{0}2.0.1\u{0}2.0.1 drops tool results; update to 2.0.3."
                .into(),
            title: "Claude Code 2.0.1 is known to be broken".into(),
            message: "2.0.1 drops tool results; update to 2.0.3.".into(),
            error: true,
        }
    );
    let unsupported = usage::provider_banner(&decode_provider(json!({
        "compatibilityAdvisory": {
            "status": "unsupported", "message": null, "recommendedRange": null,
            "recommendedVersion": null, "latestVersionStatus": null
        }
    })))
    .unwrap();
    assert_eq!(
        (unsupported.title.as_str(), unsupported.error),
        ("Claude Code 2.1.0 is unsupported", false)
    );
    let missing = usage::provider_banner(&decode_provider(json!({
        "displayName": null, "driver": "openCode", "instanceId": "openCode",
        "status": "warning", "installed": false
    })))
    .unwrap();
    assert_eq!(
        (
            missing.title.as_str(),
            missing.message.as_str(),
            missing.error
        ),
        (
            "Open Code provider status",
            "Install Open Code where T3 Code runs, then restart the agent session.",
            false
        )
    );
}

#[test]
fn usage_limits_show_whats_left_of_each_window() {
    let now = datetime!(2026-10-27 12:00 UTC);
    let provider = decode_provider(json!({
        "usageLimits": {
            "checkedAt": NOW,
            "windows": [
                {
                    "id": "five_hour", "kind": "session", "label": "5-hour",
                    "usedPercent": 23.4, "resetsAt": "2026-10-27T14:14:00.000Z",
                    "windowDurationMins": 300
                },
                {
                    "id": "seven_day", "kind": "weekly", "label": "Weekly",
                    "usedPercent": 96, "resetsAt": "2026-10-30T15:00:00.000Z"
                },
                { "id": "opus", "kind": "other", "label": "Opus", "usedPercent": 120 }
            ],
            "externalUsage": { "label": "Manage usage", "url": "https://claude.ai/settings/usage" }
        }
    }));
    assert_eq!(
        usage::usage_limits(&provider, now),
        Some(UsageLimits {
            account: "Claude Code · Max".into(),
            notice: None,
            windows: vec![
                LimitWindow {
                    label: "5-hour".into(),
                    remaining_percent: 77,
                    resets_in: Some("resets in 2h 14m".into()),
                },
                LimitWindow {
                    label: "Weekly".into(),
                    remaining_percent: 4,
                    resets_in: Some("resets in 3d 3h".into()),
                },
                LimitWindow {
                    label: "Opus".into(),
                    remaining_percent: 0,
                    resets_in: None,
                },
            ],
            manage: Some((
                "Manage usage".into(),
                "https://claude.ai/settings/usage".into()
            )),
        })
    );
    let api_key = decode_provider(json!({
        "usageLimits": {
            "checkedAt": NOW, "windows": [],
            "unavailable": { "reason": "unsupported" }
        }
    }));
    assert_eq!(
        usage::usage_limits(&api_key, now)
            .unwrap()
            .notice
            .as_deref(),
        Some("This account has no subscription limits.")
    );
    assert_eq!(usage::usage_limits(&decode_provider(json!({})), now), None);
    // Only an agent that can run has limits to show.
    let missing = decode_provider(json!({
        "installed": false,
        "usageLimits": { "checkedAt": NOW, "windows": [] }
    }));
    assert_eq!(usage::usage_limits(&missing, now), None);
    assert_eq!(usage::format_duration(59), "0m");
    assert_eq!(usage::format_duration(90 * 60), "1h 30m");
}

fn shell_thread(fields: Value) -> Value {
    let mut thread = json!({
        "id": THREAD, "projectId": "project-1", "title": "Add retries", "status": "completed",
        "createdAt": NOW, "updatedAt": NOW, "createdBy": "user", "creationSource": "web",
        "interactionMode": "default", "runtimeMode": "full-access",
        "modelSelection": model_selection(), "providerInstanceId": "claudeAgent",
        "activeProviderThreadId": "provider-thread-1", "activeRunId": null, "archivedAt": null,
        "branch": "t3/retry-backoff", "deletedAt": null, "forkedFrom": null,
        "hasActionableProposedPlan": false, "itemCount": 4, "latestRunId": "run-2",
        "latestUserMessageAt": null, "latestVisibleMessage": null,
        "lineage": { "rootThreadId": THREAD, "parentThreadId": null, "relationshipToParent": null },
        "pendingRuntimeRequest": null, "settledAt": null, "settledOverride": null,
        "visibleItemCount": 4, "worktreePath": WORKTREE
    });
    extend(&mut thread, fields);
    thread
}

fn limited(fields: Value) -> Value {
    let mut thread = shell_thread(json!({
        "status": "failed",
        "lastErrorClass": "usage_limit",
        "latestRunCompletedAt": NOW,
        "usageLimitResetAt": "2099-01-01T15:40:00.000Z"
    }));
    extend(&mut thread, fields);
    thread
}

#[test]
fn a_usage_limit_offers_to_resume_at_reset() {
    let now = datetime!(2026-10-27 12:00 UTC);
    let decode =
        |thread: Value| serde_json::from_value::<OrchestrationV2ThreadShell>(thread).unwrap();
    assert_eq!(
        usage::limit_recovery(&decode(shell_thread(json!({}))), now),
        None
    );
    assert_eq!(
        usage::limit_recovery(&decode(limited(json!({}))), now),
        Some(LimitRecovery {
            run_id: "run-2".into(),
            reset_at: Some("2099-01-01T15:40:00.000Z".into()),
            can_schedule: true,
            auto_resume: false,
            snoozed: false,
        })
    );
    let scheduled = usage::limit_recovery(
        &decode(limited(json!({
            "limitRecovery": {
                "runId": "run-2", "resetAt": "2099-01-01T15:40:00.000Z",
                "autoResume": true, "snooze": true
            },
            "snoozedUntil": "2099-01-01T15:40:00.000Z"
        }))),
        now,
    )
    .unwrap();
    assert!(scheduled.auto_resume && scheduled.snoozed);
    // A recovery set for an earlier limit doesn't count.
    let stale = usage::limit_recovery(
        &decode(limited(json!({
            "limitRecovery": { "runId": "run-1", "resetAt": NOW, "autoResume": true }
        }))),
        now,
    )
    .unwrap();
    assert!(!stale.auto_resume);
    // Without a reset time, there's nothing to wait for.
    let unknown =
        usage::limit_recovery(&decode(limited(json!({ "usageLimitResetAt": null }))), now).unwrap();
    assert!(!unknown.can_schedule);
}

// The view, against a recording backend.

#[derive(Default)]
struct Recorder {
    projection: Mutex<Value>,
    shell: Mutex<Vec<Value>>,
    providers: Mutex<Vec<Value>>,
    commands: Mutex<Vec<OrchestrationV2Command>>,
    raw_calls: Mutex<Vec<(String, Value, Access)>>,
    thread_events: Mutex<Option<mpsc::UnboundedSender<Result<OrchestrationV2ThreadStreamItem>>>>,
}

impl Recorder {
    fn new(projection: &Projection, shell: Value) -> Arc<Self> {
        Arc::new(Self {
            projection: Mutex::new(projection.json()),
            shell: Mutex::new(vec![shell]),
            providers: Mutex::new(vec![provider(json!({}))]),
            ..Default::default()
        })
    }

    fn commands(&self) -> Vec<Value> {
        self.commands.lock().iter().map(wire).collect()
    }

    /// The commands without their fresh ids, which must be UUIDs.
    fn commands_without_ids(&self) -> Vec<Value> {
        self.commands()
            .into_iter()
            .map(|mut command| {
                let object = command.as_object_mut().unwrap();
                for key in ["commandId", "messageId", "targetThreadId"] {
                    if let Some(id) = object.remove(key) {
                        let id = id.as_str().unwrap();
                        let uuid = id.split(':').next().unwrap();
                        assert!(uuid::Uuid::parse_str(uuid).is_ok(), "{key} {id}");
                    }
                }
                command
            })
            .collect()
    }

    fn push_event(&self, sequence: i64, kind: &str, payload: Value) {
        let item = serde_json::from_value(json!({
            "kind": "event",
            "sequence": sequence,
            "event": {
                "id": format!("event-{sequence}"), "type": kind, "threadId": THREAD,
                "occurredAt": NOW, "payload": payload
            }
        }))
        .unwrap();
        self.thread_events
            .lock()
            .as_ref()
            .expect("the view follows the thread")
            .unbounded_send(Ok(item))
            .unwrap();
    }
}

fn ready<T: Send + 'static>(value: Result<T>) -> BoxFuture<'static, Result<T>> {
    futures::future::ready(value).boxed()
}

impl ThreadBackend for Recorder {
    fn shell_snapshot(&self) -> BoxFuture<'static, Result<OrchestrationV2ShellSnapshot>> {
        let snapshot = json!({
            "schemaVersion": 1, "snapshotSequence": 7, "threads": *self.shell.lock(),
            "archivedThreads": [],
            "projects": [{
                "id": "project-1", "title": "api", "workspaceRoot": "/src/api", "scripts": [],
                "createdAt": NOW, "updatedAt": NOW, "defaultModelSelection": null
            }]
        });
        ready(serde_json::from_value(snapshot).map_err(Into::into))
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
        let mut projection = self.projection.lock().clone();
        // A fork opens a thread of its own.
        if thread_id != THREAD {
            projection["thread"]["id"] = json!(thread_id);
        }
        ready(
            serde_json::from_value(json!({ "snapshotSequence": 10, "projection": projection }))
                .map_err(Into::into),
        )
    }

    fn subscribe_thread(
        &self,
        input: OrchestrationV2SubscribeThreadInput,
    ) -> Result<BoxStream<'static, Result<OrchestrationV2ThreadStreamItem>>> {
        if input.thread_id != THREAD {
            return Ok(futures::stream::pending().boxed());
        }
        let (sender, receiver) = mpsc::unbounded();
        *self.thread_events.lock() = Some(sender);
        Ok(receiver.boxed())
    }

    fn turn_item(
        &self,
        _: OrchestrationV2GetTurnItemInput,
    ) -> BoxFuture<'static, Result<OrchestrationV2GetTurnItemResult>> {
        ready(Err(anyhow!("no turn items")))
    }

    fn providers(&self) -> BoxFuture<'static, Result<Vec<ServerProvider>>> {
        let providers = Value::Array(self.providers.lock().clone());
        ready(serde_json::from_value(providers).map_err(Into::into))
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
        self.raw_calls
            .lock()
            .push((tag.to_string(), payload, access));
        ready(Ok(json!({ "providers": [] })))
    }

    fn subscribe_raw(
        &self,
        tag: &str,
        payload: Value,
        access: Access,
    ) -> Result<BoxStream<'static, Result<Value>>> {
        self.raw_calls
            .lock()
            .push((tag.to_string(), payload, access));
        if tag != "subscribeServerConfig" {
            return Err(anyhow!("unexpected stream {tag}"));
        }
        let event = json!({
            "version": 1,
            "type": "providerStatuses",
            "payload": { "providers": *self.providers.lock() }
        });
        Ok(futures::stream::iter([Ok(event)])
            .chain(futures::stream::pending())
            .boxed())
    }

    fn upload(&self, _: AttachmentUpload) -> BoxFuture<'static, Result<()>> {
        ready(Err(anyhow!("no uploads")))
    }
}

async fn workspace(
    backend: Arc<dyn ThreadBackend>,
    cx: &mut TestAppContext,
) -> (Entity<Workspace>, &mut VisualTestContext) {
    cx.update(|cx| {
        let settings_store = SettingsStore::test(cx);
        cx.set_global(settings_store);
        theme_settings::init(theme::LoadThemes::JustBase, cx);
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
    cx.simulate_resize(gpui::size(gpui::px(1200.), gpui::px(1000.)));
    cx.run_until_parked();
    (workspace, cx)
}

fn open(workspace: &Entity<Workspace>, cx: &mut VisualTestContext) -> Entity<ThreadView> {
    let view = workspace
        .update_in(cx, |workspace, window, cx| {
            ThreadView::open(THREAD.into(), "Retries".into(), workspace, window, cx)
        })
        .unwrap();
    cx.run_until_parked();
    view
}

fn composer_text(view: &Entity<ThreadView>, cx: &mut VisualTestContext) -> String {
    view.read_with(cx, |view, cx| view.composer().read(cx).text(cx))
}

fn error(view: &Entity<ThreadView>, cx: &mut VisualTestContext) -> Option<String> {
    view.read_with(cx, |view, _| view.error().map(ToString::to_string))
}

#[gpui::test]
async fn edit_from_here_rewinds_then_returns_the_message(cx: &mut TestAppContext) {
    let projection = two_runs();
    let backend = Recorder::new(&projection, shell_thread(json!({})));
    let (workspace, cx) = workspace(backend.clone(), cx).await;
    let view = open(&workspace, cx);
    // Both messages can be rewound to; answers can be forked.
    for selector in [
        "thread-edit-u1",
        "thread-edit-u2",
        "thread-fork-a1",
        "thread-fork-a2",
    ] {
        assert!(cx.debug_bounds(selector).is_some(), "{selector}");
    }
    assert!(cx.debug_bounds("thread-edit-a1").is_none());

    // Something in the composer stays, with the message after it.
    let composer = view.read_with(cx, |view, _| view.composer().clone());
    composer.update_in(cx, |editor, window, cx| editor.set_text("Also", window, cx));
    view.update_in(cx, |view, window, cx| {
        view.edit_from_here("u2".into(), window, cx)
    });
    cx.run_until_parked();
    assert!(cx.has_pending_prompt());
    cx.simulate_prompt_answer("Revert Files Too");
    cx.run_until_parked();
    assert_eq!(
        backend.commands_without_ids(),
        [json!({
            "type": "checkpoint.rollback",
            "threadId": THREAD,
            "scopeId": "scope-1",
            "checkpointId": "checkpoint-1",
            "restoreFiles": true
        })]
    );
    // Until T3 has rolled the thread back, the message stays out of the composer.
    assert!(view.read_with(cx, |view, _| view.is_rewinding()));
    assert_eq!(composer_text(&view, cx), "Also");

    backend.push_event(11, "run.updated", run("run-2", 2, "rolled_back"));
    cx.run_until_parked();
    assert!(!view.read_with(cx, |view, _| view.is_rewinding()));
    assert_eq!(composer_text(&view, cx), "Also\n\nNow handle 429s");
    assert_eq!(error(&view, cx), None);
}

#[gpui::test]
async fn edit_from_here_reports_a_failed_rewind(cx: &mut TestAppContext) {
    let projection = two_runs();
    let backend = Recorder::new(&projection, shell_thread(json!({})));
    let (workspace, cx) = workspace(backend.clone(), cx).await;
    let view = open(&workspace, cx);

    // Cancelling sends nothing.
    view.update_in(cx, |view, window, cx| {
        view.edit_from_here("u1".into(), window, cx)
    });
    cx.run_until_parked();
    cx.simulate_prompt_answer("Cancel");
    cx.run_until_parked();
    assert!(backend.commands().is_empty());

    view.update_in(cx, |view, window, cx| {
        view.edit_from_here("u1".into(), window, cx)
    });
    cx.run_until_parked();
    cx.simulate_prompt_answer("Revert and Keep Changes");
    cx.run_until_parked();
    let command = backend.commands()[0].clone();
    assert_eq!(command["checkpointId"], "checkpoint-0");
    assert_eq!(command["restoreFiles"], false);

    // T3 names the failed rollback by its command's id.
    let failure = thread_json(json!({
        "rollbackRequestId": command["commandId"],
        "rollbackFailure": {
            "requestId": command["commandId"],
            "message": "the agent couldn't rewind its conversation"
        }
    }));
    backend.push_event(11, "thread.metadata-updated", failure);
    cx.run_until_parked();
    assert_eq!(
        error(&view, cx).as_deref(),
        Some("Couldn't rewind: the agent couldn't rewind its conversation")
    );
    assert_eq!(composer_text(&view, cx), "");
}

#[gpui::test]
async fn forking_opens_the_new_thread(cx: &mut TestAppContext) {
    let projection = two_runs();
    let backend = Recorder::new(&projection, shell_thread(json!({})));
    let (workspace, cx) = workspace(backend.clone(), cx).await;
    let view = open(&workspace, cx);
    view.update_in(cx, |view, window, cx| {
        view.fork_from("a1".into(), window, cx)
    });
    cx.run_until_parked();
    let command = backend.commands()[0].clone();
    assert_eq!(
        backend.commands_without_ids(),
        [json!({
            "type": "thread.fork",
            "createdBy": "user",
            "creationSource": "web",
            "sourceThreadId": THREAD,
            "sourcePoint": { "type": "run", "runId": "run-1" },
            "title": "Add retries to the webhook dispatcher fork"
        })]
    );
    // The fork is open, in its own tab.
    let active = workspace.read_with(cx, |workspace, cx| {
        let view = workspace.active_item(cx)?.downcast::<ThreadView>()?;
        view.read(cx).thread_id().map(str::to_string)
    });
    assert_eq!(active.as_deref(), command["targetThreadId"].as_str());
}

#[gpui::test]
async fn the_palette_compacts_and_restarts_the_agent(cx: &mut TestAppContext) {
    let projection = two_runs();
    let backend = Recorder::new(&projection, shell_thread(json!({})));
    let (workspace, cx) = workspace(backend.clone(), cx).await;
    open(&workspace, cx);
    assert!(cx.debug_bounds("thread-compact").is_some());

    cx.dispatch_action(CompactContext);
    cx.run_until_parked();
    cx.dispatch_action(RestartAgentSession);
    cx.run_until_parked();
    let commands = backend.commands();
    assert_eq!(
        backend.commands_without_ids(),
        [
            json!({
                "type": "message.dispatch",
                "createdBy": "user",
                "creationSource": "web",
                "threadId": THREAD,
                "text": "/compact",
                "attachments": [],
                "deliveryIntent": "auto",
                "dispatchMode": { "type": "start_immediately" }
            }),
            json!({
                "type": "provider-session.detach",
                "threadId": THREAD,
                "providerSessionId": "session-1",
                "reason": "client-requested"
            }),
        ]
    );
    assert!(
        commands[1]["commandId"]
            .as_str()
            .unwrap()
            .ends_with(":detach:session-1")
    );
    // Then T3 checks the agent again, which is a read.
    let refresh = backend
        .raw_calls
        .lock()
        .iter()
        .find(|(tag, ..)| tag == "server.refreshProviders")
        .cloned();
    assert_eq!(
        refresh,
        Some((
            "server.refreshProviders".to_string(),
            json!({ "cwd": WORKTREE, "fresh": true, "instanceId": "claudeAgent" }),
            Access::Read
        ))
    );
    // The config comes from T3's stream, which is a read too.
    assert!(
        backend
            .raw_calls
            .lock()
            .iter()
            .any(|(tag, payload, access)| tag == "subscribeServerConfig"
                && *payload == json!({})
                && *access == Access::Read)
    );
}

#[gpui::test]
async fn restarting_a_working_agent_asks_first(cx: &mut TestAppContext) {
    let mut projection = two_runs();
    projection.runs[1] = run("run-2", 2, "running");
    let backend = Recorder::new(&projection, shell_thread(json!({})));
    let (workspace, cx) = workspace(backend.clone(), cx).await;
    open(&workspace, cx);

    // While the agent works, restarting its session would end the run, so it asks.
    cx.dispatch_action(RestartAgentSession);
    cx.run_until_parked();
    assert!(cx.has_pending_prompt());
    cx.simulate_prompt_answer("Cancel");
    cx.run_until_parked();
    assert!(backend.commands().is_empty(), "nothing is sent on Cancel");

    cx.dispatch_action(RestartAgentSession);
    cx.run_until_parked();
    cx.simulate_prompt_answer("Restart");
    cx.run_until_parked();
    assert_eq!(
        backend.commands_without_ids(),
        [json!({
            "type": "provider-session.detach",
            "threadId": THREAD,
            "providerSessionId": "session-1",
            "reason": "client-requested"
        })]
    );
}

#[gpui::test]
async fn compacting_waits_for_an_agent_that_takes_it(cx: &mut TestAppContext) {
    let projection = two_runs();
    let backend = Recorder::new(&projection, shell_thread(json!({})));
    *backend.providers.lock() = vec![provider(json!({ "slashCommands": [] }))];
    let (workspace, cx) = workspace(backend.clone(), cx).await;
    let view = open(&workspace, cx);
    assert_eq!(
        view.read_with(cx, |view, cx| view.compact_blocked(cx))
            .as_deref(),
        Some("Compaction is unavailable for this agent")
    );
    cx.dispatch_action(CompactContext);
    cx.run_until_parked();
    assert!(backend.commands().is_empty());
}

#[gpui::test]
async fn a_limited_thread_can_resume_at_reset(cx: &mut TestAppContext) {
    let mut projection = two_runs();
    projection.runs[1] = run("run-2", 2, "failed");
    let backend = Recorder::new(&projection, limited(json!({})));
    let (workspace, cx) = workspace(backend.clone(), cx).await;
    open(&workspace, cx);
    assert!(cx.debug_bounds("thread-limit-recovery").is_some());

    let resume = cx.debug_bounds("thread-limit-resume-at-reset").unwrap();
    cx.simulate_click(resume.center(), gpui::Modifiers::none());
    cx.run_until_parked();
    let snooze = cx.debug_bounds("thread-limit-snooze").unwrap();
    cx.simulate_click(snooze.center(), gpui::Modifiers::none());
    cx.run_until_parked();
    assert_eq!(
        backend.commands_without_ids(),
        [
            json!({
                "type": "thread.metadata.update",
                "threadId": THREAD,
                "limitRecovery": {
                    "runId": "run-2", "resetAt": "2099-01-01T15:40:00.000Z", "autoResume": true
                }
            }),
            json!({
                "type": "thread.metadata.update",
                "threadId": THREAD,
                "limitRecovery": {
                    "runId": "run-2", "resetAt": "2099-01-01T15:40:00.000Z", "snooze": true
                }
            }),
        ]
    );
}

#[gpui::test]
async fn the_header_shows_the_agents_status_and_usage(cx: &mut TestAppContext) {
    let projection = two_runs();
    let backend = Recorder::new(&projection, shell_thread(json!({})));
    *backend.providers.lock() = vec![provider(json!({
        "status": "error",
        "auth": { "status": "unauthenticated" },
        "usageLimits": {
            "checkedAt": NOW,
            "windows": [
                { "id": "five_hour", "kind": "session", "label": "5-hour", "usedPercent": 40 },
                { "id": "seven_day", "kind": "weekly", "label": "Weekly", "usedPercent": 10 }
            ]
        }
    }))];
    let (workspace, cx) = workspace(backend.clone(), cx).await;
    let view = open(&workspace, cx);
    assert_eq!(
        view.read_with(cx, |view, _| view
            .thread_state()
            .map(crate::header::header_details)),
        Some(vec![
            "t3/retry-backoff".to_string(),
            "claude-sonnet-4-5".to_string(),
            "Full access".to_string()
        ])
    );
    assert!(cx.debug_bounds("thread-provider-banner").is_some());
    view.update(cx, |view, cx| {
        let key = view.provider_banner(cx).unwrap().key;
        view.dismiss_provider_banner(key, cx)
    });
    cx.run_until_parked();
    assert!(cx.debug_bounds("thread-provider-banner").is_none());

    assert!(cx.debug_bounds("thread-usage-panel").is_none());
    let summary = cx.debug_bounds("thread-usage-summary").unwrap();
    cx.simulate_click(summary.center(), gpui::Modifiers::none());
    cx.run_until_parked();
    assert!(cx.debug_bounds("thread-usage-panel").is_some());
    cx.dispatch_action(crate::ShowUsageLimits);
    cx.run_until_parked();
    assert!(cx.debug_bounds("thread-usage-panel").is_none());
}

#[gpui::test]
async fn dry_runs_refuse_going_back(cx: &mut TestAppContext) {
    let mut projection = two_runs();
    projection.runs[1] = run("run-2", 2, "failed");
    let recorder = Recorder::new(&projection, limited(json!({})));
    let backend = Arc::new(ReadOnlyBackend::new(
        recorder.clone(),
        "MUSTR_DRY_RUN_WRITES is set",
    ));
    let (workspace, cx) = workspace(backend, cx).await;
    let view = open(&workspace, cx);
    // The actions show, but can't be used.
    assert!(cx.debug_bounds("thread-edit-u2").is_some());
    assert!(cx.debug_bounds("thread-fork-a1").is_some());
    view.update_in(cx, |view, window, cx| {
        view.edit_from_here("u2".into(), window, cx)
    });
    cx.run_until_parked();
    assert!(!cx.has_pending_prompt());
    assert_eq!(
        error(&view, cx).as_deref(),
        Some("Couldn't rewind: MUSTR_DRY_RUN_WRITES is set")
    );
    view.update_in(cx, |view, window, cx| {
        view.fork_from("a1".into(), window, cx)
    });
    view.update(cx, |view, cx| {
        view.change_limit_recovery(RecoveryChange::AutoResume(true), cx)
    });
    cx.dispatch_action(CompactContext);
    cx.dispatch_action(RestartAgentSession);
    cx.run_until_parked();
    assert!(recorder.commands().is_empty());
    assert!(
        recorder
            .raw_calls
            .lock()
            .iter()
            .all(|(_, _, access)| *access == Access::Read)
    );
}

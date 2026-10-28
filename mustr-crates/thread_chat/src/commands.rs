//! The payloads mustr sends to drive agents, built the way T3's web app builds them
//! (`packages/client-runtime/src/operations/commands.ts` in T3).
//!
//! Every write is an `orchestration.dispatchCommand` with one `OrchestrationV2Command`, except
//! starting a thread, which is `orchestration.launchThread`. Ids are bare UUIDs, as in T3's
//! clients, so T3 can tell a retried command from a new one.
//!
//! mustr pins T3's version, and that version advertises `serverResolvedCommandContext` and
//! `threadStop` in its environment descriptor. So, like T3's web app on such a server, mustr
//! sends a message as `start_immediately` with a delivery intent and lets T3 pick between
//! starting, steering, and queueing; asks for a queue with `queue_after_active`; and stops a
//! thread with `thread.stop`.

use t3_api::{
    tagged::{
        OrchestrationV2Command, OrchestrationV2CommandDispatchMode,
        OrchestrationV2ThreadLaunchWorkspaceStrategy,
    },
    types::{
        ModelSelection, OrchestrationV2Actor, OrchestrationV2CommandDispatchModeQueueAfterActive,
        OrchestrationV2CommandDispatchModeQueueAfterActiveType,
        OrchestrationV2CommandDispatchModeStartImmediately,
        OrchestrationV2CommandDispatchModeStartImmediatelyType,
        OrchestrationV2CommandMessageDispatch, OrchestrationV2CommandMessageDispatchDeliveryIntent,
        OrchestrationV2CommandMessageDispatchType,
        OrchestrationV2CommandQueuedMessagePromoteToSteer,
        OrchestrationV2CommandQueuedMessagePromoteToSteerType,
        OrchestrationV2CommandQueuedRunCancel, OrchestrationV2CommandQueuedRunCancelType,
        OrchestrationV2CommandRuntimeRequestRespond,
        OrchestrationV2CommandRuntimeRequestRespondType, OrchestrationV2CommandThreadStop,
        OrchestrationV2CommandThreadStopType, OrchestrationV2CommandThreadUserInputDismiss,
        OrchestrationV2CommandThreadUserInputDismissType, OrchestrationV2CreationSource,
        OrchestrationV2ThreadLaunchInput, OrchestrationV2ThreadLaunchInputInitialMessage,
        OrchestrationV2ThreadLaunchWorkspaceStrategyRoot,
        OrchestrationV2ThreadLaunchWorkspaceStrategyRootType,
        OrchestrationV2ThreadLaunchWorkspaceStrategyWorktree,
        OrchestrationV2ThreadLaunchWorkspaceStrategyWorktreeType, ProviderApprovalDecision,
        ProviderInteractionMode, ProviderUserInputAnswers, RuntimeMode,
    },
};

/// mustr is one of T3's interactive clients, like its desktop and web apps, which T3 records
/// as `web`.
pub const CREATION_SOURCE: OrchestrationV2CreationSource = OrchestrationV2CreationSource::Web;

/// A fresh id for a command, message, or thread.
pub fn new_id() -> String {
    uuid::Uuid::new_v4().to_string()
}

/// How a message reaches the agent.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Delivery {
    /// Start a run if none is active; otherwise T3 steers the active run if its agent can take
    /// input mid-run, and queues the message if not.
    Auto,
    /// Run the message after the active run (and after anything queued before it).
    Queue,
    /// Hand the message to the active run now, without waiting for it to finish.
    Steer,
}

/// Ids for one command. Tests pass fixed ones; the views use [`CommandIds::new`].
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CommandIds {
    pub command_id: String,
    pub message_id: String,
}

impl CommandIds {
    pub fn new() -> Self {
        Self {
            command_id: new_id(),
            message_id: new_id(),
        }
    }
}

impl Default for CommandIds {
    fn default() -> Self {
        Self::new()
    }
}

/// A user message to a thread (`message.dispatch`).
pub fn send_message(
    thread_id: &str,
    text: &str,
    delivery: Delivery,
    ids: CommandIds,
) -> OrchestrationV2Command {
    let (dispatch_mode, delivery_intent) = match delivery {
        Delivery::Queue => (
            OrchestrationV2CommandDispatchMode::QueueAfterActive(
                OrchestrationV2CommandDispatchModeQueueAfterActive {
                    type_: OrchestrationV2CommandDispatchModeQueueAfterActiveType::QueueAfterActive,
                },
            ),
            None,
        ),
        Delivery::Auto | Delivery::Steer => (
            OrchestrationV2CommandDispatchMode::StartImmediately(
                OrchestrationV2CommandDispatchModeStartImmediately {
                    type_: OrchestrationV2CommandDispatchModeStartImmediatelyType::StartImmediately,
                },
            ),
            Some(if delivery == Delivery::Steer {
                OrchestrationV2CommandMessageDispatchDeliveryIntent::Steer
            } else {
                OrchestrationV2CommandMessageDispatchDeliveryIntent::Auto
            }),
        ),
    };
    OrchestrationV2Command::MessageDispatch(OrchestrationV2CommandMessageDispatch {
        attachments: Vec::new(),
        command_id: ids.command_id,
        context: None,
        created_by: OrchestrationV2Actor::User,
        creation_source: CREATION_SOURCE,
        delegated_completion: None,
        delivery_intent,
        dispatch_mode,
        manual_continuation_of_run_id: None,
        message_id: ids.message_id,
        model_selection: None,
        notification: None,
        restart_continuation_of_run_id: None,
        scheduled_task_id: None,
        sender_thread_id: None,
        source_plan_ref: None,
        text: text.to_string(),
        thread_id: thread_id.to_string(),
        title_seed: None,
        type_: OrchestrationV2CommandMessageDispatchType::MessageDispatch,
        usage_limit_continuation_of_run_id: None,
        usage_limit_recovery_request_id: None,
    })
}

/// Stops the thread's active run (`thread.stop`), as T3's Stop button does.
pub fn stop(thread_id: &str, command_id: String) -> OrchestrationV2Command {
    OrchestrationV2Command::ThreadStop(OrchestrationV2CommandThreadStop {
        command_id,
        reason: None,
        thread_id: thread_id.to_string(),
        type_: OrchestrationV2CommandThreadStopType::ThreadStop,
    })
}

/// Answers an approval request (`runtime-request.respond`).
pub fn respond_to_approval(
    thread_id: &str,
    request_id: &str,
    decision: ProviderApprovalDecision,
    command_id: String,
) -> OrchestrationV2Command {
    OrchestrationV2Command::RuntimeRequestRespond(OrchestrationV2CommandRuntimeRequestRespond {
        answers: None,
        attachments_by_question_id: None,
        command_id,
        decision: Some(decision),
        request_id: request_id.to_string(),
        thread_id: thread_id.to_string(),
        type_: OrchestrationV2CommandRuntimeRequestRespondType::RuntimeRequestRespond,
    })
}

/// Answers the questions of a user-input request (`runtime-request.respond`), by question id.
pub fn answer_questions(
    thread_id: &str,
    request_id: &str,
    answers: Vec<(String, String)>,
    command_id: String,
) -> OrchestrationV2Command {
    let answers = answers
        .into_iter()
        .map(|(question_id, answer)| (question_id, serde_json::Value::String(answer)))
        .collect();
    OrchestrationV2Command::RuntimeRequestRespond(OrchestrationV2CommandRuntimeRequestRespond {
        answers: Some(ProviderUserInputAnswers(answers)),
        attachments_by_question_id: None,
        command_id,
        decision: None,
        request_id: request_id.to_string(),
        thread_id: thread_id.to_string(),
        type_: OrchestrationV2CommandRuntimeRequestRespondType::RuntimeRequestRespond,
    })
}

/// Declines to answer a user-input request (`thread.user-input.dismiss`).
pub fn dismiss_questions(
    thread_id: &str,
    request_id: &str,
    command_id: String,
) -> OrchestrationV2Command {
    OrchestrationV2Command::ThreadUserInputDismiss(OrchestrationV2CommandThreadUserInputDismiss {
        command_id,
        request_id: request_id.to_string(),
        thread_id: thread_id.to_string(),
        type_: OrchestrationV2CommandThreadUserInputDismissType::ThreadUserInputDismiss,
    })
}

/// Hands a queued message to the active run now (`queued-message.promote-to-steer`).
pub fn steer_with_queued(
    thread_id: &str,
    queued_run_id: &str,
    active_run_id: &str,
    command_id: String,
) -> OrchestrationV2Command {
    OrchestrationV2Command::QueuedMessagePromoteToSteer(
        OrchestrationV2CommandQueuedMessagePromoteToSteer {
            command_id,
            queued_run_id: queued_run_id.to_string(),
            target_run_id: active_run_id.to_string(),
            thread_id: thread_id.to_string(),
            type_:
                OrchestrationV2CommandQueuedMessagePromoteToSteerType::QueuedMessagePromoteToSteer,
        },
    )
}

/// Removes a queued message before it runs (`queued-run.cancel`).
pub fn cancel_queued(
    thread_id: &str,
    queued_run_id: &str,
    command_id: String,
) -> OrchestrationV2Command {
    OrchestrationV2Command::QueuedRunCancel(OrchestrationV2CommandQueuedRunCancel {
        command_id,
        run_id: queued_run_id.to_string(),
        thread_id: thread_id.to_string(),
        type_: OrchestrationV2CommandQueuedRunCancelType::QueuedRunCancel,
    })
}

/// Where a new thread's agent works.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Workspace {
    /// In the project's own checkout.
    ProjectRoot,
    /// In a new worktree that T3 creates from `base_ref` (fetched from the remote first, as
    /// T3's `newWorktreesStartFromOrigin` default does).
    NewWorktree { base_ref: String },
}

/// What the new-thread form collects.
#[derive(Clone, Debug, PartialEq)]
pub struct NewThread {
    pub project_id: String,
    pub model_selection: ModelSelection,
    pub runtime_mode: RuntimeMode,
    pub workspace: Workspace,
    pub text: String,
}

/// The ids of a new thread and its first command and message.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct LaunchIds {
    pub thread_id: String,
    pub command: CommandIds,
}

impl LaunchIds {
    pub fn new() -> Self {
        Self {
            thread_id: new_id(),
            command: CommandIds::new(),
        }
    }
}

impl Default for LaunchIds {
    fn default() -> Self {
        Self::new()
    }
}

/// Starts a thread with its first message (`orchestration.launchThread`). T3 names it from the
/// first message.
pub fn launch(new_thread: NewThread, ids: LaunchIds) -> OrchestrationV2ThreadLaunchInput {
    let workspace_strategy = match new_thread.workspace {
        Workspace::ProjectRoot => OrchestrationV2ThreadLaunchWorkspaceStrategy::Root(
            OrchestrationV2ThreadLaunchWorkspaceStrategyRoot {
                branch: None,
                type_: OrchestrationV2ThreadLaunchWorkspaceStrategyRootType::Root,
            },
        ),
        Workspace::NewWorktree { base_ref } => {
            OrchestrationV2ThreadLaunchWorkspaceStrategy::Worktree(
                OrchestrationV2ThreadLaunchWorkspaceStrategyWorktree {
                    base_ref,
                    branch: None,
                    start_from_origin: Some(true),
                    type_: OrchestrationV2ThreadLaunchWorkspaceStrategyWorktreeType::Worktree,
                },
            )
        }
    };
    OrchestrationV2ThreadLaunchInput {
        command_id: ids.command.command_id,
        creation_source: Some(CREATION_SOURCE),
        generate_title: Some(true),
        initial_message: Some(OrchestrationV2ThreadLaunchInputInitialMessage {
            attachments: Vec::new(),
            context: None,
            message_id: Some(ids.command.message_id),
            text: new_thread.text.clone(),
        }),
        interaction_mode: ProviderInteractionMode::Default,
        model_selection: new_thread.model_selection,
        project_id: new_thread.project_id,
        reuse_existing_thread: None,
        runtime_mode: new_thread.runtime_mode,
        thread_id: Some(ids.thread_id),
        title: title_from_message(&new_thread.text),
        workspace_strategy,
    }
}

const TITLE_MAX_CHARS: usize = 60;

/// A placeholder title from the first line of the first message, until T3 generates one.
pub fn title_from_message(text: &str) -> String {
    let line = text
        .lines()
        .map(str::trim)
        .find(|line| !line.is_empty())
        .unwrap_or("New thread");
    match line.char_indices().nth(TITLE_MAX_CHARS) {
        Some((index, _)) => format!("{}…", line[..index].trim_end()),
        None => line.to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use pretty_assertions::assert_eq;
    use serde_json::{Value, json};

    fn ids() -> CommandIds {
        CommandIds {
            command_id: "command-1".into(),
            message_id: "message-1".into(),
        }
    }

    fn wire(command: &OrchestrationV2Command) -> Value {
        serde_json::to_value(command).expect("commands serialize")
    }

    #[test]
    fn messages_let_t3_pick_how_to_deliver_them() {
        assert_eq!(
            wire(&send_message(
                "thread-1",
                "Fix the test",
                Delivery::Auto,
                ids()
            )),
            json!({
                "type": "message.dispatch",
                "commandId": "command-1",
                "createdBy": "user",
                "creationSource": "web",
                "threadId": "thread-1",
                "messageId": "message-1",
                "text": "Fix the test",
                "attachments": [],
                "deliveryIntent": "auto",
                "dispatchMode": { "type": "start_immediately" }
            })
        );
        assert_eq!(
            wire(&send_message(
                "thread-1",
                "Also this",
                Delivery::Steer,
                ids()
            )),
            json!({
                "type": "message.dispatch",
                "commandId": "command-1",
                "createdBy": "user",
                "creationSource": "web",
                "threadId": "thread-1",
                "messageId": "message-1",
                "text": "Also this",
                "attachments": [],
                "deliveryIntent": "steer",
                "dispatchMode": { "type": "start_immediately" }
            })
        );
        // Queueing names the mode itself, without a delivery intent.
        assert_eq!(
            wire(&send_message("thread-1", "Later", Delivery::Queue, ids())),
            json!({
                "type": "message.dispatch",
                "commandId": "command-1",
                "createdBy": "user",
                "creationSource": "web",
                "threadId": "thread-1",
                "messageId": "message-1",
                "text": "Later",
                "attachments": [],
                "dispatchMode": { "type": "queue_after_active" }
            })
        );
    }

    #[test]
    fn run_and_queue_commands() {
        assert_eq!(
            wire(&stop("thread-1", "command-2".into())),
            json!({ "type": "thread.stop", "commandId": "command-2", "threadId": "thread-1" })
        );
        assert_eq!(
            wire(&steer_with_queued(
                "thread-1",
                "run-q",
                "run-a",
                "command-3".into()
            )),
            json!({
                "type": "queued-message.promote-to-steer",
                "commandId": "command-3",
                "threadId": "thread-1",
                "queuedRunId": "run-q",
                "targetRunId": "run-a"
            })
        );
        assert_eq!(
            wire(&cancel_queued("thread-1", "run-q", "command-4".into())),
            json!({
                "type": "queued-run.cancel",
                "commandId": "command-4",
                "threadId": "thread-1",
                "runId": "run-q"
            })
        );
    }

    #[test]
    fn approvals_and_questions() {
        assert_eq!(
            wire(&respond_to_approval(
                "thread-1",
                "request-1",
                ProviderApprovalDecision::AcceptForSession,
                "command-5".into()
            )),
            json!({
                "type": "runtime-request.respond",
                "commandId": "command-5",
                "threadId": "thread-1",
                "requestId": "request-1",
                "decision": "acceptForSession"
            })
        );
        assert_eq!(
            wire(&answer_questions(
                "thread-1",
                "request-2",
                vec![
                    ("q1".into(), "yes".into()),
                    ("q2".into(), "Option B".into())
                ],
                "command-6".into()
            )),
            json!({
                "type": "runtime-request.respond",
                "commandId": "command-6",
                "threadId": "thread-1",
                "requestId": "request-2",
                "answers": { "q1": "yes", "q2": "Option B" }
            })
        );
        assert_eq!(
            wire(&dismiss_questions(
                "thread-1",
                "request-2",
                "command-7".into()
            )),
            json!({
                "type": "thread.user-input.dismiss",
                "commandId": "command-7",
                "threadId": "thread-1",
                "requestId": "request-2"
            })
        );
    }

    fn new_thread(workspace: Workspace) -> NewThread {
        NewThread {
            project_id: "project-1".into(),
            model_selection: ModelSelection {
                instance_id: Some(json!("claudeAgent")),
                model: json!("claude-sonnet"),
                options: None,
                provider: None,
            },
            runtime_mode: RuntimeMode::ApprovalRequired,
            workspace,
            text: "  Add retries to the webhook dispatcher\nwith backoff".into(),
        }
    }

    fn launch_ids() -> LaunchIds {
        LaunchIds {
            thread_id: "thread-9".into(),
            command: ids(),
        }
    }

    #[test]
    fn new_threads_start_in_a_worktree_or_the_checkout() {
        let input = launch(
            new_thread(Workspace::NewWorktree {
                base_ref: "main".into(),
            }),
            launch_ids(),
        );
        assert_eq!(
            serde_json::to_value(&input).unwrap(),
            json!({
                "commandId": "command-1",
                "creationSource": "web",
                "threadId": "thread-9",
                "projectId": "project-1",
                "title": "Add retries to the webhook dispatcher",
                "generateTitle": true,
                "modelSelection": { "instanceId": "claudeAgent", "model": "claude-sonnet" },
                "runtimeMode": "approval-required",
                "interactionMode": "default",
                "workspaceStrategy": { "type": "worktree", "baseRef": "main", "startFromOrigin": true },
                "initialMessage": {
                    "messageId": "message-1",
                    "text": "  Add retries to the webhook dispatcher\nwith backoff",
                    "attachments": []
                }
            })
        );
        let input = launch(new_thread(Workspace::ProjectRoot), launch_ids());
        assert_eq!(
            serde_json::to_value(&input.workspace_strategy).unwrap(),
            json!({ "type": "root" })
        );
    }

    #[test]
    fn placeholder_titles_come_from_the_first_line() {
        assert_eq!(title_from_message("\n\n  Fix it  \nmore"), "Fix it");
        assert_eq!(title_from_message("   "), "New thread");
        let long = "word ".repeat(30);
        let title = title_from_message(&long);
        assert!(title.ends_with('…'));
        assert!(title.chars().count() <= TITLE_MAX_CHARS + 1);
    }

    #[test]
    fn ids_are_bare_uuids() {
        let ids = CommandIds::new();
        assert_ne!(ids.command_id, ids.message_id);
        assert!(uuid::Uuid::parse_str(&ids.command_id).is_ok());
    }
}

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
        ChatAttachment, OrchestrationV2Command, OrchestrationV2CommandDispatchMode,
        OrchestrationV2ThreadLaunchWorkspaceStrategy, SecretRequestAnswerInputAnswer,
    },
    types::{
        ChatImageAttachment, ChatImageAttachmentType, ModelSelection, OrchestrationV2Actor,
        OrchestrationV2CommandDispatchModeQueueAfterActive,
        OrchestrationV2CommandDispatchModeQueueAfterActiveType,
        OrchestrationV2CommandDispatchModeStartImmediately,
        OrchestrationV2CommandDispatchModeStartImmediatelyType,
        OrchestrationV2CommandMessageDispatch, OrchestrationV2CommandMessageDispatchDeliveryIntent,
        OrchestrationV2CommandMessageDispatchSourcePlanRef,
        OrchestrationV2CommandMessageDispatchType, OrchestrationV2CommandQueueResume,
        OrchestrationV2CommandQueueResumeType, OrchestrationV2CommandQueuedMessagePromoteToSteer,
        OrchestrationV2CommandQueuedMessagePromoteToSteerType,
        OrchestrationV2CommandQueuedRunCancel, OrchestrationV2CommandQueuedRunCancelType,
        OrchestrationV2CommandQueuedRunEdit, OrchestrationV2CommandQueuedRunEditType,
        OrchestrationV2CommandRuntimeRequestRespond,
        OrchestrationV2CommandRuntimeRequestRespondType, OrchestrationV2CommandThreadCreate,
        OrchestrationV2CommandThreadCreateType, OrchestrationV2CommandThreadDelete,
        OrchestrationV2CommandThreadDeleteType, OrchestrationV2CommandThreadInteractionModeSet,
        OrchestrationV2CommandThreadInteractionModeSetType, OrchestrationV2CommandThreadStop,
        OrchestrationV2CommandThreadStopType, OrchestrationV2CommandThreadUserInputDismiss,
        OrchestrationV2CommandThreadUserInputDismissType, OrchestrationV2CreationSource,
        OrchestrationV2ThreadLaunchInput, OrchestrationV2ThreadLaunchInputInitialMessage,
        OrchestrationV2ThreadLaunchWorkspaceStrategyExistingWorktree,
        OrchestrationV2ThreadLaunchWorkspaceStrategyExistingWorktreeType,
        OrchestrationV2ThreadLaunchWorkspaceStrategyRoot,
        OrchestrationV2ThreadLaunchWorkspaceStrategyRootType,
        OrchestrationV2ThreadLaunchWorkspaceStrategyWorktree,
        OrchestrationV2ThreadLaunchWorkspaceStrategyWorktreeType, ProviderApprovalDecision,
        ProviderInteractionMode, ProviderUserInputAnswers, RuntimeMode, SecretRequestAnswerInput,
        SecretRequestAnswerInputAnswerDecline, SecretRequestAnswerInputAnswerDeclineType,
        SecretRequestAnswerInputAnswerSave, SecretRequestAnswerInputAnswerSaveType,
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
    message(thread_id, Message::text(text), delivery, ids)
}

/// What a `message.dispatch` carries besides its thread, delivery, and ids.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Message {
    pub text: String,
    /// Images uploaded for it (see [`image_attachment`]).
    pub attachments: Vec<ChatAttachment>,
    /// The model to run it with; `None` keeps the thread's.
    pub model_selection: Option<ModelSelection>,
    /// The plan it implements.
    pub source_plan: Option<PlanRef>,
    /// A title for T3 to name the thread from, if it has none yet.
    pub title_seed: Option<String>,
}

impl Message {
    pub fn text(text: impl Into<String>) -> Self {
        Self {
            text: text.into(),
            ..Default::default()
        }
    }
}

/// A proposed plan, by its thread and id (`sourcePlanRef`).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PlanRef {
    pub thread_id: String,
    pub plan_id: String,
}

/// An uploaded image, as a message names it: by the attachment id `attachments.createUploadUrl`
/// gave it, with the name, type, and size it was minted for (`attachmentUploadQueue.ts` in T3).
pub fn image_attachment(
    attachment_id: &str,
    name: &str,
    mime_type: &str,
    size_bytes: usize,
) -> ChatAttachment {
    ChatAttachment::Image(ChatImageAttachment {
        id: attachment_id.to_string(),
        mime_type: mime_type.to_string(),
        name: name.to_string(),
        size_bytes: size_bytes as i64,
        source: None,
        type_: ChatImageAttachmentType::Image,
    })
}

/// A message to a thread (`message.dispatch`), with its images and plan.
pub fn message(
    thread_id: &str,
    message: Message,
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
        attachments: message.attachments,
        command_id: ids.command_id,
        context: None,
        created_by: OrchestrationV2Actor::User,
        creation_source: CREATION_SOURCE,
        delegated_completion: None,
        delivery_intent,
        dispatch_mode,
        manual_continuation_of_run_id: None,
        message_id: ids.message_id,
        model_selection: message.model_selection,
        notification: None,
        restart_continuation_of_run_id: None,
        scheduled_task_id: None,
        sender_thread_id: None,
        source_plan_ref: message.source_plan.map(|plan| {
            OrchestrationV2CommandMessageDispatchSourcePlanRef {
                plan_id: plan.plan_id,
                thread_id: plan.thread_id,
            }
        }),
        text: message.text,
        thread_id: thread_id.to_string(),
        title_seed: message.title_seed,
        type_: OrchestrationV2CommandMessageDispatchType::MessageDispatch,
        usage_limit_continuation_of_run_id: None,
        usage_limit_recovery_request_id: None,
    })
}

/// What T3's app sends to implement a plan: the plan itself, after this prefix
/// (`PLAN_IMPLEMENTATION_PROMPT_PREFIX` in T3's `apps/web/src/proposedPlan.ts`).
pub const PLAN_IMPLEMENTATION_PREFIX: &str = "PLEASE IMPLEMENT THIS PLAN:\n";

/// The message that implements a plan.
pub fn plan_implementation_text(markdown: &str) -> String {
    format!("{PLAN_IMPLEMENTATION_PREFIX}{}", markdown.trim())
}

/// Switches the thread between planning and doing (`thread.interaction-mode.set`), as T3 does
/// before a message in the other mode.
pub fn set_interaction_mode(
    thread_id: &str,
    interaction_mode: ProviderInteractionMode,
    command_id: String,
) -> OrchestrationV2Command {
    OrchestrationV2Command::ThreadInteractionModeSet(
        OrchestrationV2CommandThreadInteractionModeSet {
            command_id,
            interaction_mode,
            thread_id: thread_id.to_string(),
            type_: OrchestrationV2CommandThreadInteractionModeSetType::ThreadInteractionModeSet,
        },
    )
}

/// The thread a plan is implemented in, when it isn't the plan's own.
#[derive(Clone, Debug, PartialEq)]
pub struct PlanThread {
    pub project_id: String,
    pub title: String,
    pub model_selection: ModelSelection,
    pub runtime_mode: RuntimeMode,
    /// The plan's thread's branch and worktree, which the new thread works in too.
    pub branch: Option<String>,
    pub worktree_path: Option<String>,
}

const PLAN_THREAD_TITLE_MAX_CHARS: usize = 50;

/// A plan's title: its first Markdown heading (`proposedPlanTitle` in T3's
/// `packages/shared/src/proposedPlanText.ts`).
pub fn plan_title(markdown: &str) -> Option<String> {
    markdown.lines().find_map(|line| {
        let indent = line.len() - line.trim_start_matches(' ').len();
        if indent > 3 {
            return None;
        }
        let rest = line.trim_start_matches(' ');
        let hashes = rest.len() - rest.trim_start_matches('#').len();
        if !(1..=6).contains(&hashes) {
            return None;
        }
        let heading = &rest[hashes..];
        if !heading.starts_with([' ', '\t']) {
            return None;
        }
        Some(heading.trim().to_string()).filter(|heading| !heading.is_empty())
    })
}

/// "Implement <plan title>", as T3 names a thread that implements a plan, cut to 50 characters
/// (`buildPlanImplementationThreadTitle`, then `truncate` in T3's `packages/shared/src/String.ts`).
pub fn plan_thread_title(markdown: &str) -> String {
    let title = match plan_title(markdown) {
        Some(title) => format!("Implement {title}"),
        None => "Implement plan".to_string(),
    };
    let title = title.trim();
    match title.char_indices().nth(PLAN_THREAD_TITLE_MAX_CHARS) {
        Some((index, _)) => format!("{}...", &title[..index]),
        None => title.to_string(),
    }
}

/// Creates the empty thread a plan is implemented in (`thread.create`), in plan-free mode.
pub fn create_plan_thread(
    plan_thread: PlanThread,
    thread_id: &str,
    command_id: String,
) -> OrchestrationV2Command {
    OrchestrationV2Command::ThreadCreate(OrchestrationV2CommandThreadCreate {
        branch: plan_thread.branch,
        command_id,
        created_by: OrchestrationV2Actor::User,
        creation_source: CREATION_SOURCE,
        imported_native_thread: None,
        interaction_mode: ProviderInteractionMode::Default,
        model_selection: plan_thread.model_selection,
        project_id: plan_thread.project_id,
        runtime_mode: plan_thread.runtime_mode,
        thread_id: thread_id.to_string(),
        title: plan_thread.title,
        type_: OrchestrationV2CommandThreadCreateType::ThreadCreate,
        worktree_path: plan_thread.worktree_path,
    })
}

/// Deletes a thread (`thread.delete`), as T3 does with a plan's new thread that couldn't start.
pub fn delete_thread(thread_id: &str, command_id: String) -> OrchestrationV2Command {
    OrchestrationV2Command::ThreadDelete(OrchestrationV2CommandThreadDelete {
        command_id,
        thread_id: thread_id.to_string(),
        type_: OrchestrationV2CommandThreadDeleteType::ThreadDelete,
    })
}

/// What Resume sends to continue a run that was interrupted or stopped at a usage limit.
pub const CONTINUE_TEXT: &str = "Continue where you left off.";

/// Continues an interrupted or usage-limited run (`message.dispatch` with
/// `manualContinuationOfRunId`), as T3's Resume does: it starts at once, without a delivery
/// intent.
pub fn continue_run(
    thread_id: &str,
    run_id: &str,
    model_selection: Option<ModelSelection>,
    ids: CommandIds,
) -> OrchestrationV2Command {
    let mut command = message(
        thread_id,
        Message {
            model_selection,
            ..Message::text(CONTINUE_TEXT)
        },
        Delivery::Auto,
        ids,
    );
    if let OrchestrationV2Command::MessageDispatch(dispatch) = &mut command {
        dispatch.delivery_intent = None;
        dispatch.manual_continuation_of_run_id = Some(run_id.to_string());
    }
    command
}

/// Replaces a queued message's text before it runs (`queued-run.edit`). Its attachments are
/// left out, which keeps the ones it has.
pub fn edit_queued(
    thread_id: &str,
    queued_run_id: &str,
    text: &str,
    command_id: String,
) -> OrchestrationV2Command {
    OrchestrationV2Command::QueuedRunEdit(OrchestrationV2CommandQueuedRunEdit {
        attachments: None,
        command_id,
        context: None,
        run_id: queued_run_id.to_string(),
        text: text.to_string(),
        thread_id: thread_id.to_string(),
        type_: OrchestrationV2CommandQueuedRunEditType::QueuedRunEdit,
    })
}

/// Answers the agent's request for a secret (`secrets.answerRequest`): saves it, or declines
/// with `None`.
pub fn answer_secret(
    thread_id: &str,
    turn_item_id: &str,
    secret: Option<String>,
) -> SecretRequestAnswerInput {
    SecretRequestAnswerInput {
        answer: match secret {
            Some(secret) => {
                SecretRequestAnswerInputAnswer::Save(SecretRequestAnswerInputAnswerSave {
                    secret,
                    type_: SecretRequestAnswerInputAnswerSaveType::Save,
                })
            }
            None => {
                SecretRequestAnswerInputAnswer::Decline(SecretRequestAnswerInputAnswerDecline {
                    type_: SecretRequestAnswerInputAnswerDeclineType::Decline,
                })
            }
        },
        thread_id: thread_id.to_string(),
        turn_item_id: turn_item_id.to_string(),
    }
}

/// Restarts a held queue (`queue.resume`).
pub fn resume_queue(thread_id: &str, command_id: String) -> OrchestrationV2Command {
    OrchestrationV2Command::QueueResume(OrchestrationV2CommandQueueResume {
        command_id,
        thread_id: thread_id.to_string(),
        type_: OrchestrationV2CommandQueueResumeType::QueueResume,
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
    /// In the project's own checkout, on `branch` when it's known.
    ProjectRoot { branch: Option<String> },
    /// In a new worktree that T3 creates from `base_ref`, fetched from its remote first when
    /// `start_from_origin` (T3's `newWorktreesStartFromOrigin`).
    NewWorktree {
        base_ref: String,
        start_from_origin: bool,
    },
    /// In a worktree that already exists, such as another thread's.
    ExistingWorktree {
        worktree_path: String,
        branch: Option<String>,
    },
}

/// What the new-thread form collects.
#[derive(Clone, Debug, PartialEq)]
pub struct NewThread {
    pub project_id: String,
    pub model_selection: ModelSelection,
    pub runtime_mode: RuntimeMode,
    pub interaction_mode: ProviderInteractionMode,
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
        Workspace::ProjectRoot { branch } => OrchestrationV2ThreadLaunchWorkspaceStrategy::Root(
            OrchestrationV2ThreadLaunchWorkspaceStrategyRoot {
                branch,
                type_: OrchestrationV2ThreadLaunchWorkspaceStrategyRootType::Root,
            },
        ),
        Workspace::NewWorktree {
            base_ref,
            start_from_origin,
        } => OrchestrationV2ThreadLaunchWorkspaceStrategy::Worktree(
            OrchestrationV2ThreadLaunchWorkspaceStrategyWorktree {
                base_ref,
                branch: None,
                start_from_origin: Some(start_from_origin),
                type_: OrchestrationV2ThreadLaunchWorkspaceStrategyWorktreeType::Worktree,
            },
        ),
        Workspace::ExistingWorktree {
            worktree_path,
            branch,
        } => OrchestrationV2ThreadLaunchWorkspaceStrategy::ExistingWorktree(
            OrchestrationV2ThreadLaunchWorkspaceStrategyExistingWorktree {
                branch,
                type_: OrchestrationV2ThreadLaunchWorkspaceStrategyExistingWorktreeType::ExistingWorktree,
                worktree_path,
            },
        ),
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
        interaction_mode: new_thread.interaction_mode,
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
            interaction_mode: ProviderInteractionMode::Default,
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
                start_from_origin: true,
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
        let input = launch(
            new_thread(Workspace::ProjectRoot { branch: None }),
            launch_ids(),
        );
        assert_eq!(
            serde_json::to_value(&input.workspace_strategy).unwrap(),
            json!({ "type": "root" })
        );
    }

    #[test]
    fn new_threads_start_on_a_branch_or_in_an_existing_worktree() {
        let strategy = |workspace| {
            serde_json::to_value(launch(new_thread(workspace), launch_ids()).workspace_strategy)
                .unwrap()
        };
        assert_eq!(
            strategy(Workspace::ProjectRoot {
                branch: Some("feature/retries".into())
            }),
            json!({ "type": "root", "branch": "feature/retries" })
        );
        assert_eq!(
            strategy(Workspace::ExistingWorktree {
                worktree_path: "/src/api-worktrees/retries".into(),
                branch: Some("t3/retries".into()),
            }),
            json!({
                "type": "existing_worktree",
                "worktreePath": "/src/api-worktrees/retries",
                "branch": "t3/retries"
            })
        );
        assert_eq!(
            strategy(Workspace::NewWorktree {
                base_ref: "origin/release".into(),
                start_from_origin: false,
            }),
            json!({ "type": "worktree", "baseRef": "origin/release", "startFromOrigin": false })
        );
        let mut planned = new_thread(Workspace::ProjectRoot { branch: None });
        planned.interaction_mode = ProviderInteractionMode::Plan;
        assert_eq!(
            serde_json::to_value(launch(planned, launch_ids()).interaction_mode).unwrap(),
            json!("plan")
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
    fn messages_carry_images_and_the_plan_they_implement() {
        let message = Message {
            attachments: vec![image_attachment(
                "attachment-1",
                "shot.png",
                "image/png",
                2048,
            )],
            model_selection: Some(ModelSelection {
                instance_id: Some(json!("claudeAgent")),
                model: json!("claude-sonnet"),
                options: None,
                provider: None,
            }),
            source_plan: Some(PlanRef {
                thread_id: "thread-1".into(),
                plan_id: "plan-1".into(),
            }),
            title_seed: Some("Retries".into()),
            ..Message::text(plan_implementation_text("  # Retries\n\n1. Do it\n"))
        };
        assert_eq!(
            wire(&super::message("thread-1", message, Delivery::Auto, ids())),
            json!({
                "type": "message.dispatch",
                "commandId": "command-1",
                "createdBy": "user",
                "creationSource": "web",
                "threadId": "thread-1",
                "messageId": "message-1",
                "text": "PLEASE IMPLEMENT THIS PLAN:\n# Retries\n\n1. Do it",
                "attachments": [{
                    "type": "image", "id": "attachment-1", "name": "shot.png",
                    "mimeType": "image/png", "sizeBytes": 2048
                }],
                "modelSelection": { "instanceId": "claudeAgent", "model": "claude-sonnet" },
                "sourcePlanRef": { "threadId": "thread-1", "planId": "plan-1" },
                "titleSeed": "Retries",
                "deliveryIntent": "auto",
                "dispatchMode": { "type": "start_immediately" }
            })
        );
    }

    #[test]
    fn plan_and_resume_commands() {
        assert_eq!(
            wire(&set_interaction_mode(
                "thread-1",
                ProviderInteractionMode::Default,
                "command-2".into()
            )),
            json!({
                "type": "thread.interaction-mode.set",
                "commandId": "command-2",
                "threadId": "thread-1",
                "interactionMode": "default"
            })
        );
        let plan_thread = PlanThread {
            project_id: "project-1".into(),
            title: plan_thread_title("# Retries"),
            model_selection: ModelSelection {
                instance_id: Some(json!("codex")),
                model: json!("gpt-5"),
                options: None,
                provider: None,
            },
            runtime_mode: RuntimeMode::FullAccess,
            branch: Some("t3/retries".into()),
            worktree_path: None,
        };
        assert_eq!(
            wire(&create_plan_thread(
                plan_thread,
                "thread-2",
                "command-3".into()
            )),
            json!({
                "type": "thread.create",
                "commandId": "command-3",
                "createdBy": "user",
                "creationSource": "web",
                "threadId": "thread-2",
                "projectId": "project-1",
                "title": "Implement Retries",
                "modelSelection": { "instanceId": "codex", "model": "gpt-5" },
                "runtimeMode": "full-access",
                "interactionMode": "default",
                "branch": "t3/retries",
                // A thread in the project's checkout has no worktree; T3 takes `null`.
                "worktreePath": null
            })
        );
        assert_eq!(
            wire(&delete_thread("thread-2", "command-4".into())),
            json!({ "type": "thread.delete", "commandId": "command-4", "threadId": "thread-2" })
        );
        assert_eq!(
            wire(&continue_run("thread-1", "run-7", None, ids())),
            json!({
                "type": "message.dispatch",
                "commandId": "command-1",
                "createdBy": "user",
                "creationSource": "web",
                "threadId": "thread-1",
                "messageId": "message-1",
                "manualContinuationOfRunId": "run-7",
                "text": "Continue where you left off.",
                "attachments": [],
                "dispatchMode": { "type": "start_immediately" }
            })
        );
        assert_eq!(
            wire(&edit_queued(
                "thread-1",
                "run-q",
                "Then update the docs",
                "command-6".into()
            )),
            json!({
                "type": "queued-run.edit",
                "commandId": "command-6",
                "threadId": "thread-1",
                "runId": "run-q",
                "text": "Then update the docs"
            })
        );
        assert_eq!(
            wire(&resume_queue("thread-1", "command-5".into())),
            json!({ "type": "queue.resume", "commandId": "command-5", "threadId": "thread-1" })
        );
    }

    #[test]
    fn secrets_are_saved_or_declined() {
        assert_eq!(
            serde_json::to_value(answer_secret("thread-1", "item-4", Some("s3cr3t".into())))
                .unwrap(),
            json!({
                "threadId": "thread-1",
                "turnItemId": "item-4",
                "answer": { "type": "save", "secret": "s3cr3t" }
            })
        );
        assert_eq!(
            serde_json::to_value(answer_secret("thread-1", "item-4", None)).unwrap(),
            json!({
                "threadId": "thread-1",
                "turnItemId": "item-4",
                "answer": { "type": "decline" }
            })
        );
    }

    #[test]
    fn plan_threads_are_named_after_the_plans_heading() {
        assert_eq!(
            plan_title("Intro\n\n## Add retries \nmore"),
            Some("Add retries".into())
        );
        assert_eq!(plan_title("    # Indented code"), None);
        assert_eq!(plan_title("#hashtag"), None);
        assert_eq!(plan_thread_title("No heading"), "Implement plan");
        assert_eq!(
            plan_thread_title("# Retry every webhook delivery with exponential backoff and jitter"),
            "Implement Retry every webhook delivery with expone..."
        );
    }

    #[test]
    fn ids_are_bare_uuids() {
        let ids = CommandIds::new();
        assert_ne!(ids.command_id, ids.message_id);
        assert!(uuid::Uuid::parse_str(&ids.command_id).is_ok());
    }
}

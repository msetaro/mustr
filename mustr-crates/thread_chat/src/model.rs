//! One thread as mustr shows it: T3's thread projection, kept current by the events of
//! `orchestration.subscribeThread`, reduced to what the timeline and composer need.
//!
//! T3 sends the same turn item in three generated shapes (in a projection, in its visible
//! items, and in an event). They're identical on the wire, so each is decoded into
//! [`OrchestrationV2TurnItem`] once and turned into a [`TimelineItem`].

use collections::{HashMap, HashSet};
use serde::Serialize;
use t3_api::{
    tagged::{
        ChatAttachment, OrchestrationV2PlanArtifact,
        OrchestrationV2RuntimeRequestResponseCapability, OrchestrationV2ThreadStreamItem,
        OrchestrationV2TurnItem,
    },
    types::{
        OrchestrationV2AppThread, OrchestrationV2Checkpoint, OrchestrationV2CheckpointStatus,
        OrchestrationV2ConversationMessage, OrchestrationV2DomainEvent,
        OrchestrationV2NativeRefStrength, OrchestrationV2PlanArtifactProposedPlanStatus,
        OrchestrationV2ProviderCapabilities, OrchestrationV2ProviderGoal,
        OrchestrationV2ProviderGoalStatus, OrchestrationV2ProviderSession,
        OrchestrationV2ProviderSessionStatus, OrchestrationV2ProviderThread,
        OrchestrationV2ProviderTurn, OrchestrationV2Run, OrchestrationV2RunAttempt,
        OrchestrationV2RunStatus, OrchestrationV2RuntimeRequest, OrchestrationV2RuntimeRequestKind,
        OrchestrationV2RuntimeRequestKindString, OrchestrationV2RuntimeRequestStatus,
        OrchestrationV2Subagent, OrchestrationV2ThreadProjection,
        OrchestrationV2ThreadProjectionVisibleTurnItemsItemVisibility,
        OrchestrationV2TurnItemStatus, OrchestrationV2UserMessageInputIntent,
        ProviderApprovalDecision, ProviderInteractionMode,
    },
};

/// One entry of the timeline.
#[derive(Clone, Debug, PartialEq)]
pub struct TimelineItem {
    pub id: String,
    /// What T3's find in thread names the item by (`entryId` in `orchestration.searchThread`):
    /// a message's id for user and agent messages, the item's own id otherwise.
    pub entry_id: String,
    /// When the item began (`startedAt`, else when T3 first sent it), as T3 writes times. The
    /// timeline's "new since your last visit" line compares it with the thread's last visit.
    pub time: String,
    pub ordinal: i64,
    pub run_id: Option<String>,
    pub status: ItemStatus,
    /// Carried over from the thread this one was forked from.
    pub inherited: bool,
    /// The thread an inherited item belongs to, which a fork from it forks.
    pub source_thread_id: Option<String>,
    pub kind: ItemKind,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ItemStatus {
    Pending,
    Running,
    Waiting,
    Completed,
    Failed,
    Cancelled,
    Interrupted,
}

impl ItemStatus {
    fn from_wire(status: &OrchestrationV2TurnItemStatus) -> Self {
        match status {
            OrchestrationV2TurnItemStatus::Idle | OrchestrationV2TurnItemStatus::Pending => {
                Self::Pending
            }
            OrchestrationV2TurnItemStatus::Running => Self::Running,
            OrchestrationV2TurnItemStatus::Waiting => Self::Waiting,
            OrchestrationV2TurnItemStatus::Completed => Self::Completed,
            OrchestrationV2TurnItemStatus::Failed => Self::Failed,
            OrchestrationV2TurnItemStatus::Cancelled => Self::Cancelled,
            OrchestrationV2TurnItemStatus::Interrupted => Self::Interrupted,
            OrchestrationV2TurnItemStatus::Unrecognized(_) => Self::Completed,
        }
    }

    pub fn is_in_progress(self) -> bool {
        matches!(self, Self::Pending | Self::Running | Self::Waiting)
    }

    pub fn label(self) -> &'static str {
        match self {
            Self::Pending => "pending",
            Self::Running => "running",
            Self::Waiting => "waiting",
            Self::Completed => "done",
            Self::Failed => "failed",
            Self::Cancelled => "cancelled",
            Self::Interrupted => "interrupted",
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ToolKind {
    Command,
    FileSearch,
    WebSearch,
    /// An MCP or provider tool.
    Tool,
}

#[derive(Clone, Debug, PartialEq)]
pub enum ItemKind {
    UserMessage {
        text: String,
        /// Sent while a run was active, to run after it.
        queued: bool,
        /// It started its run, now or after a queue, rather than steering one: the messages
        /// "Edit from here" rewinds to (`turn_start` and `queued_turn`).
        starts_turn: bool,
        /// The images sent with it.
        images: Vec<MessageImage>,
        /// The other files sent with it, such as a big paste (`attachments.rs`).
        files: Vec<MessageFile>,
    },
    AssistantMessage {
        text: String,
        streaming: bool,
    },
    Reasoning {
        text: String,
    },
    /// A proposed plan, in Markdown.
    Plan {
        /// The plan's id, which a message that implements it names (`sourcePlanRef`).
        plan_id: String,
        title: Option<String>,
        markdown: String,
    },
    Todo {
        explanation: Option<String>,
        steps: Vec<TodoStep>,
    },
    Tool {
        tool: ToolKind,
        /// What the tool did, in one line: the command, the search, the tool's name.
        summary: String,
        /// The tool's input, when there's more to it than the summary.
        input: Option<String>,
        output: Option<String>,
        /// T3 leaves a command's output out of projections and events; it's fetched with
        /// `orchestration.getTurnItem` when the call is expanded.
        output_omitted: bool,
        /// How many images the output holds, such as a browser tool's screenshots. They load
        /// by index (`tool-output-image`).
        images: usize,
        /// The page a preview tool opened, for Open in browser (`html_renders.rs`).
        link: Option<String>,
    },
    /// A subagent the agent started (T3's `subagent` turn item), which the timeline shows as
    /// T3's subagent row (`SubagentTimelineLink` in `chat/V2LifecycleRow.tsx`).
    Subagent {
        subagent_id: String,
        title: String,
        prompt: Option<String>,
        /// What it's doing.
        progress: Option<String>,
        /// What it found.
        result: Option<String>,
        driver: String,
        provider_instance_id: String,
        /// Its thread, when it has one to open.
        child_thread_id: Option<String>,
        /// T3's word for its status ("running", "idle", "completed"…), for its badge.
        status: String,
        started_at: Option<String>,
        completed_at: Option<String>,
    },
    /// A page the agent made with T3's `html_render`, kept as an attachment
    /// (`html_renders.rs`).
    HtmlRender {
        attachment_id: String,
        title: String,
    },
    FileChange {
        path: String,
        additions: Option<i64>,
        deletions: Option<i64>,
        /// The other files of a multi-file change, with what happened to each.
        changes: Vec<FileOperation>,
        diff: Option<String>,
    },
    Approval {
        request_id: String,
        title: Option<String>,
        prompt: Option<String>,
        /// What's being approved: `command`, `file-read`, `file-change`, `mcp-elicitation` (an
        /// MCP server's question, which T3 asks as an approval), or `permission`.
        request_kind: String,
        options: Vec<ApprovalOption>,
        /// The app an MCP server's question is for (`appName`).
        app_name: Option<String>,
        /// Why the agent asks.
        reason: Option<String>,
    },
    /// The agent asked the user questions.
    Question {
        request_id: String,
        title: Option<String>,
        questions: Vec<Question>,
    },
    /// The agent asked for a secret, such as a token, which it waits for
    /// (`chat/SecretRequestCard.tsx` in T3).
    SecretRequest {
        title: Option<String>,
        /// What the secret is, such as "GitHub token".
        label: String,
        reason: String,
        placeholder: Option<String>,
        /// `pending`, `saved`, `declined`, or `cancelled`.
        status: String,
    },
    Error {
        message: String,
        /// What kind of failure it was, such as `usage_limit`.
        class: String,
        /// The execution node that failed; a run's own failure is on its root node.
        node_id: Option<String>,
    },
    /// A one-line marker: a notice, a checkpoint, a compaction, an interrupt.
    Notice {
        text: String,
    },
}

/// An image attached to a user message.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct MessageImage {
    /// T3's attachment id, which `assets.createUrl` loads it by.
    pub attachment_id: String,
    pub name: String,
    pub mime_type: String,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Question {
    pub id: String,
    pub header: String,
    pub question: String,
    /// `(label, answer)`: the answer sent back is the option's value, or its label.
    pub options: Vec<(String, String)>,
    /// Whether a typed answer, and attachments, can answer it (`allowCustomAnswer`, on unless
    /// T3 says otherwise).
    pub allow_custom_answer: bool,
}

/// A file other than an image attached to a user message.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct MessageFile {
    pub name: String,
    pub mime_type: String,
    pub size_bytes: i64,
    /// It was a big paste that mustr attached as a file.
    pub pasted_text: bool,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct TodoStep {
    pub text: String,
    /// `pending`, `running`, or `completed`.
    pub status: String,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct FileOperation {
    pub operation: String,
    pub path: String,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ApprovalOption {
    pub decision: ProviderApprovalDecision,
    pub label: String,
    /// What to know before picking it, such as what "Always allow" means.
    pub warning: Option<String>,
}

/// The value of a generated string enum or untagged union as it appears on the wire.
pub(crate) fn wire_string(value: &impl Serialize) -> String {
    match serde_json::to_value(value) {
        Ok(serde_json::Value::String(text)) => text,
        Ok(other) => other.to_string(),
        Err(error) => {
            log::warn!("couldn't encode a T3 value: {error}");
            String::new()
        }
    }
}

/// Decodes any of T3's generated turn-item shapes as the canonical one.
pub(crate) fn canonical_item(item: &impl Serialize) -> Option<OrchestrationV2TurnItem> {
    let value = serde_json::to_value(item).ok()?;
    match serde_json::from_value::<OrchestrationV2TurnItem>(value) {
        Ok(OrchestrationV2TurnItem::Unrecognized(value)) => {
            log::debug!(
                "skipping a turn item mustr doesn't know: {:?}",
                value.get("type")
            );
            None
        }
        Ok(item) => Some(item),
        Err(error) => {
            log::warn!("skipping a turn item mustr can't read: {error}");
            None
        }
    }
}

fn non_empty(text: Option<&String>) -> Option<String> {
    text.map(|text| text.trim())
        .filter(|text| !text.is_empty())
        .map(str::to_string)
}

impl TimelineItem {
    pub fn from_turn_item(item: &OrchestrationV2TurnItem) -> Option<Self> {
        macro_rules! item {
            ($item:expr, $entry_id:expr, $kind:expr) => {
                Self {
                    id: $item.id.clone(),
                    entry_id: $entry_id,
                    time: $item
                        .started_at
                        .clone()
                        .unwrap_or_else(|| $item.updated_at.clone()),
                    ordinal: $item.ordinal,
                    run_id: $item.run_id.clone(),
                    status: ItemStatus::from_wire(&$item.status),
                    inherited: false,
                    source_thread_id: None,
                    kind: $kind,
                }
            };
            ($item:expr, $kind:expr) => {
                item!($item, $item.id.clone(), $kind)
            };
        }
        let notice = |text: String| ItemKind::Notice { text };
        Some(match item {
            OrchestrationV2TurnItem::UserMessage(item) => item!(
                item,
                item.message_id.clone(),
                ItemKind::UserMessage {
                    text: item.text.clone(),
                    queued: item.input_intent == OrchestrationV2UserMessageInputIntent::QueuedTurn,
                    starts_turn: matches!(
                        item.input_intent,
                        OrchestrationV2UserMessageInputIntent::TurnStart
                            | OrchestrationV2UserMessageInputIntent::QueuedTurn
                    ),
                    images: message_images(&item.attachments),
                    files: message_files(&item.attachments),
                }
            ),
            OrchestrationV2TurnItem::AssistantMessage(item) => item!(
                item,
                item.message_id.clone(),
                ItemKind::AssistantMessage {
                    text: item.text.clone(),
                    streaming: item.streaming,
                }
            ),
            OrchestrationV2TurnItem::Reasoning(item) => item!(
                item,
                ItemKind::Reasoning {
                    text: item.text.clone()
                }
            ),
            OrchestrationV2TurnItem::ProposedPlan(item) => item!(
                item,
                ItemKind::Plan {
                    plan_id: item.plan_id.clone(),
                    title: non_empty(item.title.as_ref()),
                    markdown: item.markdown.clone(),
                }
            ),
            OrchestrationV2TurnItem::TodoList(item) => item!(
                item,
                ItemKind::Todo {
                    explanation: non_empty(item.explanation.as_ref()),
                    steps: item
                        .steps
                        .iter()
                        .map(|step| TodoStep {
                            text: step.text.clone(),
                            status: wire_string(&step.status),
                        })
                        .collect(),
                }
            ),
            OrchestrationV2TurnItem::UserInputRequest(item) => item!(
                item,
                ItemKind::Question {
                    request_id: item.request_id.clone(),
                    title: non_empty(item.title.as_ref()),
                    questions: item
                        .questions
                        .iter()
                        .map(|question| Question {
                            id: question.id.clone(),
                            header: question.header.clone(),
                            question: question.question.clone(),
                            options: question
                                .options
                                .iter()
                                .map(|option| {
                                    let answer = option
                                        .value
                                        .clone()
                                        .unwrap_or_else(|| option.label.clone());
                                    (option.label.clone(), answer)
                                })
                                .collect(),
                            allow_custom_answer: question.allow_custom_answer != Some(false),
                        })
                        .collect(),
                }
            ),
            OrchestrationV2TurnItem::FileChange(item) => item!(
                item,
                ItemKind::FileChange {
                    path: item.file_name.clone(),
                    additions: item.additions,
                    deletions: item.deletions,
                    changes: item
                        .changes
                        .iter()
                        .flatten()
                        .map(|change| FileOperation {
                            operation: change.operation.clone(),
                            path: change.path.clone(),
                        })
                        .collect(),
                    diff: non_empty(item.diff_str.as_ref()),
                }
            ),
            OrchestrationV2TurnItem::CommandExecution(item) => item!(
                item,
                ItemKind::Tool {
                    tool: ToolKind::Command,
                    summary: display_command(&item.input),
                    // The whole command, when one line can't show it.
                    input: Some(display_command(&item.input))
                        .filter(|command| command.contains('\n') || command.chars().count() > 100),
                    output: non_empty(item.output.as_ref()),
                    output_omitted: item.output_omitted.unwrap_or(false),
                    images: 0,
                    link: None,
                }
            ),
            OrchestrationV2TurnItem::FileSearch(item) => item!(
                item,
                ItemKind::Tool {
                    tool: ToolKind::FileSearch,
                    summary: non_empty(item.title.as_ref())
                        .unwrap_or_else(|| "Searched files".to_string()),
                    input: None,
                    output: item.results.as_ref().map(|results| {
                        format!(
                            "{} {}",
                            results.len(),
                            if results.len() == 1 {
                                "result"
                            } else {
                                "results"
                            }
                        )
                    }),
                    output_omitted: false,
                    images: 0,
                    link: None,
                }
            ),
            OrchestrationV2TurnItem::WebSearch(item) => item!(
                item,
                ItemKind::Tool {
                    tool: ToolKind::WebSearch,
                    summary: non_empty(item.title.as_ref()).unwrap_or_else(|| {
                        item.patterns
                            .as_ref()
                            .map(|patterns| patterns.join(", "))
                            .filter(|patterns| !patterns.is_empty())
                            .unwrap_or_else(|| "Searched the web".to_string())
                    }),
                    input: None,
                    output: None,
                    output_omitted: false,
                    images: 0,
                    link: None,
                }
            ),
            OrchestrationV2TurnItem::DynamicTool(item)
                if item.status == OrchestrationV2TurnItemStatus::Completed
                    && let Some((attachment_id, title)) = crate::html_renders::html_render_of(
                        item.tool_name.as_deref(),
                        item.output.as_ref(),
                    ) =>
            {
                item!(
                    item,
                    ItemKind::HtmlRender {
                        attachment_id,
                        title
                    }
                )
            }
            OrchestrationV2TurnItem::DynamicTool(item) => item!(
                item,
                ItemKind::Tool {
                    tool: ToolKind::Tool,
                    summary: non_empty(item.title.as_ref())
                        .or_else(|| non_empty(item.tool_name.as_ref()))
                        .unwrap_or_else(|| "Tool call".to_string()),
                    input: json_text(&item.input),
                    output: item
                        .output
                        .as_ref()
                        .and_then(|output| json_text(&without_image_data(output))),
                    output_omitted: item.output_omitted.unwrap_or(false),
                    // T3 serves a tool's images only from its full output, so none are counted
                    // while it's left out (`dynamicToolOutputImageResources` in T3).
                    images: if item.output_omitted == Some(true) {
                        0
                    } else {
                        item.output.as_ref().map_or(0, tool_output_images)
                    },
                    link: crate::html_renders::preview_link_of(
                        item.tool_name.as_deref(),
                        &item.input,
                    ),
                }
            ),
            OrchestrationV2TurnItem::Subagent(item) => item!(
                item,
                ItemKind::Subagent {
                    subagent_id: item.subagent_id.clone(),
                    title: non_empty(item.title.as_ref())
                        .map(|title| crate::family_bar::format_subagent_title(&title))
                        .unwrap_or_else(|| "Subagent".to_string()),
                    prompt: non_empty(Some(&item.prompt)),
                    progress: non_empty(item.progress.as_ref()),
                    result: non_empty(item.result.as_ref()),
                    driver: item.driver.clone(),
                    provider_instance_id: item.provider_instance_id.clone(),
                    child_thread_id: item.child_thread_id.clone(),
                    status: wire_string(&item.status),
                    started_at: item.started_at.clone(),
                    completed_at: item.completed_at.clone(),
                }
            ),
            OrchestrationV2TurnItem::ApprovalRequest(item) => item!(
                item,
                ItemKind::Approval {
                    request_id: item.request_id.clone(),
                    title: non_empty(item.title.as_ref()),
                    prompt: non_empty(item.prompt.as_ref()),
                    request_kind: wire_string(&item.request_kind),
                    options: item
                        .options
                        .iter()
                        .flatten()
                        .map(|option| ApprovalOption {
                            decision: option.decision.clone(),
                            label: option.label.clone(),
                            warning: non_empty(option.warning.as_ref()),
                        })
                        .collect(),
                    app_name: non_empty(item.app_name.as_ref()),
                    reason: non_empty(item.reason.as_ref()),
                }
            ),
            OrchestrationV2TurnItem::Error(item) => item!(
                item,
                ItemKind::Error {
                    message: item.failure.message.clone(),
                    class: wire_string(&item.failure.class),
                    node_id: item.node_id.clone(),
                }
            ),
            OrchestrationV2TurnItem::SystemNotice(item) => {
                item!(item, notice(item.message.clone()))
            }
            OrchestrationV2TurnItem::Notification(item) => {
                item!(item, notice(item.summary.clone()))
            }
            OrchestrationV2TurnItem::Checkpoint(item) => item!(
                item,
                notice(non_empty(item.title.as_ref()).unwrap_or_else(|| "Checkpoint".into()))
            ),
            OrchestrationV2TurnItem::Compaction(item) => item!(
                item,
                notice(
                    non_empty(item.title.as_ref())
                        .unwrap_or_else(|| "Compacted the conversation".into())
                )
            ),
            OrchestrationV2TurnItem::RunInterruptRequest(item) => {
                item!(item, notice("Interrupt requested".into()))
            }
            OrchestrationV2TurnItem::RunInterruptResult(item) => {
                item!(item, notice("Interrupted".into()))
            }
            OrchestrationV2TurnItem::Handoff(item) => item!(
                item,
                notice(non_empty(item.title.as_ref()).unwrap_or_else(|| "Handed off".into()))
            ),
            OrchestrationV2TurnItem::Fork(item) => item!(
                item,
                notice(
                    non_empty(item.title.as_ref()).unwrap_or_else(|| "Forked the thread".into())
                )
            ),
            OrchestrationV2TurnItem::ThreadCreated(item) => item!(
                item,
                notice(non_empty(item.title.as_ref()).unwrap_or_else(|| "Started a thread".into()))
            ),
            OrchestrationV2TurnItem::SecretRequest(item) => item!(
                item,
                ItemKind::SecretRequest {
                    title: non_empty(item.title.as_ref()),
                    label: item.label.clone(),
                    reason: item.reason.clone(),
                    placeholder: non_empty(item.placeholder.as_ref()),
                    status: wire_string(&item.secret_status),
                }
            ),
            OrchestrationV2TurnItem::Unrecognized(_) => return None,
        })
    }
}

/// A command as the agent meant it: without the login-shell wrapper (`/usr/bin/zsh -lc "…"`)
/// that agents put around every command.
pub fn display_command(input: &str) -> String {
    let input = input.trim();
    let unwrapped = input.split_once(' ').and_then(|(shell, rest)| {
        let shell_name = shell.rsplit('/').next()?;
        if !matches!(shell_name, "sh" | "bash" | "zsh" | "fish") {
            return None;
        }
        let rest = rest.trim_start();
        let script = ["-lc ", "-c ", "-l -c "]
            .iter()
            .find_map(|flag| rest.strip_prefix(flag))?
            .trim();
        let quoted = ['"', '\'']
            .iter()
            .find_map(|quote| script.strip_prefix(*quote)?.strip_suffix(*quote));
        Some(quoted.unwrap_or(script).to_string())
    });
    unwrapped.unwrap_or_else(|| input.to_string())
}

fn message_images(attachments: &[ChatAttachment]) -> Vec<MessageImage> {
    attachments
        .iter()
        .filter_map(|attachment| match attachment {
            ChatAttachment::Image(image) => Some(MessageImage {
                attachment_id: image.id.clone(),
                name: image.name.clone(),
                mime_type: image.mime_type.clone(),
            }),
            _ => None,
        })
        .collect()
}

fn message_files(attachments: &[ChatAttachment]) -> Vec<MessageFile> {
    attachments
        .iter()
        .filter_map(|attachment| match attachment {
            ChatAttachment::File(file) => Some(MessageFile {
                name: file.name.clone(),
                mime_type: file.mime_type.clone(),
                size_bytes: file.size_bytes,
                pasted_text: file.source.is_some(),
            }),
            _ => None,
        })
        .collect()
}

/// The most images T3 serves from one tool's output (`MAX_TOOL_OUTPUT_IMAGES`).
pub const MAX_TOOL_OUTPUT_IMAGES: usize = 8;

/// The blocks of a tool's output: a list, `{ content: [...] }`, or one block
/// (`outputBlocks` in T3's `packages/shared/src/toolOutput.ts`).
fn output_blocks(output: &serde_json::Value) -> Vec<&serde_json::Value> {
    match output {
        serde_json::Value::Array(blocks) => blocks.iter().collect(),
        serde_json::Value::Object(object) => match object.get("content") {
            Some(serde_json::Value::Array(blocks)) => blocks.iter().collect(),
            _ => vec![output],
        },
        other => vec![other],
    }
}

/// Whether a block is an image T3 serves: `{ type: "image", mimeType, data }` or
/// `{ type: "image", source: { type: "base64", media_type, data } }`, of a raster type agents
/// take (`readToolOutputImage` in T3).
fn is_tool_output_image(block: &serde_json::Value) -> bool {
    if block.get("type").and_then(|kind| kind.as_str()) != Some("image") {
        return false;
    }
    let mime_type = match block.get("source") {
        Some(source) if source.is_object() => {
            if source.get("type").and_then(|kind| kind.as_str()) != Some("base64") {
                return false;
            }
            source.get("media_type")
        }
        _ => block.get("mimeType"),
    };
    mime_type
        .and_then(|mime| mime.as_str())
        .is_some_and(|mime| {
            matches!(
                mime.to_ascii_lowercase().as_str(),
                "image/gif" | "image/jpeg" | "image/png" | "image/webp"
            )
        })
}

/// How many images a tool's output holds, up to the most T3 serves.
fn tool_output_images(output: &serde_json::Value) -> usize {
    output_blocks(output)
        .into_iter()
        .filter(|block| is_tool_output_image(block))
        .take(MAX_TOOL_OUTPUT_IMAGES)
        .count()
}

/// The output with its images' base64 data left out, to show as text.
fn without_image_data(output: &serde_json::Value) -> serde_json::Value {
    fn strip(value: &serde_json::Value) -> serde_json::Value {
        match value {
            serde_json::Value::Array(values) => {
                serde_json::Value::Array(values.iter().map(strip).collect())
            }
            serde_json::Value::Object(object) => {
                if object.get("type").and_then(|kind| kind.as_str()) == Some("image") {
                    let mut object = object.clone();
                    object.remove("data");
                    if let Some(serde_json::Value::Object(source)) = object.get_mut("source") {
                        source.remove("data");
                    }
                    serde_json::Value::Object(object)
                } else {
                    serde_json::Value::Object(
                        object
                            .iter()
                            .map(|(key, value)| (key.clone(), strip(value)))
                            .collect(),
                    )
                }
            }
            other => other.clone(),
        }
    }
    strip(output)
}

fn json_text(value: &serde_json::Value) -> Option<String> {
    match value {
        serde_json::Value::Null => None,
        serde_json::Value::String(text) => non_empty(Some(text)),
        other => serde_json::to_string_pretty(other).ok(),
    }
}

/// How a thread is doing, for its header and the sidebar.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ThreadActivity {
    /// The agent is waiting for an approval or an answer.
    NeedsInput,
    /// A run is preparing, starting, running, or waiting (on a tool, a subagent).
    Working(String),
    Failed,
    /// No run is active. Holds how the last one ended, if there was one.
    Idle(Option<String>),
}

impl ThreadActivity {
    pub fn label(&self) -> String {
        match self {
            Self::NeedsInput => "needs input".into(),
            Self::Working(status) => status.clone(),
            Self::Failed => "failed".into(),
            Self::Idle(Some(status)) => status.replace('_', " "),
            Self::Idle(None) => "idle".into(),
        }
    }
}

/// Tokens in the agent's context, from the latest provider turn that reported them.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ContextUsage {
    pub used_tokens: i64,
    pub max_tokens: Option<i64>,
}

/// A message waiting for the active run to finish.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct QueuedMessage {
    pub run_id: String,
    pub text: String,
}

/// A plan the agent proposed, as T3 tracks it: it's `active` until it's implemented or a
/// newer plan supersedes it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ProposedPlan {
    pub id: String,
    pub run_id: Option<String>,
    pub status: OrchestrationV2PlanArtifactProposedPlanStatus,
}

/// The plan the composer offers to implement or refine: the latest proposed plan of the latest
/// run, once that run settles.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ActionablePlan {
    pub plan_id: String,
    /// The timeline item that shows it.
    pub item_id: String,
    pub title: Option<String>,
    pub markdown: String,
}

/// Where "Edit from here" rewinds a user message to: the checkpoint T3 took before its run
/// (`deriveRevertTurnCountByUserMessageId` and `findRevertCheckpoint` in T3).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RevertTarget {
    /// How many of the thread's runs stay: the checkpoint's run ordinal, or 0 for the thread's
    /// start.
    pub turn_count: i64,
    pub checkpoint_id: String,
    pub scope_id: String,
}

/// What Resume does in an empty composer (`chat/useComposerResume.ts` in T3).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Resumable {
    /// The interrupted or usage-limited run to continue, if there is one.
    pub continue_run_id: Option<String>,
    /// Whether queued messages are held, waiting for `queue.resume`.
    pub held_queue: bool,
}

/// What the goal line says (`presentProviderGoal` in T3's
/// `client-runtime/src/state/threadExecution.ts`).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct GoalLine {
    pub title: String,
    pub objective: String,
    /// What the goal has used so far: tokens, time, and checks.
    pub usage: Option<String>,
    /// Whether `/goal resume` would pick it up again.
    pub can_resume: bool,
}

impl GoalLine {
    pub fn new(goal: &OrchestrationV2ProviderGoal, working: bool) -> Self {
        use OrchestrationV2ProviderGoalStatus as Status;
        let mut usage = Vec::new();
        if let Some(tokens) = goal.tokens_used.filter(|tokens| *tokens > 0) {
            usage.push(match goal.token_budget {
                Some(budget) => format!(
                    "{} / {} tokens",
                    format_goal_tokens(tokens),
                    format_goal_tokens(budget)
                ),
                None => format!("{} tokens", format_goal_tokens(tokens)),
            });
        }
        if let Some(seconds) = goal.time_used_seconds.filter(|seconds| *seconds >= 60) {
            usage.push(format_duration(seconds));
        }
        if let Some(checks) = goal.checks.filter(|checks| *checks > 0) {
            usage.push(format!(
                "{checks} {}",
                if checks == 1 { "check" } else { "checks" }
            ));
        }
        let title = match &goal.status {
            Status::Active if !working => "Goal set",
            Status::Active => "Pursuing goal",
            Status::Paused => "Goal paused",
            Status::Blocked => "Goal blocked",
            Status::UsageLimited => "Goal hit a usage limit",
            Status::BudgetLimited => "Goal reached its token budget",
            Status::Complete => "Goal complete",
            Status::Unrecognized(_) => "Goal",
        };
        Self {
            title: title.to_string(),
            objective: goal.objective.trim().to_string(),
            usage: (!usage.is_empty()).then(|| usage.join(" · ")),
            can_resume: !matches!(goal.status, Status::Active | Status::Complete),
        }
    }
}

/// "800", "12k", "1.5m" (`formatGoalTokens` in T3).
fn format_goal_tokens(tokens: i64) -> String {
    if tokens < 1_000 {
        tokens.to_string()
    } else if tokens < 1_000_000 {
        format!("{}k", (tokens as f64 / 1_000.).round())
    } else {
        let millions = format!("{:.1}", tokens as f64 / 1_000_000.);
        format!("{}m", millions.trim_end_matches(".0"))
    }
}

/// "5m", "1h 2m 3s", for a minute or more (`formatDuration` in T3's
/// `packages/shared/src/orchestrationTiming.ts`).
fn format_duration(seconds: i64) -> String {
    let hours = seconds / 3_600;
    let minutes = (seconds % 3_600) / 60;
    let seconds = seconds % 60;
    [(hours, "h"), (minutes, "m"), (seconds, "s")]
        .into_iter()
        .filter(|(amount, _)| *amount > 0)
        .map(|(amount, unit)| format!("{amount}{unit}"))
        .collect::<Vec<_>>()
        .join(" ")
}

/// What changed when an event was applied.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Applied {
    /// Something the view shows changed.
    pub changed: bool,
}

/// A request T3 is waiting on the user for.
#[derive(Clone, Debug, PartialEq)]
pub struct PendingRequest {
    pub request_id: String,
    /// The timeline item that shows the request, when there is one.
    pub item: Option<TimelineItem>,
    pub is_question: bool,
    /// Whether the agent's session can still take an answer. T3's web app only answers live
    /// requests; others are shown, but not answerable.
    pub can_respond: bool,
    /// Whether the agent takes a reason with a decline (`supportsFeedback`, T3 0.1.3): the
    /// composer's message then declines the request and tells the agent why. Older servers
    /// never say so.
    pub supports_feedback: bool,
}

/// What the timeline shows: single items, and runs of the agent's work (tool calls, file
/// edits, reasoning) collapsed into one row.
#[derive(Clone, Debug, PartialEq)]
pub enum Row {
    Item(usize),
    /// Indices of consecutive work items.
    Work(Vec<usize>),
}

impl Row {
    /// A key that stays the same while the row's first item does.
    pub fn first_index(&self) -> usize {
        match self {
            Row::Item(index) => *index,
            Row::Work(indices) => indices.first().copied().unwrap_or_default(),
        }
    }
}

#[derive(Clone, Debug)]
pub struct ThreadState {
    pub thread: OrchestrationV2AppThread,
    /// The last event sequence applied, to resume the subscription after it.
    pub sequence: i64,
    /// Older history the snapshot left out.
    pub has_more_history: bool,
    items: Vec<TimelineItem>,
    /// Bumped by every change to an item, by item index, so views can tell which rows changed.
    revisions: Vec<u64>,
    next_revision: u64,
    runs: Vec<OrchestrationV2Run>,
    runtime_requests: Vec<OrchestrationV2RuntimeRequest>,
    messages: Vec<OrchestrationV2ConversationMessage>,
    provider_turns: Vec<OrchestrationV2ProviderTurn>,
    provider_threads: Vec<OrchestrationV2ProviderThread>,
    /// The thread's subagents and delegated tasks, for its family bar (`family_bar.rs`).
    subagents: Vec<OrchestrationV2Subagent>,
    plans: Vec<ProposedPlan>,
    /// Each run's attempts at its provider turn, for which runs "Edit from here" can rewind
    /// the agent's conversation to (`history.rs`).
    attempts: Vec<OrchestrationV2RunAttempt>,
    /// The checkpoints T3 took, which "Edit from here" rolls back to.
    checkpoints: Vec<OrchestrationV2Checkpoint>,
    /// The agent sessions the thread holds, which "Restart agent session" detaches.
    provider_sessions: Vec<OrchestrationV2ProviderSession>,
    /// Whether the debug fixture played on this snapshot (`debug_fixture.rs`).
    #[cfg(debug_assertions)]
    pub(crate) debug_fixture_applied: bool,
}

impl ThreadState {
    pub fn new(projection: OrchestrationV2ThreadProjection, sequence: i64) -> Self {
        let items = timeline_items(&projection);
        let revisions = vec![0; items.len()];
        Self {
            thread: projection.thread,
            sequence,
            has_more_history: false,
            items,
            revisions,
            next_revision: 1,
            runs: projection.runs,
            runtime_requests: projection.runtime_requests,
            messages: projection.messages,
            provider_turns: projection.provider_turns,
            provider_threads: projection.provider_threads,
            plans: projection.plans.iter().filter_map(proposed_plan).collect(),
            subagents: projection.subagents,
            attempts: projection.attempts,
            checkpoints: projection.checkpoints,
            provider_sessions: projection.provider_sessions,
            #[cfg(debug_assertions)]
            debug_fixture_applied: false,
        }
    }

    /// Applies an event of the debug fixture (`debug_fixture.rs`), without moving the sequence
    /// the subscription resumes from.
    #[cfg(debug_assertions)]
    pub(crate) fn apply_fixture_event(&mut self, event: OrchestrationV2DomainEvent) -> Applied {
        self.apply_event(event)
    }

    /// The thread's subagents and delegated tasks, as T3 last sent them.
    pub fn subagents(&self) -> &[OrchestrationV2Subagent] {
        &self.subagents
    }

    /// Every item of the timeline, including ones [`Self::rows`] hides.
    pub fn items(&self) -> &[TimelineItem] {
        &self.items
    }

    pub fn item(&self, index: usize) -> Option<&TimelineItem> {
        self.items.get(index)
    }

    pub fn revision(&self, index: usize) -> u64 {
        self.revisions.get(index).copied().unwrap_or_default()
    }

    pub fn thread_id(&self) -> &str {
        &self.thread.id
    }

    /// Applies one item of `orchestration.subscribeThread`, the way T3's clients do
    /// (`applyOrchestrationV2ProjectionEvent` in T3's `client-runtime`): a snapshot replaces
    /// everything, events at or before the last sequence are dropped, and every event carries
    /// the whole entity, which replaces the one with its id.
    pub fn apply(&mut self, stream_item: OrchestrationV2ThreadStreamItem) -> Applied {
        match stream_item {
            OrchestrationV2ThreadStreamItem::Snapshot(snapshot) => {
                let has_more_history = snapshot.has_more_history.unwrap_or(false);
                let next_revision = self.next_revision;
                *self = Self::new(snapshot.projection, snapshot.snapshot_sequence);
                self.revisions = vec![next_revision; self.items.len()];
                self.next_revision = next_revision + 1;
                self.has_more_history = has_more_history;
                Applied { changed: true }
            }
            OrchestrationV2ThreadStreamItem::Event(event) => {
                if event.sequence <= self.sequence {
                    return Applied::default();
                }
                self.sequence = event.sequence;
                self.apply_event(event.event)
            }
            OrchestrationV2ThreadStreamItem::Event2(event) => {
                // An event type this build doesn't know: skip it, but keep its place.
                t3_api::forward_compat::note_unrecognized("thread event", &event.event.type_);
                self.sequence = self.sequence.max(event.sequence);
                Applied::default()
            }
            OrchestrationV2ThreadStreamItem::Synchronized(_)
            | OrchestrationV2ThreadStreamItem::Unrecognized(_) => Applied::default(),
        }
    }

    fn apply_event(&mut self, event: OrchestrationV2DomainEvent) -> Applied {
        let changed = Applied { changed: true };
        match event {
            OrchestrationV2DomainEvent::Variant0 { payload, .. }
            | OrchestrationV2DomainEvent::Variant1 { payload, .. } => {
                self.thread = payload;
                changed
            }
            OrchestrationV2DomainEvent::Variant3 { payload, .. }
            | OrchestrationV2DomainEvent::Variant4 { payload, .. } => {
                upsert(&mut self.runs, payload, |run| &run.id);
                changed
            }
            // `run-attempt.created` and `run-attempt.updated`.
            OrchestrationV2DomainEvent::Variant6 { payload, .. }
            | OrchestrationV2DomainEvent::Variant7 { payload, .. } => {
                upsert(&mut self.attempts, payload, |attempt| &attempt.id);
                changed
            }
            // `provider-session.attached` and `updated`, and `detached`, which drops it, as T3's
            // projection does.
            OrchestrationV2DomainEvent::Variant10 { payload, .. } => {
                upsert(&mut self.provider_sessions, payload, |session| &session.id);
                changed
            }
            OrchestrationV2DomainEvent::Variant11 { payload, .. } => {
                self.provider_sessions
                    .retain(|session| session.id != payload.provider_session_id);
                changed
            }
            // `subagent.updated`, for the family bar.
            OrchestrationV2DomainEvent::Variant9 { payload, .. } => {
                upsert(&mut self.subagents, payload, |subagent| &subagent.id);
                changed
            }
            OrchestrationV2DomainEvent::Variant12 { payload, .. } => {
                upsert(&mut self.provider_threads, payload, |thread| &thread.id);
                changed
            }
            OrchestrationV2DomainEvent::Variant13 { mut payload, .. } => {
                // T3 leaves the usage out of updates that don't change it.
                if payload.token_usage.is_none()
                    && let Some(previous) = self
                        .provider_turns
                        .iter()
                        .find(|turn| turn.id == payload.id)
                {
                    payload.token_usage = previous.token_usage.clone();
                }
                upsert(&mut self.provider_turns, payload, |turn| &turn.id);
                changed
            }
            OrchestrationV2DomainEvent::Variant14 { payload, .. } => {
                upsert(&mut self.runtime_requests, payload, |request| &request.id);
                changed
            }
            OrchestrationV2DomainEvent::Variant15 { payload, .. } => {
                upsert(&mut self.messages, payload, |message| &message.id);
                changed
            }
            OrchestrationV2DomainEvent::Variant16 { payload, .. } => {
                match TimelineItem::from_turn_item(&payload) {
                    Some(item) => self.upsert_item(item),
                    None => Applied::default(),
                }
            }
            OrchestrationV2DomainEvent::Variant17 { payload, .. } => {
                match proposed_plan(&payload) {
                    Some(plan) => {
                        upsert(&mut self.plans, plan, |plan| &plan.id);
                        changed
                    }
                    None => Applied::default(),
                }
            }
            // `checkpoint.captured`.
            OrchestrationV2DomainEvent::Variant19 { payload, .. } => {
                upsert(&mut self.checkpoints, payload, |checkpoint| &checkpoint.id);
                changed
            }
            _ => Applied::default(),
        }
    }

    /// Replaces the item with `item`'s id, or inserts it before the first later local item,
    /// as T3's `upsertVisibleTurnItem` does.
    fn upsert_item(&mut self, item: TimelineItem) -> Applied {
        let revision = self.next_revision;
        self.next_revision += 1;
        let existing = self
            .items
            .iter()
            .rposition(|existing| existing.id == item.id);
        let mut item = item;
        if let Some(index) = existing {
            // An item without a start time keeps the time T3 first sent it at.
            item.time = self.items[index].time.clone();
        }
        if let Some(index) = existing
            && !self.items[index].inherited
            && self.items[index].ordinal == item.ordinal
        {
            if self.items[index] == item {
                return Applied::default();
            }
            self.items[index] = item;
            self.revisions[index] = revision;
            return Applied { changed: true };
        }
        if let Some(index) = existing {
            self.items.remove(index);
            self.revisions.remove(index);
        }
        let later = |existing: &TimelineItem| {
            !existing.inherited
                && (existing.ordinal, existing.id.as_str()) > (item.ordinal, item.id.as_str())
        };
        let index = self
            .items
            .iter()
            .position(later)
            .unwrap_or(self.items.len());
        self.items.insert(index, item);
        self.revisions.insert(index, revision);
        Applied { changed: true }
    }

    fn run(&self, run_id: &str) -> Option<&OrchestrationV2Run> {
        self.runs.iter().find(|run| run.id == run_id)
    }

    /// Whether the timeline shows `item`. Like T3's `isOrchestrationV2TurnItemVisible`, it hides
    /// the items of rolled-back runs, and queued messages that were cancelled before they ran.
    pub fn is_visible(&self, item: &TimelineItem) -> bool {
        let Some(run) = item.run_id.as_deref().and_then(|run_id| self.run(run_id)) else {
            return true;
        };
        if run.status == OrchestrationV2RunStatus::RolledBack {
            return false;
        }
        !(matches!(item.kind, ItemKind::UserMessage { queued: true, .. })
            && run.status == OrchestrationV2RunStatus::Cancelled)
    }

    /// The timeline's rows: visible items in order, with consecutive work items grouped.
    pub fn rows(&self) -> Vec<Row> {
        let mut rows = Vec::new();
        for (index, item) in self.items.iter().enumerate() {
            if !self.is_visible(item) {
                continue;
            }
            if item.is_work() {
                if let Some(Row::Work(indices)) = rows.last_mut() {
                    indices.push(index);
                } else {
                    rows.push(Row::Work(vec![index]));
                }
            } else {
                rows.push(Row::Item(index));
            }
        }
        rows
    }

    /// A row's revision: it changes whenever anything the row shows does.
    pub fn row_revision(&self, row: &Row) -> u64 {
        match row {
            Row::Item(index) => self.revision(*index),
            Row::Work(indices) => indices
                .iter()
                .map(|index| self.revision(*index))
                .max()
                .unwrap_or_default(),
        }
    }

    /// The run the agent is working on, if any (the latest that's preparing, starting,
    /// running, or waiting, as T3 picks it).
    pub fn active_run(&self) -> Option<&OrchestrationV2Run> {
        self.runs.iter().rev().find(|run| {
            matches!(
                run.status,
                OrchestrationV2RunStatus::Preparing
                    | OrchestrationV2RunStatus::Starting
                    | OrchestrationV2RunStatus::Running
                    | OrchestrationV2RunStatus::Waiting
            )
        })
    }

    /// Messages waiting for the active run, in the order they'll run.
    pub fn queued_messages(&self) -> Vec<QueuedMessage> {
        let mut queued = self
            .runs
            .iter()
            .filter(|run| run.status == OrchestrationV2RunStatus::Queued)
            .collect::<Vec<_>>();
        queued.sort_by_key(|run| run.queue_position.unwrap_or(run.ordinal));
        queued
            .into_iter()
            .map(|run| QueuedMessage {
                run_id: run.id.clone(),
                text: self
                    .messages
                    .iter()
                    .find(|message| message.id == run.user_message_id)
                    .map(|message| message.text.clone())
                    .unwrap_or_default(),
            })
            .collect()
    }

    /// Whether the queue can be put in another order (`queued-run.reorder`): the agent's session
    /// takes queued messages (`canReorder` in T3's `client-runtime/src/state/threadWorkflows.ts`).
    pub fn can_reorder_queue(&self) -> bool {
        self.provider_session()
            .is_some_and(|session| session.capabilities.turns.supports_queued_messages)
    }

    /// Whether a queued message can start a run now (`queued-run.start`): no run is active and
    /// the thread is neither archived nor deleted (`canSendNow` in T3).
    pub fn can_start_queued(&self) -> bool {
        !self.is_running() && self.thread.archived_at.is_none() && self.thread.deleted_at.is_none()
    }

    /// The goal the agent works toward (`/goal`), from the thread's provider thread.
    pub fn goal(&self) -> Option<&OrchestrationV2ProviderGoal> {
        let provider_thread_id = self.thread.active_provider_thread_id.as_deref()?;
        self.provider_threads
            .iter()
            .find(|thread| thread.id == provider_thread_id)?
            .goal
            .as_ref()
    }

    /// Whether a run is active, so a new message can queue or steer.
    pub fn is_running(&self) -> bool {
        self.active_run().is_some()
    }

    /// Requests T3 is waiting on the user for: approvals and questions, oldest first. T3's web
    /// app leaves out auth refreshes and dynamic tool calls, which the user can't answer here.
    pub fn pending_requests(&self) -> Vec<PendingRequest> {
        self.runtime_requests
            .iter()
            .filter(|request| request.status == OrchestrationV2RuntimeRequestStatus::Pending)
            .filter(|request| {
                !matches!(
                    &request.kind,
                    OrchestrationV2RuntimeRequestKind::String(
                        OrchestrationV2RuntimeRequestKindString::AuthRefresh
                            | OrchestrationV2RuntimeRequestKindString::DynamicToolCall
                    )
                )
            })
            .map(|request| {
                let item = self
                    .items
                    .iter()
                    .find(|item| item.request_id() == Some(request.id.as_str()))
                    .cloned();
                PendingRequest {
                    request_id: request.id.clone(),
                    is_question: matches!(
                        &request.kind,
                        OrchestrationV2RuntimeRequestKind::String(
                            OrchestrationV2RuntimeRequestKindString::UserInput
                        )
                    ) || matches!(
                        item.as_ref().map(|item| &item.kind),
                        Some(ItemKind::Question { .. })
                    ),
                    item,
                    can_respond: matches!(
                        request.response_capability,
                        OrchestrationV2RuntimeRequestResponseCapability::Live(_)
                    ),
                    supports_feedback: matches!(
                        &request.response_capability,
                        OrchestrationV2RuntimeRequestResponseCapability::Live(live)
                            if live.supports_feedback == Some(true)
                    ),
                }
            })
            .collect()
    }

    pub fn activity(&self) -> ThreadActivity {
        if !self.pending_requests().is_empty() {
            return ThreadActivity::NeedsInput;
        }
        if let Some(run) = self.active_run() {
            return ThreadActivity::Working(wire_string(&run.status));
        }
        if self
            .runs
            .iter()
            .any(|run| run.status == OrchestrationV2RunStatus::Queued)
        {
            return ThreadActivity::Working("queued".into());
        }
        match self.runs.iter().max_by_key(|run| run.ordinal) {
            Some(run) if run.status == OrchestrationV2RunStatus::Failed => ThreadActivity::Failed,
            Some(run) => ThreadActivity::Idle(Some(wire_string(&run.status))),
            None => ThreadActivity::Idle(None),
        }
    }

    /// The agent's context use: the newest provider turn's usage, or else the active provider
    /// thread's, as T3's context meter reads it.
    pub fn context_usage(&self) -> Option<ContextUsage> {
        let from_turn = self
            .provider_turns
            .iter()
            .filter_map(|turn| turn.token_usage.as_ref())
            .max_by(|left, right| left.updated_at.cmp(&right.updated_at))
            .map(|usage| ContextUsage {
                used_tokens: usage.used_tokens,
                max_tokens: usage.max_tokens,
            });
        from_turn.or_else(|| {
            let active = self.thread.active_provider_thread_id.as_deref()?;
            let usage = self
                .provider_threads
                .iter()
                .find(|thread| thread.id == active)?
                .context_usage
                .as_ref()?;
            Some(ContextUsage {
                used_tokens: usage.used_tokens,
                max_tokens: usage.max_tokens,
            })
        })
    }

    /// The model the thread runs, as T3 names it.
    pub fn model(&self) -> Option<String> {
        self.thread
            .model_selection
            .model
            .as_str()
            .map(str::to_string)
    }

    /// Replaces an item's omitted output with the one T3 sent for it, with the number of images
    /// it holds.
    pub fn set_output(&mut self, item_id: &str, output: Option<String>, images: usize) -> Applied {
        let revision = self.next_revision;
        let Some(index) = self.items.iter().position(|item| item.id == item_id) else {
            return Applied::default();
        };
        let ItemKind::Tool {
            output: existing,
            output_omitted,
            images: existing_images,
            ..
        } = &mut self.items[index].kind
        else {
            return Applied::default();
        };
        *existing = output;
        *output_omitted = false;
        *existing_images = images;
        self.next_revision += 1;
        self.revisions[index] = revision;
        Applied { changed: true }
    }

    /// The latest run that started, ignoring queued runs and ones cancelled before they started
    /// (`latestExecutedRun` in T3's `packages/shared/src/orchestrationV2ThreadError.ts`). A run
    /// that hasn't finished counts as the latest, since held queues can run out of order.
    pub fn latest_executed_run(&self) -> Option<&OrchestrationV2Run> {
        let end = |run: &OrchestrationV2Run| run.completed_at.clone();
        self.runs
            .iter()
            .filter(|run| run.status != OrchestrationV2RunStatus::Queued)
            .filter(|run| {
                !(run.status == OrchestrationV2RunStatus::Cancelled && run.started_at.is_none())
            })
            .max_by(|left, right| {
                // Unfinished runs (no end) sort last; ties go to the higher ordinal.
                match (end(left), end(right)) {
                    (None, None) => left.ordinal.cmp(&right.ordinal),
                    (None, Some(_)) => std::cmp::Ordering::Greater,
                    (Some(_), None) => std::cmp::Ordering::Less,
                    (Some(left_end), Some(right_end)) => left_end
                        .cmp(&right_end)
                        .then(left.ordinal.cmp(&right.ordinal)),
                }
            })
    }

    /// The failure of a failed run's own (root) node, the newest if there are several
    /// (`latestRootProviderFailure` in T3).
    fn root_failure_class(&self, run: &OrchestrationV2Run) -> Option<&str> {
        self.items
            .iter()
            .filter(|item| {
                item.status == ItemStatus::Failed && item.run_id.as_deref() == Some(&run.id)
            })
            .filter_map(|item| match &item.kind {
                ItemKind::Error { class, node_id, .. }
                    if node_id.as_deref() == run.root_node_id.as_deref() =>
                {
                    Some((item.ordinal, class.as_str()))
                }
                _ => None,
            })
            .max_by_key(|(ordinal, _)| *ordinal)
            .map(|(_, class)| class)
    }

    /// What Resume would do, if anything: continue the latest run when it was interrupted or
    /// stopped at a usage limit, and restart a held queue (`ChatView.tsx` in T3).
    pub fn resumable(&self) -> Option<Resumable> {
        let held_queue = self.runs.iter().any(|run| {
            run.status == OrchestrationV2RunStatus::Queued && run.queue_held == Some(true)
        });
        let continue_run_id = self.latest_executed_run().and_then(|run| match run.status {
            OrchestrationV2RunStatus::Interrupted => Some(run.id.clone()),
            OrchestrationV2RunStatus::Failed
                if self.root_failure_class(run) == Some("usage_limit") =>
            {
                Some(run.id.clone())
            }
            _ => None,
        });
        (held_queue || continue_run_id.is_some()).then_some(Resumable {
            continue_run_id,
            held_queue,
        })
    }

    /// Where each user message can be rewound to with "Edit from here", by item id, as T3's
    /// timeline works it out (`deriveRevertTurnCountByUserMessageId` in `session-logic.ts`).
    /// A message rewinds to the checkpoint T3 took after the run before its own, or to the
    /// thread's start for its first run. Messages of a run that hasn't settled, inherited ones,
    /// and ones whose rewind the agent's conversation can't follow have none.
    pub fn revert_targets(&self) -> HashMap<String, RevertTarget> {
        let available = self.available_revert_turn_counts();
        let mut ready_turn_counts = HashMap::default();
        for checkpoint in &self.checkpoints {
            if checkpoint.status == OrchestrationV2CheckpointStatus::Ready
                && let (Some(ordinal), Some(run_id)) =
                    (checkpoint.app_run_ordinal, checkpoint.run_id.as_deref())
            {
                ready_turn_counts.insert(run_id, ordinal);
            }
        }
        // A window of a longer history can't tell the first run it holds is the thread's.
        let start_ready = !self.has_more_history && self.revert_checkpoint(0).is_some();
        let unsettled = self.unsettled_run_id();
        let mut target_by_run: HashMap<&str, Option<i64>> = HashMap::default();
        let mut previous_run: Option<&str> = None;
        let mut targets = HashMap::default();
        for item in &self.items {
            // Inherited history belongs to the source thread, not this one's checkpoints.
            if item.inherited || !self.is_visible(item) {
                continue;
            }
            if !matches!(
                item.kind,
                ItemKind::UserMessage { .. }
                    | ItemKind::AssistantMessage { .. }
                    | ItemKind::Notice { .. }
            ) {
                continue;
            }
            let Some(run_id) = item.run_id.as_deref() else {
                continue;
            };
            if !target_by_run.contains_key(run_id) {
                let target = match previous_run {
                    None => start_ready.then_some(0),
                    Some(previous) => ready_turn_counts.get(previous).copied(),
                };
                target_by_run.insert(run_id, target);
                previous_run = Some(run_id);
            }
            if !matches!(
                item.kind,
                ItemKind::UserMessage {
                    starts_turn: true,
                    ..
                }
            ) || unsettled == Some(run_id)
            {
                continue;
            }
            let Some(turn_count) = target_by_run.get(run_id).copied().flatten() else {
                continue;
            };
            if !available.contains(&turn_count) {
                continue;
            }
            if let Some(checkpoint) = self.revert_checkpoint(turn_count) {
                targets.insert(
                    item.id.clone(),
                    RevertTarget {
                        turn_count,
                        checkpoint_id: checkpoint.id.clone(),
                        scope_id: checkpoint.scope_id.clone(),
                    },
                );
            }
        }
        targets
    }

    /// The ready checkpoint to roll back to so that `turn_count` runs stay
    /// (`findRevertCheckpoint` in T3).
    fn revert_checkpoint(&self, turn_count: i64) -> Option<&OrchestrationV2Checkpoint> {
        self.checkpoints.iter().rev().find(|checkpoint| {
            checkpoint.status == OrchestrationV2CheckpointStatus::Ready
                && if turn_count == 0 {
                    checkpoint.ordinal_within_scope == 0 && checkpoint.app_run_ordinal.is_none()
                } else {
                    checkpoint.app_run_ordinal == Some(turn_count)
                }
        })
    }

    /// The run counts the agent's conversation can be rewound to: its session's thread must be
    /// the thread's current agent, and a run counts only when its provider turn ran in that
    /// thread, since a file checkpoint alone can't restore the conversation
    /// (`deriveAvailableRevertTurnCounts` in T3's `ChatView.logic.ts`).
    fn available_revert_turn_counts(&self) -> HashSet<i64> {
        let mut available = HashSet::default();
        let provider_thread = self
            .thread
            .active_provider_thread_id
            .as_deref()
            .and_then(|id| self.provider_threads.iter().find(|thread| thread.id == id));
        let Some(provider_thread) = provider_thread else {
            return available;
        };
        if provider_thread.provider_session_id.is_none()
            || Some(provider_thread.provider_instance_id.as_str())
                != crate::thread_settings::selection_instance_id(&self.thread.model_selection)
        {
            return available;
        }
        available.insert(0);
        for run in &self.runs {
            let Some(attempt_id) = run.active_attempt_id.as_deref() else {
                continue;
            };
            let provider_turn = self
                .provider_turns
                .iter()
                .filter(|turn| turn.run_attempt_id.as_deref() == Some(attempt_id))
                .max_by_key(|turn| turn.ordinal)
                .or_else(|| {
                    let attempt = self
                        .attempts
                        .iter()
                        .find(|attempt| attempt.id == attempt_id)?;
                    let turn_id = attempt.provider_turn_id.as_deref()?;
                    self.provider_turns.iter().find(|turn| turn.id == turn_id)
                });
            if provider_turn.is_some_and(|turn| turn.provider_thread_id == provider_thread.id) {
                available.insert(run.ordinal);
            }
        }
        available
    }

    /// The run that hasn't settled yet, whose messages can't be rewound to: the active run, or
    /// the latest one while it hasn't finished (`deriveUnsettledRunId` in T3).
    fn unsettled_run_id(&self) -> Option<&str> {
        if let Some(run) = self.active_run() {
            return Some(&run.id);
        }
        let latest = self
            .runs
            .iter()
            .filter(|run| run.status != OrchestrationV2RunStatus::Queued)
            .max_by_key(|run| run.ordinal)?;
        let settled = latest.completed_at.is_some()
            && !matches!(
                latest.status,
                OrchestrationV2RunStatus::Running
                    | OrchestrationV2RunStatus::Starting
                    | OrchestrationV2RunStatus::Waiting
            );
        (!settled).then_some(latest.id.as_str())
    }

    /// The run whose rolling back shows a rewind to `turn_count` happened: the latest settled
    /// run after it, which T3 rolls back with the rest of the thread's later runs
    /// (`waitForRevertedMessage` in T3).
    pub fn rewind_witness(&self, turn_count: i64) -> Option<&OrchestrationV2Run> {
        self.runs
            .iter()
            .filter(|run| {
                run.ordinal > turn_count
                    && matches!(
                        run.status,
                        OrchestrationV2RunStatus::Completed
                            | OrchestrationV2RunStatus::Interrupted
                            | OrchestrationV2RunStatus::Failed
                            | OrchestrationV2RunStatus::Cancelled
                    )
            })
            .max_by_key(|run| run.ordinal)
    }

    /// Every run of the thread, as T3 last sent them (for merge back, `mustr/src/thread_handoff.rs`).
    pub fn runs(&self) -> &[OrchestrationV2Run] {
        &self.runs
    }

    /// A run's current state, by id.
    pub fn run_by_id(&self, run_id: &str) -> Option<&OrchestrationV2Run> {
        self.run(run_id)
    }

    /// The agent session the thread runs in: its active provider thread's, or else the latest
    /// one that's still alive (`resolveThreadProviderSession` in T3's `threadWorkflows.ts`).
    pub fn provider_session(&self) -> Option<&OrchestrationV2ProviderSession> {
        let provider_thread_id = self
            .active_run()
            .and_then(|run| run.provider_thread_id.as_deref())
            .or(self.thread.active_provider_thread_id.as_deref());
        let active = provider_thread_id
            .and_then(|id| self.provider_threads.iter().find(|thread| thread.id == id));
        let attached = active.or_else(|| {
            self.provider_threads.iter().find(|thread| {
                thread.app_thread_id.as_deref() == Some(self.thread.id.as_str())
                    && thread.provider_session_id.is_some()
            })
        });
        if let Some(session_id) = attached.and_then(|thread| thread.provider_session_id.as_deref())
        {
            return self
                .provider_sessions
                .iter()
                .find(|session| session.id == session_id);
        }
        self.provider_sessions.iter().rev().find(|session| {
            !matches!(
                session.status,
                OrchestrationV2ProviderSessionStatus::Stopped
                    | OrchestrationV2ProviderSessionStatus::Error
            )
        })
    }

    /// Every agent session the thread holds, which restarting the agent detaches.
    pub fn provider_sessions(&self) -> &[OrchestrationV2ProviderSession] {
        &self.provider_sessions
    }

    /// Whether "Fork from this response" can fork from `item`: a finished agent message of a
    /// run, when the agent can fork natively or hand its whole thread to a new one
    /// (`canForkProjectedAssistantItem` in T3). Without a session to ask, as for inherited
    /// history, T3's server-side fallback still forks.
    pub fn can_fork(&self, item: &TimelineItem) -> bool {
        if !matches!(
            item.kind,
            ItemKind::AssistantMessage {
                streaming: false,
                ..
            }
        ) || item.run_id.is_none()
            || item.status != ItemStatus::Completed
        {
            return false;
        }
        if item.inherited {
            return true;
        }
        self.provider_session()
            .is_none_or(|session| can_fork_with(&session.capabilities))
    }

    /// Whether there's a conversation to compact: a message other than `/compact` itself.
    pub fn has_compactable_conversation(&self) -> bool {
        self.items.iter().any(|item| match &item.kind {
            ItemKind::UserMessage { text, images, .. } => {
                !text.trim().eq_ignore_ascii_case("/compact") || !images.is_empty()
            }
            _ => false,
        })
    }

    /// The plan to offer Implement and Refine for, as T3's composer does
    /// (`shouldShowPlanFollowUpPrompt` in T3's `ChatView.logic.ts`): the thread is in plan mode,
    /// its latest run has settled, no question is waiting, and that run's latest plan is still
    /// active.
    pub fn actionable_plan(&self) -> Option<ActionablePlan> {
        if self.thread.interaction_mode != ProviderInteractionMode::Plan || self.is_running() {
            return None;
        }
        if self
            .pending_requests()
            .iter()
            .any(|request| request.is_question)
        {
            return None;
        }
        let latest_run_id = self
            .runs
            .iter()
            .filter(|run| run.status != OrchestrationV2RunStatus::Queued)
            .max_by_key(|run| run.ordinal)
            .map(|run| run.id.clone());
        // The plans' turn items, in timeline order.
        let plan_items = self
            .items
            .iter()
            .filter_map(|item| match &item.kind {
                ItemKind::Plan {
                    plan_id,
                    title,
                    markdown,
                } => Some((item, plan_id, title, markdown)),
                _ => None,
            })
            .collect::<Vec<_>>();
        let in_latest_run = self
            .plans
            .iter()
            .filter(|plan| latest_run_id.is_some() && plan.run_id == latest_run_id)
            .collect::<Vec<_>>();
        let candidates = if in_latest_run.is_empty() {
            self.plans.iter().collect()
        } else {
            in_latest_run
        };
        let plan = candidates.into_iter().max_by_key(|plan| {
            plan_items
                .iter()
                .find(|(_, plan_id, _, _)| **plan_id == plan.id)
                .map(|(item, ..)| item.ordinal)
        })?;
        if plan.status != OrchestrationV2PlanArtifactProposedPlanStatus::Active {
            return None;
        }
        let (item, _, title, markdown) = plan_items
            .into_iter()
            .find(|(_, plan_id, _, _)| **plan_id == plan.id)?;
        Some(ActionablePlan {
            plan_id: plan.id.clone(),
            item_id: item.id.clone(),
            title: title.clone(),
            markdown: markdown.clone(),
        })
    }
}

impl TimelineItem {
    /// Tool calls, file edits, and reasoning: the agent's work between its messages.
    pub fn is_work(&self) -> bool {
        matches!(
            self.kind,
            ItemKind::Tool { .. } | ItemKind::FileChange { .. } | ItemKind::Reasoning { .. }
        )
    }

    pub fn request_id(&self) -> Option<&str> {
        match &self.kind {
            ItemKind::Approval { request_id, .. } | ItemKind::Question { request_id, .. } => {
                Some(request_id)
            }
            _ => None,
        }
    }
}

/// Whether an agent with `capabilities` can fork a thread from one of its turns: natively, or by
/// handing the whole thread over.
fn can_fork_with(capabilities: &OrchestrationV2ProviderCapabilities) -> bool {
    let natively = capabilities.threads.can_fork_thread
        && capabilities.threads.can_fork_from_turn
        && capabilities.identity.native_thread_ids == OrchestrationV2NativeRefStrength::Strong;
    natively || capabilities.context.supports_full_thread_handoff
}

fn proposed_plan(artifact: &OrchestrationV2PlanArtifact) -> Option<ProposedPlan> {
    match artifact {
        OrchestrationV2PlanArtifact::ProposedPlan(plan) => Some(ProposedPlan {
            id: plan.id.clone(),
            run_id: plan.run_id.clone(),
            status: plan.status.clone(),
        }),
        _ => None,
    }
}

fn upsert<T>(items: &mut Vec<T>, item: T, id: impl Fn(&T) -> &String) {
    match items.iter_mut().find(|existing| id(existing) == id(&item)) {
        Some(existing) => *existing = item,
        None => items.push(item),
    }
}

/// The projection's timeline: its visible items (which include what a fork inherited), or its
/// own turn items when T3 sent no visible ones.
fn timeline_items(projection: &OrchestrationV2ThreadProjection) -> Vec<TimelineItem> {
    if projection.visible_turn_items.is_empty() {
        let mut items = projection
            .turn_items
            .iter()
            .filter_map(canonical_item)
            .filter_map(|item| TimelineItem::from_turn_item(&item))
            .collect::<Vec<_>>();
        items.sort_by(|left, right| (left.ordinal, &left.id).cmp(&(right.ordinal, &right.id)));
        return items;
    }
    let mut visible = projection.visible_turn_items.iter().collect::<Vec<_>>();
    visible.sort_by_key(|visible| visible.position);
    visible
        .into_iter()
        .filter_map(|visible| {
            let mut item = TimelineItem::from_turn_item(&canonical_item(&visible.item)?)?;
            item.inherited = visible.visibility
                == OrchestrationV2ThreadProjectionVisibleTurnItemsItemVisibility::Inherited;
            if item.inherited {
                item.source_thread_id = Some(visible.source_thread_id.clone());
            }
            Some(item)
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn tool_output_images_are_counted_as_t3_counts_them() {
        let png = json!({ "type": "image", "mimeType": "image/png", "data": "AAAA" });
        let claude = json!({
            "type": "image",
            "source": { "type": "base64", "media_type": "image/jpeg", "data": "AAAA" }
        });
        let linked = json!({ "type": "image", "source": { "type": "url", "url": "https://x" } });
        assert_eq!(tool_output_images(&json!([png, claude, linked])), 2);
        assert_eq!(tool_output_images(&json!({ "content": [png] })), 1);
        assert_eq!(tool_output_images(&png), 1);
        assert_eq!(tool_output_images(&json!("text output")), 0);
        assert_eq!(
            tool_output_images(&json!(vec![png.clone(); 12])),
            MAX_TOOL_OUTPUT_IMAGES
        );
        assert_eq!(
            without_image_data(&json!({ "content": [png, claude] })),
            json!({ "content": [
                { "type": "image", "mimeType": "image/png" },
                { "type": "image", "source": { "type": "base64", "media_type": "image/jpeg" } }
            ] })
        );
    }
}

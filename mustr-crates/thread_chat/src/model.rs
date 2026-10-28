//! One thread as mustr shows it: T3's thread projection, kept current by the events of
//! `orchestration.subscribeThread`, reduced to what the timeline and composer need.
//!
//! T3 sends the same turn item in three generated shapes (in a projection, in its visible
//! items, and in an event). They're identical on the wire, so each is decoded into
//! [`OrchestrationV2TurnItem`] once and turned into a [`TimelineItem`].

use serde::Serialize;
use t3_api::{
    tagged::{
        OrchestrationV2RuntimeRequestResponseCapability, OrchestrationV2ThreadStreamItem,
        OrchestrationV2TurnItem,
    },
    types::{
        OrchestrationV2AppThread, OrchestrationV2ConversationMessage, OrchestrationV2DomainEvent,
        OrchestrationV2ProviderThread, OrchestrationV2ProviderTurn, OrchestrationV2Run,
        OrchestrationV2RunStatus, OrchestrationV2RuntimeRequest, OrchestrationV2RuntimeRequestKind,
        OrchestrationV2RuntimeRequestKindString, OrchestrationV2RuntimeRequestStatus,
        OrchestrationV2ThreadProjection,
        OrchestrationV2ThreadProjectionVisibleTurnItemsItemVisibility,
        OrchestrationV2TurnItemStatus, OrchestrationV2UserMessageInputIntent,
        ProviderApprovalDecision,
    },
};

/// One entry of the timeline.
#[derive(Clone, Debug, PartialEq)]
pub struct TimelineItem {
    pub id: String,
    pub ordinal: i64,
    pub run_id: Option<String>,
    pub status: ItemStatus,
    /// Carried over from the thread this one was forked from.
    pub inherited: bool,
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
    Subagent,
}

#[derive(Clone, Debug, PartialEq)]
pub enum ItemKind {
    UserMessage {
        text: String,
        /// Sent while a run was active, to run after it.
        queued: bool,
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
        /// What's being approved: `command_execution`, `file_change`, and so on.
        request_kind: String,
        options: Vec<ApprovalOption>,
    },
    /// The agent asked the user questions.
    Question {
        request_id: String,
        title: Option<String>,
        questions: Vec<Question>,
    },
    Error {
        message: String,
    },
    /// A one-line marker: a notice, a checkpoint, a compaction, an interrupt.
    Notice {
        text: String,
    },
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Question {
    pub id: String,
    pub header: String,
    pub question: String,
    /// `(label, answer)`: the answer sent back is the option's value, or its label.
    pub options: Vec<(String, String)>,
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
            ($item:expr, $kind:expr) => {
                Self {
                    id: $item.id.clone(),
                    ordinal: $item.ordinal,
                    run_id: $item.run_id.clone(),
                    status: ItemStatus::from_wire(&$item.status),
                    inherited: false,
                    kind: $kind,
                }
            };
        }
        let notice = |text: String| ItemKind::Notice { text };
        Some(match item {
            OrchestrationV2TurnItem::UserMessage(item) => item!(
                item,
                ItemKind::UserMessage {
                    text: item.text.clone(),
                    queued: item.input_intent == OrchestrationV2UserMessageInputIntent::QueuedTurn,
                }
            ),
            OrchestrationV2TurnItem::AssistantMessage(item) => item!(
                item,
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
                }
            ),
            OrchestrationV2TurnItem::DynamicTool(item) => item!(
                item,
                ItemKind::Tool {
                    tool: ToolKind::Tool,
                    summary: non_empty(item.title.as_ref())
                        .or_else(|| non_empty(item.tool_name.as_ref()))
                        .unwrap_or_else(|| "Tool call".to_string()),
                    input: json_text(&item.input),
                    output: item.output.as_ref().and_then(json_text),
                    output_omitted: item.output_omitted.unwrap_or(false),
                }
            ),
            OrchestrationV2TurnItem::Subagent(item) => item!(
                item,
                ItemKind::Tool {
                    tool: ToolKind::Subagent,
                    summary: non_empty(item.title.as_ref())
                        .unwrap_or_else(|| format!("Subagent ({})", item.driver)),
                    input: non_empty(Some(&item.prompt)),
                    output: non_empty(item.result.as_ref())
                        .or_else(|| non_empty(item.progress.as_ref())),
                    output_omitted: false,
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
                        })
                        .collect(),
                }
            ),
            OrchestrationV2TurnItem::Error(item) => item!(
                item,
                ItemKind::Error {
                    message: item.failure.message.clone(),
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
                notice(
                    non_empty(item.title.as_ref())
                        .unwrap_or_else(|| "The agent asked for a secret".into())
                )
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
        }
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
                log::debug!("skipping T3 thread event {:?}", event.event.type_);
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

    /// Replaces an item's omitted command output with the one T3 sent for it.
    pub fn set_output(&mut self, item_id: &str, output: Option<String>) -> Applied {
        let revision = self.next_revision;
        let Some(index) = self.items.iter().position(|item| item.id == item_id) else {
            return Applied::default();
        };
        let ItemKind::Tool {
            output: existing,
            output_omitted,
            ..
        } = &mut self.items[index].kind
        else {
            return Applied::default();
        };
        *existing = output;
        *output_omitted = false;
        self.next_revision += 1;
        self.revisions[index] = revision;
        Applied { changed: true }
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
            Some(item)
        })
        .collect()
}

//! The threads sidebar: T3's threads in the sections of T3's own sidebar
//! (`components/Sidebar.tsx`): Pinned, Active, Snoozed and Settled, each row with T3's status
//! pill (`resolveThreadStatusPill` in `components/Sidebar.logic.ts`). It reads the app's shell
//! ([`ThreadShells`]), so it stays current with T3.
//!
//! Snoozed and Settled start collapsed, and Settled lists its newest threads a page at a time
//! (T3 0.1.1 sends every settled thread in the shell; 0.1.3's `getOlderSettledThreads` isn't
//! there to page from). Subagents have no rows of their own, as in T3: their parents' family
//! bars list them. A row's context menu settles, archives, pins, snoozes and renames it
//! (`organize.rs`, `snooze.rs`), and the header adds projects, starts threads, filters by
//! project, lists archived threads, and searches.
//!
//! Rows drag to reorder Pinned and Active, or to pin, unpin, settle, un-settle or wake a thread
//! (`Sidebar.drag.ts` and `planSidebarThreadDrop` in T3's sidebar), with the moved order shown
//! until T3's shell catches up. The search finds threads by title, branch, project or pull
//! request at once, then threads whose messages match, from `orchestration.searchThreads`.
//! [`PreviousThread`] and [`NextThread`] move through the threads in the order they show.

use crate::{
    backend::{Access, ThreadBackendExt as _},
    commands::new_id,
    organize::{self, DropPlan, DropSource, DropTarget, SettleThread, ShowArchivedThreads},
    projects::AddProject,
    shell::{Shell, ThreadShells},
    thread_view::{NewThread, ThreadView},
};
use collections::{HashMap, HashSet};
use editor::{Editor, EditorEvent};
use gpui::{
    AnyElement, App, AppContext as _, Context, DismissEvent, Entity, EventEmitter, FocusHandle,
    Focusable, FontWeight, Global, Hsla, IntoElement, ListAlignment, ListState, MouseButton,
    MouseDownEvent, ParentElement as _, Pixels, Point, Render, SharedString, Styled as _,
    Subscription, Task, WeakEntity, Window, actions, anchored, deferred, div, list, px,
};
use mustr_ui::{Chip, Pill, palette, tokens, tokens::scaled};
use std::{cell::RefCell, cmp::Reverse, time::Duration};
use t3_api::types::{
    OrchestrationV2AppThreadLineageRelationshipToParent, OrchestrationV2SearchThreadsInput,
    OrchestrationV2ThreadShell, OrchestrationV2ThreadShellSettledOverride, ProviderInteractionMode,
    PullRequestState, ThreadPullRequestLinkSource,
};
use time::{OffsetDateTime, format_description::well_known::Rfc3339};
use ui::{
    Color, ContextMenu, HighlightedLabel, Icon, IconButton, IconName, IconPosition, IconSize,
    Indicator, Label, LabelCommon as _, LabelSize, PopoverMenu, Tooltip, prelude::*,
};
use workspace::Workspace;

actions!(
    thread_chat,
    [
        /// Shows the threads sidebar and focuses its search.
        SearchThreads,
        /// Opens the thread above the open one in the threads sidebar.
        PreviousThread,
        /// Opens the thread below the open one in the threads sidebar.
        NextThread,
    ]
);

/// How long the search waits for more typing before it asks T3 for message matches
/// (`THREAD_SEARCH_DEBOUNCE_MS`).
const MESSAGE_SEARCH_DEBOUNCE: Duration = Duration::from_millis(120);
/// T3 takes queries this long at most (`THREAD_SEARCH_QUERY_MAX_LENGTH`).
const MESSAGE_SEARCH_MAX_CHARS: usize = 200;
/// How many threads whose messages match the search lists.
const MESSAGE_MATCH_LIMIT: usize = 20;
/// T3 matches messages by words of three letters or more (`MESSAGE_TOKEN_MIN_LENGTH`).
const MESSAGE_WORD_MIN_CHARS: usize = 3;
/// A match's quality when a word only starts one of the message's (`MESSAGE_PREFIX`): matches
/// at least this good rank first.
const MESSAGE_PREFIX_QUALITY: f64 = 0.8;
/// How long a drop's order shows if T3's shell doesn't confirm it.
const PENDING_DROP_TIMEOUT: Duration = Duration::from_secs(2);

/// How many settled threads show when Settled opens, and how many more each "Show more" adds
/// (`SETTLED_TAIL_INITIAL_COUNT` and `SETTLED_TAIL_PAGE_COUNT` in T3's sidebar).
const SETTLED_FIRST_PAGE: usize = 10;
const SETTLED_PAGE: usize = 25;
/// How often the list is rebuilt when nothing changed, so snoozed threads wake on time.
const CLOCK_TICK: Duration = Duration::from_secs(30);
const ROW_HEIGHT: Pixels = px(44.);

/// A section of the sidebar.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Section {
    Pinned,
    Active,
    Snoozed,
    Settled,
}

impl Section {
    pub const ALL: [Section; 4] = [
        Section::Pinned,
        Section::Active,
        Section::Snoozed,
        Section::Settled,
    ];

    pub fn label(self) -> &'static str {
        match self {
            Self::Pinned => "Pinned",
            Self::Active => "Active",
            Self::Snoozed => "Snoozed",
            Self::Settled => "Settled",
        }
    }

    /// Snoozed and Settled fold away, and start folded, as in T3.
    fn folds(self) -> bool {
        matches!(self, Self::Snoozed | Self::Settled)
    }
}

/// A thread's status pill, as T3's sidebar picks it (`resolveThreadStatusPill`). A thread with
/// nothing to say has none.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ShellStatus {
    PendingApproval,
    AwaitingInput,
    Working,
    Connecting,
    /// Blocked on another thread or on background work.
    Waiting,
    /// The last run failed, and the user hasn't seen it.
    Failed,
    PlanReady,
    /// The last run finished, and the user hasn't seen it.
    Completed,
}

impl ShellStatus {
    pub fn from_shell(thread: &OrchestrationV2ThreadShell) -> Option<Self> {
        if let Some(request) = &thread.pending_runtime_request {
            let kind = crate::model::wire_string(&request.kind);
            return Some(if kind == "user_input" {
                Self::AwaitingInput
            } else {
                Self::PendingApproval
            });
        }
        let status = crate::model::wire_string(&thread.status);
        let blocked = thread.blocked_on_thread_wait == Some(true);
        match status.as_str() {
            "running" if !blocked => return Some(Self::Working),
            "waiting" => return Some(Self::Working),
            "preparing" | "starting" | "queued" => return Some(Self::Connecting),
            "running" => return Some(Self::Waiting),
            _ => {}
        }
        if background_work_holds_completion(thread) {
            return Some(Self::Waiting);
        }
        let completed_at = millis(thread.latest_run_completed_at.as_deref());
        let visited_at = millis(thread.last_visited_at.as_deref());
        if status == "failed" {
            let unseen = match (completed_at, visited_at) {
                (Some(completed), Some(visited)) => completed > visited,
                _ => true,
            };
            if unseen {
                return Some(Self::Failed);
            }
        }
        if thread.interaction_mode == ProviderInteractionMode::Plan
            && thread.has_actionable_proposed_plan
        {
            return Some(Self::PlanReady);
        }
        if has_unseen_completion(thread) {
            return Some(Self::Completed);
        }
        None
    }

    pub fn label(self) -> &'static str {
        match self {
            Self::PendingApproval => "Pending approval",
            Self::AwaitingInput => "Awaiting input",
            Self::Working => "Working",
            Self::Connecting => "Connecting",
            Self::Waiting => "Waiting",
            Self::Failed => "Failed",
            Self::PlanReady => "Plan ready",
            Self::Completed => "Completed",
        }
    }

    fn color(self, cx: &App) -> Hsla {
        let palette = palette(cx);
        match self {
            Self::PendingApproval => palette.warning.hsla(),
            Self::AwaitingInput => palette.pending.hsla(),
            Self::Working | Self::Connecting => palette.accent.hsla(),
            Self::Waiting => palette.text_muted.hsla(),
            Self::Failed => cx.theme().status().error,
            Self::PlanReady => palette.agent.hsla(),
            Self::Completed => palette.ok.hsla(),
        }
    }
}

/// Whether the agent is waiting on a subagent or background task to finish
/// (`backgroundWorkHoldsCompletion` in T3's shared package). Commands and monitors count only
/// when the agent waits on them, since they're often dev servers.
fn background_work_holds_completion(thread: &OrchestrationV2ThreadShell) -> bool {
    thread
        .pending_background_tasks
        .iter()
        .flatten()
        .any(|task| {
            let task = serde_json::to_value(task).unwrap_or_default();
            task["awaited"] == serde_json::Value::Bool(true)
                || matches!(task["kind"].as_str(), Some("subagent" | "background_task"))
        })
}

/// Whether the thread's last run finished after the user last looked (`hasUnseenCompletion`).
/// A thread never opened in T3 has no visit, and doesn't count.
fn has_unseen_completion(thread: &OrchestrationV2ThreadShell) -> bool {
    match (
        millis(thread.latest_run_completed_at.as_deref()),
        millis(thread.last_visited_at.as_deref()),
    ) {
        (Some(completed), Some(visited)) => completed > visited,
        _ => false,
    }
}

/// Milliseconds since the epoch of one of T3's ISO 8601 timestamps.
fn millis(timestamp: Option<&str>) -> Option<i128> {
    let timestamp = OffsetDateTime::parse(timestamp?, &Rfc3339).ok()?;
    Some(timestamp.unix_timestamp_nanos() / 1_000_000)
}

/// The time the list is built for: snoozes end by it.
pub(crate) fn now() -> OffsetDateTime {
    OffsetDateTime::now_utc()
}

/// Whether the user (or T3's auto-settle) settled the thread.
pub(crate) fn is_settled(thread: &OrchestrationV2ThreadShell) -> bool {
    thread.settled_override == Some(OrchestrationV2ThreadShellSettledOverride::Settled)
}

fn is_subagent(thread: &OrchestrationV2ThreadShell) -> bool {
    thread.lineage.relationship_to_parent
        == Some(OrchestrationV2AppThreadLineageRelationshipToParent::Subagent)
}

/// Whether a snooze hides the thread at `now` (`effectiveSnoozed`): until it wakes, unless it
/// raised its hand meanwhile with a request or a new failure.
fn is_snoozed(thread: &OrchestrationV2ThreadShell, now: i128) -> bool {
    let Some(wakes_at) = millis(thread.snoozed_until.as_deref()) else {
        return false;
    };
    if wakes_at <= now || thread.pending_runtime_request.is_some() {
        return false;
    }
    let failed = crate::model::wire_string(&thread.status) == "failed";
    let failed_since_snoozed = match (
        millis(thread.latest_run_completed_at.as_deref()),
        millis(thread.snoozed_at.as_deref()),
    ) {
        (Some(failed_at), Some(snoozed_at)) => failed_at > snoozed_at,
        _ => true,
    };
    !(failed && failed_since_snoozed)
}

pub(crate) fn display_title(title: &str) -> String {
    if title.trim().is_empty() {
        "Untitled thread".to_string()
    } else {
        title.to_string()
    }
}

/// One of the sidebar's threads.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ThreadEntry {
    pub id: String,
    pub title: String,
    pub project_id: String,
    /// The project's title.
    pub project: Option<String>,
    /// The project's checkout.
    pub workspace_root: Option<String>,
    pub branch: Option<String>,
    pub worktree_path: Option<String>,
    pub section: Section,
    pub status: Option<ShellStatus>,
    /// The last run finished, or failed, since the user last looked.
    pub unread: bool,
    /// Its agent is active, so T3 won't archive it.
    pub running: bool,
    /// A run has finished, so it can be marked unread.
    pub has_finished_run: bool,
    pub snoozed_until: Option<String>,
    /// Its pull requests' numbers.
    pub pull_requests: Vec<i64>,
    /// The pull request its row shows, if it has one.
    pub pull_request: Option<RowPullRequest>,
    /// Pinned, even when it shows elsewhere (snoozed or settled).
    pub pinned: bool,
    /// Settled, even when it shows elsewhere (snoozed).
    pub settled: bool,
    /// T3's auto-settle rules may settle it (the user didn't turn them off for it).
    pub auto_settle: bool,
    /// T3 would take a snooze now.
    pub can_snooze: bool,
}

/// The pull request a row shows: one that's open if there is one, else the latest, as T3's
/// `useLinkedThreadPullRequest` picks it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RowPullRequest {
    pub number: i64,
    pub url: String,
    /// `None` until T3 has read it from the host.
    pub state: Option<PullRequestState>,
    pub is_draft: bool,
    pub title: Option<String>,
}

impl RowPullRequest {
    pub fn of(thread: &OrchestrationV2ThreadShell) -> Option<Self> {
        let links = thread
            .pull_requests
            .iter()
            .flatten()
            .filter(|link| link.source != ThreadPullRequestLinkSource::StackDismissed)
            .collect::<Vec<_>>();
        let updated = |link: &&&t3_api::types::ThreadPullRequestLink| {
            link.snapshot
                .as_ref()
                .and_then(|snapshot| snapshot.updated_at.clone())
                .unwrap_or_else(|| link.linked_at.clone())
        };
        let open = links
            .iter()
            .filter(|link| {
                link.snapshot
                    .as_ref()
                    .is_some_and(|snapshot| snapshot.state == PullRequestState::Open)
            })
            .max_by_key(updated);
        if let Some(link) = open.or_else(|| links.iter().max_by_key(updated)) {
            let snapshot = link.snapshot.as_ref();
            return Some(Self {
                number: link.number,
                url: link.url.clone(),
                state: snapshot.map(|snapshot| snapshot.state.clone()),
                is_draft: snapshot.is_some_and(|snapshot| snapshot.is_draft),
                title: snapshot.map(|snapshot| snapshot.title.clone()),
            });
        }
        let link = thread
            .linked_pull_request
            .as_ref()
            .or(thread.branch_pull_request.as_ref())?;
        Some(Self {
            number: link.number,
            url: link.url.clone(),
            state: None,
            is_draft: false,
            title: None,
        })
    }

    fn state_label(&self) -> &'static str {
        match &self.state {
            None => "Linked",
            Some(PullRequestState::Open) if self.is_draft => "Draft",
            Some(PullRequestState::Closed) => "Closed",
            Some(PullRequestState::Merged) => "Merged",
            Some(_) => "Open",
        }
    }

    /// The colors of the thread toolbar's chip (`mustr/src/thread_toolbar.rs`).
    fn color(&self, cx: &App) -> Hsla {
        let palette = palette(cx);
        match &self.state {
            None => palette.text_muted.hsla(),
            Some(PullRequestState::Open) if self.is_draft => palette.text_muted.hsla(),
            Some(PullRequestState::Closed) => cx.theme().status().error,
            Some(PullRequestState::Merged) => palette.pending.hsla(),
            Some(_) => palette.ok.hsla(),
        }
    }
}

impl ThreadEntry {
    fn new(thread: &OrchestrationV2ThreadShell, section: Section, shell: &Shell) -> Self {
        let project = shell.projects.get(&thread.project_id);
        let status = ShellStatus::from_shell(thread);
        let mut pull_requests = thread
            .pull_requests
            .iter()
            .flatten()
            .map(|link| link.number)
            .chain(thread.linked_pull_request.iter().map(|link| link.number))
            .chain(thread.branch_pull_request.iter().map(|link| link.number))
            .collect::<Vec<_>>();
        pull_requests.dedup();
        Self {
            id: thread.id.clone(),
            title: display_title(&thread.title),
            project_id: thread.project_id.clone(),
            project: project.map(|project| project.title.clone()),
            workspace_root: project.map(|project| project.workspace_root.clone()),
            branch: thread.branch.clone(),
            worktree_path: thread.worktree_path.clone(),
            section,
            status,
            unread: matches!(status, Some(ShellStatus::Completed | ShellStatus::Failed)),
            running: organize::is_running(thread),
            has_finished_run: thread.latest_run_completed_at.is_some(),
            snoozed_until: thread.snoozed_until.clone(),
            pull_requests,
            pull_request: RowPullRequest::of(thread),
            pinned: thread.pinned_at.is_some(),
            settled: is_settled(thread),
            auto_settle: thread.auto_settle_disabled_at.is_none(),
            can_snooze: crate::snooze::can_snooze(thread),
        }
    }

    /// Whether the search `query` (lowercase words) finds it: by title, branch, project, or
    /// pull request number, as T3's search starts (`searchSidebarThreads`).
    fn matches(&self, words: &[String]) -> bool {
        let haystack = [
            Some(self.title.as_str()),
            self.branch.as_deref(),
            self.project.as_deref(),
        ]
        .into_iter()
        .flatten()
        .map(str::to_lowercase)
        .chain(self.pull_requests.iter().map(|number| format!("#{number}")))
        .collect::<Vec<_>>()
        .join(" ");
        words.iter().all(|word| haystack.contains(word.as_str()))
    }
}

/// What the sidebars show: every project, or one. It's the app's, so the palette's
/// settle-and-advance moves through what the user sees.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct ThreadListFilter {
    pub project_id: Option<String>,
}

impl Global for ThreadListFilter {}

impl ThreadListFilter {
    pub fn get(cx: &App) -> Self {
        cx.try_global::<Self>().cloned().unwrap_or_default()
    }

    pub fn set_project(project_id: Option<String>, cx: &mut App) {
        cx.set_global(Self { project_id });
    }
}

/// The shell's threads in the sidebar's sections and order.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct ThreadList {
    pub pinned: Vec<ThreadEntry>,
    pub active: Vec<ThreadEntry>,
    pub snoozed: Vec<ThreadEntry>,
    pub settled: Vec<ThreadEntry>,
}

impl ThreadList {
    /// Sorts the shell's threads into sections as T3's `partitionSidebarThreads` does: snoozed
    /// first, then settled, then pinned; the rest are active. Archived, deleted and subagent
    /// threads have no row.
    pub fn from_shell(shell: &Shell, filter: &ThreadListFilter, now: OffsetDateTime) -> Self {
        let now = now.unix_timestamp_nanos() / 1_000_000;
        let mut sections: HashMap<Section, Vec<&OrchestrationV2ThreadShell>> = HashMap::default();
        for thread in shell.threads.values() {
            if thread.archived_at.is_some() || thread.deleted_at.is_some() || is_subagent(thread) {
                continue;
            }
            if filter
                .project_id
                .as_ref()
                .is_some_and(|project_id| *project_id != thread.project_id)
            {
                continue;
            }
            let section = if is_snoozed(thread, now) {
                Section::Snoozed
            } else if is_settled(thread) {
                Section::Settled
            } else if thread.pinned_at.is_some() {
                Section::Pinned
            } else {
                Section::Active
            };
            sections.entry(section).or_default().push(thread);
        }
        let mut take = |section: Section, sort: fn(&mut Vec<&OrchestrationV2ThreadShell>)| {
            let mut threads = sections.remove(&section).unwrap_or_default();
            sort(&mut threads);
            threads
                .into_iter()
                .map(|thread| ThreadEntry::new(thread, section, shell))
                .collect::<Vec<_>>()
        };
        Self {
            pinned: take(Section::Pinned, sort_pinned),
            active: take(Section::Active, sort_active),
            snoozed: take(Section::Snoozed, sort_snoozed),
            settled: take(Section::Settled, sort_settled),
        }
    }

    pub fn section(&self, section: Section) -> &[ThreadEntry] {
        match section {
            Section::Pinned => &self.pinned,
            Section::Active => &self.active,
            Section::Snoozed => &self.snoozed,
            Section::Settled => &self.settled,
        }
    }

    pub fn len(&self) -> usize {
        Section::ALL
            .iter()
            .map(|section| self.section(*section).len())
            .sum()
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    /// Every thread, in the sidebar's order.
    pub fn iter(&self) -> impl Iterator<Item = &ThreadEntry> {
        Section::ALL
            .into_iter()
            .flat_map(|section| self.section(section).iter())
    }

    pub fn find(&self, thread_id: &str) -> Option<&ThreadEntry> {
        self.iter().find(|thread| thread.id == thread_id)
    }

    /// The thread to move to after settling `thread_id`: the next one after it that isn't
    /// settled or snoozed, wrapping around (`planForwardNavigation` in T3's sidebar).
    pub fn next_after(&self, thread_id: &str) -> Option<&ThreadEntry> {
        let order = self.iter().collect::<Vec<_>>();
        let index = order.iter().position(|thread| thread.id == thread_id)?;
        order[index + 1..]
            .iter()
            .chain(&order[..index])
            .find(|thread| matches!(thread.section, Section::Pinned | Section::Active))
            .copied()
    }

    /// The threads the search `query` finds, in the sidebar's order.
    pub fn search(&self, query: &str) -> Vec<&ThreadEntry> {
        self.search_positions(query)
            .into_iter()
            .map(|(section, index)| &self.section(section)[index])
            .collect()
    }

    /// Where the threads the search `query` finds are: their sections and places in them.
    fn search_positions(&self, query: &str) -> Vec<(Section, usize)> {
        let words = query
            .split_whitespace()
            .map(str::to_lowercase)
            .collect::<Vec<_>>();
        Section::ALL
            .into_iter()
            .flat_map(|section| {
                self.section(section)
                    .iter()
                    .enumerate()
                    .filter(|(_, thread)| thread.matches(&words))
                    .map(move |(index, _)| (section, index))
            })
            .collect()
    }
}

/// Pinned threads by their order key, then the ones without one, newest first
/// (`sortPinnedThreadsByOrderKey`).
fn sort_pinned(threads: &mut Vec<&OrchestrationV2ThreadShell>) {
    threads.sort_by_cached_key(|thread| {
        (
            thread.pin_order_key.is_none(),
            thread.pin_order_key.clone(),
            Reverse(millis(Some(&thread.created_at)).unwrap_or(0)),
            thread.id.clone(),
        )
    });
}

/// Active threads without an order key first, newest (by creation or un-settling) first, then
/// the ones the user ordered, by key (`sortActiveThreadsByOrderKey`).
fn sort_active(threads: &mut Vec<&OrchestrationV2ThreadShell>) {
    threads.sort_by_cached_key(|thread| {
        let anchor = millis(Some(&thread.created_at))
            .max(millis(thread.unsettled_at.as_deref()))
            .unwrap_or(0);
        (
            thread.active_order_key.is_some(),
            thread.active_order_key.clone(),
            Reverse(anchor),
            thread.id.clone(),
        )
    });
}

/// Snoozed threads by when they wake, soonest first.
fn sort_snoozed(threads: &mut Vec<&OrchestrationV2ThreadShell>) {
    threads.sort_by_cached_key(|thread| {
        (
            millis(thread.snoozed_until.as_deref()).unwrap_or(0),
            thread.id.clone(),
        )
    });
}

/// Settled threads newest first, by when they were settled or else their last activity
/// (`sortSettledThreads` and `resolveSettledThreadTimestamp`).
fn sort_settled(threads: &mut Vec<&OrchestrationV2ThreadShell>) {
    threads.sort_by_cached_key(|thread| {
        let settled_at = millis(thread.settled_at.as_deref())
            .or_else(|| {
                [
                    thread.latest_user_message_at.as_deref(),
                    thread.latest_run_requested_at.as_deref(),
                    thread.latest_run_started_at.as_deref(),
                    thread.latest_run_completed_at.as_deref(),
                ]
                .into_iter()
                .filter_map(millis)
                .max()
            })
            .or_else(|| millis(Some(&thread.updated_at)))
            .unwrap_or(0);
        (Reverse(settled_at), thread.id.clone())
    });
}

/// One row of the sidebar's list.
#[derive(Clone, Debug, PartialEq, Eq)]
enum Item {
    Header(Section),
    Thread(Section, usize),
    /// A section with nothing in it: Active, or Pinned while a thread is dragged.
    Empty(Section),
    MoreSettled,
    /// A search result, by title, branch, project or pull request.
    Found(Section, usize),
    /// Over the threads whose messages match.
    MessagesHeader,
    /// A thread whose messages match the search: an index into the search's matches.
    Message(usize),
    /// Asking T3 for message matches.
    SearchingMessages,
    /// T3 couldn't search messages.
    MessageSearchFailed,
    NoMatches,
}

/// What identifies a row across rebuilds, so the list keeps its scroll position.
#[derive(Clone, Debug, PartialEq, Eq)]
enum ItemKey {
    Header(Section),
    Thread(String),
    Empty(Section),
    MoreSettled,
    MessagesHeader,
    Message(String),
    SearchingMessages,
    MessageSearchFailed,
    NoMatches,
}

/// A thread whose messages match the search, with the best match's snippet.
#[derive(Clone, Debug, PartialEq)]
pub(crate) struct MessageMatch {
    pub thread: ThreadEntry,
    pub snippet: String,
    /// Whether the agent wrote the message (else the user did).
    pub from_agent: bool,
}

/// The search's message matches, for one query.
///
/// One search at a time goes to T3: a server without a message index (T3 0.1.1) scans every
/// message for each, which takes a minute on a big history, and a search mustr stops waiting
/// for still runs there. The latest query goes once the one in flight answers.
#[derive(Default)]
struct MessageSearch {
    /// The query as T3 takes it, the latest typed.
    query: String,
    matches: Vec<MessageMatch>,
    /// What T3 sent, kept so the matches follow the shell's changes.
    raw: Vec<t3_api::types::OrchestrationThreadSearchMatch>,
    pending: bool,
    failed: bool,
    _debounce: Option<Task<()>>,
    /// Whether the user stopped typing the latest query long enough to send it.
    debounced: bool,
    /// The query T3 is searching for now.
    in_flight: Option<String>,
    _request: Option<Task<()>>,
}

impl MessageSearch {
    /// Forgets the query and its matches, but not the search T3 is running.
    fn clear(&mut self) {
        self.query.clear();
        self.matches.clear();
        self.raw.clear();
        self.pending = false;
        self.failed = false;
        self._debounce = None;
        self.debounced = false;
    }
}

/// The search as T3 sends it (`useThreadSearch`): trimmed and cut to T3's limit, or `None` when
/// it has no word of three letters or more, so T3 couldn't match messages.
pub(crate) fn message_query(query: &str) -> Option<String> {
    let query = query
        .trim()
        .chars()
        .take(MESSAGE_SEARCH_MAX_CHARS)
        .collect::<String>();
    let query = query.trim();
    let searchable = query
        .split_whitespace()
        .map(|word| word.trim_matches(|c| matches!(c, '"' | '\'' | '“' | '”')))
        .any(|word| word.chars().count() >= MESSAGE_WORD_MIN_CHARS);
    searchable.then(|| query.to_string())
}

/// The threads whose messages match, best first (`rankMessageMatches`): matches of whole words
/// or word starts first, then by quality, by how many messages match, and by how new the
/// message is. A subagent has no row, so its match is its nearest ancestor's (`parent_of` gives
/// a thread's parent). A thread found by title already isn't repeated, nor is any thread twice,
/// and threads the list doesn't have (archived ones, other projects when filtered) are left out.
pub(crate) fn rank_message_matches<'a>(
    matches: &[t3_api::types::OrchestrationThreadSearchMatch],
    list: &'a ThreadList,
    found_by_title: &HashSet<&str>,
    parent_of: impl Fn(&str) -> Option<String>,
) -> Vec<MessageMatch> {
    /// How far up a subagent's lineage its row is looked for.
    const MAX_DEPTH: usize = 8;
    let row_of = |thread_id: &str| -> Option<&'a ThreadEntry> {
        let mut thread_id = thread_id.to_string();
        for _ in 0..MAX_DEPTH {
            if let Some(entry) = list.find(&thread_id) {
                return Some(entry);
            }
            thread_id = parent_of(&thread_id)?;
        }
        None
    };
    let mut ranked = matches
        .iter()
        .filter_map(|found| Some((found, row_of(&found.thread_id)?)))
        .filter(|(_, thread)| !found_by_title.contains(thread.id.as_str()))
        .collect::<Vec<_>>();
    ranked.sort_by(|(left, _), (right, _)| {
        let quality =
            |found: &t3_api::types::OrchestrationThreadSearchMatch| found.quality.unwrap_or(1.);
        let word = |found| quality(found) >= MESSAGE_PREFIX_QUALITY;
        word(right)
            .cmp(&word(left))
            .then(quality(right).total_cmp(&quality(left)))
            .then(right.hits.unwrap_or(1).cmp(&left.hits.unwrap_or(1)))
            .then(right.message_created_at.cmp(&left.message_created_at))
    });
    let mut seen = HashSet::default();
    ranked
        .into_iter()
        .filter(|(_, thread)| seen.insert(thread.id.clone()))
        .take(MESSAGE_MATCH_LIMIT)
        .map(|(found, thread)| MessageMatch {
            thread: thread.clone(),
            snippet: found
                .snippet
                .split_whitespace()
                .collect::<Vec<_>>()
                .join(" "),
            from_agent: crate::model::wire_string(&found.source) == "assistant",
        })
        .collect()
}

/// The byte offsets of the characters of `text` that the search's words match, ignoring case,
/// for highlighting them.
pub(crate) fn highlights(text: &str, query: &str) -> Vec<usize> {
    let lower = |c: char| c.to_lowercase().next().unwrap_or(c);
    let chars = text.char_indices().collect::<Vec<_>>();
    let mut marked = vec![false; chars.len()];
    for word in query.split_whitespace() {
        let word = word.chars().map(lower).collect::<Vec<_>>();
        if word.is_empty() || word.len() > chars.len() {
            continue;
        }
        for start in 0..=chars.len() - word.len() {
            let found = word
                .iter()
                .enumerate()
                .all(|(offset, c)| lower(chars[start + offset].1) == *c);
            if found {
                marked[start..start + word.len()].fill(true);
            }
        }
    }
    chars
        .iter()
        .zip(marked)
        .filter(|(_, marked)| *marked)
        .map(|((index, _), _)| *index)
        .collect()
}

/// How many characters a snippet keeps before its first match, so the match shows in a row.
const SNIPPET_LEAD_CHARS: usize = 16;

/// `snippet` starting a little before the search's first match in it, at a word, so a row that
/// cuts it off still shows the match.
pub(crate) fn focus_snippet(snippet: &str, query: &str) -> String {
    let Some(first) = highlights(snippet, query).first().copied() else {
        return snippet.to_string();
    };
    let before = &snippet[..first];
    if before.chars().count() <= SNIPPET_LEAD_CHARS {
        return snippet.to_string();
    }
    let lead_start = before
        .char_indices()
        .rev()
        .nth(SNIPPET_LEAD_CHARS - 1)
        .map_or(0, |(index, _)| index);
    // From the start of a word, if there's one in the lead.
    let at_word = before[..lead_start]
        .chars()
        .last()
        .is_none_or(|c| c.is_whitespace() || c == '…');
    let start = if at_word {
        lead_start
    } else {
        before[lead_start..]
            .find(char::is_whitespace)
            .map_or(lead_start, |space| lead_start + space + 1)
            .min(first)
    };
    format!("…{}", snippet[start..].trim_start_matches('…'))
}

// Dragging threads.

/// A thread being dragged, and what the drag shows under the pointer.
#[derive(Clone, Debug)]
pub(crate) struct DraggedThread {
    pub id: String,
    pub title: SharedString,
}

impl Render for DraggedThread {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let palette = palette(cx);
        h_flex()
            .max_w(scaled(px(260.)))
            .px_2()
            .py_1()
            .gap_1p5()
            .rounded(scaled(tokens::CONTROL_RADIUS))
            .border(tokens::BORDER_WIDTH)
            .border_color(palette.accent.hsla())
            .bg(palette.surface_2.hsla())
            .shadow_md()
            .child(
                Icon::new(IconName::Chat)
                    .size(IconSize::XSmall)
                    .color(Color::Muted),
            )
            .child(
                Label::new(self.title.clone())
                    .size(LabelSize::Small)
                    .truncate(),
            )
    }
}

/// One place in the list, as a drop sees it: a thread, or a header or note between them.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum DropSlot<'a> {
    Thread(&'a str),
    Header(Section),
    Note,
}

/// The section a slot at `index` of `slots` is in (`sectionAtSidebarSlot`): Pinned until
/// Active's header, and so on.
fn section_at(slots: &[DropSlot], index: usize) -> Section {
    slots[..index.min(slots.len())]
        .iter()
        .fold(Section::Pinned, |section, slot| match slot {
            DropSlot::Header(header) if *header != Section::Pinned => *header,
            _ => section,
        })
}

/// Where the thread at `from` lands when dropped on the slot at `over`
/// (`resolveSidebarDropTarget`): it takes `over`'s place, going after it when it came from
/// above and before it when it came from below. `None` in Snoozed, which needs a wake time.
pub(crate) fn resolve_drop(slots: &[DropSlot], from: usize, over: usize) -> Option<DropTarget> {
    let DropSlot::Thread(_) = slots.get(from)? else {
        return None;
    };
    if over >= slots.len() {
        return None;
    }
    let mut moved = slots.to_vec();
    let dragged = moved.remove(from);
    moved.insert(over, dragged);
    let section = section_at(&moved, over);
    if section == Section::Snoozed {
        return None;
    }
    let (mut pinned_order, mut active_order) = (Vec::new(), Vec::new());
    let mut current = Section::Pinned;
    for slot in &moved {
        match slot {
            DropSlot::Header(Section::Active) => current = Section::Active,
            DropSlot::Header(Section::Snoozed | Section::Settled) => break,
            DropSlot::Thread(id) if current == Section::Pinned => pinned_order.push(id.to_string()),
            DropSlot::Thread(id) => active_order.push(id.to_string()),
            _ => {}
        }
    }
    Some(DropTarget {
        section,
        pinned_order,
        active_order,
    })
}

/// A drop's order, shown until T3's shell has it.
struct PendingDrop {
    thread_id: String,
    section: Section,
    /// The section's threads in their new order (none for Settled, where it goes on top).
    order: Vec<String>,
    /// The shell sequence of the drop's last command, once they're all sent.
    sequence: Option<i64>,
    sent: bool,
    _timeout: Option<Task<()>>,
}

/// Shows `pending` in `list`: its thread in the section it was dropped in, in its new order
/// (`orderItemsByPreferredIds` in T3's sidebar).
fn apply_pending_drop(list: &mut ThreadList, pending: &PendingDrop) {
    let sections = [
        &mut list.pinned,
        &mut list.active,
        &mut list.snoozed,
        &mut list.settled,
    ];
    let mut entry = None;
    for threads in sections {
        if let Some(index) = threads
            .iter()
            .position(|thread| thread.id == pending.thread_id)
        {
            entry = Some(threads.remove(index));
            break;
        }
    }
    let Some(mut entry) = entry else {
        return;
    };
    entry.section = pending.section;
    entry.pinned = pending.section == Section::Pinned;
    entry.settled = pending.section == Section::Settled;
    match pending.section {
        Section::Settled => list.settled.insert(0, entry),
        Section::Snoozed => list.snoozed.push(entry),
        Section::Pinned | Section::Active => {
            let threads = if pending.section == Section::Pinned {
                &mut list.pinned
            } else {
                &mut list.active
            };
            threads.push(entry);
            let rank = |id: &str| {
                pending
                    .order
                    .iter()
                    .position(|ordered| ordered == id)
                    .unwrap_or(usize::MAX)
            };
            // Stable, so the ones the order doesn't name keep their places after it.
            threads.sort_by_key(|thread| rank(&thread.id));
        }
    }
}

/// Which way [`PreviousThread`] and [`NextThread`] go.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Direction {
    Previous,
    Next,
}

/// What the sidebar asks of the panel that holds it.
pub enum ThreadsSidebarEvent {
    /// Show the sidebar: its search was asked for.
    Reveal,
}

/// Every sidebar, so [`SearchThreads`] can find its workspace's.
#[derive(Default)]
struct Sidebars(Vec<WeakEntity<ThreadsSidebar>>);

impl Global for Sidebars {}

/// The sidebar of `workspace`, if it has one.
fn sidebar_of(workspace: &Workspace, cx: &App) -> Option<Entity<ThreadsSidebar>> {
    let workspace_id = workspace.weak_handle().entity_id();
    cx.try_global::<Sidebars>()?.0.iter().find_map(|sidebar| {
        let sidebar = sidebar.upgrade()?;
        (sidebar.read(cx).workspace.entity_id() == workspace_id).then_some(sidebar)
    })
}

pub(crate) fn init(cx: &mut App) {
    cx.observe_new(|workspace: &mut Workspace, _, _| {
        workspace.register_action(|workspace, _: &SearchThreads, window, cx| {
            if let Some(sidebar) = sidebar_of(workspace, cx) {
                sidebar.update(cx, |sidebar, cx| sidebar.reveal_search(window, cx));
            }
        });
        workspace.register_action(|workspace, _: &PreviousThread, window, cx| {
            open_adjacent(workspace, Direction::Previous, window, cx)
        });
        workspace.register_action(|workspace, _: &NextThread, window, cx| {
            open_adjacent(workspace, Direction::Next, window, cx)
        });
    })
    .detach();
}

/// Opens the thread before or after the open one, in the order the sidebar shows them
/// (`resolveAdjacentThreadId`): from a draft or another kind of tab, the last or the first. It
/// stops at the ends. As T3 shows one thread at a time, the thread's tab is replaced, unless its
/// composer holds a draft, so stepping through the list doesn't pile up tabs.
fn open_adjacent(
    workspace: &mut Workspace,
    direction: Direction,
    window: &mut Window,
    cx: &mut Context<Workspace>,
) {
    let from = workspace
        .active_item(cx)
        .and_then(|item| item.downcast::<ThreadView>())
        .filter(|view| view.read(cx).thread_id().is_some());
    let current = from
        .as_ref()
        .and_then(|view| view.read(cx).thread_id().map(str::to_string));
    let target = match sidebar_of(workspace, cx) {
        Some(sidebar) => sidebar
            .read(cx)
            .adjacent(current.as_deref(), direction)
            .map(|thread| (thread.id.clone(), thread.title.clone())),
        None => {
            let shells = ThreadShells::global(cx);
            let filter = ThreadListFilter::get(cx);
            shells.read(cx).shell().and_then(|shell| {
                let list = ThreadList::from_shell(shell, &filter, now());
                let order = list.iter().collect::<Vec<_>>();
                adjacent_in(&order, current.as_deref(), direction)
                    .map(|thread| (thread.id.clone(), thread.title.clone()))
            })
        }
    };
    let Some((thread_id, title)) = target else {
        return;
    };
    let opened = ThreadView::open(thread_id, title.into(), workspace, window, cx);
    if let Some(from) = from
        && opened.is_some_and(|opened| opened != from)
        && from.read(cx).composer().read(cx).is_empty(cx)
        && let Some(pane) = workspace.pane_for(&from)
    {
        pane.update(cx, |pane, cx| {
            pane.close_item_by_id(from.entity_id(), workspace::SaveIntent::Skip, window, cx)
        })
        .detach_and_log_err(cx);
    }
}

fn adjacent_in<'a>(
    order: &[&'a ThreadEntry],
    current: Option<&str>,
    direction: Direction,
) -> Option<&'a ThreadEntry> {
    let Some(current) = current else {
        return match direction {
            Direction::Previous => order.last().copied(),
            Direction::Next => order.first().copied(),
        };
    };
    let index = order.iter().position(|thread| thread.id == current)?;
    match direction {
        Direction::Previous => index.checked_sub(1).map(|index| order[index]),
        Direction::Next => order.get(index + 1).copied(),
    }
}

pub struct ThreadsSidebar {
    focus_handle: FocusHandle,
    workspace: WeakEntity<Workspace>,
    shells: Entity<ThreadShells>,
    list: ThreadList,
    items: Vec<Item>,
    keys: Vec<ItemKey>,
    list_state: ListState,
    /// The window's rem size when the list's rows were measured: they're measured again when
    /// `ui_font_size` changes.
    rem_size: Pixels,
    folded: HashSet<Section>,
    settled_shown: usize,
    active_thread_id: Option<String>,
    /// The visit last sent (thread id and `visitedAt`), so each is sent once.
    last_visit: Option<(String, String)>,
    /// Whether the active thread's first visit since it became active was sent.
    visited_since_activated: bool,
    search: Entity<Editor>,
    query: String,
    messages: MessageSearch,
    context_menu: Option<(Entity<ContextMenu>, Point<Pixels>, Subscription)>,
    /// The thread being dragged, while a drag is on.
    dragging: Option<String>,
    pending_drop: Option<PendingDrop>,
    /// Bumped whenever the rows change, so the drop check below is redone.
    generation: usize,
    /// The last drop check: for (generation, dragged thread, row), whether it can drop there.
    drop_check: RefCell<Option<(usize, String, usize, bool)>>,
    _subscriptions: Vec<Subscription>,
    _clock: Task<()>,
}

impl EventEmitter<ThreadsSidebarEvent> for ThreadsSidebar {}

impl ThreadsSidebar {
    pub fn new(
        workspace: WeakEntity<Workspace>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        let shells = ThreadShells::global(cx);
        let search = cx.new(|cx| {
            let mut editor = Editor::single_line(window, cx);
            editor.set_placeholder_text("Search threads…", window, cx);
            editor
        });
        let subscriptions = vec![
            cx.observe(&shells, |this, _, cx| {
                this.shell_changed(cx);
                this.rebuild(cx);
                this.visit_active(cx);
            }),
            cx.observe_global::<ThreadListFilter>(|this, cx| this.rebuild(cx)),
            cx.subscribe(&search, |this, search, event: &EditorEvent, cx| {
                if let EditorEvent::BufferEdited = event {
                    this.query = search.read(cx).text(cx);
                    this.search_messages(cx);
                    this.rebuild(cx);
                }
            }),
        ];
        let clock = cx.spawn(async move |this, cx| {
            loop {
                cx.background_executor().timer(CLOCK_TICK).await;
                if this.update(cx, |this, cx| this.rebuild(cx)).is_err() {
                    return;
                }
            }
        });
        let this_sidebar = cx.entity().downgrade();
        let sidebars = &mut cx.default_global::<Sidebars>().0;
        sidebars.retain(|sidebar| sidebar.upgrade().is_some());
        sidebars.push(this_sidebar);
        let mut this = Self {
            focus_handle: cx.focus_handle(),
            workspace,
            shells,
            list: ThreadList::default(),
            items: Vec::new(),
            keys: Vec::new(),
            list_state: ListState::new(0, ListAlignment::Top, px(1000.)),
            rem_size: window.rem_size(),
            folded: HashSet::from_iter([Section::Snoozed, Section::Settled]),
            settled_shown: SETTLED_FIRST_PAGE,
            active_thread_id: None,
            last_visit: None,
            visited_since_activated: false,
            search,
            query: String::new(),
            messages: MessageSearch::default(),
            context_menu: None,
            dragging: None,
            pending_drop: None,
            generation: 0,
            drop_check: RefCell::new(None),
            _subscriptions: subscriptions,
            _clock: clock,
        };
        this.rebuild(cx);
        this
    }

    /// The thread before or after `current` in the order the sidebar shows them.
    fn adjacent(&self, current: Option<&str>, direction: Direction) -> Option<&ThreadEntry> {
        let order = self
            .items
            .iter()
            .filter_map(|item| match item {
                Item::Thread(section, index) | Item::Found(section, index) => {
                    self.thread(*section, *index)
                }
                Item::Message(index) => {
                    self.messages.matches.get(*index).map(|found| &found.thread)
                }
                _ => None,
            })
            .collect::<Vec<_>>();
        adjacent_in(&order, current, direction)
    }

    /// Asks T3 for the threads whose messages match the search, once the user stops typing for
    /// a moment (`orchestration.searchThreads`, a read). Servers without it give title matches
    /// alone.
    fn search_messages(&mut self, cx: &mut Context<Self>) {
        let Some(query) = message_query(&self.query) else {
            self.messages.clear();
            return;
        };
        if query == self.messages.query {
            return;
        }
        // Matches for a query this one refines or widens show until its own arrive; others go.
        let previous = std::mem::take(&mut self.messages.query);
        if !(query.contains(&previous) || previous.contains(&query)) {
            self.messages.raw.clear();
        }
        self.messages.query = query;
        self.messages.pending = true;
        self.messages.failed = false;
        self.messages.debounced = false;
        self.messages._debounce = Some(cx.spawn(async move |this, cx| {
            cx.background_executor()
                .timer(MESSAGE_SEARCH_DEBOUNCE)
                .await;
            this.update(cx, |this, cx| {
                this.messages.debounced = true;
                this.send_message_search(cx)
            })
            .ok();
        }));
    }

    /// Sends the latest query to T3, unless a search is in flight: then it goes when that one
    /// answers.
    fn send_message_search(&mut self, cx: &mut Context<Self>) {
        if self.messages.in_flight.is_some() || self.messages.query.is_empty() {
            return;
        }
        let Some(backend) = crate::backend(cx) else {
            return;
        };
        let query = self.messages.query.clone();
        self.messages.in_flight = Some(query.clone());
        let search = backend.call::<t3_api::rpc::OrchestrationSearchThreads>(
            OrchestrationV2SearchThreadsInput {
                environment_label: None,
                limit: None,
                older_settled_before: None,
                project_ids: None,
                query: query.clone(),
                titles_only: None,
            },
            Access::Read,
        );
        self.messages._request = Some(cx.spawn(async move |this, cx| {
            let result = search.await;
            this.update(cx, |this, cx| {
                this.messages_found(&query, result, cx);
                this.rebuild(cx);
            })
            .ok();
        }));
    }

    fn messages_found(
        &mut self,
        query: &str,
        result: anyhow::Result<t3_api::types::OrchestrationV2SearchThreadsResult>,
        cx: &mut Context<Self>,
    ) {
        let messages = &mut self.messages;
        messages.in_flight = None;
        let latest = messages.query == query;
        let related = messages.query.contains(query) || query.contains(messages.query.as_str());
        match result {
            // Shown while the latest query is searched, if it's related.
            Ok(result) if latest || related => messages.raw = result.matches,
            Ok(_) => {}
            Err(error) if latest => {
                messages.raw.clear();
                if crate::is_unsupported_rpc(&error) {
                    log::info!("this T3 can't search messages: {error:#}");
                } else {
                    log::warn!("couldn't search the threads' messages: {error:#}");
                    messages.failed = true;
                }
            }
            Err(_) => {}
        }
        if latest {
            messages.pending = false;
        } else if !messages.query.is_empty() && messages.debounced {
            self.send_message_search(cx);
        }
    }

    /// Lets go of a drop's order once the shell has caught up with it.
    fn shell_changed(&mut self, cx: &mut Context<Self>) {
        let sequence = self
            .shells
            .read(cx)
            .shell()
            .map_or(0, |shell| shell.sequence);
        if self.pending_drop.as_ref().is_some_and(|pending| {
            pending.sent && pending.sequence.is_none_or(|sent| sequence >= sent)
        }) {
            self.pending_drop = None;
        }
    }

    /// The list's places, as a drop sees them.
    fn drop_slots(&self) -> Vec<DropSlot<'_>> {
        self.items
            .iter()
            .map(|item| match item {
                Item::Header(section) => DropSlot::Header(*section),
                Item::Thread(section, index) => self
                    .thread(*section, *index)
                    .map_or(DropSlot::Note, |thread| DropSlot::Thread(&thread.id)),
                _ => DropSlot::Note,
            })
            .collect()
    }

    /// Where `thread_id` lands, dropped on row `over`, and what the drop does. Without `keys`,
    /// the plan's order keys aren't the ones to write, but whether it does anything is right.
    fn plan_drop(
        &self,
        thread_id: &str,
        over: usize,
        keys: Option<(&HashMap<String, String>, &HashMap<String, String>)>,
    ) -> Option<(DropTarget, DropPlan)> {
        if !self.query.trim().is_empty() {
            return None;
        }
        let slots = self.drop_slots();
        let from = slots
            .iter()
            .position(|slot| *slot == DropSlot::Thread(thread_id))?;
        let target = resolve_drop(&slots, from, over)?;
        let thread = self.list.find(thread_id)?;
        let ids = |threads: &[ThreadEntry]| {
            threads
                .iter()
                .map(|thread| thread.id.clone())
                .collect::<Vec<_>>()
        };
        let (pinned_order, active_order) = (ids(&self.list.pinned), ids(&self.list.active));
        let none = HashMap::default();
        let (pin_keys, active_keys) = keys.unwrap_or((&none, &none));
        let plan = organize::plan_drop(
            &DropSource {
                thread_id,
                section: thread.section,
                pinned: thread.pinned,
                settled: thread.settled,
                pinned_order: &pinned_order,
                active_order: &active_order,
                pin_keys,
                active_keys,
            },
            &target,
        );
        (plan != DropPlan::None).then_some((target, plan))
    }

    /// Whether `thread_id` can drop on row `over`; asked on every frame of a drag.
    fn can_drop(&self, thread_id: &str, over: usize) -> bool {
        if let Some((generation, id, row, can)) = &*self.drop_check.borrow()
            && *generation == self.generation
            && id == thread_id
            && *row == over
        {
            return *can;
        }
        let can = self.plan_drop(thread_id, over, None).is_some();
        *self.drop_check.borrow_mut() = Some((self.generation, thread_id.to_string(), over, can));
        can
    }

    /// Whether a drop on row `over` puts the dragged thread after it (it came from above) or
    /// before it.
    fn drops_after(&self, thread_id: &str, over: usize) -> bool {
        // Pinned's header takes a thread to the top of Pinned, under it.
        if self.items.get(over) == Some(&Item::Header(Section::Pinned)) {
            return true;
        }
        self.items
            .iter()
            .position(|item| match item {
                Item::Thread(section, index) => self
                    .thread(*section, *index)
                    .is_some_and(|thread| thread.id == thread_id),
                _ => false,
            })
            .is_some_and(|from| from < over)
    }

    fn start_drag(&mut self, thread_id: &str, cx: &mut Context<Self>) {
        if self.dragging.as_deref() != Some(thread_id) {
            self.dragging = Some(thread_id.to_string());
            self.rebuild(cx);
        }
    }

    fn end_drag(&mut self, cx: &mut Context<Self>) {
        if self.dragging.take().is_some() {
            self.rebuild(cx);
        }
    }

    /// Drops `dragged` on row `over`: shows the new order at once, and sends what T3 needs to
    /// get there, one command after another.
    pub(crate) fn drop_thread(
        &mut self,
        thread_id: &str,
        over: usize,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let (pin_keys, active_keys) = organize::order_keys(cx);
        let planned = self.plan_drop(thread_id, over, Some((&pin_keys, &active_keys)));
        self.dragging = None;
        let Some((target, plan)) = planned else {
            self.rebuild(cx);
            return;
        };
        let order = match target.section {
            Section::Pinned => target.pinned_order.clone(),
            Section::Active => target.active_order.clone(),
            Section::Snoozed | Section::Settled => Vec::new(),
        };
        let timeout = cx.spawn(async move |this, cx| {
            cx.background_executor().timer(PENDING_DROP_TIMEOUT).await;
            this.update(cx, |this, cx| {
                if this
                    .pending_drop
                    .as_ref()
                    .is_some_and(|pending| pending.sent)
                {
                    this.pending_drop = None;
                    this.rebuild(cx);
                }
            })
            .ok();
        });
        let settling = plan == DropPlan::Settle;
        self.pending_drop = Some(PendingDrop {
            thread_id: thread_id.to_string(),
            section: target.section,
            order,
            sequence: None,
            sent: settling,
            _timeout: Some(timeout),
        });
        self.rebuild(cx);
        if settling {
            let open_view = organize::open_view_of(&self.workspace, thread_id, cx);
            organize::settle_thread(
                thread_id.to_string(),
                open_view,
                self.workspace.clone(),
                window,
                cx,
            );
            return;
        }
        let send = organize::send_in_order(organize::drop_commands(thread_id, &plan), cx);
        let workspace = self.workspace.clone();
        cx.spawn(async move |this, cx| {
            let result = send.await;
            this.update(cx, |this, cx| {
                match result {
                    Ok(sequence) => {
                        if let Some(pending) = &mut this.pending_drop {
                            pending.sent = true;
                            pending.sequence = sequence;
                        }
                        this.shell_changed(cx);
                    }
                    Err(error) => {
                        this.pending_drop = None;
                        organize::report(
                            &workspace,
                            format!("Couldn't move the thread: {error:#}"),
                            cx,
                        );
                    }
                }
                this.rebuild(cx);
            })
            .ok();
        })
        .detach();
    }

    /// Makes row `index` a place to drop threads: it shows where the thread would go, and
    /// takes the drop.
    fn drop_zone<E: InteractiveElement>(
        &self,
        element: E,
        index: usize,
        cx: &mut Context<Self>,
    ) -> E {
        let sidebar = cx.entity().downgrade();
        let accent = palette(cx).accent.hsla();
        let check = sidebar.clone();
        element
            .can_drop(move |dragged, _, cx| {
                dragged
                    .downcast_ref::<DraggedThread>()
                    .zip(check.upgrade())
                    .is_some_and(|(dragged, sidebar)| sidebar.read(cx).can_drop(&dragged.id, index))
            })
            .drag_over::<DraggedThread>(move |style, dragged, _, cx| {
                let after = sidebar
                    .upgrade()
                    .is_some_and(|sidebar| sidebar.read(cx).drops_after(&dragged.id, index));
                // A line where the thread would go.
                let style = style.border_color(accent);
                let line = scaled(px(2.));
                if after {
                    style.border_b(line)
                } else {
                    style.border_t(line)
                }
            })
            .on_drop(
                cx.listener(move |this, dragged: &DraggedThread, window, cx| {
                    this.drop_thread(&dragged.id, index, window, cx)
                }),
            )
    }

    /// Highlights the thread the workspace shows, and tells T3 the user is looking at it.
    pub fn set_active_thread(&mut self, thread_id: Option<String>, cx: &mut Context<Self>) {
        if self.active_thread_id != thread_id {
            self.active_thread_id = thread_id;
            self.visited_since_activated = false;
            // An open thread shows even in a folded section.
            self.rebuild(cx);
            self.visit_active(cx);
            cx.notify();
        }
    }

    /// The threads the sidebar lists, in its sections.
    pub fn thread_list(&self) -> &ThreadList {
        &self.list
    }

    /// How many rows the list has: headers, threads, and notes.
    #[cfg(test)]
    pub(crate) fn row_count(&self) -> usize {
        self.items.len()
    }

    /// The list's rows, briefly: `Pinned`, `t:<thread id>`, `found:<id>`, `message:<id>`, and
    /// so on.
    #[cfg(test)]
    pub(crate) fn rows(&self) -> Vec<String> {
        self.items
            .iter()
            .map(|item| match item {
                Item::Header(section) => section.label().to_string(),
                Item::Thread(section, index) => {
                    format!("t:{}", self.list.section(*section)[*index].id)
                }
                Item::Found(section, index) => {
                    format!("found:{}", self.list.section(*section)[*index].id)
                }
                Item::Empty(section) => format!("empty:{}", section.label()),
                Item::MoreSettled => "more".to_string(),
                Item::MessagesHeader => "In messages".to_string(),
                Item::Message(index) => {
                    format!("message:{}", self.messages.matches[*index].thread.id)
                }
                Item::SearchingMessages => "searching".to_string(),
                Item::MessageSearchFailed => "search failed".to_string(),
                Item::NoMatches => "no matches".to_string(),
            })
            .collect()
    }

    /// The row described as [`Self::rows`] does.
    #[cfg(test)]
    pub(crate) fn row(&self, row: &str) -> usize {
        self.rows()
            .iter()
            .position(|described| described == row)
            .unwrap_or_else(|| panic!("no row {row} in {:?}", self.rows()))
    }

    #[cfg(test)]
    pub(crate) fn search_editor(&self) -> &Entity<Editor> {
        &self.search
    }

    #[cfg(test)]
    pub(crate) fn can_drop_on(&self, thread_id: &str, row: usize) -> bool {
        self.can_drop(thread_id, row)
    }

    #[cfg(test)]
    pub(crate) fn begin_drag(&mut self, thread_id: &str, cx: &mut Context<Self>) {
        self.start_drag(thread_id, cx)
    }

    /// Records a visit to the active thread (`thread.visit`) when it has news the user hasn't
    /// seen, as T3's visit tracker does (`threadVisits.ts`): once when it becomes active, and
    /// when a run finishes while it's open. Dry runs and read-only pairings record nothing.
    fn visit_active(&mut self, cx: &mut Context<Self>) {
        let Some(thread_id) = self.active_thread_id.clone() else {
            return;
        };
        let Some(thread) = self.shells.read(cx).thread(&thread_id) else {
            return;
        };
        let visited_at = thread.updated_at.clone();
        let up_to_date = millis(thread.last_visited_at.as_deref())
            .zip(millis(Some(&visited_at)))
            .is_some_and(|(visited, updated)| visited >= updated);
        let first = !self.visited_since_activated;
        let wanted = !up_to_date && (first || has_unseen_completion(thread));
        let sent = self.last_visit.as_ref() == Some(&(thread_id.clone(), visited_at.clone()));
        self.visited_since_activated = true;
        if !wanted || sent || organize::blocked_reason(cx).is_some() {
            return;
        }
        self.last_visit = Some((thread_id.clone(), visited_at.clone()));
        let send = organize::send(organize::visit(&thread_id, &visited_at, new_id()), cx);
        cx.background_spawn(async move {
            if let Err(error) = send.await {
                log::warn!("couldn't record the visit to thread {thread_id}: {error:#}");
            }
        })
        .detach();
    }

    /// Rebuilds the list from the shell, the filter, the search, the folds, and a drop T3's
    /// shell doesn't show yet.
    fn rebuild(&mut self, cx: &mut Context<Self>) {
        let shells = self.shells.read(cx);
        self.list = match shells.shell() {
            Some(shell) => ThreadList::from_shell(shell, &ThreadListFilter::get(cx), now()),
            None => ThreadList::default(),
        };
        if let Some(pending) = &self.pending_drop {
            apply_pending_drop(&mut self.list, pending);
        }
        self.messages.matches = if self.query.trim().is_empty() {
            Vec::new()
        } else {
            let found = self
                .list
                .search(&self.query)
                .into_iter()
                .map(|thread| thread.id.as_str())
                .collect::<HashSet<_>>();
            let shell = self.shells.read(cx).shell();
            let parent_of = |thread_id: &str| {
                shell?
                    .threads
                    .get(thread_id)?
                    .lineage
                    .parent_thread_id
                    .clone()
            };
            rank_message_matches(&self.messages.raw, &self.list, &found, parent_of)
        };
        self.generation += 1;
        let items = self.items_for_list();
        let keys = items.iter().map(|item| self.key(item)).collect::<Vec<_>>();
        // Only what changed is remeasured, and the scroll position stays.
        let prefix = self
            .keys
            .iter()
            .zip(&keys)
            .take_while(|(old, new)| old == new)
            .count();
        let suffix = self.keys[prefix..]
            .iter()
            .rev()
            .zip(keys[prefix..].iter().rev())
            .take_while(|(old, new)| old == new)
            .count();
        if prefix != self.keys.len() || prefix != keys.len() {
            self.list_state.splice(
                prefix..self.keys.len() - suffix,
                keys.len() - prefix - suffix,
            );
        }
        self.items = items;
        self.keys = keys;
        cx.notify();
    }

    fn items_for_list(&self) -> Vec<Item> {
        let mut items = Vec::new();
        if !self.query.trim().is_empty() {
            let found = self.list.search_positions(&self.query);
            let messages = &self.messages;
            if found.is_empty() && messages.matches.is_empty() && !messages.pending {
                items.push(Item::NoMatches);
            }
            items.extend(
                found
                    .into_iter()
                    .map(|(section, index)| Item::Found(section, index)),
            );
            if !messages.matches.is_empty() {
                items.push(Item::MessagesHeader);
                items.extend((0..messages.matches.len()).map(Item::Message));
            } else if messages.pending {
                items.push(Item::SearchingMessages);
            } else if messages.failed {
                items.push(Item::MessageSearchFailed);
            }
            return items;
        }
        if self.list.is_empty() {
            return items;
        }
        for section in Section::ALL {
            let threads = self.list.section(section);
            // While a thread is dragged, an empty Pinned takes it.
            let drop_to_pin = section == Section::Pinned && self.dragging.is_some();
            if threads.is_empty() && section != Section::Active && !drop_to_pin {
                continue;
            }
            items.push(Item::Header(section));
            if threads.is_empty() {
                items.push(Item::Empty(section));
                continue;
            }
            let folded = section.folds() && self.folded.contains(&section);
            let shown = if section == Section::Settled {
                self.settled_shown.min(threads.len())
            } else {
                threads.len()
            };
            for (index, thread) in threads.iter().enumerate() {
                let open = self.active_thread_id.as_deref() == Some(thread.id.as_str());
                // The open thread shows even when its section is folded or paged away.
                if (!folded && index < shown) || open {
                    items.push(Item::Thread(section, index));
                }
            }
            if section == Section::Settled && !folded && shown < threads.len() {
                items.push(Item::MoreSettled);
            }
        }
        items
    }

    fn key(&self, item: &Item) -> ItemKey {
        match item {
            Item::Header(section) => ItemKey::Header(*section),
            Item::Thread(section, index) | Item::Found(section, index) => {
                ItemKey::Thread(self.list.section(*section)[*index].id.clone())
            }
            Item::Empty(section) => ItemKey::Empty(*section),
            Item::MoreSettled => ItemKey::MoreSettled,
            Item::MessagesHeader => ItemKey::MessagesHeader,
            Item::Message(index) => ItemKey::Message(
                self.messages
                    .matches
                    .get(*index)
                    .map(|found| found.thread.id.clone())
                    .unwrap_or_default(),
            ),
            Item::SearchingMessages => ItemKey::SearchingMessages,
            Item::MessageSearchFailed => ItemKey::MessageSearchFailed,
            Item::NoMatches => ItemKey::NoMatches,
        }
    }

    fn thread(&self, section: Section, index: usize) -> Option<&ThreadEntry> {
        self.list.section(section).get(index)
    }

    pub(crate) fn toggle_fold(&mut self, section: Section, cx: &mut Context<Self>) {
        if !self.folded.remove(&section) {
            self.folded.insert(section);
        }
        if section == Section::Settled {
            self.settled_shown = SETTLED_FIRST_PAGE;
        }
        self.rebuild(cx);
    }

    pub(crate) fn show_more_settled(&mut self, cx: &mut Context<Self>) {
        self.settled_shown += SETTLED_PAGE;
        self.rebuild(cx);
    }

    fn open(&mut self, thread: &ThreadEntry, window: &mut Window, cx: &mut Context<Self>) {
        crate::open_thread(
            thread.id.clone(),
            thread.title.clone().into(),
            self.workspace.clone(),
            window,
            cx,
        );
    }

    fn settle_or_unsettle(
        &mut self,
        thread: &ThreadEntry,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if thread.section == Section::Settled {
            organize::unsettle_thread(&thread.id, self.workspace.clone(), cx);
        } else {
            let open_view = organize::open_view_of(&self.workspace, &thread.id, cx);
            organize::settle_thread(
                thread.id.clone(),
                open_view,
                self.workspace.clone(),
                window,
                cx,
            );
        }
    }

    fn deploy_context_menu(
        &mut self,
        position: Point<Pixels>,
        thread: ThreadEntry,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let menu = organize::thread_menu(
            thread,
            self.workspace.clone(),
            self.focus_handle.clone(),
            window,
            cx,
        );
        window.focus(&menu.focus_handle(cx), cx);
        let subscription = cx.subscribe(&menu, |this, _, _: &DismissEvent, cx| {
            this.context_menu.take();
            cx.notify();
        });
        self.context_menu = Some((menu, position, subscription));
        cx.notify();
    }

    /// Focuses the search, asking the panel to show the sidebar.
    pub fn reveal_search(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        cx.emit(ThreadsSidebarEvent::Reveal);
        let focus = self.search.focus_handle(cx);
        window.defer(cx, move |window, cx| window.focus(&focus, cx));
    }

    fn clear_search(&mut self, _: &menu::Cancel, window: &mut Window, cx: &mut Context<Self>) {
        if self.query.is_empty() {
            return;
        }
        self.search
            .update(cx, |search, cx| search.set_text("", window, cx));
    }

    fn render_item(
        &mut self,
        index: usize,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let Some(item) = self.items.get(index).cloned() else {
            return div().into_any_element();
        };
        match item {
            Item::Header(section) => self.render_header(index, section, cx),
            Item::Thread(section, row) | Item::Found(section, row) => {
                let found = matches!(item, Item::Found(..));
                match self.thread(section, row).cloned() {
                    Some(thread) => self.render_thread(index, thread, found, None, window, cx),
                    None => div().into_any_element(),
                }
            }
            Item::Message(row) => match self.messages.matches.get(row).cloned() {
                Some(found) => {
                    let snippet = found.snippet.clone();
                    self.render_thread(index, found.thread, true, Some(snippet), window, cx)
                }
                None => div().into_any_element(),
            },
            Item::MessagesHeader => div()
                .px_2()
                .pt_2()
                .pb_0p5()
                .child(
                    Label::new("In messages")
                        .size(LabelSize::XSmall)
                        .weight(FontWeight::SEMIBOLD)
                        .color(Color::Muted),
                )
                .into_any_element(),
            Item::SearchingMessages => row_note("Searching messages…"),
            Item::MessageSearchFailed => row_note("Couldn't search the threads' messages."),
            Item::Empty(Section::Pinned) => self
                .drop_zone(
                    div()
                        .id(("empty", index))
                        .mx_1()
                        .px_2()
                        .py_1()
                        .rounded(scaled(tokens::CONTROL_RADIUS))
                        .border_1()
                        .border_dashed()
                        .border_color(palette(cx).border.hsla())
                        .child(
                            Label::new("Drop here to pin")
                                .size(LabelSize::XSmall)
                                .color(Color::Muted),
                        ),
                    index,
                    cx,
                )
                .into_any_element(),
            Item::Empty(_) => self
                .drop_zone(
                    div().id(("empty", index)).px_3().py_1().child(
                        Label::new("Nothing active. Settled threads are below.")
                            .size(LabelSize::XSmall)
                            .color(Color::Muted),
                    ),
                    index,
                    cx,
                )
                .into_any_element(),
            Item::MoreSettled => {
                let hidden = self.list.settled.len().saturating_sub(self.settled_shown);
                div()
                    .px_2()
                    .py_1()
                    .child(
                        Button::new(
                            "more-settled",
                            format!("Show {} more", hidden.min(SETTLED_PAGE)),
                        )
                        .style(ButtonStyle::Subtle)
                        .label_size(LabelSize::Small)
                        .on_click(cx.listener(|this, _, _, cx| this.show_more_settled(cx))),
                    )
                    .into_any_element()
            }
            Item::NoMatches => row_note("No threads match."),
        }
    }

    fn render_header(&self, index: usize, section: Section, cx: &mut Context<Self>) -> AnyElement {
        let count = self.list.section(section).len();
        let folds = section.folds();
        let folded = folds && self.folded.contains(&section);
        let header = h_flex()
            .id(SharedString::from(format!("section-{}", section.label())))
            .w_full()
            .h(scaled(px(26.)))
            .px_2()
            .pt_1()
            .gap_1();
        self.drop_zone(header, index, cx)
            .when(folds, |this| {
                this.cursor_pointer()
                    .on_click(cx.listener(move |this, _, _, cx| this.toggle_fold(section, cx)))
                    .child(
                        Icon::new(if folded {
                            IconName::ChevronRight
                        } else {
                            IconName::ChevronDown
                        })
                        .size(IconSize::XSmall)
                        .color(Color::Muted),
                    )
            })
            .child(
                Label::new(section.label())
                    .size(LabelSize::XSmall)
                    .weight(FontWeight::SEMIBOLD)
                    .color(Color::Muted),
            )
            .child(
                Label::new(count.to_string())
                    .size(LabelSize::XSmall)
                    .color(Color::Muted),
            )
            .into_any_element()
    }

    /// A thread's row. `snippet`, for a message match, takes the place of its project and
    /// branch.
    fn render_thread(
        &self,
        index: usize,
        thread: ThreadEntry,
        found: bool,
        snippet: Option<String>,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let palette = palette(cx);
        let active = self.active_thread_id.as_deref() == Some(thread.id.as_str());
        let settled = thread.section == Section::Settled;
        let dot = match thread.status {
            Some(status) => status.color(cx),
            None => palette.text_muted.hsla().opacity(0.5),
        };
        let mut details = [thread.project.as_deref(), thread.branch.as_deref()]
            .into_iter()
            .flatten()
            .collect::<Vec<_>>()
            .join(" · ");
        if details.is_empty() {
            details = "No project".to_string();
        }
        if found {
            details = format!("{} · {details}", thread.section.label());
        }
        let details_line = match snippet {
            Some(snippet) => {
                let snippet = focus_snippet(&snippet, &self.messages.query);
                let highlighted = highlights(&snippet, &self.messages.query);
                HighlightedLabel::new(snippet, highlighted)
                    .size(LabelSize::XSmall)
                    .color(Color::Muted)
                    .truncate()
                    .into_any_element()
            }
            None => Label::new(details)
                .size(LabelSize::XSmall)
                .color(Color::Muted)
                .truncate()
                .into_any_element(),
        };
        let snoozed = thread
            .snoozed_until
            .as_deref()
            .filter(|_| thread.section == Section::Snoozed)
            .and_then(|until| wake_label(until, now()));
        let group = SharedString::from(format!("thread-row-{index}"));
        let can_write = organize::blocked_reason(cx).is_none();
        let pull_request = thread.pull_request.clone();
        let action_thread = thread.clone();
        let menu_thread = thread.clone();
        let open_thread = thread.clone();
        // Rows drag to reorder, except in search results.
        let dragged = (can_write && !found).then(|| DraggedThread {
            id: thread.id.clone(),
            title: thread.title.clone().into(),
        });
        let sidebar = cx.entity().downgrade();
        let row = div()
            .id(("thread", index))
            .group(group.clone())
            .w_full()
            .h(scaled(ROW_HEIGHT))
            .px_2()
            .py_1()
            .rounded(scaled(tokens::CONTROL_RADIUS))
            .cursor_pointer()
            .when(active, |this| this.bg(palette.accent_soft.hsla()))
            .hover(|style| style.bg(palette.surface_2.hsla()))
            .when_some(dragged, |this, dragged| {
                this.on_drag(dragged, move |dragged, _, _, cx| {
                    let thread_id = dragged.id.clone();
                    let sidebar = sidebar.clone();
                    cx.defer(move |cx| {
                        sidebar
                            .update(cx, |sidebar, cx| sidebar.start_drag(&thread_id, cx))
                            .ok();
                    });
                    cx.new(|_| dragged.clone())
                })
            });
        let row = if found {
            row
        } else {
            self.drop_zone(row, index, cx)
        };
        row.on_click(cx.listener(move |this, _, window, cx| this.open(&open_thread, window, cx)))
            .on_mouse_down(
                MouseButton::Right,
                cx.listener(move |this, event: &MouseDownEvent, window, cx| {
                    cx.stop_propagation();
                    this.deploy_context_menu(event.position, menu_thread.clone(), window, cx)
                }),
            )
            .child(
                v_flex()
                    .min_w_0()
                    .child(
                        h_flex()
                            .gap_1p5()
                            .child(Indicator::dot().color(Color::Custom(dot)))
                            .child(
                                div().flex_1().min_w_0().child(
                                    Label::new(thread.title.clone())
                                        .size(LabelSize::Small)
                                        .when(thread.unread, |label| {
                                            label.weight(FontWeight::SEMIBOLD)
                                        })
                                        .when(settled && !active, |label| label.color(Color::Muted))
                                        .truncate(),
                                ),
                            )
                            .when(can_write, |this| {
                                this.child(
                                    div().visible_on_hover(group.clone()).child(
                                        IconButton::new(
                                            ("settle", index),
                                            if settled {
                                                IconName::Undo
                                            } else {
                                                IconName::Check
                                            },
                                        )
                                        .icon_size(IconSize::XSmall)
                                        .icon_color(Color::Muted)
                                        .tooltip(move |_, cx| {
                                            if settled {
                                                Tooltip::simple("Un-settle", cx)
                                            } else {
                                                Tooltip::for_action("Settle", &SettleThread, cx)
                                            }
                                        })
                                        .on_click(
                                            cx.listener(move |this, _, window, cx| {
                                                this.settle_or_unsettle(&action_thread, window, cx)
                                            }),
                                        ),
                                    ),
                                )
                            }),
                    )
                    .child(
                        h_flex()
                            .gap_1p5()
                            .pl(scaled(px(12.)))
                            .child(div().flex_1().min_w_0().child(details_line))
                            .when_some(pull_request, |this, pull_request| {
                                this.child(pull_request_chip(index, &pull_request, cx))
                            })
                            .when_some(snoozed, |this, until| {
                                this.child(
                                    h_flex()
                                        .gap_0p5()
                                        .child(
                                            Icon::new(IconName::Clock)
                                                .size(IconSize::XSmall)
                                                .color(Color::Muted),
                                        )
                                        .child(
                                            Label::new(until)
                                                .size(LabelSize::XSmall)
                                                .color(Color::Muted),
                                        ),
                                )
                            })
                            .when_some(thread.status, |this, status| {
                                this.child(Pill::tinted(status.label(), status.color(cx)))
                            }),
                    ),
            )
            .into_any_element()
    }

    fn render_project_filter(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let filter = ThreadListFilter::get(cx);
        let mut projects = self
            .shells
            .read(cx)
            .shell()
            .map(|shell| {
                shell
                    .projects
                    .values()
                    .map(|project| (project.id.clone(), project.title.clone()))
                    .collect::<Vec<_>>()
            })
            .unwrap_or_default();
        projects.sort_by_cached_key(|(_, title)| title.to_lowercase());
        PopoverMenu::new("thread-project-filter")
            .trigger_with_tooltip(
                IconButton::new("thread-project-filter-button", IconName::Filter)
                    .icon_size(IconSize::Small)
                    .toggle_state(filter.project_id.is_some()),
                Tooltip::text("Filter by project"),
            )
            .anchor(gpui::Anchor::TopRight)
            .menu(move |window, cx| {
                let projects = projects.clone();
                let selected = filter.project_id.clone();
                Some(ContextMenu::build(window, cx, move |mut menu, _, _| {
                    menu = menu.toggleable_entry(
                        "All projects",
                        selected.is_none(),
                        IconPosition::End,
                        None,
                        |_, cx| ThreadListFilter::set_project(None, cx),
                    );
                    menu = menu.separator();
                    for (id, title) in projects {
                        let checked = selected.as_deref() == Some(id.as_str());
                        menu = menu.toggleable_entry(
                            title,
                            checked,
                            IconPosition::End,
                            None,
                            move |_, cx| ThreadListFilter::set_project(Some(id.clone()), cx),
                        );
                    }
                    menu
                }))
            })
    }
}

/// A row's pull request: its number, in its state's color, with its title and state on hover
/// (T3's `ThreadSearchPullRequestNumber` and the sidebar's PR indicator).
fn pull_request_chip(index: usize, pull_request: &RowPullRequest, cx: &App) -> AnyElement {
    let color = pull_request.color(cx);
    let tooltip = match &pull_request.title {
        Some(title) => format!(
            "#{} {title} ({})",
            pull_request.number,
            pull_request.state_label()
        ),
        None => format!("Pull request #{}", pull_request.number),
    };
    h_flex()
        .id(("thread-pull-request", index))
        .flex_none()
        .gap_0p5()
        .child(
            Icon::new(IconName::PullRequest)
                .size(IconSize::XSmall)
                .color(Color::Custom(color)),
        )
        .child(
            Label::new(format!("#{}", pull_request.number))
                .size(LabelSize::XSmall)
                .color(Color::Custom(color)),
        )
        .tooltip(Tooltip::text(tooltip))
        .into_any_element()
}

/// When a snoozed thread wakes, briefly (`snoozeWakeLabel`): in minutes, hours, or days.
fn wake_label(until: &str, now: OffsetDateTime) -> Option<String> {
    let remaining = millis(Some(until))? - now.unix_timestamp_nanos() / 1_000_000;
    const MINUTE: i128 = 60_000;
    const HOUR: i128 = 60 * MINUTE;
    const DAY: i128 = 24 * HOUR;
    Some(if remaining <= 0 {
        "now".to_string()
    } else if remaining < HOUR {
        format!("{}m", ((remaining + MINUTE - 1) / MINUTE).max(1))
    } else if remaining < DAY {
        format!("{}h", (remaining + HOUR - 1) / HOUR)
    } else {
        format!("{}d", (remaining + DAY - 1) / DAY)
    })
}

impl Focusable for ThreadsSidebar {
    fn focus_handle(&self, _: &App) -> FocusHandle {
        self.focus_handle.clone()
    }
}

impl Render for ThreadsSidebar {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        if self.rem_size != window.rem_size() {
            self.rem_size = window.rem_size();
            self.list_state.remeasure();
        }
        // A drag let go outside the list drops nothing.
        if self.dragging.is_some() && !cx.has_active_drag() {
            cx.defer_in(window, |this, _, cx| this.end_drag(cx));
        }
        let palette = palette(cx);
        let shells = self.shells.read(cx);
        let loaded = shells.shell().is_some();
        let error = shells.error().cloned();
        let open_count = self.list.pinned.len() + self.list.active.len();
        let filter = ThreadListFilter::get(cx);
        let filtered_project = filter.project_id.as_ref().map(|project_id| {
            shells
                .shell()
                .and_then(|shell| shell.projects.get(project_id))
                .map_or_else(|| "a project".to_string(), |project| project.title.clone())
        });
        let body = match (loaded, error) {
            (false, Some(error)) => note(error, Color::Error),
            (false, None) => note("Loading threads…", Color::Muted),
            _ if self.items.is_empty() => note(
                if filtered_project.is_some() {
                    "No threads in this project."
                } else {
                    "No agent threads yet."
                },
                Color::Muted,
            ),
            _ => list(
                self.list_state.clone(),
                cx.processor(|this, index: usize, window, cx| this.render_item(index, window, cx)),
            )
            .flex_1()
            .px_1()
            .into_any_element(),
        };
        v_flex()
            .key_context("ThreadsSidebar")
            .track_focus(&self.focus_handle)
            .on_action(cx.listener(Self::clear_search))
            .size_full()
            .bg(palette.surface.hsla())
            .border_r(tokens::BORDER_WIDTH)
            .border_color(palette.border.hsla())
            .child(
                h_flex()
                    .flex_none()
                    .gap_1()
                    .pl_3()
                    .pr_2()
                    .py(scaled(px(6.)))
                    .child(
                        Label::new("Agent threads")
                            .size(LabelSize::Small)
                            .weight(FontWeight::SEMIBOLD),
                    )
                    .when(loaded, |this| {
                        this.child(
                            Label::new(open_count.to_string())
                                .size(LabelSize::XSmall)
                                .color(Color::Muted),
                        )
                    })
                    .child(div().flex_1())
                    .child(self.render_project_filter(cx))
                    .child(
                        IconButton::new("archived-threads", IconName::Archive)
                            .icon_size(IconSize::Small)
                            .tooltip(Tooltip::text("Archived threads"))
                            .on_click(|_, window, cx| {
                                window.dispatch_action(Box::new(ShowArchivedThreads), cx)
                            }),
                    )
                    .child(
                        IconButton::new("add-project", IconName::FolderAdd)
                            .icon_size(IconSize::Small)
                            .tooltip(Tooltip::text("Add project…"))
                            .on_click(|_, window, cx| {
                                window.dispatch_action(Box::new(AddProject), cx)
                            }),
                    )
                    .child(
                        IconButton::new("new-agent-thread", IconName::Plus)
                            .icon_size(IconSize::Small)
                            .tooltip(Tooltip::text("New thread"))
                            .on_click(|_, window, cx| {
                                window.dispatch_action(Box::new(NewThread), cx)
                            }),
                    ),
            )
            .child(
                h_flex()
                    .flex_none()
                    .mx_2()
                    .mb_1()
                    .px_2()
                    .py_1()
                    .gap_1p5()
                    .rounded(scaled(tokens::CONTROL_RADIUS))
                    .border(tokens::BORDER_WIDTH)
                    .border_color(palette.border.hsla())
                    .bg(palette.background.hsla())
                    .child(
                        Icon::new(IconName::MagnifyingGlass)
                            .size(IconSize::XSmall)
                            .color(Color::Muted),
                    )
                    .child(div().flex_1().min_w_0().child(self.search.clone())),
            )
            .when_some(filtered_project, |this, project| {
                this.child(
                    h_flex().flex_none().px_2().pb_1().child(
                        Chip::new("thread-project-filter-chip", format!("{project} ✕"))
                            .accent(true)
                            .tooltip("Show every project")
                            .on_click(|_, _, cx| ThreadListFilter::set_project(None, cx)),
                    ),
                )
            })
            .child(body)
            .children(self.context_menu.as_ref().map(|(menu, position, _)| {
                deferred(
                    anchored()
                        .position(*position)
                        .anchor(gpui::Anchor::TopLeft)
                        .child(menu.clone()),
                )
                .with_priority(1)
            }))
    }
}

fn note(text: impl Into<SharedString>, color: Color) -> AnyElement {
    div()
        .px_3()
        .py_2()
        .child(Label::new(text.into()).size(LabelSize::Small).color(color))
        .into_any_element()
}

fn row_note(text: &'static str) -> AnyElement {
    div()
        .px_3()
        .py_1()
        .child(Label::new(text).size(LabelSize::XSmall).color(Color::Muted))
        .into_any_element()
}

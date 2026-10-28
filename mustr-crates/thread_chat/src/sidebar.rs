//! The threads sidebar: T3's threads in the sections of T3's own sidebar
//! (`components/Sidebar.tsx`): Pinned, Active, Snoozed and Settled, each row with T3's status
//! pill (`resolveThreadStatusPill` in `components/Sidebar.logic.ts`). It reads the app's shell
//! ([`ThreadShells`]), so it stays current with T3.
//!
//! Snoozed and Settled start collapsed, and Settled lists its newest threads a page at a time
//! (T3 0.1.1 sends every settled thread in the shell; 0.1.3's `getOlderSettledThreads` isn't
//! there to page from). Subagents have no rows of their own, as in T3: their parents' family
//! bars list them. A row's context menu settles, archives, pins and renames it (`organize.rs`),
//! and the header adds projects, starts threads, filters by project, and searches.

use crate::{
    commands::new_id,
    organize::{self, SettleThread},
    projects::AddProject,
    shell::{Shell, ThreadShells},
    thread_view::NewThread,
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
use std::{cmp::Reverse, time::Duration};
use t3_api::types::{
    OrchestrationV2AppThreadLineageRelationshipToParent, OrchestrationV2ThreadShell,
    OrchestrationV2ThreadShellSettledOverride, ProviderInteractionMode,
};
use time::{OffsetDateTime, format_description::well_known::Rfc3339};
use ui::{
    Color, ContextMenu, Icon, IconButton, IconName, IconPosition, IconSize, Indicator, Label,
    LabelCommon as _, LabelSize, PopoverMenu, Tooltip, prelude::*,
};
use workspace::Workspace;

actions!(
    thread_chat,
    [
        /// Shows the threads sidebar and focuses its search.
        SearchThreads,
    ]
);

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
    /// A section with nothing in it.
    Empty(Section),
    MoreSettled,
    /// A search result.
    Found(Section, usize),
    NoMatches,
}

/// What identifies a row across rebuilds, so the list keeps its scroll position.
#[derive(Clone, Debug, PartialEq, Eq)]
enum ItemKey {
    Header(Section),
    Thread(String),
    Empty(Section),
    MoreSettled,
    NoMatches,
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

pub(crate) fn init(cx: &mut App) {
    cx.observe_new(|workspace: &mut Workspace, _, _| {
        workspace.register_action(|workspace, _: &SearchThreads, window, cx| {
            let workspace_id = workspace.weak_handle().entity_id();
            let Some(sidebars) = cx.try_global::<Sidebars>() else {
                return;
            };
            let sidebar = sidebars.0.iter().find_map(|sidebar| {
                let sidebar = sidebar.upgrade()?;
                (sidebar.read(cx).workspace.entity_id() == workspace_id).then_some(sidebar)
            });
            if let Some(sidebar) = sidebar {
                sidebar.update(cx, |sidebar, cx| sidebar.reveal_search(window, cx));
            }
        });
    })
    .detach();
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
    context_menu: Option<(Entity<ContextMenu>, Point<Pixels>, Subscription)>,
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
                this.rebuild(cx);
                this.visit_active(cx);
            }),
            cx.observe_global::<ThreadListFilter>(|this, cx| this.rebuild(cx)),
            cx.subscribe(&search, |this, search, event: &EditorEvent, cx| {
                if let EditorEvent::BufferEdited = event {
                    this.query = search.read(cx).text(cx);
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
            context_menu: None,
            _subscriptions: subscriptions,
            _clock: clock,
        };
        this.rebuild(cx);
        this
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

    /// Rebuilds the list from the shell, the filter, the search and the folds.
    fn rebuild(&mut self, cx: &mut Context<Self>) {
        let shells = self.shells.read(cx);
        self.list = match shells.shell() {
            Some(shell) => ThreadList::from_shell(shell, &ThreadListFilter::get(cx), now()),
            None => ThreadList::default(),
        };
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
            if found.is_empty() {
                items.push(Item::NoMatches);
            }
            items.extend(
                found
                    .into_iter()
                    .map(|(section, index)| Item::Found(section, index)),
            );
            return items;
        }
        if self.list.is_empty() {
            return items;
        }
        for section in Section::ALL {
            let threads = self.list.section(section);
            if threads.is_empty() && section != Section::Active {
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
            Item::Header(section) => self.render_header(section, cx),
            Item::Thread(section, row) | Item::Found(section, row) => {
                let found = matches!(item, Item::Found(..));
                match self.thread(section, row).cloned() {
                    Some(thread) => self.render_thread(index, thread, found, window, cx),
                    None => div().into_any_element(),
                }
            }
            Item::Empty(_) => row_note("Nothing active. Settled threads are below."),
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

    fn render_header(&self, section: Section, cx: &mut Context<Self>) -> AnyElement {
        let count = self.list.section(section).len();
        let folds = section.folds();
        let folded = folds && self.folded.contains(&section);
        h_flex()
            .id(SharedString::from(format!("section-{}", section.label())))
            .w_full()
            .h(scaled(px(26.)))
            .px_2()
            .pt_1()
            .gap_1()
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

    fn render_thread(
        &self,
        index: usize,
        thread: ThreadEntry,
        found: bool,
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
        let snoozed = thread
            .snoozed_until
            .as_deref()
            .filter(|_| thread.section == Section::Snoozed)
            .and_then(|until| wake_label(until, now()));
        let group = SharedString::from(format!("thread-row-{index}"));
        let can_write = organize::blocked_reason(cx).is_none();
        let action_thread = thread.clone();
        let menu_thread = thread.clone();
        let open_thread = thread.clone();
        div()
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
            .on_click(cx.listener(move |this, _, window, cx| this.open(&open_thread, window, cx)))
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
                            .child(
                                div().flex_1().min_w_0().child(
                                    Label::new(details)
                                        .size(LabelSize::XSmall)
                                        .color(Color::Muted)
                                        .truncate(),
                                ),
                            )
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

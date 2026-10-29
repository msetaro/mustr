//! The Review area's page: what there is to review. Pull requests in sections, each a named
//! GitHub-style query (`review_sections.rs`; by default review requested from you, yours, and
//! others' you're involved in), from GitHub's search across every repository your account can see
//! (`github_search.rs`) or T3's `pullRequests.list` across its projects, with their checks,
//! reviews and labels; and the agent work waiting for review (threads whose runs finished with
//! changes you haven't reviewed, as the Inbox finds them). A search box narrows them all, and
//! takes a pull request's link (or `owner/repo#123`) to open any pull request. A row opens the
//! pull request's review through T3 (`open_pull_request.rs` routes ones outside T3's projects),
//! or the agent review.
//!
//! GitHub's results are cached per query: the page asks GitHub again when it's shown and its
//! lists are older than T3's list is kept fresh (30 seconds), every minute while it's in front,
//! and on Refresh.

use crate::{
    agent_review::{AgentReviewTarget, open_agent_review},
    github_search::{GITHUB_HOST, GitHubSearch, SearchPage, count_label},
    home::{AgentWork, load_threads},
    inbox::{
        ChecksState, PullRequestEntry, PullRequestList, PullRequestState, ReviewDecision,
        ThreadStatus,
    },
    open_pull_request::{
        self, OpenError, PullRequestLink, T3Project, github_repositories, parse_link,
        projects_from_shell,
    },
    pull_request_review,
    review_sections::{
        DialogFocus, ProjectRepositories, PullRequestSource as _, QuerySection, ReviewSettings,
        SectionDialog, SectionSource, T3PullRequests, parse_query, remove_section, section_source,
    },
    t3_connection::{T3Connection, T3Session},
};
use collections::HashSet;
use editor::{Editor, EditorEvent};
use gpui::{
    AnyElement, App, Entity, EventEmitter, FocusHandle, Focusable, Hsla, Subscription, Task,
    WeakEntity,
};
use mustr_ui::{MustrIcon, Pill, TextButton, tokens, tokens::scaled};
use review::ReviewScope;
use std::{
    sync::Arc,
    time::{Duration, Instant},
};
use ui::{
    Button, ButtonStyle, ContextMenu, DiffStat, Divider, Indicator, PopoverMenu, Tooltip,
    prelude::*,
};
use workspace::{
    Workspace,
    item::{Item, ItemEvent},
};

const CONTENT_MAX_WIDTH: f32 = 880.;

/// The name of the agent work's section, after the pull requests'.
pub const CODE_SECTION: &str = "Code: agent work";

/// How old a list may be when the page is shown before it's asked for again (T3's own list is
/// fresh for as long, `staleTimeMs` in T3's `client-runtime/src/state/pullRequests.ts`).
const STALE_AFTER: Duration = Duration::from_secs(30);

/// How often the page in front asks for its lists again (as T3 refreshes a linked pull
/// request).
const REFRESH_INTERVAL: Duration = Duration::from_secs(60);

/// The most labels a row shows.
const ROW_LABELS: usize = 3;

/// The pull requests of `list` a T3 section's query keeps, newest first, after the search box's
/// words.
pub fn section_rows(query: &str, list: &PullRequestList, search: &str) -> Vec<usize> {
    let query = parse_query(query);
    let mut indices = (0..list.entries.len())
        .filter(|index| {
            let entry = &list.entries[*index];
            query.keeps(entry, list) && entry.matches(search)
        })
        .collect::<Vec<_>>();
    // Every timestamp is an ISO 8601 UTC string, so string order is time order.
    indices.sort_by(|left, right| {
        list.entries[*right]
            .updated_at
            .cmp(&list.entries[*left].updated_at)
    });
    indices
}

/// The rows of a section: a GitHub search's in GitHub's order (the query's `sort:`), a T3 list's
/// narrowed by what T3 doesn't filter.
fn rows_of(source: SectionSource, query: &str, list: &PullRequestList, search: &str) -> Vec<usize> {
    match source {
        SectionSource::T3Projects => section_rows(query, list, search),
        SectionSource::GitHub => (0..list.entries.len())
            .filter(|index| list.entries[*index].matches(search))
            .collect(),
    }
}

/// Whether agent work matches a search: its title, project, branch, or agent.
pub fn agent_work_matches(work: &AgentWork, query: &str) -> bool {
    let query = query.trim().to_lowercase();
    if query.is_empty() {
        return true;
    }
    let thread = &work.thread;
    [
        Some(thread.title.as_str()),
        thread.project.as_deref(),
        thread.branch.as_deref(),
        thread.agent.as_deref(),
    ]
    .into_iter()
    .flatten()
    .any(|field| field.to_lowercase().contains(&query))
}

/// A label's color, from GitHub's hex digits.
fn label_color(color: Option<&str>) -> Option<Hsla> {
    let color = color?.trim_start_matches('#');
    (color.len() == 6)
        .then(|| u32::from_str_radix(color, 16).ok())
        .flatten()
        .map(|rgb| gpui::rgb(rgb).into())
}

enum Loadable<T> {
    NotLoaded,
    Loading,
    Loaded(T),
    Failed(SharedString),
}

#[derive(Clone, Debug, PartialEq, Eq)]
enum RowKey {
    PullRequest(usize, usize),
    Thread(String),
}

/// A section's pull requests, as far as they've been loaded.
struct SectionList {
    list: PullRequestList,
    /// How many GitHub's search finds in all.
    total: Option<u64>,
    /// Where GitHub's next page starts.
    next_cursor: Option<String>,
    /// Pages loaded after the first, with Load more. A refresh leaves such a list alone.
    more_pages: usize,
    loaded_at: Instant,
}

impl SectionList {
    fn new(list: PullRequestList) -> Self {
        Self {
            list,
            total: None,
            next_cursor: None,
            more_pages: 0,
            loaded_at: Instant::now(),
        }
    }

    fn from_page(page: SearchPage) -> Self {
        Self {
            total: Some(page.total),
            next_cursor: page.next_cursor,
            ..Self::new(page.list)
        }
    }
}

struct SectionState {
    source: SectionSource,
    list: Loadable<SectionList>,
    loading_more: bool,
    more_error: Option<SharedString>,
    _task: Option<Task<()>>,
    _more_task: Option<Task<()>>,
}

impl SectionState {
    fn new(source: SectionSource) -> Self {
        Self {
            source,
            list: Loadable::NotLoaded,
            loading_more: false,
            more_error: None,
            _task: None,
            _more_task: None,
        }
    }
}

pub struct ReviewPage {
    focus_handle: FocusHandle,
    workspace: WeakEntity<Workspace>,
    connection: Entity<T3Connection>,
    search: Entity<Editor>,
    /// The connection generation the T3 lists were loaded from.
    loaded_generation: Option<usize>,
    /// T3's projects, which route pull requests outside them and decide sections' sources.
    projects: Option<Vec<T3Project>>,
    /// The pull request sections, and what each lists.
    sections: Vec<QuerySection>,
    states: Vec<SectionState>,
    /// The sections folded away, by name.
    collapsed: HashSet<String>,
    agent_work: Loadable<Vec<AgentWork>>,
    opening: Option<(RowKey, Task<()>)>,
    open_error: Option<OpenError>,
    _projects_task: Option<Task<()>>,
    _agent_work_task: Option<Task<()>>,
    _refresh_timer: Task<()>,
    _subscriptions: Vec<Subscription>,
}

/// Shows the Review page, making it the first time. Lists older than [`STALE_AFTER`] are asked
/// for again.
pub fn show(workspace: &mut Workspace, window: &mut Window, cx: &mut Context<Workspace>) {
    let existing = workspace
        .panes()
        .iter()
        .find_map(|pane| pane.read(cx).items_of_type::<ReviewPage>().next());
    if let Some(existing) = existing {
        workspace.activate_item(&existing, true, true, window, cx);
        existing.update(cx, |page, cx| page.refresh_stale(STALE_AFTER, window, cx));
        return;
    }
    let weak_workspace = workspace.weak_handle();
    let page = cx.new(|cx| ReviewPage::new(weak_workspace, window, cx));
    workspace.add_item_to_active_pane(Box::new(page), None, true, window, cx);
}

impl ReviewPage {
    pub fn new(
        workspace: WeakEntity<Workspace>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        let connection = T3Connection::global(cx);
        let search = cx.new(|cx| {
            let mut editor = Editor::single_line(window, cx);
            editor.set_placeholder_text(
                "Search pull requests and agent work, or paste a pull request's link",
                window,
                cx,
            );
            editor
        });
        let subscriptions = vec![
            cx.observe_in(&connection, window, |this, _, window, cx| {
                this.connection_changed(window, cx)
            }),
            cx.subscribe(&search, |this, _, event: &EditorEvent, cx| {
                if let EditorEvent::BufferEdited = event {
                    this.open_error = None;
                    cx.notify();
                }
            }),
            cx.observe_global_in::<ReviewSettings>(window, |this, window, cx| {
                this.sections_changed(window, cx)
            }),
        ];
        let refresh_timer = cx.spawn_in(window, async move |this, cx| {
            loop {
                cx.background_executor().timer(REFRESH_INTERVAL).await;
                let refreshed = this.update_in(cx, |this, window, cx| {
                    if window.is_window_active() && this.is_in_front(cx) {
                        this.refresh_stale(REFRESH_INTERVAL, window, cx);
                    }
                });
                if refreshed.is_err() {
                    break;
                }
            }
        });
        let sections = ReviewSettings::get(cx).sections;
        let repositories = ProjectRepositories::get(cx);
        let mut this = Self {
            focus_handle: cx.focus_handle(),
            workspace,
            connection,
            search,
            loaded_generation: None,
            projects: None,
            states: sections
                .iter()
                .map(|section| SectionState::new(section_source(section, &repositories)))
                .collect(),
            sections,
            collapsed: HashSet::default(),
            agent_work: Loadable::NotLoaded,
            opening: None,
            open_error: None,
            _projects_task: None,
            _agent_work_task: None,
            _refresh_timer: refresh_timer,
            _subscriptions: subscriptions,
        };
        // GitHub's sections don't wait for T3.
        this.load_sections(STALE_AFTER, window, cx);
        this.connection_changed(window, cx);
        this
    }

    /// Whether the page is what its workspace shows.
    fn is_in_front(&self, cx: &Context<Self>) -> bool {
        self.workspace
            .upgrade()
            .and_then(|workspace| workspace.read(cx).active_item(cx))
            .is_some_and(|item| item.item_id() == cx.entity_id())
    }

    fn connection_changed(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let connection = self.connection.read(cx);
        let generation = connection.generation();
        if let Some(session) = connection.session()
            && self.loaded_generation != Some(generation)
        {
            self.loaded_generation = Some(generation);
            self.load_projects(session.clone(), window, cx);
            self.load_t3_sections(window, cx);
            self.load_agent_work(session, window, cx);
        }
        cx.notify();
    }

    /// Lists each section's pull requests again when the sections change.
    fn sections_changed(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let sections = ReviewSettings::get(cx).sections;
        if sections == self.sections {
            return;
        }
        let repositories = self.repositories(cx);
        self.sections = sections;
        self.states = self
            .sections
            .iter()
            .map(|section| SectionState::new(section_source(section, &repositories)))
            .collect();
        self.opening = None;
        self.load_sections(STALE_AFTER, window, cx);
        cx.notify();
    }

    fn repositories(&self, cx: &App) -> HashSet<String> {
        match &self.projects {
            Some(projects) => github_repositories(projects),
            None => ProjectRepositories::get(cx),
        }
    }

    /// Lists the pull requests and the agent work again (the page's Refresh), asking GitHub
    /// rather than its cache.
    pub fn reload(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.open_error = None;
        self.load_sections(Duration::ZERO, window, cx);
        if let Some(session) = self.connection.read(cx).session() {
            self.load_projects(session.clone(), window, cx);
            self.load_agent_work(session, window, cx);
        }
        cx.notify();
    }

    /// Asks again for the lists older than `max_age` (the ones you've loaded more of stay), and
    /// for those that failed.
    pub fn refresh_stale(
        &mut self,
        max_age: Duration,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        for index in 0..self.states.len() {
            let stale = match &self.states[index].list {
                Loadable::Loaded(list) => {
                    list.more_pages == 0 && list.loaded_at.elapsed() >= max_age
                }
                Loadable::Failed(_) | Loadable::NotLoaded => true,
                Loadable::Loading => false,
            };
            if stale {
                self.load_section(index, max_age, window, cx);
            }
        }
    }

    fn load_sections(&mut self, max_age: Duration, window: &mut Window, cx: &mut Context<Self>) {
        for index in 0..self.states.len() {
            self.load_section(index, max_age, window, cx);
        }
    }

    fn load_t3_sections(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        for index in 0..self.states.len() {
            if self.states[index].source == SectionSource::T3Projects {
                self.load_section(index, Duration::ZERO, window, cx);
            }
        }
    }

    /// Loads a section's first page. What it showed stays until the new list arrives.
    fn load_section(
        &mut self,
        index: usize,
        max_age: Duration,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let (Some(section), Some(state)) = (self.sections.get(index), self.states.get(index))
        else {
            return;
        };
        let name = section.name.clone();
        let list = match state.source {
            SectionSource::T3Projects => {
                let Some(session) = self.connection.read(cx).session() else {
                    return;
                };
                let list = T3PullRequests(session).list(&parse_query(&section.query));
                cx.background_spawn(async move {
                    list.await
                        .map(SectionList::new)
                        .map_err(|error| SharedString::from(format!("{error:#}")))
                })
            }
            SectionSource::GitHub => {
                let page = GitHubSearch::global(cx).first_page(&section.query, max_age);
                cx.background_spawn(async move {
                    page.await
                        .map(SectionList::from_page)
                        .map_err(|error| SharedString::from(error.message()))
                })
            }
        };
        let state = &mut self.states[index];
        if !matches!(state.list, Loadable::Loaded(_)) {
            state.list = Loadable::Loading;
        }
        state._more_task = None;
        state.loading_more = false;
        state.more_error = None;
        state._task = Some(cx.spawn_in(window, async move |this, cx| {
            let result = list.await;
            this.update(cx, |this, cx| {
                let Some(state) = this.states.get_mut(index) else {
                    return;
                };
                state.list = match result {
                    Ok(list) => Loadable::Loaded(list),
                    Err(error) => {
                        log::error!("couldn't list the pull requests of {name}: {error}");
                        Loadable::Failed(error)
                    }
                };
                cx.notify();
            })
            .ok();
        }));
    }

    /// Loads the next page of a GitHub section's results.
    fn load_more(&mut self, index: usize, window: &mut Window, cx: &mut Context<Self>) {
        let Some(section) = self.sections.get(index) else {
            return;
        };
        let Some(state) = self.states.get_mut(index) else {
            return;
        };
        let Loadable::Loaded(list) = &state.list else {
            return;
        };
        let Some(cursor) = list.next_cursor.clone() else {
            return;
        };
        if state.loading_more {
            return;
        }
        let page = GitHubSearch::global(cx).next_page(&section.query, cursor);
        state.loading_more = true;
        state.more_error = None;
        state._more_task = Some(cx.spawn_in(window, async move |this, cx| {
            let result = page.await;
            this.update(cx, |this, cx| {
                let Some(state) = this.states.get_mut(index) else {
                    return;
                };
                state.loading_more = false;
                match (result, &mut state.list) {
                    (Ok(page), Loadable::Loaded(list)) => {
                        let shown = list
                            .list
                            .entries
                            .iter()
                            .map(|entry| (entry.repository.to_lowercase(), entry.number))
                            .collect::<HashSet<_>>();
                        // A pull request updated since moves up a page; it's shown once.
                        list.list
                            .entries
                            .extend(page.list.entries.into_iter().filter(|entry| {
                                !shown.contains(&(entry.repository.to_lowercase(), entry.number))
                            }));
                        list.next_cursor = page.next_cursor;
                        list.total = Some(page.total);
                        list.more_pages += 1;
                    }
                    (Ok(_), _) => {}
                    (Err(error), _) => state.more_error = Some(error.message().into()),
                }
                cx.notify();
            })
            .ok();
        }));
        cx.notify();
    }

    fn load_projects(
        &mut self,
        session: Arc<T3Session>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self._projects_task = Some(cx.spawn_in(window, async move |this, cx| {
            let snapshot = cx
                .background_spawn(async move { session.shell_snapshot().await })
                .await;
            this.update_in(cx, |this, window, cx| match snapshot {
                Ok(snapshot) => this.set_projects(projects_from_shell(&snapshot), window, cx),
                Err(error) => log::error!("couldn't read T3's projects: {error:#}"),
            })
            .ok();
        }));
    }

    /// Takes T3's projects: they decide the sections whose `repo:`s may be T3's, and route the
    /// pull requests outside them.
    fn set_projects(
        &mut self,
        projects: Vec<T3Project>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let repositories = github_repositories(&projects);
        if ProjectRepositories::get(cx) != repositories {
            cx.set_global(ProjectRepositories(repositories.clone()));
        }
        self.projects = Some(projects);
        for index in 0..self.states.len() {
            let source = section_source(&self.sections[index], &repositories);
            if self.states[index].source != source {
                self.states[index] = SectionState::new(source);
                self.load_section(index, STALE_AFTER, window, cx);
            }
        }
        cx.notify();
    }

    fn load_agent_work(
        &mut self,
        session: Arc<T3Session>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if !matches!(self.agent_work, Loadable::Loaded(_)) {
            self.agent_work = Loadable::Loading;
        }
        self._agent_work_task = Some(cx.spawn_in(window, async move |this, cx| {
            let result = cx
                .background_spawn(async move { load_threads(&session).await })
                .await;
            this.update(cx, |this, cx| {
                this.agent_work = match result {
                    Ok(lists) => Loadable::Loaded(lists.work),
                    Err(error) => {
                        log::error!("couldn't load agent threads: {error:#}");
                        Loadable::Failed(format!("{error:#}").into())
                    }
                };
                cx.notify();
            })
            .ok();
        }));
    }

    /// Shows these lists, as if T3 had sent them: one per section. For tests.
    #[cfg(test)]
    pub fn set_lists(
        &mut self,
        pull_requests: Vec<PullRequestList>,
        agent_work: Vec<AgentWork>,
        cx: &mut Context<Self>,
    ) {
        for (state, list) in self.states.iter_mut().zip(pull_requests) {
            state.list = Loadable::Loaded(SectionList::new(list));
        }
        self.agent_work = Loadable::Loaded(agent_work);
        cx.notify();
    }

    #[cfg(test)]
    pub fn search_editor(&self) -> Entity<Editor> {
        self.search.clone()
    }

    #[cfg(test)]
    pub fn sources(&self) -> Vec<SectionSource> {
        self.states.iter().map(|state| state.source).collect()
    }

    #[cfg(test)]
    pub fn section_error(&self, index: usize) -> Option<SharedString> {
        match &self.states.get(index)?.list {
            Loadable::Failed(error) => Some(error.clone()),
            _ => None,
        }
    }

    #[cfg(test)]
    pub fn open_error(&self) -> Option<&OpenError> {
        self.open_error.as_ref()
    }

    #[cfg(test)]
    pub fn load_more_for_test(
        &mut self,
        index: usize,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.load_more(index, window, cx)
    }

    #[cfg(test)]
    pub fn open_row_for_test(
        &mut self,
        section: usize,
        index: usize,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.open_pull_request(section, index, window, cx)
    }

    /// The sections as they show (for tests), by name, with the rows each lists: pull requests by their
    /// reference, agent work by its thread's id.
    #[cfg(test)]
    pub fn sections(&self, cx: &App) -> Vec<(String, Vec<String>)> {
        let search = self.search.read(cx).text(cx);
        let mut sections = self
            .sections
            .iter()
            .zip(&self.states)
            .map(|(section, state)| {
                let rows = match &state.list {
                    Loadable::Loaded(list) => {
                        rows_of(state.source, &section.query, &list.list, &search)
                            .into_iter()
                            .map(|index| list.list.entries[index].display_ref())
                            .collect()
                    }
                    _ => Vec::new(),
                };
                (section.name.clone(), rows)
            })
            .collect::<Vec<_>>();
        let work = match &self.agent_work {
            Loadable::Loaded(work) => work
                .iter()
                .filter(|work| agent_work_matches(work, &search))
                .map(|work| work.thread.id.clone())
                .collect(),
            _ => Vec::new(),
        };
        sections.push((CODE_SECTION.to_string(), work));
        sections
    }

    fn open_pull_request(
        &mut self,
        section: usize,
        index: usize,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(Loadable::Loaded(list)) = self.states.get(section).map(|state| &state.list) else {
            return;
        };
        let Some(entry) = list.list.entries.get(index).cloned() else {
            return;
        };
        let link = PullRequestLink::new(
            entry.host.as_deref().unwrap_or(GITHUB_HOST),
            &entry.repository,
            entry.number,
        );
        let url = entry.url.clone().unwrap_or_else(|| link.web_url());
        let open = if entry.project_id.is_empty() {
            open_pull_request::open_link(
                link,
                self.projects.clone(),
                self.workspace.clone(),
                window,
                cx,
            )
        } else {
            let open = pull_request_review::open_review(
                self.workspace.clone(),
                entry.reference(),
                window,
                cx,
            );
            cx.background_spawn(async move {
                open.await.map_err(|error| OpenError {
                    message: format!("{}: {error:#}", entry.display_ref()).into(),
                    url,
                })
            })
        };
        self.watch_open(RowKey::PullRequest(section, index), open, cx);
    }

    /// Opens the pull request whose link is in the search box.
    fn open_search_link(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(link) = parse_link(&self.search.read(cx).text(cx)) else {
            return;
        };
        let open = open_pull_request::open_link(
            link.clone(),
            self.projects.clone(),
            self.workspace.clone(),
            window,
            cx,
        );
        self.watch_open(RowKey::PullRequest(usize::MAX, 0), open, cx);
    }

    fn confirm(&mut self, _: &menu::Confirm, window: &mut Window, cx: &mut Context<Self>) {
        self.open_search_link(window, cx);
    }

    fn watch_open(
        &mut self,
        key: RowKey,
        open: Task<Result<(), OpenError>>,
        cx: &mut Context<Self>,
    ) {
        self.open_error = None;
        let task = cx.spawn(async move |this, cx| {
            let result = open.await;
            this.update(cx, |this, cx| {
                this.opening = None;
                if let Err(error) = result {
                    this.open_error = Some(error);
                }
                cx.notify();
            })
            .ok();
        });
        self.opening = Some((key, task));
        cx.notify();
    }

    fn open_agent_work(&mut self, work: &AgentWork, window: &mut Window, cx: &mut Context<Self>) {
        self.open_error = None;
        let thread = work.thread.clone();
        let key = RowKey::Thread(thread.id.clone());
        let workspace = self.workspace.clone();
        let task = cx.spawn_in(window, async move |this, cx| {
            let title = thread.title.clone();
            let result = match workspace.update_in(cx, |workspace, window, cx| {
                open_agent_review(
                    workspace,
                    AgentReviewTarget {
                        thread,
                        scope: ReviewScope::Changes,
                    },
                    None,
                    window,
                    cx,
                )
            }) {
                Ok(task) => task.await.map(|_| ()),
                Err(error) => Err(error),
            };
            this.update(cx, |this, cx| {
                this.opening = None;
                if let Err(error) = result {
                    log::error!("couldn't open the review of {title}: {error:#}");
                    this.open_error = Some(OpenError {
                        message: format!("{title}: {error:#}").into(),
                        url: String::new(),
                    });
                }
                cx.notify();
            })
            .ok();
        });
        self.opening = Some((key, task));
        cx.notify();
    }

    fn is_opening(&self, key: &RowKey) -> bool {
        self.opening
            .as_ref()
            .is_some_and(|(opening, _)| opening == key)
    }

    fn edit_section(
        &self,
        index: Option<usize>,
        focus: DialogFocus,
        window: &mut Window,
        cx: &mut App,
    ) {
        self.workspace
            .update(cx, |workspace, cx| {
                SectionDialog::open(index, focus, workspace, window, cx)
            })
            .ok();
    }

    fn render_search(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let palette = mustr_ui::palette(cx);
        h_flex()
            .gap_2()
            .child(
                h_flex()
                    .flex_1()
                    .min_w_0()
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
            .child(
                IconButton::new("review-page-refresh", IconName::RotateCw)
                    .icon_size(IconSize::Small)
                    .tooltip(Tooltip::text("Refresh"))
                    .on_click(cx.listener(|this, _, window, cx| this.reload(window, cx))),
            )
    }

    /// The card a pull request's link in the search box shows: Enter or Open opens it.
    fn render_link(&self, link: &PullRequestLink, cx: &mut Context<Self>) -> impl IntoElement {
        let opening = self.is_opening(&RowKey::PullRequest(usize::MAX, 0));
        card(cx).child(
            h_flex()
                .gap_3()
                .px_2()
                .child(
                    MustrIcon::PullRequest
                        .icon()
                        .size(IconSize::Small)
                        .color(Color::Accent),
                )
                .child(
                    v_flex()
                        .flex_1()
                        .min_w_0()
                        .child(Label::new(format!("Open {}", link.display_ref())).truncate())
                        .child(
                            Label::new(if link.host == GITHUB_HOST {
                                "In mustr's review, through T3 Code".to_string()
                            } else {
                                format!("On {}, in mustr's review, through T3 Code", link.host)
                            })
                            .size(LabelSize::Small)
                            .color(Color::Muted)
                            .truncate(),
                        ),
                )
                .child(
                    TextButton::new(
                        "review-page-open-link",
                        if opening { "Opening…" } else { "Open" },
                    )
                    .style(mustr_ui::ButtonStyle::Primary)
                    .disabled(opening)
                    .on_click(cx.listener(|this, _, window, cx| this.open_search_link(window, cx))),
                ),
        )
    }

    fn render_open_error(&self, error: OpenError, cx: &mut Context<Self>) -> impl IntoElement {
        card(cx).child(
            h_flex()
                .gap_3()
                .child(div().flex_1().min_w_0().child(error_line(error.message)))
                .when(!error.url.is_empty(), |this| {
                    let url = error.url.clone();
                    this.child(
                        TextButton::new("review-page-open-on-github", "Open on GitHub")
                            .on_click(move |_, _, cx| cx.open_url(&url)),
                    )
                }),
        )
    }

    /// A section's card: its header (folding it, its count, its query and source, and for a
    /// pull request section the menu that edits it), and its rows.
    fn render_section(
        &self,
        index: Option<usize>,
        name: SharedString,
        count: String,
        query: Option<(SharedString, SectionSource)>,
        rows: Vec<AnyElement>,
        note: Option<AnyElement>,
        cx: &mut Context<Self>,
    ) -> impl IntoElement {
        let collapsed = self.collapsed.contains(name.as_ref());
        let body = match note {
            Some(note) => {
                let mut body = rows;
                body.push(note);
                body
            }
            None if rows.is_empty() => vec![muted_line("Nothing here.").into_any_element()],
            None => rows,
        };
        let id = SharedString::from(format!("review-section-{}", index.unwrap_or(usize::MAX)));
        let toggle = {
            let name = name.to_string();
            cx.listener(move |this, _, _, cx| {
                if !this.collapsed.remove(&name) {
                    this.collapsed.insert(name.clone());
                }
                cx.notify();
            })
        };
        let last = self.sections.len().saturating_sub(1);
        let menu = index.map(|index| {
            let this = cx.weak_entity();
            PopoverMenu::new(SharedString::from(format!("review-section-menu-{index}")))
                .trigger_with_tooltip(
                    IconButton::new(
                        SharedString::from(format!("review-section-menu-button-{index}")),
                        IconName::Ellipsis,
                    )
                    .icon_size(IconSize::Small),
                    Tooltip::text("Edit this section"),
                )
                .anchor(gpui::Anchor::TopRight)
                .menu(move |window, cx| {
                    let this = this.clone();
                    Some(ContextMenu::build(window, cx, move |menu, _, _| {
                        let edit = this.clone();
                        let rename = this.clone();
                        menu.entry("Edit query…", None, move |window, cx| {
                            edit.update(cx, |this, cx| {
                                this.edit_section(Some(index), DialogFocus::Query, window, cx)
                            })
                            .ok();
                        })
                        .entry("Rename…", None, move |window, cx| {
                            rename
                                .update(cx, |this, cx| {
                                    this.edit_section(Some(index), DialogFocus::Name, window, cx)
                                })
                                .ok();
                        })
                        .when(index > 0, |menu| {
                            menu.entry("Move up", None, move |_, cx| {
                                crate::review_sections::move_section(index, -1, cx)
                            })
                        })
                        .when(index < last, |menu| {
                            menu.entry("Move down", None, move |_, cx| {
                                crate::review_sections::move_section(index, 1, cx)
                            })
                        })
                        .separator()
                        .entry("Remove", None, move |_, cx| remove_section(index, cx))
                    }))
                })
        });
        let palette = mustr_ui::palette(cx);
        card(cx)
            .id(id)
            .gap_1()
            .child(
                h_flex()
                    .gap_2()
                    .px_2()
                    .when(!collapsed, |this| this.pb_1())
                    .child(
                        h_flex()
                            .id(SharedString::from(format!(
                                "review-section-toggle-{}",
                                index.unwrap_or(usize::MAX)
                            )))
                            .flex_1()
                            .min_w_0()
                            .gap_1p5()
                            .cursor_pointer()
                            .on_click(toggle)
                            .child(
                                Icon::new(if collapsed {
                                    IconName::ChevronRight
                                } else {
                                    IconName::ChevronDown
                                })
                                .size(IconSize::XSmall)
                                .color(Color::Muted),
                            )
                            .child(Label::new(name).weight(gpui::FontWeight::SEMIBOLD))
                            .child(Label::new(count).size(LabelSize::Small).color(Color::Muted))
                            .when_some(query, |this, (query, source)| {
                                this.child(
                                    div()
                                        .id(SharedString::from(format!(
                                            "review-section-source-{}",
                                            index.unwrap_or(usize::MAX)
                                        )))
                                        .flex_none()
                                        .child(Pill::tinted(source.label(), palette.accent.hsla()))
                                        .tooltip(Tooltip::text(source.description())),
                                )
                                .child(
                                    Label::new(query)
                                        .size(LabelSize::XSmall)
                                        .color(Color::Muted)
                                        .truncate(),
                                )
                            }),
                    )
                    .children(menu),
            )
            .when(!collapsed, |this| {
                this.child(Divider::horizontal()).children(body)
            })
    }

    fn render_pull_request(
        &self,
        section: usize,
        index: usize,
        entry: &PullRequestEntry,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let colors = cx.theme().colors();
        let palette = mustr_ui::palette(cx);
        let is_opening = self.is_opening(&RowKey::PullRequest(section, index));
        // Unique across sections, which can list the same pull request.
        let row = section * 10_000 + index;
        let mut details = vec![entry.display_ref()];
        if let Some(author) = &entry.author {
            details.push(format!("@{}", author.login));
        }
        if let Some(branch) = &entry.head_branch {
            details.push(branch.clone());
        }
        let checks = entry.checks_state.map(|checks| {
            let (icon, color, label) = match checks {
                ChecksState::Passing => (IconName::Check, palette.ok.hsla(), "Checks passing"),
                ChecksState::Failing => (
                    IconName::XCircle,
                    cx.theme().status().error,
                    "Checks failing",
                ),
                ChecksState::Pending => {
                    (IconName::Circle, palette.warning.hsla(), "Checks running")
                }
            };
            div()
                .id(("review-page-checks", row))
                .flex_none()
                .child(
                    Icon::new(icon)
                        .size(IconSize::Small)
                        .color(Color::Custom(color)),
                )
                .tooltip(Tooltip::text(label))
        });
        let decision = entry.review_decision.and_then(|decision| match decision {
            ReviewDecision::Approved => Some(Pill::tinted("Approved", palette.ok.hsla())),
            ReviewDecision::ChangesRequested => {
                Some(Pill::tinted("Changes requested", cx.theme().status().error))
            }
            ReviewDecision::ReviewRequired => None,
        });
        let state = entry.state.and_then(|state| match state {
            PullRequestState::Open => None,
            PullRequestState::Merged => Some(Pill::tinted("Merged", palette.accent.hsla())),
            PullRequestState::Closed => Some(Pill::tinted("Closed", colors.text_muted)),
        });
        let labels = entry
            .labels
            .iter()
            .take(ROW_LABELS)
            .map(|label| {
                Pill::tinted(
                    label.name.clone(),
                    label_color(label.color.as_deref()).unwrap_or(colors.text_muted),
                )
            })
            .collect::<Vec<_>>();
        let icon_color = match entry.state {
            Some(PullRequestState::Merged) => Color::Custom(palette.accent.hsla()),
            Some(PullRequestState::Closed) => Color::Muted,
            _ if entry.is_draft => Color::Muted,
            _ => Color::Custom(palette.ok.hsla()),
        };
        h_flex()
            .id(("review-page-pull-request", row))
            .w_full()
            .gap_3()
            .px_2()
            .py_1p5()
            .rounded(scaled(tokens::CORNER_RADIUS / 2.))
            .cursor_pointer()
            .hover(|style| style.bg(colors.element_hover))
            .on_click(cx.listener(move |this, _, window, cx| {
                this.open_pull_request(section, index, window, cx)
            }))
            .child(
                MustrIcon::PullRequest
                    .icon()
                    .size(IconSize::Small)
                    .color(icon_color),
            )
            .child(
                v_flex()
                    .flex_1()
                    .min_w_0()
                    .child(Label::new(entry.title.clone()).truncate())
                    .child(
                        h_flex()
                            .min_w_0()
                            .gap_1()
                            .child(
                                div().min_w_0().child(
                                    Label::new(details.join(" · "))
                                        .size(LabelSize::Small)
                                        .color(Color::Muted)
                                        .truncate(),
                                ),
                            )
                            .children(labels),
                    ),
            )
            .children(state)
            .when(entry.is_draft, |this| {
                this.child(Pill::tinted("Draft", colors.text_muted))
            })
            .children(decision)
            .when(entry.has_line_counts(), |this| {
                this.child(DiffStat::new(
                    ("review-page-diff-stat", row),
                    entry.additions as usize,
                    entry.deletions as usize,
                ))
            })
            .children(checks)
            .when(is_opening, |this| {
                this.child(
                    Label::new("Opening…")
                        .size(LabelSize::Small)
                        .color(Color::Accent),
                )
            })
            .into_any_element()
    }

    /// The end of a GitHub section that has more: Load more, or why it couldn't.
    fn render_more(
        &self,
        index: usize,
        state: &SectionState,
        cx: &mut Context<Self>,
    ) -> Option<AnyElement> {
        let Loadable::Loaded(list) = &state.list else {
            return None;
        };
        if list.next_cursor.is_none() && state.more_error.is_none() {
            return None;
        }
        Some(
            h_flex()
                .px_2()
                .pt_1()
                .gap_2()
                .when(list.next_cursor.is_some(), |this| {
                    this.child(
                        TextButton::new(
                            SharedString::from(format!("review-section-more-{index}")),
                            if state.loading_more {
                                "Loading…"
                            } else {
                                "Load more"
                            },
                        )
                        .disabled(state.loading_more)
                        .on_click(
                            cx.listener(move |this, _, window, cx| {
                                this.load_more(index, window, cx)
                            }),
                        ),
                    )
                })
                .when_some(state.more_error.clone(), |this, error| {
                    this.child(Label::new(error).size(LabelSize::Small).color(Color::Error))
                })
                .into_any_element(),
        )
    }

    fn render_agent_work(&self, work: &AgentWork, cx: &mut Context<Self>) -> AnyElement {
        let colors = cx.theme().colors();
        let thread = &work.thread;
        let is_opening = self.is_opening(&RowKey::Thread(thread.id.clone()));
        let details = [
            thread.project.as_deref(),
            thread.branch.as_deref(),
            thread.agent.as_deref(),
        ]
        .into_iter()
        .flatten()
        .collect::<Vec<_>>()
        .join(" · ");
        let agent_color = mustr_ui::palette(cx).agent.hsla();
        let open_row = {
            let work = work.clone();
            cx.listener(move |this, _, window, cx| this.open_agent_work(&work, window, cx))
        };
        let open_button = {
            let work = work.clone();
            cx.listener(move |this, _: &gpui::ClickEvent, window, cx| {
                this.open_agent_work(&work, window, cx)
            })
        };
        let (status_color, status) = status(&thread.status);
        h_flex()
            .id(SharedString::from(format!(
                "review-page-agent-work-{}",
                thread.id
            )))
            .w_full()
            .gap_3()
            .px_2()
            .py_1p5()
            .rounded(scaled(tokens::CORNER_RADIUS / 2.))
            .cursor_pointer()
            .hover(|style| style.bg(colors.element_hover))
            .on_click(open_row)
            .child(
                MustrIcon::Agent
                    .icon()
                    .size(IconSize::Small)
                    .color(Color::Custom(agent_color)),
            )
            .child(
                v_flex()
                    .flex_1()
                    .min_w_0()
                    .child(Label::new(thread.title.clone()).truncate())
                    .when(!details.is_empty(), |this| {
                        this.child(
                            Label::new(details)
                                .size(LabelSize::Small)
                                .color(Color::Muted)
                                .truncate(),
                        )
                    }),
            )
            .when_some(work.files_changed, |this, files| {
                this.child(
                    Label::new(if files == 1 {
                        "1 file".to_string()
                    } else {
                        format!("{files} files")
                    })
                    .size(LabelSize::Small)
                    .color(Color::Muted),
                )
            })
            .child(
                h_flex()
                    .flex_none()
                    .gap_1()
                    .child(Indicator::dot().color(status_color))
                    .child(
                        Label::new(status)
                            .size(LabelSize::Small)
                            .color(status_color),
                    ),
            )
            .child(if is_opening {
                Label::new("Opening…")
                    .size(LabelSize::Small)
                    .color(Color::Accent)
                    .into_any_element()
            } else {
                Button::new(
                    SharedString::from(format!("review-page-review-{}", thread.id)),
                    "Review changes",
                )
                .style(ButtonStyle::Filled)
                .label_size(LabelSize::Small)
                .on_click(open_button)
                .into_any_element()
            })
            .into_any_element()
    }
}

fn status(status: &ThreadStatus) -> (Color, String) {
    let color = match status {
        ThreadStatus::NeedsInput => Color::Warning,
        ThreadStatus::Working(_) => Color::Accent,
        ThreadStatus::Failed => Color::Error,
        ThreadStatus::Settled(_) => Color::Muted,
    };
    (color, status.label().to_string())
}

fn card(cx: &App) -> Div {
    let colors = cx.theme().colors();
    v_flex()
        .w_full()
        .p_3()
        .rounded(scaled(tokens::CORNER_RADIUS))
        .border(tokens::BORDER_WIDTH)
        .border_color(colors.border)
        .bg(colors.surface_background)
}

fn muted_line(text: impl Into<SharedString>) -> impl IntoElement {
    div().px_2().py_1p5().child(
        Label::new(text.into())
            .size(LabelSize::Small)
            .color(Color::Muted),
    )
}

fn error_line(text: SharedString) -> impl IntoElement {
    div()
        .px_2()
        .py_1p5()
        .child(Label::new(text).size(LabelSize::Small).color(Color::Error))
}

impl EventEmitter<ItemEvent> for ReviewPage {}

impl Focusable for ReviewPage {
    fn focus_handle(&self, _: &App) -> FocusHandle {
        self.focus_handle.clone()
    }
}

impl Item for ReviewPage {
    type Event = ItemEvent;

    fn tab_content_text(&self, _: usize, _: &App) -> SharedString {
        "Review".into()
    }

    fn show_toolbar(&self) -> bool {
        false
    }

    fn to_item_events(event: &Self::Event, f: &mut dyn FnMut(ItemEvent)) {
        f(*event)
    }
}

impl Render for ReviewPage {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let search = self.search.read(cx).text(cx);
        let link = parse_link(&search);
        // A link is for opening, not for narrowing the lists.
        let filter = if link.is_some() { "" } else { search.as_str() };
        let connected = self.connection.read(cx).session().is_some();
        let mut cards = Vec::new();
        for (index, section) in self.sections.clone().into_iter().enumerate() {
            let Some(state) = self.states.get(index) else {
                continue;
            };
            let source = state.source;
            let (rows, note, count) = match &state.list {
                Loadable::Loaded(list) => {
                    let rows = rows_of(source, &section.query, &list.list, filter)
                        .into_iter()
                        .map(|row| {
                            self.render_pull_request(index, row, &list.list.entries[row], cx)
                        })
                        .collect::<Vec<_>>();
                    let mut notes = list
                        .list
                        .errors
                        .iter()
                        .map(|error| format!("Couldn't list {error}"))
                        .collect::<Vec<_>>();
                    if rows.is_empty() {
                        notes.insert(
                            0,
                            if filter.trim().is_empty() {
                                "No pull requests.".to_string()
                            } else {
                                "Nothing matches.".to_string()
                            },
                        );
                    }
                    let count = match list.total {
                        Some(total) if total > rows.len() as u64 && filter.trim().is_empty() => {
                            format!("{} of {}", rows.len(), count_label(total))
                        }
                        _ => rows.len().to_string(),
                    };
                    let note = (!notes.is_empty())
                        .then(|| muted_line(notes.join(" ")).into_any_element())
                        .or_else(|| self.render_more(index, state, cx));
                    (rows, note, count)
                }
                Loadable::Failed(error) => (
                    Vec::new(),
                    Some(error_line(error.clone()).into_any_element()),
                    "0".to_string(),
                ),
                Loadable::NotLoaded => (
                    Vec::new(),
                    Some(
                        muted_line(if source == SectionSource::T3Projects && !connected {
                            "Waiting for the connection to T3 Code…"
                        } else {
                            "Loading pull requests…"
                        })
                        .into_any_element(),
                    ),
                    "0".to_string(),
                ),
                Loadable::Loading => (
                    Vec::new(),
                    Some(muted_line("Loading pull requests…").into_any_element()),
                    "0".to_string(),
                ),
            };
            cards.push(
                self.render_section(
                    Some(index),
                    section.name.clone().into(),
                    count,
                    Some((section.query.clone().into(), source)),
                    rows,
                    note,
                    cx,
                )
                .into_any_element(),
            );
        }
        let add_section = h_flex().child(
            Button::new("review-page-add-section", "Add section")
                .start_icon(Icon::new(IconName::Plus).size(IconSize::Small))
                .style(ButtonStyle::Subtle)
                .label_size(LabelSize::Small)
                .on_click(cx.listener(|this, _, window, cx| {
                    this.edit_section(None, DialogFocus::Name, window, cx)
                })),
        );
        let (rows, note) = match &self.agent_work {
            Loadable::Loaded(work) => {
                let rows = work
                    .iter()
                    .filter(|work| agent_work_matches(work, filter))
                    .map(|work| self.render_agent_work(work, cx))
                    .collect::<Vec<_>>();
                let note = rows.is_empty().then(|| {
                    muted_line(if filter.trim().is_empty() {
                        "No agent work is waiting for review."
                    } else {
                        "Nothing matches."
                    })
                    .into_any_element()
                });
                (rows, note)
            }
            Loadable::Failed(error) => (
                Vec::new(),
                Some(error_line(error.clone()).into_any_element()),
            ),
            _ => (
                Vec::new(),
                Some(muted_line("Looking for agent work to review…").into_any_element()),
            ),
        };
        let count = rows.len().to_string();
        let code = self
            .render_section(None, CODE_SECTION.into(), count, None, rows, note, cx)
            .into_any_element();
        v_flex()
            .key_context("MustrReviewPage")
            .track_focus(&self.focus_handle)
            .on_action(cx.listener(Self::confirm))
            .size_full()
            .bg(cx.theme().colors().editor_background)
            .child(
                v_flex()
                    .id("review-page-scroll")
                    .size_full()
                    .overflow_y_scroll()
                    .child(
                        v_flex()
                            .w_full()
                            .max_w(scaled(px(CONTENT_MAX_WIDTH)))
                            .mx_auto()
                            .px_6()
                            .py_6()
                            .gap_4()
                            .child(
                                v_flex()
                                    .gap_0p5()
                                    .child(Headline::new("Review").size(HeadlineSize::Large))
                                    .child(
                                        Label::new(
                                            "Pull requests and agent work waiting for review.",
                                        )
                                        .color(Color::Muted),
                                    ),
                            )
                            .child(self.render_search(cx))
                            .when_some(link, |this, link| this.child(self.render_link(&link, cx)))
                            .when_some(self.open_error.clone(), |this, error| {
                                this.child(self.render_open_error(error, cx))
                            })
                            .children(cards)
                            .child(add_section)
                            .child(code),
                    ),
            )
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        agent_threads::AgentThread,
        github_search::{RecordingGh, search_fixture},
        inbox::parse_pull_request_list,
    };
    use gpui::{TestAppContext, VisualTestContext};
    use pretty_assertions::assert_eq;
    use serde_json::json;
    use settings::SettingsStore;

    fn pull_requests() -> PullRequestList {
        let entry = |number: u64, title: &str, author: &str, requested: bool, updated: &str| {
            json!({
                "projectId": "p", "host": "github.com", "repository": "acme/api",
                "number": number, "title": title, "author": { "login": author },
                "headBranch": format!("branch-{number}"), "viewerReviewRequested": requested,
                "updatedAt": updated, "checksState": "failing", "reviewDecision": "approved",
                "state": "open", "labels": [{ "name": "bug", "color": "d73a4a" }],
            })
        };
        parse_pull_request_list(json!({
            "viewers": { "github.com": "msetaro" },
            "entries": [
                entry(1, "Retry backoff", "octocat", true, "2026-10-27T00:00:00Z"),
                entry(2, "My cleanup", "msetaro", false, "2026-10-28T00:00:00Z"),
                entry(3, "Docs", "hubot", false, "2026-10-26T00:00:00Z"),
                entry(4, "My other fix", "MSetaro", true, "2026-10-29T00:00:00Z"),
            ],
        }))
    }

    fn agent_work(id: &str, title: &str, branch: &str) -> AgentWork {
        AgentWork {
            thread: AgentThread {
                id: id.to_string(),
                title: title.to_string(),
                project: Some("mustr".to_string()),
                branch: Some(branch.to_string()),
                worktree_path: None,
                project_root: None,
                status: ThreadStatus::Settled("completed".to_string()),
                agent: Some("Claude".to_string()),
                latest_run_completed_at: None,
                updated_at: "2026-10-29T00:00:00Z".to_string(),
                is_subagent: false,
            },
            files_changed: Some(3),
        }
    }

    /// The app, with `sections` and a `gh` that answers from fixtures.
    fn setup(cx: &mut TestAppContext, sections: Vec<QuerySection>) -> Arc<RecordingGh> {
        let gh = Arc::new(RecordingGh::default());
        cx.update(|cx| {
            let settings_store = SettingsStore::test(cx);
            cx.set_global(settings_store);
            theme_settings::init(theme::LoadThemes::JustBase, cx);
            crate::t3_connection::init_for_test(crate::t3_engine::T3Mode::Installed, cx);
            GitHubSearch::set_global(Arc::new(GitHubSearch::new(gh.clone())), cx);
            cx.set_global(ReviewSettings { sections });
        });
        gh
    }

    fn t3_sections() -> Vec<QuerySection> {
        vec![
            QuerySection::new("Review requested", "is:open is:pr review-requested:@me")
                .projects_only(),
            QuerySection::new("Yours", "is:open is:pr author:@me").projects_only(),
            QuerySection::new(
                "Other open pull requests",
                "is:open is:pr -author:@me -review-requested:@me",
            ),
        ]
    }

    /// T3 sections list each section's query's pull requests, newest first (T3's list for that
    /// section, narrowed by what T3 doesn't filter), then the agent work; the search narrows
    /// them all.
    #[gpui::test]
    async fn the_page_has_the_sections_and_the_code(cx: &mut TestAppContext) {
        let gh = setup(cx, t3_sections());
        let (page, cx) =
            cx.add_window_view(|window, cx| ReviewPage::new(WeakEntity::new_invalid(), window, cx));
        assert_eq!(
            page.read_with(cx, |page, _| page.sources()),
            vec![SectionSource::T3Projects; 3]
        );
        // Each section gets T3's list for its query; here the same list stands in for each.
        page.update(cx, |page, cx| {
            page.set_lists(
                vec![pull_requests(), pull_requests(), pull_requests()],
                vec![
                    agent_work("thread-1", "Retry the flaky test", "t3/flaky"),
                    agent_work("thread-2", "Write the docs", "t3/docs"),
                ],
                cx,
            )
        });
        let sections =
            |cx: &mut VisualTestContext| page.read_with(cx, |page, cx| page.sections(cx));
        assert_eq!(
            sections(cx),
            vec![
                (
                    "Review requested".to_string(),
                    vec!["acme/api#4".to_string(), "acme/api#1".to_string()]
                ),
                (
                    "Yours".to_string(),
                    vec!["acme/api#4".to_string(), "acme/api#2".to_string()]
                ),
                (
                    "Other open pull requests".to_string(),
                    vec!["acme/api#3".to_string()]
                ),
                (
                    CODE_SECTION.to_string(),
                    vec!["thread-1".to_string(), "thread-2".to_string()]
                ),
            ]
        );

        let search = page.read_with(cx, |page, _| page.search_editor());
        search.update_in(cx, |search, window, cx| {
            search.set_text("retry", window, cx)
        });
        assert_eq!(
            sections(cx),
            vec![
                (
                    "Review requested".to_string(),
                    vec!["acme/api#1".to_string()]
                ),
                ("Yours".to_string(), Vec::new()),
                ("Other open pull requests".to_string(), Vec::new()),
                (CODE_SECTION.to_string(), vec!["thread-1".to_string()]),
            ]
        );
        // It draws, with checks, reviews and labels. Nothing asked GitHub.
        cx.run_until_parked();
        assert!(gh.calls.lock().is_empty());
    }

    /// The sections are mustr's setting: a new one shows, and lists through GitHub.
    #[gpui::test]
    async fn the_sections_follow_the_setting(cx: &mut TestAppContext) {
        let gh = setup(cx, Vec::new());
        let (page, cx) =
            cx.add_window_view(|window, cx| ReviewPage::new(WeakEntity::new_invalid(), window, cx));
        gh.answer_json(search_fixture(&[("acme/api", 5, "Draft")], None));
        cx.update(|_, cx| {
            cx.set_global(ReviewSettings {
                sections: vec![QuerySection::new("Drafts", "is:open is:draft author:@me")],
            })
        });
        cx.run_until_parked();
        assert_eq!(
            page.read_with(cx, |page, cx| page.sections(cx)),
            vec![
                ("Drafts".to_string(), vec!["acme/api#5".to_string()]),
                (CODE_SECTION.to_string(), Vec::new())
            ]
        );
        assert_eq!(
            gh.queries(),
            vec!["is:open is:draft author:@me is:pr archived:false sort:updated-desc"]
        );
    }

    /// The default sections search GitHub, in GitHub's order, without waiting for T3; Load
    /// more brings the next page; the cache keeps the page from asking GitHub again until the
    /// lists are stale or you refresh.
    #[gpui::test]
    async fn the_default_sections_search_github(cx: &mut TestAppContext) {
        let gh = setup(cx, crate::review_sections::default_sections());
        gh.answer_query_json(
            "review-requested:@me",
            search_fixture(
                &[("zed-industries/zed", 9, "Fix"), ("acme/api", 12, "Retry")],
                Some("cursor-2"),
            ),
        );
        gh.answer_query_json(
            "is:open is:pr author:@me",
            search_fixture(&[("acme/web", 3, "Mine")], None),
        );
        gh.answer_query_json(
            "involves:@me",
            search_fixture(&[("rust-lang/rust", 1, "Others'")], None),
        );
        let (page, cx) =
            cx.add_window_view(|window, cx| ReviewPage::new(WeakEntity::new_invalid(), window, cx));
        cx.run_until_parked();
        assert_eq!(
            page.read_with(cx, |page, _| page.sources()),
            vec![SectionSource::GitHub; 3]
        );
        let mut queries = gh.queries();
        queries.sort();
        assert_eq!(
            queries,
            vec![
                "is:open is:pr author:@me archived:false sort:updated-desc",
                "is:open is:pr involves:@me -author:@me archived:false sort:updated-desc",
                "is:open is:pr review-requested:@me archived:false sort:updated-desc",
            ]
        );
        let names_and_rows =
            |cx: &mut VisualTestContext| page.read_with(cx, |page, cx| page.sections(cx));
        assert_eq!(
            names_and_rows(cx),
            vec![
                (
                    "Review requested".to_string(),
                    vec![
                        "zed-industries/zed#9".to_string(),
                        "acme/api#12".to_string()
                    ]
                ),
                ("Yours".to_string(), vec!["acme/web#3".to_string()]),
                ("Involved".to_string(), vec!["rust-lang/rust#1".to_string()]),
                (CODE_SECTION.to_string(), Vec::new()),
            ]
        );

        // The next page, after the cursor; one already shown isn't shown twice.
        gh.answer_query_json(
            "review-requested:@me",
            search_fixture(
                &[("acme/api", 12, "Retry"), ("acme/api", 30, "Older")],
                None,
            ),
        );
        page.update_in(cx, |page, window, cx| {
            page.load_more_for_test(0, window, cx)
        });
        cx.run_until_parked();
        assert!(gh.calls.lock()[3].contains(&"after=cursor-2".to_string()));
        assert_eq!(
            names_and_rows(cx)[0].1,
            vec!["zed-industries/zed#9", "acme/api#12", "acme/api#30"]
        );

        // Shown again while its lists are fresh, the page doesn't ask GitHub. Refresh asks for
        // every list, the one you loaded more of too.
        page.update_in(cx, |page, window, cx| {
            page.refresh_stale(STALE_AFTER, window, cx)
        });
        cx.run_until_parked();
        assert_eq!(gh.calls.lock().len(), 4);
        for query in [
            "review-requested:@me",
            "is:open is:pr author:@me",
            "involves:@me",
        ] {
            gh.answer_query_json(query, search_fixture(&[("acme/api", 99, "New")], None));
        }
        page.update_in(cx, |page, window, cx| page.reload(window, cx));
        cx.run_until_parked();
        assert_eq!(gh.calls.lock().len(), 7);
        assert_eq!(names_and_rows(cx)[0].1, vec!["acme/api#99"]);
    }

    /// GitHub's refusals and a missing or signed-out `gh` show in their sections.
    #[gpui::test]
    async fn search_errors_show_in_their_sections(cx: &mut TestAppContext) {
        let gh = setup(
            cx,
            vec![
                QuerySection::new("Signed out", "is:open author:@me"),
                QuerySection::new("No gh", "is:open involves:@me"),
                QuerySection::new("Bad", "is:open author:@me created:>someday"),
            ],
        );
        gh.answer_query(
            "is:open author:@me is:pr",
            Ok(crate::github_search::GhOutput {
                code: Some(4),
                stdout: Vec::new(),
                stderr: b"To get started with GitHub CLI, please run:  gh auth login".to_vec(),
            }),
        );
        gh.answer_query(
            "involves:@me",
            Err(std::io::Error::from(std::io::ErrorKind::NotFound)),
        );
        gh.answer_query_json("someday", search_fixture(&[], None));
        gh.answer_query(
            "someday",
            Ok(crate::github_search::GhOutput {
                code: Some(1),
                stdout: br#"{"message":"Validation Failed","errors":[{"message":"\"someday\" is not a recognized date/time format."}]}"#.to_vec(),
                stderr: b"gh: Validation Failed (HTTP 422)".to_vec(),
            }),
        );
        let (page, cx) =
            cx.add_window_view(|window, cx| ReviewPage::new(WeakEntity::new_invalid(), window, cx));
        cx.run_until_parked();
        let errors = page.read_with(cx, |page, _| {
            (0..3)
                .map(|index| page.section_error(index).unwrap_or_default().to_string())
                .collect::<Vec<_>>()
        });
        assert_eq!(
            errors,
            vec![
                "The GitHub CLI isn't signed in. Sign in with gh auth login in Terminal."
                    .to_string(),
                crate::github_search::SearchError::CliMissing.message(),
                "GitHub: \"someday\" is not a recognized date/time format.".to_string(),
            ]
        );
    }

    /// A row from GitHub opens through T3; without T3, the page says so and offers GitHub.
    #[gpui::test]
    async fn opening_without_t3_offers_github(cx: &mut TestAppContext) {
        let gh = setup(cx, vec![QuerySection::new("Yours", "is:open author:@me")]);
        gh.answer_json(search_fixture(
            &[("zed-industries/zed", 41234, "Fix")],
            None,
        ));
        let (page, cx) =
            cx.add_window_view(|window, cx| ReviewPage::new(WeakEntity::new_invalid(), window, cx));
        cx.run_until_parked();
        page.update_in(cx, |page, window, cx| {
            page.open_row_for_test(0, 0, window, cx)
        });
        cx.run_until_parked();
        let error = page.read_with(cx, |page, _| page.open_error().cloned());
        assert_eq!(
            error,
            Some(OpenError {
                message: "mustr isn't connected to T3 Code, which opens pull requests.".into(),
                url: "https://github.com/zed-industries/zed/pull/41234".to_string(),
            })
        );
    }
}

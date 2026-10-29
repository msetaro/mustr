//! The Review area's page: what there is to review. Pull requests in sections, each a named
//! GitHub-style query (`review_sections.rs`; by default review requested from you, yours, and
//! the projects' other open ones), listed through T3's `pullRequests.list` with their checks and
//! reviews; and the agent work waiting for review (threads whose runs finished with changes you
//! haven't reviewed, as the Inbox finds them). A search box narrows them all. A row opens the
//! pull request's review, or the agent review.

use crate::{
    agent_review::{AgentReviewTarget, open_agent_review},
    home::{AgentWork, load_threads},
    inbox::{ChecksState, PullRequestEntry, PullRequestList, ReviewDecision, ThreadStatus},
    pull_request_review,
    review_sections::{
        DialogFocus, PullRequestSource as _, QuerySection, ReviewSettings, SectionDialog,
        T3PullRequests, parse_query, remove_section,
    },
    t3_connection::{T3Connection, T3Session},
};
use collections::HashSet;
use editor::{Editor, EditorEvent};
use gpui::{
    AnyElement, App, Entity, EventEmitter, FocusHandle, Focusable, Subscription, Task, WeakEntity,
};
use mustr_ui::{MustrIcon, Pill, tokens, tokens::scaled};
use review::ReviewScope;
use std::sync::Arc;
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

/// The pull requests of `list` a section's query keeps, newest first, after the search box's
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

pub struct ReviewPage {
    focus_handle: FocusHandle,
    workspace: WeakEntity<Workspace>,
    connection: Entity<T3Connection>,
    search: Entity<Editor>,
    /// The connection generation the lists were loaded from.
    loaded_generation: Option<usize>,
    /// The pull request sections, and what each lists.
    sections: Vec<QuerySection>,
    lists: Vec<Loadable<PullRequestList>>,
    /// The sections folded away, by name.
    collapsed: HashSet<String>,
    agent_work: Loadable<Vec<AgentWork>>,
    opening: Option<(RowKey, Task<()>)>,
    open_error: Option<SharedString>,
    _pull_request_tasks: Vec<Task<()>>,
    _agent_work_task: Option<Task<()>>,
    _subscriptions: Vec<Subscription>,
}

/// Shows the Review page, making it the first time.
pub fn show(workspace: &mut Workspace, window: &mut Window, cx: &mut Context<Workspace>) {
    let existing = workspace
        .panes()
        .iter()
        .find_map(|pane| pane.read(cx).items_of_type::<ReviewPage>().next());
    if let Some(existing) = existing {
        workspace.activate_item(&existing, true, true, window, cx);
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
            editor.set_placeholder_text("Search pull requests and agent work", window, cx);
            editor
        });
        let subscriptions = vec![
            cx.observe_in(&connection, window, |this, _, window, cx| {
                this.connection_changed(window, cx)
            }),
            cx.subscribe(&search, |_, _, event: &EditorEvent, cx| {
                if let EditorEvent::BufferEdited = event {
                    cx.notify();
                }
            }),
            cx.observe_global_in::<ReviewSettings>(window, |this, window, cx| {
                this.sections_changed(window, cx)
            }),
        ];
        let sections = ReviewSettings::get(cx).sections;
        let mut this = Self {
            focus_handle: cx.focus_handle(),
            workspace,
            connection,
            search,
            loaded_generation: None,
            lists: sections.iter().map(|_| Loadable::NotLoaded).collect(),
            sections,
            collapsed: HashSet::default(),
            agent_work: Loadable::NotLoaded,
            opening: None,
            open_error: None,
            _pull_request_tasks: Vec::new(),
            _agent_work_task: None,
            _subscriptions: subscriptions,
        };
        this.connection_changed(window, cx);
        this
    }

    fn connection_changed(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let connection = self.connection.read(cx);
        let generation = connection.generation();
        if let Some(session) = connection.session()
            && self.loaded_generation != Some(generation)
        {
            self.loaded_generation = Some(generation);
            self.load_pull_requests(session.clone(), window, cx);
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
        self.sections = sections;
        self.lists = self.sections.iter().map(|_| Loadable::NotLoaded).collect();
        self.opening = None;
        if let Some(session) = self.connection.read(cx).session() {
            self.load_pull_requests(session, window, cx);
        }
        cx.notify();
    }

    /// Lists the pull requests and the agent work again (the rail's Review button, on the page).
    pub fn reload(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if let Some(session) = self.connection.read(cx).session() {
            self.load_pull_requests(session.clone(), window, cx);
            self.load_agent_work(session, window, cx);
        }
        cx.notify();
    }

    fn load_pull_requests(
        &mut self,
        session: Arc<T3Session>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.open_error = None;
        let source = Arc::new(T3PullRequests(session));
        self.lists = self.sections.iter().map(|_| Loadable::Loading).collect();
        self._pull_request_tasks = self
            .sections
            .iter()
            .enumerate()
            .map(|(index, section)| {
                let list = source.list(&parse_query(&section.query));
                let name = section.name.clone();
                cx.spawn_in(window, async move |this, cx| {
                    let result = list.await;
                    this.update(cx, |this, cx| {
                        let loaded = match result {
                            Ok(list) => Loadable::Loaded(list),
                            Err(error) => {
                                log::error!("couldn't list the pull requests of {name}: {error:#}");
                                Loadable::Failed(format!("{error:#}").into())
                            }
                        };
                        if let Some(slot) = this.lists.get_mut(index) {
                            *slot = loaded;
                        }
                        cx.notify();
                    })
                    .ok();
                })
            })
            .collect();
    }

    fn load_agent_work(
        &mut self,
        session: Arc<T3Session>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.agent_work = Loadable::Loading;
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
        self.lists = pull_requests.into_iter().map(Loadable::Loaded).collect();
        self.agent_work = Loadable::Loaded(agent_work);
        cx.notify();
    }

    #[cfg(test)]
    pub fn search_editor(&self) -> Entity<Editor> {
        self.search.clone()
    }

    /// The sections as they show (for tests), by name, with the rows each lists: pull requests by their
    /// reference, agent work by its thread's id.
    #[cfg(test)]
    pub fn sections(&self, cx: &App) -> Vec<(String, Vec<String>)> {
        let search = self.search.read(cx).text(cx);
        let mut sections = self
            .sections
            .iter()
            .zip(&self.lists)
            .map(|(section, list)| {
                let rows = match list {
                    Loadable::Loaded(list) => section_rows(&section.query, list, &search)
                        .into_iter()
                        .map(|index| list.entries[index].display_ref())
                        .collect(),
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
        let Some(Loadable::Loaded(list)) = self.lists.get(section) else {
            return;
        };
        let Some(entry) = list.entries.get(index).cloned() else {
            return;
        };
        self.open_error = None;
        let display_ref = entry.display_ref();
        let open =
            pull_request_review::open_review(self.workspace.clone(), entry.reference(), window, cx);
        let task = cx.spawn(async move |this, cx| {
            let result = open.await;
            this.update(cx, |this, cx| {
                this.opening = None;
                if let Err(error) = result {
                    log::error!("couldn't open {display_ref}: {error:#}");
                    this.open_error = Some(format!("{display_ref}: {error:#}").into());
                }
                cx.notify();
            })
            .ok();
        });
        self.opening = Some((RowKey::PullRequest(section, index), task));
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
                    this.open_error = Some(format!("{title}: {error:#}").into());
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

    /// A section's card: its header (folding it, its count, and for a pull request section the
    /// menu that edits it), and its rows.
    fn render_section(
        &self,
        index: Option<usize>,
        name: SharedString,
        query: Option<SharedString>,
        rows: Vec<AnyElement>,
        note: Option<AnyElement>,
        cx: &mut Context<Self>,
    ) -> impl IntoElement {
        let count = rows.len();
        let collapsed = self.collapsed.contains(name.as_ref());
        let body = match note {
            Some(note) => vec![note],
            None if rows.is_empty() => vec![muted_line("Nothing here.").into_any_element()],
            None => rows,
        };
        let id = SharedString::from(format!(
            "review-section-{}",
            index.unwrap_or(usize::MAX)
        ));
        let toggle = {
            let name = name.to_string();
            cx.listener(move |this, _, _, cx| {
                if !this.collapsed.remove(&name) {
                    this.collapsed.insert(name.clone());
                }
                cx.notify();
            })
        };
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
                        .separator()
                        .entry("Remove", None, move |_, cx| remove_section(index, cx))
                    }))
                })
        });
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
                            .child(
                                Label::new(count.to_string())
                                    .size(LabelSize::Small)
                                    .color(Color::Muted),
                            )
                            .when_some(query, |this, query| {
                                this.child(
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
                    .color(if entry.is_draft {
                        Color::Muted
                    } else {
                        Color::Custom(palette.ok.hsla())
                    }),
            )
            .child(
                v_flex()
                    .flex_1()
                    .min_w_0()
                    .child(Label::new(entry.title.clone()).truncate())
                    .child(
                        Label::new(details.join(" · "))
                            .size(LabelSize::Small)
                            .color(Color::Muted)
                            .truncate(),
                    ),
            )
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
        let mut cards = Vec::new();
        for (index, section) in self.sections.clone().into_iter().enumerate() {
            let (rows, note) = match self.lists.get(index) {
                Some(Loadable::Loaded(list)) => {
                    let rows = section_rows(&section.query, list, &search)
                        .into_iter()
                        .map(|row| self.render_pull_request(index, row, &list.entries[row], cx))
                        .collect::<Vec<_>>();
                    let mut notes = list
                        .errors
                        .iter()
                        .map(|error| format!("Couldn't list {error}"))
                        .collect::<Vec<_>>();
                    let note = if rows.is_empty() {
                        notes.insert(
                            0,
                            if search.trim().is_empty() {
                                "No pull requests.".to_string()
                            } else {
                                "Nothing matches.".to_string()
                            },
                        );
                        Some(muted_line(notes.join(" ")).into_any_element())
                    } else {
                        None
                    };
                    (rows, note)
                }
                Some(Loadable::Failed(error)) => (
                    Vec::new(),
                    Some(error_line(error.clone()).into_any_element()),
                ),
                Some(Loadable::NotLoaded) | None => (
                    Vec::new(),
                    Some(muted_line("Waiting for the connection to T3 Code…").into_any_element()),
                ),
                Some(Loadable::Loading) => (
                    Vec::new(),
                    Some(muted_line("Loading pull requests…").into_any_element()),
                ),
            };
            cards.push(
                self.render_section(
                    Some(index),
                    section.name.clone().into(),
                    Some(section.query.clone().into()),
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
                    .filter(|work| agent_work_matches(work, &search))
                    .map(|work| self.render_agent_work(work, cx))
                    .collect::<Vec<_>>();
                let note = rows.is_empty().then(|| {
                    muted_line(if search.trim().is_empty() {
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
        let code = self
            .render_section(None, CODE_SECTION.into(), None, rows, note, cx)
            .into_any_element();
        v_flex()
            .key_context("MustrReviewPage")
            .track_focus(&self.focus_handle)
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
                            .when_some(self.open_error.clone(), |this, error| {
                                this.child(card(cx).child(error_line(error)))
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
    use crate::{agent_threads::AgentThread, inbox::parse_pull_request_list};
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

    /// The page lists each section's query's pull requests, newest first (T3's list for that
    /// section, narrowed by what T3 doesn't filter), then the agent work; the search narrows
    /// them all.
    #[gpui::test]
    async fn the_page_has_the_sections_and_the_code(cx: &mut TestAppContext) {
        cx.update(|cx| {
            let settings_store = SettingsStore::test(cx);
            cx.set_global(settings_store);
            theme_settings::init(theme::LoadThemes::JustBase, cx);
            crate::t3_connection::init_for_test(crate::t3_engine::T3Mode::Installed, cx);
        });
        let (page, cx) =
            cx.add_window_view(|window, cx| ReviewPage::new(WeakEntity::new_invalid(), window, cx));
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
        // It draws, with checks and reviews.
        cx.run_until_parked();
    }

    /// The sections are mustr's setting: a new one shows, and lists through T3.
    #[gpui::test]
    async fn the_sections_follow_the_setting(cx: &mut TestAppContext) {
        cx.update(|cx| {
            let settings_store = SettingsStore::test(cx);
            cx.set_global(settings_store);
            theme_settings::init(theme::LoadThemes::JustBase, cx);
            crate::t3_connection::init_for_test(crate::t3_engine::T3Mode::Installed, cx);
        });
        let (page, cx) =
            cx.add_window_view(|window, cx| ReviewPage::new(WeakEntity::new_invalid(), window, cx));
        cx.update(|_, cx| {
            cx.set_global(ReviewSettings {
                sections: vec![QuerySection::new("Drafts", "is:open is:draft author:@me")],
            })
        });
        cx.run_until_parked();
        let names = page.read_with(cx, |page, cx| {
            page.sections(cx)
                .into_iter()
                .map(|(name, _)| name)
                .collect::<Vec<_>>()
        });
        assert_eq!(names, vec!["Drafts".to_string(), CODE_SECTION.to_string()]);
    }
}

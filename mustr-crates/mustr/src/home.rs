use crate::{
    inbox::{self, PullRequestEntry, PullRequestList, ThreadRow, ThreadStatus},
    nav_panel::NavPanel,
    pull_request_review,
    review_backend::{self, DryRunRpc, T3ReviewBackend},
    review_drafts::{self, DraftStore},
    t3_connection::{self, ConnectionStatus, T3Connection, T3Session},
};
use anyhow::{Context as _, anyhow};
use editor::Editor;
use gpui::{
    App, Entity, EventEmitter, FocusHandle, Focusable, Global, Subscription, Task, WeakEntity,
};
use mustr_ui::tokens;
use review::{ReviewContents, ReviewView};
use std::sync::Arc;
use ui::{Button, ButtonStyle, DiffStat, Divider, Indicator, prelude::*};
use workspace::{
    Toast, Workspace,
    item::{Item, ItemEvent},
    notifications::NotificationId,
};

const RECENT_THREADS: usize = 20;
const CONTENT_MAX_WIDTH: f32 = 880.;

/// Startup options that change what the home view does on its own.
#[derive(Clone, Copy, Debug, Default)]
pub struct HomeOptions {
    /// Opens the first open pull request once the list loads. For smoke tests.
    pub open_first_pull_request: bool,
}

impl Global for HomeOptions {}

enum Loadable<T> {
    NotLoaded,
    Loading,
    Loaded(T),
    Failed(SharedString),
}

struct OpeningPullRequest {
    index: usize,
    _task: Task<()>,
}

/// mustr's home: the T3 connection, open pull requests, and recent agent threads.
pub struct HomeView {
    focus_handle: FocusHandle,
    workspace: WeakEntity<Workspace>,
    connection: Entity<T3Connection>,
    pairing_editor: Entity<Editor>,
    pairing_error: Option<SharedString>,
    /// The connection generation the lists below were loaded from.
    loaded_generation: Option<usize>,
    pull_requests: Loadable<PullRequestList>,
    threads: Loadable<Vec<ThreadRow>>,
    opening: Option<OpeningPullRequest>,
    open_error: Option<SharedString>,
    open_first_pull_request: bool,
    _load_tasks: Vec<Task<()>>,
    _subscription: Subscription,
}

impl HomeView {
    pub fn new(
        workspace: WeakEntity<Workspace>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        let connection = T3Connection::global(cx);
        let pairing_editor = cx.new(|cx| {
            let mut editor = Editor::single_line(window, cx);
            editor.set_placeholder_text("Pairing credential or pairing link", window, cx);
            editor.set_masked(true, cx);
            editor
        });
        let subscription = cx.observe_in(&connection, window, |this, _, window, cx| {
            this.connection_changed(window, cx)
        });
        let mut this = Self {
            focus_handle: cx.focus_handle(),
            workspace,
            connection,
            pairing_editor,
            pairing_error: None,
            loaded_generation: None,
            pull_requests: Loadable::NotLoaded,
            threads: Loadable::NotLoaded,
            opening: None,
            open_error: None,
            open_first_pull_request: cx
                .try_global::<HomeOptions>()
                .is_some_and(|options| options.open_first_pull_request),
            _load_tasks: Vec::new(),
            _subscription: subscription,
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
            self.load(session, window, cx);
        }
        cx.notify();
    }

    fn load(&mut self, session: Arc<T3Session>, window: &mut Window, cx: &mut Context<Self>) {
        self.pull_requests = Loadable::Loading;
        self.threads = Loadable::Loading;
        self.opening = None;

        let pull_requests = cx.spawn_in(window, {
            let session = session.clone();
            async move |this, cx| {
                let result = cx
                    .background_spawn(async move { session.open_pull_requests().await })
                    .await;
                this.update_in(cx, |this, window, cx| {
                    this.pull_requests = match result {
                        Ok(list) => Loadable::Loaded(list),
                        Err(error) => {
                            log::error!("couldn't list pull requests: {error:#}");
                            Loadable::Failed(format!("{error:#}").into())
                        }
                    };
                    if this.open_first_pull_request
                        && let Loadable::Loaded(list) = &this.pull_requests
                        && !list.entries.is_empty()
                    {
                        this.open_first_pull_request = false;
                        this.open_pull_request(0, window, cx);
                    }
                    cx.notify();
                })
                .ok();
            }
        });

        let threads = cx.spawn_in(window, async move |this, cx| {
            let result = cx
                .background_spawn(async move { session.recent_threads(RECENT_THREADS).await })
                .await;
            this.update(cx, |this, cx| {
                this.threads = match result {
                    Ok(threads) => Loadable::Loaded(threads),
                    Err(error) => {
                        log::error!("couldn't load agent threads: {error:#}");
                        Loadable::Failed(format!("{error:#}").into())
                    }
                };
                cx.notify();
            })
            .ok();
        });

        self._load_tasks = vec![pull_requests, threads];
    }

    fn open_pull_request(&mut self, index: usize, window: &mut Window, cx: &mut Context<Self>) {
        let Loadable::Loaded(list) = &self.pull_requests else {
            return;
        };
        let Some(entry) = list.entries.get(index).cloned() else {
            return;
        };
        let Some(session) = self.connection.read(cx).session() else {
            self.open_error = Some("mustr isn't connected to T3".into());
            cx.notify();
            return;
        };
        self.open_error = None;
        let workspace = self.workspace.clone();
        let current_rpc = self.connection.read(cx).current_rpc();
        let task = cx.spawn_in(window, async move |this, cx| {
            let display_ref = entry.display_ref();
            let result = async {
                let reference = entry.reference();
                let rpc = current_rpc;
                let drafts = DraftStore::new(&review_drafts::drafts_dir(), &reference);
                // The files, the data around them (header, threads, viewed marks), and the
                // saved drafts load side by side; the files take longest.
                let files = cx.background_spawn({
                    let session = session.clone();
                    let reference = reference.clone();
                    async move { session.pull_request_files(&reference).await }
                });
                let review_data = cx.background_spawn({
                    let rpc = rpc.clone();
                    let reference = reference.clone();
                    async move { pull_request_review::load(&rpc, &reference).await }
                });
                let pending = cx.background_spawn({
                    let drafts = drafts.clone();
                    async move { drafts.load() }
                });
                let (files, mut review_data, pending) = futures::join!(files, review_data, pending);
                let pending = pending.unwrap_or_else(|error| {
                    log::error!("couldn't load the pending review: {error:#}");
                    review_data.problems.push(format!("{error:#}"));
                    Default::default()
                });
                let files = files.with_context(|| format!("loading {display_ref}"))?;
                let review = inbox::review_files(files);
                if review.files.is_empty() {
                    return Err(anyhow!(
                        inbox::unavailable_note(&display_ref, &review.unavailable)
                            .unwrap_or_else(|| format!("{display_ref} has no changed files"))
                    ));
                }
                log::info!(
                    "Opening {display_ref}: {} files, {} not shown, {} review threads, {} drafts \
                     (kept in {})",
                    review.files.len(),
                    review.unavailable.len(),
                    review_data.threads.len(),
                    pending.comments.len(),
                    drafts.path().display(),
                );
                let mut permissions = review_data.permissions;
                permissions.blocked = session.write_blocked_reason().map(Into::into);
                let contents = ReviewContents {
                    header: review_data.header,
                    files: review.files,
                    // The file tree lists these, dimmed, with the reason they aren't shown.
                    unavailable: review.unavailable,
                    threads: review_data.threads,
                    viewed: review_data.viewed,
                    backend: Some(if review_backend::dry_run_requested() {
                        log::warn!(
                            "{} is set: the review won't write to {display_ref}",
                            review_backend::DRY_RUN_ENV
                        );
                        Arc::new(T3ReviewBackend::new(
                            Arc::new(DryRunRpc(rpc)),
                            reference,
                            drafts,
                        ))
                    } else {
                        Arc::new(T3ReviewBackend::new(Arc::new(rpc), reference, drafts))
                    }),
                    permissions,
                    pending,
                };
                let open_review = workspace.update_in(cx, |workspace, window, cx| {
                    ReviewView::open(contents, workspace, window, cx)
                })?;
                open_review.await?;
                workspace.update_in(cx, |workspace, window, cx| {
                    workspace.open_panel::<NavPanel>(window, cx);
                    if !review_data.problems.is_empty() {
                        let id = NotificationId::composite::<HomeView>(display_ref.clone());
                        let note = format!(
                            "{display_ref}: couldn't load {}",
                            review_data.problems.join("; ")
                        );
                        workspace.show_toast(Toast::new(id, note), cx);
                    }
                })?;
                anyhow::Ok(())
            }
            .await;
            this.update(cx, |this, cx| {
                this.opening = None;
                if let Err(error) = result {
                    log::error!("couldn't open {display_ref}: {error:#}");
                    this.open_error = Some(format!("{error:#}").into());
                }
                cx.notify();
            })
            .ok();
        });
        self.opening = Some(OpeningPullRequest { index, _task: task });
        cx.notify();
    }

    fn submit_pairing(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let input = self.pairing_editor.read(cx).text(cx);
        let Some(credential) = t3_connection::parse_pairing_input(&input) else {
            self.pairing_error = Some("Paste a pairing credential or a T3 pairing link.".into());
            cx.notify();
            return;
        };
        self.pairing_error = None;
        self.pairing_editor
            .update(cx, |editor, cx| editor.clear(window, cx));
        self.connection.update(cx, |connection, cx| {
            connection.pair_with_credential(credential, cx)
        });
    }

    fn reconnect(&mut self, cx: &mut Context<Self>) {
        self.connection
            .update(cx, |connection, cx| connection.reconnect(cx));
    }

    fn render_connection(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let connection = self.connection.read(cx);
        let server = connection
            .server_url()
            .map(|url| url.as_str().trim_end_matches('/').to_string())
            .unwrap_or_default();
        let (color, title, detail): (Color, SharedString, Option<SharedString>) =
            match connection.status() {
                ConnectionStatus::Connected {
                    environment,
                    server_version,
                } => (
                    Color::Success,
                    format!("Connected to {environment}").into(),
                    Some(match server_version {
                        Some(version) => format!("T3 Code {version} at {server}").into(),
                        None => format!("T3 Code at {server}").into(),
                    }),
                ),
                ConnectionStatus::Connecting(step) => (
                    Color::Muted,
                    "Connecting to T3 Code…".into(),
                    Some(format!("{step} · {server}").into()),
                ),
                ConnectionStatus::NeedsPairing(reason) => (
                    Color::Warning,
                    "Pair mustr with T3 Code".into(),
                    Some(reason.clone()),
                ),
                ConnectionStatus::Failed(error) => (
                    Color::Error,
                    "Couldn't connect to T3 Code".into(),
                    Some(error.clone()),
                ),
            };
        // Connected, but paired without the scopes writes need: pairing again fixes it.
        let read_only = connection
            .session()
            .filter(|_| connection.status().is_connected())
            .and_then(|session| session.write_blocked_reason());
        let needs_pairing =
            matches!(connection.status(), ConnectionStatus::NeedsPairing(_)) || read_only.is_some();
        let can_retry = !matches!(connection.status(), ConnectionStatus::Connecting(_))
            && connection.server_url().is_some();
        let retry_label = if connection.status().is_connected() {
            "Reconnect"
        } else {
            "Try again"
        };

        card(cx)
            .gap_2()
            .child(
                h_flex()
                    .gap_2()
                    .child(Indicator::dot().color(color))
                    .child(Label::new(title).weight(gpui::FontWeight::MEDIUM))
                    .child(div().flex_1())
                    .when(can_retry, |this| {
                        this.child(
                            Button::new("reconnect", retry_label)
                                .style(ButtonStyle::Subtle)
                                .label_size(LabelSize::Small)
                                .on_click(cx.listener(|this, _, _, cx| this.reconnect(cx))),
                        )
                    }),
            )
            .when_some(detail, |this, detail| {
                this.child(
                    Label::new(detail)
                        .size(LabelSize::Small)
                        .color(Color::Muted),
                )
            })
            .when_some(read_only, |this, read_only| {
                this.child(
                    Label::new(read_only)
                        .size(LabelSize::Small)
                        .color(Color::Warning),
                )
            })
            .when(needs_pairing, |this| this.child(self.render_pairing(cx)))
    }

    fn render_pairing(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let colors = cx.theme().colors();
        v_flex()
            .gap_2()
            .pt_1()
            .child(
                Label::new(
                    "On the T3 machine, run this command, then paste the credential or the \
                     pairing link it prints:",
                )
                .size(LabelSize::Small),
            )
            .child(
                div()
                    .px_2()
                    .py_1()
                    .rounded(tokens::CORNER_RADIUS / 2.)
                    .bg(colors.editor_background)
                    .child(
                        Label::new(t3_connection::pairing_command())
                            .size(LabelSize::Small)
                            .buffer_font(cx),
                    ),
            )
            .child(
                h_flex()
                    .gap_2()
                    .child(
                        div()
                            .key_context("MustrPairing")
                            .on_action(cx.listener(|this, _: &menu::Confirm, window, cx| {
                                this.submit_pairing(window, cx)
                            }))
                            .flex_1()
                            .px_2()
                            .py_1()
                            .rounded(tokens::CORNER_RADIUS / 2.)
                            .border(tokens::BORDER_WIDTH)
                            .border_color(colors.border)
                            .bg(colors.editor_background)
                            .child(self.pairing_editor.clone()),
                    )
                    .child(
                        Button::new("pair", "Pair")
                            .style(ButtonStyle::Filled)
                            .on_click(
                                cx.listener(|this, _, window, cx| this.submit_pairing(window, cx)),
                            ),
                    ),
            )
            .when_some(self.pairing_error.clone(), |this, error| {
                this.child(Label::new(error).size(LabelSize::Small).color(Color::Error))
            })
    }

    fn render_pull_requests(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let count = match &self.pull_requests {
            Loadable::Loaded(list) => Some(list.entries.len()),
            _ => None,
        };
        let body = match &self.pull_requests {
            Loadable::NotLoaded => None,
            Loadable::Loading => Some(muted_line("Loading pull requests…").into_any_element()),
            Loadable::Failed(error) => Some(error_line(error.clone()).into_any_element()),
            Loadable::Loaded(list) if list.entries.is_empty() && list.errors.is_empty() => {
                Some(muted_line("No open pull requests.").into_any_element())
            }
            Loadable::Loaded(list) => Some(
                v_flex()
                    .children(
                        list.entries
                            .iter()
                            .enumerate()
                            .map(|(index, entry)| self.render_pull_request(index, entry, cx)),
                    )
                    .children(list.errors.iter().map(|error| {
                        muted_line(format!("Couldn't list {error}")).into_any_element()
                    }))
                    .into_any_element(),
            ),
        };
        section("Open pull requests", count, body, cx)
    }

    fn render_pull_request(
        &self,
        index: usize,
        entry: &PullRequestEntry,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let colors = cx.theme().colors();
        let is_opening = self
            .opening
            .as_ref()
            .is_some_and(|opening| opening.index == index);
        let mut details = vec![entry.display_ref()];
        if let Some(author) = &entry.author {
            details.push(format!("@{}", author.login));
        }
        if entry.is_draft {
            details.push("draft".to_string());
        }
        h_flex()
            .id(("pull-request", index))
            .w_full()
            .gap_3()
            .px_2()
            .py_1p5()
            .rounded(tokens::CORNER_RADIUS / 2.)
            .cursor_pointer()
            .hover(|style| style.bg(colors.element_hover))
            .on_click(
                cx.listener(move |this, _, window, cx| this.open_pull_request(index, window, cx)),
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
            .when(entry.has_line_counts(), |this| {
                this.child(DiffStat::new(
                    ("pull-request-diff-stat", index),
                    entry.additions as usize,
                    entry.deletions as usize,
                ))
            })
            .when(is_opening, |this| {
                this.child(
                    Label::new("Opening…")
                        .size(LabelSize::Small)
                        .color(Color::Accent),
                )
            })
            .into_any_element()
    }

    fn render_threads(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let count = match &self.threads {
            Loadable::Loaded(threads) => Some(threads.len()),
            _ => None,
        };
        let body = match &self.threads {
            Loadable::NotLoaded => None,
            Loadable::Loading => Some(muted_line("Loading agent threads…").into_any_element()),
            Loadable::Failed(error) => Some(error_line(error.clone()).into_any_element()),
            Loadable::Loaded(threads) if threads.is_empty() => {
                Some(muted_line("No agent threads yet.").into_any_element())
            }
            Loadable::Loaded(threads) => Some(
                v_flex()
                    .children(
                        threads
                            .iter()
                            .enumerate()
                            .map(|(index, thread)| render_thread(index, thread).into_any_element()),
                    )
                    .into_any_element(),
            ),
        };
        section("Recent agent threads", count, body, cx)
    }
}

fn render_thread(index: usize, thread: &ThreadRow) -> impl IntoElement {
    let color = match thread.status {
        ThreadStatus::NeedsInput => Color::Warning,
        ThreadStatus::Working(_) => Color::Accent,
        ThreadStatus::Failed => Color::Error,
        ThreadStatus::Settled(_) => Color::Muted,
    };
    let details = [thread.branch.as_deref(), thread.project.as_deref()]
        .into_iter()
        .flatten()
        .collect::<Vec<_>>()
        .join(" · ");
    h_flex()
        .id(("agent-thread", index))
        .w_full()
        .gap_3()
        .px_2()
        .py_1p5()
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
        .child(
            h_flex()
                .gap_1()
                .flex_none()
                .child(Indicator::dot().color(color))
                .child(
                    Label::new(thread.status.label().to_string())
                        .size(LabelSize::Small)
                        .color(color),
                ),
        )
}

fn card(cx: &App) -> Div {
    let colors = cx.theme().colors();
    v_flex()
        .w_full()
        .p_3()
        .rounded(tokens::CORNER_RADIUS)
        .border(tokens::BORDER_WIDTH)
        .border_color(colors.border)
        .bg(colors.surface_background)
}

fn section(
    title: &'static str,
    count: Option<usize>,
    body: Option<AnyElement>,
    cx: &App,
) -> impl IntoElement {
    card(cx)
        .gap_1()
        .child(
            h_flex()
                .gap_2()
                .px_2()
                .pb_1()
                .child(Label::new(title).weight(gpui::FontWeight::SEMIBOLD))
                .when_some(count, |this, count| {
                    this.child(
                        Label::new(count.to_string())
                            .size(LabelSize::Small)
                            .color(Color::Muted),
                    )
                }),
        )
        .child(Divider::horizontal())
        .children(body)
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

impl EventEmitter<ItemEvent> for HomeView {}

impl Focusable for HomeView {
    fn focus_handle(&self, _: &App) -> FocusHandle {
        self.focus_handle.clone()
    }
}

impl Item for HomeView {
    type Event = ItemEvent;

    fn tab_content_text(&self, _detail: usize, _cx: &App) -> SharedString {
        "Home".into()
    }

    fn show_toolbar(&self) -> bool {
        false
    }

    fn to_item_events(event: &Self::Event, f: &mut dyn FnMut(ItemEvent)) {
        f(*event)
    }
}

impl Render for HomeView {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let show_lists = self.loaded_generation.is_some();
        v_flex()
            .key_context("MustrHome")
            .track_focus(&self.focus_handle)
            .size_full()
            .bg(cx.theme().colors().editor_background)
            .child(
                v_flex()
                    .id("home-scroll")
                    .size_full()
                    .overflow_y_scroll()
                    .child(
                        v_flex()
                            .w_full()
                            .max_w(px(CONTENT_MAX_WIDTH))
                            .mx_auto()
                            .px_6()
                            .py_6()
                            .gap_4()
                            .child(
                                v_flex()
                                    .gap_0p5()
                                    .child(Headline::new("mustr").size(HeadlineSize::Large))
                                    .child(
                                        Label::new(
                                            "Review every change, from people and from agents.",
                                        )
                                        .color(Color::Muted),
                                    ),
                            )
                            .child(self.render_connection(cx))
                            .when_some(self.open_error.clone(), |this, error| {
                                this.child(card(cx).child(error_line(error)))
                            })
                            .when(show_lists, |this| {
                                this.child(self.render_pull_requests(cx))
                                    .child(self.render_threads(cx))
                            }),
                    ),
            )
    }
}

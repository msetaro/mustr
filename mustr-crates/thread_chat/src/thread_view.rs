//! A thread as a workspace item: its state, kept current by `orchestration.subscribeThread`,
//! and the layout of its parts: the header (`header.rs`), the timeline (`timeline.rs`), and the
//! composer that messages, queues, steers, stops, and answers the agent (`composer.rs`). A new
//! thread starts as a draft with a form for where and how the agent runs.

use crate::{
    backend::ThreadBackend,
    commands::{self, LaunchIds},
    composer::{COMPOSER_MAX_LINES, COMPOSER_MIN_LINES},
    family_bar::ThreadFamilyBar,
    model::{Row, ThreadState},
    new_thread::{NewThreadForm, NewThreadFormEvent},
    settings_bar::ThreadSettingsBar,
    timeline::truncate,
};
use collections::{HashMap, HashSet};
use editor::Editor;
use futures::StreamExt as _;
use gpui::{
    AnyElement, App, AppContext as _, Context, Entity, EventEmitter, FocusHandle, Focusable,
    FollowMode, IntoElement, ListAlignment, ListState, ParentElement as _, Render, SharedString,
    Styled as _, Subscription, Task, WeakEntity, Window, actions, div, list, px,
};
use language::LanguageRegistry;
use markdown::Markdown;
use mustr_ui::palette;
use std::sync::Arc;
use t3_api::types::OrchestrationV2SubscribeThreadInput;
use ui::{Color, Label, LabelCommon as _, LabelSize, prelude::*};
use workspace::{
    Workspace,
    item::{Item, ItemEvent},
};

actions!(
    thread_chat,
    [
        /// Opens a new agent thread.
        NewThread,
    ]
);

pub(crate) const CONTENT_MAX_WIDTH: f32 = 820.;

pub(crate) enum Loadable {
    Loading,
    Failed(SharedString),
    Loaded(Box<ThreadState>),
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Connection {
    Live,
    Reconnecting,
}

pub struct ThreadView {
    focus_handle: FocusHandle,
    pub(crate) backend: Arc<dyn ThreadBackend>,
    pub(crate) language_registry: Option<Arc<LanguageRegistry>>,
    /// `None` while this is a draft for a new thread.
    pub(crate) thread_id: Option<String>,
    /// The title from the thread list, until the thread loads.
    placeholder_title: SharedString,
    pub(crate) state: Loadable,
    pub(crate) connection: Connection,
    pub(crate) new_thread: Option<Entity<NewThreadForm>>,
    pub(crate) rows: Vec<Row>,
    pub(crate) row_revisions: Vec<u64>,
    pub(crate) list_state: ListState,
    pub(crate) markdown: HashMap<String, Entity<Markdown>>,
    /// Expanded runs of work (by `work:` and their first item's id) and expanded tool calls (by
    /// id).
    pub(crate) expanded: HashSet<String>,
    pub(crate) loading_outputs: HashSet<String>,
    pub(crate) composer: Entity<Editor>,
    /// In the composer's bottom row.
    pub(crate) settings_bar: Entity<ThreadSettingsBar>,
    /// Under the header.
    family_bar: Entity<ThreadFamilyBar>,
    pub(crate) sending: bool,
    /// Requests being answered, by id.
    pub(crate) responding: HashSet<String>,
    /// Answers picked for questions, by request id and question id.
    pub(crate) answers: HashMap<String, HashMap<String, String>>,
    pub(crate) error: Option<SharedString>,
    /// The composer's images, and the timeline's as they load (`attachments.rs`).
    pub(crate) images: crate::attachments::Images,
    /// The composer's and timeline's other state: copied messages, prompt history, drafts.
    pub(crate) composer_state: crate::composer::ComposerState,
    _load: Option<Task<()>>,
    _follow: Option<Task<()>>,
    pub(crate) _tasks: Vec<Task<()>>,
    pub(crate) _subscriptions: Vec<Subscription>,
}

impl ThreadView {
    /// Activates the view of `thread_id` if one is open, or opens one.
    pub fn open(
        thread_id: String,
        title: SharedString,
        workspace: &mut Workspace,
        window: &mut Window,
        cx: &mut Context<Workspace>,
    ) -> Option<Entity<Self>> {
        let existing = workspace.panes().iter().find_map(|pane| {
            pane.read(cx)
                .items_of_type::<Self>()
                .find(|view| view.read(cx).thread_id.as_deref() == Some(thread_id.as_str()))
        });
        if let Some(existing) = existing {
            workspace.activate_item(&existing, true, true, window, cx);
            return Some(existing);
        }
        let backend = crate::backend(cx)?;
        let languages = workspace.project().read(cx).languages().clone();
        let view =
            cx.new(|cx| Self::new(backend, Some(languages), Some(thread_id), title, window, cx));
        workspace.add_item_to_active_pane(Box::new(view.clone()), None, true, window, cx);
        Some(view)
    }

    /// Opens a draft for a new thread.
    pub fn open_new(
        workspace: &mut Workspace,
        window: &mut Window,
        cx: &mut Context<Workspace>,
    ) -> Option<Entity<Self>> {
        let backend = crate::backend(cx)?;
        let languages = workspace.project().read(cx).languages().clone();
        let view = cx.new(|cx| {
            Self::new(
                backend,
                Some(languages),
                None,
                "New thread".into(),
                window,
                cx,
            )
        });
        workspace.add_item_to_active_pane(Box::new(view.clone()), None, true, window, cx);
        Some(view)
    }

    pub fn new(
        backend: Arc<dyn ThreadBackend>,
        language_registry: Option<Arc<LanguageRegistry>>,
        thread_id: Option<String>,
        placeholder_title: SharedString,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        let is_draft = thread_id.is_none();
        let composer = cx.new(|cx| {
            let mut editor =
                Editor::auto_height(COMPOSER_MIN_LINES, COMPOSER_MAX_LINES, window, cx);
            editor.set_placeholder_text(
                if is_draft {
                    "Describe the task for the agent…"
                } else {
                    "Message the agent…"
                },
                window,
                cx,
            );
            editor
        });
        let composer_subscription = crate::composer::observe_composer(&composer, cx);
        let list_state = ListState::new(0, ListAlignment::Top, px(2048.));
        list_state.set_follow_mode(FollowMode::Tail);
        let this_view = cx.weak_entity();
        let settings_bar = cx.new(|cx| ThreadSettingsBar::new(this_view.clone(), window, cx));
        let family_bar = cx.new(|cx| ThreadFamilyBar::new(this_view, window, cx));
        let mut this = Self {
            focus_handle: cx.focus_handle(),
            backend: backend.clone(),
            language_registry,
            thread_id: thread_id.clone(),
            placeholder_title,
            state: Loadable::Loading,
            connection: Connection::Live,
            new_thread: None,
            rows: Vec::new(),
            row_revisions: Vec::new(),
            list_state,
            markdown: HashMap::default(),
            expanded: HashSet::default(),
            loading_outputs: HashSet::default(),
            composer,
            settings_bar,
            family_bar,
            sending: false,
            responding: HashSet::default(),
            answers: HashMap::default(),
            error: None,
            images: Default::default(),
            composer_state: Default::default(),
            _load: None,
            _follow: None,
            _tasks: Vec::new(),
            _subscriptions: vec![composer_subscription],
        };
        match thread_id {
            Some(thread_id) => this.load(thread_id, cx),
            None => {
                let form = cx.new(|cx| NewThreadForm::new(backend, window, cx));
                this._subscriptions.push(cx.subscribe(
                    &form,
                    |_, _, event: &NewThreadFormEvent, cx| match event {
                        NewThreadFormEvent::Changed => cx.notify(),
                    },
                ));
                this.new_thread = Some(form);
            }
        }
        this
    }

    pub fn thread_id(&self) -> Option<&str> {
        self.thread_id.as_deref()
    }

    pub fn thread_state(&self) -> Option<&ThreadState> {
        match &self.state {
            Loadable::Loaded(state) => Some(state),
            _ => None,
        }
    }

    pub fn composer(&self) -> &Entity<Editor> {
        &self.composer
    }

    pub fn error(&self) -> Option<&SharedString> {
        self.error.as_ref()
    }

    /// The rows the timeline shows, for tests.
    pub fn rows(&self) -> &[Row] {
        &self.rows
    }

    pub fn title(&self) -> SharedString {
        match &self.state {
            Loadable::Loaded(state) if !state.thread.title.trim().is_empty() => {
                state.thread.title.clone().into()
            }
            _ => self.placeholder_title.clone(),
        }
    }

    /// Loads the thread's snapshot, then follows it. Retries while T3 can't be reached (for
    /// example, before mustr has connected), showing why.
    fn load(&mut self, thread_id: String, cx: &mut Context<Self>) {
        self.state = Loadable::Loading;
        let backend = self.backend.clone();
        self._load = Some(cx.spawn(async move |this, cx| {
            let mut attempt = 0;
            loop {
                let snapshot = backend.thread_snapshot(thread_id.clone()).await;
                let loaded = this.update(cx, |this, cx| {
                    let loaded = match snapshot {
                        Ok(snapshot) => {
                            let mut state =
                                ThreadState::new(snapshot.projection, snapshot.snapshot_sequence);
                            state.has_more_history = snapshot.has_more_history.unwrap_or(false);
                            let sequence = state.sequence;
                            this.state = Loadable::Loaded(Box::new(state));
                            this.sync_rows(cx);
                            cx.emit(ItemEvent::UpdateTab);
                            this.follow(thread_id.clone(), Some(sequence), cx);
                            true
                        }
                        Err(error) => {
                            log::warn!("couldn't load thread {thread_id}: {error:#}");
                            this.state = Loadable::Failed(format!("{error:#}").into());
                            false
                        }
                    };
                    cx.notify();
                    loaded
                });
                match loaded {
                    Ok(false) => {}
                    Ok(true) | Err(_) => return,
                }
                cx.background_executor()
                    .timer(crate::backend::retry_delay(attempt))
                    .await;
                attempt += 1;
            }
        }));
    }

    /// Follows the thread from `after_sequence` (or from a fresh snapshot), resubscribing when
    /// the stream ends, which it does when mustr reconnects to T3.
    fn follow(&mut self, thread_id: String, after_sequence: Option<i64>, cx: &mut Context<Self>) {
        let backend = self.backend.clone();
        self._follow = Some(cx.spawn(async move |this, cx| {
            let mut after_sequence = after_sequence;
            let mut attempt = 0;
            loop {
                let input = OrchestrationV2SubscribeThreadInput {
                    accept_bounded_snapshot: None,
                    accept_compact_turn_items: None,
                    after_sequence,
                    request_completion_marker: None,
                    thread_id: thread_id.clone(),
                };
                match backend.subscribe_thread(input) {
                    Ok(mut stream) => {
                        while let Some(item) = stream.next().await {
                            let item = match item {
                                Ok(item) => item,
                                Err(error) => {
                                    // A missed or unreadable event leaves the timeline stale, so
                                    // start over from a snapshot.
                                    log::warn!("thread {thread_id}: {error:#}; resyncing");
                                    after_sequence = None;
                                    break;
                                }
                            };
                            attempt = 0;
                            let applied = this.update(cx, |this, cx| {
                                this.connection = Connection::Live;
                                let Loadable::Loaded(state) = &mut this.state else {
                                    return;
                                };
                                if state.apply(item).changed {
                                    this.sync_rows(cx);
                                    cx.emit(ItemEvent::UpdateTab);
                                }
                                cx.notify();
                            });
                            if applied.is_err() {
                                return;
                            }
                        }
                    }
                    Err(error) => log::warn!("couldn't follow thread {thread_id}: {error:#}"),
                }
                let current = this.update(cx, |this, cx| {
                    this.connection = Connection::Reconnecting;
                    cx.notify();
                    match &this.state {
                        Loadable::Loaded(state) => Some(state.sequence),
                        _ => None,
                    }
                });
                let Ok(current) = current else {
                    return;
                };
                if after_sequence.is_some() {
                    after_sequence = current;
                }
                cx.background_executor()
                    .timer(crate::backend::retry_delay(attempt))
                    .await;
                attempt += 1;
            }
        }));
    }

    pub(crate) fn launch(
        &mut self,
        form: Entity<NewThreadForm>,
        text: String,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let new_thread = match form.read(cx).new_thread(text.trim(), cx) {
            Ok(new_thread) => new_thread,
            Err(reason) => {
                self.error = Some(reason);
                cx.notify();
                return;
            }
        };
        let title = commands::title_from_message(&new_thread.text);
        let mut input = commands::launch(new_thread, LaunchIds::new());
        let sent_images = self.attach_composer_images(&mut input);
        let launch = self.backend.launch_thread(input);
        self.sending = true;
        self.error = None;
        cx.notify();
        self._tasks.push(cx.spawn_in(window, async move |this, cx| {
            let result = launch.await;
            this.update_in(cx, |this, window, cx| {
                this.sending = false;
                match result {
                    Ok(launched) => {
                        this.images.clear_sent(&sent_images);
                        this.new_thread = None;
                        this.placeholder_title = title.into();
                        this.thread_id = Some(launched.thread_id.clone());
                        this.composer.update(cx, |editor, cx| {
                            editor.clear(window, cx);
                            editor.set_placeholder_text("Message the agent…", window, cx);
                        });
                        this.state =
                            Loadable::Loaded(Box::new(ThreadState::new(launched.projection, 0)));
                        this.sync_rows(cx);
                        // No sequence comes with the launch, so follow from a snapshot.
                        this.follow(launched.thread_id, None, cx);
                        cx.emit(ItemEvent::UpdateTab);
                    }
                    Err(error) => this.error = Some(format!("{error:#}").into()),
                }
                cx.notify();
            })
            .ok();
        }));
    }
}

impl EventEmitter<ItemEvent> for ThreadView {}

impl Focusable for ThreadView {
    fn focus_handle(&self, cx: &App) -> FocusHandle {
        self.composer.focus_handle(cx)
    }
}

impl Item for ThreadView {
    type Event = ItemEvent;

    fn tab_content_text(&self, _detail: usize, _cx: &App) -> SharedString {
        let title = self.title();
        truncate(&title, 40).into()
    }

    fn tab_tooltip_text(&self, _cx: &App) -> Option<SharedString> {
        Some(self.title())
    }

    /// So the app can add toolbar items for threads (`mustr/src/window.rs`). Zed's toolbar
    /// draws nothing while none of its items shows, and its search bars hide for threads, so
    /// it takes no space until an item does.
    fn show_toolbar(&self) -> bool {
        true
    }

    fn to_item_events(event: &Self::Event, f: &mut dyn FnMut(ItemEvent)) {
        f(*event)
    }
}

impl Render for ThreadView {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let palette = palette(cx);
        let body: AnyElement = match (&self.state, &self.new_thread) {
            (_, Some(form)) => v_flex()
                .flex_1()
                .w_full()
                .items_center()
                .px_4()
                .py_6()
                .child(
                    div()
                        .w_full()
                        .max_w(px(CONTENT_MAX_WIDTH))
                        .child(form.clone()),
                )
                .into_any_element(),
            (Loadable::Loading, None) => centered_note("Loading the thread…", Color::Muted),
            (Loadable::Failed(error), None) => centered_note(error.clone(), Color::Error),
            (Loadable::Loaded(_), None) if self.rows.is_empty() => {
                centered_note("No messages yet.", Color::Muted)
            }
            (Loadable::Loaded(_), None) => list(
                self.list_state.clone(),
                cx.processor(|this, index: usize, window, cx| this.render_row(index, window, cx)),
            )
            .flex_1()
            .size_full()
            .into_any_element(),
        };
        v_flex()
            .key_context("ThreadView")
            .track_focus(&self.focus_handle)
            .size_full()
            .bg(palette.background.hsla())
            .when(self.new_thread.is_none(), |this| {
                this.child(self.render_header(cx))
                    .child(self.family_bar.clone())
            })
            .child(
                div()
                    .flex_1()
                    .min_h_0()
                    .w_full()
                    .flex()
                    .flex_col()
                    .child(body),
            )
            .child(self.render_composer(window, cx))
    }
}

fn centered_note(text: impl Into<SharedString>, color: Color) -> AnyElement {
    v_flex()
        .flex_1()
        .size_full()
        .items_center()
        .justify_center()
        .child(Label::new(text.into()).size(LabelSize::Small).color(color))
        .into_any_element()
}

/// Opens a thread's view in `workspace`, or activates it.
///
/// It's deferred until the current update ends, so a view can call it from its own event
/// handlers: activating an item updates the panels and the title bar, which read other views.
pub fn open_thread(
    thread_id: String,
    title: SharedString,
    workspace: WeakEntity<Workspace>,
    window: &mut Window,
    cx: &mut App,
) {
    window.defer(cx, move |window, cx| {
        let opened = workspace.update(cx, |workspace, cx| {
            ThreadView::open(thread_id, title, workspace, window, cx).is_some()
        });
        if !matches!(opened, Ok(true)) {
            log::error!("couldn't open the agent thread");
        }
    });
}

pub(crate) fn register(workspace: &mut Workspace) {
    workspace.register_action(|workspace, _: &NewThread, window, cx| {
        ThreadView::open_new(workspace, window, cx);
    });
}

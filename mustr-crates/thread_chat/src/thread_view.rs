//! A thread as a workspace item: its timeline, kept current by `orchestration.subscribeThread`,
//! and the composer that messages, queues, steers, stops, and answers the agent. A new thread
//! starts as a draft with a form for where and how the agent runs.

use crate::{
    backend::ThreadBackend,
    commands::{self, CommandIds, Delivery, LaunchIds},
    model::{
        ContextUsage, ItemKind, ItemStatus, PendingRequest, QueuedMessage, Row, ThreadActivity,
        ThreadState, TimelineItem, ToolKind, canonical_item,
    },
    new_thread::{NewThreadForm, NewThreadFormEvent},
};
use collections::{HashMap, HashSet};
use editor::Editor;
use futures::StreamExt as _;
use gpui::{
    AnyElement, App, AppContext as _, Context, Entity, EventEmitter, FocusHandle, Focusable,
    FollowMode, FontWeight, Hsla, IntoElement, ListAlignment, ListState, ParentElement as _,
    Render, SharedString, Styled as _, Subscription, Task, TextStyleRefinement, WeakEntity, Window,
    actions, div, list, px, relative,
};
use language::LanguageRegistry;
use markdown::{Markdown, MarkdownElement, MarkdownStyle};
use mustr_ui::{ButtonStyle as MustrButtonStyle, Card, Pill, TextButton, palette, tokens};
use std::sync::Arc;
use t3_api::types::{
    OrchestrationV2GetTurnItemInput, OrchestrationV2SubscribeThreadInput, ProviderApprovalDecision,
};
use ui::{
    Color, Icon, IconName, IconSize, Indicator, Label, LabelCommon as _, LabelSize, Tooltip,
    prelude::*,
};
use workspace::{
    Workspace,
    item::{Item, ItemEvent},
};

actions!(
    thread_chat,
    [
        /// Sends the composer's message: starts a run, or, while the agent works, queues it.
        SendMessage,
        /// Sends the composer's message to the running agent now, without waiting for its run
        /// to finish.
        SteerMessage,
        /// Stops the agent's run.
        StopThread,
        /// Opens a new agent thread.
        NewThread,
    ]
);

pub(crate) const COMPOSER_CONTEXT: &str = "ThreadComposer";
const COMPOSER_MIN_LINES: usize = 2;
const COMPOSER_MAX_LINES: usize = 12;
const CONTENT_MAX_WIDTH: f32 = 820.;
/// Tool calls shown as chips on a collapsed run of work.
const COLLAPSED_CHIPS: usize = 6;
const CHIP_MAX_CHARS: usize = 48;

enum Loadable {
    Loading,
    Failed(SharedString),
    Loaded(Box<ThreadState>),
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Connection {
    Live,
    Reconnecting,
}

pub struct ThreadView {
    focus_handle: FocusHandle,
    backend: Arc<dyn ThreadBackend>,
    language_registry: Option<Arc<LanguageRegistry>>,
    /// `None` while this is a draft for a new thread.
    thread_id: Option<String>,
    /// The title from the thread list, until the thread loads.
    placeholder_title: SharedString,
    state: Loadable,
    connection: Connection,
    new_thread: Option<Entity<NewThreadForm>>,
    rows: Vec<Row>,
    row_revisions: Vec<u64>,
    list_state: ListState,
    markdown: HashMap<String, Entity<Markdown>>,
    /// Expanded runs of work (by their first item's id) and expanded tool calls (by id).
    expanded: HashSet<String>,
    loading_outputs: HashSet<String>,
    composer: Entity<Editor>,
    sending: bool,
    /// Requests being answered, by id.
    responding: HashSet<String>,
    /// Answers picked for questions, by request id and question id.
    answers: HashMap<String, HashMap<String, String>>,
    error: Option<SharedString>,
    _load: Option<Task<()>>,
    _follow: Option<Task<()>>,
    _tasks: Vec<Task<()>>,
    _subscriptions: Vec<Subscription>,
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
        let list_state = ListState::new(0, ListAlignment::Top, px(2048.));
        list_state.set_follow_mode(FollowMode::Tail);
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
            sending: false,
            responding: HashSet::default(),
            answers: HashMap::default(),
            error: None,
            _load: None,
            _follow: None,
            _tasks: Vec::new(),
            _subscriptions: Vec::new(),
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

    /// Brings the list's rows in line with the thread, re-measuring only from the first row
    /// that changed. Streaming changes the last rows, so this stays cheap.
    fn sync_rows(&mut self, cx: &mut Context<Self>) {
        let Loadable::Loaded(state) = &self.state else {
            return;
        };
        let rows = state.rows();
        let revisions = rows
            .iter()
            .map(|row| state.row_revision(row))
            .collect::<Vec<_>>();
        let unchanged = rows
            .iter()
            .zip(&revisions)
            .zip(self.rows.iter().zip(&self.row_revisions))
            .take_while(|((row, revision), (old_row, old_revision))| {
                row == old_row && revision == old_revision
            })
            .count();
        // Only rows already shown have Markdown to update; the rest get theirs when they're first
        // rendered, so opening a long thread parses only what's on screen.
        let mut texts = Vec::new();
        for row in &rows[unchanged..] {
            let indices = match row {
                Row::Item(index) => std::slice::from_ref(index),
                Row::Work(indices) => indices.as_slice(),
            };
            texts.extend(
                indices
                    .iter()
                    .filter_map(|index| state.item(*index))
                    .filter(|item| self.markdown.contains_key(&item.id))
                    .filter_map(markdown_source),
            );
        }
        self.list_state
            .splice(unchanged..self.rows.len(), rows.len() - unchanged);
        self.rows = rows;
        self.row_revisions = revisions;
        for (id, text) in texts {
            self.update_markdown(id, text, cx);
        }
    }

    fn update_markdown(&mut self, id: String, text: String, cx: &mut Context<Self>) {
        if let Some(markdown) = self.markdown.get(&id) {
            markdown.update(cx, |markdown, cx| {
                if markdown.source().as_ref() != text {
                    markdown.replace(text, cx);
                }
            });
            return;
        }
        let language_registry = self.language_registry.clone();
        let markdown = cx.new(|cx| Markdown::new(text.into(), language_registry, None, cx));
        self._subscriptions
            .push(cx.observe(&markdown, |_, _, cx| cx.notify()));
        self.markdown.insert(id, markdown);
    }

    /// Creates the Markdown of the items a row shows, if they don't have it yet.
    fn ensure_markdown(&mut self, row: &Row, cx: &mut Context<Self>) {
        let Loadable::Loaded(state) = &self.state else {
            return;
        };
        let indices = match row {
            Row::Item(index) => std::slice::from_ref(index),
            Row::Work(indices) => indices.as_slice(),
        };
        let missing = indices
            .iter()
            .filter_map(|index| state.item(*index))
            .filter(|item| !self.markdown.contains_key(&item.id))
            .filter_map(markdown_source)
            .collect::<Vec<_>>();
        for (id, text) in missing {
            self.update_markdown(id, text, cx);
        }
    }

    fn composer_text(&self, cx: &App) -> String {
        self.composer.read(cx).text(cx)
    }

    fn write_blocked_reason(&self) -> Option<String> {
        self.backend.write_blocked_reason()
    }

    fn send(&mut self, delivery: Delivery, window: &mut Window, cx: &mut Context<Self>) {
        let text = self.composer_text(cx);
        if text.trim().is_empty() || self.sending {
            return;
        }
        if let Some(form) = self.new_thread.clone() {
            self.launch(form, text, window, cx);
            return;
        }
        let Loadable::Loaded(state) = &self.state else {
            return;
        };
        let command =
            commands::send_message(state.thread_id(), text.trim(), delivery, CommandIds::new());
        let dispatch = self.backend.dispatch(command);
        self.sending = true;
        self.error = None;
        cx.notify();
        self._tasks.push(cx.spawn_in(window, async move |this, cx| {
            let result = dispatch.await;
            this.update_in(cx, |this, window, cx| {
                this.sending = false;
                match result {
                    Ok(_) => this
                        .composer
                        .update(cx, |editor, cx| editor.clear(window, cx)),
                    Err(error) => this.error = Some(format!("{error:#}").into()),
                }
                cx.notify();
            })
            .ok();
        }));
    }

    fn launch(
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
        let launch = self
            .backend
            .launch_thread(commands::launch(new_thread, LaunchIds::new()));
        self.sending = true;
        self.error = None;
        cx.notify();
        self._tasks.push(cx.spawn_in(window, async move |this, cx| {
            let result = launch.await;
            this.update_in(cx, |this, window, cx| {
                this.sending = false;
                match result {
                    Ok(launched) => {
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

    fn dispatch(
        &mut self,
        command: t3_api::tagged::OrchestrationV2Command,
        request_id: Option<String>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let dispatch = self.backend.dispatch(command);
        if let Some(request_id) = &request_id {
            self.responding.insert(request_id.clone());
        }
        self.error = None;
        cx.notify();
        self._tasks.push(cx.spawn_in(window, async move |this, cx| {
            let result = dispatch.await;
            this.update(cx, |this, cx| {
                if let Some(request_id) = &request_id {
                    this.responding.remove(request_id);
                    if result.is_ok() {
                        this.answers.remove(request_id);
                    }
                }
                if let Err(error) = result {
                    this.error = Some(format!("{error:#}").into());
                }
                cx.notify();
            })
            .ok();
        }));
    }

    pub(crate) fn stop(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(thread_id) = self.thread_id.clone() else {
            return;
        };
        self.dispatch(
            commands::stop(&thread_id, commands::new_id()),
            None,
            window,
            cx,
        );
    }

    pub(crate) fn respond(
        &mut self,
        request_id: String,
        decision: ProviderApprovalDecision,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(thread_id) = self.thread_id.clone() else {
            return;
        };
        let command =
            commands::respond_to_approval(&thread_id, &request_id, decision, commands::new_id());
        self.dispatch(command, Some(request_id), window, cx);
    }

    fn answer(&mut self, request_id: String, window: &mut Window, cx: &mut Context<Self>) {
        let Some(thread_id) = self.thread_id.clone() else {
            return;
        };
        let answers = self
            .answers
            .get(&request_id)
            .map(|answers| {
                let mut answers = answers
                    .iter()
                    .map(|(question, answer)| (question.clone(), answer.clone()))
                    .collect::<Vec<_>>();
                answers.sort();
                answers
            })
            .unwrap_or_default();
        let command =
            commands::answer_questions(&thread_id, &request_id, answers, commands::new_id());
        self.dispatch(command, Some(request_id), window, cx);
    }

    fn dismiss_questions(
        &mut self,
        request_id: String,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(thread_id) = self.thread_id.clone() else {
            return;
        };
        let command = commands::dismiss_questions(&thread_id, &request_id, commands::new_id());
        self.dispatch(command, Some(request_id), window, cx);
    }

    pub(crate) fn steer_with_queued(
        &mut self,
        run_id: String,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Loadable::Loaded(state) = &self.state else {
            return;
        };
        let Some(active_run) = state.active_run() else {
            return;
        };
        let command = commands::steer_with_queued(
            state.thread_id(),
            &run_id,
            &active_run.id,
            commands::new_id(),
        );
        self.dispatch(command, None, window, cx);
    }

    pub(crate) fn cancel_queued(
        &mut self,
        run_id: String,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(thread_id) = self.thread_id.clone() else {
            return;
        };
        let command = commands::cancel_queued(&thread_id, &run_id, commands::new_id());
        self.dispatch(command, None, window, cx);
    }

    fn toggle_expanded(&mut self, key: String, cx: &mut Context<Self>) {
        if !self.expanded.remove(&key) {
            self.expanded.insert(key);
        }
        // The row's height changes; rows keep their logical scroll position.
        self.list_state.remeasure();
        cx.notify();
    }

    /// Expands a tool call, fetching its output if T3 left it out.
    pub(crate) fn toggle_tool(&mut self, item_id: String, cx: &mut Context<Self>) {
        let needs_output = matches!(
            self.thread_state()
                .and_then(|state| state.items().iter().find(|item| item.id == item_id))
                .map(|item| &item.kind),
            Some(ItemKind::Tool {
                output_omitted: true,
                ..
            })
        );
        let expanding = !self.expanded.contains(&item_id);
        self.toggle_expanded(item_id.clone(), cx);
        if !(expanding && needs_output) || !self.loading_outputs.insert(item_id.clone()) {
            return;
        }
        let Some(thread_id) = self.thread_id.clone() else {
            return;
        };
        let request = self.backend.turn_item(OrchestrationV2GetTurnItemInput {
            item_id: item_id.clone(),
            revision: None,
            thread_id,
        });
        self._tasks.push(cx.spawn(async move |this, cx| {
            let result = request.await;
            this.update(cx, |this, cx| {
                this.loading_outputs.remove(&item_id);
                let output = match result {
                    Ok(result) => result
                        .item
                        .as_ref()
                        .and_then(canonical_item)
                        .and_then(|item| TimelineItem::from_turn_item(&item))
                        .and_then(|item| match item.kind {
                            ItemKind::Tool { output, .. } => output,
                            _ => None,
                        })
                        .or_else(|| Some("No output.".to_string())),
                    Err(error) => Some(format!("Couldn't load the output: {error:#}")),
                };
                if let Loadable::Loaded(state) = &mut this.state
                    && state.set_output(&item_id, output).changed
                {
                    this.sync_rows(cx);
                }
                cx.notify();
            })
            .ok();
        }));
    }

    fn pick_answer(
        &mut self,
        request_id: String,
        question_id: String,
        answer: String,
        cx: &mut Context<Self>,
    ) {
        self.answers
            .entry(request_id)
            .or_default()
            .insert(question_id, answer);
        cx.notify();
    }

    fn on_send(&mut self, _: &SendMessage, window: &mut Window, cx: &mut Context<Self>) {
        let running = self.thread_state().is_some_and(|state| state.is_running());
        self.send(
            if running {
                Delivery::Queue
            } else {
                Delivery::Auto
            },
            window,
            cx,
        );
    }

    fn on_steer(&mut self, _: &SteerMessage, window: &mut Window, cx: &mut Context<Self>) {
        let running = self.thread_state().is_some_and(|state| state.is_running());
        self.send(
            if running {
                Delivery::Steer
            } else {
                Delivery::Auto
            },
            window,
            cx,
        );
    }

    fn on_stop(&mut self, _: &StopThread, window: &mut Window, cx: &mut Context<Self>) {
        if self.thread_state().is_some_and(|state| state.is_running()) {
            self.stop(window, cx);
        }
    }
}

/// The Markdown an item shows, keyed by item id.
fn markdown_source(item: &TimelineItem) -> Option<(String, String)> {
    let text = match &item.kind {
        ItemKind::UserMessage { text, .. }
        | ItemKind::AssistantMessage { text, .. }
        | ItemKind::Reasoning { text } => text.clone(),
        ItemKind::Plan { markdown, .. } => markdown.clone(),
        _ => return None,
    };
    Some((item.id.clone(), text))
}

fn markdown_style(window: &Window, cx: &App) -> MarkdownStyle {
    let mut style = editor::hover_markdown_style(window, cx);
    style.base_text_style.refine(&TextStyleRefinement {
        font_size: Some(tokens::text_size(tokens::TEXT_SIZE_LARGE).into()),
        line_height: Some(relative(1.5)),
        ..Default::default()
    });
    style
}

fn muted_markdown_style(window: &Window, cx: &App) -> MarkdownStyle {
    let mut style = markdown_style(window, cx);
    style.base_text_style.refine(&TextStyleRefinement {
        color: Some(palette(cx).text_muted.hsla()),
        font_size: Some(tokens::text_size(tokens::TEXT_SIZE_MEDIUM).into()),
        ..Default::default()
    });
    style
}

fn truncate(text: &str, max_chars: usize) -> String {
    let line = text.lines().next().unwrap_or_default().trim();
    match line.char_indices().nth(max_chars) {
        Some((index, _)) => format!("{}…", &line[..index]),
        None if text.lines().nth(1).is_some() => format!("{line}…"),
        None => line.to_string(),
    }
}

fn status_color(status: ItemStatus, cx: &App) -> Hsla {
    let palette = palette(cx);
    match status {
        ItemStatus::Failed => cx.theme().status().error,
        ItemStatus::Cancelled | ItemStatus::Interrupted => palette.warning.hsla(),
        ItemStatus::Pending | ItemStatus::Running | ItemStatus::Waiting => palette.accent.hsla(),
        ItemStatus::Completed => palette.ok.hsla(),
    }
}

fn tool_icon(tool: ToolKind) -> IconName {
    match tool {
        ToolKind::Command => IconName::ToolTerminal,
        ToolKind::FileSearch => IconName::ToolSearch,
        ToolKind::WebSearch => IconName::ToolWeb,
        ToolKind::Tool => IconName::ToolHammer,
        ToolKind::Subagent => IconName::Thread,
    }
}

/// "3 commands · 2 files edited (+12 −3) · thought", for a collapsed run of work.
fn work_summary(items: &[&TimelineItem]) -> String {
    let mut commands = 0;
    let mut searches = 0;
    let mut tools = 0;
    let mut files = 0;
    let mut additions = 0;
    let mut deletions = 0;
    let mut reasoning = false;
    for item in items {
        match &item.kind {
            ItemKind::Tool {
                tool: ToolKind::Command,
                ..
            } => commands += 1,
            ItemKind::Tool {
                tool: ToolKind::FileSearch | ToolKind::WebSearch,
                ..
            } => searches += 1,
            ItemKind::Tool { .. } => tools += 1,
            ItemKind::FileChange {
                additions: added,
                deletions: deleted,
                changes,
                ..
            } => {
                files += changes.len().max(1);
                additions += added.unwrap_or_default();
                deletions += deleted.unwrap_or_default();
            }
            ItemKind::Reasoning { .. } => reasoning = true,
            _ => {}
        }
    }
    let plural = |count: usize, one: &str, many: &str| {
        format!("{count} {}", if count == 1 { one } else { many })
    };
    let mut parts = Vec::new();
    if commands > 0 {
        parts.push(format!("Ran {}", plural(commands, "command", "commands")));
    }
    if files > 0 {
        let mut edited = format!("edited {}", plural(files, "file", "files"));
        if additions > 0 || deletions > 0 {
            edited.push_str(&format!(" (+{additions} −{deletions})"));
        }
        parts.push(edited);
    }
    if searches > 0 {
        parts.push(plural(searches, "search", "searches"));
    }
    if tools > 0 {
        parts.push(plural(tools, "tool call", "tool calls"));
    }
    if reasoning && parts.is_empty() {
        parts.push("Thought".to_string());
    }
    let summary = parts.join(" · ");
    let mut chars = summary.chars();
    match chars.next() {
        Some(first) => first.to_uppercase().chain(chars).collect(),
        None => summary,
    }
}

/// One line for a work item: the command, the file, the search.
fn work_label(item: &TimelineItem) -> String {
    match &item.kind {
        ItemKind::Tool { summary, .. } => summary.clone(),
        ItemKind::FileChange {
            path,
            additions,
            deletions,
            changes,
            ..
        } => {
            let mut label = if changes.len() > 1 {
                format!("{path} and {} more", changes.len() - 1)
            } else {
                path.clone()
            };
            if additions.is_some() || deletions.is_some() {
                label.push_str(&format!(
                    "  +{} −{}",
                    additions.unwrap_or_default(),
                    deletions.unwrap_or_default()
                ));
            }
            label
        }
        ItemKind::Reasoning { .. } => "Thinking".to_string(),
        _ => String::new(),
    }
}

/// A small round avatar with a letter, as in the mockups' agent cards.
fn avatar(letter: &str, background: Hsla) -> impl IntoElement {
    div()
        .flex_none()
        .size(px(18.))
        .mt(px(2.))
        .rounded_full()
        .bg(background)
        .flex()
        .items_center()
        .justify_center()
        .text_color(gpui::white())
        .text_size(px(9.5))
        .font_weight(FontWeight::BOLD)
        .child(letter.to_string())
}

impl ThreadView {
    fn agent_letter(&self) -> String {
        self.thread_state()
            .map(|state| state.thread.provider_instance_id.clone())
            .and_then(|provider| provider.chars().next())
            .map_or_else(
                || "A".to_string(),
                |letter| letter.to_uppercase().to_string(),
            )
    }

    fn render_row(
        &mut self,
        index: usize,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let Some(row) = self.rows.get(index).cloned() else {
            return div().into_any_element();
        };
        self.ensure_markdown(&row, cx);
        let Loadable::Loaded(state) = &self.state else {
            return div().into_any_element();
        };
        let selector = match &row {
            Row::Item(index) => format!(
                "thread-row-{}",
                state.item(*index).map_or("", |item| item.id.as_str())
            ),
            Row::Work(indices) => format!(
                "thread-work-{}",
                indices
                    .first()
                    .and_then(|index| state.item(*index))
                    .map_or("", |item| item.id.as_str())
            ),
        };
        let content = match row {
            Row::Item(item_index) => match state.item(item_index).cloned() {
                Some(item) => self.render_item(&item, window, cx),
                None => div().into_any_element(),
            },
            Row::Work(indices) => {
                let items = indices
                    .iter()
                    .filter_map(|index| state.item(*index).cloned())
                    .collect::<Vec<_>>();
                self.render_work(items, window, cx)
            }
        };
        h_flex()
            .w_full()
            .justify_center()
            .px_4()
            .child(
                div()
                    .w_full()
                    .max_w(px(CONTENT_MAX_WIDTH))
                    .py(px(6.))
                    .debug_selector(|| selector)
                    .child(content),
            )
            .into_any_element()
    }

    fn render_markdown(&self, id: &str, muted: bool, window: &Window, cx: &App) -> AnyElement {
        match self.markdown.get(id) {
            Some(markdown) => MarkdownElement::new(
                markdown.clone(),
                if muted {
                    muted_markdown_style(window, cx)
                } else {
                    markdown_style(window, cx)
                },
            )
            .into_any_element(),
            None => div().into_any_element(),
        }
    }

    fn render_item(
        &mut self,
        item: &TimelineItem,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let palette = palette(cx);
        match &item.kind {
            ItemKind::UserMessage { .. } => h_flex()
                .items_start()
                .gap_2()
                .child(avatar("Y", gpui::rgb(0x6e7781).into()))
                .child(
                    div()
                        .flex_1()
                        .min_w_0()
                        .px(px(10.))
                        .py(px(6.))
                        .rounded(tokens::CORNER_RADIUS)
                        .bg(palette.surface_2.hsla())
                        .child(self.render_markdown(&item.id, false, window, cx)),
                )
                .into_any_element(),
            ItemKind::AssistantMessage { streaming, .. } => h_flex()
                .items_start()
                .gap_2()
                .child(avatar(&self.agent_letter(), palette.agent.hsla()))
                .child(
                    v_flex()
                        .flex_1()
                        .min_w_0()
                        .child(self.render_markdown(&item.id, false, window, cx))
                        .when(*streaming, |this| {
                            this.child(
                                div()
                                    .w(px(6.))
                                    .h(px(12.))
                                    .bg(palette.text.hsla())
                                    .opacity(0.7),
                            )
                        }),
                )
                .into_any_element(),
            ItemKind::Plan { title, .. } => Card::new()
                .gap_1()
                .border_color(palette.accent.hsla().opacity(0.45))
                .child(
                    h_flex()
                        .gap_1p5()
                        .child(
                            Icon::new(IconName::ListTodo)
                                .size(IconSize::Small)
                                .color(Color::Accent),
                        )
                        .child(
                            Label::new(title.clone().unwrap_or_else(|| "Plan".to_string()))
                                .size(LabelSize::Small)
                                .weight(FontWeight::SEMIBOLD),
                        ),
                )
                .child(self.render_markdown(&item.id, false, window, cx))
                .into_any_element(),
            ItemKind::Todo { explanation, steps } => {
                let done = steps
                    .iter()
                    .filter(|step| step.status == "completed")
                    .count();
                Card::new()
                    .gap_1()
                    .child(
                        h_flex()
                            .gap_1p5()
                            .child(
                                Icon::new(IconName::ListTodo)
                                    .size(IconSize::Small)
                                    .color(Color::Muted),
                            )
                            .child(
                                Label::new(format!("Tasks {done}/{}", steps.len()))
                                    .size(LabelSize::Small)
                                    .weight(FontWeight::SEMIBOLD),
                            ),
                    )
                    .when_some(explanation.clone(), |this, explanation| {
                        this.child(
                            Label::new(explanation)
                                .size(LabelSize::Small)
                                .color(Color::Muted),
                        )
                    })
                    .children(steps.iter().map(|step| {
                        let (icon, color) = match step.status.as_str() {
                            "completed" => (IconName::TodoComplete, Color::Success),
                            "running" => (IconName::TodoProgress, Color::Accent),
                            _ => (IconName::TodoPending, Color::Muted),
                        };
                        h_flex()
                            .gap_1p5()
                            .items_start()
                            .child(Icon::new(icon).size(IconSize::Small).color(color))
                            .child(
                                Label::new(step.text.clone())
                                    .size(LabelSize::Small)
                                    .when(step.status == "completed", |label| {
                                        label.color(Color::Muted)
                                    }),
                            )
                    }))
                    .into_any_element()
            }
            ItemKind::Approval {
                title,
                prompt,
                request_kind,
                ..
            } => {
                let outcome = match item.status {
                    status if status.is_in_progress() => "waiting for you".to_string(),
                    ItemStatus::Completed => "answered".to_string(),
                    status => status.label().to_string(),
                };
                Card::new()
                    .gap_1()
                    .border_color(palette.warning.hsla().opacity(0.5))
                    .child(
                        h_flex()
                            .gap_1p5()
                            .child(
                                Icon::new(IconName::Warning)
                                    .size(IconSize::Small)
                                    .color(Color::Warning),
                            )
                            .child(
                                Label::new(
                                    title
                                        .clone()
                                        .unwrap_or_else(|| format!("Approval: {request_kind}")),
                                )
                                .size(LabelSize::Small)
                                .weight(FontWeight::SEMIBOLD),
                            )
                            .child(div().flex_1())
                            .child(
                                Label::new(outcome)
                                    .size(LabelSize::Small)
                                    .color(Color::Muted),
                            ),
                    )
                    .when_some(prompt.clone(), |this, prompt| {
                        this.child(
                            div()
                                .px_2()
                                .py_1()
                                .rounded(tokens::CONTROL_RADIUS)
                                .bg(palette.background.hsla())
                                .child(Label::new(prompt).size(LabelSize::Small).buffer_font(cx)),
                        )
                    })
                    .into_any_element()
            }
            ItemKind::Question {
                title, questions, ..
            } => {
                Card::new()
                    .gap_1()
                    .child(
                        h_flex()
                            .gap_1p5()
                            .child(
                                Icon::new(IconName::CircleHelp)
                                    .size(IconSize::Small)
                                    .color(Color::Accent),
                            )
                            .child(
                                Label::new(
                                    title
                                        .clone()
                                        .unwrap_or_else(|| "The agent asked".to_string()),
                                )
                                .size(LabelSize::Small)
                                .weight(FontWeight::SEMIBOLD),
                            ),
                    )
                    .children(questions.iter().map(|question| {
                        Label::new(question.question.clone()).size(LabelSize::Small)
                    }))
                    .into_any_element()
            }
            ItemKind::Error { message } => Card::new()
                .border_color(cx.theme().status().error.opacity(0.6))
                .child(
                    h_flex()
                        .gap_1p5()
                        .items_start()
                        .child(
                            Icon::new(IconName::XCircle)
                                .size(IconSize::Small)
                                .color(Color::Error),
                        )
                        .child(Label::new(message.clone()).size(LabelSize::Small)),
                )
                .into_any_element(),
            ItemKind::Notice { text } => h_flex()
                .gap_2()
                .child(div().flex_1().h(px(1.)).bg(palette.border.hsla()))
                .child(
                    Label::new(text.clone())
                        .size(LabelSize::XSmall)
                        .color(Color::Muted),
                )
                .child(div().flex_1().h(px(1.)).bg(palette.border.hsla()))
                .into_any_element(),
            ItemKind::Tool { .. } | ItemKind::FileChange { .. } | ItemKind::Reasoning { .. } => {
                self.render_work(vec![item.clone()], window, cx)
            }
        }
    }

    fn render_work(
        &mut self,
        items: Vec<TimelineItem>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let palette = palette(cx);
        let Some(first) = items.first() else {
            return div().into_any_element();
        };
        let key = first.id.clone();
        let expanded = self.expanded.contains(&key);
        let item_refs = items.iter().collect::<Vec<_>>();
        let in_progress = items.iter().any(|item| item.status.is_in_progress());
        let failed = items
            .iter()
            .filter(|item| item.status == ItemStatus::Failed)
            .count();
        let selector = format!("thread-work-header-{key}");
        let header = h_flex()
            .id(SharedString::from(format!("work-{key}")))
            .debug_selector(|| selector)
            .gap_1p5()
            .cursor_pointer()
            .on_click(cx.listener(move |this, _, _, cx| this.toggle_expanded(key.clone(), cx)))
            .child(
                Icon::new(if expanded {
                    IconName::ChevronDown
                } else {
                    IconName::ChevronRight
                })
                .size(IconSize::XSmall)
                .color(Color::Muted),
            )
            .child(
                Label::new(work_summary(&item_refs))
                    .size(LabelSize::Small)
                    .color(Color::Muted),
            )
            .when(in_progress, |this| {
                this.child(Indicator::dot().color(Color::Accent)).child(
                    Label::new("working")
                        .size(LabelSize::XSmall)
                        .color(Color::Accent),
                )
            })
            .when(failed > 0, |this| {
                this.child(
                    Label::new(format!("{failed} failed"))
                        .size(LabelSize::XSmall)
                        .color(Color::Error),
                )
            });

        let body = if expanded {
            v_flex()
                .gap_0p5()
                .pl(px(14.))
                .children(
                    items
                        .iter()
                        .map(|item| self.render_work_item(item, window, cx)),
                )
                .into_any_element()
        } else {
            let chips = items
                .iter()
                .filter(|item| !matches!(item.kind, ItemKind::Reasoning { .. }))
                .collect::<Vec<_>>();
            let hidden = chips.len().saturating_sub(COLLAPSED_CHIPS);
            h_flex()
                .flex_wrap()
                .gap(px(6.))
                .pl(px(14.))
                .children(chips.iter().take(COLLAPSED_CHIPS).map(|item| {
                    div()
                        .px(px(7.))
                        .py(px(1.))
                        .rounded_full()
                        .border_1()
                        .border_dashed()
                        .border_color(if item.status == ItemStatus::Failed {
                            cx.theme().status().error.opacity(0.6)
                        } else {
                            palette.border.hsla()
                        })
                        .text_size(tokens::text_size(tokens::TEXT_SIZE_XSMALL))
                        .text_color(palette.text_muted.hsla())
                        .font_buffer(cx)
                        .child(truncate(&work_label(item), CHIP_MAX_CHARS))
                }))
                .when(hidden > 0, |this| {
                    this.child(
                        Label::new(format!("+{hidden} more"))
                            .size(LabelSize::XSmall)
                            .color(Color::Muted),
                    )
                })
                .into_any_element()
        };
        v_flex()
            .gap_1()
            .child(header)
            .child(body)
            .into_any_element()
    }

    fn render_work_item(
        &mut self,
        item: &TimelineItem,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let palette = palette(cx);
        let expanded = self.expanded.contains(&item.id);
        let icon = match &item.kind {
            ItemKind::Tool { tool, .. } => tool_icon(*tool),
            ItemKind::FileChange { .. } => IconName::ToolPencil,
            _ => IconName::ToolThink,
        };
        let detail: Option<AnyElement> = if expanded {
            match &item.kind {
                ItemKind::Tool { input, output, .. } => {
                    let loading = self.loading_outputs.contains(&item.id);
                    let text = [input.clone(), output.clone()]
                        .into_iter()
                        .flatten()
                        .collect::<Vec<_>>()
                        .join("\n\n");
                    Some(
                        div()
                            .ml(px(20.))
                            .px_2()
                            .py_1()
                            .rounded(tokens::CONTROL_RADIUS)
                            .bg(palette.background.hsla())
                            .border_1()
                            .border_color(palette.border.hsla())
                            .text_size(tokens::text_size(tokens::TEXT_SIZE_SMALL))
                            .font_buffer(cx)
                            .text_color(palette.text_muted.hsla())
                            .child(if loading {
                                "Loading…".to_string()
                            } else if text.is_empty() {
                                "No output.".to_string()
                            } else {
                                text
                            })
                            .into_any_element(),
                    )
                }
                ItemKind::FileChange { changes, diff, .. } => Some(
                    v_flex()
                        .ml(px(20.))
                        .gap_0p5()
                        .children(changes.iter().map(|change| {
                            Label::new(format!("{} {}", change.operation, change.path))
                                .size(LabelSize::XSmall)
                                .color(Color::Muted)
                                .buffer_font(cx)
                        }))
                        .when_some(diff.clone(), |this, diff| {
                            this.child(
                                div()
                                    .px_2()
                                    .py_1()
                                    .rounded(tokens::CONTROL_RADIUS)
                                    .bg(palette.background.hsla())
                                    .text_size(tokens::text_size(tokens::TEXT_SIZE_SMALL))
                                    .font_buffer(cx)
                                    .child(diff),
                            )
                        })
                        .into_any_element(),
                ),
                ItemKind::Reasoning { .. } => Some(
                    div()
                        .ml(px(20.))
                        .child(self.render_markdown(&item.id, true, window, cx))
                        .into_any_element(),
                ),
                _ => None,
            }
        } else {
            None
        };
        let item_id = item.id.clone();
        v_flex()
            .gap_0p5()
            .child(
                h_flex()
                    .id(SharedString::from(format!("work-item-{}", item.id)))
                    .debug_selector(|| format!("thread-work-item-{}", item.id))
                    .gap_1p5()
                    .cursor_pointer()
                    .on_click(
                        cx.listener(move |this, _, _, cx| this.toggle_tool(item_id.clone(), cx)),
                    )
                    .child(Icon::new(icon).size(IconSize::XSmall).color(Color::Muted))
                    .child(
                        div()
                            .flex_1()
                            .min_w_0()
                            .overflow_hidden()
                            .whitespace_nowrap()
                            .text_ellipsis()
                            .text_size(tokens::text_size(tokens::TEXT_SIZE_SMALL))
                            .font_buffer(cx)
                            .child(truncate(&work_label(item), 160)),
                    )
                    .child(
                        div()
                            .flex_none()
                            .text_size(tokens::text_size(tokens::TEXT_SIZE_XSMALL))
                            .text_color(status_color(item.status, cx))
                            .child(item.status.label()),
                    ),
            )
            .children(detail)
            .into_any_element()
    }

    fn render_header(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let palette = palette(cx);
        let state = self.thread_state();
        let activity = state.map(ThreadState::activity);
        let mut details: Vec<String> = Vec::new();
        if let Some(state) = state {
            if let Some(branch) = &state.thread.branch {
                details.push(branch.clone());
            }
            if let Some(model) = state.model() {
                details.push(model);
            }
            details.push(crate::model::wire_string(&state.thread.runtime_mode));
        }
        let usage = state.and_then(ThreadState::context_usage);
        h_flex()
            .flex_none()
            .w_full()
            .gap_3()
            .px_4()
            .py(px(8.))
            .border_b(tokens::BORDER_WIDTH)
            .border_color(palette.border.hsla())
            .bg(palette.surface.hsla())
            .child(
                v_flex()
                    .flex_1()
                    .min_w_0()
                    .child(
                        h_flex()
                            .gap_2()
                            .child(
                                Label::new(self.title())
                                    .weight(FontWeight::SEMIBOLD)
                                    .truncate(),
                            )
                            .when_some(activity, |this, activity| {
                                this.child(activity_pill(&activity, cx))
                            })
                            .when(self.connection == Connection::Reconnecting, |this| {
                                this.child(Pill::tinted("reconnecting", palette.warning))
                            }),
                    )
                    .when(!details.is_empty(), |this| {
                        this.child(
                            Label::new(details.join(" · "))
                                .size(LabelSize::Small)
                                .color(Color::Muted)
                                .truncate(),
                        )
                    }),
            )
            .when_some(usage, |this, usage| this.child(render_usage(usage, cx)))
    }

    fn render_pending(
        &self,
        pending: &PendingRequest,
        cx: &mut Context<Self>,
    ) -> impl IntoElement + use<> {
        let palette = palette(cx);
        let request_id = pending.request_id.clone();
        let busy = self.responding.contains(&request_id);
        let blocked = self.write_blocked_reason().is_some() || !pending.can_respond || busy;
        let (title, prompt, kind, options, questions) =
            match pending.item.as_ref().map(|item| &item.kind) {
                Some(ItemKind::Approval {
                    title,
                    prompt,
                    request_kind,
                    options,
                    ..
                }) => (
                    title.clone(),
                    prompt.clone(),
                    Some(request_kind.clone()),
                    options.clone(),
                    Vec::new(),
                ),
                Some(ItemKind::Question {
                    title, questions, ..
                }) => (title.clone(), None, None, Vec::new(), questions.clone()),
                _ => (None, None, None, Vec::new(), Vec::new()),
            };
        let heading = title.unwrap_or_else(|| {
            if pending.is_question {
                "The agent has a question".to_string()
            } else {
                match &kind {
                    Some(kind) => format!("The agent asks to run a {kind} action"),
                    None => "The agent asks for approval".to_string(),
                }
            }
        });

        let mut buttons: Vec<AnyElement> = Vec::new();
        if pending.is_question {
            let answered = self.answers.get(&request_id);
            let complete = questions.iter().all(|question| {
                question.options.is_empty()
                    || answered.is_some_and(|answers| answers.contains_key(&question.id))
            });
            buttons.push(
                TextButton::new(SharedString::from(format!("answer-{request_id}")), "Answer")
                    .style(MustrButtonStyle::Primary)
                    .disabled(blocked || !complete)
                    .on_click(cx.listener({
                        let request_id = request_id.clone();
                        move |this, _, window, cx| this.answer(request_id.clone(), window, cx)
                    }))
                    .into_any_element(),
            );
            buttons.push(
                TextButton::new(
                    SharedString::from(format!("dismiss-{request_id}")),
                    "Dismiss",
                )
                .disabled(blocked)
                .on_click(cx.listener({
                    let request_id = request_id.clone();
                    move |this, _, window, cx| {
                        this.dismiss_questions(request_id.clone(), window, cx)
                    }
                }))
                .into_any_element(),
            );
        } else {
            // T3's web app offers Approve and Decline, plus "for this session" and Cancel, when
            // the agent doesn't list its own options.
            let options = if options.is_empty() {
                vec![
                    (ProviderApprovalDecision::Accept, "Approve".to_string()),
                    (
                        ProviderApprovalDecision::AcceptForSession,
                        "Approve for session".to_string(),
                    ),
                    (ProviderApprovalDecision::Decline, "Decline".to_string()),
                    (ProviderApprovalDecision::Cancel, "Cancel".to_string()),
                ]
            } else {
                options
                    .iter()
                    .map(|option| (option.decision.clone(), option.label.clone()))
                    .collect()
            };
            for (index, (decision, label)) in options.into_iter().enumerate() {
                let primary = decision == ProviderApprovalDecision::Accept;
                let selector = format!("thread-respond-{request_id}-{index}");
                buttons.push(
                    div()
                        .debug_selector(|| selector)
                        .child(
                            TextButton::new(
                                SharedString::from(format!("respond-{request_id}-{index}")),
                                label,
                            )
                            .style(if primary {
                                MustrButtonStyle::Primary
                            } else {
                                MustrButtonStyle::Default
                            })
                            .disabled(blocked)
                            .on_click(cx.listener({
                                let request_id = request_id.clone();
                                move |this, _, window, cx| {
                                    this.respond(request_id.clone(), decision.clone(), window, cx)
                                }
                            })),
                        )
                        .into_any_element(),
                );
            }
        }

        let answers = self.answers.get(&request_id).cloned().unwrap_or_default();
        let selector = format!("thread-pending-{request_id}");
        div().debug_selector(|| selector).child(
            Card::new()
                .gap_1p5()
                .border_color(palette.warning.hsla().opacity(0.6))
                .bg(palette.warning.hsla().opacity(0.06))
                .child(
                    h_flex()
                        .gap_1p5()
                        .child(
                            Icon::new(if pending.is_question {
                                IconName::CircleHelp
                            } else {
                                IconName::Warning
                            })
                            .size(IconSize::Small)
                            .color(Color::Warning),
                        )
                        .child(
                            Label::new(heading)
                                .size(LabelSize::Small)
                                .weight(FontWeight::SEMIBOLD),
                        ),
                )
                .when_some(prompt, |this, prompt| {
                    this.child(
                        div()
                            .px_2()
                            .py_1()
                            .rounded(tokens::CONTROL_RADIUS)
                            .bg(palette.background.hsla())
                            .child(Label::new(prompt).size(LabelSize::Small).buffer_font(cx)),
                    )
                })
                .children(questions.into_iter().map(|question| {
                    let picked = answers.get(&question.id).cloned();
                    v_flex()
                        .gap_1()
                        .child(Label::new(question.question.clone()).size(LabelSize::Small))
                        .child(
                            h_flex().flex_wrap().gap_1().children(
                                question.options.iter().enumerate().map(
                                    |(index, (label, answer))| {
                                        let selected = picked.as_deref() == Some(answer.as_str());
                                        let request_id = request_id.clone();
                                        let question_id = question.id.clone();
                                        let answer = answer.clone();
                                        mustr_ui::Chip::new(
                                            SharedString::from(format!(
                                                "option-{request_id}-{}-{index}",
                                                question.id
                                            )),
                                            label.clone(),
                                        )
                                        .accent(selected)
                                        .on_click(
                                            cx.listener(move |this, _, _, cx| {
                                                this.pick_answer(
                                                    request_id.clone(),
                                                    question_id.clone(),
                                                    answer.clone(),
                                                    cx,
                                                )
                                            }),
                                        )
                                    },
                                ),
                            ),
                        )
                }))
                .child(
                    h_flex()
                        .gap_1p5()
                        .children(buttons)
                        .when(!pending.can_respond, |this| {
                            this.child(
                                Label::new("The agent's session ended, so this can't be answered.")
                                    .size(LabelSize::XSmall)
                                    .color(Color::Muted),
                            )
                        }),
                ),
        )
    }

    fn render_queue(
        &self,
        queued: Vec<QueuedMessage>,
        can_steer: bool,
        cx: &mut Context<Self>,
    ) -> impl IntoElement + use<> {
        let palette = palette(cx);
        let blocked = self.write_blocked_reason().is_some();
        v_flex()
            .gap_1()
            .child(
                Label::new(format!("Queued ({})", queued.len()))
                    .size(LabelSize::XSmall)
                    .color(Color::Muted),
            )
            .children(queued.into_iter().map(|message| {
                let run_id = message.run_id.clone();
                let selector = format!("thread-queued-{run_id}");
                h_flex()
                    .debug_selector(|| selector)
                    .gap_2()
                    .px_2()
                    .py_1()
                    .rounded(tokens::CONTROL_RADIUS)
                    .border_1()
                    .border_dashed()
                    .border_color(palette.border.hsla())
                    .child(
                        Icon::new(IconName::QueueMessage)
                            .size(IconSize::XSmall)
                            .color(Color::Muted),
                    )
                    .child(
                        div()
                            .flex_1()
                            .min_w_0()
                            .overflow_hidden()
                            .whitespace_nowrap()
                            .text_ellipsis()
                            .text_size(tokens::text_size(tokens::TEXT_SIZE_SMALL))
                            .child(truncate(&message.text, 200)),
                    )
                    .child(
                        TextButton::new(
                            SharedString::from(format!("steer-queued-{run_id}")),
                            "Steer now",
                        )
                        .disabled(blocked || !can_steer)
                        .tooltip("Hand this message to the running agent now")
                        .on_click(cx.listener({
                            let run_id = run_id.clone();
                            move |this, _, window, cx| {
                                this.steer_with_queued(run_id.clone(), window, cx)
                            }
                        })),
                    )
                    .child(
                        TextButton::new(
                            SharedString::from(format!("cancel-queued-{run_id}")),
                            "Remove",
                        )
                        .disabled(blocked)
                        .on_click(cx.listener(
                            move |this, _, window, cx| {
                                this.cancel_queued(run_id.clone(), window, cx)
                            },
                        )),
                    )
            }))
    }

    fn render_composer(&self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let palette = palette(cx);
        let state = self.thread_state();
        let running = state.is_some_and(ThreadState::is_running);
        let is_draft = self.new_thread.is_some();
        let pending = state.map(ThreadState::pending_requests).unwrap_or_default();
        let queued = state.map(ThreadState::queued_messages).unwrap_or_default();
        let blocked = self.write_blocked_reason();
        let can_write = blocked.is_none() && (is_draft || state.is_some()) && !self.sending;

        let mut buttons: Vec<AnyElement> = Vec::new();
        if running {
            buttons.push(
                div()
                    .debug_selector(|| "thread-stop".into())
                    .child(
                        TextButton::new("thread-stop", "Stop")
                            .disabled(blocked.is_some())
                            .tooltip("Stop the agent's run")
                            .on_click(cx.listener(|this, _, window, cx| this.stop(window, cx))),
                    )
                    .into_any_element(),
            );
            buttons.push(
                div()
                    .debug_selector(|| "thread-steer".into())
                    .child(
                        TextButton::new("thread-steer", "Steer")
                            .disabled(!can_write)
                            .tooltip("Send to the running agent now")
                            .on_click(cx.listener(|this, _, window, cx| {
                                this.send(Delivery::Steer, window, cx)
                            })),
                    )
                    .into_any_element(),
            );
            buttons.push(
                div()
                    .debug_selector(|| "thread-queue".into())
                    .child(
                        TextButton::new("thread-queue", "Queue")
                            .style(MustrButtonStyle::Primary)
                            .disabled(!can_write)
                            .tooltip("Send when the agent's run finishes")
                            .on_click(cx.listener(|this, _, window, cx| {
                                this.send(Delivery::Queue, window, cx)
                            })),
                    )
                    .into_any_element(),
            );
        } else {
            buttons.push(
                div()
                    .debug_selector(|| "thread-send".into())
                    .child(
                        TextButton::new(
                            "thread-send",
                            if is_draft { "Start thread" } else { "Send" },
                        )
                        .style(MustrButtonStyle::Primary)
                        .disabled(!can_write)
                        .on_click(
                            cx.listener(|this, _, window, cx| {
                                this.send(Delivery::Auto, window, cx)
                            }),
                        ),
                    )
                    .into_any_element(),
            );
        }
        let composer_focus = self.composer.focus_handle(cx);
        let send_binding = ui::KeyBinding::for_action_in(&SendMessage, &composer_focus, cx)
            .size(rems_from_px(10_f32));
        let hint = send_binding.has_binding(window).then(|| {
            h_flex()
                .gap_1()
                .child(send_binding)
                .child(
                    Label::new(if running { "queue" } else { "send" })
                        .size(LabelSize::XSmall)
                        .color(Color::Muted),
                )
                .into_any_element()
        });

        v_flex()
            .flex_none()
            .w_full()
            .items_center()
            .px_4()
            .pt_2()
            .pb_3()
            .border_t(tokens::BORDER_WIDTH)
            .border_color(palette.border.hsla())
            .child(
                v_flex()
                    .w_full()
                    .max_w(px(CONTENT_MAX_WIDTH))
                    .gap_2()
                    .children(
                        pending
                            .iter()
                            .map(|pending| self.render_pending(pending, cx)),
                    )
                    .when(!queued.is_empty(), |this| {
                        this.child(self.render_queue(queued, running, cx))
                    })
                    .when_some(self.error.clone(), |this, error| {
                        this.child(
                            h_flex()
                                .gap_1()
                                .child(
                                    Icon::new(IconName::Warning)
                                        .size(IconSize::XSmall)
                                        .color(Color::Error),
                                )
                                .child(
                                    Label::new(error).size(LabelSize::Small).color(Color::Error),
                                ),
                        )
                    })
                    .child(
                        v_flex()
                            .key_context(COMPOSER_CONTEXT)
                            .on_action(cx.listener(Self::on_send))
                            .on_action(cx.listener(Self::on_steer))
                            .on_action(cx.listener(Self::on_stop))
                            .w_full()
                            .gap_1()
                            .px_2()
                            .py_1p5()
                            .rounded(tokens::CORNER_RADIUS)
                            .border(tokens::BORDER_WIDTH)
                            .border_color(palette.border.hsla())
                            .bg(palette.surface.hsla())
                            .child(self.composer.clone())
                            .child(
                                h_flex()
                                    .gap_1p5()
                                    .children(hint)
                                    .when_some(blocked, |this, blocked| {
                                        this.child(
                                            div()
                                                .id("thread-writes-blocked")
                                                .min_w_0()
                                                .overflow_hidden()
                                                .whitespace_nowrap()
                                                .text_ellipsis()
                                                .text_size(tokens::text_size(
                                                    tokens::TEXT_SIZE_XSMALL,
                                                ))
                                                .text_color(palette.warning.hsla())
                                                .child(blocked.clone())
                                                .tooltip(Tooltip::text(blocked)),
                                        )
                                    })
                                    .child(div().flex_1())
                                    .when(self.sending, |this| {
                                        this.child(
                                            Label::new("Sending…")
                                                .size(LabelSize::XSmall)
                                                .color(Color::Muted),
                                        )
                                    })
                                    .children(buttons),
                            ),
                    ),
            )
    }
}

fn activity_pill(activity: &ThreadActivity, cx: &App) -> Pill {
    let palette = palette(cx);
    let color: Hsla = match activity {
        ThreadActivity::NeedsInput => palette.warning.hsla(),
        ThreadActivity::Working(_) => palette.accent.hsla(),
        ThreadActivity::Failed => cx.theme().status().error,
        ThreadActivity::Idle(_) => palette.text_muted.hsla(),
    };
    Pill::tinted(activity.label(), color)
}

fn render_usage(usage: ContextUsage, cx: &App) -> impl IntoElement {
    let palette = palette(cx);
    let thousands = |tokens: i64| {
        if tokens >= 1000 {
            format!("{}k", (tokens + 500) / 1000)
        } else {
            tokens.to_string()
        }
    };
    let label = match usage.max_tokens {
        Some(max) => format!("{} / {}", thousands(usage.used_tokens), thousands(max)),
        None => thousands(usage.used_tokens),
    };
    let fraction = usage
        .max_tokens
        .filter(|max| *max > 0)
        .map(|max| (usage.used_tokens as f32 / max as f32).clamp(0., 1.));
    v_flex()
        .id("thread-context-usage")
        .flex_none()
        .items_end()
        .gap_0p5()
        .tooltip(Tooltip::text("Tokens in the agent's context"))
        .child(
            Label::new(format!("{label} tokens"))
                .size(LabelSize::XSmall)
                .color(Color::Muted),
        )
        .when_some(fraction, |this, fraction| {
            this.child(
                div()
                    .w(px(96.))
                    .h(px(3.))
                    .rounded_full()
                    .bg(palette.surface_2.hsla())
                    .child(div().h_full().w(relative(fraction)).rounded_full().bg(
                        if fraction > 0.85 {
                            palette.warning.hsla()
                        } else {
                            palette.accent.hsla()
                        },
                    )),
            )
        })
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

    fn show_toolbar(&self) -> bool {
        false
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

//! A thread's composer: the message editor and its buttons, which send, queue, steer, and stop,
//! and above it the agent's pending requests, to approve or answer, and the queued messages.

use crate::{
    attachments::{ComposerFile, ComposerImage, UploadState, has_image_extension},
    backend::ThreadBackendExt as _,
    commands::{self, CommandIds, Delivery, Message, PlanRef},
    model::{GoalLine, ItemKind, PendingRequest, QueuedMessage, ThreadState},
    thread_view::{CONTENT_MAX_WIDTH, Loadable, ThreadView},
    timeline::truncate,
};
use editor::{Editor, EditorEvent};
use gpui::{
    AnyElement, App, AppContext as _, ClipboardEntry, Context, Entity, ExternalPaths,
    Focusable as _, FontWeight, IntoElement, ObjectFit, ParentElement as _, SharedString,
    Styled as _, StyledImage as _, Subscription, Window, actions, div, img, px, relative,
};
use mustr_ui::{
    ButtonStyle as MustrButtonStyle, Card, TextButton, palette, tokens, tokens::scaled,
};
use t3_api::types::ProviderApprovalDecision;
use t3_api::types::ProviderInteractionMode;
use ui::{
    Color, Icon, IconButton, IconName, IconSize, Label, LabelCommon as _, LabelSize, Tooltip,
    prelude::*,
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
        /// Pastes the clipboard's text into the composer as text, even when it's big enough to
        /// go in as a file.
        PasteAsText,
    ]
);

pub(crate) const COMPOSER_CONTEXT: &str = "ThreadComposer";
/// The composer's placeholder while its message would decline an approval (T3's).
const FEEDBACK_PLACEHOLDER: &str = "Send feedback to decline this request";
/// The side of an image's tile in the composer, as T3's (`h-16 w-16`).
const IMAGE_TILE_SIZE: f32 = 64.;
/// A file's tile is as tall, and wide enough for its name.
const FILE_TILE_WIDTH: f32 = 200.;

/// A queued message being dragged to another place in the queue.
#[derive(Clone)]
struct DraggedQueued {
    index: usize,
    text: SharedString,
}

impl gpui::Render for DraggedQueued {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let palette = palette(cx);
        h_flex()
            .gap_2()
            .px_2()
            .py_1()
            .max_w(scaled(px(320.)))
            .rounded(scaled(tokens::CONTROL_RADIUS))
            .border(tokens::BORDER_WIDTH)
            .border_color(palette.accent.hsla())
            .bg(palette.surface.hsla())
            .shadow_md()
            .child(
                Icon::new(IconName::QueueMessage)
                    .size(IconSize::XSmall)
                    .color(Color::Muted),
            )
            .child(
                div()
                    .min_w_0()
                    .overflow_hidden()
                    .whitespace_nowrap()
                    .text_ellipsis()
                    .text_size(tokens::text_size(tokens::TEXT_SIZE_SMALL))
                    .child(self.text.clone()),
            )
    }
}

/// The composer's and timeline's state besides their images.
#[derive(Default)]
pub struct ComposerState {
    /// What was copied last, by the id of its item, to say so on its button.
    pub(crate) copied: Option<String>,
    pub(crate) _copied_reset: Option<gpui::Task<()>>,
    /// The sent prompt ArrowUp recalled, counting back from the newest, and its text.
    pub(crate) recalled: Option<(usize, String)>,
    /// The queued message the composer is editing, by its run's id.
    pub(crate) editing_queued: Option<String>,
    /// Masked inputs for the agent's requests for secrets, by item id.
    pub(crate) secret_inputs: collections::HashMap<String, Entity<Editor>>,
    /// Secret requests being answered, by item id.
    pub(crate) answering_secrets: collections::HashSet<String>,
    /// Running commands' output as it streams, by item id.
    pub(crate) live_outputs: collections::HashMap<String, LiveOutput>,
    pub(crate) output_streams: collections::HashMap<String, gpui::Task<()>>,
    /// The menu of mentions, commands, and skills, and the chips in the text (`mentions.rs`).
    pub(crate) menu: crate::mentions::ComposerMenu,
    /// What the placeholder says, when it isn't the usual: that the message declines an
    /// approval, or answers a question.
    placeholder: Option<String>,
    /// Where the timeline's "new since your last visit" line goes (`timeline.rs`).
    pub(crate) visit: crate::timeline::VisitLine,
    /// What's selected to quote in the composer (`quote.rs`).
    pub(crate) quote: crate::quote::QuoteState,
    /// The composer's answers to the agent's questions (`answers.rs`).
    pub(crate) answering: crate::answers::AnswerState,
    _save_draft: Option<gpui::Task<()>>,
}

/// A running command's output so far (`state/commandOutput.ts` in T3).
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct LiveOutput {
    pub text: String,
    /// T3 dropped the start of a long output.
    pub truncated: bool,
    pub running: bool,
}

impl LiveOutput {
    /// Applies a frame: `replace` starts over, `append` adds to the end.
    pub fn apply(&mut self, frame: &t3_api::types::OrchestrationV2CommandOutputFrame) {
        use t3_api::types::OrchestrationV2CommandOutputFrameKind as Kind;
        if frame.kind == Kind::Replace {
            self.text.clear();
        }
        self.text.push_str(&frame.text);
        self.truncated |= frame.truncated;
        self.running = frame.running;
    }
}

/// Unsent text, by thread, kept while mustr runs and in its database across restarts
/// (`composerDraftStore.ts` in T3), so closing a thread's tab loses nothing.
#[derive(Default)]
struct ComposerDrafts(collections::HashMap<String, String>);

impl gpui::Global for ComposerDrafts {}

/// The records of the threads and pull requests a draft mentions, by thread, kept like its
/// text, so its chips send the same context after a restart.
#[derive(Default)]
struct DraftContexts(collections::HashMap<String, Vec<serde_json::Value>>);

impl gpui::Global for DraftContexts {}

const DRAFT_KEY_PREFIX: &str = "mustr-thread-draft:";
const DRAFT_CONTEXT_KEY_PREFIX: &str = "mustr-thread-draft-context:";
const DRAFT_SAVE_DELAY: std::time::Duration = std::time::Duration::from_millis(500);

/// The draft saved for a thread, if there is one.
pub(crate) fn saved_draft(thread_id: &str, cx: &App) -> Option<String> {
    if let Some(draft) = cx
        .try_global::<ComposerDrafts>()
        .and_then(|drafts| drafts.0.get(thread_id))
    {
        return Some(draft.clone());
    }
    if !cx.has_global::<db::AppDatabase>() {
        return None;
    }
    db::kvp::KeyValueStore::global(cx)
        .read_kvp(&format!("{DRAFT_KEY_PREFIX}{thread_id}"))
        .ok()
        .flatten()
        .filter(|draft| !draft.trim().is_empty())
}

/// The records a thread's draft mentions, as [`saved_draft`] keeps its text.
fn saved_draft_context(thread_id: &str, cx: &App) -> Vec<(String, serde_json::Value)> {
    let records = match cx
        .try_global::<DraftContexts>()
        .and_then(|contexts| contexts.0.get(thread_id))
    {
        Some(records) => records.clone(),
        None if cx.has_global::<db::AppDatabase>() => db::kvp::KeyValueStore::global(cx)
            .read_kvp(&format!("{DRAFT_CONTEXT_KEY_PREFIX}{thread_id}"))
            .ok()
            .flatten()
            .and_then(|records| serde_json::from_str(&records).ok())
            .unwrap_or_default(),
        None => Vec::new(),
    };
    records
        .into_iter()
        .filter_map(|record| Some((record["contextId"].as_str()?.to_string(), record)))
        .collect()
}

impl ThreadView {
    /// Puts back the draft saved for this thread, if the composer is empty. As the view opens
    /// for a thread, it also notes the thread's last visit for the timeline's last-visit line,
    /// before this view's own visit moves it.
    pub(crate) fn restore_draft(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.note_last_visit(cx);
        let Some(thread_id) = self.thread_id.as_deref() else {
            return;
        };
        let Some(draft) = saved_draft(thread_id, cx) else {
            return;
        };
        if self.composer_text(cx).is_empty() {
            self.composer_state.menu.records = saved_draft_context(thread_id, cx);
            self.composer
                .update(cx, |editor, cx| editor.set_text(draft, window, cx));
            self.draw_mention_chips(window, cx);
        }
    }

    /// Keeps the composer's text as the thread's draft; an empty composer forgets it.
    pub(crate) fn save_draft(&mut self, cx: &mut Context<Self>) {
        let Some(thread_id) = self.thread_id.clone() else {
            return;
        };
        let text = self.composer_text(cx);
        let records = crate::mentions::referenced_records(&text, &self.composer_state.menu.records)
            .into_iter()
            .map(|(_, record)| record)
            .collect::<Vec<_>>();
        let contexts = &mut cx.default_global::<DraftContexts>().0;
        if records.is_empty() {
            contexts.remove(&thread_id);
        } else {
            contexts.insert(thread_id.clone(), records.clone());
        }
        let drafts = &mut cx.default_global::<ComposerDrafts>().0;
        let unchanged = drafts.get(&thread_id).map(String::as_str).unwrap_or("") == text;
        if text.trim().is_empty() {
            drafts.remove(&thread_id);
        } else {
            drafts.insert(thread_id.clone(), text.clone());
        }
        if unchanged || !cx.has_global::<db::AppDatabase>() {
            return;
        }
        let store = db::kvp::KeyValueStore::global(cx);
        let key = format!("{DRAFT_KEY_PREFIX}{thread_id}");
        let context_key = format!("{DRAFT_CONTEXT_KEY_PREFIX}{thread_id}");
        // Saved once typing pauses; a newer edit replaces the pending save.
        self.composer_state._save_draft = Some(cx.spawn(async move |_, cx| {
            cx.background_executor().timer(DRAFT_SAVE_DELAY).await;
            let saved = if text.trim().is_empty() {
                store.delete_kvp(key).await
            } else {
                store.write_kvp(key, text).await
            };
            if let Err(error) = saved {
                log::warn!("couldn't save the thread's draft: {error:#}");
            }
            let saved = if records.is_empty() {
                store.delete_kvp(context_key).await
            } else {
                match serde_json::to_string(&records) {
                    Ok(records) => store.write_kvp(context_key, records).await,
                    Err(error) => Err(error.into()),
                }
            };
            if let Err(error) = saved {
                log::warn!("couldn't save what the thread's draft mentions: {error:#}");
            }
        }));
    }

    /// The prompts ArrowUp recalls, oldest first: the thread's user messages, without the ones
    /// that implement a plan, and with repeats in a row merged (`composerPromptHistory.ts`).
    fn prompt_history(&self) -> Vec<String> {
        let Some(state) = self.thread_state() else {
            return Vec::new();
        };
        let mut history: Vec<String> = Vec::new();
        for item in state.items() {
            let ItemKind::UserMessage { text, .. } = &item.kind else {
                continue;
            };
            let text = text.trim();
            if text.is_empty()
                || text.starts_with(commands::PLAN_IMPLEMENTATION_PREFIX)
                || history.last().is_some_and(|last| last == text)
            {
                continue;
            }
            history.push(text.to_string());
        }
        history
    }

    /// The composer's cursor row, and its last row.
    fn cursor_row(&self, cx: &App) -> (u32, u32) {
        let editor = self.composer.read(cx);
        let snapshot = editor.buffer().read(cx).snapshot(cx);
        let head = editor.selections.newest_anchor().head();
        (
            editor::ToPoint::to_point(&head, &snapshot).row,
            snapshot.max_point().row,
        )
    }

    fn show_recalled(
        &mut self,
        recalled: Option<(usize, String)>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let text = recalled
            .as_ref()
            .map(|(_, text)| text.clone())
            .unwrap_or_default();
        self.composer_state.recalled = recalled;
        self.composer.update(cx, |editor, cx| {
            editor.set_text(text, window, cx);
            editor.move_to_end(&editor::actions::MoveToEnd, window, cx);
        });
        self.draw_mention_chips(window, cx);
    }

    /// ArrowUp in an empty composer recalls the last prompt sent, then earlier ones. Like T3, it
    /// works only without images, and from the first line.
    fn on_move_up(
        &mut self,
        _: &zed_actions::editor::MoveUp,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if !self.composer_state.menu.items.is_empty() {
            cx.stop_propagation();
            self.move_menu_highlight(-1, cx);
            return;
        }
        if self.images.has_attachments() || self.cursor_row(cx).0 != 0 {
            return;
        }
        let text = self.composer_text(cx);
        let stepping = self
            .composer_state
            .recalled
            .as_ref()
            .is_some_and(|(_, recalled)| *recalled == text);
        if !stepping && !text.is_empty() {
            return;
        }
        let history = self.prompt_history();
        let next = match &self.composer_state.recalled {
            Some((index, _)) if stepping => index + 1,
            _ => 0,
        };
        let Some(prompt) = history.iter().rev().nth(next) else {
            return;
        };
        cx.stop_propagation();
        self.show_recalled(Some((next, prompt.clone())), window, cx);
    }

    /// ArrowDown steps back toward the newest recalled prompt, then to an empty composer.
    fn on_move_down(
        &mut self,
        _: &zed_actions::editor::MoveDown,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if !self.composer_state.menu.items.is_empty() {
            cx.stop_propagation();
            self.move_menu_highlight(1, cx);
            return;
        }
        let (row, last_row) = self.cursor_row(cx);
        let text = self.composer_text(cx);
        let Some((index, recalled)) = self.composer_state.recalled.clone() else {
            return;
        };
        if recalled != text || row != last_row {
            return;
        }
        cx.stop_propagation();
        let recalled = index.checked_sub(1).and_then(|previous| {
            self.prompt_history()
                .iter()
                .rev()
                .nth(previous)
                .map(|prompt| (previous, prompt.clone()))
        });
        self.show_recalled(recalled, window, cx);
    }

    /// Tab picks the menu's highlighted item, as Enter does.
    fn on_tab(&mut self, _: &editor::actions::Tab, window: &mut Window, cx: &mut Context<Self>) {
        if self.composer_state.menu.highlighted_item().is_some() {
            cx.stop_propagation();
            self.pick_highlighted(window, cx);
        }
    }

    /// Escape closes the menu until the word that opened it changes.
    fn on_cancel(&mut self, _: &editor::actions::Cancel, _: &mut Window, cx: &mut Context<Self>) {
        if self.composer_state.menu.is_open() {
            cx.stop_propagation();
            self.dismiss_menu(cx);
        }
    }
}

impl ThreadView {
    /// Copies text, and says so on the button of `item_id` for a moment.
    pub(crate) fn copy_text(&mut self, item_id: String, text: String, cx: &mut Context<Self>) {
        cx.write_to_clipboard(gpui::ClipboardItem::new_string(text));
        self.composer_state.copied = Some(item_id.clone());
        self.composer_state._copied_reset = Some(cx.spawn(async move |this, cx| {
            cx.background_executor()
                .timer(std::time::Duration::from_secs(2))
                .await;
            this.update(cx, |this, cx| {
                if this.composer_state.copied.as_deref() == Some(item_id.as_str()) {
                    this.composer_state.copied = None;
                    cx.notify();
                }
            })
            .ok();
        }));
        cx.notify();
    }
}

/// Redraws the view as the composer's text changes, since its buttons depend on whether
/// there's anything to send.
pub(crate) fn observe_composer(
    composer: &Entity<Editor>,
    cx: &mut Context<ThreadView>,
) -> Subscription {
    cx.subscribe(composer, |this, _, event: &EditorEvent, cx| {
        if matches!(event, EditorEvent::SelectionsChanged { .. }) {
            this.refresh_menu(cx);
        }
        if matches!(event, EditorEvent::BufferEdited { .. }) {
            this.refresh_menu(cx);
            this.save_draft(cx);
            let text = this.composer_text(cx);
            if this
                .composer_state
                .recalled
                .as_ref()
                .is_some_and(|(_, recalled)| *recalled != text)
            {
                this.composer_state.recalled = None;
            }
            cx.notify();
        }
    })
}

pub(crate) const COMPOSER_MIN_LINES: usize = 2;
pub(crate) const COMPOSER_MAX_LINES: usize = 12;

impl ThreadView {
    fn composer_text(&self, cx: &App) -> String {
        self.composer.read(cx).text(cx)
    }

    fn write_blocked_reason(&self) -> Option<String> {
        self.backend.write_blocked_reason()
    }

    pub(crate) fn send(&mut self, delivery: Delivery, window: &mut Window, cx: &mut Context<Self>) {
        if let Some(run_id) = self.composer_state.editing_queued.clone() {
            self.save_queued_edit(run_id, window, cx);
            return;
        }
        let text = self.composer_text(cx);
        // While the agent waits on an approval that takes a reason, the message declines it and
        // tells the agent why, as T3's composer does.
        if let Some(request_id) = self.feedback_target()
            && !text.trim().is_empty()
        {
            self.decline_with_feedback(request_id, text, window, cx);
            return;
        }
        // While the agent waits on a question, the message answers it (`answers.rs`).
        if self.answer_with_composer(window, cx) {
            return;
        }
        let has_images = self.images.has_attachments();
        if (text.trim().is_empty() && !has_images) || self.sending {
            return;
        }
        let context = self.mention_context(&text);
        // `/plan` or `/default` alone switches the mode instead of sending.
        if !has_images
            && context.is_none()
            && let Some(command) = crate::slash_commands::standalone_mode_command(&text)
        {
            self.switch_interaction_mode(command, cx);
            self.composer
                .update(cx, |editor, cx| editor.clear(window, cx));
            cx.notify();
            return;
        }
        // Like T3's Send: it waits for images still going up, and retries failed ones first.
        if self.images.uploading() {
            self.images.send_when_uploaded = Some(delivery);
            cx.notify();
            return;
        }
        if self.images.any_failed() {
            self.retry_failed_then_send(delivery, window, cx);
            return;
        }
        if let Some(form) = self.new_thread.clone() {
            self.launch(form, text, window, cx);
            return;
        }
        let Loadable::Loaded(state) = &self.state else {
            return;
        };
        let thread_id = state.thread_id().to_string();
        let sent_images = self.images.uploaded_keys();
        let message = Message {
            attachments: self.images.attachments(),
            context,
            ..Message::text(text.trim())
        };
        let command = commands::message(&thread_id, message, delivery, CommandIds::new());
        let dispatch = self.backend.dispatch(command);
        self.sending = true;
        self.error = None;
        cx.notify();
        self._tasks.push(cx.spawn_in(window, async move |this, cx| {
            let result = dispatch.await;
            this.update_in(cx, |this, window, cx| {
                this.sending = false;
                match result {
                    Ok(_) => {
                        this.composer
                            .update(cx, |editor, cx| editor.clear(window, cx));
                        this.images.clear_sent(&sent_images);
                    }
                    Err(error) => this.error = Some(format!("{error:#}").into()),
                }
                cx.notify();
            })
            .ok();
        }));
    }

    /// The approval the composer's message declines with a reason, if there is one: the first
    /// pending approval, when its agent takes feedback (`supportsFeedback`, T3 0.1.3). Older
    /// servers never offer it, so their composer sends as usual.
    pub(crate) fn feedback_target(&self) -> Option<String> {
        self.thread_state()?
            .pending_requests()
            .into_iter()
            .find(|pending| !pending.is_question)
            .filter(|pending| pending.supports_feedback && pending.can_respond)
            .map(|pending| pending.request_id)
    }

    /// Declines the approval with the composer's text as the reason (`runtime-request.respond`
    /// with `feedback`), then clears the composer. Like T3, it takes text alone: attachments and
    /// chips would be lost, so they keep the message from going.
    fn decline_with_feedback(
        &mut self,
        request_id: String,
        text: String,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(thread_id) = self.thread_id.clone() else {
            return;
        };
        if self.sending || self.responding.contains(&request_id) {
            return;
        }
        if !self.images.is_empty() || self.mention_context(&text).is_some() {
            self.error =
                Some("Send approval feedback without attachments or added context.".into());
            cx.notify();
            return;
        }
        let command =
            commands::decline_with_feedback(&thread_id, &request_id, &text, commands::new_id());
        let dispatch = self.backend.dispatch(command);
        self.responding.insert(request_id.clone());
        self.sending = true;
        self.error = None;
        cx.notify();
        self._tasks.push(cx.spawn_in(window, async move |this, cx| {
            let result = dispatch.await;
            this.update_in(cx, |this, window, cx| {
                this.sending = false;
                this.responding.remove(&request_id);
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

    /// The composer's placeholder follows what its message would do: while it would decline an
    /// approval, it says so, as T3's does.
    fn sync_placeholder(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let placeholder = if self.feedback_target().is_some() {
            Some(FEEDBACK_PLACEHOLDER.to_string())
        } else {
            self.answer_placeholder()
        };
        if placeholder == self.composer_state.placeholder {
            return;
        }
        self.composer_state.placeholder = placeholder.clone();
        let placeholder = placeholder.unwrap_or_else(|| "Message the agent…".to_string());
        self.composer.update(cx, |editor, cx| {
            editor.set_placeholder_text(&placeholder, window, cx)
        });
    }

    /// The masked input for a secret request, made the first time it's shown.
    pub(crate) fn secret_input(
        &mut self,
        item_id: &str,
        placeholder: Option<&str>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Entity<Editor> {
        if let Some(input) = self.composer_state.secret_inputs.get(item_id) {
            return input.clone();
        }
        let placeholder = placeholder.unwrap_or("Paste the secret").to_string();
        let input = cx.new(|cx| {
            let mut editor = Editor::single_line(window, cx);
            editor.set_masked(true, cx);
            editor.set_placeholder_text(&placeholder, window, cx);
            editor
        });
        self.composer_state
            .secret_inputs
            .insert(item_id.to_string(), input.clone());
        input
    }

    /// Answers the agent's request for a secret (`secrets.answerRequest`): saves what's in its
    /// input, or declines. The secret goes only to T3; mustr never logs or keeps it.
    pub(crate) fn answer_secret(
        &mut self,
        item_id: String,
        save: bool,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(thread_id) = self.thread_id.clone() else {
            return;
        };
        let secret = if save {
            let Some(input) = self.composer_state.secret_inputs.get(&item_id) else {
                return;
            };
            let secret = input.read(cx).text(cx);
            if secret.trim().is_empty() {
                return;
            }
            Some(secret.trim().to_string())
        } else {
            None
        };
        if !self
            .composer_state
            .answering_secrets
            .insert(item_id.clone())
        {
            return;
        }
        let answer = self.backend.call::<t3_api::rpc::SecretsAnswerRequest>(
            commands::answer_secret(&thread_id, &item_id, secret),
            crate::backend::Access::Operate,
        );
        self.error = None;
        cx.notify();
        self._tasks.push(cx.spawn_in(window, async move |this, cx| {
            let result = answer.await;
            this.update_in(cx, |this, window, cx| {
                this.composer_state.answering_secrets.remove(&item_id);
                match result {
                    Ok(()) => {
                        if let Some(input) = this.composer_state.secret_inputs.get(&item_id) {
                            input.update(cx, |editor, cx| editor.clear(window, cx));
                        }
                    }
                    Err(error) => this.error = Some(format!("{error:#}").into()),
                }
                cx.notify();
            })
            .ok();
        }));
    }

    /// Opens a queued message in the composer to change it (`queuedMessageEdit.ts` in T3).
    pub(crate) fn edit_queued(
        &mut self,
        message: QueuedMessage,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.composer_state.editing_queued = Some(message.run_id);
        self.composer.update(cx, |editor, cx| {
            editor.set_text(message.text, window, cx);
            editor.move_to_end(&editor::actions::MoveToEnd, window, cx);
        });
        self.draw_mention_chips(window, cx);
        window.focus(&self.composer.focus_handle(cx), cx);
        cx.notify();
    }

    pub(crate) fn cancel_queued_edit(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.composer_state.editing_queued.take().is_some() {
            self.composer
                .update(cx, |editor, cx| editor.clear(window, cx));
        }
        cx.notify();
    }

    /// Saves the composer's text as the queued message's (`queued-run.edit`).
    fn save_queued_edit(&mut self, run_id: String, window: &mut Window, cx: &mut Context<Self>) {
        let Some(thread_id) = self.thread_id.clone() else {
            return;
        };
        let text = self.composer_text(cx);
        if text.trim().is_empty() {
            return;
        }
        let still_queued = self.thread_state().is_some_and(|state| {
            state
                .queued_messages()
                .iter()
                .any(|queued| queued.run_id == run_id)
        });
        if !still_queued {
            self.composer_state.editing_queued = None;
            self.error = Some("That message isn't queued anymore; send this as a new one.".into());
            cx.notify();
            return;
        }
        let command = commands::edit_queued(&thread_id, &run_id, text.trim(), commands::new_id());
        self.dispatch_in_order(
            vec![command],
            window,
            cx,
            |this, result, window, cx| match result {
                Ok(()) => {
                    this.composer_state.editing_queued = None;
                    this.composer
                        .update(cx, |editor, cx| editor.clear(window, cx));
                }
                Err(error) => this.error = Some(format!("{error:#}").into()),
            },
        );
    }

    /// Sends commands one after another, stopping at the first T3 refuses, then calls `done`
    /// with how it went. The composer is busy until then.
    pub(crate) fn dispatch_in_order(
        &mut self,
        commands: Vec<t3_api::tagged::OrchestrationV2Command>,
        window: &mut Window,
        cx: &mut Context<Self>,
        done: impl FnOnce(&mut Self, anyhow::Result<()>, &mut Window, &mut Context<Self>) + 'static,
    ) {
        if self.sending {
            return;
        }
        let backend = self.backend.clone();
        self.sending = true;
        self.error = None;
        cx.notify();
        self._tasks.push(cx.spawn_in(window, async move |this, cx| {
            let mut result = Ok(());
            for command in commands {
                if let Err(error) = backend.dispatch(command).await {
                    result = Err(error);
                    break;
                }
            }
            this.update_in(cx, |this, window, cx| {
                this.sending = false;
                done(this, result, window, cx);
                cx.notify();
            })
            .ok();
        }));
    }

    /// Resume, in an empty composer: continues the latest run if it was interrupted or hit a
    /// usage limit, and restarts a held queue (`useComposerResume.ts` in T3).
    pub(crate) fn resume(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(state) = self.thread_state() else {
            return;
        };
        let Some(resumable) = state.resumable() else {
            return;
        };
        let thread_id = state.thread_id().to_string();
        let mut commands = Vec::new();
        if let Some(run_id) = &resumable.continue_run_id {
            commands.push(commands::continue_run(
                &thread_id,
                run_id,
                Some(state.thread.model_selection.clone()),
                CommandIds::new(),
            ));
        }
        if resumable.held_queue {
            commands.push(commands::resume_queue(&thread_id, commands::new_id()));
        }
        self.dispatch_in_order(commands, window, cx, |this, result, _, _| {
            if let Err(error) = result {
                this.error = Some(format!("{error:#}").into());
            }
        });
    }

    /// Implements the thread's actionable plan, in this thread or a new one
    /// (`useProposedPlanSend.ts` in T3).
    pub(crate) fn implement_plan(
        &mut self,
        in_new_thread: bool,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(state) = self.thread_state() else {
            return;
        };
        let Some(plan) = state.actionable_plan() else {
            return;
        };
        let thread = &state.thread;
        let text = commands::plan_implementation_text(&plan.markdown);
        let source_plan = PlanRef {
            thread_id: thread.id.clone(),
            plan_id: plan.plan_id.clone(),
        };
        if !in_new_thread {
            let mut commands = Vec::new();
            // T3 switches the thread out of plan mode first, with its own command.
            if thread.interaction_mode != ProviderInteractionMode::Default {
                commands.push(commands::set_interaction_mode(
                    &thread.id,
                    ProviderInteractionMode::Default,
                    commands::new_id(),
                ));
            }
            commands.push(commands::message(
                &thread.id,
                Message {
                    model_selection: Some(thread.model_selection.clone()),
                    source_plan: Some(source_plan),
                    title_seed: Some(thread.title.clone()),
                    ..Message::text(text)
                },
                Delivery::Auto,
                CommandIds::new(),
            ));
            self.dispatch_in_order(commands, window, cx, |this, result, _, _| {
                if let Err(error) = result {
                    this.error = Some(format!("{error:#}").into());
                }
            });
            return;
        }

        let new_thread_id = commands::new_id();
        let title = commands::plan_thread_title(&plan.markdown);
        let create = commands::create_plan_thread(
            commands::PlanThread {
                project_id: thread.project_id.clone(),
                title: title.clone(),
                model_selection: thread.model_selection.clone(),
                runtime_mode: thread.runtime_mode.clone(),
                branch: thread.branch.clone(),
                worktree_path: thread.worktree_path.clone(),
            },
            &new_thread_id,
            commands::new_id(),
        );
        let first_message = commands::message(
            &new_thread_id,
            Message {
                model_selection: Some(thread.model_selection.clone()),
                source_plan: Some(source_plan),
                title_seed: Some(title.clone()),
                ..Message::text(text)
            },
            Delivery::Auto,
            CommandIds::new(),
        );
        let created = new_thread_id;
        self.dispatch_in_order(vec![create], window, cx, move |this, result, window, cx| {
            if let Err(error) = result {
                this.error =
                    Some(format!("Could not start the implementation thread: {error:#}").into());
                return;
            }
            this.dispatch_in_order(
                vec![first_message],
                window,
                cx,
                move |this, result, window, cx| {
                    match result {
                        Ok(()) => this.open_other_thread(created, title, window, cx),
                        Err(error) => {
                            // As T3 does, don't leave an empty thread behind.
                            this.error = Some(
                                format!("Could not start the implementation thread: {error:#}")
                                    .into(),
                            );
                            let delete = this
                                .backend
                                .dispatch(commands::delete_thread(&created, commands::new_id()));
                            cx.background_spawn(async move {
                                if let Err(error) = delete.await {
                                    log::warn!("couldn't delete the unstarted thread: {error:#}");
                                }
                            })
                            .detach();
                            let _ = window;
                        }
                    }
                },
            );
        });
    }

    /// Opens another thread's view in this view's workspace.
    fn open_other_thread(
        &mut self,
        thread_id: String,
        title: String,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(workspace) = workspace::Workspace::for_window(window, cx) else {
            return;
        };
        crate::open_thread(thread_id, title.into(), workspace.downgrade(), window, cx);
    }

    /// Refine: asks for what to change, and the next message goes in plan mode.
    pub(crate) fn refine_plan(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.composer.update(cx, |editor, cx| {
            editor.set_placeholder_text("Describe how to change the plan…", window, cx);
        });
        window.focus(&self.composer.focus_handle(cx), cx);
        cx.notify();
    }

    /// Takes a paste as T3's composer does (`shouldHandleComposerAttachmentPaste` and
    /// `foldPastedText`): images are attached, and so are copied files unless text comes with
    /// them; text of 32 KiB or more goes in as a file; anything else goes to the editor.
    fn on_paste(
        &mut self,
        _: &editor::actions::Paste,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(clipboard) = cx.read_from_clipboard() else {
            return;
        };
        let mut images = Vec::new();
        let mut paths = Vec::new();
        let mut text = String::new();
        for entry in clipboard.entries() {
            match entry {
                ClipboardEntry::Image(image) if !image.bytes.is_empty() => {
                    images.push(image.clone())
                }
                ClipboardEntry::ExternalPaths(external) => {
                    paths.extend(external.paths().iter().cloned())
                }
                ClipboardEntry::String(string) => text.push_str(string.text()),
                _ => {}
            }
        }
        let attach_paths = !paths.is_empty()
            && (text.is_empty() || paths.iter().any(|path| has_image_extension(path)));
        if !images.is_empty() || attach_paths {
            cx.stop_propagation();
            if !images.is_empty() {
                self.add_clipboard_images(images, window, cx);
            }
            if attach_paths {
                self.add_paths(paths, window, cx);
            }
            return;
        }
        if self.fold_big_paste(&text, window, cx) {
            cx.stop_propagation();
        }
    }

    /// Pastes the clipboard's text as text, however big (T3's `mod+shift+v`).
    fn on_paste_as_text(&mut self, _: &PasteAsText, window: &mut Window, cx: &mut Context<Self>) {
        let Some(text) = cx
            .read_from_clipboard()
            .and_then(|clipboard| clipboard.text())
        else {
            return;
        };
        self.composer.update(cx, |editor, cx| {
            editor.insert(&text, window, cx);
        });
    }

    fn on_drop_paths(
        &mut self,
        paths: &ExternalPaths,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.add_paths(paths.paths().to_vec(), window, cx);
    }

    pub(crate) fn dispatch(
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
                        this.composer_state.answering.forget(request_id);
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

    /// Answers a question request, with what's in the composer as the answer to the question
    /// it's on (`answers.rs`).
    fn answer(&mut self, request_id: String, window: &mut Window, cx: &mut Context<Self>) {
        let questions = self
            .thread_state()
            .and_then(|state| {
                state
                    .pending_requests()
                    .into_iter()
                    .find(|pending| pending.request_id == request_id)
            })
            .and_then(|pending| match pending.item.map(|item| item.kind) {
                Some(ItemKind::Question { questions, .. }) => Some(questions),
                _ => None,
            })
            .unwrap_or_default();
        self.answer_request(request_id, questions, window, cx);
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

    /// Moves a queued message from `from` to `to` in the queue (`queued-run.reorder`), as
    /// dragging it or its arrows do: before the message now at `to`, or last.
    pub(crate) fn move_queued(
        &mut self,
        from: usize,
        to: usize,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(state) = self.thread_state() else {
            return;
        };
        let queued = state.queued_messages();
        if from == to || from >= queued.len() || to >= queued.len() {
            return;
        }
        // Moving down lands after the target, so before the one after it.
        let before = if to > from {
            queued.get(to + 1)
        } else {
            queued.get(to)
        };
        let command = commands::reorder_queued(
            state.thread_id(),
            &queued[from].run_id,
            before.map(|message| message.run_id.as_str()),
            commands::new_id(),
        );
        self.dispatch(command, None, window, cx);
    }

    /// Send now: starts a queued message's run while nothing runs (`queued-run.start`, T3
    /// 0.1.3).
    pub(crate) fn start_queued(
        &mut self,
        run_id: String,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(thread_id) = self.thread_id.clone() else {
            return;
        };
        let command = commands::start_queued(&thread_id, &run_id, commands::new_id());
        self.dispatch(command, None, window, cx);
    }

    /// Sends one of T3's `/goal` commands, as its goal line's Resume and Clear do.
    fn send_goal_command(&mut self, text: &str, window: &mut Window, cx: &mut Context<Self>) {
        let Some(state) = self.thread_state() else {
            return;
        };
        let command = commands::message(
            state.thread_id(),
            Message {
                model_selection: Some(state.thread.model_selection.clone()),
                ..Message::text(text)
            },
            Delivery::Auto,
            CommandIds::new(),
        );
        self.dispatch(command, None, window, cx);
    }

    /// The goal the agent works toward, above the composer as T3 shows it at the end of the
    /// timeline: its status, what it has used, its objective, and Resume and Clear.
    fn render_goal(&self, cx: &mut Context<Self>) -> Option<AnyElement> {
        let state = self.thread_state()?;
        let goal = GoalLine::new(state.goal()?, state.is_running());
        let palette = palette(cx);
        let blocked = self.write_blocked_reason().is_some() || self.sending;
        let title = match &goal.usage {
            Some(usage) => format!("{} · {usage}", goal.title),
            None => goal.title.clone(),
        };
        Some(
            h_flex()
                .debug_selector(|| "thread-goal".into())
                .gap_2()
                .px_2()
                .py_1()
                .rounded(scaled(tokens::CONTROL_RADIUS))
                .border(tokens::BORDER_WIDTH)
                .border_color(palette.border.hsla())
                .bg(palette.surface.hsla())
                .child(
                    Icon::new(IconName::Crosshair)
                        .size(IconSize::XSmall)
                        .color(Color::Accent),
                )
                .child(
                    v_flex()
                        .flex_1()
                        .min_w_0()
                        .child(
                            Label::new(title)
                                .size(LabelSize::Small)
                                .weight(FontWeight::MEDIUM),
                        )
                        .when(!goal.objective.is_empty(), |this| {
                            this.child(
                                div()
                                    .min_w_0()
                                    .overflow_hidden()
                                    .whitespace_nowrap()
                                    .text_ellipsis()
                                    .text_size(tokens::text_size(tokens::TEXT_SIZE_XSMALL))
                                    .text_color(palette.text_muted.hsla())
                                    .child(goal.objective.clone()),
                            )
                        }),
                )
                // Resume would stop unrelated work, so it waits for the run to end, as T3's does.
                .when(goal.can_resume && !state.is_running(), |this| {
                    this.child(
                        div().debug_selector(|| "thread-goal-resume".into()).child(
                            TextButton::new("thread-goal-resume", "Resume")
                                .disabled(blocked)
                                .tooltip("Work toward the goal again (/goal resume)")
                                .on_click(cx.listener(|this, _, window, cx| {
                                    this.send_goal_command("/goal resume", window, cx)
                                })),
                        ),
                    )
                })
                .child(
                    div().debug_selector(|| "thread-goal-clear".into()).child(
                        TextButton::new("thread-goal-clear", "Clear")
                            .disabled(blocked)
                            .tooltip("Stop working toward the goal (/goal clear)")
                            .on_click(cx.listener(|this, _, window, cx| {
                                this.send_goal_command("/goal clear", window, cx)
                            })),
                    ),
                )
                .into_any_element(),
        )
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

    fn on_send(&mut self, _: &SendMessage, window: &mut Window, cx: &mut Context<Self>) {
        // While the menu shows something, Enter picks it (`composerCommandKey.ts` in T3).
        if self.composer_state.menu.highlighted_item().is_some() {
            self.pick_highlighted(window, cx);
            return;
        }
        let running = self.thread_state().is_some_and(|state| state.is_running());
        self.send(
            if running {
                crate::thread_settings::FollowUpBehavior::get(cx).delivery()
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
                crate::thread_settings::FollowUpBehavior::get(cx).other_delivery()
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

impl ThreadView {
    fn render_pending(
        &self,
        pending: &PendingRequest,
        cx: &mut Context<Self>,
    ) -> impl IntoElement + use<> {
        let palette = palette(cx);
        let request_id = pending.request_id.clone();
        let busy = self.responding.contains(&request_id);
        let blocked = self.write_blocked_reason().is_some() || !pending.can_respond || busy;
        let mut app_name = None;
        let mut reason = None;
        let (title, prompt, kind, options, questions) =
            match pending.item.as_ref().map(|item| &item.kind) {
                Some(ItemKind::Approval {
                    title,
                    prompt,
                    request_kind,
                    options,
                    app_name: app,
                    reason: why,
                    ..
                }) => {
                    app_name = app.clone();
                    reason = why.clone();
                    (
                        title.clone(),
                        prompt.clone(),
                        Some(request_kind.clone()),
                        options.clone(),
                        Vec::new(),
                    )
                }
                Some(ItemKind::Question {
                    title, questions, ..
                }) => (title.clone(), None, None, Vec::new(), questions.clone()),
                _ => (None, None, None, Vec::new(), Vec::new()),
            };
        // An MCP server's question reads as prose, a command or a path as code (T3's panel).
        let elicitation = kind.as_deref() == Some("mcp-elicitation");
        let heading = match (pending.is_question, &kind) {
            (true, _) => title.unwrap_or_else(|| "The agent has a question".to_string()),
            (false, Some(kind)) => crate::answers::approval_label(kind).to_string(),
            (false, None) => title.unwrap_or_else(|| "The agent asks for approval".to_string()),
        };

        let mut buttons: Vec<AnyElement> = Vec::new();
        if pending.is_question {
            let complete = self.answers_complete(&request_id, &questions, cx);
            buttons.push(
                div()
                    .debug_selector({
                        let request_id = request_id.clone();
                        move || format!("thread-answer-{request_id}")
                    })
                    .child(
                        TextButton::new(
                            SharedString::from(format!("answer-{request_id}")),
                            "Answer",
                        )
                        .style(MustrButtonStyle::Primary)
                        .disabled(blocked || !complete)
                        .on_click(cx.listener({
                            let request_id = request_id.clone();
                            move |this, _, window, cx| this.answer(request_id.clone(), window, cx)
                        })),
                    )
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
                    (
                        ProviderApprovalDecision::Accept,
                        "Approve".to_string(),
                        None,
                    ),
                    (
                        ProviderApprovalDecision::AcceptForSession,
                        "Approve for session".to_string(),
                        None,
                    ),
                    (
                        ProviderApprovalDecision::Decline,
                        "Decline".to_string(),
                        None,
                    ),
                    (ProviderApprovalDecision::Cancel, "Cancel".to_string(), None),
                ]
            } else {
                options
                    .iter()
                    .map(|option| {
                        (
                            option.decision.clone(),
                            option.label.clone(),
                            option.warning.clone(),
                        )
                    })
                    .collect()
            };
            for (index, (decision, label, warning)) in options.into_iter().enumerate() {
                let primary = decision == ProviderApprovalDecision::Accept;
                let selector = format!("thread-respond-{request_id}-{index}");
                let button = TextButton::new(
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
                }));
                buttons.push(
                    h_flex()
                        .debug_selector(|| selector)
                        .gap_0p5()
                        .when_some(warning.clone(), |this, warning| {
                            this.child(
                                div()
                                    .id(SharedString::from(format!(
                                        "respond-warning-{request_id}-{index}"
                                    )))
                                    .child(
                                        Icon::new(IconName::Warning)
                                            .size(IconSize::XSmall)
                                            .color(Color::Warning),
                                    )
                                    .tooltip(Tooltip::text(warning)),
                            )
                        })
                        .child(match warning {
                            Some(warning) => button.tooltip(warning),
                            None => button,
                        })
                        .into_any_element(),
                );
            }
        }

        let selector = format!("thread-pending-{request_id}");
        div().debug_selector(|| selector).child(
            Card::new()
                .gap_1p5()
                .border_color(palette.warning.hsla().opacity(0.6))
                .bg(palette.warning.hsla().opacity(0.06))
                .child(
                    h_flex()
                        .gap_1p5()
                        .min_w_0()
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
                        )
                        .when_some(app_name, |this, app_name| {
                            this.child(
                                Label::new(app_name)
                                    .size(LabelSize::Small)
                                    .color(Color::Muted)
                                    .truncate(),
                            )
                        }),
                )
                .when_some(prompt, |this, prompt| {
                    this.child(
                        div()
                            .px_2()
                            .py_1()
                            .rounded(scaled(tokens::CONTROL_RADIUS))
                            .bg(palette.background.hsla())
                            .child(if elicitation {
                                Label::new(prompt).size(LabelSize::Small)
                            } else {
                                Label::new(prompt).size(LabelSize::Small).buffer_font(cx)
                            }),
                    )
                })
                .when_some(reason, |this, reason| {
                    this.child(
                        Label::new(reason)
                            .size(LabelSize::XSmall)
                            .color(Color::Muted),
                    )
                })
                .children(questions.iter().enumerate().map(|(index, question)| {
                    self.render_question(&request_id, index, question, cx)
                }))
                .when(pending.supports_feedback && pending.can_respond, |this| {
                    this.child(
                        Label::new(
                            "Or write below why to decline it, and the agent gets your reason.",
                        )
                        .size(LabelSize::XSmall)
                        .color(Color::Muted),
                    )
                })
                .child(h_flex().flex_wrap().gap_1p5().children(buttons).when(
                    !pending.can_respond,
                    |this| {
                        this.child(
                            Label::new("The agent's session ended, so this can't be answered.")
                                .size(LabelSize::XSmall)
                                .color(Color::Muted),
                        )
                    },
                )),
        )
    }

    /// The queued messages, in the order they'll run, as T3's "Up next" shows them: each can be
    /// edited, removed, steered into the running agent, or, while nothing runs on a server that
    /// has it, sent now; and when the agent takes its queue in another order, dragged or moved
    /// with its arrows.
    fn render_queue(
        &mut self,
        queued: Vec<QueuedMessage>,
        running: bool,
        cx: &mut Context<Self>,
    ) -> impl IntoElement + use<> {
        let palette = palette(cx);
        let blocked = self.write_blocked_reason().is_some();
        let state = self.thread_state();
        let can_reorder = queued.len() > 1 && state.is_some_and(ThreadState::can_reorder_queue);
        let can_start = state.is_some_and(ThreadState::can_start_queued);
        if can_start {
            // Whether the server has `queued-run.start` is in its config.
            self.load_server_info(false, cx);
        }
        let can_start = can_start && self.composer_state.menu.server_has_queued_start();
        let count = queued.len();
        v_flex()
            .gap_1()
            .child(
                Label::new(format!("Queued ({count})"))
                    .size(LabelSize::XSmall)
                    .color(Color::Muted),
            )
            .children(queued.into_iter().enumerate().map(|(index, message)| {
                let run_id = message.run_id.clone();
                let selector = format!("thread-queued-{run_id}");
                let dragged = DraggedQueued {
                    index,
                    text: truncate(&message.text, 80).into(),
                };
                h_flex()
                    .id(SharedString::from(format!("queued-{run_id}")))
                    .debug_selector(|| selector)
                    .gap_2()
                    .px_2()
                    .py_1()
                    .rounded(scaled(tokens::CONTROL_RADIUS))
                    .border_1()
                    .border_dashed()
                    .border_color(palette.border.hsla())
                    .when(can_reorder && !blocked, |this| {
                        this.cursor_grab()
                            .on_drag(dragged, |dragged, _, _, cx| cx.new(|_| dragged.clone()))
                            .drag_over::<DraggedQueued>(move |style, dragged, _, cx| {
                                let palette = mustr_ui::palette(cx);
                                let style = style
                                    .border_color(palette.accent.hsla())
                                    .bg(palette.accent_soft.hsla());
                                if dragged.index < index {
                                    style.border_b_2()
                                } else {
                                    style.border_t_2()
                                }
                            })
                            .on_drop(cx.listener(
                                move |this, dragged: &DraggedQueued, window, cx| {
                                    this.move_queued(dragged.index, index, window, cx)
                                },
                            ))
                    })
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
                    .when(can_reorder, |this| {
                        this.child(
                            div()
                                .debug_selector({
                                    let run_id = run_id.clone();
                                    move || format!("thread-queued-up-{run_id}")
                                })
                                .child(
                                    IconButton::new(
                                        SharedString::from(format!("queued-up-{run_id}")),
                                        IconName::ArrowUp,
                                    )
                                    .icon_size(IconSize::XSmall)
                                    .icon_color(Color::Muted)
                                    .disabled(blocked || index == 0)
                                    .tooltip(Tooltip::text("Run this one earlier"))
                                    .on_click(cx.listener(
                                        move |this, _, window, cx| {
                                            this.move_queued(index, index - 1, window, cx)
                                        },
                                    )),
                                ),
                        )
                        .child(
                            div()
                                .debug_selector({
                                    let run_id = run_id.clone();
                                    move || format!("thread-queued-down-{run_id}")
                                })
                                .child(
                                    IconButton::new(
                                        SharedString::from(format!("queued-down-{run_id}")),
                                        IconName::ArrowDown,
                                    )
                                    .icon_size(IconSize::XSmall)
                                    .icon_color(Color::Muted)
                                    .disabled(blocked || index + 1 == count)
                                    .tooltip(Tooltip::text("Run this one later"))
                                    .on_click(cx.listener(
                                        move |this, _, window, cx| {
                                            this.move_queued(index, index + 1, window, cx)
                                        },
                                    )),
                                ),
                        )
                    })
                    .child(
                        div()
                            .debug_selector({
                                let run_id = run_id.clone();
                                move || format!("thread-edit-queued-{run_id}")
                            })
                            .child(
                                TextButton::new(
                                    SharedString::from(format!("edit-queued-{run_id}")),
                                    "Edit",
                                )
                                .disabled(blocked)
                                .tooltip("Change this message before it runs")
                                .on_click(cx.listener({
                                    let message = message.clone();
                                    move |this, _, window, cx| {
                                        this.edit_queued(message.clone(), window, cx)
                                    }
                                })),
                            ),
                    )
                    .when(running, |this| {
                        this.child(
                            TextButton::new(
                                SharedString::from(format!("steer-queued-{run_id}")),
                                "Steer now",
                            )
                            .disabled(blocked)
                            .tooltip("Hand this message to the running agent now")
                            .on_click(cx.listener({
                                let run_id = run_id.clone();
                                move |this, _, window, cx| {
                                    this.steer_with_queued(run_id.clone(), window, cx)
                                }
                            })),
                        )
                    })
                    .when(can_start, |this| {
                        this.child(
                            div()
                                .debug_selector({
                                    let run_id = run_id.clone();
                                    move || format!("thread-start-queued-{run_id}")
                                })
                                .child(
                                    TextButton::new(
                                        SharedString::from(format!("start-queued-{run_id}")),
                                        "Send now",
                                    )
                                    .disabled(blocked)
                                    .tooltip("Start a run with this message now")
                                    .on_click(cx.listener({
                                        let run_id = run_id.clone();
                                        move |this, _, window, cx| {
                                            this.start_queued(run_id.clone(), window, cx)
                                        }
                                    })),
                                ),
                        )
                    })
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

    /// The composer's images, as T3's strip shows them: a tile each, with its upload's
    /// progress, Retry when it failed, and a remove button.
    fn render_image_strip(&self, cx: &mut Context<Self>) -> impl IntoElement + use<> {
        let preparing = self.images.preparing;
        h_flex()
            .flex_wrap()
            .gap_1p5()
            .pt_0p5()
            .children(
                self.images
                    .composer
                    .iter()
                    .map(|image| self.render_image_tile(image, cx)),
            )
            .children(
                self.images
                    .files
                    .iter()
                    .map(|file| self.render_file_tile(file, cx)),
            )
            .when(preparing > 0, |this| {
                let palette = mustr_ui::palette(cx);
                this.child(
                    div()
                        .size(scaled(px(IMAGE_TILE_SIZE)))
                        .rounded(scaled(tokens::CORNER_RADIUS))
                        .border(tokens::BORDER_WIDTH)
                        .border_dashed()
                        .border_color(palette.border.hsla())
                        .flex()
                        .items_center()
                        .justify_center()
                        .child(Label::new("0%").size(LabelSize::XSmall).color(Color::Muted)),
                )
            })
    }

    /// A file's tile in the strip: its name and size, its upload's progress, Retry when it
    /// failed, and a remove button, as T3's file chips show them.
    fn render_file_tile(&self, file: &ComposerFile, cx: &mut Context<Self>) -> AnyElement {
        let palette = palette(cx);
        let key = file.key;
        let failed = match &file.upload {
            UploadState::Failed { reason } => Some(reason.clone()),
            _ => None,
        };
        let progress = match file.upload {
            UploadState::Uploading { progress } => Some(progress),
            _ => None,
        };
        let detail = if file.pasted_text {
            format!(
                "{} · pasted text",
                crate::attachments::format_size(file.size())
            )
        } else {
            crate::attachments::format_size(file.size())
        };
        let tooltip = match &failed {
            Some(reason) => format!("{}: {reason}", file.name),
            None => file.name.clone(),
        };
        let button = |id: SharedString, icon: IconName, tooltip: &'static str| {
            IconButton::new(id, icon)
                .icon_size(IconSize::XSmall)
                .icon_color(Color::Muted)
                .tooltip(Tooltip::text(tooltip))
        };
        h_flex()
            .id(SharedString::from(format!("composer-file-{key}")))
            .debug_selector(move || format!("thread-file-{key}"))
            .relative()
            .flex_none()
            .h(scaled(px(IMAGE_TILE_SIZE)))
            .w(scaled(px(FILE_TILE_WIDTH)))
            .gap_2()
            .pl_2()
            .pr_1()
            .rounded(scaled(tokens::CORNER_RADIUS))
            .overflow_hidden()
            .border(tokens::BORDER_WIDTH)
            .border_color(if failed.is_some() {
                cx.theme().status().error
            } else {
                palette.border.hsla()
            })
            .bg(palette.surface_2.hsla())
            .tooltip(Tooltip::text(tooltip))
            .child(
                Icon::new(if file.pasted_text {
                    IconName::TextSnippet
                } else {
                    IconName::File
                })
                .size(IconSize::Medium)
                .color(if failed.is_some() {
                    Color::Error
                } else {
                    Color::Muted
                }),
            )
            .child(
                v_flex()
                    .flex_1()
                    .min_w_0()
                    .gap_0p5()
                    .child(
                        div()
                            .min_w_0()
                            .overflow_hidden()
                            .whitespace_nowrap()
                            .text_ellipsis()
                            .text_size(tokens::text_size(tokens::TEXT_SIZE_SMALL))
                            .font_weight(FontWeight::MEDIUM)
                            .child(file.name.clone()),
                    )
                    .child(
                        Label::new(match (&failed, progress) {
                            (Some(_), _) => "Upload failed".to_string(),
                            (None, Some(progress)) => {
                                format!("{}%", (progress * 100.).floor() as u32)
                            }
                            (None, None) => detail,
                        })
                        .size(LabelSize::XSmall)
                        .color(if failed.is_some() {
                            Color::Error
                        } else {
                            Color::Muted
                        }),
                    ),
            )
            .child(
                v_flex()
                    .flex_none()
                    .child(
                        div()
                            .debug_selector(move || format!("thread-file-remove-{key}"))
                            .child(
                                button(
                                    SharedString::from(format!("remove-file-{key}")),
                                    IconName::Close,
                                    "Remove",
                                )
                                .on_click(
                                    cx.listener(move |this, _, _, cx| this.remove_image(key, cx)),
                                ),
                            ),
                    )
                    .when(failed.is_some(), |this| {
                        this.child(
                            div()
                                .debug_selector(move || format!("thread-file-retry-{key}"))
                                .child(
                                    button(
                                        SharedString::from(format!("retry-file-{key}")),
                                        IconName::RotateCw,
                                        "Retry",
                                    )
                                    .on_click(cx.listener(
                                        move |this, _, window, cx| {
                                            this.retry_image(key, window, cx)
                                        },
                                    )),
                                ),
                        )
                    }),
            )
            .when_some(progress, |this, progress| {
                this.child(
                    div()
                        .absolute()
                        .bottom_0()
                        .left_0()
                        .h(scaled(px(3.)))
                        .w(relative(progress.max(0.04)))
                        .bg(palette.accent.hsla()),
                )
            })
            .into_any_element()
    }

    fn render_image_tile(&self, image: &ComposerImage, cx: &mut Context<Self>) -> AnyElement {
        let palette = palette(cx);
        let key = image.key;
        let failed = match &image.upload {
            UploadState::Failed { reason } => Some(reason.clone()),
            _ => None,
        };
        let progress = match image.upload {
            UploadState::Uploading { progress } => Some(progress),
            _ => None,
        };
        let name: SharedString = image.name.clone().into();
        let thumbnail = image.image.clone();
        let small_button = |id: SharedString, icon: IconName| {
            div()
                .id(id)
                .size(scaled(px(18.)))
                .rounded_full()
                .flex()
                .items_center()
                .justify_center()
                .bg(gpui::black().opacity(0.62))
                .hover(|style| style.bg(gpui::black().opacity(0.8)))
                .cursor_pointer()
                .child(
                    Icon::new(icon)
                        .size(IconSize::XSmall)
                        .color(Color::Custom(gpui::white())),
                )
        };
        div()
            .id(SharedString::from(format!("composer-image-{key}")))
            .debug_selector(move || format!("thread-image-{key}"))
            .relative()
            .flex_none()
            .size(scaled(px(IMAGE_TILE_SIZE)))
            .rounded(scaled(tokens::CORNER_RADIUS))
            .overflow_hidden()
            .border(tokens::BORDER_WIDTH)
            .border_color(if failed.is_some() {
                cx.theme().status().error
            } else {
                palette.border.hsla()
            })
            .bg(palette.surface_2.hsla())
            .cursor_pointer()
            // The name shows in the preview; a failed upload says why.
            .when_some(failed.clone(), |this, reason| {
                this.tooltip(Tooltip::text(format!("{name}: {reason}")))
            })
            .on_click(cx.listener(move |this, _, window, cx| {
                this.open_image_preview(name.clone(), thumbnail.clone(), window, cx)
            }))
            .child(
                img(image.image.clone())
                    .size_full()
                    .object_fit(ObjectFit::Cover),
            )
            .when_some(progress, |this, progress| {
                this.child(
                    div()
                        .absolute()
                        .inset_0()
                        .bg(gpui::black().opacity(0.35))
                        .flex()
                        .items_end()
                        .child(
                            v_flex()
                                .w_full()
                                .items_center()
                                .gap_0p5()
                                .pb_1()
                                .child(
                                    div()
                                        .text_size(tokens::text_size(tokens::TEXT_SIZE_XSMALL))
                                        .font_weight(FontWeight::SEMIBOLD)
                                        .text_color(gpui::white())
                                        .child(format!("{}%", (progress * 100.).floor() as u32)),
                                )
                                .child(
                                    div()
                                        .mx_1()
                                        .h(scaled(px(3.)))
                                        .w(scaled(px(IMAGE_TILE_SIZE - 12.)))
                                        .rounded_full()
                                        .bg(gpui::white().opacity(0.3))
                                        .child(
                                            div()
                                                .h_full()
                                                .w(relative(progress.max(0.04)))
                                                .rounded_full()
                                                .bg(palette.accent.hsla()),
                                        ),
                                ),
                        ),
                )
            })
            .when(failed.is_some(), |this| {
                this.child(
                    div()
                        .absolute()
                        .inset_0()
                        .bg(cx.theme().status().error.opacity(0.25)),
                )
                .child(
                    div()
                        .absolute()
                        .bottom(scaled(px(3.)))
                        .left(scaled(px(3.)))
                        .debug_selector(move || format!("thread-image-retry-{key}"))
                        .child(
                            small_button(
                                SharedString::from(format!("retry-image-{key}")),
                                IconName::RotateCw,
                            )
                            .on_click(cx.listener(
                                move |this, _, window, cx| {
                                    cx.stop_propagation();
                                    this.retry_image(key, window, cx)
                                },
                            )),
                        ),
                )
            })
            .child(
                div()
                    .absolute()
                    .top(scaled(px(3.)))
                    .right(scaled(px(3.)))
                    .debug_selector(move || format!("thread-image-remove-{key}"))
                    .child(
                        small_button(
                            SharedString::from(format!("remove-image-{key}")),
                            IconName::Close,
                        )
                        .on_click(cx.listener(move |this, _, _, cx| {
                            cx.stop_propagation();
                            this.remove_image(key, cx)
                        })),
                    ),
            )
            .into_any_element()
    }

    pub(crate) fn render_composer(
        &mut self,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> impl IntoElement {
        self.deploy_pending_find(window, cx);
        self.sync_placeholder(window, cx);
        let declines = self.feedback_target().is_some();
        let palette = palette(cx);
        let state = self.thread_state();
        let running = state.is_some_and(ThreadState::is_running);
        let is_draft = self.new_thread.is_some();
        let pending = state.map(ThreadState::pending_requests).unwrap_or_default();
        let queued = state.map(ThreadState::queued_messages).unwrap_or_default();
        let blocked = self.write_blocked_reason();
        let can_write = blocked.is_none() && (is_draft || state.is_some()) && !self.sending;
        let has_text = !self.composer_text(cx).trim().is_empty();
        // While the agent waits on a question, the message answers it (`answers.rs`): Next
        // moves to the next question, and Answer, on the last, answers them all.
        let answering = self
            .answer_target()
            .filter(|_| !is_draft && !declines)
            .map(|(request_id, questions)| self.answers_complete(&request_id, &questions, cx));

        let editing_queued = self.composer_state.editing_queued.is_some();
        let mut buttons: Vec<AnyElement> = Vec::new();
        if editing_queued {
            buttons.push(
                TextButton::new("thread-cancel-edit", "Cancel")
                    .on_click(
                        cx.listener(|this, _, window, cx| this.cancel_queued_edit(window, cx)),
                    )
                    .into_any_element(),
            );
            buttons.push(
                div()
                    .debug_selector(|| "thread-save-queued".into())
                    .child(
                        TextButton::new("thread-save-queued", "Save")
                            .style(MustrButtonStyle::Primary)
                            .disabled(!can_write || !has_text)
                            .tooltip("Replace the queued message with this")
                            .on_click(cx.listener(|this, _, window, cx| {
                                this.send(Delivery::Queue, window, cx)
                            })),
                    )
                    .into_any_element(),
            );
        } else if running && declines {
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
                    .debug_selector(|| "thread-decline-feedback".into())
                    .child(
                        TextButton::new("thread-decline-feedback", "Decline with feedback")
                            .style(MustrButtonStyle::Primary)
                            .disabled(!can_write || !has_text)
                            .tooltip("Decline the request and tell the agent why")
                            .on_click(cx.listener(|this, _, window, cx| {
                                this.send(Delivery::Auto, window, cx)
                            })),
                    )
                    .into_any_element(),
            );
        } else if let Some(complete) = answering {
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
            let has_answer = has_text || self.images.has_attachments();
            buttons.push(
                div()
                    .debug_selector(|| "thread-answer".into())
                    .child(
                        TextButton::new("thread-answer", if complete { "Answer" } else { "Next" })
                            .style(MustrButtonStyle::Primary)
                            .disabled(!can_write || !(has_answer || complete))
                            .tooltip(if complete {
                                "Answer the agent's questions"
                            } else {
                                "Answer this question, then the next"
                            })
                            .on_click(cx.listener(|this, _, window, cx| {
                                this.send(Delivery::Auto, window, cx)
                            })),
                    )
                    .into_any_element(),
            );
        } else if running {
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
            // T3's primary actions (`ComposerPrimaryActions.tsx`): Resume in an empty composer
            // when there's a run to continue or a held queue, then Implement or Refine for an
            // active plan, then Send.
            let empty = !has_text && !self.images.has_attachments();
            let resumable = state.and_then(ThreadState::resumable);
            let plan = state
                .and_then(ThreadState::actionable_plan)
                .filter(|_| !self.images.has_attachments());
            if empty && let Some(resumable) = resumable {
                let tooltip = match (&resumable.continue_run_id, resumable.held_queue) {
                    (Some(_), true) => "Continue the stopped run, then the held queue",
                    (Some(_), false) => "Continue where the agent left off",
                    (None, _) => "Run the held queued messages",
                };
                buttons.push(
                    div()
                        .debug_selector(|| "thread-resume".into())
                        .child(
                            TextButton::new("thread-resume", "Resume")
                                .style(MustrButtonStyle::Primary)
                                .disabled(!can_write)
                                .tooltip(tooltip)
                                .on_click(
                                    cx.listener(|this, _, window, cx| this.resume(window, cx)),
                                ),
                        )
                        .into_any_element(),
                );
            } else if plan.is_some() && empty {
                buttons.push(
                    div()
                        .debug_selector(|| "thread-implement-new".into())
                        .child(
                            TextButton::new("thread-implement-new", "In a new thread")
                                .disabled(!can_write)
                                .tooltip("Implement the plan in a new thread")
                                .on_click(cx.listener(|this, _, window, cx| {
                                    this.implement_plan(true, window, cx)
                                })),
                        )
                        .into_any_element(),
                );
                buttons.push(
                    div()
                        .debug_selector(|| "thread-implement".into())
                        .child(
                            TextButton::new("thread-implement", "Implement")
                                .style(MustrButtonStyle::Primary)
                                .disabled(!can_write)
                                .tooltip("Leave plan mode and implement the plan")
                                .on_click(cx.listener(|this, _, window, cx| {
                                    this.implement_plan(false, window, cx)
                                })),
                        )
                        .into_any_element(),
                );
            } else {
                let label = if is_draft {
                    "Start thread"
                } else if plan.is_some() {
                    "Refine"
                } else {
                    "Send"
                };
                let waiting = self.images.send_blocked_reason();
                buttons.push(
                    div()
                        .debug_selector(|| "thread-send".into())
                        .child(
                            TextButton::new("thread-send", label)
                                .style(MustrButtonStyle::Primary)
                                .disabled(!can_write || empty || waiting.is_some())
                                .when_some(waiting, |button, waiting| button.tooltip(waiting))
                                .when(plan.is_some() && waiting.is_none(), |button| {
                                    button.tooltip("Ask the agent to change the plan")
                                })
                                .on_click(cx.listener(|this, _, window, cx| {
                                    this.send(Delivery::Auto, window, cx)
                                })),
                        )
                        .into_any_element(),
                );
            }
        }
        let composer_focus = self.composer.focus_handle(cx);
        let send_binding = ui::KeyBinding::for_action_in(&SendMessage, &composer_focus, cx)
            .size(rems_from_px(10_f32));
        let hint = send_binding.has_binding(window).then(|| {
            h_flex()
                .gap_1()
                .child(send_binding)
                .child(
                    Label::new(if declines {
                        "decline"
                    } else if let Some(complete) = answering {
                        if complete { "answer" } else { "next" }
                    } else if running {
                        crate::thread_settings::FollowUpBehavior::get(cx).hint()
                    } else {
                        "send"
                    })
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
                    .max_w(scaled(px(CONTENT_MAX_WIDTH)))
                    .gap_2()
                    .children(
                        pending
                            .iter()
                            .map(|pending| self.render_pending(pending, cx)),
                    )
                    .children(self.render_goal(cx))
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
                    .when(self.composer_state.editing_queued.is_some(), |this| {
                        this.child(
                            h_flex()
                                .gap_1()
                                .child(
                                    Icon::new(IconName::Pencil)
                                        .size(IconSize::XSmall)
                                        .color(Color::Accent),
                                )
                                .child(
                                    Label::new("Editing a queued message: Save replaces it")
                                        .size(LabelSize::Small)
                                        .color(Color::Muted),
                                ),
                        )
                    })
                    .when_some(self.images.intake_note.clone(), |this, note| {
                        this.child(
                            h_flex()
                                .w_full()
                                .items_start()
                                .gap_1()
                                .debug_selector(|| "thread-intake-note".into())
                                .child(
                                    div().flex_none().pt_0p5().child(
                                        Icon::new(IconName::TextSnippet)
                                            .size(IconSize::XSmall)
                                            .color(Color::Muted),
                                    ),
                                )
                                // It's long, so it wraps rather than running past the composer.
                                .child(
                                    div()
                                        .flex_1()
                                        .min_w_0()
                                        .text_size(tokens::text_size(tokens::TEXT_SIZE_SMALL))
                                        .text_color(palette.text_muted.hsla())
                                        .child(note),
                                ),
                        )
                    })
                    .when_some(self.images.intake_error.clone(), |this, error| {
                        this.child(
                            h_flex()
                                .gap_1()
                                .child(
                                    Icon::new(IconName::Warning)
                                        .size(IconSize::XSmall)
                                        .color(Color::Warning),
                                )
                                .child(
                                    Label::new(error)
                                        .size(LabelSize::Small)
                                        .color(Color::Warning),
                                ),
                        )
                    })
                    .children(self.render_composer_menu(cx))
                    .child(
                        v_flex()
                            .id("thread-composer")
                            .debug_selector(|| "thread-composer".into())
                            .key_context(COMPOSER_CONTEXT)
                            .on_action(cx.listener(Self::on_send))
                            .on_action(cx.listener(Self::on_steer))
                            .on_action(cx.listener(Self::on_stop))
                            .on_action(cx.listener(Self::on_paste_as_text))
                            .capture_action(cx.listener(Self::on_paste))
                            .capture_action(cx.listener(Self::on_move_up))
                            .capture_action(cx.listener(Self::on_move_down))
                            .capture_action(cx.listener(Self::on_tab))
                            .capture_action(cx.listener(Self::on_cancel))
                            .drag_over::<ExternalPaths>(|style, _, _, cx| {
                                let palette = mustr_ui::palette(cx);
                                style
                                    .border_color(palette.accent.hsla())
                                    .bg(palette.accent_soft.hsla())
                            })
                            .on_drop(cx.listener(Self::on_drop_paths))
                            .w_full()
                            .gap_1()
                            .px_2()
                            .py_1p5()
                            .rounded(scaled(tokens::CORNER_RADIUS))
                            .border(tokens::BORDER_WIDTH)
                            .border_color(palette.border.hsla())
                            .bg(palette.surface.hsla())
                            .when(!self.images.is_empty(), |this| {
                                this.child(self.render_image_strip(cx))
                            })
                            .child(self.composer.clone())
                            .child(
                                // When the composer is narrow (a big ui_font_size in a small
                                // window), the row wraps rather than overlapping, and the
                                // buttons stay together at the end.
                                h_flex()
                                    .flex_wrap()
                                    .gap_1p5()
                                    .child(self.settings_bar.clone())
                                    .child(
                                        div().debug_selector(|| "thread-attach".into()).child(
                                            IconButton::new("thread-attach", IconName::Paperclip)
                                                .icon_size(IconSize::Small)
                                                .icon_color(Color::Muted)
                                                .tooltip(Tooltip::text(
                                                    "Attach images and files (or paste or drop \
                                                     them)",
                                                ))
                                                .on_click(cx.listener(|this, _, window, cx| {
                                                    this.pick_files(window, cx)
                                                })),
                                        ),
                                    )
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
                                    .child(
                                        h_flex()
                                            .ml_auto()
                                            .gap_1p5()
                                            .when(
                                                self.sending
                                                    || self.images.send_when_uploaded.is_some(),
                                                |this| {
                                                    this.child(
                                                        Label::new(if self.sending {
                                                            "Sending…"
                                                        } else {
                                                            "Sending once the images are up…"
                                                        })
                                                        .size(LabelSize::XSmall)
                                                        .color(Color::Muted),
                                                    )
                                                },
                                            )
                                            .children(buttons),
                                    ),
                            ),
                    ),
            )
    }
}

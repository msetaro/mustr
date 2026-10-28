//! Writing comments: the text field every comment, reply, and summary is written in, and the
//! cards of draft line comments in the diff.

use editor::{Editor, EditorEvent};
use gpui::{
    App, AppContext as _, Context, Entity, EventEmitter, FocusHandle, Focusable, FontWeight,
    IntoElement, MouseButton, ParentElement as _, Render, SharedString, Styled as _, Subscription,
    Window, actions,
};
use language::LanguageRegistry;
use markdown::{Markdown, MarkdownElement};
use mustr_ui::tokens;
use std::sync::Arc;
use ui::{
    Button, ButtonSize, ButtonStyle, Color, Icon, IconName, IconSize, KeyBinding, Label,
    LabelCommon as _, LabelSize, Tooltip, prelude::*,
};

use crate::{
    DraftComment,
    comment_html::comment_markdown,
    header::pill,
    threads::{comment_markdown_style, ui_scaled},
};

actions!(
    review,
    [
        /// Comments on the selected lines of the review's diff.
        AddComment,
        /// Saves the comment being written: adds it to the pending review, or sends it.
        ConfirmComment,
        /// Stops writing the comment.
        CancelComment,
        /// Opens the form that submits the pending review.
        SubmitReview,
    ]
);

pub(crate) const COMPOSER_CONTEXT: &str = "ReviewComposer";
const COMPOSER_MIN_LINES: usize = 3;
const COMPOSER_MAX_LINES: usize = 16;

/// A Markdown text field for a comment.
pub(crate) struct Composer {
    editor: Entity<Editor>,
}

impl Composer {
    pub(crate) fn new(text: &str, placeholder: &str, window: &mut Window, cx: &mut App) -> Self {
        let editor = cx.new(|cx| {
            let mut editor =
                Editor::auto_height(COMPOSER_MIN_LINES, COMPOSER_MAX_LINES, window, cx);
            editor.set_placeholder_text(placeholder, window, cx);
            if !text.is_empty() {
                editor.set_text(text, window, cx);
            }
            editor
        });
        Self { editor }
    }

    pub(crate) fn editor(&self) -> &Entity<Editor> {
        &self.editor
    }

    pub(crate) fn text(&self, cx: &App) -> String {
        self.editor.read(cx).text(cx)
    }

    pub(crate) fn is_blank(&self, cx: &App) -> bool {
        self.text(cx).trim().is_empty()
    }

    pub(crate) fn focus_handle(&self, cx: &App) -> FocusHandle {
        self.editor.focus_handle(cx)
    }

    pub(crate) fn focus(&self, window: &mut Window, cx: &mut App) {
        window.focus(&self.focus_handle(cx), cx);
    }

    /// Calls `on_edit` whenever the text changes.
    pub(crate) fn subscribe<T: 'static>(
        &self,
        cx: &mut Context<T>,
        on_edit: impl Fn(&mut T, &mut Context<T>) + 'static,
    ) -> Subscription {
        cx.subscribe(&self.editor, move |this, _, event: &EditorEvent, cx| {
            if matches!(event, EditorEvent::BufferEdited) {
                on_edit(this, cx);
            }
        })
    }

    pub(crate) fn render(&self, cx: &App) -> impl IntoElement + use<> {
        let colors = cx.theme().colors();
        div()
            .key_context(COMPOSER_CONTEXT)
            .w_full()
            .px_2()
            .py_1()
            .rounded(ui_scaled(tokens::CORNER_RADIUS, cx))
            .border(tokens::BORDER_WIDTH)
            .border_color(colors.border)
            .bg(colors.editor_background)
            .child(self.editor.clone())
    }

    /// The button that confirms the text, showing its key binding.
    pub(crate) fn confirm_button(
        &self,
        id: impl Into<gpui::ElementId>,
        label: impl Into<SharedString>,
        cx: &App,
    ) -> Button {
        Button::new(id, label)
            .style(ButtonStyle::Filled)
            .size(ButtonSize::Compact)
            .label_size(LabelSize::Small)
            .key_binding(
                KeyBinding::for_action_in(&ConfirmComment, &self.focus_handle(cx), cx)
                    .size(rems_from_px(11_f32)),
            )
    }
}

pub(crate) fn cancel_button(id: impl Into<gpui::ElementId>) -> Button {
    Button::new(id, "Cancel")
        .style(ButtonStyle::Subtle)
        .size(ButtonSize::Compact)
        .label_size(LabelSize::Small)
}

/// An error from the forge, next to the action that failed.
pub(crate) fn error_label(error: &SharedString) -> impl IntoElement + use<> {
    h_flex()
        .min_w_0()
        .flex_1()
        .gap_1()
        .child(
            Icon::new(IconName::Warning)
                .size(IconSize::XSmall)
                .color(Color::Error),
        )
        .child(
            Label::new(error.clone())
                .size(LabelSize::Small)
                .color(Color::Error),
        )
}

pub enum DraftCardEvent {
    /// The draft's text or state changed, so the pending review should be saved.
    Changed,
    /// The draft was deleted, or abandoned before it was added.
    Removed,
}

/// A line comment drafted for the pending review: a composer while it's being written, then
/// the comment as it will be posted.
pub struct DraftCard {
    draft: DraftComment,
    composer: Option<Composer>,
    body: Entity<Markdown>,
    language_registry: Option<Arc<LanguageRegistry>>,
    _composer_subscription: Option<Subscription>,
}

impl EventEmitter<DraftCardEvent> for DraftCard {}

impl DraftCard {
    /// A card for `draft`, with its composer open when it hasn't been added yet.
    pub(crate) fn new(
        draft: DraftComment,
        language_registry: Option<Arc<LanguageRegistry>>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        let body = new_body(&draft.body, language_registry.clone(), cx);
        let mut this = Self {
            body,
            composer: None,
            language_registry,
            _composer_subscription: None,
            draft,
        };
        if !this.draft.added {
            this.open_composer(window, cx);
        }
        this
    }

    /// The draft as it stands, with the text being written.
    pub fn draft(&self, cx: &App) -> DraftComment {
        let mut draft = self.draft.clone();
        if let Some(composer) = &self.composer {
            draft.body = composer.text(cx);
        }
        draft
    }

    pub fn is_editing(&self) -> bool {
        self.composer.is_some()
    }

    pub fn composer_editor(&self) -> Option<&Entity<Editor>> {
        self.composer.as_ref().map(Composer::editor)
    }

    pub(crate) fn focus(&self, window: &mut Window, cx: &mut App) {
        if let Some(composer) = &self.composer {
            composer.focus(window, cx);
        }
    }

    /// A rough height in lines, before the editor measures the card.
    pub(crate) fn estimated_lines(&self) -> u32 {
        if self.composer.is_some() {
            return COMPOSER_MIN_LINES as u32 + 4;
        }
        3 + self.draft.body.lines().count().max(1) as u32
    }

    fn open_composer(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let composer = Composer::new(
            &self.draft.body,
            "Leave a comment. Markdown is supported.",
            window,
            cx,
        );
        self._composer_subscription =
            Some(composer.subscribe(cx, |_: &mut Self, cx| cx.emit(DraftCardEvent::Changed)));
        self.composer = Some(composer);
        cx.notify();
    }

    fn edit(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.composer.is_none() {
            self.open_composer(window, cx);
        }
        self.focus(window, cx);
    }

    /// Adds the comment to the pending review. Empty comments aren't added.
    pub(crate) fn confirm(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(composer) = &self.composer else {
            return;
        };
        if composer.is_blank(cx) {
            composer.focus(window, cx);
            return;
        }
        self.draft.body = composer.text(cx);
        self.draft.added = true;
        self.composer = None;
        self._composer_subscription = None;
        self.body = new_body(&self.draft.body, self.language_registry.clone(), cx);
        cx.emit(DraftCardEvent::Changed);
        cx.notify();
    }

    /// Abandons a comment that wasn't added yet, or the edits of one that was.
    pub(crate) fn cancel(&mut self, cx: &mut Context<Self>) {
        if !self.draft.added {
            cx.emit(DraftCardEvent::Removed);
            return;
        }
        self.composer = None;
        self._composer_subscription = None;
        cx.emit(DraftCardEvent::Changed);
        cx.notify();
    }

    fn delete(&mut self, cx: &mut Context<Self>) {
        cx.emit(DraftCardEvent::Removed);
    }
}

fn new_body(
    body: &str,
    language_registry: Option<Arc<LanguageRegistry>>,
    cx: &mut App,
) -> Entity<Markdown> {
    let body = SharedString::from(comment_markdown(body));
    cx.new(|cx| Markdown::new(body, language_registry, None, cx))
}

impl Render for DraftCard {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let colors = cx.theme().colors();
        let accent = colors.text_accent;
        let editing = self.composer.is_some();
        let range_note = self
            .draft
            .start_line
            .filter(|start| *start < self.draft.position.line())
            .map(|_| {
                format!(
                    "T3 posts comments on one line, so this one goes on line {}.",
                    self.draft.position.line()
                )
            });

        let title = h_flex()
            .w_full()
            .px_2()
            .py_1()
            .gap_1p5()
            .child(
                Icon::new(if editing {
                    IconName::Pencil
                } else {
                    IconName::Chat
                })
                .size(IconSize::Small)
                .color(Color::Accent),
            )
            .child(
                Label::new(if editing && !self.draft.added {
                    "New comment"
                } else {
                    "You"
                })
                .size(LabelSize::Small)
                .weight(FontWeight::SEMIBOLD),
            )
            .child(
                Label::new(self.draft.line_label())
                    .size(LabelSize::Small)
                    .color(Color::Muted),
            )
            .child(div().flex_1())
            .when(self.draft.added, |this| {
                this.child(pill("Pending", accent, cx))
            })
            .when(!editing, |this| {
                this.child(
                    Button::new("edit-draft", "Edit")
                        .style(ButtonStyle::Subtle)
                        .size(ButtonSize::Compact)
                        .label_size(LabelSize::Small)
                        .on_click(cx.listener(|this, _, window, cx| this.edit(window, cx))),
                )
                .child(
                    Button::new("delete-draft", "Delete")
                        .style(ButtonStyle::Subtle)
                        .size(ButtonSize::Compact)
                        .label_size(LabelSize::Small)
                        .tooltip(Tooltip::text("Remove this comment from the pending review"))
                        .on_click(cx.listener(|this, _, _, cx| this.delete(cx))),
                )
            });

        let content = match &self.composer {
            Some(composer) => v_flex()
                .px_2()
                .pb_2()
                .gap_1p5()
                .child(composer.render(cx))
                .when_some(range_note, |this, note| {
                    this.child(Label::new(note).size(LabelSize::XSmall).color(Color::Muted))
                })
                .child(
                    h_flex()
                        .gap_1()
                        .justify_end()
                        .child(
                            cancel_button("cancel-draft")
                                .on_click(cx.listener(|this, _, _, cx| this.cancel(cx))),
                        )
                        .child(
                            composer
                                .confirm_button(
                                    "add-draft",
                                    if self.draft.added {
                                        "Save"
                                    } else {
                                        "Add to review"
                                    },
                                    cx,
                                )
                                .on_click(
                                    cx.listener(|this, _, window, cx| this.confirm(window, cx)),
                                ),
                        ),
                )
                .into_any_element(),
            None => div()
                .px_3()
                .pb_2()
                .child(
                    MarkdownElement::new(self.body.clone(), comment_markdown_style(window, cx))
                        .on_url_click(|url, _, cx| cx.open_url(&url)),
                )
                .into_any_element(),
        };

        v_flex()
            .id("draft-card")
            .w_full()
            .rounded(ui_scaled(tokens::CORNER_RADIUS, cx))
            .border(tokens::BORDER_WIDTH)
            .border_color(accent.opacity(0.6))
            .bg(colors.surface_background)
            .overflow_hidden()
            // Clicks on a card are the card's, not the editor's: they shouldn't move the cursor.
            .on_mouse_down(MouseButton::Left, |_, _, cx| cx.stop_propagation())
            .on_action(cx.listener(|this, _: &ConfirmComment, window, cx| this.confirm(window, cx)))
            .on_action(cx.listener(|this, _: &CancelComment, _, cx| this.cancel(cx)))
            .child(title)
            .child(content)
    }
}

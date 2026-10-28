//! Submitting the pending review: a summary, a verdict, and every added draft in one
//! `submitReview`, or just a comment on the pull request's conversation.

use collections::HashSet;
use editor::Editor;
use gpui::{
    AnyElement, App, Context, Entity, FontWeight, IntoElement, ParentElement as _, SharedString,
    Styled as _, Task, Window,
};
use mustr_ui::tokens;
use ui::{
    Button, ButtonSize, ButtonStyle, Color, Label, LabelCommon as _, LabelSize, TintColor, Tooltip,
    prelude::*,
};

use crate::{
    PendingReview, ReviewPermissions, ReviewSubmission, ReviewVerdict, ReviewView, can_submit,
    composer::{CancelComment, Composer, ConfirmComment, SubmitReview, cancel_button, error_label},
    header::pill,
};

/// What the submit form sends.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SubmitMode {
    /// A review: the summary, a verdict, and the pending comments.
    Review,
    /// A comment on the conversation, outside any review.
    Comment,
}

pub(crate) struct SubmitForm {
    pub(crate) open: bool,
    pub(crate) mode: SubmitMode,
    pub(crate) verdict: ReviewVerdict,
    pub(crate) summary: Composer,
    pub(crate) error: Option<SharedString>,
    pub(crate) in_flight: Option<Task<()>>,
}

impl SubmitForm {
    pub(crate) fn new(
        pending: &PendingReview,
        permissions: &ReviewPermissions,
        window: &mut Window,
        cx: &mut App,
    ) -> Self {
        let verdict = pending
            .verdict
            .filter(|verdict| permissions.verdicts.contains(verdict))
            .or_else(|| permissions.verdicts.first().copied())
            .unwrap_or_default();
        let mode = if permissions.verdicts.is_empty() && permissions.conversation_comments {
            SubmitMode::Comment
        } else {
            SubmitMode::Review
        };
        Self {
            open: false,
            mode,
            verdict,
            summary: Composer::new(
                &pending.summary,
                "Leave a summary. Markdown is supported.",
                window,
                cx,
            ),
            error: None,
            in_flight: None,
        }
    }
}

impl ReviewView {
    /// Drafts that go with the review when it's submitted.
    pub fn added_draft_count(&self, cx: &App) -> usize {
        self.draft_cards()
            .filter(|card| card.read(cx).draft(cx).added)
            .count()
    }

    pub fn is_submit_form_open(&self) -> bool {
        self.submit.open
    }

    pub fn submit_error(&self) -> Option<&SharedString> {
        self.submit.error.as_ref()
    }

    pub fn summary_editor(&self) -> &Entity<Editor> {
        self.submit.summary.editor()
    }

    pub fn set_submit_mode(&mut self, mode: SubmitMode, cx: &mut Context<Self>) {
        self.submit.mode = mode;
        self.submit.error = None;
        cx.notify();
    }

    pub fn set_verdict(&mut self, verdict: ReviewVerdict, cx: &mut Context<Self>) {
        if self.permissions.verdicts.contains(&verdict) {
            self.submit.verdict = verdict;
            self.submit.error = None;
            self.schedule_save(cx);
            cx.notify();
        }
    }

    pub(crate) fn open_submit_form(
        &mut self,
        _: &SubmitReview,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if self.writes.is_none() {
            return;
        }
        self.submit.open = true;
        self.submit.summary.focus(window, cx);
        cx.notify();
    }

    fn close_submit_form(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.submit.open = false;
        self.submit.error = None;
        self.focus_diff(window, cx);
        cx.notify();
    }

    fn discard_stranded_drafts(&mut self, cx: &mut Context<Self>) {
        self.stranded_drafts.clear();
        self.save_now(cx);
        cx.notify();
    }

    /// Sends the form: the review, or the comment.
    pub fn submit(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(writes) = self.writes.clone() else {
            return;
        };
        if self.submit.in_flight.is_some() {
            return;
        }
        if let Some(blocked) = self.permissions.blocked.clone() {
            self.submit.error = Some(blocked);
            cx.notify();
            return;
        }
        let body = self.submit.summary.text(cx);
        let (task, submitted_drafts) = match self.submit.mode {
            SubmitMode::Review => {
                let comments = self
                    .draft_cards()
                    .map(|card| card.read(cx).draft(cx))
                    .filter(|draft| draft.added)
                    .collect::<Vec<_>>();
                let verdict = self.submit.verdict;
                if !can_submit(verdict, &body, comments.len()) {
                    self.submit.error =
                        Some("Write a summary or add a comment to the review first.".into());
                    cx.notify();
                    return;
                }
                let ids = comments
                    .iter()
                    .map(|draft| draft.id.clone())
                    .collect::<HashSet<_>>();
                let submission = ReviewSubmission {
                    verdict,
                    body: body.clone(),
                    comments,
                };
                (writes.backend.submit_review(submission, cx), ids)
            }
            SubmitMode::Comment => {
                if body.trim().is_empty() {
                    self.submit.error = Some("Write a comment first.".into());
                    cx.notify();
                    return;
                }
                (
                    writes.backend.post_comment(body.clone(), cx),
                    HashSet::default(),
                )
            }
        };
        let mode = self.submit.mode;
        self.submit.error = None;
        self.submit.in_flight = Some(cx.spawn_in(window, async move |this, cx| {
            let result = task.await;
            this.update_in(cx, |this, window, cx| {
                this.submit.in_flight = None;
                match result {
                    Ok(()) => {
                        for id in &submitted_drafts {
                            this.remove_draft(id, cx);
                        }
                        // Edits made to the summary while it was being sent are kept.
                        if this.submit.summary.text(cx) == body {
                            this.submit.summary.editor().update(cx, |editor, cx| {
                                editor.set_text("", window, cx);
                            });
                        }
                        this.submit.open = false;
                        this.notice = Some(match mode {
                            SubmitMode::Review => "Your review was submitted.".into(),
                            SubmitMode::Comment => "Your comment was posted.".into(),
                        });
                        this.save_now(cx);
                        this.focus_diff(window, cx);
                        this.reload_threads(window, cx);
                    }
                    Err(error) => {
                        log::error!("couldn't submit the review: {error:#}");
                        this.submit.error = Some(format!("{error:#}").into());
                    }
                }
                cx.notify();
            })
            .ok();
        }));
        cx.notify();
    }

    /// The pending comment count and the button that opens the submit form.
    pub(crate) fn render_review_actions(&self, cx: &mut Context<Self>) -> Option<AnyElement> {
        self.writes.as_ref()?;
        let permissions = &self.permissions;
        if !permissions.can_review() && !permissions.conversation_comments {
            return None;
        }
        let count = self.added_draft_count(cx);
        let accent = cx.theme().colors().text_accent;
        Some(
            h_flex()
                .flex_none()
                .gap_2()
                .when(count > 0, |this| {
                    this.child(pill(
                        if count == 1 {
                            "1 pending comment".to_string()
                        } else {
                            format!("{count} pending comments")
                        },
                        accent,
                    ))
                })
                .child(
                    Button::new(
                        "open-submit-review",
                        if permissions.can_review() {
                            "Submit review"
                        } else {
                            "Comment"
                        },
                    )
                    .style(if count > 0 {
                        ButtonStyle::Filled
                    } else {
                        ButtonStyle::Outlined
                    })
                    .size(ButtonSize::Compact)
                    .label_size(LabelSize::Small)
                    .toggle_state(self.submit.open)
                    .on_click(cx.listener(|this, _, window, cx| {
                        if this.submit.open {
                            this.close_submit_form(window, cx);
                        } else {
                            this.open_submit_form(&SubmitReview, window, cx);
                        }
                    })),
                )
                .into_any_element(),
        )
    }

    pub(crate) fn render_submit_form(&self, cx: &mut Context<Self>) -> Option<AnyElement> {
        if !self.submit.open || self.writes.is_none() {
            return None;
        }
        let colors = cx.theme().colors();
        let permissions = self.permissions.clone();
        let mode = self.submit.mode;
        let added = self.added_draft_count(cx);
        let unfinished = self
            .draft_cards()
            .filter(|card| {
                let card = card.read(cx);
                card.is_editing() && !card.draft(cx).added
            })
            .count();
        let stranded = self.stranded_drafts.len();
        let busy = self.submit.in_flight.is_some();
        let body = self.submit.summary.text(cx);
        let ready = match mode {
            SubmitMode::Review => can_submit(self.submit.verdict, &body, added),
            SubmitMode::Comment => !body.trim().is_empty(),
        };

        let both_modes = permissions.can_review() && permissions.conversation_comments;
        let mode_button = |id: &'static str, label: &'static str, button_mode: SubmitMode| {
            Button::new(id, label)
                .style(ButtonStyle::Subtle)
                .size(ButtonSize::Compact)
                .label_size(LabelSize::Small)
                .toggle_state(mode == button_mode)
                .selected_style(ButtonStyle::Tinted(TintColor::Accent))
                .on_click(cx.listener(move |this, _, _, cx| this.set_submit_mode(button_mode, cx)))
        };

        let mut notes = Vec::new();
        if mode == SubmitMode::Review {
            notes.push(match added {
                0 => "No line comments are pending.".to_string(),
                1 => "1 line comment will be posted with the review.".to_string(),
                count => format!("{count} line comments will be posted with the review."),
            });
            if unfinished > 0 {
                notes.push(if unfinished == 1 {
                    "1 comment is still being written and won't be posted until it's added."
                        .to_string()
                } else {
                    format!(
                        "{unfinished} comments are still being written and won't be posted until they're added."
                    )
                });
            }
        } else {
            notes.push("Posts on the conversation, outside any review.".to_string());
        }

        let submit_label = match (mode, self.submit.verdict) {
            (SubmitMode::Comment, _) => "Comment",
            (SubmitMode::Review, ReviewVerdict::Approve) => "Approve",
            (SubmitMode::Review, ReviewVerdict::RequestChanges) => "Request changes",
            (SubmitMode::Review, ReviewVerdict::Comment) => "Submit review",
        };

        Some(
            v_flex()
                .id("submit-review-form")
                .w_full()
                .p_2()
                .gap_2()
                .rounded(tokens::CORNER_RADIUS)
                .border(tokens::BORDER_WIDTH)
                .border_color(colors.border)
                .bg(colors.background)
                .on_action(cx.listener(|this, _: &ConfirmComment, window, cx| {
                    this.submit(window, cx)
                }))
                .on_action(cx.listener(|this, _: &CancelComment, window, cx| {
                    this.close_submit_form(window, cx)
                }))
                .child(
                    h_flex()
                        .gap_1()
                        .child(
                            Label::new(match mode {
                                SubmitMode::Review => "Submit your review",
                                SubmitMode::Comment => "Comment on the pull request",
                            })
                            .size(LabelSize::Small)
                            .weight(FontWeight::SEMIBOLD),
                        )
                        .child(div().flex_1())
                        .when(both_modes, |this| {
                            this.child(mode_button("submit-mode-review", "Review", SubmitMode::Review))
                                .child(mode_button(
                                    "submit-mode-comment",
                                    "Comment only",
                                    SubmitMode::Comment,
                                ))
                        }),
                )
                .child(self.submit.summary.render(cx))
                .when(mode == SubmitMode::Review, |this| {
                    this.child(h_flex().gap_1().children(
                        ReviewVerdict::ALL
                            .into_iter()
                            .filter(|verdict| permissions.verdicts.contains(verdict))
                            .map(|verdict| {
                                Button::new(
                                    SharedString::from(format!("verdict-{verdict:?}")),
                                    verdict.label(),
                                )
                                .style(ButtonStyle::Subtle)
                                .size(ButtonSize::Compact)
                                .label_size(LabelSize::Small)
                                .toggle_state(self.submit.verdict == verdict)
                                .selected_style(ButtonStyle::Tinted(match verdict {
                                    ReviewVerdict::Approve => TintColor::Success,
                                    ReviewVerdict::RequestChanges => TintColor::Error,
                                    ReviewVerdict::Comment => TintColor::Accent,
                                }))
                                .on_click(cx.listener(move |this, _, _, cx| {
                                    this.set_verdict(verdict, cx)
                                }))
                            }),
                    ))
                })
                .children(notes.into_iter().map(|note| {
                    Label::new(note)
                        .size(LabelSize::Small)
                        .color(Color::Muted)
                }))
                .when(stranded > 0 && mode == SubmitMode::Review, |this| {
                    this.child(
                        h_flex()
                            .gap_2()
                            .child(
                                Label::new(if stranded == 1 {
                                    "1 draft no longer matches the diff, so it won't be posted."
                                        .to_string()
                                } else {
                                    format!(
                                        "{stranded} drafts no longer match the diff, so they won't be posted."
                                    )
                                })
                                .size(LabelSize::Small)
                                .color(Color::Warning),
                            )
                            .child(
                                Button::new("discard-stranded", "Discard them")
                                    .style(ButtonStyle::Subtle)
                                    .size(ButtonSize::Compact)
                                    .label_size(LabelSize::Small)
                                    .on_click(
                                        cx.listener(|this, _, _, cx| this.discard_stranded_drafts(cx)),
                                    ),
                            ),
                    )
                })
                .child(
                    h_flex()
                        .gap_1()
                        .children(
                            self.submit
                                .error
                                .as_ref()
                                .or(permissions.blocked.as_ref())
                                .map(error_label),
                        )
                        .child(div().flex_1())
                        .child(cancel_button("cancel-submit").on_click(cx.listener(
                            |this, _, window, cx| this.close_submit_form(window, cx),
                        )))
                        .child(
                            self.submit
                                .summary
                                .confirm_button("submit-review", submit_label, cx)
                                .loading(busy)
                                .disabled(!ready || busy || permissions.blocked.is_some())
                                .when_some(permissions.blocked.clone(), |this, blocked| {
                                    this.tooltip(Tooltip::text(blocked))
                                })
                                .on_click(
                                    cx.listener(|this, _, window, cx| this.submit(window, cx)),
                                ),
                        ),
                )
                .into_any_element(),
        )
    }
}

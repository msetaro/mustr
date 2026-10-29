//! Answering what the agent asks (`mustr-docs/PARITY.md` §2.6), as T3's composer does:
//! - Its questions (`pendingUserInput.ts`, `questionAttachments.ts` and
//!   `chat/ComposerPendingUserInputPanel.tsx` in T3): each question takes one of its options,
//!   or, unless T3 says it doesn't (`allowCustomAnswer: false`), a typed answer. While the agent
//!   waits, the composer answers the question it's on: Enter takes its text, and its images and
//!   files on a server that takes attachments in answers (`questionAttachments`, T3 0.1.3),
//!   then moves to the next question, and answers them all with the last.
//! - Its approvals' headings (`chat/ComposerPendingApprovalPanel.tsx`): one per kind of
//!   request, with the app an MCP server asks for, since T3 asks an MCP server's question
//!   (`mcp-elicitation`) as an approval, and its options with their warnings.
//!
//! An agent that asks an MCP server's form as questions (T3's ACP agents) gets its fields as
//! questions here: options for choices and yes or no, a typed answer for the rest.

use crate::{
    commands,
    model::{ItemKind, Question},
    thread_view::ThreadView,
    timeline::truncate,
};
use collections::HashMap;
use gpui::{
    AnyElement, Context, FontWeight, IntoElement, ParentElement as _, SharedString, Styled as _,
    Window, div, px,
};
use mustr_ui::{palette, tokens, tokens::scaled};
use t3_api::tagged::ChatAttachment;
use ui::{Color, Icon, IconName, IconSize, Label, LabelCommon as _, LabelSize, prelude::*};

/// T3's words when attachments can't go with an answer.
const NO_ATTACHMENTS: &str = "This question can't take attachments.";
const STILL_UPLOADING: &str = "Wait for attachments to finish uploading, or remove failed uploads.";

/// What the composer gave as the answer to one question.
#[derive(Clone, Debug, Default, PartialEq)]
pub(crate) struct TypedAnswer {
    pub text: String,
    /// Images and files uploaded for it, which go in `attachmentsByQuestionId`.
    pub attachments: Vec<ChatAttachment>,
}

/// The composer's answers to the agent's questions, on [`crate::composer::ComposerState`].
#[derive(Default)]
pub(crate) struct AnswerState {
    /// Typed answers, by request id and question id.
    typed: HashMap<String, HashMap<String, TypedAnswer>>,
    /// The question the composer answers, by request id, when the user picked one.
    active: HashMap<String, usize>,
}

impl AnswerState {
    pub(crate) fn typed(&self, request_id: &str, question_id: &str) -> Option<&TypedAnswer> {
        self.typed.get(request_id)?.get(question_id)
    }

    /// Forgets a request's answers, once it's answered.
    pub(crate) fn forget(&mut self, request_id: &str) {
        self.typed.remove(request_id);
        self.active.remove(request_id);
    }

    /// Forgets a question's typed answer, when an option takes its place.
    pub(crate) fn forget_typed(&mut self, request_id: &str, question_id: &str) {
        if let Some(typed) = self.typed.get_mut(request_id) {
            typed.remove(question_id);
        }
    }
}

/// A question's answer, if it has one (`resolvePendingUserInputAnswer`): its typed answer, else
/// its option, else nothing typed but attachments.
pub(crate) fn resolve_answer(
    question: &Question,
    picked: Option<&String>,
    typed: Option<&TypedAnswer>,
) -> Option<String> {
    let custom = typed
        .map(|typed| typed.text.trim())
        .filter(|text| question.allow_custom_answer && !text.is_empty());
    if let Some(custom) = custom {
        return Some(custom.to_string());
    }
    if let Some(picked) =
        picked.filter(|picked| question.options.iter().any(|(_, answer)| answer == *picked))
    {
        return Some(picked.clone());
    }
    (question.allow_custom_answer && typed.is_some_and(|typed| !typed.attachments.is_empty()))
        .then(String::new)
}

/// What T3 calls an approval of each kind (`ComposerPendingApprovalPanel`).
pub(crate) fn approval_label(request_kind: &str) -> &'static str {
    match request_kind {
        "mcp-elicitation" => "App access approval",
        "command" => "Command approval",
        "file-read" => "File read approval",
        "permission" => "App permission approval",
        _ => "File change approval",
    }
}

impl ThreadView {
    /// The question request the composer answers: the first the agent waits on that takes a
    /// typed answer, with its questions.
    pub(crate) fn answer_target(&self) -> Option<(String, Vec<Question>)> {
        let pending = self.thread_state()?.pending_requests();
        pending.into_iter().find_map(|pending| {
            if !pending.is_question || !pending.can_respond {
                return None;
            }
            match pending.item.map(|item| item.kind) {
                Some(ItemKind::Question { questions, .. })
                    if questions
                        .iter()
                        .any(|question| question.allow_custom_answer) =>
                {
                    Some((pending.request_id, questions))
                }
                _ => None,
            }
        })
    }

    fn picked(&self, request_id: &str, question_id: &str) -> Option<&String> {
        self.answers.get(request_id)?.get(question_id)
    }

    fn resolve(&self, request_id: &str, question: &Question) -> Option<String> {
        resolve_answer(
            question,
            self.picked(request_id, &question.id),
            self.composer_state
                .answering
                .typed(request_id, &question.id),
        )
    }

    /// The question the composer answers: the one the user picked, or the first that takes a
    /// typed answer and has none yet, or the first that takes one.
    pub(crate) fn active_question(
        &self,
        request_id: &str,
        questions: &[Question],
    ) -> Option<usize> {
        if let Some(&index) = self.composer_state.answering.active.get(request_id)
            && questions
                .get(index)
                .is_some_and(|question| question.allow_custom_answer)
        {
            return Some(index);
        }
        questions
            .iter()
            .position(|question| {
                question.allow_custom_answer && self.resolve(request_id, question).is_none()
            })
            .or_else(|| {
                questions
                    .iter()
                    .position(|question| question.allow_custom_answer)
            })
    }

    /// Makes the composer answer another question.
    pub(crate) fn activate_question(
        &mut self,
        request_id: String,
        index: usize,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.composer_state
            .answering
            .active
            .insert(request_id, index);
        window.focus(&gpui::Focusable::focus_handle(&self.composer, cx), cx);
        cx.notify();
    }

    /// Whether every question would have an answer, with what's in the composer now as the
    /// active question's.
    pub(crate) fn answers_complete(
        &self,
        request_id: &str,
        questions: &[Question],
        cx: &gpui::App,
    ) -> bool {
        let active = self.active_question(request_id, questions);
        let composer_has_answer =
            !self.composer.read(cx).text(cx).trim().is_empty() || self.images.has_attachments();
        questions.iter().enumerate().all(|(index, question)| {
            (Some(index) == active && composer_has_answer)
                || self.resolve(request_id, question).is_some()
        })
    }

    /// The composer's message answers the question it's on, while the agent waits on one.
    /// Returns whether it did, so the message isn't sent too.
    pub(crate) fn answer_with_composer(
        &mut self,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> bool {
        let Some((request_id, questions)) = self.answer_target() else {
            return false;
        };
        self.answer_request(request_id, questions, window, cx);
        true
    }

    /// Takes the composer's text and attachments as the answer to the active question of
    /// `request_id`, then moves to the next question without an answer, or answers them all.
    pub(crate) fn answer_request(
        &mut self,
        request_id: String,
        questions: Vec<Question>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if self.sending || self.responding.contains(&request_id) {
            return;
        }
        let active = self.active_question(&request_id, &questions);
        let text = self.composer.read(cx).text(cx);
        let has_attachments = self.images.has_attachments();
        if let Some(question) = active.and_then(|index| questions.get(index))
            && (!text.trim().is_empty() || has_attachments)
        {
            if has_attachments {
                let reason = if !self.composer_state.menu.question_attachments() {
                    Some(NO_ATTACHMENTS)
                } else if self.images.uploading() || self.images.any_failed() {
                    Some(STILL_UPLOADING)
                } else {
                    None
                };
                if let Some(reason) = reason {
                    self.error = Some(reason.into());
                    cx.notify();
                    return;
                }
            }
            let attachments = self.images.attachments();
            let sent = self.images.uploaded_keys();
            self.images.clear_sent(&sent);
            // A typed answer takes the place of the option picked for the question.
            if let Some(picked) = self.answers.get_mut(&request_id) {
                picked.remove(&question.id);
            }
            self.composer_state
                .answering
                .typed
                .entry(request_id.clone())
                .or_default()
                .insert(
                    question.id.clone(),
                    TypedAnswer {
                        text: text.trim().to_string(),
                        attachments,
                    },
                );
            self.composer
                .update(cx, |editor, cx| editor.clear(window, cx));
            self.composer_state.answering.active.remove(&request_id);
        }
        self.error = None;
        let answers = questions
            .iter()
            .map(|question| Some((question.id.clone(), self.resolve(&request_id, question)?)))
            .collect::<Option<Vec<_>>>();
        match answers {
            Some(answers) => {
                let attachments = questions
                    .iter()
                    .filter_map(|question| {
                        let typed = self
                            .composer_state
                            .answering
                            .typed(&request_id, &question.id)?;
                        Some((question.id.clone(), typed.attachments.clone()))
                    })
                    .collect();
                let Some(thread_id) = self.thread_id.clone() else {
                    return;
                };
                let command = commands::answer_questions(
                    &thread_id,
                    &request_id,
                    answers,
                    attachments,
                    commands::new_id(),
                );
                self.dispatch(command, Some(request_id), window, cx);
            }
            None => {
                if let Some(next) = self.active_question(&request_id, &questions) {
                    self.composer_state
                        .answering
                        .active
                        .insert(request_id, next);
                }
            }
        }
        cx.notify();
    }

    /// What the composer's placeholder says while it answers a question.
    pub(crate) fn answer_placeholder(&self) -> Option<String> {
        let (request_id, questions) = self.answer_target()?;
        let question = &questions[self.active_question(&request_id, &questions)?];
        let attachments = if self.composer_state.menu.question_attachments() {
            ", or attach files"
        } else {
            ""
        };
        Some(format!(
            "Answer “{}”{attachments}…",
            truncate(&question.question, 60).trim_end_matches('…')
        ))
    }

    /// A question of the agent's, in its card: its header and text, its options, and its
    /// typed answer, or where to type one.
    pub(crate) fn render_question(
        &self,
        request_id: &str,
        index: usize,
        question: &Question,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let palette = palette(cx);
        let answers = self.answers.get(request_id);
        let picked = answers
            .and_then(|answers| answers.get(&question.id))
            .cloned();
        let typed = self
            .composer_state
            .answering
            .typed(request_id, &question.id)
            .cloned();
        let target = self.answer_target();
        let active = target
            .as_ref()
            .filter(|(target, _)| target == request_id)
            .and_then(|(_, questions)| self.active_question(request_id, questions))
            == Some(index);
        let typed_text = typed
            .as_ref()
            .map(|typed| typed.text.trim().to_string())
            .filter(|text| !text.is_empty());
        let attachment_count = typed.as_ref().map_or(0, |typed| typed.attachments.len());
        let note: Option<AnyElement> = if active {
            Some(
                Label::new(if question.options.is_empty() {
                    "Type your answer below."
                } else {
                    "Pick one, or type your answer below."
                })
                .size(LabelSize::XSmall)
                .color(Color::Accent)
                .into_any_element(),
            )
        } else if typed_text.is_some() || attachment_count > 0 {
            let mut parts = Vec::new();
            if let Some(text) = &typed_text {
                parts.push(format!("“{}”", truncate(text, 80)));
            }
            if attachment_count > 0 {
                parts.push(if attachment_count == 1 {
                    "1 attachment".to_string()
                } else {
                    format!("{attachment_count} attachments")
                });
            }
            Some(
                h_flex()
                    .gap_1()
                    .child(
                        Icon::new(if attachment_count > 0 {
                            IconName::Paperclip
                        } else {
                            IconName::Check
                        })
                        .size(IconSize::XSmall)
                        .color(Color::Success),
                    )
                    .child(
                        Label::new(format!("Your answer: {}", parts.join(" · ")))
                            .size(LabelSize::XSmall)
                            .color(Color::Muted),
                    )
                    .into_any_element(),
            )
        } else {
            None
        };
        let selector = format!("thread-question-{request_id}-{}", question.id);
        let header = (!question.header.trim().is_empty()
            && question.header.trim() != question.question.trim())
        .then(|| question.header.trim().to_string());
        let selectable = question.allow_custom_answer && target.is_some();
        v_flex()
            .id(SharedString::from(selector.clone()))
            .debug_selector(|| selector)
            .gap_1()
            .pl(scaled(px(6.)))
            .border_l(px(2.))
            .border_color(if active {
                palette.accent.hsla()
            } else {
                gpui::transparent_black()
            })
            .when(selectable && !active, |this| {
                let request_id = request_id.to_string();
                this.cursor_pointer()
                    .on_click(cx.listener(move |this, _, window, cx| {
                        this.activate_question(request_id.clone(), index, window, cx)
                    }))
            })
            .when_some(header, |this, header| {
                this.child(
                    div()
                        .text_size(tokens::text_size(tokens::TEXT_SIZE_XSMALL))
                        .font_weight(FontWeight::MEDIUM)
                        .text_color(palette.text_muted.hsla())
                        .child(header),
                )
            })
            .child(Label::new(question.question.clone()).size(LabelSize::Small))
            .when(!question.options.is_empty(), |this| {
                this.child(
                    h_flex()
                        .flex_wrap()
                        .gap_1()
                        .children(question.options.iter().enumerate().map(
                            |(option, (label, answer))| {
                                let selected = typed_text.is_none()
                                    && picked.as_deref() == Some(answer.as_str());
                                let request_id = request_id.to_string();
                                let question_id = question.id.clone();
                                let answer = answer.clone();
                                mustr_ui::Chip::new(
                                    SharedString::from(format!(
                                        "option-{request_id}-{}-{option}",
                                        question.id
                                    )),
                                    label.clone(),
                                )
                                .accent(selected)
                                .on_click(cx.listener(
                                    move |this, _, _, cx| {
                                        this.pick_option(
                                            request_id.clone(),
                                            question_id.clone(),
                                            answer.clone(),
                                            cx,
                                        )
                                    },
                                ))
                            },
                        )),
                )
            })
            .children(note)
            .into_any_element()
    }

    /// Picks an option for a question; it takes the place of a typed answer.
    pub(crate) fn pick_option(
        &mut self,
        request_id: String,
        question_id: String,
        answer: String,
        cx: &mut Context<Self>,
    ) {
        self.composer_state
            .answering
            .forget_typed(&request_id, &question_id);
        self.answers
            .entry(request_id)
            .or_default()
            .insert(question_id, answer);
        cx.notify();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use pretty_assertions::assert_eq;

    fn question(options: &[&str], allow_custom_answer: bool) -> Question {
        Question {
            id: "q1".into(),
            header: "Database".into(),
            question: "Which database?".into(),
            options: options
                .iter()
                .map(|option| (option.to_string(), option.to_string()))
                .collect(),
            allow_custom_answer,
        }
    }

    fn typed(text: &str, attachments: usize) -> TypedAnswer {
        TypedAnswer {
            text: text.into(),
            attachments: (0..attachments)
                .map(|index| {
                    commands::file_attachment(
                        &format!("a{index}"),
                        "notes.txt",
                        "text/plain",
                        3,
                        false,
                    )
                })
                .collect(),
        }
    }

    #[test]
    fn answers_resolve_as_t3s_do() {
        let both = question(&["Postgres", "SQLite"], true);
        let postgres = "Postgres".to_string();
        // A typed answer wins over an option, and an option over nothing.
        assert_eq!(
            resolve_answer(&both, Some(&postgres), Some(&typed(" MySQL ", 0))),
            Some("MySQL".into())
        );
        assert_eq!(
            resolve_answer(&both, Some(&postgres), None),
            Some("Postgres".into())
        );
        assert_eq!(resolve_answer(&both, None, None), None);
        // Attachments alone answer with nothing typed.
        assert_eq!(
            resolve_answer(&both, None, Some(&typed("", 2))),
            Some(String::new())
        );
        // An option the question doesn't have isn't an answer.
        assert_eq!(resolve_answer(&both, Some(&"MySQL".into()), None), None);
        // A question that takes only its options ignores what's typed.
        let options_only = question(&["Postgres", "SQLite"], false);
        assert_eq!(
            resolve_answer(&options_only, None, Some(&typed("MySQL", 1))),
            None
        );
    }

    #[test]
    fn approvals_are_labelled_by_kind_as_t3_labels_them() {
        assert_eq!(approval_label("mcp-elicitation"), "App access approval");
        assert_eq!(approval_label("command"), "Command approval");
        assert_eq!(approval_label("file-read"), "File read approval");
        assert_eq!(approval_label("file-change"), "File change approval");
        assert_eq!(approval_label("permission"), "App permission approval");
    }
}

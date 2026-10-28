//! A thread's composer: the message editor and its buttons, which send, queue, steer, and stop,
//! and above it the agent's pending requests, to approve or answer, and the queued messages.

use crate::{
    commands::{self, CommandIds, Delivery},
    model::{ItemKind, PendingRequest, QueuedMessage, ThreadState},
    thread_view::{CONTENT_MAX_WIDTH, Loadable, ThreadView},
    timeline::truncate,
};
use gpui::{
    AnyElement, App, Context, Focusable as _, FontWeight, IntoElement, ParentElement as _,
    SharedString, Styled as _, Window, actions, div, px,
};
use mustr_ui::{ButtonStyle as MustrButtonStyle, Card, TextButton, palette, tokens};
use t3_api::types::ProviderApprovalDecision;
use ui::{
    Color, Icon, IconName, IconSize, Label, LabelCommon as _, LabelSize, Tooltip, prelude::*,
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
    ]
);

pub(crate) const COMPOSER_CONTEXT: &str = "ThreadComposer";
pub(crate) const COMPOSER_MIN_LINES: usize = 2;
pub(crate) const COMPOSER_MAX_LINES: usize = 12;

impl ThreadView {
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

    pub(crate) fn render_composer(
        &self,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> impl IntoElement {
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
                                    .child(self.settings_bar.clone())
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

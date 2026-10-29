//! A pull request's review guide (`mustr-docs/PARITY.md` §2.11), as T3's
//! `pullRequest/PullRequestGuideTab.tsx` has it: the stops an agent wrote for reading the change
//! in order, each with its summary, what needs attention, its files, and the questions it
//! leaves (`pullRequests.reviewGuide`).
//!
//! T3 keeps one guide per pull request, shared by everyone on the server, and written by an
//! agent: Write guide (`pullRequests.generateReviewGuide`, which needs `orchestration:operate`
//! because it runs a model on T3's agents) starts it, T3 answers at once, and mustr asks again
//! every five seconds until it's done, as T3's `PullRequestGuideRunCoordinator.tsx` does. A guide
//! of an older head says so and offers Rewrite. Agents write the same guide with T3's
//! `write_pull_request_guides` tool.
//!
//! The guide shows in a dialog over the review: Next and Previous (or `j` and `k`) step through
//! the stops, and a stop's files scroll the review to them. Debug builds can show one from a file
//! (`MUSTR_DEBUG_REVIEW_GUIDE`).

use crate::pull_request_review;
use anyhow::Result;
use gpui::{
    App, AppContext as _, Context, DismissEvent, EventEmitter, FocusHandle, Focusable, IntoElement,
    KeyBinding, ParentElement as _, Render, SharedString, Styled as _, Task, WeakEntity, Window,
    actions, div, px, rems,
};
use mustr_ui::{ButtonStyle, Chip, TextButton, palette, tokens, tokens::scaled};
use std::time::Duration;
use t3_client::{
    PullRequestRef,
    t3_api::{
        rpc,
        tagged::PullRequestReviewGuideResultRun,
        types::{
            PullRequestReviewGuide, PullRequestReviewGuideAttentionKind,
            PullRequestReviewGuideResult, PullRequestReviewGuideStop,
        },
    },
};
use thread_chat::{Access, ThreadBackendExt as _, is_unsupported_rpc};
use ui::{Color, Label, LabelCommon as _, LabelSize, prelude::*};
use workspace::{ModalView, Workspace};

actions!(
    review_guide,
    [
        /// Shows the guide's next stop.
        NextStop,
        /// Shows the guide's previous stop.
        PreviousStop,
    ]
);

/// How often a guide being written is asked after (`PullRequestGuideRunCoordinator.tsx`).
pub const POLL_INTERVAL: Duration = Duration::from_secs(5);

pub fn key_bindings() -> Vec<KeyBinding> {
    vec![
        KeyBinding::new("j", NextStop, Some("ReviewGuide")),
        KeyBinding::new("k", PreviousStop, Some("ReviewGuide")),
        KeyBinding::new("down", NextStop, Some("ReviewGuide")),
        KeyBinding::new("up", PreviousStop, Some("ReviewGuide")),
    ]
}

/// What T3 has of the pull request's guide.
#[derive(Clone, Debug, Default, PartialEq)]
pub enum GuideState {
    /// Not asked yet, or the server has no guides.
    #[default]
    Unknown,
    Loaded(PullRequestReviewGuideResult),
    Failed(SharedString),
}

impl GuideState {
    pub fn result(&self) -> Option<&PullRequestReviewGuideResult> {
        match self {
            Self::Loaded(result) => Some(result),
            _ => None,
        }
    }

    pub fn guide(&self) -> Option<&PullRequestReviewGuide> {
        self.result()?.guide.as_ref()
    }

    /// Whether an agent is writing it now.
    pub fn is_running(&self) -> bool {
        matches!(
            self.result().and_then(|result| result.run.as_ref()),
            Some(PullRequestReviewGuideResultRun::Running(_))
        )
    }

    /// Why the last attempt failed, if it did.
    pub fn failure(&self) -> Option<&str> {
        match self.result()?.run.as_ref()? {
            PullRequestReviewGuideResultRun::Failed(failed) => Some(&failed.detail),
            _ => None,
        }
    }

    /// Whether the guide is of an older head than the pull request's.
    pub fn is_stale(&self) -> bool {
        let Some(result) = self.result() else {
            return false;
        };
        match (&result.guide, &result.current_head_sha) {
            (Some(guide), Some(head)) => &guide.head_sha != head,
            _ => false,
        }
    }

    /// The details row's chip: its label, and its tooltip.
    pub fn chip(&self) -> Option<(String, String)> {
        let result = self.result()?;
        if self.is_running() {
            return Some((
                "Writing guide…".to_string(),
                "An agent is writing the review guide".to_string(),
            ));
        }
        match &result.guide {
            Some(guide) => {
                let stops = match guide.stops.len() {
                    1 => "1 stop".to_string(),
                    count => format!("{count} stops"),
                };
                if self.is_stale() {
                    Some((
                        format!("Guide · {stops} · outdated"),
                        "Written for an older head of the pull request".to_string(),
                    ))
                } else {
                    Some((
                        format!("Guide · {stops}"),
                        "Read the change in the order the guide suggests".to_string(),
                    ))
                }
            }
            None => Some((
                "Guide".to_string(),
                match self.failure() {
                    Some(failure) => format!("Writing the guide failed: {failure}"),
                    None => "No review guide yet".to_string(),
                },
            )),
        }
    }
}

/// Reads the guide (`pullRequests.reviewGuide`). A server without guides has none.
///
/// Debug builds can show a guide from a file instead (`MUSTR_DEBUG_REVIEW_GUIDE=<result.json>`,
/// T3's `PullRequestReviewGuideResult`), to look at the dialog without an agent writing one.
pub fn load(reference: &PullRequestRef, cx: &App) -> Task<GuideState> {
    #[cfg(debug_assertions)]
    if let Some(path) = std::env::var_os("MUSTR_DEBUG_REVIEW_GUIDE") {
        let state = std::fs::read_to_string(&path)
            .map_err(anyhow::Error::from)
            .and_then(|text| Ok(serde_json::from_str(&text)?))
            .map(GuideState::Loaded)
            .unwrap_or_else(|error: anyhow::Error| {
                GuideState::Failed(format!("{}: {error:#}", path.display()).into())
            });
        return Task::ready(state);
    }
    let Some(backend) = thread_chat::backend(cx) else {
        return Task::ready(GuideState::Unknown);
    };
    let call = backend.call::<rpc::PullRequestsReviewGuide>(reference.clone(), Access::Read);
    cx.background_spawn(async move {
        match call.await {
            Ok(result) => GuideState::Loaded(result),
            Err(error) if is_unsupported_rpc(&error) => GuideState::Unknown,
            Err(error) => {
                log::warn!("couldn't read the review guide: {error:#}");
                GuideState::Failed(format!("{error:#}").into())
            }
        }
    })
}

/// Asks an agent to write (or rewrite) the guide (`pullRequests.generateReviewGuide`).
pub fn generate(
    reference: &PullRequestRef,
    cx: &App,
) -> Task<Result<PullRequestReviewGuideResult>> {
    let Some(backend) = thread_chat::backend(cx) else {
        return Task::ready(Err(anyhow::anyhow!("mustr isn't connected to T3")));
    };
    let call =
        backend.call::<rpc::PullRequestsGenerateReviewGuide>(reference.clone(), Access::Operate);
    cx.background_spawn(call)
}

/// Why Write guide can't run now, if it can't: writing one runs a model, which needs
/// `orchestration:operate`.
pub fn write_blocked_reason(cx: &App) -> Option<String> {
    match thread_chat::backend(cx) {
        Some(backend) => backend.blocked_reason(Access::Operate),
        None => Some("mustr isn't connected to T3".to_string()),
    }
}

fn attention_label(kind: &PullRequestReviewGuideAttentionKind) -> &'static str {
    match kind {
        PullRequestReviewGuideAttentionKind::Tradeoff => "A tradeoff to weigh",
        PullRequestReviewGuideAttentionKind::Contract => "A contract that changes",
        PullRequestReviewGuideAttentionKind::Product => "A product decision",
        PullRequestReviewGuideAttentionKind::Risk => "A risk to check",
        PullRequestReviewGuideAttentionKind::Accessibility => "Accessibility",
        PullRequestReviewGuideAttentionKind::Unrecognized(_) => "Worth a look",
    }
}

// ---------------------------------------------------------------------------------------------
// The dialog

/// The guide over the review: one stop at a time.
pub struct ReviewGuideDialog {
    reference: PullRequestRef,
    state: GuideState,
    stop: usize,
    review: WeakEntity<review::ReviewView>,
    /// Tells the details row what the guide is now.
    on_change: Box<dyn Fn(GuideState, &mut App)>,
    error: Option<SharedString>,
    focus_handle: FocusHandle,
    _poll: Option<Task<()>>,
}

impl ReviewGuideDialog {
    pub fn new(
        reference: PullRequestRef,
        state: GuideState,
        review: WeakEntity<review::ReviewView>,
        on_change: impl Fn(GuideState, &mut App) + 'static,
        cx: &mut Context<Self>,
    ) -> Self {
        let mut this = Self {
            reference,
            state,
            stop: 0,
            review,
            on_change: Box::new(on_change),
            error: None,
            focus_handle: cx.focus_handle(),
            _poll: None,
        };
        this.poll_while_running(cx);
        this
    }

    #[cfg(test)]
    pub fn stop(&self) -> usize {
        self.stop
    }

    fn stops(&self) -> &[PullRequestReviewGuideStop] {
        self.state
            .guide()
            .map(|guide| guide.stops.as_slice())
            .unwrap_or_default()
    }

    pub fn next_stop(&mut self, _: &NextStop, _: &mut Window, cx: &mut Context<Self>) {
        if self.stop + 1 < self.stops().len() {
            self.stop += 1;
            cx.notify();
        }
    }

    pub fn previous_stop(&mut self, _: &PreviousStop, _: &mut Window, cx: &mut Context<Self>) {
        if self.stop > 0 {
            self.stop -= 1;
            cx.notify();
        }
    }

    fn set_state(&mut self, state: GuideState, cx: &mut Context<Self>) {
        (self.on_change)(state.clone(), cx);
        self.state = state;
        self.stop = self.stop.min(self.stops().len().saturating_sub(1));
        self.poll_while_running(cx);
        cx.notify();
    }

    /// Asks again every few seconds while an agent writes the guide.
    fn poll_while_running(&mut self, cx: &mut Context<Self>) {
        if !self.state.is_running() {
            self._poll = None;
            return;
        }
        let reference = self.reference.clone();
        self._poll = Some(cx.spawn(async move |this, cx| {
            cx.background_executor().timer(POLL_INTERVAL).await;
            let state = cx.update(|cx| load(&reference, cx)).await;
            this.update(cx, |this, cx| this.set_state(state, cx)).ok();
        }));
    }

    /// Write guide, or Rewrite.
    pub fn write(&mut self, cx: &mut Context<Self>) {
        if self.state.is_running() {
            return;
        }
        if let Some(reason) = write_blocked_reason(cx) {
            self.error = Some(reason.into());
            cx.notify();
            return;
        }
        self.error = None;
        let generate = generate(&self.reference, cx);
        cx.spawn(async move |this, cx| {
            let result = generate.await;
            this.update(cx, |this, cx| match result {
                Ok(result) => this.set_state(GuideState::Loaded(result), cx),
                Err(error) => {
                    log::error!("couldn't start the review guide: {error:#}");
                    this.error = Some(format!("Couldn't write the guide: {error:#}").into());
                    cx.notify();
                }
            })
            .ok();
        })
        .detach();
    }

    fn open_file(&mut self, path: String, window: &mut Window, cx: &mut Context<Self>) {
        let shown = self
            .review
            .update(cx, |review, cx| review.scroll_to_path(&path, window, cx))
            .unwrap_or(false);
        if shown {
            cx.emit(DismissEvent);
        } else {
            self.error = Some(format!("The review doesn't show {path}").into());
            cx.notify();
        }
    }

    fn render_stop(
        &self,
        stop: &PullRequestReviewGuideStop,
        cx: &mut Context<Self>,
    ) -> impl IntoElement {
        let palette = palette(cx);
        let mut files = stop
            .files
            .iter()
            .map(|file| (file.path.clone(), file.note.clone()))
            .collect::<Vec<_>>();
        for excerpt in &stop.excerpts {
            if !files.iter().any(|(path, _)| path == &excerpt.path) {
                files.push((excerpt.path.clone(), Some(excerpt.caption.clone())));
            }
        }
        v_flex()
            .gap_3()
            .child(
                Label::new(stop.title.clone())
                    .size(LabelSize::Large)
                    .weight(gpui::FontWeight::SEMIBOLD),
            )
            .child(
                div()
                    .text_color(palette.text.hsla())
                    .child(stop.summary.clone()),
            )
            .children(stop.attention.as_ref().map(|attention| {
                v_flex()
                    .gap_1()
                    .px_3()
                    .py_2()
                    .rounded(scaled(tokens::CONTROL_RADIUS))
                    .border(tokens::BORDER_WIDTH)
                    .border_color(palette.warning.hsla().opacity(0.5))
                    .bg(palette.warning.hsla().opacity(0.08))
                    .child(
                        Label::new(attention_label(&attention.kind))
                            .size(LabelSize::Small)
                            .color(Color::Warning),
                    )
                    .child(div().child(attention.note.clone()))
            }))
            .when(!files.is_empty(), |this| {
                this.child(
                    v_flex()
                        .gap_1()
                        .child(
                            Label::new("Files in this stop")
                                .size(LabelSize::Small)
                                .color(Color::Muted),
                        )
                        .children(files.into_iter().enumerate().map(|(ix, (path, note))| {
                            let open = path.clone();
                            h_flex()
                                .gap_2()
                                .child(
                                    Chip::new(("guide-file", ix), path)
                                        .tooltip("Show it in the review")
                                        .on_click(cx.listener(move |this, _, window, cx| {
                                            this.open_file(open.clone(), window, cx)
                                        })),
                                )
                                .children(note.map(|note| {
                                    div().min_w_0().flex_1().child(
                                        Label::new(note)
                                            .size(LabelSize::Small)
                                            .color(Color::Muted)
                                            .truncate(),
                                    )
                                }))
                        })),
                )
            })
            .when(!stop.questions.is_empty(), |this| {
                this.child(
                    v_flex()
                        .gap_1()
                        .child(
                            Label::new("Questions to ask")
                                .size(LabelSize::Small)
                                .color(Color::Muted),
                        )
                        .children(
                            stop.questions
                                .iter()
                                .map(|question| div().child(format!("• {question}"))),
                        ),
                )
            })
    }
}

impl EventEmitter<DismissEvent> for ReviewGuideDialog {}

impl ModalView for ReviewGuideDialog {}

impl Focusable for ReviewGuideDialog {
    fn focus_handle(&self, _: &App) -> FocusHandle {
        self.focus_handle.clone()
    }
}

impl Render for ReviewGuideDialog {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let palette = palette(cx);
        let stops = self.stops().to_vec();
        let current = stops.get(self.stop).cloned();
        let blocked = write_blocked_reason(cx);
        let write_label = if self.state.is_running() {
            "Writing…"
        } else if self.state.guide().is_some() {
            "Rewrite"
        } else if self.state.failure().is_some() {
            "Try again"
        } else {
            "Write guide"
        };
        let write = TextButton::new("review-guide-write", write_label)
            .disabled(self.state.is_running() || blocked.is_some())
            .tooltip(blocked.unwrap_or_else(|| {
                "An agent on T3 reads the change and writes the guide (it runs a model)".to_string()
            }))
            .on_click(cx.listener(|this, _, _, cx| this.write(cx)));
        let status = if self.state.is_running() {
            Some("An agent is reading the change…".to_string())
        } else if let Some(failure) = self.state.failure() {
            Some(format!("Writing the guide failed: {failure}"))
        } else if self.state.is_stale() {
            Some("This guide was written for an older head of the pull request.".to_string())
        } else if stops.is_empty() {
            Some(
                "No guide yet. An agent can write one: the stops to read the change in, \
                 and what to look at in each."
                    .to_string(),
            )
        } else {
            None
        };
        let rail = v_flex()
            .id("review-guide-stops")
            .w(scaled(px(200.)))
            .flex_none()
            .h_full()
            .overflow_y_scroll()
            .gap_0p5()
            .children(stops.iter().enumerate().map(|(ix, stop)| {
                let selected = ix == self.stop;
                h_flex()
                    .id(("guide-stop", ix))
                    .px_2()
                    .py_1()
                    .gap_2()
                    .rounded(scaled(tokens::CONTROL_RADIUS))
                    .when(selected, |this| {
                        this.bg(palette.accent.hsla().opacity(0.14))
                    })
                    .hover(|this| this.bg(palette.text.hsla().opacity(0.06)))
                    .cursor_pointer()
                    .child(
                        Label::new(format!("{}", ix + 1))
                            .size(LabelSize::Small)
                            .color(if selected {
                                Color::Accent
                            } else {
                                Color::Muted
                            }),
                    )
                    .child(
                        div().min_w_0().flex_1().child(
                            Label::new(stop.title.clone())
                                .size(LabelSize::Small)
                                .truncate(),
                        ),
                    )
                    .on_click(cx.listener(move |this, _, _, cx| {
                        this.stop = ix;
                        cx.notify();
                    }))
            }));
        let next = stops.get(self.stop + 1).map(|stop| stop.title.clone());
        v_flex()
            .key_context("ReviewGuide")
            .track_focus(&self.focus_handle)
            .on_action(cx.listener(Self::next_stop))
            .on_action(cx.listener(Self::previous_stop))
            .on_action(cx.listener(|_, _: &menu::Cancel, _, cx| cx.emit(DismissEvent)))
            .w(tokens::modal_width(rems(60.), window))
            .max_h(window.viewport_size().height * 0.75)
            .p_4()
            .gap_3()
            .rounded(scaled(tokens::CORNER_RADIUS))
            .border(tokens::BORDER_WIDTH)
            .border_color(palette.border.hsla())
            .bg(palette.surface.hsla())
            .shadow_lg()
            .child(
                h_flex()
                    .gap_2()
                    .child(
                        v_flex()
                            .flex_1()
                            .min_w_0()
                            .gap_0p5()
                            .child(
                                Label::new("Review guide")
                                    .size(LabelSize::Large)
                                    .weight(gpui::FontWeight::SEMIBOLD),
                            )
                            .child(
                                Label::new(match self.state.guide() {
                                    Some(guide) => format!(
                                        "{} · written for {}",
                                        pull_request_review::display_ref(&self.reference),
                                        guide.head_sha.chars().take(7).collect::<String>()
                                    ),
                                    None => pull_request_review::display_ref(&self.reference),
                                })
                                .size(LabelSize::Small)
                                .color(Color::Muted),
                            ),
                    )
                    .child(write),
            )
            .children(status.map(|status| {
                Label::new(status)
                    .size(LabelSize::Small)
                    .color(Color::Muted)
            }))
            .children(
                self.error
                    .clone()
                    .map(|error| Label::new(error).size(LabelSize::Small).color(Color::Error)),
            )
            // The stops and the one shown scroll, so the buttons stay in the window.
            .children(current.map(|stop| {
                h_flex()
                    .flex_1()
                    .min_h_0()
                    .items_start()
                    .gap_4()
                    .child(rail)
                    .child(
                        v_flex()
                            .id("review-guide-stop")
                            .flex_1()
                            .min_w_0()
                            .h_full()
                            .overflow_y_scroll()
                            .child(self.render_stop(&stop, cx)),
                    )
            }))
            .when(!stops.is_empty(), |this| {
                this.child(
                    h_flex()
                        .justify_end()
                        .gap_2()
                        .child(
                            TextButton::new("review-guide-previous", "Previous")
                                .disabled(self.stop == 0)
                                .on_click(cx.listener(|this, _, window, cx| {
                                    this.previous_stop(&PreviousStop, window, cx)
                                })),
                        )
                        .child(match next {
                            Some(next) => {
                                TextButton::new("review-guide-next", format!("Next: {next}"))
                                    .style(ButtonStyle::Primary)
                                    .on_click(cx.listener(|this, _, window, cx| {
                                        this.next_stop(&NextStop, window, cx)
                                    }))
                            }
                            None => TextButton::new("review-guide-next", "Finish review")
                                .style(ButtonStyle::Primary)
                                .on_click(cx.listener(|_, _, _, cx| cx.emit(DismissEvent))),
                        }),
                )
            })
    }
}

/// Opens the guide over the review.
pub fn open(
    reference: PullRequestRef,
    state: GuideState,
    review: WeakEntity<review::ReviewView>,
    on_change: impl Fn(GuideState, &mut App) + 'static,
    workspace: &mut Workspace,
    window: &mut Window,
    cx: &mut Context<Workspace>,
) {
    workspace.toggle_modal(window, cx, |_, cx| {
        ReviewGuideDialog::new(reference, state, review, on_change, cx)
    });
}

#[cfg(test)]
#[path = "pull_request_guide_tests.rs"]
mod tests;

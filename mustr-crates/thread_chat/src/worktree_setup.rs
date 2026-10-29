//! A thread's worktree setup card, above the timeline: T3 preparing the thread's worktree and
//! running its setup script, with each stage's progress, the script's last lines, Cancel, and
//! Retry (`mustr-docs/PARITY.md` §2.5), as T3's `chat/WorktreeSetupCard.tsx` shows it.
//!
//! - It follows `subscribeWorktreeSetup` once the thread has a run T3 is preparing, as T3's
//!   `useWorktreeSetup.ts` does, and keeps the newest snapshot by `sequence`. T3 sends `null`
//!   when there's no setup, and 30 seconds after one ends.
//! - It shows while the setup runs, after it fails or is cancelled, and after it finishes with a
//!   failed setup script (`resolveVisibleWorktreeSetup`).
//! - Cancel sends `worktreeSetup.cancel`. Retry sends `prepared-run.retry` for the run whose
//!   preparation failed; T3's app offers it on that run's error row, and since T3 forgets a
//!   setup 30 seconds after it ends, the card stays for such a run even without a snapshot.
//! - Debug builds can play setup snapshots from a file into every thread's card
//!   (`MUSTR_DEBUG_WORKTREE_SETUP=<snapshots.json>`, a snapshot or a list played two seconds
//!   apart), to look at the card without launching a thread, which would start an agent.

use crate::{
    backend::{Access, ThreadBackend, ThreadBackendExt as _, is_unsupported_rpc},
    commands,
    thread_view::{CONTENT_MAX_WIDTH, ThreadView},
};
use futures::StreamExt as _;
use gpui::{
    AnyElement, App, Context, FontWeight, Hsla, InteractiveElement as _, IntoElement,
    ParentElement as _, Pixels, Render, SharedString, StatefulInteractiveElement as _, Styled as _,
    Subscription, Task, WeakEntity, Window, div, px,
};
use mustr_ui::{Card, TextButton, palette, tokens, tokens::scaled};
use std::{sync::Arc, time::Duration};
use t3_api::{
    rpc,
    tagged::OrchestrationV2Command,
    types::{
        OrchestrationV2CommandPreparedRunRetry, OrchestrationV2CommandPreparedRunRetryType,
        OrchestrationV2Run, OrchestrationV2RunStatus, WorktreeSetupCancelInput, WorktreeSetupPhase,
        WorktreeSetupSnapshot, WorktreeSetupStage, WorktreeSetupStageId, WorktreeSetupStageStatus,
        WorktreeSetupSubscribeInput,
    },
};
use time::{OffsetDateTime, format_description::well_known::Rfc3339};
use ui::{Color, Icon, IconName, IconSize, Label, LabelCommon as _, LabelSize, prelude::*};

/// The most of the window's height the card takes.
const MAX_HEIGHT_SHARE: f32 = 0.3;
/// How many of the setup script's last lines show (T3's server keeps 4).
const TAIL_LINES: usize = 4;

/// The worktree setup of one [`ThreadView`]'s thread.
pub struct WorktreeSetupCard {
    thread_view: WeakEntity<ThreadView>,
    /// The thread the card follows, once it has a run T3 prepares.
    thread_id: Option<String>,
    following: bool,
    setup: Option<WorktreeSetupSnapshot>,
    /// The run whose workspace preparation failed, which Retry prepares again.
    failed_run: Option<String>,
    show_details: bool,
    /// A Cancel or Retry on its way.
    sending: bool,
    error: Option<SharedString>,
    _follow: Option<Task<()>>,
    /// Redraws the elapsed times each second while the setup runs.
    _tick: Option<Task<()>>,
    _subscriptions: Vec<Subscription>,
}

impl WorktreeSetupCard {
    pub fn new(
        thread_view: WeakEntity<ThreadView>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        // The view is still being built, so it can only be observed once it exists.
        cx.defer_in(window, |this, _, cx| {
            if let Some(view) = this.thread_view.upgrade() {
                this._subscriptions
                    .push(cx.observe(&view, |this, _, cx| this.refresh(cx)));
                this.refresh(cx);
            }
        });
        Self {
            thread_view,
            thread_id: None,
            following: false,
            setup: None,
            failed_run: None,
            show_details: false,
            sending: false,
            error: None,
            _follow: None,
            _tick: None,
            _subscriptions: Vec::new(),
        }
    }

    /// Follows the view's thread: a draft becomes a thread when it launches.
    fn refresh(&mut self, cx: &mut Context<Self>) {
        let Some(view) = self.thread_view.upgrade() else {
            return;
        };
        let view = view.read(cx);
        let backend = view.backend.clone();
        let thread_id = view.thread_id().map(str::to_string);
        let latest_run = view
            .thread_state()
            .and_then(|state| state.latest_executed_run());
        let preparing =
            latest_run.is_some_and(|run| run.status == OrchestrationV2RunStatus::Preparing);
        let failed_run = latest_run.and_then(failed_preparation).map(str::to_string);
        if thread_id != self.thread_id {
            self.thread_id = thread_id;
            self.following = false;
            self.setup = None;
            self.show_details = false;
            self.error = None;
            self._follow = None;
            cx.notify();
        }
        if failed_run != self.failed_run {
            self.failed_run = failed_run;
            cx.notify();
        }
        #[cfg(debug_assertions)]
        if self.thread_id.is_some()
            && !self.following
            && let Some(snapshots) = debug_snapshots()
        {
            self.following = true;
            self._follow = Some(cx.spawn(async move |this, cx| {
                for snapshot in snapshots {
                    if this
                        .update(cx, |this, cx| this.apply(snapshot, cx))
                        .is_err()
                    {
                        return;
                    }
                    cx.background_executor().timer(Duration::from_secs(2)).await;
                }
            }));
        }
        if let Some(thread_id) = self.thread_id.clone()
            && preparing
            && !self.following
        {
            self.following = true;
            self.follow(backend, thread_id, cx);
        }
    }

    /// Follows `subscribeWorktreeSetup`, again after a dropped connection.
    fn follow(
        &mut self,
        backend: Arc<dyn ThreadBackend>,
        thread_id: String,
        cx: &mut Context<Self>,
    ) {
        self._follow = Some(cx.spawn(async move |this, cx| {
            let mut attempt = 0;
            loop {
                let input = WorktreeSetupSubscribeInput {
                    thread_id: thread_id.clone(),
                };
                match backend.stream::<rpc::SubscribeWorktreeSetup>(input, Access::Read) {
                    Ok(mut stream) => {
                        while let Some(item) = stream.next().await {
                            match item {
                                Ok(snapshot) => {
                                    attempt = 0;
                                    let snapshot = snapshot.map(Into::into);
                                    if this
                                        .update(cx, |this, cx| this.apply(snapshot, cx))
                                        .is_err()
                                    {
                                        return;
                                    }
                                }
                                Err(error) if is_unsupported_rpc(&error) => return,
                                Err(error) => {
                                    log::info!("lost the worktree setup of {thread_id}: {error:#}");
                                    break;
                                }
                            }
                        }
                    }
                    Err(error) => log::info!("can't follow the worktree setup: {error:#}"),
                }
                cx.background_executor()
                    .timer(crate::backend::retry_delay(attempt))
                    .await;
                attempt += 1;
            }
        }));
    }

    /// Keeps the newest snapshot; `null` clears it.
    fn apply(&mut self, snapshot: Option<WorktreeSetupSnapshot>, cx: &mut Context<Self>) {
        let newer = match (&snapshot, &self.setup) {
            (Some(next), Some(current)) => next.sequence >= current.sequence,
            _ => true,
        };
        if !newer {
            return;
        }
        let running = snapshot
            .as_ref()
            .is_some_and(|setup| setup.phase == WorktreeSetupPhase::Running);
        self.setup = snapshot;
        if running && self._tick.is_none() {
            self._tick = Some(cx.spawn(async move |this, cx| {
                loop {
                    cx.background_executor().timer(Duration::from_secs(1)).await;
                    let Ok(running) = this.update(cx, |this, cx| {
                        cx.notify();
                        this.setup
                            .as_ref()
                            .is_some_and(|setup| setup.phase == WorktreeSetupPhase::Running)
                    }) else {
                        return;
                    };
                    if !running {
                        break;
                    }
                }
                this.update(cx, |this, _| this._tick = None).ok();
            }));
        }
        cx.notify();
    }

    fn backend(&self, cx: &App) -> Option<Arc<dyn ThreadBackend>> {
        Some(self.thread_view.upgrade()?.read(cx).backend.clone())
    }

    fn cancel(&mut self, cx: &mut Context<Self>) {
        let (Some(backend), Some(thread_id)) = (self.backend(cx), self.thread_id.clone()) else {
            return;
        };
        self.sending = true;
        self.error = None;
        let call = backend.call::<rpc::WorktreeSetupCancel>(
            WorktreeSetupCancelInput { thread_id },
            Access::Operate,
        );
        cx.spawn(async move |this, cx| {
            let result = call.await;
            this.update(cx, |this, cx| {
                this.sending = false;
                this.error = match result {
                    Ok(result) if result.cancelled => None,
                    Ok(_) => Some("The setup is past the point where it can be cancelled.".into()),
                    Err(error) => Some(format!("Couldn't cancel the setup: {error:#}").into()),
                };
                cx.notify();
            })
            .ok();
        })
        .detach();
        cx.notify();
    }

    fn retry(&mut self, cx: &mut Context<Self>) {
        let (Some(backend), Some(thread_id), Some(run_id)) = (
            self.backend(cx),
            self.thread_id.clone(),
            self.failed_run.clone(),
        ) else {
            return;
        };
        self.sending = true;
        self.error = None;
        let dispatch = backend.dispatch(retry_command(&thread_id, &run_id, commands::new_id()));
        cx.spawn(async move |this, cx| {
            let result = dispatch.await;
            this.update(cx, |this, cx| {
                this.sending = false;
                if let Err(error) = result {
                    this.error = Some(format!("Couldn't retry the setup: {error:#}").into());
                }
                cx.notify();
            })
            .ok();
        })
        .detach();
        cx.notify();
    }

    fn write_blocked(&self, cx: &App) -> Option<String> {
        self.backend(cx)?.blocked_reason(Access::Operate)
    }

    // Rendering.

    fn render_card(&self, max_height: Pixels, cx: &mut Context<Self>) -> Option<AnyElement> {
        let setup = self.setup.as_ref().filter(|setup| is_visible(setup));
        let retryable = self.failed_run.is_some()
            && setup.is_none_or(|setup| setup.phase == WorktreeSetupPhase::Failed);
        if setup.is_none() && !retryable {
            return None;
        }
        let palette = palette(cx);
        let now = OffsetDateTime::now_utc();
        let (status, headline, color) = match setup {
            Some(setup) => {
                let (status, color) = phase_status(setup, cx);
                (status, headline(setup), color)
            }
            None => (
                Icon::new(IconName::XCircle)
                    .size(IconSize::Small)
                    .color(Color::Error),
                "Worktree setup failed",
                cx.theme().status().error,
            ),
        };
        let elapsed = setup.and_then(|setup| {
            elapsed(&setup.started_at, setup.ended_at.as_deref(), now).map(format_elapsed)
        });
        let blocked = self.write_blocked(cx);
        let running = setup.is_some_and(|setup| setup.phase == WorktreeSetupPhase::Running);
        let header = h_flex()
            .flex_none()
            .gap_2()
            .child(status)
            .child(
                Label::new(headline)
                    .size(LabelSize::Small)
                    .weight(FontWeight::SEMIBOLD),
            )
            .child(div().flex_1())
            .children(elapsed.map(|elapsed| {
                Label::new(elapsed)
                    .size(LabelSize::XSmall)
                    .color(Color::Muted)
            }));
        // The stages, output and details scroll in a short window, so the card never pushes
        // the composer out of view.
        let mut body = v_flex()
            .id("worktree-setup-body")
            .flex_1()
            .min_h_0()
            .overflow_y_scroll()
            .gap(scaled(px(6.)));
        if let Some(setup) = setup {
            body = body.child(v_flex().gap(scaled(px(3.))).children(
                ordered_stages(setup).map(|stage| self.render_stage(setup, stage, now, cx)),
            ));
            if let Some(tail) = shown_tail(setup) {
                body = body.child(
                    v_flex()
                        .flex_none()
                        .px_2()
                        .py_1()
                        .rounded(scaled(tokens::CONTROL_RADIUS))
                        .bg(palette.surface_2.hsla())
                        .font_family(tokens::CODE_FONT_FAMILY)
                        .text_size(tokens::text_size(tokens::TEXT_SIZE_XSMALL))
                        .text_color(palette.text_muted.hsla())
                        .children(
                            tail.into_iter()
                                .map(|line| div().min_h(scaled(px(14.))).truncate().child(line)),
                        ),
                );
            }
            if self.show_details {
                body = body.child(render_details(setup));
            }
        } else {
            body = body.child(
                Label::new("T3 couldn't prepare this thread's workspace.")
                    .size(LabelSize::Small)
                    .color(Color::Muted),
            );
        }
        // Errors stay in view when the rest scrolls.
        let setup_error = setup
            .filter(|setup| setup.phase == WorktreeSetupPhase::Failed)
            .and_then(|setup| setup.error.clone());
        let errors = [setup_error, self.error.as_ref().map(ToString::to_string)]
            .into_iter()
            .flatten()
            .map(|error| {
                div()
                    .flex_none()
                    .child(Label::new(error).size(LabelSize::Small).color(Color::Error))
            });
        let buttons = h_flex()
            .flex_none()
            .gap_1p5()
            .when(setup.is_some(), |this| {
                this.child(
                    TextButton::new(
                        "worktree-setup-details",
                        if self.show_details {
                            "Hide details"
                        } else {
                            "Details"
                        },
                    )
                    .on_click(cx.listener(|this, _, _, cx| {
                        this.show_details = !this.show_details;
                        cx.notify();
                    })),
                )
            })
            .when(retryable, |this| {
                this.child(
                    TextButton::new("worktree-setup-retry", "Retry")
                        .disabled(self.sending || blocked.is_some())
                        .when_some(blocked.clone(), |button, reason| button.tooltip(reason))
                        .on_click(cx.listener(|this, _, _, cx| this.retry(cx))),
                )
            })
            .when(running, |this| {
                this.child(
                    TextButton::new("worktree-setup-cancel", "Cancel")
                        .disabled(self.sending || blocked.is_some())
                        .when_some(blocked, |button, reason| button.tooltip(reason))
                        .on_click(cx.listener(|this, _, _, cx| this.cancel(cx))),
                )
            });
        Some(
            Card::new()
                .py(scaled(px(8.)))
                .gap(scaled(px(6.)))
                .max_h(max_height)
                .border_color(color.opacity(0.55))
                .child(header)
                .child(body)
                .children(errors)
                .child(buttons)
                .into_any_element(),
        )
    }

    fn render_stage(
        &self,
        setup: &WorktreeSetupSnapshot,
        stage: &WorktreeSetupStage,
        now: OffsetDateTime,
        cx: &App,
    ) -> AnyElement {
        let (icon, color) = stage_icon(&stage.status);
        let duration = stage.started_at.as_deref().and_then(|started| {
            elapsed(started, stage.ended_at.as_deref(), now).map(format_elapsed)
        });
        let trailing = [stage_detail(stage), duration]
            .into_iter()
            .flatten()
            .collect::<Vec<_>>()
            .join(" · ");
        let pending = stage.status == WorktreeSetupStageStatus::Pending;
        h_flex()
            .gap_2()
            .min_h(scaled(px(18.)))
            .child(Icon::new(icon).size(IconSize::XSmall).color(color))
            .child(
                Label::new(stage_label(setup, stage))
                    .size(LabelSize::Small)
                    .color(if pending {
                        Color::Muted
                    } else {
                        Color::Default
                    }),
            )
            .child(div().flex_1())
            .when(!trailing.is_empty(), |this| {
                this.child(
                    div()
                        .min_w_0()
                        .text_size(tokens::text_size(tokens::TEXT_SIZE_XSMALL))
                        .text_color(palette(cx).text_muted.hsla())
                        .truncate()
                        .child(trailing),
                )
            })
            .into_any_element()
    }
}

impl Render for WorktreeSetupCard {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let max_height = window.viewport_size().height * MAX_HEIGHT_SHARE;
        match self.render_card(max_height, cx) {
            Some(card) => h_flex()
                .w_full()
                .justify_center()
                .px_4()
                .pt_2()
                .child(
                    v_flex()
                        .w_full()
                        .max_w(scaled(px(CONTENT_MAX_WIDTH)))
                        .child(card),
                )
                .into_any_element(),
            // With nothing to show, `hidden` keeps the card out of the view's layout.
            None => div().hidden().into_any_element(),
        }
    }
}

/// The snapshots `MUSTR_DEBUG_WORKTREE_SETUP` names, in debug builds.
#[cfg(debug_assertions)]
fn debug_snapshots() -> Option<Vec<Option<WorktreeSetupSnapshot>>> {
    let path = std::env::var_os("MUSTR_DEBUG_WORKTREE_SETUP")?;
    let text = std::fs::read_to_string(&path)
        .inspect_err(|error| log::error!("couldn't read {}: {error}", path.display()))
        .ok()?;
    let value = serde_json::from_str::<serde_json::Value>(&text).ok()?;
    let values = match value {
        serde_json::Value::Array(values) => values,
        value => vec![value],
    };
    values
        .into_iter()
        .map(serde_json::from_value)
        .collect::<Result<_, _>>()
        .inspect_err(|error| log::error!("{} isn't worktree setups: {error}", path.display()))
        .ok()
}

/// `prepared-run.retry`, as T3's app sends it from a failed preparation's error row.
pub(crate) fn retry_command(
    thread_id: &str,
    run_id: &str,
    command_id: String,
) -> OrchestrationV2Command {
    OrchestrationV2Command::PreparedRunRetry(OrchestrationV2CommandPreparedRunRetry {
        command_id,
        run_id: run_id.to_string(),
        thread_id: thread_id.to_string(),
        type_: OrchestrationV2CommandPreparedRunRetryType::PreparedRunRetry,
    })
}

/// The run's id when its workspace preparation failed, so T3 can prepare it again.
fn failed_preparation(run: &OrchestrationV2Run) -> Option<&str> {
    (run.status == OrchestrationV2RunStatus::Failed && run.workspace_preparation.is_some())
        .then_some(run.id.as_str())
}

/// `resolveVisibleWorktreeSetup`: a running, failed, or cancelled setup, or a finished one whose
/// script failed.
pub(crate) fn is_visible(setup: &WorktreeSetupSnapshot) -> bool {
    match setup.phase {
        WorktreeSetupPhase::Running
        | WorktreeSetupPhase::Failed
        | WorktreeSetupPhase::Cancelled => true,
        WorktreeSetupPhase::Done => setup
            .stages
            .iter()
            .any(|stage| stage.status == WorktreeSetupStageStatus::Failed),
        WorktreeSetupPhase::Unrecognized(_) => false,
    }
}

pub(crate) fn headline(setup: &WorktreeSetupSnapshot) -> &'static str {
    let script_failed = setup.stages.iter().any(|stage| {
        stage.id == WorktreeSetupStageId::SetupScript
            && stage.status == WorktreeSetupStageStatus::Failed
    });
    match setup.phase {
        WorktreeSetupPhase::Done if script_failed => "Worktree ready, setup script failed",
        WorktreeSetupPhase::Done => "Worktree ready",
        WorktreeSetupPhase::Failed => "Worktree setup failed",
        WorktreeSetupPhase::Cancelled => "Worktree setup cancelled",
        WorktreeSetupPhase::Running | WorktreeSetupPhase::Unrecognized(_) => "Setting up worktree…",
    }
}

fn phase_status(setup: &WorktreeSetupSnapshot, cx: &App) -> (Icon, Hsla) {
    let palette = palette(cx);
    match setup.phase {
        WorktreeSetupPhase::Failed => (
            Icon::new(IconName::XCircle)
                .size(IconSize::Small)
                .color(Color::Error),
            cx.theme().status().error,
        ),
        WorktreeSetupPhase::Done if headline(setup) != "Worktree ready" => (
            Icon::new(IconName::Warning)
                .size(IconSize::Small)
                .color(Color::Warning),
            palette.warning.hsla(),
        ),
        WorktreeSetupPhase::Done => (
            Icon::new(IconName::Check)
                .size(IconSize::Small)
                .color(Color::Success),
            palette.ok.hsla(),
        ),
        WorktreeSetupPhase::Cancelled => (
            Icon::new(IconName::Close)
                .size(IconSize::Small)
                .color(Color::Muted),
            palette.border.hsla(),
        ),
        WorktreeSetupPhase::Running | WorktreeSetupPhase::Unrecognized(_) => (
            Icon::new(IconName::ArrowCircle)
                .size(IconSize::Small)
                .color(Color::Accent),
            palette.accent.hsla(),
        ),
    }
}

/// The stages in T3's order: fetch, checkout, submodules, setup script, agent.
fn ordered_stages(setup: &WorktreeSetupSnapshot) -> impl Iterator<Item = &WorktreeSetupStage> {
    let rank = |id: &WorktreeSetupStageId| match id {
        WorktreeSetupStageId::Fetch => 0,
        WorktreeSetupStageId::Checkout => 1,
        WorktreeSetupStageId::Submodules => 2,
        WorktreeSetupStageId::SetupScript => 3,
        WorktreeSetupStageId::Agent => 4,
        WorktreeSetupStageId::Unrecognized(_) => 5,
    };
    let mut stages = setup.stages.iter().collect::<Vec<_>>();
    stages.sort_by_key(|stage| rank(&stage.id));
    stages.into_iter()
}

/// T3's `WORKTREE_SETUP_STAGE_LABELS`, with the setup script's own name.
pub(crate) fn stage_label(setup: &WorktreeSetupSnapshot, stage: &WorktreeSetupStage) -> String {
    match &stage.id {
        WorktreeSetupStageId::Fetch => "Fetch base branch".into(),
        WorktreeSetupStageId::Checkout => "Check out files".into(),
        WorktreeSetupStageId::Submodules => "Init submodules".into(),
        WorktreeSetupStageId::SetupScript => setup
            .setup_script
            .as_ref()
            .map(|script| script.name.clone())
            .unwrap_or_else(|| "Run setup script".into()),
        WorktreeSetupStageId::Agent => "Start agent".into(),
        WorktreeSetupStageId::Unrecognized(id) => id.to_string(),
    }
}

/// What follows a stage's label: its percent while checking out, why it was skipped, or T3's
/// detail, such as "exited with 0".
pub(crate) fn stage_detail(stage: &WorktreeSetupStage) -> Option<String> {
    match stage.status {
        WorktreeSetupStageStatus::Pending => None,
        WorktreeSetupStageStatus::Skipped => {
            Some(stage.detail.clone().unwrap_or_else(|| "skipped".into()))
        }
        WorktreeSetupStageStatus::Running
            if stage.id == WorktreeSetupStageId::Checkout && stage.percent.is_some() =>
        {
            stage.percent.map(|percent| format!("{percent}%"))
        }
        _ => stage.detail.clone(),
    }
}

fn stage_icon(status: &WorktreeSetupStageStatus) -> (IconName, Color) {
    match status {
        WorktreeSetupStageStatus::Pending => (IconName::Circle, Color::Muted),
        WorktreeSetupStageStatus::Running => (IconName::ArrowCircle, Color::Accent),
        WorktreeSetupStageStatus::Done => (IconName::Check, Color::Success),
        WorktreeSetupStageStatus::Skipped => (IconName::Dash, Color::Muted),
        WorktreeSetupStageStatus::Warning => (IconName::Warning, Color::Warning),
        WorktreeSetupStageStatus::Failed => (IconName::XCircle, Color::Error),
        WorktreeSetupStageStatus::Unrecognized(_) => (IconName::Circle, Color::Muted),
    }
}

/// The setup script's last lines, padded to [`TAIL_LINES`], while it runs or after it fails.
pub(crate) fn shown_tail(setup: &WorktreeSetupSnapshot) -> Option<Vec<String>> {
    let stage = setup
        .stages
        .iter()
        .find(|stage| stage.id == WorktreeSetupStageId::SetupScript)?;
    let shown = stage.status == WorktreeSetupStageStatus::Failed
        || (stage.status == WorktreeSetupStageStatus::Running && !stage.tail.is_empty());
    if !shown {
        return None;
    }
    let mut lines = stage
        .tail
        .iter()
        .rev()
        .take(TAIL_LINES)
        .rev()
        .cloned()
        .collect::<Vec<_>>();
    lines.resize(TAIL_LINES, String::new());
    Some(lines)
}

fn render_details(setup: &WorktreeSetupSnapshot) -> impl IntoElement {
    let rows = [
        ("Branch", setup.branch.clone()),
        ("Base", setup.base_ref.clone()),
        ("Path", setup.worktree_path.clone()),
        (
            "Setup",
            setup
                .setup_script
                .as_ref()
                .map(|script| script.command.clone()),
        ),
    ];
    v_flex().gap_0p5().children(
        rows.into_iter()
            .filter_map(|(label, value)| Some((label, value?)))
            .map(|(label, value)| {
                h_flex()
                    .gap_2()
                    .child(
                        div().w(scaled(px(56.))).child(
                            Label::new(label)
                                .size(LabelSize::XSmall)
                                .color(Color::Muted),
                        ),
                    )
                    .child(
                        div()
                            .flex_1()
                            .min_w_0()
                            .child(Label::new(value).size(LabelSize::XSmall).truncate()),
                    )
            }),
    )
}

fn elapsed(started: &str, ended: Option<&str>, now: OffsetDateTime) -> Option<i64> {
    let started = OffsetDateTime::parse(started, &Rfc3339).ok()?;
    let ended = match ended {
        Some(ended) => OffsetDateTime::parse(ended, &Rfc3339).ok()?,
        None => now,
    };
    Some((ended - started).whole_seconds().max(0))
}

/// `12s`, `1m 05s`, or `1h 02m`.
pub(crate) fn format_elapsed(seconds: i64) -> String {
    match seconds {
        ..60 => format!("{seconds}s"),
        60..3600 => format!("{}m {:02}s", seconds / 60, seconds % 60),
        _ => format!("{}h {:02}m", seconds / 3600, seconds % 3600 / 60),
    }
}

#[cfg(test)]
mod tests;

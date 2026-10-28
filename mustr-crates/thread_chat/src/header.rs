//! A thread's header: its title and activity, its branch, model, and mode, and how full the
//! agent's context is.

use crate::{
    model::{ContextUsage, ThreadActivity, ThreadState},
    thread_view::{Connection, ThreadView},
};
use gpui::{
    App, Context, FontWeight, Hsla, IntoElement, ParentElement as _, Styled as _, div, px, relative,
};
use mustr_ui::{Pill, palette, tokens};
use ui::{Color, Label, LabelCommon as _, LabelSize, Tooltip, prelude::*};

impl ThreadView {
    pub(crate) fn render_header(&self, cx: &mut Context<Self>) -> impl IntoElement {
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

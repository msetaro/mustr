//! A thread's header: its title and activity, its branch, model, and permission mode, how full
//! the agent's context is (with Compact context), the agent's usage limits, and the thread's
//! menu. Under it go the banners for the agent's status and usage (`usage.rs`).

use crate::{
    model::{ContextUsage, ThreadActivity, ThreadState},
    thread_settings::runtime_mode_label,
    thread_view::{Connection, ThreadView},
};
use gpui::{
    AnyElement, App, Context, FontWeight, Hsla, IntoElement, ParentElement as _, Styled as _,
    WeakEntity, px,
};
use mustr_ui::{Pill, palette, tokens, tokens::scaled};
use ui::{
    Color, ContextMenu, ContextMenuEntry, IconButton, IconName, IconSize, Label, LabelCommon as _,
    LabelSize, PopoverMenu, Tooltip, prelude::*,
};

impl ThreadView {
    pub(crate) fn render_header(&mut self, cx: &mut Context<Self>) -> AnyElement {
        self.observe_history(cx);
        let palette = palette(cx);
        let state = self.thread_state();
        let activity = state.map(ThreadState::activity);
        let details = state.map(header_details).unwrap_or_default();
        let usage = state.and_then(ThreadState::context_usage);
        let loaded = state.is_some();
        let usage_summary = self.render_usage_summary(cx);
        let context_meter = usage.map(|usage| self.render_context_meter(usage, cx));
        let menu = loaded.then(|| self.render_thread_menu(cx));
        let banners = self.render_banners(cx);
        v_flex()
            .flex_none()
            .w_full()
            .child(
                h_flex()
                    .w_full()
                    .gap_3()
                    .px_4()
                    .py(scaled(px(8.)))
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
                                    })
                                    .when(self.is_rewinding(), |this| {
                                        this.child(Pill::tinted("rewinding", palette.accent))
                                    }),
                            )
                            .when(!details.is_empty(), |this| {
                                this.child(
                                    div()
                                        .debug_selector(|| "thread-header-details".into())
                                        .child(
                                            Label::new(details.join(" · "))
                                                .size(LabelSize::Small)
                                                .color(Color::Muted)
                                                .truncate(),
                                        ),
                                )
                            }),
                    )
                    .children(usage_summary)
                    .children(context_meter)
                    .children(menu),
            )
            .children(banners)
            .into_any_element()
    }

    /// How full the agent's context is, with Compact context next to it, as T3's context meter
    /// offers it.
    fn render_context_meter(&self, usage: ContextUsage, cx: &mut Context<Self>) -> AnyElement {
        let blocked = self.compact_blocked(cx);
        h_flex()
            .flex_none()
            .gap_1()
            .child(render_usage(usage, cx))
            .child(
                div().debug_selector(|| "thread-compact".into()).child(
                    IconButton::new("thread-compact", IconName::Compact)
                        .icon_size(IconSize::XSmall)
                        .icon_color(Color::Muted)
                        .disabled(blocked.is_some())
                        .tooltip(Tooltip::text(match blocked {
                            Some(reason) => format!("Compact context: {reason}"),
                            None => "Compact context: summarize the conversation so far, to \
                                     free up the agent's context"
                                .to_string(),
                        }))
                        .on_click(
                            cx.listener(|this, _, window, cx| this.compact_context(window, cx)),
                        ),
                ),
            )
            .into_any_element()
    }

    /// The thread's menu: compacting, restarting the agent, and its usage limits.
    fn render_thread_menu(&self, cx: &mut Context<Self>) -> AnyElement {
        let view: WeakEntity<Self> = cx.weak_entity();
        let compact_blocked = self.compact_blocked(cx).is_some();
        let restart_blocked = self.backend.write_blocked_reason();
        let restarting = self.is_restarting();
        let usage_open = self.usage_panel_open();
        PopoverMenu::new("thread-header-menu")
            .trigger_with_tooltip(
                IconButton::new("thread-header-menu-button", IconName::Ellipsis)
                    .icon_size(IconSize::Small)
                    .icon_color(Color::Muted),
                Tooltip::text("Thread actions"),
            )
            .anchor(gpui::Anchor::TopRight)
            .menu(move |window, cx| {
                let view = view.clone();
                let restart_blocked = restart_blocked.clone();
                Some(ContextMenu::build(window, cx, move |menu, _, _| {
                    let entry = |label: &str, icon: IconName| {
                        ContextMenuEntry::new(label.to_string())
                            .icon(icon)
                            .icon_color(Color::Muted)
                    };
                    menu.item({
                        let view = view.clone();
                        entry(
                            if usage_open {
                                "Hide Usage Limits"
                            } else {
                                "Show Usage Limits"
                            },
                            IconName::SignalMedium,
                        )
                        .handler(move |_, cx| {
                            view.update(cx, |view, cx| view.toggle_usage_limits(cx))
                                .ok();
                        })
                    })
                    .item({
                        let view = view.clone();
                        entry("Compact Context", IconName::Compact)
                            .disabled(compact_blocked)
                            .handler(move |window, cx| {
                                view.update(cx, |view, cx| view.compact_context(window, cx))
                                    .ok();
                            })
                    })
                    .separator()
                    .item({
                        let view = view.clone();
                        entry(
                            if restarting {
                                "Restarting Agent Session…"
                            } else {
                                "Restart Agent Session"
                            },
                            IconName::RotateCcw,
                        )
                        .disabled(restart_blocked.is_some() || restarting)
                        .handler(move |window, cx| {
                            view.update(cx, |view, cx| view.restart_agent_session(window, cx))
                                .ok();
                        })
                    })
                }))
            })
            .into_any_element()
    }
}

/// The header's second line: the thread's branch, its model, and its permission mode by T3's
/// name for it.
pub(crate) fn header_details(state: &ThreadState) -> Vec<String> {
    let mut details = Vec::new();
    if let Some(branch) = &state.thread.branch {
        details.push(branch.clone());
    }
    if let Some(model) = state.model() {
        details.push(model);
    }
    details.push(runtime_mode_label(&state.thread.runtime_mode).to_string());
    details
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
            this.child(crate::usage::meter_bar(
                fraction,
                if fraction > 0.85 {
                    palette.warning.hsla()
                } else {
                    palette.accent.hsla()
                },
                scaled(px(96.)),
                cx,
            ))
        })
}

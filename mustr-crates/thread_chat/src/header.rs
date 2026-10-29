//! A thread's header: one calm row with the thread's title and its status, the subagents button
//! and the thread's menu, over a muted line with its model and permission mode (and, in a
//! subagent or fork, a link to its parent). The agent's status banner goes under it, where T3
//! pins it above the timeline (`usage.rs`); what waits and what runs in the background is above
//! the composer. The context window's ring is in the composer's footer, where T3 shows it
//! (`ContextWindowMeter` in `chat/ComposerFooterControls.tsx`), and usage limits are on the
//! Usage page: the thread shows a limit only when one stops it.

use crate::{
    ShellStatus,
    family_bar::Relationship,
    model::{ContextUsage, ThreadActivity, ThreadState},
    thread_settings::runtime_mode_label,
    thread_view::{Connection, ThreadView},
};
use gpui::{
    AnyElement, App, Context, FontWeight, IntoElement, ParentElement as _, Styled as _, WeakEntity,
    px,
};
use mustr_ui::{MustrIcon, palette, tokens, tokens::scaled};
use ui::{
    ButtonLike, CircularProgress, Color, CommonAnimationExt as _, ContextMenu, ContextMenuEntry,
    IconButton, IconName, IconSize, Label, LabelCommon as _, LabelSize, PopoverMenu, Tooltip,
    prelude::*,
};

impl ThreadView {
    pub(crate) fn render_header(
        &mut self,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        self.observe_history(cx);
        let palette = palette(cx);
        let state = self.thread_state();
        let details = state.map(header_details).unwrap_or_default();
        let status = self.thread_status(cx);
        let menu = state.is_some().then(|| self.render_thread_menu(cx));
        let subagents = self.render_subagents_button(cx);
        let parent = self.render_parent_link(cx);
        let banners = self.render_banners(cx);
        let title = v_flex()
            .flex_1()
            .min_w_0()
            .gap_0p5()
            .child(
                h_flex()
                    .min_w_0()
                    .gap_2()
                    .child(
                        div().min_w_0().child(
                            Label::new(self.title())
                                .weight(FontWeight::SEMIBOLD)
                                .truncate(),
                        ),
                    )
                    .children(status.map(|status| render_status(status, cx)))
                    .when(self.connection == Connection::Reconnecting, |this| {
                        this.child(quiet_note("reconnecting", Color::Warning))
                    })
                    .when(self.is_rewinding(), |this| {
                        this.child(quiet_note("rewinding", Color::Accent))
                    }),
            )
            .when(!details.is_empty() || parent.is_some(), |this| {
                this.child(
                    h_flex()
                        .min_w_0()
                        .gap_1p5()
                        .when(!details.is_empty(), |this| {
                            this.child(
                                div()
                                    .min_w_0()
                                    .debug_selector(|| "thread-header-details".into())
                                    .child(
                                        Label::new(details.join(" · "))
                                            .size(LabelSize::Small)
                                            .color(Color::Muted)
                                            .truncate(),
                                    ),
                            )
                        })
                        .children(parent),
                )
            });
        v_flex()
            .flex_none()
            .w_full()
            .child(
                h_flex()
                    .w_full()
                    .px_4()
                    .py(scaled(px(8.)))
                    .gap_3()
                    .border_b(tokens::BORDER_WIDTH)
                    .border_color(palette.border.hsla())
                    .bg(palette.surface.hsla())
                    .child(title)
                    .child(
                        h_flex()
                            .flex_none()
                            .gap_1()
                            .children(subagents)
                            .children(menu),
                    ),
            )
            .children(banners)
            .into_any_element()
    }

    /// The thread's status as the threads sidebar shows it (`ShellStatus`), from the thread's
    /// record in T3's shell, so the two never disagree. Before the shell has the thread, the
    /// thread's own runs say what they can.
    pub(crate) fn thread_status(&self, cx: &App) -> Option<ShellStatus> {
        let thread_id = self.thread_id.as_deref()?;
        let shells = self.history.shells.as_ref()?.read(cx);
        match shells.thread(thread_id) {
            Some(thread) => ShellStatus::from_shell(thread),
            None => match self.thread_state()?.activity() {
                ThreadActivity::NeedsInput => Some(ShellStatus::AwaitingInput),
                ThreadActivity::Working(_) => Some(ShellStatus::Working),
                ThreadActivity::Failed => Some(ShellStatus::Failed),
                ThreadActivity::Idle(_) => None,
            },
        }
    }

    /// The subagents button: how many subagents, delegated tasks and workflows the thread has,
    /// with a dot while any works. It opens the subagents drawer (`family_bar.rs`).
    fn render_subagents_button(&self, cx: &mut Context<Self>) -> Option<AnyElement> {
        let bar = self.family_bar.read(cx);
        let (count, working) = bar.summary();
        if count == 0 {
            return None;
        }
        let open = bar.is_open();
        let palette = palette(cx);
        Some(
            h_flex()
                .id("thread-subagents-button")
                .debug_selector(|| "thread-subagents-button".into())
                .flex_none()
                .h(scaled(px(24.)))
                .px_1p5()
                .gap_1()
                .rounded(scaled(tokens::CONTROL_RADIUS))
                .cursor_pointer()
                .when(open, |this| this.bg(palette.surface_2.hsla()))
                .hover(|style| style.bg(palette.surface_2.hsla()))
                .tooltip(Tooltip::text(if open {
                    "Hide subagents"
                } else {
                    "Subagents"
                }))
                .on_click(cx.listener(|this, _, window, cx| {
                    this.family_bar.update(cx, |bar, cx| bar.toggle(window, cx));
                    cx.notify();
                }))
                .child(
                    MustrIcon::Bot
                        .icon()
                        .size(IconSize::Small)
                        .color(Color::Muted),
                )
                .child(
                    Label::new(count.to_string())
                        .size(LabelSize::Small)
                        .color(Color::Muted),
                )
                .when(working > 0, |this| {
                    this.child(
                        div()
                            .debug_selector(|| "thread-subagents-working".into())
                            .size(scaled(px(6.)))
                            .rounded_full()
                            .bg(palette.accent.hsla()),
                    )
                })
                .into_any_element(),
        )
    }

    /// In a subagent or a fork, a link to the thread it came from.
    fn render_parent_link(&self, cx: &mut Context<Self>) -> Option<AnyElement> {
        let parent = self.family_bar.read(cx).parent()?.clone();
        let palette = palette(cx);
        let label = match parent.relationship {
            Relationship::Subagent => "Subagent of",
            Relationship::Fork => "Forked from",
        };
        let bar = self.family_bar.downgrade();
        Some(
            h_flex()
                .id("family-open-parent")
                .debug_selector(|| "family-open-parent".into())
                .min_w_0()
                .gap_1()
                .text_size(tokens::text_size(tokens::TEXT_SIZE_SMALL))
                .text_color(palette.text_muted.hsla())
                .child(div().flex_none().child(format!("· {label}")))
                .child(
                    div()
                        .min_w_0()
                        .overflow_hidden()
                        .whitespace_nowrap()
                        .text_ellipsis()
                        .when(parent.available, |this| {
                            this.text_color(palette.text.hsla())
                                .hover(|style| style.underline())
                        })
                        .child(parent.title.clone()),
                )
                .when(parent.available, |this| {
                    this.cursor_pointer()
                        .tooltip(Tooltip::text("Open the thread that started this one"))
                        .on_click(move |_, window, cx| {
                            bar.update(cx, |bar, cx| bar.open_parent(window, cx)).ok();
                        })
                })
                .when(!parent.available, |this| {
                    this.tooltip(Tooltip::text("T3 no longer lists the parent thread"))
                })
                .into_any_element(),
        )
    }

    /// The context window, in the composer's footer as T3's `ContextWindowMeter`: a ring of how
    /// full it is, with the numbers on hover, and Compact context in its menu.
    pub(crate) fn render_context_meter(&self, cx: &mut Context<Self>) -> Option<AnyElement> {
        let usage = self.thread_state()?.context_usage()?;
        let palette = palette(cx);
        let fraction = usage
            .max_tokens
            .filter(|max| *max > 0)
            .map(|max| (usage.used_tokens as f32 / max as f32).clamp(0., 1.));
        let numbers = context_numbers(usage);
        let blocked = self.compact_blocked(cx);
        let view: WeakEntity<Self> = cx.weak_entity();
        let ring = CircularProgress::new(
            fraction.unwrap_or(0.),
            1.,
            tokens::scaled_pixels(px(14.), cx),
            cx,
        )
        .stroke_width(tokens::scaled_pixels(px(2.5), cx))
        .bg_color(palette.border.hsla())
        .progress_color(if fraction.is_some_and(|fraction| fraction > 0.9) {
            cx.theme().status().error
        } else {
            palette.text_muted.hsla()
        });
        let tooltip = format!("Context window: {numbers}");
        Some(
            div()
                .flex_none()
                .debug_selector(|| "thread-context-meter".into())
                .child(
                    PopoverMenu::new("thread-context-meter")
                        .trigger_with_tooltip(
                            ButtonLike::new("thread-context-meter-button").child(
                                div()
                                    .size(scaled(px(20.)))
                                    .flex()
                                    .items_center()
                                    .justify_center()
                                    .child(ring),
                            ),
                            Tooltip::text(tooltip),
                        )
                        .anchor(gpui::Anchor::BottomRight)
                        .menu(move |window, cx| {
                            let view = view.clone();
                            let numbers = numbers.clone();
                            let blocked = blocked.clone();
                            Some(ContextMenu::build(window, cx, move |menu, _, _| {
                                menu.header(format!("Context window · {numbers}")).item(
                                    ContextMenuEntry::new("Compact Context")
                                        .icon(IconName::Compact)
                                        .icon_color(Color::Muted)
                                        .disabled(blocked.is_some())
                                        .handler(move |window, cx| {
                                            view.update(cx, |view, cx| {
                                                view.compact_context(window, cx)
                                            })
                                            .ok();
                                        }),
                                )
                            }))
                        }),
                )
                .into_any_element(),
        )
    }

    /// The thread's menu: compacting and restarting the agent.
    fn render_thread_menu(&self, cx: &mut Context<Self>) -> AnyElement {
        let view: WeakEntity<Self> = cx.weak_entity();
        let compact_blocked = self.compact_blocked(cx).is_some();
        let restart_blocked = self.backend.write_blocked_reason();
        let restarting = self.is_restarting();
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

/// The header's second line: the thread's model and its permission mode by T3's name for it.
/// The branch is in the thread's toolbar.
pub(crate) fn header_details(state: &ThreadState) -> Vec<String> {
    let mut details = Vec::new();
    if let Some(model) = state.model() {
        details.push(model);
    }
    details.push(runtime_mode_label(&state.thread.runtime_mode).to_string());
    details
}

/// The thread's status, quietly: T3's icon and the word in its color, with no fill.
fn render_status(status: ShellStatus, cx: &App) -> AnyElement {
    let color = status.color(cx);
    let icon = status
        .icon()
        .size(IconSize::XSmall)
        .color(Color::Custom(color));
    h_flex()
        .debug_selector(move || format!("thread-header-status-{}", status.label()))
        .flex_none()
        .gap_1()
        .child(if status.spins() {
            icon.with_rotate_animation(3).into_any_element()
        } else {
            icon.into_any_element()
        })
        .child(
            Label::new(status.label())
                .size(LabelSize::Small)
                .color(Color::Custom(color)),
        )
        .into_any_element()
}

fn quiet_note(text: &'static str, color: Color) -> AnyElement {
    Label::new(text)
        .size(LabelSize::Small)
        .color(color)
        .into_any_element()
}

/// "529k / 1,000k tokens · 53% used", or the tokens alone when the window's size is unknown.
pub(crate) fn context_numbers(usage: ContextUsage) -> String {
    let thousands = |tokens: i64| {
        if tokens >= 1000 {
            format!("{}k", (tokens + 500) / 1000)
        } else {
            tokens.to_string()
        }
    };
    match usage.max_tokens.filter(|max| *max > 0) {
        Some(max) => format!(
            "{} / {} tokens · {}% used",
            thousands(usage.used_tokens),
            thousands(max),
            (usage.used_tokens as f64 * 100. / max as f64).round() as i64
        ),
        None => format!("{} tokens", thousands(usage.used_tokens)),
    }
}

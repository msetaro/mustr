//! Reviewing an agent thread's work: the header (title, agent, branch, status), the scope
//! picker (the thread's whole changes, or one turn), and the actions that lead back to the
//! thread.
//!
//! Comments on an agent's diff are local: they never go to a forge. "Send to agent" batches
//! them into one message to the thread (`mustr-docs/PLAN.md` §7), through the same submit form
//! a pull request review uses; the app's [`crate::ReviewBackend`] sends it, each comment as one
//! of T3's `review-comment` records.

use gpui::{
    Action, AnyElement, App, ClickEvent, IntoElement, ParentElement as _, SharedString,
    Styled as _, WeakEntity, Window,
};
use mustr_ui::{MustrIcon, tokens};
use serde::Deserialize;
use ui::{
    Button, ButtonSize, ButtonStyle, Color, ContextMenu, Icon, IconButton, IconName, IconPosition,
    IconSize, Label, LabelCommon as _, LabelSize, PopoverMenu, Tooltip, prelude::*,
};

use crate::{
    ReviewView,
    header::{TITLE_MIN_WIDTH, detail, pill, separator},
};

/// Opens the agent thread's chat. The review only dispatches it; the app decides what opens.
#[derive(Clone, Debug, PartialEq, Eq, Deserialize, schemars::JsonSchema, Action)]
#[action(namespace = review)]
#[serde(deny_unknown_fields)]
pub struct OpenThreadChat {
    pub thread_id: String,
    /// The thread's title, shown on the chat's tab until the thread loads.
    #[serde(default)]
    pub title: String,
}

/// Which changes of an agent thread a review shows.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum ReviewScope {
    /// The thread's branch against its base, including changes not committed yet.
    Changes,
    /// Only what isn't committed yet: the working tree against `HEAD`, untracked files included
    /// (T3's diff panel's "Uncommitted", its `working-tree` source).
    Uncommitted,
    /// What one turn changed: T3's checkpoint before the turn against the one after it.
    Turn(u32),
}

impl ReviewScope {
    pub fn label(&self) -> String {
        match self {
            ReviewScope::Changes => "Changes".to_string(),
            ReviewScope::Uncommitted => "Uncommitted".to_string(),
            ReviewScope::Turn(turn) => format!("Turn {turn}"),
        }
    }

    /// A stable name for files and logs: `changes`, `uncommitted`, `turn-3`.
    pub fn key(&self) -> String {
        match self {
            ReviewScope::Changes => "changes".to_string(),
            ReviewScope::Uncommitted => "uncommitted".to_string(),
            ReviewScope::Turn(turn) => format!("turn-{turn}"),
        }
    }
}

/// A scope the picker offers, with a note such as "4 files".
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ScopeChoice {
    pub scope: ReviewScope,
    pub detail: Option<String>,
}

/// How the thread's status is colored.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum AgentStatusTone {
    /// Done, or idle.
    #[default]
    Settled,
    Working,
    /// Waiting on an approval or an answer.
    NeedsInput,
    Failed,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct AgentThreadHeader {
    pub thread_id: String,
    pub title: String,
    pub project: Option<String>,
    pub branch: Option<String>,
    /// The agent and its model, for example "Claude · claude-opus-5-5".
    pub agent: Option<String>,
    pub status: String,
    pub status_tone: AgentStatusTone,
    /// What the picker offers. Empty hides the picker.
    pub scopes: Vec<ScopeChoice>,
    pub scope: ReviewScope,
    /// Where the diff comes from: the worktree on this machine, or T3's patch and why.
    pub source: Option<String>,
}

impl AgentThreadHeader {
    pub fn tab_title(&self) -> SharedString {
        let title = crate::header::truncated_tab_title(&self.title);
        match self.scope {
            ReviewScope::Changes => title,
            scope => format!("{title} · {}", scope.label()).into(),
        }
    }

    fn status_color(&self, cx: &App) -> gpui::Hsla {
        let status = cx.theme().status();
        match self.status_tone {
            AgentStatusTone::Settled => status.hint,
            AgentStatusTone::Working => cx.theme().colors().text_accent,
            AgentStatusTone::NeedsInput => status.warning,
            AgentStatusTone::Failed => status.error,
        }
    }
}

/// What a review view tells the app beyond editor events.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ReviewViewEvent {
    /// The reviewer picked another scope of the agent thread; the app loads it.
    SelectScope(ReviewScope),
}

/// The scope picker: a button with the current scope that opens a menu of the others.
fn render_scope_picker(
    header: &AgentThreadHeader,
    review: WeakEntity<ReviewView>,
) -> Option<AnyElement> {
    if header.scopes.is_empty() {
        return None;
    }
    let current = header.scope;
    let choices = header.scopes.clone();
    Some(
        PopoverMenu::new("review-scope-picker")
            .trigger(
                Button::new("review-scope", current.label())
                    .style(ButtonStyle::Outlined)
                    .size(ButtonSize::Compact)
                    .label_size(LabelSize::Small)
                    .end_icon(
                        Icon::new(IconName::ChevronDown)
                            .size(IconSize::XSmall)
                            .color(Color::Muted),
                    ),
            )
            .menu(move |window, cx| {
                let choices = choices.clone();
                let review = review.clone();
                Some(ContextMenu::build(window, cx, move |mut menu, _, _| {
                    for choice in choices {
                        let label = match &choice.detail {
                            Some(detail) => format!("{} · {detail}", choice.scope.label()),
                            None => choice.scope.label(),
                        };
                        let review = review.clone();
                        let scope = choice.scope;
                        menu = menu.toggleable_entry(
                            label,
                            scope == current,
                            IconPosition::Start,
                            None,
                            move |_, cx| {
                                review
                                    .update(cx, |review, cx| review.select_scope(scope, cx))
                                    .ok();
                            },
                        );
                    }
                    menu
                }))
            })
            .into_any_element(),
    )
}

/// The header card of an agent thread's review. `actions` go at the end of the title row, and
/// `extra` below the details.
/// `compact` shows Restricted Mode and Open chat as icons, with their labels in tooltips.
pub(crate) fn render_agent_header(
    header: &AgentThreadHeader,
    review: WeakEntity<ReviewView>,
    restricted_worktree: bool,
    compact: bool,
    actions: Option<AnyElement>,
    extra: Option<AnyElement>,
    cx: &App,
) -> impl IntoElement {
    let colors = cx.theme().colors();
    let muted = |text: String| detail(Label::new(text).size(LabelSize::Small).color(Color::Muted));

    let mut details = h_flex().gap_1p5().flex_wrap().child(
        h_flex()
            .flex_none()
            .gap_1()
            .child(
                MustrIcon::Agent
                    .icon()
                    .size(IconSize::XSmall)
                    .color(Color::Custom(mustr_ui::palette(cx).agent.hsla())),
            )
            .child(
                Label::new(header.agent.clone().unwrap_or_else(|| "Agent".into()))
                    .size(LabelSize::Small)
                    .color(Color::Muted),
            ),
    );
    if let Some(project) = &header.project {
        details = details.child(separator()).child(muted(project.clone()));
    }
    if let Some(branch) = &header.branch {
        details = details.child(separator()).child(
            h_flex()
                .flex_none()
                .gap_1()
                .child(
                    Icon::new(IconName::GitBranch)
                        .size(IconSize::XSmall)
                        .color(Color::Muted),
                )
                .child(
                    Label::new(branch.clone())
                        .size(LabelSize::Small)
                        .color(Color::Muted)
                        .buffer_font(cx),
                ),
        );
    }
    if let Some(source) = &header.source {
        details = details.child(separator()).child(muted(source.clone()));
    }

    let thread_id = header.thread_id.clone();
    let thread_title = header.title.clone();
    v_flex()
        .id("review-header")
        .flex_none()
        .w_full()
        .px_3()
        .py_2()
        .gap_1()
        .bg(colors.surface_background)
        .border_b(tokens::BORDER_WIDTH)
        .border_color(colors.border)
        .child(
            h_flex()
                .flex_wrap()
                .gap_2()
                .child(pill(header.status.clone(), header.status_color(cx), cx))
                .child(
                    div()
                        .flex_1()
                        .min_w(tokens::scaled_pixels(TITLE_MIN_WIDTH, cx))
                        .child(
                            Label::new(header.title.clone())
                                .size(LabelSize::Large)
                                .weight(gpui::FontWeight::SEMIBOLD)
                                .truncate(),
                        ),
                )
                .children(render_scope_picker(header, review))
                .when(restricted_worktree, |this| {
                    let tooltip = "Restricted Mode: language servers don't run in this worktree \
                                   until you trust it. Click to trust it or stay in Restricted Mode.";
                    let trust = |_: &ClickEvent, window: &mut Window, cx: &mut App| {
                        window.dispatch_action(Box::new(workspace::ToggleWorktreeSecurity), cx)
                    };
                    if compact {
                        this.child(
                            IconButton::new("trust-worktree", IconName::Warning)
                                .icon_size(IconSize::Small)
                                .icon_color(Color::Warning)
                                .tooltip(Tooltip::text(tooltip))
                                .on_click(trust),
                        )
                    } else {
                        this.child(
                            Button::new("trust-worktree", "Restricted Mode")
                                .style(ButtonStyle::Subtle)
                                .size(ButtonSize::Compact)
                                .label_size(LabelSize::Small)
                                .start_icon(
                                    Icon::new(IconName::Warning)
                                        .size(IconSize::XSmall)
                                        .color(Color::Warning),
                                )
                                .tooltip(Tooltip::text(tooltip))
                                .on_click(trust),
                        )
                    }
                })
                .map(|this| {
                    let open_chat = move |_: &ClickEvent, window: &mut Window, cx: &mut App| {
                        window.dispatch_action(
                            Box::new(OpenThreadChat {
                                thread_id: thread_id.clone(),
                                title: thread_title.to_string(),
                            }),
                            cx,
                        )
                    };
                    if compact {
                        this.child(
                            IconButton::new("open-thread-chat", IconName::Chat)
                                .icon_size(IconSize::Small)
                                .icon_color(Color::Muted)
                                .tooltip(Tooltip::text("Open chat: the agent thread's chat"))
                                .on_click(open_chat),
                        )
                    } else {
                        this.child(
                            Button::new("open-thread-chat", "Open chat")
                                .style(ButtonStyle::Subtle)
                                .size(ButtonSize::Compact)
                                .label_size(LabelSize::Small)
                                .start_icon(
                                    Icon::new(IconName::Chat)
                                        .size(IconSize::XSmall)
                                        .color(Color::Muted),
                                )
                                .tooltip(Tooltip::text("Open the agent thread's chat"))
                                .on_click(open_chat),
                        )
                    }
                })
                .children(actions),
        )
        .child(details)
        .children(extra)
}

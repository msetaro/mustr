//! The pull request header at the top of a review: title, reference, author, branches, state,
//! and checks.

use gpui::{AnyElement, App, Hsla, IntoElement, ParentElement as _, SharedString, Styled as _};
use mustr_ui::tokens;

use crate::{AgentThreadHeader, threads::ui_scaled};
use ui::{
    Color, DiffStat, Icon, IconName, IconSize, Label, LabelCommon as _, LabelSize, prelude::*,
};

/// Tab titles longer than this are cut short with an ellipsis.
const TAB_TITLE_MAX_CHARS: usize = 40;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PullRequestState {
    Open,
    Closed,
    Merged,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ChecksState {
    Passing,
    Failing,
    Pending,
}

/// The pull request's checks, rolled up.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ChecksSummary {
    pub state: ChecksState,
    pub total: usize,
    pub failing: usize,
    pub pending: usize,
}

impl ChecksSummary {
    pub fn label(&self) -> String {
        match self.state {
            ChecksState::Passing if self.total == 1 => "1 check passing".to_string(),
            ChecksState::Passing => format!("{} checks passing", self.total),
            ChecksState::Failing => format!("{} of {} checks failing", self.failing, self.total),
            ChecksState::Pending => format!("{} of {} checks pending", self.pending, self.total),
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PullRequestHeader {
    pub title: String,
    pub repository: String,
    pub number: u64,
    pub author: Option<String>,
    pub base_branch: String,
    pub head_branch: String,
    pub state: PullRequestState,
    pub is_draft: bool,
    /// `None` when the forge reports no checks.
    pub checks: Option<ChecksSummary>,
    pub additions: u64,
    pub deletions: u64,
}

impl PullRequestHeader {
    /// `owner/repo#123`
    pub fn reference(&self) -> String {
        format!("{}#{}", self.repository, self.number)
    }

    pub fn state_label(&self) -> &'static str {
        match self.state {
            PullRequestState::Open if self.is_draft => "Draft",
            PullRequestState::Open => "Open",
            PullRequestState::Closed => "Closed",
            PullRequestState::Merged => "Merged",
        }
    }

    pub fn tab_title(&self) -> SharedString {
        truncated_tab_title(&self.title)
    }

    fn state_color(&self, cx: &App) -> Hsla {
        let status = cx.theme().status();
        match self.state {
            PullRequestState::Open if self.is_draft => status.hint,
            PullRequestState::Open => status.success,
            PullRequestState::Closed => status.error,
            PullRequestState::Merged => mustr_ui::palette(cx).pending.hsla(),
        }
    }
}

/// `title`, cut short with an ellipsis when it's too long for a tab.
pub(crate) fn truncated_tab_title(title: &str) -> SharedString {
    let title = title.trim();
    let truncated = util::truncate(title, TAB_TITLE_MAX_CHARS - 1);
    if truncated.len() == title.len() {
        title.to_string().into()
    } else {
        format!("{}…", truncated.trim_end()).into()
    }
}

/// What a review is of, for its header and tab.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ReviewHeader {
    PullRequest(PullRequestHeader),
    AgentThread(AgentThreadHeader),
}

impl ReviewHeader {
    pub fn title(&self) -> &str {
        match self {
            ReviewHeader::PullRequest(header) => &header.title,
            ReviewHeader::AgentThread(header) => &header.title,
        }
    }

    pub fn tab_title(&self) -> SharedString {
        match self {
            ReviewHeader::PullRequest(header) => header.tab_title(),
            ReviewHeader::AgentThread(header) => header.tab_title(),
        }
    }

    /// What the window's tab shows: `owner/repo#123` for a pull request, as in the mockups,
    /// and the title for an agent thread.
    pub fn tab_label(&self) -> SharedString {
        match self {
            ReviewHeader::PullRequest(header) => header.reference().into(),
            ReviewHeader::AgentThread(header) => header.tab_title(),
        }
    }

    /// A second line for tooltips: the pull request's reference, or the thread's scope, project
    /// and branch.
    pub fn subtitle(&self) -> String {
        match self {
            ReviewHeader::PullRequest(header) => header.reference(),
            ReviewHeader::AgentThread(header) => [
                Some(header.scope.label()),
                header.project.clone(),
                header.branch.clone(),
            ]
            .into_iter()
            .flatten()
            .collect::<Vec<_>>()
            .join(" · "),
        }
    }

    pub fn pull_request(&self) -> Option<&PullRequestHeader> {
        match self {
            ReviewHeader::PullRequest(header) => Some(header),
            ReviewHeader::AgentThread(_) => None,
        }
    }

    pub fn agent_thread(&self) -> Option<&AgentThreadHeader> {
        match self {
            ReviewHeader::PullRequest(_) => None,
            ReviewHeader::AgentThread(header) => Some(header),
        }
    }
}

/// A small rounded label in `color`, on a faint wash of the same color.
pub(crate) fn pill(label: impl Into<SharedString>, color: Hsla, cx: &App) -> impl IntoElement {
    div()
        .flex_none()
        .px_1p5()
        .rounded_full()
        .border(tokens::BORDER_WIDTH)
        .border_color(color.opacity(0.5))
        .bg(color.opacity(0.12))
        .text_color(color)
        .text_size(ui_scaled(tokens::TEXT_SIZE_SMALL, cx))
        .child(label.into())
}

/// The header card. `actions` go at the end of the title row, and `extra` below the details
/// (for example, threads on files the review can't show).
pub(crate) fn render_header(
    header: &PullRequestHeader,
    actions: Option<AnyElement>,
    extra: Option<AnyElement>,
    cx: &App,
) -> impl IntoElement {
    let colors = cx.theme().colors();
    let status = cx.theme().status();

    let mut details = h_flex().gap_1p5().flex_wrap().child(
        Label::new(header.reference())
            .size(LabelSize::Small)
            .color(Color::Muted),
    );
    if let Some(author) = &header.author {
        details = details.child(separator()).child(
            Label::new(format!("@{author}"))
                .size(LabelSize::Small)
                .color(Color::Muted),
        );
    }
    details = details.child(separator()).child(
        h_flex()
            .gap_1()
            .child(
                Icon::new(IconName::GitBranch)
                    .size(IconSize::XSmall)
                    .color(Color::Muted),
            )
            .child(
                Label::new(format!("{} ← {}", header.base_branch, header.head_branch))
                    .size(LabelSize::Small)
                    .color(Color::Muted)
                    .buffer_font(cx),
            ),
    );
    if let Some(checks) = header.checks {
        let (icon, color) = match checks.state {
            ChecksState::Passing => (IconName::Check, status.success),
            ChecksState::Failing => (IconName::XCircle, status.error),
            ChecksState::Pending => (IconName::Circle, status.warning),
        };
        details = details.child(separator()).child(
            h_flex()
                .gap_1()
                .text_color(color)
                .child(
                    Icon::new(icon)
                        .size(IconSize::XSmall)
                        .color(Color::Custom(color)),
                )
                .child(
                    Label::new(checks.label())
                        .size(LabelSize::Small)
                        .color(Color::Custom(color)),
                ),
        );
    }

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
                .gap_2()
                .child(pill(header.state_label(), header.state_color(cx), cx))
                .child(
                    div().flex_1().min_w_0().child(
                        Label::new(header.title.clone())
                            .size(LabelSize::Large)
                            .weight(gpui::FontWeight::SEMIBOLD)
                            .truncate(),
                    ),
                )
                .when(header.additions > 0 || header.deletions > 0, |this| {
                    this.child(DiffStat::new(
                        "review-header-diff-stat",
                        header.additions as usize,
                        header.deletions as usize,
                    ))
                })
                .children(actions),
        )
        .child(details)
        .children(extra)
}

fn separator() -> impl IntoElement {
    Label::new("·").size(LabelSize::Small).color(Color::Muted)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn header(state: PullRequestState, is_draft: bool) -> PullRequestHeader {
        PullRequestHeader {
            title: "Add rate-limited retries to the webhook dispatcher and its tests".to_string(),
            repository: "acme/api".to_string(),
            number: 482,
            author: Some("octocat".to_string()),
            base_branch: "main".to_string(),
            head_branch: "retry-backoff".to_string(),
            state,
            is_draft,
            checks: None,
            additions: 10,
            deletions: 2,
        }
    }

    #[test]
    fn labels() {
        let open = header(PullRequestState::Open, false);
        assert_eq!(open.reference(), "acme/api#482");
        assert_eq!(open.state_label(), "Open");
        assert_eq!(open.tab_title(), "Add rate-limited retries to the webhook…");
        assert_eq!(header(PullRequestState::Open, true).state_label(), "Draft");
        assert_eq!(
            header(PullRequestState::Merged, false).state_label(),
            "Merged"
        );

        let checks = |state, total, failing, pending| {
            ChecksSummary {
                state,
                total,
                failing,
                pending,
            }
            .label()
        };
        assert_eq!(checks(ChecksState::Passing, 1, 0, 0), "1 check passing");
        assert_eq!(checks(ChecksState::Passing, 5, 0, 0), "5 checks passing");
        assert_eq!(
            checks(ChecksState::Failing, 5, 2, 1),
            "2 of 5 checks failing"
        );
        assert_eq!(
            checks(ChecksState::Pending, 5, 0, 3),
            "3 of 5 checks pending"
        );
    }
}

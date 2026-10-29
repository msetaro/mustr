//! A thread's details at a glance, as T3 shows them (`components/ThreadHoverCard.tsx`, the
//! sidebar row's tooltip in `sidebar/SidebarThreadRow.tsx`, and `chat/ThreadDetailsPanel.tsx`):
//! - **The hover card** on a sidebar row: its title, project, branch, model and agent, status,
//!   and pull request.
//! - **Thread Details** ([`ShowThreadDetails`], from the palette and the row's menu): the same,
//!   with where it works, its permission mode, when it started and last changed, the thread it
//!   came from, and its ID, each to copy. T3's panel also holds the branch and git actions,
//!   which mustr's thread toolbar has (`mustr/src/thread_toolbar.rs`).
//!
//! Everything comes from T3's shell ([`ThreadShells`]) and the server config's agents
//! ([`ServerConfigs`](crate::ServerConfigs)), so nothing is sent.

use crate::{
    organize, shell::ThreadShells, sidebar::RowPullRequest, thread_settings, usage::ServerConfigs,
};
use gpui::{
    App, ClipboardItem, Context, DismissEvent, EventEmitter, FocusHandle, Focusable, FontWeight,
    IntoElement, ParentElement as _, Render, SharedString, Styled as _, WeakEntity, Window,
    actions, div, px,
};
use mustr_ui::{palette, tokens, tokens::scaled};
use ui::{
    Color, Icon, IconButton, IconName, IconSize, Label, LabelCommon as _, LabelSize, Tooltip,
    prelude::*,
};
use workspace::{ModalView, Workspace};

actions!(
    thread_chat,
    [
        /// Shows the open agent thread's details: its project, branch, worktree, model, and ID.
        ShowThreadDetails,
    ]
);

/// What the hover card and the details show of a thread.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct ThreadDetails {
    pub id: String,
    pub title: String,
    pub project: Option<String>,
    /// The project's checkout, or the thread's worktree.
    pub path: Option<String>,
    pub in_worktree: bool,
    pub branch: Option<String>,
    /// The model and its agent, such as "Claude Opus 4.7 · Claude Code".
    pub model: String,
    pub permission_mode: String,
    pub status: Option<String>,
    pub created: Option<String>,
    pub updated: Option<String>,
    pub pull_request: Option<String>,
    /// The thread it was forked from or is a subagent of.
    pub parent: Option<String>,
}

impl ThreadDetails {
    /// What a sidebar row knows, for a thread the shell no longer has.
    pub(crate) fn of_row(thread: &crate::ThreadEntry) -> Self {
        Self {
            id: thread.id.clone(),
            title: thread.title.clone(),
            project: thread.project.clone(),
            path: thread
                .worktree_path
                .clone()
                .or_else(|| thread.workspace_root.clone()),
            in_worktree: thread.worktree_path.is_some(),
            branch: thread.branch.clone(),
            model: String::new(),
            permission_mode: String::new(),
            status: thread.status.map(|status| status.label().to_string()),
            created: None,
            updated: None,
            pull_request: None,
            parent: None,
        }
    }
}

/// The details of `thread_id`, from the shell, as of `now`.
pub(crate) fn thread_details(
    thread_id: &str,
    now: time::OffsetDateTime,
    cx: &mut App,
) -> Option<ThreadDetails> {
    let shells = ThreadShells::global(cx);
    let shell = shells.read(cx).shell()?;
    let thread = shell.threads.get(thread_id)?;
    let project = shell.projects.get(&thread.project_id);
    let instance_id = thread_settings::selection_instance_id(&thread.model_selection)
        .unwrap_or(thread.provider_instance_id.as_str())
        .to_string();
    let slug = thread_settings::selection_model(&thread.model_selection)
        .unwrap_or_default()
        .to_string();
    let provider = ServerConfigs::try_global(cx)
        .and_then(|configs| configs.read(cx).provider(&instance_id).cloned());
    let model_name = provider
        .as_ref()
        .and_then(|provider| provider.models.iter().find(|model| model.slug == slug))
        .map_or_else(|| slug.clone(), |model| model.name.clone());
    let agent_name = provider
        .as_ref()
        .and_then(|provider| provider.display_name.clone())
        .filter(|name| !name.trim().is_empty())
        .unwrap_or(instance_id);
    let worktree = thread
        .worktree_path
        .as_deref()
        .map(str::trim)
        .filter(|path| !path.is_empty());
    let parent = thread
        .lineage
        .parent_thread_id
        .as_deref()
        .and_then(|parent| shell.threads.get(parent))
        .map(|parent| crate::sidebar::display_title(&parent.title));
    Some(ThreadDetails {
        id: thread.id.clone(),
        title: crate::sidebar::display_title(&thread.title),
        project: project.map(|project| project.title.clone()),
        path: worktree
            .map(str::to_string)
            .or_else(|| project.map(|project| project.workspace_root.clone())),
        in_worktree: worktree.is_some(),
        branch: thread.branch.clone().filter(|branch| !branch.is_empty()),
        // Without the agent in T3's config, its model's id alone.
        model: match provider {
            Some(_) => format!("{model_name} · {agent_name}"),
            None => model_name,
        },
        permission_mode: thread_settings::runtime_mode_label(&thread.runtime_mode).to_string(),
        status: crate::sidebar::ShellStatus::from_shell(thread).map(|status| status.label().into()),
        created: organize::time_ago(&thread.created_at, now),
        updated: organize::time_ago(&thread.updated_at, now),
        pull_request: RowPullRequest::of(thread).map(|pull_request| match pull_request.title {
            Some(title) => format!("#{} {title}", pull_request.number),
            None => format!("#{}", pull_request.number),
        }),
        parent,
    })
}

/// One line of the card: an icon and a muted text.
fn line(icon: IconName, text: impl Into<SharedString>) -> impl IntoElement {
    h_flex()
        .min_w_0()
        .gap_2()
        .child(Icon::new(icon).size(IconSize::XSmall).color(Color::Muted))
        .child(
            div().min_w_0().child(
                Label::new(text.into())
                    .size(LabelSize::XSmall)
                    .color(Color::Muted)
                    .truncate(),
            ),
        )
}

/// The hover card on a sidebar row (T3's `SidebarThreadTooltip`).
pub(crate) struct ThreadHoverCard {
    pub details: ThreadDetails,
}

impl Render for ThreadHoverCard {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let details = &self.details;
        let palette = palette(cx);
        v_flex()
            .id("thread-hover-card")
            .max_w(scaled(px(320.)))
            .px_2()
            .py(scaled(px(6.)))
            .gap_1()
            .rounded(scaled(tokens::CONTROL_RADIUS))
            .border(tokens::BORDER_WIDTH)
            .border_color(palette.border.hsla())
            .bg(palette.surface.hsla())
            .shadow_md()
            .child(
                Label::new(details.title.clone())
                    .size(LabelSize::Small)
                    .weight(FontWeight::MEDIUM)
                    .truncate(),
            )
            .child(
                v_flex()
                    .gap_0p5()
                    .when_some(details.project.clone(), |this, project| {
                        this.child(line(IconName::Folder, project))
                    })
                    .when_some(details.branch.clone(), |this, branch| {
                        this.child(line(IconName::GitBranch, branch))
                    })
                    .when(!details.model.is_empty(), |this| {
                        this.child(line(IconName::ZedAgent, details.model.clone()))
                    })
                    .when_some(details.status.clone(), |this, status| {
                        let updated = details
                            .updated
                            .as_ref()
                            .map(|updated| format!("{status} · {updated}"))
                            .unwrap_or(status);
                        this.child(line(IconName::Circle, updated))
                    })
                    .when_some(details.pull_request.clone(), |this, pull_request| {
                        this.child(line(IconName::PullRequest, pull_request))
                    }),
            )
    }
}

/// Thread Details, as a modal.
pub(crate) struct ThreadDetailsModal {
    details: ThreadDetails,
    focus_handle: FocusHandle,
}

impl ThreadDetailsModal {
    #[cfg(test)]
    pub(crate) fn details(&self) -> &ThreadDetails {
        &self.details
    }

    fn cancel(&mut self, _: &menu::Cancel, _: &mut Window, cx: &mut Context<Self>) {
        cx.emit(DismissEvent);
    }
}

impl EventEmitter<DismissEvent> for ThreadDetailsModal {}

impl ModalView for ThreadDetailsModal {}

impl Focusable for ThreadDetailsModal {
    fn focus_handle(&self, _: &App) -> FocusHandle {
        self.focus_handle.clone()
    }
}

impl Render for ThreadDetailsModal {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let details = self.details.clone();
        let row = |id: &'static str,
                   label: &'static str,
                   value: Option<String>,
                   copy: bool|
         -> Option<AnyElement> {
            let value = value?;
            Some(
                h_flex()
                    .w_full()
                    .gap_3()
                    .child(
                        div()
                            .w(scaled(px(96.)))
                            .flex_none()
                            .child(Label::new(label).size(LabelSize::Small).color(Color::Muted)),
                    )
                    .child(
                        div()
                            .flex_1()
                            .min_w_0()
                            .child(Label::new(value.clone()).size(LabelSize::Small).truncate()),
                    )
                    .when(copy, |this| {
                        this.child(
                            IconButton::new(id, IconName::Copy)
                                .icon_size(IconSize::XSmall)
                                .icon_color(Color::Muted)
                                .tooltip(Tooltip::text(format!("Copy {}", label.to_lowercase())))
                                .on_click(move |_, _, cx| {
                                    cx.write_to_clipboard(ClipboardItem::new_string(value.clone()))
                                }),
                        )
                    })
                    .into_any_element(),
            )
        };
        let works_in = if details.in_worktree {
            "Worktree"
        } else {
            "Checkout"
        };
        let timing = match (&details.created, &details.updated) {
            (Some(created), Some(updated)) => Some(format!("{created}, last change {updated}")),
            (Some(created), None) => Some(created.clone()),
            _ => None,
        };
        v_flex()
            .key_context("ThreadDetails")
            .track_focus(&self.focus_handle)
            .on_action(cx.listener(Self::cancel))
            .w(scaled(px(480.)))
            .elevation_2(cx)
            .p_3()
            .gap_2()
            .child(
                v_flex()
                    .gap_0p5()
                    .child(
                        Label::new("Thread details")
                            .size(LabelSize::Small)
                            .color(Color::Muted),
                    )
                    .child(Label::new(details.title.clone()).weight(FontWeight::SEMIBOLD)),
            )
            .child(
                v_flex()
                    .gap_1()
                    .children(row(
                        "thread-details-project",
                        "Project",
                        details.project,
                        false,
                    ))
                    .children(row("thread-details-path", works_in, details.path, true))
                    .children(row("thread-details-branch", "Branch", details.branch, true))
                    .children(row(
                        "thread-details-model",
                        "Model",
                        Some(details.model),
                        false,
                    ))
                    .children(row(
                        "thread-details-mode",
                        "Permissions",
                        Some(details.permission_mode),
                        false,
                    ))
                    .children(row(
                        "thread-details-status",
                        "Status",
                        details.status,
                        false,
                    ))
                    .children(row("thread-details-started", "Started", timing, false))
                    .children(row(
                        "thread-details-pull-request",
                        "Pull request",
                        details.pull_request,
                        false,
                    ))
                    .children(row("thread-details-parent", "From", details.parent, false))
                    .children(row(
                        "thread-details-id",
                        "Thread ID",
                        Some(details.id),
                        true,
                    )),
            )
    }
}

/// Shows Thread Details for `thread_id` in `workspace`.
pub(crate) fn show_details(
    thread_id: &str,
    workspace: WeakEntity<Workspace>,
    window: &mut Window,
    cx: &mut App,
) {
    let Some(details) = thread_details(thread_id, crate::sidebar::now(), cx) else {
        return;
    };
    workspace
        .update(cx, |workspace, cx| {
            workspace.toggle_modal(window, cx, move |_, cx| ThreadDetailsModal {
                details,
                focus_handle: cx.focus_handle(),
            })
        })
        .ok();
}

pub(crate) fn init(cx: &mut App) {
    cx.observe_new(|workspace: &mut Workspace, _, _| {
        workspace.register_action(|workspace, _: &ShowThreadDetails, window, cx| {
            let thread_id = organize::active_thread_view(workspace, cx)
                .and_then(|view| view.read(cx).thread_id().map(str::to_string));
            match thread_id {
                Some(thread_id) => {
                    let handle = workspace.weak_handle();
                    window.defer(cx, move |window, cx| {
                        show_details(&thread_id, handle, window, cx)
                    });
                }
                None => cx.propagate(),
            }
        });
    })
    .detach();
}

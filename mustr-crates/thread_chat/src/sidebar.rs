//! The threads sidebar: every agent thread in T3's shell, newest first, with its status, kept
//! current by `orchestration.subscribeShell`. Clicking one opens it.

use crate::{backend::ThreadBackend, thread_view::NewThread};
use collections::HashMap;
use futures::StreamExt as _;
use gpui::{
    App, Context, FocusHandle, Focusable, FontWeight, IntoElement, ParentElement as _, Render,
    SharedString, Styled as _, Task, UniformListScrollHandle, WeakEntity, Window, div, px,
    uniform_list,
};
use mustr_ui::{palette, tokens};
use std::sync::Arc;
use t3_api::{
    tagged::OrchestrationV2ShellStreamItem,
    types::{
        OrchestrationV2ShellSnapshot, OrchestrationV2SubscribeShellInput,
        OrchestrationV2ThreadShell,
    },
};
use ui::{
    Color, IconButton, IconName, IconSize, Indicator, Label, LabelCommon as _, LabelSize, Tooltip,
    prelude::*,
};
use workspace::Workspace;

/// How a thread is doing, from T3's shell.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ShellStatus {
    NeedsInput,
    Working(String),
    Failed,
    Settled(String),
}

impl ShellStatus {
    pub fn from_shell(thread: &OrchestrationV2ThreadShell) -> Self {
        if thread.pending_runtime_request.is_some() {
            return Self::NeedsInput;
        }
        let status = crate::model::wire_string(&thread.status);
        match status.as_str() {
            "preparing" | "queued" | "starting" | "running" | "waiting" => Self::Working(status),
            "failed" => Self::Failed,
            _ => Self::Settled(status.replace('_', " ")),
        }
    }

    pub fn label(&self) -> &str {
        match self {
            Self::NeedsInput => "needs input",
            Self::Failed => "failed",
            Self::Working(status) | Self::Settled(status) => status,
        }
    }

    pub fn color(&self) -> Color {
        match self {
            Self::NeedsInput => Color::Warning,
            Self::Working(_) => Color::Accent,
            Self::Failed => Color::Error,
            Self::Settled(_) => Color::Muted,
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ThreadEntry {
    pub id: String,
    pub title: String,
    pub project_id: String,
    pub branch: Option<String>,
    pub updated_at: String,
    pub status: ShellStatus,
}

impl ThreadEntry {
    fn from_shell(thread: &OrchestrationV2ThreadShell) -> Option<Self> {
        if thread.deleted_at.is_some() || thread.archived_at.is_some() {
            return None;
        }
        Some(Self {
            id: thread.id.clone(),
            title: if thread.title.trim().is_empty() {
                "Untitled thread".to_string()
            } else {
                thread.title.clone()
            },
            project_id: thread.project_id.clone(),
            branch: thread.branch.clone(),
            updated_at: thread.updated_at.clone(),
            status: ShellStatus::from_shell(thread),
        })
    }
}

/// The shell's threads and projects, as the sidebar lists them.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct ThreadList {
    /// Newest first.
    pub threads: Vec<ThreadEntry>,
    pub projects: HashMap<String, String>,
    pub sequence: i64,
}

impl ThreadList {
    pub fn from_snapshot(snapshot: &OrchestrationV2ShellSnapshot) -> Self {
        let mut list = Self {
            threads: snapshot
                .threads
                .iter()
                .filter_map(ThreadEntry::from_shell)
                .collect(),
            projects: snapshot
                .projects
                .iter()
                .map(|project| (project.id.clone(), project.title.clone()))
                .collect(),
            sequence: snapshot.snapshot_sequence,
        };
        list.sort();
        list
    }

    fn sort(&mut self) {
        // Timestamps are ISO 8601 UTC strings, so string order is time order.
        self.threads
            .sort_by(|left, right| right.updated_at.cmp(&left.updated_at));
    }

    /// Applies one item of `orchestration.subscribeShell`. Returns whether anything changed.
    pub fn apply(&mut self, item: OrchestrationV2ShellStreamItem) -> bool {
        match item {
            // A snapshot with resolved repository roots only refreshes the projects; on a resumed
            // subscription it has no threads at all (`mergeShellSnapshotProjects` in T3's
            // client-runtime).
            OrchestrationV2ShellStreamItem::Snapshot(snapshot)
                if snapshot.resolved_repository_identity_roots.is_some() =>
            {
                self.projects.extend(
                    snapshot
                        .snapshot
                        .projects
                        .into_iter()
                        .map(|project| (project.id, project.title)),
                );
                true
            }
            OrchestrationV2ShellStreamItem::Snapshot(snapshot) => {
                *self = Self::from_snapshot(&snapshot.snapshot);
                true
            }
            OrchestrationV2ShellStreamItem::ThreadUpdated(update) => {
                self.sequence = self.sequence.max(update.sequence);
                self.threads.retain(|thread| thread.id != update.thread.id);
                if let Some(entry) = ThreadEntry::from_shell(&update.thread) {
                    self.threads.push(entry);
                }
                self.sort();
                true
            }
            OrchestrationV2ShellStreamItem::ThreadRemoved(removed) => {
                self.sequence = self.sequence.max(removed.sequence);
                let before = self.threads.len();
                self.threads.retain(|thread| thread.id != removed.thread_id);
                before != self.threads.len()
            }
            OrchestrationV2ShellStreamItem::ProjectUpdated(update) => {
                self.sequence = self.sequence.max(update.sequence);
                self.projects
                    .insert(update.project.id, update.project.title);
                true
            }
            OrchestrationV2ShellStreamItem::ProjectRemoved(_)
            | OrchestrationV2ShellStreamItem::Synchronized(_)
            | OrchestrationV2ShellStreamItem::Unrecognized(_) => false,
        }
    }
}

enum Loadable {
    NotLoaded,
    Loading,
    Failed(SharedString),
    Loaded(ThreadList),
}

pub struct ThreadsSidebar {
    focus_handle: FocusHandle,
    workspace: WeakEntity<Workspace>,
    list: Loadable,
    active_thread_id: Option<String>,
    scroll_handle: UniformListScrollHandle,
    _follow: Option<Task<()>>,
}

impl ThreadsSidebar {
    pub fn new(workspace: WeakEntity<Workspace>, cx: &mut Context<Self>) -> Self {
        Self {
            focus_handle: cx.focus_handle(),
            workspace,
            list: Loadable::NotLoaded,
            active_thread_id: None,
            scroll_handle: UniformListScrollHandle::new(),
            _follow: None,
        }
    }

    /// Highlights the thread the workspace shows.
    pub fn set_active_thread(&mut self, thread_id: Option<String>, cx: &mut Context<Self>) {
        if self.active_thread_id != thread_id {
            self.active_thread_id = thread_id;
            cx.notify();
        }
    }

    pub fn threads(&self) -> &[ThreadEntry] {
        match &self.list {
            Loadable::Loaded(list) => &list.threads,
            _ => &[],
        }
    }

    /// Loads the list and follows it, once the sidebar is first shown.
    pub fn ensure_loaded(&mut self, cx: &mut Context<Self>) {
        if !matches!(self.list, Loadable::NotLoaded) {
            return;
        }
        let Some(backend) = crate::backend(cx) else {
            self.list = Loadable::Failed("mustr isn't set up to show agent threads".into());
            return;
        };
        self.list = Loadable::Loading;
        self._follow = Some(cx.spawn(async move |this, cx| {
            let mut attempt = 0;
            loop {
                match follow_shell(&backend, &this, cx).await {
                    Ok(()) => attempt = 0,
                    Err(error) => {
                        log::warn!("couldn't follow the thread list: {error:#}");
                        let failed = this.update(cx, |this, cx| {
                            if !matches!(this.list, Loadable::Loaded(_)) {
                                this.list = Loadable::Failed(format!("{error:#}").into());
                                cx.notify();
                            }
                        });
                        if failed.is_err() {
                            return;
                        }
                    }
                }
                cx.background_executor()
                    .timer(crate::backend::retry_delay(attempt))
                    .await;
                attempt += 1;
            }
        }));
    }

    fn open(&mut self, index: usize, window: &mut Window, cx: &mut Context<Self>) {
        let Some(thread) = self.threads().get(index).cloned() else {
            return;
        };
        crate::open_thread(
            thread.id,
            thread.title.into(),
            self.workspace.clone(),
            window,
            cx,
        );
    }

    fn render_entry(&self, index: usize, cx: &mut Context<Self>) -> impl IntoElement + use<> {
        let palette = palette(cx);
        let Loadable::Loaded(list) = &self.list else {
            return div().id(index);
        };
        let Some(thread) = list.threads.get(index) else {
            return div().id(index);
        };
        let active = self.active_thread_id.as_deref() == Some(thread.id.as_str());
        let details = [
            list.projects.get(&thread.project_id).map(String::as_str),
            thread.branch.as_deref(),
        ]
        .into_iter()
        .flatten()
        .collect::<Vec<_>>()
        .join(" · ");
        let color = thread.status.color();
        div()
            .id(index)
            .w_full()
            .h(px(44.))
            .px_2()
            .py_1()
            .rounded(tokens::CONTROL_RADIUS)
            .cursor_pointer()
            .when(active, |this| this.bg(palette.accent_soft.hsla()))
            .hover(|style| style.bg(palette.surface_2.hsla()))
            .on_click(cx.listener(move |this, _, window, cx| this.open(index, window, cx)))
            .child(
                v_flex()
                    .min_w_0()
                    .child(
                        h_flex()
                            .gap_1p5()
                            .child(Indicator::dot().color(color))
                            .child(
                                div().flex_1().min_w_0().child(
                                    Label::new(thread.title.clone())
                                        .size(LabelSize::Small)
                                        .truncate(),
                                ),
                            ),
                    )
                    .child(
                        h_flex()
                            .gap_1p5()
                            .pl(px(12.))
                            .child(
                                div().flex_1().min_w_0().child(
                                    Label::new(if details.is_empty() {
                                        "No project".to_string()
                                    } else {
                                        details
                                    })
                                    .size(LabelSize::XSmall)
                                    .color(Color::Muted)
                                    .truncate(),
                                ),
                            )
                            .child(
                                Label::new(thread.status.label().to_string())
                                    .size(LabelSize::XSmall)
                                    .color(color),
                            ),
                    ),
            )
    }
}

async fn follow_shell(
    backend: &Arc<dyn ThreadBackend>,
    this: &WeakEntity<ThreadsSidebar>,
    cx: &mut gpui::AsyncApp,
) -> anyhow::Result<()> {
    let snapshot = backend.shell_snapshot().await?;
    let sequence = this.update(cx, |this, cx| {
        let list = ThreadList::from_snapshot(&snapshot);
        let sequence = list.sequence;
        this.list = Loadable::Loaded(list);
        cx.notify();
        sequence
    })?;
    let mut stream = backend.subscribe_shell(OrchestrationV2SubscribeShellInput {
        after_sequence: Some(sequence),
        request_completion_marker: None,
    })?;
    while let Some(item) = stream.next().await {
        let item = item?;
        this.update(cx, |this, cx| {
            if let Loadable::Loaded(list) = &mut this.list
                && list.apply(item)
            {
                cx.notify();
            }
        })?;
    }
    Ok(())
}

impl Focusable for ThreadsSidebar {
    fn focus_handle(&self, _: &App) -> FocusHandle {
        self.focus_handle.clone()
    }
}

impl Render for ThreadsSidebar {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        self.ensure_loaded(cx);
        let palette = palette(cx);
        let count = match &self.list {
            Loadable::Loaded(list) => Some(list.threads.len()),
            _ => None,
        };
        let body = match &self.list {
            Loadable::NotLoaded | Loadable::Loading => note("Loading threads…", Color::Muted),
            Loadable::Failed(error) => note(error.clone(), Color::Error),
            Loadable::Loaded(list) if list.threads.is_empty() => {
                note("No agent threads yet.", Color::Muted)
            }
            Loadable::Loaded(list) => uniform_list(
                "agent-threads",
                list.threads.len(),
                cx.processor(|this, range: std::ops::Range<usize>, _, cx| {
                    range
                        .map(|index| this.render_entry(index, cx))
                        .collect::<Vec<_>>()
                }),
            )
            .track_scroll(&self.scroll_handle)
            .flex_1()
            .px_1()
            .into_any_element(),
        };
        v_flex()
            .key_context("ThreadsSidebar")
            .track_focus(&self.focus_handle)
            .size_full()
            .bg(palette.surface.hsla())
            .border_r(tokens::BORDER_WIDTH)
            .border_color(palette.border.hsla())
            .child(
                h_flex()
                    .flex_none()
                    .gap_1p5()
                    .px_3()
                    .py(px(8.))
                    .child(
                        Label::new("Agent threads")
                            .size(LabelSize::Small)
                            .weight(FontWeight::SEMIBOLD),
                    )
                    .when_some(count, |this, count| {
                        this.child(
                            Label::new(count.to_string())
                                .size(LabelSize::XSmall)
                                .color(Color::Muted),
                        )
                    })
                    .child(div().flex_1())
                    .child(
                        IconButton::new("new-agent-thread", IconName::Plus)
                            .icon_size(IconSize::Small)
                            .tooltip(Tooltip::text("New thread"))
                            .on_click(|_, window, cx| {
                                window.dispatch_action(Box::new(NewThread), cx)
                            }),
                    ),
            )
            .child(body)
    }
}

fn note(text: impl Into<SharedString>, color: Color) -> gpui::AnyElement {
    div()
        .px_3()
        .py_2()
        .child(Label::new(text.into()).size(LabelSize::Small).color(color))
        .into_any_element()
}

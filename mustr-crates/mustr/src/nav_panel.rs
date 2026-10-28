//! The left dock's panel: mustr's icon rail, and next to it a sidebar: the file tree of the
//! active review, or the agent threads.
//!
//! A dock shows one panel at a time, so the rail and the sidebars share this panel
//! (`mustr-docs/ZED-INTEGRATION.md` Q4). While a review is active the panel is as wide as the
//! rail plus the tree. While a thread is active, or after the rail's Agents button, it shows the
//! threads instead. Elsewhere it narrows to just the rail.
//!
//! Other parts of mustr add their own rail buttons with [`register_rail_entry`].

use crate::{home::HomeView, navigation};
use gpui::{
    Action, AnyElement, App, AppContext as _, Context, Entity, EventEmitter, FocusHandle,
    Focusable, Global, InteractiveElement as _, IntoElement, ParentElement as _, Pixels, Render,
    SharedString, Styled as _, Subscription, WeakEntity, Window, div, px,
};
use mustr_ui::{MustrIcon, RailButton, tokens};
use review::{FileTreePanel, ReviewView, ToggleFileTree};
use thread_chat::{ThreadView, ThreadsSidebar, ThreadsSidebarEvent};
use ui::{FluentBuilder as _, Icon, h_flex, v_flex};
use workspace::{
    ItemHandle, Workspace,
    dock::{Dock, DockPosition, Panel, PanelEvent, PanelSizeState},
};
use zed_actions::OpenSettingsFile;

const DEFAULT_TREE_WIDTH: Pixels = px(280.);

/// A rail button added with [`register_rail_entry`].
struct RailEntry {
    id: SharedString,
    icon: Icon,
    tooltip: SharedString,
    action: Box<dyn Action>,
    is_active: fn(&dyn ItemHandle) -> bool,
}

#[derive(Default)]
struct RailEntries(Vec<RailEntry>);

impl Global for RailEntries {}

/// Adds a button to the icon rail, under the built-in ones and above Settings, in the order
/// they're added. Clicking it dispatches `action` from the rail, so the workspace's handlers get
/// it. `is_active` says whether the active item is the entry's destination, which highlights the
/// button. Call it from the package's `init`, before windows open.
pub fn register_rail_entry(
    id: impl Into<SharedString>,
    icon: Icon,
    tooltip: impl Into<SharedString>,
    action: impl Action,
    is_active: fn(&dyn ItemHandle) -> bool,
    cx: &mut App,
) {
    cx.default_global::<RailEntries>().0.push(RailEntry {
        id: id.into(),
        icon,
        tooltip: tooltip.into(),
        action: Box::new(action),
        is_active,
    });
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Destination {
    Home,
    Review,
    Thread,
    Other,
}

/// What shows next to the rail.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Sidebar {
    None,
    FileTree,
    Threads,
}

pub struct NavPanel {
    focus_handle: FocusHandle,
    workspace: WeakEntity<Workspace>,
    file_tree: Entity<FileTreePanel>,
    threads: Entity<ThreadsSidebar>,
    destination: Destination,
    /// The registered rail entry whose destination is active.
    active_rail_entry: Option<SharedString>,
    /// Whether the threads show while neither a review nor a thread is active (the rail's Agents
    /// button toggles it).
    threads_open: bool,
    /// What the panel was last sized for.
    sized_for: Option<Sidebar>,
    /// The panel's width with each sidebar, restored when that sidebar shows again.
    file_tree_width: Option<Pixels>,
    threads_width: Option<Pixels>,
    _subscriptions: Vec<Subscription>,
}

impl NavPanel {
    /// Doesn't read `workspace`, so it can be called while the workspace is being updated (for
    /// example, from `observe_new`).
    pub fn new(workspace: &Entity<Workspace>, window: &mut Window, cx: &mut Context<Self>) -> Self {
        let file_tree = cx.new(|cx| FileTreePanel::new(workspace, window, cx));
        let threads = cx.new(|cx| ThreadsSidebar::new(workspace.downgrade(), window, cx));
        let reveal = cx.subscribe_in(
            &threads,
            window,
            |this, _, event: &ThreadsSidebarEvent, window, cx| match event {
                ThreadsSidebarEvent::Reveal => this.reveal_threads(window, cx),
            },
        );
        let subscription = cx.subscribe_in(
            workspace,
            window,
            |this, workspace, event: &workspace::Event, window, cx| {
                if let workspace::Event::ActiveItemChanged = event {
                    let workspace = workspace.read(cx);
                    let item = workspace.active_item(cx);
                    let left_dock = workspace.left_dock().clone();
                    this.active_item_changed(item.as_deref(), &left_dock, window, cx);
                }
            },
        );
        Self {
            focus_handle: cx.focus_handle(),
            workspace: workspace.downgrade(),
            file_tree,
            threads,
            destination: Destination::Other,
            active_rail_entry: None,
            threads_open: false,
            sized_for: None,
            file_tree_width: None,
            threads_width: None,
            _subscriptions: vec![subscription, reveal],
        }
    }

    /// Highlights the rail button for `item`, and shows the sidebar that goes with it (widening
    /// the panel). Call once the panel is in `left_dock`, and whenever the active item changes.
    pub fn active_item_changed(
        &mut self,
        item: Option<&dyn ItemHandle>,
        left_dock: &Entity<Dock>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let thread = item.and_then(|item| item.downcast::<ThreadView>());
        let destination = match item {
            Some(item) if item.downcast::<ReviewView>().is_some() => Destination::Review,
            Some(item) if item.downcast::<HomeView>().is_some() => Destination::Home,
            Some(_) if thread.is_some() => Destination::Thread,
            _ => Destination::Other,
        };
        let thread_id = thread.and_then(|thread| thread.read(cx).thread_id().map(str::to_string));
        self.threads
            .update(cx, |threads, cx| threads.set_active_thread(thread_id, cx));
        self.destination = destination;
        self.active_rail_entry = item.and_then(|item| {
            cx.try_global::<RailEntries>()?
                .0
                .iter()
                .find(|entry| (entry.is_active)(item))
                .map(|entry| entry.id.clone())
        });
        cx.notify();
        self.update_size(left_dock, window, cx);
    }

    fn sidebar(&self) -> Sidebar {
        match self.destination {
            Destination::Review => Sidebar::FileTree,
            Destination::Thread => Sidebar::Threads,
            _ if self.threads_open => Sidebar::Threads,
            _ => Sidebar::None,
        }
    }

    fn update_size(
        &mut self,
        left_dock: &Entity<Dock>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let sidebar = self.sidebar();
        if self.sized_for == Some(sidebar) {
            return;
        }
        let panel = cx.entity();
        let current_width = left_dock
            .read(cx)
            .stored_panel_size_state(&panel)
            .and_then(|size_state| size_state.size);
        match self.sized_for {
            Some(Sidebar::FileTree) => self.file_tree_width = current_width,
            Some(Sidebar::Threads) => self.threads_width = current_width,
            Some(Sidebar::None) | None => {}
        }
        let width = match sidebar {
            Sidebar::FileTree => self
                .file_tree_width
                .unwrap_or_else(|| self.default_size(window, cx)),
            Sidebar::Threads => self
                .threads_width
                .unwrap_or_else(|| self.default_size(window, cx)),
            Sidebar::None => tokens::RAIL_WIDTH,
        };
        self.sized_for = Some(sidebar);
        left_dock.update(cx, |dock, cx| {
            dock.set_panel_size_state(
                &panel,
                PanelSizeState {
                    size: Some(width),
                    flex: None,
                },
                cx,
            );
        });
    }

    /// The rail's Agents button: shows or hides the threads next to the rail.
    fn toggle_threads(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.destination == Destination::Thread {
            return;
        }
        self.threads_open = !self.threads_open;
        cx.notify();
        let Some(left_dock) = self
            .workspace
            .read_with(cx, |workspace, _| workspace.left_dock().clone())
            .ok()
        else {
            return;
        };
        self.update_size(&left_dock, window, cx);
    }

    /// Shows the threads next to the rail, opening the dock if it was closed, for the threads'
    /// search.
    fn reveal_threads(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.sidebar() != Sidebar::Threads {
            self.toggle_threads(window, cx);
        }
        let workspace = self.workspace.clone();
        window.defer(cx, move |window, cx| {
            workspace
                .update(cx, |workspace, cx| {
                    workspace.open_panel::<NavPanel>(window, cx)
                })
                .ok();
        });
    }

    fn with_workspace(
        &self,
        window: &mut Window,
        cx: &mut App,
        f: impl FnOnce(&mut Workspace, &mut Window, &mut Context<Workspace>),
    ) {
        self.workspace
            .update(cx, |workspace, cx| f(workspace, window, cx))
            .ok();
    }

    fn render_rail(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let palette = mustr_ui::palette(cx);
        let this = cx.entity().downgrade();
        let on_click = move |f: fn(&mut Self, &mut Window, &mut Context<Self>)| {
            let this = this.clone();
            move |_: &gpui::ClickEvent, window: &mut Window, cx: &mut App| {
                this.update(cx, |this, cx| f(this, window, cx)).ok();
            }
        };
        v_flex()
            .flex_none()
            .w(tokens::RAIL_WIDTH)
            .h_full()
            .items_center()
            .gap(px(6.))
            .py(px(10.))
            .bg(palette.surface.hsla())
            .when(self.sidebar() != Sidebar::None, |this| {
                this.border_r(tokens::BORDER_WIDTH)
                    .border_color(palette.border.hsla())
            })
            .child(
                RailButton::new("rail-home", MustrIcon::Inbox.icon())
                    .active(self.destination == Destination::Home)
                    .tooltip("Home: pull requests and agent threads")
                    .on_click(on_click(|this, window, cx| {
                        this.with_workspace(window, cx, navigation::show_home)
                    })),
            )
            .child(
                RailButton::new("rail-pull-requests", MustrIcon::PullRequest.icon())
                    .active(self.destination == Destination::Review)
                    .tooltip("Pull requests: the last one you reviewed")
                    .on_click(on_click(Self::show_latest_review)),
            )
            .child(
                RailButton::new("rail-agents", MustrIcon::Agent.icon())
                    .active(self.sidebar() == Sidebar::Threads)
                    .tooltip("Agent threads")
                    .on_click(on_click(Self::toggle_threads)),
            )
            .children(self.render_rail_entries(cx))
            .child(div().flex_1())
            .child(
                RailButton::new("rail-settings", MustrIcon::Settings.icon())
                    .tooltip("Settings")
                    .on_click(|_, window, cx| {
                        window.dispatch_action(OpenSettingsFile.boxed_clone(), cx)
                    }),
            )
    }

    fn render_rail_entries(&self, cx: &App) -> Vec<AnyElement> {
        let Some(entries) = cx.try_global::<RailEntries>() else {
            return Vec::new();
        };
        entries
            .0
            .iter()
            .map(|entry| {
                let action = entry.action.boxed_clone();
                let focus_handle = self.focus_handle.clone();
                let selector = format!("rail-entry-{}", entry.id);
                div()
                    .debug_selector(|| selector)
                    .child(
                        RailButton::new(
                            SharedString::from(format!("rail-{}", entry.id)),
                            entry.icon.clone(),
                        )
                        .active(self.active_rail_entry.as_ref() == Some(&entry.id))
                        .tooltip(entry.tooltip.clone())
                        .on_click(move |_, window, cx| {
                            focus_handle.dispatch_action(action.as_ref(), window, cx)
                        }),
                    )
                    .into_any_element()
            })
            .collect()
    }

    /// Activates the review the file tree shows (the last one that was active), or goes home to
    /// pick one.
    fn show_latest_review(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let review = self.file_tree.read(cx).review();
        self.with_workspace(window, cx, |workspace, window, cx| match review {
            Some(review) => {
                workspace.activate_item(&review, true, true, window, cx);
            }
            None => navigation::show_home(workspace, window, cx),
        });
    }
}

impl EventEmitter<PanelEvent> for NavPanel {}

impl Focusable for NavPanel {
    fn focus_handle(&self, _: &App) -> FocusHandle {
        self.focus_handle.clone()
    }
}

impl Panel for NavPanel {
    fn persistent_name() -> &'static str {
        "Navigation"
    }

    fn panel_key() -> &'static str {
        "MustrNavigation"
    }

    fn activation_focus_handle(&self, cx: &App) -> FocusHandle {
        match self.sidebar() {
            Sidebar::FileTree => self.file_tree.focus_handle(cx),
            Sidebar::Threads => self.threads.focus_handle(cx),
            Sidebar::None => self.focus_handle.clone(),
        }
    }

    fn position(&self, _: &Window, _: &App) -> DockPosition {
        DockPosition::Left
    }

    fn position_is_valid(&self, position: DockPosition) -> bool {
        position == DockPosition::Left
    }

    fn set_position(&mut self, _: DockPosition, _: &mut Window, _: &mut Context<Self>) {}

    fn default_size(&self, _: &Window, _: &App) -> Pixels {
        tokens::RAIL_WIDTH + DEFAULT_TREE_WIDTH
    }

    fn starts_open(&self, _: &Window, _: &App) -> bool {
        true
    }

    // The rail replaces the status bar's dock buttons, so there's no icon.
    fn icon(&self, _: &Window, _: &App) -> Option<ui::IconName> {
        None
    }

    fn icon_tooltip(&self, _: &Window, _: &App) -> Option<&'static str> {
        None
    }

    fn toggle_action(&self) -> Box<dyn Action> {
        Box::new(ToggleFileTree)
    }

    fn activation_priority(&self) -> u32 {
        0
    }
}

impl Render for NavPanel {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        h_flex()
            .key_context("MustrNavigation")
            .track_focus(&self.focus_handle)
            .size_full()
            .child(self.render_rail(cx))
            .map(|this| match self.sidebar() {
                Sidebar::FileTree => this.child(
                    div()
                        .flex_1()
                        .min_w_0()
                        .h_full()
                        .child(self.file_tree.clone()),
                ),
                Sidebar::Threads => this.child(
                    div()
                        .flex_1()
                        .min_w_0()
                        .h_full()
                        .child(self.threads.clone()),
                ),
                Sidebar::None => this,
            })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use anyhow::{Result, anyhow};
    use futures::{FutureExt as _, future::BoxFuture, stream::BoxStream};
    use gpui::{TestAppContext, VisualTestContext};
    use serde_json::Value;
    use settings::SettingsStore;
    use std::sync::Arc;
    use std::{cell::Cell, rc::Rc};
    use t3_client::t3_api::{
        tagged::{
            OrchestrationV2Command, OrchestrationV2ShellStreamItem, OrchestrationV2ThreadStreamItem,
        },
        types::{
            OrchestrationV2DispatchCommandResult, OrchestrationV2GetTurnItemInput,
            OrchestrationV2GetTurnItemResult, OrchestrationV2ShellSnapshot,
            OrchestrationV2SubscribeShellInput, OrchestrationV2SubscribeThreadInput,
            OrchestrationV2ThreadDetailSnapshot, OrchestrationV2ThreadLaunchInput,
            OrchestrationV2ThreadLaunchResult, ServerProvider,
        },
    };
    use thread_chat::{Access, AttachmentUpload, ThreadBackend};
    use workspace::MultiWorkspace;

    gpui::actions!(nav_panel_tests, [OpenSchedules]);

    /// A T3 that isn't there: every read fails, and nothing can be written.
    struct UnreachableBackend;

    fn unreachable<T: Send + 'static>() -> BoxFuture<'static, Result<T>> {
        futures::future::ready(Err(anyhow!("not connected"))).boxed()
    }

    impl ThreadBackend for UnreachableBackend {
        fn shell_snapshot(&self) -> BoxFuture<'static, Result<OrchestrationV2ShellSnapshot>> {
            unreachable()
        }

        fn subscribe_shell(
            &self,
            _: OrchestrationV2SubscribeShellInput,
        ) -> Result<BoxStream<'static, Result<OrchestrationV2ShellStreamItem>>> {
            Err(anyhow!("not connected"))
        }

        fn thread_snapshot(
            &self,
            _: String,
        ) -> BoxFuture<'static, Result<OrchestrationV2ThreadDetailSnapshot>> {
            unreachable()
        }

        fn subscribe_thread(
            &self,
            _: OrchestrationV2SubscribeThreadInput,
        ) -> Result<BoxStream<'static, Result<OrchestrationV2ThreadStreamItem>>> {
            Err(anyhow!("not connected"))
        }

        fn turn_item(
            &self,
            _: OrchestrationV2GetTurnItemInput,
        ) -> BoxFuture<'static, Result<OrchestrationV2GetTurnItemResult>> {
            unreachable()
        }

        fn providers(&self) -> BoxFuture<'static, Result<Vec<ServerProvider>>> {
            unreachable()
        }

        fn dispatch(
            &self,
            _: OrchestrationV2Command,
        ) -> BoxFuture<'static, Result<OrchestrationV2DispatchCommandResult>> {
            unreachable()
        }

        fn launch_thread(
            &self,
            _: OrchestrationV2ThreadLaunchInput,
        ) -> BoxFuture<'static, Result<OrchestrationV2ThreadLaunchResult>> {
            unreachable()
        }

        fn call_raw(&self, _: &str, _: Value, _: Access) -> BoxFuture<'static, Result<Value>> {
            unreachable()
        }

        fn subscribe_raw(
            &self,
            _: &str,
            _: Value,
            _: Access,
        ) -> Result<BoxStream<'static, Result<Value>>> {
            Err(anyhow!("not connected"))
        }

        fn upload(&self, _: AttachmentUpload) -> BoxFuture<'static, Result<()>> {
            unreachable()
        }
    }

    async fn workspace_with_nav_panel(
        cx: &mut TestAppContext,
    ) -> (Entity<Workspace>, Entity<NavPanel>, &mut VisualTestContext) {
        cx.update(|cx| {
            let settings_store = SettingsStore::test(cx);
            cx.set_global(settings_store);
            theme_settings::init(theme::LoadThemes::JustBase, cx);
            thread_chat::set_backend(Arc::new(UnreachableBackend), cx);
        });
        let fs = project::FakeFs::new(cx.executor());
        let project = project::Project::test(fs, [], cx).await;
        let (multi_workspace, cx) =
            cx.add_window_view(|window, cx| MultiWorkspace::test_new(project.clone(), window, cx));
        let workspace =
            multi_workspace.read_with(cx, |multi_workspace, _| multi_workspace.workspace().clone());
        let nav_panel = workspace.update_in(cx, |workspace, window, cx| {
            let workspace_handle = cx.entity();
            let nav_panel = cx.new(|cx| NavPanel::new(&workspace_handle, window, cx));
            workspace.add_panel(nav_panel.clone(), window, cx);
            nav_panel
        });
        cx.run_until_parked();
        (workspace, nav_panel, cx)
    }

    #[gpui::test]
    async fn threads_open_from_the_sidebar_and_show_it(cx: &mut TestAppContext) {
        let (workspace, nav_panel, cx) = workspace_with_nav_panel(cx).await;
        nav_panel.read_with(cx, |nav_panel, _| {
            assert_eq!(nav_panel.sidebar(), Sidebar::None)
        });

        // As a click on one of the sidebar's threads does: from inside the sidebar's update,
        // which activating the new item reads.
        let threads = nav_panel.read_with(cx, |nav_panel, _| nav_panel.threads.clone());
        let weak_workspace = workspace.downgrade();
        threads.update_in(cx, |_, window, cx| {
            thread_chat::open_thread(
                "thread-1".into(),
                "Retry".into(),
                weak_workspace.clone(),
                window,
                cx,
            )
        });
        cx.run_until_parked();

        let active = workspace.read_with(cx, |workspace, cx| workspace.active_item(cx));
        let thread = active
            .and_then(|item| item.downcast::<ThreadView>())
            .expect("the thread is active");
        assert_eq!(
            thread.read_with(cx, |thread, _| thread.thread_id().map(str::to_string)),
            Some("thread-1".to_string())
        );
        nav_panel.read_with(cx, |nav_panel, _| {
            assert_eq!(nav_panel.destination, Destination::Thread);
            assert_eq!(nav_panel.sidebar(), Sidebar::Threads);
        });
    }

    #[gpui::test]
    async fn the_agents_button_toggles_the_threads(cx: &mut TestAppContext) {
        let (_workspace, nav_panel, cx) = workspace_with_nav_panel(cx).await;
        nav_panel.update_in(cx, |nav_panel, window, cx| {
            nav_panel.toggle_threads(window, cx)
        });
        nav_panel.read_with(cx, |nav_panel, _| {
            assert_eq!(nav_panel.sidebar(), Sidebar::Threads)
        });
        nav_panel.update_in(cx, |nav_panel, window, cx| {
            nav_panel.toggle_threads(window, cx)
        });
        nav_panel.read_with(cx, |nav_panel, _| {
            assert_eq!(nav_panel.sidebar(), Sidebar::None)
        });
    }

    #[gpui::test]
    async fn searching_threads_shows_the_threads(cx: &mut TestAppContext) {
        let (_workspace, nav_panel, cx) = workspace_with_nav_panel(cx).await;
        let threads = nav_panel.read_with(cx, |nav_panel, _| nav_panel.threads.clone());
        threads.update_in(cx, |threads, window, cx| threads.reveal_search(window, cx));
        cx.run_until_parked();
        nav_panel.read_with(cx, |nav_panel, _| {
            assert_eq!(nav_panel.sidebar(), Sidebar::Threads)
        });
        // Asking again leaves them shown.
        threads.update_in(cx, |threads, window, cx| threads.reveal_search(window, cx));
        cx.run_until_parked();
        nav_panel.read_with(cx, |nav_panel, _| {
            assert_eq!(nav_panel.sidebar(), Sidebar::Threads)
        });
    }

    #[gpui::test]
    async fn registered_rail_entries_open_their_destinations(cx: &mut TestAppContext) {
        cx.update(|cx| {
            register_rail_entry(
                "schedules",
                MustrIcon::Settings.icon(),
                "Scheduled tasks",
                OpenSchedules,
                // A thread stands in for the entry's own page.
                |item| item.downcast::<ThreadView>().is_some(),
                cx,
            )
        });
        let (workspace, nav_panel, cx) = workspace_with_nav_panel(cx).await;
        let opened = Rc::new(Cell::new(0));
        workspace.update(cx, |workspace, _| {
            let opened = opened.clone();
            workspace
                .register_action(move |_, _: &OpenSchedules, _, _| opened.set(opened.get() + 1));
        });
        cx.simulate_resize(gpui::size(px(1000.), px(800.)));
        cx.run_until_parked();

        let button = cx
            .debug_bounds("rail-entry-schedules")
            .expect("the entry is on the rail");
        cx.simulate_click(button.center(), gpui::Modifiers::none());
        cx.run_until_parked();
        assert_eq!(opened.get(), 1);

        // Its destination highlights it.
        nav_panel.read_with(cx, |nav_panel, _| {
            assert_eq!(nav_panel.active_rail_entry, None)
        });
        let weak_workspace = workspace.downgrade();
        nav_panel.update_in(cx, |_, window, cx| {
            thread_chat::open_thread(
                "thread-1".into(),
                "Retry".into(),
                weak_workspace,
                window,
                cx,
            )
        });
        cx.run_until_parked();
        nav_panel.read_with(cx, |nav_panel, _| {
            assert_eq!(nav_panel.active_rail_entry.as_deref(), Some("schedules"))
        });
    }
}

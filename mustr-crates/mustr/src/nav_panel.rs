//! The left dock's panel: mustr's icon rail, and next to it a sidebar: the file tree of the
//! active review, or the agent threads.
//!
//! A dock shows one panel at a time, so the rail and the sidebars share this panel
//! (`mustr-docs/ZED-INTEGRATION.md` Q4). While a review is active the panel is as wide as the
//! rail plus the tree. While a thread is active, or after the rail's Agents button, it shows the
//! threads instead. Elsewhere it narrows to just the rail.
//!
//! The sidebar takes at most a third of the window by default, and a window too narrow for it and
//! the content next to it (a half-screen window at mustr's default size) shows reviews and threads
//! with the rail alone. The rail's Agents button shows or hides the threads in a thread, and its
//! Pull requests button the file tree in a review.
//!
//! Other parts of mustr add their own rail buttons with [`register_rail_entry`].

use crate::{home::HomeView, navigation};
use gpui::{
    Action, AnyElement, App, AppContext as _, Context, Entity, EventEmitter, FocusHandle,
    Focusable, Global, InteractiveElement as _, IntoElement, ParentElement as _, Pixels, Render,
    SharedString, Styled as _, Subscription, WeakEntity, Window, div, px,
};
use mustr_ui::{MustrIcon, RailButton, tokens, tokens::scaled};
use review::{FileTreePanel, ReviewView, ToggleFileTree};
use settings::{Settings as _, SettingsStore};
use theme_settings::ThemeSettings;
use thread_chat::{ThreadView, ThreadsSidebar, ThreadsSidebarEvent};
use ui::{FluentBuilder as _, Icon, h_flex, v_flex};
use workspace::{
    ItemHandle, Workspace,
    dock::{Dock, DockPosition, Panel, PanelEvent, PanelSizeState},
};
use zed_actions::OpenSettingsFile;

/// The tree's or the threads' width, next to the rail, at mustr's design size (`scaled`), in a
/// window wide enough for it.
const DEFAULT_TREE_WIDTH: Pixels = px(280.);

/// The narrowest the tree or the threads get by default, at mustr's design size.
const MIN_TREE_WIDTH: Pixels = px(150.);

/// The share of the window the panel takes by default, with a sidebar.
const SIDEBAR_SHARE: f32 = 1. / 3.;

/// The room the content next to the panel keeps, at mustr's design size. A window too narrow for
/// the rail, the narrowest sidebar, and this shows reviews and threads without their sidebar.
const MIN_CONTENT_WIDTH: Pixels = px(360.);

/// The panel's width with a sidebar, by default, in a window `window_width` wide, at `rem_size`:
/// a third of the window, between the narrowest and the widest sidebar.
fn default_width(window_width: Pixels, rem_size: Pixels) -> Pixels {
    let scaled = |length: Pixels| tokens::scaled(length).to_pixels(rem_size);
    let rail = scaled(tokens::RAIL_WIDTH);
    let sidebar = (window_width * SIDEBAR_SHARE - rail)
        .min(scaled(DEFAULT_TREE_WIDTH))
        .max(scaled(MIN_TREE_WIDTH));
    rail + sidebar
}

/// The widest the panel gets with a sidebar in a window `window_width` wide: what leaves the
/// content [`MIN_CONTENT_WIDTH`], but never less than the narrowest sidebar.
fn max_width(window_width: Pixels, rem_size: Pixels) -> Pixels {
    let scaled = |length: Pixels| tokens::scaled(length).to_pixels(rem_size);
    (window_width - scaled(MIN_CONTENT_WIDTH)).max(scaled(tokens::RAIL_WIDTH + MIN_TREE_WIDTH))
}

/// A rem at the user's `ui_font_size`, which a window uses only from its first frame on.
fn rem_size(cx: &App) -> Pixels {
    ThemeSettings::get_global(cx).ui_font_size(cx)
}

/// Whether a window `window_width` wide has room for a sidebar and the content next to it.
fn has_room_for_sidebar(window_width: Pixels, rem_size: Pixels) -> bool {
    window_width
        >= tokens::scaled(tokens::RAIL_WIDTH + MIN_TREE_WIDTH + MIN_CONTENT_WIDTH)
            .to_pixels(rem_size)
}

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
    /// Whether the window has room for a sidebar next to a review or a thread, which then show
    /// theirs unless the user hid it.
    roomy: bool,
    /// Whether the user showed or hid the threads next to a thread (the Agents button) and the
    /// tree next to a review (the Pull requests button), until the window's room changes.
    thread_sidebar: Option<bool>,
    review_sidebar: Option<bool>,
    /// What the panel was last sized for.
    sized_for: Option<Sidebar>,
    /// The panel's width with each sidebar, restored when that sidebar shows again.
    file_tree_width: Option<Pixels>,
    threads_width: Option<Pixels>,
    /// The width the panel last chose for itself, which isn't remembered as the user's: a
    /// sidebar sized for a narrow window gets its default again in a wider one.
    chosen_width: Option<Pixels>,
    /// The `ui_font_size` the panel was sized at. Zooming scales its widths by as much as the
    /// font size changes, as it does the rail.
    sized_at_font_size: Pixels,
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
        let zoom = cx.observe_global_in::<SettingsStore>(window, |this, window, cx| {
            this.ui_font_size_changed(cx);
            this.window_resized(window, cx);
        });
        let resize =
            cx.observe_window_bounds(window, |this, window, cx| this.window_resized(window, cx));
        Self {
            focus_handle: cx.focus_handle(),
            workspace: workspace.downgrade(),
            file_tree,
            threads,
            destination: Destination::Other,
            active_rail_entry: None,
            threads_open: false,
            roomy: has_room_for_sidebar(window.viewport_size().width, rem_size(cx)),
            thread_sidebar: None,
            review_sidebar: None,
            sized_for: None,
            file_tree_width: None,
            threads_width: None,
            chosen_width: None,
            sized_at_font_size: ThemeSettings::get_global(cx).ui_font_size(cx),
            _subscriptions: vec![subscription, reveal, zoom, resize],
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
            Destination::Review if self.review_sidebar.unwrap_or(self.roomy) => Sidebar::FileTree,
            Destination::Thread if self.thread_sidebar.unwrap_or(self.roomy) => Sidebar::Threads,
            Destination::Review | Destination::Thread => Sidebar::None,
            _ if self.threads_open => Sidebar::Threads,
            _ => Sidebar::None,
        }
    }

    /// Hides the sidebars of reviews and threads in a window too narrow for them, and shows them
    /// again when it's wide enough. A sidebar wider than the window has room for narrows.
    fn window_resized(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let (window_width, rem_size) = (window.viewport_size().width, rem_size(cx));
        let roomy = has_room_for_sidebar(window_width, rem_size);
        let Some(left_dock) = self
            .workspace
            .read_with(cx, |workspace, _| workspace.left_dock().clone())
            .ok()
        else {
            return;
        };
        if roomy != self.roomy {
            self.roomy = roomy;
            self.thread_sidebar = None;
            self.review_sidebar = None;
            cx.notify();
            self.update_size(&left_dock, window, cx);
        }
        if self.sidebar() == Sidebar::None {
            return;
        }
        // A width the panel chose follows the window; one the user chose only narrows.
        let panel = cx.entity();
        let max_width = max_width(window_width, rem_size);
        let Some(width) = left_dock
            .read(cx)
            .stored_panel_size_state(&panel)
            .and_then(|size_state| size_state.size)
        else {
            return;
        };
        let chosen = Some(width) == self.chosen_width;
        let new_width = if chosen {
            self.default_size(window, cx).min(max_width)
        } else {
            width.min(max_width)
        };
        if new_width != width {
            if chosen {
                self.chosen_width = Some(new_width);
            }
            cx.defer(move |cx| Self::set_width(&panel, &left_dock, new_width, cx));
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
            .and_then(|size_state| size_state.size)
            .filter(|width| Some(*width) != self.chosen_width);
        match self.sized_for {
            Some(Sidebar::FileTree) => self.file_tree_width = current_width,
            Some(Sidebar::Threads) => self.threads_width = current_width,
            Some(Sidebar::None) | None => {}
        }
        let max_width = max_width(window.viewport_size().width, rem_size(cx));
        let width = match sidebar {
            Sidebar::FileTree => self
                .file_tree_width
                .unwrap_or_else(|| self.default_size(window, cx))
                .min(max_width),
            Sidebar::Threads => self
                .threads_width
                .unwrap_or_else(|| self.default_size(window, cx))
                .min(max_width),
            Sidebar::None => tokens::scaled_pixels(tokens::RAIL_WIDTH, cx),
        };
        self.sized_for = Some(sidebar);
        self.chosen_width = Some(width);
        Self::set_width(&panel, left_dock, width, cx);
    }

    fn set_width(panel: &Entity<Self>, left_dock: &Entity<Dock>, width: Pixels, cx: &mut App) {
        left_dock.update(cx, |dock, cx| {
            dock.set_panel_size_state(
                panel,
                PanelSizeState {
                    size: Some(width),
                    flex: None,
                },
                cx,
            );
        });
    }

    /// Zooming scales the panel with the rest of the UI: the rail's width exactly, and the
    /// sidebars' widths, including ones the user dragged, by as much as the font size changed.
    ///
    /// The dock resets a panel to its default size when that changes (`Dock::add_panel` in
    /// `crates/workspace/src/dock.rs`), as [`Panel::default_size`] does here with the font size.
    /// So the new width is set after the dock has seen the change.
    fn ui_font_size_changed(&mut self, cx: &mut Context<Self>) {
        let font_size = ThemeSettings::get_global(cx).ui_font_size(cx);
        if font_size == self.sized_at_font_size {
            return;
        }
        let factor = font_size / self.sized_at_font_size;
        self.sized_at_font_size = font_size;
        self.file_tree_width = self.file_tree_width.map(|width| width * factor);
        self.threads_width = self.threads_width.map(|width| width * factor);
        self.chosen_width = self.chosen_width.map(|width| width * factor);
        let Some(sidebar) = self.sized_for else {
            return;
        };
        let Some(left_dock) = self
            .workspace
            .read_with(cx, |workspace, _| workspace.left_dock().clone())
            .ok()
        else {
            return;
        };
        let panel = cx.entity();
        let width = match sidebar {
            Sidebar::None => tokens::scaled_pixels(tokens::RAIL_WIDTH, cx),
            Sidebar::FileTree | Sidebar::Threads => left_dock
                .read(cx)
                .stored_panel_size_state(&panel)
                .and_then(|size_state| size_state.size)
                .map_or_else(
                    || tokens::scaled_pixels(tokens::RAIL_WIDTH + DEFAULT_TREE_WIDTH, cx),
                    |width| width * factor,
                ),
        };
        cx.defer(move |cx| Self::set_width(&panel, &left_dock, width, cx));
    }

    /// The rail's Agents button: shows or hides the threads next to the rail.
    fn toggle_threads(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let shown = self.sidebar() == Sidebar::Threads;
        if self.destination == Destination::Thread {
            self.thread_sidebar = Some(!shown);
        } else {
            self.threads_open = !self.threads_open;
        }
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
            .w(scaled(tokens::RAIL_WIDTH))
            .h_full()
            .items_center()
            .gap(scaled(px(6.)))
            .py(scaled(px(10.)))
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
                    .tooltip(match (self.destination, self.sidebar()) {
                        (Destination::Review, Sidebar::FileTree) => "Hide the files",
                        (Destination::Review, _) => "Show the files",
                        _ => "Pull requests: the last one you reviewed",
                    })
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
    /// pick one. In that review already, shows or hides its file tree.
    fn show_latest_review(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.destination == Destination::Review {
            self.review_sidebar = Some(self.sidebar() != Sidebar::FileTree);
            cx.notify();
            if let Some(left_dock) = self
                .workspace
                .read_with(cx, |workspace, _| workspace.left_dock().clone())
                .ok()
            {
                self.update_size(&left_dock, window, cx);
            }
            return;
        }
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

    fn default_size(&self, window: &Window, cx: &App) -> Pixels {
        default_width(window.viewport_size().width, rem_size(cx))
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
    use gpui::UpdateGlobal as _;
    use gpui::{TestAppContext, VisualTestContext};
    use serde_json::Value;
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

    fn panel_width(
        workspace: &Entity<Workspace>,
        nav_panel: &Entity<NavPanel>,
        cx: &mut VisualTestContext,
    ) -> Pixels {
        workspace.read_with(cx, |workspace, cx| {
            workspace
                .left_dock()
                .read(cx)
                .stored_panel_size_state(nav_panel)
                .and_then(|size_state| size_state.size)
                .expect("the panel has a width")
        })
    }

    fn set_ui_font_size(size: f32, cx: &mut VisualTestContext) {
        cx.update(|_, cx| {
            SettingsStore::update_global(cx, |store, cx| {
                store.update_user_settings(cx, |settings| {
                    settings.theme.ui_font_size = Some(size.into());
                });
            });
        });
        cx.run_until_parked();
    }

    /// The panel is sized in rems of `ui_font_size`, like what it draws, and a zoom scales it,
    /// including a width the user dragged.
    #[gpui::test]
    async fn the_panel_scales_with_the_ui_font_size(cx: &mut TestAppContext) {
        let (workspace, nav_panel, cx) = workspace_with_nav_panel(cx).await;
        // Wide enough for the widest sidebar at either size.
        cx.simulate_resize(gpui::size(px(1920.), px(1080.)));
        set_ui_font_size(13., cx);
        let left_dock = workspace.read_with(cx, |workspace, _| workspace.left_dock().clone());
        nav_panel.update_in(cx, |nav_panel, window, cx| {
            nav_panel.active_item_changed(None, &left_dock, window, cx)
        });
        assert_eq!(panel_width(&workspace, &nav_panel, cx), px(38.));

        set_ui_font_size(26., cx);
        assert_eq!(panel_width(&workspace, &nav_panel, cx), px(76.));
        nav_panel.update_in(cx, |nav_panel, window, cx| {
            nav_panel.toggle_threads(window, cx)
        });
        assert_eq!(panel_width(&workspace, &nav_panel, cx), px(636.));

        // A width the user chose scales too, and comes back when the threads show again.
        left_dock.update(cx, |dock, cx| {
            dock.set_panel_size_state(
                &nav_panel,
                PanelSizeState {
                    size: Some(px(500.)),
                    flex: None,
                },
                cx,
            )
        });
        set_ui_font_size(13., cx);
        assert_eq!(panel_width(&workspace, &nav_panel, cx), px(250.));
        nav_panel.update_in(cx, |nav_panel, window, cx| {
            nav_panel.toggle_threads(window, cx)
        });
        assert_eq!(panel_width(&workspace, &nav_panel, cx), px(38.));
        set_ui_font_size(26., cx);
        nav_panel.update_in(cx, |nav_panel, window, cx| {
            nav_panel.toggle_threads(window, cx)
        });
        assert_eq!(panel_width(&workspace, &nav_panel, cx), px(500.));
    }

    fn open_thread(
        workspace: &Entity<Workspace>,
        nav_panel: &Entity<NavPanel>,
        cx: &mut VisualTestContext,
    ) {
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
    }

    fn sidebar(nav_panel: &Entity<NavPanel>, cx: &mut VisualTestContext) -> Sidebar {
        nav_panel.read_with(cx, |nav_panel, _| nav_panel.sidebar())
    }

    /// On a MacBook's screen zoomed to twice mustr's default size (26), the sidebar takes a
    /// third of the window; at the default size (13), the window has room for its full width.
    #[gpui::test]
    async fn the_sidebar_takes_a_third_of_a_laptops_window(cx: &mut TestAppContext) {
        let (workspace, nav_panel, cx) = workspace_with_nav_panel(cx).await;
        set_ui_font_size(26., cx);
        cx.simulate_resize(gpui::size(px(1512.), px(982.)));
        cx.run_until_parked();
        open_thread(&workspace, &nav_panel, cx);
        assert_eq!(sidebar(&nav_panel, cx), Sidebar::Threads);
        assert_eq!(panel_width(&workspace, &nav_panel, cx), px(504.));

        // In a narrower window, never below the narrowest sidebar (150 at 13).
        set_ui_font_size(13., cx);
        cx.simulate_resize(gpui::size(px(1470.), px(956.)));
        cx.run_until_parked();
        nav_panel.update_in(cx, |nav_panel, window, cx| {
            // As at startup: no width yet.
            nav_panel.threads_width = None;
            nav_panel.sized_for = None;
            nav_panel.update_size(&workspace.read(cx).left_dock().clone(), window, cx);
        });
        assert_eq!(panel_width(&workspace, &nav_panel, cx), px(318.));
    }

    /// A half-screen window at 26 has no room for the threads next to a thread, so the thread
    /// shows with the rail alone, and the Agents button shows the threads, as narrow as they get.
    /// A wider window brings them back by themselves.
    #[gpui::test]
    async fn narrow_windows_show_threads_without_the_sidebar(cx: &mut TestAppContext) {
        let (workspace, nav_panel, cx) = workspace_with_nav_panel(cx).await;
        set_ui_font_size(26., cx);
        cx.simulate_resize(gpui::size(px(756.), px(982.)));
        cx.run_until_parked();
        open_thread(&workspace, &nav_panel, cx);
        assert_eq!(sidebar(&nav_panel, cx), Sidebar::None);
        assert_eq!(panel_width(&workspace, &nav_panel, cx), px(76.));

        nav_panel.update_in(cx, |nav_panel, window, cx| {
            nav_panel.toggle_threads(window, cx)
        });
        assert_eq!(sidebar(&nav_panel, cx), Sidebar::Threads);
        assert_eq!(panel_width(&workspace, &nav_panel, cx), px(376.));
        nav_panel.update_in(cx, |nav_panel, window, cx| {
            nav_panel.toggle_threads(window, cx)
        });
        assert_eq!(sidebar(&nav_panel, cx), Sidebar::None);

        cx.simulate_resize(gpui::size(px(1512.), px(982.)));
        cx.run_until_parked();
        assert_eq!(sidebar(&nav_panel, cx), Sidebar::Threads);
        assert_eq!(panel_width(&workspace, &nav_panel, cx), px(504.));

        // In a wide window, the Agents button hides them too.
        nav_panel.update_in(cx, |nav_panel, window, cx| {
            nav_panel.toggle_threads(window, cx)
        });
        assert_eq!(sidebar(&nav_panel, cx), Sidebar::None);
    }

    /// A sidebar the window no longer has room for narrows, so the content keeps its room.
    #[gpui::test]
    async fn a_narrower_window_narrows_the_sidebar(cx: &mut TestAppContext) {
        let (workspace, nav_panel, cx) = workspace_with_nav_panel(cx).await;
        set_ui_font_size(26., cx);
        cx.simulate_resize(gpui::size(px(1512.), px(982.)));
        cx.run_until_parked();
        nav_panel.update_in(cx, |nav_panel, window, cx| {
            nav_panel.toggle_threads(window, cx)
        });
        assert_eq!(panel_width(&workspace, &nav_panel, cx), px(504.));

        // Home's threads stay, as narrow as they get.
        cx.simulate_resize(gpui::size(px(1000.), px(982.)));
        cx.run_until_parked();
        assert_eq!(sidebar(&nav_panel, cx), Sidebar::Threads);
        assert_eq!(panel_width(&workspace, &nav_panel, cx), px(376.));

        // A wider window widens them again.
        cx.simulate_resize(gpui::size(px(1512.), px(982.)));
        cx.run_until_parked();
        assert_eq!(panel_width(&workspace, &nav_panel, cx), px(504.));

        // A width the user dragged only narrows.
        let left_dock = workspace.read_with(cx, |workspace, _| workspace.left_dock().clone());
        left_dock.update(cx, |dock, cx| {
            dock.set_panel_size_state(
                &nav_panel,
                PanelSizeState {
                    size: Some(px(600.)),
                    flex: None,
                },
                cx,
            )
        });
        cx.simulate_resize(gpui::size(px(1200.), px(982.)));
        cx.run_until_parked();
        assert_eq!(panel_width(&workspace, &nav_panel, cx), px(480.));
        cx.simulate_resize(gpui::size(px(1512.), px(982.)));
        cx.run_until_parked();
        assert_eq!(panel_width(&workspace, &nav_panel, cx), px(480.));
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

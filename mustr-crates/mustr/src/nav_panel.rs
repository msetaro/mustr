//! The left dock's panel: mustr's icon rail, and next to it the sidebar of the area on screen
//! (`areas.rs`): Edit's file tree, a review's files, or the agent threads.
//!
//! The rail has four buttons, one per area, then Settings at the bottom: Inbox, Edit, Review,
//! and Agents (⌘1 to ⌘4). A button shows its area as you left it; clicked again, Inbox goes back
//! to the Inbox's page, Edit shows or hides its file tree, Review goes back to the Review page
//! from a review (or lists again on it), and Agents shows or hides the threads.
//!
//! A dock shows one panel at a time, so the rail and the sidebars share this panel
//! (`mustr-docs/ZED-INTEGRATION.md` Q4). The sidebar takes at most a third of the window by
//! default, and a window too narrow for it and the content next to it (a half-screen window at
//! twice mustr's default size) shows reviews and threads with the rail alone. Agents' sidebar
//! ends with links to the agent pages: scheduled tasks, worktrees, usage, and agents setup.

use crate::{
    agent_setup::OpenAgents,
    areas::{self, Area, Areas, ViewKind},
    edit_area::{EditFolder, EditTree},
    review_page::ReviewPage,
    schedules::OpenScheduledTasks,
    usage_page::OpenUsage,
    worktrees::ShowWorktrees,
};
use gpui::{
    Action, AnyElement, App, AppContext as _, Context, Entity, EventEmitter, FocusHandle,
    Focusable, InteractiveElement as _, IntoElement, ParentElement as _, Pixels, Render,
    SharedString, StatefulInteractiveElement as _, Styled as _, Subscription, WeakEntity, Window,
    div, px,
};
use mustr_ui::{MustrIcon, RailButton, tokens, tokens::scaled};
use review::{FileTreePanel, ToggleFileTree};
use settings::{Settings as _, SettingsStore};
use theme_settings::ThemeSettings;
use thread_chat::{ThreadView, ThreadsSidebar, ThreadsSidebarEvent};
use ui::{
    ButtonCommon as _, Clickable as _, Color, FluentBuilder as _, IconButton, IconName, IconSize,
    Toggleable as _, Tooltip, h_flex, v_flex,
};
use workspace::{
    Workspace,
    dock::{Dock, DockPosition, Panel, PanelEvent, PanelSizeState},
};

/// The file trees' width, next to the rail, at mustr's design size (`scaled`), in a window wide
/// enough for it.
const DEFAULT_TREE_WIDTH: Pixels = px(280.);

/// The threads' width, at mustr's design size: about T3's sidebar, a fifth of a laptop's window.
const DEFAULT_THREADS_WIDTH: Pixels = px(320.);

/// The narrowest the sidebars get by default, at mustr's design size.
const MIN_TREE_WIDTH: Pixels = px(150.);

/// The share of the window the panel takes by default, with a sidebar.
const SIDEBAR_SHARE: f32 = 1. / 3.;

/// The room the content next to the panel keeps, at mustr's design size. A window too narrow for
/// the rail, the narrowest sidebar, and this shows reviews and threads without their sidebar.
const MIN_CONTENT_WIDTH: Pixels = px(360.);

/// The panel's width with a sidebar `sidebar_width` wide (at mustr's design size), by default, in
/// a window `window_width` wide, at `rem_size`: at most a third of the window, and never below
/// the narrowest sidebar.
fn default_width(window_width: Pixels, sidebar_width: Pixels, rem_size: Pixels) -> Pixels {
    let scaled = |length: Pixels| tokens::scaled(length).to_pixels(rem_size);
    let rail = scaled(tokens::RAIL_WIDTH);
    let sidebar = (window_width * SIDEBAR_SHARE - rail)
        .min(scaled(sidebar_width))
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

/// What shows next to the rail.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Sidebar {
    None,
    FileTree,
    Threads,
    EditTree,
}

impl Sidebar {
    fn default_width(self) -> Pixels {
        match self {
            Sidebar::Threads => DEFAULT_THREADS_WIDTH,
            _ => DEFAULT_TREE_WIDTH,
        }
    }
}

pub struct NavPanel {
    focus_handle: FocusHandle,
    workspace: WeakEntity<Workspace>,
    areas: Entity<Areas>,
    file_tree: Entity<FileTreePanel>,
    threads: Entity<ThreadsSidebar>,
    edit_tree: Entity<EditTree>,
    /// The area and the kind of view on screen.
    area: Area,
    kind: Option<ViewKind>,
    /// Whether the window has room for a sidebar next to a review or a thread, which then show
    /// theirs unless the user hid it.
    roomy: bool,
    /// Whether the user showed or hid the threads (the Agents button), a review's files (the
    /// file tree's toggle), and Edit's files (the Edit button), until the window's room changes.
    agents_sidebar: Option<bool>,
    review_sidebar: Option<bool>,
    edit_sidebar: Option<bool>,
    /// What the panel was last sized for.
    sized_for: Option<Sidebar>,
    /// The panel's width with each sidebar, restored when that sidebar shows again.
    file_tree_width: Option<Pixels>,
    threads_width: Option<Pixels>,
    edit_tree_width: Option<Pixels>,
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
    pub fn new(
        workspace: &Entity<Workspace>,
        areas: Entity<Areas>,
        edit_folder: Entity<EditFolder>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        let file_tree = cx.new(|cx| FileTreePanel::new(workspace, window, cx));
        let threads = cx.new(|cx| ThreadsSidebar::new(workspace.downgrade(), window, cx));
        let edit_tree = cx.new(|cx| EditTree::new(workspace, edit_folder, window, cx));
        let reveal = cx.subscribe_in(
            &threads,
            window,
            |this, _, event: &ThreadsSidebarEvent, window, cx| match event {
                ThreadsSidebarEvent::Reveal => this.reveal_threads(window, cx),
            },
        );
        let areas_changed = cx.observe_in(&areas, window, |this, _, window, cx| {
            this.areas_changed(window, cx)
        });
        let zoom = cx.observe_global_in::<SettingsStore>(window, |this, window, cx| {
            this.ui_font_size_changed(cx);
            this.window_resized(window, cx);
        });
        let resize =
            cx.observe_window_bounds(window, |this, window, cx| this.window_resized(window, cx));
        Self {
            focus_handle: cx.focus_handle(),
            workspace: workspace.downgrade(),
            areas,
            file_tree,
            threads,
            edit_tree,
            area: Area::Inbox,
            kind: None,
            roomy: has_room_for_sidebar(window.viewport_size().width, rem_size(cx)),
            agents_sidebar: None,
            review_sidebar: None,
            edit_sidebar: None,
            sized_for: None,
            file_tree_width: None,
            threads_width: None,
            edit_tree_width: None,
            chosen_width: None,
            sized_at_font_size: ThemeSettings::get_global(cx).ui_font_size(cx),
            _subscriptions: vec![areas_changed, reveal, zoom, resize],
        }
    }

    /// Handles the rail's actions (⌘1 to ⌘4) on a window's workspace.
    pub fn register(workspace: &mut Workspace, nav_panel: &Entity<Self>) {
        fn on<A: Action>(workspace: &mut Workspace, nav_panel: &Entity<NavPanel>, area: Area) {
            let nav_panel = nav_panel.downgrade();
            workspace.register_action(move |_, _: &A, window, cx| {
                let nav_panel = nav_panel.clone();
                // The rail updates the workspace, which is busy handling this action.
                window.defer(cx, move |window, cx| {
                    nav_panel
                        .update(cx, |nav_panel, cx| nav_panel.rail_clicked(area, window, cx))
                        .ok();
                });
            });
        }
        on::<areas::ShowInbox>(workspace, nav_panel, Area::Inbox);
        on::<areas::ShowEdit>(workspace, nav_panel, Area::Edit);
        on::<areas::ShowReview>(workspace, nav_panel, Area::Review);
        on::<areas::ShowAgents>(workspace, nav_panel, Area::Agents);
    }

    /// Sizes the panel for the area on screen, once it's in `left_dock`. For the window's setup,
    /// which can't let the panel read the workspace it's setting up.
    pub fn added_to_dock(
        &mut self,
        left_dock: &Entity<Dock>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.update_size(left_dock, window, cx);
    }

    /// Follows the area on screen: highlights its rail button and shows its sidebar (widening
    /// the panel).
    pub fn areas_changed(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let areas = self.areas.read(cx);
        let (area, kind) = (areas.area(), areas.kind());
        let thread_id = areas
            .shown()
            .and_then(|item| item.downcast::<ThreadView>())
            .and_then(|thread| thread.read(cx).thread_id().map(str::to_string));
        self.threads
            .update(cx, |threads, cx| threads.set_active_thread(thread_id, cx));
        if area == Area::Edit && self.area != Area::Edit {
            self.edit_tree.update(cx, |tree, cx| tree.shown(cx));
        }
        self.area = area;
        self.kind = kind;
        cx.notify();
        if let Some(left_dock) = self.left_dock(cx) {
            self.update_size(&left_dock, window, cx);
        }
    }

    fn left_dock(&self, cx: &App) -> Option<Entity<Dock>> {
        self.workspace
            .read_with(cx, |workspace, _| workspace.left_dock().clone())
            .ok()
    }

    fn sidebar(&self) -> Sidebar {
        match (self.area, self.kind) {
            (Area::Edit, _) if self.edit_sidebar.unwrap_or(true) => Sidebar::EditTree,
            (Area::Review, Some(ViewKind::Review)) if self.review_sidebar.unwrap_or(self.roomy) => {
                Sidebar::FileTree
            }
            // With nothing picked, the list is what there is to pick from.
            (Area::Agents, Some(ViewKind::AgentsHome) | None)
                if self.agents_sidebar.unwrap_or(true) =>
            {
                Sidebar::Threads
            }
            (Area::Agents, _) if self.agents_sidebar.unwrap_or(self.roomy) => Sidebar::Threads,
            _ => Sidebar::None,
        }
    }

    /// Hides the sidebars of reviews and threads in a window too narrow for them, and shows them
    /// again when it's wide enough. A sidebar wider than the window has room for narrows.
    fn window_resized(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let (window_width, rem_size) = (window.viewport_size().width, rem_size(cx));
        let roomy = has_room_for_sidebar(window_width, rem_size);
        let Some(left_dock) = self.left_dock(cx) else {
            return;
        };
        if roomy != self.roomy {
            self.roomy = roomy;
            self.agents_sidebar = None;
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
            self.preferred_width(window, cx).min(max_width)
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
        // Not in the dock yet: the window's setup sizes it once it is (`window.rs`).
        if left_dock.read(cx).panel::<Self>().is_none() {
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
            Some(Sidebar::EditTree) => self.edit_tree_width = current_width,
            Some(Sidebar::None) | None => {}
        }
        let max_width = max_width(window.viewport_size().width, rem_size(cx));
        let remembered = match sidebar {
            Sidebar::FileTree => self.file_tree_width,
            Sidebar::Threads => self.threads_width,
            Sidebar::EditTree => self.edit_tree_width,
            Sidebar::None => None,
        };
        let width = match sidebar {
            Sidebar::None => tokens::scaled_pixels(tokens::RAIL_WIDTH, cx),
            _ => remembered
                .unwrap_or_else(|| self.preferred_width(window, cx))
                .min(max_width),
        };
        self.sized_for = Some(sidebar);
        self.chosen_width = Some(width);
        Self::set_width(&panel, left_dock, width, cx);
    }

    /// The panel's width with its sidebar, by default: a third of the window at most.
    fn preferred_width(&self, window: &Window, cx: &App) -> Pixels {
        default_width(
            window.viewport_size().width,
            self.sidebar().default_width(),
            rem_size(cx),
        )
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
        self.edit_tree_width = self.edit_tree_width.map(|width| width * factor);
        self.chosen_width = self.chosen_width.map(|width| width * factor);
        let Some(sidebar) = self.sized_for else {
            return;
        };
        let Some(left_dock) = self.left_dock(cx) else {
            return;
        };
        let panel = cx.entity();
        let width = match sidebar {
            Sidebar::None => tokens::scaled_pixels(tokens::RAIL_WIDTH, cx),
            _ => left_dock
                .read(cx)
                .stored_panel_size_state(&panel)
                .and_then(|size_state| size_state.size)
                .map_or_else(
                    || tokens::scaled_pixels(tokens::RAIL_WIDTH + sidebar.default_width(), cx),
                    |width| width * factor,
                ),
        };
        cx.defer(move |cx| Self::set_width(&panel, &left_dock, width, cx));
    }

    fn resize(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        cx.notify();
        if let Some(left_dock) = self.left_dock(cx) {
            self.update_size(&left_dock, window, cx);
        }
    }

    /// Shows or hides Agents' threads.
    fn toggle_threads(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.agents_sidebar = Some(self.sidebar() != Sidebar::Threads);
        self.resize(window, cx);
    }

    /// Shows or hides a review's files (`review::ToggleFileTree`, the review's own toggle).
    pub fn toggle_review_files(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.review_sidebar = Some(self.sidebar() != Sidebar::FileTree);
        self.resize(window, cx);
    }

    /// Shows the threads next to the rail, opening the dock if it was closed, for the threads'
    /// search: in Agents.
    fn reveal_threads(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.area != Area::Agents {
            self.with_workspace(window, cx, |workspace, window, cx| {
                areas::show_area(Area::Agents, workspace, window, cx)
            });
        }
        self.agents_sidebar = Some(true);
        self.resize(window, cx);
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

    /// A rail button (or its shortcut): shows its area, or, in that area already, does the
    /// button's second job.
    pub fn rail_clicked(&mut self, area: Area, window: &mut Window, cx: &mut Context<Self>) {
        if area != self.area {
            self.with_workspace(window, cx, |workspace, window, cx| {
                areas::show_area(area, workspace, window, cx)
            });
            return;
        }
        match area {
            Area::Inbox | Area::Settings => {
                self.with_workspace(window, cx, |workspace, window, cx| {
                    areas::show_root(area, workspace, window, cx)
                })
            }
            Area::Edit => {
                self.edit_sidebar = Some(self.sidebar() != Sidebar::EditTree);
                self.resize(window, cx);
            }
            Area::Review => match self.kind {
                // On the page: list again.
                Some(ViewKind::ReviewPage) => {
                    let page = self
                        .areas
                        .read(cx)
                        .shown()
                        .and_then(|item| item.downcast::<ReviewPage>());
                    if let Some(page) = page {
                        page.update(cx, |page, cx| page.reload(window, cx));
                    }
                }
                // In a review: back to the page.
                _ => self.with_workspace(window, cx, |workspace, window, cx| {
                    areas::show_root(Area::Review, workspace, window, cx)
                }),
            },
            Area::Agents => self.toggle_threads(window, cx),
        }
    }

    fn rail_tooltip(&self, area: Area) -> &'static str {
        match (area, self.area == area, self.sidebar()) {
            (Area::Inbox, _, _) => "Inbox: what needs you",
            (Area::Edit, true, Sidebar::EditTree) => "Edit: hide the files",
            (Area::Edit, true, _) => "Edit: show the files",
            (Area::Edit, false, _) => "Edit: the editor",
            (Area::Review, true, _) if self.kind == Some(ViewKind::ReviewPage) => {
                "Review: list again"
            }
            (Area::Review, true, _) => "Review: back to the list",
            (Area::Review, false, _) => "Review: pull requests and agent work",
            (Area::Agents, true, Sidebar::Threads) => "Agents: hide the threads",
            (Area::Agents, true, _) => "Agents: show the threads",
            (Area::Agents, false, _) => "Agents: the agent threads",
            (Area::Settings, _, _) => "Settings",
        }
    }

    fn render_rail(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let palette = mustr_ui::palette(cx);
        let this = cx.entity().downgrade();
        let button = |area: Area, icon: MustrIcon, action: Box<dyn Action>| {
            let this = this.clone();
            let id = match area {
                Area::Inbox => "rail-inbox",
                Area::Edit => "rail-edit",
                Area::Review => "rail-review",
                Area::Agents => "rail-agents",
                Area::Settings => "rail-settings",
            };
            // The tooltip names the button and its shortcut.
            div()
                .id(SharedString::from(format!("{id}-slot")))
                .debug_selector(|| id.into())
                .tooltip(Tooltip::for_action_title(
                    self.rail_tooltip(area),
                    action.as_ref(),
                ))
                .child(
                    RailButton::new(id, icon.icon())
                        .active(self.area == area)
                        .on_click(move |_, window, cx| {
                            this.update(cx, |this, cx| this.rail_clicked(area, window, cx))
                                .ok();
                        }),
                )
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
            .child(button(
                Area::Inbox,
                MustrIcon::Inbox,
                Box::new(areas::ShowInbox),
            ))
            .child(button(
                Area::Edit,
                MustrIcon::Edit,
                Box::new(areas::ShowEdit),
            ))
            .child(button(
                Area::Review,
                MustrIcon::PullRequest,
                Box::new(areas::ShowReview),
            ))
            .child(button(
                Area::Agents,
                MustrIcon::Agent,
                Box::new(areas::ShowAgents),
            ))
            .child(div().flex_1())
            .child(
                div()
                    .id("rail-settings-slot")
                    .debug_selector(|| "rail-settings".into())
                    .tooltip(Tooltip::for_action_title(
                        "Settings",
                        &crate::window::OpenSettings,
                    ))
                    .child(
                        RailButton::new("rail-settings", MustrIcon::Settings.icon())
                            .active(self.area == Area::Settings)
                            .on_click(|_, window, cx| {
                                window
                                    .dispatch_action(crate::window::OpenSettings.boxed_clone(), cx)
                            }),
                    ),
            )
    }

    /// Under the threads: the agent pages that used to have rail buttons.
    fn render_agent_links(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let palette = mustr_ui::palette(cx);
        let kind_shown = |page: fn(&dyn workspace::ItemHandle) -> bool| {
            self.areas.read(cx).shown().is_some_and(page)
        };
        let link = |id: &'static str,
                    icon: IconName,
                    title: &'static str,
                    action: Box<dyn Action>,
                    active: bool| {
            let tooltip = action.boxed_clone();
            div().debug_selector(|| id.into()).child(
                IconButton::new(id, icon)
                    .icon_size(IconSize::Small)
                    .icon_color(Color::Muted)
                    .toggle_state(active)
                    .tooltip(Tooltip::for_action_title(title, tooltip.as_ref()))
                    .on_click(move |_, window, cx| {
                        window.dispatch_action(action.boxed_clone(), cx)
                    }),
            )
        };
        h_flex()
            .flex_none()
            .w_full()
            .px_2()
            .py(scaled(px(4.)))
            .gap_1()
            .bg(palette.surface.hsla())
            .border_t(tokens::BORDER_WIDTH)
            .border_r(tokens::BORDER_WIDTH)
            .border_color(palette.border.hsla())
            .child(link(
                "agents-link-schedules",
                IconName::Clock,
                "Scheduled tasks",
                Box::new(OpenScheduledTasks),
                kind_shown(|item| item.downcast::<crate::schedules::SchedulesView>().is_some()),
            ))
            .child(link(
                "agents-link-worktrees",
                IconName::GitWorktree,
                "Worktrees",
                Box::new(ShowWorktrees),
                kind_shown(|item| item.downcast::<crate::worktrees::WorktreesView>().is_some()),
            ))
            .child(link(
                "agents-link-usage",
                IconName::SignalHigh,
                "Usage",
                Box::new(OpenUsage),
                kind_shown(|item| item.downcast::<crate::usage_page::UsageView>().is_some()),
            ))
            .child(link(
                "agents-link-setup",
                IconName::Blocks,
                "Agents setup: install and sign in",
                Box::new(OpenAgents),
                kind_shown(|item| {
                    item.downcast::<crate::agent_setup::AgentSetupView>()
                        .is_some()
                }),
            ))
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
            Sidebar::EditTree => self.edit_tree.focus_handle(cx),
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

    /// What the dock falls back to: the rail and a file tree, at the font size alone. Zed's dock
    /// resets a panel to its default size whenever that changes between two settings changes
    /// (`Dock::add_panel` in `crates/workspace/src/dock.rs`), so it mustn't follow the window's
    /// width or the sidebar showing: then any save (a setting, a Review section) after maximizing
    /// the window, or after leaving Agents, widened the rail with an empty strip. The panel sets
    /// its width itself ([`Self::preferred_width`]); a zoom still resets it, and
    /// [`Self::ui_font_size_changed`] scales it after.
    fn default_size(&self, _: &Window, cx: &App) -> Pixels {
        tokens::scaled_pixels(tokens::RAIL_WIDTH + DEFAULT_TREE_WIDTH, cx)
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
        let sidebar = |child: AnyElement| div().flex_1().min_w_0().h_full().child(child);
        h_flex()
            .key_context("MustrNavigation")
            .track_focus(&self.focus_handle)
            .size_full()
            .child(self.render_rail(cx))
            .map(|this| match self.sidebar() {
                Sidebar::FileTree => this.child(sidebar(self.file_tree.clone().into_any_element())),
                Sidebar::EditTree => this.child(sidebar(self.edit_tree.clone().into_any_element())),
                Sidebar::Threads => this.child(sidebar(
                    v_flex()
                        .size_full()
                        .child(div().flex_1().min_h_0().child(self.threads.clone()))
                        .child(self.render_agent_links(cx))
                        .into_any_element(),
                )),
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
        workspace_with_files(cx, serde_json::json!({})).await
    }

    /// A window as mustr opens one, with files in a fake file system for Edit.
    async fn workspace_with_files(
        cx: &mut TestAppContext,
        files: Value,
    ) -> (Entity<Workspace>, Entity<NavPanel>, &mut VisualTestContext) {
        cx.update(|cx| {
            let settings_store = SettingsStore::test(cx);
            cx.set_global(settings_store);
            theme_settings::init(theme::LoadThemes::JustBase, cx);
            editor::init(cx);
            thread_chat::set_backend(Arc::new(UnreachableBackend), cx);
            crate::t3_connection::init_for_test(crate::t3_engine::T3Mode::Installed, cx);
            crate::keymap::load_default_keymap(cx);
        });
        let fs = project::FakeFs::new(cx.executor());
        fs.insert_tree("/", files).await;
        let project = project::Project::test(fs, [], cx).await;
        let (multi_workspace, cx) =
            cx.add_window_view(|window, cx| MultiWorkspace::test_new(project.clone(), window, cx));
        let workspace =
            multi_workspace.read_with(cx, |multi_workspace, _| multi_workspace.workspace().clone());
        let nav_panel = workspace.update_in(cx, |workspace, window, cx| {
            let workspace_handle = cx.entity();
            let areas = crate::areas::ensure(workspace, window, cx);
            crate::edit_area::register(workspace, cx);
            let edit_folder = crate::edit_area::EditFolder::ensure(workspace, cx);
            let nav_panel =
                cx.new(|cx| NavPanel::new(&workspace_handle, areas, edit_folder, window, cx));
            NavPanel::register(workspace, &nav_panel);
            workspace.add_panel(nav_panel.clone(), window, cx);
            nav_panel
        });
        cx.run_until_parked();
        (workspace, nav_panel, cx)
    }

    /// The area and the kind of view on screen.
    fn shown(nav_panel: &Entity<NavPanel>, cx: &mut VisualTestContext) -> (Area, Option<ViewKind>) {
        nav_panel.read_with(cx, |nav_panel, cx| {
            let areas = nav_panel.areas.read(cx);
            (areas.area(), areas.kind())
        })
    }

    fn click(nav_panel: &Entity<NavPanel>, area: Area, cx: &mut VisualTestContext) {
        nav_panel.update_in(cx, |nav_panel, window, cx| {
            nav_panel.rail_clicked(area, window, cx)
        });
        cx.run_until_parked();
    }

    fn show_inbox(workspace: &Entity<Workspace>, cx: &mut VisualTestContext) {
        workspace.update_in(cx, |workspace, window, cx| {
            crate::navigation::show_home(workspace, window, cx)
        });
        cx.run_until_parked();
    }

    fn active_item_id(
        workspace: &Entity<Workspace>,
        cx: &mut VisualTestContext,
    ) -> Option<gpui::EntityId> {
        workspace.read_with(cx, |workspace, cx| {
            workspace.active_item(cx).map(|item| item.item_id())
        })
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
            assert_eq!(nav_panel.area, Area::Agents);
            assert_eq!(nav_panel.kind, Some(ViewKind::Thread));
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

    /// Writes a setting other than the size, as the Settings page and the Review page's sections
    /// do.
    fn write_another_setting(value: f32, cx: &mut VisualTestContext) {
        cx.update(|_, cx| {
            SettingsStore::update_global(cx, |store, cx| {
                store.update_user_settings(cx, |settings| {
                    settings.theme.buffer_font_size = Some(value.into());
                });
            });
        });
        cx.run_until_parked();
    }

    /// Writing a setting keeps the panel's width. Zed's dock resets a panel to its default size
    /// whenever that differs from what it was at the last settings change, and the panel's
    /// default followed the window's width and the sidebar showing, so a save after maximizing
    /// the window, or after leaving Agents, widened the rail with an empty strip.
    #[gpui::test]
    async fn writing_a_setting_keeps_the_panels_width(cx: &mut TestAppContext) {
        let (workspace, nav_panel, cx) = workspace_with_nav_panel(cx).await;
        set_ui_font_size(26., cx);
        cx.simulate_resize(gpui::size(px(1200.), px(800.)));
        cx.run_until_parked();
        show_inbox(&workspace, cx);
        click(&nav_panel, Area::Review, cx);
        assert_eq!(panel_width(&workspace, &nav_panel, cx), px(76.));

        // Maximized, then a save.
        cx.simulate_resize(gpui::size(px(1512.), px(982.)));
        cx.run_until_parked();
        write_another_setting(14., cx);
        assert_eq!(panel_width(&workspace, &nav_panel, cx), px(76.));

        // At 13: a save in Agents, then one on the Review page.
        set_ui_font_size(13., cx);
        click(&nav_panel, Area::Agents, cx);
        write_another_setting(15., cx);
        assert_eq!(panel_width(&workspace, &nav_panel, cx), px(38. + 320.));
        click(&nav_panel, Area::Review, cx);
        write_another_setting(16., cx);
        assert_eq!(panel_width(&workspace, &nav_panel, cx), px(38.));

        // A width the user dragged stays too.
        click(&nav_panel, Area::Agents, cx);
        let left_dock = workspace.read_with(cx, |workspace, _| workspace.left_dock().clone());
        left_dock.update(cx, |dock, cx| {
            dock.set_panel_size_state(
                &nav_panel,
                PanelSizeState {
                    size: Some(px(420.)),
                    flex: None,
                },
                cx,
            )
        });
        cx.simulate_resize(gpui::size(px(1400.), px(900.)));
        cx.run_until_parked();
        write_another_setting(17., cx);
        assert_eq!(panel_width(&workspace, &nav_panel, cx), px(420.));
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
        show_inbox(&workspace, cx);
        assert_eq!(panel_width(&workspace, &nav_panel, cx), px(38.));

        set_ui_font_size(26., cx);
        assert_eq!(panel_width(&workspace, &nav_panel, cx), px(76.));
        // Agents' threads: as wide as T3's sidebar, within a third of the window.
        click(&nav_panel, Area::Agents, cx);
        assert_eq!(panel_width(&workspace, &nav_panel, cx), px(640.));

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
        click(&nav_panel, Area::Agents, cx);
        assert_eq!(panel_width(&workspace, &nav_panel, cx), px(38.));
        set_ui_font_size(26., cx);
        click(&nav_panel, Area::Agents, cx);
        assert_eq!(panel_width(&workspace, &nav_panel, cx), px(500.));

        // At mustr's default size (13), the threads are T3's sidebar's width.
        set_ui_font_size(13., cx);
        nav_panel.update_in(cx, |nav_panel, window, cx| {
            nav_panel.threads_width = None;
            nav_panel.sized_for = None;
            nav_panel.update_size(&workspace.read(cx).left_dock().clone(), window, cx);
        });
        assert_eq!(panel_width(&workspace, &nav_panel, cx), px(38. + 320.));
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
        assert_eq!(panel_width(&workspace, &nav_panel, cx), px(358.));
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
        click(&nav_panel, Area::Agents, cx);
        assert_eq!(panel_width(&workspace, &nav_panel, cx), px(504.));

        // With no thread picked, the threads stay, as narrow as they get.
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
        let (workspace, nav_panel, cx) = workspace_with_nav_panel(cx).await;
        show_inbox(&workspace, cx);
        click(&nav_panel, Area::Agents, cx);
        assert_eq!(sidebar(&nav_panel, cx), Sidebar::Threads);
        click(&nav_panel, Area::Agents, cx);
        assert_eq!(sidebar(&nav_panel, cx), Sidebar::None);
        click(&nav_panel, Area::Agents, cx);
        assert_eq!(sidebar(&nav_panel, cx), Sidebar::Threads);
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

    /// The rail's gear opens mustr's Settings page (`mustr::OpenSettings`), never a file.
    #[gpui::test]
    async fn the_gear_opens_the_settings_page(cx: &mut TestAppContext) {
        let (_, _, cx) = workspace_with_nav_panel(cx).await;
        let opened = Rc::new(Cell::new(0));
        cx.update(|_, cx| {
            let opened = opened.clone();
            cx.on_action(move |_: &crate::window::OpenSettings, _| opened.set(opened.get() + 1));
        });
        cx.simulate_resize(gpui::size(px(1000.), px(800.)));
        cx.run_until_parked();
        let gear = cx
            .debug_bounds("rail-settings")
            .expect("the gear is on the rail");
        cx.simulate_click(gear.center(), gpui::Modifiers::none());
        cx.run_until_parked();
        assert_eq!(opened.get(), 1);
    }

    /// The rail is the four areas' buttons, top to bottom, then Settings at the bottom, and
    /// nothing else: the pages that had their own buttons are under Agents' threads.
    #[gpui::test]
    async fn the_rail_has_four_buttons_in_order(cx: &mut TestAppContext) {
        let (workspace, nav_panel, cx) = workspace_with_nav_panel(cx).await;
        show_inbox(&workspace, cx);
        cx.simulate_resize(gpui::size(px(1000.), px(800.)));
        cx.run_until_parked();
        let tops = [
            "rail-inbox",
            "rail-edit",
            "rail-review",
            "rail-agents",
            "rail-settings",
        ]
        .map(|id| cx.debug_bounds(id).expect(id).origin.y);
        assert!(tops.windows(2).all(|pair| pair[0] < pair[1]), "{tops:?}");
        for gone in [
            "rail-entry-schedules",
            "rail-entry-worktrees",
            "rail-entry-usage",
            "rail-entry-agent-setup",
            "rail-pull-requests",
        ] {
            assert!(cx.debug_bounds(gone).is_none(), "{gone} is on the rail");
        }
        // Agents' sidebar links to them.
        click(&nav_panel, Area::Agents, cx);
        for link in [
            "agents-link-schedules",
            "agents-link-worktrees",
            "agents-link-usage",
            "agents-link-setup",
        ] {
            assert!(
                cx.debug_bounds(link).is_some(),
                "{link} isn't under the threads"
            );
        }
    }

    /// Each button shows its area from every other area, with the area's own page when nothing
    /// was shown there yet, and the button and the area's sidebar follow.
    #[gpui::test]
    async fn each_button_shows_its_area_from_every_area(cx: &mut TestAppContext) {
        let (workspace, nav_panel, cx) = workspace_with_nav_panel(cx).await;
        cx.simulate_resize(gpui::size(px(1512.), px(982.)));
        show_inbox(&workspace, cx);
        assert_eq!(shown(&nav_panel, cx), (Area::Inbox, Some(ViewKind::Inbox)));
        let areas = [
            (Area::Inbox, ViewKind::Inbox, Sidebar::None),
            (Area::Edit, ViewKind::EditHome, Sidebar::EditTree),
            (Area::Review, ViewKind::ReviewPage, Sidebar::None),
            (Area::Agents, ViewKind::AgentsHome, Sidebar::Threads),
        ];
        for (from, from_kind, _) in areas {
            for (to, kind, expected_sidebar) in areas {
                if from == to {
                    continue;
                }
                click(&nav_panel, from, cx);
                assert_eq!(shown(&nav_panel, cx), (from, Some(from_kind)));
                click(&nav_panel, to, cx);
                assert_eq!(
                    shown(&nav_panel, cx),
                    (to, Some(kind)),
                    "{from:?} to {to:?}"
                );
                assert_eq!(sidebar(&nav_panel, cx), expected_sidebar, "{to:?}");
            }
        }
        // Each area's page was made once.
        let items = workspace.read_with(cx, |workspace, cx| {
            workspace.active_pane().read(cx).items_len()
        });
        assert_eq!(items, 4);
    }

    /// In its area already, Edit and Agents show or hide their sidebar, and Inbox goes to the
    /// Inbox's page.
    #[gpui::test]
    async fn the_area_on_screen_does_its_buttons_second_job(cx: &mut TestAppContext) {
        let (workspace, nav_panel, cx) = workspace_with_nav_panel(cx).await;
        cx.simulate_resize(gpui::size(px(1512.), px(982.)));
        show_inbox(&workspace, cx);
        click(&nav_panel, Area::Edit, cx);
        assert_eq!(sidebar(&nav_panel, cx), Sidebar::EditTree);
        click(&nav_panel, Area::Edit, cx);
        assert_eq!(sidebar(&nav_panel, cx), Sidebar::None);
        assert_eq!(shown(&nav_panel, cx).0, Area::Edit);
        click(&nav_panel, Area::Edit, cx);
        assert_eq!(sidebar(&nav_panel, cx), Sidebar::EditTree);

        open_thread(&workspace, &nav_panel, cx);
        assert_eq!(
            shown(&nav_panel, cx),
            (Area::Agents, Some(ViewKind::Thread))
        );
        assert_eq!(sidebar(&nav_panel, cx), Sidebar::Threads);
        click(&nav_panel, Area::Agents, cx);
        assert_eq!(sidebar(&nav_panel, cx), Sidebar::None);
        assert_eq!(
            shown(&nav_panel, cx),
            (Area::Agents, Some(ViewKind::Thread))
        );

        click(&nav_panel, Area::Inbox, cx);
        click(&nav_panel, Area::Inbox, cx);
        assert_eq!(shown(&nav_panel, cx), (Area::Inbox, Some(ViewKind::Inbox)));
    }

    /// The Pull requests button did nothing with no review open (it went to Home, where you
    /// were). Review always does something: the Review page from anywhere, back to the page from
    /// a review, and on the page it lists again.
    #[gpui::test]
    async fn the_review_button_never_does_nothing(cx: &mut TestAppContext) {
        let (workspace, nav_panel, cx) = workspace_with_nav_panel(cx).await;
        cx.simulate_resize(gpui::size(px(1512.), px(982.)));
        show_inbox(&workspace, cx);
        click(&nav_panel, Area::Review, cx);
        assert_eq!(
            shown(&nav_panel, cx),
            (Area::Review, Some(ViewKind::ReviewPage))
        );

        let review = workspace
            .update_in(cx, |workspace, window, cx| {
                review::ReviewView::open(review::ReviewContents::default(), workspace, window, cx)
            })
            .await
            .expect("the review opens");
        cx.run_until_parked();
        assert_eq!(
            shown(&nav_panel, cx),
            (Area::Review, Some(ViewKind::Review))
        );
        assert_eq!(sidebar(&nav_panel, cx), Sidebar::FileTree);

        click(&nav_panel, Area::Review, cx);
        assert_eq!(
            shown(&nav_panel, cx),
            (Area::Review, Some(ViewKind::ReviewPage))
        );
        assert_eq!(sidebar(&nav_panel, cx), Sidebar::None);
        // On the page, it stays there.
        click(&nav_panel, Area::Review, cx);
        assert_eq!(
            shown(&nav_panel, cx),
            (Area::Review, Some(ViewKind::ReviewPage))
        );

        // From elsewhere, Review comes back to the view it showed last.
        workspace.update_in(cx, |workspace, window, cx| {
            workspace.activate_item(&review, true, true, window, cx);
        });
        cx.run_until_parked();
        click(&nav_panel, Area::Inbox, cx);
        click(&nav_panel, Area::Review, cx);
        assert_eq!(
            shown(&nav_panel, cx),
            (Area::Review, Some(ViewKind::Review))
        );
        assert_eq!(active_item_id(&workspace, cx), Some(review.entity_id()));
    }

    /// ⌘1 to ⌘4 are the rail's buttons.
    #[gpui::test]
    async fn the_shortcuts_are_the_rails_buttons(cx: &mut TestAppContext) {
        let (workspace, nav_panel, cx) = workspace_with_nav_panel(cx).await;
        show_inbox(&workspace, cx);
        for (keys, area) in [
            ("secondary-2", Area::Edit),
            ("secondary-3", Area::Review),
            ("secondary-4", Area::Agents),
            ("secondary-1", Area::Inbox),
        ] {
            cx.simulate_keystrokes(keys);
            cx.run_until_parked();
            assert_eq!(shown(&nav_panel, cx).0, area, "{keys}");
        }
    }

    /// Opening a view that's open shows it again, without a second one or a second visit.
    #[gpui::test]
    async fn opening_a_view_twice_adds_nothing(cx: &mut TestAppContext) {
        let (workspace, nav_panel, cx) = workspace_with_nav_panel(cx).await;
        show_inbox(&workspace, cx);
        open_thread(&workspace, &nav_panel, cx);
        let counts = |cx: &mut VisualTestContext| {
            let items = workspace.read_with(cx, |workspace, cx| {
                workspace.active_pane().read(cx).items_len()
            });
            let visits =
                nav_panel.read_with(cx, |nav_panel, cx| nav_panel.areas.read(cx).history_len());
            (items, visits)
        };
        assert_eq!(counts(cx), (2, 2));
        open_thread(&workspace, &nav_panel, cx);
        assert_eq!(counts(cx), (2, 2));
        click(&nav_panel, Area::Agents, cx);
        click(&nav_panel, Area::Agents, cx);
        assert_eq!(counts(cx), (2, 2));
    }

    /// ⌘[ and ⌘] go back and forward through the views visited, from one area to another;
    /// outside Edit, ⌘W goes back too, and keeps the view; and each area comes back as it was
    /// left.
    #[gpui::test]
    async fn back_and_forward_go_through_the_views_visited(cx: &mut TestAppContext) {
        let (workspace, nav_panel, cx) = workspace_with_nav_panel(cx).await;
        show_inbox(&workspace, cx);
        open_thread(&workspace, &nav_panel, cx);
        click(&nav_panel, Area::Review, cx);
        let review_page = active_item_id(&workspace, cx);

        let back = |cx: &mut VisualTestContext| {
            cx.dispatch_action(crate::areas::Back);
            cx.run_until_parked();
        };
        let forward = |cx: &mut VisualTestContext| {
            cx.dispatch_action(crate::areas::Forward);
            cx.run_until_parked();
        };
        back(cx);
        assert_eq!(
            shown(&nav_panel, cx),
            (Area::Agents, Some(ViewKind::Thread))
        );
        back(cx);
        assert_eq!(shown(&nav_panel, cx), (Area::Inbox, Some(ViewKind::Inbox)));
        back(cx);
        assert_eq!(shown(&nav_panel, cx), (Area::Inbox, Some(ViewKind::Inbox)));
        forward(cx);
        assert_eq!(
            shown(&nav_panel, cx),
            (Area::Agents, Some(ViewKind::Thread))
        );
        forward(cx);
        assert_eq!(
            shown(&nav_panel, cx),
            (Area::Review, Some(ViewKind::ReviewPage))
        );

        // The keys.
        cx.simulate_keystrokes("secondary-[");
        cx.run_until_parked();
        assert_eq!(
            shown(&nav_panel, cx),
            (Area::Agents, Some(ViewKind::Thread))
        );
        cx.simulate_keystrokes("secondary-]");
        cx.run_until_parked();
        assert_eq!(
            shown(&nav_panel, cx),
            (Area::Review, Some(ViewKind::ReviewPage))
        );

        // ⌘W outside Edit goes back, and closes nothing.
        cx.dispatch_action(workspace::pane::CloseActiveItem::default());
        cx.run_until_parked();
        assert_eq!(
            shown(&nav_panel, cx),
            (Area::Agents, Some(ViewKind::Thread))
        );
        let items = workspace.read_with(cx, |workspace, cx| {
            workspace.active_pane().read(cx).items_len()
        });
        assert_eq!(items, 3);

        // Each area comes back to its last view.
        click(&nav_panel, Area::Inbox, cx);
        click(&nav_panel, Area::Review, cx);
        assert_eq!(active_item_id(&workspace, cx), review_page);
        click(&nav_panel, Area::Agents, cx);
        assert_eq!(
            shown(&nav_panel, cx),
            (Area::Agents, Some(ViewKind::Thread))
        );
    }

    fn edit_tabs(workspace: &Entity<Workspace>, cx: &mut VisualTestContext) -> Vec<String> {
        workspace.read_with(cx, |workspace, cx| {
            crate::areas::edit_items(workspace, cx)
                .iter()
                .map(|item| item.tab_content_text(0, cx).to_string())
                .collect()
        })
    }

    fn open_file(workspace: &Entity<Workspace>, path: &str, cx: &mut VisualTestContext) {
        let open = workspace.update_in(cx, |workspace, window, cx| {
            workspace.open_abs_path(
                std::path::PathBuf::from(path),
                workspace::OpenOptions {
                    visible: Some(workspace::OpenVisible::All),
                    focus: Some(true),
                    ..Default::default()
                },
                window,
                cx,
            )
        });
        cx.run_until_parked();
        cx.background_executor.run_until_parked();
        let _ = open;
        cx.run_until_parked();
    }

    /// Edit opens a folder, its files as tabs (Edit's alone), and comes back to the file it
    /// showed; ⌘W there closes the file, and Edit shows the one before, then its page.
    #[gpui::test]
    async fn edit_opens_files_as_tabs(cx: &mut TestAppContext) {
        let (workspace, nav_panel, cx) = workspace_with_files(
            cx,
            serde_json::json!({
                "repo": { "src": { "main.rs": "fn main() {}\n" }, "README.md": "# repo\n" },
            }),
        )
        .await;
        show_inbox(&workspace, cx);
        workspace.update_in(cx, |workspace, window, cx| {
            crate::edit_area::open_folder("/repo".into(), workspace, window, cx)
        });
        cx.run_until_parked();
        assert_eq!(
            shown(&nav_panel, cx),
            (Area::Edit, Some(ViewKind::EditHome))
        );
        let folder = nav_panel.read_with(cx, |nav_panel, cx| {
            nav_panel.edit_tree.read(cx).folder_path(cx)
        });
        assert_eq!(folder.as_deref(), Some(std::path::Path::new("/repo")));
        assert!(edit_tabs(&workspace, cx).is_empty());

        open_file(&workspace, "/repo/src/main.rs", cx);
        assert_eq!(shown(&nav_panel, cx), (Area::Edit, Some(ViewKind::File)));
        open_file(&workspace, "/repo/README.md", cx);
        assert_eq!(edit_tabs(&workspace, cx), vec!["main.rs", "README.md"]);

        // The Inbox isn't a tab, and Edit comes back to its last file.
        click(&nav_panel, Area::Inbox, cx);
        assert_eq!(edit_tabs(&workspace, cx), vec!["main.rs", "README.md"]);
        click(&nav_panel, Area::Edit, cx);
        let shown_tab = workspace.read_with(cx, |workspace, cx| {
            workspace
                .active_item(cx)
                .map(|item| item.tab_content_text(0, cx).to_string())
        });
        assert_eq!(shown_tab.as_deref(), Some("README.md"));

        // ⌘W closes the file.
        cx.dispatch_action(workspace::pane::CloseActiveItem::default());
        cx.run_until_parked();
        assert_eq!(edit_tabs(&workspace, cx), vec!["main.rs"]);
        assert_eq!(shown(&nav_panel, cx), (Area::Edit, Some(ViewKind::File)));
        cx.dispatch_action(workspace::pane::CloseActiveItem::default());
        cx.run_until_parked();
        assert!(edit_tabs(&workspace, cx).is_empty());
        assert_eq!(
            shown(&nav_panel, cx),
            (Area::Edit, Some(ViewKind::EditHome))
        );
    }

    /// A file opened from outside Edit (an agent message's link opens the thread's folder, then
    /// the file in it) switches to Edit, as a tab, with that folder in the tree.
    #[gpui::test]
    async fn a_file_opened_elsewhere_switches_to_edit(cx: &mut TestAppContext) {
        let (workspace, nav_panel, cx) = workspace_with_files(
            cx,
            serde_json::json!({ "worktree": { "lib.rs": "pub fn run() {}\n" } }),
        )
        .await;
        show_inbox(&workspace, cx);
        open_thread(&workspace, &nav_panel, cx);
        assert_eq!(shown(&nav_panel, cx).0, Area::Agents);

        // As `thread_chat::file_links` opens a link into the thread's folder.
        let add = workspace.update(cx, |workspace, cx| {
            workspace.project().update(cx, |project, cx| {
                project.find_or_create_worktree("/worktree", true, cx)
            })
        });
        cx.run_until_parked();
        add.await.expect("the folder opens");
        open_file(&workspace, "/worktree/lib.rs", cx);
        assert_eq!(shown(&nav_panel, cx), (Area::Edit, Some(ViewKind::File)));
        assert_eq!(edit_tabs(&workspace, cx), vec!["lib.rs"]);
        let folder = nav_panel.read_with(cx, |nav_panel, cx| {
            nav_panel.edit_tree.read(cx).folder_path(cx)
        });
        assert_eq!(folder.as_deref(), Some(std::path::Path::new("/worktree")));

        // Back goes to the thread.
        cx.dispatch_action(crate::areas::Back);
        cx.run_until_parked();
        assert_eq!(
            shown(&nav_panel, cx),
            (Area::Agents, Some(ViewKind::Thread))
        );
    }
}

//! mustr's areas, which the rail's four buttons and its gear show: the Inbox, Edit (Zed's editor
//! on a project, with a tab per open file), Review (pull requests and agent work), Agents (the
//! threads and the agent pages), and Settings.
//!
//! Outside Edit, an area shows one view at a time, with no tabs: opening a thread, a review or a
//! page replaces what was showing. The window keeps a history of the views you visit, as a
//! browser does, and ⌘[ and ⌘] (and the mouse's back and forward buttons) go back and forward
//! through it, from one area to another. Each area remembers the view it last showed, so the
//! rail takes you back to it.
//!
//! Every view is an item of the workspace's center pane, as in Zed. The pane's tab bar is hidden
//! (`default_overrides.json`), and the title bar draws tabs only in Edit (`title_bar.rs`). This
//! module decides which area an item belongs to, records a visit whenever an item becomes the
//! active one, and takes over Zed's pane actions that would switch to or close the items of the
//! other areas (⌘W, ⌘{ and ⌘}, ctrl-1…, ctrl-- …): outside Edit, ⌘W goes back.

use crate::{
    agent_setup::AgentSetupView, edit_area::EditHome, home::HomeView, navigation,
    project_settings::ProjectSettingsView, review_page::ReviewPage, schedules::SchedulesView,
    settings_page::SettingsView, usage_page::UsageView, worktrees::WorktreesView,
};
use collections::HashMap;
use gpui::{
    AnyWindowHandle, App, AppContext as _, Context, Div, Entity, EntityId, EventEmitter,
    FocusHandle, Focusable, Global, InteractiveElement as _, IntoElement, KeyBinding, MouseButton,
    MouseDownEvent, NavigationDirection, ParentElement as _, Render, SharedString, Styled as _,
    Subscription, WeakEntity, Window, actions, px,
};
use mustr_ui::{MustrIcon, tokens, tokens::scaled};
use review::ReviewView;
use thread_chat::ThreadView;
use ui::{Button, ButtonStyle, Color, Label, LabelSize, prelude::*};
use workspace::{
    ItemHandle, SaveIntent, Workspace,
    dock::Dock,
    item::{Item, ItemEvent},
    pane,
};

actions!(
    mustr,
    [
        /// Shows the Inbox: what needs you, pull requests and agent work.
        ShowInbox,
        /// Shows Edit: the editor on a project, with its files.
        ShowEdit,
        /// Shows Review: the pull requests and agent work to review.
        ShowReview,
        /// Shows Agents: the agent threads.
        ShowAgents,
        /// Goes back to the view you were on before.
        Back,
        /// Goes forward to the view you went back from.
        Forward,
    ]
);

/// The most visits the window's history keeps. Views that fall out of it, and that no area shows
/// last, are closed.
const MAX_HISTORY: usize = 50;

/// Where a view shows: the rail's four areas, and Settings (the rail's gear).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Area {
    Inbox,
    Edit,
    Review,
    Agents,
    Settings,
}

impl Area {
    pub fn title(self) -> &'static str {
        match self {
            Area::Inbox => "Inbox",
            Area::Edit => "Edit",
            Area::Review => "Review",
            Area::Agents => "Agents",
            Area::Settings => "Settings",
        }
    }
}

/// What a view is, which says the area it belongs to and the sidebar next to it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ViewKind {
    /// The Inbox's own page (`home.rs`).
    Inbox,
    /// The Review page (`review_page.rs`).
    ReviewPage,
    /// A pull request's or an agent thread's review.
    Review,
    Thread,
    /// What Agents shows before a thread is picked.
    AgentsHome,
    /// Scheduled tasks, worktrees, usage, agents setup, and a project's settings.
    AgentsPage,
    Settings,
    /// What Edit shows with no file open (`edit_area.rs`).
    EditHome,
    /// A file, or any other of Zed's views (a project search, a diff of the working copy, …).
    File,
}

impl ViewKind {
    pub fn of(item: &dyn ItemHandle) -> Self {
        if item.downcast::<HomeView>().is_some() {
            Self::Inbox
        } else if item.downcast::<ReviewPage>().is_some() {
            Self::ReviewPage
        } else if item.downcast::<ReviewView>().is_some() {
            Self::Review
        } else if item.downcast::<ThreadView>().is_some() {
            Self::Thread
        } else if item.downcast::<AgentsHome>().is_some() {
            Self::AgentsHome
        } else if item.downcast::<SchedulesView>().is_some()
            || item.downcast::<WorktreesView>().is_some()
            || item.downcast::<UsageView>().is_some()
            || item.downcast::<AgentSetupView>().is_some()
            || item.downcast::<ProjectSettingsView>().is_some()
        {
            Self::AgentsPage
        } else if item.downcast::<SettingsView>().is_some() {
            Self::Settings
        } else if item.downcast::<EditHome>().is_some() {
            Self::EditHome
        } else {
            Self::File
        }
    }

    pub fn area(self) -> Area {
        match self {
            Self::Inbox => Area::Inbox,
            Self::ReviewPage | Self::Review => Area::Review,
            Self::Thread | Self::AgentsHome | Self::AgentsPage => Area::Agents,
            Self::Settings => Area::Settings,
            Self::EditHome | Self::File => Area::Edit,
        }
    }

    /// Whether a view of this kind is closed once nothing leads back to it: the areas' own pages
    /// stay, and Edit's files stay until their tab is closed.
    fn closes_when_unreachable(self) -> bool {
        matches!(self, Self::Review | Self::Thread | Self::AgentsPage)
    }
}

struct Visit {
    area: Area,
    item: Box<dyn ItemHandle>,
}

/// A window's areas: the view on screen, the history of visits, and each area's last view.
pub struct Areas {
    workspace: WeakEntity<Workspace>,
    window: AnyWindowHandle,
    history: Vec<Visit>,
    /// The visit on screen, in `history`.
    position: usize,
    last: HashMap<Area, Box<dyn ItemHandle>>,
    /// The area of the view on screen, when that view was closed: it shows another view.
    shown_closed: Option<Area>,
    reconcile_scheduled: bool,
    _subscription: Subscription,
}

impl EventEmitter<()> for Areas {}

#[derive(Default)]
struct Registry(HashMap<EntityId, WeakEntity<Areas>>);

impl Global for Registry {}

/// The areas of `workspace`'s window, making them the first time. Call from `observe_new` on
/// workspaces, before the title bar and the rail read them.
pub fn ensure(
    workspace: &mut Workspace,
    window: &mut Window,
    cx: &mut Context<Workspace>,
) -> Entity<Areas> {
    if let Some(areas) = Areas::for_workspace(cx.entity_id(), cx) {
        return areas;
    }
    let workspace_handle = cx.entity();
    let areas = cx.new(|cx| Areas::new(&workspace_handle, window, cx));
    let workspace_id = cx.entity_id();
    cx.default_global::<Registry>()
        .0
        .insert(workspace_id, areas.downgrade());
    workspace.register_action(|workspace, _: &Back, window, cx| go_back(workspace, window, cx));
    workspace
        .register_action(|workspace, _: &Forward, window, cx| go_forward(workspace, window, cx));
    let weak_areas = areas.downgrade();
    let weak_workspace = workspace_handle.downgrade();
    workspace.register_action_renderer(move |div, _, _, _| {
        take_over_pane_actions(div, weak_areas.clone(), weak_workspace.clone())
    });
    areas
}

impl Areas {
    fn new(workspace: &Entity<Workspace>, window: &mut Window, cx: &mut Context<Self>) -> Self {
        let subscription = cx.subscribe_in(
            workspace,
            window,
            |this, _, event: &workspace::Event, window, cx| match event {
                workspace::Event::ActiveItemChanged | workspace::Event::ItemAdded { .. } => {
                    this.schedule_reconcile(window, cx)
                }
                workspace::Event::ItemRemoved { item_id } => {
                    this.forget(*item_id);
                    this.schedule_reconcile(window, cx);
                }
                _ => {}
            },
        );
        Self {
            workspace: workspace.downgrade(),
            window: window.window_handle(),
            history: Vec::new(),
            position: 0,
            last: HashMap::default(),
            shown_closed: None,
            reconcile_scheduled: false,
            _subscription: subscription,
        }
    }

    /// Records the view on screen in every window. The views a window opens while it's being
    /// made (the Inbox, and what the startup flags open) send their events before anything can
    /// hear them, so the window's setup calls this once the window is open.
    pub fn sync_all(cx: &mut App) {
        let Some(registry) = cx.try_global::<Registry>() else {
            return;
        };
        let all = registry
            .0
            .values()
            .filter_map(WeakEntity::upgrade)
            .collect::<Vec<_>>();
        for areas in all {
            let window = areas.read(cx).window;
            window
                .update(cx, |_, window, cx| {
                    areas.update(cx, |areas, cx| areas.schedule_reconcile(window, cx))
                })
                .ok();
        }
    }

    /// The areas of the workspace with this entity id, if it has them.
    pub fn for_workspace(workspace_id: EntityId, cx: &App) -> Option<Entity<Self>> {
        cx.try_global::<Registry>()?.0.get(&workspace_id)?.upgrade()
    }

    /// The area on screen.
    pub fn area(&self) -> Area {
        self.history
            .get(self.position)
            .map_or(Area::Inbox, |visit| visit.area)
    }

    /// The view on screen.
    pub fn shown(&self) -> Option<&dyn ItemHandle> {
        self.history
            .get(self.position)
            .map(|visit| visit.item.as_ref())
    }

    pub fn kind(&self) -> Option<ViewKind> {
        self.shown().map(ViewKind::of)
    }

    pub fn can_go_back(&self) -> bool {
        self.position > 0 && !self.history.is_empty()
    }

    pub fn can_go_forward(&self) -> bool {
        self.position + 1 < self.history.len()
    }

    /// The view `area` showed last.
    pub fn last_view(&self, area: Area) -> Option<&dyn ItemHandle> {
        self.last.get(&area).map(|item| item.as_ref())
    }

    #[cfg(test)]
    pub fn history_len(&self) -> usize {
        self.history.len()
    }

    /// Makes `item` the view on screen, as a new visit after the one on screen (dropping the
    /// visits you'd gone back from). Returns the views nothing leads back to any more.
    fn record(&mut self, area: Area, item: &dyn ItemHandle) -> Vec<Box<dyn ItemHandle>> {
        self.last.insert(area, item.boxed_clone());
        if self
            .shown()
            .is_some_and(|shown| shown.item_id() == item.item_id())
        {
            self.history[self.position].area = area;
            return Vec::new();
        }
        let mut dropped = Vec::new();
        if !self.history.is_empty() {
            dropped.extend(
                self.history
                    .drain(self.position + 1..)
                    .map(|visit| visit.item),
            );
        }
        self.history.push(Visit {
            area,
            item: item.boxed_clone(),
        });
        if self.history.len() > MAX_HISTORY {
            let excess = self.history.len() - MAX_HISTORY;
            dropped.extend(self.history.drain(..excess).map(|visit| visit.item));
        }
        self.position = self.history.len() - 1;
        dropped.retain(|item| !self.leads_to(item.item_id()));
        dropped.dedup_by_key(|item| item.item_id());
        dropped
    }

    /// Whether the history or an area still leads to the item.
    fn leads_to(&self, item_id: EntityId) -> bool {
        self.history
            .iter()
            .any(|visit| visit.item.item_id() == item_id)
            || self.last.values().any(|item| item.item_id() == item_id)
    }

    /// Forgets a view that was closed.
    fn forget(&mut self, item_id: EntityId) {
        if !self.leads_to(item_id) {
            return;
        }
        let shown = self
            .history
            .get(self.position)
            .and_then(|visit| (visit.item.item_id() == item_id).then_some(visit.area));
        let mut position = None;
        let mut kept = Vec::with_capacity(self.history.len());
        for (index, visit) in self.history.drain(..).enumerate() {
            if visit.item.item_id() == item_id {
                continue;
            }
            if index <= self.position {
                position = Some(kept.len());
            }
            kept.push(visit);
        }
        self.history = kept;
        self.position = position.unwrap_or(0);
        // Two visits of the same view are next to each other now, if the closed one was between.
        self.history.dedup_by(|later, earlier| {
            later.item.item_id() == earlier.item.item_id() && later.area == earlier.area
        });
        self.position = self.position.min(self.history.len().saturating_sub(1));
        self.last.retain(|_, item| item.item_id() != item_id);
        if shown.is_some() {
            self.shown_closed = shown;
        }
    }

    fn schedule_reconcile(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.reconcile_scheduled {
            return;
        }
        self.reconcile_scheduled = true;
        // After the events of the same change, such as the pane activating another item and then
        // removing the one it closed.
        cx.defer_in(window, |this, window, cx| this.reconcile(window, cx));
    }

    /// Records the active item as a visit, unless it's the view on screen. When the view on
    /// screen was closed, shows another view of its area instead of the one Zed's pane picked.
    fn reconcile(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.reconcile_scheduled = false;
        let Some(workspace) = self.workspace.upgrade() else {
            return;
        };
        if let Some(area) = self.shown_closed.take() {
            self.fall_back(area, &workspace, window, cx);
            cx.notify();
            return;
        }
        let Some(active) = workspace.read(cx).active_item(cx) else {
            // Everything was closed.
            workspace.update(cx, |workspace, cx| {
                open_root(Area::Inbox, workspace, window, cx)
            });
            return;
        };
        if self
            .shown()
            .is_some_and(|shown| shown.item_id() == active.item_id())
        {
            cx.notify();
            return;
        }
        let area = ViewKind::of(active.as_ref()).area();
        let dropped = self.record(area, active.as_ref());
        close_unreachable(&workspace, dropped, window, cx);
        cx.notify();
    }

    /// After the view on screen in `area` was closed: Edit shows the file you were on before (or
    /// its page with no file open); the other areas the view you were on before in that area (or
    /// the area's own page).
    fn fall_back(
        &mut self,
        area: Area,
        workspace: &Entity<Workspace>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let in_pane = |item: &dyn ItemHandle, cx: &App| contains_item(workspace.read(cx), item, cx);
        let previous = self.history[..self.history.len().min(self.position + 1)]
            .iter()
            .rev()
            .chain(self.history.iter().skip(self.position + 1))
            .find(|visit| {
                visit.area == area
                    && (area != Area::Edit || ViewKind::of(visit.item.as_ref()) == ViewKind::File)
                    && in_pane(visit.item.as_ref(), cx)
            })
            .map(|visit| visit.item.boxed_clone())
            .or_else(|| {
                (area == Area::Edit)
                    .then(|| edit_items(workspace.read(cx), cx).pop())
                    .flatten()
            });
        match previous {
            Some(item) => {
                let dropped = self.record(area, item.as_ref());
                workspace.update(cx, |workspace, cx| {
                    workspace.activate_item(item.as_ref(), true, true, window, cx);
                });
                close_unreachable(workspace, dropped, window, cx);
            }
            None => workspace.update(cx, |workspace, cx| open_root(area, workspace, window, cx)),
        }
    }
}

/// Whether `item` is in one of the workspace's panes.
fn contains_item(workspace: &Workspace, item: &dyn ItemHandle, cx: &App) -> bool {
    workspace
        .panes()
        .iter()
        .any(|pane| pane.read(cx).index_for_item(item).is_some())
}

/// Edit's files: the items of the active pane that are files (or Zed's other views), in the
/// pane's order.
pub fn edit_items(workspace: &Workspace, cx: &App) -> Vec<Box<dyn ItemHandle>> {
    workspace
        .active_pane()
        .read(cx)
        .items()
        .filter(|item| ViewKind::of(item.as_ref()) == ViewKind::File)
        .map(|item| item.boxed_clone())
        .collect()
}

/// Closes views nothing leads back to, except the ones with unsaved work.
fn close_unreachable(
    workspace: &Entity<Workspace>,
    items: Vec<Box<dyn ItemHandle>>,
    window: &mut Window,
    cx: &mut App,
) {
    let items = items
        .into_iter()
        .filter(|item| ViewKind::of(item.as_ref()).closes_when_unreachable() && !item.is_dirty(cx))
        .collect::<Vec<_>>();
    if items.is_empty() {
        return;
    }
    let panes = workspace.read(cx).panes().to_vec();
    for item in items {
        for pane in &panes {
            if pane.read(cx).index_for_item(item.as_ref()).is_some() {
                pane.update(cx, |pane, cx| {
                    pane.close_item_by_id(item.item_id(), SaveIntent::Skip, window, cx)
                        .detach_and_log_err(cx);
                });
            }
        }
    }
}

/// Opens an area's own page: the Inbox's, the Review page, Agents' and Edit's pages with
/// nothing picked, and Settings.
pub fn open_root(
    area: Area,
    workspace: &mut Workspace,
    window: &mut Window,
    cx: &mut Context<Workspace>,
) {
    match area {
        Area::Inbox => navigation::show_home(workspace, window, cx),
        Area::Review => crate::review_page::show(workspace, window, cx),
        Area::Agents => AgentsHome::show(workspace, window, cx),
        Area::Edit => crate::edit_area::show_home(workspace, window, cx),
        Area::Settings => crate::settings_page::show(
            workspace,
            crate::settings_page::Section::General,
            None,
            window,
            cx,
        ),
    }
}

/// Shows `area` with the view it showed last, or its own page.
pub fn show_area(
    area: Area,
    workspace: &mut Workspace,
    window: &mut Window,
    cx: &mut Context<Workspace>,
) {
    let Some(areas) = Areas::for_workspace(cx.entity_id(), cx) else {
        open_root(area, workspace, window, cx);
        return;
    };
    let last = areas
        .read(cx)
        .last_view(area)
        .map(|item| item.boxed_clone())
        .filter(|item| contains_item(workspace, item.as_ref(), cx));
    match last {
        Some(item) => show_item(&areas, area, item.as_ref(), workspace, window, cx),
        None => open_root(area, workspace, window, cx),
    }
}

/// Shows an area's own page, as a new visit.
pub fn show_root(
    area: Area,
    workspace: &mut Workspace,
    window: &mut Window,
    cx: &mut Context<Workspace>,
) {
    open_root(area, workspace, window, cx);
}

fn show_item(
    areas: &Entity<Areas>,
    area: Area,
    item: &dyn ItemHandle,
    workspace: &mut Workspace,
    window: &mut Window,
    cx: &mut Context<Workspace>,
) {
    let dropped = areas.update(cx, |areas, cx| {
        let dropped = areas.record(area, item);
        cx.notify();
        dropped
    });
    workspace.activate_item(item, true, true, window, cx);
    close_unreachable(&cx.entity(), dropped, window, cx);
}

/// Goes back to the view you were on before, in whichever area it is.
pub fn go_back(workspace: &mut Workspace, window: &mut Window, cx: &mut Context<Workspace>) {
    step(-1, workspace, window, cx);
}

/// Goes forward to the view you went back from.
pub fn go_forward(workspace: &mut Workspace, window: &mut Window, cx: &mut Context<Workspace>) {
    step(1, workspace, window, cx);
}

fn step(by: isize, workspace: &mut Workspace, window: &mut Window, cx: &mut Context<Workspace>) {
    let Some(areas) = Areas::for_workspace(cx.entity_id(), cx) else {
        return;
    };
    let item = areas.update(cx, |areas, cx| {
        let position = areas.position.checked_add_signed(by)?;
        let item = areas.history.get(position)?.item.boxed_clone();
        areas.position = position;
        let area = areas.history[position].area;
        areas.last.insert(area, item.boxed_clone());
        cx.notify();
        Some(item)
    });
    if let Some(item) = item {
        workspace.activate_item(item.as_ref(), true, true, window, cx);
    }
}

/// Whether the focus is in the right or bottom dock (such as a terminal, whose own pane has tabs
/// that Zed's pane actions should keep working on).
fn focus_in_other_dock(workspace: &Workspace, window: &Window, cx: &App) -> bool {
    let has_focus = |dock: &Entity<Dock>| {
        dock.read(cx)
            .active_panel()
            .is_some_and(|panel| panel.panel_focus_handle(cx).contains_focused(window, cx))
    };
    has_focus(workspace.right_dock()) || has_focus(workspace.bottom_dock())
}

/// Takes over Zed's pane actions where they'd reach the views of other areas: outside Edit, ⌘W
/// goes back and switching tabs does nothing; in Edit, they work on its files alone. Splitting
/// the center pane isn't offered.
fn take_over_pane_actions(
    div: Div,
    areas: WeakEntity<Areas>,
    workspace: WeakEntity<Workspace>,
) -> Div {
    // The area and the kind of view on screen, unless the action is for a dock's own pane.
    let target = {
        let workspace = workspace.clone();
        move |window: &Window, cx: &App| {
            let workspace = workspace.upgrade()?;
            if focus_in_other_dock(workspace.read(cx), window, cx) {
                return None;
            }
            let areas = areas.upgrade()?.read(cx);
            Some((areas.area(), areas.kind()))
        }
    };
    let update = {
        let workspace = workspace.clone();
        move |window: &mut Window,
              cx: &mut App,
              f: &dyn Fn(&mut Workspace, &mut Window, &mut Context<Workspace>)| {
            workspace
                .update(cx, |workspace, cx| f(workspace, window, cx))
                .ok();
        }
    };
    macro_rules! outside_edit_only {
        ($div:expr, $action:ty) => {{
            let action_target = target.clone();
            $div.capture_action(move |_: &$action, window, cx| {
                if let Some((area, _)) = action_target(window, cx)
                    && area != Area::Edit
                {
                    cx.stop_propagation();
                }
            })
        }};
    }
    let div = {
        let target = target.clone();
        let update = update.clone();
        div.capture_action(
            move |_: &pane::CloseActiveItem, window, cx| match target(window, cx) {
                Some((Area::Edit, Some(ViewKind::File))) | None => {}
                Some((Area::Edit, _)) => cx.stop_propagation(),
                Some(_) => {
                    cx.stop_propagation();
                    update(window, cx, &go_back);
                }
            },
        )
    };
    let div = {
        let target = target.clone();
        let update = update.clone();
        div.capture_action(move |_: &pane::GoBack, window, cx| {
            if let Some((area, _)) = target(window, cx)
                && area != Area::Edit
            {
                cx.stop_propagation();
                update(window, cx, &go_back);
            }
        })
    };
    let div = {
        let target = target.clone();
        let update = update.clone();
        div.capture_action(move |_: &pane::GoForward, window, cx| {
            if let Some((area, _)) = target(window, cx)
                && area != Area::Edit
            {
                cx.stop_propagation();
                update(window, cx, &go_forward);
            }
        })
    };
    // Switching tabs: Edit's files only.
    let switch = {
        let target = target.clone();
        let update = update.clone();
        move |window: &mut Window, cx: &mut App, pick: &dyn Fn(usize, usize) -> Option<usize>| {
            let Some((area, _)) = target(window, cx) else {
                return;
            };
            cx.stop_propagation();
            if area != Area::Edit {
                return;
            }
            update(window, cx, &|workspace, window, cx| {
                let items = edit_items(workspace, cx);
                let active = workspace.active_item(cx).map(|item| item.item_id());
                let current = items
                    .iter()
                    .position(|item| Some(item.item_id()) == active)
                    .unwrap_or(0);
                if let Some(item) = pick(current, items.len()).and_then(|index| items.get(index)) {
                    workspace.activate_item(item.as_ref(), true, true, window, cx);
                }
            });
        }
    };
    let div = {
        let switch = switch.clone();
        div.capture_action(move |action: &pane::ActivatePreviousItem, window, cx| {
            let wrap = action.wrap_around;
            switch(window, cx, &|current, len| match current.checked_sub(1) {
                Some(index) => Some(index),
                None if wrap => len.checked_sub(1),
                None => None,
            })
        })
    };
    let div = {
        let switch = switch.clone();
        div.capture_action(move |action: &pane::ActivateNextItem, window, cx| {
            let wrap = action.wrap_around;
            switch(window, cx, &|current, len| {
                if current + 1 < len {
                    Some(current + 1)
                } else if wrap {
                    Some(0)
                } else {
                    None
                }
            })
        })
    };
    let div = {
        let switch = switch.clone();
        div.capture_action(move |action: &pane::ActivateItem, window, cx| {
            let index = action.0;
            switch(window, cx, &|_, len| (index < len).then_some(index))
        })
    };
    let div = {
        let switch = switch.clone();
        div.capture_action(move |_: &pane::ActivateLastItem, window, cx| {
            switch(window, cx, &|_, len| len.checked_sub(1))
        })
    };
    // Closing several tabs: Edit's files only.
    let close_files = {
        let target = target.clone();
        move |window: &mut Window,
              cx: &mut App,
              keep: &dyn Fn(&dyn ItemHandle, bool, &App) -> bool| {
            let Some((area, _)) = target(window, cx) else {
                return;
            };
            cx.stop_propagation();
            if area != Area::Edit {
                return;
            }
            let Some(workspace) = workspace.upgrade() else {
                return;
            };
            let pane = workspace.read(cx).active_pane().clone();
            let active = workspace
                .read(cx)
                .active_item(cx)
                .map(|item| item.item_id());
            let closing = edit_items(workspace.read(cx), cx)
                .into_iter()
                .filter(|item| !keep(item.as_ref(), Some(item.item_id()) == active, cx))
                .map(|item| item.item_id())
                .collect::<Vec<_>>();
            pane.update(cx, |pane, cx| {
                for item_id in closing {
                    pane.close_item_by_id(item_id, SaveIntent::Close, window, cx)
                        .detach_and_log_err(cx);
                }
            });
        }
    };
    let div = {
        let close_files = close_files.clone();
        div.capture_action(move |_: &pane::CloseAllItems, window, cx| {
            close_files(window, cx, &|_, _, _| false)
        })
    };
    let div = {
        let close_files = close_files.clone();
        div.capture_action(move |_: &pane::CloseOtherItems, window, cx| {
            close_files(window, cx, &|_, active, _| active)
        })
    };
    let div = {
        let close_files = close_files.clone();
        div.capture_action(move |_: &pane::CloseCleanItems, window, cx| {
            close_files(window, cx, &|item, _, cx| item.is_dirty(cx))
        })
    };
    let div = outside_edit_only!(div, pane::CloseItemsToTheLeft);
    let div = outside_edit_only!(div, pane::CloseItemsToTheRight);
    let div = outside_edit_only!(div, pane::CloseMultibufferItems);
    let div = outside_edit_only!(div, pane::SwapItemLeft);
    let div = outside_edit_only!(div, pane::SwapItemRight);
    let div = outside_edit_only!(div, pane::AlternateFile);
    // The center has one pane, so that areas can share it.
    macro_rules! never {
        ($div:expr, $($action:ty),*) => {{
            let div = $div;
            $(
                let action_target = target.clone();
                let div = div.capture_action(move |_: &$action, window, cx| {
                    if action_target(window, cx).is_some() {
                        cx.stop_propagation();
                    }
                });
            )*
            div
        }};
    }
    let div = never!(
        div,
        pane::SplitLeft,
        pane::SplitRight,
        pane::SplitUp,
        pane::SplitDown,
        pane::SplitHorizontal,
        pane::SplitVertical,
        pane::SplitAndMoveLeft,
        pane::SplitAndMoveRight,
        pane::SplitAndMoveUp,
        pane::SplitAndMoveDown
    );
    // The mouse's back and forward buttons, anywhere in the window.
    div.capture_any_mouse_down(move |event: &MouseDownEvent, window, cx| {
        let MouseButton::Navigate(direction) = event.button else {
            return;
        };
        cx.stop_propagation();
        match direction {
            NavigationDirection::Back => update(window, cx, &go_back),
            NavigationDirection::Forward => update(window, cx, &go_forward),
        }
    })
}

/// The key bindings of the areas and the history: ⌘1 to ⌘4 for the rail's areas (Zed's keymap
/// gives them to switching between split panes, which mustr's center doesn't have), and ⌘[ and
/// ⌘] to go back and forward, also in a thread's composer and a review's editors, where Zed's
/// keymap would outdent and indent. In Edit's files, ⌘[ and ⌘] stay Zed's.
pub fn key_bindings() -> Vec<KeyBinding> {
    let mut bindings = vec![
        KeyBinding::new("secondary-1", ShowInbox, Some("Workspace")),
        KeyBinding::new("secondary-2", ShowEdit, Some("Workspace")),
        KeyBinding::new("secondary-3", ShowReview, Some("Workspace")),
        KeyBinding::new("secondary-4", ShowAgents, Some("Workspace")),
    ];
    for context in [
        "Workspace",
        "ThreadView > Editor",
        "ReviewView > Editor",
        "MustrReviewPage > Editor",
        "MustrHome > Editor",
        "MustrSettings > Editor",
    ] {
        bindings.push(KeyBinding::new("secondary-[", Back, Some(context)));
        bindings.push(KeyBinding::new("secondary-]", Forward, Some(context)));
    }
    bindings
}

/// What Agents shows before a thread is picked: a pointer to the threads list, and New thread.
pub struct AgentsHome {
    focus_handle: FocusHandle,
}

impl AgentsHome {
    /// Shows Agents' page, making it the first time.
    pub fn show(workspace: &mut Workspace, window: &mut Window, cx: &mut Context<Workspace>) {
        let existing = workspace
            .panes()
            .iter()
            .find_map(|pane| pane.read(cx).items_of_type::<Self>().next());
        if let Some(existing) = existing {
            workspace.activate_item(&existing, true, true, window, cx);
            return;
        }
        let view = cx.new(|cx| Self {
            focus_handle: cx.focus_handle(),
        });
        workspace.add_item_to_active_pane(Box::new(view), None, true, window, cx);
    }
}

impl EventEmitter<ItemEvent> for AgentsHome {}

impl Focusable for AgentsHome {
    fn focus_handle(&self, _: &App) -> FocusHandle {
        self.focus_handle.clone()
    }
}

impl Item for AgentsHome {
    type Event = ItemEvent;

    fn tab_content_text(&self, _: usize, _: &App) -> SharedString {
        "Agents".into()
    }

    fn to_item_events(event: &Self::Event, f: &mut dyn FnMut(ItemEvent)) {
        f(*event)
    }
}

impl Render for AgentsHome {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let palette = mustr_ui::palette(cx);
        v_flex()
            .key_context("MustrAgentsHome")
            .track_focus(&self.focus_handle)
            .size_full()
            .items_center()
            .justify_center()
            .gap(scaled(px(12.)))
            .bg(palette.background.hsla())
            .child(
                MustrIcon::Agent
                    .icon()
                    .size(IconSize::Custom(tokens::text_size(px(28.))))
                    .color(Color::Muted),
            )
            .child(
                Label::new("Pick a thread from the list, or start a new one.").color(Color::Muted),
            )
            .child(
                Button::new("agents-home-new-thread", "New thread")
                    .style(ButtonStyle::Filled)
                    .on_click(|_, window, cx| {
                        window.dispatch_action(Box::new(thread_chat::NewThread), cx)
                    }),
            )
            .child(
                Label::new("Schedules, worktrees, usage and agents setup are under the list.")
                    .size(LabelSize::Small)
                    .color(Color::Muted),
            )
    }
}

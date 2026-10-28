//! The left dock's panel: mustr's icon rail, and next to it the file tree of the active review.
//!
//! A dock shows one panel at a time, so the rail and the file tree share this panel
//! (`mustr-docs/ZED-INTEGRATION.md` Q4). While a review is active the panel is as wide as the
//! rail plus the tree; elsewhere it narrows to just the rail.

use crate::{home::HomeView, navigation};
use gpui::{
    Action, App, AppContext as _, Context, Entity, EventEmitter, FocusHandle, Focusable,
    InteractiveElement as _, IntoElement, ParentElement as _, Pixels, Render, Styled as _,
    Subscription, WeakEntity, Window, div, px,
};
use mustr_ui::{MustrIcon, RailButton, tokens};
use review::{FileTreePanel, ReviewView, ToggleFileTree};
use ui::{FluentBuilder as _, h_flex, v_flex};
use workspace::{
    ItemHandle, Workspace,
    dock::{Dock, DockPosition, Panel, PanelEvent, PanelSizeState},
};
use zed_actions::OpenSettingsFile;

const DEFAULT_TREE_WIDTH: Pixels = px(280.);

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Destination {
    Home,
    Review,
    Other,
}

pub struct NavPanel {
    focus_handle: FocusHandle,
    workspace: WeakEntity<Workspace>,
    file_tree: Entity<FileTreePanel>,
    destination: Destination,
    /// Whether the panel was last sized for a review (rail and file tree) or not (rail only).
    sized_for_review: Option<bool>,
    /// The panel's width while a review is active, restored when a review shows again.
    review_width: Option<Pixels>,
    _subscription: Subscription,
}

impl NavPanel {
    /// Doesn't read `workspace`, so it can be called while the workspace is being updated (for
    /// example, from `observe_new`).
    pub fn new(workspace: &Entity<Workspace>, window: &mut Window, cx: &mut Context<Self>) -> Self {
        let file_tree = cx.new(|cx| FileTreePanel::new(workspace, window, cx));
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
            destination: Destination::Other,
            sized_for_review: None,
            review_width: None,
            _subscription: subscription,
        }
    }

    /// Highlights the rail button for `item`, and shows the file tree (widening the panel) if
    /// it's a review. Call once the panel is in `left_dock`, and whenever the active item
    /// changes.
    pub fn active_item_changed(
        &mut self,
        item: Option<&dyn ItemHandle>,
        left_dock: &Entity<Dock>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let destination = match item {
            Some(item) if item.downcast::<ReviewView>().is_some() => Destination::Review,
            Some(item) if item.downcast::<HomeView>().is_some() => Destination::Home,
            _ => Destination::Other,
        };
        self.destination = destination;
        cx.notify();
        let is_review = destination == Destination::Review;
        if self.sized_for_review == Some(is_review) {
            return;
        }

        let panel = cx.entity();
        if self.sized_for_review == Some(true) {
            self.review_width = left_dock
                .read(cx)
                .stored_panel_size_state(&panel)
                .and_then(|size_state| size_state.size);
        }
        let width = if is_review {
            self.review_width
                .unwrap_or_else(|| self.default_size(window, cx))
        } else {
            tokens::RAIL_WIDTH
        };
        self.sized_for_review = Some(is_review);
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
            .when(self.destination == Destination::Review, |this| {
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
                    .tooltip("Agent threads (coming soon)"),
            )
            .child(div().flex_1())
            .child(
                RailButton::new("rail-settings", MustrIcon::Settings.icon())
                    .tooltip("Settings")
                    .on_click(|_, window, cx| {
                        window.dispatch_action(OpenSettingsFile.boxed_clone(), cx)
                    }),
            )
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
        if self.destination == Destination::Review {
            self.file_tree.focus_handle(cx)
        } else {
            self.focus_handle.clone()
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
            .when(self.destination == Destination::Review, |this| {
                this.child(
                    div()
                        .flex_1()
                        .min_w_0()
                        .h_full()
                        .child(self.file_tree.clone()),
                )
            })
    }
}

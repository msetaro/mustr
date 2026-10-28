//! mustr's title bar: a tab per open item (home, pull requests), the command search box, the T3
//! connection, and the window controls.
//!
//! Zed's `title_bar` crate is collab-centric (sign-in, calls, project and branch pickers), so
//! mustr wraps `PlatformTitleBar` directly, the way `TitleBar` does in
//! `crates/title_bar/src/title_bar.rs`. `PlatformTitleBar`
//! (`crates/platform_title_bar/src/platform_title_bar.rs`) handles each platform:
//! - On Linux, it draws the client-side window controls and the window menu, and double-click
//!   maximizes.
//! - On macOS, it draws no controls and leaves room on the left for the traffic lights, which
//!   `build_window_options` places at (9, 9). Double-click does what the user chose in System
//!   Settings (zoom, minimize, or nothing).
//! - Everywhere, dragging the empty parts of the bar moves the window. mustr's window options set
//!   `app_owns_titlebar_drag`, so on macOS this drag is the app's, not AppKit's.
//!
//! The tabs replace Zed's pane tab bar, which mustr's default settings hide
//! (`tab_bar.show: false`). They show the items of the workspace's active pane, and switch and
//! close them through Zed's pane API.

use crate::{
    home::HomeView,
    navigation,
    t3_connection::{ConnectionStatus, EngineKind, EngineSetup, T3Connection},
    t3_engine::T3Mode,
};
use gpui::{
    AnyElement, App, AppContext as _, Context, Entity, Hsla, InteractiveElement as _, IntoElement,
    MouseButton, ParentElement as _, Render, SharedString, StatefulInteractiveElement as _,
    Styled as _, Subscription, TaskExt as _, WeakEntity, Window, WindowButtonLayout, div, px,
};
use mustr_ui::{Chip, WindowTab, kbds_for_action, tokens};
use platform_title_bar::PlatformTitleBar;
use review::ReviewView;
use settings::{RegisterSetting, Settings, SettingsContent};
use theme::ActiveTheme as _;
use ui::{FluentBuilder as _, Tooltip, h_flex};
use workspace::{Pane, SaveIntent, Workspace};

pub fn init(cx: &mut App) {
    PlatformTitleBar::init(cx);
    cx.observe_new(|workspace: &mut Workspace, window, cx| {
        let Some(window) = window else {
            return;
        };
        let workspace_handle = cx.entity();
        let pane = workspace.active_pane().clone();
        let title_bar = cx.new(|cx| MustrTitleBar::new(&workspace_handle, pane, window, cx));
        workspace.set_titlebar_item(title_bar.into(), window, cx);
    })
    .detach();
}

/// Zed's `title_bar.button_layout` setting. Its `TitleBarSettings` lives in the `title_bar`
/// crate, which mustr doesn't link.
#[derive(Clone, Copy, Debug, RegisterSetting)]
struct ButtonLayoutSetting(Option<WindowButtonLayout>);

impl Settings for ButtonLayoutSetting {
    fn from_settings(content: &SettingsContent) -> Self {
        Self(
            content
                .title_bar
                .as_ref()
                .and_then(|title_bar| title_bar.button_layout.clone())
                .unwrap_or_default()
                .into_layout(),
        )
    }
}

pub struct MustrTitleBar {
    platform_title_bar: Entity<PlatformTitleBar>,
    connection: Entity<T3Connection>,
    workspace: WeakEntity<Workspace>,
    /// The pane whose items are shown as tabs: the workspace's active pane.
    pane: Entity<Pane>,
    _pane_subscriptions: [Subscription; 2],
    _subscriptions: Vec<Subscription>,
}

impl MustrTitleBar {
    /// Doesn't read `workspace`, so it can be called while the workspace is being updated (for
    /// example, from `observe_new`).
    fn new(
        workspace: &Entity<Workspace>,
        pane: Entity<Pane>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        let connection = T3Connection::global(cx);
        let subscriptions = vec![
            cx.observe(&connection, |_, _, cx| cx.notify()),
            cx.observe_button_layout_changed(window, |_, _, cx| cx.notify()),
            cx.subscribe(workspace, |this, workspace, event, cx| {
                if let workspace::Event::ActiveItemChanged
                | workspace::Event::PaneAdded(_)
                | workspace::Event::PaneRemoved = event
                {
                    let pane = workspace.read(cx).active_pane().clone();
                    if pane != this.pane {
                        this._pane_subscriptions = Self::watch_pane(&pane, cx);
                        this.pane = pane;
                    }
                    cx.notify();
                }
            }),
        ];
        Self {
            platform_title_bar: cx.new(|cx| PlatformTitleBar::new("mustr-title-bar", cx)),
            connection,
            workspace: workspace.downgrade(),
            _pane_subscriptions: Self::watch_pane(&pane, cx),
            pane,
            _subscriptions: subscriptions,
        }
    }

    /// Re-renders when the pane's items, active item, or item titles change.
    fn watch_pane(pane: &Entity<Pane>, cx: &mut Context<Self>) -> [Subscription; 2] {
        [
            cx.observe(pane, |_, _, cx| cx.notify()),
            cx.subscribe(pane, |_, _, _: &workspace::pane::Event, cx| cx.notify()),
        ]
    }

    fn render_tabs(&self, cx: &App) -> Vec<AnyElement> {
        let pane = self.pane.read(cx);
        let active_index = pane.active_item_index();
        pane.items()
            .enumerate()
            .map(|(index, item)| {
                let item_id = item.item_id();
                // Home is always there, so it can't be closed from its tab.
                let closable = item.downcast::<HomeView>().is_none();
                let weak_pane = self.pane.downgrade();
                // Pull requests are labeled `owner/repo#123`, as in the mockups, and agent
                // threads by their title; the tooltip has the rest.
                let label = item
                    .downcast::<ReviewView>()
                    .and_then(|review| Some(review.read(cx).header()?.tab_label()))
                    .unwrap_or_else(|| item.tab_content_text(0, cx));
                WindowTab::new(("title-bar-tab", item_id.as_u64()), label)
                    .active(index == active_index)
                    .when_some(item.tab_tooltip_text(cx), |tab, tooltip| {
                        tab.tooltip(tooltip)
                    })
                    .on_click({
                        let weak_pane = weak_pane.clone();
                        move |_, window, cx| {
                            weak_pane
                                .update(cx, |pane, cx| {
                                    let index =
                                        pane.items().position(|item| item.item_id() == item_id);
                                    if let Some(index) = index {
                                        pane.activate_item(index, true, true, window, cx);
                                    }
                                })
                                .ok();
                        }
                    })
                    .when(closable, |tab| {
                        tab.on_close(move |window, cx| {
                            weak_pane
                                .update(cx, |pane, cx| {
                                    pane.close_item_by_id(item_id, SaveIntent::Close, window, cx)
                                        .detach_and_log_err(cx);
                                })
                                .ok();
                        })
                    })
                    .into_any_element()
            })
            .collect()
    }

    fn render_new_tab_button(&self, cx: &App) -> impl IntoElement {
        let palette = mustr_ui::palette(cx);
        let workspace = self.workspace.clone();
        div()
            .id("title-bar-new-tab")
            .flex_none()
            .px(px(8.))
            .py(px(1.))
            .rounded(tokens::CONTROL_RADIUS)
            .text_color(palette.text_muted.hsla())
            .text_size(tokens::text_size(tokens::TEXT_SIZE_LARGE))
            .cursor_pointer()
            .hover(|style| {
                style
                    .bg(palette.surface_2.hsla())
                    .text_color(palette.text.hsla())
            })
            .tooltip(Tooltip::text("Open a pull request"))
            .on_mouse_down(MouseButton::Left, |_, window, _| window.prevent_default())
            .on_click(move |_, window, cx| {
                cx.stop_propagation();
                workspace
                    .update(cx, |workspace, cx| {
                        navigation::show_home(workspace, window, cx)
                    })
                    .ok();
            })
            .child("+")
    }

    /// A search box that opens the command palette.
    fn render_search_box(&self, window: &Window, cx: &App) -> impl IntoElement {
        let palette = mustr_ui::palette(cx);
        let action = zed_actions::command_palette::Toggle;
        h_flex()
            .id("title-bar-search")
            .flex_none()
            .w(px(220.))
            .gap(px(8.))
            .px(px(8.))
            .py(px(2.))
            .rounded(tokens::CONTROL_RADIUS)
            .border(tokens::BORDER_WIDTH)
            .border_color(palette.border.hsla())
            .bg(palette.background.hsla())
            .text_color(palette.text_muted.hsla())
            .text_size(tokens::text_size(tokens::TEXT_SIZE_SMALL))
            .whitespace_nowrap()
            .overflow_hidden()
            .cursor_pointer()
            .hover(|style| style.border_color(palette.text_muted.hsla().opacity(0.5)))
            .children(
                kbds_for_action(&action, "Workspace", window)
                    .into_iter()
                    .flatten(),
            )
            .child("Search commands…")
            .on_mouse_down(MouseButton::Left, |_, window, _| window.prevent_default())
            .on_click(|_, window, cx| {
                cx.stop_propagation();
                window.dispatch_action(Box::new(zed_actions::command_palette::Toggle), cx);
            })
    }

    fn render_connection_status(&self, cx: &App) -> impl IntoElement {
        let palette = mustr_ui::palette(cx);
        let status_colors = cx.theme().status();
        let connection = self.connection.read(cx);
        let embedded = connection.effective_mode() == T3Mode::Embedded;
        let server = connection
            .server_url()
            .map(|url| url.as_str().trim_end_matches('/').to_string())
            .unwrap_or_default();
        let (dot, label, tooltip): (Hsla, SharedString, SharedString) = match connection.status() {
            ConnectionStatus::Connected {
                environment,
                server_version,
                engine,
            } => {
                let version = server_version.as_ref().map_or_else(
                    || "T3 Code".to_string(),
                    |version| format!("T3 Code {version}"),
                );
                match engine {
                    EngineKind::Embedded => (
                        palette.ok.hsla(),
                        match server_version {
                            Some(version) => format!("Agents · {version}").into(),
                            None => "Agents".into(),
                        },
                        format!("mustr's agents engine ({version}) at {server}").into(),
                    ),
                    EngineKind::Installed | EngineKind::External => (
                        palette.ok.hsla(),
                        format!("T3 · {environment}").into(),
                        format!("Connected to {version} at {server}").into(),
                    ),
                }
            }
            ConnectionStatus::Connecting(step) => (
                palette.text_muted.hsla(),
                if embedded {
                    "Starting agents engine…".into()
                } else {
                    "Connecting…".into()
                },
                step.clone(),
            ),
            ConnectionStatus::NeedsPairing(reason) => {
                (palette.warning.hsla(), "Not paired".into(), reason.clone())
            }
            ConnectionStatus::NeedsSetup(setup) => (
                palette.warning.hsla(),
                "Set up agents".into(),
                match setup {
                    EngineSetup::NotInstalled { .. } => {
                        "The agents engine isn't installed. Open Home to set it up.".into()
                    }
                    EngineSetup::NotRunning { .. } => {
                        "T3 Code isn't running. Open Home to start it.".into()
                    }
                    EngineSetup::Incompatible { message, .. } => message.clone(),
                },
            ),
            ConnectionStatus::Failed(error) => (
                status_colors.error,
                if embedded {
                    "Engine stopped".into()
                } else {
                    "T3 offline".into()
                },
                format!("{error} Open Home to try again.").into(),
            ),
        };
        let workspace = self.workspace.clone();
        Chip::new("title-bar-connection", label)
            .dot(dot)
            .tooltip(tooltip)
            .on_click(move |_, window, cx| {
                workspace
                    .update(cx, |workspace, cx| {
                        navigation::show_home(workspace, window, cx)
                    })
                    .ok();
            })
    }
}

impl Render for MustrTitleBar {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        // A press on a tab or a button mustn't start moving the window, or a click with a little
        // mouse movement would be lost to the move. Copied from Zed's `TitleBar`, which stops
        // the same way around its buttons.
        let children = [
            h_flex()
                .id("title-bar-tabs")
                .min_w_0()
                .gap(px(4.))
                .overflow_x_scroll()
                .on_mouse_down(MouseButton::Left, |_, _, cx| cx.stop_propagation())
                .children(self.render_tabs(cx))
                .child(self.render_new_tab_button(cx))
                .into_any_element(),
            h_flex()
                .flex_none()
                .pl_2()
                .pr_1()
                .gap(px(8.))
                .on_mouse_down(MouseButton::Left, |_, _, cx| cx.stop_propagation())
                .child(self.render_search_box(window, cx))
                .child(self.render_connection_status(cx))
                .into_any_element(),
        ];
        let button_layout = ButtonLayoutSetting::get_global(cx).0;
        self.platform_title_bar.update(cx, |title_bar, _| {
            title_bar.set_button_layout(button_layout);
            title_bar.set_children(children);
        });
        self.platform_title_bar.clone()
    }
}

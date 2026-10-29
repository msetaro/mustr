//! mustr's title bar: a tab per open item (home, pull requests), the command search box, and the
//! window controls. The T3 connection's state is in the status bar (`status_bar.rs`), and
//! switching servers in Settings › Connection.
//!
//! Zed's `title_bar` crate is collab-centric (sign-in, calls, project and branch pickers), so
//! mustr draws the bar the way Zed's `PlatformTitleBar`
//! (`crates/platform_title_bar/src/platform_title_bar.rs`) does, copied in [`MustrTitleBar`]'s
//! `render`, so that the bar's height scales with `ui_font_size` like the rest of mustr. Zed's
//! bar is `1.75` rems tall with a 34px minimum, which is 34px at mustr's design size (13px) but
//! only 45.5px, not 68, at twice that. Per platform:
//! - On Linux, it draws the client-side window controls and the window menu, and double-click
//!   maximizes.
//! - On macOS, it draws no controls and leaves room on the left for the traffic lights, which are
//!   native and don't scale. `build_window_options` places them at the left, vertically centered
//!   in the bar ([`traffic_light_position`]), and the bar moves them when a zoom changes its
//!   height. Double-click does what the user chose in System Settings (zoom, minimize, or
//!   nothing).
//! - Everywhere, dragging the empty parts of the bar moves the window. mustr's window options set
//!   `app_owns_titlebar_drag`, so on macOS this drag is the app's, not AppKit's.
//! - With Zed's `use_system_window_tabs` setting on, mustr uses `PlatformTitleBar` itself, which
//!   draws the macOS window tabs under the bar (`SystemWindowTabs` is private to its crate), at
//!   Zed's height.
//!
//! The tabs replace Zed's pane tab bar, which mustr's default settings hide
//! (`tab_bar.show: false`). They show the items of the workspace's active pane, and switch and
//! close them through Zed's pane API.

use crate::{home::HomeView, navigation};
use gpui::{
    AnyElement, App, AppContext as _, Context, Decorations, Entity, InteractiveElement as _,
    IntoElement, MouseButton, ParentElement as _, Pixels, Point, Render, ScrollHandle,
    StatefulInteractiveElement as _, Styled as _, Subscription, TaskExt as _, WeakEntity, Window,
    WindowButtonLayout, WindowControlArea, div, point, px,
};
use mustr_ui::{OverflowRow, WindowTab, kbds_for_action, tokens, tokens::scaled};
use platform_title_bar::{
    PlatformTitleBar, render_left_window_controls, render_right_window_controls,
};
use review::ReviewView;
use settings::{RegisterSetting, Settings, SettingsContent};
use theme::ActiveTheme as _;
use theme_settings::ThemeSettings;
use ui::{
    ButtonCommon as _, Clickable as _, Color, FluentBuilder as _, IconButton, IconName, IconSize,
    PlatformStyle, Tooltip, h_flex, utils::TRAFFIC_LIGHT_PADDING,
};
use workspace::{Pane, SaveIntent, Workspace, WorkspaceSettings};

/// The title bar's height at mustr's design size, and its minimum: Zed's minimum
/// (`platform_title_bar_height` in `crates/ui/src/utils/constants.rs`), which fits the traffic
/// lights.
const TITLE_BAR_HEIGHT: Pixels = px(34.);

/// A tab's widest, and how narrow tabs get before their row scrolls, at mustr's design size.
const TAB_MAX_WIDTH: Pixels = px(240.);
const TAB_MIN_WIDTH: Pixels = px(64.);
/// The most of the tabs' room the active tab takes, when there are several.
const ACTIVE_TAB_SHARE: f32 = 0.4;

/// The search box's share of the window's width, and its narrowest and widest, at mustr's design
/// size.
const SEARCH_BOX_SHARE: f32 = 0.2;
const SEARCH_BOX_MIN_WIDTH: Pixels = px(160.);
const SEARCH_BOX_MAX_WIDTH: Pixels = px(220.);

/// A window narrower than this, at mustr's design size, shows the search box as a button, so the
/// tabs keep room: a half-screen window at mustr's default size.
const COMPACT_BELOW_WIDTH: Pixels = px(560.);

/// Where Zed places the traffic lights in its 34px title bar (`build_window_options` in
/// `crates/zed/src/zed.rs`): 9px from the left, and vertically centered.
const TRAFFIC_LIGHT_INSET: Pixels = px(9.);

/// The title bar's height at `ui_font_size` `font_size`: [`TITLE_BAR_HEIGHT`], scaled.
///
/// With system window tabs on, mustr uses Zed's bar (see the module docs), so it's Zed's height.
/// On Windows it's Zed's fixed height too, which Windows' window frame expects.
fn title_bar_height(font_size: Pixels, cx: &App) -> Pixels {
    if cfg!(target_os = "windows") {
        px(32.)
    } else if WorkspaceSettings::get_global(cx).use_system_window_tabs {
        (1.75 * font_size).max(TITLE_BAR_HEIGHT)
    } else {
        scaled(TITLE_BAR_HEIGHT)
            .to_pixels(font_size)
            .max(TITLE_BAR_HEIGHT)
    }
}

/// Where the macOS traffic lights go: at Zed's spot, moved down by half of what the title bar
/// grew, so they stay vertically centered in it. They're native, so they don't scale; they stay
/// 9px from the left, inside the fixed room the bar leaves them (`TRAFFIC_LIGHT_PADDING`).
pub fn traffic_light_position(cx: &App) -> Point<Pixels> {
    let height = title_bar_height(ThemeSettings::get_global(cx).ui_font_size(cx), cx);
    point(
        TRAFFIC_LIGHT_INSET,
        TRAFFIC_LIGHT_INSET + (height - TITLE_BAR_HEIGHT) / 2.,
    )
}

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
    /// Zed's title bar, used only with system window tabs on (see the module docs).
    platform_title_bar: Entity<PlatformTitleBar>,
    /// Whether a press on the bar may start moving the window. Copied from `PlatformTitleBar`.
    should_move: bool,
    workspace: WeakEntity<Workspace>,
    /// The pane whose items are shown as tabs: the workspace's active pane.
    pane: Entity<Pane>,
    /// Measures the tabs' row, which they share.
    tabs_row: OverflowRow,
    /// Scrolls the tabs' row to the active tab when it or the row's width changes.
    tabs_scroll: ScrollHandle,
    scrolled_to: Option<(usize, Option<Pixels>)>,
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
        let subscriptions = vec![
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
        #[cfg(target_os = "macos")]
        let subscriptions = {
            let mut subscriptions = subscriptions;
            subscriptions.push(Self::keep_traffic_lights_centered(window, cx));
            subscriptions
        };
        Self {
            platform_title_bar: cx.new(|cx| PlatformTitleBar::new("mustr-title-bar", cx)),
            should_move: false,
            workspace: workspace.downgrade(),
            _pane_subscriptions: Self::watch_pane(&pane, cx),
            pane,
            tabs_row: OverflowRow::new(),
            tabs_scroll: ScrollHandle::new(),
            scrolled_to: None,
            _subscriptions: subscriptions,
        }
    }

    /// Moves the traffic lights when a zoom (or the system window tabs setting) changes the title
    /// bar's height. `build_window_options` places them for the height a window opens with.
    #[cfg(target_os = "macos")]
    fn keep_traffic_lights_centered(window: &mut Window, cx: &mut Context<Self>) -> Subscription {
        let mut position = traffic_light_position(cx);
        cx.observe_global_in::<settings::SettingsStore>(window, move |_, window, cx| {
            let new_position = traffic_light_position(cx);
            if new_position != position {
                position = new_position;
                window.set_traffic_light_position(position);
            }
        })
    }

    /// Re-renders when the pane's items, active item, or item titles change.
    fn watch_pane(pane: &Entity<Pane>, cx: &mut Context<Self>) -> [Subscription; 2] {
        [
            cx.observe(pane, |_, _, cx| cx.notify()),
            cx.subscribe(pane, |_, _, _: &workspace::pane::Event, cx| cx.notify()),
        ]
    }

    /// The tabs, sharing the row's width, down to a few letters each; then the row scrolls.
    fn render_tabs(&self, window: &Window, cx: &App) -> Vec<AnyElement> {
        let pane = self.pane.read(cx);
        let active_index = pane.active_item_index();
        let rem_size = window.rem_size();
        let count = pane.items_len().max(1);
        // The room left by the new tab button and the gaps.
        let room = self
            .tabs_row
            .width()
            .map(|width| width - scaled(px(36. + 4. * count as f32)).to_pixels(rem_size));
        // The active tab gets up to two fifths of the room, and the others share the rest.
        let (min, max) = (
            scaled(TAB_MIN_WIDTH).to_pixels(rem_size),
            scaled(TAB_MAX_WIDTH).to_pixels(rem_size),
        );
        let widths = room.map(|room| {
            if count == 1 {
                return (max, max);
            }
            let active = (room * ACTIVE_TAB_SHARE).min(max).max(min);
            let others = ((room - active) / (count - 1) as f32).min(max).max(min);
            (active, others)
        });
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
                    .when_some(widths, |tab, (active, others)| {
                        tab.max_width(if index == active_index {
                            active
                        } else {
                            others
                        })
                    })
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
            .px(scaled(px(8.)))
            .py(scaled(px(1.)))
            .rounded(scaled(tokens::CONTROL_RADIUS))
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

    /// A search box that opens the command palette: a fifth of the window, from 160 to 220 at
    /// mustr's design size, or in a narrow window just a search button.
    fn render_search_box(&self, compact: bool, window: &Window, cx: &App) -> AnyElement {
        let palette = mustr_ui::palette(cx);
        let action = zed_actions::command_palette::Toggle;
        if compact {
            return div()
                .debug_selector(|| "title-bar-search".into())
                .child(
                    IconButton::new("title-bar-search", IconName::MagnifyingGlass)
                        .icon_size(IconSize::Small)
                        .icon_color(Color::Muted)
                        .tooltip(Tooltip::for_action_title("Search commands", &action))
                        .on_click(|_, window, cx| {
                            cx.stop_propagation();
                            window.dispatch_action(
                                Box::new(zed_actions::command_palette::Toggle),
                                cx,
                            );
                        }),
                )
                .into_any_element();
        }
        let rem_size = window.rem_size();
        let width = (window.viewport_size().width * SEARCH_BOX_SHARE).clamp(
            scaled(SEARCH_BOX_MIN_WIDTH).to_pixels(rem_size),
            scaled(SEARCH_BOX_MAX_WIDTH).to_pixels(rem_size),
        );
        h_flex()
            .id("title-bar-search")
            .debug_selector(|| "title-bar-search".into())
            .flex_none()
            .w(width)
            .gap(scaled(px(8.)))
            .px(scaled(px(8.)))
            .py(scaled(px(2.)))
            .rounded(scaled(tokens::CONTROL_RADIUS))
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
            .child(
                div()
                    .min_w_0()
                    .overflow_hidden()
                    .text_ellipsis()
                    .child("Search commands…"),
            )
            .on_mouse_down(MouseButton::Left, |_, window, _| window.prevent_default())
            .on_click(|_, window, cx| {
                cx.stop_propagation();
                window.dispatch_action(Box::new(zed_actions::command_palette::Toggle), cx);
            })
            .into_any_element()
    }
}

impl Render for MustrTitleBar {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        // A press on a tab or a button mustn't start moving the window, or a click with a little
        // mouse movement would be lost to the move. Copied from Zed's `TitleBar`, which stops
        // the same way around its buttons.
        let compact =
            window.viewport_size().width < scaled(COMPACT_BELOW_WIDTH).to_pixels(window.rem_size());
        let scroll_to = (
            self.pane.read(cx).active_item_index(),
            self.tabs_row.width(),
        );
        if self.scrolled_to != Some(scroll_to) {
            self.scrolled_to = Some(scroll_to);
            self.tabs_scroll.scroll_to_item(scroll_to.0);
        }
        // The tabs scroll inside a row that measures the room they share (a scrolling row
        // doesn't report its children's bounds).
        let tabs = h_flex()
            .id("title-bar-tabs")
            .w_full()
            .min_w_0()
            .gap(scaled(px(4.)))
            .overflow_x_scroll()
            .track_scroll(&self.tabs_scroll)
            .children(self.render_tabs(window, cx))
            .child(self.render_new_tab_button(cx))
            .into_any_element();
        let children = [
            self.tabs_row
                .render([(None, tabs)], window)
                .flex_1()
                .on_mouse_down(MouseButton::Left, |_, _, cx| cx.stop_propagation())
                .into_any_element(),
            // The search box is all there is at the right end: no machine name, no connection
            // chip (the status bar shows the connection).
            h_flex()
                .debug_selector(|| "title-bar-end".into())
                .flex_none()
                .pl_2()
                .pr_1()
                .gap(scaled(px(8.)))
                .on_mouse_down(MouseButton::Left, |_, _, cx| cx.stop_propagation())
                .child(self.render_search_box(compact, window, cx))
                .into_any_element(),
        ];
        let button_layout = ButtonLayoutSetting::get_global(cx).0;
        if WorkspaceSettings::get_global(cx).use_system_window_tabs {
            self.platform_title_bar.update(cx, |title_bar, _| {
                title_bar.set_button_layout(button_layout);
                title_bar.set_children(children);
            });
            return self.platform_title_bar.clone().into_any_element();
        }
        self.render_bar(children, button_layout, window, cx)
            .into_any_element()
    }
}

impl MustrTitleBar {
    /// Copied from `PlatformTitleBar`'s `render` (`crates/platform_title_bar/src/
    /// platform_title_bar.rs`), with mustr's height and without Zed's threads sidebar, which
    /// mustr leaves out.
    fn render_bar(
        &mut self,
        children: [AnyElement; 2],
        button_layout: Option<WindowButtonLayout>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> impl IntoElement {
        let platform_style = PlatformStyle::platform();
        let supported_controls = window.window_controls();
        let decorations = window.window_decorations();
        let height = title_bar_height(window.rem_size(), cx);
        // `PlatformTitleBar::effective_button_layout`.
        let button_layout = if platform_style == PlatformStyle::Linux
            && matches!(decorations, Decorations::Client { .. })
        {
            button_layout.or_else(|| cx.button_layout())
        } else {
            None
        };
        // `PlatformTitleBar::title_bar_color`.
        let colors = cx.theme().colors();
        let titlebar_color = if cfg!(any(target_os = "linux", target_os = "freebsd"))
            && !(window.is_window_active() && !self.should_move)
        {
            colors.title_bar_inactive_background
        } else {
            colors.title_bar_background
        };
        let fullscreen = window.is_fullscreen() || window.is_simple_fullscreen();

        h_flex()
            .id("mustr-title-bar")
            .window_control_area(WindowControlArea::Drag)
            .w_full()
            .h(height)
            .on_mouse_down_out(cx.listener(|this, _, _, _| this.should_move = false))
            .on_mouse_up(
                MouseButton::Left,
                cx.listener(|this, _, _, _| this.should_move = false),
            )
            .on_mouse_down(
                MouseButton::Left,
                cx.listener(|this, _, _, _| this.should_move = true),
            )
            .on_mouse_move(cx.listener(|this, _, window, _| {
                if this.should_move {
                    this.should_move = false;
                    window.start_window_move();
                }
            }))
            .when(platform_style == PlatformStyle::Mac, |this| {
                this.on_click(|event, window, _| {
                    if event.click_count() == 2 {
                        window.titlebar_double_click();
                    }
                })
            })
            .when(
                platform_style == PlatformStyle::Linux
                    && supported_controls.maximize
                    && window.is_resizable(),
                |this| {
                    this.on_click(|event, window, _| {
                        if event.click_count() == 2 {
                            window.zoom_window();
                        }
                    })
                },
            )
            .map(|this| {
                if fullscreen {
                    this.pl_2()
                } else if platform_style == PlatformStyle::Mac {
                    // The traffic lights don't scale, so neither does their room.
                    this.pl(px(TRAFFIC_LIGHT_PADDING))
                } else if let Some(controls) = render_left_window_controls(
                    button_layout,
                    Box::new(workspace::CloseWindow),
                    window,
                ) {
                    this.child(controls)
                } else {
                    this.pl_2()
                }
            })
            .map(|this| match decorations {
                Decorations::Server => this,
                Decorations::Client { tiling, .. } => this
                    .when(!(tiling.top || tiling.right), |this| {
                        this.rounded_tr(theme::CLIENT_SIDE_DECORATION_ROUNDING)
                    })
                    .when(!(tiling.top || tiling.left), |this| {
                        this.rounded_tl(theme::CLIENT_SIDE_DECORATION_ROUNDING)
                    })
                    .mt(-theme::CLIENT_SIDE_DECORATION_BORDER)
                    .mb(-theme::CLIENT_SIDE_DECORATION_BORDER)
                    .border(theme::CLIENT_SIDE_DECORATION_BORDER)
                    .border_color(titlebar_color),
            })
            .bg(titlebar_color)
            .content_stretch()
            .child(
                div()
                    .id("mustr-title-bar-content")
                    .flex()
                    .flex_row()
                    .items_center()
                    .justify_between()
                    .overflow_x_hidden()
                    .w_full()
                    .children(children),
            )
            .when(!fullscreen, |this| {
                let this = this.children(render_right_window_controls(
                    button_layout,
                    Box::new(workspace::CloseWindow),
                    window,
                ));
                if platform_style == PlatformStyle::Linux
                    && matches!(decorations, Decorations::Client { .. })
                {
                    this.when(supported_controls.window_menu, |this| {
                        this.on_mouse_down(MouseButton::Right, |event, window, _| {
                            window.show_window_menu(event.position)
                        })
                    })
                } else {
                    this
                }
            })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use gpui::{TestAppContext, UpdateGlobal as _};
    use settings::SettingsStore;

    /// The title bar's right end is the search box alone, at every window size: the connection
    /// chip (`T3 · <machine>`, `Agents · <version>`) is gone, and the status bar has its jobs.
    #[gpui::test]
    async fn the_title_bar_ends_with_the_search_box(cx: &mut TestAppContext) {
        let backend = crate::thread_git::tests::RecordingBackend::with_shell(
            crate::thread_git::tests::shell(vec![]),
        );
        crate::thread_git::tests::init_test(backend, cx);
        cx.update(|cx| {
            PlatformTitleBar::init(cx);
        });
        let fs = project::FakeFs::new(cx.executor());
        let project = project::Project::test(fs, [], cx).await;
        let (multi_workspace, cx) = cx.add_window_view(|window, cx| {
            workspace::MultiWorkspace::test_new(project.clone(), window, cx)
        });
        let workspace =
            multi_workspace.read_with(cx, |multi_workspace, _| multi_workspace.workspace().clone());
        let pane = workspace.read_with(cx, |workspace, _| workspace.active_pane().clone());
        let (_, cx) = cx
            .add_window_view(|window, cx| MustrTitleBar::new(&workspace, pane.clone(), window, cx));
        for (width, rem_size) in [(1512., 13.), (1512., 26.), (600., 26.), (400., 13.)] {
            cx.simulate_resize(gpui::size(px(width), px(200.)));
            cx.update(|window, _| {
                window.set_rem_size(px(rem_size));
                window.refresh();
            });
            cx.run_until_parked();
            let end = cx
                .debug_bounds("title-bar-end")
                .expect("the right end draws");
            let search = cx
                .debug_bounds("title-bar-search")
                .expect("the search box draws");
            // Only the end's padding (`pl_2`, `pr_1`) is around the search box.
            let padding = end.size.width - search.size.width;
            assert!(
                (padding - px(0.75 * rem_size)).abs() < px(0.5),
                "{padding:?} besides the search box at {width}px, {rem_size}"
            );
            assert!(end.right() <= px(width), "{end:?} at {width}px");
        }
    }

    fn set_settings(ui_font_size: f32, system_window_tabs: bool, cx: &mut App) {
        SettingsStore::update_global(cx, |store, cx| {
            store.update_user_settings(cx, |settings| {
                settings.theme.ui_font_size = Some(ui_font_size.into());
                settings.workspace.use_system_window_tabs = Some(system_window_tabs);
            });
        });
    }

    /// The bar is 34px, as Zed's, at mustr's design size, and grows with `ui_font_size`. The
    /// traffic lights stay 9px from the left and move down by half of what it grew, so they stay
    /// centered.
    #[gpui::test]
    fn the_traffic_lights_stay_centered_as_the_bar_grows(cx: &mut TestAppContext) {
        cx.update(|cx| {
            let settings_store = SettingsStore::test(cx);
            cx.set_global(settings_store);
            theme_settings::init(theme::LoadThemes::JustBase, cx);
            for (ui_font_size, system_window_tabs, height, top) in [
                (13., false, 34., 9.),
                (26., false, 68., 26.),
                (19.5, false, 51., 17.5),
                // Smaller than mustr's design size: Zed's minimum height, and Zed's position.
                (10., false, 34., 9.),
                // Zed's own bar, which draws the system window tabs: 1.75 rems, at least 34px.
                (13., true, 34., 9.),
                (26., true, 45.5, 14.75),
            ] {
                set_settings(ui_font_size, system_window_tabs, cx);
                if cfg!(not(target_os = "windows")) {
                    assert_eq!(
                        title_bar_height(px(ui_font_size), cx),
                        px(height),
                        "{ui_font_size}px, system window tabs {system_window_tabs}"
                    );
                    assert_eq!(
                        traffic_light_position(cx),
                        point(px(9.), px(top)),
                        "{ui_font_size}px, system window tabs {system_window_tabs}"
                    );
                }
            }
        });
    }
}

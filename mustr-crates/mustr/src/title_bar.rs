//! mustr's title bar. Zed's `title_bar` crate is collab-centric (sign-in, calls, project and
//! branch pickers), so mustr wraps `PlatformTitleBar` directly, the way `TitleBar` does in
//! `crates/title_bar/src/title_bar.rs`. `PlatformTitleBar` draws the Linux client-side window
//! controls, handles dragging and double-click to maximize, and the window menu.

use crate::t3_connection::{ConnectionStatus, T3Connection};
use gpui::{
    App, AppContext as _, Context, Entity, IntoElement, ParentElement as _, Render, Styled as _,
    Subscription, Window, WindowButtonLayout,
};
use platform_title_bar::PlatformTitleBar;
use settings::{RegisterSetting, Settings, SettingsContent};
use ui::{Color, Indicator, Label, LabelCommon as _, LabelSize, prelude::*};
use workspace::Workspace;

pub fn init(cx: &mut App) {
    PlatformTitleBar::init(cx);
    cx.observe_new(|workspace: &mut Workspace, window, cx| {
        let Some(window) = window else {
            return;
        };
        let title_bar = cx.new(|cx| MustrTitleBar::new(window, cx));
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
    _subscriptions: Vec<Subscription>,
}

impl MustrTitleBar {
    fn new(window: &mut Window, cx: &mut Context<Self>) -> Self {
        let connection = T3Connection::global(cx);
        let subscriptions = vec![
            cx.observe(&connection, |_, _, cx| cx.notify()),
            cx.observe_button_layout_changed(window, |_, _, cx| cx.notify()),
        ];
        Self {
            platform_title_bar: cx.new(|cx| PlatformTitleBar::new("mustr-title-bar", cx)),
            connection,
            _subscriptions: subscriptions,
        }
    }

    fn render_connection_status(&self, cx: &App) -> impl IntoElement {
        let (color, text): (Color, SharedString) = match self.connection.read(cx).status() {
            ConnectionStatus::Connected { environment, .. } => {
                (Color::Success, format!("T3 · {environment}").into())
            }
            ConnectionStatus::Connecting(_) => (Color::Muted, "Connecting to T3…".into()),
            ConnectionStatus::NeedsPairing(_) => (Color::Warning, "Not paired with T3".into()),
            ConnectionStatus::Failed(_) => (Color::Error, "T3 offline".into()),
        };
        h_flex()
            .gap_1p5()
            .pr_2()
            .child(Indicator::dot().color(color))
            .child(Label::new(text).size(LabelSize::Small).color(Color::Muted))
    }
}

impl Render for MustrTitleBar {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let children = [
            h_flex()
                .gap_2()
                .child(
                    Label::new("mustr")
                        .size(LabelSize::Small)
                        .weight(gpui::FontWeight::SEMIBOLD),
                )
                .into_any_element(),
            self.render_connection_status(cx).into_any_element(),
        ];
        let button_layout = ButtonLayoutSetting::get_global(cx).0;
        self.platform_title_bar.update(cx, |title_bar, _| {
            title_bar.set_button_layout(button_layout);
            title_bar.set_children(children);
        });
        self.platform_title_bar.clone()
    }
}

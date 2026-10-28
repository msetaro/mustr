use crate::{components::ClickHandler, palette, tokens, tokens::scaled};
use gpui::{
    App, ClickEvent, ElementId, FontWeight, InteractiveElement as _, IntoElement, MouseButton,
    ParentElement as _, RenderOnce, SharedString, StatefulInteractiveElement as _, Styled as _,
    Window, div, px,
};
use ui::{Color, FluentBuilder as _, Icon, IconSize, Tooltip};

/// A square icon button for the icon rail, with an optional count badge.
#[derive(IntoElement)]
pub struct RailButton {
    id: ElementId,
    icon: Icon,
    active: bool,
    badge: Option<SharedString>,
    tooltip: Option<SharedString>,
    on_click: Option<ClickHandler>,
}

impl RailButton {
    pub fn new(id: impl Into<ElementId>, icon: Icon) -> Self {
        Self {
            id: id.into(),
            icon,
            active: false,
            badge: None,
            tooltip: None,
            on_click: None,
        }
    }

    /// Whether this button's destination is showing.
    pub fn active(mut self, active: bool) -> Self {
        self.active = active;
        self
    }

    pub fn badge(mut self, badge: impl Into<SharedString>) -> Self {
        self.badge = Some(badge.into());
        self
    }

    pub fn tooltip(mut self, tooltip: impl Into<SharedString>) -> Self {
        self.tooltip = Some(tooltip.into());
        self
    }

    pub fn on_click(
        mut self,
        handler: impl Fn(&ClickEvent, &mut Window, &mut App) + 'static,
    ) -> Self {
        self.on_click = Some(Box::new(handler));
        self
    }
}

impl RenderOnce for RailButton {
    fn render(self, _window: &mut Window, cx: &mut App) -> impl IntoElement {
        let palette = palette(cx);
        let icon_color = if self.active {
            palette.text.hsla()
        } else {
            palette.text_muted.hsla()
        };
        div()
            .id(self.id)
            .relative()
            .flex_none()
            .size(scaled(px(26.)))
            .flex()
            .items_center()
            .justify_center()
            .rounded(scaled(tokens::CONTROL_RADIUS))
            .when(self.active, |this| this.bg(palette.surface_2.hsla()))
            .child(
                self.icon
                    .size(IconSize::Custom(tokens::text_size(px(15.))))
                    .color(Color::Custom(icon_color)),
            )
            .when_some(self.badge, |this, badge| {
                this.child(
                    div()
                        .absolute()
                        .top(scaled(px(-2.)))
                        .right(scaled(px(-3.)))
                        .min_w(scaled(px(14.)))
                        .h(scaled(px(14.)))
                        .px(scaled(px(3.)))
                        .rounded(scaled(px(7.)))
                        .bg(palette.accent.hsla())
                        .text_color(palette.accent_foreground.hsla())
                        .text_size(tokens::text_size(px(9.)))
                        .line_height(scaled(px(14.)))
                        .font_weight(FontWeight::BOLD)
                        .text_center()
                        .child(badge),
                )
            })
            .when_some(self.tooltip, |this, tooltip| {
                this.tooltip(Tooltip::text(tooltip))
            })
            .when_some(self.on_click, |this, on_click| {
                this.cursor_pointer()
                    .when(!self.active, |this| {
                        this.hover(|style| style.bg(palette.surface_2.hsla()))
                    })
                    .on_mouse_down(MouseButton::Left, |_, window, _| window.prevent_default())
                    .on_click(move |event, window, cx| {
                        cx.stop_propagation();
                        on_click(event, window, cx);
                    })
            })
    }
}

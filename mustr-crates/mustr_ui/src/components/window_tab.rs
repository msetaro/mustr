use crate::{components::ClickHandler, palette, tokens, tokens::scaled};
use gpui::{
    App, ClickEvent, ElementId, InteractiveElement as _, IntoElement, MouseButton,
    ParentElement as _, RenderOnce, SharedString, StatefulInteractiveElement as _, Styled as _,
    Window, div, px,
};
use ui::{FluentBuilder as _, Tooltip, h_flex};

/// A tab in the title bar, for an open pull request, thread, or the home view. The active tab
/// is raised onto the window's background.
#[derive(IntoElement)]
pub struct WindowTab {
    id: ElementId,
    label: SharedString,
    active: bool,
    tooltip: Option<SharedString>,
    on_click: Option<ClickHandler>,
    on_close: Option<Box<dyn Fn(&mut Window, &mut App) + 'static>>,
}

impl WindowTab {
    pub fn new(id: impl Into<ElementId>, label: impl Into<SharedString>) -> Self {
        Self {
            id: id.into(),
            label: label.into(),
            active: false,
            tooltip: None,
            on_click: None,
            on_close: None,
        }
    }

    pub fn active(mut self, active: bool) -> Self {
        self.active = active;
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

    /// Shows a `×` that calls `handler`. Middle-clicking the tab calls it too.
    pub fn on_close(mut self, handler: impl Fn(&mut Window, &mut App) + 'static) -> Self {
        self.on_close = Some(Box::new(handler));
        self
    }
}

impl RenderOnce for WindowTab {
    fn render(self, _window: &mut Window, cx: &mut App) -> impl IntoElement {
        let palette = palette(cx);
        let on_close = self.on_close.map(std::rc::Rc::new);
        h_flex()
            .id(self.id)
            .flex_none()
            .max_w(scaled(px(240.)))
            .gap(scaled(px(8.)))
            .px(scaled(px(10.)))
            .py(scaled(px(3.)))
            .rounded(scaled(tokens::CONTROL_RADIUS))
            .border(tokens::BORDER_WIDTH)
            .text_size(tokens::text_size(tokens::TEXT_SIZE_SMALL))
            .whitespace_nowrap()
            .map(|this| {
                if self.active {
                    this.bg(palette.background.hsla())
                        .border_color(palette.border.hsla())
                        .text_color(palette.text.hsla())
                } else {
                    this.border_color(gpui::transparent_black())
                        .text_color(palette.text_muted.hsla())
                        .hover(|style| {
                            style
                                .bg(palette.surface_2.hsla())
                                .text_color(palette.text.hsla())
                        })
                }
            })
            .child(
                div()
                    .min_w_0()
                    .overflow_hidden()
                    .text_ellipsis()
                    .child(self.label),
            )
            .when_some(on_close.clone(), |this, on_close| {
                this.child(
                    div()
                        .id("close")
                        .debug_selector(|| "window-tab-close".into())
                        .flex_none()
                        .px(scaled(px(2.)))
                        .rounded(scaled(tokens::KBD_RADIUS))
                        .opacity(0.6)
                        .cursor_pointer()
                        .hover(|style| style.opacity(1.).bg(palette.border.hsla()))
                        .on_mouse_down(MouseButton::Left, |_, window, _| window.prevent_default())
                        .on_click({
                            let on_close = on_close.clone();
                            move |_, window, cx| {
                                cx.stop_propagation();
                                on_close(window, cx);
                            }
                        })
                        .child("×"),
                )
                .on_mouse_up(MouseButton::Middle, move |_, window, cx| {
                    cx.stop_propagation();
                    on_close(window, cx);
                })
            })
            .when_some(self.tooltip, |this, tooltip| {
                this.tooltip(Tooltip::text(tooltip))
            })
            .when_some(self.on_click, |this, on_click| {
                this.cursor_pointer()
                    .on_mouse_down(MouseButton::Left, |_, window, _| window.prevent_default())
                    .on_click(move |event, window, cx| {
                        cx.stop_propagation();
                        on_click(event, window, cx);
                    })
            })
    }
}

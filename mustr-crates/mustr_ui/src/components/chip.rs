use crate::{components::ClickHandler, palette, tokens, tokens::scaled};
use gpui::{
    App, ClickEvent, ElementId, FontWeight, Hsla, InteractiveElement as _, IntoElement,
    MouseButton, ParentElement as _, RenderOnce, SharedString, StatefulInteractiveElement as _,
    Styled as _, Window, div, px,
};
use ui::{FluentBuilder as _, Tooltip, h_flex};

/// A rounded, outlined chip, such as the diff toolbar's `Hide whitespace` or the title bar's
/// connection status. An accent chip is filled with the accent color, for active filters.
#[derive(IntoElement)]
pub struct Chip {
    id: ElementId,
    label: SharedString,
    accent: bool,
    dot: Option<Hsla>,
    menu_indicator: bool,
    truncate: bool,
    tooltip: Option<SharedString>,
    on_click: Option<ClickHandler>,
}

impl Chip {
    pub fn new(id: impl Into<ElementId>, label: impl Into<SharedString>) -> Self {
        Self {
            id: id.into(),
            label: label.into(),
            accent: false,
            dot: None,
            menu_indicator: false,
            truncate: false,
            tooltip: None,
            on_click: None,
        }
    }

    /// Lets the chip shrink in a row that's too narrow, cutting its label with an ellipsis. Give
    /// it a tooltip with the whole label.
    pub fn truncate(mut self, truncate: bool) -> Self {
        self.truncate = truncate;
        self
    }

    /// Fills the chip with the accent color.
    pub fn accent(mut self, accent: bool) -> Self {
        self.accent = accent;
        self
    }

    /// Starts the chip with a status dot of this color.
    pub fn dot(mut self, color: impl Into<Hsla>) -> Self {
        self.dot = Some(color.into());
        self
    }

    /// Ends the chip with `▾`, for chips that open a menu.
    pub fn menu_indicator(mut self, menu_indicator: bool) -> Self {
        self.menu_indicator = menu_indicator;
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

impl RenderOnce for Chip {
    fn render(self, _window: &mut Window, cx: &mut App) -> impl IntoElement {
        let palette = palette(cx);
        let (border, background, text) = if self.accent {
            (
                palette.accent.hsla().opacity(0.5),
                palette.accent_soft.hsla(),
                palette.accent.hsla(),
            )
        } else {
            (
                palette.border.hsla(),
                gpui::transparent_black(),
                palette.text_muted.hsla(),
            )
        };
        let hover_background = if self.accent {
            palette.accent.hsla().opacity(0.22)
        } else {
            palette.surface_2.hsla()
        };
        let hover_text = if self.accent {
            palette.accent.hsla()
        } else {
            palette.text.hsla()
        };

        h_flex()
            .id(self.id)
            .map(|this| {
                if self.truncate {
                    this.flex_shrink_1().min_w_0()
                } else {
                    this.flex_none()
                }
            })
            .gap(scaled(px(5.)))
            .px(scaled(px(8.)))
            .py(scaled(px(1.)))
            .rounded_full()
            .border(tokens::BORDER_WIDTH)
            .border_color(border)
            .bg(background)
            .text_color(text)
            .text_size(tokens::text_size(tokens::TEXT_SIZE_SMALL))
            .whitespace_nowrap()
            .when_some(self.dot, |this, color| {
                this.child(
                    div()
                        .flex_none()
                        .size(scaled(px(6.)))
                        .rounded_full()
                        .bg(color),
                )
            })
            .map(|this| {
                if self.truncate {
                    this.child(
                        div()
                            .min_w_0()
                            .overflow_hidden()
                            .text_ellipsis()
                            .child(self.label),
                    )
                } else {
                    this.child(self.label)
                }
            })
            .when(self.menu_indicator, |this| this.child("▾"))
            .when_some(self.tooltip, |this, tooltip| {
                this.tooltip(Tooltip::text(tooltip))
            })
            .when_some(self.on_click, |this, on_click| {
                this.cursor_pointer()
                    .hover(move |style| style.bg(hover_background).text_color(hover_text))
                    .on_mouse_down(MouseButton::Left, |_, window, _| window.prevent_default())
                    .on_click(move |event, window, cx| {
                        cx.stop_propagation();
                        on_click(event, window, cx);
                    })
            })
    }
}

/// A small filled label, such as a pull request's `Open` state or a comment's `Pending`.
#[derive(IntoElement)]
pub struct Pill {
    label: SharedString,
    background: PillBackground,
}

enum PillBackground {
    Tint(Hsla),
    Fill(Hsla),
}

impl Pill {
    /// A pill in `color`, on a faint fill of the same color.
    pub fn tinted(label: impl Into<SharedString>, color: impl Into<Hsla>) -> Self {
        Self {
            label: label.into(),
            background: PillBackground::Tint(color.into()),
        }
    }

    /// A pill with the theme's text color on `background`.
    pub fn filled(label: impl Into<SharedString>, background: impl Into<Hsla>) -> Self {
        Self {
            label: label.into(),
            background: PillBackground::Fill(background.into()),
        }
    }
}

impl RenderOnce for Pill {
    fn render(self, _window: &mut Window, cx: &mut App) -> impl IntoElement {
        let palette = palette(cx);
        let (background, text) = match self.background {
            PillBackground::Tint(color) => (color.opacity(0.22), color),
            PillBackground::Fill(background) => (background, palette.text.hsla()),
        };
        div()
            .flex_none()
            .px(scaled(px(7.)))
            .py(scaled(px(1.)))
            .rounded_full()
            .bg(background)
            .text_color(text)
            .text_size(tokens::text_size(tokens::TEXT_SIZE_XSMALL))
            .font_weight(FontWeight::SEMIBOLD)
            .whitespace_nowrap()
            .child(self.label)
    }
}

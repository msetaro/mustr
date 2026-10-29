use crate::{
    ChildPosition, RoundedCorners as _, components::ClickHandler, corners::inner_radius, palette,
    tokens, tokens::Palette, tokens::scaled,
};
use gpui::{
    AnyElement, App, Axis, ClickEvent, ElementId, FontWeight, Hsla, InteractiveElement as _,
    IntoElement, MouseButton, ParentElement as _, Pixels, RenderOnce, SharedString,
    StatefulInteractiveElement as _, Styled as _, Window, div, px,
};
use ui::{FluentBuilder as _, Tooltip, h_flex};

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum ButtonStyle {
    /// An outlined button on the raised surface color, such as `Open on GitHub`.
    #[default]
    Default,
    /// A button filled with the accent color, such as `Submit review`.
    Primary,
}

struct ButtonColors {
    background: Hsla,
    hover_background: Hsla,
    border: Hsla,
    text: Hsla,
    /// The fill behind a count, and the line between the halves of a split button.
    subtle: Hsla,
}

impl ButtonStyle {
    fn colors(self, palette: &Palette) -> ButtonColors {
        match self {
            Self::Default => ButtonColors {
                background: palette.surface_2.hsla(),
                hover_background: palette.border.hsla(),
                border: palette.border.hsla(),
                text: palette.text.hsla(),
                subtle: palette.border.hsla(),
            },
            Self::Primary => ButtonColors {
                background: palette.accent.hsla(),
                hover_background: palette.accent.hsla().opacity(0.85),
                border: gpui::transparent_black(),
                text: palette.accent_foreground.hsla(),
                subtle: gpui::black().opacity(0.18),
            },
        }
    }
}

/// A button with a text label and an optional count, such as `Submit review 3`.
#[derive(IntoElement)]
pub struct TextButton {
    id: ElementId,
    label: SharedString,
    style: ButtonStyle,
    count: Option<SharedString>,
    disabled: bool,
    tooltip: Option<SharedString>,
    menu_open: bool,
    on_click: Option<ClickHandler>,
}

impl TextButton {
    pub fn new(id: impl Into<ElementId>, label: impl Into<SharedString>) -> Self {
        Self {
            id: id.into(),
            label: label.into(),
            style: ButtonStyle::Default,
            count: None,
            disabled: false,
            tooltip: None,
            menu_open: false,
            on_click: None,
        }
    }

    pub fn style(mut self, style: ButtonStyle) -> Self {
        self.style = style;
        self
    }

    pub fn count(mut self, count: impl Into<SharedString>) -> Self {
        self.count = Some(count.into());
        self
    }

    pub fn disabled(mut self, disabled: bool) -> Self {
        self.disabled = disabled;
        self
    }

    pub fn tooltip(mut self, tooltip: impl Into<SharedString>) -> Self {
        self.tooltip = Some(tooltip.into());
        self
    }

    /// Whether the menu the button opens is open: its tooltip doesn't show then, so one that
    /// was pending when the click came doesn't cover the menu.
    pub fn menu_open(mut self, menu_open: bool) -> Self {
        self.menu_open = menu_open;
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

impl RenderOnce for TextButton {
    fn render(self, window: &mut Window, cx: &mut App) -> impl IntoElement {
        let colors = self.style.colors(palette(cx));
        let content = button_content(self.label, self.count, &colors);
        let tooltip = self.tooltip.filter(|_| !self.menu_open);
        button_frame(self.style, &colors)
            .child(
                clickable_part(self.id, &colors, self.disabled, tooltip, self.on_click)
                    .rounded_at(ChildPosition::Only, Axis::Horizontal, part_radius(window))
                    .px(scaled(px(10.)))
                    .child(content),
            )
            .when(self.disabled, |this| this.opacity(0.5))
    }
}

/// A button with a main action and a `▾` that opens a menu of related actions, such as
/// `Submit review ▾` (comment, approve, or request changes).
#[derive(IntoElement)]
pub struct SplitButton {
    id: ElementId,
    label: SharedString,
    style: ButtonStyle,
    count: Option<SharedString>,
    disabled: bool,
    tooltip: Option<SharedString>,
    menu_tooltip: Option<SharedString>,
    menu_open: bool,
    on_click: Option<ClickHandler>,
    on_menu_click: Option<ClickHandler>,
}

impl SplitButton {
    pub fn new(id: impl Into<ElementId>, label: impl Into<SharedString>) -> Self {
        Self {
            id: id.into(),
            label: label.into(),
            style: ButtonStyle::Default,
            count: None,
            disabled: false,
            tooltip: None,
            menu_tooltip: None,
            menu_open: false,
            on_click: None,
            on_menu_click: None,
        }
    }

    pub fn style(mut self, style: ButtonStyle) -> Self {
        self.style = style;
        self
    }

    pub fn count(mut self, count: impl Into<SharedString>) -> Self {
        self.count = Some(count.into());
        self
    }

    pub fn disabled(mut self, disabled: bool) -> Self {
        self.disabled = disabled;
        self
    }

    pub fn tooltip(mut self, tooltip: impl Into<SharedString>) -> Self {
        self.tooltip = Some(tooltip.into());
        self
    }

    pub fn menu_tooltip(mut self, tooltip: impl Into<SharedString>) -> Self {
        self.menu_tooltip = Some(tooltip.into());
        self
    }

    /// Whether the `▾`'s menu is open: the button's tooltips don't show then, so one that was
    /// pending when the click came doesn't cover the menu.
    pub fn menu_open(mut self, menu_open: bool) -> Self {
        self.menu_open = menu_open;
        self
    }

    /// The main action.
    pub fn on_click(
        mut self,
        handler: impl Fn(&ClickEvent, &mut Window, &mut App) + 'static,
    ) -> Self {
        self.on_click = Some(Box::new(handler));
        self
    }

    /// The `▾`, which should open the menu.
    pub fn on_menu_click(
        mut self,
        handler: impl Fn(&ClickEvent, &mut Window, &mut App) + 'static,
    ) -> Self {
        self.on_menu_click = Some(Box::new(handler));
        self
    }
}

impl RenderOnce for SplitButton {
    fn render(self, window: &mut Window, cx: &mut App) -> impl IntoElement {
        let radius = part_radius(window);
        let colors = self.style.colors(palette(cx));
        let content = button_content(self.label, self.count, &colors);
        let menu_id = ElementId::NamedChild(self.id.clone().into(), "menu".into());
        let (tooltip, menu_tooltip) = if self.menu_open {
            (None, None)
        } else {
            (self.tooltip, self.menu_tooltip)
        };
        button_frame(self.style, &colors)
            .child(
                clickable_part(self.id, &colors, self.disabled, tooltip, self.on_click)
                    .debug_selector(|| "split-button-main".into())
                    .rounded_at(ChildPosition::First, Axis::Horizontal, radius)
                    .pl(scaled(px(10.)))
                    .pr(scaled(px(8.)))
                    .child(content),
            )
            .child(div().w(tokens::BORDER_WIDTH).h_full().bg(colors.subtle))
            .child(
                clickable_part(
                    menu_id,
                    &colors,
                    self.disabled,
                    menu_tooltip,
                    self.on_menu_click,
                )
                .debug_selector(|| "split-button-menu".into())
                .rounded_at(ChildPosition::Last, Axis::Horizontal, radius)
                .px(scaled(px(6.)))
                .child("▾"),
            )
            .when(self.disabled, |this| this.opacity(0.5))
    }
}

fn button_frame(style: ButtonStyle, colors: &ButtonColors) -> gpui::Div {
    h_flex()
        .flex_none()
        .items_stretch()
        .rounded(scaled(tokens::CONTROL_RADIUS))
        .border(tokens::BORDER_WIDTH)
        .border_color(colors.border)
        .bg(colors.background)
        .overflow_hidden()
        .text_color(colors.text)
        .text_size(tokens::text_size(tokens::TEXT_SIZE_SMALL))
        .when(style == ButtonStyle::Primary, |this| {
            this.font_weight(FontWeight::SEMIBOLD)
        })
        .whitespace_nowrap()
}

/// The radius of a clickable part's hover fill on the corners it shares with the frame, which
/// GPUI doesn't clip to its rounded corners.
fn part_radius(window: &Window) -> Pixels {
    inner_radius(scaled(tokens::CONTROL_RADIUS).to_pixels(window.rem_size()))
}

fn button_content(
    label: SharedString,
    count: Option<SharedString>,
    colors: &ButtonColors,
) -> AnyElement {
    h_flex()
        .child(label)
        .when_some(count, |this, count| {
            this.child(
                div()
                    .ml(scaled(px(5.)))
                    .px(scaled(px(5.)))
                    .rounded(scaled(px(8.)))
                    .bg(colors.subtle)
                    .text_size(tokens::text_size(tokens::TEXT_SIZE_TINY))
                    .child(count),
            )
        })
        .into_any_element()
}

fn clickable_part(
    id: ElementId,
    colors: &ButtonColors,
    disabled: bool,
    tooltip: Option<SharedString>,
    on_click: Option<ClickHandler>,
) -> gpui::Stateful<gpui::Div> {
    let hover_background = colors.hover_background;
    h_flex()
        .id(id)
        .py(scaled(px(3.)))
        .when_some(tooltip, |this, tooltip| {
            this.tooltip(Tooltip::text(tooltip))
        })
        .when_some(on_click.filter(|_| !disabled), |this, on_click| {
            this.cursor_pointer()
                .hover(move |style| style.bg(hover_background))
                .on_mouse_down(MouseButton::Left, |_, window, _| window.prevent_default())
                .on_click(move |event, window, cx| {
                    cx.stop_propagation();
                    on_click(event, window, cx);
                })
        })
}

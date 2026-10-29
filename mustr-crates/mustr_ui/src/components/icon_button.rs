//! Buttons for toolbars: [`IconOnlyButton`] and [`IconSplitButton`], outlined like
//! [`super::TextButton`] and [`super::SplitButton`] but showing an icon, and [`QuietButton`],
//! borderless until hovered, for what a toolbar shows rather than does (a branch, a pull
//! request).
//!
//! They're as tall as `TextButton` and `SplitButton` (each holds a line of their text, so the
//! line height is the same), so a row of them lines up. Each hover background is rounded like
//! the button, or like its own corners of a split button.

use super::ClickHandler;
use crate::{palette, tokens, tokens::Palette, tokens::scaled};
use gpui::{
    AnyElement, AnyView, App, ClickEvent, ElementId, InteractiveElement as _, IntoElement,
    MouseButton, ParentElement as _, RenderOnce, SharedString, StatefulInteractiveElement as _,
    Styled as _, Window, div, px,
};
use std::rc::Rc;
use ui::{FluentBuilder as _, h_flex};

type TooltipBuilder = Rc<dyn Fn(&mut Window, &mut App) -> AnyView>;

/// A zero-width line of the buttons' text, so an icon-only button is exactly as tall as a
/// labelled one.
const STRUT: &str = "\u{200B}";

/// The colors of an outlined button: `ButtonStyle::Default`'s.
struct Outlined {
    background: gpui::Hsla,
    hover_background: gpui::Hsla,
    border: gpui::Hsla,
    text: gpui::Hsla,
}

impl Outlined {
    fn new(palette: &Palette) -> Self {
        Self {
            background: palette.surface_2.hsla(),
            hover_background: palette.border.hsla(),
            border: palette.border.hsla(),
            text: palette.text.hsla(),
        }
    }
}

/// The outer frame every button here shares: the text size and a 1px border (transparent for a
/// quiet button), rounded like mustr's controls.
fn frame(id: ElementId) -> gpui::Stateful<gpui::Div> {
    h_flex()
        .id(id)
        .flex_none()
        .items_center()
        .rounded(scaled(tokens::CONTROL_RADIUS))
        .border(tokens::BORDER_WIDTH)
        .text_size(tokens::text_size(tokens::TEXT_SIZE_SMALL))
        .whitespace_nowrap()
}

fn clickable(
    this: gpui::Stateful<gpui::Div>,
    hover_background: gpui::Hsla,
    disabled: bool,
    tooltip: Option<TooltipBuilder>,
    on_click: Option<ClickHandler>,
) -> gpui::Stateful<gpui::Div> {
    this.when_some(tooltip, |this, tooltip| {
        this.tooltip(move |window, cx| tooltip(window, cx))
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

/// An outlined button that shows only an icon, such as the thread toolbar's Terminal. Give it a
/// tooltip that names it.
#[derive(IntoElement)]
pub struct IconOnlyButton {
    id: ElementId,
    icon: AnyElement,
    disabled: bool,
    tooltip: Option<TooltipBuilder>,
    menu_open: bool,
    on_click: Option<ClickHandler>,
}

impl IconOnlyButton {
    pub fn new(id: impl Into<ElementId>, icon: impl IntoElement) -> Self {
        Self {
            id: id.into(),
            icon: icon.into_any_element(),
            disabled: false,
            tooltip: None,
            menu_open: false,
            on_click: None,
        }
    }

    pub fn disabled(mut self, disabled: bool) -> Self {
        self.disabled = disabled;
        self
    }

    /// Its tooltip, such as `ui::Tooltip::text(…)` or `ui::Tooltip::for_action_title(…)`.
    pub fn tooltip(mut self, tooltip: impl Fn(&mut Window, &mut App) -> AnyView + 'static) -> Self {
        self.tooltip = Some(Rc::new(tooltip));
        self
    }

    /// Whether the menu it opens is open: its tooltip doesn't show then.
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

impl RenderOnce for IconOnlyButton {
    fn render(self, _window: &mut Window, cx: &mut App) -> impl IntoElement {
        let colors = Outlined::new(palette(cx));
        let tooltip = self.tooltip.filter(|_| !self.menu_open);
        clickable(
            frame(self.id),
            colors.hover_background,
            self.disabled,
            tooltip,
            self.on_click,
        )
        .border_color(colors.border)
        .bg(colors.background)
        .text_color(colors.text)
        .py(scaled(px(3.)))
        .px(scaled(px(6.)))
        .gap_0()
        .child(self.icon)
        .child(STRUT)
        .when(self.disabled, |this| this.opacity(0.5))
    }
}

/// An outlined button with an icon for its main action and a `▾` for a menu, such as the thread
/// toolbar's Open in (the editor's icon, then the editors).
#[derive(IntoElement)]
pub struct IconSplitButton {
    id: ElementId,
    icon: AnyElement,
    disabled: bool,
    tooltip: Option<TooltipBuilder>,
    menu_tooltip: Option<TooltipBuilder>,
    menu_open: bool,
    on_click: Option<ClickHandler>,
    on_menu_click: Option<ClickHandler>,
}

impl IconSplitButton {
    pub fn new(id: impl Into<ElementId>, icon: impl IntoElement) -> Self {
        Self {
            id: id.into(),
            icon: icon.into_any_element(),
            disabled: false,
            tooltip: None,
            menu_tooltip: None,
            menu_open: false,
            on_click: None,
            on_menu_click: None,
        }
    }

    pub fn disabled(mut self, disabled: bool) -> Self {
        self.disabled = disabled;
        self
    }

    pub fn tooltip(mut self, tooltip: impl Fn(&mut Window, &mut App) -> AnyView + 'static) -> Self {
        self.tooltip = Some(Rc::new(tooltip));
        self
    }

    pub fn menu_tooltip(
        mut self,
        tooltip: impl Fn(&mut Window, &mut App) -> AnyView + 'static,
    ) -> Self {
        self.menu_tooltip = Some(Rc::new(tooltip));
        self
    }

    /// Whether the `▾`'s menu is open: the button's tooltips don't show then.
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

    pub fn on_menu_click(
        mut self,
        handler: impl Fn(&ClickEvent, &mut Window, &mut App) + 'static,
    ) -> Self {
        self.on_menu_click = Some(Box::new(handler));
        self
    }
}

impl RenderOnce for IconSplitButton {
    fn render(self, _window: &mut Window, cx: &mut App) -> impl IntoElement {
        let colors = Outlined::new(palette(cx));
        let radius = scaled(tokens::CONTROL_RADIUS);
        let menu_id = ElementId::NamedChild(self.id.clone().into(), "menu".into());
        let (tooltip, menu_tooltip) = if self.menu_open {
            (None, None)
        } else {
            (self.tooltip, self.menu_tooltip)
        };
        let main = clickable(
            h_flex().id(self.id.clone()),
            colors.hover_background,
            self.disabled,
            tooltip,
            self.on_click,
        )
        .debug_selector(|| "split-button-main".into())
        .h_full()
        .rounded_l(radius)
        .py(scaled(px(3.)))
        .pl(scaled(px(6.)))
        .pr(scaled(px(5.)))
        .child(self.icon)
        .child(STRUT);
        let menu = clickable(
            h_flex().id(menu_id),
            colors.hover_background,
            self.disabled,
            menu_tooltip,
            self.on_menu_click,
        )
        .debug_selector(|| "split-button-menu".into())
        .h_full()
        .rounded_r(radius)
        .py(scaled(px(3.)))
        .px(scaled(px(5.)))
        .text_color(colors.text)
        .child("▾");
        frame(self.id)
            .items_stretch()
            .border_color(colors.border)
            .bg(colors.background)
            .text_color(colors.text)
            .child(main)
            .child(div().w(tokens::BORDER_WIDTH).bg(colors.border))
            .child(menu)
            .when(self.disabled, |this| this.opacity(0.5))
    }
}

/// A borderless button that shows something and opens it, such as the thread toolbar's branch
/// (`● main 3 ▾`). Its label is in the text color, or muted; a hover fills it.
#[derive(IntoElement)]
pub struct QuietButton {
    id: ElementId,
    label: SharedString,
    start: Vec<AnyElement>,
    end: Vec<AnyElement>,
    count: Option<SharedString>,
    muted: bool,
    menu_indicator: bool,
    truncate: bool,
    tooltip: Option<TooltipBuilder>,
    menu_open: bool,
    on_click: Option<ClickHandler>,
}

impl QuietButton {
    pub fn new(id: impl Into<ElementId>, label: impl Into<SharedString>) -> Self {
        Self {
            id: id.into(),
            label: label.into(),
            start: Vec::new(),
            end: Vec::new(),
            count: None,
            muted: false,
            menu_indicator: false,
            truncate: false,
            tooltip: None,
            menu_open: false,
            on_click: None,
        }
    }

    /// Something before the label, such as a status dot.
    pub fn start(mut self, element: impl IntoElement) -> Self {
        self.start.push(element.into_any_element());
        self
    }

    /// Something after the label (and its count), such as a status icon.
    pub fn end(mut self, element: impl IntoElement) -> Self {
        self.end.push(element.into_any_element());
        self
    }

    /// A quiet count after the label, such as the files a branch has changed.
    pub fn count(mut self, count: impl Into<SharedString>) -> Self {
        self.count = Some(count.into());
        self
    }

    /// Shows the label in the muted text color.
    pub fn muted(mut self, muted: bool) -> Self {
        self.muted = muted;
        self
    }

    /// Ends the button with `▾`, for buttons that open a menu.
    pub fn menu_indicator(mut self, menu_indicator: bool) -> Self {
        self.menu_indicator = menu_indicator;
        self
    }

    /// Lets the button shrink in a row that's too narrow, cutting its label with an ellipsis.
    pub fn truncate(mut self, truncate: bool) -> Self {
        self.truncate = truncate;
        self
    }

    pub fn tooltip(mut self, tooltip: impl Fn(&mut Window, &mut App) -> AnyView + 'static) -> Self {
        self.tooltip = Some(Rc::new(tooltip));
        self
    }

    /// Whether the menu it opens is open: it stays filled, and its tooltip doesn't show.
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

impl RenderOnce for QuietButton {
    fn render(self, _window: &mut Window, cx: &mut App) -> impl IntoElement {
        let palette = palette(cx);
        let hover_background = palette.surface_2.hsla();
        let muted = palette.text_muted.hsla();
        let tooltip = self.tooltip.filter(|_| !self.menu_open);
        let label = if self.truncate {
            div()
                .min_w_0()
                .overflow_hidden()
                .text_ellipsis()
                .child(self.label)
                .into_any_element()
        } else {
            self.label.into_any_element()
        };
        clickable(
            frame(self.id),
            hover_background,
            false,
            tooltip,
            self.on_click,
        )
        .when(self.truncate, |this| this.flex_shrink_1().min_w_0())
        .border_color(gpui::transparent_black())
        .when(self.menu_open, |this| this.bg(hover_background))
        .text_color(if self.muted {
            muted
        } else {
            palette.text.hsla()
        })
        .py(scaled(px(3.)))
        .px(scaled(px(7.)))
        .gap(scaled(px(5.)))
        .children(self.start)
        .child(label)
        .when_some(self.count, |this, count| {
            this.child(
                div()
                    .flex_none()
                    .px(scaled(px(5.)))
                    .rounded(scaled(px(8.)))
                    .bg(palette.surface_2.hsla())
                    .text_color(muted)
                    .text_size(tokens::text_size(tokens::TEXT_SIZE_XSMALL))
                    .child(count),
            )
        })
        .children(self.end)
        .when(self.menu_indicator, |this| {
            this.child(div().flex_none().text_color(muted).child("▾"))
        })
        .child(STRUT)
    }
}

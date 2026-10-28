use crate::{palette, tokens};
use gpui::{
    AnyElement, App, IntoElement, ParentElement, Refineable as _, RenderOnce, StyleRefinement,
    Styled, Window, div, px,
};
use smallvec::SmallVec;

/// A bordered box with 8 px corners on the surface color, for comment threads, agent
/// messages, and the home view's sections.
///
/// Styles set on the card (padding, background, and so on) replace its defaults.
#[derive(IntoElement, Default)]
pub struct Card {
    style: StyleRefinement,
    children: SmallVec<[AnyElement; 2]>,
}

impl Card {
    pub fn new() -> Self {
        Self::default()
    }
}

impl ParentElement for Card {
    fn extend(&mut self, elements: impl IntoIterator<Item = AnyElement>) {
        self.children.extend(elements)
    }
}

impl Styled for Card {
    fn style(&mut self) -> &mut StyleRefinement {
        &mut self.style
    }
}

impl RenderOnce for Card {
    fn render(self, _window: &mut Window, cx: &mut App) -> impl IntoElement {
        let palette = palette(cx);
        let mut card = div()
            .flex()
            .flex_col()
            .px(px(10.))
            .py(px(8.))
            .rounded(tokens::CORNER_RADIUS)
            .border(tokens::BORDER_WIDTH)
            .border_color(palette.border.hsla())
            .bg(palette.surface.hsla());
        card.style().refine(&self.style);
        card.children(self.children)
    }
}

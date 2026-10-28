use crate::{components::Kbd, palette, tokens, tokens::scaled};
use gpui::{
    App, IntoElement, ParentElement as _, RenderOnce, SharedString, Styled as _, Window, div, px,
};
use ui::h_flex;

/// A key hint for the status bar: key caps followed by what they do, as in `j k hunk`.
#[derive(IntoElement)]
pub struct StatusHint {
    keys: Vec<SharedString>,
    label: SharedString,
}

impl StatusHint {
    pub fn new(
        keys: impl IntoIterator<Item = impl Into<SharedString>>,
        label: impl Into<SharedString>,
    ) -> Self {
        Self {
            keys: keys.into_iter().map(Into::into).collect(),
            label: label.into(),
        }
    }
}

impl RenderOnce for StatusHint {
    fn render(self, _window: &mut Window, cx: &mut App) -> impl IntoElement {
        let palette = palette(cx);
        h_flex()
            .flex_none()
            .gap(scaled(px(3.)))
            .children(self.keys.into_iter().map(Kbd::new))
            .child(
                div()
                    .ml(scaled(px(2.)))
                    .text_size(tokens::text_size(tokens::TEXT_SIZE_XSMALL))
                    .text_color(palette.text_muted.hsla())
                    .child(self.label),
            )
    }
}

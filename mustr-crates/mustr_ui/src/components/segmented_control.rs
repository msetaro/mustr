use crate::{palette, tokens};
use gpui::{
    App, ElementId, InteractiveElement as _, IntoElement, MouseButton, ParentElement as _,
    RenderOnce, SharedString, StatefulInteractiveElement as _, Styled as _, Window, div, px,
};
use std::rc::Rc;
use ui::{FluentBuilder as _, Tooltip, h_flex};

type SelectHandler = Rc<dyn Fn(usize, &mut Window, &mut App) + 'static>;

/// A row of mutually exclusive options in one outlined box, such as `Unified | Split`.
#[derive(IntoElement)]
pub struct SegmentedControl {
    id: ElementId,
    segments: Vec<Segment>,
    selected: usize,
    on_select: Option<SelectHandler>,
}

struct Segment {
    label: SharedString,
    tooltip: Option<SharedString>,
}

impl SegmentedControl {
    pub fn new(id: impl Into<ElementId>) -> Self {
        Self {
            id: id.into(),
            segments: Vec::new(),
            selected: 0,
            on_select: None,
        }
    }

    pub fn segment(mut self, label: impl Into<SharedString>) -> Self {
        self.segments.push(Segment {
            label: label.into(),
            tooltip: None,
        });
        self
    }

    pub fn segment_with_tooltip(
        mut self,
        label: impl Into<SharedString>,
        tooltip: impl Into<SharedString>,
    ) -> Self {
        self.segments.push(Segment {
            label: label.into(),
            tooltip: Some(tooltip.into()),
        });
        self
    }

    pub fn selected(mut self, index: usize) -> Self {
        self.selected = index;
        self
    }

    /// Called with the index of a segment when it's clicked, whether or not it's selected.
    pub fn on_select(mut self, handler: impl Fn(usize, &mut Window, &mut App) + 'static) -> Self {
        self.on_select = Some(Rc::new(handler));
        self
    }
}

impl RenderOnce for SegmentedControl {
    fn render(self, _window: &mut Window, cx: &mut App) -> impl IntoElement {
        let palette = palette(cx);
        let selected = self.selected;
        let on_select = self.on_select;
        h_flex()
            .id(self.id)
            .flex_none()
            .border(tokens::BORDER_WIDTH)
            .border_color(palette.border.hsla())
            .rounded(tokens::CONTROL_RADIUS)
            .overflow_hidden()
            .text_size(tokens::text_size(tokens::TEXT_SIZE_SMALL))
            .children(
                self.segments
                    .into_iter()
                    .enumerate()
                    .map(|(index, segment)| {
                        let is_selected = index == selected;
                        let label = segment.label.clone();
                        div()
                            .id(("segment", index))
                            .debug_selector(move || format!("segment-{label}"))
                            .px(px(8.))
                            .py(px(1.))
                            .whitespace_nowrap()
                            .map(|this| {
                                if is_selected {
                                    this.bg(palette.surface_2.hsla())
                                        .text_color(palette.text.hsla())
                                } else {
                                    this.text_color(palette.text_muted.hsla())
                                        .hover(|style| style.text_color(palette.text.hsla()))
                                }
                            })
                            .when_some(segment.tooltip, |this, tooltip| {
                                this.tooltip(Tooltip::text(tooltip))
                            })
                            .when_some(on_select.clone(), |this, on_select| {
                                this.cursor_pointer()
                                    .on_mouse_down(MouseButton::Left, |_, window, _| {
                                        window.prevent_default()
                                    })
                                    .on_click(move |_, window, cx| {
                                        cx.stop_propagation();
                                        on_select(index, window, cx);
                                    })
                            })
                            .child(segment.label)
                    }),
            )
    }
}

//! The Settings page's building blocks: a setting's row (its name, a one-line description, and its
//! control), the groups of rows the page draws as cards, the search that filters them, and the
//! controls themselves, in mustr's look.

use editor::Editor;
use gpui::{
    AnyElement, App, Entity, FontWeight, InteractiveElement as _, IntoElement, ParentElement as _,
    SharedString, StatefulInteractiveElement as _, Styled as _, Window, div, px,
};
use mustr_ui::{ButtonStyle, Card, SegmentedControl, TextButton, palette, tokens, tokens::scaled};
use std::rc::Rc;
use ui::{
    Color, ContextMenu, DropdownMenu, DropdownStyle, IconButton, IconName, IconPosition, IconSize,
    Label, LabelCommon as _, LabelSize, Switch, ToggleState, Tooltip, prelude::*,
};

/// What a control does when it's used.
pub(crate) type Callback = Rc<dyn Fn(&mut Window, &mut App)>;

/// The search box's words, lowercased. A setting matches when each word is in its name, its
/// description, its keywords, or its group's or section's name.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub(crate) struct Query(Vec<String>);

impl Query {
    pub fn new(text: &str) -> Self {
        Self(text.split_whitespace().map(str::to_lowercase).collect())
    }

    pub fn is_empty(&self) -> bool {
        self.0.is_empty()
    }

    pub fn matches(&self, texts: &[&str]) -> bool {
        self.0.iter().all(|word| {
            texts
                .iter()
                .any(|text| text.to_lowercase().contains(word.as_str()))
        })
    }
}

enum Body {
    /// The control, beside the name (or under it, when the page is narrow).
    Beside(AnyElement),
    /// Something wide, such as a text box, under the name and description.
    Below(AnyElement),
    /// Drawn as it is, in place of a row.
    Whole(AnyElement),
}

/// One setting.
pub(crate) struct SettingRow {
    selector: SharedString,
    title: SharedString,
    description: SharedString,
    keywords: &'static str,
    body: Body,
}

impl SettingRow {
    pub fn new(
        selector: impl Into<SharedString>,
        title: impl Into<SharedString>,
        description: impl Into<SharedString>,
        control: impl IntoElement,
    ) -> Self {
        Self {
            selector: selector.into(),
            title: title.into(),
            description: description.into(),
            keywords: "",
            body: Body::Beside(control.into_any_element()),
        }
    }

    pub fn below(
        selector: impl Into<SharedString>,
        title: impl Into<SharedString>,
        description: impl Into<SharedString>,
        content: impl IntoElement,
    ) -> Self {
        Self {
            body: Body::Below(content.into_any_element()),
            ..Self::new(selector, title, description, div())
        }
    }

    /// Something that draws itself, such as a card of its own. `title` and `keywords` are what
    /// the search finds it by.
    pub fn whole(
        selector: impl Into<SharedString>,
        title: impl Into<SharedString>,
        element: impl IntoElement,
    ) -> Self {
        Self {
            body: Body::Whole(element.into_any_element()),
            ..Self::new(selector, title, "", div())
        }
    }

    /// More words the search finds the setting by.
    pub fn keywords(mut self, keywords: &'static str) -> Self {
        self.keywords = keywords;
        self
    }

    fn matches(&self, context: &[&str], query: &Query) -> bool {
        let mut texts = vec![
            self.title.as_ref(),
            self.description.as_ref(),
            self.keywords,
        ];
        texts.extend_from_slice(context);
        query.matches(&texts)
    }

    fn render(self, first: bool, cx: &App) -> AnyElement {
        let palette = palette(cx);
        let selector = self.selector.to_string();
        let text = v_flex()
            .gap_0p5()
            .child(Label::new(self.title).weight(FontWeight::MEDIUM))
            .when(!self.description.is_empty(), |this| {
                this.child(muted(self.description))
            });
        let row = match self.body {
            Body::Whole(element) => {
                return div()
                    .w_full()
                    .debug_selector(move || selector)
                    .child(element)
                    .into_any_element();
            }
            Body::Beside(control) => h_flex()
                .w_full()
                .flex_wrap()
                .items_center()
                .justify_between()
                .gap_x_4()
                .gap_y_1()
                .child(text.flex_1().min_w(scaled(px(260.))))
                .child(div().flex_none().max_w_full().child(control)),
            Body::Below(content) => v_flex()
                .w_full()
                .gap_1()
                .child(text)
                .child(div().w_full().child(content)),
        };
        row.debug_selector(move || selector)
            .px_2()
            .py(scaled(px(9.)))
            .when(!first, |this| {
                this.border_t(tokens::BORDER_WIDTH)
                    .border_color(palette.border.hsla())
            })
            .into_any_element()
    }
}

/// Settings that belong together, drawn as a card under the group's name.
pub(crate) struct Group {
    title: SharedString,
    note: Option<SharedString>,
    rows: Vec<SettingRow>,
    /// The rows draw themselves, without the group's card.
    bare: bool,
}

impl Group {
    pub fn new(title: impl Into<SharedString>, rows: Vec<SettingRow>) -> Self {
        Self {
            title: title.into(),
            note: None,
            rows,
            bare: false,
        }
    }

    pub fn note(mut self, note: impl Into<SharedString>) -> Self {
        self.note = Some(note.into());
        self
    }

    pub fn bare(mut self) -> Self {
        self.bare = true;
        self
    }

    pub fn rows(&self) -> &[SettingRow] {
        &self.rows
    }

    /// The group with only the rows `query` finds (all of them when it finds the group's name),
    /// or `None` when it finds none. `section` is the name of the page's section.
    pub fn filter(mut self, section: &str, query: &Query) -> Option<Self> {
        if query.is_empty() {
            return Some(self);
        }
        let title = self.title.clone();
        let note = self.note.clone().unwrap_or_default();
        self.rows
            .retain(|row| row.matches(&[title.as_ref(), note.as_ref(), section], query));
        (!self.rows.is_empty()).then_some(self)
    }

    pub fn render(self, cx: &App) -> AnyElement {
        let header = (!self.title.is_empty()).then(|| {
            h_flex()
                .gap_2()
                .items_baseline()
                .child(Label::new(self.title.clone()).weight(FontWeight::SEMIBOLD))
                .children(
                    self.note
                        .clone()
                        .map(|note| muted(note).size(LabelSize::XSmall)),
                )
        });
        let rows = self
            .rows
            .into_iter()
            .enumerate()
            .map(|(index, row)| row.render(index == 0, cx));
        v_flex()
            .w_full()
            .gap_1p5()
            .children(header)
            .map(|this| {
                if self.bare {
                    this.child(v_flex().w_full().gap_3().children(rows))
                } else {
                    this.child(
                        Card::new()
                            .px(scaled(px(10.)))
                            .py(scaled(px(2.)))
                            .children(rows),
                    )
                }
            })
            .into_any_element()
    }
}

pub(crate) fn muted(text: impl Into<SharedString>) -> Label {
    Label::new(text.into())
        .size(LabelSize::Small)
        .color(Color::Muted)
}

fn toggle_state(on: bool) -> ToggleState {
    if on {
        ToggleState::Selected
    } else {
        ToggleState::Unselected
    }
}

/// An on and off switch. `blocked` says why it can't be changed, if it can't.
pub(crate) fn switch(
    id: &'static str,
    on: bool,
    blocked: Option<SharedString>,
    on_change: impl Fn(bool, &mut Window, &mut App) + 'static,
) -> AnyElement {
    div()
        .id(SharedString::from(format!("{id}-wrapper")))
        .debug_selector(move || id.to_string())
        .when_some(blocked.clone(), |this, reason| {
            this.tooltip(Tooltip::text(reason))
        })
        .child(
            Switch::new(id, toggle_state(on))
                .disabled(blocked.is_some())
                .on_click(move |_, window, cx| on_change(!on, window, cx)),
        )
        .into_any_element()
}

/// A choice of a few options, side by side.
pub(crate) fn segmented(
    id: &'static str,
    segments: &[&str],
    selected: usize,
    blocked: Option<SharedString>,
    on_select: impl Fn(usize, &mut Window, &mut App) + 'static,
) -> AnyElement {
    let disabled = blocked.is_some();
    let control = segments
        .iter()
        .fold(SegmentedControl::new(id), |control, segment| {
            control.segment(SharedString::from(segment.to_string()))
        })
        .selected(selected)
        .on_select(move |index, window, cx| {
            if !disabled && index != selected {
                on_select(index, window, cx);
            }
        });
    div()
        .id(SharedString::from(format!("{id}-wrapper")))
        .debug_selector(move || id.to_string())
        .when(disabled, |this| this.opacity(0.55))
        .when_some(blocked, |this, reason| this.tooltip(Tooltip::text(reason)))
        .child(control)
        .into_any_element()
}

/// A menu of choices, showing `label`, with the `selected` one checked.
pub(crate) fn dropdown(
    id: &'static str,
    label: impl Into<SharedString>,
    choices: Vec<(SharedString, Callback)>,
    selected: Option<usize>,
    blocked: Option<SharedString>,
    window: &mut Window,
    cx: &mut App,
) -> AnyElement {
    let label = label.into();
    // The menu is kept between frames; a new choice or selection builds a new one.
    let key = SharedString::from(format!(
        "{id}-{}-{}-{label}",
        choices.len(),
        selected.unwrap_or(usize::MAX)
    ));
    let menu = window.use_keyed_state(key, cx, move |window, cx| {
        ContextMenu::new(window, cx, move |mut menu, _, _| {
            for (index, (label, on_select)) in choices.into_iter().enumerate() {
                menu = menu.toggleable_entry(
                    label,
                    selected == Some(index),
                    IconPosition::End,
                    None,
                    move |window, cx| on_select(window, cx),
                );
            }
            menu
        })
    });
    div()
        .id(SharedString::from(format!("{id}-wrapper")))
        .debug_selector(move || id.to_string())
        .when_some(blocked.clone(), |this, reason| {
            this.tooltip(Tooltip::text(reason))
        })
        .child(
            DropdownMenu::new(id, label, menu)
                .style(DropdownStyle::Outlined)
                .disabled(blocked.is_some()),
        )
        .into_any_element()
}

/// A value with − and + beside it. A missing callback disables its button (at a limit).
pub(crate) fn stepper(
    id: &'static str,
    value: impl Into<SharedString>,
    decrease: Option<Callback>,
    increase: Option<Callback>,
    cx: &App,
) -> AnyElement {
    let palette = palette(cx);
    let button = |suffix: &'static str,
                  icon: IconName,
                  tooltip: &'static str,
                  on_click: Option<Callback>| {
        let id = format!("{id}-{suffix}");
        let selector = id.clone();
        div().debug_selector(move || selector).child(
            IconButton::new(SharedString::from(id), icon)
                .icon_size(IconSize::Small)
                .tooltip(Tooltip::text(tooltip))
                .disabled(on_click.is_none())
                .when_some(on_click, |button, on_click: Callback| {
                    button.on_click(move |_, window, cx| on_click(window, cx))
                }),
        )
    };
    h_flex()
        .debug_selector(move || id.to_string())
        .flex_none()
        .gap_0p5()
        .px_0p5()
        .border(tokens::BORDER_WIDTH)
        .border_color(palette.border.hsla())
        .rounded(scaled(tokens::CONTROL_RADIUS))
        .child(button("decrease", IconName::Dash, "Smaller", decrease))
        .child(
            div()
                .min_w(scaled(px(44.)))
                .text_center()
                .text_size(tokens::text_size(tokens::TEXT_SIZE_SMALL))
                .text_color(palette.text.hsla())
                .child(value.into()),
        )
        .child(button("increase", IconName::Plus, "Bigger", increase))
        .into_any_element()
}

/// A text button.
pub(crate) fn button(
    id: &'static str,
    label: impl Into<SharedString>,
    blocked: Option<SharedString>,
    on_click: impl Fn(&mut Window, &mut App) + 'static,
) -> AnyElement {
    div()
        .debug_selector(move || id.to_string())
        .child(
            TextButton::new(id, label)
                .style(ButtonStyle::Default)
                .disabled(blocked.is_some())
                .when_some(blocked, |button, reason| button.tooltip(reason))
                .on_click(move |_, window, cx| on_click(window, cx)),
        )
        .into_any_element()
}

/// A box around a text editor, `width` wide at mustr's design size, or the row's width.
pub(crate) fn field(editor: &Entity<Editor>, width: Option<f32>, cx: &App) -> gpui::Div {
    let palette = palette(cx);
    div()
        .map(|this| match width {
            Some(width) => this.w(scaled(px(width))).max_w_full(),
            None => this.w_full(),
        })
        .px_2()
        .py_1()
        .rounded(scaled(tokens::CONTROL_RADIUS))
        .border(tokens::BORDER_WIDTH)
        .border_color(palette.border.hsla())
        .bg(palette.background.hsla())
        .child(editor.clone())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_word_must_match_something() {
        let query = Query::new("  Tab SIZE ");
        assert!(query.matches(&["Tab size", "Spaces per indent"]));
        assert!(query.matches(&["Tab width", "", "size"]));
        assert!(!query.matches(&["Tab width", "Spaces per indent"]));
        assert!(Query::new("").is_empty());
        assert!(Query::new("").matches(&["anything"]));
    }
}

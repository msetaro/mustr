//! Rows of controls that don't always fit their width, such as a toolbar at a big `ui_font_size`
//! in a laptop's window: the items that fit show, in their order, and the rest go into a "…" menu,
//! lowest priority first.
//!
//! [`fit`] decides which items show, from their widths. [`OverflowRow`] measures those widths as
//! the row is drawn: the row's own width every frame, and each item's width whenever it shows, so
//! an item in the menu keeps the width it last had. Until an item has been measured, every item
//! shows; when a measurement changes which items fit, the view is drawn again.

use gpui::{
    AnyElement, Div, ElementId, InteractiveElement as _, IntoElement, ParentElement as _, Pixels,
    SharedString, Styled as _, Window, div, px,
};
use std::{cell::RefCell, cmp::Reverse, collections::HashMap, rc::Rc};
use ui::{ButtonCommon as _, Color, IconButton, IconName, IconSize, Tooltip, h_flex};

use crate::tokens::scaled;

/// The priority of an item that never goes into the menu.
pub const ALWAYS: u32 = u32::MAX;

/// The key [`OverflowRow::render`] measures the "…" button by.
pub const MORE_KEY: &str = "overflow-more";

/// One item of a row: its key, and how much it matters. Higher priorities stay longer.
#[derive(Clone, Debug, PartialEq)]
pub struct OverflowItem {
    pub key: SharedString,
    pub priority: u32,
    /// For an item that shrinks, cutting its text with an ellipsis, rather than going into the
    /// menu: the narrowest it gets.
    pub min_width: Option<Pixels>,
}

impl OverflowItem {
    pub fn new(key: impl Into<SharedString>, priority: u32) -> Self {
        Self {
            key: key.into(),
            priority,
            min_width: None,
        }
    }

    /// Lets the item shrink down to `min_width` before others go into the menu. It never goes
    /// into the menu itself.
    pub fn shrinks_to(mut self, min_width: Pixels) -> Self {
        self.min_width = Some(min_width);
        self.priority = ALWAYS;
        self
    }
}

/// Which items show in `available`, given each one's priority and width, with `gap` between
/// neighbors. When they don't all fit, items go into the menu, lowest priority first (and, among
/// equals, the later one first), leaving room for the "…" button (`more` wide), until the rest
/// fit. Then the items that went come back, highest priority first, where they still fit, so a
/// narrow item can show where a wider, more important one didn't fit. Items with priority
/// [`ALWAYS`] stay even when they don't fit.
pub fn fit(available: Pixels, gap: Pixels, more: Pixels, items: &[(u32, Pixels)]) -> Vec<bool> {
    let mut shown = vec![true; items.len()];
    let width = |shown: &[bool], more: Option<Pixels>| {
        let widths = items
            .iter()
            .zip(shown)
            .filter(|(_, shown)| **shown)
            .map(|((_, width), _)| *width)
            .chain(more);
        let (total, count) = widths.fold((px(0.), 0usize), |(total, count), width| {
            (total + width, count + 1)
        });
        total + gap * count.saturating_sub(1) as f32
    };
    if width(&shown, None) <= available {
        return shown;
    }
    let mut order = (0..items.len())
        .filter(|index| items[*index].0 != ALWAYS)
        .collect::<Vec<_>>();
    order.sort_by_key(|index| (items[*index].0, Reverse(*index)));
    let mut gone = Vec::new();
    for index in order {
        shown[index] = false;
        gone.push(index);
        if width(&shown, Some(more)) <= available {
            break;
        }
    }
    for index in gone.into_iter().rev().skip(1) {
        shown[index] = true;
        if width(&shown, Some(more)) > available {
            shown[index] = false;
        }
    }
    shown
}

/// Measures a row's width and its items' widths as it's drawn, to decide which items show. Keep
/// one per row in the view that draws it.
#[derive(Clone, Default)]
pub struct OverflowRow(Rc<RefCell<Measured>>);

#[derive(Default)]
struct Measured {
    row_width: Option<Pixels>,
    widths: HashMap<SharedString, Pixels>,
    /// What the last [`OverflowRow::visible`] was asked, and its answer, to tell whether a new
    /// measurement changes it.
    last: Option<(Vec<OverflowItem>, Pixels, Pixels, Vec<bool>)>,
}

impl Measured {
    fn visible(&self, items: &[OverflowItem], gap: Pixels, more: Pixels) -> Vec<bool> {
        let Some(available) = self.row_width else {
            return vec![true; items.len()];
        };
        let widths = items
            .iter()
            .map(|item| {
                let width = item
                    .min_width
                    .or_else(|| self.widths.get(&item.key).copied())?;
                Some((item.priority, width))
            })
            .collect::<Option<Vec<_>>>();
        // An item that hasn't been measured yet shows, so that it can be.
        let Some(widths) = widths else {
            return vec![true; items.len()];
        };
        let more = self.widths.get(MORE_KEY).copied().unwrap_or(more);
        fit(available, gap, more, &widths)
    }
}

impl OverflowRow {
    pub fn new() -> Self {
        Self::default()
    }

    /// Which of `items` show, from what the row last measured, with `gap` between them. `more`
    /// is the "…" button's width until it has been measured.
    pub fn visible(&self, items: &[OverflowItem], gap: Pixels, more: Pixels) -> Vec<bool> {
        let mut measured = self.0.borrow_mut();
        let visible = measured.visible(items, gap, more);
        measured.last = Some((items.to_vec(), gap, more, visible.clone()));
        visible
    }

    /// The width the row last had, once it has been drawn.
    pub fn width(&self) -> Option<Pixels> {
        self.0.borrow().row_width
    }

    /// How narrow the row gets: the items that always show (shrinking ones at their narrowest)
    /// and the "…" button, once each has been measured. For a row that shrinks among others
    /// (`.min_w(min_width)`).
    pub fn min_width(&self, items: &[OverflowItem], gap: Pixels, more: Pixels) -> Option<Pixels> {
        let measured = self.0.borrow();
        let mut widths = items
            .iter()
            .filter(|item| item.priority == ALWAYS)
            .map(|item| {
                item.min_width
                    .or_else(|| measured.widths.get(&item.key).copied())
            })
            .collect::<Option<Vec<_>>>()?;
        if items.iter().any(|item| item.priority != ALWAYS) {
            widths.push(measured.widths.get(MORE_KEY).copied().unwrap_or(more));
        }
        let count = widths.len();
        Some(
            widths
                .into_iter()
                .fold(px(0.), |total, width| total + width)
                + gap * count.saturating_sub(1) as f32,
        )
    }

    /// How wide the row is with every item showing, once each has been measured: for a row that
    /// sits among others, which can take that width when there's room and shrink when there
    /// isn't (`.w(natural_width).flex_shrink_1()`), so the room it gets doesn't depend on which
    /// of its items show.
    pub fn natural_width(&self, items: &[OverflowItem], gap: Pixels) -> Option<Pixels> {
        let measured = self.0.borrow();
        let widths = items
            .iter()
            .map(|item| measured.widths.get(&item.key).copied())
            .collect::<Option<Vec<_>>>()?;
        let count = widths.len();
        Some(
            widths
                .into_iter()
                .fold(px(0.), |total, width| total + width)
                + gap * count.saturating_sub(1) as f32,
        )
    }

    /// A flex row of `children` that fills its parent's width and measures itself and each
    /// keyed child as it's drawn. The keys are the [`OverflowItem`]s' (an item's child keeps its
    /// width, or shrinks to its `min_width`) and [`MORE_KEY`] for the "…" button; children
    /// without a key, such as spacers, aren't measured. Each keyed child is wrapped in an element
    /// whose debug selector is its key, for tests. Draws the view again when a measurement
    /// changes which items show.
    pub fn render(
        &self,
        children: impl IntoIterator<Item = (Option<SharedString>, AnyElement)>,
        window: &Window,
    ) -> Div {
        let view = window.current_view();
        let min_widths = self
            .0
            .borrow()
            .last
            .as_ref()
            .map(|(items, ..)| {
                items
                    .iter()
                    .filter_map(|item| Some((item.key.clone(), item.min_width?)))
                    .collect::<HashMap<_, _>>()
            })
            .unwrap_or_default();
        let (keys, children): (Vec<_>, Vec<_>) = children
            .into_iter()
            .map(|(key, child)| {
                let child = match &key {
                    Some(key) => {
                        let selector = key.to_string();
                        match min_widths.get(key) {
                            Some(min_width) => h_flex()
                                .debug_selector(move || selector)
                                .flex_shrink_1()
                                .min_w(*min_width)
                                .overflow_hidden()
                                .child(child)
                                .into_any_element(),
                            None => div()
                                .debug_selector(move || selector)
                                .flex_none()
                                .child(child)
                                .into_any_element(),
                        }
                    }
                    None => child,
                };
                (key, child)
            })
            .unzip();
        let measured = self.0.clone();
        h_flex()
            .w_full()
            .min_w_0()
            .overflow_hidden()
            // The row's own bounds, measured first.
            .child(div().absolute().inset_0())
            .children(children)
            .on_children_prepainted(move |bounds, _, cx| {
                let mut measured = measured.borrow_mut();
                let mut bounds = bounds.into_iter();
                let row_width = bounds.next().map(|row| row.size.width);
                // A row that lays itself out by its width alone (`width`) is drawn again when
                // that changes.
                let resized = measured.last.is_none() && row_width != measured.row_width;
                measured.row_width = row_width;
                for (key, bounds) in keys.iter().zip(bounds) {
                    if let Some(key) = key {
                        measured.widths.insert(key.clone(), bounds.size.width);
                    }
                }
                let changed = resized
                    || measured
                        .last
                        .as_ref()
                        .is_some_and(|(items, gap, more, shown)| {
                            measured.visible(items, *gap, *more) != *shown
                        });
                if changed {
                    // The view can't be invalidated while it's being drawn.
                    cx.defer(move |cx| cx.notify(view));
                }
            })
    }
}

/// The "…" button that opens a row's menu of the items that didn't fit.
pub fn more_button(id: impl Into<ElementId>, tooltip: &'static str) -> IconButton {
    IconButton::new(id, IconName::Ellipsis)
        .icon_size(IconSize::Small)
        .icon_color(Color::Muted)
        .tooltip(Tooltip::text(tooltip))
}

/// The "…" button's width before it has been measured: about an icon button's, at
/// `rem_size`.
pub fn more_button_width(rem_size: Pixels) -> Pixels {
    scaled(px(24.)).to_pixels(rem_size)
}

#[cfg(test)]
mod tests {
    use super::*;
    use gpui::{Context, Entity, Render, TestAppContext, VisualTestContext, size};
    use ui::FluentBuilder as _;

    fn widths(items: &[(u32, f32)]) -> Vec<(u32, Pixels)> {
        items
            .iter()
            .map(|(priority, width)| (*priority, px(*width)))
            .collect()
    }

    #[test]
    fn everything_shows_when_it_fits() {
        let items = widths(&[(1, 100.), (2, 100.), (3, 100.)]);
        // 300 for the items and 2 gaps of 10.
        assert_eq!(fit(px(320.), px(10.), px(20.), &items), [true; 3]);
    }

    #[test]
    fn the_lowest_priorities_go_first_leaving_room_for_the_menu() {
        let items = widths(&[(3, 100.), (1, 100.), (2, 100.), (ALWAYS, 100.)]);
        // Without the 2nd: 300 + 2 gaps + the button and its gap = 350.
        assert_eq!(
            fit(px(350.), px(10.), px(20.), &items),
            [true, false, true, true]
        );
        // One pixel less, and the 3rd goes too.
        assert_eq!(
            fit(px(349.), px(10.), px(20.), &items),
            [true, false, false, true]
        );
        // Items that always show stay, even when they don't fit.
        assert_eq!(
            fit(px(50.), px(10.), px(20.), &items),
            [false, false, false, true]
        );
    }

    #[test]
    fn narrow_items_come_back_where_wider_ones_dont_fit() {
        // The 2nd goes first, then the wide 3rd; then there's room for the 2nd again.
        let items = widths(&[(ALWAYS, 100.), (1, 50.), (2, 200.), (ALWAYS, 100.)]);
        assert_eq!(
            fit(px(300.), px(10.), px(20.), &items),
            [true, true, false, true]
        );
    }

    #[test]
    fn among_equals_the_later_item_goes_first() {
        let items = widths(&[(1, 100.), (1, 100.), (1, 100.)]);
        assert_eq!(fit(px(250.), px(10.), px(20.), &items), [true, true, false]);
    }

    struct Row {
        overflow: OverflowRow,
        items: Vec<(OverflowItem, f32)>,
    }

    impl Render for Row {
        fn render(&mut self, window: &mut Window, _: &mut Context<Self>) -> impl IntoElement {
            let items = self
                .items
                .iter()
                .map(|(item, _)| item.clone())
                .collect::<Vec<_>>();
            let visible = self.overflow.visible(&items, px(10.), px(20.));
            let mut children = self
                .items
                .iter()
                .zip(&visible)
                .filter(|(_, visible)| **visible)
                .map(|((item, width), _)| {
                    let key = item.key.clone();
                    let selector = item.key.to_string();
                    let child = div()
                        .debug_selector(move || selector)
                        .h(px(20.))
                        .w(px(*width))
                        .when(item.min_width.is_some(), |this| {
                            this.flex_shrink_1().min_w_0()
                        })
                        .into_any_element();
                    (Some(key), child)
                })
                .collect::<Vec<_>>();
            if visible.contains(&false) {
                let more = div()
                    .debug_selector(|| MORE_KEY.into())
                    .flex_none()
                    .h(px(20.))
                    .w(px(20.))
                    .into_any_element();
                children.push((Some(MORE_KEY.into()), more));
            }
            div()
                .size_full()
                .child(self.overflow.render(children, window).gap(px(10.)))
        }
    }

    fn open_row(
        items: Vec<(OverflowItem, f32)>,
        cx: &mut TestAppContext,
    ) -> (Entity<Row>, &mut VisualTestContext) {
        cx.add_window_view(|_, _| Row {
            overflow: OverflowRow::new(),
            items,
        })
    }

    fn shown(cx: &mut VisualTestContext, keys: &[&'static str]) -> Vec<String> {
        keys.iter()
            .filter(|key| cx.debug_bounds(key).is_some())
            .map(|key| key.to_string())
            .collect()
    }

    /// The row measures itself and its items as it's drawn, so a narrower window moves items
    /// into the menu, and a wider one brings them back.
    #[gpui::test]
    fn the_row_follows_the_window_width(cx: &mut TestAppContext) {
        let (_, cx) = open_row(
            vec![
                (OverflowItem::new("a", 3), 100.),
                (OverflowItem::new("b", 1), 100.),
                (OverflowItem::new("c", 2), 100.),
            ],
            cx,
        );
        let keys = ["a", "b", "c", MORE_KEY];
        cx.simulate_resize(size(px(500.), px(100.)));
        cx.run_until_parked();
        assert_eq!(shown(cx, &keys), ["a", "b", "c"]);

        cx.simulate_resize(size(px(260.), px(100.)));
        cx.run_until_parked();
        assert_eq!(shown(cx, &keys), ["a", "c", MORE_KEY]);

        cx.simulate_resize(size(px(150.), px(100.)));
        cx.run_until_parked();
        assert_eq!(shown(cx, &keys), ["a", MORE_KEY]);

        cx.simulate_resize(size(px(500.), px(100.)));
        cx.run_until_parked();
        assert_eq!(shown(cx, &keys), ["a", "b", "c"]);
    }

    /// An item that shrinks stays, and gets narrower, while the others go into the menu.
    #[gpui::test]
    fn shrinking_items_stay_and_shrink(cx: &mut TestAppContext) {
        let (_, cx) = open_row(
            vec![
                (OverflowItem::new("name", 0).shrinks_to(px(40.)), 200.),
                (OverflowItem::new("a", 1), 100.),
                (OverflowItem::new("b", 2), 100.),
            ],
            cx,
        );
        let keys = ["name", "a", "b", MORE_KEY];
        // Everything fits with the name cut short: 40 + 100 + 100 and two gaps.
        cx.simulate_resize(size(px(300.), px(100.)));
        cx.run_until_parked();
        assert_eq!(shown(cx, &keys), ["name", "a", "b"]);
        assert_eq!(cx.debug_bounds("name").unwrap().size.width, px(80.));

        // Narrower, "a" goes, and the name gets what's left.
        cx.simulate_resize(size(px(250.), px(100.)));
        cx.run_until_parked();
        assert_eq!(shown(cx, &keys), ["name", "b", MORE_KEY]);
        assert_eq!(cx.debug_bounds("name").unwrap().size.width, px(110.));
    }
}

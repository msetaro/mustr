//! Rounded corners for fills drawn inside rounded containers.
//!
//! GPUI clips children to rectangles, not to their parent's rounded corners, so a hover,
//! selected or pressed fill that reaches the edge of a rounded container is drawn with square
//! corners over the container's rounded ones. mustr's rule:
//!
//! - A fill that sits flush against its container's edge (a segmented control's segments, a
//!   button's clickable parts, a card's title row) rounds the corners it shares with the
//!   container, concentric with the container's border: [`ChildPosition`], [`inner_radius`] and
//!   [`RoundedCorners::rounded_at`]. Children that wrap find the corners they touch from their
//!   layout with [`touched_corners`].
//! - Rows in lists and menus are inset from their container by [`tokens::LIST_INSET`] and rounded
//!   on every corner by [`tokens::CONTROL_RADIUS`], so they never reach its edge.

use crate::tokens;
use gpui::{Axis, Bounds, Corners, Pixels, Styled, px};

/// Where a child sits among its siblings along its container's main axis.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ChildPosition {
    /// The only child: it touches every corner.
    Only,
    First,
    Middle,
    Last,
}

impl ChildPosition {
    /// The position of the child at `index` of `count`.
    pub fn of(index: usize, count: usize) -> Self {
        match (index == 0, index + 1 >= count) {
            (true, true) => Self::Only,
            (true, false) => Self::First,
            (false, true) => Self::Last,
            (false, false) => Self::Middle,
        }
    }

    /// The container's corners a child at this position touches, in a row
    /// ([`Axis::Horizontal`]) or a column ([`Axis::Vertical`]) that fills the container.
    pub fn corners(self, axis: Axis) -> Corners<bool> {
        let (start, end) = match self {
            Self::Only => (true, true),
            Self::First => (true, false),
            Self::Middle => (false, false),
            Self::Last => (false, true),
        };
        match axis {
            Axis::Horizontal => Corners {
                top_left: start,
                top_right: end,
                bottom_right: end,
                bottom_left: start,
            },
            Axis::Vertical => Corners {
                top_left: start,
                top_right: start,
                bottom_right: end,
                bottom_left: end,
            },
        }
    }
}

/// The radius for a fill flush inside a container rounded by `outer` with a
/// [`tokens::BORDER_WIDTH`] border: concentric with the border's inside edge, so the fill
/// follows it. `outer` is in pixels, such as `scaled(tokens::CONTROL_RADIUS).to_pixels(rem_size)`.
pub fn inner_radius(outer: Pixels) -> Pixels {
    (outer - tokens::BORDER_WIDTH).max(px(0.))
}

/// The corners of `container` (its bounds inside its border) that `child` touches. Within half
/// a pixel counts, for layouts that round to whole pixels.
pub fn touched_corners(container: Bounds<Pixels>, child: Bounds<Pixels>) -> Corners<bool> {
    let near = |a: Pixels, b: Pixels| (a - b).abs() <= px(0.5);
    let left = near(child.left(), container.left());
    let right = near(child.right(), container.right());
    let top = near(child.top(), container.top());
    let bottom = near(child.bottom(), container.bottom());
    Corners {
        top_left: top && left,
        top_right: top && right,
        bottom_right: bottom && right,
        bottom_left: bottom && left,
    }
}

/// Rounds some of an element's corners, for fills inside rounded containers (see the module
/// docs).
pub trait RoundedCorners: Styled + Sized {
    /// Rounds `corners` by `radius` and squares the others.
    fn rounded_corners(mut self, corners: Corners<bool>, radius: Pixels) -> Self {
        let radius_for = |touches: bool| if touches { radius } else { px(0.) };
        let radii = &mut self.style().corner_radii;
        radii.top_left = Some(radius_for(corners.top_left).into());
        radii.top_right = Some(radius_for(corners.top_right).into());
        radii.bottom_right = Some(radius_for(corners.bottom_right).into());
        radii.bottom_left = Some(radius_for(corners.bottom_left).into());
        self
    }

    /// Rounds the corners a child at `position` along `axis` shares with its container, by
    /// `radius` (usually the container's [`inner_radius`]).
    fn rounded_at(self, position: ChildPosition, axis: Axis, radius: Pixels) -> Self {
        self.rounded_corners(position.corners(axis), radius)
    }
}

impl<E: Styled> RoundedCorners for E {}

#[cfg(test)]
mod tests {
    use super::*;
    use gpui::{AbsoluteLength, Div, bounds, div, point, size};

    fn corners(
        top_left: bool,
        top_right: bool,
        bottom_right: bool,
        bottom_left: bool,
    ) -> Corners<bool> {
        Corners {
            top_left,
            top_right,
            bottom_right,
            bottom_left,
        }
    }

    #[test]
    fn positions_of_children() {
        assert_eq!(ChildPosition::of(0, 1), ChildPosition::Only);
        assert_eq!(ChildPosition::of(0, 3), ChildPosition::First);
        assert_eq!(ChildPosition::of(1, 3), ChildPosition::Middle);
        assert_eq!(ChildPosition::of(2, 3), ChildPosition::Last);
    }

    #[test]
    fn rows_round_their_ends_and_columns_their_top_and_bottom() {
        use Axis::*;
        let all = corners(true, true, true, true);
        let none = corners(false, false, false, false);
        assert_eq!(ChildPosition::Only.corners(Horizontal), all);
        assert_eq!(ChildPosition::Only.corners(Vertical), all);
        assert_eq!(ChildPosition::Middle.corners(Horizontal), none);
        assert_eq!(ChildPosition::Middle.corners(Vertical), none);
        // A segmented control's or a split button's ends.
        assert_eq!(
            ChildPosition::First.corners(Horizontal),
            corners(true, false, false, true)
        );
        assert_eq!(
            ChildPosition::Last.corners(Horizontal),
            corners(false, true, true, false)
        );
        // A card's first and last rows.
        assert_eq!(
            ChildPosition::First.corners(Vertical),
            corners(true, true, false, false)
        );
        assert_eq!(
            ChildPosition::Last.corners(Vertical),
            corners(false, false, true, true)
        );
    }

    #[test]
    fn fills_follow_the_inside_of_the_border() {
        assert_eq!(inner_radius(px(12.)), px(11.));
        assert_eq!(inner_radius(px(0.5)), px(0.));
    }

    #[test]
    fn radii_of_a_rounded_fill() {
        let radii = |position, axis| {
            let mut fill: Div = div().rounded_at(position, axis, px(5.));
            let radii = fill.style().corner_radii.clone();
            [
                radii.top_left,
                radii.top_right,
                radii.bottom_right,
                radii.bottom_left,
            ]
        };
        let five = Some(AbsoluteLength::Pixels(px(5.)));
        let square = Some(AbsoluteLength::Pixels(px(0.)));
        assert_eq!(
            radii(ChildPosition::First, Axis::Horizontal),
            [five, square, square, five]
        );
        assert_eq!(
            radii(ChildPosition::Middle, Axis::Horizontal),
            [square, square, square, square]
        );
        assert_eq!(
            radii(ChildPosition::Last, Axis::Horizontal),
            [square, five, five, square]
        );
        assert_eq!(radii(ChildPosition::Only, Axis::Vertical), [five; 4]);
        assert_eq!(
            radii(ChildPosition::Last, Axis::Vertical),
            [square, square, five, five]
        );
    }

    #[test]
    fn wrapped_children_touch_the_corners_they_reach() {
        // Three segments in a 100 x 40 box: two on the first line, one on the second.
        let container = bounds(point(px(0.), px(0.)), size(px(100.), px(40.)));
        let first = bounds(point(px(0.), px(0.)), size(px(60.), px(20.)));
        let second = bounds(point(px(60.), px(0.)), size(px(40.), px(20.)));
        let third = bounds(point(px(0.), px(20.)), size(px(70.), px(20.)));
        assert_eq!(
            touched_corners(container, first),
            corners(true, false, false, false)
        );
        assert_eq!(
            touched_corners(container, second),
            corners(false, true, false, false)
        );
        assert_eq!(
            touched_corners(container, third),
            corners(false, false, false, true)
        );
        // On one line, they touch the top and bottom corners at their ends.
        let one_line = bounds(point(px(0.), px(0.)), size(px(100.), px(20.)));
        assert_eq!(
            touched_corners(one_line, first),
            corners(true, false, false, true)
        );
        assert_eq!(
            touched_corners(one_line, second),
            corners(false, true, true, false)
        );
        // Layout rounding leaves fractions of a pixel.
        let nearly = bounds(point(px(0.3), px(0.)), size(px(99.5), px(20.)));
        assert_eq!(
            touched_corners(one_line, nearly),
            corners(true, true, true, true)
        );
    }
}

//! mustr's components, drawn from the design tokens of the active theme (see
//! [`crate::palette`]) so they look like the mockups rather than like Zed.

mod button;
mod card;
mod chip;
mod icon_button;
mod kbd;
mod rail_button;
mod segmented_control;
mod status_hint;
mod window_tab;

pub use button::{ButtonStyle, SplitButton, TextButton};
pub use card::Card;
pub use chip::{Chip, Pill};
pub use icon_button::{IconOnlyButton, IconSplitButton, QuietButton};
pub use kbd::{Kbd, KeyStyle, kbds_for_action, key_label};
pub use rail_button::RailButton;
pub use segmented_control::SegmentedControl;
pub use status_hint::StatusHint;
pub use window_tab::WindowTab;

use gpui::{App, ClickEvent, Window};

type ClickHandler = Box<dyn Fn(&ClickEvent, &mut Window, &mut App) + 'static>;

#[cfg(test)]
mod tests;

//! Mouse reports for programs that ask for them (`tmux`'s or `vim`'s mouse mode), encoded as a
//! local terminal would, for T3's terminals: Zed's display-only terminal writes them to no PTY.
//!
//! Copied from Zed's `crates/terminal/src/mappings/mouse.rs` (itself from Alacritty), whose
//! functions are private to its crate.

use gpui::{Modifiers, MouseButton, Pixels, Point as GpuiPoint, px};
use terminal::{Modes, TerminalBounds};

/// A cell of the screen: its line (negative in the scrollback) and column.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct Cell {
    pub line: i32,
    pub column: usize,
}

enum Format {
    Sgr,
    Normal { utf8: bool },
}

impl Format {
    fn of(mode: Modes) -> Self {
        if mode.contains(Modes::SGR_MOUSE) {
            Self::Sgr
        } else {
            Self::Normal {
                utf8: mode.contains(Modes::UTF8_MOUSE),
            }
        }
    }
}

const LEFT: u8 = 0;
const MIDDLE: u8 = 1;
const RIGHT: u8 = 2;
const MOVE: u8 = 32;
const NO_BUTTON_MOVE: u8 = 35;
const SCROLL_UP: u8 = 64;
const SCROLL_DOWN: u8 = 65;

fn button_code(button: MouseButton) -> Option<u8> {
    match button {
        MouseButton::Left => Some(LEFT),
        MouseButton::Middle => Some(MIDDLE),
        MouseButton::Right => Some(RIGHT),
        MouseButton::Navigate(_) => None,
    }
}

/// Whether the program wants mouse events now: it asked for them, and Shift isn't held, which
/// keeps the local selection, as in T3's terminal and Zed's.
pub(crate) fn reports(mode: Modes, modifiers: &Modifiers) -> bool {
    mode.intersects(Modes::MOUSE_MODE) && !modifiers.shift
}

/// The cell under `position`, relative to the grid's top left (`grid_point` in Zed).
pub(crate) fn cell_at(
    position: GpuiPoint<Pixels>,
    bounds: &TerminalBounds,
    display_offset: usize,
) -> Cell {
    let last_column = bounds.num_columns().saturating_sub(1);
    let column = ((position.x.max(px(0.)) / bounds.cell_width) as usize).min(last_column);
    let last_line = i32::try_from(bounds.num_lines().saturating_sub(1)).unwrap_or(i32::MAX);
    let line = ((position.y / bounds.line_height) as i32).min(last_line);
    let display_offset = i32::try_from(display_offset).unwrap_or(i32::MAX);
    Cell {
        line: line.saturating_sub(display_offset),
        column,
    }
}

/// A press or release.
pub(crate) fn button_report(
    cell: Cell,
    button: MouseButton,
    modifiers: &Modifiers,
    pressed: bool,
    mode: Modes,
) -> Option<Vec<u8>> {
    let button = button_code(button)?;
    mode.intersects(Modes::MOUSE_MODE)
        .then(|| report(cell, button, pressed, modifiers, Format::of(mode)))
        .flatten()
}

/// A move: a drag with a button held, or any motion when the program asks for it.
pub(crate) fn moved_report(
    cell: Cell,
    pressed: Option<MouseButton>,
    modifiers: &Modifiers,
    mode: Modes,
) -> Option<Vec<u8>> {
    let button = match pressed {
        Some(button) => MOVE + button_code(button)?,
        None => NO_BUTTON_MOVE,
    };
    if !mode.intersects(Modes::MOUSE_MOTION | Modes::MOUSE_DRAG) {
        return None;
    }
    // Drag mode reports only drags.
    if mode.contains(Modes::MOUSE_DRAG) && button == NO_BUTTON_MOVE {
        return None;
    }
    report(cell, button, true, modifiers, Format::of(mode))
}

/// One wheel step's report, up or down.
pub(crate) fn scroll_report(
    cell: Cell,
    up: bool,
    modifiers: &Modifiers,
    mode: Modes,
) -> Option<Vec<u8>> {
    let button = if up { SCROLL_UP } else { SCROLL_DOWN };
    mode.intersects(Modes::MOUSE_MODE)
        .then(|| report(cell, button, true, modifiers, Format::of(mode)))
        .flatten()
}

fn report(
    cell: Cell,
    button: u8,
    pressed: bool,
    modifiers: &Modifiers,
    format: Format,
) -> Option<Vec<u8>> {
    if cell.line < 0 {
        return None;
    }
    let mut mods = 0;
    if modifiers.shift {
        mods += 4;
    }
    if modifiers.alt {
        mods += 8;
    }
    if modifiers.control {
        mods += 16;
    }
    match format {
        Format::Sgr => Some(
            format!(
                "\x1b[<{};{};{}{}",
                button + mods,
                cell.column + 1,
                cell.line + 1,
                if pressed { 'M' } else { 'm' }
            )
            .into_bytes(),
        ),
        Format::Normal { utf8 } => {
            normal_report(cell, if pressed { button + mods } else { 3 + mods }, utf8)
        }
    }
}

fn normal_report(cell: Cell, button: u8, utf8: bool) -> Option<Vec<u8>> {
    let max = if utf8 { 2015 } else { 223 };
    if cell.line >= max || cell.column >= max as usize {
        return None;
    }
    let mut message = vec![b'\x1b', b'[', b'M', 32 + button];
    let encode = |position: usize| -> Vec<u8> {
        let position = 32 + 1 + position;
        vec![(0xC0 + position / 64) as u8, (0x80 + (position & 63)) as u8]
    };
    if utf8 && cell.column >= 95 {
        message.extend(encode(cell.column));
    } else {
        message.push(32 + 1 + cell.column as u8);
    }
    if utf8 && cell.line >= 95 {
        message.extend(encode(cell.line as usize));
    } else {
        message.push(32 + 1 + cell.line as u8);
    }
    Some(message)
}

/// A report as `terminal.write` text. T3 takes text, so a legacy report of a cell past column 94
/// (bytes that aren't UTF-8) can't go; programs that care use SGR reports.
pub(crate) fn as_text(report: Vec<u8>) -> Option<String> {
    String::from_utf8(report).ok()
}

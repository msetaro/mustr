use crate::{palette, tokens};
use gpui::{
    Action, App, IntoElement, KeyContext, Modifiers, ParentElement as _, RenderOnce, SharedString,
    Styled as _, Window, div, px,
};
use ui::StyledTypography as _;

/// A key cap, such as the `j` in the status bar's `j k hunk` hint.
#[derive(IntoElement)]
pub struct Kbd {
    key: SharedString,
}

impl Kbd {
    pub fn new(key: impl Into<SharedString>) -> Self {
        Self { key: key.into() }
    }
}

impl RenderOnce for Kbd {
    fn render(self, _window: &mut Window, cx: &mut App) -> impl IntoElement {
        let palette = palette(cx);
        div()
            .flex_none()
            .min_w(px(15.))
            .px(px(4.))
            .border(tokens::BORDER_WIDTH)
            .border_b(px(2.))
            .border_color(palette.border.hsla())
            .rounded(tokens::KBD_RADIUS)
            .bg(palette.surface_2.hsla())
            .text_color(palette.text.hsla())
            .font_buffer(cx)
            .text_size(tokens::text_size(tokens::TEXT_SIZE_TINY))
            .line_height(tokens::text_size(px(14.)))
            .text_center()
            .child(self.key)
    }
}

/// How key labels are written.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum KeyStyle {
    /// Modifier symbols, as macOS writes them: `⌘⇧P`.
    Symbols,
    /// Modifier names: `Ctrl+Shift+P`.
    Words,
}

impl KeyStyle {
    pub fn platform() -> Self {
        if cfg!(target_os = "macos") {
            Self::Symbols
        } else {
            Self::Words
        }
    }
}

/// The label of one keystroke on a key cap.
pub fn key_label(modifiers: &Modifiers, key: &str, style: KeyStyle) -> String {
    let mut label = String::new();
    let mut push_modifier = |symbol: &str, word: &str| match style {
        KeyStyle::Symbols => label.push_str(symbol),
        KeyStyle::Words => {
            label.push_str(word);
            label.push('+');
        }
    };
    if modifiers.function {
        push_modifier("fn ", "Fn");
    }
    if modifiers.control {
        push_modifier("⌃", "Ctrl");
    }
    if modifiers.alt {
        push_modifier("⌥", "Alt");
    }
    if modifiers.shift {
        push_modifier("⇧", "Shift");
    }
    if modifiers.platform {
        push_modifier("⌘", "Super");
    }
    let key = match key {
        "escape" => "Esc".to_string(),
        "enter" => "Enter".to_string(),
        "space" => "Space".to_string(),
        "tab" => "Tab".to_string(),
        "backspace" => "Backspace".to_string(),
        "delete" => "Delete".to_string(),
        "up" => "↑".to_string(),
        "down" => "↓".to_string(),
        "left" => "←".to_string(),
        "right" => "→".to_string(),
        key if key.chars().count() == 1 => key.to_uppercase(),
        key => {
            let mut chars = key.chars();
            chars
                .next()
                .map(|first| first.to_uppercase().chain(chars).collect())
                .unwrap_or_default()
        }
    };
    label.push_str(&key);
    label
}

/// One key cap per keystroke of the binding for `action` in `context` (such as `"Workspace"`),
/// whatever is focused, if it has one.
pub fn kbds_for_action(action: &dyn Action, context: &str, window: &Window) -> Option<Vec<Kbd>> {
    let context = KeyContext::parse(context).ok()?;
    let binding = window.highest_precedence_binding_for_action_in_context(action, context)?;
    let style = KeyStyle::platform();
    Some(
        binding
            .keystrokes()
            .iter()
            .map(|keystroke| Kbd::new(key_label(keystroke.modifiers(), keystroke.key(), style)))
            .collect(),
    )
}

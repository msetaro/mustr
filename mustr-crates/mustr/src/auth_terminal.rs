//! An agent's sign-in terminal on the agents page (`mustr-docs/PARITY.md` §2.17, "Install and
//! sign in to agents"), as T3's `settings/ProviderAuthTerminal.tsx` draws it: the PTY T3 runs the
//! agent's sign-in command in, shown in Zed's display-only terminal.
//!
//! - **Output** comes in the sign-in's state (`provider.auth.subscribe`): the last 16,384
//!   characters of the PTY's output and, on servers that send it, `outputOffset`, how many it has
//!   written in all. As T3's app does, the new part is written (the difference of offsets), or
//!   the whole tail again when more was missed than it holds.
//! - **Input:** keys (escape sequences for Enter, arrows, `ctrl-c` and so on, as Zed's terminal
//!   encodes them), typed text, and pastes go back as `provider.auth.respond` terminal
//!   responses, through the page's queue, which sends them one at a time. The grid's size goes
//!   with an empty response whenever it changes.
//!
//! mustr never starts a sign-in on its own; the terminal only shows one the user started.

use crate::remote_terminal::keys;
use gpui::{
    App, AppContext as _, ClipboardEntry, Context, Entity, FocusHandle, Focusable,
    InteractiveElement as _, IntoElement, KeyDownEvent, ParentElement as _, Render, Styled as _,
    Subscription, Task, WeakEntity, Window, div, prelude::FluentBuilder as _,
};
use project::Project;
use settings::Settings as _;
use std::{rc::Rc, time::Duration};
use terminal::{Modes, Terminal, TerminalBuilder, terminal_settings::TerminalSettings};
use terminal_view::TerminalView;
use util::paths::PathStyle;
use workspace::Workspace;

/// T3's limits on a sign-in terminal's grid (`ProviderAuthTerminalSize`).
const MAX_COLS: i64 = 500;
const MAX_ROWS: i64 = 200;
/// How long the grid stays at one size before T3 is told.
const RESIZE_DEBOUNCE: Duration = Duration::from_millis(60);

/// What to write for a new sign-in state.
#[derive(Debug, PartialEq, Eq)]
pub(crate) enum OutputChange<'a> {
    None,
    Append(&'a str),
    Replace(&'a str),
}

/// What changed since `written` (T3's `outputOffset` last written) or, on servers without
/// offsets, since `shown` (the output last written). Offsets count UTF-16 units, as T3's
/// JavaScript strings do.
pub(crate) fn output_change<'a>(
    output: &'a str,
    offset: Option<i64>,
    written: Option<i64>,
    shown: &str,
) -> OutputChange<'a> {
    match (offset, written) {
        (Some(offset), Some(written)) => {
            let delta = offset - written;
            if delta <= 0 {
                return OutputChange::None;
            }
            let length = output.encode_utf16().count() as i64;
            if delta > length {
                return OutputChange::Replace(output);
            }
            OutputChange::Append(utf16_tail(output, delta as usize))
        }
        (Some(_), None) => OutputChange::Replace(output),
        (None, _) => {
            if output == shown {
                OutputChange::None
            } else if let Some(rest) = output.strip_prefix(shown) {
                OutputChange::Append(rest)
            } else {
                OutputChange::Replace(output)
            }
        }
    }
}

/// The last `units` UTF-16 units of `text`, widened to a whole character.
fn utf16_tail(text: &str, units: usize) -> &str {
    let mut counted = 0;
    for (index, character) in text.char_indices().rev() {
        counted += character.len_utf16();
        if counted >= units {
            return &text[index..];
        }
    }
    text
}

/// Sends what the user types to the sign-in.
pub(crate) type SendInput = Rc<dyn Fn(String, &mut App)>;
/// Tells the sign-in the grid's size, in columns and rows.
pub(crate) type SendSize = Rc<dyn Fn(i64, i64, &mut App)>;

pub(crate) struct AuthTerminal {
    terminal: Entity<Terminal>,
    view: Entity<TerminalView>,
    focus_handle: FocusHandle,
    /// T3's offset written up to, on servers that send one.
    written: Option<i64>,
    /// The output written, on servers that don't.
    shown: String,
    sent_size: Option<(i64, i64)>,
    send_input: SendInput,
    send_size: SendSize,
    resize: Option<Task<()>>,
    _subscriptions: Vec<Subscription>,
}

impl AuthTerminal {
    pub(crate) fn new(
        workspace: WeakEntity<Workspace>,
        project: Option<WeakEntity<Project>>,
        send_input: SendInput,
        send_size: SendSize,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        let settings = TerminalSettings::get_global(cx);
        let builder = TerminalBuilder::new_display_only(
            settings.cursor_shape,
            settings.alternate_scroll,
            settings.max_scroll_history_lines,
            window.window_handle().window_id().as_u64(),
            cx.background_executor(),
            PathStyle::local(),
        );
        let terminal = cx.new(|cx| builder.subscribe(cx));
        let view = cx.new(|cx| {
            TerminalView::new(
                terminal.clone(),
                workspace,
                None,
                project.unwrap_or_else(WeakEntity::new_invalid),
                window,
                cx,
            )
        });
        let subscriptions = vec![cx.observe(&view, |this, _, cx| {
            this.grid_changed(cx);
            cx.notify();
        })];
        Self {
            terminal,
            view,
            focus_handle: cx.focus_handle(),
            written: None,
            shown: String::new(),
            sent_size: None,
            send_input,
            send_size,
            resize: None,
            _subscriptions: subscriptions,
        }
    }

    /// Writes what's new in the sign-in's output.
    pub(crate) fn sync(&mut self, output: &str, offset: Option<i64>, cx: &mut Context<Self>) {
        let change = output_change(output, offset, self.written, &self.shown);
        match change {
            OutputChange::None => return,
            OutputChange::Append(text) => {
                let bytes = text.as_bytes().to_vec();
                self.terminal
                    .update(cx, |terminal, cx| terminal.write_raw_output(&bytes, cx));
            }
            OutputChange::Replace(text) => {
                let bytes = text.as_bytes().to_vec();
                let fresh = self.written.is_none() && self.shown.is_empty();
                self.terminal.update(cx, |terminal, cx| {
                    if fresh {
                        terminal.write_raw_output(&bytes, cx);
                    } else if let Err(error) = terminal.replace_display_output(&bytes, cx) {
                        log::error!("couldn't redraw a sign-in terminal: {error:#}");
                    }
                });
            }
        }
        self.written = offset;
        self.shown = output.to_string();
        cx.notify();
    }

    fn mode(&self, cx: &App) -> Modes {
        self.terminal.read(cx).last_content().mode
    }

    /// Sends input and does what Zed's terminal does locally on input: scrolls to the bottom
    /// and clears the selection.
    fn send(&mut self, text: String, cx: &mut Context<Self>) {
        if text.is_empty() {
            return;
        }
        self.terminal
            .update(cx, |terminal, _| terminal.input(text.clone().into_bytes()));
        (self.send_input)(text, cx);
    }

    fn key_down(&mut self, event: &KeyDownEvent, _: &mut Window, cx: &mut Context<Self>) {
        let keystroke = &event.keystroke;
        let modifiers = keystroke.modifiers;
        // Zed's terminal pastes with `cmd-v` on macOS and `ctrl-shift-v` elsewhere.
        let paste = keystroke.key == "v"
            && ((modifiers.platform && !modifiers.control)
                || (modifiers.control && modifiers.shift && !modifiers.platform));
        if paste {
            self.paste(cx);
            cx.stop_propagation();
            return;
        }
        let option_as_meta = TerminalSettings::get_global(cx).option_as_meta;
        if let Some(text) = keys::to_esc_str(keystroke, self.mode(cx), option_as_meta) {
            self.send(text.into_owned(), cx);
            cx.stop_propagation();
            return;
        }
        if !modifiers.control
            && !modifiers.platform
            && !modifiers.function
            && let Some(text) = keystroke.key_char.clone()
        {
            self.send(text, cx);
            cx.stop_propagation();
        }
    }

    /// Zed's `Terminal::paste`: bracketed when the program asks for it.
    fn paste(&mut self, cx: &mut Context<Self>) {
        let Some(text) = cx.read_from_clipboard().and_then(|clipboard| {
            clipboard.entries().iter().find_map(|entry| match entry {
                ClipboardEntry::String(string) => Some(string.text().to_string()),
                _ => None,
            })
        }) else {
            return;
        };
        let text = if self.mode(cx).contains(Modes::BRACKETED_PASTE) {
            format!("\x1b[200~{}\x1b[201~", text.replace('\x1b', ""))
        } else {
            text.replace("\r\n", "\r").replace('\n', "\r")
        };
        self.send(text, cx);
    }

    fn grid_size(&self, cx: &App) -> Option<(i64, i64)> {
        let bounds = self.terminal.read(cx).last_content().terminal_bounds;
        let size = (bounds.num_columns() as i64, bounds.num_lines() as i64);
        (size.0 > 0 && size.1 > 0).then(|| (size.0.clamp(1, MAX_COLS), size.1.clamp(1, MAX_ROWS)))
    }

    fn grid_changed(&mut self, cx: &mut Context<Self>) {
        let Some(size) = self.grid_size(cx) else {
            return;
        };
        if self.sent_size == Some(size) {
            self.resize = None;
            return;
        }
        self.resize = Some(cx.spawn(async move |this, cx| {
            cx.background_executor().timer(RESIZE_DEBOUNCE).await;
            this.update(cx, |this, cx| {
                let Some(size) = this.grid_size(cx) else {
                    return;
                };
                if this.sent_size != Some(size) {
                    this.sent_size = Some(size);
                    (this.send_size)(size.0, size.1, cx);
                }
            })
            .ok();
        }));
    }
}

impl Focusable for AuthTerminal {
    fn focus_handle(&self, _: &App) -> FocusHandle {
        self.focus_handle.clone()
    }
}

impl Render for AuthTerminal {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let focused = self.focus_handle.is_focused(window);
        div()
            .id("auth-terminal")
            .size_full()
            .track_focus(&self.focus_handle)
            .on_key_down(cx.listener(Self::key_down))
            // The terminal takes no clicks of its own: it has no local shell to select in.
            .capture_any_mouse_down(cx.listener(|this, _, window, cx| {
                window.focus(&this.focus_handle, cx);
                cx.stop_propagation();
            }))
            .when(!focused, |this| this.opacity(0.92))
            .child(self.view.clone())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use pretty_assertions::assert_eq;

    #[test]
    fn only_new_output_is_written() {
        // With offsets: the difference, or the whole tail after a gap.
        assert_eq!(
            output_change("hello", Some(5), None, ""),
            OutputChange::Replace("hello")
        );
        assert_eq!(
            output_change("hello world", Some(11), Some(5), "hello"),
            OutputChange::Append(" world")
        );
        assert_eq!(
            output_change("hello world", Some(11), Some(11), "hello world"),
            OutputChange::None
        );
        assert_eq!(
            output_change("tail", Some(9_000), Some(10), "x"),
            OutputChange::Replace("tail")
        );
        // Offsets count UTF-16 units: "é" is one, "🔑" two.
        assert_eq!(
            output_change("key 🔑 é", Some(10), Some(5), ""),
            OutputChange::Append(" 🔑 é")
        );
        // Without offsets: what follows what was shown, or all of it again.
        assert_eq!(
            output_change("Paste the code: ", None, None, "Paste"),
            OutputChange::Append(" the code: ")
        );
        assert_eq!(
            output_change("new screen", None, None, "old"),
            OutputChange::Replace("new screen")
        );
        assert_eq!(
            output_change("same", None, None, "same"),
            OutputChange::None
        );
    }
}

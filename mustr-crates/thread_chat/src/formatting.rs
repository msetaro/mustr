//! Markdown in the composer, as far as a plain editor goes toward T3's rich one
//! (`components/ComposerPromptEditorTiptap.tsx` in T3): the composer highlights Markdown as
//! it's typed (headings, emphasis, code, lists, links), and has T3's formatting keys: `mod+b`
//! bolds the selection (only a selection, as T3's does), `mod+i` italicizes, and `mod+e` makes
//! inline code. With text selected they wrap it, or unwrap it when it's already wrapped; with
//! none, italic and code put their marks around the cursor.

use crate::thread_view::ThreadView;
use editor::{MultiBufferOffset, SelectionEffects, ToOffset as _};
use gpui::{Context, Window, actions};

actions!(
    thread_chat,
    [
        /// Makes the composer's selection bold (`**`), or plain again.
        ToggleBold,
        /// Makes the composer's selection italic (`*`), or plain again.
        ToggleItalic,
        /// Makes the composer's selection inline code (`` ` ``), or plain again.
        ToggleInlineCode,
    ]
);

/// How a mark changes the text: the edits, and the selection after them, in bytes.
#[derive(Debug, PartialEq, Eq)]
pub(crate) struct Formatted {
    pub edits: Vec<(std::ops::Range<usize>, String)>,
    pub selection: std::ops::Range<usize>,
}

/// Whether `marker` is just before `start` and just after `end` in `text`. A `*` that's half
/// of a bold `**` doesn't count as italic's.
fn is_wrapped(text: &str, start: usize, end: usize, marker: &str) -> bool {
    let len = marker.len();
    if start < len || !text[..start].ends_with(marker) || !text[end..].starts_with(marker) {
        return false;
    }
    if marker == "*" {
        let doubled_before = text[..start - 1].ends_with('*');
        let doubled_after = text[end + 1..].starts_with('*');
        let tripled = text[..start].ends_with("***") && text[end..].starts_with("***");
        return !(doubled_before || doubled_after) || tripled;
    }
    true
}

/// What toggling `marker` on `selection` of `text` does, if anything: bold needs a selection.
pub(crate) fn toggle_mark(
    text: &str,
    selection: std::ops::Range<usize>,
    marker: &str,
    needs_selection: bool,
) -> Option<Formatted> {
    let (start, end) = (
        selection.start.min(text.len()),
        selection.end.min(text.len()),
    );
    let len = marker.len();
    if start == end {
        if needs_selection {
            return None;
        }
        return Some(Formatted {
            edits: vec![(start..start, format!("{marker}{marker}"))],
            selection: start + len..start + len,
        });
    }
    if is_wrapped(text, start, end, marker) {
        return Some(Formatted {
            edits: vec![
                (end..end + len, String::new()),
                (start - len..start, String::new()),
            ],
            selection: start - len..end - len,
        });
    }
    // A selection that takes in its marks unwraps too.
    let selected = &text[start..end];
    if selected.len() >= 2 * len
        && selected.starts_with(marker)
        && selected.ends_with(marker)
        && (marker != "*" || is_wrapped(text, start + len, end - len, marker))
    {
        return Some(Formatted {
            edits: vec![
                (end - len..end, String::new()),
                (start..start + len, String::new()),
            ],
            selection: start..end - 2 * len,
        });
    }
    Some(Formatted {
        edits: vec![
            (end..end, marker.to_string()),
            (start..start, marker.to_string()),
        ],
        selection: start + len..end + len,
    })
}

impl ThreadView {
    /// Highlights the composer's text as Markdown, once the language has loaded.
    pub(crate) fn highlight_composer_markdown(&mut self, cx: &mut Context<Self>) {
        if std::mem::replace(&mut self.composer_state.markdown_highlighting, true) {
            return;
        }
        let Some(languages) = self.language_registry.clone() else {
            return;
        };
        let language = languages.language_for_name("Markdown");
        let composer = self.composer.downgrade();
        cx.spawn(async move |_, cx| {
            let language = match language.await {
                Ok(language) => language,
                Err(error) => {
                    log::warn!("couldn't load Markdown for the composer: {error:#}");
                    return;
                }
            };
            composer
                .update(cx, |editor, cx| {
                    if let Some(buffer) = editor.buffer().read(cx).as_singleton() {
                        buffer.update(cx, |buffer, cx| buffer.set_language(Some(language), cx));
                    }
                })
                .ok();
        })
        .detach();
    }

    fn toggle_composer_mark(
        &mut self,
        marker: &str,
        needs_selection: bool,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.composer.update(cx, |editor, cx| {
            let text = editor.text(cx);
            let snapshot = editor.buffer().read(cx).snapshot(cx);
            let newest = editor.selections.newest_anchor();
            let (head, tail) = (
                newest.head().to_offset(&snapshot).0,
                newest.tail().to_offset(&snapshot).0,
            );
            let Some(formatted) = toggle_mark(
                &text,
                head.min(tail)..head.max(tail),
                marker,
                needs_selection,
            ) else {
                return;
            };
            editor.edit(
                formatted.edits.into_iter().map(|(range, text)| {
                    (
                        MultiBufferOffset(range.start)..MultiBufferOffset(range.end),
                        text,
                    )
                }),
                cx,
            );
            let selection = formatted.selection;
            editor.change_selections(SelectionEffects::default(), window, cx, |selections| {
                selections.select_ranges([
                    MultiBufferOffset(selection.start)..MultiBufferOffset(selection.end)
                ])
            });
        });
    }

    pub(crate) fn on_toggle_bold(
        &mut self,
        _: &ToggleBold,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.toggle_composer_mark("**", true, window, cx);
    }

    pub(crate) fn on_toggle_italic(
        &mut self,
        _: &ToggleItalic,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.toggle_composer_mark("*", false, window, cx);
    }

    pub(crate) fn on_toggle_inline_code(
        &mut self,
        _: &ToggleInlineCode,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.toggle_composer_mark("`", false, window, cx);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use pretty_assertions::assert_eq;

    /// The text after toggling `marker` on the `[`…`]` part of `marked`, with the selection
    /// after it marked the same way.
    fn toggled(marked: &str, marker: &str, needs_selection: bool) -> Option<String> {
        let start = marked.find('[').unwrap();
        let end = marked.find(']').unwrap() - 1;
        let text = marked.replace(['[', ']'], "");
        let formatted = toggle_mark(&text, start..end, marker, needs_selection)?;
        let mut text = text;
        let mut edits = formatted.edits;
        edits.sort_by_key(|(range, _)| std::cmp::Reverse(range.start));
        for (range, replacement) in edits {
            text.replace_range(range, &replacement);
        }
        text.insert(formatted.selection.end, ']');
        text.insert(formatted.selection.start, '[');
        Some(text)
    }

    #[test]
    fn marks_wrap_and_unwrap_the_selection() {
        assert_eq!(
            toggled("Use [retries] here", "**", true).as_deref(),
            Some("Use **[retries]** here")
        );
        assert_eq!(
            toggled("Use **[retries]** here", "**", true).as_deref(),
            Some("Use [retries] here")
        );
        assert_eq!(
            toggled("Use [**retries**] here", "**", true).as_deref(),
            Some("Use [retries] here")
        );
        assert_eq!(
            toggled("Run [cargo test]", "`", false).as_deref(),
            Some("Run `[cargo test]`")
        );
        assert_eq!(
            toggled("Run `[cargo test]`", "`", false).as_deref(),
            Some("Run [cargo test]")
        );
        // Bold isn't italic's to unwrap: italic goes inside it.
        assert_eq!(
            toggled("a **[b]** c", "*", false).as_deref(),
            Some("a ***[b]*** c")
        );
        assert_eq!(toggled("a *[b]* c", "*", false).as_deref(), Some("a [b] c"));
        // Bold needs a selection; italic and code put their marks around the cursor.
        assert_eq!(toggled("a [] c", "**", true), None);
        assert_eq!(toggled("a [] c", "*", false).as_deref(), Some("a *[]* c"));
        assert_eq!(toggled("a [] c", "`", false).as_deref(), Some("a `[]` c"));
    }
}

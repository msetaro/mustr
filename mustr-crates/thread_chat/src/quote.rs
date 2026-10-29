//! Quoting an agent's response in the composer, as T3's "Cite in composer" does
//! (`chat/AssistantSelectionToolbar.tsx`, and `packages/shared/src/assistantCitations.ts` in
//! T3): what's selected in the message, or the whole message, goes into the composer as T3's
//! citation link, `[Assistant quote](t3-citation://v1/<environment>/<thread>/<message>?text=…)`,
//! drawn as a chip. The message carries it in its text, as T3's does, so T3's app shows the
//! quote too, and mustr's timeline opens the quoted message from it (`file_links.rs`).

use crate::{
    mentions::{Chip, ChipKind},
    model::ItemKind,
    thread_view::ThreadView,
};
use gpui::{AnyElement, Context, Focusable as _, MouseButton, SharedString, Window, div};
use std::ops::Range;
use ui::{Color, IconButton, IconName, IconSize, Tooltip, prelude::*};

/// The longest quote T3 takes, in UTF-16 code units (`ASSISTANT_CITATION_MAX_TEXT_LENGTH`).
pub(crate) const MAX_QUOTE_LENGTH: usize = 8_000;
/// How much of the text around a quote T3 keeps to find it again
/// (`ASSISTANT_CITATION_CONTEXT_LENGTH`).
const CONTEXT_LENGTH: usize = 32;
/// How much of a quote its chip shows (`assistantCitationLabel`).
const LABEL_CHARS: usize = 64;
const TOOLTIP_CHARS: usize = 400;
const LINK_START: &str = "[Assistant quote](";
const HREF_PREFIX: &str = "t3-citation://v1/";

/// A quote of an agent's message (T3's `AssistantCitation`, version 1). Offsets count UTF-16
/// code units, as T3's do.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct Quote {
    pub environment_id: String,
    pub thread_id: String,
    pub message_id: String,
    pub text: String,
    pub start: usize,
    pub end: usize,
    pub prefix: String,
    pub suffix: String,
}

/// What's selected in an agent message's Markdown, noted as the timeline draws, since a click
/// on the Quote button clears the selection before the click lands.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub(crate) struct QuoteSelection {
    pub item_id: String,
    /// Where the selection starts in the message's text, in bytes.
    pub start: usize,
    pub text: String,
}

/// The composer's quoting state, on [`crate::composer::ComposerState`].
#[derive(Default)]
pub(crate) struct QuoteState {
    /// The selection as the timeline last drew it.
    selection: Option<QuoteSelection>,
    /// The selection when the Quote button was pressed.
    pressed: Option<QuoteSelection>,
}

/// `encodeURIComponent`, with `!'()*` encoded too, as T3 encodes a citation's path.
fn encode_path_part(value: &str) -> String {
    let mut encoded = String::with_capacity(value.len());
    for byte in value.bytes() {
        if byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_' | b'.' | b'~') {
            encoded.push(byte as char);
        } else {
            encoded.push_str(&format!("%{byte:02X}"));
        }
    }
    encoded
}

/// A value as `URLSearchParams` writes it (`application/x-www-form-urlencoded`).
fn encode_query_value(value: &str) -> String {
    let mut encoded = String::with_capacity(value.len());
    for byte in value.bytes() {
        match byte {
            b' ' => encoded.push('+'),
            b'*' | b'-' | b'.' | b'_' => encoded.push(byte as char),
            byte if byte.is_ascii_alphanumeric() => encoded.push(byte as char),
            byte => encoded.push_str(&format!("%{byte:02X}")),
        }
    }
    encoded
}

fn decode_query_value(value: &str) -> Option<String> {
    crate::mentions::percent_decode(&value.replace('+', " "))
}

/// The quote as T3 writes it into a message (`serializeAssistantCitation`).
pub(crate) fn quote_link(quote: &Quote) -> String {
    let path = [&quote.environment_id, &quote.thread_id, &quote.message_id]
        .map(|part| encode_path_part(part))
        .join("/");
    let query = [
        ("text", quote.text.clone()),
        ("start", quote.start.to_string()),
        ("end", quote.end.to_string()),
        ("prefix", quote.prefix.clone()),
        ("suffix", quote.suffix.clone()),
    ]
    .map(|(key, value)| format!("{key}={}", encode_query_value(&value)))
    .join("&");
    format!("{LINK_START}{HREF_PREFIX}{path}?{query})")
}

/// A quote's chip text: its first 64 characters, on one line (`assistantCitationLabel`).
pub(crate) fn quote_label(text: &str) -> String {
    let collapsed = text.split_whitespace().collect::<Vec<_>>().join(" ");
    match collapsed.char_indices().nth(LABEL_CHARS) {
        Some((index, _)) => format!("{}…", &collapsed[..index]),
        None => collapsed,
    }
}

/// A quote in the composer's text, to draw as a chip.
pub(crate) struct QuoteReference {
    pub range: Range<usize>,
    pub chip: Chip,
}

/// The quotes in `text`, where T3 finds them (`CITATION_LINK`): the link's destination runs to
/// the first space or `)`.
pub(crate) fn quote_references(text: &str) -> Vec<QuoteReference> {
    let mut found = Vec::new();
    let mut cursor = 0;
    let start_text = format!("{LINK_START}{HREF_PREFIX}");
    while let Some(offset) = text[cursor..].find(&start_text) {
        let start = cursor + offset;
        let href_start = start + LINK_START.len();
        let href_end = text[href_start..]
            .find(|c: char| c.is_whitespace() || c == ')')
            .map_or(text.len(), |end| href_start + end);
        cursor = href_end.max(start + 1);
        if !text[href_end..].starts_with(')') {
            continue;
        }
        let href = &text[href_start..href_end];
        let param = |name: &str| {
            href.split_once('?')?
                .1
                .split('&')
                .find_map(|pair| pair.strip_prefix(name)?.strip_prefix('='))
                .and_then(decode_query_value)
        };
        let Some(quoted) = param("text") else {
            continue;
        };
        let shown = param("comment").unwrap_or_else(|| quoted.clone());
        let tooltip = match quoted.char_indices().nth(TOOLTIP_CHARS) {
            Some((index, _)) => format!("{}…", &quoted[..index]),
            None => quoted,
        };
        found.push(QuoteReference {
            range: start..href_end + 1,
            chip: Chip {
                kind: ChipKind::Quote,
                label: quote_label(&shown),
                tooltip,
            },
        });
    }
    found
}

fn utf16_len(text: &str) -> usize {
    text.encode_utf16().count()
}

/// The end of `text`, at most `max` UTF-16 code units of it.
fn utf16_tail(text: &str, max: usize) -> &str {
    let mut units = 0;
    for (index, c) in text.char_indices().rev() {
        units += c.len_utf16();
        if units > max {
            return &text[index + c.len_utf8()..];
        }
    }
    text
}

/// The start of `text`, at most `max` UTF-16 code units of it.
fn utf16_head(text: &str, max: usize) -> &str {
    let mut units = 0;
    for (index, c) in text.char_indices() {
        units += c.len_utf16();
        if units > max {
            return &text[..index];
        }
    }
    text
}

/// The quote of `selected`, which starts at byte `start` of a message's `text`, with its
/// place and the text around it. Whitespace around the selection is left out.
pub(crate) fn quote_of(
    text: &str,
    start: usize,
    selected: &str,
    ids: (&str, &str, &str),
) -> Option<Quote> {
    let leading = selected.len() - selected.trim_start().len();
    let quoted = selected.trim();
    if quoted.is_empty() || utf16_len(quoted) > MAX_QUOTE_LENGTH {
        return None;
    }
    let start = start + leading;
    let end = start + quoted.len();
    let (before, after) = (text.get(..start)?, text.get(end..)?);
    let (environment_id, thread_id, message_id) = ids;
    Some(Quote {
        environment_id: environment_id.to_string(),
        thread_id: thread_id.to_string(),
        message_id: message_id.to_string(),
        text: quoted.to_string(),
        start: utf16_len(before),
        end: utf16_len(before) + utf16_len(quoted),
        prefix: utf16_tail(before, CONTEXT_LENGTH).to_string(),
        suffix: utf16_head(after, CONTEXT_LENGTH).to_string(),
    })
}

/// A quote as a Markdown block quote, for a server whose environment id mustr hasn't read.
fn block_quote(text: &str) -> String {
    text.lines()
        .map(|line| format!("> {line}").trim_end().to_string())
        .collect::<Vec<_>>()
        .join("\n")
}

impl QuoteState {
    /// Notes a selection, as the timeline does when it draws one, for tests.
    #[cfg(test)]
    pub(crate) fn quote_selection_for_test(&mut self, item_id: &str, start: usize, text: &str) {
        self.selection = Some(QuoteSelection {
            item_id: item_id.to_string(),
            start,
            text: text.to_string(),
        });
    }
}

impl ThreadView {
    /// Notes what's selected in an agent message's Markdown, as it's drawn.
    pub(crate) fn note_quote_selection(&mut self, item_id: &str, cx: &Context<Self>) {
        let Some(markdown) = self.markdown.get(item_id) else {
            return;
        };
        let markdown = markdown.read(cx);
        let state = &mut self.composer_state.quote;
        match markdown.selected_source() {
            Some(selected) if !selected.trim().is_empty() => {
                // The selection is a slice of the Markdown's source.
                let start = selected.as_ptr() as usize - markdown.source().as_ptr() as usize;
                state.selection = Some(QuoteSelection {
                    item_id: item_id.to_string(),
                    start,
                    text: selected.to_string(),
                });
            }
            _ => {
                if state
                    .selection
                    .as_ref()
                    .is_some_and(|selection| selection.item_id == item_id)
                {
                    state.selection = None;
                }
            }
        }
    }

    /// The Quote button of an agent message: it quotes what's selected in the message, or the
    /// whole message. It shows on hover, and while something in the message is selected.
    pub(crate) fn render_quote_button(
        &mut self,
        item_id: &str,
        text: &str,
        cx: &mut Context<Self>,
    ) -> Option<AnyElement> {
        if self.thread_id.is_none() || text.trim().is_empty() {
            return None;
        }
        let selection = self
            .composer_state
            .quote
            .selection
            .as_ref()
            .filter(|selection| selection.item_id == item_id);
        let selected = selection.is_some();
        let quoted = selection.map_or(text, |selection| selection.text.as_str());
        let too_long = utf16_len(quoted.trim()) > MAX_QUOTE_LENGTH;
        let tooltip = match (selected, too_long) {
            (true, true) => "Select less to quote it: at most 8,000 characters",
            (false, true) => "Select part of this response to quote it",
            (true, false) => "Quote the selection in the composer",
            (false, false) => "Quote this response in the composer",
        };
        let selector = format!("thread-quote-{item_id}");
        let item_id = item_id.to_string();
        Some(
            div()
                .flex_none()
                .debug_selector(|| selector)
                .when(!selected, |this| {
                    this.visible_on_hover(SharedString::from(format!("message-{item_id}")))
                })
                .on_mouse_down(
                    MouseButton::Left,
                    cx.listener(|this, _, _, _| {
                        let quote = &mut this.composer_state.quote;
                        quote.pressed = quote.selection.clone();
                    }),
                )
                .child(
                    IconButton::new(
                        SharedString::from(format!("quote-message-{item_id}")),
                        IconName::Quote,
                    )
                    .icon_size(IconSize::XSmall)
                    .icon_color(if selected {
                        Color::Accent
                    } else {
                        Color::Muted
                    })
                    .disabled(too_long)
                    .tooltip(Tooltip::text(tooltip))
                    .on_click(cx.listener(move |this, _, window, cx| {
                        this.quote_in_composer(&item_id, window, cx)
                    })),
                )
                .into_any_element(),
        )
    }

    /// Puts a quote of an agent message in the composer, at the cursor when the composer has
    /// focus, or at the end, as T3's "Cite in composer" does (`insertComposerText` with
    /// `ensureLeadingBoundary`).
    pub(crate) fn quote_in_composer(
        &mut self,
        item_id: &str,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let pressed = self
            .composer_state
            .quote
            .pressed
            .take()
            .or_else(|| self.composer_state.quote.selection.clone())
            .filter(|selection| selection.item_id == item_id);
        // The quote names the server's environment, from its config: read it first.
        if !self.composer_state.menu.has_server_info() {
            let config = self.backend.call_raw(
                <t3_api::rpc::ServerGetConfig as t3_api::RpcMethod>::TAG,
                serde_json::json!({}),
                crate::backend::Access::Read,
            );
            let item_id = item_id.to_string();
            self._tasks.push(cx.spawn_in(window, async move |this, cx| {
                let config = config.await;
                this.update_in(cx, |this, window, cx| {
                    this.composer_state.menu.set_server_config(config);
                    this.composer_state.quote.pressed = pressed;
                    this.quote_in_composer(&item_id, window, cx);
                })
                .ok();
            }));
            return;
        }
        let Some(state) = self.thread_state() else {
            return;
        };
        let Some(item) = state.items().iter().find(|item| item.id == item_id) else {
            return;
        };
        let ItemKind::AssistantMessage { text, .. } = &item.kind else {
            return;
        };
        let (start, selected) = match &pressed {
            Some(selection) => (selection.start, selection.text.as_str()),
            None => (0, text.as_str()),
        };
        let environment_id = self
            .composer_state
            .menu
            .environment_id()
            .map(str::to_string);
        let ids = (
            environment_id.as_deref().unwrap_or_default(),
            state.thread_id(),
            item.entry_id.as_str(),
        );
        let Some(quote) = quote_of(text, start, selected, ids) else {
            return;
        };
        let (insertion, chip) = if environment_id.is_some() {
            let link = quote_link(&quote);
            let chip = quote_references(&link)
                .pop()
                .map(|reference| reference.chip);
            (link, chip)
        } else {
            (format!("{}\n\n", block_quote(&quote.text)), None)
        };
        let composer_text = self.composer.read(cx).text(cx);
        let focused = self.composer.focus_handle(cx).is_focused(window);
        let at = match self.composer_cursor(cx) {
            Some((_, cursor)) if focused => cursor,
            _ => composer_text.len(),
        };
        let before = match composer_text[..at].chars().next_back() {
            Some(c) if !c.is_whitespace() => " ",
            _ => "",
        };
        let after = if chip.is_some() && !composer_text[at..].starts_with(char::is_whitespace) {
            " "
        } else {
            ""
        };
        let start =
            self.replace_in_composer(at..at, &format!("{before}{insertion}{after}"), window, cx)
                + before.len();
        if let Some(chip) = chip {
            self.add_chip(start..start + insertion.len(), chip, window, cx);
        }
        self.save_draft(cx);
        window.focus(&self.composer.focus_handle(cx), cx);
        cx.notify();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use pretty_assertions::assert_eq;

    fn quote(text: &str) -> Quote {
        Quote {
            environment_id: "env-1".into(),
            thread_id: "thread-1".into(),
            message_id: "message-i5".into(),
            text: text.into(),
            start: 6,
            end: 6 + text.len(),
            prefix: "Done. ".into(),
            suffix: " now wraps".into(),
        }
    }

    #[test]
    fn quotes_are_written_as_t3_writes_them() {
        // `serializeAssistantCitation` in T3, with `URLSearchParams` encoding the text.
        assert_eq!(
            quote_link(&quote("`withRetry` (x*2)!")),
            "[Assistant quote](t3-citation://v1/env-1/thread-1/message-i5?\
             text=%60withRetry%60+%28x*2%29%21&start=6&end=24&prefix=Done.+\
             &suffix=+now+wraps)"
        );
        assert_eq!(encode_path_part("a b/c!'()*~"), "a%20b%2Fc%21%27%28%29%2A~");
    }

    #[test]
    fn quotes_in_the_text_become_chips() {
        let link = quote_link(&quote("Retries back off\nexponentially, up to 5 times."));
        let text = format!("See {link} and fix it");
        let found = quote_references(&text);
        assert_eq!(found.len(), 1);
        assert_eq!(&text[found[0].range.clone()], link);
        assert_eq!(
            found[0].chip.label,
            "Retries back off exponentially, up to 5 times."
        );
        assert_eq!(
            found[0].chip.tooltip,
            "Retries back off\nexponentially, up to 5 times."
        );
        assert_eq!(quote_label(&"word ".repeat(20)).chars().count(), 65);
        // A link T3 wouldn't read isn't a chip.
        assert!(quote_references("[Assistant quote](t3-citation://v1/a b)").is_empty());
    }

    #[test]
    fn a_quote_keeps_its_place_and_whats_around_it() {
        let text = "Done. `withRetry` now wraps `post()`, with backoff — 💡 tip.";
        let start = text.find("`withRetry`").unwrap();
        let quoted = quote_of(
            text,
            start - 1,
            " `withRetry` now wraps ",
            ("env-1", "thread-1", "message-i5"),
        )
        .unwrap();
        assert_eq!(quoted.text, "`withRetry` now wraps");
        assert_eq!((quoted.start, quoted.end), (6, 27));
        assert_eq!(quoted.prefix, "Done. ");
        assert_eq!(quoted.suffix, " `post()`, with backoff — 💡 tip");
        assert_eq!(utf16_len(&quoted.suffix), CONTEXT_LENGTH);
        // Nothing, or too much, isn't quoted.
        assert_eq!(quote_of(text, 0, "  ", ("e", "t", "m")), None);
        let long = "x".repeat(MAX_QUOTE_LENGTH + 1);
        assert_eq!(quote_of(&long, 0, &long, ("e", "t", "m")), None);
    }
}

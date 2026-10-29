//! The prompt stash, as T3's (`promptStashStore.ts`, `chat/useComposerStash.ts` and
//! `chat/ComposerStashMenu.tsx` in T3): `mod+s` in the composer puts the draft away (its text,
//! chips and attachments) and clears the composer; `mod+s` in an empty composer brings the only
//! stashed prompt back, or shows the stash when there are several. The stash is the app's, not
//! a thread's, newest first, at most 20 prompts. Its text and chips are kept across restarts;
//! attachments only while mustr runs.

use crate::{thread_view::ThreadView, timeline::truncate};
use chrono::{DateTime, Utc};
use gpui::{
    AnyElement, App, Context, Global, IntoElement, ParentElement as _, SharedString, Styled as _,
    Window, actions, div, px,
};
use mustr_ui::{TextButton, palette, tokens, tokens::scaled};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use ui::{
    Color, Icon, IconButton, IconName, IconSize, Label, LabelCommon as _, LabelSize, Tooltip,
    prelude::*,
};

actions!(
    thread_chat,
    [
        /// Stashes the composer's prompt for later and clears the composer; in an empty
        /// composer, brings a stashed prompt back.
        StashPrompt,
    ]
);

/// T3's `MAX_STASH_ENTRIES`.
pub(crate) const MAX_STASHED: usize = 20;
/// How much of a prompt the stash's list shows (T3's 90).
const SNIPPET_CHARS: usize = 90;
const STASH_KEY: &str = "mustr-prompt-stash";

/// A stashed prompt.
#[derive(Clone, Debug)]
pub(crate) struct Stashed {
    pub id: String,
    pub created_at: DateTime<Utc>,
    pub text: String,
    /// The records of the chips in its text.
    pub records: Vec<Value>,
    pub images: Vec<crate::attachments::ComposerImage>,
    pub files: Vec<crate::attachments::ComposerFile>,
    /// The names of attachments a restart lost.
    pub lost: Vec<String>,
}

/// What's kept of a stashed prompt across restarts.
#[derive(Serialize, Deserialize)]
struct SavedStash {
    id: String,
    created_at: DateTime<Utc>,
    text: String,
    #[serde(default)]
    records: Vec<Value>,
    #[serde(default)]
    lost: Vec<String>,
}

/// The app's stash.
#[derive(Default)]
pub(crate) struct PromptStash {
    pub(crate) entries: Vec<Stashed>,
    loaded: bool,
}

impl Global for PromptStash {}

impl PromptStash {
    /// The stash, read from mustr's database the first time.
    pub(crate) fn get(cx: &mut App) -> &mut Self {
        let stash = cx.default_global::<Self>();
        if stash.loaded {
            return cx.global_mut::<Self>();
        }
        stash.loaded = true;
        let saved = cx
            .has_global::<db::AppDatabase>()
            .then(|| db::kvp::KeyValueStore::global(cx).read_kvp(STASH_KEY).ok())
            .flatten()
            .flatten()
            .and_then(|saved| serde_json::from_str::<Vec<SavedStash>>(&saved).ok())
            .unwrap_or_default();
        let stash = cx.global_mut::<Self>();
        stash.entries = saved
            .into_iter()
            .map(|saved| Stashed {
                id: saved.id,
                created_at: saved.created_at,
                text: saved.text,
                records: saved.records,
                images: Vec::new(),
                files: Vec::new(),
                lost: saved.lost,
            })
            .collect();
        stash
    }

    /// Keeps the stash's text and chips in mustr's database; attachments don't outlive mustr,
    /// so they're kept as names, to say what was lost.
    fn save(cx: &mut App) {
        if !cx.has_global::<db::AppDatabase>() {
            return;
        }
        let saved = cx
            .global::<Self>()
            .entries
            .iter()
            .map(|entry| SavedStash {
                id: entry.id.clone(),
                created_at: entry.created_at,
                text: entry.text.clone(),
                records: entry.records.clone(),
                lost: entry
                    .lost
                    .iter()
                    .cloned()
                    .chain(entry.images.iter().map(|image| image.name.clone()))
                    .chain(entry.files.iter().map(|file| file.name.clone()))
                    .collect(),
            })
            .collect::<Vec<_>>();
        let Ok(saved) = serde_json::to_string(&saved) else {
            return;
        };
        let store = db::kvp::KeyValueStore::global(cx);
        cx.background_spawn(async move {
            if let Err(error) = store.write_kvp(STASH_KEY.to_string(), saved).await {
                log::warn!("couldn't save the prompt stash: {error:#}");
            }
        })
        .detach();
    }
}

/// A stashed prompt in one line, as the stash's list shows it: quotes as their text.
fn snippet(text: &str) -> String {
    let mut plain = text.to_string();
    for quote in crate::quote::quote_references(text).into_iter().rev() {
        plain.replace_range(quote.range, &quote.chip.label);
    }
    let line = plain.split_whitespace().collect::<Vec<_>>().join(" ");
    truncate(&line, SNIPPET_CHARS)
}

/// "just now", "5m ago", "3h ago", "2d ago".
fn age(created_at: DateTime<Utc>, now: DateTime<Utc>) -> String {
    let seconds = (now - created_at).num_seconds().max(0);
    match seconds {
        0..60 => "just now".to_string(),
        60..3600 => format!("{}m ago", seconds / 60),
        3600..86_400 => format!("{}h ago", seconds / 3600),
        _ => format!("{}d ago", seconds / 86_400),
    }
}

/// The composer's view of the stash, on [`crate::composer::ComposerState`].
#[derive(Default)]
pub(crate) struct StashState {
    /// The stash's list shows.
    pub(crate) open: bool,
}

impl ThreadView {
    /// `mod+s`: stashes the composer's prompt, or brings one back into an empty composer.
    pub(crate) fn on_stash_prompt(
        &mut self,
        _: &StashPrompt,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let text = self.composer.read(cx).text(cx);
        if !text.trim().is_empty() || !self.images.is_empty() {
            self.stash_prompt(window, cx);
            return;
        }
        let count = PromptStash::get(cx).entries.len();
        match count {
            0 => {}
            1 => {
                let id = PromptStash::get(cx).entries[0].id.clone();
                self.restore_stashed(&id, window, cx);
            }
            _ => {
                self.composer_state.stash.open = !self.composer_state.stash.open;
                cx.notify();
            }
        }
    }

    /// Puts the composer's prompt in the stash, and clears the composer. Like T3's, it waits
    /// for attachments still going up.
    pub(crate) fn stash_prompt(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.images.uploading() || self.images.any_failed() {
            self.error = Some(
                "Wait for attachments to finish uploading, or remove failed uploads, to stash \
                 the prompt."
                    .into(),
            );
            cx.notify();
            return;
        }
        let text = self.composer.read(cx).text(cx);
        let records = crate::mentions::referenced_records(&text, &self.composer_state.menu.records)
            .into_iter()
            .map(|(_, record)| record)
            .collect();
        let images = std::mem::take(&mut self.images.composer);
        let files = std::mem::take(&mut self.images.files);
        let entry = Stashed {
            id: uuid::Uuid::new_v4().to_string(),
            created_at: Utc::now(),
            text: text.trim().to_string(),
            records,
            images,
            files,
            lost: Vec::new(),
        };
        let stash = PromptStash::get(cx);
        stash.entries.insert(0, entry);
        let dropped = stash.entries.len() > MAX_STASHED;
        stash.entries.truncate(MAX_STASHED);
        PromptStash::save(cx);
        self.composer
            .update(cx, |editor, cx| editor.clear(window, cx));
        self.images.intake_note = Some(if dropped {
            "Prompt stashed; the oldest stashed prompt made room for it.".into()
        } else {
            "Prompt stashed. Press Ctrl+S in an empty composer to bring it back."
                .replace(
                    "Ctrl",
                    if cfg!(target_os = "macos") {
                        "Cmd"
                    } else {
                        "Ctrl"
                    },
                )
                .into()
        });
        self.error = None;
        self.save_draft(cx);
        cx.notify();
    }

    /// Brings a stashed prompt back: its text after what's in the composer, its chips and its
    /// attachments, and takes it out of the stash.
    pub(crate) fn restore_stashed(
        &mut self,
        id: &str,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let stash = PromptStash::get(cx);
        let Some(index) = stash.entries.iter().position(|entry| entry.id == id) else {
            return;
        };
        let entry = stash.entries.remove(index);
        PromptStash::save(cx);
        let current = self.composer.read(cx).text(cx);
        let text = if current.trim().is_empty() {
            entry.text.clone()
        } else {
            format!("{}\n\n{}", current.trim_end(), entry.text)
        };
        for record in entry.records {
            self.composer_state.menu.remember_record(record);
        }
        self.composer.update(cx, |editor, cx| {
            editor.set_text(text, window, cx);
            editor.move_to_end(&editor::actions::MoveToEnd, window, cx);
        });
        self.draw_mention_chips(window, cx);
        self.images.restore(entry.images, entry.files);
        self.images.intake_note = (!entry.lost.is_empty()).then(|| {
            format!(
                "Restarting mustr lost what was attached: {}.",
                entry.lost.join(", ")
            )
            .into()
        });
        self.composer_state.stash.open = false;
        self.save_draft(cx);
        window.focus(&gpui::Focusable::focus_handle(&self.composer, cx), cx);
        cx.notify();
    }

    fn delete_stashed(&mut self, id: &str, cx: &mut Context<Self>) {
        let stash = PromptStash::get(cx);
        stash.entries.retain(|entry| entry.id != id);
        if stash.entries.is_empty() {
            self.composer_state.stash.open = false;
        }
        PromptStash::save(cx);
        cx.notify();
    }

    /// The stash's badge above the composer, and its list when it's open.
    pub(crate) fn render_stash(&mut self, cx: &mut Context<Self>) -> Option<AnyElement> {
        if self.new_thread.is_some() && PromptStash::get(cx).entries.is_empty() {
            return None;
        }
        let palette = palette(cx);
        let open = self.composer_state.stash.open;
        let entries = PromptStash::get(cx)
            .entries
            .iter()
            .map(|entry| {
                (
                    entry.id.clone(),
                    snippet(&entry.text),
                    entry.created_at,
                    entry.images.len() + entry.files.len(),
                    entry.lost.len(),
                )
            })
            .collect::<Vec<_>>();
        if entries.is_empty() {
            return None;
        }
        let count = entries.len();
        let now = Utc::now();
        let badge = div().debug_selector(|| "thread-stash".into()).child(
            TextButton::new("thread-stash", format!("Stashed prompts: {count}"))
                .tooltip("Show the stashed prompts")
                .on_click(cx.listener(|this, _, _, cx| {
                    this.composer_state.stash.open = !this.composer_state.stash.open;
                    cx.notify();
                })),
        );
        let list = open.then(|| {
            v_flex()
                .debug_selector(|| "thread-stash-list".into())
                .w_full()
                .p_1()
                .gap_0p5()
                .rounded(scaled(tokens::CORNER_RADIUS))
                .border(tokens::BORDER_WIDTH)
                .border_color(palette.border.hsla())
                .bg(palette.surface.hsla())
                .children(entries.into_iter().enumerate().map(
                    |(index, (id, snippet, created_at, attachments, lost))| {
                        let mut details = vec![age(created_at, now)];
                        if attachments > 0 {
                            details.push(if attachments == 1 {
                                "1 attachment".to_string()
                            } else {
                                format!("{attachments} attachments")
                            });
                        }
                        if lost > 0 {
                            details.push(format!("{lost} lost"));
                        }
                        let restore_id = id.clone();
                        h_flex()
                            .id(("stashed", index))
                            .debug_selector(move || format!("thread-stashed-{index}"))
                            .w_full()
                            .gap_2()
                            .px_2()
                            .py(scaled(px(3.)))
                            .rounded(scaled(tokens::CONTROL_RADIUS))
                            .cursor_pointer()
                            .hover(|style| style.bg(palette.surface_2.hsla()))
                            .on_click(cx.listener(move |this, _, window, cx| {
                                this.restore_stashed(&restore_id, window, cx)
                            }))
                            .child(
                                Icon::new(IconName::HistoryRerun)
                                    .size(IconSize::XSmall)
                                    .color(Color::Muted),
                            )
                            .child(
                                div()
                                    .flex_1()
                                    .min_w_0()
                                    .overflow_hidden()
                                    .whitespace_nowrap()
                                    .text_ellipsis()
                                    .text_size(tokens::text_size(tokens::TEXT_SIZE_SMALL))
                                    .child(SharedString::from(snippet)),
                            )
                            .child(
                                Label::new(details.join(" · "))
                                    .size(LabelSize::XSmall)
                                    .color(Color::Muted),
                            )
                            .child(
                                IconButton::new(("delete-stashed", index), IconName::Close)
                                    .icon_size(IconSize::XSmall)
                                    .icon_color(Color::Muted)
                                    .tooltip(Tooltip::text("Delete this stashed prompt"))
                                    .on_click(cx.listener(move |this, _, _, cx| {
                                        cx.stop_propagation();
                                        this.delete_stashed(&id, cx)
                                    })),
                            )
                    },
                ))
                .into_any_element()
        });
        Some(
            v_flex()
                .w_full()
                .gap_1()
                .child(h_flex().child(badge))
                .children(list)
                .into_any_element(),
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use pretty_assertions::assert_eq;

    #[test]
    fn the_stash_lists_prompts_in_a_line_with_their_age() {
        let now = Utc::now();
        assert_eq!(age(now, now), "just now");
        assert_eq!(age(now - chrono::Duration::minutes(5), now), "5m ago");
        assert_eq!(age(now - chrono::Duration::hours(3), now), "3h ago");
        assert_eq!(age(now - chrono::Duration::days(2), now), "2d ago");
        assert_eq!(snippet("Fix the\n\nretry   loop"), "Fix the retry loop");
        let quote = crate::quote::quote_link(&crate::quote::Quote {
            environment_id: "e".into(),
            thread_id: "t".into(),
            message_id: "m".into(),
            text: "Retries back off".into(),
            start: 0,
            end: 16,
            prefix: String::new(),
            suffix: String::new(),
        });
        assert_eq!(snippet(&format!("Why {quote}?")), "Why Retries back off?");
        assert_eq!(
            snippet(&"word ".repeat(40)).chars().count(),
            SNIPPET_CHARS + 1
        );
    }
}

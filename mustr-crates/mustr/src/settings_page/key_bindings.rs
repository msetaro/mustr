//! Keyboard shortcuts, in the Settings page: every key binding mustr has, searchable by action,
//! keys or context, and the actions without one; change a shortcut by pressing the new keys, give
//! an action one, remove one, or put an action's back the way mustr has it.
//!
//! Zed's own keymap editor can't be used: it opens `keymap.json` ("Edit in JSON"), which mustr
//! never does, and it's drawn in Zed's look. This list writes the user's key bindings file with
//! Zed's own editing functions (`KeymapFile::update_keybinding`), as Zed's editor does, so the
//! file stays in Zed's format and keeps what it had; changing or removing one of mustr's or Zed's
//! bindings adds the user's binding and an `unbind` of the old keys. mustr reloads the file when
//! it changes (`keymap.rs`), and the list follows.

use super::rows::{self, Query};
use crate::keymap::{INITIAL_KEYMAP, KeymapVersion};
use anyhow::{Context as _, Result};
use collections::{HashMap, HashSet};
use command_palette_hooks::CommandPaletteFilter;
use editor::{Editor, EditorEvent};
use fs::Fs;
use gpui::{
    Action, AnyElement, App, AppContext as _, Context, Entity, FocusHandle, FontWeight,
    InteractiveElement as _, IntoElement, KeybindingKeystroke, Keystroke, KeystrokeEvent,
    ParentElement as _, PlatformKeyboardMapper, Render, SharedString,
    StatefulInteractiveElement as _, Styled as _, Subscription, Task, UniformListScrollHandle,
    Window, div, px, uniform_list,
};
use mustr_ui::{
    ButtonStyle, Kbd, KeyStyle, TextButton, key_label, palette, tokens, tokens::scaled,
};
use serde_json::Value;
use settings::{KeybindSource, KeybindUpdateOperation, KeybindUpdateTarget, KeymapFile};
use std::ops::Range;
use ui::{
    Color, IconButton, IconName, IconSize, Label, LabelCommon as _, LabelSize, Tooltip, prelude::*,
};

/// Which rows show.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Filter {
    /// Actions with keys.
    Bound,
    /// Every action the command palette offers, with or without keys.
    All,
    /// The ones the user changed.
    Changed,
}

/// One binding, or one action without keys.
#[derive(Clone, Debug)]
pub(crate) struct BindingRow {
    pub action_name: &'static str,
    pub label: SharedString,
    pub keystrokes: Vec<KeybindingKeystroke>,
    /// The keys as the keymap writes them (`cmd-shift-p`, chords separated by spaces).
    pub keys: String,
    /// The keys as mustr shows them (`⌘⇧P`), for the search.
    pub keys_label: String,
    pub context: Option<SharedString>,
    pub source: Option<KeybindSource>,
    pub arguments: Option<SharedString>,
    pub documentation: Option<&'static str>,
}

impl BindingRow {
    fn matches(&self, query: &Query) -> bool {
        query.matches(&[
            &self.label,
            self.action_name,
            &self.keys,
            &self.keys_label,
            self.context.as_deref().unwrap_or_default(),
        ])
    }

    fn spec(&self) -> BindingSpec {
        BindingSpec {
            action: self.action_name.to_string(),
            context: self.context.as_ref().map(ToString::to_string),
            keystrokes: self.keystrokes.clone(),
            arguments: self.arguments.as_ref().map(ToString::to_string),
            source: self.source.unwrap_or(KeybindSource::User),
        }
    }
}

/// The keys as mustr shows them: one cap's label per keystroke.
fn key_labels(keystrokes: &[KeybindingKeystroke]) -> Vec<String> {
    let style = KeyStyle::platform();
    keystrokes
        .iter()
        .map(|keystroke| key_label(keystroke.modifiers(), keystroke.key(), style))
        .collect()
}

fn unparse(keystrokes: &[KeybindingKeystroke]) -> String {
    keystrokes
        .iter()
        .map(KeybindingKeystroke::unparse)
        .collect::<Vec<_>>()
        .join(" ")
}

/// A binding as the user's file names it.
#[derive(Clone, Debug, PartialEq)]
pub(crate) struct BindingSpec {
    pub action: String,
    pub context: Option<String>,
    pub keystrokes: Vec<KeybindingKeystroke>,
    pub arguments: Option<String>,
    pub source: KeybindSource,
}

impl BindingSpec {
    fn target(&self) -> KeybindUpdateTarget<'_> {
        KeybindUpdateTarget {
            context: self.context.as_deref(),
            keystrokes: &self.keystrokes,
            action_name: &self.action,
            action_arguments: self.arguments.as_deref(),
        }
    }
}

/// A change to the user's key bindings file.
#[derive(Clone, Debug, PartialEq)]
pub(crate) enum KeymapChange {
    /// New keys for a binding.
    Replace {
        binding: BindingSpec,
        keystrokes: Vec<KeybindingKeystroke>,
    },
    /// Keys for an action without any.
    Add {
        action: String,
        keystrokes: Vec<KeybindingKeystroke>,
    },
    /// No more keys for this binding.
    Remove { binding: BindingSpec },
    /// Drops what the user's file says about an action, so mustr's and Zed's bindings apply.
    Reset { action: String },
}

impl KeymapChange {
    /// The user's key bindings file with this change.
    pub fn apply(
        &self,
        text: String,
        keyboard_mapper: &dyn PlatformKeyboardMapper,
        deprecated_aliases: &HashMap<&'static str, &'static str>,
    ) -> Result<String> {
        let tab_size = settings::infer_json_indent_size(&text);
        let operation = match self {
            Self::Replace {
                binding,
                keystrokes,
            } => KeybindUpdateOperation::Replace {
                target: binding.target(),
                source: KeybindUpdateTarget {
                    keystrokes,
                    ..binding.target()
                },
                target_keybind_source: binding.source,
            },
            Self::Add { action, keystrokes } => KeybindUpdateOperation::add(KeybindUpdateTarget {
                context: None,
                keystrokes,
                action_name: action,
                action_arguments: None,
            }),
            Self::Remove { binding } => KeybindUpdateOperation::Remove {
                target: binding.target(),
                target_keybind_source: binding.source,
            },
            Self::Reset { action } => {
                return remove_action(&text, action, deprecated_aliases, tab_size);
            }
        };
        KeymapFile::update_keybinding(
            operation,
            text,
            tab_size,
            keyboard_mapper,
            deprecated_aliases,
        )
    }
}

/// The action a keymap entry names: `"editor::Copy"`, or `["editor::Copy", {…}]`.
fn entry_action(value: &Value) -> Option<&str> {
    match value {
        Value::String(name) => Some(name),
        Value::Array(items) => items.first()?.as_str(),
        _ => None,
    }
}

fn canonical<'a>(name: &'a str, aliases: &HashMap<&'static str, &'static str>) -> &'a str {
    aliases.get(name).copied().unwrap_or(name)
}

/// The actions the user's key bindings file binds or unbinds.
pub(crate) fn user_actions(
    text: &str,
    aliases: &HashMap<&'static str, &'static str>,
) -> HashSet<String> {
    let Ok(Value::Array(sections)) = settings::parse_json_with_comments::<Value>(text) else {
        return HashSet::default();
    };
    sections
        .iter()
        .flat_map(|section| {
            ["bindings", "unbind"]
                .into_iter()
                .filter_map(|kind| section.get(kind)?.as_object())
                .flat_map(|entries| entries.values())
        })
        .filter_map(entry_action)
        .map(|name| canonical(name, aliases).to_string())
        .collect()
}

/// `text` without its bindings and unbindings of `action`, and without the sections that held
/// only those, keeping the rest as it was.
pub(crate) fn remove_action(
    text: &str,
    action: &str,
    aliases: &HashMap<&'static str, &'static str>,
    tab_size: usize,
) -> Result<String> {
    let parsed: Value =
        settings::parse_json_with_comments(text).context("the key bindings file isn't valid")?;
    let Value::Array(sections) = parsed else {
        anyhow::bail!("the key bindings file isn't a list of sections");
    };
    let mut text = text.to_string();
    for (index, section) in sections.iter().enumerate().rev() {
        let mut total = 0;
        let mut found = Vec::new();
        for kind in ["bindings", "unbind"] {
            let Some(entries) = section.get(kind).and_then(Value::as_object) else {
                continue;
            };
            total += entries.len();
            for (keys, value) in entries {
                if entry_action(value).is_some_and(|name| canonical(name, aliases) == action) {
                    found.push((kind, keys.clone()));
                }
            }
        }
        if found.is_empty() {
            continue;
        }
        if found.len() == total {
            let (range, replacement) = settings::replace_top_level_array_value_in_json_text(
                &text,
                &[] as &[&str],
                None,
                None,
                index,
                tab_size,
            );
            text.replace_range(range, &replacement);
            continue;
        }
        // Later entries first, so the earlier ones stay where they were; a kind that loses all
        // its entries goes whole.
        for kind in ["unbind", "bindings"] {
            let entries = section
                .get(kind)
                .and_then(Value::as_object)
                .map_or(0, |entries| entries.len());
            let removed = found
                .iter()
                .filter(|(found_kind, _)| *found_kind == kind)
                .map(|(_, keys)| keys.as_str())
                .collect::<Vec<_>>();
            if removed.is_empty() {
                continue;
            }
            let paths = if removed.len() == entries {
                vec![vec![kind]]
            } else {
                removed.iter().rev().map(|keys| vec![kind, *keys]).collect()
            };
            for path in paths {
                let (range, replacement) = settings::replace_top_level_array_value_in_json_text(
                    &text, &path, None, None, index, tab_size,
                );
                text.replace_range(range, &replacement);
            }
        }
    }
    settings::parse_json_with_comments::<Value>(&text)
        .context("removing the action would leave the file unreadable")?;
    Ok(text)
}

/// Keys being pressed for a row.
struct Recording {
    row: usize,
    keystrokes: Vec<Keystroke>,
    _intercept: Subscription,
}

/// The keys a recording takes at most: a chord of two.
const MAX_KEYSTROKES: usize = 2;

fn is_modifier(key: &str) -> bool {
    matches!(
        key,
        "shift"
            | "control"
            | "ctrl"
            | "alt"
            | "option"
            | "platform"
            | "cmd"
            | "super"
            | "win"
            | "function"
            | "fn"
    )
}

pub struct KeyBindingsView {
    focus_handle: FocusHandle,
    search: Entity<Editor>,
    query: Query,
    filter: Filter,
    rows: Vec<BindingRow>,
    /// The rows that show, by index.
    matches: Vec<usize>,
    /// The actions the user's key bindings file binds or unbinds.
    user_actions: HashSet<String>,
    recording: Option<Recording>,
    error: Option<SharedString>,
    scroll: UniformListScrollHandle,
    _read: Task<()>,
    _write: Task<()>,
    _subscriptions: Vec<Subscription>,
}

impl KeyBindingsView {
    pub fn new(window: &mut Window, cx: &mut Context<Self>) -> Self {
        let search = cx.new(|cx| {
            let mut editor = Editor::single_line(window, cx);
            editor.set_placeholder_text("Search by action, keys, or where they work", window, cx);
            editor
        });
        let subscriptions = vec![
            cx.subscribe(&search, |this, search, event: &EditorEvent, cx| {
                if matches!(event, EditorEvent::BufferEdited) {
                    this.query = Query::new(&search.read(cx).text(cx));
                    this.update_matches();
                    cx.notify();
                }
            }),
            cx.observe_global::<KeymapVersion>(|this, cx| this.reload(cx)),
        ];
        let mut this = Self {
            focus_handle: cx.focus_handle(),
            search,
            query: Query::default(),
            filter: Filter::Bound,
            rows: Vec::new(),
            matches: Vec::new(),
            user_actions: HashSet::default(),
            recording: None,
            error: None,
            scroll: UniformListScrollHandle::new(),
            _read: Task::ready(()),
            _write: Task::ready(()),
            _subscriptions: subscriptions,
        };
        this.reload(cx);
        this
    }

    /// Reads the key bindings again: the app's, and the actions in the user's file.
    pub fn reload(&mut self, cx: &mut Context<Self>) {
        self.rows = rows_for_keymap(cx);
        self.update_matches();
        cx.notify();
        let fs = <dyn Fs>::global(cx);
        let aliases = cx.deprecated_actions_to_preferred_actions().clone();
        self._read = cx.spawn(async move |this, cx| {
            let text = fs.load(paths::keymap_file()).await.unwrap_or_default();
            let actions = user_actions(&text, &aliases);
            this.update(cx, |this, cx| {
                this.user_actions = actions;
                this.update_matches();
                cx.notify();
            })
            .ok();
        });
    }

    pub fn set_query(&mut self, query: &str, window: &mut Window, cx: &mut Context<Self>) {
        self.search
            .update(cx, |search, cx| search.set_text(query, window, cx));
        self.query = Query::new(query);
        // The action asked for may have no keys yet.
        if self.filter == Filter::Bound
            && !self
                .rows
                .iter()
                .any(|row| !row.keystrokes.is_empty() && row.matches(&self.query))
        {
            self.filter = Filter::All;
        }
        self.update_matches();
        cx.notify();
    }

    /// The shortcuts `query` finds, for the page's search.
    pub fn count_matches(&self, query: &Query, _: &App) -> usize {
        self.rows
            .iter()
            .filter(|row| !row.keystrokes.is_empty() && row.matches(query))
            .count()
    }

    #[cfg(test)]
    pub(crate) fn shown(&self) -> Vec<&BindingRow> {
        self.matches
            .iter()
            .map(|index| &self.rows[*index])
            .collect()
    }

    #[cfg(test)]
    pub(crate) fn start_recording_for_test(
        &mut self,
        shown: usize,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.start_recording(self.matches[shown], window, cx)
    }

    #[cfg(test)]
    pub(crate) fn recorded_for_test(&self, cx: &App) -> Vec<String> {
        self.recorded(cx)
            .iter()
            .map(KeybindingKeystroke::unparse)
            .collect()
    }

    #[cfg(test)]
    pub(crate) fn is_recording(&self) -> bool {
        self.recording.is_some()
    }

    fn changed(&self, row: &BindingRow) -> bool {
        row.source == Some(KeybindSource::User) || self.user_actions.contains(row.action_name)
    }

    fn update_matches(&mut self) {
        self.matches = (0..self.rows.len())
            .filter(|index| {
                let row = &self.rows[*index];
                let shown = match self.filter {
                    Filter::Bound => !row.keystrokes.is_empty(),
                    Filter::All => true,
                    Filter::Changed => self.changed(row),
                };
                shown && row.matches(&self.query)
            })
            .collect();
        if let Some(recording) = &self.recording
            && !self.matches.contains(&recording.row)
        {
            self.recording = None;
        }
    }

    fn set_filter(&mut self, filter: Filter, cx: &mut Context<Self>) {
        self.filter = filter;
        self.update_matches();
        cx.notify();
    }

    fn start_recording(&mut self, row: usize, window: &mut Window, cx: &mut Context<Self>) {
        let this = cx.entity().downgrade();
        let intercept = cx.intercept_keystrokes(move |event: &KeystrokeEvent, window, cx| {
            this.update(cx, |this, cx| this.keystroke(&event.keystroke, window, cx))
                .ok();
        });
        window.focus(&self.focus_handle, cx);
        self.error = None;
        self.recording = Some(Recording {
            row,
            keystrokes: Vec::new(),
            _intercept: intercept,
        });
        cx.notify();
    }

    pub fn cancel_recording(&mut self, _: &mut Window, cx: &mut Context<Self>) {
        if self.recording.take().is_some() {
            cx.notify();
        }
    }

    fn keystroke(&mut self, keystroke: &Keystroke, window: &mut Window, cx: &mut Context<Self>) {
        let Some(recording) = self.recording.as_mut() else {
            return;
        };
        // The keys are the recording's, not the app's bindings'.
        cx.stop_propagation();
        if is_modifier(&keystroke.key) {
            return;
        }
        if keystroke.key == "escape" && !keystroke.modifiers.modified() {
            self.cancel_recording(window, cx);
            return;
        }
        if recording.keystrokes.len() >= MAX_KEYSTROKES {
            recording.keystrokes.clear();
        }
        recording.keystrokes.push(keystroke.clone());
        cx.notify();
    }

    fn recorded(&self, cx: &App) -> Vec<KeybindingKeystroke> {
        let mapper = cx.keyboard_mapper();
        self.recording
            .iter()
            .flat_map(|recording| &recording.keystrokes)
            .map(|keystroke| {
                KeybindingKeystroke::new_with_mapper(keystroke.clone(), false, mapper.as_ref())
            })
            .collect()
    }

    fn save_recording(&mut self, cx: &mut Context<Self>) {
        let keystrokes = self.recorded(cx);
        let Some(recording) = self.recording.take() else {
            return;
        };
        if keystrokes.is_empty() {
            cx.notify();
            return;
        }
        let row = &self.rows[recording.row];
        let change = if row.keystrokes.is_empty() {
            KeymapChange::Add {
                action: row.action_name.to_string(),
                keystrokes,
            }
        } else {
            KeymapChange::Replace {
                binding: row.spec(),
                keystrokes,
            }
        };
        self.change(change, cx);
    }

    /// Writes `change` to the user's key bindings file.
    pub(crate) fn change(&mut self, change: KeymapChange, cx: &mut Context<Self>) {
        let fs = <dyn Fs>::global(cx);
        let mapper = cx.keyboard_mapper().clone();
        let aliases = cx.deprecated_actions_to_preferred_actions().clone();
        self.error = None;
        cx.notify();
        self._write = cx.spawn(async move |this, cx| {
            let result = async {
                let path = paths::keymap_file();
                let text = match fs.load(path).await {
                    Ok(text) if !text.trim().is_empty() => text,
                    Ok(_) => INITIAL_KEYMAP.to_string(),
                    Err(_) if !fs.is_file(path).await => INITIAL_KEYMAP.to_string(),
                    Err(error) => return Err(error),
                };
                let new_text = change.apply(text, mapper.as_ref(), &aliases)?;
                fs.atomic_write(path.clone(), new_text).await?;
                anyhow::Ok(())
            }
            .await;
            this.update(cx, |this, cx| {
                if let Err(error) = result {
                    log::error!("couldn't save a key binding: {error:#}");
                    this.error = Some(format!("Couldn't save the shortcut: {error:#}").into());
                }
                cx.notify();
            })
            .ok();
        });
    }

    /// Other bindings with the keys being recorded, in the same context.
    fn conflicts(&self, cx: &App) -> Vec<&BindingRow> {
        let Some(recording) = &self.recording else {
            return Vec::new();
        };
        let keys = unparse(&self.recorded(cx));
        if keys.is_empty() {
            return Vec::new();
        }
        let context = &self.rows[recording.row].context;
        self.rows
            .iter()
            .enumerate()
            .filter(|(index, row)| {
                *index != recording.row && row.keys == keys && row.context == *context
            })
            .map(|(_, row)| row)
            .collect()
    }

    fn render_row(&self, index: usize, cx: &mut Context<Self>) -> AnyElement {
        let palette = palette(cx);
        let row_index = self.matches[index];
        let row = &self.rows[row_index];
        let recording = self
            .recording
            .as_ref()
            .filter(|recording| recording.row == row_index);
        let source = match row.source {
            _ if row.keystrokes.is_empty() && self.user_actions.contains(row.action_name) => {
                Some("Yours: removed")
            }
            _ if row.keystrokes.is_empty() => None,
            Some(KeybindSource::User) => Some("Yours"),
            Some(KeybindSource::Vim) => Some("Vim"),
            Some(KeybindSource::Base) => Some("Base keymap"),
            _ => None,
        };
        let context = row.context.as_ref().map_or_else(
            || "Everywhere".to_string(),
            |context| format!("In {context}"),
        );
        let detail = match (recording, source) {
            (Some(_), _) => match self.conflicts(cx).first() {
                Some(other) => format!("Also {} there", other.label),
                None => "Press the new keys. Esc cancels.".to_string(),
            },
            (None, Some(source)) => format!("{context} · {source}"),
            (None, None) => context,
        };
        let keys: AnyElement = match recording {
            Some(recording) if recording.keystrokes.is_empty() => {
                rows::muted("Press keys…").into_any_element()
            }
            Some(_) => h_flex()
                .gap_1()
                .children(key_labels(&self.recorded(cx)).into_iter().map(Kbd::new))
                .into_any_element(),
            None if row.keystrokes.is_empty() => rows::muted("—").into_any_element(),
            None => h_flex()
                .gap_1()
                .children(key_labels(&row.keystrokes).into_iter().map(Kbd::new))
                .into_any_element(),
        };
        let changed = self.changed(row);
        let buttons = if recording.is_some() {
            h_flex()
                .gap_1()
                .child(
                    div().debug_selector(|| "key-binding-save".into()).child(
                        TextButton::new("key-binding-save", "Save")
                            .style(ButtonStyle::Primary)
                            .disabled(self.recorded(cx).is_empty())
                            .on_click(cx.listener(|this, _, _, cx| this.save_recording(cx))),
                    ),
                )
                .child(div().debug_selector(|| "key-binding-cancel".into()).child(
                    TextButton::new("key-binding-cancel", "Cancel").on_click(
                        cx.listener(|this, _, window, cx| this.cancel_recording(window, cx)),
                    ),
                ))
        } else {
            let spec = row.spec();
            let action = row.action_name.to_string();
            h_flex()
                .gap_0p5()
                .child(
                    div()
                        .debug_selector(move || format!("key-binding-change-{index}"))
                        .child(
                            IconButton::new(("key-binding-change", row_index), IconName::Pencil)
                                .icon_size(IconSize::Small)
                                .tooltip(Tooltip::text(if row.keystrokes.is_empty() {
                                    "Give it a shortcut"
                                } else {
                                    "Change the shortcut"
                                }))
                                .on_click(cx.listener(move |this, _, window, cx| {
                                    this.start_recording(row_index, window, cx)
                                })),
                        ),
                )
                .when(!row.keystrokes.is_empty(), |this| {
                    this.child(
                        IconButton::new(("key-binding-remove", row_index), IconName::Trash)
                            .icon_size(IconSize::Small)
                            .tooltip(Tooltip::text("Remove the shortcut"))
                            .on_click(cx.listener(move |this, _, _, cx| {
                                this.change(
                                    KeymapChange::Remove {
                                        binding: spec.clone(),
                                    },
                                    cx,
                                )
                            })),
                    )
                })
                .when(changed, |this| {
                    this.child(
                        IconButton::new(("key-binding-reset", row_index), IconName::RotateCcw)
                            .icon_size(IconSize::Small)
                            .tooltip(Tooltip::text("Back to mustr's shortcut"))
                            .on_click(cx.listener(move |this, _, _, cx| {
                                this.change(
                                    KeymapChange::Reset {
                                        action: action.clone(),
                                    },
                                    cx,
                                )
                            })),
                    )
                })
        };
        let documentation = row.documentation;
        h_flex()
            .id(("key-binding", row_index))
            .debug_selector(move || format!("key-binding-{index}"))
            .w_full()
            .gap_3()
            .px_2()
            .py(scaled(px(5.)))
            .border_b(tokens::BORDER_WIDTH)
            .border_color(palette.border.hsla())
            .when(recording.is_some(), |this| {
                this.bg(palette.accent_soft.hsla())
            })
            .child(
                v_flex()
                    .id(("key-binding-name", row_index))
                    .flex_1()
                    .min_w_0()
                    .when_some(documentation, |this, documentation| {
                        this.tooltip(Tooltip::text(documentation))
                    })
                    .child(
                        Label::new(row.label.clone())
                            .weight(FontWeight::MEDIUM)
                            .truncate(),
                    )
                    .child(
                        Label::new(detail)
                            .size(LabelSize::XSmall)
                            .color(if recording.is_some() {
                                Color::Default
                            } else {
                                Color::Muted
                            })
                            .truncate(),
                    ),
            )
            .child(div().flex_none().child(keys))
            .child(div().flex_none().child(buttons))
            .into_any_element()
    }
}

/// Every binding in the keymap, and the command palette's actions without one, by name.
fn rows_for_keymap(cx: &App) -> Vec<BindingRow> {
    let filter = CommandPaletteFilter::try_global(cx);
    let hidden = |action: &dyn Action| {
        filter.is_some_and(|filter| filter.is_hidden(action))
            || crate::hidden_actions::is_hidden(action.name())
    };
    let documentation = cx.action_documentation();
    let mut rows = Vec::new();
    let mut bound = HashSet::default();
    {
        let keymap = cx.key_bindings();
        let keymap = keymap.borrow();
        for binding in keymap.bindings() {
            let action = binding.action();
            if gpui::is_no_action(action) || gpui::is_unbind(action) || hidden(action) {
                continue;
            }
            // Left out when a later `null` or `unbind` turns it off, such as the user's unbind
            // of the keys they changed.
            let enabled = keymap
                .bindings_for_action(action)
                .any(|enabled| std::ptr::eq(enabled, binding));
            if !enabled {
                continue;
            }
            bound.insert(action.name());
            let keystrokes = binding.keystrokes().to_vec();
            rows.push(BindingRow {
                action_name: action.name(),
                label: command_palette::humanize_action_name(action.name()).into(),
                keys: unparse(&keystrokes),
                keys_label: key_labels(&keystrokes).join(" "),
                keystrokes,
                context: binding
                    .predicate()
                    .map(|predicate| predicate.to_string().into()),
                source: binding.meta().map(KeybindSource::from_meta),
                arguments: binding.action_input(),
                documentation: documentation.get(action.name()).copied(),
            });
        }
    }
    for name in cx.all_action_names() {
        if bound.contains(name) {
            continue;
        }
        let Ok(action) = cx.build_action(name, None) else {
            continue;
        };
        if action.name() != *name || hidden(action.as_ref()) {
            continue;
        }
        rows.push(BindingRow {
            action_name: name,
            label: command_palette::humanize_action_name(name).into(),
            keystrokes: Vec::new(),
            keys: String::new(),
            keys_label: String::new(),
            context: None,
            source: None,
            arguments: None,
            documentation: documentation.get(name).copied(),
        });
    }
    rows.sort_by(|a, b| {
        a.label
            .cmp(&b.label)
            .then_with(|| a.context.cmp(&b.context))
            .then_with(|| a.keys.cmp(&b.keys))
    });
    rows
}

impl Render for KeyBindingsView {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let palette = palette(cx);
        let count = self.matches.len();
        let this = cx.entity().downgrade();
        let filter = rows::segmented(
            "key-bindings-filter",
            &["With shortcuts", "All actions", "Changed"],
            match self.filter {
                Filter::Bound => 0,
                Filter::All => 1,
                Filter::Changed => 2,
            },
            None,
            move |choice, _, cx| {
                let filter = match choice {
                    1 => Filter::All,
                    2 => Filter::Changed,
                    _ => Filter::Bound,
                };
                this.update(cx, |this, cx| this.set_filter(filter, cx)).ok();
            },
        );
        let header = h_flex()
            .w_full()
            .flex_wrap()
            .gap_2()
            .child(
                h_flex()
                    .debug_selector(|| "key-bindings-search".into())
                    .flex_1()
                    .min_w(scaled(px(220.)))
                    .gap_1p5()
                    .px_2()
                    .py(scaled(px(5.)))
                    .rounded(scaled(tokens::CONTROL_RADIUS))
                    .border(tokens::BORDER_WIDTH)
                    .border_color(palette.border.hsla())
                    .bg(palette.background.hsla())
                    .child(
                        ui::Icon::new(IconName::MagnifyingGlass)
                            .size(IconSize::Small)
                            .color(Color::Muted),
                    )
                    .child(div().flex_1().min_w_0().child(self.search.clone())),
            )
            .child(filter)
            .child(rows::muted(if count == 1 {
                "1 shortcut".to_string()
            } else {
                format!("{count} shortcuts")
            }));
        let list = if count == 0 {
            div()
                .debug_selector(|| "key-bindings-empty".into())
                .py_4()
                .child(rows::muted(match self.filter {
                    Filter::Changed => "You haven't changed any shortcut.",
                    _ => "No shortcut matches.",
                }))
                .into_any_element()
        } else {
            uniform_list(
                "key-bindings-list",
                count,
                cx.processor(|this, range: Range<usize>, _window, cx| {
                    range.map(|index| this.render_row(index, cx)).collect()
                }),
            )
            .track_scroll(&self.scroll)
            .flex_1()
            .w_full()
            .into_any_element()
        };
        v_flex()
            .key_context("MustrKeyBindings")
            .track_focus(&self.focus_handle)
            .size_full()
            .gap_2()
            .child(header)
            .when_some(self.error.clone(), |this, error| {
                this.child(
                    div()
                        .debug_selector(|| "key-bindings-error".into())
                        .child(Label::new(error).size(LabelSize::Small).color(Color::Error)),
                )
            })
            .child(
                v_flex()
                    .flex_1()
                    .min_h_0()
                    .w_full()
                    .rounded(scaled(tokens::CORNER_RADIUS))
                    .border(tokens::BORDER_WIDTH)
                    .border_color(palette.border.hsla())
                    .bg(palette.surface.hsla())
                    .overflow_hidden()
                    .child(list),
            )
    }
}

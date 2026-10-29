//! Editor: how files you open and the code in reviews look and behave, and which keys they use.

use super::{
    merged,
    rows::{self, Callback, Group, SettingRow},
    update_settings,
};
use gpui::{App, SharedString, Window};
use settings::{
    BaseKeymapContent, FormatOnSave, LanguageSettingsContent, ShowWhitespaceSetting, SoftWrap,
};
use std::{num::NonZeroU32, rc::Rc};
use strum::VariantNames as _;

/// The tab sizes offered, as Zed allows them.
const MAX_TAB_SIZE: u32 = 16;

pub(super) fn groups(window: &mut Window, cx: &mut App) -> Vec<Group> {
    vec![editing(window, cx), display(window, cx), keys(window, cx)]
}

/// The language settings every language starts from, as they apply.
fn language(cx: &App) -> &LanguageSettingsContent {
    &merged(cx).project.all_languages.defaults
}

fn choices<T: Clone + Send + 'static>(
    options: &[(T, &'static str)],
    set: fn(&mut settings::SettingsContent, T),
) -> Vec<(SharedString, Callback)> {
    options
        .iter()
        .map(|(value, label)| {
            let value = value.clone();
            (
                SharedString::from(*label),
                Rc::new(move |_: &mut Window, cx: &mut App| {
                    let value = value.clone();
                    update_settings(cx, move |settings, _| set(settings, value))
                }) as Callback,
            )
        })
        .collect()
}

fn editing(window: &mut Window, cx: &mut App) -> Group {
    let language = language(cx).clone();
    let tab_size = language.tab_size.map_or(4, NonZeroU32::get);
    let step = |delta: i32| -> Option<Callback> {
        let size = tab_size.checked_add_signed(delta)?;
        (1..=MAX_TAB_SIZE).contains(&size).then(|| {
            Rc::new(move |_: &mut Window, cx: &mut App| {
                update_settings(cx, move |settings, _| {
                    settings.project.all_languages.defaults.tab_size = NonZeroU32::new(size)
                })
            }) as Callback
        })
    };
    let tab = rows::stepper(
        "settings-tab-size",
        tab_size.to_string(),
        step(-1),
        step(1),
        cx,
    );

    const WRAPS: [(SoftWrap, &str); 3] = [
        (SoftWrap::None, "Off"),
        (SoftWrap::EditorWidth, "At the editor's edge"),
        (SoftWrap::Bounded, "At the preferred line length"),
    ];
    let wrap = match language.soft_wrap.unwrap_or(SoftWrap::None) {
        SoftWrap::PreferLine => SoftWrap::None,
        wrap => wrap,
    };
    let wrap_selected = WRAPS.iter().position(|(choice, _)| *choice == wrap);
    let wrap = rows::dropdown(
        "settings-soft-wrap",
        wrap_selected.map_or("Off", |index| WRAPS[index].1),
        choices(&WRAPS, |settings, wrap| {
            settings.project.all_languages.defaults.soft_wrap = Some(wrap)
        }),
        wrap_selected,
        None,
        window,
        cx,
    );

    let format_on_save = !matches!(language.format_on_save, Some(FormatOnSave::Off));
    let format = rows::switch(
        "settings-format-on-save",
        format_on_save,
        None,
        |on, _, cx| {
            update_settings(cx, move |settings, _| {
                settings.project.all_languages.defaults.format_on_save = Some(if on {
                    FormatOnSave::On
                } else {
                    FormatOnSave::Off
                })
            })
        },
    );
    Group::new(
        "Editing",
        vec![
            SettingRow::new(
                "settings-row-tab-size",
                "Tab size",
                "Spaces per indent, for files whose language or project doesn't set its own.",
                tab,
            )
            .keywords("indent spaces tabs width"),
            SettingRow::new(
                "settings-row-soft-wrap",
                "Wrap long lines",
                "Long lines go on over the next line rather than off the side.",
                wrap,
            )
            .keywords("soft wrap line length"),
            SettingRow::new(
                "settings-row-format-on-save",
                "Format on save",
                "Formats a file with its language's formatter when you save it.",
                format,
            )
            .keywords("prettier formatter save"),
        ],
    )
}

fn display(window: &mut Window, cx: &mut App) -> Group {
    let settings = merged(cx);
    let line_numbers = settings
        .editor
        .gutter
        .as_ref()
        .and_then(|gutter| gutter.line_numbers)
        .unwrap_or(true);
    let blame = settings
        .git
        .as_ref()
        .and_then(|git| git.inline_blame.as_ref())
        .and_then(|blame| blame.enabled)
        .unwrap_or(false);
    const WHITESPACE: [(ShowWhitespaceSetting, &str); 5] = [
        (ShowWhitespaceSetting::Selection, "In selections"),
        (ShowWhitespaceSetting::Boundary, "Between words"),
        (ShowWhitespaceSetting::Trailing, "At line ends"),
        (ShowWhitespaceSetting::All, "Everywhere"),
        (ShowWhitespaceSetting::None, "Never"),
    ];
    let whitespace = language(cx)
        .show_whitespaces
        .unwrap_or(ShowWhitespaceSetting::Selection);
    let whitespace_selected = WHITESPACE
        .iter()
        .position(|(choice, _)| *choice == whitespace);
    Group::new(
        "Display",
        vec![
            SettingRow::new(
                "settings-row-line-numbers",
                "Line numbers",
                "Numbers beside each line, in editors and reviews.",
                rows::switch("settings-line-numbers", line_numbers, None, |on, _, cx| {
                    update_settings(cx, move |settings, _| {
                        settings.editor.gutter.get_or_insert_default().line_numbers = Some(on)
                    })
                }),
            )
            .keywords("gutter"),
            SettingRow::new(
                "settings-row-whitespace",
                "Show spaces and tabs",
                "Where to draw a dot for each space and an arrow for each tab.",
                rows::dropdown(
                    "settings-whitespace",
                    whitespace_selected.map_or("In selections", |index| WHITESPACE[index].1),
                    choices(&WHITESPACE, |settings, whitespace| {
                        settings.project.all_languages.defaults.show_whitespaces = Some(whitespace)
                    }),
                    whitespace_selected,
                    None,
                    window,
                    cx,
                ),
            )
            .keywords("whitespace invisibles"),
            SettingRow::new(
                "settings-row-inline-blame",
                "Inline blame",
                "Who last changed the line the cursor is on, and when, at the line's end.",
                rows::switch("settings-inline-blame", blame, None, |on, _, cx| {
                    update_settings(cx, move |settings, _| {
                        settings
                            .git
                            .get_or_insert_default()
                            .inline_blame
                            .get_or_insert_default()
                            .enabled = Some(on)
                    })
                }),
            )
            .keywords("git blame author commit"),
        ],
    )
}

fn keys(window: &mut Window, cx: &mut App) -> Group {
    let settings = merged(cx);
    let vim = settings.vim_mode.unwrap_or(false);
    let base = settings.base_keymap.unwrap_or_default();
    let keymaps = BaseKeymapContent::VARIANTS;
    let base_choices = <BaseKeymapContent as strum::VariantArray>::VARIANTS
        .iter()
        .zip(keymaps)
        .map(|(keymap, name)| {
            let keymap = *keymap;
            let label = match keymap {
                BaseKeymapContent::None => "None (mustr's keys only)",
                BaseKeymapContent::Zed => "Zed",
                _ => name,
            };
            (
                SharedString::from(label),
                Rc::new(move |_: &mut Window, cx: &mut App| {
                    update_settings(cx, move |settings, _| settings.base_keymap = Some(keymap))
                }) as Callback,
            )
        })
        .collect::<Vec<_>>();
    let base_selected = <BaseKeymapContent as strum::VariantArray>::VARIANTS
        .iter()
        .position(|keymap| *keymap == base);
    let base_label = base_selected
        .map(|index| base_choices[index].0.clone())
        .unwrap_or_else(|| "Zed".into());
    Group::new(
        "Keys",
        vec![
            SettingRow::new(
                "settings-row-vim",
                "Vim mode",
                "Vim's modes and keys in editors and reviews.",
                rows::switch("settings-vim", vim, None, |on, _, cx| {
                    update_settings(cx, move |settings, _| settings.vim_mode = Some(on))
                }),
            )
            .keywords("vi modal keys"),
            SettingRow::new(
                "settings-row-base-keymap",
                "Key bindings from",
                "The editor whose shortcuts mustr starts from. mustr's own shortcuts, and \
                 yours, come on top.",
                rows::dropdown(
                    "settings-base-keymap",
                    base_label,
                    base_choices,
                    base_selected,
                    None,
                    window,
                    cx,
                ),
            )
            .keywords("base keymap vscode jetbrains sublime emacs shortcuts"),
            SettingRow::new(
                "settings-row-shortcuts",
                "Keyboard shortcuts",
                "See every shortcut, and change them.",
                rows::button(
                    "settings-open-shortcuts",
                    "Change shortcuts…",
                    None,
                    |window, cx| {
                        window.dispatch_action(
                            gpui::Action::boxed_clone(&crate::window::OpenKeyBindings),
                            cx,
                        )
                    },
                ),
            )
            .keywords("key bindings keymap"),
        ],
    )
}

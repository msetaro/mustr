//! General: the theme, the size (zoom and code), the fonts, and what closing the last window does.

use super::{
    defaults, merged, platform_keys,
    rows::{self, Callback, Group, SettingRow},
    update_settings,
};
use crate::zoom::{ResetZoom, ZoomIn, ZoomOut};
use gpui::{Action as _, App, Global, Pixels, SharedString, Window, px};
use mustr_ui::{COLORBLIND_SAFE_THEME_NAME, DARK_THEME_NAME, LIGHT_THEME_NAME};
use settings::{
    FontFamilyName, OnLastWindowClosed, Settings as _, SettingsContent, ThemeAppearanceMode,
    ThemeName, ThemeSelection,
};
use std::{rc::Rc, sync::Arc};
use theme::ActiveTheme as _;
use theme_settings::{ThemeSettings, clamp_font_size};
use ui::prelude::*;

pub(super) fn groups(window: &mut Window, cx: &mut App) -> Vec<Group> {
    vec![appearance(cx), size(cx), fonts(window, cx), windows(cx)]
}

/// The themes in use, as the theme settings name them.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct Themes {
    pub mode: ThemeAppearanceMode,
    pub light: Arc<str>,
    pub dark: Arc<str>,
}

impl Themes {
    pub fn of(selection: &ThemeSelection, active_is_light: bool) -> Self {
        match selection {
            ThemeSelection::Static(name) => Self {
                mode: if active_is_light {
                    ThemeAppearanceMode::Light
                } else {
                    ThemeAppearanceMode::Dark
                },
                light: name.0.clone(),
                dark: name.0.clone(),
            },
            ThemeSelection::Dynamic { mode, light, dark } => Self {
                mode: *mode,
                light: light.0.clone(),
                dark: dark.0.clone(),
            },
        }
    }

    /// Which of the page's choices this is: Dark, Light, Colorblind-safe, or Match system.
    pub fn choice(&self) -> usize {
        match self.mode {
            ThemeAppearanceMode::Light => 1,
            ThemeAppearanceMode::Dark if &*self.dark == COLORBLIND_SAFE_THEME_NAME => 2,
            ThemeAppearanceMode::Dark => 0,
            ThemeAppearanceMode::System => 3,
        }
    }

    /// The themes for a choice. Light and dark keep the other appearance's theme; a theme of one
    /// appearance in both (a static selection) gives way to mustr's.
    pub fn chose(&self, choice: usize) -> ThemeSelection {
        let light = if self.light == self.dark {
            LIGHT_THEME_NAME.into()
        } else {
            self.light.clone()
        };
        let dark = if self.light == self.dark {
            DARK_THEME_NAME.into()
        } else {
            self.dark.clone()
        };
        let (mode, light, dark) = match choice {
            0 => (ThemeAppearanceMode::Dark, light, DARK_THEME_NAME.into()),
            1 => (ThemeAppearanceMode::Light, LIGHT_THEME_NAME.into(), dark),
            2 => (
                ThemeAppearanceMode::Dark,
                light,
                COLORBLIND_SAFE_THEME_NAME.into(),
            ),
            _ => (ThemeAppearanceMode::System, light, dark),
        };
        ThemeSelection::Dynamic {
            mode,
            light: ThemeName(light),
            dark: ThemeName(dark),
        }
    }

    /// The themes in use that aren't mustr's, from Zed's theme picker.
    pub fn others(&self) -> Vec<Arc<str>> {
        let mustr = [
            DARK_THEME_NAME,
            LIGHT_THEME_NAME,
            COLORBLIND_SAFE_THEME_NAME,
        ];
        let in_use = match self.mode {
            ThemeAppearanceMode::Light => vec![self.light.clone()],
            ThemeAppearanceMode::Dark => vec![self.dark.clone()],
            ThemeAppearanceMode::System => vec![self.light.clone(), self.dark.clone()],
        };
        let mut others = in_use
            .into_iter()
            .filter(|name| !mustr.contains(&&**name))
            .collect::<Vec<_>>();
        others.dedup();
        others
    }
}

fn appearance(cx: &mut App) -> Group {
    let themes = Themes::of(
        &merged(cx).theme.theme.clone().unwrap_or_default(),
        cx.theme().appearance().is_light(),
    );
    let choice = themes.choice();
    let others = themes.others();
    let theme = rows::segmented(
        "settings-theme",
        &["Dark", "Light", "Colorblind-safe", "Match system"],
        choice,
        None,
        {
            let themes = themes.clone();
            move |choice, _, cx| {
                let selection = themes.chose(choice);
                update_settings(cx, move |settings, _| {
                    settings.theme.theme = Some(selection);
                })
            }
        },
    );
    let mut description = match choice {
        2 => "mustr's dark theme, with blue and orange for added and deleted lines instead of \
              green and red."
            .to_string(),
        3 => format!(
            "Light or dark, as {} is set: {} and {}.",
            platform_keys("macOS", "the system"),
            themes.light,
            themes.dark
        ),
        _ => "mustr's dark or light theme, or the dark one with colors for colorblind eyes."
            .to_string(),
    };
    if !others.is_empty() && choice != 3 {
        description = format!(
            "{} from Zed's themes. Pick one of mustr's to go back.",
            others.join(" and ")
        );
    }
    Group::new(
        "Appearance",
        vec![
            SettingRow::new("settings-row-theme", "Theme", description, theme)
                .keywords("appearance dark light colorblind color blind system mode"),
            SettingRow::new(
                "settings-row-other-themes",
                "Other themes",
                "Zed's themes, if you'd rather use one of them.",
                rows::button("settings-browse-themes", "Browse…", None, |window, cx| {
                    window.dispatch_action(
                        zed_actions::theme_selector::Toggle::default().boxed_clone(),
                        cx,
                    )
                }),
            )
            .keywords("theme picker color scheme"),
        ],
    )
}

/// The zoom, as a percentage of mustr's design size.
pub(crate) fn zoom_percent(ui_font_size: Pixels) -> u32 {
    (f32::from(ui_font_size) / f32::from(mustr_ui::tokens::UI_FONT_SIZE) * 100.).round() as u32
}

fn size(cx: &mut App) -> Group {
    let theme = ThemeSettings::get_global(cx);
    let ui = theme.ui_font_size(cx);
    let code = theme.buffer_font_size(cx);
    let default_ui = defaults(cx)
        .theme
        .ui_font_size
        .map_or(mustr_ui::tokens::UI_FONT_SIZE, |size| px(size.0));
    let default_code = defaults(cx)
        .theme
        .buffer_font_size
        .map_or(default_ui, |size| px(size.0));
    let dispatch = |action: Box<dyn gpui::Action>| -> Callback {
        Rc::new(move |window: &mut Window, cx: &mut App| {
            window.dispatch_action(action.boxed_clone(), cx)
        })
    };
    let zoom = h_flex()
        .gap_2()
        .child(rows::stepper(
            "settings-zoom",
            format!("{}%", zoom_percent(ui)),
            (ui > clamp_font_size(px(0.))).then(|| dispatch(ZoomOut.boxed_clone())),
            (ui < clamp_font_size(px(1000.))).then(|| dispatch(ZoomIn.boxed_clone())),
            cx,
        ))
        .child(rows::button(
            "settings-zoom-reset",
            "Reset",
            (ui == default_ui && code == default_code).then(|| "This is mustr's size".into()),
            |window, cx| window.dispatch_action(ResetZoom.boxed_clone(), cx),
        ));
    let step_code = |delta: f32| -> Option<Callback> {
        let size = clamp_font_size(code + px(delta));
        (size != code).then(|| {
            Rc::new(move |_: &mut Window, cx: &mut App| {
                update_settings(cx, move |settings, _| {
                    settings.theme.buffer_font_size = Some(f32::from(size).into());
                })
            }) as Callback
        })
    };
    let code_size = rows::stepper(
        "settings-code-size",
        format!("{}", f32::from(code)),
        step_code(-1.),
        step_code(1.),
        cx,
    );
    Group::new(
        "Size",
        vec![
            SettingRow::new(
                "settings-row-zoom",
                "Zoom",
                "Text, code and layout together, as a browser zooms a page.",
                zoom,
            )
            .keywords("size scale bigger smaller font size ui zoom in out reset actual"),
            SettingRow::new(
                "settings-row-code-size",
                "Code size",
                "Code in editors, reviews and diffs. Zooming scales it too.",
                code_size,
            )
            .keywords("font size buffer editor text"),
        ],
    )
}

/// The font families on this computer, read once.
struct FontNames(Vec<SharedString>);

impl Global for FontNames {}

fn font_names(cx: &mut App) -> Vec<SharedString> {
    if let Some(names) = cx.try_global::<FontNames>() {
        return names.0.clone();
    }
    let mut names = cx
        .text_system()
        .all_font_names()
        .into_iter()
        .filter(|name| !name.starts_with('.'))
        .map(SharedString::from)
        .collect::<Vec<_>>();
    names.sort_by_key(|name| name.to_lowercase());
    names.dedup();
    cx.set_global(FontNames(names.clone()));
    names
}

/// A font family's name as people know it: Zed's names for its built-in fonts aren't.
pub(crate) fn font_label(family: &str) -> String {
    match family {
        ".SystemUIFont" => "The system's font".to_string(),
        ".ZedSans" => "Zed Sans (IBM Plex Sans)".to_string(),
        ".ZedMono" => "Zed Mono (Lilex)".to_string(),
        family => family.to_string(),
    }
}

fn font_choices(
    current: &str,
    default: &str,
    names: &[SharedString],
    set: fn(&mut SettingsContent, Option<FontFamilyName>),
) -> (Vec<(SharedString, Callback)>, Option<usize>) {
    let mut choices: Vec<(SharedString, Callback)> = vec![(
        format!("{} (mustr's)", font_label(default)).into(),
        Rc::new(move |_, cx| update_settings(cx, move |settings, _| set(settings, None))),
    )];
    let mut selected = (current == default).then_some(0);
    for name in names {
        if current == name.as_ref() && selected.is_none() {
            selected = Some(choices.len());
        }
        let family = name.clone();
        choices.push((
            name.clone(),
            Rc::new(move |_, cx| {
                let family = FontFamilyName(family.to_string().into());
                update_settings(cx, move |settings, _| set(settings, Some(family)))
            }),
        ));
    }
    (choices, selected)
}

fn fonts(window: &mut Window, cx: &mut App) -> Group {
    let names = font_names(cx);
    let current = merged(cx);
    let ui_font = current
        .theme
        .ui_font_family
        .as_ref()
        .map(|family| family.0.to_string())
        .unwrap_or_default();
    let code_font = current
        .theme
        .buffer_font_family
        .as_ref()
        .map(|family| family.0.to_string())
        .unwrap_or_default();
    let default_ui = defaults(cx)
        .theme
        .ui_font_family
        .as_ref()
        .map(|family| family.0.to_string())
        .unwrap_or_default();
    let default_code = defaults(cx)
        .theme
        .buffer_font_family
        .as_ref()
        .map(|family| family.0.to_string())
        .unwrap_or_default();
    let (ui_choices, ui_selected) =
        font_choices(&ui_font, &default_ui, &names, |settings, font| {
            settings.theme.ui_font_family = font
        });
    let (code_choices, code_selected) =
        font_choices(&code_font, &default_code, &names, |settings, font| {
            settings.theme.buffer_font_family = font
        });
    let ui_control = rows::dropdown(
        "settings-ui-font",
        font_label(&ui_font),
        ui_choices,
        ui_selected,
        None,
        window,
        cx,
    );
    let code_control = rows::dropdown(
        "settings-code-font",
        font_label(&code_font),
        code_choices,
        code_selected,
        None,
        window,
        cx,
    );
    Group::new(
        "Fonts",
        vec![
            SettingRow::new(
                "settings-row-ui-font",
                "Interface font",
                "Menus, lists, threads and everything that isn't code.",
                ui_control,
            )
            .keywords("font family typeface ui"),
            SettingRow::new(
                "settings-row-code-font",
                "Code font",
                "Code in editors, reviews, diffs and terminals.",
                code_control,
            )
            .keywords("font family typeface monospace buffer"),
        ],
    )
}

fn windows(cx: &mut App) -> Group {
    let on_close = merged(cx)
        .workspace
        .on_last_window_closed
        .unwrap_or_default();
    let blocked = (!cfg!(target_os = "macos"))
        .then(|| "Only on macOS: elsewhere, mustr quits with its last window".into());
    let control = rows::segmented(
        "settings-last-window",
        &["Keep running", "Quit mustr"],
        match on_close {
            OnLastWindowClosed::QuitApp => 1,
            OnLastWindowClosed::PlatformDefault => 0,
        },
        blocked,
        |choice, _, cx| {
            let on_close = if choice == 1 {
                OnLastWindowClosed::QuitApp
            } else {
                OnLastWindowClosed::PlatformDefault
            };
            update_settings(cx, move |settings, _| {
                settings.workspace.on_last_window_closed = Some(on_close)
            })
        },
    );
    Group::new(
        "Windows",
        vec![
            SettingRow::new(
                "settings-row-last-window",
                "When the last window closes",
                "Mac apps keep running, and clicking mustr in the Dock opens a window again. \
                 Or mustr can quit.",
                control,
            )
            .keywords("quit close dock on last window closed"),
        ],
    )
}

//! mustr's look: the mustr theme family, the design tokens behind it, and mustr's own
//! components (`mustr-docs/PLAN.md` §6).

pub mod app_icons;
pub mod components;
pub mod icons;
pub mod overflow;
pub mod tokens;

pub use app_icons::{AppIcon, AppIconSource, BrandIcon};
pub use components::*;
pub use icons::{MustrAssets, MustrIcon};
pub use overflow::{OverflowItem, OverflowRow};

use anyhow::{Context as _, Result};
use gpui::App;
use theme::{ActiveTheme as _, ThemeRegistry};
use theme_settings::ThemeFamilyContent;

/// The mustr theme family, in Zed's theme JSON format.
pub const THEME_FAMILY_JSON: &str = include_str!("../../../assets/themes/mustr/mustr.json");

pub const THEME_FAMILY_NAME: &str = "mustr";
pub const DARK_THEME_NAME: &str = "mustr Dark";
pub const LIGHT_THEME_NAME: &str = "mustr Light";
pub const COLORBLIND_SAFE_THEME_NAME: &str = "mustr Colorblind-safe";

/// Registers mustr's themes and re-resolves the active theme, so a settings
/// file that names a mustr theme takes effect.
///
/// Call after `theme_settings::init`. Zed's bundled-theme loader
/// (`LoadThemes::All` with Zed's `assets::Assets`) also finds
/// `assets/themes/mustr/mustr.json`; registering again just replaces the
/// same entries.
pub fn init(cx: &mut App) {
    if let Err(error) = register_themes(cx) {
        log::error!("failed to register mustr's themes: {error:#}");
        return;
    }
    theme_settings::reload_theme(cx);
}

/// Adds the mustr theme family to the global [`ThemeRegistry`].
pub fn register_themes(cx: &mut App) -> Result<()> {
    let registry =
        ThemeRegistry::try_global(cx).context("the theme registry hasn't been initialized")?;
    let family = theme_family_content()?;
    registry.insert_theme_families([theme_settings::refine_theme_family(family)]);
    Ok(())
}

/// The design tokens behind the active theme. Themes other than mustr's get the Dark or Light
/// palette by appearance, for the few colors Zed's theme format has no slot for (such as
/// [`tokens::Palette::pending`]).
pub fn palette(cx: &App) -> &'static tokens::Palette {
    let theme = cx.theme();
    if theme.name == COLORBLIND_SAFE_THEME_NAME {
        &tokens::COLORBLIND_SAFE
    } else if theme.appearance().is_light() {
        &tokens::LIGHT
    } else {
        &tokens::DARK
    }
}

/// Parses the embedded mustr theme family.
pub fn theme_family_content() -> Result<ThemeFamilyContent> {
    theme_settings::deserialize_user_theme(THEME_FAMILY_JSON.as_bytes())
        .context("failed to parse the mustr theme family")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tokens::{ColorToken, Palette};
    use gpui::{Hsla, Rgba, TestAppContext, UpdateGlobal as _};
    use serde_json::Value;
    use settings::{SettingsStore, ThemeName, ThemeSelection};
    use std::collections::BTreeSet;
    use theme::{Appearance, LoadThemes, Theme};

    const THEMES: [(&str, Appearance, &Palette); 3] = [
        (DARK_THEME_NAME, Appearance::Dark, &tokens::DARK),
        (LIGHT_THEME_NAME, Appearance::Light, &tokens::LIGHT),
        (
            COLORBLIND_SAFE_THEME_NAME,
            Appearance::Dark,
            &tokens::COLORBLIND_SAFE,
        ),
    ];

    // Zed's bundled themes, as the reference for valid syntax highlight names.
    const ZED_THEMES: [&str; 3] = [
        include_str!("../../../assets/themes/one/one.json"),
        include_str!("../../../assets/themes/ayu/ayu.json"),
        include_str!("../../../assets/themes/gruvbox/gruvbox.json"),
    ];

    /// Zed's theme types ignore unknown keys (the color maps are flattened)
    /// and turn invalid values into `None`, so a typo would silently fall back
    /// to a default color. Round-tripping through the content type and
    /// comparing with the source catches both.
    #[test]
    fn theme_json_round_trips_through_zeds_theme_types() {
        let source: Value =
            serde_json::from_str(THEME_FAMILY_JSON).expect("the theme JSON should be strict JSON");
        let content: ThemeFamilyContent =
            serde_json::from_value(source.clone()).expect("the theme JSON should deserialize");
        let round_tripped =
            serde_json::to_value(&content).expect("the theme content should serialize");

        let Value::Object(source_fields) = &source else {
            panic!("the theme JSON should be an object");
        };
        for (key, value) in source_fields {
            if key != "$schema" {
                assert_preserved(key, value, &round_tripped[key.as_str()]);
            }
        }

        assert_eq!(content.name, THEME_FAMILY_NAME);
        let themes: Vec<_> = content
            .themes
            .iter()
            .map(|theme| (theme.name.as_str(), theme.appearance))
            .collect();
        assert_eq!(
            themes,
            [
                (DARK_THEME_NAME, theme::AppearanceContent::Dark),
                (LIGHT_THEME_NAME, theme::AppearanceContent::Light),
                (COLORBLIND_SAFE_THEME_NAME, theme::AppearanceContent::Dark),
            ]
        );
    }

    #[test]
    fn theme_json_colors_parse() {
        let source: Value = serde_json::from_str(THEME_FAMILY_JSON).unwrap();
        for theme in source["themes"].as_array().unwrap() {
            assert_colors_parse(theme["name"].as_str().unwrap(), &theme["style"]);
        }
    }

    #[test]
    fn theme_json_uses_zeds_syntax_names() {
        let mut known_names = BTreeSet::new();
        for zed_theme in ZED_THEMES {
            let family: Value = serde_json::from_str(zed_theme).unwrap();
            for theme in family["themes"].as_array().unwrap() {
                known_names.extend(
                    theme["style"]["syntax"]
                        .as_object()
                        .unwrap()
                        .keys()
                        .cloned(),
                );
            }
        }

        let content = theme_family_content().unwrap();
        for theme in &content.themes {
            let unknown: Vec<_> = theme
                .style
                .syntax
                .keys()
                .filter(|name| !known_names.contains(*name))
                .collect();
            assert!(
                unknown.is_empty(),
                "{} styles syntax names that no Zed theme uses: {unknown:?}",
                theme.name
            );
        }
    }

    #[gpui::test]
    fn registers_all_three_themes(cx: &mut TestAppContext) {
        cx.update(|cx| {
            theme::init(LoadThemes::JustBase, cx);
            register_themes(cx).unwrap();

            let registry = ThemeRegistry::global(cx);
            for (name, appearance, palette) in THEMES {
                let theme = registry
                    .get(name)
                    .unwrap_or_else(|error| panic!("{name} should be registered: {error}"));
                assert_eq!(theme.appearance(), appearance, "{name}");
                assert_matches_palette(&theme, palette);
            }
        });
    }

    #[gpui::test]
    fn settings_select_mustr_themes(cx: &mut TestAppContext) {
        cx.update(|cx| {
            let settings_store = SettingsStore::test(cx);
            cx.set_global(settings_store);
            theme_settings::init(LoadThemes::JustBase, cx);
            init(cx);
        });

        for (name, _, palette) in THEMES {
            cx.update(|cx| {
                SettingsStore::update_global(cx, |store, cx| {
                    store.update_user_settings(cx, |settings| {
                        settings.theme.theme = Some(ThemeSelection::Static(ThemeName(name.into())));
                    });
                });
            });
            cx.run_until_parked();
            cx.update(|cx| {
                let theme = cx.theme();
                assert_eq!(theme.name.as_ref(), name);
                assert_matches_palette(theme, palette);
            });
        }
    }

    fn assert_matches_palette(theme: &Theme, palette: &Palette) {
        let name = theme.name.as_ref();
        let colors = theme.colors();
        let status = theme.status();
        let checks = [
            (
                "editor.background",
                colors.editor_background,
                palette.background,
            ),
            ("background", colors.background, palette.surface),
            ("panel.background", colors.panel_background, palette.surface),
            (
                "element.background",
                colors.element_background,
                palette.surface_2,
            ),
            ("border", colors.border, palette.border),
            ("text", colors.text, palette.text),
            ("text.muted", colors.text_muted, palette.text_muted),
            ("text.accent", colors.text_accent, palette.accent),
            ("border.focused", colors.border_focused, palette.accent),
            (
                "players[0].cursor",
                theme.players().local().cursor,
                palette.accent,
            ),
            (
                "editor.diff_hunk.added.background",
                colors.editor_diff_hunk_added_background,
                palette.added_line,
            ),
            (
                "version_control.word_added",
                colors.version_control_word_added,
                palette.added_line_strong,
            ),
            (
                "editor.diff_hunk.deleted.background",
                colors.editor_diff_hunk_deleted_background,
                palette.deleted_line,
            ),
            (
                "version_control.word_deleted",
                colors.version_control_word_deleted,
                palette.deleted_line_strong,
            ),
            (
                "version_control.added",
                colors.version_control_added,
                palette.ok,
            ),
            ("success", status.success, palette.ok),
            ("warning", status.warning, palette.warning),
        ];
        for (key, actual, expected) in checks {
            assert_eq!(to_hex(actual), expected.0, "{name}: {key}");
        }

        let syntax = [
            ("keyword", palette.syntax.keyword),
            ("function", palette.syntax.function),
            ("string", palette.syntax.string),
            ("number", palette.syntax.number),
            ("type", palette.syntax.type_name),
            ("property", palette.syntax.property),
            ("comment", palette.syntax.comment),
        ];
        for (capture, expected) in syntax {
            let color = theme
                .syntax()
                .style_for_name(capture)
                .and_then(|style| style.color)
                .unwrap_or_else(|| panic!("{name}: syntax.{capture} should have a color"));
            assert_eq!(to_hex(color), expected.0, "{name}: syntax.{capture}");
        }
    }

    fn assert_preserved(path: &str, source: &Value, round_tripped: &Value) {
        match (source, round_tripped) {
            (Value::Object(source), Value::Object(round_tripped)) => {
                for (key, value) in source {
                    let path = format!("{path}.{key}");
                    let round_tripped = round_tripped
                        .get(key)
                        .unwrap_or_else(|| panic!("{path} isn't a Zed theme key"));
                    assert_preserved(&path, value, round_tripped);
                }
            }
            (Value::Array(source), Value::Array(round_tripped)) => {
                assert_eq!(source.len(), round_tripped.len(), "{path}");
                for (index, (source, round_tripped)) in source.iter().zip(round_tripped).enumerate()
                {
                    assert_preserved(&format!("{path}[{index}]"), source, round_tripped);
                }
            }
            // Font weights deserialize as `f32`, so `700` comes back as `700.0`.
            (Value::Number(source), Value::Number(round_tripped)) => {
                assert_eq!(source.as_f64(), round_tripped.as_f64(), "{path}");
            }
            (source, round_tripped) => {
                assert_eq!(
                    source, round_tripped,
                    "{path} was dropped or changed by Zed's theme types"
                );
            }
        }
    }

    fn assert_colors_parse(path: &str, value: &Value) {
        match value {
            Value::Object(fields) => {
                for (key, value) in fields {
                    // The only string values in a theme that aren't colors.
                    if key != "background.appearance" && key != "font_style" {
                        assert_colors_parse(&format!("{path}.{key}"), value);
                    }
                }
            }
            Value::Array(items) => {
                for (index, item) in items.iter().enumerate() {
                    assert_colors_parse(&format!("{path}[{index}]"), item);
                }
            }
            Value::String(color) => {
                assert!(
                    color.len() == 9 && theme::try_parse_color(color).is_ok(),
                    "{path}: {color:?} should be a #rrggbbaa color"
                );
            }
            _ => {}
        }
    }

    fn to_hex(color: Hsla) -> u32 {
        let rgba = Rgba::from(color);
        [rgba.r, rgba.g, rgba.b, rgba.a]
            .into_iter()
            .fold(0, |hex, channel| {
                (hex << 8) | (channel.clamp(0., 1.) * 255.).round() as u32
            })
    }

    #[test]
    fn color_tokens_convert_losslessly() {
        let token = ColorToken(0x2ea04324);
        assert_eq!(to_hex(token.hsla()), token.0);
    }
}

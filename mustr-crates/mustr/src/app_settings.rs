use anyhow::Result;
use fs::Fs;
use gpui::{Action as _, App, AppContext as _, DismissEvent, UpdateGlobal as _};
use settings::{InvalidSettingsError, Settings, SettingsFile, SettingsStore};
use std::{io::Write as _, path::Path, sync::Arc};
use theme::ActiveTheme as _;
use theme_settings::ThemeSettings;
use ui::IconName;
use workspace::{
    WorkspaceSettings,
    notifications::{
        NotificationId, dismiss_app_notification, show_app_notification,
        simple_message_notification::MessageNotification,
    },
};

const DEFAULT_OVERRIDES: &str = include_str!("../assets/default_overrides.json");

/// What a new settings file holds. Zed's settings writer garbles the first setting it adds to
/// an empty object after a comment, or after a comment containing `://`, so the comments are
/// inside the braces. mustr has no settings docs of its own and doesn't link to Zed's site.
pub const INITIAL_SETTINGS: &str = "{\n  // mustr settings\n  //\n  // mustr uses Zed's settings format and most of Zed's editor settings.\n}\n";

/// Writes [`INITIAL_SETTINGS`] if there's no settings file yet. When something saves a setting
/// (the diff style, the theme selector), Zed's `SettingsStore::update_settings_file` starts a
/// missing file from Zed's template, which would set Zed's theme and font sizes over mustr's
/// defaults.
pub fn ensure_settings_file(path: &Path) -> Result<()> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    match std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(path)
    {
        Ok(mut file) => Ok(file.write_all(INITIAL_SETTINGS.as_bytes())?),
        Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => Ok(()),
        Err(error) => Err(error.into()),
    }
}

/// Replaces Zed's default settings with Zed's defaults plus mustr's overrides,
/// so mustr can change defaults without editing `assets/settings/default.json`.
pub fn apply_default_overrides(cx: &mut App) -> Result<()> {
    let defaults = merged_default_settings()?;
    SettingsStore::update_global(cx, |store, cx| store.set_default_settings(&defaults, cx))
}

fn merged_default_settings() -> Result<String> {
    let mut defaults: serde_json::Value =
        settings::parse_json_with_comments(&settings::default_settings())?;
    let overrides: serde_json::Value = settings::parse_json_with_comments(DEFAULT_OVERRIDES)?;
    util::merge_json_value_into(overrides, &mut defaults);
    Ok(serde_json::to_string(&defaults)?)
}

// Copied from crates/zed/src/zed.rs (`watch_settings_files`), without the
// settings-migration banner.
pub fn watch_settings_files(fs: Arc<dyn Fs>, cx: &mut App) {
    SettingsStore::update_global(cx, move |store, cx| {
        store.watch_settings_files(fs, cx, |settings_file, result, cx| {
            notify_settings_errors(result, matches!(settings_file, SettingsFile::User), cx);
        });
    });
}

// Copied from crates/zed/src/zed.rs (`notify_settings_errors`).
fn notify_settings_errors(result: settings::SettingsParseResult, is_user: bool, cx: &mut App) {
    if let settings::ParseStatus::Failed { error } = &result.parse_status {
        let settings_type = if is_user { "user" } else { "global" };
        log::error!("Failed to load {settings_type} settings: {error}");
    }

    let parse_error_id =
        NotificationId::Named(format!("failed-to-parse-settings-{is_user}").into());
    let showed_parse_error = match result.parse_status {
        settings::ParseStatus::Unchanged => return,
        settings::ParseStatus::Success => {
            dismiss_app_notification(&parse_error_id, cx);
            false
        }
        settings::ParseStatus::Failed { error } => {
            let error = anyhow::format_err!(error);
            // Projects show their own local settings errors.
            if let Some(InvalidSettingsError::LocalSettings { .. }) =
                error.downcast_ref::<InvalidSettingsError>()
            {
                false
            } else {
                show_settings_notification(
                    parse_error_id,
                    format!("Invalid user settings file\n{error}"),
                    cx,
                );
                true
            }
        }
    };

    let migration_error_id =
        NotificationId::Named(format!("failed-to-migrate-settings-{is_user}").into());
    match result.migration_status {
        settings::MigrationStatus::Succeeded | settings::MigrationStatus::NotNeeded => {
            dismiss_app_notification(&migration_error_id, cx);
        }
        settings::MigrationStatus::Failed { error } => {
            if !showed_parse_error {
                show_settings_notification(
                    migration_error_id,
                    format!("Failed to migrate settings\n{error}"),
                    cx,
                );
            }
        }
    }
}

fn show_settings_notification(id: NotificationId, message: String, cx: &mut App) {
    show_app_notification(id, cx, move |cx| {
        cx.new(|cx| {
            MessageNotification::new(message.clone(), cx)
                .primary_message("Open Settings File")
                .primary_icon(IconName::Settings)
                .primary_on_click(|window, cx| {
                    window.dispatch_action(zed_actions::OpenSettingsFile.boxed_clone(), cx);
                    cx.emit(DismissEvent);
                })
        })
    });
}

/// Applies settings that live on the app or its windows rather than on a view,
/// and keeps them in sync as settings change.
pub fn observe_app_settings(cx: &mut App) {
    init_cursor_hide_mode(cx);
    init_app_appearance(cx);
    init_reduce_motion(cx);

    // Copied from crates/zed/src/main.rs (the `SettingsStore` observer in `main`),
    // without following `server_url`, since mustr never contacts Zed's servers.
    cx.observe_global::<SettingsStore>(move |cx| {
        let background_appearance = cx.theme().window_background_appearance();
        for window in cx.windows() {
            window
                .update(cx, |_, window, _| {
                    window.set_background_appearance(background_appearance)
                })
                .ok();
        }

        cx.set_text_rendering_mode(
            match WorkspaceSettings::get_global(cx).text_rendering_mode {
                settings::TextRenderingMode::PlatformDefault => {
                    gpui::TextRenderingMode::PlatformDefault
                }
                settings::TextRenderingMode::Subpixel => gpui::TextRenderingMode::Subpixel,
                settings::TextRenderingMode::Grayscale => gpui::TextRenderingMode::Grayscale,
            },
        );
    })
    .detach();
}

// Copied from crates/zed/src/zed.rs (`CursorHideModeSetting`, `init_cursor_hide_mode`).
#[derive(Copy, Clone, Debug, settings::RegisterSetting)]
struct CursorHideModeSetting(gpui::CursorHideMode);

impl Settings for CursorHideModeSetting {
    fn from_settings(content: &settings::SettingsContent) -> Self {
        Self(match content.hide_mouse.unwrap_or_default() {
            settings::HideMouseMode::Never => gpui::CursorHideMode::Never,
            settings::HideMouseMode::OnTyping => gpui::CursorHideMode::OnTyping,
            settings::HideMouseMode::OnTypingAndAction => gpui::CursorHideMode::OnTypingAndAction,
        })
    }
}

fn init_cursor_hide_mode(cx: &mut App) {
    let apply = |cx: &mut App| cx.set_cursor_hide_mode(CursorHideModeSetting::get_global(cx).0);
    apply(cx);
    cx.observe_global::<SettingsStore>(apply).detach();
}

// Copied from crates/zed/src/zed.rs (`init_app_appearance`).
fn init_app_appearance(cx: &mut App) {
    // Make the native window chrome match the theme, so a dark theme doesn't get
    // a light window border when the OS is in light mode.
    let apply = |cx: &mut App| {
        let appearance = match ThemeSettings::get_global(cx).theme.mode() {
            Some(theme_settings::ThemeAppearanceMode::System) => None,
            _ => Some(match cx.theme().appearance() {
                theme::Appearance::Light => gpui::WindowAppearance::Light,
                theme::Appearance::Dark => gpui::WindowAppearance::Dark,
            }),
        };
        cx.set_window_appearance(appearance);
    };
    apply(cx);
    cx.observe_global::<SettingsStore>(apply).detach();
}

// Copied from crates/zed/src/zed.rs (`ReduceMotionSetting`, `init_reduce_motion`).
#[derive(Copy, Clone, Debug, settings::RegisterSetting)]
struct ReduceMotionSetting(settings::ReduceMotionMode);

impl Settings for ReduceMotionSetting {
    fn from_settings(content: &settings::SettingsContent) -> Self {
        Self(content.reduce_motion.unwrap_or_default())
    }
}

fn init_reduce_motion(cx: &mut App) {
    let apply = |cx: &mut App| {
        let reduce_motion = ReduceMotionSetting::get_global(cx).0 == settings::ReduceMotionMode::On;
        cx.set_reduce_motion(reduce_motion);
    };
    apply(cx);
    cx.observe_global::<SettingsStore>(apply).detach();
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::Value;

    /// Fails if an upstream Zed change renames or removes a setting that
    /// mustr overrides, which would otherwise be silently ignored.
    #[test]
    fn overrides_only_change_existing_zed_settings() {
        fn assert_paths_exist(overrides: &Value, defaults: &Value, path: &str) {
            let Value::Object(overrides) = overrides else {
                return;
            };
            for (key, value) in overrides {
                let path = format!("{path}/{key}");
                let default = defaults
                    .get(key)
                    .unwrap_or_else(|| panic!("{path} is not in Zed's default settings"));
                assert_paths_exist(value, default, &path);
            }
        }

        let defaults: Value =
            settings::parse_json_with_comments(&settings::default_settings()).unwrap();
        let overrides: Value = settings::parse_json_with_comments(DEFAULT_OVERRIDES).unwrap();
        assert_paths_exist(&overrides, &defaults, "");

        let merged: Value = serde_json::from_str(&merged_default_settings().unwrap()).unwrap();
        assert_eq!(merged["disable_ai"], Value::Bool(true));
        assert_eq!(merged["agent"]["enabled"], Value::Bool(false));
        assert_eq!(merged["edit_predictions"]["provider"], Value::from("none"));
        assert_eq!(merged["show_edit_predictions"], Value::Bool(false));
        assert_eq!(merged["server_url"], Value::from("https://zed.invalid"));
        assert_eq!(merged["auto_update"], Value::Bool(false));
        assert_eq!(merged["telemetry"]["metrics"], Value::Bool(false));
        assert_eq!(merged["telemetry"]["diagnostics"], Value::Bool(false));
        assert_eq!(merged["suggest_extensions"], Value::Bool(false));
        let auto_installs = merged["auto_install_extensions"].as_object().unwrap();
        for (extension, enabled) in auto_installs {
            assert_eq!(
                enabled,
                &Value::Bool(false),
                "Zed auto-installs the {extension} extension; turn it off in default_overrides.json"
            );
        }
        // mustr's title bar shows the tabs.
        assert_eq!(merged["tab_bar"]["show"], Value::Bool(false));
        // Twice the mockups' 13px, the size `mustr_ui::tokens::UI_FONT_SIZE` says they're
        // drawn at.
        assert_eq!(
            merged["ui_font_size"].as_f64(),
            Some(2. * f64::from(f32::from(mustr_ui::tokens::UI_FONT_SIZE)))
        );
        assert_eq!(merged["buffer_font_size"], merged["ui_font_size"]);
        assert_eq!(
            merged["theme"]["dark"],
            Value::from(mustr_ui::DARK_THEME_NAME)
        );
        assert_eq!(
            merged["theme"]["light"],
            Value::from(mustr_ui::LIGHT_THEME_NAME)
        );
        assert_eq!(merged["theme"]["mode"], Value::from("dark"));
    }

    /// The settings Zed's code reads, after mustr's overrides: Zed's AI is off, and Zed's
    /// client points nowhere.
    #[gpui::test]
    fn effective_defaults_turn_off_zed_ai_and_services(cx: &mut gpui::TestAppContext) {
        cx.update(|cx| {
            let settings_store = SettingsStore::test(cx);
            cx.set_global(settings_store);
            apply_default_overrides(cx).unwrap();

            assert!(project::DisableAiSettings::get_global(cx).disable_ai);
            let languages = language::language_settings::AllLanguageSettings::get_global(cx);
            assert_eq!(
                languages.edit_predictions.provider,
                settings::EditPredictionProvider::None
            );
            assert!(!languages.defaults.show_edit_predictions);

            let telemetry = client::TelemetrySettings::get_global(cx);
            assert!(!telemetry.metrics && !telemetry.diagnostics);

            let server_url = &client::ClientSettings::get_global(cx).server_url;
            let host = url::Url::parse(server_url)
                .unwrap()
                .host_str()
                .unwrap()
                .to_string();
            assert!(!host.ends_with("zed.dev"), "{server_url}");
            // Refused before any DNS lookup.
            assert!(crate::zed_services::is_blocked_host(&host), "{server_url}");
        });
    }

    #[gpui::test]
    fn initial_settings_survive_saving_a_setting(cx: &mut gpui::TestAppContext) {
        let store = cx.update(SettingsStore::test);
        let new_text = store
            .new_text_for_update(INITIAL_SETTINGS.to_string(), |settings| {
                settings.editor.diff_view_style = Some(settings::DiffViewStyle::Unified);
            })
            .unwrap();
        let parsed: Value = settings::parse_json_with_comments(&new_text).unwrap();
        assert_eq!(parsed, serde_json::json!({"diff_view_style": "unified"}));
        assert!(
            new_text.contains("  // mustr uses Zed's settings format"),
            "{new_text}"
        );
    }

    #[test]
    fn creates_a_mustr_settings_file_only_when_missing() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config").join("settings.json");
        ensure_settings_file(&path).unwrap();
        assert_eq!(std::fs::read_to_string(&path).unwrap(), INITIAL_SETTINGS);
        settings::parse_json_with_comments::<Value>(INITIAL_SETTINGS).unwrap();

        std::fs::write(&path, "{\"vim_mode\": true}").unwrap();
        ensure_settings_file(&path).unwrap();
        assert_eq!(
            std::fs::read_to_string(&path).unwrap(),
            "{\"vim_mode\": true}"
        );
    }
}

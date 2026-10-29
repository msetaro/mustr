use anyhow::{Context as _, Result};
use fs::Fs;
use futures::{StreamExt as _, channel::mpsc};
use gpui::{
    Action as _, App, AppContext as _, DismissEvent, Global, SharedString, UpdateGlobal as _,
};
use serde_json::Value;
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
/// inside the braces. The user changes settings in mustr's Settings page, never here; the file is
/// where the page keeps them.
pub const INITIAL_SETTINGS: &str = "{\n  // mustr's settings, as mustr's Settings page saves them (in Zed's settings\n  // format). Change them there.\n}\n";

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
            if is_user {
                ConfigFileErrors::set_settings(None, cx);
            }
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
                if is_user {
                    ConfigFileErrors::set_settings(Some(format!("{error:#}").into()), cx);
                }
                show_settings_notification(
                    parse_error_id,
                    format!("mustr couldn't read its settings\n{error}"),
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

/// A toast about the settings file, whose button opens the Settings page, which shows the error
/// and offers to start from mustr's defaults. mustr never opens the file itself.
fn show_settings_notification(id: NotificationId, message: String, cx: &mut App) {
    show_app_notification(id, cx, move |cx| {
        cx.new(|cx| {
            MessageNotification::new(message.clone(), cx)
                .primary_message("Open Settings")
                .primary_icon(IconName::Settings)
                .primary_on_click(|window, cx| {
                    window.dispatch_action(crate::window::OpenSettings.boxed_clone(), cx);
                    cx.emit(DismissEvent);
                })
        })
    });
}

/// Why mustr couldn't read the user's settings file or key bindings file, if it couldn't. The
/// Settings page shows these, where Zed would open the file.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct ConfigFileErrors {
    pub settings: Option<SharedString>,
    pub keymap: Option<SharedString>,
}

impl Global for ConfigFileErrors {}

impl ConfigFileErrors {
    pub fn get(cx: &App) -> Self {
        cx.try_global::<Self>().cloned().unwrap_or_default()
    }

    pub fn set_settings(error: Option<SharedString>, cx: &mut App) {
        if Self::get(cx).settings != error {
            cx.default_global::<Self>().settings = error;
        }
    }

    pub fn set_keymap(error: Option<SharedString>, cx: &mut App) {
        if Self::get(cx).keymap != error {
            cx.default_global::<Self>().keymap = error;
        }
    }
}

/// Moves a file mustr can't read aside, as `<name>.bak` next to it, and starts a new one with
/// `initial`, for the Settings page's "Start from the defaults". Returns the backup's path.
pub fn set_aside(path: &Path, initial: &str) -> Result<std::path::PathBuf> {
    let file_name = path
        .file_name()
        .context("the file has no name")?
        .to_string_lossy()
        .into_owned();
    let backup = path.with_file_name(format!("{file_name}.bak"));
    std::fs::rename(path, &backup)
        .with_context(|| format!("moving {} to {}", path.display(), backup.display()))?;
    std::fs::write(path, initial).with_context(|| format!("writing {}", path.display()))?;
    Ok(backup)
}

/// `text` (a settings file, JSON with comments) with `value` at `path`, or without `path` when
/// `value` is `None`, keeping the rest of the text (comments, order, other settings) as it was.
/// Refuses text that isn't valid, so a file mustr can't read is never overwritten.
pub fn set_value_in_settings_text(
    text: &str,
    path: &[&str],
    value: Option<&Value>,
) -> Result<String> {
    let text = if text.trim().is_empty() {
        INITIAL_SETTINGS
    } else {
        text
    };
    let old: Value = settings::parse_json_with_comments(text)?;
    let existing = path
        .iter()
        .try_fold(&old, |value, key| value.get(*key))
        .cloned();
    if existing.as_ref() == value {
        return Ok(text.to_string());
    }
    if value.is_none() && settings::find_value_range_in_json_text(text, path).is_none() {
        return Ok(text.to_string());
    }
    let tab_size = settings::infer_json_indent_size(text);
    let (range, replacement) =
        settings::replace_value_in_json_text(text, path, tab_size, value, None);
    let mut new_text = text.to_string();
    new_text.replace_range(range, &replacement);
    let new: Value = settings::parse_json_with_comments(&new_text)
        .context("changing the setting would leave the settings file unreadable")?;
    let saved = path.iter().try_fold(&new, |value, key| value.get(*key));
    anyhow::ensure!(
        saved == value,
        "the settings file didn't take {}",
        path.join(".")
    );
    Ok(new_text)
}

/// A change to one of mustr's own settings in the settings file.
struct MustrSettingEdit {
    path: &'static [&'static str],
    value: Option<Value>,
}

struct MustrSettingsWriter(mpsc::UnboundedSender<MustrSettingEdit>);

impl Global for MustrSettingsWriter {}

/// Saves one of mustr's own settings (`"notifications"`, `"updates"`, `"t3"`) in the settings
/// file, or removes it when `value` is `None`. Zed's settings types don't have these, so Zed's
/// writer can't change them. Changes are written one at a time, each after the Zed settings
/// changes asked for before it, and keep the rest of the file as it was.
pub fn update_mustr_setting(path: &'static [&'static str], value: Option<Value>, cx: &mut App) {
    if !cx.has_global::<MustrSettingsWriter>() {
        let (sender, mut edits) = mpsc::unbounded::<MustrSettingEdit>();
        cx.set_global(MustrSettingsWriter(sender));
        cx.spawn(async move |cx| {
            while let Some(edit) = edits.next().await {
                let fs = cx.update(|cx| <dyn Fs>::global(cx));
                // Zed's writes asked for before this one land first.
                let zed_writes = cx.update(|cx| {
                    cx.global::<SettingsStore>()
                        .update_settings_file_with_completion(fs.clone(), |_, _| {})
                });
                zed_writes.await.ok();
                let path = paths::settings_file();
                let result = async {
                    let text = match fs.load(path).await {
                        Ok(text) => text,
                        Err(_) if !fs.is_file(path).await => INITIAL_SETTINGS.to_string(),
                        Err(error) => return Err(error),
                    };
                    let new_text =
                        set_value_in_settings_text(&text, edit.path, edit.value.as_ref())?;
                    if new_text != text {
                        fs.atomic_write(path.clone(), new_text).await?;
                    }
                    anyhow::Ok(())
                }
                .await;
                if let Err(error) = result {
                    log::error!(
                        "couldn't save {} in the settings: {error:#}",
                        edit.path.join(".")
                    );
                }
            }
        })
        .detach();
    }
    cx.global::<MustrSettingsWriter>()
        .0
        .unbounded_send(MustrSettingEdit { path, value })
        .ok();
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

/// `"updates"` in mustr's settings file (`updates.rs`). `{"enabled": false}` stops mustr from
/// checking for updates on its own; mustr › Check for Updates… still checks. Zed's own
/// `auto_update` setting is Zed's updater's, which mustr never runs.
#[derive(Clone, Copy, Debug, PartialEq, Eq, serde::Deserialize)]
#[serde(default)]
pub struct UpdateSettings {
    pub enabled: bool,
}

impl Default for UpdateSettings {
    fn default() -> Self {
        Self { enabled: true }
    }
}

impl UpdateSettings {
    /// The settings in mustr's settings file (JSON with comments). A file without `"updates"`
    /// has the defaults.
    pub fn from_settings_text(text: &str) -> Result<Self> {
        if text.trim().is_empty() {
            return Ok(Self::default());
        }
        let settings: serde_json::Value = settings::parse_json_with_comments(text)?;
        match settings.get("updates") {
            None | Some(serde_json::Value::Null) => Ok(Self::default()),
            Some(updates) => serde_json::from_value(updates.clone())
                .map_err(|error| anyhow::anyhow!("\"updates\" in mustr's settings: {error}")),
        }
    }
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
        let mut overrides: Value = settings::parse_json_with_comments(DEFAULT_OVERRIDES).unwrap();
        // A language's settings are the same keys as the top-level language settings.
        if let Some(Value::Object(languages)) = overrides
            .as_object_mut()
            .and_then(|overrides| overrides.remove("languages"))
        {
            for (language, settings) in languages {
                assert_paths_exist(&settings, &defaults, &format!("/languages/{language}"));
            }
        }
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
        // The mockups' 13px, the size `mustr_ui::tokens::UI_FONT_SIZE` says they're drawn at.
        assert_eq!(
            merged["ui_font_size"].as_f64(),
            Some(f64::from(f32::from(mustr_ui::tokens::UI_FONT_SIZE)))
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

    /// The changed words of a diff keep the theme's syntax colors: Zed's contrast adjustment for
    /// text over highlights would otherwise recolor them over their word highlights.
    #[gpui::test]
    fn changed_words_keep_their_syntax_colors(cx: &mut gpui::TestAppContext) {
        cx.update(|cx| {
            let settings_store = SettingsStore::test(cx);
            cx.set_global(settings_store);
            apply_default_overrides(cx).unwrap();
            let min_contrast =
                editor::EditorSettings::get_global(cx).minimum_contrast_for_highlights;

            let mut families = vec![mustr_ui::theme_family_content().unwrap()];
            families.push(
                theme_settings::deserialize_user_theme(
                    include_str!("../../../assets/themes/one/one.json").as_bytes(),
                )
                .unwrap(),
            );
            let mut recolored = Vec::new();
            for theme in families
                .into_iter()
                .flat_map(|family| theme_settings::refine_theme_family(family).themes)
            {
                let colors = theme.colors();
                for word in [
                    colors.version_control_word_added,
                    colors.version_control_word_deleted,
                ] {
                    let background = colors.editor_background.blend(word);
                    let syntax = theme.syntax();
                    for index in 0_usize.. {
                        let Some(style) = syntax.get(index) else {
                            break;
                        };
                        let Some(color) = style.color else { continue };
                        let name = syntax.get_capture_name(index).unwrap_or_default();
                        if ui::utils::ensure_minimum_contrast(color, background, min_contrast)
                            != color
                        {
                            recolored.push(format!("{}: {name}", theme.name));
                        }
                    }
                }
            }
            assert_eq!(recolored, Vec::<String>::new());
        });
    }

    #[gpui::test]
    fn added_languages_start_no_language_servers(cx: &mut gpui::TestAppContext) {
        cx.update(|cx| {
            let settings_store = SettingsStore::test(cx);
            cx.set_global(settings_store);
            apply_default_overrides(cx).unwrap();

            // The servers Zed's `languages` crate attaches to these languages by name.
            let attached = ["tailwindcss-language-server", "eslint", "vtsls"]
                .map(|name| language::LanguageServerName(name.into()));
            let languages = language::language_settings::AllLanguageSettings::get_global(cx);
            for name in ["HTML", "PHP", "Svelte", "Vue.js"] {
                let name = language::LanguageName::new_static(name);
                let settings = languages.language(None, Some(&name), cx);
                assert_eq!(
                    settings.customized_language_servers(&attached),
                    Vec::<language::LanguageServerName>::new(),
                    "{name}"
                );
            }
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
            new_text.contains("  // mustr's settings, as mustr's Settings page saves them"),
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

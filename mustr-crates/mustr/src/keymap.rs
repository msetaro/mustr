use futures::{StreamExt as _, channel::mpsc, select_biased};
use gpui::{Action as _, App, AppContext as _, DismissEvent, KeyBinding};
use project::DisableAiSettings;
use settings::{
    BaseKeymap, DEFAULT_KEYMAP_PATH, KeybindSource, KeymapFile, KeymapFileLoadResult,
    SPECIFIC_OVERRIDES_KEYMAP_PATH, Settings as _, SettingsStore, VIM_KEYMAP_PATH,
};
use ui::IconName;
use vim_mode_setting::{HelixModeSetting, VimModeSetting};
use workspace::notifications::{
    NotificationId, dismiss_app_notification, show_app_notification,
    simple_message_notification::MessageNotification,
};

// Copied from crates/zed/src/zed.rs (`handle_keymap_file_changes`), without the
// keymap-migration banner and the keymap editor refresh.
pub fn handle_keymap_file_changes(
    mut user_keymap_file_rx: mpsc::UnboundedReceiver<String>,
    user_keymap_watcher: gpui::Task<()>,
    cx: &mut App,
) {
    let (base_keymap_tx, mut base_keymap_rx) = mpsc::unbounded();
    let (keyboard_layout_tx, mut keyboard_layout_rx) = mpsc::unbounded();
    let mut old_base_keymap = *BaseKeymap::get_global(cx);
    let mut old_vim_enabled = VimModeSetting::get_global(cx).0;
    let mut old_helix_enabled = HelixModeSetting::get_global(cx).0;
    let mut old_disable_ai = DisableAiSettings::get_global(cx).disable_ai;

    cx.observe_global::<SettingsStore>(move |cx| {
        let new_base_keymap = *BaseKeymap::get_global(cx);
        let new_vim_enabled = VimModeSetting::get_global(cx).0;
        let new_helix_enabled = HelixModeSetting::get_global(cx).0;
        let new_disable_ai = DisableAiSettings::get_global(cx).disable_ai;

        if new_base_keymap != old_base_keymap
            || new_vim_enabled != old_vim_enabled
            || new_helix_enabled != old_helix_enabled
            || new_disable_ai != old_disable_ai
        {
            old_base_keymap = new_base_keymap;
            old_vim_enabled = new_vim_enabled;
            old_helix_enabled = new_helix_enabled;
            old_disable_ai = new_disable_ai;
            base_keymap_tx.unbounded_send(()).ok();
        }
    })
    .detach();

    #[cfg(target_os = "windows")]
    {
        let mut current_layout_id = cx.keyboard_layout().id().to_string();
        cx.on_keyboard_layout_change(move |cx| {
            let next_layout_id = cx.keyboard_layout().id();
            if next_layout_id != current_layout_id {
                current_layout_id = next_layout_id.to_string();
                keyboard_layout_tx.unbounded_send(()).ok();
            }
        })
        .detach();
    }

    #[cfg(not(target_os = "windows"))]
    {
        let mut current_mapping = cx.keyboard_mapper().get_key_equivalents().cloned();
        cx.on_keyboard_layout_change(move |cx| {
            let next_mapping = cx.keyboard_mapper().get_key_equivalents();
            if current_mapping.as_ref() != next_mapping {
                current_mapping = next_mapping.cloned();
                keyboard_layout_tx.unbounded_send(()).ok();
            }
        })
        .detach();
    }

    load_default_keymap(cx);

    struct KeymapParseErrorNotification;
    let notification_id = NotificationId::unique::<KeymapParseErrorNotification>();

    cx.spawn(async move |cx| {
        let _user_keymap_watcher = user_keymap_watcher;
        let mut user_keymap_content = String::new();
        loop {
            select_biased! {
                _ = base_keymap_rx.next() => {},
                _ = keyboard_layout_rx.next() => {},
                content = user_keymap_file_rx.next() => {
                    if let Some(content) = content {
                        user_keymap_content = match migrator::migrate_keymap(&content) {
                            Ok(Some(migrated_content)) => migrated_content,
                            _ => content,
                        };
                    }
                }
            };
            cx.update(|cx| match KeymapFile::load(&user_keymap_content, cx) {
                KeymapFileLoadResult::Success { key_bindings } => {
                    reload_keymaps(cx, key_bindings);
                    dismiss_app_notification(&notification_id, cx);
                }
                KeymapFileLoadResult::SomeFailedToLoad {
                    key_bindings,
                    error_message,
                } => {
                    if !key_bindings.is_empty() {
                        reload_keymaps(cx, key_bindings);
                    }
                    show_keymap_error(notification_id.clone(), error_message.0, cx);
                }
                KeymapFileLoadResult::JsonParseFailure { error } => show_keymap_error(
                    notification_id.clone(),
                    format!("JSON parse error in keymap file. Bindings not reloaded.\n\n{error}"),
                    cx,
                ),
            });
        }
    })
    .detach();
}

fn show_keymap_error(notification_id: NotificationId, message: String, cx: &mut App) {
    show_app_notification(notification_id, cx, move |cx| {
        cx.new(|cx| {
            MessageNotification::new(message.clone(), cx)
                .primary_message("Open Keymap File")
                .primary_icon(IconName::Settings)
                .primary_on_click(|window, cx| {
                    window.dispatch_action(zed_actions::OpenKeymapFile.boxed_clone(), cx);
                    cx.emit(DismissEvent);
                })
        })
    });
}

// Copied from crates/zed/src/zed.rs (`reload_keymaps`).
fn reload_keymaps(cx: &mut App, mut user_key_bindings: Vec<KeyBinding>) {
    cx.clear_key_bindings();
    load_default_keymap(cx);

    for key_binding in &mut user_key_bindings {
        key_binding.set_meta(KeybindSource::User.meta());
    }
    cx.bind_keys(filter_disabled_ai_bindings(user_key_bindings, cx));
    // Menus show each action's key binding, so rebuild them with the new bindings.
    cx.set_menus(crate::window::app_menus());
}

// Copied from crates/zed/src/zed.rs (`load_default_keymap`).
fn load_default_keymap(cx: &mut App) {
    let base_keymap = *BaseKeymap::get_global(cx);
    if base_keymap == BaseKeymap::None {
        return;
    }

    bind_builtin_keymap(DEFAULT_KEYMAP_PATH, KeybindSource::Default, cx);
    if let Some(asset_path) = base_keymap.asset_path() {
        bind_builtin_keymap(asset_path, KeybindSource::Base, cx);
    }
    if VimModeSetting::get_global(cx).0 || HelixModeSetting::get_global(cx).0 {
        bind_builtin_keymap(VIM_KEYMAP_PATH, KeybindSource::Vim, cx);
    }
    bind_builtin_keymap(SPECIFIC_OVERRIDES_KEYMAP_PATH, KeybindSource::Default, cx);
    // mustr's own bindings, after Zed's (so `c` in a review beats vim's `c`) and before the
    // user's (so they can be rebound).
    let mut mustr_bindings = review::key_bindings();
    mustr_bindings.extend(thread_chat::key_bindings());
    // Work packages add their bindings here, each package under its own label
    // (`mustr-docs/PARITY.md` §3).
    // WP4, ship the work:
    // WP5, scheduled tasks:
    // WP6, terminals and worktrees:
    for key_binding in &mut mustr_bindings {
        key_binding.set_meta(KeybindSource::Default.meta());
    }
    cx.bind_keys(mustr_bindings);
}

/// Binds one of Zed's built-in keymaps.
///
/// Zed's keymaps bind actions from crates that mustr doesn't include (the
/// project panel, collab, the agent, and so on), and those actions are never
/// registered. mustr skips those bindings instead of rejecting the whole file,
/// as `KeymapFile::load_asset` would.
fn bind_builtin_keymap(asset_path: &str, source: KeybindSource, cx: &mut App) {
    let content = util::asset_str::<settings::SettingsAssets>(asset_path);
    let mut key_bindings = match KeymapFile::load(&content, cx) {
        KeymapFileLoadResult::Success { key_bindings } => key_bindings,
        KeymapFileLoadResult::SomeFailedToLoad {
            key_bindings,
            error_message,
        } => {
            log::debug!(
                "skipped bindings in built-in keymap {asset_path:?}: {}",
                error_message.0
            );
            key_bindings
        }
        KeymapFileLoadResult::JsonParseFailure { error } => {
            log::error!("JSON parse error in built-in keymap {asset_path:?}: {error}");
            return;
        }
    };
    for key_binding in &mut key_bindings {
        key_binding.set_meta(source.meta());
    }
    cx.bind_keys(filter_disabled_ai_bindings(key_bindings, cx));
}

// Copied from crates/zed/src/zed.rs (`AI_ACTION_NAMESPACES`, `is_ai_keybinding`,
// `filter_disabled_ai_bindings`). Dropping these lets lower-precedence editor
// bindings fire instead of being shadowed by AI actions that do nothing.
const AI_ACTION_NAMESPACES: &[&str] = &[
    "acp::",
    "agent::",
    "assistant::",
    "edit_prediction::",
    "inline_assistant::",
    "zeta::",
];

fn is_ai_keybinding(binding: &KeyBinding) -> bool {
    let name = binding.action().name();
    AI_ACTION_NAMESPACES
        .iter()
        .any(|namespace| name.starts_with(namespace))
}

fn filter_disabled_ai_bindings(bindings: Vec<KeyBinding>, cx: &App) -> Vec<KeyBinding> {
    if !DisableAiSettings::get_global(cx).disable_ai {
        return bindings;
    }
    bindings
        .into_iter()
        .filter(|binding| !is_ai_keybinding(binding))
        .collect()
}

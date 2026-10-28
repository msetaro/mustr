use futures::{StreamExt as _, channel::mpsc, select_biased};
use gpui::{Action as _, App, AppContext as _, DismissEvent, KeyBinding};
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

    cx.observe_global::<SettingsStore>(move |cx| {
        let new_base_keymap = *BaseKeymap::get_global(cx);
        let new_vim_enabled = VimModeSetting::get_global(cx).0;
        let new_helix_enabled = HelixModeSetting::get_global(cx).0;

        if new_base_keymap != old_base_keymap
            || new_vim_enabled != old_vim_enabled
            || new_helix_enabled != old_helix_enabled
        {
            old_base_keymap = new_base_keymap;
            old_vim_enabled = new_vim_enabled;
            old_helix_enabled = new_helix_enabled;
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
    cx.bind_keys(drop_hidden_actions(user_key_bindings));
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
    mustr_bindings.extend(crate::terminal_panel::key_bindings());
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
    cx.bind_keys(drop_hidden_actions(key_bindings));
}

/// Drops bindings to Zed's AI and service actions (`hidden_actions`), in Zed's keymaps and the
/// user's. Those actions do nothing in mustr, or would reach for Zed features mustr leaves out,
/// and dropping them lets lower-precedence bindings for the same keys work. Zed drops its AI
/// bindings only while `disable_ai` is on (`filter_disabled_ai_bindings` in
/// crates/zed/src/zed.rs); mustr always does.
fn drop_hidden_actions(bindings: Vec<KeyBinding>) -> Vec<KeyBinding> {
    bindings
        .into_iter()
        .filter(|binding| !crate::hidden_actions::is_hidden(binding.action().name()))
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::hidden_actions::{is_hidden, tests::looks_like_ai_or_a_zed_service};
    use gpui::{TestAppContext, UpdateGlobal as _};
    use settings::BaseKeymapContent;

    fn bound_actions(cx: &App) -> Vec<String> {
        cx.key_bindings()
            .borrow()
            .bindings()
            .map(|binding| binding.action().name().to_string())
            .collect()
    }

    /// mustr's keymap (Zed's built-in keymaps, then mustr's own bindings) binds no Zed AI or
    /// service action, with any base keymap, in vim mode or not.
    #[gpui::test]
    fn the_keymap_binds_no_ai_or_service_actions(cx: &mut TestAppContext) {
        cx.update(|cx| {
            let settings_store = SettingsStore::test(cx);
            cx.set_global(settings_store);
            crate::app_settings::apply_default_overrides(cx).unwrap();
            // Zed's default keymap does bind AI actions, so there's something to drop.
            let zed_keymap = util::asset_str::<settings::SettingsAssets>(DEFAULT_KEYMAP_PATH);
            for action in ["agent::ToggleFocus", "editor::ShowEditPrediction"] {
                assert!(
                    zed_keymap.contains(action),
                    "Zed's keymap no longer binds {action}"
                );
            }

            for base_keymap in [
                BaseKeymapContent::Zed,
                BaseKeymapContent::VSCode,
                BaseKeymapContent::JetBrains,
                BaseKeymapContent::SublimeText,
                BaseKeymapContent::Atom,
                BaseKeymapContent::TextMate,
                BaseKeymapContent::Emacs,
                BaseKeymapContent::Cursor,
            ] {
                for vim_mode in [false, true] {
                    SettingsStore::update_global(cx, |store, cx| {
                        store.update_user_settings(cx, |settings| {
                            settings.base_keymap = Some(base_keymap);
                            settings.vim_mode = Some(vim_mode);
                        });
                    });
                    cx.clear_key_bindings();
                    load_default_keymap(cx);
                    let bound = bound_actions(cx);
                    assert!(
                        bound.len() > 500,
                        "{base_keymap:?}: only {} bindings",
                        bound.len()
                    );
                    for name in ["thread_chat::SendMessage", "review::AddComment"] {
                        assert!(
                            bound.iter().any(|bound| bound == name),
                            "{name} isn't bound"
                        );
                    }
                    let ai_or_services: Vec<_> = bound
                        .iter()
                        .filter(|name| is_hidden(name) || looks_like_ai_or_a_zed_service(name))
                        .collect();
                    assert!(
                        ai_or_services.is_empty(),
                        "{base_keymap:?} (vim mode {vim_mode}) binds {ai_or_services:?}"
                    );
                }
            }
        });
    }

    /// The user's own bindings to hidden actions are dropped too, and the rest are kept.
    #[gpui::test]
    fn user_bindings_to_ai_actions_are_dropped(cx: &mut TestAppContext) {
        cx.update(|cx| {
            let settings_store = SettingsStore::test(cx);
            cx.set_global(settings_store);
            crate::app_settings::apply_default_overrides(cx).unwrap();
            let user_keymap = r#"[{"bindings": {
                "ctrl-alt-1": "agent::ToggleFocus",
                "ctrl-alt-2": "editor::ShowEditPrediction",
                "ctrl-alt-3": "client::SignIn",
                "ctrl-alt-4": "editor::Copy"
            }}]"#;
            let KeymapFileLoadResult::Success { key_bindings } = KeymapFile::load(user_keymap, cx)
            else {
                panic!("the user keymap didn't load");
            };
            let kept: Vec<_> = drop_hidden_actions(key_bindings)
                .iter()
                .map(|binding| binding.action().name())
                .collect();
            assert_eq!(kept, ["editor::Copy"]);
        });
    }
}

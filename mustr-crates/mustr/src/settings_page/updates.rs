//! Updates: the version, whether mustr checks for updates on its own (`"updates": {"enabled"}`),
//! and Check for Updates (`updates.rs`).

use super::rows::{self, Group, SettingRow};
use crate::updates::{CheckForUpdates, Status, Updater};
use gpui::{Action as _, App, SharedString};

pub(super) fn groups(cx: &mut App) -> Vec<Group> {
    let version = env!("CARGO_PKG_VERSION");
    let updater = Updater::global(cx);
    let status = updater
        .as_ref()
        .map(|updater| updater.read(cx).status().clone());
    let automatic = updater
        .as_ref()
        .is_none_or(|updater| updater.read(cx).automatic());
    let state = match &status {
        None => "Updates are off in this build.".to_string(),
        Some(Status::Off(reason)) => format!("This build doesn't update itself: {reason}."),
        Some(Status::Idle) => "Up to date as of the last check.".to_string(),
        Some(Status::Checking) => "Checking for an update…".to_string(),
        Some(Status::Downloading(version)) => format!("Downloading mustr {version}…"),
        Some(Status::Installing(version)) => format!("Installing mustr {version}…"),
        Some(Status::Ready(version)) => {
            format!("mustr {version} is ready: restart mustr to use it.")
        }
    };
    let check_blocked: Option<SharedString> = match &status {
        None | Some(Status::Off(_)) => Some("This build doesn't update itself".into()),
        Some(Status::Checking | Status::Downloading(_) | Status::Installing(_)) => {
            Some("Checking now".into())
        }
        Some(Status::Idle | Status::Ready(_)) => None,
    };
    let toggle = rows::switch(
        "settings-update-automatically",
        automatic,
        None,
        move |on, _, cx| {
            if let Some(updater) = Updater::global(cx) {
                updater.update(cx, |updater, cx| updater.set_automatic(on, cx));
            } else {
                crate::app_settings::update_mustr_setting(
                    &["updates", "enabled"],
                    Some(serde_json::Value::Bool(on)),
                    cx,
                );
            }
        },
    );
    vec![Group::new(
        "Updates",
        vec![
            SettingRow::new(
                "settings-row-version",
                format!("mustr {version}"),
                state,
                rows::button(
                    "settings-check-for-updates",
                    "Check for Updates",
                    check_blocked,
                    |window, cx| window.dispatch_action(CheckForUpdates.boxed_clone(), cx),
                ),
            )
            .keywords("version about release check for updates"),
            SettingRow::new(
                "settings-row-update-automatically",
                "Check automatically",
                "Looks for a new mustr 20 seconds after it starts and every 4 hours, and installs \
                 it for the next start. mustr never restarts by itself.",
                toggle,
            )
            .keywords("updates auto update automatic"),
        ],
    )]
}

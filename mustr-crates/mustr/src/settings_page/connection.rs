//! Connection: which T3 Code server mustr runs agents on (this computer's T3 Code, mustr's own
//! engine, or a saved server; `servers.rs`), and the saved servers' dialog.

use super::rows::{self, Callback, Group, SettingRow};
use crate::{
    servers::{self, ManageServers, SavedServers, ServerTarget},
    t3_connection::{ConnectionStatus, EngineKind, T3Connection},
};
use gpui::{Action as _, App, SharedString, Window};
use std::rc::Rc;

/// What the connection is doing, in a line.
fn status_line(cx: &App) -> String {
    let Some(connection) = T3Connection::try_global(cx) else {
        return "mustr isn't connected to T3 Code.".to_string();
    };
    match connection.read(cx).status() {
        ConnectionStatus::Connected {
            environment,
            server_version,
            engine,
        } => {
            let version = server_version.as_ref().map_or_else(
                || "T3 Code".to_string(),
                |version| format!("T3 Code {version}"),
            );
            match engine {
                EngineKind::Embedded => format!("Running mustr's own engine, {version}."),
                EngineKind::Installed => format!("Connected to {version} on {environment}."),
                EngineKind::External => format!(
                    "Connected to {}, {version}.",
                    servers::saved_name(cx).as_deref().unwrap_or(environment)
                ),
            }
        }
        ConnectionStatus::Connecting(step) => format!("Connecting: {step}"),
        ConnectionStatus::NeedsPairing(_) => {
            "This server needs pairing. The Inbox shows how.".to_string()
        }
        ConnectionStatus::NeedsSetup(_) => {
            "T3 Code needs setting up first. The Inbox shows how.".to_string()
        }
        ConnectionStatus::Failed(error) => format!("Couldn't connect: {error}"),
    }
}

/// Connects to `target`, and lets that pick last at the next launch too: a `"t3"` engine pinned
/// in the settings would win over it.
fn switch(target: ServerTarget, cx: &mut App) {
    crate::app_settings::update_mustr_setting(&["t3"], None, cx);
    servers::switch_server(target, cx);
}

pub(super) fn groups(window: &mut Window, cx: &mut App) -> Vec<Group> {
    let current = T3Connection::try_global(cx).map(|connection| connection.read(cx).target());
    let saved = SavedServers::global(cx).read(cx).servers().to_vec();
    let mut choices: Vec<(SharedString, Callback, ServerTarget)> = vec![
        (
            "This computer's T3 Code".into(),
            Rc::new(|_: &mut Window, cx: &mut App| switch(ServerTarget::Installed, cx)),
            ServerTarget::Installed,
        ),
        (
            "mustr's own engine".into(),
            Rc::new(|_: &mut Window, cx: &mut App| switch(ServerTarget::Embedded, cx)),
            ServerTarget::Embedded,
        ),
    ];
    for server in saved {
        let target = ServerTarget::Url {
            url: server.url.clone(),
        };
        choices.push((
            format!("{} · {}", server.name, servers::default_name(&server.url)).into(),
            Rc::new({
                let target = target.clone();
                move |_: &mut Window, cx: &mut App| switch(target.clone(), cx)
            }),
            target,
        ));
    }
    // A server from `--t3-server` or the settings, which isn't saved.
    if let Some(ServerTarget::Url { url }) = &current
        && !choices
            .iter()
            .any(|(_, _, target)| Some(target) == current.as_ref())
    {
        choices.push((
            servers::default_name(url).into(),
            Rc::new(|_: &mut Window, _: &mut App| {}),
            ServerTarget::Url { url: url.clone() },
        ));
    }
    let selected = choices
        .iter()
        .position(|(_, _, target)| Some(target) == current.as_ref());
    let label = selected.map_or_else(
        || SharedString::from("Not connected"),
        |index| choices[index].0.clone(),
    );
    let server = rows::dropdown(
        "settings-server",
        label,
        choices
            .into_iter()
            .map(|(label, on_select, _)| (label, on_select))
            .collect(),
        selected,
        None,
        window,
        cx,
    );
    vec![Group::new(
        "Agents engine",
        vec![
            SettingRow::new("settings-row-server", "Server", status_line(cx), server)
                .keywords("t3 code engine connection installed embedded remote url switch"),
            SettingRow::new(
                "settings-row-saved-servers",
                "Saved servers",
                "T3 Code servers on other machines, by name and address, each with its own \
                 sign-in.",
                rows::button(
                    "settings-manage-servers",
                    "Add or remove…",
                    None,
                    |window, cx| window.dispatch_action(ManageServers.boxed_clone(), cx),
                ),
            )
            .keywords("t3 server remote url add remove tailscale ssh"),
            SettingRow::new(
                "settings-row-reconnect",
                "Reconnect",
                "Connects to the server again, as after a restart.",
                rows::button("settings-reconnect", "Reconnect", None, |_, cx| {
                    if let Some(connection) = T3Connection::try_global(cx) {
                        connection.update(cx, |connection, cx| connection.reconnect(cx));
                    }
                }),
            )
            .keywords("t3 connection retry"),
        ],
    )]
}

//! Saved servers: the file they're kept in, the addresses accepted, which server a launch picks,
//! and switching the app's one connection between them. Nothing here reaches a T3 server.

use super::*;
use crate::t3_connection::ConnectionStatus;
use gpui::TestAppContext;
use pretty_assertions::assert_eq;
use serde_json::json;

#[test]
fn servers_are_saved_by_url_with_a_name() {
    let mut list = ServerList::default();
    assert_eq!(
        list.add(" Devbox ", "http://devbox:3773").unwrap(),
        "http://devbox:3773/"
    );
    // No scheme means http; no name means the host and port.
    assert_eq!(
        list.add("", "box.tailnet.ts.net:3773").unwrap(),
        "http://box.tailnet.ts.net:3773/"
    );
    // The same address again renames it.
    list.add("Work box", "http://devbox:3773/").unwrap();
    assert_eq!(
        list.servers,
        [
            SavedServer {
                name: "Work box".into(),
                url: "http://devbox:3773/".into()
            },
            SavedServer {
                name: "box.tailnet.ts.net:3773".into(),
                url: "http://box.tailnet.ts.net:3773/".into()
            },
        ]
    );
    assert!(list.add("x", "").is_err());
    assert!(list.add("x", "ftp://devbox").is_err());
    assert!(list.add("x", "http://").is_err());

    list.current = Some(ServerTarget::Url {
        url: "http://devbox:3773/".into(),
    });
    assert!(list.remove("http://devbox:3773/"));
    assert_eq!(list.current, None);
    assert!(!list.remove("http://devbox:3773/"));
}

#[test]
fn the_list_round_trips_through_its_file() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("config").join(SERVERS_FILE);
    assert_eq!(ServerList::load(&path).unwrap(), ServerList::default());
    let mut list = ServerList::default();
    list.add("Devbox", "http://devbox:3773").unwrap();
    list.current = Some(ServerTarget::Embedded);
    list.save(&path).unwrap();
    assert_eq!(
        serde_json::from_slice::<serde_json::Value>(&std::fs::read(&path).unwrap()).unwrap(),
        json!({
            "servers": [{ "name": "Devbox", "url": "http://devbox:3773/" }],
            "current": { "kind": "embedded" }
        })
    );
    assert_eq!(ServerList::load(&path).unwrap(), list);
    std::fs::write(&path, "not json").unwrap();
    assert!(ServerList::load(&path).is_err());
}

#[test]
fn a_remembered_server_is_used_only_when_nothing_else_picks_one() {
    // Flags, the environment, and the settings pick the engine before the menu's last choice.
    assert!(t3_engine::mode_configured(Some("embedded"), None));
    assert!(t3_engine::mode_configured(
        None,
        Some(r#"{ "t3": { "mode": "external" } }"#)
    ));
    assert!(!t3_engine::mode_configured(
        None,
        Some(r#"{ "t3": { "url": "http://x" } }"#)
    ));
    assert!(remembered_target(None, true, None).is_none());
    assert!(remembered_target(Some("installed"), false, None).is_none());

    assert!(target_for(ServerTarget::Installed).is_none());
    let (mode, url) = target_for(ServerTarget::Embedded).unwrap();
    assert_eq!(mode, T3Mode::Embedded);
    assert!(url.is_err());
    let (mode, url) = target_for(ServerTarget::Url {
        url: "http://devbox:3773/".into(),
    })
    .unwrap();
    assert_eq!(mode, T3Mode::External);
    assert_eq!(url.unwrap().as_str(), "http://devbox:3773/");
}

#[gpui::test]
async fn switching_moves_the_one_connection_and_remembers_it(cx: &mut TestAppContext) {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join(SERVERS_FILE);
    let connection = cx.update(|cx| {
        SavedServers::set_global_for_test(path.clone(), cx);
        crate::t3_connection::init_for_test(T3Mode::Installed, cx)
    });
    cx.read(|cx| assert_eq!(connection.read(cx).target(), ServerTarget::Installed));

    let url = cx
        .update(|cx| add_server("Devbox", "devbox:3773", cx))
        .unwrap();
    assert_eq!(url, "http://devbox:3773/");
    cx.read(|cx| {
        let connection = connection.read(cx);
        assert_eq!(
            connection.target(),
            ServerTarget::Url {
                url: "http://devbox:3773/".into()
            }
        );
        assert_eq!(connection.mode(), T3Mode::External);
        assert_eq!(
            connection.server_url().map(|url| url.as_str()),
            Some("http://devbox:3773/")
        );
        assert!(matches!(
            connection.status(),
            ConnectionStatus::Connecting(_)
        ));
        assert_eq!(saved_name(cx).as_deref(), Some("Devbox"));
    });
    assert_eq!(
        ServerList::load(&path).unwrap(),
        ServerList {
            servers: vec![SavedServer {
                name: "Devbox".into(),
                url: "http://devbox:3773/".into()
            }],
            current: Some(ServerTarget::Url {
                url: "http://devbox:3773/".into()
            }),
        }
    );

    // The server in use can't be removed; another can.
    cx.update(|cx| remove_server("http://devbox:3773/", cx));
    assert_eq!(ServerList::load(&path).unwrap().servers.len(), 1);
    cx.update(|cx| switch_server(ServerTarget::Installed, cx));
    cx.read(|cx| {
        assert_eq!(connection.read(cx).target(), ServerTarget::Installed);
        assert_eq!(saved_name(cx), None);
    });
    cx.update(|cx| remove_server("http://devbox:3773/", cx));
    assert_eq!(
        ServerList::load(&path).unwrap(),
        ServerList {
            servers: Vec::new(),
            current: Some(ServerTarget::Installed),
        }
    );
    // Dropping the attempts' tasks before the test ends.
    cx.update(|_| drop(connection));
}

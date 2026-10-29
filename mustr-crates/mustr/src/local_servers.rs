//! The local dev servers a thread runs (`mustr-docs/PARITY.md` §2.13), as T3's
//! `preview/useDiscoveredLocalServers.ts` and its sidebar's globe button find them: T3 scans
//! its machine's listening ports (`subscribeDiscoveredLocalServers`), and ties a server to a
//! thread when one of the thread's terminals runs it. The thread toolbar shows the thread's, and
//! opens one in the system browser; mustr has no browser of its own, where T3's app opens its
//! preview.
//!
//! A server listens on T3's machine, so its `localhost` link only works here when T3 runs here.

use futures::StreamExt as _;
use gpui::{App, AppContext as _, Context, Entity, Global, Task};
use t3_client::t3_api::{
    rpc,
    types::{DiscoveredLocalServer, SubscribeDiscoveredLocalServersPayload},
};
use thread_chat::{Access, ThreadBackendExt as _, is_unsupported_rpc};

/// What T3 last found listening on its machine.
pub struct LocalServers {
    servers: Vec<DiscoveredLocalServer>,
    _follow: Task<()>,
}

struct GlobalLocalServers(Entity<LocalServers>);

impl Global for GlobalLocalServers {}

impl LocalServers {
    /// The app's one follower, which starts following T3 on first use.
    pub fn global(cx: &mut App) -> Entity<Self> {
        if let Some(global) = cx.try_global::<GlobalLocalServers>() {
            return global.0.clone();
        }
        let entity = cx.new(|cx| Self {
            servers: Vec::new(),
            _follow: Self::follow(cx),
        });
        cx.set_global(GlobalLocalServers(entity.clone()));
        entity
    }

    /// Follows the scan, again after a dropped connection; not at all on a server without it.
    fn follow(cx: &mut Context<Self>) -> Task<()> {
        let Some(backend) = thread_chat::backend(cx) else {
            return Task::ready(());
        };
        cx.spawn(async move |this, cx| {
            let mut attempt = 0;
            loop {
                let stream = backend.stream::<rpc::SubscribeDiscoveredLocalServers>(
                    SubscribeDiscoveredLocalServersPayload {
                        configured_urls: None,
                    },
                    Access::Read,
                );
                match stream {
                    Ok(mut stream) => {
                        while let Some(list) = stream.next().await {
                            match list {
                                Ok(list) => {
                                    attempt = 0;
                                    let updated = this.update(cx, |this, cx| {
                                        this.servers = list.servers;
                                        cx.notify();
                                    });
                                    if updated.is_err() {
                                        return;
                                    }
                                }
                                Err(error) if is_unsupported_rpc(&error) => return,
                                Err(error) => {
                                    log::info!("lost T3's local servers: {error:#}");
                                    break;
                                }
                            }
                        }
                    }
                    Err(error) => log::info!("can't follow T3's local servers: {error:#}"),
                }
                cx.background_executor()
                    .timer(crate::thread_git::retry_delay(attempt))
                    .await;
                attempt += 1;
            }
        })
    }

    /// The servers the thread's terminals run, by port (`useThreadDiscoveredPorts`).
    pub fn of_thread(&self, thread_id: &str) -> Vec<DiscoveredLocalServer> {
        let mut servers = self
            .servers
            .iter()
            .filter(|server| {
                server
                    .terminal
                    .as_ref()
                    .is_some_and(|terminal| terminal.thread_id == thread_id)
            })
            .cloned()
            .collect::<Vec<_>>();
        servers.sort_by_key(|server| server.port);
        servers
    }
}

/// A server's name in the toolbar: `localhost:5173`.
pub fn server_label(server: &DiscoveredLocalServer) -> String {
    let host = match server.host.as_str() {
        "127.0.0.1" | "::1" | "0.0.0.0" | "::" | "" => "localhost",
        host => host,
    };
    format!("{host}:{}", server.port)
}

/// What the toolbar's chip says: the first server, and how many more (`Open localhost:5173 (+1)`
/// in T3's sidebar).
pub fn chip_label(servers: &[DiscoveredLocalServer]) -> Option<String> {
    let first = servers.first()?;
    Some(match servers.len() {
        1 => server_label(first),
        count => format!("{} (+{})", server_label(first), count - 1),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::thread_git::tests::{Call, RecordingBackend, init_test, shell};
    use gpui::TestAppContext;
    use pretty_assertions::assert_eq;
    use serde_json::json;

    fn server(port: i64, thread: Option<&str>) -> serde_json::Value {
        json!({
            "host": "127.0.0.1", "port": port, "url": format!("http://localhost:{port}"),
            "processName": "node", "pid": 4242,
            "terminal": thread.map(|thread| json!({ "threadId": thread, "terminalId": "term-1" }))
        })
    }

    #[gpui::test]
    async fn a_threads_servers_are_the_ones_its_terminals_run(cx: &mut TestAppContext) {
        let backend = RecordingBackend::with_shell(shell(vec![]));
        backend.script_stream(
            "subscribeDiscoveredLocalServers",
            vec![json!({
                "servers": [server(5173, Some("thread-1")), server(3000, Some("thread-1")),
                            server(8080, Some("thread-2")), server(5432, None)],
                "scannedAt": "2026-10-28T09:00:00.000Z", "configuredUrlProbing": true
            })],
            true,
        );
        init_test(backend.clone(), cx);
        let servers = cx.update(LocalServers::global);
        cx.run_until_parked();
        assert_eq!(
            backend.calls_to("subscribeDiscoveredLocalServers"),
            [Call {
                tag: "subscribeDiscoveredLocalServers".to_string(),
                payload: json!({}),
                access: Access::Read,
            }]
        );
        let thread = servers.read_with(cx, |servers, _| servers.of_thread("thread-1"));
        assert_eq!(
            thread.iter().map(|server| server.port).collect::<Vec<_>>(),
            [3000, 5173]
        );
        assert_eq!(chip_label(&thread).as_deref(), Some("localhost:3000 (+1)"));
        assert_eq!(chip_label(&[]), None);

        // T3 pushes a new scan when a server stops.
        backend.push(
            "subscribeDiscoveredLocalServers",
            json!({ "servers": [server(5173, Some("thread-1"))], "scannedAt": "2026-10-28T09:01:00.000Z" }),
        );
        cx.run_until_parked();
        let thread = servers.read_with(cx, |servers, _| servers.of_thread("thread-1"));
        assert_eq!(chip_label(&thread).as_deref(), Some("localhost:5173"));
    }
}

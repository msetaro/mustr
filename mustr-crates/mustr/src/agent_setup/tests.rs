//! The agents page against a recording backend, with T3's install and sign-in states played
//! through fake streams: what each agent shows, and the exact `provider.*` and
//! `server.updateSettings` payloads its buttons send. Nothing here talks to T3, installs
//! anything, or starts a real sign-in.

use super::*;
use anyhow::anyhow;
use futures::{channel::mpsc, stream::BoxStream};
use gpui::{TestAppContext, VisualTestContext};
use parking_lot::Mutex;
use pretty_assertions::assert_eq;
use serde_json::json;
use settings::SettingsStore;
use std::collections::{HashMap, HashSet, VecDeque};
use t3_client::t3_api::{
    tagged::{
        OrchestrationV2Command, OrchestrationV2ShellStreamItem, OrchestrationV2ThreadStreamItem,
    },
    types::{
        OrchestrationV2DispatchCommandResult, OrchestrationV2GetTurnItemInput,
        OrchestrationV2GetTurnItemResult, OrchestrationV2ShellSnapshot,
        OrchestrationV2SubscribeShellInput, OrchestrationV2SubscribeThreadInput,
        OrchestrationV2ThreadDetailSnapshot, OrchestrationV2ThreadLaunchInput,
        OrchestrationV2ThreadLaunchResult,
    },
};
use thread_chat::{AttachmentUpload, ReadOnlyBackend, UnsupportedRpc};

const NOW: &str = "2026-10-28T12:00:00.000Z";
const SIGN_IN_URL: &str = "https://auth.openai.com/oauth/authorize?client_id=app_test";

// Fixtures: T3's wire JSON.

fn extend(value: &mut Value, fields: Value) {
    if let (Value::Object(value), Value::Object(fields)) = (value, fields) {
        value.extend(fields);
    }
}

fn provider(instance_id: &str, driver: &str, name: &str, fields: Value) -> Value {
    let mut provider = json!({
        "instanceId": instance_id, "driver": driver, "displayName": name,
        "enabled": true, "installed": true, "status": "ready",
        "auth": { "status": "authenticated" },
        "checkedAt": NOW, "version": "1.0.0", "models": []
    });
    extend(&mut provider, fields);
    provider
}

/// Claude Code on its own CLI, which isn't installed: T3 can't set it up itself.
fn claude_missing() -> Value {
    provider(
        "claudeAgent",
        "claudeAgent",
        "Claude Code",
        json!({
            "installed": false, "status": "error", "version": null,
            "auth": { "status": "unknown" },
            "message": "Claude Agent CLI (`claude`) was not found on PATH.",
            "setup": { "canAuthenticate": false, "canInstall": false }
        }),
    )
}

/// Codex as T3 manages it, signed out.
fn codex_managed(fields: Value) -> Value {
    let mut codex = provider(
        "codex",
        "codex",
        "Codex",
        json!({
            "installed": false, "status": "error", "version": null,
            "auth": { "status": "unauthenticated" },
            "message": "Set up Codex to get started.",
            "setup": { "canAuthenticate": true, "canInstall": true }
        }),
    );
    extend(&mut codex, fields);
    codex
}

/// Codex on its own CLI, which isn't installed.
fn codex_existing() -> Value {
    provider(
        "codex",
        "codex",
        "Codex",
        json!({
            "installed": false, "status": "error", "version": null,
            "auth": { "status": "unknown" },
            "setup": { "canAuthenticate": false, "canInstall": false }
        }),
    )
}

fn antigravity_off() -> Value {
    provider(
        "antigravity",
        "antigravity",
        "Antigravity",
        json!({
            "enabled": false, "installed": false, "status": "disabled",
            "auth": { "status": "unknown" },
            "message": "Antigravity is disabled in T3 Code settings.",
            "setup": { "canAuthenticate": true, "canInstall": true }
        }),
    )
}

fn install_state(phase: &str, fields: Value) -> Value {
    let mut state = json!({
        "driver": "codex", "operationId": null, "phase": phase,
        "downloadedBytes": 0, "totalBytes": 146_000_000, "version": "0.156.1",
        "installedVersion": null, "canRemove": false, "message": null
    });
    extend(&mut state, fields);
    state
}

fn auth_state(phase: &str, fields: Value) -> Value {
    let mut state = json!({
        "instanceId": "codex", "phase": phase, "flowId": null,
        "authorizationUrl": null, "expiresAt": null, "message": null,
        "methods": [
            { "id": "chatgpt", "name": "ChatGPT", "description": null, "type": "agent" },
            { "id": "device-code", "name": "Device code", "description": null, "type": "agent" }
        ]
    });
    extend(&mut state, fields);
    state
}

fn waiting_on(interaction: Value) -> Value {
    auth_state(
        "waiting",
        json!({ "flowId": "flow-1", "interaction": interaction }),
    )
}

fn decode<T: serde::de::DeserializeOwned>(value: Value) -> T {
    serde_json::from_value(value).expect("the fixture decodes")
}

// The recording backend.

#[derive(Clone, Debug, PartialEq)]
struct RawCall {
    tag: String,
    payload: Value,
    access: Access,
}

fn call(tag: &str, payload: Value, access: Access) -> RawCall {
    RawCall {
        tag: tag.to_string(),
        payload,
        access,
    }
}

fn manage(tag: &str, payload: Value) -> RawCall {
    call(tag, payload, Access::ProvidersManage)
}

#[derive(Default)]
struct FakeBackend {
    calls: Mutex<Vec<RawCall>>,
    /// Answers to calls, by tag, in order; T3's idle state when none is left.
    answers: Mutex<HashMap<String, VecDeque<Value>>>,
    /// The setup streams the page follows, by tag and agent, for the test to play states into.
    streams: Mutex<HashMap<String, mpsc::UnboundedSender<Result<Value>>>>,
    /// RPCs this server doesn't have.
    unsupported: Mutex<HashSet<String>>,
}

impl FakeBackend {
    /// The calls the page made, without the server config it follows.
    fn calls(&self) -> Vec<RawCall> {
        self.calls
            .lock()
            .iter()
            .filter(|call| call.tag != "subscribeServerConfig")
            .cloned()
            .collect()
    }

    /// The calls after the setup streams the page opened.
    fn commands(&self) -> Vec<RawCall> {
        self.calls()
            .into_iter()
            .filter(|call| !call.tag.ends_with(".subscribe"))
            .collect()
    }

    fn answer(&self, tag: &str, value: Value) {
        self.answers
            .lock()
            .entry(tag.to_string())
            .or_default()
            .push_back(value);
    }

    /// Plays a state into one of the page's setup streams, as T3 sends it.
    fn play(&self, tag: &str, instance_id: &str, state: Value) {
        let streams = self.streams.lock();
        let stream = streams
            .get(&format!("{tag} {instance_id}"))
            .unwrap_or_else(|| panic!("the page doesn't follow {tag} for {instance_id}"));
        stream.unbounded_send(Ok(state)).unwrap();
    }
}

fn ready<T: Send + 'static>(value: Result<T>) -> BoxFuture<'static, Result<T>> {
    futures::future::ready(value).boxed()
}

impl ThreadBackend for FakeBackend {
    fn shell_snapshot(&self) -> BoxFuture<'static, Result<OrchestrationV2ShellSnapshot>> {
        ready(Err(anyhow!("the agents page reads no shell")))
    }

    fn subscribe_shell(
        &self,
        _: OrchestrationV2SubscribeShellInput,
    ) -> Result<futures::stream::BoxStream<'static, Result<OrchestrationV2ShellStreamItem>>> {
        Ok(futures::stream::pending().boxed())
    }

    fn thread_snapshot(
        &self,
        _: String,
    ) -> BoxFuture<'static, Result<OrchestrationV2ThreadDetailSnapshot>> {
        ready(Err(anyhow!("no threads here")))
    }

    fn subscribe_thread(
        &self,
        _: OrchestrationV2SubscribeThreadInput,
    ) -> Result<BoxStream<'static, Result<OrchestrationV2ThreadStreamItem>>> {
        Err(anyhow!("no threads here"))
    }

    fn turn_item(
        &self,
        _: OrchestrationV2GetTurnItemInput,
    ) -> BoxFuture<'static, Result<OrchestrationV2GetTurnItemResult>> {
        ready(Err(anyhow!("no threads here")))
    }

    fn providers(&self) -> BoxFuture<'static, Result<Vec<ServerProvider>>> {
        ready(Ok(Vec::new()))
    }

    fn dispatch(
        &self,
        _: OrchestrationV2Command,
    ) -> BoxFuture<'static, Result<OrchestrationV2DispatchCommandResult>> {
        ready(Err(anyhow!("the agents page dispatches no commands")))
    }

    fn launch_thread(
        &self,
        _: OrchestrationV2ThreadLaunchInput,
    ) -> BoxFuture<'static, Result<OrchestrationV2ThreadLaunchResult>> {
        ready(Err(anyhow!("the agents page launches no threads")))
    }

    fn call_raw(
        &self,
        tag: &str,
        payload: Value,
        access: Access,
    ) -> BoxFuture<'static, Result<Value>> {
        self.calls.lock().push(call(tag, payload, access));
        let answer = self
            .answers
            .lock()
            .get_mut(tag)
            .and_then(VecDeque::pop_front)
            .unwrap_or_else(|| {
                if tag.starts_with("provider.install.") {
                    install_state("idle", json!({}))
                } else if tag.starts_with("provider.auth.") {
                    auth_state("idle", json!({}))
                } else {
                    Value::Null
                }
            });
        ready(Ok(answer))
    }

    fn subscribe_raw(
        &self,
        tag: &str,
        payload: Value,
        access: Access,
    ) -> Result<BoxStream<'static, Result<Value>>> {
        self.calls.lock().push(call(tag, payload.clone(), access));
        if self.unsupported.lock().contains(tag) {
            return Err(anyhow::Error::new(UnsupportedRpc {
                tag: tag.to_string(),
            }));
        }
        if tag == "subscribeServerConfig" {
            return Ok(futures::stream::pending().boxed());
        }
        let instance_id = payload["instanceId"].as_str().unwrap_or_default();
        let (sender, receiver) = mpsc::unbounded();
        self.streams
            .lock()
            .insert(format!("{tag} {instance_id}"), sender);
        Ok(receiver.boxed())
    }

    fn upload(&self, _: AttachmentUpload) -> BoxFuture<'static, Result<()>> {
        ready(Err(anyhow!("the agents page uploads nothing")))
    }
}

// The page.

fn open_page(
    backend: Arc<dyn ThreadBackend>,
    providers: Vec<Value>,
    settings: Value,
    cx: &mut TestAppContext,
) -> (Entity<AgentSetupView>, &mut VisualTestContext) {
    cx.update(|cx| {
        let settings_store = SettingsStore::test(cx);
        cx.set_global(settings_store);
        theme_settings::init(theme::LoadThemes::JustBase, cx);
        thread_chat::set_backend(backend.clone(), cx);
        ServerConfigs::global(cx).update(cx, |configs, cx| {
            configs.set_providers(decode(Value::Array(providers)), cx);
            configs.set_settings(decode(settings), cx);
        });
    });
    let (view, cx) = cx.add_window_view(|_, cx| AgentSetupView::new(Some(backend), None, cx));
    cx.simulate_resize(gpui::size(gpui::px(1200.), gpui::px(1600.)));
    cx.run_until_parked();
    (view, cx)
}

fn click(selector: &'static str, cx: &mut VisualTestContext) {
    let bounds = cx
        .debug_bounds(selector)
        .unwrap_or_else(|| panic!("{selector} isn't rendered"));
    cx.simulate_click(bounds.center(), gpui::Modifiers::none());
    cx.run_until_parked();
}

fn clipboard(cx: &mut VisualTestContext) -> Option<String> {
    cx.read_from_clipboard().and_then(|item| item.text())
}

fn sorted(mut calls: Vec<RawCall>) -> Vec<RawCall> {
    calls.sort_by(|left, right| left.tag.cmp(&right.tag));
    calls
}

#[gpui::test]
async fn agents_show_what_they_need(cx: &mut TestAppContext) {
    let backend = Arc::new(FakeBackend::default());
    let (_, cx) = open_page(
        backend.clone(),
        vec![
            antigravity_off(),
            codex_managed(json!({})),
            claude_missing(),
        ],
        json!({}),
        cx,
    );
    // Only T3's own Codex has setup to follow: its install state is a read; its sign-in state
    // needs `providers:manage`, as T3's scope table says.
    assert_eq!(
        sorted(backend.calls()),
        [
            manage("provider.auth.subscribe", json!({ "instanceId": "codex" })),
            call(
                "provider.install.subscribe",
                json!({ "instanceId": "codex" }),
                Access::Read
            ),
        ]
    );
    // Claude Code is set up from its CLI, with T3's commands.
    assert!(cx.debug_bounds("agent-runtime-claudeAgent").is_none());
    click("agent-copy-install-claudeAgent", cx);
    assert_eq!(
        clipboard(cx).as_deref(),
        Some("curl -fsSL https://claude.ai/install.sh | bash")
    );
    click("agent-copy-sign-in-claudeAgent", cx);
    assert_eq!(clipboard(cx).as_deref(), Some("claude auth login"));
    assert!(cx.debug_bounds("agent-runtime-codex").is_some());
    assert!(cx.debug_bounds("agent-account-codex").is_some());
    // Turned-off agents wait behind a button.
    assert!(cx.debug_bounds("agent-antigravity").is_none());
    click("agents-show-off", cx);
    assert!(cx.debug_bounds("agent-antigravity").is_some());
    assert_eq!(backend.commands(), []);
}

#[gpui::test]
async fn installing_runs_t3s_install(cx: &mut TestAppContext) {
    let backend = Arc::new(FakeBackend::default());
    let (view, cx) = open_page(
        backend.clone(),
        vec![codex_managed(json!({}))],
        json!({}),
        cx,
    );
    // Until T3 says how the install stands, there's nothing to start.
    click("agent-install-codex", cx);
    assert_eq!(backend.commands(), []);

    backend.play(
        "provider.install.subscribe",
        "codex",
        install_state("idle", json!({})),
    );
    cx.run_until_parked();
    backend.answer(
        "provider.install.start",
        install_state(
            "downloading",
            json!({ "operationId": "op-1", "downloadedBytes": 73_000_000 }),
        ),
    );
    click("agent-install-codex", cx);
    view.read_with(cx, |view, _| {
        let install = view.agent("codex").unwrap().install.as_ref().unwrap();
        assert_eq!(
            install_status(Some(install), false, "Codex"),
            "Downloading 73.0 MB of 146.0 MB."
        );
    });
    click("agent-cancel-install-codex", cx);

    // Installed: removing T3's copy asks first.
    backend.play(
        "provider.install.subscribe",
        "codex",
        install_state(
            "succeeded",
            json!({ "installedVersion": "0.156.1", "canRemove": true }),
        ),
    );
    cx.run_until_parked();
    click("agent-remove-codex", cx);
    click("agent-keep-codex", cx);
    assert!(cx.debug_bounds("agent-confirm-remove-codex").is_none());
    click("agent-remove-codex", cx);
    click("agent-confirm-remove-codex", cx);
    assert_eq!(
        backend.commands(),
        [
            manage("provider.install.start", json!({ "instanceId": "codex" })),
            manage(
                "provider.install.cancel",
                json!({ "instanceId": "codex", "operationId": "op-1" })
            ),
            manage("provider.install.remove", json!({ "instanceId": "codex" })),
        ]
    );
}

#[gpui::test]
async fn signing_in_follows_t3s_flow(cx: &mut TestAppContext) {
    let backend = Arc::new(FakeBackend::default());
    let installed = json!({ "installed": true, "version": "0.156.1" });
    let (view, cx) = open_page(
        backend.clone(),
        vec![codex_managed(installed)],
        json!({}),
        cx,
    );
    backend.play(
        "provider.install.subscribe",
        "codex",
        install_state("succeeded", json!({ "installedVersion": "0.156.1" })),
    );
    backend.play(
        "provider.auth.subscribe",
        "codex",
        auth_state("idle", json!({})),
    );
    cx.run_until_parked();

    // A sign-in method other than the agent's default goes with the start.
    view.update(cx, |view, _| {
        view.agents.get_mut("codex").unwrap().method_id = Some("device-code".into())
    });
    click("agent-sign-in-codex", cx);

    // A browser page that needs consent is accepted with T3 before it opens.
    backend.play(
        "provider.auth.subscribe",
        "codex",
        waiting_on(json!({
            "type": "browser", "id": "browser-1", "url": SIGN_IN_URL,
            "requiresConsent": true, "acceptsCallback": true
        })),
    );
    cx.run_until_parked();
    assert_eq!(cx.opened_url(), None);
    backend.answer(
        "provider.auth.respond",
        waiting_on(json!({
            "type": "browser", "id": "browser-1", "url": SIGN_IN_URL,
            "requiresConsent": false, "acceptsCallback": true
        })),
    );
    click("agent-open-browser-codex", cx);
    assert_eq!(cx.opened_url().as_deref(), Some(SIGN_IN_URL));

    // When the last page can't reach T3, its address finishes the sign-in.
    assert!(cx.debug_bounds("agent-continue-codex").is_some());
    view.update(cx, |view, cx| {
        view.complete_sign_in(
            "codex".into(),
            " http://localhost:1455/auth/callback?code=abc&state=xyz ".into(),
            cx,
        )
    });
    cx.run_until_parked();
    backend.play(
        "provider.auth.subscribe",
        "codex",
        waiting_on(json!({
            "type": "browser", "id": "browser-1", "url": SIGN_IN_URL,
            "requiresConsent": false
        })),
    );
    cx.run_until_parked();
    click("agent-cancel-sign-in-codex", cx);

    assert_eq!(
        backend.commands(),
        [
            manage(
                "provider.auth.start",
                json!({ "instanceId": "codex", "methodId": "device-code" })
            ),
            manage(
                "provider.auth.respond",
                json!({
                    "instanceId": "codex", "flowId": "flow-1", "interactionId": "browser-1",
                    "response": { "type": "browser", "action": "accept" }
                })
            ),
            manage(
                "provider.auth.complete",
                json!({
                    "instanceId": "codex", "flowId": "flow-1",
                    "callbackUrl": "http://localhost:1455/auth/callback?code=abc&state=xyz"
                })
            ),
            manage(
                "provider.auth.cancel",
                json!({ "instanceId": "codex", "flowId": "flow-1" })
            ),
        ]
    );

    // Signed in: signing out asks first.
    cx.update(|_, cx| {
        ServerConfigs::global(cx).update(cx, |configs, cx| {
            configs.set_providers(
                decode(json!([codex_managed(json!({
                    "installed": true, "status": "ready", "version": "0.156.1",
                    "auth": { "status": "authenticated", "email": "dev@example.com" }
                }))])),
                cx,
            )
        })
    });
    backend.play(
        "provider.auth.subscribe",
        "codex",
        auth_state("succeeded", json!({ "flowId": "flow-1" })),
    );
    cx.run_until_parked();
    view.read_with(cx, |view, cx| {
        let provider = view.providers(cx).unwrap()[0].clone();
        let auth = view.agent("codex").unwrap().auth.clone();
        assert_eq!(
            account_description(&provider, auth.as_ref(), "ironforge"),
            "Signed in as dev@example.com."
        );
        assert_eq!(sign_in_label(&provider, auth.as_ref()), "Change account");
    });
    click("agent-sign-out-codex", cx);
    click("agent-stay-signed-in-codex", cx);
    click("agent-sign-out-codex", cx);
    click("agent-confirm-sign-out-codex", cx);
    assert_eq!(
        backend.commands().last(),
        Some(&manage(
            "provider.auth.logout",
            json!({ "instanceId": "codex" })
        ))
    );
}

#[gpui::test]
async fn terminals_codes_and_credentials_answer_the_sign_in(cx: &mut TestAppContext) {
    let backend = Arc::new(FakeBackend::default());
    let installed = json!({ "installed": true, "version": "0.156.1" });
    let (view, cx) = open_page(
        backend.clone(),
        vec![codex_managed(installed)],
        json!({}),
        cx,
    );

    let terminal_state = waiting_on(json!({
        "type": "terminal", "id": "terminal-1",
        "output": "\u{1b}[1mWelcome\u{1b}[0m\r\nPaste the code: "
    }));
    backend.play("provider.auth.subscribe", "codex", terminal_state.clone());
    for _ in 0..3 {
        backend.answer("provider.auth.respond", terminal_state.clone());
    }
    cx.run_until_parked();
    assert!(cx.debug_bounds("agent-terminal-codex").is_some());
    view.update(cx, |view, cx| {
        view.type_in_terminal("codex".into(), "abc\r".into(), cx)
    });
    cx.run_until_parked();
    // T3 takes at most 4,096 characters at a time.
    let long = "x".repeat(TERMINAL_CHUNK + 10);
    view.update(cx, |view, cx| {
        view.type_in_terminal("codex".into(), long, cx)
    });
    cx.run_until_parked();
    let terminal = |data: String| {
        manage(
            "provider.auth.respond",
            json!({
                "instanceId": "codex", "flowId": "flow-1", "interactionId": "terminal-1",
                "response": { "type": "terminal", "data": data }
            }),
        )
    };
    assert_eq!(
        backend.commands(),
        [
            terminal("abc\r".into()),
            terminal("x".repeat(TERMINAL_CHUNK)),
            terminal("x".repeat(10)),
        ]
    );

    // A device code is shown, to copy; it sends nothing.
    backend.play(
        "provider.auth.subscribe",
        "codex",
        waiting_on(json!({
            "type": "deviceCode", "id": "code-1", "url": "https://example.com/device",
            "userCode": "ABCD-1234"
        })),
    );
    cx.run_until_parked();
    click("agent-copy-code-codex", cx);
    assert_eq!(clipboard(cx).as_deref(), Some("ABCD-1234"));
    assert_eq!(backend.commands().len(), 3);

    backend.play(
        "provider.auth.subscribe",
        "codex",
        waiting_on(json!({
            "type": "credentials", "id": "credentials-1",
            "fields": [{ "name": "apiKey", "label": "API key", "secret": true }]
        })),
    );
    cx.run_until_parked();
    assert!(cx.debug_bounds("agent-connect-codex").is_some());
    view.update(cx, |view, cx| {
        view.submit_credentials(
            "codex".into(),
            BTreeMap::from([("apiKey".to_string(), "sk-test".to_string())]),
            cx,
        )
    });
    cx.run_until_parked();
    assert_eq!(
        backend.commands().last(),
        Some(&manage(
            "provider.auth.respond",
            json!({
                "instanceId": "codex", "flowId": "flow-1", "interactionId": "credentials-1",
                "response": { "type": "credentials", "values": { "apiKey": "sk-test" } }
            })
        ))
    );
}

#[gpui::test]
async fn codex_can_be_handed_to_t3_and_agents_turned_on(cx: &mut TestAppContext) {
    let backend = Arc::new(FakeBackend::default());
    let settings = json!({
        "providerInstances": {
            "antigravity": {
                "driver": "antigravity", "enabled": false,
                "config": { "authMethod": "oauth-personal" }
            }
        }
    });
    let (_, cx) = open_page(
        backend.clone(),
        vec![codex_existing(), antigravity_off()],
        settings,
        cx,
    );
    click("agent-copy-install-codex", cx);
    assert_eq!(
        clipboard(cx).as_deref(),
        Some("curl -fsSL https://chatgpt.com/codex/install.sh | sh")
    );
    click("agent-manage-codex", cx);
    click("agents-show-off", cx);
    click("agent-turn-on-antigravity", cx);
    assert_eq!(
        backend.commands(),
        [
            manage(
                "server.updateSettings",
                json!({
                    "patch": {},
                    "providerInstanceMutation": {
                        "operation": "upsert", "instanceId": "codex",
                        "instance": {
                            "driver": "codex", "enabled": true,
                            "config": { "enabled": true, "setupMode": "managed" }
                        }
                    }
                })
            ),
            manage(
                "server.updateSettings",
                json!({
                    "patch": {},
                    "providerInstanceMutation": {
                        "operation": "upsert", "instanceId": "antigravity",
                        "instance": {
                            "driver": "antigravity", "enabled": true,
                            "config": { "authMethod": "oauth-personal" }
                        }
                    }
                })
            ),
        ]
    );
}

#[gpui::test]
async fn check_again_asks_t3_to_check_its_agents(cx: &mut TestAppContext) {
    let backend = Arc::new(FakeBackend::default());
    let (view, cx) = open_page(backend.clone(), vec![claude_missing()], json!({}), cx);
    backend.answer(
        "server.refreshProviders",
        json!({ "providers": [provider("claudeAgent", "claudeAgent", "Claude Code", json!({}))] }),
    );
    click("agents-refresh", cx);
    assert_eq!(
        backend.commands(),
        [call("server.refreshProviders", json!({}), Access::Read)]
    );
    view.read_with(cx, |view, cx| {
        assert_eq!(
            agent_state(&view.providers(cx).unwrap()[0]),
            AgentState::Ready
        );
    });
    // Ready agents need nothing.
    assert!(cx.debug_bounds("agent-copy-install-claudeAgent").is_none());
}

#[gpui::test]
async fn dry_runs_read_but_never_set_up(cx: &mut TestAppContext) {
    let fake = Arc::new(FakeBackend::default());
    let backend = Arc::new(ReadOnlyBackend::new(
        fake.clone(),
        "MUSTR_DRY_RUN_WRITES is set",
    ));
    let (_, cx) = open_page(
        backend,
        vec![codex_managed(json!({})), antigravity_off()],
        json!({}),
        cx,
    );
    // The sign-in state needs `providers:manage`, so a dry run doesn't follow it.
    assert_eq!(
        fake.calls(),
        [call(
            "provider.install.subscribe",
            json!({ "instanceId": "codex" }),
            Access::Read
        )]
    );
    fake.play(
        "provider.install.subscribe",
        "codex",
        install_state("idle", json!({})),
    );
    cx.run_until_parked();
    click("agent-install-codex", cx);
    click("agents-show-off", cx);
    click("agent-turn-on-antigravity", cx);
    assert_eq!(fake.commands(), []);
}

#[gpui::test]
async fn servers_without_agent_setup_say_so(cx: &mut TestAppContext) {
    let backend = Arc::new(FakeBackend::default());
    backend
        .unsupported
        .lock()
        .extend(["provider.install.subscribe".to_string()]);
    let (view, cx) = open_page(backend, vec![codex_managed(json!({}))], json!({}), cx);
    view.read_with(cx, |view, _| {
        assert_eq!(
            view.agent("codex").unwrap().follow_error.as_deref(),
            Some(
                "This T3 Code can't set up agents. Update it to install agents and sign in to \
                 them here."
            )
        );
    });
    assert!(cx.debug_bounds("agent-error-codex").is_some());
}

// What the page says.

#[test]
fn agents_are_ready_or_say_what_they_need() {
    let state = |value: Value| agent_state(&decode(value));
    assert_eq!(state(claude_missing()), AgentState::Install);
    assert_eq!(
        state(codex_managed(json!({ "installed": true }))),
        AgentState::SignIn
    );
    assert_eq!(state(antigravity_off()), AgentState::Disabled);
    assert_eq!(
        state(provider(
            "claudeAgent",
            "claudeAgent",
            "Claude Code",
            json!({})
        )),
        AgentState::Ready
    );
    assert_eq!(
        state(provider(
            "claudeAgent",
            "claudeAgent",
            "Claude Code",
            json!({ "status": "warning" })
        )),
        AgentState::Attention
    );
    assert_eq!(
        state(provider(
            "claudeAgent",
            "claudeAgent",
            "Claude Code",
            json!({ "installed": false, "status": "warning", "auth": { "status": "unknown" } })
        )),
        AgentState::Checking
    );

    let summary = |value: Value| summary(&decode(value));
    assert_eq!(
        summary(claude_missing()),
        (
            "Not found".to_string(),
            Some("Claude Agent CLI (`claude`) was not found on PATH.".to_string())
        )
    );
    assert_eq!(
        summary(provider(
            "claudeAgent",
            "claudeAgent",
            "Claude Code",
            json!({ "auth": { "status": "unauthenticated", "label": "Max" } })
        )),
        ("Not signed in · Max".to_string(), None)
    );
    assert_eq!(
        summary(provider(
            "claudeAgent",
            "claudeAgent",
            "Claude Code",
            json!({ "auth": { "status": "authenticated", "type": "apiKey" } })
        )),
        ("Signed in · apiKey".to_string(), None)
    );
}

#[test]
fn installs_read_as_t3_words_them() {
    let state = |phase: &str, fields: Value| -> ProviderInstallState {
        decode(install_state(phase, fields))
    };
    let idle = state("idle", json!({}));
    assert_eq!(
        install_status(Some(&idle), false, "Codex"),
        "146 MB download."
    );
    assert_eq!(install_label(Some(&idle), false, "Codex"), "Install Codex");
    assert_eq!(
        install_label(Some(&idle), true, "Codex"),
        "Install T3 Code's copy"
    );
    assert_eq!(install_status(None, false, "Codex"), "Not installed.");
    let extracting = state("extracting", json!({}));
    assert_eq!(
        install_status(Some(&extracting), false, "Codex"),
        "Extracting Codex."
    );
    let failed = state("failed", json!({ "message": "The download failed." }));
    assert_eq!(
        install_label(Some(&failed), false, "Codex"),
        "Retry installation"
    );
    let installed = state("succeeded", json!({ "installedVersion": "0.156.1" }));
    assert_eq!(
        install_status(Some(&installed), true, "Codex"),
        "Installed."
    );
    assert_eq!(
        install_label(Some(&installed), true, "Codex"),
        "Reinstall Codex"
    );
    let outdated = state("idle", json!({ "installedVersion": "0.150.0" }));
    assert_eq!(
        install_label(Some(&outdated), true, "Codex"),
        "Update Codex"
    );
}

#[test]
fn sign_ins_read_as_t3_words_them() {
    let codex: ServerProvider = decode(codex_managed(json!({ "installed": true })));
    let auth = |value: Value| -> ProviderAuthState { decode(value) };
    assert_eq!(
        account_description(&codex, None, "ironforge"),
        "Sign in on ironforge."
    );
    assert_eq!(sign_in_label(&codex, None), "Sign in");
    let starting = auth(auth_state(
        "starting",
        json!({ "message": "Opening ChatGPT…" }),
    ));
    assert_eq!(
        account_description(&codex, Some(&starting), "ironforge"),
        "Opening ChatGPT…"
    );
    let terminal = auth(waiting_on(
        json!({ "type": "terminal", "id": "t", "output": "" }),
    ));
    assert_eq!(
        account_description(&codex, Some(&terminal), "ironforge"),
        "Finish signing in in the terminal below."
    );
    let browser = auth(waiting_on(json!({
        "type": "browser", "id": "b", "url": SIGN_IN_URL, "requiresConsent": false
    })));
    assert_eq!(
        account_description(&codex, Some(&browser), "ironforge"),
        "Finish signing in in your browser."
    );
    let failed = auth(auth_state("failed", json!({ "message": "Denied" })));
    assert_eq!(sign_in_label(&codex, Some(&failed)), "Retry sign-in");
    // An agent T3 can't sign in to says so.
    let no_sign_in: ServerProvider = decode(provider(
        "acp",
        "acpRegistry",
        "Gemini",
        json!({
            "auth": { "status": "unauthenticated" },
            "setup": { "canAuthenticate": false, "canInstall": true }
        }),
    ));
    assert_eq!(
        account_description(&no_sign_in, None, "ironforge"),
        "This agent can't sign in here. Follow its docs to finish setup."
    );
}

#[test]
fn cli_agents_get_t3s_commands() {
    let os = |os: &str| -> ExecutionEnvironmentPlatformOs { decode(json!(os)) };
    let claude: ServerProvider = decode(claude_missing());
    assert_eq!(
        cli_setup(&claude, None, Some(&os("darwin"))),
        Some(CliSetup {
            install: "curl -fsSL https://claude.ai/install.sh | bash".into(),
            sign_in: "claude auth login".into(),
        })
    );
    assert_eq!(
        cli_setup(&claude, None, Some(&os("windows")))
            .unwrap()
            .install,
        "irm https://claude.ai/install.ps1 | iex"
    );
    let codex: ServerProvider = decode(codex_existing());
    assert_eq!(
        cli_setup(&codex, None, Some(&os("linux"))).unwrap().sign_in,
        "codex login"
    );
    // A binary path from T3's settings, quoted for the shell.
    let settings = |path: &str| -> ServerSettings {
        decode(json!({ "providerInstances": {
            "claudeAgent": { "driver": "claudeAgent", "config": { "binaryPath": path } }
        } }))
    };
    let sign_in = |path: &str, os_name: &str| {
        cli_setup(&claude, Some(&settings(path)), Some(&os(os_name)))
            .unwrap()
            .sign_in
    };
    assert_eq!(
        sign_in("/opt/bin/claude", "linux"),
        "/opt/bin/claude auth login"
    );
    assert_eq!(
        sign_in("~/My Tools/claude", "darwin"),
        "~/'My Tools/claude' auth login"
    );
    assert_eq!(
        sign_in("/opt/it's/claude", "linux"),
        "'/opt/it'\"'\"'s/claude' auth login"
    );
    assert_eq!(
        sign_in("C:\\Program Files\\claude.exe", "windows"),
        "& 'C:\\Program Files\\claude.exe' auth login"
    );
    // Other agents have no commands of T3's.
    let cursor: ServerProvider = decode(provider("cursor", "cursor", "Cursor", json!({})));
    assert_eq!(cli_setup(&cursor, None, None), None);
}

#[test]
fn sign_in_terminals_read_as_plain_lines() {
    assert_eq!(
        terminal_text(
            "\u{1b}[1;32mWelcome\u{1b}[0m to Codex\r\n\u{1b}]0;title\u{7}Loading…\rReady.\n\
             Paste the code: \u{1b}[K"
        ),
        "Welcome to Codex\nReady.\nPaste the code:"
    );
    assert_eq!(terminal_chunks(""), [String::new()]);
    assert_eq!(terminal_chunks("abc"), ["abc".to_string()]);
}

#[gpui::test]
async fn custom_models_join_the_agents_list(cx: &mut TestAppContext) {
    let backend = Arc::new(FakeBackend::default());
    let settings = json!({
        "providerInstances": {
            "codex": {
                "driver": "codex",
                "config": {
                    "binaryPath": "codex",
                    "customModels": ["gpt-5.5-codex", { "slug": "o9", "name": "O9" }]
                }
            }
        }
    });
    let ready = provider("codex", "codex", "Codex", json!({}));
    let (view, cx) = open_page(backend.clone(), vec![ready.clone()], settings.clone(), cx);
    view.read_with(cx, |view, cx| {
        assert_eq!(
            custom_models(&decode(ready.clone()), view.configs.read(cx).settings()),
            ["gpt-5.5-codex", "o9"]
        );
    });
    click("agent-remove-model-codex-o9", cx);
    // Add shows a field until it's used or cancelled.
    click("agent-add-model-codex", cx);
    assert!(cx.debug_bounds("agent-save-model-codex").is_some());
    click("agent-cancel-model-codex", cx);
    assert!(cx.debug_bounds("agent-save-model-codex").is_none());
    assert_eq!(
        backend.commands(),
        [manage(
            "server.updateSettings",
            json!({
                "patch": {},
                "providerInstanceMutation": {
                    "operation": "upsert", "instanceId": "codex",
                    "instance": {
                        "driver": "codex",
                        "config": { "binaryPath": "codex", "customModels": ["gpt-5.5-codex"] }
                    }
                }
            })
        )]
    );

    // Adding keeps the others as T3 has them; a model already there, or none, changes nothing.
    let settings: ServerSettings = decode(settings);
    let codex: ServerProvider = decode(ready);
    let added = change_custom_model(&codex, Some(&settings), " gpt-6 ", true).unwrap();
    assert_eq!(
        serde_json::to_value(&added).unwrap()["providerInstanceMutation"]["instance"]["config"]["customModels"],
        json!(["gpt-5.5-codex", { "slug": "o9", "name": "O9" }, "gpt-6"])
    );
    assert_eq!(
        change_custom_model(&codex, Some(&settings), "o9", true),
        None
    );
    assert_eq!(
        change_custom_model(&codex, Some(&settings), "  ", true),
        None
    );
    assert_eq!(
        change_custom_model(&codex, Some(&settings), "gpt-6", false),
        None
    );
    // An agent T3's settings don't have yet gets its first.
    let first = change_custom_model(&codex, None, "gpt-6", true).unwrap();
    assert_eq!(
        serde_json::to_value(&first).unwrap()["providerInstanceMutation"]["instance"],
        json!({ "driver": "codex", "config": { "customModels": ["gpt-6"] } })
    );
}

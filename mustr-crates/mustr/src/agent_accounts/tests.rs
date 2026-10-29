//! Adding and removing agents: T3's instance ids, the exact `server.updateSettings` mutations
//! and registry calls, and the registry's search. Nothing here talks to T3.

use super::*;
use crate::fake_t3::{Call, FakeT3};
use gpui::{TestAppContext, VisualTestContext};
use pretty_assertions::assert_eq;
use settings::SettingsStore;

#[test]
fn instance_ids_are_t3s() {
    assert_eq!(slugify("Work account"), "work_account");
    assert_eq!(slugify("  Ünïcode -- Team #2 "), "n_code_team_2");
    assert_eq!(slugify("___"), "");
    let taken = HashSet::from(["codex".to_string(), "codex_work".to_string()]);
    assert_eq!(instance_id("codex", "Work", &HashSet::new()), "codex_work");
    assert_eq!(instance_id("codex", "Work", &taken), "codex_work_2");
    assert_eq!(instance_id("codex", "", &taken), "codex_2");
    assert_eq!(instance_id("claudeAgent", "", &taken), "claudeAgent");
    let long = "x".repeat(80);
    let id = instance_id("acpRegistry", &long, &HashSet::new());
    assert_eq!(id.len(), "acpRegistry_".len() + MAX_SLUG);
    assert!(is_default_instance("codex", "codex"));
    assert!(!is_default_instance("codex_work", "codex"));
}

#[test]
fn mutations_are_t3s() {
    assert_eq!(
        account_payload(AccountDriver::Codex, "codex_work", "Work", "~/.codex-work"),
        json!({ "patch": {}, "providerInstanceMutation": {
            "operation": "create", "instanceId": "codex_work",
            "instance": {
                "driver": "codex", "enabled": true, "displayName": "Work",
                "config": { "homePath": "~/.codex-work", "setupMode": "existing" }
            }
        } })
    );
    assert_eq!(
        account_payload(AccountDriver::Claude, "claudeAgent_2", "", "~/.claude-2"),
        json!({ "patch": {}, "providerInstanceMutation": {
            "operation": "create", "instanceId": "claudeAgent_2",
            "instance": {
                "driver": "claudeAgent", "enabled": true, "config": { "homePath": "~/.claude-2" }
            }
        } })
    );
    assert_eq!(
        remove_payload("codex_work"),
        json!({ "patch": {}, "providerInstanceMutation": {
            "operation": "remove", "instanceId": "codex_work"
        } })
    );
}

fn agent(id: &str, name: &str) -> Value {
    json!({
        "id": id, "name": name, "version": "1.2.0", "description": format!("{name} over ACP"),
        "authors": ["someone"], "license": "MIT", "website": null, "repository": null,
        "icon": format!("https://cdn.agentclientprotocol.com/registry/v1/latest/{id}.svg"),
        "distribution": "npx", "integrity": "registry"
    })
}

fn settings_with_registry_agent() -> Value {
    json!({
        "providerInstances": {
            "acpRegistry_goose": {
                "driver": "acpRegistry", "enabled": true, "displayName": "Goose",
                "config": { "agentId": "goose", "distribution": "auto" }
            },
            "acpRegistry_local": {
                "driver": "acpRegistry", "enabled": true,
                "config": { "source": "local", "commandPath": "/bin/agent", "agentId": "mine" }
            }
        }
    })
}

#[test]
fn registry_agents_are_found_by_their_id() {
    let settings: ServerSettings = serde_json::from_value(settings_with_registry_agent()).unwrap();
    assert_eq!(
        added_agents(Some(&settings)),
        HashSet::from(["goose".to_string(), "mine".to_string()])
    );
    assert_eq!(
        registry_agent_of(Some(&settings), "acpRegistry_goose").as_deref(),
        Some("goose")
    );
    // A local command wasn't downloaded, so there's nothing to uninstall.
    assert_eq!(
        registry_agent_of(Some(&settings), "acpRegistry_local"),
        None
    );
    let agent: AcpRegistrySearchAgent = serde_json::from_value(agent("kimi", "Kimi CLI")).unwrap();
    assert_eq!(
        registry_payload(&agent, &HashSet::new()),
        json!({ "patch": {}, "providerInstanceMutation": {
            "operation": "create", "instanceId": "acpRegistry_kimi_cli",
            "instance": {
                "driver": "acpRegistry", "enabled": true, "displayName": "Kimi CLI",
                "config": {
                    "agentId": "kimi", "distribution": "auto",
                    "registryIconUrl": "https://cdn.agentclientprotocol.com/registry/v1/latest/kimi.svg"
                }
            }
        } })
    );
}

fn open_dialog(
    backend: Arc<FakeT3>,
    cx: &mut TestAppContext,
) -> (Entity<AddAgentDialog>, &mut VisualTestContext) {
    cx.update(|cx| {
        let settings_store = SettingsStore::test(cx);
        cx.set_global(settings_store);
        theme_settings::init(theme::LoadThemes::JustBase, cx);
        thread_chat::set_backend(backend.clone(), cx);
        ServerConfigs::global(cx).update(cx, |configs, cx| {
            configs.set_settings(
                serde_json::from_value(settings_with_registry_agent()).unwrap(),
                cx,
            )
        });
    });
    let (view, cx) = cx.add_window_view(|window, cx| {
        AddAgentDialog::new(Some(backend as Arc<dyn ThreadBackend>), window, cx)
    });
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

#[gpui::test]
async fn another_account_needs_its_own_folder(cx: &mut TestAppContext) {
    let backend = FakeT3::new();
    backend.answer("server.updateSettings", settings_with_registry_agent());
    let (view, cx) = open_dialog(backend.clone(), cx);
    click("segment-Claude Code", cx);
    view.update_in(cx, |view, window, cx| {
        view.label
            .update(cx, |editor, cx| editor.set_text("Work", window, cx))
    });
    click("add-agent-save", cx);
    assert!(cx.debug_bounds("add-agent-error").is_some());
    assert!(backend.calls().is_empty());

    view.update_in(cx, |view, window, cx| {
        view.home.update(cx, |editor, cx| {
            editor.set_text("~/.claude-work", window, cx)
        })
    });
    click("add-agent-save", cx);
    assert_eq!(
        backend.calls(),
        [Call::new(
            "server.updateSettings",
            account_payload(
                AccountDriver::Claude,
                "claudeAgent_work",
                "Work",
                "~/.claude-work"
            ),
            Access::ProvidersManage
        )]
    );
}

#[gpui::test]
async fn registry_agents_are_searched_prepared_and_added(cx: &mut TestAppContext) {
    let backend = FakeT3::new();
    backend.answer(
        "server.searchAcpRegistry",
        json!({ "agents": [
            agent("goose", "Goose"), agent("codex-acp", "Codex"), agent("kimi", "Kimi CLI")
        ] }),
    );
    backend.answer(
        "server.prepareAcpRegistryAgent",
        json!({ "agentId": "kimi", "version": "1.2.0", "distribution": "npx", "prepared": true }),
    );
    backend.answer("server.updateSettings", settings_with_registry_agent());
    let (view, cx) = open_dialog(backend.clone(), cx);
    click("segment-ACP registry", cx);
    // Codex has T3's own driver, so the registry's copy isn't offered; Goose is added already.
    view.read_with(cx, |view, _| {
        assert_eq!(view.found(), Some(vec!["goose".into(), "kimi".into()]));
    });
    assert!(cx.debug_bounds("add-agent-registry-goose").is_none());
    click("add-agent-registry-kimi", cx);
    let kimi: AcpRegistrySearchAgent = serde_json::from_value(agent("kimi", "Kimi CLI")).unwrap();
    let taken = cx.update(|_, cx| view.read(cx).taken(cx));
    assert_eq!(
        backend.calls(),
        [
            Call::new(
                "server.searchAcpRegistry",
                json!({ "query": "", "limit": 512 }),
                Access::Read
            ),
            Call::new(
                "server.prepareAcpRegistryAgent",
                json!({ "agentId": "kimi" }),
                Access::ProvidersManage
            ),
            Call::new(
                "server.updateSettings",
                registry_payload(&kimi, &taken),
                Access::ProvidersManage
            ),
        ]
    );
}

#[gpui::test]
async fn servers_without_the_registry_say_so(cx: &mut TestAppContext) {
    let backend = FakeT3::new();
    backend.unsupported("server.searchAcpRegistry");
    let (view, cx) = open_dialog(backend, cx);
    click("segment-ACP registry", cx);
    view.read_with(cx, |view, _| assert_eq!(view.found(), None));
}

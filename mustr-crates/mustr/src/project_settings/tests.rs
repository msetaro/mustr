//! The project settings page against a scripted T3: the exact `projects.mutate` and
//! `server.updateSettings` payloads it sends, keeping the parts of a project's override entry
//! mustr doesn't know, and its read-only state.

use super::*;
use crate::fake_t3::{Call, FakeT3};
use gpui::{TestAppContext, VisualTestContext};
use pretty_assertions::assert_eq;
use settings::SettingsStore;

fn settings_json() -> Value {
    json!({
        "defaultThreadEnvMode": "local",
        "defaultModelSelection": { "instanceId": "codex", "model": "gpt-5.3-codex" },
        "projectSettingsFolded": true,
        "projectSettingsOverrides": {
            "p1": { "defaultAutoPull": true, "someSettingFromANewerT3": { "kept": 1 } },
            "p2": { "defaultThreadEnvMode": "worktree" }
        }
    })
}

#[test]
fn renaming_sends_t3s_project_update() {
    assert_eq!(
        rename_mutation("p1", "mustr", "cmd-1".into()),
        json!({
            "type": "project.update", "commandId": "cmd-1", "projectId": "p1", "title": "mustr"
        })
    );
}

#[test]
fn an_override_replaces_the_project_entry_keeping_what_mustr_doesnt_know() {
    let settings = settings_json();
    assert_eq!(
        override_patch(
            &settings,
            "p1",
            "defaultThreadEnvMode",
            Some(json!("worktree"))
        ),
        json!({ "projectSettingsOverrides": { "p1": {
            "defaultAutoPull": true,
            "someSettingFromANewerT3": { "kept": 1 },
            "defaultThreadEnvMode": "worktree"
        } } })
    );
    // Back to T3's setting drops the key from the entry.
    assert_eq!(
        override_patch(&settings, "p2", "defaultThreadEnvMode", None),
        json!({ "projectSettingsOverrides": { "p2": {} } })
    );
    // A project without an entry gets one.
    assert_eq!(
        override_patch(
            &settings,
            "p3",
            "defaultModelSelection",
            Some(json!({ "instanceId": "claudeAgent", "model": "opus" }))
        ),
        json!({ "projectSettingsOverrides": { "p3": {
            "defaultModelSelection": { "instanceId": "claudeAgent", "model": "opus" }
        } } })
    );
}

#[test]
fn a_projects_own_value_comes_from_its_override_then_its_legacy_field() {
    let mut settings = settings_json();
    let project = json!({ "id": "p1", "defaultThreadEnvMode": "worktree" });
    // Folded: T3 reads only the overrides.
    assert_eq!(
        project_value(&settings, Some(&project), "p1", "defaultThreadEnvMode"),
        None
    );
    settings["projectSettingsFolded"] = json!(false);
    assert_eq!(
        project_value(&settings, Some(&project), "p1", "defaultThreadEnvMode"),
        Some(&json!("worktree"))
    );
    assert_eq!(
        project_value(&settings, None, "p2", "defaultThreadEnvMode"),
        Some(&json!("worktree"))
    );
}

fn open_page<'a>(
    backend: Arc<FakeT3>,
    project_id: Option<&str>,
    cx: &'a mut TestAppContext,
) -> (Entity<ProjectSettingsView>, &'a mut VisualTestContext) {
    backend.serve_projects(vec![
        json!({ "id": "p1", "title": "mustr", "workspaceRoot": "/src/mustr",
                "scripts": [{ "id": "test", "name": "Test", "command": "cargo test",
                              "icon": "test", "runOnWorktreeCreate": false }] }),
        json!({ "id": "p2", "title": "zed", "workspaceRoot": "/src/zed" }),
    ]);
    if !backend.has_answer("server.getSettings") {
        backend.answer("server.getSettings", settings_json());
    }
    cx.update(|cx| {
        let settings_store = SettingsStore::test(cx);
        cx.set_global(settings_store);
        theme_settings::init(theme::LoadThemes::JustBase, cx);
        thread_chat::set_backend(backend.clone(), cx);
        ServerConfigs::global(cx).update(cx, |configs, cx| {
            configs.set_providers(
                serde_json::from_value(json!([{
                    "instanceId": "claudeAgent", "driver": "claudeAgent",
                    "displayName": "Claude Code", "enabled": true, "installed": true,
                    "status": "ready", "auth": { "status": "authenticated" },
                    "checkedAt": "2026-10-28T12:00:00.000Z", "version": "2.1.0", "models": [
                        { "slug": "claude-opus", "name": "Opus", "isCustom": false,
                          "capabilities": null }
                    ]
                }]))
                .unwrap(),
                cx,
            )
        });
    });
    let project_id = project_id.map(str::to_string);
    let (view, cx) = cx.add_window_view(|window, cx| {
        ProjectSettingsView::new(
            Some(backend as Arc<dyn ThreadBackend>),
            project_id,
            window,
            cx,
        )
    });
    cx.simulate_resize(gpui::size(gpui::px(1200.), gpui::px(2400.)));
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
async fn the_page_renames_and_sets_the_projects_defaults(cx: &mut TestAppContext) {
    let backend = FakeT3::new();
    backend.answer("projects.mutate", json!({ "title": "mustr app" }));
    // T3 answers a settings change with its settings.
    let mut worktree = settings_json();
    worktree["projectSettingsOverrides"]["p1"]["defaultThreadEnvMode"] = json!("worktree");
    backend.answer_once("server.updateSettings", worktree);
    backend.answer("server.updateSettings", settings_json());
    let (view, cx) = open_page(backend.clone(), Some("p1"), cx);
    assert_eq!(
        backend.calls(),
        [Call::new("server.getSettings", json!({}), Access::Read)]
    );
    view.read_with(cx, |view, cx| {
        assert_eq!(view.name.read(cx).text(cx), "mustr");
    });

    view.update_in(cx, |view, window, cx| {
        view.name
            .update(cx, |editor, cx| editor.set_text(" mustr app ", window, cx))
    });
    cx.run_until_parked();
    click("project-settings-save-name", cx);
    click("segment-Worktree", cx);
    click("segment-T3's default", cx);
    view.update(cx, |view, cx| {
        view.set_override(
            "defaultModelSelection",
            Some(json!({ "instanceId": "claudeAgent", "model": "claude-opus" })),
            cx,
        )
    });
    cx.run_until_parked();

    let calls = backend.calls();
    let rename = &calls[1];
    assert_eq!(rename.tag, "projects.mutate");
    assert_eq!(rename.access, Access::Operate);
    let mut payload = rename.payload.clone();
    assert!(
        payload["commandId"]
            .take()
            .as_str()
            .is_some_and(|id| !id.is_empty())
    );
    assert_eq!(
        payload,
        json!({ "type": "project.update", "commandId": null, "projectId": "p1", "title": "mustr app" })
    );
    let kept = json!({ "defaultAutoPull": true, "someSettingFromANewerT3": { "kept": 1 } });
    let with = |key: &str, value: Value| {
        let mut entry = kept.clone();
        entry[key] = value;
        entry
    };
    assert_eq!(
        calls[2..].to_vec(),
        [
            Call::new(
                "server.updateSettings",
                json!({ "patch": { "projectSettingsOverrides": {
                    "p1": with("defaultThreadEnvMode", json!("worktree"))
                } } }),
                Access::SettingsWrite
            ),
            Call::new(
                "server.updateSettings",
                json!({ "patch": { "projectSettingsOverrides": { "p1": kept } } }),
                Access::SettingsWrite
            ),
            Call::new(
                "server.updateSettings",
                json!({ "patch": { "projectSettingsOverrides": {
                    "p1": with(
                        "defaultModelSelection",
                        json!({ "instanceId": "claudeAgent", "model": "claude-opus" })
                    )
                } } }),
                Access::SettingsWrite
            ),
        ]
    );
}

#[gpui::test]
async fn a_failed_change_puts_the_settings_back(cx: &mut TestAppContext) {
    let backend = FakeT3::new();
    backend.fail("server.updateSettings", "settings.json is read-only");
    let (view, cx) = open_page(backend, Some("p2"), cx);
    click("segment-Checkout", cx);
    view.read_with(cx, |view, _| {
        assert_eq!(
            view.settings_value().unwrap()["projectSettingsOverrides"]["p2"],
            json!({ "defaultThreadEnvMode": "worktree" })
        );
        assert!(
            error_of(view)
                .unwrap()
                .contains("settings.json is read-only")
        );
    });
}

#[gpui::test]
async fn without_settings_write_the_defaults_are_read_only(cx: &mut TestAppContext) {
    let backend = FakeT3::new();
    backend.block(
        Access::SettingsWrite,
        "mustr's pairing with T3 can't change T3's settings (it lacks settings:write)",
    );
    let (_view, cx) = open_page(backend.clone(), None, cx);
    assert!(cx.debug_bounds("project-settings-read-only").is_some());
    click("segment-Worktree", cx);
    assert_eq!(backend.calls_to("server.updateSettings"), []);
}

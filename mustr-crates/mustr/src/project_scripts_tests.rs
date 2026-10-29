use super::*;
use crate::thread_git::tests::{Call, NOW, RecordingBackend, init_test, shell};
use gpui::{TestAppContext, VisualTestContext};
use pretty_assertions::assert_eq;
use workspace::{MultiWorkspace, Workspace};

fn script(id: &str, name: &str, extra: Value) -> Value {
    let mut script = json!({
        "id": id, "name": name, "command": format!("pnpm {id}"), "icon": "play",
        "runOnWorktreeCreate": false
    });
    script
        .as_object_mut()
        .unwrap()
        .extend(extra.as_object().unwrap().clone());
    script
}

fn scripts(values: Vec<Value>) -> Vec<ProjectScript> {
    serde_json::from_value(Value::Array(values)).unwrap()
}

fn project(own: Vec<Value>) -> OrchestrationProjectShell {
    serde_json::from_value(json!({
        "id": "project-1", "title": "api", "workspaceRoot": "/src/api", "scripts": own,
        "createdAt": NOW, "updatedAt": NOW, "defaultModelSelection": null
    }))
    .unwrap()
}

fn settings(value: Value) -> ServerSettings {
    serde_json::from_value(value).unwrap()
}

fn ids(scripts: &[ProjectScript]) -> Vec<&str> {
    scripts.iter().map(|script| script.id.as_str()).collect()
}

#[test]
fn the_scripts_are_t3s_resolved_ones() {
    let own = project(vec![script("test", "Test", json!({}))]);
    let defaults = json!([script("lint", "Lint", json!({}))]);
    // The project's own, without overrides.
    let plain = settings(json!({ "defaultProjectScripts": defaults }));
    assert_eq!(ids(&resolve_scripts(&plain, &own)), ["test"]);
    // The server's defaults for a project without any.
    assert_eq!(ids(&resolve_scripts(&plain, &project(vec![]))), ["lint"]);
    // The project's override wins, then the legacy one (null means the defaults).
    let overridden = settings(json!({
        "defaultProjectScripts": defaults,
        "projectSettingsOverrides": { "project-1": { "defaultProjectScripts": [script("dev", "Dev", json!({}))] } },
        "projectScriptOverrides": { "project-1": [script("old", "Old", json!({}))] }
    }));
    assert_eq!(ids(&resolve_scripts(&overridden, &own)), ["dev"]);
    let legacy = settings(json!({
        "defaultProjectScripts": defaults,
        "projectScriptOverrides": { "project-1": null }
    }));
    assert_eq!(ids(&resolve_scripts(&legacy, &own)), ["lint"]);
}

#[test]
fn saving_writes_the_whole_list_to_the_projects_override() {
    let settings = settings(json!({
        "projectSettingsOverrides": { "project-1": { "branchNamePrefix": "mt", "defaultProjectScripts": [] } }
    }));
    let next = scripts(vec![script("test", "Test", json!({}))]);
    assert_eq!(
        scripts_patch(&settings, "project-1", &next),
        json!({
            "projectSettingsOverrides": {
                "project-1": {
                    "branchNamePrefix": "mt",
                    "defaultProjectScripts": [{
                        "id": "test", "name": "Test", "command": "pnpm test", "icon": "play",
                        "runOnWorktreeCreate": false
                    }]
                }
            }
        })
    );
}

#[test]
fn new_scripts_get_t3s_ids_and_roles() {
    assert_eq!(next_script_id("Run tests!", []), "run-tests");
    assert_eq!(next_script_id("Test", ["test", "test-2"]), "test-3");
    assert_eq!(next_script_id("  ", []), "script");
    assert_eq!(
        next_script_id("A very long name for an action indeed", []),
        "a-very-long-name-for-an"
    );

    // A worktree script the agent waits for is `async: false`; settle only when it's set.
    let input = ScriptInput {
        name: " Install ".to_string(),
        command: " pnpm install ".to_string(),
        icon: ScriptIcon::Configure,
        run_on_worktree_create: true,
        wait_for_setup: true,
        run_on_settle: false,
    };
    assert_eq!(
        serde_json::to_value(build_script("install", &input, None)).unwrap(),
        json!({
            "id": "install", "name": "Install", "command": "pnpm install", "icon": "configure",
            "runOnWorktreeCreate": true, "async": false
        })
    );

    // Saving one that claims the worktree and settle roles takes them from the others.
    let existing = scripts(vec![
        script("setup", "Setup", json!({ "runOnWorktreeCreate": true })),
        script("clean", "Clean", json!({ "runOnSettle": true })),
    ]);
    let claims = ScriptInput {
        run_on_settle: true,
        ..input
    };
    let next = saved_scripts(&existing, None, &claims);
    assert_eq!(ids(&next), ["setup", "clean", "install"]);
    assert!(!next[0].run_on_worktree_create);
    assert_eq!(next[1].run_on_settle, Some(false));
    assert_eq!(next[2].run_on_settle, Some(true));

    // Editing keeps its id and place.
    let renamed = ScriptInput {
        name: "Set up".to_string(),
        ..ScriptInput::of(&existing[0])
    };
    let next = saved_scripts(&existing, Some("setup"), &renamed);
    assert_eq!(ids(&next), ["setup", "clean"]);
    assert_eq!(next[0].name, "Set up");
    assert!(next[0].run_on_worktree_create);

    assert_eq!(invalid(&ScriptInput::default()), Some("Name the action."));
}

async fn workspace(cx: &mut TestAppContext) -> (Entity<Workspace>, &mut VisualTestContext) {
    let fs = project::FakeFs::new(cx.executor());
    let project = project::Project::test(fs, [], cx).await;
    let (multi_workspace, cx) =
        cx.add_window_view(|window, cx| MultiWorkspace::test_new(project.clone(), window, cx));
    let workspace =
        multi_workspace.read_with(cx, |multi_workspace, _| multi_workspace.workspace().clone());
    (workspace, cx)
}

#[gpui::test]
async fn the_editor_saves_and_deletes_through_t3(cx: &mut TestAppContext) {
    let backend = RecordingBackend::with_shell(shell(vec![]));
    let current = json!({
        "projectSettingsOverrides": { "project-1": { "defaultProjectScripts": [script("test", "Test", json!({}))] } }
    });
    backend.respond("server.getSettings", Ok(current.clone()));
    backend.respond("server.updateSettings", Ok(current.clone()));
    backend.respond("server.updateSettings", Ok(current));
    init_test(backend.clone(), cx);
    cx.update(editor::init);
    let (workspace, cx) = workspace(cx).await;
    let store = cx.update(|_, cx| ProjectScripts::global(cx));
    store.update(cx, |store, cx| store.ensure_loaded(cx));
    cx.run_until_parked();

    // A new action, added after the project's.
    workspace.update_in(cx, |workspace, window, cx| {
        workspace.toggle_modal(window, cx, |window, cx| {
            ScriptEditor::new(project(vec![]), None, window, cx)
        })
    });
    let editor = workspace
        .read_with(cx, |workspace, cx| {
            workspace.active_modal::<ScriptEditor>(cx)
        })
        .unwrap();
    editor.update_in(cx, |editor, window, cx| {
        editor.set_input(
            ScriptInput {
                name: "Clean".to_string(),
                command: "rm -rf dist".to_string(),
                run_on_settle: true,
                ..ScriptInput::default()
            },
            window,
            cx,
        );
        editor.save(cx);
    });
    cx.run_until_parked();
    assert_eq!(
        backend.calls_to("server.updateSettings"),
        [Call {
            tag: "server.updateSettings".to_string(),
            payload: json!({ "patch": { "projectSettingsOverrides": { "project-1": {
                "defaultProjectScripts": [
                    { "id": "test", "name": "Test", "command": "pnpm test", "icon": "play", "runOnWorktreeCreate": false },
                    { "id": "clean", "name": "Clean", "command": "rm -rf dist", "icon": "play", "runOnWorktreeCreate": false, "runOnSettle": true }
                ]
            } } } }),
            access: Access::SettingsWrite,
        }]
    );
    assert!(
        workspace
            .read_with(cx, |workspace, cx| workspace
                .active_modal::<ScriptEditor>(cx))
            .is_none(),
        "saved, it closes"
    );

    // Deleting asks first.
    let test = scripts(vec![script("test", "Test", json!({}))]).remove(0);
    workspace.update_in(cx, |workspace, window, cx| {
        workspace.toggle_modal(window, cx, |window, cx| {
            ScriptEditor::new(project(vec![]), Some(&test), window, cx)
        })
    });
    let editor = workspace
        .read_with(cx, |workspace, cx| {
            workspace.active_modal::<ScriptEditor>(cx)
        })
        .unwrap();
    editor.update(cx, |editor, cx| editor.delete(cx));
    assert_eq!(backend.calls_to("server.updateSettings").len(), 1);
    editor.update(cx, |editor, cx| editor.delete(cx));
    cx.run_until_parked();
    assert_eq!(
        backend.calls_to("server.updateSettings")[1].payload,
        json!({ "patch": { "projectSettingsOverrides": { "project-1": { "defaultProjectScripts": [] } } } })
    );

    // Refused in a dry run, before it's sent.
    backend
        .blocked
        .lock()
        .insert(Access::SettingsWrite, "mustr is in a dry run".to_string());
    workspace.update_in(cx, |workspace, window, cx| {
        workspace.toggle_modal(window, cx, |window, cx| {
            ScriptEditor::new(project(vec![]), Some(&test), window, cx)
        })
    });
    let editor = workspace
        .read_with(cx, |workspace, cx| {
            workspace.active_modal::<ScriptEditor>(cx)
        })
        .unwrap();
    editor.update(cx, |editor, cx| editor.save(cx));
    cx.run_until_parked();
    assert_eq!(backend.calls_to("server.updateSettings").len(), 2);
}

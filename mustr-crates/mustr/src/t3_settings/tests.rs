//! The T3 settings page against a recording backend: how it reads T3's settings on servers with
//! and without `server.getSettings`, and the exact `server.updateSettings` patches its controls
//! send. Nothing here talks to T3.

use super::*;
use anyhow::anyhow;
use futures::{StreamExt as _, stream::BoxStream};
use gpui::{TestAppContext, VisualTestContext};
use parking_lot::Mutex;
use pretty_assertions::assert_eq;
use settings::SettingsStore;
use std::collections::HashSet;
use t3_client::t3_api::{
    tagged::{
        OrchestrationV2Command, OrchestrationV2ShellStreamItem, OrchestrationV2ThreadStreamItem,
    },
    types::{
        OrchestrationV2DispatchCommandResult, OrchestrationV2GetTurnItemInput,
        OrchestrationV2GetTurnItemResult, OrchestrationV2ShellSnapshot,
        OrchestrationV2SubscribeShellInput, OrchestrationV2SubscribeThreadInput,
        OrchestrationV2ThreadDetailSnapshot, OrchestrationV2ThreadLaunchInput,
        OrchestrationV2ThreadLaunchResult, ServerProvider,
    },
};
use thread_chat::{AttachmentUpload, ReadOnlyBackend, UnsupportedRpc};

/// T3's settings as `server.getSettings` sends them, with T3's defaults.
fn settings_json() -> Value {
    json!({
        "defaultRuntimeMode": "full-access",
        "defaultThreadEnvMode": null,
        "newWorktreesStartFromOrigin": true,
        "defaultModelSelection": null,
        "sidebarAutoSettleAfterDays": 3,
        "sidebarAutoSettleOnMerge": true,
        "sidebarAutoSettleKeepPullRequests": false,
        "branchNamingMode": "static",
        "branchNamePrefix": "t3",
        "branchNameInstructions": ""
    })
}

fn config_json(settings: Value) -> Value {
    json!({
        "environment": {
            "environmentId": "env-1", "label": "workstation", "serverVersion": "0.1.4",
            "platform": { "os": "linux", "arch": "x64" }, "orchestrationProtocolVersion": 2,
            "capabilities": {}
        },
        "auth": {
            "policy": "desktop-managed-local", "bootstrapMethods": ["desktop-bootstrap"],
            "sessionMethods": ["bearer-access-token"], "sessionCookieName": "t3_session"
        },
        "cwd": "/srv/t3", "keybindingsConfigPath": "/srv/t3/keybindings.json",
        "keybindings": [], "issues": [], "providers": [], "availableEditors": [],
        "observability": {
            "logsDirectoryPath": "/srv/t3/logs", "localTracingEnabled": false,
            "otlpTracesEnabled": false, "otlpMetricsEnabled": false
        },
        "settings": settings
    })
}

#[derive(Clone, Debug, PartialEq)]
struct RawCall {
    tag: String,
    payload: Value,
    access: Access,
}

fn update(patch: Value) -> RawCall {
    RawCall {
        tag: "server.updateSettings".into(),
        payload: json!({ "patch": patch }),
        access: Access::SettingsWrite,
    }
}

/// Plays T3: keeps its settings, applies each patch it gets, and answers with the result.
struct FakeBackend {
    calls: Mutex<Vec<RawCall>>,
    settings: Mutex<Value>,
    unsupported: Mutex<HashSet<String>>,
    /// Refuses writes, as T3 does when a value is out of range.
    failing: Mutex<bool>,
}

impl FakeBackend {
    fn new() -> Arc<Self> {
        Arc::new(Self {
            calls: Mutex::default(),
            settings: Mutex::new(settings_json()),
            unsupported: Mutex::default(),
            failing: Mutex::new(false),
        })
    }

    fn calls(&self) -> Vec<RawCall> {
        self.calls
            .lock()
            .iter()
            .filter(|call| call.tag != "subscribeServerConfig")
            .cloned()
            .collect()
    }

    fn updates(&self) -> Vec<RawCall> {
        self.calls()
            .into_iter()
            .filter(|call| call.tag == "server.updateSettings")
            .collect()
    }
}

fn ready<T: Send + 'static>(value: Result<T>) -> BoxFuture<'static, Result<T>> {
    futures::future::ready(value).boxed()
}

impl ThreadBackend for FakeBackend {
    fn shell_snapshot(&self) -> BoxFuture<'static, Result<OrchestrationV2ShellSnapshot>> {
        ready(Err(anyhow!("the settings page reads no shell")))
    }

    fn subscribe_shell(
        &self,
        _: OrchestrationV2SubscribeShellInput,
    ) -> Result<BoxStream<'static, Result<OrchestrationV2ShellStreamItem>>> {
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
        ready(Err(anyhow!("the settings page dispatches no commands")))
    }

    fn launch_thread(
        &self,
        _: OrchestrationV2ThreadLaunchInput,
    ) -> BoxFuture<'static, Result<OrchestrationV2ThreadLaunchResult>> {
        ready(Err(anyhow!("the settings page launches no threads")))
    }

    fn call_raw(
        &self,
        tag: &str,
        payload: Value,
        access: Access,
    ) -> BoxFuture<'static, Result<Value>> {
        self.calls.lock().push(RawCall {
            tag: tag.to_string(),
            payload: payload.clone(),
            access,
        });
        if self.unsupported.lock().contains(tag) {
            return ready(Err(anyhow::Error::new(UnsupportedRpc {
                tag: tag.to_string(),
            })));
        }
        let mut settings = self.settings.lock();
        ready(match tag {
            "server.getSettings" => Ok(settings.clone()),
            "server.getConfig" => Ok(config_json(settings.clone())),
            "server.updateSettings" if *self.failing.lock() => {
                Err(anyhow!("Branch prefixes can't hold spaces"))
            }
            "server.updateSettings" => {
                if let (Value::Object(settings), Value::Object(patch)) =
                    (&mut *settings, &payload["patch"])
                {
                    settings.extend(patch.clone());
                }
                Ok(settings.clone())
            }
            _ => Err(anyhow!("the settings page doesn't call {tag}")),
        })
    }

    fn subscribe_raw(
        &self,
        tag: &str,
        _: Value,
        _: Access,
    ) -> Result<BoxStream<'static, Result<Value>>> {
        self.calls.lock().push(RawCall {
            tag: tag.to_string(),
            payload: Value::Null,
            access: Access::Read,
        });
        Ok(futures::stream::pending().boxed())
    }

    fn upload(&self, _: AttachmentUpload) -> BoxFuture<'static, Result<()>> {
        ready(Err(anyhow!("the settings page uploads nothing")))
    }
}

fn open_page(
    backend: Arc<dyn ThreadBackend>,
    cx: &mut TestAppContext,
) -> (Entity<T3SettingsView>, &mut VisualTestContext) {
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
    let (view, cx) =
        cx.add_window_view(|window, cx| T3SettingsView::new(Some(backend), window, cx));
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
async fn the_page_reads_and_changes_t3s_settings(cx: &mut TestAppContext) {
    let backend = FakeBackend::new();
    let (view, cx) = open_page(backend.clone(), cx);
    assert_eq!(
        backend.calls(),
        [RawCall {
            tag: "server.getSettings".into(),
            payload: json!({}),
            access: Access::Read,
        }]
    );
    view.read_with(cx, |view, cx| {
        assert_eq!(view.prefix.read(cx).text(cx), "t3");
    });

    click("segment-New worktree", cx);
    click("t3-settings-from-origin", cx);
    click("t3-settings-settle-on-merge", cx);
    click("t3-settings-keep-pull-requests", cx);
    click("t3-settings-settle-inactive", cx);
    // Off hides the wait and the PR rule; on brings back T3's three days.
    assert!(cx.debug_bounds("t3-settings-settle-days").is_none());
    click("t3-settings-settle-inactive", cx);
    assert!(cx.debug_bounds("t3-settings-settle-days").is_some());

    // The prefix saves once it changed, trimmed.
    view.update_in(cx, |view, window, cx| {
        view.prefix
            .update(cx, |editor, cx| editor.set_text(" mustr/ ", window, cx))
    });
    cx.run_until_parked();
    click("t3-settings-save-prefix", cx);
    click("segment-Custom", cx);
    view.update_in(cx, |view, window, cx| {
        view.instructions.update(cx, |editor, cx| {
            editor.set_text("Use the issue ID, then a short description.", window, cx)
        })
    });
    cx.run_until_parked();
    click("t3-settings-save-instructions", cx);

    assert_eq!(
        backend.updates(),
        [
            update(json!({ "defaultThreadEnvMode": "worktree" })),
            update(json!({ "newWorktreesStartFromOrigin": false })),
            update(json!({ "sidebarAutoSettleOnMerge": false })),
            update(json!({ "sidebarAutoSettleKeepPullRequests": true })),
            update(json!({ "sidebarAutoSettleAfterDays": null })),
            update(json!({ "sidebarAutoSettleAfterDays": 3 })),
            update(json!({ "branchNamePrefix": "mustr/" })),
            update(json!({ "branchNamingMode": "custom" })),
            update(json!({
                "branchNameInstructions": "Use the issue ID, then a short description."
            })),
        ]
    );
    view.read_with(cx, |view, _| {
        let settings = view.settings().unwrap();
        assert_eq!(
            settings.default_thread_env_mode,
            Some(ThreadEnvMode::Worktree)
        );
        assert_eq!(branch_naming_mode(settings), BranchNamingMode::Custom);
        assert_eq!(branch_prefix(settings), "mustr/");
        assert_eq!(auto_settle_days(settings), Some(3));
    });
}

#[gpui::test]
async fn without_get_settings_the_page_reads_the_config(cx: &mut TestAppContext) {
    // T3's 0.1.4 nightlies dropped `server.getSettings`; their config carries the settings.
    let backend = FakeBackend::new();
    backend
        .unsupported
        .lock()
        .insert("server.getSettings".into());
    backend.settings.lock()["branchNamePrefix"] = json!("nightly");
    let (view, cx) = open_page(backend.clone(), cx);
    assert_eq!(
        backend.calls(),
        [
            RawCall {
                tag: "server.getSettings".into(),
                payload: json!({}),
                access: Access::Read,
            },
            RawCall {
                tag: "server.getConfig".into(),
                payload: json!({}),
                access: Access::Read,
            },
        ]
    );
    view.read_with(cx, |view, cx| {
        assert_eq!(branch_prefix(view.settings().unwrap()), "nightly");
        assert_eq!(view.prefix.read(cx).text(cx), "nightly");
    });
}

#[gpui::test]
async fn servers_without_update_settings_leave_the_page_read_only(cx: &mut TestAppContext) {
    let backend = FakeBackend::new();
    backend
        .unsupported
        .lock()
        .insert("server.updateSettings".into());
    let (view, cx) = open_page(backend.clone(), cx);
    assert!(cx.debug_bounds("t3-settings-read-only").is_none());
    click("t3-settings-settle-on-merge", cx);
    // The setting goes back, and nothing else is tried.
    assert!(cx.debug_bounds("t3-settings-read-only").is_some());
    view.read_with(cx, |view, _| {
        assert_eq!(
            view.settings().unwrap().sidebar_auto_settle_on_merge,
            Some(true)
        );
        assert!(view.write_unsupported);
    });
    click("t3-settings-settle-on-merge", cx);
    assert_eq!(backend.updates().len(), 1);
}

#[gpui::test]
async fn a_refused_change_puts_the_setting_back(cx: &mut TestAppContext) {
    let backend = FakeBackend::new();
    *backend.failing.lock() = true;
    let (view, cx) = open_page(backend, cx);
    click("t3-settings-keep-pull-requests", cx);
    assert!(cx.debug_bounds("t3-settings-error").is_some());
    view.read_with(cx, |view, _| {
        assert_eq!(
            view.settings()
                .unwrap()
                .sidebar_auto_settle_keep_pull_requests,
            Some(false)
        );
        assert_eq!(
            view.error.as_deref(),
            Some("Couldn't save the setting: Branch prefixes can't hold spaces")
        );
    });
}

#[gpui::test]
async fn dry_runs_show_the_settings_but_change_nothing(cx: &mut TestAppContext) {
    let fake = FakeBackend::new();
    let backend = Arc::new(ReadOnlyBackend::new(
        fake.clone(),
        "MUSTR_DRY_RUN_WRITES is set",
    ));
    let (_, cx) = open_page(backend, cx);
    assert!(cx.debug_bounds("t3-settings-read-only").is_some());
    click("t3-settings-settle-on-merge", cx);
    click("segment-New worktree", cx);
    click("segment-Semantic prefix", cx);
    assert_eq!(fake.updates(), []);
}

#[gpui::test]
async fn t3s_own_changes_reach_the_page(cx: &mut TestAppContext) {
    let backend = FakeBackend::new();
    let (view, cx) = open_page(backend, cx);
    // Another client changes the prefix: T3's `settingsUpdated` refills the untouched field.
    let mut changed = settings_json();
    changed["branchNamePrefix"] = json!("team/");
    cx.update(|_, cx| {
        ServerConfigs::global(cx).update(cx, |configs, cx| {
            configs.set_settings(serde_json::from_value(changed).unwrap(), cx)
        })
    });
    cx.run_until_parked();
    view.read_with(cx, |view, cx| {
        assert_eq!(view.prefix.read(cx).text(cx), "team/");
    });
    // A field the user is editing keeps what they typed.
    view.update_in(cx, |view, window, cx| {
        view.prefix
            .update(cx, |editor, cx| editor.set_text("mine/", window, cx))
    });
    let mut changed = settings_json();
    changed["branchNamePrefix"] = json!("theirs/");
    cx.update(|_, cx| {
        ServerConfigs::global(cx).update(cx, |configs, cx| {
            configs.set_settings(serde_json::from_value(changed).unwrap(), cx)
        })
    });
    cx.run_until_parked();
    view.read_with(cx, |view, cx| {
        assert_eq!(view.prefix.read(cx).text(cx), "mine/");
    });
}

#[gpui::test]
async fn follow_ups_are_mustrs_own(cx: &mut TestAppContext) {
    let backend = FakeBackend::new();
    let (_, cx) = open_page(backend.clone(), cx);
    cx.update(|_, cx| assert_eq!(FollowUpBehavior::get(cx), FollowUpBehavior::Queue));
    click("segment-Steer", cx);
    cx.update(|_, cx| assert_eq!(FollowUpBehavior::get(cx), FollowUpBehavior::Steer));
    // T3 keeps its own in its app; nothing goes to the server.
    assert_eq!(backend.updates(), []);
}

#[test]
fn patches_show_before_t3_answers() {
    let settings: ServerSettings = serde_json::from_value(settings_json()).unwrap();
    let off = patched(&settings, &json!({ "sidebarAutoSettleAfterDays": null })).unwrap();
    assert_eq!(auto_settle_days(&off), None);
    let model = patched(
        &settings,
        &json!({ "defaultModelSelection": { "instanceId": "codex", "model": "gpt-5.5" } }),
    )
    .unwrap();
    assert_eq!(
        model
            .default_model_selection
            .as_ref()
            .and_then(thread_chat::thread_settings::selection_model),
        Some("gpt-5.5")
    );
    assert_eq!(runtime_mode(&settings), RuntimeMode::FullAccess);
    // T3's defaults when it sends none.
    let empty: ServerSettings = serde_json::from_value(json!({})).unwrap();
    assert_eq!(branch_prefix(&empty), "t3");
    assert_eq!(branch_naming_mode(&empty), BranchNamingMode::Static);
}

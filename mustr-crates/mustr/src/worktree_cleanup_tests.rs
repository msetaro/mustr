use super::*;
use crate::thread_git::tests::{Call, RecordingBackend, init_test, shell};
use gpui::TestAppContext;
use pretty_assertions::assert_eq;
use time::macros::datetime;

fn settings(extra: Value) -> ServerSettings {
    let mut settings = json!({
        "storageCleanup": {
            "worktreeKeepWhen": "uncommitted-changes", "worktreeAfterDays": 8,
            "worktreeSettledAfterDays": null, "worktreeOnMerge": true, "worktreeOnDelete": false,
            "worktreeUnchanged": false, "browserArtifactsAfterDays": null, "logsAfterDays": null
        },
        "worktreeCleanup": null
    });
    settings
        .as_object_mut()
        .unwrap()
        .extend(extra.as_object().unwrap().clone());
    serde_json::from_value(settings).unwrap()
}

fn rules(value: Value) -> Value {
    let mut rules = json!({
        "worktreeKeepWhen": "uncommitted-changes", "worktreeAfterDays": null,
        "worktreeSettledAfterDays": null, "worktreeOnMerge": false, "worktreeOnDelete": false,
        "worktreeUnchanged": false
    });
    rules
        .as_object_mut()
        .unwrap()
        .extend(value.as_object().unwrap().clone());
    rules
}

#[test]
fn the_rules_are_the_servers_or_its_policys() {
    let server = CleanupRules::of(&settings(json!({})));
    assert_eq!(
        server,
        CleanupRules {
            keep_when: WorktreeKeepWhen::UncommittedChanges,
            after_days: Some(8),
            settled_after_days: None,
            on_merge: true,
            on_delete: false,
            unchanged: false,
        }
    );
    assert_eq!(
        server.summary(),
        "T3 removes worktrees: merged, inactive for 8 days. It keeps those with uncommitted \
         changes."
    );
    // A custom policy replaces them; an off one turns every rule off.
    let custom = CleanupRules::of(&settings(json!({
        "worktreeCleanup": { "mode": "custom", "rules": rules(json!({ "worktreeSettledAfterDays": 0, "worktreeKeepWhen": "any-local-files" })) }
    })));
    assert_eq!(custom.settled_after_days, Some(0));
    assert_eq!(custom.after_days, None);
    assert_eq!(custom.keep_when, WorktreeKeepWhen::AnyLocalFiles);
    let off = CleanupRules::of(&settings(json!({ "worktreeCleanup": { "mode": "off" } })));
    assert!(!off.any_on());
    assert_eq!(off.summary(), "Off: T3 removes no worktrees on its own");
}

#[test]
fn a_rule_patches_the_server_and_its_policy() {
    assert_eq!(
        rule_patch(&settings(json!({})), "worktreeOnDelete", json!(true)),
        json!({ "storageCleanup": { "worktreeOnDelete": true } })
    );
    // Turning a rule off sends null.
    assert_eq!(
        rule_patch(&settings(json!({})), "worktreeAfterDays", Value::Null),
        json!({ "storageCleanup": { "worktreeAfterDays": null } })
    );
    // With a custom policy, the policy changes too, in full.
    let custom = settings(json!({
        "worktreeCleanup": { "mode": "custom", "rules": rules(json!({ "worktreeOnMerge": true })) }
    }));
    assert_eq!(
        rule_patch(&custom, "worktreeUnchanged", json!(true)),
        json!({
            "storageCleanup": { "worktreeUnchanged": true },
            "worktreeCleanup": {
                "mode": "custom",
                "rules": rules(json!({ "worktreeOnMerge": true, "worktreeUnchanged": true }))
            }
        })
    );
}

fn report() -> Value {
    json!({
        "trigger": "manual", "startedAt": "2026-10-28T08:59:00.000Z",
        "finishedAt": "2026-10-28T09:00:00.000Z",
        "entries": [{
            "kind": "worktree", "outcome": "removed", "reason": "Its pull request merged",
            "path": "/tmp/worktrees/retry", "projectPath": "/src/api", "threadId": "thread-1",
            "threadTitle": "Add retries", "bytes": 3145728, "files": 120
        }],
        "counts": { "removed": 1, "kept": 4, "failed": 0 },
        "omittedCount": 0, "bytesFreed": 3145728
    })
}

#[test]
fn the_last_run_says_what_it_did() {
    let report: StorageCleanupReport = serde_json::from_value(report()).unwrap();
    assert_eq!(
        report_summary(&report, datetime!(2026-10-28 11:00 UTC)),
        "Last run 2 h ago: 1 removed, 4 kept. 3.0 MB recovered."
    );
}

#[gpui::test]
async fn rules_and_delete_now_go_through_t3(cx: &mut TestAppContext) {
    let backend = RecordingBackend::with_shell(shell(vec![]));
    let server_settings = json!({
        "storageCleanup": {
            "worktreeKeepWhen": "uncommitted-changes", "worktreeAfterDays": null,
            "worktreeSettledAfterDays": null, "worktreeOnMerge": false, "worktreeOnDelete": false,
            "worktreeUnchanged": false
        }
    });
    backend.respond("server.getSettings", Ok(server_settings.clone()));
    let mut changed = server_settings.clone();
    changed["storageCleanup"]["worktreeOnMerge"] = json!(true);
    backend.respond("server.updateSettings", Ok(changed));
    backend.respond("server.runStorageCleanup", Ok(report()));
    backend.script_stream("server.getStorageCleanupReport", vec![Value::Null], true);
    init_test(backend.clone(), cx);
    let (cleanup, cx) = cx.add_window_view(|_, cx| WorktreeCleanup::new(cx));
    cx.run_until_parked();
    cleanup.read_with(cx, |cleanup, _| {
        assert!(!cleanup.rules().unwrap().any_on());
        assert_eq!(cleanup.report(), None);
    });
    assert_eq!(
        backend.calls_to("server.getStorageCleanupReport")[0].access,
        Access::Read
    );

    // Refused in a dry run, before it's sent.
    backend
        .blocked
        .lock()
        .insert(Access::SettingsWrite, "mustr is in a dry run".to_string());
    cleanup.update(cx, |cleanup, cx| {
        cleanup.set_rule("worktreeOnMerge", json!(true), cx)
    });
    assert_eq!(backend.calls_to("server.updateSettings"), []);
    cleanup.read_with(cx, |cleanup, _| {
        assert_eq!(
            cleanup.error().map(|error| error.as_ref()),
            Some("mustr is in a dry run")
        )
    });
    backend.blocked.lock().clear();

    cleanup.update(cx, |cleanup, cx| {
        cleanup.set_rule("worktreeOnMerge", json!(true), cx)
    });
    cx.run_until_parked();
    assert_eq!(
        backend.calls_to("server.updateSettings"),
        [Call {
            tag: "server.updateSettings".to_string(),
            payload: json!({ "patch": { "storageCleanup": { "worktreeOnMerge": true } } }),
            access: Access::SettingsWrite,
        }]
    );
    cleanup.read_with(cx, |cleanup, _| assert!(cleanup.rules().unwrap().on_merge));

    cleanup.update_in(cx, |cleanup, window, cx| cleanup.run_now(window, cx));
    cx.run_until_parked();
    assert_eq!(
        backend.calls_to("server.runStorageCleanup"),
        [Call {
            tag: "server.runStorageCleanup".to_string(),
            payload: json!({}),
            access: Access::SettingsWrite,
        }]
    );
    cleanup.read_with(cx, |cleanup, _| {
        assert_eq!(
            cleanup.report().map(|report| report.counts.removed.0),
            Some(1.)
        )
    });

    // A later run reaches the card through T3's report.
    let mut later = report();
    later["counts"]["removed"] = json!(3);
    backend.push("server.getStorageCleanupReport", later);
    cx.run_until_parked();
    cleanup.read_with(cx, |cleanup, _| {
        assert_eq!(
            cleanup.report().map(|report| report.counts.removed.0),
            Some(3.)
        )
    });
}

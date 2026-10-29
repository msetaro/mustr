use super::*;
use crate::thread_git::tests::{RecordingBackend, init_test, shell, shell_thread};
use gpui::{TestAppContext, VisualTestContext};
use pretty_assertions::assert_eq;
use serde_json::{Value, json};

const T0: &str = "2026-10-28T09:00:00.000Z";
const T1: &str = "2026-10-28T09:05:00.000Z";
const T2: &str = "2026-10-28T09:10:00.000Z";
const T3: &str = "2026-10-28T09:15:00.000Z";

// Shell records, as T3 0.1.1 sends them.

/// A thread that never ran, with `state`'s fields over it.
fn thread(id: &str, title: &str, state: Value) -> Value {
    let mut thread = shell_thread(json!({
        "id": id, "title": title, "status": "idle", "createdAt": T0, "updatedAt": T0,
        "latestRunId": null, "activeRunId": null, "activityRunStatus": null,
        "latestRunCompletedAt": null,
        "lineage": { "rootThreadId": id, "parentThreadId": null, "relationshipToParent": null },
    }));
    thread
        .as_object_mut()
        .unwrap()
        .extend(state.as_object().unwrap().clone());
    thread
}

fn with(mut state: Value, more: Value) -> Value {
    state
        .as_object_mut()
        .unwrap()
        .extend(more.as_object().unwrap().clone());
    state
}

fn running(run: &str) -> Value {
    json!({
        "status": "running", "latestRunId": run, "activeRunId": run,
        "activityRunStatus": "running", "latestRunCompletedAt": null, "updatedAt": T1,
    })
}

fn completed(run: &str, at: &str) -> Value {
    json!({
        "status": "completed", "latestRunId": run, "activeRunId": null,
        "activityRunStatus": null, "latestRunCompletedAt": at, "updatedAt": at,
        "pendingRuntimeRequest": null,
    })
}

fn failed(run: &str, at: &str, class: &str) -> Value {
    json!({
        "status": "failed", "latestRunId": run, "activeRunId": null, "activityRunStatus": null,
        "latestRunCompletedAt": at, "updatedAt": at, "lastErrorClass": class,
        "lastError": "The agent stopped",
    })
}

/// The agent asks for something mid-run.
fn asking(run: &str, kind: &str, blocking: Option<bool>) -> Value {
    let mut request = json!({ "id": format!("{run}-{kind}"), "kind": kind, "createdAt": T1 });
    if let Some(blocking) = blocking {
        request["blocking"] = json!(blocking);
    }
    with(running(run), json!({ "pendingRuntimeRequest": request }))
}

fn subagent_of(parent: &str, creation_source: &str) -> Value {
    json!({
        "creationSource": creation_source,
        "lineage": { "rootThreadId": parent, "parentThreadId": parent, "relationshipToParent": "subagent" },
    })
}

fn snapshot(threads: Vec<Value>) -> Shell {
    Shell::from_snapshot(&serde_json::from_value(shell(threads)).unwrap())
}

fn update(shell: &mut Shell, thread: Value, location: &str) {
    let item = json!({
        "kind": "thread.updated", "location": location, "sequence": shell.sequence + 1,
        "thread": thread,
    });
    assert!(shell.apply(serde_json::from_value(item).unwrap()));
}

/// A shell and the transitions that follow it, from a first snapshot.
struct Feed {
    shell: Shell,
    transitions: Transitions,
}

impl Feed {
    fn new(threads: Vec<Value>) -> Self {
        let shell = snapshot(threads);
        let mut transitions = Transitions::default();
        assert_eq!(transitions.update(Some(&shell)), []);
        Self { shell, transitions }
    }

    fn update(&mut self, thread: Value) -> Vec<(String, Kind)> {
        update(&mut self.shell, thread, "active");
        self.events()
    }

    fn events(&mut self) -> Vec<(String, Kind)> {
        self.transitions
            .update(Some(&self.shell))
            .into_iter()
            .map(|event| (event.thread_id, event.kind))
            .collect()
    }
}

fn kinds(events: &[(&str, Kind)]) -> Vec<(String, Kind)> {
    events
        .iter()
        .map(|(id, kind)| (id.to_string(), *kind))
        .collect()
}

#[test]
fn a_run_that_completes_notifies_once() {
    let mut feed = Feed::new(vec![thread("a", "Add retries", json!({}))]);
    assert_eq!(
        feed.update(thread("a", "Add retries", running("run-1"))),
        []
    );
    update(
        &mut feed.shell,
        thread("a", "Add retries", completed("run-1", T2)),
        "active",
    );
    assert_eq!(
        feed.transitions.update(Some(&feed.shell)),
        [Event {
            thread_id: "a".to_string(),
            title: "Add retries".to_string(),
            kind: Kind::Completed,
        }]
    );
    // T3 sends the thread again when the user visits it, and when it settles: nothing new.
    let visited = with(completed("run-1", T2), json!({ "lastVisitedAt": T2 }));
    assert_eq!(feed.update(thread("a", "Add retries", visited)), []);
    assert_eq!(
        feed.update(thread(
            "a",
            "Add retries",
            with(
                completed("run-1", T2),
                json!({ "settledOverride": "settled" })
            )
        )),
        []
    );
    // The next run finishing does.
    assert_eq!(
        feed.update(thread("a", "Add retries", running("run-2"))),
        []
    );
    assert_eq!(
        feed.update(thread("a", "Add retries", completed("run-2", T3))),
        kinds(&[("a", Kind::Completed)])
    );
    // An idle runtime is "waiting" in T3's sidebar, not ready, so it isn't a completion.
    assert_eq!(
        feed.update(thread("a", "Add retries", running("run-3"))),
        []
    );
    let idle = with(
        completed("run-3", "2026-10-28T09:20:00.000Z"),
        json!({ "status": "idle" }),
    );
    assert_eq!(feed.update(thread("a", "Add retries", idle)), []);
}

#[test]
fn nothing_notifies_for_what_was_there_first() {
    // The first shell has threads that wait and threads that finished: none of them notify.
    let mut feed = Feed::new(vec![
        thread("a", "Waits", asking("run-1", "command", None)),
        thread("b", "Done", completed("run-1", T2)),
    ]);
    assert_eq!(feed.events(), []);
    // A thread first seen with a request doesn't either; it notifies from its next change.
    assert_eq!(
        feed.update(thread("c", "New", asking("run-1", "user_input", None))),
        []
    );
    assert_eq!(feed.update(thread("c", "New", running("run-1"))), []);
    assert_eq!(
        feed.update(thread("c", "New", asking("run-1", "user_input", None))),
        kinds(&[("c", Kind::NeedsInput)])
    );
    // While the shell isn't loaded every thread is forgotten, as T3 does while its shell isn't
    // live, so the shell coming back notifies nothing.
    assert_eq!(feed.transitions.update(None), []);
    update(
        &mut feed.shell,
        thread("b", "Done", running("run-2")),
        "active",
    );
    assert_eq!(feed.events(), []);
}

#[test]
fn requests_notify_once_for_each_run_and_status() {
    let mut feed = Feed::new(vec![thread("a", "Add retries", running("run-1"))]);
    let title = "Add retries";
    assert_eq!(
        feed.update(thread("a", title, asking("run-1", "command", None))),
        kinds(&[("a", Kind::NeedsApproval)])
    );
    // Another approval in the same run is the same attention (`runId:status` in T3).
    let another = with(
        running("run-1"),
        json!({ "pendingRuntimeRequest": { "id": "r2", "kind": "file-change", "createdAt": T2 } }),
    );
    assert_eq!(feed.update(thread("a", title, another)), []);
    assert_eq!(feed.update(thread("a", title, running("run-1"))), []);
    // A question, and a secret request, which T3 sends as a blocking `user_input` request.
    assert_eq!(
        feed.update(thread(
            "a",
            title,
            asking("run-1", "user_input", Some(true))
        )),
        kinds(&[("a", Kind::NeedsInput)])
    );
    // Back to an approval in the same run: the status changed, so it notifies.
    assert_eq!(
        feed.update(thread("a", title, asking("run-1", "permission", None))),
        kinds(&[("a", Kind::NeedsApproval)])
    );
    // A question the agent keeps working past doesn't wait on the user, and a sign-in refresh
    // isn't the user's to approve.
    assert_eq!(
        feed.update(thread(
            "a",
            title,
            asking("run-1", "user_input", Some(false))
        )),
        []
    );
    assert_eq!(
        feed.update(thread("a", title, asking("run-1", "auth_refresh", None))),
        []
    );
    // A new run asks again.
    assert_eq!(
        feed.update(thread("a", title, asking("run-2", "command", None))),
        kinds(&[("a", Kind::NeedsApproval)])
    );
}

#[test]
fn failures_and_usage_limits_notify() {
    let mut feed = Feed::new(vec![thread("a", "Add retries", running("run-1"))]);
    assert_eq!(
        feed.update(thread(
            "a",
            "Add retries",
            failed("run-1", T2, "provider_error")
        )),
        kinds(&[("a", Kind::Failed)])
    );
    assert_eq!(
        feed.update(thread("a", "Add retries", running("run-2"))),
        []
    );
    assert_eq!(
        feed.update(thread(
            "a",
            "Add retries",
            failed("run-2", T3, "usage_limit")
        )),
        kinds(&[("a", Kind::UsageLimit)])
    );
    // An interrupted run neither failed nor completed.
    assert_eq!(
        feed.update(thread("a", "Add retries", running("run-3"))),
        []
    );
    let interrupted = with(
        completed("run-3", "2026-10-28T09:20:00.000Z"),
        json!({ "status": "interrupted" }),
    );
    assert_eq!(feed.update(thread("a", "Add retries", interrupted)), []);
}

#[test]
fn subagents_notify_through_their_parents() {
    let child = |state: Value| {
        thread(
            "c",
            "Explore the codebase",
            with(subagent_of("p", "mcp"), state),
        )
    };
    let mut feed = Feed::new(vec![
        thread("p", "Plan the migration", running("run-1")),
        child(running("child-run-1")),
    ]);
    // The subagent's question is its parent's, under the parent's title.
    update(
        &mut feed.shell,
        child(asking("child-run-1", "user_input", None)),
        "active",
    );
    assert_eq!(
        feed.transitions.update(Some(&feed.shell)),
        [Event {
            thread_id: "p".to_string(),
            title: "Plan the migration".to_string(),
            kind: Kind::NeedsInput,
        }]
    );
    // Its approval wins over the question.
    assert_eq!(
        feed.update(child(asking("child-run-1", "command", None))),
        kinds(&[("p", Kind::NeedsApproval)])
    );
    // A subagent finishing tells the user nothing; its parent goes on.
    assert_eq!(feed.update(child(completed("child-run-1", T2))), []);
    // A request outliving the subagent's run doesn't hold the parent up.
    let ended = with(
        completed("child-run-1", T2),
        json!({ "pendingRuntimeRequest": { "id": "late", "kind": "user_input", "createdAt": T2 } }),
    );
    assert_eq!(feed.update(child(ended)), []);
    // Subagents native to the provider ask on their parent, which T3 shows on the parent itself.
    let native = |state: Value| {
        thread(
            "n",
            "Claude's own subagent",
            with(subagent_of("p", "provider"), state),
        )
    };
    assert_eq!(feed.update(native(running("native-run"))), []);
    assert_eq!(
        feed.update(native(asking("native-run", "command", None))),
        []
    );
    // Nested subagents roll up to the top.
    let grandchild = |state: Value| {
        let lineage = json!({
            "creationSource": "mcp",
            "lineage": { "rootThreadId": "p", "parentThreadId": "c", "relationshipToParent": "subagent" },
        });
        thread("g", "Read the schema", with(lineage, state))
    };
    assert_eq!(feed.update(child(running("child-run-2"))), []);
    assert_eq!(feed.update(grandchild(running("g-run"))), []);
    assert_eq!(
        feed.update(grandchild(asking("g-run", "user_input", None))),
        kinds(&[("p", Kind::NeedsInput)])
    );
    assert_eq!(
        feed.update(grandchild(asking("g-run", "command", None))),
        kinds(&[("p", Kind::NeedsApproval)])
    );
}

#[test]
fn archived_threads_dont_notify() {
    let mut feed = Feed::new(vec![thread("a", "Add retries", running("run-1"))]);
    update(
        &mut feed.shell,
        thread("a", "Add retries", completed("run-1", T2)),
        "archive",
    );
    assert_eq!(feed.events(), []);
    // A thread T3 still lists but stamps as archived doesn't either.
    let mut feed = Feed::new(vec![thread("b", "Old", running("run-1"))]);
    let archived = with(completed("run-1", T2), json!({ "archivedAt": T2 }));
    assert_eq!(feed.update(thread("b", "Old", archived)), []);
}

#[test]
fn the_count_has_the_threads_waiting_on_the_user() {
    let ask = |run: &str, kind: &str, at: &str| {
        with(
            running(run),
            json!({ "pendingRuntimeRequest": { "id": run, "kind": kind, "createdAt": at } }),
        )
    };
    let shell = snapshot(vec![
        thread("approval", "Needs approval", ask("r1", "command", T2)),
        thread("question", "Asks a question", ask("r2", "user_input", T1)),
        // Failed after the last visit: unseen.
        thread(
            "failed",
            "Failed",
            with(
                failed("r3", T2, "provider_error"),
                json!({ "lastVisitedAt": T1 }),
            ),
        ),
        // Seen since it failed.
        thread(
            "seen",
            "Failed and seen",
            with(
                failed("r4", T1, "provider_error"),
                json!({ "lastVisitedAt": T2 }),
            ),
        ),
        // Never opened, so never seen.
        thread("limited", "Out of credits", failed("r5", T1, "usage_limit")),
        // Settled by the user.
        thread(
            "settled",
            "Failed and settled",
            with(
                failed("r6", T3, "provider_error"),
                json!({ "settledOverride": "settled" }),
            ),
        ),
        thread("done", "Completed", completed("r7", T2)),
        thread("working", "Working", running("r8")),
        // A subagent counts once, through its parent.
        thread(
            "parent",
            "Parent",
            with(
                running("r9"),
                json!({ "updatedAt": "2026-10-28T09:07:00.000Z" }),
            ),
        ),
        thread(
            "child",
            "Child",
            with(subagent_of("parent", "mcp"), ask("r10", "command", T0)),
        ),
    ]);
    let waiting = waiting(&shell)
        .into_iter()
        .map(|waiting| (waiting.thread_id, waiting.status))
        .collect::<Vec<_>>();
    assert_eq!(
        waiting,
        [
            // Requests first, oldest first. The parent waits since its subagent asked, not
            // since its own record last changed.
            ("parent".to_string(), Status::Approval),
            ("question".to_string(), Status::Input),
            ("approval".to_string(), Status::Approval),
            // Then failures, oldest first.
            ("limited".to_string(), Status::Limited),
            ("failed".to_string(), Status::Failed),
        ]
    );
}

#[test]
fn settings_come_from_the_settings_file() {
    assert_eq!(
        NotificationSettings::from_settings_text("").unwrap(),
        NotificationSettings::default()
    );
    let defaults = NotificationSettings::default();
    assert!(defaults.completed && defaults.needs_approval && defaults.needs_input);
    assert!(!defaults.failed && !defaults.usage_limit && !defaults.sound);
    assert_eq!(
        NotificationSettings::from_settings_text(
            "{\n  // mustr settings\n  \"vim_mode\": true,\n}"
        )
        .unwrap(),
        defaults
    );
    // Fields left out keep their defaults; comments and trailing commas are fine.
    let settings = NotificationSettings::from_settings_text(
        r#"{
          // Tell me when runs fail, quietly, and not when they finish.
          "notifications": { "failed": true, "completed": false, },
        }"#,
    )
    .unwrap();
    assert_eq!(
        settings,
        NotificationSettings {
            completed: false,
            failed: true,
            ..NotificationSettings::default()
        }
    );
    assert!(settings.allows(Kind::Failed) && !settings.allows(Kind::Completed));
    assert!(!settings.allows(Kind::UsageLimit) && settings.allows(Kind::NeedsInput));
    assert!(
        NotificationSettings::from_settings_text(r#"{ "notifications": { "sound": "loud" } }"#)
            .is_err()
    );
}

// The notifier in a window.

async fn workspace(cx: &mut TestAppContext) -> (Entity<Workspace>, &mut VisualTestContext) {
    let fs = project::FakeFs::new(cx.executor());
    let project = project::Project::test(fs, [], cx).await;
    let (multi_workspace, cx) =
        cx.add_window_view(|window, cx| MultiWorkspace::test_new(project.clone(), window, cx));
    let workspace =
        multi_workspace.read_with(cx, |multi_workspace, _| multi_workspace.workspace().clone());
    (workspace, cx)
}

/// A window with a workspace on `threads`, and the app's notifier following them.
async fn notifier(
    threads: Vec<Value>,
    cx: &mut TestAppContext,
) -> (
    Entity<Notifier>,
    Entity<Workspace>,
    Shell,
    &mut VisualTestContext,
) {
    let backend = RecordingBackend::with_shell(shell(threads.clone()));
    init_test(backend, cx);
    cx.update(|cx| {
        // As `init` does: the OS shows notifications under the app's identity.
        cx.set_app_identity(APP_ID, "mustr");
        init_workspaces(cx);
    });
    let (workspace, cx) = workspace(cx).await;
    cx.run_until_parked();
    let notifier = cx.update(|_, cx| Notifier::try_global(cx).expect("the workspace starts it"));
    (notifier, workspace, snapshot(threads), cx)
}

fn play(notifier: &Entity<Notifier>, shell: &Shell, cx: &mut VisualTestContext) {
    notifier.update(cx, |notifier, cx| notifier.apply(Some(shell), cx));
    cx.run_until_parked();
}

fn open_thread_view(workspace: &Entity<Workspace>, thread_id: &str, cx: &mut VisualTestContext) {
    workspace.update_in(cx, |workspace, window, cx| {
        ThreadView::open(thread_id.to_string(), "".into(), workspace, window, cx)
    });
    cx.run_until_parked();
}

fn deliveries(
    notifier: &Entity<Notifier>,
    cx: &mut VisualTestContext,
) -> Vec<(String, Kind, Delivery)> {
    notifier.update(cx, |notifier, _| {
        notifier
            .deliveries
            .drain(..)
            .map(|(event, delivery)| (event.thread_id, event.kind, delivery))
            .collect()
    })
}

fn toasts(workspace: &Entity<Workspace>, cx: &mut VisualTestContext) -> Vec<Event> {
    workspace.read_with(cx, |workspace, cx| {
        workspace
            .notification_views()
            .into_iter()
            .filter_map(|view| view.downcast::<ThreadToast>().ok())
            .map(|toast| toast.read(cx).event().clone())
            .collect()
    })
}

fn active_thread(workspace: &Entity<Workspace>, cx: &mut VisualTestContext) -> Option<String> {
    workspace.read_with(cx, |workspace, cx| {
        let view = workspace.active_item(cx)?.downcast::<ThreadView>()?;
        view.read(cx).thread_id().map(str::to_string)
    })
}

#[gpui::test]
async fn focused_windows_toast_except_for_the_thread_on_screen(cx: &mut TestAppContext) {
    let (notifier, workspace, mut shell, cx) = notifier(
        vec![
            thread("a", "Add retries", running("run-1")),
            thread("b", "Bump tokio", running("run-1")),
        ],
        cx,
    )
    .await;
    assert!(cx.update(|window, _| window.is_window_active()));
    open_thread_view(&workspace, "a", cx);

    // The thread on screen finishing tells the user nothing more.
    update(
        &mut shell,
        thread("a", "Add retries", completed("run-1", T2)),
        "active",
    );
    play(&notifier, &shell, cx);
    assert_eq!(
        deliveries(&notifier, cx),
        [("a".to_string(), Kind::Completed, Delivery::Skipped)]
    );
    assert_eq!(toasts(&workspace, cx), []);

    // Another thread asking toasts in the focused window, and nothing goes to the OS.
    update(
        &mut shell,
        thread("b", "Bump tokio", asking("run-1", "command", None)),
        "active",
    );
    play(&notifier, &shell, cx);
    assert_eq!(
        deliveries(&notifier, cx),
        [("b".to_string(), Kind::NeedsApproval, Delivery::Toast)]
    );
    assert_eq!(
        toasts(&workspace, cx),
        [Event {
            thread_id: "b".to_string(),
            title: "Bump tokio".to_string(),
            kind: Kind::NeedsApproval,
        }]
    );
    assert_eq!(cx.shown_system_notifications().len(), 0);

    // Open shows the thread and closes the toast.
    let toast = workspace.read_with(cx, |workspace, _| {
        workspace.notification_views()[0]
            .clone()
            .downcast::<ThreadToast>()
            .unwrap()
    });
    toast.update_in(cx, |toast, window, cx| toast.open(window, cx));
    cx.run_until_parked();
    assert_eq!(active_thread(&workspace, cx), Some("b".to_string()));
    assert_eq!(toasts(&workspace, cx), []);
}

#[gpui::test]
async fn unfocused_windows_notify_through_the_os(cx: &mut TestAppContext) {
    let (notifier, workspace, mut shell, cx) = notifier(
        vec![
            thread("a", "Add retries", running("run-1")),
            thread("b", "Bump tokio", running("run-1")),
        ],
        cx,
    )
    .await;
    open_thread_view(&workspace, "a", cx);
    cx.deactivate_window();

    // With mustr in the background, even the thread on screen notifies, through the OS.
    update(
        &mut shell,
        thread("a", "Add retries", completed("run-1", T2)),
        "active",
    );
    update(
        &mut shell,
        thread("b", "Bump tokio", asking("run-1", "user_input", Some(true))),
        "active",
    );
    play(&notifier, &shell, cx);
    assert_eq!(
        deliveries(&notifier, cx),
        [
            ("a".to_string(), Kind::Completed, Delivery::System),
            ("b".to_string(), Kind::NeedsInput, Delivery::System),
        ]
    );
    assert_eq!(
        cx.shown_system_notifications(),
        [
            SystemNotification {
                tag: "mustr-thread:a".into(),
                title: "Thread completed".into(),
                body: "Add retries".into(),
                actions: Vec::new(),
            },
            SystemNotification {
                tag: "mustr-thread:b".into(),
                title: "Input needed".into(),
                body: "Bump tokio".into(),
                actions: Vec::new(),
            },
        ]
    );
    assert_eq!(toasts(&workspace, cx), []);

    // Clicking one brings mustr's window back and opens its thread.
    cx.simulate_system_notification_response(SystemNotificationResponse {
        tag: "mustr-thread:b".into(),
        action_id: None,
    });
    cx.run_until_parked();
    assert!(cx.update(|window, _| window.is_window_active()));
    assert_eq!(active_thread(&workspace, cx), Some("b".to_string()));
    // mustr having focus again clears its notifications, as T3 does.
    assert_eq!(
        cx.dismissed_system_notifications(),
        [
            SharedString::from("mustr-thread:a"),
            "mustr-thread:b".into()
        ]
    );
}

#[gpui::test]
async fn settings_choose_the_events_and_the_sound(cx: &mut TestAppContext) {
    let (notifier, _workspace, mut shell, cx) = notifier(
        vec![
            thread("a", "Add retries", running("run-1")),
            thread("b", "Bump tokio", running("run-1")),
        ],
        cx,
    )
    .await;
    // Failures are off by default.
    update(
        &mut shell,
        thread("a", "Add retries", failed("run-1", T2, "provider_error")),
        "active",
    );
    play(&notifier, &shell, cx);
    assert_eq!(deliveries(&notifier, cx), []);

    cx.update(|_, cx| {
        cx.set_global(NotificationSettings {
            failed: true,
            completed: false,
            sound: true,
            ..NotificationSettings::default()
        })
    });
    update(
        &mut shell,
        thread("a", "Add retries", running("run-2")),
        "active",
    );
    play(&notifier, &shell, cx);
    update(
        &mut shell,
        thread("a", "Add retries", failed("run-2", T3, "provider_error")),
        "active",
    );
    update(
        &mut shell,
        thread("b", "Bump tokio", completed("run-1", T3)),
        "active",
    );
    play(&notifier, &shell, cx);
    assert_eq!(
        deliveries(&notifier, cx),
        [("a".to_string(), Kind::Failed, Delivery::Toast)]
    );
    assert_eq!(
        notifier.read_with(cx, |notifier, _| notifier.sounds.clone()),
        [Kind::Failed]
    );
}

#[gpui::test]
async fn the_status_bar_counts_threads_waiting_and_opens_them(cx: &mut TestAppContext) {
    let (notifier, workspace, mut shell, cx) = notifier(
        vec![
            thread("a", "Add retries", asking("run-1", "command", None)),
            thread("b", "Bump tokio", running("run-1")),
        ],
        cx,
    )
    .await;
    // The snapshot's request counts from the start.
    let status = workspace.read_with(cx, |workspace, cx| {
        workspace
            .status_bar()
            .read(cx)
            .item_of_type::<AttentionStatus>()
            .expect("the workspace has the count")
    });
    let count = |cx: &mut VisualTestContext| {
        notifier.read_with(cx, |notifier, _| {
            (notifier.waiting().len(), notifier.badge())
        })
    };
    assert_eq!(count(cx), (1, 1));

    let failed_b = thread("b", "Bump tokio", failed("run-1", T2, "provider_error"));
    update(&mut shell, failed_b, "active");
    play(&notifier, &shell, cx);
    assert_eq!(count(cx), (2, 2));
    // The Dock's count can be turned off in the Settings page; the status bar still counts.
    let no_badge = NotificationSettings {
        dock_badge: false,
        ..NotificationSettings::default()
    };
    cx.update(|_, cx| apply_settings(no_badge, cx));
    assert_eq!(count(cx), (2, 0));
    cx.update(|_, cx| apply_settings(NotificationSettings::default(), cx));
    assert_eq!(count(cx), (2, 2));
    assert_eq!(AttentionStatus::label(1), "1 thread needs you");
    assert_eq!(AttentionStatus::label(2), "2 threads need you");

    // Clicking opens the request first, then moves on to the next thread.
    status.update_in(cx, |status, window, cx| status.open_next(window, cx));
    cx.run_until_parked();
    assert_eq!(active_thread(&workspace, cx), Some("a".to_string()));
    status.update_in(cx, |status, window, cx| status.open_next(window, cx));
    cx.run_until_parked();
    assert_eq!(active_thread(&workspace, cx), Some("b".to_string()));

    // Answered and seen, nothing waits.
    update(
        &mut shell,
        thread("a", "Add retries", running("run-1")),
        "active",
    );
    let seen = with(
        failed("run-1", T2, "provider_error"),
        json!({ "lastVisitedAt": T3 }),
    );
    update(&mut shell, thread("b", "Bump tokio", seen), "active");
    play(&notifier, &shell, cx);
    assert_eq!(count(cx), (0, 0));
}

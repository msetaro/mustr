use super::*;
use crate::thread_git::tests::{Call, RecordingBackend, init_test, shell};
use gpui::{Entity, TestAppContext, VisualTestContext};
use pretty_assertions::assert_eq;
use serde_json::{Value, json};
use workspace::MultiWorkspace;

fn reference() -> PullRequestRef {
    PullRequestRef {
        allow_stale: None,
        expected_account_id: None,
        host: Some("github.com".to_string()),
        number: 7,
        project_id: "project-1".to_string(),
        repository: "acme/hello".to_string(),
    }
}

fn stop(title: &str) -> Value {
    json!({
        "title": title, "summary": format!("What {title} does."),
        "attention": { "kind": "risk", "note": "Retries could double-send." },
        "excerpts": [{ "path": "src/send.rs", "side": "new", "start": 3, "end": 9, "caption": "The retry loop" }],
        "files": [{ "path": "src/retry.rs", "note": "The backoff" }],
        "notes": [], "questions": ["Is the cap right?"]
    })
}

fn result(guide: Option<Value>, run: Value, head: &str) -> Value {
    json!({ "guide": guide, "run": run, "currentHeadSha": head })
}

fn guide(head: &str, stops: Vec<Value>) -> Value {
    json!({
        "headSha": head, "generatedAt": "2026-10-28T09:00:00.000Z",
        "modelSelection": { "instanceId": "codex", "model": "gpt-5.5" }, "stops": stops
    })
}

fn state(value: Value) -> GuideState {
    GuideState::Loaded(serde_json::from_value(value).unwrap())
}

#[test]
fn the_chip_says_where_the_guide_is() {
    let chip = |value| state(value).chip().map(|(label, _)| label);
    assert_eq!(
        chip(result(
            Some(guide("abc", vec![stop("One"), stop("Two")])),
            Value::Null,
            "abc"
        )),
        Some("Guide · 2 stops".to_string())
    );
    assert_eq!(
        chip(result(
            Some(guide("abc", vec![stop("One")])),
            Value::Null,
            "def"
        )),
        Some("Guide · 1 stop · outdated".to_string())
    );
    assert_eq!(
        chip(result(
            None,
            json!({ "_tag": "Running", "headSha": "abc", "startedAt": "2026-10-28T09:00:00.000Z" }),
            "abc"
        )),
        Some("Writing guide…".to_string())
    );
    let failed = state(result(
        None,
        json!({ "_tag": "Failed", "headSha": "abc", "failedAt": "2026-10-28T09:00:00.000Z", "detail": "Review guides are off." }),
        "abc",
    ));
    assert_eq!(failed.failure(), Some("Review guides are off."));
    assert_eq!(
        failed.chip(),
        Some((
            "Guide".to_string(),
            "Writing the guide failed: Review guides are off.".to_string()
        ))
    );
    assert_eq!(GuideState::Unknown.chip(), None);
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
async fn writing_a_guide_asks_t3_then_follows_it(cx: &mut TestAppContext) {
    let backend = RecordingBackend::with_shell(shell(vec![]));
    let running =
        json!({ "_tag": "Running", "headSha": "abc", "startedAt": "2026-10-28T09:00:00.000Z" });
    backend.respond(
        "pullRequests.generateReviewGuide",
        Ok(result(None, running, "abc")),
    );
    backend.respond(
        "pullRequests.reviewGuide",
        Ok(result(
            Some(guide("abc", vec![stop("One"), stop("Two")])),
            Value::Null,
            "abc",
        )),
    );
    init_test(backend.clone(), cx);
    let (workspace, cx) = workspace(cx).await;
    workspace.update_in(cx, |workspace, window, cx| {
        open(
            reference(),
            state(result(None, Value::Null, "abc")),
            WeakEntity::new_invalid(),
            |_, _| {},
            workspace,
            window,
            cx,
        )
    });
    let dialog = workspace
        .read_with(cx, |workspace, cx| {
            workspace.active_modal::<ReviewGuideDialog>(cx)
        })
        .expect("the guide is open");

    // Refused in a dry run, before it's sent.
    backend
        .blocked
        .lock()
        .insert(Access::Operate, "mustr is in a dry run".to_string());
    dialog.update(cx, |dialog, cx| dialog.write(cx));
    cx.run_until_parked();
    assert_eq!(backend.calls_to("pullRequests.generateReviewGuide"), []);
    backend.blocked.lock().clear();

    dialog.update(cx, |dialog, cx| dialog.write(cx));
    cx.run_until_parked();
    let payload = json!({
        "projectId": "project-1", "host": "github.com", "repository": "acme/hello", "number": 7
    });
    assert_eq!(
        backend.calls_to("pullRequests.generateReviewGuide"),
        [Call {
            tag: "pullRequests.generateReviewGuide".to_string(),
            payload: payload.clone(),
            access: Access::Operate,
        }]
    );
    dialog.read_with(cx, |dialog, _| assert!(dialog.state.is_running()));

    // Five seconds on, mustr asks again, and the guide is there.
    cx.executor().advance_clock(POLL_INTERVAL);
    cx.run_until_parked();
    assert_eq!(
        backend.calls_to("pullRequests.reviewGuide"),
        [Call {
            tag: "pullRequests.reviewGuide".to_string(),
            payload,
            access: Access::Read,
        }]
    );
    dialog.update_in(cx, |dialog, window, cx| {
        assert_eq!(dialog.stops().len(), 2);
        dialog.next_stop(&NextStop, window, cx);
        dialog.next_stop(&NextStop, window, cx);
        assert_eq!(dialog.stop(), 1);
        dialog.previous_stop(&PreviousStop, window, cx);
        assert_eq!(dialog.stop(), 0);
    });
}

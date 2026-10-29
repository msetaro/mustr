use super::*;
use crate::thread_git::tests::{Call, RecordingBackend, init_test, shell};
use gpui::{TestAppContext, VisualTestContext};
use pretty_assertions::assert_eq;
use serde_json::json;
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

fn detail(extra: Value) -> Value {
    let mut detail = json!({
        "title": "Greet the whole world",
        "author": { "login": "octocat" },
        "viewer": "octocat",
        "reviewers": [{ "login": "hubot" }],
        "labels": [{ "name": "feature", "color": "0e8a16" }],
        "capabilities": {
            "reviewers": { "request": true, "listCandidates": true },
            "edit": { "changeRequest": true, "comment": true },
            "labels": true, "stacks": true
        },
        "viewerPermissions": { "actions": [], "requestReviewers": true }
    });
    detail
        .as_object_mut()
        .unwrap()
        .extend(extra.as_object().unwrap().clone());
    detail
}

#[test]
fn the_details_say_what_t3_offers() {
    let metadata = PullRequestMetadata::parse(&detail(json!({}))).unwrap();
    assert_eq!(
        metadata,
        PullRequestMetadata {
            title: "Greet the whole world".to_string(),
            reviewers: vec!["hubot".to_string()],
            labels: vec![PullRequestLabel {
                name: "feature".to_string(),
                color: Some("0e8a16".to_string())
            }],
            reviewer_picker: Offer::Allowed,
            label_picker: Offer::Allowed,
            title_editable: true,
            stacks: true,
        }
    );
    // The viewer may not request reviewers, a forge without labels or edits, someone else's
    // pull request without merge rights.
    let metadata = PullRequestMetadata::parse(&detail(json!({
        "viewer": "someone",
        "capabilities": { "reviewers": { "request": true, "listCandidates": true } },
        "viewerPermissions": { "actions": [], "requestReviewers": false }
    })))
    .unwrap();
    assert_eq!(metadata.reviewer_picker, Offer::Refused);
    assert_eq!(metadata.label_picker, Offer::Hidden);
    assert!(!metadata.title_editable && !metadata.stacks);
    // Azure DevOps requests reviewers but can't list them: no picker.
    let metadata = PullRequestMetadata::parse(&detail(json!({
        "capabilities": { "reviewers": { "request": true, "listCandidates": false } }
    })))
    .unwrap();
    assert_eq!(metadata.reviewer_picker, Offer::Hidden);
    // Merge rights let someone else's pull request be retitled; the forge's word wins.
    let can_edit = |viewer: Value| {
        PullRequestMetadata::parse(&detail(
            json!({ "viewer": "someone", "viewerPermissions": viewer }),
        ))
        .unwrap()
        .title_editable
    };
    assert!(can_edit(
        json!({ "actions": ["merge"], "requestReviewers": true })
    ));
    assert!(!can_edit(
        json!({ "actions": ["merge"], "requestReviewers": true, "editChangeRequest": false })
    ));
}

#[test]
fn writes_send_t3s_payloads() {
    assert_eq!(
        serde_json::to_value(request_reviewers_input(
            &reference(),
            "U_1",
            PullRequestReviewerKind::User,
            true
        ))
        .unwrap(),
        json!({
            "projectId": "project-1", "host": "github.com", "repository": "acme/hello",
            "number": 7, "reviewers": [{ "id": "U_1", "kind": "user" }], "requested": true
        })
    );
    assert_eq!(
        serde_json::to_value(set_labels_input(&reference(), "bug", false)).unwrap(),
        json!({
            "projectId": "project-1", "host": "github.com", "repository": "acme/hello",
            "number": 7, "labels": ["bug"], "applied": false
        })
    );
    assert_eq!(
        serde_json::to_value(update_title_input(&reference(), "Greet everyone")).unwrap(),
        json!({
            "projectId": "project-1", "host": "github.com", "repository": "acme/hello",
            "number": 7, "title": "Greet everyone"
        })
    );
    assert_eq!(
        title_to_save("  Greet everyone ", "Greet"),
        Some("Greet everyone".to_string())
    );
    assert_eq!(title_to_save("Greet ", "Greet"), None);
    assert_eq!(title_to_save("   ", "Greet"), None);
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

fn stack() -> Value {
    json!({
        "id": "S_1", "number": 3, "url": "https://github.com/acme/hello/stacks/3", "base": "main",
        "layers": [
            { "number": 6, "title": "Add the greeter", "headBranch": "greeter", "state": "merged" },
            { "number": 7, "title": "Greet the whole world", "headBranch": "greet-world", "state": "open" },
            { "number": 8, "headBranch": "greet-mars", "state": "open", "isDraft": true }
        ]
    })
}

#[gpui::test]
async fn reviewers_and_labels_change_one_pick_at_a_time(cx: &mut TestAppContext) {
    let backend = RecordingBackend::with_shell(shell(vec![]));
    backend.respond("pullRequests.stack", Ok(stack()));
    backend.respond(
        "pullRequests.reviewGuide",
        Ok(json!({ "guide": null, "run": null, "currentHeadSha": "abc" })),
    );
    backend.respond(
        "pullRequests.reviewerCandidates",
        Ok(json!({
            "candidates": [
                { "id": "U_1", "login": "hubot", "name": "Hubot", "avatarUrl": null, "kind": "user", "isRequested": true },
                { "id": "U_2", "login": "monalisa", "name": "Mona", "avatarUrl": null, "kind": "user", "isRequested": false },
                { "id": "T_1", "login": "acme/reviewers", "name": null, "avatarUrl": null, "kind": "team", "isRequested": false }
            ],
            "truncated": false
        })),
    );
    backend.respond(
        "pullRequests.labelCandidates",
        Ok(json!({
            "candidates": [
                { "name": "bug", "color": "d73a4a", "description": "Something isn't working", "isApplied": false },
                { "name": "feature", "color": "0e8a16", "description": null, "isApplied": true }
            ],
            "truncated": false
        })),
    );
    init_test(backend.clone(), cx);
    // The pickers' query editor.
    cx.update(editor::init);
    let (workspace, cx) = workspace(cx).await;
    let metadata = PullRequestMetadata::parse(&detail(json!({}))).unwrap();
    let details = cx.new(|cx| {
        PullRequestDetails::new(
            reference(),
            metadata,
            WeakEntity::new_invalid(),
            workspace.downgrade(),
            cx,
        )
    });
    cx.run_until_parked();
    // The stack loads with the details.
    assert_eq!(
        backend.calls_to("pullRequests.stack"),
        [Call {
            tag: "pullRequests.stack".to_string(),
            payload: json!({
                "projectId": "project-1", "host": "github.com", "repository": "acme/hello", "number": 7
            }),
            access: Access::Read,
        }]
    );
    details.read_with(cx, |details, _| {
        assert_eq!(details.stack().map(|stack| stack.layers.len()), Some(3));
        // And whether it has a review guide.
        assert_eq!(
            details.guide().chip(),
            Some(("Guide".to_string(), "No review guide yet".to_string()))
        );
    });
    assert_eq!(
        backend.calls_to("pullRequests.reviewGuide")[0].access,
        Access::Read
    );

    // Asking Mona to review.
    details.update_in(cx, |details, window, cx| {
        details.open_picker(CandidateKind::Reviewers, window, cx)
    });
    cx.run_until_parked();
    let picker = workspace
        .read_with(cx, |workspace, cx| {
            workspace.active_modal::<Picker<CandidatePickerDelegate>>(cx)
        })
        .expect("the reviewer picker is open");
    picker.update(cx, |picker, cx| {
        assert_eq!(picker.delegate.candidates().unwrap().len(), 3);
        picker.delegate.toggle(1, cx);
    });
    cx.run_until_parked();
    assert_eq!(
        backend.calls_to("pullRequests.requestReviewers"),
        [Call {
            tag: "pullRequests.requestReviewers".to_string(),
            payload: json!({
                "projectId": "project-1", "host": "github.com", "repository": "acme/hello",
                "number": 7, "reviewers": [{ "id": "U_2", "kind": "user" }], "requested": true
            }),
            access: Access::SourceControlWrite,
        }]
    );
    // Un-asking Hubot.
    picker.update(cx, |picker, cx| picker.delegate.toggle(0, cx));
    cx.run_until_parked();
    assert_eq!(
        backend.calls_to("pullRequests.requestReviewers")[1].payload["requested"],
        json!(false)
    );
    details.read_with(cx, |details, _| {
        assert_eq!(details.metadata().reviewers, ["monalisa"])
    });
    // The picker stays open for the next pick.
    assert!(
        workspace
            .read_with(cx, |workspace, cx| workspace
                .active_modal::<Picker<CandidatePickerDelegate>>(cx))
            .is_some()
    );
    workspace.update_in(cx, |workspace, window, cx| workspace.hide_modal(window, cx));

    // Adding a label, through a dry run that refuses it first.
    backend.blocked.lock().insert(
        Access::SourceControlWrite,
        "mustr is in a dry run, so it doesn't write to pull requests".to_string(),
    );
    details.update_in(cx, |details, window, cx| {
        details.open_picker(CandidateKind::Labels, window, cx)
    });
    cx.run_until_parked();
    let picker = workspace
        .read_with(cx, |workspace, cx| {
            workspace.active_modal::<Picker<CandidatePickerDelegate>>(cx)
        })
        .expect("the label picker is open");
    picker.update(cx, |picker, cx| picker.delegate.toggle(0, cx));
    cx.run_until_parked();
    assert_eq!(backend.calls_to("pullRequests.setLabels"), []);
    picker.read_with(cx, |picker, _| {
        assert!(picker.delegate.status().unwrap().contains("dry run"))
    });
    backend.blocked.lock().clear();
    picker.update(cx, |picker, cx| picker.delegate.toggle(0, cx));
    cx.run_until_parked();
    assert_eq!(
        backend.calls_to("pullRequests.setLabels"),
        [Call {
            tag: "pullRequests.setLabels".to_string(),
            payload: json!({
                "projectId": "project-1", "host": "github.com", "repository": "acme/hello",
                "number": 7, "labels": ["bug"], "applied": true
            }),
            access: Access::SourceControlWrite,
        }]
    );
    details.read_with(cx, |details, _| {
        assert_eq!(
            details
                .metadata()
                .labels
                .iter()
                .map(|label| label.name.as_str())
                .collect::<Vec<_>>(),
            ["feature", "bug"]
        )
    });
}

#[gpui::test]
async fn the_title_saves_through_t3(cx: &mut TestAppContext) {
    let backend = RecordingBackend::with_shell(shell(vec![]));
    init_test(backend.clone(), cx);
    let (workspace, cx) = workspace(cx).await;
    let metadata = PullRequestMetadata::parse(&detail(json!({ "capabilities": {} }))).unwrap();
    let details = cx.new(|cx| {
        PullRequestDetails::new(
            reference(),
            metadata,
            WeakEntity::new_invalid(),
            workspace.downgrade(),
            cx,
        )
    });
    details.update_in(cx, |details, window, cx| {
        details.open_title_editor(window, cx)
    });
    let dialog = workspace
        .read_with(cx, |workspace, cx| {
            workspace.active_modal::<EditTitleDialog>(cx)
        })
        .expect("the title editor is open");

    // Unchanged, it closes without a call.
    dialog.update(cx, |dialog, cx| dialog.save(cx));
    cx.run_until_parked();
    assert_eq!(backend.calls_to("pullRequests.update"), []);

    details.update_in(cx, |details, window, cx| {
        details.open_title_editor(window, cx)
    });
    let dialog = workspace
        .read_with(cx, |workspace, cx| {
            workspace.active_modal::<EditTitleDialog>(cx)
        })
        .unwrap();
    dialog.update_in(cx, |dialog, window, cx| {
        dialog.editor().update(cx, |editor, cx| {
            editor.set_text(" Greet everyone ", window, cx)
        });
        dialog.save(cx);
    });
    cx.run_until_parked();
    assert_eq!(
        backend.calls_to("pullRequests.update"),
        [Call {
            tag: "pullRequests.update".to_string(),
            payload: json!({
                "projectId": "project-1", "host": "github.com", "repository": "acme/hello",
                "number": 7, "title": "Greet everyone"
            }),
            access: Access::SourceControlWrite,
        }]
    );
    details.read_with(cx, |details, _| {
        assert_eq!(details.metadata().title, "Greet everyone")
    });
    assert!(
        workspace
            .read_with(cx, |workspace, cx| workspace
                .active_modal::<EditTitleDialog>(cx))
            .is_none()
    );
}

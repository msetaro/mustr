//! Reviewing an agent thread's work: local comments sent to the agent, real files from the
//! thread's worktree, and the scope picker.

use super::tests::{
    Call, FakeBackend, RUST_HEAD, buffer_for_path, build_review_view, build_review_view_with_fs,
    drafts, rel_path, review_files, rhs_editor, select_lines,
};
use super::*;
use gpui::{TestAppContext, VisualTestContext};
use parking_lot::Mutex;
use pretty_assertions::assert_eq;
use project::{FakeFs, Fs as _};
use serde_json::json;
use settings::DiffViewStyle;
use std::path::Path;

fn agent_header(scope: ReviewScope) -> ReviewHeader {
    ReviewHeader::AgentThread(AgentThreadHeader {
        thread_id: "thread-1".to_string(),
        title: "Greet the whole world".to_string(),
        project: Some("hello".to_string()),
        branch: Some("t3/greet-world".to_string()),
        agent: Some("Claude · claude-opus-5-5".to_string()),
        status: "completed".to_string(),
        status_tone: AgentStatusTone::Settled,
        scopes: vec![
            ScopeChoice {
                scope: ReviewScope::Changes,
                detail: None,
            },
            ScopeChoice {
                scope: ReviewScope::Turn(1),
                detail: Some("2 files".to_string()),
            },
        ],
        scope,
        source: None,
    })
}

/// What a review of an agent's work may do: comment anywhere, locally.
fn local_permissions() -> ReviewPermissions {
    ReviewPermissions {
        line_comments: true,
        ..ReviewPermissions::default()
    }
}

fn agent_contents(backend: &Arc<FakeBackend>) -> ReviewContents {
    ReviewContents {
        header: Some(agent_header(ReviewScope::Changes)),
        files: review_files(),
        backend: Some(backend.clone()),
        permissions: local_permissions(),
        ..ReviewContents::default()
    }
}

fn write_comment(
    review_view: &Entity<ReviewView>,
    first: &str,
    last: &str,
    body: &str,
    cx: &mut VisualTestContext,
) {
    select_lines(review_view, first, last, cx);
    cx.simulate_keystrokes("c");
    cx.simulate_input(body);
    cx.simulate_keystrokes("secondary-enter");
}

#[gpui::test]
async fn test_comments_on_an_agents_diff_are_sent_to_the_agent(cx: &mut TestAppContext) {
    let backend = Arc::new(FakeBackend::default());
    let (review_view, _, cx) =
        build_review_view(agent_contents(&backend), DiffViewStyle::Unified, cx).await;
    review_view.read_with(cx, |review_view, cx| {
        assert!(review_view.sends_to_agent());
        assert_eq!(review_view.tab_content_text(0, cx), "Greet the whole world");
    });

    // Any line takes a comment: there's no forge deciding which.
    write_comment(
        &review_view,
        "    println!(\"hello, world\");",
        "    println!(\"hello, world\");",
        "Use the logger.",
        cx,
    );
    write_comment(&review_view, "fn main() {", "}", "Rename main.", cx);
    let written = drafts(&review_view, cx);
    assert_eq!(written.len(), 2);
    assert!(written.iter().all(|draft| draft.added));
    assert!(backend.writes().is_empty(), "nothing is sent yet");

    // Once sent, the comments come back as local threads marked sent.
    *backend.threads.lock() = vec![ReviewThread {
        id: "sent-1".to_string(),
        path: "src/main.rs".to_string(),
        line: Some(2),
        side: DiffSide::Right,
        is_resolved: false,
        is_outdated: false,
        can_resolve: false,
        comments: vec![ThreadComment {
            id: "sent-1".to_string(),
            author: Some("You".to_string()),
            body: "Use the logger.".to_string(),
            created_at: None,
            edited: false,
            can_edit: false,
        }],
        comment_count: 1,
        sent_to_agent: true,
    }];
    review_view.update_in(cx, |review_view, window, cx| {
        review_view.open_submit_form(&SubmitReview, window, cx)
    });
    cx.simulate_input("Thanks!");
    cx.simulate_keystrokes("secondary-enter");
    cx.run_until_parked();

    let quote = |text: &str| QuotedCode {
        language: Some("rust".to_string()),
        text: text.to_string(),
    };
    assert_eq!(
        backend.writes(),
        vec![Call::Submit(ReviewSubmission {
            verdict: ReviewVerdict::Comment,
            body: "Thanks!".to_string(),
            comments: written.clone(),
            quotes: [
                (
                    written[0].id.clone(),
                    quote("    println!(\"hello, world\");")
                ),
                (
                    written[1].id.clone(),
                    quote("fn main() {\n    println!(\"hello, world\");\n}"),
                ),
            ]
            .into_iter()
            .collect(),
        })]
    );
    review_view.read_with(cx, |review_view, cx| {
        assert!(
            drafts_of(review_view, cx).is_empty(),
            "sent drafts are gone"
        );
        assert!(!review_view.is_submit_form_open());
        assert_eq!(
            review_view.notice().map(|notice| notice.to_string()),
            Some("Sent to the agent. Its reply shows in the thread.".to_string())
        );
        assert!(
            review_view.thread_block("sent-1").is_some(),
            "the sent comment shows as a local thread"
        );
    });
    assert!(backend.last_saved().comments.is_empty());
}

fn drafts_of(review_view: &ReviewView, cx: &App) -> Vec<DraftComment> {
    review_view
        .draft_cards()
        .map(|card| card.read(cx).draft(cx))
        .collect()
}

#[gpui::test]
async fn test_a_failed_send_keeps_every_comment(cx: &mut TestAppContext) {
    let backend = Arc::new(FakeBackend::default());
    let (review_view, _, cx) =
        build_review_view(agent_contents(&backend), DiffViewStyle::Unified, cx).await;
    write_comment(
        &review_view,
        "    println!(\"hello, world\");",
        "    println!(\"hello, world\");",
        "Use the logger.",
        cx,
    );
    *backend.fail_with.lock() = Some("Not sent: MUSTR_DRY_RUN_WRITES is set.".to_string());
    review_view.update_in(cx, |review_view, window, cx| {
        review_view.open_submit_form(&SubmitReview, window, cx)
    });
    cx.simulate_keystrokes("secondary-enter");
    cx.run_until_parked();
    review_view.read_with(cx, |review_view, cx| {
        assert_eq!(
            review_view.submit_error().map(|error| error.to_string()),
            Some("Not sent: MUSTR_DRY_RUN_WRITES is set.".to_string())
        );
        assert_eq!(review_view.added_draft_count(cx), 1);
    });
}

#[gpui::test]
async fn test_picking_a_scope_asks_the_app_to_load_it(cx: &mut TestAppContext) {
    let backend = Arc::new(FakeBackend::default());
    let (review_view, _, cx) =
        build_review_view(agent_contents(&backend), DiffViewStyle::Unified, cx).await;
    let events = Arc::new(Mutex::new(Vec::new()));
    cx.update(|_, cx| {
        let events = events.clone();
        cx.subscribe(&review_view, move |_, event: &ReviewViewEvent, _| {
            events.lock().push(event.clone())
        })
        .detach();
    });

    review_view.update(cx, |review_view, cx| {
        review_view.select_scope(ReviewScope::Changes, cx)
    });
    cx.run_until_parked();
    assert!(events.lock().is_empty(), "the scope already shown");

    review_view.update(cx, |review_view, cx| {
        review_view.select_scope(ReviewScope::Turn(1), cx)
    });
    cx.run_until_parked();
    assert_eq!(
        *events.lock(),
        vec![ReviewViewEvent::SelectScope(ReviewScope::Turn(1))]
    );
    review_view.read_with(cx, |review_view, _| {
        assert_eq!(
            review_view.notice().map(|notice| notice.to_string()),
            Some("Loading Turn 1…".to_string())
        );
    });
    assert_eq!(
        agent_header(ReviewScope::Turn(3)).tab_title(),
        "Greet the whole world · Turn 3"
    );
}

#[gpui::test]
async fn test_a_local_worktree_shows_its_real_files(cx: &mut TestAppContext) {
    let fs = FakeFs::new(cx.executor());
    fs.insert_tree(
        "/worktree",
        json!({ "src": { "main.rs": RUST_HEAD }, "docs": { "README.md": "# Title\n\nIntro.\n\nA new paragraph.\n" } }),
    )
    .await;
    let backend = Arc::new(FakeBackend::default());
    let contents = ReviewContents {
        worktree: Some(PathBuf::from("/worktree")),
        // A file whose real file is gone falls back to the text it was given.
        files: review_files()
            .into_iter()
            .chain([ReviewFileContents {
                path: rel_path("gone.rs"),
                old_path: None,
                base_text: None,
                head_text: Some("// written, then removed\n".to_string()),
                commentable: None,
            }])
            .collect(),
        ..agent_contents(&backend)
    };
    let (review_view, workspace, cx) =
        build_review_view_with_fs(contents, DiffViewStyle::Unified, fs.clone(), cx).await;

    workspace.read_with(cx, |workspace, cx| {
        let worktrees = workspace.project().read(cx).visible_worktrees(cx).count();
        assert_eq!(
            worktrees, 1,
            "the worktree is visible, so Restricted Mode applies"
        );
    });
    let main_rs = buffer_for_path(&review_view, "src/main.rs", cx);
    main_rs.read_with(cx, |buffer, _| {
        assert!(
            buffer.file().is_some_and(|file| file.as_local().is_some()),
            "src/main.rs is the real file, so language servers can attach"
        );
        assert_eq!(buffer.text(), RUST_HEAD);
    });
    review_view.read_with(cx, |review_view, _| {
        let tree = review_view.file_tree();
        let on_disk = tree
            .files()
            .iter()
            .enumerate()
            .map(|(index, file)| (file.path.clone(), review_view.is_file_on_disk(index)))
            .collect::<Vec<_>>();
        assert_eq!(
            on_disk,
            vec![
                ("src/main.rs".to_string(), true),
                ("docs/README.md".to_string(), true),
                ("gone.rs".to_string(), false),
            ]
        );
    });
    let diff_text = |cx: &mut VisualTestContext| {
        rhs_editor(&review_view, cx).update(cx, |editor, cx| editor.display_text(cx))
    };
    assert!(
        diff_text(cx).contains("println!(\"hello\");"),
        "the base line shows"
    );

    // The agent keeps working: the review follows the file.
    let changed = format!("{RUST_HEAD}\nfn goodbye() {{}}\n");
    fs.insert_file("/worktree/src/main.rs", changed.clone().into_bytes())
        .await;
    cx.run_until_parked();
    cx.executor().advance_clock(RECALCULATE_DIFF_DEBOUNCE * 2);
    cx.run_until_parked();
    assert!(
        diff_text(cx).contains("fn goodbye() {}"),
        "the new hunk shows:\n{}",
        diff_text(cx)
    );

    // Reviewing writes nothing: no save, no formatting.
    main_rs.read_with(cx, |buffer, _| assert!(!buffer.is_dirty()));
    assert_eq!(
        fs.load(Path::new("/worktree/src/main.rs"))
            .await
            .expect("the file is there"),
        changed
    );
}

/// A scope without changes opens as an empty review that says so, as T3's diff panel shows an
/// empty diff, rather than as a failure.
#[gpui::test]
async fn test_a_scope_without_changes_says_so(cx: &mut TestAppContext) {
    let backend = Arc::new(FakeBackend::default());
    let contents = ReviewContents {
        header: Some(agent_header(ReviewScope::Uncommitted)),
        files: Vec::new(),
        ..agent_contents(&backend)
    };
    let (review_view, _, cx) = build_review_view(contents, DiffViewStyle::Unified, cx).await;
    review_view.read_with(cx, |review_view, _| {
        assert_eq!(
            review_view.empty_label().as_deref(),
            Some("No uncommitted changes")
        );
        assert!(review_view.notice().is_none(), "it's not an error");
    });
    assert_eq!(
        ReviewScope::Changes.empty_label(),
        "No changes on this branch"
    );
    assert_eq!(
        ReviewScope::Turn(3).empty_label(),
        "Turn 3 changed no files"
    );
}

#[gpui::test]
async fn test_a_scope_with_changes_shows_them(cx: &mut TestAppContext) {
    let backend = Arc::new(FakeBackend::default());
    let (review_view, _, cx) =
        build_review_view(agent_contents(&backend), DiffViewStyle::Unified, cx).await;
    review_view.read_with(cx, |review_view, _| {
        assert_eq!(review_view.empty_label(), None);
    });
}

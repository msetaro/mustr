//! The terminal and review-comment chips other parts of mustr add to the composer
//! (`AddComposerContext`), against the recording backend: every payload is asserted as T3 gets
//! it.

use super::*;
use crate::actions::{AddComposerContext, ComposerContext, ReviewCommentContext, TerminalContext};
use gpui::Focusable as _;
use pretty_assertions::assert_eq;
use workspace::{MultiWorkspace, Workspace};

// Context chips.

pub(super) fn terminal_context(id: &str) -> ComposerContext {
    ComposerContext::Terminal(TerminalContext {
        context_id: format!("terminal_{id}"),
        label: "Terminal 1 lines 3-4".into(),
        terminal_id: "term-1".into(),
        terminal_label: "Terminal 1".into(),
        line_start: 3,
        line_end: 4,
        text: "$ npm test\nFAIL src/webhooks/dispatcher.test.ts".into(),
    })
}

fn review_comment_context() -> ComposerContext {
    ComposerContext::ReviewComment(ReviewCommentContext {
        context_id: "review-comment_c1".into(),
        label: "dispatcher.ts L40".into(),
        section_id: "changes".into(),
        section_title: "Changes".into(),
        file_path: "src/webhooks/dispatcher.ts".into(),
        start_index: 39,
        end_index: 39,
        range_label: "L40".into(),
        text: "Back off on 429s too.".into(),
        diff: "    return post(url, body);".into(),
        fence_language: Some("ts".into()),
        pull_request: None,
    })
}

fn move_to_end(view: &Entity<ThreadView>, cx: &mut VisualTestContext) {
    let composer = view.read_with(cx, |view, _| view.composer().clone());
    composer.update_in(cx, |editor, window, cx| {
        editor.move_to_end(&editor::actions::MoveToEnd, window, cx)
    });
}

#[gpui::test]
async fn context_goes_in_as_chips_and_rides_on_the_message(cx: &mut TestAppContext) {
    let backend = FakeBackend::with_thread(finished_thread(), 10);
    let (view, cx) = open_view(backend.clone(), Some(THREAD), cx);
    type_message(&view, "Fix what this shows:", cx);
    move_to_end(&view, cx);

    view.update_in(cx, |view, window, cx| {
        view.add_composer_context(terminal_context("t1"), window, cx);
        view.add_composer_context(review_comment_context(), window, cx);
        // The same excerpt again only refreshes its record.
        view.add_composer_context(terminal_context("t1"), window, cx);
    });
    cx.run_until_parked();
    let terminal_reference = "[Terminal 1 lines 3-4](t3-context://v1/terminal/terminal_t1)";
    let comment_reference = "[dispatcher.ts L40](t3-context://v1/review-comment/review-comment_c1)";
    assert_eq!(
        composer_text(&view, cx),
        format!("Fix what this shows: {terminal_reference} {comment_reference} ")
    );

    cx.dispatch_action(SendMessage);
    cx.run_until_parked();
    let command = without_ids(backend.commands()[0].clone());
    assert_eq!(
        command["text"],
        json!(format!(
            "Fix what this shows: {terminal_reference} {comment_reference}"
        ))
    );
    // T3's records, terminal excerpts first (`buildMessageContext`).
    assert_eq!(
        command["context"],
        json!({
            "version": 1.0,
            "records": [
                {
                    "version": 1, "kind": "terminal", "contextId": "terminal_t1",
                    "label": "Terminal 1 lines 3-4", "terminalId": "term-1",
                    "terminalLabel": "Terminal 1", "lineStart": 3, "lineEnd": 4,
                    "text": "$ npm test\nFAIL src/webhooks/dispatcher.test.ts"
                },
                {
                    "version": 1, "kind": "review-comment", "contextId": "review-comment_c1",
                    "label": "dispatcher.ts L40", "sectionId": "changes",
                    "sectionTitle": "Changes", "filePath": "src/webhooks/dispatcher.ts",
                    "startIndex": 39, "endIndex": 39, "rangeLabel": "L40",
                    "text": "Back off on 429s too.", "diff": "    return post(url, body);",
                    "fenceLanguage": "ts"
                }
            ]
        })
    );
}

#[gpui::test]
async fn a_chip_removed_from_the_text_leaves_its_record_behind(cx: &mut TestAppContext) {
    let backend = FakeBackend::with_thread(finished_thread(), 10);
    let (view, cx) = open_view(backend.clone(), Some(THREAD), cx);
    view.update_in(cx, |view, window, cx| {
        view.add_composer_context(terminal_context("t1"), window, cx);
    });
    // T3's labels are one line; ids it wouldn't take are hashed as it hashes them.
    view.update_in(cx, |view, window, cx| {
        let ComposerContext::Terminal(mut odd) = terminal_context("t2") else {
            unreachable!()
        };
        odd.context_id = "terminal 2/b".into();
        odd.label = "Terminal [2]\nlines 1-2".into();
        view.add_composer_context(ComposerContext::Terminal(odd), window, cx);
    });
    let text = composer_text(&view, cx);
    assert!(
        text.contains("[Terminal 2 lines 1-2](t3-context://v1/terminal/terminal-2-b-"),
        "{text}"
    );
    let composer = view.read_with(cx, |view, _| view.composer().clone());
    composer.update_in(cx, |editor, window, cx| {
        editor.set_text("Only this", window, cx)
    });
    cx.focus(&composer);
    cx.dispatch_action(SendMessage);
    cx.run_until_parked();
    let command = backend.commands()[0].clone();
    assert_eq!(command["text"], "Only this");
    assert_eq!(command.get("context"), None);
}

async fn workspace(
    backend: Arc<dyn ThreadBackend>,
    cx: &mut TestAppContext,
) -> (Entity<Workspace>, &mut VisualTestContext) {
    cx.update(|cx| {
        let settings_store = SettingsStore::test(cx);
        cx.set_global(settings_store);
        theme_settings::init(theme::LoadThemes::JustBase, cx);
        crate::init(cx);
        cx.bind_keys(crate::key_bindings());
        crate::set_backend(backend, cx);
    });
    let fs = project::FakeFs::new(cx.executor());
    let project = project::Project::test(fs, [], cx).await;
    let (multi_workspace, cx) =
        cx.add_window_view(|window, cx| MultiWorkspace::test_new(project.clone(), window, cx));
    let workspace =
        multi_workspace.read_with(cx, |multi_workspace, _| multi_workspace.workspace().clone());
    cx.simulate_resize(gpui::size(gpui::px(1200.), gpui::px(1000.)));
    cx.run_until_parked();
    (workspace, cx)
}

#[gpui::test]
async fn add_composer_context_opens_the_thread_with_the_chip(cx: &mut TestAppContext) {
    let backend = FakeBackend::with_thread(finished_thread(), 10);
    let (workspace, cx) = workspace(backend.clone(), cx).await;

    // Another part of mustr dispatches it, by name, with T3's record as the context.
    let action = cx
        .update(|_, cx| {
            cx.build_action(
                "thread_chat::AddComposerContext",
                Some(json!({
                    "thread_id": THREAD,
                    "context": {
                        "kind": "terminal", "contextId": "terminal_t9",
                        "label": "Terminal 1 line 7", "terminalId": "term-1",
                        "terminalLabel": "Terminal 1", "lineStart": 7, "lineEnd": 7,
                        "text": "panic: index out of range"
                    }
                })),
            )
        })
        .unwrap();
    assert!(action.as_any().is::<AddComposerContext>());
    cx.update(|window, cx| window.dispatch_action(action, cx));
    cx.run_until_parked();

    let view = workspace
        .read_with(cx, |workspace, cx| {
            workspace.active_item_as::<ThreadView>(cx)
        })
        .expect("the thread opened");
    assert_eq!(
        view.read_with(cx, |view, _| view.thread_id().map(str::to_string)),
        Some(THREAD.into())
    );
    assert_eq!(
        composer_text(&view, cx),
        "[Terminal 1 line 7](t3-context://v1/terminal/terminal_t9) "
    );
    // The composer has focus, so the next excerpt goes at the cursor.
    let composer = view.read_with(cx, |view, _| view.composer().clone());
    assert!(cx.update(|window, cx| composer.focus_handle(cx).is_focused(window)));
}

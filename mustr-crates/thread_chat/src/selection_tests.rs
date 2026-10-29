//! Tests of deleting threads and acting on several at once (WP17), against the recording backend
//! of `creation_tests.rs`. Nothing here deletes a real thread.

use crate::{
    DeleteThread, ThreadView, ThreadsSidebar,
    creation_tests::{Recorder, shell_thread, workspace},
};
use gpui::{Entity, Modifiers, TestAppContext, VisualContext as _, VisualTestContext};
use pretty_assertions::assert_eq;
use serde_json::{Value, json};
use std::sync::Arc;
use workspace::Workspace;

const NOW: &str = "2026-10-27T12:00:00.000Z";

/// A shell with threads `a` to `d` in the api project, newest first, `c` with a worktree of its
/// own and `d` running.
fn recorder() -> Arc<Recorder> {
    let recorder = Recorder::new();
    let thread = |id: &str, title: &str, created_at: &str, fields: Value| {
        let mut thread = shell_thread(id, "project-api", title);
        thread["createdAt"] = json!(created_at);
        if let (Value::Object(thread), Value::Object(fields)) = (&mut thread, fields) {
            thread.extend(fields);
        }
        thread
    };
    recorder.set_threads(vec![
        thread("a", "Alpha", "2026-10-27T11:00:00.000Z", json!({})),
        thread(
            "b",
            "Bravo",
            "2026-10-27T10:00:00.000Z",
            json!({ "pinnedAt": NOW, "latestRunCompletedAt": NOW }),
        ),
        thread(
            "c",
            "Charlie",
            "2026-10-27T09:00:00.000Z",
            json!({ "worktreePath": "/wt/charlie", "latestRunCompletedAt": NOW }),
        ),
        thread(
            "d",
            "Delta",
            "2026-10-27T08:00:00.000Z",
            json!({ "status": "running", "activeRunId": "run-3" }),
        ),
    ]);
    recorder
}

fn open(workspace: &Entity<Workspace>, thread_id: &str, cx: &mut VisualTestContext) {
    workspace.update_in(cx, |workspace, window, cx| {
        ThreadView::open(thread_id.into(), thread_id.into(), workspace, window, cx)
    });
    cx.run_until_parked();
}

fn shown_thread(workspace: &Entity<Workspace>, cx: &mut VisualTestContext) -> Option<String> {
    workspace.read_with(cx, |workspace, cx| {
        let view = workspace.active_item(cx)?.downcast::<ThreadView>()?;
        view.read(cx).thread_id().map(str::to_string)
    })
}

fn open_thread_tabs(workspace: &Entity<Workspace>, cx: &mut VisualTestContext) -> Vec<String> {
    workspace.read_with(cx, |workspace, cx| {
        workspace
            .active_pane()
            .read(cx)
            .items_of_type::<ThreadView>()
            .filter_map(|view| view.read(cx).thread_id().map(str::to_string))
            .collect()
    })
}

#[gpui::test]
async fn deleting_asks_first_then_moves_on(cx: &mut TestAppContext) {
    let backend = recorder();
    let (workspace, cx) = workspace(backend.clone(), cx).await;
    open(&workspace, "a", cx);

    // Cancelled, nothing is sent.
    cx.dispatch_action(DeleteThread);
    cx.run_until_parked();
    assert!(cx.has_pending_prompt());
    cx.simulate_prompt_answer("Cancel");
    cx.run_until_parked();
    assert_eq!(backend.commands(), Vec::<Value>::new());

    cx.dispatch_action(DeleteThread);
    cx.run_until_parked();
    cx.simulate_prompt_answer("Delete");
    cx.run_until_parked();
    assert_eq!(
        backend.commands(),
        [json!({ "type": "thread.delete", "threadId": "a" })]
    );
    // The deleted thread's tab gives way to the next thread in the sidebar.
    assert_eq!(shown_thread(&workspace, cx), Some("c".to_string()));
    assert_eq!(open_thread_tabs(&workspace, cx), ["c"]);

    // T3 refuses while an agent runs, so mustr doesn't ask.
    open(&workspace, "d", cx);
    cx.dispatch_action(DeleteThread);
    cx.run_until_parked();
    assert!(!cx.has_pending_prompt());
    assert_eq!(backend.commands().len(), 1);
}

fn sidebar(workspace: &Entity<Workspace>, cx: &mut VisualTestContext) -> Entity<ThreadsSidebar> {
    let workspace = workspace.downgrade();
    let sidebar = cx.new_window_entity(|window, cx| ThreadsSidebar::new(workspace, window, cx));
    cx.run_until_parked();
    sidebar
}

fn click(
    sidebar: &Entity<ThreadsSidebar>,
    thread_id: &str,
    modifiers: Modifiers,
    cx: &mut VisualTestContext,
) {
    sidebar.update_in(cx, |sidebar, window, cx| {
        let thread = sidebar.thread_list().find(thread_id).cloned().unwrap();
        sidebar.click_row(&thread, None, modifiers, window, cx);
    });
    cx.run_until_parked();
}

fn selected(sidebar: &Entity<ThreadsSidebar>, cx: &mut VisualTestContext) -> Vec<String> {
    sidebar.read_with(cx, |sidebar, _| {
        sidebar
            .selected_threads()
            .into_iter()
            .map(|thread| thread.id)
            .collect()
    })
}

#[gpui::test]
async fn rows_select_with_mod_and_shift_clicks(cx: &mut TestAppContext) {
    let backend = recorder();
    let (workspace, cx) = workspace(backend.clone(), cx).await;
    let sidebar = sidebar(&workspace, cx);
    // Pinned first, then the rest newest first.
    assert_eq!(
        sidebar.read_with(cx, |sidebar, _| sidebar
            .thread_list()
            .iter()
            .map(|thread| thread.id.clone())
            .collect::<Vec<_>>()),
        ["b", "a", "c", "d"]
    );

    click(&sidebar, "a", Modifiers::secondary_key(), cx);
    click(&sidebar, "d", Modifiers::shift(), cx);
    // Shown in the sidebar's order, from the row clicked first to the shift-clicked one.
    assert_eq!(selected(&sidebar, cx), ["a", "c", "d"]);
    // A mod-click takes one out again; nothing opened meanwhile.
    click(&sidebar, "c", Modifiers::secondary_key(), cx);
    assert_eq!(selected(&sidebar, cx), ["a", "d"]);
    assert_eq!(shown_thread(&workspace, cx), None);

    // A plain click opens the thread and clears the selection.
    click(&sidebar, "b", Modifiers::none(), cx);
    assert_eq!(selected(&sidebar, cx), Vec::<String>::new());
    assert_eq!(shown_thread(&workspace, cx), Some("b".to_string()));

    // Escape clears it too.
    click(&sidebar, "a", Modifiers::secondary_key(), cx);
    sidebar.update_in(cx, |sidebar, window, cx| {
        sidebar.clear_search(&menu::Cancel, window, cx)
    });
    assert_eq!(selected(&sidebar, cx), Vec::<String>::new());
}

#[gpui::test]
async fn the_selection_menu_acts_on_every_selected_thread(cx: &mut TestAppContext) {
    let backend = recorder();
    let (workspace, cx) = workspace(backend.clone(), cx).await;
    let sidebar = sidebar(&workspace, cx);
    for thread_id in ["a", "b", "c"] {
        click(&sidebar, thread_id, Modifiers::secondary_key(), cx);
    }
    let threads = sidebar.read_with(cx, |sidebar, _| sidebar.selected_threads());
    let actions = crate::bulk::bulk_actions(&threads);
    assert_eq!(
        actions
            .iter()
            .map(|action| (action.label.as_str(), action.ids.clone()))
            .collect::<Vec<_>>(),
        [
            ("Unpin (1)", vec!["b".to_string()]),
            ("Settle (3)", vec!["b".into(), "a".into(), "c".into()]),
            ("Snooze (3)", vec!["b".into(), "a".into(), "c".into()]),
            // Only threads with a finished run can be marked unread.
            ("Mark Unread (2)", vec!["b".into(), "c".into()]),
            ("Delete (3)…", vec!["b".into(), "a".into(), "c".into()]),
        ]
    );
    let run = |kind, ids: Vec<String>, until: Option<String>, cx: &mut VisualTestContext| {
        let workspace = workspace.downgrade();
        cx.update(|window, cx| {
            let focus = cx.focus_handle();
            crate::bulk::run_bulk(kind, ids, until, focus, workspace, window, cx)
        });
        cx.run_until_parked();
    };
    use crate::bulk::BulkKind;
    let ids = actions[1].ids.clone();
    run(BulkKind::Settle, ids.clone(), None, cx);
    run(
        BulkKind::Snooze,
        ids.clone(),
        Some("2026-10-28T09:00:00.000Z".into()),
        cx,
    );
    assert_eq!(
        backend.commands(),
        [
            json!({ "type": "thread.settle", "threadId": "b" }),
            json!({ "type": "thread.settle", "threadId": "a" }),
            json!({ "type": "thread.settle", "threadId": "c" }),
            json!({ "type": "thread.snooze", "threadId": "b", "snoozedUntil": "2026-10-28T09:00:00.000Z" }),
            json!({ "type": "thread.snooze", "threadId": "a", "snoozedUntil": "2026-10-28T09:00:00.000Z" }),
            json!({ "type": "thread.snooze", "threadId": "c", "snoozedUntil": "2026-10-28T09:00:00.000Z" }),
        ]
    );

    // Delete asks once for them all.
    run(BulkKind::Delete, ids, None, cx);
    assert!(cx.has_pending_prompt());
    cx.simulate_prompt_answer("Delete");
    cx.run_until_parked();
    assert_eq!(
        backend.commands()[6..],
        [
            json!({ "type": "thread.delete", "threadId": "b" }),
            json!({ "type": "thread.delete", "threadId": "a" }),
            json!({ "type": "thread.delete", "threadId": "c" }),
        ]
    );
}

/// Zed's keymap, then mustr's after it, as the app binds them.
fn bind_zed_then_mustr_keys(cx: &mut VisualTestContext) {
    cx.update(|_, cx| {
        let content = util::asset_str::<settings::SettingsAssets>(settings::DEFAULT_KEYMAP_PATH);
        let bindings = match settings::KeymapFile::load(&content, cx) {
            settings::KeymapFileLoadResult::Success { key_bindings }
            | settings::KeymapFileLoadResult::SomeFailedToLoad { key_bindings, .. } => key_bindings,
            settings::KeymapFileLoadResult::JsonParseFailure { error } => {
                panic!("Zed's keymap doesn't parse: {error}")
            }
        };
        cx.clear_key_bindings();
        cx.bind_keys(bindings);
        cx.bind_keys(crate::key_bindings());
    });
}

fn focused_composer(workspace: &Entity<Workspace>, cx: &mut VisualTestContext) {
    let composer = workspace.read_with(cx, |workspace, cx| {
        let view = workspace.active_item(cx)?.downcast::<ThreadView>()?;
        Some(view.read(cx).composer().clone())
    });
    cx.focus(&composer.unwrap());
    cx.run_until_parked();
}

#[gpui::test]
async fn mod_z_in_an_empty_composer_undoes_a_settle(cx: &mut TestAppContext) {
    let backend = recorder();
    let (workspace, cx) = workspace(backend.clone(), cx).await;
    bind_zed_then_mustr_keys(cx);
    open(&workspace, "a", cx);
    focused_composer(&workspace, cx);
    cx.simulate_keystrokes("ctrl-shift-s");
    cx.run_until_parked();
    // Settling moved on to the next thread, whose composer has focus.
    assert_eq!(shown_thread(&workspace, cx), Some("c".to_string()));
    focused_composer(&workspace, cx);
    cx.simulate_keystrokes("ctrl-z");
    cx.run_until_parked();
    assert_eq!(
        backend.commands(),
        [
            json!({ "type": "thread.settle", "threadId": "a" }),
            json!({ "type": "thread.unsettle", "threadId": "a", "reason": "user" }),
        ]
    );

    // With text in the composer, `mod+z` is the editor's undo.
    cx.simulate_keystrokes("ctrl-shift-s");
    cx.run_until_parked();
    focused_composer(&workspace, cx);
    cx.simulate_input("x");
    cx.simulate_keystrokes("ctrl-z");
    cx.run_until_parked();
    assert_eq!(backend.commands().len(), 3);
    let composer_text = workspace.read_with(cx, |workspace, cx| {
        let view = workspace.active_item(cx)?.downcast::<ThreadView>()?;
        Some(view.read(cx).composer().read(cx).text(cx))
    });
    assert_eq!(composer_text, Some(String::new()));
}

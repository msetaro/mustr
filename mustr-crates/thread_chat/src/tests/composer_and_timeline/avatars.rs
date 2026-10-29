//! Avatars as T3 draws them: none for the user or the thread's own agent, and a badge with the
//! agent's icon and a status dot for each subagent (`chat/V2LifecycleRow.tsx`).

use super::*;
use pretty_assertions::assert_eq;

fn subagent(id: &str, ordinal: i64, status: &str, fields: Value) -> Value {
    let mut item = item(
        "subagent",
        id,
        ordinal,
        Some("run-1"),
        status,
        json!({
            "subagentId": format!("sub-{id}"), "driver": "codex", "providerInstanceId": "codex",
            "origin": "app_owned", "prompt": "Review the retry logic",
            "title": "Subagent: Review the retry logic", "childThreadId": null,
            "progress": null, "result": null, "providerThreadId": null,
            "providerTurnId": null, "startedAt": "2026-10-27T11:58:00.000Z"
        }),
    );
    if let (Value::Object(item), Value::Object(fields)) = (&mut item, fields) {
        item.extend(fields);
    }
    item
}

#[gpui::test]
async fn only_subagents_have_avatars(cx: &mut TestAppContext) {
    let mut projection = finished_thread();
    projection.items.extend([
        subagent(
            "s1",
            6,
            "running",
            json!({ "progress": "Reading `src/webhooks/dispatcher.ts`" }),
        ),
        subagent(
            "s2",
            7,
            "completed",
            json!({
                "completedAt": "2026-10-27T11:59:30.000Z",
                "result": "Found two bugs, see [the notes](https://example.com)"
            }),
        ),
    ]);
    let backend = FakeBackend::with_thread(projection, 10);
    let (view, cx) = open_view(backend.clone(), Some(THREAD), cx);
    cx.simulate_resize(gpui::size(gpui::px(1200.), gpui::px(1400.)));
    cx.run_until_parked();

    // Subagents are rows of their own, not tool calls in a run of work.
    view.read_with(cx, |view, _| {
        let state = view.thread_state().unwrap();
        assert!(view.rows().iter().any(|row| {
            matches!(row, Row::Item(index) if state.item(*index).unwrap().id == "s1")
        }));
    });

    // The user's message and the agent's answer have no avatar.
    for id in ["i1", "i5"] {
        assert!(
            cx.debug_bounds(Box::leak(
                format!("thread-subagent-badge-{id}").into_boxed_str()
            ))
            .is_none()
        );
    }
    // The user's message is a bubble on the right; the answer reads full width from the left.
    let bubble = cx.debug_bounds("thread-message-bubble-i1").unwrap();
    let answer = cx.debug_bounds("thread-message-answer-i5").unwrap();
    assert!(bubble.left() > answer.left() + gpui::px(100.));
    assert!((bubble.right() - answer.right()).abs() < gpui::px(8.));

    // Each subagent has T3's badge, its dot in its status's color.
    assert!(cx.debug_bounds("thread-subagent-badge-s1").is_some());
    assert!(
        cx.debug_bounds("thread-subagent-badge-s1-running")
            .is_some()
    );
    assert!(
        cx.debug_bounds("thread-subagent-badge-s2-completed")
            .is_some()
    );
    assert!(
        cx.debug_bounds("thread-subagent-badge-s2-running")
            .is_none()
    );
    assert!(backend.commands().is_empty());
}

#[test]
fn subagent_rows_say_what_t3s_do() {
    use crate::timeline::subagent_detail;
    // While it works, what it's doing; once it settled, what it found, as plain text.
    assert_eq!(
        subagent_detail(false, Some("Reading `a.rs`"), Some("Done")),
        Some("Reading a.rs".to_string())
    );
    assert_eq!(
        subagent_detail(
            true,
            Some("Reading"),
            Some("- Found [two bugs](https://x)\n- Fixed")
        ),
        Some("Found two bugs Fixed".to_string())
    );
    // T3's generic end says nothing the status doesn't.
    assert_eq!(
        subagent_detail(true, None, Some("Child task ended with status completed")),
        None
    );
}

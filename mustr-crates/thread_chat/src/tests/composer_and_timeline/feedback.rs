//! Declining an approval with a reason (`runtime-request.respond` with `feedback`, T3 0.1.3),
//! against the recording backend.

use super::context::terminal_context;
use super::*;
use pretty_assertions::assert_eq;

// Declining with a reason.

/// A pending approval whose agent takes a reason with a decline (T3 0.1.3).
fn feedback_request(id: &str) -> Value {
    let mut request = runtime_request(id, "pending");
    request["responseCapability"]["supportsFeedback"] = json!(true);
    request
}

fn placeholder(view: &Entity<ThreadView>, cx: &mut VisualTestContext) -> Option<String> {
    let composer = view.read_with(cx, |view, _| view.composer().clone());
    cx.update(|_, cx| composer.update(cx, |editor, cx| editor.placeholder_text(cx)))
}

#[gpui::test]
async fn a_message_declines_an_approval_that_takes_a_reason(cx: &mut TestAppContext) {
    let mut projection = running_thread();
    projection.runtime_requests = vec![feedback_request("request-1")];
    let backend = FakeBackend::with_thread(projection, 20);
    let (view, cx) = open_view(backend.clone(), Some(THREAD), cx);
    cx.simulate_resize(gpui::size(gpui::px(1000.), gpui::px(900.)));
    cx.run_until_parked();
    assert_eq!(
        placeholder(&view, cx).as_deref(),
        Some("Send feedback to decline this request")
    );
    assert!(cx.debug_bounds("thread-decline-feedback").is_some());
    assert!(cx.debug_bounds("thread-queue").is_none());

    type_message(
        &view,
        "Run only the webhook tests, not the whole suite.",
        cx,
    );
    cx.dispatch_action(SendMessage);
    cx.run_until_parked();
    assert_eq!(
        without_ids(backend.commands()[0].clone()),
        json!({
            "type": "runtime-request.respond",
            "threadId": THREAD,
            "requestId": "request-1",
            "decision": "decline",
            "feedback": "Run only the webhook tests, not the whole suite."
        })
    );
    assert_eq!(composer_text(&view, cx), "");

    // With a chip in it, the message can't go as feedback, as in T3.
    type_message(&view, "See this", cx);
    view.update_in(cx, |view, window, cx| {
        view.add_composer_context(terminal_context("t1"), window, cx)
    });
    cx.dispatch_action(SendMessage);
    cx.run_until_parked();
    assert_eq!(backend.commands().len(), 1);
    assert_eq!(
        view.read_with(cx, |view, _| view.error().map(ToString::to_string)),
        Some("Send approval feedback without attachments or added context.".into())
    );

    // Once the request is answered, the composer sends as usual again.
    backend.push(event(
        21,
        "runtime-request.updated",
        runtime_request("request-1", "resolved"),
    ));
    cx.run_until_parked();
    assert_eq!(
        placeholder(&view, cx).as_deref(),
        Some("Message the agent…")
    );
}

#[gpui::test]
async fn older_servers_queue_the_message_instead(cx: &mut TestAppContext) {
    // T3 0.1.1 never says `supportsFeedback`.
    let backend = FakeBackend::with_thread(running_thread(), 20);
    let (view, cx) = open_view(backend.clone(), Some(THREAD), cx);
    cx.simulate_resize(gpui::size(gpui::px(1000.), gpui::px(900.)));
    cx.run_until_parked();
    assert_eq!(
        placeholder(&view, cx).as_deref(),
        Some("Message the agent…")
    );
    assert!(cx.debug_bounds("thread-decline-feedback").is_none());
    type_message(&view, "Also update the docs", cx);
    cx.dispatch_action(SendMessage);
    cx.run_until_parked();
    let command = backend.commands()[0].clone();
    assert_eq!(command["type"], "message.dispatch");
    assert_eq!(
        command["dispatchMode"],
        json!({ "type": "queue_after_active" })
    );
}

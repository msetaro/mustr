//! Quoting a response, answering questions with text and attachments, MCP elicitation, the
//! prompt stash, and image and file context records, against the recording backend: every
//! payload is asserted as T3 gets it.

use super::*;
use pretty_assertions::assert_eq;

/// What `server.getConfig` answers, as far as the composer reads it.
pub(super) fn server_config(version: &str, capabilities: Value) -> Value {
    json!({
        "environment": {
            "environmentId": "env-1",
            "serverVersion": version,
            "capabilities": capabilities
        },
        "providers": []
    })
}

/// Answers `server.getConfig` with `config`, and other calls with `null`.
struct ConfigBackend {
    inner: Arc<FakeBackend>,
    config: Value,
}

impl ThreadBackend for ConfigBackend {
    fn shell_snapshot(&self) -> BoxFuture<'static, Result<OrchestrationV2ShellSnapshot>> {
        self.inner.shell_snapshot()
    }

    fn subscribe_shell(
        &self,
        input: OrchestrationV2SubscribeShellInput,
    ) -> Result<BoxStream<'static, Result<OrchestrationV2ShellStreamItem>>> {
        self.inner.subscribe_shell(input)
    }

    fn thread_snapshot(
        &self,
        thread_id: String,
    ) -> BoxFuture<'static, Result<OrchestrationV2ThreadDetailSnapshot>> {
        self.inner.thread_snapshot(thread_id)
    }

    fn subscribe_thread(
        &self,
        input: OrchestrationV2SubscribeThreadInput,
    ) -> Result<BoxStream<'static, Result<OrchestrationV2ThreadStreamItem>>> {
        self.inner.subscribe_thread(input)
    }

    fn turn_item(
        &self,
        input: OrchestrationV2GetTurnItemInput,
    ) -> BoxFuture<'static, Result<OrchestrationV2GetTurnItemResult>> {
        self.inner.turn_item(input)
    }

    fn providers(&self) -> BoxFuture<'static, Result<Vec<ServerProvider>>> {
        self.inner.providers()
    }

    fn dispatch(
        &self,
        command: OrchestrationV2Command,
    ) -> BoxFuture<'static, Result<OrchestrationV2DispatchCommandResult>> {
        self.inner.dispatch(command)
    }

    fn launch_thread(
        &self,
        input: OrchestrationV2ThreadLaunchInput,
    ) -> BoxFuture<'static, Result<OrchestrationV2ThreadLaunchResult>> {
        self.inner.launch_thread(input)
    }

    fn call_raw(
        &self,
        tag: &str,
        payload: Value,
        access: Access,
    ) -> BoxFuture<'static, Result<Value>> {
        if tag == "server.getConfig" {
            self.inner.record_raw(tag, payload, access);
            return ready(Ok(self.config.clone()));
        }
        self.inner.call_raw(tag, payload, access)
    }

    fn subscribe_raw(
        &self,
        tag: &str,
        payload: Value,
        access: Access,
    ) -> Result<BoxStream<'static, Result<Value>>> {
        self.inner.subscribe_raw(tag, payload, access)
    }

    fn upload(&self, upload: AttachmentUpload) -> BoxFuture<'static, Result<()>> {
        self.inner.upload(upload)
    }

    fn fetch_asset(&self, relative_url: &str) -> BoxFuture<'static, Result<Vec<u8>>> {
        self.inner.fetch_asset(relative_url)
    }
}

pub(super) fn with_config(inner: &Arc<FakeBackend>, config: Value) -> Arc<dyn ThreadBackend> {
    Arc::new(ConfigBackend {
        inner: inner.clone(),
        config,
    })
}

// Quoting a response.

#[gpui::test]
async fn quote_puts_t3s_citation_of_the_response_in_the_composer(cx: &mut TestAppContext) {
    let inner = FakeBackend::with_thread(finished_thread(), 10);
    let backend = with_config(&inner, server_config("0.1.3", json!({})));
    let (view, cx) = open_view(backend, Some(THREAD), cx);
    cx.simulate_resize(gpui::size(gpui::px(1000.), gpui::px(900.)));
    cx.run_until_parked();
    type_message(&view, "Why this way?", cx);
    cx.run_until_parked();

    // Nothing selected: the whole response.
    let quote = cx
        .debug_bounds("thread-quote-i5")
        .expect("the answer has Quote");
    // It shows on hover.
    cx.simulate_mouse_move(quote.center(), None, gpui::Modifiers::none());
    cx.run_until_parked();
    cx.simulate_click(quote.center(), gpui::Modifiers::none());
    cx.run_until_parked();
    let link = "[Assistant quote](t3-citation://v1/env-1/thread-1/message-i5?\
                text=Done.+%60withRetry%60+now+wraps+%60post%28%29%60.\
                &start=0&end=37&prefix=&suffix=)";
    assert_eq!(composer_text(&view, cx), format!("Why this way? {link} "));

    cx.dispatch_action(SendMessage);
    cx.run_until_parked();
    let command = without_ids(inner.commands()[0].clone());
    assert_eq!(command["type"], "message.dispatch");
    assert_eq!(command["text"], json!(format!("Why this way? {link}")));
    assert_eq!(command.get("context"), None);
}

#[gpui::test]
async fn quote_takes_whats_selected_in_the_response(cx: &mut TestAppContext) {
    let inner = FakeBackend::with_thread(finished_thread(), 10);
    let backend = with_config(&inner, server_config("0.1.3", json!({})));
    let (view, cx) = open_view(backend, Some(THREAD), cx);
    cx.simulate_resize(gpui::size(gpui::px(1000.), gpui::px(900.)));
    cx.run_until_parked();
    view.update_in(cx, |view, window, cx| {
        // As the timeline notes a selection when it draws the message.
        view.composer_state.quote = Default::default();
        view.composer_state
            .quote
            .quote_selection_for_test("i5", 5, " `withRetry` ");
        view.quote_in_composer("i5", window, cx);
    });
    cx.run_until_parked();
    assert_eq!(
        composer_text(&view, cx),
        "[Assistant quote](t3-citation://v1/env-1/thread-1/message-i5?\
         text=%60withRetry%60&start=6&end=17&prefix=Done.+&suffix=+now+wraps+%60post%28%29%60.) "
    );
}

#[gpui::test]
async fn without_the_environment_a_quote_is_a_block_quote(cx: &mut TestAppContext) {
    let backend = FakeBackend::with_thread(finished_thread(), 10);
    let (view, cx) = open_view(backend, Some(THREAD), cx);
    view.update_in(cx, |view, window, cx| {
        view.quote_in_composer("i5", window, cx)
    });
    cx.run_until_parked();
    assert_eq!(
        composer_text(&view, cx),
        "> Done. `withRetry` now wraps `post()`.\n\n"
    );
}

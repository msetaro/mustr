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

/// The commands sent, without their ids.
fn sent(backend: &FakeBackend) -> Vec<Value> {
    backend.commands().into_iter().map(without_ids).collect()
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

// Answering questions, with text and attachments.

/// A run waiting on two questions: one with options that takes a typed answer too, and one
/// without options.
fn question_thread() -> Projection {
    let mut projection = finished_thread();
    projection.items.extend([
        user_message("i6", 6, "run-2", "Set up the cache"),
        item(
            "user_input_request",
            "i7",
            7,
            Some("run-2"),
            "waiting",
            json!({
                "requestId": "request-q",
                "questions": [
                    {
                        "id": "q1", "header": "Store", "question": "Which store?",
                        "options": [
                            { "label": "Redis", "description": "In memory" },
                            { "label": "Postgres", "description": "On disk", "value": "pg" }
                        ]
                    },
                    { "id": "q2", "header": "Notes", "question": "Anything else?", "options": [] }
                ]
            }),
        ),
    ]);
    projection.runs.push(run("run-2", 2, "running", "m2"));
    projection
        .messages
        .push(message("m2", "run-2", "Set up the cache"));
    let mut request = runtime_request("request-q", "pending");
    request["kind"] = json!("user_input");
    projection.runtime_requests.push(request);
    projection
}

fn attach_notes(view: &Entity<ThreadView>, cx: &mut VisualTestContext) {
    use crate::attachments::{NewAttachment, NewFile};
    view.update_in(cx, |view, window, cx| {
        view.add_attachments(
            vec![NewFile::pasted_text("notes.txt".into(), "notes").map(NewAttachment::File)],
            window,
            cx,
        )
    });
    cx.run_until_parked();
}

fn load_config(view: &Entity<ThreadView>, cx: &mut VisualTestContext) {
    view.update(cx, |view, cx| view.load_server_info(false, cx));
    cx.run_until_parked();
}

#[gpui::test]
async fn the_composer_answers_questions_with_text_and_attachments(cx: &mut TestAppContext) {
    let inner = FakeBackend::with_thread(question_thread(), 20);
    inner.raw_responses.lock().push(minted("attachment-1"));
    let config = server_config(
        "0.1.3",
        json!({
            "questionAttachments": true,
            "attachmentUploads": true,
            "fileAttachments": { "maxUploadBytes": 1048576 }
        }),
    );
    let (view, cx) = open_view(with_config(&inner, config), Some(THREAD), cx);
    cx.simulate_resize(gpui::size(gpui::px(1000.), gpui::px(1200.)));
    load_config(&view, cx);
    assert!(cx.debug_bounds("thread-question-request-q-q1").is_some());
    // The composer answers the first question, so it offers Next, not Queue or Steer.
    assert!(cx.debug_bounds("thread-answer").is_some());
    assert!(cx.debug_bounds("thread-queue").is_none());

    // A typed answer with a file answers the first question, and moves to the next.
    type_message(&view, "Memcached, see the notes", cx);
    attach_notes(&view, cx);
    cx.dispatch_action(SendMessage);
    cx.run_until_parked();
    assert!(inner.commands().is_empty(), "one question is left");
    assert_eq!(composer_text(&view, cx), "");
    assert!(view.read_with(cx, |view, _| view.composer_files().is_empty()));

    type_message(&view, "Keep it under 1 GB", cx);
    cx.dispatch_action(SendMessage);
    cx.run_until_parked();
    assert_eq!(
        sent(&inner),
        [json!({
            "type": "runtime-request.respond",
            "threadId": THREAD,
            "requestId": "request-q",
            "answers": { "q1": "Memcached, see the notes", "q2": "Keep it under 1 GB" },
            "attachmentsByQuestionId": {
                "q1": [{
                    "type": "file", "id": "attachment-1", "name": "notes.txt",
                    "mimeType": "text/plain;charset=utf-8", "sizeBytes": 5,
                    "source": { "_tag": "pasted-text" }
                }]
            }
        })]
    );
}

#[gpui::test]
async fn an_option_or_a_typed_answer_answers_a_question(cx: &mut TestAppContext) {
    let inner = FakeBackend::with_thread(question_thread(), 20);
    let (view, cx) = open_view(
        with_config(&inner, server_config("0.1.3", json!({}))),
        Some(THREAD),
        cx,
    );
    cx.simulate_resize(gpui::size(gpui::px(1000.), gpui::px(1200.)));
    load_config(&view, cx);
    // Postgres for the first, so the composer answers the second.
    view.update(cx, |view, cx| {
        view.pick_option("request-q".into(), "q1".into(), "pg".into(), cx)
    });
    type_message(&view, "No", cx);
    // The card's Answer takes the composer's text too.
    view.update_in(cx, |view, window, cx| {
        view.answer_with_composer(window, cx);
    });
    cx.run_until_parked();
    assert_eq!(
        sent(&inner),
        [json!({
            "type": "runtime-request.respond",
            "threadId": THREAD,
            "requestId": "request-q",
            "answers": { "q1": "pg", "q2": "No" }
        })]
    );
}

#[gpui::test]
async fn older_servers_take_no_attachments_in_answers(cx: &mut TestAppContext) {
    let inner = FakeBackend::with_thread(question_thread(), 20);
    inner.raw_responses.lock().push(minted("attachment-1"));
    // T3 0.1.1 has no `questionAttachments`.
    let (view, cx) = open_view(
        with_config(
            &inner,
            server_config(
                "0.1.1",
                json!({ "attachmentUploads": true, "fileAttachments": { "maxUploadBytes": 1048576 } }),
            ),
        ),
        Some(THREAD),
        cx,
    );
    load_config(&view, cx);
    type_message(&view, "Memcached", cx);
    attach_notes(&view, cx);
    cx.dispatch_action(SendMessage);
    cx.run_until_parked();
    assert!(inner.commands().is_empty());
    view.read_with(cx, |view, _| {
        assert_eq!(
            view.error().map(|error| error.as_ref()),
            Some("This question can't take attachments.")
        );
        assert_eq!(view.composer_files().len(), 1, "the file stays");
    });
}

// MCP elicitation and the other kinds of approval.

#[gpui::test]
async fn an_mcp_servers_question_is_an_app_access_approval(cx: &mut TestAppContext) {
    let mut projection = running_thread();
    projection.items[7] = item(
        "approval_request",
        "i8",
        8,
        Some("run-2"),
        "waiting",
        json!({
            "requestId": "request-1",
            "requestKind": "mcp-elicitation",
            "appName": "Linear",
            "prompt": "Allow Codex to use Linear?",
            "reason": "To file the bug it found",
            "options": [
                { "decision": "cancel", "label": "Cancel" },
                { "decision": "decline", "label": "Decline" },
                {
                    "decision": "acceptForSession", "label": "Always allow this session",
                    "warning": "Linear can act for you until the session ends"
                },
                { "decision": "accept", "label": "Approve" }
            ]
        }),
    );
    let backend = FakeBackend::with_thread(projection, 20);
    let (view, cx) = open_view(backend.clone(), Some(THREAD), cx);
    cx.simulate_resize(gpui::size(gpui::px(1000.), gpui::px(1200.)));
    cx.run_until_parked();
    let pending = view.read_with(cx, |view, _| {
        view.thread_state().unwrap().pending_requests()
    });
    assert!(matches!(
        pending[0].item.as_ref().map(|item| &item.kind),
        Some(ItemKind::Approval { request_kind, app_name: Some(app), reason: Some(_), options, .. })
            if request_kind == "mcp-elicitation" && app == "Linear"
                && options[2].warning.as_deref()
                    == Some("Linear can act for you until the session ends")
    ));
    for selector in [
        "thread-respond-request-1-0",
        "thread-respond-request-1-1",
        "thread-respond-request-1-2",
        "thread-respond-request-1-3",
    ] {
        assert!(cx.debug_bounds(selector).is_some(), "{selector}");
    }
    let approve = cx.debug_bounds("thread-respond-request-1-3").unwrap();
    cx.simulate_click(approve.center(), gpui::Modifiers::none());
    cx.run_until_parked();
    assert_eq!(
        sent(&backend),
        [json!({
            "type": "runtime-request.respond",
            "threadId": THREAD,
            "requestId": "request-1",
            "decision": "accept"
        })]
    );
}

// Image and file context records.

async fn send_an_image_and_a_file(
    capabilities: Value,
    cx: &mut TestAppContext,
) -> (Arc<FakeBackend>, Value) {
    use crate::attachments::{NewAttachment, NewFile, NewImage};
    let inner = FakeBackend::with_thread(finished_thread(), 10);
    inner
        .raw_responses
        .lock()
        .extend([minted("attachment-1"), minted("attachment-2")]);
    let (view, cx) = open_view(
        with_config(&inner, server_config("0.1.3", capabilities)),
        Some(THREAD),
        cx,
    );
    load_config(&view, cx);
    view.update_in(cx, |view, window, cx| {
        view.add_attachments(
            vec![
                NewImage::prepare(Some("screen [1].png".into()), None, super::png(4, 4))
                    .map(NewAttachment::Image),
                NewFile::pasted_text("notes.txt".into(), "notes").map(NewAttachment::File),
            ],
            window,
            cx,
        )
    });
    cx.run_until_parked();
    type_message(&view, "Here", cx);
    cx.dispatch_action(SendMessage);
    cx.run_until_parked();
    let command = sent(&inner).remove(0);
    (inner, command)
}

#[gpui::test]
async fn images_and_files_go_with_their_context_records(cx: &mut TestAppContext) {
    let capabilities = json!({
        "inlineMessageContext": true,
        "attachmentUploads": true,
        "fileAttachments": { "maxUploadBytes": 1048576 }
    });
    let (_, command) = send_an_image_and_a_file(capabilities, cx).await;
    let image_size = command["attachments"][0]["sizeBytes"].clone();
    assert_eq!(
        command["context"],
        json!({
            "version": 1.0,
            "records": [
                {
                    "version": 1, "contextId": "image_attachment-1", "label": "screen 1 .png",
                    "kind": "image", "attachmentId": "attachment-1", "name": "screen [1].png",
                    "mimeType": "image/png", "sizeBytes": image_size
                },
                {
                    "version": 1, "contextId": "file_attachment-2", "label": "notes.txt",
                    "kind": "file", "attachmentId": "attachment-2", "name": "notes.txt",
                    "mimeType": "text/plain;charset=utf-8", "sizeBytes": 5
                }
            ]
        })
    );
    assert_eq!(command["attachments"].as_array().map(Vec::len), Some(2));
}

#[gpui::test]
async fn servers_without_inline_context_get_the_attachments_alone(cx: &mut TestAppContext) {
    let capabilities = json!({
        "attachmentUploads": true,
        "fileAttachments": { "maxUploadBytes": 1048576 }
    });
    let (_, command) = send_an_image_and_a_file(capabilities, cx).await;
    assert_eq!(command.get("context"), None);
    assert_eq!(command["attachments"].as_array().map(Vec::len), Some(2));
}

// The prompt stash.

#[gpui::test]
async fn mod_s_stashes_the_prompt_and_brings_it_back(cx: &mut TestAppContext) {
    let inner = FakeBackend::with_thread(finished_thread(), 10);
    inner.raw_responses.lock().push(minted("attachment-1"));
    let config = server_config(
        "0.1.3",
        json!({ "attachmentUploads": true, "fileAttachments": { "maxUploadBytes": 1048576 } }),
    );
    let (view, cx) = open_view(with_config(&inner, config), Some(THREAD), cx);
    cx.simulate_resize(gpui::size(gpui::px(1000.), gpui::px(900.)));
    load_config(&view, cx);
    let bindings = crate::key_bindings();
    let stash = bindings
        .iter()
        .find(|binding| binding.action().name() == "thread_chat::StashPrompt")
        .expect("mod-s is bound");
    assert_eq!(
        stash.keystrokes()[0].unparse(),
        if cfg!(target_os = "macos") {
            "cmd-s"
        } else {
            "ctrl-s"
        }
    );

    type_message(&view, "Draft the migration", cx);
    attach_notes(&view, cx);
    cx.dispatch_action(crate::StashPrompt);
    cx.run_until_parked();
    assert_eq!(composer_text(&view, cx), "");
    assert!(view.read_with(cx, |view, _| view.composer_files().is_empty()));
    assert!(cx.debug_bounds("thread-stash").is_some());

    // The only stashed prompt comes back, file and all, and goes as it would have.
    cx.dispatch_action(crate::StashPrompt);
    cx.run_until_parked();
    assert_eq!(composer_text(&view, cx), "Draft the migration");
    assert!(cx.debug_bounds("thread-stash").is_none());
    cx.dispatch_action(SendMessage);
    cx.run_until_parked();
    let command = sent(&inner).remove(0);
    assert_eq!(command["text"], "Draft the migration");
    assert_eq!(command["attachments"][0]["id"], "attachment-1");
}

#[gpui::test]
async fn with_several_stashed_mod_s_shows_the_stash(cx: &mut TestAppContext) {
    let backend = FakeBackend::with_thread(finished_thread(), 10);
    let (view, cx) = open_view(backend, Some(THREAD), cx);
    cx.simulate_resize(gpui::size(gpui::px(1000.), gpui::px(900.)));
    for draft in ["First idea", "Second idea"] {
        type_message(&view, draft, cx);
        cx.dispatch_action(crate::StashPrompt);
        cx.run_until_parked();
    }
    cx.dispatch_action(crate::StashPrompt);
    cx.run_until_parked();
    // Newest first.
    assert!(cx.debug_bounds("thread-stash-list").is_some());
    let older = cx
        .debug_bounds("thread-stashed-1")
        .expect("both are listed");
    cx.simulate_click(older.center(), gpui::Modifiers::none());
    cx.run_until_parked();
    assert_eq!(composer_text(&view, cx), "First idea");
    assert!(cx.debug_bounds("thread-stash-list").is_none());
    // Something typed stays, with the stashed prompt after it.
    type_message(&view, "Also", cx);
    view.update_in(cx, |view, window, cx| {
        let id = crate::stash::PromptStash::get(cx).entries[0].id.clone();
        view.restore_stashed(&id, window, cx)
    });
    assert_eq!(composer_text(&view, cx), "Also\n\nSecond idea");
}

// Markdown in the composer.

#[gpui::test]
async fn the_composer_highlights_markdown_and_has_t3s_formatting_keys(cx: &mut TestAppContext) {
    init_test(cx);
    let languages = Arc::new(language::LanguageRegistry::test(cx.executor()));
    languages.add(Arc::new(language::Language::new(
        language::LanguageConfig {
            name: "Markdown".into(),
            ..Default::default()
        },
        None,
    )));
    let backend = FakeBackend::with_thread(finished_thread(), 10);
    let (view, cx) = cx.add_window_view(|window, cx| {
        ThreadView::new(
            backend,
            Some(languages),
            Some(THREAD.into()),
            "".into(),
            window,
            cx,
        )
    });
    cx.run_until_parked();
    let composer = view.read_with(cx, |view, _| view.composer().clone());
    let language = composer.read_with(cx, |editor, cx| {
        editor
            .buffer()
            .read(cx)
            .as_singleton()
            .and_then(|buffer| buffer.read(cx).language().map(|language| language.name()))
    });
    assert_eq!(
        language.map(|name| name.to_string()).as_deref(),
        Some("Markdown")
    );

    type_message(&view, "Use retries here", cx);
    composer.update_in(cx, |editor, window, cx| {
        editor.change_selections(Default::default(), window, cx, |selections| {
            selections.select_ranges([editor::MultiBufferOffset(4)..editor::MultiBufferOffset(11)])
        })
    });
    cx.dispatch_action(crate::ToggleBold);
    assert_eq!(composer_text(&view, cx), "Use **retries** here");
    cx.dispatch_action(crate::ToggleInlineCode);
    assert_eq!(composer_text(&view, cx), "Use **`retries`** here");
    cx.dispatch_action(crate::ToggleInlineCode);
    cx.dispatch_action(crate::ToggleBold);
    assert_eq!(composer_text(&view, cx), "Use retries here");
}

// HTML renders and previews.

#[gpui::test]
async fn an_html_render_opens_in_the_browser(cx: &mut TestAppContext) {
    let mut projection = finished_thread();
    projection.items.push(item(
        "dynamic_tool",
        "i6",
        6,
        Some("run-1"),
        "completed",
        json!({
            "toolName": "mcp__t3-code__html_render",
            "input": { "html": "<h1>p95</h1>", "title": "Latency" },
            "output": {
                "structuredContent": {
                    "htmlRender": { "attachmentId": "att-render-1", "title": "Latency", "height": 400 },
                    "message": "Rendered."
                }
            }
        }),
    ));
    projection.items.push(item(
        "dynamic_tool",
        "i7",
        7,
        Some("run-1"),
        "completed",
        json!({
            "toolName": "mcp__t3-code__preview_open",
            "input": { "url": "http://localhost:5173/" },
            "output": { "content": [{ "type": "text", "text": "Opened." }] }
        }),
    ));
    let backend = FakeBackend::with_thread(projection, 10);
    backend.raw_responses.lock().push(json!({
        "relativeUrl": "/api/assets/att-render-1.html?signature=x",
        "expiresAt": 1_800_000_000_000_i64
    }));
    let (view, cx) = open_view(backend.clone(), Some(THREAD), cx);
    cx.simulate_resize(gpui::size(gpui::px(1000.), gpui::px(1200.)));
    cx.run_until_parked();
    // The render is a row of its own, not a step of the work.
    assert!(matches!(
        view.read_with(cx, |view, _| view.thread_state().unwrap().items()[5].kind.clone()),
        ItemKind::HtmlRender { attachment_id, title }
            if attachment_id == "att-render-1" && title == "Latency"
    ));
    let open = cx
        .debug_bounds("thread-html-open-i6")
        .expect("the render has its card");
    cx.simulate_click(open.center(), gpui::Modifiers::none());
    cx.run_until_parked();
    assert_eq!(
        *backend.raw_calls.lock(),
        [RawCall {
            tag: "assets.createUrl".into(),
            payload: json!({
                "resource": {
                    "_tag": "attachment", "attachmentId": "att-render-1",
                    "fileName": "Latency.html", "mimeType": "text/html", "disposition": "inline"
                }
            }),
            access: Access::Read,
        }]
    );
    assert_eq!(
        *backend.fetched_assets.lock(),
        ["/api/assets/att-render-1.html?signature=x"]
    );
    let opened = cx.opened_url().expect("the page opened");
    assert!(
        opened.starts_with("file://") && opened.ends_with("/Latency.html"),
        "{opened}"
    );
    let path = opened.trim_start_matches("file://");
    assert_eq!(std::fs::read(path).unwrap(), super::png(4, 3));

    // The preview's page opens too, from its tool call.
    view.update(cx, |view, cx| view.toggle_expanded_for_test("work:i7", cx));
    cx.run_until_parked();
    let preview = cx
        .debug_bounds("thread-preview-open-i7")
        .expect("the preview has its page");
    cx.simulate_click(preview.center(), gpui::Modifiers::none());
    cx.run_until_parked();
    assert_eq!(cx.opened_url().as_deref(), Some("http://localhost:5173/"));
}

// Save to workspace.

#[gpui::test]
async fn a_plan_saves_to_the_threads_folder(cx: &mut TestAppContext) {
    let folder = std::env::temp_dir().join(format!("mustr-wp15-plan-{}", uuid::Uuid::new_v4()));
    std::fs::create_dir_all(&folder).unwrap();
    let mut projection = super::planned_thread("completed", "default");
    projection["thread"]["worktreePath"] = json!(folder.display().to_string());
    let (view, cx) = open_view(super::backend_with(projection), Some(THREAD), cx);
    cx.simulate_resize(gpui::size(gpui::px(1000.), gpui::px(1200.)));
    cx.run_until_parked();

    let save = cx
        .debug_bounds("thread-plan-save-i6")
        .expect("the plan has Save");
    cx.simulate_click(save.center(), gpui::Modifiers::none());
    cx.run_until_parked();
    // The path starts from the plan's title, as T3's does.
    assert!(cx.debug_bounds("thread-plan-save-path-i6").is_some());
    cx.simulate_input("docs/retries.md");
    let confirm = cx.debug_bounds("thread-plan-save-confirm-i6").unwrap();
    cx.simulate_click(confirm.center(), gpui::Modifiers::none());
    cx.run_until_parked();
    assert_eq!(
        std::fs::read_to_string(folder.join("docs/retries.md")).unwrap(),
        super::PLAN_MARKDOWN
    );
    assert!(cx.debug_bounds("thread-plan-save-path-i6").is_none());
    // Nothing went to T3.
    view.read_with(cx, |view, _| assert_eq!(view.error(), None));
    std::fs::remove_dir_all(folder).ok();
}

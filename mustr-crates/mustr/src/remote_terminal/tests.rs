use super::*;
use crate::{
    terminal_panel::{ThreadTerminalPanel, add_panel},
    worktrees::tests::{FakeBackend, RawCall, thread, workspace_with},
};
use futures::{FutureExt as _, future::BoxFuture, stream::BoxStream};
use gpui::{Action as _, ClipboardItem, TestAppContext, VisualTestContext};
use parking_lot::Mutex;
use pretty_assertions::assert_eq;
use serde_json::json;
use std::borrow::Cow;
use t3_client::RpcMethod as _;
use t3_client::t3_api::{
    tagged::{
        OrchestrationV2Command, OrchestrationV2ShellStreamItem, OrchestrationV2ThreadStreamItem,
    },
    types::{
        OrchestrationV2DispatchCommandResult, OrchestrationV2GetTurnItemInput,
        OrchestrationV2GetTurnItemResult, OrchestrationV2ShellSnapshot,
        OrchestrationV2SubscribeShellInput, OrchestrationV2SubscribeThreadInput,
        OrchestrationV2ThreadDetailSnapshot, OrchestrationV2ThreadLaunchInput,
        OrchestrationV2ThreadLaunchResult, ServerProvider,
    },
};
use thread_chat::{
    AddComposerContext, AttachmentUpload, ComposerContext, OpenThreadTerminal, ReadOnlyBackend,
};

#[test]
fn keys_send_what_a_local_terminal_would() {
    let key = |text: &str, mode| {
        keys::to_esc_str(&Keystroke::parse(text).unwrap(), mode, false).map(Cow::into_owned)
    };
    assert_eq!(key("up", Modes::NONE).as_deref(), Some("\x1b[A"));
    assert_eq!(key("up", Modes::APP_CURSOR).as_deref(), Some("\x1bOA"));
    assert_eq!(key("ctrl-c", Modes::NONE).as_deref(), Some("\x03"));
    assert_eq!(key("enter", Modes::NONE).as_deref(), Some("\r"));
    assert_eq!(key("shift-tab", Modes::NONE).as_deref(), Some("\x1b[Z"));
    // Plain text goes through the input handler instead.
    assert_eq!(key("a", Modes::NONE), None);
}

/// Every `terminal::SendText` in Zed's keymaps (`cmd-left`, `alt-b`, …) reads back exactly.
#[test]
fn send_text_bindings_read_back() {
    let keymaps = ["default-macos.json", "default-linux.json"].map(|name| {
        let path = format!("{}/../../assets/keymaps/{name}", env!("CARGO_MANIFEST_DIR"));
        std::fs::read_to_string(path).unwrap()
    });
    let mut texts = Vec::new();
    for keymap in &keymaps {
        let mut rest = keymap.as_str();
        while let Some(start) = rest.find("\"terminal::SendText\", ") {
            rest = &rest[start + "\"terminal::SendText\", ".len()..];
            let mut stream = serde_json::Deserializer::from_str(rest).into_iter::<String>();
            texts.push(stream.next().unwrap().unwrap());
        }
    }
    assert!(texts.len() > 5, "the keymaps bind SendText: {texts:?}");
    texts.extend(
        [
            "\"quoted\" \\ and 'single'",
            "日本語 ✓",
            "\n\r\t\0\u{7f}\u{200b}",
        ]
        .map(String::from),
    );
    for text in texts {
        let action = terminal_view::SendText::build(json!(text)).unwrap();
        let action = action
            .as_any()
            .downcast_ref::<terminal_view::SendText>()
            .unwrap();
        assert_eq!(action_text(action).as_deref(), Some(text.as_str()));
    }
    let keystroke = terminal_view::SendKeystroke::build(json!("ctrl-shift-home")).unwrap();
    let keystroke = keystroke
        .as_any()
        .downcast_ref::<terminal_view::SendKeystroke>()
        .unwrap();
    assert_eq!(action_text(keystroke).as_deref(), Some("ctrl-shift-home"));
}

#[test]
fn terminal_ids_follow_t3s() {
    assert_eq!(next_terminal_id([], None), "term-1");
    assert_eq!(next_terminal_id(["term-1", "term-3"], None), "term-2");
    assert_eq!(
        next_terminal_id(
            ["term-1", "term-2-0b4e5d6a-1111-4222-8333-444455556666"],
            None
        ),
        "term-3"
    );
    assert_eq!(next_terminal_id(["setup-dev", "term-x"], None), "term-1");
    assert_eq!(next_terminal_id(["term-1"], Some("abc")), "term-2-abc");
    assert_eq!(default_label("term-4"), "Terminal 4");
    assert_eq!(default_label("setup-dev"), "setup-dev");
}

#[test]
fn input_is_split_between_characters() {
    let data = "é".repeat(MAX_WRITE_BYTES);
    let chunks = write_chunks(&data);
    assert_eq!(chunks.concat(), data);
    assert!(chunks.iter().all(|chunk| chunk.len() <= MAX_WRITE_BYTES));
    assert_eq!(write_chunks(""), Vec::<&str>::new());
}

/// Plays T3's terminals over the worktrees page's fake: `terminal.attach` streams what the test
/// sends, `subscribeTerminalMetadata` lists `listed`, and other terminal calls succeed.
struct TerminalBackend {
    inner: Arc<FakeBackend>,
    listed: Mutex<Value>,
    calls: Mutex<Vec<RawCall>>,
    attaches: Mutex<Vec<(Value, mpsc::UnboundedSender<Result<Value>>)>>,
}

impl TerminalBackend {
    fn new(listed: Value) -> Arc<Self> {
        let inner = FakeBackend::new(
            vec![thread("a", Some("/wt/a")), thread("b", None)],
            Vec::new(),
        );
        Arc::new(Self {
            inner,
            listed: Mutex::new(listed),
            calls: Mutex::default(),
            attaches: Mutex::default(),
        })
    }

    fn calls(&self, tag: &str) -> Vec<Value> {
        self.calls
            .lock()
            .iter()
            .filter(|call| call.tag == tag)
            .map(|call| {
                assert_eq!(call.access, Access::TerminalOperate, "{tag}");
                call.payload.clone()
            })
            .collect()
    }

    fn attach_payloads(&self) -> Vec<Value> {
        self.attaches
            .lock()
            .iter()
            .map(|(payload, _)| payload.clone())
            .collect()
    }

    fn send(&self, attach: usize, event: Value) {
        self.attaches.lock()[attach]
            .1
            .unbounded_send(Ok(event))
            .unwrap();
    }

    /// What was typed, across every `terminal.write`.
    fn typed(&self) -> String {
        self.calls(rpc::TerminalWrite::TAG)
            .iter()
            .map(|write| {
                assert_eq!(write["threadId"], "a");
                assert_eq!(write["terminalId"], "term-1");
                write["data"].as_str().unwrap().to_string()
            })
            .collect()
    }
}

impl ThreadBackend for TerminalBackend {
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
        if !tag.starts_with("terminal.") {
            return self.inner.call_raw(tag, payload, access);
        }
        self.calls.lock().push(RawCall {
            tag: tag.to_string(),
            payload,
            access,
        });
        futures::future::ready(Ok(Value::Null)).boxed()
    }

    fn subscribe_raw(
        &self,
        tag: &str,
        payload: Value,
        access: Access,
    ) -> Result<BoxStream<'static, Result<Value>>> {
        if tag == rpc::TerminalAttach::TAG {
            assert_eq!(access, Access::TerminalOperate);
            let (tx, rx) = mpsc::unbounded();
            self.attaches.lock().push((payload, tx));
            return Ok(rx.boxed());
        }
        if tag == rpc::SubscribeTerminalMetadata::TAG {
            assert_eq!(access, Access::TerminalRead);
            let snapshot = json!({ "type": "snapshot", "terminals": self.listed.lock().clone() });
            return Ok(futures::stream::iter([Ok(snapshot)])
                .chain(futures::stream::pending())
                .boxed());
        }
        self.inner.subscribe_raw(tag, payload, access)
    }

    fn upload(&self, upload: AttachmentUpload) -> BoxFuture<'static, Result<()>> {
        self.inner.upload(upload)
    }
}

const NOW: &str = "2026-10-28T12:00:00.000Z";

fn snapshot(terminal_id: &str, history: &str, label: &str) -> Value {
    json!({
        "type": "snapshot",
        "snapshot": {
            "threadId": "a", "terminalId": terminal_id, "cwd": "/wt/a", "worktreePath": "/wt/a",
            "status": "running", "pid": 42, "history": history, "exitCode": null,
            "exitSignal": null, "label": label, "updatedAt": NOW
        }
    })
}

fn summary(thread_id: &str, terminal_id: &str, status: &str) -> Value {
    json!({
        "threadId": thread_id, "terminalId": terminal_id, "cwd": format!("/wt/{thread_id}/sub"),
        "worktreePath": format!("/wt/{thread_id}"), "status": status, "pid": 7, "exitCode": null,
        "exitSignal": null, "hasRunningSubprocess": false, "label": "", "updatedAt": NOW
    })
}

/// What T3's app sends to open thread a's first terminal: in its worktree, with T3's project
/// variables, starting the shell if T3 doesn't run one under that id.
fn first_attach() -> Value {
    json!({
        "threadId": "a", "terminalId": "term-1", "cwd": "/wt/a", "worktreePath": "/wt/a",
        "env": { "T3CODE_PROJECT_ROOT": "/src/api", "T3CODE_WORKTREE_PATH": "/wt/a" },
        "restartIfNotRunning": true
    })
}

async fn panel_for(
    backend: Arc<dyn ThreadBackend>,
    cx: &mut TestAppContext,
) -> (
    Entity<Workspace>,
    Entity<ThreadTerminalPanel>,
    &mut VisualTestContext,
) {
    let (workspace, cx) = workspace_with(backend, false, cx).await;
    let panel = workspace.update_in(cx, |workspace, window, cx| {
        let handle = cx.entity();
        add_panel(workspace, &handle, window, cx);
        workspace.panel::<ThreadTerminalPanel>(cx).unwrap()
    });
    let weak = workspace.downgrade();
    workspace.update_in(cx, |_, window, cx| {
        thread_chat::open_thread("a".into(), "Thread a".into(), weak, window, cx)
    });
    cx.run_until_parked();
    (workspace, panel, cx)
}

fn remote_terminals(
    panel: &Entity<ThreadTerminalPanel>,
    cx: &mut VisualTestContext,
) -> Vec<Entity<RemoteTerminal>> {
    panel.read_with(cx, |panel, _| panel.remote_terminals())
}

#[gpui::test]
async fn a_remote_thread_gets_t3s_terminal(cx: &mut TestAppContext) {
    let backend = TerminalBackend::new(json!([]));
    let (_workspace, panel, cx) = panel_for(backend.clone(), cx).await;

    cx.dispatch_action(OpenThreadTerminal {
        thread_id: "a".into(),
    });
    cx.run_until_parked();
    assert_eq!(backend.attach_payloads(), vec![first_attach()]);
    assert!(remote_terminals(&panel, cx).is_empty(), "it waits for T3");

    backend.send(0, snapshot("term-1", "hello from T3\r\n$ ", "Terminal 1"));
    cx.run_until_parked();
    let remote = remote_terminals(&panel, cx)
        .pop()
        .expect("the terminal shows");
    remote.read_with(cx, |remote, cx| {
        assert_eq!(remote.title(cx), "Terminal 1");
        let content = remote.terminal.read(cx).get_content();
        assert!(content.contains("hello from T3"), "{content:?}");
    });
    // The panel lays the terminal out, and T3 learns its size.
    cx.executor().advance_clock(RESIZE_DEBOUNCE * 2);
    cx.run_until_parked();
    let (cols, rows) = remote
        .read_with(cx, |remote, cx| remote.grid_size(cx))
        .expect("the terminal has a size");
    assert_eq!(
        backend.calls(rpc::TerminalResize::TAG),
        vec![json!({ "threadId": "a", "terminalId": "term-1", "cols": cols, "rows": rows })]
    );

    // Typing: text through the input handler, keys as their escape sequences.
    cx.simulate_keystrokes("l s enter");
    cx.run_until_parked();
    assert_eq!(backend.typed(), "ls\r");
    cx.simulate_keystrokes("up ctrl-c");
    cx.run_until_parked();
    assert_eq!(backend.typed(), "ls\r\x1b[A\x03");

    // Pastes end lines with returns, or are bracketed when the shell asks.
    cx.write_to_clipboard(ClipboardItem::new_string("echo hi\nls".into()));
    cx.dispatch_action(terminal::Paste);
    cx.run_until_parked();
    assert!(
        backend.typed().ends_with("echo hi\rls"),
        "{:?}",
        backend.typed()
    );
    backend.send(
        0,
        json!({ "type": "output", "threadId": "a", "terminalId": "term-1", "data": "\u{1b}[?2004h" }),
    );
    cx.run_until_parked();
    cx.dispatch_action(terminal::Paste);
    cx.run_until_parked();
    assert!(
        backend.typed().ends_with("\x1b[200~echo hi\nls\x1b[201~"),
        "{:?}",
        backend.typed()
    );

    // T3's label names what runs in it.
    backend.send(
        0,
        json!({
            "type": "activity", "threadId": "a", "terminalId": "term-1",
            "hasRunningSubprocess": true, "label": "npm run dev"
        }),
    );
    cx.run_until_parked();
    assert_eq!(
        remote.read_with(cx, |remote, cx| remote.title(cx)),
        "npm run dev"
    );

    // The shell exits: T3's app closes it, keeping its history, and its tab goes.
    backend.send(
        0,
        json!({
            "type": "exited", "threadId": "a", "terminalId": "term-1",
            "exitCode": 0, "exitSignal": null
        }),
    );
    cx.run_until_parked();
    assert_eq!(
        backend.calls(rpc::TerminalClose::TAG),
        vec![json!({ "threadId": "a", "terminalId": "term-1" })]
    );
    assert!(remote_terminals(&panel, cx).is_empty());
}

#[gpui::test]
async fn closing_a_remote_terminal_ends_it_and_its_history(cx: &mut TestAppContext) {
    let backend = TerminalBackend::new(json!([]));
    let (_workspace, panel, cx) = panel_for(backend.clone(), cx).await;
    cx.dispatch_action(OpenThreadTerminal {
        thread_id: "a".into(),
    });
    cx.run_until_parked();
    backend.send(0, snapshot("term-1", "$ ", "Terminal 1"));
    cx.run_until_parked();
    assert_eq!(remote_terminals(&panel, cx).len(), 1);

    cx.dispatch_action(crate::terminal_panel::CloseThreadTerminal);
    cx.run_until_parked();
    assert_eq!(
        backend.calls(rpc::TerminalClose::TAG),
        vec![json!({ "threadId": "a", "terminalId": "term-1", "deleteHistory": true })]
    );
    assert!(remote_terminals(&panel, cx).is_empty());
}

#[gpui::test]
async fn the_terminals_t3_still_runs_show_again(cx: &mut TestAppContext) {
    let backend = TerminalBackend::new(json!([
        summary("a", "term-3", "running"),
        summary("a", "term-1", "running"),
        summary("a", "term-2", "exited"),
        summary("b", "term-4", "running"),
    ]));
    let (_workspace, panel, cx) = panel_for(backend.clone(), cx).await;
    cx.dispatch_action(OpenThreadTerminal {
        thread_id: "a".into(),
    });
    cx.run_until_parked();
    // One at a time, each where it runs.
    let attach = |terminal_id: &str| {
        json!({
            "threadId": "a", "terminalId": terminal_id, "cwd": "/wt/a/sub",
            "worktreePath": "/wt/a",
            "env": { "T3CODE_PROJECT_ROOT": "/src/api", "T3CODE_WORKTREE_PATH": "/wt/a" },
            "restartIfNotRunning": true
        })
    };
    assert_eq!(backend.attach_payloads(), vec![attach("term-1")]);
    backend.send(0, snapshot("term-1", "one", "Terminal 1"));
    cx.run_until_parked();
    assert_eq!(backend.attach_payloads()[1], attach("term-3"));
    backend.send(1, snapshot("term-3", "three", "Terminal 3"));
    cx.run_until_parked();
    assert_eq!(remote_terminals(&panel, cx).len(), 2);

    // A new one takes the first number thread a doesn't use, exited ones included.
    cx.dispatch_action(crate::terminal_panel::NewThreadTerminal);
    cx.run_until_parked();
    let payloads = backend.attach_payloads();
    assert_eq!(payloads.len(), 3);
    assert_eq!(payloads[2]["terminalId"], "term-4");
    assert_eq!(payloads[2]["cwd"], "/wt/a");
}

#[gpui::test]
async fn a_dropped_connection_resumes_the_terminal(cx: &mut TestAppContext) {
    let backend = TerminalBackend::new(json!([]));
    let (_workspace, panel, cx) = panel_for(backend.clone(), cx).await;
    cx.dispatch_action(OpenThreadTerminal {
        thread_id: "a".into(),
    });
    cx.run_until_parked();
    backend.send(0, snapshot("term-1", "before", "Terminal 1"));
    cx.run_until_parked();
    cx.executor().advance_clock(RESIZE_DEBOUNCE * 2);
    cx.run_until_parked();
    let remote = remote_terminals(&panel, cx).pop().unwrap();
    let size = remote
        .read_with(cx, |remote, cx| remote.grid_size(cx))
        .unwrap();

    // The stream ends, as when the connection to T3 drops.
    backend.attaches.lock()[0].1.close_channel();
    cx.run_until_parked();
    cx.executor().advance_clock(reconnect_delay(0));
    cx.run_until_parked();
    assert_eq!(
        backend.attach_payloads()[1],
        json!({
            "threadId": "a", "terminalId": "term-1", "cwd": "/wt/a", "worktreePath": "/wt/a",
            "env": { "T3CODE_PROJECT_ROOT": "/src/api", "T3CODE_WORKTREE_PATH": "/wt/a" },
            "cols": size.0, "rows": size.1, "resume": true
        })
    );
    backend.send(1, snapshot("term-1", "after", "Terminal 1"));
    cx.run_until_parked();
    remote.read_with(cx, |remote, cx| {
        let content = remote.terminal.read(cx).get_content();
        assert!(
            content.contains("after") && !content.contains("before"),
            "{content:?}"
        );
    });
    assert_eq!(remote_terminals(&panel, cx).len(), 1);
}

#[gpui::test]
async fn dry_runs_open_no_terminal_on_t3(cx: &mut TestAppContext) {
    let backend = TerminalBackend::new(json!([]));
    let dry_run: Arc<dyn ThreadBackend> = Arc::new(ReadOnlyBackend::new(
        backend.clone(),
        "MUSTR_DRY_RUN_WRITES is set",
    ));
    let (_workspace, panel, cx) = panel_for(dry_run, cx).await;
    cx.dispatch_action(OpenThreadTerminal {
        thread_id: "a".into(),
    });
    cx.run_until_parked();
    assert!(backend.attach_payloads().is_empty());
    assert!(backend.calls.lock().is_empty());
    let error = panel.read_with(cx, |panel, _| panel.shown_error());
    assert_eq!(
        error.as_deref(),
        Some("Couldn't start a terminal on T3's machine: MUSTR_DRY_RUN_WRITES is set")
    );
}

#[test]
fn a_selection_becomes_t3s_terminal_chip() {
    use crate::terminal_panel::terminal_context;
    let context = terminal_context(
        "term-1".into(),
        "Terminal 1".into(),
        "\r\n$ cargo test\r\nok\r\n\r\n",
        (-2, 0),
        40,
        "abc",
    )
    .unwrap();
    assert_eq!(
        ComposerContext::Terminal(context).to_record().unwrap(),
        json!({
            "version": 1, "kind": "terminal", "contextId": "terminal_abc",
            "label": "Terminal 1 lines 39-41", "terminalId": "term-1",
            "terminalLabel": "Terminal 1", "lineStart": 39, "lineEnd": 41,
            "text": "$ cargo test\nok"
        })
    );
    let one = terminal_context("t".into(), "zsh".into(), "x", (3, 3), 0, "id").unwrap();
    assert_eq!((one.label.as_str(), one.line_start), ("zsh line 4", 4));
    assert_eq!(
        terminal_context("t".into(), "zsh".into(), "\n\n", (0, 1), 0, "id"),
        None
    );
}

#[gpui::test]
async fn add_to_chat_sends_the_selection_to_the_threads_composer(cx: &mut TestAppContext) {
    let backend = TerminalBackend::new(json!([]));
    let (workspace, panel, cx) = panel_for(backend.clone(), cx).await;
    let added = Arc::new(Mutex::new(Vec::new()));
    workspace.update(cx, |workspace, _| {
        let added = added.clone();
        workspace.register_action(move |_, action: &AddComposerContext, _, _| {
            added.lock().push(action.clone())
        });
    });
    cx.dispatch_action(OpenThreadTerminal {
        thread_id: "a".into(),
    });
    cx.run_until_parked();
    backend.send(
        0,
        snapshot("term-1", "$ cargo test\r\nok\r\n$ ", "Terminal 1"),
    );
    cx.run_until_parked();
    // Nothing selected, nothing added.
    cx.dispatch_action(crate::terminal_panel::AddTerminalSelectionToChat);
    cx.run_until_parked();
    assert!(added.lock().is_empty());

    let remote = remote_terminals(&panel, cx).pop().unwrap();
    remote.update(cx, |remote, cx| {
        remote
            .terminal
            .update(cx, |terminal, _| terminal.select_all());
        // The terminal takes its selection in when it's drawn next.
        remote.view.update(cx, |_, cx| cx.notify());
    });
    cx.run_until_parked();
    remote.read_with(cx, |remote, cx| {
        let content = remote.terminal.read(cx).last_content();
        assert!(
            content.selection.is_some(),
            "the terminal took the selection"
        );
    });
    cx.dispatch_action(crate::terminal_panel::AddTerminalSelectionToChat);
    cx.run_until_parked();
    let added = added.lock().clone();
    assert_eq!(added.len(), 1);
    let AddComposerContext { thread_id, context } = &added[0];
    assert_eq!(thread_id, "a");
    let ComposerContext::Terminal(context) = context else {
        panic!("a terminal chip: {context:?}");
    };
    assert!(context.context_id.starts_with("terminal_"));
    assert_eq!(
        (
            context.terminal_id.as_str(),
            context.terminal_label.as_str(),
            context.line_start,
        ),
        ("term-1", "Terminal 1", 1)
    );
    assert!(
        context.text.starts_with("$ cargo test\nok\n$"),
        "{:?}",
        context.text
    );
    assert!(
        context.label.starts_with("Terminal 1 lines 1-"),
        "{}",
        context.label
    );
}

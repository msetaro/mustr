//! Find in thread (`orchestration.searchThread`, or what's loaded on servers without it) and the
//! timeline's "new since your last visit" line, against the recording backend.

use super::*;
use crate::{
    find_bar::{ThreadMatch, occurrences},
    timeline::first_new_item,
};
use pretty_assertions::assert_eq;
use project::search::SearchQuery;
use util::paths::PathMatcher;
use workspace::searchable::{SearchToken, SearchableItem as _};

/// The recording backend, with a shell of its own and RPCs a server may lack.
struct Wrapped {
    inner: Arc<FakeBackend>,
    shell: Option<Value>,
    unsupported: Vec<&'static str>,
}

impl ThreadBackend for Wrapped {
    fn shell_snapshot(&self) -> BoxFuture<'static, Result<OrchestrationV2ShellSnapshot>> {
        match &self.shell {
            Some(shell) => ready(serde_json::from_value(shell.clone()).map_err(Into::into)),
            None => self.inner.shell_snapshot(),
        }
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
        if self.unsupported.contains(&tag) {
            self.inner.record_raw(tag, payload, access);
            let error = crate::backend::UnsupportedRpc { tag: tag.into() };
            return ready(Err(error.into()));
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
}

fn text_query(text: &str) -> Arc<SearchQuery> {
    Arc::new(
        SearchQuery::text(
            text,
            false,
            false,
            false,
            PathMatcher::default(),
            PathMatcher::default(),
            false,
            None,
        )
        .unwrap(),
    )
}

/// Finds `text` in the view as the search bar does, and shows the active match.
async fn find(
    view: &Entity<ThreadView>,
    text: &str,
    cx: &mut VisualTestContext,
) -> Vec<ThreadMatch> {
    let task = view.update_in(cx, |view, window, cx| {
        view.find_matches(text_query(text), window, cx)
    });
    cx.run_until_parked();
    let matches = task.await;
    view.update_in(cx, |view, window, cx| {
        let active = view.active_match_index(
            workspace::searchable::Direction::Next,
            &matches,
            SearchToken::default(),
            window,
            cx,
        );
        view.update_matches(&matches, active, SearchToken::default(), window, cx);
        if let Some(active) = active {
            view.activate_match(active, &matches, SearchToken::default(), window, cx);
        }
    });
    cx.run_until_parked();
    matches
}

fn search_call(fields: Value) -> RawCall {
    let mut payload = json!({ "threadId": THREAD, "query": "with" });
    payload
        .as_object_mut()
        .unwrap()
        .extend(fields.as_object().cloned().unwrap_or_default());
    RawCall {
        tag: "orchestration.searchThread".into(),
        payload,
        access: Access::Read,
    }
}

fn highlights(
    view: &Entity<ThreadView>,
    item_id: &str,
    cx: &mut VisualTestContext,
) -> Vec<Range<usize>> {
    view.read_with(cx, |view, cx| {
        view.markdown
            .get(item_id)
            .map(|markdown| markdown.read(cx).search_highlights().to_vec())
            .unwrap_or_default()
    })
}

use std::ops::Range;

// Find in thread.

#[test]
fn occurrences_are_found_as_t3_finds_them() {
    assert_eq!(
        occurrences("Retry, retry, RETRY", "retry"),
        [0..5, 7..12, 14..19]
    );
    // Without overlaps, and a final sigma as any other.
    assert_eq!(occurrences("aaaa", "aa"), [0..2, 2..4]);
    assert_eq!(occurrences("ΟΔΟΣ οδος", "οδοσ"), [0..8, 9..17]);
    assert_eq!(occurrences("", "x"), Vec::<Range<usize>>::new());
    assert_eq!(occurrences("x", ""), Vec::<Range<usize>>::new());
}

#[gpui::test]
async fn t3_finds_the_matches_and_the_timeline_shows_them(cx: &mut TestAppContext) {
    let backend = FakeBackend::with_thread(finished_thread(), 10);
    backend.raw_responses.lock().push(json!({
        "snapshotSequence": 10,
        "totalMatches": 2,
        "activeIndex": 0,
        "match": { "entryId": "message-i1", "runId": "run-1", "occurrence": 0 },
        "navigation": [
            { "entryId": "message-i1", "runId": "run-1", "startIndex": 0, "count": 1 },
            { "entryId": "message-i5", "runId": "run-1", "startIndex": 1, "count": 1 }
        ]
    }));
    let (view, cx) = open_view(backend.clone(), Some(THREAD), cx);
    cx.simulate_resize(gpui::size(gpui::px(1000.), gpui::px(900.)));

    let matches = find(&view, "with", cx).await;
    assert_eq!(*backend.raw_calls.lock(), [search_call(json!({}))]);
    assert_eq!(
        matches,
        [
            ThreadMatch {
                entry_id: "message-i1".into(),
                occurrence: 0,
                item_id: Some("i1".into()),
                range: Some(12..16),
            },
            ThreadMatch {
                entry_id: "message-i5".into(),
                occurrence: 0,
                item_id: Some("i5".into()),
                range: Some(7..11),
            },
        ]
    );
    // Each match is highlighted in its message's Markdown.
    assert_eq!(highlights(&view, "i1", cx), [12..16]);
    assert_eq!(highlights(&view, "i5", cx), [7..11]);

    // Closing the bar clears them.
    view.update_in(cx, |view, window, cx| {
        view.search_bar_visibility_changed(false, window, cx)
    });
    assert!(highlights(&view, "i1", cx).is_empty());
    assert!(view.read_with(cx, |view, _| view.find_matches_for_test().is_empty()));
}

#[gpui::test]
async fn t3s_pages_of_matches_are_read_until_they_cover_them_all(cx: &mut TestAppContext) {
    let backend = FakeBackend::with_thread(finished_thread(), 10);
    // Twenty entries of one match each: T3 lists the 17 around the one it picked.
    let page = |selected: usize| {
        let navigation = (0..17)
            .map(|offset| {
                let entry = (selected + offset + 20 - 8) % 20;
                json!({
                    "entryId": format!("entry-{entry}"), "runId": null,
                    "startIndex": entry, "count": 1
                })
            })
            .collect::<Vec<_>>();
        json!({
            "snapshotSequence": 10, "totalMatches": 20, "activeIndex": selected,
            "match": { "entryId": format!("entry-{selected}"), "runId": null, "occurrence": 0 },
            "navigation": navigation
        })
    };
    backend.raw_responses.lock().extend([page(0), page(9)]);
    let (view, cx) = open_view(backend.clone(), Some(THREAD), cx);

    let matches = find(&view, "with", cx).await;
    assert_eq!(
        *backend.raw_calls.lock(),
        // The first page covers entries 12 to 19 and 0 to 8, so one more, from match 9, is all.
        [search_call(json!({})), search_call(json!({ "index": 9 }))]
    );
    assert_eq!(
        matches
            .iter()
            .map(|found| found.entry_id.as_str())
            .collect::<Vec<_>>(),
        (0..20)
            .map(|entry| format!("entry-{entry}"))
            .collect::<Vec<_>>()
    );
    // Entries in history the timeline hasn't loaded still count; they just can't be shown.
    assert!(matches.iter().all(|found| found.item_id.is_none()));
}

#[gpui::test]
async fn without_searchthread_mustr_finds_in_whats_loaded(cx: &mut TestAppContext) {
    let inner = FakeBackend::with_thread(finished_thread(), 10);
    let backend = Arc::new(Wrapped {
        inner: inner.clone(),
        shell: None,
        unsupported: vec!["orchestration.searchThread"],
    });
    let (view, cx) = open_view(backend, Some(THREAD), cx);
    let matches = find(&view, "WITH", cx).await;
    assert_eq!(
        matches
            .iter()
            .map(|found| (found.item_id.as_deref(), found.range.clone()))
            .collect::<Vec<_>>(),
        [(Some("i1"), Some(12..16)), (Some("i5"), Some(7..11))]
    );
    // It doesn't ask again.
    find(&view, "retr", cx).await;
    assert_eq!(inner.raw_calls.lock().len(), 1);
}

#[gpui::test]
async fn mod_f_finds_in_a_started_thread(cx: &mut TestAppContext) {
    let bindings = crate::key_bindings();
    let find = bindings
        .iter()
        .find(|binding| binding.action().name() == "buffer_search::Deploy")
        .expect("mod-f is bound");
    assert_eq!(
        find.keystrokes()
            .iter()
            .map(|keystroke| keystroke.unparse())
            .collect::<Vec<_>>(),
        [if cfg!(target_os = "macos") {
            "cmd-f"
        } else {
            "ctrl-f"
        }]
    );

    let backend = FakeBackend::with_thread(finished_thread(), 10);
    let (view, cx) = open_view(backend, Some(THREAD), cx);
    assert!(cx.update(|_, cx| { crate::find_bar::searchable(view.read(cx), &view, cx).is_some() }));
    let draft = cx.new_window_entity(|window, cx| {
        ThreadView::new(
            Arc::new(FakeBackend::default()),
            None,
            None,
            "New".into(),
            window,
            cx,
        )
    });
    assert!(
        cx.update(|_, cx| { crate::find_bar::searchable(draft.read(cx), &draft, cx).is_none() })
    );
}

// The last-visit line.

/// A finished thread whose first run began at 11:00 and whose answer came at 13:00.
fn thread_with_times() -> Projection {
    let mut projection = finished_thread();
    for (item, started_at) in projection.items.iter_mut().zip([
        "2026-10-27T11:00:00.000Z",
        "2026-10-27T11:00:01.000Z",
        "2026-10-27T11:00:02.000Z",
        "2026-10-27T13:00:00.000Z",
        "2026-10-27T13:00:01.000Z",
    ]) {
        item["startedAt"] = json!(started_at);
    }
    projection
}

fn shell_visited(last_visited_at: Option<&str>) -> Value {
    let mut shell = shell_json();
    let thread = &mut shell["threads"][1];
    assert_eq!(thread["id"], THREAD);
    thread["lastVisitedAt"] = json!(last_visited_at);
    shell
}

#[test]
fn the_line_goes_above_the_first_row_with_news() {
    let state = ThreadState::new(thread_with_times().decode(), 10);
    let rows = state.rows();
    // Rows: the message, the work (starting at 11:00), the answer at 13:00.
    assert_eq!(
        first_new_item(&state, &rows, "2026-10-27T12:00:00.000Z").as_deref(),
        Some("i5")
    );
    // Nothing new, or everything new, draws no line.
    assert_eq!(first_new_item(&state, &rows, "2026-10-27T14:00:00Z"), None);
    assert_eq!(first_new_item(&state, &rows, "2026-10-27T10:00:00Z"), None);
    assert_eq!(first_new_item(&state, &rows, "not a time"), None);
}

type Opened<'a> = (
    Arc<FakeBackend>,
    Entity<ThreadView>,
    &'a mut VisualTestContext,
);

async fn open_visited<'a>(last_visited_at: Option<&str>, cx: &'a mut TestAppContext) -> Opened<'a> {
    open_visited_with_shell(last_visited_at, true, cx).await
}

/// Opens the thread with a shell that says when it was last visited, loaded before the view
/// opens or after.
async fn open_visited_with_shell<'a>(
    last_visited_at: Option<&str>,
    shell_loaded: bool,
    cx: &'a mut TestAppContext,
) -> Opened<'a> {
    let inner = FakeBackend::with_thread(thread_with_times(), 10);
    let backend: Arc<dyn ThreadBackend> = Arc::new(Wrapped {
        inner: inner.clone(),
        shell: Some(shell_visited(last_visited_at)),
        unsupported: Vec::new(),
    });
    init_test(cx);
    cx.update(|cx| {
        crate::set_backend(backend.clone(), cx);
        if shell_loaded {
            ThreadShells::global(cx);
        }
    });
    cx.run_until_parked();
    let (view, cx) = cx.add_window_view(|window, cx| {
        ThreadView::new(
            backend,
            None,
            Some(THREAD.to_string()),
            "Loading".into(),
            window,
            cx,
        )
    });
    cx.simulate_resize(gpui::size(gpui::px(1000.), gpui::px(2000.)));
    cx.run_until_parked();
    (inner, view, cx)
}

#[gpui::test]
async fn the_timeline_marks_whats_new_since_the_last_visit(cx: &mut TestAppContext) {
    let (backend, view, cx) = open_visited(Some("2026-10-27T12:00:00.000Z"), cx).await;
    assert_eq!(
        view.read_with(cx, |view, _| view.visit_line_item().map(str::to_string)),
        Some("i5".into())
    );
    let line = cx
        .debug_bounds("thread-visit-line")
        .expect("the line shows");
    let answer = cx.debug_bounds("thread-row-i5").unwrap();
    assert!(line.bottom() <= answer.top());

    // What arrives while the view is open doesn't move it.
    backend.push(event(
        11,
        "turn-item.updated",
        assistant_message("i9", 9, "run-2", "More", false),
    ));
    cx.run_until_parked();
    assert_eq!(
        view.read_with(cx, |view, _| view.visit_line_item().map(str::to_string)),
        Some("i5".into())
    );
}

#[gpui::test]
async fn a_thread_never_visited_has_no_line(cx: &mut TestAppContext) {
    let (_, view, cx) = open_visited(None, cx).await;
    assert_eq!(
        view.read_with(cx, |view, _| view.visit_line_item().map(str::to_string)),
        None
    );
    assert!(cx.debug_bounds("thread-visit-line").is_none());
}

#[gpui::test]
async fn a_view_opened_before_the_shell_loads_waits_for_it(cx: &mut TestAppContext) {
    // As at startup, with `--open-thread`: the shell loads after the view opens.
    let (_, view, cx) = open_visited_with_shell(Some("2026-10-27T12:00:00.000Z"), false, cx).await;
    assert_eq!(
        view.read_with(cx, |view, _| view.visit_line_item().map(str::to_string)),
        Some("i5".into())
    );
    assert!(cx.debug_bounds("thread-visit-line").is_some());
}

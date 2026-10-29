use super::*;
use crate::thread_git::tests::{
    Call, NOW, RecordingBackend, THREAD, WORKTREE, init_test, shell, shell_thread,
};
use gpui::{TestAppContext, VisualTestContext};
use pretty_assertions::assert_eq;
use serde_json::Value;
use workspace::MultiWorkspace;

fn thread_shell(extra: Value) -> OrchestrationV2ThreadShell {
    serde_json::from_value(shell_thread(extra)).unwrap()
}

fn link(number: i64, source: &str, state: &str, updated_at: &str, watching: bool) -> Value {
    json!({
        "host": "github.com", "repository": "acme/api", "number": number,
        "url": format!("https://github.com/acme/api/pull/{number}"),
        "source": source, "linkedAt": "2026-10-20T09:00:00.000Z", "stack": null,
        "snapshot": {
            "title": format!("PR {number}"), "state": state, "isDraft": false,
            "baseBranch": "main", "headBranch": format!("branch-{number}"),
            "syncedAt": NOW, "updatedAt": updated_at, "checksState": "failing"
        },
        "watch": if watching {
            json!({
                "startedAt": NOW, "remarksThrough": NOW, "headSha": null, "remarkIds": [], "failedChecks": [],
                "passed": false, "conflicting": false, "wakes": 0
            })
        } else {
            Value::Null
        }
    })
}

fn reference(number: i64, host: Option<&str>) -> PullRequestRef {
    PullRequestRef {
        allow_stale: None,
        expected_account_id: None,
        host: host.map(str::to_string),
        number,
        project_id: "project-1".to_string(),
        repository: "acme/api".to_string(),
    }
}

#[test]
fn the_thread_shows_the_pull_request_t3_would() {
    // An open linked pull request wins over newer closed ones and dismissed ones.
    let thread = thread_shell(json!({
        "pullRequests": [
            link(3, "manual", "merged", "2026-10-27T09:00:00.000Z", false),
            link(4, "created", "open", "2026-10-25T09:00:00.000Z", true),
            link(5, "stack-dismissed", "open", "2026-10-28T09:00:00.000Z", false),
        ],
        "branchPullRequest": {
            "number": 9, "projectId": "project-1", "repository": "acme/api",
            "url": "https://github.com/acme/api/pull/9"
        }
    }));
    let pull_request = thread_pull_request(&thread).unwrap();
    assert_eq!(pull_request.reference, reference(4, Some("github.com")));
    assert_eq!(
        pull_request.origin,
        PullRequestOrigin::Linked(ThreadPullRequestLinkSource::Created)
    );
    assert!(pull_request.watching && pull_request.is_linked());
    assert_eq!(pull_request.state, Some(PullRequestState::Open));
    assert_eq!(pull_request.checks, Some(PullRequestChecksState::Failing));
    assert_eq!(pull_request.others, 1);

    // With none open, the latest one.
    let thread = thread_shell(json!({
        "pullRequests": [
            link(3, "manual", "merged", "2026-10-27T09:00:00.000Z", false),
            link(6, "agent", "closed", "2026-10-21T09:00:00.000Z", false),
        ]
    }));
    assert_eq!(thread_pull_request(&thread).unwrap().reference.number, 3);

    // Then the thread's own field, then its branch's pull request.
    let linked = json!({
        "number": 8, "projectId": "project-1", "repository": "acme/api",
        "url": "https://github.com/acme/api/pull/8"
    });
    let branch = json!({
        "number": 9, "projectId": "project-1", "repository": "acme/api",
        "url": "https://github.com/acme/api/pull/9"
    });
    let thread = thread_shell(json!({ "linkedPullRequest": linked, "branchPullRequest": branch }));
    let pull_request = thread_pull_request(&thread).unwrap();
    assert_eq!(pull_request.reference, reference(8, Some("github.com")));
    assert_eq!(pull_request.origin, PullRequestOrigin::LinkedField);
    assert!(!pull_request.is_linked());
    let thread = thread_shell(json!({ "branchPullRequest": branch }));
    assert_eq!(
        thread_pull_request(&thread).unwrap().origin,
        PullRequestOrigin::Branch
    );
    assert_eq!(thread_pull_request(&thread_shell(json!({}))), None);
}

#[test]
fn pull_request_urls_parse_for_each_forge() {
    let parsed = |url| parse_pull_request_url(url);
    assert_eq!(
        parsed("https://github.com/acme/api/pull/42/files?w=1#diff"),
        Some(("github.com".to_string(), "acme/api".to_string(), 42))
    );
    assert_eq!(
        parsed("https://gitlab.example.com/group/sub/api/-/merge_requests/7"),
        Some((
            "gitlab.example.com".to_string(),
            "group/sub/api".to_string(),
            7
        ))
    );
    assert_eq!(
        parsed("https://bitbucket.org/acme/api/pull-requests/3"),
        Some(("bitbucket.org".to_string(), "acme/api".to_string(), 3))
    );
    assert_eq!(parsed("https://github.com/acme/api/issues/42"), None);
    assert_eq!(parsed("https://github.com/api/pull/42"), None);
    assert_eq!(parsed("ftp://github.com/acme/api/pull/42"), None);
}

#[test]
fn link_input_takes_urls_numbers_and_checkout_commands() {
    assert_eq!(
        parse_link_input(" https://github.com/acme/api/pull/42 ").unwrap(),
        LinkInput::Url {
            host: "github.com".to_string(),
            repository: "acme/api".to_string(),
            number: 42,
            url: "https://github.com/acme/api/pull/42".to_string(),
        }
    );
    for input in ["42", "#42", "gh pr checkout 42"] {
        assert_eq!(
            parse_link_input(input).unwrap(),
            LinkInput::Reference("42".to_string()),
            "{input}"
        );
    }
    for input in ["", "fix-the-bug", "#0", "https://github.com/acme/api"] {
        assert!(parse_link_input(input).is_err(), "{input}");
    }
}

#[test]
fn link_unlink_and_watch_send_t3s_commands() {
    let pull_request = ThreadPullRequest::for_tests(
        reference(42, Some("github.com")),
        "https://github.com/acme/api/pull/42",
        PullRequestOrigin::Linked(ThreadPullRequestLinkSource::Manual),
    );
    let json = |command| serde_json::to_value(command).unwrap();
    assert_eq!(
        json(link_command(
            THREAD,
            "github.com".to_string(),
            "acme/api".to_string(),
            42,
            "https://github.com/acme/api/pull/42".to_string(),
            "command-1".to_string()
        )),
        json!({
            "type": "thread.pull-request.link", "commandId": "command-1", "threadId": THREAD,
            "host": "github.com", "repository": "acme/api", "number": 42,
            "url": "https://github.com/acme/api/pull/42", "source": "manual"
        })
    );
    assert_eq!(
        json(unlink_command(
            THREAD,
            &pull_request,
            "command-2".to_string()
        )),
        json!({
            "type": "thread.pull-request.unlink", "commandId": "command-2", "threadId": THREAD,
            "host": "github.com", "repository": "acme/api", "number": 42
        })
    );
    assert_eq!(
        json(watch_command(
            THREAD,
            &pull_request,
            true,
            "command-3".to_string()
        )),
        json!({
            "type": "thread.pull-request.watch", "commandId": "command-3", "threadId": THREAD,
            "host": "github.com", "repository": "acme/api", "number": 42, "watching": true
        })
    );
    // A pull request known only by its URL gets the URL's host.
    let branch = ThreadPullRequest::for_tests(
        reference(9, None),
        "https://github.example.com/acme/api/pull/9",
        PullRequestOrigin::Branch,
    );
    assert_eq!(
        json(watch_command(
            THREAD,
            &branch,
            false,
            "command-4".to_string()
        ))["host"],
        json!("github.example.com")
    );
}

#[test]
fn review_changes_knows_the_thread_from_the_shell() {
    let shell = Shell::from_snapshot(
        &serde_json::from_value(shell(vec![shell_thread(json!({}))])).unwrap(),
    );
    let thread = agent_thread(&shell, THREAD).unwrap();
    assert_eq!(thread.title, "Add retries");
    assert_eq!(thread.project.as_deref(), Some("api"));
    assert_eq!(thread.worktree_path.as_deref(), Some(WORKTREE));
    assert_eq!(thread.agent.as_deref(), Some("Claude · claude-opus-5-5"));
    assert_eq!(agent_thread(&shell, "missing"), None);
}

fn summary(number: i64) -> Value {
    json!({
        "projectId": "project-1", "provider": "github", "repository": "acme/api",
        "number": number, "title": "Retry webhooks", "url": "https://github.com/acme/api/pull/4",
        "state": "open", "isDraft": true, "baseBranch": "main", "headBranch": "branch-4",
        "updatedAt": NOW, "checksState": "pending"
    })
}

#[gpui::test]
async fn the_pull_request_loads_its_summary_and_checks(cx: &mut TestAppContext) {
    let backend = RecordingBackend::with_shell(shell(vec![shell_thread(json!({
        "pullRequests": [link(4, "created", "open", NOW, false)]
    }))]));
    backend.respond("pullRequests.summary", Ok(summary(4)));
    backend.respond(
        "pullRequests.checks",
        Ok(json!({
            "state": "open",
            "checks": [
                { "name": "build", "status": "success", "description": null, "url": null },
                { "name": "lint", "status": "failure", "description": null, "url": null },
                { "name": "test", "status": "pending", "description": null, "url": null }
            ]
        })),
    );
    init_test(backend.clone(), cx);
    let status = cx.new(|cx| PullRequestStatus::new(THREAD.to_string(), cx));
    cx.run_until_parked();
    let expected_ref = json!({
        "projectId": "project-1", "host": "github.com", "repository": "acme/api", "number": 4
    });
    assert_eq!(
        backend
            .calls
            .lock()
            .iter()
            .filter(|call| call.tag.starts_with("pullRequests."))
            .cloned()
            .collect::<Vec<_>>(),
        [
            Call {
                tag: "pullRequests.summary".to_string(),
                payload: expected_ref.clone(),
                access: Access::Read,
            },
            Call {
                tag: "pullRequests.checks".to_string(),
                payload: expected_ref,
                access: Access::Read,
            },
        ]
    );
    status.read_with(cx, |status, _| {
        assert_eq!(status.state(), Some((PullRequestState::Open, true)));
        assert_eq!(status.title().as_deref(), Some("Retry webhooks"));
        assert_eq!(
            status.checks(),
            Some((
                PullRequestChecksState::Failing,
                "1 of 3 checks failing".to_string()
            ))
        );
    });

    status.update(cx, |status, cx| status.set_watching(true, cx));
    status.update(cx, |status, cx| status.unlink(cx));
    cx.run_until_parked();
    let commands = backend.commands();
    assert_eq!(
        commands
            .iter()
            .map(|command| (command["type"].clone(), command["number"].clone()))
            .collect::<Vec<_>>(),
        [
            (json!("thread.pull-request.watch"), json!(4)),
            (json!("thread.pull-request.unlink"), json!(4)),
        ]
    );
    assert_eq!(commands[0]["watching"], json!(true));
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

#[gpui::test]
async fn numbers_link_after_t3_finds_them_in_the_checkout(cx: &mut TestAppContext) {
    let backend = RecordingBackend::with_shell(shell(vec![shell_thread(json!({}))]));
    backend.respond(
        "git.resolvePullRequest",
        Ok(json!({
            "pullRequest": {
                "number": 42, "title": "Retry webhooks", "url": "https://github.com/acme/api/pull/42",
                "baseBranch": "main", "headBranch": "retry", "state": "open"
            }
        })),
    );
    init_test(backend.clone(), cx);
    let (workspace, cx) = workspace(cx).await;
    let git = cx.update(|_, cx| ThreadGit::for_thread(THREAD, cx));
    cx.run_until_parked();
    workspace.update_in(cx, |workspace, window, cx| {
        workspace.toggle_modal(window, cx, |window, cx| {
            LinkPullRequestDialog::new(THREAD.to_string(), git.clone(), window, cx)
        })
    });
    let dialog = workspace
        .read_with(cx, |workspace, cx| {
            workspace.active_modal::<LinkPullRequestDialog>(cx)
        })
        .expect("the dialog is open");

    // A number that isn't one says so.
    dialog.update_in(cx, |dialog, window, cx| {
        dialog
            .editor
            .update(cx, |editor, cx| editor.set_text("the bug", window, cx));
        dialog.link(cx);
    });
    dialog.read_with(cx, |dialog, _| {
        assert_eq!(
            dialog.error.as_deref(),
            Some("Use a pull request URL, a checkout command, 123, or #123.")
        )
    });

    dialog.update_in(cx, |dialog, window, cx| {
        dialog
            .editor
            .update(cx, |editor, cx| editor.set_text("#42", window, cx));
        dialog.link(cx);
    });
    cx.run_until_parked();
    assert_eq!(
        backend.calls_to("git.resolvePullRequest"),
        [Call {
            tag: "git.resolvePullRequest".to_string(),
            payload: json!({ "cwd": WORKTREE, "reference": "42" }),
            access: Access::Read,
        }]
    );
    let commands = backend.commands();
    assert_eq!(
        commands,
        [json!({
            "type": "thread.pull-request.link", "commandId": commands[0]["commandId"],
            "threadId": THREAD, "host": "github.com", "repository": "acme/api", "number": 42,
            "url": "https://github.com/acme/api/pull/42", "source": "manual"
        })]
    );
    // Linked, the dialog closes.
    assert!(
        workspace
            .read_with(cx, |workspace, cx| workspace
                .active_modal::<LinkPullRequestDialog>(cx))
            .is_none()
    );
}

#[gpui::test]
async fn the_toolbar_shows_over_threads_and_opens_their_editor(cx: &mut TestAppContext) {
    let backend = RecordingBackend::with_shell(shell(vec![shell_thread(json!({}))]));
    backend.respond(
        "server.getConfig",
        Ok(json!({ "availableEditors": ["zed"] })),
    );
    init_test(backend.clone(), cx);
    cx.update(init);
    let (workspace, cx) = workspace(cx).await;
    let toolbar =
        cx.new_window_entity(|window, cx| ThreadToolbar::new(workspace.downgrade(), window, cx));
    let view = cx.new_window_entity(|window, cx| {
        ThreadView::new(
            backend.clone(),
            None,
            Some(THREAD.to_string()),
            "Add retries".into(),
            window,
            cx,
        )
    });
    let draft = cx.new_window_entity(|window, cx| {
        ThreadView::new(backend.clone(), None, None, "New thread".into(), window, cx)
    });
    let location = toolbar.update_in(cx, |toolbar, window, cx| {
        toolbar.set_active_pane_item(Some(&view), window, cx)
    });
    assert_eq!(location, ToolbarItemLocation::PrimaryLeft);
    toolbar.read_with(cx, |toolbar, _| {
        assert_eq!(toolbar.thread_id(), Some(THREAD))
    });
    let location = toolbar.update_in(cx, |toolbar, window, cx| {
        toolbar.set_active_pane_item(Some(&draft), window, cx)
    });
    assert_eq!(location, ToolbarItemLocation::Hidden);
    let location = toolbar.update_in(cx, |toolbar, window, cx| {
        toolbar.set_active_pane_item(None, window, cx)
    });
    assert_eq!(location, ToolbarItemLocation::Hidden);

    // Open in editor from anywhere opens the thread's checkout in T3's editor.
    cx.update(|_, cx| crate::thread_git::Editors::global(cx));
    cx.run_until_parked();
    workspace.update_in(cx, |workspace, window, cx| {
        window.focus(&workspace.active_pane().focus_handle(cx), cx);
    });
    workspace.update_in(cx, |_, window, cx| {
        window.dispatch_action(
            Box::new(OpenInEditor {
                thread_id: THREAD.to_string(),
            }),
            cx,
        )
    });
    cx.run_until_parked();
    assert_eq!(
        backend.calls_to("shell.openInEditor"),
        [Call {
            tag: "shell.openInEditor".to_string(),
            payload: json!({ "cwd": WORKTREE, "editor": "zed" }),
            access: Access::Operate,
        }]
    );
}

/// The toolbar's keys reach it from the composer and the thread view, over Zed's own bindings
/// (`editor::SelectNext`, `workspace::Open`, the git panel), with any base keymap and in vim mode.
#[gpui::test]
fn the_toolbar_keys_beat_zeds_in_a_thread(cx: &mut TestAppContext) {
    use gpui::{Action as _, KeyContext, Keystroke, UpdateGlobal as _};
    use settings::{BaseKeymapContent, SettingsStore};
    cx.update(|cx| {
        let settings_store = SettingsStore::test(cx);
        cx.set_global(settings_store);
        crate::app_settings::apply_default_overrides(cx).unwrap();
        let contexts = |names: &[&str]| {
            names
                .iter()
                .map(|name| KeyContext::parse(name).unwrap())
                .collect::<Vec<_>>()
        };
        let in_view = contexts(&["Workspace", "Pane", "ThreadView"]);
        let in_composer = contexts(&[
            "Workspace",
            "Pane",
            "ThreadView",
            "ThreadComposer",
            "Editor mode=auto_height",
        ]);
        for base_keymap in [BaseKeymapContent::Zed, BaseKeymapContent::VSCode] {
            for vim_mode in [false, true] {
                SettingsStore::update_global(cx, |store, cx| {
                    store.update_user_settings(cx, |settings| {
                        settings.base_keymap = Some(base_keymap);
                        settings.vim_mode = Some(vim_mode);
                    });
                });
                cx.clear_key_bindings();
                crate::keymap::load_default_keymap(cx);
                for (keys, action) in [
                    ("secondary-d", ReviewChanges.name()),
                    ("secondary-o", OpenThreadInEditor.name()),
                    ("secondary-shift-g", SwitchThreadBranch.name()),
                ] {
                    let keystroke = Keystroke::parse(keys).unwrap();
                    for (where_, stack) in [("view", &in_view), ("composer", &in_composer)] {
                        let (bindings, _) = cx
                            .key_bindings()
                            .borrow()
                            .bindings_for_input(std::slice::from_ref(&keystroke), stack);
                        assert_eq!(
                            bindings.first().map(|binding| binding.action().name()),
                            Some(action),
                            "{keys} in the {where_} ({base_keymap:?}, vim {vim_mode})"
                        );
                    }
                }
            }
        }
    });
}

#[gpui::test]
async fn the_palette_offers_the_toolbar_on_the_focused_thread(cx: &mut TestAppContext) {
    let backend = RecordingBackend::with_shell(shell(vec![shell_thread(json!({}))]));
    backend.respond(
        "server.getConfig",
        Ok(json!({ "availableEditors": ["zed"] })),
    );
    init_test(backend.clone(), cx);
    cx.update(init);
    let (workspace, cx) = workspace(cx).await;
    cx.update(|_, cx| crate::thread_git::Editors::global(cx));
    cx.run_until_parked();
    let available = |cx: &mut VisualTestContext| {
        workspace.update_in(cx, |workspace, window, cx| {
            window.focus(&workspace.active_pane().focus_handle(cx), cx);
            [
                window.is_action_available(&ReviewChanges, cx),
                window.is_action_available(&OpenThreadInEditor, cx),
                window.is_action_available(&SwitchThreadBranch, cx),
                window.is_action_available(&CommitPushAndPr, cx),
                window.is_action_available(&LinkThreadPullRequest, cx),
            ]
        })
    };
    cx.run_until_parked();
    assert_eq!(available(cx), [false; 5], "no thread is open");

    let weak = workspace.downgrade();
    workspace.update_in(cx, |_, window, cx| {
        thread_chat::open_thread(THREAD.to_string(), "Add retries".into(), weak, window, cx)
    });
    cx.run_until_parked();
    assert_eq!(available(cx), [true; 5]);

    cx.dispatch_action(OpenThreadInEditor);
    cx.run_until_parked();
    assert_eq!(
        backend.calls_to("shell.openInEditor"),
        [Call {
            tag: "shell.openInEditor".to_string(),
            payload: json!({ "cwd": WORKTREE, "editor": "zed" }),
            access: Access::Operate,
        }]
    );
    cx.dispatch_action(LinkThreadPullRequest);
    cx.run_until_parked();
    assert!(
        workspace
            .read_with(cx, |workspace, cx| workspace
                .active_modal::<LinkPullRequestDialog>(cx))
            .is_some()
    );
}

/// The toolbar's items that show at `width`, after the toolbar has measured itself, at mustr's
/// default size (26), and what its "…" menu offers.
fn toolbar_at(
    toolbar: &Entity<ThreadToolbar>,
    width: f32,
    cx: &mut VisualTestContext,
) -> (Vec<&'static str>, Vec<OverflowEntry>) {
    cx.simulate_resize(gpui::size(px(width), px(200.)));
    cx.run_until_parked();
    let keys = [
        ToolbarItem::Branch,
        ToolbarItem::GitSummary,
        ToolbarItem::PullRequest,
        ToolbarItem::Checks,
        ToolbarItem::LinkPullRequest,
        ToolbarItem::ReviewChanges,
        ToolbarItem::MergeBack,
        ToolbarItem::Servers,
        ToolbarItem::Terminal,
        ToolbarItem::OpenIn,
        ToolbarItem::GitAction,
    ]
    .map(ToolbarItem::key);
    let mut shown = Vec::new();
    for key in keys.into_iter().chain(["thread-toolbar-more"]) {
        if let Some(bounds) = cx.debug_bounds(key) {
            assert!(
                bounds.right() <= px(width) + px(0.5),
                "{key} sticks out of the toolbar at {width}: {bounds:?}"
            );
            shown.push(key);
        }
    }
    let entries = toolbar.update(cx, |toolbar, cx| {
        let merge = toolbar
            .thread
            .as_ref()
            .and_then(|thread| toolbar.merge_back(thread, cx));
        toolbar.overflow_entries(merge.as_ref(), cx)
    });
    (shown, entries)
}

/// In a MacBook-sized window at mustr's default size, the toolbar doesn't fit: Link PR,
/// Terminal, and Open in go into its "…" menu first, then the git status, while Review changes
/// and Commit stay, and the branch shrinks.
#[gpui::test]
async fn the_toolbar_moves_what_doesnt_fit_into_its_menu(cx: &mut TestAppContext) {
    let backend = RecordingBackend::with_shell(shell(vec![shell_thread(json!({}))]));
    backend.respond(
        "server.getConfig",
        Ok(json!({ "availableEditors": ["vscode", "zed"] })),
    );
    init_test(backend.clone(), cx);
    cx.update(init);
    let (workspace, cx) = workspace(cx).await;
    let view = cx.new_window_entity(|window, cx| {
        ThreadView::new(
            backend.clone(),
            None,
            Some(THREAD.to_string()),
            "Add retries".into(),
            window,
            cx,
        )
    });
    let (toolbar, cx) =
        cx.add_window_view(|window, cx| ThreadToolbar::new(workspace.downgrade(), window, cx));
    cx.update(|window, cx| {
        window.set_rem_size(px(26.));
        crate::thread_git::Editors::global(cx);
    });
    toolbar.update_in(cx, |toolbar, window, cx| {
        toolbar.set_active_pane_item(Some(&view), window, cx)
    });
    cx.run_until_parked();

    // Room for everything.
    let (shown, entries) = toolbar_at(&toolbar, 3000., cx);
    assert_eq!(
        shown,
        [
            "thread-toolbar-branch",
            "thread-toolbar-git-summary",
            "thread-toolbar-link-pull-request",
            "thread-toolbar-review-changes",
            "thread-toolbar-terminal",
            "thread-toolbar-open-in",
            "thread-toolbar-git-action",
        ]
    );
    assert_eq!(entries, []);

    // A 14" MacBook's window with the threads beside the thread: the actions that matter least
    // go into the menu. Terminal is narrow enough to stay where Open in isn't.
    let (shown, entries) = toolbar_at(&toolbar, 1000., cx);
    assert_eq!(
        shown,
        [
            "thread-toolbar-branch",
            "thread-toolbar-git-summary",
            "thread-toolbar-review-changes",
            "thread-toolbar-terminal",
            "thread-toolbar-git-action",
            "thread-toolbar-more",
        ]
    );
    // In the toolbar's order.
    assert_eq!(
        entries,
        [OverflowEntry::LinkPullRequest, OverflowEntry::OpenIn]
    );

    // Narrower still, the git status goes too, and says what it is in the menu.
    let (shown, entries) = toolbar_at(&toolbar, 700., cx);
    assert_eq!(
        shown,
        [
            "thread-toolbar-branch",
            "thread-toolbar-review-changes",
            "thread-toolbar-git-action",
            "thread-toolbar-more",
        ]
    );
    assert!(matches!(entries[0], OverflowEntry::Text(_)), "{entries:?}");
    assert_eq!(
        entries[1..],
        [
            OverflowEntry::LinkPullRequest,
            OverflowEntry::Terminal,
            OverflowEntry::OpenIn,
        ]
    );

    // Wider again, they come back.
    let (shown, _) = toolbar_at(&toolbar, 3000., cx);
    assert!(shown.contains(&"thread-toolbar-terminal"));
    assert!(!shown.contains(&"thread-toolbar-more"));
}

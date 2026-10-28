use super::*;
use anyhow::bail;
use futures::{
    channel::mpsc,
    stream::{self, BoxStream},
};
use gpui::TestAppContext;
use parking_lot::Mutex;
use pretty_assertions::assert_eq;
use serde_json::{Value, json};
use std::collections::VecDeque;
use t3_client::t3_api::{
    tagged::{OrchestrationV2ShellStreamItem, OrchestrationV2ThreadStreamItem},
    types::{
        OrchestrationV2DispatchCommandResult, OrchestrationV2GetTurnItemInput,
        OrchestrationV2GetTurnItemResult, OrchestrationV2ShellSnapshot,
        OrchestrationV2SubscribeShellInput, OrchestrationV2SubscribeThreadInput,
        OrchestrationV2ThreadDetailSnapshot, OrchestrationV2ThreadLaunchInput,
        OrchestrationV2ThreadLaunchResult, ServerProvider,
    },
};
use thread_chat::AttachmentUpload;

pub(crate) const THREAD: &str = "thread-1";
pub(crate) const NOW: &str = "2026-10-28T09:00:00.000Z";
pub(crate) const WORKTREE: &str = "/tmp/worktrees/retry-backoff";

/// One call or subscription the app made, with the scope it asked for.
#[derive(Clone, Debug, PartialEq)]
pub(crate) struct Call {
    pub tag: String,
    pub payload: Value,
    pub access: Access,
}

/// Plays T3 for the thread toolbar's tests: records every call, subscription, and command with
/// the access it was made with, and answers with scripted responses.
#[derive(Default)]
pub(crate) struct RecordingBackend {
    pub shell: Mutex<Option<Value>>,
    pub calls: Mutex<Vec<Call>>,
    pub commands: Mutex<Vec<Value>>,
    /// What `call_raw` answers for a tag, in order; `null` once they run out.
    responses: Mutex<HashMap<String, VecDeque<Result<Value, String>>>>,
    /// What the next subscription to a tag yields, and whether it then stays open.
    streams: Mutex<HashMap<String, VecDeque<(Vec<Value>, bool)>>>,
    /// Open subscriptions, for pushing more items.
    senders: Mutex<HashMap<String, Vec<mpsc::UnboundedSender<Result<Value>>>>>,
    /// Calls refused before they're sent, by access.
    pub blocked: Mutex<HashMap<Access, String>>,
}

impl RecordingBackend {
    pub fn with_shell(shell: Value) -> Arc<Self> {
        let backend = Self::default();
        *backend.shell.lock() = Some(shell);
        Arc::new(backend)
    }

    pub fn respond(&self, tag: &str, response: Result<Value, &str>) {
        self.responses
            .lock()
            .entry(tag.to_string())
            .or_default()
            .push_back(response.map_err(str::to_string));
    }

    pub fn script_stream(&self, tag: &str, items: Vec<Value>, stays_open: bool) {
        self.streams
            .lock()
            .entry(tag.to_string())
            .or_default()
            .push_back((items, stays_open));
    }

    /// Sends an item to every open subscription to `tag`.
    pub fn push(&self, tag: &str, item: Value) {
        for sender in self.senders.lock().get(tag).into_iter().flatten() {
            sender.unbounded_send(Ok(item.clone())).ok();
        }
    }

    pub fn calls_to(&self, tag: &str) -> Vec<Call> {
        self.calls
            .lock()
            .iter()
            .filter(|call| call.tag == tag)
            .cloned()
            .collect()
    }

    pub fn commands(&self) -> Vec<Value> {
        self.commands.lock().clone()
    }

    fn record(&self, tag: &str, payload: Value, access: Access) {
        self.calls.lock().push(Call {
            tag: tag.to_string(),
            payload,
            access,
        });
    }
}

fn ready<T: Send + 'static>(value: Result<T>) -> BoxFuture<'static, Result<T>> {
    futures::future::ready(value).boxed()
}

impl ThreadBackend for RecordingBackend {
    fn shell_snapshot(&self) -> BoxFuture<'static, Result<OrchestrationV2ShellSnapshot>> {
        let shell = self.shell.lock().clone();
        ready(match shell {
            Some(shell) => serde_json::from_value(shell).map_err(Into::into),
            None => Err(anyhow!("no shell")),
        })
    }

    fn subscribe_shell(
        &self,
        _: OrchestrationV2SubscribeShellInput,
    ) -> Result<BoxStream<'static, Result<OrchestrationV2ShellStreamItem>>> {
        Ok(stream::pending().boxed())
    }

    fn thread_snapshot(
        &self,
        _: String,
    ) -> BoxFuture<'static, Result<OrchestrationV2ThreadDetailSnapshot>> {
        ready(Err(anyhow!("the tests don't load threads")))
    }

    fn subscribe_thread(
        &self,
        _: OrchestrationV2SubscribeThreadInput,
    ) -> Result<BoxStream<'static, Result<OrchestrationV2ThreadStreamItem>>> {
        Ok(stream::pending().boxed())
    }

    fn turn_item(
        &self,
        _: OrchestrationV2GetTurnItemInput,
    ) -> BoxFuture<'static, Result<OrchestrationV2GetTurnItemResult>> {
        ready(Err(anyhow!("the tests don't load turn items")))
    }

    fn providers(&self) -> BoxFuture<'static, Result<Vec<ServerProvider>>> {
        ready(Ok(Vec::new()))
    }

    fn dispatch(
        &self,
        command: OrchestrationV2Command,
    ) -> BoxFuture<'static, Result<OrchestrationV2DispatchCommandResult>> {
        if let Some(reason) = self.blocked_reason(Access::Operate) {
            return ready(Err(anyhow!(reason)));
        }
        self.commands
            .lock()
            .push(serde_json::to_value(&command).unwrap());
        ready(serde_json::from_value(json!({ "sequence": 1 })).map_err(anyhow::Error::from))
    }

    fn launch_thread(
        &self,
        _: OrchestrationV2ThreadLaunchInput,
    ) -> BoxFuture<'static, Result<OrchestrationV2ThreadLaunchResult>> {
        ready(Err(anyhow!("the tests launch no threads")))
    }

    fn call_raw(
        &self,
        tag: &str,
        payload: Value,
        access: Access,
    ) -> BoxFuture<'static, Result<Value>> {
        if let Some(reason) = self.blocked_reason(access) {
            return ready(Err(anyhow!(reason)));
        }
        self.record(tag, payload, access);
        let response = self
            .responses
            .lock()
            .get_mut(tag)
            .and_then(VecDeque::pop_front)
            .unwrap_or(Ok(Value::Null));
        ready(response.map_err(|error| anyhow!(error)))
    }

    fn subscribe_raw(
        &self,
        tag: &str,
        payload: Value,
        access: Access,
    ) -> Result<BoxStream<'static, Result<Value>>> {
        if let Some(reason) = self.blocked_reason(access) {
            bail!(reason);
        }
        self.record(tag, payload, access);
        let (items, stays_open) = self
            .streams
            .lock()
            .get_mut(tag)
            .and_then(VecDeque::pop_front)
            .unwrap_or((Vec::new(), true));
        let (sender, receiver) = mpsc::unbounded();
        for item in items {
            sender.unbounded_send(Ok(item)).unwrap();
        }
        if stays_open {
            self.senders
                .lock()
                .entry(tag.to_string())
                .or_default()
                .push(sender);
        }
        Ok(receiver.boxed())
    }

    fn upload(&self, _: AttachmentUpload) -> BoxFuture<'static, Result<()>> {
        ready(Err(anyhow!("the tests upload nothing")))
    }

    fn blocked_reason(&self, access: Access) -> Option<String> {
        self.blocked.lock().get(&access).cloned()
    }
}

// Fixtures: T3's wire JSON, with every field the contracts require.

pub(crate) fn shell_thread(extra: Value) -> Value {
    let mut thread = json!({
        "id": THREAD, "projectId": "project-1", "title": "Add retries", "status": "completed",
        "createdAt": NOW, "updatedAt": NOW, "createdBy": "user", "creationSource": "web",
        "interactionMode": "default", "runtimeMode": "full-access",
        "modelSelection": { "instanceId": "claudeAgent", "model": "claude-opus-5-5" },
        "providerInstanceId": "claudeAgent",
        "activeProviderThreadId": null, "activeRunId": null, "archivedAt": null,
        "branch": "t3/retry-backoff", "deletedAt": null, "forkedFrom": null,
        "hasActionableProposedPlan": false, "itemCount": 3, "latestRunId": null,
        "latestUserMessageAt": null, "latestVisibleMessage": null,
        "lineage": { "rootThreadId": THREAD, "parentThreadId": null, "relationshipToParent": null },
        "pendingRuntimeRequest": null, "settledAt": null, "settledOverride": null,
        "visibleItemCount": 3, "worktreePath": WORKTREE
    });
    if let (Some(thread), Some(extra)) = (thread.as_object_mut(), extra.as_object()) {
        thread.extend(extra.clone());
    }
    thread
}

pub(crate) fn shell(threads: Vec<Value>) -> Value {
    json!({
        "schemaVersion": 1,
        "snapshotSequence": 7,
        "threads": threads,
        "archivedThreads": [],
        "projects": [{
            "id": "project-1", "title": "api", "workspaceRoot": "/src/api", "scripts": [],
            "createdAt": NOW, "updatedAt": NOW, "defaultModelSelection": null
        }]
    })
}

pub(crate) fn init_test(backend: Arc<RecordingBackend>, cx: &mut TestAppContext) {
    cx.update(|cx| {
        let settings_store = settings::SettingsStore::test(cx);
        cx.set_global(settings_store);
        theme_settings::init(theme::LoadThemes::JustBase, cx);
        thread_chat::set_backend(backend, cx);
    });
}

fn local(extra: Value) -> Value {
    let mut local = json!({
        "isRepo": true, "hasPrimaryRemote": true, "isDefaultRef": false,
        "refName": "t3/retry-backoff", "hasWorkingTreeChanges": false,
        "workingTree": { "files": [], "insertions": 0, "deletions": 0 }
    });
    local
        .as_object_mut()
        .unwrap()
        .extend(extra.as_object().unwrap().clone());
    local
}

fn remote(extra: Value) -> Value {
    let mut remote = json!({
        "hasUpstream": true, "aheadCount": 0, "behindCount": 0, "pr": null
    });
    remote
        .as_object_mut()
        .unwrap()
        .extend(extra.as_object().unwrap().clone());
    remote
}

fn dirty() -> Value {
    json!({
        "hasWorkingTreeChanges": true,
        "workingTree": {
            "files": [
                { "path": "src/retry.rs", "insertions": 12, "deletions": 3 },
                { "path": "src/lib.rs", "insertions": 1, "deletions": 0 }
            ],
            "insertions": 13, "deletions": 3
        }
    })
}

fn snapshot(local: Value, remote: Value) -> Value {
    json!({ "_tag": "snapshot", "local": local, "remote": remote })
}

fn status(local: Value, remote: Value) -> GitStatus {
    let mut status = GitStatus::default();
    assert!(status.apply(serde_json::from_value(snapshot(local, remote)).unwrap()));
    status
}

fn open_pr() -> Value {
    json!({
        "number": 7, "title": "Retry webhooks", "url": "https://github.com/acme/api/pull/7",
        "baseRef": "main", "headRef": "t3/retry-backoff", "state": "open"
    })
}

fn quick(status: &GitStatus) -> (String, QuickKind) {
    let quick = quick_action(Some(status), false);
    (quick.label, quick.kind)
}

#[test]
fn the_main_button_does_what_t3s_does() {
    use GitStackedAction::*;
    let run = |label: &str, action| (label.to_string(), QuickKind::Run(action));
    let unavailable = |label: &str| (label.to_string(), QuickKind::Unavailable);

    // Uncommitted changes on a feature branch: all three steps, with T3 writing the text.
    assert_eq!(
        quick(&status(local(dirty()), remote(json!({})))),
        run("Commit, push & PR", CommitPushPr)
    );
    // On the default branch, or with a pull request open, there's no new pull request.
    let mut default = local(dirty());
    default["isDefaultRef"] = json!(true);
    default["refName"] = json!("main");
    assert_eq!(
        quick(&status(default.clone(), remote(json!({})))),
        run("Commit & push", CommitPush)
    );
    assert_eq!(
        quick(&status(local(dirty()), remote(json!({ "pr": open_pr() })))),
        run("Commit & push", CommitPush)
    );
    // Without a remote, only a commit.
    let mut no_remote = local(dirty());
    no_remote["hasPrimaryRemote"] = json!(false);
    assert_eq!(
        quick(&status(no_remote, Value::Null)),
        run("Commit", Commit)
    );
    // Committed but not pushed.
    assert_eq!(
        quick(&status(
            local(json!({})),
            remote(json!({ "aheadCount": 2 }))
        )),
        run("Push & create PR", CreatePr)
    );
    assert_eq!(
        quick(&status(
            local(json!({})),
            remote(json!({ "aheadCount": 2, "pr": open_pr() }))
        )),
        run("Push", Push)
    );
    // Pushing the default branch commits too, as T3 does.
    let mut clean_default = default;
    clean_default["hasWorkingTreeChanges"] = json!(false);
    assert_eq!(
        quick(&status(clean_default, remote(json!({ "aheadCount": 1 })))),
        run("Push", CommitPush)
    );
    // Behind, or diverged.
    assert_eq!(
        quick(&status(
            local(json!({})),
            remote(json!({ "behindCount": 3 }))
        )),
        ("Pull".to_string(), QuickKind::Pull)
    );
    assert_eq!(
        quick(&status(
            local(json!({})),
            remote(json!({ "aheadCount": 1, "behindCount": 3 }))
        )),
        unavailable("Sync branch")
    );
    // Pushed, with commits the default branch lacks: a pull request.
    assert_eq!(
        quick(&status(
            local(json!({})),
            remote(json!({ "aheadOfDefaultCount": 4 }))
        )),
        run("Create PR", CreatePr)
    );
    assert_eq!(
        quick(&status(
            local(json!({})),
            remote(json!({ "pr": open_pr() }))
        )),
        unavailable("Commit")
    );
    // GitLab calls them merge requests.
    let mut gitlab = local(dirty());
    gitlab["sourceControlProvider"] =
        json!({ "kind": "gitlab", "name": "GitLab", "baseUrl": "https://gitlab.com" });
    assert_eq!(
        quick(&status(gitlab, remote(json!({})))),
        run("Commit, push & MR", CommitPushPr)
    );
    // Nothing runs while something is running, or before the status arrives.
    let busy = quick_action(Some(&status(local(dirty()), remote(json!({})))), true);
    assert_eq!(busy.kind, QuickKind::Unavailable);
    assert_eq!(busy.hint.as_deref(), Some("A git action is running."));
    assert_eq!(quick_action(None, false).kind, QuickKind::Unavailable);
    let detached = quick_action(
        Some(&status(
            local(json!({ "refName": null })),
            remote(json!({})),
        )),
        false,
    );
    assert_eq!(
        detached.hint.as_deref(),
        Some("Check out a branch before pushing or opening a PR.")
    );
}

#[test]
fn the_menu_offers_every_stacked_action() {
    let entries = |status: &GitStatus| {
        menu_actions(Some(status), false)
            .into_iter()
            .map(|action| (action.label, action.enabled, action.asks_for_message))
            .collect::<Vec<_>>()
    };
    let entry = |label: &str, enabled, asks| (label.to_string(), enabled, asks);
    assert_eq!(
        entries(&status(local(dirty()), remote(json!({})))),
        [
            entry("Commit…", true, true),
            entry("Commit & push…", true, true),
            entry("Commit, push & create PR…", true, true),
            entry("Push", false, false),
            entry("Create PR", false, false),
            entry("Pull", false, false),
        ]
    );
    assert_eq!(
        entries(&status(
            local(json!({})),
            remote(json!({ "aheadCount": 2, "pr": open_pr() }))
        )),
        [
            entry("Commit…", false, true),
            entry("Commit & push…", false, true),
            entry("Push", true, false),
            entry("Pull", false, false),
        ]
    );
    let mut no_remote = local(dirty());
    no_remote["hasPrimaryRemote"] = json!(false);
    assert_eq!(
        entries(&status(no_remote, Value::Null)),
        [entry("Commit…", true, true)]
    );
    assert!(menu_actions(None, false).is_empty());
}

#[test]
fn status_events_merge_like_t3s_client() {
    let mut status = GitStatus::default();
    assert!(status.apply(
        serde_json::from_value(json!({ "_tag": "localUpdated", "local": local(dirty()) })).unwrap()
    ));
    assert_eq!(status.summary(), "2 changed");
    assert!(
        status.apply(
            serde_json::from_value(json!({
                "_tag": "remoteUpdated",
                "remote": remote(json!({ "aheadCount": 2, "behindCount": 1 }))
            }))
            .unwrap()
        )
    );
    assert_eq!(status.summary(), "2 changed · ↑2 ↓1");
    assert!(
        status
            .apply(serde_json::from_value(snapshot(local(json!({})), remote(json!({})))).unwrap())
    );
    assert_eq!(status.summary(), "Up to date");
    assert!(!status.apply(serde_json::from_value(json!({ "_tag": "somethingNew" })).unwrap()));
}

#[test]
fn payloads_match_t3s() {
    let checkout = Checkout {
        thread_id: THREAD.to_string(),
        project_id: "project-1".to_string(),
        cwd: WORKTREE.to_string(),
        branch: Some("t3/retry-backoff".to_string()),
    };
    let input = |action, options| {
        serde_json::to_value(stacked_action_input(
            &checkout,
            action,
            options,
            "action-1".to_string(),
        ))
        .unwrap()
    };
    // T3 writes the message and the pull request when none is given.
    assert_eq!(
        input(GitStackedAction::CommitPushPr, ActionOptions::default()),
        json!({
            "action": "commit_push_pr", "actionId": "action-1", "cwd": WORKTREE,
            "threadId": THREAD
        })
    );
    assert_eq!(
        input(
            GitStackedAction::CommitPush,
            ActionOptions {
                commit_message: Some("  Retry webhooks with backoff\n".to_string()),
                feature_branch: true,
            }
        ),
        json!({
            "action": "commit_push", "actionId": "action-1", "cwd": WORKTREE,
            "threadId": THREAD, "commitMessage": "Retry webhooks with backoff",
            "featureBranch": true
        })
    );
    assert_eq!(
        input(
            GitStackedAction::Commit,
            ActionOptions {
                commit_message: Some("   ".to_string()),
                feature_branch: false,
            }
        ),
        json!({ "action": "commit", "actionId": "action-1", "cwd": WORKTREE, "threadId": THREAD })
    );
    for (action, name) in [
        (GitStackedAction::Push, "push"),
        (GitStackedAction::CreatePr, "create_pr"),
    ] {
        assert_eq!(
            input(action, ActionOptions::default())["action"],
            json!(name)
        );
    }

    assert_eq!(
        serde_json::to_value(branch_update(
            THREAD,
            "feature/retries".to_string(),
            Some("t3/retry-backoff".to_string()),
            "command-1".to_string()
        ))
        .unwrap(),
        json!({
            "type": "thread.metadata.update", "commandId": "command-1", "threadId": THREAD,
            "branch": "feature/retries", "expectedBranch": "t3/retry-backoff"
        })
    );
    assert_eq!(
        serde_json::to_value(list_refs_input(WORKTREE, "  feat ")).unwrap(),
        json!({ "cwd": WORKTREE, "query": "feat", "limit": 100 })
    );
    assert_eq!(
        serde_json::to_value(open_in_editor_input(WORKTREE, EditorId::Zed)).unwrap(),
        json!({ "cwd": WORKTREE, "editor": "zed" })
    );
}

#[test]
fn remote_branches_switch_to_their_local_name() {
    let git_ref = |name: &str, remote: Option<&str>| -> VcsRef {
        serde_json::from_value(json!({
            "name": name, "current": false, "isDefault": false, "worktreePath": null,
            "isRemote": remote.is_some(), "remoteName": remote
        }))
        .unwrap()
    };
    assert_eq!(
        local_ref_name(&git_ref("origin/feature/x", Some("origin"))),
        "feature/x"
    );
    assert_eq!(
        local_ref_name(&git_ref("upstream/main", None)),
        "upstream/main"
    );
    assert_eq!(local_ref_name(&git_ref("main", None)), "main");
}

#[test]
fn editors_follow_t3s_order() {
    let editors = sorted_editors(&[
        EditorId::FileManager,
        EditorId::Zed,
        EditorId::Cursor,
        EditorId::Unrecognized("newcomer".to_string()),
    ]);
    assert_eq!(
        editors.iter().map(editor_label).collect::<Vec<_>>(),
        ["Cursor", "Zed", "Files", "newcomer"]
    );
}

fn progress(kind: &str, action_id: &str, fields: Value) -> Value {
    let mut event = json!({
        "kind": kind, "actionId": action_id, "cwd": WORKTREE, "action": "commit_push_pr"
    });
    event
        .as_object_mut()
        .unwrap()
        .extend(fields.as_object().unwrap().clone());
    event
}

fn finished_result() -> Value {
    json!({
        "action": "commit_push_pr",
        "branch": { "status": "created", "name": "feature/retries" },
        "commit": { "status": "created", "commitSha": "abc1234", "subject": "Retry webhooks" },
        "push": { "status": "pushed", "branch": "feature/retries", "setUpstream": true },
        "pr": {
            "status": "created", "url": "https://github.com/acme/api/pull/12", "number": 12,
            "baseBranch": "main", "headBranch": "feature/retries", "title": "Retry webhooks"
        },
        "toast": {
            "title": "Created PR #12", "description": "Retry webhooks",
            "cta": { "kind": "open_pr", "label": "View PR", "url": "https://github.com/acme/api/pull/12" }
        }
    })
}

async fn thread_git(backend: &Arc<RecordingBackend>, cx: &mut TestAppContext) -> Entity<ThreadGit> {
    init_test(backend.clone(), cx);
    let git = cx.update(|cx| ThreadGit::for_thread(THREAD, cx));
    cx.run_until_parked();
    git
}

#[gpui::test]
async fn the_status_follows_the_checkout(cx: &mut TestAppContext) {
    let backend = RecordingBackend::with_shell(shell(vec![shell_thread(json!({}))]));
    backend.script_stream(
        "subscribeVcsStatus",
        vec![snapshot(local(dirty()), remote(json!({})))],
        true,
    );
    let git = thread_git(&backend, cx).await;
    assert_eq!(
        backend.calls_to("subscribeVcsStatus"),
        [Call {
            tag: "subscribeVcsStatus".to_string(),
            payload: json!({ "cwd": WORKTREE }),
            access: Access::Read,
        }]
    );
    git.read_with(cx, |git, _| {
        assert_eq!(
            git.checkout(),
            Some(&Checkout {
                thread_id: THREAD.to_string(),
                project_id: "project-1".to_string(),
                cwd: WORKTREE.to_string(),
                branch: Some("t3/retry-backoff".to_string()),
            })
        );
        assert_eq!(git.quick_action().label, "Commit, push & PR");
    });

    backend.push(
        "subscribeVcsStatus",
        json!({ "_tag": "localUpdated", "local": local(json!({})) }),
    );
    cx.run_until_parked();
    git.read_with(cx, |git, _| {
        assert_eq!(git.status().unwrap().summary(), "Up to date")
    });
    // The same thread shares one follower.
    let again = cx.update(|cx| ThreadGit::for_thread(THREAD, cx));
    assert_eq!(again, git);
    assert_eq!(backend.calls_to("subscribeVcsStatus").len(), 1);
}

#[gpui::test]
async fn threads_without_a_worktree_use_the_projects_checkout(cx: &mut TestAppContext) {
    let backend = RecordingBackend::with_shell(shell(vec![shell_thread(
        json!({ "worktreePath": null, "branch": null }),
    )]));
    let git = thread_git(&backend, cx).await;
    git.read_with(cx, |git, _| {
        assert_eq!(
            git.checkout().map(|checkout| checkout.cwd.as_str()),
            Some("/src/api")
        )
    });
    assert_eq!(
        backend.calls_to("subscribeVcsStatus")[0].payload,
        json!({ "cwd": "/src/api" })
    );
}

#[gpui::test]
async fn stacked_actions_follow_their_progress(cx: &mut TestAppContext) {
    let backend = RecordingBackend::with_shell(shell(vec![shell_thread(json!({}))]));
    backend.script_stream(
        "subscribeVcsStatus",
        vec![snapshot(local(dirty()), remote(json!({})))],
        true,
    );
    let git = thread_git(&backend, cx).await;
    git.update(cx, |git, cx| {
        git.run_action(GitStackedAction::CommitPushPr, ActionOptions::default(), cx)
    });
    let calls = backend.calls_to("git.runStackedAction");
    assert_eq!(calls.len(), 1);
    let action_id = calls[0].payload["actionId"].as_str().unwrap().to_string();
    assert_eq!(
        calls[0],
        Call {
            tag: "git.runStackedAction".to_string(),
            payload: json!({
                "action": "commit_push_pr", "actionId": action_id, "cwd": WORKTREE,
                "threadId": THREAD
            }),
            access: Access::SourceControlWrite,
        }
    );
    git.read_with(cx, |git, _| {
        assert!(git.is_busy());
        assert_eq!(git.quick_action().kind, QuickKind::Unavailable);
    });

    let id = action_id.as_str();
    for event in [
        progress(
            "action_started",
            id,
            json!({ "phases": ["branch", "commit", "push", "pr"] }),
        ),
        progress(
            "phase_started",
            id,
            json!({ "phase": "branch", "label": "Creating branch..." }),
        ),
        progress(
            "phase_started",
            id,
            json!({ "phase": "commit", "label": "Committing..." }),
        ),
        progress("hook_started", id, json!({ "hookName": "pre-commit" })),
        progress(
            "hook_output",
            id,
            json!({ "hookName": "pre-commit", "stream": "stdout", "text": "lint ok\nfmt ok\n" }),
        ),
        // Another action's event changes nothing.
        progress(
            "phase_started",
            "other",
            json!({ "phase": "pr", "label": "Other" }),
        ),
    ] {
        backend.push("git.runStackedAction", event);
    }
    cx.run_until_parked();
    git.read_with(cx, |git, _| {
        let run = git.run().unwrap();
        assert_eq!(run.output.as_deref(), Some("fmt ok"));
        assert_eq!(
            run.steps(),
            [
                (GitActionProgressPhase::Branch, StepState::Done),
                (GitActionProgressPhase::Commit, StepState::Running),
                (GitActionProgressPhase::Push, StepState::Pending),
                (GitActionProgressPhase::Pr, StepState::Pending),
            ]
        );
    });

    backend.push(
        "git.runStackedAction",
        progress(
            "action_finished",
            id,
            json!({ "result": finished_result() }),
        ),
    );
    cx.run_until_parked();
    git.read_with(cx, |git, _| {
        let run = git.run().unwrap();
        assert!(!run.is_running());
        assert!(
            run.steps()
                .iter()
                .all(|(_, state)| *state == StepState::Done)
        );
        assert_eq!(git.notice(), Some(&Notice::Done("Created PR #12".into())));
        match &run.outcome {
            Some(RunOutcome::Finished(result)) => assert_eq!(
                result_pull_request(result),
                Some(("https://github.com/acme/api/pull/12".to_string(), 12))
            ),
            outcome => panic!("expected a finished run, got {outcome:?}"),
        }
    });
    // T3 made a branch for the commit: the thread is on it now.
    let commands = backend.commands();
    assert_eq!(commands.len(), 1);
    assert_eq!(
        commands[0],
        json!({
            "type": "thread.metadata.update", "commandId": commands[0]["commandId"],
            "threadId": THREAD, "branch": "feature/retries", "expectedBranch": "t3/retry-backoff"
        })
    );
    assert_eq!(
        backend.calls_to("vcs.refreshStatus"),
        [Call {
            tag: "vcs.refreshStatus".to_string(),
            payload: json!({ "cwd": WORKTREE }),
            access: Access::Read,
        }]
    );
}

#[gpui::test]
async fn failures_say_which_step_failed(cx: &mut TestAppContext) {
    let backend = RecordingBackend::with_shell(shell(vec![shell_thread(json!({}))]));
    let git = thread_git(&backend, cx).await;
    git.update(cx, |git, cx| {
        git.run_action(GitStackedAction::CommitPush, ActionOptions::default(), cx)
    });
    let action_id = backend.calls_to("git.runStackedAction")[0].payload["actionId"]
        .as_str()
        .unwrap()
        .to_string();
    for event in [
        progress(
            "action_started",
            &action_id,
            json!({ "phases": ["commit", "push"] }),
        ),
        progress(
            "phase_started",
            &action_id,
            json!({ "phase": "commit", "label": "Committing..." }),
        ),
        progress(
            "phase_started",
            &action_id,
            json!({ "phase": "push", "label": "Pushing..." }),
        ),
        progress(
            "action_failed",
            &action_id,
            json!({ "phase": "push", "message": "The remote rejected the push." }),
        ),
    ] {
        backend.push("git.runStackedAction", event);
    }
    cx.run_until_parked();
    git.read_with(cx, |git, _| {
        assert_eq!(
            git.run().unwrap().steps(),
            [
                (GitActionProgressPhase::Commit, StepState::Done),
                (GitActionProgressPhase::Push, StepState::Failed),
            ]
        );
        assert_eq!(
            git.notice(),
            Some(&Notice::Error(
                "Push failed: The remote rejected the push.".into()
            ))
        );
        assert!(!git.is_busy());
    });
    assert!(backend.commands().is_empty());
}

#[gpui::test]
async fn refused_actions_say_why(cx: &mut TestAppContext) {
    let backend = RecordingBackend::with_shell(shell(vec![shell_thread(json!({}))]));
    backend.blocked.lock().insert(
        Access::SourceControlWrite,
        "Not sent: MUSTR_DRY_RUN_WRITES is set.".to_string(),
    );
    let git = thread_git(&backend, cx).await;
    git.read_with(cx, |git, _| {
        assert_eq!(
            git.write_blocked_reason().as_deref(),
            Some("Not sent: MUSTR_DRY_RUN_WRITES is set.")
        )
    });
    git.update(cx, |git, cx| {
        git.run_action(GitStackedAction::Commit, ActionOptions::default(), cx);
    });
    cx.run_until_parked();
    git.read_with(cx, |git, _| {
        assert_eq!(
            git.notice(),
            Some(&Notice::Error(
                "Commit failed: Not sent: MUSTR_DRY_RUN_WRITES is set.".into()
            ))
        );
    });
    git.update(cx, |git, cx| git.switch_ref("main".to_string(), cx));
    cx.run_until_parked();
    assert!(backend.calls_to("git.runStackedAction").is_empty());
    assert!(backend.calls_to("vcs.switchRef").is_empty());
}

#[gpui::test]
async fn branches_switch_and_are_created_through_t3(cx: &mut TestAppContext) {
    let backend = RecordingBackend::with_shell(shell(vec![shell_thread(json!({}))]));
    backend.respond("vcs.switchRef", Ok(json!({ "refName": "main" })));
    backend.respond("vcs.createRef", Ok(json!({ "refName": "feature/docs" })));
    backend.respond(
        "vcs.listRefs",
        Ok(json!({
            "refs": [{ "name": "main", "current": false, "isDefault": true, "worktreePath": null }],
            "isRepo": true, "hasPrimaryRemote": true, "nextCursor": null, "totalCount": 1
        })),
    );
    let git = thread_git(&backend, cx).await;

    let refs = git
        .read_with(cx, |git, _| git.list_refs("ma"))
        .unwrap()
        .await
        .unwrap();
    assert_eq!(
        refs.iter()
            .map(|git_ref| git_ref.name.as_str())
            .collect::<Vec<_>>(),
        ["main"]
    );

    git.update(cx, |git, cx| git.switch_ref("main".to_string(), cx));
    cx.run_until_parked();
    git.update(cx, |git, cx| {
        git.create_ref(" feature/docs ".to_string(), cx)
    });
    cx.run_until_parked();

    let writes = backend
        .calls
        .lock()
        .iter()
        .filter(|call| call.tag != "subscribeVcsStatus")
        .cloned()
        .collect::<Vec<_>>();
    let call = |tag: &str, payload: Value, access| Call {
        tag: tag.to_string(),
        payload,
        access,
    };
    assert_eq!(
        writes,
        [
            call(
                "vcs.listRefs",
                json!({ "cwd": WORKTREE, "query": "ma", "limit": 100 }),
                Access::Read
            ),
            call(
                "vcs.switchRef",
                json!({ "cwd": WORKTREE, "refName": "main" }),
                Access::SourceControlWrite
            ),
            call(
                "vcs.refreshStatus",
                json!({ "cwd": WORKTREE }),
                Access::Read
            ),
            call(
                "vcs.createRef",
                json!({ "cwd": WORKTREE, "refName": "feature/docs", "switchRef": true }),
                Access::SourceControlWrite
            ),
            call(
                "vcs.refreshStatus",
                json!({ "cwd": WORKTREE }),
                Access::Read
            ),
        ]
    );
    // Each new branch is recorded on the thread, against the branch T3 had.
    let commands = backend.commands();
    assert_eq!(
        commands
            .iter()
            .map(|command| (
                command["type"].clone(),
                command["branch"].clone(),
                command["expectedBranch"].clone()
            ))
            .collect::<Vec<_>>(),
        [
            (
                json!("thread.metadata.update"),
                json!("main"),
                json!("t3/retry-backoff")
            ),
            (
                json!("thread.metadata.update"),
                json!("feature/docs"),
                json!("t3/retry-backoff")
            ),
        ]
    );
    git.read_with(cx, |git, _| {
        assert_eq!(
            git.notice(),
            Some(&Notice::Done("Created feature/docs".into()))
        )
    });
}

#[gpui::test]
async fn the_checkout_opens_in_the_last_editor_used(cx: &mut TestAppContext) {
    let backend = RecordingBackend::with_shell(shell(vec![shell_thread(json!({}))]));
    // Only the editors are read, so the rest of the config can be anything.
    backend.respond(
        "server.getConfig",
        Ok(json!({ "availableEditors": ["file-manager", "zed", "cursor"], "cwd": "/src" })),
    );
    let git = thread_git(&backend, cx).await;
    let editors = cx.update(Editors::global);
    cx.run_until_parked();
    editors.read_with(cx, |editors, _| {
        assert_eq!(
            editors.available(),
            [EditorId::Cursor, EditorId::Zed, EditorId::FileManager]
        );
        assert_eq!(editors.preferred(), Some(EditorId::Cursor));
    });
    assert_eq!(
        backend.calls_to("server.getConfig"),
        [Call {
            tag: "server.getConfig".to_string(),
            payload: json!({}),
            access: Access::Read,
        }]
    );

    git.update(cx, |git, cx| git.open_in_editor(None, cx));
    git.update(cx, |git, cx| git.open_in_editor(Some(EditorId::Zed), cx));
    cx.run_until_parked();
    git.update(cx, |git, cx| git.open_in_editor(None, cx));
    cx.run_until_parked();
    let opened = backend
        .calls_to("shell.openInEditor")
        .into_iter()
        .map(|call| (call.payload, call.access))
        .collect::<Vec<_>>();
    assert_eq!(
        opened,
        [
            (
                json!({ "cwd": WORKTREE, "editor": "cursor" }),
                Access::Operate
            ),
            (json!({ "cwd": WORKTREE, "editor": "zed" }), Access::Operate),
            (json!({ "cwd": WORKTREE, "editor": "zed" }), Access::Operate),
        ]
    );
}

#[gpui::test]
async fn running_actions_outlive_their_views(cx: &mut TestAppContext) {
    let backend = RecordingBackend::with_shell(shell(vec![shell_thread(json!({}))]));
    let git = thread_git(&backend, cx).await;
    let id = git.entity_id();
    git.update(cx, |git, cx| {
        git.run_action(GitStackedAction::Push, ActionOptions::default(), cx)
    });
    // The toolbar lets go of the thread while the review it opened takes its place.
    drop(git);
    cx.run_until_parked();
    let git = cx.update(|cx| ThreadGit::for_thread(THREAD, cx));
    assert_eq!(git.entity_id(), id);
    git.read_with(cx, |git, _| {
        assert!(git.run().is_some_and(ActionRun::is_running))
    });

    // Once there's nothing to show, the thread's git goes with its last view.
    let action_id = backend.calls_to("git.runStackedAction")[0].payload["actionId"]
        .as_str()
        .unwrap()
        .to_string();
    backend.push(
        "git.runStackedAction",
        progress(
            "action_failed",
            &action_id,
            json!({ "phase": null, "message": "No." }),
        ),
    );
    cx.run_until_parked();
    git.update(cx, |git, cx| git.dismiss(cx));
    drop(git);
    cx.run_until_parked();
    let git = cx.update(|cx| ThreadGit::for_thread(THREAD, cx));
    assert_ne!(git.entity_id(), id);
}

//! The review view's writes, sent to the forge through T3's pull request RPCs, and its pending
//! review, saved in mustr's data directory.
//!
//! Every RPC goes through [`PullRequestRpc`], which the real connection implements with
//! `RpcConnection::call::<t3_api::rpc::X>`. Tests swap in a recording fake and assert the exact
//! typed payloads, so no test ever writes to a real pull request.

use crate::{pull_request_review, review_drafts::DraftStore, t3_connection::CurrentRpc};
use anyhow::{Result, anyhow};
use futures::{FutureExt as _, future::BoxFuture};
use gpui::{App, AppContext as _, Task};
use review::{
    CommentPosition, DiffSide, PendingReview, ReviewBackend, ReviewSubmission, ReviewThread,
    ReviewVerdict,
};
use serde_json::Value;
use std::sync::Arc;
use t3_client::{
    PullRequestRef, RpcConnection, RpcError, RpcMethod,
    t3_api::{
        rpc,
        tagged::PullRequestReviewPosition,
        types::{
            EnvironmentAuthorizationError, PullRequestCommentInput, PullRequestCommentUpdateInput,
            PullRequestCommentUpdateInputKind, PullRequestDiffSide, PullRequestReviewCommentDraft,
            PullRequestReviewPositionAdded, PullRequestReviewPositionAddedKind,
            PullRequestReviewPositionContext, PullRequestReviewPositionContextKind,
            PullRequestReviewPositionDeleted, PullRequestReviewPositionDeletedKind,
            PullRequestReviewVerdict, PullRequestSetFilesViewedInput,
            PullRequestSetFilesViewedInputFilesItem, PullRequestSubmitReviewInput,
            PullRequestThreadReplyInput, PullRequestThreadResolutionInput,
            PullRequestUnavailableError, PullRequestUnavailableReason,
        },
    },
};

/// The RPCs the review uses: typed calls for writes, and untyped requests for the read path,
/// which decodes leniently.
pub trait PullRequestRpc: Send + Sync + 'static {
    fn call<M>(&self, payload: M::Payload) -> BoxFuture<'static, Result<M::Success, RpcError>>
    where
        M: RpcMethod + 'static,
        M::Payload: Send + 'static,
        M::Success: Send + 'static;

    fn request(
        &self,
        tag: &'static str,
        payload: Value,
    ) -> BoxFuture<'static, Result<Value, RpcError>>;
}

impl PullRequestRpc for RpcConnection {
    fn call<M>(&self, payload: M::Payload) -> BoxFuture<'static, Result<M::Success, RpcError>>
    where
        M: RpcMethod + 'static,
        M::Payload: Send + 'static,
        M::Success: Send + 'static,
    {
        let connection = self.clone();
        async move { connection.call::<M>(payload).await }.boxed()
    }

    fn request(
        &self,
        tag: &'static str,
        payload: Value,
    ) -> BoxFuture<'static, Result<Value, RpcError>> {
        let connection = self.clone();
        async move { RpcConnection::request(&connection, tag, payload).await }.boxed()
    }
}

/// Follows reconnects: calls go to whichever connection is current.
impl PullRequestRpc for CurrentRpc {
    fn call<M>(&self, payload: M::Payload) -> BoxFuture<'static, Result<M::Success, RpcError>>
    where
        M: RpcMethod + 'static,
        M::Payload: Send + 'static,
        M::Success: Send + 'static,
    {
        match self.get() {
            Some(connection) => PullRequestRpc::call::<M>(&connection, payload),
            None => not_connected(),
        }
    }

    fn request(
        &self,
        tag: &'static str,
        payload: Value,
    ) -> BoxFuture<'static, Result<Value, RpcError>> {
        match self.get() {
            Some(connection) => PullRequestRpc::request(&connection, tag, payload),
            None => not_connected(),
        }
    }
}

/// Set to make the review's writes fail instead of reaching T3, for demos and screenshots of a
/// real pull request. Reads still go through.
pub const DRY_RUN_ENV: &str = "MUSTR_DRY_RUN_WRITES";

pub fn dry_run_requested() -> bool {
    std::env::var_os(DRY_RUN_ENV).is_some_and(|value| !value.is_empty())
}

/// Sends reads to `R` and refuses every typed call, which is how the review writes.
pub struct DryRunRpc<R>(pub R);

impl<R: PullRequestRpc> PullRequestRpc for DryRunRpc<R> {
    fn call<M>(&self, _payload: M::Payload) -> BoxFuture<'static, Result<M::Success, RpcError>>
    where
        M: RpcMethod + 'static,
        M::Payload: Send + 'static,
        M::Success: Send + 'static,
    {
        log::warn!("{DRY_RUN_ENV} is set, so {} wasn't sent", M::TAG);
        futures::future::ready(Err(RpcError::Protocol(format!(
            "Not sent: {DRY_RUN_ENV} is set."
        ))))
        .boxed()
    }

    fn request(
        &self,
        tag: &'static str,
        payload: Value,
    ) -> BoxFuture<'static, Result<Value, RpcError>> {
        self.0.request(tag, payload)
    }
}

fn not_connected<T: Send + 'static>() -> BoxFuture<'static, Result<T, RpcError>> {
    futures::future::ready(Err(RpcError::Closed(
        "mustr isn't connected to T3".to_string(),
    )))
    .boxed()
}

/// One pull request's review, written through T3.
pub struct T3ReviewBackend<R> {
    rpc: Arc<R>,
    pull_request: PullRequestRef,
    drafts: DraftStore,
}

impl<R: PullRequestRpc> T3ReviewBackend<R> {
    pub fn new(rpc: Arc<R>, pull_request: PullRequestRef, drafts: DraftStore) -> Self {
        Self {
            rpc,
            pull_request,
            drafts,
        }
    }

    fn send<M>(&self, payload: M::Payload, cx: &mut App) -> Task<Result<()>>
    where
        M: RpcMethod<Success = ()> + 'static,
        M::Payload: Send + 'static,
    {
        let call = self.rpc.call::<M>(payload);
        cx.background_spawn(async move { call.await.map_err(|error| write_error(M::TAG, &error)) })
    }
}

impl<R: PullRequestRpc> ReviewBackend for T3ReviewBackend<R> {
    fn save_pending_review(&self, review: &PendingReview, cx: &mut App) {
        self.drafts.save(review.clone(), cx);
    }

    fn reply_to_thread(&self, thread_id: String, body: String, cx: &mut App) -> Task<Result<()>> {
        self.send::<rpc::PullRequestsReplyToThread>(
            reply_payload(&self.pull_request, thread_id, body),
            cx,
        )
    }

    fn set_thread_resolution(
        &self,
        thread_id: String,
        resolved: bool,
        cx: &mut App,
    ) -> Task<Result<()>> {
        self.send::<rpc::PullRequestsSetThreadResolution>(
            resolution_payload(&self.pull_request, thread_id, resolved),
            cx,
        )
    }

    fn update_comment(&self, comment_id: String, body: String, cx: &mut App) -> Task<Result<()>> {
        self.send::<rpc::PullRequestsUpdateComment>(
            update_comment_payload(&self.pull_request, comment_id, body),
            cx,
        )
    }

    fn set_files_viewed(&self, files: Vec<(String, bool)>, cx: &mut App) -> Task<Result<()>> {
        self.send::<rpc::PullRequestsSetFilesViewed>(
            files_viewed_payload(&self.pull_request, files),
            cx,
        )
    }

    fn submit_review(&self, submission: ReviewSubmission, cx: &mut App) -> Task<Result<()>> {
        self.send::<rpc::PullRequestsSubmitReview>(
            submit_review_payload(&self.pull_request, submission),
            cx,
        )
    }

    fn post_comment(&self, body: String, cx: &mut App) -> Task<Result<()>> {
        self.send::<rpc::PullRequestsComment>(comment_payload(&self.pull_request, body), cx)
    }

    fn reload_threads(&self, cx: &mut App) -> Task<Result<Vec<ReviewThread>>> {
        let rpc = self.rpc.clone();
        let pull_request = self.pull_request.clone();
        cx.background_spawn(async move {
            pull_request_review::load_threads(rpc.as_ref(), &pull_request).await
        })
    }
}

// Writes leave `allowStale` out: they go to the forge, never to T3's cache.

pub fn reply_payload(
    pull_request: &PullRequestRef,
    thread_id: String,
    body: String,
) -> PullRequestThreadReplyInput {
    PullRequestThreadReplyInput {
        allow_stale: None,
        body,
        expected_account_id: pull_request.expected_account_id.clone(),
        host: pull_request.host.clone(),
        number: pull_request.number,
        project_id: pull_request.project_id.clone(),
        repository: pull_request.repository.clone(),
        thread_id,
    }
}

pub fn resolution_payload(
    pull_request: &PullRequestRef,
    thread_id: String,
    resolved: bool,
) -> PullRequestThreadResolutionInput {
    PullRequestThreadResolutionInput {
        allow_stale: None,
        expected_account_id: pull_request.expected_account_id.clone(),
        host: pull_request.host.clone(),
        number: pull_request.number,
        project_id: pull_request.project_id.clone(),
        repository: pull_request.repository.clone(),
        resolved,
        thread_id,
    }
}

/// Edits a line comment. Review threads hold only line comments (`review-comment`); comments on
/// the conversation (`issue-comment`) aren't shown in the review.
pub fn update_comment_payload(
    pull_request: &PullRequestRef,
    comment_id: String,
    body: String,
) -> PullRequestCommentUpdateInput {
    PullRequestCommentUpdateInput {
        allow_stale: None,
        body,
        comment_id,
        expected_account_id: pull_request.expected_account_id.clone(),
        host: pull_request.host.clone(),
        kind: PullRequestCommentUpdateInputKind::ReviewComment,
        number: pull_request.number,
        project_id: pull_request.project_id.clone(),
        repository: pull_request.repository.clone(),
    }
}

pub fn files_viewed_payload(
    pull_request: &PullRequestRef,
    files: Vec<(String, bool)>,
) -> PullRequestSetFilesViewedInput {
    PullRequestSetFilesViewedInput {
        allow_stale: None,
        expected_account_id: pull_request.expected_account_id.clone(),
        files: files
            .into_iter()
            .map(|(path, viewed)| PullRequestSetFilesViewedInputFilesItem { path, viewed })
            .collect(),
        host: pull_request.host.clone(),
        number: pull_request.number,
        project_id: pull_request.project_id.clone(),
        repository: pull_request.repository.clone(),
    }
}

pub fn comment_payload(pull_request: &PullRequestRef, body: String) -> PullRequestCommentInput {
    PullRequestCommentInput {
        allow_stale: None,
        body,
        expected_account_id: pull_request.expected_account_id.clone(),
        host: pull_request.host.clone(),
        number: pull_request.number,
        project_id: pull_request.project_id.clone(),
        repository: pull_request.repository.clone(),
    }
}

/// The review with its added comments. T3 0.1.1 takes one line per comment (its
/// `PullRequestReviewCommentDraft` has no range start), so a comment written on several lines
/// goes on the last of them, as T3's web app does for forges without range comments.
pub fn submit_review_payload(
    pull_request: &PullRequestRef,
    submission: ReviewSubmission,
) -> PullRequestSubmitReviewInput {
    PullRequestSubmitReviewInput {
        allow_stale: None,
        body: submission.body,
        comments: submission
            .comments
            .into_iter()
            .map(|draft| PullRequestReviewCommentDraft {
                body: draft.body,
                old_path: draft.old_path,
                path: draft.path,
                position: wire_position(draft.position),
            })
            .collect(),
        expected_account_id: pull_request.expected_account_id.clone(),
        host: pull_request.host.clone(),
        number: pull_request.number,
        project_id: pull_request.project_id.clone(),
        repository: pull_request.repository.clone(),
        verdict: match submission.verdict {
            ReviewVerdict::Comment => PullRequestReviewVerdict::Comment,
            ReviewVerdict::Approve => PullRequestReviewVerdict::Approve,
            ReviewVerdict::RequestChanges => PullRequestReviewVerdict::RequestChanges,
        },
    }
}

fn wire_position(position: CommentPosition) -> PullRequestReviewPosition {
    match position {
        CommentPosition::Added { new_line } => {
            PullRequestReviewPosition::Added(PullRequestReviewPositionAdded {
                kind: PullRequestReviewPositionAddedKind::Added,
                new_line: i64::from(new_line),
            })
        }
        CommentPosition::Deleted { old_line } => {
            PullRequestReviewPosition::Deleted(PullRequestReviewPositionDeleted {
                kind: PullRequestReviewPositionDeletedKind::Deleted,
                old_line: i64::from(old_line),
            })
        }
        CommentPosition::Context {
            old_line,
            new_line,
            side,
        } => PullRequestReviewPosition::Context(PullRequestReviewPositionContext {
            kind: PullRequestReviewPositionContextKind::Context,
            new_line: i64::from(new_line),
            old_line: i64::from(old_line),
            side: match side {
                DiffSide::Left => PullRequestDiffSide::Left,
                DiffSide::Right => PullRequestDiffSide::Right,
            },
        }),
    }
}

/// What the reviewer sees when a write fails: T3's typed error, explained.
pub fn write_error(tag: &str, error: &RpcError) -> anyhow::Error {
    let message = match error {
        RpcError::Failed { error: value, .. } => match error.error_tag() {
            Some("EnvironmentAuthorizationError") => {
                // `requiredScope` names the scope older clients know (for writing to pull
                // requests, `orchestration:operate`); `requiredPermission` the one that's missing.
                let scope = serde_json::from_value::<EnvironmentAuthorizationError>(value.clone())
                    .map(|error| {
                        error
                            .required_permission
                            .unwrap_or_else(|| error.required_scope.as_str().to_string())
                    })
                    .unwrap_or_else(|_| "a scope".to_string());
                format!(
                    "T3 refused: mustr's pairing doesn't include {scope}. Pair again from the \
                     home view."
                )
            }
            Some("PullRequestUnavailableError") => {
                match serde_json::from_value::<PullRequestUnavailableError>(value.clone())
                    .map(|error| error.reason)
                {
                    Ok(PullRequestUnavailableReason::CliMissing) => {
                        "T3 can't reach the forge: its command-line tool isn't installed on the \
                         T3 machine."
                            .to_string()
                    }
                    Ok(PullRequestUnavailableReason::CliUnauthenticated) => {
                        "T3 can't reach the forge: its command-line tool isn't signed in on the \
                         T3 machine."
                            .to_string()
                    }
                    _ => "T3 doesn't support this forge.".to_string(),
                }
            }
            _ => error.error_detail().unwrap_or_else(|| error.to_string()),
        },
        RpcError::Closed(_) => {
            "Lost the connection to T3. Your text is kept; try again once mustr reconnects."
                .to_string()
        }
        _ => error.to_string(),
    };
    log::error!("{tag} failed: {error}");
    anyhow!(message)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_rpc::RecordingRpc;
    use gpui::TestAppContext;
    use pretty_assertions::assert_eq;
    use review::DraftComment;
    use serde_json::json;
    use t3_client::t3_api::types::PullRequestReviewPositionContext as Context;

    fn pull_request() -> PullRequestRef {
        PullRequestRef {
            allow_stale: Some(true),
            expected_account_id: None,
            project_id: "project-1".to_string(),
            host: Some("github.com".to_string()),
            repository: "acme/hello".to_string(),
            number: 7,
        }
    }

    fn backend(
        cx: &mut TestAppContext,
    ) -> (
        Arc<RecordingRpc>,
        T3ReviewBackend<RecordingRpc>,
        tempfile::TempDir,
    ) {
        let rpc = Arc::new(RecordingRpc::default());
        let dir = tempfile::tempdir().expect("a temporary directory");
        let drafts = DraftStore::new(dir.path(), &pull_request());
        let backend = T3ReviewBackend::new(rpc.clone(), pull_request(), drafts);
        cx.run_until_parked();
        (rpc, backend, dir)
    }

    fn draft(id: &str, position: CommentPosition, body: &str) -> DraftComment {
        DraftComment {
            id: id.to_string(),
            path: "src/main.rs".to_string(),
            old_path: None,
            position,
            start_line: None,
            body: body.to_string(),
            added: true,
        }
    }

    #[gpui::test]
    async fn test_submitting_a_review_sends_t3s_payload(cx: &mut TestAppContext) {
        let (rpc, backend, _dir) = backend(cx);
        let submission = ReviewSubmission {
            quotes: Default::default(),
            verdict: ReviewVerdict::RequestChanges,
            body: "A few things.".to_string(),
            comments: vec![
                draft("a", CommentPosition::Added { new_line: 12 }, "Why?"),
                DraftComment {
                    path: "lib/helpers.rs".to_string(),
                    old_path: Some("lib/util.rs".to_string()),
                    start_line: Some(3),
                    ..draft("b", CommentPosition::Deleted { old_line: 5 }, "Gone?")
                },
                draft(
                    "c",
                    CommentPosition::Context {
                        old_line: 8,
                        new_line: 9,
                        side: DiffSide::Left,
                    },
                    "Context.",
                ),
            ],
        };
        cx.update(|cx| backend.submit_review(submission, cx))
            .await
            .expect("the review should be sent");

        let payload = rpc.only_call::<rpc::PullRequestsSubmitReview>();
        assert_eq!(
            payload,
            PullRequestSubmitReviewInput {
                allow_stale: None,
                body: "A few things.".to_string(),
                comments: vec![
                    PullRequestReviewCommentDraft {
                        body: "Why?".to_string(),
                        old_path: None,
                        path: "src/main.rs".to_string(),
                        position: PullRequestReviewPosition::Added(
                            PullRequestReviewPositionAdded {
                                kind: PullRequestReviewPositionAddedKind::Added,
                                new_line: 12,
                            }
                        ),
                    },
                    PullRequestReviewCommentDraft {
                        body: "Gone?".to_string(),
                        old_path: Some("lib/util.rs".to_string()),
                        path: "lib/helpers.rs".to_string(),
                        position: PullRequestReviewPosition::Deleted(
                            PullRequestReviewPositionDeleted {
                                kind: PullRequestReviewPositionDeletedKind::Deleted,
                                old_line: 5,
                            }
                        ),
                    },
                    PullRequestReviewCommentDraft {
                        body: "Context.".to_string(),
                        old_path: None,
                        path: "src/main.rs".to_string(),
                        position: PullRequestReviewPosition::Context(Context {
                            kind: PullRequestReviewPositionContextKind::Context,
                            new_line: 9,
                            old_line: 8,
                            side: PullRequestDiffSide::Left,
                        }),
                    },
                ],
                expected_account_id: None,
                host: Some("github.com".to_string()),
                number: 7,
                project_id: "project-1".to_string(),
                repository: "acme/hello".to_string(),
                verdict: PullRequestReviewVerdict::RequestChanges,
            }
        );
        // And on the wire, exactly T3's JSON shape.
        assert_eq!(
            rpc.calls.lock()[0].1,
            json!({
                "body": "A few things.",
                "comments": [
                    { "body": "Why?", "path": "src/main.rs",
                      "position": { "kind": "added", "newLine": 12 } },
                    { "body": "Gone?", "oldPath": "lib/util.rs", "path": "lib/helpers.rs",
                      "position": { "kind": "deleted", "oldLine": 5 } },
                    { "body": "Context.", "path": "src/main.rs",
                      "position": { "kind": "context", "newLine": 9, "oldLine": 8, "side": "left" } }
                ],
                "host": "github.com",
                "number": 7,
                "projectId": "project-1",
                "repository": "acme/hello",
                "verdict": "request-changes"
            })
        );
    }

    #[gpui::test]
    async fn test_thread_writes_send_t3s_payloads(cx: &mut TestAppContext) {
        let (rpc, backend, _dir) = backend(cx);
        cx.update(|cx| backend.reply_to_thread("PRRT_1".into(), "Thanks!".into(), cx))
            .await
            .expect("the reply should be sent");
        assert_eq!(
            rpc.only_call::<rpc::PullRequestsReplyToThread>(),
            PullRequestThreadReplyInput {
                allow_stale: None,
                body: "Thanks!".to_string(),
                expected_account_id: None,
                host: Some("github.com".to_string()),
                number: 7,
                project_id: "project-1".to_string(),
                repository: "acme/hello".to_string(),
                thread_id: "PRRT_1".to_string(),
            }
        );

        rpc.calls.lock().clear();
        cx.update(|cx| backend.set_thread_resolution("PRRT_1".into(), false, cx))
            .await
            .expect("the resolution should be sent");
        assert_eq!(
            rpc.only_call::<rpc::PullRequestsSetThreadResolution>(),
            PullRequestThreadResolutionInput {
                allow_stale: None,
                expected_account_id: None,
                host: Some("github.com".to_string()),
                number: 7,
                project_id: "project-1".to_string(),
                repository: "acme/hello".to_string(),
                resolved: false,
                thread_id: "PRRT_1".to_string(),
            }
        );

        rpc.calls.lock().clear();
        cx.update(|cx| backend.update_comment("PRRC_9".into(), "Reworded".into(), cx))
            .await
            .expect("the edit should be sent");
        let edit = rpc.only_call::<rpc::PullRequestsUpdateComment>();
        assert_eq!(edit.kind, PullRequestCommentUpdateInputKind::ReviewComment);
        assert_eq!(
            (edit.comment_id.as_str(), edit.body.as_str()),
            ("PRRC_9", "Reworded")
        );

        rpc.calls.lock().clear();
        cx.update(|cx| {
            backend.set_files_viewed(
                vec![("src/main.rs".into(), true), ("old.txt".into(), false)],
                cx,
            )
        })
        .await
        .expect("the viewed marks should be sent");
        assert_eq!(
            rpc.only_call::<rpc::PullRequestsSetFilesViewed>().files,
            vec![
                PullRequestSetFilesViewedInputFilesItem {
                    path: "src/main.rs".to_string(),
                    viewed: true,
                },
                PullRequestSetFilesViewedInputFilesItem {
                    path: "old.txt".to_string(),
                    viewed: false,
                },
            ]
        );

        rpc.calls.lock().clear();
        cx.update(|cx| backend.post_comment("LGTM".into(), cx))
            .await
            .expect("the comment should be sent");
        assert_eq!(
            rpc.only_call::<rpc::PullRequestsComment>(),
            PullRequestCommentInput {
                allow_stale: None,
                body: "LGTM".to_string(),
                expected_account_id: None,
                host: Some("github.com".to_string()),
                number: 7,
                project_id: "project-1".to_string(),
                repository: "acme/hello".to_string(),
            }
        );
    }

    #[gpui::test]
    async fn test_typed_errors_are_explained(cx: &mut TestAppContext) {
        let (rpc, backend, _dir) = backend(cx);
        let failed = |error: Value| RpcError::Failed {
            tag: "pullRequests.submitReview".to_string(),
            error,
        };
        let submission = || ReviewSubmission {
            quotes: Default::default(),
            verdict: ReviewVerdict::Comment,
            body: "Hi".to_string(),
            comments: Vec::new(),
        };
        let submit = async |response: RpcError| {
            rpc.respond(Err(response));
            let error = cx
                .update(|cx| backend.submit_review(submission(), cx))
                .await
                .expect_err("the forge refused");
            format!("{error:#}")
        };

        assert_eq!(
            submit(failed(json!({
                "_tag": "EnvironmentAuthorizationError",
                "message": "Missing scope",
                "requiredScope": "orchestration:operate",
                "requiredPermission": "source-control:write"
            })))
            .await,
            "T3 refused: mustr's pairing doesn't include source-control:write. Pair again from \
             the home view."
        );
        assert_eq!(
            submit(failed(json!({
                "_tag": "PullRequestOperationError",
                "operation": "submitReview",
                "detail": "Unprocessable Entity: Pull request review thread line must be part of the diff"
            })))
            .await,
            "Unprocessable Entity: Pull request review thread line must be part of the diff"
        );
        assert_eq!(
            submit(failed(json!({
                "_tag": "PullRequestUnavailableError",
                "reason": "cli-unauthenticated"
            })))
            .await,
            "T3 can't reach the forge: its command-line tool isn't signed in on the T3 machine."
        );
        assert_eq!(
            submit(RpcError::Closed("socket ended".into())).await,
            "Lost the connection to T3. Your text is kept; try again once mustr reconnects."
        );
    }

    #[gpui::test]
    async fn test_dry_runs_send_no_writes(cx: &mut TestAppContext) {
        let rpc = Arc::new(DryRunRpc(RecordingRpc::default()));
        let dir = tempfile::tempdir().expect("a temporary directory");
        let backend = T3ReviewBackend::new(
            rpc.clone(),
            pull_request(),
            DraftStore::new(dir.path(), &pull_request()),
        );
        let error = cx
            .update(|cx| backend.reply_to_thread("PRRT_1".into(), "Hi".into(), cx))
            .await
            .expect_err("dry runs refuse writes");
        assert_eq!(
            format!("{error:#}"),
            "T3 protocol error: Not sent: MUSTR_DRY_RUN_WRITES is set."
        );
        assert!(rpc.0.calls.lock().is_empty());
        rpc.0.respond(Ok(json!({ "reviewThreads": [] })));
        cx.update(|cx| backend.reload_threads(cx))
            .await
            .expect("reads still go through");
        assert_eq!(rpc.0.calls.lock().len(), 1);
    }

    #[gpui::test]
    async fn test_threads_reload_through_the_same_rpcs(cx: &mut TestAppContext) {
        let (rpc, backend, _dir) = backend(cx);
        rpc.respond(Ok(json!({
            "reviewThreads": [{
                "id": "PRRT_1", "path": "src/main.rs", "line": 3, "side": "right",
                "isResolved": false, "isOutdated": false, "canResolve": true,
                "comments": [{ "id": "PRRC_1", "author": { "login": "me" }, "body": "Hm",
                               "createdAt": "2026-10-27T10:00:00Z", "canEdit": true }]
            }]
        })));
        let threads = cx
            .update(|cx| backend.reload_threads(cx))
            .await
            .expect("threads load");
        assert_eq!(threads.len(), 1);
        assert!(threads[0].can_resolve && threads[0].comments[0].can_edit);
        let calls = rpc.calls.lock();
        assert_eq!(calls[0].0, "pullRequests.activity");
        assert_eq!(
            calls[0].1,
            serde_json::to_value(pull_request()).expect("references encode")
        );
    }
}

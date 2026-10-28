//! The pull request data around a review's diff, read from T3: the details for the header and
//! what the viewer may write (`pullRequests.detail`), review threads (`pullRequests.activity`,
//! with longer threads paged through `pullRequests.threadComments`), and the viewer's viewed
//! marks (`pullRequests.filesViewed`).
//!
//! The serde structs here mirror just the fields mustr reads from T3's contracts
//! (`packages/contracts/src/pullRequest.ts`). They're kept together in this module so they're
//! easy to swap for the generated `t3_api` types.

use crate::{inbox::decode_each, review_backend::PullRequestRpc};
use anyhow::{Context as _, Result, anyhow};
use futures::StreamExt as _;
use review::{
    ChecksState, ChecksSummary, DiffSide, FileViewedState, PullRequestHeader, PullRequestState,
    ReviewPermissions, ReviewThread, ReviewVerdict, ThreadComment,
};
use serde::Deserialize;
use serde_json::{Value, json};
use std::collections::HashMap;
use t3_client::PullRequestRef;
use time::{OffsetDateTime, format_description::well_known::Rfc3339};

/// Pages of `pullRequests.threadComments` to follow per thread, as a guard against a cursor
/// that never ends.
const MAX_COMMENT_PAGES: usize = 20;
const COMMENT_REQUESTS_IN_FLIGHT: usize = 4;

#[derive(Deserialize)]
struct Actor {
    login: String,
}

/// `PullRequestDetail`
#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct Detail {
    repository: String,
    number: u64,
    title: String,
    #[serde(default)]
    author: Option<Actor>,
    state: String,
    #[serde(default)]
    is_draft: bool,
    head_branch: String,
    base_branch: String,
    #[serde(default)]
    additions: u64,
    #[serde(default)]
    deletions: u64,
    #[serde(default)]
    checks: Option<Value>,
    #[serde(default)]
    capabilities: Option<Capabilities>,
    #[serde(default)]
    viewer_permissions: Option<ViewerPermissions>,
}

/// `PullRequestCapabilities`: what the forge supports.
#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct Capabilities {
    /// Where the forge keeps viewed marks (`"host"` or `"environment"`); absent when it keeps none.
    #[serde(default)]
    viewed_files: Option<String>,
    #[serde(default)]
    comment: bool,
    #[serde(default)]
    review: Option<ReviewCapabilities>,
}

/// `PullRequestReviewCapabilities`
#[derive(Default, Deserialize)]
#[serde(rename_all = "camelCase")]
struct ReviewCapabilities {
    #[serde(default)]
    inline_comment: bool,
    #[serde(default)]
    reply: bool,
    #[serde(default)]
    resolve: bool,
    #[serde(default)]
    verdicts: Vec<String>,
}

/// `PullRequestViewerPermissions`: what the viewer may do on this pull request.
#[derive(Default, Deserialize)]
#[serde(rename_all = "camelCase")]
struct ViewerPermissions {
    #[serde(default)]
    comment: bool,
    #[serde(default)]
    resolve: bool,
    #[serde(default)]
    verdicts: Vec<String>,
}

/// `PullRequestCheck`
#[derive(Deserialize)]
struct Check {
    status: String,
}

/// `PullRequestActivity`
#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct Activity {
    #[serde(default)]
    review_threads: Option<Value>,
}

/// `PullRequestReviewThread`
#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct Thread {
    id: String,
    path: String,
    #[serde(default)]
    line: Option<u32>,
    side: String,
    #[serde(default)]
    is_resolved: bool,
    #[serde(default)]
    is_outdated: bool,
    #[serde(default)]
    comments: Option<Value>,
    #[serde(default)]
    comment_count: Option<usize>,
    #[serde(default)]
    next_comments_cursor: Option<String>,
    #[serde(default)]
    can_resolve: Option<bool>,
}

/// `PullRequestThreadComment`
#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct Comment {
    id: String,
    #[serde(default)]
    author: Option<Actor>,
    body: String,
    #[serde(default)]
    created_at: Option<String>,
    #[serde(default)]
    edited_at: Option<String>,
    #[serde(default)]
    can_edit: Option<bool>,
}

/// `PullRequestThreadCommentsResult`
#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct CommentsPage {
    #[serde(default)]
    comments: Option<Value>,
    #[serde(default)]
    next_cursor: Option<String>,
}

/// `PullRequestFilesViewedResult`
#[derive(Deserialize)]
struct FilesViewed {
    #[serde(default)]
    files: Option<Value>,
}

/// `PullRequestFileViewed`
#[derive(Deserialize)]
struct FileViewed {
    path: String,
    state: String,
}

#[derive(Debug, PartialEq, Eq)]
pub struct PullRequestDetail {
    pub header: PullRequestHeader,
    /// `false` when the forge keeps no viewed marks for this pull request.
    pub tracks_viewed_files: bool,
    /// What the forge supports and the viewer may do.
    pub permissions: ReviewPermissions,
}

pub fn parse_detail(value: Value) -> Result<PullRequestDetail> {
    let detail: Detail =
        serde_json::from_value(value).context("T3 sent pull request details mustr can't read")?;
    let state = match detail.state.as_str() {
        "open" => PullRequestState::Open,
        "closed" => PullRequestState::Closed,
        "merged" => PullRequestState::Merged,
        other => {
            log::warn!("unknown pull request state {other:?}, showing it as open");
            PullRequestState::Open
        }
    };
    let statuses = decode_each::<Check>(detail.checks.as_ref(), "check")
        .into_iter()
        .map(|check| check.status)
        .collect::<Vec<_>>();
    Ok(PullRequestDetail {
        header: PullRequestHeader {
            title: detail.title,
            repository: detail.repository,
            number: detail.number,
            author: detail.author.map(|author| author.login),
            base_branch: detail.base_branch,
            head_branch: detail.head_branch,
            state,
            is_draft: detail.is_draft,
            checks: checks_summary(&statuses),
            additions: detail.additions,
            deletions: detail.deletions,
        },
        tracks_viewed_files: detail
            .capabilities
            .as_ref()
            .is_some_and(|capabilities| capabilities.viewed_files.is_some()),
        permissions: permissions(detail.capabilities, detail.viewer_permissions),
    })
}

/// Both the forge and the viewer have to allow a write.
fn permissions(
    capabilities: Option<Capabilities>,
    viewer: Option<ViewerPermissions>,
) -> ReviewPermissions {
    let capabilities = capabilities.unwrap_or(Capabilities {
        viewed_files: None,
        comment: false,
        review: None,
    });
    let review = capabilities.review.unwrap_or_default();
    let viewer = viewer.unwrap_or_default();
    let verdict = |verdict: &String| match verdict.as_str() {
        "comment" => Some(ReviewVerdict::Comment),
        "approve" => Some(ReviewVerdict::Approve),
        "request-changes" => Some(ReviewVerdict::RequestChanges),
        _ => None,
    };
    let viewer_verdicts = viewer
        .verdicts
        .iter()
        .filter_map(verdict)
        .collect::<Vec<_>>();
    ReviewPermissions {
        line_comments: review.inline_comment && viewer.comment,
        reply: review.reply && viewer.comment,
        resolve: review.resolve && viewer.resolve,
        verdicts: review
            .verdicts
            .iter()
            .filter_map(verdict)
            .filter(|verdict| viewer_verdicts.contains(verdict))
            .collect(),
        conversation_comments: capabilities.comment && viewer.comment,
        mark_viewed: capabilities.viewed_files.is_some(),
        blocked: None,
    }
}

/// Rolls up check statuses the way T3's web app does (`pullRequestChecksState`): any failure or
/// cancellation fails, then anything pending or waiting for approval is pending.
fn checks_summary(statuses: &[String]) -> Option<ChecksSummary> {
    let count = |matches: &[&str]| {
        statuses
            .iter()
            .filter(|status| matches.contains(&status.as_str()))
            .count()
    };
    let failing = count(&["failure", "cancelled"]);
    let pending = count(&["pending", "action-required"]);
    let state = if failing > 0 {
        ChecksState::Failing
    } else if pending > 0 {
        ChecksState::Pending
    } else if count(&["success"]) > 0 {
        ChecksState::Passing
    } else {
        return None;
    };
    Some(ChecksSummary {
        state,
        total: statuses.len(),
        failing,
        pending,
    })
}

/// A thread from `pullRequests.activity`, and the cursor for the rest of its comments when T3
/// didn't send them all.
#[derive(Debug, PartialEq, Eq)]
pub struct ParsedThread {
    pub thread: ReviewThread,
    pub next_comments_cursor: Option<String>,
}

pub fn parse_review_threads(activity: Value) -> Result<Vec<ParsedThread>> {
    let activity: Activity = serde_json::from_value(activity)
        .context("T3 sent pull request activity mustr can't read")?;
    Ok(
        decode_each::<Thread>(activity.review_threads.as_ref(), "review thread")
            .into_iter()
            .map(|thread| {
                let comments = parse_comments(thread.comments.as_ref());
                let side = match thread.side.as_str() {
                    "left" => DiffSide::Left,
                    _ => DiffSide::Right,
                };
                ParsedThread {
                    thread: ReviewThread {
                        comment_count: thread.comment_count.unwrap_or(comments.len()),
                        sent_to_agent: false,
                        id: thread.id,
                        path: thread.path,
                        line: thread.line.filter(|line| *line > 0),
                        side,
                        is_resolved: thread.is_resolved,
                        is_outdated: thread.is_outdated,
                        // T3 leaves it out when the forge doesn't say; the forge decides then.
                        can_resolve: thread.can_resolve.unwrap_or(true),
                        comments,
                    },
                    next_comments_cursor: thread.next_comments_cursor,
                }
            })
            .collect(),
    )
}

/// One page of `pullRequests.threadComments`, and the cursor for the next.
pub fn parse_comments_page(page: Value) -> Result<(Vec<ThreadComment>, Option<String>)> {
    let page: CommentsPage =
        serde_json::from_value(page).context("T3 sent thread comments mustr can't read")?;
    Ok((parse_comments(page.comments.as_ref()), page.next_cursor))
}

fn parse_comments(comments: Option<&Value>) -> Vec<ThreadComment> {
    decode_each::<Comment>(comments, "thread comment")
        .into_iter()
        .map(|comment| ThreadComment {
            id: comment.id,
            author: comment.author.map(|author| author.login),
            body: comment.body,
            created_at: comment
                .created_at
                .and_then(|created_at| OffsetDateTime::parse(&created_at, &Rfc3339).ok()),
            edited: comment.edited_at.is_some(),
            can_edit: comment.can_edit.unwrap_or(false),
        })
        .collect()
}

/// The viewer's viewed marks by path. Files T3 doesn't list are unviewed.
pub fn parse_files_viewed(value: Value) -> Result<HashMap<String, FileViewedState>> {
    let viewed: FilesViewed =
        serde_json::from_value(value).context("T3 sent viewed files mustr can't read")?;
    Ok(
        decode_each::<FileViewed>(viewed.files.as_ref(), "viewed file")
            .into_iter()
            .map(|file| {
                let state = match file.state.as_str() {
                    "viewed" => FileViewedState::Viewed,
                    "dismissed" => FileViewedState::Dismissed,
                    _ => FileViewedState::Unviewed,
                };
                (file.path, state)
            })
            .collect(),
    )
}

/// Everything around the diff that a review shows. Each part is optional: the review opens
/// without whatever T3 couldn't provide, and `problems` says what that was.
#[derive(Default)]
pub struct PullRequestReviewData {
    pub header: Option<PullRequestHeader>,
    pub threads: Vec<ReviewThread>,
    pub viewed: Option<HashMap<String, FileViewedState>>,
    /// What the viewer may write. Without details, everything is offered and the forge decides.
    pub permissions: ReviewPermissions,
    pub problems: Vec<String>,
}

/// Calls one of T3's RPCs. T3's typed errors become their readable detail.
async fn request(rpc: &impl PullRequestRpc, tag: &'static str, payload: Value) -> Result<Value> {
    rpc.request(tag, payload)
        .await
        .map_err(|error| anyhow!(error.error_detail().unwrap_or_else(|| error.to_string())))
        .with_context(|| format!("calling {tag}"))
}

pub async fn load(
    rpc: &impl PullRequestRpc,
    pull_request: &PullRequestRef,
) -> PullRequestReviewData {
    let mut data = PullRequestReviewData {
        permissions: ReviewPermissions::all(),
        ..PullRequestReviewData::default()
    };
    let payload = match serde_json::to_value(pull_request) {
        Ok(payload) => payload,
        Err(error) => {
            data.problems
                .push(format!("the pull request reference: {error}"));
            return data;
        }
    };
    let (detail, threads, files_viewed) = futures::join!(
        request(rpc, "pullRequests.detail", payload.clone()),
        load_threads_from(rpc, &payload),
        request(rpc, "pullRequests.filesViewed", payload.clone()),
    );

    let mut tracks_viewed_files = true;
    match detail.and_then(parse_detail) {
        Ok(detail) => {
            tracks_viewed_files = detail.tracks_viewed_files;
            data.header = Some(detail.header);
            data.permissions = detail.permissions;
        }
        Err(error) => {
            log::error!("couldn't load pull request details: {error:#}");
            data.problems.push(format!("details ({error:#})"));
        }
    }

    match threads {
        Ok(threads) => data.threads = threads,
        Err(error) => {
            log::error!("couldn't load review threads: {error:#}");
            data.problems.push(format!("review threads ({error:#})"));
        }
    }

    if tracks_viewed_files {
        match files_viewed.and_then(parse_files_viewed) {
            Ok(viewed) => data.viewed = Some(viewed),
            Err(error) => {
                log::error!("couldn't load viewed files: {error:#}");
                data.problems.push(format!("viewed files ({error:#})"));
            }
        }
    }
    data
}

/// The pull request's review threads, with all their comments.
pub async fn load_threads(
    rpc: &impl PullRequestRpc,
    pull_request: &PullRequestRef,
) -> Result<Vec<ReviewThread>> {
    let payload = serde_json::to_value(pull_request).context("encoding the pull request")?;
    load_threads_from(rpc, &payload).await
}

async fn load_threads_from(
    rpc: &impl PullRequestRpc,
    payload: &Value,
) -> Result<Vec<ReviewThread>> {
    let threads = request(rpc, "pullRequests.activity", payload.clone())
        .await
        .and_then(parse_review_threads)?;
    Ok(futures::stream::iter(
        threads
            .into_iter()
            .map(|thread| load_remaining_comments(rpc, payload, thread)),
    )
    .buffered(COMMENT_REQUESTS_IN_FLIGHT)
    .collect()
    .await)
}

/// Follows a thread's comment cursor. A failure keeps the comments loaded so far; the card
/// then says how many more there are.
async fn load_remaining_comments(
    rpc: &impl PullRequestRpc,
    pull_request: &Value,
    parsed: ParsedThread,
) -> ReviewThread {
    let ParsedThread {
        mut thread,
        next_comments_cursor: mut cursor,
    } = parsed;
    for _ in 0..MAX_COMMENT_PAGES {
        let Some(page_cursor) = cursor.take() else {
            break;
        };
        let mut payload = pull_request.clone();
        payload["threadId"] = json!(thread.id);
        payload["cursor"] = json!(page_cursor);
        match request(rpc, "pullRequests.threadComments", payload)
            .await
            .and_then(parse_comments_page)
        {
            Ok((comments, next)) => {
                thread.comments.extend(comments);
                cursor = next.filter(|next| *next != page_cursor);
            }
            Err(error) => {
                log::warn!("couldn't load more comments of a review thread: {error:#}");
                break;
            }
        }
    }
    thread.comment_count = thread.comment_count.max(thread.comments.len());
    thread
}

#[cfg(test)]
mod tests {
    use super::*;
    use time::macros::datetime;

    fn actor(login: &str) -> Value {
        json!({ "login": login, "name": null, "avatarUrl": "https://avatars.example/u/1" })
    }

    /// Shaped like `PullRequestDetail` from T3's contracts, with made-up values.
    fn detail_fixture() -> Value {
        json!({
            "provider": "github",
            "capabilities": {
                "diff": true, "comment": true, "actions": ["merge"], "mergeMethods": ["squash"],
                "search": true, "viewedFiles": "host",
                "review": { "inlineComment": true, "reply": true, "resolve": true, "verdicts": ["comment"] },
                "reviewers": { "request": true, "listCandidates": true }
            },
            "viewerPermissions": {
                "actions": [], "comment": true, "resolve": true, "verdicts": ["comment"],
                "requestReviewers": false
            },
            "projectId": "project-1",
            "projectTitle": "hello",
            "workspaceRoot": "/src/hello",
            "repository": "acme/hello",
            "number": 7,
            "title": "Greet the whole world",
            "body": "Says hello to everyone.",
            "url": "https://github.com/acme/hello/pull/7",
            "author": actor("octocat"),
            "state": "open",
            "isDraft": true,
            "mergeability": "mergeable",
            "additions": 12,
            "deletions": 3,
            "changedFiles": 2,
            "headBranch": "greet-world",
            "headRepositoryNameWithOwner": "acme/hello",
            "headSha": "0123456789abcdef",
            "baseBranch": "main",
            "createdAt": "2026-10-20T09:00:00.000Z",
            "updatedAt": "2026-10-27T09:00:00.000Z",
            "observedAt": 1793091600000.0,
            "mergedAt": null,
            "closedAt": null,
            "reviewers": [actor("hubot")],
            "labels": [{ "name": "feature", "color": "0e8a16" }],
            "checks": [
                { "name": "build", "status": "success", "description": null, "url": null, "required": true },
                { "name": "lint", "status": "failure", "description": "2 errors", "url": null },
                { "name": "deploy", "status": "pending", "description": null, "url": null }
            ],
            "mergeCapabilities": { "merge": true, "squash": true, "rebase": false },
            "viewer": "octocat",
            "baseComparison": "up-to-date"
        })
    }

    /// Shaped like `PullRequestActivity`.
    fn activity_fixture() -> Value {
        json!({
            "author": actor("octocat"),
            "reviewers": [],
            "comments": [{
                "id": "IC_1", "kind": "issue-comment", "author": actor("hubot"), "body": "Nice!",
                "createdAt": "2026-10-21T10:00:00Z", "url": null, "path": null, "reviewState": null
            }],
            "commentCount": 1,
            "commentsTruncated": false,
            "reviewThreadsTruncated": false,
            "reviewThreads": [
                {
                    "id": "PRRT_1",
                    "path": "src/main.rs",
                    "line": 2,
                    "side": "right",
                    "isResolved": false,
                    "isOutdated": false,
                    "comments": [
                        {
                            "id": "PRRC_1", "author": actor("hubot"),
                            "body": "Should this say **hello, world**?",
                            "createdAt": "2026-10-21T10:00:00Z", "editedAt": null,
                            "url": "https://github.com/acme/hello/pull/7#discussion_r1",
                            "reactions": []
                        },
                        {
                            "id": "PRRC_2", "author": null, "body": "Yes.",
                            "createdAt": "2026-10-21T11:30:00.123Z",
                            "editedAt": "2026-10-21T11:31:00Z", "url": null, "canEdit": true
                        }
                    ],
                    "commentCount": 3,
                    "nextCommentsCursor": "cursor-1",
                    "canResolve": true
                },
                {
                    "id": "PRRT_2",
                    "path": "README.md",
                    "line": null,
                    "side": "left",
                    "isResolved": true,
                    "isOutdated": true,
                    "canResolve": false,
                    "comments": [{
                        "id": "PRRC_3", "author": actor("octocat"), "body": "Fixed.",
                        "createdAt": "not a date", "url": null
                    }]
                },
                { "id": "PRRT_3", "path": "broken.rs" }
            ],
            "commits": [],
            "reactions": []
        })
    }

    #[test]
    fn parses_pull_request_details() -> Result<()> {
        let detail = parse_detail(detail_fixture())?;
        assert_eq!(
            detail,
            PullRequestDetail {
                header: PullRequestHeader {
                    title: "Greet the whole world".to_string(),
                    repository: "acme/hello".to_string(),
                    number: 7,
                    author: Some("octocat".to_string()),
                    base_branch: "main".to_string(),
                    head_branch: "greet-world".to_string(),
                    state: PullRequestState::Open,
                    is_draft: true,
                    checks: Some(ChecksSummary {
                        state: ChecksState::Failing,
                        total: 3,
                        failing: 1,
                        pending: 1,
                    }),
                    additions: 12,
                    deletions: 3,
                },
                tracks_viewed_files: true,
                permissions: ReviewPermissions {
                    line_comments: true,
                    reply: true,
                    resolve: true,
                    verdicts: vec![ReviewVerdict::Comment],
                    conversation_comments: true,
                    mark_viewed: true,
                    blocked: None,
                },
            }
        );

        let mut merged = detail_fixture();
        merged["state"] = json!("merged");
        merged["author"] = json!(null);
        merged["checks"] = json!([]);
        merged["capabilities"]
            .as_object_mut()
            .map(|capabilities| capabilities.remove("viewedFiles"));
        let merged = parse_detail(merged)?;
        assert_eq!(merged.header.state, PullRequestState::Merged);
        assert_eq!(merged.header.author, None);
        assert_eq!(merged.header.checks, None);
        assert!(!merged.tracks_viewed_files);
        assert!(!merged.permissions.mark_viewed);

        // The viewer can't approve their own pull request, and can't resolve here.
        let mut own = detail_fixture();
        own["capabilities"]["review"]["verdicts"] =
            json!(["comment", "approve", "request-changes"]);
        own["viewerPermissions"]["verdicts"] =
            json!(["comment", "request-changes", "newer-verdict"]);
        own["viewerPermissions"]["resolve"] = json!(false);
        let own = parse_detail(own)?.permissions;
        assert_eq!(
            own.verdicts,
            [ReviewVerdict::Comment, ReviewVerdict::RequestChanges]
        );
        assert!(!own.resolve && own.reply && own.line_comments);

        assert!(parse_detail(json!({ "title": "no repository" })).is_err());
        Ok(())
    }

    #[test]
    fn rolls_up_checks_like_t3() {
        let summary = |statuses: &[&str]| {
            checks_summary(&statuses.iter().map(|s| s.to_string()).collect::<Vec<_>>())
                .map(|summary| summary.state)
        };
        assert_eq!(summary(&[]), None);
        assert_eq!(summary(&["skipped", "neutral"]), None);
        assert_eq!(summary(&["success", "skipped"]), Some(ChecksState::Passing));
        assert_eq!(
            summary(&["success", "action-required"]),
            Some(ChecksState::Pending)
        );
        assert_eq!(
            summary(&["pending", "cancelled"]),
            Some(ChecksState::Failing)
        );
    }

    #[test]
    fn parses_review_threads_skipping_unreadable_ones() -> Result<()> {
        let threads = parse_review_threads(activity_fixture())?;
        assert_eq!(
            threads,
            vec![
                ParsedThread {
                    thread: ReviewThread {
                        id: "PRRT_1".to_string(),
                        path: "src/main.rs".to_string(),
                        line: Some(2),
                        side: DiffSide::Right,
                        is_resolved: false,
                        is_outdated: false,
                        can_resolve: true,
                        comments: vec![
                            ThreadComment {
                                id: "PRRC_1".to_string(),
                                author: Some("hubot".to_string()),
                                body: "Should this say **hello, world**?".to_string(),
                                created_at: Some(datetime!(2026-10-21 10:00 UTC)),
                                edited: false,
                                can_edit: false,
                            },
                            ThreadComment {
                                id: "PRRC_2".to_string(),
                                author: None,
                                body: "Yes.".to_string(),
                                created_at: Some(datetime!(2026-10-21 11:30:00.123 UTC)),
                                edited: true,
                                can_edit: true,
                            },
                        ],
                        comment_count: 3,
                        sent_to_agent: false,
                    },
                    next_comments_cursor: Some("cursor-1".to_string()),
                },
                ParsedThread {
                    thread: ReviewThread {
                        id: "PRRT_2".to_string(),
                        path: "README.md".to_string(),
                        line: None,
                        side: DiffSide::Left,
                        is_resolved: true,
                        is_outdated: true,
                        can_resolve: false,
                        comments: vec![ThreadComment {
                            id: "PRRC_3".to_string(),
                            author: Some("octocat".to_string()),
                            body: "Fixed.".to_string(),
                            created_at: None,
                            edited: false,
                            can_edit: false,
                        }],
                        comment_count: 1,
                        sent_to_agent: false,
                    },
                    next_comments_cursor: None,
                },
            ]
        );
        assert_eq!(parse_review_threads(json!({}))?, Vec::new());
        Ok(())
    }

    #[test]
    fn parses_thread_comment_pages() -> Result<()> {
        let (comments, next) = parse_comments_page(json!({
            "comments": [{
                "id": "PRRC_4", "author": actor("hubot"), "body": "One more thing.",
                "createdAt": "2026-10-22T08:00:00Z", "editedAt": null, "url": null
            }],
            "nextCursor": null
        }))?;
        assert_eq!(next, None);
        assert_eq!(comments.len(), 1);
        assert_eq!(comments[0].body, "One more thing.");
        Ok(())
    }

    #[test]
    fn parses_viewed_files() -> Result<()> {
        let viewed = parse_files_viewed(json!({
            "files": [
                { "path": "src/main.rs", "state": "viewed" },
                { "path": "README.md", "state": "dismissed" },
                { "path": "a file with spaces .txt", "state": "unviewed" },
                { "path": "odd.rs", "state": "something-new" },
                { "state": "viewed" }
            ],
            "truncated": false
        }))?;
        assert_eq!(
            viewed,
            [
                ("src/main.rs".to_string(), FileViewedState::Viewed),
                ("README.md".to_string(), FileViewedState::Dismissed),
                (
                    "a file with spaces .txt".to_string(),
                    FileViewedState::Unviewed
                ),
                ("odd.rs".to_string(), FileViewedState::Unviewed),
            ]
            .into_iter()
            .collect()
        );
        Ok(())
    }
}

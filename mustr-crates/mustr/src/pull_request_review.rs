//! The pull request data around a review's diff, read from T3: the details for the header and
//! what the viewer may write (`pullRequests.detail`), review threads (`pullRequests.activity`,
//! with longer threads paged through `pullRequests.threadComments`), and the viewer's viewed
//! marks (`pullRequests.filesViewed`).
//!
//! The serde structs here mirror just the fields mustr reads from T3's contracts
//! (`packages/contracts/src/pullRequest.ts`). They're kept together in this module so they're
//! easy to swap for the generated `t3_api` types.
//!
//! It also opens a pull request's review ([`open_review`]), and puts the pull request's own
//! actions in the review's header ([`PullRequestActions`]): merging, marking ready, and closing
//! (`pullRequests.runAction`, each asking to confirm first where T3 does), and Work on PR, which
//! checks the pull request out for a new agent thread (`git.resolvePullRequest`, then
//! `git.preparePullRequestThread`, then `thread_chat::NewThreadInWorktree`).

use crate::{
    inbox::{self, decode_each},
    nav_panel::NavPanel,
    review_backend::{self, DryRunRpc, PullRequestRpc, T3ReviewBackend},
    review_drafts::{self, DraftStore},
    t3_connection::T3Connection,
    thread_git::{MenuSlot, menu_position},
};
use anyhow::{Context as _, Result, anyhow};
use futures::StreamExt as _;
use gpui::{
    AnyElement, App, AppContext as _, ClickEvent, Context, Entity, IntoElement, ParentElement as _,
    Render, SharedString, Styled as _, Task, WeakEntity, Window,
};
use mustr_ui::{ButtonStyle, SplitButton, TextButton, tokens::scaled};
use review::{
    ChecksState, ChecksSummary, DiffSide, FileViewedState, PullRequestHeader, PullRequestState,
    ReviewContents, ReviewHeader, ReviewPermissions, ReviewThread, ReviewVerdict, ReviewView,
    ThreadComment,
};
use serde::Deserialize;
use serde_json::{Value, json};
use std::{collections::HashMap, sync::Arc};
use t3_client::{
    PullRequestRef,
    t3_api::{
        rpc,
        types::{
            GitPreparePullRequestThreadInput, GitPreparePullRequestThreadInputMode,
            GitPreparePullRequestThreadResult, GitPullRequestRefInput, PullRequestAction,
            PullRequestActionInput, PullRequestMergeMethod,
        },
    },
};
use thread_chat::{
    Access, NewThreadInWorktree, ThreadBackend, ThreadBackendExt as _, ThreadShells,
};
use time::{OffsetDateTime, format_description::well_known::Rfc3339};
use ui::{Color, ContextMenu, Label, LabelCommon as _, LabelSize, prelude::*};
use workspace::{Toast, Workspace, notifications::NotificationId};

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
    url: Option<String>,
    #[serde(default)]
    mergeability: Option<String>,
    /// Which merge methods the repository allows.
    #[serde(default)]
    merge_capabilities: Option<HashMap<String, bool>>,
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
    /// What can be done to the pull request itself, such as `merge` or `close`.
    #[serde(default)]
    actions: Vec<String>,
    #[serde(default)]
    merge_methods: Vec<String>,
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
    actions: Vec<String>,
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
    pub capabilities: PullRequestCapabilities,
}

/// What can be done to the pull request itself, as T3's `PullRequestDetailActions` decides it.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct PullRequestCapabilities {
    pub url: Option<String>,
    /// Actions both the forge and the viewer allow (`capabilities.actions` and
    /// `viewerPermissions.actions`), such as `merge`, `ready`, and `close`.
    pub actions: Vec<String>,
    /// The merge methods the forge offers that the repository allows, in the forge's order.
    pub merge_methods: Vec<PullRequestMergeMethod>,
    /// The pull request conflicts with its base, so it can't be merged as it is.
    pub conflicting: bool,
}

impl PullRequestCapabilities {
    pub fn can(&self, action: &str) -> bool {
        self.actions.iter().any(|allowed| allowed == action)
    }
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
    let viewer_actions = detail
        .viewer_permissions
        .as_ref()
        .map(|viewer| viewer.actions.clone())
        .unwrap_or_default();
    let capabilities = PullRequestCapabilities {
        url: detail.url.clone(),
        actions: detail
            .capabilities
            .as_ref()
            .map(|capabilities| {
                capabilities
                    .actions
                    .iter()
                    .filter(|action| viewer_actions.contains(action))
                    .cloned()
                    .collect()
            })
            .unwrap_or_default(),
        merge_methods: detail
            .capabilities
            .as_ref()
            .map(|capabilities| {
                capabilities
                    .merge_methods
                    .iter()
                    .filter(|method| {
                        detail
                            .merge_capabilities
                            .as_ref()
                            .is_none_or(|allowed| allowed.get(*method).copied().unwrap_or(false))
                    })
                    .map(|method| PullRequestMergeMethod::from(method.as_str()))
                    .collect()
            })
            .unwrap_or_default(),
        conflicting: detail.mergeability.as_deref() == Some("conflicting"),
    };
    Ok(PullRequestDetail {
        capabilities,
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
        actions: Vec::new(),
        merge_methods: Vec::new(),
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
pub(crate) fn checks_summary(statuses: &[String]) -> Option<ChecksSummary> {
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
    /// What can be done to the pull request itself, once its details have loaded.
    pub capabilities: Option<PullRequestCapabilities>,
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
            data.capabilities = Some(detail.capabilities);
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

// ---------------------------------------------------------------------------------------------
// Opening a pull request's review

/// `owner/repo#123`
pub fn display_ref(reference: &PullRequestRef) -> String {
    format!("{}#{}", reference.repository, reference.number)
}

/// The open review of this pull request, if there is one.
fn open_review_of(
    workspace: &Workspace,
    reference: &PullRequestRef,
    cx: &App,
) -> Option<Entity<ReviewView>> {
    workspace.items_of_type::<ReviewView>(cx).find(|review| {
        review
            .read(cx)
            .header()
            .and_then(ReviewHeader::pull_request)
            .is_some_and(|header| {
                header.repository == reference.repository
                    && i64::try_from(header.number).ok() == Some(reference.number)
            })
    })
}

/// Opens mustr's review of a pull request, or shows it if it's open: its files, the data
/// around them, and the saved drafts, which load side by side; then the pull request's actions
/// go in its header.
pub fn open_review(
    workspace: WeakEntity<Workspace>,
    reference: PullRequestRef,
    window: &mut Window,
    cx: &mut App,
) -> Task<Result<()>> {
    let existing = workspace
        .read_with(cx, |workspace, cx| {
            open_review_of(workspace, &reference, cx)
        })
        .ok()
        .flatten();
    if let Some(existing) = existing {
        workspace
            .update(cx, |workspace, cx| {
                workspace.activate_item(&existing, true, true, window, cx)
            })
            .ok();
        return Task::ready(Ok(()));
    }
    let (session, rpc) = {
        let connection = T3Connection::global(cx).read(cx);
        (connection.session(), connection.current_rpc())
    };
    let Some(session) = session else {
        return Task::ready(Err(anyhow!("mustr isn't connected to T3")));
    };
    window.spawn(cx, async move |cx| {
        let display_ref = display_ref(&reference);
        let drafts = DraftStore::new(&review_drafts::drafts_dir(), &reference);
        // The files, the data around them (header, threads, viewed marks), and the saved
        // drafts load side by side; the files take longest.
        let files = cx.background_spawn({
            let session = session.clone();
            let reference = reference.clone();
            async move { session.pull_request_files(&reference).await }
        });
        let review_data = cx.background_spawn({
            let rpc = rpc.clone();
            let reference = reference.clone();
            async move { load(&rpc, &reference).await }
        });
        let pending = cx.background_spawn({
            let drafts = drafts.clone();
            async move { drafts.load() }
        });
        let (files, mut review_data, pending) = futures::join!(files, review_data, pending);
        let pending = pending.unwrap_or_else(|error| {
            log::error!("couldn't load the pending review: {error:#}");
            review_data.problems.push(format!("{error:#}"));
            Default::default()
        });
        let files = files.with_context(|| format!("loading {display_ref}"))?;
        let review = inbox::review_files(files);
        if review.files.is_empty() {
            return Err(anyhow!(
                inbox::unavailable_note(&display_ref, &review.unavailable)
                    .unwrap_or_else(|| format!("{display_ref} has no changed files"))
            ));
        }
        log::info!(
            "Opening {display_ref}: {} files, {} not shown, {} review threads, {} drafts \
             (kept in {})",
            review.files.len(),
            review.unavailable.len(),
            review_data.threads.len(),
            pending.comments.len(),
            drafts.path().display(),
        );
        let mut permissions = review_data.permissions;
        permissions.blocked = session.write_blocked_reason().map(Into::into);
        let header = review_data.header.clone();
        let capabilities = review_data.capabilities.clone();
        let contents = ReviewContents {
            header: review_data.header.map(ReviewHeader::PullRequest),
            worktree: None,
            files: review.files,
            // The file tree lists these, dimmed, with the reason they aren't shown.
            unavailable: review.unavailable,
            threads: review_data.threads,
            viewed: review_data.viewed,
            backend: Some(if review_backend::dry_run_requested() {
                log::warn!(
                    "{} is set: the review won't write to {display_ref}",
                    review_backend::DRY_RUN_ENV
                );
                Arc::new(T3ReviewBackend::new(
                    Arc::new(DryRunRpc(rpc)),
                    reference.clone(),
                    drafts,
                ))
            } else {
                Arc::new(T3ReviewBackend::new(
                    Arc::new(rpc),
                    reference.clone(),
                    drafts,
                ))
            }),
            permissions,
            pending,
        };
        let open_review = workspace.update_in(cx, |workspace, window, cx| {
            ReviewView::open(contents, workspace, window, cx)
        })?;
        let review = open_review.await?;
        workspace.update_in(cx, |workspace, window, cx| {
            let actions = cx.new(|cx| {
                PullRequestActions::new(
                    reference,
                    header,
                    capabilities.unwrap_or_default(),
                    review.downgrade(),
                    workspace.weak_handle(),
                    cx,
                )
            });
            review.update(cx, |review, cx| {
                review.set_header_actions(Some(actions.into()), cx)
            });
            workspace.open_panel::<NavPanel>(window, cx);
            if !review_data.problems.is_empty() {
                let id = NotificationId::composite::<PullRequestActions>(display_ref.clone());
                let note = format!(
                    "{display_ref}: couldn't load {}",
                    review_data.problems.join("; ")
                );
                workspace.show_toast(Toast::new(id, note), cx);
            }
        })?;
        Ok(())
    })
}

// ---------------------------------------------------------------------------------------------
// The pull request's own actions

pub fn merge_method_label(method: &PullRequestMergeMethod) -> &str {
    match method {
        PullRequestMergeMethod::Merge => "Merge",
        PullRequestMergeMethod::Squash => "Squash and merge",
        PullRequestMergeMethod::Rebase => "Rebase and merge",
        PullRequestMergeMethod::Unrecognized(method) => method,
    }
}

/// `pullRequests.runAction`'s input for one of the pull request's actions.
pub fn run_action_input(
    reference: &PullRequestRef,
    action: PullRequestAction,
    merge_method: Option<PullRequestMergeMethod>,
) -> PullRequestActionInput {
    PullRequestActionInput {
        action,
        allow_stale: None,
        expected_account_id: None,
        expected_stack_heads: None,
        host: reference.host.clone(),
        merge_method,
        number: reference.number,
        project_id: reference.project_id.clone(),
        repository: reference.repository.clone(),
        stack_number: None,
        update_method: None,
    }
}

/// The reference Work on PR gives T3: the pull request's URL, or its number.
pub fn checkout_reference(reference: &PullRequestRef, url: Option<&str>) -> String {
    url.filter(|url| !url.trim().is_empty())
        .map_or_else(|| reference.number.to_string(), str::to_string)
}

/// Where Work on PR checks the pull request out.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CheckoutMode {
    /// A worktree of its own.
    Worktree,
    /// The project's checkout.
    Local,
}

/// Checks a pull request out for a new agent thread, as T3's "Checkout pull request" dialog
/// does: resolves the reference in the project's checkout, prepares the branch (and worktree),
/// and says where the new thread should start.
pub async fn prepare_thread(
    backend: Arc<dyn ThreadBackend>,
    project_id: String,
    cwd: String,
    reference: String,
    mode: CheckoutMode,
) -> Result<(NewThreadInWorktree, GitPreparePullRequestThreadResult)> {
    backend
        .call::<rpc::GitResolvePullRequest>(
            GitPullRequestRefInput {
                cwd: cwd.clone(),
                reference: reference.clone(),
            },
            Access::Read,
        )
        .await
        .context("finding the pull request")?;
    let prepared = backend
        .call::<rpc::GitPreparePullRequestThread>(
            GitPreparePullRequestThreadInput {
                cwd,
                mode: match mode {
                    CheckoutMode::Worktree => GitPreparePullRequestThreadInputMode::Worktree,
                    CheckoutMode::Local => GitPreparePullRequestThreadInputMode::Local,
                },
                reference,
                thread_id: None,
            },
            Access::SourceControlWrite,
        )
        .await
        .context("checking out the pull request")?;
    Ok((
        NewThreadInWorktree {
            project_id,
            worktree_path: prepared.worktree_path.clone(),
            branch: Some(prepared.branch.clone()),
        },
        prepared,
    ))
}

/// An action that asks to be confirmed first, as T3 asks before merging or closing.
#[derive(Clone, Debug, PartialEq, Eq)]
enum Armed {
    Merge(PullRequestMergeMethod),
    Close,
}

/// The pull request's actions in its review's header: Work on PR ▾, Ready for review, Merge ▾,
/// and Close.
pub struct PullRequestActions {
    reference: PullRequestRef,
    header: Option<PullRequestHeader>,
    capabilities: PullRequestCapabilities,
    review: WeakEntity<ReviewView>,
    workspace: WeakEntity<Workspace>,
    merge_method: Option<PullRequestMergeMethod>,
    armed: Option<Armed>,
    running: Option<SharedString>,
    /// The state an action left the pull request in, until the review is opened again.
    state: Option<PullRequestState>,
    is_draft: Option<bool>,
    menu: MenuSlot,
    _task: Option<Task<()>>,
}

impl PullRequestActions {
    pub fn new(
        reference: PullRequestRef,
        header: Option<PullRequestHeader>,
        capabilities: PullRequestCapabilities,
        review: WeakEntity<ReviewView>,
        workspace: WeakEntity<Workspace>,
        _cx: &mut Context<Self>,
    ) -> Self {
        Self {
            merge_method: capabilities.merge_methods.first().cloned(),
            reference,
            header,
            capabilities,
            review,
            workspace,
            armed: None,
            running: None,
            state: None,
            is_draft: None,
            menu: MenuSlot::default(),
            _task: None,
        }
    }

    fn state(&self) -> PullRequestState {
        self.state.unwrap_or_else(|| {
            self.header
                .as_ref()
                .map_or(PullRequestState::Open, |header| header.state)
        })
    }

    fn is_draft(&self) -> bool {
        self.is_draft
            .unwrap_or_else(|| self.header.as_ref().is_some_and(|header| header.is_draft))
    }

    fn notify_review(&self, notice: impl Into<SharedString>, cx: &mut App) {
        let notice = notice.into();
        self.review
            .update(cx, |review, cx| review.set_notice(Some(notice), cx))
            .ok();
    }

    /// Runs one of the pull request's actions through `pullRequests.runAction`.
    pub fn run(
        &mut self,
        action: PullRequestAction,
        merge_method: Option<PullRequestMergeMethod>,
        cx: &mut Context<Self>,
    ) {
        let Some(backend) = thread_chat::backend(cx) else {
            self.notify_review("mustr isn't connected to T3", cx);
            return;
        };
        self.armed = None;
        let (running, done, failed) = match &action {
            PullRequestAction::Merge => ("Merging…", "Merge requested", "Couldn't merge"),
            PullRequestAction::Ready => (
                "Marking ready…",
                "Marked ready for review",
                "Couldn't mark it ready",
            ),
            PullRequestAction::Close => ("Closing…", "Pull request closed", "Couldn't close it"),
            _ => ("Working…", "Done", "Couldn't do that"),
        };
        self.running = Some(running.into());
        cx.notify();
        let call = backend.call::<rpc::PullRequestsRunAction>(
            run_action_input(&self.reference, action.clone(), merge_method),
            Access::SourceControlWrite,
        );
        self._task = Some(cx.spawn(async move |this, cx| {
            let result = call.await;
            this.update(cx, |this, cx| {
                this.running = None;
                match result {
                    Ok(()) => {
                        match action {
                            PullRequestAction::Merge => this.state = Some(PullRequestState::Merged),
                            PullRequestAction::Close => this.state = Some(PullRequestState::Closed),
                            PullRequestAction::Ready => this.is_draft = Some(false),
                            _ => {}
                        }
                        this.notify_review(done, cx);
                    }
                    Err(error) => {
                        log::error!("{failed}: {error:#}");
                        this.notify_review(format!("{failed}: {error:#}"), cx);
                    }
                }
                cx.notify();
            })
            .ok();
        }));
    }

    /// Checks the pull request out for a new agent thread, then opens the new thread's form
    /// there.
    pub fn work_on(&mut self, mode: CheckoutMode, window: &mut Window, cx: &mut Context<Self>) {
        let Some(backend) = thread_chat::backend(cx) else {
            self.notify_review("mustr isn't connected to T3", cx);
            return;
        };
        let project_id = self.reference.project_id.clone();
        let cwd = ThreadShells::global(cx)
            .read(cx)
            .shell()
            .and_then(|shell| shell.projects.get(&project_id))
            .map(|project| project.workspace_root.clone());
        let Some(cwd) = cwd else {
            self.notify_review(
                "T3 hasn't said where this pull request's project is checked out yet",
                cx,
            );
            return;
        };
        let reference = checkout_reference(&self.reference, self.capabilities.url.as_deref());
        self.running = Some(match mode {
            CheckoutMode::Worktree => "Preparing a worktree…".into(),
            CheckoutMode::Local => "Checking out…".into(),
        });
        cx.notify();
        let workspace = self.workspace.clone();
        let prepare = prepare_thread(backend, project_id, cwd, reference, mode);
        self._task = Some(cx.spawn_in(window, async move |this, cx| {
            let result = prepare.await;
            this.update_in(cx, |this, window, cx| {
                this.running = None;
                match result {
                    Ok((new_thread, prepared)) => {
                        let place = prepared
                            .worktree_path
                            .clone()
                            .unwrap_or_else(|| "the project's checkout".to_string());
                        let mut notice = format!("Checked out {} in {place}", prepared.branch);
                        if prepared.is_on_pull_request_head == Some(false) {
                            notice.push_str(
                                ", but not at its latest commit: the worktree has local changes",
                            );
                        }
                        this.notify_review(notice, cx);
                        workspace
                            .update(cx, |_, cx| window.dispatch_action(Box::new(new_thread), cx))
                            .ok();
                    }
                    Err(error) => {
                        log::error!("couldn't prepare a thread for the pull request: {error:#}");
                        this.notify_review(format!("Couldn't check it out: {error:#}"), cx);
                    }
                }
                cx.notify();
            })
            .ok();
        }));
    }

    fn deploy_work_menu(
        &mut self,
        event: &ClickEvent,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let this = cx.weak_entity();
        let menu = ContextMenu::build(window, cx, move |menu, _, _| {
            let worktree = this.clone();
            let local = this;
            menu.entry("In a new worktree", None, move |window, cx| {
                worktree
                    .update(cx, |this, cx| {
                        this.work_on(CheckoutMode::Worktree, window, cx)
                    })
                    .ok();
            })
            .entry("In the project's checkout", None, move |window, cx| {
                local
                    .update(cx, |this, cx| this.work_on(CheckoutMode::Local, window, cx))
                    .ok();
            })
        });
        self.menu
            .deploy(menu_position(event, window), menu, window, cx, |this| {
                &mut this.menu
            });
    }

    fn deploy_merge_menu(
        &mut self,
        event: &ClickEvent,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let this = cx.weak_entity();
        let methods = self.capabilities.merge_methods.clone();
        let current = self.merge_method.clone();
        let menu = ContextMenu::build(window, cx, move |mut menu, _, _| {
            for method in methods {
                let this = this.clone();
                let checked = current.as_ref() == Some(&method);
                menu = menu.toggleable_entry(
                    merge_method_label(&method).to_string(),
                    checked,
                    IconPosition::Start,
                    None,
                    move |_, cx| {
                        let method = method.clone();
                        this.update(cx, |this, cx| {
                            this.merge_method = Some(method);
                            cx.notify();
                        })
                        .ok();
                    },
                );
            }
            menu
        });
        self.menu
            .deploy(menu_position(event, window), menu, window, cx, |this| {
                &mut this.menu
            });
    }

    fn render_confirm(&self, armed: &Armed, cx: &mut Context<Self>) -> AnyElement {
        let base = self
            .header
            .as_ref()
            .map_or("its base".to_string(), |header| header.base_branch.clone());
        let (question, confirm) = match armed {
            Armed::Merge(method) => (
                format!("{} into {base}?", merge_method_label(method)),
                "Confirm merge",
            ),
            Armed::Close => ("Close without merging?".to_string(), "Confirm close"),
        };
        let armed = armed.clone();
        h_flex()
            .gap_2()
            .child(Label::new(question).size(LabelSize::Small))
            .child(
                TextButton::new("pull-request-cancel", "Cancel").on_click(cx.listener(
                    |this, _, _, cx| {
                        this.armed = None;
                        cx.notify();
                    },
                )),
            )
            .child(
                TextButton::new("pull-request-confirm", confirm)
                    .style(ButtonStyle::Primary)
                    .on_click(cx.listener(move |this, _, _, cx| match armed.clone() {
                        Armed::Merge(method) => {
                            this.run(PullRequestAction::Merge, Some(method), cx)
                        }
                        Armed::Close => this.run(PullRequestAction::Close, None, cx),
                    })),
            )
            .into_any_element()
    }
}

impl Render for PullRequestActions {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let blocked = thread_chat::backend(cx)
            .and_then(|backend| backend.blocked_reason(Access::SourceControlWrite));
        if let Some(armed) = self.armed.clone() {
            return self.render_confirm(&armed, cx);
        }
        let state = self.state();
        let is_open = state == PullRequestState::Open;
        let is_draft = self.is_draft();
        let capabilities = &self.capabilities;
        let can_ready = is_open && is_draft && capabilities.can("ready");
        let merge_method = self
            .merge_method
            .clone()
            .filter(|_| is_open && !is_draft && capabilities.can("merge"));
        let can_close = is_open && capabilities.can("close");
        let conflicting = capabilities.conflicting;
        let several_methods = capabilities.merge_methods.len() > 1;
        let busy = self.running.is_some();
        let write_disabled = busy || blocked.is_some();
        let tooltip = |text: &str| blocked.clone().unwrap_or_else(|| text.to_string());
        let weak = cx.weak_entity();
        let work_menu = weak.clone();
        let merge_menu = weak.clone();

        h_flex()
            .id("pull-request-actions")
            .flex_none()
            .gap(scaled(px(6.)))
            .when_some(self.running.clone(), |this, running| {
                this.child(
                    Label::new(running)
                        .size(LabelSize::Small)
                        .color(Color::Muted),
                )
            })
            .when(is_open, |this| {
                this.child(
                    SplitButton::new("pull-request-work-on", "Work on PR")
                        .disabled(write_disabled)
                        .tooltip(tooltip(
                            "Check the pull request out in a new worktree, for a new agent thread",
                        ))
                        .menu_tooltip("Where to check it out")
                        .on_click(move |_, window, cx| {
                            weak.update(cx, |this, cx| {
                                this.work_on(CheckoutMode::Worktree, window, cx)
                            })
                            .ok();
                        })
                        .on_menu_click(move |event, window, cx| {
                            work_menu
                                .update(cx, |this, cx| this.deploy_work_menu(event, window, cx))
                                .ok();
                        }),
                )
            })
            .when(can_ready, |this| {
                this.child(
                    TextButton::new("pull-request-ready", "Ready for review")
                        .disabled(write_disabled)
                        .tooltip(tooltip("Take the pull request out of draft"))
                        .on_click(cx.listener(|this, _, _, cx| {
                            this.run(PullRequestAction::Ready, None, cx)
                        })),
                )
            })
            .when(can_close, |this| {
                this.child(
                    TextButton::new("pull-request-close", "Close")
                        .disabled(write_disabled)
                        .tooltip(tooltip("Close the pull request without merging"))
                        .on_click(cx.listener(|this, _, _, cx| {
                            this.armed = Some(Armed::Close);
                            cx.notify();
                        })),
                )
            })
            .when_some(merge_method, |this, method| {
                let label = merge_method_label(&method).to_string();
                let tooltip = if conflicting {
                    "The pull request conflicts with its base; resolve the conflicts first"
                        .to_string()
                } else {
                    tooltip("Merge the pull request")
                };
                let disabled = write_disabled || conflicting;
                let arm = cx.listener(move |this: &mut Self, _: &ClickEvent, _, cx| {
                    this.armed = Some(Armed::Merge(method.clone()));
                    cx.notify();
                });
                // The ▾ only shows when there's a choice of merge methods.
                if several_methods {
                    this.child(
                        SplitButton::new("pull-request-merge", label)
                            .style(ButtonStyle::Primary)
                            .disabled(disabled)
                            .tooltip(tooltip)
                            .on_click(arm)
                            .menu_tooltip("Choose how to merge")
                            .on_menu_click(move |event, window, cx| {
                                merge_menu
                                    .update(cx, |this, cx| {
                                        this.deploy_merge_menu(event, window, cx)
                                    })
                                    .ok();
                            }),
                    )
                } else {
                    this.child(
                        TextButton::new("pull-request-merge", label)
                            .style(ButtonStyle::Primary)
                            .disabled(disabled)
                            .tooltip(tooltip)
                            .on_click(arm),
                    )
                }
            })
            .when(!is_open, |this| {
                this.child(
                    Label::new(match state {
                        PullRequestState::Merged => "Merged",
                        _ => "Closed",
                    })
                    .size(LabelSize::Small)
                    .color(Color::Muted),
                )
            })
            .children(self.menu.render())
            .into_any_element()
    }
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
                // The forge can merge, but this viewer may not.
                capabilities: PullRequestCapabilities {
                    url: Some("https://github.com/acme/hello/pull/7".to_string()),
                    actions: Vec::new(),
                    merge_methods: vec![PullRequestMergeMethod::Squash],
                    conflicting: false,
                },
            }
        );
        let mut maintainer = detail_fixture();
        maintainer["capabilities"]["actions"] = json!(["merge", "ready", "close", "revert"]);
        maintainer["capabilities"]["mergeMethods"] = json!(["merge", "squash", "rebase"]);
        maintainer["viewerPermissions"]["actions"] = json!(["merge", "ready", "close"]);
        maintainer["mergeability"] = json!("conflicting");
        let capabilities = parse_detail(maintainer)?.capabilities;
        assert_eq!(capabilities.actions, ["merge", "ready", "close"]);
        assert!(capabilities.can("ready") && !capabilities.can("revert"));
        assert_eq!(
            capabilities.merge_methods,
            [
                PullRequestMergeMethod::Merge,
                PullRequestMergeMethod::Squash
            ]
        );
        assert!(capabilities.conflicting);

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

    fn pull_request() -> PullRequestRef {
        PullRequestRef {
            allow_stale: None,
            expected_account_id: None,
            host: Some("github.com".to_string()),
            number: 7,
            project_id: "project-1".to_string(),
            repository: "acme/hello".to_string(),
        }
    }

    #[test]
    fn actions_send_t3s_run_action_input() {
        let input = |action, method| {
            serde_json::to_value(run_action_input(&pull_request(), action, method)).unwrap()
        };
        assert_eq!(
            input(
                PullRequestAction::Merge,
                Some(PullRequestMergeMethod::Squash)
            ),
            json!({
                "action": "merge", "mergeMethod": "squash", "projectId": "project-1",
                "host": "github.com", "repository": "acme/hello", "number": 7
            })
        );
        assert_eq!(
            input(PullRequestAction::Ready, None),
            json!({
                "action": "ready", "projectId": "project-1", "host": "github.com",
                "repository": "acme/hello", "number": 7
            })
        );
        assert_eq!(
            input(PullRequestAction::Close, None)["action"],
            json!("close")
        );
        assert_eq!(
            checkout_reference(
                &pull_request(),
                Some("https://github.com/acme/hello/pull/7")
            ),
            "https://github.com/acme/hello/pull/7"
        );
        assert_eq!(checkout_reference(&pull_request(), None), "7");
    }

    mod live {
        use super::*;
        use crate::thread_git::tests::{Call, RecordingBackend, init_test, shell, shell_thread};
        use gpui::{Focusable as _, TestAppContext};
        use parking_lot::Mutex;
        use pretty_assertions::assert_eq;
        use std::sync::Arc;
        use workspace::MultiWorkspace;

        fn prepared(worktree: Option<&str>) -> Value {
            json!({
                "pullRequest": {
                    "number": 7, "title": "Greet the whole world",
                    "url": "https://github.com/acme/hello/pull/7", "baseBranch": "main",
                    "headBranch": "greet-world", "state": "open"
                },
                "branch": "greet-world",
                "worktreePath": worktree,
                "isOnPullRequestHead": true
            })
        }

        #[gpui::test]
        async fn work_on_pr_prepares_a_checkout_then_a_new_thread(cx: &mut TestAppContext) {
            let backend = RecordingBackend::with_shell(shell(vec![shell_thread(json!({}))]));
            backend.respond("git.resolvePullRequest", Ok(prepared(None)));
            backend.respond(
                "git.preparePullRequestThread",
                Ok(prepared(Some("/src/api-worktrees/greet-world"))),
            );
            init_test(backend.clone(), cx);
            let fs = project::FakeFs::new(cx.executor());
            let project = project::Project::test(fs, [], cx).await;
            let (multi_workspace, cx) = cx.add_window_view(|window, cx| {
                MultiWorkspace::test_new(project.clone(), window, cx)
            });
            let workspace = multi_workspace
                .read_with(cx, |multi_workspace, _| multi_workspace.workspace().clone());
            let opened = Arc::new(Mutex::new(Vec::new()));
            workspace.update_in(cx, |workspace, window, cx| {
                let opened = opened.clone();
                workspace.register_action(move |_, action: &NewThreadInWorktree, _, _| {
                    opened.lock().push(action.clone())
                });
                window.focus(&workspace.active_pane().focus_handle(cx), cx);
            });
            // The shell says where the project is checked out.
            cx.update(|_, cx| ThreadShells::global(cx));
            cx.run_until_parked();

            let mut reference = pull_request();
            reference.repository = "acme/api".to_string();
            let actions = cx.new(|cx| {
                PullRequestActions::new(
                    reference,
                    None,
                    PullRequestCapabilities {
                        url: Some("https://github.com/acme/api/pull/7".to_string()),
                        actions: vec!["merge".to_string()],
                        merge_methods: vec![PullRequestMergeMethod::Squash],
                        conflicting: false,
                    },
                    WeakEntity::new_invalid(),
                    workspace.downgrade(),
                    cx,
                )
            });
            actions.update_in(cx, |actions, window, cx| {
                actions.work_on(CheckoutMode::Worktree, window, cx)
            });
            cx.run_until_parked();
            let checkout_calls = backend
                .calls
                .lock()
                .iter()
                .filter(|call| call.tag.starts_with("git."))
                .cloned()
                .collect::<Vec<_>>();
            assert_eq!(
                checkout_calls,
                [
                    Call {
                        tag: "git.resolvePullRequest".to_string(),
                        payload: json!({
                            "cwd": "/src/api", "reference": "https://github.com/acme/api/pull/7"
                        }),
                        access: Access::Read,
                    },
                    Call {
                        tag: "git.preparePullRequestThread".to_string(),
                        payload: json!({
                            "cwd": "/src/api", "reference": "https://github.com/acme/api/pull/7",
                            "mode": "worktree"
                        }),
                        access: Access::SourceControlWrite,
                    },
                ]
            );
            assert_eq!(
                *opened.lock(),
                [NewThreadInWorktree {
                    project_id: "project-1".to_string(),
                    worktree_path: Some("/src/api-worktrees/greet-world".to_string()),
                    branch: Some("greet-world".to_string()),
                }]
            );

            // Merging runs only once it's confirmed.
            actions.update(cx, |actions, cx| {
                actions.armed = Some(Armed::Merge(PullRequestMergeMethod::Squash));
                cx.notify();
            });
            cx.run_until_parked();
            assert!(backend.calls_to("pullRequests.runAction").is_empty());
            actions.update(cx, |actions, cx| {
                actions.run(
                    PullRequestAction::Merge,
                    Some(PullRequestMergeMethod::Squash),
                    cx,
                )
            });
            cx.run_until_parked();
            assert_eq!(
                backend.calls_to("pullRequests.runAction"),
                [Call {
                    tag: "pullRequests.runAction".to_string(),
                    payload: json!({
                        "action": "merge", "mergeMethod": "squash", "projectId": "project-1",
                        "host": "github.com", "repository": "acme/api", "number": 7
                    }),
                    access: Access::SourceControlWrite,
                }]
            );
            actions.read_with(cx, |actions, _| {
                assert_eq!(actions.state(), PullRequestState::Merged);
                assert_eq!(actions.armed, None);
            });
        }
    }
}

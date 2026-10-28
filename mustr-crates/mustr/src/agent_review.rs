//! Reviewing an agent thread's work in the review view.
//!
//! - **Changes**: the thread's branch against its base, uncommitted changes included. When the
//!   thread's worktree is on this machine, git reads the base texts and the review shows the real
//!   files (so language servers work once Zed trusts the worktree). Otherwise the review is built
//!   from T3's thread diff (`orchestration.getFullThreadDiff`).
//! - **Turn N**: what one turn changed (`orchestration.getTurnDiff` from checkpoint N-1 to N, as
//!   T3's diff panel asks for it). Full texts come from T3's checkpoint commits when their
//!   repository is on this machine, and from the patch otherwise.
//!
//! Comments stay local ([`AgentReviewStore`]). "Send to agent" batches them, each with its file,
//! line, and the code it's on, into one message to the thread: a `message.dispatch` command
//! through `orchestration.dispatchCommand`, queued after any active run, as T3's composer queues
//! follow-ups. It goes through [`PullRequestRpc`], so tests assert the exact typed payload and a
//! dry run never sends it.

use crate::{
    agent_diff::{parse_patch, review_files_from_patch},
    agent_review_store::{AgentReviewStore, ReviewedMarks, SentComment, agent_reviews_dir},
    agent_threads::{AgentThread, TurnCheckpoint, scope_choices, turn_checkpoints},
    nav_panel::NavPanel,
    review_backend::{self, DryRunRpc, PullRequestRpc, write_error},
    t3_connection::{CurrentRpc, T3Connection, T3Session},
    worktree_git::{ChangeKind, FileText, LocalGit},
};
use anyhow::{Context as _, Result, anyhow};
use gpui::{App, AppContext as _, Entity, Task, TaskExt as _, Window};
use review::{
    AgentThreadHeader, DiffSide, FileChange, PendingReview, ReviewBackend, ReviewContents,
    ReviewFileContents, ReviewHeader, ReviewPermissions, ReviewScope, ReviewSubmission,
    ReviewThread, ReviewView, ReviewViewEvent, UnavailableFile,
};
use std::{
    path::{Path, PathBuf},
    sync::Arc,
};
use t3_client::{
    RpcError, RpcMethod,
    t3_api::{
        rpc,
        tagged::{OrchestrationV2Command, OrchestrationV2CommandDispatchMode},
        types::{
            OrchestrationGetFullThreadDiffInput, OrchestrationGetTurnDiffInput,
            OrchestrationV2Actor, OrchestrationV2CommandDispatchModeQueueAfterActive,
            OrchestrationV2CommandDispatchModeQueueAfterActiveType,
            OrchestrationV2CommandMessageDispatch, OrchestrationV2CommandMessageDispatchType,
            OrchestrationV2CreationSource,
        },
    },
};
use time::{OffsetDateTime, format_description::well_known::Rfc3339};
use util::{paths::PathStyle, rel_path::RelPath};
use workspace::{Toast, Workspace, notifications::NotificationId};

/// A review shows at most this many files of a branch; the rest are listed, not loaded.
const MAX_FILES: usize = 300;

/// Set to review agent threads from T3's diffs alone, as when T3 runs on another machine. For
/// checking that path on a machine where every worktree is local.
const IGNORE_LOCAL_WORKTREES_ENV: &str = "MUSTR_IGNORE_LOCAL_WORKTREES";

fn ignore_local_worktrees() -> bool {
    std::env::var_os(IGNORE_LOCAL_WORKTREES_ENV).is_some_and(|value| !value.is_empty())
}

/// What to review: a thread and one of its scopes.
#[derive(Clone, Debug)]
pub struct AgentReviewTarget {
    pub thread: AgentThread,
    pub scope: ReviewScope,
}

/// One scope of a thread, loaded off the main thread.
struct LoadedScope {
    files: Vec<ReviewFileContents>,
    unavailable: Vec<UnavailableFile>,
    /// The checkout whose real files the review shows.
    worktree: Option<PathBuf>,
    /// Where the diff comes from, for the header.
    source: String,
    /// The worktree's checked-out branch, which can differ from the one T3 recorded.
    branch: Option<String>,
    turns: Vec<TurnCheckpoint>,
    problems: Vec<String>,
}

/// T3's typed error, as the reviewer reads it.
fn read_error(tag: &str, error: RpcError) -> anyhow::Error {
    anyhow!(error.error_detail().unwrap_or_else(|| error.to_string())).context(tag.to_string())
}

/// `/home/me/x` as `~/x`, for the header.
fn display_path(path: &Path) -> String {
    match dirs::home_dir().and_then(|home| path.strip_prefix(home).ok().map(Path::to_path_buf)) {
        Some(relative) => format!("~/{}", relative.display()),
        None => path.display().to_string(),
    }
}

/// A path git or T3 reported, if it's one the review can show: relative, inside the checkout.
fn review_path(path: &str) -> Option<Arc<RelPath>> {
    RelPath::new(Path::new(path), PathStyle::Unix)
        .ok()
        .filter(|path| !path.is_empty())
        .map(|path| path.as_ref().into_arc())
}

/// Reads a scope of a thread: its checkpoints (for the scope picker), then the files.
async fn load_scope(
    session: &T3Session,
    rpc: &impl PullRequestRpc,
    thread: &AgentThread,
    scope: ReviewScope,
) -> Result<LoadedScope> {
    let mut problems = Vec::new();
    let turns = match session.thread_snapshot(&thread.id).await {
        Ok(snapshot) => turn_checkpoints(&snapshot),
        Err(error) => {
            log::error!("couldn't load the thread's checkpoints: {error:#}");
            problems.push(format!("its turns ({error:#})"));
            Vec::new()
        }
    };
    let local = match &thread.worktree_path {
        Some(_) if ignore_local_worktrees() => None,
        Some(path) => LocalGit::open(Path::new(path)).await,
        None => None,
    };
    let mut loaded = match (scope, &local) {
        (ReviewScope::Changes, Some(local)) => changes_from_worktree(local).await?,
        (ReviewScope::Changes, None) => {
            let latest = turns
                .last()
                .context("T3 has no finished turns of this thread to diff yet")?;
            let diff = rpc
                .call::<rpc::OrchestrationGetFullThreadDiff>(OrchestrationGetFullThreadDiffInput {
                    ignore_whitespace: Some(false),
                    thread_id: thread.id.clone(),
                    to_turn_count: i64::from(latest.turn),
                })
                .await
                .map_err(|error| read_error(rpc::OrchestrationGetFullThreadDiff::TAG, error))?;
            let patch = parse_patch(&diff.diff);
            let (files, unavailable) = review_files_from_patch(&patch);
            let why = match &thread.worktree_path {
                Some(path) if session.server_is_local() && !ignore_local_worktrees() => {
                    format!("{} isn't on disk anymore", display_path(Path::new(path)))
                }
                _ => "the worktree isn't on this machine".to_string(),
            };
            LoadedScope {
                files,
                unavailable,
                worktree: None,
                source: format!(
                    "T3's thread diff, turns 1–{}{} ({why})",
                    latest.turn,
                    if patch.truncated { ", cut short" } else { "" }
                ),
                branch: None,
                turns: Vec::new(),
                problems: Vec::new(),
            }
        }
        (ReviewScope::Turn(turn), _) => {
            let checkpoint = turns.iter().find(|checkpoint| checkpoint.turn == turn);
            let repository = match (&local, checkpoint.and_then(|c| c.cwd.as_deref())) {
                (Some(local), _) => Some(local.clone()),
                (None, Some(cwd)) if !ignore_local_worktrees() => {
                    LocalGit::open(Path::new(cwd)).await
                }
                (None, _) => None,
            };
            turn_from_t3(rpc, thread, turn, checkpoint, repository.as_ref()).await?
        }
    };
    loaded.turns = turns;
    loaded.problems.extend(problems);
    Ok(loaded)
}

/// The branch against its base, read with git from the worktree on this machine.
async fn changes_from_worktree(local: &LocalGit) -> Result<LoadedScope> {
    let branch = local.current_branch().await;
    let comparison = local
        .comparison(branch.as_deref())
        .await
        .with_context(|| format!("comparing {} with its base", local.root().display()))?;
    let paths = local.changed_paths(&comparison.merge_base).await?;
    let mut files = Vec::new();
    let mut unavailable = Vec::new();
    for (index, changed) in paths.into_iter().enumerate() {
        let (change, old_path) = match &changed.kind {
            ChangeKind::Added => (FileChange::Added, None),
            ChangeKind::Deleted => (FileChange::Deleted, None),
            ChangeKind::Modified => (FileChange::Modified, None),
            ChangeKind::Renamed { old_path } => (FileChange::Renamed, Some(old_path.clone())),
        };
        let mut skip = |reason: String| {
            unavailable.push(UnavailableFile {
                path: changed.path.clone(),
                old_path: old_path.clone(),
                change,
                reason,
            })
        };
        if index >= MAX_FILES {
            skip(format!(
                "Not loaded: the review shows the first {MAX_FILES} files"
            ));
            continue;
        }
        let Some(path) = review_path(&changed.path) else {
            skip("Unsupported path".to_string());
            continue;
        };
        let base = match change {
            FileChange::Added => FileText::Missing,
            _ => {
                let base_path = old_path.as_deref().unwrap_or(&changed.path);
                local.show(&comparison.merge_base, base_path).await?
            }
        };
        let head = match change {
            FileChange::Deleted => FileText::Missing,
            _ => local.read_working_file(&changed.path)?,
        };
        let text = |text: FileText| match text {
            FileText::Text(text) => Ok(Some(text)),
            FileText::Missing => Ok(None),
            FileText::Unavailable(reason) => Err(reason),
        };
        match (text(base), text(head)) {
            (Ok(None), Ok(None)) => skip("No contents".to_string()),
            (Ok(base_text), Ok(head_text)) => files.push(ReviewFileContents {
                path,
                old_path,
                base_text,
                head_text,
                commentable: None,
            }),
            (Err(reason), _) | (_, Err(reason)) => skip(reason),
        }
    }
    let against = match &comparison.base_name {
        Some(base) => format!("vs. {base}"),
        None => "uncommitted changes".to_string(),
    };
    Ok(LoadedScope {
        files,
        unavailable,
        worktree: Some(local.root().to_path_buf()),
        source: format!("{} {against}", display_path(local.root())),
        branch,
        turns: Vec::new(),
        problems: Vec::new(),
    })
}

/// One turn's changes: T3's patch for the file list, and full texts from its checkpoint commits
/// when their repository is on this machine.
async fn turn_from_t3(
    rpc: &impl PullRequestRpc,
    thread: &AgentThread,
    turn: u32,
    checkpoint: Option<&TurnCheckpoint>,
    repository: Option<&LocalGit>,
) -> Result<LoadedScope> {
    let diff = rpc
        .call::<rpc::OrchestrationGetTurnDiff>(OrchestrationGetTurnDiffInput {
            from_turn_count: i64::from(turn.saturating_sub(1)),
            ignore_whitespace: Some(false),
            thread_id: thread.id.clone(),
            to_turn_count: i64::from(turn),
        })
        .await
        .map_err(|error| read_error(rpc::OrchestrationGetTurnDiff::TAG, error))?;
    let patch = parse_patch(&diff.diff);
    let (mut files, unavailable) = review_files_from_patch(&patch);
    let refs = checkpoint
        .and_then(|checkpoint| Some((checkpoint.from_ref.clone()?, checkpoint.to_ref.clone())));
    let mut full_texts = false;
    if let (Some(repository), Some((from_ref, to_ref))) = (repository, refs) {
        full_texts = true;
        for (file, patch_file) in files
            .iter_mut()
            .zip(patch.files.iter().filter(|patch_file| {
                review_path(patch_file.path())
                    .is_some_and(|_| !patch_file.binary && !patch_file.hunks.is_empty())
            }))
        {
            let base = match &patch_file.old_path {
                Some(old_path) => repository.show(&from_ref, old_path).await?,
                None => FileText::Missing,
            };
            let head = match &patch_file.new_path {
                Some(new_path) => repository.show(&to_ref, new_path).await?,
                None => FileText::Missing,
            };
            match (base, head) {
                (FileText::Unavailable(_), _) | (_, FileText::Unavailable(_)) => {
                    full_texts = false;
                }
                (base, head) => {
                    let text = |text: FileText| match text {
                        FileText::Text(text) => Some(text),
                        _ => None,
                    };
                    let (base, head) = (text(base), text(head));
                    // A side the patch has but the commit doesn't means the refs moved; keep
                    // the patch's text.
                    if base.is_some() == file.base_text.is_some()
                        && head.is_some() == file.head_text.is_some()
                    {
                        file.base_text = base;
                        file.head_text = head;
                    } else {
                        full_texts = false;
                    }
                }
            }
        }
    }
    let source = if full_texts {
        format!(
            "Turn {turn}: T3's checkpoints, read from {}",
            display_path(repository.map_or(Path::new(""), LocalGit::root))
        )
    } else {
        format!(
            "Turn {turn}: T3's patch{}",
            if patch.truncated { ", cut short" } else { "" }
        )
    };
    Ok(LoadedScope {
        files,
        unavailable,
        worktree: None,
        source,
        branch: None,
        turns: Vec::new(),
        problems: Vec::new(),
    })
}

/// The message "Send to agent" sends: the reviewer's note, then each comment with its file,
/// line, and the code it's on.
pub fn agent_message(scope: ReviewScope, submission: &ReviewSubmission) -> String {
    let note = submission.body.trim();
    let comments = &submission.comments;
    if comments.is_empty() {
        return note.to_string();
    }
    let mut message = String::new();
    if !note.is_empty() {
        message.push_str(note);
        message.push_str("\n\n");
    }
    let count = match comments.len() {
        1 => "1 comment".to_string(),
        count => format!("{count} comments"),
    };
    message.push_str(&match scope {
        ReviewScope::Changes => format!("Review of your changes ({count}):"),
        ReviewScope::Turn(turn) => format!(
            "Review of what you changed in turn {turn} ({count}; line numbers are as of the end \
             of that turn):"
        ),
    });
    for (index, draft) in comments.iter().enumerate() {
        let last = draft.position.line();
        let lines = match draft.start_line {
            Some(first) if first < last => format!("lines {first}–{last}"),
            _ => format!("line {last}"),
        };
        let side = match draft.position.side() {
            DiffSide::Right => "",
            DiffSide::Left => " of the previous version",
        };
        message.push_str(&format!(
            "\n\n{}. `{}`, {lines}{side}:\n",
            index + 1,
            draft.path
        ));
        if let Some(quote) = submission.quotes.get(&draft.id) {
            let longest_run = quote
                .text
                .split(|character| character != '`')
                .map(str::len)
                .max()
                .unwrap_or(0);
            let fence = "`".repeat(longest_run.max(2) + 1);
            message.push_str(&format!(
                "{fence}{}\n{}\n{fence}\n",
                quote.language.as_deref().unwrap_or(""),
                quote.text
            ));
        }
        message.push_str(draft.body.trim());
    }
    message
}

/// The `message.dispatch` command that sends `text` to the thread. It's queued after any run in
/// progress (and starts one when the thread is idle), so a review never interrupts or steers
/// the agent.
pub fn message_dispatch(
    thread_id: &str,
    text: String,
    command_id: String,
    message_id: String,
) -> OrchestrationV2Command {
    OrchestrationV2Command::MessageDispatch(OrchestrationV2CommandMessageDispatch {
        attachments: Vec::new(),
        command_id,
        context: None,
        created_by: OrchestrationV2Actor::User,
        // T3 knows its own clients; mustr is a desktop client like T3's, which say `web`.
        creation_source: OrchestrationV2CreationSource::Web,
        delegated_completion: None,
        delivery_intent: None,
        dispatch_mode: OrchestrationV2CommandDispatchMode::QueueAfterActive(
            OrchestrationV2CommandDispatchModeQueueAfterActive {
                type_: OrchestrationV2CommandDispatchModeQueueAfterActiveType::QueueAfterActive,
            },
        ),
        manual_continuation_of_run_id: None,
        message_id,
        model_selection: None,
        notification: None,
        restart_continuation_of_run_id: None,
        scheduled_task_id: None,
        sender_thread_id: None,
        source_plan_ref: None,
        text,
        thread_id: thread_id.to_string(),
        title_seed: None,
        type_: OrchestrationV2CommandMessageDispatchType::MessageDispatch,
        usage_limit_continuation_of_run_id: None,
        usage_limit_recovery_request_id: None,
    })
}

type IdSource = Arc<dyn Fn() -> String + Send + Sync>;
type Clock = Arc<dyn Fn() -> OffsetDateTime + Send + Sync>;

/// One scope of an agent thread's review: comments are kept by [`AgentReviewStore`], and
/// submitting sends them to the agent.
pub struct AgentReviewBackend<R> {
    rpc: Arc<R>,
    thread_id: String,
    scope: ReviewScope,
    store: AgentReviewStore,
    new_id: IdSource,
    now: Clock,
}

impl<R: PullRequestRpc> AgentReviewBackend<R> {
    pub fn new(
        rpc: Arc<R>,
        thread_id: String,
        scope: ReviewScope,
        store: AgentReviewStore,
    ) -> Self {
        Self {
            rpc,
            thread_id,
            scope,
            store,
            new_id: Arc::new(|| uuid::Uuid::new_v4().to_string()),
            now: Arc::new(OffsetDateTime::now_utc),
        }
    }

    #[cfg(test)]
    fn with_ids_and_clock(mut self, new_id: IdSource, now: Clock) -> Self {
        self.new_id = new_id;
        self.now = now;
        self
    }
}

fn not_for_agents<T>() -> Task<Result<T>> {
    Task::ready(Err(anyhow!(
        "Comments on an agent's work stay in mustr; there's no forge to write to."
    )))
}

impl<R: PullRequestRpc> ReviewBackend for AgentReviewBackend<R> {
    fn save_pending_review(&self, review: &PendingReview, cx: &mut App) {
        self.store.save_pending(review.clone(), cx);
    }

    fn reply_to_thread(&self, _: String, _: String, _: &mut App) -> Task<Result<()>> {
        not_for_agents()
    }

    fn set_thread_resolution(&self, _: String, _: bool, _: &mut App) -> Task<Result<()>> {
        not_for_agents()
    }

    fn update_comment(&self, _: String, _: String, _: &mut App) -> Task<Result<()>> {
        not_for_agents()
    }

    fn set_files_viewed(&self, _: Vec<(String, bool)>, _: &mut App) -> Task<Result<()>> {
        not_for_agents()
    }

    /// "Send to agent": every added comment, in one message to the thread.
    fn submit_review(&self, submission: ReviewSubmission, cx: &mut App) -> Task<Result<()>> {
        let text = agent_message(self.scope, &submission);
        let command = message_dispatch(&self.thread_id, text, (self.new_id)(), (self.new_id)());
        let call = self.rpc.call::<rpc::OrchestrationDispatchCommand>(command);
        let sent_at = (self.now)()
            .format(&Rfc3339)
            .unwrap_or_else(|_| String::new());
        let sent = submission
            .comments
            .iter()
            .map(|draft| SentComment {
                id: draft.id.clone(),
                path: draft.path.clone(),
                side: draft.position.side(),
                line: draft.position.line(),
                start_line: draft.start_line,
                body: draft.body.clone(),
                sent_at: sent_at.clone(),
            })
            .collect::<Vec<_>>();
        let store = self.store.clone();
        cx.spawn(async move |cx| {
            let result = call.await;
            result.map_err(|error| write_error(rpc::OrchestrationDispatchCommand::TAG, &error))?;
            cx.update(|cx| store.record_sent(sent, cx));
            Ok(())
        })
    }

    fn post_comment(&self, _: String, _: &mut App) -> Task<Result<()>> {
        not_for_agents()
    }

    /// The comments already sent, shown as local threads.
    fn reload_threads(&self, _: &mut App) -> Task<Result<Vec<ReviewThread>>> {
        Task::ready(Ok(self.store.sent_threads()))
    }
}

/// A scope of a thread with its saved review, loaded off the main thread.
struct LoadedReview {
    scope: LoadedScope,
    store: AgentReviewStore,
    pending: PendingReview,
}

async fn load_review(
    session: &T3Session,
    rpc: &CurrentRpc,
    target: &AgentReviewTarget,
) -> Result<LoadedReview> {
    let thread = &target.thread;
    let mut loaded = load_scope(session, rpc, thread, target.scope).await?;
    if loaded.files.is_empty() && loaded.unavailable.is_empty() {
        anyhow::bail!(
            "{} has no changes in {}",
            thread.title,
            target.scope.label()
        );
    }
    let store = AgentReviewStore::new(&agent_reviews_dir(), &thread.id, target.scope);
    let (pending, _) = store.load().unwrap_or_else(|error| {
        log::error!("couldn't load the agent review's drafts: {error:#}");
        loaded.problems.push(format!("{error:#}"));
        (PendingReview::default(), Vec::new())
    });
    Ok(LoadedReview {
        scope: loaded,
        store,
        pending,
    })
}

/// The review's contents for a loaded scope of a thread, and what couldn't be loaded.
fn review_contents(
    loaded: LoadedReview,
    session: &T3Session,
    rpc: CurrentRpc,
    target: &AgentReviewTarget,
) -> (ReviewContents, Vec<String>) {
    let thread = &target.thread;
    let LoadedReview {
        scope: loaded,
        store,
        pending,
    } = loaded;
    let problems = loaded.problems;
    log::info!(
        "Opening {} of agent thread {}: {} files, {} not shown, {} drafts (kept in {})",
        target.scope.label(),
        thread.id,
        loaded.files.len(),
        loaded.unavailable.len(),
        pending.comments.len(),
        store.path().display()
    );
    let backend: Arc<dyn ReviewBackend> = if review_backend::dry_run_requested() {
        log::warn!(
            "{} is set: Send to agent won't reach thread {}",
            review_backend::DRY_RUN_ENV,
            thread.id
        );
        Arc::new(AgentReviewBackend::new(
            Arc::new(DryRunRpc(rpc)),
            thread.id.clone(),
            target.scope,
            store.clone(),
        ))
    } else {
        Arc::new(AgentReviewBackend::new(
            Arc::new(rpc),
            thread.id.clone(),
            target.scope,
            store.clone(),
        ))
    };
    let contents = ReviewContents {
        header: Some(ReviewHeader::AgentThread(AgentThreadHeader {
            thread_id: thread.id.clone(),
            title: thread.title.clone(),
            project: thread.project.clone(),
            branch: loaded.branch.or_else(|| thread.branch.clone()),
            agent: thread.agent.clone(),
            status: thread.status.label().to_string(),
            status_tone: thread.status_tone(),
            scopes: scope_choices(&loaded.turns),
            scope: target.scope,
            source: Some(loaded.source),
        })),
        files: loaded.files,
        worktree: loaded.worktree,
        unavailable: loaded.unavailable,
        threads: store.sent_threads(),
        viewed: None,
        backend: Some(backend),
        permissions: ReviewPermissions {
            line_comments: true,
            blocked: session.agent_send_blocked_reason().map(Into::into),
            ..ReviewPermissions::default()
        },
        pending,
    };
    (contents, problems)
}

/// The open review of this thread and scope, if there is one.
fn open_review(
    workspace: &Workspace,
    target: &AgentReviewTarget,
    cx: &App,
) -> Option<Entity<ReviewView>> {
    workspace.items_of_type::<ReviewView>(cx).find(|review| {
        review
            .read(cx)
            .header()
            .and_then(ReviewHeader::agent_thread)
            .is_some_and(|header| {
                header.thread_id == target.thread.id && header.scope == target.scope
            })
    })
}

/// Opens (or shows, if it's open) a review of an agent thread's work. `replacing` is the review
/// of another scope of the same thread, which the new one takes the place of.
pub fn open_agent_review(
    workspace: &mut Workspace,
    target: AgentReviewTarget,
    replacing: Option<Entity<ReviewView>>,
    window: &mut Window,
    cx: &mut gpui::Context<Workspace>,
) -> Task<Result<Entity<ReviewView>>> {
    if let Some(review) = open_review(workspace, &target, cx) {
        workspace.activate_item(&review, true, true, window, cx);
        return Task::ready(Ok(review));
    }
    let (session, rpc) = {
        let connection = T3Connection::global(cx).read(cx);
        (connection.session(), connection.current_rpc())
    };
    let Some(session) = session else {
        return Task::ready(Err(anyhow!("mustr isn't connected to T3")));
    };
    if let Some(completed_at) = target.thread.latest_run_completed_at.clone() {
        ReviewedMarks::new(&agent_reviews_dir()).mark(target.thread.id.clone(), completed_at, cx);
    }
    cx.spawn_in(window, async move |workspace, cx| {
        let loading = cx.background_spawn({
            let target = target.clone();
            let session = session.clone();
            let rpc = rpc.clone();
            async move { load_review(&session, &rpc, &target).await }
        });
        let (contents, problems) = match loading.await {
            Ok(loaded) => review_contents(loaded, &session, rpc, &target),
            Err(error) => {
                if let Some(replacing) = &replacing {
                    replacing.update(cx, |review, cx| {
                        review.set_notice(
                            Some(
                                format!("Couldn't load {}: {error:#}", target.scope.label()).into(),
                            ),
                            cx,
                        )
                    });
                }
                return Err(error);
            }
        };
        let review = workspace
            .update_in(cx, |workspace, window, cx| {
                ReviewView::open(contents, workspace, window, cx)
            })?
            .await?;
        workspace.update_in(cx, |workspace, window, cx| {
            if let Some(replacing) = replacing {
                let pane = workspace.active_pane().clone();
                pane.update(cx, |pane, cx| {
                    pane.remove_item(replacing.entity_id(), false, false, window, cx)
                });
            }
            cx.subscribe_in(&review, window, {
                let thread = target.thread.clone();
                move |workspace, review, event: &ReviewViewEvent, window, cx| match event {
                    ReviewViewEvent::SelectScope(scope) => {
                        let target = AgentReviewTarget {
                            thread: thread.clone(),
                            scope: *scope,
                        };
                        open_agent_review(workspace, target, Some(review.clone()), window, cx)
                            .detach_and_log_err(cx);
                    }
                }
            })
            .detach();
            workspace.open_panel::<NavPanel>(window, cx);
            if !problems.is_empty() {
                let id = NotificationId::composite::<AgentReviewTarget>(target.thread.id.clone());
                workspace.show_toast(
                    Toast::new(
                        id,
                        format!(
                            "{}: couldn't load {}",
                            target.thread.title,
                            problems.join("; ")
                        ),
                    ),
                    cx,
                );
            }
        })?;
        Ok(review)
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_rpc::RecordingRpc;
    use gpui::{Focusable as _, TestAppContext};
    use pretty_assertions::assert_eq;
    use review::{CommentPosition, DraftComment, QuotedCode, ReviewVerdict};
    use serde_json::json;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use time::macros::datetime;

    fn draft(id: &str, path: &str, position: CommentPosition, body: &str) -> DraftComment {
        DraftComment {
            id: id.to_string(),
            path: path.to_string(),
            old_path: None,
            position,
            start_line: None,
            body: body.to_string(),
            added: true,
        }
    }

    fn submission() -> ReviewSubmission {
        let ranged = DraftComment {
            start_line: Some(1),
            ..draft(
                "d2",
                "src/main.rs",
                CommentPosition::Context {
                    old_line: 3,
                    new_line: 3,
                    side: DiffSide::Right,
                },
                "Rename `main`.",
            )
        };
        ReviewSubmission {
            verdict: ReviewVerdict::Comment,
            body: "  Close, a couple of things.  ".to_string(),
            comments: vec![
                draft(
                    "d1",
                    "src/main.rs",
                    CommentPosition::Added { new_line: 2 },
                    "Use the logger.\n",
                ),
                ranged,
                draft(
                    "d3",
                    "docs/README.md",
                    CommentPosition::Deleted { old_line: 4 },
                    "Why remove this?",
                ),
            ],
            quotes: [
                (
                    "d1".to_string(),
                    QuotedCode {
                        language: Some("rust".to_string()),
                        text: "    println!(\"hello, world\");".to_string(),
                    },
                ),
                (
                    "d2".to_string(),
                    QuotedCode {
                        language: Some("rust".to_string()),
                        text: "fn main() {\n    // ```\n}".to_string(),
                    },
                ),
            ]
            .into_iter()
            .collect(),
        }
    }

    #[test]
    fn the_message_quotes_each_comments_code() {
        assert_eq!(
            agent_message(ReviewScope::Changes, &submission()),
            "Close, a couple of things.

Review of your changes (3 comments):

1. `src/main.rs`, line 2:
```rust
    println!(\"hello, world\");
```
Use the logger.

2. `src/main.rs`, lines 1–3:
````rust
fn main() {
    // ```
}
````
Rename `main`.

3. `docs/README.md`, line 4 of the previous version:
Why remove this?"
        );
        let mut one = submission();
        one.comments.truncate(1);
        one.body = String::new();
        assert!(agent_message(ReviewScope::Turn(3), &one).starts_with(
            "Review of what you changed in turn 3 (1 comment; line numbers are as of the end of \
             that turn):\n\n1. `src/main.rs`, line 2:"
        ));
        let mut note_only = submission();
        note_only.comments.clear();
        assert_eq!(
            agent_message(ReviewScope::Changes, &note_only),
            "Close, a couple of things."
        );
    }

    fn backend(rpc: Arc<RecordingRpc>, dir: &Path) -> AgentReviewBackend<RecordingRpc> {
        let ids = Arc::new(AtomicUsize::new(0));
        AgentReviewBackend::new(
            rpc,
            "thread-1".to_string(),
            ReviewScope::Turn(2),
            AgentReviewStore::new(dir, "thread-1", ReviewScope::Turn(2)),
        )
        .with_ids_and_clock(
            Arc::new(move || format!("id-{}", ids.fetch_add(1, Ordering::SeqCst))),
            Arc::new(|| datetime!(2026-10-27 10:00 UTC)),
        )
    }

    #[gpui::test]
    async fn test_send_to_agent_dispatches_one_queued_message(cx: &mut TestAppContext) {
        let rpc = Arc::new(RecordingRpc::default());
        rpc.respond(Ok(json!({ "sequence": 41 })));
        let dir = tempfile::tempdir().expect("a temporary directory");
        let backend = backend(rpc.clone(), dir.path());
        cx.update(|cx| backend.submit_review(submission(), cx))
            .await
            .expect("the message is sent");

        let command = rpc.only_call::<rpc::OrchestrationDispatchCommand>();
        assert_eq!(
            command,
            message_dispatch(
                "thread-1",
                agent_message(ReviewScope::Turn(2), &submission()),
                "id-0".to_string(),
                "id-1".to_string(),
            )
        );
        // On the wire, exactly T3's `message.dispatch`.
        let wire = rpc.calls.lock()[0].1.clone();
        assert_eq!(
            wire,
            json!({
                "type": "message.dispatch",
                "commandId": "id-0",
                "messageId": "id-1",
                "threadId": "thread-1",
                "text": agent_message(ReviewScope::Turn(2), &submission()),
                "attachments": [],
                "createdBy": "user",
                "creationSource": "web",
                "dispatchMode": { "type": "queue_after_active" }
            })
        );

        // The sent comments are kept, and come back as local threads.
        let threads = cx
            .update(|cx| backend.reload_threads(cx))
            .await
            .expect("threads load");
        assert_eq!(
            threads
                .iter()
                .map(|thread| (
                    thread.id.as_str(),
                    thread.line,
                    thread.side,
                    thread.sent_to_agent
                ))
                .collect::<Vec<_>>(),
            vec![
                ("d1", Some(2), DiffSide::Right, true),
                ("d2", Some(3), DiffSide::Right, true),
                ("d3", Some(4), DiffSide::Left, true),
            ]
        );
        cx.run_until_parked();
        let reopened = AgentReviewStore::new(dir.path(), "thread-1", ReviewScope::Turn(2));
        let (_, sent) = reopened.load().expect("the review is saved");
        assert_eq!(sent.len(), 3);
        assert_eq!(sent[0].sent_at, "2026-10-27T10:00:00Z");
    }

    /// The whole loop as the reviewer goes through it: a comment in the diff, Send to agent,
    /// and the exact command T3 gets.
    #[gpui::test]
    async fn test_send_to_agent_from_the_review(cx: &mut TestAppContext) {
        cx.update(|cx| {
            let settings_store = settings::SettingsStore::test(cx);
            cx.set_global(settings_store);
            theme_settings::init(theme::LoadThemes::JustBase, cx);
            cx.bind_keys(review::key_bindings());
        });
        let fs = project::FakeFs::new(cx.executor());
        let project = project::Project::test(fs, [], cx).await;
        project.read_with(cx, |project, _| {
            project.languages().add(language::rust_lang())
        });
        let (multi_workspace, cx) = cx.add_window_view(|window, cx| {
            workspace::MultiWorkspace::test_new(project.clone(), window, cx)
        });
        let workspace =
            multi_workspace.read_with(cx, |multi_workspace, _| multi_workspace.workspace().clone());

        let rpc = Arc::new(RecordingRpc::default());
        let dir = tempfile::tempdir().expect("a temporary directory");
        let contents = ReviewContents {
            header: Some(ReviewHeader::AgentThread(AgentThreadHeader {
                thread_id: "thread-1".to_string(),
                title: "Run the thing".to_string(),
                project: Some("hello".to_string()),
                branch: Some("t3/run".to_string()),
                agent: Some("Claude · claude-opus-5-5".to_string()),
                status: "completed".to_string(),
                status_tone: review::AgentStatusTone::Settled,
                scopes: Vec::new(),
                scope: ReviewScope::Turn(2),
                source: None,
            })),
            files: vec![ReviewFileContents {
                path: review_path("src/main.rs").expect("a valid path"),
                old_path: None,
                base_text: Some("fn main() {\n}\n".to_string()),
                head_text: Some("fn main() {\n    run();\n}\n".to_string()),
                commentable: None,
            }],
            backend: Some(Arc::new(backend(rpc.clone(), dir.path()))),
            permissions: ReviewPermissions {
                line_comments: true,
                ..ReviewPermissions::default()
            },
            ..ReviewContents::default()
        };
        let review = workspace
            .update_in(cx, |workspace, window, cx| {
                ReviewView::open(contents, workspace, window, cx)
            })
            .await
            .expect("the review opens");
        cx.run_until_parked();

        let editor = review.read_with(cx, |review, cx| {
            review.editor().read(cx).rhs_editor().clone()
        });
        editor.update_in(cx, |editor, window, cx| {
            let text = editor.buffer().read(cx).snapshot(cx).text();
            let row = text
                .lines()
                .position(|line| line == "    run();")
                .expect("the added line is in the diff") as u32;
            editor.change_selections(
                editor::SelectionEffects::no_scroll(),
                window,
                cx,
                |selections| {
                    selections
                        .select_ranges([language::Point::new(row, 0)..language::Point::new(row, 0)])
                },
            );
            window.focus(&editor.focus_handle(cx), cx);
        });
        cx.run_until_parked();
        cx.simulate_keystrokes("c");
        cx.simulate_input("Handle the error.");
        cx.simulate_keystrokes("secondary-enter");
        assert!(
            rpc.calls.lock().is_empty(),
            "comments stay local until sent"
        );

        rpc.respond(Ok(json!({ "sequence": 7 })));
        cx.dispatch_action(review::SubmitReview);
        cx.simulate_input("Nearly there.");
        cx.simulate_keystrokes("secondary-enter");
        cx.run_until_parked();

        assert_eq!(
            rpc.only_call::<rpc::OrchestrationDispatchCommand>(),
            message_dispatch(
                "thread-1",
                "Nearly there.

Review of what you changed in turn 2 (1 comment; line numbers are as of the end of that turn):

1. `src/main.rs`, line 2:
```rust
    run();
```
Handle the error."
                    .to_string(),
                "id-0".to_string(),
                "id-1".to_string(),
            )
        );
        let (_, sent) = AgentReviewStore::new(dir.path(), "thread-1", ReviewScope::Turn(2))
            .load()
            .expect("the review is saved");
        assert_eq!(sent.len(), 1);
        review.read_with(cx, |review, cx| {
            assert_eq!(review.added_draft_count(cx), 0, "the comment was sent");
            assert!(
                review.thread_block(&sent[0].id).is_some(),
                "and shows as a sent local thread"
            );
        });
    }

    #[gpui::test]
    async fn test_failed_sends_and_dry_runs_keep_the_comments(cx: &mut TestAppContext) {
        let rpc = Arc::new(RecordingRpc::default());
        rpc.respond(Err(RpcError::Failed {
            tag: rpc::OrchestrationDispatchCommand::TAG.to_string(),
            error: json!({
                "_tag": "EnvironmentAuthorizationError",
                "message": "Missing scope",
                "requiredScope": "orchestration:operate"
            }),
        }));
        let dir = tempfile::tempdir().expect("a temporary directory");
        let backend = backend(rpc.clone(), dir.path());
        let error = cx
            .update(|cx| backend.submit_review(submission(), cx))
            .await
            .expect_err("T3 refused");
        assert_eq!(
            format!("{error:#}"),
            "T3 refused: mustr's pairing doesn't include orchestration:operate. Pair again from \
             the home view."
        );
        assert!(
            cx.update(|cx| backend.reload_threads(cx))
                .await
                .unwrap()
                .is_empty(),
            "nothing is marked sent"
        );

        // A dry run sends nothing at all.
        let recording = Arc::new(DryRunRpc(RecordingRpc::default()));
        let dry = AgentReviewBackend::new(
            recording.clone(),
            "thread-1".to_string(),
            ReviewScope::Changes,
            AgentReviewStore::new(dir.path(), "thread-1", ReviewScope::Changes),
        );
        let error = cx
            .update(|cx| dry.submit_review(submission(), cx))
            .await
            .expect_err("dry runs refuse writes");
        assert!(format!("{error:#}").contains("MUSTR_DRY_RUN_WRITES"));
        assert!(recording.0.calls.lock().is_empty());
    }

    #[gpui::test]
    async fn test_turn_diffs_ask_t3_for_one_turn(cx: &mut TestAppContext) {
        let rpc = RecordingRpc::default();
        rpc.respond(Ok(json!({
            "threadId": "thread-1", "fromTurnCount": 2, "toTurnCount": 3,
            "diff": "diff --git a/src/main.rs b/src/main.rs\n--- a/src/main.rs\n+++ b/src/main.rs\n@@ -1,2 +1,2 @@\n fn main() {\n-}\n+    run();\n"
        })));
        let thread = crate::agent_threads::agent_threads(&json!({
            "threads": [{ "id": "thread-1", "projectId": "p", "title": "Run", "status": "idle",
                          "updatedAt": "2026-10-27T00:00:00.000Z" }]
        }))
        .remove(0);
        let loaded = cx
            .background_executor
            .spawn(async move {
                let loaded = turn_from_t3(&rpc, &thread, 3, None, None).await;
                (loaded, rpc)
            })
            .await;
        let (loaded, rpc) = loaded;
        let loaded = loaded.expect("the turn loads");
        assert_eq!(
            rpc.only_call::<rpc::OrchestrationGetTurnDiff>(),
            OrchestrationGetTurnDiffInput {
                from_turn_count: 2,
                ignore_whitespace: Some(false),
                thread_id: "thread-1".to_string(),
                to_turn_count: 3,
            }
        );
        assert_eq!(loaded.source, "Turn 3: T3's patch");
        assert_eq!(loaded.files.len(), 1);
        assert_eq!(
            loaded.files[0].head_text.as_deref(),
            Some("fn main() {\n    run();\n")
        );
        assert!(
            loaded.worktree.is_none(),
            "a past turn isn't the files on disk"
        );
    }
}

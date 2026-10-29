//! mustr's review view: a read-only multibuffer of diffs whose base and head texts are held in
//! memory (for example, fetched from a T3 Code server) rather than read from a local checkout,
//! with the pull request's header, its review threads as cards in the diff, and a file tree.
//!
//! With a [`ReviewBackend`], the review also writes: replies, resolving threads, editing the
//! viewer's comments, viewed marks, and a pending review of drafted line comments that's
//! submitted in one go. Drafts are handed to the backend to persist on every change.
//!
//! The same view reviews an agent thread's work ([`AgentThreadHeader`]): its comments stay
//! local, and submitting sends them to the agent. When the thread's worktree is on this machine,
//! the review shows its real files ([`ReviewContents::worktree`]), so language servers work once
//! the worktree is trusted.

mod agent;
mod backend;
mod comment_html;
mod composer;
mod drafts;
mod file_tree;
mod file_tree_panel;
mod header;
mod submit;
mod threads;
mod toolbar;

pub use agent::{
    AgentStatusTone, AgentThreadHeader, OpenThreadChat, ReviewScope, ReviewViewEvent, ScopeChoice,
};
pub use backend::{ReviewBackend, ReviewPermissions};
pub use comment_html::comment_markdown;
pub use composer::{AddComment, CancelComment, ConfirmComment, DraftCard, SubmitReview};
pub use drafts::{
    CommentPosition, CommentableLines, DraftComment, HunkLine, PendingReview, QuotedCode,
    ReviewSubmission, ReviewVerdict, can_submit,
};
pub use file_tree::{FileChange, FileTreeFile, FileTreeModel, FileTreeRow, FileViewedState};
pub use file_tree_panel::{FileTreePanel, ToggleFileTree};
pub use header::{ChecksState, ChecksSummary, PullRequestHeader, PullRequestState, ReviewHeader};
pub use submit::SubmitMode;
pub use threads::{
    DiffSide, EditorSide, OutdatedThreads, ReviewThread, ThreadCard, ThreadCardEvent, ThreadComment,
};

/// The review view's key bindings, for the app to bind after its default keymap (so users can
/// rebind them):
/// - `c` in the diff comments on the selected lines. Not while a comment is being written,
///   since the composers live inside the diff's editor.
/// - `secondary-enter` adds, sends, or saves the comment being written; `escape` stops.
pub fn key_bindings() -> Vec<gpui::KeyBinding> {
    use gpui::KeyBinding;
    vec![
        KeyBinding::new(
            "c",
            AddComment,
            Some("ReviewView && !editing_comment > Editor && mode == full"),
        ),
        KeyBinding::new(
            "secondary-enter",
            ConfirmComment,
            Some("ReviewComposer > Editor"),
        ),
        KeyBinding::new("escape", CancelComment, Some("ReviewComposer > Editor")),
    ]
}

use anyhow::{Context as _, Result};
use buffer_diff::BufferDiff;
use collections::{HashMap, HashSet};
use composer::DraftCardEvent;
use editor::{
    Editor, EditorEvent, EditorSettings, HiddenDiffHunkRenderer, MultiBuffer, SelectionEffects,
    SplittableEditor,
    display_map::{BlockPlacement, CustomBlockId, DisplayRow, ToDisplayPoint as _},
    multibuffer_context_lines,
    scroll::Autoscroll,
};
use gpui::{
    AnyElement, AnyView, App, AppContext as _, AsyncWindowContext, Context, Entity, EventEmitter,
    FocusHandle, Focusable, Font, IntoElement, KeyContext, ParentElement as _, Render,
    SharedString, Styled as _, Subscription, Task, WeakEntity, Window, px,
};
use language::{
    Buffer, BufferEvent, Capability, DiskState, File, HighlightedText, LanguageRegistry,
    LineEnding, OffsetRangeExt as _, Point, ReplicaId, Rope, TextBuffer,
};
use multi_buffer::PathKey;
use multi_buffer::{MultiBufferRow, RowInfo};
use mustr_ui::{tokens, tokens::scaled};
use project::{
    Project, ProjectPath, WorktreeId,
    trusted_worktrees::{TrustedWorktrees, TrustedWorktreesEvent},
    worktree_store::WorktreeStore,
};
use settings::Settings as _;
use std::{
    any::{Any, TypeId},
    mem,
    ops::Range,
    path::PathBuf,
    sync::Arc,
};
use std::{collections::BTreeMap, time::Duration};
use submit::SubmitForm;
use threads::{ThreadWrites, card_block, line_anchor, thread_anchor, thread_head_row};
use ui::{
    Color, Icon, IconButton, IconName, IconSize, Label, LabelCommon as _, LabelSize, prelude::*,
};
use util::{ResultExt as _, paths::PathStyle, rel_path::RelPath};
use workspace::{
    Item, ItemHandle as _, ItemNavHistory, ToolbarItemLocation, Workspace, item::ItemEvent,
    searchable::SearchableItemHandle,
};

/// One changed file in a review, with both sides of the diff held in memory.
pub struct ReviewFileContents {
    pub path: Arc<RelPath>,
    /// The path before a rename.
    pub old_path: Option<String>,
    /// `None` when the file was added.
    pub base_text: Option<String>,
    /// `None` when the file was deleted.
    pub head_text: Option<String>,
    /// The lines the forge accepts line comments on, or `None` when any line may be commented
    /// on (a local review).
    pub commentable: Option<CommentableLines>,
}

impl ReviewFileContents {
    pub fn change(&self) -> FileChange {
        match (&self.base_text, &self.head_text) {
            (None, _) => FileChange::Added,
            (_, None) => FileChange::Deleted,
            _ if self.old_path.is_some() => FileChange::Renamed,
            _ => FileChange::Modified,
        }
    }
}

/// A changed file the review can't show, such as a binary file or one too large for the forge to
/// expand. It's still listed in the file tree.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct UnavailableFile {
    pub path: String,
    pub old_path: Option<String>,
    pub change: FileChange,
    pub reason: String,
}

/// Everything a review shows.
#[derive(Default)]
pub struct ReviewContents {
    /// The pull request or agent thread under review, when its details could be loaded.
    pub header: Option<ReviewHeader>,
    pub files: Vec<ReviewFileContents>,
    /// The local checkout the head side's files are in. The review then shows the real files
    /// (opened in the workspace's project, so language servers attach once Zed trusts the
    /// worktree) instead of the head texts, and keeps each diff current as the files change. The
    /// base texts still come from `files`, and each file's head text is used if its real file
    /// can't be opened.
    pub worktree: Option<PathBuf>,
    pub unavailable: Vec<UnavailableFile>,
    pub threads: Vec<ReviewThread>,
    /// The viewer's viewed marks by path, or `None` when the forge keeps none. Files missing from
    /// the map are unviewed.
    pub viewed: Option<std::collections::HashMap<String, FileViewedState>>,
    /// Writes to the forge and saves drafts. `None` for a read-only review.
    pub backend: Option<Arc<dyn ReviewBackend>>,
    pub permissions: ReviewPermissions,
    /// The pending review saved last time.
    pub pending: PendingReview,
}

/// How far above a file's first line to look for its header and the blocks listed with it.
const BLOCK_ROWS_ABOVE_FILE: u32 = 64;

/// Review files aren't backed by a worktree. An id that never matches a real worktree makes
/// project-local settings lookups fall back to the global settings.
const VIRTUAL_WORKTREE_ID: usize = usize::MAX;

/// A `language::File` for a buffer that only exists in memory. It gives the buffer a path, which
/// drives language detection and the excerpt header.
///
/// Modeled on `GitBlob` in `crates/git_ui/src/commit_view.rs`.
struct ReviewFile {
    path: Arc<RelPath>,
    is_deleted: bool,
}

impl File for ReviewFile {
    fn as_local(&self) -> Option<&dyn language::LocalFile> {
        None
    }

    fn disk_state(&self) -> DiskState {
        DiskState::Historic {
            was_deleted: self.is_deleted,
        }
    }

    fn path(&self) -> &Arc<RelPath> {
        &self.path
    }

    fn full_path(&self, _: &App) -> PathBuf {
        self.path.as_std_path().to_path_buf()
    }

    // Review paths come from git, which always uses forward slashes.
    fn path_style(&self, _: &App) -> PathStyle {
        PathStyle::Unix
    }

    fn file_name<'a>(&'a self, _: &'a App) -> &'a str {
        self.path.file_name().unwrap_or_default()
    }

    fn worktree_id(&self, _: &App) -> WorktreeId {
        WorktreeId::from_usize(VIRTUAL_WORKTREE_ID)
    }

    fn to_proto(&self, _: &App) -> language::proto::File {
        language::proto::File {
            worktree_id: WorktreeId::from_usize(VIRTUAL_WORKTREE_ID).to_proto(),
            entry_id: None,
            path: self.path.as_unix_str().to_owned(),
            mtime: None,
            is_deleted: self.is_deleted,
            is_historic: true,
        }
    }

    fn is_private(&self) -> bool {
        false
    }

    // There is no file on disk to open.
    fn can_open(&self) -> bool {
        false
    }
}

struct ReviewEntry {
    path: Arc<RelPath>,
    old_path: Option<String>,
    change: FileChange,
    buffer: Entity<Buffer>,
    diff: Entity<BufferDiff>,
    commentable: Option<CommentableLines>,
    /// Whether `buffer` is the real file in the worktree, which can change under the review.
    on_disk: bool,
}

/// The parts of [`ReviewContents`] the view keeps once the buffers are built.
struct LoadedReview {
    header: Option<ReviewHeader>,
    worktree_store: Option<Entity<WorktreeStore>>,
    entries: Vec<ReviewEntry>,
    unavailable: Vec<UnavailableFile>,
    threads: Vec<ReviewThread>,
    viewed: Option<std::collections::HashMap<String, FileViewedState>>,
    backend: Option<Arc<dyn ReviewBackend>>,
    permissions: ReviewPermissions,
    pending: PendingReview,
}

/// A file the review shows. Its index in [`ReviewView::files`] is its index in the file tree.
struct ShownFile {
    path_key: PathKey,
    path: String,
    old_path: Option<String>,
    buffer: Entity<Buffer>,
    diff: Entity<BufferDiff>,
    commentable: Option<CommentableLines>,
    on_disk: bool,
}

impl ShownFile {
    fn has_path(&self, path: &str) -> bool {
        self.path == path || self.old_path.as_deref() == Some(path)
    }
}

/// The editor blocks holding thread cards, so they can be replaced when the diff switches
/// between split and unified.
#[derive(Default)]
struct ThreadBlocks {
    rhs: HashSet<CustomBlockId>,
    lhs: Option<(WeakEntity<Editor>, HashSet<CustomBlockId>)>,
    /// Whether the blocks were placed for a split diff.
    synced_split: Option<bool>,
    /// Thread id to the editor and block of its card.
    placed: HashMap<String, (EditorSide, CustomBlockId)>,
    /// Draft id to the editor and block of its card.
    drafts: HashMap<String, (EditorSide, CustomBlockId)>,
    /// File index to the list of threads the diff can't place, shown above the file.
    outdated: HashMap<usize, (Entity<OutdatedThreads>, CustomBlockId)>,
}

/// Shows every file of a review in one diff multibuffer, in file tree order.
///
/// Modeled on `MultiDiffView` (`crates/git_ui/src/multi_diff_view.rs`) and `CommitView`
/// (`crates/git_ui/src/commit_view.rs`).
pub struct ReviewView {
    editor: Entity<SplittableEditor>,
    header: Option<ReviewHeader>,
    /// The app's actions for what's under review, at the start of the header's actions: for
    /// example, merging the pull request, or committing the agent's work.
    header_actions: Option<AnyView>,
    /// The app's details of what's under review, below the header's details line: for example,
    /// a pull request's reviewers and labels.
    header_details: Option<AnyView>,
    /// The project's worktrees, when the review shows real files: Zed's Restricted Mode applies
    /// to them.
    worktree_store: Option<Entity<WorktreeStore>>,
    /// Diffs being recomputed after a real file changed, by file index.
    diff_updates: HashMap<usize, Task<()>>,
    files: Vec<ShownFile>,
    file_tree: FileTreeModel,
    /// Cards of threads on shown files, with the file's index.
    thread_cards: Vec<(usize, Entity<ThreadCard>)>,
    /// Cards of threads on files the review doesn't show, listed in the header.
    unshown_thread_cards: Vec<Entity<ThreadCard>>,
    unshown_threads_expanded: bool,
    /// Cards of the pending review's line comments, with the file's index.
    draft_cards: Vec<(usize, Entity<DraftCard>)>,
    /// Drafts the diff no longer has a place for (the pull request changed since they were
    /// written). They're kept, but not submitted.
    stranded_drafts: Vec<DraftComment>,
    writes: Option<ThreadWrites>,
    permissions: Arc<ReviewPermissions>,
    language_registry: Arc<LanguageRegistry>,
    submit: SubmitForm,
    /// Why the last comment couldn't be started, or what just happened.
    notice: Option<SharedString>,
    viewed_error: Option<SharedString>,
    viewed_in_flight: HashMap<usize, Task<()>>,
    blocks: ThreadBlocks,
    toolbar: Entity<toolbar::ReviewToolbar>,
    save_task: Option<Task<()>>,
    reload_task: Option<Task<()>>,
    _subscriptions: Vec<Subscription>,
}

/// Where a new comment goes.
struct CommentTarget {
    file: usize,
    position: CommentPosition,
    start_line: Option<u32>,
}

/// Saving waits this long after the last keystroke, so typing doesn't write on every key.
const SAVE_DELAY: Duration = Duration::from_millis(300);

/// A real file's diff is recomputed this long after it last changed, as in Zed's
/// `FileDiffView` (`crates/git_ui_core/src/file_diff_view.rs`).
const RECALCULATE_DIFF_DEBOUNCE: Duration = Duration::from_millis(250);

impl ReviewView {
    pub fn open(
        contents: ReviewContents,
        workspace: &Workspace,
        window: &mut Window,
        cx: &mut App,
    ) -> Task<Result<Entity<Self>>> {
        let project = workspace.project().clone();
        let language_registry = project.read(cx).languages().clone();
        let workspace = workspace.weak_handle();

        window.spawn(cx, async move |cx| {
            let ReviewContents {
                header,
                files,
                worktree,
                unavailable,
                threads,
                viewed,
                backend,
                permissions,
                pending,
            } = contents;
            // A visible worktree, so Zed's Restricted Mode applies to it: Zed skips the trust
            // check for invisible ones, which would start language servers in an untrusted
            // checkout.
            let worktree_id = match &worktree {
                Some(root) => {
                    let (worktree, _) = project
                        .update(cx, |project, cx| {
                            project.find_or_create_worktree(root, true, cx)
                        })
                        .await
                        .with_context(|| format!("opening {}", root.display()))?;
                    Some(cx.update(|_, cx| worktree.read(cx).id())?)
                }
                None => None,
            };
            let mut entries = Vec::with_capacity(files.len());
            for file in files {
                entries
                    .push(load_entry(file, worktree_id, &project, &language_registry, cx).await?);
            }
            let worktree_store = worktree_id
                .map(|_| cx.update(|_, cx| project.read(cx).worktree_store()))
                .transpose()?;
            let loaded = LoadedReview {
                header,
                worktree_store,
                entries,
                unavailable,
                threads,
                viewed,
                backend,
                permissions,
                pending,
            };

            workspace.update_in(cx, |workspace, window, cx| {
                let workspace_handle = cx.entity();
                let review_view =
                    cx.new(|cx| Self::new(loaded, project, workspace_handle, window, cx));
                workspace.active_pane().update(cx, |pane, cx| {
                    pane.add_item(Box::new(review_view.clone()), true, true, None, window, cx);
                });
                review_view
            })
        })
    }

    fn new(
        loaded: LoadedReview,
        project: Entity<Project>,
        workspace: Entity<Workspace>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        let LoadedReview {
            header,
            worktree_store,
            entries,
            unavailable,
            threads,
            viewed,
            backend,
            permissions,
            pending,
        } = loaded;
        let language_registry = project.read(cx).languages().clone();
        let permissions = Arc::new(permissions);
        let writes = backend.map(|backend| ThreadWrites {
            backend,
            permissions: permissions.clone(),
        });

        let multibuffer = cx.new(|cx| {
            let mut multibuffer = MultiBuffer::new(Capability::ReadOnly);
            multibuffer.set_all_diff_hunks_expanded(cx);
            multibuffer
        });
        let editor = cx.new(|cx| {
            let editor = SplittableEditor::new(
                EditorSettings::get_global(cx).diff_view_style,
                multibuffer,
                project,
                workspace,
                window,
                cx,
            );
            editor.set_diff_hunk_renderer(Some(Arc::new(HiddenDiffHunkRenderer)), cx);
            editor.rhs_editor().update(cx, |editor, cx| {
                editor.set_show_bookmarks(false, cx);
                editor.set_show_breakpoints(false, cx);
            });
            editor
        });

        // Shown files come first in the tree's file list, so a shown file's tree index is also
        // its index in `entries`.
        let viewed_state = |path: &str| {
            viewed
                .as_ref()
                .and_then(|viewed| viewed.get(path).copied())
                .unwrap_or_default()
        };
        let mut tree_files = entries
            .iter()
            .map(|entry| {
                let path = entry.path.as_unix_str().to_string();
                FileTreeFile {
                    viewed: viewed_state(&path),
                    path,
                    old_path: entry.old_path.clone(),
                    change: entry.change,
                    line_counts: Some(line_counts(&entry.buffer, &entry.diff, cx)),
                    unavailable: None,
                }
            })
            .collect::<Vec<_>>();
        tree_files.extend(unavailable.into_iter().map(|file| FileTreeFile {
            viewed: viewed_state(&file.path),
            path: file.path,
            old_path: file.old_path,
            change: file.change,
            line_counts: None,
            unavailable: Some(file.reason),
        }));
        let file_tree = FileTreeModel::new(tree_files, viewed.is_some());

        let mut threads_by_file = vec![Vec::new(); entries.len()];
        let mut unshown_threads = Vec::new();
        for thread in threads {
            let thread = Arc::new(thread);
            let file = entries.iter().position(|entry| {
                entry.path.as_unix_str() == thread.path
                    || entry.old_path.as_deref() == Some(thread.path.as_str())
            });
            match file {
                Some(file) => threads_by_file[file].push(thread),
                None => unshown_threads.push(thread),
            }
        }

        let mut positions = vec![0; entries.len()];
        for (position, file) in file_tree
            .file_order()
            .into_iter()
            .filter(|file| *file < entries.len())
            .enumerate()
        {
            positions[file] = position;
        }
        let files = editor.update(cx, |editor, cx| {
            let context_lines = multibuffer_context_lines(cx);
            entries
                .into_iter()
                .zip(&threads_by_file)
                .zip(positions)
                .map(|((entry, threads), position)| {
                    let thread_rows = threads
                        .iter()
                        .filter_map(|thread| {
                            thread_head_row(thread, &entry.buffer, &entry.diff, cx)
                        })
                        .collect::<Vec<_>>();
                    let ranges = excerpt_ranges(&entry, &thread_rows, cx);
                    let path = entry.path.as_unix_str().to_string();
                    let path_key = PathKey::with_sort_prefix(position as u64, entry.path);
                    // Going through the `SplittableEditor` keeps the left-hand side in sync in
                    // split mode.
                    editor.update_excerpts_for_path(
                        path_key.clone(),
                        entry.buffer.clone(),
                        ranges,
                        context_lines,
                        entry.diff.clone(),
                        cx,
                    );
                    ShownFile {
                        path_key,
                        path,
                        old_path: entry.old_path,
                        buffer: entry.buffer,
                        diff: entry.diff,
                        commentable: entry.commentable,
                        on_disk: entry.on_disk,
                    }
                })
                .collect::<Vec<_>>()
        });

        let to_agent = matches!(header, Some(ReviewHeader::AgentThread(_)));
        let submit = SubmitForm::new(&pending, &permissions, to_agent, window, cx);
        let mut subscriptions = vec![
            cx.subscribe(&editor, |this, _, event: &EditorEvent, cx| {
                if event == &(EditorEvent::SelectionsChanged { local: true }) {
                    if this.notice.take().is_some() {
                        cx.notify();
                    }
                    cx.emit(event.clone())
                }
            }),
            // Split mode puts cards for the base side in the left-hand editor, which only exists
            // while the diff is split.
            cx.observe(&editor, |this, editor, cx| {
                if this.blocks.synced_split != Some(editor.read(cx).is_split()) {
                    this.sync_thread_blocks(cx);
                }
            }),
            submit
                .summary
                .subscribe(cx, |this: &mut Self, cx| this.schedule_save(cx)),
        ];
        // The last keystrokes are saved even when the review closes before the save delay.
        subscriptions.push(cx.on_release(|this, cx| this.save_now(cx)));
        // Real files change under the review (the agent keeps working, or the reviewer edits
        // one in its own tab), so their diffs follow.
        for (index, file) in files.iter().enumerate() {
            if file.on_disk {
                subscriptions.push(cx.subscribe(
                    &file.buffer,
                    move |this, _, event: &BufferEvent, cx| {
                        if matches!(event, BufferEvent::Edited { .. } | BufferEvent::Reloaded) {
                            this.schedule_diff_update(index, cx);
                        }
                    },
                ));
            }
        }
        if worktree_store.is_some()
            && let Some(trusted_worktrees) = TrustedWorktrees::try_get_global(cx)
        {
            subscriptions.push(
                cx.subscribe(&trusted_worktrees, |_, _, _: &TrustedWorktreesEvent, cx| {
                    cx.notify()
                }),
            );
        }

        let toolbar = cx.new(|cx| toolbar::ReviewToolbar::new(&editor, cx));
        let mut this = Self {
            editor,
            header,
            worktree_store,
            diff_updates: HashMap::default(),
            files,
            file_tree,
            thread_cards: Vec::new(),
            unshown_thread_cards: Vec::new(),
            unshown_threads_expanded: false,
            draft_cards: Vec::new(),
            stranded_drafts: Vec::new(),
            writes,
            permissions,
            language_registry,
            submit,
            notice: None,
            viewed_error: None,
            viewed_in_flight: HashMap::default(),
            blocks: ThreadBlocks::default(),
            toolbar,
            header_actions: None,
            header_details: None,
            save_task: None,
            reload_task: None,
            _subscriptions: subscriptions,
        };

        this.thread_cards = threads_by_file
            .into_iter()
            .enumerate()
            .flat_map(|(file, threads)| threads.into_iter().map(move |thread| (file, thread)))
            .map(|(file, thread)| (file, this.new_thread_card(thread, None, window, cx)))
            .collect();
        this.unshown_thread_cards = unshown_threads
            .into_iter()
            .map(|thread| {
                let location = Some(thread_location(&thread));
                this.new_thread_card(thread, location, window, cx)
            })
            .collect();
        for card in this
            .thread_cards
            .iter()
            .map(|(_, card)| card)
            .chain(&this.unshown_thread_cards)
        {
            card.update(cx, |card, cx| {
                let thread_id = card.thread().id.clone();
                let edit = pending.edits.iter().find(|(comment_id, _)| {
                    card.thread()
                        .comments
                        .iter()
                        .any(|comment| &comment.id == *comment_id)
                });
                card.restore(
                    pending.replies.get(&thread_id).map(String::as_str),
                    edit.map(|(id, text)| (id.as_str(), text.as_str())),
                    window,
                    cx,
                );
            });
        }
        for draft in pending.comments {
            let file = this.files.iter().position(|file| {
                file.has_path(&draft.path)
                    && file.commentable.as_ref().is_none_or(|commentable| {
                        commentable.position(draft.position.side(), draft.position.line())
                            == Some(draft.position)
                    })
            });
            match file {
                Some(file) => {
                    let card = this.new_draft_card(draft, window, cx);
                    this.draft_cards.push((file, card));
                }
                None => this.stranded_drafts.push(draft),
            }
        }
        this.sync_thread_blocks(cx);
        this
    }

    fn new_thread_card(
        &self,
        thread: Arc<ReviewThread>,
        location: Option<SharedString>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Entity<ThreadCard> {
        let language_registry = self.language_registry.clone();
        let writes = self.writes.clone();
        let card =
            cx.new(|cx| ThreadCard::new(thread, location, Some(language_registry), writes, cx));
        cx.subscribe_in(&card, window, |this, _, event, window, cx| match event {
            ThreadCardEvent::DraftChanged => this.schedule_save(cx),
            ThreadCardEvent::Posted => {
                this.save_now(cx);
                this.reload_threads(window, cx);
            }
        })
        .detach();
        card
    }

    fn new_draft_card(
        &self,
        draft: DraftComment,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Entity<DraftCard> {
        let language_registry = self.language_registry.clone();
        let card = cx.new(|cx| DraftCard::new(draft, Some(language_registry), window, cx));
        cx.subscribe_in(&card, window, |this, card, event, window, cx| match event {
            DraftCardEvent::Changed => {
                // Adding the comment (or cancelling an edit) closes its composer; the diff gets
                // the focus back.
                if !card.read(cx).is_editing() {
                    this.focus_diff(window, cx);
                    this.save_now(cx);
                } else {
                    this.schedule_save(cx);
                }
                cx.notify();
            }
            DraftCardEvent::Removed => {
                let id = card.read(cx).draft(cx).id;
                this.remove_draft(&id, cx);
                this.focus_diff(window, cx);
                this.save_now(cx);
            }
        })
        .detach();
        card
    }

    pub fn header(&self) -> Option<&ReviewHeader> {
        self.header.as_ref()
    }

    /// Puts the app's own actions in the header, before the review's (`None` removes them).
    pub fn set_header_actions(&mut self, actions: Option<AnyView>, cx: &mut Context<Self>) {
        self.header_actions = actions;
        cx.notify();
    }

    /// Puts the app's own details under the header's details line (`None` removes them).
    pub fn set_header_details(&mut self, details: Option<AnyView>, cx: &mut Context<Self>) {
        self.header_details = details;
        cx.notify();
    }

    /// Changes the pull request's header, such as after its title was edited; the tab follows.
    pub fn update_pull_request_header(
        &mut self,
        update: impl FnOnce(&mut PullRequestHeader),
        cx: &mut Context<Self>,
    ) {
        if let Some(ReviewHeader::PullRequest(header)) = &mut self.header {
            update(header);
            cx.emit(EditorEvent::TitleChanged);
            cx.notify();
        }
    }

    /// Whether this is a review of an agent's work, whose comments go to the agent rather than
    /// to a forge.
    pub fn sends_to_agent(&self) -> bool {
        matches!(self.header, Some(ReviewHeader::AgentThread(_)))
    }

    /// Whether the review shows real files in a worktree that Zed's Restricted Mode keeps
    /// language servers out of.
    pub fn is_worktree_restricted(&self, cx: &App) -> bool {
        self.worktree_store.as_ref().is_some_and(|worktree_store| {
            TrustedWorktrees::has_restricted_worktrees(worktree_store, cx)
        })
    }

    /// Whether the buffer of a file (by its index in the file tree) is the real file on disk.
    pub fn is_file_on_disk(&self, file: usize) -> bool {
        self.files.get(file).is_some_and(|file| file.on_disk)
    }

    /// Shows a line under the header (or clears it), such as what's loading.
    pub fn set_notice(&mut self, notice: Option<SharedString>, cx: &mut Context<Self>) {
        self.notice = notice;
        cx.notify();
    }

    /// Asks the app for another scope of the agent thread under review.
    pub fn select_scope(&mut self, scope: ReviewScope, cx: &mut Context<Self>) {
        let Some(ReviewHeader::AgentThread(header)) = &self.header else {
            return;
        };
        if header.scope == scope {
            return;
        }
        self.notice = Some(format!("Loading {}…", scope.label()).into());
        cx.emit(ReviewViewEvent::SelectScope(scope));
        cx.notify();
    }

    /// Recomputes a real file's diff once it stops changing, and shows any new hunks.
    fn schedule_diff_update(&mut self, file: usize, cx: &mut Context<Self>) {
        let task = cx.spawn(async move |this, cx| {
            cx.background_executor()
                .timer(RECALCULATE_DIFF_DEBOUNCE)
                .await;
            let Ok(update) = this.update(cx, |this, cx| {
                let shown = this.files.get(file)?;
                let snapshot = shown.buffer.read(cx).text_snapshot();
                let diff = shown.diff.read(cx);
                let base = diff.base_text(cx);
                let base_text = diff.base_text_string(cx).map(Arc::<str>::from);
                Some(diff.update_diff(snapshot, &base, base_text, cx))
            }) else {
                return;
            };
            let Some(update) = update else {
                return;
            };
            let update = update.await;
            this.update(cx, |this, cx| {
                this.diff_updates.remove(&file);
                let Some(shown) = this.files.get(file) else {
                    return;
                };
                shown
                    .diff
                    .update(cx, |diff, cx| diff.set_snapshot(update, cx));
                let (path_key, buffer, diff) = (
                    shown.path_key.clone(),
                    shown.buffer.clone(),
                    shown.diff.clone(),
                );
                let ranges = hunk_ranges(&buffer, &diff, &[], cx);
                this.editor.update(cx, |editor, cx| {
                    let context_lines = multibuffer_context_lines(cx);
                    editor.update_excerpts_for_path(
                        path_key,
                        buffer,
                        ranges,
                        context_lines,
                        diff,
                        cx,
                    );
                });
                this.sync_thread_blocks(cx);
            })
            .ok();
        });
        self.diff_updates.insert(file, task);
    }

    pub fn file_tree(&self) -> &FileTreeModel {
        &self.file_tree
    }

    pub fn editor(&self) -> &Entity<SplittableEditor> {
        &self.editor
    }

    /// The editor and block holding a thread's card, if the diff places the thread.
    pub fn thread_block(&self, thread_id: &str) -> Option<(EditorSide, CustomBlockId)> {
        self.blocks.placed.get(thread_id).copied()
    }

    /// The threads listed at the top of a file because the diff can't place them, and the block
    /// that lists them.
    pub fn outdated_threads(&self, path: &str, cx: &App) -> Option<(CustomBlockId, Vec<String>)> {
        let file = self
            .file_tree
            .files()
            .iter()
            .position(|file| file.path == path)?;
        let (list, block) = self.blocks.outdated.get(&file)?;
        Some((*block, list.read(cx).thread_ids(cx)))
    }

    /// Threads on files the review doesn't show, which the header lists.
    pub fn unshown_thread_ids(&self, cx: &App) -> Vec<String> {
        self.unshown_thread_cards
            .iter()
            .map(|card| card.read(cx).thread().id.clone())
            .collect()
    }

    /// Scrolls to the start of a file, given its index in the file tree. Returns `false` for
    /// files the review doesn't show.
    /// Scrolls to the file at `path` (its path in the head revision, or a renamed file's old
    /// one), if the review shows it.
    pub fn scroll_to_path(&mut self, path: &str, window: &mut Window, cx: &mut App) -> bool {
        let file = self
            .file_tree
            .files()
            .iter()
            .position(|file| file.path == path || file.old_path.as_deref() == Some(path));
        file.is_some_and(|file| self.scroll_to_file(file, window, cx))
    }

    pub fn scroll_to_file(&mut self, file: usize, window: &mut Window, cx: &mut App) -> bool {
        let Some(shown) = self.files.get(file) else {
            return false;
        };
        let rhs_editor = self.editor.read(cx).rhs_editor().clone();
        let Some(position) = rhs_editor
            .read(cx)
            .buffer()
            .read(cx)
            .location_for_path(&shown.path_key, cx)
        else {
            return false;
        };
        rhs_editor.update(cx, |editor, cx| {
            // Put the file's header, and anything listed above its first line (such as outdated
            // threads), at the top rather than under the sticky header.
            let snapshot = editor.snapshot(window, cx);
            let first_row = position.to_display_point(&snapshot).row();
            let mut top = first_row;
            let blocks = snapshot
                .blocks_in_range(
                    DisplayRow(first_row.0.saturating_sub(BLOCK_ROWS_ABOVE_FILE))..first_row,
                )
                .map(|(row, block)| (row, block.height()))
                .collect::<Vec<_>>();
            for (row, height) in blocks.into_iter().rev() {
                if row.0 + height != top.0 {
                    break;
                }
                top = row;
            }
            let rows_above = f64::from(first_row.0 - top.0);
            editor.change_selections(
                SelectionEffects::scroll(Autoscroll::top_relative(rows_above)),
                window,
                cx,
                |selections| selections.select_anchor_ranges([position..position]),
            );
        });
        true
    }

    /// Replaces every card block, placing each thread and draft for the current split or
    /// unified diff.
    fn sync_thread_blocks(&mut self, cx: &mut Context<Self>) {
        let (rhs_editor, lhs_editor) = {
            let editor = self.editor.read(cx);
            (editor.rhs_editor().clone(), editor.lhs_editor().cloned())
        };

        let old_rhs_blocks = mem::take(&mut self.blocks.rhs);
        if !old_rhs_blocks.is_empty() {
            rhs_editor.update(cx, |editor, cx| {
                editor.remove_blocks(old_rhs_blocks, None, cx)
            });
        }
        // The left-hand editor is rebuilt on every split, so only remove blocks from the one
        // they were added to.
        if let Some((old_lhs_editor, old_lhs_blocks)) = self.blocks.lhs.take()
            && let Some(lhs_editor) = lhs_editor
                .as_ref()
                .filter(|lhs_editor| lhs_editor.downgrade() == old_lhs_editor)
        {
            lhs_editor.update(cx, |editor, cx| {
                editor.remove_blocks(old_lhs_blocks, None, cx)
            });
        }
        self.blocks.placed.clear();
        self.blocks.drafts.clear();

        enum BlockOwner {
            Thread(String),
            Draft(String),
            Outdated(usize, Entity<OutdatedThreads>),
        }
        let mut rhs_blocks = Vec::new();
        let mut rhs_owners = Vec::new();
        let mut lhs_blocks = Vec::new();
        let mut lhs_owners = Vec::new();
        let mut outdated_cards: Vec<Vec<Entity<ThreadCard>>> = vec![Vec::new(); self.files.len()];
        for (file, card) in &self.thread_cards {
            let Some(shown) = self.files.get(*file) else {
                continue;
            };
            let (thread, lines) = {
                let card = card.read(cx);
                (card.thread().clone(), card.estimated_lines())
            };
            match thread_anchor(
                &thread,
                &shown.buffer,
                &shown.diff,
                &rhs_editor,
                lhs_editor.as_ref(),
                cx,
            ) {
                Some((EditorSide::Right, anchor)) => {
                    rhs_blocks.push(card_block(
                        BlockPlacement::Below(anchor),
                        card.clone(),
                        lines,
                    ));
                    rhs_owners.push(BlockOwner::Thread(thread.id.clone()));
                }
                Some((EditorSide::Left, anchor)) => {
                    lhs_blocks.push(card_block(
                        BlockPlacement::Below(anchor),
                        card.clone(),
                        lines,
                    ));
                    lhs_owners.push(BlockOwner::Thread(thread.id.clone()));
                }
                None => outdated_cards[*file].push(card.clone()),
            }
        }
        for (file, card) in &self.draft_cards {
            let (draft, lines) = {
                let card = card.read(cx);
                (card.draft(cx), card.estimated_lines())
            };
            let Some((side, placement)) =
                self.draft_placement(*file, &draft, &rhs_editor, lhs_editor.as_ref(), cx)
            else {
                continue;
            };
            let block = card_block(placement, card.clone(), lines);
            match side {
                EditorSide::Right => {
                    rhs_blocks.push(block);
                    rhs_owners.push(BlockOwner::Draft(draft.id));
                }
                EditorSide::Left => {
                    lhs_blocks.push(block);
                    lhs_owners.push(BlockOwner::Draft(draft.id));
                }
            }
        }

        let mut old_outdated = mem::take(&mut self.blocks.outdated);
        for (file, cards) in outdated_cards.into_iter().enumerate() {
            if cards.is_empty() {
                continue;
            }
            let Some(anchor) = rhs_editor
                .read(cx)
                .buffer()
                .read(cx)
                .location_for_path(&self.files[file].path_key, cx)
            else {
                continue;
            };
            // Reusing the list keeps it expanded or collapsed across syncs.
            let list = match old_outdated.remove(&file) {
                Some((list, _)) => list,
                None => cx.new(|_| OutdatedThreads::new()),
            };
            list.update(cx, |list, cx| list.set_cards(cards, cx));
            rhs_blocks.push(card_block(BlockPlacement::Above(anchor), list.clone(), 2));
            rhs_owners.push(BlockOwner::Outdated(file, list));
        }

        let rhs_ids =
            rhs_editor.update(cx, |editor, cx| editor.insert_blocks(rhs_blocks, None, cx));
        for (owner, id) in rhs_owners.into_iter().zip(rhs_ids) {
            self.blocks.rhs.insert(id);
            match owner {
                BlockOwner::Thread(thread) => {
                    self.blocks.placed.insert(thread, (EditorSide::Right, id));
                }
                BlockOwner::Draft(draft) => {
                    self.blocks.drafts.insert(draft, (EditorSide::Right, id));
                }
                BlockOwner::Outdated(file, list) => {
                    self.blocks.outdated.insert(file, (list, id));
                }
            }
        }
        if let Some(lhs_editor) = &lhs_editor
            && !lhs_blocks.is_empty()
        {
            let lhs_ids =
                lhs_editor.update(cx, |editor, cx| editor.insert_blocks(lhs_blocks, None, cx));
            for (owner, id) in lhs_owners.into_iter().zip(&lhs_ids) {
                match owner {
                    BlockOwner::Thread(thread) => {
                        self.blocks.placed.insert(thread, (EditorSide::Left, *id));
                    }
                    BlockOwner::Draft(draft) => {
                        self.blocks.drafts.insert(draft, (EditorSide::Left, *id));
                    }
                    BlockOwner::Outdated(..) => {}
                }
            }
            self.blocks.lhs = Some((lhs_editor.downgrade(), lhs_ids.into_iter().collect()));
        }
        self.blocks.synced_split = Some(lhs_editor.is_some());
        cx.notify();
    }

    /// Where a draft's card goes: below its line, or above its file when the diff doesn't show
    /// the line (for example, a context line outside the excerpts).
    fn draft_placement(
        &self,
        file: usize,
        draft: &DraftComment,
        rhs_editor: &Entity<Editor>,
        lhs_editor: Option<&Entity<Editor>>,
        cx: &App,
    ) -> Option<(EditorSide, BlockPlacement<multi_buffer::Anchor>)> {
        let shown = self.files.get(file)?;
        match line_anchor(
            draft.position.side(),
            draft.position.line(),
            &shown.buffer,
            &shown.diff,
            rhs_editor,
            lhs_editor,
            cx,
        ) {
            Some((side, anchor)) => Some((side, BlockPlacement::Below(anchor))),
            None => {
                let anchor = rhs_editor
                    .read(cx)
                    .buffer()
                    .read(cx)
                    .location_for_path(&shown.path_key, cx)?;
                Some((EditorSide::Right, BlockPlacement::Above(anchor)))
            }
        }
    }

    /// Adds one draft's block without touching the others, which keeps the diff from jumping.
    fn place_draft_card(&mut self, file: usize, card: &Entity<DraftCard>, cx: &mut Context<Self>) {
        let (rhs_editor, lhs_editor) = {
            let editor = self.editor.read(cx);
            (editor.rhs_editor().clone(), editor.lhs_editor().cloned())
        };
        let (draft, lines) = {
            let card = card.read(cx);
            (card.draft(cx), card.estimated_lines())
        };
        let Some((side, placement)) =
            self.draft_placement(file, &draft, &rhs_editor, lhs_editor.as_ref(), cx)
        else {
            return;
        };
        let block = card_block(placement, card.clone(), lines);
        match (side, lhs_editor) {
            (EditorSide::Left, Some(lhs_editor)) => {
                let [id] = lhs_editor
                    .update(cx, |editor, cx| editor.insert_blocks([block], None, cx))
                    .try_into()
                    .unwrap_or_else(|_| unreachable!("one block was inserted"));
                match &mut self.blocks.lhs {
                    Some((editor, ids)) if *editor == lhs_editor.downgrade() => {
                        ids.insert(id);
                    }
                    lhs => *lhs = Some((lhs_editor.downgrade(), HashSet::from_iter([id]))),
                }
                self.blocks.drafts.insert(draft.id, (EditorSide::Left, id));
            }
            _ => {
                let ids =
                    rhs_editor.update(cx, |editor, cx| editor.insert_blocks([block], None, cx));
                for id in ids {
                    self.blocks.rhs.insert(id);
                    self.blocks
                        .drafts
                        .insert(draft.id.clone(), (EditorSide::Right, id));
                }
            }
        }
        cx.notify();
    }

    fn remove_draft(&mut self, id: &str, cx: &mut Context<Self>) {
        self.draft_cards
            .retain(|(_, card)| card.read(cx).draft(cx).id != id);
        self.stranded_drafts.retain(|draft| draft.id != id);
        let Some((side, block)) = self.blocks.drafts.remove(id) else {
            cx.notify();
            return;
        };
        let editor = self.editor.read(cx);
        match side {
            EditorSide::Right => {
                self.blocks.rhs.remove(&block);
                let rhs_editor = editor.rhs_editor().clone();
                rhs_editor.update(cx, |editor, cx| {
                    editor.remove_blocks(HashSet::from_iter([block]), None, cx)
                });
            }
            EditorSide::Left => {
                let lhs_editor = editor.lhs_editor().cloned();
                if let Some((editor, ids)) = &mut self.blocks.lhs
                    && ids.remove(&block)
                    && let Some(lhs_editor) =
                        lhs_editor.filter(|lhs_editor| lhs_editor.downgrade() == *editor)
                {
                    lhs_editor.update(cx, |editor, cx| {
                        editor.remove_blocks(HashSet::from_iter([block]), None, cx)
                    });
                }
            }
        }
        cx.notify();
    }

    fn title(&self) -> SharedString {
        if let Some(header) = &self.header {
            return header.tab_title();
        }
        let file_count = self.file_tree.file_count();
        if file_count == 1 {
            "Review (1 file)".into()
        } else {
            format!("Review ({file_count} files)").into()
        }
    }

    fn render_unshown_threads(&self, cx: &mut Context<Self>) -> Option<AnyElement> {
        if self.unshown_thread_cards.is_empty() {
            return None;
        }
        let count = self.unshown_thread_cards.len();
        let expanded = self.unshown_threads_expanded;
        let colors = cx.theme().colors();
        Some(
            v_flex()
                .gap_1()
                .child(
                    h_flex()
                        .id("unshown-threads")
                        .gap_1p5()
                        .cursor_pointer()
                        .on_click(cx.listener(|this, _, _, cx| {
                            this.unshown_threads_expanded = !this.unshown_threads_expanded;
                            cx.notify();
                        }))
                        .child(
                            Icon::new(if expanded {
                                IconName::ChevronDown
                            } else {
                                IconName::ChevronRight
                            })
                            .size(IconSize::XSmall)
                            .color(Color::Muted),
                        )
                        .child(
                            Icon::new(IconName::Chat)
                                .size(IconSize::Small)
                                .color(Color::Muted),
                        )
                        .child(
                            Label::new(if count == 1 {
                                "1 thread on a file the review can't show".to_string()
                            } else {
                                format!("{count} threads on files the review can't show")
                            })
                            .size(LabelSize::Small)
                            .color(Color::Muted),
                        ),
                )
                .when(expanded, |this| {
                    this.child(
                        v_flex()
                            .id("unshown-thread-list")
                            .max_h(scaled(px(320.)))
                            .overflow_y_scroll()
                            .gap_2()
                            .p_2()
                            .rounded(scaled(tokens::CORNER_RADIUS))
                            .border(tokens::BORDER_WIDTH)
                            .border_color(colors.border_variant)
                            .children(self.unshown_thread_cards.iter().cloned()),
                    )
                })
                .into_any_element(),
        )
    }
}

/// Comments, drafts, and the other writes.
impl ReviewView {
    /// The pending review as it stands, including text still being written.
    pub fn pending_review(&self, cx: &App) -> PendingReview {
        let thread_cards = self
            .thread_cards
            .iter()
            .map(|(_, card)| card)
            .chain(&self.unshown_thread_cards)
            .map(|card| card.read(cx));
        let mut replies = BTreeMap::new();
        let mut edits = BTreeMap::new();
        for card in thread_cards {
            if let Some(reply) = card
                .unsent_reply(cx)
                .filter(|reply| !reply.trim().is_empty())
            {
                replies.insert(card.thread().id.clone(), reply);
            }
            if let Some((comment_id, text)) = card.unsaved_edit(cx) {
                edits.insert(comment_id, text);
            }
        }
        PendingReview {
            comments: self
                .draft_cards
                .iter()
                .map(|(_, card)| card.read(cx).draft(cx))
                // A composer that was opened but never written in isn't worth keeping.
                .filter(|draft| draft.added || !draft.body.trim().is_empty())
                .chain(self.stranded_drafts.iter().cloned())
                .collect(),
            summary: self.submit.summary.text(cx),
            verdict: Some(self.submit.verdict),
            replies,
            edits,
        }
    }

    /// The draft cards in the diff, in the order they were written.
    pub fn draft_cards(&self) -> impl Iterator<Item = &Entity<DraftCard>> {
        self.draft_cards.iter().map(|(_, card)| card)
    }

    /// The editor and block holding a draft's card.
    pub fn draft_block(&self, draft_id: &str) -> Option<(EditorSide, CustomBlockId)> {
        self.blocks.drafts.get(draft_id).copied()
    }

    /// Drafts the diff no longer has a place for. They're kept, but not submitted.
    pub fn stranded_drafts(&self) -> &[DraftComment] {
        &self.stranded_drafts
    }

    /// Why the last comment couldn't be started, or what just happened.
    /// What the review says in place of the diff when it has no files: for an agent thread, that
    /// the scope has no changes.
    pub fn empty_label(&self) -> Option<SharedString> {
        if !self.file_tree.files().is_empty() {
            return None;
        }
        Some(match &self.header {
            Some(ReviewHeader::AgentThread(header)) => header.scope.empty_label().into(),
            _ => "No changed files".into(),
        })
    }

    pub fn notice(&self) -> Option<&SharedString> {
        self.notice.as_ref()
    }

    pub fn thread_card(&self, thread_id: &str, cx: &App) -> Option<Entity<ThreadCard>> {
        self.thread_cards
            .iter()
            .map(|(_, card)| card)
            .chain(&self.unshown_thread_cards)
            .find(|card| card.read(cx).thread().id == thread_id)
            .cloned()
    }

    pub fn permissions(&self) -> &ReviewPermissions {
        &self.permissions
    }

    fn focus_diff(&self, window: &mut Window, cx: &mut App) {
        let editor = self.editor.read(cx).focused_editor().clone();
        window.focus(&editor.focus_handle(cx), cx);
    }

    fn schedule_save(&mut self, cx: &mut Context<Self>) {
        if self.writes.is_none() {
            return;
        }
        self.save_task = Some(cx.spawn(async move |this, cx| {
            cx.background_executor().timer(SAVE_DELAY).await;
            this.update(cx, |this, cx| {
                this.save_task = None;
                this.save_now(cx);
            })
            .ok();
        }));
    }

    fn save_now(&self, cx: &mut App) {
        if let Some(writes) = &self.writes {
            let pending = self.pending_review(cx);
            writes.backend.save_pending_review(&pending, cx);
        }
    }

    fn add_comment(&mut self, _: &AddComment, window: &mut Window, cx: &mut Context<Self>) {
        match self.comment_target(cx) {
            Ok(target) => {
                self.notice = None;
                self.open_draft(target, window, cx);
            }
            Err(notice) => {
                self.notice = Some(notice);
                cx.notify();
            }
        }
    }

    /// Where a comment on the selected lines goes, or why there can't be one.
    fn comment_target(&self, cx: &mut App) -> Result<CommentTarget, SharedString> {
        if self.writes.is_none() {
            return Err("This review is read-only.".into());
        }
        if !self.permissions.line_comments {
            return Err("This pull request doesn't take line comments from you.".into());
        }
        let editor = self.editor.read(cx).focused_editor().clone();
        let (start_row, end_row) = editor.update(cx, |editor, cx| {
            let snapshot = editor.display_snapshot(cx);
            let selection = editor.selections.newest::<Point>(&snapshot);
            let mut end = selection.end;
            // A selection of whole lines ends at the start of the next one.
            if end.column == 0 && end.row > selection.start.row {
                end.row -= 1;
            }
            (selection.start.row, end.row)
        });
        let snapshot = editor.read(cx).buffer().read(cx).snapshot(cx);
        let row_info = |row: u32| snapshot.row_infos(MultiBufferRow(row)).next();
        let Some((file, side, line)) =
            row_info(end_row).and_then(|info| self.locate_row(&info, cx))
        else {
            return Err("Put the cursor on a line of the diff to comment on it.".into());
        };
        let shown = &self.files[file];
        let position = match &shown.commentable {
            None => match side {
                DiffSide::Right => CommentPosition::Added { new_line: line },
                DiffSide::Left => CommentPosition::Deleted { old_line: line },
            },
            Some(commentable) => commentable
                .position(side, line)
                .ok_or_else(|| not_commentable_reason(shown, commentable, side, line))?,
        };
        let start_line = row_info(start_row)
            .and_then(|info| self.locate_row(&info, cx))
            .filter(|(start_file, start_side, start_line)| {
                *start_file == file
                    && *start_side == side
                    && *start_line < line
                    && shown
                        .commentable
                        .as_ref()
                        .is_none_or(|commentable| commentable.position(side, *start_line).is_some())
            })
            .map(|(_, _, start_line)| start_line);
        Ok(CommentTarget {
            file,
            position,
            start_line,
        })
    }

    /// The file, side, and 1-based line a row of the diff shows.
    fn locate_row(&self, info: &RowInfo, cx: &App) -> Option<(usize, DiffSide, u32)> {
        let buffer_id = info.buffer_id?;
        let line = info.buffer_row? + 1;
        self.files.iter().enumerate().find_map(|(index, file)| {
            if file.buffer.read(cx).remote_id() == buffer_id {
                Some((index, DiffSide::Right, line))
            } else if file.diff.read(cx).base_text_buffer().read(cx).remote_id() == buffer_id {
                Some((index, DiffSide::Left, line))
            } else {
                None
            }
        })
    }

    fn open_draft(&mut self, target: CommentTarget, window: &mut Window, cx: &mut Context<Self>) {
        // A comment already being written on that line gets the focus instead of a second one.
        if let Some((_, card)) = self.draft_cards.iter().find(|(file, card)| {
            let card = card.read(cx);
            *file == target.file && card.is_editing() && card.draft(cx).position == target.position
        }) {
            card.clone().update(cx, |card, cx| card.focus(window, cx));
            return;
        }
        let shown = &self.files[target.file];
        let draft = DraftComment {
            id: uuid::Uuid::new_v4().to_string(),
            path: shown.path.clone(),
            old_path: shown.old_path.clone(),
            position: target.position,
            start_line: target.start_line,
            body: String::new(),
            added: false,
        };
        let card = self.new_draft_card(draft, window, cx);
        self.draft_cards.push((target.file, card.clone()));
        self.place_draft_card(target.file, &card, cx);
        // The composer goes below the line, so a line near the bottom would hide it.
        let editor = self.editor.read(cx).focused_editor().clone();
        editor.update(cx, |editor, cx| {
            editor.request_autoscroll(Autoscroll::center(), cx)
        });
        card.update(cx, |card, cx| card.focus(window, cx));
        cx.notify();
    }

    fn reload_threads(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(writes) = &self.writes else {
            return;
        };
        let task = writes.backend.reload_threads(cx);
        self.reload_task = Some(cx.spawn_in(window, async move |this, cx| match task.await {
            Ok(threads) => {
                this.update_in(cx, |this, window, cx| {
                    this.reload_task = None;
                    this.set_threads(threads, window, cx)
                })
                .ok();
            }
            Err(error) => {
                log::error!("couldn't load the review threads again: {error:#}");
                this.update(cx, |this, cx| {
                    this.reload_task = None;
                    this.notice = Some(format!("Couldn't refresh the threads: {error:#}").into());
                    cx.notify();
                })
                .ok();
            }
        }));
    }

    /// Shows the threads as the forge now reports them. Cards are reused by thread id, so text
    /// being written in them is kept.
    fn set_threads(
        &mut self,
        threads: Vec<ReviewThread>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let mut existing = self
            .thread_cards
            .drain(..)
            .map(|(_, card)| card)
            .chain(self.unshown_thread_cards.drain(..))
            .map(|card| (card.read(cx).thread().id.clone(), card))
            .collect::<HashMap<_, _>>();
        for thread in threads {
            let thread = Arc::new(thread);
            let file = self
                .files
                .iter()
                .position(|file| file.has_path(&thread.path));
            let card = match existing.remove(&thread.id) {
                Some(card) => {
                    card.update(cx, |card, cx| card.set_thread(thread, cx));
                    card
                }
                None => {
                    let location = file.is_none().then(|| thread_location(&thread));
                    self.new_thread_card(thread, location, window, cx)
                }
            };
            match file {
                Some(file) => self.thread_cards.push((file, card)),
                None => self.unshown_thread_cards.push(card),
            }
        }
        self.sync_thread_blocks(cx);
    }

    /// Marks a file viewed, or not, on the forge. The tree shows the new mark right away and
    /// goes back to the old one if the forge refuses.
    pub fn toggle_viewed(&mut self, file: usize, cx: &mut Context<Self>) {
        let Some(writes) = self.writes.clone() else {
            return;
        };
        if !self.can_mark_viewed() || self.viewed_in_flight.contains_key(&file) {
            return;
        }
        let Some(entry) = self.file_tree.files().get(file) else {
            return;
        };
        let previous = entry.viewed;
        let viewed = previous != FileViewedState::Viewed;
        let path = entry.path.clone();
        self.file_tree.set_viewed(
            file,
            if viewed {
                FileViewedState::Viewed
            } else {
                FileViewedState::Unviewed
            },
        );
        self.viewed_error = None;
        let task = writes
            .backend
            .set_files_viewed(vec![(path.clone(), viewed)], cx);
        let task = cx.spawn(async move |this, cx| {
            let result = task.await;
            this.update(cx, |this, cx| {
                this.viewed_in_flight.remove(&file);
                if let Err(error) = result {
                    log::error!("couldn't mark {path} viewed: {error:#}");
                    this.file_tree.set_viewed(file, previous);
                    this.viewed_error = Some(
                        format!(
                            "Couldn't mark {path} {}: {error:#}",
                            if viewed { "viewed" } else { "not viewed" }
                        )
                        .into(),
                    );
                }
                cx.notify();
            })
            .ok();
        });
        self.viewed_in_flight.insert(file, task);
        cx.notify();
    }

    /// Whether the viewed checkboxes can be clicked.
    pub fn can_mark_viewed(&self) -> bool {
        self.writes.is_some()
            && self.file_tree.tracks_viewed()
            && self.permissions.mark_viewed
            && self.permissions.blocked.is_none()
    }

    pub fn viewed_error(&self) -> Option<&SharedString> {
        self.viewed_error.as_ref()
    }

    pub fn is_marking_viewed(&self, file: usize) -> bool {
        self.viewed_in_flight.contains_key(&file)
    }

    fn key_context(&self, window: &Window, cx: &App) -> KeyContext {
        let mut context = KeyContext::new_with_defaults();
        context.add("ReviewView");
        // The composers live in the diff's editor, so typing in one must not count as typing
        // in the diff.
        let editor = self.editor.read(cx);
        let diff_focused = editor.rhs_editor().focus_handle(cx).is_focused(window)
            || editor
                .lhs_editor()
                .is_some_and(|lhs| lhs.focus_handle(cx).is_focused(window));
        if !diff_focused {
            context.add("editing_comment");
        }
        context
    }
}

/// Why a line can't take a comment, for a line the forge's hunks don't include.
fn not_commentable_reason(
    shown: &ShownFile,
    commentable: &CommentableLines,
    side: DiffSide,
    line: u32,
) -> SharedString {
    let location = match side {
        DiffSide::Right => format!("{}:{line}", shown.path),
        DiffSide::Left => format!("{}:{line} (base)", shown.path),
    };
    match commentable.distance_to_nearest(side, line) {
        _ if commentable.is_empty() => format!(
            "{} has no diff hunks on the forge, so it can't take line comments, and T3 can't \
             post comments on whole files.",
            shown.path
        )
        .into(),
        Some(distance) => format!(
            "The forge only takes comments on lines in the diff's hunks. {location} is {distance} \
             {} from the nearest one.",
            if distance == 1 { "line" } else { "lines" }
        )
        .into(),
        None => format!(
            "The forge only takes comments on lines in the diff's hunks, and {location} isn't in one."
        )
        .into(),
    }
}

fn thread_location(thread: &ReviewThread) -> SharedString {
    match (thread.line, thread.side) {
        (Some(line), DiffSide::Right) => format!("{}:{line}", thread.path).into(),
        (Some(line), DiffSide::Left) => format!("{}:{line} (base)", thread.path).into(),
        (None, _) => thread.path.clone().into(),
    }
}

async fn load_entry(
    contents: ReviewFileContents,
    worktree_id: Option<WorktreeId>,
    project: &Entity<Project>,
    language_registry: &Arc<LanguageRegistry>,
    cx: &mut AsyncWindowContext,
) -> Result<ReviewEntry> {
    let change = contents.change();
    // A deleted file has nothing on disk, so it keeps an empty in-memory buffer.
    let real_buffer = match worktree_id {
        Some(worktree_id) if contents.head_text.is_some() => {
            let project_path = ProjectPath {
                worktree_id,
                path: contents.path.clone(),
            };
            match project
                .update(cx, |project, cx| project.open_buffer(project_path, cx))
                .await
            {
                // A path that isn't on disk opens as a new, empty buffer.
                Ok(buffer)
                    if cx.update(|_, cx| {
                        buffer.read(cx).file().is_some_and(|file| {
                            matches!(file.disk_state(), DiskState::Present { .. })
                        })
                    })? =>
                {
                    Some(buffer)
                }
                Ok(_) => {
                    log::warn!(
                        "{} isn't in the worktree, showing its text instead",
                        contents.path.as_unix_str()
                    );
                    None
                }
                Err(error) => {
                    log::warn!(
                        "couldn't open {} from the worktree, showing its text instead: {error:#}",
                        contents.path.as_unix_str()
                    );
                    None
                }
            }
        }
        _ => None,
    };
    let on_disk = real_buffer.is_some();
    let buffer = match real_buffer {
        Some(buffer) => buffer,
        None => {
            let file = Arc::new(ReviewFile {
                path: contents.path.clone(),
                is_deleted: contents.head_text.is_none(),
            }) as Arc<dyn File>;
            build_buffer(
                contents.head_text.unwrap_or_default(),
                file,
                language_registry,
                cx,
            )
            .await?
        }
    };
    let diff = build_buffer_diff(contents.base_text, &buffer, language_registry, cx).await?;
    Ok(ReviewEntry {
        path: contents.path,
        old_path: contents.old_path,
        change,
        buffer,
        diff,
        commentable: contents.commentable,
        on_disk,
    })
}

// Copied from `build_buffer` in `crates/git_ui/src/commit_view.rs`, which is `pub(crate)`.
async fn build_buffer(
    mut text: String,
    file: Arc<dyn File>,
    language_registry: &Arc<LanguageRegistry>,
    cx: &mut AsyncWindowContext,
) -> Result<Entity<Buffer>> {
    let line_ending = LineEnding::detect(&text);
    LineEnding::normalize(&mut text);
    let text = Rope::from(text);
    let language_id =
        cx.update(|_, cx| language_registry.language_for_file(&file, Some(&text), cx))?;
    let language = match language_id {
        Some(language_id) => language_registry
            .load_language(language_id)
            .await
            .ok()
            .and_then(|language| language.log_err()),
        None => None,
    };
    let buffer = cx.new(|cx| {
        let text_buffer = TextBuffer::new_normalized(
            ReplicaId::LOCAL,
            cx.entity_id().as_non_zero_u64().into(),
            line_ending,
            text,
        );
        let mut buffer = Buffer::build(text_buffer, Some(file), Capability::ReadOnly, cx);
        buffer.set_language_registry(language_registry.clone());
        buffer.set_language_async(language, cx);
        buffer
    });
    Ok(buffer)
}

// Copied from `build_buffer_diff` in `crates/git_ui/src/commit_view.rs`. `BufferDiff::new` gives
// the base text buffer the head buffer's language, so deleted lines are highlighted too.
async fn build_buffer_diff(
    mut base_text: Option<String>,
    buffer: &Entity<Buffer>,
    language_registry: &Arc<LanguageRegistry>,
    cx: &mut AsyncWindowContext,
) -> Result<Entity<BufferDiff>> {
    if let Some(base_text) = &mut base_text {
        LineEnding::normalize(base_text);
    }

    let language = cx.update(|_, cx| buffer.read(cx).language().cloned())?;
    let snapshot = cx.update(|_, cx| buffer.read(cx).snapshot())?;

    let diff = cx.new(|cx| {
        BufferDiff::new(
            &snapshot.text,
            language,
            Some(language_registry.clone()),
            cx,
        )
    });

    diff.update(cx, |diff, cx| {
        diff.set_base_text(
            base_text.map(|base_text| Arc::from(base_text.as_str())),
            snapshot.text.clone(),
            cx,
        )
    })
    .await;

    Ok(diff)
}

/// The hunks, plus the rows threads are on. A file without hunks (for example, one that was only
/// renamed) is shown in full, as `CommitView` does, so it doesn't silently vanish from the review.
fn excerpt_ranges(entry: &ReviewEntry, thread_rows: &[u32], cx: &App) -> Vec<Range<Point>> {
    hunk_ranges(&entry.buffer, &entry.diff, thread_rows, cx)
}

fn hunk_ranges(
    buffer: &Entity<Buffer>,
    diff: &Entity<BufferDiff>,
    thread_rows: &[u32],
    cx: &App,
) -> Vec<Range<Point>> {
    let snapshot = buffer.read(cx).snapshot();
    let diff_snapshot = diff.read(cx).snapshot(cx);
    let mut ranges = diff_snapshot
        .hunks(&snapshot)
        .map(|hunk| hunk.buffer_range.to_point(&snapshot))
        .collect::<Vec<_>>();
    if ranges.is_empty() {
        return vec![Point::zero()..snapshot.max_point()];
    }
    ranges.extend(
        thread_rows
            .iter()
            .map(|row| Point::new(*row, 0)..Point::new(*row, 0)),
    );
    ranges.sort_by_key(|range| range.start);
    ranges
}

/// Lines added and removed in a file's diff.
fn line_counts(buffer: &Entity<Buffer>, diff: &Entity<BufferDiff>, cx: &App) -> (u32, u32) {
    let snapshot = buffer.read(cx).snapshot();
    let diff_snapshot = diff.read(cx).snapshot(cx);
    let base = diff_snapshot.base_text();
    diff_snapshot
        .hunks(&snapshot)
        .fold((0, 0), |(added, removed), hunk| {
            let base_range = base.offset_to_point(hunk.diff_base_byte_range.start)
                ..base.offset_to_point(hunk.diff_base_byte_range.end);
            (
                added + row_count(&hunk.range),
                removed + row_count(&base_range),
            )
        })
}

/// The lines a range covers. Ranges end at the start of the next line, except at the end of a
/// file without a trailing newline.
fn row_count(range: &Range<Point>) -> u32 {
    range.end.row - range.start.row + u32::from(range.end.column > 0)
}

impl EventEmitter<EditorEvent> for ReviewView {}
impl EventEmitter<ReviewViewEvent> for ReviewView {}

impl Focusable for ReviewView {
    fn focus_handle(&self, cx: &App) -> FocusHandle {
        self.editor.focus_handle(cx)
    }
}

// The delegation below is copied from `impl Item for MultiDiffView`.
impl Item for ReviewView {
    type Event = EditorEvent;

    fn tab_icon(&self, _window: &Window, _cx: &App) -> Option<Icon> {
        Some(Icon::new(IconName::Diff).color(Color::Muted))
    }

    fn tab_tooltip_text(&self, _cx: &App) -> Option<SharedString> {
        match &self.header {
            Some(header) => Some(format!("{}\n{}", header.title(), header.subtitle()).into()),
            None => Some(self.title()),
        }
    }

    fn tab_content_text(&self, _detail: usize, _cx: &App) -> SharedString {
        self.title()
    }

    fn to_item_events(event: &EditorEvent, f: &mut dyn FnMut(ItemEvent)) {
        Editor::to_item_events(event, f)
    }

    fn deactivated(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.editor
            .update(cx, |editor, cx| editor.deactivated(window, cx));
    }

    fn act_as_type<'a>(
        &'a self,
        type_id: TypeId,
        self_handle: &'a Entity<Self>,
        cx: &'a App,
    ) -> Option<gpui::AnyEntity> {
        if type_id == TypeId::of::<Self>() {
            Some(self_handle.clone().into())
        } else if type_id == TypeId::of::<SplittableEditor>() {
            // Zed's buffer search bar shows collapse and split buttons for items that act as a
            // `SplittableEditor`; the review's own toolbar has those controls.
            None
        } else {
            self.editor.act_as_type(type_id, cx)
        }
    }

    fn as_searchable(&self, _: &Entity<Self>, _: &App) -> Option<Box<dyn SearchableItemHandle>> {
        Some(Box::new(self.editor.clone()))
    }

    fn active_project_path(&self, cx: &App) -> Option<ProjectPath> {
        self.editor.read(cx).active_project_path(cx)
    }

    fn set_nav_history(
        &mut self,
        nav_history: ItemNavHistory,
        _: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.editor.update(cx, |editor, cx| {
            editor.rhs_editor().update(cx, |editor, _| {
                editor.set_nav_history(Some(nav_history));
            });
        });
    }

    fn navigate(
        &mut self,
        data: Arc<dyn Any + Send>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> bool {
        self.editor
            .update(cx, |editor, cx| editor.navigate(data, window, cx))
    }

    fn breadcrumb_location(&self, _: &App) -> ToolbarItemLocation {
        ToolbarItemLocation::PrimaryLeft
    }

    fn breadcrumbs(&self, cx: &App) -> Option<(Vec<HighlightedText>, Option<Font>)> {
        self.editor.breadcrumbs(cx)
    }

    fn added_to_workspace(
        &mut self,
        workspace: &mut Workspace,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.editor.update(cx, |editor, cx| {
            editor.added_to_workspace(workspace, window, cx)
        });
    }
}

impl Render for ReviewView {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let unshown_threads = self.render_unshown_threads(cx);
        let actions = match (self.header_actions.clone(), self.render_review_actions(cx)) {
            (None, actions) => actions,
            // The app's actions go to the next line when there's no room for them next to the
            // title, at the right, and shrink there, putting what doesn't fit in a menu.
            (Some(header_actions), actions) => Some(
                h_flex()
                    .flex_shrink_1()
                    .min_w_0()
                    .ml_auto()
                    .justify_end()
                    .gap_2()
                    .child(header_actions)
                    .children(actions)
                    .into_any_element(),
            ),
        };
        let compact_header = window.viewport_size().width
            < tokens::scaled(header::COMPACT_BELOW_WIDTH).to_pixels(window.rem_size());
        let submit_form = self.render_submit_form(cx);
        let details = self.header_details.clone();
        let extra =
            (details.is_some() || unshown_threads.is_some() || submit_form.is_some()).then(|| {
                v_flex()
                    .gap_2()
                    .pt_1()
                    .children(details)
                    .children(unshown_threads)
                    .children(submit_form)
                    .into_any_element()
            });
        let colors = cx.theme().colors();
        let notice = self.notice.clone().map(|notice| {
            h_flex()
                .flex_none()
                .px_3()
                .py_1()
                .gap_1p5()
                .bg(colors.surface_background)
                .border_b(tokens::BORDER_WIDTH)
                .border_color(colors.border_variant)
                .child(
                    Icon::new(IconName::Info)
                        .size(IconSize::Small)
                        .color(Color::Muted),
                )
                .child(
                    div()
                        .flex_1()
                        .min_w_0()
                        .child(Label::new(notice).size(LabelSize::Small)),
                )
                .child(
                    IconButton::new("dismiss-review-notice", IconName::Close)
                        .icon_size(IconSize::XSmall)
                        .on_click(cx.listener(|this, _, _, cx| {
                            this.notice = None;
                            cx.notify();
                        })),
                )
        });
        v_flex()
            .key_context(self.key_context(window, cx))
            .on_action(cx.listener(Self::add_comment))
            .on_action(cx.listener(Self::open_submit_form))
            .size_full()
            .map(|this| match &self.header {
                Some(ReviewHeader::PullRequest(header)) => {
                    this.child(header::render_header(header, actions, extra, cx))
                }
                Some(ReviewHeader::AgentThread(header)) => this.child(agent::render_agent_header(
                    header,
                    cx.entity().downgrade(),
                    self.is_worktree_restricted(cx),
                    compact_header,
                    actions,
                    extra,
                    cx,
                )),
                None if actions.is_some() || extra.is_some() => this.child(
                    v_flex()
                        .flex_none()
                        .px_3()
                        .py_2()
                        .gap_1()
                        .border_b(tokens::BORDER_WIDTH)
                        .border_color(colors.border)
                        .children(actions.map(|actions| h_flex().justify_end().child(actions)))
                        .children(extra),
                ),
                None => this,
            })
            .map(|this| match self.empty_label() {
                // Nothing to show or lay out, so no toolbar either.
                Some(label) => this.children(notice).child(
                    v_flex()
                        .flex_1()
                        .min_h_0()
                        .items_center()
                        .justify_center()
                        .child(Label::new(label).color(Color::Muted)),
                ),
                None => this
                    .child(self.toolbar.clone())
                    .children(notice)
                    .child(div().flex_1().min_h_0().child(self.editor.clone())),
            })
    }
}

#[cfg(test)]
mod agent_tests;

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use buffer_diff::DiffHunkStatusKind;
    use editor::display_map::Block;
    use gpui::{TestAppContext, VisualTestContext};
    use language::{markdown_lang, rust_lang};
    use parking_lot::Mutex;
    use project::FakeFs;
    use settings::{DiffViewStyle, SettingsStore};
    use workspace::MultiWorkspace;

    pub(crate) const RUST_BASE: &str = "fn main() {\n    println!(\"hello\");\n}\n";
    pub(crate) const RUST_HEAD: &str = "fn main() {\n    println!(\"hello, world\");\n}\n";
    const MARKDOWN_BASE: &str = "# Title\n\nIntro.\n";
    const MARKDOWN_HEAD: &str = "# Title\n\nIntro.\n\nA new paragraph.\n";

    pub(crate) fn review_files() -> Vec<ReviewFileContents> {
        vec![
            ReviewFileContents {
                path: rel_path("src/main.rs"),
                old_path: None,
                base_text: Some(RUST_BASE.to_string()),
                head_text: Some(RUST_HEAD.to_string()),
                commentable: None,
            },
            ReviewFileContents {
                path: rel_path("docs/README.md"),
                old_path: None,
                base_text: Some(MARKDOWN_BASE.to_string()),
                head_text: Some(MARKDOWN_HEAD.to_string()),
                commentable: None,
            },
        ]
    }

    pub(crate) fn rel_path(path: &str) -> Arc<RelPath> {
        RelPath::from_unix_str(path)
            .expect("test paths are valid relative paths")
            .into_arc()
    }

    fn thread(id: &str, path: &str, line: Option<u32>, side: DiffSide) -> ReviewThread {
        ReviewThread {
            id: id.to_string(),
            path: path.to_string(),
            line,
            side,
            is_resolved: false,
            is_outdated: line.is_none(),
            can_resolve: true,
            comments: vec![ThreadComment {
                id: format!("{id}-comment"),
                author: Some("octocat".to_string()),
                body: format!("A comment on thread {id}."),
                created_at: None,
                edited: false,
                can_edit: false,
            }],
            comment_count: 1,
            sent_to_agent: false,
        }
    }

    // Modeled on `test_init` in `crates/git_ui/src/multi_diff_view.rs`, but with a project that
    // has no worktrees, as when reviewing a pull request without a local checkout.
    pub(crate) async fn build_review_view(
        contents: ReviewContents,
        diff_view_style: DiffViewStyle,
        cx: &mut TestAppContext,
    ) -> (
        Entity<ReviewView>,
        Entity<Workspace>,
        &mut VisualTestContext,
    ) {
        let fs = FakeFs::new(cx.executor());
        build_review_view_with_fs(contents, diff_view_style, fs, cx).await
    }

    pub(crate) async fn build_review_view_with_fs(
        contents: ReviewContents,
        diff_view_style: DiffViewStyle,
        fs: Arc<FakeFs>,
        cx: &mut TestAppContext,
    ) -> (
        Entity<ReviewView>,
        Entity<Workspace>,
        &mut VisualTestContext,
    ) {
        cx.update(|cx| {
            let settings_store = SettingsStore::test(cx);
            cx.set_global(settings_store);
            cx.update_global::<SettingsStore, _>(|store, cx| {
                store.update_user_settings(cx, |settings| {
                    settings.editor.diff_view_style = Some(diff_view_style);
                });
            });
            theme_settings::init(theme::LoadThemes::JustBase, cx);
            cx.bind_keys(key_bindings());
        });

        let project = Project::test(fs, [], cx).await;
        project.read_with(cx, |project, _| {
            let languages = project.languages();
            languages.add(rust_lang());
            languages.add(markdown_lang());
        });

        let (multi_workspace, cx) =
            cx.add_window_view(|window, cx| MultiWorkspace::test_new(project.clone(), window, cx));
        let workspace =
            multi_workspace.read_with(cx, |multi_workspace, _| multi_workspace.workspace().clone());

        let review_view = workspace
            .update_in(cx, |workspace, window, cx| {
                ReviewView::open(contents, workspace, window, cx)
            })
            .await
            .expect("review view should open");
        cx.run_until_parked();

        (review_view, workspace, cx)
    }

    pub(crate) fn rhs_editor(
        review_view: &Entity<ReviewView>,
        cx: &mut VisualTestContext,
    ) -> Entity<Editor> {
        review_view.read_with(cx, |review_view, cx| {
            review_view.editor.read(cx).rhs_editor().clone()
        })
    }

    fn lhs_editor(review_view: &Entity<ReviewView>, cx: &mut VisualTestContext) -> Entity<Editor> {
        review_view.read_with(cx, |review_view, cx| {
            review_view
                .editor
                .read(cx)
                .lhs_editor()
                .cloned()
                .expect("the diff should be split")
        })
    }

    pub(crate) fn buffer_for_path(
        review_view: &Entity<ReviewView>,
        path: &str,
        cx: &mut VisualTestContext,
    ) -> Entity<Buffer> {
        let rhs_editor = rhs_editor(review_view, cx);
        rhs_editor.read_with(cx, |editor, cx| {
            editor
                .buffer()
                .read(cx)
                .all_buffers_iter()
                .find(|buffer| {
                    buffer
                        .read(cx)
                        .file()
                        .is_some_and(|file| file.path().as_unix_str() == path)
                })
                .unwrap_or_else(|| panic!("review should contain a buffer for {path}"))
        })
    }

    /// The text line nearest to a block, above or below it, skipping the rows of other blocks
    /// (such as the spacers that keep the two sides of a split diff aligned).
    pub(crate) fn line_next_to_block(
        editor: &Entity<Editor>,
        block: CustomBlockId,
        above: bool,
        cx: &mut VisualTestContext,
    ) -> String {
        let (lines, block_rows, row) = editor.update_in(cx, |editor, window, cx| {
            let snapshot = editor.snapshot(window, cx);
            let lines = editor
                .display_text(cx)
                .lines()
                .map(str::to_string)
                .collect::<Vec<_>>();
            let mut block_rows = HashSet::default();
            let mut row = None;
            for (start, other) in snapshot
                .blocks_in_range(DisplayRow(0)..DisplayRow(snapshot.max_point().row().0 + 1))
            {
                let start = start.0 as usize;
                block_rows.extend(start..start + other.height() as usize);
                if let Block::Custom(other) = other
                    && other.id == block
                {
                    row = Some(start);
                }
            }
            (lines, block_rows, row)
        });
        let row = row.unwrap_or_else(|| panic!("block {block:?} should be in the editor"));
        let mut line = row;
        loop {
            line = if above {
                line.checked_sub(1)
                    .unwrap_or_else(|| panic!("no line above row {row}:\n{lines:#?}"))
            } else {
                line + 1
            };
            if !block_rows.contains(&line) {
                break;
            }
        }
        lines
            .get(line)
            .cloned()
            .unwrap_or_else(|| panic!("no line below row {row}:\n{lines:#?}"))
    }

    #[gpui::test]
    async fn test_review_view_from_in_memory_files(cx: &mut TestAppContext) {
        let contents = ReviewContents {
            files: review_files(),
            ..ReviewContents::default()
        };
        let (review_view, _, cx) = build_review_view(contents, DiffViewStyle::Unified, cx).await;
        let rhs_editor = rhs_editor(&review_view, cx);

        review_view.read_with(cx, |review_view, cx| {
            assert_eq!(review_view.tab_content_text(0, cx), "Review (2 files)");
        });

        // Both the head buffer and the diff's base text buffer get the language from the path.
        for (path, expected_language) in [("src/main.rs", "Rust"), ("docs/README.md", "Markdown")] {
            let buffer = buffer_for_path(&review_view, path, cx);
            rhs_editor.read_with(cx, |editor, cx| {
                let buffer_id = buffer.read(cx).remote_id();
                let diff = editor
                    .buffer()
                    .read(cx)
                    .diff_for(buffer_id)
                    .expect("every review buffer should have a diff");
                let head_language = buffer.read(cx).language().map(|language| language.name());
                let base_language = diff
                    .read(cx)
                    .base_text_buffer()
                    .read(cx)
                    .language()
                    .map(|language| language.name());
                assert_eq!(head_language, Some(expected_language.into()), "{path}");
                assert_eq!(base_language, Some(expected_language.into()), "{path}");
            });
        }

        let (header_paths, hunks) = rhs_editor.update_in(cx, |editor, window, cx| {
            let snapshot = editor.snapshot(window, cx);
            let multibuffer_snapshot = snapshot.buffer_snapshot();
            let header_paths = snapshot
                .blocks_in_range(DisplayRow(0)..DisplayRow(snapshot.max_point().row().0 + 1))
                .filter_map(|(_, block)| match block {
                    Block::BufferHeader { excerpt, .. } => excerpt
                        .buffer(multibuffer_snapshot)
                        .resolve_file_path(false, cx),
                    _ => None,
                })
                .collect::<Vec<_>>();

            let hunks = multibuffer_snapshot
                .diff_hunks()
                .map(|hunk| {
                    let buffer = editor
                        .buffer()
                        .read(cx)
                        .buffer(hunk.buffer_id)
                        .expect("hunk should belong to a review buffer")
                        .read(cx)
                        .snapshot();
                    let base_text = multibuffer_snapshot
                        .diff_for_buffer_id(hunk.buffer_id)
                        .expect("hunk should belong to a diff")
                        .base_text()
                        .text_for_range(
                            hunk.diff_base_byte_range.start.0..hunk.diff_base_byte_range.end.0,
                        )
                        .collect::<String>();
                    let head_text = buffer
                        .text_for_range(hunk.buffer_range.clone())
                        .collect::<String>();
                    let path = buffer
                        .file()
                        .map(|file| file.path().as_unix_str().to_string());
                    (path, hunk.status().kind, base_text, head_text)
                })
                .collect::<Vec<_>>();

            (header_paths, hunks)
        });

        // Files are in file tree order: directories by name.
        assert_eq!(header_paths, ["docs/README.md", "src/main.rs"]);
        assert_eq!(
            hunks,
            [
                (
                    Some("docs/README.md".to_string()),
                    DiffHunkStatusKind::Added,
                    String::new(),
                    "\nA new paragraph.\n".to_string(),
                ),
                (
                    Some("src/main.rs".to_string()),
                    DiffHunkStatusKind::Modified,
                    "    println!(\"hello\");\n".to_string(),
                    "    println!(\"hello, world\");\n".to_string(),
                ),
            ]
        );
    }

    #[gpui::test]
    async fn test_thread_cards_in_unified_diff(cx: &mut TestAppContext) {
        let contents = ReviewContents {
            files: review_files(),
            unavailable: vec![UnavailableFile {
                path: "assets/logo.png".to_string(),
                old_path: None,
                change: FileChange::Modified,
                reason: "Binary file".to_string(),
            }],
            threads: vec![
                thread("on-head-line", "src/main.rs", Some(2), DiffSide::Right),
                thread("on-deleted-line", "src/main.rs", Some(2), DiffSide::Left),
                thread(
                    "on-unchanged-base-line",
                    "src/main.rs",
                    Some(3),
                    DiffSide::Left,
                ),
                thread("outdated", "src/main.rs", None, DiffSide::Right),
                thread("past-the-end", "src/main.rs", Some(99), DiffSide::Right),
                thread(
                    "on-binary-file",
                    "assets/logo.png",
                    Some(1),
                    DiffSide::Right,
                ),
            ],
            ..ReviewContents::default()
        };
        let (review_view, _, cx) = build_review_view(contents, DiffViewStyle::Unified, cx).await;
        let rhs_editor = rhs_editor(&review_view, cx);

        let block = |thread: &str, cx: &mut VisualTestContext| {
            review_view.read_with(cx, |review_view, _| {
                review_view
                    .thread_block(thread)
                    .unwrap_or_else(|| panic!("thread {thread} should have a card in the diff"))
            })
        };

        let (side, card) = block("on-head-line", cx);
        assert_eq!(side, EditorSide::Right);
        assert_eq!(
            line_next_to_block(&rhs_editor, card, true, cx),
            "    println!(\"hello, world\");",
            "a thread on a head line goes below that line"
        );

        let (side, card) = block("on-deleted-line", cx);
        assert_eq!(side, EditorSide::Right);
        assert_eq!(
            line_next_to_block(&rhs_editor, card, true, cx),
            "    println!(\"hello\");",
            "a thread on a deleted line goes below it, inside the hunk"
        );

        let (side, card) = block("on-unchanged-base-line", cx);
        assert_eq!(side, EditorSide::Right);
        assert_eq!(line_next_to_block(&rhs_editor, card, true, cx), "}");

        let (outdated_block, outdated) = review_view
            .read_with(cx, |review_view, cx| {
                review_view.outdated_threads("src/main.rs", cx)
            })
            .expect("src/main.rs should list its outdated threads");
        assert_eq!(outdated, ["outdated", "past-the-end"]);
        assert_eq!(
            line_next_to_block(&rhs_editor, outdated_block, false, cx),
            "fn main() {",
            "outdated threads are listed above the file's first line"
        );
        review_view.read_with(cx, |review_view, cx| {
            assert_eq!(review_view.thread_block("outdated"), None);
            assert_eq!(review_view.outdated_threads("docs/README.md", cx), None);
            assert_eq!(review_view.unshown_thread_ids(cx), ["on-binary-file"]);
        });
    }

    #[gpui::test]
    async fn test_thread_cards_in_split_diff(cx: &mut TestAppContext) {
        let contents = ReviewContents {
            files: review_files(),
            threads: vec![
                thread("on-head-line", "src/main.rs", Some(2), DiffSide::Right),
                thread("on-deleted-line", "src/main.rs", Some(2), DiffSide::Left),
                thread(
                    "on-added-markdown",
                    "docs/README.md",
                    Some(5),
                    DiffSide::Right,
                ),
            ],
            ..ReviewContents::default()
        };
        let (review_view, _, cx) = build_review_view(contents, DiffViewStyle::Split, cx).await;
        let rhs_editor = rhs_editor(&review_view, cx);
        let lhs_editor = lhs_editor(&review_view, cx);

        let block = |thread: &str, cx: &mut VisualTestContext| {
            review_view.read_with(cx, |review_view, _| {
                review_view
                    .thread_block(thread)
                    .unwrap_or_else(|| panic!("thread {thread} should have a card in the diff"))
            })
        };

        let (side, card) = block("on-head-line", cx);
        assert_eq!(side, EditorSide::Right);
        assert_eq!(
            line_next_to_block(&rhs_editor, card, true, cx),
            "    println!(\"hello, world\");"
        );
        let (side, card) = block("on-added-markdown", cx);
        assert_eq!(side, EditorSide::Right);
        assert_eq!(
            line_next_to_block(&rhs_editor, card, true, cx),
            "A new paragraph."
        );

        let (side, card) = block("on-deleted-line", cx);
        assert_eq!(
            side,
            EditorSide::Left,
            "base-side threads go in the left-hand editor when the diff is split"
        );
        assert_eq!(
            line_next_to_block(&lhs_editor, card, true, cx),
            "    println!(\"hello\");"
        );

        // Unsplitting moves the base-side card into the one remaining editor.
        review_view.update_in(cx, |review_view, window, cx| {
            review_view.editor.update(cx, |editor, cx| {
                editor.toggle_split(&editor::ToggleSplitDiff, window, cx)
            })
        });
        cx.run_until_parked();
        let (side, card) = block("on-deleted-line", cx);
        assert_eq!(side, EditorSide::Right);
        assert_eq!(
            line_next_to_block(&rhs_editor, card, true, cx),
            "    println!(\"hello\");"
        );
        let (side, card) = block("on-head-line", cx);
        assert_eq!(side, EditorSide::Right);
        assert_eq!(
            line_next_to_block(&rhs_editor, card, true, cx),
            "    println!(\"hello, world\");"
        );
    }

    #[gpui::test]
    async fn test_file_tree_and_header(cx: &mut TestAppContext) {
        let header = PullRequestHeader {
            title: "Greet the whole world".to_string(),
            repository: "acme/hello".to_string(),
            number: 7,
            author: Some("octocat".to_string()),
            base_branch: "main".to_string(),
            head_branch: "greet-world".to_string(),
            state: PullRequestState::Open,
            is_draft: false,
            checks: None,
            additions: 3,
            deletions: 1,
        };
        let contents = ReviewContents {
            header: Some(ReviewHeader::PullRequest(header)),
            files: review_files(),
            unavailable: vec![UnavailableFile {
                path: "assets/logo.png".to_string(),
                old_path: None,
                change: FileChange::Added,
                reason: "Binary file".to_string(),
            }],
            viewed: Some(
                [
                    ("src/main.rs".to_string(), FileViewedState::Viewed),
                    ("docs/README.md".to_string(), FileViewedState::Dismissed),
                ]
                .into_iter()
                .collect(),
            ),
            ..ReviewContents::default()
        };
        let (review_view, workspace, cx) =
            build_review_view(contents, DiffViewStyle::Unified, cx).await;

        // The panel follows the active review.
        let panel = workspace.update_in(cx, |_, window, cx| {
            let workspace = cx.entity();
            cx.new(|cx| FileTreePanel::new(&workspace, window, cx))
        });
        workspace.update_in(cx, |workspace, window, cx| {
            workspace.active_pane().update(cx, |pane, cx| {
                pane.activate_item(0, true, true, window, cx);
            });
        });
        cx.run_until_parked();
        assert_eq!(
            panel.read_with(cx, |panel, _| panel.review()),
            Some(review_view.clone())
        );

        review_view.read_with(cx, |review_view, cx| {
            assert_eq!(review_view.tab_content_text(0, cx), "Greet the whole world");
            let tree = review_view.file_tree();
            assert_eq!(tree.summary(), "1 of 3 viewed");
            let rows = tree
                .rows()
                .iter()
                .map(|row| match row {
                    FileTreeRow::Directory { name, depth } => {
                        format!("{}{name}/", "  ".repeat(*depth))
                    }
                    FileTreeRow::File { file, name, depth } => {
                        let file = &tree.files()[*file];
                        format!(
                            "{}{name} {} {:?} {:?}{}",
                            "  ".repeat(*depth),
                            file.change.letter(),
                            file.viewed,
                            file.line_counts,
                            file.unavailable
                                .as_ref()
                                .map(|reason| format!(" ({reason})"))
                                .unwrap_or_default(),
                        )
                    }
                })
                .collect::<Vec<_>>();
            assert_eq!(
                rows,
                [
                    "assets/",
                    "  logo.png A Unviewed None (Binary file)",
                    "docs/",
                    "  README.md M Dismissed Some((2, 0))",
                    "src/",
                    "  main.rs M Viewed Some((1, 1))",
                ]
            );
        });

        // Clicking a file in the tree scrolls the review to it.
        let rhs_editor = rhs_editor(&review_view, cx);
        let main_rs = review_view.read_with(cx, |review_view, _| {
            review_view
                .file_tree()
                .files()
                .iter()
                .position(|file| file.path == "src/main.rs")
                .expect("src/main.rs should be in the tree")
        });
        let scrolled = review_view.update_in(cx, |review_view, window, cx| {
            review_view.scroll_to_file(main_rs, window, cx)
        });
        assert!(scrolled);
        let (path, point) = rhs_editor.update(cx, |editor, cx| {
            let snapshot = editor.buffer().read(cx).snapshot(cx);
            let head = editor
                .selections
                .newest::<Point>(&editor.display_snapshot(cx))
                .head();
            let (buffer, point) = snapshot
                .point_to_buffer_point(head)
                .expect("the cursor should be in a file");
            let path = buffer
                .file()
                .map(|file| file.path().as_unix_str().to_string());
            (path, point)
        });
        assert_eq!(path.as_deref(), Some("src/main.rs"));
        assert_eq!(point, Point::zero());

        let logo_png = review_view.read_with(cx, |review_view, _| {
            review_view
                .file_tree()
                .files()
                .iter()
                .position(|file| file.path == "assets/logo.png")
                .expect("assets/logo.png should be in the tree")
        });
        let scrolled = review_view.update_in(cx, |review_view, window, cx| {
            review_view.scroll_to_file(logo_png, window, cx)
        });
        assert!(
            !scrolled,
            "files the review can't show have nothing to scroll to"
        );
    }

    /// Records what the review asks of the app, and answers every write with `fail_with` if
    /// set, or success.
    #[derive(Default)]
    pub(crate) struct FakeBackend {
        pub(crate) calls: Mutex<Vec<Call>>,
        pub(crate) saved: Mutex<Vec<PendingReview>>,
        pub(crate) fail_with: Mutex<Option<String>>,
        pub(crate) threads: Mutex<Vec<ReviewThread>>,
    }

    #[derive(Clone, Debug, PartialEq)]
    pub(crate) enum Call {
        Reply(String, String),
        Resolve(String, bool),
        Update(String, String),
        Viewed(Vec<(String, bool)>),
        Submit(ReviewSubmission),
        Comment(String),
        Reload,
    }

    impl FakeBackend {
        fn write(&self, call: Call) -> Task<Result<()>> {
            self.calls.lock().push(call);
            Task::ready(match self.fail_with.lock().clone() {
                Some(error) => Err(anyhow::anyhow!(error)),
                None => Ok(()),
            })
        }

        pub(crate) fn writes(&self) -> Vec<Call> {
            self.calls
                .lock()
                .iter()
                .filter(|call| **call != Call::Reload)
                .cloned()
                .collect()
        }

        pub(crate) fn last_saved(&self) -> PendingReview {
            self.saved
                .lock()
                .last()
                .cloned()
                .expect("the pending review should have been saved")
        }
    }

    impl ReviewBackend for FakeBackend {
        fn save_pending_review(&self, review: &PendingReview, _: &mut App) {
            self.saved.lock().push(review.clone());
        }

        fn reply_to_thread(
            &self,
            thread_id: String,
            body: String,
            _: &mut App,
        ) -> Task<Result<()>> {
            self.write(Call::Reply(thread_id, body))
        }

        fn set_thread_resolution(
            &self,
            thread_id: String,
            resolved: bool,
            _: &mut App,
        ) -> Task<Result<()>> {
            self.write(Call::Resolve(thread_id, resolved))
        }

        fn update_comment(
            &self,
            comment_id: String,
            body: String,
            _: &mut App,
        ) -> Task<Result<()>> {
            self.write(Call::Update(comment_id, body))
        }

        fn set_files_viewed(&self, files: Vec<(String, bool)>, _: &mut App) -> Task<Result<()>> {
            self.write(Call::Viewed(files))
        }

        fn submit_review(&self, submission: ReviewSubmission, _: &mut App) -> Task<Result<()>> {
            self.write(Call::Submit(submission))
        }

        fn post_comment(&self, body: String, _: &mut App) -> Task<Result<()>> {
            self.write(Call::Comment(body))
        }

        fn reload_threads(&self, _: &mut App) -> Task<Result<Vec<ReviewThread>>> {
            self.calls.lock().push(Call::Reload);
            Task::ready(Ok(self.threads.lock().clone()))
        }
    }

    /// The forge's hunks for `review_files`: all of src/main.rs, and README.md's added lines
    /// without context (as with `git diff -U0`).
    fn commentable_review_files() -> Vec<ReviewFileContents> {
        use HunkLine::{Added, Context, Deleted};
        let mut files = review_files();
        files[0].commentable = Some(CommentableLines::from_hunks([(
            1,
            1,
            vec![Context, Deleted, Added, Context],
        )]));
        files[1].commentable = Some(CommentableLines::from_hunks([(3, 4, vec![Added, Added])]));
        files
    }

    fn writable_contents(backend: &Arc<FakeBackend>) -> ReviewContents {
        ReviewContents {
            files: commentable_review_files(),
            backend: Some(backend.clone()),
            permissions: ReviewPermissions::all(),
            ..ReviewContents::default()
        }
    }

    /// Puts the cursor (or a selection) on the diff lines with these texts, in the right-hand
    /// editor, and focuses it.
    pub(crate) fn select_lines(
        review_view: &Entity<ReviewView>,
        first: &str,
        last: &str,
        cx: &mut VisualTestContext,
    ) {
        let editor = rhs_editor(review_view, cx);
        editor.update_in(cx, |editor, window, cx| {
            let text = editor.buffer().read(cx).snapshot(cx).text();
            let row = |needle: &str| {
                text.lines()
                    .position(|line| line == needle)
                    .unwrap_or_else(|| panic!("no line {needle:?} in the diff:\n{text}"))
                    as u32
            };
            let (start, end) = (row(first), row(last));
            let end = Point::new(
                end,
                text.lines().nth(end as usize).unwrap_or("").len() as u32,
            );
            editor.change_selections(SelectionEffects::no_scroll(), window, cx, |selections| {
                selections.select_ranges([Point::new(start, 0)..end])
            });
            window.focus(&editor.focus_handle(cx), cx);
        });
        cx.run_until_parked();
    }

    pub(crate) fn drafts(
        review_view: &Entity<ReviewView>,
        cx: &mut VisualTestContext,
    ) -> Vec<DraftComment> {
        review_view.read_with(cx, |review_view, cx| {
            review_view
                .draft_cards()
                .map(|card| card.read(cx).draft(cx))
                .collect()
        })
    }

    fn only_draft_card(
        review_view: &Entity<ReviewView>,
        cx: &mut VisualTestContext,
    ) -> Entity<DraftCard> {
        review_view.read_with(cx, |review_view, _| {
            let cards = review_view.draft_cards().cloned().collect::<Vec<_>>();
            assert_eq!(cards.len(), 1, "expected one draft card");
            cards[0].clone()
        })
    }

    #[gpui::test]
    async fn test_composing_a_line_comment(cx: &mut TestAppContext) {
        let backend = Arc::new(FakeBackend::default());
        let (review_view, _, cx) =
            build_review_view(writable_contents(&backend), DiffViewStyle::Unified, cx).await;

        // `c` on an added line opens a composer below it, with the focus in it.
        select_lines(
            &review_view,
            "    println!(\"hello, world\");",
            "    println!(\"hello, world\");",
            cx,
        );
        cx.simulate_keystrokes("c");
        let card = only_draft_card(&review_view, cx);
        let composer = card.read_with(cx, |card, _| {
            assert!(card.is_editing());
            card.composer_editor().cloned().expect("a composer")
        });
        cx.update(|window, cx| assert!(composer.focus_handle(cx).is_focused(window)));
        let draft_id = drafts(&review_view, cx)[0].id.clone();
        let rhs_editor = rhs_editor(&review_view, cx);
        let (side, block) = review_view
            .read_with(cx, |review_view, _| review_view.draft_block(&draft_id))
            .expect("the composer should be in the diff");
        assert_eq!(side, EditorSide::Right);
        assert_eq!(
            line_next_to_block(&rhs_editor, block, true, cx),
            "    println!(\"hello, world\");"
        );

        // Typing `c` in the composer types it rather than starting another comment.
        cx.simulate_input("Say ");
        cx.simulate_keystrokes("c");
        cx.simulate_input("hello");
        assert_eq!(drafts(&review_view, cx).len(), 1);
        cx.executor().advance_clock(SAVE_DELAY * 2);
        cx.run_until_parked();
        let saved = backend.last_saved();
        assert_eq!(saved.comments.len(), 1);
        assert_eq!(saved.comments[0].body, "Say chello");
        assert!(!saved.comments[0].added, "not added until confirmed");

        // Confirming adds it to the pending review and gives the diff the focus back.
        cx.simulate_keystrokes("secondary-enter");
        let saved = backend.last_saved();
        assert_eq!(
            saved.comments,
            vec![DraftComment {
                id: draft_id,
                path: "src/main.rs".to_string(),
                old_path: None,
                position: CommentPosition::Added { new_line: 2 },
                start_line: None,
                body: "Say chello".to_string(),
                added: true,
            }]
        );
        cx.update(|window, cx| assert!(rhs_editor.focus_handle(cx).is_focused(window)));
        review_view.read_with(cx, |review_view, cx| {
            assert_eq!(review_view.added_draft_count(cx), 1);
        });
        assert!(backend.writes().is_empty(), "nothing goes to the forge yet");

        // A comment on a deleted line goes on the base side; escape abandons an empty one.
        select_lines(
            &review_view,
            "    println!(\"hello\");",
            "    println!(\"hello\");",
            cx,
        );
        cx.simulate_keystrokes("c");
        let new = drafts(&review_view, cx).pop().expect("a second draft");
        assert_eq!(new.position, CommentPosition::Deleted { old_line: 2 });
        cx.simulate_keystrokes("escape");
        assert_eq!(drafts(&review_view, cx).len(), 1);

        // A selection of several lines is anchored on its last line.
        select_lines(&review_view, "fn main() {", "}", cx);
        cx.simulate_keystrokes("c");
        let ranged = drafts(&review_view, cx).pop().expect("a ranged draft");
        assert_eq!(
            ranged.position,
            CommentPosition::Context {
                old_line: 3,
                new_line: 3,
                side: DiffSide::Right
            }
        );
        assert_eq!(ranged.start_line, Some(1));
        assert_eq!(ranged.line_label(), "Lines 1–3");
    }

    #[gpui::test]
    async fn test_lines_outside_the_forges_hunks_explain_why(cx: &mut TestAppContext) {
        let backend = Arc::new(FakeBackend::default());
        let (review_view, _, cx) =
            build_review_view(writable_contents(&backend), DiffViewStyle::Unified, cx).await;

        select_lines(&review_view, "Intro.", "Intro.", cx);
        cx.simulate_keystrokes("c");
        assert!(drafts(&review_view, cx).is_empty());
        let notice = review_view
            .read_with(cx, |review_view, _| review_view.notice().cloned())
            .expect("a notice should say why");
        assert_eq!(
            notice,
            "The forge only takes comments on lines in the diff's hunks. docs/README.md:3 is 1 \
             line from the nearest one."
        );

        // Moving on clears it, and a line in the hunk takes a comment.
        select_lines(&review_view, "A new paragraph.", "A new paragraph.", cx);
        assert_eq!(
            review_view.read_with(cx, |review_view, _| review_view.notice().cloned()),
            None
        );
        cx.simulate_keystrokes("c");
        assert_eq!(
            drafts(&review_view, cx)[0].position,
            CommentPosition::Added { new_line: 5 }
        );

        // Without a backend, the review is read-only.
        let contents = ReviewContents {
            files: commentable_review_files(),
            ..ReviewContents::default()
        };
        let (read_only, _, cx) = build_review_view(contents, DiffViewStyle::Unified, cx).await;
        select_lines(&read_only, "A new paragraph.", "A new paragraph.", cx);
        read_only.update_in(cx, |review_view, window, cx| {
            review_view.add_comment(&AddComment, window, cx)
        });
        assert_eq!(
            read_only.read_with(cx, |review_view, _| review_view.notice().cloned()),
            Some("This review is read-only.".into())
        );
    }

    #[gpui::test]
    async fn test_drafts_and_unsent_text_are_restored(cx: &mut TestAppContext) {
        let backend = Arc::new(FakeBackend::default());
        let draft = |id: &str, path: &str, position, body: &str, added| DraftComment {
            id: id.to_string(),
            path: path.to_string(),
            old_path: None,
            position,
            start_line: None,
            body: body.to_string(),
            added,
        };
        let pending = PendingReview {
            comments: vec![
                draft(
                    "added",
                    "docs/README.md",
                    CommentPosition::Added { new_line: 4 },
                    "Blank line?",
                    true,
                ),
                draft(
                    "unfinished",
                    "src/main.rs",
                    CommentPosition::Added { new_line: 2 },
                    "Half a tho",
                    false,
                ),
                // The pull request changed: line 1 isn't in a hunk any more.
                draft(
                    "stranded",
                    "docs/README.md",
                    CommentPosition::Added { new_line: 1 },
                    "Old",
                    true,
                ),
                draft(
                    "gone",
                    "deleted/file.rs",
                    CommentPosition::Added { new_line: 1 },
                    "Gone",
                    true,
                ),
            ],
            summary: "Mostly good.".to_string(),
            verdict: Some(ReviewVerdict::RequestChanges),
            replies: [("on-head-line".to_string(), "I'd keep it.".to_string())]
                .into_iter()
                .collect(),
            edits: Default::default(),
        };
        let contents = ReviewContents {
            threads: vec![thread(
                "on-head-line",
                "src/main.rs",
                Some(2),
                DiffSide::Right,
            )],
            pending: pending.clone(),
            ..writable_contents(&backend)
        };
        let (review_view, _, cx) = build_review_view(contents, DiffViewStyle::Unified, cx).await;
        let rhs_editor = rhs_editor(&review_view, cx);

        let restored = drafts(&review_view, cx);
        assert_eq!(
            restored
                .iter()
                .map(|draft| draft.id.as_str())
                .collect::<Vec<_>>(),
            ["added", "unfinished"]
        );
        review_view.read_with(cx, |review_view, cx| {
            assert_eq!(review_view.added_draft_count(cx), 1);
            assert_eq!(
                review_view
                    .stranded_drafts()
                    .iter()
                    .map(|draft| draft.id.as_str())
                    .collect::<Vec<_>>(),
                ["stranded", "gone"]
            );
            let card = review_view
                .thread_card("on-head-line", cx)
                .expect("the thread's card");
            assert_eq!(
                card.read(cx).unsent_reply(cx).as_deref(),
                Some("I'd keep it.")
            );
            assert_eq!(
                review_view.summary_editor().read(cx).text(cx),
                "Mostly good."
            );
            // Nothing was lost on the way in.
            assert_eq!(review_view.pending_review(cx), pending);
        });
        let (_, block) = review_view
            .read_with(cx, |review_view, _| review_view.draft_block("added"))
            .expect("the added draft should be in the diff");
        assert_eq!(line_next_to_block(&rhs_editor, block, true, cx), "");
        let unfinished = review_view.read_with(cx, |review_view, cx| {
            review_view
                .draft_cards()
                .find(|card| card.read(cx).draft(cx).id == "unfinished")
                .cloned()
                .expect("the unfinished draft's card")
        });
        assert!(unfinished.read_with(cx, |card, _| card.is_editing()));
    }

    #[gpui::test]
    async fn test_submitting_the_pending_review(cx: &mut TestAppContext) {
        let backend = Arc::new(FakeBackend::default());
        let added = DraftComment {
            id: "d1".to_string(),
            path: "docs/README.md".to_string(),
            old_path: None,
            position: CommentPosition::Added { new_line: 5 },
            start_line: None,
            body: "Nice paragraph.".to_string(),
            added: true,
        };
        let unfinished = DraftComment {
            id: "d2".to_string(),
            body: "Not done".to_string(),
            added: false,
            position: CommentPosition::Added { new_line: 4 },
            ..added.clone()
        };
        let contents = ReviewContents {
            pending: PendingReview {
                comments: vec![added.clone(), unfinished.clone()],
                ..PendingReview::default()
            },
            ..writable_contents(&backend)
        };
        let (review_view, _, cx) = build_review_view(contents, DiffViewStyle::Unified, cx).await;

        review_view.update_in(cx, |review_view, window, cx| {
            review_view.open_submit_form(&SubmitReview, window, cx);
            assert!(review_view.is_submit_form_open());
            review_view.set_verdict(ReviewVerdict::RequestChanges, cx);
        });
        cx.simulate_input("Please fix the typo.");

        // The forge refuses: the error shows, and nothing written is lost.
        *backend.fail_with.lock() =
            Some("GitHub: User can only have one pending review per pull request".to_string());
        cx.simulate_keystrokes("secondary-enter");
        let submission = ReviewSubmission {
            verdict: ReviewVerdict::RequestChanges,
            body: "Please fix the typo.".to_string(),
            comments: vec![added.clone()],
            quotes: [(
                "d1".to_string(),
                QuotedCode {
                    language: Some("markdown".to_string()),
                    text: "A new paragraph.".to_string(),
                },
            )]
            .into_iter()
            .collect(),
        };
        assert_eq!(backend.writes(), vec![Call::Submit(submission.clone())]);
        review_view.read_with(cx, |review_view, cx| {
            assert_eq!(
                review_view.submit_error().map(|error| error.to_string()),
                Some("GitHub: User can only have one pending review per pull request".to_string())
            );
            assert!(review_view.is_submit_form_open());
            assert_eq!(review_view.added_draft_count(cx), 1);
            assert_eq!(
                review_view.summary_editor().read(cx).text(cx),
                "Please fix the typo."
            );
        });

        // It goes through: the submitted comment is gone, the unfinished one stays.
        *backend.fail_with.lock() = None;
        review_view.update_in(cx, |review_view, window, cx| review_view.submit(window, cx));
        cx.run_until_parked();
        assert_eq!(
            backend.writes(),
            vec![Call::Submit(submission.clone()), Call::Submit(submission)]
        );
        assert!(backend.calls.lock().contains(&Call::Reload));
        review_view.read_with(cx, |review_view, cx| {
            assert!(!review_view.is_submit_form_open());
            assert_eq!(review_view.submit_error(), None);
            assert_eq!(
                review_view.notice().map(|notice| notice.to_string()),
                Some("Your review was submitted.".to_string())
            );
            assert_eq!(review_view.summary_editor().read(cx).text(cx), "");
        });
        let saved = backend.last_saved();
        assert_eq!(saved.comments, vec![unfinished]);
        assert_eq!(saved.summary, "");

        // A comment on the conversation instead of a review.
        review_view.update_in(cx, |review_view, window, cx| {
            review_view.open_submit_form(&SubmitReview, window, cx);
            review_view.set_submit_mode(SubmitMode::Comment, cx);
        });
        cx.simulate_input("Thanks!");
        cx.simulate_keystrokes("secondary-enter");
        assert_eq!(
            backend.writes().last(),
            Some(&Call::Comment("Thanks!".to_string()))
        );
    }

    #[gpui::test]
    async fn test_replying_resolving_and_editing_threads(cx: &mut TestAppContext) {
        let backend = Arc::new(FakeBackend::default());
        let mut own = thread("mine", "src/main.rs", Some(2), DiffSide::Right);
        own.comments[0].can_edit = true;
        let contents = ReviewContents {
            threads: vec![
                thread("on-head-line", "src/main.rs", Some(2), DiffSide::Right),
                own.clone(),
            ],
            ..writable_contents(&backend)
        };
        let (review_view, _, cx) = build_review_view(contents, DiffViewStyle::Unified, cx).await;
        let card = review_view
            .read_with(cx, |review_view, cx| {
                review_view.thread_card("on-head-line", cx)
            })
            .expect("the thread's card");

        // A failed reply keeps its text and shows why.
        card.update_in(cx, |card, window, cx| card.start_reply(window, cx));
        cx.simulate_input("Fixed in the next push.");
        *backend.fail_with.lock() = Some("Resource not accessible by integration".into());
        cx.simulate_keystrokes("secondary-enter");
        assert_eq!(
            backend.writes(),
            vec![Call::Reply(
                "on-head-line".to_string(),
                "Fixed in the next push.".to_string()
            )]
        );
        card.read_with(cx, |card, cx| {
            assert_eq!(
                card.error().map(|error| error.to_string()),
                Some("Resource not accessible by integration".to_string())
            );
            assert_eq!(
                card.unsent_reply(cx).as_deref(),
                Some("Fixed in the next push.")
            );
        });
        cx.executor().advance_clock(SAVE_DELAY * 2);
        cx.run_until_parked();
        assert_eq!(
            backend
                .last_saved()
                .replies
                .get("on-head-line")
                .map(String::as_str),
            Some("Fixed in the next push.")
        );

        // Once it goes through, the threads are loaded again and the card shows the reply.
        *backend.fail_with.lock() = None;
        let mut replied = thread("on-head-line", "src/main.rs", Some(2), DiffSide::Right);
        replied.comments.push(ThreadComment {
            id: "reply".to_string(),
            author: Some("me".to_string()),
            body: "Fixed in the next push.".to_string(),
            created_at: None,
            edited: false,
            can_edit: true,
        });
        replied.comment_count = 2;
        *backend.threads.lock() = vec![replied, own.clone()];
        card.update_in(cx, |card, window, cx| card.send_reply(window, cx));
        cx.run_until_parked();
        assert!(backend.calls.lock().contains(&Call::Reload));
        card.read_with(cx, |card, cx| {
            assert_eq!(card.unsent_reply(cx), None);
            assert_eq!(card.error(), None);
            assert_eq!(card.thread().comments.len(), 2);
        });
        assert!(backend.last_saved().replies.is_empty());

        // Resolving.
        card.update(cx, |card, cx| card.set_resolved(true, cx));
        cx.run_until_parked();
        assert_eq!(
            backend.writes().last(),
            Some(&Call::Resolve("on-head-line".to_string(), true))
        );

        // Editing one's own comment.
        let own_card = review_view
            .read_with(cx, |review_view, cx| review_view.thread_card("mine", cx))
            .expect("the card of the viewer's thread");
        own_card.update_in(cx, |card, window, cx| card.start_edit(0, window, cx));
        let editor = own_card
            .read_with(cx, |card, _| card.edit_editor().cloned())
            .expect("an edit composer");
        editor.update_in(cx, |editor, window, cx| {
            editor.set_text("Reworded.", window, cx)
        });
        cx.simulate_keystrokes("secondary-enter");
        assert_eq!(
            backend.writes().last(),
            Some(&Call::Update(
                "mine-comment".to_string(),
                "Reworded.".to_string()
            ))
        );
        own_card.read_with(cx, |card, _| assert!(card.edit_editor().is_none()));
    }

    #[gpui::test]
    async fn test_marking_files_viewed(cx: &mut TestAppContext) {
        let backend = Arc::new(FakeBackend::default());
        let contents = ReviewContents {
            viewed: Some(Default::default()),
            ..writable_contents(&backend)
        };
        let (review_view, _, cx) = build_review_view(contents, DiffViewStyle::Unified, cx).await;
        let main_rs = review_view.read_with(cx, |review_view, _| {
            review_view
                .file_tree()
                .files()
                .iter()
                .position(|file| file.path == "src/main.rs")
                .expect("src/main.rs")
        });
        let viewed = |cx: &mut VisualTestContext| {
            review_view.read_with(cx, |review_view, _| {
                review_view.file_tree().files()[main_rs].viewed
            })
        };
        assert!(review_view.read_with(cx, |review_view, _| review_view.can_mark_viewed()));

        review_view.update(cx, |review_view, cx| review_view.toggle_viewed(main_rs, cx));
        cx.run_until_parked();
        assert_eq!(viewed(cx), FileViewedState::Viewed);
        assert_eq!(
            backend.writes(),
            vec![Call::Viewed(vec![("src/main.rs".to_string(), true)])]
        );

        // A refusal puts the checkbox back and says why.
        *backend.fail_with.lock() = Some("rate limited".into());
        review_view.update(cx, |review_view, cx| review_view.toggle_viewed(main_rs, cx));
        cx.run_until_parked();
        assert_eq!(viewed(cx), FileViewedState::Viewed);
        assert_eq!(
            review_view.read_with(cx, |review_view, _| review_view.viewed_error().cloned()),
            Some("Couldn't mark src/main.rs not viewed: rate limited".into())
        );

        // Without the forge's permission, the checkboxes are display only.
        let contents = ReviewContents {
            viewed: Some(Default::default()),
            permissions: ReviewPermissions {
                blocked: Some("Pair again".into()),
                ..ReviewPermissions::all()
            },
            ..writable_contents(&backend)
        };
        let (blocked, _, cx) = build_review_view(contents, DiffViewStyle::Unified, cx).await;
        assert!(!blocked.read_with(cx, |review_view, _| review_view.can_mark_viewed()));
    }
}

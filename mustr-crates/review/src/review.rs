//! mustr's review view: a read-only multibuffer of diffs whose base and head texts are held in
//! memory (for example, fetched from a T3 Code server) rather than read from a local checkout,
//! with the pull request's header, its review threads as cards in the diff, and a file tree.

mod comment_html;
mod file_tree;
mod file_tree_panel;
mod header;
mod threads;
mod toolbar;

pub use comment_html::comment_markdown;
pub use file_tree::{FileChange, FileTreeFile, FileTreeModel, FileTreeRow, FileViewedState};
pub use file_tree_panel::{FileTreePanel, ToggleFileTree};
pub use header::{ChecksState, ChecksSummary, PullRequestHeader, PullRequestState};
pub use threads::{DiffSide, EditorSide, OutdatedThreads, ReviewThread, ThreadCard, ThreadComment};

use anyhow::Result;
use buffer_diff::BufferDiff;
use collections::{HashMap, HashSet};
use editor::{
    Editor, EditorEvent, EditorSettings, HiddenDiffHunkRenderer, MultiBuffer, SelectionEffects,
    SplittableEditor,
    display_map::{BlockPlacement, CustomBlockId, DisplayRow, ToDisplayPoint as _},
    multibuffer_context_lines,
    scroll::Autoscroll,
};
use gpui::{
    AnyElement, App, AppContext as _, AsyncWindowContext, Context, Entity, EventEmitter,
    FocusHandle, Focusable, Font, IntoElement, ParentElement as _, Render, SharedString,
    Styled as _, Subscription, Task, WeakEntity, Window, px,
};
use language::{
    Buffer, Capability, DiskState, File, HighlightedText, LanguageRegistry, LineEnding,
    OffsetRangeExt as _, Point, ReplicaId, Rope, TextBuffer,
};
use multi_buffer::PathKey;
use mustr_ui::tokens;
use project::{Project, ProjectPath, WorktreeId};
use settings::Settings as _;
use std::{
    any::{Any, TypeId},
    mem,
    ops::Range,
    path::PathBuf,
    sync::Arc,
};
use threads::{card_block, estimated_card_lines, thread_anchor, thread_head_row};
use ui::{Color, Icon, IconName, IconSize, Label, LabelCommon as _, LabelSize, prelude::*};
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
    /// The pull request, when the review is of one and its details could be loaded.
    pub header: Option<PullRequestHeader>,
    pub files: Vec<ReviewFileContents>,
    pub unavailable: Vec<UnavailableFile>,
    pub threads: Vec<ReviewThread>,
    /// The viewer's viewed marks by path, or `None` when the forge keeps none. Files missing from
    /// the map are unviewed.
    pub viewed: Option<std::collections::HashMap<String, FileViewedState>>,
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
}

/// The parts of [`ReviewContents`] the view keeps once the buffers are built.
struct LoadedReview {
    header: Option<PullRequestHeader>,
    entries: Vec<ReviewEntry>,
    unavailable: Vec<UnavailableFile>,
    threads: Vec<ReviewThread>,
    viewed: Option<std::collections::HashMap<String, FileViewedState>>,
}

/// A file the review shows. Its index in [`ReviewView::files`] is its index in the file tree.
struct ShownFile {
    path_key: PathKey,
    buffer: Entity<Buffer>,
    diff: Entity<BufferDiff>,
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
    /// File index to the list of threads the diff can't place, shown above the file.
    outdated: HashMap<usize, (Entity<OutdatedThreads>, CustomBlockId)>,
}

/// Shows every file of a review in one diff multibuffer, in file tree order.
///
/// Modeled on `MultiDiffView` (`crates/git_ui/src/multi_diff_view.rs`) and `CommitView`
/// (`crates/git_ui/src/commit_view.rs`).
pub struct ReviewView {
    editor: Entity<SplittableEditor>,
    header: Option<PullRequestHeader>,
    files: Vec<ShownFile>,
    file_tree: FileTreeModel,
    /// Cards of threads on shown files, with the file's index.
    thread_cards: Vec<(usize, Entity<ThreadCard>)>,
    /// Cards of threads on files the review doesn't show, listed in the header.
    unshown_thread_cards: Vec<Entity<ThreadCard>>,
    unshown_threads_expanded: bool,
    blocks: ThreadBlocks,
    toolbar: Entity<toolbar::ReviewToolbar>,
    _subscriptions: Vec<Subscription>,
}

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
                unavailable,
                threads,
                viewed,
            } = contents;
            let mut entries = Vec::with_capacity(files.len());
            for file in files {
                entries.push(load_entry(file, &language_registry, cx).await?);
            }
            let loaded = LoadedReview {
                header,
                entries,
                unavailable,
                threads,
                viewed,
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
            entries,
            unavailable,
            threads,
            viewed,
        } = loaded;
        let language_registry = project.read(cx).languages().clone();

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
                        buffer: entry.buffer,
                        diff: entry.diff,
                    }
                })
                .collect::<Vec<_>>()
        });

        let thread_cards = threads_by_file
            .into_iter()
            .enumerate()
            .flat_map(|(file, threads)| threads.into_iter().map(move |thread| (file, thread)))
            .map(|(file, thread)| {
                let language_registry = language_registry.clone();
                let card = cx.new(|cx| ThreadCard::new(thread, None, Some(language_registry), cx));
                (file, card)
            })
            .collect();
        let unshown_thread_cards = unshown_threads
            .into_iter()
            .map(|thread| {
                let location = Some(thread_location(&thread));
                let language_registry = language_registry.clone();
                cx.new(|cx| ThreadCard::new(thread, location, Some(language_registry), cx))
            })
            .collect();

        let subscriptions = vec![
            cx.subscribe(&editor, |_, _, event: &EditorEvent, cx| {
                if event == &(EditorEvent::SelectionsChanged { local: true }) {
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
        ];

        let toolbar = cx.new(|cx| toolbar::ReviewToolbar::new(&editor, cx));
        let mut this = Self {
            editor,
            header,
            files,
            file_tree,
            thread_cards,
            unshown_thread_cards,
            unshown_threads_expanded: false,
            blocks: ThreadBlocks::default(),
            toolbar,
            _subscriptions: subscriptions,
        };
        this.sync_thread_blocks(cx);
        this
    }

    pub fn header(&self) -> Option<&PullRequestHeader> {
        self.header.as_ref()
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

    /// Replaces every card block, placing each thread for the current split or unified diff.
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

        enum BlockOwner {
            Thread(String),
            Outdated(usize, Entity<OutdatedThreads>),
        }
        let mut rhs_blocks = Vec::new();
        let mut rhs_owners = Vec::new();
        let mut lhs_blocks = Vec::new();
        let mut lhs_threads = Vec::new();
        let mut outdated_cards: Vec<Vec<Entity<ThreadCard>>> = vec![Vec::new(); self.files.len()];
        for (file, card) in &self.thread_cards {
            let Some(shown) = self.files.get(*file) else {
                continue;
            };
            let (thread, expanded) = {
                let card = card.read(cx);
                (card.thread().clone(), card.is_expanded())
            };
            let lines = estimated_card_lines(&thread, expanded);
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
                    lhs_threads.push(thread.id.clone());
                }
                None => outdated_cards[*file].push(card.clone()),
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
            for (thread, id) in lhs_threads.into_iter().zip(&lhs_ids) {
                self.blocks.placed.insert(thread, (EditorSide::Left, *id));
            }
            self.blocks.lhs = Some((lhs_editor.downgrade(), lhs_ids.into_iter().collect()));
        }
        self.blocks.synced_split = Some(lhs_editor.is_some());
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
                            .max_h(px(320.))
                            .overflow_y_scroll()
                            .gap_2()
                            .p_2()
                            .rounded(tokens::CORNER_RADIUS)
                            .border(tokens::BORDER_WIDTH)
                            .border_color(colors.border_variant)
                            .children(self.unshown_thread_cards.iter().cloned()),
                    )
                })
                .into_any_element(),
        )
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
    language_registry: &Arc<LanguageRegistry>,
    cx: &mut AsyncWindowContext,
) -> Result<ReviewEntry> {
    let change = contents.change();
    let file = Arc::new(ReviewFile {
        path: contents.path.clone(),
        is_deleted: contents.head_text.is_none(),
    }) as Arc<dyn File>;
    let buffer = build_buffer(
        contents.head_text.unwrap_or_default(),
        file,
        language_registry,
        cx,
    )
    .await?;
    let diff = build_buffer_diff(contents.base_text, &buffer, language_registry, cx).await?;
    Ok(ReviewEntry {
        path: contents.path,
        old_path: contents.old_path,
        change,
        buffer,
        diff,
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
    let snapshot = entry.buffer.read(cx).snapshot();
    let diff_snapshot = entry.diff.read(cx).snapshot(cx);
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
            Some(header) => Some(format!("{}\n{}", header.title, header.reference()).into()),
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
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let unshown_threads = self.render_unshown_threads(cx);
        let colors = cx.theme().colors();
        v_flex()
            .size_full()
            .map(|this| match &self.header {
                Some(header) => this.child(header::render_header(header, unshown_threads, cx)),
                None => this.when_some(unshown_threads, |this, unshown_threads| {
                    this.child(
                        div()
                            .flex_none()
                            .px_3()
                            .py_2()
                            .border_b(tokens::BORDER_WIDTH)
                            .border_color(colors.border)
                            .child(unshown_threads),
                    )
                }),
            })
            .child(self.toolbar.clone())
            .child(div().flex_1().min_h_0().child(self.editor.clone()))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use buffer_diff::DiffHunkStatusKind;
    use editor::display_map::Block;
    use gpui::{TestAppContext, VisualTestContext};
    use language::{markdown_lang, rust_lang};
    use project::FakeFs;
    use settings::{DiffViewStyle, SettingsStore};
    use workspace::MultiWorkspace;

    const RUST_BASE: &str = "fn main() {\n    println!(\"hello\");\n}\n";
    const RUST_HEAD: &str = "fn main() {\n    println!(\"hello, world\");\n}\n";
    const MARKDOWN_BASE: &str = "# Title\n\nIntro.\n";
    const MARKDOWN_HEAD: &str = "# Title\n\nIntro.\n\nA new paragraph.\n";

    fn review_files() -> Vec<ReviewFileContents> {
        vec![
            ReviewFileContents {
                path: rel_path("src/main.rs"),
                old_path: None,
                base_text: Some(RUST_BASE.to_string()),
                head_text: Some(RUST_HEAD.to_string()),
            },
            ReviewFileContents {
                path: rel_path("docs/README.md"),
                old_path: None,
                base_text: Some(MARKDOWN_BASE.to_string()),
                head_text: Some(MARKDOWN_HEAD.to_string()),
            },
        ]
    }

    fn rel_path(path: &str) -> Arc<RelPath> {
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
            comments: vec![ThreadComment {
                id: format!("{id}-comment"),
                author: Some("octocat".to_string()),
                body: format!("A comment on thread {id}."),
                created_at: None,
                edited: false,
            }],
            comment_count: 1,
        }
    }

    // Modeled on `test_init` in `crates/git_ui/src/multi_diff_view.rs`, but with a project that
    // has no worktrees, as when reviewing a pull request without a local checkout.
    async fn build_review_view(
        contents: ReviewContents,
        diff_view_style: DiffViewStyle,
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
        });

        let fs = FakeFs::new(cx.executor());
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

    fn rhs_editor(review_view: &Entity<ReviewView>, cx: &mut VisualTestContext) -> Entity<Editor> {
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

    fn buffer_for_path(
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
    fn line_next_to_block(
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
            header: Some(header),
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
}

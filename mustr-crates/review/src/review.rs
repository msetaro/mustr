//! mustr's review view: a read-only multibuffer of diffs whose base and head texts are held in
//! memory (for example, fetched from a T3 Code server) rather than read from a local checkout.

use anyhow::Result;
use buffer_diff::BufferDiff;
use editor::{
    Editor, EditorEvent, EditorSettings, HiddenDiffHunkRenderer, MultiBuffer, SplittableEditor,
    multibuffer_context_lines,
};
use gpui::{
    App, AppContext as _, AsyncWindowContext, Context, Entity, EventEmitter, FocusHandle,
    Focusable, Font, IntoElement, Render, SharedString, Subscription, Task, Window,
};
use language::{
    Buffer, Capability, DiskState, File, HighlightedText, LanguageRegistry, LineEnding,
    OffsetRangeExt as _, Point, ReplicaId, Rope, TextBuffer,
};
use multi_buffer::PathKey;
use project::{Project, ProjectPath, WorktreeId};
use settings::Settings as _;
use std::{
    any::{Any, TypeId},
    ops::Range,
    path::PathBuf,
    sync::Arc,
};
use ui::{Color, Icon, IconName};
use util::{ResultExt as _, paths::PathStyle, rel_path::RelPath};
use workspace::{
    Item, ItemHandle as _, ItemNavHistory, ToolbarItemLocation, Workspace, item::ItemEvent,
    searchable::SearchableItemHandle,
};

/// One changed file in a review, with both sides of the diff held in memory.
pub struct ReviewFileContents {
    pub path: Arc<RelPath>,
    /// `None` when the file was added.
    pub base_text: Option<String>,
    /// `None` when the file was deleted.
    pub head_text: Option<String>,
}

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
    buffer: Entity<Buffer>,
    diff: Entity<BufferDiff>,
}

/// Shows every file of a review in one diff multibuffer, in the order the files were given.
///
/// Modeled on `MultiDiffView` (`crates/git_ui/src/multi_diff_view.rs`) and `CommitView`
/// (`crates/git_ui/src/commit_view.rs`).
pub struct ReviewView {
    editor: Entity<SplittableEditor>,
    file_count: usize,
    _editor_event_subscription: Subscription,
}

impl ReviewView {
    pub fn open(
        files: Vec<ReviewFileContents>,
        workspace: &Workspace,
        window: &mut Window,
        cx: &mut App,
    ) -> Task<Result<Entity<Self>>> {
        let project = workspace.project().clone();
        let language_registry = project.read(cx).languages().clone();
        let workspace = workspace.weak_handle();

        window.spawn(cx, async move |cx| {
            let mut entries = Vec::with_capacity(files.len());
            for file in files {
                entries.push(load_entry(file, &language_registry, cx).await?);
            }

            workspace.update_in(cx, |workspace, window, cx| {
                let workspace_handle = cx.entity();
                let review_view =
                    cx.new(|cx| Self::new(entries, project, workspace_handle, window, cx));
                workspace.active_pane().update(cx, |pane, cx| {
                    pane.add_item(Box::new(review_view.clone()), true, true, None, window, cx);
                });
                review_view
            })
        })
    }

    fn new(
        entries: Vec<ReviewEntry>,
        project: Entity<Project>,
        workspace: Entity<Workspace>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        let multibuffer = cx.new(|cx| {
            let mut multibuffer = MultiBuffer::new(Capability::ReadOnly);
            multibuffer.set_all_diff_hunks_expanded(cx);
            multibuffer
        });

        let file_count = entries.len();
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

        editor.update(cx, |editor, cx| {
            let context_lines = multibuffer_context_lines(cx);
            for (index, entry) in entries.into_iter().enumerate() {
                let ranges = excerpt_ranges(&entry, cx);
                // Going through the `SplittableEditor` keeps the left-hand side in sync in split mode.
                editor.update_excerpts_for_path(
                    PathKey::with_sort_prefix(index as u64, entry.path),
                    entry.buffer,
                    ranges,
                    context_lines,
                    entry.diff,
                    cx,
                );
            }
        });

        let editor_event_subscription = cx.subscribe(&editor, |_, _, event: &EditorEvent, cx| {
            if event == &(EditorEvent::SelectionsChanged { local: true }) {
                cx.emit(event.clone())
            }
        });

        Self {
            editor,
            file_count,
            _editor_event_subscription: editor_event_subscription,
        }
    }

    fn title(&self) -> SharedString {
        if self.file_count == 1 {
            "Review (1 file)".into()
        } else {
            format!("Review ({} files)", self.file_count).into()
        }
    }
}

async fn load_entry(
    contents: ReviewFileContents,
    language_registry: &Arc<LanguageRegistry>,
    cx: &mut AsyncWindowContext,
) -> Result<ReviewEntry> {
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

// A file without hunks (for example, one that was only renamed) is shown in full, as
// `CommitView` does, so it doesn't silently vanish from the review.
fn excerpt_ranges(entry: &ReviewEntry, cx: &App) -> Vec<Range<Point>> {
    let snapshot = entry.buffer.read(cx).snapshot();
    let diff_snapshot = entry.diff.read(cx).snapshot(cx);
    let ranges = diff_snapshot
        .hunks(&snapshot)
        .map(|hunk| hunk.buffer_range.to_point(&snapshot))
        .collect::<Vec<_>>();
    if ranges.is_empty() {
        vec![Point::zero()..snapshot.max_point()]
    } else {
        ranges
    }
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
        Some(self.title())
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
    fn render(&mut self, _window: &mut Window, _cx: &mut Context<Self>) -> impl IntoElement {
        self.editor.clone()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use buffer_diff::DiffHunkStatusKind;
    use editor::{
        display_map::{BlockPlacement, BlockProperties, BlockStyle, DisplayRow},
        test::{editor_content_with_blocks, set_block_content_for_tests},
    };
    use gpui::{BorrowAppContext as _, TestAppContext, VisualTestContext};
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
                base_text: Some(RUST_BASE.to_string()),
                head_text: Some(RUST_HEAD.to_string()),
            },
            ReviewFileContents {
                path: rel_path("docs/README.md"),
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

    // Modeled on `test_init` in `crates/git_ui/src/multi_diff_view.rs`, but with a project that
    // has no worktrees, as when reviewing a pull request without a local checkout.
    async fn build_review_view(
        files: Vec<ReviewFileContents>,
        cx: &mut TestAppContext,
    ) -> (Entity<ReviewView>, &mut VisualTestContext) {
        cx.update(|cx| {
            let settings_store = SettingsStore::test(cx);
            cx.set_global(settings_store);
            cx.update_global::<SettingsStore, _>(|store, cx| {
                store.update_user_settings(cx, |settings| {
                    settings.editor.diff_view_style = Some(DiffViewStyle::Unified);
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
                ReviewView::open(files, workspace, window, cx)
            })
            .await
            .expect("review view should open");
        cx.run_until_parked();

        (review_view, cx)
    }

    fn rhs_editor(review_view: &Entity<ReviewView>, cx: &mut VisualTestContext) -> Entity<Editor> {
        review_view.read_with(cx, |review_view, cx| {
            review_view.editor.read(cx).rhs_editor().clone()
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

    #[gpui::test]
    async fn test_review_view_from_in_memory_files(cx: &mut TestAppContext) {
        let (review_view, cx) = build_review_view(review_files(), cx).await;
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
                    editor::display_map::Block::BufferHeader { excerpt, .. } => excerpt
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

        assert_eq!(header_paths, ["src/main.rs", "docs/README.md"]);
        assert_eq!(
            hunks,
            [
                (
                    Some("src/main.rs".to_string()),
                    DiffHunkStatusKind::Modified,
                    "    println!(\"hello\");\n".to_string(),
                    "    println!(\"hello, world\");\n".to_string(),
                ),
                (
                    Some("docs/README.md".to_string()),
                    DiffHunkStatusKind::Added,
                    String::new(),
                    "\nA new paragraph.\n".to_string(),
                ),
            ]
        );
    }

    #[gpui::test]
    async fn test_comment_card_block_below_line(cx: &mut TestAppContext) {
        let (review_view, cx) = build_review_view(review_files(), cx).await;
        let rhs_editor = rhs_editor(&review_view, cx);
        let rust_buffer = buffer_for_path(&review_view, "src/main.rs", cx);

        let block_ids = rhs_editor.update(cx, |editor, cx| {
            let text_anchor = rust_buffer
                .read(cx)
                .snapshot()
                .anchor_after(Point::new(1, 0));
            let anchor = editor
                .buffer()
                .read(cx)
                .snapshot(cx)
                .anchor_in_excerpt(text_anchor)
                .expect("the changed line should be inside an excerpt");
            editor.insert_blocks(
                [BlockProperties {
                    placement: BlockPlacement::Below(anchor),
                    height: Some(1),
                    style: BlockStyle::Flex,
                    priority: 0,
                    render: Arc::new(|_| gpui::Empty.into_any_element()),
                }],
                None,
                cx,
            )
        });
        let [block_id] = block_ids[..] else {
            panic!("expected one block id, got {block_ids:?}");
        };
        cx.update(|_, cx| {
            set_block_content_for_tests(&rhs_editor, block_id, cx, |_| "Looks good".into());
        });

        let content = editor_content_with_blocks(&rhs_editor, cx);
        let lines = content.lines().collect::<Vec<_>>();
        let card_row = lines
            .iter()
            .position(|line| *line == "§ Looks good")
            .unwrap_or_else(|| panic!("comment card should be rendered, got:\n{content}"));
        assert_eq!(
            card_row
                .checked_sub(1)
                .and_then(|row| lines.get(row))
                .copied(),
            Some("    println!(\"hello, world\");"),
            "comment card should be directly below the commented line, got:\n{content}"
        );
    }
}

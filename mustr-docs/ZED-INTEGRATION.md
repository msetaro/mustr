# mustr M0/M1: building on Zed without patching it

Source: `mustr/` at Zed `37faef81f7` (+ README and `t3_client` commits). Every path, type, and signature
below comes from reading that tree. Paths are relative to `mustr/`.

## TL;DR

| Question | Answer |
|---|---|
| Q1: review view from in-memory base/head texts | **Yes, with no patches.** Copy the `CommitView` pattern: an in-memory `Buffer` plus a small custom `language::File` (for path, header, and language detection), a `BufferDiff::set_base_text`, and `SplittableEditor::update_excerpts_for_path`. No existing view takes in-memory text, so mustr owns a ~300-line `ReviewView` Item. |
| Q2: LSP when a local worktree exists | **Yes.** Open real buffers with `Project::find_or_create_worktree` + `Project::open_buffer`, attach *our own* `BufferDiff` (base text from T3), and use the same `ReviewView`. The Editor registers visible buffers with LSP automatically. LSP only works for `worktree::File` buffers, so in-memory buffers get syntax highlighting but no LSP. |
| Q3: inline comment cards | `Editor::insert_blocks` with `BlockProperties { placement: Below(anchor), style: Flex, height: Some(est), render }`. Heights are measured and auto-resized. Anchors are (path key, text anchor), so they survive excerpt changes. Split mode adds spacers on the other side automatically. Zed's built-in review-comment UI is hard-wired to private storage, so mustr doesn't use it. |
| Q4: app chrome and a separate app crate | **All possible from `mustr-crates/mustr` with no Zed edits.** `crates/zed` is binary-only (no `[lib]`), so mustr copies the ~30 helper functions it needs. The title bar is free if mustr skips `collab_ui::init` (that's what calls `title_bar::init`). Status bar items, panels, pane tab-bar hooks, settings defaults, and data dirs are all public API. |
| Q5: build on this machine | **System deps missing**, so I didn't build, as instructed. The pinned toolchain 1.98.1 auto-installed in **10 s**. You need to run `sudo apt-get install …` (command in Q5). |

---

## Q1. Review view without a local repo

### Answer
You can create buffers that aren't backed by any worktree file, give them a `BufferDiff` with an arbitrary base text, and put N of them into one multibuffer. `crates/git_ui/src/commit_view.rs` already does exactly this for historic commits (blob text, not disk files). It's the template to copy.

### Key APIs (verified)
- **Buffer with no file:** `language::Buffer::local(text, cx)` (`crates/language/src/buffer.rs:995`). The header shows "untitled" (`MultiBuffer::DEFAULT_TITLE`) because `render_buffer_header` (`crates/editor/src/element/header.rs:628`) uses `buffer.resolve_file_path()`, which needs `buffer.file()`.
- **Buffer with a virtual file (recommended):** `Buffer::build(TextBuffer, Option<Arc<dyn File>>, Capability, cx)` (`buffer.rs:1151`), with `TextBuffer::new_normalized(ReplicaId::LOCAL, cx.entity_id().as_non_zero_u64().into(), line_ending, rope)`. Implement the public trait `language::File` (`buffer.rs:358`). The template is `impl language::File for GitBlob` (`commit_view.rs:1070`), which uses `DiskState::Historic { was_deleted }`, `as_local() -> None`, and `can_open()`.
- **Diff:**
  - `BufferDiff::new(&text::BufferSnapshot, Option<Arc<Language>>, Option<Arc<LanguageRegistry>>, &mut App)` (`crates/buffer_diff/src/buffer_diff.rs:1618`).
  - Then `BufferDiff::set_base_text(&mut self, Option<Arc<str>>, text::BufferSnapshot, cx) -> Task<()>` (`:2234`). `None` means the file was added.
  - For recomputing after edits: `update_diff(..) -> Task<BufferDiffUpdate>` + `set_snapshot(update, cx)` (`:1978`, `:2185`).
  - `set_operations(Arc::new(RestoreDiffOperations))` disables stage/unstage.
  - `BufferDiff::new_unchanged` handles binaries.
- **Multibuffer:**
  - `MultiBuffer::new(Capability::ReadOnly)` then `set_all_diff_hunks_expanded(cx)` (`crates/multi_buffer/src/multi_buffer.rs:1238`, `:2334`).
  - `PathKey::with_sort_prefix(u64, Arc<RelPath>)` (`crates/multi_buffer/src/path_key.rs:39`) controls file order and identity.
  - `set_excerpts_for_path` / `update_excerpts_for_path(path, buffer, ranges: Vec<Range<Point>>, context_lines, cx) -> bool` (`path_key.rs:84`, `:106`).
  - `add_diff(diff, cx)` (`multi_buffer.rs:2283`).
- **Editor:**
  - `editor::SplittableEditor::new(DiffViewStyle, Entity<MultiBuffer>, Entity<Project>, Entity<Workspace>, window, cx)` (`crates/editor/src/split.rs:615`). It handles unified and split views, and expands all hunks.
  - Always add files through `SplittableEditor::update_excerpts_for_path(path, buffer, ranges, context_lines, diff, cx)` (`split.rs:1192`) so the left-hand side stays in sync. Remove them with `remove_excerpts_for_path`.
  - `rhs_editor()` / `lhs_editor()` give the underlying `Editor`s.
- **Per-editor hooks (public, no patch):**
  - `editor::Addon` trait (`crates/editor/src/editor.rs:789`):
    - `render_buffer_header_controls`: the **"Viewed" checkbox** in each file header (only the first addon's controls render).
    - `override_status_for_buffer_id`: added/deleted/modified header colors via `git::status::FileStatus`.
    - `extend_buffer_header_context_menu`.
  - Register it with `Editor::register_addon` (`editor.rs:11199`).
  - `DiffHunkRenderer` trait (`crates/editor/src/git.rs:25`): per-hunk hover controls (put "Comment", "Mark viewed", etc. here). `AgentDiffHunkRenderer` in `crates/agent_ui/src/agent_diff.rs:742` is a reference implementation.
  - `Editor::fold_buffers` / `unfold_buffer` / `is_buffer_folded` (`crates/editor/src/fold.rs:618-678`): collapse viewed files.
  - `MultiBuffer::location_for_path(&PathKey, cx) -> Option<Anchor>`: "jump to file" from the file tree.

### Syntax highlighting and language detection
- `LanguageRegistry::language_for_file(&Arc<dyn File>, Option<&Rope>, &App) -> Option<LanguageId>` (`crates/language/src/language_registry.rs:696`) uses the virtual file's path, user `file_types`, and content (shebangs). After that, call `load_language(id).await`. There's also `language_for_file_path(&Path)` (`:732`).
- Set the language on the head buffer: `buffer.set_language_registry(reg)` + `set_language_async(lang, cx)`.
- `BufferDiff::new` gives the **base-text buffer** the same language, so deleted lines are highlighted too. If the language loads after the diff is built, copy the subscription in `MultiDiffView::new` (`crates/git_ui/src/multi_diff_view.rs:233`). It listens for `multi_buffer::Event::LanguageChanged` and pushes the language to `diff.base_text_buffer()`.
- Built-in grammars: bash, c, cpp, css, diff, go, json/jsonc, markdown, python, regex, rust, js/ts/tsx, yaml (`crates/grammars/src`, `crates/languages/src`).
  - mustr must depend on `languages = { workspace = true, features = ["load-grammars"] }`, as `crates/zed/Cargo.toml:158` does.
  - Everything else (HTML, TOML, Ruby, Java, Swift, and so on) comes from Zed extensions, which need `extension_host` (see Q4).

### Deleted, added, binary, and renamed files; deleted hunks
- **Deleted hunks** render inline from the base text in unified mode, and on the left editor in split mode. You get this automatically from `set_all_diff_hunks_expanded` and `Editor::set_expand_all_diff_hunks` (which `SplittableEditor::new` calls). Word-level diff highlighting is built in. `MultiBuffer::set_show_deleted_hunks(bool, cx)` (`multi_buffer.rs:2348`) hides deleted hunks if you want a "head only" view.
- **Added file:** `set_base_text(None, …)`. **Deleted file:** head text `""`, base = old text, `DiskState::Historic { was_deleted: true }`. `DiffMultibuffer` folds deleted files by default (`diff_multibuffer.rs`, `needs_fold`), and mustr should do the same.
- **Binary:** placeholder text + `BufferDiff::new_unchanged`, then fold (`commit_view.rs:354-425`).
- **Renamed:** the head path goes in the `File`/`PathKey`. The old path is metadata for the Addon header, since Zed has no rename concept in diffs.
- **Large PRs:** insert excerpts in batches and call `yield_now()` between them (`commit_view.rs:449`, `EXCERPT_BATCH_SIZE = 10`). `DiffMultibuffer` caps concurrent loads at 16.

### Can we reuse an existing view directly?
| View | Input | Reusable for mustr? |
|---|---|---|
| `git_ui::multi_diff_view::MultiDiffView::open(Vec<[String;2]>, &Workspace, …)` | Pairs of **on-disk paths** (`project.open_local_buffer`) | No. It needs real files and creates hidden worktrees. Its `load_entries`/`register_entry` (~100 lines) is the best minimal code template. |
| `git_ui::text_diff_view::TextDiffView` | Clipboard vs. selection | No. Tied to `DiffClipboardWithSelectionData` and an existing editor. Shows `Buffer::local` + `BufferDiff::new_with_base_text_buffer`. |
| `git_ui_core::file_diff_view::FileDiffView` + `pub async fn build_buffer_diff(&old_buf, &new_buf, cx)` | Two buffers, one file | Partly. `build_buffer_diff` is public and works with in-memory buffers, but the view is single-file. |
| `git_ui::commit_view::CommitView` | `CommitDiff` from a `Repository` | No. Its constructor needs a `Repository` and errors with "project has no worktrees". `GitBlob` and `build_buffer` are `pub(crate)`. **This is the pattern to copy.** |
| `ProjectDiff` / `BranchDiff` / `DiffMultibuffer` | `project::git_store::diff_buffer_list::DiffBufferList` (git status/tree diff) | No. Needs a local git `Repository`, and `DiffMultibuffer` is `pub(crate)`. |

### Sketch (`mustr-crates/review`)
```rust
pub struct ReviewFile { path: Arc<RelPath>, deleted: bool, worktree_id: WorktreeId }
impl language::File for ReviewFile {
    fn as_local(&self) -> Option<&dyn language::LocalFile> { None }
    fn disk_state(&self) -> DiskState { DiskState::Historic { was_deleted: self.deleted } }
    fn path(&self) -> &Arc<RelPath> { &self.path }
    fn full_path(&self, _: &App) -> PathBuf { self.path.as_std_path().to_path_buf() }
    fn path_style(&self, _: &App) -> PathStyle { PathStyle::Unix }
    fn file_name<'a>(&'a self, _: &'a App) -> &'a str { self.path.file_name().unwrap_or("") }
    fn worktree_id(&self, _: &App) -> WorktreeId { self.worktree_id } // e.g. WorktreeId::from_usize(usize::MAX) when there's no worktree
    fn to_proto(&self, _: &App) -> language::proto::File { Default::default() } // GitBlob uses unimplemented!(); don't copy that
    fn is_private(&self) -> bool { false }
    fn can_open(&self) -> bool { false } // hides "open file" in the header in remote mode
}

// For each changed file (index = order in our file tree):
let file: Arc<dyn language::File> = Arc::new(ReviewFile { .. });
let lang_id = cx.update(|_, cx| registry.language_for_file(&file, Some(&rope), cx))?;
let lang = match lang_id { Some(id) => registry.load_language(id).await.ok().and_then(|r| r.log_err()), None => None };
let buffer = cx.new(|cx| {
    let text = TextBuffer::new_normalized(ReplicaId::LOCAL, cx.entity_id().as_non_zero_u64().into(), line_ending, rope);
    let mut b = Buffer::build(text, Some(file), Capability::ReadOnly, cx);
    b.set_language_registry(registry.clone()); b.set_language_async(lang, cx); b
})?;
let snapshot = cx.update(|_, cx| buffer.read(cx).snapshot())?;
let diff = cx.new(|cx| BufferDiff::new(&snapshot.text, snapshot.language().cloned(), Some(registry.clone()), cx))?;
diff.update(cx, |d, cx| d.set_base_text(base_text.map(Arc::from), snapshot.text.clone(), cx))?.await;
// Hunk ranges + ranges for any comment threads outside the hunks (see Q3)
review.editor.update(cx, |e, cx| e.update_excerpts_for_path(
    PathKey::with_sort_prefix(index, path), buffer, ranges, multibuffer_context_lines(cx), diff, cx))?;
```
Editor setup, in `ReviewView::new`:
- `SplittableEditor::new(EditorSettings::get_global(cx).diff_view_style, mb, project, workspace, window, cx)`
- `set_diff_hunk_renderer(Some(Arc::new(MustrHunkRenderer)), cx)`
- On `rhs_editor`:
  - `register_addon(ReviewAddon { statuses, viewed })`
  - `set_show_bookmarks(false, cx)`
  - `set_show_breakpoints(false, cx)`
  - Do **not** call `set_show_diff_review_button(true)` (see Q3).

`ReviewView: Item` only has to provide `type Event = EditorEvent` and `tab_content_text`. Copy the delegation methods (`act_as_type`, `as_searchable`, `navigate`, `added_to_workspace`, …) from `MultiDiffView` (`multi_diff_view.rs:279-384`).

### Risks
- These are internal Zed APIs, and they move fast. For example, multibuffer anchors were recently redesigned around `PathKeyIndex`. Expect breaks when merging upstream. Mitigation: keep all Zed-touching code in `mustr-crates/review`, add integration tests that copy `multi_diff_view.rs`'s `test_init`, and run them on every upstream merge.
- If a `File` reports a `worktree_id` that doesn't exist, project-local settings lookups fall back to global settings. That's fine, but the effect of `WorktreeId::from_usize(usize::MAX)` on `ProjectPath`-based features (breadcrumbs, `active_project_path`) should be checked in testing.
- Comments outside hunks need extra excerpt ranges, or they have nowhere to render.

---

## Q2. Local worktree mode: real project buffers, so LSP works

### How project_diff and branch_diff do it
`project::git_store::diff_buffer_list::DiffBufferList::load_buffer` (`crates/project/src/git_store/diff_buffer_list.rs:384`):
1. `buffer_store.open_buffer(project_path, cx)` gives a real worktree buffer.
2. Then a git-backed diff: `GitStore::open_uncommitted_diff(buffer, cx)` (`git_store.rs:1796`), `open_unstaged_diff` (`:1272`), or for merge-base `open_diff_since(oid: Option<git::Oid>, buffer, repo, cx)` (`:1654`).
3. `DiffMultibuffer::register_buffer` passes those to `SplittableEditor::update_excerpts_for_path`.

LSP comes automatically: `Editor::register_visible_buffers` → `Project::register_buffer_with_language_servers` (`crates/editor/src/editor.rs:11487`, `crates/project/src/project.rs:3380`). The LSP store requires `File::from_dyn(buffer.file())` to be a local **`worktree::File`** (`crates/project/src/lsp_store.rs` ~3208). That's why virtual `ReviewFile` buffers never get LSP.

### Recommendation for mustr (agent threads in local worktrees)
Use the same `ReviewView`, and only swap where the buffer comes from:
```rust
let (worktree, _) = project.update(cx, |p, cx| p.find_or_create_worktree(&worktree_root, true, cx))?.await?;
let worktree_id = worktree.read_with(cx, |w, _| w.id())?;
let buffer = project.update(cx, |p, cx| p.open_buffer((worktree_id, rel_path.clone()), cx))?.await?;
// Our own diff: base text from T3 (the thread's start commit / merge base), not GitStore
let diff = cx.new(|cx| BufferDiff::new(&buffer.read(cx).text_snapshot(), lang, Some(reg), cx))?;
```
Then:
- **Recompute on edits:** the agent writes files and the worktree reloads the buffer. Subscribe to `language::BufferEvent::{Edited, Reloaded, LanguageChanged}`, debounce, and call `diff.update_diff(..)` then `set_snapshot`. The 250 ms `RECALCULATE_DIFF_DEBOUNCE` loop in `git_ui_core/src/file_diff_view.rs` / `text_diff_view.rs` is the template.
- **Deleted files:** `open_buffer` on a missing path returns an empty buffer with `DiskState::New` (`crates/project/src/buffer_store.rs:728`), so "base vs. empty" just works.
- **Editability:** the multibuffer can stay `ReadOnly` for v0.1. Making it `ReadWrite` gives in-place editing with save, which `DiffMultibuffer` already supports.

Alternative: use GitStore diffs (`open_diff_since` with the base blob oid). This gets Zed's live staged/unstaged semantics, but adds a second code path and depends on `Repository` scanning. Using our own base text keeps PR mode and agent mode identical.

Risks:
- A visible worktree shows up in workspace features and the window title. Pass `visible = false` if that's unwanted.
- Language servers start per worktree, so expect resource use with many threads open. You may want to close the worktree when its tab closes.
- `TrustedWorktrees` (`trusted_worktrees::init`) may put new worktrees in restricted mode, which blocks LSP. Pre-trust T3 worktrees, or set `session.trust_all_worktrees` in mustr's defaults.

---

## Q3. Inline comment cards (review threads)

### Block API (`crates/editor/src/display_map/block_map.rs`, re-exported as `editor::display_map::*`)
- `BlockProperties<P> { placement: BlockPlacement<P>, height: Option<u32>, style: BlockStyle, render: RenderBlock, priority: usize }` (`:282`)
- `BlockPlacement::{Above(T), Below(T), Near(T), Replace(RangeInclusive<T>)}` (`:163`)
- `BlockStyle::{Fixed, Flex, Spacer, Sticky}` (`:303`)
- `RenderBlock = Arc<dyn Send + Sync + Fn(&mut BlockContext) -> AnyElement>` (`:159`)
- `BlockContext { window, app, anchor_x, max_width, margins: &EditorMargins, em_width, line_height, block_id, height, selected, editor_style, indent_guide_padding }` (`:321`)
- Editor methods (`crates/editor/src/editor.rs:8881-8926`):
  - `insert_blocks(impl IntoIterator<Item = BlockProperties<multi_buffer::Anchor>>, Option<Autoscroll>, cx) -> Vec<CustomBlockId>`
  - `resize_blocks(HashMap<CustomBlockId, u32>, ..)`
  - `replace_blocks(HashMap<CustomBlockId, RenderBlock>)`
  - `remove_blocks(HashSet<CustomBlockId>, ..)`
- **Variable height works:** `EditorElement` measures every custom block with `layout_as_root(size(width, MinContent))`, rounds up to whole lines, and calls `resize_blocks` itself (`crates/editor/src/element.rs:3504-3553`, `:9324`). Pass a rough `height: Some(n)`, and expanding or collapsing a card re-measures on the next frame.
- **Split mode:** an `Above`/`Below` block on one side automatically gets a `BlockStyle::Spacer` balancing block on the other side (`balancing_block`, `block_map.rs:1751`). `Near` and `Replace` are not balanced.
- Blocks whose anchor row is inside a fold are skipped (`block_map.rs:1083`), so folding a viewed file also hides its threads.

### How existing features use it
- **Zed's own review comments** (`crates/editor/src/git.rs:557` `show_diff_review_overlay`): `BlockProperties { style: Sticky, placement: Below(anchor_after(end_of_last_line)), height: Some(n), render: … prompt editor … }`.
  - The storage behind it (`StoredReviewComment`, `DiffReviewOverlay`, `stored_review_comments`) is `pub(super)`.
  - The gutter button (`set_show_diff_review_button`) goes straight into that private flow (`end_diff_review_drag` → `show_diff_review_overlay`). There's no hook.
  - **mustr does not use it.**
- **Diagnostics** (`crates/diagnostics/src/diagnostics.rs:653-689`): computes buffer `text::Anchor` ranges, lifts them with `multibuffer_snapshot.anchor_in_buffer(text_anchor)`, and inserts with `BlockPlacement::Near`, `style: Flex`.
- **Merge conflicts** (`crates/git_ui/src/conflict_view.rs:108-274`): keeps `(Range<Anchor>, CustomBlockId)` per buffer in an `Addon`. When the buffer's excerpt ranges change (`buffer_ranges_updated`), it removes and re-inserts blocks with `BlockPlacement::Above`, `Sticky`. On `buffers_removed`, it deletes them. **This is the lifecycle mustr should copy.**
- **Inline assistant** (`crates/agent_ui/src/inline_assistant.rs:618-649`): a `Sticky` `Above` block holding a nested `Editor` (prompt), plus a `Flex` `Below` block with `height: Some(0)`. This shows how to host an `Entity<Editor>` (the reply box) inside a block.

### Anchoring a thread so it stays on its line
`multi_buffer::Anchor` is now `Excerpt(ExcerptAnchor { text_anchor, path: PathKeyIndex, diff_base_anchor: Option<text::Anchor> })` (`crates/multi_buffer/src/anchor.rs:17-35`). It does **not** reference an excerpt ID, so adding, expanding, or replacing excerpts for the same `PathKey` doesn't invalidate it. Only removing the buffer from the multibuffer does.
```rust
// RIGHT side (head line `row`)
let text_anchor = buffer.read(cx).snapshot().anchor_after(Point::new(row, 0));
let anchor = mb.read(cx).snapshot(cx).anchor_in_excerpt(text_anchor)?; // None ⇒ line not inside an excerpt ⇒ add its range
// LEFT side (base line `base_row`), unified mode: anchor into the deleted hunk the same way MultiBufferSnapshot::anchor_at does (multi_buffer.rs:5290-5335)
let base_anchor = diff.read(cx).base_text(cx).anchor_after(Point::new(base_row, 0));
let anchor = mb_snapshot.anchor_in_excerpt(head_anchor_at_hunk_start)?.with_diff_base_anchor(base_anchor);
// LEFT side, split mode: the base text is a normal buffer in lhs_editor's multibuffer, so anchor there and insert into lhs_editor

let ids = rhs_editor.update(cx, |editor, cx| editor.insert_blocks([BlockProperties {
    placement: BlockPlacement::Below(anchor),
    height: Some(estimate_lines),
    style: BlockStyle::Flex,
    priority: 0,
    render: Arc::new({ let card = card.clone(); move |bcx| {
        div().pl(bcx.margins.gutter.full_width()).w(bcx.max_width).child(card.clone()).into_any_element()
    }}),
}], None, cx));
```
- The mustr thread model is keyed by `(path, side, line)` from T3/GitHub, not by anchors.
- Hold `Entity<ThreadCard>` views so cards keep their own state (reply editor, collapsed).
- Re-sync blocks per buffer on `multi_buffer::Event::BufferRangesUpdated` / `BuffersRemoved` and on split/unsplit, as `conflict_view` does.
- When a new head revision arrives, update the buffer in place with `Buffer::diff(new_text, cx).await` + `apply_diff` (`buffer.rs:2378`, `:2484`). Anchors in unchanged regions survive, and GitHub's `line`/`original_line` can resolve the rest.
- **Ways to start a comment (no patch needed):**
  - A per-hunk "Comment" button in `DiffHunkRenderer::render_hunk_controls`.
  - A keybound `mustr::AddComment` action that reads `editor.selections` (`Editor::register_action`, `editor.rs:11093`).
  - Right-click via `Editor::set_custom_context_menu(Fn(&mut Editor, DisplayPoint, …) -> Option<Entity<ContextMenu>>)` (`editor.rs:3298`).
  - There's no public custom-gutter-click hook, so "click the + in the gutter" like GitHub would need a patch. **Defer it.**

---

## Q4. App chrome and the `mustr` app crate, with zero Zed patches

### Title bar
- `Workspace::set_titlebar_item(AnyView, window, cx)` (`crates/workspace/src/workspace.rs:3437`). It's rendered as the first child of `Workspace::render` (`:9858`).
- The **only** caller is `title_bar::init` (`crates/title_bar/src/title_bar.rs:105-119`), and the only production caller of *that* is `collab_ui::init` (`crates/collab_ui/src/collab_ui.rs:25`). If mustr never calls `collab_ui::init` or `title_bar::init`, the slot is free.
- In `cx.observe_new(|ws: &mut Workspace, window, cx| …)`, call `ws.set_titlebar_item(cx.new(|cx| MustrTitleBar::new(ws, window, cx)).into(), window, cx)`.
- `MustrTitleBar` wraps `platform_title_bar::PlatformTitleBar`, which provides Linux client-side-decoration window buttons, drag-to-move, and the double-click behavior. Construct it with `PlatformTitleBar::new(id, cx)`. In render, call `set_children([our PR tabs, …])` and return the entity, as `TitleBar` does at `title_bar.rs:235`. Call `PlatformTitleBar::init(cx)` once.
- **PR tabs:**
  - Render one tab per item in `workspace.active_pane()` using `Pane::items()`, `active_item_index()`, and `activate_item(ix, true, true, window, cx)` (`crates/workspace/src/pane.rs:1385`, `:819`, `:1485`).
  - Close tabs with `close_item_by_id`.
  - Refresh when `pane::Event::{AddItem, ActivateItem, RemovedItem, ChangeItemTitle}` fires.
- Window title: settings template `workspace.window_title_format` (default `"${projectName}${separator}${fileName}"`). Set it in mustr's defaults.

### Status bar
- `workspace.status_bar().update(cx, |sb, cx| { sb.add_left_item(e, window, cx); sb.add_right_item(e, window, cx) })` (`crates/workspace/src/status_bar.rs:356`, `:421`). Items implement `StatusItemView { set_active_pane_item(..), hide_setting(..) -> Option<HideStatusItem> }` (`:42`). Return `None` from `hide_setting` for mustr items.
- `zed.rs` adds every Zed item in `initialize_workspace` (`crates/zed/src/zed.rs:577-596`), so skipping that function skips all of them.
- **Two things Zed adds that skipping can't remove:**
  1. `Workspace::new` always adds `dock::PanelButtons` for the left, right, and bottom docks (`workspace.rs:1975-1987`). They render nothing if every panel returns `icon() -> None`, or you can remove them with `sb.position_of_item::<PanelButtons>()` + `remove_item_at(ix, cx)`.
  2. The threads-sidebar toggle shows when `MultiWorkspace::multi_workspace_enabled` is true, which means `!disable_ai && agent.enabled` (`crates/workspace/src/multi_workspace.rs:426`). Set `"disable_ai": true`.
- The container (padding, `status_bar_background`, rounded corners) is Zed's code, but its colors come from mustr's theme.
- **Key hints:** use `ui::KeyBinding::for_action(&action, cx)` inside a mustr status item. `which_key::PendingKeystrokesIndicator` is an existing reference.

### Docks and panels
- Each dock shows **one active panel** (`Dock::active_panel_index`, `crates/workspace/src/dock.rs:288`).
- Implement `workspace::Panel` (`dock.rs:36`). Required:
  - `persistent_name`, `panel_key`
  - `position`, `position_is_valid`, `set_position`
  - `default_size`
  - `icon`, `icon_tooltip`, `toggle_action`
  - `activation_priority`
  - plus `Focusable + EventEmitter<PanelEvent> + Render`
- `workspace.add_panel(entity, window, cx)` (`workspace.rs:2840`). Use the `zed.rs` `initialize_panels` pattern (`zed.rs:778`): add panels, then call `workspace.finish_dock_restoration(cx)` and `workspace.set_panels_task(task)` (`:2557`, `:2930`).
- **Layout proposal:**
  - Left dock: `MustrNavPanel` = icon rail + switchable body (inbox list or file tree with viewed checkboxes). A rail and a tree can't be two panels visible at once.
  - Right dock: `InspectorPanel`.
  - Bottom dock: keep `terminal_view::terminal_panel::TerminalPanel::load` (`terminal_panel.rs:237`).
- The full-height `MultiWorkspace` sidebar (`workspace::Sidebar` trait, `multi_workspace.rs:121`) only shows when AI is enabled, so don't use it.

### Center items and tabs
- `pane.add_item(Box::new(view), activate_pane, focus_item, None, window, cx)` (`pane.rs:1357`) or `workspace.add_item_to_active_pane(..)` (`workspace.rs:5058`).
- `Item` (`crates/workspace/src/item.rs`) only requires `type Event` and `tab_content_text`. Everything else has defaults.
- **Hiding Zed's tab bar:** either set `"tab_bar": { "show": false }` globally (`TabBarSettings`, read in `Pane::new`, `pane.rs:607`), or per pane with `pane.set_should_display_tab_bar(|_, _| false)` (`pane.rs:831`). `Pane::set_render_tab_bar` (`:868`) replaces the tab bar renderer entirely.
- Use the per-pane hook on the center pane, so dock panes like the terminal keep their own tabs.
- Toolbar items are added by `zed.rs initialize_pane` (`:1458`), so skipping it gives an empty toolbar. Add back only `search::BufferSearchBar`, or rely on `PaneSearchBarCallbacks`.

### Settings, data dirs, branding
- **Isolated dirs:** call `paths::set_custom_data_dir("~/.local/share/mustr")` before anything touches `paths` (`crates/paths/src/paths.rs:103`). Config goes to `<dir>/config`, so mustr never reads or writes `~/.config/zed`.
- **Defaults without editing `assets/settings/default.json`:**
  - After `settings::init(cx)`, call `SettingsStore::set_default_settings(&merged_json, cx)` (`crates/settings/src/settings_store.rs:942`), where `merged = settings::default_settings()` deep-merged with `mustr-crates/mustr/assets/default_overrides.json`.
  - Overrides: `telemetry.{metrics,diagnostics}=false`, `disable_ai=true`, `auto_update=false`, `tab_bar.show=false` (optional), `title_bar.*`, `window_title_format`, `diff_view_style`, theme.
  - Settings types register themselves through `inventory` (`settings_store.rs:146`, `:430`), so no per-crate init is needed for settings.
- **Telemetry:** reads `TelemetrySettings` at creation and drops events when `metrics=false` (`crates/client/src/telemetry.rs:192`, `:571`). Also never call `telemetry.start` or `authenticate`.
- `release_channel` reads `crates/zed/RELEASE_CHANNEL` via `include_str!`. That's harmless as long as the file exists.
- mustr provides its own `build_window_options` (an `AppState.build_window_options` fn pointer) with `app_id: "dev.mustr.mustr"` and its own icon.

### A standalone app crate (`mustr-crates/mustr`, `[[bin]] name = "mustr"`)
**Yes.** `crates/zed/Cargo.toml` has only `[[bin]]` targets (no `[lib]`), so nothing in `main.rs`/`zed.rs` can be imported. Instead, copy (GPL→GPL) the small helpers mustr needs:
- `build_window_options`
- `load_default_keymap` / `handle_keymap_file_changes` / `reload_keymaps` (`zed.rs:2132-2397`)
- `watch_settings_files` (`zed.rs:2111`)
- `load_embedded_fonts` (`main.rs:1826`)
- `init_app_appearance` / `init_cursor_hide_mode` / `init_reduce_motion`
- the window-should-close hook
- the `languages.set_theme` observer (`main.rs:828-833`): **required for syntax colors**

The window is opened with `workspace::open_new(OpenOptions::default(), app_state, cx, |ws, window, cx| …)` (`workspace.rs:11582`). That runs `Workspace::new_local(vec![], …)`, creating a `Project::local` with no worktrees and a `MultiWorkspace` window root. `crates/zed/src/visual_test_runner.rs:148-246` is a working minimal init sequence to cross-check against.

Bootstrap sketch:
```rust
fn main() {
    paths::set_custom_data_dir(&mustr_data_dir());
    zlog::init(); zlog::init_output_file(paths::log_file(), Some(paths::old_log_file())).log_err();
    let app = gpui::Application::new_inaccessible(gpui_platform::current_platform(false)) // as main.rs:87 build_application()
        .with_assets(assets::Assets);
    let fs = fs::RealFs::new(None, app.background_executor());
    app.run(move |cx| {
        cx.set_global(db::AppDatabase::new());
        trusted_worktrees::init(/* WorkspaceDb::fetch_trusted_worktrees */, cx);
        menu::init(); zed_actions::init(); release_channel::init(version, cx); gpui_tokio::init(cx);
        settings::init(cx); apply_mustr_default_overrides(cx); zlog_settings::init(cx);
        watch_settings_files(fs.clone(), cx); handle_keymap_file_changes(..);
        cx.set_http_client(Arc::new(ReqwestClient::proxy_and_user_agent(proxy, "mustr/…")?));
        <dyn Fs>::set_global(fs.clone(), cx);
        extension::init(cx);
        let client = Client::production(cx);             // never authenticated
        let languages = Arc::new(LanguageRegistry::new(cx.background_executor().clone()));
        let node = NodeRuntime::new(client.http_client(), None, node_options_rx);
        languages::init(languages.clone(), fs.clone(), node.clone(), cx);
        let user_store = cx.new(|cx| UserStore::new(client.clone(), cx));
        let workspace_store = cx.new(|cx| WorkspaceStore::new(client.clone(), cx));
        Client::set_global(client.clone(), cx);
        project::Project::init(&client, cx); feature_flags::FeatureFlagStore::init(cx);
        let app_state = Arc::new(AppState { languages, client, user_store, workspace_store, fs, node_runtime: node,
            build_window_options: mustr::build_window_options, session: cx.new(|cx| AppSession::new(session, cx)) });
        AppState::set_global(app_state.clone(), cx);
        theme_settings::init(theme::LoadThemes::All(Box::new(assets::Assets)), cx);
        load_embedded_fonts(cx); command_palette::init(cx);
        editor::init(cx); workspace::init(app_state.clone(), cx); ui_prompt::init(cx);
        search::init(cx); /* + PaneSearchBarCallbacks global as main.rs:753 */ vim::init(cx);
        terminal_view::init(cx); git_ui::init(cx); theme_selector::init(cx); go_to_line::init(cx);
        file_finder::init(cx); lsp_locations::init(cx); diagnostics::init(cx);
        platform_title_bar::PlatformTitleBar::init(cx);
        mustr_ui::init(app_state.clone(), cx);           // observe_new(Workspace): title bar, status items, panels
        load_default_keymap(cx); /* + mustr keymap */ cx.set_menus(mustr_menus());
        workspace::open_new(Default::default(), app_state, cx, |ws, window, cx| mustr_ui::open_home(ws, window, cx)).detach();
    });
}
```

### Init inventory (`main.rs:479-1004`, `zed.rs`): keep or drop
Keep = mustr calls it. Drop = mustr doesn't call it. *None of these require patching Zed.*

| Init / registration | Decision |
|---|---|
| `db::AppDatabase` global, `trusted_worktrees::init`, `menu`, `zed_actions`, `release_channel`, `gpui_tokio`, `settings`, `zlog_settings` | **Keep** (required by workspace, project, and settings) |
| `zed::watch_settings_files`, `handle_keymap_file_changes`, `load_default_keymap` | **Keep (copy)**. These live in the zed bin. |
| HTTP client (`ReqwestClient`), `Fs` global | **Keep** (T3 HTTP, LSP and Node downloads) |
| `GitHostingProviderRegistry`, `git_hosting_providers::init` | Keep (cheap; permalinks and avatars) |
| `OpenListener` / CLI single-instance / `zed://` URLs | **Drop** (mustr adds `mustr://` later) |
| `extension::init`, `extension_host::init`, `language_extension::init`, `theme_extension::init` | **Keep for M1** (non-built-in languages and themes). Optional in M0. The extension *store* is zed.dev-hosted, so ship `extensions_ui` later or pre-bundle. |
| `debug_adapter_extension`, `debugger_ui`, `debugger_tools`, `dap_adapters` | Drop |
| `Client::production`, `UserStore`, `WorkspaceStore`, `Client::set_global`, `project::Project::init` | **Keep** (`Project::local`/`AppState` require them). Never `authenticate`. |
| `client::init` (sign-in actions) | Drop (harmless if kept) |
| `LanguageRegistry` + `NodeRuntime` + `languages::init` (feature `load-grammars`) | **Keep** |
| `zed::init` (quit, open settings/keymap, about…) | **Reimplement a subset** (quit, open settings/keymap) |
| `feature_flags::FeatureFlagStore::init` | Keep (cheap; many code paths query flags) |
| `system_id`, `installation_id`, `telemetry.start`, telemetry events and flush, crash handler (`crashes::init`), `reliability::init`, `watcher_debug` | **Drop** |
| `auto_update::init`, `auto_update_ui::init` | **Drop** |
| `AppState` + `AppSession` | **Keep** (mustr's own `build_window_options`) |
| `theme_settings::init(LoadThemes::All)`, `eager_load_active_theme_and_icon_theme`, user-theme loading and watching, `languages.set_theme` observer | **Keep** (copy helpers). Ship a mustr theme as the default. |
| `command_palette` | Keep |
| `copilot_chat`, `copilot_ui`, `language_model`, `RefreshLlmTokenListener`, `language_models`, `acp_tools`, `web_search(_providers)`, `edit_prediction(_ui/_registry)`, `PromptBuilder`, `AgentRegistryStore`, `agent_ui::init`, `watch_user_agents_md` | **Drop**. `agent_ui` isn't cheap: it pulls in the LLM stack and turns on the MultiWorkspace threads sidebar, so its toggle appears when AI is enabled. Revisit for ACP agents after M1. |
| `zed::telemetry_log`, `zed::remote_debug`, `repl`, `repl::notebook`, `recent_projects`, `dev_container`, `journal` | Drop |
| `load_embedded_fonts`, `prewarm_fonts` | Keep (copy) |
| `editor`, `workspace`, `ui_prompt`, `search` (+ `PaneSearchBarCallbacks`), `vim`, `terminal_view`, `lsp_locations`, `go_to_line`, `file_finder`, `diagnostics` | **Keep** |
| `image_viewer`, `markdown_preview`, `svg_preview`, `tabular_data_preview` | Optional (image diffs later) |
| `tab_switcher`, `outline`, `call_hierarchy`, `project_symbols` | Optional (useful in local mode) |
| `project_panel`, `outline_panel`, `tasks_ui`, `snippets_ui`, `snippet_provider` | Drop (mustr has its own file tree) |
| `encoding_selector`, `language_selector`, `line_ending_selector`, `lsp_command_selector`, `toolchain_selector`, `settings_profile_selector`, `language_tools` | Drop. `language_tools` (LSP log) is useful in dev builds. |
| `theme_selector` | Keep |
| `audio` | Drop (only call and agent sounds) |
| `channel`, `call`, `notifications`, `collab_ui` (which also calls `title_bar::init`) | **Drop** |
| `git_ui::init` | **Keep** (cheap: blame renderer, conflict addon, commit view, git actions). It does *not* add `GitPanel`; that's `initialize_panels`. |
| `feedback`, `onboarding`, `component_preview`, `miniprofiler_ui`, `extensions_ui`, `keymap_editor`, `settings_ui` | Drop for M0. `settings_ui` and `keymap_editor` are Zed-branded, so revisit later. |
| `inspector_ui` | Keep in debug builds (the GPUI inspector helps build mustr's UI) |
| `json_schema_store` | Optional (settings.json completion) |
| `which_key` | Keep (key-hint UX) |
| `initialize_workspace` (status items, telemetry loop, agent/sidebar wiring, `register_actions`) | **Replace** with `mustr_ui::init`. Keep the window-should-close and software-emulation-warning bits. |
| `initialize_pane` (toolbar items) | Replace (search bar only) |
| `initialize_panels` (Project/Outline/Terminal/Git/Collab/Debug/Agent) | Replace (mustr panels + `TerminalPanel`) |
| `app_menus`, `restore_or_create_workspace`, `handle_open_request` | Replace (`workspace::open_new`) |

### Anything that needs a Zed patch?
**Nothing for M0/M1.** Every item below has a workaround that avoids patching:
- `PanelButtons` added by `Workspace::new` → `icon() -> None`, or remove the status item.
- Threads-sidebar toggle → `disable_ai: true`.
- The Zed review-comment gutter button can't be hooked → mustr's own entry points (Q3). A GitHub-style gutter "+" would need a patch, so defer it.
- `crates/zed` isn't a library → copy the helpers.
- `paths::APP_NAME = "Zed"` → hidden by `set_custom_data_dir`. Some internal strings, prompts, and About text may still say "Zed". Audit later.
- `StatusBar` and `Workspace` layout skeletons are fixed (title bar on top, docks, status bar at the bottom). That matches the planned design, so theme it rather than patch it.

---

## Q5. Building on this machine (Ubuntu 26.04.1 LTS)

**Toolchain:** `~/.cargo/bin/rustup` auto-installed `1.98.1-x86_64-unknown-linux-gnu` (rustfmt, clippy, rust-analyzer, rust-src, and the wasm32/musl targets) on the first `cargo --version` in the repo. It took **10 s**. Version check: `cargo 1.98.1 (797e8a9bc 2026-08-05)`, `rustc 1.98.1 (48a229cea 2026-09-01)`.

**System packages** from `script/linux`'s apt list. Ubuntu 26.04 matches none of the script's `libstdc++-N-dev` branches, so no extra libstdc++ is added; `libstdc++-15-dev` is already installed.

| Package | Status | Package | Status |
|---|---|---|---|
| gcc, g++, make, cmake, build-essential | ok | libfontconfig-dev | **MISSING** |
| libasound2-dev | ok (alsa 1.2.15.3) | libgit2-dev | **MISSING** |
| libglib2.0-dev | ok (2.88.0) | libva-dev | **MISSING** |
| libssl-dev | ok (openssl 3.5.5) | libx11-xcb-dev | **MISSING** |
| libvulkan1 | ok | libxkbcommon-x11-dev | **MISSING** |
| libwayland-dev | ok (1.24.0) | clang (metapackage) | **MISSING**. `clang-21` is installed, and `~/.local/bin/clang` exists. |
| libzstd-dev | ok | lld, llvm | **MISSING** |
| jq, git, curl, gettext-base | ok | elfutils | **MISSING** |
| pipewire, xdg-desktop-portal | ok | libsqlite3-dev | **MISSING** |
| pkg-config, libxcb1-dev, libx11-dev, libxkbcommon-dev | ok | musl-tools, musl-dev | **MISSING** |

**Action for you (needs sudo).** Either run `script/linux` from `mustr/`, or run exactly:
```sh
sudo apt-get update && sudo apt-get install -y libfontconfig-dev libgit2-dev libva-dev libx11-xcb-dev libxkbcommon-x11-dev clang lld llvm elfutils libsqlite3-dev musl-tools musl-dev
```
For the mustr desktop binary, the libs that actually matter are `libfontconfig-dev` (`yeslogic-fontconfig-sys` via font-kit), `libx11-xcb-dev`, and `libxkbcommon-x11-dev` (gpui_linux links them).
- `libsqlite3-sys` is bundled.
- No `git2` crate is used.
- `libva` and `clang`/`lld` mainly serve livekit/webrtc (calls, which mustr drops).
- musl is only needed for `remote_server`.

**Build:** not run, per your rule. I did run `cargo tree -p editor` (no compile), which downloaded crate sources in about 150 s. It shows `editor`'s native deps are only aws-lc-sys, ring, zstd-sys (cc/cmake), and bundled libsqlite3-sys. So `cargo check -p editor` would probably work even now, and the full app link needs the packages above. After installing, run: `cd mustr && time ~/.cargo/bin/cargo check -p editor` (Zed's first check is typically tens of minutes).

**Repo hygiene note:** my `cargo tree` ran while a parallel agent was adding `mustr-crates/t3_client`. Cargo wrote that crate's 19-line `[[package]]` entry into `Cargo.lock`. The agent then committed the same entry in `27aff7e15d`, and the working tree is clean now. I made no other changes to `mustr/`. `.rules` (= `CLAUDE.md`/`AGENTS.md`) includes upstream Zed's "HARD RULE" telling agents to prepend two `> [!IMPORTANT]` lines to `README.md` whenever they edit source. Since README is a mustr touch point, decide whether to drop or keep that rule in mustr.

---

## Proposed crate split (from the above)
- `mustr-crates/mustr`: bin. Bootstrap, copied zed helpers, menus, keymap, settings overrides, window options.
- `mustr-crates/mustr_ui`: `MustrTitleBar` (PR tabs), status items (key hints, connection), `MustrNavPanel` (rail, inbox, file tree), `InspectorPanel`, theme.
- `mustr-crates/review`: `ReviewView` Item, `ReviewFile`, buffer/diff loading (virtual or worktree), `ReviewAddon` (viewed checkbox, statuses), `MustrHunkRenderer`, thread-card block manager.
- `mustr-crates/thread_chat`, `inbox`: views over `t3_client` data.
- `mustr-crates/t3_client`: already exists.

**First M0 spike:** a `#[gpui::test]` in `review` that builds a `ReviewView` from two in-memory `(path, base, head)` triples, modeled on `multi_diff_view.rs` `test_init`. It should assert the hunks, the header path, and that the base buffer has the language set. This catches upstream API drift early.

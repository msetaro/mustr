//! Edit: Zed's editor on one folder at a time. The sidebar has the folder's file tree, under a
//! picker of T3's projects whose folders are on this machine, recent folders, and Open Folder…;
//! the title bar has a tab per open file. ⌘P finds a file and ⌘⇧F searches, in that folder.
//!
//! mustr's window has one Zed project, which reviews add their worktrees to as well, so Edit
//! keeps to its folder itself: its own file tree ([`EditTree`]) and file finder
//! ([`EditFileFinder`]) list that folder alone, and project search is limited to it. Zed's
//! project panel would list every worktree. A file opened from another folder (an agent
//! message's link, Open in mustr's editor) makes that folder Edit's.

use crate::areas::{self, Area, Areas, ViewKind};
use anyhow::anyhow;
use collections::HashSet;
use file_icons::FileIcons;
use fuzzy_nucleo::PathMatch;
use gpui::{
    Action, App, AppContext as _, Context, DismissEvent, Entity, EntityId, EventEmitter,
    FocusHandle, Focusable, Global, PathPromptOptions, Subscription, Task, UniformListScrollHandle,
    WeakEntity, uniform_list,
};
use mustr_ui::{tokens, tokens::scaled};
use picker::{Picker, PickerDelegate};
use project::{Project, ProjectPath, Worktree, WorktreeId};
use schemars::JsonSchema;
use serde::Deserialize;
use std::{
    path::{Path, PathBuf},
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
};
use ui::{
    ContextMenu, HighlightedLabel, ListItem, ListItemSpacing, PopoverMenu, Tooltip, prelude::*,
};
use util::rel_path::RelPath;
use workspace::{
    ModalView, Workspace,
    item::{Item, ItemEvent},
};

/// Opens a folder in Edit: shows it in the file tree, and Edit.
#[derive(Clone, Debug, PartialEq, Eq, Deserialize, JsonSchema, Action)]
#[action(namespace = mustr)]
pub struct OpenFolderInEdit {
    pub path: PathBuf,
}

gpui::actions!(
    mustr,
    [
        /// Picks a folder to open in Edit, with the system's folder picker.
        OpenFolderForEditing,
    ]
);

const LAST_FOLDER_KEY: &str = "mustr-edit-folder";
const RECENT_FOLDERS_KEY: &str = "mustr-edit-recent-folders";
const MAX_RECENT_FOLDERS: usize = 8;

/// Edit's folder in a window, and the folders it had.
pub struct EditFolder {
    project: Entity<Project>,
    /// The folder chosen, which may still be opening.
    folder: Option<PathBuf>,
    worktree: Option<Entity<Worktree>>,
    error: Option<SharedString>,
    recent: Vec<PathBuf>,
    /// Whether the folder of last time was opened, which waits until Edit shows.
    restored: bool,
    _opening: Option<Task<()>>,
}

#[derive(Default)]
struct Registry(collections::HashMap<EntityId, WeakEntity<EditFolder>>);

impl Global for Registry {}

impl EditFolder {
    /// Edit's folder in the workspace's window, making it the first time.
    pub fn ensure(workspace: &mut Workspace, cx: &mut Context<Workspace>) -> Entity<Self> {
        if let Some(folder) = Self::for_workspace(cx.entity_id(), cx) {
            return folder;
        }
        let project = workspace.project().clone();
        let recent = read_recent_folders(cx);
        let folder = cx.new(|_| Self {
            project,
            folder: None,
            worktree: None,
            error: None,
            recent,
            restored: false,
            _opening: None,
        });
        let workspace_id = cx.entity_id();
        cx.default_global::<Registry>()
            .0
            .insert(workspace_id, folder.downgrade());
        folder
    }

    pub fn for_workspace(workspace_id: EntityId, cx: &App) -> Option<Entity<Self>> {
        cx.try_global::<Registry>()?.0.get(&workspace_id)?.upgrade()
    }

    pub fn folder(&self) -> Option<&Path> {
        self.folder.as_deref()
    }

    pub fn worktree(&self) -> Option<&Entity<Worktree>> {
        self.worktree.as_ref()
    }

    pub fn error(&self) -> Option<&SharedString> {
        self.error.as_ref()
    }

    pub fn recent(&self) -> &[PathBuf] {
        &self.recent
    }

    /// Opens the folder of last time, once.
    pub fn restore(&mut self, cx: &mut Context<Self>) {
        if std::mem::replace(&mut self.restored, true) || self.folder.is_some() {
            return;
        }
        if let Some(path) = read_kvp(LAST_FOLDER_KEY, cx).map(PathBuf::from)
            && path.is_dir()
        {
            self.open(path, cx);
        }
    }

    /// Makes `path` Edit's folder: adds it to the project, visible so Zed's Restricted Mode
    /// applies, as reviews add theirs.
    pub fn open(&mut self, path: PathBuf, cx: &mut Context<Self>) {
        self.restored = true;
        if self.folder.as_deref() == Some(path.as_path()) && self.worktree.is_some() {
            return;
        }
        self.folder = Some(path.clone());
        self.worktree = None;
        self.error = None;
        self.recent.retain(|recent| recent != &path);
        self.recent.insert(0, path.clone());
        self.recent.truncate(MAX_RECENT_FOLDERS);
        write_kvp(LAST_FOLDER_KEY, path.to_string_lossy().into_owned(), cx);
        if let Ok(recent) = serde_json::to_string(&self.recent) {
            write_kvp(RECENT_FOLDERS_KEY, recent, cx);
        }
        let add = self.project.update(cx, |project, cx| {
            project.find_or_create_worktree(&path, true, cx)
        });
        self._opening = Some(cx.spawn(async move |this, cx| {
            let result = add.await;
            this.update(cx, |this, cx| {
                if this.folder.as_deref() != Some(path.as_path()) {
                    return;
                }
                match result {
                    Ok((worktree, _)) => this.worktree = Some(worktree),
                    Err(error) => {
                        this.error =
                            Some(format!("Couldn't open {}: {error:#}", path.display()).into())
                    }
                }
                cx.notify();
            })
            .ok();
        }));
        cx.notify();
    }

    /// Takes the folder of a file opened from elsewhere (an agent message's link, a review):
    /// the directory worktree it's in, unless that's Edit's folder already.
    fn follow_file(&mut self, worktree: Entity<Worktree>, cx: &mut Context<Self>) {
        if self.worktree.as_ref() == Some(&worktree) {
            return;
        }
        let (is_dir, path) = {
            let worktree = worktree.read(cx);
            (
                !worktree.is_single_file() && worktree.is_visible(),
                worktree.abs_path().to_path_buf(),
            )
        };
        if !is_dir {
            return;
        }
        self.open(path, cx);
    }

    fn worktree_id(&self, cx: &App) -> Option<WorktreeId> {
        Some(self.worktree.as_ref()?.read(cx).id())
    }

    /// The folder's name, as project search's filters name it.
    fn root_name(&self, cx: &App) -> Option<String> {
        Some(self.worktree.as_ref()?.read(cx).root_name_str().to_string())
    }
}

impl EventEmitter<()> for EditFolder {}

fn read_kvp(key: &str, cx: &App) -> Option<String> {
    if !cx.has_global::<db::AppDatabase>() {
        return None;
    }
    db::kvp::KeyValueStore::global(cx)
        .read_kvp(key)
        .ok()
        .flatten()
}

fn write_kvp(key: &'static str, value: String, cx: &mut App) {
    if !cx.has_global::<db::AppDatabase>() {
        return;
    }
    let store = db::kvp::KeyValueStore::global(cx);
    cx.background_spawn(async move { store.write_kvp(key.to_string(), value).await })
        .detach_and_log_err(cx);
}

fn read_recent_folders(cx: &App) -> Vec<PathBuf> {
    read_kvp(RECENT_FOLDERS_KEY, cx)
        .and_then(|recent| serde_json::from_str::<Vec<PathBuf>>(&recent).ok())
        .unwrap_or_default()
}

/// `path` as a folder of this machine, when T3's folders are here and it's there: what Open in
/// mustr's editor opens.
pub fn local_folder(path: &str, cx: &App) -> Option<PathBuf> {
    let path = PathBuf::from(path);
    (crate::worktrees::worktrees_are_local(cx) && path.is_dir()).then_some(path)
}

/// A folder the picker offers.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct FolderChoice {
    pub label: String,
    pub path: PathBuf,
    /// A T3 project's folder, rather than a recent one.
    pub is_project: bool,
}

/// What the folder picker lists: T3's projects whose folders are on this machine, then the
/// recent folders that aren't one of them.
pub fn folder_choices(recent: &[PathBuf], cx: &mut App) -> Vec<FolderChoice> {
    let mut choices = Vec::new();
    let shells = thread_chat::ThreadShells::global(cx);
    if crate::worktrees::worktrees_are_local(cx)
        && let Some(shell) = shells.read(cx).shell()
    {
        let mut projects = shell
            .projects
            .values()
            .map(|project| {
                (
                    project.title.clone(),
                    PathBuf::from(&project.workspace_root),
                )
            })
            .filter(|(_, path)| path.is_dir())
            .collect::<Vec<_>>();
        projects.sort_by_cached_key(|(title, _)| title.to_lowercase());
        choices.extend(projects.into_iter().map(|(label, path)| FolderChoice {
            label,
            path,
            is_project: true,
        }));
    }
    for path in recent {
        if choices.iter().any(|choice| &choice.path == path) || !path.is_dir() {
            continue;
        }
        choices.push(FolderChoice {
            label: path
                .file_name()
                .map(|name| name.to_string_lossy().into_owned())
                .unwrap_or_else(|| path.display().to_string()),
            path: path.clone(),
            is_project: false,
        });
    }
    choices
}

/// Registers Edit's actions on a window's workspace, and takes over Zed's file finder and
/// project search while Edit shows. Call from `observe_new` on workspaces.
pub fn register(workspace: &mut Workspace, cx: &mut Context<Workspace>) {
    let edit_folder = EditFolder::ensure(workspace, cx);
    workspace.register_action(|workspace, action: &OpenFolderInEdit, window, cx| {
        open_folder(action.path.clone(), workspace, window, cx)
    });
    workspace.register_action(|workspace, _: &OpenFolderForEditing, window, cx| {
        choose_folder(workspace, window, cx)
    });
    let weak_folder = edit_folder.downgrade();
    let workspace_handle = cx.entity().downgrade();
    workspace.register_action_renderer(move |div, _, _, _| {
        let in_edit = {
            let workspace = workspace_handle.clone();
            move |cx: &App| {
                workspace.upgrade().is_some_and(|workspace| {
                    Areas::for_workspace(workspace.entity_id(), cx)
                        .is_some_and(|areas| areas.read(cx).area() == Area::Edit)
                })
            }
        };
        let finder = {
            let in_edit = in_edit.clone();
            let workspace = workspace_handle.clone();
            div.capture_action(move |_: &workspace::ToggleFileFinder, window, cx| {
                if !in_edit(cx) {
                    return;
                }
                cx.stop_propagation();
                workspace
                    .update(cx, |workspace, cx| {
                        EditFileFinder::toggle(workspace, window, cx)
                    })
                    .ok();
            })
        };
        let folder = weak_folder.clone();
        finder.capture_action(move |action: &workspace::DeploySearch, window, cx| {
            if !in_edit(cx) || action.included_files.is_some() {
                return;
            }
            let Some(root_name) = folder
                .upgrade()
                .and_then(|folder| folder.read(cx).root_name(cx))
            else {
                return;
            };
            cx.stop_propagation();
            window.dispatch_action(
                Box::new(workspace::DeploySearch {
                    included_files: Some(root_name),
                    ..action.clone()
                }),
                cx,
            );
        })
    });
    // A file opened from another folder makes that folder Edit's.
    let weak_folder = edit_folder.downgrade();
    cx.subscribe(&cx.entity(), move |workspace, _, event, cx| {
        if !matches!(event, workspace::Event::ActiveItemChanged) {
            return;
        }
        let Some(item) = workspace.active_item(cx) else {
            return;
        };
        if ViewKind::of(item.as_ref()) != ViewKind::File {
            return;
        }
        let Some(project_path) = item.project_path(cx) else {
            return;
        };
        let worktree = workspace
            .project()
            .read(cx)
            .worktree_for_id(project_path.worktree_id, cx);
        if let (Some(worktree), Some(folder)) = (worktree, weak_folder.upgrade()) {
            folder.update(cx, |folder, cx| folder.follow_file(worktree, cx));
        }
    })
    .detach();
}

/// Makes `path` Edit's folder, and shows Edit.
pub fn open_folder(
    path: PathBuf,
    workspace: &mut Workspace,
    window: &mut Window,
    cx: &mut Context<Workspace>,
) {
    let folder = EditFolder::ensure(workspace, cx);
    folder.update(cx, |folder, cx| folder.open(path, cx));
    areas::show_area(Area::Edit, workspace, window, cx);
}

/// Asks for a folder with the system's folder picker, and opens it in Edit.
pub fn choose_folder(_: &mut Workspace, window: &mut Window, cx: &mut Context<Workspace>) {
    let paths = cx.prompt_for_paths(PathPromptOptions {
        files: false,
        directories: true,
        multiple: false,
        prompt: Some("Open".into()),
    });
    cx.spawn_in(window, async move |workspace, cx| {
        let path = match paths.await {
            Ok(Ok(Some(paths))) => paths.into_iter().next(),
            Ok(Ok(None)) | Err(_) => None,
            Ok(Err(error)) => return Err(anyhow!("Couldn't open a folder picker: {error:#}")),
        };
        if let Some(path) = path {
            workspace.update_in(cx, |workspace, window, cx| {
                open_folder(path, workspace, window, cx)
            })?;
        }
        anyhow::Ok(())
    })
    .detach_and_log_err(cx);
}

/// The folder picker's menu: T3's projects here, recent folders, and Open Folder….
fn folder_menu(
    folder: WeakEntity<EditFolder>,
    window: &mut Window,
    cx: &mut App,
) -> Option<Entity<ContextMenu>> {
    let folder = folder.upgrade()?;
    let current = folder.read(cx).folder().map(Path::to_path_buf);
    let recent = folder.read(cx).recent().to_vec();
    let choices = folder_choices(&recent, cx);
    Some(ContextMenu::build(window, cx, move |mut menu, _, _| {
        let (projects, recent): (Vec<_>, Vec<_>) =
            choices.into_iter().partition(|choice| choice.is_project);
        for (header, choices) in [("T3 projects", projects), ("Recent", recent)] {
            if choices.is_empty() {
                continue;
            }
            menu = menu.header(header);
            for choice in choices {
                let checked = current.as_deref() == Some(choice.path.as_path());
                let path = choice.path.clone();
                menu = menu.toggleable_entry(
                    choice.label.clone(),
                    checked,
                    IconPosition::End,
                    None,
                    move |window, cx| {
                        window
                            .dispatch_action(Box::new(OpenFolderInEdit { path: path.clone() }), cx)
                    },
                );
            }
            menu = menu.separator();
        }
        menu.action("Open Folder…", Box::new(OpenFolderForEditing))
    }))
}

fn folder_label(folder: &EditFolder) -> SharedString {
    folder
        .folder()
        .map(|path| {
            path.file_name()
                .map(|name| name.to_string_lossy().into_owned())
                .unwrap_or_else(|| path.display().to_string())
        })
        .unwrap_or_else(|| "Choose a folder".to_string())
        .into()
}

/// The picker's button: the folder's name, opening the menu.
fn folder_picker(id: &'static str, folder: &Entity<EditFolder>, cx: &App) -> impl IntoElement {
    let label = folder_label(folder.read(cx));
    let tooltip = folder
        .read(cx)
        .folder()
        .map(|path| path.display().to_string())
        .unwrap_or_else(|| "Pick a folder to edit".to_string());
    let weak = folder.downgrade();
    PopoverMenu::new(id)
        .trigger_with_tooltip(
            Button::new(SharedString::from(format!("{id}-button")), label)
                .style(ButtonStyle::Subtle)
                .end_icon(
                    Icon::new(IconName::ChevronDown)
                        .size(IconSize::XSmall)
                        .color(Color::Muted),
                ),
            Tooltip::text(tooltip),
        )
        .anchor(gpui::Anchor::TopLeft)
        .menu(move |window, cx| folder_menu(weak.clone(), window, cx))
}

// The file tree.

/// Adds `dir`'s entries to `rows`, in the order Zed's project panel sorts them (directories
/// first), each expanded directory's entries after it.
fn push_children(
    worktree: &Worktree,
    dir: &RelPath,
    depth: usize,
    expanded: &HashSet<Arc<RelPath>>,
    rows: &mut Vec<Row>,
) {
    let mut children = worktree
        .child_entries(dir)
        .filter(|entry| entry.path.file_name() != Some(".git"))
        .collect::<Vec<_>>();
    children.sort_by(|left, right| {
        util::paths::compare_rel_paths((&left.path, left.is_file()), (&right.path, right.is_file()))
    });
    for entry in children {
        let is_expanded = entry.is_dir() && expanded.contains(&entry.path);
        rows.push(Row {
            name: entry
                .path
                .file_name()
                .unwrap_or_default()
                .to_string()
                .into(),
            path: entry.path.clone(),
            depth,
            is_dir: entry.is_dir(),
            expanded: is_expanded,
            ignored: entry.is_ignored,
            entry_id: entry.id,
        });
        if is_expanded {
            push_children(worktree, &entry.path, depth + 1, expanded, rows);
        }
    }
}

#[derive(Clone, Debug)]
struct Row {
    path: Arc<RelPath>,
    name: SharedString,
    depth: usize,
    is_dir: bool,
    expanded: bool,
    ignored: bool,
    entry_id: project::ProjectEntryId,
}

/// Edit's sidebar: the folder picker, and the folder's files.
pub struct EditTree {
    focus_handle: FocusHandle,
    workspace: WeakEntity<Workspace>,
    folder: Entity<EditFolder>,
    expanded: HashSet<Arc<RelPath>>,
    rows: Vec<Row>,
    /// The file the active editor shows, in the folder.
    selected: Option<Arc<RelPath>>,
    scroll: UniformListScrollHandle,
    /// The worktree the rows are of.
    showing: Option<EntityId>,
    _subscriptions: Vec<Subscription>,
}

impl EditTree {
    /// Doesn't read `workspace`, so it can be called while the workspace is being updated.
    pub fn new(
        workspace: &Entity<Workspace>,
        folder: Entity<EditFolder>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        let project = folder.read(cx).project.clone();
        let subscriptions = vec![
            cx.observe(&folder, |this, _, cx| this.folder_changed(cx)),
            // The folder's files as Zed scans them, and as they change.
            cx.subscribe(&project, |this, _, event: &project::Event, cx| {
                if let project::Event::WorktreeUpdatedEntries(worktree_id, _) = event
                    && this.folder.read(cx).worktree_id(cx) == Some(*worktree_id)
                {
                    this.rebuild(cx);
                }
            }),
            cx.subscribe_in(
                workspace,
                window,
                |this, workspace, event: &workspace::Event, _, cx| {
                    if let workspace::Event::ActiveItemChanged = event {
                        let path = workspace
                            .read(cx)
                            .active_item(cx)
                            .and_then(|item| item.project_path(cx));
                        this.reveal(path, cx);
                    }
                },
            ),
        ];
        let mut this = Self {
            focus_handle: cx.focus_handle(),
            workspace: workspace.downgrade(),
            folder,
            expanded: HashSet::default(),
            rows: Vec::new(),
            selected: None,
            scroll: UniformListScrollHandle::new(),
            showing: None,
            _subscriptions: subscriptions,
        };
        this.folder_changed(cx);
        this
    }

    #[cfg(test)]
    pub fn folder_path(&self, cx: &App) -> Option<PathBuf> {
        self.folder.read(cx).folder().map(Path::to_path_buf)
    }

    /// Opens the folder of last time, when Edit first shows.
    pub fn shown(&mut self, cx: &mut Context<Self>) {
        self.folder.update(cx, |folder, cx| folder.restore(cx));
    }

    fn worktree(&self, cx: &App) -> Option<Entity<Worktree>> {
        self.folder.read(cx).worktree().cloned()
    }

    fn folder_changed(&mut self, cx: &mut Context<Self>) {
        let worktree_id = self.worktree(cx).map(|worktree| worktree.entity_id());
        if worktree_id != self.showing {
            self.showing = worktree_id;
            self.expanded.clear();
            self.selected = None;
        }
        self.rebuild(cx);
    }

    /// Lists the folder's entries, the expanded directories' children under them.
    fn rebuild(&mut self, cx: &mut Context<Self>) {
        self.rows.clear();
        if let Some(worktree) = self.worktree(cx) {
            let worktree = worktree.read(cx);
            push_children(
                worktree,
                RelPath::empty(),
                0,
                &self.expanded,
                &mut self.rows,
            );
        }
        cx.notify();
    }

    /// Selects the active editor's file, expanding the directories it's in.
    fn reveal(&mut self, path: Option<ProjectPath>, cx: &mut Context<Self>) {
        let Some(worktree_id) = self.folder.read(cx).worktree_id(cx) else {
            return;
        };
        let Some(path) = path.filter(|path| path.worktree_id == worktree_id) else {
            return;
        };
        let mut parent = path.path.parent();
        while let Some(dir) = parent {
            if !dir.is_empty() {
                self.expanded.insert(dir.into_arc());
            }
            parent = dir.parent();
        }
        self.selected = Some(path.path.clone());
        self.rebuild(cx);
        if let Some(index) = self.rows.iter().position(|row| row.path == path.path) {
            self.scroll
                .scroll_to_item(index, gpui::ScrollStrategy::Center);
        }
    }

    fn click(&mut self, index: usize, window: &mut Window, cx: &mut Context<Self>) {
        let Some(row) = self.rows.get(index).cloned() else {
            return;
        };
        let Some(worktree) = self.worktree(cx) else {
            return;
        };
        let worktree_id = worktree.read(cx).id();
        if row.is_dir {
            if !self.expanded.remove(&row.path) {
                self.expanded.insert(row.path.clone());
                // Zed loads ignored directories' entries when they're expanded.
                let project = self.folder.read(cx).project.clone();
                if let Some(task) = project.update(cx, |project, cx| {
                    project.expand_entry(worktree_id, row.entry_id, cx)
                }) {
                    task.detach_and_log_err(cx);
                }
            }
            self.rebuild(cx);
            return;
        }
        self.selected = Some(row.path.clone());
        let path = ProjectPath {
            worktree_id,
            path: row.path,
        };
        self.workspace
            .update(cx, |workspace, cx| {
                workspace
                    .open_path(path, None, true, window, cx)
                    .detach_and_log_err(cx);
            })
            .ok();
        cx.notify();
    }

    fn render_row(&self, index: usize, row: &Row, cx: &mut Context<Self>) -> AnyElement {
        let palette = mustr_ui::palette(cx);
        let selected = self.selected.as_ref() == Some(&row.path);
        let icon = if row.is_dir {
            FileIcons::get_folder_icon(row.expanded, row.path.as_std_path(), cx)
        } else {
            FileIcons::get_icon(row.path.as_std_path(), cx)
        };
        let color = if row.ignored {
            Color::Disabled
        } else {
            Color::Default
        };
        h_flex()
            .id(("edit-tree-row", index))
            .w_full()
            .h(scaled(px(24.)))
            .pl(scaled(px(8. + 12. * row.depth as f32)))
            .pr_2()
            .gap_1p5()
            .rounded(scaled(tokens::CONTROL_RADIUS))
            .cursor_pointer()
            .when(selected, |this| this.bg(palette.surface_2.hsla()))
            .when(!selected, |this| {
                this.hover(|style| style.bg(palette.surface_2.hsla().opacity(0.6)))
            })
            .on_click(cx.listener(move |this, _, window, cx| this.click(index, window, cx)))
            .child(
                div()
                    .w(scaled(px(12.)))
                    .flex_none()
                    .when(row.is_dir, |this| {
                        this.child(
                            Icon::new(if row.expanded {
                                IconName::ChevronDown
                            } else {
                                IconName::ChevronRight
                            })
                            .size(IconSize::XSmall)
                            .color(Color::Muted),
                        )
                    }),
            )
            .children(icon.map(|icon| {
                Icon::from_path(icon)
                    .size(IconSize::Small)
                    .color(Color::Muted)
            }))
            .child(
                Label::new(row.name.clone())
                    .size(LabelSize::Small)
                    .color(color)
                    .truncate(),
            )
            .into_any_element()
    }
}

impl Focusable for EditTree {
    fn focus_handle(&self, _: &App) -> FocusHandle {
        self.focus_handle.clone()
    }
}

impl Render for EditTree {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let palette = mustr_ui::palette(cx);
        let folder = self.folder.read(cx);
        let body = if let Some(error) = folder.error().cloned() {
            note(error, Color::Error)
        } else if folder.folder().is_none() {
            note("Pick a folder above to see its files.", Color::Muted)
        } else if folder.worktree().is_none() {
            note("Opening…", Color::Muted)
        } else if self.rows.is_empty() {
            note("No files.", Color::Muted)
        } else {
            let rows = self.rows.clone();
            uniform_list(
                "edit-tree-rows",
                rows.len(),
                cx.processor(move |this, range: std::ops::Range<usize>, _, cx| {
                    range
                        .filter_map(|index| Some(this.render_row(index, rows.get(index)?, cx)))
                        .collect()
                }),
            )
            .flex_1()
            .px_1()
            .track_scroll(&self.scroll)
            .into_any_element()
        };
        v_flex()
            .key_context("MustrEditTree")
            .track_focus(&self.focus_handle)
            .size_full()
            .bg(palette.surface.hsla())
            .border_r(tokens::BORDER_WIDTH)
            .border_color(palette.border.hsla())
            .child(
                h_flex()
                    .flex_none()
                    .gap_1()
                    .pl_1()
                    .pr_2()
                    .py(scaled(px(6.)))
                    .child(div().flex_1().min_w_0().child(folder_picker(
                        "edit-folder-picker",
                        &self.folder,
                        cx,
                    )))
                    .child(
                        IconButton::new("edit-find-file", IconName::MagnifyingGlass)
                            .icon_size(IconSize::Small)
                            .tooltip(Tooltip::for_action_title(
                                "Find a file",
                                &workspace::ToggleFileFinder::default(),
                            ))
                            .on_click(|_, window, cx| {
                                window.dispatch_action(
                                    Box::new(workspace::ToggleFileFinder::default()),
                                    cx,
                                )
                            }),
                    ),
            )
            .child(body)
    }
}

fn note(text: impl Into<SharedString>, color: Color) -> AnyElement {
    div()
        .px_3()
        .py_2()
        .child(Label::new(text.into()).size(LabelSize::Small).color(color))
        .into_any_element()
}

// What Edit shows with no file open.

/// Edit's page with no file open: the folder, and how to open a file.
pub struct EditHome {
    focus_handle: FocusHandle,
    folder: Entity<EditFolder>,
    _folder: Subscription,
}

/// Shows Edit's page, making it the first time.
pub fn show_home(workspace: &mut Workspace, window: &mut Window, cx: &mut Context<Workspace>) {
    let existing = workspace
        .panes()
        .iter()
        .find_map(|pane| pane.read(cx).items_of_type::<EditHome>().next());
    if let Some(existing) = existing {
        workspace.activate_item(&existing, true, true, window, cx);
        return;
    }
    let folder = EditFolder::ensure(workspace, cx);
    let view = cx.new(|cx| EditHome {
        focus_handle: cx.focus_handle(),
        _folder: cx.observe(&folder, |_, _, cx| cx.notify()),
        folder,
    });
    workspace.add_item_to_active_pane(Box::new(view), None, true, window, cx);
}

impl EventEmitter<ItemEvent> for EditHome {}

impl Focusable for EditHome {
    fn focus_handle(&self, _: &App) -> FocusHandle {
        self.focus_handle.clone()
    }
}

impl Item for EditHome {
    type Event = ItemEvent;

    fn tab_content_text(&self, _: usize, _: &App) -> SharedString {
        "Edit".into()
    }

    fn to_item_events(event: &Self::Event, f: &mut dyn FnMut(ItemEvent)) {
        f(*event)
    }
}

impl Render for EditHome {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let palette = mustr_ui::palette(cx);
        let recent = self.folder.read(cx).recent().to_vec();
        let choices = folder_choices(&recent, cx);
        let folder = self.folder.read(cx);
        let has_folder = folder.folder().is_some();
        let path = folder
            .folder()
            .map(|path| path.display().to_string())
            .unwrap_or_default();
        let action_row = |id: &'static str, label: &'static str, action: Box<dyn Action>| {
            let tooltip_action = action.boxed_clone();
            Button::new(id, label)
                .style(ButtonStyle::Subtle)
                .key_binding(None)
                .tooltip(move |_, cx| Tooltip::for_action(label, tooltip_action.as_ref(), cx))
                .on_click(move |_, window, cx| window.dispatch_action(action.boxed_clone(), cx))
        };
        v_flex()
            .key_context("MustrEditHome")
            .track_focus(&self.focus_handle)
            .size_full()
            .items_center()
            .justify_center()
            .gap(scaled(px(12.)))
            .bg(palette.background.hsla())
            .child(Headline::new(folder_label(folder)).size(HeadlineSize::Medium))
            .when(has_folder, |this| {
                this.child(Label::new(path).size(LabelSize::Small).color(Color::Muted))
                    .child(
                        h_flex()
                            .gap_2()
                            .child(action_row(
                                "edit-home-find-file",
                                "Find a file",
                                Box::new(workspace::ToggleFileFinder::default()),
                            ))
                            .child(action_row(
                                "edit-home-search",
                                "Search the folder",
                                Box::new(workspace::DeploySearch::default()),
                            )),
                    )
            })
            .when(!has_folder, |this| {
                this.child(
                    Label::new("Pick a folder to edit: one of T3's projects, or any folder.")
                        .color(Color::Muted),
                )
                .child(v_flex().w(scaled(px(320.))).gap_0p5().children(
                    choices.into_iter().enumerate().map(|(index, choice)| {
                        let path = choice.path.clone();
                        ListItem::new(("edit-home-choice", index))
                            .spacing(ListItemSpacing::Sparse)
                            .start_slot(
                                Icon::new(if choice.is_project {
                                    IconName::Folder
                                } else {
                                    IconName::HistoryRerun
                                })
                                .size(IconSize::Small)
                                .color(Color::Muted),
                            )
                            .child(Label::new(choice.label).truncate())
                            .on_click(move |_, window, cx| {
                                window.dispatch_action(
                                    Box::new(OpenFolderInEdit { path: path.clone() }),
                                    cx,
                                )
                            })
                    }),
                ))
                .child(
                    Button::new("edit-home-open-folder", "Open Folder…")
                        .style(ButtonStyle::Filled)
                        .on_click(|_, window, cx| {
                            window.dispatch_action(Box::new(OpenFolderForEditing), cx)
                        }),
                )
            })
    }
}

// The file finder.

/// ⌘P in Edit: finds a file in Edit's folder.
pub struct EditFileFinder {
    picker: Entity<Picker<FileFinderDelegate>>,
    _dismiss: Subscription,
}

impl EditFileFinder {
    pub fn toggle(workspace: &mut Workspace, window: &mut Window, cx: &mut Context<Workspace>) {
        let Some(worktree) = EditFolder::for_workspace(cx.entity_id(), cx)
            .and_then(|folder| folder.read(cx).worktree().cloned())
        else {
            // No folder yet: Edit's page offers the folders.
            areas::show_root(Area::Edit, workspace, window, cx);
            return;
        };
        let weak_workspace = workspace.weak_handle();
        workspace.toggle_modal(window, cx, move |window, cx| {
            let delegate = FileFinderDelegate {
                workspace: weak_workspace,
                worktree,
                matches: Vec::new(),
                selected: 0,
                cancel: Arc::new(AtomicBool::new(false)),
            };
            let picker = cx.new(|cx| Picker::uniform_list(delegate, window, cx));
            // The picker's Escape and choosing a file close the finder.
            let dismiss = cx.subscribe(&picker, |_, _, _: &DismissEvent, cx| cx.emit(DismissEvent));
            Self {
                picker,
                _dismiss: dismiss,
            }
        });
    }
}

impl EventEmitter<DismissEvent> for EditFileFinder {}

impl ModalView for EditFileFinder {}

impl Focusable for EditFileFinder {
    fn focus_handle(&self, cx: &App) -> FocusHandle {
        self.picker.focus_handle(cx)
    }
}

impl Render for EditFileFinder {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        v_flex()
            .key_context("MustrEditFileFinder")
            .child(self.picker.clone())
            .on_mouse_down_out(cx.listener(|_, _, _, cx| cx.emit(DismissEvent)))
    }
}

pub struct FileFinderDelegate {
    workspace: WeakEntity<Workspace>,
    worktree: Entity<Worktree>,
    matches: Vec<PathMatch>,
    selected: usize,
    cancel: Arc<AtomicBool>,
}

impl PickerDelegate for FileFinderDelegate {
    type ListItem = ListItem;

    fn name() -> &'static str {
        "mustr edit file finder"
    }

    fn match_count(&self) -> usize {
        self.matches.len()
    }

    fn selected_index(&self) -> usize {
        self.selected
    }

    fn set_selected_index(&mut self, ix: usize, _: &mut Window, _: &mut Context<Picker<Self>>) {
        self.selected = ix;
    }

    fn placeholder_text(&self, _: &mut Window, cx: &mut App) -> Arc<str> {
        format!("Find a file in {}…", self.worktree.read(cx).root_name_str()).into()
    }

    fn no_matches_text(&self, _: &mut Window, _: &mut App) -> Option<SharedString> {
        Some("No files match.".into())
    }

    fn update_matches(
        &mut self,
        query: String,
        window: &mut Window,
        cx: &mut Context<Picker<Self>>,
    ) -> Task<()> {
        self.cancel.store(true, Ordering::Release);
        self.cancel = Arc::new(AtomicBool::new(false));
        let cancel = self.cancel.clone();
        let snapshot = self.worktree.read(cx).snapshot();
        let query = query.trim().to_string();
        cx.spawn_in(window, async move |picker, cx| {
            let sets = [project::PathMatchCandidateSet {
                snapshot,
                include_ignored: false,
                include_root_name: false,
                candidates: project::Candidates::Files,
            }];
            let executor = cx.background_executor().clone();
            let matches = fuzzy_nucleo::match_path_sets(
                &sets,
                &query,
                &None,
                fuzzy_nucleo::Case::smart_if_uppercase_in(&query),
                100,
                &cancel,
                executor,
            )
            .await;
            if cancel.load(Ordering::Acquire) {
                return;
            }
            picker
                .update(cx, |picker, cx| {
                    picker.delegate.matches = matches;
                    picker.delegate.selected = 0;
                    cx.notify();
                })
                .ok();
        })
    }

    fn confirm(&mut self, _: bool, window: &mut Window, cx: &mut Context<Picker<Self>>) {
        let Some(found) = self.matches.get(self.selected) else {
            return;
        };
        let path = ProjectPath {
            worktree_id: self.worktree.read(cx).id(),
            path: found.path.clone(),
        };
        self.workspace
            .update(cx, |workspace, cx| {
                workspace
                    .open_path(path, None, true, window, cx)
                    .detach_and_log_err(cx);
            })
            .ok();
        cx.emit(DismissEvent);
    }

    fn dismissed(&mut self, _: &mut Window, cx: &mut Context<Picker<Self>>) {
        cx.emit(DismissEvent);
    }

    fn render_match(
        &self,
        ix: usize,
        selected: bool,
        _: &mut Window,
        cx: &mut Context<Picker<Self>>,
    ) -> Option<Self::ListItem> {
        let found = self.matches.get(ix)?;
        let full = found.path.as_unix_str().to_string();
        let name_start = full.rfind('/').map_or(0, |slash| slash + 1);
        let name = full[name_start..].to_string();
        let name_positions = found
            .positions
            .iter()
            .filter(|position| **position >= name_start)
            .map(|position| position - name_start)
            .collect::<Vec<_>>();
        let dir = full[..name_start].trim_end_matches('/').to_string();
        let icon = FileIcons::get_icon(found.path.as_std_path(), cx);
        Some(
            ListItem::new(ix)
                .inset(true)
                .spacing(ListItemSpacing::Sparse)
                .toggle_state(selected)
                .start_slot::<Icon>(icon.map(|icon| {
                    Icon::from_path(icon)
                        .size(IconSize::Small)
                        .color(Color::Muted)
                }))
                .child(
                    h_flex()
                        .gap_2()
                        .min_w_0()
                        .child(HighlightedLabel::new(name, name_positions))
                        .child(
                            Label::new(dir)
                                .size(LabelSize::Small)
                                .color(Color::Muted)
                                .truncate(),
                        ),
                ),
        )
    }
}

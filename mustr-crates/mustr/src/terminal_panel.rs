//! Agent threads' terminals: a panel in the bottom dock with the active thread's own terminals,
//! opened in its worktree (or its project's checkout), like T3's terminal drawer
//! (`components/ThreadTerminalDrawer.tsx`, `components/PersistentThreadTerminals.tsx`).
//!
//! - Each thread keeps its terminals while mustr runs. Switching threads shows the other
//!   thread's terminals, and switching back finds them as they were.
//! - Whether the panel shows is kept per thread too, as T3 keeps its drawer open per thread: it
//!   opens for a thread whose terminals were showing and closes for one whose weren't. Other
//!   items, such as home or a review, leave it as it is.
//! - When T3 runs on this machine, the terminals are Zed's (`terminal_view`), started in the
//!   thread's folder. When it runs on another machine (or `MUSTR_IGNORE_LOCAL_WORKTREES` is set),
//!   they're T3's, on that machine (`remote_terminal.rs`), in the same tabs with the same keys.
//!   A thread's terminals that T3 still runs show again when the panel first shows the thread.
//! - The panel is created synchronously with the workspace (`window.rs`), so dock restoration
//!   finds it. Zed's own terminal panel loads asynchronously, which is why mustr doesn't use it.
//!   Terminals start later, when a thread first wants one.
//!
//! [`OpenSetupTerminal`] (the worktree setup card's Open terminal) shows the terminal T3 runs a
//! thread's setup script in: T3's own terminal, on whichever machine T3 runs.
//!
//! [`OpenThreadTerminal`] (the thread toolbar's Terminal button) opens a thread's terminals,
//! [`ToggleThreadTerminal`] (`secondary-j`, as in T3, and Zed's `ctrl-\``) shows or hides the
//! active thread's, and [`NewThreadTerminal`] adds one. [`AddTerminalSelectionToChat`] (the
//! header's Add to chat, or `secondary->`) puts the selected lines in the thread's composer as a
//! terminal chip (`thread_chat::AddComposerContext`), as T3's terminal does.
//!
//! The header's Run ▾ runs the project's scripts (T3's `ProjectScriptsControl.tsx`): the
//! project's own (as T3's settings resolve them, `project_scripts.rs`) and those in the thread
//! folder's `t3.json` (`projects.readFile`), each in a new terminal, typed with a space first as
//! T3 does. It adds, edits (the pencil) and removes the project's scripts, and adds `t3.json`'s
//! to them (the plus), as T3's menu does.

use crate::{
    project_scripts::{ProjectScripts, ScriptEditor, ScriptIcon, ScriptInput, saved_scripts},
    remote_terminal::{self, Launch, RemoteTerminal, RemoteTerminalEvent},
    thread_git::{MenuSlot, menu_position},
    worktrees::{display_path, worktrees_are_local},
};
use gpui::{
    Action, AnyElement, AnyView, App, AppContext as _, Context, Entity, EntityId, EventEmitter,
    FocusHandle, Focusable, FontWeight, KeyBinding, Pixels, SharedString, Subscription, Task,
    WeakEntity, Window, actions,
};
use mustr_ui::{TextButton, WindowTab, kbds_for_action, palette, tokens, tokens::scaled};
use project::Project;
use std::{
    collections::HashMap,
    path::{Path, PathBuf},
};
use t3_client::t3_api::{
    rpc,
    types::{
        OrchestrationProjectShell, OrchestrationV2ThreadShell, ProjectReadFileInput, ProjectScript,
        TerminalSessionStatus, TerminalSummary,
    },
};
use terminal_view::TerminalView;
use thread_chat::{
    Access, AddComposerContext, ComposerContext, OpenSetupTerminal, OpenThreadTerminal,
    ThreadBackendExt as _, ThreadShells, ThreadView, actions::TerminalContext,
};
use ui::{ContextMenu, Tooltip, prelude::*};
use workspace::{
    ItemHandle, Workspace,
    dock::{DockPosition, Panel, PanelEvent},
    item::ItemEvent,
    pane::CloseActiveItem,
};

actions!(
    mustr,
    [
        /// Shows the active agent thread's terminals, opening one in its worktree if it has
        /// none, or hides them.
        ToggleThreadTerminal,
        /// Opens another terminal for the agent thread the terminal panel shows.
        NewThreadTerminal,
        /// Closes the terminal the terminal panel shows.
        CloseThreadTerminal,
        /// Adds the lines selected in the thread's terminal to its composer, for the next
        /// message.
        AddTerminalSelectionToChat,
    ]
);

const KEY_CONTEXT: &str = "ThreadTerminals";

/// The panel's height at mustr's design size, in a window tall enough for it.
const DEFAULT_HEIGHT: Pixels = px(300.);

/// The most of the window's height the panel takes when it first opens, so the thread above it
/// keeps its timeline and composer: at mustr's default size (26), 300 is 600 pixels, more than
/// half of a MacBook's window.
const MAX_HEIGHT_SHARE: f32 = 0.4;

/// The panel's height when it first opens in a window `window_height` tall: `scaled_height`
/// ([`DEFAULT_HEIGHT`], scaled), or [`MAX_HEIGHT_SHARE`] of the window if that's less.
fn default_height(window_height: Pixels, scaled_height: Pixels) -> Pixels {
    scaled_height.min(window_height * MAX_HEIGHT_SHARE)
}

pub fn init(cx: &mut App) {
    cx.observe_new(|workspace: &mut Workspace, _, _| {
        workspace.register_action(|workspace, action: &OpenThreadTerminal, window, cx| {
            open_terminals(workspace, Some(action.thread_id.clone()), window, cx);
        });
        workspace.register_action(|workspace, _: &ToggleThreadTerminal, window, cx| {
            toggle_terminals(workspace, window, cx);
        });
        workspace.register_action(|workspace, action: &OpenSetupTerminal, window, cx| {
            open_setup_terminal(workspace, action, window, cx);
        });
        workspace.register_action(|workspace, _: &NewThreadTerminal, window, cx| {
            open_terminals(workspace, None, window, cx);
            if let Some(panel) = workspace.panel::<ThreadTerminalPanel>(cx) {
                panel.update(cx, |panel, cx| {
                    if panel.has_terminals() {
                        panel.new_terminal(window, cx);
                    }
                });
            }
        });
    })
    .detach();
}

/// The terminal panel's key bindings. `secondary-j` is T3's, and replaces Zed's bottom-dock
/// toggle, since the panel is the bottom dock's only one; `ctrl-\`` is Zed's terminal toggle.
pub fn key_bindings() -> Vec<KeyBinding> {
    vec![
        KeyBinding::new("ctrl-`", ToggleThreadTerminal, Some("Workspace")),
        KeyBinding::new("secondary-j", ToggleThreadTerminal, Some("Workspace")),
        // In the terminal, Linux's `ctrl-w` still goes to the shell: Zed's `Terminal` context
        // binds it, and it's nested deeper than the panel's.
        KeyBinding::new("secondary-w", CloseThreadTerminal, Some(KEY_CONTEXT)),
        // Zed's key for adding a selection to its agent, which mustr leaves out.
        KeyBinding::new("secondary->", AddTerminalSelectionToChat, Some(KEY_CONTEXT)),
    ]
}

/// Adds the terminal panel to a new workspace. It must run while the workspace is set up, before
/// dock restoration finishes (`window.rs`); it doesn't read the workspace entity.
pub fn add_panel(
    workspace: &mut Workspace,
    workspace_handle: &Entity<Workspace>,
    window: &mut Window,
    cx: &mut Context<Workspace>,
) {
    let project = workspace.project().clone();
    let panel = cx.new(|cx| ThreadTerminalPanel::new(workspace_handle, project, window, cx));
    workspace.add_panel(panel, window, cx);
}

/// Shows a thread's terminals (the active thread's, or the one the panel shows, when
/// `thread_id` is `None`), starting one if it has none, and focuses them.
fn open_terminals(
    workspace: &mut Workspace,
    thread_id: Option<String>,
    window: &mut Window,
    cx: &mut Context<Workspace>,
) {
    let Some(panel) = workspace.panel::<ThreadTerminalPanel>(cx) else {
        return;
    };
    let thread_id = thread_id.or_else(|| active_thread_id(workspace, cx));
    panel.update(cx, |panel, cx| {
        if let Some(thread_id) = thread_id {
            panel.show_thread(thread_id, window, cx);
        }
        panel.ensure_terminal(window, cx);
        // A terminal that's already open gets focus below; one that's starting, when it starts.
        panel.focus_on_start = !panel.has_terminals();
    });
    // Opening the dock calls `set_active`, which records that this thread's terminals show.
    workspace.focus_panel::<ThreadTerminalPanel>(window, cx);
}

/// Shows the terminal T3 runs a thread's setup script in, attaching to it the first time.
fn open_setup_terminal(
    workspace: &mut Workspace,
    action: &OpenSetupTerminal,
    window: &mut Window,
    cx: &mut Context<Workspace>,
) {
    let Some(panel) = workspace.panel::<ThreadTerminalPanel>(cx) else {
        return;
    };
    panel.update(cx, |panel, cx| {
        panel.show_thread(action.thread_id.clone(), window, cx);
        panel.show_t3_terminal(action, window, cx);
    });
    workspace.focus_panel::<ThreadTerminalPanel>(window, cx);
}

/// Hides the terminals when they have focus, and otherwise shows and focuses them, as Zed's
/// terminal toggle does.
fn toggle_terminals(workspace: &mut Workspace, window: &mut Window, cx: &mut Context<Workspace>) {
    let Some(panel) = workspace.panel::<ThreadTerminalPanel>(cx) else {
        return;
    };
    let showing = panel_is_showing(workspace, &panel, cx);
    if showing && panel.focus_handle(cx).contains_focused(window, cx) {
        workspace.close_panel::<ThreadTerminalPanel>(window, cx);
        workspace.focus_center_pane(window, cx);
    } else {
        open_terminals(workspace, None, window, cx);
    }
}

fn panel_is_showing(workspace: &Workspace, panel: &Entity<ThreadTerminalPanel>, cx: &App) -> bool {
    let dock = workspace.bottom_dock().read(cx);
    dock.is_open()
        && dock
            .visible_panel()
            .is_some_and(|visible| visible.panel_id() == panel.entity_id())
}

fn active_thread_id(workspace: &Workspace, cx: &App) -> Option<String> {
    let thread = workspace.active_item(cx)?.downcast::<ThreadView>()?;
    thread.read(cx).thread_id().map(str::to_string)
}

/// Where a thread's terminals start.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum TerminalLocation {
    /// A folder on this machine: the thread's worktree, or its project's checkout.
    Local(PathBuf),
    /// T3's threads haven't loaded yet.
    Loading,
    /// T3's shell doesn't have the thread, so its worktree isn't known: it's archived or gone.
    UnknownThread,
    /// T3 runs on another machine, where the folder is: its terminals run there.
    Remote(RemoteFolder),
    /// T3 runs here, but the folder is gone. `checkout` is the project's, if it's still here.
    Missing {
        path: String,
        checkout: Option<PathBuf>,
    },
}

/// A thread's folder on T3's machine, where its terminals start: its worktree, or its project's
/// checkout (`projectScriptCwd` in T3's `packages/shared/src/projectScripts.ts`).
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct RemoteFolder {
    pub cwd: String,
    pub worktree_path: Option<String>,
    pub project_root: String,
}

pub(crate) fn terminal_location(
    thread: &OrchestrationV2ThreadShell,
    project_root: Option<&str>,
    on_this_machine: bool,
    is_dir: impl Fn(&Path) -> bool,
) -> TerminalLocation {
    let root = project_root.map(str::trim).filter(|root| !root.is_empty());
    if !on_this_machine {
        let worktree_path =
            crate::worktrees::normalize_worktree_path(thread.worktree_path.as_deref())
                .map(str::to_string);
        let project_root = root.unwrap_or_default().to_string();
        let cwd = worktree_path
            .clone()
            .unwrap_or_else(|| project_root.clone());
        if cwd.is_empty() {
            return TerminalLocation::Missing {
                path: String::new(),
                checkout: None,
            };
        }
        return TerminalLocation::Remote(RemoteFolder {
            cwd,
            worktree_path,
            project_root,
        });
    }
    let checkout = root.map(PathBuf::from).filter(|root| is_dir(root));
    match crate::worktrees::normalize_worktree_path(thread.worktree_path.as_deref()) {
        Some(path) if is_dir(Path::new(path)) => TerminalLocation::Local(PathBuf::from(path)),
        Some(path) => TerminalLocation::Missing {
            path: path.to_string(),
            checkout,
        },
        None => match checkout {
            Some(checkout) => TerminalLocation::Local(checkout),
            None => TerminalLocation::Missing {
                path: root.unwrap_or_default().to_string(),
                checkout: None,
            },
        },
    }
}

/// A terminal the panel hosts: Zed's on this machine, or one on T3's machine.
struct HostedTerminal {
    view: Entity<TerminalView>,
    remote: Option<Entity<RemoteTerminal>>,
    /// Where it started, as the tab's tooltip shows it.
    cwd: SharedString,
    title: SharedString,
    _subscriptions: Vec<Subscription>,
}

impl HostedTerminal {
    fn content(&self) -> AnyView {
        match &self.remote {
            Some(remote) => remote.clone().into(),
            None => self.view.clone().into(),
        }
    }

    fn title(&self, cx: &App) -> SharedString {
        match &self.remote {
            Some(remote) => remote.read(cx).title(cx),
            None => terminal_title(&self.view, cx),
        }
    }

    fn remote_id<'a>(&self, cx: &'a App) -> Option<&'a str> {
        let remote = self.remote.as_ref()?;
        Some(remote.read(cx).launch().terminal_id.as_str())
    }
}

/// One thread's terminals.
#[derive(Default)]
struct ThreadTerminals {
    terminals: Vec<HostedTerminal>,
    active: usize,
    /// Whether the panel showed while this thread was the one it showed.
    open: bool,
    /// A terminal starting.
    starting: Option<Task<()>>,
    /// Why the last terminal didn't start.
    error: Option<SharedString>,
    /// Where terminals start instead of the thread's folder, which is gone.
    cwd_override: Option<PathBuf>,
    /// A terminal waits for T3's threads to load, to know where to start.
    waiting_for_shell: bool,
    /// Whether the panel has looked for the terminals T3 runs for this thread on its machine.
    found_remote: bool,
    /// A script's command for the next terminal that starts.
    pending_command: Option<String>,
    /// The scripts in the thread folder's `t3.json`, once read.
    file_scripts: Option<Vec<Script>>,
    _read_scripts: Option<Task<()>>,
}

impl ThreadTerminals {
    fn active(&self) -> Option<&HostedTerminal> {
        self.terminals.get(self.active)
    }
}

pub struct ThreadTerminalPanel {
    focus_handle: FocusHandle,
    workspace: WeakEntity<Workspace>,
    project: Entity<Project>,
    /// The thread whose terminals show: the active thread, or the last one that was.
    thread_id: Option<String>,
    threads: HashMap<String, ThreadTerminals>,
    /// Whether the next terminal that starts for the shown thread takes focus.
    focus_on_start: bool,
    /// T3's threads, for where terminals start and the thread's title, once a thread is shown.
    shells: Option<(Entity<ThreadShells>, Subscription)>,
    /// The active thread view, whose thread changes when a draft is started.
    _active_thread: Option<Subscription>,
    /// The Run ▾ menu.
    menu: MenuSlot,
    /// T3's settings, for which scripts apply, once a thread is shown.
    scripts: Option<(Entity<ProjectScripts>, Subscription)>,
    _subscriptions: Vec<Subscription>,
}

impl ThreadTerminalPanel {
    /// Doesn't read `workspace`, so it can be called while the workspace is being updated.
    fn new(
        workspace: &Entity<Workspace>,
        project: Entity<Project>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        let subscription = cx.subscribe_in(
            workspace,
            window,
            |this, workspace, event: &workspace::Event, window, cx| {
                if let workspace::Event::ActiveItemChanged = event {
                    let item = workspace.read(cx).active_item(cx);
                    this.active_item_changed(item.as_deref(), window, cx);
                }
            },
        );
        Self {
            focus_handle: cx.focus_handle(),
            workspace: workspace.downgrade(),
            project,
            thread_id: None,
            threads: HashMap::new(),
            focus_on_start: false,
            shells: None,
            _active_thread: None,
            menu: MenuSlot::default(),
            scripts: None,
            _subscriptions: vec![subscription],
        }
    }

    fn active_item_changed(
        &mut self,
        item: Option<&dyn ItemHandle>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let thread = item.and_then(|item| item.downcast::<ThreadView>());
        self._active_thread = thread.as_ref().map(|thread| {
            cx.observe_in(thread, window, |this, thread, window, cx| {
                let thread_id = thread.read(cx).thread_id().map(str::to_string);
                this.thread_activated(thread_id, window, cx);
            })
        });
        // Other items leave the terminals as they are.
        if let Some(thread) = thread {
            let thread_id = thread.read(cx).thread_id().map(str::to_string);
            self.thread_activated(thread_id, window, cx);
        }
    }

    /// Shows the newly active thread's terminals, and shows or hides the panel as it was for
    /// that thread.
    fn thread_activated(
        &mut self,
        thread_id: Option<String>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        // A draft has no worktree yet.
        let Some(thread_id) = thread_id else {
            return;
        };
        if self.thread_id.as_deref() == Some(thread_id.as_str()) {
            return;
        }
        let open = self
            .threads
            .get(&thread_id)
            .is_some_and(|terminals| terminals.open);
        self.show_thread(thread_id, window, cx);
        // After this update: opening or closing the dock calls back into the panel.
        let workspace = self.workspace.clone();
        let panel = cx.entity();
        window.defer(cx, move |window, cx| {
            workspace
                .update(cx, |workspace, cx| {
                    let showing = panel_is_showing(workspace, &panel, cx);
                    if open && !showing {
                        workspace.open_panel::<Self>(window, cx);
                    } else if !open && showing {
                        workspace.close_panel::<Self>(window, cx);
                    }
                })
                .ok();
        });
    }

    fn show_thread(&mut self, thread_id: String, window: &mut Window, cx: &mut Context<Self>) {
        self.follow_shell(window, cx);
        let scripts = self
            .scripts
            .get_or_insert_with(|| {
                let store = ProjectScripts::global(cx);
                let subscription = cx.observe(&store, |_, _, cx| cx.notify());
                (store, subscription)
            })
            .0
            .clone();
        scripts.update(cx, |scripts, cx| scripts.ensure_loaded(cx));
        self.threads.entry(thread_id.clone()).or_default();
        self.thread_id = Some(thread_id.clone());
        self.read_file_scripts(thread_id, cx);
        cx.notify();
    }

    /// The thread's folder on T3's machine (or this one): where its scripts run, and where its
    /// `t3.json` is.
    fn script_folder(&self, thread_id: &str, cx: &App) -> Option<String> {
        let (shells, _) = self.shells.as_ref()?;
        let shell = shells.read(cx).shell()?;
        let thread = shell.threads.get(thread_id)?;
        crate::worktrees::normalize_worktree_path(thread.worktree_path.as_deref())
            .map(str::to_string)
            .or_else(|| {
                let project = shell.projects.get(&thread.project_id)?;
                Some(project.workspace_root.clone()).filter(|root| !root.trim().is_empty())
            })
    }

    /// The thread's project, from T3's shell.
    fn thread_project(&self, thread_id: &str, cx: &App) -> Option<OrchestrationProjectShell> {
        let (shells, _) = self.shells.as_ref()?;
        let shell = shells.read(cx).shell()?;
        let thread = shell.threads.get(thread_id)?;
        shell.projects.get(&thread.project_id).cloned()
    }

    /// The project's own scripts, as T3's settings resolve them (the shell's until they load).
    fn project_scripts(&self, thread_id: &str, cx: &App) -> Vec<Script> {
        let Some(project) = self.thread_project(thread_id, cx) else {
            return Vec::new();
        };
        let scripts = match &self.scripts {
            Some((store, _)) => store.read(cx).scripts(&project),
            None => project.scripts.clone(),
        };
        scripts.iter().map(Script::from_project).collect()
    }

    /// Opens the editor of one of the project's scripts, or of a new one.
    fn edit_script(
        &mut self,
        script: Option<ProjectScript>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(project) = self
            .thread_id
            .as_deref()
            .and_then(|thread_id| self.thread_project(thread_id, cx))
        else {
            return;
        };
        self.workspace
            .update(cx, |workspace, cx| {
                workspace.toggle_modal(window, cx, |window, cx| {
                    ScriptEditor::new(project, script.as_ref(), window, cx)
                })
            })
            .ok();
    }

    /// Adds one of `t3.json`'s scripts to the project's.
    fn import_script(&mut self, script: &Script, cx: &mut Context<Self>) {
        let (Some(project), Some(store)) = (
            self.thread_id
                .as_deref()
                .and_then(|thread_id| self.thread_project(thread_id, cx)),
            self.scripts.as_ref().map(|(store, _)| store.clone()),
        ) else {
            return;
        };
        let input = ScriptInput {
            name: script.name.clone(),
            command: script.command.clone(),
            icon: ScriptIcon::Play,
            run_on_worktree_create: script.setup,
            wait_for_setup: false,
            run_on_settle: script.on_settle,
        };
        let next = saved_scripts(&store.read(cx).scripts(&project), None, &input);
        let save = store.update(cx, |store, cx| store.save(&project.id, next, cx));
        cx.spawn(async move |_, _| {
            if let Err(error) = save.await {
                log::error!("couldn't add the action from t3.json: {error:#}");
            }
        })
        .detach();
    }

    /// Reads the thread folder's `t3.json` once, when the panel first shows the thread.
    fn read_file_scripts(&mut self, thread_id: String, cx: &mut Context<Self>) {
        let Some(terminals) = self.threads.get(&thread_id) else {
            return;
        };
        if terminals.file_scripts.is_some() || terminals._read_scripts.is_some() {
            return;
        }
        let (Some(backend), Some(cwd)) =
            (thread_chat::backend(cx), self.script_folder(&thread_id, cx))
        else {
            return;
        };
        let read = backend.call::<rpc::ProjectsReadFile>(
            ProjectReadFileInput {
                cwd,
                relative_path: T3_PROJECT_FILE.to_string(),
                allow_missing: Some(true),
            },
            Access::FilesystemRead,
        );
        let key = thread_id.clone();
        let task = cx.spawn(async move |this, cx| {
            let scripts = match read.await {
                Ok(file) if !file.truncated && file.exists != Some(false) => {
                    file_scripts(&file.contents)
                }
                Ok(_) => Vec::new(),
                Err(error) => {
                    log::info!("couldn't read {T3_PROJECT_FILE}: {error:#}");
                    Vec::new()
                }
            };
            this.update(cx, |this, cx| {
                if let Some(terminals) = this.threads.get_mut(&thread_id) {
                    terminals.file_scripts = Some(scripts);
                    terminals._read_scripts = None;
                    cx.notify();
                }
            })
            .ok();
        });
        if let Some(terminals) = self.threads.get_mut(&key) {
            terminals._read_scripts = Some(task);
        }
    }

    /// The scripts Run ▾ offers: the project's, then `t3.json`'s that the project doesn't have
    /// (by command or name, as T3 compares them).
    pub(crate) fn scripts(&self, cx: &App) -> (Vec<Script>, Vec<Script>) {
        let Some(thread_id) = self.thread_id.as_deref() else {
            return Default::default();
        };
        let project = self.project_scripts(thread_id, cx);
        let file = self
            .threads
            .get(thread_id)
            .and_then(|terminals| terminals.file_scripts.clone())
            .unwrap_or_default()
            .into_iter()
            .filter(|script| {
                !project.iter().any(|known| {
                    known.command == script.command || known.name.eq_ignore_ascii_case(&script.name)
                })
            })
            .collect();
        (project, file)
    }

    /// Runs a script in a new terminal of the shown thread.
    pub(crate) fn run_script(
        &mut self,
        command: String,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(thread_id) = self.thread_id.clone() else {
            return;
        };
        let terminals = self.threads.entry(thread_id.clone()).or_default();
        if terminals.starting.is_some() {
            return;
        }
        terminals.pending_command = Some(command);
        terminals.error = None;
        self.focus_on_start = true;
        self.start_terminal(thread_id, window, cx);
        let workspace = self.workspace.clone();
        window.defer(cx, move |window, cx| {
            workspace
                .update(cx, |workspace, cx| workspace.open_panel::<Self>(window, cx))
                .ok();
        });
    }

    fn deploy_scripts_menu(
        &mut self,
        position: gpui::Point<Pixels>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let (project, file) = self.scripts(cx);
        let editable = self
            .scripts
            .as_ref()
            .is_some_and(|(store, _)| store.read(cx).blocked_reason(cx).is_none());
        let this = cx.weak_entity();
        let menu = ContextMenu::build(window, cx, move |mut menu, _, _| {
            for script in &project {
                let run = this.clone();
                let command = script.command.clone();
                let label = script.menu_label();
                let run_it = move |window: &mut Window, cx: &mut App| {
                    run.update(cx, |this, cx| this.run_script(command.clone(), window, cx))
                        .ok();
                };
                menu = match script.source.clone().filter(|_| editable) {
                    Some(source) => {
                        let edit = this.clone();
                        menu.entry_with_end_slot_on_hover(
                            label,
                            None,
                            run_it,
                            IconName::Pencil,
                            "Edit action".into(),
                            move |window, cx| {
                                let source = source.clone();
                                edit.update(cx, |this, cx| {
                                    this.edit_script(Some(source), window, cx)
                                })
                                .ok();
                            },
                        )
                    }
                    None => menu.entry(label, None, run_it),
                };
            }
            if !file.is_empty() {
                if !project.is_empty() {
                    menu = menu.separator();
                }
                menu = menu.header(format!("From {T3_PROJECT_FILE}"));
                for script in &file {
                    let run = this.clone();
                    let command = script.command.clone();
                    let run_it = move |window: &mut Window, cx: &mut App| {
                        run.update(cx, |this, cx| this.run_script(command.clone(), window, cx))
                            .ok();
                    };
                    menu = if editable {
                        let import = this.clone();
                        let script = script.clone();
                        menu.entry_with_end_slot_on_hover(
                            script.menu_label(),
                            None,
                            run_it,
                            IconName::Plus,
                            "Add to the project's actions".into(),
                            move |_, cx| {
                                import
                                    .update(cx, |this, cx| this.import_script(&script, cx))
                                    .ok();
                            },
                        )
                    } else {
                        menu.entry(script.menu_label(), None, run_it)
                    };
                }
            }
            if editable {
                let add = this.clone();
                menu = menu
                    .separator()
                    .entry("Add action…", None, move |window, cx| {
                        add.update(cx, |this, cx| this.edit_script(None, window, cx))
                            .ok();
                    });
            }
            menu
        });
        self.menu
            .deploy(position, menu, window, cx, |this| &mut this.menu);
    }

    fn follow_shell(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.shells.is_some() {
            return;
        }
        let shells = ThreadShells::global(cx);
        let subscription = cx.observe_in(&shells, window, |this, _, window, cx| {
            this.shell_changed(window, cx)
        });
        self.shells = Some((shells, subscription));
    }

    /// Starts the terminal that waited for T3's threads, once they've loaded.
    fn shell_changed(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        cx.notify();
        let Some(thread_id) = self.thread_id.clone() else {
            return;
        };
        // Its `t3.json` can be read once the shell says where its folder is.
        self.read_file_scripts(thread_id.clone(), cx);
        let waiting = self
            .threads
            .get(&thread_id)
            .is_some_and(|terminals| terminals.waiting_for_shell);
        if !waiting || self.location(&thread_id, cx) == TerminalLocation::Loading {
            return;
        }
        if let Some(terminals) = self.threads.get_mut(&thread_id) {
            terminals.waiting_for_shell = false;
        }
        self.start_terminal(thread_id, window, cx);
    }

    fn location(&self, thread_id: &str, cx: &App) -> TerminalLocation {
        let Some((shells, _)) = &self.shells else {
            return TerminalLocation::Loading;
        };
        let Some(shell) = shells.read(cx).shell() else {
            return TerminalLocation::Loading;
        };
        let Some(thread) = shell.threads.get(thread_id) else {
            return TerminalLocation::UnknownThread;
        };
        let root = shell
            .projects
            .get(&thread.project_id)
            .map(|project| project.workspace_root.as_str());
        terminal_location(thread, root, worktrees_are_local(cx), Path::is_dir)
    }

    fn thread_title(&self, thread_id: &str, cx: &App) -> Option<SharedString> {
        let (shells, _) = self.shells.as_ref()?;
        let thread = shells.read(cx).thread(thread_id)?;
        let title = thread.title.trim();
        Some(
            if title.is_empty() {
                "Untitled thread"
            } else {
                title
            }
            .to_string()
            .into(),
        )
    }

    fn shown(&self) -> Option<&ThreadTerminals> {
        self.threads.get(self.thread_id.as_deref()?)
    }

    fn has_terminals(&self) -> bool {
        self.shown()
            .is_some_and(|terminals| !terminals.terminals.is_empty())
    }

    /// Starts a terminal for the shown thread if it has none and none is starting.
    fn ensure_terminal(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(thread_id) = self.thread_id.clone() else {
            return;
        };
        let terminals = self.threads.entry(thread_id.clone()).or_default();
        if terminals.terminals.is_empty()
            && terminals.starting.is_none()
            && terminals.error.is_none()
        {
            self.start_terminal(thread_id, window, cx);
        }
    }

    fn new_terminal(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if let Some(thread_id) = self.thread_id.clone() {
            self.focus_on_start = true;
            self.start_terminal(thread_id, window, cx);
        }
    }

    fn start_terminal(&mut self, thread_id: String, window: &mut Window, cx: &mut Context<Self>) {
        let location = self.location(&thread_id, cx);
        let terminals = self.threads.entry(thread_id.clone()).or_default();
        if terminals.starting.is_some() {
            return;
        }
        let cwd = match (terminals.cwd_override.clone(), location) {
            (Some(cwd), _) | (None, TerminalLocation::Local(cwd)) => cwd,
            (None, TerminalLocation::Remote(folder)) => {
                return self.start_remote_terminals(thread_id, folder, window, cx);
            }
            (None, TerminalLocation::Loading) => {
                terminals.waiting_for_shell = true;
                self.follow_shell(window, cx);
                cx.notify();
                return;
            }
            // The panel says why there's no terminal.
            (None, _) => {
                cx.notify();
                return;
            }
        };
        terminals.error = None;
        let create = self.project.update(cx, |project, cx| {
            project.create_terminal_shell(Some(cwd.clone()), cx)
        });
        terminals.starting = Some(cx.spawn_in(window, async move |this, cx| {
            let terminal = create.await;
            this.update_in(cx, |this, window, cx| {
                let workspace = this.workspace.clone();
                let project = this.project.downgrade();
                let view = terminal.map(|terminal| {
                    cx.new(|cx| TerminalView::new(terminal, workspace, None, project, window, cx))
                });
                let Some(terminals) = this.threads.get_mut(&thread_id) else {
                    return;
                };
                terminals.starting = None;
                let command = terminals.pending_command.take();
                match view {
                    Ok(view) => {
                        if let Some(command) = command {
                            let terminal = view.read(cx).terminal().clone();
                            terminal.update(cx, |terminal, _| {
                                terminal.write_init_command(format!(" {command}\r").into_bytes())
                            });
                        }
                        let cwd = display_path(&cwd).into();
                        this.add_terminal(&thread_id, view, None, cwd, window, cx)
                    }
                    Err(error) => {
                        log::error!("couldn't start a terminal in {}: {error:#}", cwd.display());
                        terminals.error = Some(
                            format!(
                                "Couldn't start a terminal in {}: {error:#}",
                                display_path(&cwd)
                            )
                            .into(),
                        );
                    }
                }
                cx.notify();
            })
            .ok();
        }));
        cx.notify();
    }

    /// Starts a terminal on T3's machine. The first time the panel shows the thread, it shows
    /// the terminals T3 still runs for it instead, if there are any, as T3's drawer does.
    fn start_remote_terminals(
        &mut self,
        thread_id: String,
        folder: RemoteFolder,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(backend) = thread_chat::backend(cx) else {
            return;
        };
        let size = self.grid_size(cx);
        let Some(terminals) = self.threads.get_mut(&thread_id) else {
            return;
        };
        terminals.error = None;
        let command = terminals.pending_command.take();
        let reattach = !std::mem::replace(&mut terminals.found_remote, true);
        let shown_ids = terminals
            .terminals
            .iter()
            .filter_map(|hosted| hosted.remote_id(cx).map(str::to_string))
            .collect::<Vec<_>>();
        let listed = remote_terminal::thread_terminals(backend, thread_id.clone(), cx);
        terminals.starting = Some(cx.spawn_in(window, async move |this, cx| {
            let mut launches = match listed.await {
                Ok(listed) => {
                    let mut launches =
                        remote_launches(&thread_id, &folder, &listed, &shown_ids, reattach);
                    // A script runs in a terminal of its own, after the ones T3 still runs.
                    if command.is_some()
                        && launches.iter().any(|launch| {
                            listed
                                .iter()
                                .any(|listed| listed.terminal_id == launch.terminal_id)
                        })
                    {
                        let ids = listed
                            .iter()
                            .map(|terminal| terminal.terminal_id.as_str())
                            .chain(shown_ids.iter().map(String::as_str));
                        launches.push(
                            folder.launch(&thread_id, remote_terminal::next_terminal_id(ids, None)),
                        );
                    }
                    launches
                }
                Err(error) => {
                    // Without T3's list, a suffix keeps the id from reaching a terminal mustr
                    // can't see, as T3's app does.
                    log::warn!("couldn't list T3's terminals: {error:#}");
                    let suffix = uuid::Uuid::new_v4().to_string();
                    let ids = shown_ids.iter().map(String::as_str);
                    let terminal_id = remote_terminal::next_terminal_id(ids, Some(&suffix));
                    vec![folder.launch(&thread_id, terminal_id)]
                }
            };
            let command_for = command.map(|command| {
                (
                    launches.last().map(|launch| launch.terminal_id.clone()),
                    command,
                )
            });
            let mut error = None;
            for launch in launches.drain(..) {
                let run = command_for
                    .as_ref()
                    .filter(|(terminal_id, _)| terminal_id.as_ref() == Some(&launch.terminal_id))
                    .map(|(_, command)| command.clone());
                let Ok(open) = this.update_in(cx, |this, window, cx| {
                    RemoteTerminal::open(
                        launch,
                        size,
                        this.workspace.clone(),
                        &this.project,
                        window,
                        cx,
                    )
                }) else {
                    return;
                };
                match open.await {
                    Ok(remote) => this
                        .update_in(cx, |this, window, cx| {
                            if let Some(command) = &run {
                                remote.update(cx, |remote, cx| remote.run_command(command, cx));
                            }
                            this.add_remote_terminal(&thread_id, remote, window, cx)
                        })
                        .ok(),
                    Err(open_error) => {
                        log::error!("couldn't start a terminal on T3's machine: {open_error:#}");
                        error = Some(open_error);
                        None
                    }
                };
            }
            this.update(cx, |this, cx| {
                let Some(terminals) = this.threads.get_mut(&thread_id) else {
                    return;
                };
                terminals.starting = None;
                if let Some(error) = error
                    && terminals.terminals.is_empty()
                {
                    terminals.error = Some(
                        format!("Couldn't start a terminal on T3's machine: {error:#}").into(),
                    );
                }
                cx.notify();
            })
            .ok();
        }));
        cx.notify();
    }

    /// Shows one of T3's terminals for the thread the panel shows, such as its setup script's:
    /// the tab already showing it, or a new one attached to it.
    fn show_t3_terminal(
        &mut self,
        action: &OpenSetupTerminal,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let thread_id = action.thread_id.clone();
        let shown = self.threads.get(&thread_id).and_then(|terminals| {
            terminals
                .terminals
                .iter()
                .position(|hosted| hosted.remote_id(cx) == Some(action.terminal_id.as_str()))
        });
        if let Some(index) = shown {
            self.activate_terminal(index, window, cx);
            return;
        }
        let project_root = match self.location(&thread_id, cx) {
            TerminalLocation::Remote(folder) => folder.project_root,
            TerminalLocation::Local(path) => path.to_string_lossy().to_string(),
            _ => String::new(),
        };
        let project_root = self
            .shells
            .as_ref()
            .and_then(|(shells, _)| {
                let shell = shells.read(cx).shell()?;
                let thread = shell.threads.get(&thread_id)?;
                Some(
                    shell
                        .projects
                        .get(&thread.project_id)?
                        .workspace_root
                        .clone(),
                )
            })
            .unwrap_or(project_root);
        let cwd = action.cwd.clone().unwrap_or_else(|| project_root.clone());
        let launch = Launch {
            thread_id: thread_id.clone(),
            terminal_id: action.terminal_id.clone(),
            worktree_path: action.cwd.clone(),
            cwd,
            project_root,
        };
        let size = self.grid_size(cx);
        let open = RemoteTerminal::open(
            launch,
            size,
            self.workspace.clone(),
            &self.project,
            window,
            cx,
        );
        self.focus_on_start = true;
        let terminals = self.threads.entry(thread_id.clone()).or_default();
        terminals.error = None;
        terminals.starting = Some(cx.spawn_in(window, async move |this, cx| {
            let result = open.await;
            this.update_in(cx, |this, window, cx| {
                match result {
                    Ok(remote) => this.add_remote_terminal(&thread_id, remote, window, cx),
                    Err(error) => {
                        log::error!("couldn't show T3's setup terminal: {error:#}");
                        if let Some(terminals) = this.threads.get_mut(&thread_id) {
                            terminals.error = Some(
                                format!("Couldn't show the setup script's terminal: {error:#}")
                                    .into(),
                            );
                        }
                    }
                }
                if let Some(terminals) = this.threads.get_mut(&thread_id) {
                    terminals.starting = None;
                }
                cx.notify();
            })
            .ok();
        }));
        cx.notify();
    }

    /// The grid size of a terminal the panel shows, for a new one to start at.
    fn grid_size(&self, cx: &App) -> Option<(i64, i64)> {
        let hosted = self.shown().and_then(ThreadTerminals::active).or_else(|| {
            self.threads
                .values()
                .find_map(|terminals| terminals.active())
        })?;
        let bounds = hosted
            .view
            .read(cx)
            .terminal()
            .read(cx)
            .last_content()
            .terminal_bounds;
        let size = (bounds.num_columns() as i64, bounds.num_lines() as i64);
        (size.0 > 1 && size.1 > 1).then_some(size)
    }

    fn add_remote_terminal(
        &mut self,
        thread_id: &str,
        remote: Entity<RemoteTerminal>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let view = remote.read(cx).view().clone();
        let cwd = remote.read(cx).launch().cwd.clone().into();
        self.add_terminal(thread_id, view, Some(remote), cwd, window, cx);
    }

    fn add_terminal(
        &mut self,
        thread_id: &str,
        view: Entity<TerminalView>,
        remote: Option<Entity<RemoteTerminal>>,
        cwd: SharedString,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let terminal = view.read(cx).terminal().clone();
        let mut subscriptions = vec![
            cx.subscribe_in(
                &view,
                window,
                |this, view, event: &ItemEvent, window, cx| match event {
                    // The shell exited.
                    ItemEvent::CloseItem => this.remove_terminal(view.entity_id(), window, cx),
                    ItemEvent::UpdateTab => this.update_title(view, cx),
                    _ => {}
                },
            ),
            // The header's Add to chat follows the selection.
            cx.subscribe(&terminal, |_, _, event: &terminal::Event, cx| {
                if let terminal::Event::SelectionsChanged = event {
                    cx.notify();
                }
            }),
        ];
        if let Some(remote) = &remote {
            let view = view.clone();
            subscriptions.push(cx.subscribe_in(
                remote,
                window,
                move |this, _, event: &RemoteTerminalEvent, window, cx| match event {
                    RemoteTerminalEvent::TitleChanged => this.update_title(&view, cx),
                    RemoteTerminalEvent::Ended => {
                        this.remove_terminal(view.entity_id(), window, cx)
                    }
                },
            ));
        }
        let mut hosted = HostedTerminal {
            view: view.clone(),
            remote,
            cwd,
            title: SharedString::default(),
            _subscriptions: subscriptions,
        };
        hosted.title = hosted.title(cx);
        let Some(terminals) = self.threads.get_mut(thread_id) else {
            return;
        };
        terminals.terminals.push(hosted);
        terminals.active = terminals.terminals.len() - 1;
        if self.focus_on_start && self.thread_id.as_deref() == Some(thread_id) {
            self.focus_on_start = false;
            view.focus_handle(cx).focus(window, cx);
        }
        cx.notify();
    }

    fn update_title(&mut self, view: &Entity<TerminalView>, cx: &mut Context<Self>) {
        let hosted = self
            .threads
            .values_mut()
            .flat_map(|terminals| terminals.terminals.iter_mut())
            .find(|hosted| hosted.view == *view);
        if let Some(hosted) = hosted {
            let title = hosted.title(cx);
            if hosted.title != title {
                hosted.title = title;
                cx.notify();
            }
        }
    }

    fn activate_terminal(&mut self, index: usize, window: &mut Window, cx: &mut Context<Self>) {
        let Some(thread_id) = self.thread_id.clone() else {
            return;
        };
        let Some(terminals) = self.threads.get_mut(&thread_id) else {
            return;
        };
        if let Some(hosted) = terminals.terminals.get(index) {
            terminals.active = index;
            hosted.view.focus_handle(cx).focus(window, cx);
            cx.notify();
        }
    }

    fn close_active_terminal(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if let Some(id) = self
            .shown()
            .and_then(ThreadTerminals::active)
            .map(|hosted| hosted.view.entity_id())
        {
            self.close_terminal(id, window, cx);
        }
    }

    /// Closes a terminal the user closed. One on T3's machine is ended there too; a local one
    /// ends when it's dropped.
    fn close_terminal(&mut self, id: EntityId, window: &mut Window, cx: &mut Context<Self>) {
        let remote = self
            .threads
            .values()
            .flat_map(|terminals| terminals.terminals.iter())
            .find(|hosted| hosted.view.entity_id() == id)
            .and_then(|hosted| hosted.remote.clone());
        if let Some(remote) = remote {
            remote.update(cx, |remote, cx| remote.close(cx));
        }
        self.remove_terminal(id, window, cx);
    }

    /// Drops a terminal, which ends its shell. Closing the shown thread's last one hides the
    /// panel, as closing T3's last terminal hides its drawer.
    fn remove_terminal(&mut self, id: EntityId, window: &mut Window, cx: &mut Context<Self>) {
        let had_focus = self.focus_handle.contains_focused(window, cx);
        let Some((thread_id, terminals)) = self.threads.iter_mut().find(|(_, terminals)| {
            terminals
                .terminals
                .iter()
                .any(|hosted| hosted.view.entity_id() == id)
        }) else {
            return;
        };
        terminals
            .terminals
            .retain(|hosted| hosted.view.entity_id() != id);
        terminals.active = terminals
            .active
            .min(terminals.terminals.len().saturating_sub(1));
        let shown = self.thread_id.as_ref() == Some(thread_id);
        let empty = terminals.terminals.is_empty();
        if shown && !empty && had_focus {
            if let Some(hosted) = terminals.active() {
                hosted.view.focus_handle(cx).focus(window, cx);
            }
        }
        if shown && empty {
            cx.emit(PanelEvent::Close);
            if had_focus {
                let workspace = self.workspace.clone();
                window.defer(cx, move |window, cx| {
                    workspace
                        .update(cx, |workspace, cx| workspace.focus_center_pane(window, cx))
                        .ok();
                });
            }
        }
        cx.notify();
    }

    /// The shown terminal's selection as a composer chip for its thread.
    fn selection_context(&self, cx: &App) -> Option<(String, TerminalContext)> {
        let thread_id = self.thread_id.clone()?;
        let terminals = self.shown()?;
        let hosted = terminals.active()?;
        let content = hosted.view.read(cx).terminal().read(cx).last_content();
        let selection = content.selection?;
        let terminal_id = match hosted.remote_id(cx) {
            Some(terminal_id) => terminal_id.to_string(),
            None => format!("local-{}", terminals.active + 1),
        };
        let context = terminal_context(
            terminal_id,
            hosted.title.to_string(),
            content.selection_text.as_deref()?,
            (selection.start.line, selection.end.line),
            content.total_lines.saturating_sub(content.screen_lines),
            &uuid::Uuid::new_v4().simple().to_string(),
        )?;
        Some((thread_id, context))
    }

    /// Puts the selection in the thread's composer, which `AddComposerContext`'s handler owns,
    /// and keeps the terminal's focus, as T3's Add to chat does. (Zed's terminal can't clear a
    /// selection without copying it, so it stays.)
    fn add_selection_to_chat(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some((thread_id, context)) = self.selection_context(cx) else {
            return;
        };
        window.dispatch_action(
            Box::new(AddComposerContext {
                thread_id,
                context: ComposerContext::Terminal(context),
            }),
            cx,
        );
        cx.notify();
    }

    /// Starts terminals in the project's checkout instead of the thread's folder, which is gone.
    fn use_checkout(&mut self, checkout: PathBuf, window: &mut Window, cx: &mut Context<Self>) {
        let Some(thread_id) = self.thread_id.clone() else {
            return;
        };
        let terminals = self.threads.entry(thread_id.clone()).or_default();
        terminals.cwd_override = Some(checkout);
        terminals.error = None;
        self.focus_on_start = true;
        self.start_terminal(thread_id, window, cx);
    }

    fn retry(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if let Some(terminals) = self
            .thread_id
            .as_ref()
            .and_then(|thread_id| self.threads.get_mut(thread_id))
        {
            terminals.error = None;
        }
        self.focus_on_start = true;
        self.ensure_terminal(window, cx);
    }

    // Rendering.

    fn render_header(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let palette = palette(cx);
        let terminals = self.shown();
        let title = self
            .thread_id
            .as_deref()
            .and_then(|thread_id| self.thread_title(thread_id, cx));
        let cwd = terminals
            .and_then(ThreadTerminals::active)
            .map(|hosted| hosted.cwd.to_string());
        let details = [title.map(|title| title.to_string()), cwd]
            .into_iter()
            .flatten()
            .collect::<Vec<_>>()
            .join(" · ");
        let tabs = terminals
            .map(|terminals| {
                terminals
                    .terminals
                    .iter()
                    .enumerate()
                    .map(|(index, hosted)| {
                        let id = hosted.view.entity_id();
                        WindowTab::new(
                            SharedString::from(format!("thread-terminal-{index}")),
                            hosted.title.clone(),
                        )
                        .active(index == terminals.active)
                        .tooltip(hosted.cwd.clone())
                        .on_click(cx.listener(move |this, _, window, cx| {
                            this.activate_terminal(index, window, cx)
                        }))
                        .on_close({
                            let this = cx.entity().downgrade();
                            move |window, cx| {
                                this.update(cx, |this, cx| this.close_terminal(id, window, cx))
                                    .ok();
                            }
                        })
                        .into_any_element()
                    })
                    .collect::<Vec<_>>()
            })
            .unwrap_or_default();
        let can_add = self.thread_id.is_some()
            && terminals.is_some_and(|terminals| !terminals.terminals.is_empty());
        let has_selection = self.selection_context(cx).is_some();
        // Run ▾ shows for any thread with a project, which can add an action.
        let has_scripts = {
            let (project, file) = self.scripts(cx);
            !project.is_empty()
                || !file.is_empty()
                || self
                    .thread_id
                    .as_deref()
                    .is_some_and(|thread_id| self.thread_project(thread_id, cx).is_some())
        };
        let focus_handle = self.focus_handle.clone();
        h_flex()
            .flex_none()
            .w_full()
            .h(scaled(px(32.)))
            .gap_1()
            .pl_2()
            .pr_1()
            .border_b(tokens::BORDER_WIDTH)
            .border_color(palette.border.hsla())
            .bg(palette.surface.hsla())
            .child(
                Icon::new(IconName::Terminal)
                    .size(IconSize::Small)
                    .color(Color::Muted),
            )
            .child(
                div().pl_1().pr_2().child(
                    Label::new("Terminal")
                        .size(LabelSize::Small)
                        .weight(FontWeight::SEMIBOLD),
                ),
            )
            .children(tabs)
            .when(can_add, |this| {
                this.child(
                    IconButton::new("new-thread-terminal", IconName::Plus)
                        .icon_size(IconSize::Small)
                        .tooltip(move |_window, cx| {
                            Tooltip::for_action_in(
                                "New terminal",
                                &NewThreadTerminal,
                                &focus_handle,
                                cx,
                            )
                        })
                        .on_click(cx.listener(|this, _, window, cx| this.new_terminal(window, cx))),
                )
            })
            .child(
                div().flex_1().min_w_0().px_2().child(
                    Label::new(details)
                        .size(LabelSize::Small)
                        .color(Color::Muted)
                        .truncate(),
                ),
            )
            .when(has_scripts, |this| {
                this.child(
                    TextButton::new("thread-terminal-scripts", "Run ▾")
                        .tooltip("Run one of the project's scripts in a new terminal")
                        .on_click(cx.listener(|_, event, window, cx| {
                            // After the click's own focus changes (the dock focuses its
                            // terminal), which would close a menu opened now.
                            let position = menu_position(event, window);
                            cx.defer_in(window, move |this, window, cx| {
                                this.deploy_scripts_menu(position, window, cx)
                            });
                        })),
                )
            })
            .when(has_selection, |this| {
                this.child(
                    TextButton::new("add-terminal-selection-to-chat", "Add to chat")
                        .tooltip("Add the selected lines to the thread's next message")
                        .on_click(cx.listener(|this, _, window, cx| {
                            this.add_selection_to_chat(window, cx)
                        })),
                )
            })
            .child(
                IconButton::new("hide-thread-terminals", IconName::Close)
                    .icon_size(IconSize::Small)
                    .tooltip({
                        let focus_handle = self.focus_handle.clone();
                        move |_window, cx| {
                            Tooltip::for_action_in(
                                "Hide terminals",
                                &ToggleThreadTerminal,
                                &focus_handle,
                                cx,
                            )
                        }
                    })
                    .on_click(cx.listener(|_, _, _, cx| cx.emit(PanelEvent::Close))),
            )
    }

    fn render_body(&self, window: &Window, cx: &mut Context<Self>) -> AnyElement {
        let Some(thread_id) = self.thread_id.clone() else {
            return self
                .message(
                    "Terminals belong to agent threads. Open a thread to use its terminals.",
                    None,
                    window,
                    cx,
                )
                .into_any_element();
        };
        let terminals = self.threads.get(&thread_id);
        if let Some(hosted) = terminals.and_then(ThreadTerminals::active) {
            return div()
                .flex_1()
                .min_h_0()
                .w_full()
                .child(hosted.content())
                .into_any_element();
        }
        if let Some(error) = terminals.and_then(|terminals| terminals.error.clone()) {
            return self
                .message(
                    error,
                    Some((
                        "Try again",
                        Box::new(|this, window, cx| this.retry(window, cx)),
                    )),
                    window,
                    cx,
                )
                .into_any_element();
        }
        if terminals.is_some_and(|terminals| terminals.starting.is_some()) {
            return self
                .message("Starting a terminal…", None, window, cx)
                .into_any_element();
        }
        let start: Option<(&'static str, MessageAction)> = Some((
            "New terminal",
            Box::new(|this, window, cx| this.retry(window, cx)),
        ));
        let (text, action): (SharedString, Option<(&'static str, MessageAction)>) = match self
            .location(&thread_id, cx)
        {
            TerminalLocation::Local(cwd) => (
                format!("No terminals open in {}.", display_path(&cwd)).into(),
                start,
            ),
            TerminalLocation::Loading => {
                let error = self
                    .shells
                    .as_ref()
                    .and_then(|(shells, _)| shells.read(cx).error().cloned());
                match error {
                    Some(error) => (format!("Couldn't read T3's threads: {error}").into(), None),
                    None => ("Loading T3's threads…".into(), None),
                }
            }
            TerminalLocation::UnknownThread => (
                "T3 doesn't list this thread as active, so mustr doesn't know its worktree.".into(),
                None,
            ),
            TerminalLocation::Remote(folder) => (
                format!("No terminals open in {} on T3's machine.", folder.cwd).into(),
                start,
            ),
            TerminalLocation::Missing { path, checkout } => {
                let text: SharedString =
                    format!("{} isn't on disk anymore.", display_path(Path::new(&path))).into();
                match checkout {
                    Some(checkout) => (
                        text,
                        Some((
                            "Open one in the project's checkout",
                            Box::new(move |this, window, cx| {
                                this.use_checkout(checkout.clone(), window, cx)
                            }),
                        )),
                    ),
                    None => (text, None),
                }
            }
        };
        self.message(text, action, window, cx).into_any_element()
    }

    fn message(
        &self,
        text: impl Into<SharedString>,
        action: Option<(&'static str, MessageAction)>,
        window: &Window,
        cx: &mut Context<Self>,
    ) -> impl IntoElement {
        let palette = palette(cx);
        let hint = kbds_for_action(&ToggleThreadTerminal, "Workspace", window).map(|keys| {
            h_flex().gap(scaled(px(3.))).children(keys).child(
                div()
                    .ml(scaled(px(2.)))
                    .text_size(tokens::text_size(tokens::TEXT_SIZE_XSMALL))
                    .text_color(palette.text_muted.hsla())
                    .child("shows or hides terminals"),
            )
        });
        v_flex()
            .flex_1()
            .w_full()
            .items_center()
            .justify_center()
            .gap_2()
            .px_4()
            .child(
                Label::new(text.into())
                    .size(LabelSize::Small)
                    .color(Color::Muted),
            )
            .when_some(action, |this, (label, action)| {
                this.child(
                    TextButton::new("thread-terminal-action", label)
                        .on_click(cx.listener(move |this, _, window, cx| action(this, window, cx))),
                )
            })
            .children(hint)
    }
}

impl RemoteFolder {
    fn launch(&self, thread_id: &str, terminal_id: String) -> Launch {
        Launch {
            thread_id: thread_id.to_string(),
            terminal_id,
            cwd: self.cwd.clone(),
            worktree_path: self.worktree_path.clone(),
            project_root: self.project_root.clone(),
        }
    }
}

/// The terminals to show for a thread on T3's machine: the ones T3 runs for it, when the panel
/// first shows the thread (`reattach`), or else a new one, numbered after every terminal T3 has
/// for it and every one the panel shows.
fn remote_launches(
    thread_id: &str,
    folder: &RemoteFolder,
    listed: &[TerminalSummary],
    shown_ids: &[String],
    reattach: bool,
) -> Vec<Launch> {
    let mut running = listed
        .iter()
        .filter(|terminal| {
            matches!(
                terminal.status,
                TerminalSessionStatus::Running | TerminalSessionStatus::Starting
            ) && !shown_ids.contains(&terminal.terminal_id)
        })
        .collect::<Vec<_>>();
    if reattach && !running.is_empty() {
        running.sort_by(|a, b| a.terminal_id.cmp(&b.terminal_id));
        return running
            .into_iter()
            .map(|terminal| Launch {
                thread_id: thread_id.to_string(),
                terminal_id: terminal.terminal_id.clone(),
                cwd: terminal.cwd.clone(),
                worktree_path: terminal.worktree_path.clone(),
                project_root: folder.project_root.clone(),
            })
            .collect();
    }
    let ids = listed
        .iter()
        .map(|terminal| terminal.terminal_id.as_str())
        .chain(shown_ids.iter().map(String::as_str));
    vec![folder.launch(thread_id, remote_terminal::next_terminal_id(ids, None))]
}

/// A terminal selection as T3's composer chip (`terminalContextRecord`): its text without
/// leading and trailing blank lines, and its lines counted from the top of the scrollback, from
/// 1. `lines` are the selection's grid lines, which count from the screen's top, negative in the
/// scrollback above it (`history` lines).
pub(crate) fn terminal_context(
    terminal_id: String,
    terminal_label: String,
    selection_text: &str,
    lines: (i32, i32),
    history: usize,
    producer_id: &str,
) -> Option<TerminalContext> {
    const MAX_TEXT: usize = 64_000;
    let normalized = selection_text.replace("\r\n", "\n");
    let text = normalized.trim_matches('\n');
    if text.trim().is_empty() {
        return None;
    }
    let text = match text.char_indices().nth(MAX_TEXT) {
        Some((end, _)) => &text[..end],
        None => text,
    };
    let row = |line: i32| (i64::from(line) + history as i64 + 1).max(1) as u32;
    let line_start = row(lines.0.min(lines.1));
    let line_end = row(lines.0.max(lines.1)).max(line_start);
    let range = if line_start == line_end {
        format!("line {line_start}")
    } else {
        format!("lines {line_start}-{line_end}")
    };
    Some(TerminalContext {
        context_id: format!("terminal_{producer_id}"),
        label: format!("{terminal_label} {range}"),
        terminal_id,
        terminal_label,
        line_start,
        line_end,
        text: text.to_string(),
    })
}

/// T3's project file (`T3_PROJECT_FILE_NAME`).
const T3_PROJECT_FILE: &str = "t3.json";

/// A script Run ▾ offers.
#[derive(Clone, Debug, PartialEq)]
pub(crate) struct Script {
    pub name: String,
    pub command: String,
    pub setup: bool,
    pub on_settle: bool,
    /// The project's script it is, which its editor changes; `None` for `t3.json`'s.
    pub source: Option<ProjectScript>,
}

impl Script {
    fn from_project(script: &ProjectScript) -> Self {
        Self {
            name: script.name.clone(),
            command: script.command.clone(),
            setup: script.run_on_worktree_create,
            on_settle: script.run_on_settle == Some(true),
            source: Some(script.clone()),
        }
    }

    /// `projectScriptMenuLabel`: `Test`, or `Install (setup, on settle)`.
    pub(crate) fn menu_label(&self) -> String {
        let roles = [(self.setup, "setup"), (self.on_settle, "on settle")]
            .into_iter()
            .filter_map(|(has, role)| has.then_some(role))
            .collect::<Vec<_>>();
        if roles.is_empty() {
            self.name.clone()
        } else {
            format!("{} ({})", self.name, roles.join(", "))
        }
    }
}

/// The scripts in a `t3.json` (T3's `T3ProjectFile`): those with a name and a command.
pub(crate) fn file_scripts(contents: &str) -> Vec<Script> {
    let Ok(file) = serde_json::from_str::<serde_json::Value>(contents) else {
        return Vec::new();
    };
    let text = |script: &serde_json::Value, field: &str| {
        script
            .get(field)
            .and_then(|value| value.as_str())
            .map(str::trim)
            .filter(|value| !value.is_empty())
            .map(str::to_string)
    };
    let flag = |script: &serde_json::Value, field: &str| {
        script.get(field).and_then(|value| value.as_bool()) == Some(true)
    };
    file.get("scripts")
        .and_then(|scripts| scripts.as_array())
        .into_iter()
        .flatten()
        .filter_map(|script| {
            Some(Script {
                name: text(script, "name")?,
                command: text(script, "command")?,
                setup: flag(script, "runOnWorktreeCreate"),
                on_settle: flag(script, "runOnSettle"),
                source: None,
            })
        })
        .collect()
}

type MessageAction =
    Box<dyn Fn(&mut ThreadTerminalPanel, &mut Window, &mut Context<ThreadTerminalPanel>)>;

fn terminal_title(view: &Entity<TerminalView>, cx: &App) -> SharedString {
    let view = view.read(cx);
    view.custom_title()
        .map(str::to_string)
        .filter(|title| !title.trim().is_empty())
        .unwrap_or_else(|| view.terminal().read(cx).title(true))
        .into()
}

#[cfg(test)]
impl ThreadTerminalPanel {
    /// The terminals on T3's machine the panel shows for its thread.
    pub(crate) fn remote_terminals(&self) -> Vec<Entity<RemoteTerminal>> {
        self.shown()
            .map(|terminals| {
                terminals
                    .terminals
                    .iter()
                    .filter_map(|hosted| hosted.remote.clone())
                    .collect()
            })
            .unwrap_or_default()
    }

    /// Why the shown thread's last terminal didn't start.
    pub(crate) fn shown_error(&self) -> Option<SharedString> {
        self.shown()?.error.clone()
    }
}

impl EventEmitter<PanelEvent> for ThreadTerminalPanel {}

impl Focusable for ThreadTerminalPanel {
    fn focus_handle(&self, _: &App) -> FocusHandle {
        self.focus_handle.clone()
    }
}

impl Panel for ThreadTerminalPanel {
    fn persistent_name() -> &'static str {
        "ThreadTerminals"
    }

    fn panel_key() -> &'static str {
        "MustrThreadTerminals"
    }

    fn activation_focus_handle(&self, cx: &App) -> FocusHandle {
        self.shown()
            .and_then(ThreadTerminals::active)
            .map(|hosted| hosted.view.focus_handle(cx))
            .unwrap_or_else(|| self.focus_handle.clone())
    }

    fn position(&self, _: &Window, _: &App) -> DockPosition {
        DockPosition::Bottom
    }

    fn position_is_valid(&self, position: DockPosition) -> bool {
        position == DockPosition::Bottom
    }

    fn set_position(&mut self, _: DockPosition, _: &mut Window, _: &mut Context<Self>) {}

    fn default_size(&self, window: &Window, cx: &App) -> Pixels {
        default_height(
            window.viewport_size().height,
            tokens::scaled_pixels(DEFAULT_HEIGHT, cx),
        )
    }

    fn set_active(&mut self, active: bool, window: &mut Window, cx: &mut Context<Self>) {
        let Some(thread_id) = self.thread_id.clone() else {
            return;
        };
        self.threads.entry(thread_id).or_default().open = active;
        if active {
            self.ensure_terminal(window, cx);
        }
    }

    // The rail and the keymap open it; there are no dock buttons in the status bar.
    fn icon(&self, _: &Window, _: &App) -> Option<ui::IconName> {
        None
    }

    fn icon_tooltip(&self, _: &Window, _: &App) -> Option<&'static str> {
        None
    }

    fn toggle_action(&self) -> Box<dyn Action> {
        Box::new(ToggleThreadTerminal)
    }

    fn activation_priority(&self) -> u32 {
        1
    }
}

impl Render for ThreadTerminalPanel {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let palette = palette(cx);
        v_flex()
            .key_context(KEY_CONTEXT)
            .track_focus(&self.focus_handle)
            .size_full()
            .bg(palette.background.hsla())
            .on_action(
                cx.listener(|this, _: &NewThreadTerminal, window, cx| {
                    this.new_terminal(window, cx)
                }),
            )
            .on_action(cx.listener(|this, _: &workspace::NewTerminal, window, cx| {
                this.new_terminal(window, cx)
            }))
            .on_action(cx.listener(|this, _: &CloseThreadTerminal, window, cx| {
                this.close_active_terminal(window, cx)
            }))
            .on_action(
                cx.listener(|this, _: &AddTerminalSelectionToChat, window, cx| {
                    this.add_selection_to_chat(window, cx)
                }),
            )
            .on_action(cx.listener(|this, _: &CloseActiveItem, window, cx| {
                this.close_active_terminal(window, cx)
            }))
            .child(self.render_header(cx))
            .child(self.render_body(window, cx))
            .children(self.menu.render())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::worktrees::tests::{FakeBackend, thread, workspace_with};
    use gpui::TestAppContext;

    fn shell_thread(worktree: Option<&str>) -> OrchestrationV2ThreadShell {
        serde_json::from_value(thread("a", worktree)).unwrap()
    }

    /// At mustr's default size the panel would take 600 pixels, more than half of a MacBook's
    /// window, so it takes 40% of it; at the design size it takes its full 300.
    #[test]
    fn the_panel_leaves_the_thread_room_on_a_laptop() {
        let height = default_height(px(982.), px(600.));
        assert!((height - px(392.8)).abs() < px(0.01), "{height:?}");
        assert_eq!(default_height(px(982.), px(300.)), px(300.));
        assert_eq!(default_height(px(1964.), px(600.)), px(600.));
    }

    #[test]
    fn terminals_start_in_the_worktree_or_the_checkout() {
        let exists = |path: &Path| path.starts_with("/here");
        assert_eq!(
            terminal_location(
                &shell_thread(Some("/here/wt/a")),
                Some("/here/api"),
                true,
                exists
            ),
            TerminalLocation::Local("/here/wt/a".into())
        );
        assert_eq!(
            terminal_location(&shell_thread(None), Some("/here/api"), true, exists),
            TerminalLocation::Local("/here/api".into())
        );
        assert_eq!(
            terminal_location(
                &shell_thread(Some("/gone/wt/a")),
                Some("/here/api"),
                true,
                exists
            ),
            TerminalLocation::Missing {
                path: "/gone/wt/a".into(),
                checkout: Some("/here/api".into()),
            }
        );
        assert_eq!(
            terminal_location(
                &shell_thread(Some("/here/wt/a")),
                Some("/here/api"),
                false,
                exists
            ),
            TerminalLocation::Remote(RemoteFolder {
                cwd: "/here/wt/a".into(),
                worktree_path: Some("/here/wt/a".into()),
                project_root: "/here/api".into(),
            })
        );
        // On T3's machine, mustr can't check the folder; without a worktree, the checkout.
        assert_eq!(
            terminal_location(&shell_thread(None), Some("/there/api"), false, exists),
            TerminalLocation::Remote(RemoteFolder {
                cwd: "/there/api".into(),
                worktree_path: None,
                project_root: "/there/api".into(),
            })
        );
    }

    fn workspace_panel(
        workspace: &Entity<Workspace>,
        cx: &mut gpui::VisualTestContext,
    ) -> Entity<ThreadTerminalPanel> {
        workspace.update_in(cx, |workspace, window, cx| {
            let handle = cx.entity();
            add_panel(workspace, &handle, window, cx);
            workspace.panel::<ThreadTerminalPanel>(cx).unwrap()
        })
    }

    fn showing(
        workspace: &Entity<Workspace>,
        panel: &Entity<ThreadTerminalPanel>,
        cx: &mut gpui::VisualTestContext,
    ) -> bool {
        workspace.read_with(cx, |workspace, cx| panel_is_showing(workspace, panel, cx))
    }

    #[gpui::test]
    async fn each_thread_keeps_its_own_terminals_open_or_closed(cx: &mut TestAppContext) {
        // Remote, so no shell starts in the test (and the fake can't open T3's); where
        // terminals would start is checked above.
        let backend = FakeBackend::new(
            vec![thread("a", Some("/wt/a")), thread("b", Some("/wt/b"))],
            Vec::new(),
        );
        let (workspace, cx) = workspace_with(backend, false, cx).await;
        let panel = workspace_panel(&workspace, cx);
        let open_thread = |thread_id: &str, cx: &mut gpui::VisualTestContext| {
            let weak = workspace.downgrade();
            let thread_id = thread_id.to_string();
            workspace.update_in(cx, |_, window, cx| {
                thread_chat::open_thread(thread_id, "Thread".into(), weak, window, cx)
            });
            cx.run_until_parked();
        };

        open_thread("a", cx);
        assert_eq!(
            panel.read_with(cx, |panel, _| panel.thread_id.clone()),
            Some("a".into())
        );
        assert!(!showing(&workspace, &panel, cx));

        // The thread toolbar's Terminal button.
        cx.dispatch_action(OpenThreadTerminal {
            thread_id: "a".into(),
        });
        cx.run_until_parked();
        assert!(showing(&workspace, &panel, cx));
        panel.read_with(cx, |panel, cx| {
            assert_eq!(
                panel.location("a", cx),
                TerminalLocation::Remote(RemoteFolder {
                    cwd: "/wt/a".into(),
                    worktree_path: Some("/wt/a".into()),
                    project_root: "/src/api".into(),
                })
            )
        });

        // Thread b's terminals weren't showing, so switching hides the panel...
        open_thread("b", cx);
        assert_eq!(
            panel.read_with(cx, |panel, _| panel.thread_id.clone()),
            Some("b".into())
        );
        assert!(!showing(&workspace, &panel, cx));
        // ...and switching back shows a's again.
        open_thread("a", cx);
        assert!(showing(&workspace, &panel, cx));

        // The toggle hides them while they have focus, and a stays hidden.
        cx.dispatch_action(ToggleThreadTerminal);
        cx.run_until_parked();
        assert!(
            showing(&workspace, &panel, cx),
            "the first toggle focuses them"
        );
        cx.dispatch_action(ToggleThreadTerminal);
        cx.run_until_parked();
        assert!(!showing(&workspace, &panel, cx));
        open_thread("b", cx);
        open_thread("a", cx);
        assert!(!showing(&workspace, &panel, cx));
    }
}

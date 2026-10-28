//! Agent threads' terminals: a panel in the bottom dock with the active thread's own terminals,
//! opened in its worktree (or its project's checkout), like T3's terminal drawer
//! (`components/ThreadTerminalDrawer.tsx`, `components/PersistentThreadTerminals.tsx`).
//!
//! - Each thread keeps its terminals while mustr runs. Switching threads shows the other
//!   thread's terminals, and switching back finds them as they were.
//! - Whether the panel shows is kept per thread too, as T3 keeps its drawer open per thread: it
//!   opens for a thread whose terminals were showing and closes for one whose weren't. Other
//!   items, such as home or a review, leave it as it is.
//! - The terminals are Zed's (`terminal_view`), on this machine. A thread on a T3 that runs on
//!   another machine would need T3's `terminal.*` RPCs and the `terminal:*` scopes, which mustr
//!   doesn't use yet, so the panel says that instead.
//! - The panel is created synchronously with the workspace (`window.rs`), so dock restoration
//!   finds it. Zed's own terminal panel loads asynchronously, which is why mustr doesn't use it.
//!   Terminals start later, when a thread first wants one.
//!
//! [`OpenThreadTerminal`] (the thread toolbar's Terminal button) opens a thread's terminals,
//! [`ToggleThreadTerminal`] (`secondary-j`, as in T3, and Zed's `ctrl-\``) shows or hides the
//! active thread's, and [`NewThreadTerminal`] adds one.

use crate::worktrees::{display_path, worktrees_are_local};
use gpui::{
    Action, AnyElement, App, AppContext as _, Context, Entity, EntityId, EventEmitter, FocusHandle,
    Focusable, FontWeight, KeyBinding, Pixels, SharedString, Subscription, Task, WeakEntity,
    Window, actions,
};
use mustr_ui::{TextButton, WindowTab, kbds_for_action, palette, tokens, tokens::scaled};
use project::Project;
use std::{
    collections::HashMap,
    path::{Path, PathBuf},
};
use t3_client::t3_api::types::OrchestrationV2ThreadShell;
use terminal_view::TerminalView;
use thread_chat::{OpenThreadTerminal, ThreadShells, ThreadView};
use ui::{Tooltip, prelude::*};
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
    ]
);

const KEY_CONTEXT: &str = "ThreadTerminals";

pub fn init(cx: &mut App) {
    cx.observe_new(|workspace: &mut Workspace, _, _| {
        workspace.register_action(|workspace, action: &OpenThreadTerminal, window, cx| {
            open_terminals(workspace, Some(action.thread_id.clone()), window, cx);
        });
        workspace.register_action(|workspace, _: &ToggleThreadTerminal, window, cx| {
            toggle_terminals(workspace, window, cx);
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
    /// T3 runs on another machine, where the folder is.
    Remote,
    /// T3 runs here, but the folder is gone. `checkout` is the project's, if it's still here.
    Missing {
        path: String,
        checkout: Option<PathBuf>,
    },
}

pub(crate) fn terminal_location(
    thread: &OrchestrationV2ThreadShell,
    project_root: Option<&str>,
    on_this_machine: bool,
    is_dir: impl Fn(&Path) -> bool,
) -> TerminalLocation {
    if !on_this_machine {
        return TerminalLocation::Remote;
    }
    let root = project_root.map(str::trim).filter(|root| !root.is_empty());
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

/// A terminal the panel hosts.
struct HostedTerminal {
    view: Entity<TerminalView>,
    cwd: PathBuf,
    title: SharedString,
    _subscription: Subscription,
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
        self.threads.entry(thread_id.clone()).or_default();
        self.thread_id = Some(thread_id);
        cx.notify();
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
                match view {
                    Ok(view) => this.add_terminal(&thread_id, view, cwd, window, cx),
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

    fn add_terminal(
        &mut self,
        thread_id: &str,
        view: Entity<TerminalView>,
        cwd: PathBuf,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let subscription = cx.subscribe_in(
            &view,
            window,
            |this, view, event: &ItemEvent, window, cx| match event {
                // The shell exited.
                ItemEvent::CloseItem => this.remove_terminal(view.entity_id(), window, cx),
                ItemEvent::UpdateTab => this.update_title(view, cx),
                _ => {}
            },
        );
        let title = terminal_title(&view, cx);
        let Some(terminals) = self.threads.get_mut(thread_id) else {
            return;
        };
        terminals.terminals.push(HostedTerminal {
            view: view.clone(),
            cwd,
            title,
            _subscription: subscription,
        });
        terminals.active = terminals.terminals.len() - 1;
        if self.focus_on_start && self.thread_id.as_deref() == Some(thread_id) {
            self.focus_on_start = false;
            view.focus_handle(cx).focus(window, cx);
        }
        cx.notify();
    }

    fn update_title(&mut self, view: &Entity<TerminalView>, cx: &mut Context<Self>) {
        let title = terminal_title(view, cx);
        let hosted = self
            .threads
            .values_mut()
            .flat_map(|terminals| terminals.terminals.iter_mut())
            .find(|hosted| hosted.view == *view);
        if let Some(hosted) = hosted
            && hosted.title != title
        {
            hosted.title = title;
            cx.notify();
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
            self.remove_terminal(id, window, cx);
        }
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
            .map(|hosted| display_path(&hosted.cwd));
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
                        .tooltip(display_path(&hosted.cwd))
                        .on_click(cx.listener(move |this, _, window, cx| {
                            this.activate_terminal(index, window, cx)
                        }))
                        .on_close({
                            let this = cx.entity().downgrade();
                            move |window, cx| {
                                this.update(cx, |this, cx| this.remove_terminal(id, window, cx))
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
                .child(hosted.view.clone())
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
            TerminalLocation::Remote => (
                "This thread's worktree is on the machine T3 runs on. Terminals on a remote \
                     T3 aren't supported yet."
                    .into(),
                None,
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

    fn default_size(&self, _: &Window, cx: &App) -> Pixels {
        tokens::scaled_pixels(px(300.), cx)
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
            .on_action(cx.listener(|this, _: &CloseActiveItem, window, cx| {
                this.close_active_terminal(window, cx)
            }))
            .child(self.render_header(cx))
            .child(self.render_body(window, cx))
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
            TerminalLocation::Remote
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
        // Remote, so no shell starts in the test; where terminals would start is checked above.
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
            assert_eq!(panel.location("a", cx), TerminalLocation::Remote)
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

//! Organizing threads (`mustr-docs/PARITY.md` §2.15): settling and un-settling, archiving,
//! pinning, renaming, and marking threads unread, as T3's thread menu does
//! (`apps/web/src/components/threadActionMenu.logic.ts` and `hooks/useThreadActions.ts`).
//!
//! The commands are built the way T3's client builds them
//! (`packages/client-runtime/src/operations/commands.ts`) and go through the installed
//! [`ThreadBackend`](crate::ThreadBackend), so dry runs and read-only pairings refuse them. The
//! palette actions act on the thread the workspace shows; the sidebar's context menu
//! ([`thread_menu`]) acts on the thread it was opened on.

use crate::{
    actions::{NewThreadInWorktree, RemoveThreadWorktree},
    backend::Access,
    commands::new_id,
    shell::ThreadShells,
    sidebar::{Section, ThreadEntry, ThreadList, ThreadListFilter},
    thread_view::ThreadView,
};
use anyhow::{Result, anyhow};
use editor::Editor;
use futures::{FutureExt as _, future::BoxFuture};
use gpui::{
    App, AppContext as _, ClipboardItem, Context, DismissEvent, Entity, EventEmitter, FocusHandle,
    Focusable, IntoElement, ParentElement as _, PromptLevel, Render, SharedString, Styled as _,
    WeakEntity, Window, actions, px,
};
use mustr_ui::{palette, tokens, tokens::scaled};
use t3_api::{
    tagged::OrchestrationV2Command,
    types::{
        OrchestrationV2CommandThreadArchive, OrchestrationV2CommandThreadArchiveType,
        OrchestrationV2CommandThreadMarkUnread, OrchestrationV2CommandThreadMarkUnreadType,
        OrchestrationV2CommandThreadMetadataUpdate, OrchestrationV2CommandThreadMetadataUpdateType,
        OrchestrationV2CommandThreadPin, OrchestrationV2CommandThreadPinType,
        OrchestrationV2CommandThreadSettle, OrchestrationV2CommandThreadSettleType,
        OrchestrationV2CommandThreadUnarchive, OrchestrationV2CommandThreadUnarchiveType,
        OrchestrationV2CommandThreadUnpin, OrchestrationV2CommandThreadUnpinType,
        OrchestrationV2CommandThreadUnsettle, OrchestrationV2CommandThreadUnsettleReason,
        OrchestrationV2CommandThreadUnsettleType, OrchestrationV2CommandThreadVisit,
        OrchestrationV2CommandThreadVisitType,
    },
};
use ui::{ContextMenu, ContextMenuEntry, IconName, Label, LabelCommon as _, LabelSize, prelude::*};
use workspace::{ModalView, SaveIntent, Toast, Workspace, notifications::NotificationId};

actions!(
    thread_chat,
    [
        /// Settles the open agent thread and moves to the next one that isn't settled.
        SettleThread,
        /// Moves the open agent thread out of Settled.
        UnsettleThread,
        /// Archives the open agent thread, offering to remove its worktree if no other thread
        /// uses it.
        ArchiveThread,
        /// Pins the open agent thread to the top of the threads sidebar.
        PinThread,
        /// Unpins the open agent thread.
        UnpinThread,
        /// Marks the open agent thread unread.
        MarkThreadUnread,
        /// Renames the open agent thread.
        RenameThread,
    ]
);

// The commands. Each takes the command's id, so tests can fix it.

/// Settles a thread (`thread.settle`). T3 stamps the time itself, as its own app lets it.
pub fn settle(thread_id: &str, command_id: String) -> OrchestrationV2Command {
    OrchestrationV2Command::ThreadSettle(OrchestrationV2CommandThreadSettle {
        command_id,
        settled_at: None,
        thread_id: thread_id.to_string(),
        type_: OrchestrationV2CommandThreadSettleType::ThreadSettle,
    })
}

/// Moves a settled thread back to Active (`thread.unsettle`), because the user asked.
pub fn unsettle(thread_id: &str, command_id: String) -> OrchestrationV2Command {
    OrchestrationV2Command::ThreadUnsettle(OrchestrationV2CommandThreadUnsettle {
        command_id,
        reason: OrchestrationV2CommandThreadUnsettleReason::User,
        thread_id: thread_id.to_string(),
        type_: OrchestrationV2CommandThreadUnsettleType::ThreadUnsettle,
    })
}

/// Archives a thread (`thread.archive`). T3 refuses while its agent runs.
pub fn archive(thread_id: &str, command_id: String) -> OrchestrationV2Command {
    OrchestrationV2Command::ThreadArchive(OrchestrationV2CommandThreadArchive {
        command_id,
        thread_id: thread_id.to_string(),
        type_: OrchestrationV2CommandThreadArchiveType::ThreadArchive,
    })
}

/// Brings an archived thread back (`thread.unarchive`), for undoing an archive.
pub fn unarchive(thread_id: &str, command_id: String) -> OrchestrationV2Command {
    OrchestrationV2Command::ThreadUnarchive(OrchestrationV2CommandThreadUnarchive {
        command_id,
        thread_id: thread_id.to_string(),
        type_: OrchestrationV2CommandThreadUnarchiveType::ThreadUnarchive,
    })
}

/// Pins a thread (`thread.pin`) after the pinned threads that have an order; T3 picks its place.
pub fn pin(thread_id: &str, command_id: String) -> OrchestrationV2Command {
    OrchestrationV2Command::ThreadPin(OrchestrationV2CommandThreadPin {
        command_id,
        order_key: None,
        thread_id: thread_id.to_string(),
        type_: OrchestrationV2CommandThreadPinType::ThreadPin,
    })
}

pub fn unpin(thread_id: &str, command_id: String) -> OrchestrationV2Command {
    OrchestrationV2Command::ThreadUnpin(OrchestrationV2CommandThreadUnpin {
        command_id,
        thread_id: thread_id.to_string(),
        type_: OrchestrationV2CommandThreadUnpinType::ThreadUnpin,
    })
}

/// Records that the user saw a thread as of `visited_at` (`thread.visit`), which clears its
/// unread state. T3's app sends the thread's `updatedAt` (`threadVisits.ts`).
pub fn visit(thread_id: &str, visited_at: &str, command_id: String) -> OrchestrationV2Command {
    OrchestrationV2Command::ThreadVisit(OrchestrationV2CommandThreadVisit {
        command_id,
        opened: None,
        search_query: None,
        thread_id: thread_id.to_string(),
        type_: OrchestrationV2CommandThreadVisitType::ThreadVisit,
        visited_at: visited_at.to_string(),
    })
}

/// Marks a thread unread (`thread.mark-unread`): T3 moves its visit to just before its last run
/// finished.
pub fn mark_unread(thread_id: &str, command_id: String) -> OrchestrationV2Command {
    OrchestrationV2Command::ThreadMarkUnread(OrchestrationV2CommandThreadMarkUnread {
        command_id,
        thread_id: thread_id.to_string(),
        type_: OrchestrationV2CommandThreadMarkUnreadType::ThreadMarkUnread,
    })
}

/// Renames a thread (`thread.metadata.update` with only a title).
pub fn rename(thread_id: &str, title: &str, command_id: String) -> OrchestrationV2Command {
    OrchestrationV2Command::ThreadMetadataUpdate(OrchestrationV2CommandThreadMetadataUpdate {
        branch: None,
        command_id,
        expected_branch: None,
        expected_empty: None,
        expected_worktree_path: None,
        limit_recovery: None,
        linked_pull_request: None,
        regenerate_title: None,
        thread_id: thread_id.to_string(),
        title: Some(title.to_string()),
        type_: OrchestrationV2CommandThreadMetadataUpdateType::ThreadMetadataUpdate,
        worktree_path: None,
    })
}

/// Sends `command` through the installed backend.
pub(crate) fn send(command: OrchestrationV2Command, cx: &App) -> BoxFuture<'static, Result<()>> {
    let Some(backend) = crate::backend(cx) else {
        return futures::future::ready(Err(anyhow!("mustr isn't set up to drive agent threads")))
            .boxed();
    };
    let dispatch = backend.dispatch(command);
    async move { dispatch.await.map(|_| ()) }.boxed()
}

/// Why organizing threads is refused before anything is sent, if it is: a dry run, or a pairing
/// without `orchestration:operate`.
pub(crate) fn blocked_reason(cx: &App) -> Option<String> {
    match crate::backend(cx) {
        Some(backend) => backend.blocked_reason(Access::Operate),
        None => Some("mustr isn't set up to drive agent threads".to_string()),
    }
}

struct OrganizeToast;

/// Shows why something couldn't be done, as a toast in `workspace`. It's deferred, so the
/// workspace's own action handlers can report.
fn report(workspace: &WeakEntity<Workspace>, message: String, cx: &mut App) {
    log::error!("{message}");
    let workspace = workspace.clone();
    cx.defer(move |cx| {
        workspace
            .update(cx, |workspace, cx| {
                workspace.show_toast(
                    Toast::new(NotificationId::unique::<OrganizeToast>(), message),
                    cx,
                )
            })
            .ok();
    });
}

/// Sends `command` and reports a failure as "Couldn't {what}".
fn send_reporting(
    command: OrchestrationV2Command,
    what: &'static str,
    workspace: WeakEntity<Workspace>,
    cx: &mut App,
) {
    let send = send(command, cx);
    cx.spawn(async move |cx| {
        if let Err(error) = send.await {
            cx.update(|cx| report(&workspace, format!("Couldn't {what}: {error:#}"), cx));
        }
    })
    .detach();
}

// The palette's actions, on the thread the workspace shows.

pub(crate) fn init(cx: &mut App) {
    cx.observe_new(|workspace: &mut Workspace, _, _| register(workspace))
        .detach();
}

fn register(workspace: &mut Workspace) {
    workspace.register_action(|workspace, _: &SettleThread, window, cx| {
        let Some(view) = active_thread_view(workspace, cx) else {
            return;
        };
        let Some(thread_id) = view.read(cx).thread_id().map(str::to_string) else {
            return;
        };
        // Like T3's `mod+shift+s`, a settled thread stays put.
        let settled = ThreadShells::global(cx)
            .read(cx)
            .thread(&thread_id)
            .is_some_and(|thread| crate::sidebar::is_settled(thread));
        if !settled {
            settle_thread(thread_id, Some(view), workspace.weak_handle(), window, cx);
        }
    });
    workspace.register_action(|workspace, _: &UnsettleThread, _, cx| {
        if let Some(thread_id) = active_thread_id(workspace, cx) {
            unsettle_thread(&thread_id, workspace.weak_handle(), cx);
        }
    });
    workspace.register_action(|workspace, _: &ArchiveThread, window, cx| {
        if let Some(view) = active_thread_view(workspace, cx)
            && let Some(thread_id) = view.read(cx).thread_id().map(str::to_string)
        {
            let focus = workspace.focus_handle(cx);
            archive_thread(
                thread_id,
                Some(view),
                focus,
                workspace.weak_handle(),
                window,
                cx,
            );
        }
    });
    workspace.register_action(|workspace, _: &PinThread, _, cx| {
        if let Some(thread_id) = active_thread_id(workspace, cx) {
            send_reporting(
                pin(&thread_id, new_id()),
                "pin the thread",
                workspace.weak_handle(),
                cx,
            );
        }
    });
    workspace.register_action(|workspace, _: &UnpinThread, _, cx| {
        if let Some(thread_id) = active_thread_id(workspace, cx) {
            send_reporting(
                unpin(&thread_id, new_id()),
                "unpin the thread",
                workspace.weak_handle(),
                cx,
            );
        }
    });
    workspace.register_action(|workspace, _: &MarkThreadUnread, _, cx| {
        if let Some(thread_id) = active_thread_id(workspace, cx) {
            send_reporting(
                mark_unread(&thread_id, new_id()),
                "mark the thread unread",
                workspace.weak_handle(),
                cx,
            );
        }
    });
    workspace.register_action(|workspace, _: &RenameThread, window, cx| {
        if let Some(view) = active_thread_view(workspace, cx)
            && let Some(thread_id) = view.read(cx).thread_id().map(str::to_string)
        {
            let title = view.read(cx).title();
            show_rename(thread_id, title, workspace, window, cx);
        }
    });
}

/// The view of the thread the workspace shows, if it shows a thread (not a draft).
pub(crate) fn active_thread_view(workspace: &Workspace, cx: &App) -> Option<Entity<ThreadView>> {
    let view = workspace.active_item(cx)?.downcast::<ThreadView>()?;
    view.read(cx).thread_id()?;
    Some(view)
}

fn active_thread_id(workspace: &Workspace, cx: &App) -> Option<String> {
    active_thread_view(workspace, cx)?
        .read(cx)
        .thread_id()
        .map(str::to_string)
}

/// The thread to open after settling `thread_id`: the next one in the sidebar that isn't
/// settled or snoozed, wrapping around, as T3's `planForwardNavigation` picks it.
fn next_after_settling(thread_id: &str, cx: &mut App) -> Option<ThreadEntry> {
    let shells = ThreadShells::global(cx);
    let filter = ThreadListFilter::get(cx);
    let shell = shells.read(cx).shell()?;
    ThreadList::from_shell(shell, &filter, crate::sidebar::now())
        .next_after(thread_id)
        .cloned()
}

/// Settles a thread (`thread.settle`). If `open_view` shows it, the workspace then moves on to
/// the next thread that isn't settled or snoozed, as T3's `mod+shift+s` does
/// (`settleAndAdvance.ts`), or to a new thread when there's none. The settled thread's tab
/// closes, unless its composer holds a draft.
pub(crate) fn settle_thread(
    thread_id: String,
    open_view: Option<Entity<ThreadView>>,
    workspace: WeakEntity<Workspace>,
    window: &mut Window,
    cx: &mut App,
) {
    // Planned before settling, from the order the user sees.
    let next = open_view
        .is_some()
        .then(|| next_after_settling(&thread_id, cx))
        .flatten();
    let send = send(settle(&thread_id, new_id()), cx);
    window
        .spawn(cx, async move |cx| {
            if let Err(error) = send.await {
                cx.update(|_, cx| {
                    report(
                        &workspace,
                        format!("Couldn't settle the thread: {error:#}"),
                        cx,
                    )
                })
                .ok();
                return;
            }
            let Some(view) = open_view else {
                return;
            };
            cx.update(|window, cx| advance(view, next, workspace, window, cx))
                .ok();
        })
        .detach();
}

/// Moves a thread out of Settled (`thread.unsettle`).
pub(crate) fn unsettle_thread(thread_id: &str, workspace: WeakEntity<Workspace>, cx: &mut App) {
    send_reporting(
        unsettle(thread_id, new_id()),
        "un-settle the thread",
        workspace,
        cx,
    );
}

/// Opens `next` (or a new thread) in place of `from`.
fn advance(
    from: Entity<ThreadView>,
    next: Option<ThreadEntry>,
    workspace: WeakEntity<Workspace>,
    window: &mut Window,
    cx: &mut App,
) {
    window.defer(cx, move |window, cx| {
        workspace
            .update(cx, |workspace, cx| {
                // Only if the user is still on the settled thread.
                let still_open = workspace
                    .active_item(cx)
                    .is_some_and(|item| item.item_id() == from.entity_id());
                if !still_open {
                    return;
                }
                match next {
                    Some(next) => {
                        ThreadView::open(next.id, next.title.into(), workspace, window, cx);
                    }
                    None => {
                        ThreadView::open_new(workspace, window, cx);
                    }
                }
                let has_draft = !from.read(cx).composer().read(cx).is_empty(cx);
                if !has_draft && let Some(pane) = workspace.pane_for(&from) {
                    pane.update(cx, |pane, cx| {
                        pane.close_item_by_id(from.entity_id(), SaveIntent::Skip, window, cx)
                    })
                    .detach_and_log_err(cx);
                }
            })
            .ok();
    });
}

/// Whether T3 would refuse to archive the thread because its agent is active
/// (`threadRuntimeCanArchive` in T3's client runtime).
pub(crate) fn is_running(thread: &t3_api::types::OrchestrationV2ThreadShell) -> bool {
    match crate::model::wire_string(&thread.status).as_str() {
        "queued" => thread.active_run_id.is_some(),
        "preparing" | "starting" | "running" => true,
        _ => false,
    }
}

/// The thread's worktree, if no other active thread uses it (`getOrphanedWorktreePathForThread`
/// in T3's app).
pub(crate) fn unshared_worktree(thread_id: &str, cx: &mut App) -> Option<String> {
    let shells = ThreadShells::global(cx);
    let shell = shells.read(cx).shell()?;
    let path = shell
        .threads
        .get(thread_id)?
        .worktree_path
        .as_deref()
        .map(str::trim)
        .filter(|path| !path.is_empty())?;
    let shared = shell.threads.values().any(|thread| {
        thread.id != thread_id
            && thread.deleted_at.is_none()
            && thread.worktree_path.as_deref().map(str::trim) == Some(path)
    });
    (!shared).then(|| path.to_string())
}

/// Archives a thread (`thread.archive`). If no other thread uses its worktree and the worktrees
/// view can remove it (WP6 handles [`RemoveThreadWorktree`]), it asks whether to remove that
/// too. A toast offers to undo the archive. If `open_view` shows the thread, a new thread
/// takes its place, as in T3.
///
/// `focus` is where [`RemoveThreadWorktree`] is dispatched from: any element in the workspace.
pub(crate) fn archive_thread(
    thread_id: String,
    open_view: Option<Entity<ThreadView>>,
    focus: FocusHandle,
    workspace: WeakEntity<Workspace>,
    window: &mut Window,
    cx: &mut App,
) {
    let shells = ThreadShells::global(cx);
    let Some(thread) = shells.read(cx).thread(&thread_id).cloned() else {
        return;
    };
    if is_running(&thread) {
        report(
            &workspace,
            "Stop the agent before archiving the thread.".to_string(),
            cx,
        );
        return;
    }
    if let Some(reason) = blocked_reason(cx) {
        report(
            &workspace,
            format!("Couldn't archive the thread: {reason}"),
            cx,
        );
        return;
    }
    let remove_action = RemoveThreadWorktree {
        thread_id: thread_id.clone(),
    };
    let worktree = unshared_worktree(&thread_id, cx)
        .filter(|_| window.is_action_available_in(&remove_action, &focus));
    let title = crate::sidebar::display_title(&thread.title);
    let prompt = worktree.as_ref().map(|path| {
        window.prompt(
            PromptLevel::Info,
            &format!("Archive “{title}”?"),
            Some(&format!(
                "No other thread uses its worktree, {path}. Remove the worktree too? Its \
                 uncommitted changes would be lost."
            )),
            &["Archive and Remove Worktree", "Archive", "Cancel"],
            cx,
        )
    });
    window
        .spawn(cx, async move |cx| {
            let remove_worktree = match prompt {
                Some(prompt) => match prompt.await {
                    Ok(0) => true,
                    Ok(1) => false,
                    _ => return,
                },
                None => false,
            };
            let send = cx.update(|window, cx| {
                // Before archiving: T3 drops archived threads from the shell the worktrees view
                // reads the worktree from.
                if remove_worktree {
                    focus.dispatch_action(&remove_action, window, cx);
                }
                send(archive(&thread_id, new_id()), cx)
            });
            let Ok(send) = send else {
                return;
            };
            let result = send.await;
            cx.update(|window, cx| match result {
                Ok(()) => {
                    show_archived(&thread_id, &title, &workspace, cx);
                    if let Some(view) = open_view {
                        advance(view, None, workspace, window, cx);
                    }
                }
                Err(error) => report(
                    &workspace,
                    format!("Couldn't archive the thread: {error:#}"),
                    cx,
                ),
            })
            .ok();
        })
        .detach();
}

struct ArchivedToast;

/// "Archived", with Undo (`thread.unarchive`), as T3's undo notice offers.
fn show_archived(thread_id: &str, title: &str, workspace: &WeakEntity<Workspace>, cx: &mut App) {
    let thread_id = thread_id.to_string();
    let undo_workspace = workspace.clone();
    workspace
        .update(cx, |workspace, cx| {
            workspace.show_toast(
                Toast::new(
                    NotificationId::unique::<ArchivedToast>(),
                    format!("Archived “{title}”."),
                )
                .on_click("Undo", move |_, cx| {
                    send_reporting(
                        unarchive(&thread_id, new_id()),
                        "unarchive the thread",
                        undo_workspace.clone(),
                        cx,
                    )
                })
                .autohide(),
                cx,
            )
        })
        .ok();
}

// The sidebar's context menu.

/// The thread menu for one of the sidebar's rows, as T3's `buildThreadActionMenuItems` lays it
/// out. Writes are disabled when they'd be refused (a dry run, or a read-only pairing).
pub(crate) fn thread_menu(
    thread: ThreadEntry,
    workspace: WeakEntity<Workspace>,
    focus: FocusHandle,
    window: &mut Window,
    cx: &mut App,
) -> Entity<ContextMenu> {
    let can_write = blocked_reason(cx).is_none();
    let filter = ThreadListFilter::get(cx);
    let new_thread_here = thread.branch.as_ref().map(|branch| NewThreadInWorktree {
        project_id: thread.project_id.clone(),
        worktree_path: thread.worktree_path.clone(),
        branch: Some(branch.clone()),
    });
    // Only when the new-thread form can take it (WP1 handles it).
    let new_thread_here = new_thread_here
        .filter(|action| window.is_action_available_in(action, &focus))
        .map(|action| (thread.branch.clone().unwrap_or_default(), action));
    let remove_focus = focus.clone();
    ContextMenu::build(window, cx, move |mut menu, _, _| {
        if let Some((branch, action)) = new_thread_here {
            let focus = focus.clone();
            menu = menu
                .item(
                    ContextMenuEntry::new(format!("New Thread on {branch}"))
                        .icon(IconName::Plus)
                        .handler(move |window, cx| focus.dispatch_action(&action, window, cx)),
                )
                .separator();
        }
        let id = thread.id.clone();
        let entry = |label: &str, icon: IconName| {
            ContextMenuEntry::new(label.to_string())
                .icon(icon)
                .icon_color(Color::Muted)
        };
        let pinned = thread.section == Section::Pinned;
        menu = menu.item({
            let (id, workspace) = (id.clone(), workspace.clone());
            entry(
                if pinned { "Unpin Thread" } else { "Pin Thread" },
                if pinned {
                    IconName::Unpin
                } else {
                    IconName::Pin
                },
            )
            .disabled(!can_write)
            .handler(move |_, cx| {
                let (command, what) = if pinned {
                    (unpin(&id, new_id()), "unpin the thread")
                } else {
                    (pin(&id, new_id()), "pin the thread")
                };
                send_reporting(command, what, workspace.clone(), cx)
            })
        });
        let settled = thread.section == Section::Settled;
        menu = menu.item({
            let (id, workspace) = (id.clone(), workspace.clone());
            entry(
                if settled {
                    "Un-settle Thread"
                } else {
                    "Settle Thread"
                },
                if settled {
                    IconName::Undo
                } else {
                    IconName::Check
                },
            )
            .disabled(!can_write)
            .handler(move |window, cx| {
                if settled {
                    unsettle_thread(&id, workspace.clone(), cx);
                } else {
                    let open_view = open_view_of(&workspace, &id, cx);
                    settle_thread(id.clone(), open_view, workspace.clone(), window, cx);
                }
            })
        });
        menu = menu.separator().item({
            let (id, title, workspace) = (id.clone(), thread.title.clone(), workspace.clone());
            entry("Rename Thread…", IconName::Pencil)
                .disabled(!can_write)
                .handler(move |window, cx| {
                    let (id, title) = (id.clone(), title.clone());
                    workspace
                        .update(cx, |workspace, cx| {
                            show_rename(id, title.into(), workspace, window, cx)
                        })
                        .ok();
                })
        });
        menu = menu.item({
            let (id, workspace) = (id.clone(), workspace.clone());
            entry("Mark Unread", IconName::Envelope)
                .disabled(!can_write || !thread.has_finished_run)
                .handler(move |_, cx| {
                    send_reporting(
                        mark_unread(&id, new_id()),
                        "mark the thread unread",
                        workspace.clone(),
                        cx,
                    )
                })
        });
        if let Some(project) = thread.project.clone() {
            let filtered = filter.project_id.as_deref() == Some(thread.project_id.as_str());
            let project_id = thread.project_id.clone();
            menu = menu.item(
                entry(
                    &if filtered {
                        "Show All Projects".to_string()
                    } else {
                        format!("Filter by {project}")
                    },
                    IconName::Filter,
                )
                .handler(move |_, cx| {
                    ThreadListFilter::set_project((!filtered).then(|| project_id.clone()), cx)
                }),
            );
        }
        menu = menu.separator();
        let copy = |label: &str, text: String| {
            entry(label, IconName::Copy).handler(move |_, cx| {
                cx.write_to_clipboard(ClipboardItem::new_string(text.clone()))
            })
        };
        if let Some(path) = thread
            .worktree_path
            .clone()
            .or_else(|| thread.workspace_root.clone())
        {
            menu = menu.item(copy("Copy Path", path));
        }
        if let Some(branch) = thread.branch.clone() {
            menu = menu.item(copy("Copy Branch", branch));
        }
        menu = menu.item(copy("Copy Thread ID", id.clone()));
        menu.separator().item({
            let (workspace, focus) = (workspace.clone(), remove_focus);
            entry("Archive Thread", IconName::Archive)
                .disabled(!can_write || thread.running)
                .handler(move |window, cx| {
                    let open_view = open_view_of(&workspace, &id, cx);
                    archive_thread(
                        id.clone(),
                        open_view,
                        focus.clone(),
                        workspace.clone(),
                        window,
                        cx,
                    );
                })
        })
    })
}

/// The workspace's active item, if it's the view of `thread_id`.
pub(crate) fn open_view_of(
    workspace: &WeakEntity<Workspace>,
    thread_id: &str,
    cx: &App,
) -> Option<Entity<ThreadView>> {
    let workspace = workspace.upgrade()?;
    active_thread_view(workspace.read(cx), cx)
        .filter(|view| view.read(cx).thread_id() == Some(thread_id))
}

// Renaming.

fn show_rename(
    thread_id: String,
    title: SharedString,
    workspace: &mut Workspace,
    window: &mut Window,
    cx: &mut Context<Workspace>,
) {
    let handle = workspace.weak_handle();
    workspace.toggle_modal(window, cx, move |window, cx| {
        RenameThreadModal::new(thread_id, title, handle, window, cx)
    });
}

/// Asks for a thread's new title, then sends it (`thread.metadata.update`).
pub(crate) struct RenameThreadModal {
    thread_id: String,
    title: SharedString,
    editor: Entity<Editor>,
    workspace: WeakEntity<Workspace>,
}

impl RenameThreadModal {
    fn new(
        thread_id: String,
        title: SharedString,
        workspace: WeakEntity<Workspace>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        let editor = cx.new(|cx| {
            let mut editor = Editor::single_line(window, cx);
            editor.set_text(title.to_string(), window, cx);
            editor.select_all(&editor::actions::SelectAll, window, cx);
            editor
        });
        Self {
            thread_id,
            title,
            editor,
            workspace,
        }
    }

    #[cfg(test)]
    pub(crate) fn editor(&self) -> &Entity<Editor> {
        &self.editor
    }

    fn confirm(&mut self, _: &menu::Confirm, _: &mut Window, cx: &mut Context<Self>) {
        let title = self.editor.read(cx).text(cx).trim().to_string();
        if !title.is_empty() && title != self.title.as_ref() {
            send_reporting(
                rename(&self.thread_id, &title, new_id()),
                "rename the thread",
                self.workspace.clone(),
                cx,
            );
        }
        cx.emit(DismissEvent);
    }

    fn cancel(&mut self, _: &menu::Cancel, _: &mut Window, cx: &mut Context<Self>) {
        cx.emit(DismissEvent);
    }
}

impl EventEmitter<DismissEvent> for RenameThreadModal {}

impl ModalView for RenameThreadModal {}

impl Focusable for RenameThreadModal {
    fn focus_handle(&self, cx: &App) -> FocusHandle {
        self.editor.focus_handle(cx)
    }
}

impl Render for RenameThreadModal {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let palette = palette(cx);
        v_flex()
            .key_context("RenameThread")
            .on_action(cx.listener(Self::confirm))
            .on_action(cx.listener(Self::cancel))
            .w(scaled(px(440.)))
            .elevation_2(cx)
            .p_3()
            .gap_2()
            .child(
                Label::new("Rename thread")
                    .size(LabelSize::Small)
                    .color(Color::Muted),
            )
            .child(
                div()
                    .px_2()
                    .py_1()
                    .rounded(scaled(tokens::CONTROL_RADIUS))
                    .border(tokens::BORDER_WIDTH)
                    .border_color(palette.border.hsla())
                    .bg(palette.surface.hsla())
                    .child(self.editor.clone()),
            )
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use pretty_assertions::assert_eq;
    use serde_json::{Value, json};

    fn wire(command: &OrchestrationV2Command) -> Value {
        serde_json::to_value(command).expect("commands serialize")
    }

    #[test]
    fn organizing_commands_match_t3s() {
        assert_eq!(
            wire(&settle("thread-1", "command-1".into())),
            json!({ "type": "thread.settle", "commandId": "command-1", "threadId": "thread-1" })
        );
        assert_eq!(
            wire(&unsettle("thread-1", "command-2".into())),
            json!({
                "type": "thread.unsettle",
                "commandId": "command-2",
                "threadId": "thread-1",
                "reason": "user"
            })
        );
        assert_eq!(
            wire(&archive("thread-1", "command-3".into())),
            json!({ "type": "thread.archive", "commandId": "command-3", "threadId": "thread-1" })
        );
        assert_eq!(
            wire(&unarchive("thread-1", "command-4".into())),
            json!({ "type": "thread.unarchive", "commandId": "command-4", "threadId": "thread-1" })
        );
        assert_eq!(
            wire(&pin("thread-1", "command-5".into())),
            json!({ "type": "thread.pin", "commandId": "command-5", "threadId": "thread-1" })
        );
        assert_eq!(
            wire(&unpin("thread-1", "command-6".into())),
            json!({ "type": "thread.unpin", "commandId": "command-6", "threadId": "thread-1" })
        );
        assert_eq!(
            wire(&visit(
                "thread-1",
                "2026-10-27T12:00:00.000Z",
                "command-7".into()
            )),
            json!({
                "type": "thread.visit",
                "commandId": "command-7",
                "threadId": "thread-1",
                "visitedAt": "2026-10-27T12:00:00.000Z"
            })
        );
        assert_eq!(
            wire(&mark_unread("thread-1", "command-8".into())),
            json!({ "type": "thread.mark-unread", "commandId": "command-8", "threadId": "thread-1" })
        );
        assert_eq!(
            wire(&rename(
                "thread-1",
                "Retry with backoff",
                "command-9".into()
            )),
            json!({
                "type": "thread.metadata.update",
                "commandId": "command-9",
                "threadId": "thread-1",
                "title": "Retry with backoff"
            })
        );
    }
}

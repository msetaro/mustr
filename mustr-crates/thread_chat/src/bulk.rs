//! Deleting threads, and acting on several at once (`mustr-docs/PARITY.md` §2.15), as T3's
//! sidebar does (`components/Sidebar.tsx`, `threadSelectionStore.ts`, `hooks/useThreadActions.ts`):
//! - **Delete** (`thread.delete`) asks first, since it clears the conversation for good, and, for
//!   one thread whose worktree no other thread uses, whether to remove that too. The thread's
//!   tab closes, and an open one moves on to the next thread.
//! - **Several threads:** mod-click and shift-click select rows in the sidebar (`sidebar.rs`);
//!   right-clicking a selected row opens [`bulk_menu`], with T3's Unpin, Settle, Snooze, Mark
//!   Unread and Delete for them all.
//!
//! Every command goes through the installed backend, so dry runs and read-only pairings refuse
//! them, and tests record them.

use crate::{
    actions::RemoveThreadWorktree,
    commands::new_id,
    organize::{self, report},
    shell::ThreadShells,
    sidebar::{Section, ThreadEntry},
    thread_view::ThreadView,
};
use gpui::{App, Entity, FocusHandle, Focusable as _, PromptLevel, WeakEntity, Window, actions};
use std::rc::Rc;
use t3_api::{
    tagged::OrchestrationV2Command,
    types::{OrchestrationV2CommandThreadDelete, OrchestrationV2CommandThreadDeleteType},
};
use ui::{ContextMenu, ContextMenuEntry, IconName, prelude::*};
use workspace::{SaveIntent, Workspace};

actions!(
    thread_chat,
    [
        /// Deletes the open agent thread and its conversation, after asking.
        DeleteThread,
    ]
);

/// Deletes a thread and its conversation (`thread.delete`).
pub fn delete(thread_id: &str, command_id: String) -> OrchestrationV2Command {
    OrchestrationV2Command::ThreadDelete(OrchestrationV2CommandThreadDelete {
        command_id,
        thread_id: thread_id.to_string(),
        type_: OrchestrationV2CommandThreadDeleteType::ThreadDelete,
    })
}

pub(crate) fn init(cx: &mut App) {
    cx.observe_new(|workspace: &mut Workspace, _, _| {
        workspace.register_action(|workspace, _: &DeleteThread, window, cx| {
            let thread_id = organize::active_thread_view(workspace, cx)
                .and_then(|view| view.read(cx).thread_id().map(str::to_string));
            match thread_id {
                Some(thread_id) => {
                    let focus = workspace.focus_handle(cx);
                    delete_threads(vec![thread_id], focus, workspace.weak_handle(), window, cx);
                }
                None => cx.propagate(),
            }
        });
    })
    .detach();
}

/// Deletes `thread_ids` after asking, as T3 does. For one thread whose worktree no other thread
/// uses, and when the worktrees view can remove it, it asks whether to remove the worktree too.
/// T3 refuses while an agent runs, so those threads are left alone, with a note.
///
/// `focus` is where [`RemoveThreadWorktree`] is dispatched from: any element in the workspace.
pub(crate) fn delete_threads(
    thread_ids: Vec<String>,
    focus: FocusHandle,
    workspace: WeakEntity<Workspace>,
    window: &mut Window,
    cx: &mut App,
) {
    if let Some(reason) = organize::blocked_reason(cx) {
        report(&workspace, format!("Couldn't delete: {reason}"), cx);
        return;
    }
    let shells = ThreadShells::global(cx);
    let threads = thread_ids
        .iter()
        .filter_map(|id| shells.read(cx).thread(id).cloned())
        .collect::<Vec<_>>();
    if threads.iter().any(organize::is_running) {
        let message = match threads.as_slice() {
            [_] => "Stop the agent before deleting the thread.",
            _ => "Stop the running agents before deleting their threads.",
        };
        report(&workspace, message.to_string(), cx);
        return;
    }
    let (title, detail, remove) = match threads.as_slice() {
        [] => return,
        [thread] => {
            let title = crate::sidebar::display_title(&thread.title);
            let remove_action = RemoveThreadWorktree {
                thread_id: thread.id.clone(),
            };
            let worktree = organize::unshared_worktree(&thread.id, cx)
                .filter(|_| window.is_action_available_in(&remove_action, &focus));
            let detail = match &worktree {
                Some(path) => format!(
                    "This clears its conversation for good. No other thread uses its worktree, \
                     {path}. Remove the worktree too? Its uncommitted changes would be lost."
                ),
                None => "This clears its conversation for good.".to_string(),
            };
            (
                format!("Delete “{title}”?"),
                detail,
                worktree.map(|_| remove_action),
            )
        }
        several => (
            format!("Delete {} threads?", several.len()),
            "This clears their conversations for good.".to_string(),
            None,
        ),
    };
    let answers: &[&str] = if remove.is_some() {
        &["Delete and Remove Worktree", "Delete", "Cancel"]
    } else {
        &["Delete", "Cancel"]
    };
    let prompt = window.prompt(PromptLevel::Warning, &title, Some(&detail), answers, cx);
    let thread_ids = threads
        .into_iter()
        .map(|thread| thread.id)
        .collect::<Vec<_>>();
    window
        .spawn(cx, async move |cx| {
            let remove = match (prompt.await, remove) {
                (Ok(0), Some(remove)) => Some(remove),
                (Ok(1), Some(_)) | (Ok(0), None) => None,
                _ => return,
            };
            // The thread the workspace shows moves on to the next one that isn't deleted.
            let next = cx
                .update(|_, cx| next_after_deleting(&thread_ids, &workspace, cx))
                .ok()
                .flatten();
            let Ok(backend) = cx.update(|window, cx| {
                // Before deleting: T3 drops deleted threads from the shell the worktrees view
                // reads the worktree from.
                if let Some(remove) = &remove {
                    focus.dispatch_action(remove, window, cx);
                }
                crate::backend(cx)
            }) else {
                return;
            };
            let Some(backend) = backend else {
                return;
            };
            let mut deleted = Vec::new();
            let mut failure = None;
            for thread_id in thread_ids {
                match backend.dispatch(delete(&thread_id, new_id())).await {
                    Ok(_) => deleted.push(thread_id),
                    Err(error) => {
                        failure.get_or_insert(error);
                    }
                }
            }
            cx.update(|window, cx| {
                if let Some(error) = failure {
                    report(&workspace, format!("Couldn't delete: {error:#}"), cx);
                }
                close_deleted(&deleted, next, workspace, window, cx);
            })
            .ok();
        })
        .detach();
}

/// The thread to show after deleting `thread_ids`, if the workspace shows one of them.
fn next_after_deleting(
    thread_ids: &[String],
    workspace: &WeakEntity<Workspace>,
    cx: &mut App,
) -> Option<(String, ThreadEntry)> {
    let workspace = workspace.upgrade()?;
    let shown = organize::active_thread_view(workspace.read(cx), cx)?
        .read(cx)
        .thread_id()?
        .to_string();
    if !thread_ids.contains(&shown) {
        return None;
    }
    let shells = ThreadShells::global(cx);
    let filter = crate::sidebar::ThreadListFilter::get(cx);
    let shell = shells.read(cx).shell()?;
    let list = crate::sidebar::ThreadList::from_shell(shell, &filter, crate::sidebar::now());
    // As `ThreadList::next_after` picks it, past the other deleted threads.
    let order = list.iter().collect::<Vec<_>>();
    let index = order.iter().position(|thread| thread.id == shown)?;
    let next = order[index + 1..]
        .iter()
        .chain(&order[..index])
        .find(|thread| {
            matches!(thread.section, Section::Pinned | Section::Active)
                && !thread_ids.contains(&thread.id)
        })
        .map(|thread| (*thread).clone());
    Some((shown, next?))
}

/// Closes the views of deleted threads. The one the workspace shows moves on to `next`, or to a
/// new thread.
fn close_deleted(
    deleted: &[String],
    next: Option<(String, ThreadEntry)>,
    workspace: WeakEntity<Workspace>,
    window: &mut Window,
    cx: &mut App,
) {
    let Some(handle) = workspace.upgrade() else {
        return;
    };
    let views = handle
        .read(cx)
        .panes()
        .iter()
        .flat_map(|pane| {
            pane.read(cx)
                .items_of_type::<ThreadView>()
                .collect::<Vec<_>>()
        })
        .filter(|view| {
            view.read(cx)
                .thread_id()
                .is_some_and(|id| deleted.iter().any(|deleted| deleted == id))
        })
        .collect::<Vec<Entity<ThreadView>>>();
    let shown = organize::active_thread_view(handle.read(cx), cx);
    for view in views {
        if shown.as_ref() == Some(&view) {
            let next = next
                .clone()
                .filter(|(from, _)| view.read(cx).thread_id() == Some(from.as_str()))
                .map(|(_, next)| next);
            organize::advance(view, next, workspace.clone(), window, cx);
            continue;
        }
        let pane = handle.read(cx).pane_for(&view);
        if let Some(pane) = pane {
            pane.update(cx, |pane, cx| {
                pane.close_item_by_id(view.entity_id(), SaveIntent::Skip, window, cx)
            })
            .detach_and_log_err(cx);
        }
    }
}

/// One of the actions for several selected rows.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum BulkKind {
    Unpin,
    Settle,
    Snooze,
    MarkUnread,
    Delete,
}

/// An action for several selected rows, with the threads it applies to.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct BulkAction {
    pub kind: BulkKind,
    pub label: String,
    pub ids: Vec<String>,
}

/// The actions for several selected rows, as T3's sidebar lists them
/// (`handleMultiSelectContextMenu`): Unpin for the pinned ones, Settle for those not settled,
/// Snooze when they can all snooze, Mark Unread for those with a finished run, and Delete.
pub(crate) fn bulk_actions(threads: &[ThreadEntry]) -> Vec<BulkAction> {
    let ids = |keep: fn(&ThreadEntry) -> bool| {
        threads
            .iter()
            .filter(|thread| keep(thread))
            .map(|thread| thread.id.clone())
            .collect::<Vec<_>>()
    };
    let action = |kind, label: &str, ids: Vec<String>| BulkAction {
        kind,
        label: format!("{label} ({})", ids.len()),
        ids,
    };
    let mut actions = Vec::new();
    let pinned = ids(|thread| thread.pinned);
    if !pinned.is_empty() {
        actions.push(action(BulkKind::Unpin, "Unpin", pinned));
    }
    actions.push(action(
        BulkKind::Settle,
        "Settle",
        ids(|thread| !thread.settled && thread.section != Section::Settled),
    ));
    if threads.iter().all(|thread| thread.can_snooze) {
        actions.push(action(BulkKind::Snooze, "Snooze", ids(|_| true)));
    }
    actions.push(action(
        BulkKind::MarkUnread,
        "Mark Unread",
        ids(|thread| thread.has_finished_run),
    ));
    let mut delete = action(BulkKind::Delete, "Delete", ids(|_| true));
    delete.label.push('…');
    actions.push(delete);
    actions
}

/// Runs `kind` on `ids`: one command each, in order (`snooze_until` for a snooze), or, for
/// Delete, [`delete_threads`] after asking.
pub(crate) fn run_bulk(
    kind: BulkKind,
    ids: Vec<String>,
    snooze_until: Option<String>,
    focus: FocusHandle,
    workspace: WeakEntity<Workspace>,
    window: &mut Window,
    cx: &mut App,
) {
    match kind {
        BulkKind::Unpin => send_each(&ids, organize::unpin, "unpin the threads", workspace, cx),
        BulkKind::Settle => send_each(&ids, organize::settle, "settle the threads", workspace, cx),
        BulkKind::MarkUnread => send_each(
            &ids,
            organize::mark_unread,
            "mark the threads unread",
            workspace,
            cx,
        ),
        BulkKind::Snooze => {
            let Some(until) = snooze_until else {
                return;
            };
            let commands = ids
                .iter()
                .map(|id| organize::snooze(id, &until, new_id()))
                .collect();
            send_all(commands, "snooze the threads", workspace, cx);
        }
        BulkKind::Delete => delete_threads(ids, focus, workspace, window, cx),
    }
}

/// The menu for several selected rows ([`bulk_actions`]). `done` runs after an action, to
/// clear the selection.
pub(crate) fn bulk_menu(
    threads: Vec<ThreadEntry>,
    workspace: WeakEntity<Workspace>,
    focus: FocusHandle,
    done: Rc<dyn Fn(&mut App)>,
    window: &mut Window,
    cx: &mut App,
) -> Entity<ContextMenu> {
    let can_write = organize::blocked_reason(cx).is_none();
    let actions = bulk_actions(&threads);
    let presets = crate::snooze::presets(chrono::Local::now());
    ContextMenu::build(window, cx, move |mut menu, _, _| {
        for action in actions {
            let icon = match action.kind {
                BulkKind::Unpin => IconName::Unpin,
                BulkKind::Settle => IconName::Check,
                BulkKind::Snooze => IconName::Clock,
                BulkKind::MarkUnread => IconName::Envelope,
                BulkKind::Delete => IconName::Trash,
            };
            if action.kind == BulkKind::Delete {
                menu = menu.separator();
            }
            let (workspace, focus, done) = (workspace.clone(), focus.clone(), done.clone());
            if action.kind == BulkKind::Snooze {
                let presets = presets.clone();
                menu = menu.submenu_with_icon(action.label, icon, move |mut menu, _, _| {
                    for preset in &presets {
                        let (ids, workspace, focus, done) = (
                            action.ids.clone(),
                            workspace.clone(),
                            focus.clone(),
                            done.clone(),
                        );
                        let until = preset.until.clone();
                        menu = menu.item(
                            ContextMenuEntry::new(format!("{} ({})", preset.label, preset.when))
                                .disabled(!can_write)
                                .handler(move |window, cx| {
                                    run_bulk(
                                        BulkKind::Snooze,
                                        ids.clone(),
                                        Some(until.clone()),
                                        focus.clone(),
                                        workspace.clone(),
                                        window,
                                        cx,
                                    );
                                    done(cx);
                                }),
                        );
                    }
                    menu
                });
                continue;
            }
            let ids = action.ids;
            menu = menu.item(
                ContextMenuEntry::new(action.label)
                    .icon(icon)
                    .icon_color(Color::Muted)
                    .disabled(!can_write || ids.is_empty())
                    .handler(move |window, cx| {
                        run_bulk(
                            action.kind,
                            ids.clone(),
                            None,
                            focus.clone(),
                            workspace.clone(),
                            window,
                            cx,
                        );
                        done(cx);
                    }),
            );
        }
        menu
    })
}

fn send_each(
    ids: &[String],
    command: fn(&str, String) -> OrchestrationV2Command,
    what: &'static str,
    workspace: WeakEntity<Workspace>,
    cx: &mut App,
) {
    let commands = ids.iter().map(|id| command(id, new_id())).collect();
    send_all(commands, what, workspace, cx);
}

/// Sends every command, reporting the first T3 refuses as "Couldn't {what}".
fn send_all(
    commands: Vec<OrchestrationV2Command>,
    what: &'static str,
    workspace: WeakEntity<Workspace>,
    cx: &mut App,
) {
    let sends = commands
        .into_iter()
        .map(|command| organize::send(command, cx))
        .collect::<Vec<_>>();
    cx.spawn(async move |cx| {
        let mut failure = None;
        for send in sends {
            if let Err(error) = send.await {
                failure.get_or_insert(error);
            }
        }
        if let Some(error) = failure {
            cx.update(|cx| report(&workspace, format!("Couldn't {what}: {error:#}"), cx));
        }
    })
    .detach();
}

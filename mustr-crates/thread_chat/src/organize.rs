//! Organizing threads (`mustr-docs/PARITY.md` §2.15): settling and un-settling, archiving and
//! the archived list, pinning and ordering, per-thread auto-settle, renaming, and marking
//! threads unread, as T3's thread menu does (`apps/web/src/components/threadActionMenu.logic.ts`
//! and `hooks/useThreadActions.ts`). Settling, unpinning and snoozing offer Undo for 5 seconds,
//! with a toast and `mod+z` (`hooks/showThreadUndoNotice.ts`). The order keys and drop plans the
//! sidebar's drag and drop sends are T3's own (`threadSort.ts`, `planSidebarThreadDrop`).
//!
//! The commands are built the way T3's client builds them
//! (`packages/client-runtime/src/operations/commands.ts`) and go through the installed
//! [`ThreadBackend`](crate::ThreadBackend), so dry runs and read-only pairings refuse them. The
//! palette actions act on the thread the workspace shows; the sidebar's context menu
//! ([`thread_menu`]) acts on the thread it was opened on.

use crate::{
    actions::{NewThreadInWorktree, RemoveThreadWorktree},
    backend::{Access, ThreadBackendExt as _},
    commands::new_id,
    shell::ThreadShells,
    sidebar::{Section, ThreadEntry, ThreadList, ThreadListFilter},
    thread_view::ThreadView,
};
use anyhow::{Result, anyhow};
use collections::{HashMap, HashSet};
use editor::Editor;
use futures::{FutureExt as _, future::BoxFuture};
use gpui::{
    App, AppContext as _, ClipboardItem, Context, DismissEvent, Entity, EventEmitter, FocusHandle,
    Focusable, Global, IntoElement, ParentElement as _, PromptLevel, Render, SharedString,
    Styled as _, Task, WeakEntity, Window, actions, px,
};
use mustr_ui::{palette, tokens, tokens::scaled};
use picker::{Picker, PickerDelegate};
use std::{sync::Arc, time::Duration};
use t3_api::{
    tagged::OrchestrationV2Command,
    types::{
        OrchestrationV2CommandThreadActiveReorder, OrchestrationV2CommandThreadActiveReorderType,
        OrchestrationV2CommandThreadArchive, OrchestrationV2CommandThreadArchiveType,
        OrchestrationV2CommandThreadAutoSettleSet, OrchestrationV2CommandThreadAutoSettleSetType,
        OrchestrationV2CommandThreadMarkUnread, OrchestrationV2CommandThreadMarkUnreadType,
        OrchestrationV2CommandThreadMetadataUpdate, OrchestrationV2CommandThreadMetadataUpdateType,
        OrchestrationV2CommandThreadPin, OrchestrationV2CommandThreadPinReorder,
        OrchestrationV2CommandThreadPinReorderType, OrchestrationV2CommandThreadPinType,
        OrchestrationV2CommandThreadSettle, OrchestrationV2CommandThreadSettleType,
        OrchestrationV2CommandThreadSnooze, OrchestrationV2CommandThreadSnoozeType,
        OrchestrationV2CommandThreadUnarchive, OrchestrationV2CommandThreadUnarchiveType,
        OrchestrationV2CommandThreadUnpin, OrchestrationV2CommandThreadUnpinType,
        OrchestrationV2CommandThreadUnsettle, OrchestrationV2CommandThreadUnsettleReason,
        OrchestrationV2CommandThreadUnsettleType, OrchestrationV2CommandThreadUnsnooze,
        OrchestrationV2CommandThreadUnsnoozeReason, OrchestrationV2CommandThreadUnsnoozeType,
        OrchestrationV2CommandThreadVisit, OrchestrationV2CommandThreadVisitType,
        OrchestrationV2ThreadShell,
    },
};
use ui::{
    ContextMenu, ContextMenuEntry, Icon, IconName, IconPosition, IconSize, Label, LabelCommon as _,
    LabelSize, ListItem, ListItemSpacing, Tooltip, prelude::*,
};
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
        /// Undoes the latest settle, unpin or snooze, for 5 seconds after it.
        UndoThreadAction,
        /// Wakes the open agent thread from its snooze.
        WakeThread,
        /// Lets T3's auto-settle rules settle the open agent thread, or keeps them from it.
        ToggleThreadAutoSettle,
        /// Copies the link to the open agent thread's pull request.
        CopyPullRequestLink,
        /// Lists the archived agent threads, to bring them back.
        ShowArchivedThreads,
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
    pin_at(thread_id, None, command_id)
}

/// Pins a thread (`thread.pin`) at `order_key` among the pinned threads, or where T3 picks.
pub fn pin_at(
    thread_id: &str,
    order_key: Option<String>,
    command_id: String,
) -> OrchestrationV2Command {
    OrchestrationV2Command::ThreadPin(OrchestrationV2CommandThreadPin {
        command_id,
        order_key,
        thread_id: thread_id.to_string(),
        type_: OrchestrationV2CommandThreadPinType::ThreadPin,
    })
}

/// Moves a pinned thread to `order_key` among the pinned threads (`thread.pin.reorder`).
pub fn reorder_pinned(
    thread_id: &str,
    order_key: &str,
    command_id: String,
) -> OrchestrationV2Command {
    OrchestrationV2Command::ThreadPinReorder(OrchestrationV2CommandThreadPinReorder {
        command_id,
        order_key: order_key.to_string(),
        thread_id: thread_id.to_string(),
        type_: OrchestrationV2CommandThreadPinReorderType::ThreadPinReorder,
    })
}

/// Moves an active thread to `order_key` among the active threads (`thread.active.reorder`).
pub fn reorder_active(
    thread_id: &str,
    order_key: &str,
    command_id: String,
) -> OrchestrationV2Command {
    OrchestrationV2Command::ThreadActiveReorder(OrchestrationV2CommandThreadActiveReorder {
        command_id,
        order_key: order_key.to_string(),
        thread_id: thread_id.to_string(),
        type_: OrchestrationV2CommandThreadActiveReorderType::ThreadActiveReorder,
    })
}

/// Snoozes a thread until `snoozed_until`, one of T3's ISO 8601 timestamps (`thread.snooze`).
pub fn snooze(thread_id: &str, snoozed_until: &str, command_id: String) -> OrchestrationV2Command {
    OrchestrationV2Command::ThreadSnooze(OrchestrationV2CommandThreadSnooze {
        command_id,
        snoozed_until: snoozed_until.to_string(),
        thread_id: thread_id.to_string(),
        type_: OrchestrationV2CommandThreadSnoozeType::ThreadSnooze,
    })
}

/// Wakes a snoozed thread, because the user asked (`thread.unsnooze`).
pub fn unsnooze(thread_id: &str, command_id: String) -> OrchestrationV2Command {
    OrchestrationV2Command::ThreadUnsnooze(OrchestrationV2CommandThreadUnsnooze {
        command_id,
        reason: OrchestrationV2CommandThreadUnsnoozeReason::User,
        thread_id: thread_id.to_string(),
        type_: OrchestrationV2CommandThreadUnsnoozeType::ThreadUnsnooze,
    })
}

/// Lets T3's auto-settle rules settle the thread, or keeps them from it
/// (`thread.auto-settle.set`).
pub fn set_auto_settle(
    thread_id: &str,
    enabled: bool,
    command_id: String,
) -> OrchestrationV2Command {
    OrchestrationV2Command::ThreadAutoSettleSet(OrchestrationV2CommandThreadAutoSettleSet {
        command_id,
        enabled,
        thread_id: thread_id.to_string(),
        type_: OrchestrationV2CommandThreadAutoSettleSetType::ThreadAutoSettleSet,
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

/// Sends `commands` one after another, stopping at the first T3 refuses. Returns the shell
/// sequence of the last one.
pub(crate) fn send_in_order(
    commands: Vec<OrchestrationV2Command>,
    cx: &App,
) -> BoxFuture<'static, Result<Option<i64>>> {
    let Some(backend) = crate::backend(cx) else {
        return futures::future::ready(Err(anyhow!("mustr isn't set up to drive agent threads")))
            .boxed();
    };
    async move {
        let mut sequence = None;
        for command in commands {
            sequence = Some(backend.dispatch(command).await?.sequence);
        }
        Ok(sequence)
    }
    .boxed()
}

// Order keys: where pinned and active threads go when the user orders them, as T3's client
// writes them (`packages/client-runtime/src/state/threadSort.ts`). Keys are strings of a to z
// that sort as fractions, so a thread moves with one write between its new neighbors.

const ORDER_DIGITS: &[u8] = b"abcdefghijklmnopqrstuvwxyz";

fn order_digit(byte: u8) -> usize {
    ORDER_DIGITS
        .iter()
        .position(|digit| *digit == byte)
        .unwrap_or(0)
}

/// Whether T3's client could have written `key` (`isValidPinOrderKey`). A trailing `a` leaves
/// no room before it, so T3 treats it as corrupt.
fn is_valid_order_key(key: &str) -> bool {
    !key.is_empty() && key.bytes().all(|byte| byte.is_ascii_lowercase()) && !key.ends_with('a')
}

/// The midpoint of two keys read as fractions, `""` being the open bound on either side
/// (`pinOrderMidpoint`). Needs `a < b` when `b` isn't open.
fn order_midpoint(a: &str, b: &str) -> String {
    let (a_bytes, b_bytes) = (a.as_bytes(), b.as_bytes());
    if !b.is_empty() {
        let mut shared = 0;
        while Some(*a_bytes.get(shared).unwrap_or(&b'a')) == b_bytes.get(shared).copied() {
            shared += 1;
        }
        if shared > 0 {
            return format!(
                "{}{}",
                &b[..shared],
                order_midpoint(a.get(shared..).unwrap_or(""), &b[shared..])
            );
        }
    }
    let digit_a = a_bytes.first().map_or(0, |byte| order_digit(*byte));
    let digit_b = b_bytes
        .first()
        .map_or(ORDER_DIGITS.len(), |byte| order_digit(*byte));
    if digit_b - digit_a > 1 {
        // `Math.round` of the mean, halves up.
        return (ORDER_DIGITS[(digit_a + digit_b).div_ceil(2)] as char).to_string();
    }
    if b.len() > 1 {
        return b[..1].to_string();
    }
    format!(
        "{}{}",
        ORDER_DIGITS[digit_a] as char,
        order_midpoint(a.get(1..).unwrap_or(""), "")
    )
}

/// A key that sorts strictly between two neighbors' (`pinOrderKeyBetween`), `None` standing for
/// the top or the bottom. `None` when a neighbor's key is corrupt or they're out of order.
pub(crate) fn order_key_between(before: Option<&str>, after: Option<&str>) -> Option<String> {
    let (a, b) = (before.unwrap_or(""), after.unwrap_or(""));
    if (!a.is_empty() && !is_valid_order_key(a)) || (!b.is_empty() && !is_valid_order_key(b)) {
        return None;
    }
    if !b.is_empty() && a >= b {
        return None;
    }
    Some(order_midpoint(a, b))
}

/// `count` keys spread evenly, for rewriting a whole section (`generateSpreadPinOrderKeys`).
pub(crate) fn spread_order_keys(count: usize) -> Vec<String> {
    let base = ORDER_DIGITS.len() as u64;
    let mut width = 2;
    let mut space = base.pow(2);
    while space <= (count as u64 + 1) * 2 {
        width += 1;
        space *= base;
    }
    let step = space as f64 / (count as f64 + 1.);
    (0..count)
        .map(|index| {
            // `Math.round`.
            let mut value = (step * (index as f64 + 1.) + 0.5).floor() as u64;
            if value.is_multiple_of(base) {
                value += 1;
            }
            let mut key = vec![b'a'; width];
            for digit in key.iter_mut().rev() {
                *digit = ORDER_DIGITS[(value % base) as usize];
                value /= base;
            }
            String::from_utf8(key).unwrap_or_default()
        })
        .collect()
}

/// The key writes that put `moved` where it is in `ordered` (`planPinnedReorder`): one write
/// between its neighbors when they have keys, or else keys for the whole section. `keys` has
/// every thread's key, those of threads that aren't shown too, so no write reuses theirs.
pub(crate) fn plan_reorder(
    ordered: &[String],
    keys: &HashMap<String, String>,
    moved: &str,
) -> Vec<(String, String)> {
    let visible = ordered.iter().map(String::as_str).collect::<HashSet<_>>();
    let reserved = keys
        .iter()
        .filter(|(id, _)| !visible.contains(id.as_str()))
        .map(|(_, key)| key.as_str())
        .collect::<HashSet<_>>();
    let Some(index) = ordered.iter().position(|id| id == moved) else {
        return Vec::new();
    };
    let before = index.checked_sub(1).map(|index| &ordered[index]);
    let after = ordered.get(index + 1);
    let before_key = before.and_then(|id| keys.get(id)).map(String::as_str);
    let after_key = after.and_then(|id| keys.get(id)).map(String::as_str);
    let usable =
        (before.is_none() || before_key.is_some()) && (after.is_none() || after_key.is_some());
    if usable {
        let mut key = order_key_between(before_key, after_key);
        while let Some(taken) = key.as_deref().filter(|key| reserved.contains(key)) {
            key = order_key_between(Some(taken), after_key);
        }
        if let Some(key) = key {
            return vec![(moved.to_string(), key)];
        }
    }
    spread_order_keys(ordered.len() + reserved.len())
        .into_iter()
        .filter(|key| !reserved.contains(key.as_str()))
        .zip(ordered)
        .filter(|(key, id)| keys.get(*id) != Some(key))
        .map(|(key, id)| (id.clone(), key))
        .collect()
}

/// Where a dragged thread lands: its section, and the orders of the pinned and active threads
/// with it there (`SidebarDropTarget`).
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct DropTarget {
    pub section: Section,
    pub pinned_order: Vec<String>,
    pub active_order: Vec<String>,
}

/// What a drop does (`SidebarThreadDropPlan`). Order keys are `(thread id, key)` writes.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum DropPlan {
    None,
    /// Reorders the pinned threads (`thread.pin.reorder`).
    ReorderPinned {
        assignments: Vec<(String, String)>,
    },
    /// Pins the thread at `order_key`, then writes the other pinned threads' keys.
    Pin {
        order_key: Option<String>,
        extra: Vec<(String, String)>,
    },
    /// Moves the thread into Active (unpinning, un-settling or waking it first), then writes
    /// the active threads' keys (`thread.active.reorder`).
    MoveActive {
        assignments: Vec<(String, String)>,
        unpin: bool,
        unsettle: bool,
        unsnooze: bool,
    },
    Settle,
}

/// The dragged thread and the sidebar as they are, for [`plan_drop`].
pub(crate) struct DropSource<'a> {
    pub thread_id: &'a str,
    pub section: Section,
    /// Pinned even if it shows elsewhere (snoozed or settled).
    pub pinned: bool,
    pub settled: bool,
    /// The pinned and active threads in the order they show.
    pub pinned_order: &'a [String],
    pub active_order: &'a [String],
    /// Every thread's pin and active order keys.
    pub pin_keys: &'a HashMap<String, String>,
    pub active_keys: &'a HashMap<String, String>,
}

/// What dropping `source` at `target` does, as T3's `planSidebarThreadDrop` plans it. Snoozed
/// threads can't be dropped among snoozed ones: that needs a wake time.
pub(crate) fn plan_drop(source: &DropSource, target: &DropTarget) -> DropPlan {
    match target.section {
        Section::Active => {
            if source.section == Section::Active && target.active_order == source.active_order {
                return DropPlan::None;
            }
            DropPlan::MoveActive {
                assignments: plan_reorder(
                    &target.active_order,
                    source.active_keys,
                    source.thread_id,
                ),
                unpin: source.pinned,
                unsettle: source.settled,
                unsnooze: source.section == Section::Snoozed,
            }
        }
        Section::Settled if source.section == Section::Settled => DropPlan::None,
        Section::Settled => DropPlan::Settle,
        Section::Pinned => {
            if source.section == Section::Pinned && target.pinned_order == source.pinned_order {
                return DropPlan::None;
            }
            let assignments = plan_reorder(&target.pinned_order, source.pin_keys, source.thread_id);
            if source.section == Section::Pinned {
                return if assignments.is_empty() {
                    DropPlan::None
                } else {
                    DropPlan::ReorderPinned { assignments }
                };
            }
            let order_key = assignments
                .iter()
                .find(|(id, _)| id == source.thread_id)
                .map(|(_, key)| key.clone());
            let extra = assignments
                .into_iter()
                .filter(|(id, _)| source.pinned || id != source.thread_id)
                .collect();
            DropPlan::Pin { order_key, extra }
        }
        Section::Snoozed => DropPlan::None,
    }
}

/// The commands a drop sends, in order. Settling goes through [`settle_thread`] instead.
pub(crate) fn drop_commands(thread_id: &str, plan: &DropPlan) -> Vec<OrchestrationV2Command> {
    match plan {
        DropPlan::None | DropPlan::Settle => Vec::new(),
        DropPlan::ReorderPinned { assignments } => assignments
            .iter()
            .map(|(id, key)| reorder_pinned(id, key, new_id()))
            .collect(),
        DropPlan::Pin { order_key, extra } => {
            std::iter::once(pin_at(thread_id, order_key.clone(), new_id()))
                .chain(
                    extra
                        .iter()
                        .map(|(id, key)| reorder_pinned(id, key, new_id())),
                )
                .collect()
        }
        DropPlan::MoveActive {
            assignments,
            unpin: unpin_first,
            unsettle: unsettle_first,
            unsnooze: unsnooze_first,
        } => {
            let mut commands = Vec::new();
            if *unpin_first {
                commands.push(unpin(thread_id, new_id()));
            }
            if *unsettle_first {
                commands.push(unsettle(thread_id, new_id()));
            }
            if *unsnooze_first {
                commands.push(unsnooze(thread_id, new_id()));
            }
            commands.extend(
                assignments
                    .iter()
                    .map(|(id, key)| reorder_active(id, key, new_id())),
            );
            commands
        }
    }
}

/// Every thread's pin order key and active order key, by thread id, from the shell.
pub(crate) fn order_keys(cx: &mut App) -> (HashMap<String, String>, HashMap<String, String>) {
    let shells = ThreadShells::global(cx);
    let Some(shell) = shells.read(cx).shell() else {
        return Default::default();
    };
    let keys = |key: fn(&OrchestrationV2ThreadShell) -> &Option<String>| {
        shell
            .threads
            .values()
            .filter_map(|thread| Some((thread.id.clone(), key(thread).clone()?)))
            .collect::<HashMap<_, _>>()
    };
    (
        keys(|thread| &thread.pin_order_key),
        keys(|thread| &thread.active_order_key),
    )
}

// Undo, for settling, unpinning and snoozing, as T3's undo notice offers it for 5 seconds
// (`hooks/showThreadUndoNotice.ts`, and `planThreadUndo` in T3's client runtime).

/// How long Undo is offered, as long as the toast shows.
const UNDO_WINDOW: Duration = Duration::from_secs(5);

/// One step of putting a thread back.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum UndoStep {
    Unsettle,
    Unsnooze,
    Pin { order_key: Option<String> },
    Snooze { until: String },
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum UndoKind {
    Settled,
    Unpinned,
    Snoozed,
}

/// The steps that undo `kind` on `thread`, as it was before (`planThreadUndo`). Settling and
/// pinning clear a snooze on T3's side, so the snooze is put back after the pin.
pub(crate) fn undo_steps(
    kind: UndoKind,
    thread: Option<&OrchestrationV2ThreadShell>,
) -> Vec<UndoStep> {
    if kind == UndoKind::Snoozed {
        return vec![UndoStep::Unsnooze];
    }
    let pin = UndoStep::Pin {
        order_key: thread.and_then(|thread| thread.pin_order_key.clone()),
    };
    let snooze = thread
        .and_then(|thread| thread.snoozed_until.clone())
        .map(|until| UndoStep::Snooze { until });
    let mut steps = Vec::new();
    match kind {
        UndoKind::Unpinned => steps.push(pin),
        _ => {
            steps.push(UndoStep::Unsettle);
            if thread.is_some_and(|thread| thread.pinned_at.is_some()) {
                steps.push(pin);
            }
        }
    }
    steps.extend(snooze);
    steps
}

/// The commands for `steps`, leaving out a snooze that has ended meanwhile (T3 refuses it).
pub(crate) fn undo_commands(
    thread_id: &str,
    steps: &[UndoStep],
    now: time::OffsetDateTime,
) -> Vec<OrchestrationV2Command> {
    steps
        .iter()
        .filter_map(|step| match step {
            UndoStep::Unsettle => Some(unsettle(thread_id, new_id())),
            UndoStep::Unsnooze => Some(unsnooze(thread_id, new_id())),
            UndoStep::Pin { order_key } => Some(pin_at(thread_id, order_key.clone(), new_id())),
            UndoStep::Snooze { until } => {
                let until_time = time::OffsetDateTime::parse(
                    until,
                    &time::format_description::well_known::Rfc3339,
                )
                .ok()?;
                (until_time > now).then(|| snooze(thread_id, until, new_id()))
            }
        })
        .collect()
}

struct Undoable {
    thread_id: String,
    kind: UndoKind,
    steps: Vec<UndoStep>,
    workspace: WeakEntity<Workspace>,
}

/// What [`UndoThreadAction`] can undo now: the latest settle, unpin or snooze, for 5 seconds.
#[derive(Default)]
struct ThreadUndo {
    latest: Option<Undoable>,
    generation: usize,
    _expiry: Option<Task<()>>,
}

impl Global for ThreadUndo {}

struct UndoToast;

/// Offers to undo `kind` for 5 seconds: a toast with Undo, and [`UndoThreadAction`]
/// (`mod+z`).
pub(crate) fn offer_undo(
    kind: UndoKind,
    thread_id: &str,
    title: &str,
    steps: Vec<UndoStep>,
    workspace: &WeakEntity<Workspace>,
    cx: &mut App,
) {
    let undo = cx.default_global::<ThreadUndo>();
    undo.generation += 1;
    let generation = undo.generation;
    undo.latest = Some(Undoable {
        thread_id: thread_id.to_string(),
        kind,
        steps,
        workspace: workspace.clone(),
    });
    let expiry = cx.spawn(async move |cx| {
        cx.background_executor().timer(UNDO_WINDOW).await;
        cx.update(|cx| {
            let undo = cx.default_global::<ThreadUndo>();
            if undo.generation == generation {
                undo.latest = None;
            }
        });
    });
    cx.default_global::<ThreadUndo>()._expiry = Some(expiry);
    let message = match kind {
        UndoKind::Settled => format!("Settled “{title}”."),
        UndoKind::Unpinned => format!("Unpinned “{title}”."),
        UndoKind::Snoozed => format!("Snoozed “{title}”."),
    };
    let workspace = workspace.clone();
    cx.defer(move |cx| {
        workspace
            .update(cx, |workspace, cx| {
                workspace.show_toast(
                    Toast::new(NotificationId::unique::<UndoToast>(), message)
                        .on_click("Undo", |_, cx| {
                            undo_latest(cx);
                        })
                        .autohide(),
                    cx,
                )
            })
            .ok();
    });
}

/// Undoes the latest settle, unpin or snooze while Undo is offered. Returns whether there was
/// one.
pub(crate) fn undo_latest(cx: &mut App) -> bool {
    let Some(undoable) = cx.default_global::<ThreadUndo>().latest.take() else {
        return false;
    };
    let workspace = undoable.workspace.clone();
    // Deferred, since `mod+z` reaches here from the workspace's own action handlers.
    let toast_workspace = workspace.clone();
    cx.defer(move |cx| {
        toast_workspace
            .update(cx, |workspace, cx| {
                workspace.dismiss_toast(&NotificationId::unique::<UndoToast>(), cx)
            })
            .ok();
    });
    let commands = undo_commands(&undoable.thread_id, &undoable.steps, crate::sidebar::now());
    let what = match undoable.kind {
        UndoKind::Settled => "undo the settle",
        UndoKind::Unpinned => "undo the unpin",
        UndoKind::Snoozed => "undo the snooze",
    };
    let send = send_in_order(commands, cx);
    cx.spawn(async move |cx| {
        if let Err(error) = send.await {
            cx.update(|cx| report(&workspace, format!("Couldn't {what}: {error:#}"), cx));
        }
    })
    .detach();
    true
}

struct OrganizeToast;

/// Shows why something couldn't be done, as a toast in `workspace`. It's deferred, so the
/// workspace's own action handlers can report.
pub(crate) fn report(workspace: &WeakEntity<Workspace>, message: String, cx: &mut App) {
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
            unpin_thread(&thread_id, workspace.weak_handle(), cx);
        }
    });
    workspace.register_action(|_, _: &UndoThreadAction, _, cx| {
        if !undo_latest(cx) {
            cx.propagate();
        }
    });
    workspace.register_action(|workspace, _: &WakeThread, _, cx| {
        if let Some(thread_id) = active_thread_id(workspace, cx) {
            send_reporting(
                unsnooze(&thread_id, new_id()),
                "wake the thread",
                workspace.weak_handle(),
                cx,
            );
        }
    });
    workspace.register_action(|workspace, _: &ToggleThreadAutoSettle, _, cx| {
        if let Some(thread_id) = active_thread_id(workspace, cx) {
            let enabled = ThreadShells::global(cx)
                .read(cx)
                .thread(&thread_id)
                .is_some_and(|thread| thread.auto_settle_disabled_at.is_none());
            send_reporting(
                set_auto_settle(&thread_id, !enabled, new_id()),
                "change the thread's auto-settle",
                workspace.weak_handle(),
                cx,
            );
        }
    });
    workspace.register_action(|workspace, _: &CopyPullRequestLink, _, cx| {
        let link = active_thread_id(workspace, cx).and_then(|thread_id| {
            let shells = ThreadShells::global(cx);
            let thread = shells.read(cx).thread(&thread_id)?;
            crate::sidebar::RowPullRequest::of(thread).map(|pull_request| pull_request.url)
        });
        match link {
            Some(url) => cx.write_to_clipboard(ClipboardItem::new_string(url)),
            None => cx.propagate(),
        }
    });
    workspace.register_action(|workspace, _: &ShowArchivedThreads, window, cx| {
        show_archived_threads(workspace, window, cx);
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
pub(crate) fn next_after_settling(thread_id: &str, cx: &mut App) -> Option<ThreadEntry> {
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
    let before = ThreadShells::global(cx)
        .read(cx)
        .thread(&thread_id)
        .cloned();
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
            cx.update(|window, cx| {
                let title = before
                    .as_ref()
                    .map(|thread| crate::sidebar::display_title(&thread.title))
                    .unwrap_or_else(|| "the thread".to_string());
                let steps = undo_steps(UndoKind::Settled, before.as_ref());
                offer_undo(UndoKind::Settled, &thread_id, &title, steps, &workspace, cx);
                if let Some(view) = open_view {
                    advance(view, next, workspace, window, cx);
                }
            })
            .ok();
        })
        .detach();
}

/// Unpins a thread (`thread.unpin`), offering Undo.
pub(crate) fn unpin_thread(thread_id: &str, workspace: WeakEntity<Workspace>, cx: &mut App) {
    let before = ThreadShells::global(cx).read(cx).thread(thread_id).cloned();
    let send = send(unpin(thread_id, new_id()), cx);
    let thread_id = thread_id.to_string();
    cx.spawn(async move |cx| match send.await {
        Ok(()) => cx.update(|cx| {
            let title = before
                .as_ref()
                .map(|thread| crate::sidebar::display_title(&thread.title))
                .unwrap_or_else(|| "the thread".to_string());
            let steps = undo_steps(UndoKind::Unpinned, before.as_ref());
            offer_undo(
                UndoKind::Unpinned,
                &thread_id,
                &title,
                steps,
                &workspace,
                cx,
            );
        }),
        Err(error) => cx.update(|cx| {
            report(
                &workspace,
                format!("Couldn't unpin the thread: {error:#}"),
                cx,
            )
        }),
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
pub(crate) fn advance(
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
        let pinned = thread.pinned;
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
                if pinned {
                    unpin_thread(&id, workspace.clone(), cx);
                } else {
                    send_reporting(pin(&id, new_id()), "pin the thread", workspace.clone(), cx)
                }
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
        if thread.section == Section::Snoozed {
            menu = menu.item({
                let (id, workspace) = (id.clone(), workspace.clone());
                entry("Wake Thread", IconName::Clock)
                    .disabled(!can_write)
                    .handler(move |_, cx| {
                        send_reporting(
                            unsnooze(&id, new_id()),
                            "wake the thread",
                            workspace.clone(),
                            cx,
                        )
                    })
            });
        } else {
            let presets = crate::snooze::presets(chrono::Local::now());
            let can_snooze = can_write && thread.can_snooze;
            let (id, title, workspace) = (id.clone(), thread.title.clone(), workspace.clone());
            menu = menu.submenu_with_icon("Snooze", IconName::Clock, move |mut menu, _, _| {
                for preset in &presets {
                    let (id, title, workspace) = (id.clone(), title.clone(), workspace.clone());
                    let until = preset.until.clone();
                    menu = menu.item(
                        ContextMenuEntry::new(format!("{} ({})", preset.label, preset.when))
                            .disabled(!can_snooze)
                            .handler(move |window, cx| {
                                crate::snooze::snooze_thread(
                                    id.clone(),
                                    title.clone(),
                                    until.clone(),
                                    workspace.clone(),
                                    window,
                                    cx,
                                )
                            }),
                    );
                }
                let (id, title, workspace) = (id.clone(), title.clone(), workspace.clone());
                menu.separator().item(
                    ContextMenuEntry::new("Custom…")
                        .disabled(!can_snooze)
                        .handler(move |window, cx| {
                            crate::snooze::show_snooze_dialog(
                                id.clone(),
                                title.clone(),
                                workspace.clone(),
                                window,
                                cx,
                            )
                        }),
                )
            });
        }
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
        {
            let (id, workspace) = (id.clone(), workspace.clone());
            let enabled = thread.auto_settle;
            menu = menu.submenu_with_icon(
                "Auto-Settle",
                IconName::CountdownTimer,
                move |menu, _, _| {
                    let choice = |menu: ContextMenu, label: &'static str, value: bool| {
                        let (id, workspace) = (id.clone(), workspace.clone());
                        menu.item(
                            ContextMenuEntry::new(label)
                                .toggleable(IconPosition::End, enabled == value)
                                .disabled(!can_write)
                                .handler(move |_, cx| {
                                    if enabled != value {
                                        send_reporting(
                                            set_auto_settle(&id, value, new_id()),
                                            "change the thread's auto-settle",
                                            workspace.clone(),
                                            cx,
                                        )
                                    }
                                }),
                        )
                    };
                    let menu = choice(menu, "Enabled", true);
                    choice(menu, "Disabled", false)
                },
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
        if let Some(pull_request) = &thread.pull_request {
            menu = menu.item(copy("Copy PR Link", pull_request.url.clone()));
        }
        let delete_id = id.clone();
        menu.separator()
            .item({
                let (workspace, focus) = (workspace.clone(), remove_focus.clone());
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
            .item({
                // WP17: T3's Delete, after asking (`bulk.rs`).
                let (workspace, focus) = (workspace.clone(), remove_focus.clone());
                entry("Delete Thread…", IconName::Trash)
                    .disabled(!can_write || thread.running)
                    .handler(move |window, cx| {
                        crate::bulk::delete_threads(
                            vec![delete_id.clone()],
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

// The archived list (T3's `settings/ArchivedThreadsPanel.tsx`): archived threads, newest
// first, read once with `orchestration.getArchivedShellSnapshot`, each brought back with
// `thread.unarchive`.

/// One archived thread.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct ArchivedThread {
    pub id: String,
    pub title: String,
    pub project: Option<String>,
    pub archived_at: Option<String>,
}

/// The archived threads of `snapshot` that the archived list shows, newest first. Subagents
/// come back with their parents.
pub(crate) fn archived_threads(
    snapshot: &t3_api::types::OrchestrationV2ArchivedShellSnapshot,
    projects: &HashMap<String, String>,
) -> Vec<ArchivedThread> {
    let mut threads = snapshot
        .threads
        .iter()
        .filter(|thread| {
            thread.deleted_at.is_none()
                && thread.lineage.relationship_to_parent
                    != Some(t3_api::types::OrchestrationV2AppThreadLineageRelationshipToParent::Subagent)
        })
        .map(|thread| ArchivedThread {
            id: thread.id.clone(),
            title: crate::sidebar::display_title(&thread.title),
            project: snapshot
                .projects
                .iter()
                .find(|project| project.id == thread.project_id)
                .map(|project| project.title.clone())
                .or_else(|| projects.get(&thread.project_id).cloned()),
            archived_at: thread.archived_at.clone(),
        })
        .collect::<Vec<_>>();
    threads.sort_by(|left, right| {
        right
            .archived_at
            .cmp(&left.archived_at)
            .then_with(|| left.id.cmp(&right.id))
    });
    threads
}

/// How long ago `timestamp` was, briefly: "just now", "5m ago", "3h ago", "2d ago".
pub(crate) fn time_ago(timestamp: &str, now: time::OffsetDateTime) -> Option<String> {
    let then =
        time::OffsetDateTime::parse(timestamp, &time::format_description::well_known::Rfc3339)
            .ok()?;
    let minutes = (now - then).whole_minutes();
    Some(match minutes {
        ..1 => "just now".to_string(),
        1..60 => format!("{minutes}m ago"),
        60..1440 => format!("{}h ago", minutes / 60),
        _ => format!("{}d ago", minutes / 1440),
    })
}

fn show_archived_threads(
    workspace: &mut Workspace,
    window: &mut Window,
    cx: &mut Context<Workspace>,
) {
    let Some(backend) = crate::backend(cx) else {
        report(
            &workspace.weak_handle(),
            "mustr isn't set up to show agent threads".to_string(),
            cx,
        );
        return;
    };
    let projects = ThreadShells::global(cx)
        .read(cx)
        .shell()
        .map(|shell| {
            shell
                .projects
                .values()
                .map(|project| (project.id.clone(), project.title.clone()))
                .collect::<HashMap<_, _>>()
        })
        .unwrap_or_default();
    let handle = workspace.weak_handle();
    workspace.toggle_modal(window, cx, move |window, cx| {
        let delegate = ArchivedThreadsDelegate {
            workspace: handle,
            threads: None,
            error: None,
            matches: Vec::new(),
            selected: 0,
            restoring: HashSet::default(),
            blocked: blocked_reason(cx),
            _load: Task::ready(()),
        };
        let mut picker = Picker::uniform_list(delegate, window, cx)
            .initial_width(mustr_ui::tokens::modal_width(rems(34.), window));
        let load = backend.call::<t3_api::rpc::OrchestrationGetArchivedShellSnapshot>(
            t3_api::types::OrchestrationGetArchivedShellSnapshotPayload(Default::default()),
            Access::Read,
        );
        picker.delegate._load = cx.spawn_in(window, async move |picker, cx| {
            let result = load.await;
            picker
                .update_in(cx, |picker, window, cx| {
                    match result {
                        Ok(snapshot) => {
                            picker.delegate.threads = Some(archived_threads(&snapshot, &projects))
                        }
                        Err(error) => {
                            log::warn!("couldn't list the archived threads: {error:#}");
                            picker.delegate.error = Some(format!("{error:#}").into());
                        }
                    }
                    picker.refresh(window, cx);
                })
                .ok();
        });
        picker
    });
}

/// The archived list: search by title or project, and Enter (or a click) brings one back.
pub(crate) struct ArchivedThreadsDelegate {
    workspace: WeakEntity<Workspace>,
    /// `None` until they load.
    threads: Option<Vec<ArchivedThread>>,
    error: Option<SharedString>,
    /// Indexes into `threads`.
    matches: Vec<usize>,
    selected: usize,
    /// Threads being brought back.
    restoring: HashSet<String>,
    /// Why unarchiving would be refused (a dry run, or a read-only pairing).
    blocked: Option<String>,
    _load: Task<()>,
}

impl ArchivedThreadsDelegate {
    #[cfg(test)]
    pub(crate) fn listed(&self) -> Vec<&ArchivedThread> {
        let threads = self.threads.as_deref().unwrap_or_default();
        self.matches.iter().map(|index| &threads[*index]).collect()
    }

    fn unarchive(&mut self, index: usize, cx: &mut Context<Picker<Self>>) {
        let Some(thread) = self
            .matches
            .get(index)
            .and_then(|index| self.threads.as_ref()?.get(*index))
            .cloned()
        else {
            return;
        };
        if self.blocked.is_some() || !self.restoring.insert(thread.id.clone()) {
            return;
        }
        let send = send(unarchive(&thread.id, new_id()), cx);
        let workspace = self.workspace.clone();
        cx.spawn(async move |picker, cx| {
            let result = send.await;
            picker
                .update(cx, |picker, cx| {
                    let query = picker.query(cx);
                    let delegate = &mut picker.delegate;
                    delegate.restoring.remove(&thread.id);
                    match result {
                        Ok(()) => {
                            if let Some(threads) = &mut delegate.threads {
                                threads.retain(|archived| archived.id != thread.id);
                            }
                            delegate.matches = filter_archived(
                                delegate.threads.as_deref().unwrap_or_default(),
                                &query,
                            );
                            delegate.selected = delegate
                                .selected
                                .min(delegate.matches.len().saturating_sub(1));
                            show_unarchived(&thread.title, &workspace, cx);
                        }
                        Err(error) => report(
                            &workspace,
                            format!("Couldn't unarchive the thread: {error:#}"),
                            cx,
                        ),
                    }
                    cx.notify();
                })
                .ok();
        })
        .detach();
        cx.notify();
    }
}

/// The archived threads whose title or project has every word of `query`, in order.
fn filter_archived(threads: &[ArchivedThread], query: &str) -> Vec<usize> {
    let words = query
        .split_whitespace()
        .map(str::to_lowercase)
        .collect::<Vec<_>>();
    threads
        .iter()
        .enumerate()
        .filter(|(_, thread)| {
            let haystack = format!(
                "{} {}",
                thread.title.to_lowercase(),
                thread.project.as_deref().unwrap_or_default().to_lowercase()
            );
            words.iter().all(|word| haystack.contains(word.as_str()))
        })
        .map(|(index, _)| index)
        .collect()
}

struct UnarchivedToast;

fn show_unarchived(title: &str, workspace: &WeakEntity<Workspace>, cx: &mut App) {
    workspace
        .update(cx, |workspace, cx| {
            workspace.show_toast(
                Toast::new(
                    NotificationId::unique::<UnarchivedToast>(),
                    format!("Unarchived “{title}”."),
                )
                .autohide(),
                cx,
            )
        })
        .ok();
}

impl PickerDelegate for ArchivedThreadsDelegate {
    type ListItem = ListItem;

    fn name() -> &'static str {
        "mustr archived threads"
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

    fn placeholder_text(&self, _: &mut Window, _: &mut App) -> Arc<str> {
        "Search archived threads…".into()
    }

    fn no_matches_text(&self, _: &mut Window, _: &mut App) -> Option<SharedString> {
        Some(match (&self.threads, &self.error) {
            (None, Some(error)) => format!("Couldn't list the archived threads: {error}").into(),
            (None, None) => "Loading archived threads…".into(),
            (Some(threads), _) if threads.is_empty() => "No archived threads.".into(),
            (Some(_), _) => "No archived threads match.".into(),
        })
    }

    fn update_matches(
        &mut self,
        query: String,
        _: &mut Window,
        _: &mut Context<Picker<Self>>,
    ) -> Task<()> {
        self.matches = filter_archived(self.threads.as_deref().unwrap_or_default(), &query);
        self.selected = self.selected.min(self.matches.len().saturating_sub(1));
        Task::ready(())
    }

    fn confirm(&mut self, _secondary: bool, _: &mut Window, cx: &mut Context<Picker<Self>>) {
        self.unarchive(self.selected, cx);
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
        let thread = self.threads.as_ref()?.get(*self.matches.get(ix)?)?;
        let details = [
            thread.project.clone(),
            thread
                .archived_at
                .as_deref()
                .and_then(|at| time_ago(at, crate::sidebar::now()))
                .map(|ago| format!("archived {ago}")),
        ]
        .into_iter()
        .flatten()
        .collect::<Vec<_>>()
        .join(" · ");
        let restoring = self.restoring.contains(&thread.id);
        let blocked = self.blocked.clone();
        Some(
            ListItem::new(ix)
                .inset(true)
                .spacing(ListItemSpacing::Sparse)
                .toggle_state(selected)
                .start_slot(
                    Icon::new(IconName::Archive)
                        .size(IconSize::Small)
                        .color(Color::Muted),
                )
                .child(
                    v_flex()
                        .min_w_0()
                        .child(
                            Label::new(thread.title.clone())
                                .size(LabelSize::Small)
                                .truncate(),
                        )
                        .child(
                            Label::new(details)
                                .size(LabelSize::XSmall)
                                .color(Color::Muted)
                                .truncate(),
                        ),
                )
                .end_slot(
                    Button::new(
                        ("unarchive", ix),
                        if restoring {
                            "Unarchiving…"
                        } else {
                            "Unarchive"
                        },
                    )
                    .style(ButtonStyle::Subtle)
                    .label_size(LabelSize::Small)
                    .disabled(restoring || blocked.is_some())
                    .when_some(blocked, |button, reason| {
                        button.tooltip(Tooltip::text(format!("Unarchiving is off: {reason}")))
                    })
                    .on_click(
                        cx.listener(move |picker, _, _, cx| picker.delegate.unarchive(ix, cx)),
                    ),
                ),
        )
    }

    fn render_footer(&self, _: &mut Window, cx: &mut Context<Picker<Self>>) -> Option<AnyElement> {
        let count = self.threads.as_ref()?.len();
        let text = match &self.blocked {
            Some(reason) => format!("Unarchiving is off: {reason}"),
            None => format!(
                "{count} archived thread{}. Enter brings the selected one back.",
                if count == 1 { "" } else { "s" }
            ),
        };
        Some(
            div()
                .w_full()
                .px_3()
                .py_1p5()
                .border_t_1()
                .border_color(cx.theme().colors().border_variant)
                .child(Label::new(text).size(LabelSize::XSmall).color(Color::Muted))
                .into_any_element(),
        )
    }
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

    #[test]
    fn ordering_and_snoozing_commands_match_t3s() {
        assert_eq!(
            wire(&pin_at("thread-1", Some("n".into()), "command-1".into())),
            json!({ "type": "thread.pin", "commandId": "command-1", "threadId": "thread-1", "orderKey": "n" })
        );
        assert_eq!(
            wire(&reorder_pinned("thread-1", "h", "command-2".into())),
            json!({ "type": "thread.pin.reorder", "commandId": "command-2", "threadId": "thread-1", "orderKey": "h" })
        );
        assert_eq!(
            wire(&reorder_active("thread-1", "u", "command-3".into())),
            json!({ "type": "thread.active.reorder", "commandId": "command-3", "threadId": "thread-1", "orderKey": "u" })
        );
        assert_eq!(
            wire(&snooze(
                "thread-1",
                "2026-10-27T17:00:00.000Z",
                "command-4".into()
            )),
            json!({
                "type": "thread.snooze",
                "commandId": "command-4",
                "threadId": "thread-1",
                "snoozedUntil": "2026-10-27T17:00:00.000Z"
            })
        );
        assert_eq!(
            wire(&unsnooze("thread-1", "command-5".into())),
            json!({ "type": "thread.unsnooze", "commandId": "command-5", "threadId": "thread-1", "reason": "user" })
        );
        assert_eq!(
            wire(&set_auto_settle("thread-1", false, "command-6".into())),
            json!({ "type": "thread.auto-settle.set", "commandId": "command-6", "threadId": "thread-1", "enabled": false })
        );
    }

    /// The expected keys come from T3's own `threadSort.ts`, run on the same inputs.
    #[test]
    fn order_keys_match_t3s() {
        for (before, after, key) in [
            (None, None, Some("n")),
            (Some("n"), None, Some("u")),
            (None, Some("n"), Some("h")),
            (Some("a"), Some("b"), None),
            (Some("az"), None, Some("n")),
            (Some("y"), Some("z"), Some("yn")),
            (Some("n"), Some("o"), Some("nn")),
            (Some("nz"), Some("o"), Some("nzn")),
            (Some("h"), Some("hb"), Some("han")),
            (Some("zz"), None, Some("zzn")),
            (None, Some("b"), Some("an")),
            (None, Some("ab"), Some("aan")),
            (Some("a0"), None, None),
            (Some("c"), Some("b"), None),
            (Some("mmm"), Some("mmn"), Some("mmmn")),
        ] {
            assert_eq!(
                order_key_between(before, after).as_deref(),
                key,
                "between {before:?} and {after:?}"
            );
        }
        assert!(spread_order_keys(0).is_empty());
        assert_eq!(spread_order_keys(1), ["nb"]);
        assert_eq!(spread_order_keys(3), ["gn", "nb", "tn"]);
        let keys = spread_order_keys(25);
        assert_eq!(
            (keys[0].as_str(), keys[1].as_str(), keys[24].as_str()),
            ("bb", "cb", "zb")
        );
        let keys = spread_order_keys(400);
        assert_eq!(
            (
                keys[0].as_str(),
                keys[1].as_str(),
                keys[399].as_str(),
                keys.len()
            ),
            ("abs", "adk", "zyi", 400)
        );
    }

    #[test]
    fn reordering_writes_as_few_keys_as_t3_does() {
        let ids = |ids: &[&str]| ids.iter().map(|id| id.to_string()).collect::<Vec<_>>();
        let keys = |keys: &[(&str, &str)]| {
            keys.iter()
                .map(|(id, key)| (id.to_string(), key.to_string()))
                .collect::<HashMap<_, _>>()
        };
        let writes = |writes: &[(&str, &str)]| {
            writes
                .iter()
                .map(|(id, key)| (id.to_string(), key.to_string()))
                .collect::<Vec<_>>()
        };
        // Between neighbors with keys: one write.
        let ordered = keys(&[("a", "h"), ("b", "p"), ("c", "t")]);
        assert_eq!(
            plan_reorder(&ids(&["a", "c", "b"]), &ordered, "c"),
            writes(&[("c", "l")])
        );
        // After a neighbor without one, at the end.
        assert_eq!(
            plan_reorder(
                &ids(&["n1", "a", "b"]),
                &keys(&[("a", "h"), ("b", "p")]),
                "b"
            ),
            writes(&[("b", "r")])
        );
        // A hidden thread's key isn't reused.
        let with_hidden = keys(&[("a", "h"), ("b", "p"), ("c", "t"), ("hidden", "l")]);
        assert_eq!(
            plan_reorder(&ids(&["a", "c", "b"]), &with_hidden, "c"),
            writes(&[("c", "n")])
        );
        // No keys yet, or keys T3 wouldn't write: the section is rewritten.
        assert_eq!(
            plan_reorder(&ids(&["x", "y", "z"]), &HashMap::default(), "y"),
            writes(&[("x", "gn"), ("y", "nb"), ("z", "tn")])
        );
        assert_eq!(
            plan_reorder(&ids(&["a", "b"]), &keys(&[("a", "a0"), ("b", "b0")]), "b"),
            writes(&[("a", "ir"), ("b", "rj")])
        );
    }

    #[test]
    fn drops_plan_t3s_commands() {
        let ids = |ids: &[&str]| ids.iter().map(|id| id.to_string()).collect::<Vec<_>>();
        let pin_keys = [
            ("p1".to_string(), "h".to_string()),
            ("p2".to_string(), "p".to_string()),
        ]
        .into_iter()
        .collect::<HashMap<_, _>>();
        let active_keys = HashMap::default();
        let (pinned, active) = (ids(&["p1", "p2"]), ids(&["a1", "a2"]));
        let source = |thread_id, section, pinned_flag| DropSource {
            thread_id,
            section,
            pinned: pinned_flag,
            settled: section == Section::Settled,
            pinned_order: &pinned,
            active_order: &active,
            pin_keys: &pin_keys,
            active_keys: &active_keys,
        };
        let target = |section, pinned_order: &[&str], active_order: &[&str]| DropTarget {
            section,
            pinned_order: ids(pinned_order),
            active_order: ids(active_order),
        };
        // Pinning an active thread between the pinned ones.
        let plan = plan_drop(
            &source("a1", Section::Active, false),
            &target(Section::Pinned, &["p1", "a1", "p2"], &["a2"]),
        );
        assert_eq!(
            plan,
            DropPlan::Pin {
                order_key: Some("l".into()),
                extra: Vec::new()
            }
        );
        let commands = drop_commands("a1", &plan)
            .iter()
            .map(|command| {
                let mut command = wire(command);
                command.as_object_mut().unwrap().remove("commandId");
                command
            })
            .collect::<Vec<_>>();
        assert_eq!(
            commands,
            [json!({ "type": "thread.pin", "threadId": "a1", "orderKey": "l" })]
        );
        // Unpinning to the top of Active, whose threads have no keys yet: they get some.
        let plan = plan_drop(
            &source("p2", Section::Pinned, true),
            &target(Section::Active, &["p1"], &["p2", "a1", "a2"]),
        );
        assert_eq!(
            plan,
            DropPlan::MoveActive {
                assignments: vec![
                    ("p2".into(), "gn".into()),
                    ("a1".into(), "nb".into()),
                    ("a2".into(), "tn".into())
                ],
                unpin: true,
                unsettle: false,
                unsnooze: false,
            }
        );
        // Settling, waking, and nothing to do.
        assert_eq!(
            plan_drop(
                &source("a1", Section::Active, false),
                &target(Section::Settled, &["p1", "p2"], &["a2"])
            ),
            DropPlan::Settle
        );
        let woken = plan_drop(
            &source("s1", Section::Snoozed, false),
            &target(Section::Active, &["p1", "p2"], &["a1", "s1", "a2"]),
        );
        assert!(matches!(
            woken,
            DropPlan::MoveActive {
                unsnooze: true,
                unpin: false,
                ..
            }
        ));
        assert_eq!(
            plan_drop(
                &source("a1", Section::Active, false),
                &target(Section::Active, &["p1", "p2"], &["a1", "a2"])
            ),
            DropPlan::None
        );
        assert_eq!(
            plan_drop(
                &source("p1", Section::Pinned, true),
                &target(Section::Pinned, &["p1", "p2"], &["a1", "a2"])
            ),
            DropPlan::None
        );
    }

    #[test]
    fn undo_puts_threads_back_as_t3_does() {
        let thread = |fields: Value| {
            let mut thread = json!({
                "id": "t", "projectId": "p", "title": "T", "status": "completed",
                "createdAt": "2026-10-27T09:00:00.000Z", "updatedAt": "2026-10-27T09:30:00.000Z",
                "createdBy": "user", "creationSource": "web", "interactionMode": "default",
                "runtimeMode": "full-access",
                "modelSelection": { "instanceId": "codex", "model": "gpt-5" },
                "providerInstanceId": "codex", "activeProviderThreadId": null, "activeRunId": null,
                "archivedAt": null, "branch": null, "deletedAt": null, "forkedFrom": null,
                "hasActionableProposedPlan": false, "itemCount": 0, "latestRunId": null,
                "latestUserMessageAt": null, "latestVisibleMessage": null,
                "lineage": { "rootThreadId": "t", "parentThreadId": null, "relationshipToParent": null },
                "pendingRuntimeRequest": null, "settledAt": null, "settledOverride": null,
                "visibleItemCount": 0, "worktreePath": null
            });
            thread
                .as_object_mut()
                .unwrap()
                .extend(fields.as_object().unwrap().clone());
            serde_json::from_value::<OrchestrationV2ThreadShell>(thread).unwrap()
        };
        let pinned_and_snoozed = thread(json!({
            "pinnedAt": "2026-10-27T09:00:00.000Z", "pinOrderKey": "h",
            "snoozedUntil": "2026-10-27T15:00:00.000Z"
        }));
        assert_eq!(
            undo_steps(UndoKind::Settled, Some(&pinned_and_snoozed)),
            [
                UndoStep::Unsettle,
                UndoStep::Pin {
                    order_key: Some("h".into())
                },
                UndoStep::Snooze {
                    until: "2026-10-27T15:00:00.000Z".into()
                }
            ]
        );
        assert_eq!(
            undo_steps(
                UndoKind::Unpinned,
                Some(&thread(json!({ "pinnedAt": "2026-10-27T09:00:00.000Z" })))
            ),
            [UndoStep::Pin { order_key: None }]
        );
        assert_eq!(undo_steps(UndoKind::Snoozed, None), [UndoStep::Unsnooze]);
        // A snooze that ended meanwhile isn't put back.
        let steps = undo_steps(UndoKind::Settled, Some(&pinned_and_snoozed));
        let tags = |now| {
            undo_commands("t", &steps, now)
                .iter()
                .map(|command| command.tag().unwrap_or_default().to_string())
                .collect::<Vec<_>>()
        };
        assert_eq!(
            tags(time::macros::datetime!(2026-10-27 12:00 UTC)),
            ["thread.unsettle", "thread.pin", "thread.snooze"]
        );
        assert_eq!(
            tags(time::macros::datetime!(2026-10-27 16:00 UTC)),
            ["thread.unsettle", "thread.pin"]
        );
    }

    #[test]
    fn archived_threads_list_newest_first() {
        let snapshot = serde_json::from_value::<t3_api::types::OrchestrationV2ArchivedShellSnapshot>(json!({
            "schemaVersion": 1, "snapshotSequence": 3,
            "projects": [{
                "id": "p", "title": "api", "workspaceRoot": "/src/api", "scripts": [],
                "createdAt": "2026-10-01T00:00:00.000Z", "updatedAt": "2026-10-01T00:00:00.000Z",
                "defaultModelSelection": null
            }],
            "threads": []
        }))
        .unwrap();
        assert!(archived_threads(&snapshot, &HashMap::default()).is_empty());
        let now = time::macros::datetime!(2026-10-27 12:00 UTC);
        assert_eq!(
            time_ago("2026-10-27T11:59:40.000Z", now).as_deref(),
            Some("just now")
        );
        assert_eq!(
            time_ago("2026-10-27T11:15:00.000Z", now).as_deref(),
            Some("45m ago")
        );
        assert_eq!(
            time_ago("2026-10-27T07:00:00.000Z", now).as_deref(),
            Some("5h ago")
        );
        assert_eq!(
            time_ago("2026-10-24T12:00:00.000Z", now).as_deref(),
            Some("3d ago")
        );
    }
}

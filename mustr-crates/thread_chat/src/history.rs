//! Going back in a thread (`mustr-docs/PARITY.md` §2.7 and §2.8), as T3's
//! `chat/useTimelineRewind.ts` does it:
//! - **Edit from here** on a user message rewinds the thread to before it
//!   (`checkpoint.rollback`), after the user picks whether to revert the files too, then puts
//!   the message back in the composer.
//! - **Fork from this response** on an agent message starts a new thread from its run
//!   (`thread.fork`) and opens it.
//! - **Compact context** asks the agent to compact its context, by sending `/compact` as T3's
//!   context meter does.
//! - **Restart agent session** detaches the thread's agent sessions
//!   (`provider-session.detach`), so its next message starts a fresh one that picks up new skills
//!   and MCP servers, as T3's command palette does.
//!
//! Every write goes through the view's [`ThreadBackend`](crate::ThreadBackend), so dry runs and
//! read-only pairings refuse them, and the buttons say why.

use crate::{
    backend::{Access, ThreadBackendExt as _},
    commands::{self, CommandIds, Delivery},
    model::{ItemKind, RevertTarget, ThreadState, TimelineItem},
    organize::active_thread_view,
    shell::ThreadShells,
    thread_view::{Loadable, ThreadView},
};
use collections::HashSet;
use futures::{FutureExt as _, channel::oneshot};
use gpui::{
    AnyElement, App, Context, Entity, Focusable as _, IntoElement, ParentElement as _, PromptLevel,
    SharedString, Styled as _, Window, actions, div,
};
use std::time::Duration;
use t3_api::{
    rpc,
    tagged::{OrchestrationV2Command, OrchestrationV2ThreadForkSourcePoint},
    types::{
        OrchestrationV2Actor, OrchestrationV2CommandCheckpointRollback,
        OrchestrationV2CommandCheckpointRollbackType, OrchestrationV2CommandProviderSessionDetach,
        OrchestrationV2CommandProviderSessionDetachType, OrchestrationV2CommandThreadFork,
        OrchestrationV2CommandThreadForkType, OrchestrationV2RunStatus,
        OrchestrationV2ThreadForkSourcePointRun, OrchestrationV2ThreadForkSourcePointRunType,
        ServerRefreshProvidersPayload,
    },
};
use ui::{Color, IconButton, IconName, IconSize, Tooltip, prelude::*};
use workspace::{Toast, Workspace, notifications::NotificationId};

actions!(
    thread_chat,
    [
        /// Asks the open agent thread's agent to compact its context (`/compact`).
        CompactContext,
        /// Restarts the open agent thread's agent session: its next message starts a fresh one,
        /// which picks up new skills and MCP servers.
        RestartAgentSession,
    ]
);

/// What compacting sends, as T3's Compact context does.
pub const COMPACT_COMMAND: &str = "/compact";

/// How long a rewind may take before mustr stops waiting for it (`waitForRevertedMessage` in
/// T3 allows two minutes).
const REWIND_TIMEOUT: Duration = Duration::from_secs(120);

/// What the user is asked before a rewind, with T3's choices (`RevertConfirmHost` in
/// `chat/ChatOverlayHosts.tsx`).
pub(crate) const REWIND_TITLE: &str = "Edit from here?";
pub(crate) const REWIND_DETAIL: &str = "Rewind the chat to before this message. The message returns \
     to the composer. “Revert Files Too” also undoes the changes to files since then, your own \
     edits included.";
pub(crate) const REWIND_ANSWERS: [&str; 3] =
    ["Revert Files Too", "Revert and Keep Changes", "Cancel"];

const THREAD_CHANGED: &str = "The thread has changed. Pick a message to rewind to again.";

// The commands. Each takes its command's id, so tests can fix it.

/// Rolls a thread back to a checkpoint (`checkpoint.rollback`), restoring the files it captured
/// when `restore_files` is set.
pub fn rollback(
    thread_id: &str,
    target: &RevertTarget,
    restore_files: bool,
    command_id: String,
) -> OrchestrationV2Command {
    OrchestrationV2Command::CheckpointRollback(OrchestrationV2CommandCheckpointRollback {
        checkpoint_id: target.checkpoint_id.clone(),
        command_id,
        restore_files: Some(restore_files),
        scope_id: target.scope_id.clone(),
        thread_id: thread_id.to_string(),
        type_: OrchestrationV2CommandCheckpointRollbackType::CheckpointRollback,
    })
}

/// Starts `target_thread_id` from a run of `source_thread_id` (`thread.fork`), as T3's
/// `forkThreadFromRun` does.
pub fn fork(
    source_thread_id: &str,
    target_thread_id: &str,
    run_id: &str,
    title: Option<String>,
    command_id: String,
) -> OrchestrationV2Command {
    OrchestrationV2Command::ThreadFork(OrchestrationV2CommandThreadFork {
        command_id,
        created_at: None,
        created_by: OrchestrationV2Actor::User,
        creation_source: commands::CREATION_SOURCE,
        source_point: OrchestrationV2ThreadForkSourcePoint::Run(
            OrchestrationV2ThreadForkSourcePointRun {
                run_id: run_id.to_string(),
                type_: OrchestrationV2ThreadForkSourcePointRunType::Run,
            },
        ),
        source_thread_id: source_thread_id.to_string(),
        target_thread_id: target_thread_id.to_string(),
        title,
        type_: OrchestrationV2CommandThreadForkType::ThreadFork,
    })
}

/// Detaches one of a thread's agent sessions (`provider-session.detach`), because the user asked,
/// as T3's `stopThreadSession` does.
pub fn detach_session(
    thread_id: &str,
    provider_session_id: &str,
    command_id: String,
) -> OrchestrationV2Command {
    OrchestrationV2Command::ProviderSessionDetach(OrchestrationV2CommandProviderSessionDetach {
        command_id,
        provider_session_id: provider_session_id.to_string(),
        reason: Some("client-requested".to_string()),
        thread_id: thread_id.to_string(),
        type_: OrchestrationV2CommandProviderSessionDetachType::ProviderSessionDetach,
    })
}

/// The commands that restart a thread's agent: one detach per session, with ids derived from
/// one command id as T3 derives them (`{id}:detach:{session}`).
pub fn restart_session(state: &ThreadState, command_id: &str) -> Vec<OrchestrationV2Command> {
    state
        .provider_sessions()
        .iter()
        .map(|session| {
            detach_session(
                state.thread_id(),
                &session.id,
                format!("{command_id}:detach:{}", session.id),
            )
        })
        .collect()
}

/// Asks the agent to compact its context: `/compact`, sent as a message.
pub fn compact(thread_id: &str, ids: CommandIds) -> OrchestrationV2Command {
    commands::send_message(thread_id, COMPACT_COMMAND, Delivery::Auto, ids)
}

// A thread view's state.

/// What a thread view keeps for going back and for its agent's status and usage.
#[derive(Default)]
pub(crate) struct HistoryState {
    /// The rewind in flight, until T3 has rolled the thread back.
    rewind: Option<Rewind>,
    /// Agent messages being forked from, by item id.
    forking: HashSet<String>,
    /// While the agent's sessions are being detached.
    restarting: bool,
    /// T3's server config and shell, which the view follows for its banners once its header
    /// shows.
    pub(crate) configs: Option<Entity<crate::usage::ServerConfigs>>,
    pub(crate) shells: Option<Entity<ThreadShells>>,
    /// Where each user message can be rewound to, by item id, worked out when the thread changes.
    revert_targets: collections::HashMap<String, RevertTarget>,
    /// The provider status and usage banners' state (`usage.rs`).
    pub(crate) usage: crate::usage::UsageState,
}

struct Rewind {
    /// The rollback's command id, which T3 names a failed rollback by.
    command_id: String,
    /// The run whose rolling back shows the rewind happened.
    witness_run_id: String,
    done: Option<oneshot::Sender<Result<(), String>>>,
}

impl ThreadView {
    /// Starts following what the header's banners show: T3's server config, for the agent's
    /// status and usage, and its shell, for usage-limit recovery. Once per view.
    pub(crate) fn observe_history(&mut self, cx: &mut Context<Self>) {
        if self.history.configs.is_some() {
            return;
        }
        let configs = crate::usage::ServerConfigs::global(cx);
        self._subscriptions
            .push(cx.observe(&configs, |_, _, cx| cx.notify()));
        // Every thread's changes come through the shell, so only this one's limit redraws it.
        let shells = ThreadShells::global(cx);
        self._subscriptions
            .push(cx.observe(&shells, |this, _, cx| this.sync_limit_recovery(cx)));
        self.history.configs = Some(configs);
        self.history.shells = Some(shells);
        self.sync_limit_recovery(cx);
    }

    /// Brings the view's notes on its history up to date with the thread: where its messages
    /// can be rewound to, and whether a rewind in flight has happened. Called whenever the
    /// thread changes.
    pub(crate) fn sync_history(&mut self) {
        if let Loadable::Loaded(state) = &self.state {
            self.history.revert_targets = state.revert_targets();
        }
        self.check_rewind();
    }

    pub(crate) fn is_rewinding(&self) -> bool {
        self.history.rewind.is_some()
    }

    /// Whether the thread's agent can rewind its conversation, as its status in T3's config
    /// says (`supportsConversationRollback`, which T3 takes to be true unless it says no).
    fn supports_rollback(&self, cx: &App) -> bool {
        self.provider(cx)
            .is_some_and(|provider| provider.supports_conversation_rollback != Some(false))
    }

    /// Where each user message can be rewound to, when the agent can rewind at all.
    pub(crate) fn revert_target(&self, item_id: &str, cx: &App) -> Option<RevertTarget> {
        if !self.supports_rollback(cx) {
            return None;
        }
        self.history.revert_targets.get(item_id).cloned()
    }

    /// Why going back can't start now, if it can't: writes are refused, or the agent is busy.
    fn rewind_blocked(&self) -> Option<String> {
        if let Some(reason) = self.backend.blocked_reason(Access::Operate) {
            return Some(reason);
        }
        let state = self.thread_state()?;
        if state.is_running() || self.sending {
            return Some("Stop the agent's run before rewinding".to_string());
        }
        if self.is_rewinding() {
            return Some("Rewinding the thread…".to_string());
        }
        None
    }

    /// "Edit from here": asks whether to revert the files too, then rewinds.
    pub(crate) fn edit_from_here(
        &mut self,
        item_id: String,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if self.revert_target(&item_id, cx).is_none() {
            return;
        }
        if let Some(reason) = self.rewind_blocked() {
            self.error = Some(format!("Couldn't rewind: {reason}").into());
            cx.notify();
            return;
        }
        let answer = window.prompt(
            PromptLevel::Warning,
            REWIND_TITLE,
            Some(REWIND_DETAIL),
            &REWIND_ANSWERS,
            cx,
        );
        self._tasks.push(cx.spawn_in(window, async move |this, cx| {
            let restore_files = match answer.await {
                Ok(0) => true,
                Ok(1) => false,
                _ => return,
            };
            this.update_in(cx, |this, window, cx| {
                this.rewind(item_id, restore_files, window, cx)
            })
            .ok();
        }));
    }

    /// Rolls the thread back to before a user message, waits until T3 has, and puts the message
    /// back in the composer (`rewindToMessage` in T3's `chat/chatViewSend.ts`).
    fn rewind(
        &mut self,
        item_id: String,
        restore_files: bool,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if let Some(reason) = self.rewind_blocked() {
            self.error = Some(format!("Couldn't rewind: {reason}").into());
            cx.notify();
            return;
        }
        // The thread may have moved on while the user decided.
        let target = self.revert_target(&item_id, cx);
        let Some(state) = self.thread_state() else {
            return;
        };
        let text = state.items().iter().find_map(|item| match &item.kind {
            ItemKind::UserMessage { text, .. } if item.id == item_id => Some(text.clone()),
            _ => None,
        });
        let witness = target
            .as_ref()
            .and_then(|target| state.rewind_witness(target.turn_count));
        let (Some(target), Some(text), Some(witness)) = (target, text, witness) else {
            self.error = Some(THREAD_CHANGED.into());
            cx.notify();
            return;
        };
        let command_id = commands::new_id();
        let command = rollback(
            state.thread_id(),
            &target,
            restore_files,
            command_id.clone(),
        );
        let (done, finished) = oneshot::channel();
        self.history.rewind = Some(Rewind {
            command_id,
            witness_run_id: witness.id.clone(),
            done: Some(done),
        });
        let dispatch = self.backend.dispatch(command);
        self.error = None;
        cx.notify();
        self._tasks.push(cx.spawn_in(window, async move |this, cx| {
            let outcome = match dispatch.await {
                Err(error) => Err(format!("{error:#}")),
                Ok(_) => {
                    // The thread's events may have come before T3's answer.
                    this.update(cx, |this, _| this.check_rewind()).ok();
                    let timeout = cx.background_executor().timer(REWIND_TIMEOUT);
                    futures::select_biased! {
                        outcome = finished.fuse() => outcome.unwrap_or(Ok(())),
                        _ = timeout.fuse() => {
                            Err("Timed out waiting for the thread to rewind.".to_string())
                        }
                    }
                }
            };
            this.update_in(cx, |this, window, cx| {
                this.history.rewind = None;
                match outcome {
                    Ok(()) => this.restore_prompt(&text, window, cx),
                    Err(error) => this.error = Some(format!("Couldn't rewind: {error}").into()),
                }
                cx.notify();
            })
            .ok();
        }));
    }

    /// Settles the rewind in flight once T3 has rolled its runs back, or said it couldn't.
    fn check_rewind(&mut self) {
        let (Some(rewind), Loadable::Loaded(state)) = (&mut self.history.rewind, &self.state)
        else {
            return;
        };
        let failure = state
            .thread
            .rollback_failure
            .as_ref()
            .filter(|failure| failure.request_id == rewind.command_id);
        let outcome = if let Some(failure) = failure {
            Some(Err(failure.message.clone()))
        } else if state
            .run_by_id(&rewind.witness_run_id)
            .is_some_and(|run| run.status == OrchestrationV2RunStatus::RolledBack)
        {
            Some(Ok(()))
        } else {
            None
        };
        if let Some(outcome) = outcome
            && let Some(done) = rewind.done.take()
        {
            done.send(outcome).ok();
        }
    }

    /// Puts a rewound message back in the composer, after anything already there.
    fn restore_prompt(&mut self, text: &str, window: &mut Window, cx: &mut Context<Self>) {
        let restored = text.trim();
        if restored.is_empty() {
            return;
        }
        let current = self.composer.read(cx).text(cx);
        let prompt = if current.trim().is_empty() {
            restored.to_string()
        } else {
            format!("{current}\n\n{restored}")
        };
        self.composer.update(cx, |editor, cx| {
            editor.set_text(prompt, window, cx);
            editor.move_to_end(&editor::actions::MoveToEnd, window, cx);
        });
        window.focus(&self.composer.focus_handle(cx), cx);
    }

    /// "Fork from this response": starts a new thread from the message's run and opens it.
    pub(crate) fn fork_from(
        &mut self,
        item_id: String,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(state) = self.thread_state() else {
            return;
        };
        let Some(item) = state.items().iter().find(|item| item.id == item_id) else {
            return;
        };
        let Some(run_id) = item.run_id.clone().filter(|_| state.can_fork(item)) else {
            return;
        };
        // An inherited answer forks the thread it came from.
        let source_thread_id = item
            .source_thread_id
            .clone()
            .unwrap_or_else(|| state.thread_id().to_string());
        if let Some(reason) = self.backend.blocked_reason(Access::Operate) {
            self.error = Some(format!("Couldn't fork the thread: {reason}").into());
            cx.notify();
            return;
        }
        if !self.history.forking.insert(item_id.clone()) {
            return;
        }
        let title = format!("{} fork", self.title());
        let target_thread_id = commands::new_id();
        let command = fork(
            &source_thread_id,
            &target_thread_id,
            &run_id,
            Some(title.clone()),
            commands::new_id(),
        );
        let dispatch = self.backend.dispatch(command);
        self.error = None;
        cx.notify();
        self._tasks.push(cx.spawn_in(window, async move |this, cx| {
            let result = dispatch.await;
            this.update_in(cx, |this, window, cx| {
                this.history.forking.remove(&item_id);
                match result {
                    Ok(_) => {
                        if let Some(workspace) = Workspace::for_window(window, cx) {
                            crate::open_thread(
                                target_thread_id,
                                title.into(),
                                workspace.downgrade(),
                                window,
                                cx,
                            );
                        }
                    }
                    Err(error) => {
                        this.error = Some(format!("Couldn't fork the thread: {error:#}").into())
                    }
                }
                cx.notify();
            })
            .ok();
        }));
    }

    /// Why Compact context can't run now, if it can't (`compactDisabledReason` in T3's
    /// `ChatView.tsx`).
    pub(crate) fn compact_blocked(&self, cx: &App) -> Option<String> {
        if let Some(reason) = self.backend.blocked_reason(Access::Operate) {
            return Some(reason);
        }
        let Some(state) = self.thread_state() else {
            return Some("Compacting is unavailable right now".into());
        };
        let supported = self
            .provider(cx)
            .is_some_and(crate::usage::supports_compaction);
        if !supported {
            return Some("Compaction is unavailable for this agent".into());
        }
        let busy = state.is_running()
            || self.sending
            || self.is_rewinding()
            || !state.pending_requests().is_empty()
            || state.actionable_plan().is_some();
        if busy || !state.has_compactable_conversation() {
            return Some("Compacting is unavailable right now".into());
        }
        None
    }

    /// Compact context: sends `/compact` to the agent.
    pub(crate) fn compact_context(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.compact_blocked(cx).is_some() {
            return;
        }
        let Some(thread_id) = self.thread_id.clone() else {
            return;
        };
        let command = compact(&thread_id, CommandIds::new());
        self.dispatch_in_order(vec![command], window, cx, |this, result, _, _| {
            if let Err(error) = result {
                this.error = Some(format!("Couldn't compact the context: {error:#}").into());
            }
        });
    }

    pub(crate) fn is_restarting(&self) -> bool {
        self.history.restarting
    }

    /// Restart agent session: detaches the thread's agent sessions, then has T3 check its agents
    /// again, as T3's command palette does.
    pub(crate) fn restart_agent_session(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(state) = self.thread_state() else {
            return;
        };
        if self.history.restarting {
            return;
        }
        if let Some(reason) = self.backend.blocked_reason(Access::Operate) {
            self.error = Some(format!("Couldn't restart the agent session: {reason}").into());
            cx.notify();
            return;
        }
        let detaches = restart_session(state, &commands::new_id());
        let project_id = state.thread.project_id.clone();
        let worktree = state.thread.worktree_path.clone();
        let instance_id = crate::usage::thread_instance_id(state).to_string();
        let shells = ThreadShells::global(cx);
        let worktree = worktree.or_else(|| {
            let shell = shells.read(cx).shell()?;
            shell
                .projects
                .get(&project_id)
                .map(|project| project.workspace_root.clone())
        });
        let refresh = ServerRefreshProvidersPayload {
            cwd: worktree,
            fresh: Some(true),
            instance_id: Some(instance_id),
            refresh_models: None,
        };
        let backend = self.backend.clone();
        self.history.restarting = true;
        self.error = None;
        cx.notify();
        self._tasks.push(cx.spawn_in(window, async move |this, cx| {
            let mut result = Ok(());
            for command in detaches {
                if let Err(error) = backend.dispatch(command).await {
                    result = Err(error);
                    break;
                }
            }
            if result.is_ok() {
                // A failed check leaves the agents as they were; the restart still happens.
                let refreshed = backend
                    .call::<rpc::ServerRefreshProviders>(refresh, Access::Read)
                    .await;
                if let Err(error) = refreshed {
                    log::warn!("couldn't check T3's agents again: {error:#}");
                }
            }
            this.update_in(cx, |this, window, cx| {
                this.history.restarting = false;
                match result {
                    Ok(()) => show_toast(
                        "Agent session will restart. Your next message starts a fresh session.",
                        window,
                        cx,
                    ),
                    Err(error) => {
                        this.error =
                            Some(format!("Couldn't restart the agent session: {error:#}").into())
                    }
                }
                cx.notify();
            })
            .ok();
        }));
    }

    /// "Edit from here" for a user message: shown on hover, when the message can be rewound to.
    pub(crate) fn render_edit_from_here(
        &self,
        item: &TimelineItem,
        cx: &mut Context<Self>,
    ) -> Option<AnyElement> {
        self.revert_target(&item.id, cx)?;
        let blocked = self.rewind_blocked();
        let item_id = item.id.clone();
        Some(
            message_action(&item.id, "edit")
                .child(
                    IconButton::new(
                        SharedString::from(format!("edit-from-{}", item.id)),
                        IconName::Undo,
                    )
                    .icon_size(IconSize::XSmall)
                    .icon_color(Color::Muted)
                    .disabled(blocked.is_some())
                    .tooltip(Tooltip::text(match &blocked {
                        Some(reason) => format!("Edit from here: {reason}"),
                        None => "Edit from here".to_string(),
                    }))
                    .on_click(cx.listener(move |this, _, window, cx| {
                        this.edit_from_here(item_id.clone(), window, cx)
                    })),
                )
                .into_any_element(),
        )
    }

    /// "Fork from this response" for an agent message: shown on hover, when it can be forked.
    pub(crate) fn render_fork(
        &self,
        item: &TimelineItem,
        cx: &mut Context<Self>,
    ) -> Option<AnyElement> {
        let state = self.thread_state()?;
        if !state.can_fork(item) {
            return None;
        }
        let blocked = self.backend.blocked_reason(Access::Operate);
        let busy = self.history.forking.contains(&item.id);
        let item_id = item.id.clone();
        Some(
            message_action(&item.id, "fork")
                .child(
                    IconButton::new(
                        SharedString::from(format!("fork-from-{}", item.id)),
                        IconName::GitBranch,
                    )
                    .icon_size(IconSize::XSmall)
                    .icon_color(Color::Muted)
                    .disabled(blocked.is_some() || busy)
                    .tooltip(Tooltip::text(match &blocked {
                        Some(reason) => format!("Fork from this response: {reason}"),
                        None if busy => "Forking…".to_string(),
                        None => "Fork from this response".to_string(),
                    }))
                    .on_click(cx.listener(move |this, _, window, cx| {
                        this.fork_from(item_id.clone(), window, cx)
                    })),
                )
                .into_any_element(),
        )
    }
}

/// The frame of a message's hover action, with a selector for tests.
fn message_action(item_id: &str, action: &str) -> gpui::Div {
    let selector = format!("thread-{action}-{item_id}");
    div()
        .flex_none()
        .debug_selector(|| selector)
        .visible_on_hover(SharedString::from(format!("message-{item_id}")))
}

struct HistoryToast;

/// Tells the user how something went, in the workspace's toast.
fn show_toast(message: &str, window: &mut Window, cx: &mut App) {
    let Some(workspace) = Workspace::for_window(window, cx) else {
        return;
    };
    let message = message.to_string();
    workspace.update(cx, |workspace, cx| {
        workspace.show_toast(
            Toast::new(NotificationId::unique::<HistoryToast>(), message).autohide(),
            cx,
        )
    });
}

// The palette's actions, on the thread the workspace shows.

pub(crate) fn init(cx: &mut App) {
    cx.observe_new(|workspace: &mut Workspace, _, _| register(workspace))
        .detach();
}

fn register(workspace: &mut Workspace) {
    workspace.register_action(|workspace, _: &CompactContext, window, cx| {
        if let Some(view) = active_thread_view(workspace, cx) {
            view.update(cx, |view, cx| view.compact_context(window, cx));
        }
    });
    workspace.register_action(|workspace, _: &RestartAgentSession, window, cx| {
        if let Some(view) = active_thread_view(workspace, cx) {
            view.update(cx, |view, cx| view.restart_agent_session(window, cx));
        }
    });
}

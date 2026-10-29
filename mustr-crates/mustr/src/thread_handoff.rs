//! Moving a thread's work to another thread or agent (`mustr-docs/PARITY.md` §2.8), from the
//! thread toolbar:
//!
//! - **Merge back** brings a fork's result back to the thread it was forked from
//!   (`thread.merge_back`), as T3's `chat/ThreadRelationshipsControl.tsx` does: from the fork's
//!   latest finished run, into its parent, which then opens. T3 starts nothing: the parent's next
//!   message carries the fork's work to its agent.
//! - **Hand off to another agent** switches a thread to another agent
//!   (`thread.model-selection.set`, as picking another agent in the settings bar does), when the
//!   thread's agent can hand its context over (`threadSupportsProviderHandoff` in T3's
//!   `client-runtime`). T3 plans the switch, and the thread's next message carries a summary of
//!   the conversation to the new agent ("Context handoff" in T3's timeline). Until then the
//!   toolbar says a handoff is pending (`resolvePendingHandoff`).

use gpui::{App, SharedString, Task, WeakEntity, Window};
use t3_client::t3_api::types::{
    ModelSelection, OrchestrationV2ThreadHistoryOrigin, ServerProvider,
};
use t3_client::t3_api::{
    tagged::{
        OrchestrationV2Command, OrchestrationV2ThreadForkSourcePoint,
        OrchestrationV2ThreadShellForkedFrom,
    },
    types::{
        OrchestrationV2Actor, OrchestrationV2AppThreadLineageRelationshipToParent,
        OrchestrationV2CommandThreadMergeBack, OrchestrationV2CommandThreadMergeBackType,
        OrchestrationV2Run, OrchestrationV2RunStatus, OrchestrationV2ThreadForkSourcePointRun,
        OrchestrationV2ThreadForkSourcePointRunType, OrchestrationV2ThreadShell,
    },
};
use thread_chat::{
    Access, ThreadShells,
    model::ThreadState,
    thread_settings::{self, AgentChoice},
};
use workspace::{Toast, Workspace, notifications::NotificationId};

// ---------------------------------------------------------------------------------------------
// Merge back

/// The thread a fork merges back into: the thread of the run it was forked from, or its parent
/// (`resolveMergeBackTargetThreadId` in T3). Only forks merge back.
pub fn merge_back_target(thread: &OrchestrationV2ThreadShell) -> Option<String> {
    if thread.lineage.relationship_to_parent
        != Some(OrchestrationV2AppThreadLineageRelationshipToParent::Fork)
    {
        return None;
    }
    match &thread.forked_from {
        Some(OrchestrationV2ThreadShellForkedFrom::Run(run)) => Some(run.thread_id.clone()),
        _ => thread.lineage.parent_thread_id.clone(),
    }
}

/// The run a fork merges back from: its latest run that finished (or is capturing its
/// checkpoint, `waiting`), unless a newer one is under way (`resolveLatestMergeBackRun` in T3).
pub fn latest_merge_back_run(runs: &[OrchestrationV2Run]) -> Option<&OrchestrationV2Run> {
    let latest = runs
        .iter()
        .filter(|run| {
            matches!(
                run.status,
                OrchestrationV2RunStatus::Waiting | OrchestrationV2RunStatus::Completed
            )
        })
        .max_by_key(|run| run.ordinal)?;
    let newer_active = runs.iter().any(|run| {
        run.ordinal > latest.ordinal
            && matches!(
                run.status,
                OrchestrationV2RunStatus::Preparing
                    | OrchestrationV2RunStatus::Starting
                    | OrchestrationV2RunStatus::Running
            )
    });
    (!newer_active).then_some(latest)
}

/// Merges `source_thread_id`'s run back into `target_thread_id` (`thread.merge_back`), as T3's
/// `mergeThreadBack` sends it.
pub fn merge_back_command(
    source_thread_id: &str,
    target_thread_id: &str,
    run_id: &str,
    command_id: String,
) -> OrchestrationV2Command {
    OrchestrationV2Command::ThreadMergeBack(OrchestrationV2CommandThreadMergeBack {
        command_id,
        created_at: None,
        created_by: OrchestrationV2Actor::User,
        creation_source: thread_chat::commands::CREATION_SOURCE,
        source_point: OrchestrationV2ThreadForkSourcePoint::Run(
            OrchestrationV2ThreadForkSourcePointRun {
                run_id: run_id.to_string(),
                type_: OrchestrationV2ThreadForkSourcePointRunType::Run,
            },
        ),
        source_thread_id: source_thread_id.to_string(),
        target_thread_id: target_thread_id.to_string(),
        type_: OrchestrationV2CommandThreadMergeBackType::ThreadMergeBack,
    })
}

/// What merging a fork back would do, for its button.
#[derive(Clone, Debug, PartialEq)]
pub struct MergeBack {
    pub source_thread_id: String,
    pub target_thread_id: String,
    /// The parent's title, while T3 lists it.
    pub target_title: Option<String>,
    /// The run to merge back, once one has finished.
    pub run_id: Option<String>,
}

impl MergeBack {
    /// A fork's merge back, from T3's shell and, once the fork's view has loaded it, its runs.
    pub fn of_thread(
        thread_id: &str,
        state: Option<&ThreadState>,
        shells: &ThreadShells,
    ) -> Option<Self> {
        let thread = shells.thread(thread_id)?;
        let target_thread_id = merge_back_target(thread)?;
        let target_title = shells
            .thread(&target_thread_id)
            .map(|target| target.title.clone());
        let run_id = state
            .filter(|state| state.thread_id() == thread_id)
            .and_then(|state| latest_merge_back_run(state.runs()))
            .map(|run| run.id.clone());
        Some(Self {
            source_thread_id: thread_id.to_string(),
            target_thread_id,
            target_title,
            run_id,
        })
    }

    pub fn label(&self) -> SharedString {
        "Merge back".into()
    }

    /// What the button's tooltip says, as T3's does.
    pub fn tooltip(&self) -> String {
        if self.run_id.is_none() {
            return "Complete a run in this fork before merging it back".to_string();
        }
        match &self.target_title {
            Some(title) => format!(
                "Merge this conversation back into “{title}”: its next message brings this \
                 fork's work"
            ),
            None => "Merge this conversation back into its source".to_string(),
        }
    }

    /// Sends the merge back, then opens the parent; or says in a toast why it couldn't. The
    /// task ends once T3 has answered.
    pub fn run(
        self,
        workspace: WeakEntity<Workspace>,
        window: &mut Window,
        cx: &mut App,
    ) -> Task<()> {
        let Some(run_id) = self.run_id.clone() else {
            return Task::ready(());
        };
        let Some(backend) = thread_chat::backend(cx) else {
            return Task::ready(());
        };
        if let Some(reason) = backend.blocked_reason(Access::Operate) {
            show_toast(&workspace, &self.source_thread_id, reason, cx);
            return Task::ready(());
        }
        let dispatch = backend.dispatch(merge_back_command(
            &self.source_thread_id,
            &self.target_thread_id,
            &run_id,
            thread_chat::commands::new_id(),
        ));
        window.spawn(cx, async move |cx| {
            let result = dispatch.await;
            cx.update(|window, cx| match result {
                Ok(_) => {
                    let title = self
                        .target_title
                        .clone()
                        .unwrap_or_else(|| "the source thread".to_string());
                    show_toast(
                        &workspace,
                        &self.source_thread_id,
                        format!(
                            "Merged back into “{title}”. Its next message brings the fork's work."
                        ),
                        cx,
                    );
                    thread_chat::open_thread(
                        self.target_thread_id.clone(),
                        title.into(),
                        workspace.clone(),
                        window,
                        cx,
                    );
                }
                Err(error) => {
                    log::error!("couldn't merge the fork back: {error:#}");
                    show_toast(
                        &workspace,
                        &self.source_thread_id,
                        format!("Couldn't merge back: {error:#}"),
                        cx,
                    );
                }
            })
            .ok();
        })
    }
}

// ---------------------------------------------------------------------------------------------
// Hand off to another agent

/// An agent and model, for the toolbar: `Codex · gpt-5.5`.
fn endpoint_label(selection: &ModelSelection, providers: &[ServerProvider]) -> String {
    let instance = thread_settings::selection_instance_id(selection).unwrap_or_default();
    let agent = providers
        .iter()
        .find(|provider| provider.instance_id == instance)
        .and_then(|provider| provider.display_name.clone())
        .unwrap_or_else(|| instance.to_string());
    match thread_settings::selection_model(selection) {
        Some(model) if !model.is_empty() => format!("{agent} · {model}"),
        _ => agent,
    }
}

/// A handoff the thread's next message makes: from the agent its last run used to the one it's
/// set to now.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PendingHandoff {
    pub from: String,
    pub to: String,
}

/// Whether the thread's next message hands its context to another agent
/// (`resolvePendingHandoff` in T3's `client-runtime`): its last run that started used another
/// agent (or a model the agent can't switch to in place), and the timeline has something to hand
/// over. mustr changes a thread's agent at once (`thread.model-selection.set`), so the thread's
/// own selection is the one the next message uses.
pub fn pending_handoff(
    state: &ThreadState,
    providers: &[ServerProvider],
) -> Option<PendingHandoff> {
    if state.items().is_empty() {
        return None;
    }
    let from = &state
        .runs()
        .iter()
        .filter(|run| run.status != OrchestrationV2RunStatus::RolledBack)
        .filter(|run| {
            run.started_at.is_some()
                || (run.status == OrchestrationV2RunStatus::Starting
                    && run.context_handoff_id.is_some())
        })
        .max_by_key(|run| run.ordinal)?
        .model_selection;
    let to = &state.thread.model_selection;
    let from_instance = thread_settings::selection_instance_id(from);
    let to_instance = thread_settings::selection_instance_id(to);
    let provider = |instance: Option<&str>| {
        providers
            .iter()
            .find(|provider| Some(provider.instance_id.as_str()) == instance)
    };
    let (source, target) = (provider(from_instance), provider(to_instance));
    let needs_handoff = if from_instance == to_instance {
        thread_settings::selection_model(from) != thread_settings::selection_model(to)
            && target
                .is_some_and(|target| target.requires_new_thread_for_model_change == Some(true))
    } else {
        let group = |provider: Option<&ServerProvider>| {
            provider
                .and_then(|provider| provider.continuation.as_ref())
                .map(|continuation| continuation.group_key.clone())
        };
        group(source).is_none()
            || source.map(|source| &source.driver) != target.map(|target| &target.driver)
            || group(source) != group(target)
    };
    needs_handoff.then(|| PendingHandoff {
        from: endpoint_label(from, providers),
        to: endpoint_label(to, providers),
    })
}

/// Whether the thread's agent can hand its context to another (`threadSupportsProviderHandoff`
/// in T3): its session says so; without one, it isn't running, and it's new, imported, or has a
/// conversation of its own to hand over.
pub fn supports_handoff(state: &ThreadState) -> bool {
    if let Some(session) = state.provider_session() {
        return session
            .capabilities
            .sessions
            .supports_provider_switching_via_handoff;
    }
    if state.active_run().is_some() {
        return false;
    }
    state.runs().is_empty()
        || state.thread.history_origin == Some(OrchestrationV2ThreadHistoryOrigin::V1Import)
        || state.thread.active_provider_thread_id.is_some()
}

/// The agents the thread can be handed to: every other agent T3 can run, on its default model.
pub fn handoff_targets(state: &ThreadState, providers: &[ServerProvider]) -> Vec<AgentChoice> {
    let current = thread_settings::selection_instance_id(&state.thread.model_selection)
        .unwrap_or(state.thread.provider_instance_id.as_str())
        .to_string();
    thread_settings::agent_choices(providers)
        .into_iter()
        .filter(|agent| agent.instance_id != current && agent.default_model.is_some())
        .collect()
}

/// Switches the thread to `agent`'s default model (`thread.model-selection.set`); T3 plans the
/// switch, and the next message carries the conversation over.
pub fn handoff_command(
    thread_id: &str,
    agent: &AgentChoice,
    command_id: String,
) -> Option<OrchestrationV2Command> {
    let model = &agent.models.get(agent.default_model?)?.slug;
    Some(thread_settings::set_model_selection(
        thread_id,
        thread_settings::model_selection(&agent.instance_id, model, &[]),
        command_id,
    ))
}

/// Hands the thread to `agent`, saying in a toast what happens next, or why it couldn't.
pub fn hand_off(
    thread_id: String,
    agent: AgentChoice,
    workspace: WeakEntity<Workspace>,
    cx: &mut App,
) {
    let Some(backend) = thread_chat::backend(cx) else {
        return;
    };
    let Some(command) = handoff_command(&thread_id, &agent, thread_chat::commands::new_id()) else {
        return;
    };
    if let Some(reason) = backend.blocked_reason(Access::Operate) {
        show_toast(&workspace, &thread_id, reason, cx);
        return;
    }
    let dispatch = backend.dispatch(command);
    cx.spawn(async move |cx| {
        let message = match dispatch.await {
            Ok(_) => format!(
                "The thread's next message goes to {}, with a summary of the conversation.",
                agent.name
            ),
            Err(error) => {
                log::error!("couldn't hand the thread off: {error:#}");
                format!("Couldn't hand off to {}: {error:#}", agent.name)
            }
        };
        cx.update(|cx| show_toast(&workspace, &thread_id, message, cx));
    })
    .detach();
}

fn show_toast(workspace: &WeakEntity<Workspace>, key: &str, message: String, cx: &mut App) {
    let id = NotificationId::composite::<MergeBack>(key.to_string());
    workspace
        .update(cx, |workspace, cx| {
            workspace.show_toast(Toast::new(id, message), cx)
        })
        .ok();
}

#[cfg(test)]
#[path = "thread_handoff_tests.rs"]
mod tests;

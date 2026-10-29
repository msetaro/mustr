//! Merge back, from a fork's thread toolbar (`mustr-docs/PARITY.md` §2.8): brings a fork's
//! result back to the thread it was forked from (`thread.merge_back`), as T3's
//! `chat/ThreadRelationshipsControl.tsx` does: from the fork's latest finished run, into its
//! parent, which then opens. T3 starts nothing: the parent's next message carries the fork's work
//! to its agent.

use gpui::{App, SharedString, Task, WeakEntity, Window};
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
use thread_chat::{Access, ThreadShells, model::ThreadState};
use workspace::{Toast, Workspace, notifications::NotificationId};

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

fn show_toast(workspace: &WeakEntity<Workspace>, key: &str, message: String, cx: &mut App) {
    let id = NotificationId::composite::<MergeBack>(key.to_string());
    workspace
        .update(cx, |workspace, cx| {
            workspace.show_toast(Toast::new(id, message), cx)
        })
        .ok();
}

#[cfg(test)]
#[path = "merge_back_tests.rs"]
mod tests;

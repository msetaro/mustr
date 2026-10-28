//! T3's shell for the whole app: every active thread and project as T3 sends them, kept current
//! by `orchestration.subscribeShell`. The threads sidebar keeps its own list of what it shows;
//! [`ThreadShells`] keeps T3's records whole (lineage, worktrees, pending work) for views that
//! need more of them, such as a thread's family bar and the worktrees view. It starts following
//! the shell when a view first asks for it.

use crate::backend::ThreadBackend;
use anyhow::Result;
use collections::HashMap;
use futures::StreamExt as _;
use gpui::{
    App, AppContext as _, AsyncApp, Context, Entity, Global, SharedString, Task, WeakEntity,
};
use std::sync::Arc;
use t3_api::{
    tagged::OrchestrationV2ShellStreamItem,
    types::{
        OrchestrationProjectShell, OrchestrationV2ShellSnapshot,
        OrchestrationV2ShellStreamItemThreadUpdatedLocation, OrchestrationV2SubscribeShellInput,
        OrchestrationV2ThreadShell,
    },
};

/// The shell's active threads and projects by id, kept the way T3's own client keeps them
/// (`applyShellStreamEvent` in T3's `packages/client-runtime/src/state/shellReducer.ts`).
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Shell {
    pub threads: HashMap<String, OrchestrationV2ThreadShell>,
    pub projects: HashMap<String, OrchestrationProjectShell>,
    pub sequence: i64,
}

impl Shell {
    pub fn from_snapshot(snapshot: &OrchestrationV2ShellSnapshot) -> Self {
        Self {
            threads: snapshot
                .threads
                .iter()
                .map(|thread| (thread.id.clone(), thread.clone()))
                .collect(),
            projects: snapshot
                .projects
                .iter()
                .map(|project| (project.id.clone(), project.clone()))
                .collect(),
            sequence: snapshot.snapshot_sequence,
        }
    }

    /// Applies one item of `orchestration.subscribeShell`. Returns whether anything changed.
    pub fn apply(&mut self, item: OrchestrationV2ShellStreamItem) -> bool {
        let sequence = match &item {
            OrchestrationV2ShellStreamItem::ThreadUpdated(update) => Some(update.sequence),
            OrchestrationV2ShellStreamItem::ThreadRemoved(removed) => Some(removed.sequence),
            OrchestrationV2ShellStreamItem::ProjectUpdated(update) => Some(update.sequence),
            OrchestrationV2ShellStreamItem::ProjectRemoved(removed) => Some(removed.sequence),
            _ => None,
        };
        // The snapshot already has what came before it.
        if let Some(sequence) = sequence {
            if sequence <= self.sequence {
                return false;
            }
            self.sequence = sequence;
        }
        match item {
            // A snapshot with resolved repository roots only refreshes the projects; on a resumed
            // subscription it has no threads at all (`ThreadList::apply` in `sidebar.rs`).
            OrchestrationV2ShellStreamItem::Snapshot(snapshot)
                if snapshot.resolved_repository_identity_roots.is_some() =>
            {
                self.projects.extend(
                    snapshot
                        .snapshot
                        .projects
                        .into_iter()
                        .map(|project| (project.id.clone(), project)),
                );
                true
            }
            OrchestrationV2ShellStreamItem::Snapshot(snapshot) => {
                *self = Self::from_snapshot(&snapshot.snapshot);
                true
            }
            // Archived threads have a shell of their own; T3 drops them from this one.
            OrchestrationV2ShellStreamItem::ThreadUpdated(update) => {
                if update.location == OrchestrationV2ShellStreamItemThreadUpdatedLocation::Active {
                    self.threads.insert(update.thread.id.clone(), update.thread);
                } else {
                    self.threads.remove(&update.thread.id);
                }
                true
            }
            OrchestrationV2ShellStreamItem::ThreadRemoved(removed) => {
                self.threads.remove(&removed.thread_id).is_some()
            }
            OrchestrationV2ShellStreamItem::ProjectUpdated(update) => {
                self.projects
                    .insert(update.project.id.clone(), update.project);
                true
            }
            OrchestrationV2ShellStreamItem::ProjectRemoved(removed) => {
                self.projects.remove(&removed.project_id).is_some()
            }
            OrchestrationV2ShellStreamItem::Synchronized(_)
            | OrchestrationV2ShellStreamItem::Unrecognized(_) => false,
        }
    }
}

/// The app's one follower of T3's shell. Views observe it to hear of changes.
pub struct ThreadShells {
    shell: Option<Shell>,
    error: Option<SharedString>,
    _follow: Option<Task<()>>,
}

struct GlobalThreadShells(Entity<ThreadShells>);

impl Global for GlobalThreadShells {}

impl ThreadShells {
    /// The app's shell, which starts following T3 through the installed backend on first use.
    pub fn global(cx: &mut App) -> Entity<Self> {
        if let Some(global) = cx.try_global::<GlobalThreadShells>() {
            return global.0.clone();
        }
        let shells = cx.new(|cx| {
            let mut shells = Self {
                shell: None,
                error: None,
                _follow: None,
            };
            shells.follow(cx);
            shells
        });
        cx.set_global(GlobalThreadShells(shells.clone()));
        shells
    }

    /// The shell, once it has loaded.
    pub fn shell(&self) -> Option<&Shell> {
        self.shell.as_ref()
    }

    /// One active thread's record.
    pub fn thread(&self, thread_id: &str) -> Option<&OrchestrationV2ThreadShell> {
        self.shell.as_ref()?.threads.get(thread_id)
    }

    /// Why the shell couldn't be followed the last time it was tried, until it can be.
    pub fn error(&self) -> Option<&SharedString> {
        self.error.as_ref()
    }

    fn follow(&mut self, cx: &mut Context<Self>) {
        let Some(backend) = crate::backend(cx) else {
            self.error = Some("mustr isn't set up to show agent threads".into());
            return;
        };
        self._follow = Some(cx.spawn(async move |this, cx| {
            let mut attempt = 0;
            loop {
                match follow_shell(&backend, &this, cx).await {
                    Ok(()) => attempt = 0,
                    Err(error) => {
                        log::warn!("couldn't follow T3's shell: {error:#}");
                        let updated = this.update(cx, |this, cx| {
                            this.error = Some(format!("{error:#}").into());
                            cx.notify();
                        });
                        if updated.is_err() {
                            return;
                        }
                    }
                }
                cx.background_executor()
                    .timer(crate::backend::retry_delay(attempt))
                    .await;
                attempt += 1;
            }
        }));
    }
}

async fn follow_shell(
    backend: &Arc<dyn ThreadBackend>,
    this: &WeakEntity<ThreadShells>,
    cx: &mut AsyncApp,
) -> Result<()> {
    let snapshot = backend.shell_snapshot().await?;
    let sequence = this.update(cx, |this, cx| {
        let shell = Shell::from_snapshot(&snapshot);
        let sequence = shell.sequence;
        this.shell = Some(shell);
        this.error = None;
        cx.notify();
        sequence
    })?;
    let mut stream = backend.subscribe_shell(OrchestrationV2SubscribeShellInput {
        after_sequence: Some(sequence),
        omit_idle_subagents: None,
        omit_older_settled: None,
        request_completion_marker: None,
    })?;
    while let Some(item) = stream.next().await {
        let item = item?;
        this.update(cx, |this, cx| {
            if let Some(shell) = &mut this.shell
                && shell.apply(item)
            {
                cx.notify();
            }
        })?;
    }
    Ok(())
}

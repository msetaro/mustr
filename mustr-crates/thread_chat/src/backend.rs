//! What the thread views ask of the app: reading threads and projects from T3, following a
//! thread's changes, and the writes that drive agents. The views know nothing about T3's
//! transport; the app implements [`ThreadBackend`] on its T3 connection, and tests implement it
//! with a recording fake.
//!
//! Every write goes through [`ThreadBackend::dispatch`] or [`ThreadBackend::launch_thread`],
//! which take T3's typed payloads, so a fake can assert exactly what would be sent.

use anyhow::Result;
use futures::{FutureExt as _, future::BoxFuture, stream::BoxStream};
use gpui::{App, Global};
use std::{sync::Arc, time::Duration};
use t3_api::{
    tagged::{
        OrchestrationV2Command, OrchestrationV2ShellStreamItem, OrchestrationV2ThreadStreamItem,
    },
    types::{
        OrchestrationV2DispatchCommandResult, OrchestrationV2GetTurnItemInput,
        OrchestrationV2GetTurnItemResult, OrchestrationV2ShellSnapshot,
        OrchestrationV2SubscribeShellInput, OrchestrationV2SubscribeThreadInput,
        OrchestrationV2ThreadDetailSnapshot, OrchestrationV2ThreadLaunchInput,
        OrchestrationV2ThreadLaunchResult, ServerProvider,
    },
};

pub trait ThreadBackend: Send + Sync + 'static {
    /// The thread list and projects (`GET /api/orchestration/shell`).
    fn shell_snapshot(&self) -> BoxFuture<'static, Result<OrchestrationV2ShellSnapshot>>;

    /// Follows the shell (`orchestration.subscribeShell`): a snapshot, then changes.
    fn subscribe_shell(
        &self,
        input: OrchestrationV2SubscribeShellInput,
    ) -> Result<BoxStream<'static, Result<OrchestrationV2ShellStreamItem>>>;

    /// One thread's projection (`GET /api/orchestration/threads/:threadId`).
    fn thread_snapshot(
        &self,
        thread_id: String,
    ) -> BoxFuture<'static, Result<OrchestrationV2ThreadDetailSnapshot>>;

    /// Follows one thread (`orchestration.subscribeThread`): events after
    /// `input.after_sequence`, or a snapshot first.
    fn subscribe_thread(
        &self,
        input: OrchestrationV2SubscribeThreadInput,
    ) -> Result<BoxStream<'static, Result<OrchestrationV2ThreadStreamItem>>>;

    /// One turn item in full (`orchestration.getTurnItem`), for the parts T3 leaves out of
    /// projections and events, such as a command's output.
    fn turn_item(
        &self,
        input: OrchestrationV2GetTurnItemInput,
    ) -> BoxFuture<'static, Result<OrchestrationV2GetTurnItemResult>>;

    /// The agents T3 can run, with their models (`server.getConfig`).
    fn providers(&self) -> BoxFuture<'static, Result<Vec<ServerProvider>>>;

    /// Sends a command (`orchestration.dispatchCommand`): messages, steering, interrupts,
    /// approvals, and queue changes. Needs the `orchestration:operate` scope.
    fn dispatch(
        &self,
        command: OrchestrationV2Command,
    ) -> BoxFuture<'static, Result<OrchestrationV2DispatchCommandResult>>;

    /// Starts a thread (`orchestration.launchThread`), with its worktree and first message.
    fn launch_thread(
        &self,
        input: OrchestrationV2ThreadLaunchInput,
    ) -> BoxFuture<'static, Result<OrchestrationV2ThreadLaunchResult>>;

    /// Why writes are refused before they're sent, if they are: the pairing lacks
    /// `orchestration:operate`, or writes are switched off for a dry run.
    fn write_blocked_reason(&self) -> Option<String> {
        None
    }
}

/// How long to wait before the `attempt`th retry of a read that failed, for example because
/// mustr hasn't connected to T3 yet.
pub(crate) fn retry_delay(attempt: usize) -> Duration {
    const DELAYS: [Duration; 4] = [
        Duration::from_secs(1),
        Duration::from_secs(2),
        Duration::from_secs(5),
        Duration::from_secs(10),
    ];
    DELAYS[attempt.min(DELAYS.len() - 1)]
}

/// The backend the thread views use, set by the app once it's connected to T3.
#[derive(Clone)]
pub struct GlobalThreadBackend(pub Arc<dyn ThreadBackend>);

impl Global for GlobalThreadBackend {}

/// Installs the backend every thread view and the sidebar use.
pub fn set_backend(backend: Arc<dyn ThreadBackend>, cx: &mut App) {
    cx.set_global(GlobalThreadBackend(backend));
}

/// The installed backend, if the app has set one.
pub fn backend(cx: &App) -> Option<Arc<dyn ThreadBackend>> {
    cx.try_global::<GlobalThreadBackend>()
        .map(|global| global.0.clone())
}

/// Passes reads through and refuses every write, for demos and screenshots against a real T3
/// server (`MUSTR_DRY_RUN_WRITES`). Nothing it refuses reaches T3.
pub struct ReadOnlyBackend {
    inner: Arc<dyn ThreadBackend>,
    reason: String,
}

impl ReadOnlyBackend {
    pub fn new(inner: Arc<dyn ThreadBackend>, reason: impl Into<String>) -> Self {
        Self {
            inner,
            reason: reason.into(),
        }
    }

    fn refuse<T: Send + 'static>(&self, what: &str) -> BoxFuture<'static, Result<T>> {
        log::warn!("{}, so {what} wasn't sent", self.reason);
        let error = anyhow::anyhow!("Not sent: {}.", self.reason);
        futures::future::ready(Err(error)).boxed()
    }
}

impl ThreadBackend for ReadOnlyBackend {
    fn shell_snapshot(&self) -> BoxFuture<'static, Result<OrchestrationV2ShellSnapshot>> {
        self.inner.shell_snapshot()
    }

    fn subscribe_shell(
        &self,
        input: OrchestrationV2SubscribeShellInput,
    ) -> Result<BoxStream<'static, Result<OrchestrationV2ShellStreamItem>>> {
        self.inner.subscribe_shell(input)
    }

    fn thread_snapshot(
        &self,
        thread_id: String,
    ) -> BoxFuture<'static, Result<OrchestrationV2ThreadDetailSnapshot>> {
        self.inner.thread_snapshot(thread_id)
    }

    fn subscribe_thread(
        &self,
        input: OrchestrationV2SubscribeThreadInput,
    ) -> Result<BoxStream<'static, Result<OrchestrationV2ThreadStreamItem>>> {
        self.inner.subscribe_thread(input)
    }

    fn turn_item(
        &self,
        input: OrchestrationV2GetTurnItemInput,
    ) -> BoxFuture<'static, Result<OrchestrationV2GetTurnItemResult>> {
        self.inner.turn_item(input)
    }

    fn providers(&self) -> BoxFuture<'static, Result<Vec<ServerProvider>>> {
        self.inner.providers()
    }

    fn dispatch(
        &self,
        command: OrchestrationV2Command,
    ) -> BoxFuture<'static, Result<OrchestrationV2DispatchCommandResult>> {
        let what = command.tag().unwrap_or("a command").to_string();
        self.refuse(&what)
    }

    fn launch_thread(
        &self,
        _input: OrchestrationV2ThreadLaunchInput,
    ) -> BoxFuture<'static, Result<OrchestrationV2ThreadLaunchResult>> {
        self.refuse("orchestration.launchThread")
    }

    fn write_blocked_reason(&self) -> Option<String> {
        Some(self.reason.clone())
    }
}

//! What the thread views ask of the app: reading threads and projects from T3, following a
//! thread's changes, and the writes that drive agents. The views know nothing about T3's
//! transport; the app implements [`ThreadBackend`] on its T3 connection, and tests implement it
//! with a recording fake.
//!
//! Every write goes through [`ThreadBackend::dispatch`], [`ThreadBackend::launch_thread`],
//! [`ThreadBackend::call_raw`], [`ThreadBackend::subscribe_raw`], or [`ThreadBackend::upload`],
//! so a fake can assert exactly what would be sent. Images in messages load through
//! [`ThreadBackend::fetch_asset`]. [`ThreadBackendExt`] types the raw calls
//! with the method tables in `t3_api::rpc`, so any of T3's RPCs can be called without a method
//! of its own here.

use anyhow::{Context as _, Result, anyhow, bail};
use futures::{FutureExt as _, StreamExt as _, future::BoxFuture, stream::BoxStream};
use gpui::{App, Global};
use serde_json::Value;
use std::{sync::Arc, time::Duration};
use t3_api::{
    RpcMethod,
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

/// What an RPC needs from mustr's pairing with T3: the scope T3 checks for the method
/// (`RPC_REQUIRED_SCOPES` in T3's `apps/server/src/auth/RpcAuthorization.ts`). Dry runs
/// (`MUSTR_DRY_RUN_WRITES`) refuse every call that isn't [`Access::Read`].
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Access {
    /// Needs `orchestration:read`, which every pairing has: lists, snapshots, settings, diffs,
    /// and git status. Goes through in dry runs.
    Read,
    /// Needs `orchestration:operate`: drives agents, and changes threads, projects,
    /// attachments, schedules, and editors. The few reads T3 guards with it, such as a webhook's
    /// delivery log, are `Operate` too, so dry runs refuse them as well.
    Operate,
    /// Needs `source-control:write`: commits, pushes, pull requests, branches, and worktrees.
    SourceControlWrite,
    /// Needs `filesystem:read`: searches the files on T3's machine, for the composer's `@` on a
    /// thread whose folder isn't on this one. A read, so dry runs let it through.
    FilesystemRead,
}

impl Access {
    /// Whether dry runs refuse calls that need this.
    pub fn is_write(self) -> bool {
        matches!(self, Self::Operate | Self::SourceControlWrite)
    }
}

/// An attachment's bytes, for [`ThreadBackend::upload`].
pub struct AttachmentUpload {
    /// The `relativeUrl` that `attachments.createUploadUrl` minted for it.
    pub relative_url: String,
    /// The MIME type it was minted for, such as `image/png`.
    pub mime_type: String,
    /// Exactly as many bytes as it was minted for: T3 refuses any other length.
    pub bytes: Vec<u8>,
    /// Called with the number of bytes sent so far, as they go.
    pub on_progress: Option<Arc<dyn Fn(u64) + Send + Sync>>,
}

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

    /// Calls one of T3's RPCs by its tag (`t3_api::rpc::X::TAG`), with its payload as JSON. It's
    /// the way to every RPC that has no method of its own here; prefer the typed
    /// [`ThreadBackendExt::call`]. The call is refused before it's sent when
    /// [`ThreadBackend::blocked_reason`] gives a reason for `access`.
    fn call_raw(
        &self,
        tag: &str,
        payload: Value,
        access: Access,
    ) -> BoxFuture<'static, Result<Value>>;

    /// Follows one of T3's streaming RPCs by its tag, with its payload as JSON, such as
    /// `git.runStackedAction`'s progress or `subscribeVcsStatus`. Prefer the typed
    /// [`ThreadBackendExt::stream`]. Refused like [`ThreadBackend::call_raw`].
    fn subscribe_raw(
        &self,
        tag: &str,
        payload: Value,
        access: Access,
    ) -> Result<BoxStream<'static, Result<Value>>>;

    /// Sends an attachment's bytes to the URL that `attachments.createUploadUrl` minted
    /// (`POST /api/attachments/upload/:token`). It needs `orchestration:operate`, which T3's app
    /// checks before uploading too, and dry runs refuse it.
    fn upload(&self, upload: AttachmentUpload) -> BoxFuture<'static, Result<()>>;

    /// Reads an asset, such as an image in a message, from the URL that `assets.createUrl`
    /// signed (`GET /api/assets/*`). Assets are reads, so dry runs let them through. Backends
    /// that can't reach T3's HTTP server leave it unsupported.
    fn fetch_asset(&self, relative_url: &str) -> BoxFuture<'static, Result<Vec<u8>>> {
        let error = anyhow!("mustr can't load {relative_url} from T3 here");
        futures::future::ready(Err(error)).boxed()
    }

    /// Why writes are refused before they're sent, if they are: the pairing lacks
    /// `orchestration:operate`, or writes are switched off for a dry run.
    fn write_blocked_reason(&self) -> Option<String> {
        None
    }

    /// Why calls that need `access` are refused before they're sent, if they are: the pairing
    /// lacks the scope, or writes are switched off for a dry run. By default, writes are refused
    /// for the reason [`ThreadBackend::write_blocked_reason`] gives.
    fn blocked_reason(&self, access: Access) -> Option<String> {
        if access.is_write() {
            self.write_blocked_reason()
        } else {
            None
        }
    }
}

/// Any of T3's RPCs on any [`ThreadBackend`], typed by the method tables in `t3_api::rpc` the way
/// `RpcConnection::call` and `stream` are: `backend.call::<rpc::ShellOpenInEditor>(input,
/// Access::Operate)`. They go through [`ThreadBackend::call_raw`] and
/// [`ThreadBackend::subscribe_raw`], so they honor dry runs and the pairing's scopes, and
/// recording backends see the JSON T3 would get.
pub trait ThreadBackendExt {
    /// Calls the non-streaming RPC `M`.
    fn call<M>(
        &self,
        payload: M::Payload,
        access: Access,
    ) -> BoxFuture<'static, Result<M::Success>>
    where
        M: RpcMethod,
        M::Success: Send + 'static;

    /// Follows the streaming RPC `M`. An item this build can't decode is yielded as an error,
    /// and the stream goes on.
    fn stream<M>(
        &self,
        payload: M::Payload,
        access: Access,
    ) -> Result<BoxStream<'static, Result<M::Success>>>
    where
        M: RpcMethod,
        M::Success: Send + 'static;
}

impl<B: ThreadBackend + ?Sized> ThreadBackendExt for B {
    fn call<M>(&self, payload: M::Payload, access: Access) -> BoxFuture<'static, Result<M::Success>>
    where
        M: RpcMethod,
        M::Success: Send + 'static,
    {
        if M::STREAM {
            let error = anyhow!(
                "{} is a streaming RPC; use ThreadBackendExt::stream",
                M::TAG
            );
            return futures::future::ready(Err(error)).boxed();
        }
        let payload = match serde_json::to_value(&payload) {
            Ok(payload) => payload,
            Err(error) => return futures::future::ready(Err(error.into())).boxed(),
        };
        let call = self.call_raw(M::TAG, payload, access);
        async move { decode::<M>(call.await?) }.boxed()
    }

    fn stream<M>(
        &self,
        payload: M::Payload,
        access: Access,
    ) -> Result<BoxStream<'static, Result<M::Success>>>
    where
        M: RpcMethod,
        M::Success: Send + 'static,
    {
        if !M::STREAM {
            bail!(
                "{} isn't a streaming RPC; use ThreadBackendExt::call",
                M::TAG
            );
        }
        let payload = serde_json::to_value(&payload)?;
        let stream = self.subscribe_raw(M::TAG, payload, access)?;
        Ok(stream.map(|item| item.and_then(decode::<M>)).boxed())
    }
}

fn decode<M: RpcMethod>(value: Value) -> Result<M::Success> {
    serde_json::from_value(value)
        .with_context(|| format!("couldn't decode the response to {}", M::TAG))
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
        futures::future::ready(Err(self.refusal(what))).boxed()
    }

    fn refusal(&self, what: &str) -> anyhow::Error {
        log::warn!("{}, so {what} wasn't sent", self.reason);
        anyhow::anyhow!("Not sent: {}.", self.reason)
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

    fn call_raw(
        &self,
        tag: &str,
        payload: Value,
        access: Access,
    ) -> BoxFuture<'static, Result<Value>> {
        if access.is_write() {
            return self.refuse(tag);
        }
        self.inner.call_raw(tag, payload, access)
    }

    fn subscribe_raw(
        &self,
        tag: &str,
        payload: Value,
        access: Access,
    ) -> Result<BoxStream<'static, Result<Value>>> {
        if access.is_write() {
            return Err(self.refusal(tag));
        }
        self.inner.subscribe_raw(tag, payload, access)
    }

    fn upload(&self, _upload: AttachmentUpload) -> BoxFuture<'static, Result<()>> {
        self.refuse("an attachment upload")
    }

    fn fetch_asset(&self, relative_url: &str) -> BoxFuture<'static, Result<Vec<u8>>> {
        self.inner.fetch_asset(relative_url)
    }

    fn write_blocked_reason(&self) -> Option<String> {
        Some(self.reason.clone())
    }

    fn blocked_reason(&self, access: Access) -> Option<String> {
        if access.is_write() {
            Some(self.reason.clone())
        } else {
            self.inner.blocked_reason(access)
        }
    }
}

//! The agent threads' backend: `thread_chat::ThreadBackend` on mustr's T3 connection. Every call
//! goes to whichever session is current, so open thread views keep working across reconnects.
//!
//! With `MUSTR_DRY_RUN_WRITES` set, writes are refused before they reach T3
//! (`thread_chat::ReadOnlyBackend`), so a live demo can't start, message, steer, approve, or
//! stop an agent.

use crate::{
    review_backend::{DRY_RUN_ENV, dry_run_requested},
    t3_connection::{CurrentSession, T3Session},
};
use anyhow::{Context as _, Result, anyhow};
use futures::{FutureExt as _, StreamExt as _, future::BoxFuture, stream::BoxStream};
use gpui::App;
use std::sync::Arc;
use t3_client::{
    RpcError,
    t3_api::{
        http, rpc,
        tagged::{
            OrchestrationV2Command, OrchestrationV2ShellStreamItem, OrchestrationV2ThreadStreamItem,
        },
        types::{
            HttpOrchestrationThreadSnapshotParams, OrchestrationV2DispatchCommandResult,
            OrchestrationV2GetTurnItemInput, OrchestrationV2GetTurnItemResult,
            OrchestrationV2ShellSnapshot, OrchestrationV2SubscribeShellInput,
            OrchestrationV2SubscribeThreadInput, OrchestrationV2ThreadDetailSnapshot,
            OrchestrationV2ThreadLaunchInput, OrchestrationV2ThreadLaunchResult,
            ServerGetConfigPayload, ServerProvider,
        },
    },
};
use thread_chat::{ReadOnlyBackend, ThreadBackend};

/// Installs the threads' backend. Call once, after `t3_connection::init`.
pub fn init(session: CurrentSession, cx: &mut App) {
    let backend: Arc<dyn ThreadBackend> = Arc::new(T3ThreadBackend { session });
    let backend: Arc<dyn ThreadBackend> = if dry_run_requested() {
        log::warn!("{DRY_RUN_ENV} is set: mustr won't start, message, steer, or stop agents");
        Arc::new(ReadOnlyBackend::new(
            backend,
            format!("{DRY_RUN_ENV} is set"),
        ))
    } else {
        backend
    };
    thread_chat::set_backend(backend, cx);
}

struct T3ThreadBackend {
    session: CurrentSession,
}

impl T3ThreadBackend {
    fn session(&self) -> Result<Arc<T3Session>> {
        self.session
            .get()
            .ok_or_else(|| anyhow!("mustr isn't connected to T3"))
    }

    fn call<M>(&self, payload: M::Payload) -> BoxFuture<'static, Result<M::Success>>
    where
        M: t3_client::RpcMethod + 'static,
        M::Payload: Send + 'static,
        M::Success: Send + 'static,
    {
        let session = self.session();
        async move {
            let session = session?;
            session
                .rpc()
                .call::<M>(payload)
                .await
                .map_err(|error| rpc_error(M::TAG, &error))
        }
        .boxed()
    }

    fn stream<M>(&self, payload: M::Payload) -> Result<BoxStream<'static, Result<M::Success>>>
    where
        M: t3_client::RpcMethod + 'static,
        M::Success: Send + 'static,
    {
        let stream = self
            .session()?
            .rpc()
            .stream::<M>(payload)
            .map_err(|error| rpc_error(M::TAG, &error))?;
        Ok(stream
            .map(|item| item.map_err(|error| rpc_error(M::TAG, &error)))
            .boxed())
    }
}

fn rpc_error(tag: &str, error: &RpcError) -> anyhow::Error {
    match error.error_detail() {
        Some(detail) => anyhow!("{detail}"),
        None => anyhow!("{tag}: {error}"),
    }
}

impl ThreadBackend for T3ThreadBackend {
    fn shell_snapshot(&self) -> BoxFuture<'static, Result<OrchestrationV2ShellSnapshot>> {
        let session = self.session();
        async move {
            session?
                .call_endpoint::<http::OrchestrationShellSnapshot>(&(), &(), &())
                .await
                .context("loading the thread list")
        }
        .boxed()
    }

    fn subscribe_shell(
        &self,
        input: OrchestrationV2SubscribeShellInput,
    ) -> Result<BoxStream<'static, Result<OrchestrationV2ShellStreamItem>>> {
        self.stream::<rpc::OrchestrationSubscribeShell>(input)
    }

    fn thread_snapshot(
        &self,
        thread_id: String,
    ) -> BoxFuture<'static, Result<OrchestrationV2ThreadDetailSnapshot>> {
        let session = self.session();
        async move {
            session?
                .call_endpoint::<http::OrchestrationThreadSnapshot>(
                    &HttpOrchestrationThreadSnapshotParams { thread_id },
                    &(),
                    &(),
                )
                .await
                .context("loading the thread")
        }
        .boxed()
    }

    fn subscribe_thread(
        &self,
        input: OrchestrationV2SubscribeThreadInput,
    ) -> Result<BoxStream<'static, Result<OrchestrationV2ThreadStreamItem>>> {
        self.stream::<rpc::OrchestrationSubscribeThread>(input)
    }

    fn turn_item(
        &self,
        input: OrchestrationV2GetTurnItemInput,
    ) -> BoxFuture<'static, Result<OrchestrationV2GetTurnItemResult>> {
        self.call::<rpc::OrchestrationGetTurnItem>(input)
    }

    fn providers(&self) -> BoxFuture<'static, Result<Vec<ServerProvider>>> {
        let config = self.call::<rpc::ServerGetConfig>(ServerGetConfigPayload(Default::default()));
        async move { Ok(config.await?.providers) }.boxed()
    }

    fn dispatch(
        &self,
        command: OrchestrationV2Command,
    ) -> BoxFuture<'static, Result<OrchestrationV2DispatchCommandResult>> {
        if let Some(reason) = self.write_blocked_reason() {
            return futures::future::ready(Err(anyhow!(reason))).boxed();
        }
        self.call::<rpc::OrchestrationDispatchCommand>(command)
    }

    fn launch_thread(
        &self,
        input: OrchestrationV2ThreadLaunchInput,
    ) -> BoxFuture<'static, Result<OrchestrationV2ThreadLaunchResult>> {
        if let Some(reason) = self.write_blocked_reason() {
            return futures::future::ready(Err(anyhow!(reason))).boxed();
        }
        self.call::<rpc::OrchestrationLaunchThread>(input)
    }

    fn write_blocked_reason(&self) -> Option<String> {
        self.session.get()?.operate_blocked_reason()
    }
}

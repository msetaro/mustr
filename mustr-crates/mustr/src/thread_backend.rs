//! The agent threads' backend: `thread_chat::ThreadBackend` on mustr's T3 connection. Every call
//! goes to whichever session is current, so open thread views keep working across reconnects.
//!
//! With `MUSTR_DRY_RUN_WRITES` set, writes are refused before they reach T3
//! (`thread_chat::ReadOnlyBackend`), so a live demo can't start, message, steer, approve, or
//! stop an agent, or make any other call that isn't a read.

use crate::{
    review_backend::{DRY_RUN_ENV, dry_run_requested},
    t3_connection::{CurrentSession, T3Connection, T3Session},
};
use anyhow::{Context as _, Result, anyhow, bail};
use futures::{FutureExt as _, StreamExt as _, future::BoxFuture, stream::BoxStream};
use gpui::App;
use http_client::{HttpClient, Url};
use parking_lot::Mutex;
use serde_json::Value;
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
use thread_chat::{Access, AttachmentUpload, ReadOnlyBackend, ThreadBackend};

/// Installs the threads' backend. Call once, after `t3_connection::init`.
pub fn init(session: CurrentSession, cx: &mut App) {
    let connection = T3Connection::global(cx);
    let server_url = Arc::new(Mutex::new(connection.read(cx).server_url().cloned()));
    // The embedded engine's URL is known only once it has started, and changes when it restarts.
    cx.observe(&connection, {
        let server_url = server_url.clone();
        move |connection, cx| *server_url.lock() = connection.read(cx).server_url().cloned()
    })
    .detach();
    let backend: Arc<dyn ThreadBackend> = Arc::new(T3ThreadBackend {
        session,
        http: cx.http_client(),
        server_url,
    });
    let backend: Arc<dyn ThreadBackend> = if dry_run_requested() {
        log::warn!("{DRY_RUN_ENV} is set: mustr won't start, message, steer, or stop agents");
        Arc::new(ReadOnlyBackend::new(
            backend,
            format!("{DRY_RUN_ENV} is set"),
        ))
    } else {
        backend
    };
    // Debug builds can play scripted thread changes over the shell, for checking notifications
    // without an agent run (`MUSTR_DEBUG_SHELL_REPLAY`).
    #[cfg(debug_assertions)]
    let backend = crate::notifications::replay::wrap(backend);
    thread_chat::set_backend(backend, cx);
}

struct T3ThreadBackend {
    session: CurrentSession,
    /// Attachments upload over HTTP: T3's upload URLs carry their own signature, so they need
    /// no token, only the server.
    http: Arc<dyn HttpClient>,
    /// T3's server, as the connection knows it now.
    server_url: Arc<Mutex<Option<Url>>>,
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

fn refused<T: Send + 'static>(reason: String) -> BoxFuture<'static, Result<T>> {
    futures::future::ready(Err(anyhow!(reason))).boxed()
}

/// The largest asset mustr loads, such as a screenshot in a tool's output.
const MAX_ASSET_BYTES: usize = 32 * 1024 * 1024;

impl ThreadBackend for T3ThreadBackend {
    fn shell_snapshot(&self) -> BoxFuture<'static, Result<OrchestrationV2ShellSnapshot>> {
        let session = self.session();
        async move {
            session?
                .call_endpoint::<http::OrchestrationShellSnapshot>(&(), &Default::default(), &())
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

    fn call_raw(
        &self,
        tag: &str,
        payload: Value,
        access: Access,
    ) -> BoxFuture<'static, Result<Value>> {
        if let Some(reason) = self.blocked_reason(access) {
            return refused(reason);
        }
        let session = self.session();
        let tag = tag.to_string();
        async move {
            session?
                .rpc()
                .request(&tag, payload)
                .await
                .map_err(|error| rpc_error(&tag, &error))
        }
        .boxed()
    }

    fn subscribe_raw(
        &self,
        tag: &str,
        payload: Value,
        access: Access,
    ) -> Result<BoxStream<'static, Result<Value>>> {
        if let Some(reason) = self.blocked_reason(access) {
            bail!(reason);
        }
        let chunks = self
            .session()?
            .rpc()
            .subscribe(tag, payload)
            .map_err(|error| rpc_error(tag, &error))?;
        let tag = tag.to_string();
        Ok(chunks
            .map(move |chunk| chunk.map_err(|error| rpc_error(&tag, &error)))
            .boxed())
    }

    fn upload(&self, upload: AttachmentUpload) -> BoxFuture<'static, Result<()>> {
        if let Some(reason) = self.blocked_reason(Access::Operate) {
            return refused(reason);
        }
        let Some(server_url) = self.server_url.lock().clone() else {
            return refused("mustr has no T3 server to upload to".to_string());
        };
        let http = self.http.clone();
        async move {
            t3_client::upload_attachment(
                http.as_ref(),
                &server_url,
                &upload.relative_url,
                &upload.mime_type,
                upload.bytes,
                upload.on_progress,
            )
            .await
        }
        .boxed()
    }

    fn fetch_asset(&self, relative_url: &str) -> BoxFuture<'static, Result<Vec<u8>>> {
        let Some(server_url) = self.server_url.lock().clone() else {
            return refused("mustr has no T3 server to load assets from".to_string());
        };
        let http = self.http.clone();
        let relative_url = relative_url.to_string();
        async move {
            let asset =
                t3_client::fetch_asset(http.as_ref(), &server_url, &relative_url, MAX_ASSET_BYTES)
                    .await?;
            Ok(asset.bytes)
        }
        .boxed()
    }

    fn write_blocked_reason(&self) -> Option<String> {
        self.session.get()?.operate_blocked_reason()
    }

    fn blocked_reason(&self, access: Access) -> Option<String> {
        let session = self.session.get()?;
        match access {
            Access::Read => None,
            Access::Operate => session.operate_blocked_reason(),
            Access::SourceControlWrite => session.write_blocked_reason(),
            Access::FilesystemRead => session.filesystem_blocked_reason(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use futures::AsyncReadExt as _;
    use gpui::TestAppContext;
    use http_client::{AsyncBody, Request, Response, http::HeaderValue};
    use pretty_assertions::assert_eq;

    /// One request T3's HTTP server got.
    #[derive(Debug, PartialEq)]
    struct SentRequest {
        method: String,
        url: String,
        content_type: Option<String>,
        content_length: Option<String>,
        body: Vec<u8>,
    }

    /// Plays T3's HTTP server: records each request, and answers with `status`.
    struct RecordingHttp {
        requests: Arc<Mutex<Vec<SentRequest>>>,
        status: u16,
    }

    impl HttpClient for RecordingHttp {
        fn user_agent(&self) -> Option<&HeaderValue> {
            None
        }

        fn proxy(&self) -> Option<&Url> {
            None
        }

        fn send(
            &self,
            request: Request<AsyncBody>,
        ) -> BoxFuture<'static, Result<Response<AsyncBody>>> {
            let requests = self.requests.clone();
            let status = self.status;
            async move {
                let (parts, mut body) = request.into_parts();
                let mut bytes = Vec::new();
                body.read_to_end(&mut bytes).await?;
                let header = |name: &str| {
                    parts
                        .headers
                        .get(name)
                        .map(|value| value.to_str().unwrap().to_string())
                };
                requests.lock().push(SentRequest {
                    method: parts.method.to_string(),
                    url: parts.uri.to_string(),
                    content_type: header("content-type"),
                    content_length: header("content-length"),
                    body: bytes,
                });
                Ok(Response::builder().status(status).body(AsyncBody::from(
                    "Content-Length must match the upload size.",
                ))?)
            }
            .boxed()
        }
    }

    /// A backend that isn't connected to T3's socket, with `status` from its HTTP server.
    fn backend(status: u16) -> (T3ThreadBackend, Arc<Mutex<Vec<SentRequest>>>) {
        let requests = Arc::new(Mutex::new(Vec::new()));
        let backend = T3ThreadBackend {
            session: CurrentSession::default(),
            http: Arc::new(RecordingHttp {
                requests: requests.clone(),
                status,
            }),
            server_url: Arc::new(Mutex::new(Some(
                Url::parse("http://127.0.0.1:3773").unwrap(),
            ))),
        };
        (backend, requests)
    }

    fn upload(relative_url: &str) -> AttachmentUpload {
        AttachmentUpload {
            relative_url: relative_url.to_string(),
            mime_type: "image/png".to_string(),
            bytes: b"png bytes".to_vec(),
            on_progress: None,
        }
    }

    #[gpui::test]
    async fn uploads_post_the_bytes_to_the_minted_url(_cx: &mut TestAppContext) {
        let (backend, requests) = backend(204);
        let progress = Arc::new(Mutex::new(Vec::new()));
        backend
            .upload(AttachmentUpload {
                on_progress: Some(Arc::new({
                    let progress = progress.clone();
                    move |sent| progress.lock().push(sent)
                })),
                ..upload("/api/attachments/upload/token.signature")
            })
            .await
            .unwrap();
        assert_eq!(
            *requests.lock(),
            [SentRequest {
                method: "POST".to_string(),
                url: "http://127.0.0.1:3773/api/attachments/upload/token.signature".to_string(),
                content_type: Some("image/png".to_string()),
                content_length: Some("9".to_string()),
                body: b"png bytes".to_vec(),
            }]
        );
        assert_eq!(progress.lock().last(), Some(&9));
    }

    #[gpui::test]
    async fn uploads_stay_on_t3s_server(_cx: &mut TestAppContext) {
        let (backend, requests) = backend(204);
        for relative_url in [
            "//attacker.example/api/attachments/upload/token",
            "https://attacker.example/api/attachments/upload/token",
        ] {
            let error = backend.upload(upload(relative_url)).await.unwrap_err();
            assert!(
                error.to_string().contains("isn't on its server"),
                "{relative_url}: {error:#}"
            );
        }
        assert!(requests.lock().is_empty());
    }

    #[gpui::test]
    async fn refused_uploads_say_why(_cx: &mut TestAppContext) {
        let (backend, _) = backend(400);
        let error = backend
            .upload(upload("/api/attachments/upload/token.signature"))
            .await
            .unwrap_err();
        assert_eq!(
            error.to_string(),
            "T3 refused the attachment (400 Bad Request): Content-Length must match the upload size."
        );
    }

    #[gpui::test]
    async fn assets_load_from_the_signed_url(_cx: &mut TestAppContext) {
        let (backend, requests) = backend(200);
        let bytes = backend
            .fetch_asset("/api/assets/attachment/a1?expires=1&signature=s")
            .await
            .unwrap();
        assert_eq!(bytes, b"Content-Length must match the upload size.");
        assert_eq!(
            *requests.lock(),
            [SentRequest {
                method: "GET".to_string(),
                url: "http://127.0.0.1:3773/api/assets/attachment/a1?expires=1&signature=s"
                    .to_string(),
                content_type: None,
                content_length: None,
                body: Vec::new(),
            }]
        );
        let error = backend
            .fetch_asset("https://attacker.example/api/assets/x")
            .await
            .unwrap_err();
        assert!(
            error.to_string().contains("isn't on its server"),
            "{error:#}"
        );
    }

    #[gpui::test]
    async fn raw_calls_need_a_connection(_cx: &mut TestAppContext) {
        let (backend, _) = backend(204);
        let error = backend
            .call_raw("server.getSettings", Value::Null, Access::Read)
            .await
            .unwrap_err();
        assert_eq!(error.to_string(), "mustr isn't connected to T3");
        assert!(
            backend
                .subscribe_raw("subscribeVcsStatus", Value::Null, Access::Read)
                .is_err()
        );
        // Without a session there's no pairing to lack a scope.
        assert_eq!(backend.blocked_reason(Access::SourceControlWrite), None);
    }
}

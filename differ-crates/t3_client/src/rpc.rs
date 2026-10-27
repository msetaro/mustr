//! Effect RPC over WebSocket, as spoken by T3's server (`RpcSerialization.layerJson`).
//!
//! Each frame holds one JSON message or an array of them. Requests are answered by an
//! `Exit`; streaming requests get `Chunk`s first, and the server only sends the next
//! chunk after the client acknowledges the previous one with an `Ack`.

use std::collections::HashMap;
use std::sync::Arc;

use anyhow::{Context as _, Result, anyhow, bail};
use async_tungstenite::tungstenite::Message;
use futures::channel::{mpsc, oneshot};
use futures::future::BoxFuture;
use futures::{FutureExt as _, StreamExt as _};
use http_client::Url;
use parking_lot::Mutex;
use serde_json::{Value, json};

#[derive(Clone, Debug, thiserror::Error)]
pub enum RpcError {
    /// The server returned one of the RPC's declared, typed errors.
    #[error("{tag} failed: {error}")]
    Failed { tag: String, error: Value },
    #[error("server defect: {0}")]
    Defect(String),
    #[error("T3 connection closed: {0}")]
    Closed(String),
    #[error("T3 protocol error: {0}")]
    Protocol(String),
}

impl RpcError {
    /// The `_tag` of a typed error, e.g. `OrchestrationGetFullThreadDiffError`.
    pub fn error_tag(&self) -> Option<&str> {
        match self {
            RpcError::Failed { error, .. } => error.get("_tag").and_then(Value::as_str),
            _ => None,
        }
    }
}

enum PendingRequest {
    Single {
        tag: String,
        respond: oneshot::Sender<Result<Value, RpcError>>,
    },
    Stream {
        tag: String,
        chunks: mpsc::UnboundedSender<Result<Value, RpcError>>,
    },
}

#[derive(Default)]
struct ConnectionState {
    next_request_id: u64,
    pending: HashMap<String, PendingRequest>,
    closed: Option<String>,
}

#[derive(Clone)]
pub struct RpcConnection {
    outgoing: mpsc::UnboundedSender<String>,
    state: Arc<Mutex<ConnectionState>>,
}

impl RpcConnection {
    /// Opens the socket. The returned future drives it and must be spawned by the caller;
    /// it resolves when the connection ends, after failing every pending request.
    pub async fn connect(url: &Url) -> Result<(Self, BoxFuture<'static, Result<()>>)> {
        if url.scheme() != "ws" {
            bail!("only ws:// T3 endpoints are supported so far, got {url}");
        }
        let host = url
            .host_str()
            .context("T3 WebSocket URL has no host")?
            .to_string();
        let port = url
            .port_or_known_default()
            .context("T3 WebSocket URL has no port")?;
        let tcp = smol::net::TcpStream::connect((host.as_str(), port))
            .await
            .with_context(|| format!("connecting to T3 at {host}:{port}"))?;
        let (socket, _response) = async_tungstenite::client_async(url.as_str(), tcp)
            .await
            .context("T3 WebSocket handshake")?;
        let (mut sink, mut incoming) = socket.split();

        let (outgoing, mut outgoing_rx) = mpsc::unbounded::<String>();
        let connection = Self {
            outgoing: outgoing.clone(),
            state: Arc::new(Mutex::new(ConnectionState::default())),
        };

        let state = connection.state.clone();
        let driver = async move {
            let write = async {
                while let Some(text) = outgoing_rx.next().await {
                    sink.send(Message::text(text)).await?;
                }
                anyhow::Ok(())
            };
            let read = async {
                while let Some(frame) = incoming.next().await {
                    let text = match frame? {
                        Message::Text(text) => text.to_string(),
                        Message::Binary(bytes) => String::from_utf8(bytes.to_vec())?,
                        Message::Close(close) => {
                            return Err(anyhow!("server closed the socket: {close:?}"));
                        }
                        Message::Ping(_) | Message::Pong(_) | Message::Frame(_) => continue,
                    };
                    handle_frame(&state, &outgoing, &text)?;
                }
                Err(anyhow!("socket ended"))
            };
            let result = futures::select! {
                result = write.fuse() => result,
                result = read.fuse() => result,
            };
            let reason = match &result {
                Ok(()) => "client closed the connection".to_string(),
                Err(error) => format!("{error:#}"),
            };
            fail_all_pending(&state, &reason);
            result
        }
        .boxed();

        Ok((connection, driver))
    }

    /// Calls a non-streaming RPC and waits for its result.
    pub async fn request(&self, tag: &str, payload: Value) -> Result<Value, RpcError> {
        let (respond, response) = oneshot::channel();
        self.send_request(
            tag,
            payload,
            PendingRequest::Single {
                tag: tag.to_string(),
                respond,
            },
        )?;
        response
            .await
            .map_err(|_| RpcError::Closed(format!("{tag} was dropped before it finished")))?
    }

    /// Calls a streaming RPC. The receiver yields each chunk value and ends when the
    /// stream completes; an error is delivered as the final item.
    pub fn subscribe(
        &self,
        tag: &str,
        payload: Value,
    ) -> Result<mpsc::UnboundedReceiver<Result<Value, RpcError>>, RpcError> {
        let (chunks, receiver) = mpsc::unbounded();
        self.send_request(
            tag,
            payload,
            PendingRequest::Stream {
                tag: tag.to_string(),
                chunks,
            },
        )?;
        Ok(receiver)
    }

    /// Ends the session politely; the driver future then resolves.
    pub fn close(&self) {
        if self
            .outgoing
            .unbounded_send(json!({ "_tag": "Eof" }).to_string())
            .is_err()
        {
            log::debug!("T3 connection was already closed");
        }
        self.outgoing.close_channel();
    }

    fn send_request(
        &self,
        tag: &str,
        payload: Value,
        pending: PendingRequest,
    ) -> Result<(), RpcError> {
        let mut state = self.state.lock();
        if let Some(reason) = &state.closed {
            return Err(RpcError::Closed(reason.clone()));
        }
        state.next_request_id += 1;
        let id = state.next_request_id.to_string();
        let request = json!({
            "_tag": "Request",
            "id": id,
            "tag": tag,
            "payload": payload,
            "headers": [],
        });
        self.outgoing
            .unbounded_send(request.to_string())
            .map_err(|_| RpcError::Closed("the connection's writer has stopped".to_string()))?;
        state.pending.insert(id, pending);
        Ok(())
    }
}

fn handle_frame(
    state: &Mutex<ConnectionState>,
    outgoing: &mpsc::UnboundedSender<String>,
    text: &str,
) -> Result<()> {
    let decoded: Value = serde_json::from_str(text).context("T3 sent a frame that isn't JSON")?;
    let messages = match decoded {
        Value::Array(messages) => messages,
        message => vec![message],
    };
    for message in messages {
        handle_message(state, outgoing, message)?;
    }
    Ok(())
}

fn handle_message(
    state: &Mutex<ConnectionState>,
    outgoing: &mpsc::UnboundedSender<String>,
    message: Value,
) -> Result<()> {
    let message_tag = message
        .get("_tag")
        .and_then(Value::as_str)
        .unwrap_or_default();
    match message_tag {
        "Chunk" => {
            let request_id = request_id(&message)?;
            let values = message
                .get("values")
                .and_then(Value::as_array)
                .cloned()
                .unwrap_or_default();
            let state = state.lock();
            match state.pending.get(&request_id) {
                Some(PendingRequest::Stream { chunks, .. }) => {
                    for value in values {
                        if chunks.unbounded_send(Ok(value)).is_err() {
                            log::debug!("dropping a chunk for a stream nobody is reading");
                        }
                    }
                }
                Some(PendingRequest::Single { tag, .. }) => {
                    log::warn!("T3 sent stream chunks for non-streaming request {tag}");
                }
                None => log::debug!("chunk for unknown request {request_id}"),
            }
            outgoing
                .unbounded_send(json!({ "_tag": "Ack", "requestId": request_id }).to_string())
                .map_err(|_| anyhow!("couldn't acknowledge a stream chunk"))?;
        }
        "Exit" => {
            let request_id = request_id(&message)?;
            let Some(pending) = state.lock().pending.remove(&request_id) else {
                log::debug!("exit for unknown request {request_id}");
                return Ok(());
            };
            let exit = message.get("exit").cloned().unwrap_or(Value::Null);
            match pending {
                PendingRequest::Single { tag, respond } => {
                    let result = match exit.get("_tag").and_then(Value::as_str) {
                        Some("Success") => Ok(exit.get("value").cloned().unwrap_or(Value::Null)),
                        _ => Err(failure_to_error(&tag, &exit)),
                    };
                    if respond.send(result).is_err() {
                        log::debug!("{tag} finished after its caller stopped waiting");
                    }
                }
                PendingRequest::Stream { tag, chunks } => {
                    if exit.get("_tag").and_then(Value::as_str) != Some("Success")
                        && chunks
                            .unbounded_send(Err(failure_to_error(&tag, &exit)))
                            .is_err()
                    {
                        log::debug!("{tag} failed after its reader went away");
                    }
                }
            }
        }
        "Defect" => {
            let defect = message.get("defect").cloned().unwrap_or(Value::Null);
            bail!("T3 server defect: {defect}");
        }
        "ClientProtocolError" => {
            let error = message.get("error").cloned().unwrap_or(Value::Null);
            bail!("T3 rejected a message from this client: {error}");
        }
        "Ping" => {
            outgoing
                .unbounded_send(json!({ "_tag": "Pong" }).to_string())
                .map_err(|_| anyhow!("couldn't answer a ping"))?;
        }
        "Pong" => {}
        other => log::debug!("ignoring T3 message {other:?}"),
    }
    Ok(())
}

fn request_id(message: &Value) -> Result<String> {
    match message.get("requestId") {
        Some(Value::String(id)) => Ok(id.clone()),
        Some(Value::Number(id)) => Ok(id.to_string()),
        _ => bail!("T3 message without a requestId: {message}"),
    }
}

fn failure_to_error(tag: &str, exit: &Value) -> RpcError {
    let causes = exit
        .get("cause")
        .and_then(Value::as_array)
        .cloned()
        .unwrap_or_default();
    for cause in &causes {
        match cause.get("_tag").and_then(Value::as_str) {
            Some("Fail") => {
                return RpcError::Failed {
                    tag: tag.to_string(),
                    error: cause.get("error").cloned().unwrap_or(Value::Null),
                };
            }
            Some("Die") => {
                return RpcError::Defect(
                    cause
                        .get("defect")
                        .cloned()
                        .unwrap_or(Value::Null)
                        .to_string(),
                );
            }
            Some("Interrupt") => return RpcError::Closed(format!("{tag} was interrupted")),
            _ => {}
        }
    }
    RpcError::Protocol(format!("{tag} ended with an unrecognized exit: {exit}"))
}

fn fail_all_pending(state: &Mutex<ConnectionState>, reason: &str) {
    let pending = {
        let mut state = state.lock();
        state.closed = Some(reason.to_string());
        std::mem::take(&mut state.pending)
    };
    for (_, request) in pending {
        match request {
            PendingRequest::Single { respond, .. } => {
                if respond
                    .send(Err(RpcError::Closed(reason.to_string())))
                    .is_err()
                {
                    log::debug!("a caller stopped waiting before the connection closed");
                }
            }
            PendingRequest::Stream { chunks, .. } => {
                if chunks
                    .unbounded_send(Err(RpcError::Closed(reason.to_string())))
                    .is_err()
                {
                    log::debug!("a stream reader went away before the connection closed");
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn connection_with_pending(
        pending: PendingRequest,
    ) -> (
        Mutex<ConnectionState>,
        mpsc::UnboundedSender<String>,
        mpsc::UnboundedReceiver<String>,
    ) {
        let state = Mutex::new(ConnectionState::default());
        state.lock().pending.insert("1".to_string(), pending);
        let (outgoing, outgoing_rx) = mpsc::unbounded();
        (state, outgoing, outgoing_rx)
    }

    #[test]
    fn success_exit_resolves_the_request() -> Result<()> {
        let (respond, mut response) = oneshot::channel();
        let (state, outgoing, _outgoing_rx) = connection_with_pending(PendingRequest::Single {
            tag: "server.getConfig".to_string(),
            respond,
        });
        let frame =
            r#"{"_tag":"Exit","requestId":"1","exit":{"_tag":"Success","value":{"ok":true}}}"#;
        handle_frame(&state, &outgoing, frame)?;
        let result = response.try_recv()?.context("no response")?;
        assert_eq!(
            result.map_err(|error| anyhow!(error))?,
            json!({ "ok": true })
        );
        Ok(())
    }

    #[test]
    fn typed_failures_keep_their_tag() -> Result<()> {
        let (respond, mut response) = oneshot::channel();
        let (state, outgoing, _outgoing_rx) = connection_with_pending(PendingRequest::Single {
            tag: "orchestration.getFullThreadDiff".to_string(),
            respond,
        });
        let frame = r#"{"_tag":"Exit","requestId":"1","exit":{"_tag":"Failure","cause":[{"_tag":"Fail","error":{"_tag":"OrchestrationGetFullThreadDiffError","message":"Failed to load full thread diff"}}]}}"#;
        handle_frame(&state, &outgoing, frame)?;
        let error = response
            .try_recv()?
            .context("no response")?
            .err()
            .context("expected an error")?;
        assert_eq!(
            error.error_tag(),
            Some("OrchestrationGetFullThreadDiffError")
        );
        Ok(())
    }

    #[test]
    fn stream_chunks_are_delivered_and_acknowledged() -> Result<()> {
        let (chunks, mut receiver) = mpsc::unbounded();
        let (state, outgoing, mut outgoing_rx) = connection_with_pending(PendingRequest::Stream {
            tag: "orchestration.subscribeShell".to_string(),
            chunks,
        });
        let frames = r#"[{"_tag":"Chunk","requestId":"1","values":[{"n":1},{"n":2}]},{"_tag":"Exit","requestId":"1","exit":{"_tag":"Success","value":null}}]"#;
        handle_frame(&state, &outgoing, frames)?;

        let mut received = Vec::new();
        while let Ok(item) = receiver.try_recv() {
            received.push(item.map_err(|error| anyhow!(error))?);
        }
        assert_eq!(received, vec![json!({ "n": 1 }), json!({ "n": 2 })]);

        let ack = outgoing_rx
            .try_recv()
            .map_err(|error| anyhow!("no ack sent: {error:?}"))?;
        let ack: Value = serde_json::from_str(&ack)?;
        assert_eq!(ack, json!({ "_tag": "Ack", "requestId": "1" }));
        assert!(state.lock().pending.is_empty());
        Ok(())
    }

    #[test]
    fn closing_fails_pending_requests() -> Result<()> {
        let (respond, mut response) = oneshot::channel();
        let (state, _outgoing, _outgoing_rx) = connection_with_pending(PendingRequest::Single {
            tag: "pullRequests.list".to_string(),
            respond,
        });
        fail_all_pending(&state, "socket ended");
        let result = response.try_recv()?.context("no response")?;
        assert!(matches!(result, Err(RpcError::Closed(_))));
        assert!(state.lock().closed.is_some());
        Ok(())
    }
}

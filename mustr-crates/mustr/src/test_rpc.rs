//! A recording stand-in for T3's RPC, so tests assert the exact typed payloads mustr sends and
//! no test ever writes to a real pull request or agent thread.

use crate::review_backend::PullRequestRpc;
use futures::{FutureExt as _, future::BoxFuture};
use parking_lot::Mutex;
use serde_json::Value;
use t3_client::{RpcError, RpcMethod};

/// Plays T3: records each call's tag and payload, and answers with the next scripted response
/// (success with `null` when none is left).
#[derive(Default)]
pub struct RecordingRpc {
    pub calls: Mutex<Vec<(String, Value)>>,
    responses: Mutex<Vec<Result<Value, RpcError>>>,
}

impl RecordingRpc {
    pub fn respond(&self, response: Result<Value, RpcError>) {
        self.responses.lock().push(response);
    }

    fn next_response(&self) -> Result<Value, RpcError> {
        let mut responses = self.responses.lock();
        if responses.is_empty() {
            Ok(Value::Null)
        } else {
            responses.remove(0)
        }
    }

    /// The one recorded call, decoded as `M`'s payload.
    pub fn only_call<M: RpcMethod>(&self) -> M::Payload {
        let calls = self.calls.lock();
        assert_eq!(calls.len(), 1, "expected one call, got {calls:#?}");
        let (tag, payload) = &calls[0];
        assert_eq!(tag, M::TAG);
        serde_json::from_value(payload.clone()).expect("the payload should be M's")
    }
}

impl PullRequestRpc for RecordingRpc {
    fn call<M>(&self, payload: M::Payload) -> BoxFuture<'static, Result<M::Success, RpcError>>
    where
        M: RpcMethod + 'static,
        M::Payload: Send + 'static,
        M::Success: Send + 'static,
    {
        let payload = serde_json::to_value(&payload).expect("payloads encode");
        self.calls.lock().push((M::TAG.to_string(), payload));
        let response = self.next_response().and_then(|value| {
            serde_json::from_value(value).map_err(|error| RpcError::Decode {
                tag: M::TAG.to_string(),
                message: error.to_string(),
            })
        });
        async move { response }.boxed()
    }

    fn request(
        &self,
        tag: &'static str,
        payload: Value,
    ) -> BoxFuture<'static, Result<Value, RpcError>> {
        self.calls.lock().push((tag.to_string(), payload));
        let response = self.next_response();
        async move { response }.boxed()
    }
}

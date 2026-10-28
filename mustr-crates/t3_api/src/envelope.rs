//! Effect RPC framing over `RpcSerialization.layerJson` (one JSON message per WS frame).
//!
//! Hand-written from `effect/rpc/RpcMessage` (Effect 4.0.5). These shapes belong to the
//! Effect RPC protocol, not to T3, so they only change when T3 bumps Effect.

use serde::{Deserialize, Serialize, de::DeserializeOwned};
use serde_json::Value;

use crate::RpcMethod;

/// Request ids are strings or numbers on the wire (Effect sends bigint ids as strings).
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(untagged)]
pub enum RequestId {
    String(String),
    Number(u64),
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "_tag")]
pub enum FromClient {
    Request {
        id: RequestId,
        tag: String,
        payload: Value,
        #[serde(default)]
        headers: Vec<(String, String)>,
        #[serde(rename = "traceId", default, skip_serializing_if = "Option::is_none")]
        trace_id: Option<String>,
        #[serde(rename = "spanId", default, skip_serializing_if = "Option::is_none")]
        span_id: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        sampled: Option<bool>,
    },
    Ack {
        #[serde(rename = "requestId")]
        request_id: RequestId,
    },
    Interrupt {
        #[serde(rename = "requestId")]
        request_id: RequestId,
    },
    Ping,
    Eof,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "_tag")]
pub enum FromServer {
    Chunk {
        #[serde(rename = "requestId")]
        request_id: RequestId,
        values: Vec<Value>,
    },
    Exit {
        #[serde(rename = "requestId")]
        request_id: RequestId,
        exit: Exit<Value, Value>,
    },
    Defect {
        defect: Value,
    },
    Pong,
    ClientProtocolError {
        error: Value,
    },
}

/// `Schema.Exit` encoded by `toCodecJson`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "_tag")]
pub enum Exit<A, E> {
    Success { value: A },
    Failure { cause: Vec<CauseReason<E>> },
}

/// One reason inside an encoded `Cause` (`Fail` carries the typed RPC error).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "_tag")]
pub enum CauseReason<E> {
    Fail {
        error: E,
    },
    Die {
        defect: Value,
    },
    Interrupt {
        #[serde(rename = "fiberId")]
        fiber_id: Option<f64>,
    },
}

/// Build a `Request` frame for `M`.
pub fn request<M: RpcMethod>(
    id: RequestId,
    payload: &M::Payload,
) -> serde_json::Result<FromClient> {
    Ok(FromClient::Request {
        id,
        tag: M::TAG.to_string(),
        payload: serde_json::to_value(payload)?,
        headers: Vec::new(),
        trace_id: None,
        span_id: None,
        sampled: None,
    })
}

/// Decode the `exit` of an `Exit` frame for `M`. For stream RPCs the success value is `()`.
pub fn decode_exit<M: RpcMethod>(
    exit: Exit<Value, Value>,
) -> serde_json::Result<Exit<Value, M::Error>> {
    Ok(match exit {
        Exit::Success { value } => Exit::Success { value },
        Exit::Failure { cause } => Exit::Failure {
            cause: cause
                .into_iter()
                .map(|r| {
                    Ok(match r {
                        CauseReason::Fail { error } => CauseReason::Fail {
                            error: serde_json::from_value(error)?,
                        },
                        CauseReason::Die { defect } => CauseReason::Die { defect },
                        CauseReason::Interrupt { fiber_id } => CauseReason::Interrupt { fiber_id },
                    })
                })
                .collect::<serde_json::Result<_>>()?,
        },
    })
}

/// Decode a non-stream success value for `M`.
pub fn decode_success<M: RpcMethod>(value: Value) -> serde_json::Result<M::Success> {
    serde_json::from_value(value)
}

/// Decode the values of a `Chunk` frame for stream RPC `M`.
pub fn decode_chunk<M: RpcMethod>(values: Vec<Value>) -> serde_json::Result<Vec<M::Success>>
where
    M::Success: DeserializeOwned,
{
    values.into_iter().map(serde_json::from_value).collect()
}

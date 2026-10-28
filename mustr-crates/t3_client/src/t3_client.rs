//! mustr's client for a T3 Code server.
//!
//! mustr never patches T3; it talks to an unmodified server the same way T3's own
//! desktop, web, and mobile apps do. Request and response types are generated from T3's
//! `packages/contracts` into the `t3_api` crate (see `mustr-docs/SYNC.md`), so this crate
//! stays a thin, mechanical layer that is cheap to update when T3 releases:
//!
//! - RPC: [`RpcConnection::call`] / [`RpcConnection::stream`] with a method from
//!   [`t3_api::rpc`], e.g. `connection.call::<rpc::PullRequestsList>(input)`.
//! - HTTP: [`call_endpoint`] / [`call_endpoint_with`] with an endpoint from
//!   [`t3_api::http`].

mod auth;
mod endpoints;
mod pull_requests;
mod rpc;

pub use auth::{
    AccessToken, ClientMetadata, EnvironmentDescriptor, ORCHESTRATION_PROTOCOL_HEADER,
    ORCHESTRATION_PROTOCOL_VERSION, Scope, check_protocol_compatibility,
    exchange_pairing_credential, fetch_descriptor, websocket_ticket, websocket_url,
};
pub use endpoints::{call_endpoint, call_endpoint_with};
pub use pull_requests::{
    ChangeType, ChangedFile, PatchHunk, PatchLine, PullRequestFile, PullRequestRef, changed_files,
    load_pull_request_files, pull_request_patch,
};
pub use rpc::{RpcConnection, RpcError};
pub use t3_api::{self, HttpEndpoint, RpcMethod};

//! Differ's client for a T3 Code server.
//!
//! Differ never patches T3; it talks to an unmodified server the same way T3's own
//! desktop, web, and mobile apps do. Request and response types will be generated
//! from T3's `packages/contracts` (see `differ-docs/SYNC.md`), so this crate stays a
//! thin, mechanical layer that is cheap to update when T3 releases.

mod auth;
mod pull_requests;
mod rpc;

pub use auth::{
    AccessToken, ClientMetadata, EnvironmentDescriptor, ORCHESTRATION_PROTOCOL_HEADER,
    ORCHESTRATION_PROTOCOL_VERSION, Scope, check_protocol_compatibility,
    exchange_pairing_credential, fetch_descriptor, websocket_ticket, websocket_url,
};
pub use pull_requests::{
    ChangeType, ChangedFile, PullRequestFile, PullRequestRef, changed_files,
    load_pull_request_files, pull_request_patch,
};
pub use rpc::{RpcConnection, RpcError};

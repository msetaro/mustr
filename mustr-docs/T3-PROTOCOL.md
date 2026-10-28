# M0 spike: Rust client ↔ T3 Code server

Ran on 2026-10-27 against the local T3 server ("ironforge", T3 runtime 0.1.1, `127.0.0.1:3773`), using a read-only pairing token. The spike's sessions were revoked afterwards.

**Result: the architecture works.** A plain Rust client (reqwest + tokio-tungstenite, about 300 lines) authenticates, connects, and reads PRs, PR diffs with old/new file contents, and the full thread list.

## Wire protocol (verified)

| Step | Request | Notes |
|---|---|---|
| 1. Descriptor | `GET /.well-known/t3/environment` | No auth. Returns `label`, `environmentId`, `orchestrationProtocolVersion`, `serverVersion`, `capabilities`, `platform`. |
| 2. Pairing token | `t3 auth pairing create --scope … --ttl 5m --label … --json` → `credential` | Or the pairing link from T3's UI. One-time use. |
| 3. Token exchange | `POST /oauth/token`, form-encoded: `grant_type=urn:ietf:params:oauth:grant-type:token-exchange`, `subject_token=<pairing credential>`, `subject_token_type=urn:t3:params:oauth:token-type:environment-bootstrap`, `requested_token_type=urn:ietf:params:oauth:token-type:access_token`, plus `client_label`, `client_device_type=desktop`, `client_os` | Returns a `Bearer` token (no DPoP proof sent), `expires_in` ≈ 30 days, with the granted `scope`. |
| 4. WebSocket ticket | `POST /api/auth/websocket-ticket` with `Authorization: Bearer …` | Returns `{ ticket, expiresAt }`. |
| 5. Connect | `ws://host/ws?orchestrationProtocol=2&wsTicket=<ticket>` | A protocol mismatch returns HTTP 426 with the server's version. |
| 6. RPC | Effect RPC, JSON serialization | See below. |

**Effect RPC over the socket** (Effect 4.0.5, `RpcSerialization.layerJson`). Each text frame holds one JSON message or an array of them.
- Client → server: `{"_tag":"Request","id":"1","tag":"pullRequests.list","payload":{…},"headers":[]}`; `{"_tag":"Ack","requestId":"1"}` after each stream chunk; `{"_tag":"Interrupt","requestId":"1","interruptors":[]}`; `{"_tag":"Ping"}`; `{"_tag":"Eof"}`.
- Server → client: `{"_tag":"Chunk","requestId":"1","values":[…]}` for streams; `{"_tag":"Exit","requestId":"1","exit":{"_tag":"Success","value":…}}` or `{"_tag":"Failure","cause":[{"_tag":"Fail","error":{…typed error with _tag…}}]}`; `{"_tag":"Defect",…}`; `{"_tag":"Pong"}`.
- Typed errors come through intact, e.g. `OrchestrationGetFullThreadDiffError` with its cause.

**HTTP endpoints** under `/api/orchestration/*` also need the header `x-t3-orchestration-protocol: 2`. Without it they return 400.

**Compatibility:** client and server protocol versions must match exactly (`ORCHESTRATION_PROTOCOL_VERSION` in T3's `packages/contracts/src/environment.ts`, currently 2). It has changed once so far (1 → 2). Schemas otherwise evolve additively.

**Scopes:** reads need `orchestration:read`. PR writes (comment, reply, resolve, submit review, viewed files) need `source-control:write`. Sending to agents (`orchestration.dispatchCommand`) needs `orchestration:operate`. Terminals need `terminal:read` and `terminal:operate`. mustr should request only these, never `access:write` or `relay:*`. `GET /api/auth/session` lists a session's full grant in `permissions`; its `scopes` holds only the scopes older clients know, so `source-control:write` appears in `permissions` alone (verified against 0.1.1). Typed authorization errors work the same way: `requiredPermission` is the missing scope, `requiredScope` its older parent (`orchestration:operate` for `source-control:write`).

## Data (verified on real data)

| Call | Result | Time |
|---|---|---|
| WebSocket connect | OK | 3 ms |
| `server.getConfig` | config, providers, available editors, keybindings | 31–38 ms |
| `pullRequests.list` `{state:"open", involvement:"all", limit:25}` | 3 entries (title, repository, number, projectId, host, author, …), provider summary for github.com | 2.0–2.2 s |
| `POST /api/pull-requests/diff` | unified patch: 514 KB, 45 files for `msetaro/revenant#730` | 0.9 s |
| `pullRequests.diffFileContents` | `oldContents` / `newContents` (20 KB / 21 KB), which is exactly a Zed `BufferDiff`'s base text + buffer text | 1.2 s |
| `GET /api/orchestration/shell` | 1,529 threads, 4 projects; each thread has `worktreePath`, `branch`, `status`, `linkedPullRequest`/`pullRequests`, `latestVisibleMessage`, timestamps | 0.23–0.28 s |
| `orchestration.searchThreads` | works but takes 33 s (full-text search); not for listing | 33 s |
| `orchestration.getFullThreadDiff` | needs `toTurnCount` from the thread's turn summaries (`checkpointTurnCount`); guessing `1` fails with a typed error | — |

## What this means for mustr

- **Inbox:** the shell snapshot is fast enough to load all threads at once. Keep it fresh with `orchestration.subscribeShell`. `pullRequests.list` makes live forge calls (~2 s), so render the cached list first and refresh through `pullRequests.subscribeRefreshes`.
- **PR review view:** build each file's diff from `diffFileContents` (old/new text) into a Zed `BufferDiff`. Use the patch from `/api/pull-requests/diff` for the file list and hunks.
- **Agent thread review:** threads run in local worktrees (`worktreePath`), so the branch view can use Zed's own git diff on the real worktree, which gives full IntelliSense. Per-turn views use `orchestration.getTurnDiff` with `checkpointTurnCount` from the thread detail (`GET /api/orchestration/threads/:id`).
- **Codegen:** messages are plain JSON with `_tag` unions, which map directly to `#[serde(tag = "_tag")]` enums.
- **Credential:** store the bearer token in the OS keychain. It lasts about 30 days, so re-pair or refresh before it expires (check whether `/oauth/token` supports a refresh path in M0).

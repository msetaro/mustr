# M0 spike: Rust client ↔ T3 Code server

Ran on 2026-10-27 against the local T3 server ("ironforge", T3 runtime 0.1.1, `127.0.0.1:3773`), using a read-only pairing token. The spike's sessions were revoked afterwards. mustr now pins T3 Code 0.1.3; which versions it works with is under "Compatibility" at the end.

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

**Scopes:** reads need `orchestration:read`. PR writes (comment, reply, resolve, submit review, viewed files) need `source-control:write`. Sending to agents (`orchestration.dispatchCommand`) needs `orchestration:operate`. Terminals need `terminal:read` (following them) and `terminal:operate` (opening and typing in them). Changing T3's settings needs `settings:write`, and `providers:manage` for its agents' settings; installing and signing in to agents needs `providers:manage`. mustr requests only these, never `access:write` or `relay:*`. `GET /api/auth/session` lists a session's full grant in `permissions`; its `scopes` holds only the scopes older clients know, so `source-control:write` appears in `permissions` alone (verified against 0.1.1). Typed authorization errors work the same way: `requiredPermission` is the missing scope, `requiredScope` its older parent (`orchestration:operate` for `source-control:write`).

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

## Compatibility

mustr is built and tested against one T3 Code release, `t3_server_version` in `mustr-crates/t3_client/T3_VERSION` (0.1.3 since 2026-10-28), and its own engine runs exactly that release. It connects to any T3 Code that speaks the same orchestration protocol (2):

| Server | What happens |
|---|---|
| Another protocol | Refused when connecting (`check_protocol_compatibility`; the server answers HTTP 426 too). For an older installed T3, Home offers `t3 update`; for a newer one, mustr needs updating. |
| An older 0.1.x, such as 0.1.1 | Works, with what that version has. A call it doesn't have fails with Effect's "Unknown request tag" defect, which `RpcError::is_unknown_method` recognizes and mustr's backend turns into `thread_chat::UnsupportedRpc`; callers fall back to the older way, such as the family bar using the shell's `lineage` instead of `orchestration.getThreadFamily`. For an installed T3, Home says quietly "T3 Code 0.1.3 is available." with an Update button (`t3 update`). Nothing blocks. |
| A newer 0.1.x, such as the 0.1.4 nightlies | Works. What it adds decodes tolerantly (below). A call it dropped falls back the same way: the 0.1.4 nightlies dropped `server.getSettings`, so the new-thread form reads the settings from `server.getConfig`. `t3_api`'s `newer_server` test checks that every `rpc::` and `http::` call in mustr's crates still exists on the newest recorded nightly, or has a declared fallback. |

Checked live on 2026-10-28 against 0.1.1 (the user's T3, read-only), 0.1.3 (mustr's engine), and 0.1.4-nightly.20261028.3841 (in a temporary home).

**What mustr doesn't opt into.** T3 0.1.3 added opt-ins that change what a server sends: `omitIdleSubagents` and `omitOlderSettled` on the shell, and `acceptDedupedBodies`, `acceptCompactIds`, `acceptCompactTurnItems` and `acceptBoundedSnapshot` on a thread's subscription. mustr sends none of them, so the 0.1.3 shell still holds every active thread, as 0.1.1's does. The family bar reads `getThreadFamily` anyway and merges it behind the shell's rows, as T3's own client does, so it stays right if mustr opts into a smaller shell later.

**Tolerant decoding.** A newer server can add a field, an enum value, a union member, an event type, a turn-item kind, or a list element of a shape this build can't read. None of them fails a snapshot, a stream, or a list:
1. Unknown fields are ignored: no generated type denies them.
2. String enums with two or more values and tagged unions have an `Unrecognized` variant. An unknown value decodes to it, is logged once per kind (`t3_api::forward_compat::note_unrecognized`), and the views skip it: unknown thread events go to T3's own unknown-event arm and keep their place in the sequence, unknown turn items and shell items are left out, and an unknown status shows as the nearest known one.
3. T3's own lenient fields (`ForwardCompatibleArray` and the like) decode exactly as T3 decodes them.
4. Every other list or optional field of a struct or union type is lenient too, beyond T3: an element that still doesn't decode is dropped, and an optional value becomes `None`, each logged once per type. So a widened constant or a changed nested shape loses one element, such as one thread of 1,500, not the whole shell. Inside a strict pass (an untagged union's first try, or a value inside one of T3's lenient fields) they stay strict, so T3's semantics hold where T3 defines them.
5. Typed streams skip an item that still doesn't decode, logged once per RPC, instead of ending the subscription.

The codegen produces 1 to 4 (`mustr-crates/t3_api/codegen/README.md`, "Forward compatibility"); `t3_client` and `thread_chat` do 5.

**Not covered:** a required field that a newer T3 removes, renames, or makes optional, outside any list or optional value. T3 treats those as breaking and bumps the protocol for its orchestration types; the 0.1.4 nightlies did it to the OAuth metadata at `/.well-known/oauth-*`, which mustr doesn't read.

**How it's proven** (`cargo test -p t3_api`):
- `forward_compat.rs`: each mechanism on its own.
- `future_values.rs`: every sample, T3 test fixture, and captured payload, mutated where the schema allows: an unknown field on every struct (which must change nothing), an unknown or unreadable element in every list, and each tag, enum value and optional constant replaced by an unknown one, one at a time. About 4,000 mutated values, all decode.
- `newer_server.rs`: the newest nightly's own surface, recorded by `codegen/newer-server.sh`: 4,356 Arbitrary values of what it sends, for 256 RPCs and endpoints, decoded with this build's types by RPC tag and path; 12 payloads captured from a running nightly (`fixtures/newer-server/live/`, recorded by `t3_runtime`'s ignored `capture_server` test); and the check that mustr's calls still exist.
- Refresh both for each new nightly or release: `mustr-docs/SYNC.md`, "Newer servers".

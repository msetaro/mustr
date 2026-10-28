# T3-RUNTIME: obtaining, running, and pairing with a pinned T3 Code server

Research date: 2026-10-27. Sources:
- T3 source: `/tmp/prdesk-preview/src/t3code` (shallow clone of `main` at `8d94203c`, `apps/server/package.json` version `0.1.3`). Paths below are relative to that repo.
- Install scripts, downloaded but not run: `/tmp/t3-install-scripts/install.sh` and `install.ps1`.
- `gh api repos/pingdotgg/t3code/releases`.
- This machine's install: `~/.t3`, listed read-only.

## 1. Summary and recommendation

- T3 publishes a self-contained CLI/server archive for each release on GitHub Releases: `t3-<version>-<platform>-<arch>.tar.gz` or `.zip`. Each release also has a `SHA256SUMS` file. There are no detached signatures. The macOS and Windows binaries are code-signed inside the archive.
- The headless server is `t3 serve --base-dir <dir> [--host 127.0.0.1] [--port N]`. All state lives under `--base-dir` (same as `T3CODE_HOME`, default `~/.t3`).
- A client pairs in two steps. First it gets a one-time bootstrap credential, which comes from `t3 pair`, `t3 auth pairing create --json`, or the `Token:` line `t3 serve` prints. Then it exchanges that credential at `POST /oauth/token`, which returns a Bearer access token that lasts 30 days. There is no refresh token.
- Every WebSocket connection must send `?orchestrationProtocol=2`. Protocol 2 holds for v0.1.0 through the current nightly. A mismatch gets HTTP 426.
- **Recommendation:**
  - (a) If `~/.t3/userdata/server-runtime.json` points to a live server whose descriptor reports protocol 2 and a version in range, connect to it. Pair with the user's consent.
  - (b) Otherwise, download a pinned runtime into mustr's own data dir. Verify it against a SHA-256 baked into mustr, then run `t3 serve` with its own `--base-dir`, bound to 127.0.0.1, with telemetry off and a scrubbed environment.
  - Never install into, or run against, the user's `~/.t3/runtime`.

## 2. Releases, versioning, assets, checksums

**Tag scheme.** These patterns come from the install scripts (`install.sh:149-150`, `install.ps1:130-134`):

| Channel | Tag pattern | Example | GitHub flags |
|---|---|---|---|
| stable | `v<MAJOR>.<MINOR>.<PATCH>` | `v0.1.3` (2026-10-27, latest) | `prerelease=false`, marked "latest" |
| nightly | `v<next>-nightly.<YYYYMMDD>.<run>` | `v0.1.4-nightly.20261027.3783` | prerelease |
| preview | `v<x>-preview.<YYYYMMDD>.<run>` | `v0.1.3-preview.20261026.3702` | prerelease. A maintainers' test build: never use it. |

- Recent stable releases: v0.1.0 (Oct 15), v0.1.1 (Oct 21), v0.1.2 (Oct 22), v0.1.3 (Oct 27). Stable ships every few days. Nightlies ship several times a day.
- `v0.0.42` was the first release with CLI archives. Earlier releases only exist on npm.
- Version sources:
  - The tag name.
  - `apps/server/package.json` `"version"`. The npm package `t3` and `packages/contracts/package.json` have the same value.
  - `t3 --version`.
  - `serverVersion` in `GET /.well-known/t3/environment`.

**CLI archive assets.** The URL pattern is `https://github.com/pingdotgg/t3code/releases/download/v<V>/<asset>`. The install scripts allow a mirror via `T3CODE_RELEASE_BASE_URL`.

| OS/arch | Asset | v0.1.3 size |
|---|---|---|
| linux x64 | `t3-<V>-linux-x64.tar.gz` | 121,937,072 |
| linux arm64 | `t3-<V>-linux-arm64.tar.gz` | 117,659,603 |
| macOS arm64 | `t3-<V>-darwin-arm64.tar.gz` | 108,841,196 |
| Windows x64 | `t3-<V>-win32-x64.zip` | 111,970,250 |
| Windows arm64 | `t3-<V>-win32-arm64.zip` | 100,930,605 |
| **macOS x64** | **none.** Node single-executable apps segfault on x64 macOS (`.github/workflows/release.yml:645-648`). | n/a |

Examples:
- `https://github.com/pingdotgg/t3code/releases/download/v0.1.3/t3-0.1.3-linux-x64.tar.gz`
- `https://github.com/pingdotgg/t3code/releases/download/v0.1.1/t3-0.1.1-linux-x64.tar.gz`

The other assets are the Electron desktop app, which mustr doesn't need: `T3-Code-<V>-*.dmg/.exe/.zip/.deb/.AppImage`, electron-updater `latest*.yml` and `nightly*.yml`, and `*.blockmap`.

**Integrity**
- `SHA256SUMS` is in sha256sum format and lists only the `t3-*` CLI archives (`release.yml:819-833`). It is written from the final signed bytes.
  - v0.1.3 linux-x64: `63cd0f0547eef62bb4ece27fa779bbf9e2ba8531cc68542568ba9e78838dc684`
  - v0.1.1 linux-x64: `33c43ea2f803215662f5a31fdf9e1bca86ab67cac4f2398565c2ba12e17c2185`
- The GitHub API also returns `assets[].digest` (`sha256:...`). For v0.1.3 it matches `SHA256SUMS`, which gives a second source.
- None of the following exist:
  - detached signatures (.sig, minisign, cosign)
  - GitHub artifact attestations (`gh api .../attestations/sha256:<digest>` returns 404)
  - a signed `SHA256SUMS`
- So `SHA256SUMS` only proves that a download matches whatever is on the GitHub release. It is not an independent trust anchor.
- Code signing inside the archives:
  - macOS: `codesign` with Developer ID, then `notarytool` (`scripts/build-cli-archive.ts:261-331`). If the signing secrets are missing, the binary is signed ad hoc.
  - Windows: Azure Trusted Signing (`.github/workflows/release-desktop.yml:304-345`).
  - Linux: not signed.
- The npm package `t3` is published with `--provenance` (`release.yml:995`). It is not self-contained and needs Node `26.12.1` (root `package.json` `engines`).

## 3. Install script behavior (read only, not executed)

`install.sh` (240 lines; macOS and Linux; needs `sh`, `tar`, `sha256sum` or `shasum`, and `curl` or `wget`):
1. Maps the platform and arch: `uname -s` gives darwin or linux, and `uname -m` gives arm64 or x64. Any other OS or arch fails.
2. Resolves the version (lines 145-170):
   - If `T3CODE_VERSION` is set, it uses that.
   - For stable, it sends `GET https://github.com/pingdotgg/t3code/releases/latest` with `Accept: application/json` and reads `tag_name`. This is the github.com page, not the API, so it avoids the rate limit.
   - For nightly or preview, it pages through `api.github.com/.../releases?per_page=100`, up to 10 pages.
   - Preview builds are refused unless explicitly requested.
3. Downloads `SHA256SUMS`. A 404 means the release has no archive, which is the npm-only case.
4. Downloads the archive, checks its SHA-256 against `SHA256SUMS`, and fails on a mismatch.
5. Extracts with `tar -xzf --strip-components=1` into `$T3CODE_HOME/runtime/versions/.staging-XXXX`. The archive's top-level directory is `t3-<V>-<platform>-<arch>/`.
6. Smoke-tests with `t3 --version`, writes `.install-complete` containing the version, and moves the result to `$T3CODE_HOME/runtime/versions/<V>`.
7. Symlinks `$T3CODE_INSTALL_BIN_DIR/t3` (default `~/.local/bin/t3`) to the versioned `t3`. It appends the launcher path to `$T3CODE_HOME/runtime/launchers`, which protects that version from pruning.
8. It never edits shell rc files. If `~/.local/bin` isn't on PATH, it only prints a hint. It never needs sudo.

`install.ps1` (234 lines; Windows; needs PowerShell 5.1 or later) follows the same flow:
- The arch comes from `PROCESSOR_ARCHITEW6432` or `PROCESSOR_ARCHITECTURE`.
- The asset is `t3-<V>-win32-<arch>.zip`, checked with `Get-FileHash`, then `Expand-Archive`. The `t3-<V>-win32-<arch>\` folder is flattened.
- It runs `t3.exe --version`.
- It writes a `t3.cmd` shim (UTF-8 without BOM) in `~\.local\bin` instead of a symlink.
- It doesn't change PATH.

mustr can reuse this exact algorithm in Rust, minus the PATH and launcher steps, with the version and SHA-256 pinned at build time.

## 4. Runtime directory layout

From this machine's `~/.t3/runtime/versions/0.1.1/` (linux-x64), listed read-only:

```
.install-complete        6 B    "0.1.1"
t3                       160 MB  ELF x86-64 Node single-executable app (embeds Node 26 + bundled server), not stripped
node_modules/            91 MB   native and external deps: node-pty, @napi-rs/keyring, ffi-rs, @ff-labs/fff-*,
                                 @cursor/sdk(+linux-x64), playwright-core, undici, zod, @connectrpc/*, @bufbuild/protobuf, @statsig/*
client/                  48 MB   the built web UI (index.html, assets/, favicons, manifest)
                                 + third-party-licenses.json (504 entries)
resource-monitor/linux-x64/t3-resource-monitor   676 KB  Rust helper
```

- About 300 MB on disk, from a download of about 116 MB.
- `t3` loads `node_modules/` and `client/` relative to itself. Keep the extracted tree intact: `config.ts:264` resolves `client/` next to the entry.

## 5. Running headless: `t3 serve`

The flags are defined in `apps/server/src/cli/config.ts:30-85` and shared by `t3`, `t3 start`, and `t3 serve`. Each also has an env var form. Precedence is flag, then env, then bootstrap envelope (`config.ts:286-424`).

| Flag | Env | Default / notes |
|---|---|---|
| `--port N` | `T3CODE_PORT` | Must be 1-65535; `0` isn't allowed. Web mode without a port uses 3773 if it's free; otherwise the OS assigns a free loopback port (`packages/shared/src/Net.ts:170-175`; `config.ts:29` sets `DEFAULT_PORT = 3773`). Desktop mode uses 3773. |
| `--host H` | `T3CODE_HOST` | Unset means 127.0.0.1 (`server.ts:320`). Under the boot service, unset means `0.0.0.0` restricted to private networks (`cli/config.ts:411-424`). |
| `--base-dir D` | `T3CODE_HOME` | Defaults to `~/.t3`. State goes in `D/userdata` (§7). |
| `--no-browser` | `T3CODE_NO_BROWSER` | `serve` forces this on. |
| `--bootstrap-fd N` | `T3CODE_BOOTSTRAP_FD` | Reads one JSON line from fd N within a 1 s timeout (`bootstrap.ts:86`). See below. |
| `--mode web\|desktop` | `T3CODE_MODE` | Defaults to `web`. |
| `--auto-bootstrap-project-from-cwd` | `T3CODE_AUTO_BOOTSTRAP_PROJECT_FROM_CWD` | `serve` forces this off. |
| `--log-websocket-events` / `--log-ws-events` | `T3CODE_LOG_WS_EVENTS` | |
| `--tailscale-serve`, `--tailscale-serve-port` | `T3CODE_TAILSCALE_SERVE[_PORT]` | Leave off. |
| `--dev-url` | `VITE_DEV_SERVER_URL` | Dev only. |
| `[cwd]` (positional) | | Working directory for provider sessions. |
| | `T3CODE_TELEMETRY_ENABLED=false` | Turns off T3's PostHog analytics (`telemetry/AnalyticsService.ts:70`). mustr should set this. |
| | `T3CODE_LOG_LEVEL`, `T3CODE_OTLP_*`, `T3CODE_TRACE_*` | Logging and observability. |

How `serve` differs from plain `t3`/`start` (`cli/server.ts:76-85`):
- It is headless: no browser, and no project is created automatically.
- It doesn't run the "server already running" preflight. A second server on the same state dir is still refused by the `server-owner.sqlite` lock, with exit code 78 (`serverOwnership.ts`, `SERVER_EXIT_CODE_STATE_DIR_OWNED`).
- On startup it prints the following to stdout (`startupAccess.ts:183-192`):
  ```
  T3 Code server is ready.
  Connection string: http://127.0.0.1:<port>
  Token: <one-time credential, 5 min TTL>
  Pairing URL: http://127.0.0.1:<port>/pair#token=<credential>
  <QR code>
  ```
- It writes `D/userdata/server-runtime.json` with `{version:1, pid, ownerId, host?, privateNetworkOnly?, port, origin, devUrl?, startedAt, serviceManaged?, serverVersion?}` (schema at `serverRuntimeState.ts:12-24`).
- In web mode it advertises no self-update capability (`cloud/selfUpdate.ts:46-52`), which suits a pinned runtime.

The bootstrap envelope (`packages/contracts/src/desktopBootstrap.ts:8-30`, type `DesktopBackendBootstrap`):
- It is one JSON line with these fields: `{mode:"desktop", noBrowser, port, host, t3Home?, desktopBootstrapToken, desktopBootstrapSecret?, shellEnvironmentPrepared?, tailscaleServeEnabled, tailscaleServePort, otlp*Url?, desktopTelemetryFd?, desktopTelemetryControlFd?, desktopBrowserFd?, desktopBrowserControlFd?, resourceMonitorPath?}`.
- The Electron app spawns `<entry> --bootstrap-fd 3` with this envelope (`apps/desktop/src/backend/DesktopBackendConfiguration.ts:499-530`). It then exchanges `desktopBootstrapToken` at `/oauth/token` (`apps/desktop/src/backend/DesktopLocalEnvironmentAuth.ts:86-110`).
- Desktop mode has three effects:
  - The server exits when its parent process disappears (`launcherWatch.ts`). This doesn't apply on Windows.
  - The auth policy becomes `desktop-managed-local`.
  - The server advertises `serverSelfUpdate: "desktop-managed"`. Other T3 clients that connect may then offer the user a "desktop" update.
- It's a private contract between T3's own apps. mustr could use it, but `serve` is the public, documented surface (see §10).

## 6. Service mode: `t3 service`

Source: `apps/server/src/cli/service.ts` and `cloud/bootService.ts`.
- Subcommands:
  - `install [--allow-downgrade]`
  - `restart`
  - `uninstall`
  - `status`
  - `update`, which is deprecated in favor of `t3 update`
  - All of them take `--base-dir`.
- Supported only on Linux with systemd and on macOS with launchd. Windows is unsupported (`service.ts`, `formatServiceStatus`).
- Linux:
  - The unit file is `~/.config/systemd/user/t3code.service`, a user unit (`Restart=always`, `KillMode=mixed`, `WantedBy=default.target`).
  - It sets `T3CODE_HOME` and `T3_BOOT_SERVICE_UNIT`, and runs `ExecStart=<home>/runtime/versions/<V>/t3 __service-launcher`.
  - Logs append to `<home>/userdata/logs/boot-service.log`.
  - To keep running after logout, the user needs **`sudo loginctl enable-linger $USER`**. T3 prints this instruction and never runs sudo itself (`bootService.ts:426`).
- macOS: the plist is `~/Library/LaunchAgents/com.t3tools.t3code.service.plist` (label `com.t3tools.t3code.service`), installed with `launchctl bootstrap gui/<uid>`.
- How the launcher works (`serviceLauncher.ts`):
  - It reads `<home>/runtime/service-state.json`, which contains `{protocol:3, activeVersion, update?}` (`SERVICE_LAUNCHER_PROTOCOL = 3`).
  - It spawns `runtime/versions/<activeVersion>/t3 serve` with `T3_SERVICE_LAUNCHER_CONTEXT` set in the env and an IPC channel.
  - It merges `<home>/service.env` into the child's env (lines 98-110). This is how a user sets `T3CODE_HOST`.
  - Self-update runs a trial child. Before an update it backs up the DB to `runtime/db-backup/<updateId>/`, and rolls back on failure.
- On this machine:
  - The unit's ExecStart is the **0.1.0** launcher.
  - `service-state.json` has `activeVersion` 0.1.1.
  - `server-runtime.json` reports `serviceManaged: true`, `serverVersion: 0.1.1`, and origin `http://127.0.0.1:3773`.
- mustr should **never** install a service of its own. The unit and label names are fixed and would collide with the user's T3.

## 7. State: `T3CODE_HOME` / `--base-dir`

Paths come from `apps/server/src/config.ts:138-173`. With a dev URL the state dir is `dev/`; otherwise it's `userdata/`.

```
<base>/
  userdata/                  stateDir
    statev2.sqlite(-wal,-shm)   orchestration DB (state.sqlite is a legacy DB)
    server-owner.sqlite         lifetime ownership lock
    server-runtime.json         live server record (pid, port, origin, serverVersion, ...)
    settings.json  keybindings.json  themes/  environment-id  anonymous-id
    secrets/                    signing keys, cloud/relay creds, DPoP replay markers (0600)
    attachments/  browser-artifacts/  logs/{server.trace.ndjson*, provider/, terminals/, boot-service.log}
  caches/                    provider status caches, pull-requests/
  worktrees/                 agent worktrees
  runtime/versions/<V>/      installed runtimes (§4); runtime/launchers; runtime/service-state.json; runtime/db-backup/
  service.env                optional env for the boot service
```

- Agent CLI logins live in each CLI's own home (for example `~/.claude` or `~/.codex`), not in `<base>`. A separate `--base-dir` therefore still uses the user's agent logins, but it has separate threads, projects, settings, and forge credentials.
- The Electron app also defaults to `~/.t3` (`apps/desktop/src/app/DesktopStatePaths.ts:13-21`). Desktop, service, and CLI all share one environment.

## 8. Pairing and token exchange

**Scopes.** These come from `packages/contracts/src/auth.ts:30-66`:
- `orchestration:read|operate`
- `settings:write`
- `providers:manage`
- `environment:maintain`
- `preview:operate`
- `diagnostics:read`
- `terminal:read|operate`
- `source-control:write`
- `filesystem:read|write`
- `access:read|write`
- `relay:read|write`

`review:write` exists but can't be granted. The default set is `AuthStandardClientScopes` (`auth.ts:149-166`), which is every grantable scope. Repeating `--scope` replaces the default set.

**Ways to get a one-time bootstrap credential.** All of them are local and need filesystem access to `<base>` as the same OS user:
1. `t3 pair [--base-dir D] [--scope S]... [--label L] [--ttl 5m] [--tailscale]` (`cli/pair.ts:446-545`):
   - It finds the server through `server-runtime.json`, checks the pid is alive, and probes `GET /.well-known/t3/environment`.
   - It **refuses if the server's protocol ≠ the CLI's** (`pair.ts:493`).
   - It prints human-readable text: a QR code, `Pairing URL: <origin>/pair#token=<cred>`, `Token: <cred>`, and `Expires: <iso>`. There's no JSON option.
   - The TTL defaults to 5 minutes.
2. `t3 auth pairing create [--base-dir D] [--scope ...] [--label L] [--ttl T] [--base-url U] --json` (`cli/auth.ts:80-111`):
   - It prints `{id, credential, label?, scopes, expiresAt, pairUrl?}`. This is the best option for automation.
   - `t3 auth pairing list|revoke <id>` manage pairing credentials.
3. `t3 auth session issue ... --token-only|--json` (`cli/auth.ts:159-192`) issues a Bearer token directly and skips the exchange.
4. The `Token:` line printed by `t3 serve` at startup (5 min TTL).
5. A user-supplied link `http(s)://host:port/pair#token=<cred>`. The token is in the fragment; `?token=` is also accepted. The mobile app also takes `t3code://...?pairingUrl=<encoded>` (`apps/mobile/src/features/connection/pairing.ts`). Pairing links have no custom URL scheme of their own.

**Hazard:** `t3 pair` and `t3 auth` open the target database with the CLI's own migrations (comment at `pair.ts:247-250`). mustr must run them with the binary that matches the running server, `<base>/runtime/versions/<serverVersion>/t3`. It must never use mustr's own pinned runtime against the user's DB, or it could migrate the live DB to an unreleased schema.

**Exchange.** These are wire details from `auth.ts:219-238`, `environmentHttp.ts:552-558`, and `auth/EnvironmentAuth.ts:937-990`.

Example exchange (fill in the placeholders):

```sh
ORIGIN=http://127.0.0.1:3773          # from server-runtime.json .origin
CRED='<one-time credential>'          # never log it

# 0. Unauthenticated descriptor: identity, version, protocol
curl -s "$ORIGIN/.well-known/t3/environment"
# -> {environmentId, label, platform:{os,arch}, serverVersion:"0.1.1", orchestrationProtocolVersion:2, capabilities:{...}}

# 1. Token exchange (form-encoded; RFC 8693 shape)
curl -s -X POST "$ORIGIN/oauth/token" \
  --data-urlencode grant_type=urn:ietf:params:oauth:grant-type:token-exchange \
  --data-urlencode subject_token="$CRED" \
  --data-urlencode subject_token_type=urn:t3:params:oauth:token-type:environment-bootstrap \
  --data-urlencode requested_token_type=urn:ietf:params:oauth:token-type:access_token \
  --data-urlencode client_label=mustr --data-urlencode client_device_type=desktop \
  --data-urlencode client_os=Linux      # optional: scope="<space-separated subset>"
# -> {access_token, issued_token_type:"urn:ietf:params:oauth:token-type:access_token",
#     token_type:"Bearer", expires_in:<~2592000>, scope:"..."}

# 2. Use it on HTTP
curl -s -H "Authorization: Bearer $AT" -H "x-t3-orchestration-protocol: 2" "$ORIGIN/api/auth/session"

# 3. WebSocket: mint a 5-minute single-use ticket, then connect
curl -s -X POST -H "Authorization: Bearer $AT" "$ORIGIN/api/auth/websocket-ticket/rpc"   # -> {ticket, expiresAt}
#   ws://127.0.0.1:3773/ws?wsTicket=<ticket>&orchestrationProtocol=2&clientSurface=desktop&clientAppVersion=<mustr ver>
#   (a native client may instead send "Authorization: Bearer" on the upgrade; ws auth falls back to it, EnvironmentAuth.ts:1228-1279)
```

- **TTLs** (`auth/SessionStore.ts:435-436`, `auth/PairingGrantStore.ts:249-251`):
  - Bearer session: 30 days
  - DPoP-bound session: 1 hour
  - WebSocket ticket: 5 minutes
  - One-time pairing credential: 5 minutes by default
  - Desktop bootstrap: 24 hours
- **No refresh token.** When the 30 days are up, mustr has to pair again. For a managed runtime this happens silently via `t3 auth`. For the user's environment it needs a new link or consent.
- DPoP: if the exchange request includes a `DPoP:` proof header, the token is bound to that key and `token_type` is `DPoP`. T3 uses this for relay (T3 Connect) connections. mustr v0.1 can use Bearer over loopback.
- **Revocation:**
  - `POST /api/auth/clients/revoke {sessionId}`
  - `POST /api/auth/pairing-links/revoke {id}`
  - `GET /api/auth/clients`
  - the user can also revoke the "mustr" connection in T3's settings, or with `t3 auth session revoke <id>`.
- mustr should store only `{environmentId, origin, access_token, expiresAt}` in the OS keychain.

## 9. Discovering an existing environment; compatibility check

Discovery mirrors `discoverPairTarget` (`cli/pair.ts:239-283`):
1. The base dir is `$T3CODE_HOME`, or `~/.t3` if that's unset. Read `<base>/userdata/server-runtime.json`, and also `<base>/dev/server-runtime.json` for dev builds.
2. Require the recorded `pid` to be alive. Then `GET <origin>/.well-known/t3/environment` and decode the descriptor (`packages/contracts/src/environment.ts:152-159`).
3. **Protocol:** `orchestrationProtocolVersion` must equal mustr's generated `ORCHESTRATION_PROTOCOL_VERSION`.
   - Its value is `2` at `packages/contracts/src/environment.ts:13`, the same in tags v0.1.0, v0.1.1, v0.1.2, v0.1.3, and v0.1.4-nightly.20261027.3783.
   - A missing field means protocol 1.
   - The server enforces this on `/ws`: a missing or different `orchestrationProtocol` gets **HTTP 426** `{code:"orchestration_protocol_incompatible", orchestrationProtocolVersion}` (`apps/server/src/ws.ts:635-640, 3329-3338`).
   - T3's own client applies the same rule (`packages/client-runtime/src/connection/compatibility.ts`). If the server's number is higher, the client must update. If it's lower, the server must update.
4. **Version:** the pinned `T3_VERSION` is the tested version. mustr connects to any version on the same protocol and falls back for calls a server lacks (`mustr-docs/T3-PROTOCOL.md`, "Compatibility").
   - If the version is out of range, offer the user `t3 update <V>`. That is the user's own action and is self-updatable when the descriptor has `capabilities.serverSelfUpdate`.
   - Otherwise fall back to mustr's own runtime.
5. A missing file or a dead pid means T3 is installed but not running. mustr shouldn't start the user's server. It can tell the user to run `t3` or `t3 service install`, or use its own runtime.
6. Detecting an install: any of `~/.t3/runtime/versions/*/.install-complete`, `~/.local/bin/t3`, `t3` on PATH, or the desktop app (`com.t3tools.t3code`).

Silent pairing is technically possible on the same machine as the same user (`t3 auth pairing create --json`), but it is still a credential grant with full scopes. Ask for consent once, for example "Connect mustr to your T3 Code (0.1.1 at 127.0.0.1:3773)?". Use `--label mustr` and a minimal `--scope` set. The default set includes `access:write` and `relay:write`; leave them out unless mustr needs them.

## 10. Recommendation for mustr

**(a) The user has T3 Code. This is the default; threads are shared with T3's desktop and mobile apps.**
- Discover the server per §9, check the protocol and version, and get the user's consent.
- Mint a credential with `<base>/runtime/versions/<serverVersion>/t3 auth pairing create --base-dir <base> --label mustr --ttl 2m --json`. The binary must match the server version. As a fallback, accept a pasted `/pair#token=` link.
- Exchange the credential (§8) and store the token in the keychain.
- Remote environments (over Tailscale or LAN) work only with a pasted link.

**(b) No compatible T3 is available: mustr manages its own pinned runtime.**
- **Location:** `<mustr data dir>/t3/runtime/<V>/` holds the extracted archive, and `<mustr data dir>/t3/home/` is the `--base-dir`. The runtime must not live in `~/.t3`: T3's pruning and launchers manage that tree, and the service would see it.
- **Pin:**
  - `mustr-crates/t3_client/T3_VERSION` holds the version.
  - A checked-in `T3_RUNTIME.sha256` holds one SHA-256 per asset, copied from the release's `SHA256SUMS` and cross-checked against the API `digest` when `mustr-sync-t3` runs.
  - mustr verifies the download against **its own baked-in hash**, not a `SHA256SUMS` fetched at runtime. That makes the pin independent of later changes to the release.
  - On macOS, also check `codesign --verify` and the Team ID.
- **Download:** `https://github.com/pingdotgg/t3code/releases/download/v<V>/t3-<V>-<os>-<arch>.(tar.gz|zip)`. Write to a staging dir, verify, extract with strip-components=1, run `t3 --version`, write `.install-complete`, then rename atomically.
- **Unsupported:** there is no macOS x64 archive. Either drop Intel Macs or require (a) there.
- **Spawn** (as a child process; never as a service):
  - `t3 serve --base-dir <data>/t3/home --host 127.0.0.1 [cwd=<data>/t3/home]`
  - Omit `--port` so T3 uses 3773 if it's free, or else an OS-assigned port. Read the actual origin from `<home>/userdata/server-runtime.json`, matching on the child pid, or from the `Connection string:` stdout line.
- **Env:**
  - Start from a scrubbed environment: **remove every `T3CODE_*` and `T3_*` variable**. When mustr is started from a terminal inside a T3 service, it inherits `T3_SERVICE_LAUNCHER_CONTEXT`, `T3CODE_HOME`, `T3_BOOT_SERVICE_UNIT`, and `T3CODE_TAILSCALE_SERVE*`. These are set in this session right now. They would make the child act like the service: it would bind 0.0.0.0, and its `--base-dir` could be overridden.
  - Then set `T3CODE_TELEMETRY_ENABLED=false`.
- **Credential:**
  - Parse the `Token:` line from the child's stdout, or run `<runtime>/t3 auth pairing create --base-dir <home> --json`.
  - Exchange it, then cache the Bearer token in the keychain.
  - Pair again automatically when the token expires or the server returns 401.
- **Lifecycle:**
  - In web mode the child doesn't watch its parent. On launch, mustr should check any existing `server-runtime.json` (pid alive, `ownerId`). If a server is already running there, reuse it; otherwise start a new one.
  - mustr owns shutdown: SIGTERM, then a timeout, then SIGKILL. The ownership lock is released automatically after SIGKILL.
  - On Windows, use a Job Object so the child dies with mustr.
  - Exit code 78 means another process owns the state dir: don't restart in a loop.
- **Updates:** only bump when `mustr-sync-t3` passes the contract tests. Keep the previous `<V>` dir until the new one starts. Before a version change, back up `statev2.sqlite*`; T3's launcher does the same.
- **Alternative to evaluate in the M0 spike:** desktop mode via `--bootstrap-fd 3`, which gives parent-death shutdown and a token mustr generates itself. The cost is depending on T3's private desktop envelope and advertising `serverSelfUpdate: desktop-managed`.

## 11. Licensing obligations

- **T3 Code is MIT**, "Copyright (c) 2026 T3 Tools Inc." (`LICENSE`). mustr derives code from it in two ways:
  - `t3_client` types generated from `packages/contracts`. Ship T3's full MIT notice in `THIRD_PARTY_NOTICES.md` and in `mustr-crates/t3_client/` (for example `LICENSE-T3`). Header-comment the generated files with their source and version.
  - If any T3 install logic is ported, the same notice covers it.
- **The runtime is downloaded, not redistributed.** mustr fetches it from T3's official GitHub release on the user's machine, so mustr doesn't redistribute T3's binaries.
  - Show T3's notice in mustr's About/Licenses screen.
  - Link to `<runtime>/client/third-party-licenses.json`, which is the runtime's own notice file (504 entries in 0.1.1, built from `third-party-licenses.config.json`).
  - GPL-3.0 (mustr) and an MIT server talking over HTTP are separate programs, so this is not a combined work.
- **Do not bundle the runtime in mustr installers** without legal review. Besides MIT, Apache, ISC, and BSD code, it contains components that are not open source:
  - `@anthropic-ai/claude-agent-sdk` ("SEE LICENSE IN README.md": Anthropic's terms, inside the bundled server)
  - `@cursor/sdk` and `@cursor/sdk-linux-x64` ("© Anysphere Inc. All rights reserved… Cursor Terms of Service", in `node_modules/`)
  - Node.js, embedded in the single executable, isn't listed in `third-party-licenses.json`.
- T3's trademarks ("T3 Code" and its logo) aren't covered by MIT. mustr should say it is "compatible with T3 Code", not use T3's branding, and keep its "not affiliated" line.

## 12. Open questions and items needing the user

- **Unverified because this research made no requests to the running server:**
  - whether a one-time token is accepted when the server policy is `desktop-managed-local`, where `bootstrapMethods` lists only `desktop-bootstrap`. The list appears only in `auth/EnvironmentAuthPolicy.ts`, and nothing in the server enforces it, so it looks advisory.
  - whether Bearer sessions slide on use or expire exactly at 30 days.
  - the exact `/ws` RPC framing. That is in the M0 spike's scope.
- Whether `t3 serve` (not linked to T3 Connect) makes any outbound calls besides PostHog. Examples could be update checks, the relay at `T3CODE_RELAY_URL`, or the model manifest. Check by running it under a network monitor or offline in the spike.
- T3 publishes no signature that is independent of GitHub. To get one, ask upstream for GitHub artifact attestations or a signed `SHA256SUMS`. Until then, mustr's own pinned hash is the trust anchor.
- Supporting macOS x64 needs a product decision: drop Intel Macs, or require the user's own T3 there.
- Settled for now as protocol-only compatibility, with fallbacks for calls a server lacks and tolerant decoding (`mustr-docs/T3-PROTOCOL.md`, "Compatibility"). The protocol number has stayed at 2 while the RPC shapes still change between releases; `t3_api`'s `newer_server` test checks the newest nightly.
- **Needs the user:**
  - Nothing for (b). It requires no sudo and no credentials.
  - For (a), consent to pair mustr with their T3.
  - Only if they want the *T3 service* to survive logout on Linux: `sudo loginctl enable-linger $USER`. That is T3's requirement, not mustr's.

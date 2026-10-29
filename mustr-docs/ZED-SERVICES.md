# ZED-SERVICES.md: Zed's AI and hosted services in mustr

mustr's agent features are its own: agent threads (`thread_chat`), agent reviews and Send to agent
(`review`, `mustr`), all backed by T3 Code. Zed's AI features and Zed's hosted services are not part
of mustr. This file lists each one, what mustr does about it, and how that's checked. Paths are
relative to the repo root; Zed line numbers are from the Zed tree in `mustr-docs/ZED_VERSION`.

## How it's kept out

mustr never edits Zed's crates, so it works in layers:

1. **Not depended on.** No mustr crate depends on a Zed AI or service crate. `script/mustr-check-no-ai`
   fails if one does, or if Zed's core crates start pulling a new one into mustr's graph.
2. **Not initialized.** mustr's `main.rs` calls only the inits in the keep list of
   `mustr-docs/ZED-INTEGRATION.md`. None of Zed's AI or service inits run.
3. **Off in the default settings** (`mustr-crates/mustr/assets/default_overrides.json`):
   `disable_ai: true` (a saturating setting, so user and project settings can't turn AI back on),
   `agent.enabled: false`, `edit_predictions.provider: "none"`, `show_edit_predictions: false`,
   telemetry off, `auto_update: false`, extension auto-installs off, and
   `server_url: "https://zed.invalid"`.
4. **Hidden** (`mustr-crates/mustr/src/hidden_actions.rs`): Zed's AI and service actions are
   hidden from the command palette (with Zed's `CommandPaletteFilter`), from vim's `:` commands
   (mustr wraps vim's palette interceptor), and from every keymap, including the user's
   (`mustr-crates/mustr/src/keymap.rs`).
5. **Blocked** (`mustr-crates/mustr/src/zed_services.rs`): `ZedHostGuard` wraps the HTTP client
   mustr installs, so every request made through `cx.http_client()` (which includes the
   `Client`, telemetry, Node and language-server downloads, and images) to `zed.dev`, any
   subdomain of it, or `zed.invalid` fails locally, before any DNS lookup. It follows redirects
   itself, so a redirect can't reach those hosts either, and logs each refused host once.
   `scrub_env` removes `ZED_SERVER_URL`, `ZED_RPC_URL`, `ZED_IMPERSONATE`, `ZED_ADMIN_API_TOKEN`,
   `ZED_WEB_LOGIN`, `ZED_MINIDUMP_ENDPOINT`, `ZED_GENERATE_MINIDUMPS`, and
   `ZED_CLIENT_CHECKSUM_SEED` before anything reads them. It runs first in `main`, which also covers
   the login-shell environment: Zed captures that by re-running the binary with `--printenv`.

Statuses below: **Removed** means it isn't compiled into mustr. **Inert** means Zed's core crates
compile it in; mustr never initializes it, hides its entry points, and blocks its hosts. Removing an
inert crate would mean patching a Zed crate's `Cargo.toml`, or dropping a Zed crate that mustr needs.

## Zed AI

| Feature | Crates | Status in mustr | Verified by |
|---|---|---|---|
| Agent panel, its threads and text threads, agent UI | `agent_ui`, `agent`, `acp_thread`, `acp_tools` | Removed | check script |
| External agents (ACP) | `agent_servers` | Removed. `project`'s `AgentServerStore` (`crates/project/src/project.rs:1377`) is inert: it reads `agent_servers` settings and launches nothing by itself. The ACP registry (`AgentRegistryStore`, `cdn.agentclientprotocol.com`) is created only by Zed's `main`. | check script |
| Inline assistant | `agent_ui` | Removed. `assistant::InlineAssist` is compiled in (`zed_actions`) and the split diff editor registers it (`crates/editor/src/split.rs:2302`): hidden. Its bindings are dropped. | palette tests, live palette "inline" |
| Edit predictions (Zeta, Copilot, Supermaven, Codestral, Ollama, ...) | `edit_prediction`, `edit_prediction_ui`, `copilot*`, `codestral` | Removed. Inert: `edit_prediction_types` and the editor's prediction plumbing (via `editor`), and `zeta_prompt`'s types (via `cloud_api_types`); no provider is ever registered, AI is off, the provider is `none`. The editor still registers `editor::*EditPrediction` actions (`crates/editor/src/element.rs:607,687,772-774`) and the workspace `workspace::ToggleEditPrediction` (`crates/workspace/src/workspace.rs:8088`, which would write `show_edit_predictions` to the settings file): hidden, and their bindings (`alt-\`, `alt-tab`, ...) are dropped. | palette, keymap, and settings tests; live palette "predict" |
| Copilot, Copilot Chat | `copilot`, `copilot_chat`, `copilot_ui` | Removed | check script; live palette "copilot" |
| Language model providers | `language_models`, `language_models_cloud`, `open_ai`, `anthropic`, `google_ai`, `x_ai`, `ollama`, `lmstudio`, `mistral`, `deepseek`, `bedrock`, `open_router`, `opencode`, ... | Removed. Inert: `language_model`, `language_model_core`, `cloud_llm_client` (via `agent_settings` and `settings_content`); `language_model::init` never runs, so no provider is registered. Their settings in Zed's `default.json` are read by nothing mustr links. | check script |
| Agent settings | `agent_settings` | Inert (via `workspace`, `git_ui`, `diagnostics`), read only as a gate. `AgentSettings::enabled` is `enabled && !disable_ai` (`crates/agent_settings/src/agent_settings.rs:263`). | settings tests |
| AI commit messages, prompts, skills | `prompt_store`, `agent_skills` | Inert (via `git_ui`). `git::GenerateCommitMessage` is hidden, and its handlers return early unless AI is on (`crates/git_ui/src/git_panel.rs:4276`, `commit_modal.rs:621`). mustr doesn't build Zed's git panel. | palette tests |
| MCP servers | `context_server`, `project`'s `ContextServerStore` | Inert (via `project`). The store doesn't start servers and stops running ones while `disable_ai` is on (`crates/project/src/context_server_store.rs:476-540`, `:1766-1778`). `context_server::Restart` is hidden. The OAuth client-metadata URL (`crates/context_server/src/oauth.rs:37`) is only sent as a `client_id` during an MCP OAuth flow, which never runs. | settings tests |
| Zed's threads sidebar | `workspace` (`multi_workspace`, `agents_sidebar` actions) | Inert. It needs `!disable_ai && agent.enabled` (`crates/workspace/src/multi_workspace.rs:426-428`), and both are off. Its actions are hidden. | palette tests |
| Zed's diff review sent to its agent | `editor` (`*ReviewComment`, `SendReviewToAgent`), `git_ui` (`git::ReviewDiff`) | Inert. The gutter button needs a feature flag and AI on (`crates/editor/src/element.rs:2661-2667`). Actions hidden. mustr's own reviews and Send to agent are separate. | palette tests |
| AI onboarding | `ai_onboarding`, `onboarding` | Removed. `zed_predict_onboarding::*` and `zed::OpenOnboarding` hidden. | check script, palette tests |
| AI settings pages | `settings_ui` | Removed. mustr's own Settings page (`mustr/src/settings_page.rs`) has mustr's settings and Zed's that mustr uses; mustr never opens `settings.json`. | check script |
| Right-click menus | `editor`, `terminal_view` | Inert. The editor's "Add to Agent Thread" needs AI on (`crates/editor/src/mouse_context_menu.rs:223-226`, `:294-296`). The terminal's "Inline Assist" and "Add to Agent Thread" (`crates/terminal_view/src/terminal_view.rs:627-630`) need `assistant_enabled`, which only `agent_ui` sets. | live right-click in a diff and an editor |
| Buttons and indicators | `diagnostics`, `git_ui`, `workspace` | Inert, gated by `AgentSettings::enabled`: diagnostics' Inline Assist (`crates/diagnostics/src/toolbar_controls.rs:51`), commit-message generation (`crates/git_ui/src/git_panel.rs:6306`), Review Diff (`branch_diff.rs:799`), Resolve with Agent (`conflict_view.rs:340`), the welcome page's agent card (`crates/workspace/src/welcome.rs:420`). The status-bar threads toggle follows the threads sidebar. mustr owns its title bar and status bar, and Zed's `EditPredictionButton` lives in `edit_prediction_ui`. | live screenshots |
| Vim's `:AI` | `vim` (`crates/vim/src/command.rs:1775`) | Filtered from vim's command results by mustr's interceptor wrapper. | `vim_commands_dont_open_zeds_agent`; live before/after screenshot |

## Zed services (REBRAND.md §2.6–2.7, rows 25–36)

| Service | What it does | Status in mustr | Verified by |
|---|---|---|---|
| `server_url` and `Client` (row 25) | Base URL for every zed.dev API | Inert: `Project` and `Workspace` require a `Client`. Its base is `https://zed.invalid`, the env overrides are scrubbed, and the guard refuses Zed hosts. | `effective_defaults_turn_off_zed_ai_and_services`, guard tests, live run with `ZED_SERVER_URL` set |
| Sign-in, account, collab RPC, cloud API (row 30) | `client`, `cloud_api_client`, `rpc`, `proto` | Inert. `client::init`, which registers `SignIn`, `SignOut`, and `Reconnect`, isn't called, so the client stays signed out and never reconnects. The `client` and `collab` namespaces are hidden. The RPC websocket (`crates/client/src/client.rs:1326-1428`), the cloud websocket (`crates/cloud_api_client/src/websocket/native.rs:68-100`), and the sign-in callback server (`client.rs:1470`) don't go through the HTTP client, and are reachable only from `Client::connect` or `sign_in`, which nothing in mustr calls. | palette tests (`client::SignIn` hidden even with a handler) |
| Channels and collab notifications | `channel`, `notifications` | Inert (via `file_finder` and `git_ui`, for UI types). `channel::init` and `notification_store::init` never run. | check script |
| Calls, screen sharing, Zed's title bar | `call`, `livekit*`, `collab_ui`, `title_bar` | Removed | check script |
| Telemetry (row 26) | `client::telemetry`, `telemetry`, `telemetry_events` | Inert. `metrics` and `diagnostics` are off, and `Telemetry::start` never runs. With metrics off, events are dropped before they're queued (`crates/client/src/telemetry.rs:571-573`). If a user turns metrics on, the upload goes to `zed.invalid` and the guard refuses it. `Telemetry::new` still creates an empty `telemetry.log`. | settings tests, live run |
| Hang telemetry | `hang_telemetry` | Removed | check script |
| Crash reports (row 27) | `crashes`, Zed's `reliability.rs` (minidumps to Sentry) | Removed. The minidump env vars are scrubbed. | check script |
| Auto-update and release notes (row 28) | `auto_update`, `auto_update_ui`, `auto_update_helper` | `auto_update` is inert (via `remote_connection`, via `git_ui`): `auto_update::init` never runs, so nothing polls and `Check` and `ViewReleaseNotes` have no handlers. The `auto_update` namespace is hidden. The UI crate and the Windows helper are removed. | palette tests, live palette "update" |
| Extension registry (row 29) | `extension_host`, `extensions_ui` | Removed (decided 2026-10-27). Auto-installs stay off. `zed::Extensions` is hidden. | check script, live palette "extension" |
| MCP OAuth client metadata (row 31) | `context_server/src/oauth.rs:37` | Inert string (see MCP servers above). | |
| Feedback, docs, status, and merch links (row 32) | `feedback`, `zed_actions` | `feedback` is removed. `feedback::*`, `workspace::Feedback`, `zed::OpenDocs`, `zed::OpenStatusPage`, `zed::GetMerch`, `zed::OpenAccountSettings`, and `zed::ShowWelcome` are hidden (none has a handler in mustr). mustr's settings and keymap file templates no longer link to zed.dev. Links left in Zed's UI are listed below. | palette tests, live palette "feedback" and "docs" |
| Zed's CLI and install-cli (rows 34–35) | `cli`, `install_cli` | Removed. mustr has no separate CLI, and the `cli` namespace is hidden. | check script |
| Remote-server download (row 36) | `auto_update` via `remote_connection` | Inert. Without `auto_update::init`, the download fails with "auto-update not initialized" before any request (`crates/auto_update/src/auto_update.rs:599-675`). `projects::OpenRemote`, `OpenWsl`, `OpenFolderInWsl`, and `OpenDevContainer` are hidden, and their handlers (`recent_projects`) aren't linked. | palette tests |
| Credentials | `zed_credentials_provider` | Inert (via `client`). Read only on sign-in, and never at startup. | |

### Zed links left in Zed's UI

These open the user's browser at zed.dev; mustr itself makes no request. Removing them needs a
patch to the Zed crate, or a mustr copy of the picker or view (REBRAND.md P5):

- The theme and icon-theme pickers' docs buttons: `crates/theme_selector/src/theme_selector.rs:551`, `crates/theme_selector/src/icon_theme_selector.rs:342`.
- The Linux file-portal error's "See docs": `crates/workspace/src/workspace_error.rs:201`.
- The invalid-tasks-file toast: `crates/project/src/project.rs:3963`.
- The custom git commands docs in the git graph and commit menus: `crates/git_ui/src/git_graph.rs:1420`, `crates/git_ui/src/commit_context_menu.rs:168` (through `release_channel::docs_url`).
- Edit prediction's missing-keybinding link, `crates/editor/src/edit_prediction.rs:2555`, which can't show without a provider.

## Network outside `cx.http_client()`

The guard covers everything that uses the app's HTTP client. These paths don't:

| Path | Where | Why it's not a concern |
|---|---|---|
| Collab RPC websocket, cloud websocket, sign-in callback | `crates/client/src/client.rs:1326-1428`, `:1470`; `crates/cloud_api_client/src/websocket/native.rs:68-100` | Reached only from sign-in or `Client::connect` (see above). |
| MCP OAuth callback server | `crates/oauth_callback_server` | Only during an MCP OAuth flow, which needs AI on. |
| Debug adapters, JS debug companion | `crates/dap/src/transport.rs:544,661`; `crates/project/src/debugger/session.rs:3095-3230` | No debug adapters are registered (`dap_adapters` isn't linked). |
| Remote server download on an SSH or Docker host | `crates/remote/src/transport/ssh.rs:1147-1240`, `docker.rs:572-640` | mustr opens no remote projects, and the download URL lookup fails first. |
| Crash upload, Windows update helper, LiveKit | Zed's `reliability.rs`, `auto_update_helper`, `livekit*` | Not linked. |
| Other `reqwest` clients | | None. Only `reqwest_client`'s own tests build one. |
| Subprocesses: `npm`, `go install`, `pip`, `git` | `node_runtime`, `languages`, `prettier_store`, `git` | Kept; see below. |

## Network mustr keeps

None of this goes to Zed's servers:

- **Language servers** for local worktrees, so files get completions and diagnostics: GitHub releases (`api.github.com`, `github.com`, `release-assets.githubusercontent.com`) for clangd, rust-analyzer, ty, ruff, vscode-eslint, and `zed-industries/package-version-server` (Zed's package.json server, downloaded from GitHub, not from zed.dev); the npm registry for the JSON, CSS, TypeScript, vtsls, Tailwind, YAML, Bash, and Pyright servers; the Go module proxy (gopls) and PyPI (pylsp).
- **Prettier** and its plugins from the npm registry, for formatting.
- **Node.js** from `nodejs.org`, only when there's no Node on `PATH`.
- **Git hosting providers**: commit authors and avatars for blame and commit tooltips (GitHub, GitLab, Bitbucket, Gitee, Gitea, Chromium).
- **Images** in Markdown, through `cx.http_client()`.
- **git** fetch, pull, and push, when the user runs them.
- **T3 Code**: mustr's own server connection (`t3_client`), `127.0.0.1:3773` by default.

## Verification

- `script/mustr-check-no-ai`: no mustr crate depends directly (normal, build, or dev) on a Zed AI or
  service crate, and the inert ones in mustr's graph are exactly the 17 listed in the script
  (`agent_settings`, `agent_skills`, `auto_update`, `channel`, `cloud_api_client`,
  `cloud_api_types`, `cloud_llm_client`, `context_server`, `edit_prediction_types`,
  `language_model`, `language_model_core`, `notifications`, `prompt_store`, `telemetry`,
  `telemetry_events`, `zed_credentials_provider`, `zeta_prompt`).
- `cargo test -p mustr`:
  - `hidden_actions`: every compiled-in action whose name looks like Zed AI or a Zed service is
    hidden; the palette, with an editor focused, lists none of them even though Zed's crates register
    some; vim's `:AI` returns nothing while `:w` still works.
  - `keymap`: with every base keymap, in vim mode or not, no binding is to an AI or service
    action; the user's own bindings to them are dropped.
  - `app_settings`: the effective defaults turn off AI and edit predictions, telemetry is off, and
    `server_url` is `zed.invalid`.
  - `zed_services`: the guard refuses `zed.dev`, its subdomains, and `zed.invalid`, lets other
    hosts through, refuses redirects to Zed, and follows the request's redirect policy.
- Live under Xvfb (2026-10-28), with an isolated data dir and `strace` on `connect` and `sendto`:
  startup with `ZED_SERVER_URL` and `ZED_RPC_URL` set to Zed, a pull request review, palette searches
  for AI and service terms, right-clicks in a diff and an editor, and mustr's new-thread form.
  Zero DNS lookups or connections to Zed hosts. The only external traffic was the npm registry and
  GitHub, for the JSON language server and Prettier, when `settings.json` was opened. Screenshots:
  `m0/screens/m3-noai-*.png` in the planning folder.

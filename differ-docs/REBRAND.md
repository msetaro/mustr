# REBRAND.md: making Differ stop being "Zed"

Scope: every place the Zed tree (`differ/` at Zed main `37faef81f7`) brands the app as Zed, and how
Differ can change it without editing Zed's crates. Paths are relative to the repo root, and line
numbers are from that commit, checked with `rg`/`sed`.

**Class legend**
- **A**: Differ's own app crate (`differ-crates/differ`) can override it: it owns `main()`, init, window options, menus, the open listener, and the asset source.
- **B**: Build config, env, settings, or Differ-owned resources and scripts can override it (Differ's `Cargo.toml` bundle metadata, `cargo --config <differ file>` `[env]`, `script/differ-bundle-*`, default-settings overlay).
- **C**: Needs a patch to a Zed crate (a friction point), or Differ accepts the leftover.
- **keep**: Zed-named on purpose. Renaming it would break user configs, tools, or merges.

Proposed Differ identifiers, used below as placeholders. **The user needs to confirm them.**
Display name `Differ` (`Differ Dev` for local builds). App and bundle ID `io.github.msetaro.Differ`
(`…Differ.Dev`): Differ has no website, so the ID uses the reverse DNS of the GitHub Pages domain
the user controls. URL scheme `differ://`. Directory name `Differ`/`differ`. CLI `differ`.

---

## 1. Summary

- **Counts** (53 rows in §2): **A 24, B 9, C 13, keep 7.** Rows 5, 22, 26, 27, 32, and 48 are mixed (A+B, or A for Differ's own copy and C where a reused Zed crate calls the same API). These count under their main class. Of the 13 C rows, only **3 matter functionally**: rows 16/18 (directory names), 1/3 (display name and app ID in reused crates), and 34 (CLI). The rest are cosmetic or deferred.
- **Structural caveat, the biggest cost:** `crates/zed` is binary-only. `crates/zed/Cargo.toml:57-63` has `[[bin]] zed` and `zed_visual_test_runner`, and no `[lib]`. So `differ` **cannot call** `zed::build_window_options` (`crates/zed/src/zed.rs:360`), `initialize_workspace` (`zed.rs:430`), `open_listener`, `app_menus`, `reliability`, or `*_only_instance`. Every "A" row below assumes Differ writes or copies its own version (`main.rs` is 2,074 lines, `zed.rs` 8,674, `open_listener.rs` 3,054). That is copy drift, not a merge conflict. Keep the copies minimal and record their upstream provenance so `differ-sync-zed` can diff them (see §4).
- **Key friction points (C):**
  1. `paths::APP_NAME` is a `const "Zed"` (`crates/paths/src/paths.rs:18`). The upstream comment says "Forks should change this". The workaround is `paths::set_custom_data_dir` (A), but that still leaves `temp_dir`, the macOS `logs_dir`, and `telemetry.log` under Zed's names.
  2. `ReleaseChannel::display_name()` / `app_id()` / `app_identifier()` are hardcoded per variant (`crates/release_channel/src/lib.rs:45,206,228`). No setter exists, and `init()` only stores the variant.
  3. `crates/cli` is hardwired to Zed's binary names, socket, and pipe names. Reused unmodified, it **talks to a running real Zed** on Linux and Windows.
  4. About 436 string-literal lines containing "Zed" sit in reused crates, plus about 82 `https://zed.dev/docs` links. These are cosmetic: accept them for v0.1 and ratchet them down in CI.
- **Does not need a patch:** zed.dev traffic can be fully controlled from Differ (A/B): which subsystems get initialized, plus a default-settings overlay. The one exception is the extension registry, a product decision (row 29).

---

## 2. Inventory

### 2.1 Names and app IDs

| # | Item | Where | Current value | Class | Differ action |
|---|---|---|---|---|---|
| 1 | Display name used by reused crates | `release_channel/src/lib.rs:206-213` `display_name()`. Callers: `workspace/src/workspace.rs:7039-7045` (`${appName}` in the window title), `lsp/src/lsp.rs:1158-1162` (LSP `clientInfo.name`), `install_cli/src/install_cli_binary.rs:134`, `cli/src/main.rs:1499,1508`, `agent_servers/src/acp/v1.rs:563`, `acp/v2.rs:250-253` | `"Zed"`, `"Zed Dev"`, `"Zed Nightly"`, `"Zed Preview"` | C | The default `window_title_format` doesn't use `${appName}` (`assets/settings/default.json:265`), so it isn't visible by default. Keep the LSP `clientInfo` as "Zed": some servers special-case it. See patch P2. |
| 2 | Main window app ID (Wayland `app_id`, X11 `WM_CLASS`) | `release_channel/src/lib.rs:228-235` `app_id()`, used by `zed/src/zed.rs:366,412` | `dev.zed.Zed[-Dev/-Nightly/-Preview]` | A | Differ's own `build_window_options` goes in `workspace::AppState.build_window_options` (a fn pointer, `workspace.rs:1338`) and sets `app_id: Some("io.github.msetaro.Differ")`. This must match the `.desktop` file basename. |
| 3 | Popup and notification window app ID | `agent_ui/src/ui/agent_notification.rs:55`, `collab_ui/src/collab_ui.rs:45`, `copilot_ui/src/sign_in.rs:65` | `ReleaseChannel::app_id()` | C | These features are excluded in v0.1 (collab, Copilot, Zed agent). If agent notifications ever come back, they need P2. |
| 4 | macOS bundle: identifier, name, icon, URL schemes, Info.plist extras | `crates/zed/Cargo.toml:286-320` `[package.metadata.bundle-{dev,nightly,preview,stable}]` | `dev.zed.Zed`, `"Zed"`, `osx_url_schemes=["zed"]`, `osx_info_plist_exts=["resources/info/*"]` | B | Put the same tables in `differ-crates/differ/Cargo.toml`. cargo-bundle reads the package it bundles. `CFBundleName` also sets the macOS menu-bar app title. |
| 5 | Windows AppUserModelID | Not set at runtime by Zed. The API exists: `gpui/src/app.rs:1934` `set_app_identity`, `gpui_windows/src/platform.rs:920-935`. The installer sets it: `crates/zed/resources/windows/zed.iss:84-85` `{#AppUserId}` = `ZedIndustries.Zed[.Preview/…]` (`script/bundle-windows.ps1:277,291,305,319`) | `ZedIndustries.Zed` | A+B | Call `cx.set_app_identity("io.github.msetaro.Differ", "Differ")` in Differ's main, and use the same ID in Differ's `.iss`. |
| 6 | Windows single-instance mutex and pipe | `release_channel/src/lib.rs:44-52` `app_identifier()`, used in `zed/src/zed/windows_only_instance.rs:34,67,212` | `Zed-Editor-Stable-Instance-Mutex`, `\\.\pipe\Zed-Editor-Stable-Named-Pipe` | A | Differ's copy of `windows_only_instance.rs` uses `Differ-…` names. The installer's `AppMutex` (`bundle-windows.ps1:274`) must match. |
| 7 | Windows EXE version resource (ProductName, CompanyName, copyright, icon) | `windows_resources/src/windows_resources.rs:41-50,100-105`, called from `zed/build.rs:211`, `cli/build.rs:35`, `auto_update_helper/build.rs:9` | `"Zed"`, `"Zed Industries, Inc."`, icon from `../zed/resources/windows` | A | Differ's `build.rs` writes its own `.rc` and calls `embed_resource` directly. Do not call `windows_resources::compile`. |
| 8 | Windows window class names | `gpui_windows/src/window.rs:1424`, `platform.rs:1839` | `Zed::Window`, `Zed::PlatformWindow` | C | Invisible to users. Ignore. |
| 9 | Linux Secret Service item label | `gpui_linux/src/linux/platform.rs:57`, also used as a read filter at `:1091,1110` | `zed-github-account` | C | Cosmetic, shown in Seahorse/KWallet. Key Differ's T3 credential by a distinct URL (for example `differ-t3://<env-id>`) so it never collides. Accept the label. |

### 2.2 Icons and assets

| # | Item | Where | Class | Differ action |
|---|---|---|---|---|
| 10 | App icons (PNG, ICNS, ICO) | `crates/zed/resources/app-icon{,-dev,-nightly,-preview}{,@2x}.png`, `resources/Document.icns`, `resources/windows/app-icon*.ico`. Referenced by: Cargo bundle `icon=` (`zed/Cargo.toml:287,296,305,314`), `zed/build.rs:220-259` (X11 icon → `OUT_DIR/app_icon.png`, included at `zed/src/zed.rs:382`), the About dialog (`zed.rs:1532-1539`), `windows_resources.rs:41` (`ICON_DIR`), `script/bundle-linux:214-217`, `script/bundle-mac:135-141`, the flatpak manifest (`resources/flatpak/manifest-template.json:40`), `zed.iss` `SetupIconFile` | B | Put Differ's artwork in `differ-crates/differ/resources/`, with Differ's own `build.rs` (copy `prepare_app_icon_x11`) and bundle scripts. **Needs artwork from the user.** |
| 11 | In-app Zed logos and icons | `assets/images/zed_logo.svg`, `zed_x_copilot.svg`, `assets/icons/zed_*.svg`, `ai_zed.svg` (`icons/src/icons.rs:300-309` `IconName::Zed*`), embedded by `assets/src/assets.rs:10-24` | A | GPUI takes any `AssetSource` (`gpui/src/app.rs:233` `with_assets`). Differ passes an overlay source: Differ's files first, then Zed's `Assets`. Most of these icons belong to agent and AI UI that Differ hides. |

### 2.3 URL scheme

| # | Item | Where | Current | Class | Differ action |
|---|---|---|---|---|---|
| 12 | OS registration | macOS: `osx_url_schemes` (row 4). Linux: `crates/zed/resources/zed.desktop.in:16` `MimeType=…;x-scheme-handler/zed;`. Windows: `zed.iss:1259-1262` `HKCU\Software\Classes\zed`. Runtime: `install_cli/src/register_zed_scheme.rs:13` using `client::ZED_URL_SCHEME` (`client/src/client.rs:1930`) | `zed` | B | Register `differ` only. **Never** register `zed://`, which would hijack Zed's links. Don't wire up `RegisterZedScheme`. If needed, call `cx.register_url_scheme("differ")` yourself. |
| 13 | Handler | `zed/src/zed/open_listener.rs:135-215` (`strip_prefix("zed://file" / "zed://ssh" / "zed://extension/" / "zed://agent" / "zed://settings" / "zed://git/…")`, `"zed-cli://"`), `open_url_modal.rs:30,56` | `zed://…` | A | Differ's own open listener parses `differ://` (inbox, PR, and thread deep links). It can also accept `zed://settings/…` internally. |
| 14 | `zed://` links created by reused crates | `settings_ui/src/settings_ui.rs:1581` (copy-link `zed://settings/…`), `agent_skills/agent_skills.rs:797-825` (skill share), `client/src/zed_urls.rs:102` | `zed://…` | C | If clicked outside the app, these open real Zed. Cosmetic: accept. |
| 15 | Internal schema URIs | `settings/src/settings_store.rs:46` `zed://schemas/settings/lsp/`, `project/src/lsp_store/json_language_server_ext.rs:13` | `zed://schemas/…` | keep | These stay in-process (JSON language server). They are not OS URLs. |

### 2.4 Config, data, cache, and log directories (`crates/paths`)

| # | Item | Where | Current | Class | Differ action |
|---|---|---|---|---|---|
| 16 | App name used for all directories | `paths/src/paths.rs:18` `pub const APP_NAME = "Zed"`, `:22` `APP_NAME_LOWERCASE` | mac `~/Library/Application Support/Zed`, `~/.config/zed`. Linux `$XDG_{CONFIG,DATA}_HOME/zed`. Windows `%APPDATA%\Zed`, `%LOCALAPPDATA%\Zed` | C | A `const` can't be changed from outside. Patch P1. |
| 17 | Custom data dir | `paths.rs:103-120` `set_custom_data_dir(dir)`. It panics if called after `data_dir()` or `config_dir()`. `config_dir()` becomes `<dir>/config` (`:122-141`) and `data_dir()` becomes `<dir>` (`:144-166`) | none | A | Call it **first** in Differ's `main()`, with mac `~/Library/Application Support/Differ`, Linux `$XDG_DATA_HOME/differ`, Windows `%LOCALAPPDATA%\Differ`. This moves the DB (`db/src/db.rs:64,166` → `data/db/0-<channel>/db.sqlite`), extensions, languages, prompts, and so on. Settings end up in `<data>/config/settings.json`, not `~/.config/differ` (cosmetic). Also expose `--user-data-dir` in Differ's own `Args` and pass it in `with_restart_arguments`, as `zed/src/main.rs:262-270` does. |
| 18 | Directories that ignore the custom dir | `temp_dir()` `paths.rs:193-219` (mac `~/Library/Caches/Zed`, Linux `~/.cache/zed`, Windows `%LOCALAPPDATA%\Zed`). `logs_dir()` `:228-237` (**mac:** `~/Library/Logs/Zed`). `log_file()` `:246-249` `Zed.log`. `GLOBAL_AGENTS_FILE_DISPLAY` `:339-343` | Zed paths | C | Differ passes its own log path to `zlog::init_output_file` and the crash server (A). Leftovers: temp files (keymap editor, `fs` atomic-write fallback, crash-handler sockets) are shared with Zed. On macOS, `Client::new` → `Telemetry::new` **truncates `~/Library/Logs/Zed/telemetry.log`** (`client/src/telemetry.rs:219,284-286`). Fix with P1. |
| 19 | Project-local folder | `paths.rs:487-531` (`.zed/settings.json`, `.zed/tasks.json`, `.zed/debug.json`) | `.zed` | keep | Repos' Zed project settings keep working in Differ. |
| 20 | Remote server dirs | `paths.rs:69-81` (`.zed_server`, `.zed_wsl_server`) | | keep (defer) | SSH and WSL remoting are out of v0.1 (see row 36). |

### 2.5 Release channels

| # | Item | Where | Class | Differ action |
|---|---|---|---|---|
| 21 | Channel source | `release_channel/src/lib.rs:13-35`. Compile time: `include_str!("../../zed/RELEASE_CHANNEL")`, or `env!("ZED_RELEASE_CHANNEL")` when that env var is set (`release_channel/build.rs` sets the cfg). Debug builds also read runtime `ZED_RELEASE_CHANNEL`. Only `dev`, `nightly`, `preview`, `stable` are valid (otherwise it panics). | B | **Always set `ZED_RELEASE_CHANNEL` explicitly** for Differ builds, through `cargo --config differ-crates/differ/cargo-differ.toml` `[env]` or in `script/differ-bundle-*`. Reason: `crates/zed/RELEASE_CHANNEL` is `dev` on Zed main but `stable` at Zed's stable tags (checked `v1.25.3` with `gh api`), so merging a stable tag flips the file. Mapping: Differ release = `stable`, local = `dev`. Don't use `nightly`/`preview` in v0.1. |
| 22 | What the channel controls | `display_name`/`app_id` (rows 1-3), `docs_url` (`lib.rs:249-260`), Windows `app_identifier`, `AppVersion` build metadata (`:99-126`), `release_query_param`, `poll_for_updates`, extension Wasm API range (`extension_host/src/wasm_host/wit.rs:60-67`: dev/nightly allow unreleased API versions), the `"stable"`/`"dev"` override blocks in settings JSON, the DB scope `0-<channel>`, Zed's single-instance skip for Dev (`zed/src/main.rs:360-361`), and socket/port names | A/B | Release builds use `stable` so extension compatibility matches Zed stable. |
| 23 | App version | `AppVersion::load(pkg_version, build_id, sha)` `lib.rs:99-126` (the `ZED_APP_VERSION` env var overrides it). `release_channel::init` at `:161`. `AppCommitSha::set_global` | A | Pass Differ's own `CARGO_PKG_VERSION`. Differ's `build.rs` emits `ZED_COMMIT_SHA`/`ZED_BUILD_ID` like `zed/build.rs:47-79` does. The extension registry doesn't use the app version (`extension_host.rs:983-1000`). |
| 24 | Channel display names inside Zed binaries | `cli/src/main.rs:926-940,1203-1215` (`"Zed {channel} {version} – …"`), `windows_resources.rs:46-50` | C | Only relevant if Differ reuses those binaries (rows 33, 7). |

### 2.6 Network endpoints (zed.dev)

| # | Item | Where | Default | Class | Differ action |
|---|---|---|---|---|---|
| 25 | Base server URL | `assets/settings/default.json:2888` `"server_url": "https://zed.dev"`. The `ZED_SERVER_URL` env var overrides it (`client/src/client.rs:63-64,119-127`). Used by `Client::production` (`client.rs:584-590`) and `zed/src/main.rs:818-820` (`set_base_url`). `http_client/src/http_client.rs:281-335` maps `https://zed.dev` → `api.zed.dev` / `cloud.zed.dev`, and any other value passes through unchanged. | zed.dev | A | Differ's default-settings overlay goes in after `settings::init`, using `SettingsStore::update_default_settings` (`settings/src/settings_store.rs:929`) or `set_default_settings` (`:942`). Caveat: `client/src/zed_urls.rs` derives docs, account, and terms links from `server_url`, so a sink URL breaks those links. Those features are excluded. |
| 26 | Telemetry events | `client/src/telemetry.rs:636-657` POST `{api}/telemetry/events`, gated by the `telemetry.metrics` setting (`:386,571`). `ZED_CLIENT_CHECKSUM_SEED` is compile-time or env (`:82-90`). `crates/telemetry` (`event!` macro), `crates/hang_telemetry` | `metrics: true`, `diagnostics: true` (`default.json:1718-1723`) | A+B | The overlay sets both to `false`. Never set the checksum seed in Differ's builds. `zed/src/zed/telemetry_log.rs` is not copied. |
| 27 | Crash reporting (minidumps to Sentry) | Handler: `crates/crashes` (subprocess with `--crash-handler`, `crashes.rs:78,647`). Install gate: `client/src/telemetry.rs:98-103` (only if `ZED_GENERATE_MINIDUMPS` or `ZED_MINIDUMP_ENDPOINT`, compile-time or env, `:92-96`). Upload: `zed/src/reliability.rs:76-85,259-321` (`sentry[...]` multipart). Symbols: `sentry-cli … -p zed -o zed-dev` in `script/bundle-mac:341-347`, `bundle-linux:140-146`, `bundle-windows.ps1:180-183` | Off unless the endpoint is set | A+B | Don't set the endpoint, and don't copy `reliability.rs`. Differ's scripts drop Sentry. Local panics still log through `crashes::force_backtrace()`. |
| 28 | Auto-update | `auto_update/src/auto_update.rs:267` `init`. Assets: `cloud.zed.dev/releases/<channel>/<ver>/asset` (`:707-718`). Release notes: `zed.dev/releases/…` and GitHub (`:345-363`). `ZED_UPDATE_EXPLANATION` (`:285-313`). Also `auto_update_ui`, and `auto_update_helper` (Windows) | `"auto_update": true` (`default.json:1733`) | A | Don't call `auto_update::init` or `auto_update_ui::init`. The overlay sets `auto_update: false`. Differ's own updater comes later. |
| 29 | **Extension registry** | `extension_host/src/extension_host.rs:684,819,983-1028` → `api.zed.dev/extensions…` through the Client's base URL (`extension_host::init` takes `client: Arc<Client>`, `:320-334`). First run auto-installs `"html"` (`default.json:2358-2360`). `suggest_extensions: true` | zed.dev | A | **Decision for the user:** (a) keep Zed's registry, so Differ makes requests to `api.zed.dev` for extensions only, using a dedicated `Client` whose base is `https://zed.dev` passed to `extension_host::init` while the main Client points elsewhere; or (b) disable it, set `auto_install_extensions: {}`, and allow dev extensions only. PLAN.md's M0 goal of "no requests to zed.dev" implies (b), or an explicit exception. |

> **Decided 2026-10-27:** option (b). Zed's terms forbid accessing non-public APIs of Zed's service, so the registry is disabled and Differ makes no requests to zed.dev.

| 30 | Cloud, account, LLM, edit prediction, web search, collab RPC | `cloud_api_client/src/cloud_api_client.rs:102-305` (`build_zed_cloud_url`), `edit_prediction/src/edit_prediction.rs:1157…`, `language_models_cloud`, `web_search_providers/src/cloud.rs:74`, `client.rs:1297-1301` (`/rpc`, `ZED_RPC_URL`), sign-in `:1487-1523` | zed.dev | A | Don't initialize any of them, and never call `client.sign_in*`. This matches the plan's feature strip. |
| 31 | MCP OAuth client ID | `context_server/src/oauth.rs:37` `CIMD_URL = "https://zed.dev/oauth/client-metadata.json"` | Zed's identity | C | Only matters for OAuth MCP servers in Zed's agent. Out of v0.1. |
| 32 | Docs, feedback, and other links | `release_channel/src/lib.rs:10` `ZED_DOCS_URL` and `docs_url()`, about 82 literal `https://zed.dev/docs…` across crates, `zed/src/zed.rs:113-115` (docs, status, merch), `feedback/src/feedback.rs:19-26` (zed GitHub issues and discussions), `open_router/src/open_router.rs:500,526` (`HTTP-Referer`), `workspace/src/notifications.rs:1405` | zed.dev | C (A for Differ's copies) | These are links, not requests. Zed docs are accurate for Zed's editor features, so keep them. Don't call `feedback::init`. Differ's own "File a bug" action points to `msetaro/differ`. |
| 33 | User-Agent | `zed/src/main.rs:502-507` `"Zed/{ver} ({os}; {arch})"` | | A | Differ's main builds its `ReqwestClient` with `Differ/{ver} …`. |

### 2.7 CLI and install

| # | Item | Where | Class | Differ action |
|---|---|---|---|---|
| 34 | `crates/cli` | Bin `cli` (`cli/Cargo.toml:15-18`), clap `name="zed"` (`main.rs:51`), renamed to `bin/zed` by `bundle-linux:192` and `bundle-windows.ps1:246`. App discovery: Linux `../libexec/zed-editor`, `../lib/zed/zed-editor`, `./zed` (`main.rs:909-913`); mac walks up to `.app` and runs `Contents/MacOS/zed` (`main.rs:1335-1345` and nearby); Windows `../Zed.exe` (`main.rs:1277-1279`). IPC: Linux `data_dir()/zed-<channel>.sock` (`main.rs:948-950`, uses the Zed data dir unless `--user-data-dir` is given); Windows mutex and pipe from `app_identifier()` (`main.rs:1191,1234`). Flatpak checks `dev.zed.Zed` (`:1093,1108`). Version string "Zed …". `zed-cli://` handshake (`:586`) | C | Reused as is, it opens or forwards to **real Zed**. v0.1: **no separate CLI**. The `differ` binary takes paths through its own clap `Args`. Later: a small `differ-crates/differ_cli` (copy plus rename) or patch P4. |
| 35 | Install CLI action and helper lookup | `install_cli/src/install_cli_binary.rs:27-28` (symlink `/usr/local/bin/zed` → `Contents/MacOS/cli`). `util/src/util.rs:353-375` `get_zed_cli_path` (`bin/zed.exe`, `../bin/zed`, `./cli`), called only from `zed/src/main.rs:273` | A | Don't register `install_cli` actions. Differ's main has its own lookup if it ever needs askpass on Windows. |
| 36 | SSH and WSL remoting server | `auto_update.rs:619-671` downloads `zed-remote-server` for the app's version from zed.dev. `script/bundle-*` builds `remote_server` | C (defer) | It's keyed to Zed versions and Zed's CDN, so it can't work with Differ versioning without Differ hosting its own builds. Out of v0.1. |

### 2.8 Packaging scripts and resources (Zed's scripts all build `--package zed`)

| # | Item | Where | Class | Differ action |
|---|---|---|---|---|
| 37 | macOS bundle | `script/bundle-mac`: `cargo bundle` in `crates/zed` with `zed-industries/cargo-bundle` `zed-deploy` v0.6.1-zed (`:79-80`), `IDENTITY="Zed Industries, Inc."` (`:15`), `APPLE_NOTARIZATION_TEAM="MQ55VZLNZQ"` (`:16`), `zed.keychain`, `crates/zed/contents/<channel>/embedded.provisionprofile` (`:223`), `crates/zed/resources/zed.entitlements`, DMG `Zed-<arch>.dmg` with volume name `Zed` (`:272,284`), `ZED_BUNDLE=true` | B | `script/differ-bundle-mac`, using the same cargo-bundle fork, run in `differ-crates/differ`. Drop the provisioning profile and the associated-domains entitlement. **Needs the user:** an Apple Developer ID certificate and notarization API key (team ID, key ID, issuer). |
| 38 | Linux tarball | `script/bundle-linux`: `zed$suffix.app/{bin/zed,libexec/zed-editor}` (`:186-192`), icons `share/icons/hicolor/*/apps/zed.png` (`:214-217`), `APP_ID=dev.zed.Zed…` and `APP_NAME` (`:221-235`), `envsubst < crates/zed/resources/zed.desktop.in` (`:239`), `zed-linux-<arch>.tar.gz` (`:247`) | B | `script/differ-bundle-linux` plus `differ-crates/differ/resources/differ.desktop.in`. The desktop filename must be `<app_id>.desktop`. Comment, GenericName, and Keywords also say Zed. |
| 39 | Windows installer | `script/bundle-windows.ps1:265-349` (per-channel `AppName`, `AppUserId`, `AppMutex`, `RegValueName`, `AppxFullName` `ZedIndustries.Zed_…_japxn1gcva8rg`), `crates/zed/resources/windows/zed.iss` (publisher "Zed Industries", zed.dev URLs `:5-8`, `Zed.exe`, file associations, URL protocol `:1259-1262`, explorer appx `:1368`), `sign.ps1` (Azure Trusted Signing `ENDPOINT`/`ACCOUNT_NAME`), `zed.sh`, `messages/*.isl`, and `crates/explorer_command_injector` (fixed CLSIDs `:130-134`, "Open with Zed") | B | `script/differ-bundle-windows.ps1` plus `differ.iss`. Defer the Explorer context menu and `auto_update_helper`. **Needs the user:** a Windows code-signing identity. |
| 40 | Flatpak, Snap, FreeBSD, install script | `crates/zed/resources/flatpak/*` and `script/flatpak/bundle-flatpak:15-33` (`dev.zed.Zed…`), `resources/snap/snapcraft.yaml.in`, `script/bundle-freebsd`, `script/install.sh` (downloads from `cloud.zed.dev/releases`, `:86,155`) and `uninstall.sh` | B (defer) | Not in v0.1. |
| 41 | macOS Info.plist extras | `crates/zed/resources/info/{DocumentTypes,Permissions,SupportedPlatforms}.plist`. The permission strings say "An application in Zed wants…" | B | Copy them to `differ-crates/differ/resources/info/` and edit. |

### 2.9 In-app text and UI

| # | Item | Where | Class | Differ action |
|---|---|---|---|---|
| 42 | App menu | `zed/src/zed/app_menus.rs:71-112` (menu "Zed", "About Zed", "Hide Zed", "Quit Zed") | A | Differ's own `cx.set_menus`. |
| 43 | About dialog and version | `zed/src/zed.rs:1532-1560,1721` | A | Differ's own About. |
| 44 | Launch-failure text | `zed/src/main.rs:97,159,179-182` ("Zed failed to launch…", zed.dev/docs/linux) | A | Differ's main. |
| 45 | Welcome page | `workspace/src/welcome.rs:454-456` ("Welcome to Zed") | A | Differ never opens it. It shows the inbox when there's no workspace to restore. |
| 46 | macOS single-instance port and handshake | `zed/src/zed/mac_only_instance.rs:15-53`: TCP `127.0.0.1:43737 + channel*100 + uid`, handshake `"Zed Editor Stable Instance Running"` | A (**must change**) | Copied verbatim, Differ would detect a running Zed of the same channel as itself and exit. Use a different port base and a `"Differ … Instance Running"` handshake. |
| 47 | Linux single-instance socket | `zed/src/zed/open_listener.rs:409-431` `data_dir()/zed-<channel>.sock` | A | This is safe under the custom data dir, but name it `differ-<channel>.sock` anyway. |
| 48 | Initial settings file text | `assets/settings/initial_user_settings.json:1-5` ("// Zed settings", zed.dev/docs). Written by `zed/src/zed.rs:264` (Differ's copy) and `settings/src/settings_store.rs:561` | A / C | Differ's open-settings action uses its own template. The `settings_store` fallback stays Zed's (cosmetic). |
| 49 | Leftover "Zed" UI strings in reused crates | About 436 string-literal lines (by `rg`). Most are in `language_models` (32), `settings_ui` (30), `extensions_ui` (20), `agent_ui` (20), `workspace` (13: `notifications.rs:1278,1381,1399`, `pane_group.rs:424-428`, `workspace.rs:10957`), `title_bar` (6, collab and auto-update) | C | Accept for v0.1. Most are in excluded features, and Differ replaces the title bar. Track them with the CI ratchet (§5). |
| 50 | Theme, font, keymap, and action identifiers | `theme/src/icon_theme.rs:424` "Zed (Default)", `theme/src/fallback_themes.rs:18`, `gpui/src/text_system.rs:1268-1286` (`.ZedSans`, `.ZedMono`), `settings/src/base_keymap_setting.rs:61-89` (`BaseKeymap::Zed`), `zed::` action namespace (`zed_actions`) | keep | These are setting values and keymap IDs. Renaming them breaks user configs. |
| 51 | Env vars and terminal identity | `terminal/src/terminal.rs:656-664` (`TERM_PROGRAM=zed`, `ZED_TERM=true`), task vars (`$ZED_FILE`, `$ZED_WORKTREE_ROOT`…), about 150 `ZED_*` env vars | keep | Shell integrations and users' `tasks.json` depend on them. |
| 52 | Git checkpoint author | `git/src/repository.rs:4090-4096` (`Zed <hi@zed.dev>`, internal checkpoint commits) | keep | Internal. Only used by Zed's agent checkpoints. |
| 53 | macOS pasteboard types | `gpui_macos/src/pasteboard.rs:52-53` (`zed-text-hash`, `zed-metadata`) | keep | Internal. Sharing them keeps copy and paste between Zed and Differ lossless. |

---

## 3. Needs a Zed patch (friction points)

Each patch is small and written to be upstreamable as a "fork hook". Until a patch lands, use the
fallback.

| ID | Problem (rows) | Minimal patch | Fallback without a patch |
|---|---|---|---|
| **P1** | `APP_NAME` is a const, and `temp_dir`/macOS `logs_dir`/`telemetry.log` ignore the custom dir (16, 18) | `crates/paths/src/paths.rs:18`: `pub const APP_NAME: &str = match option_env!("ZED_APP_NAME") { Some(n) => n, None => "Zed" };`. That's one line. `option_env!` is const-compatible, and cargo rebuilds `paths` when the variable changes. Differ sets `ZED_APP_NAME=Differ` in its `cargo --config` `[env]` file. Note the binary-name assert in `zed/src/main.rs:10-16`: building `-p zed` with that env set would fail, so set it only for Differ invocations. | `set_custom_data_dir` (row 17). Accept shared temp files, `<data>/config` as the settings location, and the clobbered `~/Library/Logs/Zed/telemetry.log` on macOS. |
| **P2** | `display_name()`/`app_id()`/`app_identifier()` are hardcoded (1, 3, 6, 24) | `crates/release_channel/src/lib.rs`: add compile-time overrides, the same pattern as P1. `ZED_APP_DISPLAY_NAME` is a prefix: Stable → `"{name}"`, Dev → `"{name} Dev"`. `ZED_APP_ID` → `"{id}"` / `"{id}-Dev"`. `app_identifier()` derived from the display name. Keep the `&'static str` return type by using `const_format::concatcp!` (already a dependency of `paths`). | Avoid the call sites: Differ's own window options and About, the default title format, and excluded agent/collab/copilot notifications. Accept LSP `clientInfo` "Zed". |
| **P3** | `release_channel` reads its channel from a file in `crates/zed` (21) | No patch. The env var already works. | Always export `ZED_RELEASE_CHANNEL` in Differ builds. |
| **P4** | `crates/cli` is hardwired to Zed (34) | Derive binary names (`zed-editor`, `Contents/MacOS/zed`, `zed-<ch>.sock`) from `paths::APP_NAME_LOWERCASE` after P1, and pipe names from `app_identifier()` after P2. About 10 lines, but it touches the high-churn `cli/src/main.rs`. | v0.1 ships no separate CLI. Later, a `differ_cli` copy. |
| **P5** | Cosmetic strings and links (8, 9, 14, 31, 32, 48, 49) | None. Not worth the conflicts. | Accept them and ratchet in CI. |
| **P6** | `crates/zed` has no `[lib]` (§1 caveat) | Add `[lib] path = "src/zed.rs"` and make `main.rs` thin. Unlikely to be accepted upstream, and it's a large diff. | Differ writes a minimal `main.rs`/`init.rs` and copies only the functions it needs, recording provenance (§4). |

**Recommendation:** v0.1 ships with **no patches** (fallbacks only). Open P1 upstream early. It is one
line, and the existing "Forks should change this" comment invites it. P2 can follow. Each patch merged
upstream removes a fallback, and nothing is ever carried as a local diff.

---

## 4. Recommended v0.1 approach

**In `differ-crates/differ` (A):**
1. Start of `main()`: resolve Differ's data dir, honoring `--user-data-dir`, and call `paths::set_custom_data_dir` before anything touches `paths`. Pass the flag back through `Application::with_restart_arguments`.
2. Set up logging with `zlog::init_output_file(<data>/logs/Differ.log, …)`. Install the crash handler only for local backtraces, with no upload.
3. Build the HTTP client with User-Agent `Differ/<ver> (<os>; <arch>)`. Load `AppVersion` from Differ's own `CARGO_PKG_VERSION`. Differ's `build.rs` emits `ZED_COMMIT_SHA`/`ZED_BUILD_ID` and the X11 icon.
4. After `settings::init`, apply a default-settings overlay with `SettingsStore::update_default_settings`: `telemetry.metrics=false`, `telemetry.diagnostics=false`, `auto_update=false`, `server_url` per the row 29 decision, `auto_install_extensions={}`, and `suggest_extensions=false` if the registry is off. Optional first-run step: offer to copy the user's Zed `settings.json`/`keymap.json`/`themes/`, because Differ no longer reads `~/.config/zed`.
5. Write its own `build_window_options` with Differ's app ID. On Windows, call `cx.set_app_identity`. Also write its own menus, About, open listener (`differ://`), and single-instance code (new mac port base and handshake, `differ-<ch>.sock`, `Differ-…` mutex and pipe).
6. Use an overlay `AssetSource`: Differ's `assets/` first, then Zed's `assets::Assets`.
7. Don't initialize: `auto_update`, `auto_update_ui`, `reliability` uploads, `feedback`, `install_cli`/`RegisterZedScheme`, `collab_ui`, `call`, cloud/LLM providers, `edit_prediction`, `copilot`, `onboarding`, `telemetry_log`. Never call `client.sign_in*`.
8. Copied-code provenance: each file or function copied from `crates/zed` gets a header like `// Copied from crates/zed/src/zed.rs@<zed-tag> fn build_window_options`. `differ-sync-zed` should print `git diff <old-tag> <new-tag> -- <those upstream files>` so the agent can port relevant changes. (Hand this to whoever owns the sync tooling.)

**Build config (B):**
- `differ-crates/differ/cargo-differ.toml`, used via `cargo --config differ-crates/differ/cargo-differ.toml …`. It holds `[env] ZED_RELEASE_CHANNEL = "dev"`, with scripts overriding it to `stable`, and `ZED_APP_NAME = "Differ"` once P1 lands. This avoids editing Zed's `.cargo/config.toml`.
- `differ-crates/differ/Cargo.toml` `[package.metadata.bundle-stable]` / `bundle-dev` with identifier `io.github.msetaro.Differ[.Dev]`, name `Differ[ Dev]`, `osx_url_schemes=["differ"]`, Differ icons, and `osx_info_plist_exts=["resources/info/*"]`.
- `differ-crates/differ/resources/`: icons (png, @2x, icns, ico), `differ.desktop.in` (`MimeType=…;x-scheme-handler/differ;`), `info/*.plist`, `differ.entitlements`, `windows/differ.iss`.

**Scripts Differ owns:** `script/differ-bundle-mac`, `script/differ-bundle-linux`, and `script/differ-bundle-windows.ps1`. Copy them from Zed's versions, build `-p differ`, and drop Sentry, remote_server, the Explorer injector, the auto-update helper, and provisioning profiles. Zed's own `script/bundle-*` stay untouched.

**Deferred:** a separate CLI (P4), Flatpak/Snap/FreeBSD and the install script, the Windows Explorer menu, SSH/WSL remoting, and MCP OAuth identity.

**Needs the user:** confirm the identifiers (name, app ID, URL scheme), provide icon artwork, provide an Apple Developer ID certificate and notarization API key, provide a Windows code-signing identity, and decide on the extension registry (row 29).

---

## 5. Suggested CI checks (brief)

1. **Source check (hard fail):** `rg -n '\bZed\b|zed\.dev|dev\.zed\.' differ-crates/ script/differ-* --glob '!**/PROVENANCE*'`. Fail unless the match is on an allowlist (`differ-docs/rebrand-allowlist.txt`, with copied-code provenance headers allowed).
2. **Binary ratchet (soft):** `strings target/release/differ | rg -c '\bZed\b'` and `… | rg -c 'zed\.dev'`. Store the counts in `differ-docs/rebrand-baseline.txt` and fail if they go up. This catches newly reachable strings that upstream merges bring in.
3. **No zed.dev traffic (hard fail):** launch Differ headless for N seconds with a throwaway `--user-data-dir` whose `settings.json` sets `"proxy": "http://127.0.0.1:<port>"`, pointing at a tiny logging proxy. Zed's HTTP client honors `proxy` (`default.json:2950`). Fail on any `CONNECT *.zed.dev`. For a belt-and-braces check on Linux CI, add `--add-host` entries for `zed.dev`, `api.zed.dev`, and `cloud.zed.dev` pointing at `127.0.0.1` in a container, with a listener that records connections. If the row 29 decision keeps the registry, allowlist `api.zed.dev/extensions*` only.
4. **Bundle metadata check:** after `differ-bundle-*`, assert `CFBundleIdentifier`, the `.desktop` filename and `StartupWMClass`, and the `.iss` `AppUserModelID`/`AppMutex` all equal the identifiers in one `differ-crates/differ/branding.toml`. Scripts should read that file rather than hardcode values.

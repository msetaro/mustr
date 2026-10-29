# mustr on macOS

How to install a mustr release on an Apple silicon Mac, how to build and run mustr from a
checkout, what it does on the first run, and what hasn't been checked on a real Mac yet.

## Install a release

In Terminal:

```sh
curl -fsSL https://github.com/msetaro/mustr/releases/latest/download/install.sh | sh
```

- It needs an Apple silicon Mac, and nothing else: no Rust, Xcode, or Homebrew.
- It downloads the latest release's `manifest.json` and zip, checks the zip's size and SHA-256,
  unzips it with `ditto`, checks that the app is mustr (bundle ID `dev.mustr.mustr`), and installs
  `/Applications/mustr.app`. If `/Applications` isn't writable for your account, it uses
  `~/Applications` and says so. It never uses `sudo`.
- If mustr is running, it asks before quitting it.
- **Why there's no Gatekeeper warning:** mustr isn't signed with an Apple Developer ID yet, only ad
  hoc. macOS checks apps that carry the quarantine attribute, which browsers set on what they
  download; curl doesn't set it. A zip downloaded in a browser is quarantined, and macOS refuses
  to open the app in it, so install with the command above.
- `MUSTR_VERSION=0.3.0-beta.1` (after `|`, before `sh`) installs that release instead, prereleases
  too. `MUSTR_INSTALL_DIR=<dir>` installs `<dir>/mustr.app`.

**Updates.** mustr checks for a newer release 20 seconds after it starts and every 4 hours. It
downloads it into `~/Library/Application Support/mustr/updates/`, checks it (the manifest's
Ed25519 signature against the key mustr is built with, the zip's size and SHA-256, the app's
bundle ID, version, and code signature), and swaps it in for `mustr.app`; the app you're using
keeps working. Then the status bar shows **Restart to update**. mustr never restarts by itself.

- mustr › Check for Updates… (or "mustr: check for updates" in the command palette) checks now,
  and says what it found.
- `"updates": {"enabled": false}` in the settings file stops the automatic checks; Check for
  Updates… still works.
- The replaced app waits next to the new one as `.mustr.app.previous` (hidden) until mustr next
  starts. To go back by hand before then: quit mustr, then in that folder
  `rm -rf mustr.app && mv .mustr.app.previous mustr.app`.
- Only the release builds `script/mustr-package-mac` makes, run from an app bundle, update
  themselves. `cargo run` and `script/mustr-bundle-mac` builds (`--release` too) log
  `Updates: off, because …` and don't, so a local build never turns into the published release.
- Cutting releases is in `mustr-docs/RELEASING.md`.

## First run on a new Mac

A checklist for the first launch of a release, such as on a work laptop.

1. **Install** in Terminal, then open mustr from Applications or Spotlight:
   `curl -fsSL https://github.com/msetaro/mustr/releases/latest/download/install.sh | sh`
2. **With T3 Code installed** (its `t3` command and background service): within a few seconds
   Home says "Connected to <your Mac's name>", and the title bar's chip shows `T3 · <name>`.
   - mustr pairs by itself: it runs `t3 auth pairing create` for `orchestration:read`,
     `orchestration:operate`, `source-control:write`, `filesystem:read`, `terminal:read`,
     `terminal:operate`, `settings:write`, `providers:manage` and `diagnostics:read`, and the
     session shows as "mustr" in `t3 auth session list`. It never asks for `access:*` or `relay:*`.
   - **Keychain:** the token goes in your login keychain as `mustr-t3:<server URL>`. Saving it
     doesn't prompt; the next launch does: "mustr wants to use your confidential information…".
     Enter your login password and choose **Always Allow**. The app is signed ad hoc, so macOS
     probably asks again after each update (not yet seen on a Mac).
   - **Pairing again** happens by itself when the token expires (after about 30 days), when T3
     revokes it, or when a newer mustr needs another scope. To start over, revoke the "mustr"
     session (`t3 auth session revoke <id>`), then choose Reconnect on Home.
   - If T3's service is installed but stopped, mustr runs `t3 service restart` once. If nothing
     runs T3, Home offers **Run T3 Code in the background** (`t3 service install`). If T3 Code
     speaks an older protocol, Home offers **Update T3 Code** (`t3 update`); an older 0.1.x works
     and Home says quietly that 0.1.3 is available.
3. **With no T3 Code:** Home shows "Set up the agents engine" with **Install the agents engine**
   (T3 Code 0.1.3, about 100 MB, installed as T3's own installer does: `~/.t3`, the `t3` command
   in `~/.local/bin`, and its launch agent), or **Use mustr's own engine**: a private T3 Code in
   mustr's data folder, with its own projects and threads, that runs only while mustr does.
4. **With only T3's desktop app** (no `t3` command): while T3's app is open, mustr finds the
   server it runs but can't pair by itself, so Home shows "Pair mustr with T3 Code" and a field.
   In T3's app, open Settings › Sharing & access, create a pairing link with the scopes above,
   and paste it into the field. mustr then works while T3's app is open. To stop depending on it,
   quit T3's app and choose Install the agents engine on Home (T3's app can then use that
   background service too), or use mustr's own engine.
5. **Check the Agents page** (the rail's grid icon, or "mustr: open agents" in the command
   palette, ⌘⇧P): Claude and Codex should say **Ready**, with "2 ready" in the heading. If one is
   signed out, sign in there, or in Terminal with `claude auth login` or `codex login`, then choose
   Check again. The agents' logins are their own CLIs', shared with T3 Code.
6. **Restricted Mode:** the first review of an agent's worktree may show **Restricted Mode** (a
   warning icon in a narrow window): Zed keeps language servers and project settings off in
   folders you haven't trusted. Click it and choose **Trust Project**. That trusts the project's
   checkout, and its agent worktrees follow: the ones open now, and the others whenever they
   open, with no more asking. A project you trusted before covers its worktrees from the start.
   Untrusted projects still ask, and only the worktrees T3 lists for the project's threads on this
   Mac inherit; **Trust Only This Worktree** trusts just the one.
7. **Zoom:** ⌘= and ⌘- make everything bigger or smaller, and ⌘0 goes back to the default size.
   mustr starts at twice the size its designs are drawn at; try ⌘- once or twice on a laptop.
8. **Settings and logs** are in `~/Library/Application Support/mustr/`: `config/settings.json`
   (⌘, opens it), `logs/mustr.log` (with `logs/mustr.log.old` before it), and
   `logs/t3-engine.log` for mustr's own engine. T3 Code's own log is where `t3 service status`
   says.
9. **Reporting a problem:** say what you did and what you saw (a screenshot helps), and send:
   - the version and server line: the `starting mustr` line and the `Connected to T3
     environment …` line from `mustr.log`;
   - the warnings and errors around the time it happened:
     `grep -E "starting mustr|Connected to T3|ERROR|WARN|panic" ~/Library/Application\ Support/mustr/logs/mustr.log | tail -100`;
   - for a crash, the `panic:` lines with the backtrace under them in `mustr.log`, or the report in
     Console › Crash Reports (`~/Library/Logs/DiagnosticReports/mustr-*.ips`);
   - for an engine problem in mustr's own engine, the end of `t3-engine.log`.

   mustr doesn't write tokens or pairing credentials to its log; still, look it over for
   repository and file names you'd rather not share. Right after launch, a few `… mustr isn't connected to T3` warnings are normal:
   views restored from last time load before the connection is up, then load again.

## Prerequisites

- **An Apple silicon Mac.** mustr builds on an Intel Mac too, but T3 Code publishes no engine for
  Intel Macs, so there mustr can only connect to a T3 Code server on another machine (external
  mode).
- **Apple's Command Line Tools:** `xcode-select --install`. Full Xcode isn't needed. mustr turns on
  GPUI's `runtime_shaders` feature (the `runtime_shaders` feature of `mustr-crates/mustr`, on by
  default), so the build never runs Xcode's `metal` compiler: macOS compiles the shaders when mustr
  starts. Full Xcode works too.
- **Rust**, through [rustup](https://rustup.rs):
  `curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | sh -s -- -y`, then
  `. "$HOME/.cargo/env"` (new terminals pick it up on their own). `rust-toolchain.toml` pins the
  toolchain (1.98.1), and the first `cargo` command installs it, with the extra targets the file
  lists.
- **CMake:** `brew install cmake`, or CMake.app from [cmake.org](https://cmake.org/download/) in
  Applications (`script/mustr-bundle-mac` finds it there; for `cargo run`, add
  `/Applications/CMake.app/Contents/bin` to `PATH`). Zed's `language` crate turns on tree-sitter's WebAssembly grammar support, and its
  runtime's build script (`wasmtime-c-api-impl`) runs `cmake`. That's in Zed's crates, so mustr
  can't turn it off.
- **Not needed:** `protoc`. The other C dependencies (SQLite, zstd, tree-sitter, aws-lc) build
  with the Command Line Tools' `clang`, and Zed's protobuf code is generated in Rust (`protox`).
- **Disk:** a debug build compiles about 1,000 crates, and `target/` grows to about 20 GB.

## Build and run

From the repository root (debug builds load assets from the checkout):

```sh
cargo run -p mustr
```

- The first build takes a while. Later builds are incremental.
- **As an app,** for daily use: `script/mustr-bundle-mac --open`. It builds mustr, wraps it in
  `target/mustr-debug/mustr.app` with an `Info.plist` and mustr's icon, signs it ad hoc for this
  Mac, and opens it. Only a bundle gets system notifications (GPUI needs a bundle identifier), the
  `mustr://` URL scheme, and mustr's name in the Dock and the ⌘-Tab switcher. Add `--release` for a
  faster app. Keep the bundle where the script puts it: debug builds find their assets by walking up
  from the executable to the checkout.
- **Release build:** `cargo run -p mustr --release`.
- **Precompiled shaders,** as Zed ships them: `cargo run -p mustr --no-default-features`. This
  needs full Xcode with its Metal toolchain. On Xcode 26 and later, also run
  `xcodebuild -downloadComponent MetalToolchain` once.
- **Tests:** `cargo test -p <crate>`, one crate at a time.
- **Gatekeeper delay:** macOS checks each new build before it runs. To skip that, add your terminal
  under System Settings › Privacy & Security › Developer Tools (Zed's
  `docs/src/development/macos.md`, "Speeding up verification").

## What to expect

- **Dock and menu bar.** `cargo run` starts a bare executable, not an app bundle, so mustr sets its
  Dock icon itself. The menu bar has:
  - **mustr:** About mustr, Check for Updates…, Settings… (⌘,), Key Bindings…, Select Theme…,
    Select Icon Theme…, Services, Hide mustr (⌘H), Hide Others (⌥⌘H), Show All, and Quit mustr
    (⌘Q).
  - **Edit:** Undo, Redo, Cut, Copy, Paste, Select All, and Find.
  - **View:** Zoom In (⌘+), Zoom Out (⌘-), and Actual Size (⌘0).
  - **Window:** Minimize (⌘M), Zoom, and the open windows.
- **Title bar.** The traffic lights sit at the left, vertically centered in the bar, and mustr's
  tabs start after them. The bar grows when you zoom in, and the traffic lights, which macOS draws
  at a fixed size, move down to stay centered. Drag an empty part of the bar to move the window.
  Double-clicking it does what System Settings › Desktop & Dock › "Double-click a window's title
  bar to" says. mustr draws no window buttons of its own on macOS.
- **Keys.** mustr loads Zed's macOS keymap (`assets/keymaps/default-macos.json`), so shortcuts use
  ⌘: ⌘⇧P opens the command palette, and ⌘, opens the settings file. In a thread's composer, ↩ or
  ⌘↩ sends, ⇧↩ adds a new line, and ⌘⇧↩ steers the running agent. In a review, `c` comments on
  the selected lines, ⌘↩ adds the comment, and ⎋ cancels it.
- **Zoom.** ⌘= (or ⌘+) and ⌘- make all of mustr bigger or smaller, about 10% a step, the way a
  browser zooms a page: text, code, and layout together. ⌘0 goes back to mustr's default size,
  `ui_font_size` and `buffer_font_size` 26, twice the size its designs are drawn at.
  They work wherever the focus is. A zoom changes `ui_font_size` and `buffer_font_size` in your
  settings file, by the same factor, so it lasts; ⌘0 removes both. The command palette has them
  too: "mustr: zoom in", "mustr: zoom out", and "mustr: reset zoom".
- **Closing the last window** keeps mustr running, as other Mac apps do. Click the Dock icon to
  open a window again. To quit instead, set `"on_last_window_closed": "quit_app"` in the settings.
- **Files.** Everything is in `~/Library/Application Support/mustr/` (`--user-data-dir <dir>`
  moves it):
  - `config/settings.json` and `config/keymap.json`.
  - `db/`, the database.
  - `logs/mustr.log`, and `logs/t3-engine.log` for mustr's own engine.
  - `t3-runtime/` and `t3-home/`, mustr's own engine, if you use it.
  - `updates/`, the update being downloaded and checked; it's emptied once it's installed.

  mustr doesn't read or write Zed's settings or data.

## First run with your T3 Code

mustr's default engine mode, `installed`, uses your own T3 Code: its data in `~/.t3` (or
`$T3CODE_HOME`), its server, and the agents it runs with their own logins. mustr shows the same
threads as T3 Code's own app.

1. **Finding the server.** mustr reads `~/.t3/userdata/server-runtime.json`, which T3's server
   writes while it runs. T3's launch agent (`com.t3tools.t3code.service`) keeps that server
   running.
   - If T3's launch agent is installed but stopped, mustr runs `t3 service restart` once.
   - If nothing runs, Home offers to set up T3's launch agent (`t3 service install`), or to use
     mustr's own engine.
   - If T3 Code isn't installed, Home offers to install it the way T3's `install.sh` does: the
     runtime in `~/.t3/runtime/versions/`, the `t3` command in `~/.local/bin`, and the launch
     agent.
2. **Version check.** mustr speaks T3's orchestration protocol 2 and is built for T3 Code 0.1.3
   (`mustr-crates/t3_client/T3_VERSION`). Other T3 Code versions on protocol 2 work; for an older
   one, Home says quietly that 0.1.3 is available. If your T3 Code speaks an older protocol, Home
   offers `t3 update`. If it speaks a newer one, mustr needs updating.
3. **Pairing.** mustr finds T3's `t3` command and asks it for a one-time credential:
   `t3 auth pairing create --scope orchestration:read --scope orchestration:operate --scope
   source-control:write --scope filesystem:read --scope terminal:read --scope terminal:operate
   --scope settings:write --scope providers:manage --scope diagnostics:read --ttl 5m --label mustr
   --json`. A token from an older mustr that lacks one of these is replaced by pairing again the
   same way.
   - It looks for the runtime that matches the server's version in `~/.t3/runtime/versions/`
     first, then `t3` on your `PATH`, then the newest runtime.
   - It exchanges the credential for a token, which lasts about 30 days. The session shows as
     "mustr" in `t3 auth session list`.
   - If there's no `t3` command (for example, with only T3 Code's desktop app installed), Home
     shows that command. Run it where T3's CLI is installed, then paste the credential or the
     pairing link it prints.
4. **Keychain.** mustr stores the token in your login keychain as an internet password named
   `mustr-t3:<server URL>`, with account `mustr`.
   - Creating the item doesn't prompt.
   - Using it does, once each time the mustr binary changes, so after each rebuild: "mustr wants
     to use your confidential information stored in … in your keychain". Enter your login password
     and choose **Always Allow**. Debug builds aren't code-signed, so macOS can't tell that a new
     build is the same app.
   - To avoid the prompts while developing, set `MUSTR_CREDENTIALS_FILE=<path>`. mustr then keeps
     tokens in that JSON file, protected only by its file permissions (0600).

To try mustr without touching your T3 Code, use its own engine:
`cargo run -p mustr -- --t3-mode embedded`. It downloads T3 Code 0.1.3 (about 100 MB) into mustr's
data directory and runs it on a free loopback port, never on 3773.

## Known gaps

mustr's macOS support was written on Linux and type-checked there for `aarch64-apple-darwin` with
`script/mustr-check-macos` (`cargo clippy --all-targets`, warnings denied, for every mustr crate).
On real Macs so far: the user built an early `m3/next` from source and ran it on an Apple silicon
Mac (it needed Rust and CMake, now in Prerequisites), and the release workflow has built, linked,
signed and packaged the app on GitHub's `macos-15` runners. Most of what follows hasn't been
looked at on a Mac yet.

- **Not checked on a Mac:** the Dock icon, the About panel, the menu bar, the title bar's layout
  around the traffic lights and their moving when you zoom, the zoom keys (only their bindings are
  tested, on Linux), keychain prompts, and mustr's tests on macOS. Nor are the installer (its logic
  ran on Linux with stand-ins for `ditto`, `plutil`, and `osascript`) and the updater's `ditto`,
  `codesign`, and relaunch steps (its download, checks, and bundle swap ran in tests with stand-in
  bundles). The launch agent and `t3` command handling was reviewed against T3's
  CLI (`mustr-docs/T3-RUNTIME.md`), not run.
- **What the cross check can't see.** It runs without the macOS SDK: C code from build scripts
  goes to a stand-in compiler, and GPUI's shader outputs are stubbed. So C-level build failures
  (missing headers, SDK versions) and link errors can only show up on a Mac.
- **`cargo run` has no app bundle.** It has no `Info.plist`, so there's no `mustr://` URL scheme and
  GPUI's system notifications are off (they need a bundle identifier); mustr shows in-app toasts
  only. `script/mustr-bundle-mac` fixes that for this Mac, and releases install a bundle.
- **Releases are signed ad hoc,** not with an Apple Developer ID, and aren't notarized. They
  install with curl (above); device management that allows only identified developers would block
  them. `mustr-docs/RELEASING.md`, "Adding a Developer ID", has the steps.
- **System window tabs.** With Zed's `use_system_window_tabs` setting on, mustr shows Zed's own
  title bar, which draws those tabs, and it grows less than the rest of mustr when you zoom in.
- **Intel Macs** have no T3 engine (see Prerequisites).

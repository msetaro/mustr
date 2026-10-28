# mustr on macOS

How to build and run mustr from a checkout on an Apple silicon Mac, what it does there on the
first run, and what hasn't been checked on a real Mac yet.

## Prerequisites

- **An Apple silicon Mac.** mustr builds on an Intel Mac too, but T3 Code publishes no engine for
  Intel Macs, so there mustr can only connect to a T3 Code server on another machine (external
  mode).
- **Apple's Command Line Tools:** `xcode-select --install`. Full Xcode isn't needed. mustr turns on
  GPUI's `runtime_shaders` feature (the `runtime_shaders` feature of `mustr-crates/mustr`, on by
  default), so the build never runs Xcode's `metal` compiler: macOS compiles the shaders when mustr
  starts. Full Xcode works too.
- **Rust**, through [rustup](https://rustup.rs). `rust-toolchain.toml` pins the toolchain (1.98.1),
  and the first `cargo` command installs it, with the extra targets the file lists.
- **Not needed:** CMake, Homebrew, or `protoc`. The C dependencies (SQLite, zstd, tree-sitter,
  aws-lc) build with the Command Line Tools' `clang`, and Zed's protobuf code is generated in Rust.
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
  - **mustr:** About mustr, Settings… (⌘,), Key Bindings…, Select Theme…, Select Icon Theme…,
    Services, Hide mustr (⌘H), Hide Others (⌥⌘H), Show All, and Quit mustr (⌘Q).
  - **Edit:** Undo, Redo, Cut, Copy, Paste, Select All, and Find.
  - **Window:** Minimize (⌘M), Zoom, and the open windows.
- **Title bar.** The traffic lights sit at the left, and mustr's tabs start after them. Drag an
  empty part of the bar to move the window. Double-clicking it does what System Settings ›
  Desktop & Dock › "Double-click a window's title bar to" says. mustr draws no window buttons of
  its own on macOS.
- **Keys.** mustr loads Zed's macOS keymap (`assets/keymaps/default-macos.json`), so shortcuts use
  ⌘: ⌘⇧P opens the command palette, and ⌘, opens the settings file. In a thread's composer, ↩ or
  ⌘↩ sends, ⇧↩ adds a new line, and ⌘⇧↩ steers the running agent. In a review, `c` comments on
  the selected lines, ⌘↩ adds the comment, and ⎋ cancels it.
- **Closing the last window** keeps mustr running, as other Mac apps do. Click the Dock icon to
  open a window again. To quit instead, set `"on_last_window_closed": "quit_app"` in the settings.
- **Files.** Everything is in `~/Library/Application Support/mustr/` (`--user-data-dir <dir>`
  moves it):
  - `config/settings.json` and `config/keymap.json`.
  - `db/`, the database.
  - `logs/mustr.log`, and `logs/t3-engine.log` for mustr's own engine.
  - `t3-runtime/` and `t3-home/`, mustr's own engine, if you use it.

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
2. **Version check.** mustr speaks T3's orchestration protocol 2 (T3 Code 0.1.1,
   `mustr-crates/t3_client/T3_VERSION`). If your T3 Code speaks an older protocol, Home offers
   `t3 update`. If it speaks a newer one, mustr needs updating.
3. **Pairing.** mustr finds T3's `t3` command and asks it for a one-time credential:
   `t3 auth pairing create --scope orchestration:read --scope orchestration:operate --scope
   source-control:write --ttl 5m --label mustr --json`.
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
`cargo run -p mustr -- --t3-mode embedded`. It downloads T3 Code 0.1.1 (about 100 MB) into mustr's
data directory and runs it on a free loopback port, never on 3773.

## Known gaps

mustr's macOS support was written on Linux and type-checked there for `aarch64-apple-darwin` with
`script/mustr-check-macos` (`cargo clippy --all-targets`, warnings denied, for every mustr crate).
Nothing has run on a Mac yet.

- **Not checked on a Mac:** linking, the runtime-compiled Metal shaders, the Dock icon, the About
  panel, the menu bar, the title bar's layout around the traffic lights, keychain prompts, and
  mustr's tests on macOS. The launch agent and `t3` command handling was reviewed against T3's
  CLI (`mustr-docs/T3-RUNTIME.md`), not run.
- **What the cross check can't see.** It runs without the macOS SDK: C code from build scripts
  goes to a stand-in compiler, and GPUI's shader outputs are stubbed. So C-level build failures
  (missing headers, SDK versions) and link errors can only show up on a Mac.
- **`cargo run` has no app bundle.** It has no `Info.plist`, so there's no `mustr://` URL scheme and
  GPUI's system notifications are off (they need a bundle identifier); mustr shows in-app toasts
  only. `script/mustr-bundle-mac` fixes that for this Mac. Packaging for other Macs (a Developer ID
  signed, notarized `.app` and `.dmg`) is M3 work (`mustr-docs/PLAN.md`).
- **Font size shortcuts do nothing** (⌘=, ⌘-, ⌘0), on every platform. Zed's keymap binds them,
  but their handlers are in Zed's `crates/zed`, which mustr doesn't include.
- **Intel Macs** have no T3 engine (see Prerequisites).

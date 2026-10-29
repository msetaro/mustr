# mustr: Plan

> **mustr** is a desktop app where you review every change, from people's pull requests and from your own coding agents, with a full editor underneath. It's a GPUI app built as a soft fork of [Zed](https://github.com/zed-industries/zed), and a client of an unmodified [T3 Code](https://github.com/pingdotgg/t3code) server. It isn't affiliated with Zed Industries or with T3 Code's authors.
> Scope: v0.1 in about 3 months for one developer. GitHub first; T3's server also supports GitLab, Bitbucket, Forgejo, and Azure DevOps. Free and open source (GPL-3.0-or-later). No project website (§16).

---

## 1. Vision and principles

**Goal:** one app to write code with agents and review every change, whether a person or an agent wrote it. Review is the bottleneck, so review is the center.

1. **Reviewer first, with agents and an IDE underneath.** The inbox is the home screen. It holds every change waiting on you: PRs from people and agent threads with changes ready to review. Agents and IDE tools (go to definition, references, search, full files, a terminal) are one keystroke away and always lead back to the review.
2. **Easy to keep current.** mustr sits on two fast-moving upstreams, Zed and T3 Code, and has one maintainer. Pulling either one in must be a routine task an agent can run (§3). A feature that would make syncing harder waits, or goes upstream.
3. **Build only the UI.** T3 Code's server already runs agents and talks to forges, and Zed is already an editor. mustr's own code is the review-first interface on top of them.
4. **Keyboard-first, mouse-friendly.** Every action can be done from the keyboard and every action can be clicked.
5. **Local-first.** Drafts are never lost. Data you've seen opens from cache. Bad Wi-Fi degrades gracefully.
6. **Great defaults.** Customization is opt-in. Zed's settings, keymaps, and themes work as usual.
7. **Trustworthy.** No telemetry. All PR and agent content is treated as hostile input.

### Non-goals for v0.1
- **No changes to Zed's crates** (§3).
- **No mustr-owned agent runtime or forge client.** T3's server is the backend (§2).
- **Not a git client.** Zed's git panel comes along with the fork. Hiding it by default is recommended (§18).
- **No hosted AI.** Zed's hosted models, edit predictions, and Copilot are left out. Agents run on the user's own CLIs through T3.
- No collaboration features, mobile app, or web build. T3's own mobile app keeps working against the same server.

---

## 2. Architecture

| Part | Owns | Language |
|---|---|---|
| **mustr app** (soft fork of Zed) | Inbox, review view, comment cards and composer, thread chat, PR overview, the look, T3 runtime management and pairing | Rust (GPUI) |
| **Zed's crates**, unmodified | Editor and multibuffers, IntelliSense (LSP), tree-sitter, search, terminal, vim, settings, keymaps, themes, extensions | Rust |
| **T3 Code server**, unmodified | Agent threads and turns, approvals, worktrees, terminals, agent CLI install and auth, PR and review data, forge credentials | TypeScript, in T3's runtime |

```
┌───────────────────────── mustr app (GPUI, soft fork of Zed) ──────────────────────────┐
│ mustr-crates/                                                                         │
│   mustr        app binary + init: rebrand; leaves out Zed's collab, hosted AI,        │
│                 Copilot, onboarding, telemetry upload; T3 runtime + pairing            │
│   mustr_ui     mustr's components and look (§6)                                      │
│   inbox         one inbox: PRs and agent threads waiting on you                        │
│   review        review multibuffer, comment cards, composer, submit, Send to agent     │
│   thread_chat   agent thread timeline, composer, approvals, queue and steer            │
│   t3_client     types generated from T3's contracts + RPC/WebSocket transport          │
│                                                                                        │
│ Zed's crates, unmodified: gpui · workspace · editor · multi_buffer · buffer_diff ·     │
│ project · lsp · language · git_ui · terminal_view · vim · settings · theme · markdown  │
└───────────────────────────────────────────┬────────────────────────────────────────────┘
                                            │ HTTP + WebSocket, paired device credential
┌─── T3 Code server (unmodified, pinned version) ────────────────────────────────────────┐
│ orchestration: threads, turns, diffs, approvals                                        │
│ pullRequests: list, detail, diffs, review threads, viewed files, submit review         │
│ vcs worktrees · terminals · shell.openInEditor · agent CLI install and auth            │
└───────────────┬─────────────────────────────────────────────────────┬──────────────────┘
                │ spawns                                              │ HTTPS
      agent CLIs: claude, codex, cursor,                    forge APIs: GitHub, GitLab,
      opencode, any ACP agent                               Bitbucket, Forgejo, Azure DevOps
```

### Why this shape
- **T3's server already does the backend work.** T3's desktop, web, and mobile apps are all clients of the same server over HTTP and WebSocket, so a fourth client is a supported shape. Its RPC API covers what mustr needs (§4), across GitHub, GitLab, Bitbucket, Forgejo, and Azure DevOps.
- **Zed is already the editor.** Multibuffers make every hunk of a review a live editor with IntelliSense, and Zed's workspace lets an app replace its title bar, status bar, panels, and items without patching anything.
- **Rust stays where performance matters:** the UI, the editor, and IntelliSense. The agent and forge layers mostly wait on CLIs and network calls, so TypeScript costs little there.

**Rejected**
- **Porting T3's agent layer to Rust.** Every T3 release would become a translation job instead of a version bump, and mustr would own every agent CLI's quirks.
- **Native Rust agent adapters or a mustr-owned GitHub client.** Same problem: they duplicate code T3 maintains, including its multi-forge support.
- **Patching Zed's `ui` crate, icons, or hover popover in v0.1.** Each patch is a future merge conflict (§6 covers what that means for the look).
- **A VS Code fork:** over 200 MB of TypeScript, monthly releases, a workbench built around editing, and no Microsoft marketplace.
- **GPUI from scratch or Tauri:** the whole IDE layer would have to be built by hand.

**Costs**
- A second process: T3's runtime is a 167 MB executable on Linux, plus a `client/` directory and `node_modules`.
- T3's RPC API isn't documented as public (§17).
- Two upstreams to track. Zed is large (251 crates, about 64 MB of Rust) and fast (about 220 commits in two weeks), and Zed's core crates depend on its `client`, `telemetry`, and `rpc` crates, so those stay compiled in and unused.
- GPL-3.0-or-later, inherited from Zed's editor and core crates.

---

## 3. Staying in sync with Zed and T3 Code

The rule: mustr's code lives in its own directories, and every upstream pull is a scripted task with a runbook an agent can follow.

**Touch points in Zed's tree** (the only places a Zed merge can conflict):

| File | mustr's change |
|---|---|
| Root `Cargo.toml` | One line adding `mustr-crates/*` to the workspace members |
| `README.md` | mustr's README |
| Everything else mustr adds | New paths upstream never touches: `mustr-crates/`, `mustr-docs/`, `assets/themes/mustr/`, `script/mustr-*`, `.github/workflows/mustr-*.yml` |

- Zed's crates, including `crates/zed`, stay in the tree unchanged. mustr's builds use `mustr-crates/mustr` instead of `crates/zed`.
- Zed's GitHub workflows stay in the tree but are disabled in the repo's Actions settings, not deleted, so they never cause merge conflicts. The sync script disables any new upstream workflow.

**`script/mustr-sync-zed`**
1. Fetch upstream and find the latest Zed stable tag.
2. Merge it into a sync branch (merge, not rebase, so contributors' branches stay valid).
3. Report any conflicts. Only the touch points above should ever conflict.
4. Build `mustr`, run mustr's tests, and run a launch smoke test.
5. Disable any new upstream workflow, then open a PR summarizing upstream changes to the crates mustr uses.

**`script/mustr-sync-t3`**
1. Bump the pinned T3 version (`mustr-crates/t3_client/T3_VERSION`).
2. Fetch that tag's `packages/contracts` and regenerate `t3_client`'s types (§4).
3. Build. The compiler points at every call site a contract change affects.
4. Run the contract tests against that runtime version.
5. Update the pinned runtime checksums, then open a PR.

**`mustr-docs/SYNC.md`** is the runbook, written for an agent: the steps, the expected conflicts and how to resolve each, and when to stop and ask.

**Contract tests:** record-and-replay fixtures for every T3 RPC mustr calls. CI runs them against the pinned runtime, and a nightly job runs them against T3's latest release to warn early. A nightly job also builds mustr against Zed's `main`.

**Cadence and budget:** Zed with each stable release (weekly), T3 when it releases. About an hour or two a week, mostly agent time.

---

## 4. The T3 server connection

### Getting a server
mustr runs on T3 Code's engine, unmodified. The setting `"t3": {"mode": …}` picks which one (`mustr-crates/mustr/src/t3_engine.rs`); the runtime work is in `mustr-crates/t3_runtime`.

- **`"installed"`, the default: the user's own T3 Code.** Its projects and threads appear in mustr and stay shared with T3's other apps.
  - mustr finds T3's home (`$T3CODE_HOME` or `~/.t3`) and the server running from it (`userdata/server-runtime.json`, pid alive, descriptor answers).
  - If T3's background service is installed but stopped, mustr starts it once with T3's own `t3 service restart`.
  - If T3 Code is installed but nothing runs it, the home view offers **Run T3 Code in the background** (`t3 service install`).
  - If T3 Code isn't installed, the home view offers **Install the agents engine**. mustr does what `install.sh` does:
    - downloads the pinned release into `~/.t3/runtime/versions/<V>`, verified against the release's `SHA256SUMS`;
    - links `~/.local/bin/t3` and lists it in `runtime/launchers`;
    - sets up the service with `t3 service install`.
  - mustr never installs, starts, or updates anything there without the user's click, except for restarting the user's already-installed service.
- **`"embedded"`: mustr's own hidden copy of the pinned release,** also the fallback when the installed one can't be used ("Use mustr's own engine").
  - Its files are separate from the user's T3 Code:
    - the runtime is in `<data dir>/t3-runtime/<V>` (the previous version is kept for rollback);
    - the engine's home is `<data dir>/t3-home`;
    - its log is `<data dir>/logs/t3-engine.log`.
  - It listens on a free loopback port that is never 3773, holds `<data dir>/t3-engine.lock`, and is started like T3's desktop app starts its backend: `t3 --mode desktop --bootstrap-fd 0 …`.
  - It is restarted with backoff (0.5 s doubling to 10 s; mustr gives up after six crashes in a row).
  - It is stopped when mustr quits: SIGTERM, then a kill after 5 s. In desktop mode it also exits on its own if mustr dies.
  - It never touches `~/.t3`, the user's service, or port 3773.
- **`"external"`:** a T3 server at a URL (`"t3": {"url": …}` or `--t3-server`), paired by pasting a credential. For development and T3 servers on other machines.
- **Version check:** T3 requires the client's orchestration protocol version to match the server's exactly (`ORCHESTRATION_PROTOCOL_VERSION`, currently 2; it has changed once). A mismatch returns HTTP 426. mustr reads the server's version from `/.well-known/t3/environment` before connecting. If the installed T3 Code doesn't match, mustr explains why. If T3 Code is older, it offers **Update T3 Code** (`t3 update <V> --yes`). In either case it offers mustr's own engine.
- **Platforms:** T3 publishes engine archives for macOS on Apple silicon, Linux x64/arm64, and Windows x64/arm64. There is none for Intel Macs; there, mustr says so and offers the external mode. T3's service exists only with systemd and launchd.

### Pairing
- **Installed and external modes** use the same path as T3's mobile app. A one-time pairing credential (from `t3 auth pairing create --json` run with the installed runtime, or a pasted pairing link) is exchanged at `POST /oauth/token` (OAuth token exchange) for a bearer token that lasts about 30 days.
- **The embedded engine needs no pairing.** mustr hands it a random secret in its bootstrap envelope. It then exchanges the token the secret derives (HMAC-SHA256 over 12-hour windows, as T3's desktop app does) at the same endpoint, with the `environment-bootstrap` grant. Nothing is stored; each launch signs in again.
- mustr asks only for the scopes it needs: `orchestration:read`, `orchestration:operate`, `source-control:write`, `filesystem:read` (the composer's `@` on threads whose folder is on T3's machine), `terminal:read`/`terminal:operate` (T3's terminals), `settings:write` (T3's settings), and `providers:manage` (installing and signing in to agents). Never `access:write` or `relay:*`.
- The bearer token is stored in the OS keychain and is the only secret mustr holds. Forge tokens stay in T3.
- **Verified in the M0 spike** (`mustr-docs/T3-PROTOCOL.md`):
  - Connecting: `POST /api/auth/websocket-ticket`, then `ws://…/ws?orchestrationProtocol=2&wsTicket=…`.
  - Messages: Effect RPC as JSON (`Request` / `Chunk` + `Ack` / `Exit`), with typed errors.
  - Orchestration HTTP endpoints also need the header `x-t3-orchestration-protocol: 2`.
  - A plain Rust client did all of it in about 300 lines.

### `t3_client`
- **Types are generated from T3's contracts** in `packages/contracts/src` (`rpc.ts` about 2,160 lines, `orchestrationV2.ts` about 4,200, `pullRequest.ts` about 925). The plan is Effect Schema → JSON Schema → Rust through `typify`, validated in the M0 spike. If that pipeline doesn't hold up, the fallback is hand-written types for just the RPCs mustr uses, checked by the contract tests.
- **A small transport** handles RPC over WebSocket: requests, streams, and reconnects.
- Only the RPCs mustr uses get a typed wrapper.

### RPCs mustr uses in v0.1

| Area | RPCs |
|---|---|
| Agent threads | `GET /api/orchestration/shell` + `orchestration.subscribeShell` (thread list: 1,529 threads in about 250 ms in the spike), `orchestration.launchThread`, `dispatchCommand` (messages, approvals, queue, steer, interrupt), `subscribeThread`, `getTurnDiff` (per turn, using `checkpointTurnCount` from the thread detail), `getFullThreadDiff`, `getWorktreeThreadIds`. Not `searchThreads` for listing: it's full-text search and took 33 s. |
| PRs and review | `pullRequests.list` (live forge calls, about 2 s, so show the cached list first), `detail`, `POST /api/pull-requests/diff` (the unified patch), `diffFileContents` (old/new text, which becomes the base text and buffer text of a Zed `BufferDiff`), `filesViewed`, `setFilesViewed`, `threadComments`, `replyToThread`, `setThreadResolution`, `submitReview`, `comment`, `updateComment`, `checks`, `activity`, `linkedThreads`, `subscribeRefreshes` |
| Workspace | `vcs.createWorktree`, `vcs.removeWorktree`, `vcs.listRefs`, `shell.openInEditor`, `projects.list` |
| Setup | `provider.install.*`, `provider.auth.*` |

Remaining semantics are confirmed as each milestone wires them up. For agent threads in local worktrees, the branch view can use Zed's own git diff on the real worktree, which gives full IntelliSense.

---

## 5. v0.1 scope

### Must have
**Inbox**
- **One inbox for every change:** PRs waiting on you and agent threads with changes ready to review, side by side.
- Fixed sections in v0.1: *Needs my review*, *Agent work to review*, *My PRs*, and *Running agents*. They're built on T3's PR list filters (state and involvement) and thread search.
- A "whose turn" indicator, where the data allows: review requests, and new commits since your last review.
- Open by URL or `#number` from the command palette.

**Review view** (§7)
- PRs and agent threads in the same review view.
- Viewed files with progress, collapsed generated files and lockfiles, a whitespace toggle, and context expansion.
- Per-commit diffs and changes since your last review.
- Comments as cards: single-line, multi-line, and file-level. Reply, resolve, and edit or delete your own.
- Suggestions, including edits that become suggestions.
- A pending review whose drafts are never lost, submitted as Comment, Approve, or Request changes.
- On agent diffs: local comments, and **Send to agent**.

**PR overview**
- Description rendered as native markdown, labels, reviewers, checks, mergeability, activity, and commits.

**Agent threads** (§8)
- Chat timeline, composer, approvals, queue and steer, new threads with worktrees, thread to PR, and Open in editor.

**The IDE underneath** (§9)
- From Zed: hover, cmd+click, find references, search, the terminal, and vim mode, on PR checkouts and thread worktrees.

**Look** (§6)
- Matches the mockups, through themes and mustr's own chrome.

**Shipping**
- Signed and notarized macOS and Windows builds, Linux packages, mustr's updater, and T3 runtime management.

### Should have (if time allows)
- Merge through `pullRequests.runAction`, and editing reviewers and labels.
- Check annotations inline in the diff.
- A side-by-side diff layout, if Zed's `git_ui` already provides one.
- PR stacks (`pullRequests.stack`).

### Won't have in v0.1
Everything in the Later list (§15).

---

## 6. UX layout and look

```
┌ [acme/api #482 ×] [Retry backoff · Claude ×] [+] ───────────── ⌘K PRs, threads, files… ┐
│ Inbox 4│ Open · Add rate-limited retry… #482   [Open in Cursor ▾] [Submit review ▾]    │
│ Agents │ Overview · Conversation 6 · Files 12 · Commits 4 · Checks 5                   │
│ PRs    ├────────────────┬──────────────────────────────────────────┬───────────────────┤
│        │ Files     7/12 │ Since last review ▾ · Unified | Split    │ Inspector         │
│        │ ☑ dispatcher.ts│ Review multibuffer: every hunk is a live │ Threads · Checks  │
│        │ ☐ retry.ts     │ editor (hover, cmd+click, vim motions);  │ Reviewers · Agent │
│ Prefs  │ ☐ schema.ts    │ comments and drafts render as cards      │                   │
├────────┴────────────────┴──────────────────────────────────────────┴───────────────────┤
│ ● T3 connected · 3 drafts        j/k hunk · ]f file · c comment · v viewed · a agent   │
└────────────────────────────────────────────────────────────────────────────────────────┘
```

- **Title bar:** mustr's own, holding tabs for open PRs and threads, plus the command search.
- **Icon rail:** Inbox (with an unread count), Agents, PRs, and Settings.
- **Review item:** a header (state pill, title, Open in editor, Submit review) and tabs (Overview, Conversation, Files, Commits, Checks).
- **Panels:** the file panel with viewed checkboxes, the review multibuffer with its toolbar, and an inspector on the right (Threads, Checks, Reviewers, Agent).
- **Status bar:** T3 connection state, draft count, and key hints for the focused pane.
- It's all built on Zed's workspace, so the rail, file panel, and inspector are dock panels that can move, resize, or hide.

### Look and feel

**Requirement: mustr looks like the mockups** (main review screen, inbox, hover card, inline agent thread), not like Zed.

**Design tokens**, taken from the mockups' CSS:

| Token | Dark | Light | Colorblind-safe (dark base) |
|---|---|---|---|
| Background | `#0f1318` | `#ffffff` | as Dark |
| Surface | `#151a21` | `#f6f8fa` | as Dark |
| Surface 2 | `#1c232c` | `#eceff3` | as Dark |
| Border | `#29313b` | `#d8dee4` | as Dark |
| Text | `#d6dce3` | `#1f2328` | as Dark |
| Muted text | `#8792a0` | `#636c76` | as Dark |
| Accent | `#5aa2ff` | `#0969da` | as Dark |
| Added line (strong) | `rgba(46,160,67,.14)` (`.42`) | `#e6ffec` (`#abf2bc`) | `rgba(56,139,253,.15)` (`.46`) |
| Deleted line (strong) | `rgba(248,81,73,.12)` (`.40`) | `#ffebe9` (`#ffc1bd`) | `rgba(219,109,40,.14)` (`.46`) |
| Warning | `#d29922` | `#9a6700` | as Dark |
| OK | `#3fb950` | `#1a7f37` | `#58a6ff` |
| Pending (drafts) | `#b083f0` | `#8250df` | as Dark |
| Agent | `#d97757` | `#d97757` | as Dark |

- **Type:** the system UI font for the interface and a monospace font for code, on a compact 11.5–13 px scale.
- **Components:** pill chips, segmented controls, kbd hints, split buttons, cards with 8 px corners, 1 px borders, the icon rail, and key hints in the status bar.

**How v0.1 gets there, with no patches to Zed**
1. **Themes:** mustr Dark, Light, and Colorblind-safe, written as Zed theme JSON from the tokens above in `assets/themes/mustr/`. mustr's default settings select them and set the default fonts and `ui_density`.
2. **A `mustr_ui` crate** holds mustr's components: chip, pill, segmented control, kbd, split button, card, file row with a viewed checkbox, inbox row, thread card, and status hints. Every mustr surface is built from it.
3. **mustr's own chrome, through Zed's extension points:**
   - The title bar, through `Workspace::set_titlebar_item`.
   - Status bar items, through `StatusBar::add_left_item` and `add_right_item`.
   - Dock panels and pane items for the rail, inbox, file panel, inspector, review, and threads.
   - Zed's pane tab bar turned off with `tab_bar.show`, and Zed's own panels hidden by default.
4. **What still has Zed's shape in v0.1:** menus, pickers, the hover popover, icons, the settings UI, and the terminal. They're recolored by the theme. Restyling them comes later: first by proposing configurable theme options upstream to Zed, and only if that fails, as small tracked patches.

---

## 7. The review view

- **Where diffs come from:** T3. PRs come through `pullRequests.diffFileContents` and the PR diff; agent threads through `orchestration.getTurnDiff` and `getFullThreadDiff`.
- **How they're shown:** as excerpts of whole-file buffers in a Zed multibuffer, so tree-sitter always parses full files and every hunk is a live editor.
- **Where comments can go:** the forge's diff hunks decide. GitHub rejects line comments outside its hunks, so the UI disables line comments on those lines and explains why. File-level comments are always allowed.
- **Comment cards:** threads and drafts render as editor blocks, both in the multibuffer and in code view.
- **Drafts are never lost.** That's a requirement. It's satisfied by T3's server if it persists pending review drafts, or otherwise by a small local store in mustr on Zed's `db`. Verified in M0.
- **Submitting:** `pullRequests.submitReview` takes the verdict, body, and comments. Errors are reported per comment.
- **Viewed files:** `filesViewed` and `setFilesViewed`.
- **Freshness:** `pullRequests.subscribeRefreshes` and `orchestration.subscribeThread` push changes. mustr does no polling of its own.

**Reviewing an agent's work**
- A thread's changes open per turn or for the whole thread, with the same tools as a PR.
- Comments on an agent's diff are local threads. **Send to agent** batches them, with file and line anchors, into the thread's next message through `dispatchCommand`.
- When a thread becomes a PR (`pullRequests.linkedThreads`), any open local comments can be posted as a review.

---

## 8. Agent threads

- **Threads panel:** threads across projects, with status: running, needs approval, ready for review, or sent back.
- **Timeline** from `subscribeThread`: messages, tool calls, file edits as inline diffs, plans, and usage.
- **Composer:** send, queue messages while the agent works, steer (interrupt and redirect), attach files and images, and @-mention files.
- **Approvals:** prompts appear inline, with a permission mode per thread as T3 supports it.
- **New threads:** pick a project and base branch, and T3 creates the worktree. The agent and model pickers come from T3's provider catalog.
- **Thread to PR:** through T3's existing thread-to-PR flow, verified in M0.
- **Open in editor:** mustr's split button calls T3's `shell.openInEditor`.
- **Shared with T3's apps:** a thread started from T3's phone app shows up in mustr, and the reverse.
- **Safety:** T3's approval model applies, and mustr starts new threads in "ask" mode. PR content can carry prompt injection, so agent sessions over other people's PRs stay read-only by default once they exist (§15).

---

## 9. The IDE underneath

- **From Zed:** every hunk in the review is a live editor, with hover, cmd+click, find references, vim motions, and search. Code view tabs open the full file from the worktree.
- **Worktrees:** a thread's worktree comes with the thread. For a PR, mustr asks T3 for a worktree (`vcs.createWorktree`) when you open code view or IntelliSense, or on PR open if a setting says so.
- **Trust:** Zed's Restricted Mode, as-is. Untrusted worktrees run no language servers and ignore `.zed/settings.json` until you trust them through Zed's own prompt. The sandboxed "review" trust level is Later (Appendix A).
- **mustr's wiring in v0.1:**
  - cmd+click in the review jumps within the review when the target is in a changed file.
  - Back returns to your exact spot.
  - Grouping references into "in this PR" and "outside this PR" is Later.

---

## 10. Customization

v0.1 keeps Zed's customization intact and adds a little.

- **Zed's files work as usual:** `settings.json`, `keymap.json` (with key contexts), themes and `theme_overrides`, tasks, and extensions.
- **mustr's settings** live in a `review` block in the same `settings.json`:
```jsonc
{
  "theme": "mustr Dark",
  "vim_mode": true,
  "review": {
    "context_lines": 3,
    "ignore_whitespace": false,
    "collapse": ["generated", "**/*.lock"],
    "confirm_submit": true
  },
  "t3": { "mode": "installed" }     // "installed" | "embedded" | "external" (with "url")
}
```
- **mustr's actions** have their own key contexts:
```jsonc
[
  {
    "context": "ReviewDiff",
    "bindings": {
      "j": "review::NextHunk",
      "c": "review::CommentOnLine",
      "v": "review::ToggleViewed",
      "a": "review::SendToAgent",
      "g i": "mustr::OpenInbox"
    }
  }
]
```
- Later (§15): a custom inbox view language, per-host, org, and repo settings scopes, saved replies, keymap presets, and repo-provided defaults.

---

## 11. Repo layout

```
mustr/                       # Zed's tree, synced by merging Zed stable tags
  crates/                     # Zed's crates, unchanged
  mustr-crates/
    mustr/                   # app binary + init, used instead of crates/zed
    mustr_ui/                # components and look (§6)
    inbox/  review/  thread_chat/
    t3_client/                # generated types + transport; T3_VERSION pin (§4)
  assets/themes/mustr/       # mustr Dark, Light, Colorblind-safe
  mustr-docs/                # SYNC.md and mustr's docs; Zed's docs/ stays as it is
  script/mustr-sync-zed
  script/mustr-sync-t3
  THIRD_PARTY_NOTICES.md      # T3 Code (MIT): downloaded runtime, generated client types
  .github/workflows/mustr-*.yml
```
- If upstream Zed ever adds a crate with the same name as one of mustr's, mustr's gets renamed.
- Tooling follows Zed's: cargo, `./script/clippy`, and rustfmt, plus DCO sign-off for mustr's own changes.

---

## 12. Security

- **Secrets:** mustr holds only the T3 pairing credential, in the OS keychain. Forge tokens live in T3 and never pass through mustr.
- **The T3 server:** when mustr manages it, it listens locally only. Remote access is T3's own feature, configured in T3.
- **Untrusted input:** PR titles, bodies, file names, diffs, and agent output.
  - Markdown is rendered natively by GPUI, with no HTML engine, so there's no XSS class of bugs.
  - Links open in the system browser.
- **Agents:** T3's approval model applies, and mustr defaults new threads to "ask" (§8).
- **Language servers:** Zed's Restricted Mode until you trust a worktree (§9).
- **Zed services:** telemetry upload and zed.dev endpoints (accounts, hosted AI, extension registry, auto-update) are disabled in mustr's app crate. A CI test runs the app with zed.dev blocked and fails on any attempt.
- **Updates:** mustr's builds are signed. The T3 runtime download is checksum-pinned per mustr release.
- **No telemetry.**

---

## 13. Performance budgets

| Metric | Budget |
|---|---|
| Cold start to interactive inbox (T3 server already running) | < 1 s |
| Local T3 RPC round trip (p50) | < 10 ms |
| Open a PR or thread whose diff is cached | < 200 ms |
| Open an uncached typical PR (~20 files) | < 2 s |
| Scrolling a 300-file / 30k-line review multibuffer | 120 fps on ProMotion displays, 60 fps on a 2020-era laptop |
| Thread timeline update after T3 emits an event | < 50 ms |
| Memory with 10 PRs and threads open, excluding language servers and T3 | < 500 MB |
| Hover or go to definition, language server warm | < 150 ms |

---

## 14. Forge API constraints

T3's server talks to the forges. These are the limits the UI has to respect, and the M0 spike confirms how T3 handles each one.

- Line comments must fall **inside the diff hunks**. File-level comments are the exception.
- **Large PRs:** GitHub's files endpoint caps at 3,000 files, patches are omitted for very large files, and the `.diff` media type errors on very large PRs. The UI loads files lazily and shows a placeholder when a patch is missing.
- **No public API for uploading images in comments,** so there's no paste-to-upload. Paste a link instead.
- **One pending review per user per PR** on GitHub.
- **Rate limits,** including secondary limits on content creation. Bulk comment submission has to be paced.
- **Org restrictions:** SAML SSO and OAuth-app restrictions surface as errors from T3. The UI shows them with the fix (an authorize link, or T3's credential settings).

---

## 15. Roadmap

Sizes assume **one experienced developer working full-time**.

| Milestone | Contents | Exit criteria | Size |
|---|---|---|---|
| **M0: Foundations** | **Spike first:** a Rust client lists threads and PRs from a real T3 server (done 2026-10-27; see `mustr-docs/T3-PROTOCOL.md`), then opens one PR's diff and one thread's diff in a multibuffer. Then: rebrand (name, icon, app ID); strip features at the top in the `mustr` crate (no collab, hosted AI, edit predictions, Copilot, onboarding, or telemetry upload; zed.dev endpoints off); mustr themes, title bar, and status bar; `mustr-sync-zed`, `mustr-sync-t3`, and `SYNC.md`; T3 runtime management and pairing; generated `t3_client`. | The spike works; both sync scripts run end to end; a rebranded build looks like the mockups and makes no requests to zed.dev | 2–3 wk |
| **M1: Inbox and review** | The unified inbox. The review view for PRs and agent threads: multibuffer, viewed files, comment cards, reply and resolve, submit review, local comments on agent diffs, and Send to agent. The PR overview with native markdown. | **Dogfood gate:** you review every PR and agent change in mustr | 4–5 wk |
| **M2: Agent threads** | Thread chat (timeline, composer, approvals, queue and steer), new threads with worktrees, thread to PR, and Open in editor | You run your agents from mustr instead of T3's app | 3–4 wk |
| **M3: Ship v0.1** | Packaging for macOS, Windows, and Linux; mustr's updater on GitHub Releases; docs; CONTRIBUTING | v0.1 tagged and announced | 2 wk |

**Total to v0.1: about 3 months solo,** including an hour or two a week of upstream syncs.

**Packaging:** macOS `.dmg` and Homebrew cask; a Windows installer, winget, and Scoop; Linux AppImage, `.deb`, `.rpm`, and AUR. Flathub waits, because its sandbox complicates running T3 and the agent CLIs.

### Later
One line each, roughly in priority order:
- **A mustr extension registry:** build extensions from the open-source `zed-industries/extensions` repo (each under its own license) and publish them on GitHub Releases, since Zed's own registry is off (§18).
- **The review trust level:** sandboxed language servers with safe per-language presets, so PR checkouts get IntelliSense without full trust (Appendix A).
- **Review assistants:** inline "is this right?" asks, PR chat, and proposed comments. T3 already has `pullRequests.reviewGuide`, `generateReviewGuide`, and an orchestrator MCP, so check what they cover before building anything.
- **Restyling Zed's shared UI** (menus, pickers, hover popover, icons) through configurable theme options proposed upstream, or small tracked patches if upstream declines.
- **Custom inbox views:** a query and filter language with computed fields, beyond v0.1's fixed sections.
- **Settings scopes** per host, org, and repo, plus saved replies and keymap presets (Default, GitHub-web-compatible, Vim).
- **"In this PR" / "outside this PR"** grouping for references and call hierarchy.
- **A browser extension and `mustr://` deep links** ("Open in mustr" on GitHub PR pages; links can only navigate, never act).
- **A tray or menu-bar badge** and OS notifications. GPUI has no tray support, so this is platform work.
- **A review extension API** (diff decorations, inbox columns, file renderers) on Zed's WASM extension system, and a mustr extension registry.
- **More forges in the UI.** T3 already supports them, so this is mostly testing and capability flags.

---

## 16. Open source project setup

- **License: GPL-3.0-or-later.** Zed's crates keep their own licenses (GPL-3.0-or-later for the editor and core, Apache-2.0 for `gpui` and a few others). Keep Zed's `LICENSE-GPL` and `LICENSE-APACHE`.
- **Attribution:**
  - The README says mustr is a fork of Zed and a client of T3 Code, and is affiliated with neither.
  - Zed's and T3 Code's names and logos aren't used for mustr.
  - `THIRD_PARTY_NOTICES.md` carries T3 Code's MIT notice, because mustr downloads T3's runtime and generates code from its contracts.
- **Contributions:** DCO sign-off for mustr's own code. Fixes that belong upstream go to Zed (under Zed's CLA) or T3 Code.
- **No website.** The GitHub repo is the home page. Docs are markdown in `mustr-docs/`, and releases and the updater manifest live on GitHub Releases.
- **Templates:** CONTRIBUTING, a code of conduct, and issue and PR templates that replace Zed's.
- **Funding:** GitHub Sponsors or Open Collective, to pay for the Apple Developer Program ($99/yr, needed for notarization). Windows signing is free through the SignPath Foundation for OSS.
- **Dogfooding:** review mustr's own PRs and agent work in mustr from M1 onward.

---

## 17. Risks and mitigations

| Risk | Impact | Mitigation |
|---|---|---|
| T3's RPC API isn't a documented public API and changes | High | Pin the T3 version, generate the client from its contracts, run contract tests in CI plus a nightly run against T3's latest release, and follow `SYNC.md` |
| The Effect Schema → Rust codegen doesn't hold up | Medium | Validate it in the M0 spike; fall back to hand-written types for the RPCs mustr uses, checked by contract tests |
| T3's product direction changes (licensing, accounts, features) | Medium | It's MIT, so fork the pinned version if needed; keep mustr's use of T3 behind `t3_client` |
| The user's installed T3 is incompatible | Medium | A version check on connect; offer an update or fall back to mustr's own pinned runtime |
| Two processes and two update channels confuse users or fail | Medium | mustr manages its runtime's lifecycle, shows T3's connection state in the status bar, and documents both update paths |
| Zed upstream churn breaks mustr's crates | Medium | Zero patches to Zed's crates, a small set of Zed APIs, weekly merges, and a nightly build against Zed's `main` |
| Zed-shaped menus and pickers clash with the mockup look | Low | Themed in v0.1; propose theme options upstream later |
| Incomplete rebrand, or zed.dev services leak through | Medium | A rebrand checklist in M0, a CI check for user-visible "Zed" strings, and a CI run with zed.dev blocked |
| Prompt injection through PR content into agents | High | T3's approvals, "ask" by default, and read-only agent sessions over others' PRs once they exist |
| GPUI accessibility | Medium | AccessKit support is in upstream `gpui` but not the published crate. Track it, and test with screen readers before v0.1. |
| Scope creep | High | The Later list (§15), a dogfood gate on every milestone, and principle 2: no feature that makes syncing harder |
| Code signing for a FOSS project | Low | SignPath for Windows, a sponsor-funded Apple account, and a documented release runbook |

---

## 18. Decisions

**Decided**
1. **Name:** mustr.
2. **License:** GPL-3.0-or-later.
3. **Base:** a soft fork of Zed as the UI, and an unmodified T3 Code server as the backend (§2).
4. **Zed channel:** stable releases. On 2026-10-27, `main` was recreated on Zed v1.25.3; `script/mustr-sync-zed` follows each new stable tag (`mustr-docs/SYNC.md`).
5. **Agent instructions:** `CLAUDE.md` and `AGENTS.md` point to `mustr-docs/AGENTS.md`, which keeps Zed's Rust and GPUI rules and drops the parts written for Zed's own repository.
6. **Zed's extension registry is off.** Zed's terms (§2.4) forbid accessing non-public APIs of Zed's service, so mustr makes no requests to zed.dev at all. v0.1 relies on Zed's built-in languages. A mustr registry built from the open-source extensions repo is Later.
7. **App identity:** app ID `dev.mustr.mustr`, URL scheme `mustr://`, and a placeholder icon until a real one exists.
8. **M0 started** on 2026-10-27, solo.
9. **The engine (2026-10-28):** mustr uses the installed T3 Code by default (shared threads, §4), offers to install it when it's missing, and runs its own pinned copy when it can't be used or `"t3": {"mode": "embedded"}` is set.

**Open**
1. **Zed's git commit and push UI:** hidden by default (recommended, since mustr is review-first) or visible.

---

## Appendix A: The review trust level (Later)

Zed's Restricted Mode runs no language servers until a worktree is trusted, and full trust is too loose for code from strangers. A middle "review" level would run language servers for PR checkouts in two layers:

1. **A sandbox:** a `mustr_sandbox` wrapper with no network and writes only to cache directories, using Seatbelt on macOS and Landlock + seccomp on Linux.
2. **Safe presets per language:**

| Language | Review level | Trusted repo |
|---|---|---|
| TypeScript / JS | Dependencies installed with `--ignore-scripts`; mustr's own TypeScript, never the repo's; no project tsserver plugins | The repo's TypeScript and plugins |
| Rust | `cargo fetch` only; no build scripts, proc macros, or `rustc` wrappers | Build scripts and proc macros on |
| Go | `go mod download` with `GOTOOLCHAIN=local` and `CGO_ENABLED=0` | cgo and toolchain downloads allowed |
| Python | No installs; mustr picks the interpreter | A venv you choose, or `uv sync` |
| C / C++ | `compile_commands.json` if present; no `--query-driver` | `--query-driver` allowed |

The cost is a patch to Zed's `project/src/trusted_worktrees.rs`. So it waits until it can be proposed upstream as a pluggable trust level, or until IntelliSense on PR checkouts matters enough to carry one tracked patch.

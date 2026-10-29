# mustr release notes

## 0.1.1

- **Back to the original size.** 0.1.0 started at twice the size of mustr's designs, which was too
  big on a laptop. The default is the designs' size again (Settings › General › Zoom, 100%). If you
  zoomed in 0.1.0, mustr keeps your size; ⌘0 or Reset goes back to the new default. ⌘= and ⌘- still
  zoom the whole app, about 10% a step.
- **Settings, all in the app.** ⌘, (or mustr › Settings…, or the gear at the bottom of the rail)
  opens one Settings page instead of a settings file. Its sections: General (theme, zoom, code
  size, fonts, what closing the last window does), Editor, Terminal, Agents and threads, Connection,
  Updates, and Keyboard shortcuts. The search box finds a setting in any section, and changes apply
  at once.
- **T3 Code's settings moved into it.** They're the "T3 Code on <your Mac>" part of Agents and
  threads, next to mustr's own (follow-ups, which notifications to show, sounds, and a new switch
  for the count on the Dock), with worktree cleanup and your source control accounts. The rail's
  T3 Code settings button is gone; "mustr: open t3 settings" in the palette opens that part.
- **Keyboard shortcuts** (Key Bindings…, or Settings › Keyboard shortcuts): every shortcut and
  every action, searchable. Change a shortcut by pressing the new keys, give an action one, remove
  one, or put an action's back the way mustr has it. ⌘↩ on a command in the palette opens its
  shortcut here.
- **No JSON.** mustr no longer opens `settings.json` or `keymap.json`: it keeps your choices there,
  but you change them in Settings. If one of them can't be read, Settings says why and can start it
  over from mustr's defaults, keeping the old file next to it as `.bak`.
- **A gear** for Settings on the rail, in place of the icon that looked like a sun.
- **Clean rounded corners.** Hovering a button, a split button's ▾, or a review comment's title,
  and the selected option of a control like Unified | Split, no longer draws square edges over the
  rounded corners. The shortcut you're changing in Keyboard shortcuts is highlighted the same way.
- **A calmer thread toolbar.** One row, one height: at the left the branch (`● main 3 ▾`, the
  count is its changed files) and the pull request (`● #80 ✗ ▾`, its state as a dot and its
  checks as an icon, the details in the tooltip); at the right Review changes, Commit ▾, then
  Terminal and Open in as icon buttons, and "…" for what doesn't fit. The pull request's menu has
  everything in one place: review it, open it on GitHub, mark a draft ready, merge or close it,
  watch it for CI and comments, unlink it, and the thread's other linked pull requests (0.1.0's
  `+2`).
- **Open in shows the programs' icons.** The button shows the icon of the editor it opens (the
  name is in its tooltip, with ⌘O), and its menu lists each editor with its icon. On a Mac
  running T3 Code, they're the apps' own icons; otherwise T3's.
- **Terminal is an icon button,** with its shortcut (⌘J) in the tooltip.
- **No machine name in the title bar.** The connection chip at the top right is gone. The status
  bar shows the connection (Synced, T3 offline, …); click it to reconnect, switch servers
  (Settings › Connection), or see what's wrong in the Inbox.
- **Hand off to another agent is gone** from the thread toolbar.
- **Four places on the rail: Inbox, Edit, Review, Agents** (⌘1 to ⌘4), with Settings at the
  bottom. Each button's tooltip names it and its shortcut.
  - **Inbox** is what Home was: what needs you, the connection, and your projects.
  - **Edit** is the editor on one folder: pick one of T3's projects on this Mac, a recent folder,
    or Open Folder… at the top of its file tree. Files open as tabs (only here), ⌘P finds a file
    and ⌘⇧F searches in that folder, and mustr remembers the folder. Open in mustr's editor (in a
    thread's Open in menu) and file links in agent messages open here. Clicking Edit again shows
    or hides the files.
  - **Review** lists what there is to review: pull requests in sections, each a GitHub-style
    query (Review requested, Yours, and the other open ones in your projects, to start with), with
    their checks and reviews, and the agent work waiting for review. A section's menu edits its
    query, renames or removes it, and Add section adds one (Settings › Review lists them too). A
    search box narrows everything. In a review, Review goes back to the list. This fixes the Pull
    requests button, which did nothing when no review was open.
  - **Agents** is the threads, in a wider list (about T3's width), and the thread you pick.
    Scheduled tasks, worktrees, usage (⌘U) and agents setup moved from the rail to the row of
    buttons under the threads; the palette and their shortcuts still open them.
- **No more tabs outside Edit.** Opening a thread, a review or a page shows it in place of what
  was there, like an app rather than an editor. The title bar shows where you are, with back and
  forward arrows: ⌘[ and ⌘] (and the mouse's back and forward buttons) go back and forward
  through what you looked at, from one place to another, and each rail button brings back what
  its place showed last. Outside Edit, ⌘W goes back instead of closing anything; in Edit it
  closes the file.

## 0.1.0

The first release. mustr is a review-first desktop app for working with coding agents: agents
write the changes, and you read, review and ship them in a full code editor. It's built on Zed and
runs its agents on T3 Code, your own installation or a copy mustr runs for you, with your own
Claude Code and Codex.

### Install

On an Apple silicon Mac, in Terminal:

```sh
curl -fsSL https://github.com/msetaro/mustr/releases/latest/download/install.sh | sh
```

It installs `/Applications/mustr.app` (or `~/Applications/mustr.app`). Install with this command,
not from a browser download: the app isn't signed with an Apple Developer ID yet, and macOS won't
open a copy a browser downloaded. `mustr-docs/MACOS.md`, "First run on a new Mac", walks through
the first launch.

### What's in it

**Connecting to T3 Code**
- Uses the T3 Code installed on your Mac and pairs with it through its `t3` command; the token
  stays in your keychain.
- Without T3 Code, Home offers to install it, set up its service, or use mustr's own engine.
- Saved servers: switch between this Mac's T3 Code, mustr's own engine and other T3 servers from
  the title bar.

**Agent threads**
- A new-thread form with T3's models, effort and other options, permissions, plan mode, and where
  the agent works: the project's checkout, a new worktree from a base branch, an existing worktree,
  or no project.
- Start a thread in the background, or one prompt with several models at once.
- T3's thread list: Pinned, Active, Snoozed and Settled, with settle and settle-and-advance,
  snooze, pin, archive, delete, multi-select, drag between sections, and undo.
- Search threads by title, branch, project or PR, then by what was said in them; find in a
  thread; hover cards and thread details.
- Subagents, delegated tasks and workflows above the timeline, to open or stop each one.
- Scheduled tasks: a page to create, edit, enable, run and delete them, with webhook delivery logs.

**Composer and timeline**
- Images, files and big pastes; `@` for files and threads, `#` for PRs, `/` for commands and `$`
  for skills.
- Drafts that survive restarts, prompt recall, the prompt stash, quoting a response, and Markdown
  formatting keys.
- Approvals, questions (with images and files in the answer), MCP requests and secret requests.
- The queue: steer, edit, reorder, send now, cancel and resume; usage limits with resume at reset.
- Edit from here, fork, compact the context, restart the agent's session, and hand off to another
  agent.
- Live command output, file links that open in the editor, HTML renders and plans saved to the
  project.

**Reviewing and shipping**
- Review a thread's whole change, its uncommitted changes, or one turn, and send comments back to
  the agent.
- Agent worktrees inherit Restricted Mode trust from their project: trust a project once (Trust
  Project, from the review's Restricted Mode button) and its worktrees open with language servers
  on.
- Commit, push and open a PR from the thread; the branch picker; open the folder in your editor.
- Link, watch and merge PRs, Work on PR, a PR's threads, reviewers, labels, title, stacks and review
  guides, next to mustr's own PR review.
- Worktrees: a page of T3's worktrees, removal on archive, T3's automatic cleanup rules, moving a
  thread between its checkout and a worktree, and merging a fork back.
- Terminals per thread (`⌘J`), local or on T3's machine, with the project's scripts and its local
  dev servers.

**Settings and accounts**
- T3's settings for new threads, follow-ups, auto-settle and branch names.
- An agents page: install, update and sign in to agents, several accounts per agent, and ACP
  agents.
- Source control accounts, project settings, and a usage dashboard (`⌘U`).

**The app**
- Notifications, sounds and the Dock badge for threads that need you.
- The command palette has mustr's actions, including "New thread in <project>".
- Zoom the whole app with `⌘=`, `⌘-` and `⌘0`; mustr remembers your size.
- It updates itself (below).

### Known gaps

- **Barely tried on a Mac.** mustr was built and tested on Linux and type-checked for macOS.
  It has been built from source and run on one Apple silicon Mac, and this release was built and
  packaged on GitHub's Macs, but the packaged app, the installer, the updater's last steps, the
  menus and the keychain prompts run on a Mac for the first time with this release.
- **Signed ad hoc.** No Apple Developer ID or notarization yet, so install with the command above.
  A work Mac that only allows identified developers' apps would block it.
- **Apple silicon only.** T3 Code publishes no engine for Intel Macs.
- **One server at a time.** T3's SSH, Tailscale and T3 Connect setup isn't in mustr; a server
  reached over Tailscale or SSH can be saved by its address.
- **No web engine.** Agents' previews and HTML renders open in your browser; there's no preview
  panel or MCP apps.
- **Smaller gaps:** no math in Markdown or timeline minimap; the schedule editor's and T3
  settings' model pickers still show hidden models; no Regenerate title, project icons, Merge
  stack or Rebase stack; importing Claude or Codex sessions, guest share links and paired-client
  management stay in T3's own app or CLI.
- `mustr-docs/PARITY.md`, "After v0.1.0", has the full list.

### Updates

mustr checks for a newer release 20 seconds after it starts and every 4 hours. It downloads it,
checks its signature and checksum, and swaps it in while you work; then the status bar offers
**Restart to update**. mustr › Check for Updates… checks right away, and
`"updates": {"enabled": false}` in the settings file turns the automatic checks off (from 0.1.1:
Settings › Updates › Check automatically).
`mustr-docs/MACOS.md`, "Install a release", has the details.

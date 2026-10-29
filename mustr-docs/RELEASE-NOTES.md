# mustr release notes

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
- Zoom the whole app with `⌘=`, `⌘-` and `⌘0`; the default size is twice the designs'.
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
`"updates": {"enabled": false}` in the settings file turns the automatic checks off.
`mustr-docs/MACOS.md`, "Install a release", has the details.

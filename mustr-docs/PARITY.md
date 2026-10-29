# Parity with T3 Code's app for agent work

What T3 Code's web and desktop app does for agent-driven development, what mustr does today, and
what to build so a developer can stop opening T3's app.

- **Compared:** mustr 0.1.0 (`m3/next` at `0c4bb5b473`, after WP0–WP18 landed) against T3 Code
  `v0.1.3` (`d2c44031`, 2026-10-26). The list was first written at `m2/threads` (`a96368ce93`).
  Since 2026-10-28 mustr's `t3_api` is generated from T3 0.1.3 contracts (`t3_client/T3_VERSION`);
  the local server is still 0.1.1. Calls added after 0.1.1 are marked *(0.1.3)*: a 0.1.1 server
  doesn't have them, so mustr falls back there (`mustr-docs/T3-PROTOCOL.md`, "Compatibility").
- **Paths:** T3 paths are relative to T3's `apps/web/src/` unless they start with `docs/`,
  `packages/` or `apps/`. `chat/`, `settings/`, `pullRequest/` and `preview/` are short for
  `components/chat/` and so on. mustr paths are relative to `mustr-crates/`.
- **Sources:** T3's API comes from `packages/contracts/src/rpc.ts` (`WS_METHODS`),
  `orchestrationV2.ts` (the `OrchestrationV2Command` union) and `environmentHttp.ts`. Scopes come
  from `apps/server/src/auth/RpcAuthorization.ts`.
- **Effort** (one agent): **S** is a day or less, **M** is 2 to 5 days, **L** is more than a week.
  For partial rows it's what's left.
- **Priority** for a developer replacing T3's app at work: **P0** blocks the switch. **P1** is
  needed within weeks. **P2** is nice to have. **Skip** means mustr doesn't need it, with the reason
  given.
- **The user:** macOS on Apple Silicon, with Claude Code and Codex. Their must-haves are subagents
  and scheduled tasks, worktrees and terminals, and the PR flow. Those rows say "P0: user's
  must-have".

## 1. Where mustr stands at v0.1.0

mustr 0.1.0 covers T3's app for daily agent work, on the user's own T3 Code or on mustr's own
engine. Every P0 and P1 row is done, and 33 of the 38 P2 rows (the counts are at the end of §2).

- **The must-haves:** subagents, delegated tasks and workflows in the family bar; the scheduled
  tasks page and editor; worktrees with T3's cleanup rules, worktree handoff and merge back;
  per-thread terminals (local ones, or T3's own for a thread on another machine) with project
  scripts; and the PR flow: commit, push and PR from the thread, link, watch, merge, Work on PR,
  a PR's threads, reviewers, labels, title, stacks and review guides.
- **Threads:** the new-thread form with T3's models, options, permissions and workspaces, one
  prompt for several models, and starting in the background; T3's sidebar sections with settle,
  snooze, pin, archive, delete, bulk select, drag, undo, hover cards and details; search by title
  and by message; find in a thread; cloning a repository as a project.
- **Composer and timeline:** images, files and big pastes; `@`, `#`, `/` and `$`; drafts, the
  prompt stash, quotes and Markdown formatting; approvals, questions (with attachments), MCP
  requests and secrets; the queue; Edit from here, Fork, Compact, hand off to another agent; file
  links, HTML renders and plans saved to the folder.
- **Reviewing agent work:** the whole thread's changes, its uncommitted changes, and each turn,
  with comments sent back to the agent.
- **Settings and accounts:** T3's settings, the agents page (installing, signing in, several
  accounts, ACP agents), source control accounts, project settings, the usage dashboard, and
  saved servers switched from the title bar.
- **The app:** notifications and the Dock badge, the command palette's thread and project
  actions, zoom, macOS menus, and a self-updating app bundle (`mustr-docs/RELEASING.md`).

mustr sends 38 of T3's 55 orchestration command types (most of the rest are the server's own) and
calls about 90 of its RPCs, with the scopes listed at the end of §2. It works against T3 0.1.1,
0.1.3 and the 0.1.4 nightlies (all protocol 2). It fits a MacBook's window at its default size (13) and
zoomed to twice that (26). §3 lists what's left after v0.1.0.

## 2. Features by area

### 2.1 Connection and environments

| Feature | What T3 Code does | T3 API | mustr status | Effort | Priority |
|---|---|---|---|---|---|
| Pair with a local T3 | Exchanges a pairing credential for a bearer token, then opens the WebSocket | `/.well-known/t3/environment`, `POST /oauth/token`, `POST /api/auth/websocket-ticket`, `/ws` | Done (`mustr/src/t3_connection.rs`) | — | P0 |
| Run the engine in-app | The desktop app bundles and runs the server (`apps/desktop`) | `t3 serve`, `t3 auth pairing create` | Done (`mustr/src/t3_engine.rs`, `t3_runtime/`): the installed T3, mustr's own engine (also the fallback), or a server at a URL | — | P0 |
| Version skew, server update | Warns on mismatch and updates the server (`versionSkew.ts`, `components/ServerUpdateAction.tsx`) | `server.updateServerWithProgress`, `subscribeServerLifecycle` | Done (`mustr/src/t3_engine.rs`, `mustr/src/home.rs`): the protocol is checked on connect, and Home offers Update T3 Code (`t3 update`) for an older installed T3: as a prompt for an older protocol, and as a quiet "T3 Code 0.1.3 is available." for an older version on the same protocol; the embedded engine is pinned | — | P2 |
| Several environments at once | One client, many servers; picks a host for each new thread (`environments/`, `settings/AddEnvironmentDialog.tsx`) | One `/ws` per environment, `server.getHostResources` | Partial (`mustr/src/servers.rs`): saved servers by name and URL, switched from the title bar's connection chip or Home's Servers… (this computer's T3 Code, mustr's own engine, or a saved one), each with its own token, the last pick kept for the next launch. One connected at a time; the embedded engine keeps running when mustr switches away | L (the rest) | P2 |
| Remote access (SSH, Tailscale, T3 Connect) | Reach a home machine's server from elsewhere (`packages/ssh`, `packages/tailscale`, `cloud/`) | `/api/connect/*`, `cloud.*` | Missing | L | P2: T3's own remote setup keeps working |
| Paired clients, pairing links | List and revoke sessions (`settings/AuthorizedClientsSettings.tsx`) | `/api/auth/clients*`, `/api/auth/pairing-links*`, `subscribeAuthAccess` | Missing | S | Skip: needs `access:*` scopes, which mustr never asks for (PLAN §4); use `t3 auth` |

### 2.2 Projects and new threads

| Feature | What T3 Code does | T3 API | mustr status | Effort | Priority |
|---|---|---|---|---|---|
| Add a project | Adds a local folder as a project (`components/AddProjectToMachineDialog.tsx`, `CommandPaletteAddProject.tsx`) | `projects.mutate` `{project.create}` (or `POST /api/projects/mutate`); `filesystem.browse` for remote | Done (`thread_chat/src/projects.rs`, `mustr/src/home.rs`): from Home, the threads sidebar and the palette. A folder that's already a project (T3's `ProjectConflictError`) says "<folder> is already a project" and selects it | — | P0: with mustr's own engine there's no other way |
| New thread: project, agent, model | Draft composer with a project heading and model picker | `orchestration.launchThread` | Done (`thread_chat/src/new_thread.rs`) | — | P0 |
| Worktree from a base branch | Picks the base branch from a paged list, with a "Start from remote" switch (`components/WorktreeBaseBranchPicker.tsx`) | `launchThread` `workspaceStrategy: worktree{baseRef}`, `vcs.listRefs` | Done (`new_thread.rs`, `thread_settings.rs`): `vcs.listRefs` with search and "Start from remote" | — | P0: user's must-have |
| Existing worktree or branch | "New thread in this worktree"; `mod+shift+l` reuses the previous worktree | `existing_worktree{worktreePath}`, `root{branch}` | Done (`new_thread.rs`, and "New Thread on <branch>" in the thread menu, `organize.rs`); `mod+shift+l` picks the project's previous worktree on a draft (`sidebar.rs`) | — | P0: user's must-have |
| Scratch thread (no project) | Runs in `~/.t3/scratch/<date-slug>` (`docs/user/thread-sidebar.md`) | `projects.ensureScratch`, then `launchThread` | Done (`new_thread.rs`: "No project") | — | P0: 19 scratch threads in `~/.t3/scratch` this month |
| Start in background | `mod+Enter` starts the thread and opens another draft | `launchThread` | Done (`thread_chat/src/launch.rs`, `new_thread.rs`): `mod+Enter` (or `mod+alt+Enter`) on a draft launches its thread and keeps the draft, with its project, workspace and model, for the next one; a toast offers Open. The form shows the keys; on a started thread they still send | — | P2 |
| One prompt, several models | Shift-click models; one thread and worktree each (`chat/useMultipleModelSend.ts`) | `launchThread` once per model | Done (`settings_bar.rs`, `new_thread.rs`, `launch.rs`): shift-click models in a draft's model picker, in a git project; one `launchThread` per model, each in a new worktree from the base branch, in the background. The first model keeps its options; models that fail stay picked, with the prompt | — | P2 |
| Thread from a PR | "Work on PR": checks out a PR locally or in a worktree for an agent (`components/PullRequestThreadDialog.tsx`) | `git.resolvePullRequest`, `git.preparePullRequestThread` | Done (`mustr/src/pull_request_review.rs`): Work on PR ▾ in the PR header, in the checkout or a worktree | — | P0: user's must-have; the "fix the review comments" flow |
| Rename, regenerate title | Thread menu | `thread.metadata.update` `{title, regenerateTitle}` | Done (`thread_chat/src/organize.rs`): Rename in the thread menu and palette. No Regenerate title | — | P1 |
| Project settings | Default model, workspace mode, scripts, icon (`settings/ProjectSettingsPanel.tsx`) | `projects.mutate` `{project.update}`; `server.updateSettings` `{projectSettingsOverrides}` | Done (`mustr/src/project_settings.rs`): the name (`project.update`), and the model and workspace of new threads as T3's project overrides, from Home's projects, a thread's menu and the palette; scripts are listed (the terminal panel runs them). No icon | — | P2 |
| Clone a repo as a project | Clone from a forge | `projectClone.*`, `sourceControl.cloneRepository` | Done (`thread_chat/src/clone.rs`): Clone Repository… in the palette and the sidebar's add-project menu: a URL or GitHub's `owner/name`, into T3's add-project folder or `~/`, with `projectClone.start` (`source-control:write`), T3's progress from `subscribeProjectClones`, Retry and Cancel; a new thread then opens in the project. No browsing of a forge's repositories | — | P2 |
| Import Claude or Codex sessions | Imports native CLI sessions as threads | `agentSessions.scan`, `agentSessions.import` | Missing | M | Skip: a one-time step at setup |

### 2.3 Agents, models and modes

| Feature | What T3 Code does | T3 API | mustr status | Effort | Priority |
|---|---|---|---|---|---|
| Permission mode | Supervised, Auto-accept edits, Auto or Full access per thread; default in Settings (`chat/runtimeModeConfig.ts`, `docs/user/permission-modes.md`) | `launchThread.runtimeMode`, `thread.runtime-mode.set`, default from `server.getSettings` | Done (`thread_chat/src/settings_bar.rs`): T3's modes, `mod+shift+a`; new threads start from T3's default | — | P0 |
| Change model or provider mid-thread | Model picker, `mod+shift+m` (`chat/ProviderModelPicker.tsx`, `chat/ModelPickerContent.tsx`) | `thread.model-selection.set` or `message.dispatch.modelSelection`; `provider.switch` | Done (`settings_bar.rs`: `thread.model-selection.set`, which T3 plans as a switch when the agent changes) | — | P0 |
| Model options | Effort, fast mode, thinking, context window, `mod+shift+e` (`chat/TraitsPicker.tsx`) | `ModelSelection.options[{id,value}]` from provider capabilities in `server.getConfig` | Done (`settings_bar.rs`, `thread_settings.rs`) | — | P0 |
| Plan mode, Implement plan | Toggle plan mode, or type `/plan` or `/default`. The plan card offers Implement, Refine, Implement in a new thread, Copy and Save to workspace (`chat/ProposedPlanCard.tsx`, `chat/useProposedPlanSend.ts`) | `thread.interaction-mode.set`; `message.dispatch` with `sourcePlanRef`; `thread.create` for a new thread | Done (`settings_bar.rs` Build or Plan, `slash_commands.rs`, the plan card in `timeline.rs`). Save to workspace writes the plan into the thread's folder when it's on this machine (`thread_chat/src/plan_save.rs`; T3's `projects.writeFile` needs `filesystem:write`, which mustr doesn't ask for), and is off for a folder on T3's machine | — | P0 |
| Provider status | Banner when an agent is signed out or outdated (`chat/ProviderStatusBanner.tsx`) | `server.getConfig`, `subscribeServerConfig` | Done (`thread_chat/src/usage.rs`); Home says when no agent can run. Both have Set up, which opens the agents page | — | P1 |
| Restart agent session | Picks up new skills or MCP servers (command palette) | `provider-session.detach` | Done (`thread_chat/src/history.rs`: header menu and palette); asks first while the agent works | — | P1 |
| Custom models, model visibility | `settings/CustomModelEditor.tsx`, `modelPickerVisibility.ts` | `server.updateSettings` | Partial: custom models on the agents page (`mustr/src/agent_setup.rs`), by id, without per-model options. The settings bar's model picker hides the models T3's settings hide and lists the rest in their order (`thread_settings::shown_agents`); the schedule editor's and T3 settings' pickers still list every model | S | P2 |
| Goals | `/goal …` keeps the agent working until done, with a goal line | Message text; shell `goal` | Done (`slash_commands.rs`, `composer.rs`): `/goal` in the slash menu, and the goal line above the composer with T3's titles and usage, Resume and Clear | — | P2 |

### 2.4 Composer

| Feature | What T3 Code does | T3 API | mustr status | Effort | Priority |
|---|---|---|---|---|---|
| Send, queue, steer, stop | Enter sends; follow-up behavior; `mod+Enter` does the opposite | `message.dispatch` (`dispatchMode`, `deliveryIntent`), `thread.stop` | Done (`thread_chat/src/composer.rs`, `commands.rs`) | — | P0 |
| Image attachments | Paste, drag or pick images, up to 10 MiB each (`chat/ComposerAttachmentStrip.tsx`, `chat/useComposerAttachmentIntake.ts`) | `attachments.createUploadUrl`, then `POST /api/attachments/upload/:token`, then `message.dispatch.attachments` | Done (`thread_chat/src/attachments.rs`, `t3_client/src/attachments.rs`): paste, drop or pick, with progress, Retry and remove | — | P0: screenshots are a daily input |
| File attachments, big pastes | Up to 100 files at 50 MiB each; pastes of 32 KiB or more become files (`chat/composerAttachmentFiles.ts`) | Same, with `type: "file"` | Done (`thread_chat/src/attachments.rs`): any file, dropped, picked or pasted, up to 100 at 50 MiB (or the server's limit), as tiles with progress, Retry and remove, and chips on sent messages; pastes of 32 KiB or more go as `pasted-text.txt` (`mod+shift+v` pastes text). On servers with `inlineMessageContext`, images and files also go as T3's image and file context records | — | P1 |
| @-mentions and context chips | `@` for files, folders and threads, and `#` for PRs; terminal excerpts and review comments show as inline chips (`chat/useComposerMenuItems.ts`, `components/ContextChip.tsx`) | `projects.searchEntries` (needs `filesystem:read`), `pullRequests.list`; `message.dispatch.context` | Done: `@` and `#` with chips (`thread_chat/src/mentions.rs`; files on this machine through Zed's fuzzy matcher, `projects.searchEntries` when T3's folders are elsewhere), and terminal-excerpt and review-comment chips (`composer.rs`, through `AddComposerContext`), sent as T3's context records and kept in drafts | — | P1 |
| Slash commands, skills | A `/` menu and `$skill`. T3's own commands are `/model`, `/plan` and `/default`; providers add `/compact` and `/goal` (`chat/ComposerCommandMenu.tsx`, `providerSkillSearch.ts`) | Skills and commands in `server.getConfig` providers; `message.dispatch` | Done (`thread_chat/src/slash_commands.rs`) | — | P1 |
| Drafts kept per thread | Drafts survive closing and reloading (`composerDraftStore.ts`) | Local | Done (`composer.rs`): per thread, and across restarts in Zed's key-value store | — | P1: PLAN's "drafts are never lost" |
| Recall a sent prompt | ArrowUp in an empty composer (`chat/composerPromptHistory.ts`) | Local | Done (`composer.rs`) | — | P1 |
| Prompt stash | `mod+S` saves a prompt for later (`promptStashStore.ts`) | Local | Done (`thread_chat/src/stash.rs`): `mod+s` stashes the prompt with its chips and attachments, and brings back the only one, or shows the stash (newest first, at most 20, kept across restarts without attachments) | — | P2 |
| Quote a response | Select text, then Cite in composer (`chat/AssistantSelectionToolbar.tsx`) | T3's citation link in `message.dispatch.text` (`[Assistant quote](t3-citation://v1/…)`) | Done (`thread_chat/src/quote.rs`): Quote on an agent message puts the selection (or the whole message) in the composer as T3's citation, drawn as a chip; clicking one in the timeline shows the quoted message | — | P2 |
| Rich Markdown editing | Styled lists and code blocks as you type (`components/ComposerPromptEditorTiptap.tsx`) | — | Done (`thread_chat/src/formatting.rs`): the composer highlights Markdown and continues lists, with T3's `mod+b` (a selection only), `mod+i` and `mod+e`. The marks stay visible, as in any editor | — | P2 |
| Voice input | iPhone only | — | — | — | Skip: mobile only |

### 2.5 Runs and the queue

| Feature | What T3 Code does | T3 API | mustr status | Effort | Priority |
|---|---|---|---|---|---|
| Promote or cancel queued | "Up next" list (`chat/QueuedRunsControl.tsx`) | `queued-message.promote-to-steer`, `queued-run.cancel` | Done (`composer.rs` `render_queue`) | — | P0 |
| Resume | Resume in an empty composer restarts a held queue or continues an interrupted or limited turn (`chat/useComposerResume.ts`) | `queue.resume`; `message.dispatch` with `manualContinuationOfRunId` | Done (`composer.rs`) | — | P0 |
| Edit a queued message | Pencil opens it in the composer (`chat/queuedMessageEdit.ts`) | `queued-run.edit` | Done (`composer.rs`) | — | P1 |
| Reorder the queue | Drag handle | `queued-run.reorder` | Done (`composer.rs`): drag and arrows, when the agent's session takes queued messages | — | P2 |
| Send a queued message now | "Send now" on a queued message | `queued-run.start` *(0.1.3)* | Done (`composer.rs`): while nothing runs, on 0.1.3 or later (hidden on older servers) | — | P2 |
| Usage-limit recovery | Resume at reset, or snooze until reset (`chat/UsageLimitRecoveryBanner.tsx`) | `thread.metadata.update` `{limitRecovery}`, `thread.snooze`; shell `usageLimitResetAt` | Done (`thread_chat/src/usage.rs`): Resume at reset, Snooze until reset, and Resume now | — | P1 |
| Worktree setup progress, retry | Setup card with retry and cancel (`chat/WorktreeSetupCard.tsx`) | `subscribeWorktreeSetup`, `prepared-run.retry`, `worktreeSetup.cancel` | Done (`thread_chat/src/worktree_setup.rs`): the card above the timeline with stages, the script's last lines, Details, Cancel, and Retry for a failed preparation; Open terminal shows T3's setup terminal in the panel, and Work locally cancels the setup and sends the thread's first message again in the project's checkout | — | P1 |

### 2.6 Approvals, questions and secrets

| Feature | What T3 Code does | T3 API | mustr status | Effort | Priority |
|---|---|---|---|---|---|
| Approve, approve for session, decline | Inline approval panel (`chat/ComposerPendingApprovalPanel.tsx`) | `runtime-request.respond` `{decision}` | Done (`composer.rs` `render_pending`) | — | P0 |
| Answer questions | Options plus free text; dismiss (`chat/ComposerPendingUserInputPanel.tsx`) | `runtime-request.respond` `{answers}`, `thread.user-input.dismiss` | Done (`composer.rs`) | — | P0 |
| Decline with feedback | Typing a reply declines a Claude approval and tells the agent why | `runtime-request.respond` `{feedback}` *(0.1.3)* | Done (`composer.rs`): Decline with feedback when the request `supportsFeedback`; older servers queue the reply as before | — | P1 |
| Secret requests | The agent asks for a token; a masked input answers it (`chat/SecretRequestCard.tsx`) | `secrets.answerRequest` | Done (`timeline.rs` card, `composer.rs`) | — | P1: the agent waits until it's answered |
| Attachments in answers | `questionAttachments.ts` | `runtime-request.respond.attachmentsByQuestionId` *(0.1.3)* | Done (`thread_chat/src/answers.rs`): the composer answers the question it's on with its text, images and files (Next, then Answer), on servers with `questionAttachments`; older ones take text only, and say so | — | P2 |
| MCP elicitation | Request kinds `mcp-elicitation` and `permission` | `runtime-request.respond` | Done (`answers.rs`, `composer.rs`): as T3 asks them, an App access approval with the app's name, the prompt as prose, the reason, and the agent's options with their warnings; ACP agents' MCP forms come as questions, which take typed answers | — | P2 |

### 2.7 Timeline

| Feature | What T3 Code does | T3 API | mustr status | Effort | Priority |
|---|---|---|---|---|---|
| Messages, reasoning, plans, todos, tools, file diffs | `chat/MessagesTimeline.tsx`, `chat/TimelineWorkGroupRows.tsx`. T3 shows changed files and stats, not inline diffs | `orchestration.subscribeThread`, `getTurnItem` | Done (`model.rs`, `timeline.rs`) | — | P0 |
| Context meter, compaction | Used tokens, the auto-compact threshold and a Compact context button (`chat/ContextWindowMeter.tsx`) | `message.dispatch` with `/compact` | Done (`header.rs` meter, `history.rs` Compact context) | — | P1 |
| Images in messages | Thumbnails and a zoom dialog for attachments and tool screenshots (`chat/ZoomableImage.tsx`, `chat/ExpandedImageDialog.tsx`) | `assets.createUrl`, then `GET /api/assets/*` | Done (`attachments.rs`, `timeline.rs`) | — | P0 (goes with attachments) |
| Live command output | Output streams while a command runs (`chat/CommandOutputPanel.tsx`) | `orchestration.subscribeCommandOutput` | Done (`timeline.rs`, while expanded) | — | P1 |
| Copy a message or code block | `chat/MessageCopyButton.tsx` | — | Done (`timeline.rs`) | — | P1 |
| Find in thread | `mod+F` (`chat/ThreadFindBar.tsx`) | `orchestration.searchThread` | Done (`thread_chat/src/find_bar.rs`): `mod+f` opens Zed's search bar over the thread, with T3's matches across the whole history highlighted, or what's loaded on servers without the call. It scrolls inside a long message to the match, and a message-search match in the sidebar opens the thread with find on the query (`thread_chat::open_thread_with_find`) | — | P1 |
| Subagents | The subagent bar in a child ("Open parent") and the parent's lineage list, to open or stop a child (`chat/ProviderSubagentBar.tsx`, `chat/ThreadRelationshipsControl.tsx`) | On 0.1.1: shell `lineage` (`parentThreadId`, `rootThreadId`, `relationshipToParent: subagent`), the subagent item's `childThreadId`, `subagent.stop` `{runId, subagentId}`. *(0.1.3)* adds `orchestration.getThreadFamily` | Done (`thread_chat/src/family_bar.rs`): Open parent in a child; a parent's children with Open and Stop, from `getThreadFamily` merged behind the shell's `lineage`, or the `lineage` alone on 0.1.1 | — | P0: user's must-have |
| "New since last visit" | Visit watermark (`chat/ThreadVisitWatermark.tsx`) | `thread.visit` | Done (`sidebar.rs` records visits; `timeline.rs` draws the line above what began after the last one) | — | P1 |
| Markdown extras | Mermaid, math, tables, file links that open the file (`chat/MarkdownMermaidCodeBlock.tsx`, `chat/MarkdownFileLink.tsx`) | — | Done but math (`thread_chat/src/file_links.rs`): links to files and paths in inline code (T3's rule) open in mustr's editor at the line, or the thread's review when its folder is on T3's machine; web links open in the browser. Tables and Mermaid through Zed's Markdown; no math | — | P2 |
| Inline charts, HTML renders | `chat/HtmlRenderFrame.tsx` (agents' `html_render`, `html_preview`) | `assets.createUrl` | Done (`thread_chat/src/html_renders.rs`): a card with the render's title and Open in browser, which loads it as T3's frame does into a temporary file | — | P2: mustr has no HTML engine; open in the system browser |
| Minimap, history paging | `chat/TimelineMinimap.tsx`; history pages | `GET /api/orchestration/threads/:id/history` | Partial: no minimap. Paging isn't needed: mustr reads the whole thread (`threadSnapshot`, unbounded), so nothing is left to load | M | P2 |
| MCP apps | `chat/McpAppFrame.tsx` | `mcpApps.*` | Missing | L | Skip: needs a webview |

### 2.8 Checkpoints, revert and fork

| Feature | What T3 Code does | T3 API | mustr status | Effort | Priority |
|---|---|---|---|---|---|
| Edit from here, revert | Rewinds to before a message, keeping or restoring files (`chat/useTimelineRewind.ts`) | `checkpoint.rollback` `{checkpointId, restoreFiles}` | Done (`thread_chat/src/history.rs`) | — | P1 |
| Fork a thread | "Fork from this response" on an assistant message (`chat/TimelineAssistantRows.tsx`) | `thread.fork` | Done (`history.rs`): forks and opens the fork | — | P1 |
| Merge back | Brings a fork's result back to its parent | `thread.merge_back` | Done (`mustr/src/thread_handoff.rs`): Merge back in a fork's toolbar, from its latest finished run into the thread it came from, which then opens; T3 hands the work over with the parent's next message | — | P2 |
| Hand off to another provider | Moves context to a new provider (`docs/user/portable-handoffs.md`) | `provider.switch`, `thread.fork` with `portable_context` | Done (`thread_handoff.rs`): the toolbar's Hand off ▾ switches the thread to another agent (`thread.model-selection.set`, as T3's model picker does) when its agent can hand off, and says the next message's handoff is pending until it's sent | — | P2 |

### 2.9 Diffs and reviewing agent work

| Feature | What T3 Code does | T3 API | mustr status | Effort | Priority |
|---|---|---|---|---|---|
| Whole-thread changes | Branch vs. base, uncommitted included (`components/DiffPanel.tsx`) | `review.getDiffPreview`, `orchestration.getFullThreadDiff` | Done: local git when the worktree is here (`mustr/src/worktree_git.rs`, `mustr/src/agent_review.rs`) | — | P0 |
| Per-turn diff | Turn picker in the diff panel | `orchestration.getTurnDiff` | Done (`agent_review.rs`) | — | P0 |
| Diff comments to the agent | Review comments become composer chips (`reviewCommentContext.ts`) | `message.dispatch` (`context`: review-comment) | Done (`agent_review.rs`): Send to agent sends T3's review-comment records, one per comment | — | P0 |
| Diff from the thread | The chat header toggles the diff panel (`chat/ChatHeader.tsx`) | — | Done (`mustr/src/thread_toolbar.rs`: Review changes) | — | P0 |
| Open the worktree in mustr's editor | T3 has a file tree, file picker, content search and editable file viewer (`components/files/`) | `projects.listEntries`, `searchEntries`, `searchContents`, `readFile`, `writeFile` | Done (`mustr/src/worktrees.rs`): Open adds the worktree to the window's project and shows the file finder | — | P1 |
| Uncommitted-only diff | Diff panel source `working-tree`, besides branch and turn (`diffPanelStore.ts`) | `review.getDiffPreview` | Done (`agent_review.rs`): Uncommitted beside Changes and the turns, read with git (the project's checkout for a thread without a worktree), or T3's `working-tree` diff when the folder is on T3's machine | — | P2 |

### 2.10 Worktrees and branches

| Feature | What T3 Code does | T3 API | mustr status | Effort | Priority |
|---|---|---|---|---|---|
| A worktree per thread | Created when the thread launches | `launchThread` | Done | — | P0 |
| Remove worktrees | Offers removal of an unshared worktree on archive or delete; automatic cleanup rules (`worktreeCleanup.ts`, `settings/StorageSettings.tsx`) | `vcs.removeWorktree`; whether a worktree is shared: `orchestration.getWorktreeThreadIds` *(0.1.3)*, or on 0.1.1 the shell threads with the same `worktreePath`; `server.runStorageCleanup` | Done (`mustr/src/worktrees.rs`): the worktrees page, and on archive; T3's automatic cleanup rules and Delete now above the list (`worktree_cleanup.rs`, `server.updateSettings`, `server.runStorageCleanup`, `server.getStorageCleanupReport`) | — | P0: user's must-have; about 150 T3 worktrees on this machine |
| Switch or create a branch | `mod+shift+g` (`components/BranchPicker.tsx`) | `vcs.listRefs`, `vcs.switchRef`, `vcs.createRef` | Done (`mustr/src/thread_git.rs`: the toolbar's branch picker, `mod+shift+g`) | — | P0: user's must-have |
| Git status | Ahead, behind and dirty for the git actions | `subscribeVcsStatus`, `vcs.refreshStatus` | Done (`thread_git.rs`) | — | P0: user's must-have (feeds §2.11) |
| Worktree handoff | Moves a thread between the checkout and a worktree | `thread.metadata.update` `{worktreePath, branch}` | Done (`thread_git.rs`): the branch picker moves a thread into the worktree a branch is checked out in, or back to the project's checkout, as T3's does; a thread in the checkout moves to a new worktree on a new branch (`vcs.createWorktree`, as T3's `t3_worktree_handoff`). Not while the agent runs | — | P2 |
| Project scripts | Run `dev` or `test` from `t3.json`; `runOnSettle` (`components/ProjectScriptsControl.tsx`) | `terminal.open`; `projects.mutate` scripts | Done (`mustr/src/terminal_panel.rs`, `project_scripts.rs`): Run ▾ runs the scripts T3's settings resolve and `t3.json`'s in a new terminal; Add action…, the pencil and the plus add, edit, remove and import them (`server.updateSettings` with the project's override, as T3's app saves them), with the worktree and `runOnSettle` roles. No keybindings or previews (mustr has no preview browser) | — | P2 |

### 2.11 Git and pull requests

| Feature | What T3 Code does | T3 API | mustr status | Effort | Priority |
|---|---|---|---|---|---|
| Commit, push, create PR | One control for commit, push and PR, with progress. T3 writes the commit message and PR text, and links the PR to the thread (`components/GitActionsControl.tsx`) | `git.runStackedAction` (stream; `commit`, `push`, `create_pr`, `commit_push`, `commit_push_pr`), `subscribeVcsStatus` | Done (`mustr/src/thread_git.rs`): Commit ▾ in the thread toolbar and the agent review header | — | P0 |
| The thread's PR | PR chip with checks in the header and sidebar (`components/ThreadStatusIndicators.tsx`, `chat/ThreadDetailsPrRow.tsx`) | Shell `linkedPullRequest`, `branchPullRequest`; `pullRequests.summary`, `pullRequests.checks` | Done (`mustr/src/thread_toolbar.rs`): opens mustr's review; sidebar rows show a PR chip (`sidebar.rs`) | — | P0 (with the row above) |
| Link or unlink a PR | `pullRequest/LinkPullRequestDialog.tsx` | `thread.pull-request.link`, `thread.pull-request.unlink` | Done (`thread_toolbar.rs`) | — | P0: user's must-have |
| Watch a PR | The agent reacts to CI and comments | `thread.pull-request.watch` | Done (`thread_toolbar.rs`) | — | P0: user's must-have |
| A PR's threads | `pullRequest/PullRequestThreadLinks.tsx` | `pullRequests.linkedThreads` | Done (`mustr/src/pull_request_review.rs`): Threads in the PR review header, opening each | — | P1 |
| Merge, ready, close | `pullRequest/PullRequestDetailActions.tsx` | `pullRequests.runAction` | Done (`mustr/src/pull_request_review.rs`): in the PR header, confirming merges and closes | — | P0: user's must-have |
| PR inbox, review, comments, submit | `pullRequest/PullRequestsPage.tsx` | `pullRequests.*` | Done: mustr's core (`review/`, `mustr/src/home.rs`) | — | — |
| Review guides | `pullRequest/PullRequestGuideTab.tsx`; agents' `write_pull_request_guides` | `pullRequests.reviewGuide`, `generateReviewGuide` | Done (`mustr/src/pull_request_guide.rs`): Guide in the PR review's header opens the stops, `j` and `k` step, files scroll the review; Write guide and Rewrite ask T3's agent (operate), followed every five seconds | — | P2 |
| Reviewers, labels, title; stacks | `pullRequest/PullRequestReviewerPicker.tsx`, `PullRequestStack*.tsx` | `requestReviewers`, `setLabels`, `update`, `stack` | Done (`mustr/src/pull_request_details.rs`): the PR header's reviewers and labels with pickers that apply each pick (`reviewerCandidates`, `labelCandidates`), Edit title, and the stack's layers, each opening its review. Not the stack's Merge stack and Rebase stack, nor reviewers' verdicts | — | P2 |
| Source control accounts | Hosts, tokens, writing style (`settings/SourceControlSettings.tsx`) | `server.discoverSourceControl`, `server.updateSettings` | Done (`mustr/src/source_control.rs`, `t3_settings.rs`): each forge's CLI and account on T3's machine (and `gh`'s logins), T3's sign-in instructions with the command to copy (mustr never runs `gh auth`), git and jj, and the writing style. No saved tokens or per-host account choice | — | P2: set once; T3 reads `gh` and `glab` auth |

### 2.12 Terminals and editors

| Feature | What T3 Code does | T3 API | mustr status | Effort | Priority |
|---|---|---|---|---|---|
| Open in editor | Cursor, VS Code, Zed and others for the thread's folder (`chat/OpenInPicker.tsx`) | `shell.openInEditor`; editors from `server.getConfig` | Done (`thread_git.rs`: Open in ▾ in the thread toolbar and the agent review header, `mod+o`) | — | P0: the way out for anything mustr lacks |
| Terminal per thread | A drawer in the thread's worktree that stays open (`components/ThreadTerminalDrawer.tsx`, `components/PersistentThreadTerminals.tsx`) | `terminal.open/attach/write/resize/close`, `subscribeTerminalEvents` (`terminal:*` scopes) | Done (`mustr/src/terminal_panel.rs`, `remote_terminal.rs`): Zed terminals in the thread's worktree, kept per thread, `mod+j`; on a T3 on another machine, T3's terminals (`terminal.attach`, `write`, `resize`, `close`) in the same panel, found again while T3 runs them, with mouse reports to full-screen programs and an IME's text | — | P0: user's must-have |
| Terminal excerpt to the composer | `chat/TerminalContextInlineChip.tsx` | `message.dispatch.context` (terminal) | Done (`terminal_panel.rs`): Add to chat on a selection (`mod+>`) adds T3's terminal record to the thread's composer as a chip | — | P2 |

### 2.13 Preview browser and devices

| Feature | What T3 Code does | T3 API | mustr status | Effort | Priority |
|---|---|---|---|---|---|
| Preview browser | An Electron webview on desktop, or the server's headless Chrome streamed into a panel; the agent drives it too (`preview/PreviewPanel.tsx`, `browser/ServerBrowserSurface.tsx`) | `preview.*`, `subscribePreviewEvents`, `/api/preview-stream/*` (`preview:operate`) | Partial: a preview tool's page (`preview_open`, `preview_navigate`) has Open in browser on its tool call (`html_renders.rs`); no preview panel | L | P2: agents' `preview_*` tools run headless without a client; open URLs in the system browser |
| Local dev servers | Finds listening ports (`preview/useDiscoveredLocalServers.ts`) | `subscribeDiscoveredLocalServers` | Done (`mustr/src/local_servers.rs`): the thread toolbar shows the servers its terminals run, each opening in the system browser when T3 runs here | — | P2 |
| Device emulation | iOS and Android simulators (`components/device/`) | `device.*`, `subscribeDeviceState` | Missing | L | Skip: only for mobile app work; agents' `device_*` tools still work |

### 2.14 Delegation, workflows and scheduled tasks

| Feature | What T3 Code does | T3 API | mustr status | Effort | Priority |
|---|---|---|---|---|---|
| Orchestrator MCP for agents | 63 tools at `/mcp`: threads, queue, PRs, preview, schedules (`apps/server/src/mcp/toolkits/`) | Served by the server | Done: needs no client | — | — |
| Delegated tasks, workflows | Delegated-task rows, background work with Stop, and workflow rows (`chat/ThreadLineageWorkflowRow.tsx`, `chat/WorkflowOpeningBlock.tsx`, `chat/ThreadNow.tsx`) | Shell `pendingBackgroundTasks`, `blockedOnThreadWait`; `orchestration.getWorkflowScript`, `orchestration.stopWorkflow`; `thread.stop` | Done (`thread_chat/src/family_bar.rs`): delegated tasks, workflows with their script, and background work, each with Stop | — | P0: user's must-have |
| Outside agents | Claude Code or Codex in your own terminal sign in to T3's `/mcp`; you approve them at `/connect-agent` (`components/auth/ConnectAgentSurface.tsx`) | `/oauth/mcp/*` | Done (`thread_chat/src/outside.rs`): Connect Outside Agent in the palette shows T3's MCP URL and its add and login commands for Claude Code and Codex, to copy; the agent's login opens the server's own sign-in page in the browser | — | P2 |
| Scheduled tasks | List, edit, enable, run now and delete tasks on an interval, a fixed time or a webhook, with the webhook's delivery log (`settings/ScheduledTasksSettings.tsx`, `chat/ThreadAutomationsPanel.tsx`) | `scheduledTasks.subscribe`, `upsert`, `setEnabled`, `runNow`, `delete`; webhooks read-only: `listWebhookDeliveries`, `getWebhookDelivery` | Done (`mustr/src/schedules.rs`): a page from the rail; webhook tasks are read-only, with their delivery log | — | P0: user's must-have; agents also create them with `schedule_task` |

### 2.15 Thread organization

| Feature | What T3 Code does | T3 API | mustr status | Effort | Priority |
|---|---|---|---|---|---|
| Sections: pinned, active, snoozed, settled | One list in sections Pinned, Active, Working (beta), Snoozed and Settled, with status pills and unread dots (`components/Sidebar.tsx`, `sidebar/SidebarThreadRow.tsx`) | Shell `settledAt`, `settledOverride`, `pinnedAt`, `snoozedUntil`, `lastVisitedAt`; `orchestration.getOlderSettledThreads` *(0.1.3)* | Done (`thread_chat/src/sidebar.rs`): T3's sections, orders and status pills, without the beta Working section | — | P0 |
| Project filter or grouping | "Filter by project"; grouping by project is an option (`components/LegacySidebar.tsx`, Settings → General → Sidebar) | Shell `projectId` | Done (`sidebar.rs`, `organize.rs`): Filter by project. No grouping | — | P1 |
| Settle, un-settle | Settle from the menu or `mod+shift+s`, which then moves to the next unsettled thread (`settleAndAdvance.ts`); auto-settle per thread | `thread.settle`, `thread.unsettle`, `thread.auto-settle.set` | Done (`thread_chat/src/organize.rs`): menu, row button, palette, and `mod+shift+s` settle-and-advance; per-thread Auto-Settle in the thread menu | — | P0 |
| Archive | Thread menu; archived list in Settings (`settings/ArchivedThreadsPanel.tsx`) | `thread.archive`, `thread.unarchive`, `orchestration.subscribeArchivedShell` | Done (`organize.rs`): archive with Undo, offering to remove the worktree; the archived list, to unarchive, from the sidebar's header and the palette | — | P0 (archive) / P2 (archived list) |
| Pin and reorder | Pinned threads stay on top | `thread.pin`, `thread.unpin`, `thread.pin.reorder`, `thread.active.reorder` | Done (`organize.rs`, `sidebar.rs`): pin and unpin, and dragging rows to reorder Pinned and Active with T3's order keys | — | P1 |
| Unread | Dots for new activity; mark unread | `thread.visit`, `thread.mark-unread`; shell `lastVisitedAt` | Done (`sidebar.rs`, `organize.rs`): unread titles are bold; Mark Unread | — | P1 |
| Search threads | `mod+K` by title, branch, PR or project, then message matches (`components/CommandPaletteThreadItems.tsx`) | Local filter, then `orchestration.searchThreads` | Done (`sidebar.rs`): the sidebar's search (`mod+alt+k`) by title, branch, project or PR, then message matches (`orchestration.searchThreads`, one search at a time) with the snippet, a subagent's on its parent's row; title matches alone on servers without the call | — | P1 |
| Snooze, wake | `components/CustomSnoozeDialog.tsx` | `thread.snooze`, `thread.unsnooze` | Done (`organize.rs`, `thread_chat/src/snooze.rs`): T3's presets, the custom dialog (a date and time, or a duration), and Wake | — | P2 |
| Delete; bulk select; drag to settle | `components/Sidebar.drag.ts`, `threadSelectionStore.ts` | `thread.delete` and the commands above | Done (`thread_chat/src/bulk.rs`, `sidebar.rs`): Delete Thread… in the thread menu and the palette, after asking, offering to remove a worktree no other thread uses (not while the agent runs: T3 refuses, and mustr doesn't stop it); mod-click and shift-click select rows, and their menu has T3's Unpin, Settle, Snooze, Mark Unread and Delete for them all; a row dragged onto a section pins, unpins, settles, un-settles or wakes it | — | P2 |
| Undo | `mod+z` undoes settle, unpin, snooze and archive for 5 seconds (`hooks/threadUndo.ts`) | The inverse commands | Done (`organize.rs`): a toast and `mod+z` for 5 seconds, with T3's undo plans, also from an empty composer (with text in it, `mod+z` is the editor's) | — | P2 |
| Hover card, details panel | `components/ThreadHoverCard.tsx`, `chat/ThreadDetailsPanel.tsx` | — | Done (`thread_chat/src/details.rs`): T3's hover card on sidebar rows, and Thread Details (the row menu and the palette): project, path, branch, model, permissions, times, PR, the thread it came from, and its ID, to copy. The panel's git actions are the thread toolbar's | — | P2 |
| Copy branch, path, thread ID, PR link | Thread menu and palette (`components/threadActionMenu.logic.ts`) | — | Done (`organize.rs`): path, branch, thread ID and PR link | — | P2 |

### 2.16 Notifications

| Feature | What T3 Code does | T3 API | mustr status | Effort | Priority |
|---|---|---|---|---|---|
| Desktop notifications | An OS notification, a sound and an in-app toast when a thread completes, needs approval or input, fails or hits a limit (`components/ThreadNotificationCoordinator.tsx`, `threadNotifications.ts`); off by default | Shell stream | Done (`mustr/src/notifications.rs`): toasts, OS notifications when no mustr window has focus (on macOS, from the app bundle), sounds, a switch per event | — | P1 |
| Badge, attention count | App badge with the number of threads waiting on you | Shell stream | Done (`notifications.rs`): the status bar's count and the macOS Dock badge | — | P1 |
| Mobile push | `apps/mobile`, `docs/user/mobile-notifications.md` | Sent by the server through T3 Connect | n/a: comes from the server, not the client | — | Skip |

### 2.17 Keys, settings, providers and usage

| Feature | What T3 Code does | T3 API | mustr status | Effort | Priority |
|---|---|---|---|---|---|
| Thread and composer keys | Model `mod+shift+m`, effort `mod+shift+e`, access `mod+shift+a`, workspace `mod+shift+x`, branch `mod+shift+g`; previous and next thread `mod+shift+[` and `]`; diff `mod+d`; open in editor `mod+o` (`packages/shared/src/keybindings.ts`) | Local; `server.upsertKeybinding` for T3's own store | Done: send, queue, steer, model, effort, access, settle, search (`mod+alt+k`), terminal (`mod+j`) (`thread_chat/src/thread_chat.rs` `key_bindings`, `mustr/src/terminal_panel.rs`); diff (`mod+d`), open in editor (`mod+o`) and branch (`mod+shift+g`) (`mustr/src/thread_toolbar.rs`); workspace (`mod+shift+x`), previous and next thread (`mod+shift+[` and `]`) and the previous worktree (`mod+shift+l`) (`thread_chat/src/sidebar.rs`); find (`mod+f`, `find_bar.rs`) and undo (`mod+z`, `organize.rs`) | — | P1: mustr uses Zed's `keymap.json`, so no sync with T3 |
| Command palette | Thread actions, new thread in…, settle, usage | — | Done: thread actions (settle, archive, pin, rename, compact, restart, usage), terminals, worktrees and schedules are in Zed's palette, and Review changes, Open in, the branch picker, Commit push and PR, and Link PR on the open thread (`thread_toolbar.rs`). T3's "New thread in <project>" for each project (the open thread's first), New thread without a project, Find in thread, Copy thread ID, Delete thread, Thread details, Clone repository and Connect outside agent too (`thread_chat/src/palette.rs`, through mustr's palette interceptor in `mustr/src/hidden_actions.rs`) | — | P1 |
| T3 settings | Thread defaults, auto-settle rules, follow-up behavior, branch naming (`settings/SettingsPanels.tsx`) | `server.getSettings` (or `server.getConfig`), `server.updateSettings` (`settings:write`) | Done (`mustr/src/t3_settings.rs`): new threads' model, permissions, workspace and remote start; auto-settle; branch naming. Follow-up behavior is a setting of T3's app, so mustr keeps its own (`thread_settings.rs`) | — | P1 |
| Install and sign in to agents | Install CLIs, log in (`settings/ProviderSetupSection.tsx`, `settings/ProviderAuthTerminal.tsx`, `onboarding/`) | `provider.install.*`, `provider.auth.*` (`providers:manage`) | Done (`mustr/src/agent_setup.rs`): install, update and remove T3's own Codex and Antigravity, sign in and out (browser, device code, terminal, credentials), T3's install and sign-in commands for agents on their own CLI, "Let T3 Code install Codex", Turn on, Check again. The sign-in terminal is Zed's terminal (`mustr/src/auth_terminal.rs`): T3's PTY output by offset, keys, pastes and its size sent back in order | — | P1: CLI logins live on disk and are shared |
| Usage limits | 5-hour and weekly windows per account (`chat/ComposerUsageLimits.tsx`, `components/usage/UsageLimits.tsx`) | Provider status in `server.getConfig` and `subscribeServerConfig`, `server.refreshProviders`; `provider.consumeResetCredit` | Done (`thread_chat/src/usage.rs`): the header's windows and a usage panel with reset times | — | P1 |
| Usage dashboard | Tokens, cost, model breakdown, `mod+u` (`components/usage/UsagePage.tsx`) | `server.getUsageSummary` (`diagnostics:read`) | Done (`mustr/src/usage_page.rs`): the rail's Usage, the palette and `mod+u`; cost or tokens for 24 hours (by hour) or 7, 30 or 90 days, each agent's share, a chart, token totals and kinds, a breakdown by model or day, and Limits. No price overrides or several environments | — | P2 |
| Provider instances, ACP agents | Several accounts per provider; ACP registry (`settings/AddProviderInstanceDialog.tsx`) | `server.updateSettings`, `server.*AcpRegistry*` | Done (`mustr/src/agent_accounts.rs`, `agent_setup.rs`): Add an agent… adds another Codex or Claude Code account (its own label, T3's id and home folder) or an agent from the ACP registry (search, prepare, add), and Remove takes them away. No renaming, accent colors, environment variables, or ACP session import | — | P2 |
| Appearance, themes | `settings/AppearanceSettingsPanel.tsx` | — | n/a | — | Skip: Zed themes and mustr's theme |
| Diagnostics, storage, telemetry | `settings/DiagnosticsSettings.tsx`, `settings/StorageSettings.tsx` | `server.getProcessDiagnostics`, `server.runStorageCleanup` | Missing | M | Skip: maintenance; use T3's CLI |

### 2.18 Sharing and mobile

| Feature | What T3 Code does | T3 API | mustr status | Effort | Priority |
|---|---|---|---|---|---|
| Guest share links | Read-only or chat link for a thread (`chat/GuestShareControls.tsx`, `guestPage.ts`) | `guestShare.create` (`access:write`), `/api/guest/*` | Missing | — | Skip: needs `access:write`; collaboration is a non-goal (PLAN §1) |
| Mobile app | `apps/mobile` | The same server | n/a | — | Skip: keeps working against the server when mustr pairs with the user's T3 |
| Snap Shot, browser import | Screen capture tool; cookie import for preview (`docs/user/snap-shot.md`, `docs/user/browser-import.md`) | — | Missing | — | Skip: desktop extras; browser import only matters with the preview |

**On the pin.** mustr pins 0.1.3 since 2026-10-28 and still works against 0.1.1 (the local
server); `mustr-docs/T3-PROTOCOL.md`, "Compatibility", has the rules. Where 0.1.3 adds a call,
mustr uses 0.1.1's data on servers without it:
- **Subagents:** 0.1.1's shell holds every thread, subagent children included, and each has a
  `lineage`: `parentThreadId`, `rootThreadId`, and `relationshipToParent` (`fork` or `subagent`).
  A subagent turn item names its `childThreadId` and `subagentId`. So the family bar builds a
  thread's family from the shell it already follows, and stops a child with `subagent.stop` (or a
  delegated task with `thread.stop`).
- **On 0.1.3, `getThreadFamily`:** snapshots leave out idle subagent children only for clients
  that opt in (`omitIdleSubagents`, `packages/contracts/src/orchestrationV2.ts:3438`; T3's own
  client does), and `orchestration.getThreadFamily` serves them. mustr doesn't opt in, so its
  shell still has every child, but the family bar reads `getThreadFamily` once per thread and shell
  generation and merges it behind the shell's rows, as T3's client does, so it stays right if mustr
  opts in later. A 0.1.1 server answers "Unknown request tag", and the bar uses the shell alone.
- **Other 0.1.3-only calls:** for shared worktrees, mustr matches `worktreePath` across the shell's
  threads instead of calling `getWorktreeThreadIds`. `getOlderSettledThreads` isn't needed, since
  a shell without `omitOlderSettled` has every thread. Two rows waited for the pin, and wave 3
  built both with their fallbacks: Decline with feedback (`runtime-request.respond` `{feedback}`)
  shows only when the request says `supportsFeedback`, which 0.1.1 never sends, and Send now
  (`queued-run.start`) only on servers of 0.1.3 or later (`server.getConfig`'s version).
- **Wave 3's calls in 0.1.1:** `terminal.*`, `subscribeWorktreeSetup`,
  `pullRequests.linkedThreads`, `orchestration.searchThread` and `searchThreads`,
  `provider.install.*`, `provider.auth.*`, `server.updateSettings`, and the `thread.snooze`,
  `thread.pin.reorder`, `thread.active.reorder`, `thread.auto-settle.set`, `queued-run.reorder` and
  `prepared-run.retry` commands are all in 0.1.1 (`t3_api/src/generated/methods.rs`, `tagged.rs`).

**Counts** at v0.1.0 (125 rows):
- **P0:** 41, all done.
- **P1:** 33, all done.
- **P2:** 38. 33 are done (Markdown extras without math), 4 partial (several environments at once,
  model visibility in two pickers, the minimap, and the preview browser) and 1 missing (remote
  access).
- **Skip:** 11.
- **No priority:** 2 done rows.

**Scopes:** mustr asks for `orchestration:read`, `orchestration:operate`, `source-control:write`,
`filesystem:read`, `terminal:read` and `terminal:operate` (remote terminals), `settings:write`
(T3's settings), `providers:manage` (installing and signing in to agents) and `diagnostics:read`
(the usage dashboard) (`SCOPES` in
`mustr/src/t3_connection.rs`), each with an `Access` level (`thread_chat/src/backend.rs`).
`access:*` and `relay:*` stay out.

## 3. After v0.1.0

What's left once 0.1.0 is out: the open P2 rows, what each finished row still leaves out, and what
the release checks found. Nothing here blocks daily use; T3's own app or CLI covers each gap.

### Open rows (P2)

- **Several environments at once** (partial, L): one server is connected at a time (saved servers
  switch between them); T3 lets each new thread pick its host and connects to all of them.
- **Remote access** (missing, L): T3's SSH, Tailscale and T3 Connect setup. A server reached over
  Tailscale or SSH forwarding can already be saved by its address.
- **Model visibility** (partial, S): the schedule editor's and T3 settings' model pickers still
  list the models T3's settings hide; custom models have no per-model options.
- **Minimap** (partial, M): no timeline minimap. History paging isn't needed, since mustr reads the
  whole thread.
- **Preview browser** (partial, L): no preview panel, since mustr has no web engine; preview pages
  and HTML renders open in the system browser.
- **Math in Markdown**: Mermaid and tables render, math doesn't.

### What finished rows leave out

- **Threads:** Regenerate title; grouping the sidebar by project (only the filter); T3's beta
  Working section; browsing a forge's repositories when cloning (a URL or `owner/name` only).
- **Plans:** Save to workspace for a thread whose folder is on T3's machine (T3's
  `projects.writeFile` needs `filesystem:write`, which mustr doesn't ask for).
- **Projects:** a project's icon; keybindings and previews for project scripts.
- **Pull requests:** Merge stack and Rebase stack, and reviewers' verdicts beside the requested
  reviewers.
- **Accounts and agents:** saved forge tokens and a per-host account choice; renaming agent
  accounts, their accent colors and environment variables, and importing ACP sessions.
- **Usage:** price overrides, and usage across several environments.

### Found by the release checks

From the integrated smoke test on 2026-10-29 (mustr's engine, and the user's T3 read-only):
- **Zoom:** Zed's own command palette is 34 rems wide, so at 26 it's wider than a half-screen
  window and clipped at both sides (Zed's file). Setting `ui_font_size` alone in the settings file
  leaves code and terminals at `buffer_font_size`; the zoom keys change both.
- **Restricted Mode in reviews** (done; the user's call: trust a worktree if its project is
  trusted): a T3 worktree on this machine inherits Zed's trust from its project's checkout
  (`mustr/src/worktree_trust.rs`). When Zed trusts the checkout (or a folder holding it), the
  project's worktrees open trusted, and those already open follow when it's trusted later. The
  review's Restricted Mode button asks to trust the project, which covers its agent worktrees, or
  only the worktree. Untrusted projects still ask, and a folder that isn't the `worktreePath` of
  one of the project's threads, or isn't a linked git worktree, never inherits. As with any
  folder Zed trusts, Zed keeps an inherited worktree's path in its trusted list after T3 removes
  the worktree.
- **Zed's name:** what's left is in Zed's crates and rarely seen, and mustr can't change it
  without patching them: the Node.js download error for language servers ("restart Zed to
  retry", which goes to the log), the trust dialog's `.zed/settings.json` (where project settings
  live, kept on purpose), `${appName}` in a window title format the user writes, and the theme
  picker's View Theme Docs, which opens Zed's docs. mustr says Zed on purpose where it means Zed:
  Open in Zed (the editor), the About panel's credits, and "uses Zed's format" in the settings
  and key binding files.

Fixed before v0.1.0: tooltips that covered the menu their button opened, Zed's name in the
Restricted Mode tooltip, the command palette's "zed: …" entries and the empty pane's Welcome to
Zed page, the terminal panel coming back over home after a restart (it now comes back only with
its thread), empty scopes of an agent review shown as a failure to load, and the "isn't
connected to T3" warnings logged at launch. The report that the first `mod+j` after such a
launch didn't show the panel didn't reproduce.

### Not planned (Skip rows)

Paired clients and pairing links (they need `access:*`), importing Claude or Codex sessions, MCP
apps, device emulation, diagnostics and storage maintenance, guest share links, Snap Shot and
browser import, voice input, mobile push and the mobile app, and T3's appearance settings. §2 gives
each reason.

### macOS

mustr has been built from source and run on one Apple silicon Mac, and the release workflow builds
and packages it on GitHub's Macs. The packaged app, the installer, the updater's last steps,
keychain prompts and the menus were checked on Linux only
(`mustr-docs/MACOS.md`, "Known gaps"). Releases are signed ad hoc, not with a Developer ID, and
Intel Macs have no T3 engine.

### How v0.1.0 was built

WP0 split the thread view and added the seams (`ThreadBackend`, shared shells, cross-package
actions, rail entries, labeled slots); each later package owned its files. Wave 1 (WP1–WP7):
thread settings and new threads, the composer and timeline, the thread list and projects, the PR
flow, subagents and schedules, terminals and worktrees, and macOS. Wave 2 (WP8–WP10):
notifications, mentions and slash commands, history and usage limits. Wave 3 (WP11–WP14): T3's
terminals, setup progress and a PR's threads; attachments, chips and find; dragging, snooze and
undo; the agents page and T3's settings. Then the 2x fit pass, release packaging and the updater,
and the T3 0.1.3 pin. Wave 4 (WP15–WP18): the timeline and composer extras, git, PR details,
handoffs and cleanup, background and multi-model launches, bulk actions and the palette, and the
usage dashboard, saved servers, project settings and accounts. The commits are on `m3/next`.

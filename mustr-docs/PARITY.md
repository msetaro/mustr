# Parity with T3 Code's app for agent work

What T3 Code's web and desktop app does for agent-driven development, what mustr does today, and
what to build so a developer can stop opening T3's app.

- **Compared:** mustr at `m2/threads` (`a96368ce93`) against T3 Code `v0.1.3` (`d2c44031`, 2026-10-26).
  mustr's `t3_api` is generated from T3 0.1.1 contracts (`t3_client/T3_VERSION`), and the local
  server is 0.1.1. Calls added after 0.1.1 are marked *(0.1.3)*.
- **Paths:** T3 paths are relative to T3's `apps/web/src/` unless they start with `docs/`,
  `packages/` or `apps/`. `chat/`, `settings/`, `pullRequest/` and `preview/` are short for
  `components/chat/` and so on. mustr paths are relative to `mustr-crates/`.
- **Sources:** T3's API comes from `packages/contracts/src/rpc.ts` (`WS_METHODS`),
  `orchestrationV2.ts` (the `OrchestrationV2Command` union) and `environmentHttp.ts`. Scopes come
  from `apps/server/src/auth/RpcAuthorization.ts`.
- **Effort** (one agent): **S** is a day or less, **M** is 2 to 5 days, **L** is more than a week.
- **Priority** for a developer replacing T3's app at work: **P0** blocks the switch. **P1** is
  needed within weeks. **P2** is nice to have. **Skip** means mustr doesn't need it, with the reason
  given.
- **The user:** macOS on Apple Silicon, with Claude Code and Codex. Their must-haves are subagents
  and scheduled tasks, worktrees and terminals, and the PR flow. Those rows say "P0: user's
  must-have".

## 1. Where mustr stands

mustr already covers the core loop. It pairs with a local T3 server and lists every thread live
(`GET /api/orchestration/shell` plus `orchestration.subscribeShell`). It opens a thread's live
timeline (`orchestration.subscribeThread`) with messages, reasoning, plans, todos, tool calls whose
output loads on expand, inline file diffs, and a context meter. You can send, queue, steer and stop
from the composer, approve or decline requests, answer questions, and promote or cancel queued
messages. New threads take a project, agent and model, and run in the checkout or a new worktree.
An agent's work can be reviewed per turn or as the whole branch, with local comments and **Send to
agent**, next to full PR review. So far mustr sends 6 of the roughly 50 orchestration command types
and calls 20 of T3's 203 RPCs. Every RPC in 0.1.1 already has a typed wrapper in `t3_api`
(`t3_api/src/generated/methods.rs`), so most gaps are UI work, not protocol work.

The gaps that block daily use:
- **Thread settings:** every new thread is forced into `approval-required`, while T3 defaults to
  Full access. Model, effort and plan mode can't be changed.
- **Attachments:** there are no image attachments.
- **Organization:** the sidebar is one flat list that mixes in T3's settled and snoozed threads.
  There's no settle or archive.
- **Shipping work:** there's no commit, push or PR from a thread, and no Open in editor.
- **Projects:** you can't add a project, and there are no scratch threads. Once mustr runs its own
  engine, you'd have to open T3's app to do either.
- **Subagents and schedules:** a subagent shows as a tool row, so its thread can't be opened or
  stopped. Workflows and scheduled tasks have no UI.
- **Worktrees and terminals:** there's no terminal and no branch list. Existing worktrees can't be
  reused, branches can't be switched, and worktrees can't be removed.
- **PR flow:** a PR can't be linked or watched, a thread can't start from a PR, and a PR can't be
  merged.

Notifications, revert and fork, @-mentions, and slash commands come next (P1). Running
T3's engine inside mustr is in progress on `m3/embedded-engine` and isn't counted below.

## 2. Features by area

### 2.1 Connection and environments

| Feature | What T3 Code does | T3 API | mustr status | Effort | Priority |
|---|---|---|---|---|---|
| Pair with a local T3 | Exchanges a pairing credential for a bearer token, then opens the WebSocket | `/.well-known/t3/environment`, `POST /oauth/token`, `POST /api/auth/websocket-ticket`, `/ws` | Done (`mustr/src/t3_connection.rs`) | — | P0 |
| Run the engine in-app | The desktop app bundles and runs the server (`apps/desktop`) | `t3 serve`, `t3 auth pairing create` | Partial: in progress on `m3/embedded-engine` (`mustr-docs/T3-RUNTIME.md`) | L | P0 (owned by the engine agent, not by §3) |
| Version skew, server update | Warns on mismatch and updates the server (`versionSkew.ts`, `components/ServerUpdateAction.tsx`) | `server.updateServerWithProgress`, `subscribeServerLifecycle` | Partial: protocol is checked on connect (`t3_client/src/auth.rs:65`); no update UI | M | P2 |
| Several environments at once | One client, many servers; picks a host for each new thread (`environments/`, `settings/AddEnvironmentDialog.tsx`) | One `/ws` per environment, `server.getHostResources` | Missing: one server | L | P2 |
| Remote access (SSH, Tailscale, T3 Connect) | Reach a home machine's server from elsewhere (`packages/ssh`, `packages/tailscale`, `cloud/`) | `/api/connect/*`, `cloud.*` | Missing | L | P2: T3's own remote setup keeps working |
| Paired clients, pairing links | List and revoke sessions (`settings/AuthorizedClientsSettings.tsx`) | `/api/auth/clients*`, `/api/auth/pairing-links*`, `subscribeAuthAccess` | Missing | S | Skip: needs `access:*` scopes, which mustr never asks for (PLAN §4); use `t3 auth` |

### 2.2 Projects and new threads

| Feature | What T3 Code does | T3 API | mustr status | Effort | Priority |
|---|---|---|---|---|---|
| Add a project | Adds a local folder as a project (`components/AddProjectToMachineDialog.tsx`, `CommandPaletteAddProject.tsx`) | `projects.mutate` `{project.create}` (or `POST /api/projects/mutate`); `filesystem.browse` for remote | Missing: the form says "Add one in T3 Code first" (`thread_chat/src/new_thread.rs:393`) | S | P0: with mustr's own engine there's no other way |
| New thread: project, agent, model | Draft composer with a project heading and model picker | `orchestration.launchThread` | Done (`thread_chat/src/new_thread.rs`) | — | P0 |
| Worktree from a base branch | Picks the base branch from a paged list, with a "Start from remote" switch (`components/WorktreeBaseBranchPicker.tsx`) | `launchThread` `workspaceStrategy: worktree{baseRef}`, `vcs.listRefs` | Partial: the base ref is free text, with no branch list | S | P0: user's must-have |
| Existing worktree or branch | "New thread in this worktree"; `mod+shift+l` reuses the previous worktree | `existing_worktree{worktreePath}`, `root{branch}` | Missing: only the checkout or a new worktree | S | P0: user's must-have |
| Scratch thread (no project) | Runs in `~/.t3/scratch/<date-slug>` (`docs/user/thread-sidebar.md`) | `projects.ensureScratch`, then `launchThread` | Missing | S | P0: 19 scratch threads in `~/.t3/scratch` this month |
| Start in background | `mod+Enter` starts the thread and opens another draft | `launchThread` | Missing | S | P2 |
| One prompt, several models | Shift-click models; one thread and worktree each (`chat/useMultipleModelSend.ts`) | `launchThread` once per model | Missing | S | P2 |
| Thread from a PR | "Work on PR": checks out a PR locally or in a worktree for an agent (`components/PullRequestThreadDialog.tsx`) | `git.resolvePullRequest`, `git.preparePullRequestThread` | Missing | M | P0: user's must-have; the "fix the review comments" flow |
| Rename, regenerate title | Thread menu | `thread.metadata.update` `{title, regenerateTitle}` | Partial: T3's title is shown, but can't be renamed | S | P1 |
| Project settings | Default model, workspace mode, scripts, icon (`settings/ProjectSettingsPanel.tsx`) | `projects.mutate` `{project.update}` | Partial: the project's default model is honored (`new_thread.rs:266`) | M | P2 |
| Clone a repo as a project | Clone from a forge | `projectClone.*`, `sourceControl.cloneRepository` | Missing | M | P2 |
| Import Claude or Codex sessions | Imports native CLI sessions as threads | `agentSessions.scan`, `agentSessions.import` | Missing | M | Skip: a one-time step at setup |

### 2.3 Agents, models and modes

| Feature | What T3 Code does | T3 API | mustr status | Effort | Priority |
|---|---|---|---|---|---|
| Permission mode | Supervised, Auto-accept edits, Auto or Full access per thread; default in Settings (`chat/runtimeModeConfig.ts`, `docs/user/permission-modes.md`) | `launchThread.runtimeMode`, `thread.runtime-mode.set`, default from `server.getSettings` | Partial: shown in the header only; new threads are forced to `approval-required` (`new_thread.rs:102`) | S | P0: T3 defaults to Full access, so every mustr thread behaves differently |
| Change model or provider mid-thread | Model picker, `mod+shift+m` (`chat/ProviderModelPicker.tsx`, `chat/ModelPickerContent.tsx`) | `thread.model-selection.set` or `message.dispatch.modelSelection`; `provider.switch` | Partial: new threads only | M | P0 |
| Model options | Effort, fast mode, thinking, context window, `mod+shift+e` (`chat/TraitsPicker.tsx`) | `ModelSelection.options[{id,value}]` from provider capabilities in `server.getConfig` | Missing: `options: None` (`new_thread.rs:318`) | M | P0 |
| Plan mode, Implement plan | Toggle plan mode, or type `/plan` or `/default`. The plan card offers Implement, Refine, Implement in a new thread, Copy and Save to workspace (`chat/ProposedPlanCard.tsx`, `chat/useProposedPlanSend.ts`) | `thread.interaction-mode.set`; `message.dispatch` with `sourcePlanRef`; `thread.create` for a new thread | Partial: plans render (`thread_chat/src/model.rs` `ItemKind::Plan`); no toggle, no Implement | S | P0 |
| Provider status | Banner when an agent is signed out or outdated (`chat/ProviderStatusBanner.tsx`) | `server.getConfig`, `subscribeServerConfig` | Partial: the form notes when no agent is set up | S | P1 |
| Restart agent session | Picks up new skills or MCP servers (command palette) | `provider-session.detach` | Missing | S | P1 |
| Custom models, model visibility | `settings/CustomModelEditor.tsx`, `modelPickerVisibility.ts` | `server.updateSettings` | Missing | S | P2 |
| Goals | `/goal …` keeps the agent working until done, with a goal line | Message text; shell `goal` | Partial: the text goes through; the goal line isn't shown | S | P2 |

### 2.4 Composer

| Feature | What T3 Code does | T3 API | mustr status | Effort | Priority |
|---|---|---|---|---|---|
| Send, queue, steer, stop | Enter sends; follow-up behavior; `mod+Enter` does the opposite | `message.dispatch` (`dispatchMode`, `deliveryIntent`), `thread.stop` | Done (`thread_chat/src/thread_view.rs`, `commands.rs`) | — | P0 |
| Image attachments | Paste, drag or pick images, up to 10 MiB each (`chat/ComposerAttachmentStrip.tsx`, `chat/useComposerAttachmentIntake.ts`) | `attachments.createUploadUrl`, then `POST /api/attachments/upload/:token`, then `message.dispatch.attachments` | Missing: `attachments: Vec::new()` (`commands.rs`) | M | P0: screenshots are a daily input |
| File attachments, big pastes | Up to 100 files at 50 MiB each; pastes of 32 KiB or more become files (`chat/composerAttachmentFiles.ts`) | Same, with `type: "file"` | Missing | S (after images) | P1 |
| @-mentions and context chips | `@` for files, folders and threads, and `#` for PRs; terminal excerpts and review comments show as inline chips (`chat/useComposerMenuItems.ts`, `components/ContextChip.tsx`) | `projects.searchEntries` (needs `filesystem:read`), `pullRequests.list`; `message.dispatch.context` | Missing | M | P1 (needs a new scope) |
| Slash commands, skills | A `/` menu and `$skill`. T3's own commands are `/model`, `/plan` and `/default`; providers add `/compact` and `/goal` (`chat/ComposerCommandMenu.tsx`, `providerSkillSearch.ts`) | Skills and commands in `server.getConfig` providers; `message.dispatch` | Partial: raw text reaches the provider, so its own commands at the start of a message work; no menu, no `/model` or `/plan` | M | P1 |
| Drafts kept per thread | Drafts survive closing and reloading (`composerDraftStore.ts`) | Local | Missing: the text lives in the view | S | P1: PLAN's "drafts are never lost" |
| Recall a sent prompt | ArrowUp in an empty composer (`chat/composerPromptHistory.ts`) | Local | Missing | S | P1 |
| Prompt stash | `mod+S` saves a prompt for later (`promptStashStore.ts`) | Local | Missing | S | P2 |
| Quote a response | Select text, then Cite in composer (`chat/AssistantSelectionToolbar.tsx`) | `message.dispatch.context` | Missing | M | P2 |
| Rich Markdown editing | Styled lists and code blocks as you type (`components/ComposerPromptEditorTiptap.tsx`) | — | Partial: a plain Zed editor, so Markdown is text | M | P2 |
| Voice input | iPhone only | — | — | — | Skip: mobile only |

### 2.5 Runs and the queue

| Feature | What T3 Code does | T3 API | mustr status | Effort | Priority |
|---|---|---|---|---|---|
| Promote or cancel queued | "Up next" list (`chat/QueuedRunsControl.tsx`) | `queued-message.promote-to-steer`, `queued-run.cancel` | Done (`thread_view.rs` `render_queue`) | — | P0 |
| Resume | Resume in an empty composer restarts a held queue or continues an interrupted or limited turn (`chat/useComposerResume.ts`) | `queue.resume`; `message.dispatch` with `manualContinuationOfRunId` | Missing: a held queue restarts only when you send a new message, and an interrupted turn can't be continued | S | P0 |
| Edit a queued message | Pencil opens it in the composer (`chat/queuedMessageEdit.ts`) | `queued-run.edit` | Missing | S | P1 |
| Reorder the queue | Drag handle | `queued-run.reorder` | Missing | S | P2 |
| Send a queued message now | "Send now" on a queued message | `queued-run.start` *(0.1.3)* | Missing | S | P2 |
| Usage-limit recovery | Resume at reset, or snooze until reset (`chat/UsageLimitRecoveryBanner.tsx`) | `thread.metadata.update` `{limitRecovery}`, `thread.snooze`; shell `usageLimitResetAt` | Missing: no limited state or reset time | S | P1 |
| Worktree setup progress, retry | Setup card with retry and cancel (`chat/WorktreeSetupCard.tsx`) | `subscribeWorktreeSetup`, `prepared-run.retry`, `worktreeSetup.cancel` | Missing | S | P1 |

### 2.6 Approvals, questions and secrets

| Feature | What T3 Code does | T3 API | mustr status | Effort | Priority |
|---|---|---|---|---|---|
| Approve, approve for session, decline | Inline approval panel (`chat/ComposerPendingApprovalPanel.tsx`) | `runtime-request.respond` `{decision}` | Done (`thread_view.rs` `render_pending`) | — | P0 |
| Answer questions | Options plus free text; dismiss (`chat/ComposerPendingUserInputPanel.tsx`) | `runtime-request.respond` `{answers}`, `thread.user-input.dismiss` | Done | — | P0 |
| Decline with feedback | Typing a reply declines a Claude approval and tells the agent why | `runtime-request.respond` `{feedback}` | Missing | S | P1 |
| Secret requests | The agent asks for a token; a masked input answers it (`chat/SecretRequestCard.tsx`) | `secrets.answerRequest` | Missing: shown as a notice (`model.rs:458`) | S | P1: the agent waits until it's answered |
| Attachments in answers | `questionAttachments.ts` | `runtime-request.respond.attachmentsByQuestionId` | Missing | S | P2 |
| MCP elicitation | Request kinds `mcp-elicitation` and `permission` | `runtime-request.respond` | Partial: the generic approval card | S | P2 |

### 2.7 Timeline

| Feature | What T3 Code does | T3 API | mustr status | Effort | Priority |
|---|---|---|---|---|---|
| Messages, reasoning, plans, todos, tools, file diffs | `chat/MessagesTimeline.tsx`, `chat/TimelineWorkGroupRows.tsx`. T3 shows changed files and stats, not inline diffs | `orchestration.subscribeThread`, `getTurnItem` | Done (`model.rs`, `thread_view.rs`) | — | P0 |
| Context meter, compaction | Used tokens, the auto-compact threshold and a Compact context button (`chat/ContextWindowMeter.tsx`) | `message.dispatch` with `/compact` | Partial: the meter only (`thread_view.rs` `render_usage`) | S | P1 |
| Images in messages | Thumbnails and a zoom dialog for attachments and tool screenshots (`chat/ZoomableImage.tsx`, `chat/ExpandedImageDialog.tsx`) | `assets.createUrl`, then `GET /api/assets/*` | Missing | M | P0 (goes with attachments) |
| Live command output | Output streams while a command runs (`chat/CommandOutputPanel.tsx`) | `orchestration.subscribeCommandOutput` | Partial: fetched once on expand | S | P1 |
| Copy a message or code block | `chat/MessageCopyButton.tsx` | — | Missing | S | P1 |
| Find in thread | `mod+F` (`chat/ThreadFindBar.tsx`) | `orchestration.searchThread` | Missing | S | P1 |
| Subagents | The subagent bar in a child ("Open parent") and the parent's lineage list, to open or stop a child (`chat/ProviderSubagentBar.tsx`, `chat/ThreadRelationshipsControl.tsx`) | On 0.1.1: shell `lineage` (`parentThreadId`, `rootThreadId`, `relationshipToParent: subagent`), the subagent item's `childThreadId`, `subagent.stop` `{runId, subagentId}`. *(0.1.3)* adds `orchestration.getThreadFamily` | Partial: a tool row (`model.rs:385`); the child can't be opened or stopped | M | P0: user's must-have |
| "New since last visit" | Visit watermark (`chat/ThreadVisitWatermark.tsx`) | `thread.visit` | Missing | S | P1 |
| Markdown extras | Mermaid, math, tables, file links that open the file (`chat/MarkdownMermaidCodeBlock.tsx`, `chat/MarkdownFileLink.tsx`) | — | Partial: Zed markdown; file links don't open in mustr | M | P2 |
| Inline charts, HTML renders | `chat/HtmlRenderFrame.tsx` (agents' `html_render`, `html_preview`) | `assets.createUrl` | Missing | M | P2: mustr has no HTML engine; open in the system browser |
| Minimap, history paging | `chat/TimelineMinimap.tsx`; history pages | `GET /api/orchestration/threads/:id/history` | Partial: bounded snapshot, no minimap | M | P2 |
| MCP apps | `chat/McpAppFrame.tsx` | `mcpApps.*` | Missing | L | Skip: needs a webview |

### 2.8 Checkpoints, revert and fork

| Feature | What T3 Code does | T3 API | mustr status | Effort | Priority |
|---|---|---|---|---|---|
| Edit from here, revert | Rewinds to before a message, keeping or restoring files (`chat/useTimelineRewind.ts`) | `checkpoint.rollback` `{checkpointId, restoreFiles}` | Missing | M | P1 |
| Fork a thread | "Fork from this response" on an assistant message (`chat/TimelineAssistantRows.tsx`) | `thread.fork` | Partial: inherited items render (`model.rs` `inherited`); can't fork | S | P1 |
| Merge back | Brings a fork's result back to its parent | `thread.merge_back` | Missing | S | P2 |
| Hand off to another provider | Moves context to a new provider (`docs/user/portable-handoffs.md`) | `provider.switch`, `thread.fork` with `portable_context` | Missing | M | P2 |

### 2.9 Diffs and reviewing agent work

| Feature | What T3 Code does | T3 API | mustr status | Effort | Priority |
|---|---|---|---|---|---|
| Whole-thread changes | Branch vs. base, uncommitted included (`components/DiffPanel.tsx`) | `review.getDiffPreview`, `orchestration.getFullThreadDiff` | Done: local git when the worktree is here (`mustr/src/worktree_git.rs`, `mustr/src/agent_review.rs`) | — | P0 |
| Per-turn diff | Turn picker in the diff panel | `orchestration.getTurnDiff` | Done (`agent_review.rs`) | — | P0 |
| Diff comments to the agent | Review comments become composer chips (`reviewCommentContext.ts`) | `message.dispatch` (`context`: review-comment) | Done: batched as text (`agent_review.rs:346`) | — | P0 |
| Diff from the thread | The chat header toggles the diff panel (`chat/ChatHeader.tsx`) | — | Missing: opens only from Home's "Agent work to review" or `--review-thread` | S | P0 |
| Open the worktree in mustr's editor | T3 has a file tree, file picker, content search and editable file viewer (`components/files/`) | `projects.listEntries`, `searchEntries`, `searchContents`, `readFile`, `writeFile` | Partial: the review opens real files; no "open the worktree as a project", so Zed's file finder and search don't cover it | S | P1 |
| Uncommitted-only diff | Diff panel source `working-tree`, besides branch and turn (`diffPanelStore.ts`) | `review.getDiffPreview` | Partial: folded into Changes | S | P2 |

### 2.10 Worktrees and branches

| Feature | What T3 Code does | T3 API | mustr status | Effort | Priority |
|---|---|---|---|---|---|
| A worktree per thread | Created when the thread launches | `launchThread` | Done | — | P0 |
| Remove worktrees | Offers removal of an unshared worktree on archive or delete; automatic cleanup rules (`worktreeCleanup.ts`, `settings/StorageSettings.tsx`) | `vcs.removeWorktree`; whether a worktree is shared: `orchestration.getWorktreeThreadIds` *(0.1.3)*, or on 0.1.1 the shell threads with the same `worktreePath`; `server.runStorageCleanup` | Missing | S | P0: user's must-have; about 150 T3 worktrees on this machine |
| Switch or create a branch | `mod+shift+g` (`components/BranchPicker.tsx`) | `vcs.listRefs`, `vcs.switchRef`, `vcs.createRef` | Missing | S | P0: user's must-have |
| Git status | Ahead, behind and dirty for the git actions | `subscribeVcsStatus`, `vcs.refreshStatus` | Missing | S | P0: user's must-have (feeds §2.11) |
| Worktree handoff | Moves a thread between the checkout and a worktree | `thread.metadata.update` `{worktreePath, branch}` | Missing | M | P2 |
| Project scripts | Run `dev` or `test` from `t3.json`; `runOnSettle` (`components/ProjectScriptsControl.tsx`) | `terminal.open`; `projects.mutate` scripts | Missing | M | P2 |

### 2.11 Git and pull requests

| Feature | What T3 Code does | T3 API | mustr status | Effort | Priority |
|---|---|---|---|---|---|
| Commit, push, create PR | One control for commit, push and PR, with progress. T3 writes the commit message and PR text, and links the PR to the thread (`components/GitActionsControl.tsx`) | `git.runStackedAction` (stream; `commit`, `push`, `create_pr`, `commit_push`, `commit_push_pr`), `subscribeVcsStatus` | Missing | M | P0 |
| The thread's PR | PR chip with checks in the header and sidebar (`components/ThreadStatusIndicators.tsx`, `chat/ThreadDetailsPrRow.tsx`) | Shell `linkedPullRequest`, `branchPullRequest`; `pullRequests.summary`, `pullRequests.checks` | Partial: decoded for the inbox (`mustr/src/agent_threads.rs`); not shown in the thread | S | P0 (with the row above) |
| Link or unlink a PR | `pullRequest/LinkPullRequestDialog.tsx` | `thread.pull-request.link`, `thread.pull-request.unlink` | Missing | S | P0: user's must-have |
| Watch a PR | The agent reacts to CI and comments | `thread.pull-request.watch` | Missing | S | P0: user's must-have |
| A PR's threads | `pullRequest/PullRequestThreadLinks.tsx` | `pullRequests.linkedThreads` | Missing | S | P1 |
| Merge, ready, close | `pullRequest/PullRequestDetailActions.tsx` | `pullRequests.runAction` | Missing (a PLAN §5 should-have) | S | P0: user's must-have |
| PR inbox, review, comments, submit | `pullRequest/PullRequestsPage.tsx` | `pullRequests.*` | Done: mustr's core (`review/`, `mustr/src/home.rs`) | — | — |
| Review guides | `pullRequest/PullRequestGuideTab.tsx`; agents' `write_pull_request_guides` | `pullRequests.reviewGuide`, `generateReviewGuide` | Missing | M | P2 |
| Reviewers, labels, title; stacks | `pullRequest/PullRequestReviewerPicker.tsx`, `PullRequestStack*.tsx` | `requestReviewers`, `setLabels`, `update`, `stack` | Missing | M | P2 |
| Source control accounts | Hosts, tokens, writing style (`settings/SourceControlSettings.tsx`) | `server.discoverSourceControl`, `server.updateSettings` | Missing | M | P2: set once; T3 reads `gh` and `glab` auth |

### 2.12 Terminals and editors

| Feature | What T3 Code does | T3 API | mustr status | Effort | Priority |
|---|---|---|---|---|---|
| Open in editor | Cursor, VS Code, Zed and others for the thread's folder (`chat/OpenInPicker.tsx`) | `shell.openInEditor`; editors from `server.getConfig` | Missing (listed in PLAN §8) | S | P0: the way out for anything mustr lacks |
| Terminal per thread | A drawer in the thread's worktree that stays open (`components/ThreadTerminalDrawer.tsx`, `components/PersistentThreadTerminals.tsx`) | `terminal.open/attach/write/resize/close`, `subscribeTerminalEvents` (`terminal:*` scopes) | Missing: Zed's `terminal_view` is set up (`mustr/src/main.rs:394`) but there's no panel | M | P0: user's must-have. Use Zed's terminal at the worktree for local threads, and T3's `terminal.*` for remote ones |
| Terminal excerpt to the composer | `chat/TerminalContextInlineChip.tsx` | `message.dispatch.context` (terminal) | Missing | S | P2 |

### 2.13 Preview browser and devices

| Feature | What T3 Code does | T3 API | mustr status | Effort | Priority |
|---|---|---|---|---|---|
| Preview browser | An Electron webview on desktop, or the server's headless Chrome streamed into a panel; the agent drives it too (`preview/PreviewPanel.tsx`, `browser/ServerBrowserSurface.tsx`) | `preview.*`, `subscribePreviewEvents`, `/api/preview-stream/*` (`preview:operate`) | Missing | L | P2: agents' `preview_*` tools run headless without a client; open URLs in the system browser |
| Local dev servers | Finds listening ports (`preview/useDiscoveredLocalServers.ts`) | `subscribeDiscoveredLocalServers` | Missing | S | P2 |
| Device emulation | iOS and Android simulators (`components/device/`) | `device.*`, `subscribeDeviceState` | Missing | L | Skip: only for mobile app work; agents' `device_*` tools still work |

### 2.14 Delegation, workflows and scheduled tasks

| Feature | What T3 Code does | T3 API | mustr status | Effort | Priority |
|---|---|---|---|---|---|
| Orchestrator MCP for agents | 63 tools at `/mcp`: threads, queue, PRs, preview, schedules (`apps/server/src/mcp/toolkits/`) | Served by the server | Done: needs no client | — | — |
| Delegated tasks, workflows | Delegated-task rows, background work with Stop, and workflow rows (`chat/ThreadLineageWorkflowRow.tsx`, `chat/WorkflowOpeningBlock.tsx`, `chat/ThreadNow.tsx`) | Shell `pendingBackgroundTasks`, `blockedOnThreadWait`; `orchestration.getWorkflowScript`, `orchestration.stopWorkflow`; `thread.stop` | Missing: delegated calls show as plain tool rows | M | P0: user's must-have |
| Outside agents | Claude Code or Codex in your own terminal sign in to T3's `/mcp`; you approve them at `/connect-agent` (`components/auth/ConnectAgentSurface.tsx`) | `/oauth/mcp/*` | Missing: the server still serves that page, so a system browser works | S | P2 |
| Scheduled tasks | List, edit, enable, run now and delete tasks on an interval, a fixed time or a webhook, with the webhook's delivery log (`settings/ScheduledTasksSettings.tsx`, `chat/ThreadAutomationsPanel.tsx`) | `scheduledTasks.subscribe`, `upsert`, `setEnabled`, `runNow`, `delete`; webhooks read-only: `listWebhookDeliveries`, `getWebhookDelivery` | Missing | M | P0: user's must-have; agents also create them with `schedule_task` |

### 2.15 Thread organization

| Feature | What T3 Code does | T3 API | mustr status | Effort | Priority |
|---|---|---|---|---|---|
| Sections: pinned, active, snoozed, settled | One list in sections Pinned, Active, Working (beta), Snoozed and Settled, with status pills and unread dots (`components/Sidebar.tsx`, `sidebar/SidebarThreadRow.tsx`) | Shell `settledAt`, `settledOverride`, `pinnedAt`, `snoozedUntil`, `lastVisitedAt`; `orchestration.getOlderSettledThreads` *(0.1.3)* | Partial: one list, newest first, with T3's settled and snoozed threads mixed in (`thread_chat/src/sidebar.rs:78`) | M | P0 |
| Project filter or grouping | "Filter by project"; grouping by project is an option (`components/LegacySidebar.tsx`, Settings → General → Sidebar) | Shell `projectId` | Missing | S | P1 |
| Settle, un-settle | Settle from the menu or `mod+shift+s`, which then moves to the next unsettled thread (`settleAndAdvance.ts`); auto-settle per thread | `thread.settle`, `thread.unsettle`, `thread.auto-settle.set` | Missing | S | P0 |
| Archive | Thread menu; archived list in Settings (`settings/ArchivedThreadsPanel.tsx`) | `thread.archive`, `thread.unarchive`, `orchestration.subscribeArchivedShell` | Partial: archived threads are hidden; can't archive | S | P0 (archive) / P2 (archived list) |
| Pin and reorder | Pinned threads stay on top | `thread.pin`, `thread.unpin`, `thread.pin.reorder`, `thread.active.reorder` | Missing | S | P1 |
| Unread | Dots for new activity; mark unread | `thread.visit`, `thread.mark-unread`; shell `lastVisitedAt` | Missing | S | P1 |
| Search threads | `mod+K` by title, branch, PR or project, then message matches (`components/CommandPaletteThreadItems.tsx`) | Local filter, then `orchestration.searchThreads` | Missing | M | P1 |
| Snooze, wake | `components/CustomSnoozeDialog.tsx` | `thread.snooze`, `thread.unsnooze` | Missing | S | P2 |
| Delete; bulk select; drag to settle | `components/Sidebar.drag.ts`, `threadSelectionStore.ts` | `thread.delete` and the commands above | Missing | M | P2 |
| Undo | `mod+z` undoes settle, unpin, snooze and archive for 5 seconds (`hooks/threadUndo.ts`) | The inverse commands | Missing | S | P2 |
| Hover card, details panel | `components/ThreadHoverCard.tsx`, `chat/ThreadDetailsPanel.tsx` | — | Missing | S | P2 |
| Copy branch, path, thread ID, PR link | Thread menu and palette (`components/threadActionMenu.logic.ts`) | — | Missing | S | P2 |

### 2.16 Notifications

| Feature | What T3 Code does | T3 API | mustr status | Effort | Priority |
|---|---|---|---|---|---|
| Desktop notifications | An OS notification, a sound and an in-app toast when a thread completes, needs approval or input, fails or hits a limit (`components/ThreadNotificationCoordinator.tsx`, `threadNotifications.ts`); off by default | Shell stream | Missing: in-app status only | M | P1: PLAN §15 has this as Later; move it up |
| Badge, attention count | App badge with the number of threads waiting on you | Shell stream | Missing | S | P1 |
| Mobile push | `apps/mobile`, `docs/user/mobile-notifications.md` | Sent by the server through T3 Connect | n/a: comes from the server, not the client | — | Skip |

### 2.17 Keys, settings, providers and usage

| Feature | What T3 Code does | T3 API | mustr status | Effort | Priority |
|---|---|---|---|---|---|
| Thread and composer keys | Model `mod+shift+m`, effort `mod+shift+e`, access `mod+shift+a`, workspace `mod+shift+x`, branch `mod+shift+g`; previous and next thread `mod+shift+[` and `]`; diff `mod+d`; open in editor `mod+o` (`packages/shared/src/keybindings.ts`) | Local; `server.upsertKeybinding` for T3's own store | Partial: send, queue and steer only (`thread_chat/src/thread_chat.rs` `key_bindings`) | S | P1: mustr uses Zed's `keymap.json`, so no sync with T3 |
| Command palette | Thread actions, new thread in…, settle, usage | — | Partial: New thread, Send, Steer and Stop are in Zed's palette | S | P1 |
| T3 settings | Thread defaults, auto-settle rules, follow-up behavior, branch naming (`settings/SettingsPanels.tsx`) | `server.getSettings`, `server.updateSettings` (`settings:write`) | Missing | M | P1 (reading defaults is P0, in WP1) |
| Install and sign in to agents | Install CLIs, log in (`settings/ProviderSetupSection.tsx`, `settings/ProviderAuthTerminal.tsx`, `onboarding/`) | `provider.install.*`, `provider.auth.*` (`providers:manage`) | Missing | M | P1: CLI logins live on disk and are shared |
| Usage limits | 5-hour and weekly windows per account (`chat/ComposerUsageLimits.tsx`, `components/usage/UsageLimits.tsx`) | Provider status in `server.getConfig` and `subscribeServerConfig`, `server.refreshProviders`; `provider.consumeResetCredit` | Missing | M | P1 |
| Usage dashboard | Tokens, cost, model breakdown, `mod+u` (`components/usage/UsagePage.tsx`) | `server.getUsageSummary` (`diagnostics:read`) | Missing | M | P2 |
| Provider instances, ACP agents | Several accounts per provider; ACP registry (`settings/AddProviderInstanceDialog.tsx`) | `server.updateSettings`, `server.*AcpRegistry*` | Missing | M | P2 |
| Appearance, themes | `settings/AppearanceSettingsPanel.tsx` | — | n/a | — | Skip: Zed themes and mustr's theme |
| Diagnostics, storage, telemetry | `settings/DiagnosticsSettings.tsx`, `settings/StorageSettings.tsx` | `server.getProcessDiagnostics`, `server.runStorageCleanup` | Missing | M | Skip: maintenance; use T3's CLI |

### 2.18 Sharing and mobile

| Feature | What T3 Code does | T3 API | mustr status | Effort | Priority |
|---|---|---|---|---|---|
| Guest share links | Read-only or chat link for a thread (`chat/GuestShareControls.tsx`, `guestPage.ts`) | `guestShare.create` (`access:write`), `/api/guest/*` | Missing | — | Skip: needs `access:write`; collaboration is a non-goal (PLAN §1) |
| Mobile app | `apps/mobile` | The same server | n/a | — | Skip: keeps working against the server when mustr pairs with the user's T3 |
| Snap Shot, browser import | Screen capture tool; cookie import for preview (`docs/user/snap-shot.md`, `docs/user/browser-import.md`) | — | Missing | — | Skip: desktop extras; browser import only matters with the preview |

**On the 0.1.1 pin.** Every P0 works on 0.1.1. Where 0.1.3 adds a call, mustr uses 0.1.1's data
until the pin moves:
- **Subagents:** 0.1.1's shell holds every thread, subagent children included, and each has a
  `lineage`: `parentThreadId`, `rootThreadId`, and `relationshipToParent` (`fork` or `subagent`).
  A subagent turn item names its `childThreadId` and `subagentId`. So mustr builds a thread's family
  from the shell it already follows, and stops a child with `subagent.stop` (or the whole tree with
  `thread.stop`).
- **On 0.1.3, use `getThreadFamily`:** its snapshots leave out idle subagent children
  (`omitIdleSubagents`, `packages/contracts/src/orchestrationV2.ts:3438`), which is why
  `orchestration.getThreadFamily` exists. Switch to it when the pin moves.
- **Other 0.1.3-only calls:** for shared worktrees, match `worktreePath` across the shell's
  threads instead of calling `getWorktreeThreadIds`. `getOlderSettledThreads` isn't needed, since
  0.1.1's shell has every thread. Only `queued-run.start` (P2) waits for the pin.
- **Already there:** workflows (`getWorkflowScript`, `stopWorkflow`), all nine
  `scheduledTasks.*` calls, `terminal.*`, `vcs.*`, `git.*` and `pullRequests.runAction` are in
  0.1.1 (`t3_api/src/generated/methods.rs`).

**Counts** (125 rows):
- **P0:** 41. 11 are done and 30 open: 29 for §3, plus the embedded engine.
- **P1:** 33.
- **P2:** 38.
- **Skip:** 11.
- **No priority:** 2 done rows.

**Scopes:** P0 fits mustr's current grant (`orchestration:read`, `orchestration:operate`,
`source-control:write`), except T3's terminals for remote threads, which need `terminal:read` and
`terminal:operate`. Local threads use Zed's terminal and need no scope. P1 adds `filesystem:read`
(@-mentions), `settings:write` and `providers:manage`. `access:*` and `relay:*` stay out.

## 3. Recommended build order

All 29 open P0 rows, in six packages that separate agents can build at the same time. Each package
owns its files. A short WP0 lands first and adds the seams that keep the packages out of each
other's files. Rules for every package:
- Write through the installed backend or connection, so `MUSTR_DRY_RUN_WRITES` still refuses
  writes.
- Assert exact payloads with the recording backends.
- Don't touch `mustr/src/t3_connection.rs` or the runtime code, which belong to the engine agent.

**WP0: Seams** (S, one agent, lands before the rest)
- Split `thread_chat/src/thread_view.rs` (2,137 lines) into `thread_view.rs` (item, state, follow),
  `timeline.rs`, `composer.rs` and `header.rs`. No behavior changes. `header.rs` gets no P0
  work, since WP4 adds its actions through the toolbar.
- Add `ThreadBackend::call_raw(tag, payload)` and `upload(...)` to `thread_chat/src/backend.rs` and
  `mustr/src/thread_backend.rs`. They honor the dry-run and scope checks, so later packages never
  edit the backend.
- Add empty `thread_chat/src/settings_bar.rs` (`ThreadSettingsBar`, rendered by `composer.rs`)
  and `thread_chat/src/family_bar.rs` (`ThreadFamilyBar`, rendered above the timeline).
- Return `true` from `ThreadView::show_toolbar` so the app can add a Zed `ToolbarItemView`.
- Add cross-package actions in `thread_chat/src/thread_chat.rs`, each handled by its owner:
  `OpenThreadTerminal { thread_id }` (WP6), `ReviewThreadChanges { thread_id }` (WP4),
  `NewThreadInWorktree { project_id, worktree_path, branch }` (WP1), and
  `RemoveThreadWorktree { thread_id }` (WP6). WP4's toolbar can then open WP6's terminal, and
  WP3's Archive can offer WP6's worktree removal, without editing each other's files.
- Add a rail registry to `mustr/src/nav_panel.rs` (`register_rail_entry(id, icon, tooltip,
  action)`, drawn between Home and Settings in `render_rail`), so WP5 and WP6 add entries without
  editing it.
- Reserve the only shared lines, so later edits are one-line appends: the `mod` and `init` lines
  in `thread_chat/src/thread_chat.rs` and `mustr/src/main.rs`, the toolbar line next to
  `add_search_bars` (`mustr/src/window.rs:297`), and a dock-panel line next to
  `workspace.add_panel(nav_panel…)` (`mustr/src/window.rs:95`).

**WP1: Thread settings and new threads** (about 1.5 weeks)
- **Owns:** `thread_chat/src/settings_bar.rs`, `thread_chat/src/new_thread.rs`, and a new
  `thread_chat/src/thread_settings.rs` (command builders, defaults).
- **Builds:** permission-mode picker, with T3's default from `server.getSettings` replacing the
  forced `approval-required`; model and provider picker mid-thread; model options (effort, fast
  mode, thinking, context window from provider capabilities); the plan-mode toggle; and scratch
  threads ("No project" through `projects.ensureScratch`).
- **Also builds (user's must-haves):** the base-branch picker (`vcs.listRefs`, "Start from
  remote"), and new threads in an existing worktree or on a branch (`existing_worktree`, `root
  {branch}`), including the `NewThreadInWorktree` handler.
- **Commands:** `thread.runtime-mode.set`, `thread.model-selection.set`,
  `thread.interaction-mode.set`.

**WP2: Composer and timeline** (about 1.5 weeks)
- **Owns:** `thread_chat/src/composer.rs`, `timeline.rs`, `model.rs`, `commands.rs`, a new
  `thread_chat/src/attachments.rs`, and a new `t3_client/src/attachments.rs` (upload, signed asset
  fetch).
- **Builds:** image attachments by paste, drag and picker, with a thumbnail strip, upload progress
  and retry; images in user messages and tool output; Implement plan on the plan card
  (`sourcePlanRef`, switching to `default`); and Resume (`queue.resume`, or `message.dispatch` with
  `manualContinuationOfRunId`).

**WP3: Thread list and projects** (about 1 week)
- **Owns:** `thread_chat/src/sidebar.rs`, a new `thread_chat/src/organize.rs` (Zed actions and
  context menu: settle, un-settle, archive), a new `thread_chat/src/projects.rs` (Add project: the OS
  folder picker, then `projects.mutate` `{project.create}`), the built-in rail entries in
  `mustr/src/nav_panel.rs`, and the thread sections of `mustr/src/home.rs`.
- **Builds:** sidebar sections for pinned, active, snoozed and settled threads (pinned and snoozed
  are display only; pinning and snoozing themselves are P1 and P2), and settle and un-settle with
  `mod+shift+s` settle-and-advance. It also adds Archive (which dispatches `RemoveThreadWorktree`),
  and Add project in the sidebar header and the palette. Home's "Recent agent threads" leaves out
  settled threads.

**WP4: Ship the work and the PR flow** (about 2 weeks)
- **Owns:** a new `mustr/src/thread_toolbar.rs` (a `ToolbarItemView` for `ThreadView`), a new
  `mustr/src/thread_git.rs`, `mustr/src/agent_review.rs`, `review/src/agent.rs`, and for the PR
  header actions `review/src/header.rs` and `mustr/src/pull_request_review.rs`.
- **Builds:** **Review changes** (the thread's Changes or latest turn in the review); **Open in ▾**
  (`shell.openInEditor`, editors from `server.getConfig`); **Commit, push, PR ▾** (the
  `git.runStackedAction` stream, with phase progress and errors per step); the thread's PR chip,
  which opens mustr's PR review; and a **Terminal** button that dispatches `OpenThreadTerminal`.
- **Also builds (user's must-haves):** git status (`subscribeVcsStatus`), which feeds the commit
  and push states; branch switch and create (`vcs.listRefs`, `vcs.switchRef`, `vcs.createRef`,
  then `thread.metadata.update` `{branch}`); link, unlink and watch a PR
  (`thread.pull-request.link`, `unlink`, `watch`); **Work on PR** in the PR header
  (`git.resolvePullRequest`, `git.preparePullRequestThread`, then `NewThreadInWorktree`); and
  merge (merge, squash or rebase), ready and close through `pullRequests.runAction`.
- The same thread actions also go in the agent review header.

**WP5: Subagents, workflows and scheduled tasks** (about 1.5 weeks)
- **Owns:** a new `thread_chat/src/family_bar.rs` and a new `mustr/src/schedules.rs` (a schedules
  page, with a rail entry through the registry).
- **Builds the family bar:** in a child, "Open parent" and the subagent's status. In a parent, its
  children with Open (`open_thread`) and Stop (`subagent.stop`), delegated tasks and background
  work (`pendingBackgroundTasks`, `blockedOnThreadWait`), and workflow rows with the script
  (`getWorkflowScript`) and Stop (`stopWorkflow`). On 0.1.1 the family comes from the shell's
  `lineage`; a version check moves it to `getThreadFamily` on 0.1.3.
- **Builds the schedules page** (`scheduledTasks.subscribe`, `upsert`, `setEnabled`, `runNow`,
  `delete`): a list, an edit form for interval and fixed-time triggers, and webhook triggers shown
  read-only with their delivery log (`listWebhookDeliveries`, `getWebhookDelivery`).

**WP6: Terminals and worktrees** (about 1.5 weeks)
- **Owns:** a new `mustr/src/terminal_panel.rs` and a new `mustr/src/worktrees.rs`.
- **Builds the terminal:** a Zed terminal dock panel opened at the thread's `worktreePath` (Zed's
  `terminal_view` is already set up in `mustr/src/main.rs:394`), one per thread and kept across
  navigation, as the `OpenThreadTerminal` handler. T3's `terminal.*` is used only for remote
  threads, behind the `terminal:*` scopes.
- **Builds the worktrees view:** the shell's worktrees with their threads; removing unshared ones
  (`vcs.removeWorktree`, matching `worktreePath` across threads on 0.1.1), which also handles
  `RemoveThreadWorktree`; and opening a worktree as an editor project, so Zed's file finder,
  search and language servers cover it.

**After P0:** start the P1s that make the switch stick: notifications and the badge; @-mentions,
the slash menu, drafts, recall and queue edits; revert, fork, rename, pin, unread and search; and
usage limits, then provider setup. The same ownership holds: composer and timeline items go to
WP2, list items to WP3, git and PR items to WP4, and family items to WP5.

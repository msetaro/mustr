//! mustr's agent threads (`mustr-docs/PLAN.md` §8): a thread's timeline and composer
//! ([`ThreadView`]), the threads sidebar ([`ThreadsSidebar`]), and starting new threads.
//!
//! Everything goes through T3's orchestration API behind [`ThreadBackend`], which the app sets
//! with [`set_backend`]. Other parts of mustr drive threads through [`send_message`] (for
//! example, the review's "Send to agent") and open them with [`open_thread`]. [`ThreadShells`]
//! keeps T3's shell, every thread and project, for the whole app. The parts of mustr that drive
//! threads trigger each other through the actions in [`actions`], such as [`OpenThreadTerminal`].

pub mod actions;
pub mod backend;
pub mod commands;
mod composer;
mod family_bar;
mod header;
pub mod model;
mod new_thread;
mod settings_bar;
pub mod shell;
mod sidebar;
mod thread_view;
mod timeline;

// Work packages add their `mod` and `pub use` lines here, each package under its own label, so
// their branches merge without conflicts (`mustr-docs/PARITY.md` §3).
// WP1, thread settings and new threads:
pub mod thread_settings;
pub use settings_bar::{
    ToggleModelOptions, ToggleModelPicker, TogglePermissionMode, TogglePlanMode,
};
// WP2, composer and timeline:
pub mod attachments;
// WP3, thread list and projects:
pub mod organize;
pub mod projects;
pub use organize::{
    ArchiveThread, MarkThreadUnread, PinThread, RenameThread, SettleThread, UnpinThread,
    UnsettleThread,
};
pub use projects::AddProject;
pub use sidebar::{SearchThreads, Section, ThreadListFilter, ThreadsSidebarEvent};
#[cfg(test)]
mod thread_list_tests;
// WP10, history, agent status and usage:
pub mod history;
pub mod usage;
pub use history::{CompactContext, RestartAgentSession};
pub use usage::{ServerConfigs, ShowUsageLimits};
#[cfg(test)]
mod history_tests;
// WP9, composer mentions and slash commands:
mod mentions;
mod slash_commands;
pub use mentions::set_local_folders;
// WP11, terminals, worktrees and the PR flow:
mod worktree_setup;
// WP12, composer and timeline:
mod find_bar;
pub use mentions::{context_reference_and_record, message_context_of};
// WP13, thread list and navigation:
mod snooze;
pub use new_thread::{ToggleWorkspacePicker, UsePreviousWorktree};
pub use organize::{
    CopyPullRequestLink, ShowArchivedThreads, ToggleThreadAutoSettle, UndoThreadAction, WakeThread,
};
pub use sidebar::{NextThread, PreviousThread, RowPullRequest};
pub use snooze::SnoozeThread;
// WP14, agent setup and T3's settings:
// WP17, threads, creation and navigation:
pub mod bulk;
mod clone;
mod details;
mod launch;
mod outside;
pub mod palette;
pub use bulk::DeleteThread;
pub use clone::CloneRepository;
pub use details::ShowThreadDetails;
pub use launch::StartInBackground;
pub use outside::ConnectOutsideAgent;
#[cfg(test)]
mod creation_tests;
#[cfg(test)]
mod selection_tests;
pub use palette::{
    CopyThreadId, FindInThread, NewThreadInProject, NewThreadWithoutProject, PaletteResult,
    palette_results,
};

#[cfg(test)]
mod tests;

pub use actions::{
    AddComposerContext, ComposerContext, NewThreadInWorktree, OpenInEditor, OpenProjectSettings,
    OpenThreadTerminal, RemoveThreadWorktree, ReviewThreadChanges,
};
pub use backend::{
    Access, AttachmentUpload, GlobalThreadBackend, ReadOnlyBackend, ThreadBackend,
    ThreadBackendExt, UnsupportedRpc, backend, is_unsupported_rpc, set_backend,
};
pub use composer::{SendMessage, SteerMessage, StopThread};
pub use family_bar::ThreadFamilyBar;
pub use new_thread::{AgentChoice, NewThreadForm, agent_choices, runtime_mode_for};
pub use settings_bar::ThreadSettingsBar;
pub use shell::{Shell, ThreadShells};
pub use sidebar::{ShellStatus, ThreadEntry, ThreadList, ThreadsSidebar};
pub use thread_view::{NewThread, ThreadView, open_thread};

use anyhow::{Context as _, Result, anyhow};
use gpui::{App, AppContext as _, Task};
use workspace::Workspace;

/// Registers the thread actions on every workspace. Call once at startup.
pub fn init(cx: &mut App) {
    cx.observe_new(|workspace: &mut Workspace, _, _| thread_view::register(workspace))
        .detach();
    // Work packages call their `init` here, each package under its own label
    // (`mustr-docs/PARITY.md` §3).
    // WP1, thread settings and new threads:
    cx.observe_new(|workspace: &mut Workspace, _, _| {
        settings_bar::register(workspace);
        new_thread::register(workspace);
    })
    .detach();
    // WP2, composer and timeline:
    // WP3, thread list and projects:
    organize::init(cx);
    sidebar::init(cx);
    // WP5, subagents and workflows:
    // WP10, history, agent status and usage:
    history::init(cx);
    usage::init(cx);
    // WP11, terminals, worktrees and the PR flow:
    // WP12, composer and timeline:
    mentions::init(cx);
    // WP13, thread list and navigation:
    snooze::init(cx);
    // WP14, agent setup and T3's settings:
    thread_settings::FollowUpBehavior::load(cx);
    // WP17, threads, creation and navigation:
    palette::init(cx);
    launch::init(cx);
    bulk::init(cx);
    details::init(cx);
    clone::init(cx);
    outside::init(cx);
}

/// mustr's key bindings for threads. The app binds them after Zed's, so users can rebind them:
/// - `enter` (or `secondary-enter`) in the composer sends, or queues while the agent works;
///   `shift-enter` is a new line.
/// - `secondary-shift-enter` steers the running agent with the message.
pub fn key_bindings() -> Vec<gpui::KeyBinding> {
    use gpui::KeyBinding;
    let composer = Some("ThreadComposer > Editor");
    // The last binding of an action is the one key hints show.
    vec![
        KeyBinding::new("secondary-enter", SendMessage, composer),
        KeyBinding::new("enter", SendMessage, composer),
        KeyBinding::new("secondary-shift-enter", SteerMessage, composer),
        // Work packages add their bindings here, each package under its own label
        // (`mustr-docs/PARITY.md` §3).
        // WP1, thread settings and new threads: T3's `mod+shift+m` (model), `mod+shift+e`
        // (effort and the model's other options), and `mod+shift+a` (permission mode).
        KeyBinding::new("secondary-shift-m", ToggleModelPicker, Some("ThreadView")),
        KeyBinding::new("secondary-shift-e", ToggleModelOptions, Some("ThreadView")),
        KeyBinding::new(
            "secondary-shift-a",
            TogglePermissionMode,
            Some("ThreadView"),
        ),
        // WP2, composer and timeline:
        // WP3, thread list and projects:
        KeyBinding::new("secondary-shift-s", SettleThread, Some("ThreadView")),
        KeyBinding::new("secondary-shift-s", SettleThread, Some("ThreadsSidebar")),
        KeyBinding::new("secondary-alt-k", SearchThreads, Some("Workspace")),
        KeyBinding::new("escape", menu::Cancel, Some("ThreadsSidebar > Editor")),
        // WP11, terminals, worktrees and the PR flow:
        // WP12, composer and timeline: T3's `mod+shift+v` pastes text as text, however big.
        KeyBinding::new("secondary-shift-v", composer::PasteAsText, composer),
        // Find in thread: Zed binds `mod+f` in full editors only, and the composer isn't one.
        KeyBinding::new(
            "secondary-f",
            zed_actions::buffer_search::Deploy::find(),
            Some("ThreadView"),
        ),
        // WP13, thread list and navigation: T3's `mod+shift+[` and `]` (previous and next
        // thread, over Zed's tab switching and Fold in a thread), `mod+shift+x` (where a new
        // thread works), `mod+shift+l` (its previous worktree, over the composer's Select All
        // Matches on drafts), and `mod+z` (undo a settle, unpin or snooze) outside text fields.
        // GPUI reads shift-[ as `{`.
        KeyBinding::new("secondary-{", PreviousThread, Some("ThreadView")),
        KeyBinding::new("secondary-}", NextThread, Some("ThreadView")),
        KeyBinding::new("secondary-{", PreviousThread, Some("ThreadView > Editor")),
        KeyBinding::new("secondary-}", NextThread, Some("ThreadView > Editor")),
        KeyBinding::new("secondary-{", PreviousThread, Some("ThreadsSidebar")),
        KeyBinding::new("secondary-}", NextThread, Some("ThreadsSidebar")),
        KeyBinding::new(
            "secondary-{",
            PreviousThread,
            Some("ThreadsSidebar > Editor"),
        ),
        KeyBinding::new("secondary-}", NextThread, Some("ThreadsSidebar > Editor")),
        KeyBinding::new(
            "secondary-shift-x",
            ToggleWorkspacePicker,
            Some("ThreadView"),
        ),
        KeyBinding::new("secondary-shift-l", UsePreviousWorktree, Some("ThreadView")),
        KeyBinding::new(
            "secondary-shift-l",
            UsePreviousWorktree,
            Some("ThreadView > Editor"),
        ),
        KeyBinding::new("secondary-z", UndoThreadAction, Some("ThreadView")),
        KeyBinding::new("secondary-z", UndoThreadAction, Some("ThreadsSidebar")),
        // WP14, agent setup and T3's settings:
        // WP17, threads, creation and navigation: T3's `mod+Enter` and `mod+alt+Enter` start a
        // draft's thread in the background. On other threads they go on to Send.
        KeyBinding::new("secondary-alt-enter", StartInBackground, composer),
        KeyBinding::new("secondary-enter", StartInBackground, composer),
        // `mod+z` undoes a settle, unpin or snooze from an empty composer too, as the next
        // thread's composer has focus after settling. With text in it, it's the editor's undo.
        KeyBinding::new("secondary-z", UndoThreadAction, composer),
    ]
}

/// Sends `text` to a thread as the user, as the composer's Send does: T3 starts a run if the
/// agent is idle, and otherwise steers the running agent with it, or queues it when the agent
/// can't take input mid-run (`deliveryIntent: "auto"`).
///
/// This is the seam for "Send to agent" from a review. It writes through the installed
/// [`ThreadBackend`], so it honors `MUSTR_DRY_RUN_WRITES` and the pairing's scopes, and tests can
/// install a recording backend.
pub fn send_message(thread_id: &str, text: &str, cx: &mut App) -> Task<Result<()>> {
    let Some(backend) = backend(cx) else {
        return Task::ready(Err(anyhow!("mustr isn't set up to drive agent threads")));
    };
    if text.trim().is_empty() {
        return Task::ready(Err(anyhow!("the message is empty")));
    }
    let command = commands::send_message(
        thread_id,
        text,
        commands::Delivery::Auto,
        commands::CommandIds::new(),
    );
    let dispatch = backend.dispatch(command);
    cx.background_spawn(async move {
        dispatch.await.context("sending the message to the agent")?;
        Ok(())
    })
}

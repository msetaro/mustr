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
// WP2, composer and timeline:
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

#[cfg(test)]
mod tests;

pub use actions::{
    NewThreadInWorktree, OpenInEditor, OpenThreadTerminal, RemoveThreadWorktree,
    ReviewThreadChanges,
};
pub use backend::{
    Access, AttachmentUpload, GlobalThreadBackend, ReadOnlyBackend, ThreadBackend,
    ThreadBackendExt, backend, set_backend,
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
    // WP2, composer and timeline:
    // WP3, thread list and projects:
    organize::init(cx);
    sidebar::init(cx);
    // WP5, subagents and workflows:
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
        // WP1, thread settings and new threads:
        // WP2, composer and timeline:
        // WP3, thread list and projects:
        KeyBinding::new("secondary-shift-s", SettleThread, Some("ThreadView")),
        KeyBinding::new("secondary-shift-s", SettleThread, Some("ThreadsSidebar")),
        KeyBinding::new("secondary-alt-k", SearchThreads, Some("Workspace")),
        KeyBinding::new("escape", menu::Cancel, Some("ThreadsSidebar > Editor")),
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

//! mustr's agent threads (`mustr-docs/PLAN.md` §8): a thread's timeline and composer
//! ([`ThreadView`]), the threads sidebar ([`ThreadsSidebar`]), and starting new threads.
//!
//! Everything goes through T3's orchestration API behind [`ThreadBackend`], which the app sets
//! with [`set_backend`]. Other parts of mustr drive threads through [`send_message`] (for
//! example, the review's "Send to agent") and open them with [`open_thread`].

pub mod backend;
pub mod commands;
pub mod model;
mod new_thread;
mod sidebar;
mod thread_view;

#[cfg(test)]
mod tests;

pub use backend::{GlobalThreadBackend, ReadOnlyBackend, ThreadBackend, backend, set_backend};
pub use new_thread::{AgentChoice, NewThreadForm, agent_choices, runtime_mode_for};
pub use sidebar::{ShellStatus, ThreadEntry, ThreadList, ThreadsSidebar};
pub use thread_view::{NewThread, SendMessage, SteerMessage, StopThread, ThreadView, open_thread};

use anyhow::{Context as _, Result, anyhow};
use gpui::{App, AppContext as _, Task};
use workspace::Workspace;

/// Registers the thread actions on every workspace. Call once at startup.
pub fn init(cx: &mut App) {
    cx.observe_new(|workspace: &mut Workspace, _, _| thread_view::register(workspace))
        .detach();
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

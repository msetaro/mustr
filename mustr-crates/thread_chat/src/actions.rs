//! Actions one part of mustr dispatches and another handles, so neither edits the other's files
//! (`mustr-docs/PARITY.md` §3). Each says which file handles it. Until that handler exists,
//! dispatching the action does nothing.

use gpui::Action;
use schemars::JsonSchema;
use serde::Deserialize;

/// Opens a terminal in the thread's worktree. Handled by the terminal panel
/// (`mustr/src/terminal_panel.rs`, WP6).
#[derive(Clone, Debug, PartialEq, Eq, Deserialize, JsonSchema, Action)]
#[action(namespace = thread_chat)]
#[serde(deny_unknown_fields)]
pub struct OpenThreadTerminal {
    pub thread_id: String,
}

/// Reviews the thread's changes: its branch, or its latest turn. Handled by the thread toolbar
/// (`mustr/src/thread_toolbar.rs`, WP4).
#[derive(Clone, Debug, PartialEq, Eq, Deserialize, JsonSchema, Action)]
#[action(namespace = thread_chat)]
#[serde(deny_unknown_fields)]
pub struct ReviewThreadChanges {
    pub thread_id: String,
}

/// Opens the thread's worktree in an editor, through T3's `shell.openInEditor`. Handled by the
/// thread toolbar (`mustr/src/thread_toolbar.rs`, WP4).
#[derive(Clone, Debug, PartialEq, Eq, Deserialize, JsonSchema, Action)]
#[action(namespace = thread_chat)]
#[serde(deny_unknown_fields)]
pub struct OpenInEditor {
    pub thread_id: String,
}

/// Opens a draft for a new thread in an existing worktree, or on a branch of the project's
/// checkout. Handled by the new-thread form (`thread_chat/src/new_thread.rs`, WP1).
#[derive(Clone, Debug, PartialEq, Eq, Deserialize, JsonSchema, Action)]
#[action(namespace = thread_chat)]
#[serde(deny_unknown_fields)]
pub struct NewThreadInWorktree {
    pub project_id: String,
    /// The worktree to start in (T3's `existing_worktree`), or `None` for the project's checkout
    /// (`root`).
    #[serde(default)]
    pub worktree_path: Option<String>,
    /// The branch to start on.
    #[serde(default)]
    pub branch: Option<String>,
}

/// Removes the thread's worktree if no other thread uses it. Handled by the worktrees view
/// (`mustr/src/worktrees.rs`, WP6).
#[derive(Clone, Debug, PartialEq, Eq, Deserialize, JsonSchema, Action)]
#[action(namespace = thread_chat)]
#[serde(deny_unknown_fields)]
pub struct RemoveThreadWorktree {
    pub thread_id: String,
}

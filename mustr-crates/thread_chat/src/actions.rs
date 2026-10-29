//! Actions one part of mustr dispatches and another handles, so neither edits the other's files
//! (`mustr-docs/PARITY.md` §3). Each says which file handles it. Until that handler exists,
//! dispatching the action does nothing.

use gpui::Action;
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

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

/// Adds a terminal excerpt or a review comment to a thread's composer, as a chip the next
/// message carries in its `context` (`message.dispatch`). Handled by the composer
/// (`thread_chat/src/composer.rs`, WP12); dispatched by the terminal panel
/// (`mustr/src/terminal_panel.rs`, WP11) and the agent review's Send to agent
/// (`mustr/src/agent_review.rs`, WP12).
#[derive(Clone, Debug, PartialEq, Eq, Deserialize, JsonSchema, Action)]
#[action(namespace = thread_chat)]
#[serde(deny_unknown_fields)]
pub struct AddComposerContext {
    pub thread_id: String,
    pub context: ComposerContext,
}

/// One of T3's composer context records (`ComposerContextRecord` in T3's
/// `contracts/src/composerContext.ts`), the kinds mustr sends. `t3_api` leaves the records as
/// JSON, so this is their shape: [`ComposerContext::to_record`] gives what goes in
/// `context.records`.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(tag = "kind", rename_all = "kebab-case")]
pub enum ComposerContext {
    Terminal(TerminalContext),
    ReviewComment(ReviewCommentContext),
}

/// Lines from a terminal (T3's `TerminalContextRecord`).
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct TerminalContext {
    /// Unique within the message: T3 uses `terminal_<id>`. Letters, digits, `_` and `-`.
    pub context_id: String,
    /// The chip's text, such as "Terminal 1 lines 3-10".
    pub label: String,
    pub terminal_id: String,
    pub terminal_label: String,
    pub line_start: u32,
    pub line_end: u32,
    /// At most 64,000 characters.
    pub text: String,
}

/// A comment on a diff (T3's `ReviewCommentContextRecord`).
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct ReviewCommentContext {
    /// Unique within the message: T3 uses `review-comment_<comment id>`.
    pub context_id: String,
    pub label: String,
    pub section_id: String,
    pub section_title: String,
    pub file_path: String,
    pub start_index: u32,
    pub end_index: u32,
    /// Such as "Lines 12-18".
    pub range_label: String,
    /// The comment, at most 16,000 characters.
    pub text: String,
    /// The commented lines' diff, at most 32,000 characters.
    pub diff: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub fence_language: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub pull_request: Option<PullRequestContext>,
}

/// The pull request a review comment is on (T3's `PullRequestContextMetadata`).
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct PullRequestContext {
    pub number: u64,
    pub title: String,
    pub url: String,
    pub head_branch: String,
    pub base_branch: String,
    pub state: PullRequestContextState,
    pub is_draft: bool,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "lowercase")]
pub enum PullRequestContextState {
    Open,
    Closed,
    Merged,
}

impl ComposerContext {
    /// The record for `context.records` in `message.dispatch`, in T3's version 1.
    pub fn to_record(&self) -> serde_json::Result<serde_json::Value> {
        let mut record = serde_json::to_value(self)?;
        if let Some(fields) = record.as_object_mut() {
            fields.insert("version".into(), 1.into());
        }
        Ok(record)
    }
}

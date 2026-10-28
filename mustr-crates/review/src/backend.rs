//! What the review view asks of the app for anything beyond showing a diff: saving the pending
//! review, and the writes that go to the forge. The view knows nothing about T3; the app
//! implements this on top of its T3 connection.

use anyhow::Result;
use gpui::{App, SharedString, Task};

use crate::{PendingReview, ReviewSubmission, ReviewThread, ReviewVerdict};

pub trait ReviewBackend: 'static {
    /// Saves the pending review so it survives a restart. Called on every change; the latest
    /// call wins. An empty review should delete what was saved.
    fn save_pending_review(&self, review: &PendingReview, cx: &mut App);

    fn reply_to_thread(&self, thread_id: String, body: String, cx: &mut App) -> Task<Result<()>>;

    fn set_thread_resolution(
        &self,
        thread_id: String,
        resolved: bool,
        cx: &mut App,
    ) -> Task<Result<()>>;

    /// Edits one of the viewer's own line comments.
    fn update_comment(&self, comment_id: String, body: String, cx: &mut App) -> Task<Result<()>>;

    /// Marks files viewed (`true`) or not, by path.
    fn set_files_viewed(&self, files: Vec<(String, bool)>, cx: &mut App) -> Task<Result<()>>;

    fn submit_review(&self, submission: ReviewSubmission, cx: &mut App) -> Task<Result<()>>;

    /// Posts a comment on the pull request's conversation, outside any review.
    fn post_comment(&self, body: String, cx: &mut App) -> Task<Result<()>>;

    /// Loads the review threads again, after a write changed them.
    fn reload_threads(&self, cx: &mut App) -> Task<Result<Vec<ReviewThread>>>;
}

/// What the viewer may write, from the forge's capabilities and the viewer's permissions.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct ReviewPermissions {
    /// Line comments in a review.
    pub line_comments: bool,
    pub reply: bool,
    pub resolve: bool,
    /// The verdicts a review can be submitted with. Empty when the viewer can't review.
    pub verdicts: Vec<ReviewVerdict>,
    /// Comments on the conversation, outside a review.
    pub conversation_comments: bool,
    pub mark_viewed: bool,
    /// Why nothing can be written right now, when that's so (for example, mustr's pairing with
    /// T3 doesn't include writing to pull requests). Drafts can still be written.
    pub blocked: Option<SharedString>,
}

impl ReviewPermissions {
    /// Everything allowed, for tests and local reviews.
    pub fn all() -> Self {
        Self {
            line_comments: true,
            reply: true,
            resolve: true,
            verdicts: ReviewVerdict::ALL.to_vec(),
            conversation_comments: true,
            mark_viewed: true,
            blocked: None,
        }
    }

    pub fn can_review(&self) -> bool {
        self.line_comments || !self.verdicts.is_empty()
    }
}

//! A thread's worktree setup card, above the timeline: T3 preparing the thread's worktree and
//! running its setup script, with progress, Retry, and Cancel (`subscribeWorktreeSetup`,
//! `prepared-run.retry`, `worktreeSetup.cancel`; `mustr-docs/PARITY.md` §2.10). It's empty for
//! now: WP11 fills it in.

use crate::thread_view::ThreadView;
use gpui::{Context, IntoElement, Render, Styled as _, WeakEntity, Window, div};

/// The worktree setup of one [`ThreadView`]'s thread.
pub struct WorktreeSetupCard {
    #[expect(
        dead_code,
        reason = "the card is empty until it follows the thread's worktree setup (WP11)"
    )]
    thread_view: WeakEntity<ThreadView>,
}

impl WorktreeSetupCard {
    pub fn new(
        thread_view: WeakEntity<ThreadView>,
        _window: &mut Window,
        _cx: &mut Context<Self>,
    ) -> Self {
        Self { thread_view }
    }
}

impl Render for WorktreeSetupCard {
    fn render(&mut self, _window: &mut Window, _cx: &mut Context<Self>) -> impl IntoElement {
        // With nothing to show, `hidden` keeps the card out of the view's layout.
        div().hidden()
    }
}

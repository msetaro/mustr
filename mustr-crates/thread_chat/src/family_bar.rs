//! A thread's family bar, between the header and the timeline: its parent and subagents,
//! delegated tasks, and workflows (`mustr-docs/PARITY.md` §2.14). It's empty for now. On T3
//! 0.1.1 the family comes from the shell's threads and their `lineage`, which
//! [`crate::ThreadShells`] keeps for the whole app.

use crate::thread_view::ThreadView;
use gpui::{Context, IntoElement, Render, Styled as _, WeakEntity, Window, div};

/// The family of one [`ThreadView`]'s thread.
pub struct ThreadFamilyBar {
    #[expect(
        dead_code,
        reason = "the bar is empty until it reads the thread's family"
    )]
    thread_view: WeakEntity<ThreadView>,
}

impl ThreadFamilyBar {
    pub fn new(
        thread_view: WeakEntity<ThreadView>,
        _window: &mut Window,
        _cx: &mut Context<Self>,
    ) -> Self {
        Self { thread_view }
    }
}

impl Render for ThreadFamilyBar {
    fn render(&mut self, _window: &mut Window, _cx: &mut Context<Self>) -> impl IntoElement {
        // With nothing to show, `hidden` keeps the bar out of the view's layout.
        div().hidden()
    }
}

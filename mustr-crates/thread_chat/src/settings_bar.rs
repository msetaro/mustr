//! A thread's settings bar, in the composer's bottom row: the agent, model, and permission mode
//! the thread runs with (`mustr-docs/PARITY.md` §2.3). It's empty for now.

use crate::thread_view::ThreadView;
use gpui::{Context, IntoElement, Render, Styled as _, WeakEntity, Window, div};

/// The pickers for a thread's agent, model, and mode. It belongs to one [`ThreadView`], which
/// may still be a draft for a new thread.
pub struct ThreadSettingsBar {
    #[expect(
        dead_code,
        reason = "the bar is empty until its pickers read the thread"
    )]
    thread_view: WeakEntity<ThreadView>,
}

impl ThreadSettingsBar {
    pub fn new(
        thread_view: WeakEntity<ThreadView>,
        _window: &mut Window,
        _cx: &mut Context<Self>,
    ) -> Self {
        Self { thread_view }
    }
}

impl Render for ThreadSettingsBar {
    fn render(&mut self, _window: &mut Window, _cx: &mut Context<Self>) -> impl IntoElement {
        // With nothing to show, `hidden` keeps the bar out of the row's layout, gaps included.
        div().hidden()
    }
}

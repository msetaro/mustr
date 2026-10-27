use gpui::{App, EventEmitter, FocusHandle, Focusable};
use ui::prelude::*;
use workspace::item::{Item, ItemEvent};

/// mustr's center view until there's an inbox to show.
pub struct HomeView {
    focus_handle: FocusHandle,
}

impl HomeView {
    pub fn new(cx: &mut Context<Self>) -> Self {
        Self {
            focus_handle: cx.focus_handle(),
        }
    }
}

impl EventEmitter<ItemEvent> for HomeView {}

impl Focusable for HomeView {
    fn focus_handle(&self, _: &App) -> FocusHandle {
        self.focus_handle.clone()
    }
}

impl Item for HomeView {
    type Event = ItemEvent;

    fn tab_content_text(&self, _detail: usize, _cx: &App) -> SharedString {
        "mustr".into()
    }

    fn show_toolbar(&self) -> bool {
        false
    }

    fn to_item_events(event: &Self::Event, f: &mut dyn FnMut(ItemEvent)) {
        f(*event)
    }
}

impl Render for HomeView {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        v_flex()
            .key_context("MustrHome")
            .track_focus(&self.focus_handle)
            .size_full()
            .items_center()
            .justify_center()
            .gap_2()
            .bg(cx.theme().colors().editor_background)
            .child(Headline::new("mustr").size(HeadlineSize::Large))
            .child(Label::new("Connect to a T3 server").color(Color::Muted))
    }
}

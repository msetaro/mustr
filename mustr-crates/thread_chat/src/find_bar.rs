//! Find in thread (`mustr-docs/PARITY.md` §2.7): Zed's buffer search bar, in every pane's
//! toolbar (`mustr/src/window.rs`), searches the item that `Item::as_searchable` returns. For a
//! [`ThreadView`] that's [`searchable`], with the [`SearchableItem`] below: it searches the
//! timeline's rows, keeps its matches in [`FindState`] (the view's `find`) for the timeline to
//! highlight, and scrolls the timeline to the active match.
//!
//! It's empty for now, and [`searchable`] returns nothing, so the search bar stays hidden over
//! threads. WP12 fills it in.

use crate::thread_view::ThreadView;
use gpui::{App, Context, Entity, EventEmitter, Task, Window};
use project::search::SearchQuery;
use settings::SeedQuerySetting;
use std::sync::Arc;
use workspace::searchable::{
    Direction, SearchEvent, SearchOptions, SearchToken, SearchableItem, SearchableItemHandle,
};

/// What find in thread keeps on a [`ThreadView`]: its matches and the active one.
#[derive(Default)]
pub(crate) struct FindState {}

/// What [`ThreadView`]'s `Item::as_searchable` returns: `Some(Box::new(view.clone()))` once the
/// view searches its timeline.
pub(crate) fn searchable(
    _thread_view: &ThreadView,
    _view: &Entity<ThreadView>,
    _cx: &App,
) -> Option<Box<dyn SearchableItemHandle>> {
    None
}

impl EventEmitter<SearchEvent> for ThreadView {}

impl SearchableItem for ThreadView {
    /// A match's place in the timeline. WP12 picks what it holds.
    type Match = usize;

    fn supported_options(&self) -> SearchOptions {
        SearchOptions {
            case: true,
            word: true,
            regex: true,
            replacement: false,
            selection: false,
            select_all: false,
            find_in_results: false,
        }
    }

    fn clear_matches(&mut self, _window: &mut Window, _cx: &mut Context<Self>) {}

    fn update_matches(
        &mut self,
        _matches: &[Self::Match],
        _active_match_index: Option<usize>,
        _token: SearchToken,
        _window: &mut Window,
        _cx: &mut Context<Self>,
    ) {
    }

    fn query_suggestion(
        &mut self,
        _seed_query_override: Option<SeedQuerySetting>,
        _window: &mut Window,
        _cx: &mut Context<Self>,
    ) -> String {
        String::new()
    }

    fn activate_match(
        &mut self,
        _index: usize,
        _matches: &[Self::Match],
        _token: SearchToken,
        _window: &mut Window,
        _cx: &mut Context<Self>,
    ) {
    }

    fn select_matches(
        &mut self,
        _matches: &[Self::Match],
        _token: SearchToken,
        _window: &mut Window,
        _cx: &mut Context<Self>,
    ) {
    }

    /// A thread's timeline can't be edited, so there's nothing to replace.
    fn replace(
        &mut self,
        _: &Self::Match,
        _: &SearchQuery,
        _token: SearchToken,
        _window: &mut Window,
        _: &mut Context<Self>,
    ) {
    }

    fn find_matches(
        &mut self,
        _query: Arc<SearchQuery>,
        _window: &mut Window,
        _cx: &mut Context<Self>,
    ) -> Task<Vec<Self::Match>> {
        Task::ready(Vec::new())
    }

    fn active_match_index(
        &mut self,
        _direction: Direction,
        _matches: &[Self::Match],
        _token: SearchToken,
        _window: &mut Window,
        _cx: &mut Context<Self>,
    ) -> Option<usize> {
        None
    }
}

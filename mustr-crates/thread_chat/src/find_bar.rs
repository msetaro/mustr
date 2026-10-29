//! Find in thread (`mustr-docs/PARITY.md` §2.7, T3's `chat/ThreadFindBar.tsx` and
//! `chat/useThreadFind.ts`): Zed's buffer search bar, in every pane's toolbar
//! (`mustr/src/window.rs`), searches the item that `Item::as_searchable` returns, and `mod+f`
//! opens it over a thread ([`ThreadView`] binds it, since the composer isn't a full editor).
//!
//! T3 finds the matches (`orchestration.searchThread`), as its own find bar does: in the text of
//! the thread's messages and plans, ignoring case, across the whole history, not only what's
//! loaded. Each match is an occurrence in one entry (a message, or a plan), which mustr finds in
//! that item's Markdown to highlight it and scroll to it. T3 answers with up to 17 entries around
//! the one it picked, so mustr pages through the rest. On a server without the call, mustr finds
//! the same way in what's loaded. While the thread streams, the matches are found again, at most
//! every 300 milliseconds, as T3's are.

use crate::{
    backend::{Access, ThreadBackend, ThreadBackendExt as _, is_unsupported_rpc},
    model::{ItemKind, TimelineItem},
    thread_view::ThreadView,
};
use anyhow::Result;
use collections::{BTreeMap, HashMap, HashSet};
use gpui::{App, Context, Entity, EventEmitter, Focusable as _, Task, Window};
use project::search::SearchQuery;
use settings::SeedQuerySetting;
use std::{ops::Range, sync::Arc, time::Duration};
use t3_api::{
    rpc,
    types::{OrchestrationV2SearchThreadInput, OrchestrationV2SearchThreadResult},
};
use workspace::searchable::{
    Direction, SearchEvent, SearchOptions, SearchToken, SearchableItem, SearchableItemHandle,
};

/// How long find waits after the thread changes before it finds again (T3's 300 ms).
const REFRESH_DELAY: Duration = Duration::from_millis(300);
/// The most pages of matches mustr asks T3 for, 17 entries each.
const MAX_PAGES: usize = 40;
/// T3's longest query (`OrchestrationV2SearchThreadInput.query`).
const MAX_QUERY_CHARS: usize = 200;

/// One match: an occurrence of the query in one of T3's entries, and where it is in the
/// timeline once that's loaded.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ThreadMatch {
    /// T3's `entryId`: a message's id, or a plan's item id.
    pub entry_id: String,
    /// Which occurrence in the entry, from 0.
    pub occurrence: usize,
    /// The timeline item that shows the entry, when it's loaded.
    pub item_id: Option<String>,
    /// The occurrence in the item's Markdown, when it's found there.
    pub range: Option<Range<usize>>,
}

/// What find in thread keeps on a [`ThreadView`]: its matches and the active one.
#[derive(Default)]
pub(crate) struct FindState {
    /// What the bar searches for, while it does.
    query: Option<String>,
    /// The query the bar opens with next, from [`open_thread_with_find`].
    seed: Option<String>,
    /// The bar opens once the view has drawn, so the action reaches the workspace.
    deploy: bool,
    matches: Vec<ThreadMatch>,
    active: Option<usize>,
    /// The items whose Markdown has highlights.
    highlighted: HashSet<String>,
    /// The server doesn't have `orchestration.searchThread`, so mustr finds in what's loaded.
    local_only: bool,
    _refresh: Option<Task<()>>,
}

/// What [`ThreadView`]'s `Item::as_searchable` returns: the view, for a thread that's started.
pub(crate) fn searchable(
    thread_view: &ThreadView,
    view: &Entity<ThreadView>,
    _cx: &App,
) -> Option<Box<dyn SearchableItemHandle>> {
    thread_view
        .thread_id
        .is_some()
        .then(|| Box::new(view.clone()) as Box<dyn SearchableItemHandle>)
}

/// Folds case as T3's find does (`foldThreadSearchCase` in T3's
/// `packages/shared/src/threadSearch.ts`): lowercase, with a final sigma as any other, keeping
/// each character's length so offsets stay put.
fn fold_case(text: &str) -> String {
    text.chars()
        .map(|character| {
            let mut lower = character.to_lowercase();
            match (lower.next(), lower.next()) {
                (Some('ς'), None) => 'σ',
                (Some(lower), None) if lower.len_utf8() == character.len_utf8() => lower,
                _ => character,
            }
        })
        .collect()
}

/// Where `query` occurs in `text`, ignoring case, without overlaps (`findThreadSearchOccurrences`
/// in T3).
pub fn occurrences(text: &str, query: &str) -> Vec<Range<usize>> {
    let query = fold_case(query);
    if text.is_empty() || query.is_empty() {
        return Vec::new();
    }
    let folded = fold_case(text);
    let mut found = Vec::new();
    let mut cursor = 0;
    while let Some(offset) = folded[cursor..].find(&query) {
        let start = cursor + offset;
        found.push(start..start + query.len());
        cursor = start + query.len();
    }
    found
}

/// The text of an item find searches, as T3's does: user and agent messages, and plans.
fn searchable_text(item: &TimelineItem) -> Option<&str> {
    match &item.kind {
        ItemKind::UserMessage { text, .. } | ItemKind::AssistantMessage { text, .. } => Some(text),
        ItemKind::Plan { markdown, .. } => Some(markdown),
        _ => None,
    }
}

/// The matches in what's loaded, for a server without `orchestration.searchThread`.
fn local_matches(items: &[&TimelineItem], query: &str) -> Vec<ThreadMatch> {
    items
        .iter()
        .flat_map(|item| {
            let text = searchable_text(item).unwrap_or_default();
            occurrences(text, query)
                .into_iter()
                .enumerate()
                .map(|(occurrence, range)| ThreadMatch {
                    entry_id: item.entry_id.clone(),
                    occurrence,
                    item_id: Some(item.id.clone()),
                    range: Some(range),
                })
        })
        .collect()
}

/// Finds T3's matches in the timeline's items: each entry's item, and the occurrence in it.
fn place_matches(
    found: Vec<(String, usize)>,
    items: &[&TimelineItem],
    query: &str,
) -> Vec<ThreadMatch> {
    let by_entry = items
        .iter()
        .map(|item| (item.entry_id.as_str(), *item))
        .collect::<HashMap<_, _>>();
    let mut ranges = HashMap::<&str, Vec<Range<usize>>>::default();
    found
        .into_iter()
        .map(|(entry_id, occurrence)| {
            let item = by_entry.get(entry_id.as_str()).copied();
            let range = item.and_then(|item| {
                ranges
                    .entry(item.id.as_str())
                    .or_insert_with(|| {
                        occurrences(searchable_text(item).unwrap_or_default(), query)
                    })
                    .get(occurrence)
                    .cloned()
            });
            ThreadMatch {
                item_id: item.map(|item| item.id.clone()),
                entry_id,
                occurrence,
                range,
            }
        })
        .collect()
}

/// Every match T3 finds for `query` in the thread, in order, as `(entry id, occurrence)`. T3
/// lists up to 17 entries around the one it picked, so this asks again from the first match it
/// hasn't seen until it has them all.
async fn search_thread(
    backend: Arc<dyn ThreadBackend>,
    thread_id: String,
    query: String,
) -> Result<Vec<(String, usize)>> {
    let mut entries = BTreeMap::<usize, (String, usize)>::new();
    let mut total = None;
    let mut next = None::<usize>;
    for _ in 0..MAX_PAGES {
        let result: OrchestrationV2SearchThreadResult = backend
            .call::<rpc::OrchestrationSearchThread>(
                OrchestrationV2SearchThreadInput {
                    index: next.map(|index| index as i64),
                    offset: None,
                    query: query.clone(),
                    skills: None,
                    start: None,
                    thread_id: thread_id.clone(),
                },
                Access::Read,
            )
            .await?;
        let total_matches = result.total_matches.max(0) as usize;
        total = Some(total_matches);
        for entry in result.navigation.into_iter().flatten() {
            entries.insert(
                entry.start_index.max(0) as usize,
                (entry.entry_id, entry.count.max(0) as usize),
            );
        }
        // The first match no entry covers yet.
        let mut covered = 0;
        for (start, (_, count)) in &entries {
            if *start > covered {
                break;
            }
            covered = covered.max(start + count);
        }
        if covered >= total_matches || next == Some(covered) {
            break;
        }
        next = Some(covered);
    }
    let total = total.unwrap_or_default();
    let mut found = Vec::with_capacity(total);
    for (entry_id, count) in entries.into_values() {
        found.extend((0..count).map(|occurrence| (entry_id.clone(), occurrence)));
    }
    found.truncate(total);
    Ok(found)
}

/// Opens a thread's view in `workspace` (or activates it) with find in thread on `query`, as
/// a message-search match in the threads sidebar does: the find bar opens with the query, and
/// the timeline scrolls to the first match.
///
/// Deferred like [`crate::open_thread`], so a view can call it from its own handlers.
pub fn open_thread_with_find(
    thread_id: String,
    query: String,
    workspace: gpui::WeakEntity<workspace::Workspace>,
    window: &mut Window,
    cx: &mut App,
) {
    let title = crate::shell::ThreadShells::global(cx)
        .read(cx)
        .thread(&thread_id)
        .map(|thread| gpui::SharedString::from(thread.title.clone()))
        .unwrap_or_else(|| "Agent thread".into());
    window.defer(cx, move |window, cx| {
        let view = workspace.update(cx, |workspace, cx| {
            ThreadView::open(thread_id, title, workspace, window, cx)
        });
        let Ok(Some(view)) = view else {
            log::error!("couldn't open the agent thread");
            return;
        };
        let query = query
            .trim()
            .chars()
            .take(MAX_QUERY_CHARS)
            .collect::<String>();
        if query.is_empty() {
            return;
        }
        view.update(cx, |view, cx| {
            view.find.seed = Some(query);
            view.find.deploy = true;
            cx.notify();
        });
    });
}

impl ThreadView {
    /// Opens the find bar on the query [`open_thread_with_find`] left, once this view has drawn:
    /// Zed's bar handles `Deploy` on the workspace, which an action dispatched from a view not
    /// yet drawn doesn't reach. The bar takes the query as it opens (`query_suggestion`).
    pub(crate) fn deploy_pending_find(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if !std::mem::take(&mut self.find.deploy) {
            return;
        }
        let focus = self.composer.focus_handle(cx);
        window.defer(cx, move |window, cx| {
            window.focus(&focus, cx);
            window.dispatch_action(Box::new(zed_actions::buffer_search::Deploy::find()), cx);
        });
    }

    /// The query the find bar opens with next, for tests.
    pub fn find_seed_for_test(&self) -> Option<&str> {
        self.find.seed.as_deref()
    }

    /// The timeline's items, in order, that find searches.
    fn findable_items(&self) -> Vec<&TimelineItem> {
        let Some(state) = self.thread_state() else {
            return Vec::new();
        };
        state
            .items()
            .iter()
            .filter(|item| state.is_visible(item) && searchable_text(item).is_some())
            .collect()
    }

    /// The matches find has now, for tests.
    pub fn find_matches_for_test(&self) -> &[ThreadMatch] {
        &self.find.matches
    }

    /// Highlights `matches` in the timeline's Markdown, the active one apart.
    fn highlight_matches(
        &mut self,
        matches: &[ThreadMatch],
        active: Option<usize>,
        cx: &mut Context<Self>,
    ) {
        let mut by_item = HashMap::<String, (Vec<Range<usize>>, Option<usize>)>::default();
        for (index, found) in matches.iter().enumerate() {
            let (Some(item_id), Some(range)) = (&found.item_id, &found.range) else {
                continue;
            };
            let (ranges, active_in_item) = by_item.entry(item_id.clone()).or_default();
            if Some(index) == active {
                *active_in_item = Some(ranges.len());
            }
            ranges.push(range.clone());
        }
        for item_id in std::mem::take(&mut self.find.highlighted) {
            if !by_item.contains_key(&item_id)
                && let Some(markdown) = self.markdown.get(&item_id)
            {
                markdown.update(cx, |markdown, cx| markdown.clear_search_highlights(cx));
            }
        }
        for (item_id, (mut ranges, active)) in by_item {
            self.ensure_item_markdown(&item_id, cx);
            let Some(markdown) = self.markdown.get(&item_id) else {
                continue;
            };
            ranges.sort_by_key(|range| (range.start, range.end));
            markdown.update(cx, |markdown, cx| {
                markdown.set_search_highlights(ranges, active, cx)
            });
            self.find.highlighted.insert(item_id);
        }
        cx.notify();
    }

    /// Finds again soon, after the thread changed while find is open: the bar asks for the
    /// matches anew.
    pub(crate) fn refresh_find(&mut self, cx: &mut Context<Self>) {
        if self.find.query.is_none() || self.find._refresh.is_some() {
            return;
        }
        self.find._refresh = Some(cx.spawn(async move |this, cx| {
            cx.background_executor().timer(REFRESH_DELAY).await;
            this.update(cx, |this, cx| {
                this.find._refresh = None;
                if this.find.query.is_some() {
                    cx.emit(SearchEvent::MatchesInvalidated);
                }
            })
            .ok();
        }));
    }
}

impl EventEmitter<SearchEvent> for ThreadView {}

impl SearchableItem for ThreadView {
    type Match = ThreadMatch;

    /// T3's find ignores case and takes the query as text, so the bar offers no options.
    fn supported_options(&self) -> SearchOptions {
        SearchOptions {
            case: false,
            word: false,
            regex: false,
            replacement: false,
            selection: false,
            select_all: false,
            find_in_results: false,
        }
    }

    fn search_bar_visibility_changed(
        &mut self,
        visible: bool,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if !visible {
            self.find.query = None;
            self.find.active = None;
            self.find.matches.clear();
            self.highlight_matches(&[], None, cx);
        }
    }

    fn get_matches(&self, _window: &mut Window, _cx: &mut App) -> (Vec<Self::Match>, SearchToken) {
        (self.find.matches.clone(), SearchToken::default())
    }

    fn clear_matches(&mut self, _window: &mut Window, cx: &mut Context<Self>) {
        self.find.matches.clear();
        self.find.active = None;
        self.highlight_matches(&[], None, cx);
    }

    fn update_matches(
        &mut self,
        matches: &[Self::Match],
        active_match_index: Option<usize>,
        _token: SearchToken,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.find.matches = matches.to_vec();
        self.find.active = active_match_index;
        self.highlight_matches(matches, active_match_index, cx);
    }

    fn query_suggestion(
        &mut self,
        _seed_query_override: Option<SeedQuerySetting>,
        _window: &mut Window,
        _cx: &mut Context<Self>,
    ) -> String {
        self.find
            .seed
            .take()
            .or_else(|| self.find.query.clone())
            .unwrap_or_default()
    }

    /// Shows the match: highlights it as the active one, scrolls the timeline to its row, and
    /// inside a long message to the match itself, as T3's find bar does.
    fn activate_match(
        &mut self,
        index: usize,
        matches: &[Self::Match],
        _token: SearchToken,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(found) = matches.get(index) else {
            return;
        };
        self.find.active = Some(index);
        self.highlight_matches(matches, Some(index), cx);
        if let Some(row) = found
            .item_id
            .as_deref()
            .and_then(|item_id| self.row_of_item(item_id))
        {
            // A timeline following its end would snap back to it.
            self.list_state.pause_following_tail();
            self.list_state.scroll_to_reveal_item(row);
        }
        // The Markdown asks the timeline's list to scroll to the match once it's laid out,
        // which the list does for its rows (`request_autoscroll`).
        if let (Some(item_id), Some(range)) = (found.item_id.as_deref(), found.range.as_ref())
            && let Some(markdown) = self.markdown.get(item_id)
        {
            let start = range.start;
            markdown.update(cx, |markdown, cx| {
                markdown.request_autoscroll_to_source_index(start, cx)
            });
        }
        cx.emit(SearchEvent::ActiveMatchChanged);
        cx.notify();
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
        query: Arc<SearchQuery>,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Task<Vec<Self::Match>> {
        let query = query
            .as_str()
            .trim()
            .chars()
            .take(MAX_QUERY_CHARS)
            .collect::<String>();
        let Some(thread_id) = self.thread_id.clone().filter(|_| !query.is_empty()) else {
            self.find.query = None;
            return Task::ready(Vec::new());
        };
        if self.find.query.as_deref() != Some(query.as_str()) {
            self.find.active = None;
        }
        self.find.query = Some(query.clone());
        #[cfg(debug_assertions)]
        if crate::debug_fixture::active() {
            self.find.local_only = true;
        }
        if self.find.local_only {
            return Task::ready(local_matches(&self.findable_items(), &query));
        }
        let search = search_thread(self.backend.clone(), thread_id, query.clone());
        cx.spawn(async move |this, cx| {
            let found = search.await;
            this.update(cx, |this, _| match found {
                Ok(found) => place_matches(found, &this.findable_items(), &query),
                Err(error) => {
                    if is_unsupported_rpc(&error) {
                        this.find.local_only = true;
                    } else {
                        log::warn!("couldn't search the thread: {error:#}");
                    }
                    local_matches(&this.findable_items(), &query)
                }
            })
            .unwrap_or_default()
        })
    }

    /// The active match stays active as the matches are found again; a new query starts at
    /// the first, as T3's does.
    fn active_match_index(
        &mut self,
        _direction: Direction,
        matches: &[Self::Match],
        _token: SearchToken,
        _window: &mut Window,
        _cx: &mut Context<Self>,
    ) -> Option<usize> {
        if matches.is_empty() {
            return None;
        }
        let current = self
            .find
            .active
            .and_then(|active| self.find.matches.get(active))
            .and_then(|current| {
                matches.iter().position(|found| {
                    found.entry_id == current.entry_id && found.occurrence == current.occurrence
                })
            });
        Some(current.unwrap_or(0))
    }
}

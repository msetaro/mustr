//! mustr's status bar items: the T3 sync state and drafts on the left, and key hints for the
//! active item on the right. They replace Zed's items, which mustr never adds (Zed adds them in
//! `initialize_workspace`, crates/zed/src/zed.rs), and the dock buttons that `Workspace::new`
//! adds, which mustr removes; the icon rail replaces them.

use crate::{
    t3_connection::{ConnectionStatus, T3Connection},
    t3_engine::T3Mode,
};
use gpui::{
    App, AppContext as _, Context, Entity, Hsla, IntoElement, ParentElement as _, Pixels, Render,
    SharedString, Styled as _, Subscription, Task, Window, div, px,
};
use mustr_ui::{StatusHint, tokens, tokens::scaled};
use review::ReviewView;
use std::time::{Duration, Instant};
use theme::ActiveTheme as _;
use ui::{FluentBuilder as _, h_flex};
use workspace::{HideStatusItem, ItemHandle, StatusItemView, Workspace, dock::PanelButtons};

pub fn init(cx: &mut App) {
    cx.observe_new(|workspace: &mut Workspace, window, cx| {
        let Some(window) = window else {
            return;
        };
        let sync_status = cx.new(SyncStatus::new);
        let drafts = cx.new(|_| DraftsStatus::default());
        let key_hints = cx.new(|_| KeyHints::default());
        workspace.status_bar().update(cx, |status_bar, cx| {
            while let Some(index) = status_bar.position_of_item::<PanelButtons>() {
                status_bar.remove_item_at(index, cx);
            }
            status_bar.add_left_item(sync_status, window, cx);
            status_bar.add_left_item(drafts, window, cx);
            status_bar.add_right_item(key_hints, window, cx);
        });
    })
    .detach();
}

fn status_text(color: Hsla, text: impl Into<SharedString>) -> impl IntoElement {
    div()
        .px(scaled(px(5.)))
        .text_size(tokens::text_size(tokens::TEXT_SIZE_XSMALL))
        .text_color(color)
        .whitespace_nowrap()
        .child(text.into())
}

/// `● Synced 12s ago`, or why mustr isn't connected to T3.
pub struct SyncStatus {
    connection: Entity<T3Connection>,
    /// When the connection last delivered a fresh session, which reloads every view.
    synced_at: Option<Instant>,
    _refresh: Task<()>,
    _subscription: Subscription,
}

impl SyncStatus {
    fn new(cx: &mut Context<Self>) -> Self {
        let connection = T3Connection::global(cx);
        let subscription = cx.observe(&connection, |this, connection, cx| {
            this.synced_at = connection
                .read(cx)
                .status()
                .is_connected()
                .then(Instant::now);
            cx.notify();
        });
        let synced_at = connection
            .read(cx)
            .status()
            .is_connected()
            .then(Instant::now);
        // Re-render only when the label would change.
        let refresh = cx.spawn(async move |this, cx| {
            loop {
                let Ok(delay) = this.update(cx, |this, cx| {
                    cx.notify();
                    this.synced_at.map_or(Duration::from_secs(60), |synced_at| {
                        until_label_changes(synced_at.elapsed())
                    })
                }) else {
                    break;
                };
                cx.background_executor().timer(delay).await;
            }
        });
        Self {
            connection,
            synced_at,
            _refresh: refresh,
            _subscription: subscription,
        }
    }
}

/// "12s ago", "3m ago", "2h ago".
fn ago(elapsed: Duration) -> String {
    let seconds = elapsed.as_secs();
    match seconds {
        0..60 => format!("{seconds}s ago"),
        60..3600 => format!("{}m ago", seconds / 60),
        _ => format!("{}h ago", seconds / 3600),
    }
}

/// How long until [`ago`] reads differently.
fn until_label_changes(elapsed: Duration) -> Duration {
    let unit = match elapsed.as_secs() {
        0..60 => 1,
        60..3600 => 60,
        _ => 3600,
    };
    let unit = Duration::from_secs(unit);
    let into_unit = Duration::from_nanos((elapsed.as_nanos() % unit.as_nanos()) as u64);
    unit - into_unit
}

impl Render for SyncStatus {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let palette = mustr_ui::palette(cx);
        let (dot, text): (Hsla, SharedString) = match self.connection.read(cx).status() {
            ConnectionStatus::Connected { .. } => (
                palette.ok.hsla(),
                match self.synced_at {
                    Some(synced_at) => format!("Synced {}", ago(synced_at.elapsed())).into(),
                    None => "Synced".into(),
                },
            ),
            ConnectionStatus::Connecting(_) => (
                palette.text_muted.hsla(),
                if self.connection.read(cx).effective_mode() == T3Mode::Embedded {
                    "Starting agents engine…".into()
                } else {
                    "Connecting to T3…".into()
                },
            ),
            ConnectionStatus::NeedsPairing(_) => {
                (palette.warning.hsla(), "Not paired with T3".into())
            }
            ConnectionStatus::NeedsSetup(_) => {
                (palette.warning.hsla(), "Agents engine needs setup".into())
            }
            ConnectionStatus::Failed(_) => (cx.theme().status().error, "T3 offline".into()),
        };
        h_flex()
            .pl(scaled(px(5.)))
            .child(
                div()
                    .flex_none()
                    .size(scaled(px(6.)))
                    .rounded_full()
                    .bg(dot),
            )
            .child(status_text(palette.text_muted.hsla(), text))
    }
}

impl StatusItemView for SyncStatus {
    fn set_active_pane_item(
        &mut self,
        _: Option<&dyn ItemHandle>,
        _: &mut Window,
        _: &mut Context<Self>,
    ) {
    }

    fn hide_setting(&self, _: &App) -> Option<HideStatusItem> {
        None
    }
}

/// The active review's pending comments: the drafts that go with the review when it's submitted.
#[derive(Default)]
pub struct DraftsStatus {
    review: Option<Entity<ReviewView>>,
    _observe_review: Option<Subscription>,
}

impl DraftsStatus {
    fn label(count: usize) -> SharedString {
        match count {
            0 => "No drafts".into(),
            1 => "1 pending comment".into(),
            count => format!("{count} pending comments").into(),
        }
    }
}

impl Render for DraftsStatus {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let palette = mustr_ui::palette(cx);
        let count = self
            .review
            .as_ref()
            .map(|review| review.read(cx).added_draft_count(cx));
        div().when_some(count, |this, count| {
            let color = if count == 0 {
                palette.text_muted.hsla()
            } else {
                palette.accent.hsla()
            };
            this.child(status_text(color, Self::label(count)))
        })
    }
}

impl StatusItemView for DraftsStatus {
    fn set_active_pane_item(
        &mut self,
        item: Option<&dyn ItemHandle>,
        _: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let review = item.and_then(|item| item.downcast::<ReviewView>());
        self._observe_review = review
            .as_ref()
            .map(|review| cx.observe(review, |_, _, cx| cx.notify()));
        self.review = review;
        cx.notify();
    }

    fn hide_setting(&self, _: &App) -> Option<HideStatusItem> {
        None
    }
}

/// Key hints for the active item.
#[derive(Default)]
pub struct KeyHints {
    reviewing: bool,
}

const REVIEW_HINTS: [(&[&str], &str); 4] = [
    (&["j", "k"], "hunk"),
    (&["]", "f"], "file"),
    (&["c"], "comment"),
    (&["v"], "viewed"),
];

/// The room each key hint takes, what the status bar's left items keep, and what an update's
/// status takes, at mustr's design size.
const HINT_WIDTH: Pixels = px(75.);
const LEFT_ITEMS_WIDTH: Pixels = px(200.);
const UPDATE_STATUS_WIDTH: Pixels = px(130.);

/// How many of the key hints fit in a window `window_width` wide, next to `others` (such as an
/// update's status), at `rem_size`: in a narrow window, the first ones, so they don't run over
/// the sync state.
fn hints_that_fit(window_width: Pixels, others: Pixels, rem_size: Pixels) -> usize {
    let room = window_width - others - scaled(LEFT_ITEMS_WIDTH).to_pixels(rem_size);
    let count = (room / scaled(HINT_WIDTH).to_pixels(rem_size)).floor();
    (count.max(0.) as usize).min(REVIEW_HINTS.len())
}

impl Render for KeyHints {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let rem_size = window.rem_size();
        let others = if crate::updates::Updater::shows_status(cx) {
            scaled(UPDATE_STATUS_WIDTH).to_pixels(rem_size)
        } else {
            px(0.)
        };
        let count = hints_that_fit(window.viewport_size().width, others, rem_size);
        h_flex()
            .gap(scaled(px(14.)))
            .pr(scaled(px(5.)))
            .when(self.reviewing, |this| {
                this.children(
                    REVIEW_HINTS
                        .iter()
                        .take(count)
                        .map(|(keys, label)| StatusHint::new(keys.iter().copied(), *label)),
                )
            })
    }
}

impl StatusItemView for KeyHints {
    fn set_active_pane_item(
        &mut self,
        item: Option<&dyn ItemHandle>,
        _: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.reviewing = is_review(item);
        cx.notify();
    }

    fn hide_setting(&self, _: &App) -> Option<HideStatusItem> {
        None
    }
}

fn is_review(item: Option<&dyn ItemHandle>) -> bool {
    item.is_some_and(|item| item.downcast::<ReviewView>().is_some())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sync_age_labels() {
        assert_eq!(ago(Duration::from_millis(300)), "0s ago");
        assert_eq!(ago(Duration::from_secs(12)), "12s ago");
        assert_eq!(ago(Duration::from_secs(185)), "3m ago");
        assert_eq!(ago(Duration::from_secs(7300)), "2h ago");
    }

    /// On a MacBook's screen at mustr's default size every hint fits; in a half-screen window,
    /// the first two.
    #[test]
    fn key_hints_fit_the_window() {
        assert_eq!(hints_that_fit(px(1512.), px(0.), px(26.)), 4);
        assert_eq!(hints_that_fit(px(1470.), px(0.), px(26.)), 4);
        assert_eq!(hints_that_fit(px(756.), px(0.), px(26.)), 2);
        assert_eq!(hints_that_fit(px(756.), px(0.), px(13.)), 4);
        assert_eq!(hints_that_fit(px(300.), px(0.), px(26.)), 0);
        // With "Restart to update" next to them.
        assert_eq!(hints_that_fit(px(756.), px(260.), px(26.)), 0);
        assert_eq!(hints_that_fit(px(1512.), px(260.), px(26.)), 4);
    }

    #[test]
    fn drafts_labels() {
        assert_eq!(DraftsStatus::label(0), "No drafts");
        assert_eq!(DraftsStatus::label(1), "1 pending comment");
        assert_eq!(DraftsStatus::label(3), "3 pending comments");
    }

    #[test]
    fn sync_age_refreshes_when_the_label_changes() {
        assert_eq!(
            until_label_changes(Duration::from_millis(12_250)),
            Duration::from_millis(750)
        );
        assert_eq!(
            until_label_changes(Duration::from_secs(185)),
            Duration::from_secs(55)
        );
        assert_eq!(
            until_label_changes(Duration::from_secs(3600)),
            Duration::from_secs(3600)
        );
    }
}

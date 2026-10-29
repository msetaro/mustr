//! mustr's status bar items: the T3 connection's state and drafts on the left, and key hints for
//! the active item on the right. The connection's state is the only place it shows outside Home
//! (the title bar has no connection chip): clicking it reconnects, opens Settings › Connection
//! to switch servers, or Home to pair or set up. They replace Zed's items, which mustr never adds (Zed adds them in
//! `initialize_workspace`, crates/zed/src/zed.rs), and the dock buttons that `Workspace::new`
//! adds, which mustr removes; the icon rail replaces them.

use crate::{
    navigation, settings_page,
    t3_connection::{ConnectionStatus, EngineKind, EngineSetup, T3Connection},
    t3_engine::T3Mode,
    thread_git::{MenuSlot, menu_position},
};
use gpui::{
    App, AppContext as _, ClickEvent, Context, Entity, Hsla, InteractiveElement as _, IntoElement,
    MouseButton, ParentElement as _, Pixels, Render, SharedString, StatefulInteractiveElement as _,
    Styled as _, Subscription, Task, WeakEntity, Window, div, px,
};
use mustr_ui::{StatusHint, tokens, tokens::scaled};
use review::ReviewView;
use std::time::{Duration, Instant};
use theme::ActiveTheme as _;
use ui::{ContextMenu, FluentBuilder as _, Tooltip, h_flex};
use workspace::{HideStatusItem, ItemHandle, StatusItemView, Workspace, dock::PanelButtons};

pub fn init(cx: &mut App) {
    cx.observe_new(|workspace: &mut Workspace, window, cx| {
        let Some(window) = window else {
            return;
        };
        let weak_workspace = cx.weak_entity();
        let sync_status = cx.new(|cx| SyncStatus::new(weak_workspace, cx));
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

/// How the connection's state reads in the status bar.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Tone {
    Ok,
    Busy,
    Warning,
    Error,
}

/// The connection's state as the status bar shows it: a dot, a few words, the details in a
/// tooltip, and what its menu offers.
#[derive(Clone, Debug, PartialEq, Eq)]
struct SyncState {
    tone: Tone,
    text: SharedString,
    tooltip: SharedString,
    /// Reconnect, or Try again, when it can.
    retry: Option<&'static str>,
    /// Whether Home has what fixes it (pairing, setting up, the error's details).
    on_home: bool,
}

fn sync_state(status: &ConnectionStatus, embedded: bool, synced: Option<String>) -> SyncState {
    match status {
        ConnectionStatus::Connected {
            server_version,
            engine,
            ..
        } => {
            let version = server_version.as_ref().map_or_else(
                || "T3 Code".to_string(),
                |version| format!("T3 Code {version}"),
            );
            SyncState {
                tone: Tone::Ok,
                text: match synced {
                    Some(ago) => format!("Synced {ago}").into(),
                    None => "Synced".into(),
                },
                tooltip: match engine {
                    EngineKind::Embedded => format!("Running mustr's own engine, {version}").into(),
                    EngineKind::Installed | EngineKind::External => {
                        format!("Connected to {version}").into()
                    }
                },
                retry: Some("Reconnect"),
                on_home: false,
            }
        }
        ConnectionStatus::Connecting(step) => SyncState {
            tone: Tone::Busy,
            text: if embedded {
                "Starting agents engine…".into()
            } else {
                "Connecting to T3…".into()
            },
            tooltip: step.clone(),
            retry: None,
            on_home: false,
        },
        ConnectionStatus::NeedsPairing(reason) => SyncState {
            tone: Tone::Warning,
            text: "Not paired with T3".into(),
            tooltip: reason.clone(),
            retry: Some("Try again"),
            on_home: true,
        },
        ConnectionStatus::NeedsSetup(setup) => SyncState {
            tone: Tone::Warning,
            text: "Agents engine needs setup".into(),
            tooltip: match setup {
                EngineSetup::NotInstalled { .. } => "The agents engine isn't installed.".into(),
                EngineSetup::NotRunning { .. } => "T3 Code isn't running.".into(),
                EngineSetup::Incompatible { message, .. } => message.clone(),
            },
            retry: Some("Try again"),
            on_home: true,
        },
        ConnectionStatus::Failed(error) => SyncState {
            tone: Tone::Error,
            text: if embedded {
                "Agents engine stopped".into()
            } else {
                "T3 offline".into()
            },
            tooltip: error.clone(),
            retry: Some("Try again"),
            on_home: true,
        },
    }
}

/// `● Synced 12s ago`, or why mustr isn't connected to T3. Clicking it opens a menu to reconnect,
/// switch servers (Settings › Connection), or see the connection on Home.
pub struct SyncStatus {
    connection: Entity<T3Connection>,
    workspace: WeakEntity<Workspace>,
    /// When the connection last delivered a fresh session, which reloads every view.
    synced_at: Option<Instant>,
    menu: MenuSlot,
    _refresh: Task<()>,
    _subscription: Subscription,
}

impl SyncStatus {
    fn new(workspace: WeakEntity<Workspace>, cx: &mut Context<Self>) -> Self {
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
            workspace,
            synced_at,
            menu: MenuSlot::default(),
            _refresh: refresh,
            _subscription: subscription,
        }
    }

    fn state(&self, cx: &App) -> SyncState {
        let connection = self.connection.read(cx);
        sync_state(
            connection.status(),
            connection.effective_mode() == T3Mode::Embedded,
            self.synced_at.map(|synced_at| ago(synced_at.elapsed())),
        )
    }

    fn deploy_menu(&mut self, event: &ClickEvent, window: &mut Window, cx: &mut Context<Self>) {
        let state = self.state(cx);
        let connection = self.connection.clone();
        let workspace = self.workspace.clone();
        let menu = ContextMenu::build(window, cx, move |mut menu, _, _| {
            menu = menu.header(state.tooltip.clone());
            if let Some(retry) = state.retry {
                let connection = connection.clone();
                menu = menu.entry(retry, None, move |_, cx| {
                    connection.update(cx, |connection, cx| connection.reconnect(cx))
                });
            }
            let settings = workspace.clone();
            menu = menu.entry("Connection settings…", None, move |window, cx| {
                settings
                    .update(cx, |workspace, cx| {
                        settings_page::show(
                            workspace,
                            settings_page::Section::Connection,
                            None,
                            window,
                            cx,
                        )
                    })
                    .ok();
            });
            let home = workspace.clone();
            menu.entry(
                if state.on_home {
                    "Fix it in the Inbox"
                } else {
                    "Connection details"
                },
                None,
                move |window, cx| {
                    home.update(cx, |workspace, cx| {
                        navigation::show_home(workspace, window, cx)
                    })
                    .ok();
                },
            )
        });
        // The menu flips above the click, at the bottom of the window.
        let position = menu_position(event, window);
        self.menu
            .deploy(position, menu, window, cx, |this| &mut this.menu);
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
        let state = self.state(cx);
        let dot: Hsla = match state.tone {
            Tone::Ok => palette.ok.hsla(),
            Tone::Busy => palette.text_muted.hsla(),
            Tone::Warning => palette.warning.hsla(),
            Tone::Error => cx.theme().status().error,
        };
        // Only a problem is in color.
        let text_color = match state.tone {
            Tone::Error => cx.theme().status().error,
            _ => palette.text_muted.hsla(),
        };
        let menu_open = self.menu.is_open();
        let tooltip = format!("{}\nClick to reconnect or switch servers", state.tooltip);
        h_flex()
            .id("status-bar-connection")
            .pl(scaled(px(5.)))
            .rounded(scaled(tokens::CONTROL_RADIUS))
            .cursor_pointer()
            .hover(|style| style.bg(palette.surface_2.hsla()))
            .when(menu_open, |this| this.bg(palette.surface_2.hsla()))
            .when(!menu_open, |this| this.tooltip(Tooltip::text(tooltip)))
            .on_mouse_down(MouseButton::Left, |_, window, _| window.prevent_default())
            .on_click(cx.listener(|this, event, window, cx| this.deploy_menu(event, window, cx)))
            .child(
                div()
                    .flex_none()
                    .size(scaled(px(6.)))
                    .rounded_full()
                    .bg(dot),
            )
            .child(status_text(text_color, state.text))
            .children(self.menu.render())
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

    /// Every state but connecting can be acted on from the status bar; only an error is in color,
    /// and what fixes pairing or setup is on Home.
    #[test]
    fn the_connection_state_reads_and_acts() {
        let connected = ConnectionStatus::Connected {
            environment: "matts-macbook".into(),
            server_version: Some("0.1.3".into()),
            engine: EngineKind::Installed,
        };
        let state = sync_state(&connected, false, Some("12s ago".into()));
        assert_eq!(
            state,
            SyncState {
                tone: Tone::Ok,
                text: "Synced 12s ago".into(),
                tooltip: "Connected to T3 Code 0.1.3".into(),
                retry: Some("Reconnect"),
                on_home: false,
            }
        );
        // The machine's name isn't shown.
        assert!(!state.tooltip.contains("matts-macbook"));

        let failed = sync_state(
            &ConnectionStatus::Failed("Connection refused.".into()),
            false,
            None,
        );
        assert_eq!(
            (
                failed.tone,
                failed.text.as_ref(),
                failed.retry,
                failed.on_home
            ),
            (Tone::Error, "T3 offline", Some("Try again"), true)
        );
        assert_eq!(failed.tooltip, "Connection refused.");
        let stopped = sync_state(&ConnectionStatus::Failed("Exited.".into()), true, None);
        assert_eq!(stopped.text, "Agents engine stopped");

        let pairing = sync_state(
            &ConnectionStatus::NeedsPairing("Paste a credential.".into()),
            false,
            None,
        );
        assert_eq!(
            (pairing.tone, pairing.text.as_ref(), pairing.on_home),
            (Tone::Warning, "Not paired with T3", true)
        );
        let connecting = sync_state(&ConnectionStatus::Connecting("Pairing".into()), true, None);
        assert_eq!(
            (connecting.tone, connecting.text.as_ref(), connecting.retry),
            (Tone::Busy, "Starting agents engine…", None)
        );
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

//! Snoozing threads (`mustr-docs/PARITY.md` §2.15): T3's presets in the thread menu (in an hour,
//! in three hours, this evening, tomorrow, next week: `resolveSnoozePresets` in T3's client
//! runtime), and its custom snooze dialog (`components/CustomSnoozeDialog.tsx`), a date and
//! time or a duration. A snoozed thread waits in Snoozed until then, or until it needs the user;
//! Wake brings it back early.
//!
//! Snoozing sends `thread.snooze`, waking `thread.unsnooze`, through the installed backend, so
//! dry runs and read-only pairings refuse them. Snoozing the open thread moves on to the next one,
//! as settling does, and a toast offers Undo for 5 seconds.

use crate::{
    commands::new_id,
    organize::{self, UndoKind},
    shell::ThreadShells,
};
use chrono::{DateTime, Duration as ChronoDuration, Local, NaiveDate, NaiveTime, TimeZone, Utc};
use editor::Editor;
use gpui::{
    App, AppContext as _, Context, DismissEvent, Entity, EventEmitter, FocusHandle, Focusable,
    IntoElement, ParentElement as _, Render, SharedString, Styled as _, WeakEntity, Window,
    actions, rems,
};
use mustr_ui::{ButtonStyle, SegmentedControl, TextButton, palette, tokens, tokens::scaled};
use t3_api::types::OrchestrationV2ThreadShell;
use ui::{Color, Label, LabelCommon as _, LabelSize, prelude::*};
use workspace::{ModalView, Workspace};

actions!(
    thread_chat,
    [
        /// Snoozes the open agent thread until a time you choose.
        SnoozeThread,
    ]
);

const HOUR: i64 = 60 * 60;
const EVENING_HOUR: u32 = 18;
const MORNING_HOUR: u32 = 9;

/// One of T3's snooze presets: "In 1 hour (3:45 PM)".
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct SnoozePreset {
    pub label: &'static str,
    /// When it wakes, in the user's words: "3:45 PM", or "Mon 9:00 AM".
    pub when: String,
    /// When it wakes, as T3 takes it (UTC, ISO 8601).
    pub until: String,
}

/// A timestamp as T3 writes them: UTC, with milliseconds.
fn iso(time: DateTime<Utc>) -> String {
    time.format("%Y-%m-%dT%H:%M:%S%.3fZ").to_string()
}

/// `base`'s day (plus `days`) at `hour` o'clock, local to `base` (`snoozeAtHour` and
/// `addSnoozeDays`: calendar days, so a DST change doesn't skip one).
fn at_hour<Tz: TimeZone>(base: &DateTime<Tz>, days: u64, hour: u32) -> Option<DateTime<Tz>> {
    let day = base
        .date_naive()
        .checked_add_days(chrono::Days::new(days))?;
    let time = NaiveTime::from_hms_opt(hour, 0, 0)?;
    base.timezone()
        .from_local_datetime(&day.and_time(time))
        .earliest()
}

fn time_of_day<Tz: TimeZone>(time: &DateTime<Tz>) -> String
where
    Tz::Offset: std::fmt::Display,
{
    time.format("%-I:%M %p").to_string()
}

/// T3's snooze presets at `now` (`resolveSnoozePresets`): in 1 and 3 hours, this evening at 6
/// (if that's over an hour away), tomorrow at 9, and next Monday at 9 (unless that's tomorrow).
pub(crate) fn presets<Tz: TimeZone>(now: DateTime<Tz>) -> Vec<SnoozePreset>
where
    Tz::Offset: std::fmt::Display,
{
    let preset = |label, when: String, time: &DateTime<Tz>| SnoozePreset {
        label,
        when,
        until: iso(time.with_timezone(&Utc)),
    };
    let in_an_hour = now.clone() + ChronoDuration::seconds(HOUR);
    let in_three_hours = now.clone() + ChronoDuration::seconds(3 * HOUR);
    let mut presets = vec![
        preset("In 1 hour", time_of_day(&in_an_hour), &in_an_hour),
        preset("In 3 hours", time_of_day(&in_three_hours), &in_three_hours),
    ];
    if let Some(evening) = at_hour(&now, 0, EVENING_HOUR)
        && evening.clone() - now.clone() > ChronoDuration::seconds(HOUR)
    {
        presets.push(preset("This evening", time_of_day(&evening), &evening));
    }
    let tomorrow = at_hour(&now, 1, MORNING_HOUR);
    if let Some(tomorrow) = &tomorrow {
        presets.push(preset("Tomorrow", time_of_day(tomorrow), tomorrow));
    }
    use chrono::Datelike as _;
    let days_until_monday = match (8 - now.weekday().number_from_monday()) % 7 {
        0 => 7,
        days => days,
    };
    if let Some(next_week) = at_hour(&now, u64::from(days_until_monday), MORNING_HOUR)
        && Some(&next_week) != tomorrow.as_ref()
    {
        let when = format!("{} {}", next_week.format("%a"), time_of_day(&next_week));
        presets.push(preset("Next week", when, &next_week));
    }
    presets
}

/// Whether T3 would take a snooze now (`canSnooze`): not while the agent waits on the user or
/// is about to start, since that would wake it at once.
pub(crate) fn can_snooze(thread: &OrchestrationV2ThreadShell) -> bool {
    if thread.pending_runtime_request.is_some()
        || thread.pending_guest_message_count.unwrap_or(0) > 0
    {
        return false;
    }
    !matches!(
        crate::model::wire_string(&thread.status).as_str(),
        "preparing" | "queued" | "starting"
    )
}

/// What the custom dialog asks for.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum CustomMode {
    DateAndTime,
    Duration,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum DurationUnit {
    Minutes,
    Hours,
    Days,
}

impl DurationUnit {
    const ALL: [Self; 3] = [Self::Minutes, Self::Hours, Self::Days];

    fn seconds(self) -> f64 {
        match self {
            Self::Minutes => 60.,
            Self::Hours => 3600.,
            Self::Days => 86_400.,
        }
    }

    fn label(self) -> &'static str {
        match self {
            Self::Minutes => "Minutes",
            Self::Hours => "Hours",
            Self::Days => "Days",
        }
    }
}

/// When a custom snooze wakes (`resolveCustomSnooze`): a local `date` (`YYYY-MM-DD`) and `time`
/// (`HH:MM`), or `amount` of `unit` from `now`. `None` unless that's a real time in the future.
pub(crate) fn resolve_custom<Tz: TimeZone>(
    mode: CustomMode,
    date: &str,
    time: &str,
    amount: &str,
    unit: DurationUnit,
    now: DateTime<Tz>,
) -> Option<String> {
    let wake = match mode {
        CustomMode::Duration => {
            let amount = amount
                .trim()
                .parse::<f64>()
                .ok()
                .filter(|amount| amount.is_finite() && *amount > 0.)?;
            let milliseconds = (amount * unit.seconds() * 1000.).round();
            if milliseconds > 1e15 {
                return None;
            }
            now.with_timezone(&Utc) + ChronoDuration::milliseconds(milliseconds as i64)
        }
        CustomMode::DateAndTime => {
            let date = NaiveDate::parse_from_str(date.trim(), "%Y-%m-%d").ok()?;
            let time = NaiveTime::parse_from_str(time.trim(), "%H:%M").ok()?;
            // Times that don't exist (a DST gap) aren't taken; ambiguous ones are the first.
            now.timezone()
                .from_local_datetime(&date.and_time(time))
                .earliest()?
                .with_timezone(&Utc)
        }
    };
    (wake > now.with_timezone(&Utc)).then(|| iso(wake))
}

pub(crate) fn init(cx: &mut App) {
    cx.observe_new(|workspace: &mut Workspace, _, _| {
        workspace.register_action(|workspace, _: &SnoozeThread, window, cx| {
            let Some(view) = organize::active_thread_view(workspace, cx) else {
                return;
            };
            let Some(thread_id) = view.read(cx).thread_id().map(str::to_string) else {
                return;
            };
            let title = view.read(cx).title();
            let handle = workspace.weak_handle();
            workspace.toggle_modal(window, cx, move |window, cx| {
                SnoozeDialog::new(thread_id, title.to_string(), handle, window, cx)
            });
        });
    })
    .detach();
}

/// Snoozes a thread until `until` (`thread.snooze`). If the workspace shows it, it moves on to the
/// next thread that isn't settled or snoozed, as settling does; a toast offers Undo.
pub(crate) fn snooze_thread(
    thread_id: String,
    title: String,
    until: String,
    workspace: WeakEntity<Workspace>,
    window: &mut Window,
    cx: &mut App,
) {
    let open_view = organize::open_view_of(&workspace, &thread_id, cx);
    let next = open_view
        .is_some()
        .then(|| organize::next_after_settling(&thread_id, cx))
        .flatten();
    let before = ThreadShells::global(cx)
        .read(cx)
        .thread(&thread_id)
        .cloned();
    let send = organize::send(organize::snooze(&thread_id, &until, new_id()), cx);
    window
        .spawn(cx, async move |cx| {
            let result = send.await;
            cx.update(|window, cx| match result {
                Ok(()) => {
                    let steps = organize::undo_steps(UndoKind::Snoozed, before.as_ref());
                    organize::offer_undo(
                        UndoKind::Snoozed,
                        &thread_id,
                        &title,
                        steps,
                        &workspace,
                        cx,
                    );
                    if let Some(view) = open_view {
                        organize::advance(view, next, workspace, window, cx);
                    }
                }
                Err(error) => organize::report(
                    &workspace,
                    format!("Couldn't snooze the thread: {error:#}"),
                    cx,
                ),
            })
            .ok();
        })
        .detach();
}

/// Opens the custom snooze dialog for a thread.
pub(crate) fn show_snooze_dialog(
    thread_id: String,
    title: String,
    workspace: WeakEntity<Workspace>,
    window: &mut Window,
    cx: &mut App,
) {
    let handle = workspace.clone();
    workspace
        .update(cx, |workspace, cx| {
            workspace.toggle_modal(window, cx, move |window, cx| {
                SnoozeDialog::new(thread_id, title, handle, window, cx)
            })
        })
        .ok();
}

/// T3's custom snooze dialog: a date and time, or a duration.
pub(crate) struct SnoozeDialog {
    thread_id: String,
    title: String,
    workspace: WeakEntity<Workspace>,
    mode: CustomMode,
    date: Entity<Editor>,
    time: Entity<Editor>,
    amount: Entity<Editor>,
    unit: DurationUnit,
    error: Option<SharedString>,
}

fn field_editor(
    text: &str,
    placeholder: &str,
    window: &mut Window,
    cx: &mut App,
) -> Entity<Editor> {
    cx.new(|cx| {
        let mut editor = Editor::single_line(window, cx);
        editor.set_placeholder_text(placeholder, window, cx);
        editor.set_text(text.to_string(), window, cx);
        editor
    })
}

impl SnoozeDialog {
    fn new(
        thread_id: String,
        title: String,
        workspace: WeakEntity<Workspace>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        // An hour from now, as T3's dialog starts.
        let start = Local::now() + ChronoDuration::seconds(HOUR);
        let date = field_editor(
            &start.format("%Y-%m-%d").to_string(),
            "YYYY-MM-DD",
            window,
            cx,
        );
        let time = field_editor(&start.format("%H:%M").to_string(), "HH:MM", window, cx);
        let amount = field_editor("2", "2", window, cx);
        for editor in [&date, &time, &amount] {
            cx.subscribe(
                editor,
                |this: &mut Self, _, event: &editor::EditorEvent, cx| {
                    if let editor::EditorEvent::BufferEdited = event {
                        this.error = None;
                        cx.notify();
                    }
                },
            )
            .detach();
        }
        Self {
            thread_id,
            title,
            workspace,
            mode: CustomMode::DateAndTime,
            date,
            time,
            amount,
            unit: DurationUnit::Hours,
            error: None,
        }
    }

    #[cfg(test)]
    pub(crate) fn editors(&self) -> [&Entity<Editor>; 3] {
        [&self.date, &self.time, &self.amount]
    }

    #[cfg(test)]
    pub(crate) fn set_mode(
        &mut self,
        mode: CustomMode,
        unit: DurationUnit,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.mode = mode;
        self.unit = unit;
        self.focus_first(window, cx);
    }

    #[cfg(test)]
    pub(crate) fn error(&self) -> Option<&SharedString> {
        self.error.as_ref()
    }

    fn confirm(&mut self, _: &menu::Confirm, window: &mut Window, cx: &mut Context<Self>) {
        let text = |editor: &Entity<Editor>| editor.read(cx).text(cx);
        let until = resolve_custom(
            self.mode,
            &text(&self.date),
            &text(&self.time),
            &text(&self.amount),
            self.unit,
            Local::now(),
        );
        let Some(until) = until else {
            self.error = Some(match self.mode {
                CustomMode::DateAndTime => "Choose a valid date and time in the future.".into(),
                CustomMode::Duration => "Enter a positive duration.".into(),
            });
            cx.notify();
            return;
        };
        snooze_thread(
            self.thread_id.clone(),
            self.title.clone(),
            until,
            self.workspace.clone(),
            window,
            cx,
        );
        cx.emit(DismissEvent);
    }

    fn cancel(&mut self, _: &menu::Cancel, _: &mut Window, cx: &mut Context<Self>) {
        cx.emit(DismissEvent);
    }

    fn focus_first(&self, window: &mut Window, cx: &mut Context<Self>) {
        let editor = match self.mode {
            CustomMode::DateAndTime => &self.date,
            CustomMode::Duration => &self.amount,
        };
        window.focus(&editor.focus_handle(cx), cx);
    }
}

impl EventEmitter<DismissEvent> for SnoozeDialog {}

impl ModalView for SnoozeDialog {}

impl Focusable for SnoozeDialog {
    fn focus_handle(&self, cx: &App) -> FocusHandle {
        match self.mode {
            CustomMode::DateAndTime => self.date.focus_handle(cx),
            CustomMode::Duration => self.amount.focus_handle(cx),
        }
    }
}

/// A labeled text field of the dialog.
fn field(label: &'static str, editor: &Entity<Editor>, cx: &App) -> impl IntoElement {
    let palette = palette(cx);
    v_flex()
        .flex_1()
        .min_w_0()
        .gap_1()
        .child(Label::new(label).size(LabelSize::Small).color(Color::Muted))
        .child(
            div()
                .px_2()
                .py_1p5()
                .rounded(scaled(tokens::CONTROL_RADIUS))
                .border(tokens::BORDER_WIDTH)
                .border_color(palette.border.hsla())
                .bg(palette.background.hsla())
                .child(editor.clone()),
        )
}

impl Render for SnoozeDialog {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let palette = palette(cx);
        let this = cx.entity().downgrade();
        let modes = SegmentedControl::new("snooze-mode")
            .segment("Date and time")
            .segment("Duration")
            .selected(match self.mode {
                CustomMode::DateAndTime => 0,
                CustomMode::Duration => 1,
            })
            .on_select({
                let this = this.clone();
                move |index, window, cx| {
                    this.update(cx, |this, cx| {
                        this.mode = if index == 0 {
                            CustomMode::DateAndTime
                        } else {
                            CustomMode::Duration
                        };
                        this.error = None;
                        this.focus_first(window, cx);
                        cx.notify();
                    })
                    .ok();
                }
            });
        let fields = match self.mode {
            CustomMode::DateAndTime => h_flex()
                .gap_3()
                .child(field("Date", &self.date, cx))
                .child(field("Time", &self.time, cx))
                .into_any_element(),
            CustomMode::Duration => h_flex()
                .gap_3()
                .items_end()
                .child(field("Snooze for", &self.amount, cx))
                .child(
                    DurationUnit::ALL
                        .iter()
                        .fold(SegmentedControl::new("snooze-unit"), |control, unit| {
                            control.segment(unit.label())
                        })
                        .selected(
                            DurationUnit::ALL
                                .iter()
                                .position(|unit| *unit == self.unit)
                                .unwrap_or(1),
                        )
                        .on_select(move |index, _, cx| {
                            this.update(cx, |this, cx| {
                                this.unit = DurationUnit::ALL[index.min(2)];
                                cx.notify();
                            })
                            .ok();
                        }),
                )
                .into_any_element(),
        };
        // When a duration ends, as the user would say it.
        let wakes = (self.mode == CustomMode::Duration).then(|| {
            let amount = self.amount.read(cx).text(cx);
            match resolve_custom(self.mode, "", "", &amount, self.unit, Local::now()) {
                Some(until) => until
                    .parse::<DateTime<Utc>>()
                    .map(|until| {
                        let until = until.with_timezone(&Local);
                        format!("Wakes {}", until.format("%a %-I:%M %p"))
                    })
                    .unwrap_or_default(),
                None => "Enter a positive duration.".to_string(),
            }
        });
        v_flex()
            .key_context("SnoozeDialog")
            .on_action(cx.listener(Self::confirm))
            .on_action(cx.listener(Self::cancel))
            .w(rems(30.))
            .p_4()
            .gap_3()
            .rounded(scaled(tokens::CORNER_RADIUS))
            .border(tokens::BORDER_WIDTH)
            .border_color(palette.border.hsla())
            .bg(palette.surface.hsla())
            .shadow_lg()
            .child(
                v_flex()
                    .gap_0p5()
                    .child(
                        Label::new("Custom snooze")
                            .size(LabelSize::Large)
                            .weight(gpui::FontWeight::SEMIBOLD),
                    )
                    .child(
                        Label::new(format!(
                            "Choose when “{}” comes back to Active. It comes back sooner if the \
                             agent needs you.",
                            self.title
                        ))
                        .size(LabelSize::Small)
                        .color(Color::Muted),
                    ),
            )
            .child(modes)
            .child(fields)
            .when_some(wakes, |this, wakes| {
                this.child(
                    Label::new(wakes)
                        .size(LabelSize::XSmall)
                        .color(Color::Muted),
                )
            })
            .when_some(self.error.clone(), |this, error| {
                this.child(Label::new(error).size(LabelSize::Small).color(Color::Error))
            })
            .child(
                h_flex()
                    .justify_end()
                    .gap_2()
                    .child(
                        TextButton::new("snooze-cancel", "Cancel")
                            .on_click(cx.listener(|_, _, _, cx| cx.emit(DismissEvent))),
                    )
                    .child(
                        TextButton::new("snooze-confirm", "Snooze")
                            .style(ButtonStyle::Primary)
                            .on_click(cx.listener(|this, _, window, cx| {
                                this.confirm(&menu::Confirm, window, cx)
                            })),
                    ),
            )
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::FixedOffset;
    use pretty_assertions::assert_eq;

    fn at(text: &str) -> DateTime<FixedOffset> {
        DateTime::parse_from_rfc3339(text).unwrap()
    }

    fn summary(presets: &[SnoozePreset]) -> Vec<(String, String)> {
        presets
            .iter()
            .map(|preset| {
                (
                    format!("{} ({})", preset.label, preset.when),
                    preset.until.clone(),
                )
            })
            .collect()
    }

    #[test]
    fn presets_follow_t3s() {
        // A Tuesday afternoon, two hours behind UTC.
        let tuesday = presets(at("2026-10-27T14:20:00-02:00"));
        assert_eq!(
            summary(&tuesday),
            [
                (
                    "In 1 hour (3:20 PM)".into(),
                    "2026-10-27T17:20:00.000Z".into()
                ),
                (
                    "In 3 hours (5:20 PM)".into(),
                    "2026-10-27T19:20:00.000Z".into()
                ),
                (
                    "This evening (6:00 PM)".into(),
                    "2026-10-27T20:00:00.000Z".into()
                ),
                (
                    "Tomorrow (9:00 AM)".into(),
                    "2026-10-28T11:00:00.000Z".into()
                ),
                (
                    "Next week (Mon 9:00 AM)".into(),
                    "2026-11-02T11:00:00.000Z".into()
                ),
            ]
        );
        // Under an hour before six there's no "This evening"; on a Sunday, tomorrow is next
        // week's Monday, so there's no "Next week".
        let labels = |now| {
            presets(at(now))
                .into_iter()
                .map(|preset| preset.label)
                .collect::<Vec<_>>()
        };
        assert_eq!(
            labels("2026-11-01T17:30:00+00:00"),
            ["In 1 hour", "In 3 hours", "Tomorrow"]
        );
        // On a Monday, next week is seven days away.
        assert_eq!(
            presets(at("2026-11-02T08:00:00+00:00"))
                .last()
                .unwrap()
                .until,
            "2026-11-09T09:00:00.000Z"
        );
    }

    #[test]
    fn custom_snoozes_must_be_in_the_future() {
        let now = at("2026-10-27T14:20:00-02:00");
        let resolve =
            |mode, date, time, amount, unit| resolve_custom(mode, date, time, amount, unit, now);
        use CustomMode::*;
        use DurationUnit::*;
        assert_eq!(
            resolve(DateAndTime, "2026-10-28", "08:30", "", Hours).as_deref(),
            Some("2026-10-28T10:30:00.000Z")
        );
        assert_eq!(resolve(DateAndTime, "2026-10-27", "14:00", "", Hours), None);
        assert_eq!(resolve(DateAndTime, "2026-02-30", "08:30", "", Hours), None);
        assert_eq!(resolve(DateAndTime, "tomorrow", "08:30", "", Hours), None);
        assert_eq!(resolve(DateAndTime, "2026-10-28", "8.30", "", Hours), None);
        assert_eq!(
            resolve(Duration, "", "", "90", Minutes).as_deref(),
            Some("2026-10-27T17:50:00.000Z")
        );
        assert_eq!(
            resolve(Duration, "", "", "1.5", Days).as_deref(),
            Some("2026-10-29T04:20:00.000Z")
        );
        assert_eq!(resolve(Duration, "", "", "0", Hours), None);
        assert_eq!(resolve(Duration, "", "", "-2", Hours), None);
        assert_eq!(resolve(Duration, "", "", "soon", Hours), None);
    }
}

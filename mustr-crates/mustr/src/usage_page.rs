//! The usage dashboard (`mustr-docs/PARITY.md` §2.17, "Usage dashboard"): what the agents T3
//! Code runs have used, as T3's `mod+u` page shows it (`components/usage/UsagePage.tsx`).
//!
//! - **Cost and tokens** for the past 24 hours (by hour) or 7, 30 or 90 days (by day): the total,
//!   each agent's share, a chart over the window, the token totals and their kinds, and a
//!   breakdown by model or by day. Costs are T3's API estimates (LiteLLM's prices), not bills.
//! - **Limits:** each agent's subscription windows, from T3's provider status, as the thread
//!   header's usage panel shows them.
//!
//! The numbers come from `server.getUsageSummary`, which reads the agents' own session logs on
//! T3's machine and needs `diagnostics:read` ([`Access::DiagnosticsRead`]). When T3 is still
//! scanning, the page shows what it has and asks again with `awaitRefresh`. A server without the
//! call says so; a pairing without the scope says how to pair again.

#[cfg(test)]
mod tests;

use crate::t3_connection::T3Connection;
use anyhow::Result;
use chrono::{DateTime, Datelike as _, Duration as ChronoDuration, NaiveDate, Timelike as _, Utc};
use gpui::{
    AnyElement, App, AppContext as _, Context, Entity, EventEmitter, FocusHandle, Focusable,
    FontWeight, Hsla, InteractiveElement as _, IntoElement, KeyBinding, ParentElement as _, Render,
    SharedString, StatefulInteractiveElement as _, Styled as _, Subscription, Task, Window, div,
    px, relative,
};
use mustr_ui::{ButtonStyle, Card, SegmentedControl, TextButton, palette, tokens, tokens::scaled};
use std::{collections::BTreeMap, sync::Arc};
use t3_client::t3_api::{
    rpc,
    types::{
        UsageBucket, UsageCostSource, UsageProviderKind, UsageResolution, UsageSourceStatus,
        UsageSummary, UsageSummaryInput,
    },
};
use thread_chat::{
    Access, ServerConfigs, ThreadBackend, ThreadBackendExt as _,
    usage::{UsageLimits, usage_limits},
};
use ui::{
    Color, Headline, HeadlineSize, Icon, IconName, Label, LabelCommon as _, LabelSize, Tooltip,
    prelude::*,
};
use workspace::{
    Workspace,
    item::{Item, ItemEvent},
};

const CONTENT_MAX_WIDTH: f32 = 880.;
const CHART_HEIGHT: f32 = 120.;
/// The models the breakdown lists before folding the rest into "Other".
const BREAKDOWN_ROWS: usize = 12;

gpui::actions!(
    mustr,
    [
        /// Opens the usage dashboard: the tokens and API-estimated cost of the agents T3 Code
        /// runs, by agent, model and day, and their usage limits.
        OpenUsage,
    ]
);

/// Adds the page's action and `mod+u` (Agents' sidebar links to the page). Call once at startup, before windows open.
pub fn init(cx: &mut App) {
    cx.observe_new(|workspace: &mut Workspace, _, _| {
        workspace
            .register_action(|workspace, _: &OpenUsage, window, cx| show(workspace, window, cx));
    })
    .detach();
}

/// T3's `mod+u`, everywhere but in text fields and terminals (T3's `!terminalFocus`), which keep
/// their own `mod+u`: mustr's pages and the threads' views and list.
pub fn key_bindings() -> Vec<KeyBinding> {
    [
        "ThreadView",
        "ThreadsSidebar",
        "MustrHome",
        "MustrAgentSetup",
        "MustrSettings",
        "MustrSchedules",
        "MustrWorktrees",
        "MustrUsage",
    ]
    .into_iter()
    .map(|context| KeyBinding::new("secondary-u", OpenUsage, Some(context)))
    .collect()
}

/// Activates the usage page, or opens it.
pub fn show(workspace: &mut Workspace, window: &mut Window, cx: &mut Context<Workspace>) {
    let existing = workspace
        .panes()
        .iter()
        .find_map(|pane| pane.read(cx).items_of_type::<UsageView>().next());
    if let Some(existing) = existing {
        workspace.activate_item(&existing, true, true, window, cx);
        return;
    }
    let backend = thread_chat::backend(cx);
    let view = cx.new(|cx| UsageView::new(backend, cx));
    workspace.add_item_to_active_pane(Box::new(view), None, true, window, cx);
}

// The window and what's asked for.

/// The windows T3's page offers (`WINDOW_OPTIONS`).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(crate) enum Period {
    Day,
    Week,
    #[default]
    Month,
    Quarter,
}

impl Period {
    pub(crate) const ALL: [Self; 4] = [Self::Day, Self::Week, Self::Month, Self::Quarter];

    pub(crate) fn days(self) -> i64 {
        match self {
            Self::Day => 1,
            Self::Week => 7,
            Self::Month => 30,
            Self::Quarter => 90,
        }
    }

    fn label(self) -> &'static str {
        match self {
            Self::Day => "Past 24h",
            Self::Week => "7 days",
            Self::Month => "30 days",
            Self::Quarter => "90 days",
        }
    }

    fn hourly(self) -> bool {
        self == Self::Day
    }
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(crate) enum Metric {
    #[default]
    Cost,
    Tokens,
    Limits,
}

impl Metric {
    const ALL: [Self; 3] = [Self::Cost, Self::Tokens, Self::Limits];

    fn label(self) -> &'static str {
        match self {
            Self::Cost => "Cost",
            Self::Tokens => "Tokens",
            Self::Limits => "Limits",
        }
    }
}

/// Where "today" is: T3 counts days in the zone the client names.
#[derive(Clone, Debug)]
pub(crate) struct Clock {
    pub now: DateTime<Utc>,
    /// The IANA name of the zone, or `UTC` when the system's can't be named.
    pub time_zone: String,
    /// The zone's calendar day of an instant.
    pub local_day: fn(DateTime<Utc>) -> NaiveDate,
}

impl Clock {
    pub(crate) fn system() -> Self {
        match iana_time_zone::get_timezone() {
            Ok(time_zone) if !time_zone.trim().is_empty() => Self {
                now: Utc::now(),
                time_zone,
                local_day: |instant| instant.with_timezone(&chrono::Local).date_naive(),
            },
            _ => Self {
                now: Utc::now(),
                time_zone: "UTC".into(),
                local_day: |instant| instant.date_naive(),
            },
        }
    }

    /// A clock in UTC, for tests.
    #[cfg(test)]
    pub(crate) fn utc(now: &str) -> Self {
        Self {
            now: DateTime::parse_from_rfc3339(now)
                .unwrap()
                .with_timezone(&Utc),
            time_zone: "UTC".into(),
            local_day: |instant| instant.date_naive(),
        }
    }
}

/// What `server.getUsageSummary` gets for `period` (`makeWindow` in T3's `usageFormat.ts`): the
/// calendar days up to today, or for the past 24 hours, the minute-rounded span by the hour.
pub(crate) fn summary_input(
    period: Period,
    clock: &Clock,
    await_refresh: bool,
) -> UsageSummaryInput {
    let until_day = (clock.local_day)(clock.now);
    let day = |day: NaiveDate| day.format("%Y-%m-%d").to_string();
    let await_refresh = await_refresh.then_some(true);
    if period.hourly() {
        let until = clock
            .now
            .with_second(0)
            .and_then(|now| now.with_nanosecond(0))
            .unwrap_or(clock.now);
        let since = until - ChronoDuration::hours(24);
        let iso = |instant: DateTime<Utc>| instant.format("%Y-%m-%dT%H:%M:%S%.3fZ").to_string();
        return UsageSummaryInput {
            await_refresh,
            resolution: Some(UsageResolution::Hour),
            since_day: day((clock.local_day)(since)),
            since_time: Some(iso(since)),
            time_zone: clock.time_zone.clone(),
            until_day: day((clock.local_day)(until)),
            until_time: Some(iso(until)),
        };
    }
    let since_day = until_day - ChronoDuration::days(period.days() - 1);
    UsageSummaryInput {
        await_refresh,
        resolution: Some(UsageResolution::Day),
        since_day: day(since_day),
        since_time: None,
        time_zone: clock.time_zone.clone(),
        until_day: day(until_day),
        until_time: None,
    }
}

// What the page shows, from T3's buckets (`mergeUsage` in T3's `usageMerge.ts`, for one
// environment).

#[derive(Clone, Debug, Default, PartialEq)]
pub(crate) struct TokenTotals {
    pub uncached_input: i64,
    pub cached_input: i64,
    pub cache_creation: i64,
    pub output: i64,
}

impl TokenTotals {
    /// Processed tokens. Reasoning tokens are part of output, so they aren't added again.
    pub(crate) fn total(&self) -> i64 {
        self.uncached_input + self.cached_input + self.cache_creation + self.output
    }

    fn add(&mut self, bucket: &UsageBucket) {
        self.uncached_input += bucket.totals.uncached_input_tokens;
        self.cached_input += bucket.totals.cached_input_tokens;
        self.cache_creation += bucket.totals.cache_creation_tokens;
        self.output += bucket.totals.output_tokens;
    }
}

#[derive(Clone, Debug, PartialEq)]
pub(crate) struct ProviderTotals {
    pub provider: UsageProviderKind,
    pub cost_usd: f64,
    pub tokens: i64,
    pub sessions: i64,
}

#[derive(Clone, Debug, PartialEq)]
pub(crate) struct ModelTotals {
    pub provider: UsageProviderKind,
    pub model: String,
    pub cost_usd: f64,
    pub tokens: i64,
    records: i64,
    unpriced_records: i64,
}

impl ModelTotals {
    /// Every record of the model was unpriced (`isModelCostUnknown`).
    pub(crate) fn cost_unknown(&self) -> bool {
        self.records > 0 && self.unpriced_records >= self.records
    }
}

/// A day, or an hour of the past 24.
#[derive(Clone, Debug, PartialEq)]
pub(crate) struct Slot {
    /// `2026-10-28`, or the hour's start (`2026-10-28T14:00:00.000Z`).
    pub key: String,
    pub label: String,
    pub cost_usd: f64,
    pub tokens: i64,
    /// Each agent's part, in the agents' order.
    pub by_provider: Vec<(UsageProviderKind, f64, i64)>,
}

impl Slot {
    fn value(&self, metric: Metric) -> f64 {
        match metric {
            Metric::Tokens => self.tokens as f64,
            _ => self.cost_usd,
        }
    }
}

#[derive(Clone, Debug, Default, PartialEq)]
pub(crate) struct Totals {
    pub cost_usd: f64,
    pub cache_savings_usd: f64,
    pub tokens: TokenTotals,
    pub sessions: i64,
    records: i64,
    unpriced_records: i64,
    /// By cost, then tokens, most first.
    pub providers: Vec<ProviderTotals>,
    pub models: Vec<ModelTotals>,
    /// Every day (or hour) of the window, oldest first, with or without activity.
    pub slots: Vec<Slot>,
    /// What T3 said about the session logs it couldn't read in full.
    pub notices: Vec<String>,
    /// T3 is still scanning some logs, so the totals may grow.
    pub refreshing: bool,
}

impl Totals {
    /// The share of records T3 couldn't price, which the cost leaves out.
    pub(crate) fn unpriced_share(&self) -> f64 {
        if self.records == 0 {
            0.
        } else {
            self.unpriced_records as f64 / self.records as f64
        }
    }

    fn value(&self, metric: Metric) -> f64 {
        match metric {
            Metric::Tokens => self.tokens.total() as f64,
            _ => self.cost_usd,
        }
    }
}

/// The agents in T3's order (`PROVIDER_ORDER`), for colors and stacking that don't depend on
/// rank.
const PROVIDER_ORDER: [&str; 8] = [
    "codex",
    "claude",
    "grok",
    "cursor",
    "opencode",
    "antigravity",
    "pi",
    "muse",
];

fn provider_rank(provider: &UsageProviderKind) -> usize {
    PROVIDER_ORDER
        .iter()
        .position(|kind| *kind == provider.as_str())
        .unwrap_or(PROVIDER_ORDER.len())
}

/// T3's names for the agents (`PROVIDER_PRESENTATION`).
pub(crate) fn provider_label(provider: &UsageProviderKind) -> String {
    match provider {
        UsageProviderKind::Codex => "Codex".into(),
        UsageProviderKind::Claude => "Claude Code".into(),
        UsageProviderKind::Grok => "Grok Build".into(),
        UsageProviderKind::Cursor => "Cursor".into(),
        UsageProviderKind::Opencode => "OpenCode".into(),
        UsageProviderKind::Antigravity => "Antigravity".into(),
        UsageProviderKind::Pi => "Pi".into(),
        UsageProviderKind::Muse => "Muse Code".into(),
        UsageProviderKind::Unrecognized(kind) => kind.clone(),
    }
}

/// T3's colors for the agents, with mustr's ink for Codex's and Grok's foreground colors.
fn provider_color(provider: &UsageProviderKind, cx: &App) -> Hsla {
    let palette = palette(cx);
    let hex = |rgb: u32| gpui::rgb(rgb).into();
    match provider {
        UsageProviderKind::Codex => palette.text.hsla(),
        UsageProviderKind::Claude => palette.agent.hsla(),
        UsageProviderKind::Grok => palette.text_muted.hsla(),
        UsageProviderKind::Cursor => hex(0x8b8b8b),
        UsageProviderKind::Opencode => hex(0x5b9bbd),
        UsageProviderKind::Antigravity => hex(0x8c7bd1),
        UsageProviderKind::Pi => hex(0x4d9f8a),
        UsageProviderKind::Muse => hex(0x0866ff),
        UsageProviderKind::Unrecognized(_) => palette.border.hsla(),
    }
}

/// Adds up T3's buckets for the window `input` asked for.
pub(crate) fn totals(summary: &UsageSummary, input: &UsageSummaryInput) -> Totals {
    let mut totals = Totals::default();
    let mut providers: BTreeMap<String, ProviderTotals> = BTreeMap::new();
    let mut models: BTreeMap<(String, String), ModelTotals> = BTreeMap::new();
    let hourly = input.resolution == Some(UsageResolution::Hour);
    let mut slots = window_slots(input);

    for source in &summary.sources {
        let provider = source.fingerprint.provider.clone();
        let entry = providers
            .entry(provider.as_str().to_string())
            .or_insert_with(|| ProviderTotals {
                provider,
                cost_usd: 0.,
                tokens: 0,
                sessions: 0,
            });
        entry.sessions += source.distinct_sessions;
        totals.sessions += source.distinct_sessions;
        totals.refreshing |= source.refreshing == Some(true);
        if matches!(
            source.status,
            UsageSourceStatus::Partial | UsageSourceStatus::Failed
        ) {
            let name = provider_label(&source.fingerprint.provider);
            totals.notices.push(match &source.message {
                Some(message) if !message.trim().is_empty() => {
                    format!("{name}: {}", message.trim())
                }
                _ if source.status == UsageSourceStatus::Failed => {
                    format!("{name}: T3 Code couldn't read its session logs.")
                }
                _ => format!(
                    "{name}: some session logs couldn't be read, so its totals are partial."
                ),
            });
        }
    }

    for bucket in &summary.buckets {
        let cost = bucket.cost_usd.0;
        let mut tokens = TokenTotals::default();
        tokens.add(bucket);
        let bucket_tokens = tokens.total();
        totals.cost_usd += cost;
        totals.cache_savings_usd += bucket.cache_savings_usd.0;
        totals.tokens.add(bucket);
        totals.records += bucket.records;
        totals.unpriced_records += bucket.unpriced_records;

        let provider = providers
            .entry(bucket.provider.as_str().to_string())
            .or_insert_with(|| ProviderTotals {
                provider: bucket.provider.clone(),
                cost_usd: 0.,
                tokens: 0,
                sessions: 0,
            });
        provider.cost_usd += cost;
        provider.tokens += bucket_tokens;

        let model = models
            .entry((bucket.provider.as_str().to_string(), bucket.model.clone()))
            .or_insert_with(|| ModelTotals {
                provider: bucket.provider.clone(),
                model: bucket.model.clone(),
                cost_usd: 0.,
                tokens: 0,
                records: 0,
                unpriced_records: 0,
            });
        model.cost_usd += cost;
        model.tokens += bucket_tokens;
        model.records += bucket.records;
        model.unpriced_records += bucket.unpriced_records;
        if bucket.cost_source == UsageCostSource::Unpriced && bucket.unpriced_records == 0 {
            model.unpriced_records += bucket.records;
        }

        let key = if hourly {
            bucket.hour_start.as_deref().and_then(hour_key)
        } else {
            Some(bucket.day.clone())
        };
        if let Some(slot) = key.and_then(|key| slots.iter_mut().find(|slot| slot.key == key)) {
            slot.cost_usd += cost;
            slot.tokens += bucket_tokens;
            match slot
                .by_provider
                .iter_mut()
                .find(|(provider, _, _)| *provider == bucket.provider)
            {
                Some((_, slot_cost, slot_tokens)) => {
                    *slot_cost += cost;
                    *slot_tokens += bucket_tokens;
                }
                None => {
                    slot.by_provider
                        .push((bucket.provider.clone(), cost, bucket_tokens));
                    slot.by_provider
                        .sort_by_key(|(provider, _, _)| provider_rank(provider));
                }
            }
        }
    }

    let mut providers = providers
        .into_values()
        .filter(|provider| provider.tokens > 0 || provider.cost_usd > 0. || provider.sessions > 0)
        .collect::<Vec<_>>();
    providers.sort_by(|a, b| {
        b.cost_usd
            .total_cmp(&a.cost_usd)
            .then(b.tokens.cmp(&a.tokens))
            .then(provider_rank(&a.provider).cmp(&provider_rank(&b.provider)))
    });
    let mut models = models.into_values().collect::<Vec<_>>();
    models.sort_by(|a, b| {
        b.cost_usd
            .total_cmp(&a.cost_usd)
            .then(b.tokens.cmp(&a.tokens))
            .then(a.model.cmp(&b.model))
    });
    totals.providers = providers;
    totals.models = models;
    totals.slots = slots;
    totals
}

/// The window's days (or hours), oldest first, each still empty.
fn window_slots(input: &UsageSummaryInput) -> Vec<Slot> {
    let empty = |key: String, label: String| Slot {
        key,
        label,
        cost_usd: 0.,
        tokens: 0,
        by_provider: Vec::new(),
    };
    if input.resolution == Some(UsageResolution::Hour) {
        let Some(since) = input
            .since_time
            .as_deref()
            .and_then(|since| DateTime::parse_from_rfc3339(since).ok())
        else {
            return Vec::new();
        };
        let since = since.with_timezone(&Utc);
        let first = since
            .with_minute(0)
            .and_then(|hour| hour.with_second(0))
            .and_then(|hour| hour.with_nanosecond(0))
            .unwrap_or(since);
        let hours = if first == since { 24 } else { 25 };
        return (0..hours)
            .map(|offset| {
                let hour = first + ChronoDuration::hours(offset);
                empty(
                    hour.format("%Y-%m-%dT%H:00:00.000Z").to_string(),
                    hour_label(hour),
                )
            })
            .collect();
    }
    let parse = |day: &str| NaiveDate::parse_from_str(day, "%Y-%m-%d").ok();
    let (Some(since), Some(until)) = (parse(&input.since_day), parse(&input.until_day)) else {
        return Vec::new();
    };
    since
        .iter_days()
        .take_while(|day| *day <= until)
        .map(|day| empty(day.format("%Y-%m-%d").to_string(), day_label(day)))
        .collect()
}

/// An hour's start as the slots key it: T3 sends ISO times, with or without milliseconds.
fn hour_key(hour_start: &str) -> Option<String> {
    let hour = DateTime::parse_from_rfc3339(hour_start)
        .ok()?
        .with_timezone(&Utc);
    Some(hour.format("%Y-%m-%dT%H:00:00.000Z").to_string())
}

fn day_label(day: NaiveDate) -> String {
    day.format("%b %-d").to_string()
}

fn hour_label(hour: DateTime<Utc>) -> String {
    hour.with_timezone(&chrono::Local)
        .format("%-I %p")
        .to_string()
}

// Numbers as T3 writes them (`packages/shared/src/usageFormat.ts`).

/// `$1,234.56`.
pub(crate) fn format_usd(value: f64) -> String {
    let cents = (value.abs() * 100.).round() as u64;
    let sign = if value < 0. && cents > 0 { "-" } else { "" };
    format!("{sign}${}.{:02}", group_thousands(cents / 100), cents % 100)
}

/// `1,234`.
pub(crate) fn format_count(value: i64) -> String {
    let sign = if value < 0 { "-" } else { "" };
    format!("{sign}{}", group_thousands(value.unsigned_abs()))
}

fn group_thousands(value: u64) -> String {
    let digits = value.to_string();
    let mut grouped = String::new();
    for (index, digit) in digits.chars().enumerate() {
        if index > 0 && (digits.len() - index).is_multiple_of(3) {
            grouped.push(',');
        }
        grouped.push(digit);
    }
    grouped
}

/// `1.50M`, `12.3K`, `950`: two decimals under 10, one under 100, and none above, with all-zero
/// decimals dropped.
pub(crate) fn format_tokens(value: i64) -> String {
    let abs = value.unsigned_abs() as f64;
    let scaled = |divisor: f64, suffix: &str| {
        let scaled = value as f64 / divisor;
        let digits = if scaled.abs() >= 100. {
            0
        } else if scaled.abs() >= 10. {
            1
        } else {
            2
        };
        let text = format!("{scaled:.digits$}");
        let text = match text.split_once('.') {
            Some((whole, decimals)) if decimals.chars().all(|digit| digit == '0') => {
                whole.to_string()
            }
            _ => text,
        };
        format!("{text}{suffix}")
    };
    if abs >= 1e12 {
        scaled(1e12, "T")
    } else if abs >= 1e9 {
        scaled(1e9, "B")
    } else if abs >= 1e6 {
        scaled(1e6, "M")
    } else if abs >= 1e3 {
        scaled(1e3, "K")
    } else {
        format_count(value)
    }
}

/// `12.5%`, or `<0.1%` for a sliver.
pub(crate) fn format_percent(share: f64) -> String {
    let percent = share * 100.;
    if percent > 0. && percent < 0.1 {
        "<0.1%".into()
    } else {
        format!("{percent:.1}%")
    }
}

fn format_value(value: f64, metric: Metric) -> String {
    match metric {
        Metric::Tokens => format_tokens(value.round() as i64),
        _ => format_usd(value),
    }
}

fn plural(count: i64, one: &str, many: &str) -> String {
    format!(
        "{} {}",
        format_count(count),
        if count == 1 { one } else { many }
    )
}

// The page.

enum Loadable {
    Loading,
    Loaded(Totals),
    Failed(SharedString),
    /// The server doesn't have `server.getUsageSummary`.
    Unsupported,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
enum Breakdown {
    #[default]
    Model,
    Time,
}

pub struct UsageView {
    focus_handle: FocusHandle,
    backend: Option<Arc<dyn ThreadBackend>>,
    configs: Entity<ServerConfigs>,
    period: Period,
    metric: Metric,
    breakdown: Breakdown,
    summary: Loadable,
    /// A load is in flight; the last numbers stay up meanwhile.
    loading: bool,
    /// The connection generation the numbers are from.
    generation: Option<usize>,
    /// When the numbers were read, for "Read at …".
    read_at: Option<String>,
    load: Option<Task<()>>,
    /// Where "today" is: the system's zone, or a fixed one in tests.
    clock: fn() -> Clock,
    _subscriptions: Vec<Subscription>,
}

impl UsageView {
    pub(crate) fn new(backend: Option<Arc<dyn ThreadBackend>>, cx: &mut Context<Self>) -> Self {
        Self::with_clock(backend, Clock::system, cx)
    }

    pub(crate) fn with_clock(
        backend: Option<Arc<dyn ThreadBackend>>,
        clock: fn() -> Clock,
        cx: &mut Context<Self>,
    ) -> Self {
        let configs = ServerConfigs::global(cx);
        let mut subscriptions = vec![cx.observe(&configs, |_, _, cx| cx.notify())];
        if let Some(connection) = T3Connection::try_global(cx) {
            subscriptions.push(cx.observe(&connection, |this, connection, cx| {
                // Another server, or the same one after a reconnect: read it again.
                let connection = connection.read(cx);
                if connection.session().is_some()
                    && this.generation != Some(connection.generation())
                {
                    this.reload(false, cx);
                }
            }));
        }
        let mut this = Self {
            focus_handle: cx.focus_handle(),
            backend,
            configs,
            period: Period::default(),
            metric: Metric::default(),
            breakdown: Breakdown::default(),
            summary: Loadable::Loading,
            loading: false,
            generation: None,
            read_at: None,
            load: None,
            clock,
            _subscriptions: subscriptions,
        };
        this.reload(false, cx);
        this
    }

    fn blocked(&self) -> Option<String> {
        match &self.backend {
            Some(backend) => backend.blocked_reason(Access::DiagnosticsRead),
            None => Some("mustr isn't connected to T3 Code".into()),
        }
    }

    pub(crate) fn set_period(&mut self, period: Period, cx: &mut Context<Self>) {
        if self.period != period {
            self.period = period;
            self.reload(false, cx);
        }
    }

    pub(crate) fn set_metric(&mut self, metric: Metric, cx: &mut Context<Self>) {
        self.metric = metric;
        cx.notify();
    }

    /// Reads the window's usage. `await_refresh` waits for T3 to finish scanning.
    pub(crate) fn reload(&mut self, await_refresh: bool, cx: &mut Context<Self>) {
        self.generation =
            T3Connection::try_global(cx).map(|connection| connection.read(cx).generation());
        let Some(backend) = self.backend.clone() else {
            self.summary = Loadable::Failed("mustr isn't connected to T3 Code".into());
            return;
        };
        if self.blocked().is_some() {
            self.load = None;
            self.loading = false;
            cx.notify();
            return;
        }
        let input = summary_input(self.period, &(self.clock)(), await_refresh);
        self.loading = true;
        cx.notify();
        let period = self.period;
        self.load = Some(cx.spawn(async move |this, cx| {
            let result = backend
                .call::<rpc::ServerGetUsageSummary>(input.clone(), Access::DiagnosticsRead)
                .await;
            this.update(cx, |this, cx| {
                if this.period != period {
                    return;
                }
                this.apply(result, &input, cx);
            })
            .ok();
        }));
    }

    fn apply(
        &mut self,
        result: Result<UsageSummary>,
        input: &UsageSummaryInput,
        cx: &mut Context<Self>,
    ) {
        self.loading = false;
        match result {
            Ok(summary) => {
                let totals = totals(&summary, input);
                let refreshing = totals.refreshing && input.await_refresh.is_none();
                self.read_at = Some(summary.read_at);
                self.summary = Loadable::Loaded(totals);
                // T3 answered from what it had; ask again once it has scanned the rest.
                if refreshing {
                    self.reload(true, cx);
                }
            }
            Err(error) if thread_chat::is_unsupported_rpc(&error) => {
                self.summary = Loadable::Unsupported;
            }
            Err(error) => {
                log::warn!("couldn't read T3's usage: {error:#}");
                self.summary =
                    Loadable::Failed(format!("Couldn't read T3 Code's usage: {error:#}").into());
            }
        }
        cx.notify();
    }

    #[cfg(test)]
    pub(crate) fn totals(&self) -> Option<&Totals> {
        match &self.summary {
            Loadable::Loaded(totals) => Some(totals),
            _ => None,
        }
    }
}

// Rendering.

fn muted(text: impl Into<SharedString>) -> Label {
    Label::new(text.into())
        .size(LabelSize::Small)
        .color(Color::Muted)
}

fn section(
    title: impl Into<SharedString>,
    trailing: Option<AnyElement>,
    body: AnyElement,
) -> AnyElement {
    v_flex()
        .w_full()
        .gap_1p5()
        .child(
            h_flex()
                .w_full()
                .gap_2()
                .child(Label::new(title.into()).weight(FontWeight::SEMIBOLD))
                .child(div().flex_1())
                .children(trailing),
        )
        .child(body)
        .into_any_element()
}

fn dot(color: Hsla) -> impl IntoElement {
    div()
        .flex_none()
        .size(scaled(px(8.)))
        .rounded_full()
        .bg(color)
}

impl UsageView {
    fn render_controls(&self, cx: &mut Context<Self>) -> AnyElement {
        let this = cx.entity().downgrade();
        let periods = Period::ALL
            .iter()
            .fold(SegmentedControl::new("usage-period"), |control, period| {
                control.segment(period.label())
            })
            .selected(
                Period::ALL
                    .iter()
                    .position(|period| *period == self.period)
                    .unwrap_or(2),
            )
            .on_select({
                let this = this.clone();
                move |index, _, cx| {
                    this.update(cx, |this, cx| this.set_period(Period::ALL[index], cx))
                        .ok();
                }
            });
        let metrics = Metric::ALL
            .iter()
            .fold(SegmentedControl::new("usage-metric"), |control, metric| {
                control.segment(metric.label())
            })
            .selected(
                Metric::ALL
                    .iter()
                    .position(|metric| *metric == self.metric)
                    .unwrap_or(0),
            )
            .on_select(move |index, _, cx| {
                this.update(cx, |this, cx| this.set_metric(Metric::ALL[index], cx))
                    .ok();
            });
        h_flex()
            .w_full()
            .flex_wrap()
            .gap_2()
            .child(
                div()
                    .debug_selector(|| "usage-metric".into())
                    .child(metrics),
            )
            .when(self.metric != Metric::Limits, |this| {
                this.child(
                    div()
                        .debug_selector(|| "usage-period".into())
                        .child(periods),
                )
            })
            .into_any_element()
    }

    fn render_headline(&self, totals: &Totals, cx: &mut Context<Self>) -> AnyElement {
        let metric = self.metric;
        let total = totals.value(metric);
        let palette = palette(cx);
        let mut detail = plural(totals.sessions, "session", "sessions");
        if metric == Metric::Cost {
            detail.push_str(" · API estimate");
            let unpriced = totals.unpriced_share();
            if unpriced > 0. {
                detail.push_str(&format!(
                    ", without the {} of records T3 Code couldn't price",
                    format_percent(unpriced)
                ));
            }
        }
        let rows = totals
            .providers
            .iter()
            .enumerate()
            .map(|(index, provider)| {
                let value = match metric {
                    Metric::Tokens => provider.tokens as f64,
                    _ => provider.cost_usd,
                };
                let share = if total > 0. { value / total } else { 0. };
                let aside = match metric {
                    Metric::Tokens => format!(
                        "{} of tokens · {}",
                        format_percent(share),
                        format_usd(provider.cost_usd)
                    ),
                    _ => format!(
                        "{} of cost · {} tokens",
                        format_percent(share),
                        format_tokens(provider.tokens)
                    ),
                };
                h_flex()
                    .w_full()
                    .gap_2()
                    .py(scaled(px(7.)))
                    .when(index > 0, |this| {
                        this.border_t(tokens::BORDER_WIDTH)
                            .border_color(palette.border.hsla())
                    })
                    .child(dot(provider_color(&provider.provider, cx)))
                    .child(
                        v_flex()
                            .flex_1()
                            .min_w_0()
                            .child(Label::new(provider_label(&provider.provider)).truncate())
                            .child(muted(plural(provider.sessions, "session", "sessions"))),
                    )
                    .child(
                        v_flex()
                            .flex_none()
                            .items_end()
                            .child(
                                Label::new(format_value(value, metric)).weight(FontWeight::MEDIUM),
                            )
                            .child(muted(aside)),
                    )
                    .into_any_element()
            })
            .collect::<Vec<_>>();
        Card::new()
            .px(scaled(px(14.)))
            .py(scaled(px(12.)))
            .gap_2()
            .child(
                v_flex()
                    .debug_selector(|| "usage-total".into())
                    .child(
                        div()
                            .text_size(tokens::text_size(px(26.)))
                            .font_weight(FontWeight::SEMIBOLD)
                            .text_color(palette.text.hsla())
                            .child(format_value(total, metric)),
                    )
                    .child(muted(detail)),
            )
            .when(!rows.is_empty(), |this| {
                this.child(v_flex().w_full().children(rows))
            })
            .when(!totals.slots.is_empty(), |this| {
                this.child(self.render_chart(totals, cx))
            })
            .into_any_element()
    }

    /// A bar a day (or an hour), stacked by agent in T3's order, with the values on hover.
    fn render_chart(&self, totals: &Totals, cx: &mut Context<Self>) -> AnyElement {
        let metric = self.metric;
        let palette = palette(cx);
        let peak = totals
            .slots
            .iter()
            .map(|slot| slot.value(metric))
            .fold(0., f64::max);
        let hourly = self.period.hourly();
        let title = match (hourly, metric) {
            (true, Metric::Tokens) => "Hourly processed tokens",
            (false, Metric::Tokens) => "Daily processed tokens",
            (true, _) => "Hourly cost",
            (false, _) => "Daily cost",
        };
        let gap = if totals.slots.len() > 40 { 1. } else { 2. };
        let bars = totals.slots.iter().enumerate().map(|(index, slot)| {
            let value = slot.value(metric);
            let fraction = if peak > 0. { (value / peak) as f32 } else { 0. };
            let mut tooltip = format!(
                "{} · {} · {} tokens",
                slot.label,
                format_usd(slot.cost_usd),
                format_tokens(slot.tokens)
            );
            for (provider, cost, tokens) in &slot.by_provider {
                tooltip.push_str(&format!(
                    "\n{}: {}",
                    provider_label(provider),
                    match metric {
                        Metric::Tokens => format_tokens(*tokens),
                        _ => format_usd(*cost),
                    }
                ));
            }
            let segments = slot
                .by_provider
                .iter()
                .rev()
                .map(|(provider, cost, tokens)| {
                    let part = match metric {
                        Metric::Tokens => *tokens as f64,
                        _ => *cost,
                    };
                    let share = if value > 0. {
                        (part / value) as f32
                    } else {
                        0.
                    };
                    div()
                        .w_full()
                        .h(relative(share))
                        .bg(provider_color(provider, cx))
                });
            div()
                .id(("usage-bar", index))
                .flex_1()
                .min_w_0()
                .h_full()
                .flex()
                .flex_col()
                .justify_end()
                .rounded_sm()
                .hover(|style| style.bg(palette.surface_2.hsla()))
                .tooltip(Tooltip::text(tooltip))
                .when(value > 0., |this| {
                    this.child(
                        v_flex()
                            .w_full()
                            .h(relative(fraction.max(0.02)))
                            .rounded_t(scaled(px(3.)))
                            .overflow_hidden()
                            .gap(px(1.))
                            .children(segments),
                    )
                })
        });
        let first = totals.slots.first().map(|slot| slot.label.clone());
        let middle = totals
            .slots
            .get(totals.slots.len() / 2)
            .map(|slot| slot.label.clone());
        let last = totals.slots.last().map(|slot| slot.label.clone());
        v_flex()
            .debug_selector(|| "usage-chart".into())
            .w_full()
            .gap_1()
            .pt_2()
            .border_t(tokens::BORDER_WIDTH)
            .border_color(palette.border.hsla())
            .child(
                h_flex()
                    .w_full()
                    .child(muted(title))
                    .child(div().flex_1())
                    .child(
                        muted(format!("Peak {}", format_value(peak, metric)))
                            .size(LabelSize::XSmall),
                    ),
            )
            .child(
                h_flex()
                    .w_full()
                    .h(scaled(px(CHART_HEIGHT)))
                    .items_end()
                    .gap(px(gap))
                    .border_b(tokens::BORDER_WIDTH)
                    .border_color(palette.border.hsla())
                    .children(bars),
            )
            .child(
                h_flex()
                    .w_full()
                    .children(first.map(|label| muted(label).size(LabelSize::XSmall)))
                    .child(div().flex_1())
                    .children(middle.map(|label| muted(label).size(LabelSize::XSmall)))
                    .child(div().flex_1())
                    .children(last.map(|label| muted(label).size(LabelSize::XSmall))),
            )
            .into_any_element()
    }

    fn render_totals(&self, totals: &Totals, cx: &mut Context<Self>) -> AnyElement {
        let palette = palette(cx);
        let tokens_ = &totals.tokens;
        let rows = [
            ("Processed tokens", format_tokens(tokens_.total())),
            ("Cached input", format_tokens(tokens_.cached_input)),
            ("Uncached input", format_tokens(tokens_.uncached_input)),
            ("Cache writes", format_tokens(tokens_.cache_creation)),
            ("Output", format_tokens(tokens_.output)),
            ("Cache savings", format_usd(totals.cache_savings_usd)),
        ];
        let grid = h_flex()
            .w_full()
            .flex_wrap()
            .gap_y_2()
            .children(rows.into_iter().map(|(label, value)| {
                v_flex()
                    .w(relative(1. / 3.))
                    .min_w(scaled(px(140.)))
                    .flex_grow(1.)
                    .child(muted(label))
                    .child(Label::new(value).weight(FontWeight::MEDIUM))
            }));
        // The tokens by kind (`tokenTypeSegments`), in T3's ink steps.
        let total = tokens_.total();
        let ink = |percent: f32| palette.text.hsla().opacity(percent);
        let kinds = [
            ("Input", tokens_.uncached_input, ink(0.6)),
            ("Cache read", tokens_.cached_input, ink(0.3)),
            ("Cache write", tokens_.cache_creation, ink(0.72)),
            ("Output", tokens_.output, ink(1.)),
        ];
        let share_bar = (total > 0).then(|| {
            v_flex()
                .debug_selector(|| "usage-token-kinds".into())
                .w_full()
                .gap_1()
                .pt_2()
                .border_t(tokens::BORDER_WIDTH)
                .border_color(palette.border.hsla())
                .child(muted("Tokens by kind"))
                .child(
                    h_flex().w_full().h(scaled(px(8.))).gap(px(2.)).children(
                        kinds
                            .iter()
                            .filter(|(_, count, _)| *count > 0)
                            .map(|(_, count, color)| {
                                div()
                                    .h_full()
                                    .flex_basis(relative(*count as f32 / total as f32))
                                    .flex_grow(1.)
                                    .min_w(px(2.))
                                    .rounded_sm()
                                    .bg(*color)
                            }),
                    ),
                )
                .child(
                    h_flex()
                        .flex_wrap()
                        .gap_x_3()
                        .gap_y_1()
                        .children(kinds.iter().map(|(label, count, color)| {
                            h_flex().gap_1().child(dot(*color)).child(muted(format!(
                                "{label} {}",
                                format_percent(*count as f64 / total as f64)
                            )))
                        })),
                )
        });
        section(
            "Totals",
            None,
            Card::new()
                .px(scaled(px(14.)))
                .py(scaled(px(10.)))
                .gap_2()
                .child(grid)
                .children(share_bar)
                .into_any_element(),
        )
    }

    fn render_breakdown(&self, totals: &Totals, cx: &mut Context<Self>) -> AnyElement {
        let metric = self.metric;
        let palette = palette(cx);
        let this = cx.entity().downgrade();
        let toggle = SegmentedControl::new("usage-breakdown")
            .segment("Model")
            .segment(if self.period.hourly() { "Hour" } else { "Day" })
            .selected(match self.breakdown {
                Breakdown::Model => 0,
                Breakdown::Time => 1,
            })
            .on_select(move |index, _, cx| {
                this.update(cx, |this, cx| {
                    this.breakdown = if index == 0 {
                        Breakdown::Model
                    } else {
                        Breakdown::Time
                    };
                    cx.notify();
                })
                .ok();
            });
        let total = totals.value(metric);
        let header = |cells: Vec<&'static str>| {
            h_flex()
                .w_full()
                .gap_3()
                .pb_1()
                .border_b(tokens::BORDER_WIDTH)
                .border_color(palette.border.hsla())
                .children(cells.into_iter().enumerate().map(|(index, cell)| {
                    let label = muted(cell).size(LabelSize::XSmall);
                    if index == 0 {
                        div().flex_1().min_w_0().child(label)
                    } else {
                        div()
                            .w(scaled(px(84.)))
                            .flex_none()
                            .flex()
                            .justify_end()
                            .child(label)
                    }
                }))
        };
        let cell = |text: String, color: Color| {
            div()
                .w(scaled(px(84.)))
                .flex_none()
                .flex()
                .justify_end()
                .child(Label::new(text).size(LabelSize::Small).color(color))
        };
        let row = |index: usize| {
            h_flex()
                .w_full()
                .gap_3()
                .py(scaled(px(5.)))
                .when(index > 0, |this| {
                    this.border_t(tokens::BORDER_WIDTH)
                        .border_color(palette.border.hsla())
                })
        };
        let body = match self.breakdown {
            Breakdown::Model if totals.models.is_empty() => {
                muted("No models in this window.").into_any_element()
            }
            Breakdown::Model => {
                let peak = totals
                    .models
                    .iter()
                    .map(|model| match metric {
                        Metric::Tokens => model.tokens as f64,
                        _ => model.cost_usd,
                    })
                    .fold(0., f64::max);
                let mut rows =
                    vec![header(vec!["Model", "Cost", "Share", "Tokens"]).into_any_element()];
                rows.extend(totals.models.iter().take(BREAKDOWN_ROWS).enumerate().map(
                    |(index, model)| {
                        let value = match metric {
                            Metric::Tokens => model.tokens as f64,
                            _ => model.cost_usd,
                        };
                        let share = if metric == Metric::Cost && model.cost_unknown() {
                            String::new()
                        } else if total > 0. {
                            format_percent(value / total)
                        } else {
                            String::new()
                        };
                        let fraction = if peak > 0. { (value / peak) as f32 } else { 0. };
                        row(index)
                            .child(
                                v_flex()
                                    .flex_1()
                                    .min_w_0()
                                    .gap_1()
                                    .child(
                                        h_flex()
                                            .gap_1p5()
                                            .child(dot(provider_color(&model.provider, cx)))
                                            .child(
                                                Label::new(model.model.clone())
                                                    .size(LabelSize::Small)
                                                    .truncate(),
                                            ),
                                    )
                                    .child(
                                        div()
                                            .w(relative(0.6))
                                            .max_w(scaled(px(200.)))
                                            .h(px(2.))
                                            .child(
                                                div()
                                                    .h_full()
                                                    .w(relative(fraction.max(0.02)))
                                                    .rounded_full()
                                                    .bg(provider_color(&model.provider, cx)),
                                            ),
                                    ),
                            )
                            .child(if model.cost_unknown() {
                                cell("Unpriced".into(), Color::Muted)
                            } else {
                                cell(format_usd(model.cost_usd), Color::Default)
                            })
                            .child(cell(share, Color::Muted))
                            .child(cell(format_tokens(model.tokens), Color::Default))
                            .into_any_element()
                    },
                ));
                let rest = totals
                    .models
                    .iter()
                    .skip(BREAKDOWN_ROWS)
                    .collect::<Vec<_>>();
                if !rest.is_empty() {
                    let cost = rest.iter().map(|model| model.cost_usd).sum::<f64>();
                    let tokens = rest.iter().map(|model| model.tokens).sum::<i64>();
                    rows.push(
                        row(1)
                            .child(
                                div()
                                    .flex_1()
                                    .min_w_0()
                                    .child(muted(format!("{} more models", rest.len()))),
                            )
                            .child(cell(format_usd(cost), Color::Muted))
                            .child(cell(String::new(), Color::Muted))
                            .child(cell(format_tokens(tokens), Color::Muted))
                            .into_any_element(),
                    );
                }
                v_flex().w_full().children(rows).into_any_element()
            }
            Breakdown::Time => {
                let active = totals
                    .slots
                    .iter()
                    .rev()
                    .filter(|slot| slot.tokens > 0 || slot.cost_usd > 0.)
                    .collect::<Vec<_>>();
                if active.is_empty() {
                    muted("No activity in this window.").into_any_element()
                } else {
                    let mut rows = vec![
                        header(vec![
                            if self.period.hourly() { "Hour" } else { "Day" },
                            "Cost",
                            "Share",
                            "Tokens",
                        ])
                        .into_any_element(),
                    ];
                    rows.extend(active.into_iter().enumerate().map(|(index, slot)| {
                        let value = slot.value(metric);
                        row(index)
                            .child(
                                h_flex()
                                    .flex_1()
                                    .min_w_0()
                                    .gap_1p5()
                                    .child(Label::new(slot.label.clone()).size(LabelSize::Small))
                                    .children(
                                        slot.by_provider.iter().map(|(provider, _, _)| {
                                            dot(provider_color(provider, cx))
                                        }),
                                    ),
                            )
                            .child(cell(format_usd(slot.cost_usd), Color::Default))
                            .child(cell(
                                if total > 0. {
                                    format_percent(value / total)
                                } else {
                                    String::new()
                                },
                                Color::Muted,
                            ))
                            .child(cell(format_tokens(slot.tokens), Color::Default))
                            .into_any_element()
                    }));
                    v_flex().w_full().children(rows).into_any_element()
                }
            }
        };
        section(
            "Breakdown",
            Some(
                div()
                    .debug_selector(|| "usage-breakdown".into())
                    .child(toggle)
                    .into_any_element(),
            ),
            Card::new()
                .px(scaled(px(14.)))
                .py(scaled(px(8.)))
                .child(body)
                .into_any_element(),
        )
    }

    fn render_limits(&self, cx: &mut Context<Self>) -> AnyElement {
        let palette = palette(cx);
        let now = time::OffsetDateTime::now_utc();
        let limits: Vec<UsageLimits> = self
            .configs
            .read(cx)
            .providers()
            .map(|providers| {
                providers
                    .iter()
                    .filter_map(|provider| usage_limits(provider, now))
                    .collect()
            })
            .unwrap_or_default();
        if limits.is_empty() {
            return Card::new()
                .child(muted(
                    "No agent reports usage limits. Signed-in Claude Code and Codex accounts with \
                     a subscription show their 5-hour and weekly windows here.",
                ))
                .into_any_element();
        }
        v_flex()
            .debug_selector(|| "usage-limits".into())
            .w_full()
            .gap_3()
            .children(limits.into_iter().map(|limits| {
                let windows = limits.windows.iter().map(|window| {
                    let left = window.remaining_percent as f32 / 100.;
                    let color = if window.remaining_percent <= 10 {
                        cx.theme().status().error
                    } else if window.remaining_percent <= 25 {
                        palette.warning.hsla()
                    } else {
                        palette.accent.hsla()
                    };
                    v_flex()
                        .w_full()
                        .gap_1()
                        .child(
                            h_flex()
                                .w_full()
                                .child(Label::new(window.label.clone()).size(LabelSize::Small))
                                .child(div().flex_1())
                                .child(muted(match &window.resets_in {
                                    Some(resets) => {
                                        format!("{}% left · {resets}", window.remaining_percent)
                                    }
                                    None => format!("{}% left", window.remaining_percent),
                                })),
                        )
                        .child(
                            div()
                                .w_full()
                                .h(scaled(px(4.)))
                                .rounded_full()
                                .bg(palette.surface_2.hsla())
                                .child(
                                    div()
                                        .h_full()
                                        .w(relative(left.clamp(0., 1.)))
                                        .rounded_full()
                                        .bg(color),
                                ),
                        )
                });
                Card::new()
                    .px(scaled(px(14.)))
                    .py(scaled(px(10.)))
                    .gap_2()
                    .child(
                        h_flex()
                            .w_full()
                            .child(Label::new(limits.account.clone()).weight(FontWeight::MEDIUM))
                            .child(div().flex_1())
                            .children(limits.manage.clone().map(|(label, _)| muted(label))),
                    )
                    .children(limits.notice.clone().map(muted))
                    .children(windows)
            }))
            .into_any_element()
    }
}

impl EventEmitter<ItemEvent> for UsageView {}

impl Focusable for UsageView {
    fn focus_handle(&self, _: &App) -> FocusHandle {
        self.focus_handle.clone()
    }
}

impl Item for UsageView {
    type Event = ItemEvent;

    fn tab_content_text(&self, _detail: usize, _cx: &App) -> SharedString {
        "Usage".into()
    }

    fn tab_icon(&self, _window: &Window, _cx: &App) -> Option<Icon> {
        Some(Icon::new(IconName::SignalHigh))
    }

    fn show_toolbar(&self) -> bool {
        false
    }

    fn to_item_events(event: &Self::Event, f: &mut dyn FnMut(ItemEvent)) {
        f(*event)
    }
}

impl Render for UsageView {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let palette = palette(cx);
        let environment = self
            .configs
            .read(cx)
            .environment()
            .map(|environment| environment.label.trim().to_string())
            .filter(|label| !label.is_empty());
        let refresh = {
            let this = cx.entity().downgrade();
            div().debug_selector(|| "usage-refresh".into()).child(
                TextButton::new(
                    "usage-refresh",
                    if self.loading {
                        "Reading…"
                    } else {
                        "Refresh"
                    },
                )
                .style(ButtonStyle::Default)
                .disabled(self.loading || self.blocked().is_some() || self.metric == Metric::Limits)
                .on_click(move |_, _, cx| {
                    this.update(cx, |this, cx| this.reload(true, cx)).ok();
                }),
            )
        };
        let header = h_flex()
            .w_full()
            .gap_3()
            .items_start()
            .child(
                v_flex()
                    .flex_1()
                    .min_w_0()
                    .gap_0p5()
                    .child(Headline::new("Usage").size(HeadlineSize::Large))
                    .child(muted(match &environment {
                        Some(environment) => format!(
                            "What the agents T3 Code runs on {environment} have used, read from \
                             their own session logs. Costs are API-price estimates, not bills."
                        ),
                        None => "What the agents T3 Code runs have used, read from their own \
                                 session logs. Costs are API-price estimates, not bills."
                            .to_string(),
                    })),
            )
            .child(div().flex_none().child(refresh));
        let body: Vec<AnyElement> = if self.metric == Metric::Limits {
            vec![self.render_limits(cx)]
        } else if let Some(reason) = self.blocked() {
            vec![
                div()
                    .debug_selector(|| "usage-blocked".into())
                    .child(Card::new().child(muted(reason)))
                    .into_any_element(),
            ]
        } else {
            match &self.summary {
                Loadable::Loading => vec![
                    Card::new()
                        .child(muted("Reading the agents' usage…"))
                        .into_any_element(),
                ],
                Loadable::Unsupported => vec![
                    div()
                        .debug_selector(|| "usage-unsupported".into())
                        .child(Card::new().child(muted(
                            "This T3 Code doesn't report usage. Update it to see tokens and \
                             cost here; the Limits tab still works.",
                        )))
                        .into_any_element(),
                ],
                Loadable::Failed(error) => vec![
                    div()
                        .debug_selector(|| "usage-error".into())
                        .child(
                            Card::new().child(
                                Label::new(error.clone())
                                    .size(LabelSize::Small)
                                    .color(Color::Error),
                            ),
                        )
                        .into_any_element(),
                ],
                Loadable::Loaded(totals) => {
                    let totals = totals.clone();
                    let mut body = Vec::new();
                    if !totals.notices.is_empty() || totals.refreshing {
                        body.push(
                            v_flex()
                                .gap_0p5()
                                .when(totals.refreshing, |this| {
                                    this.child(muted(
                                        "T3 Code is still reading some session logs, so the \
                                         totals may grow.",
                                    ))
                                })
                                .children(totals.notices.iter().map(|notice| {
                                    Label::new(notice.clone())
                                        .size(LabelSize::Small)
                                        .color(Color::Warning)
                                }))
                                .into_any_element(),
                        );
                    }
                    if totals.tokens.total() == 0 && totals.cost_usd == 0. {
                        body.push(self.render_headline(&totals, cx));
                        body.push(
                            div()
                                .debug_selector(|| "usage-empty".into())
                                .child(Card::new().child(muted("No activity in this window.")))
                                .into_any_element(),
                        );
                    } else {
                        body.push(self.render_headline(&totals, cx));
                        body.push(self.render_totals(&totals, cx));
                        body.push(self.render_breakdown(&totals, cx));
                    }
                    if let Some(read_at) = self.read_at.clone() {
                        body.push(
                            muted(format!("Read at {}", read_time(&read_at)))
                                .size(LabelSize::XSmall)
                                .into_any_element(),
                        );
                    }
                    body
                }
            }
        };
        v_flex()
            .key_context("MustrUsage")
            .track_focus(&self.focus_handle)
            .size_full()
            .bg(palette.background.hsla())
            .child(
                v_flex()
                    .id("usage-scroll")
                    .size_full()
                    .overflow_y_scroll()
                    .child(
                        v_flex()
                            .w_full()
                            .max_w(scaled(px(CONTENT_MAX_WIDTH)))
                            .mx_auto()
                            .px_6()
                            .py_6()
                            .gap_4()
                            .child(header)
                            .child(self.render_controls(cx))
                            .when(
                                self.loading && matches!(self.summary, Loadable::Loaded(_)),
                                |this| this.child(muted("Reading…").size(LabelSize::XSmall)),
                            )
                            .children(body),
                    ),
            )
    }
}

/// When T3 read the logs, in the user's time.
fn read_time(read_at: &str) -> String {
    match DateTime::parse_from_rfc3339(read_at) {
        Ok(at) => {
            let local = at.with_timezone(&chrono::Local);
            let today = chrono::Local::now().date_naive();
            if local.date_naive() == today {
                local.format("%-I:%M %p").to_string()
            } else if local.year() == today.year() {
                local.format("%b %-d, %-I:%M %p").to_string()
            } else {
                local.format("%b %-d %Y, %-I:%M %p").to_string()
            }
        }
        Err(_) => read_at.to_string(),
    }
}

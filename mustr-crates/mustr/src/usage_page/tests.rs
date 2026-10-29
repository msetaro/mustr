//! The usage dashboard against a scripted T3: the exact `server.getUsageSummary` inputs for each
//! window, how T3's buckets add up, T3's number formats, and the states for servers without the
//! call and pairings without `diagnostics:read`.

use super::*;
use crate::fake_t3::{Call, FakeT3};
use gpui::{TestAppContext, VisualTestContext};
use pretty_assertions::assert_eq;
use serde_json::{Value, json};
use settings::SettingsStore;

fn clock() -> Clock {
    Clock::utc("2026-10-28T14:37:21.512Z")
}

#[test]
fn windows_are_t3s() {
    let input = |period, await_refresh| {
        serde_json::to_value(summary_input(period, &clock(), await_refresh)).unwrap()
    };
    assert_eq!(
        input(Period::Month, false),
        json!({
            "resolution": "day", "sinceDay": "2026-09-29", "untilDay": "2026-10-28",
            "timeZone": "UTC"
        })
    );
    assert_eq!(
        input(Period::Week, true),
        json!({
            "awaitRefresh": true, "resolution": "day", "sinceDay": "2026-10-22",
            "untilDay": "2026-10-28", "timeZone": "UTC"
        })
    );
    assert_eq!(
        input(Period::Quarter, false)["sinceDay"],
        json!("2026-07-31")
    );
    // The past 24 hours, to the minute, by the hour.
    assert_eq!(
        input(Period::Day, false),
        json!({
            "resolution": "hour", "sinceDay": "2026-10-27", "untilDay": "2026-10-28",
            "sinceTime": "2026-10-27T14:37:00.000Z", "untilTime": "2026-10-28T14:37:00.000Z",
            "timeZone": "UTC"
        })
    );
    // Days are the zone's: just after midnight in Tokyo is still the 27th in UTC.
    let tokyo = Clock {
        now: DateTime::parse_from_rfc3339("2026-10-27T15:30:00Z")
            .unwrap()
            .with_timezone(&Utc),
        time_zone: "Asia/Tokyo".into(),
        local_day: |instant| {
            instant
                .with_timezone(&chrono::FixedOffset::east_opt(9 * 3600).unwrap())
                .date_naive()
        },
    };
    let input = serde_json::to_value(summary_input(Period::Week, &tokyo, false)).unwrap();
    assert_eq!(input["untilDay"], json!("2026-10-28"));
    assert_eq!(input["sinceDay"], json!("2026-10-22"));
    assert_eq!(input["timeZone"], json!("Asia/Tokyo"));
}

fn bucket(day: &str, provider: &str, model: &str, cost: f64, tokens: [i64; 4]) -> Value {
    json!({
        "day": day, "provider": provider, "model": model,
        "totals": {
            "uncachedInputTokens": tokens[0], "cachedInputTokens": tokens[1],
            "cacheCreationTokens": tokens[2], "outputTokens": tokens[3], "reasoningTokens": 7
        },
        "costUsd": cost, "cacheSavingsUsd": 0.5, "costSource": "modelPriced",
        "records": 4, "unpricedRecords": 0, "sessions": 2
    })
}

fn source(provider: &str, sessions: i64, status: &str, message: Value) -> Value {
    json!({
        "fingerprint": {
            "hostId": "host", "provider": provider, "resolvedHomePath": "/home/u", "volumeId": "v"
        },
        "status": status, "scannedFiles": 3, "skippedFiles": 0, "malformedRecords": 0,
        "distinctSessions": sessions, "message": message
    })
}

fn summary(buckets: Vec<Value>, sources: Vec<Value>) -> Value {
    json!({
        "contractVersion": 6, "readAt": "2026-10-28T14:37:22.000Z", "timeZone": "UTC",
        "sinceDay": "2026-10-22", "untilDay": "2026-10-28",
        "buckets": buckets, "sources": sources,
        "pricing": {
            "status": "fresh", "source": "https://litellm", "fetchedAt": null, "knownModels": 900
        },
        "scanDurationMs": 12
    })
}

fn week_summary() -> Value {
    let mut unpriced = bucket("2026-10-27", "codex", "gpt-6-preview", 0., [100, 0, 0, 50]);
    unpriced["costSource"] = json!("unpriced");
    unpriced["unpricedRecords"] = json!(4);
    summary(
        vec![
            bucket(
                "2026-10-22",
                "claude",
                "claude-opus-4-5",
                12.5,
                [1_000, 200_000, 3_000, 9_000],
            ),
            bucket(
                "2026-10-28",
                "claude",
                "claude-opus-4-5",
                7.25,
                [500, 100_000, 0, 4_500],
            ),
            bucket(
                "2026-10-28",
                "codex",
                "gpt-5.3-codex",
                3.0,
                [20_000, 80_000, 0, 6_000],
            ),
            unpriced,
        ],
        vec![
            source("claude", 9, "ok", Value::Null),
            source(
                "codex",
                4,
                "partial",
                json!("2 session logs were cut short."),
            ),
        ],
    )
}

#[test]
fn buckets_add_up_by_agent_model_and_day() {
    let input = summary_input(Period::Week, &clock(), false);
    let summary: UsageSummary = serde_json::from_value(week_summary()).unwrap();
    let totals = totals(&summary, &input);

    assert_eq!(format_usd(totals.cost_usd), "$22.75");
    assert_eq!(totals.sessions, 13);
    assert_eq!(
        totals.tokens,
        TokenTotals {
            uncached_input: 21_600,
            cached_input: 380_000,
            cache_creation: 3_000,
            output: 19_550,
        }
    );
    assert_eq!(totals.tokens.total(), 424_150);
    assert_eq!(format_percent(totals.unpriced_share()), "25.0%");
    assert_eq!(
        totals
            .providers
            .iter()
            .map(|provider| (
                provider_label(&provider.provider),
                format_usd(provider.cost_usd),
                provider.tokens,
                provider.sessions
            ))
            .collect::<Vec<_>>(),
        [
            ("Claude Code".to_string(), "$19.75".to_string(), 318_000, 9),
            ("Codex".to_string(), "$3.00".to_string(), 106_150, 4),
        ]
    );
    assert_eq!(
        totals
            .models
            .iter()
            .map(|model| (model.model.as_str(), model.cost_unknown()))
            .collect::<Vec<_>>(),
        [
            ("claude-opus-4-5", false),
            ("gpt-5.3-codex", false),
            ("gpt-6-preview", true)
        ]
    );
    // Every day of the window, with the agents stacked in T3's order (Codex, then Claude).
    assert_eq!(
        totals
            .slots
            .iter()
            .map(|slot| (slot.label.as_str(), slot.tokens))
            .collect::<Vec<_>>(),
        [
            ("Oct 22", 213_000),
            ("Oct 23", 0),
            ("Oct 24", 0),
            ("Oct 25", 0),
            ("Oct 26", 0),
            ("Oct 27", 150),
            ("Oct 28", 211_000),
        ]
    );
    assert_eq!(
        totals.slots[6]
            .by_provider
            .iter()
            .map(|(provider, cost, _)| (provider.as_str().to_string(), *cost))
            .collect::<Vec<_>>(),
        [("codex".to_string(), 3.0), ("claude".to_string(), 7.25)]
    );
    assert_eq!(totals.notices, ["Codex: 2 session logs were cut short."]);
    assert!(!totals.refreshing);
}

#[test]
fn the_past_day_is_by_the_hour() {
    let input = summary_input(Period::Day, &clock(), false);
    let mut late = bucket("2026-10-28", "claude", "claude-opus-4-5", 1.0, [1, 2, 3, 4]);
    late["hourStart"] = json!("2026-10-28T14:00:00Z");
    let mut early = bucket("2026-10-27", "claude", "claude-opus-4-5", 2.0, [1, 1, 1, 1]);
    early["hourStart"] = json!("2026-10-27T14:00:00.000Z");
    let summary: UsageSummary = serde_json::from_value(summary(vec![late, early], vec![])).unwrap();
    let totals = totals(&summary, &input);
    // 14:37 yesterday to 14:37 today touches 25 hours.
    assert_eq!(totals.slots.len(), 25);
    assert_eq!(totals.slots[0].key, "2026-10-27T14:00:00.000Z");
    assert_eq!(totals.slots[0].tokens, 4);
    assert_eq!(totals.slots[24].key, "2026-10-28T14:00:00.000Z");
    assert_eq!(totals.slots[24].tokens, 10);
}

#[test]
fn numbers_are_written_as_t3_writes_them() {
    assert_eq!(format_usd(0.), "$0.00");
    assert_eq!(format_usd(1234.567), "$1,234.57");
    assert_eq!(format_usd(1_000_000.), "$1,000,000.00");
    assert_eq!(format_usd(-3.5), "-$3.50");
    assert_eq!(format_tokens(950), "950");
    assert_eq!(format_tokens(1_000), "1K");
    assert_eq!(format_tokens(1_500_000), "1.50M");
    assert_eq!(format_tokens(12_340), "12.3K");
    assert_eq!(format_tokens(123_456_789), "123M");
    assert_eq!(format_tokens(2_000_000_000), "2B");
    assert_eq!(format_count(1_234_567), "1,234,567");
    assert_eq!(format_percent(0.125), "12.5%");
    assert_eq!(format_percent(0.0004), "<0.1%");
    assert_eq!(format_percent(0.), "0.0%");
}

fn open_page(
    backend: Arc<FakeT3>,
    cx: &mut TestAppContext,
) -> (Entity<UsageView>, &mut VisualTestContext) {
    cx.update(|cx| {
        let settings_store = SettingsStore::test(cx);
        cx.set_global(settings_store);
        theme_settings::init(theme::LoadThemes::JustBase, cx);
        thread_chat::set_backend(backend.clone(), cx);
    });
    let (view, cx) = cx.add_window_view(|_, cx| {
        UsageView::with_clock(Some(backend as Arc<dyn ThreadBackend>), clock, cx)
    });
    cx.simulate_resize(gpui::size(gpui::px(1200.), gpui::px(2400.)));
    cx.run_until_parked();
    (view, cx)
}

fn click(selector: &'static str, cx: &mut VisualTestContext) {
    let bounds = cx
        .debug_bounds(selector)
        .unwrap_or_else(|| panic!("{selector} isn't rendered"));
    cx.simulate_click(bounds.center(), gpui::Modifiers::none());
    cx.run_until_parked();
}

fn usage_call(payload: Value) -> Call {
    Call::new("server.getUsageSummary", payload, Access::DiagnosticsRead)
}

#[gpui::test]
async fn the_page_reads_t3s_usage_for_each_window(cx: &mut TestAppContext) {
    let backend = FakeT3::new();
    backend.answer("server.getUsageSummary", week_summary());
    let (view, cx) = open_page(backend.clone(), cx);
    assert_eq!(
        backend.calls(),
        [usage_call(json!({
            "resolution": "day", "sinceDay": "2026-09-29", "untilDay": "2026-10-28",
            "timeZone": "UTC"
        }))]
    );
    assert!(cx.debug_bounds("usage-total").is_some());
    assert!(cx.debug_bounds("usage-chart").is_some());
    assert!(cx.debug_bounds("usage-token-kinds").is_some());
    view.read_with(cx, |view, _| {
        assert_eq!(view.totals().unwrap().providers.len(), 2);
    });

    backend.clear_calls();
    click("segment-7 days", cx);
    click("segment-Past 24h", cx);
    click("segment-Tokens", cx);
    click("segment-Hour", cx);
    click("usage-refresh", cx);
    assert_eq!(
        backend.calls(),
        [
            usage_call(json!({
                "resolution": "day", "sinceDay": "2026-10-22", "untilDay": "2026-10-28",
                "timeZone": "UTC"
            })),
            usage_call(json!({
                "resolution": "hour", "sinceDay": "2026-10-27", "untilDay": "2026-10-28",
                "sinceTime": "2026-10-27T14:37:00.000Z", "untilTime": "2026-10-28T14:37:00.000Z",
                "timeZone": "UTC"
            })),
            // Refresh waits for T3 to scan.
            usage_call(json!({
                "awaitRefresh": true, "resolution": "hour", "sinceDay": "2026-10-27",
                "untilDay": "2026-10-28", "sinceTime": "2026-10-27T14:37:00.000Z",
                "untilTime": "2026-10-28T14:37:00.000Z", "timeZone": "UTC"
            })),
        ]
    );
    // Limits come from T3's provider status, without another call.
    backend.clear_calls();
    click("segment-Limits", cx);
    assert!(backend.calls().is_empty());
}

#[gpui::test]
async fn a_scan_in_progress_is_asked_again(cx: &mut TestAppContext) {
    let backend = FakeT3::new();
    let mut refreshing = week_summary();
    refreshing["sources"][0]["refreshing"] = json!(true);
    backend.answer_once("server.getUsageSummary", refreshing);
    backend.answer("server.getUsageSummary", week_summary());
    let (_view, _cx) = open_page(backend.clone(), cx);
    let calls = backend.calls_to("server.getUsageSummary");
    assert_eq!(calls.len(), 2);
    assert_eq!(calls[0].payload.get("awaitRefresh"), None);
    assert_eq!(calls[1].payload["awaitRefresh"], json!(true));
}

#[gpui::test]
async fn older_servers_and_narrow_pairings_say_why(cx: &mut TestAppContext) {
    let backend = FakeT3::new();
    backend.unsupported("server.getUsageSummary");
    let (_view, cx) = open_page(backend, cx);
    assert!(cx.debug_bounds("usage-unsupported").is_some());

    let backend = FakeT3::new();
    backend.block(
        Access::DiagnosticsRead,
        "mustr's pairing with T3 can't read T3's usage (it lacks diagnostics:read).",
    );
    let (_view, cx) = open_page(backend.clone(), cx);
    assert!(cx.debug_bounds("usage-blocked").is_some());
    assert!(backend.calls().is_empty());
}

#[gpui::test]
async fn an_empty_window_says_so(cx: &mut TestAppContext) {
    let backend = FakeT3::new();
    backend.answer("server.getUsageSummary", summary(vec![], vec![]));
    let (_view, cx) = open_page(backend, cx);
    assert!(cx.debug_bounds("usage-empty").is_some());
}

#[gpui::test]
async fn a_failed_read_shows_t3s_error(cx: &mut TestAppContext) {
    let backend = FakeT3::new();
    backend.fail("server.getUsageSummary", "The window is invalid");
    let (_view, cx) = open_page(backend, cx);
    assert!(cx.debug_bounds("usage-error").is_some());
    assert!(cx.debug_bounds("usage-total").is_none());
}

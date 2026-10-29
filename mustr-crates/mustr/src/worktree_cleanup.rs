//! Automatic worktree cleanup, on the worktrees page (`mustr-docs/PARITY.md` §2.10): T3's own
//! rules for removing agent worktrees, as its settings' Cleanup section has them
//! (`settings/StorageSettings.tsx`), and Delete now.
//!
//! - The rules are the server's (`storageCleanup.worktree*`), or a custom policy that replaces
//!   them (`worktreeCleanup`), as T3's `resolveWorktreeCleanup` reads them. T3 runs them itself,
//!   hourly and on events (`storageCleanup.ts`).
//! - Changing one writes `server.updateSettings` with `{storageCleanup: {<rule>: <value>}}`, and
//!   with the whole policy too when the server has a custom one, as T3's settings page does
//!   (`scopedSettings.ts`). It's raw JSON because turning a rule off sends `null`.
//! - Delete now runs every rule at once (`server.runStorageCleanup`), and the last run comes from
//!   `server.getStorageCleanupReport`.
//!
//! The writes need `settings:write`, so dry runs and read-only pairings refuse them, and the
//! controls say why.

use anyhow::{Context as _, Result};
use futures::StreamExt as _;
use gpui::{
    AnyElement, App, Context, FontWeight, IntoElement, ParentElement as _, Render, SharedString,
    Styled as _, Task, Window, div, px,
};
use mustr_ui::{Card, SegmentedControl, TextButton, palette, tokens, tokens::scaled};
use serde_json::{Map, Value, json};
use std::{rc::Rc, sync::Arc};
use t3_client::t3_api::{
    RpcMethod as _, rpc,
    tagged::ServerSettingsWorktreeCleanup,
    types::{
        ServerGetStorageCleanupReportPayload, ServerRunStorageCleanupPayload, ServerSettings,
        StorageCleanupReport, StorageCleanupReportBytesFreed, WorktreeKeepWhen,
    },
};
use thread_chat::{Access, ThreadBackend, ThreadBackendExt as _, is_unsupported_rpc};
use time::{OffsetDateTime, format_description::well_known::Rfc3339};
use ui::{
    Color, Icon, IconName, IconSize, Label, LabelCommon as _, LabelSize, Switch, ToggleState,
    Tooltip, prelude::*,
};

/// T3's rules for removing worktrees, as they apply now.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CleanupRules {
    pub keep_when: WorktreeKeepWhen,
    /// Removes worktrees whose threads were inactive this many days.
    pub after_days: Option<i64>,
    /// Removes worktrees this many days after their thread settled (0: at the next run).
    pub settled_after_days: Option<i64>,
    pub on_merge: bool,
    pub on_delete: bool,
    pub unchanged: bool,
}

impl CleanupRules {
    /// The server's rules (`resolveWorktreeCleanup` in T3's `projectSettings.ts`, for the whole
    /// server): a custom policy's, none under an `off` one, or else `storageCleanup`'s.
    pub fn of(settings: &ServerSettings) -> Self {
        let storage = settings.storage_cleanup.clone().unwrap_or_default();
        let keep_when = storage
            .worktree_keep_when
            .clone()
            .unwrap_or(WorktreeKeepWhen::UncommittedChanges);
        match &settings.worktree_cleanup {
            Some(ServerSettingsWorktreeCleanup::Custom(custom)) => Self {
                keep_when: custom.rules.worktree_keep_when.clone().unwrap_or(keep_when),
                after_days: custom.rules.worktree_after_days,
                settled_after_days: custom.rules.worktree_settled_after_days,
                on_merge: custom.rules.worktree_on_merge,
                on_delete: custom.rules.worktree_on_delete,
                unchanged: custom.rules.worktree_unchanged,
            },
            Some(ServerSettingsWorktreeCleanup::Off(_)) => Self {
                keep_when,
                after_days: None,
                settled_after_days: None,
                on_merge: false,
                on_delete: false,
                unchanged: false,
            },
            _ => Self {
                keep_when,
                after_days: storage.worktree_after_days,
                settled_after_days: storage.worktree_settled_after_days,
                on_merge: storage.worktree_on_merge.unwrap_or(false),
                on_delete: storage.worktree_on_delete.unwrap_or(false),
                unchanged: storage.worktree_unchanged.unwrap_or(false),
            },
        }
    }

    /// Whether any rule removes worktrees.
    pub fn any_on(&self) -> bool {
        self.on_delete
            || self.on_merge
            || self.unchanged
            || self.after_days.is_some()
            || self.settled_after_days.is_some()
    }

    /// The rules in a line, for the page's header.
    pub fn summary(&self) -> String {
        let mut rules = Vec::new();
        if self.on_delete {
            rules.push("of deleted threads".to_string());
        }
        if self.on_merge {
            rules.push("merged".to_string());
        }
        if let Some(days) = self.after_days {
            rules.push(format!("inactive {}", days_label(days)));
        }
        match self.settled_after_days {
            Some(0) => rules.push("settled".to_string()),
            Some(days) => rules.push(format!("settled {}", days_label(days))),
            None => {}
        }
        if self.unchanged {
            rules.push("unchanged".to_string());
        }
        if rules.is_empty() {
            return "Off: T3 removes no worktrees on its own".to_string();
        }
        format!(
            "T3 removes worktrees: {}. It keeps those with {}.",
            rules.join(", "),
            keep_when_summary(&self.keep_when)
        )
    }

    /// The rules as T3's `WorktreeCleanupRules`, for a custom policy.
    fn json(&self) -> Value {
        json!({
            "worktreeKeepWhen": keep_when_value(&self.keep_when),
            "worktreeAfterDays": self.after_days,
            "worktreeSettledAfterDays": self.settled_after_days,
            "worktreeOnMerge": self.on_merge,
            "worktreeOnDelete": self.on_delete,
            "worktreeUnchanged": self.unchanged,
        })
    }
}

fn days_label(days: i64) -> String {
    match days {
        1 => "for 1 day".to_string(),
        days => format!("for {days} days"),
    }
}

fn keep_when_value(keep_when: &WorktreeKeepWhen) -> &str {
    match keep_when {
        WorktreeKeepWhen::AnyLocalFiles => "any-local-files",
        WorktreeKeepWhen::UncommittedChanges => "uncommitted-changes",
        WorktreeKeepWhen::TrackedChanges => "tracked-changes",
        WorktreeKeepWhen::Unrecognized(value) => value,
    }
}

fn keep_when_summary(keep_when: &WorktreeKeepWhen) -> &'static str {
    match keep_when {
        WorktreeKeepWhen::AnyLocalFiles => "any local files",
        WorktreeKeepWhen::TrackedChanges => "edited tracked files",
        WorktreeKeepWhen::UncommittedChanges | WorktreeKeepWhen::Unrecognized(_) => {
            "uncommitted changes"
        }
    }
}

/// The `server.updateSettings` patch that sets one rule (`worktreeOnMerge`, …) to `value`, as
/// T3's settings page plans it: the server's `storageCleanup`, and, when the server has a custom
/// policy instead, that policy with the rule changed too.
pub fn rule_patch(settings: &ServerSettings, key: &'static str, value: Value) -> Value {
    let mut patch = Map::new();
    patch.insert("storageCleanup".into(), json!({ key: value.clone() }));
    if settings.worktree_cleanup.is_some() {
        let mut rules = CleanupRules::of(settings).json();
        rules[key] = value;
        patch.insert(
            "worktreeCleanup".into(),
            json!({ "mode": "custom", "rules": rules }),
        );
    }
    Value::Object(patch)
}

fn bytes(report: &StorageCleanupReport) -> f64 {
    match &report.bytes_freed {
        Some(StorageCleanupReportBytesFreed::Number(bytes)) => *bytes,
        _ => 0.,
    }
}

fn format_bytes(bytes: f64) -> String {
    const UNITS: [&str; 4] = ["KB", "MB", "GB", "TB"];
    if bytes < 1024. {
        return format!("{bytes:.0} B");
    }
    let mut value = bytes / 1024.;
    let mut unit = 0;
    while value >= 1024. && unit + 1 < UNITS.len() {
        value /= 1024.;
        unit += 1;
    }
    format!("{value:.1} {}", UNITS[unit])
}

fn ago(at: &str, now: OffsetDateTime) -> String {
    let Ok(at) = OffsetDateTime::parse(at, &Rfc3339) else {
        return "recently".to_string();
    };
    let minutes = (now - at).whole_minutes().max(0);
    match minutes {
        0 => "just now".to_string(),
        1..=59 => format!("{minutes} min ago"),
        60..=1439 => format!("{} h ago", minutes / 60),
        _ => format!("{} days ago", minutes / 1440),
    }
}

/// What the last run did, as T3's settings page says it: when, what it recovered, and what it
/// couldn't remove.
pub fn report_summary(report: &StorageCleanupReport, now: OffsetDateTime) -> String {
    let counts = &report.counts;
    let mut summary = format!(
        "Last run {}: {} removed, {} kept",
        ago(&report.finished_at, now),
        counts.removed.0,
        counts.kept.0
    );
    if counts.failed.0 > 0. {
        summary.push_str(&format!(", {} failed", counts.failed.0));
    }
    summary.push_str(&format!(". {} recovered.", format_bytes(bytes(report))));
    summary
}

// ---------------------------------------------------------------------------------------------
// The card

/// The cleanup rules and Delete now, on the worktrees page.
pub struct WorktreeCleanup {
    backend: Option<Arc<dyn ThreadBackend>>,
    settings: Option<ServerSettings>,
    /// Why the settings couldn't be read, or a write failed.
    error: Option<SharedString>,
    report: Option<StorageCleanupReport>,
    expanded: bool,
    saving: bool,
    running: bool,
    /// Tells the page to look at its worktrees again after a run.
    on_ran: Option<Rc<dyn Fn(&mut Window, &mut App)>>,
    _load: Task<()>,
    _report: Task<()>,
    _write: Option<Task<()>>,
}

impl WorktreeCleanup {
    pub fn new(cx: &mut Context<Self>) -> Self {
        let backend = thread_chat::backend(cx);
        let mut this = Self {
            backend,
            settings: None,
            error: None,
            report: None,
            expanded: false,
            saving: false,
            running: false,
            on_ran: None,
            _load: Task::ready(()),
            _report: Task::ready(()),
            _write: None,
        };
        this.load(cx);
        this.follow_report(cx);
        this
    }

    pub fn on_ran(mut self, on_ran: impl Fn(&mut Window, &mut App) + 'static) -> Self {
        self.on_ran = Some(Rc::new(on_ran));
        self
    }

    pub fn rules(&self) -> Option<CleanupRules> {
        self.settings.as_ref().map(CleanupRules::of)
    }

    #[cfg(test)]
    pub fn report(&self) -> Option<&StorageCleanupReport> {
        self.report.as_ref()
    }

    #[cfg(test)]
    pub fn error(&self) -> Option<&SharedString> {
        self.error.as_ref()
    }

    pub fn set_expanded(&mut self, expanded: bool, cx: &mut Context<Self>) {
        self.expanded = expanded;
        cx.notify();
    }

    fn load(&mut self, cx: &mut Context<Self>) {
        let Some(backend) = self.backend.clone() else {
            return;
        };
        self._load = cx.spawn(async move |this, cx| {
            let settings = thread_chat::thread_settings::load_server_settings(&backend).await;
            this.update(cx, |this, cx| {
                match settings {
                    Ok(settings) => this.settings = Some(settings),
                    Err(error) => {
                        log::warn!("couldn't read T3's cleanup settings: {error:#}");
                        this.error =
                            Some(format!("Couldn't read T3's cleanup rules: {error:#}").into());
                    }
                }
                cx.notify();
            })
            .ok();
        });
    }

    /// Follows T3's last cleanup report (`server.getStorageCleanupReport`).
    fn follow_report(&mut self, cx: &mut Context<Self>) {
        let Some(backend) = self.backend.clone() else {
            return;
        };
        let stream = backend.stream::<rpc::ServerGetStorageCleanupReport>(
            ServerGetStorageCleanupReportPayload(Map::new()),
            Access::Read,
        );
        self._report = cx.spawn(async move |this, cx| {
            let mut stream = match stream {
                Ok(stream) => stream,
                Err(error) => {
                    log::info!("no cleanup reports from T3: {error:#}");
                    return;
                }
            };
            while let Some(report) = stream.next().await {
                match report {
                    Ok(Some(report)) => {
                        let updated = this.update(cx, |this, cx| {
                            this.report = Some(report.0);
                            cx.notify();
                        });
                        if updated.is_err() {
                            return;
                        }
                    }
                    Ok(None) => {}
                    Err(error) if is_unsupported_rpc(&error) => return,
                    Err(error) => log::warn!("server.getStorageCleanupReport: {error:#}"),
                }
            }
        });
    }

    fn blocked(&self) -> Option<String> {
        match &self.backend {
            Some(backend) => backend.blocked_reason(Access::SettingsWrite),
            None => Some("mustr isn't connected to T3".to_string()),
        }
    }

    /// Sets one rule on T3's server.
    pub fn set_rule(&mut self, key: &'static str, value: Value, cx: &mut Context<Self>) {
        let (Some(backend), Some(settings)) = (self.backend.clone(), self.settings.as_ref()) else {
            return;
        };
        if self.saving {
            return;
        }
        if let Some(reason) = self.blocked() {
            self.error = Some(reason.into());
            cx.notify();
            return;
        }
        let call = backend.call_raw(
            rpc::ServerUpdateSettings::TAG,
            json!({ "patch": rule_patch(settings, key, value) }),
            Access::SettingsWrite,
        );
        self.saving = true;
        self.error = None;
        cx.notify();
        self._write = Some(cx.spawn(async move |this, cx| {
            let result = async {
                let settings = call.await?;
                serde_json::from_value::<ServerSettings>(settings)
                    .context("couldn't read T3 Code's settings")
            }
            .await;
            this.update(cx, |this, cx| {
                this.saving = false;
                match result {
                    Ok(settings) => this.settings = Some(settings),
                    Err(error) => {
                        log::error!("couldn't change the cleanup rule: {error:#}");
                        this.error = Some(format!("Couldn't change the rule: {error:#}").into());
                    }
                }
                cx.notify();
            })
            .ok();
        }));
    }

    /// Delete now: runs every rule at once (`server.runStorageCleanup`).
    pub fn run_now(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(backend) = self.backend.clone() else {
            return;
        };
        if self.running {
            return;
        }
        if let Some(reason) = self.blocked() {
            self.error = Some(reason.into());
            cx.notify();
            return;
        }
        let call = backend.call::<rpc::ServerRunStorageCleanup>(
            ServerRunStorageCleanupPayload(Map::new()),
            Access::SettingsWrite,
        );
        self.running = true;
        self.error = None;
        cx.notify();
        self._write = Some(cx.spawn_in(window, async move |this, cx| {
            let result: Result<StorageCleanupReport> = call.await;
            this.update_in(cx, |this, window, cx| {
                this.running = false;
                match result {
                    Ok(report) => {
                        if let Some(failed) = report
                            .entries
                            .iter()
                            .find(|entry| entry.outcome.to_string() == "failed")
                        {
                            this.error =
                                Some(format!("Some couldn't be removed: {}", failed.reason).into());
                        }
                        this.report = Some(report);
                        if let Some(on_ran) = this.on_ran.clone() {
                            on_ran(window, cx);
                        }
                    }
                    Err(error) => {
                        log::error!("cleanup failed: {error:#}");
                        this.error = Some(format!("Cleanup failed: {error:#}").into());
                    }
                }
                cx.notify();
            })
            .ok();
        }));
    }

    fn switch(
        &self,
        id: &'static str,
        on: bool,
        key: &'static str,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let blocked = self.blocked();
        let this = cx.weak_entity();
        div()
            .id(SharedString::from(format!("{id}-wrapper")))
            .when_some(blocked.clone(), |this, reason| {
                this.tooltip(Tooltip::text(reason))
            })
            .child(
                Switch::new(
                    id,
                    if on {
                        ToggleState::Selected
                    } else {
                        ToggleState::Unselected
                    },
                )
                .disabled(blocked.is_some() || self.saving)
                .on_click(move |_, _, cx| {
                    this.update(cx, |this, cx| this.set_rule(key, json!(!on), cx))
                        .ok();
                }),
            )
            .into_any_element()
    }

    /// Off, or one of a few day counts, for a rule that takes days.
    fn days(
        &self,
        id: &'static str,
        value: Option<i64>,
        choices: &'static [(Option<i64>, &'static str)],
        key: &'static str,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let mut segments: Vec<(Option<i64>, String)> = choices
            .iter()
            .map(|(days, label)| (*days, label.to_string()))
            .collect();
        if !segments.iter().any(|(days, _)| *days == value) {
            segments.push((
                value,
                value.map(|days| format!("{days} days")).unwrap_or_default(),
            ));
        }
        let selected = segments
            .iter()
            .position(|(days, _)| *days == value)
            .unwrap_or(0);
        self.segmented(
            id,
            segments
                .into_iter()
                .map(|(days, label)| (label, json!(days)))
                .collect(),
            selected,
            key,
            cx,
        )
    }

    fn segmented(
        &self,
        id: &'static str,
        segments: Vec<(String, Value)>,
        selected: usize,
        key: &'static str,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let blocked = self.blocked();
        let disabled = blocked.is_some() || self.saving;
        let values = segments
            .iter()
            .map(|(_, value)| value.clone())
            .collect::<Vec<_>>();
        let this = cx.weak_entity();
        let control = segments
            .into_iter()
            .fold(SegmentedControl::new(id), |control, (label, _)| {
                control.segment(label)
            })
            .selected(selected)
            .on_select(move |index, _, cx| {
                if disabled || index == selected {
                    return;
                }
                let value = values[index].clone();
                this.update(cx, |this, cx| this.set_rule(key, value, cx))
                    .ok();
            });
        div()
            .id(SharedString::from(format!("{id}-wrapper")))
            .when(disabled, |this| this.opacity(0.55))
            .when_some(blocked, |this, reason| this.tooltip(Tooltip::text(reason)))
            .child(control)
            .into_any_element()
    }
}

/// A rule: its name and what it does, with its control beside them, or under them when the page
/// is narrow (as the T3 settings page's rows).
fn row(
    title: &'static str,
    description: impl Into<SharedString>,
    control: AnyElement,
    first: bool,
    cx: &App,
) -> AnyElement {
    let palette = palette(cx);
    h_flex()
        .w_full()
        .flex_wrap()
        .items_center()
        .justify_between()
        .gap_x_4()
        .gap_y_1()
        .px_2()
        .py(scaled(px(8.)))
        .when(!first, |this| {
            this.border_t(tokens::BORDER_WIDTH)
                .border_color(palette.border.hsla())
        })
        .child(
            v_flex()
                .flex_1()
                .min_w(scaled(px(240.)))
                .gap_0p5()
                .child(Label::new(title).weight(FontWeight::MEDIUM))
                .child(
                    Label::new(description.into())
                        .size(LabelSize::Small)
                        .color(Color::Muted),
                ),
        )
        .child(div().flex_none().child(control))
        .into_any_element()
}

const INACTIVE_DAYS: &[(Option<i64>, &str)] = &[
    (None, "Off"),
    (Some(8), "8 days"),
    (Some(30), "30 days"),
    (Some(90), "90 days"),
];
const SETTLED_DAYS: &[(Option<i64>, &str)] =
    &[(None, "Off"), (Some(0), "Next run"), (Some(7), "7 days")];

impl Render for WorktreeCleanup {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let palette = palette(cx);
        let rules = self.rules();
        let blocked = self.blocked();
        let summary = match &rules {
            Some(rules) => rules.summary(),
            None if self.error.is_some() => "T3's cleanup rules are unavailable".to_string(),
            None => "Reading T3's cleanup rules…".to_string(),
        };
        let any_on = rules.as_ref().is_some_and(CleanupRules::any_on);
        let delete_now = TextButton::new(
            "worktree-cleanup-run",
            if self.running {
                "Deleting…"
            } else {
                "Delete now"
            },
        )
        .disabled(self.running || !any_on || blocked.is_some())
        .tooltip(match (&blocked, any_on) {
            (Some(reason), _) => reason.clone(),
            (None, false) => "Turn on a cleanup rule to use this.".to_string(),
            (None, true) => "Run T3's cleanup rules now, on every project".to_string(),
        })
        .on_click(cx.listener(|this, _, window, cx| this.run_now(window, cx)));
        let report = self
            .report
            .as_ref()
            .map(|report| report_summary(report, OffsetDateTime::now_utc()))
            .unwrap_or_else(|| "Cleanup hasn't run yet.".to_string());
        let expanded = self.expanded;
        let header = h_flex()
            .id("worktree-cleanup-header")
            .w_full()
            .flex_wrap()
            .items_center()
            .gap_x_3()
            .gap_y_1()
            .child(
                h_flex()
                    .id("worktree-cleanup-toggle")
                    .gap_1p5()
                    .cursor_pointer()
                    .child(
                        Icon::new(if expanded {
                            IconName::ChevronDown
                        } else {
                            IconName::ChevronRight
                        })
                        .size(IconSize::Small)
                        .color(Color::Muted),
                    )
                    .child(Label::new("Automatic cleanup").weight(FontWeight::SEMIBOLD))
                    .on_click(cx.listener(move |this, _, _, cx| this.set_expanded(!expanded, cx))),
            )
            .child(
                div().flex_1().min_w(scaled(px(200.))).child(
                    Label::new(summary)
                        .size(LabelSize::Small)
                        .color(Color::Muted),
                ),
            )
            .child(delete_now);
        let mut card = Card::new().px(scaled(px(12.))).py(scaled(px(8.))).child(
            v_flex()
                .gap_1()
                .child(header)
                .child(
                    Label::new(report)
                        .size(LabelSize::XSmall)
                        .color(Color::Muted),
                )
                .children(self.error.clone().map(|error| {
                    div()
                        .text_size(tokens::text_size(tokens::TEXT_SIZE_SMALL))
                        .text_color(cx.theme().status().error)
                        .child(error)
                })),
        );
        if let (true, Some(rules)) = (expanded, rules) {
            let keep_when = [
                (WorktreeKeepWhen::AnyLocalFiles, "Any local files"),
                (WorktreeKeepWhen::UncommittedChanges, "Uncommitted changes"),
                (WorktreeKeepWhen::TrackedChanges, "Edited tracked files"),
            ];
            let keep_selected = keep_when
                .iter()
                .position(|(value, _)| *value == rules.keep_when)
                .unwrap_or(1);
            let keep_description = match rules.keep_when {
                WorktreeKeepWhen::AnyLocalFiles => "Includes ignored files, except node_modules.",
                WorktreeKeepWhen::TrackedChanges => {
                    "Deletes untracked and ignored files, including .env."
                }
                _ => "Keeps edits and untracked files. Deletes ignored files, including .env.",
            };
            let rows = vec![
                row(
                    "Delete worktrees with deleted threads",
                    "Remove unused worktrees when active or archived threads are deleted. \
                     Worktrees with local changes are kept.",
                    self.switch("cleanup-on-delete", rules.on_delete, "worktreeOnDelete", cx),
                    true,
                    cx,
                ),
                row(
                    "Delete merged worktrees",
                    "Remove worktrees whose pull request is merged and whose commits are in the \
                     default branch.",
                    self.switch("cleanup-on-merge", rules.on_merge, "worktreeOnMerge", cx),
                    false,
                    cx,
                ),
                row(
                    "Delete inactive worktrees",
                    "Remove worktrees after their threads have been inactive this long. Branches \
                     and thread history are kept.",
                    self.days(
                        "cleanup-after-days",
                        rules.after_days,
                        INACTIVE_DAYS,
                        "worktreeAfterDays",
                        cx,
                    ),
                    false,
                    cx,
                ),
                row(
                    "Delete settled worktrees",
                    "Remove worktrees this long after their thread settles. Branches and thread \
                     history are kept.",
                    self.days(
                        "cleanup-settled-days",
                        rules.settled_after_days,
                        SETTLED_DAYS,
                        "worktreeSettledAfterDays",
                        cx,
                    ),
                    false,
                    cx,
                ),
                row(
                    "Delete unchanged worktrees",
                    "Remove worktrees with no commits beyond the default branch.",
                    self.switch(
                        "cleanup-unchanged",
                        rules.unchanged,
                        "worktreeUnchanged",
                        cx,
                    ),
                    false,
                    cx,
                ),
                row(
                    "Keep worktrees with local changes",
                    keep_description,
                    self.segmented(
                        "cleanup-keep-when",
                        keep_when
                            .iter()
                            .map(|(value, label)| {
                                (label.to_string(), json!(keep_when_value(value)))
                            })
                            .collect(),
                        keep_selected,
                        "worktreeKeepWhen",
                        cx,
                    ),
                    false,
                    cx,
                ),
            ];
            // The rules scroll in part of the window, so the worktrees stay in view.
            card = card.child(
                v_flex()
                    .id("worktree-cleanup-rules")
                    .mt_1()
                    .max_h(window.viewport_size().height * 0.4)
                    .overflow_y_scroll()
                    .border_t(tokens::BORDER_WIDTH)
                    .border_color(palette.border.hsla())
                    .children(rows),
            );
        }
        div().w_full().child(card)
    }
}

#[cfg(test)]
#[path = "worktree_cleanup_tests.rs"]
mod tests;

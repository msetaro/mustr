//! The thread's agent: its status, its usage limits, and recovering from a limit
//! (`mustr-docs/PARITY.md` §2.3, §2.5 and §2.17).
//! - **Provider status:** a banner when the thread's agent is signed out, outdated, or erroring,
//!   as T3's `chat/ProviderStatusBanner.tsx` shows it.
//! - **Usage limits:** the agent's windows, such as its 5-hour and weekly ones, with how much is
//!   left and when each resets (`chat/ComposerUsageLimits.tsx`, `usage/UsageLimits.tsx`).
//! - **Usage-limit recovery:** on a thread a limit stopped, when the limit resets, and "Resume at
//!   reset" or "Snooze until reset" (`chat/UsageLimitRecoveryBanner.tsx`), through
//!   `thread.metadata.update` `{limitRecovery}`. Resuming now is the composer's Resume.
//!
//! The agents' status comes from T3's server config: [`ServerConfigs`] follows
//! `subscribeServerConfig` for the whole app.

use crate::{
    backend::{Access, ThreadBackend, ThreadBackendExt as _},
    commands,
    model::ThreadState,
    organize::active_thread_view,
    thread_view::ThreadView,
};
use anyhow::Result;
use futures::StreamExt as _;
use gpui::{
    AnyElement, App, AppContext as _, AsyncApp, Context, Entity, FontWeight, Global, Hsla,
    IntoElement, ParentElement as _, Rems, Styled as _, Task, WeakEntity, actions, div, px,
    relative,
};
use mustr_ui::{ButtonStyle, Card, TextButton, palette, tokens, tokens::scaled};
use std::sync::Arc;
use t3_api::{
    rpc,
    tagged::{OrchestrationV2Command, ServerConfigStreamEvent},
    types::{
        OrchestrationV2CommandThreadMetadataUpdate, OrchestrationV2CommandThreadMetadataUpdateType,
        OrchestrationV2LimitRecoveryUpdate, OrchestrationV2ProviderFailureClass,
        OrchestrationV2ThreadShell, ServerProvider, ServerProviderAuthStatus,
        ServerProviderAvailability, ServerProviderCompatibilityAdvisory,
        ServerProviderCompatibilityStatus, ServerProviderState, ServerProviderUsageLimits,
        ServerProviderUsageLimitsUnavailableReason, SubscribeServerConfigPayload,
    },
};
use time::{OffsetDateTime, format_description::well_known::Rfc3339};
use ui::{
    Color, Icon, IconButton, IconName, IconSize, Label, LabelCommon as _, LabelSize, Tooltip,
    prelude::*,
};
use workspace::Workspace;

actions!(
    thread_chat,
    [
        /// Shows or hides the open agent thread's usage limits: its agent's 5-hour, weekly, and
        /// other windows.
        ShowUsageLimits,
    ]
);

// T3's server config, for the whole app.

/// T3's agents as its server config reports them, kept current by `subscribeServerConfig`.
/// It starts following T3 when a view first asks for it.
pub struct ServerConfigs {
    providers: Option<Vec<ServerProvider>>,
    _follow: Option<Task<()>>,
}

struct GlobalServerConfigs(Entity<ServerConfigs>);

impl Global for GlobalServerConfigs {}

impl ServerConfigs {
    /// The app's config, which starts following T3 through the installed backend on first use.
    pub fn global(cx: &mut App) -> Entity<Self> {
        if let Some(global) = cx.try_global::<GlobalServerConfigs>() {
            return global.0.clone();
        }
        let configs = cx.new(|cx| {
            let mut configs = Self {
                providers: None,
                _follow: None,
            };
            configs.follow(cx);
            configs
        });
        cx.set_global(GlobalServerConfigs(configs.clone()));
        configs
    }

    /// The app's config, if something already follows it.
    pub fn try_global(cx: &App) -> Option<Entity<Self>> {
        cx.try_global::<GlobalServerConfigs>()
            .map(|global| global.0.clone())
    }

    /// T3's agents, once the config has loaded.
    pub fn providers(&self) -> Option<&[ServerProvider]> {
        self.providers.as_deref()
    }

    /// One agent, by its instance id.
    pub fn provider(&self, instance_id: &str) -> Option<&ServerProvider> {
        self.providers
            .as_ref()?
            .iter()
            .find(|provider| provider.instance_id == instance_id)
    }

    fn follow(&mut self, cx: &mut Context<Self>) {
        let Some(backend) = crate::backend(cx) else {
            return;
        };
        self._follow = Some(cx.spawn(async move |this, cx| {
            let mut attempt = 0;
            loop {
                match follow_config(&backend, &this, cx).await {
                    Ok(()) => attempt = 0,
                    Err(error) => log::warn!("couldn't follow T3's server config: {error:#}"),
                }
                if this.upgrade().is_none() {
                    return;
                }
                cx.background_executor()
                    .timer(crate::backend::retry_delay(attempt))
                    .await;
                attempt += 1;
            }
        }));
    }
}

/// Follows `subscribeServerConfig`: a snapshot of the config, then the agents' statuses as they
/// change.
async fn follow_config(
    backend: &Arc<dyn ThreadBackend>,
    this: &WeakEntity<ServerConfigs>,
    cx: &mut AsyncApp,
) -> Result<()> {
    let mut stream = backend.stream::<rpc::SubscribeServerConfig>(
        SubscribeServerConfigPayload::default(),
        Access::Read,
    )?;
    while let Some(event) = stream.next().await {
        let providers = match event {
            Ok(ServerConfigStreamEvent::Snapshot(snapshot)) => snapshot.config.providers,
            Ok(ServerConfigStreamEvent::ProviderStatuses(statuses)) => statuses.payload.providers,
            Ok(_) => continue,
            Err(error) => {
                log::warn!("skipping a server config event mustr can't read: {error:#}");
                continue;
            }
        };
        this.update(cx, |this, cx| {
            this.providers = Some(providers);
            cx.notify();
        })?;
    }
    Ok(())
}

/// The agent instance a thread runs on: its model selection's, as T3's composer picks it.
pub fn thread_instance_id(state: &ThreadState) -> &str {
    crate::thread_settings::selection_instance_id(&state.thread.model_selection)
        .unwrap_or(&state.thread.provider_instance_id)
}

/// Whether an agent takes `/compact` (`providerSupportsManualCompaction` in T3).
pub fn supports_compaction(provider: &ServerProvider) -> bool {
    let is_compact = |name: &str| name == "compact";
    provider
        .slash_commands
        .iter()
        .flatten()
        .any(|command| is_compact(&command.name))
        || provider
            .workspace_snapshots
            .iter()
            .flatten()
            .flat_map(|snapshot| &snapshot.slash_commands)
            .any(|command| is_compact(&command.name))
}

/// An agent's name, as T3 shows it: its display name, or its driver in words
/// (`formatProviderDriverKindLabel`).
pub fn provider_name(provider: &ServerProvider) -> String {
    if let Some(name) = provider
        .display_name
        .as_deref()
        .map(str::trim)
        .filter(|name| !name.is_empty())
    {
        return name.to_string();
    }
    driver_label(&provider.driver)
}

fn driver_label(driver: &str) -> String {
    let mut label = String::new();
    let mut previous: Option<char> = None;
    for char in driver.chars() {
        if char == '_' || char == '-' {
            label.push(' ');
        } else {
            if char.is_uppercase() && previous.is_some_and(|previous| previous.is_lowercase()) {
                label.push(' ');
            }
            label.push(char);
        }
        previous = Some(char);
    }
    label
        .split_whitespace()
        .map(|word| {
            let mut chars = word.chars();
            chars
                .next()
                .map(|first| first.to_uppercase().chain(chars).collect::<String>())
                .unwrap_or_default()
        })
        .collect::<Vec<_>>()
        .join(" ")
}

// The provider status banner.

/// What the provider status banner says about the thread's agent.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ProviderBanner {
    /// Changes when what's wrong does, so a dismissed banner comes back for a new problem.
    pub key: String,
    pub title: String,
    pub message: String,
    /// An error rather than a warning.
    pub error: bool,
}

/// A version of the agent T3 knows to be broken, or a working one it doesn't support.
fn incompatible_version(provider: &ServerProvider) -> Option<&ServerProviderCompatibilityAdvisory> {
    if provider.status == ServerProviderState::Error
        && provider.auth.status == ServerProviderAuthStatus::Unauthenticated
    {
        return None;
    }
    let advisory = provider.compatibility_advisory.as_ref()?;
    (advisory.status == ServerProviderCompatibilityStatus::Broken
        || (provider.status == ServerProviderState::Ready
            && advisory.status == ServerProviderCompatibilityStatus::Unsupported))
        .then_some(advisory)
}

/// What's wrong with the agent, in words (`getProviderStatusMessage` in T3, without its pointers
/// to T3's provider setup, which mustr doesn't have yet).
fn provider_status_message(provider: &ServerProvider) -> String {
    if provider.auth.status != ServerProviderAuthStatus::Unauthenticated
        && let Some(advisory) = &provider.compatibility_advisory
        && advisory.status == ServerProviderCompatibilityStatus::Broken
        && let Some(message) = &advisory.message
    {
        return message.clone();
    }
    if let Some(message) = provider
        .message
        .as_ref()
        .filter(|message| !message.trim().is_empty())
    {
        return message.clone();
    }
    let name = provider_name(provider);
    if !provider.installed {
        return format!(
            "Install {} where T3 Code runs, then restart the agent session.",
            driver_label(&provider.driver)
        );
    }
    if provider.auth.status == ServerProviderAuthStatus::Unauthenticated {
        return "Sign in via the CLI to authenticate again.".to_string();
    }
    match provider.status {
        ServerProviderState::Ready => "No models are available for this provider.".to_string(),
        ServerProviderState::Error => format!("{name} provider is unavailable."),
        _ => format!("{name} provider has limited availability."),
    }
}

/// The provider status banner for an agent, if it needs one (`getProviderStatusBannerKey` and
/// `ProviderStatusBanner` in T3): signed out, a broken or unsupported version, or not ready.
pub fn provider_banner(provider: &ServerProvider) -> Option<ProviderBanner> {
    if provider.status == ServerProviderState::Disabled {
        return None;
    }
    let incompatible = incompatible_version(provider);
    let status = crate::model::wire_string(&provider.status);
    let auth = crate::model::wire_string(&provider.auth.status);
    let key = match incompatible {
        Some(advisory) => [
            provider.instance_id.clone(),
            crate::model::wire_string(&advisory.status),
            provider.version.clone().unwrap_or_default(),
            advisory.message.clone().unwrap_or_default(),
        ]
        .join("\u{0}"),
        None => {
            if provider.status == ServerProviderState::Ready {
                return None;
            }
            // Antigravity checks its credentials when a session starts.
            if provider.driver == "antigravity"
                && provider.installed
                && provider.status == ServerProviderState::Warning
                && provider.auth.status == ServerProviderAuthStatus::Unknown
            {
                return None;
            }
            [
                provider.instance_id.clone(),
                status,
                auth,
                provider.message.clone().unwrap_or_default(),
            ]
            .join("\u{0}")
        }
    };
    let name = provider_name(provider);
    let unauthenticated = provider.status == ServerProviderState::Error
        && provider.auth.status == ServerProviderAuthStatus::Unauthenticated;
    let broken = incompatible
        .is_some_and(|advisory| advisory.status == ServerProviderCompatibilityStatus::Broken);
    let title = if unauthenticated {
        format!("{name} is signed out")
    } else if incompatible.is_some() {
        let version = provider.version.clone().unwrap_or_default();
        let verdict = if broken {
            "is known to be broken"
        } else {
            "is unsupported"
        };
        format!("{name} {version} {verdict}").replace("  ", " ")
    } else {
        format!("{name} provider status")
    };
    let message = incompatible
        .and_then(|advisory| advisory.message.clone())
        .unwrap_or_else(|| provider_status_message(provider));
    let warning =
        !broken && (provider.status == ServerProviderState::Warning || incompatible.is_some());
    Some(ProviderBanner {
        key,
        title,
        message,
        error: !warning,
    })
}

// Usage limits.

/// One of an agent's usage windows, such as its 5-hour or weekly one.
#[derive(Clone, Debug, PartialEq)]
pub struct LimitWindow {
    pub label: String,
    /// How much of the window is left, from 0 to 100.
    pub remaining_percent: u32,
    /// "resets in 2h 14m", when T3 knows.
    pub resets_in: Option<String>,
}

/// An agent's usage limits, as the usage panel shows them.
#[derive(Clone, Debug, PartialEq)]
pub struct UsageLimits {
    /// The agent and its account: "Claude Code · Max".
    pub account: String,
    /// Why there are no windows to show, when there aren't (`limitsNotice` in T3).
    pub notice: Option<String>,
    pub windows: Vec<LimitWindow>,
    /// Where the account's usage is managed, as `(label, url)`.
    pub manage: Option<(String, String)>,
}

/// Why an agent shows no windows, if it shows none (`limitsNotice` in T3).
fn limits_notice(limits: &ServerProviderUsageLimits) -> Option<String> {
    if let Some(unavailable) = &limits.unavailable {
        match unavailable.reason {
            ServerProviderUsageLimitsUnavailableReason::Unsupported => {
                return Some(
                    unavailable
                        .message
                        .clone()
                        .unwrap_or_else(|| "This account has no subscription limits.".into()),
                );
            }
            ServerProviderUsageLimitsUnavailableReason::ProbeFailed => {
                return Some(
                    unavailable
                        .message
                        .clone()
                        .unwrap_or_else(|| "Couldn't read the limits.".into()),
                );
            }
            _ => {}
        }
    }
    limits
        .windows
        .is_empty()
        .then(|| "No limits reported.".to_string())
}

/// A span of time as T3 words it: "2d 3h", "2h 14m", or "14m" (`formatDuration`).
pub fn format_duration(seconds: i64) -> String {
    let remaining = seconds.max(0);
    let days = remaining / 86_400;
    let hours = remaining % 86_400 / 3_600;
    let minutes = remaining % 3_600 / 60;
    if days > 0 {
        format!("{days}d {hours}h")
    } else if hours > 0 {
        format!("{hours}h {minutes}m")
    } else {
        format!("{minutes}m")
    }
}

fn parse_time(timestamp: &str) -> Option<OffsetDateTime> {
    OffsetDateTime::parse(timestamp, &Rfc3339).ok()
}

/// When a window resets, from `now` (`formatResetsIn` in T3).
fn resets_in(resets_at: Option<&str>, now: OffsetDateTime) -> Option<String> {
    let resets_at = parse_time(resets_at?)?;
    let seconds = (resets_at - now).whole_seconds();
    Some(if seconds <= 0 {
        "resets now".to_string()
    } else if seconds < 60 {
        "resets momentarily".to_string()
    } else {
        format!("resets in {}", format_duration(seconds))
    })
}

/// An agent's usage limits, if T3 reads them for it: an enabled, installed, and available agent
/// that reports them (`providersWithLimits` in T3).
pub fn usage_limits(provider: &ServerProvider, now: OffsetDateTime) -> Option<UsageLimits> {
    if !provider.enabled
        || !provider.installed
        || provider.availability == Some(ServerProviderAvailability::Unavailable)
    {
        return None;
    }
    let limits = provider.usage_limits.as_ref()?;
    let mut account = provider_name(provider);
    if let Some(plan) = provider
        .auth
        .label
        .as_deref()
        .filter(|plan| !plan.trim().is_empty())
    {
        account.push_str(" · ");
        account.push_str(plan.trim());
    }
    let notice = limits_notice(limits);
    let windows = if notice.is_some() {
        Vec::new()
    } else {
        limits
            .windows
            .iter()
            .map(|window| LimitWindow {
                label: window.label.clone(),
                remaining_percent: (100. - window.used_percent.0.clamp(0., 100.)).round() as u32,
                resets_in: resets_in(window.resets_at.as_deref(), now),
            })
            .collect()
    };
    Some(UsageLimits {
        account,
        notice,
        windows,
        manage: limits
            .external_usage
            .as_ref()
            .map(|external| (external.label.clone(), external.url.clone())),
    })
}

// Usage-limit recovery.

/// What the usage-limit recovery banner shows for a thread a limit stopped
/// (`usageLimitRecoveryBannerItem` in T3).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct LimitRecovery {
    /// The run the limit stopped.
    pub run_id: String,
    /// When the limit resets, as T3 sent it.
    pub reset_at: Option<String>,
    /// Whether the reset is still to come, so the thread can wait for it.
    pub can_schedule: bool,
    /// Whether T3 resumes the run when the limit resets.
    pub auto_resume: bool,
    /// Whether the thread is snoozed until the limit resets.
    pub snoozed: bool,
}

/// The usage-limit recovery banner for a thread, from its record in T3's shell: when its latest
/// run failed at a usage limit.
pub fn limit_recovery(
    thread: &OrchestrationV2ThreadShell,
    now: OffsetDateTime,
) -> Option<LimitRecovery> {
    if crate::model::wire_string(&thread.status) != "failed"
        || thread.last_error_class != Some(OrchestrationV2ProviderFailureClass::UsageLimit)
    {
        return None;
    }
    let run_id = thread.latest_run_id.clone()?;
    let reset_at = thread.usage_limit_reset_at.clone();
    let stopped_at = thread
        .latest_run_completed_at
        .as_deref()
        .unwrap_or(&thread.updated_at);
    let reset = reset_at.as_deref().and_then(parse_time);
    let can_schedule = match (reset, parse_time(stopped_at)) {
        (Some(reset), Some(stopped)) => reset > stopped,
        _ => false,
    };
    let recovery = thread.limit_recovery.as_ref().filter(|recovery| {
        recovery.run_id == run_id && Some(&recovery.reset_at) == reset_at.as_ref()
    });
    let auto_resume = recovery.is_some_and(|recovery| recovery.auto_resume);
    // T3 leaves `snoozedUntil` set after the snooze wakes the thread.
    let snoozed = recovery.is_some_and(|recovery| recovery.snooze == Some(true))
        && reset.is_some_and(|reset| reset > now)
        && thread.snoozed_until.as_deref().and_then(parse_time) == reset;
    Some(LimitRecovery {
        run_id,
        reset_at,
        can_schedule,
        auto_resume,
        snoozed,
    })
}

/// A change to how a limited thread recovers.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RecoveryChange {
    /// Resume the run when the limit resets, or stop waiting to.
    AutoResume(bool),
    /// Snooze the thread until the limit resets, or wake it.
    Snooze(bool),
}

/// Changes how a limited thread recovers (`thread.metadata.update` `{limitRecovery}`).
pub fn update_limit_recovery(
    thread_id: &str,
    run_id: &str,
    reset_at: &str,
    change: RecoveryChange,
    command_id: String,
) -> OrchestrationV2Command {
    let (auto_resume, snooze) = match change {
        RecoveryChange::AutoResume(on) => (Some(on), None),
        RecoveryChange::Snooze(on) => (None, Some(on)),
    };
    OrchestrationV2Command::ThreadMetadataUpdate(OrchestrationV2CommandThreadMetadataUpdate {
        branch: None,
        command_id,
        expected_branch: None,
        expected_empty: None,
        expected_worktree_path: None,
        limit_recovery: Some(OrchestrationV2LimitRecoveryUpdate {
            auto_resume,
            reset_at: reset_at.to_string(),
            run_id: run_id.to_string(),
            snooze,
        }),
        linked_pull_request: None,
        regenerate_title: None,
        thread_id: thread_id.to_string(),
        title: None,
        type_: OrchestrationV2CommandThreadMetadataUpdateType::ThreadMetadataUpdate,
        worktree_path: None,
    })
}

/// A reset time in the user's time zone: "3:40 PM" today, "Thu 9:00 AM" this week, or the date.
fn local_time(timestamp: &str, now: OffsetDateTime) -> Option<String> {
    use chrono::{Datelike as _, Local, TimeZone as _};
    let at = parse_time(timestamp)?;
    let local = Local.timestamp_opt(at.unix_timestamp(), 0).single()?;
    let today = Local.timestamp_opt(now.unix_timestamp(), 0).single()?;
    let days = local
        .date_naive()
        .signed_duration_since(today.date_naive())
        .num_days();
    Some(if days == 0 {
        local.format("%-I:%M %p").to_string()
    } else if (1..7).contains(&days) {
        local.format("%a %-I:%M %p").to_string()
    } else if local.year() == today.year() {
        local.format("%b %-d, %-I:%M %p").to_string()
    } else {
        local.format("%b %-d %Y, %-I:%M %p").to_string()
    })
}

// A thread view's banners.

/// The banners' state in a thread view.
#[derive(Default)]
pub(crate) struct UsageState {
    /// The provider banner the user dismissed, by its key.
    dismissed_banner: Option<String>,
    /// Whether the usage limits panel is open.
    panel_open: bool,
    /// While a change to the thread's recovery is being sent.
    recovery_pending: bool,
    /// The usage-limit recovery banner's state, from the thread's record in T3's shell.
    recovery: Option<LimitRecovery>,
}

impl ThreadView {
    /// The thread's agent, as T3's config reports it.
    pub(crate) fn provider<'a>(&self, cx: &'a App) -> Option<&'a ServerProvider> {
        let state = self.thread_state()?;
        ServerConfigs::try_global(cx)?
            .read(cx)
            .provider(thread_instance_id(state))
    }

    /// The thread's agent's usage limits, if T3 reads them.
    pub(crate) fn usage_limits(&self, cx: &App) -> Option<UsageLimits> {
        usage_limits(self.provider(cx)?, OffsetDateTime::now_utc())
    }

    pub(crate) fn usage_panel_open(&self) -> bool {
        self.history.usage.panel_open
    }

    pub(crate) fn toggle_usage_limits(&mut self, cx: &mut Context<Self>) {
        self.history.usage.panel_open = !self.history.usage.panel_open;
        cx.notify();
    }

    pub(crate) fn dismiss_provider_banner(&mut self, key: String, cx: &mut Context<Self>) {
        self.history.usage.dismissed_banner = Some(key);
        cx.notify();
    }

    /// The provider status banner, unless the user dismissed it.
    pub(crate) fn provider_banner(&self, cx: &App) -> Option<ProviderBanner> {
        provider_banner(self.provider(cx)?)
            .filter(|banner| self.history.usage.dismissed_banner.as_ref() != Some(&banner.key))
    }

    /// The usage-limit recovery banner's state, when a limit stopped the thread.
    pub(crate) fn limit_recovery(&self) -> Option<LimitRecovery> {
        self.history.usage.recovery.clone()
    }

    /// Reads the thread's limit from its record in T3's shell, redrawing when it changed.
    pub(crate) fn sync_limit_recovery(&mut self, cx: &mut Context<Self>) {
        let recovery = self.thread_id.as_deref().and_then(|thread_id| {
            let shells = self.history.shells.as_ref()?.read(cx);
            limit_recovery(shells.thread(thread_id)?, OffsetDateTime::now_utc())
        });
        if recovery != self.history.usage.recovery {
            self.history.usage.recovery = recovery;
            cx.notify();
        }
    }

    /// Changes how the limited thread recovers.
    pub(crate) fn change_limit_recovery(&mut self, change: RecoveryChange, cx: &mut Context<Self>) {
        let Some(recovery) = self.limit_recovery() else {
            return;
        };
        let (Some(thread_id), Some(reset_at)) = (self.thread_id.clone(), recovery.reset_at.clone())
        else {
            return;
        };
        if self.history.usage.recovery_pending {
            return;
        }
        // Like T3, a snooze can't start once the reset has passed.
        if change == RecoveryChange::Snooze(true)
            && parse_time(&reset_at).is_some_and(|reset| reset <= OffsetDateTime::now_utc())
        {
            self.error = Some(
                "Couldn't snooze the thread: the limit has reset. Resume the thread instead."
                    .into(),
            );
            cx.notify();
            return;
        }
        let command = update_limit_recovery(
            &thread_id,
            &recovery.run_id,
            &reset_at,
            change,
            commands::new_id(),
        );
        let dispatch = self.backend.dispatch(command);
        self.history.usage.recovery_pending = true;
        self.error = None;
        cx.notify();
        self._tasks.push(cx.spawn(async move |this, cx| {
            let result = dispatch.await;
            this.update(cx, |this, cx| {
                this.history.usage.recovery_pending = false;
                if let Err(error) = result {
                    let what = match change {
                        RecoveryChange::AutoResume(_) => "change auto-resume",
                        RecoveryChange::Snooze(_) => "snooze the thread",
                    };
                    this.error = Some(format!("Couldn't {what}: {error:#}").into());
                }
                cx.notify();
            })
            .ok();
        }));
    }

    /// The banners under the header: the agent's status, a usage limit that stopped the thread,
    /// and the usage limits panel.
    pub(crate) fn render_banners(&mut self, cx: &mut Context<Self>) -> Option<AnyElement> {
        let mut banners: Vec<AnyElement> = Vec::new();
        if let Some(banner) = self.provider_banner(cx) {
            banners.push(self.render_provider_banner(banner, cx));
        }
        if let Some(recovery) = self.limit_recovery() {
            banners.push(self.render_limit_recovery(recovery, cx));
        }
        if self.history.usage.panel_open {
            banners.push(self.render_usage_panel(cx));
        }
        if banners.is_empty() {
            return None;
        }
        Some(
            h_flex()
                .w_full()
                .justify_center()
                .px_4()
                .pt_2()
                .child(
                    v_flex()
                        .w_full()
                        .max_w(scaled(px(crate::thread_view::CONTENT_MAX_WIDTH)))
                        .gap_1p5()
                        .children(banners),
                )
                .into_any_element(),
        )
    }

    fn render_provider_banner(&self, banner: ProviderBanner, cx: &mut Context<Self>) -> AnyElement {
        let palette = palette(cx);
        let color: Hsla = if banner.error {
            cx.theme().status().error
        } else {
            palette.warning.hsla()
        };
        let key = banner.key.clone();
        banner_card(color)
            .child(
                h_flex()
                    .items_start()
                    .gap_2()
                    .child(
                        Icon::new(if banner.error {
                            IconName::XCircle
                        } else {
                            IconName::Warning
                        })
                        .size(IconSize::Small)
                        .color(if banner.error {
                            Color::Error
                        } else {
                            Color::Warning
                        }),
                    )
                    .child(
                        v_flex()
                            .flex_1()
                            .min_w_0()
                            .gap_0p5()
                            .child(
                                Label::new(banner.title)
                                    .size(LabelSize::Small)
                                    .weight(FontWeight::SEMIBOLD),
                            )
                            .child(
                                Label::new(banner.message)
                                    .size(LabelSize::Small)
                                    .color(Color::Muted),
                            ),
                    )
                    .child(
                        IconButton::new("thread-provider-banner-dismiss", IconName::Close)
                            .icon_size(IconSize::XSmall)
                            .icon_color(Color::Muted)
                            .tooltip(Tooltip::text("Dismiss"))
                            .on_click(cx.listener(move |this, _, _, cx| {
                                this.dismiss_provider_banner(key.clone(), cx)
                            })),
                    ),
            )
            .into_any_element()
            .into_selected("thread-provider-banner")
    }

    fn render_limit_recovery(&self, recovery: LimitRecovery, cx: &mut Context<Self>) -> AnyElement {
        let palette = palette(cx);
        let now = OffsetDateTime::now_utc();
        let description = match recovery.reset_at.as_deref() {
            Some(reset_at) => {
                let at = local_time(reset_at, now);
                let seconds = parse_time(reset_at).map(|reset| (reset - now).whole_seconds());
                match (seconds, at) {
                    (Some(seconds), Some(at)) if seconds > 60 => {
                        format!("Resets in {}, at {at}", format_duration(seconds))
                    }
                    (_, Some(at)) => format!("Reset at {at}"),
                    _ => "Resets soon".to_string(),
                }
            }
            None => "Reset time unavailable; retry by hand".to_string(),
        };
        let blocked = self.backend.blocked_reason(Access::Operate);
        let pending = self.history.usage.recovery_pending;
        let disabled = blocked.is_some() || pending;
        let can_resume_now = self
            .thread_state()
            .and_then(ThreadState::resumable)
            .is_some();
        let tooltip = |text: &str| match &blocked {
            Some(reason) => format!("{text} ({reason})"),
            None => text.to_string(),
        };
        let mut actions = h_flex().gap_1p5().flex_none();
        if recovery.can_schedule {
            let auto_resume = recovery.auto_resume;
            let snoozed = recovery.snoozed;
            actions = actions
                .child(
                    div()
                        .debug_selector(|| "thread-limit-resume-at-reset".into())
                        .child(
                            TextButton::new(
                                "thread-limit-resume-at-reset",
                                if auto_resume {
                                    "Cancel Auto-resume"
                                } else {
                                    "Resume at Reset"
                                },
                            )
                            .disabled(disabled)
                            .tooltip(tooltip(if auto_resume {
                                "Stop waiting to resume the run"
                            } else {
                                "Resume the run when the limit resets"
                            }))
                            .on_click(cx.listener(
                                move |this, _, _, cx| {
                                    this.change_limit_recovery(
                                        RecoveryChange::AutoResume(!auto_resume),
                                        cx,
                                    )
                                },
                            )),
                        ),
                )
                .child(
                    div().debug_selector(|| "thread-limit-snooze".into()).child(
                        TextButton::new(
                            "thread-limit-snooze",
                            if snoozed {
                                "Wake Now"
                            } else {
                                "Snooze Until Reset"
                            },
                        )
                        .disabled(disabled)
                        .tooltip(tooltip(if snoozed {
                            "Bring the thread back now"
                        } else {
                            "Hide the thread until the limit resets"
                        }))
                        .on_click(cx.listener(move |this, _, _, cx| {
                            this.change_limit_recovery(RecoveryChange::Snooze(!snoozed), cx)
                        })),
                    ),
                );
        }
        if can_resume_now {
            actions = actions.child(
                div()
                    .debug_selector(|| "thread-limit-resume-now".into())
                    .child(
                        TextButton::new("thread-limit-resume-now", "Resume Now")
                            .style(ButtonStyle::Primary)
                            .disabled(blocked.is_some() || self.sending)
                            .tooltip(tooltip("Continue where the agent left off"))
                            .on_click(cx.listener(|this, _, window, cx| this.resume(window, cx))),
                    ),
            );
        }
        banner_card(palette.warning.hsla())
            .child(
                h_flex()
                    .gap_2()
                    .child(
                        Icon::new(IconName::Warning)
                            .size(IconSize::Small)
                            .color(Color::Warning),
                    )
                    .child(
                        v_flex()
                            .flex_1()
                            .min_w_0()
                            .gap_0p5()
                            .child(
                                Label::new("Usage limit reached")
                                    .size(LabelSize::Small)
                                    .weight(FontWeight::SEMIBOLD),
                            )
                            .child(
                                Label::new(description)
                                    .size(LabelSize::Small)
                                    .color(Color::Muted),
                            ),
                    )
                    .child(actions),
            )
            .into_any_element()
            .into_selected("thread-limit-recovery")
    }

    fn render_usage_panel(&self, cx: &mut Context<Self>) -> AnyElement {
        let palette = palette(cx);
        let limits = self.usage_limits(cx);
        let header = h_flex()
            .gap_2()
            .child(
                Icon::new(IconName::SignalMedium)
                    .size(IconSize::Small)
                    .color(Color::Muted),
            )
            .child(
                Label::new("Usage limits")
                    .size(LabelSize::Small)
                    .weight(FontWeight::SEMIBOLD),
            )
            .when_some(limits.as_ref(), |this, limits| {
                this.child(
                    Label::new(limits.account.clone())
                        .size(LabelSize::Small)
                        .color(Color::Muted)
                        .truncate(),
                )
            })
            .child(div().flex_1())
            .when_some(
                limits.as_ref().and_then(|limits| limits.manage.clone()),
                |this, (label, url)| {
                    this.child(
                        TextButton::new("thread-usage-manage", label)
                            .tooltip(url.clone())
                            .on_click(move |_, _, cx| cx.open_url(&url)),
                    )
                },
            )
            .child(
                IconButton::new("thread-usage-close", IconName::Close)
                    .icon_size(IconSize::XSmall)
                    .icon_color(Color::Muted)
                    .tooltip(Tooltip::text("Hide usage limits"))
                    .on_click(cx.listener(|this, _, _, cx| this.toggle_usage_limits(cx))),
            );
        let body: AnyElement = match limits {
            None => Label::new("T3 doesn't report usage limits for this agent.")
                .size(LabelSize::Small)
                .color(Color::Muted)
                .into_any_element(),
            Some(UsageLimits {
                notice: Some(notice),
                ..
            }) => Label::new(notice)
                .size(LabelSize::Small)
                .color(Color::Muted)
                .into_any_element(),
            Some(limits) => v_flex()
                .gap_1()
                .children(limits.windows.into_iter().map(|window| {
                    let low = window.remaining_percent < 15;
                    h_flex()
                        .gap_3()
                        .child(
                            div()
                                .w(scaled(px(120.)))
                                .flex_none()
                                .child(Label::new(window.label).size(LabelSize::Small).truncate()),
                        )
                        .child(meter_bar(
                            window.remaining_percent as f32 / 100.,
                            if low {
                                palette.warning.hsla()
                            } else {
                                palette.accent.hsla()
                            },
                            scaled(px(160.)),
                            cx,
                        ))
                        .child(
                            Label::new(format!("{}% left", window.remaining_percent))
                                .size(LabelSize::Small)
                                .color(if low { Color::Warning } else { Color::Default }),
                        )
                        .child(div().flex_1())
                        .child(
                            Label::new(window.resets_in.unwrap_or_default())
                                .size(LabelSize::Small)
                                .color(Color::Muted),
                        )
                }))
                .into_any_element(),
        };
        Card::new()
            .gap_1p5()
            .child(header)
            .child(body)
            .into_any_element()
            .into_selected("thread-usage-panel")
    }

    /// The header's usage limits: each window's share left, which opens the usage panel.
    pub(crate) fn render_usage_summary(&self, cx: &mut Context<Self>) -> Option<AnyElement> {
        let limits = self.usage_limits(cx)?;
        let palette = palette(cx);
        let open = self.history.usage.panel_open;
        let windows = limits.windows.iter().take(2).cloned().collect::<Vec<_>>();
        let summary: AnyElement = if windows.is_empty() {
            Label::new("Limits")
                .size(LabelSize::XSmall)
                .color(Color::Muted)
                .into_any_element()
        } else {
            h_flex()
                .gap_2()
                .children(windows.into_iter().map(|window| {
                    let low = window.remaining_percent < 15;
                    v_flex()
                        .items_end()
                        .gap_0p5()
                        .child(
                            Label::new(format!(
                                "{} {}% left",
                                short_window_label(&window.label),
                                window.remaining_percent
                            ))
                            .size(LabelSize::XSmall)
                            .color(if low {
                                Color::Warning
                            } else {
                                Color::Muted
                            }),
                        )
                        .child(meter_bar(
                            window.remaining_percent as f32 / 100.,
                            if low {
                                palette.warning.hsla()
                            } else {
                                palette.text_muted.hsla()
                            },
                            scaled(px(48.)),
                            cx,
                        ))
                }))
                .into_any_element()
        };
        Some(
            h_flex()
                .id("thread-usage-summary")
                .debug_selector(|| "thread-usage-summary".into())
                .flex_none()
                .px_1p5()
                .py_0p5()
                .rounded(scaled(tokens::CONTROL_RADIUS))
                .cursor_pointer()
                .when(open, |this| this.bg(palette.surface_2.hsla()))
                .hover(|this| this.bg(palette.surface_2.hsla()))
                .tooltip(Tooltip::text(if open {
                    "Hide usage limits"
                } else {
                    "Usage limits: how much of each window is left"
                }))
                .on_click(cx.listener(|this, _, _, cx| this.toggle_usage_limits(cx)))
                .child(summary)
                .into_any_element(),
        )
    }
}

/// A shorter name for a window in the header: "5h" for a 5-hour window.
fn short_window_label(label: &str) -> String {
    let lower = label.to_lowercase();
    if lower.contains("5-hour") || lower.contains("5 hour") {
        "5h".to_string()
    } else {
        label.to_string()
    }
}

/// A thin bar filled to `fraction`, like the context meter's.
pub(crate) fn meter_bar(fraction: f32, color: Hsla, width: Rems, cx: &App) -> impl IntoElement {
    let palette = palette(cx);
    div()
        .w(width)
        .h(scaled(px(3.)))
        .flex_none()
        .rounded_full()
        .bg(palette.surface_2.hsla())
        .child(
            div()
                .h_full()
                .w(relative(fraction.clamp(0., 1.)))
                .rounded_full()
                .bg(color),
        )
}

/// An element with a selector for tests.
trait IntoSelected {
    fn into_selected(self, selector: &'static str) -> AnyElement;
}

impl IntoSelected for AnyElement {
    fn into_selected(self, selector: &'static str) -> AnyElement {
        div()
            .debug_selector(|| selector.into())
            .child(self)
            .into_any_element()
    }
}

/// A banner's card: tinted by `color`, as the timeline's warning cards are.
fn banner_card(color: Hsla) -> Card {
    Card::new()
        .py(scaled(px(6.)))
        .border_color(color.opacity(0.55))
}

// The palette's action, on the thread the workspace shows.

pub(crate) fn init(cx: &mut App) {
    cx.observe_new(|workspace: &mut Workspace, _, _| register(workspace))
        .detach();
}

fn register(workspace: &mut Workspace) {
    workspace.register_action(|workspace, _: &ShowUsageLimits, _, cx| {
        if let Some(view) = active_thread_view(workspace, cx) {
            view.update(cx, |view, cx| view.toggle_usage_limits(cx));
        }
    });
}

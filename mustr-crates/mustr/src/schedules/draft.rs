//! A scheduled task as its form holds it, the `scheduledTasks.upsert` it saves as, and how a
//! task reads in the list. Built the way T3's settings page builds them (`scheduleFromDraft`,
//! `taskToDraft`, and `submit` in T3's
//! `apps/web/src/components/settings/ScheduledTasksSettings.tsx` and its `.logic.ts`).

use std::collections::BTreeSet;
use t3_client::t3_api::{
    tagged::{
        OrchestrationV2ThreadLaunchWorkspaceStrategy, ScheduledTaskSchedule,
        ScheduledTaskUpsertSchedule,
    },
    types::{
        ModelSelection, OrchestrationV2CreationSource,
        OrchestrationV2ThreadLaunchWorkspaceStrategyExistingWorktree,
        OrchestrationV2ThreadLaunchWorkspaceStrategyExistingWorktreeType,
        OrchestrationV2ThreadLaunchWorkspaceStrategyRoot,
        OrchestrationV2ThreadLaunchWorkspaceStrategyRootType,
        OrchestrationV2ThreadLaunchWorkspaceStrategyWorktree,
        OrchestrationV2ThreadLaunchWorkspaceStrategyWorktreeType, ProviderInteractionMode,
        RuntimeMode, ScheduledTask, ScheduledTaskUpsertInput, ScheduledTaskUpsertScheduleFixedTime,
        ScheduledTaskUpsertScheduleFixedTimeType, ScheduledTaskUpsertScheduleInterval,
        ScheduledTaskUpsertScheduleIntervalType, ScheduledTaskUpsertScheduleWebhook,
        ScheduledTaskUpsertScheduleWebhookSignature,
        ScheduledTaskUpsertScheduleWebhookSignatureEncoding,
        ScheduledTaskUpsertScheduleWebhookType,
    },
};
use time::{OffsetDateTime, format_description::well_known::Rfc3339};

/// The weekday chips, Monday first, with T3's numbers (0 is Sunday).
pub const WEEKDAYS: [(i64, &str); 7] = [
    (1, "Mo"),
    (2, "Tu"),
    (3, "We"),
    (4, "Th"),
    (5, "Fr"),
    (6, "Sa"),
    (0, "Su"),
];
const WEEKDAY_NAMES: [&str; 7] = ["Sun", "Mon", "Tue", "Wed", "Thu", "Fri", "Sat"];
/// T3's shortest interval (`MIN_SCHEDULED_TASK_INTERVAL_MS`).
const MIN_INTERVAL_MS: i64 = 60_000;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ScheduleMode {
    /// A fixed local time on some weekdays.
    AtTime,
    /// Every so many minutes.
    Every,
    /// When the task's webhook URL is called. mustr shows these but doesn't edit the trigger.
    Webhook,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum WorkspaceMode {
    NewWorktree,
    Checkout,
    ExistingWorktree,
}

/// The permission modes a task's agent can run in, with the names T3's app gives them.
pub const RUNTIME_MODES: [(RuntimeMode, &str); 4] = [
    (RuntimeMode::ApprovalRequired, "Supervised"),
    (RuntimeMode::AutoAcceptEdits, "Accept edits"),
    (RuntimeMode::Auto, "Auto"),
    (RuntimeMode::FullAccess, "Full access"),
];

/// What the form holds, besides its text fields, which it reads into `title` and the rest
/// before saving.
#[derive(Clone, Debug, PartialEq)]
pub struct Draft {
    /// The task being edited; `None` for a new one.
    pub editing: Option<ScheduledTask>,
    pub title: String,
    pub prompt: String,
    pub project_id: Option<String>,
    pub model: Option<ModelSelection>,
    pub workspace: WorkspaceMode,
    pub base_ref: String,
    pub existing_worktree: String,
    /// The thread each run messages; `None` starts a new thread every run.
    pub thread_id: Option<String>,
    pub runtime_mode: RuntimeMode,
    pub interaction_mode: ProviderInteractionMode,
    pub schedule: ScheduleMode,
    /// `HH:MM`, on the T3 machine's clock.
    pub time_of_day: String,
    pub weekdays: BTreeSet<i64>,
    pub interval_minutes: String,
    pub enabled: bool,
    /// Where an edited task runs, when this build doesn't know that kind of place (a newer
    /// T3's); it's saved back as it was.
    pub unknown_workspace: Option<serde_json::Value>,
}

impl Draft {
    /// A new task, with T3's defaults: weekdays at 09:00, in a new worktree from `main`, with
    /// full access.
    pub fn new(project_id: Option<String>, model: Option<ModelSelection>) -> Self {
        Self {
            editing: None,
            title: String::new(),
            prompt: String::new(),
            project_id,
            model,
            workspace: WorkspaceMode::NewWorktree,
            base_ref: "main".to_string(),
            existing_worktree: String::new(),
            thread_id: None,
            runtime_mode: RuntimeMode::FullAccess,
            interaction_mode: ProviderInteractionMode::Default,
            schedule: ScheduleMode::AtTime,
            time_of_day: "09:00".to_string(),
            weekdays: [1, 2, 3, 4, 5].into(),
            interval_minutes: "15".to_string(),
            enabled: true,
            unknown_workspace: None,
        }
    }

    pub fn from_task(task: &ScheduledTask) -> Self {
        let mut draft = Self::new(
            Some(task.project_id.clone()),
            Some(task.model_selection.clone()),
        );
        draft.title = task.title.clone();
        draft.prompt = task.prompt.clone();
        draft.thread_id = task.thread_id.clone();
        draft.runtime_mode = task.runtime_mode.clone();
        draft.interaction_mode = task.interaction_mode.clone();
        draft.enabled = task.enabled;
        match &task.workspace_strategy {
            OrchestrationV2ThreadLaunchWorkspaceStrategy::Root(_) => {
                draft.workspace = WorkspaceMode::Checkout;
            }
            OrchestrationV2ThreadLaunchWorkspaceStrategy::Worktree(worktree) => {
                draft.workspace = WorkspaceMode::NewWorktree;
                draft.base_ref = worktree.base_ref.clone();
            }
            OrchestrationV2ThreadLaunchWorkspaceStrategy::ExistingWorktree(existing) => {
                draft.workspace = WorkspaceMode::ExistingWorktree;
                draft.existing_worktree = existing.worktree_path.clone();
            }
            OrchestrationV2ThreadLaunchWorkspaceStrategy::Unrecognized(value) => {
                draft.unknown_workspace = Some(value.clone());
            }
        }
        match &task.schedule {
            ScheduledTaskSchedule::FixedTime(fixed) => {
                draft.schedule = ScheduleMode::AtTime;
                draft.time_of_day = fixed.time_of_day.clone();
                draft.weekdays = match fixed.weekdays.as_deref() {
                    Some(weekdays) if !weekdays.is_empty() => weekdays.iter().copied().collect(),
                    _ => (0..7).collect(),
                };
            }
            ScheduledTaskSchedule::Interval(interval) => {
                draft.schedule = ScheduleMode::Every;
                draft.interval_minutes = minutes_text((interval.every_ms as f64 / 60_000.).max(1.));
            }
            // A trigger mustr doesn't know is shown, not edited, like a webhook's.
            ScheduledTaskSchedule::Webhook(_) | ScheduledTaskSchedule::Unrecognized(_) => {
                draft.schedule = ScheduleMode::Webhook
            }
        }
        draft.editing = Some(task.clone());
        draft
    }

    /// The `scheduledTasks.upsert` this draft saves as, or what's missing.
    pub fn upsert_input(&self) -> Result<ScheduledTaskUpsertInput, String> {
        let title = self.title.trim();
        let prompt = self.prompt.trim();
        if title.is_empty() || prompt.is_empty() {
            return Err("Add a name and a prompt.".to_string());
        }
        let Some(project_id) = self.project_id.clone() else {
            return Err("Pick a project.".to_string());
        };
        let Some(model_selection) = self.model.clone() else {
            return Err("Pick an agent and a model.".to_string());
        };
        let schedule = self.schedule()?;
        let workspace_strategy = self.workspace_strategy()?;
        Ok(ScheduledTaskUpsertInput {
            command_id: None,
            created_by: None,
            creation_source: Some(OrchestrationV2CreationSource::Web),
            enabled: self.enabled,
            id: self.editing.as_ref().map(|task| task.id.clone()),
            // An edit fails if the task was deleted meanwhile, rather than bringing it back.
            require_existing: self.editing.as_ref().map(|_| true),
            interaction_mode: self.interaction_mode.clone(),
            model_selection,
            project_id,
            prompt: prompt.to_string(),
            runtime_mode: self.runtime_mode.clone(),
            schedule,
            thread_id: self.thread_id.clone(),
            title: title.to_string(),
            workspace_strategy,
        })
    }

    fn schedule(&self) -> Result<ScheduledTaskUpsertSchedule, String> {
        match self.schedule {
            ScheduleMode::AtTime => {
                let time_of_day = parse_time_of_day(&self.time_of_day)
                    .ok_or_else(|| "Enter the time as HH:MM, such as 09:00.".to_string())?;
                if self.weekdays.is_empty() {
                    return Err("Pick at least one day.".to_string());
                }
                let every_day = self.weekdays.len() == 7;
                Ok(ScheduledTaskUpsertSchedule::FixedTime(
                    ScheduledTaskUpsertScheduleFixedTime {
                        time_of_day,
                        type_: ScheduledTaskUpsertScheduleFixedTimeType::FixedTime,
                        weekdays: (!every_day).then(|| self.weekdays.iter().copied().collect()),
                    },
                ))
            }
            ScheduleMode::Every => {
                let minutes: f64 = self
                    .interval_minutes
                    .trim()
                    .parse()
                    .map_err(|_| "Enter the interval in minutes.".to_string())?;
                let every_ms = (minutes * 60_000.).round();
                if !every_ms.is_finite() || (every_ms as i64) < MIN_INTERVAL_MS {
                    return Err("Enter an interval of at least one minute.".to_string());
                }
                Ok(ScheduledTaskUpsertSchedule::Interval(
                    ScheduledTaskUpsertScheduleInterval {
                        every_ms: (every_ms as i64).into(),
                        type_: ScheduledTaskUpsertScheduleIntervalType::Interval,
                    },
                ))
            }
            // The trigger stays as it is: the same signature check (T3 keeps the stored secret
            // when none is sent), age limit, and, by leaving them out, dedupe and filters.
            ScheduleMode::Webhook => match self.editing.as_ref().map(|task| &task.schedule) {
                Some(ScheduledTaskSchedule::Webhook(webhook)) => Ok(
                    ScheduledTaskUpsertSchedule::Webhook(ScheduledTaskUpsertScheduleWebhook {
                        dedupe: None,
                        filters: None,
                        max_delivery_age_minutes: webhook.max_delivery_age_minutes,
                        signature: webhook.signature.as_ref().map(|signature| {
                            ScheduledTaskUpsertScheduleWebhookSignature {
                                encoding: ScheduledTaskUpsertScheduleWebhookSignatureEncoding::from(
                                    signature.encoding.as_str(),
                                ),
                                header: signature.header.clone(),
                                prefix: signature.prefix.clone(),
                                secret: None,
                                secret_ref: None,
                            }
                        }),
                        type_: ScheduledTaskUpsertScheduleWebhookType::Webhook,
                    }),
                ),
                _ => Err("Edit this task's trigger in T3 Code.".to_string()),
            },
        }
    }

    fn workspace_strategy(&self) -> Result<OrchestrationV2ThreadLaunchWorkspaceStrategy, String> {
        if let Some(value) = &self.unknown_workspace {
            return Ok(OrchestrationV2ThreadLaunchWorkspaceStrategy::Unrecognized(
                value.clone(),
            ));
        }
        let editing = self.editing.as_ref().map(|task| &task.workspace_strategy);
        Ok(match self.workspace {
            WorkspaceMode::Checkout => {
                let branch = match editing {
                    Some(OrchestrationV2ThreadLaunchWorkspaceStrategy::Root(root)) => {
                        root.branch.clone()
                    }
                    _ => None,
                };
                OrchestrationV2ThreadLaunchWorkspaceStrategy::Root(
                    OrchestrationV2ThreadLaunchWorkspaceStrategyRoot {
                        branch,
                        type_: OrchestrationV2ThreadLaunchWorkspaceStrategyRootType::Root,
                    },
                )
            }
            WorkspaceMode::NewWorktree => {
                let base_ref = match self.base_ref.trim() {
                    "" => "main".to_string(),
                    base_ref => base_ref.to_string(),
                };
                // T3's form starts new worktrees from the remote's copy of the branch.
                let (branch, start_from_origin) = match editing {
                    Some(OrchestrationV2ThreadLaunchWorkspaceStrategy::Worktree(worktree)) => {
                        (worktree.branch.clone(), worktree.start_from_origin)
                    }
                    _ => (None, Some(true)),
                };
                OrchestrationV2ThreadLaunchWorkspaceStrategy::Worktree(
                    OrchestrationV2ThreadLaunchWorkspaceStrategyWorktree {
                        base_ref,
                        branch,
                        start_from_origin,
                        type_: OrchestrationV2ThreadLaunchWorkspaceStrategyWorktreeType::Worktree,
                    },
                )
            }
            WorkspaceMode::ExistingWorktree => {
                let worktree_path = self.existing_worktree.trim().to_string();
                if worktree_path.is_empty() {
                    return Err("Enter the path of the checkout to run in.".to_string());
                }
                let branch = match editing {
                    Some(OrchestrationV2ThreadLaunchWorkspaceStrategy::ExistingWorktree(
                        existing,
                    )) if existing.worktree_path == worktree_path => existing.branch.clone(),
                    _ => None,
                };
                OrchestrationV2ThreadLaunchWorkspaceStrategy::ExistingWorktree(
                    OrchestrationV2ThreadLaunchWorkspaceStrategyExistingWorktree {
                        branch,
                        type_:
                            OrchestrationV2ThreadLaunchWorkspaceStrategyExistingWorktreeType::ExistingWorktree,
                        worktree_path,
                    },
                )
            }
        })
    }
}

fn minutes_text(minutes: f64) -> String {
    if minutes.fract() == 0. {
        format!("{minutes:.0}")
    } else {
        format!("{minutes}")
    }
}

/// `9:5` or `09:05` as `09:05`; `None` unless it's a time of day.
fn parse_time_of_day(text: &str) -> Option<String> {
    let (hours, minutes) = text.trim().split_once(':')?;
    let hours: u8 = hours.parse().ok()?;
    let minutes: u8 = minutes.parse().ok()?;
    (hours < 24 && minutes < 60).then(|| format!("{hours:02}:{minutes:02}"))
}

/// `Weekdays at 09:00`, `Every 15 min`, or `On webhook` (T3's `scheduleLabel`).
pub fn schedule_label(schedule: &ScheduledTaskSchedule) -> String {
    match schedule {
        ScheduledTaskSchedule::Webhook(_) => "On webhook".to_string(),
        ScheduledTaskSchedule::Unrecognized(_) => "On a trigger mustr doesn't know".to_string(),
        ScheduledTaskSchedule::Interval(interval) => {
            if interval.every_ms % 60_000 == 0 {
                format!("Every {} min", interval.every_ms / 60_000)
            } else {
                format!("Every {} sec", (interval.every_ms as f64 / 1000.).round())
            }
        }
        ScheduledTaskSchedule::FixedTime(fixed) => {
            let weekdays = fixed.weekdays.as_deref().unwrap_or_default();
            let days = if weekdays.is_empty() || weekdays.len() == 7 {
                "Daily".to_string()
            } else if weekdays.len() == 5 && weekdays.iter().all(|day| (1..=5).contains(day)) {
                "Weekdays".to_string()
            } else {
                let mut sorted = weekdays.to_vec();
                // Monday first, as the chips are.
                sorted.sort_by_key(|day| (day + 6) % 7);
                sorted
                    .iter()
                    .filter_map(|day| WEEKDAY_NAMES.get(usize::try_from(*day).ok()?))
                    .copied()
                    .collect::<Vec<_>>()
                    .join(", ")
            };
            format!("{days} at {}", fixed.time_of_day)
        }
    }
}

/// When a task runs next, as its row says it.
pub fn next_run_label(task: &ScheduledTask, now: OffsetDateTime) -> String {
    if !task.enabled {
        return "Paused".to_string();
    }
    if matches!(task.schedule, ScheduledTaskSchedule::Webhook(_)) {
        return "Listening".to_string();
    }
    match task.next_run_at.as_deref().and_then(|at| relative(at, now)) {
        Some(when) => format!("Next run {when}"),
        None => "Not scheduled".to_string(),
    }
}

/// `in 3h`, `in under a minute`, `5m ago`, or `2d ago`: how far `at` is from `now`.
pub fn relative(at: &str, now: OffsetDateTime) -> Option<String> {
    let at = OffsetDateTime::parse(at, &Rfc3339).ok()?;
    let seconds = (at - now).whole_seconds();
    let span = |seconds: i64| {
        let minutes = (seconds + 59) / 60;
        if minutes < 60 {
            format!("{minutes}m")
        } else if minutes < 60 * 24 {
            format!("{}h", (minutes + 30) / 60)
        } else {
            format!("{}d", (minutes + 60 * 12) / (60 * 24))
        }
    };
    Some(if seconds > 0 {
        if seconds < 120 {
            "in under a minute".to_string()
        } else {
            format!("in {}", span(seconds))
        }
    } else if -seconds < 60 {
        "just now".to_string()
    } else {
        format!("{} ago", span(-seconds - 59))
    })
}

/// A webhook URL with its secret token hidden: T3's URLs end in `/<task id>/<token>`.
pub fn mask_webhook_address(address: &str) -> String {
    match address.rsplit_once('/') {
        Some((base, token)) if !token.is_empty() => format!("{base}/••••••••"),
        _ => "••••••••".to_string(),
    }
}

/// What happened to one webhook delivery, as T3's delivery log says it
/// (`webhookDeliveryLabel` in T3's settings page).
pub fn delivery_label(outcome: &str, pending_window: bool) -> String {
    match outcome {
        "accepted" if pending_window => "Opened window".to_string(),
        "accepted" => "Ran".to_string(),
        "dispatch_failed" => "Run failed".to_string(),
        "duplicate" => "Duplicate".to_string(),
        "coalesced" => "Coalesced".to_string(),
        "filtered" => "Filtered".to_string(),
        "rejected_signature" => "Bad signature".to_string(),
        "disabled" => "Task paused".to_string(),
        "rate_limited" => "Rate limited".to_string(),
        "expired" => "Too old".to_string(),
        other => other.replace('_', " "),
    }
}

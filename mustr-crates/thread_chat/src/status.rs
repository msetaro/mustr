//! A thread's status, worked out once, the way T3's sidebar does it (`resolveThreadStatusPill`
//! in `components/Sidebar.logic.ts`), from the thread's record in T3's shell. The sidebar's
//! rows, the thread's header, its hover card and details, the Inbox and notifications all read
//! it here, so they never disagree: a thread whose last run completed while its agent still
//! waits on background work reads Waiting everywhere.

use gpui::{App, Hsla};
use mustr_ui::MustrIcon;
use mustr_ui::palette;
use t3_api::types::{OrchestrationV2ThreadShell, ProviderInteractionMode};
use time::{OffsetDateTime, format_description::well_known::Rfc3339};
use ui::{ActiveTheme as _, Icon, IconName};

/// A thread's status pill, as T3's sidebar picks it (`resolveThreadStatusPill`). A thread with
/// nothing to say has none.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ShellStatus {
    PendingApproval,
    AwaitingInput,
    Working,
    Connecting,
    /// Blocked on another thread or on background work.
    Waiting,
    /// The last run failed, and the user hasn't seen it.
    Failed,
    PlanReady,
    /// The last run finished, and the user hasn't seen it.
    Completed,
}

impl ShellStatus {
    pub fn from_shell(thread: &OrchestrationV2ThreadShell) -> Option<Self> {
        if let Some(request) = &thread.pending_runtime_request {
            let kind = crate::model::wire_string(&request.kind);
            return Some(if kind == "user_input" {
                Self::AwaitingInput
            } else {
                Self::PendingApproval
            });
        }
        let status = crate::model::wire_string(&thread.status);
        let blocked = thread.blocked_on_thread_wait == Some(true);
        match status.as_str() {
            "running" if !blocked => return Some(Self::Working),
            "waiting" => return Some(Self::Working),
            "preparing" | "starting" | "queued" => return Some(Self::Connecting),
            "running" => return Some(Self::Waiting),
            _ => {}
        }
        if background_work_holds_completion(thread) {
            return Some(Self::Waiting);
        }
        let completed_at = millis(thread.latest_run_completed_at.as_deref());
        let visited_at = millis(thread.last_visited_at.as_deref());
        if status == "failed" {
            let unseen = match (completed_at, visited_at) {
                (Some(completed), Some(visited)) => completed > visited,
                _ => true,
            };
            if unseen {
                return Some(Self::Failed);
            }
        }
        if thread.interaction_mode == ProviderInteractionMode::Plan
            && thread.has_actionable_proposed_plan
        {
            return Some(Self::PlanReady);
        }
        if has_unseen_completion(thread) {
            return Some(Self::Completed);
        }
        None
    }

    pub fn label(self) -> &'static str {
        match self {
            Self::PendingApproval => "Pending approval",
            Self::AwaitingInput => "Awaiting input",
            Self::Working => "Working",
            Self::Connecting => "Connecting",
            Self::Waiting => "Waiting",
            Self::Failed => "Failed",
            Self::PlanReady => "Plan ready",
            Self::Completed => "Completed",
        }
    }

    /// T3's icon for the status in a thread's row (`SidebarThreadRow.tsx`): a dashed ring while
    /// the agent works, a dashed ring around a dot while it waits.
    pub fn icon(self) -> Icon {
        match self {
            Self::Working | Self::Connecting => MustrIcon::CircleDashed.icon(),
            Self::Waiting => MustrIcon::CircleDotDashed.icon(),
            Self::AwaitingInput => Icon::new(IconName::CircleHelp),
            Self::PendingApproval => Icon::new(IconName::Warning),
            Self::Failed => Icon::new(IconName::XCircle),
            Self::PlanReady => Icon::new(IconName::ListTodo),
            Self::Completed => Icon::new(IconName::Check),
        }
    }

    /// Whether the status's icon turns, as T3's working ring does.
    pub fn spins(self) -> bool {
        matches!(self, Self::Working | Self::Connecting)
    }

    /// When the status began, for the time T3 shows next to it: the working run's start
    /// (`resolveThreadWorkingStartedAt`), or the end of the run the thread waits after.
    pub fn since(self, thread: &OrchestrationV2ThreadShell) -> Option<&str> {
        match self {
            Self::Working => thread
                .activity_run_started_at
                .as_deref()
                .or_else(|| {
                    thread
                        .latest_run_completed_at
                        .is_none()
                        .then_some(thread.latest_run_started_at.as_deref())
                        .flatten()
                })
                .or(thread.latest_run_requested_at.as_deref()),
            Self::Waiting => thread.latest_run_completed_at.as_deref(),
            _ => None,
        }
    }

    /// T3's color for the status: amber, indigo, sky, muted, red, violet, and emerald.
    pub fn color(self, cx: &App) -> Hsla {
        let palette = palette(cx);
        match self {
            Self::PendingApproval => palette.warning.hsla(),
            Self::AwaitingInput => palette.pending.hsla(),
            Self::Working | Self::Connecting => palette.accent.hsla(),
            Self::Waiting => palette.text_muted.hsla(),
            Self::Failed => cx.theme().status().error,
            Self::PlanReady => palette.agent.hsla(),
            Self::Completed => palette.ok.hsla(),
        }
    }
}

/// Whether the agent is waiting on a subagent or background task to finish
/// (`backgroundWorkHoldsCompletion` in T3's shared package). Commands and monitors count only
/// when the agent waits on them, since they're often dev servers.
pub(crate) fn background_work_holds_completion(thread: &OrchestrationV2ThreadShell) -> bool {
    thread
        .pending_background_tasks
        .iter()
        .flatten()
        .any(|task| {
            let task = serde_json::to_value(task).unwrap_or_default();
            task["awaited"] == serde_json::Value::Bool(true)
                || matches!(task["kind"].as_str(), Some("subagent" | "background_task"))
        })
}

/// Whether the thread's last run finished after the user last looked (`hasUnseenCompletion`).
/// A thread never opened in T3 has no visit, and doesn't count.
pub(crate) fn has_unseen_completion(thread: &OrchestrationV2ThreadShell) -> bool {
    match (
        millis(thread.latest_run_completed_at.as_deref()),
        millis(thread.last_visited_at.as_deref()),
    ) {
        (Some(completed), Some(visited)) => completed > visited,
        _ => false,
    }
}

/// Milliseconds since the epoch of one of T3's ISO 8601 timestamps.
pub(crate) fn millis(timestamp: Option<&str>) -> Option<i128> {
    let timestamp = OffsetDateTime::parse(timestamp?, &Rfc3339).ok()?;
    Some(timestamp.unix_timestamp_nanos() / 1_000_000)
}

/// A span of time, briefly, as T3's sidebar shows it: "4s", "35m", "3h", "3d", "2w".
pub fn short_duration(milliseconds: i128) -> String {
    let seconds = (milliseconds / 1000).max(0);
    if seconds < 60 {
        format!("{seconds}s")
    } else if seconds < 3600 {
        format!("{}m", seconds / 60)
    } else if seconds < 86_400 {
        format!("{}h", seconds / 3600)
    } else if seconds < 7 * 86_400 {
        format!("{}d", seconds / 86_400)
    } else {
        format!("{}w", seconds / (7 * 86_400))
    }
}

/// How long ago a timestamp was, as an idle row shows it (`RelativeTimeLabel`): "now" in the
/// first minute, then [`short_duration`].
pub fn relative_time(timestamp: &str, now: OffsetDateTime) -> Option<String> {
    let ago = now.unix_timestamp_nanos() / 1_000_000 - millis(Some(timestamp))?;
    Some(if ago < 60_000 {
        "now".to_string()
    } else {
        short_duration(ago)
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn shell(fields: serde_json::Value) -> OrchestrationV2ThreadShell {
        let mut thread = json!({
            "id": "thread-1", "projectId": "project-1", "title": "Audit Dial Hard Alert Noise",
            "status": "completed", "createdAt": "2026-10-27T10:00:00.000Z",
            "updatedAt": "2026-10-27T10:05:00.000Z", "createdBy": "user",
            "creationSource": "web", "interactionMode": "default", "runtimeMode": "full-access",
            "hasActionableProposedPlan": false, "itemCount": 4, "visibleItemCount": 4,
            "providerInstanceId": "claudeAgent",
            "modelSelection": { "instanceId": "claudeAgent", "model": "claude-opus-5-5" },
            "lineage": {
                "rootThreadId": "thread-1", "parentThreadId": null, "relationshipToParent": null
            },
            "activeProviderThreadId": null, "activeRunId": null, "archivedAt": null,
            "branch": null, "deletedAt": null, "forkedFrom": null, "latestUserMessageAt": null,
            "latestVisibleMessage": null, "pendingRuntimeRequest": null, "settledAt": null,
            "settledOverride": null, "worktreePath": null,
            "latestRunId": "run-1",
            "latestRunStartedAt": "2026-10-27T10:00:00.000Z",
            "latestRunCompletedAt": "2026-10-27T10:04:00.000Z",
            "lastVisitedAt": "2026-10-27T10:05:00.000Z"
        });
        if let (Some(thread), serde_json::Value::Object(fields)) = (thread.as_object_mut(), fields)
        {
            thread.extend(fields);
        }
        serde_json::from_value(thread).unwrap()
    }

    #[test]
    fn background_work_that_holds_completion_reads_waiting() {
        // The last run completed, but the agent still watches a pull request in the background.
        let waiting = shell(json!({
            "pendingBackgroundTasks": [{
                "taskId": "task-1", "kind": "background_task",
                "description": "Watching pull request #80"
            }]
        }));
        assert_eq!(
            ShellStatus::from_shell(&waiting),
            Some(ShellStatus::Waiting)
        );
        assert_eq!(
            ShellStatus::Waiting.since(&waiting),
            Some("2026-10-27T10:04:00.000Z")
        );
        // Without it, a completed thread the user has seen has nothing to say.
        assert_eq!(ShellStatus::from_shell(&shell(json!({}))), None);
    }

    #[test]
    fn statuses_follow_t3s_order() {
        // A request outranks everything, even a running agent.
        let asking = shell(json!({
            "status": "running",
            "pendingRuntimeRequest": {
                "id": "request-1", "kind": "command", "createdAt": "2026-10-27T10:06:00.000Z"
            }
        }));
        assert_eq!(
            ShellStatus::from_shell(&asking),
            Some(ShellStatus::PendingApproval)
        );
        let working = shell(json!({
            "status": "running", "latestRunCompletedAt": null,
            "activityRunStartedAt": "2026-10-27T10:06:00.000Z"
        }));
        assert_eq!(
            ShellStatus::from_shell(&working),
            Some(ShellStatus::Working)
        );
        assert_eq!(
            ShellStatus::Working.since(&working),
            Some("2026-10-27T10:06:00.000Z")
        );
        assert_eq!(
            ShellStatus::from_shell(&shell(json!({ "status": "starting" }))),
            Some(ShellStatus::Connecting)
        );
        // Running, but blocked on another thread: Waiting, ahead of background work.
        assert_eq!(
            ShellStatus::from_shell(&shell(json!({
                "status": "running", "blockedOnThreadWait": true
            }))),
            Some(ShellStatus::Waiting)
        );
        // A failure the user hasn't seen, then an unseen completion.
        assert_eq!(
            ShellStatus::from_shell(&shell(json!({
                "status": "failed", "lastVisitedAt": "2026-10-27T10:01:00.000Z"
            }))),
            Some(ShellStatus::Failed)
        );
        assert_eq!(
            ShellStatus::from_shell(&shell(json!({
                "lastVisitedAt": "2026-10-27T10:01:00.000Z"
            }))),
            Some(ShellStatus::Completed)
        );
    }

    #[test]
    fn times_read_briefly() {
        assert_eq!(short_duration(4_000), "4s");
        assert_eq!(short_duration(35 * 60_000), "35m");
        assert_eq!(short_duration(3 * 86_400_000), "3d");
        assert_eq!(short_duration(15 * 86_400_000), "2w");
        let now = OffsetDateTime::parse("2026-10-27T10:35:30.000Z", &Rfc3339).unwrap();
        assert_eq!(
            relative_time("2026-10-27T10:35:00.000Z", now).as_deref(),
            Some("now")
        );
        assert_eq!(
            relative_time("2026-10-27T10:00:00.000Z", now).as_deref(),
            Some("35m")
        );
    }
}

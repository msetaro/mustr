//! Agent threads as T3 reports them: the shell snapshot's threads (for the inbox and review
//! headers), and one thread's turn checkpoints (for its per-turn diffs).
//!
//! Only the fields mustr shows are decoded, each entry on its own, so one malformed or
//! newer-shaped thread doesn't hide the rest.

use crate::inbox::{ThreadStatus, decode_each};
use review::{AgentStatusTone, ReviewScope, ScopeChoice};
use serde::Deserialize;
use serde_json::Value;
use std::collections::{HashMap, HashSet};

/// The turns the scope picker offers, newest first.
const MAX_TURN_CHOICES: usize = 60;

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct AgentThread {
    pub id: String,
    pub title: String,
    pub project: Option<String>,
    pub branch: Option<String>,
    pub worktree_path: Option<String>,
    pub status: ThreadStatus,
    /// The agent and its model, for example "Claude · claude-opus-5-5".
    pub agent: Option<String>,
    pub latest_run_completed_at: Option<String>,
    pub updated_at: String,
    /// A subagent or delegated task of another thread, which works in its parent's worktree.
    pub is_subagent: bool,
}

impl AgentThread {
    pub fn status_tone(&self) -> AgentStatusTone {
        match self.status {
            ThreadStatus::NeedsInput => AgentStatusTone::NeedsInput,
            ThreadStatus::Working(_) => AgentStatusTone::Working,
            ThreadStatus::Failed => AgentStatusTone::Failed,
            ThreadStatus::Settled(_) => AgentStatusTone::Settled,
        }
    }

    /// Whether the latest run finished after the reviewer last opened this thread's review in
    /// mustr (`reviewed_at`). T3's own `lastVisitedAt` doesn't count: looking at a thread's chat
    /// in T3 isn't reviewing its changes. Timestamps are ISO 8601 UTC strings, so string order
    /// is time order.
    pub fn has_unseen_changes(&self, reviewed_at: Option<&str>) -> bool {
        let Some(completed) = self.latest_run_completed_at.as_deref() else {
            return false;
        };
        reviewed_at.is_none_or(|reviewed| completed > reviewed)
    }
}

/// What T3's provider instance ids mean to people.
fn agent_name(provider_instance_id: &str) -> String {
    match provider_instance_id {
        "claudeAgent" | "claude" => "Claude".to_string(),
        "codex" => "Codex".to_string(),
        "cursor" => "Cursor".to_string(),
        "opencode" => "OpenCode".to_string(),
        "grok" => "Grok".to_string(),
        "gemini" => "Gemini".to_string(),
        other => other.to_string(),
    }
}

/// Every thread of T3's shell snapshot (`GET /api/orchestration/shell`) that isn't deleted or
/// archived, most recently updated first.
pub fn agent_threads(snapshot: &Value) -> Vec<AgentThread> {
    #[derive(Deserialize)]
    #[serde(rename_all = "camelCase")]
    struct Thread {
        id: String,
        project_id: String,
        title: String,
        status: String,
        #[serde(default)]
        branch: Option<String>,
        #[serde(default)]
        worktree_path: Option<String>,
        updated_at: String,
        #[serde(default)]
        provider_instance_id: Option<String>,
        #[serde(default)]
        model_selection: Option<Value>,
        #[serde(default)]
        latest_run_completed_at: Option<String>,
        #[serde(default)]
        pending_runtime_request: Option<Value>,
        #[serde(default)]
        lineage: Option<Value>,
        #[serde(default)]
        deleted_at: Option<String>,
        #[serde(default)]
        archived_at: Option<String>,
    }

    #[derive(Deserialize)]
    struct Project {
        id: String,
        title: String,
    }

    let projects: HashMap<String, String> =
        decode_each::<Project>(snapshot.get("projects"), "project")
            .into_iter()
            .map(|project| (project.id, project.title))
            .collect();
    let mut threads = decode_each::<Thread>(snapshot.get("threads"), "thread");
    threads.retain(|thread| thread.deleted_at.is_none() && thread.archived_at.is_none());
    threads.sort_by(|left, right| right.updated_at.cmp(&left.updated_at));
    threads
        .into_iter()
        .map(|thread| {
            let model = thread
                .model_selection
                .as_ref()
                .and_then(|selection| selection.get("model"))
                .and_then(Value::as_str)
                .map(str::to_string);
            let agent = match (thread.provider_instance_id.as_deref(), model) {
                (Some(provider), Some(model)) => {
                    Some(format!("{} · {model}", agent_name(provider)))
                }
                (Some(provider), None) => Some(agent_name(provider)),
                (None, model) => model,
            };
            let is_subagent = thread
                .lineage
                .as_ref()
                .and_then(|lineage| lineage.get("parentThreadId"))
                .is_some_and(|parent| !parent.is_null());
            AgentThread {
                id: thread.id,
                title: if thread.title.trim().is_empty() {
                    "Untitled thread".to_string()
                } else {
                    thread.title
                },
                status: ThreadStatus::from_shell(
                    thread.status,
                    thread.pending_runtime_request.is_some(),
                ),
                project: projects.get(&thread.project_id).cloned(),
                branch: thread.branch,
                worktree_path: thread.worktree_path.filter(|path| !path.is_empty()),
                agent,
                latest_run_completed_at: thread.latest_run_completed_at,
                updated_at: thread.updated_at,
                is_subagent,
            }
        })
        .collect()
}

/// The threads that may have work to review: top-level threads whose agent is done (not
/// running or waiting on the reviewer), that work in a worktree, and whose latest run finished
/// after their review was last opened. Newest completion first. Whether each has a diff is
/// checked afterwards, since that takes git.
pub fn agent_work_candidates(
    threads: &[AgentThread],
    reviewed: &HashMap<String, String>,
    limit: usize,
) -> Vec<AgentThread> {
    let mut candidates = threads
        .iter()
        .filter(|thread| {
            !thread.is_subagent
                && matches!(thread.status, ThreadStatus::Settled(_))
                && thread.worktree_path.is_some()
                && thread.has_unseen_changes(reviewed.get(&thread.id).map(String::as_str))
        })
        .cloned()
        .collect::<Vec<_>>();
    candidates.sort_by(|left, right| {
        right
            .latest_run_completed_at
            .cmp(&left.latest_run_completed_at)
    });
    candidates.truncate(limit);
    candidates
}

/// One turn's checkpoint: the git commit T3 captured after the turn, and the one before it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct TurnCheckpoint {
    /// T3's `checkpointTurnCount` (the checkpoint's `appRunOrdinal`).
    pub turn: u32,
    /// How many files T3 recorded the turn changing. Older checkpoints record none.
    pub files: usize,
    pub to_ref: String,
    /// The checkpoint before it in the same scope, which `getTurnDiff` compares against.
    pub from_ref: Option<String>,
    /// The directory whose repository holds the refs.
    pub cwd: Option<String>,
}

/// A thread's turns that `orchestration.getTurnDiff` can show, oldest first: ready
/// checkpoints of runs that completed (`CheckpointDiffQuery.ts`). The bounded snapshot may
/// leave out older runs, so a checkpoint whose run isn't listed is kept.
pub fn turn_checkpoints(thread_snapshot: &Value) -> Vec<TurnCheckpoint> {
    #[derive(Deserialize)]
    #[serde(rename_all = "camelCase")]
    struct Checkpoint {
        #[serde(default)]
        app_run_ordinal: Option<u32>,
        #[serde(default)]
        run_id: Option<String>,
        #[serde(rename = "ref")]
        reference: String,
        scope_id: String,
        ordinal_within_scope: u32,
        status: String,
        #[serde(default)]
        files: Option<Value>,
    }

    #[derive(Deserialize)]
    struct Run {
        id: String,
        status: String,
    }

    #[derive(Deserialize)]
    struct Scope {
        id: String,
        #[serde(default)]
        cwd: Option<String>,
    }

    let projection = thread_snapshot.get("projection").unwrap_or(thread_snapshot);
    let checkpoints = decode_each::<Checkpoint>(projection.get("checkpoints"), "checkpoint");
    let runs = decode_each::<Run>(projection.get("runs"), "run");
    let incomplete_runs = runs
        .iter()
        .filter(|run| run.status != "completed")
        .map(|run| run.id.as_str())
        .collect::<HashSet<_>>();
    let scopes = decode_each::<Scope>(projection.get("checkpointScopes"), "checkpoint scope")
        .into_iter()
        .map(|scope| (scope.id, scope.cwd))
        .collect::<HashMap<_, _>>();
    let ready = |checkpoint: &&Checkpoint| checkpoint.status == "ready";
    let mut turns = checkpoints
        .iter()
        .filter(ready)
        .filter_map(|checkpoint| {
            let turn = checkpoint.app_run_ordinal?;
            let run_id = checkpoint.run_id.as_deref()?;
            if incomplete_runs.contains(run_id) {
                return None;
            }
            let from_ref = checkpoints
                .iter()
                .filter(ready)
                .find(|other| {
                    other.scope_id == checkpoint.scope_id
                        && other.ordinal_within_scope + 1 == checkpoint.ordinal_within_scope
                })
                .map(|other| other.reference.clone());
            Some(TurnCheckpoint {
                turn,
                files: checkpoint
                    .files
                    .as_ref()
                    .and_then(Value::as_array)
                    .map_or(0, Vec::len),
                to_ref: checkpoint.reference.clone(),
                from_ref,
                cwd: scopes.get(&checkpoint.scope_id).cloned().flatten(),
            })
        })
        .collect::<Vec<_>>();
    turns.sort_by_key(|turn| turn.turn);
    turns.dedup_by_key(|turn| turn.turn);
    turns
}

/// What the scope picker offers: the thread's changes, then its turns newest first. When T3
/// recorded which files turns changed, turns that changed nothing are left out.
pub fn scope_choices(turns: &[TurnCheckpoint]) -> Vec<ScopeChoice> {
    let knows_files = turns.iter().any(|turn| turn.files > 0);
    let mut choices = vec![ScopeChoice {
        scope: ReviewScope::Changes,
        detail: Some("branch vs. base".to_string()),
    }];
    choices.extend(
        turns
            .iter()
            .rev()
            .filter(|turn| !knows_files || turn.files > 0)
            .take(MAX_TURN_CHOICES)
            .map(|turn| ScopeChoice {
                scope: ReviewScope::Turn(turn.turn),
                detail: (turn.files > 0).then(|| match turn.files {
                    1 => "1 file".to_string(),
                    files => format!("{files} files"),
                }),
            }),
    );
    choices
}

#[cfg(test)]
mod tests {
    use super::*;
    use pretty_assertions::assert_eq;
    use serde_json::json;

    fn thread(id: &str, status: &str, completed: Option<&str>, visited: Option<&str>) -> Value {
        json!({
            "id": id, "projectId": "p1", "title": id, "status": status,
            "branch": format!("t3/{id}"), "worktreePath": format!("/worktrees/{id}"),
            "updatedAt": completed.unwrap_or("2026-10-20T00:00:00.000Z"),
            "providerInstanceId": "claudeAgent",
            "modelSelection": { "instanceId": "claudeAgent", "model": "claude-opus-5-5" },
            "latestRunCompletedAt": completed, "lastVisitedAt": visited,
            "pendingRuntimeRequest": null,
            "lineage": { "parentThreadId": null, "relationshipToParent": null },
            "deletedAt": null, "archivedAt": null
        })
    }

    fn snapshot() -> Value {
        let mut subagent = thread(
            "subagent",
            "completed",
            Some("2026-10-27T12:00:00.000Z"),
            None,
        );
        subagent["lineage"] =
            json!({ "parentThreadId": "fresh", "relationshipToParent": "subagent" });
        let mut no_worktree = thread("rooted", "idle", Some("2026-10-27T11:00:00.000Z"), None);
        no_worktree["worktreePath"] = json!(null);
        let mut waiting = thread("waiting", "running", Some("2026-10-27T10:30:00.000Z"), None);
        waiting["pendingRuntimeRequest"] = json!({ "id": "r1" });
        let mut archived = thread("archived", "idle", Some("2026-10-27T10:00:00.000Z"), None);
        archived["archivedAt"] = json!("2026-10-27T10:05:00.000Z");
        json!({
            "projects": [{ "id": "p1", "title": "hello" }],
            "threads": [
                thread("fresh", "completed", Some("2026-10-27T09:00:00.000Z"), Some("2026-10-27T08:00:00.000Z")),
                thread("seen", "idle", Some("2026-10-26T09:00:00.000Z"), Some("2026-10-26T10:00:00.000Z")),
                thread("never-visited", "interrupted", Some("2026-10-25T09:00:00.000Z"), None),
                thread("never-ran", "idle", None, None),
                thread("running", "running", Some("2026-10-27T09:30:00.000Z"), None),
                thread("failed", "failed", Some("2026-10-27T09:40:00.000Z"), None),
                subagent, no_worktree, waiting, archived,
                { "id": "malformed" }
            ]
        })
    }

    #[test]
    fn threads_carry_what_the_inbox_and_header_show() {
        let threads = agent_threads(&snapshot());
        let fresh = threads.iter().find(|thread| thread.id == "fresh").unwrap();
        assert_eq!(
            *fresh,
            AgentThread {
                id: "fresh".to_string(),
                title: "fresh".to_string(),
                project: Some("hello".to_string()),
                branch: Some("t3/fresh".to_string()),
                worktree_path: Some("/worktrees/fresh".to_string()),
                status: ThreadStatus::Settled("completed".to_string()),
                agent: Some("Claude · claude-opus-5-5".to_string()),
                latest_run_completed_at: Some("2026-10-27T09:00:00.000Z".to_string()),
                updated_at: "2026-10-27T09:00:00.000Z".to_string(),
                is_subagent: false,
            }
        );
        assert!(threads.iter().all(|thread| thread.id != "archived"));
        assert!(
            threads
                .iter()
                .find(|thread| thread.id == "subagent")
                .unwrap()
                .is_subagent
        );
        assert_eq!(threads[0].id, "subagent", "most recently updated first");
    }

    #[test]
    fn agent_work_is_settled_unreviewed_top_level_threads_with_a_worktree() {
        let threads = agent_threads(&snapshot());
        let ids = |reviewed: &HashMap<String, String>, limit| {
            agent_work_candidates(&threads, reviewed, limit)
                .into_iter()
                .map(|thread| thread.id)
                .collect::<Vec<_>>()
        };
        // Visiting a thread in T3 ("seen") doesn't review its changes.
        assert_eq!(ids(&HashMap::new(), 10), ["fresh", "seen", "never-visited"]);
        assert_eq!(ids(&HashMap::new(), 1), ["fresh"]);
        // Opening its review in mustr does, until the agent finishes another run.
        let reviewed =
            HashMap::from_iter([("fresh".to_string(), "2026-10-27T09:05:00.000Z".to_string())]);
        assert_eq!(ids(&reviewed, 10), ["seen", "never-visited"]);
        let stale =
            HashMap::from_iter([("fresh".to_string(), "2026-10-27T08:30:00.000Z".to_string())]);
        assert_eq!(ids(&stale, 10), ["fresh", "seen", "never-visited"]);
    }

    #[test]
    fn turns_come_from_ready_checkpoints_of_completed_runs() {
        let checkpoint = |ordinal: u32, turn: Option<u32>, status: &str, files: usize| {
            json!({
                "id": format!("c{ordinal}"), "appRunOrdinal": turn,
                "runId": turn.map(|turn| format!("run-{turn}")),
                "ref": format!("refs/t3/orchestration-v2/checkpoints/abc/ordinal/{ordinal}"),
                "scopeId": "root", "ordinalWithinScope": ordinal, "status": status,
                "files": (0..files).map(|index| json!({ "path": format!("f{index}"), "kind": "modified", "additions": 1, "deletions": 0 })).collect::<Vec<_>>(),
                "threadId": "t", "nodeId": "n", "capturedAt": "2026-10-27T00:00:00.000Z",
                "parentCheckpointId": null
            })
        };
        let snapshot = json!({
            "projection": {
                "checkpoints": [
                    checkpoint(0, None, "ready", 0),
                    checkpoint(1, Some(1), "ready", 2),
                    checkpoint(2, Some(2), "ready", 0),
                    checkpoint(3, Some(3), "missing", 1),
                    checkpoint(4, Some(4), "ready", 1),
                    checkpoint(5, Some(5), "ready", 3),
                ],
                "runs": [
                    { "id": "run-1", "status": "completed" },
                    { "id": "run-2", "status": "completed" },
                    { "id": "run-5", "status": "running" }
                ],
                "checkpointScopes": [{ "id": "root", "cwd": "/worktrees/fresh", "kind": "root_run" }]
            }
        });
        let turns = turn_checkpoints(&snapshot);
        assert_eq!(
            turns
                .iter()
                .map(|turn| (turn.turn, turn.files, turn.from_ref.is_some()))
                .collect::<Vec<_>>(),
            // Turn 3's checkpoint isn't ready; turn 4's run isn't listed, so it's kept (its
            // previous checkpoint isn't ready, so there's no ref to compare against); turn 5
            // is still running.
            vec![(1, 2, true), (2, 0, true), (4, 1, false)]
        );
        assert_eq!(
            turns[0].from_ref.as_deref(),
            Some("refs/t3/orchestration-v2/checkpoints/abc/ordinal/0")
        );
        assert_eq!(turns[0].cwd.as_deref(), Some("/worktrees/fresh"));

        let choices = scope_choices(&turns);
        assert_eq!(
            choices,
            vec![
                ScopeChoice {
                    scope: ReviewScope::Changes,
                    detail: Some("branch vs. base".to_string())
                },
                ScopeChoice {
                    scope: ReviewScope::Turn(4),
                    detail: Some("1 file".to_string())
                },
                ScopeChoice {
                    scope: ReviewScope::Turn(1),
                    detail: Some("2 files".to_string())
                },
            ]
        );
        // Without file counts, every turn is offered.
        let unknown = turns
            .iter()
            .map(|turn| TurnCheckpoint {
                files: 0,
                ..turn.clone()
            })
            .collect::<Vec<_>>();
        assert_eq!(scope_choices(&unknown).len(), 4);
        assert!(turn_checkpoints(&json!({})).is_empty());
    }
}

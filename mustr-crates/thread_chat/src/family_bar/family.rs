//! A thread's family as the family bar shows it, worked out from T3's shell and the thread's
//! projection the way T3's app does (`deriveThreadRelationshipGraph` in T3's
//! `packages/client-runtime/src/state/threadRelationships.ts`, and `ThreadRelationshipsPanel` in
//! `apps/web/src/components/chat/ThreadRelationshipsControl.tsx`).
//!
//! T3 0.1.1 has no `orchestration.getThreadFamily`, but its shell holds every active thread,
//! subagent children included, each with a `lineage` naming its parent. The thread's own
//! projection lists its subagents and delegated tasks, which name their child threads and say
//! how they can be stopped, and its workflows.

use crate::{commands, model::wire_string, shell::Shell};
use t3_api::{
    tagged::{
        OrchestrationV2AppThreadForkedFrom, OrchestrationV2Command,
        OrchestrationV2ThreadShellForkedFrom,
    },
    types::{
        OrchestrationV2AppThread, OrchestrationV2AppThreadLineageRelationshipToParent,
        OrchestrationV2CommandSubagentStop, OrchestrationV2CommandSubagentStopType,
        OrchestrationV2NativeRefStrength, OrchestrationV2PendingBackgroundTask,
        OrchestrationV2Subagent, OrchestrationV2SubagentOrigin, OrchestrationV2SubagentStatus,
        OrchestrationV2ThreadShell, OrchestrationV2WorkflowAgent,
        OrchestrationV2WorkflowAgentState,
    },
};

/// The only agent whose native subagents T3 0.1.1 can stop on their own (`dispatchSubagentStop`
/// in T3's orchestrator).
const CLAUDE_DRIVER: &str = "claudeAgent";

/// How a relative is doing.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Activity {
    /// Pending, preparing, starting, running, or waiting; holds which.
    Working(String),
    /// Waiting for an approval or an answer.
    NeedsInput,
    Failed,
    /// Anything else: completed, stopped, interrupted, idle. Holds which.
    Done(String),
}

impl Activity {
    fn from_wire(status: &str) -> Self {
        match status {
            "pending" | "preparing" | "queued" | "starting" | "running" | "waiting" => {
                Self::Working(status.to_string())
            }
            "failed" | "error" => Self::Failed,
            "rolled_back" => Self::Done("interrupted".to_string()),
            other => Self::Done(other.replace('_', " ")),
        }
    }

    fn from_shell(thread: &OrchestrationV2ThreadShell) -> Self {
        if thread.pending_runtime_request.is_some() {
            return Self::NeedsInput;
        }
        match &thread.activity_run_status {
            Some(status) => Self::Working(status.as_str().to_string()),
            None => Self::from_wire(&wire_string(&thread.status)),
        }
    }

    fn from_subagent(status: &OrchestrationV2SubagentStatus) -> Self {
        Self::from_wire(status.as_str())
    }

    pub fn label(&self) -> &str {
        match self {
            Self::Working(status) | Self::Done(status) => status,
            Self::NeedsInput => "needs input",
            Self::Failed => "failed",
        }
    }

    pub fn is_working(&self) -> bool {
        matches!(self, Self::Working(_) | Self::NeedsInput)
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Relationship {
    /// A subagent or delegated task the parent started.
    Subagent,
    /// A fork of the parent's conversation.
    Fork,
}

/// What stopping a running child sends.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum StopTarget {
    /// `subagent.stop` on the parent's run: Claude's own subagents, which T3 interrupts inside the
    /// parent's provider session.
    NativeSubagent {
        thread_id: String,
        run_id: String,
        subagent_id: String,
    },
    /// `thread.stop` on the child thread. T3 0.1.1's `subagent.stop` refuses delegated tasks,
    /// whose children run on their own, so T3's app stops the child thread instead
    /// (`interruptThreadTurn` in T3's client runtime).
    Thread { thread_id: String },
}

impl StopTarget {
    pub fn command(&self, command_id: String) -> OrchestrationV2Command {
        match self {
            Self::NativeSubagent {
                thread_id,
                run_id,
                subagent_id,
            } => stop_subagent(thread_id, run_id, subagent_id, command_id),
            Self::Thread { thread_id } => commands::stop(thread_id, command_id),
        }
    }

    /// Identifies the stop while it's in flight.
    pub fn key(&self) -> &str {
        match self {
            Self::NativeSubagent { subagent_id, .. } => subagent_id,
            Self::Thread { thread_id } => thread_id,
        }
    }
}

/// Stops one of the run's native subagents (`subagent.stop`).
pub fn stop_subagent(
    thread_id: &str,
    run_id: &str,
    subagent_id: &str,
    command_id: String,
) -> OrchestrationV2Command {
    OrchestrationV2Command::SubagentStop(OrchestrationV2CommandSubagentStop {
        command_id,
        run_id: run_id.to_string(),
        subagent_id: subagent_id.to_string(),
        thread_id: thread_id.to_string(),
        type_: OrchestrationV2CommandSubagentStopType::SubagentStop,
    })
}

/// The parent, or a child thread.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Relative {
    pub thread_id: String,
    pub title: String,
    pub relationship: Relationship,
    pub activity: Activity,
    /// The agent it runs on (T3's provider instance), and its model.
    pub provider: Option<String>,
    pub model: Option<String>,
    /// Whether the shell has the thread, so it can be opened.
    pub available: bool,
    pub stop: Option<StopTarget>,
    created_at: String,
}

/// A running subagent with no thread of its own, such as one of Claude's in-process subagents.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SubagentTask {
    pub id: String,
    pub title: String,
    pub activity: Activity,
    pub provider: Option<String>,
    pub model: Option<String>,
    pub stop: Option<StopTarget>,
}

/// One of a workflow's agents.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct WorkflowMember {
    pub label: String,
    pub activity: Activity,
    pub thread_id: Option<String>,
    pub available: bool,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct WorkflowPhase {
    /// `None` for agents outside any phase.
    pub title: Option<String>,
    pub members: Vec<WorkflowMember>,
}

impl WorkflowPhase {
    pub fn settled(&self) -> usize {
        self.members
            .iter()
            .filter(|member| !member.activity.is_working())
            .count()
    }
}

/// A workflow the thread runs: a script that coordinates agents in phases (T3's
/// `deriveWorkflowGroups`).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Workflow {
    /// The coordinator's subagent id, which `orchestration.stopWorkflow` takes.
    pub subagent_id: String,
    pub name: String,
    pub activity: Activity,
    /// The coordinator's thread, when it has one.
    pub thread_id: Option<String>,
    pub available: bool,
    pub phases: Vec<WorkflowPhase>,
    /// For `orchestration.getWorkflowScript`.
    pub script_path: Option<String>,
    /// T3 stops only Claude's running workflows (`canStopWorkflow` in T3's app).
    pub can_stop: bool,
}

impl Workflow {
    pub fn members(&self) -> impl Iterator<Item = &WorkflowMember> {
        self.phases.iter().flat_map(|phase| &phase.members)
    }

    /// `3 agents · 1 working · 2 done`, as T3's `workflowOpeningHeadline` counts them.
    pub fn summary(&self) -> String {
        let mut counts = [("working", 0), ("done", 0), ("failed", 0), ("stopped", 0)];
        let mut total = 0;
        for member in self.members() {
            total += 1;
            let key = match &member.activity {
                Activity::Working(_) | Activity::NeedsInput => 0,
                Activity::Done(status) if status == "completed" => 1,
                Activity::Failed => 2,
                Activity::Done(_) => 3,
            };
            counts[key].1 += 1;
        }
        if total == 0 {
            return "no agents yet".to_string();
        }
        let mut parts = vec![format!(
            "{total} {}",
            if total == 1 { "agent" } else { "agents" }
        )];
        parts.extend(
            counts
                .iter()
                .filter(|(_, count)| *count > 0)
                .map(|(label, count)| format!("{count} {label}")),
        );
        parts.join(" · ")
    }
}

/// Background work the thread's agent left running: subagents, commands, and monitors
/// (`presentPendingBackgroundWork` in T3's client runtime).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct BackgroundWork {
    pub title: String,
    /// Subagents among it that have a thread, as `(label, thread id)`.
    pub threads: Vec<(String, String)>,
}

/// Everything the family bar shows for one thread.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Family {
    pub parent: Option<Relative>,
    /// Running ones first, then newest first.
    pub children: Vec<Relative>,
    pub tasks: Vec<SubagentTask>,
    pub workflows: Vec<Workflow>,
    pub background: Option<BackgroundWork>,
    /// The agent is waiting for another thread (T3's `wait_for_threads`).
    pub waiting_on_thread: bool,
}

impl Family {
    pub fn derive(
        thread: &OrchestrationV2AppThread,
        shell: Option<&Shell>,
        subagents: &[OrchestrationV2Subagent],
    ) -> Self {
        let shell_thread = |id: &str| {
            shell
                .and_then(|shell| shell.threads.get(id))
                .filter(|thread| thread.deleted_at.is_none())
        };
        let own_shell = shell_thread(&thread.id);

        let parent = parent_id(thread).map(|parent_id| {
            let relationship = relationship(thread.lineage.relationship_to_parent.as_ref());
            match shell_thread(&parent_id) {
                Some(parent) => relative(parent, relationship, None),
                None => Relative {
                    thread_id: parent_id,
                    title: "Parent thread".to_string(),
                    relationship,
                    activity: Activity::Done("unavailable".to_string()),
                    provider: None,
                    model: None,
                    available: false,
                    stop: None,
                    created_at: String::new(),
                },
            }
        });

        let workflows: Vec<Workflow> = subagents
            .iter()
            .filter(|subagent| subagent.workflow.is_some())
            .map(|subagent| workflow(subagent, &shell_thread))
            .collect();
        // A workflow's coordinator and agents are listed under the workflow.
        let workflow_threads = workflows
            .iter()
            .flat_map(|workflow| {
                workflow.thread_id.as_deref().into_iter().chain(
                    workflow
                        .members()
                        .filter_map(|member| member.thread_id.as_deref()),
                )
            })
            .collect::<Vec<_>>();

        // Children the shell knows of, then children the projection names that it doesn't.
        let mut children: Vec<Relative> = shell
            .into_iter()
            .flat_map(|shell| shell.threads.values())
            .filter(|child| child.deleted_at.is_none() && child.id != thread.id)
            .filter(|child| parent_id_of_shell(child).as_deref() == Some(thread.id.as_str()))
            .filter(|child| !workflow_threads.contains(&child.id.as_str()))
            .map(|child| {
                let subagent = subagents
                    .iter()
                    .find(|subagent| subagent.child_thread_id.as_deref() == Some(&child.id));
                let relationship = if subagent.is_some() {
                    Relationship::Subagent
                } else {
                    relationship(child.lineage.relationship_to_parent.as_ref())
                };
                relative(child, relationship, subagent)
            })
            .collect();
        for subagent in subagents {
            let Some(child_id) = subagent.child_thread_id.as_deref() else {
                continue;
            };
            if subagent.workflow.is_some()
                || children.iter().any(|child| child.thread_id == child_id)
            {
                continue;
            }
            let activity = Activity::from_subagent(&subagent.status);
            children.push(Relative {
                thread_id: child_id.to_string(),
                title: subagent_title(subagent),
                relationship: Relationship::Subagent,
                stop: stop_target(subagent, &activity),
                activity,
                provider: Some(subagent.provider_instance_id.clone()),
                model: subagent.model.clone(),
                available: false,
                created_at: subagent
                    .started_at
                    .clone()
                    .unwrap_or_else(|| subagent.updated_at.clone()),
            });
        }
        // Running ones first, then newest first; ISO timestamps sort as text.
        children.sort_by(|left, right| {
            right
                .activity
                .is_working()
                .cmp(&left.activity.is_working())
                .then_with(|| right.created_at.cmp(&left.created_at))
                .then_with(|| left.thread_id.cmp(&right.thread_id))
        });

        let tasks = subagents
            .iter()
            .filter(|subagent| subagent.child_thread_id.is_none() && subagent.workflow.is_none())
            .filter_map(|subagent| {
                let activity = Activity::from_subagent(&subagent.status);
                activity.is_working().then(|| SubagentTask {
                    id: subagent.id.clone(),
                    title: subagent_title(subagent),
                    stop: stop_target(subagent, &activity),
                    activity,
                    provider: Some(subagent.provider_instance_id.clone()),
                    model: subagent.model.clone(),
                })
            })
            .collect();

        Self {
            parent,
            children,
            tasks,
            workflows,
            background: own_shell
                .and_then(|shell| shell.pending_background_tasks.as_deref())
                .and_then(background_work),
            waiting_on_thread: own_shell
                .and_then(|shell| shell.blocked_on_thread_wait)
                .unwrap_or(false),
        }
    }

    pub fn is_empty(&self) -> bool {
        self.parent.is_none()
            && self.children.is_empty()
            && self.tasks.is_empty()
            && self.workflows.is_empty()
            && self.background.is_none()
            && !self.waiting_on_thread
    }

    /// Children, tasks, and workflows still working.
    pub fn working_count(&self) -> usize {
        self.children
            .iter()
            .filter(|child| child.activity.is_working())
            .count()
            + self.tasks.len()
            + self
                .workflows
                .iter()
                .filter(|workflow| workflow.activity.is_working())
                .count()
    }
}

/// The thread this one came from: the run it was forked from, or its lineage's parent, as T3's
/// relationship graph has it.
fn parent_id(thread: &OrchestrationV2AppThread) -> Option<String> {
    match &thread.forked_from {
        Some(OrchestrationV2AppThreadForkedFrom::Run(run)) => Some(run.thread_id.clone()),
        _ => thread.lineage.parent_thread_id.clone(),
    }
}

fn parent_id_of_shell(thread: &OrchestrationV2ThreadShell) -> Option<String> {
    match &thread.forked_from {
        Some(OrchestrationV2ThreadShellForkedFrom::Run(run)) => Some(run.thread_id.clone()),
        _ => thread.lineage.parent_thread_id.clone(),
    }
}

fn relationship(
    relationship: Option<&OrchestrationV2AppThreadLineageRelationshipToParent>,
) -> Relationship {
    match relationship {
        Some(OrchestrationV2AppThreadLineageRelationshipToParent::Subagent) => {
            Relationship::Subagent
        }
        _ => Relationship::Fork,
    }
}

fn relative(
    thread: &OrchestrationV2ThreadShell,
    relationship: Relationship,
    subagent: Option<&OrchestrationV2Subagent>,
) -> Relative {
    // A child that ran on its own says best how it's doing; one that never ran (such as a
    // mirror of Claude's own subagent) goes by its subagent record (T3's relationship graph).
    let activity = match subagent {
        Some(subagent)
            if thread.activity_run_status.is_none()
                && thread.pending_runtime_request.is_none()
                && (thread.latest_run_id.is_none()
                    || Activity::from_subagent(&subagent.status).is_working()) =>
        {
            Activity::from_subagent(&subagent.status)
        }
        _ => Activity::from_shell(thread),
    };
    let stop = match subagent {
        Some(subagent) => stop_target(subagent, &activity),
        // A subagent child T3 has no record of still runs as a thread of its own.
        None if relationship == Relationship::Subagent && activity.is_working() => {
            Some(StopTarget::Thread {
                thread_id: thread.id.clone(),
            })
        }
        None => None,
    };
    Relative {
        thread_id: thread.id.clone(),
        title: display_title(&thread.title, relationship == Relationship::Subagent)
            .or_else(|| subagent.map(subagent_title))
            .unwrap_or_else(|| "Untitled thread".to_string()),
        relationship,
        activity,
        provider: Some(thread.provider_instance_id.clone()),
        model: thread
            .model_selection
            .model
            .as_str()
            .filter(|model| !model.is_empty())
            .map(str::to_string),
        available: true,
        stop,
        created_at: thread.created_at.clone(),
    }
}

/// How a running subagent can be stopped, as T3's app decides (`canStop` in
/// `ThreadRelationshipsControl.tsx`): T3's own delegated tasks always, and Claude's subagents
/// when T3 knows the native task to interrupt.
fn stop_target(subagent: &OrchestrationV2Subagent, activity: &Activity) -> Option<StopTarget> {
    if !activity.is_working() || subagent.started_at.is_none() {
        return None;
    }
    match subagent.origin {
        OrchestrationV2SubagentOrigin::AppOwned => subagent
            .child_thread_id
            .clone()
            .map(|thread_id| StopTarget::Thread { thread_id }),
        OrchestrationV2SubagentOrigin::ProviderNative => {
            let native_task = subagent.native_task_ref.as_ref().filter(|native| {
                native.strength == OrchestrationV2NativeRefStrength::Strong
                    && native.native_id.is_some()
            });
            let active = Activity::from_subagent(&subagent.status).is_working();
            match (&subagent.run_id, native_task) {
                (Some(run_id), Some(_)) if subagent.driver == CLAUDE_DRIVER && active => {
                    Some(StopTarget::NativeSubagent {
                        thread_id: subagent.thread_id.clone(),
                        run_id: run_id.clone(),
                        subagent_id: subagent.id.clone(),
                    })
                }
                _ => None,
            }
        }
        OrchestrationV2SubagentOrigin::Unrecognized(_) => None,
    }
}

fn workflow<'a>(
    subagent: &OrchestrationV2Subagent,
    shell_thread: &impl Fn(&str) -> Option<&'a OrchestrationV2ThreadShell>,
) -> Workflow {
    let activity = Activity::from_subagent(&subagent.status);
    let settled = !activity.is_working();
    let Some(workflow) = &subagent.workflow else {
        unreachable!("only subagents with a workflow are workflows");
    };
    let member = |agent: &OrchestrationV2WorkflowAgent| {
        // Agents the coordinator left queued or running end with it (T3's
        // `workflowMembersToRuntime`).
        let member_activity = match &agent.state {
            OrchestrationV2WorkflowAgentState::Queued
            | OrchestrationV2WorkflowAgentState::Running
                if settled =>
            {
                activity.clone()
            }
            OrchestrationV2WorkflowAgentState::Queued => Activity::Working("pending".to_string()),
            state => Activity::from_wire(state.as_str()),
        };
        WorkflowMember {
            label: agent.label.clone(),
            activity: member_activity,
            available: agent
                .child_thread_id
                .as_deref()
                .is_some_and(|id| shell_thread(id).is_some()),
            thread_id: agent.child_thread_id.clone(),
        }
    };
    // The declared phases, plus any an agent names before the plan admits it.
    let mut phase_titles: Vec<(i64, String)> = Vec::new();
    for agent in &workflow.agents {
        if let Some(index) = agent.phase_index
            && !phase_titles.iter().any(|(known, _)| *known == index)
        {
            let title = agent
                .phase_title
                .clone()
                .unwrap_or_else(|| format!("Phase {}", index + 1));
            phase_titles.push((index, title));
        }
    }
    for phase in &workflow.phases {
        match phase_titles
            .iter_mut()
            .find(|(index, _)| *index == phase.index)
        {
            Some(known) => known.1 = phase.title.clone(),
            None => phase_titles.push((phase.index, phase.title.clone())),
        }
    }
    phase_titles.sort_by_key(|(index, _)| *index);
    let mut agents = workflow.agents.iter().collect::<Vec<_>>();
    agents.sort_by_key(|agent| agent.index);
    let mut phases: Vec<WorkflowPhase> = phase_titles
        .into_iter()
        .map(|(index, title)| WorkflowPhase {
            title: Some(title),
            members: agents
                .iter()
                .filter(|agent| agent.phase_index == Some(index))
                .map(|agent| member(agent))
                .collect(),
        })
        .filter(|phase| !phase.members.is_empty())
        .collect();
    let unphased = agents
        .iter()
        .filter(|agent| agent.phase_index.is_none())
        .map(|agent| member(agent))
        .collect::<Vec<_>>();
    if !unphased.is_empty() {
        phases.push(WorkflowPhase {
            title: None,
            members: unphased,
        });
    }
    let thread_id = subagent.child_thread_id.clone();
    Workflow {
        subagent_id: subagent.id.clone(),
        name: workflow
            .name
            .clone()
            .filter(|name| !name.trim().is_empty())
            .unwrap_or_else(|| subagent_title(subagent)),
        can_stop: activity == Activity::Working("running".to_string())
            && subagent.origin == OrchestrationV2SubagentOrigin::ProviderNative
            && subagent.driver == CLAUDE_DRIVER,
        activity,
        available: thread_id
            .as_deref()
            .is_some_and(|id| shell_thread(id).is_some()),
        thread_id,
        phases,
        script_path: workflow
            .run_handles
            .as_ref()
            .and_then(|handles| handles.script_path.clone()),
    }
}

fn background_work(tasks: &[OrchestrationV2PendingBackgroundTask]) -> Option<BackgroundWork> {
    const KINDS: [(&str, &str, &str); 4] = [
        ("subagent", "subagent", "subagents"),
        ("command", "command", "commands"),
        ("monitor", "monitor", "monitors"),
        ("background_task", "background task", "background tasks"),
    ];
    if tasks.is_empty() {
        return None;
    }
    let mut items: Vec<(usize, String, Option<String>)> = tasks
        .iter()
        .map(|task| {
            let (kind, description, child_thread_id) = match task {
                OrchestrationV2PendingBackgroundTask::Variant0 {
                    description,
                    child_thread_id,
                    ..
                } => ("subagent", description, child_thread_id.clone()),
                OrchestrationV2PendingBackgroundTask::Variant1 { description, .. } => {
                    ("command", description, None)
                }
                OrchestrationV2PendingBackgroundTask::Variant2 { description, .. } => {
                    ("monitor", description, None)
                }
                OrchestrationV2PendingBackgroundTask::Variant3 { description, .. }
                | OrchestrationV2PendingBackgroundTask::Variant4 { description, .. } => {
                    ("background_task", description, None)
                }
            };
            let order = KINDS
                .iter()
                .position(|(known, ..)| *known == kind)
                .unwrap_or(KINDS.len() - 1);
            let label = description
                .as_deref()
                .map(str::trim)
                .filter(|description| !description.is_empty())
                .map(|description| {
                    if kind == "subagent" {
                        format_subagent_title(description)
                    } else {
                        description.to_string()
                    }
                })
                .unwrap_or_else(|| KINDS[order].1.to_string());
            (order, label, child_thread_id)
        })
        .collect();
    items.sort_by_key(|(order, ..)| *order);
    let title = if let [(order, label, _)] = items.as_slice() {
        let noun = KINDS[*order].1;
        if label == noun {
            format!("Running a {noun}")
        } else {
            format!("Running: {label}")
        }
    } else {
        let groups = KINDS
            .iter()
            .enumerate()
            .filter_map(|(order, (_, singular, plural))| {
                let count = items.iter().filter(|(kind, ..)| *kind == order).count();
                (count > 0)
                    .then(|| format!("{count} {}", if count == 1 { singular } else { plural }))
            })
            .collect::<Vec<_>>();
        format!("Running {}", join_with_and(&groups))
    };
    Some(BackgroundWork {
        title,
        threads: items
            .into_iter()
            .filter_map(|(_, label, thread_id)| Some((label, thread_id?)))
            .collect(),
    })
}

fn join_with_and(parts: &[String]) -> String {
    match parts {
        [] => String::new(),
        [only] => only.clone(),
        [rest @ .., last] => format!("{} and {last}", rest.join(", ")),
    }
}

fn subagent_title(subagent: &OrchestrationV2Subagent) -> String {
    if let Some(title) = display_title(subagent.title.as_deref().unwrap_or_default(), true) {
        return title;
    }
    let prompt = subagent.prompt.trim();
    if prompt.chars().count() > 80 {
        format!("{}…", prompt.chars().take(77).collect::<String>())
    } else if prompt.is_empty() {
        "Subagent".to_string()
    } else {
        prompt.to_string()
    }
}

fn display_title(title: &str, is_subagent: bool) -> Option<String> {
    let title = title.trim();
    if title.is_empty() {
        return None;
    }
    Some(if is_subagent {
        format_subagent_title(title)
    } else {
        title.to_string()
    })
}

/// Drops the `Subagent: ` prefix, and names a Codex agent path such as
/// `/root/review_agent` by its last part (T3's `formatSubagentDisplayTitle`).
fn format_subagent_title(title: &str) -> String {
    let title = title
        .get(..9)
        .filter(|prefix| prefix.eq_ignore_ascii_case("subagent:"))
        .map_or(title, |_| title[9..].trim_start());
    let Some(path) = title.strip_prefix("/root/") else {
        return title.to_string();
    };
    let name = path
        .trim_end_matches('/')
        .rsplit('/')
        .next()
        .unwrap_or_default()
        .replace('_', " ");
    let name = name.split_whitespace().map(capitalize).collect::<Vec<_>>();
    if name.is_empty() {
        title.to_string()
    } else {
        name.join(" ")
    }
}

fn capitalize(word: &str) -> String {
    let mut chars = word.chars();
    match chars.next() {
        Some(first) => first.to_uppercase().chain(chars).collect(),
        None => String::new(),
    }
}

/// A provider instance's name, for T3's built-in agents; other instances go by their id.
pub fn provider_name(instance_id: &str) -> String {
    match instance_id {
        "claudeAgent" => "Claude".to_string(),
        "codex" => "Codex".to_string(),
        "cursor" => "Cursor".to_string(),
        "opencode" => "OpenCode".to_string(),
        other => other.to_string(),
    }
}

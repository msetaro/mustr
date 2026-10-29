//! The form of a draft thread: where the agent works. That's a project, or no project (T3's
//! scratch folder), and in the project's checkout, a new worktree from a base branch, or a
//! worktree that already exists. How the agent runs (agent, model, options, permission mode,
//! plan mode) is picked in the composer's settings bar (`settings_bar.rs`) and kept here until
//! the thread launches.
//!
//! The projects come from T3's shell, the agents and models from T3's provider catalog
//! (`server.getConfig`), the defaults from T3's settings (`server.getSettings`), and the
//! branches and worktrees from `vcs.listRefs`.
//!
//! As in T3, `mod+shift+x` opens the menu of where the agent works ([`ToggleWorkspacePicker`]),
//! and `mod+shift+l` picks the worktree the project's latest thread used
//! ([`UsePreviousWorktree`], T3's `resolvePreviousWorktreeSeed`).

pub use crate::thread_settings::{AgentChoice, agent_choices};

use crate::{
    actions::NewThreadInWorktree,
    backend::{Access, ThreadBackend, ThreadBackendExt as _},
    commands::{NewThread, Workspace as ThreadWorkspace},
    shell::ThreadShells,
    thread_settings::{
        self, ModelChoice, ModelOption, RefChoice, ThreadDefaults, compatible_runtime_mode,
        launch_options, model_selection, parse_options, ref_choices, resolve_options,
        selection_instance_id, selection_model, selections_from,
    },
    thread_view::ThreadView,
};
use collections::HashMap;
use gpui::{
    Anchor, App, AppContext as _, Context, DismissEvent, Entity, EventEmitter, FocusHandle,
    Focusable as _, FontWeight, IntoElement, ParentElement as _, Render, SharedString, Styled as _,
    Subscription, Task, WeakEntity, Window, actions, anchored, deferred, div, px, rems,
};
use mustr_ui::{Card, SegmentedControl, tokens::scaled};
use picker::{Picker, PickerDelegate};
use std::sync::Arc;
use t3_api::{
    rpc,
    types::{
        OrchestrationProjectShell, ProjectsEnsureScratchPayload, ProviderInteractionMode,
        ProviderOptionSelection, ProviderOptionSelectionValue, RuntimeMode, ServerGetConfigPayload,
        ServerGetSettingsPayload, ServerSettings, ThreadEnvMode, VcsListRefsInput,
        VcsListRefsInputRefKind,
    },
};
use ui::{
    Button, ButtonStyle, Color, ContextMenu, ContextMenuEntry, DropdownMenu, DropdownStyle, Icon,
    IconButton, IconName, IconPosition, IconSize, Label, LabelCommon as _, LabelSize, ListItem,
    ListItemSpacing, PopoverMenu, PopoverMenuHandle, Switch, ToggleState, Tooltip, prelude::*,
};
use workspace::Workspace;

actions!(
    thread_chat,
    [
        /// Opens the menu of where a new thread's agent works: a new worktree, an existing one,
        /// the project's checkout, or the previous worktree.
        ToggleWorkspacePicker,
        /// Makes a new thread work in the worktree its project's latest thread used.
        UsePreviousWorktree,
    ]
);

pub enum NewThreadFormEvent {
    Changed,
}

/// The worktree a new thread in `project_id` would reuse (`resolvePreviousWorktreeSeed`): the
/// one of the project's most recently updated thread that has one, other than `current`.
pub(crate) fn previous_worktree<'a>(
    threads: impl IntoIterator<Item = &'a t3_api::types::OrchestrationV2ThreadShell>,
    project_id: &str,
    current: Option<&str>,
) -> Option<(String, Option<String>)> {
    threads
        .into_iter()
        .filter(|thread| {
            thread.project_id == project_id
                && thread.archived_at.is_none()
                && thread.deleted_at.is_none()
        })
        .filter_map(|thread| {
            let path = thread
                .worktree_path
                .as_deref()
                .filter(|path| !path.trim().is_empty() && Some(*path) != current)?;
            let updated = time::OffsetDateTime::parse(
                &thread.updated_at,
                &time::format_description::well_known::Rfc3339,
            )
            .ok()?;
            Some((updated, path, thread.branch.clone()))
        })
        .max_by_key(|(updated, _, _)| *updated)
        .map(|(_, path, branch)| (path.to_string(), branch))
}

/// T3's title for its scratch project (`ensureScratchProject` in T3's
/// `ManagedProjectFolders.ts`). The form offers it as "No project", ahead of the others.
pub(crate) const SCRATCH_PROJECT_TITLE: &str = "No project";

/// How many refs one `vcs.listRefs` page holds, as in T3's branch picker.
const REF_PAGE_SIZE: i64 = 100;

enum Loadable<T> {
    Loading,
    Failed(SharedString),
    Loaded(T),
}

impl<T> Loadable<T> {
    fn loaded(&self) -> Option<&T> {
        match self {
            Self::Loaded(value) => Some(value),
            _ => None,
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
enum ProjectPick {
    Project(String),
    /// No project: the thread runs in a folder of its own under T3's scratch directory.
    Scratch,
}

/// T3's scratch project, which `projects.ensureScratch` finds or creates.
enum Scratch {
    Idle,
    Opening,
    Ready(String),
    Failed(SharedString),
}

/// Where the agent works.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum WorkspaceKind {
    NewWorktree,
    ExistingWorktree,
    Checkout,
}

struct RefList {
    refs: Vec<RefChoice>,
    is_repo: bool,
    total: i64,
}

pub struct NewThreadForm {
    backend: Arc<dyn ThreadBackend>,
    projects: Loadable<Vec<OrchestrationProjectShell>>,
    agents: Loadable<Vec<AgentChoice>>,
    settings: Loadable<ServerSettings>,
    project: Option<ProjectPick>,
    /// The project to pick once the projects load, from [`NewThreadInWorktree`].
    pending_project: Option<String>,
    scratch: Scratch,
    /// The agent and model picked, by instance id and slug.
    model: Option<(String, String)>,
    /// The model options picked, or `None` to take the default model's.
    options: Option<Vec<ProviderOptionSelection>>,
    runtime_mode: Option<RuntimeMode>,
    interaction_mode: ProviderInteractionMode,
    workspace: Option<WorkspaceKind>,
    base_ref: Option<String>,
    start_from_origin: Option<bool>,
    /// The existing worktree picked: its path and branch.
    worktree: Option<(String, Option<String>)>,
    /// The checkout's branch, when the draft was opened for one.
    branch: Option<String>,
    /// The refs of each project's repository, by its folder.
    refs: HashMap<String, Loadable<RefList>>,
    base_ref_picker: PopoverMenuHandle<Picker<RefPickerDelegate>>,
    worktree_picker: PopoverMenuHandle<Picker<RefPickerDelegate>>,
    /// The menu of where the agent works, while it's open, and what had focus before it.
    workspace_menu: Option<(Entity<ContextMenu>, Option<FocusHandle>, Subscription)>,
    _tasks: Vec<Task<()>>,
    _subscriptions: Vec<Subscription>,
}

impl EventEmitter<NewThreadFormEvent> for NewThreadForm {}

/// The permission mode a new thread with `agent` starts in: `default` (from T3's settings) if
/// the agent supports it, or else the first mode it does.
pub fn runtime_mode_for(agent: &AgentChoice, default: RuntimeMode) -> RuntimeMode {
    compatible_runtime_mode(default, &agent.runtime_modes())
}

impl NewThreadForm {
    pub fn new(
        backend: Arc<dyn ThreadBackend>,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        let load_projects = cx.spawn({
            let backend = backend.clone();
            async move |this, cx| {
                let mut attempt = 0;
                loop {
                    let shell = backend.shell_snapshot().await;
                    let loaded = this.update(cx, |this, cx| {
                        this.projects_loaded(shell.map(|shell| shell.projects), cx)
                    });
                    if !matches!(loaded, Ok(false)) {
                        return;
                    }
                    cx.background_executor()
                        .timer(crate::backend::retry_delay(attempt))
                        .await;
                    attempt += 1;
                }
            }
        });
        let load_agents = cx.spawn({
            let backend = backend.clone();
            async move |this, cx| {
                let mut attempt = 0;
                loop {
                    let providers = backend.providers().await;
                    let loaded = this.update(cx, |this, cx| this.agents_loaded(providers, cx));
                    if !matches!(loaded, Ok(false)) {
                        return;
                    }
                    cx.background_executor()
                        .timer(crate::backend::retry_delay(attempt))
                        .await;
                    attempt += 1;
                }
            }
        });
        let load_settings = cx.spawn({
            let backend = backend.clone();
            async move |this, cx| {
                let mut attempt = 0;
                loop {
                    let settings = load_server_settings(&backend).await;
                    let loaded = this.update(cx, |this, cx| this.settings_loaded(settings, cx));
                    if !matches!(loaded, Ok(false)) {
                        return;
                    }
                    cx.background_executor()
                        .timer(crate::backend::retry_delay(attempt))
                        .await;
                    attempt += 1;
                }
            }
        });
        // Follow the app's shell too, when there is one, so projects added later show up.
        let mut subscriptions = Vec::new();
        if crate::backend(cx).is_some() {
            let shells = ThreadShells::global(cx);
            subscriptions.push(cx.observe(&shells, |this, shells, cx| {
                let projects = shells
                    .read(cx)
                    .shell()
                    .map(|shell| shell.projects.values().cloned().collect::<Vec<_>>());
                if let Some(projects) = projects {
                    this.projects_loaded(Ok(projects), cx);
                }
            }));
        }
        Self {
            backend,
            projects: Loadable::Loading,
            agents: Loadable::Loading,
            settings: Loadable::Loading,
            project: None,
            pending_project: None,
            scratch: Scratch::Idle,
            model: None,
            options: None,
            runtime_mode: None,
            interaction_mode: ProviderInteractionMode::Default,
            workspace: None,
            base_ref: None,
            start_from_origin: None,
            worktree: None,
            branch: None,
            refs: HashMap::default(),
            base_ref_picker: PopoverMenuHandle::default(),
            worktree_picker: PopoverMenuHandle::default(),
            workspace_menu: None,
            _tasks: vec![load_projects, load_agents, load_settings],
            _subscriptions: subscriptions,
        }
    }

    fn changed(&mut self, cx: &mut Context<Self>) {
        cx.emit(NewThreadFormEvent::Changed);
        cx.notify();
    }

    /// Returns whether the projects loaded.
    fn projects_loaded(
        &mut self,
        projects: anyhow::Result<Vec<OrchestrationProjectShell>>,
        cx: &mut Context<Self>,
    ) -> bool {
        let loaded = projects.is_ok();
        if !loaded && self.projects.loaded().is_some() {
            // Keep the projects the shell already gave.
            return false;
        }
        self.projects = match projects {
            Ok(mut projects) => {
                projects.sort_by(|left, right| {
                    (left.title.to_lowercase(), &left.id)
                        .cmp(&(right.title.to_lowercase(), &right.id))
                });
                Loadable::Loaded(projects)
            }
            Err(error) => Loadable::Failed(format!("{error:#}").into()),
        };
        if let Some(project_id) = self.pending_project.take() {
            self.select_project_id(project_id, cx);
        } else if self.project.is_none()
            && let Some(first) = self
                .listed_projects()
                .first()
                .map(|project| project.id.clone())
        {
            self.select_project_id(first, cx);
        }
        self.changed(cx);
        loaded
    }

    /// Returns whether the agents loaded.
    fn agents_loaded(
        &mut self,
        providers: anyhow::Result<Vec<t3_api::types::ServerProvider>>,
        cx: &mut Context<Self>,
    ) -> bool {
        let agents = providers.as_deref().map(agent_choices);
        // Until T3 has checked its agents (just after it starts) there may be none; ask again.
        let loaded = agents.as_ref().is_ok_and(|agents| !agents.is_empty());
        self.agents = match agents {
            Ok(agents) => Loadable::Loaded(agents),
            Err(error) => Loadable::Failed(format!("{error:#}").into()),
        };
        self.changed(cx);
        loaded
    }

    /// Returns whether the settings loaded. Until they do, the form uses T3's own defaults.
    fn settings_loaded(
        &mut self,
        settings: anyhow::Result<ServerSettings>,
        cx: &mut Context<Self>,
    ) -> bool {
        let loaded = settings.is_ok();
        self.settings = match settings {
            Ok(settings) => Loadable::Loaded(settings),
            Err(error) => {
                log::warn!("couldn't read T3's settings for a new thread: {error:#}");
                Loadable::Failed(format!("{error:#}").into())
            }
        };
        self.changed(cx);
        loaded
    }

    pub(crate) fn agents(&self) -> Option<&[AgentChoice]> {
        self.agents.loaded().map(Vec::as_slice)
    }

    /// The projects the form lists after "No project": every project but T3's scratch one.
    fn listed_projects(&self) -> Vec<&OrchestrationProjectShell> {
        let scratch_id = match &self.scratch {
            Scratch::Ready(id) => Some(id.as_str()),
            _ => None,
        };
        self.projects
            .loaded()
            .map(|projects| {
                projects
                    .iter()
                    .filter(|project| {
                        project.title != SCRATCH_PROJECT_TITLE
                            && Some(project.id.as_str()) != scratch_id
                    })
                    .collect()
            })
            .unwrap_or_default()
    }

    fn selected_project(&self) -> Option<&OrchestrationProjectShell> {
        let id = match self.project.as_ref()? {
            ProjectPick::Project(id) => id.as_str(),
            ProjectPick::Scratch => match &self.scratch {
                Scratch::Ready(id) => id.as_str(),
                _ => return None,
            },
        };
        self.projects
            .loaded()?
            .iter()
            .find(|project| project.id == id)
    }

    pub(crate) fn selected_project_id(&self) -> Option<&str> {
        match self.project.as_ref()? {
            ProjectPick::Project(id) => Some(id),
            ProjectPick::Scratch => match &self.scratch {
                Scratch::Ready(id) => Some(id),
                _ => None,
            },
        }
    }

    fn is_scratch(&self) -> bool {
        self.project == Some(ProjectPick::Scratch)
    }

    /// What T3's settings give a new thread in the selected project.
    pub(crate) fn defaults(&self) -> ThreadDefaults {
        thread_settings::thread_defaults(
            self.settings.loaded(),
            self.selected_project_id(),
            self.selected_project(),
        )
    }

    fn select_project_id(&mut self, project_id: String, cx: &mut Context<Self>) {
        let Some(root) = self
            .projects
            .loaded()
            .and_then(|projects| projects.iter().find(|project| project.id == project_id))
            .map(|project| project.workspace_root.clone())
        else {
            return;
        };
        if self.project.as_ref() != Some(&ProjectPick::Project(project_id.clone())) {
            self.project = Some(ProjectPick::Project(project_id));
            self.base_ref = None;
            self.worktree = None;
            self.branch = None;
        }
        self.load_refs(root, cx);
        self.changed(cx);
    }

    /// Picks the project at `index` of the dropdown: 0 is "No project".
    fn select_project(&mut self, index: usize, cx: &mut Context<Self>) {
        if index == 0 {
            self.select_scratch(cx);
            return;
        }
        let Some(project_id) = self
            .listed_projects()
            .get(index - 1)
            .map(|project| project.id.clone())
        else {
            return;
        };
        self.select_project_id(project_id, cx);
    }

    /// Picks no project: T3's scratch project, which `projects.ensureScratch` finds or creates.
    pub(crate) fn select_scratch(&mut self, cx: &mut Context<Self>) {
        self.project = Some(ProjectPick::Scratch);
        self.branch = None;
        self.worktree = None;
        if matches!(self.scratch, Scratch::Idle | Scratch::Failed(_)) {
            self.scratch = Scratch::Opening;
            let ensure = self.backend.call::<rpc::ProjectsEnsureScratch>(
                ProjectsEnsureScratchPayload(Default::default()),
                Access::Operate,
            );
            self._tasks.push(cx.spawn(async move |this, cx| {
                let result = ensure.await;
                this.update(cx, |this, cx| {
                    this.scratch = match result {
                        Ok(result) => Scratch::Ready(result.project_id),
                        Err(error) => Scratch::Failed(format!("{error:#}").into()),
                    };
                    this.changed(cx);
                })
                .ok();
            }));
        }
        self.changed(cx);
    }

    fn load_refs(&mut self, cwd: String, cx: &mut Context<Self>) {
        if matches!(
            self.refs.get(&cwd),
            Some(Loadable::Loading | Loadable::Loaded(_))
        ) {
            return;
        }
        self.refs.insert(cwd.clone(), Loadable::Loading);
        let list = self.backend.call::<rpc::VcsListRefs>(
            VcsListRefsInput {
                cursor: None,
                cwd: cwd.clone(),
                include_matching_remote_refs: None,
                limit: Some(REF_PAGE_SIZE),
                query: None,
                ref_kind: None,
                refresh: None,
            },
            Access::Read,
        );
        self._tasks.push(cx.spawn(async move |this, cx| {
            let result = list.await;
            this.update(cx, |this, cx| {
                let refs = match result {
                    Ok(result) => Loadable::Loaded(RefList {
                        refs: ref_choices(&result),
                        is_repo: result.is_repo,
                        total: result.total_count,
                    }),
                    Err(error) => {
                        log::warn!("couldn't list the branches of {cwd}: {error:#}");
                        Loadable::Failed(format!("{error:#}").into())
                    }
                };
                this.refs.insert(cwd, refs);
                this.changed(cx);
            })
            .ok();
        }));
    }

    /// The selected project's folder and its refs, once they've loaded.
    fn project_refs(&self) -> Option<(&str, Option<&RefList>)> {
        let project = self.selected_project()?;
        let refs = self
            .refs
            .get(&project.workspace_root)
            .and_then(Loadable::loaded);
        Some((project.workspace_root.as_str(), refs))
    }

    fn is_repository(&self) -> bool {
        !matches!(self.project_refs(), Some((_, Some(refs))) if !refs.is_repo)
    }

    /// Where the agent works: what was picked, or else what T3's settings say.
    pub(crate) fn workspace_kind(&self) -> WorkspaceKind {
        if self.is_scratch() || !self.is_repository() {
            return WorkspaceKind::Checkout;
        }
        self.workspace
            .unwrap_or_else(|| match self.defaults().env_mode {
                ThreadEnvMode::Worktree => WorkspaceKind::NewWorktree,
                _ => WorkspaceKind::Checkout,
            })
    }

    fn set_workspace_kind(&mut self, kind: WorkspaceKind, cx: &mut Context<Self>) {
        self.workspace = Some(kind);
        self.changed(cx);
    }

    fn start_from_origin(&self) -> bool {
        self.start_from_origin
            .unwrap_or_else(|| self.defaults().start_from_origin)
    }

    pub(crate) fn set_start_from_origin(
        &mut self,
        start_from_origin: bool,
        cx: &mut Context<Self>,
    ) {
        self.start_from_origin = Some(start_from_origin);
        self.changed(cx);
    }

    /// The ref a new worktree starts from: the one picked, or the repository's default branch.
    fn base_ref(&self) -> Option<&RefChoice> {
        let (_, refs) = self.project_refs()?;
        let refs = &refs?.refs;
        match &self.base_ref {
            Some(name) => refs.iter().find(|vcs_ref| &vcs_ref.name == name),
            None => thread_settings::default_base_ref(refs),
        }
    }

    fn base_ref_name(&self) -> Option<String> {
        self.base_ref
            .clone()
            .or_else(|| self.base_ref().map(|vcs_ref| vcs_ref.name.clone()))
    }

    pub(crate) fn pick_ref(
        &mut self,
        mode: RefPickerMode,
        choice: RefChoice,
        cx: &mut Context<Self>,
    ) {
        match mode {
            RefPickerMode::BaseRef => self.base_ref = Some(choice.name),
            RefPickerMode::Worktree => {
                self.worktree = choice.worktree_path.map(|path| (path, Some(choice.name)));
            }
        }
        self.changed(cx);
    }

    /// Opens the draft for [`NewThreadInWorktree`]: its project, and its worktree, or its
    /// branch of the project's checkout.
    pub(crate) fn prefill(&mut self, action: &NewThreadInWorktree, cx: &mut Context<Self>) {
        let project_id = action.project_id.clone();
        if self.projects.loaded().is_some() {
            self.select_project_id(project_id.clone(), cx);
        } else {
            self.pending_project = Some(project_id.clone());
        }
        self.project = Some(ProjectPick::Project(project_id));
        let root = self
            .selected_project()
            .map(|project| project.workspace_root.clone());
        match &action.worktree_path {
            Some(path) if Some(path) != root.as_ref() => {
                self.workspace = Some(WorkspaceKind::ExistingWorktree);
                self.worktree = Some((path.clone(), action.branch.clone()));
                self.branch = None;
            }
            _ => {
                self.workspace = Some(WorkspaceKind::Checkout);
                self.worktree = None;
                self.branch = action.branch.clone();
            }
        }
        self.changed(cx);
    }

    /// Opens the draft in `project_id`, working where T3's settings say, as T3's palette's "New
    /// thread in <project>" does.
    pub(crate) fn start_in_project(&mut self, project_id: String, cx: &mut Context<Self>) {
        if self.projects.loaded().is_some() {
            self.select_project_id(project_id, cx);
        } else {
            self.pending_project = Some(project_id.clone());
            self.project = Some(ProjectPick::Project(project_id));
        }
        self.changed(cx);
    }

    /// Whether the agent's workspace can be picked: a project with a repository.
    fn can_pick_workspace(&self) -> bool {
        !self.is_scratch() && self.selected_project().is_some() && self.is_repository()
    }

    /// The worktree [`UsePreviousWorktree`] would pick, from the app's shell.
    pub(crate) fn previous_worktree(&self, cx: &mut App) -> Option<(String, Option<String>)> {
        if !self.can_pick_workspace() {
            return None;
        }
        let project_id = self.selected_project_id()?;
        let current = match self.workspace_kind() {
            WorkspaceKind::ExistingWorktree => {
                self.worktree.as_ref().map(|(path, _)| path.as_str())
            }
            _ => None,
        };
        let shells = ThreadShells::global(cx);
        let shell = shells.read(cx).shell()?;
        previous_worktree(shell.threads.values(), project_id, current)
    }

    /// Works in the previous worktree, if there is one. Returns whether there was.
    pub(crate) fn use_previous_worktree(&mut self, cx: &mut Context<Self>) -> bool {
        let Some(worktree) = self.previous_worktree(cx) else {
            return false;
        };
        self.workspace = Some(WorkspaceKind::ExistingWorktree);
        self.worktree = Some(worktree);
        self.branch = None;
        self.changed(cx);
        true
    }

    /// Opens or closes the menu of where the agent works. Returns whether it applies (a
    /// project with a repository).
    pub(crate) fn toggle_workspace_menu(
        &mut self,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> bool {
        if !self.can_pick_workspace() {
            return false;
        }
        if let Some((menu, restore, _)) = self.workspace_menu.take() {
            menu.update(cx, |_, cx| cx.emit(DismissEvent));
            if let Some(restore) = restore {
                window.focus(&restore, cx);
            }
            cx.notify();
            return true;
        }
        // Key hints in the menu are the ones bound where the user is.
        let restore = window.focused(cx);
        let context = restore.clone();
        let kind = self.workspace_kind();
        let previous = self.previous_worktree(cx);
        let form = cx.entity().downgrade();
        let menu = ContextMenu::build(window, cx, move |mut menu, _, _| {
            if let Some(context) = context.clone() {
                menu = menu.context(context);
            }
            let pick = |menu: ContextMenu, label: &'static str, choice: WorkspaceKind| {
                let form = form.clone();
                menu.toggleable_entry(
                    label,
                    kind == choice,
                    IconPosition::End,
                    None,
                    move |window, cx| {
                        form.update(cx, |form, cx| {
                            form.set_workspace_kind(choice, cx);
                            if choice == WorkspaceKind::ExistingWorktree && form.worktree.is_none()
                            {
                                form.worktree_picker.show(window, cx);
                            }
                        })
                        .ok();
                    },
                )
            };
            menu = pick(menu, "New worktree", WorkspaceKind::NewWorktree);
            menu = pick(menu, "Existing worktree…", WorkspaceKind::ExistingWorktree);
            menu = pick(menu, "Project checkout", WorkspaceKind::Checkout);
            if let Some((_, branch)) = &previous {
                let form = form.clone();
                menu = menu.separator().item(
                    ContextMenuEntry::new(match branch {
                        Some(branch) => format!("Previous worktree ({branch})"),
                        None => "Previous worktree".to_string(),
                    })
                    .icon(IconName::GitBranch)
                    .icon_color(Color::Muted)
                    .action(Box::new(UsePreviousWorktree))
                    .handler(move |_, cx| {
                        form.update(cx, |form, cx| form.use_previous_worktree(cx))
                            .ok();
                    }),
                );
            }
            menu
        });
        window.focus(&menu.focus_handle(cx), cx);
        let subscription =
            cx.subscribe_in(&menu, window, |this, _, _: &DismissEvent, window, cx| {
                if let Some((_, restore, _)) = this.workspace_menu.take()
                    && let Some(restore) = restore
                    && !this.worktree_picker.is_deployed()
                {
                    window.focus(&restore, cx);
                }
                cx.notify();
            });
        self.workspace_menu = Some((menu, restore, subscription));
        cx.notify();
        true
    }

    #[cfg(test)]
    pub(crate) fn workspace_menu_is_open(&self) -> bool {
        self.workspace_menu.is_some()
    }

    #[cfg(test)]
    pub(crate) fn picked_worktree(
        &self,
    ) -> Option<(WorkspaceKind, Option<(String, Option<String>)>)> {
        Some((self.workspace_kind(), self.worktree.clone()))
    }

    // The agent, model, options, and modes, which the settings bar picks.

    /// The agent and model the thread starts with: the ones picked, or else the default model
    /// in T3's settings, or else the first agent's default model.
    pub(crate) fn model(&self) -> Option<(&AgentChoice, &ModelChoice)> {
        let agents = self.agents()?;
        let find = |instance_id: &str, slug: Option<&str>| {
            let agent = agents
                .iter()
                .find(|agent| agent.instance_id == instance_id)?;
            let model = slug
                .and_then(|slug| agent.model(slug))
                .or_else(|| agent.models.get(agent.default_model?))?;
            Some((agent, model))
        };
        if let Some((instance_id, slug)) = &self.model
            && let Some(found) = find(instance_id, Some(slug))
        {
            return Some(found);
        }
        if let Some(default) = self.defaults().model_selection
            && let Some(instance_id) = selection_instance_id(&default)
            && let Some(found) = find(instance_id, selection_model(&default))
        {
            return Some(found);
        }
        agents
            .iter()
            .find_map(|agent| Some((agent, agent.models.get(agent.default_model?)?)))
    }

    /// The options picked: the user's, or else the default model selection's when that's the
    /// model in use.
    fn picked_options(&self) -> Vec<ProviderOptionSelection> {
        if let Some(options) = &self.options {
            return options.clone();
        }
        let Some((agent, model)) = self.model() else {
            return Vec::new();
        };
        self.defaults()
            .model_selection
            .filter(|default| {
                selection_instance_id(default) == Some(agent.instance_id.as_str())
                    && selection_model(default) == Some(model.slug.as_str())
            })
            .map(|default| parse_options(default.options.as_ref()))
            .unwrap_or_default()
    }

    /// The model's options with the values picked.
    pub(crate) fn resolved_options(&self) -> Vec<ModelOption> {
        self.model()
            .map(|(_, model)| resolve_options(&model.options, &self.picked_options()))
            .unwrap_or_default()
    }

    pub(crate) fn pick_model(&mut self, instance_id: String, slug: String, cx: &mut Context<Self>) {
        let picked = self.picked_options();
        let options = self
            .agents()
            .and_then(|agents| agents.iter().find(|agent| agent.instance_id == instance_id))
            .and_then(|agent| agent.model(&slug))
            .map(|model| {
                // Keep the options picked that the new model takes too.
                selections_from(&resolve_options(&model.options, &picked))
                    .into_iter()
                    .filter(|selection| picked.iter().any(|picked| picked.id == selection.id))
                    .collect::<Vec<_>>()
            })
            .unwrap_or_default();
        self.model = Some((instance_id, slug));
        self.options = Some(options);
        self.changed(cx);
    }

    pub(crate) fn pick_option(
        &mut self,
        id: &str,
        value: ProviderOptionSelectionValue,
        cx: &mut Context<Self>,
    ) {
        let options = thread_settings::with_option(&self.resolved_options(), id, value);
        if let Some((agent, model)) = self.model() {
            self.model = Some((agent.instance_id.clone(), model.slug.clone()));
        }
        self.options = Some(options);
        self.changed(cx);
    }

    /// The permission mode the thread starts in.
    pub(crate) fn runtime_mode(&self) -> RuntimeMode {
        let mode = self
            .runtime_mode
            .clone()
            .unwrap_or_else(|| self.defaults().runtime_mode);
        match self.model() {
            Some((agent, _)) => runtime_mode_for(agent, mode),
            None => mode,
        }
    }

    pub(crate) fn pick_runtime_mode(&mut self, mode: RuntimeMode, cx: &mut Context<Self>) {
        self.runtime_mode = Some(mode);
        self.changed(cx);
    }

    pub(crate) fn interaction_mode(&self) -> ProviderInteractionMode {
        match self.model() {
            Some((agent, _)) if !agent.plan_mode => ProviderInteractionMode::Default,
            _ => self.interaction_mode.clone(),
        }
    }

    pub(crate) fn set_interaction_mode(
        &mut self,
        mode: ProviderInteractionMode,
        cx: &mut Context<Self>,
    ) {
        self.interaction_mode = mode;
        self.changed(cx);
    }

    /// The thread to launch with `text` as its first message, or why it can't be launched yet.
    pub fn new_thread(&self, text: &str, _cx: &App) -> Result<NewThread, SharedString> {
        if matches!(self.settings, Loadable::Loading) {
            return Err("Still reading T3's settings for new threads.".into());
        }
        let project_id = match &self.project {
            None => return Err("Pick a project for the thread.".into()),
            Some(ProjectPick::Project(id)) => id.clone(),
            Some(ProjectPick::Scratch) => match &self.scratch {
                Scratch::Ready(id) => id.clone(),
                Scratch::Failed(error) => {
                    return Err(format!("Couldn't start without a project: {error}").into());
                }
                Scratch::Idle | Scratch::Opening => {
                    return Err("Still opening T3's scratch folder.".into());
                }
            },
        };
        let (agent, model) = self
            .model()
            .ok_or_else(|| SharedString::from("Pick an agent and a model for the thread."))?;
        let options = launch_options(&model.options, &self.picked_options());
        let workspace = match self.workspace_kind() {
            WorkspaceKind::Checkout => ThreadWorkspace::ProjectRoot {
                branch: self.branch.clone(),
            },
            WorkspaceKind::NewWorktree => {
                let refs_loading = matches!(
                    self.selected_project()
                        .and_then(|project| self.refs.get(&project.workspace_root)),
                    Some(Loadable::Loading)
                );
                let base_ref = match self.base_ref_name() {
                    Some(base_ref) => base_ref,
                    None if refs_loading => {
                        return Err("Still loading the project's branches.".into());
                    }
                    // Without a branch list, start from `main`, as mustr always did.
                    None => "main".to_string(),
                };
                ThreadWorkspace::NewWorktree {
                    base_ref,
                    start_from_origin: self.start_from_origin(),
                }
            }
            WorkspaceKind::ExistingWorktree => {
                let (worktree_path, branch) = self
                    .worktree
                    .clone()
                    .ok_or_else(|| SharedString::from("Pick the worktree to work in."))?;
                ThreadWorkspace::ExistingWorktree {
                    worktree_path,
                    branch,
                }
            }
        };
        Ok(NewThread {
            project_id,
            model_selection: model_selection(&agent.instance_id, &model.slug, &options),
            runtime_mode: self.runtime_mode(),
            interaction_mode: self.interaction_mode(),
            workspace,
            text: text.to_string(),
        })
    }

    fn dropdown(
        &self,
        id: &'static str,
        label: String,
        entries: Vec<String>,
        selected: Option<usize>,
        on_select: fn(&mut Self, usize, &mut Context<Self>),
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> impl IntoElement + use<> {
        let this: WeakEntity<Self> = cx.entity().downgrade();
        let key = SharedString::from(format!(
            "{id}-{}-{}",
            entries.join("\u{1f}"),
            selected.unwrap_or(usize::MAX)
        ));
        let menu = window.use_keyed_state(key, cx, move |window, cx| {
            ContextMenu::new(window, cx, move |mut menu, _, _| {
                for (index, entry) in entries.into_iter().enumerate() {
                    let this = this.clone();
                    menu = menu.toggleable_entry(
                        entry,
                        selected == Some(index),
                        IconPosition::End,
                        None,
                        move |_, cx| {
                            this.update(cx, |this, cx| on_select(this, index, cx)).ok();
                        },
                    );
                }
                menu
            })
        });
        DropdownMenu::new(id, label, menu).style(DropdownStyle::Outlined)
    }

    fn ref_picker(
        &self,
        mode: RefPickerMode,
        label: String,
        cx: &mut Context<Self>,
    ) -> impl IntoElement + use<> {
        let (root, refs) = self
            .project_refs()
            .map(|(root, refs)| {
                (
                    root.to_string(),
                    refs.map(|refs| (refs.refs.clone(), refs.total)),
                )
            })
            .unwrap_or_default();
        let current = match mode {
            RefPickerMode::BaseRef => self.base_ref_name(),
            RefPickerMode::Worktree => self.worktree.as_ref().map(|(path, _)| path.clone()),
        };
        let handle = match mode {
            RefPickerMode::BaseRef => self.base_ref_picker.clone(),
            RefPickerMode::Worktree => self.worktree_picker.clone(),
        };
        let form = cx.entity().downgrade();
        let backend = self.backend.clone();
        let start_from_origin = self.start_from_origin();
        PopoverMenu::new(match mode {
            RefPickerMode::BaseRef => "new-thread-base-ref",
            RefPickerMode::Worktree => "new-thread-worktree",
        })
        .trigger(
            Button::new(
                match mode {
                    RefPickerMode::BaseRef => "new-thread-base-ref-trigger",
                    RefPickerMode::Worktree => "new-thread-worktree-trigger",
                },
                label,
            )
            .style(ButtonStyle::Outlined)
            .start_icon(
                Icon::new(IconName::GitBranch)
                    .size(IconSize::XSmall)
                    .color(Color::Muted),
            )
            .end_icon(
                Icon::new(IconName::ChevronDown)
                    .size(IconSize::XSmall)
                    .color(Color::Muted),
            ),
        )
        .with_handle(handle)
        .anchor(Anchor::TopLeft)
        .menu(move |window, cx| {
            let delegate = RefPickerDelegate {
                form: form.clone(),
                backend: backend.clone(),
                mode,
                cwd: root.clone(),
                current: current.clone(),
                matches: filter_refs(mode, &root, refs.as_ref().map(|(refs, _)| refs.as_slice())),
                total: refs.as_ref().map(|(_, total)| *total),
                status: None,
                selected_index: 0,
                start_from_origin,
            };
            Some(cx.new(|cx| {
                Picker::uniform_list(delegate, window, cx)
                    .popover()
                    .initial_width(rems(22.))
                    .reopenable(false, cx)
            }))
        })
    }
}

/// A labeled row of the form. A control too wide to go beside its label (a big
/// `ui_font_size` in a small window) goes below it.
fn field(label: &'static str, control: impl IntoElement) -> impl IntoElement {
    h_flex()
        .flex_wrap()
        .gap_3()
        .child(
            div()
                .w(scaled(px(88.)))
                .flex_none()
                .child(Label::new(label).size(LabelSize::Small).color(Color::Muted)),
        )
        .child(h_flex().flex_auto().min_w_0().child(control))
}

fn note(text: impl Into<SharedString>, color: Color) -> impl IntoElement {
    Label::new(text.into()).size(LabelSize::Small).color(color)
}

impl Render for NewThreadForm {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let project_control = match &self.projects {
            Loadable::Loading => note("Loading projects…", Color::Muted).into_any_element(),
            Loadable::Failed(error) => note(error.clone(), Color::Error).into_any_element(),
            Loadable::Loaded(_) => {
                let listed = self.listed_projects();
                let label = match &self.project {
                    Some(ProjectPick::Scratch) => SCRATCH_PROJECT_TITLE.to_string(),
                    Some(ProjectPick::Project(id)) => listed
                        .iter()
                        .find(|project| &project.id == id)
                        .map_or_else(|| "Pick a project".into(), |p| p.title.clone()),
                    None => "Pick a project".into(),
                };
                let selected = match &self.project {
                    Some(ProjectPick::Scratch) => Some(0),
                    Some(ProjectPick::Project(id)) => listed
                        .iter()
                        .position(|project| &project.id == id)
                        .map(|index| index + 1),
                    None => None,
                };
                let entries = std::iter::once(SCRATCH_PROJECT_TITLE.to_string())
                    .chain(listed.iter().map(|project| project.title.clone()))
                    .collect();
                self.dropdown(
                    "new-thread-project",
                    label,
                    entries,
                    selected,
                    Self::select_project,
                    window,
                    cx,
                )
                .into_any_element()
            }
        };

        let workspace_control = self.render_workspace(cx);
        let agent_note = match &self.agents {
            Loadable::Loaded(agents) if agents.is_empty() => Some(
                note(
                    "No agent is set up in T3. Install and sign in to one in T3 Code.",
                    Color::Muted,
                )
                .into_any_element(),
            ),
            Loadable::Failed(error) => Some(note(error.clone(), Color::Error).into_any_element()),
            _ => None,
        };
        let summary = self.summary();
        Card::new()
            .gap_3()
            .px(scaled(px(14.)))
            .py(scaled(px(12.)))
            .child(
                v_flex()
                    .gap_0p5()
                    .child(Label::new("New agent thread").weight(FontWeight::SEMIBOLD))
                    .child(
                        Label::new(
                            "T3 runs the agent on this machine with your own CLI. Pick its \
                             model and permissions below the message.",
                        )
                        .size(LabelSize::Small)
                        .color(Color::Muted),
                    ),
            )
            .child(field("Project", project_control))
            .child(field("Works in", workspace_control))
            .children(agent_note)
            .when_some(summary, |this, summary| {
                this.child(
                    Label::new(summary)
                        .size(LabelSize::XSmall)
                        .color(Color::Muted),
                )
            })
    }
}

impl NewThreadForm {
    fn render_workspace(&mut self, cx: &mut Context<Self>) -> AnyElement {
        if self.is_scratch() {
            return match &self.scratch {
                Scratch::Failed(error) => note(
                    format!("Couldn't start without a project: {error}"),
                    Color::Error,
                )
                .into_any_element(),
                _ => note(
                    "A folder of its own in T3's scratch directory, with no repository.",
                    Color::Muted,
                )
                .into_any_element(),
            };
        }
        if self.selected_project().is_none() {
            return div().into_any_element();
        }
        if !self.is_repository() {
            return note(
                "The project's folder. It isn't a git repository, so there are no worktrees.",
                Color::Muted,
            )
            .into_any_element();
        }
        let kind = self.workspace_kind();
        let this = cx.entity().downgrade();
        let segments = SegmentedControl::new("new-thread-workspace")
            .segment_with_tooltip("New worktree", "A new worktree and branch for this thread")
            .segment_with_tooltip(
                "Existing worktree",
                "A worktree that's already there, such as another thread's",
            )
            .segment_with_tooltip("Project checkout", "The project's own folder")
            .selected(match kind {
                WorkspaceKind::NewWorktree => 0,
                WorkspaceKind::ExistingWorktree => 1,
                WorkspaceKind::Checkout => 2,
            })
            .on_select(move |index, _, cx| {
                let kind = match index {
                    0 => WorkspaceKind::NewWorktree,
                    1 => WorkspaceKind::ExistingWorktree,
                    _ => WorkspaceKind::Checkout,
                };
                this.update(cx, |this, cx| this.set_workspace_kind(kind, cx))
                    .ok();
            });
        let detail = match kind {
            WorkspaceKind::NewWorktree => {
                let base = self.base_ref_name();
                let label = match (base, self.base_ref()) {
                    (Some(base), Some(vcs_ref))
                        if self.start_from_origin() && !vcs_ref.is_remote =>
                    {
                        format!("{base} (remote)")
                    }
                    (Some(base), _) => base,
                    (None, _) => "Loading branches…".to_string(),
                };
                h_flex()
                    .gap_1()
                    .child(
                        Label::new("from")
                            .size(LabelSize::Small)
                            .color(Color::Muted),
                    )
                    .child(self.ref_picker(RefPickerMode::BaseRef, label, cx))
                    .into_any_element()
            }
            WorkspaceKind::ExistingWorktree => {
                let label = match &self.worktree {
                    Some((path, Some(branch))) => format!("{branch} · {}", short_path(path)),
                    Some((path, None)) => short_path(path),
                    None => "Pick a worktree".to_string(),
                };
                self.ref_picker(RefPickerMode::Worktree, label, cx)
                    .into_any_element()
            }
            WorkspaceKind::Checkout => match &self.branch {
                Some(branch) => note(format!("on {branch}"), Color::Muted).into_any_element(),
                None => div().into_any_element(),
            },
        };
        let menu = self.workspace_menu.as_ref().map(|(menu, _, _)| {
            deferred(
                anchored()
                    .anchor(Anchor::TopLeft)
                    .snap_to_window_with_margin(px(8.))
                    .child(menu.clone()),
            )
            .with_priority(1)
        });
        let form = cx.entity().downgrade();
        let more = v_flex()
            .child(
                IconButton::new("new-thread-workspace-menu", IconName::Ellipsis)
                    .icon_size(IconSize::Small)
                    .icon_color(Color::Muted)
                    .toggle_state(self.workspace_menu.is_some())
                    .tooltip(|_, cx| {
                        Tooltip::for_action("Where the agent works", &ToggleWorkspacePicker, cx)
                    })
                    .on_click(move |_, window, cx| {
                        form.update(cx, |form, cx| form.toggle_workspace_menu(window, cx))
                            .ok();
                    }),
            )
            .child(div().h_0().children(menu));
        h_flex()
            .gap_2()
            .flex_wrap()
            .child(segments)
            .child(detail)
            .child(more)
            .into_any_element()
    }

    /// One line on how the thread will start, so nothing about it is a surprise.
    fn summary(&self) -> Option<String> {
        let (agent, model) = self.model()?;
        let mode = self.runtime_mode();
        let plan = if self.interaction_mode() == ProviderInteractionMode::Plan {
            ", in plan mode"
        } else {
            ""
        };
        let defaults = match self.settings {
            Loadable::Loaded(_) => "",
            Loadable::Failed(_) => " (T3's settings couldn't be read, so these are its defaults)",
            Loadable::Loading => " (reading T3's settings…)",
        };
        Some(format!(
            "{} runs {} with {}{plan}: {}{defaults}",
            agent.name,
            model.name,
            thread_settings::runtime_mode_label(&mode),
            thread_settings::runtime_mode_description(&mode),
        ))
    }
}

/// The last two parts of a path, which tell worktrees apart.
fn short_path(path: &str) -> String {
    let parts = path
        .trim_end_matches('/')
        .rsplit('/')
        .take(2)
        .collect::<Vec<_>>();
    parts.into_iter().rev().collect::<Vec<_>>().join("/")
}

// Picking a base branch or a worktree.

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum RefPickerMode {
    /// The branch a new worktree starts from: any ref.
    BaseRef,
    /// A worktree that exists: the branches checked out in one.
    Worktree,
}

fn filter_refs(mode: RefPickerMode, root: &str, refs: Option<&[RefChoice]>) -> Vec<RefChoice> {
    let refs = refs.unwrap_or_default();
    match mode {
        RefPickerMode::BaseRef => refs.to_vec(),
        RefPickerMode::Worktree => thread_settings::worktrees(refs, root)
            .into_iter()
            .cloned()
            .collect(),
    }
}

/// A searchable list of the project's refs, from `vcs.listRefs`, as T3's
/// `WorktreeBaseBranchPicker` offers them, with its "Start from remote" switch.
struct RefPickerDelegate {
    form: WeakEntity<NewThreadForm>,
    backend: Arc<dyn ThreadBackend>,
    mode: RefPickerMode,
    cwd: String,
    /// The ref or worktree path picked now.
    current: Option<String>,
    matches: Vec<RefChoice>,
    total: Option<i64>,
    status: Option<SharedString>,
    selected_index: usize,
    start_from_origin: bool,
}

impl PickerDelegate for RefPickerDelegate {
    type ListItem = ListItem;

    fn name() -> &'static str {
        "mustr new thread ref picker"
    }

    fn match_count(&self) -> usize {
        self.matches.len()
    }

    fn selected_index(&self) -> usize {
        self.selected_index
    }

    fn set_selected_index(
        &mut self,
        ix: usize,
        _window: &mut Window,
        _cx: &mut Context<Picker<Self>>,
    ) {
        self.selected_index = ix;
    }

    fn placeholder_text(&self, _window: &mut Window, _cx: &mut App) -> Arc<str> {
        match self.mode {
            RefPickerMode::BaseRef => "Search branches…".into(),
            RefPickerMode::Worktree => "Search worktrees by branch…".into(),
        }
    }

    fn no_matches_text(&self, _window: &mut Window, _cx: &mut App) -> Option<SharedString> {
        Some(match self.mode {
            RefPickerMode::BaseRef => "No branches match".into(),
            RefPickerMode::Worktree => "No other worktrees".into(),
        })
    }

    fn update_matches(
        &mut self,
        query: String,
        window: &mut Window,
        cx: &mut Context<Picker<Self>>,
    ) -> Task<()> {
        let query = query.trim().to_string();
        let list = self.backend.call::<rpc::VcsListRefs>(
            VcsListRefsInput {
                cursor: None,
                cwd: self.cwd.clone(),
                include_matching_remote_refs: None,
                limit: Some(REF_PAGE_SIZE),
                query: (!query.is_empty()).then_some(query),
                ref_kind: (self.mode == RefPickerMode::Worktree)
                    .then_some(VcsListRefsInputRefKind::Local),
                refresh: None,
            },
            Access::Read,
        );
        cx.spawn_in(window, async move |picker, cx| {
            let result = list.await;
            picker
                .update(cx, |picker, cx| {
                    let delegate = &mut picker.delegate;
                    match result {
                        Ok(result) => {
                            let refs = ref_choices(&result);
                            delegate.matches =
                                filter_refs(delegate.mode, &delegate.cwd, Some(&refs));
                            delegate.total = Some(result.total_count);
                            delegate.status = None;
                        }
                        Err(error) => {
                            delegate.status = Some(format!("{error:#}").into());
                        }
                    }
                    delegate.selected_index = delegate
                        .selected_index
                        .min(delegate.matches.len().saturating_sub(1));
                    cx.notify();
                })
                .ok();
        })
    }

    fn confirm(&mut self, _secondary: bool, window: &mut Window, cx: &mut Context<Picker<Self>>) {
        if let Some(choice) = self.matches.get(self.selected_index).cloned() {
            let mode = self.mode;
            self.form
                .update(cx, |form, cx| form.pick_ref(mode, choice, cx))
                .ok();
        }
        self.dismissed(window, cx);
    }

    fn dismissed(&mut self, _window: &mut Window, cx: &mut Context<Picker<Self>>) {
        cx.emit(DismissEvent);
    }

    fn render_match(
        &self,
        ix: usize,
        selected: bool,
        _window: &mut Window,
        _cx: &mut Context<Picker<Self>>,
    ) -> Option<Self::ListItem> {
        let choice = self.matches.get(ix)?;
        let (title, detail, current) = match self.mode {
            RefPickerMode::BaseRef => (
                choice.name.clone(),
                choice.badge(&self.cwd).map(str::to_string),
                self.current.as_deref() == Some(choice.name.as_str()),
            ),
            RefPickerMode::Worktree => (
                choice.name.clone(),
                choice.worktree_path.as_deref().map(short_path),
                self.current.is_some() && self.current == choice.worktree_path,
            ),
        };
        Some(
            ListItem::new(ix)
                .inset(true)
                .spacing(ListItemSpacing::Sparse)
                .toggle_state(selected)
                .start_slot(
                    Icon::new(if current {
                        IconName::Check
                    } else {
                        IconName::GitBranch
                    })
                    .size(IconSize::XSmall)
                    .color(if current { Color::Accent } else { Color::Muted }),
                )
                .child(
                    h_flex()
                        .w_full()
                        .min_w_0()
                        .gap_2()
                        .justify_between()
                        .child(Label::new(title).size(LabelSize::Small).truncate())
                        .when_some(detail, |this, detail| {
                            this.child(
                                Label::new(detail)
                                    .size(LabelSize::XSmall)
                                    .color(Color::Muted),
                            )
                        }),
                ),
        )
    }

    fn render_footer(
        &self,
        _window: &mut Window,
        cx: &mut Context<Picker<Self>>,
    ) -> Option<AnyElement> {
        let shown = self.matches.len() as i64;
        let status = self.status.clone().or_else(|| {
            self.total
                .filter(|total| *total > shown && self.mode == RefPickerMode::BaseRef)
                .map(|total| {
                    format!("Showing {shown} of {total} refs. Search for the rest.").into()
                })
        });
        if self.mode == RefPickerMode::Worktree && status.is_none() {
            return None;
        }
        let border = cx.theme().colors().border_variant;
        Some(
            v_flex()
                .w_full()
                .border_t_1()
                .border_color(border)
                .when(self.mode == RefPickerMode::BaseRef, |this| {
                    let form = self.form.clone();
                    let picker = cx.entity().downgrade();
                    let checked = self.start_from_origin;
                    this.child(
                        h_flex()
                            .px_3()
                            .py_1p5()
                            .gap_3()
                            .justify_between()
                            .child(
                                Label::new("Start from remote")
                                    .size(LabelSize::Small)
                                    .color(Color::Muted),
                            )
                            .child(
                                Switch::new(
                                    "new-thread-start-from-remote",
                                    if checked {
                                        ToggleState::Selected
                                    } else {
                                        ToggleState::Unselected
                                    },
                                )
                                .on_click(move |state, _, cx| {
                                    let checked = *state == ToggleState::Selected;
                                    form.update(cx, |form, cx| {
                                        form.set_start_from_origin(checked, cx)
                                    })
                                    .ok();
                                    picker
                                        .update(cx, |picker, cx| {
                                            picker.delegate.start_from_origin = checked;
                                            cx.notify();
                                        })
                                        .ok();
                                }),
                            ),
                    )
                })
                .when_some(status, |this, status| {
                    this.child(
                        div().px_3().py_1().child(
                            Label::new(status)
                                .size(LabelSize::XSmall)
                                .color(Color::Muted),
                        ),
                    )
                })
                .into_any_element(),
        )
    }
}

/// Opens new threads for [`NewThreadInWorktree`], which the worktrees view and the thread
/// toolbar dispatch: a draft with the project, worktree, and branch filled in.
pub(crate) fn register(workspace: &mut Workspace) {
    workspace.register_action(|workspace, action: &NewThreadInWorktree, window, cx| {
        let Some(view) = ThreadView::open_new(workspace, window, cx) else {
            log::error!("couldn't open a new thread: mustr isn't connected to T3");
            return;
        };
        if let Some(form) = view.read(cx).new_thread.clone() {
            form.update(cx, |form, cx| form.prefill(action, cx));
        }
    });
    // T3's `mod+shift+x` and `mod+shift+l`, on a draft. Elsewhere the keys go on to their other
    // bindings (such as the editor's Select All Matches).
    workspace.register_action(|workspace, _: &ToggleWorkspacePicker, window, cx| {
        let opened = active_form(workspace, cx)
            .is_some_and(|form| form.update(cx, |form, cx| form.toggle_workspace_menu(window, cx)));
        if !opened {
            cx.propagate();
        }
    });
    workspace.register_action(|workspace, _: &UsePreviousWorktree, _, cx| {
        let used = active_form(workspace, cx)
            .is_some_and(|form| form.update(cx, |form, cx| form.use_previous_worktree(cx)));
        if !used {
            cx.propagate();
        }
    });
}

/// The form of the draft the workspace shows, if it shows one.
fn active_form(workspace: &Workspace, cx: &App) -> Option<gpui::Entity<NewThreadForm>> {
    let view = workspace.active_item(cx)?.downcast::<ThreadView>()?;
    view.read(cx).new_thread.clone()
}

/// The form of `view`, if it's a draft. For tests.
#[cfg(test)]
pub(crate) fn form_of(
    view: &gpui::Entity<ThreadView>,
    cx: &App,
) -> Option<gpui::Entity<NewThreadForm>> {
    view.read(cx).new_thread.clone()
}

/// T3's server settings (`server.getSettings`). The 0.1.4 nightlies dropped that call; their
/// server config carries the settings, as every T3's does, so read them there instead.
async fn load_server_settings(backend: &Arc<dyn ThreadBackend>) -> anyhow::Result<ServerSettings> {
    let settings = backend
        .call::<rpc::ServerGetSettings>(ServerGetSettingsPayload(Default::default()), Access::Read)
        .await;
    match settings {
        Err(error) if crate::backend::is_unsupported_rpc(&error) => backend
            .call::<rpc::ServerGetConfig>(ServerGetConfigPayload(Default::default()), Access::Read)
            .await
            .map(|config| config.settings),
        settings => settings,
    }
}

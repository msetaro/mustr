use crate::{
    agent_review::{AgentReviewTarget, open_agent_review},
    agent_review_store::{ReviewedMarks, agent_reviews_dir},
    agent_threads::{AgentThread, agent_threads, agent_work_candidates},
    inbox::{ProjectRow, PullRequestEntry, PullRequestList, ThreadRow, ThreadStatus},
    pull_request_review, review_backend,
    t3_connection::{self, ConnectionStatus, EngineKind, EngineSetup, T3Connection, T3Session},
    t3_engine::T3Mode,
    worktree_git::LocalGit,
};
use anyhow::{Context as _, anyhow};
use editor::Editor;
use futures::StreamExt as _;
use gpui::{
    App, Entity, EventEmitter, FocusHandle, Focusable, Global, PathPromptOptions, Subscription,
    Task, WeakEntity,
};
use mustr_ui::{MustrIcon, Pill, SegmentedControl, tokens, tokens::scaled};
use review::ReviewScope;
use std::{
    path::{Path, PathBuf},
    sync::Arc,
};
use t3_client::t3_api::{rpc, types};
use thread_chat::{Shell, ThreadShells};
use ui::{Button, ButtonStyle, DiffStat, Divider, Indicator, prelude::*};
use workspace::{
    Toast, Workspace,
    item::{Item, ItemEvent},
    notifications::NotificationId,
};

const RECENT_THREADS: usize = 20;
/// The most recently finished threads checked for work to review.
const AGENT_WORK_CANDIDATES: usize = 25;
/// Worktrees checked with git at once.
const WORKTREE_CHECKS_IN_FLIGHT: usize = 4;
const CONTENT_MAX_WIDTH: f32 = 880.;

/// Startup options that change what the home view does on its own.
#[derive(Clone, Debug, Default)]
pub struct HomeOptions {
    /// Adds this folder as a project once connected. For smoke tests.
    pub add_project: Option<PathBuf>,
    /// Opens the first open pull request once the list loads. For smoke tests.
    pub open_first_pull_request: bool,
    /// Opens a review of this agent thread once the threads load. For smoke tests.
    pub review_thread: Option<(String, ReviewScope)>,
}

impl Global for HomeOptions {}

enum Loadable<T> {
    NotLoaded,
    Loading,
    Loaded(T),
    Failed(SharedString),
}

#[derive(Clone, Debug, PartialEq, Eq)]
enum InboxKey {
    PullRequest(usize),
    Thread(String),
}

struct Opening {
    key: InboxKey,
    _task: Task<()>,
}

/// Which of the inbox's items show.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum InboxFilter {
    #[default]
    All,
    PullRequests,
    AgentWork,
}

impl InboxFilter {
    const ALL: [InboxFilter; 3] = [
        InboxFilter::All,
        InboxFilter::PullRequests,
        InboxFilter::AgentWork,
    ];

    fn shows_pull_requests(self) -> bool {
        self != InboxFilter::AgentWork
    }

    fn shows_agent_work(self) -> bool {
        self != InboxFilter::PullRequests
    }
}

/// An agent thread with changes ready to review.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct AgentWork {
    pub thread: AgentThread,
    /// Files the branch changed against its base, when the worktree is on this machine.
    pub files_changed: Option<usize>,
}

/// What the shell snapshot gives the home view.
struct ThreadLists {
    all: Vec<AgentThread>,
    work: Vec<AgentWork>,
}

/// mustr's home: the T3 connection, the inbox (pull requests waiting on the reviewer and agent
/// threads with changes ready to review), and recent agent threads.
pub struct HomeView {
    focus_handle: FocusHandle,
    workspace: WeakEntity<Workspace>,
    connection: Entity<T3Connection>,
    pairing_editor: Entity<Editor>,
    pairing_error: Option<SharedString>,
    /// The connection generation the lists below were loaded from.
    loaded_generation: Option<usize>,
    pull_requests: Loadable<PullRequestList>,
    /// The app's shell: the projects and the recent threads, as T3 changes them.
    shells: Entity<ThreadShells>,
    threads: Vec<ThreadRow>,
    projects: Vec<ProjectRow>,
    agent_work: Loadable<Vec<AgentWork>>,
    adding_project: Option<Task<()>>,
    project_error: Option<SharedString>,
    add_project_on_load: Option<PathBuf>,
    /// Why no agent can run yet (from T3's provider status), if none can.
    agents_notice: Option<SharedString>,
    /// Every thread of the last shell snapshot, for opening one by id.
    all_threads: Vec<AgentThread>,
    filter: InboxFilter,
    opening: Option<Opening>,
    open_error: Option<SharedString>,
    open_first_pull_request: bool,
    review_thread: Option<(String, ReviewScope)>,
    _load_tasks: Vec<Task<()>>,
    _subscriptions: Vec<Subscription>,
}

impl HomeView {
    pub fn new(
        workspace: WeakEntity<Workspace>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        let connection = T3Connection::global(cx);
        let pairing_editor = cx.new(|cx| {
            let mut editor = Editor::single_line(window, cx);
            editor.set_placeholder_text("Pairing credential or pairing link", window, cx);
            editor.set_masked(true, cx);
            editor
        });
        let subscriptions = vec![cx.observe_in(&connection, window, |this, _, window, cx| {
            this.connection_changed(window, cx)
        })];
        let shells = ThreadShells::global(cx);
        let options = cx.try_global::<HomeOptions>().cloned().unwrap_or_default();
        let mut this = Self {
            focus_handle: cx.focus_handle(),
            workspace,
            connection,
            pairing_editor,
            pairing_error: None,
            loaded_generation: None,
            pull_requests: Loadable::NotLoaded,
            shells: shells.clone(),
            threads: Vec::new(),
            projects: Vec::new(),
            agent_work: Loadable::NotLoaded,
            adding_project: None,
            project_error: None,
            add_project_on_load: options.add_project,
            agents_notice: None,
            all_threads: Vec::new(),
            filter: InboxFilter::default(),
            opening: None,
            open_error: None,
            open_first_pull_request: options.open_first_pull_request,
            review_thread: options.review_thread,
            _load_tasks: Vec::new(),
            _subscriptions: subscriptions,
        };
        this._subscriptions
            .push(cx.observe(&shells, |this, _, cx| this.shell_changed(cx)));
        this.shell_changed(cx);
        this.connection_changed(window, cx);
        this
    }

    /// Takes the projects and the recent threads from the app's shell.
    fn shell_changed(&mut self, cx: &mut Context<Self>) {
        if let Some(shell) = self.shells.read(cx).shell() {
            self.projects = shell_projects(shell);
            self.threads = recent_threads(shell, RECENT_THREADS);
        }
        cx.notify();
    }

    fn connection_changed(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let connection = self.connection.read(cx);
        let generation = connection.generation();
        if let Some(session) = connection.session()
            && self.loaded_generation != Some(generation)
        {
            self.loaded_generation = Some(generation);
            self.load(session, window, cx);
        }
        cx.notify();
    }

    fn load(&mut self, session: Arc<T3Session>, window: &mut Window, cx: &mut Context<Self>) {
        self.pull_requests = Loadable::Loading;
        self.agent_work = Loadable::Loading;
        self.agents_notice = None;
        self.opening = None;

        let agents = cx.spawn_in(window, {
            let session = session.clone();
            async move |this, cx| {
                let result = cx
                    .background_spawn(async move {
                        // Refreshing runs T3's provider checks; right after the engine starts,
                        // its config says every provider is unchecked.
                        session
                            .rpc()
                            .call::<rpc::ServerRefreshProviders>(Default::default())
                            .await
                            .map(|update| update.providers)
                            .map_err(|error| anyhow!(error.to_string()))
                    })
                    .await;
                this.update(cx, |this, cx| {
                    match result {
                        Ok(providers) => {
                            this.agents_notice = agents_notice(&providers).map(Into::into)
                        }
                        Err(error) => log::warn!("couldn't check T3's agents: {error:#}"),
                    }
                    cx.notify();
                })
                .ok();
            }
        });

        let pull_requests = cx.spawn_in(window, {
            let session = session.clone();
            async move |this, cx| {
                let result = cx
                    .background_spawn(async move { session.open_pull_requests().await })
                    .await;
                this.update_in(cx, |this, window, cx| {
                    this.pull_requests = match result {
                        Ok(list) => Loadable::Loaded(list),
                        Err(error) => {
                            log::error!("couldn't list pull requests: {error:#}");
                            Loadable::Failed(format!("{error:#}").into())
                        }
                    };
                    if this.open_first_pull_request
                        && let Loadable::Loaded(list) = &this.pull_requests
                        && !list.entries.is_empty()
                    {
                        this.open_first_pull_request = false;
                        this.open_pull_request(0, window, cx);
                    }
                    cx.notify();
                })
                .ok();
            }
        });

        let threads = cx.spawn_in(window, async move |this, cx| {
            let result = cx
                .background_spawn(async move { load_threads(&session).await })
                .await;
            this.update_in(cx, |this, window, cx| {
                match result {
                    Ok(lists) => {
                        this.agent_work = Loadable::Loaded(lists.work);
                        this.all_threads = lists.all;
                    }
                    Err(error) => {
                        log::error!("couldn't load agent threads: {error:#}");
                        this.agent_work = Loadable::Failed(format!("{error:#}").into());
                    }
                }
                if let Some(path) = this.add_project_on_load.take() {
                    this.add_project(path, window, cx);
                }
                if let Some((thread_id, scope)) = this.review_thread.take() {
                    match this
                        .all_threads
                        .iter()
                        .find(|thread| thread.id == thread_id)
                    {
                        Some(thread) => this.open_agent_work(thread.clone(), scope, window, cx),
                        None => {
                            this.open_error = Some(format!("T3 has no thread {thread_id}").into());
                        }
                    }
                }
                cx.notify();
            })
            .ok();
        });

        self._load_tasks = vec![pull_requests, threads, agents];
    }

    fn open_pull_request(&mut self, index: usize, window: &mut Window, cx: &mut Context<Self>) {
        let Loadable::Loaded(list) = &self.pull_requests else {
            return;
        };
        let Some(entry) = list.entries.get(index).cloned() else {
            return;
        };
        self.open_error = None;
        let display_ref = entry.display_ref();
        let open =
            pull_request_review::open_review(self.workspace.clone(), entry.reference(), window, cx);
        let task = cx.spawn(async move |this, cx| {
            let result = open.await;
            this.update(cx, |this, cx| {
                this.opening = None;
                if let Err(error) = result {
                    log::error!("couldn't open {display_ref}: {error:#}");
                    this.open_error = Some(format!("{error:#}").into());
                }
                cx.notify();
            })
            .ok();
        });
        self.opening = Some(Opening {
            key: InboxKey::PullRequest(index),
            _task: task,
        });
        cx.notify();
    }

    /// Opens a review of an agent thread's work.
    fn open_agent_work(
        &mut self,
        thread: AgentThread,
        scope: ReviewScope,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.open_error = None;
        let key = InboxKey::Thread(thread.id.clone());
        let workspace = self.workspace.clone();
        let task = cx.spawn_in(window, async move |this, cx| {
            let title = thread.title.clone();
            let result = match workspace.update_in(cx, |workspace, window, cx| {
                open_agent_review(
                    workspace,
                    AgentReviewTarget { thread, scope },
                    None,
                    window,
                    cx,
                )
            }) {
                Ok(task) => task.await.map(|_| ()),
                Err(error) => Err(error),
            };
            this.update(cx, |this, cx| {
                this.opening = None;
                if let Err(error) = result {
                    log::error!("couldn't open the review of {title}: {error:#}");
                    this.open_error = Some(format!("{title}: {error:#}").into());
                }
                cx.notify();
            })
            .ok();
        });
        self.opening = Some(Opening { key, _task: task });
        cx.notify();
    }

    fn is_opening(&self, key: &InboxKey) -> bool {
        self.opening
            .as_ref()
            .is_some_and(|opening| opening.key == *key)
    }

    fn set_filter(&mut self, filter: InboxFilter, cx: &mut Context<Self>) {
        self.filter = filter;
        cx.notify();
    }

    fn submit_pairing(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let input = self.pairing_editor.read(cx).text(cx);
        let Some(credential) = t3_connection::parse_pairing_input(&input) else {
            self.pairing_error = Some("Paste a pairing credential or a T3 pairing link.".into());
            cx.notify();
            return;
        };
        self.pairing_error = None;
        self.pairing_editor
            .update(cx, |editor, cx| editor.clear(window, cx));
        self.connection.update(cx, |connection, cx| {
            connection.pair_with_credential(credential, cx)
        });
    }

    fn reconnect(&mut self, cx: &mut Context<Self>) {
        self.connection
            .update(cx, |connection, cx| connection.reconnect(cx));
    }

    fn render_connection(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let connection = self.connection.read(cx);
        let server = connection
            .server_url()
            .map(|url| url.as_str().trim_end_matches('/').to_string())
            .unwrap_or_default();
        let mode = connection.effective_mode();
        let pinned = t3_client::pinned_server_version();
        let (color, title, detail): (Color, SharedString, Option<SharedString>) =
            match connection.status() {
                ConnectionStatus::Connected {
                    environment,
                    server_version,
                    engine,
                } => {
                    let version = server_version.as_ref().map_or_else(
                        || "T3 Code".to_string(),
                        |version| format!("T3 Code {version}"),
                    );
                    match engine {
                        EngineKind::Embedded => (
                            Color::Success,
                            "Agents engine running".into(),
                            Some(format!("mustr's own engine · {version} · {server}").into()),
                        ),
                        EngineKind::Installed => (
                            Color::Success,
                            format!("Connected to T3 Code on {environment}").into(),
                            Some(format!("{version} at {server}").into()),
                        ),
                        EngineKind::External => (
                            Color::Success,
                            format!("Connected to {environment}").into(),
                            Some(format!("{version} at {server}").into()),
                        ),
                    }
                }
                ConnectionStatus::Connecting(step) => match mode {
                    T3Mode::Embedded => (
                        Color::Muted,
                        "Starting agents engine…".into(),
                        Some(step.clone()),
                    ),
                    T3Mode::Installed => (
                        Color::Muted,
                        "Connecting to T3 Code…".into(),
                        Some(step.clone()),
                    ),
                    T3Mode::External => (
                        Color::Muted,
                        "Connecting to T3 Code…".into(),
                        Some(format!("{step} · {server}").into()),
                    ),
                },
                ConnectionStatus::NeedsPairing(reason) => (
                    Color::Warning,
                    "Pair mustr with T3 Code".into(),
                    Some(reason.clone()),
                ),
                ConnectionStatus::NeedsSetup(EngineSetup::NotInstalled { unsupported: None }) => (
                    Color::Warning,
                    "Set up the agents engine".into(),
                    Some(
                        format!(
                            "mustr runs agents on T3 Code's engine, which isn't on this computer \
                             yet. mustr can install T3 Code {pinned} (about 100 MB) the way T3 \
                             Code's own installer does, and run it in the background."
                        )
                        .into(),
                    ),
                ),
                ConnectionStatus::NeedsSetup(EngineSetup::NotInstalled {
                    unsupported: Some(reason),
                }) => (
                    Color::Error,
                    "The agents engine isn't available here".into(),
                    Some(reason.clone()),
                ),
                ConnectionStatus::NeedsSetup(EngineSetup::NotRunning { service_supported }) => (
                    Color::Warning,
                    "T3 Code isn't running".into(),
                    Some(if *service_supported {
                        "T3 Code is installed, but nothing is running it. mustr can set it up to \
                         run in the background (t3 service install)."
                            .into()
                    } else {
                        "T3 Code is installed, but it isn't running, and it has no background \
                         service on this platform. Start it with `t3`, or use mustr's own engine."
                            .into()
                    }),
                ),
                ConnectionStatus::NeedsSetup(EngineSetup::Incompatible {
                    message,
                    can_update,
                }) => (
                    Color::Warning,
                    "This T3 Code doesn't work with this mustr".into(),
                    Some(if *can_update {
                        format!("{message} Updating it to {pinned} (t3 update) fixes this.").into()
                    } else {
                        message.clone()
                    }),
                ),
                ConnectionStatus::Failed(error) => (
                    Color::Error,
                    if mode == T3Mode::Embedded {
                        "The agents engine stopped".into()
                    } else {
                        "Couldn't connect to T3 Code".into()
                    },
                    Some(error.clone()),
                ),
            };
        // Connected, but paired without the scopes writes need: pairing again fixes it.
        let read_only = connection
            .session()
            .filter(|_| connection.status().is_connected())
            .and_then(|session| session.write_blocked_reason());
        let needs_pairing =
            matches!(connection.status(), ConnectionStatus::NeedsPairing(_)) || read_only.is_some();
        let status = connection.status();
        let can_retry = !matches!(status, ConnectionStatus::Connecting(_))
            && !matches!(
                status,
                ConnectionStatus::NeedsSetup(EngineSetup::NotInstalled { unsupported: None })
            );
        let retry_label = if status.is_connected() {
            "Reconnect"
        } else {
            "Try again"
        };
        let primary: Option<(
            &'static str,
            &'static str,
            fn(&mut T3Connection, &mut Context<T3Connection>),
        )> = match status {
            ConnectionStatus::NeedsSetup(EngineSetup::NotInstalled { unsupported: None }) => {
                Some((
                    "install-engine",
                    "Install the agents engine",
                    T3Connection::install_engine,
                ))
            }
            ConnectionStatus::NeedsSetup(EngineSetup::NotRunning {
                service_supported: true,
            }) => Some((
                "run-in-background",
                "Run T3 Code in the background",
                T3Connection::run_in_background,
            )),
            ConnectionStatus::NeedsSetup(EngineSetup::Incompatible {
                can_update: true, ..
            }) => Some((
                "update-t3",
                "Update T3 Code",
                T3Connection::update_installed,
            )),
            _ => None,
        };
        // The installed T3 Code can't be used right now: mustr's own engine still can.
        let offer_embedded = connection.mode() == T3Mode::Installed
            && !connection.is_fallback()
            && matches!(
                status,
                ConnectionStatus::NeedsSetup(_)
                    | ConnectionStatus::Failed(_)
                    | ConnectionStatus::NeedsPairing(_)
            );
        // An installed T3 Code older than mustr's works, with what its version has; say quietly
        // that updating brings the rest.
        let update_hint = match status {
            ConnectionStatus::Connected {
                server_version,
                engine: EngineKind::Installed,
                ..
            } => update_hint(server_version.as_deref(), pinned),
            _ => None,
        };
        let fallback_note = connection.is_fallback().then_some(
            "Using mustr's own engine for this session, because your T3 Code couldn't be used. \
             Your T3 Code's projects and threads aren't shown here.",
        );
        let log_note = matches!(status, ConnectionStatus::Failed(_))
            .then(|| connection.engine_log_file(cx))
            .flatten()
            .filter(|_| mode == T3Mode::Embedded)
            .map(|log| format!("The engine's log is {}.", log.display()));

        card(cx)
            .gap_2()
            .child(
                h_flex()
                    .gap_2()
                    .child(Indicator::dot().color(color))
                    .child(Label::new(title).weight(gpui::FontWeight::MEDIUM))
                    .child(div().flex_1())
                    .when(can_retry, |this| {
                        this.child(
                            Button::new("reconnect", retry_label)
                                .style(ButtonStyle::Subtle)
                                .label_size(LabelSize::Small)
                                .on_click(cx.listener(|this, _, _, cx| this.reconnect(cx))),
                        )
                    }),
            )
            .when_some(detail, |this, detail| {
                this.child(
                    Label::new(detail)
                        .size(LabelSize::Small)
                        .color(Color::Muted),
                )
            })
            .when_some(log_note, |this, note| {
                this.child(Label::new(note).size(LabelSize::Small).color(Color::Muted))
            })
            .when_some(fallback_note, |this, note| {
                this.child(
                    Label::new(note)
                        .size(LabelSize::Small)
                        .color(Color::Warning),
                )
            })
            .when(primary.is_some() || offer_embedded, |this| {
                this.child(
                    h_flex()
                        .gap_2()
                        .when_some(primary, |this, (id, label, action)| {
                            this.child(Button::new(id, label).style(ButtonStyle::Filled).on_click(
                                cx.listener(move |this, _, _, cx| {
                                    this.connection.update(cx, action)
                                }),
                            ))
                        })
                        .when(offer_embedded, |this| {
                            this.child(
                                Button::new("use-embedded", "Use mustr's own engine")
                                    .style(ButtonStyle::Subtle)
                                    .on_click(cx.listener(|this, _, _, cx| {
                                        this.connection.update(cx, |connection, cx| {
                                            connection.use_embedded(cx)
                                        })
                                    })),
                            )
                        }),
                )
            })
            .when_some(update_hint, |this, hint| {
                this.child(
                    h_flex()
                        .gap_2()
                        .child(Label::new(hint).size(LabelSize::Small).color(Color::Muted))
                        .child(
                            Button::new("update-t3-hint", "Update")
                                .style(ButtonStyle::Subtle)
                                .label_size(LabelSize::Small)
                                .on_click(cx.listener(|this, _, _, cx| {
                                    this.connection.update(cx, T3Connection::update_installed)
                                })),
                        ),
                )
            })
            .when_some(read_only, |this, read_only| {
                this.child(
                    Label::new(read_only)
                        .size(LabelSize::Small)
                        .color(Color::Warning),
                )
            })
            .when(needs_pairing, |this| this.child(self.render_pairing(cx)))
    }

    fn choose_project_folder(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let folder = choose_folder(cx);
        cx.spawn_in(window, async move |this, cx| match folder.await {
            Ok(Some(path)) => {
                this.update_in(cx, |this, window, cx| this.add_project(path, window, cx))
                    .ok();
            }
            Ok(None) => {}
            Err(error) => {
                this.update(cx, |this, cx| {
                    this.project_error = Some(format!("{error:#}").into());
                    cx.notify();
                })
                .ok();
            }
        })
        .detach();
    }

    /// Adds a local folder as a T3 project, showing how it went in the projects card. The
    /// shell then brings the project to every view.
    fn add_project(&mut self, path: PathBuf, window: &mut Window, cx: &mut Context<Self>) {
        self.project_error = None;
        let task = add_project(path, cx);
        self.adding_project = Some(cx.spawn_in(window, async move |this, cx| {
            let result = task.await;
            this.update(cx, |this, cx| {
                this.adding_project = None;
                match result {
                    Ok(title) => log::info!("added the project {title}"),
                    Err(error) => {
                        log::error!("couldn't add the project: {error:#}");
                        this.project_error = Some(format!("{error:#}").into());
                    }
                }
                cx.notify();
            })
            .ok();
        }));
        cx.notify();
    }

    fn render_projects(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let adding = self.adding_project.is_some();
        let can_add = self.connection.read(cx).session().is_some() && !adding;
        let shells = self.shells.read(cx);
        let loaded = shells.shell().is_some();
        let error = shells.error().cloned();
        let mut body = v_flex();
        match (&self.projects, loaded, error) {
            (_, false, Some(error)) => body = body.child(error_line(error)),
            (_, false, None) => body = body.child(muted_line("Loading projects…")),
            (projects, true, _) if projects.is_empty() => {
                body = body.child(muted_line(
                    "No projects yet. Add a folder on this computer, such as a git repository, to \
                     start agent threads in it.",
                ))
            }
            (projects, true, _) => {
                body = body.children(projects.iter().map(|project| {
                    h_flex()
                        .w_full()
                        .gap_3()
                        .px_2()
                        .py_1p5()
                        .child(
                            Icon::new(IconName::Folder)
                                .size(IconSize::Small)
                                .color(Color::Muted),
                        )
                        .child(
                            v_flex()
                                .flex_1()
                                .min_w_0()
                                .child(Label::new(project.title.clone()).truncate())
                                .when_some(project.workspace_root.clone(), |this, root| {
                                    this.child(
                                        Label::new(root)
                                            .size(LabelSize::Small)
                                            .color(Color::Muted)
                                            .truncate(),
                                    )
                                }),
                        )
                }))
            }
        }
        let count = loaded.then_some(self.projects.len());
        card(cx)
            .gap_1()
            .child(
                h_flex()
                    .gap_2()
                    .px_2()
                    .pb_1()
                    .child(Label::new("Projects").weight(gpui::FontWeight::SEMIBOLD))
                    .when_some(count, |this, count| {
                        this.child(
                            Label::new(count.to_string())
                                .size(LabelSize::Small)
                                .color(Color::Muted),
                        )
                    })
                    .child(div().flex_1())
                    .child(
                        Button::new(
                            "add-project",
                            if adding {
                                "Adding…"
                            } else {
                                "Add project…"
                            },
                        )
                        .style(ButtonStyle::Subtle)
                        .label_size(LabelSize::Small)
                        .disabled(!can_add)
                        .on_click(
                            cx.listener(|this, _, window, cx| {
                                this.choose_project_folder(window, cx)
                            }),
                        ),
                    ),
            )
            .child(Divider::horizontal())
            .child(body)
            .when_some(self.project_error.clone(), |this, error| {
                this.child(error_line(error))
            })
    }

    fn render_agents_notice(&self, cx: &mut Context<Self>) -> Option<impl IntoElement> {
        let notice = self.agents_notice.clone()?;
        Some(
            card(cx).child(
                h_flex()
                    .gap_2()
                    .child(Indicator::dot().color(Color::Warning))
                    .child(
                        Label::new(notice)
                            .size(LabelSize::Small)
                            .color(Color::Muted),
                    ),
            ),
        )
    }

    fn render_pairing(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let colors = cx.theme().colors();
        v_flex()
            .gap_2()
            .pt_1()
            .child(
                Label::new(
                    "On the T3 machine, run this command, then paste the credential or the \
                     pairing link it prints:",
                )
                .size(LabelSize::Small),
            )
            .child(
                div()
                    .px_2()
                    .py_1()
                    .rounded(scaled(tokens::CORNER_RADIUS / 2.))
                    .bg(colors.editor_background)
                    .child(
                        Label::new(t3_connection::pairing_command())
                            .size(LabelSize::Small)
                            .buffer_font(cx),
                    ),
            )
            .child(
                h_flex()
                    .gap_2()
                    .child(
                        div()
                            .key_context("MustrPairing")
                            .on_action(cx.listener(|this, _: &menu::Confirm, window, cx| {
                                this.submit_pairing(window, cx)
                            }))
                            .flex_1()
                            .px_2()
                            .py_1()
                            .rounded(scaled(tokens::CORNER_RADIUS / 2.))
                            .border(tokens::BORDER_WIDTH)
                            .border_color(colors.border)
                            .bg(colors.editor_background)
                            .child(self.pairing_editor.clone()),
                    )
                    .child(
                        Button::new("pair", "Pair")
                            .style(ButtonStyle::Filled)
                            .on_click(
                                cx.listener(|this, _, window, cx| this.submit_pairing(window, cx)),
                            ),
                    ),
            )
            .when_some(self.pairing_error.clone(), |this, error| {
                this.child(Label::new(error).size(LabelSize::Small).color(Color::Error))
            })
    }

    fn render_inbox(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let pull_requests = match &self.pull_requests {
            Loadable::Loaded(list) => Some(&list.entries),
            _ => None,
        };
        let agent_work = match &self.agent_work {
            Loadable::Loaded(work) => Some(work),
            _ => None,
        };
        let count = |filter: InboxFilter| {
            let pull_requests = pull_requests
                .filter(|_| filter.shows_pull_requests())
                .map_or(0, Vec::len);
            let agent_work = agent_work
                .filter(|_| filter.shows_agent_work())
                .map_or(0, Vec::len);
            pull_requests + agent_work
        };
        let filter = self.filter;
        let this = cx.entity().downgrade();
        let filters = InboxFilter::ALL.iter().fold(
            SegmentedControl::new("inbox-filter"),
            |control, option| {
                let label = match option {
                    InboxFilter::All => "All",
                    InboxFilter::PullRequests => "PRs",
                    InboxFilter::AgentWork => "Agent work",
                };
                control.segment(format!("{label} {}", count(*option)))
            },
        );
        let filters = filters
            .selected(
                InboxFilter::ALL
                    .iter()
                    .position(|option| *option == filter)
                    .unwrap_or(0),
            )
            .on_select(move |index, _, cx| {
                if let Some(filter) = InboxFilter::ALL.get(index).copied() {
                    this.update(cx, |this, cx| this.set_filter(filter, cx)).ok();
                }
            });

        // Newest first, pull requests and agent work together.
        enum Item<'a> {
            PullRequest(usize, &'a PullRequestEntry),
            Agent(&'a AgentWork),
        }
        let mut items = Vec::new();
        if filter.shows_pull_requests()
            && let Some(entries) = pull_requests
        {
            items.extend(entries.iter().enumerate().map(|(index, entry)| {
                (entry.updated_at.as_deref(), Item::PullRequest(index, entry))
            }));
        }
        if filter.shows_agent_work()
            && let Some(work) = agent_work
        {
            items.extend(work.iter().map(|work| {
                (
                    work.thread.latest_run_completed_at.as_deref(),
                    Item::Agent(work),
                )
            }));
        }
        items.sort_by(|(left, _), (right, _)| right.cmp(left));

        let mut body = v_flex().children(items.into_iter().map(|(_, item)| match item {
            Item::PullRequest(index, entry) => self.render_pull_request(index, entry, cx),
            Item::Agent(work) => self.render_agent_work(work, cx),
        }));
        let mut notes = Vec::new();
        if filter.shows_pull_requests() {
            match &self.pull_requests {
                Loadable::NotLoaded => {}
                Loadable::Loading => {
                    notes.push(muted_line("Loading pull requests…").into_any_element())
                }
                Loadable::Failed(error) => notes.push(error_line(error.clone()).into_any_element()),
                Loadable::Loaded(list) => {
                    notes.extend(list.errors.iter().map(|error| {
                        muted_line(format!("Couldn't list {error}")).into_any_element()
                    }))
                }
            }
        }
        if filter.shows_agent_work() {
            match &self.agent_work {
                Loadable::NotLoaded | Loadable::Loaded(_) => {}
                Loadable::Loading => {
                    notes.push(muted_line("Looking for agent work to review…").into_any_element())
                }
                Loadable::Failed(error) => notes.push(error_line(error.clone()).into_any_element()),
            }
        }
        let loaded_any = (filter.shows_pull_requests() && pull_requests.is_some())
            || (filter.shows_agent_work() && agent_work.is_some());
        if count(filter) == 0 && loaded_any && notes.is_empty() {
            notes.push(
                muted_line(match filter {
                    InboxFilter::All => "Nothing is waiting on you.",
                    InboxFilter::PullRequests => "No open pull requests.",
                    InboxFilter::AgentWork => "No agent work to review.",
                })
                .into_any_element(),
            );
        }
        body = body.children(notes);

        card(cx)
            .gap_1()
            .child(
                h_flex()
                    .gap_2()
                    .px_2()
                    .pb_1()
                    .child(Label::new("Inbox").weight(gpui::FontWeight::SEMIBOLD))
                    .child(
                        Label::new(count(InboxFilter::All).to_string())
                            .size(LabelSize::Small)
                            .color(Color::Muted),
                    )
                    .child(div().flex_1())
                    .child(filters),
            )
            .child(Divider::horizontal())
            .child(body)
    }

    fn render_agent_work(&self, work: &AgentWork, cx: &mut Context<Self>) -> AnyElement {
        let colors = cx.theme().colors();
        let thread = &work.thread;
        let is_opening = self.is_opening(&InboxKey::Thread(thread.id.clone()));
        let details = [
            thread.project.as_deref(),
            thread.branch.as_deref(),
            thread.agent.as_deref(),
        ]
        .into_iter()
        .flatten()
        .collect::<Vec<_>>()
        .join(" · ");
        let agent_color = mustr_ui::palette(cx).agent.hsla();
        let open = {
            let thread = thread.clone();
            cx.listener(move |this, _, window, cx| {
                this.open_agent_work(thread.clone(), ReviewScope::Changes, window, cx)
            })
        };
        h_flex()
            .id(SharedString::from(format!("agent-work-{}", thread.id)))
            .w_full()
            .gap_3()
            .px_2()
            .py_1p5()
            .rounded(scaled(tokens::CORNER_RADIUS / 2.))
            .cursor_pointer()
            .hover(|style| style.bg(colors.element_hover))
            .on_click(open)
            .child(
                MustrIcon::Agent
                    .icon()
                    .size(IconSize::Small)
                    .color(Color::Custom(agent_color)),
            )
            .child(
                v_flex()
                    .flex_1()
                    .min_w_0()
                    .child(Label::new(thread.title.clone()).truncate())
                    .when(!details.is_empty(), |this| {
                        this.child(
                            Label::new(details)
                                .size(LabelSize::Small)
                                .color(Color::Muted)
                                .truncate(),
                        )
                    }),
            )
            .when_some(work.files_changed, |this, files| {
                this.child(
                    Label::new(if files == 1 {
                        "1 file".to_string()
                    } else {
                        format!("{files} files")
                    })
                    .size(LabelSize::Small)
                    .color(Color::Muted),
                )
            })
            .child(thread_status(&thread.status))
            .when(is_opening, |this| {
                this.child(
                    Label::new("Opening…")
                        .size(LabelSize::Small)
                        .color(Color::Accent),
                )
            })
            .into_any_element()
    }

    fn render_pull_request(
        &self,
        index: usize,
        entry: &PullRequestEntry,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let colors = cx.theme().colors();
        let is_opening = self.is_opening(&InboxKey::PullRequest(index));
        let mut details = vec![entry.display_ref()];
        if let Some(author) = &entry.author {
            details.push(format!("@{}", author.login));
        }
        if entry.is_draft {
            details.push("draft".to_string());
        }
        h_flex()
            .id(("pull-request", index))
            .w_full()
            .gap_3()
            .px_2()
            .py_1p5()
            .rounded(scaled(tokens::CORNER_RADIUS / 2.))
            .cursor_pointer()
            .hover(|style| style.bg(colors.element_hover))
            .on_click(
                cx.listener(move |this, _, window, cx| this.open_pull_request(index, window, cx)),
            )
            .child(
                MustrIcon::PullRequest
                    .icon()
                    .size(IconSize::Small)
                    .color(Color::Muted),
            )
            .child(
                v_flex()
                    .flex_1()
                    .min_w_0()
                    .child(Label::new(entry.title.clone()).truncate())
                    .child(
                        Label::new(details.join(" · "))
                            .size(LabelSize::Small)
                            .color(Color::Muted)
                            .truncate(),
                    ),
            )
            .when(entry.viewer_review_requested, |this| {
                this.child(Pill::tinted(
                    "Review requested",
                    cx.theme().colors().text_accent,
                ))
            })
            .when(entry.has_line_counts(), |this| {
                this.child(DiffStat::new(
                    ("pull-request-diff-stat", index),
                    entry.additions as usize,
                    entry.deletions as usize,
                ))
            })
            .when(is_opening, |this| {
                this.child(
                    Label::new("Opening…")
                        .size(LabelSize::Small)
                        .color(Color::Accent),
                )
            })
            .into_any_element()
    }

    /// The threads that changed last and aren't settled: settled threads are done with, and
    /// the threads sidebar keeps them.
    fn render_threads(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let shells = self.shells.read(cx);
        let loaded = shells.shell().is_some();
        let any_threads = shells
            .shell()
            .is_some_and(|shell| !shell.threads.is_empty());
        let body = match (loaded, shells.error().cloned()) {
            (false, Some(error)) => error_line(error).into_any_element(),
            (false, None) => muted_line("Loading agent threads…").into_any_element(),
            _ if self.threads.is_empty() && any_threads => {
                muted_line("Every thread is settled. The threads sidebar keeps them.")
                    .into_any_element()
            }
            _ if self.threads.is_empty() => muted_line("No agent threads yet.").into_any_element(),
            _ => v_flex()
                .children(
                    self.threads
                        .iter()
                        .enumerate()
                        .map(|(index, thread)| render_thread(index, thread, cx).into_any_element()),
                )
                .into_any_element(),
        };
        section(
            "Recent agent threads",
            loaded.then_some(self.threads.len()),
            Some(body),
            cx,
        )
    }
}

/// The threads of T3's shell snapshot: the recent ones, and those with work to review. A
/// candidate whose worktree is on this machine is kept only if git finds changes in it; one
/// whose worktree is gone from a T3 on this machine has nothing left to diff.
async fn load_threads(session: &T3Session) -> anyhow::Result<ThreadLists> {
    let snapshot = session.shell_snapshot().await?;
    let all = agent_threads(&snapshot);
    let reviewed = ReviewedMarks::new(&agent_reviews_dir()).load();
    let server_is_local = session.server_is_local();
    let mut candidates = agent_work_candidates(&all, &reviewed, usize::MAX);
    // On T3's own machine, a worktree that's gone has nothing left to diff; most threads'
    // worktrees are cleaned up eventually, so those are dropped before picking the newest.
    if server_is_local {
        candidates.retain(|thread| {
            thread
                .worktree_path
                .as_deref()
                .is_some_and(|path| Path::new(path).is_dir())
        });
    }
    candidates.truncate(AGENT_WORK_CANDIDATES);
    let work = futures::stream::iter(candidates.into_iter().map(|thread| async move {
        let path = thread.worktree_path.clone()?;
        match LocalGit::open(Path::new(&path)).await {
            Some(local) => {
                let branch = local.current_branch().await;
                match local.changed_file_count(branch.as_deref()).await {
                    Ok(0) => None,
                    Ok(files) => Some(AgentWork {
                        thread,
                        files_changed: Some(files),
                    }),
                    Err(error) => {
                        log::warn!("couldn't count the changes in {path}: {error:#}");
                        Some(AgentWork {
                            thread,
                            files_changed: None,
                        })
                    }
                }
            }
            None if server_is_local => None,
            None => Some(AgentWork {
                thread,
                files_changed: None,
            }),
        }
    }))
    .buffered(WORKTREE_CHECKS_IN_FLIGHT)
    .filter_map(futures::future::ready)
    .collect::<Vec<_>>()
    .await;
    Ok(ThreadLists { all, work })
}

/// Handles [`thread_chat::AddProject`] (the palette's, and the threads sidebar's) on every
/// workspace, the way the home view's Add project works.
pub fn init(cx: &mut App) {
    cx.observe_new(|workspace: &mut Workspace, _, _| {
        workspace.register_action(|workspace, _: &thread_chat::AddProject, window, cx| {
            choose_and_add_project(workspace.weak_handle(), window, cx)
        });
    })
    .detach();
}

/// Asks for a folder with the OS's folder picker. `None` if the user cancelled.
fn choose_folder(cx: &mut App) -> Task<anyhow::Result<Option<PathBuf>>> {
    let paths = cx.prompt_for_paths(PathPromptOptions {
        files: false,
        directories: true,
        multiple: false,
        prompt: Some("Add project".into()),
    });
    cx.spawn(async move |_| match paths.await {
        Ok(Ok(Some(paths))) => Ok(paths.into_iter().next()),
        Ok(Ok(None)) | Err(_) => Ok(None),
        Ok(Err(error)) => Err(anyhow!("Couldn't open a folder picker: {error:#}")),
    })
}

struct ProjectToast;

/// The folder picker, then [`add_project`], reporting in a toast.
fn choose_and_add_project(workspace: WeakEntity<Workspace>, window: &mut Window, cx: &mut App) {
    let folder = choose_folder(cx);
    window
        .spawn(cx, async move |cx| {
            let result = match folder.await {
                Ok(Some(path)) => match cx.update(|_, cx| add_project(path, cx)) {
                    Ok(task) => task.await,
                    Err(_) => return,
                },
                Ok(None) => return,
                Err(error) => Err(error),
            };
            let message = match result {
                Ok(title) => format!("Added the project {title}."),
                Err(error) => {
                    log::error!("couldn't add the project: {error:#}");
                    format!("Couldn't add the project: {error:#}")
                }
            };
            workspace
                .update(cx, |workspace, cx| {
                    workspace.show_toast(
                        Toast::new(NotificationId::unique::<ProjectToast>(), message).autohide(),
                        cx,
                    )
                })
                .ok();
        })
        .detach();
}

/// Asks T3 to make `path` a project (`projects.mutate` with `project.create`), as T3's "Add
/// project" does. Returns the project's title.
///
/// The embedded engine is mustr's own, so the dry-run switch (which protects the user's real
/// T3 Code) doesn't block projects there; that's why this calls the session itself rather than
/// the threads' backend.
fn add_project(path: PathBuf, cx: &mut App) -> Task<anyhow::Result<String>> {
    let Some(session) = T3Connection::global(cx).read(cx).session() else {
        return Task::ready(Err(anyhow!("mustr isn't connected to an agents engine")));
    };
    if review_backend::dry_run_requested() && session.kind() != EngineKind::Embedded {
        return Task::ready(Err(anyhow!(
            "{} is set, so mustr didn't add {}",
            review_backend::DRY_RUN_ENV,
            path.display()
        )));
    }
    if let Some(reason) = session.operate_blocked_reason() {
        return Task::ready(Err(anyhow!(reason)));
    }
    let existing = ThreadShells::global(cx)
        .read(cx)
        .shell()
        .map(|shell| {
            shell
                .projects
                .values()
                .map(|project| (project.title.clone(), project.workspace_root.clone()))
                .collect::<Vec<_>>()
        })
        .unwrap_or_default();
    cx.background_spawn(async move {
        let project = thread_chat::projects::new_project(
            &path,
            existing
                .iter()
                .map(|(title, root)| (title.as_str(), root.as_str())),
        )?;
        let created = session
            .rpc()
            .call::<rpc::ProjectsMutate>(thread_chat::projects::create_mutation(&project))
            .await
            .map_err(|error| anyhow!(error.error_detail().unwrap_or_else(|| error.to_string())))
            .with_context(|| format!("adding {}", project.workspace_root))?;
        Ok(created.title)
    })
}

/// The shell's projects, by title.
fn shell_projects(shell: &Shell) -> Vec<ProjectRow> {
    let mut projects = shell
        .projects
        .values()
        .map(|project| ProjectRow {
            id: project.id.clone(),
            title: project.title.clone(),
            workspace_root: Some(project.workspace_root.clone()),
        })
        .collect::<Vec<_>>();
    projects.sort_by_cached_key(|project| (project.title.to_lowercase(), project.id.clone()));
    projects
}

/// The `limit` most recently updated threads of the shell, newest first, without settled,
/// archived or deleted ones.
fn recent_threads(shell: &Shell, limit: usize) -> Vec<ThreadRow> {
    let mut threads = shell
        .threads
        .values()
        .filter(|thread| {
            thread.deleted_at.is_none()
                && thread.archived_at.is_none()
                && thread.settled_override
                    != Some(types::OrchestrationV2ThreadShellSettledOverride::Settled)
        })
        .collect::<Vec<_>>();
    // Every timestamp is an ISO 8601 UTC string, so string order is time order.
    threads.sort_by(|left, right| {
        right
            .updated_at
            .cmp(&left.updated_at)
            .then_with(|| left.id.cmp(&right.id))
    });
    threads
        .into_iter()
        .take(limit)
        .map(|thread| {
            let status = match serde_json::to_value(&thread.status) {
                Ok(serde_json::Value::String(status)) => status,
                _ => String::new(),
            };
            ThreadRow {
                id: thread.id.clone(),
                title: if thread.title.trim().is_empty() {
                    "Untitled thread".to_string()
                } else {
                    thread.title.clone()
                },
                status: ThreadStatus::from_shell(status, thread.pending_runtime_request.is_some()),
                branch: thread.branch.clone(),
                project: shell
                    .projects
                    .get(&thread.project_id)
                    .map(|project| project.title.clone()),
            }
        })
        .collect()
}

/// Why no agent can run yet, from T3's provider status, or `None` when one can. Agents are the
/// coding CLIs T3 drives (Claude Code, Codex, …), each installed and signed in on its own.
fn agents_notice(providers: &[types::ServerProvider]) -> Option<String> {
    use types::{ServerProviderAuthStatus, ServerProviderState};
    let enabled = providers
        .iter()
        .filter(|provider| provider.enabled)
        .collect::<Vec<_>>();
    if enabled.is_empty() {
        return Some("No agents are turned on in T3 Code's settings.".to_string());
    }
    let usable = |provider: &types::ServerProvider| {
        provider.installed
            && matches!(
                provider.status,
                ServerProviderState::Ready | ServerProviderState::Warning
            )
            && provider.auth.status != ServerProviderAuthStatus::Unauthenticated
    };
    if enabled.iter().any(|provider| usable(provider)) {
        return None;
    }
    let problems = enabled
        .iter()
        .map(|provider| {
            let name = provider
                .display_name
                .clone()
                .unwrap_or_else(|| provider.driver.clone());
            if !provider.installed {
                format!("{name} isn't installed")
            } else if provider.auth.status == ServerProviderAuthStatus::Unauthenticated {
                format!("{name} isn't signed in")
            } else {
                match provider.message.as_deref() {
                    Some(message) => format!("{name}: {}", message.trim_end_matches('.')),
                    None => format!("{name} isn't ready"),
                }
            }
        })
        .collect::<Vec<_>>();
    Some(format!(
        "No agent can run yet: {}. Install an agent's CLI on this computer and sign in to it, \
         then choose Reconnect.",
        problems.join("; ")
    ))
}

fn status_color(status: &ThreadStatus) -> Color {
    match status {
        ThreadStatus::NeedsInput => Color::Warning,
        ThreadStatus::Working(_) => Color::Accent,
        ThreadStatus::Failed => Color::Error,
        ThreadStatus::Settled(_) => Color::Muted,
    }
}

fn thread_status(status: &ThreadStatus) -> impl IntoElement {
    let color = status_color(status);
    h_flex()
        .gap_1()
        .flex_none()
        .child(Indicator::dot().color(color))
        .child(
            Label::new(status.label().to_string())
                .size(LabelSize::Small)
                .color(color),
        )
}

fn render_thread(index: usize, thread: &ThreadRow, cx: &mut Context<HomeView>) -> impl IntoElement {
    let details = [thread.branch.as_deref(), thread.project.as_deref()]
        .into_iter()
        .flatten()
        .collect::<Vec<_>>()
        .join(" · ");
    let thread_id = thread.id.clone();
    let title = SharedString::from(thread.title.clone());
    h_flex()
        .id(("agent-thread", index))
        .w_full()
        .gap_3()
        .px_2()
        .py_1p5()
        .rounded(scaled(tokens::CORNER_RADIUS / 2.))
        .cursor_pointer()
        .hover(|style| style.bg(cx.theme().colors().element_hover))
        .on_click(cx.listener(move |this, _, window, cx| {
            thread_chat::open_thread(
                thread_id.clone(),
                title.clone(),
                this.workspace.clone(),
                window,
                cx,
            )
        }))
        .child(
            v_flex()
                .flex_1()
                .min_w_0()
                .child(Label::new(thread.title.clone()).truncate())
                .when(!details.is_empty(), |this| {
                    this.child(
                        Label::new(details)
                            .size(LabelSize::Small)
                            .color(Color::Muted)
                            .truncate(),
                    )
                }),
        )
        .child(thread_status(&thread.status))
}

fn card(cx: &App) -> Div {
    let colors = cx.theme().colors();
    v_flex()
        .w_full()
        .p_3()
        .rounded(scaled(tokens::CORNER_RADIUS))
        .border(tokens::BORDER_WIDTH)
        .border_color(colors.border)
        .bg(colors.surface_background)
}

fn section(
    title: &'static str,
    count: Option<usize>,
    body: Option<AnyElement>,
    cx: &App,
) -> impl IntoElement {
    card(cx)
        .gap_1()
        .child(
            h_flex()
                .gap_2()
                .px_2()
                .pb_1()
                .child(Label::new(title).weight(gpui::FontWeight::SEMIBOLD))
                .when_some(count, |this, count| {
                    this.child(
                        Label::new(count.to_string())
                            .size(LabelSize::Small)
                            .color(Color::Muted),
                    )
                }),
        )
        .child(Divider::horizontal())
        .children(body)
}

fn muted_line(text: impl Into<SharedString>) -> impl IntoElement {
    div().px_2().py_1p5().child(
        Label::new(text.into())
            .size(LabelSize::Small)
            .color(Color::Muted),
    )
}

fn error_line(text: SharedString) -> impl IntoElement {
    div()
        .px_2()
        .py_1p5()
        .child(Label::new(text).size(LabelSize::Small).color(Color::Error))
}

impl EventEmitter<ItemEvent> for HomeView {}

impl Focusable for HomeView {
    fn focus_handle(&self, _: &App) -> FocusHandle {
        self.focus_handle.clone()
    }
}

impl Item for HomeView {
    type Event = ItemEvent;

    fn tab_content_text(&self, _detail: usize, _cx: &App) -> SharedString {
        "Home".into()
    }

    fn show_toolbar(&self) -> bool {
        false
    }

    fn to_item_events(event: &Self::Event, f: &mut dyn FnMut(ItemEvent)) {
        f(*event)
    }
}

impl Render for HomeView {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let show_lists = self.loaded_generation.is_some();
        v_flex()
            .key_context("MustrHome")
            .track_focus(&self.focus_handle)
            .size_full()
            .bg(cx.theme().colors().editor_background)
            .child(
                v_flex()
                    .id("home-scroll")
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
                            .child(
                                v_flex()
                                    .gap_0p5()
                                    .child(Headline::new("mustr").size(HeadlineSize::Large))
                                    .child(
                                        Label::new(
                                            "Review every change, from people and from agents.",
                                        )
                                        .color(Color::Muted),
                                    ),
                            )
                            .child(self.render_connection(cx))
                            .when(show_lists, |this| {
                                this.children(self.render_agents_notice(cx))
                            })
                            .when_some(self.open_error.clone(), |this, error| {
                                this.child(card(cx).child(error_line(error)))
                            })
                            .when(show_lists, |this| {
                                this.child(self.render_inbox(cx))
                                    .child(self.render_projects(cx))
                                    .child(self.render_threads(cx))
                            }),
                    ),
            )
    }
}

/// "T3 Code 0.1.3 is available", when the connected T3 Code is older than the version mustr is
/// built for (`T3_VERSION`). Older 0.1.x servers on the same protocol work; they only lack
/// what came later.
fn update_hint(server_version: Option<&str>, pinned: &str) -> Option<SharedString> {
    let server = semver::Version::parse(server_version?).ok()?;
    let pinned_version = semver::Version::parse(pinned).ok()?;
    (server < pinned_version).then(|| format!("T3 Code {pinned} is available.").into())
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn older_t3_codes_get_a_quiet_update_hint() {
        assert_eq!(
            update_hint(Some("0.1.1"), "0.1.3").as_deref(),
            Some("T3 Code 0.1.3 is available.")
        );
        // The nightlies before a release are older than it.
        assert!(update_hint(Some("0.1.3-nightly.20261027.3754"), "0.1.3").is_some());
        for newer_or_same in ["0.1.3", "0.1.4-nightly.20261028.3841", "0.2.0"] {
            assert_eq!(update_hint(Some(newer_or_same), "0.1.3"), None);
        }
        assert_eq!(update_hint(None, "0.1.3"), None);
        assert_eq!(update_hint(Some("dev"), "0.1.3"), None);
    }

    fn shell(threads: Vec<serde_json::Value>) -> Shell {
        let thread_fields = |thread: serde_json::Value| {
            let mut full = json!({
                "projectId": "p1", "createdAt": "2026-10-01T00:00:00.000Z",
                "createdBy": "user", "creationSource": "web", "interactionMode": "default",
                "runtimeMode": "full-access",
                "modelSelection": { "instanceId": "codex", "model": "gpt-5" },
                "providerInstanceId": "codex", "activeProviderThreadId": null,
                "activeRunId": null, "archivedAt": null, "branch": null, "deletedAt": null,
                "forkedFrom": null, "hasActionableProposedPlan": false, "itemCount": 1,
                "latestRunId": null, "latestUserMessageAt": null, "latestVisibleMessage": null,
                "lineage": { "rootThreadId": "x", "parentThreadId": null, "relationshipToParent": null },
                "pendingRuntimeRequest": null, "settledAt": null, "settledOverride": null,
                "visibleItemCount": 1, "worktreePath": null
            });
            util::merge_json_value_into(thread, &mut full);
            full
        };
        let project = |id: &str, title: &str| {
            json!({
                "id": id, "title": title, "workspaceRoot": format!("/src/{title}"),
                "scripts": [], "createdAt": "2026-10-01T00:00:00.000Z",
                "updatedAt": "2026-10-01T00:00:00.000Z", "defaultModelSelection": null
            })
        };
        let snapshot: types::OrchestrationV2ShellSnapshot = serde_json::from_value(json!({
            "schemaVersion": 1,
            "snapshotSequence": 3,
            "threads": threads.into_iter().map(thread_fields).collect::<Vec<_>>(),
            "archivedThreads": [],
            "projects": [project("p2", "zed"), project("p1", "mustr")]
        }))
        .unwrap();
        Shell::from_snapshot(&snapshot)
    }

    #[test]
    fn recent_threads_are_newest_first_without_settled_ones() {
        let thread = |id: &str, status: &str, updated_at: &str| {
            json!({
                "id": id, "title": id, "status": status, "branch": format!("t3/{id}"),
                "updatedAt": updated_at
            })
        };
        let mut waiting = thread("waiting", "running", "2026-10-27T10:00:00.000Z");
        waiting["pendingRuntimeRequest"] =
            json!({ "id": "r1", "kind": "command", "createdAt": "2026-10-27T10:00:00.000Z" });
        let mut settled = thread("settled", "completed", "2026-10-27T12:30:00.000Z");
        settled["settledOverride"] = json!("settled");
        settled["settledAt"] = json!("2026-10-27T12:30:00.000Z");
        let mut deleted = thread("deleted", "idle", "2026-10-27T12:00:00.000Z");
        deleted["deletedAt"] = json!("2026-10-27T12:00:00.000Z");
        let mut unnamed = thread("", "failed", "2026-10-26T09:00:00.000Z");
        unnamed["projectId"] = json!("unknown");
        unnamed["branch"] = json!(null);
        // Un-settled threads are back.
        let mut back = thread("back", "completed", "2026-10-26T08:00:00.000Z");
        back["settledOverride"] = json!("active");

        let shell = shell(vec![
            thread("old", "idle", "2026-10-20T00:00:00.000Z"),
            waiting,
            settled,
            deleted,
            unnamed,
            back,
            thread("newest", "running", "2026-10-27T11:00:00.000Z"),
        ]);
        let threads = recent_threads(&shell, 4);
        assert_eq!(
            threads,
            vec![
                ThreadRow {
                    id: "newest".into(),
                    title: "newest".into(),
                    status: ThreadStatus::Working("running".into()),
                    branch: Some("t3/newest".into()),
                    project: Some("mustr".into()),
                },
                ThreadRow {
                    id: "waiting".into(),
                    title: "waiting".into(),
                    status: ThreadStatus::NeedsInput,
                    branch: Some("t3/waiting".into()),
                    project: Some("mustr".into()),
                },
                ThreadRow {
                    id: "".into(),
                    title: "Untitled thread".into(),
                    status: ThreadStatus::Failed,
                    branch: None,
                    project: None,
                },
                ThreadRow {
                    id: "back".into(),
                    title: "back".into(),
                    status: ThreadStatus::Settled("completed".into()),
                    branch: Some("t3/back".into()),
                    project: Some("mustr".into()),
                },
            ]
        );
        assert_eq!(threads[1].status.label(), "needs input");
        assert_eq!(
            ThreadStatus::from_shell("rolled_back".into(), false).label(),
            "rolled back"
        );
        assert_eq!(
            shell_projects(&shell)
                .into_iter()
                .map(|project| (project.title, project.workspace_root.unwrap()))
                .collect::<Vec<_>>(),
            [
                ("mustr".to_string(), "/src/mustr".to_string()),
                ("zed".to_string(), "/src/zed".to_string())
            ]
        );
    }

    fn provider(driver: &str, value: serde_json::Value) -> types::ServerProvider {
        let mut provider = json!({
            "auth": { "status": "unknown" },
            "checkedAt": "2026-10-28T00:00:00Z",
            "driver": driver,
            "enabled": true,
            "installed": true,
            "instanceId": driver,
            "models": [],
            "status": "ready",
            "version": null,
        });
        util::merge_json_value_into(value, &mut provider);
        serde_json::from_value(provider).unwrap()
    }

    #[test]
    fn the_agents_notice_appears_only_when_no_agent_can_run() {
        let claude = |value| provider("claudeAgent", value);
        let codex = |value| provider("codex", value);
        assert_eq!(agents_notice(&[claude(json!({})), codex(json!({}))]), None);
        // One agent that can run is enough.
        assert_eq!(
            agents_notice(&[
                claude(json!({ "installed": false, "status": "error" })),
                codex(json!({ "status": "warning", "auth": { "status": "authenticated" } })),
            ]),
            None
        );
        let notice = agents_notice(&[
            claude(json!({ "installed": false, "status": "error", "displayName": "Claude Code" })),
            codex(json!({ "auth": { "status": "unauthenticated" }, "displayName": "Codex" })),
            provider("cursor", json!({ "enabled": false })),
        ])
        .unwrap();
        assert!(
            notice.starts_with(
                "No agent can run yet: Claude Code isn't installed; Codex isn't signed in."
            ),
            "{notice}"
        );
        assert!(!notice.contains("cursor"));
        assert_eq!(
            agents_notice(&[provider("cursor", json!({ "enabled": false }))]).unwrap(),
            "No agents are turned on in T3 Code's settings."
        );
        // Otherwise T3's own explanation, under the driver's name when T3 gives no other.
        let notice = agents_notice(&[claude(json!({
            "status": "error",
            "message": "Claude Code 1.0 is too old.",
        }))])
        .unwrap();
        assert!(
            notice.contains("yet: claudeAgent: Claude Code 1.0 is too old. Install"),
            "{notice}"
        );
    }
}

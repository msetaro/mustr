//! Cloning a repository as a project, as T3's palette does (`components/CommandPalette.tsx`'s
//! clone flow, `ProjectCloneToastCoordinator.tsx`): [`CloneRepository`] asks for a repository
//! (a clone URL, or GitHub's `owner/name`) and the folder to clone into, which starts in T3's
//! "Add project" base folder (`addProjectBaseDirectory`, else `~/`) named after the repository.
//!
//! T3 clones it and adds the project itself (`projectClone.start`, which needs
//! `source-control:write`), and reports progress on `subscribeProjectClones`; once it's done, a
//! new thread opens in the project. A failed clone can be retried (`projectClone.retry`), and a
//! running one cancelled (`projectClone.cancel`). T3 0.1.1 has all four.

use crate::{
    backend::{Access, ThreadBackend, ThreadBackendExt as _},
    palette::NewThreadInProject,
    shell::ThreadShells,
};
use editor::{Editor, EditorEvent};
use futures::StreamExt as _;
use gpui::{
    App, AppContext as _, Context, DismissEvent, Entity, EventEmitter, FocusHandle, Focusable,
    FontWeight, IntoElement, ParentElement as _, Render, SharedString, Styled as _, Subscription,
    Task, WeakEntity, Window, actions, div, px,
};
use mustr_ui::{ButtonStyle as MustrButtonStyle, TextButton, palette, tokens, tokens::scaled};
use std::{sync::Arc, time::Duration};
use t3_api::{
    rpc,
    types::{
        ProjectCloneActionInput, ProjectClonePhase, ProjectCloneSnapshot, ProjectCloneStage,
        ProjectCloneStartInput, ProjectCloneSubscribeInput,
    },
};
use ui::{Color, Label, LabelCommon as _, LabelSize, prelude::*};
use workspace::{ModalView, Workspace};

actions!(
    thread_chat,
    [
        /// Clones a git repository into a folder and adds it as a project.
        CloneRepository,
    ]
);

/// How long a finished clone waits for its project to reach the shell before the new thread
/// opens, as T3's `waitForProject` does.
const PROJECT_WAIT: Duration = Duration::from_secs(3);

/// GitHub's `owner/name`, as T3 reads it (`REPOSITORY_SHORTHAND` in T3's
/// `client-runtime/src/operations/projects.ts`).
pub(crate) fn is_shorthand(input: &str) -> bool {
    let input = input.trim();
    let Some((owner, name)) = input.split_once('/') else {
        return false;
    };
    let owner_ok = !owner.is_empty()
        && owner.len() <= 39
        && owner
            .chars()
            .next()
            .is_some_and(|c| c.is_ascii_alphanumeric())
        && owner.chars().all(|c| c.is_ascii_alphanumeric() || c == '-');
    let name_ok = !name.is_empty()
        && name
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '-'));
    owner_ok && name_ok
}

/// The folder a clone of `repository` goes in, as T3 names it (`getCloneDirectoryName`): the
/// last part of its path, without `.git`. Empty when there's none, such as a bare host.
pub(crate) fn directory_name(repository: &str) -> String {
    let without_query = repository.split(['?', '#']).next().unwrap_or("").trim();
    let scheme = without_query.find("://");
    let ssh_like = {
        let (user, rest) = without_query.split_once('@').unwrap_or(("", ""));
        !user.is_empty() && !user.contains(['/', '\\', ':']) && rest.contains(':')
    };
    let has_host = scheme.is_some() || ssh_like;
    let path = match scheme {
        Some(index) => &without_query[index + 3..],
        None => without_query,
    };
    let segments = path
        .split(['/', '\\', ':'])
        .filter(|segment| !segment.trim().is_empty())
        .collect::<Vec<_>>();
    if has_host && segments.len() < 2 {
        return String::new();
    }
    let last = segments.last().map_or("", |segment| segment.trim());
    if has_host && segments.len() == 2 && last.chars().all(|c| c.is_ascii_digit()) {
        return String::new();
    }
    last.strip_suffix(".git").unwrap_or(last).to_string()
}

/// Where a clone of `repository` goes by default: in `base` (T3's `addProjectBaseDirectory`,
/// else `~/`), in a folder named after it.
pub(crate) fn default_destination(base: Option<&str>, repository: &str) -> String {
    let base = base
        .map(str::trim)
        .filter(|base| !base.is_empty())
        .unwrap_or("~/");
    let base = if base.ends_with('/') || base.ends_with('\\') {
        base.to_string()
    } else {
        format!("{base}/")
    };
    format!("{base}{}", directory_name(repository))
}

/// `projectClone.start` for cloning `repository` into `destination`, as T3's palette sends it:
/// a URL as `remoteUrl`, and GitHub's `owner/name` for T3 to look up with its own credentials.
/// The project is named after the folder (T3's `inferProjectTitleFromPath`).
pub(crate) fn start_input(
    project_id: String,
    repository: &str,
    destination: &str,
    created_at: String,
) -> ProjectCloneStartInput {
    let repository = repository.trim();
    let destination = destination.trim().trim_end_matches(['/', '\\']).to_string();
    let title = destination
        .rsplit(['/', '\\'])
        .find(|part| !part.is_empty())
        .unwrap_or(destination.as_str())
        .to_string();
    let shorthand = is_shorthand(repository);
    ProjectCloneStartInput {
        created_at,
        destination_path: destination,
        project_id,
        protocol: None,
        provider: shorthand.then(|| "github".to_string()),
        remote_url: (!shorthand).then(|| repository.to_string()),
        repository: shorthand.then(|| repository.to_string()),
        title,
    }
}

/// A clone's progress in T3's words (`projectCloneProgressSummary`): "Receiving objects · 45%".
pub(crate) fn progress_summary(snapshot: &ProjectCloneSnapshot) -> String {
    let stage = match &snapshot.stage {
        ProjectCloneStage::Connecting => "Connecting",
        ProjectCloneStage::Counting => "Counting objects",
        ProjectCloneStage::Receiving => "Receiving objects",
        ProjectCloneStage::Resolving => "Resolving deltas",
        ProjectCloneStage::Checkout => "Checking out files",
        ProjectCloneStage::Unrecognized(_) => "Cloning",
    };
    let mut parts = vec![stage.to_string()];
    if let Some(percent) = snapshot.percent {
        parts.push(format!("{percent}%"));
    }
    if snapshot.stalled == Some(true) {
        parts.push("Stalled".to_string());
    } else if let Some(detail) = snapshot.detail.clone().filter(|detail| !detail.is_empty()) {
        parts.push(detail);
    }
    parts.join(" · ")
}

enum CloneState {
    Idle,
    Starting,
    Cloning {
        project_id: String,
        progress: Option<String>,
    },
    Failed {
        project_id: Option<String>,
        error: SharedString,
    },
}

pub(crate) struct CloneRepositoryModal {
    backend: Arc<dyn ThreadBackend>,
    workspace: WeakEntity<Workspace>,
    repository: Entity<Editor>,
    destination: Entity<Editor>,
    base: Option<String>,
    /// Whether the user edited the folder, so the repository no longer names it.
    destination_edited: bool,
    /// The folder mustr filled in last, so its own edit isn't taken for the user's (the
    /// editor's events come after the edit).
    filled_destination: Option<String>,
    state: CloneState,
    _follow: Option<Task<()>>,
    _tasks: Vec<Task<()>>,
    _subscriptions: Vec<Subscription>,
}

impl CloneRepositoryModal {
    fn new(
        backend: Arc<dyn ThreadBackend>,
        base: Option<String>,
        workspace: WeakEntity<Workspace>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        let repository = cx.new(|cx| {
            let mut editor = Editor::single_line(window, cx);
            editor.set_placeholder_text(
                "https://github.com/owner/name.git, or owner/name",
                window,
                cx,
            );
            editor
        });
        let destination = cx.new(|cx| {
            let mut editor = Editor::single_line(window, cx);
            editor.set_placeholder_text("The folder to clone into", window, cx);
            editor
        });
        let subscriptions = vec![
            cx.subscribe_in(
                &repository,
                window,
                |this, repository, event: &EditorEvent, window, cx| {
                    if let EditorEvent::BufferEdited = event
                        && !this.destination_edited
                    {
                        let text = repository.read(cx).text(cx);
                        let destination = if text.trim().is_empty() {
                            String::new()
                        } else {
                            default_destination(this.base.as_deref(), &text)
                        };
                        this.filled_destination = Some(destination.clone());
                        this.destination
                            .update(cx, |editor, cx| editor.set_text(destination, window, cx));
                    }
                },
            ),
            cx.subscribe(
                &destination,
                |this, destination, event: &EditorEvent, cx| {
                    if let EditorEvent::BufferEdited = event
                        && this.filled_destination.as_deref()
                            != Some(destination.read(cx).text(cx).as_str())
                    {
                        this.destination_edited = true;
                    }
                },
            ),
        ];
        Self {
            backend,
            workspace,
            repository,
            destination,
            base,
            destination_edited: false,
            filled_destination: None,
            state: CloneState::Idle,
            _follow: None,
            _tasks: Vec::new(),
            _subscriptions: subscriptions,
        }
    }

    #[cfg(test)]
    pub(crate) fn editors(&self) -> (Entity<Editor>, Entity<Editor>) {
        (self.repository.clone(), self.destination.clone())
    }

    /// What the modal says under the fields: the clone's progress, or why it failed.
    pub(crate) fn status(&self) -> Option<SharedString> {
        match &self.state {
            CloneState::Idle => None,
            CloneState::Starting => Some("Starting the clone…".into()),
            CloneState::Cloning { progress, .. } => Some(
                progress
                    .clone()
                    .unwrap_or_else(|| "Cloning…".to_string())
                    .into(),
            ),
            CloneState::Failed { error, .. } => Some(error.clone()),
        }
    }

    pub(crate) fn clone_repository(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if matches!(
            self.state,
            CloneState::Starting | CloneState::Cloning { .. }
        ) {
            return;
        }
        if let CloneState::Failed {
            project_id: Some(project_id),
            ..
        } = &self.state
        {
            let project_id = project_id.clone();
            self.retry(project_id, window, cx);
            return;
        }
        let repository = self.repository.read(cx).text(cx);
        let destination = self.destination.read(cx).text(cx);
        if repository.trim().is_empty() || destination.trim().is_empty() {
            self.state = CloneState::Failed {
                project_id: None,
                error: "Enter a repository and the folder to clone it into.".into(),
            };
            cx.notify();
            return;
        }
        if let Some(reason) = self.backend.blocked_reason(Access::SourceControlWrite) {
            self.state = CloneState::Failed {
                project_id: None,
                error: format!("Couldn't clone: {reason}").into(),
            };
            cx.notify();
            return;
        }
        let input = start_input(
            crate::commands::new_id(),
            &repository,
            &destination,
            chrono::Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Millis, true),
        );
        let project_id = input.project_id.clone();
        let start = self
            .backend
            .call::<rpc::ProjectCloneStart>(input, Access::SourceControlWrite);
        self.state = CloneState::Starting;
        self.follow(project_id.clone(), window, cx);
        cx.notify();
        self._tasks.push(cx.spawn_in(window, async move |this, cx| {
            let result = start.await;
            this.update(cx, |this, cx| {
                match result {
                    Ok(_) => {
                        if matches!(this.state, CloneState::Starting) {
                            this.state = CloneState::Cloning {
                                project_id,
                                progress: None,
                            };
                        }
                    }
                    Err(error) => {
                        this._follow = None;
                        this.state = CloneState::Failed {
                            project_id: None,
                            error: format!("Couldn't clone: {error:#}").into(),
                        };
                    }
                }
                cx.notify();
            })
            .ok();
        }));
    }

    fn retry(&mut self, project_id: String, window: &mut Window, cx: &mut Context<Self>) {
        let retry = self.backend.call::<rpc::ProjectCloneRetry>(
            ProjectCloneActionInput {
                project_id: project_id.clone(),
            },
            Access::SourceControlWrite,
        );
        self.state = CloneState::Cloning {
            project_id: project_id.clone(),
            progress: None,
        };
        self.follow(project_id, window, cx);
        cx.notify();
        self._tasks.push(cx.spawn(async move |this, cx| {
            if let Err(error) = retry.await {
                this.update(cx, |this, cx| {
                    this.state = CloneState::Failed {
                        project_id: None,
                        error: format!("Couldn't retry the clone: {error:#}").into(),
                    };
                    cx.notify();
                })
                .ok();
            }
        }));
    }

    fn cancel(&mut self, _: &menu::Cancel, _: &mut Window, cx: &mut Context<Self>) {
        // Closing the modal while it clones cancels the clone, so none runs on unseen.
        if let CloneState::Cloning { project_id, .. } = &self.state {
            let cancel = self.backend.call::<rpc::ProjectCloneCancel>(
                ProjectCloneActionInput {
                    project_id: project_id.clone(),
                },
                Access::SourceControlWrite,
            );
            cx.background_spawn(async move {
                if let Err(error) = cancel.await {
                    log::warn!("couldn't cancel the clone: {error:#}");
                }
            })
            .detach();
        }
        cx.emit(DismissEvent);
    }

    fn confirm(&mut self, _: &menu::Confirm, window: &mut Window, cx: &mut Context<Self>) {
        self.clone_repository(window, cx);
    }

    /// Follows `subscribeProjectClones` for `project_id` until the clone ends.
    fn follow(&mut self, project_id: String, window: &mut Window, cx: &mut Context<Self>) {
        let stream = self.backend.stream::<rpc::SubscribeProjectClones>(
            ProjectCloneSubscribeInput(Default::default()),
            Access::Read,
        );
        self._follow = Some(cx.spawn_in(window, async move |this, cx| {
            let mut stream = match stream {
                Ok(stream) => stream,
                Err(error) => {
                    log::warn!("couldn't follow the clone: {error:#}");
                    return;
                }
            };
            while let Some(chunk) = stream.next().await {
                let Ok(chunk) = chunk else {
                    continue;
                };
                let Some(snapshot) = chunk
                    .0
                    .into_iter()
                    .filter(|snapshot| snapshot.project_id == project_id)
                    .max_by_key(|snapshot| snapshot.sequence)
                else {
                    continue;
                };
                let finished = this.update_in(cx, |this, window, cx| {
                    let finished = this.cloned(snapshot, window, cx);
                    cx.notify();
                    finished
                });
                if !matches!(finished, Ok(false)) {
                    return;
                }
            }
        }));
    }

    /// Takes a clone's snapshot. Returns whether the clone has ended.
    fn cloned(
        &mut self,
        snapshot: ProjectCloneSnapshot,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> bool {
        match snapshot.phase {
            ProjectClonePhase::Running | ProjectClonePhase::Unrecognized(_) => {
                self.state = CloneState::Cloning {
                    progress: Some(progress_summary(&snapshot)),
                    project_id: snapshot.project_id,
                };
                false
            }
            ProjectClonePhase::Done => {
                open_new_thread(snapshot.project_id, self.workspace.clone(), window, cx);
                cx.emit(DismissEvent);
                true
            }
            ProjectClonePhase::Failed => {
                self.state = CloneState::Failed {
                    error: snapshot
                        .error
                        .unwrap_or_else(|| "The clone failed.".to_string())
                        .into(),
                    project_id: Some(snapshot.project_id),
                };
                true
            }
            ProjectClonePhase::Cancelled => {
                self.state = CloneState::Failed {
                    error: "The clone was cancelled.".into(),
                    project_id: None,
                };
                true
            }
        }
    }
}

/// Opens a new thread in the cloned project once the shell has it, as T3 does.
fn open_new_thread(
    project_id: String,
    workspace: WeakEntity<Workspace>,
    window: &mut Window,
    cx: &mut App,
) {
    let shells = ThreadShells::global(cx);
    window
        .spawn(cx, async move |cx| {
            let step = Duration::from_millis(100);
            let mut waited = Duration::ZERO;
            while waited < PROJECT_WAIT {
                let has_project = cx
                    .update(|_, cx| {
                        shells
                            .read(cx)
                            .shell()
                            .is_some_and(|shell| shell.projects.contains_key(&project_id))
                    })
                    .unwrap_or(false);
                if has_project {
                    break;
                }
                cx.background_executor().timer(step).await;
                waited += step;
            }
            cx.update(|window, cx| {
                workspace
                    .update(cx, |_, cx| {
                        window.dispatch_action(Box::new(NewThreadInProject { project_id }), cx)
                    })
                    .ok();
            })
            .ok();
        })
        .detach();
}

impl EventEmitter<DismissEvent> for CloneRepositoryModal {}

impl ModalView for CloneRepositoryModal {}

impl Focusable for CloneRepositoryModal {
    fn focus_handle(&self, cx: &App) -> FocusHandle {
        self.repository.focus_handle(cx)
    }
}

impl Render for CloneRepositoryModal {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let palette = palette(cx);
        let field = |label: &'static str, editor: Entity<Editor>| {
            v_flex()
                .gap_1()
                .child(Label::new(label).size(LabelSize::Small).color(Color::Muted))
                .child(
                    div()
                        .px_2()
                        .py_1()
                        .rounded(scaled(tokens::CONTROL_RADIUS))
                        .border(tokens::BORDER_WIDTH)
                        .border_color(palette.border.hsla())
                        .bg(palette.surface.hsla())
                        .child(editor),
                )
        };
        let busy = matches!(
            self.state,
            CloneState::Starting | CloneState::Cloning { .. }
        );
        let failed = matches!(self.state, CloneState::Failed { .. });
        let retry = matches!(
            self.state,
            CloneState::Failed {
                project_id: Some(_),
                ..
            }
        );
        let status = self.status();
        v_flex()
            .key_context("CloneRepository")
            .on_action(cx.listener(Self::confirm))
            .on_action(cx.listener(Self::cancel))
            .w(scaled(px(520.)))
            .elevation_2(cx)
            .p_3()
            .gap_3()
            .child(
                v_flex()
                    .gap_0p5()
                    .child(Label::new("Clone a repository").weight(FontWeight::SEMIBOLD))
                    .child(
                        Label::new(
                            "T3 clones it with its own git and credentials, adds it as a \
                             project, and opens a new thread in it.",
                        )
                        .size(LabelSize::Small)
                        .color(Color::Muted),
                    ),
            )
            .child(field("Repository", self.repository.clone()))
            .child(field("Clone into", self.destination.clone()))
            .when_some(status, |this, status| {
                this.child(Label::new(status).size(LabelSize::Small).color(if failed {
                    Color::Error
                } else {
                    Color::Muted
                }))
            })
            .child(
                h_flex()
                    .justify_end()
                    .gap_2()
                    .child(
                        TextButton::new("clone-repository-cancel", "Cancel").on_click(cx.listener(
                            |this, _, window, cx| this.cancel(&menu::Cancel, window, cx),
                        )),
                    )
                    .child(
                        TextButton::new(
                            "clone-repository-clone",
                            if busy {
                                "Cloning…"
                            } else if retry {
                                "Retry"
                            } else {
                                "Clone"
                            },
                        )
                        .style(MustrButtonStyle::Primary)
                        .disabled(busy)
                        .on_click(
                            cx.listener(|this, _, window, cx| this.clone_repository(window, cx)),
                        ),
                    ),
            )
    }
}

pub(crate) fn init(cx: &mut App) {
    cx.observe_new(|workspace: &mut Workspace, _, _| {
        workspace.register_action(|workspace, _: &CloneRepository, window, cx| {
            let Some(backend) = crate::backend(cx) else {
                log::error!("couldn't clone: mustr isn't connected to T3");
                return;
            };
            // T3's "Add project" base folder, from the server config's settings.
            let base = crate::usage::ServerConfigs::try_global(cx).and_then(|configs| {
                configs
                    .read(cx)
                    .settings()
                    .and_then(|settings| settings.add_project_base_directory.clone())
            });
            let handle = workspace.weak_handle();
            workspace.toggle_modal(window, cx, move |window, cx| {
                CloneRepositoryModal::new(backend, base, handle, window, cx)
            });
        });
    })
    .detach();
}

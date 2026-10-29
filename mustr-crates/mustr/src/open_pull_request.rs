//! Opening any GitHub pull request in mustr's review, not only those of T3's projects: a result
//! of a section's GitHub search, or a link (or `owner/repo#123`) pasted in the Review page's box
//! or in "mustr: open pull request".
//!
//! T3 reads and writes any pull request on a host when the reference names the host
//! (`PullRequestRef.host`): its `requireProject` (T3's `PullRequestService.ts`) routes a
//! repository that isn't a project's to a project on that host, swapping the repository in, and
//! on github.com, with no project there, reads through the given project with the host's account
//! (`readsPublicHostWithoutCheckout`). The account is chosen by host, never by the project's
//! folder. So mustr picks the context project ([`route`]): the repository's own project if T3
//! has it, else one on the host, else (github.com only) any project. Everything the review does
//! after that (files, threads, comments, reviews, merging) carries the same reference, so writes
//! take the same route. When T3 can't (no project, a private repository its account can't see),
//! mustr says why and offers to open it on GitHub.

use crate::{
    github_search::GITHUB_HOST, inbox::decode_each, pull_request_review,
    t3_connection::T3Connection,
};
use anyhow::Result;
use editor::Editor;
use gpui::{
    App, AppContext as _, Context, DismissEvent, Entity, EventEmitter, FocusHandle, Focusable,
    IntoElement, ParentElement as _, Render, SharedString, Styled as _, Task, WeakEntity, Window,
    actions, rems,
};
use mustr_ui::{ButtonStyle, TextButton, palette, tokens, tokens::scaled};
use serde::Deserialize;
use serde_json::Value;
use t3_client::PullRequestRef;
use ui::{Color, Label, LabelCommon as _, LabelSize, prelude::*};
use url::Url;
use workspace::{ModalView, Workspace};

actions!(
    mustr,
    [
        /// Opens a GitHub pull request in mustr's review, by its link or as owner/repo#123, even
        /// one outside T3's projects.
        OpenPullRequest,
    ]
);

/// Adds "mustr: open pull request". Call once at startup.
pub fn init(cx: &mut App) {
    cx.observe_new(|workspace: &mut Workspace, _, _| {
        workspace.register_action(|workspace, _: &OpenPullRequest, window, cx| {
            OpenPullRequestDialog::toggle(workspace, window, cx)
        });
    })
    .detach();
}

/// A pull request named by a link or `owner/repo#123`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PullRequestLink {
    /// Lower case, without `www.`.
    pub host: String,
    /// `owner/name`, as written.
    pub repository: String,
    pub number: i64,
}

impl PullRequestLink {
    pub fn new(host: &str, repository: &str, number: i64) -> Self {
        Self {
            host: host.to_lowercase(),
            repository: repository.to_string(),
            number,
        }
    }

    /// `owner/repo#123`.
    pub fn display_ref(&self) -> String {
        format!("{}#{}", self.repository, self.number)
    }

    /// Its page on the forge.
    pub fn web_url(&self) -> String {
        format!(
            "https://{}/{}/pull/{}",
            self.host, self.repository, self.number
        )
    }
}

fn is_name(text: &str) -> bool {
    !text.is_empty()
        && text
            .chars()
            .all(|character| character.is_ascii_alphanumeric() || "._-".contains(character))
}

fn number(text: &str) -> Option<i64> {
    text.parse::<i64>().ok().filter(|number| *number > 0)
}

/// Reads a pull request's link (`https://github.com/owner/repo/pull/123`, with or without the
/// scheme, a tab such as `/files`, or a `#` anchor), `owner/repo#123`, or `owner/repo/pull/123`.
/// Links to other hosts (GitHub Enterprise) keep their host.
pub fn parse_link(text: &str) -> Option<PullRequestLink> {
    let text = text.trim().trim_start_matches('<').trim_end_matches('>');
    if text.is_empty() || text.contains(char::is_whitespace) {
        return None;
    }
    if let Some((repository, number_text)) = text.split_once('#')
        && !text.contains("://")
        && let Some((owner, name)) = repository.split_once('/')
        && is_name(owner)
        && is_name(name)
    {
        return Some(PullRequestLink::new(
            GITHUB_HOST,
            repository,
            number(number_text)?,
        ));
    }
    let with_scheme = if text.contains("://") {
        text.to_string()
    } else if text
        .split('/')
        .next()
        .is_some_and(|first| first.contains('.'))
    {
        format!("https://{text}")
    } else {
        format!("https://{GITHUB_HOST}/{text}")
    };
    let url = Url::parse(&with_scheme).ok()?;
    if !matches!(url.scheme(), "http" | "https") {
        return None;
    }
    let host = url.host_str()?.to_lowercase();
    let host = host.strip_prefix("www.").unwrap_or(&host).to_string();
    let segments = url.path_segments()?.collect::<Vec<_>>();
    match segments.as_slice() {
        [owner, name, "pull" | "pulls", number_text, ..] if is_name(owner) && is_name(name) => {
            Some(PullRequestLink::new(
                &host,
                &format!("{owner}/{name}"),
                number(number_text)?,
            ))
        }
        _ => None,
    }
}

/// One of T3's projects, as routing needs it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct T3Project {
    pub id: String,
    pub title: String,
    /// Its forge's host (`github.com`), from its repository's identity.
    pub host: Option<String>,
    /// `owner/name`.
    pub repository: Option<String>,
    /// T3's name for its forge (`github`, `gitlab`, …).
    pub provider: Option<String>,
}

impl T3Project {
    pub fn is_on(&self, host: &str) -> bool {
        self.host
            .as_deref()
            .is_some_and(|own| own.eq_ignore_ascii_case(host))
    }

    pub fn is_repository(&self, host: &str, repository: &str) -> bool {
        self.is_on(host)
            && self
                .repository
                .as_deref()
                .is_some_and(|own| own.eq_ignore_ascii_case(repository))
    }
}

/// The projects of T3's shell snapshot (`GET /api/orchestration/shell`). A repository's identity
/// has a `canonicalKey` of `host/owner/name` and a `displayName` of `owner/name`.
pub fn projects_from_shell(snapshot: &Value) -> Vec<T3Project> {
    #[derive(Deserialize)]
    #[serde(rename_all = "camelCase")]
    struct Identity {
        #[serde(default)]
        canonical_key: Option<String>,
        #[serde(default)]
        display_name: Option<String>,
        #[serde(default)]
        provider: Option<String>,
    }

    #[derive(Deserialize)]
    #[serde(rename_all = "camelCase")]
    struct Project {
        id: String,
        title: String,
        #[serde(default)]
        deleted_at: Option<String>,
        #[serde(default)]
        repository_identity: Option<Identity>,
    }

    decode_each::<Project>(snapshot.get("projects"), "project")
        .into_iter()
        .filter(|project| project.deleted_at.is_none())
        .map(|project| {
            let identity = project.repository_identity;
            let key = identity
                .as_ref()
                .and_then(|identity| identity.canonical_key.clone());
            let (host, path) = match key.as_deref().and_then(|key| key.split_once('/')) {
                Some((host, path)) => (Some(host.to_lowercase()), Some(path.to_string())),
                None => (None, None),
            };
            T3Project {
                id: project.id,
                title: project.title,
                host,
                repository: identity
                    .as_ref()
                    .and_then(|identity| identity.display_name.clone())
                    .or(path),
                provider: identity.and_then(|identity| identity.provider),
            }
        })
        .collect()
}

/// The lower-cased `owner/name` of T3's projects on github.com.
pub fn github_repositories(projects: &[T3Project]) -> collections::HashSet<String> {
    projects
        .iter()
        .filter(|project| project.is_on(GITHUB_HOST))
        .filter_map(|project| project.repository.as_ref())
        .map(|repository| repository.to_lowercase())
        .collect()
}

/// Why T3 can't be asked for a pull request.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum RouteError {
    /// T3 has no projects to read it through.
    NoProjects,
    /// T3 has no project on this host, which isn't github.com.
    NoProjectOnHost(String),
}

impl RouteError {
    pub fn message(&self) -> String {
        match self {
            Self::NoProjects => "T3 opens a pull request through one of its projects, and it has \
                 none yet. Add a project in the Inbox, or open it on GitHub."
                .to_string(),
            Self::NoProjectOnHost(host) => {
                format!("T3 has no project on {host}, so it can't read this pull request.")
            }
        }
    }
}

/// The reference T3 reads (and writes) `link` with: the host, and as context the repository's own
/// project, else a project on the host (GitHub-backed first), else on github.com any project,
/// as T3's `requireProject` routes them.
pub fn route(link: &PullRequestLink, projects: &[T3Project]) -> Result<PullRequestRef, RouteError> {
    let on_host = || projects.iter().filter(|project| project.is_on(&link.host));
    let project = on_host()
        .find(|project| project.is_repository(&link.host, &link.repository))
        .or_else(|| on_host().find(|project| project.provider.as_deref() == Some("github")))
        .or_else(|| on_host().next())
        .or_else(|| {
            (link.host == GITHUB_HOST)
                .then(|| projects.first())
                .flatten()
        });
    let Some(project) = project else {
        return Err(if projects.is_empty() {
            RouteError::NoProjects
        } else {
            RouteError::NoProjectOnHost(link.host.clone())
        });
    };
    Ok(PullRequestRef {
        allow_stale: None,
        expected_account_id: None,
        project_id: project.id.clone(),
        host: Some(link.host.clone()),
        repository: link.repository.clone(),
        number: link.number,
    })
}

/// Why a pull request didn't open, and where it can be seen instead.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct OpenError {
    pub message: SharedString,
    /// Its page on GitHub, for Open on GitHub.
    pub url: String,
}

/// What T3's refusal means, in words: T3's errors arrive as JSON (`_tag`, `reason`, `detail`)
/// inside the HTTP or RPC error.
pub fn explain(error: &anyhow::Error, link: &PullRequestLink) -> String {
    let text = format!("{error:#}");
    let body = text
        .char_indices()
        .filter(|(_, character)| *character == '{')
        .find_map(|(start, _)| serde_json::from_str::<Value>(&text[start..]).ok());
    let field = |name: &str| {
        body.as_ref()
            .and_then(|body| body.get(name))
            .and_then(Value::as_str)
    };
    let display = link.display_ref();
    match (field("_tag"), field("reason")) {
        (_, Some("not-found")) => format!(
            "GitHub couldn't find {display}, or T3's GitHub account can't see it (a private \
             repository, or one of an organization that hasn't let it in)."
        ),
        (Some("PullRequestUnavailableError"), Some("cli-unauthenticated")) => format!(
            "T3 isn't signed in to {}. Sign in with gh auth login in Terminal on T3's machine \
             (Settings › Agents and threads › Source control).",
            link.host
        ),
        (Some("PullRequestUnavailableError"), Some("cli-missing")) => format!(
            "T3 can't reach {}: the GitHub CLI isn't installed on T3's machine.",
            link.host
        ),
        (Some("PullRequestUnavailableError"), _) => {
            format!("T3 can't open pull requests on {}.", link.host)
        }
        (Some("EnvironmentAuthorizationError"), _) => {
            "mustr's pairing with T3 doesn't allow reading pull requests. Pair again from the \
             Inbox."
                .to_string()
        }
        _ => match field("detail").or_else(|| field("message")) {
            Some(detail) => format!("{display}: {detail}"),
            None => format!("{display}: {text}"),
        },
    }
}

/// Opens `link` in mustr's review through T3. `projects` are T3's projects when the caller has
/// them; otherwise they're read from T3's shell.
pub fn open_link(
    link: PullRequestLink,
    projects: Option<Vec<T3Project>>,
    workspace: WeakEntity<Workspace>,
    window: &mut Window,
    cx: &mut App,
) -> Task<Result<(), OpenError>> {
    let failed = {
        let url = link.web_url();
        move |message: String| OpenError {
            message: message.into(),
            url: url.clone(),
        }
    };
    let Some(session) = T3Connection::global(cx).read(cx).session() else {
        return Task::ready(Err(failed(
            "mustr isn't connected to T3 Code, which opens pull requests.".to_string(),
        )));
    };
    window.spawn(cx, async move |cx| {
        let projects = match projects {
            Some(projects) => projects,
            None => {
                let snapshot = cx
                    .background_spawn(async move { session.shell_snapshot().await })
                    .await
                    .map_err(|error| failed(format!("Couldn't read T3's projects: {error:#}")))?;
                projects_from_shell(&snapshot)
            }
        };
        let reference = route(&link, &projects).map_err(|error| failed(error.message()))?;
        log::info!(
            "Opening {} through T3's project {} on {}",
            link.display_ref(),
            reference.project_id,
            link.host
        );
        let open = cx
            .update(|window, cx| pull_request_review::open_review(workspace, reference, window, cx))
            .map_err(|error| failed(format!("{error:#}")))?;
        open.await.map_err(|error| {
            log::error!("couldn't open {}: {error:#}", link.display_ref());
            failed(explain(&error, &link))
        })
    })
}

/// "mustr: open pull request": a box for a link or `owner/repo#123`.
pub struct OpenPullRequestDialog {
    workspace: WeakEntity<Workspace>,
    editor: Entity<Editor>,
    error: Option<OpenError>,
    opening: bool,
    _task: Option<Task<()>>,
}

impl OpenPullRequestDialog {
    pub fn toggle(workspace: &mut Workspace, window: &mut Window, cx: &mut Context<Workspace>) {
        let weak = workspace.weak_handle();
        // A pull request's link on the clipboard is what's wanted, most of the time.
        let copied = cx
            .read_from_clipboard()
            .and_then(|clipboard| clipboard.text())
            .filter(|text| parse_link(text).is_some());
        workspace.toggle_modal(window, cx, move |window, cx| {
            Self::new(weak, copied, window, cx)
        });
    }

    pub fn new(
        workspace: WeakEntity<Workspace>,
        text: Option<String>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        let editor = cx.new(|cx| {
            let mut editor = Editor::single_line(window, cx);
            editor.set_placeholder_text(
                "https://github.com/owner/repo/pull/123, or owner/repo#123",
                window,
                cx,
            );
            if let Some(text) = text {
                editor.set_text(text.trim().to_string(), window, cx);
                editor.select_all(&editor::actions::SelectAll, window, cx);
            }
            editor
        });
        window.focus(&editor.focus_handle(cx), cx);
        Self {
            workspace,
            editor,
            error: None,
            opening: false,
            _task: None,
        }
    }

    fn confirm(&mut self, _: &menu::Confirm, window: &mut Window, cx: &mut Context<Self>) {
        self.open(window, cx);
    }

    pub fn open(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.opening {
            return;
        }
        let Some(link) = parse_link(&self.editor.read(cx).text(cx)) else {
            self.error = Some(OpenError {
                message: "That isn't a pull request's link. Paste one like \
                          https://github.com/owner/repo/pull/123, or type owner/repo#123."
                    .into(),
                url: String::new(),
            });
            cx.notify();
            return;
        };
        self.error = None;
        self.opening = true;
        cx.notify();
        let open = open_link(link, None, self.workspace.clone(), window, cx);
        self._task = Some(cx.spawn(async move |this, cx| {
            let result = open.await;
            this.update(cx, |this, cx| {
                this.opening = false;
                match result {
                    Ok(()) => cx.emit(DismissEvent),
                    Err(error) => this.error = Some(error),
                }
                cx.notify();
            })
            .ok();
        }));
    }

    #[cfg(test)]
    pub fn error(&self) -> Option<&OpenError> {
        self.error.as_ref()
    }
}

impl EventEmitter<DismissEvent> for OpenPullRequestDialog {}

impl ModalView for OpenPullRequestDialog {}

impl Focusable for OpenPullRequestDialog {
    fn focus_handle(&self, cx: &App) -> FocusHandle {
        self.editor.focus_handle(cx)
    }
}

impl Render for OpenPullRequestDialog {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let palette = palette(cx);
        let error = self.error.clone();
        v_flex()
            .key_context("MustrOpenPullRequest")
            .on_action(cx.listener(Self::confirm))
            .on_action(cx.listener(|_, _: &menu::Cancel, _, cx| cx.emit(DismissEvent)))
            .w(tokens::modal_width(rems(36.), window))
            .p_4()
            .gap_3()
            .rounded(scaled(tokens::CORNER_RADIUS))
            .border(tokens::BORDER_WIDTH)
            .border_color(palette.border.hsla())
            .bg(palette.surface.hsla())
            .shadow_lg()
            .child(
                v_flex()
                    .gap_0p5()
                    .child(
                        Label::new("Open a pull request")
                            .size(LabelSize::Large)
                            .weight(gpui::FontWeight::SEMIBOLD),
                    )
                    .child(
                        Label::new(
                            "Any pull request on GitHub, in your projects or not. T3 Code reads \
                             it with its GitHub account.",
                        )
                        .size(LabelSize::Small)
                        .color(Color::Muted),
                    ),
            )
            .child(
                div()
                    .px_2()
                    .py_1p5()
                    .rounded(scaled(tokens::CONTROL_RADIUS))
                    .border(tokens::BORDER_WIDTH)
                    .border_color(palette.border.hsla())
                    .bg(palette.background.hsla())
                    .child(self.editor.clone()),
            )
            .when_some(error.clone(), |this, error| {
                this.child(
                    Label::new(error.message)
                        .size(LabelSize::Small)
                        .color(Color::Error),
                )
            })
            .child(
                h_flex()
                    .justify_end()
                    .gap_2()
                    .when_some(
                        error.filter(|error| !error.url.is_empty()),
                        |this, error| {
                            this.child(
                                TextButton::new("open-pull-request-on-github", "Open on GitHub")
                                    .on_click(move |_, _, cx| cx.open_url(&error.url)),
                            )
                        },
                    )
                    .child(
                        TextButton::new("open-pull-request-cancel", "Cancel")
                            .on_click(cx.listener(|_, _, _, cx| cx.emit(DismissEvent))),
                    )
                    .child(
                        TextButton::new(
                            "open-pull-request-open",
                            if self.opening { "Opening…" } else { "Open" },
                        )
                        .style(ButtonStyle::Primary)
                        .disabled(self.opening)
                        .on_click(cx.listener(|this, _, window, cx| this.open(window, cx))),
                    ),
            )
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        pull_request_review::run_action_input, review_backend::T3ReviewBackend,
        review_drafts::DraftStore, test_rpc::RecordingRpc,
    };
    use gpui::TestAppContext;
    use pretty_assertions::assert_eq;
    use review::{ReviewBackend as _, ReviewSubmission, ReviewVerdict};
    use serde_json::json;
    use std::sync::Arc;

    fn link(repository: &str, number: i64) -> Option<PullRequestLink> {
        Some(PullRequestLink::new(GITHUB_HOST, repository, number))
    }

    #[test]
    fn links_and_short_references_name_a_pull_request() {
        for text in [
            "https://github.com/zed-industries/zed/pull/41234",
            "  https://github.com/zed-industries/zed/pull/41234/files#diff-abc ",
            "http://www.github.com/zed-industries/zed/pull/41234?w=1",
            "github.com/zed-industries/zed/pull/41234",
            "<https://github.com/zed-industries/zed/pull/41234>",
            "zed-industries/zed#41234",
            "zed-industries/zed/pull/41234",
        ] {
            assert_eq!(
                parse_link(text),
                link("zed-industries/zed", 41234),
                "{text}"
            );
        }
        assert_eq!(
            parse_link("https://ghe.acme.dev/platform/api/pull/7/commits"),
            Some(PullRequestLink::new("ghe.acme.dev", "platform/api", 7))
        );
        for text in [
            "",
            "fix the crash",
            "zed-industries/zed",
            "zed-industries/zed#",
            "zed-industries/zed#0",
            "https://github.com/zed-industries/zed/issues/12",
            "https://github.com/zed-industries/zed/pull/abc",
            "owner/repo#12 and more",
            "ftp://github.com/a/b/pull/1",
        ] {
            assert_eq!(parse_link(text), None, "{text}");
        }
        let parsed = link("Acme/API", 9).unwrap();
        assert_eq!(parsed.web_url(), "https://github.com/Acme/API/pull/9");
        assert_eq!(parsed.display_ref(), "Acme/API#9");
    }

    fn shell() -> Value {
        let project = |id: &str, title: &str, key: Option<&str>, provider: &str| {
            json!({
                "id": id, "title": title, "workspaceRoot": format!("/src/{title}"),
                "repositoryIdentity": key.map(|key| json!({
                    "canonicalKey": key,
                    "displayName": key.split_once('/').unwrap().1,
                    "provider": provider,
                    "locator": { "source": "git-remote", "remoteName": "origin",
                                 "remoteUrl": format!("https://{key}.git") },
                })),
            })
        };
        json!({
            "projects": [
                project("scratch", "scratch", None, ""),
                project("lab", "lab", Some("gitlab.com/acme/lab"), "gitlab"),
                project("mustr", "mustr", Some("github.com/msetaro/mustr"), "github"),
                project("api", "api", Some("github.com/acme/api"), "github"),
                { "id": "gone", "title": "gone", "deletedAt": "2026-10-01T00:00:00Z" },
            ],
        })
    }

    #[test]
    fn t3s_projects_come_from_its_shell() {
        let projects = projects_from_shell(&shell());
        assert_eq!(
            projects
                .iter()
                .map(|project| (
                    project.id.as_str(),
                    project.host.as_deref(),
                    project.repository.as_deref()
                ))
                .collect::<Vec<_>>(),
            vec![
                ("scratch", None, None),
                ("lab", Some("gitlab.com"), Some("acme/lab")),
                ("mustr", Some("github.com"), Some("msetaro/mustr")),
                ("api", Some("github.com"), Some("acme/api")),
            ]
        );
        assert_eq!(
            github_repositories(&projects),
            ["msetaro/mustr".to_string(), "acme/api".to_string()]
                .into_iter()
                .collect()
        );
    }

    #[test]
    fn any_pull_request_routes_through_a_project_with_its_host() {
        let projects = projects_from_shell(&shell());
        let routed = |link: &PullRequestLink, projects: &[T3Project]| {
            route(link, projects).map(|reference| reference.project_id)
        };
        // Its own project, matched without regard to case.
        assert_eq!(
            routed(&link("ACME/api", 3).unwrap(), &projects),
            Ok("api".to_string())
        );
        // Another repository on GitHub goes through a GitHub project, with the host, as T3
        // routes it.
        let reference = route(&link("zed-industries/zed", 41234).unwrap(), &projects).unwrap();
        assert_eq!(
            serde_json::to_value(&reference).unwrap(),
            json!({
                "projectId": "mustr", "host": "github.com",
                "repository": "zed-industries/zed", "number": 41234,
            })
        );
        // Without a GitHub project, any project does on github.com (T3 reads it with the
        // host's account, without a checkout).
        let local = &projects[..2];
        assert_eq!(
            routed(&link("zed-industries/zed", 1).unwrap(), local),
            Ok("scratch".to_string())
        );
        // Not elsewhere: T3 only reads public github.com without a project on the host.
        assert_eq!(
            routed(
                &PullRequestLink::new("ghe.acme.dev", "platform/api", 7),
                &projects
            ),
            Err(RouteError::NoProjectOnHost("ghe.acme.dev".to_string()))
        );
        assert_eq!(
            routed(&link("zed-industries/zed", 1).unwrap(), &[]),
            Err(RouteError::NoProjects)
        );
    }

    /// The review's writes go through the same route: T3 gets the host, the context project
    /// and the other repository in every write. Recorded, never sent.
    #[gpui::test]
    async fn writes_take_the_same_route(cx: &mut TestAppContext) {
        let reference = route(
            &link("zed-industries/zed", 41234).unwrap(),
            &projects_from_shell(&shell()),
        )
        .unwrap();
        let rpc = Arc::new(RecordingRpc::default());
        let dir = tempfile::tempdir().unwrap();
        let backend = T3ReviewBackend::new(
            rpc.clone(),
            reference.clone(),
            DraftStore::new(dir.path(), &reference),
        );
        cx.run_until_parked();
        let target = json!({
            "projectId": "mustr", "host": "github.com",
            "repository": "zed-industries/zed", "number": 41234,
        });
        let with_target = |extra: Value| {
            let mut payload = target.clone();
            payload
                .as_object_mut()
                .unwrap()
                .extend(extra.as_object().unwrap().clone());
            payload
        };

        cx.update(|cx| backend.post_comment("Looks good.".to_string(), cx))
            .await
            .unwrap();
        for verdict in [ReviewVerdict::Approve, ReviewVerdict::RequestChanges] {
            cx.update(|cx| {
                backend.submit_review(
                    ReviewSubmission {
                        quotes: Default::default(),
                        verdict,
                        body: "Review.".to_string(),
                        comments: Vec::new(),
                    },
                    cx,
                )
            })
            .await
            .unwrap();
        }
        let calls = rpc.calls.lock().clone();
        assert_eq!(
            calls,
            vec![
                (
                    "pullRequests.comment".to_string(),
                    with_target(json!({ "body": "Looks good." }))
                ),
                (
                    "pullRequests.submitReview".to_string(),
                    with_target(json!({ "body": "Review.", "comments": [], "verdict": "approve" }))
                ),
                (
                    "pullRequests.submitReview".to_string(),
                    with_target(
                        json!({ "body": "Review.", "comments": [], "verdict": "request-changes" })
                    )
                ),
            ]
        );
        // Merging, from the review's header.
        let merge = run_action_input(&reference, rpc_action("merge"), Some(merge_method()));
        assert_eq!(
            serde_json::to_value(&merge).unwrap(),
            with_target(json!({ "action": "merge", "mergeMethod": "squash" }))
        );
    }

    fn rpc_action(action: &str) -> t3_client::t3_api::types::PullRequestAction {
        serde_json::from_value(json!(action)).unwrap()
    }

    fn merge_method() -> t3_client::t3_api::types::PullRequestMergeMethod {
        serde_json::from_value(json!("squash")).unwrap()
    }

    /// "mustr: open pull request" takes a link; it says what's wrong with one it can't read,
    /// and why one didn't open, with the pull request's page on GitHub.
    #[gpui::test]
    async fn the_dialog_opens_links(cx: &mut TestAppContext) {
        cx.update(|cx| {
            let store = settings::SettingsStore::test(cx);
            cx.set_global(store);
            theme_settings::init(theme::LoadThemes::JustBase, cx);
            crate::t3_connection::init_for_test(crate::t3_engine::T3Mode::Installed, cx);
        });
        let (dialog, cx) = cx.add_window_view(|window, cx| {
            OpenPullRequestDialog::new(
                WeakEntity::new_invalid(),
                Some("not a link".to_string()),
                window,
                cx,
            )
        });
        dialog.update_in(cx, |dialog, window, cx| dialog.open(window, cx));
        let error = dialog
            .read_with(cx, |dialog, _| dialog.error().cloned())
            .unwrap();
        assert!(
            error
                .message
                .starts_with("That isn't a pull request's link.")
        );
        assert_eq!(error.url, "");

        dialog.update_in(cx, |dialog, window, cx| {
            dialog.editor.update(cx, |editor, cx| {
                editor.set_text(
                    "https://github.com/zed-industries/zed/pull/41234/files",
                    window,
                    cx,
                )
            });
            dialog.open(window, cx)
        });
        cx.run_until_parked();
        assert_eq!(
            dialog.read_with(cx, |dialog, _| dialog.error().cloned()),
            Some(OpenError {
                message: "mustr isn't connected to T3 Code, which opens pull requests.".into(),
                url: "https://github.com/zed-industries/zed/pull/41234".to_string(),
            })
        );
    }

    #[test]
    fn t3s_refusals_are_explained() {
        let zed = link("zed-industries/zed", 1).unwrap();
        let refused = |body: Value| {
            explain(
                &anyhow::anyhow!(
                    "http://127.0.0.1:3773/api/pull-requests/diff returned 502 Bad Gateway: \
                     {body}"
                )
                .context("loading zed-industries/zed#1"),
                &zed,
            )
        };
        assert!(
            refused(json!({
                "_tag": "PullRequestOperationError", "operation": "diff", "reason": "not-found",
                "detail": "GitHub could not find the requested resource, or the credential cannot see it.",
            }))
            .starts_with("GitHub couldn't find zed-industries/zed#1, or T3's GitHub account can't see it")
        );
        assert!(
            refused(json!({
                "_tag": "PullRequestUnavailableError", "reason": "cli-unauthenticated",
            }))
            .contains("gh auth login in Terminal on T3's machine")
        );
        assert_eq!(
            refused(json!({
                "_tag": "PullRequestOperationError", "operation": "diff",
                "detail": "The pull request is too large.",
            })),
            "zed-industries/zed#1: The pull request is too large."
        );
        assert_eq!(
            explain(
                &anyhow::anyhow!("zed-industries/zed#1 has no changed files"),
                &zed
            ),
            "zed-industries/zed#1: zed-industries/zed#1 has no changed files"
        );
    }
}

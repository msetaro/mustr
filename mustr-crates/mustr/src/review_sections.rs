//! The Review page's pull request sections: each has a name and a GitHub-style query, such as
//! `is:open is:pr review-requested:@me`. They're mustr's `"review": {"sections"}` setting, which
//! the page's section menus and Settings › Review change (never by hand).
//!
//! A section lists its pull requests from one of two sources ([`section_source`]):
//! - **GitHub's search** (`github_search.rs`), across every repository the user's GitHub account
//!   can see, as github.com/pulls does. Queries that name people or places (`author:`,
//!   `involves:`, `review-requested:`, `org:`, a `repo:` outside T3's projects…) use it.
//! - **T3's `pullRequests.list`** across T3's projects ([`T3PullRequests`]): mustr turns what it
//!   can of the query into that call's `state`, `involvement` and `filters`, and applies the rest
//!   itself ([`PullRequestQuery::keeps`]). A section marked "Only T3's projects", one whose
//!   `repo:`s are all T3's projects, and one that narrows to no one and nowhere (which on GitHub
//!   would be every pull request there is) use it.

use crate::{
    github_search::{GitHubSearch, SEARCH_DOCS_URL, SearchError, count_label},
    inbox::{PullRequestEntry, PullRequestList, parse_pull_request_list},
    t3_connection::T3Session,
};
use anyhow::{Context as _, Result, anyhow};
use collections::HashSet;
use editor::{Editor, EditorEvent};
use futures::{FutureExt as _, StreamExt as _, future::BoxFuture};
use gpui::{
    App, AppContext as _, Context, DismissEvent, EventEmitter, FocusHandle, Focusable, Global,
    IntoElement, ParentElement as _, Render, Styled as _, Subscription, Task, Window, px, rems,
};
use mustr_ui::{ButtonStyle as MustrButtonStyle, Pill, TextButton, tokens, tokens::scaled};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::{sync::Arc, time::Duration};
use ui::{Color, ContextMenu, Label, LabelSize, PopoverMenu, Switch, Tooltip, prelude::*};
use workspace::{ModalView, Workspace};

/// The most pull requests a section lists.
const SECTION_LIMIT: u32 = 100;

/// A section of the Review page: its name, and the query that fills it.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct QuerySection {
    pub name: String,
    pub query: String,
    /// Lists only T3's projects' pull requests, through T3, even when the query would search
    /// GitHub.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub projects_only: bool,
}

impl QuerySection {
    pub fn new(name: &str, query: &str) -> Self {
        Self {
            name: name.to_string(),
            query: query.to_string(),
            projects_only: false,
        }
    }

    #[cfg(test)]
    pub fn projects_only(mut self) -> Self {
        self.projects_only = true;
        self
    }
}

/// The sections mustr starts with: GitHub-wide, as on github.com/pulls.
pub fn default_sections() -> Vec<QuerySection> {
    vec![
        QuerySection::new("Review requested", "is:open is:pr review-requested:@me"),
        QuerySection::new("Yours", "is:open is:pr author:@me"),
        QuerySection::new("Involved", "is:open is:pr involves:@me -author:@me"),
    ]
}

/// Where a section's pull requests come from.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SectionSource {
    /// T3's `pullRequests.list`, across T3's projects.
    T3Projects,
    /// GitHub's search, across every repository the account can see.
    GitHub,
}

impl SectionSource {
    pub fn label(self) -> &'static str {
        match self {
            Self::T3Projects => "T3's projects",
            Self::GitHub => "GitHub",
        }
    }

    pub fn description(self) -> &'static str {
        match self {
            Self::T3Projects => "Lists the pull requests of T3's projects, through T3 Code.",
            Self::GitHub => {
                "Searches all of GitHub, as github.com/pulls does, with the GitHub CLI's \
                 sign-in on this computer."
            }
        }
    }
}

/// The qualifiers that narrow a search to people or places. A query without one (or with only
/// their negations) would list every pull request on GitHub.
const SCOPES: &[&str] = &[
    "author",
    "assignee",
    "mentions",
    "commenter",
    "involves",
    "review-requested",
    "reviewed-by",
    "team-review-requested",
    "team",
    "repo",
    "org",
    "user",
];

/// Where a section's pull requests come from. `project_repositories` are T3's projects'
/// `owner/name`s on github.com, in lower case.
pub fn section_source(
    section: &QuerySection,
    project_repositories: &HashSet<String>,
) -> SectionSource {
    if section.projects_only {
        return SectionSource::T3Projects;
    }
    let scopes = tokens(&section.query)
        .into_iter()
        .filter(|token| !token.starts_with('-'))
        .filter_map(|token| {
            let (key, value) = token.split_once(':')?;
            let key = key.to_lowercase();
            SCOPES
                .contains(&key.as_str())
                .then(|| (key, value.trim_matches('"').to_lowercase()))
        })
        .collect::<Vec<_>>();
    if scopes.is_empty() {
        return SectionSource::T3Projects;
    }
    let only_projects = scopes
        .iter()
        .all(|(key, value)| key == "repo" && project_repositories.contains(value));
    if only_projects && parse_query(&section.query).unsupported.is_empty() {
        SectionSource::T3Projects
    } else {
        SectionSource::GitHub
    }
}

/// T3's projects' repositories on github.com (lower-cased `owner/name`), as the Review page last
/// read them, for deciding a section's source outside the page.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct ProjectRepositories(pub HashSet<String>);

impl Global for ProjectRepositories {}

impl ProjectRepositories {
    pub fn get(cx: &App) -> HashSet<String> {
        cx.try_global::<Self>()
            .map(|repositories| repositories.0.clone())
            .unwrap_or_default()
    }
}

/// `"review"` in mustr's settings file.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ReviewSettings {
    pub sections: Vec<QuerySection>,
}

impl Default for ReviewSettings {
    fn default() -> Self {
        Self {
            sections: default_sections(),
        }
    }
}

impl Global for ReviewSettings {}

impl ReviewSettings {
    /// The settings in mustr's settings file (JSON with comments). A file without `"review"`
    /// has the default sections.
    pub fn from_settings_text(text: &str) -> Result<Self> {
        if text.trim().is_empty() {
            return Ok(Self::default());
        }
        let settings: Value = settings::parse_json_with_comments(text)?;
        match settings
            .get("review")
            .and_then(|review| review.get("sections"))
        {
            None | Some(Value::Null) => Ok(Self::default()),
            Some(sections) => Ok(Self {
                sections: serde_json::from_value(sections.clone())
                    .context("\"review\" in mustr's settings")?,
            }),
        }
    }

    pub fn get(cx: &App) -> Self {
        cx.try_global::<Self>().cloned().unwrap_or_default()
    }
}

/// Moves section `index` up (`-1`) or down (`1`).
pub fn move_section(index: usize, offset: isize, cx: &mut App) {
    let mut sections = ReviewSettings::get(cx).sections;
    let Some(target) = index.checked_add_signed(offset) else {
        return;
    };
    if index < sections.len() && target < sections.len() {
        sections.swap(index, target);
        set_sections(sections, cx);
    }
}

/// Changes the sections, at once and in the settings file.
pub fn set_sections(sections: Vec<QuerySection>, cx: &mut App) {
    let value = serde_json::to_value(&sections).ok();
    cx.set_global(ReviewSettings { sections });
    crate::app_settings::update_mustr_setting(&["review", "sections"], value, cx);
    // The Settings page lists them.
    cx.refresh_windows();
}

/// Keeps [`ReviewSettings`] current with the settings file. An invalid one keeps the last
/// sections that parsed.
pub fn init(cx: &mut App) {
    let (mut contents, watcher) = settings::watch_config_file(
        cx.background_executor(),
        <dyn fs::Fs>::global(cx),
        paths::settings_file().clone(),
    );
    cx.spawn(async move |cx| {
        let _watcher = watcher;
        while let Some(text) = contents.next().await {
            match ReviewSettings::from_settings_text(&text) {
                Ok(settings) => cx.update(|cx| {
                    if cx.try_global::<ReviewSettings>() != Some(&settings) {
                        cx.set_global(settings)
                    }
                }),
                Err(error) => log::error!("couldn't read the review sections: {error:#}"),
            }
        }
    })
    .detach();
}

/// A pull request's state, in a query (`is:open`, the default).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum State {
    #[default]
    Open,
    Closed,
    Merged,
}

/// What a query asks for, from GitHub's search syntax.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct PullRequestQuery {
    pub state: State,
    /// `review-requested:@me`.
    pub review_requested: bool,
    /// `author:@me`.
    pub authored: bool,
    /// `-review-requested:@me` and `-author:@me`.
    pub not_review_requested: bool,
    pub not_authored: bool,
    /// `author:<login>`.
    pub author: Option<String>,
    /// `is:draft` (`Some(true)`), `-is:draft` (`Some(false)`).
    pub draft: Option<bool>,
    /// `review:approved|changes_requested|required|none`, as T3's filter names them.
    pub review: Option<&'static str>,
    /// `status:success|failure`, as T3's filter names them.
    pub checks: Option<&'static str>,
    pub labels: Vec<String>,
    pub excluded_labels: Vec<String>,
    /// `repo:owner/name`, and `org:owner` as `owner/`.
    pub repos: Vec<String>,
    /// The words that aren't qualifiers.
    pub text: String,
    /// The qualifiers mustr can't apply with T3's list.
    pub unsupported: Vec<String>,
}

/// Reads a GitHub-style query. Qualifiers mustr can't apply land in `unsupported`.
pub fn parse_query(query: &str) -> PullRequestQuery {
    let mut parsed = PullRequestQuery::default();
    let mut words = Vec::new();
    for token in tokens(query) {
        let (negated, token) = match token.strip_prefix('-') {
            Some(rest) if rest.contains(':') => (true, rest.to_string()),
            _ => (false, token),
        };
        let Some((key, value)) = token.split_once(':') else {
            words.push(token);
            continue;
        };
        let value = value.trim_matches('"').to_string();
        let lower = value.to_lowercase();
        let me = lower == "@me";
        match (key.to_lowercase().as_str(), negated) {
            ("is" | "type", false) if lower == "pr" => {}
            ("is" | "state", false) if lower == "open" => parsed.state = State::Open,
            ("is" | "state", false) if lower == "closed" => parsed.state = State::Closed,
            ("is", false) if lower == "merged" => parsed.state = State::Merged,
            ("is", false) if lower == "draft" => parsed.draft = Some(true),
            ("is", true) if lower == "draft" => parsed.draft = Some(false),
            ("draft", _) if lower == "true" || lower == "false" => {
                parsed.draft = Some((lower == "true") != negated)
            }
            ("review-requested", false) if me => parsed.review_requested = true,
            ("review-requested", true) if me => parsed.not_review_requested = true,
            ("author", false) if me => parsed.authored = true,
            ("author", true) if me => parsed.not_authored = true,
            ("author", false) => parsed.author = Some(value),
            ("review", false) => match lower.as_str() {
                "approved" => parsed.review = Some("approved"),
                "changes_requested" | "changes-requested" => {
                    parsed.review = Some("changes-requested")
                }
                "required" | "review-required" => parsed.review = Some("review-required"),
                "none" => parsed.review = Some("none"),
                _ => parsed.unsupported.push(format!("review:{value}")),
            },
            ("status", false) if lower == "success" => parsed.checks = Some("passing"),
            ("status", false) if lower == "failure" => parsed.checks = Some("failing"),
            ("label", false) => parsed.labels.push(value),
            ("label", true) => parsed.excluded_labels.push(value),
            ("repo", false) => parsed.repos.push(lower),
            ("org" | "user", false) => parsed.repos.push(format!("{lower}/")),
            _ => parsed
                .unsupported
                .push(format!("{}{key}:{value}", if negated { "-" } else { "" })),
        }
    }
    parsed.text = words.join(" ");
    parsed
}

/// A query's words, keeping quoted values (`label:"needs review"`) together.
fn tokens(query: &str) -> Vec<String> {
    let mut tokens = Vec::new();
    let mut current = String::new();
    let mut quoted = false;
    for character in query.chars() {
        match character {
            '"' => {
                quoted = !quoted;
                current.push(character);
            }
            character if character.is_whitespace() && !quoted => {
                if !current.is_empty() {
                    tokens.push(std::mem::take(&mut current));
                }
            }
            character => current.push(character),
        }
    }
    if !current.is_empty() {
        tokens.push(current);
    }
    tokens
}

impl PullRequestQuery {
    /// The `pullRequests.list` input that lists what T3 can of the query, across its projects.
    pub fn t3_input(&self) -> Value {
        let mut input = json!({
            "state": match self.state {
                State::Open => "open",
                State::Closed => "closed",
                State::Merged => "merged",
            },
            "involvement": match (self.review_requested, self.authored) {
                (true, false) => "reviewing",
                (false, true) => "authored",
                _ => "all",
            },
            "limit": SECTION_LIMIT,
        });
        let mut filters = serde_json::Map::new();
        if let Some(draft) = self.draft {
            filters.insert("draft".into(), json!(if draft { "only" } else { "hide" }));
        }
        if let Some(review) = self.review {
            filters.insert("review".into(), json!(review));
        }
        if let Some(checks) = self.checks {
            filters.insert("checks".into(), json!(checks));
        }
        if !self.labels.is_empty() {
            let groups = self
                .labels
                .iter()
                .map(|label| vec![label.clone()])
                .collect::<Vec<_>>();
            filters.insert("labels".into(), json!(groups));
        }
        if !self.excluded_labels.is_empty() {
            filters.insert("excludedLabels".into(), json!(self.excluded_labels));
        }
        if let Some(author) = &self.author {
            filters.insert("author".into(), json!(author));
        }
        if !filters.is_empty() {
            input["filters"] = Value::Object(filters);
        }
        input
    }

    /// Whether a pull request T3 listed matches the parts of the query T3's list doesn't apply:
    /// you as author or reviewer when both or their negations are asked, the repository, and
    /// the words.
    pub fn keeps(&self, entry: &PullRequestEntry, list: &PullRequestList) -> bool {
        let authored = entry.is_authored_by_viewer(&list.viewers);
        (!self.authored || authored)
            && (!self.review_requested || entry.viewer_review_requested)
            && !(self.not_authored && authored)
            && !(self.not_review_requested && entry.viewer_review_requested)
            && self.draft.is_none_or(|draft| entry.is_draft == draft)
            && (self.repos.is_empty()
                || self.repos.iter().any(|repo| {
                    let repository = entry.repository.to_lowercase();
                    match repo.strip_suffix('/') {
                        Some(owner) => repository.starts_with(&format!("{owner}/")),
                        None => repository == *repo,
                    }
                }))
            && entry.matches(&self.text)
    }
}

/// Where a section's pull requests come from.
pub trait PullRequestSource: Send + Sync {
    /// Lists the pull requests `query` finds. The caller applies [`PullRequestQuery::keeps`].
    fn list(&self, query: &PullRequestQuery) -> BoxFuture<'static, Result<PullRequestList>>;
}

/// T3's `pullRequests.list`, across all of T3's projects.
pub struct T3PullRequests(pub Arc<T3Session>);

impl PullRequestSource for T3PullRequests {
    fn list(&self, query: &PullRequestQuery) -> BoxFuture<'static, Result<PullRequestList>> {
        let session = self.0.clone();
        let input = query.t3_input();
        async move {
            let list = session
                .rpc()
                .request("pullRequests.list", input)
                .await
                .map_err(|error| anyhow!(error.error_detail().unwrap_or_else(|| error.to_string())))
                .context("listing pull requests")?;
            Ok(parse_pull_request_list(list))
        }
        .boxed()
    }
}

/// Examples for the query editor's help: a query, and what it lists.
pub const QUERY_EXAMPLES: &[(&str, &str)] = &[
    (
        "is:open is:pr review-requested:@me",
        "Waiting for your review",
    ),
    ("is:open is:pr author:@me", "Yours"),
    (
        "is:open is:pr involves:@me -author:@me",
        "Others' you're in: assigned, mentioned, commented",
    ),
    (
        "is:open is:pr org:my-org draft:false",
        "Ready ones in an organization",
    ),
    (
        "is:open is:pr repo:owner/name label:bug",
        "One repository's, with a label",
    ),
    (
        "is:merged is:pr author:@me sort:updated-desc",
        "Yours, merged",
    ),
];

/// What GitHub says of the query being edited.
#[derive(Clone, Debug, PartialEq, Eq)]
enum QueryCheck {
    /// Nothing asked: the query is T3's, or empty.
    None,
    Checking,
    Finds(u64),
    Refused(SearchError),
}

/// How long the query editor waits after typing before asking GitHub about the query.
const CHECK_DELAY: Duration = Duration::from_millis(800);

/// Adds a section, or changes one (its index), with a name and a query.
pub struct SectionDialog {
    editing: Option<usize>,
    name: gpui::Entity<Editor>,
    query: gpui::Entity<Editor>,
    projects_only: bool,
    error: Option<SharedString>,
    check: QueryCheck,
    _check_task: Option<Task<()>>,
    _subscription: Subscription,
}

/// Which field the dialog starts in.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum DialogFocus {
    Name,
    Query,
}

impl SectionDialog {
    /// Opens the dialog over the workspace: for section `editing`, or a new one.
    pub fn open(
        editing: Option<usize>,
        focus: DialogFocus,
        workspace: &mut Workspace,
        window: &mut Window,
        cx: &mut Context<Workspace>,
    ) {
        let section =
            editing.and_then(|index| ReviewSettings::get(cx).sections.get(index).cloned());
        workspace.toggle_modal(window, cx, move |window, cx| {
            Self::new(editing, section, focus, window, cx)
        });
    }

    pub fn new(
        editing: Option<usize>,
        section: Option<QuerySection>,
        focus: DialogFocus,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        let field = |text: &str, placeholder: &str, window: &mut Window, cx: &mut Context<Self>| {
            let text = text.to_string();
            let placeholder = placeholder.to_string();
            cx.new(|cx| {
                let mut editor = Editor::single_line(window, cx);
                editor.set_placeholder_text(&placeholder, window, cx);
                editor.set_text(text, window, cx);
                editor
            })
        };
        let name = field(
            section.as_ref().map_or("", |section| section.name.as_str()),
            "Name, such as Needs my review",
            window,
            cx,
        );
        let query = field(
            section
                .as_ref()
                .map_or("is:open is:pr ", |section| section.query.as_str()),
            "is:open is:pr review-requested:@me",
            window,
            cx,
        );
        let focused = match focus {
            DialogFocus::Name => name.focus_handle(cx),
            DialogFocus::Query => query.focus_handle(cx),
        };
        window.focus(&focused, cx);
        let subscription = cx.subscribe(&query, |this, _, event: &EditorEvent, cx| {
            if let EditorEvent::BufferEdited = event {
                this.schedule_check(CHECK_DELAY, cx);
            }
        });
        let mut this = Self {
            editing,
            name,
            query,
            projects_only: section.is_some_and(|section| section.projects_only),
            error: None,
            check: QueryCheck::None,
            _check_task: None,
            _subscription: subscription,
        };
        this.schedule_check(Duration::ZERO, cx);
        this
    }

    fn section(&self, cx: &App) -> QuerySection {
        QuerySection {
            name: self.name.read(cx).text(cx).trim().to_string(),
            query: self.query.read(cx).text(cx).trim().to_string(),
            projects_only: self.projects_only,
        }
    }

    fn source(&self, cx: &App) -> SectionSource {
        section_source(&self.section(cx), &ProjectRepositories::get(cx))
    }

    /// Asks GitHub how many pull requests the query finds (or why it refuses it) once typing
    /// pauses, when the section searches GitHub.
    fn schedule_check(&mut self, delay: Duration, cx: &mut Context<Self>) {
        let query = self.section(cx).query;
        if query.is_empty() || self.source(cx) != SectionSource::GitHub {
            self.check = QueryCheck::None;
            self._check_task = None;
            cx.notify();
            return;
        }
        let search = GitHubSearch::global(cx);
        self._check_task = Some(cx.spawn(async move |this, cx| {
            if !delay.is_zero() {
                cx.background_executor().timer(delay).await;
            }
            this.update(cx, |this, cx| {
                this.check = QueryCheck::Checking;
                cx.notify();
            })
            .ok();
            let result = search.check(&query).await;
            this.update(cx, |this, cx| {
                this.check = match result {
                    Ok(count) => QueryCheck::Finds(count),
                    Err(error) => QueryCheck::Refused(error),
                };
                cx.notify();
            })
            .ok();
        }));
    }

    fn set_projects_only(&mut self, projects_only: bool, cx: &mut Context<Self>) {
        self.projects_only = projects_only;
        self.schedule_check(Duration::ZERO, cx);
    }

    fn use_example(&mut self, query: &str, window: &mut Window, cx: &mut Context<Self>) {
        self.query.update(cx, |editor, cx| {
            editor.set_text(query.to_string(), window, cx)
        });
        if self.name.read(cx).text(cx).trim().is_empty()
            && let Some((_, name)) = QUERY_EXAMPLES.iter().find(|(example, _)| *example == query)
        {
            self.name.update(cx, |editor, cx| {
                editor.set_text(name.to_string(), window, cx)
            });
        }
    }

    fn save(&mut self, _: &menu::Confirm, _: &mut Window, cx: &mut Context<Self>) {
        let section = self.section(cx);
        if section.name.is_empty() || section.query.is_empty() {
            self.error = Some("A section needs a name and a query.".into());
            cx.notify();
            return;
        }
        let mut sections = ReviewSettings::get(cx).sections;
        match self.editing.filter(|index| *index < sections.len()) {
            Some(index) => sections[index] = section,
            None => sections.push(section),
        }
        set_sections(sections, cx);
        cx.emit(DismissEvent);
    }

    fn cancel(&mut self, _: &menu::Cancel, _: &mut Window, cx: &mut Context<Self>) {
        cx.emit(DismissEvent);
    }

    #[cfg(test)]
    pub fn query_editor(&self) -> gpui::Entity<Editor> {
        self.query.clone()
    }

    #[cfg(test)]
    pub fn status(&self, cx: &App) -> (SectionSource, String) {
        (self.source(cx), self.check_text().unwrap_or_default())
    }

    fn check_text(&self) -> Option<String> {
        match &self.check {
            QueryCheck::None => None,
            QueryCheck::Checking => Some("Asking GitHub…".to_string()),
            QueryCheck::Finds(1) => Some("GitHub finds 1 pull request.".to_string()),
            QueryCheck::Finds(count) => Some(format!(
                "GitHub finds {} pull requests.",
                count_label(*count)
            )),
            QueryCheck::Refused(error) => Some(error.message()),
        }
    }

    /// The query's help: examples to start from, and GitHub's guide to the syntax.
    fn render_help(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let this = cx.weak_entity();
        PopoverMenu::new("review-section-query-help")
            .trigger_with_tooltip(
                IconButton::new("review-section-query-help-button", IconName::Info)
                    .icon_size(IconSize::Small)
                    .icon_color(Color::Muted),
                Tooltip::text("Query syntax and examples"),
            )
            .anchor(gpui::Anchor::TopRight)
            .menu(move |window, cx| {
                let this = this.clone();
                Some(ContextMenu::build(window, cx, move |mut menu, _, _| {
                    menu = menu.header("GitHub's search syntax, as on github.com/pulls");
                    for (query, description) in QUERY_EXAMPLES {
                        let this = this.clone();
                        menu = menu.custom_entry(
                            move |_, _| {
                                v_flex()
                                    .child(Label::new(*query).size(LabelSize::Small))
                                    .child(
                                        Label::new(*description)
                                            .size(LabelSize::XSmall)
                                            .color(Color::Muted),
                                    )
                                    .into_any_element()
                            },
                            move |window, cx| {
                                this.update(cx, |this, cx| this.use_example(query, window, cx))
                                    .ok();
                            },
                        );
                    }
                    menu.separator()
                        .custom_row(|_, _| {
                            div()
                                .max_w(scaled(px(360.)))
                                .child(
                            Label::new(
                                "is:open, is:merged, is:draft, author:, assignee:, mentions:, \
                                 involves:, review-requested:, review:, status:, label:, org:, \
                                 repo:, sort:. A minus leaves out, as in -author:@me.",
                            )
                            .size(LabelSize::XSmall)
                            .color(Color::Muted),
                                )
                            .into_any_element()
                        })
                        .custom_entry(
                            |_, _| {
                                h_flex()
                                    .gap_1()
                                    .child(
                                        Label::new("GitHub's search syntax").size(LabelSize::Small),
                                    )
                                    .child(
                                        Icon::new(IconName::ArrowUpRight)
                                            .size(IconSize::XSmall)
                                            .color(Color::Muted),
                                    )
                                    .into_any_element()
                            },
                            |_, cx| cx.open_url(SEARCH_DOCS_URL),
                        )
                }))
            })
    }
}

impl EventEmitter<DismissEvent> for SectionDialog {}

impl ModalView for SectionDialog {}

impl Focusable for SectionDialog {
    fn focus_handle(&self, cx: &App) -> FocusHandle {
        self.name.focus_handle(cx)
    }
}

impl Render for SectionDialog {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let palette = mustr_ui::palette(cx);
        let field = |editor: &gpui::Entity<Editor>| {
            div()
                .px_2()
                .py_1p5()
                .rounded(scaled(tokens::CONTROL_RADIUS))
                .border(tokens::BORDER_WIDTH)
                .border_color(palette.border.hsla())
                .bg(palette.background.hsla())
                .child(editor.clone())
        };
        let caption =
            |text: &'static str| Label::new(text).size(LabelSize::Small).color(Color::Muted);
        let source = self.source(cx);
        let unsupported = match source {
            SectionSource::T3Projects => parse_query(&self.query.read(cx).text(cx)).unsupported,
            SectionSource::GitHub => Vec::new(),
        };
        let check_color = match &self.check {
            QueryCheck::Refused(_) => Color::Error,
            _ => Color::Muted,
        };
        let projects_only = self.projects_only;
        v_flex()
            .key_context("MustrReviewSection")
            .on_action(cx.listener(Self::save))
            .on_action(cx.listener(Self::cancel))
            .w(tokens::modal_width(rems(36.), window))
            .p_4()
            .gap_3()
            .rounded(scaled(tokens::CORNER_RADIUS))
            .border(tokens::BORDER_WIDTH)
            .border_color(palette.border.hsla())
            .bg(palette.surface.hsla())
            .shadow_lg()
            .child(
                Label::new(if self.editing.is_some() {
                    "Edit section"
                } else {
                    "Add a section"
                })
                .size(LabelSize::Large)
                .weight(gpui::FontWeight::SEMIBOLD),
            )
            .child(
                v_flex()
                    .gap_1()
                    .child(caption("Name"))
                    .child(field(&self.name)),
            )
            .child(
                v_flex()
                    .gap_1()
                    .child(
                        h_flex()
                            .justify_between()
                            .child(caption("Query"))
                            .child(self.render_help(cx)),
                    )
                    .child(field(&self.query))
                    .child(
                        h_flex()
                            .gap_1p5()
                            .child(Pill::tinted(source.label(), palette.accent.hsla()))
                            .child(
                                Label::new(source.description())
                                    .size(LabelSize::XSmall)
                                    .color(Color::Muted),
                            ),
                    )
                    .when_some(self.check_text(), |this, text| {
                        this.child(Label::new(text).size(LabelSize::XSmall).color(check_color))
                    })
                    .when(!unsupported.is_empty(), |this| {
                        this.child(
                            Label::new(format!(
                                "T3's list can't apply these: {}",
                                unsupported.join(", ")
                            ))
                            .size(LabelSize::XSmall)
                            .color(Color::Warning),
                        )
                    }),
            )
            .child(
                h_flex()
                    .gap_2()
                    .child(
                        Switch::new(
                            "review-section-projects-only",
                            if projects_only {
                                ToggleState::Selected
                            } else {
                                ToggleState::Unselected
                            },
                        )
                        .on_click(cx.listener(move |this, _, _, cx| {
                            this.set_projects_only(!projects_only, cx)
                        })),
                    )
                    .child(
                        v_flex()
                            .child(Label::new("Only T3's projects").size(LabelSize::Small))
                            .child(
                                Label::new(
                                    "List this query's pull requests in T3's projects only, \
                                     through T3 Code, instead of searching GitHub.",
                                )
                                .size(LabelSize::XSmall)
                                .color(Color::Muted),
                            ),
                    ),
            )
            .when_some(self.error.clone(), |this, error| {
                this.child(Label::new(error).size(LabelSize::Small).color(Color::Error))
            })
            .child(
                h_flex()
                    .justify_end()
                    .gap_2()
                    .child(TextButton::new("review-section-cancel", "Cancel").on_click(
                        |_, window, cx| window.dispatch_action(Box::new(menu::Cancel), cx),
                    ))
                    .child(
                        TextButton::new("review-section-save", "Save")
                            .style(MustrButtonStyle::Primary)
                            .on_click(|_, window, cx| {
                                window.dispatch_action(Box::new(menu::Confirm), cx)
                            }),
                    ),
            )
    }
}

/// Removes section `index`.
pub fn remove_section(index: usize, cx: &mut App) {
    let mut sections = ReviewSettings::get(cx).sections;
    if index < sections.len() {
        sections.remove(index);
        set_sections(sections, cx);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use pretty_assertions::assert_eq;

    #[test]
    fn queries_ask_t3_for_what_it_can() {
        let inputs = default_sections()
            .iter()
            .map(|section| parse_query(&section.query).t3_input())
            .collect::<Vec<_>>();
        assert_eq!(
            inputs,
            vec![
                json!({ "state": "open", "involvement": "reviewing", "limit": 100 }),
                json!({ "state": "open", "involvement": "authored", "limit": 100 }),
                json!({ "state": "open", "involvement": "all", "limit": 100 }),
            ]
        );
    }

    #[test]
    fn queries_map_to_t3s_filters() {
        let query = parse_query(
            r#"is:merged is:pr -is:draft review:changes_requested status:failure label:"needs review" -label:wip author:octocat repo:acme/api org:msetaro fix crash involves:@me"#,
        );
        assert_eq!(
            query.t3_input(),
            json!({
                "state": "merged",
                "involvement": "all",
                "limit": 100,
                "filters": {
                    "draft": "hide",
                    "review": "changes-requested",
                    "checks": "failing",
                    "labels": [["needs review"]],
                    "excludedLabels": ["wip"],
                    "author": "octocat",
                },
            })
        );
        assert_eq!(
            query.repos,
            vec!["acme/api".to_string(), "msetaro/".to_string()]
        );
        assert_eq!(query.text, "fix crash");
        assert_eq!(query.unsupported, vec!["involves:@me".to_string()]);
        // No `is:` means open, as GitHub's pull request page does.
        assert_eq!(parse_query("crash").state, State::Open);
    }

    #[test]
    fn what_t3_doesnt_filter_is_kept_or_left_out_here() {
        let list = parse_pull_request_list(json!({
            "viewers": { "github.com": "msetaro" },
            "entries": [
                { "projectId": "p", "host": "github.com", "repository": "msetaro/mustr",
                  "number": 1, "title": "Mine", "author": { "login": "MSetaro" },
                  "viewerReviewRequested": false },
                { "projectId": "p", "host": "github.com", "repository": "acme/api",
                  "number": 2, "title": "Theirs", "author": { "login": "octocat" },
                  "viewerReviewRequested": true },
                { "projectId": "p", "host": "github.com", "repository": "acme/web",
                  "number": 3, "title": "Other crash", "author": { "login": "octocat" },
                  "viewerReviewRequested": false },
            ],
        }));
        let kept = |query: &str| {
            let query = parse_query(query);
            list.entries
                .iter()
                .filter(|entry| query.keeps(entry, &list))
                .map(|entry| entry.number)
                .collect::<Vec<_>>()
        };
        assert_eq!(kept("is:open is:pr author:@me"), vec![1]);
        assert_eq!(kept("is:open is:pr review-requested:@me"), vec![2]);
        assert_eq!(kept("is:open -author:@me -review-requested:@me"), vec![3]);
        assert_eq!(kept("org:acme"), vec![2, 3]);
        assert_eq!(kept("repo:acme/web crash"), vec![3]);
    }

    #[test]
    fn sections_search_github_unless_they_stay_in_t3s_projects() {
        let projects = ["msetaro/mustr".to_string(), "acme/api".to_string()]
            .into_iter()
            .collect::<HashSet<_>>();
        let source = |query: &str| section_source(&QuerySection::new("s", query), &projects);
        // The defaults, and anything naming people or places beyond T3's projects.
        for section in default_sections() {
            assert_eq!(
                section_source(&section, &projects),
                SectionSource::GitHub,
                "{}",
                section.query
            );
        }
        assert_eq!(source("is:open assignee:@me"), SectionSource::GitHub);
        assert_eq!(source("is:open mentions:@me"), SectionSource::GitHub);
        assert_eq!(source("is:open org:zed-industries"), SectionSource::GitHub);
        assert_eq!(source("is:open repo:rust-lang/rust"), SectionSource::GitHub);
        assert_eq!(
            source("repo:acme/api repo:rust-lang/rust"),
            SectionSource::GitHub
        );
        // `repo:`s that are all T3's projects, and what T3 can filter, stay on T3's list...
        assert_eq!(
            source("is:open repo:ACME/api label:bug"),
            SectionSource::T3Projects
        );
        // ...unless T3 can't apply the rest.
        assert_eq!(
            source("is:open repo:acme/api involves:@me"),
            SectionSource::GitHub
        );
        // A query that narrows to no one and nowhere would be all of GitHub: T3's projects.
        assert_eq!(
            source("is:open is:pr -author:@me -review-requested:@me"),
            SectionSource::T3Projects
        );
        assert_eq!(source("crash"), SectionSource::T3Projects);
        // The switch keeps any query in T3's projects.
        assert_eq!(
            section_source(
                &QuerySection::new("s", "is:open involves:@me").projects_only(),
                &projects
            ),
            SectionSource::T3Projects
        );
    }

    #[test]
    fn only_t3s_projects_is_stored_when_set() {
        assert_eq!(
            serde_json::to_value(vec![
                QuerySection::new("Mine", "author:@me"),
                QuerySection::new("Ours", "involves:@me").projects_only(),
            ])
            .unwrap(),
            json!([
                { "name": "Mine", "query": "author:@me" },
                { "name": "Ours", "query": "involves:@me", "projects_only": true },
            ])
        );
        assert_eq!(
            ReviewSettings::from_settings_text(
                r#"{ "review": { "sections": [
                    { "name": "Ours", "query": "involves:@me", "projects_only": true }
                ] } }"#
            )
            .unwrap()
            .sections,
            vec![QuerySection::new("Ours", "involves:@me").projects_only()]
        );
    }

    #[gpui::test]
    async fn sections_move_up_and_down(cx: &mut gpui::TestAppContext) {
        let fs = fs::FakeFs::new(cx.executor());
        fs::Fs::create_dir(fs.as_ref(), paths::settings_file().parent().unwrap())
            .await
            .unwrap();
        cx.update(|cx| {
            let store = settings::SettingsStore::test(cx);
            cx.set_global(store);
            <dyn fs::Fs>::set_global(fs.clone(), cx);
        });
        let names = |cx: &App| {
            ReviewSettings::get(cx)
                .sections
                .into_iter()
                .map(|section| section.name)
                .collect::<Vec<_>>()
        };
        cx.update(|cx| {
            move_section(0, 1, cx);
            assert_eq!(names(cx), vec!["Yours", "Review requested", "Involved"]);
            move_section(2, -1, cx);
            assert_eq!(names(cx), vec!["Yours", "Involved", "Review requested"]);
            // Past either end, nothing moves.
            move_section(0, -1, cx);
            move_section(2, 1, cx);
            assert_eq!(names(cx), vec!["Yours", "Involved", "Review requested"]);
        });
        cx.run_until_parked();
        // In the settings file, where the Settings page keeps them.
        let text = fs::Fs::load(fs.as_ref(), paths::settings_file())
            .await
            .unwrap();
        assert_eq!(
            ReviewSettings::from_settings_text(&text).unwrap().sections,
            vec![
                QuerySection::new("Yours", "is:open is:pr author:@me"),
                QuerySection::new("Involved", "is:open is:pr involves:@me -author:@me"),
                QuerySection::new("Review requested", "is:open is:pr review-requested:@me"),
            ]
        );
    }

    /// The query editor says where a query lists from, and asks GitHub (once typing pauses)
    /// how many pull requests it finds, or why GitHub refuses it.
    #[gpui::test]
    async fn the_query_editor_checks_queries_with_github(cx: &mut gpui::TestAppContext) {
        let gh = Arc::new(crate::github_search::RecordingGh::default());
        cx.update(|cx| {
            let store = settings::SettingsStore::test(cx);
            cx.set_global(store);
            theme_settings::init(theme::LoadThemes::JustBase, cx);
            GitHubSearch::set_global(Arc::new(GitHubSearch::new(gh.clone())), cx);
        });
        let (dialog, cx) = cx.add_window_view(|window, cx| {
            SectionDialog::new(None, None, DialogFocus::Query, window, cx)
        });
        cx.run_until_parked();
        // "is:open is:pr " names no one: T3's projects, and GitHub isn't asked.
        let status =
            |cx: &mut gpui::VisualTestContext| dialog.read_with(cx, |dialog, cx| dialog.status(cx));
        assert_eq!(status(cx), (SectionSource::T3Projects, String::new()));
        let query = dialog.read_with(cx, |dialog, _| dialog.query_editor());
        let type_query = |text: &str, cx: &mut gpui::VisualTestContext| {
            query.update_in(cx, |editor, window, cx| editor.set_text(text, window, cx));
            cx.executor().advance_clock(CHECK_DELAY);
            cx.run_until_parked();
        };

        gh.answer_json(json!({ "total_count": 22, "items": [] }));
        type_query("is:open is:pr involves:@me", cx);
        assert_eq!(
            status(cx),
            (
                SectionSource::GitHub,
                "GitHub finds 22 pull requests.".to_string()
            )
        );
        assert_eq!(
            gh.queries(),
            vec!["is:open is:pr involves:@me archived:false sort:updated-desc"]
        );

        gh.answer(Ok(crate::github_search::GhOutput {
            code: Some(1),
            stdout: br#"{"message":"Validation Failed","errors":[{"message":"The listed users and repositories cannot be searched either because the resources do not exist or you do not have permission to view them."}]}"#.to_vec(),
            stderr: b"gh: Validation Failed (HTTP 422)".to_vec(),
        }));
        type_query("is:open repo:nobody/nothing", cx);
        assert_eq!(
            status(cx).1,
            "GitHub: The listed users and repositories cannot be searched either because the \
             resources do not exist or you do not have permission to view them."
        );

        // Only T3's projects: no more asking GitHub.
        dialog.update(cx, |dialog, cx| dialog.set_projects_only(true, cx));
        cx.run_until_parked();
        assert_eq!(status(cx), (SectionSource::T3Projects, String::new()));
        assert_eq!(gh.calls.lock().len(), 2);
    }

    #[test]
    fn sections_come_from_the_settings_file() {
        assert_eq!(
            ReviewSettings::from_settings_text("{}").unwrap(),
            ReviewSettings::default()
        );
        assert_eq!(
            ReviewSettings::from_settings_text(
                r#"{ "review": { "sections": [{ "name": "Mine", "query": "author:@me" }] } }"#
            )
            .unwrap()
            .sections,
            vec![QuerySection::new("Mine", "author:@me")]
        );
    }
}

//! The Review page's pull request sections: each has a name and a GitHub-style query, such as
//! `is:open is:pr review-requested:@me`. They're mustr's `"review": {"sections"}` setting, which
//! the page's section menus and Settings › Review change (never by hand).
//!
//! A [`PullRequestSource`] lists what a query finds. Today's only source is T3's
//! `pullRequests.list` across all of T3's projects ([`T3PullRequests`]): mustr turns what it can of
//! the query into that call's `state`, `involvement` and `filters`, and applies the rest itself
//! ([`PullRequestQuery::keeps`]). A source that searches GitHub itself can be added beside it.

use crate::{
    inbox::{PullRequestEntry, PullRequestList, parse_pull_request_list},
    t3_connection::T3Session,
};
use anyhow::{Context as _, Result, anyhow};
use editor::Editor;
use futures::{FutureExt as _, StreamExt as _, future::BoxFuture};
use gpui::{
    App, AppContext as _, Context, DismissEvent, EventEmitter, FocusHandle, Focusable, Global,
    IntoElement, ParentElement as _, Render, Styled as _, Window, px,
};
use mustr_ui::{tokens, tokens::scaled};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::sync::Arc;
use ui::{Button, ButtonStyle, Color, Label, LabelSize, prelude::*};
use workspace::{ModalView, Workspace};

/// The most pull requests a section lists.
const SECTION_LIMIT: u32 = 100;

/// A section of the Review page: its name, and the query that fills it.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct QuerySection {
    pub name: String,
    pub query: String,
}

impl QuerySection {
    pub fn new(name: &str, query: &str) -> Self {
        Self {
            name: name.to_string(),
            query: query.to_string(),
        }
    }
}

/// The sections mustr starts with.
pub fn default_sections() -> Vec<QuerySection> {
    vec![
        QuerySection::new("Review requested", "is:open is:pr review-requested:@me"),
        QuerySection::new("Yours", "is:open is:pr author:@me"),
        QuerySection::new(
            "Other open pull requests",
            "is:open is:pr -author:@me -review-requested:@me",
        ),
    ]
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

/// Adds a section, or changes one (its index), with a name and a query.
pub struct SectionDialog {
    editing: Option<usize>,
    name: gpui::Entity<Editor>,
    query: gpui::Entity<Editor>,
    error: Option<SharedString>,
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
            let field =
                |text: &str, placeholder: &str, window: &mut Window, cx: &mut Context<Self>| {
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
            Self {
                editing,
                name,
                query,
                error: None,
            }
        });
    }

    fn save(&mut self, _: &menu::Confirm, _: &mut Window, cx: &mut Context<Self>) {
        let name = self.name.read(cx).text(cx).trim().to_string();
        let query = self.query.read(cx).text(cx).trim().to_string();
        if name.is_empty() || query.is_empty() {
            self.error = Some("A section needs a name and a query.".into());
            cx.notify();
            return;
        }
        let mut sections = ReviewSettings::get(cx).sections;
        let section = QuerySection { name, query };
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
}

impl EventEmitter<DismissEvent> for SectionDialog {}

impl ModalView for SectionDialog {}

impl Focusable for SectionDialog {
    fn focus_handle(&self, cx: &App) -> FocusHandle {
        self.name.focus_handle(cx)
    }
}

impl Render for SectionDialog {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let palette = mustr_ui::palette(cx);
        let field = |label: &'static str, editor: &gpui::Entity<Editor>| {
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
                        .child(editor.clone()),
                )
        };
        let unsupported = parse_query(&self.query.read(cx).text(cx)).unsupported;
        v_flex()
            .key_context("MustrReviewSection")
            .on_action(cx.listener(Self::save))
            .on_action(cx.listener(Self::cancel))
            .w(scaled(px(480.)))
            .elevation_2(cx)
            .p_3()
            .gap_2()
            .child(
                Label::new(if self.editing.is_some() {
                    "Edit section"
                } else {
                    "Add a section"
                })
                .weight(gpui::FontWeight::SEMIBOLD),
            )
            .child(field("Name", &self.name))
            .child(field("Query", &self.query))
            .child(
                Label::new(
                    "GitHub's search syntax: is:open, is:draft, author:@me, review-requested:@me, \
                     review:approved, status:failure, label:…, repo:owner/name, and words. A minus \
                     leaves out, as in -author:@me.",
                )
                .size(LabelSize::XSmall)
                .color(Color::Muted),
            )
            .when(!unsupported.is_empty(), |this| {
                this.child(
                    Label::new(format!(
                        "mustr can't apply these with T3's list yet: {}",
                        unsupported.join(", ")
                    ))
                    .size(LabelSize::XSmall)
                    .color(Color::Warning),
                )
            })
            .when_some(self.error.clone(), |this, error| {
                this.child(Label::new(error).size(LabelSize::Small).color(Color::Error))
            })
            .child(
                h_flex()
                    .justify_end()
                    .gap_2()
                    .child(Button::new("review-section-cancel", "Cancel").on_click(
                        |_, window, cx| window.dispatch_action(Box::new(menu::Cancel), cx),
                    ))
                    .child(
                        Button::new("review-section-save", "Save")
                            .style(ButtonStyle::Filled)
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
    fn the_default_sections_ask_t3_for_what_it_can() {
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

//! The Review page's GitHub search: a section's query run the way github.com/pulls runs it, across
//! every repository the user's GitHub account can see.
//!
//! T3 can't do this: `pullRequests.list` quotes its `query` as a phrase and always adds `repo:`
//! qualifiers for T3's own projects (`searchQuery` in T3's `GitHubPullRequestApi.ts`). So mustr
//! runs GitHub's search on this computer with the user's GitHub CLI login, read-only: a GraphQL
//! `search(type: ISSUE)` through `gh api graphql`, and, when a query finds nothing, REST's
//! `search/issues` (a GET) to learn whether GitHub refused it (a bad qualifier, a repository it
//! can't see), which GraphQL's search doesn't say. mustr never runs `gh auth` or anything that
//! writes ([`is_read_only`] guards every command).
//!
//! The results come back in the shape of T3's list entries ([`PullRequestEntry`]), so the page
//! shows and opens them the same way. First pages are cached per query ([`GitHubSearch`]), so
//! drawing the page never spends GitHub's rate limit; the page refreshes them when it opens, on a
//! timer, and on demand.

use crate::inbox::{PullRequestEntry, PullRequestList};
use collections::HashMap;
use futures::{FutureExt as _, future::BoxFuture};
use gpui::{App, BackgroundExecutor, Global};
use parking_lot::Mutex;
use serde::Deserialize;
use serde_json::{Value, json};
use std::{
    io,
    path::PathBuf,
    sync::Arc,
    time::{Duration, Instant},
};

/// The host GitHub search runs on.
pub const GITHUB_HOST: &str = "github.com";

/// Pull requests a page of results holds, as on github.com/pulls.
pub const PAGE_SIZE: u32 = 25;

/// GitHub's guide to the search syntax for issues and pull requests.
pub const SEARCH_DOCS_URL: &str = "https://docs.github.com/en/search-github/searching-on-github/searching-issues-and-pull-requests";

/// How long `gh` may take to answer.
const GH_TIMEOUT: Duration = Duration::from_secs(30);

/// One page of a search, with each pull request's checks rollup and review requests. `@me` in
/// the query is the signed-in account.
const SEARCH_QUERY: &str = "query($q: String!, $first: Int!, $after: String) {
  viewer { login }
  search(query: $q, type: ISSUE, first: $first, after: $after) {
    issueCount
    pageInfo { hasNextPage endCursor }
    nodes {
      ... on PullRequest {
        number title url isDraft state createdAt updatedAt
        repository { nameWithOwner }
        author { login }
        headRefName baseRefName reviewDecision additions deletions
        labels(first: 20) { nodes { name color } }
        reviewRequests(first: 20) {
          nodes { requestedReviewer { ... on User { login } ... on Team { slug } } }
        }
        commits(last: 1) { nodes { commit { statusCheckRollup { state } } } }
      }
    }
  }
}";

/// What running `gh` gave.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct GhOutput {
    /// The exit code; `None` when a signal ended it.
    pub code: Option<i32>,
    pub stdout: Vec<u8>,
    pub stderr: Vec<u8>,
}

impl GhOutput {
    pub fn succeeded(&self) -> bool {
        self.code == Some(0)
    }
}

/// Runs `gh` with arguments. The real one is [`GhCli`]; tests record the arguments and answer
/// from fixtures.
pub trait GhRunner: Send + Sync {
    fn run(&self, args: Vec<String>) -> BoxFuture<'static, io::Result<GhOutput>>;
}

/// Whether a `gh` command only reads: `gh api graphql` with a query (no mutation), or a `GET`
/// of GitHub's search API. Anything else is refused before it runs.
pub fn is_read_only(args: &[String]) -> bool {
    let mut args = args.iter().map(String::as_str);
    if args.next() != Some("api") {
        return false;
    }
    let rest = args.collect::<Vec<_>>();
    if rest.contains(&"graphql") {
        let mut queries = rest.iter().filter_map(|arg| arg.strip_prefix("query="));
        let read = queries.next().is_some_and(|query| {
            query.trim_start().starts_with("query") && !query.contains("mutation")
        });
        return read
            && queries.next().is_none()
            && !rest
                .iter()
                .any(|arg| matches!(*arg, "-X" | "--method" | "--input"));
    }
    let method = rest
        .iter()
        .position(|arg| matches!(*arg, "-X" | "--method"))
        .and_then(|index| rest.get(index + 1));
    method == Some(&"GET")
        && rest.iter().any(|arg| arg.starts_with("search/"))
        && !rest.contains(&"--input")
}

/// Where `gh` is: on `PATH` (mustr loads the login shell's), or where Homebrew and the
/// installers put it.
fn find_gh() -> Option<PathBuf> {
    let name = if cfg!(windows) { "gh.exe" } else { "gh" };
    let path = std::env::var_os("PATH").unwrap_or_default();
    std::env::split_paths(&path)
        .chain(
            ["/opt/homebrew/bin", "/usr/local/bin", "/usr/bin"]
                .into_iter()
                .map(PathBuf::from),
        )
        .map(|directory| directory.join(name))
        .find(|candidate| candidate.is_file())
}

/// The user's `gh`, run without a shell, a prompt, or an update check.
pub struct GhCli {
    executor: BackgroundExecutor,
}

impl GhCli {
    pub fn new(executor: BackgroundExecutor) -> Self {
        Self { executor }
    }
}

impl GhRunner for GhCli {
    fn run(&self, args: Vec<String>) -> BoxFuture<'static, io::Result<GhOutput>> {
        let executor = self.executor.clone();
        async move {
            if !is_read_only(&args) {
                return Err(io::Error::other("mustr only reads from GitHub"));
            }
            let gh = find_gh().ok_or_else(|| io::Error::from(io::ErrorKind::NotFound))?;
            let mut command = util::command::new_command(gh);
            command
                .args(&args)
                .env("GH_PROMPT_DISABLED", "1")
                .env("GH_NO_UPDATE_NOTIFIER", "1")
                .env("GH_SPINNER_DISABLED", "1")
                .env("NO_COLOR", "1")
                .stdin(util::command::Stdio::null());
            let output = futures::select! {
                output = command.output().fuse() => output?,
                _ = executor.timer(GH_TIMEOUT).fuse() => {
                    return Err(io::Error::new(
                        io::ErrorKind::TimedOut,
                        format!("gh didn't answer within {}s", GH_TIMEOUT.as_secs()),
                    ));
                }
            };
            Ok(GhOutput {
                code: output.status.code(),
                stdout: output.stdout,
                stderr: output.stderr,
            })
        }
        .boxed()
    }
}

/// Why a search found nothing to show.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum SearchError {
    /// The GitHub CLI isn't installed (or isn't where mustr looks).
    CliMissing,
    /// The GitHub CLI isn't signed in to github.com, or GitHub refused its login.
    SignedOut,
    /// GitHub's rate limit for the account is used up.
    RateLimited,
    /// GitHub refused the query, in its words: a bad qualifier, a repository it can't see.
    Refused(String),
    /// `gh` failed some other way.
    Failed(String),
}

impl SearchError {
    pub fn message(&self) -> String {
        match self {
            Self::CliMissing => "mustr searches GitHub with the GitHub CLI (gh), which isn't \
                 installed. Install it from cli.github.com, then sign in with gh auth login in \
                 Terminal."
                .to_string(),
            Self::SignedOut => {
                "The GitHub CLI isn't signed in. Sign in with gh auth login in Terminal."
                    .to_string()
            }
            Self::RateLimited => "GitHub's rate limit for your account is used up for now. \
                 mustr tries again on the next refresh."
                .to_string(),
            Self::Refused(message) => format!("GitHub: {message}"),
            Self::Failed(message) => format!("Couldn't search GitHub: {message}"),
        }
    }
}

impl std::fmt::Display for SearchError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(&self.message())
    }
}

/// One page of a search's results.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct SearchPage {
    /// The pull requests, in T3's entry shape, and the signed-in account as the viewer on
    /// github.com.
    pub list: PullRequestList,
    /// How many the query finds in all.
    pub total: u64,
    /// The cursor of the next page, if there is one.
    pub next_cursor: Option<String>,
}

/// A count as GitHub shows one: `1,391`.
pub fn count_label(count: u64) -> String {
    let digits = count.to_string();
    let mut label = String::new();
    for (index, digit) in digits.chars().enumerate() {
        if index > 0 && (digits.len() - index) % 3 == 0 {
            label.push(',');
        }
        label.push(digit);
    }
    label
}

/// A section's query as github.com/pulls runs it: pull requests only (`is:pr` implied), not in
/// archived repositories, newest updates first, unless the query says otherwise.
pub fn github_query(query: &str) -> String {
    let words = query.split_whitespace().collect::<Vec<_>>();
    let has = |qualifiers: &[&str]| {
        words.iter().any(|word| {
            let word = word.trim_start_matches('-').to_lowercase();
            qualifiers
                .iter()
                .any(|qualifier| word.starts_with(qualifier))
        })
    };
    let mut query = words.join(" ");
    let mut add = |qualifier: &str| {
        if !query.is_empty() {
            query.push(' ');
        }
        query.push_str(qualifier);
    };
    if !has(&["is:pr", "type:pr", "is:issue", "type:issue"]) {
        add("is:pr");
    }
    if !has(&["archived:"]) {
        add("archived:false");
    }
    if !has(&["sort:"]) {
        add("sort:updated-desc");
    }
    query
}

/// `gh api graphql` for a page of `query`'s results (a GitHub query, [`github_query`]).
pub fn search_args(query: &str, after: Option<&str>) -> Vec<String> {
    let mut args = vec![
        "api".to_string(),
        "graphql".to_string(),
        "--hostname".to_string(),
        GITHUB_HOST.to_string(),
        "-f".to_string(),
        format!("query={SEARCH_QUERY}"),
        "-f".to_string(),
        format!("q={query}"),
        "-F".to_string(),
        format!("first={PAGE_SIZE}"),
    ];
    if let Some(after) = after {
        args.extend(["-f".to_string(), format!("after={after}")]);
    }
    args
}

/// `GET search/issues` for one result: GitHub's REST search says why it refuses a query, which
/// the GraphQL search doesn't (it finds nothing instead).
pub fn check_args(query: &str) -> Vec<String> {
    [
        "api",
        "--hostname",
        GITHUB_HOST,
        "-X",
        "GET",
        "search/issues",
        "-f",
        &format!("q={query}"),
        "-f",
        "per_page=1",
    ]
    .into_iter()
    .map(str::to_string)
    .collect()
}

#[derive(Deserialize)]
struct Nodes<T> {
    #[serde(default = "Vec::new")]
    nodes: Vec<Option<T>>,
}

impl<T> Nodes<T> {
    fn into_iter(nodes: Option<Self>) -> impl Iterator<Item = T> {
        nodes
            .map(|nodes| nodes.nodes)
            .unwrap_or_default()
            .into_iter()
            .flatten()
    }
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct SearchNode {
    number: u64,
    title: String,
    #[serde(default)]
    url: Option<String>,
    #[serde(default)]
    is_draft: bool,
    #[serde(default)]
    state: Option<String>,
    #[serde(default)]
    created_at: Option<String>,
    #[serde(default)]
    updated_at: Option<String>,
    repository: Repository,
    #[serde(default)]
    author: Option<Login>,
    #[serde(default)]
    head_ref_name: Option<String>,
    #[serde(default)]
    base_ref_name: Option<String>,
    #[serde(default)]
    review_decision: Option<String>,
    #[serde(default)]
    additions: Option<u64>,
    #[serde(default)]
    deletions: Option<u64>,
    #[serde(default)]
    labels: Option<Nodes<Label>>,
    #[serde(default)]
    review_requests: Option<Nodes<ReviewRequest>>,
    #[serde(default)]
    commits: Option<Nodes<CommitNode>>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct Repository {
    name_with_owner: String,
}

#[derive(Deserialize)]
struct Login {
    login: String,
}

#[derive(Deserialize)]
struct Label {
    name: String,
    #[serde(default)]
    color: Option<String>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct ReviewRequest {
    #[serde(default)]
    requested_reviewer: Option<Reviewer>,
}

#[derive(Deserialize)]
struct Reviewer {
    #[serde(default)]
    login: Option<String>,
}

#[derive(Deserialize)]
struct CommitNode {
    commit: Commit,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct Commit {
    #[serde(default)]
    status_check_rollup: Option<Rollup>,
}

#[derive(Deserialize)]
struct Rollup {
    state: String,
}

/// A search result as T3's list entry (`PullRequestListEntry`), without a project: GitHub's
/// search isn't T3's, so which project opens it is decided when it opens.
fn entry(node: SearchNode, viewer: Option<&str>) -> Option<PullRequestEntry> {
    let viewer_review_requested = viewer.is_some_and(|viewer| {
        Nodes::into_iter(node.review_requests)
            .filter_map(|request| request.requested_reviewer?.login)
            .any(|login| login.eq_ignore_ascii_case(viewer))
    });
    let checks = Nodes::into_iter(node.commits)
        .last()
        .and_then(|node| node.commit.status_check_rollup)
        .and_then(|rollup| match rollup.state.as_str() {
            "SUCCESS" => Some("passing"),
            "FAILURE" | "ERROR" => Some("failing"),
            "PENDING" | "EXPECTED" => Some("pending"),
            _ => None,
        });
    let review = node
        .review_decision
        .as_deref()
        .and_then(|decision| match decision {
            "APPROVED" => Some("approved"),
            "CHANGES_REQUESTED" => Some("changes-requested"),
            "REVIEW_REQUIRED" => Some("review-required"),
            _ => None,
        });
    let labels = Nodes::into_iter(node.labels)
        .map(|label| json!({ "name": label.name, "color": label.color }))
        .collect::<Vec<_>>();
    let mut value = json!({
        "projectId": "",
        "provider": "github",
        "host": GITHUB_HOST,
        "repository": node.repository.name_with_owner,
        "number": node.number,
        "title": node.title,
        "url": node.url,
        "author": node.author.map(|author| json!({ "login": author.login })),
        "headBranch": node.head_ref_name,
        "baseBranch": node.base_ref_name,
        "state": node.state.map(|state| state.to_lowercase()),
        "isDraft": node.is_draft,
        "additions": node.additions.unwrap_or_default(),
        "deletions": node.deletions.unwrap_or_default(),
        "createdAt": node.created_at,
        "updatedAt": node.updated_at,
        "viewerReviewRequested": viewer_review_requested,
        "labels": labels,
    });
    if let Some(review) = review {
        value["reviewDecision"] = json!(review);
    }
    if let Some(checks) = checks {
        value["checksState"] = json!(checks);
    }
    serde_json::from_value(value)
        .inspect_err(|error| log::warn!("skipping a search result mustr can't read: {error}"))
        .ok()
}

/// Reads a page of `gh api graphql`'s search. GitHub can answer with results and errors at once
/// (a repository behind SAML, say): the results win.
pub fn parse_search(output: &GhOutput) -> Result<SearchPage, SearchError> {
    let value = serde_json::from_slice::<Value>(&output.stdout).ok();
    let data = value.as_ref().and_then(|value| value.get("data"));
    let Some(search) = data
        .and_then(|data| data.get("search"))
        .filter(|search| !search.is_null())
    else {
        return Err(failure(output));
    };
    let viewer = data
        .and_then(|data| data.pointer("/viewer/login"))
        .and_then(Value::as_str)
        .map(str::to_string);
    let entries = search
        .get("nodes")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        // Issues come back as empty objects: the fragment only asks pull requests.
        .filter(|node| node.get("number").is_some())
        .filter_map(|node| {
            serde_json::from_value::<SearchNode>(node.clone())
                .inspect_err(|error| {
                    log::warn!("skipping a search result mustr can't read: {error}")
                })
                .ok()
        })
        .filter_map(|node| entry(node, viewer.as_deref()))
        .collect();
    let next_cursor = search
        .pointer("/pageInfo/hasNextPage")
        .and_then(Value::as_bool)
        .filter(|more| *more)
        .and_then(|_| search.pointer("/pageInfo/endCursor"))
        .and_then(Value::as_str)
        .map(str::to_string);
    let mut viewers = HashMap::default();
    if let Some(viewer) = viewer {
        viewers.insert(GITHUB_HOST.to_string(), viewer);
    }
    Ok(SearchPage {
        list: PullRequestList {
            entries,
            errors: Vec::new(),
            viewers,
        },
        total: search
            .get("issueCount")
            .and_then(Value::as_u64)
            .unwrap_or_default(),
        next_cursor,
    })
}

/// Reads `GET search/issues`'s answer: how many pull requests the query finds, or why GitHub
/// refused it.
pub fn parse_check(output: &GhOutput) -> Result<u64, SearchError> {
    serde_json::from_slice::<Value>(&output.stdout)
        .ok()
        .filter(|_| output.succeeded())
        .and_then(|value| value.get("total_count").and_then(Value::as_u64))
        .ok_or_else(|| failure(output))
}

/// Why `gh` failed: not installed is told apart before it runs; here, signed out (exit code 4,
/// or GitHub refusing the login), the rate limit, or GitHub's own message.
fn failure(output: &GhOutput) -> SearchError {
    let stderr = String::from_utf8_lossy(&output.stderr);
    let value = serde_json::from_slice::<Value>(&output.stdout).ok();
    // GraphQL's `errors: [{type, message}]`, REST's `{message, errors: [{message}]}`.
    let errors = value
        .as_ref()
        .and_then(|value| value.get("errors"))
        .and_then(Value::as_array)
        .cloned()
        .unwrap_or_default();
    let kinds = errors
        .iter()
        .filter_map(|error| error.get("type").and_then(Value::as_str))
        .collect::<Vec<_>>();
    let messages = errors
        .iter()
        .filter_map(|error| error.get("message").and_then(Value::as_str))
        .chain(
            value
                .as_ref()
                .and_then(|value| value.get("message"))
                .and_then(Value::as_str)
                .filter(|_| errors.is_empty()),
        )
        .map(str::trim)
        .filter(|message| !message.is_empty())
        .collect::<Vec<_>>();
    let text = format!("{stderr} {}", messages.join(" ")).to_lowercase();
    if output.code == Some(4)
        || text.contains("gh auth login")
        || text.contains("bad credentials")
        || text.contains("http 401")
    {
        SearchError::SignedOut
    } else if kinds.contains(&"RATE_LIMITED") || text.contains("rate limit") {
        SearchError::RateLimited
    } else if let Some(message) = messages.first() {
        SearchError::Refused(message.to_string())
    } else {
        let line = stderr
            .lines()
            .map(|line| line.trim().trim_start_matches("gh:").trim())
            .find(|line| !line.is_empty())
            .unwrap_or("gh failed without saying why");
        SearchError::Failed(line.to_string())
    }
}

fn run_error(error: io::Error) -> SearchError {
    if error.kind() == io::ErrorKind::NotFound {
        SearchError::CliMissing
    } else {
        SearchError::Failed(error.to_string())
    }
}

struct Cached {
    at: Instant,
    page: SearchPage,
}

/// GitHub's search, with each query's first page kept so the page draws without asking GitHub.
pub struct GitHubSearch {
    runner: Arc<dyn GhRunner>,
    first_pages: Mutex<HashMap<String, Cached>>,
}

struct GlobalGitHubSearch(Arc<GitHubSearch>);

impl Global for GlobalGitHubSearch {}

/// Searches GitHub with the user's `gh`.
pub fn init(cx: &mut App) {
    let runner = Arc::new(GhCli::new(cx.background_executor().clone()));
    cx.set_global(GlobalGitHubSearch(Arc::new(GitHubSearch::new(runner))));
}

impl GitHubSearch {
    pub fn new(runner: Arc<dyn GhRunner>) -> Self {
        Self {
            runner,
            first_pages: Mutex::new(HashMap::default()),
        }
    }

    /// The app's search: the user's `gh`, or what a test put there.
    pub fn global(cx: &App) -> Arc<Self> {
        match cx.try_global::<GlobalGitHubSearch>() {
            Some(search) => search.0.clone(),
            None => Arc::new(Self::new(Arc::new(GhCli::new(
                cx.background_executor().clone(),
            )))),
        }
    }

    /// Uses `search` as the app's search. For tests.
    #[cfg(test)]
    pub fn set_global(search: Arc<Self>, cx: &mut App) {
        cx.set_global(GlobalGitHubSearch(search));
    }

    /// The first page of a section's `query`, from the cache when it's younger than `max_age`.
    /// When it's empty, GitHub's REST search says whether it refused the query.
    pub fn first_page(
        self: &Arc<Self>,
        query: &str,
        max_age: Duration,
    ) -> BoxFuture<'static, Result<SearchPage, SearchError>> {
        let query = github_query(query);
        if let Some(cached) = self.first_pages.lock().get(&query)
            && cached.at.elapsed() < max_age
        {
            return futures::future::ready(Ok(cached.page.clone())).boxed();
        }
        let this = self.clone();
        async move {
            let page = this.page(&query, None).await?;
            if page.list.entries.is_empty() {
                this.check_github_query(&query).await?;
            }
            this.first_pages.lock().insert(
                query,
                Cached {
                    at: Instant::now(),
                    page: page.clone(),
                },
            );
            Ok(page)
        }
        .boxed()
    }

    /// The page after `cursor` (never cached: "Load more" wants what's there now).
    pub fn next_page(
        self: &Arc<Self>,
        query: &str,
        cursor: String,
    ) -> BoxFuture<'static, Result<SearchPage, SearchError>> {
        let query = github_query(query);
        let this = self.clone();
        async move { this.page(&query, Some(&cursor)).await }.boxed()
    }

    /// How many pull requests a section's query finds, or why GitHub refuses it (for the query
    /// editor).
    pub fn check(self: &Arc<Self>, query: &str) -> BoxFuture<'static, Result<u64, SearchError>> {
        let query = github_query(query);
        let this = self.clone();
        async move { this.check_github_query(&query).await }.boxed()
    }

    async fn page(&self, query: &str, cursor: Option<&str>) -> Result<SearchPage, SearchError> {
        let output = self
            .runner
            .run(search_args(query, cursor))
            .await
            .map_err(run_error)?;
        parse_search(&output)
    }

    async fn check_github_query(&self, query: &str) -> Result<u64, SearchError> {
        let output = self
            .runner
            .run(check_args(query))
            .await
            .map_err(run_error)?;
        parse_check(&output)
    }
}

/// A stand-in for `gh`: records each command and answers with the first fixture meant for its
/// query (any query, for one given with [`RecordingGh::answer`]). Background tasks run in any
/// order in tests, so sections' answers are matched by query, not by turn.
#[cfg(test)]
#[derive(Default)]
pub struct RecordingGh {
    pub calls: Mutex<Vec<Vec<String>>>,
    answers: Mutex<Vec<(Option<String>, io::Result<GhOutput>)>>,
}

#[cfg(test)]
impl RecordingGh {
    pub fn answer(&self, answer: io::Result<GhOutput>) {
        self.answers.lock().push((None, answer));
    }

    pub fn answer_json(&self, value: Value) {
        self.answer(Ok(Self::json(value)));
    }

    /// Answers the next command whose search query contains `query`.
    pub fn answer_query(&self, query: &str, answer: io::Result<GhOutput>) {
        self.answers.lock().push((Some(query.to_string()), answer));
    }

    pub fn answer_query_json(&self, query: &str, value: Value) {
        self.answer_query(query, Ok(Self::json(value)));
    }

    fn json(value: Value) -> GhOutput {
        GhOutput {
            code: Some(0),
            stdout: serde_json::to_vec(&value).expect("JSON encodes"),
            stderr: Vec::new(),
        }
    }

    /// The search query (`q=`) of each recorded command, in the order they ran.
    pub fn queries(&self) -> Vec<String> {
        self.calls
            .lock()
            .iter()
            .filter_map(|args| query_of(args))
            .collect()
    }
}

#[cfg(test)]
fn query_of(args: &[String]) -> Option<String> {
    args.iter()
        .find_map(|arg| arg.strip_prefix("q="))
        .map(str::to_string)
}

#[cfg(test)]
impl GhRunner for RecordingGh {
    fn run(&self, args: Vec<String>) -> BoxFuture<'static, io::Result<GhOutput>> {
        assert!(is_read_only(&args), "a write through gh: {args:?}");
        let query = query_of(&args).unwrap_or_default();
        self.calls.lock().push(args);
        let mut answers = self.answers.lock();
        let answer = answers
            .iter()
            .position(|(key, _)| key.as_ref().is_none_or(|key| query.contains(key.as_str())))
            .map(|index| answers.remove(index).1)
            .unwrap_or_else(|| Err(io::Error::other("no answer scripted")));
        futures::future::ready(answer).boxed()
    }
}

/// A GraphQL search answer with these pull requests (`(repository, number, title)`).
#[cfg(test)]
pub fn search_fixture(pull_requests: &[(&str, u64, &str)], next: Option<&str>) -> Value {
    json!({
        "data": {
            "viewer": { "login": "msetaro" },
            "search": {
                "issueCount": pull_requests.len() as u64 + u64::from(next.is_some()) * 25,
                "pageInfo": { "hasNextPage": next.is_some(), "endCursor": next },
                "nodes": pull_requests.iter().map(|(repository, number, title)| json!({
                    "number": number, "title": title,
                    "url": format!("https://github.com/{repository}/pull/{number}"),
                    "isDraft": false, "state": "OPEN",
                    "createdAt": "2026-10-20T00:00:00Z", "updatedAt": "2026-10-28T00:00:00Z",
                    "repository": { "nameWithOwner": repository },
                    "author": { "login": "octocat" },
                    "headRefName": "feature", "baseRefName": "main",
                    "reviewDecision": null, "additions": 1, "deletions": 0,
                    "labels": { "nodes": [] }, "reviewRequests": { "nodes": [] },
                    "commits": { "nodes": [] },
                })).collect::<Vec<_>>(),
            },
        },
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use pretty_assertions::assert_eq;

    fn output(code: i32, stdout: &str, stderr: &str) -> GhOutput {
        GhOutput {
            code: Some(code),
            stdout: stdout.as_bytes().to_vec(),
            stderr: stderr.as_bytes().to_vec(),
        }
    }

    #[test]
    fn queries_run_as_github_com_pulls_runs_them() {
        assert_eq!(
            github_query("is:open review-requested:@me"),
            "is:open review-requested:@me is:pr archived:false sort:updated-desc"
        );
        // What the query says stays as it says it.
        assert_eq!(
            github_query("  is:pr   author:@me sort:created-asc archived:true "),
            "is:pr author:@me sort:created-asc archived:true"
        );
        assert_eq!(
            github_query("type:pr -archived:true"),
            "type:pr -archived:true sort:updated-desc"
        );
    }

    #[test]
    fn counts_read_as_on_github() {
        assert_eq!(count_label(0), "0");
        assert_eq!(count_label(999), "999");
        assert_eq!(count_label(1391), "1,391");
        assert_eq!(count_label(1234567), "1,234,567");
    }

    #[test]
    fn a_search_is_one_read_only_graphql_query() {
        let args = search_args("is:pr author:@me", Some("Y3Vyc29yOjI1"));
        assert_eq!(
            args.iter()
                .filter(|arg| !arg.starts_with("query="))
                .collect::<Vec<_>>(),
            vec![
                "api",
                "graphql",
                "--hostname",
                "github.com",
                "-f",
                "-f",
                "q=is:pr author:@me",
                "-F",
                "first=25",
                "-f",
                "after=Y3Vyc29yOjI1",
            ]
        );
        let query = args
            .iter()
            .find_map(|arg| arg.strip_prefix("query="))
            .unwrap();
        assert!(query.contains("search(query: $q, type: ISSUE, first: $first, after: $after)"));
        assert!(is_read_only(&args));
        assert!(is_read_only(&check_args("is:pr")));
        assert_eq!(
            check_args("is:pr involves:@me"),
            vec![
                "api",
                "--hostname",
                "github.com",
                "-X",
                "GET",
                "search/issues",
                "-f",
                "q=is:pr involves:@me",
                "-f",
                "per_page=1",
            ]
        );
    }

    #[test]
    fn writes_never_run() {
        let args = |args: &[&str]| args.iter().map(|arg| arg.to_string()).collect::<Vec<_>>();
        assert!(!is_read_only(&args(&["auth", "login"])));
        assert!(!is_read_only(&args(&["auth", "status"])));
        assert!(!is_read_only(&args(&["pr", "merge", "1"])));
        assert!(!is_read_only(&args(&[
            "api",
            "graphql",
            "-f",
            "query=mutation { addComment(input: {}) { clientMutationId } }",
        ])));
        assert!(!is_read_only(&args(&[
            "api",
            "graphql",
            "-f",
            "query=query { viewer { login } } mutation { x }",
        ])));
        assert!(!is_read_only(&args(&[
            "api",
            "-X",
            "POST",
            "repos/acme/api/issues/1/comments",
        ])));
        assert!(!is_read_only(&args(&[
            "api",
            "repos/acme/api/pulls/1/merge"
        ])));
        assert!(!is_read_only(&args(&[
            "api",
            "-X",
            "GET",
            "repos/acme/api/pulls/1",
        ])));
    }

    #[test]
    fn results_read_as_t3s_list_entries() {
        let answer = json!({
            "data": {
                "viewer": { "login": "msetaro" },
                "search": {
                    "issueCount": 31,
                    "pageInfo": { "hasNextPage": true, "endCursor": "Y3Vyc29yOjI1" },
                    "nodes": [
                        {
                            "number": 42, "title": "Retry the upload",
                            "url": "https://github.com/acme/api/pull/42",
                            "isDraft": true, "state": "OPEN",
                            "createdAt": "2026-10-20T00:00:00Z",
                            "updatedAt": "2026-10-28T09:00:00Z",
                            "repository": { "nameWithOwner": "acme/api" },
                            "author": { "login": "octocat" },
                            "headRefName": "retry", "baseRefName": "main",
                            "reviewDecision": "CHANGES_REQUESTED",
                            "additions": 12, "deletions": 3,
                            "labels": { "nodes": [{ "name": "bug", "color": "d73a4a" }] },
                            "reviewRequests": { "nodes": [
                                { "requestedReviewer": { "slug": "core" } },
                                { "requestedReviewer": { "login": "MSetaro" } },
                            ] },
                            "commits": { "nodes": [
                                { "commit": { "statusCheckRollup": { "state": "FAILURE" } } },
                            ] },
                        },
                        // An issue, when a query asks for them.
                        {},
                        {
                            "number": 7, "title": "Docs", "url": "https://github.com/a/b/pull/7",
                            "isDraft": false, "state": "MERGED",
                            "updatedAt": "2026-10-27T00:00:00Z",
                            "repository": { "nameWithOwner": "a/b" }, "author": null,
                            "headRefName": "docs", "baseRefName": "main",
                            "reviewDecision": "APPROVED",
                            "labels": { "nodes": [] }, "reviewRequests": { "nodes": [] },
                            "commits": { "nodes": [
                                { "commit": { "statusCheckRollup": null } },
                            ] },
                        },
                    ],
                },
            },
        });
        let page = parse_search(&output(0, &answer.to_string(), "")).unwrap();
        assert_eq!(page.total, 31);
        assert_eq!(page.next_cursor.as_deref(), Some("Y3Vyc29yOjI1"));
        assert_eq!(
            page.list.viewers.get("github.com").map(String::as_str),
            Some("msetaro")
        );
        let expected = crate::inbox::parse_pull_request_list(json!({
            "entries": [
                {
                    "projectId": "", "host": "github.com", "repository": "acme/api",
                    "number": 42, "title": "Retry the upload",
                    "url": "https://github.com/acme/api/pull/42",
                    "author": { "login": "octocat" }, "headBranch": "retry", "baseBranch": "main",
                    "state": "open", "isDraft": true, "additions": 12, "deletions": 3,
                    "updatedAt": "2026-10-28T09:00:00Z", "viewerReviewRequested": true,
                    "labels": [{ "name": "bug", "color": "d73a4a" }],
                    "reviewDecision": "changes-requested", "checksState": "failing",
                },
                {
                    "projectId": "", "host": "github.com", "repository": "a/b", "number": 7,
                    "title": "Docs", "url": "https://github.com/a/b/pull/7", "author": null,
                    "headBranch": "docs", "baseBranch": "main", "state": "merged",
                    "isDraft": false, "updatedAt": "2026-10-27T00:00:00Z",
                    "viewerReviewRequested": false, "labels": [], "reviewDecision": "approved",
                },
            ],
        }));
        assert_eq!(page.list.entries, expected.entries);
        assert!(
            page.list.entries[0].is_authored_by_viewer(&page.list.viewers) == false
                && page.list.entries[0].viewer_review_requested
        );
    }

    #[test]
    fn failures_say_what_to_do() {
        let failure = |code: i32, stdout: &str, stderr: &str| {
            parse_search(&output(code, stdout, stderr)).unwrap_err()
        };
        assert_eq!(
            failure(
                4,
                "",
                "To get started with GitHub CLI, please run:  gh auth login\n"
            ),
            SearchError::SignedOut
        );
        assert_eq!(
            failure(
                1,
                r#"{"message":"Bad credentials","status":"401"}"#,
                "gh: Bad credentials (HTTP 401)"
            ),
            SearchError::SignedOut
        );
        assert_eq!(
            failure(
                1,
                r#"{"errors":[{"type":"RATE_LIMITED","message":"API rate limit already exceeded for user ID 1."}]}"#,
                "gh: API rate limit already exceeded for user ID 1."
            ),
            SearchError::RateLimited
        );
        assert_eq!(
            failure(
                1,
                r#"{"documentation_url":"https://docs.github.com/rest/overview/rate-limits-for-the-rest-api","message":"You have exceeded a secondary rate limit."}"#,
                "gh: You have exceeded a secondary rate limit. (HTTP 403)"
            ),
            SearchError::RateLimited
        );
        assert_eq!(
            failure(1, "", "gh: Post \"https://api.github.com/graphql\": dial tcp: lookup api.github.com: no such host\n"),
            SearchError::Failed(
                "Post \"https://api.github.com/graphql\": dial tcp: lookup api.github.com: no such host"
                    .to_string()
            )
        );
        assert_eq!(
            run_error(io::Error::from(io::ErrorKind::NotFound)),
            SearchError::CliMissing
        );
        assert!(
            SearchError::SignedOut
                .message()
                .contains("gh auth login in Terminal")
        );

        // REST's search says why it refuses a query.
        let refused = output(
            1,
            r#"{"message":"Validation Failed","errors":[{"message":"The listed users and repositories cannot be searched either because the resources do not exist or you do not have permission to view them.","resource":"Search","field":"q","code":"invalid"}],"status":"422"}"#,
            "gh: Validation Failed (HTTP 422)",
        );
        assert_eq!(
            parse_check(&refused),
            Err(SearchError::Refused(
                "The listed users and repositories cannot be searched either because the \
                 resources do not exist or you do not have permission to view them."
                    .to_string()
            ))
        );
        assert_eq!(
            parse_check(&output(0, r#"{"total_count":22,"items":[]}"#, "")),
            Ok(22)
        );
    }

    #[gpui::test]
    async fn first_pages_are_cached_per_query(cx: &mut gpui::TestAppContext) {
        let gh = Arc::new(RecordingGh::default());
        let search = Arc::new(GitHubSearch::new(gh.clone()));
        gh.answer_json(search_fixture(&[("acme/api", 1, "One")], Some("c1")));
        let page = search
            .first_page("is:open author:@me", Duration::from_secs(30))
            .await
            .unwrap();
        assert_eq!(page.list.entries.len(), 1);
        // Drawing again doesn't ask GitHub; another query does.
        search
            .first_page("is:open author:@me", Duration::from_secs(30))
            .await
            .unwrap();
        assert_eq!(gh.calls.lock().len(), 1);
        // A refresh asks again; so does the next page, after the cursor.
        gh.answer_json(search_fixture(&[("acme/api", 2, "Two")], None));
        search
            .first_page("is:open author:@me", Duration::ZERO)
            .await
            .unwrap();
        gh.answer_json(search_fixture(&[("acme/api", 3, "Three")], None));
        let next = search
            .next_page("is:open author:@me", "c1".to_string())
            .await
            .unwrap();
        assert_eq!(next.list.entries[0].number, 3);
        assert_eq!(next.next_cursor, None);
        let calls = gh.calls.lock().clone();
        assert_eq!(calls.len(), 3);
        assert!(calls[2].contains(&"after=c1".to_string()));
        assert_eq!(
            gh.queries(),
            vec!["is:open author:@me is:pr archived:false sort:updated-desc"; 3]
        );
        cx.run_until_parked();
    }

    #[gpui::test]
    async fn an_empty_search_asks_github_why(cx: &mut gpui::TestAppContext) {
        let gh = Arc::new(RecordingGh::default());
        let search = Arc::new(GitHubSearch::new(gh.clone()));
        gh.answer_json(search_fixture(&[], None));
        gh.answer(Ok(output(
            1,
            r#"{"message":"Validation Failed","errors":[{"message":"\"notadate\" is not a recognized date/time format.","code":"invalid"}],"status":"422"}"#,
            "gh: Validation Failed (HTTP 422)",
        )));
        assert_eq!(
            search
                .first_page("created:>notadate", Duration::from_secs(30))
                .await,
            Err(SearchError::Refused(
                "\"notadate\" is not a recognized date/time format.".to_string()
            ))
        );
        let calls = gh.calls.lock().clone();
        assert_eq!(
            calls[1][..6],
            [
                "api",
                "--hostname",
                "github.com",
                "-X",
                "GET",
                "search/issues"
            ]
        );
        // An empty search GitHub accepts is just empty.
        gh.answer_json(search_fixture(&[], None));
        gh.answer_json(json!({ "total_count": 0, "items": [] }));
        assert_eq!(
            search
                .first_page("is:open label:nothing", Duration::from_secs(30))
                .await
                .map(|page| page.total),
            Ok(0)
        );
        cx.run_until_parked();
    }
}

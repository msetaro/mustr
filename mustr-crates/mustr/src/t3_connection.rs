//! mustr's connection to a T3 Code server: finding or starting the engine for the configured
//! mode (`crate::t3_engine`), checking the server, signing in (pairing with the user's T3 Code,
//! or the embedded engine's bootstrap token), keeping the bearer token, and holding the RPC
//! socket open.
//!
//! mustr asks for exactly [`SCOPES`]: reading orchestration (pull requests, threads), driving
//! agents (messages, approvals, new threads), and writing to pull requests (comments, reviews,
//! viewed marks). A stored token that lacks one of them is replaced by pairing again.

use anyhow::{Context as _, Result, anyhow, bail};
use futures::{AsyncReadExt as _, FutureExt as _, future::BoxFuture};
use gpui::{
    App, AppContext as _, AsyncApp, BackgroundExecutor, Context, Entity, Global, SharedString,
    Subscription, Task, WeakEntity,
};
use http_client::{AsyncBody, HttpClient, Method, Request, Url};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::{
    collections::BTreeMap,
    path::{Path, PathBuf},
    sync::Arc,
    time::{Duration, SystemTime, UNIX_EPOCH},
};
use t3_client::{
    AccessToken, ClientMetadata, EnvironmentDescriptor, PullRequestFile, PullRequestRef,
    RpcConnection, Scope,
};
use t3_runtime::{
    install::{InstallStep, Installer},
    installed::{self, InstallState},
    release::{Arch, Os, Target},
};
use util::ResultExt as _;

use crate::{
    inbox::PullRequestList,
    t3_engine::{EmbeddedEngine, EngineState, T3Mode, executor_sleep},
};

pub const DEFAULT_SERVER_URL: &str = "http://127.0.0.1:3773";
/// Overrides the server URL from settings (the `--t3-server` flag overrides both).
pub const SERVER_URL_ENV: &str = "MUSTR_T3_SERVER";
/// Stores tokens in this JSON file instead of the OS keychain. For tests and headless runs
/// only: the file is protected by file permissions alone.
pub const CREDENTIALS_FILE_ENV: &str = "MUSTR_CREDENTIALS_FILE";

/// The scopes mustr asks T3 for. Never `access:*` or `relay:*`.
pub const SCOPES: [Scope; 3] = [
    Scope::OrchestrationRead,
    Scope::OrchestrationOperate,
    Scope::SourceControlWrite,
];
const PAIRING_LABEL: &str = "mustr";
const PAIRING_TTL: &str = "5m";
const PAIRING_TIMEOUT: Duration = Duration::from_secs(30);
/// Tokens this close to expiring are replaced rather than used.
const TOKEN_EXPIRY_MARGIN_SECS: u64 = 60;
const OPEN_PULL_REQUESTS_LIMIT: u32 = 50;

/// The command that mints a pairing credential with [`SCOPES`] on the T3 machine.
pub fn pairing_command() -> String {
    let scopes = SCOPES
        .iter()
        .map(|scope| format!("--scope {}", scope.as_str()))
        .collect::<Vec<_>>()
        .join(" ");
    format!("t3 auth pairing create {scopes} --label {PAIRING_LABEL}")
}

/// The scopes of [`SCOPES`] that `granted` (T3's scope names) doesn't include.
pub fn missing_scopes<'a>(granted: impl IntoIterator<Item = &'a str>) -> Vec<Scope> {
    let granted = granted.into_iter().collect::<Vec<_>>();
    SCOPES
        .into_iter()
        .filter(|scope| !granted.contains(&scope.as_str()))
        .collect()
}

/// Picks the T3 server: the `--t3-server` flag, then `MUSTR_T3_SERVER`, then `"t3": {"url"}`
/// in mustr's settings file, then T3's default local address.
pub fn resolve_server_url(cli_flag: Option<String>, settings_text: Option<&str>) -> Result<Url> {
    let from_settings = settings_text.and_then(server_url_from_settings);
    let url = cli_flag
        .or_else(|| std::env::var(SERVER_URL_ENV).ok())
        .filter(|url| !url.trim().is_empty())
        .or(from_settings)
        .unwrap_or_else(|| DEFAULT_SERVER_URL.to_string());
    parse_server_url(&url)
}

fn parse_server_url(url: &str) -> Result<Url> {
    let parsed =
        Url::parse(url.trim()).with_context(|| format!("{url:?} isn't a valid T3 server URL"))?;
    match parsed.scheme() {
        "http" | "https" => Ok(parsed),
        other => bail!("T3 server URLs must start with http:// or https://, not {other}://"),
    }
}

// Zed's settings types ignore keys they don't know, so mustr can keep its own settings in the
// same file without patching Zed's `SettingsContent`.
fn server_url_from_settings(text: &str) -> Option<String> {
    let settings: Value = settings::parse_json_with_comments(text).log_err()?;
    settings.get("t3")?.get("url")?.as_str().map(str::to_string)
}

/// Runs `t3 auth pairing create` (argv only, no shell) and returns the one-time credential.
async fn mint_pairing_credential(
    cli: &Path,
    t3_home: Option<&Path>,
    executor: &BackgroundExecutor,
) -> Result<String> {
    let mut command = util::command::new_command(cli);
    command.args(["auth", "pairing", "create"]);
    if let Some(t3_home) = t3_home {
        command.arg("--base-dir").arg(t3_home);
    }
    for scope in SCOPES {
        command.args(["--scope", scope.as_str()]);
    }
    command
        .args(["--ttl", PAIRING_TTL, "--label", PAIRING_LABEL, "--json"])
        .stdin(util::command::Stdio::null());
    // A mustr started from a T3 terminal inherits the T3 service's variables, which would make
    // the CLI act as the service (`mustr-docs/T3-RUNTIME.md` §10). `--base-dir` replaces them.
    for key in t3_runtime::scrubbed_env_keys(std::env::vars_os().map(|(key, _)| key)) {
        command.env_remove(key);
    }

    let output = futures::select! {
        output = command.output().fuse() => output
            .with_context(|| format!("running {}", cli.display()))?,
        _ = executor.timer(PAIRING_TIMEOUT).fuse() => {
            bail!("{} didn't answer within {}s", cli.display(), PAIRING_TIMEOUT.as_secs())
        }
    };
    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        bail!(
            "`t3 auth pairing create` failed ({}): {}",
            output.status,
            truncate(stderr.trim(), 300)
        );
    }
    parse_pairing_output(&output.stdout)
}

fn parse_pairing_output(stdout: &[u8]) -> Result<String> {
    #[derive(Deserialize)]
    struct PairingOutput {
        credential: String,
    }
    // Never include stdout in the error: it holds the credential.
    let output: PairingOutput = serde_json::from_slice(stdout)
        .map_err(|_| anyhow!("`t3 auth pairing create --json` printed something unexpected"))?;
    let credential = output.credential.trim().to_string();
    if credential.is_empty() {
        bail!("`t3 auth pairing create` returned an empty credential");
    }
    Ok(credential)
}

/// Accepts what T3's UI hands out: a bare credential, or a pairing link
/// (`http://host:port/pair#token=…`, or `?token=…`).
pub fn parse_pairing_input(input: &str) -> Option<String> {
    let input = input.trim();
    if input.is_empty() {
        return None;
    }
    let Ok(url) = Url::parse(input) else {
        return (!input.contains(char::is_whitespace)).then(|| input.to_string());
    };
    let from_fragment = url.fragment().and_then(|fragment| {
        url::form_urlencoded::parse(fragment.as_bytes())
            .find(|(key, _)| key == "token")
            .map(|(_, value)| value.into_owned())
    });
    from_fragment
        .or_else(|| {
            url.query_pairs()
                .find(|(key, _)| key == "token")
                .map(|(_, value)| value.into_owned())
        })
        .filter(|token| !token.is_empty())
}

fn truncate(text: &str, max_chars: usize) -> String {
    match text.char_indices().nth(max_chars) {
        Some((index, _)) => format!("{}…", &text[..index]),
        None => text.to_string(),
    }
}

fn unix_now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |elapsed| elapsed.as_secs())
}

/// What mustr keeps between launches (`mustr-docs/T3-RUNTIME.md` §8).
#[derive(Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
struct StoredToken {
    environment_id: String,
    access_token: String,
    scope: String,
    expires_at: u64,
}

impl std::fmt::Debug for StoredToken {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("StoredToken")
            .field("environment_id", &self.environment_id)
            .field("scope", &self.scope)
            .field("expires_at", &self.expires_at)
            .finish_non_exhaustive()
    }
}

impl StoredToken {
    fn new(environment_id: &str, token: &AccessToken, now: u64) -> Self {
        Self {
            environment_id: environment_id.to_string(),
            access_token: token.access_token.clone(),
            scope: token.scope.clone(),
            expires_at: now.saturating_add(token.expires_in.max(0.0) as u64),
        }
    }

    fn is_usable_for(&self, environment_id: &str, now: u64) -> bool {
        self.environment_id == environment_id
            && self.expires_at > now.saturating_add(TOKEN_EXPIRY_MARGIN_SECS)
    }

    fn to_access_token(&self, now: u64) -> AccessToken {
        AccessToken {
            access_token: self.access_token.clone(),
            token_type: "Bearer".to_string(),
            expires_in: self.expires_at.saturating_sub(now) as f64,
            scope: self.scope.clone(),
        }
    }
}

/// The OS keychain, through GPUI's platform credentials API, or a plain file when
/// `MUSTR_CREDENTIALS_FILE` is set.
#[derive(Clone, Debug)]
enum CredentialStore {
    Keychain,
    File(PathBuf),
}

impl CredentialStore {
    fn from_env() -> Self {
        match std::env::var_os(CREDENTIALS_FILE_ENV).filter(|path| !path.is_empty()) {
            Some(path) => {
                log::info!("Storing T3 tokens in {CREDENTIALS_FILE_ENV} instead of the keychain");
                Self::File(PathBuf::from(path))
            }
            None => Self::Keychain,
        }
    }

    fn key(server_url: &Url) -> String {
        format!("mustr-t3:{server_url}")
    }

    async fn read(&self, server_url: &Url, cx: &mut AsyncApp) -> Result<Option<StoredToken>> {
        let key = Self::key(server_url);
        let bytes = match self {
            Self::Keychain => cx
                .update(|cx| cx.read_credentials(&key))
                .await?
                .map(|(_, password)| password),
            Self::File(path) => {
                let path = path.clone();
                cx.background_spawn(async move {
                    let mut tokens = read_token_file(&path)?;
                    anyhow::Ok(
                        tokens
                            .remove(&key)
                            .map(|token| token.to_string().into_bytes()),
                    )
                })
                .await?
            }
        };
        bytes
            .map(|bytes| serde_json::from_slice(&bytes).context("the stored T3 token is corrupt"))
            .transpose()
    }

    async fn write(&self, server_url: &Url, token: &StoredToken, cx: &mut AsyncApp) -> Result<()> {
        let key = Self::key(server_url);
        let bytes = serde_json::to_vec(token)?;
        match self {
            Self::Keychain => {
                cx.update(|cx| cx.write_credentials(&key, PAIRING_LABEL, &bytes))
                    .await
            }
            Self::File(path) => {
                let path = path.clone();
                cx.background_spawn(async move {
                    let mut tokens = read_token_file(&path)?;
                    tokens.insert(key, serde_json::from_slice(&bytes)?);
                    write_token_file(&path, &tokens)
                })
                .await
            }
        }
    }

    async fn delete(&self, server_url: &Url, cx: &mut AsyncApp) -> Result<()> {
        let key = Self::key(server_url);
        match self {
            Self::Keychain => cx.update(|cx| cx.delete_credentials(&key)).await,
            Self::File(path) => {
                let path = path.clone();
                cx.background_spawn(async move {
                    let mut tokens = read_token_file(&path)?;
                    if tokens.remove(&key).is_some() {
                        write_token_file(&path, &tokens)?;
                    }
                    anyhow::Ok(())
                })
                .await
            }
        }
    }
}

fn read_token_file(path: &Path) -> Result<BTreeMap<String, Value>> {
    match std::fs::read(path) {
        Ok(bytes) => serde_json::from_slice(&bytes)
            .with_context(|| format!("{} isn't a mustr token file", path.display())),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(BTreeMap::new()),
        Err(error) => Err(error).with_context(|| format!("reading {}", path.display())),
    }
}

fn write_token_file(path: &Path, tokens: &BTreeMap<String, Value>) -> Result<()> {
    let bytes = serde_json::to_vec_pretty(tokens)?;
    let mut options = std::fs::OpenOptions::new();
    options.write(true).create(true).truncate(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt as _;
        options.mode(0o600);
    }
    let mut file = options
        .open(path)
        .with_context(|| format!("writing {}", path.display()))?;
    std::io::Write::write_all(&mut file, &bytes)?;
    Ok(())
}

/// An authenticated, connected T3 session. Cheap to clone into background tasks.
pub struct T3Session {
    server_url: Url,
    http: Arc<dyn HttpClient>,
    token: AccessToken,
    rpc: RpcConnection,
    descriptor: EnvironmentDescriptor,
    kind: EngineKind,
}

impl T3Session {
    /// The scopes mustr needs that this session wasn't granted, for example when it was paired
    /// with a read-only credential.
    pub fn missing_scopes(&self) -> Vec<Scope> {
        missing_scopes(self.token.scope.split_whitespace())
    }

    /// Why pull requests can't be written to with this session, if they can't.
    pub fn write_blocked_reason(&self) -> Option<String> {
        self.missing_scopes()
            .contains(&Scope::SourceControlWrite)
            .then(|| {
                format!(
                    "mustr's pairing with T3 can't write to pull requests (it lacks {}). Pair \
                     again from the home view: {}",
                    Scope::SourceControlWrite.as_str(),
                    pairing_command()
                )
            })
    }

    /// Why agents can't be driven with this session, if they can't.
    pub fn operate_blocked_reason(&self) -> Option<String> {
        self.missing_scopes()
            .contains(&Scope::OrchestrationOperate)
            .then(|| {
                format!(
                    "mustr's pairing with T3 can't drive agents (it lacks {}). Pair again from \
                     the home view: {}",
                    Scope::OrchestrationOperate.as_str(),
                    pairing_command()
                )
            })
    }

    pub fn rpc(&self) -> &RpcConnection {
        &self.rpc
    }

    /// Calls one of T3's HTTP endpoints with this session's token.
    pub async fn call_endpoint<E>(
        &self,
        params: &E::Params,
        query: &E::Query,
        body: &E::Request,
    ) -> Result<E::Response>
    where
        E: t3_client::HttpEndpoint,
        E::Params: Serialize,
        E::Query: Serialize,
        E::Request: Serialize,
        E::Response: serde::de::DeserializeOwned,
    {
        t3_client::call_endpoint_with::<E>(
            self.http.as_ref(),
            &self.server_url,
            Some(&self.token),
            params,
            query,
            body,
        )
        .await
    }

    pub async fn open_pull_requests(&self) -> Result<PullRequestList> {
        let list = self
            .rpc
            .request(
                "pullRequests.list",
                json!({
                    "state": "open",
                    "involvement": "all",
                    "limit": OPEN_PULL_REQUESTS_LIMIT,
                }),
            )
            .await
            .map_err(|error| anyhow!(error.error_detail().unwrap_or_else(|| error.to_string())))
            .context("listing pull requests")?;
        Ok(crate::inbox::parse_pull_request_list(list))
    }

    /// Why messages can't be sent to agent threads with this session, if they can't.
    pub fn agent_send_blocked_reason(&self) -> Option<String> {
        self.missing_scopes()
            .contains(&Scope::OrchestrationOperate)
            .then(|| {
                format!(
                    "mustr's pairing with T3 can't send messages to agents (it lacks {}). Pair \
                     again from the home view: {}",
                    Scope::OrchestrationOperate.as_str(),
                    pairing_command()
                )
            })
    }

    /// Whether T3 runs on this machine, so the paths it reports (such as a thread's worktree)
    /// are this machine's.
    pub fn server_is_local(&self) -> bool {
        server_is_local(&self.server_url)
    }

    /// T3's shell snapshot (`GET /api/orchestration/shell`): every thread and project, without
    /// their histories. Decoded leniently by its readers.
    pub async fn shell_snapshot(&self) -> Result<Value> {
        let url = self
            .server_url
            .join("/api/orchestration/shell")
            .context("building the shell URL")?;
        let request = Request::builder()
            .method(Method::GET)
            .uri(url.as_str())
            .header("Authorization", self.authorization())
            .header(
                t3_client::ORCHESTRATION_PROTOCOL_HEADER,
                t3_client::ORCHESTRATION_PROTOCOL_VERSION.to_string(),
            )
            .body(AsyncBody::empty())?;
        let mut response = self.http.send(request).await?;
        let mut body = Vec::new();
        response.body_mut().read_to_end(&mut body).await?;
        if !response.status().is_success() {
            bail!(
                "{url} returned {}: {}",
                response.status(),
                truncate(&String::from_utf8_lossy(&body), 300)
            );
        }
        serde_json::from_slice(&body).context("T3 sent a shell snapshot that isn't JSON")
    }

    /// One thread's snapshot with a bounded history (`/bounded`, about a megabyte where the
    /// full snapshot of a long thread can be tens), which still lists every checkpoint.
    pub async fn thread_snapshot(&self, thread_id: &str) -> Result<Value> {
        t3_client::call_endpoint_as::<
            t3_client::t3_api::http::OrchestrationThreadBoundedSnapshot,
            Value,
        >(
            self.http.as_ref(),
            &self.server_url,
            Some(&self.token),
            &t3_client::t3_api::types::HttpOrchestrationThreadBoundedSnapshotParams {
                thread_id: thread_id.to_string(),
            },
            &Default::default(),
            &(),
        )
        .await
        .with_context(|| format!("loading thread {thread_id}"))
    }

    pub async fn pull_request_files(
        &self,
        pull_request: &PullRequestRef,
    ) -> Result<Vec<PullRequestFile>> {
        t3_client::load_pull_request_files(
            self.http.as_ref(),
            &self.server_url,
            &self.token,
            &self.rpc,
            pull_request,
        )
        .await
    }

    fn authorization(&self) -> String {
        format!("Bearer {}", self.token.access_token)
    }
}

/// Whether a server URL names this machine.
pub fn server_is_local(url: &Url) -> bool {
    match url.host() {
        Some(url::Host::Domain(domain)) => domain.eq_ignore_ascii_case("localhost"),
        Some(url::Host::Ipv4(ip)) => ip.is_loopback(),
        Some(url::Host::Ipv6(ip)) => ip.is_loopback(),
        None => false,
    }
}

/// What `/api/auth/session` says about a token.
#[derive(Debug, PartialEq, Eq)]
struct SessionCheck {
    accepted: bool,
    /// The scopes T3 granted the session, when it says.
    scopes: Option<Vec<String>>,
}

/// Whether T3 still accepts the token, and with which scopes. `/api/auth/session` answers 200
/// either way.
async fn check_session(
    http: &dyn HttpClient,
    server_url: &Url,
    token: &AccessToken,
) -> Result<SessionCheck> {
    let url = server_url.join("/api/auth/session")?;
    let request = Request::builder()
        .method(Method::GET)
        .uri(url.as_str())
        .header("Authorization", format!("Bearer {}", token.access_token))
        .header(
            t3_client::ORCHESTRATION_PROTOCOL_HEADER,
            t3_client::ORCHESTRATION_PROTOCOL_VERSION.to_string(),
        )
        .body(AsyncBody::empty())?;
    let mut response = http.send(request).await?;
    let mut body = Vec::new();
    response.body_mut().read_to_end(&mut body).await?;
    if response.status().as_u16() == 401 || response.status().as_u16() == 403 {
        return Ok(SessionCheck {
            accepted: false,
            scopes: None,
        });
    }
    if !response.status().is_success() {
        bail!("{url} returned {}", response.status());
    }
    parse_session_check(&body).with_context(|| format!("unexpected response from {url}"))
}

#[derive(Debug, PartialEq, Eq)]
enum StoredTokenUse {
    Use,
    /// T3 accepts it, but it lacks some of [`SCOPES`]: pair again, and keep it in case that
    /// fails.
    PairAgain,
    /// T3 no longer accepts it.
    Discard,
}

/// What to do with a stored token, and the scopes it has. T3's account of the session wins over
/// the scopes stored with the token.
fn stored_token_use(session: &SessionCheck, stored_scope: &str) -> (StoredTokenUse, String) {
    let scope = session
        .scopes
        .as_ref()
        .map_or_else(|| stored_scope.to_string(), |scopes| scopes.join(" "));
    let stored_use = if !session.accepted {
        StoredTokenUse::Discard
    } else if missing_scopes(scope.split_whitespace()).is_empty() {
        StoredTokenUse::Use
    } else {
        StoredTokenUse::PairAgain
    };
    (stored_use, scope)
}

/// T3's `AuthSessionState`. Only the fields mustr reads, so it works across T3 versions.
///
/// T3 lists the session's full grant in `permissions`, and in `scopes` only the scopes older
/// clients know (`authScopeResponse` in T3's `contracts/src/auth.ts`), so `source-control:write`
/// shows up in `permissions` alone. Servers from before the split send only `scopes`.
fn parse_session_check(body: &[u8]) -> Result<SessionCheck> {
    #[derive(Deserialize)]
    struct SessionState {
        authenticated: bool,
        #[serde(default)]
        scopes: Option<Vec<String>>,
        #[serde(default)]
        permissions: Option<Vec<String>>,
    }
    let state: SessionState = serde_json::from_slice(body)?;
    Ok(SessionCheck {
        accepted: state.authenticated,
        scopes: state.permissions.or(state.scopes),
    })
}

/// Which engine a connection runs on.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum EngineKind {
    /// The user's own T3 Code.
    Installed,
    /// mustr's own engine.
    Embedded,
    /// A T3 server at a URL.
    External,
}

/// What the installed T3 Code needs before mustr can use it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum EngineSetup {
    /// No T3 Code on this computer. `unsupported` says why it can't be installed here.
    NotInstalled { unsupported: Option<SharedString> },
    /// Installed, but nothing runs it: no server, and no background service to start.
    NotRunning { service_supported: bool },
    /// It speaks another protocol than mustr.
    Incompatible {
        message: SharedString,
        /// It's older than mustr, and `t3 update` can fix that.
        can_update: bool,
    },
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ConnectionStatus {
    Connecting(SharedString),
    /// Automatic pairing isn't possible; the user can paste a pairing credential.
    NeedsPairing(SharedString),
    /// The installed T3 Code needs the user's go-ahead (install, start, update) first.
    NeedsSetup(EngineSetup),
    Connected {
        environment: SharedString,
        server_version: Option<SharedString>,
        engine: EngineKind,
    },
    Failed(SharedString),
}

impl ConnectionStatus {
    pub fn is_connected(&self) -> bool {
        matches!(self, Self::Connected { .. })
    }
}

enum ConnectOutcome {
    Connected {
        session: Arc<T3Session>,
        driver: BoxFuture<'static, Result<()>>,
    },
    NeedsPairing(String),
    NeedsSetup(EngineSetup),
}

/// How a connection gets its token.
enum Auth {
    /// A stored token, else pairing through the local `t3` CLI, else a pasted credential.
    Pair(Option<String>),
    /// The embedded engine's bootstrap token: no pairing, and nothing stored.
    Bootstrap(String),
}

/// Makes the `t3` commands that change the user's T3 Code (`t3 service install`, `t3 service
/// restart`, `t3 update`) fail locally instead of running, for test runs on a machine with a
/// real T3 Code.
pub const DRY_RUN_SERVICE_ENV: &str = "MUSTR_DRY_RUN_SERVICE";
/// How long T3's service gets to come up after mustr starts, installs, or updates it.
const SERVICE_START_TIMEOUT: Duration = Duration::from_secs(90);

/// The app's one T3 connection, shared by every window.
pub struct T3Connection {
    mode: T3Mode,
    /// The installed T3 Code can't be used, so this launch runs mustr's own engine.
    fallback: bool,
    external_url: Result<Url, SharedString>,
    server_url: Option<Url>,
    status: ConnectionStatus,
    session: Option<Arc<T3Session>>,
    /// Bumped on every (re)connect, so views can tell sessions apart and stale tasks can tell
    /// they've been superseded.
    generation: usize,
    connect_task: Option<Task<()>>,
    driver_task: Option<Task<()>>,
    current_rpc: CurrentRpc,
    current_session: CurrentSession,
    engine: Option<Entity<EmbeddedEngine>>,
    /// The engine run the current session (or attempt) belongs to.
    engine_run: Option<usize>,
    /// mustr started the installed T3's service once already this launch.
    service_started: bool,
    _engine_subscription: Option<Subscription>,
}

/// The RPC connection of whichever session is current, for work that outlives a reconnect (such
/// as a review open across one). Empty while disconnected.
#[derive(Clone, Default)]
pub struct CurrentRpc(Arc<parking_lot::Mutex<Option<RpcConnection>>>);

impl CurrentRpc {
    pub fn get(&self) -> Option<RpcConnection> {
        self.0.lock().clone()
    }

    fn set(&self, rpc: Option<RpcConnection>) {
        *self.0.lock() = rpc;
    }
}

/// Whichever session is current, for background work that outlives a reconnect (such as the
/// agent threads' backend). Empty while disconnected.
#[derive(Clone, Default)]
pub struct CurrentSession(Arc<parking_lot::Mutex<Option<Arc<T3Session>>>>);

impl CurrentSession {
    pub fn get(&self) -> Option<Arc<T3Session>> {
        self.0.lock().clone()
    }

    fn set(&self, session: Option<Arc<T3Session>>) {
        *self.0.lock() = session;
    }
}

struct GlobalT3Connection(Entity<T3Connection>);

impl Global for GlobalT3Connection {}

pub fn init(mode: T3Mode, external_url: Result<Url>, cx: &mut App) {
    let connection = cx.new(|cx| {
        let mut connection = T3Connection::new(mode, external_url);
        connection.connect(None, cx);
        connection
    });
    cx.set_global(GlobalT3Connection(connection));
}

impl T3Connection {
    pub fn global(cx: &App) -> Entity<Self> {
        cx.global::<GlobalT3Connection>().0.clone()
    }

    fn new(mode: T3Mode, external_url: Result<Url>) -> Self {
        Self {
            mode,
            fallback: false,
            external_url: external_url.map_err(|error| format!("{error:#}").into()),
            server_url: None,
            status: ConnectionStatus::Connecting("Starting".into()),
            session: None,
            generation: 0,
            connect_task: None,
            driver_task: None,
            current_rpc: CurrentRpc::default(),
            current_session: CurrentSession::default(),
            engine: None,
            engine_run: None,
            service_started: false,
            _engine_subscription: None,
        }
    }

    pub fn status(&self) -> &ConnectionStatus {
        &self.status
    }

    /// The mode from settings.
    pub fn mode(&self) -> T3Mode {
        self.mode
    }

    /// The mode in use: the embedded engine when the installed T3 Code fell back to it.
    pub fn effective_mode(&self) -> T3Mode {
        if self.fallback {
            T3Mode::Embedded
        } else {
            self.mode
        }
    }

    pub fn is_fallback(&self) -> bool {
        self.fallback
    }

    pub fn server_url(&self) -> Option<&Url> {
        self.server_url.as_ref()
    }

    /// The embedded engine's log, once it has started.
    pub fn engine_log_file(&self, cx: &App) -> Option<PathBuf> {
        self.engine
            .as_ref()
            .map(|engine| engine.read(cx).log_file())
    }

    pub fn generation(&self) -> usize {
        self.generation
    }

    pub fn current_rpc(&self) -> CurrentRpc {
        self.current_rpc.clone()
    }

    pub fn current_session(&self) -> CurrentSession {
        self.current_session.clone()
    }

    pub fn session(&self) -> Option<Arc<T3Session>> {
        self.session.clone()
    }

    pub fn reconnect(&mut self, cx: &mut Context<Self>) {
        if self.effective_mode() == T3Mode::Embedded
            && let Some(engine) = self.engine.clone()
            && matches!(engine.read(cx).state(), EngineState::Failed(_))
        {
            engine.update(cx, |engine, cx| engine.restart(cx));
        }
        self.connect(None, cx);
    }

    pub fn pair_with_credential(&mut self, credential: String, cx: &mut Context<Self>) {
        self.connect(Some(credential), cx);
    }

    /// Runs mustr's own engine for the rest of this launch, instead of the installed T3 Code.
    pub fn use_embedded(&mut self, cx: &mut Context<Self>) {
        self.fallback = true;
        self.connect(None, cx);
    }

    /// Installs T3 Code (the version mustr is built for) the way T3's installer does, sets up its
    /// background service with `t3 service install`, and connects to it.
    pub fn install_engine(&mut self, cx: &mut Context<Self>) {
        let generation = self.begin_attempt(cx);
        self.set_status(
            ConnectionStatus::Connecting("Installing T3 Code".into()),
            cx,
        );
        let http = cx.http_client();
        self.connect_task = Some(cx.spawn(async move |this, cx| {
            let result = install_t3(&this, generation, http, cx).await;
            this.update(cx, |this, cx| this.after_setup(generation, result, cx))
                .ok();
        }));
    }

    /// Sets up the installed T3 Code's background service (`t3 service install`), which also
    /// starts it, and connects to it.
    pub fn run_in_background(&mut self, cx: &mut Context<Self>) {
        let generation = self.begin_attempt(cx);
        self.set_status(
            ConnectionStatus::Connecting("Setting up T3 Code's background service".into()),
            cx,
        );
        let http = cx.http_client();
        self.connect_task = Some(cx.spawn(async move |this, cx| {
            let result = async {
                let home = installed_home()?;
                let cli =
                    installed::find_t3_cli(Some(&home), std::env::var_os("PATH").as_deref(), None)
                        .context("No T3 Code CLI found on this computer")?;
                run_t3(&installed::CliCommand::service_install(&cli, &home), cx).await?;
                wait_for_installed_server(&home, http.as_ref(), cx).await
            }
            .await;
            this.update(cx, |this, cx| this.after_setup(generation, result, cx))
                .ok();
        }));
    }

    /// Updates the installed T3 Code to the version mustr is built for (`t3 update`), which also
    /// restarts its service, and connects to it.
    pub fn update_installed(&mut self, cx: &mut Context<Self>) {
        let generation = self.begin_attempt(cx);
        let version = t3_client::pinned_server_version();
        self.set_status(
            ConnectionStatus::Connecting(format!("Updating T3 Code to {version}").into()),
            cx,
        );
        let http = cx.http_client();
        self.connect_task = Some(cx.spawn(async move |this, cx| {
            let result = async {
                let home = installed_home()?;
                let server_version =
                    installed::read_server_record(&home).and_then(|record| record.server_version);
                let cli = installed::find_t3_cli(
                    Some(&home),
                    std::env::var_os("PATH").as_deref(),
                    server_version.as_deref(),
                )
                .context("No T3 Code CLI found on this computer")?;
                run_t3(&installed::CliCommand::update(&cli, version, &home), cx).await?;
                wait_for_installed_server(&home, http.as_ref(), cx).await
            }
            .await;
            this.update(cx, |this, cx| this.after_setup(generation, result, cx))
                .ok();
        }));
    }

    /// After installing, starting, or updating the installed T3 Code: connect, or show why not.
    fn after_setup(&mut self, generation: usize, result: Result<()>, cx: &mut Context<Self>) {
        if self.generation != generation {
            return;
        }
        match result {
            Ok(()) => self.connect(None, cx),
            Err(error) => {
                log::error!("couldn't set up T3 Code: {error:#}");
                self.set_status(ConnectionStatus::Failed(format!("{error:#}").into()), cx);
            }
        }
    }

    /// Starts a new attempt: drops the current session and supersedes running tasks.
    fn begin_attempt(&mut self, cx: &mut Context<Self>) -> usize {
        self.generation += 1;
        self.current_rpc.set(None);
        self.current_session.set(None);
        if let Some(session) = self.session.take() {
            session.rpc.close();
        }
        self.driver_task = None;
        self.connect_task = None;
        cx.notify();
        self.generation
    }

    fn set_status(&mut self, status: ConnectionStatus, cx: &mut Context<Self>) {
        self.status = status;
        cx.notify();
    }

    fn connect(&mut self, pasted_credential: Option<String>, cx: &mut Context<Self>) {
        let generation = self.begin_attempt(cx);
        let http = cx.http_client();
        match self.effective_mode() {
            T3Mode::Embedded => {
                self.server_url = None;
                self.engine_run = None;
                let engine = self.ensure_engine(cx);
                engine.update(cx, |engine, cx| engine.start(cx));
                self.engine_changed(cx);
            }
            T3Mode::External => {
                let server_url = match self.external_url.clone() {
                    Ok(url) => url,
                    Err(error) => {
                        self.set_status(ConnectionStatus::Failed(error), cx);
                        return;
                    }
                };
                self.server_url = Some(server_url.clone());
                self.set_status(
                    ConnectionStatus::Connecting("Checking the T3 server".into()),
                    cx,
                );
                self.connect_task = Some(cx.spawn(async move |this, cx| {
                    let outcome = establish(
                        &this,
                        generation,
                        server_url,
                        http,
                        CredentialStore::from_env(),
                        Auth::Pair(pasted_credential),
                        EngineKind::External,
                        cx,
                    )
                    .await;
                    this.update(cx, |this, cx| this.finish_attempt(generation, outcome, cx))
                        .ok();
                }));
            }
            T3Mode::Installed => {
                self.set_status(
                    ConnectionStatus::Connecting("Looking for T3 Code".into()),
                    cx,
                );
                let start_service = !self.service_started;
                self.connect_task = Some(cx.spawn(async move |this, cx| {
                    let outcome = connect_installed(
                        &this,
                        generation,
                        http,
                        pasted_credential,
                        start_service,
                        cx,
                    )
                    .await;
                    this.update(cx, |this, cx| this.finish_attempt(generation, outcome, cx))
                        .ok();
                }));
            }
        }
    }

    fn ensure_engine(&mut self, cx: &mut Context<Self>) -> Entity<EmbeddedEngine> {
        if let Some(engine) = &self.engine {
            return engine.clone();
        }
        let engine = cx.new(EmbeddedEngine::new);
        self._engine_subscription =
            Some(cx.observe(&engine, |this, _, cx| this.engine_changed(cx)));
        self.engine = Some(engine.clone());
        engine
    }

    /// Follows the embedded engine: connects to each run once it's ready, and shows its state
    /// until then.
    fn engine_changed(&mut self, cx: &mut Context<Self>) {
        if self.effective_mode() != T3Mode::Embedded {
            return;
        }
        let Some(engine) = self.engine.clone() else {
            return;
        };
        let state = engine.read(cx).state().clone();
        let status = match state {
            EngineState::Ready { url, run, .. } => {
                if self.engine_run != Some(run) {
                    self.engine_run = Some(run);
                    let token = engine.read(cx).bootstrap_token();
                    self.connect_embedded(url, token, cx);
                }
                return;
            }
            EngineState::Idle | EngineState::Starting => {
                ConnectionStatus::Connecting("Starting the agents engine".into())
            }
            EngineState::Installing(step) => ConnectionStatus::Connecting(
                format!("Setting up the agents engine · {step}").into(),
            ),
            EngineState::Restarting { attempt, reason } => ConnectionStatus::Connecting(
                format!("Restarting the agents engine (attempt {attempt}) · {reason}").into(),
            ),
            EngineState::Failed(error) => ConnectionStatus::Failed(error),
        };
        // The session's engine is gone or restarting.
        if self.session.is_some() {
            self.begin_attempt(cx);
        }
        self.engine_run = None;
        self.set_status(status, cx);
    }

    fn connect_embedded(&mut self, server_url: Url, token: String, cx: &mut Context<Self>) {
        let generation = self.begin_attempt(cx);
        self.server_url = Some(server_url.clone());
        self.set_status(
            ConnectionStatus::Connecting("Connecting to the agents engine".into()),
            cx,
        );
        let http = cx.http_client();
        self.connect_task = Some(cx.spawn(async move |this, cx| {
            let outcome = establish(
                &this,
                generation,
                server_url,
                http,
                CredentialStore::from_env(),
                Auth::Bootstrap(token),
                EngineKind::Embedded,
                cx,
            )
            .await;
            this.update(cx, |this, cx| this.finish_attempt(generation, outcome, cx))
                .ok();
        }));
    }

    fn finish_attempt(
        &mut self,
        generation: usize,
        outcome: Result<ConnectOutcome>,
        cx: &mut Context<Self>,
    ) {
        if self.generation != generation {
            return;
        }
        let status = match outcome {
            Ok(ConnectOutcome::Connected { session, driver }) => {
                self.watch_driver(generation, driver, cx);
                let status = ConnectionStatus::Connected {
                    environment: session.descriptor.label.clone().into(),
                    server_version: session.descriptor.server_version.clone().map(Into::into),
                    engine: session.kind,
                };
                self.server_url = Some(session.server_url.clone());
                self.current_rpc.set(Some(session.rpc.clone()));
                self.current_session.set(Some(session.clone()));
                self.session = Some(session);
                status
            }
            Ok(ConnectOutcome::NeedsPairing(reason)) => {
                ConnectionStatus::NeedsPairing(reason.into())
            }
            Ok(ConnectOutcome::NeedsSetup(setup)) => ConnectionStatus::NeedsSetup(setup),
            Err(error) => {
                log::error!("couldn't connect to T3: {error:#}");
                ConnectionStatus::Failed(format!("{error:#}").into())
            }
        };
        self.set_status(status, cx);
    }

    fn watch_driver(
        &mut self,
        generation: usize,
        driver: BoxFuture<'static, Result<()>>,
        cx: &mut Context<Self>,
    ) {
        let driver = cx.background_spawn(driver);
        self.driver_task = Some(cx.spawn(async move |this, cx| {
            let result = driver.await;
            this.update(cx, |this, cx| {
                if this.generation != generation {
                    return;
                }
                let reason = match result {
                    Ok(()) => "the connection was closed".to_string(),
                    Err(error) => format!("{error:#}"),
                };
                log::warn!("T3 connection ended: {reason}");
                this.session = None;
                this.current_rpc.set(None);
                this.current_session.set(None);
                if this.effective_mode() == T3Mode::Embedded {
                    // Reconnect to the engine once; if it's restarting, its state shows.
                    this.engine_run = None;
                    this.engine_changed(cx);
                } else {
                    this.set_status(
                        ConnectionStatus::Failed(format!("Disconnected from T3: {reason}").into()),
                        cx,
                    );
                }
            })
            .ok();
        }));
    }

    fn set_step(
        &mut self,
        generation: usize,
        step: impl Into<SharedString>,
        cx: &mut Context<Self>,
    ) {
        if self.generation == generation {
            self.set_status(ConnectionStatus::Connecting(step.into()), cx);
        }
    }
}

fn installed_home() -> Result<PathBuf> {
    installed::t3_home().context("couldn't find your home directory")
}

/// The platform to look for runtimes as. T3 ships nothing for Intel Macs, but an install there
/// (from npm) still names its executable `t3`.
fn detection_target() -> Target {
    Target::current().unwrap_or(Target {
        os: if cfg!(windows) {
            Os::Windows
        } else if cfg!(target_os = "macos") {
            Os::MacOs
        } else {
            Os::Linux
        },
        arch: Arch::Arm64,
    })
}

/// Runs a `t3` command that changes the user's T3 Code, unless [`DRY_RUN_SERVICE_ENV`] is set.
async fn run_t3(command: &installed::CliCommand, cx: &mut AsyncApp) -> Result<()> {
    if std::env::var_os(DRY_RUN_SERVICE_ENV).is_some_and(|value| !value.is_empty()) {
        log::warn!("{DRY_RUN_SERVICE_ENV} is set: not running `{command}`");
        bail!("{DRY_RUN_SERVICE_ENV} is set, so mustr didn't run `{command}`");
    }
    let sleep = executor_sleep(cx);
    let command = command.clone();
    let stdout = cx
        .background_spawn(async move { command.run(&sleep).await })
        .await?;
    log::info!("{}", truncate(stdout.trim(), 500));
    Ok(())
}

/// Waits until a server runs from `home` and answers.
async fn wait_for_installed_server(
    home: &Path,
    http: &dyn HttpClient,
    cx: &mut AsyncApp,
) -> Result<()> {
    let sleep = executor_sleep(cx);
    let started = std::time::Instant::now();
    loop {
        let home_dir = home.to_path_buf();
        let record = cx
            .background_spawn(async move { installed::running_server(&home_dir) })
            .await;
        if let Some(record) = record
            && t3_runtime::engine::answers(http, &record.origin, &sleep).await
        {
            return Ok(());
        }
        if started.elapsed() >= SERVICE_START_TIMEOUT {
            bail!(
                "T3 Code's background service didn't start within {}s. Its log is {}.",
                SERVICE_START_TIMEOUT.as_secs(),
                home.join("userdata/logs/boot-service.log").display()
            );
        }
        cx.background_executor()
            .timer(Duration::from_millis(500))
            .await;
    }
}

#[derive(Debug, PartialEq, Eq)]
enum InstalledStep {
    /// A server runs from the user's T3 home at this URL.
    Connect(Url),
    /// Start T3's installed service with this CLI (`t3 service restart`).
    StartService(PathBuf),
    /// The user has to choose (install, run in the background, or use mustr's own engine).
    Setup(EngineSetup),
}

/// What to do about what [`installed::detect`] found. mustr starts an installed service once
/// per launch on its own; it installs T3 Code, or a service, only when the user asks.
fn installed_step(
    state: InstallState,
    start_service: bool,
    unsupported: Option<SharedString>,
) -> InstalledStep {
    match state {
        InstallState::NotInstalled => {
            InstalledStep::Setup(EngineSetup::NotInstalled { unsupported })
        }
        InstallState::Installed {
            server: Some(server),
            ..
        } => InstalledStep::Connect(server.origin),
        InstallState::Installed {
            server: None,
            service_installed: true,
            cli: Some(cli),
            ..
        } if start_service => InstalledStep::StartService(cli),
        InstallState::Installed {
            service_supported, ..
        } => InstalledStep::Setup(EngineSetup::NotRunning { service_supported }),
    }
}

/// Why the installed T3 Code can't be used with this mustr, if it can't.
fn incompatibility(descriptor: &EnvironmentDescriptor) -> Option<EngineSetup> {
    let error = t3_client::check_protocol_compatibility(descriptor).err()?;
    Some(EngineSetup::Incompatible {
        message: format!("{error:#}").into(),
        can_update: descriptor.protocol_version() < t3_client::ORCHESTRATION_PROTOCOL_VERSION,
    })
}

/// Finds the user's own T3 Code and connects to its server, starting its service first when
/// it's installed but stopped.
async fn connect_installed(
    this: &WeakEntity<T3Connection>,
    generation: usize,
    http: Arc<dyn HttpClient>,
    pasted_credential: Option<String>,
    start_service: bool,
    cx: &mut AsyncApp,
) -> Result<ConnectOutcome> {
    let home = installed_home()?;
    let state = cx
        .background_spawn({
            let home = home.clone();
            async move {
                installed::detect(
                    &home,
                    dirs::home_dir().as_deref(),
                    std::env::var_os("PATH").as_deref(),
                    detection_target(),
                )
            }
        })
        .await;
    let unsupported = Target::current()
        .err()
        .map(|error| format!("{error:#}").into());
    let server_url = match installed_step(state, start_service, unsupported) {
        InstalledStep::Connect(url) => url,
        InstalledStep::Setup(setup) => return Ok(ConnectOutcome::NeedsSetup(setup)),
        InstalledStep::StartService(cli) => {
            this.update(cx, |this, cx| {
                this.service_started = true;
                this.set_step(generation, "Starting T3 Code's background service", cx);
            })?;
            run_t3(&installed::CliCommand::service_restart(&cli, &home), cx).await?;
            wait_for_installed_server(&home, http.as_ref(), cx).await?;
            installed::running_server(&home)
                .context("T3 Code's service started, but its server isn't running")?
                .origin
        }
    };

    let descriptor = cx
        .background_spawn({
            let http = http.clone();
            let server_url = server_url.clone();
            async move { t3_client::fetch_descriptor(http.as_ref(), &server_url).await }
        })
        .await
        .with_context(|| format!("Couldn't reach T3 Code at {server_url}"))?;
    if let Some(setup) = incompatibility(&descriptor) {
        return Ok(ConnectOutcome::NeedsSetup(setup));
    }
    establish(
        this,
        generation,
        server_url,
        http,
        CredentialStore::from_env(),
        Auth::Pair(pasted_credential),
        EngineKind::Installed,
        cx,
    )
    .await
}

/// Installs the pinned T3 Code into the user's T3 home the way `install.sh` does, then sets up
/// its service with T3's own `t3 service install`.
async fn install_t3(
    this: &WeakEntity<T3Connection>,
    generation: usize,
    http: Arc<dyn HttpClient>,
    cx: &mut AsyncApp,
) -> Result<()> {
    let home = installed_home()?;
    let target = Target::current()?;
    let version = t3_client::pinned_server_version();
    let sleep = executor_sleep(cx);
    let (progress_tx, mut progress_rx) = futures::channel::mpsc::unbounded::<InstallStep>();
    let install = cx.background_spawn({
        let installer = Installer::new(http.clone(), target, sleep);
        let versions_dir = installed::versions_dir(&home);
        async move {
            let progress = move |step: InstallStep| {
                progress_tx.unbounded_send(step).ok();
            };
            installer.ensure(&versions_dir, version, &progress).await
        }
    });
    let mut install = install.fuse();
    let runtime = loop {
        futures::select! {
            step = futures::StreamExt::next(&mut progress_rx) => if let Some(step) = step {
                this.update(cx, |this, cx| {
                    this.set_step(generation, format!("Installing T3 Code {version} · {}", step.describe()), cx)
                })?;
            },
            result = install => break result.context("installing T3 Code")?,
        }
    };
    let bin_dir = installed::default_bin_dir().context("couldn't find your home directory")?;
    let launcher = installed::link_launcher(&runtime, &bin_dir, &home)?;
    log::info!(
        "installed T3 Code {version}; `t3` is {}",
        launcher.display()
    );
    if installed::ServiceManager::for_os(target.os).is_none() {
        // Nothing to run it in the background; the setup card offers mustr's own engine.
        return Ok(());
    }
    this.update(cx, |this, cx| {
        this.service_started = true;
        this.set_step(generation, "Setting up T3 Code's background service", cx)
    })?;
    run_t3(
        &installed::CliCommand::service_install(&runtime.executable, &home),
        cx,
    )
    .await?;
    wait_for_installed_server(&home, http.as_ref(), cx).await
}

#[allow(clippy::too_many_arguments)]
async fn establish(
    this: &WeakEntity<T3Connection>,
    generation: usize,
    server_url: Url,
    http: Arc<dyn HttpClient>,
    store: CredentialStore,
    auth: Auth,
    kind: EngineKind,
    cx: &mut AsyncApp,
) -> Result<ConnectOutcome> {
    let set_step = |step: &'static str, cx: &mut AsyncApp| {
        this.update(cx, |this, cx| this.set_step(generation, step, cx))
            .ok();
    };

    let descriptor = cx
        .background_spawn({
            let http = http.clone();
            let server_url = server_url.clone();
            async move { t3_client::fetch_descriptor(http.as_ref(), &server_url).await }
        })
        .await
        .with_context(|| format!("Couldn't reach T3 Code at {server_url}"))?;
    t3_client::check_protocol_compatibility(&descriptor)?;

    let token = match auth {
        Auth::Bootstrap(bootstrap_token) => {
            set_step("Signing in to the agents engine", cx);
            let token = cx
                .background_spawn({
                    let http = http.clone();
                    let server_url = server_url.clone();
                    async move {
                        t3_client::exchange_pairing_credential(
                            http.as_ref(),
                            &server_url,
                            &bootstrap_token,
                            &SCOPES,
                            &ClientMetadata::default(),
                        )
                        .await
                    }
                })
                .await
                .context("The agents engine didn't accept mustr's sign-in")?;
            let missing = missing_scopes(token.scope.split_whitespace());
            if !missing.is_empty() {
                log::warn!(
                    "the agents engine granted mustr only {:?}, without {}",
                    token.scope,
                    scope_list(&missing)
                );
            }
            token
        }
        Auth::Pair(pasted_credential) => {
            match stored_or_paired_token(
                &server_url,
                &http,
                &descriptor,
                &store,
                pasted_credential,
                &set_step,
                cx,
            )
            .await?
            {
                Ok(token) => token,
                Err(reason) => return Ok(ConnectOutcome::NeedsPairing(reason)),
            }
        }
    };

    set_step("Connecting", cx);
    let (rpc, driver) = cx
        .background_spawn({
            let http = http.clone();
            let server_url = server_url.clone();
            let token = token.clone();
            async move {
                let ticket =
                    t3_client::websocket_ticket(http.as_ref(), &server_url, &token).await?;
                RpcConnection::connect(&t3_client::websocket_url(&server_url, &ticket)?).await
            }
        })
        .await
        .context("opening the T3 connection")?;
    log::info!(
        "Connected to T3 environment {} ({}, {kind:?})",
        descriptor.label,
        descriptor
            .server_version
            .as_deref()
            .unwrap_or("unknown version")
    );
    Ok(ConnectOutcome::Connected {
        session: Arc::new(T3Session {
            server_url,
            http,
            token,
            rpc,
            descriptor,
            kind,
        }),
        driver,
    })
}

/// A token for [`SCOPES`]: the stored one if T3 still accepts it, else a new pairing. `Err` holds
/// why the user has to paste a credential.
async fn stored_or_paired_token(
    server_url: &Url,
    http: &Arc<dyn HttpClient>,
    descriptor: &EnvironmentDescriptor,
    store: &CredentialStore,
    pasted_credential: Option<String>,
    set_step: &impl Fn(&'static str, &mut AsyncApp),
    cx: &mut AsyncApp,
) -> Result<Result<AccessToken, String>> {
    // A token T3 still accepts but that lacks some of `SCOPES`, used if pairing again fails, so
    // mustr stays connected (without the missing scopes) instead of losing the connection.
    let mut fallback_token = None;
    if pasted_credential.is_none() {
        match store.read(server_url, cx).await {
            Ok(Some(stored)) if stored.is_usable_for(&descriptor.environment_id, unix_now()) => {
                set_step("Signing in", cx);
                let mut candidate = stored.to_access_token(unix_now());
                let session = cx
                    .background_spawn({
                        let http = http.clone();
                        let server_url = server_url.clone();
                        let candidate = candidate.clone();
                        async move { check_session(http.as_ref(), &server_url, &candidate).await }
                    })
                    .await
                    .context("checking mustr's stored T3 token")?;
                let (stored_use, granted_scope) = stored_token_use(&session, &stored.scope);
                candidate.scope = granted_scope;
                match stored_use {
                    StoredTokenUse::Use => return Ok(Ok(candidate)),
                    StoredTokenUse::PairAgain => {
                        log::info!(
                            "mustr's stored T3 token lacks {}; pairing again",
                            scope_list(&missing_scopes(candidate.scope.split_whitespace()))
                        );
                        fallback_token = Some(candidate);
                    }
                    StoredTokenUse::Discard => {
                        log::info!("T3 no longer accepts mustr's stored token; pairing again");
                        store.delete(server_url, cx).await.log_err();
                    }
                }
            }
            Ok(Some(_)) => {
                log::info!(
                    "mustr's stored T3 token is expired or for another server; pairing again"
                );
                store.delete(server_url, cx).await.log_err();
            }
            Ok(None) => {}
            Err(error) => log::warn!("couldn't read mustr's stored T3 token: {error:#}"),
        }
    }

    let paired = pair(
        server_url,
        http,
        descriptor,
        pasted_credential,
        set_step,
        cx,
    )
    .await;
    let access_token = match (paired, fallback_token) {
        (Ok(access_token), _) => {
            let stored = StoredToken::new(&descriptor.environment_id, &access_token, unix_now());
            if let Err(error) = store.write(server_url, &stored, cx).await {
                log::error!(
                    "couldn't save the T3 token, so mustr will pair again next launch: {error:#}"
                );
            }
            access_token
        }
        (Err(failure), Some(fallback)) => {
            log::warn!(
                "couldn't pair again ({}); staying connected without {}",
                failure.message(),
                scope_list(&missing_scopes(fallback.scope.split_whitespace()))
            );
            fallback
        }
        (Err(PairingFailure::NeedsPairing(reason)), None) => return Ok(Err(reason)),
        (Err(PairingFailure::Failed(error)), None) => return Err(error),
    };
    let missing = missing_scopes(access_token.scope.split_whitespace());
    if !missing.is_empty() {
        log::warn!(
            "T3 granted mustr only {:?}, without {}",
            access_token.scope,
            scope_list(&missing)
        );
    }
    Ok(Ok(access_token))
}

enum PairingFailure {
    /// Automatic pairing isn't possible, or T3 refused a pasted credential: the user can paste
    /// one.
    NeedsPairing(String),
    Failed(anyhow::Error),
}

impl PairingFailure {
    fn message(&self) -> String {
        match self {
            PairingFailure::NeedsPairing(reason) => reason.clone(),
            PairingFailure::Failed(error) => format!("{error:#}"),
        }
    }
}

/// Gets a new token for [`SCOPES`], from a pasted credential or one minted with the local CLI.
async fn pair(
    server_url: &Url,
    http: &Arc<dyn HttpClient>,
    descriptor: &EnvironmentDescriptor,
    pasted_credential: Option<String>,
    set_step: &impl Fn(&'static str, &mut AsyncApp),
    cx: &mut AsyncApp,
) -> Result<AccessToken, PairingFailure> {
    let is_pasted = pasted_credential.is_some();
    let credential = match pasted_credential {
        Some(credential) => credential,
        None => {
            set_step("Pairing with the local T3 Code", cx);
            let server_version = descriptor.server_version.clone();
            let executor = cx.background_executor().clone();
            let minted = cx
                .background_spawn(async move {
                    let home = installed::t3_home();
                    let path_env = std::env::var_os("PATH");
                    let cli = installed::find_t3_cli(
                        home.as_deref(),
                        path_env.as_deref(),
                        server_version.as_deref(),
                    )
                    .context("No T3 Code CLI found on this computer")?;
                    log::info!("Pairing with T3 through {}", cli.display());
                    mint_pairing_credential(&cli, home.as_deref(), &executor).await
                })
                .await;
            match minted {
                Ok(credential) => credential,
                Err(error) => {
                    log::warn!("automatic pairing failed: {error:#}");
                    return Err(PairingFailure::NeedsPairing(format!("{error:#}")));
                }
            }
        }
    };
    set_step("Pairing with T3", cx);
    let exchanged = cx
        .background_spawn({
            let http = http.clone();
            let server_url = server_url.clone();
            async move {
                t3_client::exchange_pairing_credential(
                    http.as_ref(),
                    &server_url,
                    &credential,
                    &SCOPES,
                    &ClientMetadata::default(),
                )
                .await
            }
        })
        .await;
    match exchanged {
        Ok(access_token) => Ok(access_token),
        // A typo in a pasted credential shouldn't lose the paste field.
        Err(error) if is_pasted => Err(PairingFailure::NeedsPairing(format!(
            "T3 didn't accept that pairing credential: {error:#}"
        ))),
        Err(error) => Err(PairingFailure::Failed(
            error.context("T3 didn't accept mustr's pairing"),
        )),
    }
}

fn scope_list(scopes: &[Scope]) -> String {
    scopes
        .iter()
        .map(|scope| scope.as_str())
        .collect::<Vec<_>>()
        .join(", ")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_installed_t3_is_used_started_or_set_up() {
        let server = installed::ServerRecord {
            pid: 1,
            origin: Url::parse("http://127.0.0.1:3773").unwrap(),
            server_version: Some("0.1.1".to_string()),
            service_managed: true,
        };
        let cli = PathBuf::from("/home/someone/.t3/runtime/versions/0.1.1/t3");
        let installed = |server: Option<installed::ServerRecord>, service_installed: bool| {
            InstallState::Installed {
                cli: Some(cli.clone()),
                runtimes: Vec::new(),
                server,
                service_installed,
                service_supported: true,
            }
        };
        assert_eq!(
            installed_step(installed(Some(server.clone()), true), true, None),
            InstalledStep::Connect(server.origin)
        );
        // A stopped service is started once per launch, then left to the user.
        assert_eq!(
            installed_step(installed(None, true), true, None),
            InstalledStep::StartService(cli.clone())
        );
        assert_eq!(
            installed_step(installed(None, true), false, None),
            InstalledStep::Setup(EngineSetup::NotRunning {
                service_supported: true
            })
        );
        // No service: mustr never installs one without asking.
        assert_eq!(
            installed_step(installed(None, false), true, None),
            InstalledStep::Setup(EngineSetup::NotRunning {
                service_supported: true
            })
        );
        assert_eq!(
            installed_step(
                InstallState::NotInstalled,
                true,
                Some("no Intel Macs".into())
            ),
            InstalledStep::Setup(EngineSetup::NotInstalled {
                unsupported: Some("no Intel Macs".into())
            })
        );
    }

    #[test]
    fn older_t3s_can_be_updated_and_newer_ones_need_a_newer_mustr() {
        let descriptor = |protocol| EnvironmentDescriptor {
            environment_id: "e".to_string(),
            label: "ironforge".to_string(),
            orchestration_protocol_version: protocol,
            server_version: Some("0.0.40".to_string()),
            capabilities: Value::Null,
        };
        assert_eq!(
            incompatibility(&descriptor(Some(t3_client::ORCHESTRATION_PROTOCOL_VERSION))),
            None
        );
        let Some(EngineSetup::Incompatible {
            message,
            can_update,
        }) = incompatibility(&descriptor(None))
        else {
            panic!("protocol 1 is incompatible");
        };
        assert!(can_update);
        assert!(message.contains("Update T3 Code"), "{message}");
        let Some(EngineSetup::Incompatible { can_update, .. }) = incompatibility(&descriptor(
            Some(t3_client::ORCHESTRATION_PROTOCOL_VERSION + 1),
        )) else {
            panic!("a newer protocol is incompatible");
        };
        assert!(!can_update);
    }

    #[test]
    fn pairing_output_is_parsed_without_echoing_it() {
        let output = br#"{"id":"p1","credential":" secret-credential ","scopes":["orchestration:read"],"expiresAt":"2026-10-27T00:05:00Z"}"#;
        assert_eq!(parse_pairing_output(output).unwrap(), "secret-credential");

        let error = parse_pairing_output(br#"{"credential": 7, "note": "secret"}"#).unwrap_err();
        assert!(!format!("{error:#}").contains("secret"));
        assert!(parse_pairing_output(br#"{"credential":"  "}"#).is_err());
    }

    #[test]
    fn pairing_input_accepts_credentials_and_links() {
        assert_eq!(parse_pairing_input("  abc123  ").as_deref(), Some("abc123"));
        assert_eq!(
            parse_pairing_input("http://127.0.0.1:3773/pair#token=abc%2B123").as_deref(),
            Some("abc+123")
        );
        assert_eq!(
            parse_pairing_input("https://box.tailnet.ts.net/pair?token=xyz").as_deref(),
            Some("xyz")
        );
        assert_eq!(parse_pairing_input("http://127.0.0.1:3773/pair"), None);
        assert_eq!(parse_pairing_input("two words"), None);
        assert_eq!(parse_pairing_input("   "), None);
    }

    #[test]
    fn server_url_precedence() {
        let settings = r#"{
            // mustr's own key; Zed ignores it.
            "t3": { "url": "http://10.0.0.2:3773" },
            "theme": "mustr Dark",
        }"#;
        // The env var is process-global; only check it isn't set by the harness.
        if std::env::var_os(SERVER_URL_ENV).is_none() {
            assert_eq!(
                resolve_server_url(None, Some(settings)).unwrap().as_str(),
                "http://10.0.0.2:3773/"
            );
            assert_eq!(
                resolve_server_url(None, None).unwrap().as_str(),
                "http://127.0.0.1:3773/"
            );
            assert_eq!(
                resolve_server_url(None, Some("{ not json"))
                    .unwrap()
                    .as_str(),
                "http://127.0.0.1:3773/"
            );
        }
        assert_eq!(
            resolve_server_url(Some("http://localhost:9999".into()), Some(settings))
                .unwrap()
                .as_str(),
            "http://localhost:9999/"
        );
        assert!(resolve_server_url(Some("ws://localhost:3773".into()), None).is_err());
        assert!(resolve_server_url(Some("not a url".into()), None).is_err());
    }

    #[test]
    fn stored_tokens_expire_and_belong_to_one_environment() {
        let access_token = AccessToken {
            access_token: "secret".to_string(),
            token_type: "Bearer".to_string(),
            expires_in: 3600.0,
            scope: "orchestration:read".to_string(),
        };
        let stored = StoredToken::new("environment", &access_token, 1_000);
        assert_eq!(stored.expires_at, 4_600);
        assert!(stored.is_usable_for("environment", 1_000));
        assert!(!stored.is_usable_for("other", 1_000));
        assert!(!stored.is_usable_for("environment", 4_600 - TOKEN_EXPIRY_MARGIN_SECS));
        assert!(!format!("{stored:?}").contains("secret"));

        let restored = stored.to_access_token(2_000);
        assert_eq!(restored.access_token, "secret");
        assert_eq!(restored.expires_in, 2_600.0);
        assert_eq!(restored.scope, "orchestration:read");
    }

    #[test]
    fn mustr_asks_for_reading_operating_agents_and_pull_request_writes_only() {
        assert_eq!(
            SCOPES.map(Scope::as_str),
            [
                "orchestration:read",
                "orchestration:operate",
                "source-control:write"
            ]
        );
        assert_eq!(
            pairing_command(),
            "t3 auth pairing create --scope orchestration:read --scope orchestration:operate \
             --scope source-control:write --label mustr"
        );
        assert_eq!(
            missing_scopes("orchestration:read".split_whitespace()),
            [Scope::OrchestrationOperate, Scope::SourceControlWrite]
        );
        assert!(
            missing_scopes([
                "source-control:write",
                "orchestration:read",
                "orchestration:operate"
            ])
            .is_empty()
        );
    }

    #[test]
    fn local_servers() {
        let local = |url: &str| server_is_local(&Url::parse(url).unwrap());
        assert!(local("http://127.0.0.1:3773"));
        assert!(local("http://localhost:3773"));
        assert!(local("http://[::1]:3773"));
        assert!(!local("https://box.tailnet.ts.net"));
        assert!(!local("http://10.0.0.2:3773"));
    }

    #[test]
    fn stored_tokens_without_the_write_scopes_are_replaced() {
        let check = |accepted: bool, scopes: Option<&[&str]>| SessionCheck {
            accepted,
            scopes: scopes.map(|scopes| scopes.iter().map(|scope| scope.to_string()).collect()),
        };
        let all = "orchestration:read orchestration:operate source-control:write";
        assert_eq!(
            stored_token_use(&check(true, None), all),
            (StoredTokenUse::Use, all.to_string())
        );
        // A token from M0, paired read-only.
        assert_eq!(
            stored_token_use(&check(true, None), "orchestration:read"),
            (StoredTokenUse::PairAgain, "orchestration:read".to_string())
        );
        // A token from M1, which couldn't drive agents yet.
        let review_only = "orchestration:read source-control:write";
        assert_eq!(
            stored_token_use(&check(true, None), review_only),
            (StoredTokenUse::PairAgain, review_only.to_string())
        );
        // T3's account of the session wins over what was stored.
        assert_eq!(
            stored_token_use(&check(true, Some(&["orchestration:read"])), all),
            (StoredTokenUse::PairAgain, "orchestration:read".to_string())
        );
        assert_eq!(
            stored_token_use(&check(false, None), all).0,
            StoredTokenUse::Discard
        );

        // A token from M1, without the scope to message agents.
        assert_eq!(
            stored_token_use(
                &check(true, None),
                "orchestration:read source-control:write"
            )
            .0,
            StoredTokenUse::PairAgain
        );

        // What T3 0.1.1 answers for mustr's session: the write scope is only in `permissions`.
        let session = parse_session_check(
            br#"{"authenticated":true,"auth":{},"scopes":["orchestration:read","orchestration:operate"],"permissions":["orchestration:read","orchestration:operate","source-control:write"],"sessionMethod":"bearer-access-token"}"#,
        )
        .unwrap();
        assert_eq!(
            session,
            check(
                true,
                Some(&[
                    "orchestration:read",
                    "orchestration:operate",
                    "source-control:write"
                ])
            )
        );
        assert_eq!(stored_token_use(&session, all).0, StoredTokenUse::Use);
        // A server from before `permissions`.
        assert_eq!(
            parse_session_check(
                br#"{"authenticated":true,"auth":{},"scopes":["orchestration:read"]}"#
            )
            .unwrap(),
            check(true, Some(&["orchestration:read"]))
        );
        assert_eq!(
            parse_session_check(br#"{"authenticated":false,"auth":{}}"#).unwrap(),
            check(false, None)
        );
    }

    #[test]
    fn token_file_round_trips_with_private_permissions() {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("credentials.json");
        assert!(read_token_file(&path).unwrap().is_empty());

        let mut tokens = BTreeMap::new();
        tokens.insert(
            "mustr-t3:http://127.0.0.1:3773/".to_string(),
            json!({ "environmentId": "e", "accessToken": "t", "scope": "s", "expiresAt": 1 }),
        );
        write_token_file(&path, &tokens).unwrap();
        assert_eq!(read_token_file(&path).unwrap(), tokens);
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt as _;
            let mode = std::fs::metadata(&path).unwrap().permissions().mode();
            assert_eq!(mode & 0o777, 0o600);
        }
    }
}

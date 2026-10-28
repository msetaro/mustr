//! mustr's connection to a T3 Code server: checking the server, pairing with it, keeping the
//! bearer token, and holding the RPC socket open.
//!
//! mustr asks for exactly [`SCOPES`]: reading orchestration (pull requests, threads) and writing
//! to pull requests (comments, reviews, viewed marks). A stored token that lacks one of them is
//! replaced by pairing again.

use anyhow::{Context as _, Result, anyhow, bail};
use futures::{AsyncReadExt as _, FutureExt as _, future::BoxFuture};
use gpui::{
    App, AppContext as _, AsyncApp, BackgroundExecutor, Context, Entity, Global, SharedString,
    Task, WeakEntity,
};
use http_client::{AsyncBody, HttpClient, Method, Request, Url};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::{
    collections::BTreeMap,
    ffi::OsStr,
    path::{Path, PathBuf},
    sync::Arc,
    time::{Duration, SystemTime, UNIX_EPOCH},
};
use t3_client::{
    AccessToken, ClientMetadata, EnvironmentDescriptor, PullRequestFile, PullRequestRef,
    RpcConnection, Scope,
};
use util::ResultExt as _;

use crate::inbox::{PullRequestList, ThreadRow};

pub const DEFAULT_SERVER_URL: &str = "http://127.0.0.1:3773";
/// Overrides the server URL from settings (the `--t3-server` flag overrides both).
const SERVER_URL_ENV: &str = "MUSTR_T3_SERVER";
/// Stores tokens in this JSON file instead of the OS keychain. For tests and headless runs
/// only: the file is protected by file permissions alone.
pub const CREDENTIALS_FILE_ENV: &str = "MUSTR_CREDENTIALS_FILE";

/// The scopes mustr asks T3 for. Not `orchestration:operate` until mustr drives agents, and
/// never `access:*` or `relay:*`.
pub const SCOPES: [Scope; 2] = [Scope::OrchestrationRead, Scope::SourceControlWrite];
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

/// Where T3 keeps its data: `$T3CODE_HOME`, or `~/.t3`.
pub fn t3_home() -> Option<PathBuf> {
    std::env::var_os("T3CODE_HOME")
        .filter(|home| !home.is_empty())
        .map(PathBuf::from)
        .or_else(|| dirs::home_dir().map(|home| home.join(".t3")))
}

fn t3_executable_name() -> &'static str {
    if cfg!(windows) { "t3.exe" } else { "t3" }
}

/// Finds a local T3 CLI to mint a pairing credential with.
///
/// The CLI opens T3's database with its own migrations, so the runtime matching the server's
/// version comes first (`mustr-docs/T3-RUNTIME.md` §8). Then `t3` on `PATH`, which is T3's
/// launcher, then the newest installed runtime.
pub fn find_t3_cli(
    t3_home: Option<&Path>,
    path_env: Option<&OsStr>,
    server_version: Option<&str>,
) -> Option<PathBuf> {
    let versions_dir = t3_home.map(|home| home.join("runtime").join("versions"));
    if let (Some(versions_dir), Some(version)) = (&versions_dir, server_version)
        // The version comes from the server, so it must not be able to name another directory.
        && semver::Version::parse(version).is_ok()
    {
        let candidate = versions_dir.join(version).join(t3_executable_name());
        if is_executable(&candidate) {
            return Some(candidate);
        }
    }
    if let Some(path_env) = path_env {
        let on_path = std::env::split_paths(path_env)
            .filter(|dir| dir.is_absolute())
            .map(|dir| dir.join(t3_executable_name()))
            .find(|candidate| is_executable(candidate));
        if on_path.is_some() {
            return on_path;
        }
    }
    newest_runtime_cli(versions_dir.as_deref()?)
}

fn newest_runtime_cli(versions_dir: &Path) -> Option<PathBuf> {
    std::fs::read_dir(versions_dir)
        .ok()?
        .filter_map(|entry| {
            let entry = entry.ok()?;
            let version = semver::Version::parse(entry.file_name().to_str()?).ok()?;
            let cli = entry.path().join(t3_executable_name());
            is_executable(&cli).then_some((version, cli))
        })
        .max_by(|(left, _), (right, _)| left.cmp(right))
        .map(|(_, cli)| cli)
}

fn is_executable(path: &Path) -> bool {
    let Ok(metadata) = std::fs::metadata(path) else {
        return false;
    };
    if !metadata.is_file() {
        return false;
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt as _;
        metadata.permissions().mode() & 0o111 != 0
    }
    #[cfg(not(unix))]
    {
        true
    }
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
    for (key, _) in std::env::vars_os() {
        if let Some(key) = key.to_str()
            && (key.starts_with("T3_") || key.starts_with("T3CODE_"))
        {
            command.env_remove(key);
        }
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

    pub async fn recent_threads(&self, limit: usize) -> Result<Vec<ThreadRow>> {
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
        let snapshot: Value =
            serde_json::from_slice(&body).context("T3 sent a shell snapshot that isn't JSON")?;
        Ok(crate::inbox::recent_threads(snapshot, limit))
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

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ConnectionStatus {
    Connecting(SharedString),
    /// Automatic pairing isn't possible; the user can paste a pairing credential.
    NeedsPairing(SharedString),
    Connected {
        environment: SharedString,
        server_version: Option<SharedString>,
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
}

/// The app's one T3 connection, shared by every window.
pub struct T3Connection {
    server_url: Option<Url>,
    status: ConnectionStatus,
    session: Option<Arc<T3Session>>,
    /// Bumped on every (re)connect, so views can tell sessions apart and stale tasks can tell
    /// they've been superseded.
    generation: usize,
    connect_task: Option<Task<()>>,
    driver_task: Option<Task<()>>,
    current_rpc: CurrentRpc,
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

struct GlobalT3Connection(Entity<T3Connection>);

impl Global for GlobalT3Connection {}

pub fn init(server_url: Result<Url>, cx: &mut App) {
    let connection = cx.new(|cx| {
        let mut connection = T3Connection::new(server_url);
        connection.connect(None, cx);
        connection
    });
    cx.set_global(GlobalT3Connection(connection));
}

impl T3Connection {
    pub fn global(cx: &App) -> Entity<Self> {
        cx.global::<GlobalT3Connection>().0.clone()
    }

    fn new(server_url: Result<Url>) -> Self {
        let (server_url, status) = match server_url {
            Ok(url) => (Some(url), ConnectionStatus::Connecting("Starting".into())),
            Err(error) => (None, ConnectionStatus::Failed(format!("{error:#}").into())),
        };
        Self {
            server_url,
            status,
            session: None,
            generation: 0,
            connect_task: None,
            driver_task: None,
            current_rpc: CurrentRpc::default(),
        }
    }

    pub fn status(&self) -> &ConnectionStatus {
        &self.status
    }

    pub fn server_url(&self) -> Option<&Url> {
        self.server_url.as_ref()
    }

    pub fn generation(&self) -> usize {
        self.generation
    }

    pub fn current_rpc(&self) -> CurrentRpc {
        self.current_rpc.clone()
    }

    pub fn session(&self) -> Option<Arc<T3Session>> {
        self.session.clone()
    }

    pub fn reconnect(&mut self, cx: &mut Context<Self>) {
        self.connect(None, cx);
    }

    pub fn pair_with_credential(&mut self, credential: String, cx: &mut Context<Self>) {
        self.connect(Some(credential), cx);
    }

    fn connect(&mut self, pairing_credential: Option<String>, cx: &mut Context<Self>) {
        self.generation += 1;
        let generation = self.generation;
        self.current_rpc.set(None);
        if let Some(session) = self.session.take() {
            session.rpc.close();
        }
        self.driver_task = None;
        let Some(server_url) = self.server_url.clone() else {
            cx.notify();
            return;
        };
        self.status = ConnectionStatus::Connecting("Checking the T3 server".into());
        cx.notify();

        let http = cx.http_client();
        self.connect_task = Some(cx.spawn(async move |this, cx| {
            let outcome = establish(
                &this,
                generation,
                server_url,
                http,
                CredentialStore::from_env(),
                pairing_credential,
                cx,
            )
            .await;
            this.update(cx, |this, cx| {
                if this.generation != generation {
                    return;
                }
                this.status = match outcome {
                    Ok(ConnectOutcome::Connected { session, driver }) => {
                        this.watch_driver(generation, driver, cx);
                        let status = ConnectionStatus::Connected {
                            environment: session.descriptor.label.clone().into(),
                            server_version: session
                                .descriptor
                                .server_version
                                .clone()
                                .map(Into::into),
                        };
                        this.current_rpc.set(Some(session.rpc.clone()));
                        this.session = Some(session);
                        status
                    }
                    Ok(ConnectOutcome::NeedsPairing(reason)) => {
                        ConnectionStatus::NeedsPairing(reason.into())
                    }
                    Err(error) => {
                        log::error!("couldn't connect to T3: {error:#}");
                        ConnectionStatus::Failed(format!("{error:#}").into())
                    }
                };
                cx.notify();
            })
            .ok();
        }));
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
                this.status =
                    ConnectionStatus::Failed(format!("Disconnected from T3: {reason}").into());
                cx.notify();
            })
            .ok();
        }));
    }

    fn set_step(&mut self, generation: usize, step: &'static str, cx: &mut Context<Self>) {
        if self.generation == generation {
            self.status = ConnectionStatus::Connecting(step.into());
            cx.notify();
        }
    }
}

async fn establish(
    this: &WeakEntity<T3Connection>,
    generation: usize,
    server_url: Url,
    http: Arc<dyn HttpClient>,
    store: CredentialStore,
    pasted_credential: Option<String>,
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

    let mut token = None;
    // A token T3 still accepts but that lacks some of `SCOPES`, used if pairing again fails, so
    // mustr stays connected (without the missing scopes) instead of losing the connection.
    let mut fallback_token = None;
    if pasted_credential.is_none() {
        match store.read(&server_url, cx).await {
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
                    StoredTokenUse::Use => token = Some(candidate),
                    StoredTokenUse::PairAgain => {
                        log::info!(
                            "mustr's stored T3 token lacks {}; pairing again",
                            scope_list(&missing_scopes(candidate.scope.split_whitespace()))
                        );
                        fallback_token = Some(candidate);
                    }
                    StoredTokenUse::Discard => {
                        log::info!("T3 no longer accepts mustr's stored token; pairing again");
                        store.delete(&server_url, cx).await.log_err();
                    }
                }
            }
            Ok(Some(_)) => {
                log::info!(
                    "mustr's stored T3 token is expired or for another server; pairing again"
                );
                store.delete(&server_url, cx).await.log_err();
            }
            Ok(None) => {}
            Err(error) => log::warn!("couldn't read mustr's stored T3 token: {error:#}"),
        }
    }

    let token = match token {
        Some(token) => token,
        None => {
            let paired = pair(
                &server_url,
                &http,
                &descriptor,
                pasted_credential,
                &set_step,
                cx,
            )
            .await;
            let access_token = match (paired, fallback_token) {
                (Ok(access_token), _) => {
                    let stored =
                        StoredToken::new(&descriptor.environment_id, &access_token, unix_now());
                    if let Err(error) = store.write(&server_url, &stored, cx).await {
                        log::error!(
                            "couldn't save the T3 token, so mustr will pair again next launch: \
                             {error:#}"
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
                (Err(PairingFailure::NeedsPairing(reason)), None) => {
                    return Ok(ConnectOutcome::NeedsPairing(reason));
                }
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
            access_token
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
        "Connected to T3 environment {} ({})",
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
        }),
        driver,
    })
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
                    let home = t3_home();
                    let path_env = std::env::var_os("PATH");
                    let cli = find_t3_cli(
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

    fn make_cli(dir: &Path) -> PathBuf {
        std::fs::create_dir_all(dir).unwrap();
        let cli = dir.join(t3_executable_name());
        std::fs::write(&cli, "#!/bin/sh\n").unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt as _;
            std::fs::set_permissions(&cli, std::fs::Permissions::from_mode(0o755)).unwrap();
        }
        cli
    }

    #[test]
    fn finds_the_runtime_matching_the_server_first() {
        let temp = tempfile::tempdir().unwrap();
        let home = temp.path().join(".t3");
        let versions = home.join("runtime").join("versions");
        let matching = make_cli(&versions.join("0.1.1"));
        make_cli(&versions.join("0.1.10"));
        let bin = temp.path().join("bin");
        make_cli(&bin);

        let path_env = std::env::join_paths([&bin]).unwrap();
        assert_eq!(
            find_t3_cli(Some(&home), Some(&path_env), Some("0.1.1")),
            Some(matching)
        );
    }

    #[test]
    fn falls_back_to_path_then_to_the_newest_runtime() {
        let temp = tempfile::tempdir().unwrap();
        let home = temp.path().join(".t3");
        let versions = home.join("runtime").join("versions");
        make_cli(&versions.join("0.1.9"));
        let newest = make_cli(&versions.join("0.1.10"));
        make_cli(&versions.join("0.1.10-nightly.1"));
        std::fs::create_dir_all(versions.join("not-a-version")).unwrap();
        // A directory without the binary, e.g. an interrupted install.
        std::fs::create_dir_all(versions.join("0.3.0")).unwrap();
        let bin = temp.path().join("bin");
        let on_path = make_cli(&bin);
        let path_env = std::env::join_paths([temp.path().join("empty"), bin]).unwrap();

        // The server's version isn't installed, so `t3` on PATH wins.
        assert_eq!(
            find_t3_cli(Some(&home), Some(&path_env), Some("0.4.0")),
            Some(on_path)
        );
        // Without one on PATH: the newest by semver, not by string order ("0.1.9" > "0.1.10").
        let empty_path = std::env::join_paths([temp.path().join("empty")]).unwrap();
        assert_eq!(
            find_t3_cli(Some(&home), Some(&empty_path), None),
            Some(newest)
        );
    }

    #[test]
    fn ignores_server_versions_that_are_paths_and_non_executables() {
        let temp = tempfile::tempdir().unwrap();
        let home = temp.path().join(".t3");
        let versions = home.join("runtime").join("versions");
        make_cli(&temp.path().join("evil"));
        let not_executable = versions.join("0.1.1");
        std::fs::create_dir_all(&not_executable).unwrap();
        std::fs::write(not_executable.join(t3_executable_name()), "").unwrap();

        assert_eq!(find_t3_cli(Some(&home), None, Some("../../evil")), None);
        #[cfg(unix)]
        assert_eq!(find_t3_cli(Some(&home), None, Some("0.1.1")), None);
        assert_eq!(find_t3_cli(None, None, Some("0.1.1")), None);
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
    fn mustr_asks_for_reading_and_pull_request_writes_only() {
        assert_eq!(
            SCOPES.map(Scope::as_str),
            ["orchestration:read", "source-control:write"]
        );
        assert_eq!(
            pairing_command(),
            "t3 auth pairing create --scope orchestration:read --scope source-control:write \
             --label mustr"
        );
        assert_eq!(
            missing_scopes("orchestration:read".split_whitespace()),
            [Scope::SourceControlWrite]
        );
        assert!(missing_scopes(["source-control:write", "orchestration:read"]).is_empty());
    }

    #[test]
    fn stored_tokens_without_the_write_scope_are_replaced() {
        let check = |accepted: bool, scopes: Option<&[&str]>| SessionCheck {
            accepted,
            scopes: scopes.map(|scopes| scopes.iter().map(|scope| scope.to_string()).collect()),
        };
        let both = "orchestration:read source-control:write";
        assert_eq!(
            stored_token_use(&check(true, None), both),
            (StoredTokenUse::Use, both.to_string())
        );
        // A token from M0, paired read-only.
        assert_eq!(
            stored_token_use(&check(true, None), "orchestration:read"),
            (StoredTokenUse::PairAgain, "orchestration:read".to_string())
        );
        // T3's account of the session wins over what was stored.
        assert_eq!(
            stored_token_use(&check(true, Some(&["orchestration:read"])), both),
            (StoredTokenUse::PairAgain, "orchestration:read".to_string())
        );
        assert_eq!(
            stored_token_use(&check(false, None), both).0,
            StoredTokenUse::Discard
        );

        // What T3 0.1.1 answers for mustr's session: the write scope is only in `permissions`.
        let session = parse_session_check(
            br#"{"authenticated":true,"auth":{},"scopes":["orchestration:read"],"permissions":["orchestration:read","source-control:write"],"sessionMethod":"bearer-access-token"}"#,
        )
        .unwrap();
        assert_eq!(
            session,
            check(true, Some(&["orchestration:read", "source-control:write"]))
        );
        assert_eq!(stored_token_use(&session, both).0, StoredTokenUse::Use);
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

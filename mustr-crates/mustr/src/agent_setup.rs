//! The agents page (`mustr-docs/PARITY.md` §2.17, "Install and sign in to agents"): every agent
//! T3 Code can run, whether it's ready, and what it takes to make it ready, as T3's provider
//! setup does it (`settings/ProviderSetupSection.tsx`, `ProviderAuthenticationSection.tsx`,
//! `ProviderAuthTerminal.tsx` and `onboarding/WelcomeWizard.tsx`).
//!
//! - **Install:** T3 0.1.3 downloads and manages only Codex and Antigravity itself, into its own
//!   base directory (`provider.install.start`, `subscribe`, `cancel` and `remove`). Codex is
//!   T3's to manage once the user lets it ("Let T3 Code install Codex", T3's managed Codex). For
//!   the others, such as Claude Code, the page gives T3's commands to run where T3 runs.
//! - **Sign in and out:** `provider.auth.start`, `subscribe`, `respond`, `complete`, `cancel` and
//!   `logout`, for the agents T3 signs in to itself: a browser page or a device code, a
//!   terminal, or credentials.
//! - **Check again:** `server.refreshProviders`.
//!
//! Every call goes through the installed thread backend at T3's scope for it (`providers:manage`
//! for setup, a read for the install state and the refresh), so dry runs and the pairing's scopes
//! apply. The agents come from [`ServerConfigs`]. Home's "no agent can run" notice and a thread's
//! provider banner open the page ([`OpenAgentSetup`]).

#[cfg(test)]
mod tests;

use crate::nav_panel::register_rail_entry;
use anyhow::Result;
use editor::Editor;
use futures::{FutureExt as _, StreamExt as _, future::BoxFuture};
use gpui::{
    AnyElement, App, AppContext as _, ClipboardItem, Context, Entity, EventEmitter, FocusHandle,
    Focusable, FontWeight, Hsla, InteractiveElement as _, IntoElement, ParentElement as _, Render,
    SharedString, Styled as _, Subscription, Task, Window, div, px, relative,
};
use mustr_ui::{ButtonStyle, Card, Pill, TextButton, palette, tokens, tokens::scaled};
use serde_json::Value;
use std::{
    collections::{BTreeMap, HashMap},
    sync::Arc,
    time::Duration,
};
use t3_client::{
    RpcMethod,
    t3_api::{
        rpc,
        tagged::{
            ProviderAuthInteraction, ProviderAuthResponse,
            ServerUpdateSettingsPayloadProviderInstanceMutation,
        },
        types::{
            ExecutionEnvironmentPlatformOs, ProviderAuthCancelInput, ProviderAuthCompleteInput,
            ProviderAuthRespondInput, ProviderAuthResponseBrowser,
            ProviderAuthResponseBrowserAction, ProviderAuthResponseBrowserType,
            ProviderAuthResponseCredentials, ProviderAuthResponseCredentialsType,
            ProviderAuthResponseTerminal, ProviderAuthResponseTerminalType, ProviderAuthStartInput,
            ProviderAuthState, ProviderAuthStatePhase, ProviderInstallCancelInput,
            ProviderInstallState, ProviderInstallStatePhase, ProviderInstanceConfig,
            ProviderSetupInput, ServerProvider, ServerProviderAuthStatus, ServerProviderSetup,
            ServerProviderState, ServerSettings, ServerSettingsPatch, ServerUpdateSettingsPayload,
            ServerUpdateSettingsPayloadProviderInstanceMutationUpsert,
            ServerUpdateSettingsPayloadProviderInstanceMutationUpsertOperation,
        },
    },
};
use thread_chat::{
    Access, ServerConfigs, ThreadBackend, ThreadBackendExt as _,
    usage::{OpenAgentSetup, provider_name},
};
use ui::{
    Color, ContextMenu, DropdownMenu, DropdownStyle, Headline, HeadlineSize, Icon, IconName,
    IconPosition, IconSize, Label, LabelCommon as _, LabelSize, prelude::*,
};
use workspace::{
    Workspace,
    item::{Item, ItemEvent},
};

const CONTENT_MAX_WIDTH: f32 = 880.;
/// The last lines of a sign-in terminal shown.
const TERMINAL_LINES: usize = 14;
/// T3 takes a terminal's input in pieces of at most this many characters.
const TERMINAL_CHUNK: usize = 4_096;
/// The agents T3's onboarding offers first (`PRIMARY_AGENT_DRIVERS`).
const PRIMARY_DRIVERS: [&str; 2] = ["claudeAgent", "codex"];

gpui::actions!(
    mustr,
    [
        /// Opens the agents page: install the agents T3 Code runs, and sign in to them.
        OpenAgents,
    ]
);

/// Adds the page's rail entry and actions. Call once at startup, before windows open.
pub fn init(cx: &mut App) {
    register_rail_entry(
        "agent-setup",
        Icon::new(IconName::Blocks),
        "Agents: install and sign in",
        OpenAgents,
        |item| item.downcast::<AgentSetupView>().is_some(),
        cx,
    );
    cx.observe_new(|workspace: &mut Workspace, _, _| {
        workspace.register_action(|workspace, _: &OpenAgents, window, cx| {
            show(workspace, None, window, cx)
        });
        workspace.register_action(|workspace, action: &OpenAgentSetup, window, cx| {
            show(workspace, action.instance_id.clone(), window, cx)
        });
    })
    .detach();
}

/// Activates the agents page, or opens it, with `focus`'s agent first.
pub fn show(
    workspace: &mut Workspace,
    focus: Option<String>,
    window: &mut Window,
    cx: &mut Context<Workspace>,
) {
    let existing = workspace
        .panes()
        .iter()
        .find_map(|pane| pane.read(cx).items_of_type::<AgentSetupView>().next());
    if let Some(existing) = existing {
        if focus.is_some() {
            existing.update(cx, |view, cx| {
                view.focused = focus;
                cx.notify();
            });
        }
        workspace.activate_item(&existing, true, true, window, cx);
        return;
    }
    let backend = thread_chat::backend(cx);
    let view = cx.new(|cx| AgentSetupView::new(backend, focus, cx));
    workspace.add_item_to_active_pane(Box::new(view), None, true, window, cx);
}

// What the page says, from T3's provider status.

/// What an agent needs before it can run (`getOnboardingProviderState` in T3).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum AgentState {
    Checking,
    Disabled,
    Install,
    SignIn,
    Attention,
    Ready,
}

impl AgentState {
    fn label(self) -> &'static str {
        match self {
            Self::Checking => "Checking",
            Self::Disabled => "Turned off",
            Self::Install => "Not installed",
            Self::SignIn => "Signed out",
            Self::Attention => "Needs attention",
            Self::Ready => "Ready",
        }
    }
}

pub(crate) fn agent_state(provider: &ServerProvider) -> AgentState {
    if !provider.enabled || provider.status == ServerProviderState::Disabled {
        return AgentState::Disabled;
    }
    let unauthenticated = provider.auth.status == ServerProviderAuthStatus::Unauthenticated;
    if !provider.installed
        && provider.status == ServerProviderState::Warning
        && provider.auth.status == ServerProviderAuthStatus::Unknown
    {
        AgentState::Checking
    } else if !provider.installed {
        AgentState::Install
    } else if unauthenticated {
        AgentState::SignIn
    } else if provider.status == ServerProviderState::Ready {
        AgentState::Ready
    } else {
        AgentState::Attention
    }
}

/// An agent's status in a line and its detail (`getProviderSummary` in T3).
pub(crate) fn summary(provider: &ServerProvider) -> (String, Option<String>) {
    let message = provider
        .message
        .clone()
        .filter(|message| !message.trim().is_empty());
    let auth_label = provider
        .auth
        .label
        .clone()
        .or_else(|| provider.auth.type_.clone());
    let with_label = |headline: &str| match &auth_label {
        Some(label) => format!("{headline} · {label}"),
        None => headline.to_string(),
    };
    if !provider.enabled || provider.status == ServerProviderState::Disabled {
        return (
            "Turned off".into(),
            Some(message.unwrap_or_else(|| {
                "This agent is turned off for new threads in T3 Code's settings.".into()
            })),
        );
    }
    if !provider.installed {
        return (
            "Not found".into(),
            Some(message.unwrap_or_else(|| "Its CLI isn't on T3 Code's PATH.".into())),
        );
    }
    if provider.auth.status == ServerProviderAuthStatus::Unauthenticated {
        return (with_label("Not signed in"), message);
    }
    match provider.status {
        ServerProviderState::Warning => {
            (
                "Needs attention".into(),
                Some(message.unwrap_or_else(|| {
                    "It's installed, but T3 Code couldn't fully check it.".into()
                })),
            )
        }
        ServerProviderState::Error => (
            "Unavailable".into(),
            Some(message.unwrap_or_else(|| "It failed T3 Code's startup checks.".into())),
        ),
        _ if provider.auth.status == ServerProviderAuthStatus::Authenticated => {
            (with_label("Signed in"), message)
        }
        _ => ("Available".into(), message),
    }
}

fn install_active(install: Option<&ProviderInstallState>) -> bool {
    install.is_some_and(|install| {
        matches!(
            install.phase,
            ProviderInstallStatePhase::Downloading
                | ProviderInstallStatePhase::Extracting
                | ProviderInstallStatePhase::Verifying
        )
    })
}

fn auth_active(auth: Option<&ProviderAuthState>) -> bool {
    auth.is_some_and(|auth| {
        matches!(
            auth.phase,
            ProviderAuthStatePhase::Starting
                | ProviderAuthStatePhase::Waiting
                | ProviderAuthStatePhase::Verifying
        )
    })
}

/// Whether the agent is installed, by T3's own copy if not on its PATH.
fn is_installed(provider: &ServerProvider, install: Option<&ProviderInstallState>) -> bool {
    provider.installed || install.is_some_and(|install| install.installed_version.is_some())
}

fn megabytes(bytes: i64) -> f64 {
    bytes as f64 / 1_000_000.
}

/// How T3's copy of the agent is doing (`installationStatusMessage` in T3).
pub(crate) fn install_status(
    install: Option<&ProviderInstallState>,
    installed: bool,
    name: &str,
) -> String {
    match install.map(|install| &install.phase) {
        Some(ProviderInstallStatePhase::Downloading) => {
            let install = install.expect("a phase comes with its state");
            let total = install
                .total_bytes
                .map(|total| format!(" of {:.1} MB", megabytes(total)))
                .unwrap_or_default();
            format!(
                "Downloading {:.1} MB{total}.",
                megabytes(install.downloaded_bytes)
            )
        }
        Some(ProviderInstallStatePhase::Extracting) => format!("Extracting {name}."),
        Some(ProviderInstallStatePhase::Verifying) => "Checking the download.".into(),
        _ if installed => "Installed.".into(),
        _ => match install.and_then(|install| install.total_bytes) {
            Some(total) if total > 0 => format!("{} MB download.", megabytes(total).ceil()),
            _ => "Not installed.".into(),
        },
    }
}

/// The install button's label, as T3 words it.
pub(crate) fn install_label(
    install: Option<&ProviderInstallState>,
    installed: bool,
    name: &str,
) -> String {
    if let Some(install) = install
        && let Some(installed_version) = &install.installed_version
    {
        return match &install.version {
            Some(version) if version != installed_version => format!("Update {name}"),
            _ => format!("Reinstall {name}"),
        };
    }
    match install.map(|install| &install.phase) {
        Some(ProviderInstallStatePhase::Failed | ProviderInstallStatePhase::Cancelled) => {
            "Retry installation".into()
        }
        _ if installed => "Install T3 Code's copy".into(),
        _ => format!("Install {name}"),
    }
}

/// The interaction a sign-in waits on, in T3's tagged shape.
pub(crate) fn interaction(auth: Option<&ProviderAuthState>) -> Option<ProviderAuthInteraction> {
    let value = serde_json::to_value(auth?.interaction.as_ref()?).ok()?;
    serde_json::from_value(value).ok()
}

/// Whether the agent is signed in, as T3 decides it.
fn signed_in(provider: &ServerProvider, auth: Option<&ProviderAuthState>) -> bool {
    provider.auth.status == ServerProviderAuthStatus::Authenticated
        || (provider.auth.status == ServerProviderAuthStatus::Unknown
            && auth.is_some_and(|auth| auth.phase == ProviderAuthStatePhase::Succeeded))
}

/// What the account row says (`accountDescription` in T3's `ProviderAuthenticationSection`).
pub(crate) fn account_description(
    provider: &ServerProvider,
    auth: Option<&ProviderAuthState>,
    environment: &str,
) -> String {
    if auth_active(auth) {
        let auth = auth.expect("an active sign-in has its state");
        return match (&auth.phase, interaction(Some(auth))) {
            (ProviderAuthStatePhase::Starting, _) => auth
                .message
                .clone()
                .unwrap_or_else(|| "Starting sign-in…".into()),
            (ProviderAuthStatePhase::Verifying, _) => "Checking your account…".into(),
            (_, Some(ProviderAuthInteraction::Terminal(_))) => {
                "Finish signing in in the terminal below.".into()
            }
            (_, Some(ProviderAuthInteraction::Credentials(_))) => {
                "Enter your credentials below.".into()
            }
            _ => "Finish signing in in your browser.".into(),
        };
    }
    if signed_in(provider, auth) {
        return match provider.auth.email.as_deref().map(str::trim) {
            Some(email) if !email.is_empty() => format!("Signed in as {email}."),
            _ => "Signed in.".into(),
        };
    }
    if provider
        .setup
        .as_ref()
        .is_some_and(|setup| !setup.can_authenticate)
    {
        return "This agent can't sign in here. Follow its docs to finish setup.".into();
    }
    format!("Sign in on {environment}.")
}

/// The sign-in button's label.
pub(crate) fn sign_in_label(
    provider: &ServerProvider,
    auth: Option<&ProviderAuthState>,
) -> &'static str {
    if signed_in(provider, auth) {
        "Change account"
    } else if auth.is_some_and(|auth| {
        matches!(
            auth.phase,
            ProviderAuthStatePhase::Failed | ProviderAuthStatePhase::Cancelled
        )
    }) {
        "Retry sign-in"
    } else {
        "Sign in"
    }
}

/// The commands that install an agent and sign in to it where T3 runs, for the agents T3 leaves
/// to their own CLI (`NATIVE_INSTALL_COMMANDS` and `resolveOnboardingProviderLoginCommand` in
/// T3's `onboarding/providerReadiness.logic.ts`).
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct CliSetup {
    pub install: String,
    pub sign_in: String,
}

pub(crate) fn cli_setup(
    provider: &ServerProvider,
    settings: Option<&ServerSettings>,
    os: Option<&ExecutionEnvironmentPlatformOs>,
) -> Option<CliSetup> {
    let windows = matches!(os, Some(ExecutionEnvironmentPlatformOs::Windows));
    let (install, binary, sign_in) = match provider.driver.as_str() {
        "claudeAgent" => (
            if windows {
                "irm https://claude.ai/install.ps1 | iex"
            } else {
                "curl -fsSL https://claude.ai/install.sh | bash"
            },
            "claude",
            "auth login",
        ),
        "codex" => (
            if windows {
                "irm https://chatgpt.com/codex/install.ps1 | iex"
            } else {
                "curl -fsSL https://chatgpt.com/codex/install.sh | sh"
            },
            "codex",
            "login",
        ),
        _ => return None,
    };
    let configured = settings
        .and_then(|settings| settings.provider_instances.as_ref())
        .and_then(|instances| instances.get(&provider.instance_id))
        .and_then(|instance| instance.config.as_ref())
        .and_then(|config| config.get("binaryPath"))
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|path| !path.is_empty())
        .unwrap_or(binary);
    Some(CliSetup {
        install: install.to_string(),
        sign_in: format!("{} {sign_in}", quote_binary(configured, binary, os)),
    })
}

/// A binary path as a shell command word (`quoteProviderBinary` in T3).
fn quote_binary(path: &str, fallback: &str, os: Option<&ExecutionEnvironmentPlatformOs>) -> String {
    let windows = matches!(os, Some(ExecutionEnvironmentPlatformOs::Windows));
    let safe = !path.is_empty()
        && path
            .chars()
            .all(|char| char.is_ascii_alphanumeric() || "_./:\\-".contains(char));
    if safe && (windows || !path.contains('\\')) {
        return path.to_string();
    }
    match os {
        Some(ExecutionEnvironmentPlatformOs::Windows) => {
            format!("& '{}'", path.replace('\'', "''"))
        }
        Some(ExecutionEnvironmentPlatformOs::Darwin | ExecutionEnvironmentPlatformOs::Linux) => {
            let quote = |text: &str| format!("'{}'", text.replace('\'', "'\"'\"'"));
            match path.strip_prefix("~/").or_else(|| path.strip_prefix("~\\")) {
                Some(rest) => format!("~/{}", quote(rest)),
                None => quote(path),
            }
        }
        _ => fallback.to_string(),
    }
}

/// A sign-in terminal's output as plain lines: escape sequences dropped, and each line as the
/// last carriage return left it.
pub(crate) fn terminal_text(output: &str) -> String {
    let mut text = String::with_capacity(output.len());
    let mut chars = output.chars().peekable();
    while let Some(char) = chars.next() {
        match char {
            '\u{1b}' => match chars.next() {
                // CSI: parameters, then a final byte from @ to ~.
                Some('[') => {
                    for next in chars.by_ref() {
                        if ('@'..='~').contains(&next) {
                            break;
                        }
                    }
                }
                // OSC: up to BEL or ESC \.
                Some(']') => {
                    while let Some(next) = chars.next() {
                        if next == '\u{7}' {
                            break;
                        }
                        if next == '\u{1b}' {
                            chars.next_if_eq(&'\\');
                            break;
                        }
                    }
                }
                _ => {}
            },
            '\n' | '\r' | '\t' => text.push(char),
            char if char.is_control() => {}
            char => text.push(char),
        }
    }
    text.split('\n')
        .map(|line| {
            let line = line.strip_suffix('\r').unwrap_or(line);
            line.rsplit('\r').next().unwrap_or(line).trim_end()
        })
        .collect::<Vec<_>>()
        .join("\n")
}

/// The settings change that lets T3 manage Codex itself (T3's `setupMode: "managed"`), so it can
/// install Codex and sign in with ChatGPT.
pub(crate) fn managed_codex(
    provider: &ServerProvider,
    settings: Option<&ServerSettings>,
) -> ServerUpdateSettingsPayload {
    let mut instance = instance_config(provider, settings);
    instance.enabled = Some(true);
    let mut config = match instance.config.take() {
        Some(Value::Object(config)) => config,
        _ => serde_json::Map::new(),
    };
    config.insert("enabled".into(), Value::Bool(true));
    config.insert("setupMode".into(), Value::String("managed".into()));
    instance.config = Some(Value::Object(config));
    upsert(provider, instance)
}

/// The settings change that turns a turned-off agent on (T3's `enableInstance`).
pub(crate) fn turned_on(
    provider: &ServerProvider,
    settings: Option<&ServerSettings>,
) -> ServerUpdateSettingsPayload {
    let mut instance = instance_config(provider, settings);
    instance.enabled = Some(true);
    if let Some(Value::Object(config)) = &mut instance.config
        && config.contains_key("enabled")
    {
        config.insert("enabled".into(), Value::Bool(true));
    }
    upsert(provider, instance)
}

/// The models the user added to the agent's list in T3's settings (`customModels`, each a slug
/// or `{slug, name, capabilities}`), by slug.
pub(crate) fn custom_models(
    provider: &ServerProvider,
    settings: Option<&ServerSettings>,
) -> Vec<String> {
    instance_config(provider, settings)
        .config
        .as_ref()
        .and_then(|config| config.get("customModels"))
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter_map(|model| match model {
            Value::String(slug) => Some(slug.trim().to_string()),
            model => model
                .get("slug")?
                .as_str()
                .map(|slug| slug.trim().to_string()),
        })
        .filter(|slug| !slug.is_empty())
        .collect()
}

/// The settings change that adds `slug` to the agent's custom models, or removes it, keeping the
/// others as T3 has them. `None` when there's nothing to change.
pub(crate) fn change_custom_model(
    provider: &ServerProvider,
    settings: Option<&ServerSettings>,
    slug: &str,
    add: bool,
) -> Option<ServerUpdateSettingsPayload> {
    let slug = slug.trim();
    if slug.is_empty() {
        return None;
    }
    let mut instance = instance_config(provider, settings);
    let mut config = match instance.config.take() {
        Some(Value::Object(config)) => config,
        _ => serde_json::Map::new(),
    };
    let mut models = config
        .get("customModels")
        .and_then(Value::as_array)
        .cloned()
        .unwrap_or_default();
    let slug_of = |model: &Value| match model {
        Value::String(slug) => Some(slug.trim().to_string()),
        model => model
            .get("slug")?
            .as_str()
            .map(|slug| slug.trim().to_string()),
    };
    let present = models
        .iter()
        .any(|model| slug_of(model).as_deref() == Some(slug));
    match (add, present) {
        (true, false) => models.push(Value::String(slug.to_string())),
        (false, true) => models.retain(|model| slug_of(model).as_deref() != Some(slug)),
        _ => return None,
    }
    config.insert("customModels".into(), Value::Array(models));
    instance.config = Some(Value::Object(config));
    Some(upsert(provider, instance))
}

/// The agent's instance in T3's settings, or a new one for its driver.
fn instance_config(
    provider: &ServerProvider,
    settings: Option<&ServerSettings>,
) -> ProviderInstanceConfig {
    settings
        .and_then(|settings| settings.provider_instances.as_ref())
        .and_then(|instances| instances.get(&provider.instance_id))
        .cloned()
        .unwrap_or_else(|| ProviderInstanceConfig {
            accent_color: None,
            config: None,
            display_name: None,
            driver: provider.driver.clone(),
            enabled: None,
            environment: None,
        })
}

fn upsert(
    provider: &ServerProvider,
    instance: ProviderInstanceConfig,
) -> ServerUpdateSettingsPayload {
    ServerUpdateSettingsPayload {
        patch: ServerSettingsPatch::default(),
        provider_instance_mutation: Some(
            ServerUpdateSettingsPayloadProviderInstanceMutation::Upsert(
                ServerUpdateSettingsPayloadProviderInstanceMutationUpsert {
                    instance,
                    instance_id: provider.instance_id.clone(),
                    operation:
                        ServerUpdateSettingsPayloadProviderInstanceMutationUpsertOperation::Upsert,
                },
            ),
        ),
    }
}

/// Terminal input in the pieces T3 takes.
pub(crate) fn terminal_chunks(data: &str) -> Vec<String> {
    let chars = data.chars().collect::<Vec<_>>();
    if chars.is_empty() {
        return vec![String::new()];
    }
    chars
        .chunks(TERMINAL_CHUNK)
        .map(|chunk| chunk.iter().collect())
        .collect()
}

/// Agents in the order the page lists them: the one asked for, Claude Code and Codex, the others
/// by name; turned-off ones are listed apart.
fn ordered<'a>(providers: &'a [ServerProvider], focused: Option<&str>) -> Vec<&'a ServerProvider> {
    let mut providers = providers.iter().collect::<Vec<_>>();
    providers.sort_by_cached_key(|provider| {
        (
            Some(provider.instance_id.as_str()) != focused,
            PRIMARY_DRIVERS
                .iter()
                .position(|driver| *driver == provider.driver)
                .unwrap_or(PRIMARY_DRIVERS.len()),
            provider_name(provider).to_lowercase(),
            provider.instance_id.clone(),
        )
    });
    providers
}

/// What T3 sets up for the agent itself, if anything: Claude Code and Codex on their own CLI
/// come with a setup that allows neither.
fn managed_setup(provider: &ServerProvider) -> Option<&ServerProviderSetup> {
    provider
        .setup
        .as_ref()
        .filter(|setup| setup.can_install || setup.can_authenticate)
}

fn driver_icon(driver: &str) -> IconName {
    match driver {
        "claudeAgent" => IconName::AiClaude,
        "codex" => IconName::AiOpenAi,
        "antigravity" => IconName::AiGoogle,
        "cursor" => IconName::EditorCursor,
        "opencode" | "openCode" => IconName::AiOpenCode,
        _ => IconName::Terminal,
    }
}

// The page.

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Confirm {
    Remove,
    SignOut,
}

/// An agent's setup on the page: T3's install and sign-in state, and what the user is doing.
#[derive(Default)]
struct AgentSetup {
    install: Option<ProviderInstallState>,
    auth: Option<ProviderAuthState>,
    /// Why T3's setup state for the agent couldn't be followed.
    follow_error: Option<SharedString>,
    /// The command in flight, in words.
    pending: Option<&'static str>,
    error: Option<SharedString>,
    confirming: Option<Confirm>,
    /// The sign-in method picked, when the agent offers several; `None` is the agent's default.
    method_id: Option<String>,
    _install: Option<Task<()>>,
    _auth: Option<Task<()>>,
}

pub struct AgentSetupView {
    focus_handle: FocusHandle,
    backend: Option<Arc<dyn ThreadBackend>>,
    configs: Entity<ServerConfigs>,
    agents: HashMap<String, AgentSetup>,
    /// The agent the page was opened for, listed first.
    focused: Option<String>,
    show_turned_off: bool,
    /// The agent whose custom model is being typed.
    adding_model: Option<String>,
    refreshing: bool,
    error: Option<SharedString>,
    /// The command just copied, by its selector.
    copied: Option<String>,
    _tasks: Vec<Task<()>>,
    _subscriptions: Vec<Subscription>,
}

impl AgentSetupView {
    fn new(
        backend: Option<Arc<dyn ThreadBackend>>,
        focused: Option<String>,
        cx: &mut Context<Self>,
    ) -> Self {
        let configs = ServerConfigs::global(cx);
        let subscriptions = vec![cx.observe(&configs, |this, _, cx| {
            this.follow_agents(cx);
            cx.notify();
        })];
        let mut this = Self {
            focus_handle: cx.focus_handle(),
            backend,
            configs,
            agents: HashMap::default(),
            focused,
            show_turned_off: false,
            adding_model: None,
            refreshing: false,
            error: None,
            copied: None,
            _tasks: Vec::new(),
            _subscriptions: subscriptions,
        };
        this.follow_agents(cx);
        this
    }

    fn providers<'a>(&self, cx: &'a App) -> Option<&'a [ServerProvider]> {
        self.configs.read(cx).providers()
    }

    fn environment(&self, cx: &App) -> String {
        self.configs
            .read(cx)
            .environment()
            .map(|environment| environment.label.trim().to_string())
            .filter(|label| !label.is_empty())
            .unwrap_or_else(|| "the machine T3 Code runs on".into())
    }

    fn blocked(&self, access: Access) -> Option<String> {
        match &self.backend {
            Some(backend) => backend.blocked_reason(access),
            None => Some("mustr isn't connected to T3 Code".into()),
        }
    }

    /// Follows T3's install and sign-in state for each agent on that T3 sets up itself. The sign-in state
    /// needs `providers:manage`, so a dry run shows the account from T3's provider status alone.
    fn follow_agents(&mut self, cx: &mut Context<Self>) {
        let Some(providers) = self.providers(cx).map(<[_]>::to_vec) else {
            return;
        };
        let can_follow_auth = self.blocked(Access::ProvidersManage).is_none();
        for provider in providers {
            // A turned-off agent is followed once it's turned on.
            let Some(setup) = provider.setup.clone().filter(|_| provider.enabled) else {
                continue;
            };
            let id = provider.instance_id.clone();
            let agent = self.agents.entry(id.clone()).or_default();
            let follow_install = setup.can_install && agent._install.is_none();
            let follow_auth = setup.can_authenticate && can_follow_auth && agent._auth.is_none();
            if follow_install {
                let task = self.follow::<rpc::ProviderInstallSubscribe>(
                    id.clone(),
                    Access::Read,
                    |agent, state| agent.install = Some(state),
                    cx,
                );
                self.agents.entry(id.clone()).or_default()._install = Some(task);
            }
            if follow_auth {
                let task = self.follow::<rpc::ProviderAuthSubscribe>(
                    id.clone(),
                    Access::ProvidersManage,
                    |agent, state| agent.auth = Some(state),
                    cx,
                );
                self.agents.entry(id).or_default()._auth = Some(task);
            }
        }
    }

    /// Follows one of T3's per-agent setup streams, subscribing again when it ends, as it does
    /// when mustr reconnects.
    fn follow<M>(
        &self,
        instance_id: String,
        access: Access,
        apply: fn(&mut AgentSetup, M::Success),
        cx: &mut Context<Self>,
    ) -> Task<()>
    where
        M: RpcMethod<Payload = ProviderSetupInput>,
        M::Success: Send + 'static,
    {
        let backend = self.backend.clone();
        cx.spawn(async move |this, cx| {
            let Some(backend) = backend else {
                return;
            };
            let mut attempt = 0;
            loop {
                let stream = backend.stream::<M>(
                    ProviderSetupInput {
                        instance_id: instance_id.clone(),
                    },
                    access,
                );
                let error = match stream {
                    Ok(mut stream) => loop {
                        match stream.next().await {
                            Some(Ok(state)) => {
                                attempt = 0;
                                let updated = this.update(cx, |this, cx| {
                                    let agent = this.agents.entry(instance_id.clone()).or_default();
                                    agent.follow_error = None;
                                    apply(agent, state);
                                    cx.notify();
                                });
                                if updated.is_err() {
                                    return;
                                }
                            }
                            Some(Err(error)) => break Some(error),
                            None => break None,
                        }
                    },
                    Err(error) => Some(error),
                };
                if let Some(error) = error {
                    let unsupported = thread_chat::is_unsupported_rpc(&error);
                    log::warn!("couldn't follow {} for {instance_id}: {error:#}", M::TAG);
                    let message: SharedString = if unsupported {
                        "This T3 Code can't set up agents. Update it to install agents and sign in \
                         to them here."
                            .into()
                    } else {
                        format!("Couldn't read the agent's setup: {error:#}").into()
                    };
                    let updated = this.update(cx, |this, cx| {
                        this.agents
                            .entry(instance_id.clone())
                            .or_default()
                            .follow_error = Some(message);
                        cx.notify();
                    });
                    if updated.is_err() || unsupported {
                        return;
                    }
                }
                cx.background_executor().timer(retry_delay(attempt)).await;
                attempt += 1;
            }
        })
    }

    /// Runs one setup command for an agent, showing `label` while it's in flight and T3's error
    /// if it fails.
    fn run<T: Send + 'static>(
        &mut self,
        instance_id: &str,
        label: &'static str,
        call: BoxFuture<'static, Result<T>>,
        apply: fn(&mut AgentSetup, T),
        cx: &mut Context<Self>,
    ) {
        let agent = self.agents.entry(instance_id.to_string()).or_default();
        if agent.pending.is_some() {
            return;
        }
        agent.pending = Some(label);
        agent.error = None;
        agent.confirming = None;
        cx.notify();
        let instance_id = instance_id.to_string();
        self._tasks.push(cx.spawn(async move |this, cx| {
            let result = call.await;
            this.update(cx, |this, cx| {
                let agent = this.agents.entry(instance_id.clone()).or_default();
                agent.pending = None;
                match result {
                    Ok(value) => apply(agent, value),
                    Err(error) => {
                        log::warn!("{label} for {instance_id} failed: {error:#}");
                        agent.error = Some(format!("{error:#}").into());
                    }
                }
                cx.notify();
            })
            .ok();
        }));
    }

    fn backend(&self) -> Option<Arc<dyn ThreadBackend>> {
        self.backend.clone()
    }

    fn start_install(&mut self, instance_id: String, cx: &mut Context<Self>) {
        let Some(backend) = self.backend() else {
            return;
        };
        let call = backend.call::<rpc::ProviderInstallStart>(
            ProviderSetupInput {
                instance_id: instance_id.clone(),
            },
            Access::ProvidersManage,
        );
        self.run(&instance_id, "Starting installation", call, set_install, cx);
    }

    fn cancel_install(
        &mut self,
        instance_id: String,
        operation_id: String,
        cx: &mut Context<Self>,
    ) {
        let Some(backend) = self.backend() else {
            return;
        };
        let call = backend.call::<rpc::ProviderInstallCancel>(
            ProviderInstallCancelInput {
                instance_id: instance_id.clone(),
                operation_id,
            },
            Access::ProvidersManage,
        );
        self.run(
            &instance_id,
            "Cancelling installation",
            call,
            set_install,
            cx,
        );
    }

    fn remove_install(&mut self, instance_id: String, cx: &mut Context<Self>) {
        let Some(backend) = self.backend() else {
            return;
        };
        let call = backend.call::<rpc::ProviderInstallRemove>(
            ProviderSetupInput {
                instance_id: instance_id.clone(),
            },
            Access::ProvidersManage,
        );
        self.run(&instance_id, "Removing the download", call, set_install, cx);
    }

    fn start_sign_in(&mut self, instance_id: String, cx: &mut Context<Self>) {
        let Some(backend) = self.backend() else {
            return;
        };
        let method_id = self.agents.get(&instance_id).and_then(|agent| {
            let picked = agent.method_id.as_ref()?;
            agent
                .auth
                .as_ref()?
                .methods
                .as_ref()?
                .iter()
                .any(|method| &method.id == picked)
                .then(|| picked.clone())
        });
        let call = backend.call::<rpc::ProviderAuthStart>(
            ProviderAuthStartInput {
                callback_mode: None,
                instance_id: instance_id.clone(),
                method_id,
                return_url: None,
            },
            Access::ProvidersManage,
        );
        self.run(&instance_id, "Starting sign-in", call, set_auth, cx);
    }

    /// Answers the interaction the agent's sign-in waits on.
    pub(crate) fn respond(
        &mut self,
        instance_id: String,
        response: ProviderAuthResponse,
        cx: &mut Context<Self>,
    ) {
        let Some(backend) = self.backend() else {
            return;
        };
        let Some((flow_id, interaction_id)) = self.waiting_on(&instance_id) else {
            return;
        };
        let call = backend.call::<rpc::ProviderAuthRespond>(
            ProviderAuthRespondInput {
                flow_id,
                instance_id: instance_id.clone(),
                interaction_id,
                response,
            },
            Access::ProvidersManage,
        );
        self.run(&instance_id, "Sending", call, set_auth, cx);
    }

    /// Types `data` into the sign-in terminal, in the pieces T3 takes, one after another.
    pub(crate) fn type_in_terminal(
        &mut self,
        instance_id: String,
        data: String,
        cx: &mut Context<Self>,
    ) {
        let Some(backend) = self.backend() else {
            return;
        };
        let Some((flow_id, interaction_id)) = self.waiting_on(&instance_id) else {
            return;
        };
        let calls = terminal_chunks(&data)
            .into_iter()
            .map(|data| ProviderAuthRespondInput {
                flow_id: flow_id.clone(),
                instance_id: instance_id.clone(),
                interaction_id: interaction_id.clone(),
                response: ProviderAuthResponse::Terminal(ProviderAuthResponseTerminal {
                    data,
                    size: None,
                    type_: ProviderAuthResponseTerminalType::Terminal,
                }),
            })
            .collect::<Vec<_>>();
        let call = async move {
            let mut state = None;
            for input in calls {
                state = Some(
                    backend
                        .call::<rpc::ProviderAuthRespond>(input, Access::ProvidersManage)
                        .await?,
                );
            }
            state.ok_or_else(|| anyhow::anyhow!("nothing to send"))
        }
        .boxed();
        self.run(&instance_id, "Sending", call, set_auth, cx);
    }

    pub(crate) fn submit_credentials(
        &mut self,
        instance_id: String,
        values: BTreeMap<String, String>,
        cx: &mut Context<Self>,
    ) {
        self.respond(
            instance_id,
            ProviderAuthResponse::Credentials(ProviderAuthResponseCredentials {
                type_: ProviderAuthResponseCredentialsType::Credentials,
                values,
            }),
            cx,
        );
    }

    /// Finishes a browser sign-in with the address its last page landed on, when that page
    /// couldn't reach T3 (`provider.auth.complete`).
    pub(crate) fn complete_sign_in(
        &mut self,
        instance_id: String,
        callback_url: String,
        cx: &mut Context<Self>,
    ) {
        let callback_url = callback_url.trim().to_string();
        let Some(backend) = self.backend() else {
            return;
        };
        let Some(flow_id) = self.flow_id(&instance_id) else {
            return;
        };
        if callback_url.is_empty() {
            return;
        }
        let call = backend.call::<rpc::ProviderAuthComplete>(
            ProviderAuthCompleteInput {
                callback_url,
                flow_id,
                instance_id: instance_id.clone(),
            },
            Access::ProvidersManage,
        );
        self.run(&instance_id, "Finishing sign-in", call, set_auth, cx);
    }

    fn cancel_sign_in(&mut self, instance_id: String, cx: &mut Context<Self>) {
        let Some(backend) = self.backend() else {
            return;
        };
        let Some(flow_id) = self.flow_id(&instance_id) else {
            return;
        };
        let call = backend.call::<rpc::ProviderAuthCancel>(
            ProviderAuthCancelInput {
                flow_id,
                instance_id: instance_id.clone(),
            },
            Access::ProvidersManage,
        );
        self.run(&instance_id, "Cancelling sign-in", call, set_auth, cx);
    }

    fn sign_out(&mut self, instance_id: String, cx: &mut Context<Self>) {
        let Some(backend) = self.backend() else {
            return;
        };
        let call = backend.call::<rpc::ProviderAuthLogout>(
            ProviderSetupInput {
                instance_id: instance_id.clone(),
            },
            Access::ProvidersManage,
        );
        self.run(&instance_id, "Signing out", call, set_auth, cx);
    }

    /// Opens the sign-in page in the browser. A page that needs the user's consent is accepted
    /// with T3 first, as T3's Open browser does.
    fn open_sign_in_page(
        &mut self,
        instance_id: String,
        url: String,
        requires_consent: bool,
        cx: &mut Context<Self>,
    ) {
        if !requires_consent {
            cx.open_url(&url);
            return;
        }
        let Some(backend) = self.backend() else {
            return;
        };
        let Some((flow_id, interaction_id)) = self.waiting_on(&instance_id) else {
            return;
        };
        let call = backend.call::<rpc::ProviderAuthRespond>(
            ProviderAuthRespondInput {
                flow_id,
                instance_id: instance_id.clone(),
                interaction_id,
                response: accept_browser(),
            },
            Access::ProvidersManage,
        );
        let agent = self.agents.entry(instance_id.clone()).or_default();
        if agent.pending.is_some() {
            return;
        }
        agent.pending = Some("Opening the sign-in page");
        agent.error = None;
        cx.notify();
        self._tasks.push(cx.spawn(async move |this, cx| {
            let result = call.await;
            this.update(cx, |this, cx| {
                let agent = this.agents.entry(instance_id.clone()).or_default();
                agent.pending = None;
                match result {
                    Ok(state) => {
                        agent.auth = Some(state);
                        cx.open_url(&url);
                    }
                    Err(error) => agent.error = Some(format!("{error:#}").into()),
                }
                cx.notify();
            })
            .ok();
        }));
    }

    /// Copies the sign-in link, accepting the page with T3 when it needs consent.
    fn copy_sign_in_link(
        &mut self,
        instance_id: String,
        url: String,
        requires_consent: bool,
        cx: &mut Context<Self>,
    ) {
        cx.write_to_clipboard(ClipboardItem::new_string(url));
        self.copied = Some(format!("agent-copy-link-{instance_id}"));
        if requires_consent {
            self.respond(instance_id, accept_browser(), cx);
        }
        cx.notify();
    }

    fn copy(&mut self, selector: String, text: String, cx: &mut Context<Self>) {
        cx.write_to_clipboard(ClipboardItem::new_string(text));
        self.copied = Some(selector);
        cx.notify();
    }

    /// Asks T3 to check every agent again (`server.refreshProviders`), as after installing one
    /// from its CLI.
    fn refresh(&mut self, cx: &mut Context<Self>) {
        let Some(backend) = self.backend() else {
            return;
        };
        if self.refreshing {
            return;
        }
        self.refreshing = true;
        self.error = None;
        let call = backend.call::<rpc::ServerRefreshProviders>(Default::default(), Access::Read);
        cx.notify();
        self._tasks.push(cx.spawn(async move |this, cx| {
            let result = call.await;
            this.update(cx, |this, cx| {
                this.refreshing = false;
                match result {
                    Ok(update) => this.configs.update(cx, |configs, cx| {
                        configs.set_providers(update.providers, cx)
                    }),
                    Err(error) => {
                        this.error = Some(format!("Couldn't check the agents: {error:#}").into())
                    }
                }
                cx.notify();
            })
            .ok();
        }));
    }

    /// Changes an agent's instance in T3's settings (`server.updateSettings` with
    /// `providerInstanceMutation`, which needs `providers:manage`).
    fn change_instance(
        &mut self,
        instance_id: String,
        label: &'static str,
        payload: ServerUpdateSettingsPayload,
        cx: &mut Context<Self>,
    ) {
        let Some(backend) = self.backend() else {
            return;
        };
        let call = backend.call::<rpc::ServerUpdateSettings>(payload, Access::ProvidersManage);
        let configs = self.configs.clone();
        let agent = self.agents.entry(instance_id.clone()).or_default();
        if agent.pending.is_some() {
            return;
        }
        agent.pending = Some(label);
        agent.error = None;
        cx.notify();
        self._tasks.push(cx.spawn(async move |this, cx| {
            let result = call.await;
            this.update(cx, |this, cx| {
                let agent = this.agents.entry(instance_id.clone()).or_default();
                agent.pending = None;
                match result {
                    Ok(settings) => {
                        configs.update(cx, |configs, cx| configs.set_settings(settings, cx))
                    }
                    Err(error) => agent.error = Some(format!("{error:#}").into()),
                }
                cx.notify();
            })
            .ok();
        }));
    }

    /// Adds the model typed in `input` to the agent's custom models.
    fn add_custom_model(
        &mut self,
        provider: &ServerProvider,
        input: &Entity<Editor>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let slug = input.read(cx).text(cx);
        let settings = self.configs.read(cx).settings();
        if let Some(payload) = change_custom_model(provider, settings, &slug, true) {
            self.change_instance(
                provider.instance_id.clone(),
                "Adding the model",
                payload,
                cx,
            );
        }
        input.update(cx, |input, cx| input.clear(window, cx));
        self.adding_model = None;
        cx.notify();
    }

    fn flow_id(&self, instance_id: &str) -> Option<String> {
        self.agents.get(instance_id)?.auth.as_ref()?.flow_id.clone()
    }

    /// The sign-in flow and the interaction it waits on.
    fn waiting_on(&self, instance_id: &str) -> Option<(String, String)> {
        let auth = self.agents.get(instance_id)?.auth.as_ref()?;
        let interaction_id = match interaction(Some(auth))? {
            ProviderAuthInteraction::Browser(browser) => browser.id,
            ProviderAuthInteraction::DeviceCode(code) => code.id,
            ProviderAuthInteraction::Terminal(terminal) => terminal.id,
            ProviderAuthInteraction::Credentials(credentials) => credentials.id,
            ProviderAuthInteraction::Unrecognized(_) => return None,
        };
        Some((auth.flow_id.clone()?, interaction_id))
    }

    #[cfg(test)]
    fn agent(&self, instance_id: &str) -> Option<&AgentSetup> {
        self.agents.get(instance_id)
    }
}

fn set_install(agent: &mut AgentSetup, state: ProviderInstallState) {
    agent.install = Some(state);
}

fn set_auth(agent: &mut AgentSetup, state: ProviderAuthState) {
    agent.auth = Some(state);
}

fn accept_browser() -> ProviderAuthResponse {
    ProviderAuthResponse::Browser(ProviderAuthResponseBrowser {
        action: ProviderAuthResponseBrowserAction::Accept,
        type_: ProviderAuthResponseBrowserType::Browser,
    })
}

fn retry_delay(attempt: usize) -> Duration {
    const DELAYS: [u64; 4] = [1, 2, 5, 10];
    Duration::from_secs(DELAYS[attempt.min(DELAYS.len() - 1)])
}

// Rendering.

fn muted(text: impl Into<SharedString>) -> Label {
    Label::new(text.into())
        .size(LabelSize::Small)
        .color(Color::Muted)
}

fn state_color(state: AgentState, cx: &App) -> Hsla {
    let palette = palette(cx);
    match state {
        AgentState::Ready => palette.ok.hsla(),
        AgentState::Install | AgentState::SignIn | AgentState::Attention => palette.warning.hsla(),
        AgentState::Checking | AgentState::Disabled => palette.text_muted.hsla(),
    }
}

/// A command to run, in code type, with Copy.
fn code(text: impl Into<SharedString>, cx: &App) -> impl IntoElement {
    let palette = palette(cx);
    div()
        .min_w_0()
        .px_1p5()
        .py(scaled(px(1.)))
        .rounded(scaled(tokens::CONTROL_RADIUS))
        .bg(palette.surface_2.hsla())
        .font_family(tokens::CODE_FONT_FAMILY)
        .text_size(tokens::text_size(tokens::TEXT_SIZE_XSMALL))
        .text_color(palette.text.hsla())
        .child(text.into())
}

fn progress_bar(fraction: f32, cx: &App) -> impl IntoElement {
    let palette = palette(cx);
    div()
        .w_full()
        .h(scaled(px(4.)))
        .rounded_full()
        .bg(palette.surface_2.hsla())
        .child(
            div()
                .h_full()
                .w(relative(fraction.clamp(0., 1.)))
                .rounded_full()
                .bg(palette.accent.hsla()),
        )
}

/// A titled part of an agent's row: a title and what it says, with its controls beside them,
/// or under them when the row is narrow.
fn part(
    title: &'static str,
    description: impl Into<SharedString>,
    controls: Vec<AnyElement>,
) -> Div {
    h_flex()
        .w_full()
        .flex_wrap()
        .items_start()
        .justify_between()
        .gap_x_3()
        .gap_y_1()
        .child(
            v_flex()
                .flex_1()
                .min_w(scaled(px(220.)))
                .gap_0p5()
                .child(
                    Label::new(title)
                        .size(LabelSize::Small)
                        .weight(FontWeight::MEDIUM),
                )
                .child(muted(description)),
        )
        .child(h_flex().flex_none().gap_1().children(controls))
}

impl AgentSetupView {
    fn button(
        &self,
        selector: String,
        label: impl Into<SharedString>,
        style: ButtonStyle,
        blocked: Option<String>,
        cx: &mut Context<Self>,
        on_click: impl Fn(&mut Self, &mut Window, &mut Context<Self>) + 'static,
    ) -> AnyElement {
        let this = cx.entity().downgrade();
        let id_selector = selector.clone();
        div()
            .debug_selector(move || id_selector)
            .child(
                TextButton::new(SharedString::from(selector), label)
                    .style(style)
                    .disabled(blocked.is_some())
                    .when_some(blocked, |button, reason| button.tooltip(reason))
                    .on_click(move |_, window, cx| {
                        this.update(cx, |this, cx| on_click(this, window, cx)).ok();
                    }),
            )
            .into_any_element()
    }

    fn render_agent(
        &self,
        index: usize,
        provider: &ServerProvider,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let palette = palette(cx);
        let id = provider.instance_id.clone();
        let agent = self.agents.get(&id);
        let state = agent_state(provider);
        let name = provider_name(provider);
        let (headline, detail) = summary(provider);
        let focused = self.focused.as_deref() == Some(id.as_str());

        let mut parts: Vec<AnyElement> = Vec::new();
        match (managed_setup(provider), state) {
            (_, AgentState::Disabled) => {}
            (Some(setup), _) => {
                if setup.can_install {
                    parts.push(self.render_runtime(provider, cx));
                }
                parts.push(self.render_account(provider, window, cx));
            }
            (None, _) => parts.extend(self.render_cli(provider, cx)),
        }
        if state != AgentState::Disabled && provider.installed {
            parts.push(self.render_custom_models(provider, window, cx));
        }
        if let Some(agent) = agent {
            if let Some(error) = agent.follow_error.clone().or(agent.error.clone()) {
                parts.push(
                    div()
                        .debug_selector({
                            let id = id.clone();
                            move || format!("agent-error-{id}")
                        })
                        .child(Label::new(error).size(LabelSize::Small).color(Color::Error))
                        .into_any_element(),
                );
            }
            if let Some(pending) = agent.pending {
                parts.push(muted(format!("{pending}…")).into_any_element());
            }
        }

        let turn_on = (state == AgentState::Disabled).then(|| self.turn_on_button(provider, cx));
        let line = match state {
            AgentState::Ready => match &provider.auth.email {
                Some(email) if !email.trim().is_empty() => {
                    format!("Ready to code · {}", email.trim())
                }
                _ => "Ready to code.".to_string(),
            },
            // The pill says it's off.
            AgentState::Disabled => detail.unwrap_or(headline),
            _ => match detail {
                Some(detail) => format!("{headline} · {detail}"),
                None => headline,
            },
        };
        v_flex()
            .id(("agent", index))
            .debug_selector({
                let id = id.clone();
                move || format!("agent-{id}")
            })
            .w_full()
            .gap_2()
            .px_2()
            .py(scaled(px(10.)))
            .when(index > 0, |this| {
                this.border_t(tokens::BORDER_WIDTH)
                    .border_color(palette.border.hsla())
            })
            .when(focused, |this| {
                this.bg(palette.accent_soft.hsla())
                    .rounded(scaled(tokens::CONTROL_RADIUS))
            })
            .child(
                h_flex()
                    .w_full()
                    .gap_2()
                    .child(
                        Icon::new(driver_icon(&provider.driver))
                            .size(IconSize::Small)
                            .color(Color::Muted),
                    )
                    .child(
                        div()
                            .min_w_0()
                            .child(Label::new(name).weight(FontWeight::SEMIBOLD).truncate()),
                    )
                    .child(Pill::tinted(state.label(), state_color(state, cx)))
                    .when_some(
                        provider
                            .version
                            .clone()
                            .or(provider.installed_version.clone()),
                        |this, version| this.child(muted(version).size(LabelSize::XSmall)),
                    )
                    .when_some(turn_on, |this, turn_on| {
                        this.child(div().flex_1()).child(turn_on)
                    }),
            )
            .child(muted(line))
            .when(!parts.is_empty(), |this| {
                this.child(
                    v_flex()
                        .w_full()
                        .gap_2()
                        .pl(scaled(px(22.)))
                        .children(parts),
                )
            })
            .into_any_element()
    }

    fn turn_on_button(&self, provider: &ServerProvider, cx: &mut Context<Self>) -> AnyElement {
        let id = provider.instance_id.clone();
        let busy = self
            .agents
            .get(&id)
            .is_some_and(|agent| agent.pending.is_some());
        let blocked = self
            .blocked(Access::ProvidersManage)
            .or_else(|| busy.then(|| "Busy".to_string()));
        let provider = provider.clone();
        self.button(
            format!("agent-turn-on-{id}"),
            "Turn on",
            ButtonStyle::Default,
            blocked,
            cx,
            move |this, _, cx| {
                let payload = turned_on(&provider, this.configs.read(cx).settings());
                this.change_instance(provider.instance_id.clone(), "Turning on", payload, cx);
            },
        )
    }

    fn render_runtime(&self, provider: &ServerProvider, cx: &mut Context<Self>) -> AnyElement {
        let id = provider.instance_id.clone();
        let agent = self.agents.get(&id);
        let install = agent.and_then(|agent| agent.install.as_ref());
        let auth = agent.and_then(|agent| agent.auth.as_ref());
        let name = provider_name(provider);
        let installed = is_installed(provider, install);
        let active = install_active(install);
        let busy = agent.is_some_and(|agent| agent.pending.is_some());
        let blocked = self
            .blocked(Access::ProvidersManage)
            .or_else(|| busy.then(|| "Busy".to_string()));
        let status = install_status(install, installed, &name);

        let mut controls = Vec::new();
        let confirming = agent.and_then(|agent| agent.confirming) == Some(Confirm::Remove);
        if confirming {
            controls.push(self.button(
                format!("agent-confirm-remove-{id}"),
                "Remove",
                ButtonStyle::Primary,
                blocked,
                cx,
                {
                    let id = id.clone();
                    move |this, _, cx| this.remove_install(id.clone(), cx)
                },
            ));
            controls.push(self.button(
                format!("agent-keep-{id}"),
                "Keep",
                ButtonStyle::Default,
                None,
                cx,
                {
                    let id = id.clone();
                    move |this, _, cx| {
                        if let Some(agent) = this.agents.get_mut(&id) {
                            agent.confirming = None;
                        }
                        cx.notify();
                    }
                },
            ));
        } else if active {
            if let Some(operation_id) = install.and_then(|install| install.operation_id.clone()) {
                controls.push(self.button(
                    format!("agent-cancel-install-{id}"),
                    "Cancel installation",
                    ButtonStyle::Default,
                    blocked,
                    cx,
                    {
                        let id = id.clone();
                        move |this, _, cx| this.cancel_install(id.clone(), operation_id.clone(), cx)
                    },
                ));
            }
        } else {
            // T3 waits for the install state before offering to install, and not mid sign-in.
            let blocked = blocked.or_else(|| {
                if install.is_none() {
                    Some("Checking the installation".to_string())
                } else if auth_active(auth) {
                    Some("Finish signing in first".to_string())
                } else {
                    None
                }
            });
            controls.push(self.button(
                format!("agent-install-{id}"),
                install_label(install, installed, &name),
                if installed {
                    ButtonStyle::Default
                } else {
                    ButtonStyle::Primary
                },
                blocked.clone(),
                cx,
                {
                    let id = id.clone();
                    move |this, _, cx| this.start_install(id.clone(), cx)
                },
            ));
            if install.is_some_and(|install| install.can_remove) {
                controls.push(self.button(
                    format!("agent-remove-{id}"),
                    "Remove",
                    ButtonStyle::Default,
                    blocked,
                    cx,
                    {
                        let id = id.clone();
                        move |this, _, cx| {
                            this.agents.entry(id.clone()).or_default().confirming =
                                Some(Confirm::Remove);
                            cx.notify();
                        }
                    },
                ));
            }
        }

        let description = if confirming {
            format!(
                "Remove T3 Code's copy of {name} from {}? Its sign-in and your threads are kept.",
                self.environment(cx)
            )
        } else {
            status.clone()
        };
        let progress = install.and_then(|install| {
            let total = install.total_bytes.filter(|total| *total > 0)?;
            (install.phase == ProviderInstallStatePhase::Downloading)
                .then(|| install.downloaded_bytes as f32 / total as f32)
        });
        let message = install
            .and_then(|install| install.message.clone())
            .filter(|message| !active && *message != status && !message.trim().is_empty());
        v_flex()
            .w_full()
            .gap_1()
            .debug_selector({
                let id = id.clone();
                move || format!("agent-runtime-{id}")
            })
            .child(part("Runtime", description, controls))
            .when_some(progress, |this, fraction| {
                this.child(progress_bar(fraction, cx))
            })
            .when_some(message, |this, message| {
                this.child(muted(message).size(LabelSize::XSmall))
            })
            .into_any_element()
    }

    fn render_account(
        &self,
        provider: &ServerProvider,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let id = provider.instance_id.clone();
        let agent = self.agents.get(&id);
        let auth = agent.and_then(|agent| agent.auth.as_ref());
        let install = agent.and_then(|agent| agent.install.as_ref());
        let active = auth_active(auth);
        let signed_in = signed_in(provider, auth);
        let busy = agent.is_some_and(|agent| agent.pending.is_some());
        let blocked = self
            .blocked(Access::ProvidersManage)
            .or_else(|| busy.then(|| "Busy".to_string()));
        let environment = self.environment(cx);
        let interaction = interaction(auth);
        let can_authenticate = provider
            .setup
            .as_ref()
            .is_some_and(|setup| setup.can_authenticate);
        let confirming = agent.and_then(|agent| agent.confirming) == Some(Confirm::SignOut);

        let mut controls = Vec::new();
        let methods = auth
            .and_then(|auth| auth.methods.clone())
            .unwrap_or_default();
        if !active && methods.len() > 1 {
            controls.push(self.render_methods(&id, &methods, blocked.is_some(), window, cx));
        }
        let url = match &interaction {
            Some(ProviderAuthInteraction::Browser(browser)) => Some(browser.url.clone()),
            Some(ProviderAuthInteraction::DeviceCode(code)) => Some(code.url.clone()),
            _ => auth.and_then(|auth| auth.authorization_url.clone()),
        };
        let requires_consent = matches!(
            &interaction,
            Some(ProviderAuthInteraction::Browser(browser)) if browser.requires_consent
        );
        if let Some(url) = url.clone().filter(|_| active) {
            controls.push(self.button(
                format!("agent-open-browser-{id}"),
                "Open browser",
                ButtonStyle::Primary,
                blocked.clone(),
                cx,
                {
                    let (id, url) = (id.clone(), url.clone());
                    move |this, _, cx| {
                        this.open_sign_in_page(id.clone(), url.clone(), requires_consent, cx)
                    }
                },
            ));
            let copied = self.copied.as_deref() == Some(&format!("agent-copy-link-{id}"));
            controls.push(self.button(
                format!("agent-copy-link-{id}"),
                if copied { "Copied" } else { "Copy link" },
                ButtonStyle::Default,
                blocked.clone(),
                cx,
                {
                    let id = id.clone();
                    move |this, _, cx| {
                        this.copy_sign_in_link(id.clone(), url.clone(), requires_consent, cx)
                    }
                },
            ));
        }
        if confirming {
            controls.push(self.button(
                format!("agent-confirm-sign-out-{id}"),
                "Sign out",
                ButtonStyle::Primary,
                blocked.clone(),
                cx,
                {
                    let id = id.clone();
                    move |this, _, cx| this.sign_out(id.clone(), cx)
                },
            ));
            controls.push(self.button(
                format!("agent-stay-signed-in-{id}"),
                "Stay signed in",
                ButtonStyle::Default,
                None,
                cx,
                {
                    let id = id.clone();
                    move |this, _, cx| {
                        if let Some(agent) = this.agents.get_mut(&id) {
                            agent.confirming = None;
                        }
                        cx.notify();
                    }
                },
            ));
        } else if active {
            if auth.is_some_and(|auth| auth.flow_id.is_some()) {
                controls.push(self.button(
                    format!("agent-cancel-sign-in-{id}"),
                    "Cancel",
                    ButtonStyle::Default,
                    blocked.clone(),
                    cx,
                    {
                        let id = id.clone();
                        move |this, _, cx| this.cancel_sign_in(id.clone(), cx)
                    },
                ));
            }
        } else if can_authenticate {
            let installed = is_installed(provider, install);
            // T3 offers sign-in once the agent is on and installed and it has the sign-in state,
            // and sign-out once it has the sign-in state.
            let checking = auth.is_none().then(|| "Checking the sign-in".to_string());
            let sign_in_blocked = blocked.clone().or_else(|| {
                if !provider.enabled {
                    Some("Turn the agent on first".to_string())
                } else if !installed {
                    Some("Install the agent first".to_string())
                } else {
                    checking.clone()
                }
            });
            controls.push(self.button(
                format!("agent-sign-in-{id}"),
                sign_in_label(provider, auth),
                if signed_in {
                    ButtonStyle::Default
                } else {
                    ButtonStyle::Primary
                },
                sign_in_blocked,
                cx,
                {
                    let id = id.clone();
                    move |this, _, cx| this.start_sign_in(id.clone(), cx)
                },
            ));
            let can_sign_out = provider.auth.can_logout.unwrap_or(can_authenticate);
            if signed_in && can_sign_out {
                controls.push(self.button(
                    format!("agent-sign-out-{id}"),
                    "Sign out",
                    ButtonStyle::Default,
                    blocked.clone().or(checking),
                    cx,
                    {
                        let id = id.clone();
                        move |this, _, cx| {
                            this.agents.entry(id.clone()).or_default().confirming =
                                Some(Confirm::SignOut);
                            cx.notify();
                        }
                    },
                ));
            }
        } else if let Some(docs) = provider
            .setup
            .as_ref()
            .and_then(|setup| setup.documentation_url.clone())
        {
            controls.push(self.button(
                format!("agent-docs-{id}"),
                "Open docs",
                ButtonStyle::Default,
                None,
                cx,
                move |_, _, cx| cx.open_url(&docs),
            ));
        }

        let description = if confirming {
            format!(
                "Sign out of {} on {environment}? Running threads that share this sign-in stop. \
                 Your threads are kept.",
                provider_name(provider)
            )
        } else {
            account_description(provider, auth, &environment)
        };
        let failure = auth
            .filter(|auth| auth.phase == ProviderAuthStatePhase::Failed)
            .and_then(|auth| auth.message.clone());
        let mut body: Vec<AnyElement> = Vec::new();
        if active {
            body.extend(self.render_interaction(
                &id,
                interaction.as_ref(),
                url.as_deref(),
                blocked,
                window,
                cx,
            ));
        }
        v_flex()
            .w_full()
            .gap_1()
            .debug_selector({
                let id = id.clone();
                move || format!("agent-account-{id}")
            })
            .child(part("Account", description, controls))
            .when_some(failure, |this, failure| {
                this.child(
                    Label::new(failure)
                        .size(LabelSize::Small)
                        .color(Color::Error),
                )
            })
            .children(body)
            .into_any_element()
    }

    fn render_methods(
        &self,
        id: &str,
        methods: &[t3_client::t3_api::types::ProviderAuthMethod],
        disabled: bool,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let picked = self
            .agents
            .get(id)
            .and_then(|agent| agent.method_id.clone())
            .filter(|picked| methods.iter().any(|method| &method.id == picked));
        let label = picked
            .as_ref()
            .and_then(|picked| methods.iter().find(|method| &method.id == picked))
            .map_or_else(
                || "Agent's default".to_string(),
                |method| method.name.clone(),
            );
        let mut entries = vec![(None, "Agent's default".to_string())];
        entries.extend(
            methods
                .iter()
                .map(|method| (Some(method.id.clone()), method.name.clone())),
        );
        let this = cx.entity().downgrade();
        let instance_id = id.to_string();
        let key = SharedString::from(format!(
            "agent-methods-{id}-{}-{}",
            entries.len(),
            picked.clone().unwrap_or_default()
        ));
        let menu = window.use_keyed_state(key, cx, move |window, cx| {
            ContextMenu::new(window, cx, move |mut menu, _, _| {
                for (method_id, name) in entries {
                    let this = this.clone();
                    let instance_id = instance_id.clone();
                    let selected = method_id == picked;
                    menu = menu.toggleable_entry(
                        name,
                        selected,
                        IconPosition::End,
                        None,
                        move |_, cx| {
                            let method_id = method_id.clone();
                            this.update(cx, |this, cx| {
                                this.agents
                                    .entry(instance_id.clone())
                                    .or_default()
                                    .method_id = method_id;
                                cx.notify();
                            })
                            .ok();
                        },
                    );
                }
                menu
            })
        });
        DropdownMenu::new(
            SharedString::from(format!("agent-method-{id}")),
            label,
            menu,
        )
        .style(DropdownStyle::Outlined)
        .disabled(disabled)
        .into_any_element()
    }

    /// What a sign-in in progress waits on: a device code, a terminal, credentials, or the
    /// address the browser landed on.
    fn render_interaction(
        &self,
        id: &str,
        interaction: Option<&ProviderAuthInteraction>,
        url: Option<&str>,
        blocked: Option<String>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Vec<AnyElement> {
        let palette = palette(cx);
        let flow = self.flow_id(id).unwrap_or_default();
        let mut body: Vec<AnyElement> = Vec::new();
        match interaction {
            Some(ProviderAuthInteraction::DeviceCode(device)) => {
                let selector = format!("agent-copy-code-{id}");
                let copied = self.copied.as_deref() == Some(selector.as_str());
                let code_text = device.user_code.clone();
                body.push(
                    h_flex()
                        .gap_2()
                        .child(muted("Enter this code in your browser:"))
                        .child(code(device.user_code.clone(), cx))
                        .child(self.button(
                            selector.clone(),
                            if copied { "Copied" } else { "Copy code" },
                            ButtonStyle::Default,
                            None,
                            cx,
                            move |this, _, cx| this.copy(selector.clone(), code_text.clone(), cx),
                        ))
                        .into_any_element(),
                );
            }
            Some(ProviderAuthInteraction::Terminal(terminal)) => {
                let text = terminal_text(&terminal.output);
                let lines = text.lines().collect::<Vec<_>>();
                let shown = lines[lines.len().saturating_sub(TERMINAL_LINES)..].join("\n");
                let input = window.use_keyed_state(
                    SharedString::from(format!("agent-terminal-{id}-{flow}-{}", terminal.id)),
                    cx,
                    |window, cx| {
                        let mut editor = Editor::single_line(window, cx);
                        editor.set_placeholder_text("Type here, then Send", window, cx);
                        editor
                    },
                );
                body.push(
                    div()
                        .debug_selector({
                            let id = id.to_string();
                            move || format!("agent-terminal-{id}")
                        })
                        .w_full()
                        .px_2()
                        .py_1p5()
                        .rounded(scaled(tokens::CONTROL_RADIUS))
                        .border(tokens::BORDER_WIDTH)
                        .border_color(palette.border.hsla())
                        .bg(palette.background.hsla())
                        .font_family(tokens::CODE_FONT_FAMILY)
                        .text_size(tokens::text_size(tokens::TEXT_SIZE_XSMALL))
                        .text_color(palette.text.hsla())
                        .whitespace_normal()
                        .children(shown.lines().map(|line| div().child(line.to_string())))
                        .into_any_element(),
                );
                let send = {
                    let (id, input) = (id.to_string(), input.clone());
                    self.button(
                        format!("agent-send-{id}"),
                        "Send",
                        ButtonStyle::Default,
                        blocked.clone(),
                        cx,
                        move |this, window, cx| {
                            let text = input.read(cx).text(cx);
                            input.update(cx, |input, cx| input.clear(window, cx));
                            this.type_in_terminal(id.clone(), format!("{text}\r"), cx);
                        },
                    )
                };
                body.push(
                    h_flex()
                        .w_full()
                        .gap_2()
                        .child(field_box(input.into_any_element(), cx))
                        .child(send)
                        .into_any_element(),
                );
            }
            Some(ProviderAuthInteraction::Credentials(credentials)) => {
                let mut editors = Vec::new();
                for field in &credentials.fields {
                    let editor = window.use_keyed_state(
                        SharedString::from(format!(
                            "agent-credential-{id}-{flow}-{}-{}",
                            credentials.id, field.name
                        )),
                        cx,
                        {
                            let secret = field.secret;
                            move |window, cx| {
                                let mut editor = Editor::single_line(window, cx);
                                editor.set_masked(secret, cx);
                                editor
                            }
                        },
                    );
                    body.push(
                        h_flex()
                            .w_full()
                            .gap_2()
                            .child(
                                div()
                                    .w(scaled(px(120.)))
                                    .flex_none()
                                    .child(muted(field.label.clone())),
                            )
                            .child(field_box(editor.clone().into_any_element(), cx))
                            .into_any_element(),
                    );
                    editors.push((field.name.clone(), editor));
                }
                let id = id.to_string();
                body.push(self.button(
                    format!("agent-connect-{id}"),
                    "Connect",
                    ButtonStyle::Primary,
                    blocked.clone(),
                    cx,
                    move |this, _, cx| {
                        let values = editors
                            .iter()
                            .map(|(name, editor)| (name.clone(), editor.read(cx).text(cx)))
                            .collect();
                        this.submit_credentials(id.clone(), values, cx);
                    },
                ));
            }
            _ => {}
        }
        let accepts_callback = match interaction {
            Some(ProviderAuthInteraction::Browser(browser)) => {
                browser.accepts_callback.unwrap_or(false)
            }
            None => url.is_some(),
            _ => false,
        };
        if accepts_callback {
            let input = window.use_keyed_state(
                SharedString::from(format!("agent-callback-{id}-{flow}")),
                cx,
                |window, cx| {
                    let mut editor = Editor::single_line(window, cx);
                    editor.set_placeholder_text("http://localhost:…", window, cx);
                    editor
                },
            );
            let id = id.to_string();
            let cont = {
                let input = input.clone();
                self.button(
                    format!("agent-continue-{id}"),
                    "Continue",
                    ButtonStyle::Default,
                    blocked,
                    cx,
                    move |this, _, cx| {
                        let text = input.read(cx).text(cx);
                        this.complete_sign_in(id.clone(), text, cx);
                    },
                )
            };
            body.push(
                v_flex()
                    .w_full()
                    .gap_1()
                    .child(muted(
                        "If the last page, on localhost, doesn't load, paste its whole address here.",
                    ))
                    .child(
                        h_flex()
                            .w_full()
                            .gap_2()
                            .child(field_box(input.into_any_element(), cx))
                            .child(cont),
                    )
                    .into_any_element(),
            );
        }
        body
    }

    /// For agents T3 leaves to their own CLI: the commands to run where T3 runs, and for Codex,
    /// the offer to let T3 install and sign in to it itself.
    fn render_cli(&self, provider: &ServerProvider, cx: &mut Context<Self>) -> Vec<AnyElement> {
        let state = agent_state(provider);
        if matches!(state, AgentState::Ready | AgentState::Checking) {
            return Vec::new();
        }
        let id = provider.instance_id.clone();
        let configs = self.configs.read(cx);
        let os = configs
            .environment()
            .map(|environment| environment.platform.os.clone());
        let Some(commands) = cli_setup(provider, configs.settings(), os.as_ref()) else {
            return vec![muted(format!(
                "Install {} where T3 Code runs and sign in with its CLI, then choose Check again.",
                provider_name(provider)
            ))
            .into_any_element()];
        };
        let environment = self.environment(cx);
        let mut steps: Vec<(&'static str, &'static str, String)> = Vec::new();
        if !provider.installed {
            steps.push(("Install", "install", commands.install));
        }
        steps.push(("Sign in", "sign-in", commands.sign_in));
        let mut parts = vec![
            muted(format!(
                "T3 Code uses this agent's own CLI. Run these on {environment}, then choose Check again."
            ))
            .into_any_element(),
        ];
        for (label, kind, command) in steps {
            let selector = format!("agent-copy-{kind}-{id}");
            let copied = self.copied.as_deref() == Some(selector.as_str());
            let copy = {
                let (selector, command) = (selector.clone(), command.clone());
                self.button(
                    selector.clone(),
                    if copied { "Copied" } else { "Copy" },
                    ButtonStyle::Default,
                    None,
                    cx,
                    move |this, _, cx| this.copy(selector.clone(), command.clone(), cx),
                )
            };
            parts.push(
                h_flex()
                    .w_full()
                    .gap_2()
                    .child(div().w(scaled(px(56.))).flex_none().child(muted(label)))
                    .child(div().flex_1().min_w_0().child(code(command, cx)))
                    .child(copy)
                    .into_any_element(),
            );
        }
        if provider.driver == "codex" {
            let busy = self
                .agents
                .get(&id)
                .is_some_and(|agent| agent.pending.is_some());
            let blocked = self
                .blocked(Access::ProvidersManage)
                .or_else(|| busy.then(|| "Busy".to_string()));
            let provider = provider.clone();
            let manage = self.button(
                format!("agent-manage-{id}"),
                "Let T3 Code install Codex",
                ButtonStyle::Default,
                blocked,
                cx,
                move |this, _, cx| {
                    let payload = managed_codex(&provider, this.configs.read(cx).settings());
                    this.change_instance(
                        provider.instance_id.clone(),
                        "Setting up Codex",
                        payload,
                        cx,
                    );
                },
            );
            parts.push(
                part(
                    "Or let T3 Code set it up",
                    "T3 Code downloads its own copy of Codex and signs in with ChatGPT.",
                    vec![manage],
                )
                .into_any_element(),
            );
        }
        parts
    }

    /// Models the agent can run that T3 doesn't list, as T3's custom model editor adds them
    /// (`customModels` in the agent's settings, which `providers:manage` guards). T3 then lists
    /// them with the agent's models, so every model picker offers them.
    fn render_custom_models(
        &self,
        provider: &ServerProvider,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let palette = palette(cx);
        let id = provider.instance_id.clone();
        let models = custom_models(provider, self.configs.read(cx).settings());
        let busy = self
            .agents
            .get(&id)
            .is_some_and(|agent| agent.pending.is_some());
        let blocked = self
            .blocked(Access::ProvidersManage)
            .or_else(|| busy.then(|| "Busy".to_string()));
        let adding = self.adding_model.as_deref() == Some(id.as_str());
        let mut controls = Vec::new();
        if !adding {
            controls.push(self.button(
                format!("agent-add-model-{id}"),
                "Add model",
                ButtonStyle::Default,
                blocked.clone(),
                cx,
                {
                    let id = id.clone();
                    move |this, _, cx| {
                        this.adding_model = Some(id.clone());
                        cx.notify();
                    }
                },
            ));
        }
        let description = if models.is_empty() {
            "None. Add a model the agent runs that T3 Code doesn't list yet, by its id."
        } else {
            "Listed with the agent's own models."
        };
        let chips = models.iter().map(|slug| {
            let remove = {
                let provider = provider.clone();
                let slug = slug.clone();
                self.button(
                    format!("agent-remove-model-{id}-{slug}"),
                    "Remove",
                    ButtonStyle::Default,
                    blocked.clone(),
                    cx,
                    move |this, _, cx| {
                        let settings = this.configs.read(cx).settings();
                        if let Some(payload) =
                            change_custom_model(&provider, settings, &slug, false)
                        {
                            this.change_instance(
                                provider.instance_id.clone(),
                                "Removing the model",
                                payload,
                                cx,
                            );
                        }
                    },
                )
            };
            h_flex()
                .gap_2()
                .child(code(slug.clone(), cx))
                .child(remove)
                .into_any_element()
        });
        let chips = chips.collect::<Vec<_>>();
        let editor = adding.then(|| {
            let input = window.use_keyed_state(
                SharedString::from(format!("agent-model-input-{id}")),
                cx,
                |window, cx| {
                    let mut editor = Editor::single_line(window, cx);
                    editor.set_placeholder_text("Model id, such as gpt-5.5-codex", window, cx);
                    editor
                },
            );
            let add = {
                let (provider, input) = (provider.clone(), input.clone());
                self.button(
                    format!("agent-save-model-{id}"),
                    "Add",
                    ButtonStyle::Primary,
                    blocked.clone(),
                    cx,
                    move |this, window, cx| this.add_custom_model(&provider, &input, window, cx),
                )
            };
            let confirm = {
                let (provider, input) = (provider.clone(), input.clone());
                let blocked = blocked.is_some();
                cx.listener(move |this, _: &menu::Confirm, window, cx| {
                    if !blocked {
                        this.add_custom_model(&provider, &input, window, cx)
                    }
                })
            };
            let cancel = self.button(
                format!("agent-cancel-model-{id}"),
                "Cancel",
                ButtonStyle::Default,
                None,
                cx,
                |this, _, cx| {
                    this.adding_model = None;
                    cx.notify();
                },
            );
            h_flex()
                .w_full()
                .gap_2()
                .child(
                    div()
                        .key_context("MustrCustomModel")
                        .on_action(confirm)
                        .flex_1()
                        .min_w_0()
                        .px_2()
                        .py_1()
                        .rounded(scaled(tokens::CONTROL_RADIUS))
                        .border(tokens::BORDER_WIDTH)
                        .border_color(palette.border.hsla())
                        .bg(palette.background.hsla())
                        .child(input),
                )
                .child(add)
                .child(cancel)
        });
        v_flex()
            .w_full()
            .gap_1()
            .debug_selector({
                let id = id.clone();
                move || format!("agent-models-{id}")
            })
            .child(part("Custom models", description, controls))
            .children(chips)
            .children(editor)
            .into_any_element()
    }

    fn render_list(&self, window: &mut Window, cx: &mut Context<Self>) -> AnyElement {
        if self.backend.is_none() {
            return Card::new()
                .child(muted("mustr isn't connected to T3 Code."))
                .into_any_element();
        }
        let Some(providers) = self.providers(cx).map(<[_]>::to_vec) else {
            return Card::new()
                .child(muted("Loading the agents…"))
                .into_any_element();
        };
        if providers.is_empty() {
            return Card::new()
                .child(muted("T3 Code lists no agents."))
                .into_any_element();
        }
        let ordered = ordered(&providers, self.focused.as_deref());
        let (on, off): (Vec<_>, Vec<_>) = ordered
            .into_iter()
            .partition(|provider| agent_state(provider) != AgentState::Disabled);
        let mut rows = Vec::new();
        for provider in &on {
            rows.push(self.render_agent(rows.len(), provider, window, cx));
        }
        if self.show_turned_off {
            for provider in &off {
                rows.push(self.render_agent(rows.len(), provider, window, cx));
            }
        }
        let toggle = (!off.is_empty()).then(|| {
            self.button(
                "agents-show-off".into(),
                if self.show_turned_off {
                    "Hide turned-off agents".to_string()
                } else {
                    format!("Show turned-off agents ({})", off.len())
                },
                ButtonStyle::Default,
                None,
                cx,
                |this, _, cx| {
                    this.show_turned_off = !this.show_turned_off;
                    cx.notify();
                },
            )
        });
        v_flex()
            .w_full()
            .gap_2()
            .child(
                Card::new()
                    .px(scaled(px(10.)))
                    .py(scaled(px(4.)))
                    .children(rows),
            )
            .children(toggle.map(|toggle| h_flex().child(toggle)))
            .into_any_element()
    }
}

/// An editor in an outlined box, like mustr's other text fields.
fn field_box(editor: AnyElement, cx: &App) -> impl IntoElement {
    let palette = palette(cx);
    div()
        .flex_1()
        .min_w_0()
        .px_2()
        .py_1()
        .rounded(scaled(tokens::CONTROL_RADIUS))
        .border(tokens::BORDER_WIDTH)
        .border_color(palette.border.hsla())
        .bg(palette.background.hsla())
        .child(editor)
}

impl EventEmitter<ItemEvent> for AgentSetupView {}

impl Focusable for AgentSetupView {
    fn focus_handle(&self, _: &App) -> FocusHandle {
        self.focus_handle.clone()
    }
}

impl Item for AgentSetupView {
    type Event = ItemEvent;

    fn tab_content_text(&self, _detail: usize, _cx: &App) -> SharedString {
        "Agents".into()
    }

    fn tab_icon(&self, _window: &Window, _cx: &App) -> Option<Icon> {
        Some(Icon::new(IconName::Blocks))
    }

    fn show_toolbar(&self) -> bool {
        false
    }

    fn to_item_events(event: &Self::Event, f: &mut dyn FnMut(ItemEvent)) {
        f(*event)
    }
}

impl Render for AgentSetupView {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let palette = palette(cx);
        let environment = self.environment(cx);
        let count = self.providers(cx).map(|providers| {
            providers
                .iter()
                .filter(|provider| agent_state(provider) == AgentState::Ready)
                .count()
        });
        let refresh = self.button(
            "agents-refresh".into(),
            if self.refreshing {
                "Checking…"
            } else {
                "Check again"
            },
            ButtonStyle::Default,
            self.blocked(Access::Read)
                .or_else(|| self.refreshing.then(|| "Checking".to_string())),
            cx,
            |this, _, cx| this.refresh(cx),
        );
        let header = h_flex()
            .w_full()
            .items_end()
            .gap_3()
            .child(
                v_flex()
                    .flex_1()
                    .min_w_0()
                    .gap_0p5()
                    .child(
                        h_flex()
                            .gap_2()
                            .items_baseline()
                            .child(Headline::new("Agents").size(HeadlineSize::Large))
                            .children(count.map(|count| muted(format!("{count} ready")))),
                    )
                    .child(muted(format!(
                        "The coding agents T3 Code runs on {environment}. Install them and sign in \
                         here; each agent's sign-in is shared with its own CLI."
                    ))),
            )
            .child(refresh);
        v_flex()
            .key_context("MustrAgentSetup")
            .track_focus(&self.focus_handle)
            .size_full()
            .bg(palette.background.hsla())
            .child(
                v_flex()
                    .id("agent-setup-scroll")
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
                            .child(header)
                            .when_some(self.error.clone(), |this, error| {
                                this.child(
                                    Label::new(error).size(LabelSize::Small).color(Color::Error),
                                )
                            })
                            .child(self.render_list(window, cx)),
                    ),
            )
    }
}

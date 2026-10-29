use anyhow::{Context as _, Result, anyhow, bail};
use http_client::{HttpClient, Url};
use serde::Deserialize;
use t3_api::http as endpoint;
use t3_api::types::{
    AuthAccessTokenResult, AuthClientMetadataDeviceType, AuthTokenExchangeRequest,
    AuthTokenExchangeRequestGrantType, AuthTokenExchangeRequestRequestedTokenType,
    AuthTokenExchangeRequestSubjectTokenType,
};

use crate::endpoints::{self, call_endpoint};

/// T3 rejects clients whose protocol differs from the server's, in either direction.
/// Bumped by `script/mustr-sync-t3` from T3's `packages/contracts/src/environment.ts`.
pub const ORCHESTRATION_PROTOCOL_VERSION: u32 = 2;
pub const ORCHESTRATION_PROTOCOL_HEADER: &str = "x-t3-orchestration-protocol";

const ORCHESTRATION_PROTOCOL_QUERY_PARAM: &str = "orchestrationProtocol";

/// The T3 release mustr is built and tested against (`t3_server_version` in `T3_VERSION`). mustr
/// installs and runs exactly this version when it manages T3's engine itself.
pub fn pinned_server_version() -> &'static str {
    include_str!("../T3_VERSION")
        .lines()
        .find_map(|line| line.strip_prefix("t3_server_version")?.strip_prefix('='))
        .map(str::trim)
        .unwrap_or_default()
}
const WEBSOCKET_TICKET_QUERY_PARAM: &str = "wsTicket";

/// The scopes mustr asks for. mustr never requests `access:write` or `relay:*`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Scope {
    OrchestrationRead,
    OrchestrationOperate,
    SourceControlWrite,
    TerminalRead,
    TerminalOperate,
    FilesystemRead,
    SettingsWrite,
    ProvidersManage,
    DiagnosticsRead,
}

impl Scope {
    pub fn as_str(self) -> &'static str {
        match self {
            Scope::OrchestrationRead => "orchestration:read",
            Scope::OrchestrationOperate => "orchestration:operate",
            Scope::SourceControlWrite => "source-control:write",
            Scope::TerminalRead => "terminal:read",
            Scope::TerminalOperate => "terminal:operate",
            Scope::FilesystemRead => "filesystem:read",
            Scope::SettingsWrite => "settings:write",
            Scope::ProvidersManage => "providers:manage",
            Scope::DiagnosticsRead => "diagnostics:read",
        }
    }
}

/// What `GET /.well-known/t3/environment` says about a server. Hand-written rather than
/// `t3_api::types::ExecutionEnvironmentDescriptor`: mustr reads it from servers of every
/// version to tell the user which side to update, so it requires only what all of them send.
#[derive(Clone, Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct EnvironmentDescriptor {
    pub environment_id: String,
    pub label: String,
    /// Servers released before protocol negotiation omit this and speak version 1.
    pub orchestration_protocol_version: Option<u32>,
    pub server_version: Option<String>,
    #[serde(default)]
    pub capabilities: serde_json::Value,
}

impl EnvironmentDescriptor {
    pub fn protocol_version(&self) -> u32 {
        self.orchestration_protocol_version.unwrap_or(1)
    }
}

pub fn check_protocol_compatibility(descriptor: &EnvironmentDescriptor) -> Result<()> {
    let server_version = descriptor.protocol_version();
    if server_version == ORCHESTRATION_PROTOCOL_VERSION {
        return Ok(());
    }
    if server_version > ORCHESTRATION_PROTOCOL_VERSION {
        bail!(
            "{} speaks T3 orchestration protocol {server_version}, but this mustr speaks {ORCHESTRATION_PROTOCOL_VERSION}. Update mustr.",
            descriptor.label
        )
    }
    bail!(
        "{} speaks T3 orchestration protocol {server_version}, but this mustr needs {ORCHESTRATION_PROTOCOL_VERSION}. Update T3 Code on that machine.",
        descriptor.label
    )
}

/// How the client appears in T3's connections list.
#[derive(Clone, Debug)]
pub struct ClientMetadata {
    pub label: String,
    pub operating_system: String,
}

impl Default for ClientMetadata {
    fn default() -> Self {
        Self {
            label: "mustr".to_string(),
            operating_system: std::env::consts::OS.to_string(),
        }
    }
}

#[derive(Clone, Deserialize)]
pub struct AccessToken {
    pub access_token: String,
    pub token_type: String,
    pub expires_in: f64,
    pub scope: String,
}

impl From<AuthAccessTokenResult> for AccessToken {
    fn from(result: AuthAccessTokenResult) -> Self {
        Self {
            access_token: result.access_token,
            token_type: result.token_type.to_string(),
            expires_in: result.expires_in.0,
            scope: result.scope,
        }
    }
}

impl std::fmt::Debug for AccessToken {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("AccessToken")
            .field("token_type", &self.token_type)
            .field("expires_in", &self.expires_in)
            .field("scope", &self.scope)
            .finish_non_exhaustive()
    }
}

pub async fn fetch_descriptor(
    http: &dyn HttpClient,
    base_url: &Url,
) -> Result<EnvironmentDescriptor> {
    endpoints::send::<endpoint::MetadataDescriptor, _>(http, base_url, None, &(), &(), &()).await
}

/// Trades a one-time pairing credential for a bearer token (OAuth 2.0 token exchange).
pub async fn exchange_pairing_credential(
    http: &dyn HttpClient,
    base_url: &Url,
    pairing_credential: &str,
    scopes: &[Scope],
    client: &ClientMetadata,
) -> Result<AccessToken> {
    let scope = scopes
        .iter()
        .map(|scope| scope.as_str())
        .collect::<Vec<_>>()
        .join(" ");
    let request = AuthTokenExchangeRequest {
        grant_type: AuthTokenExchangeRequestGrantType::UrnIetfParamsOauthGrantTypeTokenExchange,
        subject_token: pairing_credential.to_string(),
        subject_token_type:
            AuthTokenExchangeRequestSubjectTokenType::UrnT3ParamsOauthTokenTypeEnvironmentBootstrap,
        requested_token_type:
            AuthTokenExchangeRequestRequestedTokenType::UrnIetfParamsOauthTokenTypeAccessToken,
        client_label: Some(client.label.clone()),
        client_device_type: Some(AuthClientMetadataDeviceType::Desktop),
        client_os: Some(client.operating_system.clone()),
        scope: (!scope.is_empty()).then_some(scope),
    };
    let result = call_endpoint::<endpoint::AuthToken>(http, base_url, None, &request).await?;
    Ok(result.into())
}

/// A short-lived ticket that authorizes one WebSocket connection.
pub async fn websocket_ticket(
    http: &dyn HttpClient,
    base_url: &Url,
    access_token: &AccessToken,
) -> Result<String> {
    let result =
        call_endpoint::<endpoint::AuthWebSocketTicket>(http, base_url, Some(access_token), &())
            .await?;
    Ok(result.ticket)
}

pub fn websocket_url(base_url: &Url, ticket: &str) -> Result<Url> {
    let mut url = base_url
        .join("/ws")
        .with_context(|| format!("invalid T3 server URL {base_url}"))?;
    let scheme = match base_url.scheme() {
        "https" => "wss",
        "http" => "ws",
        other => bail!("unsupported T3 server URL scheme {other:?}"),
    };
    url.set_scheme(scheme)
        .map_err(|()| anyhow!("can't build a WebSocket URL from {base_url}"))?;
    url.query_pairs_mut()
        .append_pair(
            ORCHESTRATION_PROTOCOL_QUERY_PARAM,
            &ORCHESTRATION_PROTOCOL_VERSION.to_string(),
        )
        .append_pair(WEBSOCKET_TICKET_QUERY_PARAM, ticket);
    Ok(url)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn descriptor(version: Option<u32>) -> EnvironmentDescriptor {
        EnvironmentDescriptor {
            environment_id: "environment".to_string(),
            label: "ironforge".to_string(),
            orchestration_protocol_version: version,
            server_version: None,
            capabilities: serde_json::Value::Null,
        }
    }

    #[test]
    fn protocol_must_match_exactly() {
        assert!(
            check_protocol_compatibility(&descriptor(Some(ORCHESTRATION_PROTOCOL_VERSION))).is_ok()
        );
        assert!(
            check_protocol_compatibility(&descriptor(Some(ORCHESTRATION_PROTOCOL_VERSION + 1)))
                .is_err()
        );
        assert!(check_protocol_compatibility(&descriptor(None)).is_err());
    }

    #[test]
    fn websocket_url_carries_protocol_and_ticket() -> Result<()> {
        let base_url = Url::parse("http://127.0.0.1:3773")?;
        let url = websocket_url(&base_url, "ticket value")?;
        assert_eq!(
            url.as_str(),
            "ws://127.0.0.1:3773/ws?orchestrationProtocol=2&wsTicket=ticket+value"
        );
        Ok(())
    }

    #[test]
    fn scopes_are_t3_scopes() {
        for scope in [
            Scope::OrchestrationRead,
            Scope::OrchestrationOperate,
            Scope::SourceControlWrite,
            Scope::TerminalRead,
            Scope::TerminalOperate,
            Scope::FilesystemRead,
            Scope::SettingsWrite,
            Scope::ProvidersManage,
            Scope::DiagnosticsRead,
        ] {
            assert!(
                t3_api::types::AuthGrantScope::KNOWN_VALUES.contains(&scope.as_str()),
                "T3 can't grant {:?}",
                scope.as_str()
            );
            assert!(
                t3_api::types::AuthEnvironmentScope::KNOWN_VALUES.contains(&scope.as_str()),
                "T3 has no scope {:?}",
                scope.as_str()
            );
        }
    }

    #[test]
    fn token_request_is_t3s_form() -> Result<()> {
        let request = AuthTokenExchangeRequest {
            grant_type: AuthTokenExchangeRequestGrantType::UrnIetfParamsOauthGrantTypeTokenExchange,
            subject_token: "pairing".to_string(),
            subject_token_type:
                AuthTokenExchangeRequestSubjectTokenType::UrnT3ParamsOauthTokenTypeEnvironmentBootstrap,
            requested_token_type:
                AuthTokenExchangeRequestRequestedTokenType::UrnIetfParamsOauthTokenTypeAccessToken,
            client_label: Some("mustr".to_string()),
            client_device_type: Some(AuthClientMetadataDeviceType::Desktop),
            client_os: None,
            scope: Some("orchestration:read terminal:read".to_string()),
        };
        assert_eq!(
            serde_urlencoded::to_string(&request)?,
            "client_device_type=desktop&client_label=mustr\
             &grant_type=urn%3Aietf%3Aparams%3Aoauth%3Agrant-type%3Atoken-exchange\
             &requested_token_type=urn%3Aietf%3Aparams%3Aoauth%3Atoken-type%3Aaccess_token\
             &scope=orchestration%3Aread+terminal%3Aread&subject_token=pairing\
             &subject_token_type=urn%3At3%3Aparams%3Aoauth%3Atoken-type%3Aenvironment-bootstrap"
        );
        Ok(())
    }

    #[test]
    fn the_protocol_constant_matches_the_pins() -> Result<()> {
        let pin = |key: &str| {
            include_str!("../T3_VERSION")
                .lines()
                .find_map(|line| line.strip_prefix(key)?.strip_prefix('='))
                .map(str::trim)
        };
        assert_eq!(
            pin("orchestration_protocol"),
            Some(ORCHESTRATION_PROTOCOL_VERSION.to_string().as_str())
        );
        // t3_api's generated types must come from the pinned T3 release.
        let source: serde_json::Value =
            serde_json::from_str(include_str!("../../t3_api/schema/SOURCE.json"))?;
        assert_eq!(pin("t3_server_version"), source["t3Version"].as_str());
        // The orchestration endpoints require the same version, by T3's contract.
        assert_eq!(
            <endpoint::OrchestrationShellSnapshot as t3_api::HttpEndpoint>::CONSTANT_HEADERS,
            &[(
                ORCHESTRATION_PROTOCOL_HEADER,
                ORCHESTRATION_PROTOCOL_VERSION.to_string().as_str()
            )]
        );
        let commit = pin("contracts_commit").unwrap_or_default();
        assert!(
            !commit.is_empty()
                && source["t3Commit"]
                    .as_str()
                    .is_some_and(|full| full.starts_with(commit)),
            "T3_VERSION pins contracts {commit:?}, t3_api was generated from {}",
            source["t3Commit"]
        );
        Ok(())
    }

    #[test]
    fn the_pinned_server_version_is_semver() {
        let version = pinned_server_version();
        assert!(
            version.split('.').count() == 3
                && version.split('.').all(|part| part.parse::<u32>().is_ok()),
            "T3_VERSION pins {version:?}"
        );
    }

    #[test]
    fn access_token_debug_hides_the_secret() {
        let token = AccessToken {
            access_token: "secret".to_string(),
            token_type: "Bearer".to_string(),
            expires_in: 60.0,
            scope: "orchestration:read".to_string(),
        };
        assert!(!format!("{token:?}").contains("secret"));
    }
}

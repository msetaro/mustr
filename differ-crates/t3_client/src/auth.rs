use anyhow::{Context as _, Result, anyhow, bail};
use futures::AsyncReadExt as _;
use http_client::{AsyncBody, HttpClient, Method, Request, Url};
use serde::{Deserialize, de::DeserializeOwned};

/// T3 rejects clients whose protocol differs from the server's, in either direction.
/// Bumped by `script/differ-sync-t3` from T3's `packages/contracts/src/environment.ts`.
pub const ORCHESTRATION_PROTOCOL_VERSION: u32 = 2;
pub const ORCHESTRATION_PROTOCOL_HEADER: &str = "x-t3-orchestration-protocol";

const ORCHESTRATION_PROTOCOL_QUERY_PARAM: &str = "orchestrationProtocol";
const WEBSOCKET_TICKET_QUERY_PARAM: &str = "wsTicket";
const TOKEN_EXCHANGE_GRANT_TYPE: &str = "urn:ietf:params:oauth:grant-type:token-exchange";
const ENVIRONMENT_BOOTSTRAP_TOKEN_TYPE: &str =
    "urn:t3:params:oauth:token-type:environment-bootstrap";
const ACCESS_TOKEN_TYPE: &str = "urn:ietf:params:oauth:token-type:access_token";

/// The scopes Differ asks for. Differ never requests `access:write` or `relay:*`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Scope {
    OrchestrationRead,
    OrchestrationOperate,
    SourceControlWrite,
    TerminalRead,
    TerminalOperate,
}

impl Scope {
    pub fn as_str(self) -> &'static str {
        match self {
            Scope::OrchestrationRead => "orchestration:read",
            Scope::OrchestrationOperate => "orchestration:operate",
            Scope::SourceControlWrite => "source-control:write",
            Scope::TerminalRead => "terminal:read",
            Scope::TerminalOperate => "terminal:operate",
        }
    }
}

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
            "{} speaks T3 orchestration protocol {server_version}, but this Differ speaks {ORCHESTRATION_PROTOCOL_VERSION}. Update Differ.",
            descriptor.label
        )
    }
    bail!(
        "{} speaks T3 orchestration protocol {server_version}, but this Differ needs {ORCHESTRATION_PROTOCOL_VERSION}. Update T3 Code on that machine.",
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
            label: "Differ".to_string(),
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
    let request = Request::builder()
        .method(Method::GET)
        .uri(endpoint(base_url, "/.well-known/t3/environment")?.as_str())
        .body(AsyncBody::empty())?;
    send_json(http, request).await
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
    let mut form = vec![
        ("grant_type", TOKEN_EXCHANGE_GRANT_TYPE),
        ("subject_token", pairing_credential),
        ("subject_token_type", ENVIRONMENT_BOOTSTRAP_TOKEN_TYPE),
        ("requested_token_type", ACCESS_TOKEN_TYPE),
        ("client_label", client.label.as_str()),
        ("client_device_type", "desktop"),
        ("client_os", client.operating_system.as_str()),
    ];
    if !scope.is_empty() {
        form.push(("scope", scope.as_str()));
    }
    let request = Request::builder()
        .method(Method::POST)
        .uri(endpoint(base_url, "/oauth/token")?.as_str())
        .header("Content-Type", "application/x-www-form-urlencoded")
        .body(AsyncBody::from(serde_urlencoded::to_string(&form)?))?;
    send_json(http, request).await
}

/// A short-lived ticket that authorizes one WebSocket connection.
pub async fn websocket_ticket(
    http: &dyn HttpClient,
    base_url: &Url,
    access_token: &AccessToken,
) -> Result<String> {
    #[derive(Deserialize)]
    struct TicketResponse {
        ticket: String,
    }
    let request = Request::builder()
        .method(Method::POST)
        .uri(endpoint(base_url, "/api/auth/websocket-ticket")?.as_str())
        .header(
            "Authorization",
            format!("Bearer {}", access_token.access_token),
        )
        .body(AsyncBody::empty())?;
    let response: TicketResponse = send_json(http, request).await?;
    Ok(response.ticket)
}

pub fn websocket_url(base_url: &Url, ticket: &str) -> Result<Url> {
    let mut url = endpoint(base_url, "/ws")?;
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

fn endpoint(base_url: &Url, path: &str) -> Result<Url> {
    base_url
        .join(path)
        .with_context(|| format!("invalid T3 server URL {base_url}"))
}

async fn send_json<T: DeserializeOwned>(
    http: &dyn HttpClient,
    request: Request<AsyncBody>,
) -> Result<T> {
    let uri = request.uri().clone();
    let mut response = http.send(request).await?;
    let mut body = String::new();
    response.body_mut().read_to_string(&mut body).await?;
    if !response.status().is_success() {
        bail!("{uri} returned {}: {body}", response.status());
    }
    serde_json::from_str(&body).with_context(|| format!("unexpected response from {uri}"))
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

//! Keeps mustr away from Zed's hosted services (mustr-docs/ZED-SERVICES.md).
//!
//! mustr never initializes the Zed features that talk to Zed's servers. This
//! module is the backstop: [`scrub_env`] drops the environment variables that
//! point Zed's code at a server, and [`ZedHostGuard`] refuses every HTTP
//! request to a Zed host, so a Zed code path that's reached by mistake fails
//! locally instead of contacting Zed.

use anyhow::{Result, anyhow, bail};
use futures::{FutureExt as _, future::BoxFuture};
use http_client::{
    AsyncBody, HttpClient, Inner, Method, RedirectPolicy, Request, Response, StatusCode, Url,
    http::{HeaderValue, Uri, header},
};
use parking_lot::Mutex;
use std::{collections::HashSet, io::Cursor, sync::Arc};

/// Domains mustr never contacts, with their subdomains: Zed's servers
/// (`zed.dev`, `api.zed.dev`, `cloud.zed.dev`, `collab.zed.dev`, ...), and
/// the unroutable `server_url` in mustr's default settings.
const BLOCKED_DOMAINS: &[&str] = &["zed.dev", "zed.invalid"];

/// Environment variables that point Zed's code at a Zed server, or turn on
/// uploads to one. Zed's crates read some of them once, into statics, and the
/// login-shell environment is captured by re-running mustr with `--printenv`,
/// so they're removed before anything else runs.
pub const SCRUBBED_ENV_VARS: &[&str] = &[
    // crates/client/src/client.rs: the server and RPC URLs, and sign-in overrides.
    "ZED_SERVER_URL",
    "ZED_RPC_URL",
    "ZED_IMPERSONATE",
    "ZED_ADMIN_API_TOKEN",
    "ZED_WEB_LOGIN",
    // crates/client/src/telemetry.rs: minidump upload and telemetry checksums.
    "ZED_MINIDUMP_ENDPOINT",
    "ZED_GENERATE_MINIDUMPS",
    "ZED_CLIENT_CHECKSUM_SEED",
];

/// Removes [`SCRUBBED_ENV_VARS`] from mustr's environment.
///
/// Call it first thing in `main`, before any thread starts.
pub fn scrub_env() {
    for name in SCRUBBED_ENV_VARS {
        if std::env::var_os(name).is_some() {
            // SAFETY: `main` calls this before it starts any thread, so nothing
            // reads the environment concurrently.
            unsafe { std::env::remove_var(name) };
            eprintln!("mustr ignores {name}: mustr doesn't contact Zed's servers");
        }
    }
}

/// Whether `host` is a Zed server (or mustr's unroutable stand-in for one).
pub fn is_blocked_host(host: &str) -> bool {
    let host = host.trim_end_matches('.').to_ascii_lowercase();
    BLOCKED_DOMAINS.iter().any(|domain| {
        host == *domain
            || host
                .strip_suffix(domain)
                .is_some_and(|subdomain| subdomain.ends_with('.'))
    })
}

/// reqwest's default redirect policy, which Zed's `ReqwestClient` keeps for
/// requests that don't set a [`RedirectPolicy`].
const DEFAULT_REDIRECT_LIMIT: u32 = 10;
/// What Zed's `ReqwestClient` uses for [`RedirectPolicy::FollowAll`].
const FOLLOW_ALL_REDIRECT_LIMIT: u32 = 100;

/// An [`HttpClient`] that refuses requests to Zed's servers.
///
/// It follows redirects itself, so a redirect from an allowed host can't lead
/// to a Zed host either. Each refused host is logged once.
pub struct ZedHostGuard {
    inner: Arc<dyn HttpClient>,
    refused_hosts: Arc<Mutex<HashSet<String>>>,
}

impl ZedHostGuard {
    pub fn new(inner: Arc<dyn HttpClient>) -> Self {
        Self {
            inner,
            refused_hosts: Arc::default(),
        }
    }
}

fn check_host(uri: &Uri, refused_hosts: &Mutex<HashSet<String>>) -> Result<()> {
    let Some(host) = uri.host() else {
        return Ok(());
    };
    if !is_blocked_host(host) {
        return Ok(());
    }
    let host = host.to_ascii_lowercase();
    if refused_hosts.lock().insert(host.clone()) {
        log::warn!("refused a request to {host}: mustr doesn't contact Zed's servers");
    }
    Err(anyhow!(
        "mustr refused a request to {host}: mustr doesn't contact Zed's servers"
    ))
}

impl HttpClient for ZedHostGuard {
    fn user_agent(&self) -> Option<&HeaderValue> {
        self.inner.user_agent()
    }

    fn proxy(&self) -> Option<&Url> {
        self.inner.proxy()
    }

    fn send(
        &self,
        mut request: Request<AsyncBody>,
    ) -> BoxFuture<'static, Result<Response<AsyncBody>>> {
        if let Err(error) = check_host(request.uri(), &self.refused_hosts) {
            return futures::future::ready(Err(error)).boxed();
        }
        let redirect_limit = match request.extensions().get::<RedirectPolicy>() {
            None => DEFAULT_REDIRECT_LIMIT,
            Some(RedirectPolicy::NoFollow) | Some(RedirectPolicy::FollowLimit(0)) => {
                return self.inner.send(request);
            }
            Some(RedirectPolicy::FollowLimit(limit)) => *limit,
            Some(RedirectPolicy::FollowAll) => FOLLOW_ALL_REDIRECT_LIMIT,
        };
        request.extensions_mut().insert(RedirectPolicy::NoFollow);

        let inner = self.inner.clone();
        let refused_hosts = self.refused_hosts.clone();
        async move {
            let mut request = request;
            let mut redirects = 0;
            loop {
                let replay = Replay::of(&request);
                let response = inner.send(request).await?;
                let Some(location) = redirect_location(&response, &replay.uri) else {
                    return Ok(response);
                };
                if redirects == redirect_limit {
                    bail!("too many redirects from {}", replay.uri);
                }
                redirects += 1;
                check_host(&location, &refused_hosts)?;
                match replay.follow(response.status(), location) {
                    Some(next) => request = next,
                    // A 307 or 308 with a streamed body, which can't be sent again.
                    None => return Ok(response),
                }
            }
        }
        .boxed()
    }
}

/// Where a redirect response points, resolved against the request's URI.
fn redirect_location(response: &Response<AsyncBody>, request_uri: &Uri) -> Option<Uri> {
    if !matches!(
        response.status(),
        StatusCode::MOVED_PERMANENTLY
            | StatusCode::FOUND
            | StatusCode::SEE_OTHER
            | StatusCode::TEMPORARY_REDIRECT
            | StatusCode::PERMANENT_REDIRECT
    ) {
        return None;
    }
    let location = response.headers().get(header::LOCATION)?.to_str().ok()?;
    let base = Url::parse(&request_uri.to_string()).ok()?;
    base.join(location).ok()?.as_str().parse().ok()
}

/// What's needed to send a request again to a redirect's target.
struct Replay {
    method: Method,
    uri: Uri,
    headers: http_client::http::HeaderMap,
    extensions: http_client::http::Extensions,
    /// `None` for a streamed body.
    body: Option<AsyncBody>,
}

impl Replay {
    fn of(request: &Request<AsyncBody>) -> Self {
        Self {
            method: request.method().clone(),
            uri: request.uri().clone(),
            headers: request.headers().clone(),
            extensions: request.extensions().clone(),
            body: match &request.body().0 {
                Inner::Empty => Some(AsyncBody::empty()),
                Inner::Bytes(bytes) => Some(AsyncBody(Inner::Bytes(Cursor::new(
                    bytes.get_ref().clone(),
                )))),
                Inner::AsyncReader(_) => None,
            },
        }
    }

    /// The request to send to `location`, as reqwest's redirect handling would build it.
    fn follow(self, status: StatusCode, location: Uri) -> Option<Request<AsyncBody>> {
        let Self {
            mut method,
            uri,
            mut headers,
            extensions,
            mut body,
        } = self;
        if matches!(
            status,
            StatusCode::MOVED_PERMANENTLY | StatusCode::FOUND | StatusCode::SEE_OTHER
        ) {
            if method != Method::GET && method != Method::HEAD {
                method = Method::GET;
            }
            body = Some(AsyncBody::empty());
            for name in [
                header::TRANSFER_ENCODING,
                header::CONTENT_ENCODING,
                header::CONTENT_TYPE,
                header::CONTENT_LENGTH,
            ] {
                headers.remove(name);
            }
        }
        let same_origin = uri.scheme() == location.scheme()
            && uri.host() == location.host()
            && uri.port_u16() == location.port_u16();
        if !same_origin {
            for name in [
                header::AUTHORIZATION,
                header::COOKIE,
                header::PROXY_AUTHORIZATION,
                header::WWW_AUTHENTICATE,
            ] {
                headers.remove(name);
            }
        }

        let mut request = Request::builder()
            .method(method)
            .uri(location)
            .body(body?)
            .ok()?;
        *request.headers_mut() = headers;
        *request.extensions_mut() = extensions;
        Some(request)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use futures::AsyncReadExt as _;
    use http_client::FakeHttpClient;

    /// A guard over a fake client that records every request that gets past the guard, and
    /// answers with `respond`.
    fn guard(
        respond: impl Fn(&Request<AsyncBody>) -> Response<AsyncBody> + Send + Sync + 'static,
    ) -> (ZedHostGuard, Arc<Mutex<Vec<String>>>) {
        let sent = Arc::new(Mutex::new(Vec::new()));
        let respond = Arc::new(respond);
        let fake = FakeHttpClient::create({
            let sent = sent.clone();
            move |request| {
                sent.lock().push(format!(
                    "{} {}{}",
                    request.method(),
                    request.uri(),
                    request
                        .headers()
                        .get(header::AUTHORIZATION)
                        .map_or(String::new(), |_| " (authorized)".to_string())
                ));
                let response = respond(&request);
                async move { Ok(response) }
            }
        });
        (ZedHostGuard::new(fake), sent)
    }

    fn ok() -> Response<AsyncBody> {
        Response::builder()
            .status(200)
            .body(AsyncBody::from("ok".to_string()))
            .unwrap()
    }

    fn redirect(status: u16, location: &str) -> Response<AsyncBody> {
        Response::builder()
            .status(status)
            .header(header::LOCATION, location)
            .body(AsyncBody::empty())
            .unwrap()
    }

    fn get(uri: &str) -> Request<AsyncBody> {
        Request::get(uri).body(AsyncBody::empty()).unwrap()
    }

    fn send_err(guard: &ZedHostGuard, request: Request<AsyncBody>) -> anyhow::Error {
        match futures::executor::block_on(guard.send(request)) {
            Ok(response) => panic!("expected an error, got {}", response.status()),
            Err(error) => error,
        }
    }

    #[test]
    fn blocks_zed_hosts_and_nothing_else() {
        for host in [
            "zed.dev",
            "api.zed.dev",
            "cloud.zed.dev",
            "collab.zed.dev",
            "llm.zed.dev",
            "ZED.dev",
            "zed.dev.",
            "zed.invalid",
            "api.zed.invalid",
        ] {
            assert!(is_blocked_host(host), "{host} should be blocked");
        }
        for host in [
            "github.com",
            "api.github.com",
            "objects.githubusercontent.com",
            "registry.npmjs.org",
            "nodejs.org",
            "notzed.dev",
            "zed.dev.example.com",
            "zed.devices.example",
            "127.0.0.1",
            "localhost",
        ] {
            assert!(!is_blocked_host(host), "{host} should be allowed");
        }
    }

    #[test]
    fn refuses_requests_to_zed_and_lets_others_through() {
        let (guard, sent) = guard(|_| ok());
        for uri in [
            "https://zed.dev/api/extensions",
            "https://api.zed.dev/telemetry/events",
            "https://cloud.zed.dev/releases/stable/latest/asset",
            "wss://collab.zed.dev/rpc",
            "https://zed.invalid/rpc",
        ] {
            let error = send_err(&guard, get(uri));
            assert!(
                error.to_string().contains("doesn't contact Zed's servers"),
                "{error}"
            );
        }
        assert!(sent.lock().is_empty(), "{:?}", sent.lock());

        let mut response =
            futures::executor::block_on(guard.send(get("https://api.github.com/repos/x/y")))
                .unwrap();
        let mut body = String::new();
        futures::executor::block_on(response.body_mut().read_to_string(&mut body)).unwrap();
        assert_eq!(body, "ok");
        assert_eq!(*sent.lock(), ["GET https://api.github.com/repos/x/y"]);

        // Each refused host is logged once, however often it's requested.
        send_err(&guard, get("https://zed.dev/again"));
        let mut refused: Vec<_> = guard.refused_hosts.lock().iter().cloned().collect();
        refused.sort();
        assert_eq!(
            refused,
            [
                "api.zed.dev",
                "cloud.zed.dev",
                "collab.zed.dev",
                "zed.dev",
                "zed.invalid"
            ]
        );
    }

    #[test]
    fn redirects_are_followed_but_never_to_zed() {
        let (guard, sent) = guard(|request| match request.uri().path() {
            "/download" => redirect(302, "https://objects.githubusercontent.com/asset"),
            "/sneaky" => redirect(301, "https://cloud.zed.dev/releases"),
            "/relative" => redirect(307, "/download"),
            _ => ok(),
        });

        let response =
            futures::executor::block_on(guard.send(get("https://github.com/download"))).unwrap();
        assert_eq!(response.status(), 200);
        assert_eq!(
            *sent.lock(),
            [
                "GET https://github.com/download",
                "GET https://objects.githubusercontent.com/asset"
            ]
        );

        sent.lock().clear();
        let error = send_err(&guard, get("https://github.com/sneaky"));
        assert!(error.to_string().contains("cloud.zed.dev"), "{error}");
        assert_eq!(*sent.lock(), ["GET https://github.com/sneaky"]);

        sent.lock().clear();
        futures::executor::block_on(guard.send(get("https://github.com/relative"))).unwrap();
        assert_eq!(
            *sent.lock(),
            [
                "GET https://github.com/relative",
                "GET https://github.com/download",
                "GET https://objects.githubusercontent.com/asset"
            ]
        );
    }

    #[test]
    fn redirects_follow_the_request_policy() {
        let (guard, sent) = guard(|request| match request.uri().path() {
            "/loop" => redirect(302, "/loop"),
            "/see-other" => redirect(303, "https://other.example/result"),
            _ => ok(),
        });

        // `NoFollow` hands the redirect back.
        let mut request = get("https://github.com/loop");
        request.extensions_mut().insert(RedirectPolicy::NoFollow);
        let response = futures::executor::block_on(guard.send(request)).unwrap();
        assert_eq!(response.status(), 302);
        assert_eq!(sent.lock().len(), 1);

        sent.lock().clear();
        let mut request = get("https://github.com/loop");
        request
            .extensions_mut()
            .insert(RedirectPolicy::FollowLimit(3));
        let error = send_err(&guard, request);
        assert!(error.to_string().contains("too many redirects"), "{error}");
        assert_eq!(sent.lock().len(), 4);

        // A 303 turns a POST into a GET, and credentials don't go to another host.
        sent.lock().clear();
        let request = Request::post("https://github.com/see-other")
            .header(header::AUTHORIZATION, "token secret")
            .body(AsyncBody::from("{}".to_string()))
            .unwrap();
        futures::executor::block_on(guard.send(request)).unwrap();
        assert_eq!(
            *sent.lock(),
            [
                "POST https://github.com/see-other (authorized)",
                "GET https://other.example/result"
            ]
        );
    }
}

//! T3's HTTP API, typed by the endpoint tables in `t3_api::http`.

use anyhow::{Context as _, Result, anyhow, bail};
use futures::AsyncReadExt as _;
use http_client::{AsyncBody, HttpClient, Method, Request, Url};
use serde::{Serialize, de::DeserializeOwned};
use serde_json::Value;
use t3_api::HttpEndpoint;

use crate::AccessToken;

/// Calls an endpoint without path parameters or a query, such as
/// `t3_api::http::PullRequestsDiff`. Pass `&()` as `body` when it takes none.
pub async fn call_endpoint<E>(
    http: &dyn HttpClient,
    base_url: &Url,
    access_token: Option<&AccessToken>,
    body: &E::Request,
) -> Result<E::Response>
where
    E: HttpEndpoint<Params = (), Query = ()>,
    E::Request: Serialize,
    E::Response: DeserializeOwned,
{
    send::<E, E::Response>(http, base_url, access_token, &(), &(), body).await
}

/// Calls any endpoint: `params` fills the `:name` segments of `E::PATH`, and `query` is
/// URL-encoded. Use `&()` for the parts `E` doesn't have.
pub async fn call_endpoint_with<E>(
    http: &dyn HttpClient,
    base_url: &Url,
    access_token: Option<&AccessToken>,
    params: &E::Params,
    query: &E::Query,
    body: &E::Request,
) -> Result<E::Response>
where
    E: HttpEndpoint,
    E::Params: Serialize,
    E::Query: Serialize,
    E::Request: Serialize,
    E::Response: DeserializeOwned,
{
    send::<E, E::Response>(http, base_url, access_token, params, query, body).await
}

/// Like [`call_endpoint_with`], but decodes the response as `R` instead of
/// `E::Response`, for responses mustr must read from servers of any version.
pub(crate) async fn send<E, R>(
    http: &dyn HttpClient,
    base_url: &Url,
    access_token: Option<&AccessToken>,
    params: &E::Params,
    query: &E::Query,
    body: &E::Request,
) -> Result<R>
where
    E: HttpEndpoint,
    E::Params: Serialize,
    E::Query: Serialize,
    E::Request: Serialize,
    R: DeserializeOwned,
{
    let url = endpoint_url::<E>(base_url, params, query)?;
    let method = Method::from_bytes(E::METHOD.as_bytes())
        .with_context(|| format!("T3 endpoint {} has an invalid method", E::PATH))?;
    let mut request = Request::builder().method(method).uri(url.as_str());
    // Exactly what the endpoint's contract requires, e.g. `x-t3-orchestration-protocol: 2`.
    for (name, value) in E::CONSTANT_HEADERS {
        request = request.header(*name, *value);
    }
    if let Some(access_token) = access_token {
        request = request.header(
            "Authorization",
            format!("Bearer {}", access_token.access_token),
        );
    }
    let request = match E::REQUEST_CONTENT_TYPE {
        None => request.body(AsyncBody::empty())?,
        Some(content_type @ "application/json") => request
            .header("Content-Type", content_type)
            .body(AsyncBody::from(serde_json::to_string(body)?))?,
        Some(content_type @ "application/x-www-form-urlencoded") => request
            .header("Content-Type", content_type)
            .body(AsyncBody::from(serde_urlencoded::to_string(body)?))?,
        Some(other) => bail!(
            "T3 endpoint {} sends {other}, which isn't supported",
            E::PATH
        ),
    };

    let mut response = http.send(request).await?;
    let mut text = String::new();
    response.body_mut().read_to_string(&mut text).await?;
    if !response.status().is_success() {
        bail!("{url} returned {}: {text}", response.status());
    }
    serde_json::from_str(&text).with_context(|| format!("unexpected response from {url}"))
}

fn endpoint_url<E>(base_url: &Url, params: &E::Params, query: &E::Query) -> Result<Url>
where
    E: HttpEndpoint,
    E::Params: Serialize,
    E::Query: Serialize,
{
    let params = serde_json::to_value(params)?;
    let mut url = base_url.clone();
    {
        let mut segments = url
            .path_segments_mut()
            .map_err(|()| anyhow!("invalid T3 server URL {base_url}"))?;
        segments.clear();
        for segment in E::PATH.trim_start_matches('/').split('/') {
            let Some(name) = segment.strip_prefix(':') else {
                segments.push(segment);
                continue;
            };
            match params.get(name) {
                Some(Value::String(value)) => segments.push(value),
                Some(Value::Number(value)) => segments.push(&value.to_string()),
                _ => bail!("{} needs the path parameter {name}", E::PATH),
            };
        }
    }
    let query = serde_urlencoded::to_string(query)?;
    url.set_query((!query.is_empty()).then_some(query.as_str()));
    Ok(url)
}

#[cfg(test)]
mod tests {
    use super::*;

    struct ThreadMessages;

    #[derive(Serialize)]
    struct ThreadParams {
        #[serde(rename = "threadId")]
        thread_id: String,
    }

    #[derive(Serialize)]
    struct Page {
        limit: u32,
    }

    impl HttpEndpoint for ThreadMessages {
        const METHOD: &'static str = "GET";
        const PATH: &'static str = "/api/orchestration/threads/:threadId/messages";
        const REQUEST_CONTENT_TYPE: Option<&'static str> = None;
        const SUCCESS_STATUS: u16 = 200;
        const ERROR_STATUSES: &'static [u16] = &[];
        const CONSTANT_HEADERS: &'static [(&'static str, &'static str)] = &[];
        type Params = ThreadParams;
        type Query = Page;
        type Headers = ();
        type Request = ();
        type Response = ();
    }

    #[test]
    fn urls_fill_path_parameters_and_the_query() -> Result<()> {
        let base_url = Url::parse("http://127.0.0.1:3773/ignored/")?;
        let params = ThreadParams {
            thread_id: "thread 1/2".to_string(),
        };
        let url = endpoint_url::<ThreadMessages>(&base_url, &params, &Page { limit: 5 })?;
        assert_eq!(
            url.as_str(),
            "http://127.0.0.1:3773/api/orchestration/threads/thread%201%2F2/messages?limit=5"
        );

        let url = endpoint_url::<t3_api::http::MetadataDescriptor>(&base_url, &(), &())?;
        assert_eq!(
            url.as_str(),
            "http://127.0.0.1:3773/.well-known/t3/environment"
        );
        Ok(())
    }
}

//! Attachments and assets over HTTP, the two calls T3 makes outside its RPC socket:
//!
//! - [`upload_attachment`] sends an attachment's bytes to the URL `attachments.createUploadUrl`
//!   minted (`POST /api/attachments/upload/:token`).
//! - [`fetch_asset`] reads an asset, such as an image in a message, from the URL
//!   `assets.createUrl` signed (`GET /api/assets/*`).
//!
//! Both URLs carry their own signature, so neither needs the session's token, and both must be
//! on T3's own server: [`signed_url`] refuses any other origin, so a URL from T3 can't send
//! mustr's bytes, or its requests, anywhere else.

use anyhow::{Context as _, Result, bail, ensure};
use futures::{AsyncRead, AsyncReadExt as _};
use http_client::{AsyncBody, HttpClient, Method, Request, Url};
use std::{
    pin::Pin,
    sync::Arc,
    task::{Context, Poll},
};

/// Called with the number of bytes sent so far, as they go.
pub type UploadProgress = Arc<dyn Fn(u64) + Send + Sync>;

/// The absolute URL of a relative one T3 signed, which must be on T3's server.
pub fn signed_url(server_url: &Url, relative_url: &str) -> Result<Url> {
    let url = server_url
        .join(relative_url)
        .with_context(|| format!("T3's URL {relative_url:?} isn't valid"))?;
    ensure!(
        relative_url.starts_with('/')
            && !relative_url.starts_with("//")
            && url.origin() == server_url.origin(),
        "T3's URL {relative_url:?} isn't on its server"
    );
    Ok(url)
}

/// The request that uploads `bytes` to `url`, as T3's web app sends it
/// (`apps/web/src/lib/attachmentUploadQueue.ts`). T3 refuses a length other than the size the
/// URL was minted for.
pub fn upload_request(
    url: &Url,
    mime_type: &str,
    bytes: Vec<u8>,
    on_progress: Option<UploadProgress>,
) -> Result<Request<AsyncBody>> {
    let size = bytes.len();
    let body = match on_progress {
        Some(on_progress) => AsyncBody::from_reader(ProgressReader {
            bytes: futures::io::Cursor::new(bytes),
            sent: 0,
            on_progress,
        }),
        None => AsyncBody::from(bytes),
    };
    Ok(Request::builder()
        .method(Method::POST)
        .uri(url.as_str())
        .header("Content-Type", mime_type)
        .header("Content-Length", size)
        .body(body)?)
}

/// Uploads an attachment's bytes to the `relative_url` that `attachments.createUploadUrl`
/// minted for them.
pub async fn upload_attachment(
    http: &dyn HttpClient,
    server_url: &Url,
    relative_url: &str,
    mime_type: &str,
    bytes: Vec<u8>,
    on_progress: Option<UploadProgress>,
) -> Result<()> {
    let url = signed_url(server_url, relative_url)?;
    let request = upload_request(&url, mime_type, bytes, on_progress)?;
    let mut response = http
        .send(request)
        .await
        .context("uploading the attachment")?;
    if !response.status().is_success() {
        let mut text = String::new();
        response.body_mut().read_to_string(&mut text).await.ok();
        bail!(
            "T3 refused the attachment ({}): {}",
            response.status(),
            text.trim()
        );
    }
    Ok(())
}

/// An asset's bytes, as T3 served them.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Asset {
    pub bytes: Vec<u8>,
    /// The `Content-Type` T3 sent, such as `image/png`.
    pub content_type: Option<String>,
}

/// Reads the asset at the `relative_url` that `assets.createUrl` signed. Refuses one larger than
/// `max_bytes`, so a broken or hostile server can't make mustr hold an unbounded body.
pub async fn fetch_asset(
    http: &dyn HttpClient,
    server_url: &Url,
    relative_url: &str,
    max_bytes: usize,
) -> Result<Asset> {
    let url = signed_url(server_url, relative_url)?;
    let request = Request::builder()
        .method(Method::GET)
        .uri(url.as_str())
        .body(AsyncBody::empty())?;
    let mut response = http.send(request).await.context("loading the asset")?;
    if !response.status().is_success() {
        let mut text = String::new();
        response
            .body_mut()
            .take(4096)
            .read_to_string(&mut text)
            .await
            .ok();
        bail!(
            "T3 refused the asset ({}): {}",
            response.status(),
            text.trim()
        );
    }
    let content_type = response
        .headers()
        .get("content-type")
        .and_then(|value| value.to_str().ok())
        .map(str::to_string);
    let mut bytes = Vec::new();
    response
        .body_mut()
        .take(max_bytes as u64 + 1)
        .read_to_end(&mut bytes)
        .await
        .context("loading the asset")?;
    ensure!(
        bytes.len() <= max_bytes,
        "the asset is larger than {max_bytes} bytes"
    );
    Ok(Asset {
        bytes,
        content_type,
    })
}

/// An upload's body, reporting how much of it has been read, which is about how much has been
/// sent.
struct ProgressReader {
    bytes: futures::io::Cursor<Vec<u8>>,
    sent: u64,
    on_progress: UploadProgress,
}

impl AsyncRead for ProgressReader {
    fn poll_read(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &mut [u8],
    ) -> Poll<std::io::Result<usize>> {
        let this = &mut *self;
        let poll = Pin::new(&mut this.bytes).poll_read(cx, buf);
        if let Poll::Ready(Ok(read)) = &poll
            && *read > 0
        {
            this.sent += *read as u64;
            (this.on_progress)(this.sent);
        }
        poll
    }
}

#[cfg(test)]
mod tests {
    //! Against a real HTTP server on a loopback port, through the client mustr ships
    //! (`reqwest_client`), so these see the requests exactly as T3 would.

    use super::*;
    use parking_lot::Mutex;
    use std::{
        io::{BufRead as _, BufReader, Read as _, Write as _},
        net::TcpListener,
        thread,
    };

    /// One request the fake server got.
    #[derive(Debug, PartialEq)]
    struct Received {
        request_line: String,
        content_type: Option<String>,
        content_length: Option<String>,
        authorization: Option<String>,
        body: Vec<u8>,
    }

    /// Plays T3's HTTP server for one request: records it, and answers with `status`,
    /// `content_type`, and `body`.
    fn serve_once(
        status: &'static str,
        content_type: &'static str,
        body: Vec<u8>,
    ) -> (Url, thread::JoinHandle<Received>) {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let url = Url::parse(&format!("http://{}", listener.local_addr().unwrap())).unwrap();
        let handle = thread::spawn(move || {
            let (stream, _) = listener.accept().unwrap();
            let mut reader = BufReader::new(stream.try_clone().unwrap());
            let mut request_line = String::new();
            reader.read_line(&mut request_line).unwrap();
            let mut headers = Vec::new();
            loop {
                let mut line = String::new();
                reader.read_line(&mut line).unwrap();
                let line = line.trim_end().to_string();
                if line.is_empty() {
                    break;
                }
                let (name, value) = line.split_once(':').unwrap();
                headers.push((name.trim().to_ascii_lowercase(), value.trim().to_string()));
            }
            let header = |name: &str| {
                headers
                    .iter()
                    .find(|(header, _)| header == name)
                    .map(|(_, value)| value.clone())
            };
            let length = header("content-length")
                .map(|length| length.parse::<usize>().unwrap())
                .unwrap_or(0);
            let mut request_body = vec![0; length];
            reader.read_exact(&mut request_body).unwrap();
            let mut stream = stream;
            write!(
                stream,
                "HTTP/1.1 {status}\r\nContent-Type: {content_type}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                body.len()
            )
            .unwrap();
            stream.write_all(&body).unwrap();
            stream.flush().unwrap();
            Received {
                request_line: request_line.trim_end().to_string(),
                content_type: header("content-type"),
                content_length: header("content-length"),
                authorization: header("authorization"),
                body: request_body,
            }
        });
        (url, handle)
    }

    #[test]
    fn uploads_post_the_bytes_to_the_minted_url() {
        let (server_url, server) = serve_once("204 No Content", "text/plain", Vec::new());
        let http = reqwest_client::ReqwestClient::new();
        let progress = Arc::new(Mutex::new(Vec::new()));
        let png = b"\x89PNG\r\n\x1a\n not really".to_vec();
        smol::block_on(upload_attachment(
            &http,
            &server_url,
            "/api/attachments/upload/token.signature",
            "image/png",
            png.clone(),
            Some(Arc::new({
                let progress = progress.clone();
                move |sent| progress.lock().push(sent)
            })),
        ))
        .unwrap();
        assert_eq!(
            server.join().unwrap(),
            Received {
                request_line: "POST /api/attachments/upload/token.signature HTTP/1.1".into(),
                content_type: Some("image/png".into()),
                content_length: Some(png.len().to_string()),
                authorization: None,
                body: png.clone(),
            }
        );
        assert_eq!(progress.lock().last(), Some(&(png.len() as u64)));
    }

    #[test]
    fn refused_uploads_say_why() {
        let (server_url, server) = serve_once(
            "400 Bad Request",
            "text/plain",
            b"Content-Length must match the upload size.".to_vec(),
        );
        let http = reqwest_client::ReqwestClient::new();
        let error = smol::block_on(upload_attachment(
            &http,
            &server_url,
            "/api/attachments/upload/token.signature",
            "image/png",
            b"png".to_vec(),
            None,
        ))
        .unwrap_err();
        server.join().unwrap();
        assert_eq!(
            error.to_string(),
            "T3 refused the attachment (400 Bad Request): Content-Length must match the upload size."
        );
    }

    #[test]
    fn assets_are_read_from_the_signed_url() {
        let image = b"GIF89a tiny".to_vec();
        let (server_url, server) = serve_once("200 OK", "image/gif", image.clone());
        let http = reqwest_client::ReqwestClient::new();
        let asset = smol::block_on(fetch_asset(
            &http,
            &server_url,
            "/api/assets/attachment/a1?expires=1&signature=abc",
            1024,
        ))
        .unwrap();
        assert_eq!(
            asset,
            Asset {
                bytes: image,
                content_type: Some("image/gif".into()),
            }
        );
        let received = server.join().unwrap();
        assert_eq!(
            received.request_line,
            "GET /api/assets/attachment/a1?expires=1&signature=abc HTTP/1.1"
        );
        // The URL's signature is its credential; the session's token stays on the socket.
        assert_eq!(received.authorization, None);
    }

    #[test]
    fn assets_larger_than_the_limit_are_refused() {
        let (server_url, server) = serve_once("200 OK", "image/png", vec![0; 2048]);
        let http = reqwest_client::ReqwestClient::new();
        let error =
            smol::block_on(fetch_asset(&http, &server_url, "/api/assets/big", 1024)).unwrap_err();
        server.join().unwrap();
        assert_eq!(error.to_string(), "the asset is larger than 1024 bytes");
    }

    #[test]
    fn signed_urls_stay_on_t3s_server() {
        let server_url = Url::parse("http://127.0.0.1:3773").unwrap();
        assert_eq!(
            signed_url(&server_url, "/api/assets/x?signature=1")
                .unwrap()
                .as_str(),
            "http://127.0.0.1:3773/api/assets/x?signature=1"
        );
        for relative_url in [
            "//attacker.example/api/attachments/upload/token",
            "https://attacker.example/api/attachments/upload/token",
            "api/assets/x",
        ] {
            let error = signed_url(&server_url, relative_url).unwrap_err();
            assert!(
                error.to_string().contains("isn't on its server"),
                "{relative_url}: {error:#}"
            );
        }
    }
}

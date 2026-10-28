//! Talks to a real T3 Code server. Ignored by default; to run it:
//!
//! ```sh
//! export T3_PAIRING_TOKEN=$(t3 auth pairing create --scope orchestration:read \
//!   --ttl 5m --label "mustr test (read-only)" --json | jq -r .credential)
//! cargo test -p t3_client --test live_server -- --ignored --nocapture
//! ```
//!
//! Revoke the session afterwards with `t3 auth session list` / `t3 auth session revoke`.

use std::time::Instant;

use anyhow::{Context as _, Result};
use futures::StreamExt as _;
use http_client::Url;
use serde_json::json;
use t3_client::t3_api::tagged::OrchestrationV2ShellStreamItem;
use t3_client::t3_api::{http, rpc, types};
use t3_client::{ClientMetadata, RpcConnection, Scope};

#[test]
#[ignore = "needs a running T3 server and T3_PAIRING_TOKEN"]
fn live_server_round_trip() -> Result<()> {
    smol::block_on(async {
        let base_url = Url::parse(
            &std::env::var("T3_BASE_URL").unwrap_or_else(|_| "http://127.0.0.1:3773".to_string()),
        )?;
        let pairing_credential =
            std::env::var("T3_PAIRING_TOKEN").context("set T3_PAIRING_TOKEN")?;
        let http = reqwest_client::ReqwestClient::new();

        let descriptor = t3_client::fetch_descriptor(&http, &base_url).await?;
        t3_client::check_protocol_compatibility(&descriptor)?;
        println!(
            "environment {} (protocol {})",
            descriptor.label,
            descriptor.protocol_version()
        );
        // The same endpoint through the generated (strict) type.
        let typed =
            t3_client::call_endpoint::<http::MetadataDescriptor>(&http, &base_url, None, &())
                .await?;
        println!(
            "typed descriptor: T3 {} on {:?}",
            typed.server_version, typed.platform.os
        );

        let client = ClientMetadata {
            label: "mustr test (read-only)".to_string(),
            ..ClientMetadata::default()
        };
        let token = t3_client::exchange_pairing_credential(
            &http,
            &base_url,
            &pairing_credential,
            &[Scope::OrchestrationRead],
            &client,
        )
        .await?;
        println!("token: {token:?}");

        // An orchestration endpoint: needs the contract's `x-t3-orchestration-protocol`.
        // The default query also compiles once T3 gives the endpoint query options.
        let started = Instant::now();
        let shell = t3_client::call_endpoint_with::<http::OrchestrationShellSnapshot>(
            &http,
            &base_url,
            Some(&token),
            &(),
            &Default::default(),
            &(),
        )
        .await?;
        println!(
            "GET /api/orchestration/shell: {} threads in {:?}",
            shell.threads.len(),
            started.elapsed()
        );

        let ticket = t3_client::websocket_ticket(&http, &base_url, &token).await?;
        let (connection, driver) =
            RpcConnection::connect(&t3_client::websocket_url(&base_url, &ticket)?).await?;
        let driver = smol::spawn(driver);

        let started = Instant::now();
        let config = connection.request("server.getConfig", json!({})).await?;
        println!(
            "server.getConfig (untyped): {} keys in {:?}",
            config.as_object().map_or(0, |object| object.len()),
            started.elapsed()
        );

        let started = Instant::now();
        let config = connection
            .call::<rpc::ServerGetConfig>(types::ServerGetConfigPayload(Default::default()))
            .await?;
        println!(
            "server.getConfig: {} providers, {} keybindings in {:?}",
            config.providers.len(),
            config.keybindings.len(),
            started.elapsed()
        );

        let started = Instant::now();
        let pull_requests = connection
            .call::<rpc::PullRequestsList>(serde_json::from_value(
                json!({ "state": "open", "limit": 10 }),
            )?)
            .await?;
        println!(
            "pullRequests.list: {} entries in {:?}",
            pull_requests.entries.len(),
            started.elapsed()
        );

        if let Some(entry) = pull_requests.entries.first() {
            let pull_request = t3_client::PullRequestRef {
                project_id: entry.project_id.clone(),
                host: Some(entry.host.clone()),
                repository: entry.repository.clone(),
                number: entry.number,
                allow_stale: None,
                expected_account_id: None,
            };
            let started = Instant::now();
            let files = t3_client::load_pull_request_files(
                &http,
                &base_url,
                &token,
                &connection,
                &pull_request,
            )
            .await?;
            let with_text = files
                .iter()
                .filter(|file| file.old_text.is_some() || file.new_text.is_some())
                .count();
            println!(
                "{}#{}: {} files, {with_text} with text, in {:?}",
                pull_request.repository,
                pull_request.number,
                files.len(),
                started.elapsed()
            );
            for file in files.iter().filter(|file| file.unavailable.is_some()) {
                println!(
                    "  unavailable: {} ({})",
                    file.file.new_path,
                    file.unavailable.as_deref().unwrap_or_default()
                );
            }
            assert!(with_text > 0, "expected at least one text file");
        }

        let started = Instant::now();
        let mut shell = connection.stream::<rpc::OrchestrationSubscribeShell>(Default::default())?;
        let first = shell
            .next()
            .await
            .transpose()?
            .context("shell stream ended without a snapshot")?;
        match &first {
            OrchestrationV2ShellStreamItem::Snapshot(snapshot) => println!(
                "orchestration.subscribeShell: snapshot of {} threads, {} projects in {:?}",
                snapshot.snapshot.threads.len(),
                snapshot.snapshot.projects.len(),
                started.elapsed()
            ),
            other => println!(
                "orchestration.subscribeShell: first item kind={:?} in {:?}",
                other.tag(),
                started.elapsed()
            ),
        }

        connection.close();
        if let Err(error) = driver.await {
            println!("connection ended: {error:#}");
        }
        Ok(())
    })
}

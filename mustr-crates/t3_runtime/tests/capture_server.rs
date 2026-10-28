//! Records what a real T3 Code engine sends mustr, for `t3_api`'s compatibility tests
//! (`t3_api/tests/fixtures/newer-server/live/`). Ignored by default: it downloads an engine
//! (about 120 MB) from GitHub, checks it against the release's `SHA256SUMS`, and starts it in a
//! temporary home, the way mustr runs its own engine.
//!
//! ```sh
//! MUSTR_T3_CAPTURE_VERSION=0.1.4-nightly.20261028.3841 \
//! MUSTR_T3_CAPTURE_OUT=mustr-crates/t3_api/tests/fixtures/newer-server/live \
//!   cargo test -p t3_runtime --test capture_server -- --ignored --nocapture
//! ```
//!
//! It adds a project (a new git repository in the temporary directory) and a thread without a
//! message, so no agent ever runs, then reads what mustr reads. This machine's name and user name
//! are replaced in what it writes. `MUSTR_T3_CAPTURE_HOME` keeps the engine's home there instead,
//! with the project's repository next to it (to look at them with mustr afterwards), and
//! `MUSTR_T3_TEST_VERSIONS_DIR` keeps the download. The user's own T3 Code (`~/.t3`), its service
//! and its port are never touched.

use anyhow::{Context as _, Result, anyhow};
use futures::StreamExt as _;
use serde_json::{Value, json};
use std::{path::PathBuf, sync::Arc, time::Duration};
use t3_client::{ClientMetadata, RpcConnection, Scope, t3_api::http};
use t3_runtime::{
    Sleep,
    engine::{self, BootstrapSecret, EngineLaunch, EngineProcess},
    install::Installer,
    release::Target,
};

fn new_id(prefix: &str) -> String {
    format!("{prefix}-{}", engine::unix_millis())
}

/// Replaces this machine's names in every string of `value`: its host name (and the
/// tailnet name around it) and the user's name.
fn redact(value: &mut Value, host: &str, user: &str) {
    match value {
        Value::String(text) => {
            if text.ends_with(".ts.net") {
                *text = "workstation.example.ts.net".to_string();
            } else {
                if !host.is_empty() {
                    *text = text.replace(host, "workstation");
                }
                if !user.is_empty() && text == user {
                    *text = "dev".to_string();
                }
            }
        }
        Value::Array(values) => values.iter_mut().for_each(|v| redact(v, host, user)),
        Value::Object(object) => object.values_mut().for_each(|v| redact(v, host, user)),
        _ => {}
    }
}

/// The first item of the stream `tag`, or `None` if none comes within a few seconds.
async fn first_item(connection: &RpcConnection, tag: &str, payload: Value) -> Result<Vec<Value>> {
    let mut items = connection.subscribe(tag, payload)?;
    let first = smol::future::or(async { items.next().await }, async {
        smol::Timer::after(Duration::from_secs(10)).await;
        None
    })
    .await;
    match first {
        Some(item) => Ok(vec![item?]),
        None => Ok(Vec::new()),
    }
}

/// The engine's label: this machine's host name.
fn descriptor_label(captured: &[(&str, Vec<Value>)]) -> String {
    captured
        .iter()
        .find(|(def, _)| *def == "ExecutionEnvironmentDescriptor")
        .and_then(|(_, values)| values.first()?["label"].as_str())
        .unwrap_or_default()
        .to_string()
}

#[test]
#[ignore = "downloads T3 Code's engine from GitHub"]
fn captures_what_a_real_engine_sends() -> Result<()> {
    let out = PathBuf::from(
        std::env::var_os("MUSTR_T3_CAPTURE_OUT").context("set MUSTR_T3_CAPTURE_OUT")?,
    );
    let version = std::env::var("MUSTR_T3_CAPTURE_VERSION")
        .unwrap_or_else(|_| t3_client::pinned_server_version().to_string());
    smol::block_on(async {
        let temp = tempfile::tempdir()?;
        let versions = std::env::var_os("MUSTR_T3_TEST_VERSIONS_DIR")
            .map(PathBuf::from)
            .unwrap_or_else(|| temp.path().join("t3-runtime"));
        let home = std::env::var_os("MUSTR_T3_CAPTURE_HOME")
            .map(PathBuf::from)
            .unwrap_or_else(|| temp.path().join("t3-home"));
        let http = Arc::new(reqwest_client::ReqwestClient::new());
        let sleep: Sleep =
            Arc::new(|duration| Box::pin(smol::unblock(move || std::thread::sleep(duration))));
        let installer = Installer::new(http.clone(), Target::current()?, sleep.clone());
        let runtime = installer
            .ensure(&versions, &version, &|step| println!("{}", step.describe()))
            .await?;

        // A project folder for the engine to see: a fresh repository with one commit.
        let repo = home
            .parent()
            .filter(|_| std::env::var_os("MUSTR_T3_CAPTURE_HOME").is_some())
            .unwrap_or(temp.path())
            .join("capture-project");
        std::fs::create_dir_all(&repo)?;
        std::fs::write(
            repo.join("README.md"),
            "A project for capturing T3's payloads.\n",
        )?;
        for args in [
            &["init", "-q", "-b", "main"][..],
            &["add", "README.md"],
            &[
                "-c",
                "user.name=mustr",
                "-c",
                "user.email=mustr@example.invalid",
                "commit",
                "-q",
                "-m",
                "Start",
            ],
        ] {
            let status = std::process::Command::new("git")
                .args(args)
                .current_dir(&repo)
                .status()?;
            anyhow::ensure!(status.success(), "git {args:?} failed");
        }

        let launch = EngineLaunch {
            executable: runtime.executable.clone(),
            base_dir: home.clone(),
            port: engine::pick_port()?,
            log_file: temp.path().join("logs").join("t3-engine.log"),
        };
        let secret = BootstrapSecret::generate();
        let mut process =
            EngineProcess::spawn(&launch, launch.envelope(&secret, engine::unix_millis())?).await?;
        println!(
            "T3 {version}: engine pid {} at {}",
            process.pid(),
            launch.url()
        );

        let result =
            async {
                let url = launch.url();
                engine::wait_until_ready(
                    http.as_ref(),
                    &url,
                    || process.try_exit(),
                    Duration::from_secs(120),
                    &sleep,
                )
                .await?;
                let token = t3_client::exchange_pairing_credential(
                    http.as_ref(),
                    &url,
                    &secret.current_token(),
                    &[Scope::OrchestrationRead, Scope::OrchestrationOperate],
                    &ClientMetadata::default(),
                )
                .await?;
                let ticket = t3_client::websocket_ticket(http.as_ref(), &url, &token).await?;
                let (connection, driver) =
                    RpcConnection::connect(&t3_client::websocket_url(&url, &ticket)?).await?;
                let _driver = smol::spawn(driver);
                let mut captured: Vec<(&str, Vec<Value>)> = Vec::new();

                let descriptor: Value = t3_client::call_endpoint_as::<
                    http::MetadataDescriptor,
                    Value,
                >(http.as_ref(), &url, None, &(), &(), &())
                .await?;
                captured.push(("ExecutionEnvironmentDescriptor", vec![descriptor]));
                let config = connection.request("server.getConfig", json!({})).await?;
                captured.push(("ServerConfig", vec![config.clone()]));
                // The 0.1.4 nightlies dropped `server.getSettings`; the config carries the settings.
                match connection.request("server.getSettings", json!({})).await {
                    Ok(settings) => captured.push(("ServerSettings", vec![settings])),
                    Err(error) => println!("server.getSettings: {error}"),
                }

                // A project and a thread without a message: nothing runs.
                let project_id = new_id("project");
                let created = connection
                    .request(
                        "projects.mutate",
                        json!({
                            "type": "project.create", "commandId": new_id("command"),
                            "projectId": project_id, "title": "Capture",
                            "workspaceRoot": repo.to_string_lossy(),
                            "createWorkspaceRootIfMissing": false
                        }),
                    )
                    .await?;
                println!("project: {created}");
                let provider = config["providers"]
                    .as_array()
                    .and_then(|providers| providers.first())
                    .cloned()
                    .unwrap_or(Value::Null);
                let instance_id = provider["instanceId"]
                    .as_str()
                    .unwrap_or("codex")
                    .to_string();
                let model = provider["models"][0]["slug"]
                    .as_str()
                    .or_else(|| provider["models"][0]["id"].as_str())
                    .unwrap_or("gpt-5")
                    .to_string();
                let thread_id = new_id("thread");
                let dispatched = connection
                    .request(
                        "orchestration.dispatchCommand",
                        json!({
                            "type": "thread.create", "commandId": new_id("command"),
                            "threadId": thread_id, "projectId": project_id,
                            "title": "Capture what T3 sends",
                            "modelSelection": { "instanceId": instance_id, "model": model },
                            "runtimeMode": "approval-required", "interactionMode": "default",
                            "branch": null, "worktreePath": null,
                            "createdBy": "user", "creationSource": "web"
                        }),
                    )
                    .await?;
                captured.push(("OrchestrationV2DispatchCommandResult", vec![dispatched]));

                let shell: Value =
                    t3_client::call_endpoint_as::<http::OrchestrationShellSnapshot, Value>(
                        http.as_ref(),
                        &url,
                        Some(&token),
                        &(),
                        &Default::default(),
                        &(),
                    )
                    .await?;
                captured.push(("OrchestrationV2ShellSnapshot", vec![shell]));
                let thread: Value =
                    t3_client::call_endpoint_as::<http::OrchestrationThreadSnapshot, Value>(
                        http.as_ref(),
                        &url,
                        Some(&token),
                        &serde_json::from_value(json!({ "threadId": thread_id }))?,
                        &(),
                        &(),
                    )
                    .await?;
                captured.push(("OrchestrationV2ThreadDetailSnapshot", vec![thread]));
                captured.push((
                    "OrchestrationV2ShellStreamItem",
                    first_item(&connection, "orchestration.subscribeShell", json!({})).await?,
                ));
                captured.push((
                    "OrchestrationV2ThreadStreamItem",
                    first_item(
                        &connection,
                        "orchestration.subscribeThread",
                        json!({ "threadId": thread_id }),
                    )
                    .await?,
                ));
                captured.push((
                    "ScheduledTaskListResult",
                    first_item(&connection, "scheduledTasks.subscribe", json!({})).await?,
                ));
                let archived = connection
                    .request("orchestration.getArchivedShellSnapshot", json!({}))
                    .await?;
                captured.push(("OrchestrationV2ArchivedShellSnapshot", vec![archived]));
                for (tag, def, payload) in [
                    (
                        "orchestration.getThreadFamily",
                        "OrchestrationV2ThreadFamily",
                        json!({ "threadId": thread_id }),
                    ),
                    (
                        "vcs.listRefs",
                        "VcsListRefsResult",
                        json!({ "cwd": repo.to_string_lossy() }),
                    ),
                    (
                        "vcs.refreshStatus",
                        "VcsStatusResult",
                        json!({ "cwd": repo.to_string_lossy() }),
                    ),
                ] {
                    match connection.request(tag, payload).await {
                        Ok(value) => captured.push((def, vec![value])),
                        Err(error) => println!("{tag}: {error}"),
                    }
                }
                connection.close();

                let host = descriptor_label(&captured);
                let user = std::env::var("USER").unwrap_or_default();
                std::fs::create_dir_all(&out)?;
                for (def, mut values) in captured {
                    values
                        .iter_mut()
                        .for_each(|value| redact(value, &host, &user));
                    if values.is_empty() {
                        println!("{def}: nothing came");
                        continue;
                    }
                    let path = out.join(format!("{def}.{version}.json"));
                    std::fs::write(&path, serde_json::to_string_pretty(&values)? + "\n")?;
                    println!("wrote {}", path.display());
                }
                anyhow::Ok(())
            }
            .await;

        let status = process.stop(engine::STOP_GRACE, &sleep).await;
        println!("engine stopped: {status:?}");
        result.map_err(|error| anyhow!("{error:#}"))
    })
}

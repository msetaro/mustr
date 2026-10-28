//! Downloads T3 Code's pinned engine, starts it the way mustr does, and connects with the desktop
//! bootstrap token: no pairing and no `t3` CLI. Ignored by default because it downloads about
//! 120 MB from GitHub:
//!
//! ```sh
//! cargo test -p t3_runtime --test real_engine -- --ignored --nocapture
//! ```
//!
//! Everything lives in a temporary directory; the user's own T3 Code (`~/.t3`), its service, and
//! its port are never touched. Set `MUSTR_T3_TEST_VERSIONS_DIR` to keep the download between
//! runs.

use anyhow::{Context as _, Result};
use serde_json::json;
use std::{path::PathBuf, sync::Arc, time::Duration};
use t3_client::{ClientMetadata, RpcConnection, Scope};
use t3_runtime::{
    Sleep,
    engine::{self, BootstrapSecret, EngineLaunch, EngineProcess},
    install::Installer,
    release::Target,
};

#[test]
#[ignore = "downloads T3 Code's engine from GitHub"]
fn downloads_starts_and_connects_to_the_real_engine() -> Result<()> {
    smol::block_on(async {
        let temp = tempfile::tempdir()?;
        let versions = std::env::var_os("MUSTR_T3_TEST_VERSIONS_DIR")
            .map(PathBuf::from)
            .unwrap_or_else(|| temp.path().join("t3-runtime"));
        let http = Arc::new(reqwest_client::ReqwestClient::new());
        let sleep: Sleep =
            Arc::new(|duration| Box::pin(smol::unblock(move || std::thread::sleep(duration))));
        let version = t3_client::pinned_server_version();

        let installer = Installer::new(http.clone(), Target::current()?, sleep.clone());
        let runtime = installer
            .ensure(&versions, version, &|step| println!("{}", step.describe()))
            .await?;
        println!("runtime: {}", runtime.dir.display());
        // A second ensure reuses the verified install.
        assert_eq!(
            installer.ensure(&versions, version, &|_| {}).await?,
            runtime
        );

        let launch = EngineLaunch {
            executable: runtime.executable.clone(),
            base_dir: temp.path().join("t3-home"),
            port: engine::pick_port()?,
            log_file: temp.path().join("logs").join("t3-engine.log"),
        };
        let secret = BootstrapSecret::generate();
        let mut process =
            EngineProcess::spawn(&launch, launch.envelope(&secret, engine::unix_millis())?).await?;
        println!("engine pid {} at {}", process.pid(), launch.url());

        let result = async {
            let url = launch.url();
            let descriptor = engine::wait_until_ready(
                http.as_ref(),
                &url,
                || process.try_exit(),
                Duration::from_secs(120),
                &sleep,
            )
            .await?;
            t3_client::check_protocol_compatibility(&descriptor)?;
            assert_eq!(descriptor.server_version.as_deref(), Some(version));
            println!("ready: {} (T3 {version})", descriptor.label);

            let token = t3_client::exchange_pairing_credential(
                http.as_ref(),
                &url,
                &secret.current_token(),
                &[Scope::OrchestrationRead, Scope::OrchestrationOperate],
                &ClientMetadata::default(),
            )
            .await
            .context("exchanging the desktop bootstrap token")?;
            println!("token: {token:?}");
            // A wrong token is refused.
            assert!(
                t3_client::exchange_pairing_credential(
                    http.as_ref(),
                    &url,
                    &BootstrapSecret::generate().current_token(),
                    &[Scope::OrchestrationRead],
                    &ClientMetadata::default(),
                )
                .await
                .is_err()
            );

            let ticket = t3_client::websocket_ticket(http.as_ref(), &url, &token).await?;
            let (connection, driver) =
                RpcConnection::connect(&t3_client::websocket_url(&url, &ticket)?).await?;
            let driver = smol::spawn(driver);
            let config = connection.request("server.getConfig", json!({})).await?;
            let providers = config["providers"]
                .as_array()
                .map(|providers| {
                    providers
                        .iter()
                        .map(|provider| {
                            format!(
                                "{} ({}, {}, installed {}, auth {}: {})",
                                provider["driver"].as_str().unwrap_or("?"),
                                provider["status"].as_str().unwrap_or("?"),
                                if provider["enabled"].as_bool() == Some(true) {
                                    "enabled"
                                } else {
                                    "disabled"
                                },
                                provider["installed"],
                                provider["auth"]["status"].as_str().unwrap_or("?"),
                                provider["message"].as_str().unwrap_or("")
                            )
                        })
                        .collect::<Vec<_>>()
                })
                .unwrap_or_default();
            println!("providers:\n  {}", providers.join("\n  "));
            connection.close();
            drop(driver);

            // The engine keeps its state in its own base dir.
            let record = t3_runtime::installed::read_server_record(&launch.base_dir)
                .context("the engine wrote no server record")?;
            assert_eq!(record.pid, process.pid());
            assert_eq!(record.origin.port(), Some(launch.port));
            anyhow::Ok(())
        }
        .await;

        let pid = process.pid();
        let status = process.stop(engine::STOP_GRACE, &sleep).await;
        println!("engine stopped: {status:?}");
        if let Ok(log) = std::fs::read_to_string(&launch.log_file) {
            println!("engine log: {} lines", log.lines().count());
        }
        result?;
        assert!(status.is_some());
        assert!(!t3_runtime::installed::process_is_alive(pid));
        Ok(())
    })
}

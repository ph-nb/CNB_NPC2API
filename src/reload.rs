use crate::{api::ReloadHandle, config::Config};
use std::{path::PathBuf, time::Duration};

pub async fn watch(
    handle: ReloadHandle,
    path: PathBuf,
    mut applied: String,
    host: Option<String>,
    port: Option<u16>,
) {
    let mut interval = tokio::time::interval(Duration::from_secs(1));
    let mut warned = false;
    interval.tick().await;
    loop {
        interval.tick().await;
        let result = async {
            let raw = std::fs::read_to_string(&path)?;
            if raw == applied {
                return Ok::<Option<String>, anyhow::Error>(None);
            }
            let mut cfg = Config::parse(&raw)?;
            if let Some(host) = &host {
                cfg.server.host = host.clone();
            }
            if let Some(port) = port {
                cfg.server.port = port;
            }
            handle.replace(cfg).await?;
            Ok(Some(raw))
        }
        .await;
        match result {
            Ok(Some(raw)) => {
                applied = raw;
                warned = false;
                tracing::info!("configuration reloaded");
            }
            Ok(None) => warned = false,
            Err(_) if !warned => {
                warned = true;
                tracing::warn!("configuration reload failed; keeping the previous configuration");
            }
            Err(_) => (),
        }
    }
}

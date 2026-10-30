mod api;
mod config;
mod fc;
#[cfg(test)]
mod integration_tests;
mod model;
mod multimodal;
mod pool;
mod reload;
mod response_store;
mod streaming;
mod upstream;
mod wire;

use anyhow::{bail, Context, Result};
use config::Config;
use std::{env, path::PathBuf};

const VERSION: &str = match option_env!("CNB_NPC2API_RELEASE_VERSION") {
    Some(version) => version,
    None => env!("CARGO_PKG_VERSION"),
};

#[tokio::main]
async fn main() -> Result<()> {
    tracing_subscriber::fmt()
        .with_writer(std::io::stderr)
        .with_env_filter(tracing_subscriber::EnvFilter::from_default_env())
        .init();
    let mut args = env::args().skip(1);
    let mut path = env::var_os("TOOLFORGE_CONFIG").map(PathBuf::from);
    let mut host = None;
    let mut port = None;
    let mut reload = false;
    while let Some(arg) = args.next() {
        match arg.as_str() {
            "serve" => (),
            "-c" | "-config" | "--config" => {
                path = Some(PathBuf::from(
                    args.next()
                        .ok_or_else(|| anyhow::anyhow!("missing config path"))?,
                ))
            }
            "--host" => host = Some(args.next().ok_or_else(|| anyhow::anyhow!("missing host"))?),
            "--port" => {
                port = Some(
                    args.next()
                        .ok_or_else(|| anyhow::anyhow!("missing port"))?
                        .parse::<u16>()?,
                )
            }
            "--reload" => reload = true,
            "-V" | "--version" => {
                println!("CNB_NPC2API {VERSION}");
                return Ok(());
            }
            "-h" | "--help" => {
                println!("Usage: cnb-npc2api [serve] [-c config.yaml] [--host HOST] [--port PORT] [--reload]");
                return Ok(());
            }
            _ => bail!("unknown argument: {arg}"),
        }
    }
    let path = path.unwrap_or_else(|| {
        if PathBuf::from("config.yaml").is_file() {
            "config.yaml".into()
        } else {
            "config.example.yaml".into()
        }
    });
    let raw = std::fs::read_to_string(&path)
        .with_context(|| format!("read config {}", path.display()))?;
    let mut cfg = Config::parse(&raw)?;
    if let Some(host) = &host {
        cfg.server.host = host.clone();
    }
    if let Some(port) = port {
        cfg.server.port = port;
    }
    let listener =
        tokio::net::TcpListener::bind((cfg.server.host.as_str(), cfg.server.port)).await?;
    let app = api::App::new(cfg).await?;
    let router = if reload {
        let (router, handle) = api::reloadable_router(app);
        tokio::spawn(reload::watch(handle, path, raw, host, port));
        router
    } else {
        api::router(app)
    };
    tracing::info!(address = %listener.local_addr()?, "CNB_NPC2API listening");
    axum::serve(listener, router)
        .with_graceful_shutdown(async {
            let _ = tokio::signal::ctrl_c().await;
        })
        .await?;
    Ok(())
}

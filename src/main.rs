use std::sync::Arc;

use tracing::info;
use tracing_subscriber::{EnvFilter, fmt};

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    fmt()
        .with_env_filter(EnvFilter::from_default_env())
        .with_target(true)
        .with_thread_ids(true)
        .compact()
        .init();

    info!("NitroBackrun starting up…");

    let config = Arc::new(nitro_backrun::config::BotConfig::from_env()?);
    nitro_backrun::run(config, None).await
}

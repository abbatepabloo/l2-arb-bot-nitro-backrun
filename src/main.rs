mod config;
mod engine;
mod error;
mod execution;
mod feed;
mod pool;
mod types;

use std::sync::Arc;

use tokio::sync::mpsc;
use tracing::info;
use tracing_subscriber::{EnvFilter, fmt};

use crate::config::BotConfig;
use crate::engine::ArbitrageEngine;
use crate::execution::ExecutionManager;
use crate::pool::manager::LiquidityPoolManager;
use crate::types::ArbitrageSignal;

// ── Entry Point ───────────────────────────────────────────────────────────────

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    // ── Logging ───────────────────────────────────────────────────────────────
    // Set RUST_LOG=nitro_backrun=debug,info for verbose output.
    fmt()
        .with_env_filter(EnvFilter::from_default_env())
        .with_target(true)
        .with_thread_ids(true)
        .compact()
        .init();

    info!("NitroBackrun starting up…");

    // ── Configuration ─────────────────────────────────────────────────────────
    let config = Arc::new(BotConfig::from_env()?);
    info!(
        feed_url    = %config.sequencer_feed_url,
        rpc_url     = %config.rpc_url,
        pool_count_target = "loaded from SQLite below",
        "Configuration loaded"
    );

    // ── Database → LiquidityPoolManager ───────────────────────────────────────
    let lpm = LiquidityPoolManager::load_from_db(&config.db_path, &config.rpc_url).await?;
    info!(pool_count = lpm.pool_count(), "LiquidityPoolManager initialised");

    // ── Execution channel ─────────────────────────────────────────────────────
    // Bounded at 64: if the executor is saturated, engine signals are dropped.
    // In practice, opportunities should be processed in < 1 ms each, so a
    // queue of 64 provides ~64 ms of headroom before backpressure hits.
    let (exec_tx, exec_rx) = mpsc::channel::<ArbitrageSignal>(64);

    // ── ExecutionManager ──────────────────────────────────────────────────────
    let exec_manager = ExecutionManager::new(Arc::clone(&config)).await?;
    tokio::spawn({
        let exec_manager = Arc::clone(&exec_manager);
        async move { exec_manager.run(exec_rx).await }
    });

    // ── ArbitrageEngine (runs the full bootstrap + live loop) ─────────────────
    //
    // This call:
    //   1. Opens the sequencer feed WebSocket and starts buffering.
    //   2. Fetches all pool reserves from the RPC node.
    //   3. Replays buffered transactions to reconcile state.
    //   4. Spawns the live evaluation loop.
    //   5. Returns (the spawned tasks continue running).
    ArbitrageEngine::bootstrap(
        Arc::clone(&config),
        Arc::clone(&lpm),
        exec_tx,
    )
    .await?;

    info!("All subsystems running — bot is live");

    // ── Shutdown handler ──────────────────────────────────────────────────────
    // Wait for Ctrl+C and exit cleanly.
    tokio::signal::ctrl_c().await?;
    info!("Received Ctrl+C — shutting down");

    Ok(())
}

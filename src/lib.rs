// ── Public module tree ─────────────────────────────────────────────────────────
pub mod config;
pub mod engine;
pub mod error;
pub mod execution;
pub mod feed;
pub mod pool;
pub mod types;

use std::sync::Arc;

use tokio::sync::mpsc;
use tracing::info;

use crate::config::BotConfig;
use crate::engine::ArbitrageEngine;
use crate::execution::ExecutionManager;
use crate::pool::manager::LiquidityPoolManager;
use crate::types::ArbitrageSignal;

// ── Core run function (dependency-injected, testable) ─────────────────────────
//
// Wires together every subsystem and drives the main event loop.
//
// `max_transactions`:
//   None       — production default; runs until Ctrl+C.
//   Some(n)    — integration-test mode; exits cleanly after the engine has
//                evaluated exactly `n` transactions from the sequencer feed.
//
// Example (integration test):
//
//   let config = Arc::new(BotConfig::new(
//       "http://127.0.0.1:8545",           // local Anvil node
//       "ws://127.0.0.1:8546/feed",        // mock feed
//       "http://127.0.0.1:9545",           // conditional RPC proxy
//       "0xac0974bec39a17e36ba4a6b4d238ff944bacb478cbed5efcae784d7bf4f2ff80",
//       Address::ZERO,                     // mock executor contract
//       "/tmp/test-pools.db",
//       0,                                 // min_profit_wei
//       vec![mock_router],                 // monitored_routers
//       31337,                             // chain_id (Anvil default)
//   ));
//   run(config, Some(1)).await?;
//
pub async fn run(config: Arc<BotConfig>, max_transactions: Option<usize>) -> anyhow::Result<()> {
    info!(
        feed_url  = %config.sequencer_feed_url,
        rpc_url   = %config.rpc_url,
        chain_id  = config.chain_id,
        "Configuration loaded"
    );

    // ── Database → LiquidityPoolManager ──────────────────────────────────────
    let lpm = LiquidityPoolManager::load_from_db(&config.db_path, &config.rpc_url).await?;
    info!(pool_count = lpm.pool_count(), "LiquidityPoolManager initialised");

    // ── Execution channel ─────────────────────────────────────────────────────
    let (exec_tx, exec_rx) = mpsc::channel::<ArbitrageSignal>(64);

    // ── ExecutionManager ──────────────────────────────────────────────────────
    let exec_manager = ExecutionManager::new(Arc::clone(&config)).await?;
    tokio::spawn({
        let exec_manager = Arc::clone(&exec_manager);
        async move { exec_manager.run(exec_rx).await }
    });

    // ── ArbitrageEngine ───────────────────────────────────────────────────────
    let engine_handle = ArbitrageEngine::bootstrap(
        Arc::clone(&config),
        Arc::clone(&lpm),
        exec_tx,
        max_transactions,
    )
    .await?;

    info!("All subsystems running — bot is live");

    if max_transactions.is_some() {
        engine_handle
            .await
            .map_err(|e| anyhow::anyhow!("Engine task panicked: {e}"))?;
    } else {
        tokio::signal::ctrl_c().await?;
        info!("Received Ctrl+C — shutting down");
    }

    Ok(())
}

use alloy::primitives::Address;
use std::str::FromStr;

use crate::error::{BotError, Result};

/// Runtime configuration loaded from environment / `.env` file at startup.
#[derive(Debug, Clone)]
pub struct BotConfig {
    // ── Network ───────────────────────────────────────────────────────────────
    /// HTTPS RPC endpoint (Arbitrum One or Sepolia).
    /// Used only for initial pool-reserve bootstrap.
    pub rpc_url: String,

    /// Arbitrum Nitro Sequencer Feed WebSocket URL.
    /// Mainnet:  wss://arb1-feed.arbitrum.io/feed
    /// Sepolia:  wss://sepolia-feed.arbitrum.io/feed
    pub sequencer_feed_url: String,

    /// Direct sequencer RPC endpoint for `eth_sendRawTransactionConditional`.
    /// This is distinct from the public RPC — use the sequencer's private endpoint
    /// to achieve sub-millisecond submission before the transaction is broadcast.
    pub direct_sequencer_rpc_url: String,

    // ── Bot Identity ─────────────────────────────────────────────────────────
    /// Raw hex private key (no 0x prefix) for the execution wallet.
    pub private_key_hex: String,

    /// Address of our deployed Solidity executor contract.
    pub executor_contract_address: Address,

    // ── Storage ───────────────────────────────────────────────────────────────
    /// Path to the SQLite DB written by the external C# pool-discovery service.
    pub db_path: String,

    // ── Profit Filter ─────────────────────────────────────────────────────────
    /// Minimum net profit (in wei of the output token) required to fire.
    /// Accounts for gas costs; set conservatively to avoid marginal trades.
    pub min_profit_wei: u128,

    // ── Routers ───────────────────────────────────────────────────────────────
    /// DEX router addresses to monitor on the sequencer feed.
    /// Any transaction whose `to` field does not match this set is ignored.
    pub monitored_routers: Vec<Address>,

    // ── Chain ─────────────────────────────────────────────────────────────────
    /// EIP-155 chain ID used when signing execution transactions.
    /// Arbitrum One = 42161, Arbitrum Sepolia = 421614, Anvil default = 31337.
    pub chain_id: u64,
}

impl BotConfig {
    /// Construct directly from values — used in integration tests to inject
    /// mock/local URLs without relying on environment variables.
    ///
    /// ```rust,ignore
    /// let config = BotConfig::new(
    ///     "http://localhost:8545",
    ///     "ws://localhost:8546/feed",
    ///     "http://localhost:8545",
    ///     "deadbeefdeadbeef...",   // 32-byte hex private key
    ///     Address::ZERO,           // executor contract (mock)
    ///     ":memory:",              // SQLite in-memory
    ///     0,                       // min_profit_wei
    ///     vec![],                  // monitored_routers
    /// );
    /// ```
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        rpc_url:                   impl Into<String>,
        sequencer_feed_url:        impl Into<String>,
        direct_sequencer_rpc_url:  impl Into<String>,
        private_key_hex:           impl Into<String>,
        executor_contract_address: Address,
        db_path:                   impl Into<String>,
        min_profit_wei:            u128,
        monitored_routers:         Vec<Address>,
        chain_id:                  u64,
    ) -> Self {
        Self {
            rpc_url:                  rpc_url.into(),
            sequencer_feed_url:       sequencer_feed_url.into(),
            direct_sequencer_rpc_url: direct_sequencer_rpc_url.into(),
            private_key_hex:          private_key_hex.into(),
            executor_contract_address,
            db_path:                  db_path.into(),
            min_profit_wei,
            monitored_routers,
            chain_id,
        }
    }

    /// Load from environment variables (reads `.env` via `dotenvy` first).
    pub fn from_env() -> Result<Self> {
        dotenvy::dotenv().ok(); // gracefully ignore missing .env

        let require = |key: &str| -> Result<String> {
            std::env::var(key).map_err(|_| BotError::Config(key.to_string()))
        };

        let router_csv = require("MONITORED_ROUTERS")?;
        let monitored_routers = router_csv
            .split(',')
            .map(|s| Address::from_str(s.trim()).map_err(|e| BotError::Config(e.to_string())))
            .collect::<Result<Vec<_>>>()?;

        Ok(Self {
            rpc_url:                   require("RPC_URL")?,
            sequencer_feed_url:        require("SEQUENCER_FEED_URL")?,
            direct_sequencer_rpc_url:  require("DIRECT_SEQUENCER_RPC_URL")?,
            private_key_hex:           require("PRIVATE_KEY_HEX")?,
            executor_contract_address: Address::from_str(&require("EXECUTOR_CONTRACT_ADDRESS")?)
                .map_err(|e| BotError::Config(e.to_string()))?,
            db_path:                   require("DB_PATH")?,
            min_profit_wei:            require("MIN_PROFIT_WEI")?
                .parse()
                .map_err(|e: std::num::ParseIntError| BotError::Config(e.to_string()))?,
            monitored_routers,
            chain_id: std::env::var("CHAIN_ID")
                .ok()
                .and_then(|s| s.parse().ok())
                .unwrap_or(42161), // Arbitrum One default
        })
    }
}

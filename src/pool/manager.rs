use std::str::FromStr;
use std::sync::Arc;

use alloy::primitives::{Address, U256};
use alloy::providers::{Provider, ProviderBuilder};
use alloy::rpc::types::eth::TransactionRequest;
use alloy::sol;
use alloy::sol_types::SolCall;
use dashmap::DashMap;
use tokio::sync::RwLock;
use tracing::{info, warn};

use crate::error::{BotError, Result};
use crate::pool::{LiquidityPool, PoolRef};
use crate::types::DexProtocol;

// ── Solidity interface stubs (ABI-only, no deployment) ────────────────────────

sol! {
    /// Uniswap V2 / Camelot pair — getReserves
    #[allow(missing_docs)]
    interface IUniswapV2Pair {
        function getReserves()
            external view
            returns (uint112 reserve0, uint112 reserve1, uint32 blockTimestampLast);
    }

    /// Uniswap V3 pool — slot0 + liquidity
    #[allow(missing_docs)]
    interface IUniswapV3Pool {
        function slot0()
            external view
            returns (
                uint160 sqrtPriceX96,
                int24   tick,
                uint16  observationIndex,
                uint16  observationCardinality,
                uint16  observationCardinalityNext,
                uint8   feeProtocol,
                bool    unlocked
            );
        function liquidity() external view returns (uint128);
    }
}

// ── LiquidityPoolManager ─────────────────────────────────────────────────────

/// Owns the in-memory pool registry.
///
/// Uses `DashMap<Address, PoolRef>` (sharded concurrent map) so the hot-path
/// evaluation loop can read pool state with **no mutex contention** in the
/// common case. Individual writes are wrapped in `Arc<RwLock<LiquidityPool>>`
/// to protect the dynamic state (reserve0/reserve1) during updates.
pub struct LiquidityPoolManager {
    pools:   DashMap<Address, PoolRef>,
    rpc_url: String,
}

impl LiquidityPoolManager {
    // ── Construction ──────────────────────────────────────────────────────────

    /// Load static pool metadata from SQLite, build the in-memory registry.
    /// Dynamic state (reserves) is zeroed until `fetch_all_reserves()` is called.
    pub async fn load_from_db(db_path: &str, rpc_url: &str) -> Result<Arc<Self>> {
        let db_path = db_path.to_string();

        let rows = tokio::task::spawn_blocking(move || -> Result<Vec<PoolRow>> {
            let conn = rusqlite::Connection::open(&db_path)?;
            let mut stmt = conn.prepare(
                "SELECT
                    pool_address,   dex_name,
                    token0_address, token1_address,
                    token0_decimals,token1_decimals,
                    token0_symbol,  token1_symbol,
                    fee_tier,       tick_spacing,
                    enabled
                FROM liquidity_pools
                WHERE enabled = 1",
            )?;
            let rows = stmt
                .query_map([], |row| {
                    Ok(PoolRow {
                        pool_address:    row.get(0)?,
                        dex_name:        row.get(1)?,
                        token0_address:  row.get(2)?,
                        token1_address:  row.get(3)?,
                        token0_decimals: row.get::<_, i64>(4)? as u8,
                        token1_decimals: row.get::<_, i64>(5)? as u8,
                        token0_symbol:   row.get(6)?,
                        token1_symbol:   row.get(7)?,
                        fee_tier:        row.get::<_, i64>(8)? as u32,
                        tick_spacing:    row.get::<_, i64>(9)? as i32,
                    })
                })?
                .collect::<rusqlite::Result<Vec<_>>>()?;
            Ok(rows)
        })
        .await
        .map_err(|e| BotError::Rpc(e.to_string()))??;

        info!(count = rows.len(), "Loaded pool rows from SQLite");

        let pools: DashMap<Address, PoolRef> = DashMap::with_capacity(rows.len());

        for row in rows {
            let addr = Address::from_str(&row.pool_address)
                .map_err(|e| BotError::Config(e.to_string()))?;
            let t0 = Address::from_str(&row.token0_address)
                .map_err(|e| BotError::Config(e.to_string()))?;
            let t1 = Address::from_str(&row.token1_address)
                .map_err(|e| BotError::Config(e.to_string()))?;

            let pool = LiquidityPool {
                pool_address:          addr,
                dex_name:              row.dex_name.clone(),
                protocol:              DexProtocol::from_str(&row.dex_name),
                token0_address:        t0,
                token1_address:        t1,
                token0_decimals:       row.token0_decimals,
                token1_decimals:       row.token1_decimals,
                token0_symbol:         row.token0_symbol,
                token1_symbol:         row.token1_symbol,
                fee_tier:              row.fee_tier,
                tick_spacing:          row.tick_spacing,
                enabled:               true,
                reserve0:              U256::ZERO,
                reserve1:              U256::ZERO,
                last_sequence_number:  0,
                timestamp:             0,
            };

            pools.insert(addr, Arc::new(RwLock::new(pool)));
        }

        Ok(Arc::new(Self { pools, rpc_url: rpc_url.to_string() }))
    }

    // ── Bootstrap: RPC reserve fetch ─────────────────────────────────────────

    /// Fetch current reserves for every pool from the RPC node.
    ///
    /// Runs sequentially (bootstrap is one-time; sequential RPC calls avoid
    /// the complex lifetime/Send bounds that parallel spawning of generic
    /// providers requires). Returns the latest observed block number.
    pub async fn fetch_all_reserves(&self) -> Result<u64> {
        let url = self
            .rpc_url
            .parse::<reqwest::Url>()
            .map_err(|e| BotError::Rpc(e.to_string()))?;
        // connect_http returns a concrete provider that owns its connection pool.
        let provider = ProviderBuilder::new().connect_http(url);

        let pool_entries: Vec<(Address, PoolRef)> = self
            .pools
            .iter()
            .map(|e| (*e.key(), Arc::clone(e.value())))
            .collect();

        let mut max_block: u64 = 0;
        for (addr, pool_ref) in pool_entries {
            match fetch_pool_reserves(addr, &pool_ref, &provider).await {
                Ok(block) => {
                    if block > max_block {
                        max_block = block;
                    }
                }
                Err(e) => warn!(pool = ?addr, "Reserve fetch failed: {e}"),
            }
        }

        info!(checkpoint_block = max_block, "Bootstrap reserve fetch complete");
        Ok(max_block)
    }

    // ── Hot-path accessors ────────────────────────────────────────────────────

    #[inline]
    pub fn get(&self, address: &Address) -> Option<PoolRef> {
        self.pools.get(address).map(|r| Arc::clone(&*r))
    }

    #[inline]
    pub fn contains(&self, address: &Address) -> bool {
        self.pools.contains_key(address)
    }

    pub fn iter(&self) -> impl Iterator<Item = PoolRef> + '_ {
        self.pools.iter().map(|r| Arc::clone(&*r))
    }

    pub fn pool_count(&self) -> usize {
        self.pools.len()
    }
}

// ── Per-pool RPC helpers ──────────────────────────────────────────────────────

async fn fetch_pool_reserves(
    addr: Address,
    pool_ref: &PoolRef,
    provider: &impl Provider,
) -> Result<u64> {
    let protocol = pool_ref.read().await.protocol;
    match protocol {
        DexProtocol::UniswapV2 | DexProtocol::Camelot => {
            fetch_v2_reserves(addr, pool_ref, provider).await
        }
        DexProtocol::UniswapV3 => fetch_v3_reserves(addr, pool_ref, provider).await,
        _ => {
            warn!(pool = ?addr, "Unknown protocol; skipping reserve fetch");
            Ok(0)
        }
    }
}

async fn fetch_v2_reserves(
    addr: Address,
    pool_ref: &PoolRef,
    provider: &impl Provider,
) -> Result<u64> {
    let calldata = IUniswapV2Pair::getReservesCall {}.abi_encode();

    let call_result = provider
        .call(TransactionRequest::default().to(addr).input(calldata.into()))
        .await
        .map_err(|e| BotError::Rpc(e.to_string()))?;

    // alloy 1.x+: abi_decode_returns takes only the data slice (validate removed).
    let decoded = IUniswapV2Pair::getReservesCall::abi_decode_returns(&call_result)
        .map_err(|e| BotError::AbiDecode(e.to_string()))?;

    let block = provider
        .get_block_number()
        .await
        .map_err(|e| BotError::Rpc(e.to_string()))?;

    let mut guard = pool_ref.write().await;
    guard.reserve0 = U256::from(decoded.reserve0);
    guard.reserve1 = U256::from(decoded.reserve1);

    Ok(block)
}

async fn fetch_v3_reserves(
    addr: Address,
    pool_ref: &PoolRef,
    provider: &impl Provider,
) -> Result<u64> {
    let slot0_data = IUniswapV3Pool::slot0Call {}.abi_encode();
    let liq_data   = IUniswapV3Pool::liquidityCall {}.abi_encode();

    // V3 calls are independent; run concurrently.
    let (slot0_res, liq_res) = tokio::try_join!(
        provider.call(TransactionRequest::default().to(addr).input(slot0_data.into())),
        provider.call(TransactionRequest::default().to(addr).input(liq_data.into())),
    )
    .map_err(|e| BotError::Rpc(e.to_string()))?;

    let slot0 = IUniswapV3Pool::slot0Call::abi_decode_returns(&slot0_res)
        .map_err(|e| BotError::AbiDecode(e.to_string()))?;
    let liq   = IUniswapV3Pool::liquidityCall::abi_decode_returns(&liq_res)
        .map_err(|e| BotError::AbiDecode(e.to_string()))?;

    let block = provider
        .get_block_number()
        .await
        .map_err(|e| BotError::Rpc(e.to_string()))?;

    let mut guard = pool_ref.write().await;
    // reserve0 = sqrtPriceX96, reserve1 = in-range liquidity (see pool/mod.rs docs).
    guard.reserve0 = U256::from(slot0.sqrtPriceX96);
    // liquidityCall returns bare u128 (single unnamed return → primitive, not struct).
    guard.reserve1 = U256::from(liq);

    Ok(block)
}

// ── Internal SQLite row struct ────────────────────────────────────────────────

struct PoolRow {
    pool_address:    String,
    dex_name:        String,
    token0_address:  String,
    token1_address:  String,
    token0_decimals: u8,
    token1_decimals: u8,
    token0_symbol:   String,
    token1_symbol:   String,
    fee_tier:        u32,
    tick_spacing:    i32,
}

pub mod manager;

use alloy::primitives::{Address, U256};
use std::sync::Arc;
use tokio::sync::RwLock;

use crate::types::DexProtocol;

/// Full representation of a monitored liquidity pool.
///
/// Static fields (addresses, fee_tier, etc.) are set once at startup and never
/// mutate. Dynamic fields (reserve0, reserve1) are updated in the hot path by
/// `update()` and must be accessed through `Arc<RwLock<LiquidityPool>>`.
#[derive(Debug, Clone)]
pub struct LiquidityPool {
    // ── Static identity ───────────────────────────────────────────────────────
    pub pool_address:   Address,
    pub dex_name:       String,
    pub protocol:       DexProtocol,

    pub token0_address: Address,
    pub token1_address: Address,
    pub token0_decimals: u8,
    pub token1_decimals: u8,
    pub token0_symbol:  String,
    pub token1_symbol:  String,

    /// For V2/Camelot: basis-points fee (e.g., 3000 = 0.30 %).
    /// For V3/V4:      the fee tier used to locate the exact pool contract.
    pub fee_tier:     u32,

    /// V3/V4 only — spacing between initializable ticks. Zero for V2.
    pub tick_spacing: i32,

    pub enabled: bool,

    // ── Dynamic state (updated live from sequencer feed) ──────────────────────
    /// Token0 reserve / sqrtPriceX96 depending on protocol.
    ///
    /// V2 / Camelot:  reserve0 in token0's smallest unit (uint112).
    /// V3:            sqrtPriceX96 packed in lower 160 bits (uint160 cast to U256).
    ///                reserve1 holds the in-range liquidity (uint128).
    pub reserve0: U256,

    /// Token1 reserve (V2) or in-range liquidity (V3, stored in lower 128 bits).
    pub reserve1: U256,

    /// Sequence number of the last feed message that mutated this pool's state.
    /// Used during bootstrap reconciliation to know which buffered transactions
    /// are newer than the RPC snapshot.
    pub last_sequence_number: u64,

    /// Unix timestamp (seconds) of the last update — for staleness checks.
    pub timestamp: u64,
}

impl LiquidityPool {
    /// Apply a reserve update atomically. Called from the hot-path
    /// sequencer feed processor after simulating the user's swap.
    ///
    /// The caller must hold a write lock on the containing `Arc<RwLock<Self>>`.
    #[inline]
    pub fn update(&mut self, res0: U256, res1: U256, seq_num: u64) {
        self.reserve0              = res0;
        self.reserve1              = res1;
        self.last_sequence_number  = seq_num;
        self.timestamp             = unix_now();
    }

    // ── AMM simulation helpers ─────────────────────────────────────────────────
    //
    // These are called in the ArbitrageEngine evaluation loop to predict post-
    // swap state WITHOUT touching the RPC node. Every nanosecond saved here
    // is a nanosecond of advantage over latency-unaware bots.
    //
    // INJECT: Replace the `unimplemented!()` bodies with the correct invariant
    // for each DEX protocol, keyed on `self.protocol`.

    /// Simulate a V2-style constant-product swap (x * y = k).
    ///
    /// Returns `(amount_out, new_reserve0, new_reserve1)`.
    ///
    /// # V2 AMM Invariant
    ///
    /// Given input amount `amount_in` of token0 (after fee deduction):
    ///   amount_in_with_fee = amount_in * (10_000 - fee_bps)
    ///   amount_out = (amount_in_with_fee * reserve1)
    ///              / (reserve0 * 10_000 + amount_in_with_fee)
    ///   new_reserve0 = reserve0 + amount_in
    ///   new_reserve1 = reserve1 - amount_out
    ///
    /// Camelot uses a dual-fee (LP fee + protocol fee) mechanism — adjust
    /// `fee_bps` accordingly when `self.protocol == DexProtocol::Camelot`.
    pub fn simulate_v2_swap_exact_in(
        &self,
        amount_in: U256,
        zero_for_one: bool, // true: sell token0, false: sell token1
    ) -> crate::error::Result<(U256, U256, U256)> {
        use crate::error::BotError;

        let fee_bps = U256::from(self.fee_tier); // e.g. 3000 for 0.30%; Camelot stores total combined bps
        let ten_k   = U256::from(10_000u32);

        let (reserve_in, reserve_out) = if zero_for_one {
            (self.reserve0, self.reserve1)
        } else {
            (self.reserve1, self.reserve0)
        };

        if reserve_in.is_zero() || reserve_out.is_zero() {
            return Err(BotError::Overflow);
        }

        // UniswapV2Library.getAmountOut:
        //   amountInWithFee = amountIn * (10000 - feeBps)
        //   amountOut = (amountInWithFee * reserveOut) / (reserveIn * 10000 + amountInWithFee)
        let fee_complement     = ten_k.checked_sub(fee_bps).ok_or(BotError::Overflow)?;
        let amount_in_with_fee = amount_in.checked_mul(fee_complement).ok_or(BotError::Overflow)?;

        let numerator   = amount_in_with_fee.checked_mul(reserve_out).ok_or(BotError::Overflow)?;
        let denominator = reserve_in
            .checked_mul(ten_k)
            .ok_or(BotError::Overflow)?
            .checked_add(amount_in_with_fee)
            .ok_or(BotError::Overflow)?;

        let amount_out = numerator / denominator;

        if amount_out >= reserve_out {
            return Err(BotError::Overflow);
        }

        let (new_reserve0, new_reserve1) = if zero_for_one {
            (
                self.reserve0.checked_add(amount_in).ok_or(BotError::Overflow)?,
                self.reserve1 - amount_out, // safe: checked amount_out < reserve_out
            )
        } else {
            (
                self.reserve0 - amount_out,
                self.reserve1.checked_add(amount_in).ok_or(BotError::Overflow)?,
            )
        };

        Ok((amount_out, new_reserve0, new_reserve1))
    }

    /// Simulate a V3-style concentrated-liquidity swap (sqrt-price model).
    ///
    /// For a simple approximation suitable for in-range single-tick swaps:
    ///
    /// # V3 Approximation (in-range, single-tick)
    ///
    ///   sqrtP_new = sqrtP_current ± (amount_in * 2^96) / liquidity
    ///   amount_out = liquidity * |1/sqrtP_new - 1/sqrtP_current| * 2^96
    ///
    /// For a production implementation, port the full `SwapMath.computeSwapStep`
    /// logic from the Uniswap V3 Solidity contracts. This requires iterating
    /// across initialised tick boundaries stored in the pool's tick bitmap.
    ///
    /// Returns `(amount_out, new_sqrt_price_x96, new_liquidity)`.
    pub fn simulate_v3_swap_exact_in(
        &self,
        amount_in: U256,
        zero_for_one: bool,
        sqrt_price_limit_x96: U256,
    ) -> crate::error::Result<(U256, U256, U256)> {
        use crate::error::BotError;

        // reserve0 = sqrtPriceX96 (Q64.96), reserve1 = in-range liquidity (uint128)
        let sqrt_price = self.reserve0;
        let liquidity  = self.reserve1;

        let q96 = U256::from(1u64) << 96;

        if sqrt_price.is_zero() || liquidity.is_zero() {
            return Err(BotError::Overflow);
        }

        if zero_for_one {
            // Selling token0 → price falls.
            //
            // sqrtP_new = L * sqrtP / (L + amount_in * sqrtP / Q96)
            // amount_out (token1) = L * (sqrtP - sqrtP_new) / Q96

            let amount_in_scaled = amount_in
                .checked_mul(sqrt_price)
                .ok_or(BotError::Overflow)?
                / q96;

            let denom = liquidity.checked_add(amount_in_scaled).ok_or(BotError::Overflow)?;

            let mut new_sqrt = liquidity
                .checked_mul(sqrt_price)
                .ok_or(BotError::Overflow)?
                / denom;

            if !sqrt_price_limit_x96.is_zero() && new_sqrt < sqrt_price_limit_x96 {
                new_sqrt = sqrt_price_limit_x96;
            }

            if new_sqrt > sqrt_price {
                return Err(BotError::Overflow);
            }

            let delta_sqrt = sqrt_price - new_sqrt;

            // L * delta_sqrt may overflow U256 for extreme pool states; checked accordingly.
            let amount_out = liquidity
                .checked_mul(delta_sqrt)
                .ok_or(BotError::Overflow)?
                / q96;

            Ok((amount_out, new_sqrt, liquidity))
        } else {
            // Selling token1 → price rises.
            //
            // sqrtP_new = sqrtP + amount_in * Q96 / L
            // amount_out (token0) = L * Q96 * (sqrtP_new - sqrtP) / (sqrtP_new * sqrtP)
            //                     = (L * Q96 / sqrtP) * delta_sqrt / sqrtP_new

            let delta_sqrt = amount_in
                .checked_mul(q96)
                .ok_or(BotError::Overflow)?
                / liquidity;

            let mut new_sqrt = sqrt_price.checked_add(delta_sqrt).ok_or(BotError::Overflow)?;

            if !sqrt_price_limit_x96.is_zero() && new_sqrt > sqrt_price_limit_x96 {
                new_sqrt = sqrt_price_limit_x96;
            }

            let actual_delta = new_sqrt - sqrt_price;

            // L * Q96 ≤ 2^224, fits in U256; divide by sqrtP before multiplying by
            // actual_delta to keep the intermediate within 256 bits for typical pools.
            let lq96       = liquidity.checked_mul(q96).ok_or(BotError::Overflow)?;
            let amount_out = lq96
                .checked_div(sqrt_price)
                .ok_or(BotError::Overflow)?
                .checked_mul(actual_delta)
                .ok_or(BotError::Overflow)?
                / new_sqrt;

            Ok((amount_out, new_sqrt, liquidity))
        }
    }
}

// ── Thread-safe pool reference type ──────────────────────────────────────────

/// Alias used throughout the codebase to avoid verbose type annotations.
pub type PoolRef = Arc<RwLock<LiquidityPool>>;

fn unix_now() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs()
}

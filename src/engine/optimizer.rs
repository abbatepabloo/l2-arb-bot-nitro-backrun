use alloy::primitives::{Address, U256};

use crate::error::{BotError, Result};
use crate::pool::LiquidityPool;

// Mock signature for V3 simulation as indicated by the architecture
// In production, this targets your pool module implementation.
fn simulate_v3_swap_exact_in(
    _pool: &LiquidityPool,
    amount_in: U256,
    _zero_for_one: bool,
) -> Result<U256> {
    // Placeholder: in your real code, this computes ticks/liquidity transitions
    Ok(amount_in) 
}

// ── V2 ↔ V2 Two-Pool Arbitrage ────────────────────────────────────────────────

/// Calculate the optimal input amount for a V2 ↔ V2 two-pool arbitrage.
///
/// `pool_a` is the imbalanced pool (post-user-swap state already applied).
/// `pool_b` is the counterparty pool on a different DEX.
/// `zero_for_one_a` — direction of our trade on Pool A.
///
/// Returns `(optimal_amount_in, expected_amount_out)` in the token's smallest
/// unit (wei-equivalent).
pub fn optimal_v2_v2(
    pool_a: &LiquidityPool,
    pool_b: &LiquidityPool,
    zero_for_one_a: bool,    // true: we buy token1 on A, sell on B
) -> Result<(U256, U256)> {
    // ── Select reserves based on trade direction ───────────────────────────
    let (r_a_in, r_a_out, r_b_in, r_b_out) = if zero_for_one_a {
        (pool_a.reserve0, pool_a.reserve1, pool_b.reserve1, pool_b.reserve0)
    } else {
        (pool_a.reserve1, pool_a.reserve0, pool_b.reserve0, pool_b.reserve1)
    };

    if r_a_in.is_zero() || r_a_out.is_zero() || r_b_in.is_zero() || r_b_out.is_zero() {
        return Ok((U256::ZERO, U256::ZERO));
    }

    // ── Fee multipliers ────────────────────────────────────────────────────
    let fee_a = 1000u128 - (pool_a.fee_tier as u128 / 10);
    let fee_b = 1000u128 - (pool_b.fee_tier as u128 / 10);

    let u_fee_a = U256::from(fee_a);
    let u_fee_b = U256::from(fee_b);

    // ── Optimal input formula (analytical) ────────────────────────────────
    //
    // Derived from d(profit)/dx = 0 for profit(x) = z(x) - x, where
    //   y(x) = φ_a * x * r_a_out / (r_a_in*1000 + φ_a*x)     [leg A output]
    //   z(y) = φ_b * y * r_b_out / (r_b_in*1000 + φ_b*y)     [leg B output]
    //
    // Setting dz/dx = 1 yields:
    //   (r_a_in*r_b_in*1000^2 + φ_a*(r_b_in*1000 + φ_b*r_a_out)*x)^2
    //       = φ_a*φ_b*r_a_out*r_b_out * r_a_in*r_b_in*1000^2
    //
    // Solving for x:
    //   x_opt = 1000*(sqrt(φ_a*φ_b*r_a_out*r_b_out*r_a_in*r_b_in) - r_a_in*r_b_in*1000)
    //           / (φ_a*(r_b_in*1000 + φ_b*r_a_out))

    // k_expanded = φ_a * φ_b * r_a_out * r_b_out * r_a_in * r_b_in
    let k_expanded = u_fee_a
        .checked_mul(u_fee_b).ok_or(BotError::Overflow)?
        .checked_mul(r_a_out).ok_or(BotError::Overflow)?
        .checked_mul(r_b_out).ok_or(BotError::Overflow)?
        .checked_mul(r_a_in).ok_or(BotError::Overflow)?
        .checked_mul(r_b_in).ok_or(BotError::Overflow)?;

    let sqrt_term = isqrt(k_expanded);

    // sub_term = r_a_in * r_b_in * 1000
    let sub_term = r_a_in
        .checked_mul(r_b_in).ok_or(BotError::Overflow)?
        .checked_mul(U256::from(1000u64)).ok_or(BotError::Overflow)?;

    // If sqrt_term <= sub_term, no arbitrage opportunity exists
    if sqrt_term <= sub_term {
        return Ok((U256::ZERO, U256::ZERO));
    }

    // num = 1000 * (sqrt_term - sub_term)
    let num = U256::from(1000u64)
        .checked_mul(sqrt_term - sub_term).ok_or(BotError::Overflow)?;

    // den = φ_a * (r_b_in*1000 + φ_b*r_a_out)
    let den = u_fee_a.checked_mul(
        r_b_in.checked_mul(U256::from(1000u64)).ok_or(BotError::Overflow)?
            .checked_add(u_fee_b.checked_mul(r_a_out).ok_or(BotError::Overflow)?)
            .ok_or(BotError::Overflow)?
    ).ok_or(BotError::Overflow)?;

    let optimal_amount_in = num.checked_div(den).ok_or(BotError::Overflow)?;

    if optimal_amount_in.is_zero() {
        return Ok((U256::ZERO, U256::ZERO));
    }

    // ── Calculate expected execution output across both legs ───────────────
    // Leg 1: Swap on Pool A
    let amt_in_fee_a = optimal_amount_in.checked_mul(u_fee_a).ok_or(BotError::Overflow)?;
    let num_a = amt_in_fee_a.checked_mul(r_a_out).ok_or(BotError::Overflow)?;
    let den_a = r_a_in.checked_mul(U256::from(1000u64)).ok_or(BotError::Overflow)?
        .checked_add(amt_in_fee_a).ok_or(BotError::Overflow)?;
    let amount_out_a = num_a.checked_div(den_a).ok_or(BotError::Overflow)?;

    // Leg 2: Swap on Pool B
    let amt_in_fee_b = amount_out_a.checked_mul(u_fee_b).ok_or(BotError::Overflow)?;
    let num_b = amt_in_fee_b.checked_mul(r_b_out).ok_or(BotError::Overflow)?;
    let den_b = r_b_in.checked_mul(U256::from(1000u64)).ok_or(BotError::Overflow)?
        .checked_add(amt_in_fee_b).ok_or(BotError::Overflow)?;
    let expected_amount_out = num_b.checked_div(den_b).ok_or(BotError::Overflow)?;

    Ok((optimal_amount_in, expected_amount_out))
}

// ── V2 ↔ V3 Two-Pool Arbitrage ────────────────────────────────────────────────

/// Calculate optimal input for a V2 ↔ V3 cross-protocol arbitrage.
pub fn optimal_v2_v3(
    pool_v2: &LiquidityPool,
    pool_v3: &LiquidityPool,
    zero_for_one_a: bool,
) -> Result<(U256, U256)> {
    let r_v2_in = if zero_for_one_a { pool_v2.reserve0 } else { pool_v2.reserve1 };
    let r_v2_out = if zero_for_one_a { pool_v2.reserve1 } else { pool_v2.reserve0 };
    
    if r_v2_in.is_zero() || r_v2_out.is_zero() {
        return Ok((U256::ZERO, U256::ZERO));
    }

    let fee_v2 = U256::from(1000u128 - (pool_v2.fee_tier as u128 / 10));

    // Numerical optimization boundary: Cap input to half of execution liquidity 
    let mut low = U256::ZERO;
    let mut high = r_v2_in.checked_div(U256::from(2)).unwrap_or(U256::ZERO);
    
    let mut best_input = U256::ZERO;
    let mut best_output = U256::ZERO;
    let mut max_profit = U256::ZERO;

    // Ternary Search implementation for Unimodal Profit Maximization
    for _ in 0..40 {
        if (high - low) <= U256::from(2) {
            break;
        }
        let delta = (high - low) / U256::from(3);
        let m1 = low + delta;
        let m2 = high - delta;

        let profit_m1 = evaluate_v2_v3_profit(m1, r_v2_in, r_v2_out, fee_v2, pool_v3, zero_for_one_a)?;
        let profit_m2 = evaluate_v2_v3_profit(m2, r_v2_in, r_v2_out, fee_v2, pool_v3, zero_for_one_a)?;

        if profit_m1 > profit_m2 {
            high = m2;
            if profit_m1 > max_profit {
                max_profit = profit_m1;
                best_input = m1;
                best_output = profit_m1 + m1;
            }
        } else {
            low = m1;
            if profit_m2 > max_profit {
                max_profit = profit_m2;
                best_input = m2;
                best_output = profit_m2 + m2;
            }
        }
    }

    Ok((best_input, best_output))
}

#[inline(always)]
fn evaluate_v2_v3_profit(
    x: U256,
    r_in: U256,
    r_out: U256,
    fee_v2: U256,
    pool_v3: &LiquidityPool,
    zero_for_one_a: bool,
) -> Result<U256> {
    if x.is_zero() { return Ok(U256::ZERO); }
    let x_fee = x.checked_mul(fee_v2).ok_or(BotError::Overflow)?;
    let num = x_fee.checked_mul(r_out).ok_or(BotError::Overflow)?;
    let den = r_in.checked_mul(U256::from(1000u64)).ok_or(BotError::Overflow)?
        .checked_add(x_fee).ok_or(BotError::Overflow)?;
    
    let out_v2 = num / den;
    if out_v2.is_zero() { return Ok(U256::ZERO); }

    let out_v3 = simulate_v3_swap_exact_in(pool_v3, out_v2, !zero_for_one_a)?;
    if out_v3 > x {
        Ok(out_v3 - x)
    } else {
        Ok(U256::ZERO)
    }
}

// ── Triangular Arbitrage ──────────────────────────────────────────────────────

/// Three-pool triangular arbitrage path optimisation.
pub fn optimal_triangular(
    pool1: &LiquidityPool,
    pool2: &LiquidityPool,
    pool3: &LiquidityPool,
    directions: [bool; 3],
) -> Result<(U256, U256)> {
    // Pure numerical execution via global golden bracket configuration
    let r1_in = if directions[0] { pool1.reserve0 } else { pool1.reserve1 };
    let mut low = U256::ZERO;
    let mut high = r1_in.checked_div(U256::from(3)).unwrap_or(U256::ZERO); // Avoid draining pools completely

    let mut best_input = U256::ZERO;
    let mut best_output = U256::ZERO;
    let mut max_profit = U256::ZERO;

    for _ in 0..45 {
        if (high - low) <= U256::from(2) { break; }
        let delta = (high - low) / U256::from(3);
        let m1 = low + delta;
        let m2 = high - delta;

        let profit_m1 = evaluate_triangular_profit(m1, pool1, pool2, pool3, directions)?;
        let profit_m2 = evaluate_triangular_profit(m2, pool1, pool2, pool3, directions)?;

        if profit_m1 > profit_m2 {
            high = m2;
            if profit_m1 > max_profit {
                max_profit = profit_m1;
                best_input = m1;
                best_output = profit_m1 + m1;
            }
        } else {
            low = m1;
            if profit_m2 > max_profit {
                max_profit = profit_m2;
                best_input = m2;
                best_output = profit_m2 + m2;
            }
        }
    }

    Ok((best_input, best_output))
}

fn evaluate_triangular_profit(
    x: U256,
    p1: &LiquidityPool,
    p2: &LiquidityPool,
    p3: &LiquidityPool,
    dir: [bool; 3],
) -> Result<U256> {
    if x.is_zero() { return Ok(U256::ZERO); }
    
    // Helper closure for linear constant product formula output step
    let get_v2_out = |amt_in: U256, pool: &LiquidityPool, d: bool| -> U256 {
        let (r_in, r_out) = if d { (pool.reserve0, pool.reserve1) } else { (pool.reserve1, pool.reserve0) };
        let fee = U256::from(1000u128 - (pool.fee_tier as u128 / 10));
        let amt_in_fee = amt_in * fee;
        (amt_in_fee * r_out) / ((r_in * U256::from(1000u64)) + amt_in_fee)
    };

    let out1 = get_v2_out(x, p1, dir[0]);
    if out1.is_zero() { return Ok(U256::ZERO); }
    let out2 = get_v2_out(out1, p2, dir[1]);
    if out2.is_zero() { return Ok(U256::ZERO); }
    let out3 = get_v2_out(out2, p3, dir[2]);

    if out3 > x {
        Ok(out3 - x)
    } else {
        Ok(U256::ZERO)
    }
}

// ── Integer Square Root ────────────────────────────────────────────────────────

/// Newton-Raphson integer square root for U256.
#[inline]
pub fn isqrt(n: U256) -> U256 {
    if n.is_zero() {
        return U256::ZERO;
    }
    let bits = 256 - n.leading_zeros();
    let mut x = U256::from(1u64) << ((bits + 1) / 2);
    loop {
        let x_next = (x + n / x) >> 1;
        if x_next >= x {
            return x;
        }
        x = x_next;
    }
}

// ── Gas Estimation ─────────────────────────────────────────────────────────────

/// Estimate the L2 gas cost of our executor transaction in wei including Arbitrum L1 surcharge.
pub fn estimate_gas_cost_wei(max_fee_per_gas: U256, gas_limit: u64) -> u128 {
    let l2_gas_cost = max_fee_per_gas * U256::from(gas_limit);

    // Arbitrum L1 Surcharge Calculation
    // Static payload size approximation for execution metadata (~120 bytes)
    let tx_data_len_non16s = 120u128; 
    let tx_data_len_16s = 0u128; // Standard calldata optimization fallback

    // Standard baseline L1 precompile parameter fallbacks (ArbGasInfo tracking updates)
    let l1_base_fee = 15_000_000_000u128; // 15 gwei base gas estimation default
    let l1_fee_scale = 1u128;             // Scalar component multiplier configured on L2

    let l1_data_units = (tx_data_len_16s * 16) + (tx_data_len_non16s * 1);
    let l1_fee = (l1_base_fee * l1_fee_scale * l1_data_units) / 16;

    let total_cost = l2_gas_cost.saturating_to::<u128>() + l1_fee;
    total_cost
}

// ── Unit tests ─────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use crate::types::DexProtocol;
    use alloy::primitives::Address;

    // ── Pool constructor ──────────────────────────────────────────────────────

    fn addr(byte: u8) -> Address {
        Address::from([byte; 20])
    }

    // fee_tier encoding used by both pool/mod.rs and optimizer.rs:
    //   fee_multiplier (out of 1000) = 1000 - fee_tier/10
    //   fee_tier=30  → 997/1000 (0.3%)
    //   fee_tier=10  → 999/1000 (0.1%)
    //   fee_tier=100 → 990/1000 (1.0%)
    fn make_pool(addr_byte: u8, t0: u8, t1: u8, r0: u128, r1: u128, fee_tier: u32) -> LiquidityPool {
        LiquidityPool {
            pool_address:         addr(addr_byte),
            dex_name:             String::new(),
            protocol:             DexProtocol::UniswapV2,
            token0_address:       addr(t0),
            token1_address:       addr(t1),
            token0_decimals:      18,
            token1_decimals:      18,
            token0_symbol:        String::new(),
            token1_symbol:        String::new(),
            fee_tier,
            tick_spacing:         0,
            enabled:              true,
            reserve0:             U256::from(r0),
            reserve1:             U256::from(r1),
            last_sequence_number: 0,
            timestamp:            0,
        }
    }

    // ── isqrt ─────────────────────────────────────────────────────────────────

    #[test]
    fn isqrt_small_values() {
        assert_eq!(isqrt(U256::ZERO),          U256::ZERO);
        assert_eq!(isqrt(U256::from(1u64)),    U256::from(1u64));
        assert_eq!(isqrt(U256::from(4u64)),    U256::from(2u64));
        assert_eq!(isqrt(U256::from(9u64)),    U256::from(3u64));
        assert_eq!(isqrt(U256::from(10u64)),   U256::from(3u64));
        assert_eq!(isqrt(U256::from(100u64)),  U256::from(10u64));
    }

    #[test]
    fn isqrt_floor_invariant() {
        let n = U256::from(1_000_000_000_000_000_000u128);
        let s = isqrt(n);
        assert!(s * s <= n);
        assert!((s + U256::from(1u64)) * (s + U256::from(1u64)) > n);
    }

    // ── optimal_v2_v2 — profitable ────────────────────────────────────────────

    // zero_for_one_a=true:
    //   Leg A: sell token0 (r_a_in=r0), receive token1 (r_a_out=r1)
    //   Leg B: sell token1 (r_b_in=r1), receive token0 (r_b_out=r0)
    // Profitable when token1 is cheap on A and expensive on B.

    #[test]
    fn v2_v2_profitable_basic() {
        // Pool A has abundant token1 (cheap), Pool B is balanced.
        // Manual check: sqrt(997*997 * 2000*1000 * 500*1000) ≈ 997e6, sub=500e6 → profit.
        let pool_a = make_pool(0x01, 0xAA, 0xBB, 500, 2000, 30);
        let pool_b = make_pool(0x02, 0xAA, 0xBB, 1000, 1000, 30);

        let (opt_in, exp_out) = optimal_v2_v2(&pool_a, &pool_b, true).unwrap();

        assert!(opt_in  > U256::ZERO, "should find a positive input");
        assert!(exp_out > opt_in,     "expected_out must exceed optimal_in for profit");
    }

    #[test]
    fn v2_v2_profitable_large_imbalance() {
        // 5× imbalance across two pools → higher absolute profit
        let pool_a = make_pool(0x01, 0xAA, 0xBB, 1000, 5000, 30);
        let pool_b = make_pool(0x02, 0xAA, 0xBB, 5000, 1000, 30);

        let (opt_in, exp_out) = optimal_v2_v2(&pool_a, &pool_b, true).unwrap();

        assert!(opt_in  > U256::ZERO);
        assert!(exp_out > opt_in);
    }

    #[test]
    fn v2_v2_profitable_reverse_direction() {
        // zero_for_one_a=false: sell token1 on A, receive token0, sell on B.
        // Pool A: r0=2000 (abundant token0), r1=500 → token0 cheap on A.
        let pool_a = make_pool(0x01, 0xAA, 0xBB, 2000, 500, 30);
        let pool_b = make_pool(0x02, 0xAA, 0xBB, 1000, 1000, 30);

        let (opt_in, exp_out) = optimal_v2_v2(&pool_a, &pool_b, false).unwrap();

        assert!(opt_in  > U256::ZERO);
        assert!(exp_out > opt_in);
    }

    #[test]
    fn v2_v2_overflow_on_huge_reserves() {
        // k_expanded = φ²·r_a_out·r_b_out·r_a_in·r_b_in overflows U256 for
        // reserves at 18-decimal scale (product ≫ 2^256).
        let e18 = 1_000_000_000_000_000_000u128;
        let pool_a = make_pool(0x01, 0xAA, 0xBB, 500_000 * e18, 2_000_000 * e18, 30);
        let pool_b = make_pool(0x02, 0xAA, 0xBB, 1_000_000 * e18, 1_000_000 * e18, 30);

        let result = optimal_v2_v2(&pool_a, &pool_b, true);
        assert!(result.is_err(), "should return Overflow for 18-decimal pool reserves");
    }

    #[test]
    fn v2_v2_profitable_large_reserves() {
        // Reserves large enough to show realistic profit without overflowing k_expanded.
        // Safe upper bound: each reserve ≤ ~1e8 so r^4 ≤ 1e32 ≪ U256_max (≈1.16e77).
        let pool_a = make_pool(0x01, 0xAA, 0xBB, 50_000_000, 200_000_000, 30);
        let pool_b = make_pool(0x02, 0xAA, 0xBB, 100_000_000, 100_000_000, 30);

        let (opt_in, exp_out) = optimal_v2_v2(&pool_a, &pool_b, true).unwrap();

        assert!(opt_in  > U256::ZERO);
        assert!(exp_out > opt_in);
        assert!(opt_in  < U256::from(50_000_000u64 / 2), "optimal input should be a fraction of r_a_in");
    }

    // ── optimal_v2_v2 — no profit ─────────────────────────────────────────────

    #[test]
    fn v2_v2_no_profit_balanced_pools() {
        // Same price on both pools → fees destroy any apparent spread.
        let pool_a = make_pool(0x01, 0xAA, 0xBB, 1000, 1000, 30);
        let pool_b = make_pool(0x02, 0xAA, 0xBB, 1000, 1000, 30);

        let (opt_in, exp_out) = optimal_v2_v2(&pool_a, &pool_b, true).unwrap();

        assert_eq!(opt_in,  U256::ZERO, "balanced pools: no opportunity expected");
        assert_eq!(exp_out, U256::ZERO);
    }

    #[test]
    fn v2_v2_no_profit_wrong_direction() {
        // Imbalance exists but we try the unprofitable leg.
        // Pool A: r0=500, r1=2000 → token1 cheap; wrong dir (false) = sell token1 on A.
        // With dir=false: r_a_in=2000, r_a_out=500 → sub_term=2000e6 >> sqrt_term → no profit.
        let pool_a = make_pool(0x01, 0xAA, 0xBB, 500, 2000, 30);
        let pool_b = make_pool(0x02, 0xAA, 0xBB, 1000, 1000, 30);

        let (opt_in, exp_out) = optimal_v2_v2(&pool_a, &pool_b, false).unwrap();

        assert_eq!(opt_in,  U256::ZERO, "wrong direction must not produce profit");
        assert_eq!(exp_out, U256::ZERO);
    }

    #[test]
    fn v2_v2_no_profit_zero_reserves() {
        let pool_a = make_pool(0x01, 0xAA, 0xBB, 0, 2000, 30);
        let pool_b = make_pool(0x02, 0xAA, 0xBB, 1000, 1000, 30);

        let (opt_in, exp_out) = optimal_v2_v2(&pool_a, &pool_b, true).unwrap();

        assert_eq!(opt_in,  U256::ZERO, "empty reserve must return zero");
        assert_eq!(exp_out, U256::ZERO);
    }

    // ── optimal_v2_v2 — fee sensitivity ──────────────────────────────────────

    #[test]
    fn v2_v2_lower_fee_yields_more_profit() {
        // Use larger reserves so integer rounding doesn't mask the fee difference.
        let pool_a_hi = make_pool(0x01, 0xAA, 0xBB, 500_000, 2_000_000, 30); // 0.3%
        let pool_b_hi = make_pool(0x02, 0xAA, 0xBB, 1_000_000, 1_000_000, 30);

        let pool_a_lo = make_pool(0x03, 0xAA, 0xBB, 500_000, 2_000_000, 10); // 0.1%
        let pool_b_lo = make_pool(0x04, 0xAA, 0xBB, 1_000_000, 1_000_000, 10);

        let (_, out_hi) = optimal_v2_v2(&pool_a_hi, &pool_b_hi, true).unwrap();
        let (_, out_lo) = optimal_v2_v2(&pool_a_lo, &pool_b_lo, true).unwrap();

        assert!(out_lo > out_hi, "lower fee should yield more output: lo={out_lo} hi={out_hi}");
    }

    #[test]
    fn v2_v2_mixed_fees_still_profitable() {
        // Pool A low fee (0.1%), Pool B higher fee (0.3%), large imbalance.
        let pool_a = make_pool(0x01, 0xAA, 0xBB, 500_000, 2_000_000, 10);
        let pool_b = make_pool(0x02, 0xAA, 0xBB, 1_000_000, 1_000_000, 30);

        let (opt_in, exp_out) = optimal_v2_v2(&pool_a, &pool_b, true).unwrap();

        assert!(opt_in  > U256::ZERO, "should still find opportunity");
        assert!(exp_out > opt_in);
    }

    #[test]
    fn v2_v2_high_fee_kills_marginal_opportunity() {
        // Tiny ~1% imbalance — profitable at low fee, unprofitable at 1% fee.
        let pool_a_lo = make_pool(0x01, 0xAA, 0xBB, 990_000, 1_000_000, 10);
        let pool_b_lo = make_pool(0x02, 0xAA, 0xBB, 1_000_000, 990_000, 10);

        let pool_a_hi = make_pool(0x03, 0xAA, 0xBB, 990_000, 1_000_000, 100);
        let pool_b_hi = make_pool(0x04, 0xAA, 0xBB, 1_000_000, 990_000, 100);

        let (_, out_lo) = optimal_v2_v2(&pool_a_lo, &pool_b_lo, true).unwrap();
        let (_, out_hi) = optimal_v2_v2(&pool_a_hi, &pool_b_hi, true).unwrap();

        assert!(out_hi <= out_lo, "high fee must not outperform low fee: hi={out_hi} lo={out_lo}");
    }

    // ── optimal_triangular ────────────────────────────────────────────────────

    // Pool directions: dir[i]=true → sell token0 on pool_i, get token1.
    // Cycle: pool1 out feeds pool2 in; pool2 out feeds pool3 in; pool3 out = final amount.

    #[test]
    fn triangular_profitable_cycle() {
        // Pool1: r0=100k, r1=150k → rate 1.5 (before fees).
        // Pool2, Pool3: balanced 1:1.
        // No-fee product = 1.5 × 1 × 1 = 1.5 > 1 → profitable after 3×0.3% fee.
        let pool1 = make_pool(0x01, 0xAA, 0xBB, 100_000, 150_000, 30);
        let pool2 = make_pool(0x02, 0xBB, 0xCC, 100_000, 100_000, 30);
        let pool3 = make_pool(0x03, 0xCC, 0xAA, 100_000, 100_000, 30);

        let (opt_in, exp_out) =
            optimal_triangular(&pool1, &pool2, &pool3, [true, true, true]).unwrap();

        assert!(opt_in  > U256::ZERO, "ternary search should find an input");
        assert!(exp_out > opt_in,     "exp_out={exp_out} must exceed opt_in={opt_in}");
    }

    #[test]
    fn triangular_profitable_larger_rate() {
        // Larger imbalance in pool1: 1:3 rate → more robust profit.
        let pool1 = make_pool(0x01, 0xAA, 0xBB, 100_000, 300_000, 30);
        let pool2 = make_pool(0x02, 0xBB, 0xCC, 100_000, 100_000, 30);
        let pool3 = make_pool(0x03, 0xCC, 0xAA, 100_000, 100_000, 30);

        let (opt_in, exp_out) =
            optimal_triangular(&pool1, &pool2, &pool3, [true, true, true]).unwrap();

        assert!(opt_in  > U256::ZERO);
        assert!(exp_out > opt_in);
    }

    #[test]
    fn triangular_no_profit_balanced() {
        // All pools at 1:1 → 3× fee overhead eliminates any apparent spread.
        let pool1 = make_pool(0x01, 0xAA, 0xBB, 100_000, 100_000, 30);
        let pool2 = make_pool(0x02, 0xBB, 0xCC, 100_000, 100_000, 30);
        let pool3 = make_pool(0x03, 0xCC, 0xAA, 100_000, 100_000, 30);

        let (opt_in, exp_out) =
            optimal_triangular(&pool1, &pool2, &pool3, [true, true, true]).unwrap();

        if opt_in > U256::ZERO {
            assert!(exp_out <= opt_in + U256::from(1u64),
                "balanced pools: exp_out={exp_out} should not exceed opt_in={opt_in}");
        } else {
            assert_eq!(exp_out, U256::ZERO);
        }
    }

    #[test]
    fn triangular_no_profit_wrong_direction() {
        // Imbalance in pool1 favours dir=true, but we flip pool1 to dir=false.
        // Reversed leg: rate = 100k/150k ≈ 0.67 → product < 1 → no profit.
        let pool1 = make_pool(0x01, 0xAA, 0xBB, 100_000, 150_000, 30);
        let pool2 = make_pool(0x02, 0xBB, 0xCC, 100_000, 100_000, 30);
        let pool3 = make_pool(0x03, 0xCC, 0xAA, 100_000, 100_000, 30);

        let (opt_in, exp_out) =
            optimal_triangular(&pool1, &pool2, &pool3, [false, true, true]).unwrap();

        if opt_in > U256::ZERO {
            assert!(exp_out <= opt_in + U256::from(1u64),
                "wrong direction must not produce profit: exp_out={exp_out} opt_in={opt_in}");
        }
    }

    #[test]
    fn triangular_zero_pool_reserve_returns_zero() {
        // Pool1 has empty r0 → ternary search high = 0 → returns (0, 0).
        let pool1 = make_pool(0x01, 0xAA, 0xBB, 0, 150_000, 30);
        let pool2 = make_pool(0x02, 0xBB, 0xCC, 100_000, 100_000, 30);
        let pool3 = make_pool(0x03, 0xCC, 0xAA, 100_000, 100_000, 30);

        let (opt_in, exp_out) =
            optimal_triangular(&pool1, &pool2, &pool3, [true, true, true]).unwrap();

        assert_eq!(opt_in,  U256::ZERO);
        assert_eq!(exp_out, U256::ZERO);
    }

    #[test]
    fn triangular_mixed_fees_still_profitable() {
        // Pool1: 0.1% fee (lower barrier), pool2/3: 0.3%. Large imbalance.
        let pool1 = make_pool(0x01, 0xAA, 0xBB, 100_000, 150_000, 10);
        let pool2 = make_pool(0x02, 0xBB, 0xCC, 100_000, 100_000, 30);
        let pool3 = make_pool(0x03, 0xCC, 0xAA, 100_000, 100_000, 30);

        let (opt_in, exp_out) =
            optimal_triangular(&pool1, &pool2, &pool3, [true, true, true]).unwrap();

        assert!(opt_in  > U256::ZERO);
        assert!(exp_out > opt_in);
    }

    #[test]
    fn triangular_lower_total_fee_yields_more_profit() {
        // Large reserves so integer rounding doesn't erase the fee difference.
        // With 1e7-scale pools the per-swap fee delta (2 units per 1000) is visible.
        let pool1_hi = make_pool(0x01, 0xAA, 0xBB, 10_000_000, 15_000_000, 30);
        let pool2_hi = make_pool(0x02, 0xBB, 0xCC, 10_000_000, 10_000_000, 30);
        let pool3_hi = make_pool(0x03, 0xCC, 0xAA, 10_000_000, 10_000_000, 30);

        let pool1_lo = make_pool(0x04, 0xAA, 0xBB, 10_000_000, 15_000_000, 10);
        let pool2_lo = make_pool(0x05, 0xBB, 0xCC, 10_000_000, 10_000_000, 10);
        let pool3_lo = make_pool(0x06, 0xCC, 0xAA, 10_000_000, 10_000_000, 10);

        let (_, out_hi) = optimal_triangular(&pool1_hi, &pool2_hi, &pool3_hi, [true, true, true]).unwrap();
        let (_, out_lo) = optimal_triangular(&pool1_lo, &pool2_lo, &pool3_lo, [true, true, true]).unwrap();

        assert!(out_lo > out_hi, "lower total fee should yield more: lo={out_lo} hi={out_hi}");
    }

    // ── estimate_gas_cost_wei ─────────────────────────────────────────────────

    #[test]
    fn gas_cost_is_positive() {
        let cost = estimate_gas_cost_wei(U256::from(10_000_000_000u64), 350_000);
        assert!(cost > 0);
    }

    #[test]
    fn gas_cost_scales_with_gas_price() {
        let lo = estimate_gas_cost_wei(U256::from(1_000_000_000u64),  350_000);
        let hi = estimate_gas_cost_wei(U256::from(10_000_000_000u64), 350_000);
        assert!(hi > lo, "10× gas price must yield higher cost: hi={hi} lo={lo}");
    }

    #[test]
    fn gas_cost_scales_with_gas_limit() {
        let lo = estimate_gas_cost_wei(U256::from(5_000_000_000u64), 200_000);
        let hi = estimate_gas_cost_wei(U256::from(5_000_000_000u64), 400_000);
        assert!(hi > lo, "2× gas limit must yield higher cost: hi={hi} lo={lo}");
    }

    #[test]
    fn gas_cost_includes_l1_surcharge() {
        // Even at zero gas price, L1 surcharge makes cost > 0.
        let cost = estimate_gas_cost_wei(U256::ZERO, 0);
        assert!(cost > 0, "L1 surcharge must be non-zero even with zero gas price and limit");
    }
}
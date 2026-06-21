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
    // k = φ_a * φ_b * r_a_out * r_b_out
    let k = u_fee_a
        .checked_mul(u_fee_b).ok_or(BotError::Overflow)?
        .checked_mul(r_a_out).ok_or(BotError::Overflow)?
        .checked_mul(r_b_out).ok_or(BotError::Overflow)?;

    // k_expanded = k * r_a_in * r_b_in
    let k_expanded = k
        .checked_mul(r_a_in).ok_or(BotError::Overflow)?
        .checked_mul(r_b_in).ok_or(BotError::Overflow)?;

    let sqrt_term = isqrt(k_expanded);
    
    // sub_term = r_a_in * 1000 * 1000
    let sub_term = r_a_in
        .checked_mul(U256::from(1000000u64)).ok_or(BotError::Overflow)?;

    // If sqrt_term <= sub_term, no arbitrage opportunity exists
    if sqrt_term <= sub_term {
        return Ok((U256::ZERO, U256::ZERO));
    }

    let num = sqrt_term.checked_sub(sub_term).ok_or(BotError::Overflow)?;
    
    // den = φ_a * 1000 (adjusting for integer scale constraints)
    let den = u_fee_a.checked_mul(U256::from(1000u64)).ok_or(BotError::Overflow)?;
    
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

    #[test]
    fn isqrt_known_values() {
        assert_eq!(isqrt(U256::from(0u64)),  U256::ZERO);
        assert_eq!(isqrt(U256::from(1u64)),  U256::from(1u64));
        assert_eq!(isqrt(U256::from(4u64)),  U256::from(2u64));
        assert_eq!(isqrt(U256::from(9u64)),  U256::from(3u64));
        assert_eq!(isqrt(U256::from(10u64)), U256::from(3u64));
        
        let n = U256::from(1_000_000_000_000_000_000u128); 
        let s = isqrt(n);
        assert!(s * s <= n);
        assert!((s + U256::from(1u64)) * (s + U256::from(1u64)) > n);
    }
}
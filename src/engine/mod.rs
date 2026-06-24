pub mod optimizer;

use std::collections::HashMap;
use std::sync::Arc;

use alloy::primitives::{Address, U256};
use tokio::sync::{mpsc, oneshot};
use tracing::{debug, info, warn};

use crate::config::BotConfig;
use crate::error::{BotError, Result};
use crate::feed::SequencerFeedTransaction;
use crate::pool::manager::LiquidityPoolManager;
use crate::pool::LiquidityPool;
use crate::types::{ArbitrageSignal, DexProtocol, SwapAction};

use optimizer::{estimate_gas_cost_wei, isqrt, optimal_v2_v2};

// ── Token-Pair Graph ──────────────────────────────────────────────────────────
//
// The engine maintains an adjacency map:
//   (sorted_token_pair) → Vec<pool_address>
//
// This allows O(1) lookup of counterparty pools when we know which token pair
// was imbalanced by the user's swap.

type TokenPair = (Address, Address);   // always sorted: token0 < token1

// ── ArbitrageEngine ───────────────────────────────────────────────────────────

pub struct ArbitrageEngine {
    config:        Arc<BotConfig>,
    lpm:           Arc<LiquidityPoolManager>,

    /// Adjacency index: sorted (tokenA, tokenB) → list of pool addresses.
    /// Built once at startup, read-only in the hot path.
    token_graph:   HashMap<TokenPair, Vec<Address>>,

    /// Sender half of the execution channel — sends signals to ExecutionManager.
    exec_tx:       mpsc::Sender<ArbitrageSignal>,
}

impl ArbitrageEngine {
    // ── Bootstrap ─────────────────────────────────────────────────────────────

    /// Full bootstrap sequence implementing the Catch-Up Buffer Strategy.
    ///
    /// Step 1: Start WebSocket feed listener → buffer to `feed_rx`.
    /// Step 2: Fetch RPC reserve snapshot → get checkpoint block / seq_num.
    /// Step 3: Reconcile buffer (discard stale, apply newer).
    /// Step 4: Spawn live evaluation loop.
    ///
    /// Returns once the engine is in live mode and the evaluation loop is running.
    /// `max_transactions`: if `Some(n)`, the live evaluation loop exits after
    /// processing `n` transactions. If `None` (production default), the loop
    /// runs indefinitely until the feed channel closes.
    ///
    /// Returns a `JoinHandle` for the live loop so the caller can `await` it
    /// in test mode to know when the requested number of transactions is done.
    pub async fn bootstrap(
        config: Arc<BotConfig>,
        lpm: Arc<LiquidityPoolManager>,
        exec_tx: mpsc::Sender<ArbitrageSignal>,
        max_transactions: Option<usize>,
    ) -> Result<tokio::task::JoinHandle<()>> {
        info!("ArbitrageEngine: starting bootstrap sequence");

        // ── Step 1: Open WebSocket feed → buffer everything ────────────────
        //
        // Channel capacity: 8,192 messages.  At peak Arbitrum throughput
        // (~4,000 tx/s) this gives ~2 seconds of buffer headroom, which
        // comfortably covers a typical RPC reserve-fetch round-trip (~200 ms).
        let (feed_tx, mut feed_rx) = mpsc::channel::<SequencerFeedTransaction>(8_192);

        // Build the HashSet of monitored routers for the feed listener.
        let monitored_routers: std::collections::HashSet<Address> =
            config.monitored_routers.iter().copied().collect();

        // Spawn feed listener — it runs independently and survives past bootstrap.
        let feed_url  = config.sequencer_feed_url.clone();
        let lpm_clone = Arc::clone(&lpm);
        let feed_tx_clone = feed_tx.clone();
        tokio::spawn(async move {
            crate::feed::run_feed_listener(
                feed_url,
                monitored_routers,
                lpm_clone,
                feed_tx_clone,
            )
            .await;
        });

        info!("Sequencer Feed listener spawned — buffering transactions");

        // ── Step 2: Fetch on-chain reserves (RPC checkpoint) ──────────────
        //
        // This call blocks until all pool reserves have been fetched from the
        // RPC node. The returned block number is our "checkpoint": any feed
        // transaction whose state changes are reflected in on-chain state at
        // or before this block is already encoded in our reserves and must
        // be discarded.
        let checkpoint_block = lpm.fetch_all_reserves().await?;
        info!(checkpoint_block, "RPC bootstrap complete");

        // ── Step 3: Reconcile the buffer ──────────────────────────────────
        //
        // Drain feed_rx. For each buffered transaction:
        //   - If seq_num <= checkpoint_block (rough approximation — in production
        //     correlate the feed sequence numbers to L2 block numbers more
        //     precisely using the ArbSys precompile or block-feed metadata):
        //       → Discard (state already reflected in our reserves).
        //   - Else:
        //       → Apply the simulated swap to our in-memory reserves so we
        //         start the live loop with a consistent state.
        //
        // We use `try_recv()` to drain without blocking — the feed_rx is not
        // empty (we've been buffering), but we don't want to wait for new
        // messages in this phase.
        let mut discarded = 0u64;
        let mut replayed  = 0u64;

        loop {
            match feed_rx.try_recv() {
                Ok(seq_tx) => {
                    // INJECT: Replace this approximation with a precise mapping
                    // of feed sequence numbers to L2 block numbers.
                    // The Arbitrum feed includes block_number in the message header
                    // (accessible via FeedMessage::message.message.header.block_number).
                    // Store that during parsing and compare against checkpoint_block.
                    if seq_tx.sequence_number <= checkpoint_block {
                        discarded += 1;
                        continue;
                    }

                    // Apply this buffered swap to the in-memory reserve state.
                    apply_swap_to_lpm(&seq_tx, &lpm).await;
                    replayed += 1;
                }
                Err(mpsc::error::TryRecvError::Empty) => break,
                Err(mpsc::error::TryRecvError::Disconnected) => {
                    return Err(BotError::Rpc("Feed channel disconnected during reconciliation".into()));
                }
            }
        }

        info!(discarded, replayed, "Buffer reconciliation complete — entering live mode");

        // ── Step 4: Build token-pair graph ────────────────────────────────
        let token_graph = build_token_graph(&lpm).await;
        info!(
            pair_count = token_graph.len(),
            "Token-pair graph built"
        );

        // ── Step 5: Spawn live evaluation loop ─────────────────────────────
        let engine = ArbitrageEngine {
            config,
            lpm,
            token_graph,
            exec_tx,
        };
        let handle = tokio::spawn(engine.run_live(feed_rx, max_transactions));

        Ok(handle)
    }

    // ── Live Evaluation Loop ───────────────────────────────────────────────────

    /// Hot path: receives arbitrable `SequencerFeedTransaction`s and evaluates
    /// them for profitability as fast as possible.
    ///
    /// Target latency: < 10 µs per arbitrable transaction.
    /// Non-arbitrable transactions are pre-filtered by the feed parser and
    /// never reach this function.
    async fn run_live(
        self,
        mut feed_rx: mpsc::Receiver<SequencerFeedTransaction>,
        max_transactions: Option<usize>,
    ) {
        info!("ArbitrageEngine live evaluation loop started");

        let mut count = 0usize;

        while let Some(seq_tx) = feed_rx.recv().await {
            // Only arbitrable transactions reach the engine — the feed parser
            // already checked is_arbitrable before sending.
            if let Err(e) = self.evaluate(seq_tx).await {
                // Log but never panic in the hot loop.
                debug!("Evaluation skipped: {e}");
            }

            count += 1;
            if let Some(max) = max_transactions {
                if count >= max {
                    info!(count, "Reached max_transactions limit — evaluation loop exiting");
                    return;
                }
            }
        }

        warn!("Feed channel closed — evaluation loop exiting");
    }

    /// Core evaluation pipeline for a single arbitrable transaction.
    ///
    /// Sub-steps (each must complete in microseconds):
    ///   1. Identify the imbalanced pool(s) from the swap path.
    ///   2. Simulate the user's swap locally → get post-swap reserves.
    ///   3. Query the token graph for counterparty pools.
    ///   4. For each candidate path, compute optimal input and expected profit.
    ///   5. Deduct gas. If profitable, emit ArbitrageSignal.
    ///   6. Update in-memory reserves (optimistically, regardless of profit).
    async fn evaluate(&self, seq_tx: SequencerFeedTransaction) -> Result<()> {
        let path = seq_tx
            .action
            .token_path_simple()
            .ok_or_else(|| BotError::NoPath(Address::ZERO, Address::ZERO))?;

        if path.len() < 2 {
            return Err(BotError::NoPath(Address::ZERO, Address::ZERO));
        }

        // ── 1. Identify Pool A (the pool imbalanced by the user's swap) ────
        let token_in  = path[0];
        let token_out = path[path.len() - 1];
        let pair_key  = sort_pair(token_in, token_out);

        let pool_a_address = self
            .find_matching_pool(token_in, token_out, &seq_tx.action)
            .ok_or_else(|| BotError::NoPath(token_in, token_out))?;

        let pool_a_ref = self
            .lpm
            .get(&pool_a_address)
            .ok_or_else(|| BotError::NoPath(token_in, token_out))?;

        // ── 2. Simulate the user's swap on Pool A ──────────────────────────
        //
        // Read Pool A's current state (pre-user-swap reserves).
        let (post_reserve0, post_reserve1) = {
            let pool_a = pool_a_ref.read().await;
            let amount_in = seq_tx.action.approx_amount_in();
            let zero_for_one = pool_a.token0_address == token_in;

            match pool_a.protocol {
                DexProtocol::UniswapV2 | DexProtocol::Camelot => {
                    let (_, r0_new, r1_new) =
                        pool_a.simulate_v2_swap_exact_in(amount_in, zero_for_one)?;
                    (r0_new, r1_new)
                }
                DexProtocol::UniswapV3 => {
                    let sqrt_limit = U256::ZERO; // no limit for simulation
                    let (_, sqrt_new, liq_new) =
                        pool_a.simulate_v3_swap_exact_in(amount_in, zero_for_one, sqrt_limit)?;
                    (sqrt_new, liq_new)
                }
                _ => return Err(BotError::NoPath(token_in, token_out)),
            }
        };

        // Snapshot the post-swap state for the optimizer (avoid holding the lock).
        let pool_a_snapshot = {
            let pool_a = pool_a_ref.read().await;
            let mut snap = pool_a.clone();
            snap.reserve0 = post_reserve0;
            snap.reserve1 = post_reserve1;
            snap
        };

        // ── 3. Find counterparty pools (Pool B) ───────────────────────────
        let candidates = self.token_graph.get(&pair_key).cloned().unwrap_or_default();

        let mut best_signal: Option<ArbitrageSignal> = None;
        let mut best_profit: U256 = U256::ZERO;

        for pool_b_address in candidates {
            if pool_b_address == pool_a_address {
                continue; // same pool — not an arbitrage
            }

            let Some(pool_b_ref) = self.lpm.get(&pool_b_address) else {
                continue;
            };
            let pool_b = pool_b_ref.read().await;

            // ── 4. Compute optimal input & expected profit ─────────────────
            //
            // Direction: our arbitrage buys the cheap token from Pool A
            // (which has too much of token_in) and sells it on Pool B.
            let our_zero_for_one = pool_a_snapshot.token0_address != token_in;

            // INJECT: Dispatch to the appropriate optimizer based on the
            // protocol combination (V2↔V2, V2↔V3, V3↔V3, etc.).
            // For now, we only handle V2↔V2.
            let (optimal_in, expected_out) = match (
                pool_a_snapshot.protocol,
                pool_b.protocol,
            ) {
                (DexProtocol::UniswapV2 | DexProtocol::Camelot,
                 DexProtocol::UniswapV2 | DexProtocol::Camelot) => {
                    optimal_v2_v2(&pool_a_snapshot, &pool_b, our_zero_for_one)?
                }
                // INJECT: Add V2↔V3, V3↔V3, triangular routes here.
                _ => continue,
            };

            let gross_profit = expected_out.saturating_sub(optimal_in);

            // ── 5. Deduct gas, apply profit threshold ──────────────────────
            //
            // Our tx's max_fee_per_gas is set to trigger_tx.max_fee_per_gas + 1
            // so the sequencer places us immediately after the trigger.
            let our_gas_price = seq_tx.max_fee_per_gas + U256::from(1u64);
            let gas_estimate  = estimate_gas_cost_wei(
                our_gas_price,
                350_000, // conservative gas limit for two-pool swap
            );

            if gross_profit.saturating_to::<u128>() <= gas_estimate {
                debug!(
                    pool_a = ?pool_a_address,
                    pool_b = ?pool_b_address,
                    "No profit after gas"
                );
                continue;
            }

            let net_profit_wei = gross_profit.saturating_to::<u128>() - gas_estimate;

            if net_profit_wei < self.config.min_profit_wei {
                continue;
            }

            if gross_profit > best_profit {
                best_profit = gross_profit;
                best_signal = Some(ArbitrageSignal {
                    trigger_sequence_number: seq_tx.sequence_number,
                    pool_a_address,
                    pool_b_address,
                    token_in,
                    token_out,
                    optimal_amount_in:       optimal_in,
                    expected_gross_profit:   gross_profit,
                    estimated_gas_cost_wei:  gas_estimate,
                    max_fee_per_gas:         our_gas_price,
                });
            }
        }

        // ── 6. Update in-memory reserves (optimistic, regardless of profit) ─
        //
        // Apply the user's swap to Pool A's in-memory state so future
        // evaluations see the updated price. This must happen even if we don't
        // fire — the user's tx will execute regardless.
        {
            let mut pool_a = pool_a_ref.write().await;
            pool_a.update(post_reserve0, post_reserve1, seq_tx.sequence_number);
        }

        // ── 7. Emit signal if profitable ────────────────────────────────────
        if let Some(signal) = best_signal {
            info!(
                seq = signal.trigger_sequence_number,
                profit_wei = signal.expected_gross_profit.to_string(),
                "Arbitrage opportunity found — signalling executor"
            );
            // Non-blocking send; if the executor is saturated, we skip.
            let _ = self.exec_tx.try_send(signal);
        }

        Ok(())
    }

    // ── Token Graph Helpers ───────────────────────────────────────────────────

    /// Find the specific pool address from our LPM that matches the swap path
    /// and protocol implied by the `SwapAction`.
    ///
    /// For V2 swaps, the first hop's pool is determined by the (path[0], path[1])
    /// pair. For V3 single-hop, the (tokenIn, tokenOut, fee) triple is used.
    fn find_matching_pool(
        &self,
        token_in: Address,
        token_out: Address,
        action: &SwapAction,
    ) -> Option<Address> {
        let pair = sort_pair(token_in, token_out);
        let candidates = self.token_graph.get(&pair)?;

        for &pool_addr in candidates {
            let Some(pool_ref) = self.lpm.get(&pool_addr) else { continue; };

            // Evaluate the match while holding the read guard, then drop the guard
            // before using `pool_ref` again — this avoids the lifetime overlap that
            // would occur if the guard (which borrows pool_ref) were alive at the
            // point where pool_ref is dropped by the loop.
            let matches = if let Ok(pool) = pool_ref.try_read() {
                match action {
                    SwapAction::ExactInputSingle { fee, .. }
                    | SwapAction::ExactOutputSingle { fee, .. } => pool.fee_tier == *fee,
                    _ => true,
                }
            } else {
                false // pool is locked; skip this candidate
            };
            // Guard is dropped here; pool_ref can now be safely released.
            if matches {
                return Some(pool_addr);
            }
        }
        None
    }
}

// ── Module-level helpers ──────────────────────────────────────────────────────

/// Sort a token pair so the map key is canonical (token_a < token_b).
#[inline]
fn sort_pair(a: Address, b: Address) -> TokenPair {
    if a < b { (a, b) } else { (b, a) }
}

/// Build the token-pair adjacency graph from the current LPM contents.
///
/// Runs once at bootstrap — not called in the hot path.
async fn build_token_graph(
    lpm: &LiquidityPoolManager,
) -> HashMap<TokenPair, Vec<Address>> {
    let mut graph: HashMap<TokenPair, Vec<Address>> = HashMap::new();

    for pool_ref in lpm.iter() {
        let pool = pool_ref.read().await;
        let key  = sort_pair(pool.token0_address, pool.token1_address);
        graph.entry(key).or_default().push(pool.pool_address);
    }

    graph
}

/// Apply a feed transaction's swap effect to the LPM in-memory state.
///
/// Used during bootstrap reconciliation to replay buffered transactions that
/// arrived while the RPC fetch was in flight.
async fn apply_swap_to_lpm(
    seq_tx: &SequencerFeedTransaction,
    lpm: &LiquidityPoolManager,
) {
    let Some(path) = seq_tx.action.token_path_simple() else {
        return;
    };
    if path.len() < 2 {
        return;
    }

    // For each consecutive pair in the path, find the pool and simulate.
    for window in path.windows(2) {
        let (t_in, t_out) = (window[0], window[1]);
        let pair = sort_pair(t_in, t_out);

        // INJECT: Use the token-pair index (when built) for O(1) lookup.
        for pool_ref in lpm.iter() {
            let matches = {
                let p = pool_ref.read().await;
                sort_pair(p.token0_address, p.token1_address) == pair
            };
            if !matches {
                continue;
            }

            let amount_in   = seq_tx.action.approx_amount_in();
            let zero_for_one;
            let (new_r0, new_r1);

            {
                let pool = pool_ref.read().await;
                zero_for_one = pool.token0_address == t_in;
                match pool.protocol {
                    DexProtocol::UniswapV2 | DexProtocol::Camelot => {
                        match pool.simulate_v2_swap_exact_in(amount_in, zero_for_one) {
                            Ok((_, r0, r1)) => { new_r0 = r0; new_r1 = r1; }
                            Err(_)          => continue,
                        }
                    }
                    DexProtocol::UniswapV3 => {
                        match pool.simulate_v3_swap_exact_in(amount_in, zero_for_one, U256::ZERO) {
                            Ok((_, r0, r1)) => { new_r0 = r0; new_r1 = r1; }
                            Err(_)          => continue,
                        }
                    }
                    _ => continue,
                }
            }

            let mut pool = pool_ref.write().await;
            pool.update(new_r0, new_r1, seq_tx.sequence_number);
            break; // only update the first matching pool per hop
        }
    }
}

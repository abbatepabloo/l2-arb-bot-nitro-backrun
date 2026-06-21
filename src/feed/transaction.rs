use alloy::consensus::TxEnvelope;
use alloy::eips::eip2718::Decodable2718;
use alloy::primitives::{Address, Bytes, TxHash, U256};
use alloy::sol;
use alloy::sol_types::SolCall;
use base64::engine::general_purpose::STANDARD as B64;
use base64::Engine as _;
use std::collections::HashSet;
use tracing::trace;

use crate::error::{BotError, Result};
use crate::feed::message::{FeedMessage, L2_MSG_TYPE_SIGNED_TX};
use crate::pool::manager::LiquidityPoolManager;
use crate::types::*;

// ── ABI interface declarations ─────────────────────────────────────────────────
//
// These are generated at compile-time by `sol!`. The macro produces zero-cost
// Rust types matching the Solidity ABI, with `abi_decode` / `abi_encode`
// methods available.

sol! {
    // ── Uniswap V2 Router02 / Camelot V2 ──────────────────────────────────────
    #[allow(missing_docs)]
    interface IUniswapV2Router {
        function swapExactTokensForTokens(
            uint256 amountIn,
            uint256 amountOutMin,
            address[] calldata path,
            address to,
            uint256 deadline
        ) external returns (uint256[] memory amounts);

        function swapTokensForExactTokens(
            uint256 amountOut,
            uint256 amountInMax,
            address[] calldata path,
            address to,
            uint256 deadline
        ) external returns (uint256[] memory amounts);

        function swapExactETHForTokens(
            uint256 amountOutMin,
            address[] calldata path,
            address to,
            uint256 deadline
        ) external payable returns (uint256[] memory amounts);

        function swapTokensForExactETH(
            uint256 amountOut,
            uint256 amountInMax,
            address[] calldata path,
            address to,
            uint256 deadline
        ) external returns (uint256[] memory amounts);
    }

    // ── Uniswap V3 SwapRouter / SwapRouter02 ──────────────────────────────────
    #[allow(missing_docs)]
    interface ISwapRouter {
        struct ExactInputSingleParams {
            address tokenIn;
            address tokenOut;
            uint24  fee;
            address recipient;
            uint256 deadline;
            uint256 amountIn;
            uint256 amountOutMinimum;
            uint160 sqrtPriceLimitX96;
        }
        struct ExactInputParams {
            bytes   path;
            address recipient;
            uint256 deadline;
            uint256 amountIn;
            uint256 amountOutMinimum;
        }
        struct ExactOutputSingleParams {
            address tokenIn;
            address tokenOut;
            uint24  fee;
            address recipient;
            uint256 deadline;
            uint256 amountOut;
            uint256 amountInMaximum;
            uint160 sqrtPriceLimitX96;
        }
        struct ExactOutputParams {
            bytes   path;
            address recipient;
            uint256 deadline;
            uint256 amountOut;
            uint256 amountInMaximum;
        }

        function exactInputSingle(ExactInputSingleParams calldata params)
            external payable returns (uint256 amountOut);
        function exactInput(ExactInputParams calldata params)
            external payable returns (uint256 amountOut);
        function exactOutputSingle(ExactOutputSingleParams calldata params)
            external payable returns (uint256 amountIn);
        function exactOutput(ExactOutputParams calldata params)
            external payable returns (uint256 amountIn);
    }
}

// ── SequencerFeedTransaction ──────────────────────────────────────────────────

/// A parsed, classified transaction from the Arbitrum Sequencer Feed.
///
/// Constructed from a raw `FeedMessage` by `SequencerFeedTransaction::parse`.
/// Fields are populated ONLY if `is_arbitrable == true`; callers should gate
/// on that flag before accessing `action`.
#[derive(Debug, Clone)]
pub struct SequencerFeedTransaction {
    /// Global L2 sequence number assigned by the sequencer.
    pub sequence_number: u64,

    /// `to` field of the EVM transaction — the DEX router being called.
    pub target_router: Address,

    /// EIP-1559 max_fee_per_gas (or gas_price for legacy txs).
    /// Used to price our response tx slightly higher.
    pub max_fee_per_gas: U256,

    /// Keccak256 hash of the signed transaction.
    pub tx_hash: TxHash,

    /// Decoded swap intent. Populated even if is_arbitrable is false
    /// (may be `SwapAction::Unknown`).
    pub action: SwapAction,

    /// True iff:
    ///   1. `target_router` matches a monitored DEX router address, AND
    ///   2. The token path in `action` contains at least one pool present
    ///      in the LiquidityPoolManager's in-memory registry.
    ///
    /// Only transactions where `is_arbitrable == true` enter the Engine's
    /// evaluation loop.
    pub is_arbitrable: bool,
}

impl SequencerFeedTransaction {
    /// Parse a raw `FeedMessage` into a `SequencerFeedTransaction`.
    ///
    /// This function runs on every message in the hot path. Allocations are
    /// limited to:
    ///   - One `Vec<u8>` for Base64 decode (avoidable with a pooled buffer).
    ///   - ABI-decoded path `Vec<Address>` (only when is_arbitrable).
    ///
    /// Returns `None` if the message is not a signed L2 transaction (e.g., it's
    /// a batch, a non-mutating call, or a different message type).
    pub fn parse(
        msg: &FeedMessage<'_>,
        monitored_routers: &HashSet<Address>,
        lpm: &LiquidityPoolManager,
    ) -> Option<Self> {
        let seq = msg.sequence_number;
        let l2_msg = msg.message.message.l2_msg;

        // ── 1. Base64 decode the l2Msg ─────────────────────────────────────
        let raw = B64.decode(l2_msg).ok()?;

        // The first byte is the L2MessageType.
        let (&msg_type, tx_bytes) = raw.split_first()?;
        if msg_type != L2_MSG_TYPE_SIGNED_TX {
            trace!(seq, msg_type, "Ignoring non-signed L2 message type");
            return None;
        }

        // ── 2. RLP-decode the EVM transaction ──────────────────────────────
        // alloy's TxEnvelope implements Decodable2718, handling both:
        //   - EIP-2718 typed envelopes (0x01, 0x02 prefix bytes)
        //   - Legacy transactions (RLP list, no prefix)
        let tx_envelope = TxEnvelope::decode_2718(&mut tx_bytes.as_ref()).ok()?;

        let tx_hash = *tx_envelope.tx_hash();

        // ── 3. Extract routing fields ──────────────────────────────────────
        let (to, calldata, max_fee_per_gas) = extract_tx_fields(&tx_envelope)?;

        let target_router = match to {
            Some(addr) => addr,
            None       => return None, // contract creation — not a DEX call
        };

        // ── 4. Filter: is this a monitored router? ─────────────────────────
        // Fast O(1) hash-set lookup. Unrecognised routers are dropped here
        // before any ABI decoding happens.
        if !monitored_routers.contains(&target_router) {
            return None;
        }

        // ── 5. ABI-decode the calldata to recover swap intent ──────────────
        let action = decode_swap_action(calldata, max_fee_per_gas);

        // ── 6. Check that at least one touched pool is in our registry ─────
        let is_arbitrable = action
            .token_path_simple()
            .map(|path| is_path_monitored(&path, lpm))
            .unwrap_or(false);

        Some(Self {
            sequence_number: seq,
            target_router,
            max_fee_per_gas,
            tx_hash,
            action,
            is_arbitrable,
        })
    }
}

// ── Internal helpers ──────────────────────────────────────────────────────────

/// Extract (to, calldata, max_fee_per_gas) from any `TxEnvelope` variant.
///
/// Returns `None` for transaction types we don't recognise / can't handle.
fn extract_tx_fields(
    envelope: &TxEnvelope,
) -> Option<(Option<Address>, &[u8], U256)> {
    match envelope {
        TxEnvelope::Legacy(signed) => {
            let tx = signed.tx();
            Some((
                tx.to.to().copied(),
                tx.input.as_ref(),
                U256::from(tx.gas_price),
            ))
        }
        TxEnvelope::Eip2930(signed) => {
            let tx = signed.tx();
            Some((
                tx.to.to().copied(),
                tx.input.as_ref(),
                U256::from(tx.gas_price),
            ))
        }
        TxEnvelope::Eip1559(signed) => {
            let tx = signed.tx();
            Some((
                tx.to.to().copied(),
                tx.input.as_ref(),
                U256::from(tx.max_fee_per_gas),
            ))
        }
        // EIP-4844 blob txs are not swap calls.
        _ => None,
    }
}

/// Attempt to ABI-decode the calldata against every known swap function selector.
///
/// The 4-byte function selector is compared first (O(1)) before any ABI
/// decoding occurs. Unknown selectors fast-path to `SwapAction::Unknown`.
fn decode_swap_action(calldata: &[u8], eth_value: U256) -> SwapAction {
    if calldata.len() < 4 {
        return SwapAction::Unknown;
    }

    let selector: [u8; 4] = calldata[..4].try_into().unwrap();
    let data = &calldata[4..];

    match selector {
        // ── Uniswap V2 / Camelot ───────────────────────────────────────────
        SEL_V2_SWAP_EXACT_TOKENS_FOR_TOKENS => {
            let Ok(d) = IUniswapV2Router::swapExactTokensForTokensCall::abi_decode(data)
            else { return SwapAction::Unknown; };
            SwapAction::SwapExactTokensForTokens {
                amount_in:      d.amountIn,
                amount_out_min: d.amountOutMin,
                path:           d.path,
                deadline:       d.deadline.saturating_to::<u64>(),
            }
        }
        SEL_V2_SWAP_TOKENS_FOR_EXACT_TOKENS => {
            let Ok(d) = IUniswapV2Router::swapTokensForExactTokensCall::abi_decode(data)
            else { return SwapAction::Unknown; };
            SwapAction::SwapTokensForExactTokens {
                amount_out:    d.amountOut,
                amount_in_max: d.amountInMax,
                path:          d.path,
                deadline:      d.deadline.saturating_to::<u64>(),
            }
        }
        SEL_V2_SWAP_EXACT_ETH_FOR_TOKENS => {
            let Ok(d) = IUniswapV2Router::swapExactETHForTokensCall::abi_decode(data)
            else { return SwapAction::Unknown; };
            SwapAction::SwapExactEthForTokens {
                amount_out_min: d.amountOutMin,
                path:           d.path,
                deadline:       d.deadline.saturating_to::<u64>(),
                eth_value,
            }
        }
        SEL_V2_SWAP_TOKENS_FOR_EXACT_ETH => {
            let Ok(d) = IUniswapV2Router::swapTokensForExactETHCall::abi_decode(data)
            else { return SwapAction::Unknown; };
            SwapAction::SwapTokensForExactEth {
                amount_out:    d.amountOut,
                amount_in_max: d.amountInMax,
                path:          d.path,
                deadline:      d.deadline.saturating_to::<u64>(),
            }
        }

        // ── Uniswap V3 ────────────────────────────────────────────────────
        SEL_V3_EXACT_INPUT_SINGLE => {
            let Ok(d) = ISwapRouter::exactInputSingleCall::abi_decode(data)
            else { return SwapAction::Unknown; };
            SwapAction::ExactInputSingle {
                token_in:             d.params.tokenIn,
                token_out:            d.params.tokenOut,
                fee:                  d.params.fee.to::<u32>(),
                amount_in:            d.params.amountIn,
                amount_out_minimum:   d.params.amountOutMinimum,
                sqrt_price_limit_x96: U256::from(d.params.sqrtPriceLimitX96),
            }
        }
        SEL_V3_EXACT_INPUT => {
            let Ok(d) = ISwapRouter::exactInputCall::abi_decode(data)
            else { return SwapAction::Unknown; };
            SwapAction::ExactInput {
                encoded_path:        d.params.path,
                amount_in:           d.params.amountIn,
                amount_out_minimum:  d.params.amountOutMinimum,
            }
        }
        SEL_V3_EXACT_OUTPUT_SINGLE => {
            let Ok(d) = ISwapRouter::exactOutputSingleCall::abi_decode(data)
            else { return SwapAction::Unknown; };
            SwapAction::ExactOutputSingle {
                token_in:             d.params.tokenIn,
                token_out:            d.params.tokenOut,
                fee:                  d.params.fee.to::<u32>(),
                amount_out:           d.params.amountOut,
                amount_in_maximum:    d.params.amountInMaximum,
                sqrt_price_limit_x96: U256::from(d.params.sqrtPriceLimitX96),
            }
        }
        SEL_V3_EXACT_OUTPUT => {
            let Ok(d) = ISwapRouter::exactOutputCall::abi_decode(data)
            else { return SwapAction::Unknown; };
            SwapAction::ExactOutput {
                encoded_path:       d.params.path,
                amount_out:         d.params.amountOut,
                amount_in_maximum:  d.params.amountInMaximum,
            }
        }

        _ => SwapAction::Unknown,
    }
}

/// Returns true if any adjacent pair in `path` corresponds to a known pool.
///
/// Example: path = [USDC, WETH, ARB]
///   → checks (USDC, WETH) pool and (WETH, ARB) pool in the LPM registry.
///
/// This is a read-only scan of the LPM DashMap — no lock acquisition.
fn is_path_monitored(path: &[Address], lpm: &LiquidityPoolManager) -> bool {
    // INJECT: A direct (token0, token1) → pool_address lookup index would
    // make this O(1) instead of O(pools). Add a secondary DashMap index in
    // LiquidityPoolManager keyed on sorted (Address, Address) pair if needed.

    // For now, iterate the LPM to find a matching token pair in the path.
    // This is fast enough when pool count < 10,000; for larger registries
    // build the token-pair index described above.
    for pool_ref in lpm.iter() {
        // Use try_read() to avoid blocking — skip locked pools in this tight loop.
        // The worst case is a missed classification, which the engine retries next tick.
        if let Ok(pool) = pool_ref.try_read() {
            let t0 = pool.token0_address;
            let t1 = pool.token1_address;
            for window in path.windows(2) {
                if (window[0] == t0 && window[1] == t1)
                    || (window[0] == t1 && window[1] == t0)
                {
                    return true;
                }
            }
        }
    }
    false
}

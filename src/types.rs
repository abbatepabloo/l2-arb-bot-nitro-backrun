use alloy::primitives::{Address, Bytes, U256};
use serde::{Deserialize, Serialize};

// ── Known 4-byte function selectors ──────────────────────────────────────────
// These are the EVM call selectors we know how to decode.
// Any calldata that doesn't match is classified as `SwapAction::Unknown`.

pub const SEL_V2_SWAP_EXACT_TOKENS_FOR_TOKENS: [u8; 4]  = [0x38, 0xed, 0x17, 0x39];
pub const SEL_V2_SWAP_TOKENS_FOR_EXACT_TOKENS: [u8; 4]  = [0x88, 0x03, 0xdb, 0xee];
pub const SEL_V2_SWAP_EXACT_ETH_FOR_TOKENS:    [u8; 4]  = [0x7f, 0xf3, 0x6a, 0xb5];
pub const SEL_V2_SWAP_TOKENS_FOR_EXACT_ETH:    [u8; 4]  = [0x40, 0x93, 0xf8, 0x1e];
pub const SEL_V3_EXACT_INPUT_SINGLE:           [u8; 4]  = [0x41, 0x4b, 0xf3, 0x89];
pub const SEL_V3_EXACT_INPUT:                  [u8; 4]  = [0xc0, 0x4b, 0x8d, 0x59];
pub const SEL_V3_EXACT_OUTPUT_SINGLE:          [u8; 4]  = [0xdb, 0x3e, 0x21, 0x98];
pub const SEL_V3_EXACT_OUTPUT:                 [u8; 4]  = [0xf2, 0x8c, 0x04, 0x98];

// ── DEX protocol classification ───────────────────────────────────────────────

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum DexProtocol {
    UniswapV2,
    UniswapV3,
    UniswapV4,
    Camelot,  // Arbitrum-native, V2-style with dual-fee mechanism
    Unknown,
}

impl DexProtocol {
    pub fn from_str(s: &str) -> Self {
        match s.to_lowercase().as_str() {
            "uniswap_v2" | "uniswapv2" => Self::UniswapV2,
            "uniswap_v3" | "uniswapv3" => Self::UniswapV3,
            "uniswap_v4" | "uniswapv4" => Self::UniswapV4,
            "camelot"                   => Self::Camelot,
            _                           => Self::Unknown,
        }
    }
}

// ── SwapAction: decoded intent extracted from sequencer feed calldata ─────────
//
// This enum represents the **parsed** form of a DEX swap call. The parser
// (feed::transaction) fills in the fields after ABI-decoding the calldata.
// The ArbitrageEngine uses these values to simulate the user's swap locally
// without touching the RPC node.

#[derive(Debug, Clone)]
pub enum SwapAction {
    // ── Uniswap V2 / Camelot V2 style ────────────────────────────────────────
    SwapExactTokensForTokens {
        amount_in:      U256,
        amount_out_min: U256,
        path:           Vec<Address>, // [tokenIn, ..., tokenOut]
        deadline:       u64,
    },
    SwapTokensForExactTokens {
        amount_out:     U256,
        amount_in_max:  U256,
        path:           Vec<Address>,
        deadline:       u64,
    },
    SwapExactEthForTokens {
        amount_out_min: U256,
        path:           Vec<Address>,
        deadline:       u64,
        eth_value:      U256, // from tx.value
    },
    SwapTokensForExactEth {
        amount_out:    U256,
        amount_in_max: U256,
        path:          Vec<Address>,
        deadline:      u64,
    },

    // ── Uniswap V3 style ─────────────────────────────────────────────────────
    ExactInputSingle {
        token_in:              Address,
        token_out:             Address,
        fee:                   u32,
        amount_in:             U256,
        amount_out_minimum:    U256,
        sqrt_price_limit_x96:  U256,
    },
    ExactInput {
        /// ABI-encoded V3 path: abi.encodePacked(token0, fee, token1, fee, token2, ...)
        /// Each hop is 20 bytes (address) + 3 bytes (fee uint24).
        encoded_path:         Bytes,
        amount_in:            U256,
        amount_out_minimum:   U256,
    },
    ExactOutputSingle {
        token_in:             Address,
        token_out:            Address,
        fee:                  u32,
        amount_out:           U256,
        amount_in_maximum:    U256,
        sqrt_price_limit_x96: U256,
    },
    ExactOutput {
        encoded_path:        Bytes,
        amount_out:          U256,
        amount_in_maximum:   U256,
    },

    /// Calldata matched a monitored router but did not match any known selector.
    Unknown,
}

impl SwapAction {
    /// Extract the list of token addresses involved in the swap.
    /// For V2-style, the path is explicit. For V3 single-hop,
    /// we synthesize a 2-element vec. For V3 multi-hop, the
    /// encoded path must be decoded separately.
    pub fn token_path_simple(&self) -> Option<Vec<Address>> {
        match self {
            Self::SwapExactTokensForTokens { path, .. }
            | Self::SwapTokensForExactTokens { path, .. }
            | Self::SwapExactEthForTokens { path, .. }
            | Self::SwapTokensForExactEth { path, .. } => Some(path.clone()),

            Self::ExactInputSingle { token_in, token_out, .. }
            | Self::ExactOutputSingle { token_in, token_out, .. } => {
                Some(vec![*token_in, *token_out])
            }

            Self::ExactInput { .. }
            | Self::ExactOutput { .. }
            | Self::Unknown => None,
        }
    }

    /// Gross amount flowing into the user's swap (upper-bound estimate for
    /// ExactOutput variants where we only know the maximum input).
    pub fn approx_amount_in(&self) -> U256 {
        match self {
            Self::SwapExactTokensForTokens { amount_in, .. }
            | Self::SwapExactEthForTokens { eth_value: amount_in, .. }
            | Self::ExactInputSingle { amount_in, .. }
            | Self::ExactInput { amount_in, .. } => *amount_in,

            Self::SwapTokensForExactTokens { amount_in_max, .. }
            | Self::SwapTokensForExactEth { amount_in_max, .. }
            | Self::ExactOutputSingle { amount_in_maximum: amount_in_max, .. }
            | Self::ExactOutput { amount_in_maximum: amount_in_max, .. } => *amount_in_max,

            Self::Unknown => U256::ZERO,
        }
    }
}

// ── ArbitrageSignal ───────────────────────────────────────────────────────────
//
// Produced by the ArbitrageEngine when a profitable opportunity is detected.
// Consumed by the ExecutionManager to construct and fire the on-chain call.

#[derive(Debug, Clone)]
pub struct ArbitrageSignal {
    /// Sequence number of the user transaction we are backrunning.
    pub trigger_sequence_number: u64,

    /// Pool that will be imbalanced by the user's swap.
    pub pool_a_address: Address,

    /// Counterparty pool on a different DEX we will trade through.
    pub pool_b_address: Address,

    /// Token flowing in to our executor contract.
    pub token_in: Address,

    /// Token flowing out of our executor contract (same as token_in for
    /// atomic round-trip arbitrage).
    pub token_out: Address,

    /// Optimal input amount calculated by the optimizer (in wei).
    pub optimal_amount_in: U256,

    /// Expected gross profit (in wei of token_out).
    pub expected_gross_profit: U256,

    /// Estimated L2 gas cost (in wei of ETH).
    pub estimated_gas_cost_wei: u128,

    /// max_fee_per_gas to use for our response transaction.
    /// Set slightly above the trigger tx to guarantee sequencer ordering.
    pub max_fee_per_gas: U256,
}

// ── ConditionalTransactionOptions ─────────────────────────────────────────────
//
// Payload for `eth_sendRawTransactionConditional` (Arbitrum sequencer RPC).
// If the on-chain state at execution time doesn't match these assertions,
// the sequencer drops the transaction for FREE (no gas wasted on revert).

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ConditionalOptions {
    /// The transaction will not be included before this L2 block number.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub block_number_min: Option<u64>,

    /// The transaction will be dropped if the current L2 block number exceeds
    /// this value — our primary freshness guard.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub block_number_max: Option<u64>,

    /// Asserted storage slot values. The sequencer checks these on the spot.
    /// Key:   account address hex string
    /// Value: map of storage slot hex → expected value hex
    ///
    /// INJECT: Encode the reserve slots of Pool A here so the tx is
    /// invalidated for free if another backrunner changes the pool before us.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub known_accounts: Option<std::collections::HashMap<String, KnownAccountCondition>>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct KnownAccountCondition {
    /// If present, assert the full storage root hash.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub storage_root: Option<String>,

    /// Individual slot assertions (more granular than storage_root).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub storage: Option<std::collections::HashMap<String, String>>,
}

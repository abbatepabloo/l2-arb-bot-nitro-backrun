use std::collections::HashMap;
use std::sync::Arc;

use alloy::consensus::{SignableTransaction, TxEip1559, TxEnvelope};
use alloy::eips::eip2718::Encodable2718;
use alloy::network::TxSignerSync;
use alloy::primitives::U256;
use alloy::providers::{Provider, ProviderBuilder};
use alloy::signers::local::PrivateKeySigner;
use alloy::sol;
use alloy::sol_types::SolCall;
use serde_json::json;
use tokio::sync::mpsc;
use tracing::{error, info, warn};

use crate::config::BotConfig;
use crate::error::{BotError, Result};
use crate::types::{ArbitrageSignal, ConditionalOptions, KnownAccountCondition};

// ── Executor Contract Interface ────────────────────────────────────────────────
//
// Our deployed Solidity contract receives the full arbitrage specification
// in a single call. The contract:
//   1. Borrows flash capital from a lender (e.g., Balancer, AAVE) if needed.
//   2. Calls Pool A → Pool B via the respective router contracts.
//   3. Repays the flash loan + premium.
//   4. Transfers profit to `recipient`.
//
// INJECT: Replace this stub with your actual contract ABI once deployed.
sol! {
    #[allow(missing_docs)]
    interface IArbitrageExecutor {
        struct ArbParams {
            address poolA;
            address poolB;
            address tokenIn;
            address tokenOut;
            uint256 amountIn;
            uint256 minProfit;       // slippage guard — revert if profit < this
            address recipient;
        }

        /// Entry point called by our bot. Executes the full arbitrage atomically.
        function execute(ArbParams calldata params) external;
    }
}

// ── Gas constants ─────────────────────────────────────────────────────────────

/// Conservative gas limit covering two DEX swaps + flash loan overhead.
const EXECUTION_GAS_LIMIT: u64 = 400_000;

// ── ExecutionManager ──────────────────────────────────────────────────────────

pub struct ExecutionManager {
    config:  Arc<BotConfig>,
    signer:  PrivateKeySigner,

    /// Nonce tracked locally to avoid async RPC round-trips in the hot path.
    /// Initialised from `eth_getTransactionCount` at startup and incremented
    /// monotonically. Reset if the RPC rejects with `nonce too low`.
    nonce:   std::sync::atomic::AtomicU64,
}

impl ExecutionManager {
    // ── Construction ──────────────────────────────────────────────────────────

    pub async fn new(config: Arc<BotConfig>) -> Result<Arc<Self>> {
        let signer = config
            .private_key_hex
            .parse::<PrivateKeySigner>()
            .map_err(|e| BotError::Signing(e.to_string()))?;

        // Fetch current nonce from the RPC node.
        let url = config.rpc_url.parse::<reqwest::Url>()
            .map_err(|e| BotError::Rpc(e.to_string()))?;
        let provider = ProviderBuilder::new().connect_http(url);
        let nonce = provider
            .get_transaction_count(signer.address())
            .await
            .map_err(|e| BotError::Rpc(e.to_string()))?;

        info!(address = ?signer.address(), nonce, "ExecutionManager ready");

        Ok(Arc::new(Self {
            config,
            signer,
            nonce: std::sync::atomic::AtomicU64::new(nonce),
        }))
    }

    // ── Main receive loop ─────────────────────────────────────────────────────

    /// Listens for `ArbitrageSignal`s from the engine and fires the execution
    /// transaction for each one.
    pub async fn run(self: Arc<Self>, mut signal_rx: mpsc::Receiver<ArbitrageSignal>) {
        info!("ExecutionManager listening for arbitrage signals…");

        while let Some(signal) = signal_rx.recv().await {
            let this = Arc::clone(&self);
            // Spawn so the next signal can be received immediately while the
            // RPC call for the current one is in flight.
            tokio::spawn(async move {
                if let Err(e) = this.fire(&signal).await {
                    error!(
                        seq = signal.trigger_sequence_number,
                        "Execution failed: {e}"
                    );
                }
            });
        }

        warn!("Signal channel closed — ExecutionManager exiting");
    }

    // ── Transaction Construction & Firing ─────────────────────────────────────

    /// Construct, sign, and submit the arbitrage transaction using
    /// `eth_sendRawTransactionConditional`.
    async fn fire(&self, signal: &ArbitrageSignal) -> Result<()> {
        // ── 1. Encode the executor contract calldata ───────────────────────
        let calldata = self.build_calldata(signal);

        // ── 2. Acquire and increment the local nonce ───────────────────────
        let nonce = self
            .nonce
            .fetch_add(1, std::sync::atomic::Ordering::SeqCst);

        // ── 3. Build EIP-1559 transaction ─────────────────────────────────
        let chain_id: u64 = self.config.chain_id;

        // TxEip1559 gas fields are u128 in alloy 2.x (not U256).
        let mut tx = TxEip1559 {
            chain_id,
            nonce,
            max_fee_per_gas:          signal.max_fee_per_gas.saturating_to::<u128>(),
            max_priority_fee_per_gas: 100_000_000_u128, // 0.1 gwei tip
            gas_limit:                EXECUTION_GAS_LIMIT,
            to:                       alloy::primitives::TxKind::Call(
                self.config.executor_contract_address
            ),
            value:                    U256::ZERO,
            input:                    calldata.into(),
            access_list:              Default::default(),
        };

        // ── 4. Sign the transaction ────────────────────────────────────────
        // alloy 2.x: sign_transaction_sync(&mut dyn SignableTransaction) → Signature
        // then tx.into_signed(sig) → Signed<TxEip1559>
        let sig = self
            .signer
            .sign_transaction_sync(&mut tx)
            .map_err(|e| BotError::Signing(e.to_string()))?;
        let signed_tx = tx.into_signed(sig);

        // Encode into raw EIP-2718 bytes for the RPC call.
        let mut raw_tx_bytes = Vec::with_capacity(256);
        TxEnvelope::Eip1559(signed_tx).encode_2718(&mut raw_tx_bytes);
        let raw_tx_hex = format!("0x{}", const_hex::encode(&raw_tx_bytes));

        // ── 5. Build conditional options ───────────────────────────────────
        let conditions = self.build_conditions(signal);

        // ── 6. Submit via eth_sendRawTransactionConditional ────────────────
        //
        // This Arbitrum-specific RPC method has the sequencer validate our
        // `conditions` assertions BEFORE including the tx. If the on-chain
        // state doesn't match, the tx is **dropped for free** (zero gas cost).
        // This is the key latency/gas-protection mechanism for backrunning.
        //
        // Reference: https://docs.arbitrum.io/build-decentralized-apps/
        //            nodeinterface/reference#eth_sendrawtransactionconditional
        let result = self
            .send_conditional(&raw_tx_hex, &conditions)
            .await;

        match result {
            Ok(tx_hash) => {
                info!(
                    seq = signal.trigger_sequence_number,
                    tx_hash = %tx_hash,
                    profit = signal.expected_gross_profit.to_string(),
                    "Arbitrage transaction submitted ✓"
                );
            }
            Err(BotError::ConditionalSend(ref msg)) if msg.contains("condition") => {
                // The sequencer rejected our conditional — state changed.
                // This is expected (another bot won the race). No gas wasted.
                info!(
                    seq = signal.trigger_sequence_number,
                    "Conditional tx rejected by sequencer (stale state) — no gas spent"
                );
                // Revert the nonce since this tx was not broadcast.
                self.nonce.fetch_sub(1, std::sync::atomic::Ordering::SeqCst);
            }
            Err(e) => return Err(e),
        }

        Ok(())
    }

    // ── Calldata Encoding ─────────────────────────────────────────────────────

    fn build_calldata(&self, signal: &ArbitrageSignal) -> Vec<u8> {
        // Encode IArbitrageExecutor::execute(ArbParams) using alloy sol! ABI.
        IArbitrageExecutor::executeCall {
            params: IArbitrageExecutor::ArbParams {
                poolA:     signal.pool_a_address,
                poolB:     signal.pool_b_address,
                tokenIn:   signal.token_in,
                tokenOut:  signal.token_out,
                amountIn:  signal.optimal_amount_in,
                // minProfit: set to 80% of expected to tolerate 20% slippage.
                // INJECT: Use a tighter bound in production to reduce MEV extraction.
                minProfit: signal.expected_gross_profit * U256::from(80u64)
                            / U256::from(100u64),
                recipient: self.signer.address(),
            },
        }
        .abi_encode()
    }

    // ── Conditional Options ───────────────────────────────────────────────────

    fn build_conditions(&self, signal: &ArbitrageSignal) -> ConditionalOptions {
        // ── block_number_max ───────────────────────────────────────────────
        //
        // The sequencer drops our tx for free if the L2 block number has
        // advanced beyond `block_number_max`. Set to current + 2 blocks
        // (~500 ms on Arbitrum) — enough for 1 retry, tight enough to avoid
        // trading on stale state.
        //
        // INJECT: Fetch the current L2 block number from the feed message
        // header and pass it here. For now use 0 as a placeholder.
        let block_number_max: Option<u64> = None; // INJECT

        // ── known_accounts (storage slot assertions) ───────────────────────
        //
        // Assert the reserve storage slots of Pool A match what we simulated.
        // If another transaction changes the pool's reserves between our
        // evaluation and sequencer inclusion, this condition fires and the
        // tx is dropped for free.
        //
        // Uniswap V2 reserves are packed into a single storage slot (slot 8):
        //   [blockTimestampLast (32 bits)] [reserve1 (112 bits)] [reserve0 (112 bits)]
        //
        // INJECT: Compute the expected packed slot value from
        //   signal.pool_a reserves and build the `storage` map below.
        //
        // Example (pseudocode):
        //   let packed = (reserve1 << 112) | reserve0;
        //   let slot_value = format!("0x{:064x}", packed);
        //   storage.insert("0x0000000000000000000000000000000000000008".to_string(), slot_value);

        let known_accounts: Option<HashMap<String, KnownAccountCondition>> = None; // INJECT

        ConditionalOptions {
            block_number_min: None,
            block_number_max,
            known_accounts,
        }
    }

    // ── RPC Call ──────────────────────────────────────────────────────────────

    async fn send_conditional(
        &self,
        raw_tx_hex: &str,
        conditions: &ConditionalOptions,
    ) -> Result<String> {
        // Use reqwest directly for the conditional RPC — the standard alloy
        // Provider does not natively expose this Arbitrum-specific method.
        //
        // INJECT: Replace with a typed alloy extension trait if the alloy
        // `ArbitrumLayer` or transport extension lands in a future release.
        let body = json!({
            "jsonrpc": "2.0",
            "method":  "eth_sendRawTransactionConditional",
            "params":  [raw_tx_hex, conditions],
            "id":      1
        });

        let client = reqwest::Client::new();
        let resp = client
            .post(&self.config.direct_sequencer_rpc_url)
            .json(&body)
            .send()
            .await
            .map_err(|e| BotError::Rpc(e.to_string()))?;

        let json: serde_json::Value = resp
            .json()
            .await
            .map_err(|e| BotError::Rpc(e.to_string()))?;

        if let Some(err) = json.get("error") {
            return Err(BotError::ConditionalSend(err.to_string()));
        }

        let tx_hash = json["result"]
            .as_str()
            .ok_or_else(|| BotError::ConditionalSend("No result in response".into()))?
            .to_string();

        Ok(tx_hash)
    }
}

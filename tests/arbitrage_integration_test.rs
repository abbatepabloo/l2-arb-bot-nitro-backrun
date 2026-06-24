/// Integration test: end-to-end arbitrage detection and execution on local Anvil.
///
/// # Prerequisites
///
/// 1. Anvil running at `http://127.0.0.1:8545` with chain-id 31337 (default).
///    Quick-start:
///      docker run --rm -p 8545:8545 ghcr.io/foundry-rs/foundry \
///        anvil --chain-id 31337 --host 0.0.0.0
///
/// 2. `solc` ≥ 0.8.20 on PATH, **or** Foundry's `forge` on PATH.
///    The test writes the contracts to tests/contracts/ and compiles them
///    automatically; see `compile_contract()` for details.
///    • solc:  https://docs.soliditylang.org/en/latest/installing-solidity.html
///    • forge: https://getfoundry.sh
///
/// # Scenario
///
///   • Deploy two ERC-20 tokens (TKA, TKB) and two V2 pairs (Pool A, Pool B).
///   • Pool A is seeded with imbalanced reserves: lots of TKA, little TKB.
///     This simulates the aftermath of a whale selling TKA into the pool.
///   • Pool B is seeded with fair (balanced) reserves.
///   • A mock WebSocket server replays the Arbitrum sequencer-feed format,
///     sending the whale's swap transaction to the bot.
///   • A mock HTTP proxy translates `eth_sendRawTransactionConditional`
///     (Arbitrum-specific) → `eth_sendRawTransaction` so Anvil can mine it.
///   • After `run(config, Some(1))` returns, we assert the bot's recipient
///     address gained TKB — proving the full arbitrage loop succeeded.

use std::net::SocketAddr;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use alloy::consensus::{SignableTransaction, TxEip1559, TxEnvelope};
use alloy::eips::eip2718::Encodable2718;
use alloy::network::TxSignerSync;
use alloy::primitives::{Address, TxKind, U256};
use alloy::providers::{Provider, ProviderBuilder};
use alloy::rpc::types::eth::TransactionRequest;
use alloy::signers::local::PrivateKeySigner;
use alloy::network::TransactionBuilder;
use alloy::sol;
use alloy::sol_types::{SolCall, SolValue};
use base64::engine::general_purpose::STANDARD as B64;
use base64::Engine;
use futures_util::{SinkExt, StreamExt};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpListener;
use tokio::sync::{mpsc, oneshot};
use tokio_tungstenite::accept_async;
use tokio_tungstenite::tungstenite::Message;

use nitro_backrun::config::BotConfig;

// ── Anvil well-known accounts ─────────────────────────────────────────────────

const BOT_PRIVATE_KEY: &str =
    "ac0974bec39a17e36ba4a6b4d238ff944bacb478cbed5efcae784d7bf4f2ff80";
const BOT_ADDRESS_HEX: &str = "f39Fd6e51aad88F6F4ce6aB8827279cffFb92266";

// Account 1 — used as the "whale" in the crafted feed message.
const WHALE_PRIVATE_KEY: &str =
    "59c6995e998f97a5a0044966f0945389dc9e86dae88c7a8412f4603b6b78690d";
const WHALE_ADDRESS_HEX: &str = "70997970C51812dc3A010C7d01b50e0d17dc79C8";

const ANVIL_RPC:   &str = "http://127.0.0.1:8545";
const ANVIL_CHAIN: u64  = 31337;

// ── Pool scenario ─────────────────────────────────────────────────────────────
//
// Pool A (imbalanced — whale has already dumped TKA here):
//   reserve TKA = 600 000  ← pool holds far too much TKA  → TKA is cheap
//   reserve TKB =  80 000
//   implied price: 1 TKA ≈ 0.133 TKB
//
// Pool B (balanced — reference pool):
//   reserve TKA = 100 000
//   reserve TKB = 200 000
//   implied price: 1 TKA = 2 TKB
//
// Arbitrage: buy cheap TKA from Pool A (spend TKB), sell on Pool B (get TKB back).

const E18: u128 = 1_000_000_000_000_000_000;

const POOL_A_TKA: u128 = 600_000 * E18;
const POOL_A_TKB: u128 =  80_000 * E18;
const POOL_B_TKA: u128 = 100_000 * E18;
const POOL_B_TKB: u128 = 200_000 * E18;

// Working capital minted directly into the executor contract.
const EXECUTOR_TKB_SEED: u128 = 50_000 * E18;

// The small whale swap that triggers the bot's evaluation pipeline.
// (Pool A is already imbalanced; we just need a valid feed message.)
const WHALE_SWAP_TKA: u128 = 500 * E18; // 500 TKA

// ── Solidity interface stubs (ABI-only; no bytecode needed) ───────────────────

sol! {
    interface IMockERC20 {
        function mint(address to, uint256 amount) external;
        function balanceOf(address owner) external view returns (uint256);
        function transfer(address to, uint256 amount) external returns (bool);
        function approve(address spender, uint256 amount) external returns (bool);
    }

    interface IMockV2Pair {
        function initialize(address token0, address token1) external;
        function sync() external;
        function token0() external view returns (address t0);
        function token1() external view returns (address t1);
        function getReserves()
            external view
            returns (uint112 reserve0, uint112 reserve1, uint32 ts);
    }

    interface IUniswapV2Router {
        function swapExactTokensForTokens(
            uint256          amountIn,
            uint256          amountOutMin,
            address[] calldata path,
            address          to,
            uint256          deadline
        ) external returns (uint256[] memory amounts);
    }
}

// ── Contract handles ──────────────────────────────────────────────────────────

struct Contracts {
    token_a:     Address,
    token_b:     Address,
    pool_a:      Address,
    pool_b:      Address,
    executor:    Address,
    pool_token0: Address,  // canonical token0 (lower address, matches what pair stores)
    pool_token1: Address,
    mock_router: Address,  // sentinel monitored address; whale's tx is `to = mock_router`
}

// ─────────────────────────────────────────────────────────────────────────────
//  Helper: compile a Solidity contract
// ─────────────────────────────────────────────────────────────────────────────

/// Compile `sol_file` (under `tests/contracts/`) and return the creation bytecode.
/// Tries `solc` first; falls back to Foundry's `forge build`.
/// Panics with instructions if neither is available.
fn compile_contract(sol_file: &str, contract_name: &str) -> Vec<u8> {
    let contracts_dir = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("tests")
        .join("contracts");
    let sol_path = contracts_dir.join(sol_file);

    // ── Attempt 1: solc ───────────────────────────────────────────────────────
    let solc_result = std::process::Command::new("solc")
        .args([
            "--bin", "--optimize", "--optimize-runs", "200",
            "--overwrite", "-o", contracts_dir.to_str().unwrap(),
            sol_path.to_str().unwrap(),
        ])
        .output();

    if let Ok(out) = solc_result {
        if out.status.success() {
            let bin_path = contracts_dir.join(format!("{contract_name}.bin"));
            if let Ok(hex) = std::fs::read_to_string(&bin_path) {
                return const_hex::decode(hex.trim())
                    .unwrap_or_else(|e| panic!("solc hex decode for {contract_name}: {e}"));
            }
        }
    }

    // ── Attempt 2: forge build ────────────────────────────────────────────────
    let out_dir = contracts_dir.join("out");
    let forge_result = std::process::Command::new("forge")
        .args([
            "build",
            "--root",    contracts_dir.to_str().unwrap(),
            "--contracts", contracts_dir.to_str().unwrap(),
            "--out",     out_dir.to_str().unwrap(),
        ])
        .output();

    if let Ok(out) = forge_result {
        if out.status.success() {
            // Foundry artifact path: out/<Contract>.sol/<Contract>.json
            let artifact = out_dir
                .join(format!("{}.sol", sol_file.trim_end_matches(".sol")))
                .join(format!("{contract_name}.json"));
            if let Ok(json_bytes) = std::fs::read(&artifact) {
                let json: serde_json::Value = serde_json::from_slice(&json_bytes).unwrap();
                if let Some(hex) = json["bytecode"]["object"].as_str() {
                    let hex = hex.trim_start_matches("0x");
                    return const_hex::decode(hex)
                        .unwrap_or_else(|e| panic!("forge hex decode for {contract_name}: {e}"));
                }
            }
        }
    }

    panic!(
        "Cannot compile {sol_file}: neither `solc` nor `forge` is on PATH.\n\
         Install one:\n\
         • solc:  https://docs.soliditylang.org/en/latest/installing-solidity.html\n\
         • forge: https://getfoundry.sh"
    );
}

// ─────────────────────────────────────────────────────────────────────────────
//  Helpers: deploy & interact with Anvil contracts
// ─────────────────────────────────────────────────────────────────────────────

async fn deploy_contract(provider: &impl Provider, creation_code: Vec<u8>) -> Address {
    provider
        .send_transaction(TransactionRequest::default().with_deploy_code(creation_code))
        .await
        .expect("deploy tx rejected")
        .get_receipt()
        .await
        .expect("deploy receipt failed")
        .contract_address
        .expect("no contract_address in receipt")
}

async fn call_view(provider: &impl Provider, to: Address, calldata: Vec<u8>) -> Vec<u8> {
    provider
        .call(TransactionRequest::default().to(to).input(calldata.into()))
        .await
        .expect("eth_call failed")
        .to_vec()
}

async fn send_transaction(provider: &impl Provider, to: Address, calldata: Vec<u8>) {
    provider
        .send_transaction(
            TransactionRequest::default().to(to).input(calldata.into()),
        )
        .await
        .expect("send_transaction failed")
        .get_receipt()
        .await
        .expect("tx receipt failed");
}

// ─────────────────────────────────────────────────────────────────────────────
//  Helpers: Anvil cheat-codes (raw JSON-RPC)
// ─────────────────────────────────────────────────────────────────────────────

async fn anvil_rpc(method: &str, params: serde_json::Value) -> serde_json::Value {
    reqwest::Client::new()
        .post(ANVIL_RPC)
        .json(&serde_json::json!({
            "jsonrpc": "2.0",
            "method":  method,
            "params":  params,
            "id":      1
        }))
        .send()
        .await
        .expect("anvil RPC send failed")
        .json::<serde_json::Value>()
        .await
        .expect("anvil RPC parse failed")
}

/// `anvil_impersonateAccount` — allow unsigned txs from any address.
async fn impersonate(addr: Address) {
    anvil_rpc("anvil_impersonateAccount", serde_json::json!([format!("{addr:#x}")])).await;
}

/// `anvil_stopImpersonatingAccount`
async fn stop_impersonate(addr: Address) {
    anvil_rpc("anvil_stopImpersonatingAccount", serde_json::json!([format!("{addr:#x}")])).await;
}

/// `anvil_setBalance` — give an address ETH for gas.
async fn set_balance(addr: Address, wei: U256) {
    anvil_rpc(
        "anvil_setBalance",
        serde_json::json!([format!("{addr:#x}"), format!("{wei:#x}")]),
    )
    .await;
}

// ─────────────────────────────────────────────────────────────────────────────
//  Helper: populate the SQLite pool database
// ─────────────────────────────────────────────────────────────────────────────

fn setup_pool_db(c: &Contracts, db_path: &str) {
    use rusqlite::Connection;

    let conn = Connection::open(db_path).expect("open db");
    conn.execute_batch(
        "CREATE TABLE IF NOT EXISTS liquidity_pools (
            pool_address     TEXT    NOT NULL,
            dex_name         TEXT    NOT NULL,
            token0_address   TEXT    NOT NULL,
            token1_address   TEXT    NOT NULL,
            token0_decimals  INTEGER NOT NULL,
            token1_decimals  INTEGER NOT NULL,
            token0_symbol    TEXT    NOT NULL,
            token1_symbol    TEXT    NOT NULL,
            fee_tier         INTEGER NOT NULL,
            tick_spacing     INTEGER NOT NULL,
            enabled          INTEGER NOT NULL
        )",
    )
    .unwrap();

    let (sym0, sym1) = if c.pool_token0 == c.token_a {
        ("TKA", "TKB")
    } else {
        ("TKB", "TKA")
    };

    for pool_addr in [c.pool_a, c.pool_b] {
        conn.execute(
            "INSERT INTO liquidity_pools VALUES \
             (?1,?2,?3,?4,?5,?6,?7,?8,?9,?10,?11)",
            rusqlite::params![
                format!("{pool_addr:#x}"),
                "uniswap_v2",                          // → DexProtocol::UniswapV2
                format!("{:#x}", c.pool_token0),
                format!("{:#x}", c.pool_token1),
                18i64,
                18i64,
                sym0,
                sym1,
                3000i64,   // 0.30 % fee
                0i64,      // tick_spacing (V2 = 0)
                1i64,      // enabled
            ],
        )
        .unwrap();
    }
}

// ─────────────────────────────────────────────────────────────────────────────
//  Helper: mock sequencer-feed WebSocket server
// ─────────────────────────────────────────────────────────────────────────────

/// Spawns a minimal WS server.
/// Returns:
///   `addr`     — bind address for the `sequencer_feed_url` config field
///   `msg_tx`   — push JSON strings here to deliver them to the connected bot
///   `ready_rx` — fires once the bot has established the WS connection
async fn spawn_mock_feed() -> (SocketAddr, mpsc::Sender<String>, oneshot::Receiver<()>) {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr     = listener.local_addr().unwrap();
    let (msg_tx, mut msg_rx) = mpsc::channel::<String>(8);
    let (ready_tx, ready_rx) = oneshot::channel::<()>();

    tokio::spawn(async move {
        // Accept exactly one connection.
        let (tcp_stream, _peer) = listener.accept().await.unwrap();
        let ws = accept_async(tcp_stream).await.unwrap();
        let (mut ws_tx, _ws_rx) = ws.split();

        let _ = ready_tx.send(());  // signal: bot is connected

        while let Some(json) = msg_rx.recv().await {
            ws_tx
                .send(Message::Text(json.into()))
                .await
                .expect("WS send failed");
        }
    });

    (addr, msg_tx, ready_rx)
}

// ─────────────────────────────────────────────────────────────────────────────
//  Helper: mock conditional-RPC proxy
// ─────────────────────────────────────────────────────────────────────────────

/// Minimal HTTP proxy that translates:
///   `eth_sendRawTransactionConditional` → `eth_sendRawTransaction`
///
/// All other JSON-RPC requests are forwarded verbatim to Anvil.
/// This lets the standard bot execution code run unchanged against a local node.
async fn spawn_conditional_rpc_proxy() -> SocketAddr {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr     = listener.local_addr().unwrap();

    tokio::spawn(async move {
        loop {
            let (mut stream, _) = match listener.accept().await {
                Ok(v)  => v,
                Err(_) => continue,
            };

            tokio::spawn(async move {
                let mut buf = vec![0u8; 65_536];
                let n = stream.read(&mut buf).await.unwrap_or(0);
                if n == 0 { return; }

                let raw  = std::str::from_utf8(&buf[..n]).unwrap_or_default();
                let body = raw
                    .find("\r\n\r\n")
                    .map(|i| &raw[i + 4..])
                    .unwrap_or(raw)
                    .trim_end_matches('\0');

                let mut json: serde_json::Value =
                    serde_json::from_str(body).unwrap_or_default();

                // ── Translate Arbitrum-specific method ─────────────────────
                // `eth_sendRawTransactionConditional` carries an extra
                // "conditions" param that Anvil does not understand.
                // We strip it and forward as a plain send.
                if json["method"] == "eth_sendRawTransactionConditional" {
                    json["method"] = serde_json::json!("eth_sendRawTransaction");
                    let raw_tx = json["params"][0].clone();
                    json["params"] = serde_json::json!([raw_tx]);
                }

                // ── Forward to Anvil ───────────────────────────────────────
                let resp = reqwest::Client::new()
                    .post(ANVIL_RPC)
                    .json(&json)
                    .send()
                    .await
                    .unwrap()
                    .text()
                    .await
                    .unwrap_or_else(|_| {
                        r#"{"jsonrpc":"2.0","error":"proxy error","id":1}"#.into()
                    });

                let http_resp = format!(
                    "HTTP/1.1 200 OK\r\n\
                     Content-Type: application/json\r\n\
                     Content-Length: {}\r\n\
                     Connection: close\r\n\r\n{}",
                    resp.len(),
                    resp
                );
                let _ = stream.write_all(http_resp.as_bytes()).await;
            });
        }
    });

    addr
}

// ─────────────────────────────────────────────────────────────────────────────
//  Helper: craft a valid Arbitrum sequencer-feed JSON frame
// ─────────────────────────────────────────────────────────────────────────────
//
// Wire format (src/feed/message.rs):
//
//  Top-level:
//  { "version":1, "messages":[{ "sequenceNumber":<u64>,
//      "message":{ "message":{ "header":{…}, "l2Msg":"<base64>" },
//                  "delayedMessagesRead":0 } }] }
//
//  l2Msg bytes: [L2MsgType=0x02] ++ [EIP-2718 TxEnvelope bytes]

fn craft_whale_feed_message(
    mock_router:     Address,
    token_in:        Address,   // token the whale is selling (TKA)
    token_out:       Address,   // token the whale is buying  (TKB)
    amount_in:       U256,
    sequence_number: u64,
) -> String {
    let whale_addr = Address::from_slice(
        &const_hex::decode(WHALE_ADDRESS_HEX).unwrap()
    );

    // Encode swapExactTokensForTokens calldata.
    // selector: keccak256("swapExactTokensForTokens(uint256,uint256,address[],address,uint256)")[0..4]
    //         = 0x38ed1739
    let selector = [0x38u8, 0xed, 0x17, 0x39];
    let body = (
        amount_in,
        U256::ZERO,                 // amountOutMin — no slippage protection in test
        vec![token_in, token_out],  // path
        whale_addr,                 // to (recipient of output tokens)
        U256::from(u64::MAX),       // deadline (far future)
    )
    .abi_encode();

    let mut calldata = Vec::with_capacity(4 + body.len());
    calldata.extend_from_slice(&selector);
    calldata.extend_from_slice(&body);

    // Sign with the whale's known test private key.
    let signer = WHALE_PRIVATE_KEY.parse::<PrivateKeySigner>().unwrap();
    let mut tx = TxEip1559 {
        chain_id:                 ANVIL_CHAIN,
        nonce:                    0,
        max_fee_per_gas:          1_000_000_000_u128, // 1 gwei
        max_priority_fee_per_gas: 100_000_000_u128,
        gas_limit:                300_000,
        to:                       TxKind::Call(mock_router),
        value:                    U256::ZERO,
        input:                    calldata.into(),
        access_list:              Default::default(),
    };
    let sig    = signer.sign_transaction_sync(&mut tx).expect("whale sign");
    let signed = tx.into_signed(sig);

    // EIP-2718 encode: [0x02 (type byte)] ++ RLP fields.
    let mut tx_bytes = Vec::with_capacity(256);
    TxEnvelope::Eip1559(signed).encode_2718(&mut tx_bytes);

    // Arbitrum L2MsgType = 0x02 (L2MessageType_SignedTx).
    // Reference: nitro/arbos/arbostypes/messagetype.go
    let mut l2_msg = vec![0x02u8];
    l2_msg.extend_from_slice(&tx_bytes);

    let l2_msg_b64 = B64.encode(&l2_msg);
    let timestamp  = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_secs();

    // blockNumber is kept small so the engine's seq_num > checkpoint_block
    // comparison classifies this message as "new" and sends it to the live loop.
    serde_json::json!({
        "version": 1,
        "messages": [{
            "sequenceNumber": sequence_number,
            "message": {
                "message": {
                    "header": {
                        "kind":        3,
                        "sender":      "0x1234567890123456789012345678901234567890",
                        "blockNumber": 1u64,
                        "timestamp":   timestamp,
                        "l1BaseFee":   "1000000000"
                    },
                    "l2Msg": l2_msg_b64
                },
                "delayedMessagesRead": 0u64
            }
        }]
    })
    .to_string()
}

// ─────────────────────────────────────────────────────────────────────────────
//  The integration test
// ─────────────────────────────────────────────────────────────────────────────

#[tokio::test]
async fn test_arbitrage_detected_and_executed() -> Result<(), Box<dyn std::error::Error>> {
    // ── 0. Tracing ────────────────────────────────────────────────────────────
    let _ = tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| "warn".into()),
        )
        .try_init();

    // ── 1. Verify Anvil is reachable ──────────────────────────────────────────
    let provider = ProviderBuilder::new()
        .wallet(alloy::network::EthereumWallet::new(
            BOT_PRIVATE_KEY.parse::<PrivateKeySigner>().unwrap(),
        ))
        .connect_http(ANVIL_RPC.parse().unwrap());

    let block_num = provider.get_block_number().await.expect(
        "Cannot reach Anvil at http://127.0.0.1:8545.\n\
         Start it with:\n\
         docker run --rm -p 8545:8545 ghcr.io/foundry-rs/foundry \\\n\
             anvil --chain-id 31337 --host 0.0.0.0",
    );
    println!("[test] Anvil reachable — current block: {block_num}");

    // ── 2. Compile Solidity test contracts ────────────────────────────────────
    println!("[test] Compiling contracts (needs solc or forge)…");
    let erc20_bin = compile_contract("MockERC20.sol",         "MockERC20");
    let pair_bin  = compile_contract("MockUniswapV2Pair.sol", "MockUniswapV2Pair");
    let exec_bin  = compile_contract("ArbitrageExecutor.sol", "ArbitrageExecutor");
    println!("[test] Compilation OK.");

    // ── 3. Deploy ERC-20 tokens ───────────────────────────────────────────────
    // Constructor: (string name, string symbol, uint8 decimals).
    // ABI-encoding: uint8 pads to 32 bytes exactly like uint256, so U256 works.
    let erc20_ctor = |name: &str, sym: &str| {
        let mut code = erc20_bin.clone();
        code.extend_from_slice(
            &(name.to_string(), sym.to_string(), U256::from(18u8)).abi_encode(),
        );
        code
    };

    let token_a = deploy_contract(&provider, erc20_ctor("Token A", "TKA")).await;
    let token_b = deploy_contract(&provider, erc20_ctor("Token B", "TKB")).await;
    println!("[test] TKA: {token_a:#x}");
    println!("[test] TKB: {token_b:#x}");

    // ── 4. Deploy two V2 pairs ────────────────────────────────────────────────
    let pool_a = deploy_contract(&provider, pair_bin.clone()).await;
    let pool_b = deploy_contract(&provider, pair_bin).await;
    println!("[test] Pool A: {pool_a:#x}");
    println!("[test] Pool B: {pool_b:#x}");

    // Initialize pairs — the contract sorts token0 < token1 internally.
    for pool in [pool_a, pool_b] {
        send_transaction(
            &provider, pool,
            IMockV2Pair::initializeCall { token0: token_a, token1: token_b }.abi_encode(),
        ).await;
    }

    // Determine canonical ordering (pair contract sorts by address).
    let (pool_token0, pool_token1) =
        if token_a < token_b { (token_a, token_b) } else { (token_b, token_a) };
    println!("[test] Pool token0: {pool_token0:#x}  token1: {pool_token1:#x}");

    // ── 5. Deploy ArbitrageExecutor ───────────────────────────────────────────
    let executor = deploy_contract(&provider, exec_bin).await;
    println!("[test] Executor: {executor:#x}");

    // ── 6. Seed pool liquidity ────────────────────────────────────────────────
    //
    // Mint the target amounts directly into the pair contracts, then call
    // `sync()` so the contracts record these as their official reserves.

    // Pool A: TKA-heavy (cheap TKA, expensive TKB).
    send_transaction(&provider, token_a,
        IMockERC20::mintCall { to: pool_a, amount: U256::from(POOL_A_TKA) }.abi_encode(),
    ).await;
    send_transaction(&provider, token_b,
        IMockERC20::mintCall { to: pool_a, amount: U256::from(POOL_A_TKB) }.abi_encode(),
    ).await;
    send_transaction(&provider, pool_a, IMockV2Pair::syncCall {}.abi_encode()).await;

    // Pool B: balanced (fair-price reference).
    send_transaction(&provider, token_a,
        IMockERC20::mintCall { to: pool_b, amount: U256::from(POOL_B_TKA) }.abi_encode(),
    ).await;
    send_transaction(&provider, token_b,
        IMockERC20::mintCall { to: pool_b, amount: U256::from(POOL_B_TKB) }.abi_encode(),
    ).await;
    send_transaction(&provider, pool_b, IMockV2Pair::syncCall {}.abi_encode()).await;
    println!("[test] Pool liquidity seeded.");

    // ── 7. Pre-fund the executor with TKB ─────────────────────────────────────
    //
    // The executor uses its own capital (no flash loan in this test).
    // Minting TKB directly into the executor contract.
    send_transaction(&provider, token_b,
        IMockERC20::mintCall { to: executor, amount: U256::from(EXECUTOR_TKB_SEED) }.abi_encode(),
    ).await;
    println!("[test] Executor seeded with {EXECUTOR_TKB_SEED} TKB wei.");

    // ── 8. Record initial TKB balance of the bot's wallet ─────────────────────
    let bot_addr = Address::from_slice(&const_hex::decode(BOT_ADDRESS_HEX).unwrap());

    let balance_raw = call_view(
        &provider, token_b,
        IMockERC20::balanceOfCall { owner: bot_addr }.abi_encode(),
    ).await;
    // alloy returns a single `uint256` return value directly as `U256`.
    let initial_tkb: U256 = IMockERC20::balanceOfCall::abi_decode_returns(&balance_raw)
        .expect("decode balanceOf");
    println!("[test] Bot initial TKB: {initial_tkb}");

    // ── 9. Anvil cheat-codes: set up whale impersonation ─────────────────────
    //
    // We use `anvil_impersonateAccount` to demonstrate the cheat-code pattern.
    // In this test the whale's on-chain swap has already happened (Pool A is
    // pre-imbalanced), so we just need ETH for gas in case a subsequent tx is
    // needed.  The whale's swap is delivered only through the mock feed.
    let whale_addr = Address::from_slice(&const_hex::decode(WHALE_ADDRESS_HEX).unwrap());
    set_balance(whale_addr, U256::from(10 * E18)).await; // 10 ETH for gas
    impersonate(whale_addr).await;
    println!("[test] Whale {whale_addr:#x} impersonated via anvil_impersonateAccount.");
    // Release impersonation — no on-chain whale tx needed in this scenario.
    stop_impersonate(whale_addr).await;

    // ── 10. Spawn the mock sequencer-feed WebSocket server ────────────────────
    let (feed_addr, feed_tx, feed_ready) = spawn_mock_feed().await;
    let feed_url = format!("ws://{feed_addr}/feed");
    println!("[test] Mock feed listening at {feed_url}");

    // ── 11. Spawn the conditional-RPC proxy ───────────────────────────────────
    let proxy_addr = spawn_conditional_rpc_proxy().await;
    let proxy_url  = format!("http://{proxy_addr}");
    println!("[test] Conditional-RPC proxy at {proxy_url}");

    // ── 12. Create the SQLite pool database ───────────────────────────────────
    let db_dir  = tempfile::tempdir().unwrap();
    let db_path = db_dir.path().join("pools.db");
    let db_path_str = db_path.to_str().unwrap().to_string();

    // A sentinel address that appears in `monitored_routers`.
    // The whale's feed transaction has `to = mock_router` so the feed parser
    // classifies it as arbitrable.
    let mock_router = Address::from([0x12u8; 20]);

    let contracts = Contracts {
        token_a, token_b, pool_a, pool_b, executor,
        pool_token0, pool_token1, mock_router,
    };
    setup_pool_db(&contracts, &db_path_str);
    println!("[test] SQLite DB at {db_path_str}");

    // ── 13. Build bot config ──────────────────────────────────────────────────
    let config = Arc::new(BotConfig::new(
        ANVIL_RPC,                // rpc_url — bootstrap + execution
        feed_url,                 // sequencer_feed_url — mock WS
        proxy_url,                // direct_sequencer_rpc_url — translating proxy
        BOT_PRIVATE_KEY,          // private_key_hex
        executor,                 // executor_contract_address
        &db_path_str,             // db_path
        0u128,                    // min_profit_wei = 0 (accept any profit)
        vec![mock_router],        // monitored_routers
        ANVIL_CHAIN,              // chain_id = 31337 (Anvil default)
    ));

    // ── 14. Start the bot in a background task ────────────────────────────────
    //
    // `run(config, Some(1))` returns once the engine has processed exactly
    // one feed transaction — ideal for deterministic integration tests.
    let config_clone = Arc::clone(&config);
    let bot_task = tokio::spawn(async move {
        nitro_backrun::run(config_clone, Some(1))
            .await
            .expect("bot run() failed")
    });

    // ── 15. Wait for bot→feed connection, then deliver the whale tx ───────────
    //
    // `feed_ready` fires when the bot establishes the WS connection.
    // We add a short sleep so the bootstrap RPC calls complete and
    // the engine enters its live evaluation loop before we push the message.
    feed_ready
        .await
        .expect("bot never connected to mock feed (timeout?)");
    tokio::time::sleep(Duration::from_millis(700)).await;

    // Sequence number 10_000_000 is many orders of magnitude larger than
    // Anvil's block number, so the engine's reconciler classifies the message
    // as "newer than the checkpoint" and forwards it to the live loop.
    let whale_msg = craft_whale_feed_message(
        mock_router,
        token_a,                          // whale is selling TKA
        token_b,                          // whale is buying  TKB
        U256::from(WHALE_SWAP_TKA),
        10_000_000,                       // sequence_number >> block_number
    );
    feed_tx.send(whale_msg).await.expect("failed to push whale msg to mock feed");
    println!("[test] Whale swap message delivered to feed.");

    // ── 16. Wait for the bot to process the tx (max 15 s) ────────────────────
    tokio::time::timeout(Duration::from_secs(15), bot_task)
        .await
        .expect("bot timed out after 15 s")
        .expect("bot task panicked");
    println!("[test] Bot engine finished.");

    // The ExecutionManager fires the arb tx in a separate spawned task.
    // Give it time to submit to the proxy, which forwards to Anvil and mines.
    tokio::time::sleep(Duration::from_millis(1_000)).await;

    // ── 17. Assert: bot wallet gained TKB ────────────────────────────────────
    let balance_raw = call_view(
        &provider, token_b,
        IMockERC20::balanceOfCall { owner: bot_addr }.abi_encode(),
    ).await;
    let final_tkb: U256 = IMockERC20::balanceOfCall::abi_decode_returns(&balance_raw)
        .expect("decode final balanceOf");

    let profit = final_tkb.saturating_sub(initial_tkb);
    println!("[test] Bot final TKB:  {final_tkb}");
    println!("[test] Arbitrage profit: {profit} TKB wei");

    assert!(
        final_tkb > initial_tkb,
        "\nArbitrage did NOT produce profit.\n\
         Initial TKB balance: {initial_tkb}\n\
         Final   TKB balance: {final_tkb}\n\n\
         Possible causes:\n\
         • The bot found no profitable path (check pool reserve ratios).\n\
         • The executor transaction reverted (check executor logic).\n\
         • min_profit_wei was set too high (currently 0 — should be fine).\n\
         • The conditional-RPC proxy failed to forward the tx to Anvil.\n\
         Rerun with RUST_LOG=debug for full bot output."
    );

    println!("[test] ✓ Arbitrage successful — {profit} TKB wei profit.");
    Ok(())
}

use thiserror::Error;

#[derive(Debug, Error)]
pub enum BotError {
    // ── Feed / WebSocket ─────────────────────────────────────────────────────
    #[error("WebSocket connection error: {0}")]
    WebSocket(#[from] tokio_tungstenite::tungstenite::Error),

    #[error("Feed message deserialization failed: {0}")]
    FeedDeserialize(#[from] serde_json::Error),

    #[error("Base64 decode of l2Msg failed: {0}")]
    Base64Decode(#[from] base64::DecodeError),

    #[error("RLP decode of transaction failed: {0}")]
    RlpDecode(String),

    #[error("ABI decode of calldata failed: {0}")]
    AbiDecode(String),

    // ── Database ─────────────────────────────────────────────────────────────
    #[error("SQLite error: {0}")]
    Database(#[from] rusqlite::Error),

    // ── RPC / Provider ───────────────────────────────────────────────────────
    #[error("RPC call failed: {0}")]
    Rpc(String),

    // ── Engine ───────────────────────────────────────────────────────────────
    #[error("No arbitrage path found for token pair {0:?} → {1:?}")]
    NoPath(alloy::primitives::Address, alloy::primitives::Address),

    #[error("Arithmetic overflow in AMM simulation")]
    Overflow,

    // ── Execution ─────────────────────────────────────────────────────────────
    #[error("Transaction signing failed: {0}")]
    Signing(String),

    #[error("eth_sendRawTransactionConditional rejected: {0}")]
    ConditionalSend(String),

    // ── Configuration ─────────────────────────────────────────────────────────
    #[error("Missing environment variable: {0}")]
    Config(String),
}

pub type Result<T> = std::result::Result<T, BotError>;

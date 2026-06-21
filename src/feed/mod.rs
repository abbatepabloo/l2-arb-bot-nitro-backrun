pub mod message;
pub mod transaction;

pub use transaction::SequencerFeedTransaction;

use futures_util::{SinkExt, StreamExt};
use std::collections::HashSet;
use std::sync::Arc;
use tokio::sync::mpsc;
use tokio_tungstenite::{connect_async, tungstenite::Message};
use tracing::{error, info, warn};

use crate::error::Result;
use crate::pool::manager::LiquidityPoolManager;
use message::FeedFrame;

/// Connects to the Arbitrum Sequencer Feed WebSocket and forwards every parsed
/// `SequencerFeedTransaction` onto the provided `mpsc::Sender`.
///
/// This task runs indefinitely and handles reconnects automatically.
/// It is launched by the `ArbitrageEngine` during bootstrap and must be the
/// **first** thing started so the engine can buffer transactions while the
/// RPC reserve fetch is in flight.
///
/// # Hot-path contract
///
/// Every allocation inside this loop impacts latency.  The current design:
///   - Zero-copy JSON deserialization via lifetime-borrowed `FeedFrame<'_>`.
///   - Base64 decode into a local `Vec<u8>` (INJECT: replace with thread-local
///     pooled buffer to eliminate heap allocation on every message).
///   - ABI decode only occurs when the 4-byte selector matches a monitored fn.
pub async fn run_feed_listener(
    feed_url: String,
    monitored_routers: HashSet<alloy::primitives::Address>,
    lpm: Arc<LiquidityPoolManager>,
    tx: mpsc::Sender<SequencerFeedTransaction>,
) {
    loop {
        match connect_and_stream(&feed_url, &monitored_routers, &lpm, &tx).await {
            Ok(()) => {
                warn!("Sequencer feed connection closed cleanly — reconnecting…");
            }
            Err(e) => {
                error!("Sequencer feed error: {e} — reconnecting in 500 ms…");
                tokio::time::sleep(tokio::time::Duration::from_millis(500)).await;
            }
        }
    }
}

async fn connect_and_stream(
    feed_url: &str,
    monitored_routers: &HashSet<alloy::primitives::Address>,
    lpm: &Arc<LiquidityPoolManager>,
    tx: &mpsc::Sender<SequencerFeedTransaction>,
) -> Result<()> {
    info!(url = feed_url, "Connecting to Arbitrum Sequencer Feed…");

    let (ws_stream, _response) = connect_async(feed_url).await?;
    let (_write, mut read) = ws_stream.split();

    info!("Sequencer Feed WebSocket connected");

    while let Some(msg) = read.next().await {
        match msg? {
            Message::Text(text) => {
                process_text_frame(text.as_str(), monitored_routers, lpm, tx).await;
            }
            Message::Binary(bin) => {
                // Some sequencer relay implementations send binary frames.
                if let Ok(text) = std::str::from_utf8(&bin) {
                    process_text_frame(text, monitored_routers, lpm, tx).await;
                }
            }
            Message::Ping(payload) => {
                // tokio-tungstenite handles Pong automatically, nothing to do.
                let _ = payload;
            }
            Message::Close(_) => break,
            _ => {}
        }
    }

    Ok(())
}

/// Parse a single JSON WebSocket frame and emit any arbitrable transactions.
///
/// Designed to be inlined into the hot loop. `#[inline(always)]` is intentional
/// — the function body is small and the branch misprediction cost of an indirect
/// call would exceed the inlining cost.
#[inline(always)]
async fn process_text_frame(
    text: &str,
    monitored_routers: &HashSet<alloy::primitives::Address>,
    lpm: &Arc<LiquidityPoolManager>,
    tx: &mpsc::Sender<SequencerFeedTransaction>,
) {
    // Zero-copy serde deserialize: FeedFrame<'_> borrows from `text`.
    let frame: FeedFrame<'_> = match serde_json::from_str(text) {
        Ok(f)  => f,
        Err(e) => {
            warn!("Failed to deserialize feed frame: {e}");
            return;
        }
    };

    for msg in &frame.messages {
        let Some(seq_tx) =
            SequencerFeedTransaction::parse(msg, monitored_routers, lpm)
        else {
            continue;
        };

        // Send to the engine channel.  If the receiver is full (back-pressure),
        // we drop the message and log a warning — the engine can't keep up.
        // In production, use a bounded channel sized to hold ~10 ms of messages
        // at peak throughput (empirically ~1,000–5,000 tx/s on Arbitrum One).
        if tx.try_send(seq_tx).is_err() {
            warn!("Engine channel full — dropping sequencer tx");
        }
    }
}

use serde::Deserialize;

// ── Raw JSON deserialization of the Arbitrum Nitro Sequencer Feed ─────────────
//
// The sequencer feed is a WebSocket stream. Each frame contains a JSON object
// with the structure below. The critical field is `l2Msg`: a Base64-encoded
// byte array that, after decoding, contains:
//
//   Byte 0:       L2MessageType  (0x00=unsigned user tx, 0x02=signed tx, ...)
//   Bytes 1..:    RLP-encoded EIP-2718 typed transaction (or legacy tx)
//
// We zero-copy deserialize using `serde_json`'s `&'de str` lifetime to avoid
// cloning the large base64 string until we absolutely must decode it.

/// Top-level Sequencer Feed frame.
#[derive(Debug, Deserialize)]
pub struct FeedFrame<'a> {
    pub version: u8,

    #[serde(borrow)]
    pub messages: Vec<FeedMessage<'a>>,
}

/// One sequencer-ordered message.
#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct FeedMessage<'a> {
    #[serde(rename = "sequenceNumber")]
    pub sequence_number: u64,

    #[serde(borrow)]
    pub message: MessageWrapper<'a>,
}

/// The outer envelope that wraps the actual L2 payload.
#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct MessageWrapper<'a> {
    #[serde(borrow)]
    pub message: L2Message<'a>,

    pub delayed_messages_read: u64,
}

/// The inner message that contains the raw L2 transaction data.
#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct L2Message<'a> {
    #[serde(borrow)]
    pub header: MessageHeader<'a>,

    /// Base64-encoded bytes:  [l2MsgType (1 byte)] ++ [RLP-encoded EVM tx]
    ///
    /// Use `base64::engine::general_purpose::STANDARD.decode()` to get raw bytes.
    /// This is a borrowed `&str` pointing into the original WebSocket buffer,
    /// avoiding a heap allocation until decode time.
    #[serde(rename = "l2Msg")]
    pub l2_msg: &'a str,
}

/// L2 message header — contains network context needed for gas calculations.
#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct MessageHeader<'a> {
    /// Message kind byte:
    ///   3 = L1MessageType_L2Message (most sequencer feed txs)
    pub kind: u8,

    /// Sender address of the sequencer node (hex string).
    pub sender: &'a str,

    /// L1 block number at which this message was included.
    pub block_number: u64,

    /// Unix timestamp in seconds.
    pub timestamp: u64,

    /// L1 base fee at the time of inclusion (null for L2-only messages).
    #[serde(rename = "l1BaseFee")]
    pub l1_base_fee: Option<&'a str>,
}

// ── L2 Message type constants ─────────────────────────────────────────────────
//
// Source: nitro/arbos/arbostypes/messagetype.go

pub const L2_MSG_TYPE_UNSIGNED_USER_TX: u8 = 0x00;
pub const L2_MSG_TYPE_SIGNED_TX:        u8 = 0x02;
pub const L2_MSG_TYPE_NON_MUTATING:     u8 = 0x04;
pub const L2_MSG_TYPE_BATCH:            u8 = 0x06;

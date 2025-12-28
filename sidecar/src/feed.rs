use serde::{Serialize, Serializer};
use serde_json::Value as JsonValue;

/// API version for forward compatibility
pub const API_VERSION: &str = "v1";

/// Serialize i64 as a decimal string for JavaScript compatibility
fn serialize_i64_as_string<S>(value: &i64, serializer: S) -> Result<S::Ok, S::Error>
where
    S: Serializer,
{
    serializer.serialize_str(&value.to_string())
}

/// Serialize Option<i64> as a decimal string for JavaScript compatibility
fn serialize_option_i64_as_string<S>(value: &Option<i64>, serializer: S) -> Result<S::Ok, S::Error>
where
    S: Serializer,
{
    match value {
        Some(v) => serializer.serialize_some(&v.to_string()),
        None => serializer.serialize_none(),
    }
}

/// Watermarks indicating chain and wallet sync state
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Watermarks {
    /// The current chain head height
    #[serde(serialize_with = "serialize_i64_as_string")]
    pub chain_head: i64,
    /// The highest block height where the wallet is fully synced
    #[serde(serialize_with = "serialize_option_i64_as_string")]
    pub wallet_ready_height: Option<i64>,
    /// The highest finalized block height (optional, for reorg safety)
    #[serde(serialize_with = "serialize_option_i64_as_string")]
    pub finalized_height: Option<i64>,
}

/// Block metadata included in feed responses
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct BlockMeta {
    /// Block height
    #[serde(serialize_with = "serialize_i64_as_string")]
    pub height: i64,
    /// Block hash (hex encoded)
    pub hash: String,
    /// Parent block hash (hex encoded)
    pub parent_hash: String,
    /// Block timestamp (Unix seconds)
    #[serde(serialize_with = "serialize_i64_as_string")]
    pub timestamp: i64,
}

/// Transaction metadata (optional heavy fields, emitted once per tx)
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct TransactionMeta {
    /// Transaction ID
    #[serde(serialize_with = "serialize_i64_as_string")]
    pub tx_id: i64,
    /// Transaction hash (hex encoded)
    pub tx_hash: String,
    /// Block height
    #[serde(serialize_with = "serialize_i64_as_string")]
    pub block_height: i64,
    /// Protocol version
    #[serde(serialize_with = "serialize_i64_as_string")]
    pub protocol_version: i64,
    /// Transaction result (JSON), null for system txs
    pub transaction_result: Option<JsonValue>,
    /// Serialized transaction identifiers (hex encoded), optional
    pub identifiers: Option<Vec<String>>,
    /// Fees for this transaction
    pub fees: TransactionFees,
    /// Merkle indices and root for this transaction
    pub merkle: TransactionMerkle,
    /// Contract action summaries for this transaction (optional; omitted when empty)
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub contract_actions: Vec<ContractActionSummary>,
    /// Raw serialized transaction (hex encoded), optional heavy field
    pub raw: Option<String>,
}

/// Transaction fee metadata
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct TransactionFees {
    /// Fees paid (decimal string), null for system txs
    pub paid: Option<String>,
    /// Estimated fees (decimal string), null for system txs
    pub estimated: Option<String>,
}

/// Transaction merkle metadata
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct TransactionMerkle {
    /// Merkle root (hex encoded), null for system txs
    pub root: Option<String>,
    /// Start index (decimal string), null for system txs
    #[serde(serialize_with = "serialize_option_i64_as_string")]
    pub start_index: Option<i64>,
    /// End index (decimal string), null for system txs
    #[serde(serialize_with = "serialize_option_i64_as_string")]
    pub end_index: Option<i64>,
}

/// A shielded relevant transaction from the wallet indexer
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ShieldedRelevantTx {
    /// Transaction ID
    #[serde(serialize_with = "serialize_i64_as_string")]
    pub tx_id: i64,
    /// Transaction hash (hex encoded)
    pub tx_hash: String,
    /// Block height
    #[serde(serialize_with = "serialize_i64_as_string")]
    pub block_height: i64,
    /// Raw zswap ledger event bytes (hex encoded), ordered by ledger_events.id
    pub zswap_events: Vec<String>,
    /// Contract action summaries for this transaction
    pub contract_actions: Vec<ContractActionSummary>,
    /// Fees paid for this transaction (decimal string)
    pub fees_paid: String,
}

/// Summary of a contract action
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ContractActionSummary {
    /// Contract address (hex encoded)
    pub contract_address: String,
    /// Action type: "Deploy", "Call", or "Update"
    pub action_type: String,
    /// Entry point name (only present for "Call" actions)
    #[serde(skip_serializing_if = "Option::is_none")]
    pub entry_point: Option<String>,
}

/// Raw ZswapInput ledger event for global spend detection
///
/// This provides the raw event bytes instead of parsed nullifiers, allowing:
/// - No server-side preprocessing required
/// - Client parses with midnight-ledger SDK
/// - Consistent with other raw event formats (zswap_events, dust)
///
/// Clients can disable this via `includeNullifiers=false` if using
/// 0-value change outputs for spend detection instead.
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ZswapInputRaw {
    /// Transaction ID
    #[serde(serialize_with = "serialize_i64_as_string")]
    pub tx_id: i64,
    /// Transaction hash (hex encoded)
    pub tx_hash: String,
    /// Ledger event ID (for ordering)
    #[serde(serialize_with = "serialize_i64_as_string")]
    pub ledger_event_id: i64,
    /// Block height
    #[serde(serialize_with = "serialize_i64_as_string")]
    pub block_height: i64,
    /// Raw ZswapInput event bytes (hex encoded)
    /// Client parses this using midnight-ledger SDK to extract nullifiers
    pub raw: String,
}

/// Raw dust ledger event
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct DustLedgerEventRaw {
    /// Transaction ID
    #[serde(serialize_with = "serialize_i64_as_string")]
    pub tx_id: i64,
    /// Ledger event ID (for ordering)
    #[serde(serialize_with = "serialize_i64_as_string")]
    pub ledger_event_id: i64,
    /// Block height
    #[serde(serialize_with = "serialize_i64_as_string")]
    pub block_height: i64,
    /// Raw event bytes (hex encoded)
    pub raw: String,
}

/// Unshielded delta for a registered address
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct UnshieldedDelta {
    /// Transaction ID
    #[serde(serialize_with = "serialize_i64_as_string")]
    pub tx_id: i64,
    /// Block height
    #[serde(serialize_with = "serialize_i64_as_string")]
    pub block_height: i64,
    /// The unshielded address (hex encoded)
    pub address: String,
    /// Delta amount (decimal string for bigint)
    pub delta: String,
}

/// Unshielded UTXO event for a registered address (create or spend)
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct UnshieldedUtxoEvent {
    /// Transaction ID
    #[serde(serialize_with = "serialize_i64_as_string")]
    pub tx_id: i64,
    /// Transaction hash (hex encoded)
    pub tx_hash: String,
    /// Block height
    #[serde(serialize_with = "serialize_i64_as_string")]
    pub block_height: i64,
    /// True if this event creates the UTXO, false if it spends it
    pub is_create: bool,
    /// The unshielded address (hex encoded)
    pub address: String,
    /// UTXO intent hash (hex encoded)
    pub intent_hash: String,
    /// Output index within the transaction
    #[serde(serialize_with = "serialize_i64_as_string")]
    pub output_index: i64,
    /// Token type (hex encoded)
    pub token_type: String,
    /// UTXO value (decimal string for bigint)
    pub value: String,
    /// Initial nonce (hex encoded)
    pub initial_nonce: String,
    /// Whether registered for dust generation
    pub registered_for_dust_generation: bool,
    /// Creation time from UTXO record (unix ms, optional)
    #[serde(serialize_with = "serialize_option_i64_as_string")]
    pub ctime: Option<i64>,
    /// Fees paid for the transaction (decimal string)
    pub fees_paid: String,
    /// ALL contract actions for this transaction (not just the first one!)
    /// Fetched separately from contract_actions table and attached by tx_id.
    pub contract_actions: Vec<ContractActionSummary>,
}

/// Collapsed merkle tree update for SDK fast-forward
/// Sent before shieldedRelevantTx when there's a gap in merkle tree indices
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct MerkleCollapsedUpdate {
    /// Start index in the commitment tree
    #[serde(serialize_with = "serialize_i64_as_string")]
    pub start_index: i64,
    /// End index in the commitment tree
    #[serde(serialize_with = "serialize_i64_as_string")]
    pub end_index: i64,
    /// Hex-encoded collapsed merkle tree update bytes (for SDK)
    pub update: String,
    /// Protocol version
    pub protocol_version: u32,
}

/// Control events for feed state management
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
#[serde(tag = "type")]
pub enum ControlEvent {
    /// Signals that a reorg was detected and client should reset
    #[serde(rename_all = "camelCase")]
    ResetRequired {
        /// Recommended height to restart sync from (decimal string)
        safe_restart_height: String,
        /// Reason for the reset
        reason: String,
    },
}

impl ControlEvent {
    /// Create a reset required event
    pub fn reset_required(safe_restart_height: i64, reason: impl Into<String>) -> Self {
        Self::ResetRequired {
            safe_restart_height: safe_restart_height.to_string(),
            reason: reason.into(),
        }
    }
}

/// A feed item - one of the possible event types
/// Uses adjacently tagged serde format: {"type": "...", "data": {...}}
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
#[serde(tag = "type", content = "data")]
pub enum FeedItem {
    /// Transaction metadata (optional heavy fields)
    TransactionMeta(TransactionMeta),
    /// Collapsed merkle tree update (sent before shieldedRelevantTx when there's a gap)
    MerkleCollapsedUpdate(MerkleCollapsedUpdate),
    /// Shielded relevant transaction
    ShieldedRelevantTx(ShieldedRelevantTx),
    /// Raw ZswapInput event for global spend detection (no preprocessing needed)
    ZswapInputRaw(ZswapInputRaw),
    /// Dust ledger event
    DustLedgerEvent(DustLedgerEventRaw),
    /// Unshielded delta
    UnshieldedDelta(UnshieldedDelta),
    /// Unshielded UTXO event (create/spend)
    UnshieldedUtxo(UnshieldedUtxoEvent),
    /// Control event
    Control(ControlEvent),
}

/// Feed item with an ordered sequence cursor
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct OrderedFeedItem {
    /// Global sequence cursor (block_height:tx_id:phase:ordinal)
    pub sequence: String,
    /// The actual feed item (flattened into the JSON object)
    #[serde(flatten)]
    pub item: FeedItem,
}

/// A block bundle containing all feed items for a single block
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct BlockBundle {
    /// Block metadata
    pub meta: BlockMeta,
    /// Feed items for this block, ordered by sequence
    pub items: Vec<OrderedFeedItem>,
}

/// Response for GET /v1/feed
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct FeedResponse {
    /// API version
    pub api_version: &'static str,
    /// Current watermarks
    pub watermarks: Watermarks,
    /// Block bundles, contiguous and ordered by height
    pub blocks: Vec<BlockBundle>,
    /// Next height to request (cursor for pagination)
    #[serde(serialize_with = "serialize_option_i64_as_string")]
    pub next_height: Option<i64>,
    /// Next sequence cursor for pagination (optional)
    #[serde(skip_serializing_if = "Option::is_none")]
    pub next_cursor: Option<String>,
}

impl FeedResponse {
    /// Create a new feed response with the given watermarks
    pub fn new(watermarks: Watermarks) -> Self {
        Self {
            api_version: API_VERSION,
            watermarks,
            blocks: Vec::new(),
            next_height: None,
            next_cursor: None,
        }
    }

    /// Create an empty response (no blocks, just watermarks)
    pub fn empty(watermarks: Watermarks) -> Self {
        Self::new(watermarks)
    }
}

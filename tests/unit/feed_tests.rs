//! Unit tests for the feed module
//!
//! Run with: cargo test --test feed_tests

use nocy_wallet_feed::feed::{
    BlockBundle, BlockMeta, ContractActionSummary, ControlEvent, DustLedgerEventRaw, FeedItem,
    FeedResponse, MerkleCollapsedUpdate, OrderedFeedItem, ShieldedRelevantTx, TransactionFees,
    TransactionMerkle, TransactionMeta, UnshieldedDelta, Watermarks, ZswapInputRaw, API_VERSION,
};

// =============================================================================
// Watermarks Tests
// =============================================================================

#[test]
fn test_watermarks_serialization() {
    let watermarks = Watermarks {
        chain_head: 1000,
        wallet_ready_height: Some(950),
        finalized_height: Some(1000),
    };

    let json = serde_json::to_string(&watermarks).unwrap();
    assert!(json.contains("\"chainHead\":\"1000\""));
    assert!(json.contains("\"walletReadyHeight\":\"950\""));
    assert!(json.contains("\"finalizedHeight\":\"1000\""));
}

#[test]
fn test_watermarks_serialization_with_null() {
    let watermarks = Watermarks {
        chain_head: 500,
        wallet_ready_height: None,
        finalized_height: None,
    };

    let json = serde_json::to_string(&watermarks).unwrap();
    assert!(json.contains("\"chainHead\":\"500\""));
    assert!(json.contains("\"walletReadyHeight\":null"));
    assert!(json.contains("\"finalizedHeight\":null"));
}

#[test]
fn test_watermarks_large_numbers() {
    let watermarks = Watermarks {
        chain_head: 9007199254740993,
        wallet_ready_height: Some(9007199254740994),
        finalized_height: Some(9007199254740995),
    };

    let json = serde_json::to_string(&watermarks).unwrap();
    assert!(json.contains("\"9007199254740993\""));
    assert!(json.contains("\"9007199254740994\""));
    assert!(json.contains("\"9007199254740995\""));
}

// =============================================================================
// BlockMeta Tests
// =============================================================================

#[test]
fn test_block_meta_serialization() {
    let meta = BlockMeta {
        height: 12345,
        hash: "abc123".to_string(),
        parent_hash: "def456".to_string(),
        timestamp: 1703123456,
    };

    let json = serde_json::to_string(&meta).unwrap();
    assert!(json.contains("\"height\":\"12345\""));
    assert!(json.contains("\"hash\":\"abc123\""));
    assert!(json.contains("\"parentHash\":\"def456\""));
    assert!(json.contains("\"timestamp\":\"1703123456\""));
}

// =============================================================================
// ControlEvent Tests
// =============================================================================

#[test]
fn test_control_event_reset_required() {
    let event = ControlEvent::reset_required(99, "Reorg detected at height 100");

    match event {
        ControlEvent::ResetRequired {
            safe_restart_height,
            reason,
        } => {
            assert_eq!(safe_restart_height, "99");
            assert_eq!(reason, "Reorg detected at height 100");
        }
    }
}

#[test]
fn test_control_event_serialization() {
    let event = ControlEvent::reset_required(99, "Reorg detected");

    let json = serde_json::to_string(&event).unwrap();
    assert!(json.contains("\"type\":\"resetRequired\""));
    assert!(json.contains("\"safeRestartHeight\":\"99\""));
    assert!(json.contains("\"reason\":\"Reorg detected\""));
}

// =============================================================================
// FeedItem Tests
// =============================================================================

#[test]
fn test_feed_item_shielded_relevant_tx_serialization() {
    let tx = ShieldedRelevantTx {
        tx_id: 12345,
        tx_hash: "abc123".to_string(),
        block_height: 100,
        zswap_events: vec!["event1".to_string(), "event2".to_string()],
        contract_actions: vec![],
        fees_paid: "1000".to_string(),
    };

    let item = FeedItem::ShieldedRelevantTx(tx);
    let json = serde_json::to_string(&item).unwrap();

    assert!(json.contains("\"type\":\"shieldedRelevantTx\""));
    assert!(json.contains("\"data\":{"));
    assert!(json.contains("\"txId\":\"12345\""));
    assert!(json.contains("\"blockHeight\":\"100\""));
}

#[test]
fn test_feed_item_zswap_input_raw_serialization() {
    let raw = ZswapInputRaw {
        tx_id: 12346,
        tx_hash: "def456".to_string(),
        ledger_event_id: 5678,
        block_height: 100,
        raw: "deadbeef".to_string(),
    };

    let item = FeedItem::ZswapInputRaw(raw);
    let json = serde_json::to_string(&item).unwrap();

    assert!(json.contains("\"type\":\"zswapInputRaw\""));
    assert!(json.contains("\"ledgerEventId\":\"5678\""));
    assert!(json.contains("\"raw\":\"deadbeef\""));
}

#[test]
fn test_feed_item_dust_ledger_event_serialization() {
    let dust = DustLedgerEventRaw {
        tx_id: 12347,
        ledger_event_id: 9999,
        block_height: 100,
        raw: "cafebabe".to_string(),
    };

    let item = FeedItem::DustLedgerEvent(dust);
    let json = serde_json::to_string(&item).unwrap();

    assert!(json.contains("\"type\":\"dustLedgerEvent\""));
    assert!(json.contains("\"raw\":\"cafebabe\""));
}

#[test]
fn test_feed_item_unshielded_delta_serialization() {
    let delta = UnshieldedDelta {
        tx_id: 12348,
        block_height: 100,
        address: "address123".to_string(),
        delta: "-500000".to_string(),
    };

    let item = FeedItem::UnshieldedDelta(delta);
    let json = serde_json::to_string(&item).unwrap();

    assert!(json.contains("\"type\":\"unshieldedDelta\""));
    assert!(json.contains("\"delta\":\"-500000\""));
}

#[test]
fn test_feed_item_control_serialization() {
    let control = ControlEvent::reset_required(50, "Test reset");
    let item = FeedItem::Control(control);
    let json = serde_json::to_string(&item).unwrap();

    assert!(json.contains("\"type\":\"control\""));
    assert!(json.contains("\"resetRequired\""));
}

// =============================================================================
// OrderedFeedItem Tests
// =============================================================================

#[test]
fn test_ordered_feed_item_serialization() {
    let tx = ShieldedRelevantTx {
        tx_id: 100,
        tx_hash: "hash".to_string(),
        block_height: 50,
        zswap_events: vec![],
        contract_actions: vec![],
        fees_paid: "0".to_string(),
    };

    let ordered = OrderedFeedItem {
        sequence: "50:100:0:0".to_string(),
        item: FeedItem::ShieldedRelevantTx(tx),
    };

    let json = serde_json::to_string(&ordered).unwrap();
    assert!(json.contains("\"sequence\":\"50:100:0:0\""));
    assert!(json.contains("\"type\":\"shieldedRelevantTx\""));
}

// =============================================================================
// BlockBundle Tests
// =============================================================================

#[test]
fn test_block_bundle_serialization() {
    let bundle = BlockBundle {
        meta: BlockMeta {
            height: 100,
            hash: "blockhash".to_string(),
            parent_hash: "parenthash".to_string(),
            timestamp: 1703123456,
        },
        items: vec![],
    };

    let json = serde_json::to_string(&bundle).unwrap();
    assert!(json.contains("\"meta\":{"));
    assert!(json.contains("\"items\":[]"));
}

#[test]
fn test_block_bundle_with_items() {
    let tx = ShieldedRelevantTx {
        tx_id: 1,
        tx_hash: "tx1".to_string(),
        block_height: 100,
        zswap_events: vec!["e1".to_string()],
        contract_actions: vec![ContractActionSummary {
            contract_address: "c1".to_string(),
            action_type: "Call".to_string(),
            entry_point: Some("transfer".to_string()),
        }],
        fees_paid: "100".to_string(),
    };

    let bundle = BlockBundle {
        meta: BlockMeta {
            height: 100,
            hash: "blockhash".to_string(),
            parent_hash: "parenthash".to_string(),
            timestamp: 1703123456,
        },
        items: vec![OrderedFeedItem {
            sequence: "100:1:0:0".to_string(),
            item: FeedItem::ShieldedRelevantTx(tx),
        }],
    };

    let json = serde_json::to_string(&bundle).unwrap();
    assert!(json.contains("\"items\":[{"));
    assert!(json.contains("\"contractActions\":[{"));
    assert!(json.contains("\"entryPoint\":\"transfer\""));
}

// =============================================================================
// FeedResponse Tests
// =============================================================================

#[test]
fn test_feed_response_new() {
    let watermarks = Watermarks {
        chain_head: 1000,
        wallet_ready_height: Some(950),
        finalized_height: Some(1000),
    };

    let response = FeedResponse::new(watermarks);

    assert_eq!(response.api_version, "v1");
    assert!(response.blocks.is_empty());
    assert!(response.next_height.is_none());
    assert!(response.next_cursor.is_none());
}

#[test]
fn test_feed_response_empty() {
    let watermarks = Watermarks {
        chain_head: 500,
        wallet_ready_height: None,
        finalized_height: None,
    };

    let response = FeedResponse::empty(watermarks);

    assert_eq!(response.api_version, API_VERSION);
    assert!(response.blocks.is_empty());
}

#[test]
fn test_feed_response_serialization() {
    let watermarks = Watermarks {
        chain_head: 1000,
        wallet_ready_height: Some(950),
        finalized_height: Some(1000),
    };

    let mut response = FeedResponse::new(watermarks);
    response.next_height = Some(101);
    response.next_cursor = Some("100:999:2:0".to_string());

    let json = serde_json::to_string(&response).unwrap();
    assert!(json.contains("\"apiVersion\":\"v1\""));
    assert!(json.contains("\"watermarks\":{"));
    assert!(json.contains("\"blocks\":[]"));
    assert!(json.contains("\"nextHeight\":\"101\""));
    assert!(json.contains("\"nextCursor\":\"100:999:2:0\""));
}

#[test]
fn test_feed_response_next_cursor_omitted_when_none() {
    let watermarks = Watermarks {
        chain_head: 1000,
        wallet_ready_height: None,
        finalized_height: None,
    };

    let response = FeedResponse::new(watermarks);
    let json = serde_json::to_string(&response).unwrap();

    assert!(!json.contains("nextCursor"));
    assert!(json.contains("\"nextHeight\":null"));
}

// =============================================================================
// ContractActionSummary Tests
// =============================================================================

#[test]
fn test_contract_action_summary_with_entry_point() {
    let action = ContractActionSummary {
        contract_address: "abc123".to_string(),
        action_type: "Call".to_string(),
        entry_point: Some("transfer".to_string()),
    };

    let json = serde_json::to_string(&action).unwrap();
    assert!(json.contains("\"contractAddress\":\"abc123\""));
    assert!(json.contains("\"actionType\":\"Call\""));
    assert!(json.contains("\"entryPoint\":\"transfer\""));
}

#[test]
fn test_contract_action_summary_without_entry_point() {
    let action = ContractActionSummary {
        contract_address: "def456".to_string(),
        action_type: "Deploy".to_string(),
        entry_point: None,
    };

    let json = serde_json::to_string(&action).unwrap();
    assert!(json.contains("\"actionType\":\"Deploy\""));
    assert!(!json.contains("entryPoint"));
}

// =============================================================================
// TransactionMeta Tests
// =============================================================================

#[test]
fn test_transaction_meta_serialization() {
    let meta = TransactionMeta {
        tx_id: 12345,
        tx_hash: "txhash".to_string(),
        block_height: 100,
        protocol_version: 2,
        transaction_result: Some(serde_json::json!({"status": "success"})),
        identifiers: Some(vec!["id1".to_string(), "id2".to_string()]),
        fees: TransactionFees {
            paid: Some("1000".to_string()),
            estimated: Some("900".to_string()),
        },
        merkle: TransactionMerkle {
            root: Some("merkleroot".to_string()),
            start_index: Some(10),
            end_index: Some(15),
        },
        contract_actions: vec![],
        raw: None,
    };

    let json = serde_json::to_string(&meta).unwrap();
    assert!(json.contains("\"txId\":\"12345\""));
    assert!(json.contains("\"protocolVersion\":\"2\""));
    assert!(json.contains("\"transactionResult\":{\"status\":\"success\"}"));
    assert!(json.contains("\"identifiers\":[\"id1\",\"id2\"]"));
}

#[test]
fn test_transaction_meta_contract_actions_omitted_when_empty() {
    let meta = TransactionMeta {
        tx_id: 1,
        tx_hash: "hash".to_string(),
        block_height: 1,
        protocol_version: 2,
        transaction_result: None,
        identifiers: None,
        fees: TransactionFees {
            paid: None,
            estimated: None,
        },
        merkle: TransactionMerkle {
            root: None,
            start_index: None,
            end_index: None,
        },
        contract_actions: vec![],
        raw: None,
    };

    let json = serde_json::to_string(&meta).unwrap();
    assert!(!json.contains("contractActions"));
}

// =============================================================================
// MerkleCollapsedUpdate Tests
// =============================================================================

#[test]
fn test_merkle_collapsed_update_serialization() {
    let update = MerkleCollapsedUpdate {
        start_index: 1000,
        end_index: 2000,
        update: "deadbeefcafe".to_string(),
        protocol_version: 2,
    };

    let json = serde_json::to_string(&update).unwrap();
    assert!(json.contains("\"startIndex\":\"1000\""));
    assert!(json.contains("\"endIndex\":\"2000\""));
    assert!(json.contains("\"update\":\"deadbeefcafe\""));
    assert!(json.contains("\"protocolVersion\":2"));
}

//! Unit tests for the sequence module
//!
//! Run with: cargo test --test sequence_tests

use nocy_wallet_feed::sequence::{parse_cursor, SequenceKey};

#[test]
fn test_sequence_key_to_cursor() {
    let key = SequenceKey {
        block_height: 100,
        tx_id: 12345,
        phase: 1,
        ordinal: 5678,
    };
    assert_eq!(key.to_cursor(), "100:12345:1:5678");
}

#[test]
fn test_sequence_key_to_cursor_with_zeros() {
    let key = SequenceKey {
        block_height: 0,
        tx_id: 0,
        phase: 0,
        ordinal: 0,
    };
    assert_eq!(key.to_cursor(), "0:0:0:0");
}

#[test]
fn test_sequence_key_to_cursor_with_negative_phase() {
    // Control events use phase -1
    let key = SequenceKey {
        block_height: 100,
        tx_id: 0,
        phase: -1,
        ordinal: 0,
    };
    assert_eq!(key.to_cursor(), "100:0:-1:0");
}

#[test]
fn test_parse_cursor_valid() {
    let cursor = "100:12345:1:5678";
    let key = parse_cursor(cursor).unwrap();
    assert_eq!(key.block_height, 100);
    assert_eq!(key.tx_id, 12345);
    assert_eq!(key.phase, 1);
    assert_eq!(key.ordinal, 5678);
}

#[test]
fn test_parse_cursor_with_zeros() {
    let cursor = "0:0:0:0";
    let key = parse_cursor(cursor).unwrap();
    assert_eq!(key.block_height, 0);
    assert_eq!(key.tx_id, 0);
    assert_eq!(key.phase, 0);
    assert_eq!(key.ordinal, 0);
}

#[test]
fn test_parse_cursor_with_negative_phase() {
    let cursor = "100:0:-1:0";
    let key = parse_cursor(cursor).unwrap();
    assert_eq!(key.block_height, 100);
    assert_eq!(key.tx_id, 0);
    assert_eq!(key.phase, -1);
    assert_eq!(key.ordinal, 0);
}

#[test]
fn test_parse_cursor_large_numbers() {
    let cursor = "9007199254740993:9007199254740994:2:9007199254740995";
    let key = parse_cursor(cursor).unwrap();
    assert_eq!(key.block_height, 9007199254740993);
    assert_eq!(key.tx_id, 9007199254740994);
    assert_eq!(key.phase, 2);
    assert_eq!(key.ordinal, 9007199254740995);
}

#[test]
fn test_parse_cursor_roundtrip() {
    let original = SequenceKey {
        block_height: 12345,
        tx_id: 67890,
        phase: 1,
        ordinal: 999,
    };
    let cursor = original.to_cursor();
    let parsed = parse_cursor(&cursor).unwrap();
    assert_eq!(original, parsed);
}

#[test]
fn test_parse_cursor_too_few_parts() {
    let result = parse_cursor("100:200:300");
    assert!(result.is_err());
    assert!(result.unwrap_err().contains("4 colon-separated parts"));
}

#[test]
fn test_parse_cursor_too_many_parts() {
    let result = parse_cursor("100:200:300:400:500");
    assert!(result.is_err());
    assert!(result.unwrap_err().contains("4 colon-separated parts"));
}

#[test]
fn test_parse_cursor_empty() {
    let result = parse_cursor("");
    assert!(result.is_err());
}

#[test]
fn test_parse_cursor_invalid_block_height() {
    let result = parse_cursor("abc:200:300:400");
    assert!(result.is_err());
    assert!(result.unwrap_err().contains("blockHeight"));
}

#[test]
fn test_parse_cursor_invalid_tx_id() {
    let result = parse_cursor("100:abc:300:400");
    assert!(result.is_err());
    assert!(result.unwrap_err().contains("txId"));
}

#[test]
fn test_parse_cursor_invalid_phase() {
    let result = parse_cursor("100:200:abc:400");
    assert!(result.is_err());
    assert!(result.unwrap_err().contains("phase"));
}

#[test]
fn test_parse_cursor_invalid_ordinal() {
    let result = parse_cursor("100:200:300:abc");
    assert!(result.is_err());
    assert!(result.unwrap_err().contains("ordinal"));
}

#[test]
fn test_sequence_key_ordering() {
    let key1 = SequenceKey {
        block_height: 100,
        tx_id: 1,
        phase: 0,
        ordinal: 0,
    };
    let key2 = SequenceKey {
        block_height: 100,
        tx_id: 2,
        phase: 0,
        ordinal: 0,
    };
    let key3 = SequenceKey {
        block_height: 101,
        tx_id: 1,
        phase: 0,
        ordinal: 0,
    };

    // Same block, different tx_id
    assert!(key1 < key2);

    // Different block heights
    assert!(key2 < key3);

    // Transitivity
    assert!(key1 < key3);
}

#[test]
fn test_sequence_key_ordering_by_phase() {
    let key1 = SequenceKey {
        block_height: 100,
        tx_id: 1,
        phase: 0,
        ordinal: 0,
    };
    let key2 = SequenceKey {
        block_height: 100,
        tx_id: 1,
        phase: 1,
        ordinal: 0,
    };

    assert!(key1 < key2);
}

#[test]
fn test_sequence_key_ordering_by_ordinal() {
    let key1 = SequenceKey {
        block_height: 100,
        tx_id: 1,
        phase: 1,
        ordinal: 5,
    };
    let key2 = SequenceKey {
        block_height: 100,
        tx_id: 1,
        phase: 1,
        ordinal: 10,
    };

    assert!(key1 < key2);
}

#[test]
fn test_sequence_key_equality() {
    let key1 = SequenceKey {
        block_height: 100,
        tx_id: 200,
        phase: 1,
        ordinal: 300,
    };
    let key2 = SequenceKey {
        block_height: 100,
        tx_id: 200,
        phase: 1,
        ordinal: 300,
    };

    assert_eq!(key1, key2);
}

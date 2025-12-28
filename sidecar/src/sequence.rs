//! Sequence cursor utilities for unified feed ordering.

/// Ordering key for feed items.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub struct SequenceKey {
    pub block_height: i64,
    pub tx_id: i64,
    pub phase: i64,
    pub ordinal: i64,
}

impl SequenceKey {
    /// Render to cursor string format: "blockHeight:txId:phase:ordinal".
    pub fn to_cursor(self) -> String {
        format!(
            "{}:{}:{}:{}",
            self.block_height, self.tx_id, self.phase, self.ordinal
        )
    }
}

/// Parse a cursor string into a sequence key.
pub fn parse_cursor(cursor: &str) -> Result<SequenceKey, String> {
    let parts: Vec<&str> = cursor.split(':').collect();
    if parts.len() != 4 {
        return Err("cursor must have 4 colon-separated parts".to_string());
    }

    let block_height = parts[0]
        .parse::<i64>()
        .map_err(|_| "cursor blockHeight must be a valid integer".to_string())?;
    let tx_id = parts[1]
        .parse::<i64>()
        .map_err(|_| "cursor txId must be a valid integer".to_string())?;
    let phase = parts[2]
        .parse::<i64>()
        .map_err(|_| "cursor phase must be a valid integer".to_string())?;
    let ordinal = parts[3]
        .parse::<i64>()
        .map_err(|_| "cursor ordinal must be a valid integer".to_string())?;

    Ok(SequenceKey {
        block_height,
        tx_id,
        phase,
        ordinal,
    })
}

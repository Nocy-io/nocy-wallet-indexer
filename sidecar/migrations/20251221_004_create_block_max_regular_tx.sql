-- Optional materialized cache for walletReadyHeight performance
-- Stores the maximum regular transaction ID per block height
CREATE TABLE nocy_sidecar.block_max_regular_tx (
    block_height BIGINT PRIMARY KEY,
    max_regular_tx_id BIGINT
);

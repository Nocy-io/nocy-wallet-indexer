# Nocy Wallet Feed Sidecar

A unified wallet synchronization service for the Midnight blockchain that aggregates, transforms, and streams wallet-relevant data through a single REST/SSE API.

## Overview

The Nocy Wallet Feed Sidecar sits between wallet clients and the Midnight blockchain infrastructure, providing a simplified, pre-aggregated data feed. Instead of requiring clients to make multiple database queries, join tables, manage WebSocket subscriptions, and handle complex data transformations, the sidecar delivers a single unified feed containing:

- **Shielded Transactions** — Transactions relevant to the wallet's viewing key, with pre-joined ZSwap events
- **Transaction Metadata** — Fees, merkle tree indices, protocol version, and optional raw transaction bytes
- **Nullifier Streaming** — Global ZSwapInput events for client-side spend detection
- **Dust Events** — Ledger events matching the wallet's dust public key
- **Unshielded UTXOs** — UTXO creates and spends for registered addresses
- **Contract Actions** — Deploy, call, and update actions attached to relevant transactions
- **Merkle Tree State** — First-free index and collapsed updates for fast tree synchronization

### Key Features

- **Unified Feed API** — Single endpoint replaces 10+ separate database queries
- **Real-time Streaming** — Server-Sent Events (SSE) for live block updates
- **Deterministic Session IDs** — BLAKE3-based session IDs enable client reconnection without state storage
- **Automatic Reorg Detection** — Parent hash validation with `control.resetRequired` events
- **Data Normalization** — Accepts hex or bech32m viewing keys, outputs canonical formats
- **JavaScript Compatibility** — Large integers serialized as strings to avoid precision loss
- **Background Caching** — Merkle updates cached for fast historical queries
- **Prometheus Metrics** — Built-in observability for production deployments

## Architecture

```
┌─────────────────────────────────────────────────────────────────┐
│                         Clients (Wallets)                        │
└─────────────────────────────────────────────────────────────────┘
                                │
                    REST API / SSE (port 8080)
                                │
┌─────────────────────────────────────────────────────────────────┐
│                    nocy-wallet-feed Sidecar                      │
│  ┌─────────────┐  ┌─────────────┐  ┌─────────────────────────┐  │
│  │   Session   │  │    Feed     │  │        Zswap            │  │
│  │  Management │  │  Streaming  │  │  (merkle, first-free)   │  │
│  └─────────────┘  └─────────────┘  └─────────────────────────┘  │
└─────────────────────────────────────────────────────────────────┘
         │                    │                    │
         ▼                    ▼                    ▼
┌─────────────┐      ┌─────────────┐      ┌─────────────────────┐
│  PostgreSQL │      │    NATS     │      │  Upstream Indexer   │
│  (sessions) │      │  (ledger)   │      │  (GraphQL + WS)     │
└─────────────┘      └─────────────┘      └─────────────────────┘
```

## External Dependencies

The sidecar requires connections to three external services:

| Service | Protocol | Purpose | Required |
|---------|----------|---------|----------|
| **PostgreSQL** | TCP/5432 | Session profiles, merkle cache, address tracking | Yes |
| **NATS** | TCP/4222 | Ledger state snapshots (real-time chain state) | Yes |
| **Upstream Wallet Indexer** | HTTP/WS | Shielded transaction indexing, GraphQL API | Yes |

> ⚠️ **Important: Standalone Indexer Not Supported**
>
> This sidecar **does not work with the standalone wallet-indexer**. It requires a full deployment with NATS messaging infrastructure. The NATS connection provides real-time ledger state snapshots that are essential for the `/v1/zswap/*` endpoints.

### NATS Data Requirements

The sidecar subscribes to the `ledger_state` NATS subject to receive real-time ledger state snapshots:

```json
{
  "blockHeight": 12345,
  "protocolVersion": 2,
  "merkleRoot": "abc123def456...",
  "firstFreeIndex": 1000,
  "commitmentCount": 999,
  "timestampMs": 1703123456789
}
```

| Field | Type | Description |
|-------|------|-------------|
| `blockHeight` | i64 | Block height this snapshot is valid for |
| `protocolVersion` | u32 | Protocol version at this height |
| `merkleRoot` | string | Merkle tree root hash (hex) |
| `firstFreeIndex` | u64 | First free index in commitment tree |
| `commitmentCount` | u64 | Total number of commitments |
| `timestampMs` | i64 | Snapshot creation timestamp (Unix ms) |

**Used by:**
- `GET /v1/zswap/first-free` - Returns `firstFreeIndex` and `blockHeight`
- `GET /v1/zswap/collapsed-update` - Returns full snapshot for merkle tree sync
- `GET /readyz` - Checks NATS connection state for readiness probe

**Without NATS:**
- `/v1/zswap/first-free` falls back to database query (slower, less accurate)
- `/v1/zswap/collapsed-update` returns `503 Service Unavailable`
- `/readyz` returns `503` with `nats: false`

## Quick Start

### Prerequisites

- Rust 1.70+ (install via [rustup](https://rustup.rs/))
- PostgreSQL 14+
- NATS Server 2.9+
- Access to an upstream wallet-indexer instance

### Configuration

Create a `.env` file in the `sidecar/` directory:

```bash
# Required
DATABASE_URL=postgres://user:password@localhost:5432/nocy_wallet
NATS_URL=nats://localhost:4222
UPSTREAM_INDEXER_URL=http://localhost:3000

# Optional (with defaults)
SERVER_HOST=0.0.0.0
SERVER_PORT=8080
LIMIT_BLOCKS_MAX=100
UPSTREAM_TIMEOUT_SECS=30
HEARTBEAT_INTERVAL_SECS=15
POLL_INTERVAL_SECS=1
SSE_SEND_TIMEOUT_SECS=5
DB_ACQUIRE_TIMEOUT_SECS=5
MERKLE_CACHE_ENABLED=true

# Optional: For persistent session IDs across restarts
# SERVER_SECRET=<64-char-hex-string>
```

### Build & Run

```bash
cd sidecar

# Build
cargo build --release

# Run
cargo run --release
```

### Database Setup

The sidecar automatically creates its schema (`nocy_sidecar`) on startup. No manual migration is required.

## API Overview

Full API documentation: [sidecar/API.md](sidecar/API.md)

### Health & Monitoring

| Endpoint | Description |
|----------|-------------|
| `GET /healthz` | Liveness probe |
| `GET /readyz` | Readiness probe (checks Postgres + NATS) |
| `GET /metrics` | Prometheus metrics |

### Session Management

| Endpoint | Description |
|----------|-------------|
| `POST /v1/session/bootstrap` | Create session with viewing key, dust key, and address |
| `POST /v1/session/disconnect` | Disconnect session |
| `PUT /v1/session/:id/profile` | Update session profile |

### Feed Endpoints

| Endpoint | Description |
|----------|-------------|
| `GET /v1/feed` | Polling feed with block bundles |
| `GET /v1/feed/subscribe` | SSE streaming feed |

### Zswap Endpoints

| Endpoint | Description |
|----------|-------------|
| `GET /v1/zswap/first-free` | First free commitment tree index |
| `GET /v1/zswap/collapsed-update` | Collapsed merkle state for height range |

## Testing

```bash
cd sidecar

# Run unit tests (no external dependencies)
cargo test --test sequence_tests --test config_tests --test error_tests --test feed_tests

# Run integration tests (requires running sidecar)
# First start the sidecar, then:
cargo test --test integration_tests
```

## Project Structure

```
nocy-wallet-indexer/
├── sidecar/                    # Main Rust application
│   ├── src/
│   │   ├── main.rs             # Application entry point
│   │   ├── lib.rs              # Library exports
│   │   ├── config.rs           # Configuration management
│   │   ├── error.rs            # Error types
│   │   ├── feed.rs             # Feed types and serialization
│   │   ├── sequence.rs         # Cursor parsing and ordering
│   │   ├── upstream.rs         # GraphQL client for wallet-indexer
│   │   ├── snapshot.rs         # NATS client for ledger state
│   │   ├── graphql_ws.rs       # WebSocket subscriptions
│   │   ├── merkle_cache.rs     # Background merkle caching
│   │   ├── keepalive.rs        # Session keepalive
│   │   ├── metrics.rs          # Prometheus metrics
│   │   ├── schema.rs           # Database schema management
│   │   ├── db.rs               # Database queries
│   │   └── routes/             # HTTP route handlers
│   │       ├── session.rs      # Session management
│   │       ├── feed.rs         # Feed polling
│   │       ├── subscribe.rs    # SSE streaming
│   │       └── zswap.rs        # Zswap endpoints
│   ├── migrations/             # SQL migrations
│   ├── tests/                  # Integration tests
│   ├── Cargo.toml
│   └── API.md                  # API documentation
├── tests/
│   └── unit/                   # Unit tests
│       ├── sequence_tests.rs
│       ├── config_tests.rs
│       ├── error_tests.rs
│       └── feed_tests.rs
└── README.md
```

## Key Features

- **Unified Feed**: Single API for shielded transactions, nullifiers, dust events, and unshielded UTXOs
- **SSE Streaming**: Real-time block updates via Server-Sent Events
- **Deterministic Session IDs**: Session IDs derived from credentials + server secret
- **Merkle Caching**: Background caching of collapsed merkle updates for fast historical queries
- **Prometheus Metrics**: Built-in observability
- **Graceful Shutdown**: Clean shutdown with connection draining

## Environment Variables

| Variable | Default | Description |
|----------|---------|-------------|
| `DATABASE_URL` | *required* | PostgreSQL connection URL |
| `NATS_URL` | *required* | NATS server URL |
| `UPSTREAM_INDEXER_URL` | *required* | Upstream wallet-indexer URL |
| `SERVER_HOST` | `0.0.0.0` | Server bind address |
| `SERVER_PORT` | `8080` | Server port |
| `SERVER_SECRET` | *random* | 32-byte hex secret for session ID generation |
| `LIMIT_BLOCKS_MAX` | `100` | Max blocks per feed request |
| `UPSTREAM_TIMEOUT_SECS` | `30` | Upstream request timeout |
| `HEARTBEAT_INTERVAL_SECS` | `15` | SSE heartbeat interval |
| `POLL_INTERVAL_SECS` | `1` | SSE poll interval |
| `SSE_SEND_TIMEOUT_SECS` | `5` | Timeout for slow SSE clients |
| `DB_ACQUIRE_TIMEOUT_SECS` | `5` | Database connection acquire timeout |
| `MERKLE_CACHE_ENABLED` | `true` | Enable background merkle caching |

## Data Flow: Indexer vs Sidecar

The sidecar aggregates data from multiple upstream sources into a unified, wallet-centric feed. This eliminates the need for clients to make multiple queries and join data themselves.

### Data Sources

```
┌─────────────────────────────────────────────────────────────────────────────┐
│                        UPSTREAM DATA SOURCES                                │
├─────────────────────────────────────────────────────────────────────────────┤
│                                                                             │
│  PostgreSQL (Shared with Wallet-Indexer)                                    │
│  ├── blocks                    → Block metadata (height, hash, timestamp)   │
│  ├── transactions              → All transactions (hash, protocol_version)  │
│  ├── regular_transactions      → Fees, merkle indices, identifiers          │
│  ├── wallets                   → Session tracking, last_indexed_tx_id       │
│  ├── relevant_transactions     → Shielded txs relevant to viewing key       │
│  ├── ledger_events             → Raw Zswap/Dust events (grouping enum)      │
│  ├── contract_actions          → Contract deploy/call/update actions        │
│  ├── unshielded_utxos          → UTXO creates/spends for addresses          │
│  └── transaction_identifiers   → Serialized tx identifiers                  │
│                                                                             │
│  NATS                                                                       │
│  └── ledger_state subject      → Real-time snapshots (merkle root,          │
│                                   first_free_index, commitment_count)       │
│                                                                             │
│  Upstream Wallet-Indexer (GraphQL + WebSocket)                              │
│  ├── connect/disconnect        → Session management                         │
│  └── shieldedTransactions      → WebSocket subscription for merkle updates  │
│                                                                             │   
└─────────────────────────────────────────────────────────────────────────────┘
                                    │
                                    ▼
┌─────────────────────────────────────────────────────────────────────────────┐
│                         SIDECAR AGGREGATION                                 │
├─────────────────────────────────────────────────────────────────────────────┤
│                                                                             │
│  Per-Request Aggregation (GET /v1/feed):                                    │
│  1. Resolve local session → upstream session mapping                        │
│  2. Compute wallet-ready height (last_indexed_tx_id check)                  │
│  3. Fetch block metadata for range                                          │
│  4. Detect reorgs (parent hash validation)                                  │
│  5. Query relevant_transactions (shielded txs for viewing key)              │
│  6. Query ledger_events (Zswap events for relevant txs)                     │
│  7. Query ledger_events (Dust events for dust_public_key)                   │
│  8. Query unshielded_utxos (for registered addresses)                       │
│  9. Query contract_actions (for ALL wallet-relevant txs)                    │
│  10. Query transaction metadata (fees, merkle, identifiers, raw)            │
│  11. Query global ZswapInput events (for nullifier streaming)               │
│  12. Sort all items by sequence (height:tx_id:phase:ordinal)                │
│  13. Bundle into blocks with watermarks                                     │
│                                                                             │
└─────────────────────────────────────────────────────────────────────────────┘
                                    │
                                    ▼
┌─────────────────────────────────────────────────────────────────────────────┐
│                         UNIFIED FEED OUTPUT                                 │
├─────────────────────────────────────────────────────────────────────────────┤
│                                                                             │
│  FeedResponse {                                                             │
│    watermarks: { chainHead, walletReadyHeight, finalizedHeight }            │
│    blocks: [                                                                │
│      {                                                                      │
│        meta: { height, hash, parentHash, timestamp }                        │
│        items: [                                                             │
│          { type: "transactionMeta", data: { txId, fees, merkle, ... } }     │
│          { type: "shieldedRelevantTx", data: { zswapEvents, ... } }         │
│          { type: "zswapInputRaw", data: { raw } }  // for spend detection   │
│          { type: "dustLedgerEvent", data: { raw } }                         │
│          { type: "unshieldedUtxo", data: { address, value, ... } }          │
│          { type: "control", data: { resetRequired, ... } }                  │
│        ]                                                                    │
│      }                                                                      │
│    ]                                                                        │
│    nextHeight, nextCursor                                                   │
│  }                                                                          │
│                                                                             │
└─────────────────────────────────────────────────────────────────────────────┘
```

### What the Sidecar Adds

| Feature | Without Sidecar | With Sidecar |
|---------|-----------------|--------------|
| **Session Management** | Client calls upstream GraphQL directly | Single bootstrap endpoint, deterministic session IDs |
| **Shielded Transactions** | Query `relevant_transactions` + join `ledger_events` | Pre-joined `shieldedRelevantTx` with `zswapEvents` array |
| **Transaction Metadata** | Separate query to `regular_transactions` | Inline `transactionMeta` with fees, merkle, identifiers |
| **Contract Actions** | Separate query to `contract_actions` | Attached to each relevant feed item |
| **Dust Events** | Query `ledger_events` filtered by dust key | Pre-filtered `dustLedgerEvent` items |
| **Unshielded UTXOs** | Query `unshielded_utxos` by address | Pre-filtered `unshieldedUtxo` with full identity |
| **Nullifier Streaming** | Query ALL `ZswapInput` events globally | Optional `zswapInputRaw` items for client-side parsing |
| **Merkle Updates** | WebSocket subscription per client | Background caching, inline collapsed updates |
| **Reorg Detection** | Client must track parent hashes | Automatic `control.resetRequired` events |
| **Watermarks** | Multiple queries for chain state | Single `watermarks` object per response |
| **Ordering** | Client must sort by block + tx + event | Pre-sorted by `sequence` cursor |
| **Pagination** | Height-based only | Unified cursor (`height:txId:phase:ordinal`) |

### Data Transformations

The sidecar performs several data transformations to normalize inputs and format outputs:

| Transformation | Input | Output | Purpose |
|----------------|-------|--------|---------|
| **Viewing Key Normalization** | 32-byte hex OR bech32m | Canonical bech32m (`mn_shield-esk_undeployed`) | Upstream indexer requires bech32m |
| **Session ID Generation** | Credentials + server_secret | 32-byte BLAKE3 hash (hex) | Deterministic, server-specific IDs |
| **BYTEA → Hex** | Database binary fields | Hex strings | JSON-safe encoding |
| **i64 → String** | Large integers | Decimal strings | JavaScript `MAX_SAFE_INTEGER` safety |
| **Fees Decoding** | Big-endian BYTEA | u128 → decimal string | Human-readable fee amounts |

**Viewing Key Flow:**
```
Client Input                    Sidecar                         Upstream Indexer
─────────────────────────────────────────────────────────────────────────────────
"0x1234...abcd" (hex)    →    normalize_viewing_key()    →    "mn_shield-esk_undeployed1..."
       OR                            ↓
"mn_shield-esk_..."      →    validate & re-encode       →    canonical bech32m
```

**Session ID Flow:**
```
viewing_key + dust_key + address + SERVER_SECRET
                    ↓
            BLAKE3 keyed hash
                    ↓
        32-byte session_id (hex-encoded)
```

### Database Tables Used

**Upstream Schema (read-only):**
- `blocks` - Block metadata
- `transactions` - Transaction base data
- `regular_transactions` - Fees, merkle indices, result
- `wallets` - Session tracking
- `relevant_transactions` - Shielded tx relevance
- `ledger_events` - Raw Zswap/Dust event bytes
- `contract_actions` - Contract interactions
- `unshielded_utxos` - UTXO lifecycle
- `transaction_identifiers` - Serialized identifiers (optional table)

**Sidecar Schema (`nocy_sidecar`):**
- `session_profile` - Local session → upstream session mapping, dust key, nullifier preference
- `session_unshielded_address` - Registered unshielded addresses per session
- `block_max_regular_tx` - Cache for wallet-ready height computation
- `collapsed_merkle_cache` - Cached collapsed merkle updates

## License

Licensed under the Apache License, Version 2.0. See [LICENSE](LICENSE) for details.

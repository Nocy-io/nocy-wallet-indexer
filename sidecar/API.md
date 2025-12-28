# Nocy Wallet Feed API Documentation

## Overview

The Nocy Wallet Feed sidecar provides a unified REST + SSE API for wallet synchronization. It serves block data, transaction metadata, shielded transactions, nullifiers, dust events, and unshielded deltas filtered by session.

**Base URL:** `http://localhost:8080` (configurable via `SERVER_HOST` and `SERVER_PORT`)

**API Version:** `v1`

---

## Health & Readiness

### GET /healthz

Returns service health status.

**Response:**
```json
{
  "status": "ok"
}
```

### GET /readyz

Returns readiness status with dependency checks.

**Response (200 OK):**
```json
{
  "status": "ready",
  "checks": {
    "postgres": true,
    "nats": true
  }
}
```

**Response (503 Service Unavailable):**
```json
{
  "status": "not_ready",
  "checks": {
    "postgres": false,
    "nats": true
  }
}
```

### GET /metrics

Returns Prometheus metrics for monitoring.

---

## Session Management

### POST /v1/session/bootstrap

Creates a new session with the upstream indexer and persists the session profile.

**Request:**
```json
{
  "viewingKey": "mn_shield-esk_undeployed1...",
  "dustPublicKey": "0a1b2c3d4e5f...",
  "unshieldedAddress": "1234567890abcdef...",
  "nullifierStreaming": "enabled"
}
```

| Field | Type | Required | Description |
|-------|------|----------|-------------|
| `viewingKey` | string | Yes | Bech32m-encoded viewing key (HRP `mn_shield-esk_undeployed`) or 32-byte hex |
| `dustPublicKey` | string | Yes | Hex-encoded 32-byte dust public key |
| `unshieldedAddress` | string | Yes | Hex-encoded 32-byte unshielded address |
| `nullifierStreaming` | string | No | Nullifier streaming preference: `"enabled"` (default) or `"disabled"` |

**Response (200 OK):**
```json
{
  "sessionId": "a1b2c3d4e5f6..."
}
```

| Field | Type | Description |
|-------|------|-------------|
| `sessionId` | string | Hex-encoded 32-byte *local* session ID (used by `/v1/feed`, `/v1/feed/subscribe`, `/v1/zswap/merkle-update`, and profile updates) |

**Note:** This `sessionId` is server-derived (using `SERVER_SECRET`) and may differ from the upstream indexer session ID.

**Errors:**
- `400 Bad Request` - Invalid viewing key or dust public key format
- `502 Bad Gateway` - Upstream indexer connection failed

---

### POST /v1/session/connect

Proxy-only connection to upstream indexer (no profile persistence).
Returns the **upstream** session ID.

**Request:**
```json
{
  "viewingKey": "mn_shield-esk_undeployed1..."
}
```

**Response (200 OK):**
```json
{
  "sessionId": "a1b2c3d4e5f6..."
}
```

**Note:** This endpoint does **not** create a sidecar session profile, so the returned session ID is not valid for `/v1/feed` in sidecar11. Use `/v1/session/bootstrap` instead.

---

### POST /v1/session/disconnect

Disconnects a session from the upstream indexer.

**Request:**
```json
{
  "sessionId": "a1b2c3d4e5f6..."
}
```

**Response (200 OK):**
```json
{
  "success": true
}
```

---

### PUT /v1/session/:sessionId/profile

Updates session profile (dust key and/or unshielded addresses).

**URL Parameters:**
- `sessionId` - Hex-encoded 32-byte local session ID (from `/v1/session/bootstrap`)

**Request:**
```json
{
  "dustPublicKey": "0a1b2c3d4e5f...",
  "unshieldedAddresses": [
    "1234567890abcdef...",
    "fedcba0987654321..."
  ],
  "nullifierStreaming": "enabled"
}
```

| Field | Type | Required | Description |
|-------|------|----------|-------------|
| `dustPublicKey` | string | No | Hex-encoded 32-byte dust public key |
| `unshieldedAddresses` | string[] | No | Array of hex-encoded 32-byte addresses |
| `nullifierStreaming` | string | No | Nullifier streaming preference: `"enabled"` (default) or `"disabled"` |

**Response (200 OK):**
```json
{
  "success": true,
  "addressesChanged": true,
  "restartFromHeight": "0",
  "nullifierStreaming": "enabled"
}
```

| Field | Type | Description |
|-------|------|-------------|
| `success` | boolean | Whether update succeeded |
| `addressesChanged` | boolean | Whether addresses changed (requires resync) |
| `restartFromHeight` | string | If addresses changed, restart feed from this height (decimal string) |
| `nullifierStreaming` | string | Current nullifier streaming preference after update |

**Note:** If `addressesChanged` is `true`, the client MUST restart the feed subscription from `restartFromHeight` to receive historical unshielded deltas for new addresses.

---

## Feed Endpoints

### GET /v1/feed

Fetches feed blocks with watermarks (polling mode).

**Query Parameters:**

| Parameter | Type | Required | Default | Description |
|-----------|------|----------|---------|-------------|
| `sessionId` | string | Yes | - | Hex-encoded 32-byte local session ID (from `/v1/session/bootstrap`) |
| `fromHeight` | number | No | 0 | Starting block height |
| `fromCursor` | string | No | - | Starting sequence cursor (overrides `fromHeight`) |
| `limitBlocks` | number | No | 50 | Max blocks to return (capped by config) |
| `includeNullifiers` | boolean | No | true | Include `zswapInputRaw` events for spend detection |
| `includeTxMeta` | boolean | No | true | Include `transactionMeta` items per wallet-relevant tx |
| `includeRawTx` | boolean | No | false | Include raw tx bytes in `transactionMeta` |
| `includeIdentifiers` | boolean | No | true | Include identifiers array in `transactionMeta` |

**Example Request:**
```
GET /v1/feed?sessionId=a1b2c3d4...&fromHeight=100&limitBlocks=10&includeNullifiers=true&includeTxMeta=true&includeRawTx=false&includeIdentifiers=true
```

**Response (200 OK):**
```json
{
  "apiVersion": "v1",
  "watermarks": {
    "chainHead": "1000",
    "walletReadyHeight": "950",
    "finalizedHeight": "1000"
  },
  "blocks": [
    {
      "meta": {
        "height": "100",
        "hash": "abc123...",
        "parentHash": "def456...",
        "timestamp": "1703123456"
      },
      "items": [
        {
          "sequence": "100:12345:0:0",
          "type": "transactionMeta",
          "data": {
            "txId": "12345",
            "txHash": "789abc...",
            "blockHeight": "100",
            "protocolVersion": "2",
            "transactionResult": { "status": "success" },
            "identifiers": ["identifier_hex_1..."],
            "fees": {
              "paid": "0",
              "estimated": "0"
            },
            "merkle": {
              "root": "merkle_root_hex...",
              "startIndex": "10",
              "endIndex": "12"
            },
            "raw": null
          }
        },
        {
          "sequence": "100:12345:0:1",
          "type": "shieldedRelevantTx",
          "data": {
            "txId": "12345",
            "txHash": "789abc...",
            "blockHeight": "100",
            "feesPaid": "0",
            "zswapEvents": ["raw_hex_1...", "raw_hex_2..."],
            "contractActions": [
              {
                "contractAddress": "contract_hex...",
                "actionType": "Call"
              }
            ]
          }
        },
        {
          "sequence": "100:12346:1:5679",
          "type": "zswapInputRaw",
          "data": {
            "txId": "12346",
            "txHash": "789def...",
            "ledgerEventId": "5679",
            "blockHeight": "100",
            "raw": "raw_zswap_input_event_hex..."
          }
        },
        {
          "sequence": "100:12347:1:5678",
          "type": "dustLedgerEvent",
          "data": {
            "txId": "12347",
            "ledgerEventId": "5678",
            "blockHeight": "100",
            "raw": "raw_event_hex..."
          }
        },
        {
          "sequence": "100:12348:2:999",
          "type": "unshieldedUtxo",
          "data": {
            "txId": "12348",
            "txHash": "aaaabbbb...",
            "blockHeight": "100",
            "address": "address_hex...",
            "isCreate": true,
            "intentHash": "deadbeef...",
            "outputIndex": "0",
            "tokenType": "token_hex...",
            "value": "1000000",
            "initialNonce": "nonce_hex...",
            "registeredForDustGeneration": false,
            "ctime": "1703123456789",
            "feesPaid": "0",
            "contractAddress": null,
            "entryPoint": null
          }
        }
      ]
    }
  ],
  "nextHeight": "101",
  "nextCursor": "100:12348:2:999"
}
```

**Watermarks:**

| Field | Type | Description |
|-------|------|-------------|
| `chainHead` | string | Latest block height in the chain (decimal string) |
| `walletReadyHeight` | string\|null | Highest height where wallet indexing is complete (decimal string) |
| `finalizedHeight` | string | Finalized block height (equals chainHead since indexer only processes finalized blocks) |

**Response Fields:**
- `nextHeight`: next block height cursor (block pagination)
- `nextCursor`: optional unified sequence cursor (item pagination)

**Feed Item Types:**

| Type | Description |
|------|-------------|
| `transactionMeta` | Per-transaction metadata (one per wallet-relevant tx) |
| `shieldedRelevantTx` | Transaction relevant to this wallet's viewing key |
| `zswapInputRaw` | Global ZswapInput event raw bytes for client-side spend detection |
| `dustLedgerEvent` | Dust ledger event matching session's dust key |
| `unshieldedUtxo` | Unshielded UTXO create/spend event with full identity |
| `control` | Control event (e.g., reset required) |

Each feed item includes a `sequence` field, a colon-separated cursor:
`blockHeight:txId:phase:ordinal`. Use this for a unified, deterministic order
and to resume with `fromCursor`.

**Cursor Compatibility:** When resuming with `fromCursor`, the `includeTxMeta`,
`includeRawTx`, and `includeIdentifiers` flags must match the original request.
Changing these flags can change item ordering within a block and may cause gaps
or duplicates.

**transactionMeta:**
- Emitted once per wallet-relevant transaction (shielded relevant, dust, or unshielded UTXO).
- System transactions may have `transactionResult` null; `fees`/`merkle` subfields and `identifiers` may be null.
- `raw` is only populated when `includeRawTx=true`.

**Note:** `unshieldedUtxo` supersedes the older `unshieldedDelta` format by
including full UTXO identity and transaction metadata.

**Nullifier Streaming:**

The `zswapInputRaw` items contain raw ZswapInput event bytes that clients parse using the midnight-ledger SDK to extract nullifiers for spend detection. Two approaches exist:

1. **Global nullifier streaming** (default): Enable `includeNullifiers=true`, parse raw events client-side
2. **0-value change outputs**: Use wallet-side spend detection, disable streaming with `includeNullifiers=false`

Parsed nullifier arrays are not emitted by the server; only raw ZswapInput events are streamed.

Set `nullifierStreaming` in session profile or override per-request with `includeNullifiers` query param.

**Control Event (Reset Required):**
```json
{
  "type": "control",
  "data": {
    "type": "resetRequired",
    "safeRestartHeight": "99",
    "reason": "Reorg detected at height 100: parent hash mismatch"
  }
}
```

---

### GET /v1/feed/subscribe

SSE stream for real-time feed updates.

**Query Parameters:**

| Parameter | Type | Required | Default | Description |
|-----------|------|----------|---------|-------------|
| `sessionId` | string | Yes | - | Hex-encoded 32-byte local session ID (from `/v1/session/bootstrap`) |
| `fromHeight` | number | No | 0 | Starting block height |
| `fromCursor` | string | No | - | Starting sequence cursor (overrides `fromHeight`) |
| `includeNullifiers` | boolean | No | true | Include `zswapInputRaw` events for spend detection |
| `includeTxMeta` | boolean | No | true | Include `transactionMeta` items per wallet-relevant tx |
| `includeRawTx` | boolean | No | false | Include raw tx bytes in `transactionMeta` |
| `includeIdentifiers` | boolean | No | true | Include identifiers array in `transactionMeta` |

**Example Request:**
```
GET /v1/feed/subscribe?sessionId=a1b2c3d4...&fromHeight=100&includeNullifiers=true&includeTxMeta=true&includeRawTx=false&includeIdentifiers=true
```

**SSE Event Types:**

1. **heartbeat** - Progress update while waiting for new data
```
event: heartbeat
data: {"type":"heartbeat","data":{"watermarks":{"chainHead":"1000","walletReadyHeight":"950","finalizedHeight":"1000"},"currentHeight":"100"}}
```

2. **block** - Block bundle with feed items
```
event: block
data: {"type":"block","data":{"meta":{"height":"100",...},"items":[...]}}
```

3. **control** - Control event (reorg, reset required)
```
event: control
data: {"type":"control","data":{"type":"resetRequired","safeRestartHeight":"99","reason":"..."}}
```

4. **error** - Error event (invalid session, etc.)
```
event: error
data: Invalid session_id format
```

**Connection Headers:**
- `Content-Type: text/event-stream`
- `Cache-Control: no-cache`

---

## Zswap Endpoints

### GET /v1/zswap/first-free

Returns the first free index in the commitment tree.

**Response (200 OK):**
```json
{
  "firstFreeIndex": "1000",
  "blockHeight": "500",
  "source": "snapshot"
}
```

| Field | Type | Description |
|-------|------|-------------|
| `firstFreeIndex` | string | Decimal string of the first free index |
| `blockHeight` | string | Block height this value is valid for (decimal string) |
| `source` | string | Data source: `"snapshot"` or `"db"` |

---

### GET /v1/zswap/collapsed-update

Returns collapsed zswap state for a height range.

**Query Parameters:**

| Parameter | Type | Required | Description |
|-----------|------|----------|-------------|
| `fromHeight` | number | Yes | Start height (inclusive, >= 0) |
| `toHeight` | number | Yes | End height (must equal chain head) |
| `protocolVersion` | number | Yes | Expected protocol version |

**Example Request:**
```
GET /v1/zswap/collapsed-update?fromHeight=0&toHeight=1000&protocolVersion=2
```

**Response (200 OK):**
```json
{
  "blockHeight": "1000",
  "protocolVersion": 2,
  "merkleRoot": "abc123...",
  "firstFreeIndex": "5000",
  "commitmentCount": "4999",
  "timestampMs": "1703123456789"
}
```

**Errors:**
- `400 Bad Request` - `fromHeight >= toHeight`, `fromHeight < 0`, `toHeight != chainHead`, or protocol version mismatch
- `503 Service Unavailable` - Ledger state snapshot not available

---

## Error Responses

All endpoints return errors in the following format:

```json
{
  "error": "Error message here",
  "code": "ERROR_CODE"
}
```

**Common Error Codes:**

| Code | HTTP Status | Description |
|------|-------------|-------------|
| `BAD_REQUEST` | 400 | Invalid request parameters |
| `NOT_FOUND` | 404 | Session or resource not found |
| `UPSTREAM_ERROR` | 502 | Upstream indexer error |
| `SERVICE_UNAVAILABLE` | 503 | Service dependency unavailable |
| `INTERNAL_ERROR` | 500 | Internal server error |

---

## Data Encoding

- **Hashes & Keys**: Hex-encoded (lowercase)
- **Session IDs**: 32 bytes, hex-encoded (64 characters)
- **Addresses**: 32 bytes, hex-encoded (64 characters)
- **Big Integers**: Decimal strings (for JavaScript compatibility)
- **Timestamps**: Unix seconds (block timestamp) or Unix milliseconds (snapshot)

---

## SDK Compatibility

### Client SDK Requirements

The sidecar provides raw ledger event bytes that clients parse using the Midnight Ledger SDK. Ensure SDK version compatibility for correct deserialization.

**Required SDK:**

| Package | Version | Notes |
|---------|---------|-------|
| `@midnight/ledger` | `^6.1.0` | JavaScript/TypeScript SDK for parsing raw event bytes |
| `midnight-ledger` (Rust) | `^6.1.0` | Rust SDK for parsing raw event bytes |

**Event Parsing:**

Raw event bytes use the `event[v6]` tagged serialization format. Parse with:

```typescript
import { deserializeLedgerEvent } from '@midnight/ledger';

// Parse raw ZswapInput event from feed
const eventBytes = Buffer.from(item.raw, 'hex');
const event = deserializeLedgerEvent(eventBytes, protocolVersion);

// Extract nullifier for spend detection
if (event.type === 'ZswapInput') {
  const nullifier = event.nullifier;
  // ... check against local coin nullifiers
}
```

**Protocol Versions:**

| Protocol Version | SDK Support | Notes |
|------------------|-------------|-------|
| 2 | `^6.1.0` | Current mainnet version |

---

## Configuration

Environment variables:

| Variable | Default | Description |
|----------|---------|-------------|
| `DATABASE_URL` | - | PostgreSQL connection URL |
| `SERVER_HOST` | `0.0.0.0` | Server bind address |
| `SERVER_PORT` | `8080` | Server port |
| `UPSTREAM_INDEXER_URL` | - | Upstream wallet-indexer URL |
| `NATS_URL` | - | NATS server URL |
| `LIMIT_BLOCKS_MAX` | `100` | Maximum blocks per feed request |
| `HEARTBEAT_INTERVAL_SECS` | `15` | SSE heartbeat interval (seconds) |
| `POLL_INTERVAL_SECS` | `1` | SSE poll interval (seconds) |
| `SSE_SEND_TIMEOUT_SECS` | `5` | Timeout for slow SSE clients |

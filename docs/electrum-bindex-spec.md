# Electrum-Compatible Server Spec for bindex

Status: draft implementation spec

Primary upstream reference: <https://github.com/spesmilo/electrum-protocol>

Rendered upstream docs used for this draft:

- <https://electrum-protocol.readthedocs.io/en/latest/>
- <https://electrum-protocol.readthedocs.io/en/latest/protocol-basics.html>
- <https://electrum-protocol.readthedocs.io/en/latest/protocol-methods.html>
- <https://electrum-protocol.readthedocs.io/en/latest/protocol-changes.html>

## 1. Goal

Build an Electrum-compatible Bitcoin server that uses `bindex` as its confirmed-chain index and bitcoind as its chain, mempool, relay, and fee-estimation authority.

The server must be usable by standard Electrum clients without client-side changes. It must only advertise protocol versions for which it implements the complete required method set.

## 2. Compatibility Target

The implementation target is Electrum Protocol `1.6.x` from the start. Do not build protocol `1.3` or `1.4` as separate products and then layer newer behavior on top. Implement the latest required method surface, data model, and mempool semantics directly.

Production readiness requires advertising `protocol_max = "1.6"` and passing compatibility tests for every required `1.6` method and response shape. The server should also support older client protocol ranges where the older response shape differs, by negotiating the client's version in `server.version` and formatting responses accordingly.

Recommended production advertisement:

```json
{
  "protocol_min": "1.4",
  "protocol_max": "1.6"
}
```

`protocol_min` may be lowered only if we intentionally implement the removed/deprecated methods and response shapes required by those older protocol versions. In particular, advertising below `1.4` creates legacy obligations such as older address-based methods and deserialized header behavior. `protocol_max` must remain `1.6` until a newer upstream protocol is intentionally implemented.

Do not advertise `protocol_max = "1.6"` until all of the following are true:

- every required method through protocol `1.4.2` is implemented, including nonzero `cp_height` header proofs, `blockchain.transaction.id_from_pos`, and `blockchain.scripthash.unsubscribe`
- `blockchain.block.headers` returns `headers` as an array, not concatenated hex.
- `blockchain.estimatefee` accepts the optional `mode` argument.
- `blockchain.scripthash.get_mempool` and scripthash status use canonical mempool ordering.
- `blockchain.transaction.broadcast_package` is implemented.
- `mempool.get_info` is implemented.
- `blockchain.relayfee` is not advertised as part of the current protocol surface.

Version `1.5` is skipped by the upstream protocol and must not be advertised.

Backwards compatibility rule: older protocol support must be handled as a negotiation and response-formatting concern, not as separate legacy implementations. The backing indexes, mempool model, status calculation, and method handlers should be designed around the latest `1.6` semantics, with thin version-specific serializers only where the upstream protocol requires different output.

Bitcoin-only scope: this project targets Bitcoin networks supported by `bitcoin::Network`. Altcoin-only protocol extensions, including name-index methods, are out of scope and must not be advertised.

## 2.1 Negotiated Version Compatibility

Implement one latest-version core and adapt only the wire shape where negotiated protocol versions require it.

Required compatibility matrix:

| Negotiated version | Required compatibility behavior |
| --- | --- |
| `1.6` | Current target. `blockchain.block.headers` returns `headers` as an array. Mempool ordering is canonical. `estimatefee(number, mode)`, `mempool.get_info`, and `broadcast_package` are available. `blockchain.relayfee` is removed. |
| `1.4` - `1.4.3` | Support nonzero `cp_height` header proofs. `blockchain.block.headers` uses the pre-`1.6` response shape, which contains concatenated header hex instead of a `headers` array. `blockchain.relayfee` remains available for these negotiated versions unless `protocol_min` is raised to `1.6`. |
| `1.3` and below | Optional legacy support only. Do not advertise this range unless we implement the removed/deprecated methods and older response shapes those clients may use. |

The default supported range is therefore `1.4` through `1.6`. This gives practical backwards compatibility for modern Electrum clients without committing the first implementation to obsolete address-based APIs.

## 3. Architecture

### 3.1 Components

- `electrum-server`: async TCP server implementing newline-delimited JSON-RPC.
- `session`: per-client state, including negotiated version and active subscriptions.
- `router`: validates JSON-RPC messages and dispatches methods.
- `chain_index`: wrapper around `bindex::IndexedChain` for confirmed data.
- `mempool_index`: in-memory or persistent mempool view derived from bitcoind.
- `bitcoind_client`: JSON-RPC and REST client for fee estimates, mempool, relay, block data, and fallback lookups.
- `notifier`: broadcasts header and scripthash subscription changes to sessions.

### 3.2 Backend Prerequisites

Required bitcoind capabilities:

- Bitcoin Core version new enough for the bindex REST endpoints already required by this repository, including `/rest/spenttxouts/` and `/rest/blockpart/`
- REST enabled
- JSON-RPC enabled
- enough historical block data available to build and maintain the bindex database
- package relay support sufficient for `submitpackage` before advertising `blockchain.transaction.broadcast_package`

Recommended bitcoind configuration:

- run unpruned for initial indexing and public service operation
- enable ZMQ block and transaction notifications if low-latency mempool/header updates are desired
- if using verbose confirmed transaction responses through bitcoind, call bitcoind with the known block hash from bindex so global `txindex` is not required

Startup checks must fail fast if required REST/RPC methods are unavailable, if the bitcoind network does not match the configured `bitcoin::Network`, or if bindex and bitcoind disagree on genesis or active-chain headers.

### 3.3 Data Ownership

`bindex` owns:

- confirmed script-hash history location lookup
- confirmed txid location lookup
- confirmed transaction byte lookup by indexed location
- current active-chain headers and reorg handling
- block/transaction position mapping needed for efficient confirmed tx retrieval

Required bindex-facing API additions or wrappers:

- get active-chain header by height
- get active-chain block hash by height
- expose transaction position within block for a `Location`
- expose transaction count or txid list for a block, or provide an efficient helper for merkle proof construction
- expose enough reorg information for Electrum caches to invalidate from the fork height
- expose or derive spent-output metadata needed to validate script-hash spending hits

`bitcoind` owns:

- mempool contents and mempool ancestry
- transaction broadcast and package relay
- fee estimation
- relay-fee information
- raw blocks and merkle data when not already available in bindex
- verbose transaction responses

The Electrum server owns:

- JSON-RPC transport and error handling
- protocol version negotiation
- response shape compatibility
- subscription state
- scripthash status calculation
- result caching and rate limiting

## 4. Transport

### 4.1 TCP

Support plain TCP first. Default mainnet port should be configurable and should default to `50001` if running as an Electrum-compatible public service.

Each request and response over TCP must be a single JSON object followed by one newline byte (`\n`). Incoming lines above the configured maximum request size must be rejected and the connection should be closed.

### 4.2 TLS

TLS is required for public compatibility, but can be a second milestone. Default TLS port should be configurable and should default to `50002`.

### 4.3 WebSocket

WebSocket support is optional. If implemented, JSON-RPC messages are WebSocket frames and must not be newline-delimited inside the frame.

### 4.4 JSON-RPC Mode

Support JSON-RPC 2.0 requests and responses. The implementation may also tolerate JSON-RPC 1.0-style requests that omit `jsonrpc`, because Electrum clients and compatible tools vary.

If the server includes `"jsonrpc": "2.0"` in responses, it must implement JSON-RPC 2.0 semantics consistently:

- support both positional params and named params, using the upstream parameter names
- support bounded batch requests
- preserve request IDs exactly, including string and numeric IDs
- return no response for valid client notifications that omit `id`
- reject malformed batch entries individually where possible
- preserve response ordering for batches when practical, but clients must match by `id`

Batch limits:

- maximum batch item count is configurable
- maximum aggregate response size is configurable
- expensive methods inside batches count against the same per-session and per-IP rate limits as standalone calls
- subscription methods in a batch are allowed but each subscription still mutates session state independently

Requests:

```json
{"jsonrpc":"2.0","id":1,"method":"server.version","params":["client",["1.4","1.6"]]}
```

Success:

```json
{"jsonrpc":"2.0","id":1,"result":["bindex-electrum 0.1.0","1.6"]}
```

Notification:

```json
{"jsonrpc":"2.0","method":"blockchain.headers.subscribe","params":[{"height":840000,"hex":"..."}]}
```

Error:

```json
{"jsonrpc":"2.0","id":1,"error":{"code":-32602,"message":"invalid params"}}
```

Use JSON-RPC standard error codes where possible:

- `-32700`: parse error
- `-32600`: invalid request
- `-32601`: method not found
- `-32602`: invalid params
- `-32603`: internal error

Use implementation-specific positive codes for bitcoind relay errors and backend failures when useful.

## 5. Version Negotiation

`server.version` must be the first message on a connection for protocol `1.6` compatibility. For strictness and simpler session state, require it as the first method for all advertised versions.

Signature:

```text
server.version(client_name = "", protocol_version = "1.4")
```

Parameter handling:

- `client_name`: optional string.
- `protocol_version`: either one string or `[protocol_min, protocol_max]`.
- Extra arguments must be ignored for `1.6` compatibility.

Negotiation:

```text
selected = min(client_protocol_max, server_protocol_max)
if selected < max(client_protocol_min, server_protocol_min):
    close connection after error or no response
```

Version comparison must parse dotted version strings numerically by component. Do not compare protocol versions lexicographically; for example, `1.10` is greater than `1.6`.

Result:

```json
["bindex-electrum 0.1.0", "1.6"]
```

Reject any later `server.version` call on the same connection.

## 6. Script Hashes

Script hashes are the reversed hexadecimal SHA256 hash of the raw `scriptPubKey` bytes. `bindex::ScriptHash` already matches this convention.

Validation:

- must be exactly 64 lowercase or uppercase hex characters
- decode into 32 bytes
- store internally as the same byte order used by `bindex::ScriptHash`
- normalize output and cache keys to lowercase hex

## 7. Scripthash Status

Status is the SHA256 hex digest of the concatenation:

```text
tx_hash:height:tx_hash:height:...
```

Rules:

- Include confirmed and mempool transactions touching the script hash.
- Confirmed transactions are ordered by increasing height, then increasing transaction position within the block.
- Mempool transactions use height `0` when all inputs are confirmed.
- Mempool transactions use height `-1` when at least one input is unconfirmed.
- For protocol `1.6`, order mempool transactions by `(-height, tx_hash)`, so height `0` entries precede `-1` entries, then txid ascending by display hex.
- Return `null` when the script hash has no history.

This status is used by:

- `blockchain.scripthash.subscribe`
- scripthash subscription notifications
- server-side change detection

## 8. Method Surface

### 8.1 Server Methods

#### `server.version(client_name = "", protocol_version = "1.4")`

Backend: session only.

Return `[server_software_version, negotiated_protocol_version]`.

#### `server.features()`

Backend: `chain_index` plus config.

Return:

```json
{
  "hosts": {
    "example.com": {
      "tcp_port": 50001,
      "ssl_port": 50002
    }
  },
  "genesis_hash": "...",
  "hash_function": "sha256",
  "server_version": "bindex-electrum 0.1.0",
  "protocol_min": "1.4",
  "protocol_max": "1.6",
  "pruning": null
}
```

For private/local deployments, `hosts` may be empty or use configured local host names. `genesis_hash` must match the active network. `protocol_min` and `protocol_max` must match the configured negotiation range, not just the implementation's preferred version.

#### `server.banner()`

Backend: config.

Return a configured string. Default:

```text
bindex electrum server
```

#### `server.donation_address()`

Backend: config.

Return configured donation address, or empty string.

#### `server.peers.subscribe()`

Backend: config.

Despite the name, this is not a real subscription. Return configured peers, or `[]` for private deployments.

#### `server.ping()`

Backend: session only.

Return `null`.

#### `server.add_peer(features)`

Backend: optional peer manager.

For private deployments return `false`. If public peer discovery is implemented later, validate peer features and perform out-of-band checks before advertising it.

### 8.2 Header and Block Methods

#### `blockchain.headers.subscribe()`

Backend: `chain_index`, `notifier`.

Return current tip:

```json
{"height": 840000, "hex": "<80-byte-header-hex>"}
```

Register the session for tip updates. Notifications use the same object as the only parameter.

On reorg, send the new active tip. Clients are responsible for finding the common ancestor.

#### `blockchain.block.header(height, cp_height = 0)`

Backend: `chain_index`, with bitcoind fallback.

If `cp_height == 0`, return the 80-byte raw block header as hex.

If `cp_height > 0`, return:

```json
{
  "branch": ["..."],
  "header": "...",
  "root": "..."
}
```

Validation:

- `height >= 0`
- `cp_height >= 0`
- if `cp_height > 0`, require `height <= cp_height`
- reject heights above current tip

Implementation note: checkpoint header merkle proofs require a header-merkle tree over all headers up to `cp_height`. This is not currently exposed by `bindex` and must be implemented as part of the `1.6` target because `cp_height` support is required for negotiated protocol versions `1.4` and newer.

#### `blockchain.block.headers(start_height, count, cp_height = 0)`

Backend: `chain_index`, with bitcoind fallback.

For protocol `1.6`, return:

```json
{
  "count": 2,
  "headers": ["<header0>", "<header1>"],
  "max": 2016
}
```

For negotiated versions below `1.6`, return the pre-`1.6` response shape:

```json
{
  "count": 2,
  "hex": "<concatenated-header-hex>",
  "max": 2016
}
```

If `count` and `cp_height` are nonzero, include `root` and `branch` using the same proof semantics as `blockchain.block.header`.

Validation:

- `start_height >= 0`
- `count >= 0`
- cap returned headers to configured `max`, default `2016`
- if `cp_height > 0`, require `start_height + count - 1 <= cp_height`

If fewer headers exist than requested, return only available headers.

### 8.3 Scripthash Methods

#### `blockchain.scripthash.get_history(scripthash)`

Backend: `chain_index`, `mempool_index`.

Return confirmed entries followed by mempool entries:

```json
[
  {"tx_hash":"...","height":840000},
  {"tx_hash":"...","height":0,"fee":1200}
]
```

Confirmed history:

- collect locations with `IndexedChain::locations_by_scripthash`
- post-filter false positives by fetching and parsing each transaction
- include transactions that fund the script hash or spend outputs previously funded by the script hash
- order by height then tx position
- de-duplicate transactions that both spend from and pay to the same script hash

Mempool history:

- append `blockchain.scripthash.get_mempool(scripthash)`

#### `blockchain.scripthash.get_balance(scripthash)`

Backend: `chain_index`, UTXO derivation, `mempool_index`.

Return:

```json
{"confirmed": 100000, "unconfirmed": -2500}
```

Confirmed balance is the sum of currently unspent confirmed outputs for the script hash at the active tip.

Unconfirmed balance is the mempool delta compared to confirmed state:

- add mempool outputs paying to the script hash
- subtract confirmed or mempool outputs spent by mempool transactions
- include chained unconfirmed transactions consistently with the status height rules

#### `blockchain.scripthash.listunspent(scripthash)`

Backend: `chain_index`, UTXO derivation, `mempool_index`.

Return UTXOs in blockchain order:

```json
[
  {"tx_hash":"...","tx_pos":0,"height":840000,"value":50000}
]
```

Rules:

- include confirmed UTXOs not spent by the active chain
- exclude any confirmed UTXO currently spent in mempool
- include mempool outputs paying to the script hash with `height = 0`
- `tx_pos` is the output index
- `value` is satoshis

#### `blockchain.scripthash.get_mempool(scripthash)`

Backend: `mempool_index`.

Return:

```json
[
  {"tx_hash":"...","height":0,"fee":1200},
  {"tx_hash":"...","height":-1,"fee":900}
]
```

Rules:

- include unconfirmed transactions touching the script hash
- height `0` means all inputs are confirmed
- height `-1` means at least one input is unconfirmed
- fee is in satoshis
- for `1.6`, sort by `(-height, tx_hash)`

#### `blockchain.scripthash.subscribe(scripthash)`

Backend: `chain_index`, `mempool_index`, `notifier`.

Return current status or `null`. Register the session for future changes.

Notification:

```json
{
  "jsonrpc": "2.0",
  "method": "blockchain.scripthash.subscribe",
  "params": ["<scripthash>", "<status-or-null>"]
}
```

Notify only when the status changes.

#### `blockchain.scripthash.unsubscribe(scripthash)`

Backend: session only.

Return `true` if the session had an active subscription for the script hash, otherwise `false`.

### 8.4 Transaction Methods

#### `blockchain.transaction.get(tx_hash, verbose = false)`

Backend: `chain_index`, `bitcoind_client`.

If `verbose == false`, return raw transaction hex.

Lookup order:

1. Check confirmed bindex txid index.
2. Check mempool via bitcoind.
3. Return JSON-RPC error if not found.

If `verbose == true`, return the bitcoind verbose response. This should be fetched directly from bitcoind so the shape follows bitcoind behavior.

#### `blockchain.transaction.broadcast(raw_tx)`

Backend: selected by `--broadcast-via` (default `tor`).

- `tor` (default): POST the raw tx hex through the local tor SOCKS5 proxy
  (`--tor-proxy`, default `127.0.0.1:9050`) to an onion push endpoint
  (`--tor-broadcast-url`, default mempool.space's onion `POST /api/tx` for the
  configured network; no default exists for regtest). The hostname is resolved
  by the proxy, so the `.onion` name never hits DNS, and the submitting node is
  decoupled from the local bitcoind. Each push authenticates to the proxy with
  never-repeated SOCKS5 username/password credentials, so tor's
  `IsolateSOCKSAuth` (on by default) gives every broadcast its own circuit
  instead of reusing the previous push's rendezvous circuit; a proxy that will
  not take credentials is rejected rather than silently downgraded to a shared
  circuit. The raw tx is validated and decoded locally first; the returned
  txid is computed locally.
- `bitcoind`: call bitcoind `sendrawtransaction`.

Return txid hex on success. Return a JSON-RPC error on rejection (the push
endpoint's error body, e.g. the upstream `sendrawtransaction` message, is
passed through).

#### `blockchain.transaction.broadcast_package(raw_txs, verbose = false)`

Backend: `bitcoind_client`.

Protocol `1.6` only.

Call bitcoind `submitpackage` or equivalent.

If `verbose == false`, normalize result:

```json
{"success": true}
```

or:

```json
{
  "success": false,
  "errors": [
    {"txid":"...","error":"..."}
  ]
}
```

If `verbose == true`, return the raw bitcoind package result. Only advertise `1.6` if the configured bitcoind supports package relay semantics well enough for production.

#### `blockchain.transaction.get_merkle(tx_hash, height)`

Backend: bitcoind raw block, optionally cached.

Return:

```json
{
  "block_height": 840000,
  "merkle": ["..."],
  "pos": 123
}
```

Validation:

- tx must be confirmed at `height`
- txid at computed position must match `tx_hash`
- merkle branch must reconstruct the block header merkle root
- branch entries are transaction hashes encoded as Electrum display hex strings
- hashing and byte-order handling must be covered by test vectors

Implementation:

- fetch full block or txids for `height`
- find tx position
- construct branch from block txids
- cache per-block merkle trees for hot blocks

#### `blockchain.transaction.id_from_pos(height, tx_pos, merkle = false)`

Backend: bitcoind raw block, optionally cached.

If `merkle == false`, return txid hex.

If `merkle == true`, return:

```json
{
  "tx_hash": "...",
  "merkle": ["..."]
}
```

Validation:

- `height` is in active chain
- `tx_pos` is within the block transaction count
- when `merkle == true`, the returned branch must reconstruct the block header merkle root for the returned `tx_hash`

### 8.5 Fee and Mempool Methods

#### `blockchain.estimatefee(number, mode = null)`

Backend: `bitcoind_client`.

Call bitcoind `estimatesmartfee`. Return BTC per kilobyte as a floating point number. Return `-1` if bitcoind has insufficient data.

For protocol `1.6`, pass `mode` through as bitcoind `estimate_mode` when provided.

#### `mempool.get_fee_histogram()`

Backend: `mempool_index` or `bitcoind_client`.

Return array of `[fee_rate_sat_per_vbyte, cumulative_vsize]` buckets ordered from higher fee rate to lower fee rate.

Bucket sizing is implementation-defined. The output must be stable enough for wallet fee UI use and cheap enough to compute under load.

#### `mempool.get_info()`

Backend: `bitcoind_client`.

Protocol `1.6` only.

Return:

```json
{
  "mempoolminfee": 0.00001000,
  "minrelaytxfee": 0.00001000,
  "incrementalrelayfee": 0.00001000
}
```

Values are BTC per kilobyte and should be derived from bitcoind `getmempoolinfo`, `getnetworkinfo`, and policy fields as needed.

### 8.6 Legacy Negotiated Methods

These are not part of the `1.6` current method surface, but are required if the server advertises older negotiated versions that still include them.

#### `blockchain.relayfee()`

Backend: `bitcoind_client`.

Only available for negotiated protocol versions below `1.6`. Return the minimum relay fee in BTC per kilobyte, matching the legacy method that `mempool.get_info().minrelaytxfee` replaces in `1.6`.

For negotiated protocol `1.6`, return `method not found`.

#### Address-based methods below `1.3`

If `protocol_min` is ever lowered below `1.4`, explicitly implement and test the removed address-based methods from the upstream removed-methods document, or keep `protocol_min >= "1.4"`. Do not silently accept older negotiated versions without those legacy methods.

## 9. Confirmed History and UTXO Derivation

`bindex` stores script-hash prefix hits for both funding outputs and spent prevouts. Because the stored key is a prefix, every scripthash or txid lookup must post-filter false positives.

Electrum queries require more than "which transaction touched this script hash". They also require accurate current UTXOs, spend status, output values, transaction order, and mempool deltas. The Electrum layer must therefore maintain or derive the following confirmed-chain metadata:

- funding outpoint: `txid`, `vout`, script hash, value, block height, transaction position
- confirmed spend: spent outpoint, spending txid, spending block height, spending transaction position, input index
- transaction order: height and transaction position for every returned history entry
- block hash/header by height

This metadata can be built as a separate Electrum cache from bindex scan results, by extending bindex column families, or by another indexed store. The implementation must not rely on ad hoc full-chain rescans per public query.

For a queried script hash:

1. Scan `locations_by_scripthash`.
2. Fetch candidate transaction bytes.
3. Parse transaction.
4. Include the transaction if:
   - any output has `ScriptHash::new(script_pubkey) == queried_scripthash`, or
   - any input spends a known output whose script hash is the queried scripthash.
5. Build a per-query or persistent set of:
   - funding outpoints: `(txid, vout, value, height, tx_pos)`
   - spending outpoints: `(prev_txid, prev_vout, spending_txid, spending_height, spending_tx_pos)`
6. UTXOs are funding outpoints with no confirmed spender, adjusted by mempool spends.

Important validation detail: a spending transaction input does not contain the previous output script or value. To post-filter spending hits correctly, the server must look up the spent outpoint in confirmed funding metadata or in the spent-output data captured during indexing. Transaction bytes alone are not sufficient.

The current `cache::Cache` is address-watch oriented. For a public Electrum server, add a reusable scripthash query cache that does not require pre-registering addresses and that supports arbitrary script hashes.

Recommended cache tables:

```sql
CREATE TABLE scripthash_history (
    script_hash BLOB NOT NULL,
    tx_hash BLOB NOT NULL,
    height INTEGER NOT NULL,
    tx_pos INTEGER NOT NULL,
    PRIMARY KEY (script_hash, height, tx_pos, tx_hash)
);

CREATE TABLE scripthash_utxo (
    script_hash BLOB NOT NULL,
    tx_hash BLOB NOT NULL,
    vout INTEGER NOT NULL,
    height INTEGER NOT NULL,
    tx_pos INTEGER NOT NULL,
    value INTEGER NOT NULL,
    spent_by_tx_hash BLOB,
    spent_height INTEGER,
    spent_tx_pos INTEGER,
    PRIMARY KEY (script_hash, tx_hash, vout)
);

CREATE TABLE scripthash_status (
    script_hash BLOB PRIMARY KEY,
    tip_hash BLOB NOT NULL,
    status TEXT
);

CREATE TABLE outpoint_spend (
    prev_tx_hash BLOB NOT NULL,
    prev_vout INTEGER NOT NULL,
    spending_tx_hash BLOB NOT NULL,
    spending_height INTEGER NOT NULL,
    spending_tx_pos INTEGER NOT NULL,
    input_index INTEGER NOT NULL,
    PRIMARY KEY (prev_tx_hash, prev_vout)
);
```

Cache invalidation:

- store active tip hash/height per cached script hash
- on normal extension, append from last cached height
- on reorg, delete cached rows at and after the fork height
- if fork height is unknown for a cached key, drop the key and rebuild

## 10. Mempool Index

Electrum compatibility requires mempool awareness for history, balance, listunspent, status, and subscriptions.

Maintain a mempool index refreshed from bitcoind:

- poll `getrawmempool true` or use ZMQ plus reconciliation
- fetch raw mempool transactions as needed
- parse each tx
- index outputs by script hash
- index inputs by prevout
- compute fees from bitcoind metadata or from prevout values
- classify each tx height as `0` or `-1`
- resolve prevout values for fee and balance calculation from, in order, mempool outputs, confirmed UTXO metadata, or bitcoind
- track replacement/removal events, including RBF, block inclusion, expiry, and reorg return-to-mempool

Required indexes:

- `txid -> mempool tx`
- `scripthash -> touching mempool txids`
- `prevout -> spending mempool txid`
- `outpoint -> mempool output`
- `txid -> fee_sat, vsize, has_unconfirmed_inputs`

Refresh rules:

- detect added and removed txids
- update all affected scripthash statuses
- when a new block is indexed, reconcile mempool after the chain tip changes
- notification order should be header notification first, then affected scripthash status notifications
- on reconciliation failure, rebuild the mempool index from bitcoind rather than applying partial deltas
- keep a short-lived cache of recently removed mempool txids only for diagnostics; removed transactions must not appear in Electrum results

## 11. Subscriptions

Per session track:

- negotiated protocol version
- subscribed headers: boolean
- subscribed script hashes: map `scripthash -> last_status`
- last activity timestamp

Notify:

- `blockchain.headers.subscribe`: when active tip changes
- `blockchain.scripthash.subscribe`: when status changes due to confirmed chain or mempool changes

Do not send notifications for methods that are not subscriptions.

The server may drop subscriptions under resource pressure. If `unsubscribe` is later called for a dropped subscription, returning `false` is valid.

## 12. Concurrency Model

Use one shared `IndexedChain` owner for syncing and queries. Avoid opening RocksDB from multiple processes or independent owners.

Recommended model:

- one sync task owns write access to `IndexedChain`
- query tasks use read access
- tip updates are published through a watch channel
- mempool refresh is independent but ordered after tip sync
- heavy per-scripthash queries run behind a bounded worker pool

Public servers need explicit limits:

- max connections
- max request line length
- max batch size
- max concurrent expensive scripthash scans
- max response bytes
- per-IP rate limits
- per-session subscription count
- idle timeout, default roughly 10 minutes

## 13. Configuration

Required:

- network: `bitcoin`, `testnet`, `testnet4`, `signet`, or `regtest`
- bindex db path
- bitcoind REST URL
- bitcoind JSON-RPC URL and credentials
- listen TCP address

Optional:

- TLS listen address
- TLS certificate/key
- advertised hosts
- protocol min/max
- max headers per request
- peer list
- banner
- donation address
- cache path
- mempool refresh interval
- rate limit settings

## 14. Validation and Errors

Validate all parameters before backend calls.

Reject:

- unknown methods
- malformed JSON
- invalid JSON-RPC request objects
- missing `server.version` as first message
- unsupported negotiated protocol ranges
- invalid script hashes
- invalid txids
- invalid raw transaction hex
- negative heights
- heights above current tip
- excessive `count`
- invalid `cp_height`
- non-boolean `verbose` or `merkle`
- unsupported named parameters
- oversized batches or oversized responses

Backend errors:

- bitcoind unavailable: JSON-RPC internal/backend error
- transaction not found: method-specific not-found error
- tx relay rejection: JSON-RPC error, except package submission with `success: false` when protocol requires a result object
- inconsistent bindex/bitcoind chain: temporarily fail requests and resync

## 15. Testing Plan

Unit tests:

- JSON-RPC parser and formatter
- JSON-RPC 2.0 batch request handling
- version negotiation edge cases
- numeric dotted-version comparison
- negotiated response serializers for `1.4` through `1.6`
- script-hash parsing and byte order
- status calculation, including mempool order
- merkle branch construction
- header checkpoint proof construction
- error-code mapping
- confirmed outpoint spend reconstruction

Integration tests with regtest bitcoind:

- startup and initial sync
- startup fails on wrong network or missing required bitcoind REST/RPC methods
- `server.version` first-message enforcement
- `server.features` genesis hash
- header subscribe and new-block notification
- nonzero `cp_height` proof verification
- scripthash history for funding, spending, and self-transfer txs
- balance and listunspent before and after spends
- mempool history, balance delta, and status changes
- mempool replacement/removal status changes
- transaction get confirmed and mempool
- broadcast success and rejection
- package broadcast success and rejection
- reorg removes stale history and sends changed statuses

Compatibility tests:

- connect with Electrum wallet in regtest/testnet mode where practical
- run a small Electrum client script against every advertised method
- negotiate `1.4`, `1.4.2`, and `1.6` and verify version-specific response shapes
- compare method outputs against ElectrumX/electrs on the same regtest chain for common cases

## 16. Implementation Milestones

These milestones are organized by subsystem, not by old protocol versions. The server should be designed against the `1.6` method surface from the first implementation pass.

### Milestone 1: Protocol Shell

- TCP JSON-RPC server with newline framing
- session lifecycle and idle timeout
- `server.version` negotiation with `protocol_max = "1.6"`
- version-aware response serializer hooks
- `server.features`, `server.banner`, `server.donation_address`, `server.peers.subscribe`, `server.ping`, `server.add_peer`
- standard JSON-RPC error handling and request validation

### Milestone 2: Confirmed Chain Queries

- shared `IndexedChain` sync owner
- `blockchain.headers.subscribe`
- `blockchain.block.header`
- `blockchain.block.headers` using the `1.6` `headers` array response
- nonzero `cp_height` header proofs
- `blockchain.transaction.get` for confirmed txs
- `blockchain.transaction.get_merkle`
- `blockchain.transaction.id_from_pos`
- confirmed scripthash history, balance, and listunspent
- confirmed-chain scripthash status calculation
- reorg handling and tests

### Milestone 3: Mempool and Relay

- mempool index
- canonical `1.6` mempool ordering
- scripthash mempool history
- unconfirmed balance delta
- mempool-adjusted listunspent
- mempool-triggered scripthash notifications
- `mempool.get_fee_histogram`
- `mempool.get_info`
- `blockchain.estimatefee(number, mode)`
- `blockchain.transaction.broadcast`
- `blockchain.transaction.broadcast_package`

### Milestone 4: Subscriptions and Compatibility

- `blockchain.scripthash.subscribe`
- `blockchain.scripthash.unsubscribe`
- header notifications
- status-change notifications
- backwards-compatible serializers for negotiated versions below `1.6`
- compatibility tests against every advertised method and negotiated response shape
- Electrum wallet smoke test

### Milestone 5: Public Service Hardening

- TLS
- rate limits
- peer advertisement, if desired
- persistent query cache
- monitoring
- graceful shutdown
- resource-pressure subscription dropping
- load tests with adversarial scripthash scans

## 17. Open Design Decisions

- Whether header checkpoint proofs are backed by a persisted proof cache or computed from an in-memory header tree. They are required for the `1.6` target because `cp_height` support arrived in protocol `1.4`.
- Whether to store persistent per-scripthash cache in SQLite, RocksDB column families, or a separate embedded database.
- Whether public deployments should expose peer discovery or always return an empty peer list.
- Whether verbose transaction responses should be proxied exactly from bitcoind or normalized for non-Bitcoin networks. For Bitcoin, proxying bitcoind is preferred.
- Whether to use ZMQ for mempool/block notifications or polling-only. ZMQ gives lower latency but polling is simpler and easier to deploy.

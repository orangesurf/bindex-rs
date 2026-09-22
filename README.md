# Bitcoin indexing library in Rust

[![CI](https://github.com/romanz/bindex-rs/actions/workflows/ci.yml/badge.svg)](https://github.com/romanz/bindex-rs/actions)
[![crates.io](https://img.shields.io/crates/v/bindex.svg)](https://crates.io/crates/bindex)

See [slides](https://docs.google.com/presentation/d/1Zez-6DApKRu59kke4i_g9jwxQlaFKKRpOPdYFYsFXfA/) for more details.

Bitcoin Core 31 is required for efficient indexing and querying.

## Usage

[![asciicast](https://asciinema.org/a/yFjcbagORZNMtoOPikw0kUlC9.svg)](https://asciinema.org/a/yFjcbagORZNMtoOPikw0kUlC9)

## Liquid (Elements) support

`bindex-lib`, `bindex-sync` and `bindex-electrum` build with `--features liquid` to
index a Liquid/Elements chain. The only format-aware code is `bindex-lib/src/fmt.rs`
(rust-bitcoin by default, rust-elements behind the feature); hashes are exposed as
`bitcoin::BlockHash`/`bitcoin::Txid` everywhere else and the on-disk layout is
unchanged (header rows carry the raw header bytes, which are 80 bytes on Bitcoin and
variable-length dynafed headers on Liquid). Elements exposes no `spenttxouts` or
`blockpart` REST endpoints, so indexing Liquid needs a facade that serves the
full REST surface from batched RPC. Build Liquid binaries into their own target dir so
the Bitcoin binaries under `target/release` (used by running services) stay put:

    cargo build --release --target-dir target-liquid -p bindex-sync -p bindex-electrum \
        --features "bindex-sync/liquid bindex-electrum/liquid"

## Esplora-compatible REST API

`bindex-electrum --http-addr 127.0.0.1:3000` serves the mempool/electrs REST
surface (`/blocks`, `/block`, `/tx`, `/address`, `/scripthash`, `/mempool`,
`/fee-estimates`, the `/internal` batches and the broadcast routes) beside the
Electrum listener, from the same chain and mempool state. Response shapes, TTLs
and error texts follow `mempool/electrs`; see `bindex-electrum/src/rest/`.

Notes specific to this backend:

* The Liquid build (`--features liquid`) serves the electrs-liquid shapes from
  the same routes; see "The REST API on Liquid" below.
* A REST scripthash is `sha256(scriptPubKey)` **as given** — unlike the Electrum
  protocol's reversed form.
* Bodies that the index does not store come from bitcoind's REST interface,
  which bindex already requires: `/rest/block/notxdetails` for block metadata
  and txids, `/rest/spenttxouts` for a whole block's prevouts in one request,
  `/rest/tx` for mempool bodies.
* The mempool index is filled by a poller that starts only when `--http-addr`
  is set. It fetches the body of each new transaction once (a full mainnet
  mempool takes a few seconds) and never resolves a prevout: an input is
  attributed to a script by looking its outpoint up in that script's UTXO set.
* `/tx/:txid/outspend*` has no index behind it — the `spending` column family is
  never written — so the spender is found by scanning the funding script's index
  rows from the funding block on. That is cheap for a normal address and
  proportional to reuse for a hot one. `POST /internal/txs/outspends/by-txid`
  keeps the reference's "no limit" on the batch, so it pays that cost once per
  output of every transaction posted to it: it is bounded only by the
  per-request deadline and the query semaphore below, and a batch of hot
  transactions will hit the deadline rather than finish.
* A query never holds the chain lock while it waits on the node. The index work
  (the scripthash scan, and resolving each row to a byte range) runs under the
  read lock; the bodies are fetched with it released, eight at a time, and the
  tip is re-read afterwards so a fold never mixes two views of the chain. This
  matters because the secondary refresh needs the write lock: a query that held
  the read lock for a minute would stall the refresh and every other reader
  behind it.
* Bounds, all configurable: `--rest-request-timeout-secs` (30) gives up with a
  504, `--rest-max-concurrent-queries` (4) sheds the expensive routes with a
  503 rather than letting them crowd out the cheap ones,
  `--rest-max-connections` (100) answers 503 instead of dropping, and
  `--rest-header-timeout-secs` (10) / `--rest-idle-timeout-secs` (30) close
  connections that never finish a request. Request bodies stop at
  `--request-body-bytes-cap` (20,000,200). Only `Content-Length` framing is
  accepted: anything ambiguous, and any `Transfer-Encoding`, is refused and the
  connection closed.
* `POST /txs/test` is refused under `--broadcast-via tor`: forwarding the
  client's hex to the local node is what that mode exists to prevent, and
  mempool.space's onion has no `testmempoolaccept` endpoint to forward to.
* `/address-prefix/:prefix` cannot be served: the index stores an 8-byte prefix
  of each scripthash and no addresses.
* Running a second instance against a live index needs `--secondary-path` (and
  its own `--monitor-path`/`--cache-path`): a RocksDB secondary directory cannot
  be shared between processes.

### The REST API on Liquid

With `--features liquid`, `--http-addr` serves what electrs-liquid (liquid.network)
serves, and `--network` names the parent chain (`bitcoin` for Liquid). The
transaction, block and outspend JSON matches liquid.network byte for byte on the
fixtures in `bindex-electrum/tests/fixtures/liquid/` (a coinbase, a confidential
transaction, an issuance, a reissuance, a peg-in and a peg-out):

* Outputs carry `value`/`asset` when explicit and `valuecommitment`/`assetcommitment`
  when blinded, never both; the explicit fee output is typed `fee`; a peg-out gets
  a `pegout` object with its Bitcoin address. Inputs carry `is_pegin` and, when they
  issue, an `issuance` object. A peg-in has no `prevout`.
* `fee` is what the explicit fee outputs pay in L-BTC. `sigops` is counted as
  electrs counts it (legacy only for a coinbase or any peg-in).
* `/block/:hash` includes the header's `ext` (dynafed parameters and signblock
  witness); the list routes leave it out. There are no `nonce`, `bits` or
  `difficulty` fields and no `/tx/:txid/merkleblock-proof`.
* Address stats carry counts only (no sums), summaries a zero `value`, and each
  `/utxo` entry the output's commitments and nonce, which costs one transaction
  fetch per UTXO. `/mempool/recent` entries have no `value`. Where the deployed
  liquid.network differs from the electrs source, liquid.network wins: stats are
  ordered `funded_txo_count, spent_txo_count, tx_count`, and UTXOs carry no
  surjection or range proofs.
* Prevouts come from the funding transactions: from the same block when possible,
  otherwise one index lookup per funding transaction. The facade's `spenttxouts`
  cannot be used for this: its Bitcoin encoding has no room for commitments.
* Block metadata, mempool bodies and the mempool list come from Elements RPC
  (`getblock <hash> 1`, `getrawtransaction`, `getrawmempool true`), because the
  REST facade serves only what the indexer needs. A prevout the index does not
  have (a pruned-region stub) falls back to `getrawtransaction`, which answers
  only when the node runs with `-txindex`.
* `/asset*` and `/assets*` need an asset index that does not exist here. With
  `--asset-upstream https://liquid.network/api` they are forwarded there, status,
  body and the registry's `X-Total-Results` included; without it they answer 404.


# Bitcoin indexing library in Rust

[![CI](https://github.com/romanz/bindex-rs/actions/workflows/ci.yml/badge.svg)](https://github.com/romanz/bindex-rs/actions)
[![crates.io](https://img.shields.io/crates/v/bindex.svg)](https://crates.io/crates/bindex)

See [slides](https://docs.google.com/presentation/d/1Zez-6DApKRu59kke4i_g9jwxQlaFKKRpOPdYFYsFXfA/) for more details.

Bitcoin Core 31 is required for efficient indexing and querying.

## What this fork adds

This fork turns bindex from an indexing library into a set of servers you can
point wallets and block explorers at, for Bitcoin and for Liquid. All credit for
the index itself goes to [Roman Zeyde](https://github.com/romanz): the compact
scripthash and txid index, and the Core REST fetching it relies on, are his.
Everything below sits on top of that, on branch `main`; `master` tracks upstream.

Upstream ships the library and `bindex-cli`. The fork adds three programs:

| Program | What it does |
|---|---|
| `bindex-electrum` | Electrum protocol server, plus an optional Esplora REST API |
| `bindex-web` | Owns and syncs the index, and serves an address and txid search page |
| `bindex-sync` | A minimal writer that only keeps the index synced, for pairing with read-only servers |

The main features:

* **Electrum server:** headers, scripthash history, balance, UTXOs, mempool and
  subscriptions, transaction and merkle-proof lookups, fee estimates and
  histograms, JSON-RPC batching, TCP and TLS, and a result cache.
* **Package broadcast:** `blockchain.transaction.broadcast_package` submits a
  package of related transactions, as Bitcoin Core's `submitpackage` does.
* **Tor broadcast:** with `--broadcast-via tor`, every transaction and package
  goes to mempool.space's onion endpoint on a fresh Tor circuit, and nothing is
  ever submitted through your own node. The trade-off is a dependency on that
  endpoint; `--broadcast-via bitcoind` uses your node instead.
* **Esplora REST API:** `--http-addr` serves the mempool/electrs REST surface
  (blocks, transactions, outspends, the mempool, address and scripthash routes,
  the `/internal` batch routes and broadcast) with the reference's shapes, TTLs
  and error texts. The trade-off is that it keeps no extra indexes: to show
  which transaction spent an output, it walks the history of the address that
  received it. That is instant for a typical address and takes seconds for one
  with thousands of transactions; see the section below for the costs and bounds.
* **Liquid:** `--features liquid` indexes a Liquid (Elements) chain and serves
  both the Electrum protocol and the electrs-liquid REST shapes: commitments,
  peg-ins, peg-outs and issuances. Six real Liquid transactions are test
  fixtures, and the REST output matches liquid.network's byte for byte on them.
  Asset pages (issuance history, supply, names and tickers) need an index of
  every asset, which the fork does not build; `--asset-upstream` forwards those
  requests to another Esplora server, such as liquid.network.
* **Read-only secondaries:** a server can open the index as a RocksDB secondary
  while a separate writer syncs it. A refresh reads only the new header rows, so
  following a 4-million-block Liquid chain costs milliseconds rather than the
  ~0.9 s a full reload took; a reorg below the tip still reloads the chain.
* **Connection reuse:** the REST client reuses HTTP connections, which stops
  long syncs running out of ephemeral ports.

Every feature has tests: `cargo test -p bindex-electrum` runs unit tests and
regtest end-to-end suites against a local `bitcoind`, and the Liquid build's
tests run with `--features liquid`. None of this has been reviewed upstream.

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
  of each scripthash and no addresses. It answers 400 "address search disabled"
  by default and an empty list with `--address-search`, which an explorer's
  search box handles as "no suggestions".
* The address/scripthash stats object goes out with its keys sorted (the label
  first or last, `tx_count` last), because electrs builds it with `json!`.
* Mempool `vsize` (in `/mempool`, the fee histogram and `/mempool/recent`) is
  `weight / 4` rounded down, as electrs computes it, not the node's figure.
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
  fetch per UTXO. `/mempool/recent` entries have no `value`. UTXOs carry no surjection
  or range proofs, because electrs stores outputs without their witness.
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


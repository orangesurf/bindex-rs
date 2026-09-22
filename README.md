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

* Only in the Bitcoin build. The module is compiled out with `--features liquid`,
  whose transactions are Elements-encoded, and `--http-addr` is rejected there.
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
  proportional to reuse for a hot one.
* `/address-prefix/:prefix` cannot be served: the index stores an 8-byte prefix
  of each scripthash and no addresses.
* Running a second instance against a live index needs `--secondary-path` (and
  its own `--monitor-path`/`--cache-path`): a RocksDB secondary directory cannot
  be shared between processes.

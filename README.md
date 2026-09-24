# bindex

bindex is a compact address and transaction index for Bitcoin and Liquid, with
the servers that wallets and block explorers connect to. One index serves four
uses: a command-line address watcher, an Electrum server, an Esplora REST API and
a search page. On Bitcoin mainnet the index takes about 57 GB (September 2026).

This repository is a fork of Roman Zeyde's
[romanz/bindex-rs](https://github.com/romanz/bindex-rs). The index design is his;
see his [slides](https://docs.google.com/presentation/d/1Zez-6DApKRu59kke4i_g9jwxQlaFKKRpOPdYFYsFXfA/)
for how it works. Features marked ![new][new] were added in this fork and are not in
upstream. Everything else is upstream's.

## How the index works

For every confirmed transaction, bindex stores an 8-byte prefix of its txid and of
each script it touches, mapped to a 4-byte transaction number. It stores no
transaction bodies, amounts or addresses. When a query needs a transaction, bindex
reads just that transaction's bytes from Bitcoin Core's REST interface, using the
block position it recorded.

That is why the index is small. The trade-off is that every answer involves the
node: bodies come from Core, and a short prefix can match the wrong script, so
each candidate is fetched and checked. Bitcoin Core 31 or later is required, with
`rest=1`, for the REST endpoints bindex reads.

## Index library (`bindex-lib`)

- **Sync from Bitcoin Core:** fetches headers, blocks and spent outputs over REST
  in binary format, follows reorgs, and compacts the database when idle.
- **Lookups:** transaction locations by script hash or txid, raw transaction
  bytes, and the header chain.
- **Address cache:** an SQLite cache holding the history and transactions of a
  set of watched addresses, kept in step with the index.
- **Custom REST URL:** the node's REST server does not have to be on localhost.
- **Read-only followers:** ![new][new] a second process can open the index
  as a RocksDB secondary while one writer syncs it. A refresh reads only the new
  header rows, so following Liquid's 4-million-block chain costs milliseconds
  rather than about 0.9 s. A reorg below the tip still reloads the chain.
- **Named index directories:** ![new][new] several indexes, such as Bitcoin
  and Liquid, can share one data directory.
- **Liquid format:** ![new][new] with `--features liquid`, the library reads
  Elements blocks, including variable-length dynafed headers. The on-disk layout
  is unchanged.
- **Connection reuse:** ![new][new] REST calls reuse HTTP connections, which
  stops long syncs running out of ephemeral ports.

## Address watcher (`bindex-cli`)

- **Watch a list of addresses:** reads addresses from a file or standard input,
  syncs the index and prints each address's history with a running balance.
- **Persistent cache:** `--cache` keeps that history in an SQLite file between
  runs; `contrib/history.sql` queries it directly.
- **One-shot mode:** `--sync-once` exits after catching up.
- **All networks:** mainnet, testnet, testnet4, signet and regtest.

## Index writer (`bindex-sync`) ![new][new]

- **Writer only:** ![new][new] keeps an index synced and serves nothing, so
  servers can follow it as read-only secondaries.
- **Any Core-compatible REST source:** ![new][new] a node, or a facade in
  front of one.
- **Catch-up mode:** ![new][new] `--once` exits when no new blocks arrive.

## Electrum server (`bindex-electrum`) ![new][new]

- **Protocol 1.4 to 1.6:** ![new][new] negotiated per connection, including
  1.6's header arrays, `mempool.get_info` and canonical mempool ordering.
- **Full method set:** ![new][new] headers with checkpoint proofs, script-hash
  history, balance, UTXOs and mempool, subscriptions, transaction and Merkle-proof
  lookups, fee estimates and fee histograms.
- **Package broadcast:** ![new][new] `blockchain.transaction.broadcast_package`
  submits related transactions together, as Core's `submitpackage` does.
- **Tor broadcast:** ![new][new] by default on Bitcoin, every transaction goes
  to mempool.space's onion endpoint on a fresh Tor circuit, and nothing is
  submitted through your node. That needs a running Tor daemon and depends on
  mempool.space; `--broadcast-via bitcoind` uses your node instead.
- **Transports:** ![new][new] TCP and TLS, JSON-RPC batching, and per-session
  limits on batch size and subscriptions.
- **History cache and monitor:** ![new][new] script histories and statuses are
  cached in SQLite, and request latencies are written to a JSON file for the
  monitor page.

The server follows the index as a read-only secondary, so it needs a writer:
`bindex-sync` or `bindex-web`. It polls the node's mempool every 5 seconds.

## Esplora REST API (`bindex-electrum --http-addr`) ![new][new]

- **The mempool/electrs REST API:** ![new][new] blocks, transactions,
  outspends, the mempool, fee estimates, address and script-hash routes, the
  `/internal` batch routes and broadcast, with the reference's response shapes,
  cache lifetimes and error texts.
- **Liquid responses:** ![new][new] the Liquid build serves electrs-liquid's
  shapes (commitments, peg-ins, peg-outs, issuances). On six real Liquid
  transactions kept as test fixtures, the output matches liquid.network byte for
  byte.
- **Asset forwarding:** ![new][new] `--asset-upstream` forwards asset routes
  to another Esplora server, since the fork builds no asset index.
- **Bounded cost:** ![new][new] request deadlines, a cap on concurrent
  expensive queries and a connection limit, all configurable.

The API adds no indexes of its own. To find which transaction spent an output, it
walks the history of the script that received it. That's instant for a typical
address and takes seconds for one with thousands of transactions.
[docs/esplora-rest-api.md](docs/esplora-rest-api.md) covers the costs, the bounds
and every known difference from the reference.

## Search page (`bindex-web`) ![new][new]

- **Address and transaction search:** ![new][new] a page showing an address's
  history or a transaction's summary, served from the index.
- **Monitor page:** ![new][new] request counts, errors and latencies per route,
  for itself and for `bindex-electrum`.
- **Owns the index:** ![new][new] it syncs the index as the primary writer.

Its settings (mainnet, `./db`, a loopback listen address) are constants in
`bindex-web/src/main.rs`; it has no command-line options yet.

## Liquid ![new][new]

`bindex-lib`, `bindex-sync` and `bindex-electrum` build with `--features liquid` to
index a Liquid chain and serve both Electrum and the Liquid REST API from it.
Elements has no `spenttxouts` or `blockpart` REST endpoints, so indexing Liquid
needs a REST facade in front of the node. This repository doesn't include one.

## Build and run

    cargo build --release -p bindex-sync -p bindex-electrum -p bindex-web -p bindex-cli
    cargo build --release --target-dir target-liquid -p bindex-sync -p bindex-electrum \
        --features "bindex-sync/liquid bindex-electrum/liquid"

A minimal Bitcoin setup is a writer plus a server over the same data directory:

    target/release/bindex-sync --db-path ./db --rest-url http://127.0.0.1:8332
    target/release/bindex-electrum --bindex-db-path ./db \
        --bitcoind-rpc-cookie /path/to/.cookie --broadcast-via bitcoind \
        --http-addr 127.0.0.1:3000

Run `cargo test` with `BITCOIND_EXE` pointing at a `bitcoind` binary. Without it,
the end-to-end suites skip themselves and still report success.

bindex is MIT-licensed; see [LICENSE](LICENSE).

[new]: https://img.shields.io/badge/NEW-2ea44f?style=flat-square

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

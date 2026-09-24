# Agent instructions

This repository is a public fork of [romanz/bindex-rs](https://github.com/romanz/bindex-rs).
On top of the upstream library and `bindex-cli` it adds `bindex-electrum` (an Electrum
server with an optional Esplora REST API), `bindex-web`, `bindex-sync`, read-only
RocksDB secondaries and a Liquid build (`--features liquid`). `README.md` lists the
features.

## Branches

- `main` is the development branch. Start work on a topic branch off `main`; the owner
  reviews it and merges it into `main`.
- `master` mirrors upstream. Never commit to it.
- Upstream pull requests are cut from `master`, one feature each, and only when the
  owner asks for one.

## Build and test

Tests need `libclang` (Debian/Ubuntu: `libclang-dev`) and a Bitcoin Core 31 `bitcoind`.
The end-to-end suites **skip themselves and still pass** when `BITCOIND_EXE` is unset,
so set it before trusting a green run:

    wget https://bitcoincore.org/bin/bitcoin-core-31.0/bitcoin-31.0-x86_64-linux-gnu.tar.gz
    tar xf bitcoin-31.0-x86_64-linux-gnu.tar.gz bitcoin-31.0/bin/bitcoind
    export BITCOIND_EXE=$PWD/bitcoin-31.0/bin/bitcoind

The gate, which every change must pass:

    cargo test -p bindex --lib && cargo test -p bindex-electrum
    cargo test --target-dir target-liquid -p bindex --lib --features liquid
    cargo test --target-dir target-liquid -p bindex-electrum --features liquid
    cargo clippy -p bindex-electrum --all-targets    # add no new warnings

Much of the fork's code is not yet rustfmt-clean. Do not run `cargo fmt --all`: format
only the code your change touches, so diffs stay reviewable.

Keep the Liquid build in its own `--target-dir`, so its binaries never overwrite the
Bitcoin build's.

On macOS with Homebrew LLVM, set `LIBCLANG_PATH=/opt/homebrew/opt/llvm/lib`.

## Esplora REST API

`bindex-electrum --http-addr` serves the REST API of mempool's electrs fork
(`mempool/electrs`). Match the reference byte for byte: routes, JSON shapes and key
order, status codes, error texts and `Cache-Control` values. When unsure how the
reference behaves, read its source rather than guessing. For the Liquid build, the
reference is electrs's `liquid` feature as served by liquid.network, and the fixtures
under `bindex-electrum/tests/fixtures/liquid/` are real transactions with its responses.

The fork deliberately builds no indexes beyond bindex's own. Features that would need
one (such as a full asset index) are forwarded to an upstream server
(`--asset-upstream`) rather than indexed here.

## This repository is public

Nothing committed may reveal where or how the owner runs this software:

- No host or network names, home-directory paths, datadirs, local port assignments,
  service names or anything copied from the owner's machine configuration. Examples
  use generic values (`127.0.0.1:3000`, `/path/to/bitcoin.conf`).
- Commit messages describe the change, not a deployment.
- Author commits with the owner's GitHub noreply address, never a personal email.
- Machine-specific notes go in `CLAUDE.local.md` or `.agents/`, which are never tracked.
  Do not `git add -f` them.

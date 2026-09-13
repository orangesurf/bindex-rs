//! The primary writer for a bindex index: fetch new blocks through the node's
//! (or a shim's) Bitcoin-Core-compatible REST API and index them, forever.
//! Readers such as bindex-electrum open the same RocksDB as secondaries.
//!
//! Built with `--features liquid` this speaks the Elements block format and
//! defaults the index directory to `<db-path>/liquid`.
use std::{path::PathBuf, time::Duration};

use anyhow::Context as _;
use clap::Parser;

#[derive(Debug, Parser)]
#[command(name = "bindex-sync", about = "Sync a bindex index from a REST API")]
struct Args {
    /// Directory holding the index (a `<db-name>` subdirectory is created inside)
    #[arg(long)]
    db_path: PathBuf,

    /// Index directory name; defaults to the chain format name ("bitcoin" or "liquid")
    #[arg(long)]
    db_name: Option<String>,

    /// Base URL of a Bitcoin-Core-compatible REST API (node or shim)
    #[arg(long, default_value = "http://127.0.0.1:8332")]
    rest_url: String,

    /// Headers fetched per sync round
    #[arg(long, default_value_t = 2000)]
    batch: usize,

    /// Seconds to wait between rounds once caught up
    #[arg(long, default_value_t = 5)]
    poll_secs: u64,

    /// Exit once no new blocks are found (one-shot catch-up)
    #[arg(long)]
    once: bool,
}

fn main() -> anyhow::Result<()> {
    env_logger::Builder::from_env(env_logger::Env::default().default_filter_or("info")).init();
    let args = Args::parse();
    let name = args
        .db_name
        .clone()
        .unwrap_or_else(|| bindex::fmt::NAME.to_string());
    log::info!("format={} db={}/{} rest={}", bindex::fmt::NAME, args.db_path.display(), name, args.rest_url);
    let mut chain = bindex::IndexedChain::open_named(&args.db_path, &name, args.rest_url.clone())
        .context("open index")?;
    loop {
        let stats = chain.sync(args.batch).context("sync")?;
        if stats.indexed_blocks == 0 {
            if args.once {
                log::info!("caught up at {}", stats.tip);
                return Ok(());
            }
            std::thread::sleep(Duration::from_secs(args.poll_secs));
        }
    }
}

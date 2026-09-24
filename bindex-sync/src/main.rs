//! The primary writer for a bindex index: fetch new blocks through the node's
//! (or a shim's) Bitcoin-Core-compatible REST API and index them, forever.
//! Readers such as bindex-electrum open the same RocksDB as secondaries.
//!
//! Built with `--features liquid` this speaks the Elements block format and
//! defaults the index directory to `<db-path>/liquid`.
use std::{
    path::PathBuf,
    sync::mpsc::{self, Receiver, RecvTimeoutError},
    time::Duration,
};

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

    /// Blocks indexed per sync round. A round asks the REST API for one header
    /// more than this, and Bitcoin Core serves at most 2000 per request, so
    /// against Core keep it below 2000.
    #[arg(long, default_value_t = 1000)]
    batch: usize,

    /// Seconds to wait between rounds once caught up
    #[arg(long, default_value_t = 5)]
    poll_secs: u64,

    /// Exit once no new blocks are found (one-shot catch-up)
    #[arg(long)]
    once: bool,
    /// The node's `zmqpubrawblock` endpoint (e.g. `tcp://127.0.0.1:28332`).
    /// Each announced block starts a sync round at once instead of after
    /// `--poll-secs`; polling stays as the fallback, because ZMQ can drop
    /// messages.
    #[arg(long)]
    zmq_rawblock: Option<String>,
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
    let blocks = match &args.zmq_rawblock {
        Some(endpoint) if !args.once => Some(subscribe_blocks(endpoint.clone())?),
        _ => None,
    };
    let poll = Duration::from_secs(args.poll_secs);
    loop {
        let stats = chain.sync(args.batch).context("sync")?;
        if stats.indexed_blocks == 0 {
            if args.once {
                log::info!("caught up at {}", stats.tip);
                return Ok(());
            }
            match &blocks {
                Some(blocks) => wait_for_block(blocks, poll),
                None => std::thread::sleep(poll),
            }
        }
    }
}

/// Wait for a block announcement or `poll`, whichever comes first. Announcements
/// that queued up meanwhile are drained: one sync round covers them all.
fn wait_for_block(blocks: &Receiver<()>, poll: Duration) {
    match blocks.recv_timeout(poll) {
        Ok(()) => while blocks.try_recv().is_ok() {},
        Err(RecvTimeoutError::Timeout) => {}
        // the listener thread is gone; fall back to plain polling
        Err(RecvTimeoutError::Disconnected) => std::thread::sleep(poll),
    }
}

/// Listen for the node's `rawblock` announcements on a thread of its own,
/// reconnecting for as long as the process runs. The payload is ignored; each
/// message only sends a wake-up.
fn subscribe_blocks(endpoint: String) -> anyhow::Result<Receiver<()>> {
    anyhow::ensure!(
        endpoint.starts_with("tcp://"),
        "zmq-rawblock must be a tcp:// endpoint, got {endpoint}"
    );
    let (wake, blocks) = mpsc::channel();
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .context("start the zmq runtime")?;
    std::thread::Builder::new()
        .name("zmq-rawblock".to_string())
        .spawn(move || {
            runtime.block_on(async move {
                use zeromq::{Socket as _, SocketRecv as _};
                loop {
                    let listen = async {
                        let mut socket = zeromq::SubSocket::new();
                        // waits for the node to come up rather than failing
                        socket.connect(&endpoint).await?;
                        socket.subscribe("rawblock").await?;
                        log::info!("zmq: subscribed to rawblock at {endpoint}");
                        loop {
                            socket.recv().await?;
                            if wake.send(()).is_err() {
                                return Ok::<(), zeromq::ZmqError>(());
                            }
                        }
                    };
                    match listen.await {
                        Ok(()) => return,
                        Err(err) => log::warn!("zmq rawblock at {endpoint}: {err}; reconnecting"),
                    }
                    tokio::time::sleep(Duration::from_secs(1)).await;
                }
            })
        })
        .context("spawn the zmq thread")?;
    Ok(blocks)
}

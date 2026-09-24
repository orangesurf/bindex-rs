//! Optional ZMQ wake-ups from the node (`--zmq-rawblock`, `--zmq-rawtx`).
//!
//! The node's `zmqpubrawblock`/`zmqpubrawtx` notifications are used only as a
//! signal that something changed: payloads are ignored, and the regular index
//! refresh and mempool poll do the work. ZMQ drops messages under load, and a
//! subscriber misses everything published while it reconnects, so the interval
//! timers stay as the fallback; ZMQ only makes them fire sooner.

use std::{convert::Infallible, time::Duration};

use tokio::sync::Notify;
use zeromq::{Socket as _, SocketRecv as _, SubSocket};

const RECONNECT_DELAY: Duration = Duration::from_secs(1);

/// When to refresh the index after a block announcement, counted from the
/// announcement. The writer indexes the block on its own schedule, so the first
/// refresh may find nothing new; these cover a writer that polls every few
/// seconds.
pub const BLOCK_REFRESH_OFFSETS: [Duration; 6] = [
    Duration::ZERO,
    Duration::from_millis(250),
    Duration::from_secs(1),
    Duration::from_secs(2),
    Duration::from_secs(5),
    Duration::from_secs(10),
];

/// The least time between the starts of two mempool polls. Each poll fetches
/// the node's whole mempool, so a burst of transaction announcements must not
/// turn into back-to-back polls.
pub const MIN_MEMPOOL_POLL_GAP: Duration = Duration::from_secs(1);

/// Signals from the ZMQ listeners to the refresh and mempool tasks. A `Notify`
/// holds at most one pending wake-up, so any number of announcements that
/// arrive while a task is busy cost it one extra run.
#[derive(Default)]
pub struct Wakeups {
    pub refresh: Notify,
    pub mempool: Notify,
}

/// Subscribe to `topic` at `endpoint` and call `on_message` for each
/// notification, reconnecting for as long as the process runs.
pub async fn subscribe<F>(endpoint: String, topic: &'static str, on_message: F)
where
    F: Fn() + Send + Sync + 'static,
{
    loop {
        let Err(err) = listen(&endpoint, topic, &on_message).await;
        log::warn!("zmq {topic} at {endpoint}: {err}; reconnecting");
        tokio::time::sleep(RECONNECT_DELAY).await;
    }
}

async fn listen<F>(endpoint: &str, topic: &str, on_message: &F) -> zeromq::ZmqResult<Infallible>
where
    F: Fn() + Send + Sync,
{
    let mut socket = SubSocket::new();
    // waits for the node to come up rather than failing
    socket.connect(endpoint).await?;
    socket.subscribe(topic).await?;
    log::info!("zmq: subscribed to {topic} at {endpoint}");
    loop {
        let message = socket.recv().await?;
        if message
            .get(0)
            .is_some_and(|t| t.as_ref() == topic.as_bytes())
        {
            on_message();
        }
    }
}

#![cfg(not(feature = "liquid"))] // drives a regtest bitcoind
//! With a ten-minute poll, only a ZMQ block announcement can make the writer
//! index a new block within seconds.

use std::{
    process::{Child, Command, Stdio},
    time::{Duration, Instant},
};

use corepc_node::{exe_path, Conf, Node};
use tempfile::TempDir;

/// Kills the writer when the test ends, pass or fail.
struct Writer(Child);

impl Drop for Writer {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

#[test]
fn rawblock_announcements_start_a_sync_round() -> anyhow::Result<()> {
    let bitcoind = match exe_path() {
        Ok(path) => path,
        Err(err) => {
            eprintln!("skipping bindex-sync ZMQ test: BITCOIND_EXE is not set or invalid: {err}");
            return Ok(());
        }
    };
    let endpoint = format!(
        "tcp://127.0.0.1:{}",
        std::net::TcpListener::bind("127.0.0.1:0")?
            .local_addr()?
            .port()
    );
    let zmq_arg = format!("-zmqpubrawblock={endpoint}");
    let mut conf = Conf::default();
    conf.args.push("-rest");
    conf.args.push(&zmq_arg);
    let node = Node::with_conf(bitcoind, &conf)?;
    let miner = node.client.new_address()?;
    node.client.generate_to_address(101, &miner)?;

    let db_dir = TempDir::with_prefix("bindex-sync-zmq")?;
    let rest_url = format!("http://{}", node.params.rpc_socket);
    let _writer = Writer(
        Command::new(env!("CARGO_BIN_EXE_bindex-sync"))
            .args(["--db-path", db_dir.path().to_str().unwrap()])
            .args(["--db-name", "regtest", "--rest-url", &rest_url])
            .args(["--poll-secs", "600", "--zmq-rawblock", &endpoint])
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()?,
    );

    let secondary = TempDir::with_prefix("bindex-sync-zmq-secondary")?;
    let mut reader = None;
    let height = |reader: &mut Option<bindex::IndexedChain>| -> Option<usize> {
        if reader.is_none() {
            *reader = bindex::IndexedChain::open_secondary_named(
                db_dir.path(),
                "regtest",
                rest_url.clone(),
                secondary.path(),
            )
            .ok();
        }
        let chain = reader.as_mut()?;
        chain.refresh_secondary().ok()?;
        chain.headers().tip_height()
    };
    wait_until(Duration::from_secs(20), "the initial sync", || {
        height(&mut reader) == Some(101)
    })?;
    // A subscriber misses whatever is published before it joins.
    std::thread::sleep(Duration::from_secs(1));

    node.client.generate_to_address(1, &miner)?;
    wait_until(Duration::from_secs(10), "the announced block", || {
        height(&mut reader) == Some(102)
    })
}

fn wait_until(limit: Duration, what: &str, mut done: impl FnMut() -> bool) -> anyhow::Result<()> {
    let started = Instant::now();
    while started.elapsed() < limit {
        if done() {
            return Ok(());
        }
        std::thread::sleep(Duration::from_millis(100));
    }
    anyhow::bail!("timed out after {limit:?} waiting for {what}")
}

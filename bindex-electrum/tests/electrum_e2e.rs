#![cfg(not(feature = "liquid"))] // drives a regtest bitcoind; Liquid is covered by the unit tests and fixtures
use std::{collections::VecDeque, future, time::Duration};

use anyhow::Context as _;
use bindex_electrum::{
    config::{BroadcastVia, Config, RestConfig},
    protocol::{ElectrumScripthash, ProtocolVersion},
    server::Server,
};
use bitcoin::{consensus::serialize, Amount, Network};
use corepc_node::{exe_path, Conf, Node};
use serde_json::{json, Value};
use tempfile::TempDir;
use tokio::{
    io::{AsyncBufReadExt, AsyncWriteExt, BufReader, Lines},
    net::{
        tcp::{OwnedReadHalf, OwnedWriteHalf},
        TcpListener, TcpStream,
    },
};

struct ElectrumClient {
    writer: OwnedWriteHalf,
    lines: Lines<BufReader<OwnedReadHalf>>,
    next_id: i64,
    /// Notifications that arrived while a call waited for its response.
    notifications: VecDeque<Value>,
}

impl ElectrumClient {
    async fn connect(addr: std::net::SocketAddr) -> anyhow::Result<Self> {
        let stream = TcpStream::connect(addr).await?;
        let (reader, writer) = stream.into_split();
        Ok(Self {
            writer,
            lines: BufReader::new(reader).lines(),
            next_id: 0,
            notifications: VecDeque::new(),
        })
    }

    async fn call(&mut self, method: &str, params: Value) -> anyhow::Result<Value> {
        self.next_id += 1;
        let request = json!({
            "jsonrpc": "2.0",
            "id": self.next_id,
            "method": method,
            "params": params,
        });
        let mut bytes = serde_json::to_vec(&request)?;
        bytes.push(b'\n');
        self.writer.write_all(&bytes).await?;

        let response = loop {
            let message = self.read().await?;
            if message.get("id").is_some() {
                break message;
            }
            self.notifications.push_back(message);
        };
        if let Some(error) = response.get("error") {
            anyhow::bail!("{method} returned error: {error}");
        }
        Ok(response
            .get("result")
            .cloned()
            .context("response missing result")?)
    }

    async fn read(&mut self) -> anyhow::Result<Value> {
        let line = self
            .lines
            .next_line()
            .await?
            .context("server closed connection")?;
        Ok(serde_json::from_str(&line)?)
    }

    /// The next server notification, waiting up to `limit` for one.
    async fn notification(&mut self, limit: Duration) -> anyhow::Result<Value> {
        if let Some(message) = self.notifications.pop_front() {
            return Ok(message);
        }
        let message = tokio::time::timeout(limit, self.read())
            .await
            .context("no notification arrived")??;
        anyhow::ensure!(message.get("id").is_none(), "unexpected response {message}");
        Ok(message)
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn electrum_server_serves_synced_regtest_chain() -> anyhow::Result<()> {
    let bitcoind = match exe_path() {
        Ok(path) => path,
        Err(err) => {
            eprintln!("skipping electrum E2E test: BITCOIND_EXE is not set or invalid: {err}");
            return Ok(());
        }
    };

    let mut conf = Conf::default();
    conf.args.push("-rest");
    let node = Node::with_conf(bitcoind, &conf)?;

    let miner = node.client.new_address()?;
    node.client.generate_to_address(101, &miner)?;

    let recipient = node.client.new_address()?;
    let amount = Amount::from_sat(50_000);
    let txid = node
        .client
        .send_to_address(&recipient, amount)?
        .txid()
        .context("send_to_address returned no txid")?;
    let tx = node.client.get_raw_transaction(txid)?.transaction()?;
    let recipient_script = recipient.script_pubkey();
    let recipient_vout = tx
        .output
        .iter()
        .position(|output| output.script_pubkey == recipient_script)
        .context("recipient output not found")? as u32;
    node.client.generate_to_address(1, &miner)?;

    let db_dir = TempDir::with_prefix("bindex-electrum-db")?;
    let rest_url = format!("http://{}", node.params.rpc_socket);
    let mut chain = bindex::IndexedChain::open_with_rest_url(
        db_dir.path(),
        Network::Regtest,
        rest_url.clone(),
    )?;
    chain.sync(1000)?;
    drop(chain);

    let listener = TcpListener::bind("127.0.0.1:0").await?;
    let addr = listener.local_addr()?;
    let cache_dir = TempDir::with_prefix("bindex-electrum-cache")?;
    let monitor_path = cache_dir.path().join("electrum-monitor.json");
    let config = regtest_config(&node, &db_dir, &cache_dir, addr)?;
    let server = Server::new(config)?;
    let server_task =
        tokio::spawn(server.run_tcp_listener_until_shutdown(listener, future::pending()));

    let mut client = ElectrumClient::connect(addr).await?;
    assert_eq!(
        client
            .call("server.version", json!(["e2e", ["1.4", "1.6"]]))
            .await?,
        json!(["bindex-electrum", "1.6"])
    );

    let tip = client
        .call("blockchain.headers.subscribe", json!([]))
        .await?;
    assert_eq!(tip["height"], 102);
    assert_eq!(tip["hex"].as_str().context("header hex")?.len(), 160);

    let header = client.call("blockchain.block.header", json!([102])).await?;
    assert_eq!(header.as_str().context("block header hex")?.len(), 160);

    let headers = client
        .call("blockchain.block.headers", json!([102, 1, 0]))
        .await?;
    assert_eq!(headers["count"], 1);
    assert_eq!(headers["hex"], header);

    let raw_hex = hex::encode(serialize(&tx));
    assert_eq!(
        client
            .call("blockchain.transaction.get", json!([txid.to_string()]))
            .await?,
        json!(raw_hex)
    );

    let scripthash = ElectrumScripthash::from_script(recipient_script.as_script()).to_string();
    let history = client
        .call("blockchain.scripthash.get_history", json!([scripthash]))
        .await?;
    assert_eq!(history.as_array().context("history array")?.len(), 1);
    assert_eq!(history[0]["tx_hash"], txid.to_string());
    assert_eq!(history[0]["height"], 102);

    let listunspent = client
        .call(
            "blockchain.scripthash.listunspent",
            json!([ElectrumScripthash::from_script(recipient_script.as_script()).to_string()]),
        )
        .await?;
    assert_eq!(listunspent.as_array().context("utxo array")?.len(), 1);
    assert_eq!(listunspent[0]["tx_hash"], txid.to_string());
    assert_eq!(listunspent[0]["tx_pos"], recipient_vout);
    assert_eq!(listunspent[0]["value"], amount.to_sat());

    let balance = client
        .call(
            "blockchain.scripthash.get_balance",
            json!([ElectrumScripthash::from_script(recipient_script.as_script()).to_string()]),
        )
        .await?;
    assert_eq!(balance["confirmed"], amount.to_sat());
    assert_eq!(balance["unconfirmed"], 0);

    let merkle = client
        .call(
            "blockchain.transaction.get_merkle",
            json!([txid.to_string(), 102]),
        )
        .await?;
    assert_eq!(merkle["block_height"], 102);
    let pos = merkle["pos"].as_u64().context("merkle pos")?;

    assert_eq!(
        client
            .call("blockchain.transaction.id_from_pos", json!([102, pos]))
            .await?,
        json!(txid.to_string())
    );
    let id_with_merkle = client
        .call(
            "blockchain.transaction.id_from_pos",
            json!([102, pos, true]),
        )
        .await?;
    assert_eq!(id_with_merkle["tx_hash"], txid.to_string());
    assert_eq!(id_with_merkle["merkle"], merkle["merkle"]);

    let monitor: Value = serde_json::from_slice(&std::fs::read(&monitor_path)?)?;
    assert_eq!(monitor["ok"], true);
    assert_eq!(monitor["active_sessions"], 1);
    assert!(monitor["total_requests"].as_u64().unwrap_or_default() >= 9);
    assert!(monitor["methods"]
        .as_array()
        .unwrap_or(&Vec::new())
        .iter()
        .any(|method| method["method"] == "server.version"));

    assert_eq!(
        client.call("server.reset_monitor", json!([])).await?,
        json!(true)
    );
    let monitor: Value = serde_json::from_slice(&std::fs::read(&monitor_path)?)?;
    assert_eq!(monitor["ok"], true);
    assert_eq!(monitor["active_sessions"], 1);
    assert_eq!(monitor["total_requests"], 1);
    assert!(monitor["methods"]
        .as_array()
        .unwrap_or(&Vec::new())
        .iter()
        .any(|method| method["method"] == "server.reset_monitor"));

    server_task.abort();
    Ok(())
}

/// A wallet learns about payments through notifications: an incoming
/// unconfirmed payment, a spend of one of its confirmed outputs, and the block
/// that confirms both must each change the script's status, and the block must
/// reach header subscribers.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn subscribers_are_notified_of_mempool_and_block_changes() -> anyhow::Result<()> {
    let bitcoind = match exe_path() {
        Ok(path) => path,
        Err(err) => {
            eprintln!("skipping notification E2E test: BITCOIND_EXE is not set or invalid: {err}");
            return Ok(());
        }
    };

    let mut conf = Conf::default();
    conf.args.push("-rest");
    let node = Node::with_conf(bitcoind, &conf)?;
    let miner = node.client.new_address()?;
    node.client.generate_to_address(101, &miner)?;

    // the watched address owns one confirmed output
    let watched = node.client.new_address()?;
    let funding_txid = node
        .client
        .send_to_address(&watched, Amount::from_sat(1_000_000))?
        .txid()
        .context("send_to_address returned no txid")?;
    let funding = node
        .client
        .get_raw_transaction(funding_txid)?
        .transaction()?;
    let funding_vout = funding
        .output
        .iter()
        .position(|output| output.script_pubkey == watched.script_pubkey())
        .context("funding output not found")?;
    node.client.generate_to_address(1, &miner)?;
    // Keep the wallet from spending it: the spend below must be the only one,
    // or full-RBF lets it replace the incoming payment.
    let funding_outpoint = json!([{ "txid": funding_txid.to_string(), "vout": funding_vout }]);
    let locked: bool = node
        .client
        .call("lockunspent", &[json!(false), funding_outpoint.clone()])?;
    assert!(locked);

    // the test is the index writer, and keeps the primary open throughout
    let db_dir = TempDir::with_prefix("bindex-notify-db")?;
    let rest_url = format!("http://{}", node.params.rpc_socket);
    let mut writer =
        bindex::IndexedChain::open_with_rest_url(db_dir.path(), Network::Regtest, rest_url)?;
    writer.sync(1000)?;

    let listener = TcpListener::bind("127.0.0.1:0").await?;
    let addr = listener.local_addr()?;
    let cache_dir = TempDir::with_prefix("bindex-notify-cache")?;
    let mut config = regtest_config(&node, &db_dir, &cache_dir, addr)?;
    config.bitcoind_rpc_cookie = Some(node.params.cookie_file.clone());
    config.mempool_poll_secs = 1;
    config.secondary_refresh_ms = 200;
    let server = Server::new(config)?;
    server.spawn_secondary_refresh_task();
    server.spawn_mempool_poll_task();
    let server_task =
        tokio::spawn(server.run_tcp_listener_until_shutdown(listener, future::pending()));

    let mut client = ElectrumClient::connect(addr).await?;
    client
        .call("server.version", json!(["e2e", ["1.4", "1.6"]]))
        .await?;
    assert_eq!(
        client
            .call("blockchain.headers.subscribe", json!([]))
            .await?["height"],
        102
    );
    let scripthash =
        ElectrumScripthash::from_script(watched.script_pubkey().as_script()).to_string();
    let confirmed_status = client
        .call("blockchain.scripthash.subscribe", json!([scripthash]))
        .await?;
    assert!(confirmed_status.is_string());

    let limit = Duration::from_secs(15);

    // 1. an incoming payment, unconfirmed
    let incoming = node
        .client
        .send_to_address(&watched, Amount::from_sat(500_000))?
        .txid()
        .context("send_to_address returned no txid")?;
    let note = client.notification(limit).await?;
    assert_eq!(note["method"], "blockchain.scripthash.subscribe");
    assert_eq!(note["params"][0], scripthash);
    let incoming_status = note["params"][1].clone();
    assert_ne!(incoming_status, confirmed_status);
    assert_eq!(
        client
            .call("blockchain.scripthash.subscribe", json!([scripthash]))
            .await?,
        incoming_status,
        "resubscribing returns the status just notified"
    );

    // 2. an unconfirmed spend of the confirmed output, paying elsewhere
    node.client
        .call::<bool>("lockunspent", &[json!(true), funding_outpoint.clone()])?;
    let spend = funding_outpoint;
    let pay_to = json!([{ miner.to_string(): 0.0099 }]);
    let unsigned: String = node.client.call("createrawtransaction", &[spend, pay_to])?;
    let signed: Value = node
        .client
        .call("signrawtransactionwithwallet", &[json!(unsigned)])?;
    let spend_txid: String = node
        .client
        .call("sendrawtransaction", &[signed["hex"].clone()])?;
    let note = client.notification(limit).await?;
    assert_eq!(note["params"][0], scripthash);
    assert_ne!(note["params"][1], incoming_status);
    let mempool = client
        .call("blockchain.scripthash.get_mempool", json!([scripthash]))
        .await?;
    let mut unconfirmed = mempool
        .as_array()
        .context("get_mempool array")?
        .iter()
        .map(|entry| entry["tx_hash"].as_str().unwrap_or_default().to_string())
        .collect::<Vec<_>>();
    unconfirmed.sort();
    let mut expected = vec![incoming.to_string(), spend_txid.clone()];
    expected.sort();
    assert_eq!(
        unconfirmed, expected,
        "the spend is in the script's mempool"
    );

    // 3. a block confirming both
    node.client.generate_to_address(1, &miner)?;
    writer.sync(1000)?;
    let mut header = None;
    let mut status = None;
    while header.is_none() || status.is_none() {
        let note = client.notification(limit).await?;
        match note["method"].as_str() {
            Some("blockchain.headers.subscribe") => header = Some(note["params"][0].clone()),
            Some("blockchain.scripthash.subscribe") => status = Some(note["params"][1].clone()),
            other => anyhow::bail!("unexpected notification {other:?}"),
        }
    }
    assert_eq!(header.context("header")?["height"], 103);
    let history = client
        .call("blockchain.scripthash.get_history", json!([scripthash]))
        .await?;
    let history = history.as_array().context("history array")?;
    assert_eq!(
        history.len(),
        3,
        "funding, incoming payment and spend: {history:?}"
    );
    assert!(history
        .iter()
        .all(|entry| entry["height"].as_i64() > Some(0)));
    assert_eq!(
        client
            .call("blockchain.scripthash.subscribe", json!([scripthash]))
            .await?,
        status.context("status")?
    );

    // 4. unsubscribed scripts are not notified again
    assert_eq!(
        client
            .call("blockchain.scripthash.unsubscribe", json!([scripthash]))
            .await?,
        json!(true)
    );
    node.client
        .send_to_address(&watched, Amount::from_sat(300_000))?;
    tokio::time::sleep(Duration::from_secs(3)).await;
    client.call("server.ping", json!([])).await?;
    assert!(
        client.notifications.is_empty(),
        "{:?}",
        client.notifications
    );

    server_task.abort();
    Ok(())
}

/// The server configuration both tests use, against `node` and the index in
/// `db_dir`.
fn regtest_config(
    node: &Node,
    db_dir: &TempDir,
    cache_dir: &TempDir,
    addr: std::net::SocketAddr,
) -> anyhow::Result<Config> {
    Ok(Config {
        network: Network::Regtest,
        db_name: None,
        bindex_db_path: db_dir.path().to_path_buf(),
        secondary_path: None,
        bitcoind_rest_url: format!("http://{}", node.params.rpc_socket),
        bitcoind_rpc_url: format!("http://{}", node.params.rpc_socket),
        bitcoind_rpc_user: None,
        bitcoind_rpc_password: None,
        bitcoind_rpc_cookie: None,
        bitcoind_rpc_conf: None,
        tcp_listen: addr,
        tls_listen: None,
        tls_cert: None,
        tls_key: None,
        advertised_host: Vec::new(),
        cache_path: Some(cache_dir.path().join("electrum-cache.sqlite3")),
        monitor_path: Some(cache_dir.path().join("electrum-monitor.json")),
        protocol_min: ProtocolVersion::v1_4(),
        protocol_max: ProtocolVersion::v1_6(),
        mempool_poll_secs: 5,
        secondary_refresh_ms: 1000,
        zmq_rawtx: None,
        zmq_rawblock: None,
        max_batch_size: 100,
        max_subscriptions_per_session: 1000,
        broadcast_via: BroadcastVia::Bitcoind,
        tor_proxy: "127.0.0.1:9050".parse()?,
        tor_broadcast_url: None,
        tor_package_url: None,
        banner: "bindex electrum test".to_string(),
        peer: Vec::new(),
        donation_address: None,
        rest: RestConfig::default(),
    })
}

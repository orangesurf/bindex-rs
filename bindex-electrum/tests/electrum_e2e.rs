use std::future;

use anyhow::Context as _;
use bindex_electrum::{
    config::{BroadcastVia, Config},
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
}

impl ElectrumClient {
    async fn connect(addr: std::net::SocketAddr) -> anyhow::Result<Self> {
        let stream = TcpStream::connect(addr).await?;
        let (reader, writer) = stream.into_split();
        Ok(Self {
            writer,
            lines: BufReader::new(reader).lines(),
            next_id: 0,
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

        let line = self
            .lines
            .next_line()
            .await?
            .context("server closed connection")?;
        let response: Value = serde_json::from_str(&line)?;
        if let Some(error) = response.get("error") {
            anyhow::bail!("{method} returned error: {error}");
        }
        Ok(response
            .get("result")
            .cloned()
            .context("response missing result")?)
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
    let config = Config {
        network: Network::Regtest,
        bindex_db_path: db_dir.path().to_path_buf(),
        bitcoind_rest_url: rest_url,
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
        monitor_path: Some(monitor_path.clone()),
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
        banner: "bindex electrum test".to_string(),
        peer: Vec::new(),
        donation_address: None,
    };
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

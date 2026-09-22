#![cfg(not(feature = "liquid"))] // drives a regtest bitcoind; the REST API is Bitcoin-only
//! End-to-end cover for the Esplora-compatible REST API against a regtest node.

use std::{collections::HashMap, future, net::SocketAddr, time::Duration};

use anyhow::Context as _;
use bindex_electrum::{
    config::Config,
    rest::{self, RestApi},
    server::Server,
};
use bitcoin::{consensus::serialize, hashes::Hash as _, Amount, Network};
use clap::Parser as _;
use corepc_node::{exe_path, Conf, Node};
use serde_json::Value;
use tempfile::TempDir;
use tokio::{
    io::{AsyncReadExt as _, AsyncWriteExt as _},
    net::{TcpListener, TcpStream},
};

struct Response {
    status: u16,
    headers: HashMap<String, String>,
    body: String,
}

impl Response {
    fn json(&self) -> Value {
        serde_json::from_str(&self.body)
            .unwrap_or_else(|err| panic!("body is not JSON ({err}): {}", self.body))
    }

    fn cache_control(&self) -> Option<&str> {
        self.headers.get("cache-control").map(String::as_str)
    }
}

/// Minimal HTTP/1.1 client: one request per connection, read to EOF.
async fn request(
    addr: SocketAddr,
    method: &str,
    path: &str,
    body: Option<&str>,
) -> anyhow::Result<Response> {
    let mut stream = TcpStream::connect(addr).await?;
    let body = body.unwrap_or("");
    let head = format!(
        "{method} {path} HTTP/1.1\r\nHost: {addr}\r\nConnection: close\r\nContent-Length: {}\r\n\r\n",
        body.len()
    );
    stream.write_all(head.as_bytes()).await?;
    stream.write_all(body.as_bytes()).await?;
    let mut raw = Vec::new();
    stream.read_to_end(&mut raw).await?;
    let text = String::from_utf8_lossy(&raw).into_owned();
    let (head, body) = text
        .split_once("\r\n\r\n")
        .context("response without a header terminator")?;
    let mut lines = head.split("\r\n");
    let status = lines
        .next()
        .and_then(|line| line.split(' ').nth(1))
        .and_then(|code| code.parse().ok())
        .context("response without a status code")?;
    let headers = lines
        .filter_map(|line| line.split_once(": "))
        .map(|(name, value)| (name.to_ascii_lowercase(), value.to_string()))
        .collect();
    Ok(Response {
        status,
        headers,
        body: body.to_string(),
    })
}

async fn get(addr: SocketAddr, path: &str) -> anyhow::Result<Response> {
    request(addr, "GET", path, None).await
}

async fn get_ok(addr: SocketAddr, path: &str) -> anyhow::Result<Response> {
    let response = get(addr, path).await?;
    anyhow::ensure!(
        response.status == 200,
        "GET {path} -> {} {}",
        response.status,
        response.body
    );
    Ok(response)
}

#[tokio::test(flavor = "multi_thread", worker_threads = 6)]
async fn rest_api_serves_a_regtest_chain() -> anyhow::Result<()> {
    let bitcoind = match exe_path() {
        Ok(path) => path,
        Err(err) => {
            eprintln!("skipping REST E2E test: BITCOIND_EXE is not set or invalid: {err}");
            return Ok(());
        }
    };

    let mut conf = Conf::default();
    conf.args.push("-rest");
    let node = Node::with_conf(bitcoind, &conf)?;

    let miner = node.client.new_address()?;
    node.client.generate_to_address(101, &miner)?;

    // Pay addresses the node's wallet does not own, so coin selection for the
    // unconfirmed payment below can never spend them and the assertions stay
    // deterministic.
    let recipient = external_address(7);
    let amount = Amount::from_sat(50_000);
    let txid = node
        .client
        .send_to_address(&recipient, amount)?
        .txid()
        .context("send_to_address returned no txid")?;
    let tx = node.client.get_raw_transaction(txid)?.transaction()?;
    let spent_outpoint = tx.input[0].previous_output;
    let recipient_script = recipient.script_pubkey();
    let recipient_vout = tx
        .output
        .iter()
        .position(|output| output.script_pubkey == recipient_script)
        .context("recipient output not found")? as u32;
    node.client.generate_to_address(1, &miner)?;

    // and an unconfirmed one
    let pending_to = external_address(8);
    let pending_amount = Amount::from_sat(70_000);
    let pending_txid = node
        .client
        .send_to_address(&pending_to, pending_amount)?
        .txid()
        .context("send_to_address returned no txid")?;
    let pending_tx = node.client.get_raw_transaction(pending_txid)?.transaction()?;

    let db_dir = TempDir::with_prefix("bindex-rest-db")?;
    let rest_url = format!("http://{}", node.params.rpc_socket);
    let mut chain =
        bindex::IndexedChain::open_with_rest_url(db_dir.path(), Network::Regtest, rest_url.clone())?;
    chain.sync(1000)?;
    drop(chain);

    let state_dir = TempDir::with_prefix("bindex-rest-state")?;
    let config = Config::try_parse_from([
        "bindex-electrum",
        "--network",
        "regtest",
        "--bindex-db-path",
        db_dir.path().to_str().context("db path")?,
        "--bitcoind-rest-url",
        &rest_url,
        "--bitcoind-rpc-url",
        &rest_url,
        "--bitcoind-rpc-cookie",
        node.params.cookie_file.to_str().context("cookie path")?,
        "--tcp-listen",
        "127.0.0.1:0",
        "--cache-path",
        state_dir.path().join("cache.sqlite3").to_str().context("cache")?,
        "--monitor-path",
        state_dir.path().join("monitor.json").to_str().context("monitor")?,
        "--mempool-poll-secs",
        "1",
        "--secondary-refresh-ms",
        "500",
        "--broadcast-via",
        "bitcoind",
        // enables the mempool poller; the listener below binds its own port
        "--http-addr",
        "127.0.0.1:0",
    ])?;
    config.validate()?;

    let electrum = TcpListener::bind("127.0.0.1:0").await?;
    let http = TcpListener::bind("127.0.0.1:0").await?;
    let addr = http.local_addr()?;

    let server = Server::new(config)?;
    server.spawn_mempool_poll_task();
    let api = RestApi::new(server.clone());
    let rest_task = tokio::spawn(rest::run_listener(api, http));
    let electrum_task =
        tokio::spawn(server.run_tcp_listener_until_shutdown(electrum, future::pending()));

    // ---------------------------------------------------------------- blocks

    let tip_height = get_ok(addr, "/blocks/tip/height").await?;
    assert_eq!(tip_height.body, "102");
    assert_eq!(tip_height.cache_control(), Some("public, max-age=10"));
    assert_eq!(
        tip_height.headers.get("x-powered-by").map(String::as_str),
        Some(concat!("bindex-electrum/", env!("CARGO_PKG_VERSION")))
    );

    let tip_hash = get_ok(addr, "/blocks/tip/hash").await?.body;
    assert_eq!(get_ok(addr, "/block-height/102").await?.body, tip_hash);

    let block = get_ok(addr, &format!("/block/{tip_hash}")).await?;
    assert_eq!(block.cache_control(), Some("public, max-age=157784630"));
    let block = block.json();
    assert_eq!(block["id"], tip_hash);
    assert_eq!(block["height"], 102);
    assert_eq!(block["tx_count"], 2);
    for field in [
        "version",
        "timestamp",
        "size",
        "weight",
        "merkle_root",
        "previousblockhash",
        "mediantime",
        "nonce",
        "bits",
        "difficulty",
    ] {
        assert!(!block[field].is_null(), "block.{field} missing");
    }

    let genesis_hash = get_ok(addr, "/block-height/0").await?.body;
    let genesis = get_ok(addr, &format!("/block/{genesis_hash}")).await?.json();
    assert!(
        genesis["previousblockhash"].is_null(),
        "genesis keeps an explicit null previousblockhash"
    );

    let status = get_ok(addr, &format!("/block/{tip_hash}/status")).await?.json();
    assert_eq!(status["in_best_chain"], true);
    assert_eq!(status["height"], 102);
    assert!(status["next_best"].is_null());

    let blocks = get_ok(addr, "/blocks").await?.json();
    assert_eq!(blocks.as_array().context("blocks array")?.len(), 10);
    assert_eq!(blocks[0]["id"], tip_hash);
    assert_eq!(blocks[0]["height"], 102);
    assert_eq!(blocks[9]["height"], 93);
    // a non-numeric start means "from the tip"
    assert_eq!(get_ok(addr, "/blocks/tip").await?.json()[0]["id"], tip_hash);

    let txids = get_ok(addr, &format!("/block/{tip_hash}/txids")).await?.json();
    let txids = txids.as_array().context("txids array")?;
    assert_eq!(txids.len(), 2);
    assert_eq!(
        get_ok(addr, &format!("/block/{tip_hash}/txid/1")).await?.body,
        txids[1].as_str().context("txid")?
    );
    assert_eq!(
        get(addr, &format!("/block/{tip_hash}/txid/9")).await?.status,
        404
    );

    let header = get_ok(addr, &format!("/block/{tip_hash}/header")).await?.body;
    assert_eq!(header.len(), 160);
    let raw = get_ok(addr, &format!("/block/{tip_hash}/raw")).await?;
    assert_eq!(
        raw.headers.get("content-type").map(String::as_str),
        Some("application/octet-stream")
    );
    assert!(
        raw.headers
            .get("content-length")
            .and_then(|value| value.parse::<usize>().ok())
            .unwrap_or(0)
            > 80
    );

    let block_txs = get_ok(addr, &format!("/block/{tip_hash}/txs")).await?.json();
    let block_txs = block_txs.as_array().context("block txs")?;
    assert_eq!(block_txs.len(), 2);
    assert_eq!(block_txs[0]["vin"][0]["is_coinbase"], true);
    assert!(block_txs[0]["vin"][0]["prevout"].is_null());
    assert_eq!(block_txs[0]["fee"], 0);
    // the non-coinbase transaction got its prevouts from /rest/spenttxouts
    let payment = &block_txs[1];
    assert_eq!(payment["txid"], txid.to_string());
    assert!(!payment["vin"][0]["prevout"].is_null());
    assert!(payment["fee"].as_u64().context("fee")? > 0);
    assert_eq!(payment["status"]["confirmed"], true);
    assert_eq!(payment["status"]["block_height"], 102);

    // in range but not a page boundary (the reference's typo included)
    let unaligned = get(addr, &format!("/block/{tip_hash}/txs/1")).await?;
    assert_eq!(unaligned.status, 400);
    assert_eq!(unaligned.body, "start index must be a multipication of 25");
    let past_end = get(addr, &format!("/block/{tip_hash}/txs/25")).await?;
    assert_eq!(past_end.status, 404);
    assert_eq!(past_end.body, "start index out of range");
    let internal = get_ok(addr, &format!("/internal/block/{tip_hash}/txs"))
        .await?
        .json();
    assert_eq!(internal.as_array().context("internal txs")?.len(), 2);

    // ---------------------------------------------------------------- transactions

    let tx_json = get_ok(addr, &format!("/tx/{txid}")).await?.json();
    assert_eq!(tx_json["txid"], txid.to_string());
    assert_eq!(tx_json["size"], tx.total_size());
    assert_eq!(tx_json["weight"], tx.weight().to_wu());
    assert!(tx_json["sigops"].as_u64().is_some());
    assert_eq!(tx_json["vout"][recipient_vout as usize]["value"], amount.to_sat());
    assert_eq!(
        tx_json["vout"][recipient_vout as usize]["scriptpubkey_address"],
        recipient.to_string()
    );
    let prevout_value = tx_json["vin"][0]["prevout"]["value"].as_u64().context("prevout")?;
    let out_total: u64 = tx
        .output
        .iter()
        .map(|output| output.value.to_sat())
        .sum();
    assert_eq!(tx_json["fee"], prevout_value - out_total);

    assert_eq!(
        get_ok(addr, &format!("/tx/{txid}/hex")).await?.body,
        hex::encode(serialize(&tx))
    );
    let tx_raw = get_ok(addr, &format!("/tx/{txid}/raw")).await?;
    assert_eq!(
        tx_raw.headers.get("content-type").map(String::as_str),
        Some("application/octet-stream")
    );
    assert_eq!(
        tx_raw.headers.get("content-length").map(String::as_str),
        Some(serialize(&tx).len().to_string().as_str())
    );
    let tx_status = get_ok(addr, &format!("/tx/{txid}/status")).await?.json();
    assert_eq!(tx_status["confirmed"], true);
    assert_eq!(tx_status["block_height"], 102);

    let proof = get_ok(addr, &format!("/tx/{txid}/merkle-proof")).await?.json();
    assert_eq!(proof["block_height"], 102);
    assert_eq!(proof["pos"], 1);
    let merkleblock = get_ok(addr, &format!("/tx/{txid}/merkleblock-proof")).await?.body;
    assert!(merkleblock.len() > 160, "{merkleblock}");

    assert_eq!(get(addr, &format!("/tx/{}", "ab".repeat(32))).await?.status, 404);
    assert_eq!(get(addr, "/tx/nothex").await?.status, 400);

    // the transaction spends a coinbase output, so that output is spent by it
    let outspend = get_ok(
        addr,
        &format!(
            "/tx/{}/outspend/{}",
            spent_outpoint.txid, spent_outpoint.vout
        ),
    )
    .await?
    .json();
    assert_eq!(outspend["spent"], true);
    assert_eq!(outspend["txid"], txid.to_string());
    assert_eq!(outspend["vin"], 0);
    assert_eq!(outspend["status"]["block_height"], 102);

    let outspends = get_ok(addr, &format!("/tx/{txid}/outspends")).await?.json();
    let outspends = outspends.as_array().context("outspends")?;
    assert_eq!(outspends.len(), tx.output.len());
    assert_eq!(outspends[recipient_vout as usize]["spent"], false);

    let batch = get_ok(addr, &format!("/txs/outspends?txids={txid},{}", "ab".repeat(32)))
        .await?
        .json();
    assert_eq!(batch.as_array().context("batch")?.len(), 2);
    assert_eq!(batch[1].as_array().context("unknown txid")?.len(), 0);
    assert_eq!(get(addr, "/txs/outspends").await?.status, 400);

    let internal_txs = request(
        addr,
        "POST",
        "/internal/txs",
        Some(&format!("[\"{txid}\"]")),
    )
    .await?;
    assert_eq!(internal_txs.status, 200);
    assert_eq!(internal_txs.json()[0]["txid"], txid.to_string());

    let by_outpoint = request(
        addr,
        "POST",
        "/internal/txs/outspends/by-outpoint",
        Some(&format!(
            "[\"{}:{}\", \"garbage\"]",
            spent_outpoint.txid, spent_outpoint.vout
        )),
    )
    .await?
    .json();
    assert_eq!(by_outpoint[0]["spent"], true);
    assert_eq!(by_outpoint[1]["spent"], false);

    // ---------------------------------------------------------------- addresses

    let stats = get_ok(addr, &format!("/address/{recipient}")).await?.json();
    assert_eq!(stats["address"], recipient.to_string());
    assert_eq!(stats["chain_stats"]["tx_count"], 1);
    assert_eq!(stats["chain_stats"]["funded_txo_count"], 1);
    assert_eq!(stats["chain_stats"]["funded_txo_sum"], amount.to_sat());
    assert_eq!(stats["chain_stats"]["spent_txo_count"], 0);
    assert_eq!(stats["mempool_stats"]["tx_count"], 0);

    // the same script through the REST scripthash form (not byte-reversed)
    let scripthash = hex::encode(
        bitcoin::hashes::sha256::Hash::hash(recipient_script.as_bytes()).to_byte_array(),
    );
    let by_scripthash = get_ok(addr, &format!("/scripthash/{scripthash}")).await?.json();
    assert_eq!(by_scripthash["scripthash"], scripthash);
    assert_eq!(by_scripthash["chain_stats"], stats["chain_stats"]);

    let utxos = get_ok(addr, &format!("/address/{recipient}/utxo")).await?.json();
    let utxos = utxos.as_array().context("utxos")?;
    assert_eq!(utxos.len(), 1);
    assert_eq!(utxos[0]["txid"], txid.to_string());
    assert_eq!(utxos[0]["vout"], recipient_vout);
    assert_eq!(utxos[0]["value"], amount.to_sat());
    assert_eq!(utxos[0]["status"]["confirmed"], true);

    let address_txs = get_ok(addr, &format!("/address/{recipient}/txs")).await?.json();
    assert_eq!(address_txs.as_array().context("address txs")?.len(), 1);
    assert_eq!(address_txs[0]["txid"], txid.to_string());
    let chain_txs = get_ok(addr, &format!("/address/{recipient}/txs/chain")).await?.json();
    assert_eq!(chain_txs.as_array().context("chain txs")?.len(), 1);
    // paging past the only transaction yields nothing
    let page_two = get_ok(addr, &format!("/address/{recipient}/txs/chain/{txid}"))
        .await?
        .json();
    assert_eq!(page_two.as_array().context("page two")?.len(), 0);
    assert_eq!(
        get(
            addr,
            &format!("/address/{recipient}/txs?after_txid={}", "ab".repeat(32))
        )
        .await?
        .status,
        422
    );

    let summary = get_ok(addr, &format!("/address/{recipient}/txs/summary"))
        .await?
        .json();
    assert_eq!(summary[0]["txid"], txid.to_string());
    assert_eq!(summary[0]["height"], 102);
    assert_eq!(summary[0]["value"], amount.to_sat());
    assert_eq!(summary[0]["tx_position"], 1);

    let multi = request(
        addr,
        "POST",
        "/addresses/txs",
        Some(&format!("[\"{recipient}\", \"not-an-address\"]")),
    )
    .await?;
    assert_eq!(multi.status, 200);
    assert_eq!(multi.json()[0]["txid"], txid.to_string());

    assert_eq!(get(addr, "/address/not-an-address").await?.status, 400);
    assert_eq!(get(addr, "/scripthash/00").await?.status, 400);
    assert_eq!(get(addr, "/address-prefix/bcrt1").await?.status, 400);

    // ---------------------------------------------------------------- mempool

    wait_for_mempool(addr, 1).await?;

    let mempool = get_ok(addr, "/mempool").await?.json();
    assert_eq!(mempool["count"], 1);
    assert!(mempool["vsize"].as_u64().context("vsize")? > 0);
    assert!(mempool["total_fee"].as_u64().context("total_fee")? > 0);
    assert_eq!(
        mempool["fee_histogram"]
            .as_array()
            .context("histogram")?
            .len(),
        1
    );

    let mempool_txids = get_ok(addr, "/mempool/txids").await?.json();
    assert_eq!(mempool_txids[0], pending_txid.to_string());
    let paged = get_ok(addr, "/mempool/txids/page").await?.json();
    assert_eq!(paged[0], pending_txid.to_string());
    let recent = get_ok(addr, "/mempool/recent").await?;
    assert_eq!(recent.cache_control(), Some("public, max-age=5"));
    let recent = recent.json();
    assert_eq!(recent[0]["txid"], pending_txid.to_string());
    assert_eq!(
        recent[0]["value"].as_u64().context("value")?,
        pending_tx.output.iter().map(|o| o.value.to_sat()).sum::<u64>()
    );

    let mempool_txs = get_ok(addr, "/internal/mempool/txs").await?.json();
    assert_eq!(mempool_txs[0]["txid"], pending_txid.to_string());
    assert_eq!(mempool_txs[0]["status"]["confirmed"], false);
    assert!(!mempool_txs[0]["vin"][0]["prevout"].is_null());
    let all = get_ok(addr, "/internal/mempool/txs/all").await?.json();
    assert_eq!(all.as_array().context("all mempool txs")?.len(), 1);

    // the pending payment shows up on its recipient
    let pending_stats = get_ok(addr, &format!("/address/{pending_to}")).await?.json();
    assert_eq!(pending_stats["chain_stats"]["tx_count"], 0);
    assert_eq!(pending_stats["mempool_stats"]["tx_count"], 1);
    assert_eq!(
        pending_stats["mempool_stats"]["funded_txo_sum"],
        pending_amount.to_sat()
    );
    let pending_mempool_txs = get_ok(addr, &format!("/address/{pending_to}/txs/mempool"))
        .await?
        .json();
    assert_eq!(pending_mempool_txs[0]["txid"], pending_txid.to_string());
    let pending_utxos = get_ok(addr, &format!("/address/{pending_to}/utxo")).await?.json();
    assert_eq!(pending_utxos[0]["status"]["confirmed"], false);
    assert_eq!(pending_utxos[0]["value"], pending_amount.to_sat());

    // and it is the spender of whatever it spends
    let pending_spent = pending_tx.input[0].previous_output;
    let pending_outspend = get_ok(
        addr,
        &format!("/tx/{}/outspend/{}", pending_spent.txid, pending_spent.vout),
    )
    .await?
    .json();
    assert_eq!(pending_outspend["spent"], true);
    assert_eq!(pending_outspend["txid"], pending_txid.to_string());
    assert_eq!(pending_outspend["status"]["confirmed"], false);

    // ---------------------------------------------------------------- broadcast

    let rebroadcast = request(
        addr,
        "POST",
        "/tx",
        Some(&hex::encode(serialize(&pending_tx))),
    )
    .await?;
    assert_eq!(rebroadcast.status, 200, "{}", rebroadcast.body);
    assert_eq!(rebroadcast.body, pending_txid.to_string());
    assert_eq!(rebroadcast.cache_control(), Some("public, max-age=0"));

    let bad = request(addr, "POST", "/tx", Some("00")).await?;
    assert_eq!(bad.status, 400);

    assert_eq!(get(addr, "/broadcast").await?.status, 400);
    assert_eq!(get(addr, "/broadcast").await?.body, "Missing tx");

    let test_accept = request(
        addr,
        "POST",
        "/txs/test",
        Some(&format!("[\"{}\"]", hex::encode(serialize(&pending_tx)))),
    )
    .await?;
    assert_eq!(test_accept.status, 200, "{}", test_accept.body);
    assert_eq!(test_accept.json()[0]["txid"], pending_txid.to_string());
    let too_short = request(addr, "POST", "/txs/test", Some("[\"00\"]")).await?;
    assert_eq!(too_short.status, 400);
    assert_eq!(too_short.body, "Invalid transaction size/hex for item 0");
    let bad_rate = request(
        addr,
        "POST",
        "/txs/test?maxfeerate=abc",
        Some("[]"),
    )
    .await?;
    assert_eq!(bad_rate.status, 400);
    assert_eq!(bad_rate.body, "Invalid maxfeerate");

    let fees = get_ok(addr, "/fee-estimates").await?.json();
    assert!(fees.is_object(), "fee estimates: {fees}");

    // ---------------------------------------------------------------- fallthrough

    let missing = get(addr, "/does/not/exist").await?;
    assert_eq!(missing.status, 404);
    assert_eq!(missing.body, "endpoint does not exist \"/does/not/exist\"");
    assert_eq!(missing.cache_control(), None);
    // no 405: a wrong method is just an unknown path
    assert_eq!(request(addr, "PUT", "/blocks/tip/hash", None).await?.status, 404);

    rest_task.abort();
    electrum_task.abort();
    Ok(())
}

/// A regtest P2WPKH address outside the node's wallet.
fn external_address(seed: u8) -> bitcoin::Address {
    let script = bitcoin::ScriptBuf::new_p2wpkh(&bitcoin::WPubkeyHash::from_byte_array(
        [seed; 20],
    ));
    bitcoin::Address::from_script(&script, Network::Regtest).expect("p2wpkh address")
}

/// The poller runs on a one-second tick; give it a few of them.
async fn wait_for_mempool(addr: SocketAddr, expected: usize) -> anyhow::Result<()> {
    for _ in 0..40 {
        let mempool = get_ok(addr, "/mempool").await?.json();
        if mempool["count"].as_u64().unwrap_or(0) as usize >= expected {
            return Ok(());
        }
        tokio::time::sleep(Duration::from_millis(250)).await;
    }
    anyhow::bail!("mempool never reached {expected} transactions")
}

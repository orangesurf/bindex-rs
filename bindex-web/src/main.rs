//! Tiny web UI for address / txid search over a bindex index.
//!
//! This is the "embed bindex as a library" usage model: a single process owns
//! the RocksDB index, keeps it synced to bitcoind in a background thread, and
//! answers queries in-process over HTTP. Because there is exactly one opener of
//! the index, there is no RocksDB single-writer lock contention.
//!
//! Endpoints:
//!   GET /                      static search page
//!   GET /api/address/<addr>    address history (reuses bindex's cache layer)
//!   GET /api/tx/<txid>         transaction summary (via locations_by_txid)
//!   GET /api/status            tip height / sync status

use std::collections::{BTreeMap, VecDeque};
use std::fs;
use std::io::{BufRead, BufReader, Write};
use std::net::{SocketAddr, TcpStream};
use std::sync::{Arc, Mutex, RwLock};
use std::thread;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use anyhow::{anyhow, Context, Result};
use bindex::bitcoin::{self, consensus::deserialize, hashes::Hash, Address, Network, Txid};
use bindex::{cache, IndexedChain};
use log::{error, info, warn};
use tiny_http::{Header, Method, Request, Response, Server};

const DB_PATH: &str = "./db";
const ELECTRUM_MONITOR_PATH: &str = "./db/electrum-monitor.json";
const ELECTRUM_CONTROL_ADDR: &str = "127.0.0.1:50001";
const NETWORK: Network = Network::Bitcoin;
const ADDR: &str = "127.0.0.1:9200";
const WORKERS: usize = 4;

type Chain = Arc<RwLock<IndexedChain>>;
type Metrics = Arc<Monitor>;

const RECENT_LIMIT: usize = 120;
const LATENCY_LIMIT: usize = 512;

struct Monitor {
    started_at: Instant,
    started_unix: u64,
    state: Mutex<MonitorState>,
}

#[derive(Default)]
struct MonitorState {
    routes: BTreeMap<String, RouteMetrics>,
    recent: VecDeque<QuerySample>,
    sync: SyncMetrics,
}

#[derive(Default)]
struct RouteMetrics {
    count: u64,
    errors: u64,
    total_ms: f64,
    max_ms: f64,
    samples_ms: VecDeque<f64>,
}

#[derive(Default)]
struct SyncMetrics {
    runs: u64,
    indexed_blocks: u64,
    bytes_read: u64,
    last_started_unix: Option<u64>,
    last_finished_unix: Option<u64>,
    last_indexed_blocks: Option<usize>,
    last_elapsed_ms: Option<f64>,
    last_tip: Option<String>,
    last_error: Option<String>,
}

struct QuerySample {
    unix: u64,
    route: String,
    target: String,
    ok: bool,
    duration_ms: f64,
    error: Option<String>,
}

impl Monitor {
    fn new() -> Self {
        Self {
            started_at: Instant::now(),
            started_unix: unix_now(),
            state: Mutex::new(MonitorState::default()),
        }
    }

    fn record_request(
        &self,
        route: &str,
        target: impl Into<String>,
        ok: bool,
        duration: Duration,
        error: Option<String>,
    ) {
        let duration_ms = duration.as_secs_f64() * 1000.0;
        let mut state = self.state.lock().expect("monitor lock");
        let route_metrics = state.routes.entry(route.to_string()).or_default();
        route_metrics.count += 1;
        if !ok {
            route_metrics.errors += 1;
        }
        route_metrics.total_ms += duration_ms;
        route_metrics.max_ms = route_metrics.max_ms.max(duration_ms);
        route_metrics.samples_ms.push_back(duration_ms);
        while route_metrics.samples_ms.len() > LATENCY_LIMIT {
            route_metrics.samples_ms.pop_front();
        }

        state.recent.push_front(QuerySample {
            unix: unix_now(),
            route: route.to_string(),
            target: target.into(),
            ok,
            duration_ms,
            error,
        });
        while state.recent.len() > RECENT_LIMIT {
            state.recent.pop_back();
        }
    }

    fn record_sync_success(&self, stats: &bindex::Stats) {
        let mut state = self.state.lock().expect("monitor lock");
        state.sync.runs += 1;
        state.sync.indexed_blocks += stats.indexed_blocks as u64;
        state.sync.bytes_read += stats.size_read as u64;
        state.sync.last_finished_unix = Some(unix_now());
        state.sync.last_indexed_blocks = Some(stats.indexed_blocks);
        state.sync.last_elapsed_ms = Some(stats.elapsed.as_secs_f64() * 1000.0);
        state.sync.last_tip = Some(stats.tip.to_string());
        state.sync.last_error = None;
    }

    fn record_sync_start(&self) {
        self.state
            .lock()
            .expect("monitor lock")
            .sync
            .last_started_unix = Some(unix_now());
    }

    fn record_sync_error(&self, error: &str) {
        let mut state = self.state.lock().expect("monitor lock");
        state.sync.last_finished_unix = Some(unix_now());
        state.sync.last_error = Some(error.to_string());
    }

    fn reset_request_metrics(&self) {
        let mut state = self.state.lock().expect("monitor lock");
        state.routes.clear();
        state.recent.clear();
    }

    fn json(&self, chain: &Chain) -> serde_json::Value {
        let chain_status = status_json(chain).unwrap_or_else(err_json);
        let electrum = electrum_monitor_json();
        let state = self.state.lock().expect("monitor lock");
        let routes = state
            .routes
            .iter()
            .map(|(route, metrics)| {
                let mut samples = metrics.samples_ms.iter().copied().collect::<Vec<_>>();
                samples.sort_by(|a, b| a.total_cmp(b));
                let p50 = percentile(&samples, 0.50);
                let p95 = percentile(&samples, 0.95);
                serde_json::json!({
                    "route": route,
                    "count": metrics.count,
                    "errors": metrics.errors,
                    "avg_ms": if metrics.count == 0 { 0.0 } else { metrics.total_ms / metrics.count as f64 },
                    "p50_ms": p50,
                    "p95_ms": p95,
                    "max_ms": metrics.max_ms,
                })
            })
            .collect::<Vec<_>>();
        let recent = state
            .recent
            .iter()
            .map(|item| {
                serde_json::json!({
                    "time": item.unix,
                    "route": item.route,
                    "target": item.target,
                    "ok": item.ok,
                    "duration_ms": item.duration_ms,
                    "error": item.error,
                })
            })
            .collect::<Vec<_>>();
        serde_json::json!({
            "ok": true,
            "started_at": self.started_unix,
            "uptime_secs": self.started_at.elapsed().as_secs(),
            "workers": WORKERS,
            "chain": chain_status,
            "sync": {
                "runs": state.sync.runs,
                "indexed_blocks": state.sync.indexed_blocks,
                "bytes_read": state.sync.bytes_read,
                "last_started_at": state.sync.last_started_unix,
                "last_finished_at": state.sync.last_finished_unix,
                "last_indexed_blocks": state.sync.last_indexed_blocks,
                "last_elapsed_ms": state.sync.last_elapsed_ms,
                "last_tip": state.sync.last_tip,
                "last_error": state.sync.last_error,
            },
            "routes": routes,
            "recent": recent,
            "electrum": electrum,
        })
    }
}

fn main() {
    env_logger::builder().format_timestamp_micros().init();
    if let Err(e) = run() {
        error!("fatal: {e:?}");
        std::process::exit(1);
    }
}

fn run() -> Result<()> {
    let chain = IndexedChain::open(DB_PATH, NETWORK, None).context("open index")?;
    let chain: Chain = Arc::new(RwLock::new(chain));
    let metrics: Metrics = Arc::new(Monitor::new());

    // Background syncer: keep the index following bitcoind's tip.
    {
        let chain = Arc::clone(&chain);
        let metrics = Arc::clone(&metrics);
        thread::Builder::new()
            .name("syncer".into())
            .spawn(move || sync_loop(chain, metrics))?;
    }

    let server = Server::http(ADDR).map_err(|e| anyhow!("bind {ADDR}: {e}"))?;
    let server = Arc::new(server);
    info!("listening on http://{ADDR}");

    let mut handles = vec![];
    for _ in 0..WORKERS {
        let server = Arc::clone(&server);
        let chain = Arc::clone(&chain);
        let metrics = Arc::clone(&metrics);
        handles.push(thread::spawn(move || {
            for req in server.incoming_requests() {
                let chain = Arc::clone(&chain);
                let metrics = Arc::clone(&metrics);
                if let Err(e) = handle(req, &chain, &metrics) {
                    warn!("request error: {e:#}");
                }
            }
        }));
    }
    for h in handles {
        let _ = h.join();
    }
    Ok(())
}

fn sync_loop(chain: Chain, metrics: Metrics) {
    loop {
        match chain.write() {
            Ok(mut c) => loop {
                metrics.record_sync_start();
                match c.sync(1000) {
                    Ok(stats) if stats.indexed_blocks > 0 => {
                        metrics.record_sync_success(&stats);
                        continue;
                    }
                    Ok(stats) => {
                        metrics.record_sync_success(&stats);
                        break;
                    }
                    Err(e) => {
                        warn!("sync error (will retry): {e}");
                        metrics.record_sync_error(&e.to_string());
                        break;
                    }
                }
            },
            Err(e) => error!("chain lock poisoned: {e}"),
        }
        thread::sleep(Duration::from_secs(2));
    }
}

// ---- HTTP routing ------------------------------------------------------------

fn handle(req: Request, chain: &Chain, metrics: &Metrics) -> Result<()> {
    let start = Instant::now();
    let url = req.url().to_string();
    let path = url.split('?').next().unwrap_or("");

    if path == "/api/monitor/reset" && req.method() == &Method::Post {
        metrics.reset_request_metrics();
        let json = reset_electrum_monitor().unwrap_or_else(err_json);
        metrics.record_request(
            "monitor_reset",
            "/api/monitor/reset",
            json_ok(&json),
            start.elapsed(),
            json_error(&json),
        );
        return respond_json(req, &json);
    }

    if req.method() != &Method::Get {
        metrics.record_request(
            "method",
            path,
            false,
            start.elapsed(),
            Some("method not allowed".into()),
        );
        return respond(req, 405, "text/plain", b"method not allowed".to_vec());
    }

    if path == "/" {
        metrics.record_request("page", "/", true, start.elapsed(), None);
        return respond(req, 200, "text/html; charset=utf-8", INDEX_HTML.into());
    }
    if path == "/monitor" {
        metrics.record_request("page", "/monitor", true, start.elapsed(), None);
        return respond(req, 200, "text/html; charset=utf-8", MONITOR_HTML.into());
    }
    if path == "/api/status" {
        let json = status_json(chain).unwrap_or_else(err_json);
        metrics.record_request(
            "status",
            "/api/status",
            json_ok(&json),
            start.elapsed(),
            json_error(&json),
        );
        return respond_json(req, &json);
    }
    if path == "/api/monitor" {
        let json = metrics.json(chain);
        metrics.record_request("monitor", "/api/monitor", true, start.elapsed(), None);
        return respond_json(req, &json);
    }
    if let Some(rest) = path.strip_prefix("/api/address/") {
        let q = pct_decode(rest);
        let json = address_json(chain, &q).unwrap_or_else(err_json);
        metrics.record_request(
            "address",
            &q,
            json_ok(&json),
            start.elapsed(),
            json_error(&json),
        );
        return respond_json(req, &json);
    }
    if let Some(rest) = path.strip_prefix("/api/tx/") {
        let q = pct_decode(rest);
        let json = tx_json(chain, &q).unwrap_or_else(err_json);
        metrics.record_request("tx", &q, json_ok(&json), start.elapsed(), json_error(&json));
        return respond_json(req, &json);
    }
    metrics.record_request(
        "not_found",
        path,
        false,
        start.elapsed(),
        Some("not found".into()),
    );
    respond(req, 404, "text/plain", b"not found".to_vec())
}

// ---- query handlers ----------------------------------------------------------

/// Address history, computed by spinning up an in-memory cache for this one
/// address and letting bindex's cache layer derive its funding/spending history
/// (handles amounts and reorgs for us). Read-locks the chain for the scan.
fn address_json(chain: &Chain, addr_str: &str) -> Result<serde_json::Value> {
    let address = Address::from_str(addr_str.trim())
        .with_context(|| format!("invalid address {addr_str:?}"))?
        .require_network(NETWORK)
        .context("wrong network for address")?;

    let conn = rusqlite::Connection::open_in_memory()?;
    let cache = cache::Cache::open(conn).map_err(|e| anyhow!("cache open: {e}"))?;
    cache
        .add([address.clone()])
        .map_err(|e| anyhow!("cache add: {e}"))?;
    {
        let c = chain.read().map_err(|_| anyhow!("chain lock"))?;
        cache.sync(&c).map_err(|e| anyhow!("cache sync: {e}"))?;
    }

    let db = cache.db();
    let mut stmt = db.prepare(
        r"
        WITH history_deltas AS (
            SELECT block_offset, block_height, sum(amount) AS delta
            FROM history GROUP BY 1, 2
        )
        SELECT h.header_bytes, t.block_offset, t.block_height, t.tx_id, d.delta
        FROM history_deltas d, transactions t, headers h
        WHERE d.block_height = t.block_height
          AND d.block_offset = t.block_offset
          AND d.block_height = h.block_height
        ORDER BY d.block_height ASC, d.block_offset ASC",
    )?;

    let mut balance: i64 = 0;
    let mut txs = vec![];
    let mut rows = stmt.query([])?;
    while let Some(row) = rows.next()? {
        let header_bytes: Vec<u8> = row.get(0)?;
        let block_offset: u64 = row.get(1)?;
        let block_height: u64 = row.get(2)?;
        let txid_bytes: [u8; 32] = row.get(3)?;
        let delta: i64 = row.get(4)?;
        balance += delta;
        let header: bitcoin::block::Header =
            deserialize(&header_bytes).map_err(|e| anyhow!("bad header: {e}"))?;
        txs.push(serde_json::json!({
            "txid": Txid::from_byte_array(txid_bytes).to_string(),
            "height": block_height,
            "block_offset": block_offset,
            "time": header.time,
            "delta_sat": delta,
            "delta_btc": format!("{:+.8}", delta as f64 / 1e8),
            "balance_sat": balance,
            "balance_btc": format!("{:.8}", balance as f64 / 1e8),
        }));
    }

    let received: i64 = db.query_row(
        "SELECT coalesce(sum(amount), 0) FROM history WHERE amount > 0",
        [],
        |r| r.get(0),
    )?;
    // newest first for display
    txs.reverse();
    Ok(serde_json::json!({
        "ok": true,
        "kind": "address",
        "address": address.to_string(),
        "tx_count": txs.len(),
        "balance_sat": balance,
        "balance_btc": format!("{:.8}", balance as f64 / 1e8),
        "total_received_btc": format!("{:.8}", received as f64 / 1e8),
        "txs": txs,
    }))
}

/// Transaction summary by txid. `locations_by_txid` can yield false positives
/// (txid prefix index), so each candidate is fetched and verified.
fn tx_json(chain: &Chain, txid_str: &str) -> Result<serde_json::Value> {
    let txid = Txid::from_str(txid_str.trim()).context("invalid txid")?;
    let c = chain.read().map_err(|_| anyhow!("chain lock"))?;
    let locations: Vec<_> = c
        .locations_by_txid(&txid)
        .map_err(|e| anyhow!("lookup: {e}"))?
        .collect();

    for loc in locations {
        let bytes = c.get_tx_bytes(&loc).map_err(|e| anyhow!("get tx: {e}"))?;
        let tx: bitcoin::Transaction =
            deserialize(&bytes).map_err(|e| anyhow!("decode tx: {e}"))?;
        if tx.compute_txid() != txid {
            continue; // false positive, keep looking
        }
        let total_out: u64 = tx.output.iter().map(|o| o.value.to_sat()).sum();
        let outputs: Vec<_> = tx
            .output
            .iter()
            .map(|o| {
                serde_json::json!({
                    "value_sat": o.value.to_sat(),
                    "value_btc": format!("{:.8}", o.value.to_sat() as f64 / 1e8),
                    "address": Address::from_script(&o.script_pubkey, NETWORK)
                        .map(|a| a.to_string())
                        .unwrap_or_else(|_| format!("script:{}", o.script_pubkey.to_hex_string())),
                })
            })
            .collect();
        return Ok(serde_json::json!({
            "ok": true,
            "kind": "tx",
            "txid": txid.to_string(),
            "height": loc.block_height(),
            "block_hash": loc.block_hash().to_string(),
            "version": tx.version.0,
            "lock_time": tx.lock_time.to_consensus_u32(),
            "size": bytes.len(),
            "weight": tx.weight().to_wu(),
            "is_coinbase": tx.is_coinbase(),
            "input_count": tx.input.len(),
            "output_count": tx.output.len(),
            "total_out_sat": total_out,
            "total_out_btc": format!("{:.8}", total_out as f64 / 1e8),
            "outputs": outputs,
        }));
    }
    Ok(serde_json::json!({"ok": false, "error": "transaction not found in index"}))
}

fn status_json(chain: &Chain) -> Result<serde_json::Value> {
    let c = chain.read().map_err(|_| anyhow!("chain lock"))?;
    let headers = c.headers();
    Ok(serde_json::json!({
        "ok": true,
        "tip_hash": headers.tip_hash().to_string(),
        "tip_height": headers.tip_height(),
    }))
}

// ---- helpers -----------------------------------------------------------------

fn err_json(e: anyhow::Error) -> serde_json::Value {
    serde_json::json!({"ok": false, "error": format!("{e:#}")})
}

fn electrum_monitor_json() -> serde_json::Value {
    match fs::read_to_string(ELECTRUM_MONITOR_PATH) {
        Ok(data) => serde_json::from_str(&data)
            .unwrap_or_else(|err| serde_json::json!({"ok": false, "error": err.to_string()})),
        Err(err) => serde_json::json!({
            "ok": false,
            "error": format!("electrum monitor unavailable: {err}"),
        }),
    }
}

fn reset_electrum_monitor() -> Result<serde_json::Value> {
    let addr: SocketAddr = ELECTRUM_CONTROL_ADDR.parse()?;
    let mut stream = TcpStream::connect_timeout(&addr, Duration::from_secs(2))
        .with_context(|| format!("connect electrum control {ELECTRUM_CONTROL_ADDR}"))?;
    stream.set_read_timeout(Some(Duration::from_secs(2)))?;
    stream.set_write_timeout(Some(Duration::from_secs(2)))?;

    let request = serde_json::json!({
        "jsonrpc": "2.0",
        "id": "bindex-web-reset",
        "method": "server.reset_monitor",
        "params": [],
    });
    let mut bytes = serde_json::to_vec(&request)?;
    bytes.push(b'\n');
    stream.write_all(&bytes)?;
    stream.flush()?;

    let mut reader = BufReader::new(stream);
    let mut line = String::new();
    reader.read_line(&mut line)?;
    let response: serde_json::Value = serde_json::from_str(&line)?;
    if let Some(error) = response.get("error") {
        anyhow::bail!("electrum reset failed: {error}");
    }
    Ok(serde_json::json!({
        "ok": true,
        "electrum": response.get("result").cloned().unwrap_or(serde_json::Value::Null),
    }))
}

fn json_ok(json: &serde_json::Value) -> bool {
    json.get("ok")
        .and_then(serde_json::Value::as_bool)
        .unwrap_or(false)
}

fn json_error(json: &serde_json::Value) -> Option<String> {
    json.get("error")
        .and_then(serde_json::Value::as_str)
        .map(ToOwned::to_owned)
}

fn percentile(sorted: &[f64], p: f64) -> f64 {
    if sorted.is_empty() {
        return 0.0;
    }
    let index = ((sorted.len() - 1) as f64 * p).round() as usize;
    sorted[index]
}

fn unix_now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs()
}

fn respond_json(req: Request, json: &serde_json::Value) -> Result<()> {
    respond(req, 200, "application/json", json.to_string().into_bytes())
}

fn respond(req: Request, code: u16, content_type: &str, body: Vec<u8>) -> Result<()> {
    let header = Header::from_bytes(&b"Content-Type"[..], content_type.as_bytes())
        .map_err(|_| anyhow!("bad header"))?;
    req.respond(
        Response::from_data(body)
            .with_status_code(code)
            .with_header(header),
    )?;
    Ok(())
}

/// Minimal percent-decoding for path segments (enough for addresses/txids).
fn pct_decode(s: &str) -> String {
    let bytes = s.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'%' && i + 2 < bytes.len() {
            if let Ok(b) = u8::from_str_radix(&s[i + 1..i + 3], 16) {
                out.push(b);
                i += 3;
                continue;
            }
        }
        out.push(bytes[i]);
        i += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}

// Re-export FromStr for address/txid parsing.
use std::str::FromStr;

const INDEX_HTML: &str = include_str!("index.html");
const MONITOR_HTML: &str = include_str!("monitor.html");

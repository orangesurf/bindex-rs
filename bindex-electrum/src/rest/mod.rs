//! Esplora-compatible REST API (mempool/electrs shapes) served from the bindex
//! index.
//!
//! Enabled with `--http-addr`; it runs on the Electrum server's tokio runtime
//! and reads the same chain and mempool state the Electrum methods read. The
//! route inventory, TTLs and error texts follow `mempool/electrs`, so the
//! responses can be diffed against a real Esplora deployment.
//!
//! The `liquid` build serves the electrs-liquid shapes from the same routes:
//! `format` hides which transaction type is being read, `types` (with
//! `liquid_types`) holds the per-chain response shapes, and `assets` proxies
//! the asset routes, which have no index here.

pub mod address;
#[cfg(feature = "liquid")]
pub mod assets;
pub mod format;
pub mod handlers;
pub mod http;
#[cfg(feature = "liquid")]
pub mod liquid_types;
pub mod query;
pub mod types;

use std::{
    net::SocketAddr,
    sync::{Arc, Mutex, RwLock},
    time::{Duration, Instant},
};

use tokio::sync::Semaphore;

use anyhow::Context as _;
use serde::Serialize;
use serde_json::Value;
use tokio::{
    io::{AsyncBufReadExt as _, BufReader},
    net::TcpListener,
};

use crate::{
    config::RestConfig,
    server::Server,
};

use crate::{
    corerest::{self, CoreRest},
    deadline::Deadline,
};

use self::http::{HttpRequest, HttpResponse, IdentityHeaders};

/// One in-flight request: what the client sent, plus when the server must give
/// up on it. Derefs to the parsed request, so handlers read it as before.
pub struct Req<'a> {
    pub http: &'a HttpRequest,
    pub deadline: Deadline,
}

impl std::ops::Deref for Req<'_> {
    type Target = HttpRequest;

    fn deref(&self) -> &Self::Target {
        self.http
    }
}

/// Static or final data: ~5 years.
pub const TTL_LONG: u32 = 157_784_630;
/// Anything that can still change.
pub const TTL_SHORT: u32 = 10;
/// `/mempool/recent`.
pub const TTL_MEMPOOL_RECENT: u32 = 5;
/// Confirmations after which a block is treated as final.
pub const CONF_FINAL: usize = 10;

/// `TTL_LONG` once the block is buried `CONF_FINAL` deep, else `TTL_SHORT`.
pub fn ttl_by_depth(height: Option<usize>, tip: usize) -> u32 {
    match height {
        Some(height) if tip.saturating_sub(height) >= CONF_FINAL => TTL_LONG,
        _ => TTL_SHORT,
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HttpError {
    pub status: u16,
    pub message: String,
    /// The reference emits `Cache-Control: public, max-age=0` on the errors it
    /// routes through its `http_message` helper, and no `Cache-Control` at all
    /// on the plain error fallthrough.
    pub cache_max_age: Option<u32>,
}

impl HttpError {
    pub fn new(status: u16, message: impl Into<String>) -> Self {
        Self {
            status,
            message: message.into(),
            cache_max_age: None,
        }
    }

    pub fn bad_request(message: impl Into<String>) -> Self {
        Self::new(400, message)
    }

    pub fn not_found(message: impl Into<String>) -> Self {
        Self::new(404, message)
    }

    /// 422, used for `after_txid not found` and `body too long`.
    pub fn unprocessable(message: impl Into<String>) -> Self {
        Self::new(422, message)
    }

    pub fn server_error(message: impl Into<String>) -> Self {
        Self::new(500, message)
    }

    pub fn cached(mut self, max_age: u32) -> Self {
        self.cache_max_age = Some(max_age);
        self
    }

    fn into_response(self) -> HttpResponse {
        HttpResponse::text(self.status, self.message, self.cache_max_age)
    }
}

impl From<crate::chain::Error> for HttpError {
    fn from(err: crate::chain::Error) -> Self {
        match err {
            // the query ran out of time: 504, as a gateway that gave up
            crate::chain::Error::Deadline => HttpError::new(504, err.to_string()),
            // the chain moved under it repeatedly: transient, ask again
            crate::chain::Error::Reorg => HttpError::new(503, err.to_string()),
            other => HttpError::server_error(other.to_string()),
        }
    }
}

impl From<crate::protocol::Error> for HttpError {
    fn from(err: crate::protocol::Error) -> Self {
        HttpError::bad_request(err.to_string())
    }
}

impl From<corerest::Error> for HttpError {
    fn from(err: corerest::Error) -> Self {
        match err {
            corerest::Error::NotFound => HttpError::not_found("Block not found"),
            other => HttpError::server_error(other.to_string()),
        }
    }
}

pub type Result<T> = std::result::Result<T, HttpError>;

/// JSON body with a cache TTL.
pub fn json<T: Serialize>(value: &T, ttl: u32) -> Result<HttpResponse> {
    let body = serde_json::to_vec(value)
        .map_err(|err| HttpError::server_error(format!("serialize response: {err}")))?;
    Ok(HttpResponse::json(body, Some(ttl)))
}

pub fn text(value: impl Into<String>, ttl: u32) -> HttpResponse {
    HttpResponse::text(200, value, Some(ttl))
}

/// Shared REST state: the Electrum server (chain, mempool, bitcoind RPC and the
/// broadcast policy) plus the REST-only bits.
pub struct RestApi {
    pub server: Server,
    pub core: CoreRest,
    /// What the response shapes need to know about the chain; on Bitcoin just
    /// the `Network`.
    pub network: format::Params,
    pub config: RestConfig,
    identity: RwLock<IdentityHeaders>,
    fee_cache: Mutex<Option<(Instant, Value)>>,
    /// Bounds how many history folds, spender scans and whole-block
    /// transaction builds run at once, so they cannot crowd out the cheap
    /// routes (or the node) when several land together.
    queries: Semaphore,
    /// Bounds open connections, so a client that opens sockets and never
    /// finishes a request cannot exhaust the server.
    connections: Arc<Semaphore>,
}

impl RestApi {
    pub fn new(server: Server) -> anyhow::Result<Arc<Self>> {
        let (network, rest, rest_url) = {
            let config = server.config();
            (
                config.network,
                config.rest.clone(),
                config.bitcoind_rest_url.clone(),
            )
        };
        let identity = IdentityHeaders {
            powered_by: format!("bindex-electrum/{}", env!("CARGO_PKG_VERSION")),
            cors: rest.cors,
            bitcoin_version: None,
        };
        Ok(Arc::new(Self {
            network: format::params(network)?,
            queries: Semaphore::new(rest.rest_max_concurrent_queries.max(1)),
            connections: Arc::new(Semaphore::new(rest.rest_max_connections.max(1))),
            config: rest,
            core: rest_core(&server, rest_url),
            identity: RwLock::new(identity),
            fee_cache: Mutex::new(None),
            server,
        }))
    }

    fn identity(&self) -> IdentityHeaders {
        self.identity.read().expect("identity lock").clone()
    }

    pub(crate) fn fee_estimates_cached(&self) -> Option<Value> {
        let guard = self.fee_cache.lock().expect("fee cache lock");
        guard.as_ref().and_then(|(at, value)| {
            (at.elapsed() < Duration::from_secs(TTL_SHORT as u64)).then(|| value.clone())
        })
    }

    pub(crate) fn store_fee_estimates(&self, value: Value) {
        *self.fee_cache.lock().expect("fee cache lock") = Some((Instant::now(), value));
    }

    /// Ask bitcoind for its subversion once, for the `X-Bitcoin-Version` header.
    /// Optional metadata: serving never waits for it.
    fn spawn_version_probe(self: &Arc<Self>) {
        let api = Arc::clone(self);
        tokio::spawn(async move {
            loop {
                match api.server.bitcoind().call("getnetworkinfo", serde_json::json!([])).await {
                    Ok(info) => {
                        if let Some(subversion) = info.get("subversion").and_then(Value::as_str) {
                            api.identity.write().expect("identity lock").bitcoin_version =
                                Some(subversion.to_string());
                            return;
                        }
                        log::debug!("getnetworkinfo carried no subversion");
                    }
                    Err(err) => log::debug!("bitcoind version probe failed: {err}"),
                }
                tokio::time::sleep(Duration::from_secs(60)).await;
            }
        });
    }

    async fn respond(&self, request: &HttpRequest) -> HttpResponse {
        let deadline = Deadline::after(self.config.request_timeout());
        let expensive = handlers::is_expensive(&request.method, &request.segments());

        // queue the expensive routes; a cheap one must stay answerable while a
        // hot address is being replayed
        let _permit = if expensive {
            let wait = deadline.remaining().unwrap_or(Duration::from_secs(30));
            match tokio::time::timeout(wait, self.queries.acquire()).await {
                Ok(Ok(permit)) => Some(permit),
                Ok(Err(_)) => return HttpError::new(503, "server shutting down").into_response(),
                Err(_) => {
                    return HttpError::new(503, "too many concurrent queries").into_response()
                }
            }
        } else {
            None
        };

        let request = &Req {
            http: request,
            deadline,
        };
        match handlers::route(self, request).await {
            Ok(response) => response,
            Err(err) => {
                if err.status >= 500 {
                    log::warn!(
                        "{} {} -> {}: {}",
                        request.method,
                        request.path,
                        err.status,
                        err.message
                    );
                } else {
                    log::debug!(
                        "{} {} -> {}: {}",
                        request.method,
                        request.path,
                        err.status,
                        err.message
                    );
                }
                err.into_response()
            }
        }
    }
}

/// The node reads the REST API makes beyond what the index stores.
pub(crate) fn rest_core(server: &Server, rest_url: String) -> CoreRest {
    let core = CoreRest::new(rest_url);
    // Elements serves these over RPC; its REST facade only has the indexer's
    // endpoints
    #[cfg(feature = "liquid")]
    let core = core.with_rpc(server.bitcoind().clone());
    #[cfg(not(feature = "liquid"))]
    let _ = server;
    core
}

/// Bind and serve until the task is dropped.
pub async fn serve(server: Server, addr: SocketAddr) -> anyhow::Result<()> {
    let listener = TcpListener::bind(addr)
        .await
        .with_context(|| format!("bind REST {addr}"))?;
    log::info!("esplora REST listening on {addr}");
    let api = RestApi::new(server)?;
    api.spawn_version_probe();
    run_listener(api, listener).await
}

pub async fn run_listener(api: Arc<RestApi>, listener: TcpListener) -> anyhow::Result<()> {
    loop {
        let (stream, peer) = listener.accept().await?;
        let api = Arc::clone(&api);
        let Ok(permit) = Arc::clone(&api.connections).try_acquire_owned() else {
            log::debug!("refusing REST connection from {peer}: connection limit reached");
            tokio::spawn(async move { refuse(api, stream).await });
            continue;
        };
        tokio::spawn(async move {
            let _permit = permit;
            if let Err(err) = handle_connection(api, stream).await {
                log::debug!("REST connection from {peer} ended: {err}");
            }
        });
    }
}

/// Answer a connection we have no capacity for, rather than dropping it.
async fn refuse(api: Arc<RestApi>, stream: tokio::net::TcpStream) {
    let (_, mut writer) = stream.into_split();
    let response = HttpError::new(503, "too many connections").into_response();
    let _ = tokio::time::timeout(
        Duration::from_secs(5),
        http::write_response(&mut writer, &response, &api.identity(), false),
    )
    .await;
}

async fn handle_connection(api: Arc<RestApi>, stream: tokio::net::TcpStream) -> std::io::Result<()> {
    let (reader, mut writer) = stream.into_split();
    let mut reader = BufReader::new(reader);
    let header_timeout = api.config.header_timeout();
    let idle_timeout = api.config.idle_timeout();
    let mut first = true;

    loop {
        // A connection that has said nothing yet gets the header timeout; one
        // waiting for its next pipelined request gets the idle timeout. Either
        // way a half-sent request cannot hold the slot open indefinitely.
        let wait = if first { header_timeout } else { idle_timeout };
        match tokio::time::timeout(wait, reader.fill_buf()).await {
            Err(_) => return Ok(()),
            // the peer closed without starting a request
            Ok(Ok([])) => return Ok(()),
            Ok(Ok(_)) => {}
            Ok(Err(err)) => return Err(err),
        }
        first = false;

        let outcome = match tokio::time::timeout(
            header_timeout,
            http::read_request(&mut reader, api.config.request_body_bytes_cap),
        )
        .await
        {
            Ok(outcome) => outcome?,
            Err(_) => {
                let response = HttpError::new(408, "request timed out").into_response();
                http::write_response(&mut writer, &response, &api.identity(), false).await?;
                return Ok(());
            }
        };

        let request = match outcome {
            http::ReadOutcome::Closed => return Ok(()),
            http::ReadOutcome::Invalid { status, message } => {
                let response = HttpError::new(status, message).into_response();
                http::write_response(&mut writer, &response, &api.identity(), false).await?;
                return Ok(());
            }
            http::ReadOutcome::Request(request) => request,
        };

        let keep_alive = request.keep_alive;
        let response = api.respond(&request).await;
        http::write_response(&mut writer, &response, &api.identity(), keep_alive).await?;
        if !keep_alive {
            return Ok(());
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ttl_switches_at_ten_confirmations() {
        assert_eq!(ttl_by_depth(Some(100), 109), TTL_SHORT);
        assert_eq!(ttl_by_depth(Some(100), 110), TTL_LONG);
        assert_eq!(ttl_by_depth(None, 110), TTL_SHORT);
    }

    #[test]
    fn errors_keep_their_cache_policy() {
        let plain = HttpError::not_found("Transaction not found");
        assert_eq!(plain.cache_max_age, None);
        let missing = HttpError::server_error("Transaction missing prevouts").cached(0);
        assert_eq!(missing.cache_max_age, Some(0));
        assert_eq!(missing.status, 500);
    }
}

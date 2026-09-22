//! Esplora-compatible REST API (mempool/electrs shapes) served from the bindex
//! index.
//!
//! Enabled with `--http-addr`; it runs on the Electrum server's tokio runtime
//! and reads the same chain and mempool state the Electrum methods read. The
//! route inventory, TTLs and error texts follow `mempool/electrs`, so the
//! responses can be diffed against a real Esplora deployment.
//!
//! Bitcoin only: the whole module is compiled out of the `liquid` build, whose
//! transactions are Elements-encoded and need a different set of shapes.
#![cfg(not(feature = "liquid"))]

pub mod handlers;
pub mod http;
pub mod query;
pub mod types;

use std::{
    net::SocketAddr,
    sync::{Arc, Mutex, RwLock},
    time::{Duration, Instant},
};

use anyhow::Context as _;
use bitcoin::Network;
use serde::Serialize;
use serde_json::Value;
use tokio::{io::BufReader, net::TcpListener};

use crate::{
    config::RestConfig,
    server::Server,
};

use crate::corerest::{self, CoreRest};

use self::http::{HttpRequest, HttpResponse, IdentityHeaders};

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
        HttpError::server_error(err.to_string())
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
    pub network: Network,
    pub config: RestConfig,
    identity: RwLock<IdentityHeaders>,
    fee_cache: Mutex<Option<(Instant, Value)>>,
}

impl RestApi {
    pub fn new(server: Server) -> Arc<Self> {
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
        Arc::new(Self {
            network,
            config: rest,
            core: CoreRest::new(rest_url),
            identity: RwLock::new(identity),
            fee_cache: Mutex::new(None),
            server,
        })
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
        match handlers::route(self, request).await {
            Ok(response) => response,
            Err(err) => {
                if err.status >= 500 {
                    log::warn!("{} {} -> {}: {}", request.method, request.path, err.status, err.message);
                } else {
                    log::debug!("{} {} -> {}: {}", request.method, request.path, err.status, err.message);
                }
                err.into_response()
            }
        }
    }
}

/// Bind and serve until the task is dropped.
pub async fn serve(server: Server, addr: SocketAddr) -> anyhow::Result<()> {
    let listener = TcpListener::bind(addr)
        .await
        .with_context(|| format!("bind REST {addr}"))?;
    log::info!("esplora REST listening on {addr}");
    let api = RestApi::new(server);
    api.spawn_version_probe();
    run_listener(api, listener).await
}

pub async fn run_listener(api: Arc<RestApi>, listener: TcpListener) -> anyhow::Result<()> {
    loop {
        let (stream, peer) = listener.accept().await?;
        let api = Arc::clone(&api);
        tokio::spawn(async move {
            if let Err(err) = handle_connection(api, stream).await {
                log::debug!("REST connection from {peer} ended: {err}");
            }
        });
    }
}

async fn handle_connection(api: Arc<RestApi>, stream: tokio::net::TcpStream) -> std::io::Result<()> {
    let (reader, mut writer) = stream.into_split();
    let mut reader = BufReader::new(reader);
    loop {
        let Some(request) = http::read_request(&mut reader).await? else {
            return Ok(());
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

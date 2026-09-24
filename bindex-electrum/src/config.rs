use std::{net::SocketAddr, path::PathBuf, time::Duration};

use bitcoin::Network;
use clap::Parser;

use crate::{protocol::ProtocolVersion, torpush};

#[derive(Debug, Clone, Copy, PartialEq, Eq, clap::ValueEnum)]
pub enum BroadcastVia {
    /// POST the raw tx through the tor SOCKS5 proxy to an onion push endpoint,
    /// on a fresh circuit per push (unique SOCKS credentials + IsolateSOCKSAuth)
    Tor,
    /// sendrawtransaction on the bitcoind RPC
    Bitcoind,
}

#[derive(Debug, Clone, Parser)]
#[command(name = "bindex-electrum")]
#[command(about = "Electrum protocol server backed by bindex")]
pub struct Config {
    #[arg(long, default_value = "bitcoin")]
    pub network: Network,

    /// Name of the index directory under --bindex-db-path (default: the network
    /// name on Bitcoin, "liquid" when built with the liquid feature)
    #[arg(long)]
    pub db_name: Option<String>,

    #[arg(long)]
    pub bindex_db_path: PathBuf,

    /// RocksDB secondary directory (default: `<bindex-db-path>/<db-name>-electrum-secondary`).
    /// Each process needs its own, so a second instance against the same index
    /// must override it.
    #[arg(long)]
    pub secondary_path: Option<PathBuf>,

    #[arg(long, default_value = "http://127.0.0.1:8332")]
    pub bitcoind_rest_url: String,

    #[arg(long, default_value = "http://127.0.0.1:8332")]
    pub bitcoind_rpc_url: String,

    #[arg(long)]
    pub bitcoind_rpc_user: Option<String>,

    #[arg(long)]
    pub bitcoind_rpc_password: Option<String>,

    #[arg(long)]
    pub bitcoind_rpc_cookie: Option<PathBuf>,

    #[arg(long)]
    pub bitcoind_rpc_conf: Option<PathBuf>,

    #[arg(long, default_value = "127.0.0.1:50001")]
    pub tcp_listen: SocketAddr,

    #[arg(long)]
    pub tls_listen: Option<SocketAddr>,

    #[arg(long)]
    pub tls_cert: Option<PathBuf>,

    #[arg(long)]
    pub tls_key: Option<PathBuf>,

    #[arg(long)]
    pub advertised_host: Vec<String>,

    /// How many index references the script-history cache may hold, summed
    /// over the cached scripts. Each costs a few hundred bytes; a script with
    /// more than this many transactions is never cached.
    #[arg(long, default_value_t = 200_000)]
    pub script_cache_refs: usize,

    #[arg(long)]
    pub monitor_path: Option<PathBuf>,

    #[arg(long, default_value = "1.4")]
    pub protocol_min: ProtocolVersion,

    #[arg(long, default_value = "1.6")]
    pub protocol_max: ProtocolVersion,

    #[arg(long, default_value_t = 5)]
    pub mempool_poll_secs: u64,

    #[arg(long, default_value_t = 30_000)]
    pub secondary_refresh_ms: u64,

    /// The node's `zmqpubrawtx` endpoint (e.g. `tcp://127.0.0.1:28333`). Each
    /// announced transaction wakes the mempool poll early, at most once a
    /// second.
    #[arg(long)]
    pub zmq_rawtx: Option<String>,

    /// The node's `zmqpubrawblock` endpoint (e.g. `tcp://127.0.0.1:28332`). Each
    /// announced block wakes the index refresh (retried for ten seconds while
    /// the writer catches up) and the mempool poll early.
    #[arg(long)]
    pub zmq_rawblock: Option<String>,

    #[arg(long, default_value_t = 100)]
    pub max_batch_size: usize,

    #[arg(long, default_value_t = 1000)]
    pub max_subscriptions_per_session: usize,

    /// How blockchain.transaction.broadcast submits transactions (tor on
    /// Bitcoin; the node's sendrawtransaction on Liquid, where mempool.space's
    /// onion push endpoint would be the wrong chain)
    #[arg(long, value_enum, default_value_t = default_broadcast_via())]
    pub broadcast_via: BroadcastVia,

    /// SOCKS5 address of the local tor daemon
    #[arg(long, default_value = "127.0.0.1:9050")]
    pub tor_proxy: SocketAddr,

    /// Push endpoint URL; defaults to mempool.space's onion endpoint for --network
    #[arg(long)]
    pub tor_broadcast_url: Option<String>,

    /// Package push endpoint URL (blockchain.transaction.broadcast_package);
    /// defaults to the /api/v1/txs/package endpoint beside --tor-broadcast-url
    #[arg(long)]
    pub tor_package_url: Option<String>,

    #[arg(long, default_value = "bindex electrum")]
    pub banner: String,

    #[arg(long)]
    pub peer: Vec<String>,

    #[arg(long)]
    pub donation_address: Option<String>,

    #[command(flatten)]
    pub rest: RestConfig,
}

// Esplora-compatible REST API (off unless `--http-addr` is given). The defaults
// mirror the mempool/electrs REST server this API is modelled on.
//
// Deliberately not a doc comment: clap would promote it to the binary's `about`
// text. The flags get their own `--help` section instead.
#[derive(Debug, Clone, PartialEq, Eq, clap::Args)]
#[command(next_help_heading = "REST API")]
pub struct RestConfig {
    /// Serve the Esplora-compatible REST API on this address.
    #[arg(long)]
    pub http_addr: Option<SocketAddr>,

    /// Send `Access-Control-Allow-Origin: *` (no other CORS headers, no preflight).
    #[arg(long)]
    pub cors: bool,

    /// Answer `GET /address-prefix/:prefix` with an empty list instead of 400
    /// (the index holds no addresses to search).
    #[arg(long)]
    pub address_search: bool,

    /// Blocks returned by `GET /blocks[/:start_height]`.
    #[arg(long, default_value_t = 10)]
    pub rest_default_block_limit: usize,

    /// Confirmed transactions per page of `GET /address/:addr/txs/chain`.
    #[arg(long, default_value_t = 25)]
    pub rest_default_chain_txs_per_page: usize,

    /// Default (and cap) for `?max_txs` on the combined/mempool address routes.
    #[arg(long, default_value_t = 50)]
    pub rest_default_max_mempool_txs: usize,

    /// Default and hard cap for `GET /address/:addr/txs/summary`.
    #[arg(long, default_value_t = 5000)]
    pub rest_default_max_address_summary_txs: usize,

    /// Default and cap for `GET /internal/mempool/txs`.
    #[arg(long, default_value_t = 1000)]
    pub rest_max_mempool_page_size: usize,

    /// Default and cap for `GET /mempool/txids/page`.
    #[arg(long, default_value_t = 10000)]
    pub rest_max_mempool_txid_page_size: usize,

    /// Peak historical live-set cap for `GET /address/:addr/utxo`.
    #[arg(long, default_value_t = 500)]
    pub utxos_limit: usize,

    /// Entries kept for `GET /mempool/recent`.
    #[arg(long, default_value_t = 10)]
    pub mempool_recent_txs_size: usize,

    /// Hardcoded in the reference; kept configurable for tests.
    #[arg(long, default_value_t = 100)]
    pub rest_max_history_txs: usize,

    /// Give up on a REST request after this many seconds (0 disables).
    #[arg(long, default_value_t = 30)]
    pub rest_request_timeout_secs: u64,

    /// How many expensive REST queries (history folds, spender scans,
    /// whole-block transaction lists) may run at once.
    #[arg(long, default_value_t = 4)]
    pub rest_max_concurrent_queries: usize,

    /// Maximum simultaneously open REST connections.
    #[arg(long, default_value_t = 100)]
    pub rest_max_connections: usize,

    /// Time a client gets to send a complete request head and body.
    #[arg(long, default_value_t = 10)]
    pub rest_header_timeout_secs: u64,

    /// Time a kept-alive REST connection may sit idle between requests.
    #[arg(long, default_value_t = 30)]
    pub rest_idle_timeout_secs: u64,

    /// Largest REST request body accepted.
    #[arg(long, default_value_t = 20_000_200)]
    pub request_body_bytes_cap: usize,

    /// Esplora API that `/asset*` and `/assets*` are proxied to (e.g.
    /// `https://liquid.network/api`): there is no asset index here. Unset,
    /// those routes answer 404.
    #[cfg(feature = "liquid")]
    #[arg(long)]
    pub asset_upstream: Option<String>,
}

impl Default for RestConfig {
    fn default() -> Self {
        Self {
            http_addr: None,
            cors: false,
            address_search: false,
            rest_default_block_limit: 10,
            rest_default_chain_txs_per_page: 25,
            rest_default_max_mempool_txs: 50,
            rest_default_max_address_summary_txs: 5000,
            rest_max_mempool_page_size: 1000,
            rest_max_mempool_txid_page_size: 10000,
            utxos_limit: 500,
            mempool_recent_txs_size: 10,
            rest_max_history_txs: 100,
            rest_request_timeout_secs: 30,
            rest_max_concurrent_queries: 4,
            rest_max_connections: 100,
            rest_header_timeout_secs: 10,
            rest_idle_timeout_secs: 30,
            request_body_bytes_cap: 20_000_200,
            #[cfg(feature = "liquid")]
            asset_upstream: None,
        }
    }
}

impl RestConfig {
    /// `capped_max_txs`: the `?max_txs` query parameter, defaulted then capped.
    pub fn capped_max_txs(&self, requested: Option<usize>, default: usize, cap: usize) -> usize {
        requested.unwrap_or(default).min(cap)
    }

    pub fn request_timeout(&self) -> Duration {
        Duration::from_secs(self.rest_request_timeout_secs)
    }

    pub fn header_timeout(&self) -> Duration {
        Duration::from_secs(self.rest_header_timeout_secs)
    }

    pub fn idle_timeout(&self) -> Duration {
        Duration::from_secs(self.rest_idle_timeout_secs)
    }
}

fn default_broadcast_via() -> BroadcastVia {
    if bindex::fmt::NAME == "bitcoin" {
        BroadcastVia::Tor
    } else {
        BroadcastVia::Bitcoind
    }
}

impl Config {
    pub fn db_name(&self) -> String {
        self.db_name.clone().unwrap_or_else(|| {
            if bindex::fmt::NAME == "bitcoin" {
                self.network.to_string()
            } else {
                bindex::fmt::NAME.to_string()
            }
        })
    }

    pub fn validate(&self) -> anyhow::Result<()> {
        if self.protocol_min > self.protocol_max {
            anyhow::bail!("protocol-min must be <= protocol-max");
        }
        if self.tls_listen.is_some() && (self.tls_cert.is_none() || self.tls_key.is_none()) {
            anyhow::bail!("tls-listen requires tls-cert and tls-key");
        }
        if self.bitcoind_rpc_user.is_some() ^ self.bitcoind_rpc_password.is_some() {
            anyhow::bail!("bitcoind-rpc-user and bitcoind-rpc-password must be provided together");
        }
        if self.bitcoind_rpc_cookie.is_some() && self.bitcoind_rpc_conf.is_some() {
            anyhow::bail!("only one of bitcoind-rpc-cookie or bitcoind-rpc-conf can be provided");
        }
        if self.max_batch_size == 0 {
            anyhow::bail!("max-batch-size must be positive");
        }
        if self.max_subscriptions_per_session == 0 {
            anyhow::bail!("max-subscriptions-per-session must be positive");
        }
        if self.secondary_refresh_ms == 0 {
            anyhow::bail!("secondary-refresh-ms must be positive");
        }
        for (flag, endpoint) in [
            ("zmq-rawtx", &self.zmq_rawtx),
            ("zmq-rawblock", &self.zmq_rawblock),
        ] {
            if let Some(endpoint) = endpoint {
                if !endpoint.starts_with("tcp://") {
                    anyhow::bail!("{flag} must be a tcp:// endpoint, got {endpoint}");
                }
            }
        }
        if self.rest.http_addr.is_some() {
            // the REST shapes need to know the chain the index is of
            crate::rest::format::params(self.network)?;
        }
        if self.broadcast_via == BroadcastVia::Tor {
            self.tor_broadcast_target()?;
            self.tor_package_target()?;
        }
        Ok(())
    }

    /// mempool.space's onion serves each network under a path prefix.
    fn onion_network_prefix(&self) -> anyhow::Result<&'static str> {
        match self.network {
            Network::Bitcoin => Ok(""),
            Network::Testnet => Ok("/testnet"),
            Network::Testnet4 => Ok("/testnet4"),
            Network::Signet => Ok("/signet"),
            network => anyhow::bail!(
                "no default onion push endpoint for {network}; \
                 pass --tor-broadcast-url or --broadcast-via bitcoind"
            ),
        }
    }

    pub fn tor_broadcast_target(&self) -> anyhow::Result<torpush::PushTarget> {
        let url = match &self.tor_broadcast_url {
            Some(url) => url.clone(),
            None => format!(
                "http://{}{}/api/tx",
                torpush::MEMPOOL_SPACE_ONION,
                self.onion_network_prefix()?
            ),
        };
        Ok(torpush::PushTarget::parse(&url)?)
    }

    /// Where `broadcast_package` pushes. With tor, packages must never fall
    /// back to local bitcoind, so this is resolved (and validated) up front:
    /// explicit `--tor-package-url`, else the package endpoint next to a
    /// mempool-style `/api/tx` broadcast URL, else mempool.space's onion.
    pub fn tor_package_target(&self) -> anyhow::Result<torpush::PushTarget> {
        let url = match (&self.tor_package_url, &self.tor_broadcast_url) {
            (Some(url), _) => url.clone(),
            (None, None) => format!(
                "http://{}{}/api/v1/txs/package",
                torpush::MEMPOOL_SPACE_ONION,
                self.onion_network_prefix()?
            ),
            (None, Some(tx_url)) => match tx_url.strip_suffix("/api/tx") {
                Some(base) => format!("{base}/api/v1/txs/package"),
                None => anyhow::bail!(
                    "--tor-broadcast-url {tx_url:?} is not a mempool-style /api/tx endpoint, \
                     so no package endpoint can be derived; pass --tor-package-url"
                ),
            },
        };
        Ok(torpush::PushTarget::parse(&url)?)
    }

    pub fn mempool_poll_interval(&self) -> Duration {
        Duration::from_secs(self.mempool_poll_secs)
    }

    pub fn secondary_refresh_interval(&self) -> Duration {
        Duration::from_millis(self.secondary_refresh_ms)
    }

    pub fn monitor_path(&self) -> PathBuf {
        self.monitor_path
            .clone()
            .unwrap_or_else(|| self.bindex_db_path.join("electrum-monitor.json"))
    }

    pub fn secondary_path(&self) -> PathBuf {
        self.secondary_path.clone().unwrap_or_else(|| {
            self.bindex_db_path
                .join(format!("{}-electrum-secondary", self.db_name()))
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn config(args: &[&str]) -> Config {
        let mut argv = vec!["bindex-electrum", "--bindex-db-path", "/nonexistent"];
        argv.extend_from_slice(args);
        Config::try_parse_from(argv).expect("args parse")
    }

    #[test]
    fn rest_defaults_match_the_clap_defaults() {
        assert_eq!(config(&[]).rest, RestConfig::default());
    }

    #[test]
    fn package_endpoint_sits_beside_the_tx_endpoint() {
        let c = config(&[]);
        assert_eq!(
            c.tor_broadcast_target().unwrap().url(),
            format!("http://{}:80/api/tx", torpush::MEMPOOL_SPACE_ONION)
        );
        assert_eq!(
            c.tor_package_target().unwrap().url(),
            format!("http://{}:80/api/v1/txs/package", torpush::MEMPOOL_SPACE_ONION)
        );

        let c = config(&["--network", "signet"]);
        assert_eq!(
            c.tor_package_target().unwrap().url(),
            format!("http://{}:80/signet/api/v1/txs/package", torpush::MEMPOOL_SPACE_ONION)
        );

        let c = config(&["--tor-broadcast-url", "http://push.example.onion:8080/api/tx"]);
        assert_eq!(
            c.tor_package_target().unwrap().url(),
            "http://push.example.onion:8080/api/v1/txs/package"
        );

        let c = config(&["--tor-package-url", "http://pkg.example.onion/submit"]);
        assert_eq!(c.tor_package_target().unwrap().url(), "http://pkg.example.onion:80/submit");
    }

    #[test]
    fn custom_tx_endpoint_without_package_endpoint_is_rejected_up_front() {
        let c = config(&["--broadcast-via", "tor", "--tor-broadcast-url", "http://push.example.onion/push"]);
        let err = c.validate().unwrap_err().to_string();
        assert!(err.contains("--tor-package-url"), "{err}");
        assert!(config(&["--broadcast-via", "tor", "--tor-broadcast-url", "http://push.example.onion/push",
                         "--tor-package-url", "http://push.example.onion/pkg"]).validate().is_ok());
        assert!(config(&["--tor-broadcast-url", "http://push.example.onion/push",
                         "--broadcast-via", "bitcoind"]).validate().is_ok());
    }
}

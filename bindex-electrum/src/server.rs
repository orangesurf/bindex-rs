use std::{
    future::Future,
    net::SocketAddr,
    str::FromStr,
    sync::{Arc, RwLock},
    time::Instant,
};

use anyhow::Context as _;
use bitcoin::{consensus::deserialize, Transaction, Txid};
use serde_json::{json, Value};
use tokio::{
    io::{AsyncBufReadExt, AsyncRead, AsyncWrite, AsyncWriteExt, BufReader},
    net::TcpListener,
    time::{self, MissedTickBehavior},
};
use tokio_rustls::TlsAcceptor;

use crate::{
    bitcoind::RpcClient,
    chain::ChainAdapter,
    config::{BroadcastVia, Config},
    mempool::MempoolIndex,
    merkle,
    monitor::Monitor,
    protocol::{
        optional_protocol_param, params_array, parse_line, scripthash_status, serialize_response,
        serialize_responses, server_features, string_param, ElectrumScripthash,
        Error as ProtocolError, Frame, Params, ProtocolVersion, Request, Response,
    },
    session::Session,
    tls,
    torpush::{self, PushTarget},
};

#[derive(Clone)]
pub struct Server {
    state: Arc<State>,
}

struct State {
    config: Config,
    chain: ChainAdapter,
    mempool: RwLock<MempoolIndex>,
    bitcoind: RpcClient,
    tor_push: Option<PushTarget>,
    monitor: Monitor,
}

impl Server {
    pub fn new(config: Config) -> anyhow::Result<Self> {
        let chain = ChainAdapter::open(&config).context("open bindex chain")?;
        let monitor = Monitor::new(config.monitor_path()).context("open electrum monitor")?;
        let bitcoind = RpcClient::new(
            config.bitcoind_rpc_url.clone(),
            config.bitcoind_rpc_user.clone(),
            config.bitcoind_rpc_password.clone(),
            config.bitcoind_rpc_cookie.clone(),
            config.bitcoind_rpc_conf.clone(),
        );
        let tor_push = match config.broadcast_via {
            BroadcastVia::Tor => {
                let target = config
                    .tor_broadcast_target()
                    .context("resolve tor broadcast target")?;
                log::info!(
                    "broadcasting via tor proxy {} to {}",
                    config.tor_proxy,
                    target.url()
                );
                Some(target)
            }
            BroadcastVia::Bitcoind => None,
        };
        Ok(Self {
            state: Arc::new(State {
                config,
                chain,
                mempool: RwLock::new(MempoolIndex::default()),
                bitcoind,
                tor_push,
                monitor,
            }),
        })
    }

    pub async fn run(self) -> anyhow::Result<()> {
        self.spawn_secondary_refresh_task();

        let tcp = TcpListener::bind(self.state.config.tcp_listen)
            .await
            .with_context(|| format!("bind {}", self.state.config.tcp_listen))?;
        log::info!("electrum TCP listening on {}", self.state.config.tcp_listen);

        if let Some(addr) = self.state.config.tls_listen {
            let cert = self.state.config.tls_cert.as_ref().unwrap();
            let key = self.state.config.tls_key.as_ref().unwrap();
            let acceptor = TlsAcceptor::from(tls::load_server_config(cert, key)?);
            let server = self.clone();
            tokio::spawn(async move {
                if let Err(err) = server.run_tls(addr, acceptor).await {
                    log::error!("TLS listener stopped: {err:?}");
                }
            });
        }

        self.run_tcp_listener_until_shutdown(tcp, tokio::signal::ctrl_c())
            .await
    }

    fn spawn_secondary_refresh_task(&self) {
        let chain = self.state.chain.clone();
        let interval = chain.refresh_interval();
        tokio::spawn(async move {
            time::sleep(interval).await;
            let mut ticker = time::interval(interval);
            ticker.set_missed_tick_behavior(MissedTickBehavior::Skip);
            loop {
                ticker.tick().await;
                let chain = chain.clone();
                match tokio::task::spawn_blocking(move || chain.refresh()).await {
                    Ok(Ok(elapsed)) => {
                        log::debug!("refreshed electrum secondary in {:.1?}", elapsed);
                    }
                    Ok(Err(err)) => log::warn!("electrum secondary refresh failed: {err}"),
                    Err(err) => log::warn!("electrum secondary refresh task failed: {err}"),
                }
            }
        });
    }

    pub async fn run_tcp_listener_until_shutdown<F>(
        self,
        tcp: TcpListener,
        shutdown: F,
    ) -> anyhow::Result<()>
    where
        F: Future<Output = std::io::Result<()>>,
    {
        tokio::pin!(shutdown);
        loop {
            tokio::select! {
                accepted = tcp.accept() => {
                    let (stream, peer) = accepted?;
                    let server = self.clone();
                    tokio::spawn(async move {
                        if let Err(err) = server.handle_stream(stream, peer).await {
                            log::debug!("session {peer} ended: {err:?}");
                        }
                    });
                }
                signal = &mut shutdown => {
                    signal?;
                    log::info!("shutdown signal received");
                    break;
                }
            }
        }
        Ok(())
    }

    async fn run_tls(self, addr: SocketAddr, acceptor: TlsAcceptor) -> anyhow::Result<()> {
        let listener = TcpListener::bind(addr)
            .await
            .with_context(|| format!("bind TLS {addr}"))?;
        log::info!("electrum TLS listening on {addr}");
        loop {
            let (stream, peer) = listener.accept().await?;
            let acceptor = acceptor.clone();
            let server = self.clone();
            tokio::spawn(async move {
                match acceptor.accept(stream).await {
                    Ok(stream) => {
                        if let Err(err) = server.handle_stream(stream, peer).await {
                            log::debug!("TLS session {peer} ended: {err:?}");
                        }
                    }
                    Err(err) => log::debug!("TLS handshake from {peer} failed: {err}"),
                }
            });
        }
    }

    async fn handle_stream<S>(&self, stream: S, peer: SocketAddr) -> anyhow::Result<()>
    where
        S: AsyncRead + AsyncWrite + Unpin,
    {
        let mut session = Session::new(self.state.config.protocol_max.clone());
        let (reader, mut writer) = tokio::io::split(stream);
        let mut lines = BufReader::new(reader).lines();
        let peer = peer.to_string();
        let _session_guard = self.state.monitor.session(peer.clone());

        while let Some(line) = lines.next_line().await? {
            if line.trim().is_empty() {
                continue;
            }
            let response = match parse_line(line.as_bytes()) {
                Ok(Frame::Single(request)) => {
                    if request.id.is_none() {
                        self.handle_tracked_request(&mut session, request, &peer)
                            .await?;
                        None
                    } else {
                        let response = self
                            .handle_tracked_request(&mut session, request, &peer)
                            .await?;
                        Some(serialize_response(&response)?)
                    }
                }
                Ok(Frame::Batch(requests)) => {
                    if requests.len() > self.state.config.max_batch_size {
                        self.state.monitor.record_request(
                            &peer,
                            "batch",
                            false,
                            Instant::now().elapsed(),
                            Some("batch too large".to_string()),
                        );
                        Some(serialize_response(&Response::error(
                            None,
                            -32600,
                            "batch too large",
                        ))?)
                    } else {
                        let mut responses = Vec::new();
                        for request in requests {
                            if request.id.is_some() {
                                responses.push(
                                    self.handle_tracked_request(&mut session, request, &peer)
                                        .await?,
                                );
                            } else {
                                self.handle_tracked_request(&mut session, request, &peer)
                                    .await?;
                            }
                        }
                        Some(serialize_responses(&responses)?)
                    }
                }
                Err(err) => {
                    let message = err.to_string();
                    self.state.monitor.record_request(
                        &peer,
                        "parse_error",
                        false,
                        Instant::now().elapsed(),
                        Some(message),
                    );
                    Some(serialize_response(&Response::from_error(None, err))?)
                }
            };
            if let Some(response) = response {
                writer.write_all(&response).await?;
            }
        }
        log::debug!("peer {peer} disconnected");
        Ok(())
    }

    async fn handle_tracked_request(
        &self,
        session: &mut Session,
        request: Request,
        peer: &str,
    ) -> anyhow::Result<Response> {
        let method = request.method.clone();
        let start = Instant::now();
        let response = self.handle_request(session, request).await;
        match &response {
            Ok(response) => {
                self.state.monitor.record_request(
                    peer,
                    method,
                    response.error.is_none(),
                    start.elapsed(),
                    response.error.as_ref().map(|error| error.message.clone()),
                );
            }
            Err(err) => {
                self.state.monitor.record_request(
                    peer,
                    method,
                    false,
                    start.elapsed(),
                    Some(err.to_string()),
                );
            }
        }
        response
    }

    async fn handle_request(
        &self,
        session: &mut Session,
        request: Request,
    ) -> anyhow::Result<Response> {
        let id = request.id.clone();
        let result = self.dispatch(session, &request).await;
        Ok(match result {
            Ok(value) => Response::result(id, value),
            Err(err) => Response::from_error(id, err),
        })
    }

    async fn dispatch(
        &self,
        session: &mut Session,
        request: &Request,
    ) -> Result<Value, ProtocolError> {
        match request.method.as_str() {
            "server.version" => self.server_version(session, &request.params),
            "server.features" => self.server_features(),
            "server.banner" => Ok(json!(self.state.config.banner)),
            "server.donation_address" => Ok(json!(self.state.config.donation_address)),
            "server.peers.subscribe" => Ok(json!(self.state.config.peer)),
            "server.ping" => Ok(Value::Null),
            "server.reset_monitor" => {
                self.state.monitor.reset();
                Ok(json!(true))
            }
            "server.add_peer" => {
                let peer = params_array(&request.params)?
                    .first()
                    .map(Value::to_string)
                    .unwrap_or_default();
                session.peers_seen.insert(peer);
                Ok(json!(false))
            }
            "blockchain.headers.subscribe" => {
                session.header_subscribed = true;
                self.state
                    .chain
                    .tip()
                    .map_err(|err| ProtocolError::Server(err.to_string()))?
                    .map(|tip| serde_json::to_value(tip).unwrap())
                    .ok_or_else(|| ProtocolError::Server("chain has no headers".to_string()))
            }
            "blockchain.block.header" => {
                let height = integer_param(&request.params, 0, "height")? as usize;
                Ok(json!(self
                    .state
                    .chain
                    .block_header_hex(height)
                    .map_err(|err| ProtocolError::Server(err.to_string()))?))
            }
            "blockchain.block.headers" => {
                let start = integer_param(&request.params, 0, "start_height")? as usize;
                let count = integer_param(&request.params, 1, "count")? as usize;
                let cp_height = optional_integer_param(&request.params, 2)? as usize;
                let headers = self
                    .state
                    .chain
                    .block_headers(start, count, cp_height)
                    .map_err(|err| ProtocolError::Server(err.to_string()))?;
                Ok(serde_json::to_value(headers).unwrap())
            }
            "blockchain.scripthash.get_history" => self.scripthash_history(&request.params),
            "blockchain.scripthash.get_balance" => self.scripthash_balance(&request.params),
            "blockchain.scripthash.listunspent" => self.scripthash_listunspent(&request.params),
            "blockchain.scripthash.get_mempool" => self.scripthash_mempool(&request.params),
            "blockchain.scripthash.subscribe" => {
                self.scripthash_subscribe(session, &request.params)
            }
            "blockchain.scripthash.unsubscribe" => {
                let sh = string_param(&request.params, 0, "scripthash")?;
                Ok(json!(session.scripthash_status.remove(&sh).is_some()))
            }
            "blockchain.transaction.get" => self.transaction_get(&request.params),
            "blockchain.transaction.broadcast" => {
                let raw = string_param(&request.params, 0, "raw_tx")?;
                self.broadcast(raw.trim()).await
            }
            "blockchain.transaction.broadcast_package" => {
                let raw = params_array(&request.params)?
                    .first()
                    .and_then(Value::as_array)
                    .ok_or_else(|| {
                        ProtocolError::InvalidParams("raw transaction array required".to_string())
                    })?
                    .iter()
                    .map(|value| {
                        value.as_str().map(ToOwned::to_owned).ok_or_else(|| {
                            ProtocolError::InvalidParams("raw transaction must be hex".to_string())
                        })
                    })
                    .collect::<Result<Vec<_>, _>>()?;
                self.state
                    .bitcoind
                    .broadcast_package(&raw)
                    .await
                    .map_err(|err| ProtocolError::Server(err.to_string()))
            }
            "blockchain.transaction.get_merkle" => self.transaction_get_merkle(&request.params),
            "blockchain.transaction.id_from_pos" => self.transaction_id_from_pos(&request.params),
            "blockchain.estimatefee" => {
                let blocks = integer_param(&request.params, 0, "blocks")? as usize;
                self.state
                    .bitcoind
                    .estimate_fee(blocks)
                    .await
                    .map(|fee| json!(fee))
                    .map_err(|err| ProtocolError::Server(err.to_string()))
            }
            "mempool.get_fee_histogram" => {
                let histogram = self.state.mempool.read().unwrap().fee_histogram();
                Ok(json!(histogram))
            }
            "mempool.get_info" => {
                let histogram = self.state.mempool.read().unwrap().fee_histogram();
                Ok(json!({ "loaded": true, "fee_histogram": histogram }))
            }
            "blockchain.relayfee" if !session.negotiated_protocol.supports_1_6() => self
                .state
                .bitcoind
                .relay_fee()
                .await
                .map(|fee| json!(fee))
                .map_err(|err| ProtocolError::Server(err.to_string())),
            method => Err(ProtocolError::MethodNotFound(method.to_string())),
        }
    }

    async fn broadcast(&self, raw_tx_hex: &str) -> Result<Value, ProtocolError> {
        let Some(target) = &self.state.tor_push else {
            return self
                .state
                .bitcoind
                .broadcast(raw_tx_hex)
                .await
                .map(|txid| json!(txid))
                .map_err(|err| ProtocolError::Server(err.to_string()));
        };
        let raw = hex::decode(raw_tx_hex)
            .map_err(|_| ProtocolError::InvalidParams("raw_tx must be hex".to_string()))?;
        let tx: Transaction = deserialize(&raw).map_err(|_| {
            ProtocolError::InvalidParams("raw_tx is not a valid transaction".to_string())
        })?;
        let txid = tx.compute_txid();
        let response = torpush::push_tx(self.state.config.tor_proxy, target, raw_tx_hex)
            .await
            .map_err(|err| ProtocolError::Server(err.to_string()))?;
        if response != txid.to_string() {
            log::warn!("push endpoint returned {response:?} for txid {txid}");
        }
        Ok(json!(txid.to_string()))
    }

    fn server_version(
        &self,
        session: &mut Session,
        params: &Params,
    ) -> Result<Value, ProtocolError> {
        let values = params_array(params)?;
        let (client_min, client_max) = match values.get(1) {
            Some(Value::Array(range)) if range.len() == 2 => {
                let min = range[0]
                    .as_str()
                    .ok_or_else(|| {
                        ProtocolError::InvalidParams("protocol min must be string".to_string())
                    })?
                    .parse()
                    .map_err(ProtocolError::InvalidParams)?;
                let max = range[1]
                    .as_str()
                    .ok_or_else(|| {
                        ProtocolError::InvalidParams("protocol max must be string".to_string())
                    })?
                    .parse()
                    .map_err(ProtocolError::InvalidParams)?;
                (Some(min), Some(max))
            }
            Some(Value::String(_)) => (None, optional_protocol_param(params, 1)?),
            None => (None, None),
            _ => {
                return Err(ProtocolError::InvalidParams(
                    "protocol version must be string or [min, max]".to_string(),
                ))
            }
        };

        let negotiated = ProtocolVersion::negotiate(
            client_min.as_ref(),
            client_max.as_ref(),
            &self.state.config.protocol_min,
            &self.state.config.protocol_max,
        )
        .ok_or_else(|| ProtocolError::Server("unsupported protocol version".to_string()))?;
        session.negotiated_protocol = negotiated.clone();
        Ok(json!(["bindex-electrum", negotiated.to_string()]))
    }

    fn server_features(&self) -> Result<Value, ProtocolError> {
        let genesis = self
            .state
            .chain
            .genesis_hash()
            .map_err(|err| ProtocolError::Server(err.to_string()))?
            .ok_or_else(|| ProtocolError::Server("chain has no genesis header".to_string()))?;
        Ok(server_features(
            genesis,
            &self.state.config.protocol_min,
            &self.state.config.protocol_max,
        ))
    }

    fn scripthash_history(&self, params: &Params) -> Result<Value, ProtocolError> {
        let scripthash = parse_scripthash(params)?;
        let confirmed = self
            .state
            .chain
            .confirmed_scripthash(scripthash)
            .map_err(|err| ProtocolError::Server(err.to_string()))?;
        let mut history = confirmed
            .history
            .into_iter()
            .map(|tx| crate::protocol::HistoryEntry {
                tx_hash: tx.tx_hash,
                height: tx.height as i64,
                fee: None,
            })
            .collect::<Vec<_>>();
        let mut mempool = self
            .state
            .mempool
            .read()
            .unwrap()
            .history_entries(scripthash);
        history.append(&mut mempool);
        Ok(json!(history))
    }

    fn scripthash_balance(&self, params: &Params) -> Result<Value, ProtocolError> {
        let scripthash = parse_scripthash(params)?;
        let confirmed = self
            .state
            .chain
            .confirmed_scripthash(scripthash)
            .map_err(|err| ProtocolError::Server(err.to_string()))?
            .utxos
            .into_iter()
            .map(|utxo| utxo.value)
            .sum::<u64>();
        let unconfirmed = 0i64;
        Ok(json!({ "confirmed": confirmed, "unconfirmed": unconfirmed }))
    }

    fn scripthash_listunspent(&self, params: &Params) -> Result<Value, ProtocolError> {
        let scripthash = parse_scripthash(params)?;
        let utxos = self
            .state
            .chain
            .confirmed_scripthash(scripthash)
            .map_err(|err| ProtocolError::Server(err.to_string()))?
            .utxos;
        Ok(json!(utxos
            .into_iter()
            .map(|utxo| json!({
                "tx_hash": utxo.tx_hash,
                "tx_pos": utxo.tx_pos,
                "height": utxo.height,
                "value": utxo.value
            }))
            .collect::<Vec<_>>()))
    }

    fn scripthash_mempool(&self, params: &Params) -> Result<Value, ProtocolError> {
        let scripthash = parse_scripthash(params)?;
        Ok(json!(self
            .state
            .mempool
            .read()
            .unwrap()
            .entries(scripthash)))
    }

    fn scripthash_subscribe(
        &self,
        session: &mut Session,
        params: &Params,
    ) -> Result<Value, ProtocolError> {
        if session.scripthash_status.len() >= self.state.config.max_subscriptions_per_session {
            return Err(ProtocolError::Server(
                "subscription limit exceeded".to_string(),
            ));
        }
        let scripthash = parse_scripthash(params)?;
        let confirmed = self
            .state
            .chain
            .confirmed_scripthash(scripthash)
            .map_err(|err| ProtocolError::Server(err.to_string()))?;
        let mut full_history = confirmed
            .history
            .into_iter()
            .map(|tx| crate::protocol::HistoryEntry {
                tx_hash: tx.tx_hash,
                height: tx.height as i64,
                fee: None,
            })
            .collect::<Vec<_>>();
        full_history.extend(
            self.state
                .mempool
                .read()
                .unwrap()
                .history_entries(scripthash),
        );
        let status = scripthash_status(&full_history);
        session
            .scripthash_status
            .insert(scripthash.to_string(), status.clone());
        Ok(json!(status))
    }

    fn transaction_get(&self, params: &Params) -> Result<Value, ProtocolError> {
        let txid = txid_param(params, 0)?;
        let verbose = optional_bool_param(params, 1)?;
        if let Some(raw) = self.state.mempool.read().unwrap().raw_transaction(&txid) {
            return Ok(tx_value(raw, verbose));
        }
        let raw = self
            .state
            .chain
            .transaction_by_txid(&txid)
            .map_err(|err| ProtocolError::Server(err.to_string()))?
            .ok_or_else(|| ProtocolError::Server("transaction not found".to_string()))?;
        Ok(tx_value(&raw, verbose))
    }

    fn transaction_get_merkle(&self, params: &Params) -> Result<Value, ProtocolError> {
        let txid = txid_param(params, 0)?;
        let height = nonnegative_integer_param(params, 1, "height")? as usize;
        let txids = self
            .state
            .chain
            .block_txids(height)
            .map_err(|err| ProtocolError::Server(err.to_string()))?;
        let pos = txids
            .iter()
            .position(|item| *item == txid)
            .ok_or_else(|| ProtocolError::Server("transaction not found in block".to_string()))?;
        Ok(merkle_value(height, &txids, pos))
    }

    fn transaction_id_from_pos(&self, params: &Params) -> Result<Value, ProtocolError> {
        let height = nonnegative_integer_param(params, 0, "height")? as usize;
        let pos = nonnegative_integer_param(params, 1, "tx_pos")? as usize;
        let include_merkle = optional_bool_param(params, 2)?;
        let txids = self
            .state
            .chain
            .block_txids(height)
            .map_err(|err| ProtocolError::Server(err.to_string()))?;
        let txid = txids
            .get(pos)
            .ok_or_else(|| ProtocolError::InvalidParams("tx_pos out of range".to_string()))?;
        if include_merkle {
            let mut value = merkle_value(height, &txids, pos);
            value["tx_hash"] = json!(txid.to_string());
            Ok(value)
        } else {
            Ok(json!(txid.to_string()))
        }
    }
}

fn merkle_value(height: usize, txids: &[Txid], pos: usize) -> Value {
    let leaves = txids
        .iter()
        .map(|txid| *txid.as_raw_hash())
        .collect::<Vec<_>>();
    let proof = merkle::branch_and_root(&leaves, pos);
    json!({
        "block_height": height,
        "merkle": proof.branch.into_iter().map(|hash| hash.to_string()).collect::<Vec<_>>(),
        "pos": pos
    })
}

fn tx_value(raw: &[u8], verbose: bool) -> Value {
    if verbose {
        json!({ "hex": hex::encode(raw) })
    } else {
        json!(hex::encode(raw))
    }
}

fn parse_scripthash(params: &Params) -> Result<ElectrumScripthash, ProtocolError> {
    ElectrumScripthash::parse(&string_param(params, 0, "scripthash")?)
}

fn integer_param(params: &Params, index: usize, name: &str) -> Result<i64, ProtocolError> {
    params_array(params)?
        .get(index)
        .and_then(Value::as_i64)
        .ok_or_else(|| ProtocolError::InvalidParams(format!("missing integer param {name}")))
}

fn nonnegative_integer_param(
    params: &Params,
    index: usize,
    name: &str,
) -> Result<i64, ProtocolError> {
    let value = integer_param(params, index, name)?;
    if value < 0 {
        return Err(ProtocolError::InvalidParams(format!(
            "{name} must be non-negative"
        )));
    }
    Ok(value)
}

fn optional_integer_param(params: &Params, index: usize) -> Result<i64, ProtocolError> {
    Ok(params_array(params)?
        .get(index)
        .and_then(Value::as_i64)
        .unwrap_or(0))
}

fn optional_bool_param(params: &Params, index: usize) -> Result<bool, ProtocolError> {
    Ok(params_array(params)?
        .get(index)
        .and_then(Value::as_bool)
        .unwrap_or(false))
}

fn txid_param(params: &Params, index: usize) -> Result<Txid, ProtocolError> {
    Txid::from_str(&string_param(params, index, "txid")?)
        .map_err(|_| ProtocolError::InvalidParams("invalid txid".to_string()))
}

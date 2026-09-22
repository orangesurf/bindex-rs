use std::{fs, path::PathBuf};

use serde_json::{json, Value};

#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error("bitcoind RPC transport failed: {0}")]
    Transport(#[from] ureq::Error),

    #[error("bitcoind RPC response decode failed: {0}")]
    Decode(#[from] std::io::Error),

    #[error("bitcoind RPC error: {0}")]
    Rpc(String),

    #[error("bitcoind RPC auth error: {0}")]
    Auth(String),

    #[error("bitcoind RPC worker failed: {0}")]
    Join(#[from] tokio::task::JoinError),
}

#[derive(Debug, Clone)]
pub struct RpcClient {
    agent: ureq::Agent,
    url: String,
    user: Option<String>,
    password: Option<String>,
    cookie_path: Option<PathBuf>,
    conf_path: Option<PathBuf>,
}

impl RpcClient {
    pub fn new(
        url: String,
        user: Option<String>,
        password: Option<String>,
        cookie_path: Option<PathBuf>,
        conf_path: Option<PathBuf>,
    ) -> Self {
        Self {
            agent: ureq::Agent::new_with_defaults(),
            url,
            user,
            password,
            cookie_path,
            conf_path,
        }
    }

    pub fn url(&self) -> &str {
        &self.url
    }

    pub async fn call(&self, method: &str, params: Value) -> Result<Value, Error> {
        let client = self.clone();
        let method = method.to_string();
        tokio::task::spawn_blocking(move || client.call_blocking(&method, params)).await?
    }

    fn call_blocking(&self, method: &str, params: Value) -> Result<Value, Error> {
        let request = json!({
            "jsonrpc": "1.0",
            "id": "bindex-electrum",
            "method": method,
            "params": params,
        });
        let mut builder = self
            .agent
            .post(&self.url)
            .header("Content-Type", "application/json");

        if let Some((user, password)) = self.credentials()? {
            use base64::Engine as _;
            let auth =
                base64::engine::general_purpose::STANDARD.encode(format!("{user}:{password}"));
            builder = builder.header("Authorization", format!("Basic {auth}"));
        }

        let mut response = builder.send_json(&request)?;
        let response: Value = response.body_mut().read_json()?;
        if let Some(err) = response.get("error").filter(|value| !value.is_null()) {
            return Err(Error::Rpc(err.to_string()));
        }
        response
            .get("result")
            .cloned()
            .ok_or_else(|| Error::Rpc("missing JSON-RPC result".to_string()))
    }

    /// A blocking call that keeps the node's error object: an RPC error comes
    /// back as `Ok(Err(error))` rather than as an HTTP status, so callers can
    /// tell "no such block" from a transport failure. Only for blocking contexts.
    #[cfg(feature = "liquid")]
    pub fn call_sync(&self, method: &str, params: Value) -> Result<Result<Value, Value>, Error> {
        let request = json!({
            "jsonrpc": "1.0",
            "id": "bindex-electrum",
            "method": method,
            "params": params,
        });
        let mut builder = self
            .agent
            .post(&self.url)
            .config()
            .http_status_as_error(false)
            .build()
            .header("Content-Type", "application/json");
        if let Some((user, password)) = self.credentials()? {
            use base64::Engine as _;
            let auth =
                base64::engine::general_purpose::STANDARD.encode(format!("{user}:{password}"));
            builder = builder.header("Authorization", format!("Basic {auth}"));
        }
        let mut response = builder.send_json(&request)?;
        if response.status() == 401 {
            return Err(Error::Auth("node refused the RPC credentials".to_string()));
        }
        let response: Value = response.body_mut().with_config().limit(1 << 30).read_json()?;
        if let Some(err) = response.get("error").filter(|value| !value.is_null()) {
            return Ok(Err(err.clone()));
        }
        response
            .get("result")
            .cloned()
            .map(Ok)
            .ok_or_else(|| Error::Rpc("missing JSON-RPC result".to_string()))
    }

    fn credentials(&self) -> Result<Option<(String, String)>, Error> {
        if let (Some(user), Some(password)) = (&self.user, &self.password) {
            return Ok(Some((user.clone(), password.clone())));
        }
        if let Some(path) = &self.cookie_path {
            let cookie = fs::read_to_string(path)
                .map_err(|err| Error::Auth(format!("read cookie {}: {err}", path.display())))?;
            let (user, password) = cookie
                .trim()
                .split_once(':')
                .ok_or_else(|| Error::Auth(format!("invalid cookie {}", path.display())))?;
            return Ok(Some((user.to_string(), password.to_string())));
        }
        if let Some(path) = &self.conf_path {
            return read_conf_credentials(path).map(Some);
        }
        Ok(None)
    }

    pub async fn broadcast(&self, raw_tx_hex: &str) -> Result<String, Error> {
        let value = self.call("sendrawtransaction", json!([raw_tx_hex])).await?;
        value
            .as_str()
            .map(ToOwned::to_owned)
            .ok_or_else(|| Error::Rpc("sendrawtransaction returned non-string result".to_string()))
    }

    pub async fn broadcast_package(&self, raw_txs: &[String]) -> Result<Value, Error> {
        self.call("submitpackage", json!([raw_txs])).await
    }

    pub async fn estimate_fee(&self, blocks: usize) -> Result<f64, Error> {
        let value = self.call("estimatesmartfee", json!([blocks])).await?;
        Ok(value.get("feerate").and_then(Value::as_f64).unwrap_or(-1.0))
    }

    pub async fn relay_fee(&self) -> Result<f64, Error> {
        let value = self.call("getnetworkinfo", json!([])).await?;
        Ok(value.get("relayfee").and_then(Value::as_f64).unwrap_or(0.0))
    }
}

fn read_conf_credentials(path: &PathBuf) -> Result<(String, String), Error> {
    let text = fs::read_to_string(path)
        .map_err(|err| Error::Auth(format!("read config {}: {err}", path.display())))?;
    let mut user = None;
    let mut password = None;
    for line in text.lines() {
        let line = line.trim();
        if line.is_empty() || line.starts_with('#') || line.starts_with('[') {
            continue;
        }
        let Some((key, value)) = line.split_once('=') else {
            continue;
        };
        match key.trim() {
            "rpcuser" => user = Some(value.trim().to_string()),
            "rpcpassword" => password = Some(value.trim().to_string()),
            _ => {}
        }
    }
    match (user, password) {
        (Some(user), Some(password)) => Ok((user, password)),
        _ => Err(Error::Auth(format!(
            "missing rpcuser/rpcpassword in {}",
            path.display()
        ))),
    }
}

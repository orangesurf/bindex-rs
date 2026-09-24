use std::{cmp::Ordering, fmt, str::FromStr};

use bitcoin::hashes::{sha256, Hash as _};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};

#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error("invalid JSON-RPC frame: {0}")]
    Json(#[from] serde_json::Error),

    #[error("invalid request")]
    InvalidRequest,

    #[error("invalid params: {0}")]
    InvalidParams(String),

    #[error("method not found: {0}")]
    MethodNotFound(String),

    #[error("server error: {0}")]
    Server(String),
}

#[derive(Debug, Clone, Eq, PartialEq, Hash)]
pub struct ProtocolVersion {
    components: Vec<u32>,
}

impl ProtocolVersion {
    pub fn new(major: u32, minor: u32) -> Self {
        Self {
            components: vec![major, minor],
        }
    }

    pub fn v1_4() -> Self {
        Self::new(1, 4)
    }

    pub fn v1_6() -> Self {
        Self::new(1, 6)
    }

    pub fn negotiate(
        client_min: Option<&ProtocolVersion>,
        client_max: Option<&ProtocolVersion>,
        server_min: &ProtocolVersion,
        server_max: &ProtocolVersion,
    ) -> Option<Self> {
        let min = client_min.map_or(server_min, |v| std::cmp::max(v, server_min));
        let max = client_max.map_or(server_max, |v| std::cmp::min(v, server_max));
        (min <= max).then(|| max.clone())
    }

    pub fn supports_1_6(&self) -> bool {
        self >= &Self::v1_6()
    }
}

impl Default for ProtocolVersion {
    fn default() -> Self {
        Self::v1_6()
    }
}

impl FromStr for ProtocolVersion {
    type Err = String;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        let mut components = s
            .split('.')
            .map(|part| {
                if part.is_empty() {
                    return Err(format!("empty protocol version component in {s:?}"));
                }
                part.parse::<u32>()
                    .map_err(|_| format!("invalid protocol version component {part:?}"))
            })
            .collect::<Result<Vec<_>, _>>()?;
        if components.len() < 2 {
            return Err(format!(
                "protocol version {s:?} must include major and minor"
            ));
        }
        while components.len() > 2 && components.last() == Some(&0) {
            components.pop();
        }
        Ok(Self { components })
    }
}

impl fmt::Display for ProtocolVersion {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        for (i, component) in self.components.iter().enumerate() {
            if i > 0 {
                f.write_str(".")?;
            }
            write!(f, "{component}")?;
        }
        Ok(())
    }
}

impl Serialize for ProtocolVersion {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: serde::Serializer,
    {
        serializer.serialize_str(&self.to_string())
    }
}

impl<'de> Deserialize<'de> for ProtocolVersion {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        let s = String::deserialize(deserializer)?;
        s.parse().map_err(serde::de::Error::custom)
    }
}

impl PartialOrd for ProtocolVersion {
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
        Some(self.cmp(other))
    }
}

impl Ord for ProtocolVersion {
    fn cmp(&self, other: &Self) -> Ordering {
        let len = self.components.len().max(other.components.len());
        for i in 0..len {
            let left = *self.components.get(i).unwrap_or(&0);
            let right = *other.components.get(i).unwrap_or(&0);
            match left.cmp(&right) {
                Ordering::Equal => {}
                ordering => return ordering,
            }
        }
        Ordering::Equal
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(untagged)]
pub enum Id {
    Number(i64),
    String(String),
    Null,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Request {
    #[serde(default)]
    pub jsonrpc: Option<String>,
    pub id: Option<Id>,
    pub method: String,
    #[serde(default)]
    pub params: Params,
}

#[derive(Debug, Clone, PartialEq, Default, Serialize, Deserialize)]
#[serde(untagged)]
pub enum Params {
    Array(Vec<Value>),
    Object(serde_json::Map<String, Value>),
    #[default]
    None,
}

#[derive(Debug, Clone, PartialEq)]
pub enum Frame {
    Single(Request),
    Batch(Vec<Request>),
}

#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct Response {
    pub jsonrpc: &'static str,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub id: Option<Id>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub result: Option<Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<RpcError>,
}

#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct RpcError {
    pub code: i64,
    pub message: String,
}

impl Response {
    pub fn result(id: Option<Id>, result: Value) -> Self {
        Self {
            jsonrpc: "2.0",
            id,
            result: Some(result),
            error: None,
        }
    }

    pub fn error(id: Option<Id>, code: i64, message: impl Into<String>) -> Self {
        Self {
            jsonrpc: "2.0",
            id,
            result: None,
            error: Some(RpcError {
                code,
                message: message.into(),
            }),
        }
    }

    pub fn from_error(id: Option<Id>, err: Error) -> Self {
        match err {
            Error::InvalidRequest => Self::error(id, -32600, "Invalid Request"),
            Error::MethodNotFound(method) => {
                Self::error(id, -32601, format!("Method not found: {method}"))
            }
            Error::InvalidParams(message) => Self::error(id, -32602, message),
            Error::Json(err) => Self::error(id, -32700, err.to_string()),
            Error::Server(message) => Self::error(id, -32000, message),
        }
    }
}

pub fn parse_line(line: &[u8]) -> Result<Frame, Error> {
    let value: Value = serde_json::from_slice(line)?;
    match value {
        Value::Array(values) => {
            let mut requests = Vec::with_capacity(values.len());
            for value in values {
                requests.push(serde_json::from_value(value)?);
            }
            Ok(Frame::Batch(requests))
        }
        Value::Object(_) => Ok(Frame::Single(serde_json::from_value(value)?)),
        _ => Err(Error::InvalidRequest),
    }
}

pub fn serialize_response(response: &Response) -> Result<Vec<u8>, Error> {
    let mut bytes = serde_json::to_vec(response)?;
    bytes.push(b'\n');
    Ok(bytes)
}

pub fn serialize_responses(responses: &[Response]) -> Result<Vec<u8>, Error> {
    let mut bytes = serde_json::to_vec(responses)?;
    bytes.push(b'\n');
    Ok(bytes)
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct ElectrumScripthash(pub [u8; 32]);

impl ElectrumScripthash {
    pub fn parse(s: &str) -> Result<Self, Error> {
        let bytes = hex::decode(s)
            .map_err(|_| Error::InvalidParams("scripthash must be 32-byte hex".to_string()))?;
        let bytes: [u8; 32] = bytes
            .try_into()
            .map_err(|_| Error::InvalidParams("scripthash must be 32-byte hex".to_string()))?;
        Ok(Self(bytes))
    }

    pub fn from_script(script: &bitcoin::Script) -> Self {
        Self::parse(&bindex::ScriptHash::new(script).to_string())
            .expect("bindex-generated scripthash must parse")
    }

    pub fn to_bindex(self) -> Result<bindex::ScriptHash, Error> {
        self.to_string()
            .parse()
            .map_err(|_| Error::InvalidParams("invalid scripthash".to_string()))
    }
}

impl fmt::Display for ElectrumScripthash {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&hex::encode(self.0))
    }
}

impl FromStr for ElectrumScripthash {
    type Err = Error;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        Self::parse(s)
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct HistoryEntry {
    pub tx_hash: String,
    pub height: i64,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub fee: Option<u64>,
}

/// The protocol's script status: the SHA-256 of `tx_hash:height:` for each
/// history entry in order, or `None` for an empty history. Fees are not part of
/// it; wallets recompute it from `get_history` and must arrive at the same
/// string.
pub fn scripthash_status(history: &[HistoryEntry]) -> Option<String> {
    if history.is_empty() {
        return None;
    }
    let mut payload = String::new();
    for item in history {
        payload.push_str(&item.tx_hash);
        payload.push(':');
        payload.push_str(&item.height.to_string());
        payload.push(':');
    }
    Some(sha256::Hash::hash(payload.as_bytes()).to_string())
}

pub fn params_array(params: &Params) -> Result<&[Value], Error> {
    match params {
        Params::Array(values) => Ok(values),
        Params::None => Ok(&[]),
        Params::Object(_) => Err(Error::InvalidParams(
            "named params are not supported for this method".to_string(),
        )),
    }
}

pub fn string_param(params: &Params, index: usize, name: &str) -> Result<String, Error> {
    let values = params_array(params)?;
    values
        .get(index)
        .and_then(Value::as_str)
        .map(ToOwned::to_owned)
        .ok_or_else(|| Error::InvalidParams(format!("missing string param {name}")))
}

pub fn optional_protocol_param(
    params: &Params,
    index: usize,
) -> Result<Option<ProtocolVersion>, Error> {
    let values = params_array(params)?;
    values
        .get(index)
        .map(|value| {
            value
                .as_str()
                .ok_or_else(|| {
                    Error::InvalidParams("protocol version must be a string".to_string())
                })
                .and_then(|s| s.parse().map_err(Error::InvalidParams))
        })
        .transpose()
}

pub fn server_features(
    genesis_hash: bitcoin::BlockHash,
    protocol_min: &ProtocolVersion,
    protocol_max: &ProtocolVersion,
) -> Value {
    json!({
        "genesis_hash": genesis_hash.to_string(),
        "hosts": {},
        "protocol_min": protocol_min.to_string(),
        "protocol_max": protocol_max.to_string(),
        "server_version": env!("CARGO_PKG_VERSION"),
        "hash_function": "sha256"
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn protocol_versions_compare_numerically() {
        assert!("1.4".parse::<ProtocolVersion>().unwrap() < "1.4.2".parse().unwrap());
        assert!("1.4.10".parse::<ProtocolVersion>().unwrap() > "1.4.2".parse().unwrap());
        assert_eq!(
            "1.6.0".parse::<ProtocolVersion>().unwrap(),
            "1.6".parse().unwrap()
        );
    }

    #[test]
    fn protocol_negotiation_picks_overlap_max() {
        let server_min = "1.4".parse().unwrap();
        let server_max = "1.6".parse().unwrap();
        let client_min = "1.4.2".parse().unwrap();
        let client_max = "1.5".parse().unwrap();
        let negotiated = ProtocolVersion::negotiate(
            Some(&client_min),
            Some(&client_max),
            &server_min,
            &server_max,
        )
        .unwrap();
        assert_eq!(negotiated.to_string(), "1.5");
    }

    #[test]
    fn json_rpc_single_and_batch_parse() {
        let single = parse_line(br#"{"jsonrpc":"2.0","id":1,"method":"server.ping"}"#).unwrap();
        assert!(matches!(single, Frame::Single(_)));

        let batch = parse_line(
            br#"[{"jsonrpc":"2.0","id":1,"method":"server.ping"},{"id":"x","method":"server.banner"}]"#,
        )
        .unwrap();
        match batch {
            Frame::Batch(items) => assert_eq!(items.len(), 2),
            _ => panic!("expected batch"),
        }
    }

    #[test]
    fn scripthash_requires_32_byte_hex() {
        assert!(ElectrumScripthash::parse(&"00".repeat(32)).is_ok());
        assert!(ElectrumScripthash::parse("00").is_err());
        assert!(ElectrumScripthash::parse(&"zz".repeat(32)).is_err());
    }

    #[test]
    fn status_hash_includes_unconfirmed_fee() {
        let history = vec![
            HistoryEntry {
                tx_hash: "a".repeat(64),
                height: 100,
                fee: None,
            },
            HistoryEntry {
                tx_hash: "b".repeat(64),
                height: 0,
                fee: Some(250),
            },
        ];
        assert_eq!(
            scripthash_status(&history),
            Some(
                sha256::Hash::hash(
                    format!("{}:100:{}:0:", "a".repeat(64), "b".repeat(64)).as_bytes()
                )
                .to_string()
            )
        );
    }
}

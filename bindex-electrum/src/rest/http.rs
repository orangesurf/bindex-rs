//! A very small HTTP/1.1 server.
//!
//! The REST API needs nothing that a general-purpose HTTP stack provides: every
//! request is a `GET` or a `POST` with a `Content-Length` body, every response
//! is a buffered byte string. Hand-rolling it keeps the dependency footprint of
//! this crate unchanged (no hyper, no axum, no tower).

use std::{collections::HashMap, io};

use tokio::io::{AsyncBufRead, AsyncBufReadExt as _, AsyncReadExt as _, AsyncWrite, AsyncWriteExt as _};

/// Largest request body accepted before the connection is dropped. A 25-element
/// package of 400 kB transactions is the biggest legitimate body.
pub const MAX_BODY: usize = 32 << 20;

const MAX_LINE: usize = 64 << 10;
const MAX_HEADERS: usize = 100;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HttpRequest {
    pub method: String,
    /// Percent-decoded path, without the query string.
    pub path: String,
    /// Raw path as received (used verbatim in the 404 message).
    pub raw_target: String,
    pub query: HashMap<String, String>,
    pub body: Vec<u8>,
    pub keep_alive: bool,
}

impl HttpRequest {
    /// Path split into non-empty segments, which is what the router matches on.
    pub fn segments(&self) -> Vec<&str> {
        self.path.split('/').filter(|s| !s.is_empty()).collect()
    }

    pub fn param(&self, name: &str) -> Option<&str> {
        self.query.get(name).map(String::as_str)
    }
}

/// Read one request. `Ok(None)` means the peer closed the connection cleanly.
pub async fn read_request<R>(reader: &mut R) -> io::Result<Option<HttpRequest>>
where
    R: AsyncBufRead + Unpin,
{
    let Some(request_line) = read_line(reader).await? else {
        return Ok(None);
    };
    if request_line.trim().is_empty() {
        // tolerate a stray CRLF between pipelined requests
        let Some(next) = read_line(reader).await? else {
            return Ok(None);
        };
        return parse_request(next, reader).await;
    }
    parse_request(request_line, reader).await
}

async fn parse_request<R>(request_line: String, reader: &mut R) -> io::Result<Option<HttpRequest>>
where
    R: AsyncBufRead + Unpin,
{
    let mut parts = request_line.trim_end().split(' ');
    let method = parts.next().unwrap_or_default().to_string();
    let target = parts.next().unwrap_or_default().to_string();
    let version = parts.next().unwrap_or("HTTP/1.1").to_string();
    if method.is_empty() || target.is_empty() {
        return Err(io::Error::new(io::ErrorKind::InvalidData, "malformed request line"));
    }

    let mut headers = HashMap::new();
    for _ in 0..MAX_HEADERS {
        let Some(line) = read_line(reader).await? else {
            break;
        };
        let line = line.trim_end_matches(['\r', '\n']);
        if line.is_empty() {
            break;
        }
        if let Some((name, value)) = line.split_once(':') {
            headers.insert(name.trim().to_ascii_lowercase(), value.trim().to_string());
        }
    }

    if headers
        .get("transfer-encoding")
        .is_some_and(|value| value.to_ascii_lowercase().contains("chunked"))
    {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "chunked request bodies are not supported",
        ));
    }

    let content_length = headers
        .get("content-length")
        .and_then(|value| value.parse::<usize>().ok())
        .unwrap_or(0);
    if content_length > MAX_BODY {
        return Err(io::Error::new(io::ErrorKind::InvalidData, "request body too large"));
    }
    let mut body = vec![0u8; content_length];
    if content_length > 0 {
        reader.read_exact(&mut body).await?;
    }

    let keep_alive = match headers.get("connection").map(|v| v.to_ascii_lowercase()) {
        Some(value) if value.contains("close") => false,
        Some(value) if value.contains("keep-alive") => true,
        _ => version != "HTTP/1.0",
    };

    let (raw_path, raw_query) = match target.split_once('?') {
        Some((path, query)) => (path, Some(query)),
        None => (target.as_str(), None),
    };

    Ok(Some(HttpRequest {
        method,
        path: percent_decode(raw_path),
        raw_target: raw_path.to_string(),
        query: raw_query.map(parse_query).unwrap_or_default(),
        body,
        keep_alive,
    }))
}

async fn read_line<R>(reader: &mut R) -> io::Result<Option<String>>
where
    R: AsyncBufRead + Unpin,
{
    let mut buf = Vec::new();
    let mut limited = reader.take(MAX_LINE as u64);
    let read = limited.read_until(b'\n', &mut buf).await?;
    if read == 0 {
        return Ok(None);
    }
    Ok(Some(String::from_utf8_lossy(&buf).into_owned()))
}

fn parse_query(query: &str) -> HashMap<String, String> {
    query
        .split('&')
        .filter(|pair| !pair.is_empty())
        .map(|pair| match pair.split_once('=') {
            Some((key, value)) => (percent_decode(key), percent_decode(value)),
            None => (percent_decode(pair), String::new()),
        })
        .collect()
}

/// `%XX` decoding. `+` is left alone: every parameter this API takes is hex,
/// a decimal or a comma-separated list, never a form-encoded phrase.
fn percent_decode(input: &str) -> String {
    if !input.contains('%') {
        return input.to_string();
    }
    let bytes = input.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'%' && i + 2 < bytes.len() {
            let hi = (bytes[i + 1] as char).to_digit(16);
            let lo = (bytes[i + 2] as char).to_digit(16);
            if let (Some(hi), Some(lo)) = (hi, lo) {
                out.push((hi * 16 + lo) as u8);
                i += 3;
                continue;
            }
        }
        out.push(bytes[i]);
        i += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HttpResponse {
    pub status: u16,
    pub content_type: &'static str,
    pub body: Vec<u8>,
    /// `Cache-Control: public, max-age=<ttl>` when present.
    pub cache_max_age: Option<u32>,
}

impl HttpResponse {
    pub fn text(status: u16, body: impl Into<String>, cache_max_age: Option<u32>) -> Self {
        Self {
            status,
            content_type: "text/plain",
            body: body.into().into_bytes(),
            cache_max_age,
        }
    }

    pub fn json(body: Vec<u8>, cache_max_age: Option<u32>) -> Self {
        Self {
            status: 200,
            content_type: "application/json",
            body,
            cache_max_age,
        }
    }

    pub fn binary(body: Vec<u8>, cache_max_age: Option<u32>) -> Self {
        Self {
            status: 200,
            content_type: "application/octet-stream",
            body,
            cache_max_age,
        }
    }
}

fn reason(status: u16) -> &'static str {
    match status {
        200 => "OK",
        400 => "Bad Request",
        404 => "Not Found",
        422 => "Unprocessable Entity",
        500 => "Internal Server Error",
        _ => "Unknown",
    }
}

/// Headers applied to every response, matching the reference's identity headers.
#[derive(Debug, Clone, Default)]
pub struct IdentityHeaders {
    pub powered_by: String,
    pub cors: bool,
    pub bitcoin_version: Option<String>,
}

pub async fn write_response<W>(
    writer: &mut W,
    response: &HttpResponse,
    identity: &IdentityHeaders,
    keep_alive: bool,
) -> io::Result<()>
where
    W: AsyncWrite + Unpin,
{
    let mut head = format!(
        "HTTP/1.1 {} {}\r\nContent-Type: {}\r\nContent-Length: {}\r\n",
        response.status,
        reason(response.status),
        response.content_type,
        response.body.len()
    );
    head.push_str(&format!("X-Powered-By: {}\r\n", identity.powered_by));
    if identity.cors {
        head.push_str("Access-Control-Allow-Origin: *\r\n");
    }
    if let Some(version) = &identity.bitcoin_version {
        head.push_str(&format!("X-Bitcoin-Version: {version}\r\n"));
    }
    if let Some(ttl) = response.cache_max_age {
        head.push_str(&format!("Cache-Control: public, max-age={ttl}\r\n"));
    }
    head.push_str(if keep_alive {
        "Connection: keep-alive\r\n\r\n"
    } else {
        "Connection: close\r\n\r\n"
    });

    writer.write_all(head.as_bytes()).await?;
    writer.write_all(&response.body).await?;
    writer.flush().await
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::io::BufReader;

    async fn parse(raw: &str) -> HttpRequest {
        let mut reader = BufReader::new(raw.as_bytes());
        read_request(&mut reader).await.unwrap().unwrap()
    }

    #[tokio::test]
    async fn parses_path_and_query() {
        let request = parse("GET /txs/outspends?txids=aa,bb&max_txs=5 HTTP/1.1\r\nHost: x\r\n\r\n").await;
        assert_eq!(request.method, "GET");
        assert_eq!(request.segments(), ["txs", "outspends"]);
        assert_eq!(request.param("txids"), Some("aa,bb"));
        assert_eq!(request.param("max_txs"), Some("5"));
        assert!(request.keep_alive);
        assert!(request.body.is_empty());
    }

    #[tokio::test]
    async fn reads_a_body_and_honours_connection_close() {
        let request = parse(
            "POST /tx HTTP/1.1\r\nContent-Length: 4\r\nConnection: close\r\n\r\ndead",
        )
        .await;
        assert_eq!(request.body, b"dead");
        assert!(!request.keep_alive);
    }

    #[tokio::test]
    async fn percent_decodes_the_path() {
        let request = parse("GET /address/bc1q%20x HTTP/1.1\r\n\r\n").await;
        assert_eq!(request.path, "/address/bc1q x");
        assert_eq!(request.raw_target, "/address/bc1q%20x");
    }

    #[tokio::test]
    async fn empty_stream_ends_the_connection() {
        let mut reader = BufReader::new(&b""[..]);
        assert!(read_request(&mut reader).await.unwrap().is_none());
    }

    #[tokio::test]
    async fn writes_identity_and_cache_headers() {
        let mut out = Vec::new();
        let identity = IdentityHeaders {
            powered_by: "bindex-electrum/0.1.0".to_string(),
            cors: true,
            bitcoin_version: Some("/Satoshi:30.0.0/".to_string()),
        };
        write_response(&mut out, &HttpResponse::text(200, "hi", Some(10)), &identity, true)
            .await
            .unwrap();
        let text = String::from_utf8(out).unwrap();
        assert!(text.starts_with("HTTP/1.1 200 OK\r\n"), "{text}");
        assert!(text.contains("X-Powered-By: bindex-electrum/0.1.0\r\n"));
        assert!(text.contains("Access-Control-Allow-Origin: *\r\n"));
        assert!(text.contains("X-Bitcoin-Version: /Satoshi:30.0.0/\r\n"));
        assert!(text.contains("Cache-Control: public, max-age=10\r\n"));
        assert!(text.ends_with("\r\n\r\nhi"));
    }
}

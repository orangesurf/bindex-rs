//! A very small HTTP/1.1 server.
//!
//! The REST API needs nothing that a general-purpose HTTP stack provides: every
//! request is a `GET` or a `POST` with a `Content-Length` body, every response
//! is a buffered byte string. Hand-rolling it keeps the dependency footprint of
//! this crate unchanged (no hyper, no axum, no tower).

use std::{collections::HashMap, io};

use tokio::io::{AsyncBufRead, AsyncBufReadExt as _, AsyncReadExt as _, AsyncWrite, AsyncWriteExt as _};

/// Largest request body accepted unless `--request-body-bytes-cap` says
/// otherwise: 25 transactions of 800 000 hex characters plus JSON framing,
/// which is the biggest legitimate body (`POST /txs/package`).
pub const MAX_BODY: usize = 20_000_200;

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

/// What reading one request produced.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ReadOutcome {
    Request(Box<HttpRequest>),
    /// The peer closed the connection.
    Closed,
    /// The request cannot be framed. The server answers with this status and
    /// closes: leaving the connection open would mean guessing where the next
    /// request starts, which is how request smuggling works.
    Invalid { status: u16, message: &'static str },
}

fn invalid(status: u16, message: &'static str) -> io::Result<ReadOutcome> {
    Ok(ReadOutcome::Invalid { status, message })
}

/// Read one request.
pub async fn read_request<R>(reader: &mut R, max_body: usize) -> io::Result<ReadOutcome>
where
    R: AsyncBufRead + Unpin,
{
    let mut request_line = match read_line(reader).await? {
        Line::Eof => return Ok(ReadOutcome::Closed),
        Line::TooLong => return invalid(400, "request line too long"),
        Line::Read(line) => line,
    };
    // tolerate a stray CRLF left over from a previous request
    if request_line.trim().is_empty() {
        request_line = match read_line(reader).await? {
            Line::Eof => return Ok(ReadOutcome::Closed),
            Line::TooLong => return invalid(400, "request line too long"),
            Line::Read(line) => line,
        };
    }

    let parts: Vec<&str> = request_line.trim_end().split(' ').collect();
    let (method, target, version) = match parts.as_slice() {
        [method, target, version] => (*method, *target, *version),
        [method, target] => (*method, *target, "HTTP/1.1"),
        _ => return invalid(400, "malformed request line"),
    };
    if method.is_empty()
        || target.is_empty()
        || !method.bytes().all(|b| b.is_ascii_alphabetic())
        || !version.starts_with("HTTP/")
    {
        return invalid(400, "malformed request line");
    }
    let (method, target, version) = (method.to_string(), target.to_string(), version.to_string());

    let mut headers: Vec<(String, String)> = Vec::new();
    loop {
        if headers.len() >= MAX_HEADERS {
            return invalid(400, "too many request headers");
        }
        let line = match read_line(reader).await? {
            Line::Eof => return Ok(ReadOutcome::Closed),
            Line::TooLong => return invalid(400, "request header too long"),
            Line::Read(line) => line,
        };
        let line = line.trim_end_matches(['\r', '\n']);
        if line.is_empty() {
            break;
        }
        let Some((name, value)) = line.split_once(':') else {
            return invalid(400, "malformed request header");
        };
        // an obs-fold continuation line, or a space before the colon
        if name.is_empty() || name.chars().any(char::is_whitespace) {
            return invalid(400, "malformed request header");
        }
        headers.push((name.to_ascii_lowercase(), value.trim().to_string()));
    }

    // No framing but Content-Length is supported, and a request that claims
    // another one must be refused rather than framed by guesswork.
    if headers.iter().any(|(name, _)| name == "transfer-encoding") {
        return invalid(501, "transfer-encoding is not supported");
    }
    let content_length = match content_length(&headers) {
        Ok(length) => length,
        Err(message) => return invalid(400, message),
    };
    if content_length > max_body {
        return invalid(400, "request body too large");
    }

    // grown while reading, never pre-allocated from the claimed length
    let mut body = Vec::new();
    if content_length > 0 {
        let read = reader
            .take(content_length as u64)
            .read_to_end(&mut body)
            .await?;
        if read != content_length {
            return Ok(ReadOutcome::Closed);
        }
    }

    let keep_alive = match connection_header(&headers) {
        Some(value) if value.contains("close") => false,
        Some(value) if value.contains("keep-alive") => true,
        _ => version != "HTTP/1.0",
    };

    let (raw_path, raw_query) = match target.split_once('?') {
        Some((path, query)) => (path, Some(query)),
        None => (target.as_str(), None),
    };

    Ok(ReadOutcome::Request(Box::new(HttpRequest {
        method,
        path: percent_decode(raw_path),
        raw_target: raw_path.to_string(),
        query: raw_query.map(parse_query).unwrap_or_default(),
        body,
        keep_alive,
    })))
}

/// The body length, refusing everything ambiguous: an unparseable or negative
/// value, a list whose members disagree, or two headers that disagree.
fn content_length(headers: &[(String, String)]) -> Result<usize, &'static str> {
    let mut length: Option<usize> = None;
    for (_, raw) in headers.iter().filter(|(name, _)| name == "content-length") {
        for field in raw.split(',') {
            let field = field.trim();
            let Ok(value) = field.parse::<usize>() else {
                return Err("invalid Content-Length");
            };
            // a leading sign or padding parses nowhere else, but be explicit
            if !field.bytes().all(|b| b.is_ascii_digit()) {
                return Err("invalid Content-Length");
            }
            match length {
                Some(seen) if seen != value => return Err("conflicting Content-Length"),
                _ => length = Some(value),
            }
        }
    }
    Ok(length.unwrap_or(0))
}

fn connection_header(headers: &[(String, String)]) -> Option<String> {
    headers
        .iter()
        .find(|(name, _)| name == "connection")
        .map(|(_, value)| value.to_ascii_lowercase())
}

enum Line {
    Read(String),
    TooLong,
    Eof,
}

async fn read_line<R>(reader: &mut R) -> io::Result<Line>
where
    R: AsyncBufRead + Unpin,
{
    let mut buf = Vec::new();
    let read = reader
        .take(MAX_LINE as u64)
        .read_until(b'\n', &mut buf)
        .await?;
    if read == 0 {
        return Ok(Line::Eof);
    }
    if !buf.ends_with(b"\n") {
        return Ok(Line::TooLong);
    }
    Ok(Line::Read(String::from_utf8_lossy(&buf).into_owned()))
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
    /// Relayed verbatim (the Liquid asset proxy passes some upstream headers on).
    pub extra_headers: Vec<(&'static str, String)>,
}

impl HttpResponse {
    pub fn text(status: u16, body: impl Into<String>, cache_max_age: Option<u32>) -> Self {
        Self {
            status,
            content_type: "text/plain",
            body: body.into().into_bytes(),
            cache_max_age,
            extra_headers: Vec::new(),
        }
    }

    pub fn json(body: Vec<u8>, cache_max_age: Option<u32>) -> Self {
        Self {
            status: 200,
            content_type: "application/json",
            body,
            cache_max_age,
            extra_headers: Vec::new(),
        }
    }

    pub fn binary(body: Vec<u8>, cache_max_age: Option<u32>) -> Self {
        Self {
            status: 200,
            content_type: "application/octet-stream",
            body,
            cache_max_age,
            extra_headers: Vec::new(),
        }
    }
}

fn reason(status: u16) -> &'static str {
    match status {
        200 => "OK",
        400 => "Bad Request",
        404 => "Not Found",
        408 => "Request Timeout",
        422 => "Unprocessable Entity",
        500 => "Internal Server Error",
        501 => "Not Implemented",
        503 => "Service Unavailable",
        504 => "Gateway Timeout",
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
    for (name, value) in &response.extra_headers {
        // an upstream value is relayed, never allowed to start a new header
        if !value.contains(['\r', '\n']) {
            head.push_str(&format!("{name}: {value}\r\n"));
        }
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

    async fn read(raw: &str) -> ReadOutcome {
        let mut reader = BufReader::new(raw.as_bytes());
        read_request(&mut reader, MAX_BODY).await.unwrap()
    }

    async fn parse(raw: &str) -> HttpRequest {
        match read(raw).await {
            ReadOutcome::Request(request) => *request,
            other => panic!("expected a request, got {other:?}"),
        }
    }

    async fn rejected(raw: &str) -> (u16, &'static str) {
        match read(raw).await {
            ReadOutcome::Invalid { status, message } => (status, message),
            other => panic!("expected a rejection, got {other:?}"),
        }
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
        assert_eq!(read("").await, ReadOutcome::Closed);
    }

    #[tokio::test]
    async fn malformed_request_lines_are_rejected() {
        assert_eq!(rejected("nonsense\r\n\r\n").await.0, 400);
        assert_eq!(rejected("GET\r\n\r\n").await.0, 400);
        assert_eq!(rejected("GET / SPDY/1.0\r\n\r\n").await.0, 400);
        assert_eq!(rejected("G3T / HTTP/1.1\r\n\r\n").await.0, 400);
    }

    #[tokio::test]
    async fn an_unparseable_content_length_is_rejected() {
        // would otherwise be read as zero, leaving the body to be framed as the
        // next pipelined request
        for header in ["x2", "-5", "+4", "0x4", "", "4a"] {
            let raw = format!("POST /tx HTTP/1.1\r\nContent-Length: {header}\r\n\r\nGET / HTTP/1.1\r\n\r\n");
            assert_eq!(rejected(&raw).await, (400, "invalid Content-Length"), "{header:?}");
        }
    }

    #[tokio::test]
    async fn conflicting_content_lengths_are_rejected() {
        let listed = "POST /tx HTTP/1.1\r\nContent-Length: 5, 6\r\n\r\nhello";
        assert_eq!(rejected(listed).await, (400, "conflicting Content-Length"));
        let duplicated =
            "POST /tx HTTP/1.1\r\nContent-Length: 5\r\nContent-Length: 6\r\n\r\nhello";
        assert_eq!(rejected(duplicated).await, (400, "conflicting Content-Length"));
    }

    #[tokio::test]
    async fn repeated_identical_content_lengths_are_accepted() {
        let request = parse("POST /tx HTTP/1.1\r\nContent-Length: 5, 5\r\n\r\nhello").await;
        assert_eq!(request.body, b"hello");
        let request =
            parse("POST /tx HTTP/1.1\r\nContent-Length: 5\r\nContent-Length: 5\r\n\r\nhello").await;
        assert_eq!(request.body, b"hello");
    }

    #[tokio::test]
    async fn transfer_encoding_is_refused_outright() {
        let chunked = "POST /tx HTTP/1.1\r\nTransfer-Encoding: chunked\r\n\r\n0\r\n\r\n";
        assert_eq!(rejected(chunked).await, (501, "transfer-encoding is not supported"));
        let smuggled =
            "POST /tx HTTP/1.1\r\nContent-Length: 6\r\nTransfer-Encoding: identity\r\n\r\nhello!";
        assert_eq!(rejected(smuggled).await.0, 501);
    }

    #[tokio::test]
    async fn a_body_over_the_cap_is_rejected() {
        let mut reader = BufReader::new(&b"POST /tx HTTP/1.1\r\nContent-Length: 11\r\n\r\nhello world"[..]);
        let outcome = read_request(&mut reader, 10).await.unwrap();
        assert!(
            matches!(outcome, ReadOutcome::Invalid { status: 400, message } if message == "request body too large"),
            "{outcome:?}"
        );
    }

    #[tokio::test]
    async fn a_truncated_body_closes_instead_of_reserving_it() {
        // the claimed length is never pre-allocated, so a client can declare a
        // huge body and send nothing without costing the server that memory
        let raw = "POST /tx HTTP/1.1\r\nContent-Length: 10000000\r\n\r\nabc";
        assert_eq!(read(raw).await, ReadOutcome::Closed);
    }

    #[tokio::test]
    async fn too_many_headers_are_rejected() {
        let mut raw = String::from("GET / HTTP/1.1\r\n");
        for i in 0..MAX_HEADERS + 1 {
            raw.push_str(&format!("X-Pad-{i}: 1\r\n"));
        }
        raw.push_str("\r\n");
        assert_eq!(rejected(&raw).await, (400, "too many request headers"));
    }

    #[tokio::test]
    async fn an_overlong_request_line_is_rejected() {
        let raw = format!("GET /{} HTTP/1.1\r\n\r\n", "a".repeat(MAX_LINE));
        assert_eq!(rejected(&raw).await, (400, "request line too long"));
    }

    #[tokio::test]
    async fn a_malformed_header_is_rejected() {
        assert_eq!(
            rejected("GET / HTTP/1.1\r\nnot a header\r\n\r\n").await,
            (400, "malformed request header")
        );
        assert_eq!(
            rejected("GET / HTTP/1.1\r\nContent-Length : 0\r\n\r\n").await,
            (400, "malformed request header")
        );
    }

    #[tokio::test]
    async fn pipelined_requests_are_framed_by_content_length() {
        let raw = "POST /tx HTTP/1.1\r\nContent-Length: 5\r\n\r\nhelloGET /blocks/tip/height HTTP/1.1\r\n\r\n";
        let mut reader = BufReader::new(raw.as_bytes());
        let first = read_request(&mut reader, MAX_BODY).await.unwrap();
        let ReadOutcome::Request(first) = first else {
            panic!("expected a request, got {first:?}");
        };
        assert_eq!(first.body, b"hello");
        let second = read_request(&mut reader, MAX_BODY).await.unwrap();
        let ReadOutcome::Request(second) = second else {
            panic!("expected a request, got {second:?}");
        };
        assert_eq!(second.path, "/blocks/tip/height");
        assert!(second.body.is_empty());
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

    #[tokio::test]
    async fn rejection_statuses_have_reason_phrases() {
        for status in [400, 408, 501, 503, 504] {
            assert_ne!(reason(status), "Unknown", "{status}");
        }
    }
}

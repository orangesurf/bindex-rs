use std::{
    net::SocketAddr,
    sync::atomic::{AtomicU64, Ordering},
    time::{Duration, SystemTime, UNIX_EPOCH},
};

use tokio::{
    io::{AsyncReadExt as _, AsyncWriteExt as _},
    net::TcpStream,
};

/// mempool.space's v3 onion service.
pub const MEMPOOL_SPACE_ONION: &str =
    "mempoolhqx4isw62xs7abwphsq7ldayuidyx2v2oethdhhj6mlo2r6ad.onion";

/// Overall budget for one push: SOCKS handshake + onion circuit + HTTP round trip.
pub const PUSH_TIMEOUT: Duration = Duration::from_secs(60);

const MAX_ERROR_BODY_CHARS: usize = 512;

#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error("invalid push URL: {0}")]
    Url(String),

    #[error("tor proxy I/O failed: {0}")]
    Io(#[from] std::io::Error),

    #[error("SOCKS5 handshake failed: {0}")]
    Socks(String),

    #[error("push endpoint returned HTTP {status}: {body}")]
    Http { status: u16, body: String },

    #[error("invalid HTTP response from push endpoint: {0}")]
    BadResponse(String),

    #[error("push timed out after {0:?}")]
    Timeout(Duration),
}

/// Push endpoint parsed from an `http://` URL. Onion services encrypt and
/// authenticate the transport themselves, so plain HTTP is the norm and TLS
/// is deliberately unsupported.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PushTarget {
    host: String,
    port: u16,
    path: String,
}

impl PushTarget {
    pub fn parse(url: &str) -> Result<Self, Error> {
        if url.starts_with("https://") {
            return Err(Error::Url(
                "https is not supported; onion push endpoints use plain http".to_string(),
            ));
        }
        let rest = url
            .strip_prefix("http://")
            .ok_or_else(|| Error::Url(format!("{url:?} must start with http://")))?;
        let (authority, path) = match rest.find('/') {
            Some(idx) => (&rest[..idx], &rest[idx..]),
            None => (rest, "/"),
        };
        if authority.contains('@') {
            return Err(Error::Url("userinfo is not supported".to_string()));
        }
        let (host, port) = match authority.rsplit_once(':') {
            Some((host, port)) => (
                host,
                port.parse::<u16>()
                    .map_err(|_| Error::Url(format!("invalid port {port:?}")))?,
            ),
            None => (authority, 80),
        };
        if host.is_empty() {
            return Err(Error::Url("missing host".to_string()));
        }
        // SOCKS5 domain names are length-prefixed with a single byte.
        if host.len() > 255 {
            return Err(Error::Url("host exceeds 255 bytes".to_string()));
        }
        Ok(Self {
            host: host.to_string(),
            port,
            path: path.to_string(),
        })
    }

    pub fn url(&self) -> String {
        format!("http://{}:{}{}", self.host, self.port, self.path)
    }
}

/// POST a raw transaction (hex) to the push endpoint through a SOCKS5 proxy.
/// The hostname is passed to the proxy for remote resolution (required for
/// `.onion`). Returns the response body, which mempool.space's `POST /api/tx`
/// sets to the txid.
pub async fn push_tx(proxy: SocketAddr, target: &PushTarget, tx_hex: &str) -> Result<String, Error> {
    tokio::time::timeout(PUSH_TIMEOUT, push_tx_inner(proxy, target, tx_hex))
        .await
        .map_err(|_| Error::Timeout(PUSH_TIMEOUT))?
}

async fn push_tx_inner(
    proxy: SocketAddr,
    target: &PushTarget,
    tx_hex: &str,
) -> Result<String, Error> {
    let mut stream = TcpStream::connect(proxy).await?;
    socks5_connect(&mut stream, &target.host, target.port).await?;

    let request = format!(
        "POST {} HTTP/1.1\r\n\
         Host: {}\r\n\
         User-Agent: bindex-electrum/{}\r\n\
         Content-Type: text/plain\r\n\
         Content-Length: {}\r\n\
         Connection: close\r\n\
         \r\n",
        target.path,
        target.host,
        env!("CARGO_PKG_VERSION"),
        tx_hex.len(),
    );
    stream.write_all(request.as_bytes()).await?;
    stream.write_all(tx_hex.as_bytes()).await?;

    let mut raw = Vec::new();
    stream.read_to_end(&mut raw).await?;
    let (status, body) = parse_http_response(&raw)?;
    if !(200..300).contains(&status) {
        let body = body.trim().chars().take(MAX_ERROR_BODY_CHARS).collect();
        return Err(Error::Http { status, body });
    }
    Ok(body.trim().to_string())
}

/// Fresh SOCKS5 credentials for one push. Tor never checks them; it uses them
/// as an isolation key: `IsolateSOCKSAuth` (on by default) refuses to put
/// streams carrying different credentials on the same circuit, so a
/// never-repeated pair forces a fresh circuit per broadcast. Successive pushes
/// therefore cannot be linked by sharing a rendezvous circuit, and
/// `MaxCircuitDirtiness` no longer matters. Only uniqueness matters — the pair
/// travels no further than the loopback link to the local tor — so a
/// pid + timestamp + counter is enough; no entropy source is needed.
fn isolation_credentials() -> (String, String) {
    static COUNTER: AtomicU64 = AtomicU64::new(0);
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    let seq = COUNTER.fetch_add(1, Ordering::Relaxed);
    (
        format!("bindex-{}-{nanos:x}-{seq}", std::process::id()),
        "isolate".to_string(),
    )
}

async fn socks5_connect(stream: &mut TcpStream, host: &str, port: u16) -> Result<(), Error> {
    // Greeting: version 5, exactly one auth method — username/password
    // (RFC 1929). "No auth" is deliberately not offered as a fallback: a proxy
    // that cannot take credentials cannot isolate circuits either, and a
    // silent downgrade would put every broadcast back on one shared circuit.
    stream.write_all(&[0x05, 0x01, 0x02]).await?;
    let mut reply = [0u8; 2];
    stream.read_exact(&mut reply).await?;
    if reply != [0x05, 0x02] {
        return Err(Error::Socks(format!(
            "proxy rejected username/password auth (required for per-push circuit isolation): {reply:02x?}"
        )));
    }

    // RFC 1929 sub-negotiation with this push's unique credentials.
    let (user, pass) = isolation_credentials();
    let mut auth = Vec::with_capacity(3 + user.len() + pass.len());
    auth.push(0x01);
    auth.push(user.len() as u8);
    auth.extend_from_slice(user.as_bytes());
    auth.push(pass.len() as u8);
    auth.extend_from_slice(pass.as_bytes());
    stream.write_all(&auth).await?;
    let mut status = [0u8; 2];
    stream.read_exact(&mut status).await?;
    if status[1] != 0x00 {
        return Err(Error::Socks(format!(
            "proxy refused isolation credentials: status {:#04x}",
            status[1]
        )));
    }

    // CONNECT with the hostname as a domain (ATYP 3): the proxy resolves it.
    let mut request = Vec::with_capacity(7 + host.len());
    request.extend_from_slice(&[0x05, 0x01, 0x00, 0x03, host.len() as u8]);
    request.extend_from_slice(host.as_bytes());
    request.extend_from_slice(&port.to_be_bytes());
    stream.write_all(&request).await?;

    let mut header = [0u8; 4];
    stream.read_exact(&mut header).await?;
    if header[0] != 0x05 {
        return Err(Error::Socks(format!("bad reply version {}", header[0])));
    }
    if header[1] != 0x00 {
        return Err(Error::Socks(socks_reply_message(header[1]).to_string()));
    }
    // Consume BND.ADDR + BND.PORT.
    let addr_len = match header[3] {
        0x01 => 4,
        0x04 => 16,
        0x03 => {
            let mut len = [0u8; 1];
            stream.read_exact(&mut len).await?;
            len[0] as usize
        }
        other => return Err(Error::Socks(format!("bad bind address type {other}"))),
    };
    let mut bind = vec![0u8; addr_len + 2];
    stream.read_exact(&mut bind).await?;
    Ok(())
}

fn socks_reply_message(code: u8) -> &'static str {
    match code {
        0x01 => "general SOCKS server failure",
        0x02 => "connection not allowed by ruleset",
        0x03 => "network unreachable",
        0x04 => "host unreachable",
        0x05 => "connection refused",
        0x06 => "TTL expired",
        0x07 => "command not supported",
        0x08 => "address type not supported",
        _ => "unknown SOCKS error",
    }
}

fn parse_http_response(raw: &[u8]) -> Result<(u16, String), Error> {
    let header_end = raw
        .windows(4)
        .position(|window| window == b"\r\n\r\n")
        .ok_or_else(|| Error::BadResponse("missing header terminator".to_string()))?;
    let head = std::str::from_utf8(&raw[..header_end])
        .map_err(|_| Error::BadResponse("headers are not valid UTF-8".to_string()))?;
    let mut lines = head.split("\r\n");
    let status_line = lines.next().unwrap_or_default();
    let status = status_line
        .split_whitespace()
        .nth(1)
        .and_then(|code| code.parse::<u16>().ok())
        .ok_or_else(|| Error::BadResponse(format!("invalid status line {status_line:?}")))?;

    let mut chunked = false;
    let mut content_length = None;
    for line in lines {
        let Some((name, value)) = line.split_once(':') else {
            continue;
        };
        match name.trim().to_ascii_lowercase().as_str() {
            "transfer-encoding" => chunked = value.to_ascii_lowercase().contains("chunked"),
            "content-length" => content_length = value.trim().parse::<usize>().ok(),
            _ => {}
        }
    }

    let body = &raw[header_end + 4..];
    let body = if chunked {
        decode_chunked(body)?
    } else if let Some(length) = content_length {
        body.get(..length)
            .ok_or_else(|| Error::BadResponse("truncated body".to_string()))?
            .to_vec()
    } else {
        body.to_vec()
    };
    Ok((status, String::from_utf8_lossy(&body).into_owned()))
}

fn decode_chunked(mut body: &[u8]) -> Result<Vec<u8>, Error> {
    let mut decoded = Vec::new();
    loop {
        let line_end = body
            .windows(2)
            .position(|window| window == b"\r\n")
            .ok_or_else(|| Error::BadResponse("missing chunk size".to_string()))?;
        let size_line = std::str::from_utf8(&body[..line_end])
            .map_err(|_| Error::BadResponse("invalid chunk size".to_string()))?;
        let size_str = size_line.split(';').next().unwrap_or_default().trim();
        let size = usize::from_str_radix(size_str, 16)
            .map_err(|_| Error::BadResponse(format!("invalid chunk size {size_str:?}")))?;
        body = &body[line_end + 2..];
        if size == 0 {
            return Ok(decoded); // trailers, if any, are ignored
        }
        if body.len() < size + 2 {
            return Err(Error::BadResponse("truncated chunk".to_string()));
        }
        decoded.extend_from_slice(&body[..size]);
        body = &body[size + 2..];
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::net::TcpListener;

    #[test]
    fn parses_push_urls() {
        let target = PushTarget::parse(&format!("http://{MEMPOOL_SPACE_ONION}/api/tx")).unwrap();
        assert_eq!(target.host, MEMPOOL_SPACE_ONION);
        assert_eq!(target.port, 80);
        assert_eq!(target.path, "/api/tx");

        let target = PushTarget::parse("http://127.0.0.1:8999/testnet/api/tx").unwrap();
        assert_eq!(target.host, "127.0.0.1");
        assert_eq!(target.port, 8999);
        assert_eq!(target.path, "/testnet/api/tx");

        assert_eq!(PushTarget::parse("http://host").unwrap().path, "/");
        assert!(PushTarget::parse("https://mempool.space/api/tx").is_err());
        assert!(PushTarget::parse("host/api/tx").is_err());
        assert!(PushTarget::parse("http://:80/api/tx").is_err());
        assert!(PushTarget::parse("http://user@host/api/tx").is_err());
        assert!(PushTarget::parse("http://host:notaport/").is_err());
    }

    #[test]
    fn parses_http_responses() {
        let (status, body) =
            parse_http_response(b"HTTP/1.1 200 OK\r\nContent-Length: 4\r\n\r\nabcdEXTRA").unwrap();
        assert_eq!(status, 200);
        assert_eq!(body, "abcd");

        let (status, body) = parse_http_response(
            b"HTTP/1.1 200 OK\r\nTransfer-Encoding: chunked\r\n\r\n4\r\nabcd\r\n2\r\nef\r\n0\r\n\r\n",
        )
        .unwrap();
        assert_eq!(status, 200);
        assert_eq!(body, "abcdef");

        let (status, body) =
            parse_http_response(b"HTTP/1.1 400 Bad Request\r\n\r\nrejected").unwrap();
        assert_eq!(status, 400);
        assert_eq!(body, "rejected");

        assert!(parse_http_response(b"HTTP/1.1 200 OK\r\nContent-Length: 9\r\n\r\nabcd").is_err());
        assert!(parse_http_response(b"garbage").is_err());
    }

    /// Serve method selection + the RFC 1929 sub-negotiation the way tor does
    /// and return the username the client presented.
    async fn mock_socks_auth(stream: &mut tokio::net::TcpStream) -> String {
        let mut greeting = [0u8; 2];
        stream.read_exact(&mut greeting).await.unwrap();
        assert_eq!(greeting[0], 5);
        let mut methods = vec![0u8; greeting[1] as usize];
        stream.read_exact(&mut methods).await.unwrap();
        assert_eq!(methods, vec![2], "client must offer only username/password");
        stream.write_all(&[5, 2]).await.unwrap();

        let mut head = [0u8; 2];
        stream.read_exact(&mut head).await.unwrap();
        assert_eq!(head[0], 1);
        let mut user = vec![0u8; head[1] as usize];
        stream.read_exact(&mut user).await.unwrap();
        let mut plen = [0u8; 1];
        stream.read_exact(&mut plen).await.unwrap();
        let mut pass = vec![0u8; plen[0] as usize];
        stream.read_exact(&mut pass).await.unwrap();
        assert!(!user.is_empty() && !pass.is_empty());
        stream.write_all(&[1, 0]).await.unwrap();
        String::from_utf8(user).unwrap()
    }

    /// Accept one connection, act as a SOCKS5 proxy + HTTP origin, and return
    /// (requested host, requested port, full HTTP request, SOCKS username).
    async fn run_mock_proxy(
        listener: TcpListener,
        response: String,
    ) -> (String, u16, String, String) {
        let (mut stream, _) = listener.accept().await.unwrap();
        let user = mock_socks_auth(&mut stream).await;

        let mut header = [0u8; 4];
        stream.read_exact(&mut header).await.unwrap();
        assert_eq!(header, [5, 1, 0, 3]);
        let mut len = [0u8; 1];
        stream.read_exact(&mut len).await.unwrap();
        let mut host = vec![0u8; len[0] as usize];
        stream.read_exact(&mut host).await.unwrap();
        let mut port = [0u8; 2];
        stream.read_exact(&mut port).await.unwrap();
        stream
            .write_all(&[5, 0, 0, 1, 0, 0, 0, 0, 0, 0])
            .await
            .unwrap();

        let mut request = Vec::new();
        loop {
            let mut buf = [0u8; 1024];
            let n = stream.read(&mut buf).await.unwrap();
            request.extend_from_slice(&buf[..n]);
            if let Some(pos) = request.windows(4).position(|window| window == b"\r\n\r\n") {
                let head = String::from_utf8_lossy(&request[..pos]);
                let content_length = head
                    .split("\r\n")
                    .find_map(|line| line.to_ascii_lowercase().strip_prefix("content-length:")
                        .and_then(|value| value.trim().parse::<usize>().ok()))
                    .unwrap_or(0);
                if request.len() >= pos + 4 + content_length {
                    break;
                }
            }
            if n == 0 {
                break;
            }
        }
        stream.write_all(response.as_bytes()).await.unwrap();
        (
            String::from_utf8(host).unwrap(),
            u16::from_be_bytes(port),
            String::from_utf8_lossy(&request).into_owned(),
            user,
        )
    }

    #[tokio::test]
    async fn pushes_tx_through_socks5_proxy() {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let proxy = listener.local_addr().unwrap();
        let txid = "aa".repeat(32);
        let response = format!("HTTP/1.1 200 OK\r\nContent-Length: 64\r\n\r\n{txid}");
        let mock = tokio::spawn(run_mock_proxy(listener, response));

        let target = PushTarget::parse("http://push.example.onion/api/tx").unwrap();
        let result = push_tx(proxy, &target, "deadbeef").await.unwrap();
        assert_eq!(result, txid);

        let (host, port, request, user) = mock.await.unwrap();
        assert!(user.starts_with("bindex-"), "unexpected SOCKS username {user:?}");
        assert_eq!(host, "push.example.onion");
        assert_eq!(port, 80);
        assert!(request.starts_with("POST /api/tx HTTP/1.1\r\n"));
        assert!(request.contains("\r\nHost: push.example.onion\r\n"));
        assert!(request.contains("\r\nContent-Length: 8\r\n"));
        assert!(request.ends_with("\r\n\r\ndeadbeef"));
    }

    #[tokio::test]
    async fn surfaces_push_rejection() {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let proxy = listener.local_addr().unwrap();
        let body = "sendrawtransaction RPC error: {\"code\":-25,\"message\":\"bad-txns-inputs-missingorspent\"}";
        let response = format!(
            "HTTP/1.1 400 Bad Request\r\nContent-Length: {}\r\n\r\n{body}",
            body.len()
        );
        let mock = tokio::spawn(run_mock_proxy(listener, response));

        let target = PushTarget::parse("http://push.example.onion/api/tx").unwrap();
        let err = push_tx(proxy, &target, "deadbeef").await.unwrap_err();
        match err {
            Error::Http { status, body } => {
                assert_eq!(status, 400);
                assert!(body.contains("bad-txns-inputs-missingorspent"));
            }
            other => panic!("expected HTTP error, got {other:?}"),
        }
        mock.await.unwrap();
    }

    #[tokio::test]
    async fn surfaces_socks_connect_failure() {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let proxy = listener.local_addr().unwrap();
        let mock = tokio::spawn(async move {
            let (mut stream, _) = listener.accept().await.unwrap();
            mock_socks_auth(&mut stream).await;
            let mut request = vec![0u8; 4 + 1 + "push.example.onion".len() + 2];
            stream.read_exact(&mut request).await.unwrap();
            // Reply: connection refused.
            stream
                .write_all(&[5, 5, 0, 1, 0, 0, 0, 0, 0, 0])
                .await
                .unwrap();
        });

        let target = PushTarget::parse("http://push.example.onion/api/tx").unwrap();
        let err = push_tx(proxy, &target, "deadbeef").await.unwrap_err();
        assert!(
            matches!(&err, Error::Socks(message) if message.contains("connection refused")),
            "unexpected error: {err:?}"
        );
        mock.await.unwrap();
    }

    #[tokio::test]
    async fn each_push_presents_fresh_isolation_credentials() {
        let mut users = Vec::new();
        for _ in 0..3 {
            let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
            let proxy = listener.local_addr().unwrap();
            let response = "HTTP/1.1 200 OK\r\nContent-Length: 2\r\n\r\nok".to_string();
            let mock = tokio::spawn(run_mock_proxy(listener, response));
            let target = PushTarget::parse("http://push.example.onion/api/tx").unwrap();
            push_tx(proxy, &target, "deadbeef").await.unwrap();
            let (_, _, _, user) = mock.await.unwrap();
            users.push(user);
        }
        let distinct: std::collections::HashSet<_> = users.iter().collect();
        assert_eq!(distinct.len(), users.len(), "credentials repeated: {users:?}");
    }

    #[tokio::test]
    async fn refuses_proxy_that_cannot_isolate() {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let proxy = listener.local_addr().unwrap();
        let mock = tokio::spawn(async move {
            let (mut stream, _) = listener.accept().await.unwrap();
            let mut greeting = [0u8; 2];
            stream.read_exact(&mut greeting).await.unwrap();
            let mut methods = vec![0u8; greeting[1] as usize];
            stream.read_exact(&mut methods).await.unwrap();
            // A proxy that only speaks "no auth" picks method 0 — or 0xff if
            // it cannot honour any offered method. Both must be fatal.
            stream.write_all(&[5, 0]).await.unwrap();
        });

        let target = PushTarget::parse("http://push.example.onion/api/tx").unwrap();
        let err = push_tx(proxy, &target, "deadbeef").await.unwrap_err();
        assert!(
            matches!(&err, Error::Socks(message) if message.contains("isolation")),
            "unexpected error: {err:?}"
        );
        mock.await.unwrap();
    }
}

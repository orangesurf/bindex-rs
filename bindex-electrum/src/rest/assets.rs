//! `/asset*` and `/assets*` (Liquid only).
//!
//! bindex has no asset index: issuance history, supply and the registry all
//! need one. Until it exists these routes are forwarded to an upstream Esplora
//! (`--asset-upstream`, e.g. liquid.network), which is what the Python shim
//! this replaces did. Status, body, `Content-Type`, `Cache-Control` and the
//! registry's `X-Total-Results` pass through; with no upstream configured the
//! routes answer 404 like an unknown asset.

use std::{sync::OnceLock, time::Duration};

use crate::rest::{
    http::HttpResponse,
    HttpError, Req, RestApi, Result,
};

/// How long one upstream request may take.
const UPSTREAM_TIMEOUT: Duration = Duration::from_secs(20);
/// How long one connect attempt may take.
const CONNECT_TIMEOUT: Duration = Duration::from_secs(3);
/// Largest upstream body relayed (the registry list is paged at 100 entries).
const UPSTREAM_BODY_LIMIT: u64 = 16 << 20;

fn agent() -> &'static ureq::Agent {
    static AGENT: OnceLock<ureq::Agent> = OnceLock::new();
    AGENT.get_or_init(|| {
        // IPv4 only: ureq tries the resolved addresses one after another, IPv6
        // first, with no happy eyeballs. Where IPv6 connects hang instead of
        // failing, that cost ~6 s per address, ~30 s before the first IPv4 try.
        ureq::Agent::config_builder()
            .ip_family(ureq::config::IpFamily::Ipv4Only)
            .timeout_connect(Some(CONNECT_TIMEOUT))
            .timeout_global(Some(UPSTREAM_TIMEOUT))
            .http_status_as_error(false)
            .user_agent(format!("bindex-electrum/{}", env!("CARGO_PKG_VERSION")))
            .build()
            .into()
    })
}

pub async fn proxy(api: &RestApi, request: &Req<'_>) -> Result<HttpResponse> {
    let Some(upstream) = api.config.asset_upstream.as_deref() else {
        return Err(HttpError::not_found("Asset id not found"));
    };
    let url = upstream_url(upstream, &request.segments(), &request.query)?;
    tokio::task::spawn_blocking(move || fetch(&url))
        .await
        .map_err(|err| HttpError::server_error(format!("asset proxy task: {err}")))?
}

/// Rebuilt from the parsed path and query rather than copied from the raw
/// target, so only plain path segments reach the upstream.
fn upstream_url(
    upstream: &str,
    segments: &[&str],
    query: &std::collections::HashMap<String, String>,
) -> Result<String> {
    let plain = |s: &str| {
        !s.is_empty() && s.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'_' || b == b'-')
    };
    if !segments.iter().all(|s| plain(s)) {
        return Err(HttpError::bad_request("Invalid asset path"));
    }
    let mut url = format!("{}/{}", upstream.trim_end_matches('/'), segments.join("/"));
    let mut pairs: Vec<(&String, &String)> = query.iter().collect();
    pairs.sort();
    for (index, (key, value)) in pairs.into_iter().enumerate() {
        url.push(if index == 0 { '?' } else { '&' });
        url.push_str(&percent_encode(key));
        url.push('=');
        url.push_str(&percent_encode(value));
    }
    Ok(url)
}

fn percent_encode(value: &str) -> String {
    let mut out = String::with_capacity(value.len());
    for byte in value.bytes() {
        if byte.is_ascii_alphanumeric() || b"-_.~".contains(&byte) {
            out.push(byte as char);
        } else {
            out.push_str(&format!("%{byte:02X}"));
        }
    }
    out
}

fn fetch(url: &str) -> Result<HttpResponse> {
    let mut response = agent().get(url).call().map_err(|err| {
        log::warn!("asset upstream {url}: {err}");
        HttpError::new(503, "asset upstream unavailable")
    })?;
    let status = response.status().as_u16();
    let header = |name: &str| {
        response
            .headers()
            .get(name)
            .and_then(|value| value.to_str().ok())
            .map(ToString::to_string)
    };
    let content_type = match header("content-type") {
        Some(ct) if ct.starts_with("application/json") => "application/json",
        Some(ct) if ct.starts_with("application/octet-stream") => "application/octet-stream",
        _ => "text/plain",
    };
    let mut extra_headers = Vec::new();
    if let Some(total) = header("x-total-results") {
        extra_headers.push(("X-Total-Results", total));
    }
    if let Some(cache) = header("cache-control") {
        extra_headers.push(("Cache-Control", cache));
    }
    let body = response
        .body_mut()
        .with_config()
        .limit(UPSTREAM_BODY_LIMIT)
        .read_to_vec()
        .map_err(|err| HttpError::new(503, format!("asset upstream body: {err}")))?;
    Ok(HttpResponse {
        status,
        content_type,
        body,
        cache_max_age: None,
        extra_headers,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;

    #[test]
    fn upstream_url_keeps_only_plain_segments() {
        let query = HashMap::from([
            ("q".to_string(), "tether usd".to_string()),
            ("limit".to_string(), "5".to_string()),
        ]);
        assert_eq!(
            upstream_url("https://liquid.network/api/", &["assets", "registry", "search"], &query)
                .unwrap(),
            "https://liquid.network/api/assets/registry/search?limit=5&q=tether%20usd"
        );
        assert!(upstream_url("https://x", &["asset", ".."], &HashMap::new()).is_err());
    }
}

//! HTTP custom-tool execution.
//!
//! The target is resolved and classified before the request is sent. When the
//! URL names a DNS host, the checked addresses are pinned into the client so
//! the connection cannot be re-resolved into a private address between the
//! check and the connect (DNS rebinding). Redirects are disabled entirely, so
//! a fixed `Authorization` header can never follow a 3xx to another origin.

use std::net::{IpAddr, SocketAddr};
use std::time::Duration;

use anyhow::{Result, anyhow, bail};
use reqwest::header::{CONTENT_TYPE, HeaderMap};
use reqwest::{Method, StatusCode};
use serde_json::{Map, Value};
use url::{Host, Url};

use crate::config::HttpExec;
use crate::util::object_input;

/// The headers returned to the model are bounded so a chatty server cannot
/// balloon the tool result.
const MAX_RESULT_HEADERS: usize = 16;
const MAX_RESULT_HEADER_CHARS: usize = 512;

pub(crate) async fn execute(tool: &str, exec: &HttpExec, input: &Value) -> Result<Value> {
    let object = object_input(tool, input)?;
    let method = exec.method.clone();
    let is_query = method == Method::GET || method == Method::DELETE;
    let mut url = exec.url.clone();
    if is_query {
        append_query(&mut url, object);
    }
    normalize_host(&mut url);

    // The configured timeout covers name resolution plus request/response.
    let deadline = tokio::time::Instant::now() + exec.timeout;
    let pin =
        match tokio::time::timeout_at(deadline, resolve_target(tool, &url, exec.allow_private))
            .await
        {
            Ok(result) => result?,
            Err(_) => bail!(
                "custom tool {tool:?} HTTP request timed out after {}s while resolving the target",
                exec.timeout.as_secs()
            ),
        };
    let remaining = deadline
        .saturating_duration_since(tokio::time::Instant::now())
        .max(Duration::from_millis(1));
    let mut builder = reqwest::Client::builder()
        .timeout(remaining)
        // Connect directly so an inherited HTTP proxy cannot resolve the
        // target on our behalf and bypass the address checks or DNS pinning.
        .no_proxy()
        // Never follow a redirect: fixed headers (including credentials) must
        // stay on the configured origin.
        .redirect(reqwest::redirect::Policy::none());
    if let Some((host, addresses)) = pin {
        builder = builder.resolve_to_addrs(&host, &addresses);
    }
    let client = builder
        .build()
        .map_err(|err| anyhow!("custom tool {tool:?} could not build an HTTP client: {err}"))?;

    let mut request = client.request(method, url).headers(exec.headers.clone());
    if !is_query {
        if !exec.headers.contains_key(CONTENT_TYPE) {
            request = request.header(CONTENT_TYPE, "application/json");
        }
        request = request.json(input);
    }

    let response = request.send().await.map_err(|err| {
        if err.is_timeout() {
            anyhow!(
                "custom tool {tool:?} HTTP request timed out after {}s",
                exec.timeout.as_secs()
            )
        } else {
            anyhow!("custom tool {tool:?} HTTP request failed: {err}")
        }
    })?;

    let status = response.status();
    let headers = result_headers(response.headers());
    let (body, truncated) = read_body_capped(tool, response, exec.max_response_bytes).await?;
    Ok(result(status, headers, body, truncated))
}

/// Drops a trailing DNS root dot so the host string used for resolution and
/// the host string reqwest pins are identical.
fn normalize_host(url: &mut Url) {
    let domain = match url.host() {
        Some(Host::Domain(domain)) => domain.to_string(),
        _ => return,
    };
    let trimmed = domain.trim_end_matches('.');
    if trimmed.len() != domain.len() {
        let _ = url.set_host(Some(trimmed));
    }
}

/// GET/DELETE arguments become query parameters. Scalars keep their natural
/// spelling; arrays and objects are serialized as compact JSON.
fn append_query(url: &mut Url, input: &Map<String, Value>) {
    let mut pairs = url.query_pairs_mut();
    for (name, value) in input {
        let text = match value {
            Value::String(text) => text.clone(),
            Value::Null => String::new(),
            other => other.to_string(),
        };
        pairs.append_pair(name, &text);
    }
}

fn result(status: StatusCode, headers: Value, body: String, truncated: bool) -> Value {
    let mut result = Map::new();
    result.insert("status".to_string(), Value::from(status.as_u16()));
    result.insert("headers".to_string(), headers);
    result.insert("body".to_string(), Value::String(body));
    result.insert("truncated".to_string(), Value::Bool(truncated));
    Value::Object(result)
}

fn result_headers(headers: &HeaderMap) -> Value {
    let mut result = Map::new();
    for (name, value) in headers.iter().take(MAX_RESULT_HEADERS) {
        let text = String::from_utf8_lossy(value.as_bytes());
        let text: String = text.chars().take(MAX_RESULT_HEADER_CHARS).collect();
        result.insert(name.as_str().to_string(), Value::String(text));
    }
    Value::Object(result)
}

/// Reads at most `max` body bytes. Stops at the cap instead of draining a
/// hostile stream; the response is dropped at that point.
async fn read_body_capped(
    tool: &str,
    mut response: reqwest::Response,
    max: usize,
) -> Result<(String, bool)> {
    let mut body = Vec::new();
    let mut truncated = false;
    loop {
        let chunk = response.chunk().await.map_err(|err| {
            anyhow!("custom tool {tool:?} HTTP response body could not be read: {err}")
        })?;
        let Some(chunk) = chunk else {
            break;
        };
        if body.len() >= max {
            truncated = true;
            break;
        }
        let remaining = max.saturating_sub(body.len());
        if chunk.len() > remaining {
            body.extend_from_slice(&chunk[..remaining]);
            truncated = true;
            break;
        }
        body.extend_from_slice(&chunk);
    }
    Ok((String::from_utf8_lossy(&body).into_owned(), truncated))
}

/// Resolves the target, rejects private destinations unless opted in, and
/// returns the checked addresses for pinning.
async fn resolve_target(
    tool: &str,
    url: &Url,
    allow_private: bool,
) -> Result<Option<(String, Vec<SocketAddr>)>> {
    match url.host() {
        Some(Host::Ipv4(ip)) => {
            reject_if_private(tool, IpAddr::V4(ip), allow_private)?;
            Ok(None)
        }
        Some(Host::Ipv6(ip)) => {
            reject_if_private(tool, IpAddr::V6(ip), allow_private)?;
            Ok(None)
        }
        Some(Host::Domain(domain)) => {
            let domain = domain.trim_end_matches('.').to_ascii_lowercase();
            if !allow_private && is_localhost_name(&domain) {
                bail!(
                    "custom tool {tool:?}: target {domain:?} is local; \
                     set allow_private: true in the tool config to opt in"
                );
            }
            let port = url
                .port_or_known_default()
                .ok_or_else(|| anyhow!("custom tool {tool:?}: target URL has no usable port"))?;
            let addresses: Vec<SocketAddr> = tokio::net::lookup_host((domain.as_str(), port))
                .await
                .map_err(|err| {
                    anyhow!("custom tool {tool:?} could not resolve target host {domain:?}: {err}")
                })?
                .collect();
            if addresses.is_empty() {
                bail!("custom tool {tool:?}: target host {domain:?} resolved to no addresses");
            }
            if !allow_private {
                for address in &addresses {
                    reject_if_private(tool, address.ip(), false)?;
                }
            }
            Ok(Some((domain, addresses)))
        }
        None => bail!("custom tool {tool:?}: target URL must include a host"),
    }
}

fn reject_if_private(tool: &str, ip: IpAddr, allow_private: bool) -> Result<()> {
    if allow_private || !is_forbidden_ip(ip) {
        return Ok(());
    }
    bail!(
        "custom tool {tool:?}: target {ip} is private, loopback, link-local, or a metadata \
         address; set allow_private: true in the tool config to opt in"
    )
}

fn is_localhost_name(domain: &str) -> bool {
    domain == "localhost"
        || domain.ends_with(".localhost")
        || domain == "metadata.google.internal"
        || domain == "metadata"
}

/// Whether an address is unsafe by default. IPv4-mapped IPv6 is classified as
/// its IPv4 form so `::ffff:127.0.0.1` cannot slip through.
pub(crate) fn is_forbidden_ip(ip: IpAddr) -> bool {
    match ip {
        IpAddr::V4(ip) => {
            let octets = ip.octets();
            ip.is_loopback()
                || ip.is_private()
                || ip.is_link_local()
                || ip.is_unspecified()
                || ip.is_broadcast()
                || ip.is_multicast()
                || octets[0] == 0
                || (octets[0] == 100 && (64..=127).contains(&octets[1]))
        }
        IpAddr::V6(ip) => {
            if let Some(mapped) = ip.to_ipv4_mapped() {
                return is_forbidden_ip(IpAddr::V4(mapped));
            }
            let segments = ip.segments();
            ip.is_loopback()
                || ip.is_unspecified()
                || ip.is_multicast()
                || (segments[0] & 0xfe00) == 0xfc00
                || (segments[0] & 0xffc0) == 0xfe80
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn classifies_private_and_public_addresses() {
        for ip in [
            "127.0.0.1",
            "10.0.0.1",
            "172.16.0.1",
            "192.168.1.1",
            "169.254.169.254",
            "100.64.0.1",
            "0.0.0.0",
            "::1",
            "fc00::1",
            "fe80::1",
            "::ffff:127.0.0.1",
        ] {
            assert!(
                is_forbidden_ip(ip.parse().expect("ip")),
                "{ip} should be forbidden"
            );
        }
        for ip in ["8.8.8.8", "2606:4700:4700::1111"] {
            assert!(
                !is_forbidden_ip(ip.parse().expect("ip")),
                "{ip} should be allowed"
            );
        }
    }

    #[tokio::test]
    async fn rejects_private_targets_without_opt_in() {
        for raw in [
            "http://127.0.0.1/",
            "http://10.0.0.1/",
            "http://169.254.169.254/latest/meta-data",
            "http://[::1]/",
            "http://localhost/",
        ] {
            let url = Url::parse(raw).expect("url");
            let error = resolve_target("probe", &url, false)
                .await
                .expect_err("private target");
            assert!(error.to_string().contains("private"), "{raw}: {error}");
        }
        let url = Url::parse("http://127.0.0.1/").expect("url");
        assert!(resolve_target("probe", &url, true).await.is_ok());
    }

    #[test]
    fn strips_a_trailing_root_dot_from_the_host() {
        let mut url = Url::parse("http://Example.COM./x").expect("url");
        normalize_host(&mut url);
        assert_eq!(url.host_str(), Some("example.com"));
    }

    #[test]
    fn go_style_scalars_become_query_values() {
        let mut url = Url::parse("https://example.com/weather?lang=en").expect("url");
        let mut input = Map::new();
        input.insert("city".to_string(), Value::String("Paris".to_string()));
        input.insert("days".to_string(), Value::from(3));
        input.insert("active".to_string(), Value::Bool(true));
        append_query(&mut url, &input);
        assert_eq!(
            url.as_str(),
            "https://example.com/weather?lang=en&city=Paris&days=3&active=true"
        );
    }
}

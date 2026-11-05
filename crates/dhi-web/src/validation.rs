//! Request validation, SSRF protection, and text normalization.

use std::net::{IpAddr, Ipv4Addr, Ipv6Addr};

use url::{Host, Url};

use crate::Secret;
use crate::error::WebError;
use crate::model::{
    MAX_DOCUMENT_CHARS, MAX_DOMAIN_CHARS, MAX_DOMAINS, MAX_OBJECTIVE_CHARS, MAX_QUERIES,
    MAX_QUERY_CHARS, MAX_QUERY_ITEM_CHARS, MAX_RECENCY_MINUTES, MAX_RESEARCH_QUESTION_CHARS,
    MAX_RESULT_URL_CHARS, MAX_SEARCH_LIMIT, MAX_SNIPPET_CHARS, MAX_TITLE_CHARS, MAX_URL_CHARS,
    SearchRequest,
};

/// Validates a search request before any provider is called.
pub(crate) fn validate_search(request: &SearchRequest) -> Result<(), WebError> {
    let query = request.query.trim();
    if query.is_empty() {
        return Err(WebError::invalid_input(
            "web_search: query must be a non-empty string",
        ));
    }
    if query.chars().count() > MAX_QUERY_CHARS {
        return Err(WebError::invalid_input(format!(
            "web_search: query must be at most {MAX_QUERY_CHARS} characters"
        )));
    }
    if !(1..=MAX_SEARCH_LIMIT).contains(&request.limit) {
        return Err(WebError::invalid_input(format!(
            "web_search: limit must be between 1 and {MAX_SEARCH_LIMIT}; got {}",
            request.limit
        )));
    }
    if let Some(objective) = request.objective.as_deref()
        && objective.chars().count() > MAX_OBJECTIVE_CHARS
    {
        return Err(WebError::invalid_input(format!(
            "web_search: objective must be at most {MAX_OBJECTIVE_CHARS} characters"
        )));
    }
    if request.queries.len() > MAX_QUERIES {
        return Err(WebError::invalid_input(format!(
            "web_search: at most {MAX_QUERIES} keyword queries are allowed"
        )));
    }
    for query in &request.queries {
        if query.chars().count() > MAX_QUERY_ITEM_CHARS {
            return Err(WebError::invalid_input(format!(
                "web_search: each keyword query must be at most {MAX_QUERY_ITEM_CHARS} characters"
            )));
        }
    }
    validate_domains("web_search", "include_domains", &request.include_domains)?;
    validate_domains("web_search", "exclude_domains", &request.exclude_domains)?;
    if let Some(minutes) = request.recency_minutes
        && !(1..=MAX_RECENCY_MINUTES).contains(&minutes)
    {
        return Err(WebError::invalid_input(format!(
            "web_search: recency_minutes must be between 1 and {MAX_RECENCY_MINUTES}"
        )));
    }
    Ok(())
}

fn validate_domains(tool: &str, field: &str, domains: &[String]) -> Result<(), WebError> {
    if domains.len() > MAX_DOMAINS {
        return Err(WebError::invalid_input(format!(
            "{tool}: {field} may contain at most {MAX_DOMAINS} entries"
        )));
    }
    for domain in domains {
        if domain.chars().count() > MAX_DOMAIN_CHARS {
            return Err(WebError::invalid_input(format!(
                "{tool}: each {field} entry must be at most {MAX_DOMAIN_CHARS} characters"
            )));
        }
    }
    Ok(())
}

pub(crate) fn validate_read_request(url: &str, objective: Option<&str>) -> Result<(), WebError> {
    validate_url("web_read", url)?;
    if let Some(objective) = objective
        && objective.chars().count() > MAX_OBJECTIVE_CHARS
    {
        return Err(WebError::invalid_input(format!(
            "web_read: objective must be at most {MAX_OBJECTIVE_CHARS} characters"
        )));
    }
    Ok(())
}

pub(crate) fn validate_research_question(question: &str) -> Result<(), WebError> {
    let question = question.trim();
    if question.is_empty() {
        return Err(WebError::invalid_input(
            "web_research: question must be a non-empty string",
        ));
    }
    if question.chars().count() > MAX_RESEARCH_QUESTION_CHARS {
        return Err(WebError::invalid_input(format!(
            "web_research: question must be at most {MAX_RESEARCH_QUESTION_CHARS} characters"
        )));
    }
    Ok(())
}

/// Validates and normalizes an http(s) URL before any provider is called.
pub(crate) fn validate_url(tool: &str, raw: &str) -> Result<String, WebError> {
    let raw = raw.trim();
    if raw.is_empty() {
        return Err(WebError::invalid_input(format!(
            "{tool}: url must be a non-empty http(s) string"
        )));
    }
    if raw.chars().count() > MAX_URL_CHARS {
        return Err(WebError::invalid_input(format!(
            "{tool}: url must be at most {MAX_URL_CHARS} characters"
        )));
    }
    let mut url = Url::parse(raw)
        .map_err(|err| WebError::invalid_input(format!("{tool}: invalid url: {err}")))?;
    if !matches!(url.scheme(), "http" | "https") {
        return Err(WebError::invalid_input(format!(
            "{tool}: url must use http or https; got {}",
            url.scheme()
        )));
    }
    if url.host_str().is_none() {
        return Err(WebError::invalid_input(format!(
            "{tool}: url must include a host"
        )));
    }
    if !url.username().is_empty() || url.password().is_some() {
        return Err(WebError::invalid_input(format!(
            "{tool}: url must not include credentials"
        )));
    }
    // The fragment never changes the fetched document; dropping it makes cache
    // keys stable for `page` and `page#section` spellings.
    url.set_fragment(None);
    if (url.scheme() == "http" && url.port() == Some(80))
        || (url.scheme() == "https" && url.port() == Some(443))
    {
        let _ = url.set_port(None);
    }
    Ok(url.to_string())
}

/// Validates a target URL and rejects private/local destinations unless the
/// caller explicitly opted in.
pub(crate) async fn validate_target_url(
    tool: &str,
    raw: &str,
    allow_private: bool,
) -> Result<String, WebError> {
    let normalized = validate_url(tool, raw)?;
    if allow_private {
        return Ok(normalized);
    }
    let url = Url::parse(&normalized)
        .map_err(|err| WebError::invalid_input(format!("{tool}: invalid url: {err}")))?;
    match url.host() {
        Some(Host::Ipv4(ip)) => {
            reject_forbidden_ip(tool, IpAddr::V4(ip))?;
        }
        Some(Host::Ipv6(ip)) => {
            reject_forbidden_ip(tool, IpAddr::V6(ip))?;
        }
        Some(Host::Domain(domain)) => {
            let domain = domain.trim_end_matches('.').to_ascii_lowercase();
            if is_localhost_name(&domain) {
                return Err(ssrf_error(tool, &domain));
            }
            let port = url.port_or_known_default().unwrap_or(443);
            let addrs = tokio::net::lookup_host((domain.as_str(), port))
                .await
                .map_err(|err| {
                    WebError::invalid_input(format!(
                        "{tool}: could not resolve target host {domain:?}: {err}"
                    ))
                })?
                .collect::<Vec<_>>();
            if addrs.is_empty() {
                return Err(WebError::invalid_input(format!(
                    "{tool}: target host {domain:?} resolved to no addresses"
                )));
            }
            for addr in addrs {
                reject_forbidden_ip(tool, addr.ip())?;
            }
        }
        None => {
            return Err(WebError::invalid_input(format!(
                "{tool}: url must include a host"
            )));
        }
    }
    Ok(normalized)
}

fn reject_forbidden_ip(tool: &str, ip: IpAddr) -> Result<(), WebError> {
    if is_forbidden_ip(ip) {
        Err(ssrf_error(tool, &ip.to_string()))
    } else {
        Ok(())
    }
}

fn ssrf_error(tool: &str, target: &str) -> WebError {
    WebError::invalid_input(format!(
        "{tool}: target {target} is private, loopback, link-local, or a metadata address; \
         set DHI_WEB_ALLOW_PRIVATE_TARGETS=1 to opt in"
    ))
}

fn is_localhost_name(domain: &str) -> bool {
    domain == "localhost"
        || domain.ends_with(".localhost")
        || domain == "metadata.google.internal"
        || domain == "metadata"
}

/// Whether an IP address is unsafe for a provider to fetch by default.
pub(crate) fn is_forbidden_ip(ip: IpAddr) -> bool {
    match ip {
        IpAddr::V4(ip) => is_forbidden_v4(ip),
        IpAddr::V6(ip) => {
            if let Some(mapped) = ip.to_ipv4_mapped() {
                return is_forbidden_v4(mapped);
            }
            is_forbidden_v6(ip)
        }
    }
}

fn is_forbidden_v4(ip: Ipv4Addr) -> bool {
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

fn is_forbidden_v6(ip: Ipv6Addr) -> bool {
    let segments = ip.segments();
    ip.is_loopback()
        || ip.is_unspecified()
        || ip.is_multicast()
        || (segments[0] & 0xfe00) == 0xfc00
        || (segments[0] & 0xffc0) == 0xfe80
}

/// Collapses insignificant whitespace for cache matching while preserving the
/// query's case and punctuation.
pub(crate) fn normalize_query(raw: &str) -> String {
    raw.split_whitespace().collect::<Vec<_>>().join(" ")
}

/// Caps a document and states whether anything was removed.
pub(crate) fn cap_document(text: &str) -> (String, bool) {
    cap_chars(text, MAX_DOCUMENT_CHARS)
}

/// Caps a research answer and states whether anything was removed.
pub(crate) fn cap_research_answer(text: &str) -> (String, bool) {
    cap_chars(text, crate::model::MAX_RESEARCH_ANSWER_CHARS)
}

/// Caps a search snippet and marks the cut with an ellipsis.
pub(crate) fn cap_snippet(text: &str) -> String {
    cap_chars_marked(text.trim(), MAX_SNIPPET_CHARS)
}

/// Caps a result title and marks the cut with an ellipsis.
pub(crate) fn cap_title(text: &str) -> String {
    cap_chars_marked(text.trim(), MAX_TITLE_CHARS)
}

/// Caps a result URL length.
pub(crate) fn cap_result_url(text: &str) -> String {
    cap_chars_marked(text.trim(), MAX_RESULT_URL_CHARS)
}

fn cap_chars_marked(text: &str, max: usize) -> String {
    let mut chars = text.chars();
    let prefix: String = chars.by_ref().take(max).collect();
    if chars.next().is_none() {
        return prefix;
    }
    format!("{prefix}…")
}

fn cap_chars(text: &str, max: usize) -> (String, bool) {
    let mut chars = text.chars();
    let prefix: String = chars.by_ref().take(max).collect();
    let truncated = chars.next().is_some();
    (prefix, truncated)
}

/// Redacts secrets and caps upstream error text so a provider body can never
/// carry a key into logs or a model-visible error.
pub(crate) fn short_error(text: &str, secrets: &[&Secret]) -> String {
    let redacted = crate::secret::redact_secrets(text, secrets);
    let trimmed = redacted.trim();
    if trimmed.is_empty() {
        return "(empty response body)".to_string();
    }
    let mut chars = trimmed.chars();
    let prefix: String = chars.by_ref().take(500).collect();
    if chars.next().is_some() {
        format!("{prefix}… [truncated]")
    } else {
        prefix
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rejects_non_http_urls_and_credentials() {
        assert!(validate_url("web_read", "ftp://example.com/x").is_err());
        assert!(validate_url("web_read", "not a url").is_err());
        let credential_error =
            validate_url("web_read", "https://user:pass@example.com").expect_err("credentials");
        assert!(!credential_error.message().contains("pass"));
        assert!(!credential_error.message().contains("user"));
        assert_eq!(
            validate_url("web_read", "https://example.com/page#section").unwrap(),
            "https://example.com/page"
        );
    }

    #[test]
    fn caps_long_documents() {
        let long = "x".repeat(MAX_DOCUMENT_CHARS + 4);
        let (text, truncated) = cap_document(&long);
        assert!(truncated);
        assert_eq!(text.chars().count(), MAX_DOCUMENT_CHARS);
    }

    #[test]
    fn classifies_private_addresses() {
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
        ] {
            assert!(
                is_forbidden_ip(ip.parse().expect("ip")),
                "{ip} should be forbidden"
            );
        }
        assert!(!is_forbidden_ip("8.8.8.8".parse().expect("ip")));
        assert!(!is_forbidden_ip(
            "2606:4700:4700::1111".parse().expect("ip")
        ));
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
            let error = validate_target_url("web_read", raw, false)
                .await
                .expect_err("private target");
            assert_eq!(error.kind, crate::ErrorKind::InvalidInput, "{raw}");
        }
        assert!(
            validate_target_url("web_read", "http://127.0.0.1/", true)
                .await
                .is_ok()
        );
    }
}

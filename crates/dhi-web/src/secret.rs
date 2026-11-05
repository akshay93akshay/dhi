//! Secret handling for provider API keys and URLs.

use std::fmt;

use url::Url;

/// A non-empty API key that redacts itself in logs and `Debug` output.
#[derive(Clone, PartialEq, Eq)]
pub struct Secret(String);

impl Secret {
    /// Trims `raw` and returns `None` when the result is empty.
    pub fn new(raw: impl Into<String>) -> Option<Self> {
        let raw = raw.into();
        let raw = raw.trim();
        if raw.is_empty() {
            None
        } else {
            Some(Self(raw.to_string()))
        }
    }

    /// The real value. Only provider request builders should call this.
    pub fn expose(&self) -> &str {
        &self.0
    }

    /// Replaces every occurrence of this secret in `text`.
    pub fn redact(&self, text: &str) -> String {
        text.replace(self.expose(), "[redacted]")
    }
}

impl fmt::Debug for Secret {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("[redacted]")
    }
}

impl fmt::Display for Secret {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("[redacted]")
    }
}

/// Redacts every non-empty secret from `text`.
pub(crate) fn redact_secrets(text: &str, secrets: &[&Secret]) -> String {
    let mut redacted = text.to_string();
    for secret in secrets {
        redacted = secret.redact(&redacted);
    }
    redacted
}

/// A URL safe to put in a log line or skip reason.
///
/// Userinfo is removed, all query values are replaced, and fragments are
/// dropped. Unparseable values become `<invalid-url>` so credentials in a
/// malformed string never reach a log.
pub fn redact_url(raw: &str) -> String {
    let Ok(mut url) = Url::parse(raw.trim()) else {
        return "<invalid-url>".to_string();
    };
    let _ = url.set_username("");
    let _ = url.set_password(None);
    url.set_fragment(None);
    if url.query().is_some() {
        let pairs = url
            .query_pairs()
            .map(|(key, _)| (key.into_owned(), "[redacted]".to_string()))
            .collect::<Vec<_>>();
        url.query_pairs_mut().clear().extend_pairs(pairs);
    }
    url.to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn secrets_are_redacted_from_debug_and_display() {
        let secret = Secret::new("sk-live-123").expect("non-empty");
        assert_eq!(format!("{secret:?}"), "[redacted]");
        assert_eq!(format!("{secret}"), "[redacted]");
        assert_eq!(
            redact_secrets("denied for sk-live-123", &[&secret]),
            "denied for [redacted]"
        );
    }

    #[test]
    fn empty_secrets_are_dropped() {
        assert!(Secret::new("   ").is_none());
        assert!(Secret::new("").is_none());
    }

    #[test]
    fn urls_lose_credentials_and_query_values() {
        assert_eq!(
            redact_url("https://user:pass@example.com/path?token=abc&x=1"),
            "https://example.com/path?token=%5Bredacted%5D&x=%5Bredacted%5D"
        );
        assert_eq!(redact_url("not a url with user:pass"), "<invalid-url>");
    }
}

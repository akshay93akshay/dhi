//! In-memory LRU cache with single-flight de-duplication.

use std::collections::HashMap;
use std::future::Future;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use serde::Serialize;
use sha2::{Digest, Sha256};
use tokio::sync::OnceCell;

use crate::error::WebError;
use crate::model::{
    ReadRequest, ReadResult, ResearchRequest, ResearchResult, SearchRequest, SearchResult,
};
use crate::validation::normalize_query;

/// Hard bound on live cache entries.
pub const MAX_CACHE_ENTRIES: usize = 1_024;
/// Hard bound on a cache TTL (seven days).
pub const MAX_CACHE_TTL_SECS: u64 = 7 * 24 * 60 * 60;

#[derive(Debug, Clone)]
pub(crate) enum Cached {
    Search(Vec<SearchResult>),
    Read(ReadResult),
    Research(ResearchResult),
}

#[derive(Debug, Clone)]
struct CacheEntry {
    value: Cached,
    expires_at: Instant,
    last_used: Instant,
}

impl CacheEntry {
    fn expired(&self, now: Instant) -> bool {
        now >= self.expires_at
    }
}

type InFlightCell = Arc<OnceCell<Result<Cached, WebError>>>;
type InFlightMap = Mutex<HashMap<String, InFlightCell>>;

/// Per-router cache. Identical in-flight requests share one `OnceCell`, so a
/// provider is never called twice for the same key at the same time.
#[derive(Debug)]
pub(crate) struct Cache {
    ttl: Duration,
    entries: Mutex<HashMap<String, CacheEntry>>,
    inflight: InFlightMap,
}

impl Cache {
    pub(crate) fn new(ttl: Duration) -> Self {
        let ttl = ttl.min(Duration::from_secs(MAX_CACHE_TTL_SECS));
        Self {
            ttl,
            entries: Mutex::new(HashMap::new()),
            inflight: Mutex::new(HashMap::new()),
        }
    }

    pub(crate) fn ttl(&self) -> Duration {
        self.ttl
    }

    pub(crate) async fn get_or_load<F, Fut>(&self, key: String, load: F) -> Result<Cached, WebError>
    where
        F: FnOnce() -> Fut,
        Fut: Future<Output = Result<Cached, WebError>>,
    {
        if self.ttl.is_zero() {
            return load().await;
        }
        if let Some(value) = self.lookup(&key) {
            return Ok(value);
        }
        let cell = {
            let mut inflight = self
                .inflight
                .lock()
                .expect("web cache in-flight mutex poisoned");
            inflight
                .entry(key.clone())
                .or_insert_with(|| Arc::new(OnceCell::new()))
                .clone()
        };
        let result = cell.get_or_init(|| async { load().await }).await.clone();
        if let Ok(value) = &result {
            self.store(key.clone(), value.clone());
        }
        {
            let mut inflight = self
                .inflight
                .lock()
                .expect("web cache in-flight mutex poisoned");
            if inflight
                .get(&key)
                .is_some_and(|candidate| Arc::ptr_eq(candidate, &cell))
            {
                inflight.remove(&key);
            }
        }
        result
    }

    pub(crate) fn len(&self) -> usize {
        let now = Instant::now();
        self.entries
            .lock()
            .expect("web cache mutex poisoned")
            .values()
            .filter(|entry| !entry.expired(now))
            .count()
    }

    pub(crate) fn clear(&self) {
        self.entries
            .lock()
            .expect("web cache mutex poisoned")
            .clear();
    }

    fn lookup(&self, key: &str) -> Option<Cached> {
        let now = Instant::now();
        let mut entries = self.entries.lock().expect("web cache mutex poisoned");
        match entries.get_mut(key) {
            Some(entry) if !entry.expired(now) => {
                entry.last_used = now;
                Some(entry.value.clone())
            }
            Some(_) => {
                entries.remove(key);
                None
            }
            None => None,
        }
    }

    fn store(&self, key: String, value: Cached) {
        let now = Instant::now();
        let mut entries = self.entries.lock().expect("web cache mutex poisoned");
        entries.retain(|_, entry| !entry.expired(now));
        if !entries.contains_key(&key)
            && entries.len() >= MAX_CACHE_ENTRIES
            && let Some(oldest) = entries
                .iter()
                .min_by_key(|(_, entry)| entry.last_used)
                .map(|(key, _)| key.clone())
        {
            entries.remove(&oldest);
        }
        entries.insert(
            key,
            CacheEntry {
                value,
                expires_at: now.checked_add(self.ttl).unwrap_or(now),
                last_used: now,
            },
        );
    }
}

#[derive(Serialize)]
struct SearchKey<'a> {
    settings: &'a str,
    query: String,
    limit: usize,
    objective: String,
    queries: Vec<String>,
    recency_minutes: Option<u64>,
    include_domains: Vec<String>,
    exclude_domains: Vec<String>,
}

#[derive(Serialize)]
struct ReadKey<'a> {
    settings: &'a str,
    url: &'a str,
    objective: String,
}

#[derive(Serialize)]
struct ResearchKey<'a> {
    settings: &'a str,
    question: String,
    depth: &'a str,
}

pub(crate) fn search_key(settings: &str, request: &SearchRequest) -> String {
    let mut include = request
        .include_domains
        .iter()
        .map(|domain| domain.trim().to_ascii_lowercase())
        .filter(|domain| !domain.is_empty())
        .collect::<Vec<_>>();
    let mut exclude = request
        .exclude_domains
        .iter()
        .map(|domain| domain.trim().to_ascii_lowercase())
        .filter(|domain| !domain.is_empty())
        .collect::<Vec<_>>();
    include.sort();
    include.dedup();
    exclude.sort();
    exclude.dedup();
    let material = SearchKey {
        settings,
        query: normalize_query(&request.query),
        limit: request.limit,
        objective: normalize_query(request.objective.as_deref().unwrap_or_default()),
        queries: request
            .queries
            .iter()
            .map(|query| normalize_query(query))
            .filter(|query| !query.is_empty())
            .collect(),
        recency_minutes: request.recency_minutes,
        include_domains: include,
        exclude_domains: exclude,
    };
    digest("search", &material)
}

pub(crate) fn read_key(settings: &str, request: &ReadRequest, normalized_url: &str) -> String {
    let material = ReadKey {
        settings,
        url: normalized_url,
        objective: normalize_query(request.objective.as_deref().unwrap_or_default()),
    };
    digest("read", &material)
}

pub(crate) fn research_key(settings: &str, request: &ResearchRequest) -> String {
    let material = ResearchKey {
        settings,
        question: normalize_query(&request.question),
        depth: request.depth.as_str(),
    };
    digest("research", &material)
}

fn digest<T: Serialize>(namespace: &str, material: &T) -> String {
    let encoded = serde_json::to_vec(material).unwrap_or_default();
    let mut hasher = Sha256::new();
    hasher.update(namespace.as_bytes());
    hasher.update(b"\0");
    hasher.update(&encoded);
    format!("{namespace}:{:x}", hasher.finalize())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cache_keys_are_canonical_and_collision_resistant() {
        let one = SearchRequest::new("a\nlimit=2").limit(5);
        let two = SearchRequest::new("a").limit(2);
        assert_ne!(search_key("s", &one), search_key("s", &two));

        let mut reordered = SearchRequest::new("same").include_domains(["b.example", "a.example"]);
        let sorted = SearchRequest::new("same").include_domains(["a.example", "b.example"]);
        assert_eq!(search_key("s", &reordered), search_key("s", &sorted));
        reordered.query = "different".to_string();
        assert_ne!(search_key("s", &reordered), search_key("s", &sorted));
    }

    #[test]
    fn cache_is_bounded_and_evicts_lru_entries() {
        let cache = Cache::new(Duration::from_secs(60));
        for index in 0..MAX_CACHE_ENTRIES + 5 {
            cache.store(
                format!("read:{index}"),
                Cached::Search(vec![SearchResult {
                    title: index.to_string(),
                    url: "https://example.com".to_string(),
                    snippet: String::new(),
                    provider: "test".to_string(),
                }]),
            );
        }
        assert_eq!(cache.len(), MAX_CACHE_ENTRIES);
    }
}

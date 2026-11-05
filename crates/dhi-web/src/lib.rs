//! DHI's provider-neutral web tools: `web_search`, `web_read`, and
//! `web_research`.
//!
//! One [`WebRouter`] sits behind all three stable tool names. Providers are
//! tried in a configured order and every result is normalized to the same
//! structs. The router falls back for rate limits, auth/quota failures,
//! timeouts, 5xx, network failures, and upstream content failures. Invalid
//! input and 400/404/422 stop immediately. An `Unsupported` provider is
//! skipped rather than treated as a terminal failure.
//!
//! # Configuration
//!
//! Environment wins over the optional `DHI_CONFIG` (default
//! `~/.config/dhi/config.json`) `firecrawl` section. No key is ever logged or
//! included in an error message. Per-provider management overrides
//! ([`ProviderSettingsOverride`]) win over both; a stored row is authoritative
//! for its key, including the absence of one.
//!
//! | Variable | Meaning |
//! | --- | --- |
//! | `DHI_WEB_PROVIDER` | `auto` (default), `tinyfish`, `parallel`, or `firecrawl` |
//! | `TINYFISH_API_KEY` | TinyFish Search/Fetch key, sent as `X-API-Key` |
//! | `TINYFISH_SEARCH_URL` / `TINYFISH_FETCH_URL` | Optional endpoint overrides |
//! | `TINYFISH_FETCH_TIMEOUT_SECS` | Fetch deadline, default 180 (minimum 150) |
//! | `PARALLEL_API_KEY` | Parallel key, sent as `x-api-key` |
//! | `PARALLEL_BASE_URL` | Optional Parallel endpoint override |
//! | `FIRECRAWL_API_KEY` | Firecrawl key, sent as `Authorization: Bearer` |
//! | `FIRECRAWL_BASE_URL` | Self-host or proxy endpoint |
//! | `FIRECRAWL_TIMEOUT_SECS` | Firecrawl request deadline, default 60 |
//! | `DHI_WEB_TIMEOUT_SECS` | TinyFish/Parallel request deadline, default 60 |
//! | `DHI_WEB_RESEARCH_TIMEOUT_SECS` | Overall research deadline, default 900 |
//! | `DHI_WEB_OPERATION_TIMEOUT_SECS` | Overall search/read chain deadline, default 180 |
//! | `DHI_WEB_CACHE_TTL_SECS` | Cache TTL, default 300, capped at 7 days; `0` disables |
//! | `DHI_WEB_ALLOW_PRIVATE_TARGETS` | Explicit opt-in for private/loopback reads |
//! | `DHI_CONFIG` | Optional config-file path |
//!
//! Default order:
//! - `web_search` / `web_read`: TinyFish -> Parallel -> Firecrawl.
//! - `web_research`: Parallel only. TinyFish and Firecrawl have no research API.
//!
//! Firecrawl may run without a key only when `FIRECRAWL_BASE_URL` is local
//! (`localhost`, loopback, `0.0.0.0`, or a `.local` host).
//!
//! # Provider mapping
//!
//! - **TinyFish**: `GET https://api.search.tinyfish.ai?query=...` and
//!   `POST https://api.fetch.tinyfish.ai` with
//!   `{urls:[url], format:"markdown", ttl:0}`.
//! - **Parallel**: `POST /v1/search` with `{objective, search_queries}` and
//!   `POST /v1/extract` with full content plus an explicit fresh-fetch policy.
//!   Research uses the Task API (`POST /v1/tasks/runs` then the blocking result
//!   endpoint, retrying 408 responses while preserving the run id).
//! - **Firecrawl**: `POST /v2/search` (domain filters and a `tbs` freshness
//!   bucket) and `POST /v2/scrape` with `formats: ["markdown"]`, `maxAge: 0`.
//!   `web_parse` is folded into `web_read`; document URLs enable the PDF parser.
//!
//! # Safety and limits
//!
//! - Target URLs are rejected when they are loopback, RFC1918, link-local,
//!   IPv6 unique-local, or a metadata address, including DNS names that resolve
//!   there. Private targets require `DHI_WEB_ALLOW_PRIVATE_TARGETS=1`.
//! - Client redirects are disabled so auth headers can never follow a redirect.
//! - Success and error bodies, titles, URLs, snippets, answers, citations, the
//!   cache, and the ledger are all bounded.
//! - Results are cached in memory by canonical, collision-resistant request
//!   keys. Identical in-flight calls share one future.
//! - Every real provider attempt is recorded with latency and a [`Cost`] class.
//!   TinyFish is `Free` only while inside its daily allowance counters.
//!
//! # Known limitations
//!
//! - Parallel documents a 600-second floor for `fetch_policy.max_age_seconds`,
//!   so its own cache may serve content younger than that. TinyFish and
//!   Firecrawl requests explicitly ask for zero provider-side TTL.
//! - Parallel search has no `recency_minutes` mapping in this crate; TinyFish
//!   sends the value directly and Firecrawl maps it to a `tbs` bucket.
//! - Exact per-call prices are not hardcoded; the ledger marks unknown costs as
//!   `Unknown`.

mod cache;
mod config;
mod error;
mod http;
mod ledger;
mod model;
mod router;
mod secret;
mod tools;
mod validation;

pub mod providers;

pub use cache::{MAX_CACHE_ENTRIES, MAX_CACHE_TTL_SECS};
pub use config::{
    DEFAULT_CACHE_TTL_SECS, DEFAULT_OPERATION_TIMEOUT_SECS, DEFAULT_RESEARCH_TIMEOUT_SECS,
    DEFAULT_TIMEOUT_SECS, DhiConfigFile, FirecrawlFileConfig, ProviderKind, ProviderMode,
    ProviderSettingsOverride, WebConfig, config_path, load_config_file,
};
pub use error::{ErrorKind, WebError};
pub use ledger::{Cost, Ledger, LedgerEntry, MAX_LEDGER_ENTRIES, Operation, ProviderStats};
pub use model::{
    DEFAULT_SEARCH_LIMIT, MAX_CITATIONS, MAX_DOCUMENT_CHARS, MAX_DOMAIN_CHARS, MAX_DOMAINS,
    MAX_OBJECTIVE_CHARS, MAX_QUERIES, MAX_QUERY_CHARS, MAX_QUERY_ITEM_CHARS, MAX_RECENCY_MINUTES,
    MAX_RESEARCH_ANSWER_CHARS, MAX_RESULT_URL_CHARS, MAX_SEARCH_LIMIT, MAX_SNIPPET_CHARS,
    MAX_TITLE_CHARS, MAX_URL_CHARS, ReadRequest, ReadResult, ResearchDepth, ResearchRequest,
    ResearchResult, SearchRequest, SearchResult,
};
pub use providers::{
    FirecrawlConfig, FirecrawlProvider, ParallelConfig, ParallelProvider, TinyFishConfig,
    TinyFishProvider, WebProvider, is_local_base_url,
};
pub use router::{DEFAULT_OPERATION_TIMEOUT, DEFAULT_RESEARCH_TIMEOUT, SkippedProvider, WebRouter};
pub use secret::{Secret, redact_url};
pub use tools::{
    WEB_TOOL_NAMES, WebToolSpec, parse_read_input, parse_research_input, parse_search_input,
    web_tool_specs,
};

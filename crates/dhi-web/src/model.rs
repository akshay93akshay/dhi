//! Provider-neutral request and result types.

use std::fmt;
use std::str::FromStr;

use serde::{Deserialize, Serialize};

use crate::error::WebError;

/// The most results a caller may request.
pub const MAX_SEARCH_LIMIT: usize = 10;
/// The number of results returned when the caller omits `limit`.
pub const DEFAULT_SEARCH_LIMIT: usize = 5;
/// Markdown cap for page reads.
pub const MAX_DOCUMENT_CHARS: usize = 20_000;
/// Snippet cap inside one search result.
pub const MAX_SNIPPET_CHARS: usize = 500;
/// Title cap inside one result.
pub const MAX_TITLE_CHARS: usize = 300;
/// URL cap inside one result.
pub const MAX_RESULT_URL_CHARS: usize = 2_048;
/// Maximum query length.
pub const MAX_QUERY_CHARS: usize = 2_000;
/// Maximum objective/purpose length.
pub const MAX_OBJECTIVE_CHARS: usize = 2_000;
/// Maximum number of extra keyword queries.
pub const MAX_QUERIES: usize = 10;
/// Maximum length of one keyword query.
pub const MAX_QUERY_ITEM_CHARS: usize = 200;
/// Maximum domains in each include/exclude list.
pub const MAX_DOMAINS: usize = 200;
/// Maximum length of one domain filter.
pub const MAX_DOMAIN_CHARS: usize = 253;
/// Maximum target URL length.
pub const MAX_URL_CHARS: usize = 2_048;
/// Maximum research question length.
pub const MAX_RESEARCH_QUESTION_CHARS: usize = 10_000;
/// Research answer cap.
pub const MAX_RESEARCH_ANSWER_CHARS: usize = 20_000;
/// Research citation cap.
pub const MAX_CITATIONS: usize = 50;
/// Largest accepted freshness window (10 years, matching TinyFish).
pub const MAX_RECENCY_MINUTES: u64 = 5_256_000;

/// One normalized search request.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct SearchRequest {
    pub query: String,
    pub limit: usize,
    /// A natural-language goal. TinyFish sends it as `purpose`; Parallel uses
    /// it as the search objective.
    pub objective: Option<String>,
    /// Optional keyword queries. Parallel uses these as `search_queries`.
    pub queries: Vec<String>,
    /// Freshness window in minutes. TinyFish sends it directly; Firecrawl maps
    /// it to its closest `tbs` bucket.
    pub recency_minutes: Option<u64>,
    pub include_domains: Vec<String>,
    pub exclude_domains: Vec<String>,
}

impl Default for SearchRequest {
    fn default() -> Self {
        Self {
            query: String::new(),
            limit: DEFAULT_SEARCH_LIMIT,
            objective: None,
            queries: Vec::new(),
            recency_minutes: None,
            include_domains: Vec::new(),
            exclude_domains: Vec::new(),
        }
    }
}

impl SearchRequest {
    pub fn new(query: impl Into<String>) -> Self {
        Self {
            query: query.into(),
            ..Self::default()
        }
    }

    pub fn limit(mut self, limit: usize) -> Self {
        self.limit = limit;
        self
    }

    pub fn objective(mut self, objective: impl Into<String>) -> Self {
        self.objective = Some(objective.into());
        self
    }

    pub fn queries<I, S>(mut self, queries: I) -> Self
    where
        I: IntoIterator<Item = S>,
        S: Into<String>,
    {
        self.queries = queries.into_iter().map(Into::into).collect();
        self
    }

    pub fn recency_minutes(mut self, minutes: u64) -> Self {
        self.recency_minutes = Some(minutes);
        self
    }

    pub fn include_domains<I, S>(mut self, domains: I) -> Self
    where
        I: IntoIterator<Item = S>,
        S: Into<String>,
    {
        self.include_domains = domains.into_iter().map(Into::into).collect();
        self
    }

    pub fn exclude_domains<I, S>(mut self, domains: I) -> Self
    where
        I: IntoIterator<Item = S>,
        S: Into<String>,
    {
        self.exclude_domains = domains.into_iter().map(Into::into).collect();
        self
    }
}

/// One normalized page-read request.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ReadRequest {
    pub url: String,
    /// Optional question the extractor should focus on.
    pub objective: Option<String>,
}

impl ReadRequest {
    pub fn new(url: impl Into<String>) -> Self {
        Self {
            url: url.into(),
            objective: None,
        }
    }

    pub fn objective(mut self, objective: impl Into<String>) -> Self {
        self.objective = Some(objective.into());
        self
    }
}

/// How much work `web_research` may spend.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ResearchDepth {
    /// Fastest/cheapest research processor.
    Quick,
    /// Balanced default.
    #[default]
    Standard,
    /// Deepest supported research processor.
    Deep,
}

impl ResearchDepth {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Quick => "quick",
            Self::Standard => "standard",
            Self::Deep => "deep",
        }
    }

    /// The Parallel Task API processor used for this depth.
    pub fn parallel_processor(self) -> &'static str {
        match self {
            Self::Quick => "lite",
            Self::Standard => "base",
            Self::Deep => "pro",
        }
    }
}

impl fmt::Display for ResearchDepth {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

impl FromStr for ResearchDepth {
    type Err = WebError;

    fn from_str(raw: &str) -> Result<Self, Self::Err> {
        match raw.trim().to_ascii_lowercase().as_str() {
            "" | "standard" | "default" | "normal" => Ok(Self::Standard),
            "quick" | "fast" | "shallow" => Ok(Self::Quick),
            "deep" | "thorough" => Ok(Self::Deep),
            other => Err(WebError::invalid_input(format!(
                "web_research: depth must be quick, standard, or deep; got {other:?}"
            ))),
        }
    }
}

/// One normalized search hit.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SearchResult {
    pub title: String,
    pub url: String,
    pub snippet: String,
    pub provider: String,
}

/// One normalized page read.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ReadResult {
    pub url: String,
    pub title: String,
    pub markdown: String,
    pub truncated: bool,
    pub status_code: Option<u16>,
    pub provider: String,
}

/// One normalized research answer.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ResearchResult {
    pub answer: String,
    pub citations: Vec<String>,
    pub truncated: bool,
    pub provider: String,
}

/// One research request.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ResearchRequest {
    pub question: String,
    #[serde(default)]
    pub depth: ResearchDepth,
}

impl ResearchRequest {
    pub fn new(question: impl Into<String>) -> Self {
        Self {
            question: question.into(),
            depth: ResearchDepth::default(),
        }
    }

    pub fn depth(mut self, depth: ResearchDepth) -> Self {
        self.depth = depth;
        self
    }
}

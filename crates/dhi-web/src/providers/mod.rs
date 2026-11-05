//! Provider implementations behind the router.

use async_trait::async_trait;

use crate::error::WebError;
use crate::ledger::{Cost, Operation};
use crate::model::{
    ReadRequest, ReadResult, ResearchRequest, ResearchResult, SearchRequest, SearchResult,
};

pub mod firecrawl;
pub mod parallel;
pub mod tinyfish;

pub use firecrawl::{
    DEFAULT_CLOUD_BASE_URL, DEFAULT_LOCAL_BASE_URL, FirecrawlConfig, FirecrawlProvider,
    is_local_base_url,
};
pub use parallel::{ParallelConfig, ParallelProvider};
pub use tinyfish::{TinyFishConfig, TinyFishProvider};

/// One interchangeable web provider.
#[async_trait]
pub trait WebProvider: Send + Sync {
    /// Stable provider name used in results, the ledger, and tool output.
    fn name(&self) -> &'static str;

    /// Configuration identity used in cache keys. Providers with custom base
    /// URLs override the default.
    fn cache_namespace(&self) -> String {
        self.name().to_string()
    }

    /// The provider's price class for one operation. Unknown is allowed.
    fn cost_hint(&self, _operation: Operation) -> Cost {
        Cost::Unknown
    }

    /// Synchronizes the router-wide SSRF opt-in with a directly constructed
    /// provider. Providers with no target URL keep the default no-op.
    fn set_allow_private_targets(&self, _allow: bool) {}

    async fn search(&self, request: &SearchRequest) -> Result<Vec<SearchResult>, WebError>;

    async fn read(&self, request: &ReadRequest) -> Result<ReadResult, WebError>;

    async fn research(&self, request: &ResearchRequest) -> Result<ResearchResult, WebError>;
}

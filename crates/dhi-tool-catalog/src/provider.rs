use anyhow::Result;

use crate::entry::ToolCatalogEntry;

/// A source that contributes catalog entries.
///
/// Each call returns a fresh set of entries. A source error is isolated by
/// [`crate::build_catalog`]; the other providers still contribute.
pub trait ToolCatalogProvider: Send + Sync {
    /// Stable provider name used in error reports and `provider_names`.
    fn name(&self) -> &'static str;
    /// Entries owned by this source.
    fn entries(&self) -> Result<Vec<ToolCatalogEntry>>;
}

/// A fixed provider for tests and server wiring.
///
/// Built-in, web, coding, PTY, and other static tool sets can be supplied
/// without writing a source-specific adapter. Entries are cloned on each
/// call so callers cannot mutate the provider through the catalog.
#[derive(Debug, Clone)]
pub struct StaticProvider {
    name: &'static str,
    entries: Vec<ToolCatalogEntry>,
}

impl StaticProvider {
    pub fn new(name: &'static str, entries: Vec<ToolCatalogEntry>) -> Self {
        Self { name, entries }
    }

    pub fn empty(name: &'static str) -> Self {
        Self::new(name, Vec::new())
    }
}

impl ToolCatalogProvider for StaticProvider {
    fn name(&self) -> &'static str {
        self.name
    }

    fn entries(&self) -> Result<Vec<ToolCatalogEntry>> {
        Ok(self.entries.clone())
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use super::*;
    use crate::{ToolSource, build_catalog};

    #[test]
    fn static_provider_returns_its_name_and_entries() {
        let provider = StaticProvider::new(
            "builtin",
            vec![
                ToolCatalogEntry::new("web_search", ToolSource::Web),
                ToolCatalogEntry::new("load_skill", ToolSource::Skills),
            ],
        );

        assert_eq!(provider.name(), "builtin");
        let entries = provider.entries().expect("entries");
        assert_eq!(entries.len(), 2);
        assert_eq!(entries[0].key, "web_search");

        // Each call returns an owned copy.
        let mut second = provider.entries().expect("entries");
        second.clear();
        assert_eq!(provider.entries().expect("entries").len(), 2);
    }

    #[test]
    fn empty_static_provider_contributes_nothing() {
        let providers: Vec<Arc<dyn ToolCatalogProvider>> =
            vec![Arc::new(StaticProvider::empty("empty"))];
        let build = build_catalog(&providers);
        assert!(build.snapshot.is_empty());
        assert!(build.errors.is_empty());
    }
}

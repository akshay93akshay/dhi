use std::fmt;
use std::sync::{Arc, RwLock};

use crate::provider::ToolCatalogProvider;
use crate::snapshot::{CatalogBuild, build_catalog};

/// A thread-safe read model over a replaceable set of catalog providers.
///
/// Snapshots are collected outside the provider lock: a slow source does not
/// block [`Self::replace_providers`] or [`Self::provider_names`]. Each
/// snapshot uses one consistent provider list, so a concurrent replacement is
/// either fully visible or not visible at all.
pub struct CatalogRuntime {
    providers: RwLock<Vec<Arc<dyn ToolCatalogProvider>>>,
}

impl CatalogRuntime {
    pub fn new(providers: Vec<Arc<dyn ToolCatalogProvider>>) -> Self {
        Self {
            providers: RwLock::new(providers),
        }
    }

    /// Collect a fresh snapshot and the isolated source errors.
    ///
    /// Provider order and duplicate handling match [`build_catalog`]: the
    /// first provider to contribute a key wins.
    pub fn snapshot(&self) -> CatalogBuild {
        let providers = self
            .providers
            .read()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .clone();
        build_catalog(&providers)
    }

    /// Replace every provider for a live source change.
    pub fn replace_providers(&self, providers: Vec<Arc<dyn ToolCatalogProvider>>) {
        let mut current = self
            .providers
            .write()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        *current = providers;
    }

    /// Provider names in registration order, including duplicate names.
    pub fn provider_names(&self) -> Vec<&'static str> {
        self.providers
            .read()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .iter()
            .map(|provider| provider.name())
            .collect()
    }
}

impl Default for CatalogRuntime {
    fn default() -> Self {
        Self::new(Vec::new())
    }
}

impl fmt::Debug for CatalogRuntime {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("CatalogRuntime")
            .field("providers", &self.provider_names())
            .finish()
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;
    use std::sync::atomic::{AtomicUsize, Ordering};

    use anyhow::Result;

    use super::*;
    use crate::{StaticProvider, ToolCatalogEntry, ToolSource};

    struct FailingProvider;

    impl ToolCatalogProvider for FailingProvider {
        fn name(&self) -> &'static str {
            "bad"
        }

        fn entries(&self) -> Result<Vec<ToolCatalogEntry>> {
            anyhow::bail!("source unavailable")
        }
    }

    struct CountingProvider {
        name: &'static str,
        entries: Vec<ToolCatalogEntry>,
        calls: AtomicUsize,
    }

    impl ToolCatalogProvider for CountingProvider {
        fn name(&self) -> &'static str {
            self.name
        }

        fn entries(&self) -> Result<Vec<ToolCatalogEntry>> {
            self.calls.fetch_add(1, Ordering::SeqCst);
            Ok(self.entries.clone())
        }
    }

    fn static_provider(
        name: &'static str,
        keys: &[&str],
        source: ToolSource,
    ) -> Arc<dyn ToolCatalogProvider> {
        Arc::new(StaticProvider::new(
            name,
            keys.iter()
                .map(|key| ToolCatalogEntry::new(*key, source))
                .collect(),
        ))
    }

    fn keys(build: &CatalogBuild) -> Vec<&str> {
        build
            .snapshot
            .tools
            .iter()
            .map(|tool| tool.key.as_str())
            .collect()
    }

    #[test]
    fn new_builds_a_snapshot_and_lists_provider_names() {
        let runtime = CatalogRuntime::new(vec![
            static_provider("zeta", &["z"], ToolSource::Web),
            static_provider("alpha", &["a"], ToolSource::Builtin),
        ]);

        let build = runtime.snapshot();
        assert!(build.is_ok());
        assert_eq!(keys(&build), ["a", "z"]);
        assert_eq!(runtime.provider_names(), ["zeta", "alpha"]);
    }

    #[test]
    fn empty_runtime_and_default_are_empty() {
        for runtime in [CatalogRuntime::new(Vec::new()), CatalogRuntime::default()] {
            let build = runtime.snapshot();
            assert!(build.is_ok());
            assert!(build.snapshot.is_empty());
            assert!(runtime.provider_names().is_empty());
        }
    }

    #[test]
    fn replace_providers_swaps_the_live_sources() {
        let runtime = CatalogRuntime::new(vec![static_provider(
            "builtin",
            &["old", "shared"],
            ToolSource::Builtin,
        )]);
        assert_eq!(keys(&runtime.snapshot()), ["old", "shared"]);

        runtime.replace_providers(vec![
            static_provider("web", &["new"], ToolSource::Web),
            static_provider("custom", &["shared"], ToolSource::Custom),
        ]);

        let build = runtime.snapshot();
        assert_eq!(keys(&build), ["new", "shared"]);
        assert_eq!(
            build.snapshot.find("new").map(|tool| tool.source),
            Some(ToolSource::Web)
        );
        assert_eq!(
            build.snapshot.find("shared").map(|tool| tool.source),
            Some(ToolSource::Custom)
        );
        assert_eq!(runtime.provider_names(), ["web", "custom"]);

        runtime.replace_providers(Vec::new());
        assert!(runtime.snapshot().snapshot.is_empty());
        assert!(runtime.provider_names().is_empty());
    }

    #[test]
    fn source_errors_are_isolated_per_snapshot() {
        let runtime = CatalogRuntime::new(vec![
            static_provider("good", &["good"], ToolSource::Builtin),
            Arc::new(FailingProvider),
        ]);

        let build = runtime.snapshot();
        assert_eq!(keys(&build), ["good"]);
        assert_eq!(build.errors.len(), 1);
        assert_eq!(build.errors[0].source, "bad");
        assert!(build.errors[0].error.contains("source unavailable"));
    }

    #[test]
    fn duplicate_keys_report_the_second_provider_and_keep_the_first() {
        let runtime = CatalogRuntime::new(vec![
            static_provider("one", &["same"], ToolSource::Web),
            static_provider("two", &["same"], ToolSource::Custom),
        ]);

        let build = runtime.snapshot();
        assert_eq!(keys(&build), ["same"]);
        assert_eq!(build.snapshot.tools[0].source, ToolSource::Web);
        assert_eq!(build.errors.len(), 1);
        assert_eq!(build.errors[0].source, "two");
    }

    #[test]
    fn every_snapshot_collects_from_the_current_providers() {
        let counting = Arc::new(CountingProvider {
            name: "counted",
            entries: vec![ToolCatalogEntry::new("tool", ToolSource::Builtin)],
            calls: AtomicUsize::new(0),
        });
        let runtime = CatalogRuntime::new(vec![counting.clone()]);

        assert_eq!(runtime.snapshot().snapshot.len(), 1);
        assert_eq!(runtime.snapshot().snapshot.len(), 1);
        assert_eq!(counting.calls.load(Ordering::SeqCst), 2);
    }

    #[test]
    fn runtime_is_send_and_sync() {
        fn assert_send_sync<T: Send + Sync>() {}
        assert_send_sync::<CatalogRuntime>();
    }

    #[test]
    fn concurrent_snapshots_never_see_a_partial_provider_list() {
        let runtime = Arc::new(CatalogRuntime::new(vec![static_provider(
            "a",
            &["a"],
            ToolSource::Builtin,
        )]));

        std::thread::scope(|scope| {
            let writer = Arc::clone(&runtime);
            scope.spawn(move || {
                for round in 0..200 {
                    if round % 2 == 0 {
                        writer.replace_providers(vec![static_provider(
                            "b",
                            &["b"],
                            ToolSource::Web,
                        )]);
                    } else {
                        writer.replace_providers(vec![static_provider(
                            "a",
                            &["a"],
                            ToolSource::Builtin,
                        )]);
                    }
                }
            });

            for _ in 0..200 {
                let build = runtime.snapshot();
                assert!(build.errors.is_empty());
                assert_eq!(build.snapshot.len(), 1);
                let tool = &build.snapshot.tools[0];
                match tool.key.as_str() {
                    "a" => assert_eq!(tool.source, ToolSource::Builtin),
                    "b" => assert_eq!(tool.source, ToolSource::Web),
                    other => panic!("unexpected key {other:?}"),
                }
                assert_eq!(runtime.provider_names().len(), 1);
            }
        });
    }
}

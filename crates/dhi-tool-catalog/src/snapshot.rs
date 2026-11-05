use std::collections::BTreeMap;
use std::sync::Arc;

use serde::{Deserialize, Serialize};

use crate::entry::{ToolCatalogEntry, ToolSource, is_recognized_role};
use crate::provider::ToolCatalogProvider;

/// A normalized snapshot returned to the UI and control layer.
///
/// Snapshots produced by [`build_catalog`] are sorted by key with duplicates
/// removed. Helpers preserve that order, and [`Self::sort_by_key`] is a
/// stable sort for snapshots constructed by callers.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct ToolCatalogSnapshot {
    pub tools: Vec<ToolCatalogEntry>,
}

impl ToolCatalogSnapshot {
    /// Find one entry by its stable key.
    pub fn find(&self, key: &str) -> Option<&ToolCatalogEntry> {
        self.tools.iter().find(|tool| tool.key == key)
    }

    /// Entries that are currently enabled, in catalog order.
    ///
    /// This does not consider role visibility; compose it with
    /// [`Self::visible_to`] when both filters are needed.
    pub fn enabled_only(&self) -> Self {
        self.filtered(|tool| tool.enabled)
    }

    /// Entries visible to `role`, in catalog order.
    ///
    /// An empty `default_roles` list means every recognized role:
    /// `planner`, `worker`, `reporter`, `digest`, and any
    /// `specialist:<name>`. A non-empty list is matched exactly, so `worker`
    /// does not imply `specialist:worker` and a specialist does not inherit a
    /// base role. Unrecognized roles see nothing.
    ///
    /// This filters by role only; it does not remove disabled tools. Compose
    /// with [`Self::enabled_only`] when both filters are needed.
    pub fn visible_to(&self, role: &str) -> Self {
        if !is_recognized_role(role) {
            return Self::default();
        }
        self.filtered(|tool| {
            tool.default_roles.is_empty()
                || tool.default_roles.iter().any(|allowed| allowed == role)
        })
    }

    /// Entries owned by one source, in catalog order.
    pub fn by_source(&self, source: ToolSource) -> Self {
        self.filtered(|tool| tool.source == source)
    }

    /// Stable sort by key. Entries with equal keys keep their relative order.
    pub fn sort_by_key(&mut self) {
        self.tools.sort_by(|left, right| left.key.cmp(&right.key));
    }

    /// Return a copy sorted by key in stable order.
    #[must_use]
    pub fn sorted(mut self) -> Self {
        self.sort_by_key();
        self
    }

    pub fn len(&self) -> usize {
        self.tools.len()
    }

    pub fn is_empty(&self) -> bool {
        self.tools.is_empty()
    }

    fn filtered(&self, predicate: impl Fn(&ToolCatalogEntry) -> bool) -> Self {
        Self {
            tools: self
                .tools
                .iter()
                .filter(|tool| predicate(tool))
                .cloned()
                .collect(),
        }
    }
}

/// One source failure. Other sources still contribute.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CatalogSourceError {
    pub source: String,
    pub error: String,
}

/// The result of collecting all providers.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct CatalogBuild {
    pub snapshot: ToolCatalogSnapshot,
    pub errors: Vec<CatalogSourceError>,
}

impl CatalogBuild {
    /// Whether collection produced no source or entry errors.
    pub fn is_ok(&self) -> bool {
        self.errors.is_empty()
    }
}

/// Collects every provider, validates entries, sorts by key, and keeps the
/// first entry for a duplicate key.
///
/// Provider errors and per-entry validation errors are isolated: the error is
/// recorded in [`CatalogBuild::errors`] and the remaining entries and
/// providers still contribute. A duplicate is reported, never silently
/// merged.
pub fn build_catalog(providers: &[Arc<dyn ToolCatalogProvider>]) -> CatalogBuild {
    let mut tools = BTreeMap::new();
    let mut errors = Vec::new();
    for provider in providers {
        let source = provider.name();
        match provider.entries() {
            Ok(entries) => {
                for entry in entries {
                    if let Err(error) = entry.validate() {
                        errors.push(CatalogSourceError {
                            source: source.to_string(),
                            error: error.to_string(),
                        });
                        continue;
                    }
                    if tools.contains_key(&entry.key) {
                        errors.push(CatalogSourceError {
                            source: source.to_string(),
                            error: format!("duplicate tool key {:?}", entry.key),
                        });
                        continue;
                    }
                    tools.insert(entry.key.clone(), entry);
                }
            }
            Err(error) => errors.push(CatalogSourceError {
                source: source.to_string(),
                error: format!("{error:#}"),
            }),
        }
    }
    CatalogBuild {
        snapshot: ToolCatalogSnapshot {
            tools: tools.into_values().collect(),
        },
        errors,
    }
}

#[cfg(test)]
mod tests {
    use anyhow::Result;
    use serde_json::json;

    use super::*;
    use crate::{MAX_TOOL_KEY_CHARS, StaticProvider};

    struct FakeProvider {
        name: &'static str,
        entries: Vec<ToolCatalogEntry>,
        fail: bool,
    }

    impl ToolCatalogProvider for FakeProvider {
        fn name(&self) -> &'static str {
            self.name
        }

        fn entries(&self) -> Result<Vec<ToolCatalogEntry>> {
            if self.fail {
                anyhow::bail!("boom");
            }
            Ok(self.entries.clone())
        }
    }

    fn entry(key: &str, source: ToolSource) -> ToolCatalogEntry {
        ToolCatalogEntry::new(key, source)
    }

    fn provider(
        name: &'static str,
        entries: Vec<ToolCatalogEntry>,
    ) -> Arc<dyn ToolCatalogProvider> {
        Arc::new(FakeProvider {
            name,
            entries,
            fail: false,
        })
    }

    fn failing(name: &'static str) -> Arc<dyn ToolCatalogProvider> {
        Arc::new(FakeProvider {
            name,
            entries: Vec::new(),
            fail: true,
        })
    }

    fn keys(snapshot: &ToolCatalogSnapshot) -> Vec<&str> {
        snapshot
            .tools
            .iter()
            .map(|tool| tool.key.as_str())
            .collect()
    }

    fn roles(roles: &[&str]) -> Vec<String> {
        roles.iter().map(|role| (*role).to_string()).collect()
    }

    fn snapshot(entries: Vec<ToolCatalogEntry>) -> ToolCatalogSnapshot {
        build_catalog(&[provider("test", entries)]).snapshot
    }

    #[test]
    fn providers_are_sorted_and_source_errors_are_isolated() {
        let providers = vec![
            provider(
                "good",
                vec![entry("z", ToolSource::Web), entry("a", ToolSource::Builtin)],
            ),
            failing("bad"),
        ];
        let build = build_catalog(&providers);
        assert_eq!(keys(&build.snapshot), ["a", "z"]);
        assert_eq!(build.errors.len(), 1);
        assert_eq!(build.errors[0].source, "bad");
        assert!(build.errors[0].error.contains("boom"));
        assert!(!build.is_ok());
    }

    #[test]
    fn duplicate_keys_keep_the_first_and_report_the_second() {
        let providers = vec![
            provider("one", vec![entry("same", ToolSource::Web)]),
            provider("two", vec![entry("same", ToolSource::Custom)]),
        ];
        let build = build_catalog(&providers);
        assert_eq!(build.snapshot.tools.len(), 1);
        assert_eq!(build.snapshot.tools[0].source, ToolSource::Web);
        assert_eq!(build.errors.len(), 1);
        assert_eq!(build.errors[0].source, "two");
        assert!(build.errors[0].error.contains("duplicate"));
    }

    #[test]
    fn duplicate_keys_within_one_provider_keep_the_first() {
        let build = build_catalog(&[provider(
            "one",
            vec![
                entry("same", ToolSource::Web),
                entry("same", ToolSource::Custom),
            ],
        )]);
        assert_eq!(build.snapshot.tools.len(), 1);
        assert_eq!(build.snapshot.tools[0].source, ToolSource::Web);
        assert_eq!(build.errors.len(), 1);
        assert_eq!(build.errors[0].source, "one");
    }

    #[test]
    fn invalid_entries_are_reported_and_skipped_per_entry() {
        let long_key = "k".repeat(MAX_TOOL_KEY_CHARS + 1);
        let build = build_catalog(&[provider(
            "mixed",
            vec![
                entry("good", ToolSource::Web),
                entry("", ToolSource::Web),
                entry("bad\nkey", ToolSource::Web),
                entry(&long_key, ToolSource::Web),
                entry("bad_schema", ToolSource::Web)
                    .with_input_schema(json!(["not", "an", "object"])),
                entry("bad_role", ToolSource::Web).with_default_roles(roles(&["specialist:"])),
            ],
        )]);

        assert_eq!(keys(&build.snapshot), ["good"]);
        assert_eq!(build.errors.len(), 5);
        assert!(build.errors.iter().all(|error| error.source == "mixed"));
        assert!(
            build
                .errors
                .iter()
                .any(|error| error.error == "empty tool key")
        );
        assert!(
            build
                .errors
                .iter()
                .any(|error| error.error.contains("control characters"))
        );
        assert!(
            build
                .errors
                .iter()
                .any(|error| error.error.contains("maximum is 128"))
        );
        assert!(
            build
                .errors
                .iter()
                .any(|error| error.error.contains("must be a JSON object"))
        );
        assert!(
            build
                .errors
                .iter()
                .any(|error| error.error.contains("unrecognized role"))
        );
    }

    #[test]
    fn an_invalid_first_entry_does_not_block_a_valid_duplicate() {
        let build = build_catalog(&[provider(
            "mixed",
            vec![
                entry("same", ToolSource::Web).with_input_schema(json!(null)),
                entry("same", ToolSource::Custom),
            ],
        )]);
        assert_eq!(keys(&build.snapshot), ["same"]);
        assert_eq!(build.snapshot.tools[0].source, ToolSource::Custom);
        assert_eq!(build.errors.len(), 1);
    }

    #[test]
    fn empty_catalog_is_valid() {
        let build = build_catalog(&[]);
        assert!(build.is_ok());
        assert!(build.snapshot.is_empty());
        assert_eq!(build.snapshot.len(), 0);
        assert_eq!(build.snapshot.find("anything"), None);
        assert!(build.snapshot.enabled_only().is_empty());
        assert!(build.snapshot.visible_to("worker").is_empty());
        assert!(build.snapshot.by_source(ToolSource::Builtin).is_empty());
    }

    #[test]
    fn find_and_source_filter_use_stable_keys() {
        let catalog = snapshot(vec![
            entry("z_web", ToolSource::Web),
            entry("a_mcp", ToolSource::Mcp),
            entry("m_web", ToolSource::Web),
        ]);
        assert_eq!(
            catalog.find("a_mcp").map(|tool| tool.source),
            Some(ToolSource::Mcp)
        );
        assert_eq!(catalog.find("missing"), None);
        assert_eq!(
            keys(&catalog.by_source(ToolSource::Web)),
            ["m_web", "z_web"]
        );
        assert!(catalog.by_source(ToolSource::Pty).is_empty());
    }

    #[test]
    fn enabled_and_visible_filters_are_independent() {
        let catalog = snapshot(vec![
            entry("always", ToolSource::Builtin),
            entry("worker_only", ToolSource::Coding).with_default_roles(roles(&["worker"])),
            entry("disabled", ToolSource::Builtin).with_enabled(false),
            entry("chess", ToolSource::Custom).with_default_roles(roles(&["specialist:chess"])),
        ]);

        assert_eq!(
            keys(&catalog.enabled_only()),
            ["always", "chess", "worker_only"]
        );
        assert_eq!(keys(&catalog.visible_to("planner")), ["always", "disabled"]);
        assert_eq!(
            keys(&catalog.visible_to("worker")),
            ["always", "disabled", "worker_only"]
        );
        assert_eq!(
            keys(&catalog.visible_to("specialist:chess")),
            ["always", "chess", "disabled"]
        );
        assert_eq!(
            keys(&catalog.visible_to("specialist:other")),
            ["always", "disabled"]
        );
        assert!(catalog.visible_to("intern").is_empty());
        assert!(catalog.visible_to("Worker").is_empty());

        // Composition applies both filters.
        assert_eq!(
            keys(&catalog.enabled_only().visible_to("worker")),
            ["always", "worker_only"]
        );
    }

    #[test]
    fn explicit_roles_are_exact_and_do_not_inherit() {
        let catalog = snapshot(vec![
            entry("worker", ToolSource::Builtin).with_default_roles(roles(&["worker"])),
            entry("specialist", ToolSource::Builtin)
                .with_default_roles(roles(&["specialist:worker"])),
        ]);
        assert_eq!(keys(&catalog.visible_to("worker")), ["worker"]);
        assert_eq!(
            keys(&catalog.visible_to("specialist:worker")),
            ["specialist"]
        );
    }

    #[test]
    fn filters_and_sorting_preserve_stable_order() {
        let mut catalog = ToolCatalogSnapshot {
            tools: vec![
                entry("z", ToolSource::Web).with_description("z"),
                entry("a", ToolSource::Web).with_description("first a"),
                entry("m", ToolSource::Web),
                entry("a", ToolSource::Web).with_description("second a"),
            ],
        };
        catalog.sort_by_key();
        assert_eq!(keys(&catalog), ["a", "a", "m", "z"]);
        assert_eq!(
            catalog.tools[0].description.as_str(),
            "first a",
            "sort must be stable for equal keys"
        );
        assert_eq!(catalog.tools[1].description.as_str(), "second a");
        assert_eq!(keys(&catalog.enabled_only()), ["a", "a", "m", "z"]);

        let sorted = std::mem::take(&mut catalog).sorted();
        assert_eq!(keys(&sorted), ["a", "a", "m", "z"]);
    }

    #[test]
    fn static_provider_feeds_the_catalog() {
        let providers: Vec<Arc<dyn ToolCatalogProvider>> = vec![Arc::new(StaticProvider::new(
            "builtin",
            vec![
                entry("b", ToolSource::Builtin),
                entry("a", ToolSource::Builtin),
            ],
        ))];
        let build = build_catalog(&providers);
        assert_eq!(keys(&build.snapshot), ["a", "b"]);
    }
}

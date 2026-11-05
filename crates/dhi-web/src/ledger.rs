//! Per-provider latency and cost accounting with a bounded history.

use std::collections::{BTreeMap, VecDeque};
use std::sync::Mutex;
use std::time::Duration;

use serde::{Deserialize, Serialize};

use crate::error::ErrorKind;

/// Hard bound on retained ledger entries.
pub const MAX_LEDGER_ENTRIES: usize = 10_000;

/// Which stable DHI web tool made a call.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Operation {
    Search,
    Read,
    Research,
}

impl Operation {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Search => "search",
            Self::Read => "read",
            Self::Research => "research",
        }
    }
}

/// A provider call's cost. Exact prices are optional; unknown is honest.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum Cost {
    /// Part of a free allowance (for example TinyFish search/fetch).
    Free,
    /// An estimate in millionths of a US dollar.
    Estimated { usd_micros: u64 },
    /// The provider does not publish or expose a per-call price here.
    #[default]
    Unknown,
}

impl Cost {
    pub fn estimated_usd_micros(usd_micros: u64) -> Self {
        Self::Estimated { usd_micros }
    }

    pub fn usd_micros(self) -> Option<u64> {
        match self {
            Self::Free => Some(0),
            Self::Estimated { usd_micros } => Some(usd_micros),
            Self::Unknown => None,
        }
    }
}

/// One recorded provider attempt.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct LedgerEntry {
    pub provider: String,
    pub operation: Operation,
    pub success: bool,
    pub latency_ms: u64,
    pub cost: Cost,
    pub error_kind: Option<ErrorKind>,
}

/// Aggregated latency/cost numbers for one provider.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct ProviderStats {
    pub provider: String,
    pub calls: u64,
    pub successes: u64,
    pub failures: u64,
    pub total_latency_ms: u64,
    pub known_cost_usd_micros: u64,
    pub unknown_cost_calls: u64,
}

/// A router-wide ledger of provider attempts. Cache hits do not record a call
/// because no provider was contacted.
#[derive(Debug, Default)]
pub struct Ledger {
    entries: Mutex<VecDeque<LedgerEntry>>,
}

impl Ledger {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn record(
        &self,
        provider: impl Into<String>,
        operation: Operation,
        latency: Duration,
        success: bool,
        cost: Cost,
        error_kind: Option<ErrorKind>,
    ) {
        let latency_ms = u64::try_from(latency.as_millis()).unwrap_or(u64::MAX);
        let mut entries = self.entries.lock().expect("web ledger mutex poisoned");
        if entries.len() >= MAX_LEDGER_ENTRIES {
            entries.pop_front();
        }
        entries.push_back(LedgerEntry {
            provider: provider.into(),
            operation,
            success,
            latency_ms,
            cost,
            error_kind,
        });
    }

    pub fn entries(&self) -> Vec<LedgerEntry> {
        self.entries
            .lock()
            .expect("web ledger mutex poisoned")
            .iter()
            .cloned()
            .collect()
    }

    pub fn clear(&self) {
        self.entries
            .lock()
            .expect("web ledger mutex poisoned")
            .clear();
    }

    /// Aggregated stats, sorted by provider name.
    pub fn provider_stats(&self) -> Vec<ProviderStats> {
        let mut stats: BTreeMap<String, ProviderStats> = BTreeMap::new();
        for entry in self
            .entries
            .lock()
            .expect("web ledger mutex poisoned")
            .iter()
        {
            let row = stats
                .entry(entry.provider.clone())
                .or_insert_with(|| ProviderStats {
                    provider: entry.provider.clone(),
                    ..ProviderStats::default()
                });
            row.calls += 1;
            if entry.success {
                row.successes += 1;
            } else {
                row.failures += 1;
            }
            row.total_latency_ms = row.total_latency_ms.saturating_add(entry.latency_ms);
            match entry.cost.usd_micros() {
                Some(cost) => row.known_cost_usd_micros += cost,
                None => row.unknown_cost_calls += 1,
            }
        }
        stats.into_values().collect()
    }

    pub fn stats_for(&self, provider: &str) -> ProviderStats {
        self.provider_stats()
            .into_iter()
            .find(|row| row.provider == provider)
            .unwrap_or_else(|| ProviderStats {
                provider: provider.to_string(),
                ..ProviderStats::default()
            })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn aggregates_latency_and_cost() {
        let ledger = Ledger::new();
        ledger.record(
            "tinyfish",
            Operation::Search,
            Duration::from_millis(12),
            true,
            Cost::Free,
            None,
        );
        ledger.record(
            "tinyfish",
            Operation::Read,
            Duration::from_millis(8),
            false,
            Cost::Unknown,
            Some(ErrorKind::Timeout),
        );
        let stats = ledger.stats_for("tinyfish");
        assert_eq!(stats.calls, 2);
        assert_eq!(stats.successes, 1);
        assert_eq!(stats.failures, 1);
        assert_eq!(stats.total_latency_ms, 20);
        assert_eq!(stats.known_cost_usd_micros, 0);
        assert_eq!(stats.unknown_cost_calls, 1);
    }

    #[test]
    fn ledger_history_is_bounded() {
        let ledger = Ledger::new();
        for _ in 0..MAX_LEDGER_ENTRIES + 10 {
            ledger.record(
                "tinyfish",
                Operation::Search,
                Duration::from_millis(1),
                true,
                Cost::Free,
                None,
            );
        }
        assert_eq!(ledger.entries().len(), MAX_LEDGER_ENTRIES);
    }
}

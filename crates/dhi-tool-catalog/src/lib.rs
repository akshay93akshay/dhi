//! One read model for every DHI tool source.
//!
//! Sources keep their own registries. This crate only normalizes their
//! metadata for the catalog and the control API. The key is stable across
//! restarts and unique across sources.
//!
//! [`CatalogRuntime`] owns the live provider list behind a lock and builds
//! fresh snapshots on demand. [`build_catalog`] is the stateless equivalent.
//! Every source is isolated: one provider failure becomes a
//! [`CatalogSourceError`] and the remaining sources still contribute.
//!
//! # Role visibility
//!
//! An empty [`ToolCatalogEntry::default_roles`] list means every recognized
//! role: `planner`, `worker`, `reporter`, `digest`, and any
//! `specialist:<name>`. A non-empty list is matched exactly. There is no role
//! inheritance, and an unrecognized role sees nothing. `[]` is not used to
//! mean "visible to nobody"; use [`ToolCatalogEntry::enabled`] or a control
//! action for that. [`ToolCatalogSnapshot::visible_to`] filters by role only;
//! compose it with [`ToolCatalogSnapshot::enabled_only`] when both are
//! needed.
//!
//! # Metadata
//!
//! [`ToolCatalogEntry::metadata`] is non-secret, source-specific descriptive
//! data. Providers must never put tokens, credentials, authorization headers,
//! endpoint query strings, or other secret values in it. Snapshots are
//! returned to the API and UI without redaction.

mod entry;
mod provider;
mod runtime;
mod snapshot;

pub use entry::{
    CatalogValidationError, MAX_TOOL_KEY_CHARS, SPECIALIST_PREFIX, ToolCatalogEntry, ToolSource,
    all_roles, is_recognized_role, specialist_role, validate_entry,
};
pub use provider::{StaticProvider, ToolCatalogProvider};
pub use runtime::CatalogRuntime;
pub use snapshot::{CatalogBuild, CatalogSourceError, ToolCatalogSnapshot, build_catalog};

//! DHI skills.
//!
//! This crate owns the skill catalog and the `load_skill` meta-tool. The
//! seam is wired by `dhi-server`; this block fills the catalog and the tool.
//!
//! ## Catalog
//!
//! `DHI_SKILLS_DIR` (default `skills`) contains one directory per skill:
//! `skills/<name>/SKILL.md`. A file may start with optional frontmatter:
//!
//! ```markdown
//! ---
//! name: rust-review
//! description: Review Rust changes
//! visible_to: [worker, specialist:researcher]
//! ---
//! Instructions...
//! ```
//!
//! Only `name`, `description`, and `visible_to` are consumed. Without
//! frontmatter the directory name is the skill name, the first non-empty body
//! line is the description, and every recognized role can see it.
//! `visible_to: []` means nobody. Unknown roles see nothing.
//!
//! ## Tool
//!
//! `load_skill` is registered through the shared guarded sink; it is never a
//! raw tool. Its callback reads [`RunRole`] from the dispatch context and
//! stamps it into the sink request under a private key, so the backend can
//! enforce visibility without trusting model input.

mod catalog;
mod tool;

use std::path::{Path, PathBuf};
use std::sync::Arc;

use anyhow::Result;
use dhi_guard::ToolBackend;
use dhi_tool_source::{NamedBackend, ToolSource, ToolSourceContext};
use tracing::{info, warn};

pub use catalog::{
    BASE_ROLES, MAX_SKILL_BYTES, MAX_SKILLS, Skill, SkillCatalog, is_recognized_role, is_safe_name,
};
pub use tool::{LOAD_SKILL, SkillsBackend, TRUSTED_ROLE_KEY, load_skill_schema, load_skill_tool};

/// The process-lifetime skills runtime.
pub struct SkillsRuntime {
    catalog: Arc<SkillCatalog>,
}

impl SkillsRuntime {
    /// Loads the skill catalog from `DHI_SKILLS_DIR` (default `skills`).
    /// An absent directory is an empty catalog.
    pub fn from_env() -> Arc<Self> {
        let root = std::env::var_os("DHI_SKILLS_DIR")
            .filter(|value| !value.is_empty())
            .map(PathBuf::from)
            .unwrap_or_else(|| PathBuf::from("skills"));
        Self::from_dir(root)
    }

    /// Loads the catalog rooted at `path`. Malformed skills are logged and
    /// skipped; this never fails the process.
    pub fn from_dir(path: impl AsRef<Path>) -> Arc<Self> {
        let path = path.as_ref();
        let catalog = Arc::new(SkillCatalog::from_dir(path));
        if catalog.is_empty() {
            info!(
                path = %path.display(),
                "skill catalog is empty (missing directory or no valid skills)"
            );
        } else {
            info!(
                path = %path.display(),
                skills = catalog.len(),
                "skill catalog loaded"
            );
        }
        Arc::new(Self { catalog })
    }

    /// The loaded catalog.
    pub fn catalog(&self) -> &SkillCatalog {
        &self.catalog
    }

    pub fn is_empty(&self) -> bool {
        self.catalog.is_empty()
    }

    /// Wraps the delegate with the skills backend. An empty catalog keeps the
    /// delegate unchanged.
    pub fn backend(self: &Arc<Self>, delegate: Arc<dyn ToolBackend>) -> Arc<dyn ToolBackend> {
        if self.catalog.is_empty() {
            return delegate;
        }
        let backend = Arc::new(SkillsBackend::new(self.catalog.clone()));
        Arc::new(NamedBackend::new(
            [LOAD_SKILL.to_string()],
            backend,
            delegate,
        ))
    }
}

impl ToolSource for SkillsRuntime {
    fn name(&self) -> &'static str {
        "skills"
    }

    fn register(&self, context: &ToolSourceContext<'_>) -> Result<Vec<String>> {
        if self.catalog.is_empty() {
            return Ok(Vec::new());
        }
        if context.handle.snapshot().contains(LOAD_SKILL) {
            warn!(
                tool = LOAD_SKILL,
                "load_skill is already registered; skills registration skipped"
            );
            return Ok(Vec::new());
        }

        let tool = tool::load_skill_tool(context.sink.clone())?;
        context.handle.add_dynamic_tool(tool);
        info!(
            tool = LOAD_SKILL,
            skills = self.catalog.len(),
            "skill meta-tool registered"
        );
        Ok(vec![LOAD_SKILL.to_string()])
    }
}

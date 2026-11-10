//! Project folders and planner-only `AGENTS.md` instructions.
//!
//! A project folder is an absolute directory the user selected at task
//! creation. The task's instruction file is the selected `agents_md_path`,
//! which can be inside or outside that folder. There is deliberately no
//! global file, no automatic nested file, and no upward search. This module
//! also inspects legacy `<project>/AGENTS.md` metadata for old project-folder
//! rows. The file is capped at [`AGENTS_MD_MAX_BYTES`] and truncated on a
//! UTF-8 boundary.

use std::fs::{self, File};
use std::io::{self, Read};
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

/// The one instruction file name inside a project folder.
pub const AGENTS_MD_FILE: &str = "AGENTS.md";

/// Largest number of `AGENTS.md` bytes a planner prompt may receive.
pub const AGENTS_MD_MAX_BYTES: usize = 32 * 1024;

/// What happened when DHI looked for a project's `AGENTS.md` at creation.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AgentsMdState {
    /// The file exists and is at or below the cap.
    Loaded,
    /// The file exists but is larger than the cap; only its prefix is used.
    Truncated,
    /// The file does not exist or is not a regular file.
    NotFound,
    /// The task's toggle is off, so no lookup happens.
    #[default]
    Disabled,
}

impl AgentsMdState {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Loaded => "loaded",
            Self::Truncated => "truncated",
            Self::NotFound => "not_found",
            Self::Disabled => "disabled",
        }
    }

    /// Parses one stored spelling. `None` is an unknown value.
    pub fn parse(value: &str) -> Option<Self> {
        match value {
            "loaded" => Some(Self::Loaded),
            "truncated" => Some(Self::Truncated),
            "not_found" => Some(Self::NotFound),
            "disabled" => Some(Self::Disabled),
            _ => None,
        }
    }

    /// Whether the planner receives an instruction section.
    pub fn has_instructions(self) -> bool {
        matches!(self, Self::Loaded | Self::Truncated)
    }
}

impl std::fmt::Display for AgentsMdState {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

/// The bounded instruction text and the state that describes it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AgentsMdContent {
    pub text: String,
    pub state: AgentsMdState,
}

impl AgentsMdContent {
    fn empty(state: AgentsMdState) -> Self {
        Self {
            text: String::new(),
            state,
        }
    }
}

/// The exact path DHI reads. There is no fallback and no parent search.
pub fn agents_md_path(project: &Path) -> PathBuf {
    project.join(AGENTS_MD_FILE)
}

/// Inspects `<project>/AGENTS.md` without reading its contents.
///
/// A missing path, a directory, or a symbolic link counts as
/// [`AgentsMdState::NotFound`]. A symbolic link is refused so a project file
/// cannot redirect the read to a global or out-of-project file.
pub fn inspect_agents_md(project: &Path) -> io::Result<AgentsMdState> {
    let path = agents_md_path(project);
    let metadata = match fs::symlink_metadata(&path) {
        Ok(metadata) => metadata,
        Err(err) if err.kind() == io::ErrorKind::NotFound => {
            return Ok(AgentsMdState::NotFound);
        }
        Err(err) => return Err(err),
    };
    if metadata.file_type().is_symlink() || !metadata.is_file() {
        return Ok(AgentsMdState::NotFound);
    }
    if metadata.len() > AGENTS_MD_MAX_BYTES as u64 {
        Ok(AgentsMdState::Truncated)
    } else {
        Ok(AgentsMdState::Loaded)
    }
}

/// Reads `<project>/AGENTS.md`, capped at [`AGENTS_MD_MAX_BYTES`].
///
/// A missing file is a normal [`AgentsMdState::NotFound`] result. At most
/// `AGENTS_MD_MAX_BYTES + 4` bytes are read, so a multi-gigabyte file cannot
/// exhaust server memory. A file over the cap is truncated at the last UTF-8
/// character boundary that fits.
pub fn read_agents_md(project: &Path) -> io::Result<AgentsMdContent> {
    let path = agents_md_path(project);
    let metadata = match fs::symlink_metadata(&path) {
        Ok(metadata) => metadata,
        Err(err) if err.kind() == io::ErrorKind::NotFound => {
            return Ok(AgentsMdContent::empty(AgentsMdState::NotFound));
        }
        Err(err) => return Err(err),
    };
    // Refuse a link instead of reading through it: the only file DHI may read
    // is the project's own regular AGENTS.md.
    if metadata.file_type().is_symlink() || !metadata.is_file() {
        return Ok(AgentsMdContent::empty(AgentsMdState::NotFound));
    }

    // Always read through the same cap, independently of the metadata size:
    // a file that grew between the stat above and this open still cannot make
    // this process allocate more than the cap. Four extra bytes cover the
    // longest UTF-8 character that can straddle the boundary.
    let mut file = File::open(&path)?;
    let mut bytes = Vec::with_capacity(AGENTS_MD_MAX_BYTES + 4);
    file.by_ref()
        .take((AGENTS_MD_MAX_BYTES + 4) as u64)
        .read_to_end(&mut bytes)?;
    if bytes.len() > AGENTS_MD_MAX_BYTES {
        Ok(truncated_content(&bytes))
    } else {
        Ok(AgentsMdContent {
            text: String::from_utf8_lossy(&bytes).into_owned(),
            state: AgentsMdState::Loaded,
        })
    }
}

fn truncated_content(bytes: &[u8]) -> AgentsMdContent {
    let end = utf8_boundary(bytes, AGENTS_MD_MAX_BYTES);
    AgentsMdContent {
        text: String::from_utf8_lossy(&bytes[..end]).into_owned(),
        state: AgentsMdState::Truncated,
    }
}

/// The largest index at or below `max` that is a UTF-8 character boundary.
fn utf8_boundary(bytes: &[u8], max: usize) -> usize {
    let mut end = max.min(bytes.len());
    // Continuation bytes (`10xxxxxx`) belong to a character that started
    // before `end`; move back to that character's first byte and cut there.
    while end > 0 && end < bytes.len() && (bytes[end] & 0b1100_0000) == 0b1000_0000 {
        end -= 1;
    }
    end
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_project() -> tempfile::TempDir {
        tempfile::tempdir().unwrap()
    }

    #[test]
    fn missing_file_is_not_found() {
        let project = temp_project();
        let content = read_agents_md(project.path()).unwrap();
        assert_eq!(content.state, AgentsMdState::NotFound);
        assert!(content.text.is_empty());
        assert_eq!(
            inspect_agents_md(project.path()).unwrap(),
            AgentsMdState::NotFound
        );
    }

    #[test]
    fn a_small_file_is_loaded() {
        let project = temp_project();
        fs::write(project.path().join(AGENTS_MD_FILE), "use tabs\n").unwrap();
        let content = read_agents_md(project.path()).unwrap();
        assert_eq!(content.state, AgentsMdState::Loaded);
        assert_eq!(content.text, "use tabs\n");
        assert_eq!(
            inspect_agents_md(project.path()).unwrap(),
            AgentsMdState::Loaded
        );
    }

    #[test]
    fn an_oversized_file_is_truncated_on_a_utf8_boundary() {
        let project = temp_project();
        // Fill just below the cap, then add a multi-byte character that
        // straddles it: the truncation must not split the character.
        let mut text = "a".repeat(AGENTS_MD_MAX_BYTES - 1);
        text.push('é'); // two bytes in UTF-8
        fs::write(project.path().join(AGENTS_MD_FILE), &text).unwrap();

        let content = read_agents_md(project.path()).unwrap();
        assert_eq!(content.state, AgentsMdState::Truncated);
        assert_eq!(content.text.len(), AGENTS_MD_MAX_BYTES - 1);
        assert!(content.text.chars().all(|ch| ch == 'a'));
        assert_eq!(
            inspect_agents_md(project.path()).unwrap(),
            AgentsMdState::Truncated
        );
    }

    #[test]
    fn a_large_file_is_read_only_up_to_the_cap() {
        let project = temp_project();
        fs::write(project.path().join(AGENTS_MD_FILE), "b".repeat(1024 * 1024)).unwrap();

        let content = read_agents_md(project.path()).unwrap();
        assert_eq!(content.state, AgentsMdState::Truncated);
        assert_eq!(content.text.len(), AGENTS_MD_MAX_BYTES);
        assert!(content.text.bytes().all(|byte| byte == b'b'));
    }

    #[test]
    fn a_directory_named_agents_md_is_not_found() {
        let project = temp_project();
        fs::create_dir(project.path().join(AGENTS_MD_FILE)).unwrap();
        assert_eq!(
            read_agents_md(project.path()).unwrap().state,
            AgentsMdState::NotFound
        );
    }

    #[cfg(unix)]
    #[test]
    fn a_symlinked_agents_md_is_not_followed() {
        use std::os::unix::fs::symlink;

        let project = temp_project();
        let outside = temp_project();
        let global = outside.path().join("AGENTS.md");
        fs::write(&global, "global instructions").unwrap();
        symlink(&global, project.path().join(AGENTS_MD_FILE)).unwrap();

        let content = read_agents_md(project.path()).unwrap();
        assert_eq!(content.state, AgentsMdState::NotFound);
        assert!(content.text.is_empty());
    }

    #[test]
    fn only_the_project_root_file_is_read() {
        let project = temp_project();
        fs::create_dir(project.path().join("nested")).unwrap();
        fs::write(
            project.path().join("nested").join(AGENTS_MD_FILE),
            "nested instructions",
        )
        .unwrap();

        let content = read_agents_md(project.path()).unwrap();
        assert_eq!(content.state, AgentsMdState::NotFound);
        assert!(content.text.is_empty());
    }

    #[test]
    fn state_spellings_round_trip() {
        for state in [
            AgentsMdState::Loaded,
            AgentsMdState::Truncated,
            AgentsMdState::NotFound,
            AgentsMdState::Disabled,
        ] {
            assert_eq!(AgentsMdState::parse(state.as_str()), Some(state));
            assert_eq!(
                serde_json::to_string(&state).unwrap(),
                format!("\"{state}\"")
            );
        }
    }
}

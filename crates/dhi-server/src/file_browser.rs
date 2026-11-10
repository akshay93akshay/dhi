//! Server-side host file browser for project-folder and `AGENTS.md`
//! selection.
//!
//! `DHI_FILE_BROWSER_ROOT` is an optional absolute directory. When it is set,
//! the UI lists it through `GET /api/fs` and task creation accepts any real
//! directory or `AGENTS.md` file contained in it (the root itself is listable
//! and selectable as a project folder). When it is unset, the browser is
//! disabled: the route answers `404` and task creation rejects a
//! `project_folder` or `agents_md_path`.
//!
//! The browser never reads file contents. It lists directories and regular
//! files, skips symlinks, and refuses every path that is relative, contains
//! `..`, escapes the canonical root, or passes through a symlink inside the
//! root.

use std::ffi::OsStr;
use std::fs;
use std::io;
use std::path::{Component, Path, PathBuf};

use anyhow::{anyhow, bail};
use chrono::{DateTime, Utc};
use dhi_core::{AGENTS_MD_FILE, AGENTS_MD_MAX_BYTES, AgentsMdState};
use serde::Serialize;

/// The environment variable that enables the host file browser.
pub const FILE_BROWSER_ROOT_ENV: &str = "DHI_FILE_BROWSER_ROOT";

/// One entry of a directory listing.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum FileKind {
    Directory,
    File,
}

/// `GET /api/fs` entry.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct FileEntry {
    pub name: String,
    pub path: String,
    pub kind: FileKind,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub size: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub modified_at: Option<String>,
}

/// `GET /api/fs` response.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct DirectoryListing {
    pub root: String,
    pub path: String,
    pub parent: Option<String>,
    pub entries: Vec<FileEntry>,
}

/// A validated `AGENTS.md` selection: canonical path plus its metadata state.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SelectedAgentsMd {
    pub path: PathBuf,
    pub state: AgentsMdState,
}

/// The validated, optional host file-browser root.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct FileBrowser {
    root: Option<PathBuf>,
}

impl FileBrowser {
    /// Reads and validates `DHI_FILE_BROWSER_ROOT` from the process
    /// environment. An unset or empty value disables the browser.
    pub fn from_env() -> anyhow::Result<Self> {
        let raw = std::env::var(FILE_BROWSER_ROOT_ENV).ok();
        Self::parse(raw.as_deref())
    }

    /// Parses and validates a configured root. A missing directory is
    /// created. Relative paths, `..`, files, symlinks, and unreadable
    /// directories are refused.
    pub fn parse(raw: Option<&str>) -> anyhow::Result<Self> {
        let Some(raw) = raw.map(str::trim).filter(|value| !value.is_empty()) else {
            return Ok(Self::default());
        };
        let requested = Path::new(raw);
        if !requested.is_absolute() {
            bail!("{FILE_BROWSER_ROOT_ENV} must be an absolute path: {raw}");
        }
        if has_parent_component(requested) {
            bail!("{FILE_BROWSER_ROOT_ENV} must not contain `..`: {raw}");
        }
        // Drop a trailing separator so `/path/to/link/` still exposes the
        // final symlink to the metadata check below.
        let requested = lexical_path(requested);
        match fs::symlink_metadata(&requested) {
            Ok(metadata) => {
                check_root_metadata(&metadata, raw)?;
            }
            Err(err) if err.kind() == io::ErrorKind::NotFound => {
                fs::create_dir_all(&requested)
                    .map_err(|err| anyhow!("cannot create {FILE_BROWSER_ROOT_ENV} {raw}: {err}"))?;
            }
            Err(err) => {
                bail!("{FILE_BROWSER_ROOT_ENV} {raw} cannot be inspected: {err}");
            }
        }
        let metadata = fs::symlink_metadata(&requested)
            .map_err(|err| anyhow!("{FILE_BROWSER_ROOT_ENV} {raw} cannot be inspected: {err}"))?;
        check_root_metadata(&metadata, raw)?;
        let canonical = fs::canonicalize(&requested)
            .map_err(|err| anyhow!("{FILE_BROWSER_ROOT_ENV} {raw} cannot be resolved: {err}"))?;
        check_readable(&canonical)?;
        Ok(Self {
            root: Some(canonical),
        })
    }

    /// Whether `DHI_FILE_BROWSER_ROOT` was configured.
    pub fn enabled(&self) -> bool {
        self.root.is_some()
    }

    /// The canonical configured root.
    pub fn root(&self) -> Option<&Path> {
        self.root.as_deref()
    }

    /// Lists one directory below the configured root.
    ///
    /// The requested path may be the root itself. Every other path must
    /// resolve inside the canonical root.
    pub fn browse(&self, requested: &str) -> Result<DirectoryListing, String> {
        let root = self.configured_root()?;
        let requested = requested_path(requested, "path")?;
        let canonical = self.canonical_below_root(root, &requested, "path")?;
        let metadata = fs::metadata(&canonical)
            .map_err(|err| format!("path {} cannot be inspected: {err}", canonical.display()))?;
        if !metadata.is_dir() {
            return Err(format!("path is not a directory: {}", canonical.display()));
        }
        let entries = read_entries(&canonical)?;
        let parent = if canonical == root {
            None
        } else {
            canonical
                .parent()
                .filter(|parent| parent.starts_with(root))
                .map(path_text)
        };
        Ok(DirectoryListing {
            root: path_text(root),
            path: path_text(&canonical),
            parent,
            entries,
        })
    }

    /// Validates a create-time `project_folder`: an absolute real directory
    /// below the configured root, with no symlink in the selected path.
    pub fn resolve_directory(&self, requested: &str) -> Result<PathBuf, String> {
        let root = self.configured_root()?;
        let requested = requested_path(requested, "project_folder")?;
        let canonical = self.canonical_below_root(root, &requested, "project_folder")?;
        let metadata = fs::metadata(&canonical).map_err(|err| {
            format!(
                "project_folder {} cannot be inspected: {err}",
                canonical.display()
            )
        })?;
        if !metadata.is_dir() {
            return Err(format!(
                "project_folder is not a directory: {}",
                canonical.display()
            ));
        }
        Ok(canonical)
    }

    /// Validates a create-time `agents_md_path`: a real regular file named
    /// exactly `AGENTS.md`, at most [`AGENTS_MD_MAX_BYTES`] bytes, below the
    /// configured root and not reachable through a symlink.
    pub fn resolve_agents_md(&self, requested: &str) -> Result<SelectedAgentsMd, String> {
        let root = self.configured_root()?;
        let requested = requested_path(requested, "agents_md_path")?;
        if requested.file_name() != Some(OsStr::new(AGENTS_MD_FILE)) {
            return Err(format!(
                "agents_md_path must name {AGENTS_MD_FILE} (case-sensitive): {}",
                requested.display()
            ));
        }
        // Resolve and prove the path is under the root before reading any
        // directory. This prevents a rejected outside-root path from probing
        // its parent directory.
        let canonical = self.canonical_below_root(root, &requested, "agents_md_path")?;
        // On a case-insensitive filesystem the request can open `agents.md`
        // through the `AGENTS.md` spelling. Require the directory entry's real
        // name to match too, so the basename rule stays case-sensitive. The
        // check now reads only a canonical parent inside the root.
        let parent = canonical.parent().ok_or_else(|| {
            format!(
                "agents_md_path {} has no parent directory",
                canonical.display()
            )
        })?;
        let entries = fs::read_dir(parent).map_err(|err| {
            format!(
                "agents_md_path {} parent cannot be inspected: {err}",
                canonical.display()
            )
        })?;
        let exact = entries
            .filter_map(Result::ok)
            .any(|entry| entry.file_name().as_os_str() == OsStr::new(AGENTS_MD_FILE));
        if !exact {
            return Err(format!(
                "agents_md_path must name {AGENTS_MD_FILE} (case-sensitive): {}",
                canonical.display()
            ));
        }
        let metadata = fs::metadata(&canonical).map_err(|err| {
            format!(
                "agents_md_path {} cannot be inspected: {err}",
                canonical.display()
            )
        })?;
        if !metadata.is_file() {
            return Err(format!(
                "agents_md_path is not a regular file: {}",
                canonical.display()
            ));
        }
        if metadata.len() > AGENTS_MD_MAX_BYTES as u64 {
            return Err(format!(
                "agents_md_path {} is {} bytes; the limit is {AGENTS_MD_MAX_BYTES} bytes",
                canonical.display(),
                metadata.len()
            ));
        }
        Ok(SelectedAgentsMd {
            path: canonical,
            state: agents_md_state(&metadata),
        })
    }

    fn configured_root(&self) -> Result<&Path, String> {
        self.root
            .as_deref()
            .ok_or_else(|| format!("{FILE_BROWSER_ROOT_ENV} is not configured"))
    }

    /// Resolves `requested` below `root` and returns its canonical path.
    ///
    /// The check rejects `..` before touching the filesystem, refuses a
    /// symlink in the selected path, canonicalizes, and proves the result is
    /// still inside the root.
    fn canonical_below_root(
        &self,
        root: &Path,
        requested: &Path,
        field: &str,
    ) -> Result<PathBuf, String> {
        reject_symlinks(root, requested, field)?;
        let canonical = fs::canonicalize(requested)
            .map_err(|err| format!("{field} {} cannot be resolved: {err}", requested.display()))?;
        if !canonical.starts_with(root) {
            return Err(format!(
                "{field} {} is outside {FILE_BROWSER_ROOT_ENV}",
                requested.display()
            ));
        }
        Ok(canonical)
    }
}

fn check_root_metadata(metadata: &fs::Metadata, raw: &str) -> anyhow::Result<()> {
    if metadata.file_type().is_symlink() {
        bail!(
            "{FILE_BROWSER_ROOT_ENV} must not be a symbolic link; \
             configure its real directory instead: {raw}"
        );
    }
    if !metadata.is_dir() {
        bail!("{FILE_BROWSER_ROOT_ENV} is not a directory: {raw}");
    }
    Ok(())
}

/// Proves the server can list the configured root.
fn check_readable(path: &Path) -> anyhow::Result<()> {
    fs::read_dir(path).map_err(|err| {
        anyhow!(
            "{FILE_BROWSER_ROOT_ENV} is not readable ({}): {err}",
            path.display()
        )
    })?;
    Ok(())
}

fn has_parent_component(path: &Path) -> bool {
    path.components()
        .any(|component| matches!(component, Component::ParentDir))
}

fn requested_path(requested: &str, field: &str) -> Result<PathBuf, String> {
    if requested.trim().is_empty() {
        return Err(format!("{field} must not be empty"));
    }
    let path = Path::new(requested);
    if !path.is_absolute() {
        return Err(format!("{field} must be absolute: {requested}"));
    }
    if has_parent_component(path) {
        return Err(format!("{field} must not contain `..`: {requested}"));
    }
    Ok(lexical_path(path))
}

/// A lexical path with `.` and a trailing separator removed. `..` is rejected
/// before this normalizes, so it can never move a component upward.
fn lexical_path(path: &Path) -> PathBuf {
    path.components().collect()
}

/// Refuses a symlink in the selected path.
///
/// The walk covers every component, so a link is refused even when it lives
/// above the selected leaf. System aliases that merely spell the route to the
/// canonical root (for example macOS `/var` for `/private/var`) are the one
/// exception: their target is a strict ancestor of the configured root.
fn reject_symlinks(root: &Path, path: &Path, field: &str) -> Result<(), String> {
    let mut current = PathBuf::new();
    for component in path.components() {
        match component {
            Component::CurDir => continue,
            Component::ParentDir => {
                return Err(format!("{field} must not contain `..`: {}", path.display()));
            }
            _ => current.push(component),
        }
        if current.as_os_str().is_empty() {
            continue;
        }
        let metadata = match fs::symlink_metadata(&current) {
            Ok(metadata) => metadata,
            Err(_) => continue,
        };
        if !metadata.file_type().is_symlink() {
            continue;
        }
        let target = fs::canonicalize(&current).map_err(|err| {
            format!(
                "{field} symbolic link {} cannot be resolved: {err}",
                current.display()
            )
        })?;
        // Compare the CANONICAL parent, not the lexical path. A request can
        // reach the root through an above-root alias (macOS `/var`), so a
        // lexical `starts_with(root)` check misses a user symlink inside the
        // root. The canonical parent tells us whether the link lives inside
        // the root, regardless of how the caller spelled the path.
        let parent = current
            .parent()
            .ok_or_else(|| format!("{field} symbolic link {} has no parent", current.display()))?;
        let canonical_parent = fs::canonicalize(parent).map_err(|err| {
            format!(
                "{field} symbolic link parent {} cannot be resolved: {err}",
                parent.display()
            )
        })?;
        if canonical_parent.starts_with(root) {
            return Err(format!(
                "{field} must not be a symbolic link or contain one: {}",
                current.display()
            ));
        }
        if root.starts_with(&target) && target != root {
            continue;
        }
        return Err(format!(
            "{field} must not be a symbolic link or contain one: {}",
            current.display()
        ));
    }
    Ok(())
}

fn read_entries(dir: &Path) -> Result<Vec<FileEntry>, String> {
    let reader = fs::read_dir(dir)
        .map_err(|err| format!("path {} cannot be listed: {err}", dir.display()))?;
    let mut entries = Vec::new();
    for entry in reader {
        let entry =
            entry.map_err(|err| format!("path {} cannot be listed: {err}", dir.display()))?;
        // One unreadable entry must not hide the rest of the directory. macOS
        // home roots, for example, can contain `.Trash` entries that cannot be
        // inspected by the server's user. Skip them.
        let Ok(file_type) = entry.file_type() else {
            continue;
        };
        if file_type.is_symlink() {
            continue;
        }
        let kind = if file_type.is_dir() {
            FileKind::Directory
        } else if file_type.is_file() {
            FileKind::File
        } else {
            continue;
        };
        let Ok(metadata) = entry.metadata() else {
            continue;
        };
        entries.push(FileEntry {
            name: entry.file_name().to_string_lossy().into_owned(),
            path: entry.path().to_string_lossy().into_owned(),
            kind,
            size: (kind == FileKind::File).then_some(metadata.len()),
            modified_at: metadata.modified().ok().map(timestamp),
        });
    }
    entries.sort_by(|left, right| {
        let left_dir = left.kind == FileKind::Directory;
        let right_dir = right.kind == FileKind::Directory;
        right_dir
            .cmp(&left_dir)
            .then_with(|| left.name.cmp(&right.name))
    });
    Ok(entries)
}

fn agents_md_state(metadata: &fs::Metadata) -> AgentsMdState {
    if metadata.len() > AGENTS_MD_MAX_BYTES as u64 {
        AgentsMdState::Truncated
    } else {
        AgentsMdState::Loaded
    }
}

fn timestamp(time: std::time::SystemTime) -> String {
    DateTime::<Utc>::from(time).to_rfc3339()
}

fn path_text(path: &Path) -> String {
    path.to_string_lossy().into_owned()
}

#[cfg(test)]
mod tests {
    use super::*;
    #[cfg(unix)]
    use std::os::unix::fs::symlink;

    fn browser() -> (tempfile::TempDir, FileBrowser) {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path().join("root");
        let browser = FileBrowser::parse(Some(root.to_str().unwrap())).unwrap();
        (temp, browser)
    }

    fn root(browser: &FileBrowser) -> PathBuf {
        browser
            .root()
            .expect("test browser is enabled")
            .to_path_buf()
    }

    #[test]
    fn disabled_without_a_root() {
        for raw in [None, Some(""), Some("   ")] {
            let browser = FileBrowser::parse(raw).unwrap();
            assert!(!browser.enabled());
            assert!(browser.root().is_none());
            assert!(browser.browse("/").is_err());
            assert!(browser.resolve_directory("/").is_err());
            assert!(browser.resolve_agents_md("/AGENTS.md").is_err());
        }
    }

    #[test]
    fn missing_root_is_created_and_canonicalized() {
        let temp = tempfile::tempdir().unwrap();
        let missing = temp.path().join("nested").join("root");
        let browser = FileBrowser::parse(Some(missing.to_str().unwrap())).unwrap();
        let canonical = fs::canonicalize(&missing).unwrap();
        assert!(canonical.is_dir());
        assert_eq!(browser.root(), Some(canonical.as_path()));
    }

    #[test]
    fn invalid_roots_are_refused() {
        let temp = tempfile::tempdir().unwrap();
        assert!(FileBrowser::parse(Some("relative/root")).is_err());

        let file = temp.path().join("file");
        fs::write(&file, "x").unwrap();
        assert!(FileBrowser::parse(Some(file.to_str().unwrap())).is_err());
    }

    #[cfg(unix)]
    #[test]
    fn a_symlinked_root_is_refused() {
        let temp = tempfile::tempdir().unwrap();
        let real = temp.path().join("real");
        fs::create_dir(&real).unwrap();
        let link = temp.path().join("link");
        symlink(&real, &link).unwrap();
        let err = FileBrowser::parse(Some(link.to_str().unwrap())).unwrap_err();
        assert!(err.to_string().contains("symbolic link"), "{err}");

        // A trailing separator must not follow the link into its target.
        let with_slash = format!("{}/", link.display());
        let err = FileBrowser::parse(Some(&with_slash)).unwrap_err();
        assert!(err.to_string().contains("symbolic link"), "{err}");
    }

    #[test]
    fn root_listing_has_no_parent_and_sorts_directories_first() {
        let (temp, browser) = browser();
        let root = root(&browser);
        fs::create_dir(root.join("zeta")).unwrap();
        fs::create_dir(root.join("alpha")).unwrap();
        fs::write(root.join("b.txt"), "hello").unwrap();
        fs::write(root.join("a.txt"), "").unwrap();
        // An unrelated directory above the root must never leak into it.
        fs::create_dir(temp.path().join("outside")).unwrap();

        let listing = browser.browse(root.to_str().unwrap()).unwrap();
        assert_eq!(listing.root, path_text(&root));
        assert_eq!(listing.path, path_text(&root));
        assert_eq!(listing.parent, None);

        let names: Vec<&str> = listing
            .entries
            .iter()
            .map(|entry| entry.name.as_str())
            .collect();
        assert_eq!(names, ["alpha", "zeta", "a.txt", "b.txt"]);
        assert_eq!(listing.entries[0].kind, FileKind::Directory);
        assert_eq!(listing.entries[2].kind, FileKind::File);
        assert_eq!(listing.entries[2].size, Some(0));
        assert_eq!(listing.entries[3].size, Some(5));
        assert!(listing.entries[0].size.is_none());
        assert!(listing.entries[3].modified_at.is_some());
        assert!(listing.entries[0].path.starts_with(root.to_str().unwrap()));
    }

    #[test]
    fn child_listing_reports_its_parent() {
        let (_temp, browser) = browser();
        let root = root(&browser);
        let child = root.join("project");
        fs::create_dir(&child).unwrap();
        fs::create_dir(child.join("src")).unwrap();

        let listing = browser.browse(child.to_str().unwrap()).unwrap();
        assert_eq!(listing.path, path_text(&child));
        assert_eq!(listing.parent, Some(path_text(&root)));
        assert_eq!(listing.entries.len(), 1);
        assert_eq!(listing.entries[0].name, "src");
        assert_eq!(listing.entries[0].kind, FileKind::Directory);
    }

    #[test]
    fn traversal_and_relative_paths_are_refused() {
        let (temp, browser) = browser();
        let root = root(&browser);
        let outside = temp.path().join("outside");
        fs::create_dir(&outside).unwrap();

        let invalid = vec![
            "relative".to_string(),
            String::new(),
            "   ".to_string(),
            root.join("..").display().to_string(),
            root.join("..").join("outside").display().to_string(),
            outside.display().to_string(),
            "/".to_string(),
        ];
        for path in invalid {
            assert!(browser.browse(&path).is_err(), "{path:?} must be refused");
        }

        // A project folder cannot be a file or a missing path either.
        let file = root.join("file.txt");
        fs::write(&file, "x").unwrap();
        assert!(browser.resolve_directory(file.to_str().unwrap()).is_err());
        assert!(
            browser
                .resolve_directory(root.join("missing").to_str().unwrap())
                .is_err()
        );
    }

    #[cfg(unix)]
    #[test]
    fn symlinks_are_skipped_and_refused() {
        let (temp, browser) = browser();
        let root = root(&browser);
        let target = root.join("target");
        fs::create_dir(&target).unwrap();
        fs::create_dir(target.join("child")).unwrap();
        fs::write(root.join("regular.txt"), "x").unwrap();
        let dir_link = root.join("dir-link");
        let file_link = root.join("file-link");
        symlink(&target, &dir_link).unwrap();
        symlink(root.join("regular.txt"), &file_link).unwrap();

        let listing = browser.browse(root.to_str().unwrap()).unwrap();
        let names: Vec<&str> = listing
            .entries
            .iter()
            .map(|entry| entry.name.as_str())
            .collect();
        assert_eq!(names, ["target", "regular.txt"]);
        assert!(!names.contains(&"dir-link"));
        assert!(!names.contains(&"file-link"));

        assert!(browser.browse(dir_link.to_str().unwrap()).is_err());
        // A symlink ancestor is refused even when the target stays inside the
        // root and the leaf below it is a real directory.
        assert!(
            browser
                .browse(dir_link.join("child").to_str().unwrap())
                .is_err()
        );
        assert!(
            browser
                .resolve_directory(dir_link.to_str().unwrap())
                .is_err()
        );

        let outside = temp.path().join("outside");
        fs::create_dir(&outside).unwrap();
        let agents_link = root.join("AGENTS.md");
        symlink(outside.join("AGENTS.md"), &agents_link).unwrap();
        assert!(
            browser
                .resolve_agents_md(agents_link.to_str().unwrap())
                .is_err()
        );

        // A link inside the root that points at an ancestor of the root must
        // still be refused. The macOS `/var` alias exception applies only to
        // links above the root, not to a link the user placed inside it.
        let ancestor_link = root.join("ancestor-link");
        symlink(temp.path(), &ancestor_link).unwrap();
        assert!(
            browser
                .browse(ancestor_link.join("root").to_str().unwrap())
                .is_err()
        );

        // The same inside-root link must be refused when the caller spells the
        // route through an above-root alias. This is the macOS `/var` case.
        let alias = temp.path().join("alias");
        symlink(temp.path(), &alias).unwrap();
        assert!(
            browser
                .browse(
                    alias
                        .join("root")
                        .join("ancestor-link")
                        .join("root")
                        .to_str()
                        .unwrap()
                )
                .is_err()
        );
    }

    #[test]
    fn agents_md_selection_checks_name_size_and_kind() {
        let (temp, browser) = browser();
        let root = root(&browser);
        let agents = root.join("AGENTS.md");
        fs::write(&agents, "use tabs\n").unwrap();

        let selected = browser.resolve_agents_md(agents.to_str().unwrap()).unwrap();
        assert_eq!(selected.path, fs::canonicalize(&agents).unwrap());
        assert_eq!(selected.state, AgentsMdState::Loaded);

        // The file may live in a nested directory; only the basename is fixed.
        let nested = root.join("nested");
        fs::create_dir(&nested).unwrap();
        let nested_agents = nested.join("AGENTS.md");
        fs::write(&nested_agents, "nested rules").unwrap();
        assert!(
            browser
                .resolve_agents_md(nested_agents.to_str().unwrap())
                .is_ok()
        );

        let wrong_case = root.join("agents.md");
        fs::write(&wrong_case, "rules").unwrap();
        assert!(
            browser
                .resolve_agents_md(wrong_case.to_str().unwrap())
                .is_err()
        );

        let other_name = root.join("OTHER.md");
        fs::write(&other_name, "rules").unwrap();
        assert!(
            browser
                .resolve_agents_md(other_name.to_str().unwrap())
                .is_err()
        );

        let directory = root.join("dir");
        fs::create_dir(&directory).unwrap();
        fs::create_dir(directory.join("AGENTS.md")).unwrap();
        assert!(
            browser
                .resolve_agents_md(directory.join("AGENTS.md").to_str().unwrap())
                .is_err()
        );

        let outside = temp.path().join("outside");
        fs::create_dir(&outside).unwrap();
        let outside_agents = outside.join("AGENTS.md");
        fs::write(&outside_agents, "outside").unwrap();
        assert!(
            browser
                .resolve_agents_md(outside_agents.to_str().unwrap())
                .is_err()
        );
    }

    #[test]
    fn agents_md_size_limit_is_inclusive() {
        let (_temp, browser) = browser();
        let root = root(&browser);
        let at_limit = root.join("at-limit");
        fs::create_dir(&at_limit).unwrap();
        fs::write(at_limit.join("AGENTS.md"), vec![b'a'; AGENTS_MD_MAX_BYTES]).unwrap();
        assert!(
            browser
                .resolve_agents_md(at_limit.join("AGENTS.md").to_str().unwrap())
                .is_ok()
        );

        let over_limit = root.join("over-limit");
        fs::create_dir(&over_limit).unwrap();
        fs::write(
            over_limit.join("AGENTS.md"),
            vec![b'a'; AGENTS_MD_MAX_BYTES + 1],
        )
        .unwrap();
        let err = browser
            .resolve_agents_md(over_limit.join("AGENTS.md").to_str().unwrap())
            .unwrap_err();
        assert!(err.contains("limit"), "{err}");
    }
}

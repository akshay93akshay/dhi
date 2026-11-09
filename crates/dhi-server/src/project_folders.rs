//! Configured project folders for project-mode tasks.
//!
//! `DHI_PROJECT_FOLDERS` is a comma-separated list of absolute paths. Each
//! path is canonicalized and validated at startup: it must exist, be a
//! directory, not be a symbolic link, and be writable by the server. A task
//! may select exactly one of these canonical paths; subfolders, `..`, and
//! symlinked selections are refused.

use std::collections::HashSet;
use std::fs;
use std::io::Write;
use std::path::{Component, Path, PathBuf};
use std::sync::Arc;

use anyhow::{anyhow, bail};
use serde::Serialize;

/// The environment variable that owns the project folder list.
pub const PROJECT_FOLDERS_ENV: &str = "DHI_PROJECT_FOLDERS";

/// One validated project folder.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProjectFolder {
    /// The canonical path.
    pub path: PathBuf,
    /// A display name, normally the final path component.
    pub name: String,
}

/// The validated project folder list.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ProjectFolders {
    folders: Vec<ProjectFolder>,
}

impl ProjectFolders {
    /// Reads and validates `DHI_PROJECT_FOLDERS` from the process environment.
    pub fn from_env() -> anyhow::Result<Self> {
        let raw = std::env::var(PROJECT_FOLDERS_ENV).ok();
        Self::parse(raw.as_deref())
    }

    /// Parses a comma-separated list. An unset or empty value is an empty,
    /// valid list.
    pub fn parse(raw: Option<&str>) -> anyhow::Result<Self> {
        let Some(raw) = raw else {
            return Ok(Self::default());
        };
        let mut folders = Vec::new();
        let mut seen = HashSet::new();
        for entry in raw.split(',') {
            let entry = entry.trim();
            if entry.is_empty() {
                continue;
            }
            let folder = validate_folder(entry)
                .map_err(|err| anyhow!("{PROJECT_FOLDERS_ENV} entry {entry:?}: {err}"))?;
            if !seen.insert(folder.path.clone()) {
                bail!(
                    "{PROJECT_FOLDERS_ENV} lists the project folder {} more than once",
                    folder.path.display()
                );
            }
            folders.push(folder);
        }
        Ok(Self { folders })
    }

    pub fn folders(&self) -> &[ProjectFolder] {
        &self.folders
    }

    pub fn is_empty(&self) -> bool {
        self.folders.is_empty()
    }

    pub fn paths(&self) -> Vec<&Path> {
        self.folders
            .iter()
            .map(|folder| folder.path.as_path())
            .collect()
    }

    /// Resolves a requested create-time folder to the one canonical configured
    /// path it must match exactly.
    ///
    /// `..`, relative paths, and subfolders are refused. A requested path that
    /// is itself a symbolic link is refused even when it resolves to a
    /// configured folder, so a task cannot enter a project through a link.
    pub fn resolve(&self, requested: &str) -> Result<PathBuf, String> {
        let requested = requested.trim();
        if requested.is_empty() {
            return Err("project_folder must not be empty".to_string());
        }
        let path = Path::new(requested);
        if !path.is_absolute() {
            return Err(format!("project_folder must be absolute: {requested}"));
        }
        if path
            .components()
            .any(|component| matches!(component, Component::ParentDir))
        {
            return Err(format!("project_folder must not contain `..`: {requested}"));
        }
        let metadata = fs::symlink_metadata(path)
            .map_err(|err| format!("project_folder {requested} cannot be inspected: {err}"))?;
        if metadata.file_type().is_symlink() {
            return Err(format!(
                "project_folder must not be a symbolic link: {requested}"
            ));
        }
        if !metadata.is_dir() {
            return Err(format!("project_folder is not a directory: {requested}"));
        }
        let canonical = fs::canonicalize(path)
            .map_err(|err| format!("project_folder {requested} cannot be resolved: {err}"))?;
        if self.folders.iter().any(|folder| folder.path == canonical) {
            Ok(canonical)
        } else {
            Err(format!(
                "project_folder {requested} is not one of the configured {PROJECT_FOLDERS_ENV} folders"
            ))
        }
    }
}

/// The JSON shape of one `GET /api/project-folders` entry.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct ProjectFolderInfo {
    pub path: String,
    pub name: String,
    pub agents_md_present: bool,
}

impl ProjectFolders {
    /// The listing returned by `GET /api/project-folders`.
    pub fn info(&self) -> Vec<ProjectFolderInfo> {
        self.folders
            .iter()
            .map(|folder| ProjectFolderInfo {
                path: folder.path.to_string_lossy().into_owned(),
                name: folder.name.clone(),
                agents_md_present: dhi_core::inspect_agents_md(&folder.path)
                    .is_ok_and(|state| state.has_instructions()),
            })
            .collect()
    }
}

fn validate_folder(entry: &str) -> anyhow::Result<ProjectFolder> {
    let requested = Path::new(entry);
    if !requested.is_absolute() {
        bail!("path must be absolute");
    }
    if requested
        .components()
        .any(|component| matches!(component, Component::ParentDir))
    {
        bail!("path must not contain `..`");
    }
    let metadata = fs::symlink_metadata(requested)
        .map_err(|err| anyhow!("path cannot be inspected: {err}"))?;
    if metadata.file_type().is_symlink() {
        bail!("path is a symbolic link; configure its real directory instead");
    }
    if !metadata.is_dir() {
        bail!("path is not a directory");
    }
    let canonical =
        fs::canonicalize(requested).map_err(|err| anyhow!("path cannot be resolved: {err}"))?;
    check_writable(&canonical)?;
    let name = canonical
        .file_name()
        .map(|name| name.to_string_lossy().into_owned())
        .unwrap_or_else(|| canonical.display().to_string());
    Ok(ProjectFolder {
        path: canonical,
        name,
    })
}

/// Proves the server can create and remove a file in `path`.
fn check_writable(path: &Path) -> anyhow::Result<()> {
    let probe = path.join(format!(".dhi-write-check-{}", uuid::Uuid::new_v4()));
    let created = fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(&probe);
    let mut file = match created {
        Ok(file) => file,
        Err(err) => {
            return Err(anyhow!(
                "path is not writable by the server ({}): {err}",
                path.display()
            ));
        }
    };
    let write = file.write_all(b"ok");
    drop(file);
    let removed = fs::remove_file(&probe);
    if let Err(err) = write {
        return Err(anyhow!(
            "path is not writable by the server ({}): {err}",
            path.display()
        ));
    }
    removed.map_err(|err| {
        anyhow!(
            "cannot clean up the write probe in {}: {err}",
            path.display()
        )
    })
}

/// The shared runtime value held by [`crate::AppState`].
pub type SharedProjectFolders = Arc<ProjectFolders>;

#[cfg(test)]
mod tests {
    use super::*;
    #[cfg(unix)]
    use std::os::unix::fs::symlink;

    fn dirs() -> (tempfile::TempDir, PathBuf, PathBuf) {
        let temp = tempfile::tempdir().unwrap();
        let first = temp.path().join("first");
        let second = temp.path().join("second");
        fs::create_dir_all(&first).unwrap();
        fs::create_dir_all(&second).unwrap();
        (temp, first, second)
    }

    #[test]
    fn valid_paths_are_canonical_and_named() {
        let (_temp, first, second) = dirs();
        let parsed = ProjectFolders::parse(Some(&format!(
            " {} , {} ",
            first.display(),
            second.display()
        )))
        .unwrap();
        assert_eq!(parsed.folders().len(), 2);
        assert_eq!(parsed.folders()[0].path, fs::canonicalize(&first).unwrap());
        assert_eq!(parsed.folders()[0].name, "first");
        assert_eq!(parsed.folders()[1].name, "second");
    }

    #[test]
    fn empty_configuration_is_valid_and_empty() {
        assert!(ProjectFolders::parse(None).unwrap().is_empty());
        assert!(ProjectFolders::parse(Some("")).unwrap().is_empty());
        assert!(ProjectFolders::parse(Some(" , , ")).unwrap().is_empty());
    }

    #[test]
    fn relative_paths_are_refused() {
        let err = ProjectFolders::parse(Some("relative/project")).unwrap_err();
        assert!(err.to_string().contains("absolute"), "{err}");
    }

    #[test]
    fn missing_paths_are_refused() {
        let temp = tempfile::tempdir().unwrap();
        let missing = temp.path().join("missing");
        let err = ProjectFolders::parse(Some(&missing.display().to_string())).unwrap_err();
        assert!(err.to_string().contains("cannot be inspected"), "{err}");
    }

    #[test]
    fn files_are_refused() {
        let temp = tempfile::tempdir().unwrap();
        let file = temp.path().join("file");
        fs::write(&file, "not a folder").unwrap();
        let err = ProjectFolders::parse(Some(&file.display().to_string())).unwrap_err();
        assert!(err.to_string().contains("not a directory"), "{err}");
    }

    #[cfg(unix)]
    #[test]
    fn symlinked_folders_are_refused_even_when_the_target_is_valid() {
        let (temp, first, _) = dirs();
        let link = temp.path().join("link");
        symlink(&first, &link).unwrap();
        let err = ProjectFolders::parse(Some(&link.display().to_string())).unwrap_err();
        assert!(err.to_string().contains("symbolic link"), "{err}");
    }

    #[test]
    fn duplicate_paths_are_refused() {
        let (_temp, first, _) = dirs();
        let value = format!("{},{}", first.display(), first.display());
        let err = ProjectFolders::parse(Some(&value)).unwrap_err();
        assert!(err.to_string().contains("more than once"), "{err}");
    }

    #[cfg(unix)]
    #[test]
    fn unwritable_paths_are_refused() {
        use std::os::unix::fs::PermissionsExt;

        let (_temp, first, _) = dirs();
        fs::set_permissions(&first, fs::Permissions::from_mode(0o500)).unwrap();
        // Root can write despite the mode bits; only assert where the probe
        // actually observes the permission failure.
        let probe = first.join(".probe");
        let writable = fs::write(&probe, b"x").is_ok();
        let _ = fs::remove_file(&probe);
        if writable {
            fs::set_permissions(&first, fs::Permissions::from_mode(0o700)).unwrap();
            return;
        }
        // Validate while the folder is still read-only; restore afterwards.
        let err = ProjectFolders::parse(Some(&first.display().to_string())).unwrap_err();
        fs::set_permissions(&first, fs::Permissions::from_mode(0o700)).unwrap();
        assert!(err.to_string().contains("not writable"), "{err}");
    }

    #[test]
    fn resolve_requires_an_exact_configured_folder() {
        let (_temp, first, second) = dirs();
        let parsed =
            ProjectFolders::parse(Some(&format!("{},{}", first.display(), second.display())))
                .unwrap();
        let canonical = fs::canonicalize(&first).unwrap();
        assert_eq!(
            parsed.resolve(&first.display().to_string()).unwrap(),
            canonical
        );
        assert!(
            parsed
                .resolve(&second.join("nested").display().to_string())
                .is_err()
        );
        assert!(parsed.resolve("relative").is_err());
        assert!(parsed.resolve("../escape").is_err());
    }

    #[cfg(unix)]
    #[test]
    fn resolve_refuses_a_symlink_even_to_a_configured_folder() {
        let (temp, first, _) = dirs();
        let parsed = ProjectFolders::parse(Some(&first.display().to_string())).unwrap();
        let link = temp.path().join("alias");
        symlink(&first, &link).unwrap();
        assert!(parsed.resolve(&link.display().to_string()).is_err());
    }

    #[test]
    fn info_reports_agents_md_presence() {
        let (_temp, first, second) = dirs();
        fs::write(first.join("AGENTS.md"), "instructions").unwrap();
        let parsed =
            ProjectFolders::parse(Some(&format!("{},{}", first.display(), second.display())))
                .unwrap();
        let info = parsed.info();
        assert!(info[0].agents_md_present);
        assert!(!info[1].agents_md_present);
        assert_eq!(info[0].name, "first");
    }
}

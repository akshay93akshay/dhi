//! Browser-folder web projects: storage, upload safety, and result outputs.
//!
//! The browser never sends its folder handle. It creates a project, uploads
//! one snapshot file per request as raw bytes, and later downloads the task's
//! `.dhi` outputs to write back through the File System Access API.
//!
//! `DHI_WEB_PROJECTS_ROOT` is an optional absolute directory. It must exist or
//! be creatable, be writable by the server, and must not be a symbolic link.
//! The same absolute path must be visible to the server process and to the
//! Docker daemon: a task's `project_folder` is `<root>/<id>` and the existing
//! project mount uses it directly, with no host-root remapping.

use std::fs::{self, File, OpenOptions};
use std::io::{self, Read, Write};
use std::path::{Component, Path, PathBuf};

use anyhow::{anyhow, bail};
use async_trait::async_trait;
use chrono::{DateTime, Utc};
use dhi_store::Store;
use dhi_store::StoreError;
use dhi_store::web_projects::WebProjectRecord;
use serde::Serialize;
use uuid::Uuid;

/// The environment variable that enables browser-folder web projects.
pub const WEB_PROJECTS_ROOT_ENV: &str = "DHI_WEB_PROJECTS_ROOT";

/// Largest accepted upload for one snapshot file (8 MB).
pub const WEB_PROJECT_MAX_FILE_BYTES: u64 = 8 * 1024 * 1024;

/// Largest accepted derived tree for one project (256 MB).
pub const WEB_PROJECT_MAX_TOTAL_BYTES: u64 = 256 * 1024 * 1024;

/// Longest accepted project display name, in characters.
pub const WEB_PROJECT_NAME_MAX_CHARS: usize = 200;

/// Largest single output file returned by `GET /api/tasks/{id}/outputs`.
pub const OUTPUT_MAX_FILE_BYTES: u64 = 4 * 1024 * 1024;

/// Largest raw total returned by `GET /api/tasks/{id}/outputs`. Base64 rows
/// expand by roughly one third after this cap is applied.
pub const OUTPUT_MAX_TOTAL_BYTES: u64 = 16 * 1024 * 1024;

/// The server-managed directory inside every project.
pub const WEB_PROJECT_STATE_DIR: &str = ".dhi";

/// Metadata persistence used by the web-project routes.
///
/// Production wires [`dhi_store::Store`]; tests use a small in-memory double.
#[async_trait]
pub trait WebProjectStore: Send + Sync {
    async fn create_web_project(
        &self,
        id: Uuid,
        name: &str,
        root_path: &str,
    ) -> Result<WebProjectRecord, StoreError>;

    async fn list_web_projects(&self) -> Result<Vec<WebProjectRecord>, StoreError>;

    async fn get_web_project(&self, id: Uuid) -> Result<Option<WebProjectRecord>, StoreError>;
}

#[async_trait]
impl WebProjectStore for Store {
    async fn create_web_project(
        &self,
        id: Uuid,
        name: &str,
        root_path: &str,
    ) -> Result<WebProjectRecord, StoreError> {
        Store::create_web_project(self, id, name, root_path).await
    }

    async fn list_web_projects(&self) -> Result<Vec<WebProjectRecord>, StoreError> {
        Store::list_web_projects(self).await
    }

    async fn get_web_project(&self, id: Uuid) -> Result<Option<WebProjectRecord>, StoreError> {
        Store::get_web_project(self, id).await
    }
}

/// The validated, optional web-project root.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct WebProjects {
    root: Option<PathBuf>,
}

impl WebProjects {
    /// Reads and validates `DHI_WEB_PROJECTS_ROOT` from the process
    /// environment. An unset or empty value disables web projects.
    pub fn from_env() -> anyhow::Result<Self> {
        let raw = std::env::var(WEB_PROJECTS_ROOT_ENV).ok();
        Self::parse(raw.as_deref())
    }

    /// Parses and validates a configured root. Missing directories are
    /// created; symbolic links, relative paths, files, and unwritable
    /// directories are refused.
    pub fn parse(raw: Option<&str>) -> anyhow::Result<Self> {
        let Some(raw) = raw.map(str::trim).filter(|value| !value.is_empty()) else {
            return Ok(Self::default());
        };
        let requested = Path::new(raw);
        if !requested.is_absolute() {
            bail!("{WEB_PROJECTS_ROOT_ENV} must be an absolute path: {raw}");
        }
        if requested
            .components()
            .any(|component| matches!(component, Component::ParentDir))
        {
            bail!("{WEB_PROJECTS_ROOT_ENV} must not contain `..`: {raw}");
        }
        match fs::symlink_metadata(requested) {
            Ok(metadata) => {
                if metadata.file_type().is_symlink() {
                    bail!(
                        "{WEB_PROJECTS_ROOT_ENV} must not be a symbolic link; \
                         configure its real directory instead"
                    );
                }
                if !metadata.is_dir() {
                    bail!("{WEB_PROJECTS_ROOT_ENV} is not a directory: {raw}");
                }
            }
            Err(err) if err.kind() == io::ErrorKind::NotFound => {
                fs::create_dir_all(requested)
                    .map_err(|err| anyhow!("cannot create {WEB_PROJECTS_ROOT_ENV} {raw}: {err}"))?;
            }
            Err(err) => {
                bail!("{WEB_PROJECTS_ROOT_ENV} {raw} cannot be inspected: {err}");
            }
        }
        let metadata = fs::symlink_metadata(requested)
            .map_err(|err| anyhow!("{WEB_PROJECTS_ROOT_ENV} {raw} cannot be inspected: {err}"))?;
        if metadata.file_type().is_symlink() {
            bail!("{WEB_PROJECTS_ROOT_ENV} must not be a symbolic link");
        }
        if !metadata.is_dir() {
            bail!("{WEB_PROJECTS_ROOT_ENV} is not a directory: {raw}");
        }
        let canonical = fs::canonicalize(requested)
            .map_err(|err| anyhow!("{WEB_PROJECTS_ROOT_ENV} {raw} cannot be resolved: {err}"))?;
        check_writable(&canonical)?;
        Ok(Self {
            root: Some(canonical),
        })
    }

    /// Whether `DHI_WEB_PROJECTS_ROOT` was configured.
    pub fn enabled(&self) -> bool {
        self.root.is_some()
    }

    /// The canonical configured root.
    pub fn root(&self) -> Option<&Path> {
        self.root.as_deref()
    }

    /// The directory of one project under the configured root.
    pub fn project_dir(&self, id: Uuid) -> Option<PathBuf> {
        self.root.as_ref().map(|root| root.join(id.to_string()))
    }

    /// Creates `<root>/<id>` for a newly created project.
    ///
    /// The directory must end up as a real directory, never a symlink.
    pub fn create_project_dir(&self, id: Uuid) -> Result<PathBuf, String> {
        let dir = self
            .project_dir(id)
            .ok_or_else(|| "web projects are disabled".to_string())?;
        match fs::symlink_metadata(&dir) {
            Ok(metadata) => {
                if metadata.file_type().is_symlink() || !metadata.is_dir() {
                    return Err(format!(
                        "web project directory is not a real directory: {}",
                        dir.display()
                    ));
                }
            }
            Err(err) if err.kind() == io::ErrorKind::NotFound => {
                fs::create_dir(&dir).map_err(|err| {
                    format!(
                        "cannot create web project directory {}: {err}",
                        dir.display()
                    )
                })?;
            }
            Err(err) => {
                return Err(format!(
                    "web project directory {} cannot be inspected: {err}",
                    dir.display()
                ));
            }
        }
        Ok(dir)
    }

    /// Writes one snapshot file atomically under a validated relative path.
    ///
    /// Server-side limits are enforced here, not only in the browser: 8 MB per
    /// file and 256 MB for the derived project tree after the write.
    pub fn upload_file(
        &self,
        project_dir: &Path,
        path: &UploadPath,
        bytes: &[u8],
    ) -> Result<u64, UploadError> {
        let incoming = bytes.len() as u64;
        if incoming > WEB_PROJECT_MAX_FILE_BYTES {
            return Err(UploadError::rejected(format!(
                "file is {incoming} bytes; the per-file limit is {WEB_PROJECT_MAX_FILE_BYTES} bytes"
            )));
        }
        ensure_real_directory(project_dir)?;
        let dest = project_dir.join(&path.relative);
        let replaced = existing_file_size(&dest)?;
        let current = scan_tree(project_dir).map_err(UploadError::Io)?.total_bytes;
        let next = project_total_after_upload(current, replaced, incoming);
        if next > WEB_PROJECT_MAX_TOTAL_BYTES {
            return Err(UploadError::rejected(format!(
                "upload would make the project {next} bytes; the total limit is \
                 {WEB_PROJECT_MAX_TOTAL_BYTES} bytes"
            )));
        }
        let dest = prepare_destination(project_dir, &path.relative)?;
        atomic_write(&dest, bytes).map_err(UploadError::Io)?;
        Ok(incoming)
    }
}

fn check_writable(path: &Path) -> anyhow::Result<()> {
    let probe = path.join(format!(".dhi-write-check-{}", Uuid::new_v4()));
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

/// Validates a project display name and returns its trimmed form.
pub fn validate_name(raw: &str) -> Result<String, String> {
    let name = raw.trim();
    if name.is_empty() {
        return Err("name must not be empty".to_string());
    }
    if name.chars().count() > WEB_PROJECT_NAME_MAX_CHARS {
        return Err(format!(
            "name must be at most {WEB_PROJECT_NAME_MAX_CHARS} characters"
        ));
    }
    if name.chars().any(char::is_control) {
        return Err("name must not contain control characters".to_string());
    }
    Ok(name.to_string())
}

/// One validated, normalized upload path.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UploadPath {
    /// The filesystem-relative path, safe to join below the project root.
    pub relative: PathBuf,
    /// The same path with `/` separators, echoed by the route.
    pub display: String,
}

/// Validates a browser-supplied relative path.
///
/// Absolute paths, `..`, empty paths, NUL, backslashes, and any path under the
/// server-managed `.dhi` directory are refused.
pub fn validate_upload_path(raw: &str) -> Result<UploadPath, String> {
    if raw.is_empty() {
        return Err("path must not be empty".to_string());
    }
    if raw.contains('\0') {
        return Err("path must not contain NUL".to_string());
    }
    if raw.contains('\\') {
        return Err("path must use `/` separators".to_string());
    }
    if raw.len() >= 2 && raw.as_bytes()[0].is_ascii_alphabetic() && raw.as_bytes()[1] == b':' {
        return Err("path must be relative".to_string());
    }
    let path = Path::new(raw);
    if path.is_absolute() {
        return Err("path must be relative".to_string());
    }
    let mut relative = PathBuf::new();
    for component in path.components() {
        match component {
            Component::Normal(part) => {
                let part = part
                    .to_str()
                    .ok_or_else(|| "path must be valid UTF-8".to_string())?;
                relative.push(part);
            }
            Component::CurDir => {}
            Component::ParentDir => return Err("path must not contain `..`".to_string()),
            Component::RootDir | Component::Prefix(_) => {
                return Err("path must be relative".to_string());
            }
        }
    }
    if relative.as_os_str().is_empty() {
        return Err("path must not be empty".to_string());
    }
    if relative.components().next().is_some_and(|component| {
        component
            .as_os_str()
            .to_string_lossy()
            .eq_ignore_ascii_case(WEB_PROJECT_STATE_DIR)
    }) {
        return Err(format!(
            "path must not target the reserved {WEB_PROJECT_STATE_DIR} directory"
        ));
    }
    let mut display = String::new();
    for component in relative.components() {
        if !display.is_empty() {
            display.push('/');
        }
        display.push_str(&component.as_os_str().to_string_lossy());
    }
    Ok(UploadPath { relative, display })
}

/// The absolute directory of one stored project. The stored root wins so a
/// later environment change cannot silently repoint old projects.
pub fn record_project_dir(record: &WebProjectRecord) -> PathBuf {
    Path::new(&record.root_path).join(record.id.to_string())
}

/// One project row plus its tree-derived counts.
#[derive(Debug, Clone, Serialize)]
pub struct WebProjectInfo {
    pub id: Uuid,
    pub name: String,
    pub agents_md_present: bool,
    pub file_count: u64,
    pub total_bytes: u64,
    pub created_at: DateTime<Utc>,
}

/// Derived counts for one project tree.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct ProjectTreeStats {
    pub file_count: u64,
    pub total_bytes: u64,
}

/// Builds the JSON view of one project, scanning its tree for the optional
/// counts. A missing or unreadable tree degrades to zero counts rather than
/// hiding the project from listings.
pub fn project_info(record: &WebProjectRecord) -> WebProjectInfo {
    let dir = record_project_dir(record);
    let agents_md_present = dhi_core::inspect_agents_md(&dir)
        .map(|state| state.has_instructions())
        .unwrap_or(false);
    let stats = scan_tree(&dir).unwrap_or_default();
    WebProjectInfo {
        id: record.id,
        name: record.name.clone(),
        agents_md_present,
        file_count: stats.file_count,
        total_bytes: stats.total_bytes,
        created_at: record.created_at,
    }
}

/// Counts regular files and bytes in `dir`, skipping symlinks recursively.
/// The project-root `.dhi` directory is server-generated task state, so it is
/// excluded from snapshot metadata (`file_count`, `total_bytes`, upload cap).
pub fn scan_tree(dir: &Path) -> io::Result<ProjectTreeStats> {
    scan_tree_at(dir, true)
}

fn scan_tree_at(dir: &Path, skip_state_dir: bool) -> io::Result<ProjectTreeStats> {
    let mut stats = ProjectTreeStats::default();
    let metadata = match fs::symlink_metadata(dir) {
        Ok(metadata) => metadata,
        Err(err) if err.kind() == io::ErrorKind::NotFound => return Ok(stats),
        Err(err) => return Err(err),
    };
    if metadata.file_type().is_symlink() || !metadata.is_dir() {
        return Ok(stats);
    }
    for entry in fs::read_dir(dir)? {
        let entry = entry?;
        if skip_state_dir
            && entry
                .file_name()
                .to_string_lossy()
                .eq_ignore_ascii_case(WEB_PROJECT_STATE_DIR)
        {
            continue;
        }
        let file_type = entry.file_type()?;
        if file_type.is_symlink() {
            continue;
        }
        if file_type.is_dir() {
            let nested = scan_tree_at(&entry.path(), false)?;
            stats.file_count += nested.file_count;
            stats.total_bytes += nested.total_bytes;
        } else if file_type.is_file() {
            stats.file_count += 1;
            stats.total_bytes += entry.metadata()?.len();
        }
    }
    Ok(stats)
}

fn project_total_after_upload(current: u64, replaced: u64, incoming: u64) -> u64 {
    current.saturating_sub(replaced).saturating_add(incoming)
}

fn ensure_real_directory(dir: &Path) -> Result<(), UploadError> {
    let metadata = fs::symlink_metadata(dir).map_err(UploadError::Io)?;
    if metadata.file_type().is_symlink() || !metadata.is_dir() {
        return Err(UploadError::rejected(format!(
            "project directory is not a real directory: {}",
            dir.display()
        )));
    }
    Ok(())
}

/// The current size of a regular file, or zero when it does not exist. A
/// symlink or a directory at the destination is refused so a rename can never
/// replace something the project did not upload.
fn existing_file_size(path: &Path) -> Result<u64, UploadError> {
    match fs::symlink_metadata(path) {
        Ok(metadata) => {
            if metadata.file_type().is_symlink() {
                return Err(UploadError::rejected(format!(
                    "path must not be a symbolic link: {}",
                    path.display()
                )));
            }
            if !metadata.is_file() {
                return Err(UploadError::rejected(format!(
                    "path is not a regular file: {}",
                    path.display()
                )));
            }
            Ok(metadata.len())
        }
        Err(err) if err.kind() == io::ErrorKind::NotFound => Ok(0),
        Err(err) => Err(UploadError::Io(err)),
    }
}

/// Creates every parent directory below `project_dir`, refusing symlink
/// components, and returns the final file path.
fn prepare_destination(project_dir: &Path, relative: &Path) -> Result<PathBuf, UploadError> {
    ensure_real_directory(project_dir)?;
    let canonical_project = fs::canonicalize(project_dir).map_err(UploadError::Io)?;
    let mut current = project_dir.to_path_buf();
    let components: Vec<&std::ffi::OsStr> = relative
        .components()
        .filter_map(|component| match component {
            Component::Normal(part) => Some(part),
            _ => None,
        })
        .collect();
    let Some((file_name, parents)) = components.split_last() else {
        return Err(UploadError::rejected("path must not be empty"));
    };
    for part in parents {
        current.push(*part);
        match fs::symlink_metadata(&current) {
            Ok(metadata) => {
                if metadata.file_type().is_symlink() {
                    return Err(UploadError::rejected(format!(
                        "path component must not be a symbolic link: {}",
                        current.display()
                    )));
                }
                if !metadata.is_dir() {
                    return Err(UploadError::rejected(format!(
                        "path component is not a directory: {}",
                        current.display()
                    )));
                }
            }
            Err(err) if err.kind() == io::ErrorKind::NotFound => match fs::create_dir(&current) {
                Ok(()) => {}
                Err(err) if err.kind() == io::ErrorKind::AlreadyExists => {
                    let metadata = fs::symlink_metadata(&current).map_err(UploadError::Io)?;
                    if metadata.file_type().is_symlink() || !metadata.is_dir() {
                        return Err(UploadError::rejected(format!(
                            "path component is not a real directory: {}",
                            current.display()
                        )));
                    }
                }
                Err(err) => return Err(UploadError::Io(err)),
            },
            Err(err) => return Err(UploadError::Io(err)),
        }
    }
    let canonical_parent = fs::canonicalize(&current).map_err(UploadError::Io)?;
    if !canonical_parent.starts_with(&canonical_project) {
        return Err(UploadError::rejected(
            "path resolves outside the project directory",
        ));
    }
    let dest = current.join(*file_name);
    existing_file_size(&dest)?;
    Ok(dest)
}

fn atomic_write(dest: &Path, bytes: &[u8]) -> io::Result<()> {
    let temp = dest.with_file_name(format!(".dhi-upload-{}.tmp", Uuid::new_v4()));
    let write_result = (|| -> io::Result<()> {
        let mut file = OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&temp)?;
        file.write_all(bytes)?;
        file.sync_all()?;
        Ok(())
    })();
    if let Err(err) = write_result {
        let _ = fs::remove_file(&temp);
        return Err(err);
    }
    if let Err(err) = fs::rename(&temp, dest) {
        let _ = fs::remove_file(&temp);
        return Err(err);
    }
    Ok(())
}

/// Upload validation or storage failure.
#[derive(Debug)]
pub enum UploadError {
    /// The request violates a path or size rule; maps to `400`.
    Rejected(String),
    /// The server could not read or write the project tree; maps to `500`.
    Io(io::Error),
}

impl UploadError {
    fn rejected(message: impl Into<String>) -> Self {
        Self::Rejected(message.into())
    }
}

impl std::fmt::Display for UploadError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Rejected(message) => f.write_str(message),
            Self::Io(err) => write!(f, "{err}"),
        }
    }
}

impl std::error::Error for UploadError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Rejected(_) => None,
            Self::Io(err) => Some(err),
        }
    }
}

/// Output encoding of one result file.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum OutputEncoding {
    Utf8,
    Base64,
}

/// One result file returned by `GET /api/tasks/{id}/outputs`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct TaskOutputFile {
    pub path: String,
    pub content: String,
    pub encoding: OutputEncoding,
}

/// Reads `<project>/.dhi/<task-id>/` recursively and returns files relative to
/// the project root. Symlinks are skipped; oversized files are excluded.
pub fn read_task_outputs(project: &Path, task_id: Uuid) -> io::Result<Vec<TaskOutputFile>> {
    read_task_outputs_with_limits(
        project,
        task_id,
        OUTPUT_MAX_FILE_BYTES,
        OUTPUT_MAX_TOTAL_BYTES,
    )
}

fn read_task_outputs_with_limits(
    project: &Path,
    task_id: Uuid,
    max_file_bytes: u64,
    max_total_bytes: u64,
) -> io::Result<Vec<TaskOutputFile>> {
    let state_dir = project.join(WEB_PROJECT_STATE_DIR);
    match fs::symlink_metadata(&state_dir) {
        Ok(metadata) if metadata.file_type().is_symlink() || !metadata.is_dir() => {
            return Ok(Vec::new());
        }
        Ok(_) => {}
        Err(err) if err.kind() == io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(err) => return Err(err),
    }
    let root = state_dir.join(task_id.to_string());
    let mut files = Vec::new();
    let mut total = 0u64;
    let prefix = format!("{WEB_PROJECT_STATE_DIR}/{task_id}");
    read_output_dir(
        &root,
        &prefix,
        max_file_bytes,
        max_total_bytes,
        &mut total,
        &mut files,
    )?;
    Ok(files)
}

fn read_output_dir(
    dir: &Path,
    prefix: &str,
    max_file_bytes: u64,
    max_total_bytes: u64,
    total: &mut u64,
    files: &mut Vec<TaskOutputFile>,
) -> io::Result<()> {
    let metadata = match fs::symlink_metadata(dir) {
        Ok(metadata) => metadata,
        Err(err) if err.kind() == io::ErrorKind::NotFound => return Ok(()),
        Err(err) => return Err(err),
    };
    if metadata.file_type().is_symlink() || !metadata.is_dir() {
        return Ok(());
    }
    let mut entries = fs::read_dir(dir)?.collect::<Result<Vec<_>, io::Error>>()?;
    entries.sort_by_key(|entry| entry.file_name());
    for entry in entries {
        let file_type = match entry.file_type() {
            Ok(file_type) => file_type,
            Err(_) => continue,
        };
        if file_type.is_symlink() {
            continue;
        }
        let Some(name) = entry.file_name().to_str().map(str::to_owned) else {
            continue;
        };
        let path = entry.path();
        let relative = format!("{prefix}/{name}");
        if file_type.is_dir() {
            read_output_dir(
                &path,
                &relative,
                max_file_bytes,
                max_total_bytes,
                total,
                files,
            )?;
            continue;
        }
        if !file_type.is_file() {
            continue;
        }
        let size = match entry.metadata() {
            Ok(metadata) => metadata.len(),
            Err(_) => continue,
        };
        if size > max_file_bytes || total.saturating_add(size) > max_total_bytes {
            continue;
        }
        let file = match File::open(&path) {
            Ok(file) => file,
            Err(_) => continue,
        };
        let mut bytes = Vec::with_capacity(size as usize);
        if file
            .take(max_file_bytes.saturating_add(1))
            .read_to_end(&mut bytes)
            .is_err()
        {
            continue;
        }
        let bytes_len = bytes.len() as u64;
        if bytes_len > max_file_bytes || total.saturating_add(bytes_len) > max_total_bytes {
            continue;
        }
        *total += bytes_len;
        let (content, encoding) = match std::str::from_utf8(&bytes) {
            Ok(text) => (text.to_string(), OutputEncoding::Utf8),
            Err(_) => (base64_encode(&bytes), OutputEncoding::Base64),
        };
        files.push(TaskOutputFile {
            path: relative,
            content,
            encoding,
        });
    }
    Ok(())
}

/// Standard base64 with padding. Kept local so the server needs no extra
/// dependency for the small outputs payload.
pub fn base64_encode(bytes: &[u8]) -> String {
    const ALPHABET: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut out = String::with_capacity(bytes.len().div_ceil(3) * 4);
    for chunk in bytes.chunks(3) {
        let first = chunk[0] as u32;
        let second = chunk.get(1).copied().unwrap_or(0) as u32;
        let third = chunk.get(2).copied().unwrap_or(0) as u32;
        let combined = (first << 16) | (second << 8) | third;
        out.push(ALPHABET[((combined >> 18) & 0x3f) as usize] as char);
        out.push(ALPHABET[((combined >> 12) & 0x3f) as usize] as char);
        out.push(if chunk.len() > 1 {
            ALPHABET[((combined >> 6) & 0x3f) as usize] as char
        } else {
            '='
        });
        out.push(if chunk.len() > 2 {
            ALPHABET[(combined & 0x3f) as usize] as char
        } else {
            '='
        });
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    fn temp_root() -> (tempfile::TempDir, WebProjects) {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path().join("web-projects");
        let projects = WebProjects::parse(Some(root.to_str().unwrap())).unwrap();
        (temp, projects)
    }

    #[test]
    fn unset_or_empty_root_disables_web_projects() {
        assert!(!WebProjects::parse(None).unwrap().enabled());
        assert!(!WebProjects::parse(Some("   ")).unwrap().enabled());
    }

    #[test]
    fn root_must_be_absolute_and_a_real_directory() {
        assert!(WebProjects::parse(Some("relative/path")).is_err());
        let temp = tempfile::tempdir().unwrap();
        let file = temp.path().join("file");
        fs::write(&file, "x").unwrap();
        assert!(WebProjects::parse(Some(file.to_str().unwrap())).is_err());
    }

    #[test]
    fn root_is_created_and_canonicalized() {
        let temp = tempfile::tempdir().unwrap();
        let requested = temp.path().join("nested").join("web");
        let projects = WebProjects::parse(Some(requested.to_str().unwrap())).unwrap();
        assert!(requested.is_dir());
        let canonical = fs::canonicalize(&requested).unwrap();
        assert_eq!(projects.root(), Some(canonical.as_path()));
    }

    #[cfg(unix)]
    #[test]
    fn symlinked_root_is_refused() {
        use std::os::unix::fs::symlink;
        let temp = tempfile::tempdir().unwrap();
        let real = temp.path().join("real");
        fs::create_dir_all(&real).unwrap();
        let link = temp.path().join("link");
        symlink(&real, &link).unwrap();
        assert!(WebProjects::parse(Some(link.to_str().unwrap())).is_err());
    }

    #[test]
    fn names_are_trimmed_and_validated() {
        assert_eq!(validate_name("  My App  ").unwrap(), "My App");
        assert!(validate_name("").is_err());
        assert!(validate_name("   ").is_err());
        assert!(validate_name("bad\nname").is_err());
        assert!(validate_name(&"a".repeat(WEB_PROJECT_NAME_MAX_CHARS + 1)).is_err());
    }

    #[test]
    fn upload_paths_reject_unsafe_shapes() {
        for raw in [
            "",
            ".",
            "/etc/passwd",
            "../escape",
            "src/../escape",
            ".dhi/task/file",
            ".DHI/task/file",
            "a\\b",
            "C:/windows",
            "nul\0byte",
        ] {
            assert!(validate_upload_path(raw).is_err(), "{raw:?} was accepted");
        }
        let path = validate_upload_path("./src//app.ts").unwrap();
        assert_eq!(path.display, "src/app.ts");
        assert_eq!(path.relative, PathBuf::from("src/app.ts"));
    }

    #[test]
    fn upload_writes_text_and_binary_and_creates_parents() {
        let (_temp, projects) = temp_root();
        let id = Uuid::new_v4();
        let dir = projects.create_project_dir(id).unwrap();
        let text = validate_upload_path("src/app.ts").unwrap();
        projects.upload_file(&dir, &text, b"hello").unwrap();
        let binary = validate_upload_path("assets/logo.bin").unwrap();
        projects
            .upload_file(&dir, &binary, &[0, 159, 146, 150])
            .unwrap();
        assert_eq!(fs::read(dir.join("src/app.ts")).unwrap(), b"hello");
        assert_eq!(
            fs::read(dir.join("assets/logo.bin")).unwrap(),
            vec![0, 159, 146, 150]
        );
        let stats = scan_tree(&dir).unwrap();
        assert_eq!(stats.file_count, 2);
        assert_eq!(stats.total_bytes, 9);
    }

    #[test]
    fn upload_refuses_per_file_and_total_limits() {
        let (_temp, projects) = temp_root();
        let id = Uuid::new_v4();
        let dir = projects.create_project_dir(id).unwrap();
        let path = validate_upload_path("big.bin").unwrap();
        let too_big = vec![0u8; WEB_PROJECT_MAX_FILE_BYTES as usize + 1];
        assert!(matches!(
            projects.upload_file(&dir, &path, &too_big),
            Err(UploadError::Rejected(_))
        ));
        assert_eq!(
            project_total_after_upload(WEB_PROJECT_MAX_TOTAL_BYTES, 0, 1),
            WEB_PROJECT_MAX_TOTAL_BYTES + 1
        );

        // A sparse file reaches the total cap without allocating it.
        let huge = File::create(dir.join("huge.bin")).unwrap();
        huge.set_len(WEB_PROJECT_MAX_TOTAL_BYTES).unwrap();
        drop(huge);
        let tiny = validate_upload_path("tiny.txt").unwrap();
        assert!(matches!(
            projects.upload_file(&dir, &tiny, b"x"),
            Err(UploadError::Rejected(_))
        ));
    }

    #[cfg(unix)]
    #[test]
    fn upload_refuses_a_symlink_component() {
        use std::os::unix::fs::symlink;
        let (_temp, projects) = temp_root();
        let id = Uuid::new_v4();
        let dir = projects.create_project_dir(id).unwrap();
        let outside = dir.parent().unwrap().join("outside");
        fs::create_dir_all(&outside).unwrap();
        symlink(&outside, dir.join("linked")).unwrap();
        let path = validate_upload_path("linked/file.txt").unwrap();
        assert!(matches!(
            projects.upload_file(&dir, &path, b"x"),
            Err(UploadError::Rejected(_))
        ));
        assert!(!outside.join("file.txt").exists());
    }

    #[test]
    fn agents_md_is_detected_from_the_project_root() {
        let (_temp, projects) = temp_root();
        let id = Uuid::new_v4();
        let dir = projects.create_project_dir(id).unwrap();
        let record = WebProjectRecord {
            id,
            name: "app".to_string(),
            root_path: projects.root().unwrap().to_string_lossy().into_owned(),
            created_at: Utc::now(),
            updated_at: Utc::now(),
        };
        assert!(!project_info(&record).agents_md_present);
        fs::write(dir.join("AGENTS.md"), "follow the rules").unwrap();
        let info = project_info(&record);
        assert!(info.agents_md_present);
        assert_eq!(info.file_count, 1);
        assert_eq!(info.total_bytes, 16);
    }

    #[test]
    fn snapshot_stats_skip_the_generated_state_directory() {
        let (_temp, projects) = temp_root();
        let id = Uuid::new_v4();
        let dir = projects.create_project_dir(id).unwrap();
        fs::create_dir_all(dir.join("src")).unwrap();
        fs::write(dir.join("src/app.ts"), "hi").unwrap();
        let task_id = Uuid::new_v4();
        let task_dir = dir.join(WEB_PROJECT_STATE_DIR).join(task_id.to_string());
        fs::create_dir_all(&task_dir).unwrap();
        fs::write(task_dir.join("generated.bin"), [1, 2, 3, 4]).unwrap();
        let record = WebProjectRecord {
            id,
            name: "app".to_string(),
            root_path: projects.root().unwrap().to_string_lossy().into_owned(),
            created_at: Utc::now(),
            updated_at: Utc::now(),
        };

        let info = project_info(&record);
        assert_eq!(info.file_count, 1);
        assert_eq!(info.total_bytes, 2);
        let stats = scan_tree(&dir).unwrap();
        assert_eq!(stats.file_count, 1);
        assert_eq!(stats.total_bytes, 2);
    }

    #[test]
    fn outputs_cover_text_binary_nested_and_empty() {
        let temp = tempfile::tempdir().unwrap();
        let task_id = Uuid::new_v4();
        let task_dir = temp
            .path()
            .join(WEB_PROJECT_STATE_DIR)
            .join(task_id.to_string());
        fs::create_dir_all(task_dir.join("nested")).unwrap();
        fs::write(task_dir.join("notes.txt"), "hello").unwrap();
        fs::write(task_dir.join("nested/raw.bin"), [0xff, 0x00, 0xfe]).unwrap();

        let files = read_task_outputs(temp.path(), task_id).unwrap();
        assert_eq!(files.len(), 2);
        assert_eq!(files[0].path, format!(".dhi/{task_id}/nested/raw.bin"));
        assert_eq!(files[0].encoding, OutputEncoding::Base64);
        assert_eq!(files[0].content, "/wD+");
        assert_eq!(files[1].path, format!(".dhi/{task_id}/notes.txt"));
        assert_eq!(files[1].encoding, OutputEncoding::Utf8);
        assert_eq!(files[1].content, "hello");

        assert!(
            read_task_outputs(temp.path(), Uuid::new_v4())
                .unwrap()
                .is_empty()
        );
    }

    #[cfg(unix)]
    #[test]
    fn outputs_skip_symlinks() {
        use std::os::unix::fs::symlink;
        let temp = tempfile::tempdir().unwrap();
        let task_id = Uuid::new_v4();
        let task_dir = temp
            .path()
            .join(WEB_PROJECT_STATE_DIR)
            .join(task_id.to_string());
        fs::create_dir_all(&task_dir).unwrap();
        let outside = temp.path().join("secret.txt");
        fs::write(&outside, "secret").unwrap();
        symlink(&outside, task_dir.join("link.txt")).unwrap();
        assert!(read_task_outputs(temp.path(), task_id).unwrap().is_empty());
    }

    #[cfg(unix)]
    #[test]
    fn outputs_skip_a_symlinked_state_directory() {
        use std::os::unix::fs::symlink;
        let temp = tempfile::tempdir().unwrap();
        let task_id = Uuid::new_v4();
        let outside = temp.path().join("outside");
        let outside_task = outside
            .join(WEB_PROJECT_STATE_DIR)
            .join(task_id.to_string());
        fs::create_dir_all(&outside_task).unwrap();
        fs::write(outside_task.join("leak.txt"), "secret").unwrap();
        symlink(
            outside.join(WEB_PROJECT_STATE_DIR),
            temp.path().join(WEB_PROJECT_STATE_DIR),
        )
        .unwrap();
        assert!(read_task_outputs(temp.path(), task_id).unwrap().is_empty());
    }

    #[test]
    fn outputs_exclude_oversized_files_and_cap_the_total() {
        let temp = tempfile::tempdir().unwrap();
        let task_id = Uuid::new_v4();
        let task_dir = temp
            .path()
            .join(WEB_PROJECT_STATE_DIR)
            .join(task_id.to_string());
        fs::create_dir_all(&task_dir).unwrap();
        fs::write(task_dir.join("a.txt"), "12345").unwrap();
        fs::write(task_dir.join("b.txt"), "67890").unwrap();
        let files = read_task_outputs_with_limits(temp.path(), task_id, 8, 6).unwrap();
        assert_eq!(files.len(), 1);
        assert_eq!(files[0].content, "12345");
    }

    #[test]
    fn base64_matches_standard_padding() {
        assert_eq!(base64_encode(b""), "");
        assert_eq!(base64_encode(b"f"), "Zg==");
        assert_eq!(base64_encode(b"fo"), "Zm8=");
        assert_eq!(base64_encode(b"foo"), "Zm9v");
        assert_eq!(base64_encode(b"foobar"), "Zm9vYmFy");
    }
}

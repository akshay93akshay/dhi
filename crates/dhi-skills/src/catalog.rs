//! The skill catalog.
//!
//! A skill is one directory under the configured root:
//! `skills/<directory>/SKILL.md`. Frontmatter is optional and intentionally
//! parses only a small YAML subset: `key: value`, `key: [a, b]`, and block
//! lists with `- item`. Malformed skills are logged and skipped so a broken
//! file never keeps the server from booting.

use std::collections::HashSet;
use std::fs;
use std::path::{Path, PathBuf};

use tracing::warn;

/// The largest catalog the runtime loads. Extra skill directories are skipped
/// with a warning.
pub const MAX_SKILLS: usize = 128;
/// The largest `SKILL.md` accepted, in bytes.
pub const MAX_SKILL_BYTES: u64 = 64 * 1024;

/// The recognized run roles. `specialist:<name>` is recognized separately.
pub const BASE_ROLES: [&str; 4] = ["planner", "worker", "reporter", "digest"];

/// One loaded skill.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Skill {
    pub name: String,
    pub description: String,
    /// `None` means every recognized role; `Some([])` means nobody. The
    /// authored strings are kept as-is so an unknown role simply matches no
    /// caller.
    pub visible_to: Option<Vec<String>>,
    pub instructions: String,
}

impl Skill {
    /// Whether `role` may see and load this skill. Unknown roles see nothing.
    pub fn is_visible_to(&self, role: &str) -> bool {
        if !is_recognized_role(role) {
            return false;
        }
        match &self.visible_to {
            None => true,
            Some(roles) => roles.iter().any(|candidate| candidate == role),
        }
    }
}

/// The process-local catalog, in stable name order.
#[derive(Debug, Clone, Default)]
pub struct SkillCatalog {
    skills: Vec<Skill>,
}

impl SkillCatalog {
    /// Loads `root/<name>/SKILL.md`, skipping malformed entries. A missing
    /// root is an empty catalog.
    pub fn from_dir(root: impl AsRef<Path>) -> Self {
        let root = root.as_ref();
        let mut skills = Vec::new();
        let mut seen = HashSet::new();
        let mut directories = match skill_directories(root) {
            Ok(directories) => directories,
            Err(error) => {
                warn!(
                    path = %root.display(),
                    %error,
                    "skill catalog directory could not be read; using an empty catalog"
                );
                return Self::default();
            }
        };
        directories.sort();

        for directory in directories {
            if skills.len() >= MAX_SKILLS {
                warn!(
                    path = %root.display(),
                    limit = MAX_SKILLS,
                    "skill catalog limit reached; remaining skill directories were skipped"
                );
                break;
            }
            if !directory.is_dir() {
                continue;
            }
            let Some(directory_name) = directory.file_name().and_then(|name| name.to_str()) else {
                warn!(
                    path = %directory.display(),
                    "skill directory name is not valid UTF-8; skipping"
                );
                continue;
            };
            let file = directory.join("SKILL.md");
            let raw = match read_skill_file(&file) {
                Ok(raw) => raw,
                Err(error) => {
                    warn!(
                        path = %file.display(),
                        %error,
                        "malformed skill skipped"
                    );
                    continue;
                }
            };
            let skill = match parse_skill(directory_name, &raw) {
                Ok(skill) => skill,
                Err(error) => {
                    warn!(
                        path = %file.display(),
                        %error,
                        "malformed skill skipped"
                    );
                    continue;
                }
            };
            if !seen.insert(skill.name.clone()) {
                warn!(
                    path = %file.display(),
                    skill = %skill.name,
                    "duplicate skill name skipped; the first definition wins"
                );
                continue;
            }
            skills.push(skill);
        }

        skills.sort_by(|left, right| left.name.cmp(&right.name));
        Self { skills }
    }

    pub fn is_empty(&self) -> bool {
        self.skills.is_empty()
    }

    pub fn len(&self) -> usize {
        self.skills.len()
    }

    /// Every loaded skill, sorted by name.
    pub fn skills(&self) -> &[Skill] {
        &self.skills
    }

    /// A skill by exact name.
    pub fn get(&self, name: &str) -> Option<&Skill> {
        self.skills.iter().find(|skill| skill.name == name)
    }

    /// The skills visible to `role`, in catalog order.
    pub fn visible_to(&self, role: &str) -> Vec<&Skill> {
        self.skills
            .iter()
            .filter(|skill| skill.is_visible_to(role))
            .collect()
    }
}

/// Whether `role` is one of the runtime's recognized roles. A specialist role
/// must carry a safe, non-empty specialist name.
pub fn is_recognized_role(role: &str) -> bool {
    if BASE_ROLES.contains(&role) {
        return true;
    }
    role.strip_prefix("specialist:").is_some_and(is_safe_name)
}

/// `[A-Za-z0-9_-]{1,64}`.
pub fn is_safe_name(name: &str) -> bool {
    (1..=64).contains(&name.len())
        && name
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || byte == b'_' || byte == b'-')
}

fn skill_directories(root: &Path) -> Result<Vec<PathBuf>, std::io::Error> {
    let entries = match fs::read_dir(root) {
        Ok(entries) => entries,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(error) => return Err(error),
    };
    let mut directories = Vec::new();
    for entry in entries {
        let entry = match entry {
            Ok(entry) => entry,
            Err(error) => {
                warn!(
                    path = %root.display(),
                    %error,
                    "skill directory entry could not be read; skipping"
                );
                continue;
            }
        };
        directories.push(entry.path());
    }
    Ok(directories)
}

fn read_skill_file(path: &Path) -> Result<String, String> {
    let metadata =
        fs::metadata(path).map_err(|error| format!("SKILL.md could not be read: {error}"))?;
    if !metadata.is_file() {
        return Err("SKILL.md is not a regular file".to_string());
    }
    if metadata.len() > MAX_SKILL_BYTES {
        return Err(format!(
            "SKILL.md is {} bytes; the limit is {} bytes",
            metadata.len(),
            MAX_SKILL_BYTES
        ));
    }
    let bytes = fs::read(path).map_err(|error| format!("SKILL.md could not be read: {error}"))?;
    if bytes.len() as u64 > MAX_SKILL_BYTES {
        return Err(format!(
            "SKILL.md is {} bytes; the limit is {} bytes",
            bytes.len(),
            MAX_SKILL_BYTES
        ));
    }
    String::from_utf8(bytes).map_err(|error| format!("SKILL.md is not valid UTF-8: {error}"))
}

#[derive(Default)]
struct Frontmatter {
    name: Option<String>,
    description: Option<String>,
    visible_to: Option<Vec<String>>,
}

#[derive(Clone, Copy)]
enum ListKey {
    VisibleTo,
    Ignore,
}

fn parse_skill(directory_name: &str, raw: &str) -> Result<Skill, String> {
    let raw = raw.strip_prefix('\u{feff}').unwrap_or(raw);
    let normalized = raw.replace("\r\n", "\n").replace('\r', "\n");
    let mut lines = normalized.split('\n');
    let first = lines.next().unwrap_or_default();

    let (frontmatter, body) = if first.trim_end() == "---" {
        let mut frontmatter_lines = Vec::new();
        let mut body_lines = Vec::new();
        let mut closed = false;
        for line in lines {
            if !closed {
                if line.trim_end() == "---" {
                    closed = true;
                } else {
                    frontmatter_lines.push(line);
                }
            } else {
                body_lines.push(line);
            }
        }
        if !closed {
            return Err("frontmatter is not closed".to_string());
        }
        (
            parse_frontmatter(&frontmatter_lines.join("\n"))?,
            body_lines.join("\n"),
        )
    } else {
        (Frontmatter::default(), normalized)
    };

    let name = frontmatter
        .name
        .as_deref()
        .map(str::trim)
        .filter(|name| !name.is_empty())
        .unwrap_or(directory_name)
        .to_string();
    if !is_safe_name(&name) {
        return Err(format!(
            "skill name {name:?} is not a safe name ([A-Za-z0-9_-]{{1,64}})"
        ));
    }

    let body = body.trim();
    let description = frontmatter
        .description
        .as_deref()
        .map(str::trim)
        .filter(|description| !description.is_empty())
        .map(str::to_string)
        .unwrap_or_else(|| first_body_line(body).to_string());

    Ok(Skill {
        name,
        description,
        visible_to: frontmatter.visible_to,
        instructions: body.to_string(),
    })
}

fn parse_frontmatter(text: &str) -> Result<Frontmatter, String> {
    let mut frontmatter = Frontmatter::default();
    let mut pending: Option<ListKey> = None;

    for (index, raw_line) in text.split('\n').enumerate() {
        let line = raw_line.trim_end();
        let trimmed = line.trim();
        if trimmed.is_empty() || trimmed.starts_with('#') {
            continue;
        }

        if let Some(item) = trimmed.strip_prefix('-')
            && (item.is_empty() || item.starts_with(char::is_whitespace))
        {
            let item = unquote(item);
            match pending {
                Some(ListKey::VisibleTo) => {
                    frontmatter
                        .visible_to
                        .get_or_insert_with(Vec::new)
                        .push(item);
                }
                Some(ListKey::Ignore) => {}
                None => {
                    return Err(format!(
                        "frontmatter line {} is a list item without a list key",
                        index + 1
                    ));
                }
            }
            continue;
        }

        let Some((key, value)) = trimmed.split_once(':') else {
            return Err(format!(
                "frontmatter line {} is not a `key: value` pair",
                index + 1
            ));
        };
        let key = key.trim();
        let value = value.trim();
        pending = None;

        match key {
            "name" => frontmatter.name = Some(unquote(value)),
            "description" => frontmatter.description = Some(unquote(value)),
            "visible_to" => {
                if value.is_empty() {
                    frontmatter.visible_to = Some(Vec::new());
                    pending = Some(ListKey::VisibleTo);
                } else {
                    frontmatter.visible_to = Some(parse_list(value)?);
                }
            }
            "" => {
                return Err(format!("frontmatter line {} has an empty key", index + 1));
            }
            _ => {
                // Unknown keys are ignored: frontmatter should be able to carry
                // metadata this block does not consume.
                if value.is_empty() {
                    pending = Some(ListKey::Ignore);
                }
            }
        }
    }

    Ok(frontmatter)
}

fn parse_list(value: &str) -> Result<Vec<String>, String> {
    let value = value.trim();
    if let Some(inner) = value.strip_prefix('[') {
        let inner = inner
            .strip_suffix(']')
            .ok_or_else(|| format!("inline list {value:?} is not closed"))?;
        if inner.trim().is_empty() {
            return Ok(Vec::new());
        }
        return Ok(inner
            .split(',')
            .map(unquote)
            .filter(|item| !item.is_empty())
            .collect());
    }

    let item = unquote(value);
    if item.is_empty() {
        Ok(Vec::new())
    } else {
        Ok(vec![item])
    }
}

fn unquote(value: &str) -> String {
    let value = value.trim();
    if value.len() >= 2 {
        let bytes = value.as_bytes();
        let first = bytes[0];
        let last = bytes[value.len() - 1];
        if (first == b'"' && last == b'"') || (first == b'\'' && last == b'\'') {
            return value[1..value.len() - 1]
                .replace("\\\"", "\"")
                .replace("\\'", "'")
                .replace("\\\\", "\\");
        }
    }
    value.to_string()
}

fn first_body_line(body: &str) -> &str {
    body.lines()
        .map(str::trim)
        .find(|line| !line.is_empty())
        .unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use tempfile::TempDir;

    fn write_skill(root: &Path, directory: &str, contents: &str) {
        let directory = root.join(directory);
        fs::create_dir_all(&directory).expect("skill directory");
        fs::write(directory.join("SKILL.md"), contents).expect("SKILL.md");
    }

    fn temp_root() -> TempDir {
        tempfile::tempdir().expect("temporary skills directory")
    }

    #[test]
    fn valid_frontmatter_parses_every_supported_field() {
        let root = temp_root();
        write_skill(
            root.path(),
            "rust-review",
            "---\n\
             name: rust-review\n\
             description: Review Rust changes\n\
             visible_to: [worker, specialist:researcher]\n\
             ---\n\
             Instructions...\n",
        );

        let catalog = SkillCatalog::from_dir(root.path());
        assert_eq!(catalog.len(), 1);
        let skill = catalog.get("rust-review").expect("loaded skill");
        assert_eq!(skill.description, "Review Rust changes");
        assert_eq!(
            skill.visible_to.as_deref(),
            Some(&["worker".to_string(), "specialist:researcher".to_string()][..])
        );
        assert_eq!(skill.instructions, "Instructions...");
    }

    #[test]
    fn block_lists_parse_for_visibility() {
        let root = temp_root();
        write_skill(
            root.path(),
            "blocky",
            "---\n\
             description: Block lists\n\
             visible_to:\n\
               - worker\n\
               - reporter\n\
             ---\n\
             Body\n",
        );

        let catalog = SkillCatalog::from_dir(root.path());
        let skill = catalog.get("blocky").expect("loaded skill");
        assert_eq!(
            skill.visible_to.as_deref(),
            Some(&["worker".to_string(), "reporter".to_string()][..])
        );
    }

    #[test]
    fn a_missing_directory_is_an_empty_catalog() {
        let root = temp_root();
        let catalog = SkillCatalog::from_dir(root.path().join("missing"));
        assert!(catalog.is_empty());
        assert_eq!(catalog.len(), 0);
    }

    #[test]
    fn malformed_skills_are_skipped_not_fatal() {
        let root = temp_root();
        write_skill(root.path(), "unclosed", "---\nname: unclosed\n");
        write_skill(root.path(), "bad.name", "No frontmatter.\n");
        write_skill(
            root.path(),
            "bad-liar",
            "---\nname: has a space\n---\nbody\n",
        );
        fs::create_dir_all(root.path().join("no-file")).expect("directory");
        write_skill(
            root.path(),
            "good",
            "---\ndescription: Still here\n---\nGood instructions.\n",
        );

        let catalog = SkillCatalog::from_dir(root.path());
        assert_eq!(catalog.len(), 1);
        assert!(catalog.get("good").is_some());
    }

    #[test]
    fn duplicate_names_keep_the_first_sorted_directory() {
        let root = temp_root();
        write_skill(
            root.path(),
            "a-first",
            "---\nname: shared\ndescription: first\n---\nfirst body\n",
        );
        write_skill(
            root.path(),
            "b-second",
            "---\nname: shared\ndescription: second\n---\nsecond body\n",
        );

        let catalog = SkillCatalog::from_dir(root.path());
        assert_eq!(catalog.len(), 1);
        let skill = catalog.get("shared").expect("first definition wins");
        assert_eq!(skill.description, "first");
        assert_eq!(skill.instructions, "first body");
    }

    #[test]
    fn no_frontmatter_uses_the_directory_and_first_body_line() {
        let root = temp_root();
        write_skill(
            root.path(),
            "plain-skill",
            "\n\nFirst non-empty line\nSecond line\n",
        );

        let catalog = SkillCatalog::from_dir(root.path());
        let skill = catalog.get("plain-skill").expect("loaded skill");
        assert_eq!(skill.name, "plain-skill");
        assert_eq!(skill.description, "First non-empty line");
        assert_eq!(skill.visible_to, None);
        assert_eq!(skill.instructions, "First non-empty line\nSecond line");
    }

    #[test]
    fn oversized_skills_are_skipped() {
        let root = temp_root();
        let oversized = "x".repeat(MAX_SKILL_BYTES as usize + 1);
        write_skill(root.path(), "oversized", &oversized);
        write_skill(root.path(), "small", "Small body\n");

        let catalog = SkillCatalog::from_dir(root.path());
        assert_eq!(catalog.len(), 1);
        assert!(catalog.get("small").is_some());
        assert!(catalog.get("oversized").is_none());
    }

    #[test]
    fn the_catalog_stops_at_the_configured_limit() {
        let root = temp_root();
        for index in 0..(MAX_SKILLS + 2) {
            write_skill(
                root.path(),
                &format!("skill-{index:03}"),
                &format!("Description {index}\n"),
            );
        }

        let catalog = SkillCatalog::from_dir(root.path());
        assert_eq!(catalog.len(), MAX_SKILLS);
    }

    #[test]
    fn empty_visibility_is_explicitly_nobody() {
        let root = temp_root();
        write_skill(
            root.path(),
            "hidden",
            "---\nvisible_to: []\ndescription: hidden\n---\nbody\n",
        );

        let catalog = SkillCatalog::from_dir(root.path());
        let skill = catalog.get("hidden").expect("loaded skill");
        assert_eq!(skill.visible_to.as_deref(), Some(&[][..]));
        assert!(!skill.is_visible_to("worker"));
    }
}

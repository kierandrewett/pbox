use anyhow::{Context, Result, anyhow, bail};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, Output, Stdio};

const DEFAULT_REPOSITORY: &str = "https://github.com/kierandrewett/pbox-recipes.git";
const DEFAULT_REFERENCE: &str = "main";

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "kebab-case")]
pub enum RecipeKind {
    Playbook,
    Role,
}

impl RecipeKind {
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Playbook => "playbook",
            Self::Role => "role",
        }
    }
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
#[serde(default)]
pub struct RecipeMetadata {
    pub description: Option<String>,
    pub requires: Vec<String>,
    pub supports: Vec<String>,
    pub capabilities: Vec<String>,
    pub resources: ResourceRecommendations,
    pub desktop: Option<DesktopMetadata>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
#[serde(default)]
pub struct ResourceRecommendations {
    pub cores: Option<u64>,
    pub memory: Option<u64>,
    pub disk: Option<String>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
#[serde(default)]
pub struct DesktopMetadata {
    pub protocol: Option<String>,
    pub host: Option<String>,
    pub port: Option<u16>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct Recipe {
    pub id: String,
    pub kind: RecipeKind,
    pub path: String,
    pub metadata: RecipeMetadata,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct RecipeCatalog {
    pub repository: String,
    pub reference: String,
    pub revision: String,
    pub recipes: Vec<Recipe>,
}

#[derive(Debug, Clone)]
pub struct RecipeRepository {
    repository: String,
    reference: String,
    cache_dir: PathBuf,
}

impl RecipeRepository {
    pub fn new(repository: Option<&str>, reference: &str) -> Result<Self> {
        let repository = repository.unwrap_or(DEFAULT_REPOSITORY).trim();
        if repository.is_empty() {
            bail!("recipe repository cannot be empty");
        }
        let repository = normalise_repository(repository);
        let reference = if reference.trim().is_empty() {
            DEFAULT_REFERENCE.to_owned()
        } else {
            reference.trim().to_owned()
        };
        let cache_dir = default_cache_dir(&repository)?;
        Ok(Self {
            repository,
            reference,
            cache_dir,
        })
    }

    #[cfg(test)]
    pub fn with_cache_dir(
        repository: Option<&str>,
        reference: &str,
        cache_dir: impl Into<PathBuf>,
    ) -> Result<Self> {
        let mut result = Self::new(repository, reference)?;
        result.cache_dir = cache_dir.into();
        Ok(result)
    }

    pub fn sync(&self) -> Result<RecipeCatalog> {
        if self.cache_dir.join(".git").is_dir() {
            run_git(
                &[
                    "-C",
                    self.cache_dir
                        .to_str()
                        .ok_or_else(|| anyhow!("recipe cache path is not UTF-8"))?,
                    "fetch",
                    "--depth",
                    "1",
                    "origin",
                    &self.reference,
                ],
                &self.repository,
            )?;
            run_git(
                &[
                    "-C",
                    self.cache_dir
                        .to_str()
                        .ok_or_else(|| anyhow!("recipe cache path is not UTF-8"))?,
                    "checkout",
                    "--force",
                    "FETCH_HEAD",
                ],
                &self.repository,
            )?;
        } else {
            if self.cache_dir.exists() {
                bail!(
                    "recipe cache path exists but is not a Git checkout: {}",
                    self.cache_dir.display()
                );
            }
            if let Some(parent) = self.cache_dir.parent() {
                fs::create_dir_all(parent).with_context(|| {
                    format!("create recipe cache directory {}", parent.display())
                })?;
            }
            let cache = self
                .cache_dir
                .to_str()
                .ok_or_else(|| anyhow!("recipe cache path is not UTF-8"))?;
            run_git(
                &[
                    "clone",
                    "--depth",
                    "1",
                    "--branch",
                    &self.reference,
                    &self.repository,
                    cache,
                ],
                &self.repository,
            )?;
        }
        self.discover()
    }

    pub fn discover(&self) -> Result<RecipeCatalog> {
        discover_path(&self.cache_dir, &self.repository, &self.reference)
    }
}

pub fn discover_path(root: &Path, repository: &str, reference: &str) -> Result<RecipeCatalog> {
    if !root.is_dir() {
        bail!(
            "recipe repository directory does not exist: {}",
            root.display()
        );
    }
    let manifest = read_manifest(root)?;
    let mut candidates = Vec::new();
    collect_candidates(root, root, &mut candidates)?;

    let mut recipes = Vec::new();
    let mut paths = BTreeSet::new();
    for candidate in candidates {
        let id = candidate.id.clone();
        let metadata = manifest
            .recipes
            .get(&id)
            .map(|entry| entry.metadata.clone())
            .unwrap_or_default();
        paths.insert(candidate.path.clone());
        recipes.push(Recipe {
            id,
            kind: candidate.kind,
            path: candidate.path,
            metadata,
        });
    }

    for (id, entry) in manifest.recipes {
        if recipes.iter().any(|recipe| recipe.id == id) {
            continue;
        }
        let relative_path = entry.path.ok_or_else(|| {
            anyhow!("recipe metadata for {id} must include a path because it was not discovered")
        })?;
        let path = normalise_relative_path(&relative_path)?;
        if !root.join(&path).exists() {
            bail!("recipe {id} points to missing path {}", path.display());
        }
        let path_text = path_string(&path);
        if paths.contains(&path_text) {
            continue;
        }
        let kind = recipe_kind(root, &path)?;
        paths.insert(path_text);
        recipes.push(Recipe {
            id,
            kind,
            path: path_string(&path),
            metadata: entry.metadata,
        });
    }

    recipes.sort_by(|left, right| left.id.cmp(&right.id));
    for pair in recipes.windows(2) {
        if pair[0].id == pair[1].id {
            bail!(
                "duplicate recipe id {} for {} and {}",
                pair[0].id,
                pair[0].path,
                pair[1].path
            );
        }
    }

    let revision = git_revision(root).unwrap_or_else(|_| "working-tree".to_owned());
    Ok(RecipeCatalog {
        repository: repository.to_owned(),
        reference: reference.to_owned(),
        revision,
        recipes,
    })
}

#[derive(Debug, Clone)]
struct Candidate {
    id: String,
    kind: RecipeKind,
    path: String,
}

#[derive(Debug, Clone, Default, Deserialize)]
#[serde(default)]
struct RecipeManifest {
    recipes: BTreeMap<String, RecipeManifestEntry>,
}

#[derive(Debug, Clone, Default, Deserialize)]
#[serde(default)]
struct RecipeManifestEntry {
    path: Option<String>,
    #[serde(flatten)]
    metadata: RecipeMetadata,
}

fn read_manifest(root: &Path) -> Result<RecipeManifest> {
    for name in ["pbox.yml", "pbox.yaml"] {
        let path = root.join(name);
        if !path.is_file() {
            continue;
        }
        let contents = fs::read_to_string(&path)
            .with_context(|| format!("read recipe metadata {}", path.display()))?;
        return serde_yaml::from_str(&contents)
            .with_context(|| format!("parse recipe metadata {}", path.display()));
    }
    Ok(RecipeManifest::default())
}

fn collect_candidates(
    root: &Path,
    directory: &Path,
    candidates: &mut Vec<Candidate>,
) -> Result<()> {
    let mut entries = fs::read_dir(directory)
        .with_context(|| format!("read recipe directory {}", directory.display()))?
        .collect::<std::result::Result<Vec<_>, _>>()?;
    entries.sort_by_key(|entry| entry.file_name());
    for entry in entries {
        let path = entry.path();
        if path.file_name().is_some_and(|name| name == ".git") {
            continue;
        }
        if path.is_dir() {
            collect_candidates(root, &path, candidates)?;
            continue;
        }
        if !path.is_file() {
            continue;
        }
        let relative = path
            .strip_prefix(root)
            .with_context(|| format!("resolve recipe path {}", path.display()))?;
        let components = relative.components().collect::<Vec<_>>();
        if components
            .first()
            .is_some_and(|component| component.as_os_str() == "roles")
        {
            if components.len() == 4
                && components[2].as_os_str() == "tasks"
                && matches!(
                    components[3].as_os_str().to_str(),
                    Some("main.yml" | "main.yaml")
                )
            {
                let role = components[1]
                    .as_os_str()
                    .to_str()
                    .ok_or_else(|| anyhow!("recipe role path is not UTF-8"))?;
                candidates.push(Candidate {
                    id: role.to_owned(),
                    kind: RecipeKind::Role,
                    path: path_string(relative),
                });
            }
            continue;
        }
        if !is_yaml_file(&path) || is_metadata_file(&path) || is_ansible_support_file(relative) {
            continue;
        }
        let id = playbook_id(relative)?;
        candidates.push(Candidate {
            id,
            kind: RecipeKind::Playbook,
            path: path_string(relative),
        });
    }
    Ok(())
}

fn recipe_kind(root: &Path, path: &Path) -> Result<RecipeKind> {
    let absolute = root.join(path);
    if absolute.is_file() {
        if !is_yaml_file(&absolute) {
            bail!(
                "recipe path is not an Ansible YAML file: {}",
                path.display()
            );
        }
        return Ok(RecipeKind::Playbook);
    }
    if absolute.join("tasks/main.yml").is_file() || absolute.join("tasks/main.yaml").is_file() {
        return Ok(RecipeKind::Role);
    }
    bail!(
        "recipe path is not a playbook or Ansible role: {}",
        path.display()
    )
}

fn playbook_id(relative: &Path) -> Result<String> {
    let mut components = relative.components();
    let first = components.next();
    let path = if first.is_some_and(|component| component.as_os_str() == "playbooks") {
        components.collect::<PathBuf>()
    } else {
        relative.to_owned()
    };
    let mut value = path_string(&path);
    for suffix in [".yaml", ".yml"] {
        if let Some(stripped) = value.strip_suffix(suffix) {
            value = stripped.to_owned();
            break;
        }
    }
    if value.is_empty() {
        bail!("recipe playbook path has no name: {}", relative.display());
    }
    Ok(value)
}

fn is_yaml_file(path: &Path) -> bool {
    matches!(
        path.extension().and_then(|extension| extension.to_str()),
        Some("yml" | "yaml")
    )
}

fn is_metadata_file(path: &Path) -> bool {
    matches!(
        path.file_name().and_then(|name| name.to_str()),
        Some("pbox.yml" | "pbox.yaml")
    )
}

fn is_ansible_support_file(relative: &Path) -> bool {
    let name = relative.file_name().and_then(|name| name.to_str());
    matches!(
        name,
        Some("requirements.yml" | "requirements.yaml" | "galaxy.yml")
    )
}

fn normalise_relative_path(value: &str) -> Result<PathBuf> {
    let path = PathBuf::from(value);
    if path.is_absolute()
        || path
            .components()
            .any(|component| component.as_os_str() == "..")
    {
        bail!("recipe metadata path must stay inside the repository: {value}");
    }
    if value.trim().is_empty() {
        bail!("recipe metadata path cannot be empty");
    }
    Ok(path)
}

fn path_string(path: &Path) -> String {
    path.components()
        .map(|component| component.as_os_str().to_string_lossy())
        .collect::<Vec<_>>()
        .join("/")
}

fn normalise_repository(repository: &str) -> String {
    let trimmed = repository.trim();
    if trimmed.matches('/').count() == 1
        && !trimmed.contains("://")
        && !trimmed.starts_with("git@")
        && !trimmed.starts_with('.')
        && !trimmed.starts_with('/')
    {
        return format!("https://github.com/{trimmed}.git");
    }
    trimmed.to_owned()
}

fn default_cache_dir(repository: &str) -> Result<PathBuf> {
    let mut digest = Sha256::new();
    digest.update(repository.as_bytes());
    let hash = format!("{:x}", digest.finalize());
    let root = dirs::cache_dir()
        .unwrap_or_else(|| PathBuf::from("."))
        .join("pbox")
        .join("recipes");
    Ok(root.join(hash))
}

fn run_git(arguments: &[&str], repository: &str) -> Result<Output> {
    let output = Command::new("git")
        .args(arguments)
        .stdin(Stdio::null())
        .output()
        .context("run git for recipe repository")?;
    if output.status.success() {
        return Ok(output);
    }
    let detail = String::from_utf8_lossy(&output.stderr)
        .trim()
        .replace(repository, "<recipe repository>");
    if detail.is_empty() {
        bail!(
            "git recipe repository operation failed with {}",
            output.status
        );
    }
    bail!("git recipe repository operation failed: {detail}");
}

fn git_revision(root: &Path) -> Result<String> {
    let output = Command::new("git")
        .args([
            "-C",
            root.to_str()
                .ok_or_else(|| anyhow!("recipe path is not UTF-8"))?,
            "rev-parse",
            "HEAD",
        ])
        .stdin(Stdio::null())
        .output()
        .context("read recipe repository revision")?;
    if !output.status.success() {
        bail!("recipe repository revision is unavailable");
    }
    Ok(String::from_utf8(output.stdout)?.trim().to_owned())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::{SystemTime, UNIX_EPOCH};

    fn temporary_directory() -> PathBuf {
        let suffix = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("system clock")
            .as_nanos();
        std::env::temp_dir().join(format!("pbox-recipes-test-{suffix}"))
    }

    #[test]
    fn discover_finds_playbooks_roles_and_metadata() {
        let root = temporary_directory();
        fs::create_dir_all(root.join("playbooks/desktop")).expect("create playbook directory");
        fs::create_dir_all(root.join("roles/docker/tasks")).expect("create role directory");
        fs::write(
            root.join("playbooks/desktop/xfce.yml"),
            "---\n- hosts: all\n",
        )
        .expect("write playbook");
        fs::write(
            root.join("roles/docker/tasks/main.yml"),
            "---\n- name: Docker\n",
        )
        .expect("write role");
        fs::write(
            root.join("pbox.yml"),
            "recipes:\n  desktop/xfce:\n    description: XFCE desktop\n    capabilities: [desktop]\n    path: playbooks/desktop/xfce.yml\n",
        )
        .expect("write metadata");

        let catalog = discover_path(&root, "test", "main").expect("discover recipes");
        assert_eq!(catalog.recipes.len(), 2);
        assert_eq!(catalog.recipes[0].id, "desktop/xfce");
        assert_eq!(catalog.recipes[0].metadata.capabilities, ["desktop"]);
        assert_eq!(catalog.recipes[1].id, "docker");
        assert_eq!(catalog.recipes[1].kind, RecipeKind::Role);
        fs::remove_dir_all(root).expect("remove fixture");
    }

    #[test]
    fn repository_shorthand_uses_github() {
        let repository =
            RecipeRepository::with_cache_dir(Some("kierandrewett/pbox-recipes"), "main", ".")
                .expect("create repository");
        assert_eq!(repository.repository, DEFAULT_REPOSITORY);
    }

    #[test]
    fn metadata_path_cannot_escape_repository() {
        let root = temporary_directory();
        fs::create_dir_all(&root).expect("create fixture");
        fs::write(
            root.join("pbox.yml"),
            "recipes:\n  escape:\n    path: ../outside.yml\n",
        )
        .expect("write metadata");
        let error = discover_path(&root, "test", "main").expect_err("reject escape");
        assert!(error.to_string().contains("must stay inside"));
        fs::remove_dir_all(root).expect("remove fixture");
    }
}

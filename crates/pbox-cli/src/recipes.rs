use anyhow::{Context, Result, anyhow, bail};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::collections::BTreeMap;
use std::fs::{self, OpenOptions};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::{Command, Output, Stdio};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

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
pub(crate) struct RecipeCacheLock {
    path: PathBuf,
}

impl Drop for RecipeCacheLock {
    fn drop(&mut self) {
        let _ = fs::remove_file(&self.path);
    }
}

impl RecipeRepository {
    pub fn new(repository: Option<&str>, reference: &str) -> Result<Self> {
        let repository = repository.unwrap_or(DEFAULT_REPOSITORY).trim();
        if repository.is_empty() {
            bail!("recipe repository cannot be empty");
        }
        validate_repository_reference(repository)?;
        let repository = normalise_repository(repository);
        let reference = if reference.trim().is_empty() {
            DEFAULT_REFERENCE.to_owned()
        } else {
            reference.trim().to_owned()
        };
        validate_recipe_reference(&reference)?;
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
        let _lock = self.acquire_lock()?;
        self.sync_unlocked()
    }

    fn sync_unlocked(&self) -> Result<RecipeCatalog> {
        self.ensure_cache_path_is_safe()?;
        let git_directory = self.cache_dir.join(".git");
        let is_git_checkout = match fs::symlink_metadata(&git_directory) {
            Ok(metadata) if metadata.file_type().is_symlink() => {
                bail!("recipe cache Git directory must not be a symbolic link");
            }
            Ok(metadata) if metadata.file_type().is_dir() => true,
            Ok(_) => false,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => false,
            Err(error) => {
                return Err(error)
                    .with_context(|| format!("inspect recipe cache {}", self.cache_dir.display()));
            }
        };
        if is_git_checkout {
            self.ensure_cache_integrity()?;
            run_git(
                &[
                    "-C",
                    self.cache_path()?,
                    "fetch",
                    "--depth",
                    "1",
                    "origin",
                    "--",
                    &self.reference,
                ],
                &self.repository,
            )?;
            run_git(
                &[
                    "-C",
                    self.cache_path()?,
                    "checkout",
                    "--force",
                    "FETCH_HEAD",
                ],
                &self.repository,
            )?;
        } else {
            if let Ok(metadata) = fs::symlink_metadata(&self.cache_dir) {
                if metadata.file_type().is_symlink() {
                    bail!("recipe cache path must not be a symbolic link");
                }
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
            let cache = self.cache_path()?;
            run_git(
                &[
                    "clone",
                    "--depth",
                    "1",
                    "--branch",
                    &self.reference,
                    "--",
                    &self.repository,
                    cache,
                ],
                &self.repository,
            )?;
        }
        restrict_cache_directory(&self.cache_dir)?;
        self.write_reference_marker()?;
        self.discover_unlocked_checked()
    }

    pub fn sync_if_stale(&self, ttl: Duration) -> Result<RecipeCatalog> {
        let _lock = self.acquire_lock()?;
        self.sync_if_stale_unlocked(ttl)
    }

    fn sync_if_stale_unlocked(&self, ttl: Duration) -> Result<RecipeCatalog> {
        self.ensure_cache_path_is_safe()?;
        let reference_is_current = self
            .reference_marker()?
            .is_some_and(|reference| reference == self.reference);
        let fetch_head = self.cache_dir.join(".git").join("FETCH_HEAD");
        if ttl > Duration::ZERO
            && reference_is_current
            && let Ok(modified) = fs::metadata(&fetch_head).and_then(|metadata| metadata.modified())
            && let Ok(age) = SystemTime::now().duration_since(modified)
            && age < ttl
        {
            return self.discover_unlocked_checked();
        }
        self.sync_unlocked()
    }

    pub(crate) fn prepare_for_apply(
        &self,
        ttl: Option<Duration>,
    ) -> Result<(RecipeCacheLock, RecipeCatalog)> {
        let lock = self.acquire_lock()?;
        let catalog = match ttl {
            Some(ttl) => self.sync_if_stale_unlocked(ttl)?,
            None => self.discover_unlocked_checked()?,
        };
        Ok((lock, catalog))
    }

    pub fn cache_dir(&self) -> &Path {
        &self.cache_dir
    }

    pub fn discover(&self) -> Result<RecipeCatalog> {
        let _lock = self.acquire_lock()?;
        self.discover_unlocked_checked()
    }

    fn discover_unlocked_checked(&self) -> Result<RecipeCatalog> {
        self.ensure_cache_path_is_safe()?;
        restrict_cache_directory(&self.cache_dir)?;
        self.ensure_cache_integrity()?;
        let reference = self.reference_marker()?.ok_or_else(|| {
            anyhow!("recipe cache reference marker is missing; sync the repository")
        })?;
        if reference != self.reference {
            bail!(
                "recipe cache contains reference {reference}, expected {}",
                self.reference
            );
        }
        let expected_revision = self.revision_marker()?.ok_or_else(|| {
            anyhow!("recipe cache revision marker is missing; sync the repository")
        })?;
        let actual_revision = git_revision(&self.cache_dir)?;
        if expected_revision != actual_revision {
            bail!("recipe cache HEAD does not match its recorded revision; synchronise again");
        }
        self.discover_unlocked()
    }

    fn discover_unlocked(&self) -> Result<RecipeCatalog> {
        discover_path(&self.cache_dir, &self.repository, &self.reference)
    }
    fn ensure_cache_path_is_safe(&self) -> Result<()> {
        match fs::symlink_metadata(&self.cache_dir) {
            Ok(metadata) if metadata.file_type().is_symlink() => {
                bail!("recipe cache path must not be a symbolic link");
            }
            Ok(_) => Ok(()),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
            Err(error) => Err(error)
                .with_context(|| format!("inspect recipe cache {}", self.cache_dir.display())),
        }
    }

    fn acquire_lock(&self) -> Result<RecipeCacheLock> {
        let path = self.cache_dir.with_extension("lock");
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent).with_context(|| {
                format!("create recipe cache lock directory {}", parent.display())
            })?;
        }
        for _attempt in 0..2 {
            let mut file = match OpenOptions::new().write(true).create_new(true).open(&path) {
                Ok(file) => file,
                Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {
                    if !stale_lock(&path)? {
                        bail!("recipe cache lock is held: {}", path.display());
                    }
                    let stamp = SystemTime::now()
                        .duration_since(UNIX_EPOCH)
                        .context("read system clock")?
                        .as_nanos();
                    let quarantine =
                        path.with_extension(format!("lock.stale-{}-{stamp}", std::process::id()));
                    match fs::rename(&path, &quarantine) {
                        Ok(()) => {
                            fs::remove_file(&quarantine).with_context(|| {
                                format!(
                                    "remove quarantined recipe cache lock {}",
                                    quarantine.display()
                                )
                            })?;
                            continue;
                        }
                        Err(error) if error.kind() == std::io::ErrorKind::NotFound => continue,
                        Err(error) => {
                            return Err(error).with_context(|| {
                                format!("quarantine stale recipe cache lock {}", path.display())
                            });
                        }
                    }
                }
                Err(error) => {
                    return Err(error)
                        .with_context(|| format!("acquire recipe cache lock {}", path.display()));
                }
            };
            if let Err(error) = writeln!(file, "pid={}", std::process::id()) {
                let _ = fs::remove_file(&path);
                return Err(error)
                    .with_context(|| format!("write recipe cache lock {}", path.display()));
            }
            return Ok(RecipeCacheLock { path });
        }
        bail!("could not acquire recipe cache lock: {}", path.display())
    }

    fn cache_path(&self) -> Result<&str> {
        self.cache_dir
            .to_str()
            .ok_or_else(|| anyhow!("recipe cache path is not UTF-8"))
    }

    fn cache_is_git_checkout(&self) -> Result<bool> {
        match fs::symlink_metadata(self.cache_dir.join(".git")) {
            Ok(metadata) if metadata.file_type().is_symlink() => {
                bail!("recipe cache Git directory must not be a symbolic link");
            }
            Ok(metadata) => Ok(metadata.is_dir()),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(false),
            Err(error) => Err(error)
                .with_context(|| format!("inspect recipe cache {}", self.cache_dir.display())),
        }
    }

    fn reference_marker(&self) -> Result<Option<String>> {
        let path = self.cache_dir.join(".git").join("pbox-reference");
        match fs::symlink_metadata(&path) {
            Ok(metadata) if metadata.file_type().is_symlink() => {
                bail!("recipe cache reference marker must not be a symbolic link");
            }
            Ok(_) => Ok(Some(
                fs::read_to_string(&path)
                    .with_context(|| format!("read recipe cache reference {}", path.display()))?
                    .trim()
                    .to_owned(),
            )),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(None),
            Err(error) => Err(error)
                .with_context(|| format!("inspect recipe cache reference {}", path.display())),
        }
    }
    fn revision_marker(&self) -> Result<Option<String>> {
        let path = self.cache_dir.join(".git").join("pbox-revision");
        match fs::symlink_metadata(&path) {
            Ok(metadata) if metadata.file_type().is_symlink() => {
                bail!("recipe cache revision marker must not be a symbolic link");
            }
            Ok(_) => Ok(Some(
                fs::read_to_string(&path)
                    .with_context(|| format!("read recipe cache revision {}", path.display()))?
                    .trim()
                    .to_owned(),
            )),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(None),
            Err(error) => Err(error)
                .with_context(|| format!("inspect recipe cache revision {}", path.display())),
        }
    }

    fn write_reference_marker(&self) -> Result<()> {
        let reference_path = self.cache_dir.join(".git").join("pbox-reference");
        write_cache_marker(&reference_path, &format!("{}\n", self.reference))?;
        let revision_path = self.cache_dir.join(".git").join("pbox-revision");
        let revision = git_revision(&self.cache_dir)?;
        write_cache_marker(&revision_path, &format!("{revision}\n"))
    }

    fn ensure_cache_integrity(&self) -> Result<()> {
        if !self.cache_is_git_checkout()? {
            bail!("recipe cache is not a Git checkout; synchronise the repository");
        }
        let origin = run_git(
            &["-C", self.cache_path()?, "remote", "get-url", "origin"],
            &self.repository,
        )?;
        let actual_origin = String::from_utf8(origin.stdout)
            .context("recipe cache origin is not UTF-8")?
            .trim()
            .to_owned();
        if repository_identity(&actual_origin) != repository_identity(&self.repository) {
            bail!("recipe cache origin does not match the configured repository");
        }
        let submodules = run_git(
            &[
                "-C",
                self.cache_path()?,
                "submodule",
                "status",
                "--recursive",
            ],
            &self.repository,
        )?;
        if !submodules.stdout.is_empty() {
            bail!("recipe cache must not contain Git submodules");
        }
        let index_flags = run_git(
            &["-C", self.cache_path()?, "ls-files", "-v"],
            &self.repository,
        )?;
        if has_unsafe_git_index_flags(&index_flags.stdout) {
            bail!("recipe cache uses Git index flags; remove the cache and synchronise again");
        }
        let status = run_git(
            &[
                "-C",
                self.cache_path()?,
                "status",
                "--porcelain",
                "--ignored=matching",
                "--untracked-files=all",
            ],
            &self.repository,
        )?;
        if run_git(
            &[
                "-C",
                self.cache_path()?,
                "diff",
                "--no-ext-diff",
                "--quiet",
                "HEAD",
                "--",
            ],
            &self.repository,
        )
        .is_err()
        {
            bail!("recipe cache has local changes; remove the cache and synchronise again");
        }
        if !status.stdout.is_empty() {
            bail!("recipe cache has local changes; remove the cache and synchronise again");
        }
        Ok(())
    }
}

fn write_cache_marker(path: &Path, contents: &str) -> Result<()> {
    let parent = path
        .parent()
        .ok_or_else(|| anyhow!("recipe cache marker has no parent directory"))?;
    let stamp = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .context("read system clock")?
        .as_nanos();
    let file_name = path
        .file_name()
        .and_then(|name| name.to_str())
        .ok_or_else(|| anyhow!("recipe cache marker path is not UTF-8"))?;
    let temporary = parent.join(format!(".{file_name}.tmp-{}-{stamp}", std::process::id()));
    let mut file = OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(&temporary)
        .with_context(|| {
            format!(
                "create temporary recipe cache marker {}",
                temporary.display()
            )
        })?;
    if let Err(error) = file
        .write_all(contents.as_bytes())
        .and_then(|()| file.sync_all())
    {
        let _ = fs::remove_file(&temporary);
        return Err(error).with_context(|| format!("write recipe cache marker {}", path.display()));
    }
    if let Err(error) = fs::rename(&temporary, path) {
        let _ = fs::remove_file(&temporary);
        return Err(error)
            .with_context(|| format!("replace recipe cache marker {}", path.display()));
    }
    Ok(())
}
fn restrict_cache_directory(path: &Path) -> Result<()> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let metadata = fs::symlink_metadata(path)
            .with_context(|| format!("inspect recipe cache {}", path.display()))?;
        if metadata.file_type().is_symlink() {
            bail!("recipe cache path must not be a symbolic link");
        }
        fs::set_permissions(path, fs::Permissions::from_mode(0o700))
            .with_context(|| format!("restrict recipe cache {}", path.display()))?;
    }
    #[cfg(not(unix))]
    {
        let _ = path;
    }
    Ok(())
}

fn stale_lock(path: &Path) -> Result<bool> {
    let metadata = fs::symlink_metadata(path)
        .with_context(|| format!("inspect recipe cache lock {}", path.display()))?;
    if metadata.file_type().is_symlink() {
        bail!("recipe cache lock must not be a symbolic link");
    }
    let contents = fs::read_to_string(path)
        .with_context(|| format!("read recipe cache lock {}", path.display()))?;
    if let Some(pid) = contents
        .lines()
        .find_map(|line| line.strip_prefix("pid="))
        .and_then(|value| value.trim().parse::<u32>().ok())
    {
        return Ok(!Path::new("/proc").join(pid.to_string()).exists());
    }
    let age = metadata
        .modified()
        .ok()
        .and_then(|modified| SystemTime::now().duration_since(modified).ok());
    Ok(age.is_some_and(|age| age >= Duration::from_secs(3600)))
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
    for candidate in candidates {
        let id = candidate.id.clone();
        let manifest_entry = manifest.recipes.get(&id);
        let (kind, path) = if let Some(entry) = manifest_entry {
            if entry.path.is_some() {
                manifest_recipe_path(root, &id, entry)?
            } else {
                (candidate.kind, candidate.path)
            }
        } else {
            (candidate.kind, candidate.path)
        };
        let metadata = manifest_entry
            .map(|entry| entry.metadata.clone())
            .unwrap_or_default();
        validate_recipe_capabilities(&id, &metadata.capabilities)?;
        recipes.push(Recipe {
            id,
            kind,
            path,
            metadata,
        });
    }

    for (id, entry) in manifest.recipes {
        if recipes.iter().any(|recipe| recipe.id == id) {
            continue;
        }
        let (kind, path) = manifest_recipe_path(root, &id, &entry)?;
        let metadata = entry.metadata;
        validate_recipe_capabilities(&id, &metadata.capabilities)?;
        recipes.push(Recipe {
            id,
            kind,
            path,
            metadata,
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

fn manifest_recipe_path(
    root: &Path,
    id: &str,
    entry: &RecipeManifestEntry,
) -> Result<(RecipeKind, String)> {
    let relative_path = entry.path.as_deref().ok_or_else(|| {
        anyhow!("recipe metadata for {id} must include a path because it was not discovered")
    })?;
    let path = normalise_relative_path(relative_path)?;
    if !validated_repository_path(root, &path)?.exists() {
        bail!("recipe {id} points to missing path {}", path.display());
    }
    let kind = recipe_kind(root, &path)?;
    Ok((kind, path_string(&path)))
}

fn read_manifest(root: &Path) -> Result<RecipeManifest> {
    for name in ["pbox.yml", "pbox.yaml"] {
        let path = root.join(name);
        let metadata = match fs::symlink_metadata(&path) {
            Ok(metadata) => metadata,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => continue,
            Err(error) => {
                return Err(error)
                    .with_context(|| format!("read recipe metadata {}", path.display()));
            }
        };
        if metadata.file_type().is_symlink() {
            bail!(
                "recipe metadata must not be a symbolic link: {}",
                path.display()
            );
        }
        if !metadata.file_type().is_file() {
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
        let file_type = entry
            .file_type()
            .with_context(|| format!("read recipe entry type {}", path.display()))?;
        if file_type.is_symlink() {
            continue;
        }
        if path.file_name().is_some_and(|name| name == ".git") {
            continue;
        }
        if file_type.is_dir() {
            collect_candidates(root, &path, candidates)?;
            continue;
        }
        if !file_type.is_file() {
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
    let _ = validated_repository_path(root, path)?;
    let absolute = root.join(path);
    reject_symlink_ancestors(root, path)?;
    if role_path(root, path)? {
        return Ok(RecipeKind::Role);
    }
    if absolute.is_file() {
        if !is_yaml_file(&absolute) {
            bail!(
                "recipe path is not an Ansible YAML file: {}",
                path.display()
            );
        }
        return Ok(RecipeKind::Playbook);
    }
    bail!(
        "recipe path is not a playbook or Ansible role: {}",
        path.display()
    )
}

fn role_path(root: &Path, path: &Path) -> Result<bool> {
    let components = path.components().collect::<Vec<_>>();
    if components.len() == 2
        && components[0].as_os_str() == "roles"
        && components[1].as_os_str() != ""
    {
        for name in ["main.yml", "main.yaml"] {
            let task_path = root.join(path).join("tasks").join(name);
            let relative_task = task_path
                .strip_prefix(root)
                .with_context(|| format!("resolve recipe role task {}", task_path.display()))?;
            reject_symlink_ancestors(root, relative_task)?;
            match fs::symlink_metadata(&task_path) {
                Ok(metadata) if metadata.file_type().is_symlink() => {
                    bail!(
                        "recipe role task must not be a symbolic link: {}",
                        task_path.display()
                    );
                }
                Ok(metadata) if metadata.file_type().is_file() => return Ok(true),
                Ok(_) => {}
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
                Err(error) => {
                    return Err(error)
                        .with_context(|| format!("read recipe role task {}", task_path.display()));
                }
            }
        }
        return Ok(false);
    }
    if components.len() == 4
        && components[0].as_os_str() == "roles"
        && components[2].as_os_str() == "tasks"
        && matches!(
            components[3].as_os_str().to_str(),
            Some("main.yml" | "main.yaml")
        )
    {
        reject_symlink_ancestors(root, path)?;
        return Ok(root.join(path).is_file());
    }
    Ok(false)
}

fn reject_symlink_ancestors(root: &Path, relative: &Path) -> Result<()> {
    let mut current = root.to_owned();
    for component in relative.components() {
        current.push(component.as_os_str());
        match fs::symlink_metadata(&current) {
            Ok(metadata) if metadata.file_type().is_symlink() => {
                bail!(
                    "recipe path must not contain a symbolic link: {}",
                    current.display()
                );
            }
            Ok(_) => {}
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(()),
            Err(error) => {
                return Err(error)
                    .with_context(|| format!("read recipe path {}", current.display()));
            }
        }
    }
    Ok(())
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

fn validated_repository_path(root: &Path, relative: &Path) -> Result<PathBuf> {
    if relative.is_absolute() {
        bail!(
            "recipe metadata path must be relative to the repository: {}",
            relative.display()
        );
    }
    let canonical_root =
        fs::canonicalize(root).with_context(|| format!("resolve repository {}", root.display()))?;
    let candidate = root.join(relative);
    let canonical_candidate = fs::canonicalize(&candidate)
        .with_context(|| format!("resolve recipe path {}", candidate.display()))?;
    if !canonical_candidate.starts_with(&canonical_root) {
        bail!(
            "recipe metadata path escapes the repository: {}",
            relative.display()
        );
    }
    Ok(candidate)
}

fn path_string(path: &Path) -> String {
    path.components()
        .map(|component| component.as_os_str().to_string_lossy())
        .collect::<Vec<_>>()
        .join("/")
}

const MAX_RECIPE_CAPABILITIES: usize = 32;
const MAX_RECIPE_CAPABILITY_LENGTH: usize = 64;

fn validate_recipe_capabilities(recipe_id: &str, capabilities: &[String]) -> Result<()> {
    if capabilities.len() > MAX_RECIPE_CAPABILITIES {
        bail!("recipe {recipe_id} declares more than {MAX_RECIPE_CAPABILITIES} capabilities");
    }
    for capability in capabilities {
        if capability.is_empty()
            || capability.len() > MAX_RECIPE_CAPABILITY_LENGTH
            || !capability.bytes().all(|byte| {
                byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_' | b'.' | b'/')
            })
        {
            bail!("recipe {recipe_id} has an invalid capability: {capability:?}");
        }
    }
    Ok(())
}
fn has_unsafe_git_index_flags(output: &[u8]) -> bool {
    output
        .split(|byte| *byte == b'\n')
        .any(|line| matches!(line.first(), Some(b'h' | b'S')))
}

fn validate_repository_reference(repository: &str) -> Result<()> {
    if repository.contains('?') || repository.contains('#') {
        bail!("recipe repository must not contain query or fragment data");
    }
    if repository.contains("::") {
        bail!("recipe repository must not use Git external transport");
    }
    if let Some((scheme, authority_and_path)) = repository.split_once("://") {
        let scheme = scheme.to_ascii_lowercase();
        if !matches!(scheme.as_str(), "https" | "ssh" | "file") {
            bail!("recipe repository URLs must use HTTPS, SSH, or file transport");
        }
        let authority = authority_and_path
            .split(['/', '\\'])
            .next()
            .unwrap_or_default();
        if authority.contains('@') {
            bail!("recipe repository URLs must not contain embedded credentials");
        }
        if scheme != "file" && authority.is_empty() {
            bail!("recipe repository URL must include a host");
        }
    }
    Ok(())
}
fn validate_recipe_reference(reference: &str) -> Result<()> {
    if reference.is_empty()
        || reference.starts_with('-')
        || reference.contains("..")
        || reference.contains("@{")
        || reference.ends_with('/')
        || reference.ends_with('.')
        || reference
            .chars()
            .any(|character| character.is_control() || character.is_whitespace())
        || reference
            .chars()
            .any(|character| matches!(character, '~' | '^' | ':' | '?' | '*' | '[' | '\\'))
    {
        bail!("recipe reference is not a valid Git ref: {reference}");
    }
    Ok(())
}

fn normalise_repository(repository: &str) -> String {
    let trimmed = repository.trim();
    if trimmed.matches('/').count() == 1
        && !trimmed.contains("://")
        && !trimmed.starts_with("git@")
        && !trimmed.starts_with('.')
        && !trimmed.starts_with('/')
    {
        if trimmed.ends_with(".git") {
            return format!("https://github.com/{trimmed}");
        }
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
    let mut command = Command::new("git");
    command.env_clear();
    for key in ["PATH", "HOME", "LANG", "LC_ALL", "TMPDIR", "SSH_AUTH_SOCK"] {
        if let Some(value) = std::env::var_os(key) {
            command.env(key, value);
        }
    }
    command
        .env("GIT_ALLOW_PROTOCOL", "file:ssh:https")
        .env("GIT_TERMINAL_PROMPT", "0")
        .env("GIT_CONFIG_NOSYSTEM", "1");
    let output = command
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
    let output = run_git(
        &[
            "-C",
            root.to_str()
                .ok_or_else(|| anyhow!("recipe path is not UTF-8"))?,
            "rev-parse",
            "HEAD",
        ],
        "<recipe repository>",
    )?;
    Ok(String::from_utf8(output.stdout)?.trim().to_owned())
}

fn repository_identity(repository: &str) -> String {
    let normalised = normalise_repository(repository);
    fs::canonicalize(&normalised)
        .map(|path| path.to_string_lossy().into_owned())
        .unwrap_or(normalised)
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
    fn stale_cache_lock_is_recovered() {
        let root = temporary_directory();
        fs::create_dir_all(&root).expect("create fixture");
        let cache = root.join("cache");
        let lock_path = cache.with_extension("lock");
        fs::write(&lock_path, "pid=4294967295\n").expect("write stale lock");
        let repository = RecipeRepository::with_cache_dir(Some("test/repository"), "main", &cache)
            .expect("create repository");

        let lock = repository.acquire_lock().expect("recover stale lock");
        assert!(lock_path.is_file());
        drop(lock);
        assert!(!lock_path.exists());
        fs::remove_dir_all(root).expect("remove fixture");
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
    fn manifest_role_aliases_use_role_wrapper_kind() {
        let root = temporary_directory();
        fs::create_dir_all(root.join("roles/docker/tasks")).expect("create role directory");
        fs::write(
            root.join("roles/docker/tasks/main.yml"),
            "---\n- name: Docker\n",
        )
        .expect("write role");
        fs::write(
            root.join("pbox.yml"),
            "recipes:\n  containers/docker:\n    path: roles/docker/tasks/main.yml\n",
        )
        .expect("write metadata");

        let catalog = discover_path(&root, "test", "main").expect("discover role alias");
        let recipe = catalog
            .recipes
            .iter()
            .find(|recipe| recipe.id == "containers/docker")
            .expect("find role alias");
        assert_eq!(recipe.kind, RecipeKind::Role);
        fs::remove_dir_all(root).expect("remove fixture");
    }

    #[test]
    fn discovery_skips_symlink_cycles() {
        use std::os::unix::fs::symlink;

        let root = temporary_directory();
        fs::create_dir_all(&root).expect("create fixture");
        fs::write(root.join("main.yml"), "---\n- hosts: all\n").expect("write playbook");
        symlink(&root, root.join("loop")).expect("create symlink cycle");
        let catalog = discover_path(&root, "test", "main").expect("discover without recursion");
        assert_eq!(catalog.recipes.len(), 1);
        assert_eq!(catalog.recipes[0].id, "main");
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
    fn repository_rejects_plain_http_and_unknown_transports() {
        assert!(RecipeRepository::new(Some("http://example.test/recipes.git"), "main").is_err());
        assert!(RecipeRepository::new(Some("git://example.test/recipes.git"), "main").is_err());
        assert!(RecipeRepository::new(Some("https://example.test/recipes.git"), "main").is_ok());
        assert!(RecipeRepository::new(Some("ext::sh -c evil"), "main").is_err());
    }
    #[test]
    fn repository_rejects_invalid_git_references() {
        for reference in ["-main", "main..broken", "main\nbroken", "main:evil"] {
            assert!(
                RecipeRepository::new(Some("https://example.test/recipes.git"), reference).is_err(),
                "reference should be rejected: {reference:?}"
            );
        }
        assert_eq!(
            RecipeRepository::new(Some("https://example.test/recipes.git"), "")
                .expect("empty reference uses the default")
                .reference,
            DEFAULT_REFERENCE
        );
    }

    #[test]
    fn recipe_capabilities_are_bounded_slugs() {
        assert!(validate_recipe_capabilities("demo", &["desktop".to_owned()]).is_ok());
        assert!(validate_recipe_capabilities("demo", &["not valid".to_owned()]).is_err());
        assert!(validate_recipe_capabilities("demo", &[String::from("x").repeat(65)]).is_err());
    }
    #[test]
    fn unsafe_git_index_flags_are_detected() {
        assert!(!has_unsafe_git_index_flags(b"H playbook.yml\n"));
        assert!(has_unsafe_git_index_flags(b"h playbook.yml\n"));
        assert!(has_unsafe_git_index_flags(b"S playbook.yml\n"));
    }

    #[test]
    fn discovery_rejects_non_git_cache() {
        let root = temporary_directory();
        fs::create_dir_all(&root).expect("create fixture");
        fs::write(root.join("main.yml"), "---\n- hosts: all\n").expect("write playbook");
        let repository = RecipeRepository::with_cache_dir(Some("test/repository"), "main", &root)
            .expect("create repository");

        let error = repository
            .discover()
            .expect_err("reject non-Git recipe cache");

        assert!(error.to_string().contains("not a Git checkout"));
        fs::remove_dir_all(root).expect("remove fixture");
    }
    #[cfg(unix)]
    #[test]
    fn discovery_rejects_symlinked_cache_root() {
        use std::os::unix::fs::symlink;

        let root = temporary_directory();
        let target = temporary_directory();
        fs::create_dir_all(&target).expect("create target");
        fs::write(target.join("main.yml"), "---\n- hosts: all\n").expect("write playbook");
        symlink(&target, &root).expect("create cache symlink");
        let repository = RecipeRepository::with_cache_dir(Some("test/repository"), "main", &root)
            .expect("create repository");

        let error = repository.discover().expect_err("reject cache symlink");

        assert!(error.to_string().contains("symbolic link"));
        fs::remove_file(root).expect("remove cache symlink");
        fs::remove_dir_all(target).expect("remove target");
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

use crate::VmidPattern;
use reqwest::Url;
use serde::{Deserialize, Deserializer, Serialize, Serializer};
use std::collections::BTreeMap;
use std::fmt;
use std::fs;
use std::path::{Path, PathBuf};
use std::time::Duration;
use thiserror::Error;

#[derive(Clone, PartialEq, Eq)]
pub struct Secret(String);

impl Secret {
    pub fn new(value: impl Into<String>) -> Self {
        Self(value.into())
    }

    pub fn expose(&self) -> &str {
        &self.0
    }
}

impl fmt::Debug for Secret {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("<redacted>")
    }
}

impl Serialize for Secret {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        serializer.serialize_str(&self.0)
    }
}

impl<'de> Deserialize<'de> for Secret {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        Ok(Self(String::deserialize(deserializer)?))
    }
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
#[serde(default)]
pub struct PveConfig {
    pub url: Option<String>,
    pub token_id: Option<String>,
    pub token_secret: Option<Secret>,
    pub tls_insecure: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(default)]
pub struct AgentConfig {
    pub binary: Option<PathBuf>,
    pub port: u16,
}

impl Default for AgentConfig {
    fn default() -> Self {
        Self {
            binary: None,
            port: 7443,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(default)]
pub struct RecipeConfig {
    pub repository: String,
    #[serde(rename = "ref")]
    pub reference: String,
    pub auto_sync: bool,
    pub sync_ttl: String,
    pub snapshot_before_apply: String,
    pub rollback_on_failure: bool,
}

impl Default for RecipeConfig {
    fn default() -> Self {
        Self {
            repository: "https://github.com/kierandrewett/pbox-recipes.git".to_owned(),
            reference: "main".to_owned(),
            auto_sync: true,
            sync_ttl: "15m".to_owned(),
            snapshot_before_apply: "auto".to_owned(),
            rollback_on_failure: false,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(default)]
pub struct Config {
    pub pve: PveConfig,
    pub agent: AgentConfig,
    pub recipes: RecipeConfig,
    pub vmid_pattern: VmidPattern,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            pve: PveConfig::default(),
            agent: AgentConfig::default(),
            recipes: RecipeConfig::default(),
            vmid_pattern: VmidPattern::parse("9xxx").expect("default VMID pattern is valid"),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct RedactedPveConfig {
    pub url: Option<String>,
    pub token_id: Option<String>,
    pub token_secret: Option<String>,
    pub tls_insecure: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct RedactedAgentConfig {
    pub binary: Option<PathBuf>,
    pub port: u16,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct RedactedRecipeConfig {
    pub repository: String,
    #[serde(rename = "ref")]
    pub reference: String,
    pub auto_sync: bool,
    pub sync_ttl: String,
    pub snapshot_before_apply: String,
    pub rollback_on_failure: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct RedactedConfig {
    pub pve: RedactedPveConfig,
    pub agent: RedactedAgentConfig,
    pub recipes: RedactedRecipeConfig,
    pub vmid_pattern: VmidPattern,
}
pub fn parse_duration(value: &str) -> Result<Duration, String> {
    let trimmed = value.trim();
    if trimmed.is_empty() {
        return Err("duration cannot be empty".to_owned());
    }
    let (amount_text, multiplier) = match trimmed.chars().last() {
        Some('s') => (&trimmed[..trimmed.len() - 1], 1_u64),
        Some('m') => (&trimmed[..trimmed.len() - 1], 60_u64),
        Some('h') => (&trimmed[..trimmed.len() - 1], 60_u64 * 60),
        Some('d') => (&trimmed[..trimmed.len() - 1], 60_u64 * 60 * 24),
        Some(character) if character.is_ascii_digit() => (trimmed, 1_u64),
        _ => return Err("use seconds, minutes, hours, or days".to_owned()),
    };
    let amount = amount_text
        .parse::<u64>()
        .map_err(|_| "duration amount must be an integer".to_owned())?;
    let seconds = amount
        .checked_mul(multiplier)
        .ok_or_else(|| "duration is too large".to_owned())?;
    Ok(Duration::from_secs(seconds))
}

impl Config {
    pub fn validate(&self) -> Result<(), ConfigError> {
        if let Some(url) = &self.pve.url {
            validate_non_empty("pve.url", url, "URL cannot be empty")?;
            validate_pve_url("pve.url", url)?;
        }
        if let Some(token_id) = &self.pve.token_id {
            validate_non_empty("pve.token_id", token_id, "token id cannot be empty")?;
        }
        if let Some(token_secret) = &self.pve.token_secret
            && token_secret.expose().is_empty()
        {
            return Err(ConfigError::InvalidValue {
                key: "pve.token_secret".to_owned(),
                reason: "token secret cannot be empty".to_owned(),
            });
        }
        if let Some(binary) = &self.agent.binary
            && binary.as_os_str().is_empty()
        {
            return Err(ConfigError::InvalidValue {
                key: "agent.binary".to_owned(),
                reason: "agent binary path cannot be empty".to_owned(),
            });
        }
        if self.agent.port == 0 {
            return Err(ConfigError::InvalidValue {
                key: "agent.port".to_owned(),
                reason: "expected a TCP port from 1 to 65535".to_owned(),
            });
        }
        validate_repository_reference("recipes.repository", &self.recipes.repository)?;
        validate_non_empty(
            "recipes.ref",
            &self.recipes.reference,
            "recipe reference cannot be empty",
        )?;
        parse_duration(&self.recipes.sync_ttl).map_err(|reason| ConfigError::InvalidValue {
            key: "recipes.sync-ttl".to_owned(),
            reason,
        })?;
        validate_snapshot_policy(
            "recipes.snapshot-before-apply",
            &self.recipes.snapshot_before_apply,
        )?;
        Ok(())
    }

    pub fn redacted(&self) -> RedactedConfig {
        RedactedConfig {
            pve: RedactedPveConfig {
                url: redact_url(&self.pve.url),
                token_id: self.pve.token_id.clone(),
                token_secret: self
                    .pve
                    .token_secret
                    .as_ref()
                    .map(|_| "<redacted>".to_owned()),
                tls_insecure: self.pve.tls_insecure,
            },
            agent: RedactedAgentConfig {
                binary: self.agent.binary.clone(),
                port: self.agent.port,
            },
            recipes: RedactedRecipeConfig {
                repository: self.recipes.repository.clone(),
                reference: self.recipes.reference.clone(),
                auto_sync: self.recipes.auto_sync,
                sync_ttl: self.recipes.sync_ttl.clone(),
                snapshot_before_apply: self.recipes.snapshot_before_apply.clone(),
                rollback_on_failure: self.recipes.rollback_on_failure,
            },
            vmid_pattern: self.vmid_pattern.clone(),
        }
    }

    pub fn redacted_pairs(&self) -> BTreeMap<String, String> {
        let mut values = BTreeMap::new();
        values.insert(
            "pve.url".to_owned(),
            optional_value(&redact_url(&self.pve.url)),
        );
        values.insert(
            "pve.token_id".to_owned(),
            optional_value(&self.pve.token_id),
        );
        values.insert(
            "pve.token_secret".to_owned(),
            if self.pve.token_secret.is_some() {
                "<redacted>".to_owned()
            } else {
                "<unset>".to_owned()
            },
        );
        values.insert(
            "pve.tls_insecure".to_owned(),
            self.pve.tls_insecure.to_string(),
        );
        values.insert(
            "agent.binary".to_owned(),
            self.agent
                .binary
                .as_ref()
                .map(|path| path.display().to_string())
                .unwrap_or_else(|| "<unset>".to_owned()),
        );
        values.insert("agent.port".to_owned(), self.agent.port.to_string());
        values.insert(
            "recipes.repository".to_owned(),
            self.recipes.repository.clone(),
        );
        values.insert("recipes.ref".to_owned(), self.recipes.reference.clone());
        values.insert(
            "recipes.auto-sync".to_owned(),
            self.recipes.auto_sync.to_string(),
        );
        values.insert("recipes.sync-ttl".to_owned(), self.recipes.sync_ttl.clone());
        values.insert(
            "recipes.snapshot-before-apply".to_owned(),
            self.recipes.snapshot_before_apply.clone(),
        );
        values.insert(
            "recipes.rollback-on-failure".to_owned(),
            self.recipes.rollback_on_failure.to_string(),
        );
        values.insert("vmid_pattern".to_owned(), self.vmid_pattern.to_string());
        values
    }

    pub fn get_redacted(&self, key: &str) -> Option<String> {
        self.redacted_pairs().remove(key)
    }

    pub fn set_value(&mut self, key: &str, value: &str) -> Result<(), ConfigError> {
        match key {
            "pve.url" => {
                validate_non_empty(key, value, "URL cannot be empty")?;
                validate_pve_url(key, value)?;
                self.pve.url = Some(value.trim().to_owned());
            }
            "pve.token_id" => {
                if value.trim().is_empty() {
                    return Err(ConfigError::InvalidValue {
                        key: key.to_owned(),
                        reason: "token id cannot be empty".to_owned(),
                    });
                }
                self.pve.token_id = Some(value.to_owned());
            }
            "pve.token_secret" => {
                if value.is_empty() {
                    return Err(ConfigError::InvalidValue {
                        key: key.to_owned(),
                        reason: "token secret cannot be empty".to_owned(),
                    });
                }
                self.pve.token_secret = Some(Secret::new(value));
            }
            "pve.tls_insecure" => {
                self.pve.tls_insecure = parse_bool(key, value)?;
            }
            "agent.binary" => {
                if value.trim().is_empty() {
                    return Err(ConfigError::InvalidValue {
                        key: key.to_owned(),
                        reason: "agent binary path cannot be empty".to_owned(),
                    });
                }
                self.agent.binary = Some(PathBuf::from(value));
            }
            "agent.port" => {
                self.agent.port = parse_port(key, value)?;
            }
            "recipes.repository" => {
                validate_repository_reference(key, value)?;
                self.recipes.repository = value.to_owned();
            }
            "recipes.ref" => {
                validate_non_empty(key, value, "recipe reference cannot be empty")?;
                self.recipes.reference = value.to_owned();
            }
            "recipes.auto-sync" => {
                self.recipes.auto_sync = parse_bool(key, value)?;
            }
            "recipes.sync-ttl" => {
                parse_duration(value).map_err(|reason| ConfigError::InvalidValue {
                    key: key.to_owned(),
                    reason,
                })?;
                self.recipes.sync_ttl = value.to_owned();
            }
            "recipes.snapshot-before-apply" => {
                validate_snapshot_policy(key, value)?;
                self.recipes.snapshot_before_apply = value.to_owned();
            }
            "recipes.rollback-on-failure" => {
                self.recipes.rollback_on_failure = parse_bool(key, value)?;
            }
            "vmid_pattern" => {
                self.vmid_pattern =
                    value
                        .parse()
                        .map_err(|error: crate::VmidError| ConfigError::InvalidValue {
                            key: key.to_owned(),
                            reason: error.to_string(),
                        })?;
            }
            _ => return Err(ConfigError::UnknownKey(key.to_owned())),
        }
        Ok(())
    }
    pub fn unset_value(&mut self, key: &str) -> Result<(), ConfigError> {
        match key {
            "pve.url" => self.pve.url = None,
            "pve.token_id" => self.pve.token_id = None,
            "pve.token_secret" => self.pve.token_secret = None,
            "pve.tls_insecure" => self.pve.tls_insecure = false,
            "agent.binary" => self.agent.binary = None,
            "agent.port" => self.agent.port = AgentConfig::default().port,
            "recipes.repository" => self.recipes.repository = RecipeConfig::default().repository,
            "recipes.ref" => self.recipes.reference = RecipeConfig::default().reference,
            "recipes.auto-sync" => self.recipes.auto_sync = RecipeConfig::default().auto_sync,
            "recipes.sync-ttl" => self.recipes.sync_ttl = RecipeConfig::default().sync_ttl,
            "recipes.snapshot-before-apply" => {
                self.recipes.snapshot_before_apply = RecipeConfig::default().snapshot_before_apply
            }
            "recipes.rollback-on-failure" => {
                self.recipes.rollback_on_failure = RecipeConfig::default().rollback_on_failure
            }
            "vmid_pattern" => self.vmid_pattern = Config::default().vmid_pattern,
            _ => return Err(ConfigError::UnknownKey(key.to_owned())),
        }
        Ok(())
    }

    pub fn apply_overrides(&mut self, overrides: &ConfigOverrides) -> Result<(), ConfigError> {
        if let Some(value) = &overrides.pve_url {
            self.set_value("pve.url", value)?;
        }
        if let Some(value) = &overrides.pve_token_id {
            self.set_value("pve.token_id", value)?;
        }
        if let Some(value) = &overrides.pve_token_secret {
            self.set_value("pve.token_secret", value)?;
        }
        if let Some(value) = &overrides.pve_tls_insecure {
            self.pve.tls_insecure = *value;
        }
        if let Some(value) = &overrides.agent_binary {
            self.set_value("agent.binary", value)?;
        }
        if let Some(value) = &overrides.agent_port {
            self.set_value("agent.port", value)?;
        }
        if let Some(value) = &overrides.recipes_repository {
            self.set_value("recipes.repository", value)?;
        }
        if let Some(value) = &overrides.recipes_reference {
            self.set_value("recipes.ref", value)?;
        }
        if let Some(value) = &overrides.recipes_auto_sync {
            self.recipes.auto_sync = *value;
        }
        if let Some(value) = &overrides.recipes_sync_ttl {
            self.set_value("recipes.sync-ttl", value)?;
        }
        if let Some(value) = &overrides.recipes_snapshot_before_apply {
            self.set_value("recipes.snapshot-before-apply", value)?;
        }
        if let Some(value) = &overrides.recipes_rollback_on_failure {
            self.recipes.rollback_on_failure = *value;
        }
        if let Some(value) = &overrides.vmid_pattern {
            self.set_value("vmid_pattern", value)?;
        }
        Ok(())
    }
}

fn optional_value(value: &Option<String>) -> String {
    value.clone().unwrap_or_else(|| "<unset>".to_owned())
}
fn redact_url(value: &Option<String>) -> Option<String> {
    let value = value.as_ref()?;
    let Ok(mut parsed) = Url::parse(value) else {
        return Some("<invalid>".to_owned());
    };
    if !parsed.username().is_empty() {
        let _ = parsed.set_username("");
    }
    if parsed.password().is_some() {
        let _ = parsed.set_password(None);
    }
    if parsed.query().is_some() {
        parsed.set_query(None);
    }
    if parsed.fragment().is_some() {
        parsed.set_fragment(None);
    }
    Some(parsed.to_string())
}

fn validate_non_empty(key: &str, value: &str, reason: &str) -> Result<(), ConfigError> {
    if value.trim().is_empty() {
        return Err(ConfigError::InvalidValue {
            key: key.to_owned(),
            reason: reason.to_owned(),
        });
    }
    Ok(())
}

fn validate_pve_url(key: &str, value: &str) -> Result<(), ConfigError> {
    if value != value.trim() {
        return Err(ConfigError::InvalidValue {
            key: key.to_owned(),
            reason: "URL must not have surrounding whitespace".to_owned(),
        });
    }
    let parsed = Url::parse(value.trim()).map_err(|_| ConfigError::InvalidValue {
        key: key.to_owned(),
        reason: "URL must be a valid HTTPS URL".to_owned(),
    })?;
    if parsed.scheme() != "https" || parsed.host_str().is_none() {
        return Err(ConfigError::InvalidValue {
            key: key.to_owned(),
            reason: "URL must use HTTPS and include a host".to_owned(),
        });
    }
    if !parsed.username().is_empty()
        || parsed.password().is_some()
        || parsed.query().is_some()
        || parsed.fragment().is_some()
    {
        return Err(ConfigError::InvalidValue {
            key: key.to_owned(),
            reason: "URL must not contain credentials, query, or fragment data".to_owned(),
        });
    }
    Ok(())
}

fn validate_snapshot_policy(key: &str, value: &str) -> Result<(), ConfigError> {
    if matches!(value, "auto" | "always" | "never") {
        return Ok(());
    }
    Err(ConfigError::InvalidValue {
        key: key.to_owned(),
        reason: "expected auto, always, or never".to_owned(),
    })
}

fn parse_bool(key: &str, value: &str) -> Result<bool, ConfigError> {
    value
        .parse::<bool>()
        .map_err(|_| ConfigError::InvalidValue {
            key: key.to_owned(),
            reason: "expected true or false".to_owned(),
        })
}

fn parse_port(key: &str, value: &str) -> Result<u16, ConfigError> {
    let port = value
        .parse::<u16>()
        .map_err(|_| ConfigError::InvalidValue {
            key: key.to_owned(),
            reason: "expected a TCP port from 1 to 65535".to_owned(),
        })?;
    if port == 0 {
        return Err(ConfigError::InvalidValue {
            key: key.to_owned(),
            reason: "expected a TCP port from 1 to 65535".to_owned(),
        });
    }
    Ok(port)
}

fn validate_repository_reference(key: &str, value: &str) -> Result<(), ConfigError> {
    validate_non_empty(key, value, "recipe repository cannot be empty")?;
    if value.contains('?') || value.contains('#') {
        return Err(ConfigError::InvalidValue {
            key: key.to_owned(),
            reason: "recipe repository must not contain query or fragment data".to_owned(),
        });
    }
    if value.contains("::") {
        return Err(ConfigError::InvalidValue {
            key: key.to_owned(),
            reason: "recipe repository must not use Git external transport".to_owned(),
        });
    }
    if let Some((scheme, authority_and_path)) = value.split_once("://") {
        let scheme = scheme.to_ascii_lowercase();
        if !matches!(scheme.as_str(), "https" | "ssh" | "file") {
            return Err(ConfigError::InvalidValue {
                key: key.to_owned(),
                reason: "recipe repository URLs must use HTTPS, SSH, or file transport".to_owned(),
            });
        }
        let authority = authority_and_path
            .split(['/', '\\'])
            .next()
            .unwrap_or_default();
        if authority.contains('@') {
            return Err(ConfigError::InvalidValue {
                key: key.to_owned(),
                reason: "recipe repository URLs must not contain embedded credentials".to_owned(),
            });
        }
        if scheme != "file" && authority.is_empty() {
            return Err(ConfigError::InvalidValue {
                key: key.to_owned(),
                reason: "recipe repository URL must include a host".to_owned(),
            });
        }
    }
    Ok(())
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ConfigOverrides {
    pub pve_url: Option<String>,
    pub pve_token_id: Option<String>,
    pub pve_token_secret: Option<String>,
    pub pve_tls_insecure: Option<bool>,
    pub agent_binary: Option<String>,
    pub agent_port: Option<String>,
    pub recipes_repository: Option<String>,
    pub recipes_reference: Option<String>,
    pub recipes_auto_sync: Option<bool>,
    pub recipes_sync_ttl: Option<String>,
    pub recipes_snapshot_before_apply: Option<String>,
    pub recipes_rollback_on_failure: Option<bool>,
    pub vmid_pattern: Option<String>,
}

#[derive(Debug, Error)]
pub enum ConfigError {
    #[error("could not read config file {path}: {source}")]
    Read {
        path: PathBuf,
        source: std::io::Error,
    },
    #[error("could not write config file {path}: {source}")]
    Write {
        path: PathBuf,
        source: std::io::Error,
    },
    #[error("invalid TOML in config file: {0}")]
    Parse(#[from] toml::de::Error),
    #[error("could not serialize config: {0}")]
    Serialize(#[from] toml::ser::Error),
    #[error("unknown config key: {0}")]
    UnknownKey(String),
    #[error("invalid value for {key}: {reason}")]
    InvalidValue { key: String, reason: String },
}

pub fn default_config_path() -> PathBuf {
    dirs::config_dir()
        .unwrap_or_else(|| PathBuf::from("."))
        .join("pbox")
        .join("config.toml")
}

#[derive(Debug, Clone)]
pub struct ConfigStore {
    path: PathBuf,
}

impl ConfigStore {
    pub fn new(path: impl Into<PathBuf>) -> Self {
        Self { path: path.into() }
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    pub fn load_file(&self) -> Result<Config, ConfigError> {
        load_file(&self.path)
    }

    pub fn load(&self, overrides: &ConfigOverrides) -> Result<Config, ConfigError> {
        let mut config = self.load_file()?;
        apply_environment(&mut config)?;
        config.apply_overrides(overrides)?;
        config.validate()?;
        Ok(config)
    }

    pub fn save(&self, config: &Config) -> Result<(), ConfigError> {
        save_file(&self.path, config)
    }
}

impl Default for ConfigStore {
    fn default() -> Self {
        Self::new(default_config_path())
    }
}

pub fn load_file(path: &Path) -> Result<Config, ConfigError> {
    match fs::read_to_string(path) {
        Ok(contents) => {
            let config: Config = toml::from_str(&contents)?;
            config.validate()?;
            Ok(config)
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(Config::default()),
        Err(source) => Err(ConfigError::Read {
            path: path.to_owned(),
            source,
        }),
    }
}

pub fn save_file(path: &Path, config: &Config) -> Result<(), ConfigError> {
    config.validate()?;
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent).map_err(|source| ConfigError::Write {
            path: path.to_owned(),
            source,
        })?;
    }
    if let Ok(metadata) = fs::symlink_metadata(path)
        && metadata.file_type().is_symlink()
    {
        return Err(ConfigError::Write {
            path: path.to_owned(),
            source: std::io::Error::new(
                std::io::ErrorKind::InvalidInput,
                "refusing to write through a symbolic link",
            ),
        });
    }
    let contents = toml::to_string_pretty(config)?;
    fs::write(path, format!("{contents}\n")).map_err(|source| ConfigError::Write {
        path: path.to_owned(),
        source,
    })?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let permissions = fs::Permissions::from_mode(0o600);
        fs::set_permissions(path, permissions).map_err(|source| ConfigError::Write {
            path: path.to_owned(),
            source,
        })?;
    }
    Ok(())
}

fn apply_environment(config: &mut Config) -> Result<(), ConfigError> {
    let mut overrides = ConfigOverrides::default();
    for (key, value) in std::env::vars() {
        match key.as_str() {
            "PBOX_PVE_URL" => overrides.pve_url = Some(value),
            "PBOX_PVE_TOKEN_ID" => overrides.pve_token_id = Some(value),
            "PBOX_PVE_TOKEN_SECRET" => overrides.pve_token_secret = Some(value),
            "PBOX_PVE_TLS_INSECURE" => {
                overrides.pve_tls_insecure = Some(parse_bool("PBOX_PVE_TLS_INSECURE", &value)?)
            }
            "PBOX_AGENT_BINARY" => overrides.agent_binary = Some(value),
            "PBOX_AGENT_PORT" => overrides.agent_port = Some(value),
            "PBOX_RECIPES_REPOSITORY" => overrides.recipes_repository = Some(value),
            "PBOX_RECIPES_REF" => overrides.recipes_reference = Some(value),
            "PBOX_RECIPES_AUTO_SYNC" => {
                overrides.recipes_auto_sync = Some(parse_bool("PBOX_RECIPES_AUTO_SYNC", &value)?)
            }
            "PBOX_RECIPES_SYNC_TTL" => overrides.recipes_sync_ttl = Some(value),
            "PBOX_RECIPES_SNAPSHOT_BEFORE_APPLY" => {
                overrides.recipes_snapshot_before_apply = Some(value)
            }
            "PBOX_RECIPES_ROLLBACK_ON_FAILURE" => {
                overrides.recipes_rollback_on_failure =
                    Some(parse_bool("PBOX_RECIPES_ROLLBACK_ON_FAILURE", &value)?)
            }
            "PBOX_VMID_PATTERN" => overrides.vmid_pattern = Some(value),
            _ => {}
        }
    }
    config.apply_overrides(&overrides)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::{SystemTime, UNIX_EPOCH};

    fn temporary_path() -> PathBuf {
        let suffix = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        std::env::temp_dir().join(format!("pbox-config-test-{suffix}.toml"))
    }

    #[test]
    fn defaults_are_safe_and_secrets_are_redacted() {
        let mut config = Config::default();
        config
            .set_value("pve.token_secret", "do-not-print")
            .unwrap();
        let json = serde_json::to_string(&config.redacted()).unwrap();
        assert!(!json.contains("do-not-print"));
        assert!(json.contains("<redacted>"));
    }

    #[test]
    fn file_then_environment_then_cli_precedence_is_explicit() {
        let path = temporary_path();
        let mut file = Config::default();
        file.set_value("pve.url", "https://file.example").unwrap();
        save_file(&path, &file).unwrap();
        let mut resolved = load_file(&path).unwrap();
        resolved
            .apply_overrides(&ConfigOverrides {
                pve_url: Some("https://environment.example".to_owned()),
                ..ConfigOverrides::default()
            })
            .unwrap();
        resolved
            .apply_overrides(&ConfigOverrides {
                pve_url: Some("https://cli.example".to_owned()),
                ..ConfigOverrides::default()
            })
            .unwrap();
        assert_eq!(resolved.pve.url.as_deref(), Some("https://cli.example"));
        let _ = fs::remove_file(path);
    }
    #[test]
    fn url_and_repository_transports_reject_credential_leaks_and_plain_http() {
        let mut config = Config::default();
        assert!(
            config
                .set_value("pve.url", "https://user:secret@pve.example")
                .is_err()
        );
        assert!(
            config
                .set_value("recipes.repository", "http://example.test/recipes.git")
                .is_err()
        );
        assert!(
            config
                .set_value("recipes.repository", "ext::sh -c evil")
                .is_err()
        );
        config.pve.url = Some("https://user:secret@pve.example".to_owned());
        assert!(config.validate().is_err());
    }

    #[cfg(unix)]
    #[test]
    fn save_rejects_symlinked_config_path() {
        use std::os::unix::fs::symlink;

        let suffix = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let target = std::env::temp_dir().join(format!("pbox-config-target-{suffix}.toml"));
        let link = std::env::temp_dir().join(format!("pbox-config-link-{suffix}.toml"));
        fs::write(&target, "original\n").unwrap();
        symlink(&target, &link).unwrap();

        let result = save_file(&link, &Config::default());

        assert!(result.is_err());
        assert_eq!(fs::read_to_string(&target).unwrap(), "original\n");
        fs::remove_file(&link).unwrap();
        fs::remove_file(&target).unwrap();
    }

    #[test]
    fn duration_parser_accepts_units_and_rejects_invalid_values() {
        assert_eq!(parse_duration("15m").unwrap(), Duration::from_secs(900));
        assert_eq!(parse_duration("2h").unwrap(), Duration::from_secs(7_200));
        assert_eq!(parse_duration("3d").unwrap(), Duration::from_secs(259_200));
        assert_eq!(parse_duration("30").unwrap(), Duration::from_secs(30));
        assert!(parse_duration("").is_err());
        assert!(parse_duration("15x").is_err());
        assert!(parse_duration("xm").is_err());
    }

    #[test]
    fn invalid_recipe_sync_ttl_is_rejected_when_setting_or_loading() {
        let mut config = Config::default();
        assert!(config.set_value("recipes.sync-ttl", "15x").is_err());

        let path = temporary_path();
        fs::write(&path, "[recipes]\nsync_ttl = \"15x\"\n").unwrap();
        assert!(load_file(&path).is_err());
        fs::remove_file(path).unwrap();
    }
}

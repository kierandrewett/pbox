use crate::VmidPattern;
use serde::{Deserialize, Deserializer, Serialize, Serializer};
use std::collections::BTreeMap;
use std::fmt;
use std::fs;
use std::path::{Path, PathBuf};
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
pub struct Config {
    pub pve: PveConfig,
    pub vmid_pattern: VmidPattern,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            pve: PveConfig::default(),
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
pub struct RedactedConfig {
    pub pve: RedactedPveConfig,
    pub vmid_pattern: VmidPattern,
}

impl Config {
    pub fn redacted(&self) -> RedactedConfig {
        RedactedConfig {
            pve: RedactedPveConfig {
                url: self.pve.url.clone(),
                token_id: self.pve.token_id.clone(),
                token_secret: self
                    .pve
                    .token_secret
                    .as_ref()
                    .map(|_| "<redacted>".to_owned()),
                tls_insecure: self.pve.tls_insecure,
            },
            vmid_pattern: self.vmid_pattern.clone(),
        }
    }

    pub fn redacted_pairs(&self) -> BTreeMap<String, String> {
        let mut values = BTreeMap::new();
        values.insert("pve.url".to_owned(), optional_value(&self.pve.url));
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
        values.insert("vmid_pattern".to_owned(), self.vmid_pattern.to_string());
        values
    }

    pub fn get_redacted(&self, key: &str) -> Option<String> {
        self.redacted_pairs().remove(key)
    }

    pub fn set_value(&mut self, key: &str, value: &str) -> Result<(), ConfigError> {
        match key {
            "pve.url" => {
                if value.trim().is_empty() {
                    return Err(ConfigError::InvalidValue {
                        key: key.to_owned(),
                        reason: "URL cannot be empty".to_owned(),
                    });
                }
                self.pve.url = Some(value.to_owned());
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
        if let Some(value) = &overrides.vmid_pattern {
            self.set_value("vmid_pattern", value)?;
        }
        Ok(())
    }
}

fn optional_value(value: &Option<String>) -> String {
    value.clone().unwrap_or_else(|| "<unset>".to_owned())
}

fn parse_bool(key: &str, value: &str) -> Result<bool, ConfigError> {
    value
        .parse::<bool>()
        .map_err(|_| ConfigError::InvalidValue {
            key: key.to_owned(),
            reason: "expected true or false".to_owned(),
        })
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ConfigOverrides {
    pub pve_url: Option<String>,
    pub pve_token_id: Option<String>,
    pub pve_token_secret: Option<String>,
    pub pve_tls_insecure: Option<bool>,
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
        Ok(contents) => Ok(toml::from_str(&contents)?),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(Config::default()),
        Err(source) => Err(ConfigError::Read {
            path: path.to_owned(),
            source,
        }),
    }
}

pub fn save_file(path: &Path, config: &Config) -> Result<(), ConfigError> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent).map_err(|source| ConfigError::Write {
            path: path.to_owned(),
            source,
        })?;
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
}

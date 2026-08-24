use rand::Rng;
use serde::de::Error as DeError;
use serde::{Deserialize, Deserializer, Serialize, Serializer};
use std::fmt;
use std::str::FromStr;
use thiserror::Error;

const PREFIX: &str = "pbx_";
const LENGTH: usize = 8;
// Crockford-style lowercase alphabet: ambiguous i/l/o/u are intentionally omitted.
const ALPHABET: &[u8] = b"0123456789abcdefghjkmnpqrstvwxyz";

#[derive(Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct PboxId(String);

#[derive(Debug, Error, Clone, PartialEq, Eq)]
pub enum PboxIdError {
    #[error("public id must start with pbx_")]
    InvalidPrefix,
    #[error("public id must contain exactly 8 characters after pbx_")]
    InvalidLength,
    #[error("public id contains an invalid character: {0}")]
    InvalidCharacter(char),
}

impl PboxId {
    pub fn generate() -> Self {
        let mut rng = rand::thread_rng();
        let value: String = (0..LENGTH)
            .map(|_| {
                let index = rng.gen_range(0..ALPHABET.len());
                ALPHABET[index] as char
            })
            .collect();
        Self(format!("{PREFIX}{value}"))
    }

    pub fn parse(value: &str) -> Result<Self, PboxIdError> {
        if !value.starts_with(PREFIX) {
            return Err(PboxIdError::InvalidPrefix);
        }
        let suffix = &value[PREFIX.len()..];
        if suffix.chars().count() != LENGTH {
            return Err(PboxIdError::InvalidLength);
        }
        for character in suffix.chars() {
            if !ALPHABET.contains(&(character as u8)) {
                return Err(PboxIdError::InvalidCharacter(character));
            }
        }
        Ok(Self(value.to_owned()))
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl fmt::Display for PboxId {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(&self.0)
    }
}

impl FromStr for PboxId {
    type Err = PboxIdError;

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        Self::parse(value)
    }
}

impl Serialize for PboxId {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        serializer.serialize_str(self.as_str())
    }
}

impl<'de> Deserialize<'de> for PboxId {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        let value = String::deserialize(deserializer)?;
        Self::parse(&value).map_err(D::Error::custom)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn generated_ids_have_the_stable_shape() {
        let id = PboxId::generate();
        assert!(id.as_str().starts_with("pbx_"));
        assert_eq!(id.as_str().len(), 12);
        assert_eq!(PboxId::parse(id.as_str()).expect("generated id"), id);
    }

    #[test]
    fn parser_rejects_ambiguous_or_malformed_values() {
        for value in [
            "box_t3yzd9y3",
            "pbx_t3yzd9y",
            "pbx_t3yzd9y3!",
            "pbx_T3yzd9y3",
        ] {
            assert!(PboxId::parse(value).is_err(), "accepted {value}");
        }
    }
}

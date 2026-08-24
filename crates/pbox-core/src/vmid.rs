use serde::de::Error as DeError;
use serde::{Deserialize, Deserializer, Serialize, Serializer};
use std::borrow::Borrow;
use std::fmt;
use std::str::FromStr;
use thiserror::Error;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct VmidPattern {
    prefix: u64,
    wildcard_digits: u32,
    first: u64,
    last: u64,
}

#[derive(Debug, Error, Clone, PartialEq, Eq)]
pub enum VmidError {
    #[error("VMID pattern must match ^[1-9][0-9]*x+$")]
    InvalidPattern,
    #[error("VMID pattern is too large for a 64-bit VMID")]
    Overflow,
    #[error("all VMIDs in pattern {0} are already in use")]
    Exhausted(String),
}

impl VmidPattern {
    pub fn parse(pattern: &str) -> Result<Self, VmidError> {
        let bytes = pattern.as_bytes();
        if bytes.len() < 2 || !bytes[0].is_ascii_digit() || bytes[0] == b'0' {
            return Err(VmidError::InvalidPattern);
        }

        let wildcard_start = bytes
            .iter()
            .position(|byte| *byte == b'x')
            .ok_or(VmidError::InvalidPattern)?;
        if wildcard_start == 0
            || bytes[wildcard_start..].iter().any(|byte| *byte != b'x')
            || bytes[..wildcard_start]
                .iter()
                .any(|byte| !byte.is_ascii_digit())
        {
            return Err(VmidError::InvalidPattern);
        }

        let prefix_text = &pattern[..wildcard_start];
        if prefix_text.len() > 1 && prefix_text.starts_with('0') {
            return Err(VmidError::InvalidPattern);
        }
        let prefix = prefix_text
            .parse::<u64>()
            .map_err(|_| VmidError::Overflow)?;
        let wildcard_digits =
            u32::try_from(bytes.len() - wildcard_start).map_err(|_| VmidError::Overflow)?;
        let factor = 10u64
            .checked_pow(wildcard_digits)
            .ok_or(VmidError::Overflow)?;
        let first = prefix.checked_mul(factor).ok_or(VmidError::Overflow)?;
        let last = first.checked_add(factor - 1).ok_or(VmidError::Overflow)?;

        Ok(Self {
            prefix,
            wildcard_digits,
            first,
            last,
        })
    }

    pub fn prefix(&self) -> u64 {
        self.prefix
    }

    pub fn wildcard_digits(&self) -> u32 {
        self.wildcard_digits
    }

    pub fn first(&self) -> u64 {
        self.first
    }

    pub fn last(&self) -> u64 {
        self.last
    }

    pub fn contains(&self, vmid: u64) -> bool {
        (self.first..=self.last).contains(&vmid)
    }

    pub fn allocate_lowest<I>(&self, used: I) -> Result<u64, VmidError>
    where
        I: IntoIterator,
        I::Item: Borrow<u64>,
    {
        let used: std::collections::BTreeSet<u64> =
            used.into_iter().map(|value| *value.borrow()).collect();
        let mut candidate = self.first;
        loop {
            if !used.contains(&candidate) {
                return Ok(candidate);
            }
            if candidate == self.last {
                return Err(VmidError::Exhausted(self.to_string()));
            }
            candidate += 1;
        }
    }
}

impl fmt::Display for VmidPattern {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            formatter,
            "{}{}",
            self.prefix,
            "x".repeat(self.wildcard_digits as usize)
        )
    }
}

impl FromStr for VmidPattern {
    type Err = VmidError;

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        Self::parse(value)
    }
}

impl Serialize for VmidPattern {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        serializer.serialize_str(&self.to_string())
    }
}

impl<'de> Deserialize<'de> for VmidPattern {
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
    fn pattern_exposes_expected_range() {
        let pattern = VmidPattern::parse("9xxx").expect("valid pattern");
        assert_eq!(pattern.first(), 9000);
        assert_eq!(pattern.last(), 9999);
        assert!(pattern.contains(9000));
        assert!(!pattern.contains(8999));
    }

    #[test]
    fn allocation_returns_lowest_unused_candidate() {
        let pattern = VmidPattern::parse("42xx").expect("valid pattern");
        assert_eq!(pattern.allocate_lowest([4202, 4200, 4201]), Ok(4203));
    }

    #[test]
    fn invalid_patterns_and_exhaustion_are_reported() {
        for value in ["0xxx", "9", "9xx9", "xx9", "09xx"] {
            assert!(VmidPattern::parse(value).is_err(), "accepted {value}");
        }
        let pattern = VmidPattern::parse("1x").expect("valid pattern");
        assert!(matches!(
            pattern.allocate_lowest([10, 11, 12, 13, 14, 15, 16, 17, 18, 19]),
            Err(VmidError::Exhausted(_))
        ));
    }
}

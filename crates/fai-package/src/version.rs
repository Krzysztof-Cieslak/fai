//! Explicit semantic-version requirements with per-release prerelease opt-in.

use std::{fmt, str::FromStr};

use semver::{Comparator, Op, Version, VersionReq};
use serde::{Serialize, Serializer};

use crate::MetadataError;

/// A complete package version, excluding semantically unordered build metadata.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct PackageVersion(Version);

impl PackageVersion {
    /// Parses a complete major/minor/patch version and optional prerelease.
    pub fn parse(text: &str) -> Result<Self, MetadataError> {
        let version =
            Version::parse(text).map_err(|error| MetadataError::new(error.to_string()))?;
        if !version.build.is_empty() {
            return Err(MetadataError::new("package versions cannot contain build metadata"));
        }
        Ok(Self(version))
    }

    /// Returns the semantic version used for ordering and matching.
    #[must_use]
    pub fn as_semver(&self) -> &Version {
        &self.0
    }
}

impl FromStr for PackageVersion {
    type Err = MetadataError;

    fn from_str(text: &str) -> Result<Self, Self::Err> {
        Self::parse(text)
    }
}

impl fmt::Display for PackageVersion {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        self.0.fmt(f)
    }
}

impl Serialize for PackageVersion {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.collect_str(self)
    }
}

impl<'de> serde::Deserialize<'de> for PackageVersion {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let text = <String as serde::Deserialize>::deserialize(deserializer)?;
        Self::parse(&text).map_err(serde::de::Error::custom)
    }
}

/// A union of comparator intersections. Bare versions are exact, never caret ranges.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct VersionRequirement {
    alternatives: Vec<VersionReq>,
}

impl VersionRequirement {
    /// Parses full-version comparators, comma intersections, and `||` alternatives.
    pub fn parse(text: &str) -> Result<Self, MetadataError> {
        let mut alternatives = Vec::new();
        for arm in text.split("||") {
            let mut comparators = Vec::new();
            for token in arm.split(',') {
                let token = token.trim();
                if token.is_empty() {
                    return Err(MetadataError::new("expected a version comparator"));
                }
                if token == "*" {
                    continue;
                }
                if token.contains('+') {
                    return Err(MetadataError::new("requirements cannot contain build metadata"));
                }
                let explicit = if token.as_bytes()[0].is_ascii_digit() {
                    format!("={token}")
                } else {
                    token.to_owned()
                };
                let comparator = Comparator::parse(&explicit)
                    .map_err(|error| MetadataError::new(error.to_string()))?;
                if comparator.minor.is_none()
                    || comparator.patch.is_none()
                    || comparator.op == Op::Wildcard
                {
                    return Err(MetadataError::new("write a full major.minor.patch version, or *"));
                }
                comparators.push(comparator);
            }
            comparators.sort_by_cached_key(|comparator| {
                (operator_order(comparator.op), comparator.to_string())
            });
            comparators.dedup();
            alternatives.push(VersionReq { comparators });
        }
        alternatives.sort_by_cached_key(ToString::to_string);
        alternatives.dedup();
        Ok(Self { alternatives })
    }

    /// Whether any arm admits the version, including that arm's prerelease opt-in.
    #[must_use]
    pub fn matches(&self, version: &PackageVersion) -> bool {
        self.alternatives.iter().any(|arm| arm.matches(version.as_semver()))
    }
}

fn operator_order(op: Op) -> u8 {
    match op {
        Op::Exact => 0,
        Op::Caret => 1,
        Op::Tilde => 2,
        Op::GreaterEq => 3,
        Op::Greater => 4,
        Op::Less => 5,
        Op::LessEq => 6,
        _ => 7,
    }
}

impl Default for VersionRequirement {
    fn default() -> Self {
        Self { alternatives: vec![VersionReq::STAR] }
    }
}

impl FromStr for VersionRequirement {
    type Err = MetadataError;

    fn from_str(text: &str) -> Result<Self, Self::Err> {
        Self::parse(text)
    }
}

impl fmt::Display for VersionRequirement {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        for (index, arm) in self.alternatives.iter().enumerate() {
            if index != 0 {
                f.write_str(" || ")?;
            }
            arm.fmt(f)?;
        }
        Ok(())
    }
}

impl Serialize for VersionRequirement {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.collect_str(self)
    }
}

impl<'de> serde::Deserialize<'de> for VersionRequirement {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let text = <String as serde::Deserialize>::deserialize(deserializer)?;
        Self::parse(&text).map_err(serde::de::Error::custom)
    }
}

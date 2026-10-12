//! Target descriptors and order-independent platform predicates.

use std::{fmt, str::FromStr};

use serde::{Serialize, Serializer};
use target_lexicon::{Architecture, Environment, OperatingSystem, Triple};

use crate::MetadataError;

/// A parsed target triple used independently of the process's host platform.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct TargetPlatform {
    triple: Triple,
    os: String,
    arch: String,
    environment: String,
}

impl TargetPlatform {
    /// Parses a target triple and derives canonical manifest selector values.
    pub fn parse(text: &str) -> Result<Self, MetadataError> {
        let triple =
            Triple::from_str(text).map_err(|error| MetadataError::new(error.to_string()))?;
        let os = normalize_os(&triple.operating_system.to_string());
        let arch = triple.architecture.to_string();
        let environment = if triple.environment == Environment::Unknown {
            String::new()
        } else {
            triple.environment.to_string()
        };
        Ok(Self { triple, os, arch, environment })
    }

    /// Manifest operating-system value (`macos` for Darwin targets).
    #[must_use]
    pub fn os(&self) -> &str {
        &self.os
    }

    /// Manifest architecture value.
    #[must_use]
    pub fn arch(&self) -> &str {
        &self.arch
    }

    /// Manifest environment value; empty when the target has no environment.
    #[must_use]
    pub fn environment(&self) -> &str {
        &self.environment
    }
}

impl fmt::Display for TargetPlatform {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        self.triple.fmt(f)
    }
}

impl Serialize for TargetPlatform {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.collect_str(self)
    }
}

/// A conjunction of exact target properties; omitted properties match any value.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize)]
pub struct PlatformPredicate {
    os: Option<String>,
    arch: Option<String>,
    environment: Option<String>,
}

impl PlatformPredicate {
    /// Validates a nonempty platform predicate.
    pub fn new(
        os: Option<&str>,
        arch: Option<&str>,
        environment: Option<&str>,
    ) -> Result<Self, MetadataError> {
        if os.is_none() && arch.is_none() && environment.is_none() {
            return Err(MetadataError::new("a platform predicate needs os, arch, or env"));
        }
        let os = os
            .map(|os| {
                if os == "macos" {
                    return Ok(os.to_owned());
                }
                let parsed = OperatingSystem::from_str(os)
                    .map_err(|_| MetadataError::new("unknown platform operating system"))?;
                Ok(normalize_os(&parsed.to_string()))
            })
            .transpose()?;
        let arch = arch
            .map(|arch| {
                Architecture::from_str(arch)
                    .map(|value| value.to_string())
                    .map_err(|_| MetadataError::new("unknown platform architecture"))
            })
            .transpose()?;
        let environment = environment
            .map(|env| {
                if env.is_empty() {
                    return Ok(String::new());
                }
                Environment::from_str(env)
                    .map(|value| {
                        if value == Environment::Unknown {
                            String::new()
                        } else {
                            value.to_string()
                        }
                    })
                    .map_err(|_| MetadataError::new("unknown platform environment"))
            })
            .transpose()?;
        Ok(Self { os, arch, environment })
    }

    /// Whether all specified properties match a target.
    #[must_use]
    pub fn matches(&self, target: &TargetPlatform) -> bool {
        self.os.as_deref().is_none_or(|os| os == target.os())
            && self.arch.as_deref().is_none_or(|arch| arch == target.arch())
            && self.environment.as_deref().is_none_or(|env| env == target.environment())
    }

    /// Whether two predicates can match the same target descriptor.
    #[must_use]
    pub fn overlaps(&self, other: &Self) -> bool {
        fn compatible(a: &Option<String>, b: &Option<String>) -> bool {
            a.is_none() || b.is_none() || a == b
        }
        compatible(&self.os, &other.os)
            && compatible(&self.arch, &other.arch)
            && compatible(&self.environment, &other.environment)
    }
}

fn normalize_os(os: &str) -> String {
    if os == "darwin" || os.starts_with("macosx") { "macos".to_owned() } else { os.to_owned() }
}

//! Canonical package coordinates and identifiers, independent of retrieval.

use std::{fmt, str::FromStr};

use serde::Serialize;
use url::Url;

use crate::{MetadataError, PackageVersion};

macro_rules! string_identity {
    ($name:ident, $description:literal, $validate:ident) => {
        #[doc = $description]
        #[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize)]
        #[serde(transparent)]
        pub struct $name(String);

        impl $name {
            /// Validates and normalizes an identifier.
            pub fn parse(text: &str) -> Result<Self, MetadataError> {
                $validate(text).map(Self)
            }

            /// Returns the canonical spelling.
            #[must_use]
            pub fn as_str(&self) -> &str {
                &self.0
            }
        }

        impl FromStr for $name {
            type Err = MetadataError;
            fn from_str(text: &str) -> Result<Self, Self::Err> {
                Self::parse(text)
            }
        }

        impl<'de> serde::Deserialize<'de> for $name {
            fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
                let text = <String as serde::Deserialize>::deserialize(deserializer)?;
                Self::parse(&text).map_err(serde::de::Error::custom)
            }
        }

        impl fmt::Display for $name {
            fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
                f.write_str(&self.0)
            }
        }
    };
}

string_identity!(PackageName, "A portable, lower-case package display name.", package_name);
string_identity!(DependencyAlias, "A file-importable dependency alias.", dependency_alias);
string_identity!(
    PackagePath,
    "A normalized, package-relative path with no escaping components.",
    package_path
);
string_identity!(
    GitRepository,
    "A canonical Git locator; authentication is supplied separately.",
    git_repository
);
string_identity!(GitRevision, "A complete SHA-1 or SHA-256 Git object ID.", git_revision);
string_identity!(GitBranch, "A literal Git branch name, not a revision expression.", git_branch);

fn package_name(text: &str) -> Result<String, MetadataError> {
    if !text.as_bytes().first().is_some_and(u8::is_ascii_lowercase)
        || text.split('-').any(|part| {
            part.is_empty() || !part.bytes().all(|c| c.is_ascii_lowercase() || c.is_ascii_digit())
        })
    {
        return Err(MetadataError::new(
            "expected a lower-case package name with optional hyphen-separated components",
        ));
    }
    Ok(text.to_owned())
}

fn dependency_alias(text: &str) -> Result<String, MetadataError> {
    let valid = text.as_bytes().first().is_some_and(|c| c.is_ascii_alphabetic() || *c == b'_')
        && text.bytes().all(|c| c.is_ascii_alphanumeric() || c == b'_');
    let reserved = matches!(
        text,
        "_" | "module"
            | "let"
            | "type"
            | "interface"
            | "match"
            | "with"
            | "if"
            | "then"
            | "else"
            | "fun"
            | "public"
            | "internal"
            | "opaque"
            | "example"
            | "forall"
            | "as"
            | "foreign"
            | "import"
            | "true"
            | "false"
    );
    if !valid || reserved {
        return Err(MetadataError::new(
            "expected a non-keyword Fai identifier for the dependency alias",
        ));
    }
    Ok(text.to_owned())
}

fn package_path(text: &str) -> Result<String, MetadataError> {
    if text.is_empty()
        || text.starts_with('/')
        || text.contains(['\\', ':'])
        || text.chars().any(char::is_control)
    {
        return Err(MetadataError::new(
            "expected a nonempty package-relative path using / separators",
        ));
    }
    let mut parts = Vec::new();
    for part in text.split('/') {
        match part {
            "" | "." => {}
            ".." => {
                if parts.pop().is_none() {
                    return Err(MetadataError::new("path escapes its package directory"));
                }
            }
            part if part.eq_ignore_ascii_case(".git") => {
                return Err(MetadataError::new("package paths cannot enter Git metadata"));
            }
            part => parts.push(part),
        }
    }
    Ok(if parts.is_empty() { ".".to_owned() } else { parts.join("/") })
}

impl PackagePath {
    /// The package directory itself.
    #[must_use]
    pub fn root() -> Self {
        Self(".".to_owned())
    }

    /// Appends a previously validated relative path.
    #[must_use]
    pub fn join(&self, child: &Self) -> Self {
        if self.is_root() {
            child.clone()
        } else if child.is_root() {
            self.clone()
        } else {
            Self(format!("{self}/{child}"))
        }
    }

    /// Whether this path denotes the package root.
    #[must_use]
    pub fn is_root(&self) -> bool {
        self.0 == "."
    }

    /// Whether this path is equal to or below `parent`, by path components.
    #[must_use]
    pub fn is_within(&self, parent: &Self) -> bool {
        parent.is_root()
            || self == parent
            || self.0.strip_prefix(parent.as_str()).is_some_and(|rest| rest.starts_with('/'))
    }

    /// The number of non-root path components.
    #[must_use]
    pub fn depth(&self) -> usize {
        if self.is_root() { 0 } else { self.0.split('/').count() }
    }

    /// The final component, or `None` for the package root.
    #[must_use]
    pub fn file_name(&self) -> Option<&str> {
        if self.is_root() { None } else { self.0.rsplit('/').next() }
    }

    /// The parent directory, or `None` for the package root.
    #[must_use]
    pub fn parent(&self) -> Option<Self> {
        if self.is_root() {
            None
        } else {
            Some(
                self.0
                    .rsplit_once('/')
                    .map_or_else(Self::root, |(parent, _)| Self(parent.to_owned())),
            )
        }
    }
}

fn git_repository(text: &str) -> Result<String, MetadataError> {
    if text.is_empty()
        || text.starts_with('-')
        || text.contains('\\')
        || text.chars().any(|c| c.is_whitespace() || c.is_control())
    {
        return Err(MetadataError::new("expected a Git HTTPS, SSH, or local file locator"));
    }
    if !text.contains("://") && !text.starts_with("file:") {
        return scp_repository(text);
    }
    let mut url = Url::parse(text).map_err(|_| MetadataError::new("invalid Git repository URL"))?;
    if !matches!(url.scheme(), "https" | "ssh" | "file") {
        return Err(MetadataError::new("Git repository URLs must use https, ssh, or file"));
    }
    if url.query().is_some()
        || url.fragment().is_some()
        || url.password().is_some()
        || (url.scheme() != "ssh" && !url.username().is_empty())
    {
        return Err(MetadataError::new(
            "use Git authentication configuration instead of URL credentials, queries, or fragments",
        ));
    }
    if url.path().is_empty() || url.path() == "/" {
        return Err(MetadataError::new("repository URL must identify a repository path"));
    }
    if url.scheme() != "file" && url.host_str().is_none() {
        return Err(MetadataError::new("repository URL must identify a host"));
    }
    if url.host_str() == Some("github.com")
        && ((url.scheme() == "https" && url.port().is_none())
            || (url.scheme() == "ssh"
                && matches!(url.port(), None | Some(22))
                && url.username() == "git"))
    {
        return github_repository(url.path());
    }
    if url.scheme() == "ssh" && url.port() == Some(22) {
        let _ = url.set_port(None);
    }
    Ok(url.to_string())
}

fn github_repository(path: &str) -> Result<String, MetadataError> {
    let path = path.trim_matches('/').to_ascii_lowercase();
    let path = path.strip_suffix(".git").unwrap_or(&path);
    let mut parts = path.split('/');
    let owner = parts.next().unwrap_or("");
    let repo = parts.next().unwrap_or("");
    if parts.next().is_some()
        || owner.is_empty()
        || repo.is_empty()
        || !owner.bytes().all(|c| c.is_ascii_alphanumeric() || c == b'-')
        || !repo.bytes().all(|c| c.is_ascii_alphanumeric() || matches!(c, b'-' | b'_' | b'.'))
        || matches!(repo, "." | "..")
    {
        return Err(MetadataError::new("GitHub locators must identify owner/repository"));
    }
    Ok(format!("https://github.com/{owner}/{repo}.git"))
}

fn scp_repository(text: &str) -> Result<String, MetadataError> {
    let (authority, path) = if let Some(end) = text.find("]:") {
        (&text[..end + 1], &text[end + 2..])
    } else {
        text.split_once(':').ok_or_else(|| MetadataError::new("expected a Git repository URL"))?
    };
    let (user, host) =
        authority.rsplit_once('@').map_or((None, authority), |(user, host)| (Some(user), host));
    if path.is_empty()
        || path.starts_with(['-', ':'])
        || host.is_empty()
        || (user.is_none() && host.len() == 1 && host.as_bytes()[0].is_ascii_alphabetic())
        || user.is_some_and(|user| {
            user.is_empty()
                || !user
                    .bytes()
                    .all(|c| c.is_ascii_alphanumeric() || matches!(c, b'_' | b'-' | b'.'))
        })
    {
        return Err(MetadataError::new("invalid SSH repository locator"));
    }
    let parsed_host =
        url::Host::parse(host).map_err(|_| MetadataError::new("invalid SSH repository host"))?;
    let host = parsed_host.to_string().to_ascii_lowercase();
    if host == "github.com" && user == Some("git") {
        return github_repository(path);
    }
    // SCP paths can be relative to a remote home; an ssh:// absolute path is not equivalent.
    Ok(match user {
        Some(user) => format!("{user}@{host}:{path}"),
        None => format!("{host}:{path}"),
    })
}

impl GitRepository {
    /// Expands a CLI GitHub shorthand, or validates an ordinary Git locator.
    pub fn from_cli(text: &str) -> Result<Self, MetadataError> {
        if let Some(path) = text.strip_prefix("github:") {
            github_repository(path).map(Self)
        } else {
            Self::parse(text)
        }
    }

    /// Whether the locator refers to a local, non-publishable Git repository.
    #[must_use]
    pub fn is_local(&self) -> bool {
        self.0.starts_with("file:")
    }
}

fn git_revision(text: &str) -> Result<String, MetadataError> {
    if !matches!(text.len(), 40 | 64) || !text.bytes().all(|c| c.is_ascii_hexdigit()) {
        return Err(MetadataError::new("expected a full 40- or 64-digit Git commit ID"));
    }
    Ok(text.to_ascii_lowercase())
}

fn git_branch(text: &str) -> Result<String, MetadataError> {
    if text.is_empty()
        || matches!(text, "@" | "HEAD")
        || text.starts_with('-')
        || !valid_ref_path(text)
    {
        return Err(MetadataError::new(
            "expected a literal Git branch name, not a revision expression",
        ));
    }
    Ok(text.to_owned())
}

fn valid_ref_path(text: &str) -> bool {
    !text.contains("..")
        && !text.contains("@{")
        && !text.ends_with('.')
        && !text.chars().any(|c| {
            c.is_control()
                || c.is_whitespace()
                || matches!(c, '~' | '^' | ':' | '?' | '*' | '[' | '\\')
        })
        && text
            .split('/')
            .all(|part| !part.is_empty() && !part.starts_with('.') && !part.ends_with(".lock"))
}

/// A logical package identified independently of its version, alias, and transport.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize)]
pub struct GitPackage {
    /// Canonical repository locator.
    pub repository: GitRepository,
    /// Package directory within the repository.
    pub subdir: PackagePath,
}

impl GitPackage {
    /// Computes the canonical path-scoped tag for a release.
    pub fn release_tag(&self, version: &PackageVersion) -> Result<String, MetadataError> {
        let tag = if self.subdir.is_root() {
            format!("v{version}")
        } else {
            format!("{}/v{version}", self.subdir)
        };
        if !valid_ref_path(&tag) {
            return Err(MetadataError::new("package directory cannot form a valid release tag"));
        }
        Ok(tag)
    }

    /// Recognizes only canonical release tags belonging to this package.
    #[must_use]
    pub fn version_from_tag(&self, tag: &str) -> Option<PackageVersion> {
        let prefix =
            if self.subdir.is_root() { "v".to_owned() } else { format!("{}/v", self.subdir) };
        let version = PackageVersion::parse(tag.strip_prefix(&prefix)?).ok()?;
        (self.release_tag(&version).ok()?.as_str() == tag).then_some(version)
    }
}

//! Fixed-tag release lookup and bounded, checksum-verified downloads.

use std::{
    io::{Read, Write},
    time::Duration,
};

use fai_toolchain::{Artifact, Release, Result, file_digest, invalid, validate_version};

const RELEASES: &str = "https://github.com/Krzysztof-Cieslak/fai/releases";

pub struct Client {
    agent: ureq::Agent,
    base: String,
}

impl Client {
    pub fn new() -> Result<Self> {
        let base = std::env::var("FAI_RELEASE_BASE_URL").unwrap_or_else(|_| RELEASES.into());
        validate_url(&base)?;
        Ok(Self {
            agent: ureq::AgentBuilder::new()
                .timeout(Duration::from_secs(120))
                .timeout_connect(Duration::from_secs(20))
                .build(),
            base: base.trim_end_matches('/').to_owned(),
        })
    }

    fn response(&self, url: &str) -> Result<ureq::Response> {
        validate_url(url)?;
        let response = self
            .agent
            .get(url)
            .set("User-Agent", concat!("fai-installer/", env!("CARGO_PKG_VERSION")))
            .call()
            .map_err(|error| invalid(format!("downloading {url}: {error}")))?;
        validate_url(response.get_url())?;
        Ok(response)
    }

    fn json(&self, url: &str) -> Result<Vec<u8>> {
        let mut bytes = Vec::new();
        self.response(url)?.into_reader().take(8 * 1024 * 1024 + 1).read_to_end(&mut bytes)?;
        if bytes.len() > 8 * 1024 * 1024 {
            return Err(invalid("release metadata exceeds the size limit"));
        }
        Ok(bytes)
    }

    pub fn release(&self, version: &str) -> Result<Release> {
        let url = if version == "stable" {
            format!("{}/latest/download/release.json", self.base)
        } else {
            validate_version(version)?;
            format!("{}/download/v{version}/release.json", self.base)
        };
        let release: Release = serde_json::from_slice(&self.json(&url)?)?;
        release.validate()?;
        if version != "stable" && version != release.version {
            return Err(invalid("release manifest does not match the requested version"));
        }
        if version == "stable" && release.version.contains('-') {
            return Err(invalid("stable channel points to a prerelease"));
        }
        Ok(release)
    }

    pub fn download(
        &self,
        release: &Release,
        artifact: &Artifact,
    ) -> Result<tempfile::NamedTempFile> {
        let url = format!("{}/download/{}/{}", self.base, release.tag, artifact.name);
        let mut archive = tempfile::NamedTempFile::new()?;
        let count = std::io::copy(
            &mut self.response(&url)?.into_reader().take(artifact.size + 1),
            &mut archive,
        )?;
        archive.flush()?;
        if count != artifact.size || file_digest(archive.path())? != artifact.sha256 {
            return Err(invalid(format!("checksum or size mismatch for {}", artifact.name)));
        }
        Ok(archive)
    }

    pub fn available(&self) -> Result<Vec<String>> {
        let mut versions = Vec::new();
        for page in 1..=100 {
            let url = if self.base == RELEASES {
                format!(
                    "https://api.github.com/repos/Krzysztof-Cieslak/fai/releases?per_page=100&page={page}"
                )
            } else {
                format!("{}/index.json?page={page}", self.base)
            };
            let releases: Vec<serde_json::Value> = serde_json::from_slice(&self.json(&url)?)?;
            for item in &releases {
                if item["draft"] == true {
                    continue;
                }
                if let Some(version) =
                    item["tag_name"].as_str().and_then(|tag| tag.strip_prefix('v'))
                    && let Ok(version) = validate_version(version)
                {
                    versions.push(version);
                }
            }
            if releases.len() < 100 {
                versions.sort();
                versions.dedup();
                versions.reverse();
                return Ok(versions.into_iter().map(|version| version.to_string()).collect());
            }
        }
        Err(invalid("too many release pages; choose an exact version"))
    }
}

fn validate_url(value: &str) -> Result<()> {
    let url = url::Url::parse(value).map_err(|error| invalid(error.to_string()))?;
    let loopback = matches!(url.host_str(), Some("localhost" | "127.0.0.1" | "[::1]" | "::1"));
    if (url.scheme() != "https" && !(url.scheme() == "http" && loopback))
        || !url.username().is_empty()
        || url.password().is_some()
    {
        return Err(invalid(
            "release URLs require HTTPS (loopback HTTP is supported for local testing)",
        ));
    }
    Ok(())
}

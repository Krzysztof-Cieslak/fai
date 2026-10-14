//! Candidate release validation and journaled, atomic Git publication.

use std::{
    collections::{BTreeMap, BTreeSet},
    io::Write,
    process::{Command, Stdio},
};

use camino::{Utf8Path, Utf8PathBuf};
use fai_package::{
    Dependency, GitPackage, GitRepository, GitRevision, ManifestDocument, PackagePath,
    PackageVersion, TargetPlatform,
};
use fai_package_store::{FileEdit, GitStore, MetadataTransaction, atomic_write, lock_file};
use serde::{Deserialize, Serialize};

use crate::project::{PrepareMode, PreparePolicy, ProjectOptions, prepare_request};
use crate::{DriverError, Session};

/// Explicit release intent and destination selection.
#[derive(Debug, Clone, Default)]
pub struct PublishOptions {
    /// One bump/exact version, or `package=intent` entries for a batch.
    pub intents: Vec<String>,
    /// Validate and report without changing worktree files or refs.
    pub plan: bool,
    /// Resume the exact prepared commit and tags from the journal.
    pub resume: bool,
    /// Existing Git remote name; inferred only when unambiguous.
    pub remote: Option<String>,
    /// Destination branch; defaults to the current upstream branch.
    pub branch: Option<String>,
    /// Explicit metadata target matrix, otherwise use the project declaration.
    pub targets: Vec<TargetPlatform>,
}

/// One version-only release edit.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PublishedPackage {
    /// Package display name.
    pub name: String,
    /// Repository-relative package directory.
    pub path: PackagePath,
    /// Previously authored version.
    pub before: PackageVersion,
    /// Explicitly requested version.
    pub version: PackageVersion,
    /// Canonical path-scoped tag.
    pub tag: String,
}

/// Advisory public declaration difference, never an automatic version bump.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ApiChange {
    /// Declaring module and exported name.
    pub name: String,
    /// Added, removed, or changed declaration.
    pub kind: String,
}

/// Stable publication result and resumable receipt payload.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PublishOutput {
    /// Result schema version.
    pub schema_version: u32,
    /// Whether the requested plan or publication completed.
    pub ok: bool,
    /// Planned, prepared, or published.
    pub status: String,
    /// Exact prepared release commit.
    pub commit: GitRevision,
    /// Validated Git tree; development locks are not rewritten with future IDs.
    pub tree: GitRevision,
    /// Destination remote name.
    pub remote: String,
    /// Destination branch.
    pub branch: String,
    /// Selected package versions and tags.
    pub packages: Vec<PublishedPackage>,
    /// Advisory declaration changes.
    pub api_changes: Vec<ApiChange>,
    /// False when re-export expansion or a baseline could not be compared fully.
    pub api_complete: bool,
    /// Host on which code and contracts were validated.
    pub validated_host: String,
    /// Other target graphs validated as metadata, not executable tests.
    pub metadata_targets: Vec<String>,
    /// Additional comparison and development-lock notes.
    pub notices: Vec<String>,
}

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Receipt {
    version: u32,
    base: GitRevision,
    local_branch: String,
    edits: Vec<FileEdit>,
    output: PublishOutput,
}

/// Prepares and validates one same-repository release batch before publishing.
pub fn publish(
    root: &Utf8Path,
    options: &PublishOptions,
    project_options: &ProjectOptions,
) -> Result<PublishOutput, DriverError> {
    if project_options.locked {
        return Err(fail("publication validates fresh candidate graphs and cannot use --locked"));
    }
    if project_options.offline && !options.plan {
        return Err(fail("publication pushes Git refs and cannot run offline"));
    }
    let repository =
        Utf8PathBuf::from(git(root, &["rev-parse", "--show-toplevel"], None, None)?.trim())
            .canonicalize_utf8()
            .map_err(|error| fail(error.to_string()))?;
    let _guard = lock_file(&repository.join(".fai/publish.lock"))?;
    let journal = repository.join(".fai/publish.json");
    if options.resume {
        if options.plan || !options.intents.is_empty() {
            return Err(fail("--resume cannot also choose new version intents or --plan"));
        }
        let receipt: Receipt = serde_json::from_slice(
            &std::fs::read(&journal).map_err(|error| fail(error.to_string()))?,
        )
        .map_err(|error| fail(error.to_string()))?;
        if receipt.version != 1 {
            return Err(fail("unsupported publication receipt"));
        }
        if options.remote.as_ref().is_some_and(|remote| remote != &receipt.output.remote)
            || options.branch.as_ref().is_some_and(|branch| branch != &receipt.output.branch)
        {
            return Err(code_error(
                fai_package::PUBLISH_PENDING,
                "resume retains the prepared release destination",
            ));
        }
        return finish(&repository, receipt, &journal);
    }
    if journal.exists() {
        return Err(code_error(
            fai_package::PUBLISH_PENDING,
            "a prepared release needs --resume before another version intent",
        ));
    }
    if options.intents.is_empty() {
        return Err(fail("publish requires explicit version or bump intent"));
    }
    let project = prepare_request(
        root,
        &[],
        &ProjectOptions { no_local: true, ..project_options.clone() },
        None,
        PrepareMode {
            selection_only: true,
            metadata_only: true,
            write: false,
            targets: options.targets.clone(),
            ..Default::default()
        },
    )?
    .ok_or_else(|| fail("publish requires a versioned package or workspace"))?;
    let selected = select_intents(&project.roots, &options.intents)?;
    let base = revision(&repository, "HEAD")?;
    let local_branch =
        git(&repository, &["symbolic-ref", "--quiet", "--short", "HEAD"], None, None)?
            .trim()
            .to_owned();
    let remotes = git(&repository, &["remote"], None, None)?;
    let names: Vec<_> = remotes.lines().collect();
    let remote = options
        .remote
        .clone()
        .or_else(|| {
            optional_git(
                &repository,
                &["config", "--get", &format!("branch.{local_branch}.remote")],
            )
            .filter(|name| name != ".")
        })
        .or_else(|| (names.len() == 1).then(|| names[0].to_owned()))
        .ok_or_else(|| fail("select an unambiguous publication remote with --remote"))?;
    if !names.contains(&remote.as_str()) {
        return Err(fail("publication remote is not configured in this repository"));
    }
    let destination = options
        .branch
        .clone()
        .or_else(|| {
            optional_git(&repository, &["config", "--get", &format!("branch.{local_branch}.merge")])
                .map(|branch| branch.trim_start_matches("refs/heads/").to_owned())
        })
        .unwrap_or_else(|| local_branch.clone());
    git(&repository, &["check-ref-format", &format!("refs/heads/{destination}")], None, None)?;
    let remote_url =
        git(&repository, &["config", "--get", &format!("remote.{remote}.url")], None, None)?
            .trim()
            .to_owned();
    let logical_repository = match GitRepository::parse(&remote_url) {
        Ok(repository) => repository,
        Err(_) => {
            let path = repository.join(&remote_url);
            let url =
                url::Url::from_file_path(path).map_err(|_| fail("invalid publication remote"))?;
            GitRepository::parse(url.as_str()).map_err(|error| fail(error.to_string()))?
        }
    };

    let mut packages = Vec::new();
    let mut edits = Vec::new();
    for (name, directory, intent) in selected {
        let relative = directory
            .strip_prefix(&repository)
            .map_err(|_| fail("all published packages must belong to one Git repository"))?;
        let path = package_path(relative)?;
        let manifest_path = directory.join("fai.toml");
        let text =
            std::fs::read_to_string(&manifest_path).map_err(|error| fail(error.to_string()))?;
        let document = parse_manifest(&manifest_path, &text)?;
        let metadata = document
            .manifest()
            .package
            .as_ref()
            .ok_or_else(|| fail("selected member is not a package"))?;
        if !metadata.explicit_fai_version {
            return Err(fail(format!(
                "{name} must declare package.fai-version before publication"
            )));
        }
        for dependency in document
            .manifest()
            .dependencies
            .values()
            .chain(document.manifest().dev_dependencies.values())
            .chain(document.manifest().platforms.iter().flat_map(|platform| {
                platform.dependencies.values().chain(platform.dev_dependencies.values())
            }))
        {
            match dependency {
                Dependency::Path { .. } => {
                    return Err(fail(format!(
                        "{name} contains a nonportable path dependency; publish portable Git declarations"
                    )));
                }
                Dependency::Git { package, .. }
                    if package.repository.is_local() && !logical_repository.is_local() =>
                {
                    return Err(fail(format!("{name} contains a local-only Git dependency")));
                }
                _ => {}
            }
        }
        let version = next_version(&metadata.version, &intent)?;
        let identity = GitPackage { repository: logical_repository.clone(), subdir: path.clone() };
        let tag = identity.release_tag(&version).map_err(|error| fail(error.to_string()))?;
        let candidate = document
            .with_package_version(&version)
            .map_err(|_| fail("cannot prepare package version edit"))?;
        let manifest = if path.is_root() { "fai.toml".into() } else { format!("{path}/fai.toml") };
        edits.push(FileEdit::new(manifest, Some(&text), candidate.text().into()));
        packages.push(PublishedPackage {
            name,
            path,
            before: metadata.version.clone(),
            version,
            tag,
        });
    }
    packages.sort_by(|a, b| a.path.cmp(&b.path));
    check_clean(&repository, &packages)?;
    let scratch = tempfile::tempdir().map_err(|error| fail(error.to_string()))?;
    let scratch = Utf8Path::from_path(scratch.path())
        .ok_or_else(|| fail("temporary directory is not UTF-8"))?;
    let index = scratch.join("candidate.index");
    git(&repository, &["read-tree", base.as_str()], Some(&index), None)?;
    for edit in &edits {
        let object = git(
            &repository,
            &["hash-object", "--no-filters", "-w", "--stdin"],
            None,
            Some(edit.after.as_bytes()),
        )?;
        let tree_entry =
            git(&repository, &["ls-tree", base.as_str(), "--", &edit.path], None, None)?;
        let mode = tree_entry
            .split_whitespace()
            .next()
            .ok_or_else(|| fail("published manifest must already be committed"))?;
        git(
            &repository,
            &[
                "update-index",
                "--add",
                "--cacheinfo",
                &format!("{mode},{},{}", object.trim(), edit.path),
            ],
            Some(&index),
            None,
        )?;
    }
    let tree = GitRevision::parse(git(&repository, &["write-tree"], Some(&index), None)?.trim())
        .map_err(|error| fail(error.to_string()))?;
    let base_tree = revision(&repository, &format!("{base}^{{tree}}"))?;
    let message = format!(
        "Release {}",
        packages
            .iter()
            .map(|package| format!("{} {}", package.name, package.version))
            .collect::<Vec<_>>()
            .join(", ")
    );
    let commit = if tree == base_tree {
        base.clone()
    } else {
        GitRevision::parse(
            git(
                &repository,
                &["commit-tree", tree.as_str(), "-p", base.as_str(), "-m", &message],
                None,
                None,
            )?
            .trim(),
        )
        .map_err(|error| fail(error.to_string()))?
    };
    for package in &packages {
        if optional_revision(&repository, &format!("refs/tags/{}^{{commit}}", package.tag))
            .is_some_and(|existing| existing != commit)
        {
            return Err(code_error(
                fai_package::PUBLISH_CONFLICT,
                format!("release tag {} already identifies another commit", package.tag),
            ));
        }
    }

    let candidate_repo = scratch.join("candidate.git");
    // `-C` already selects the repository. A relative source keeps Windows
    // canonical path prefixes out of Git's clone source parser.
    git(
        &repository,
        &["clone", "--bare", "--shared", "--no-hardlinks", "--", ".", candidate_repo.as_str()],
        None,
        None,
    )?;
    if !project_options.offline {
        git(
            &candidate_repo,
            &["remote", "add", "upstream", logical_repository.as_str()],
            None,
            None,
        )?;
        git(
            &candidate_repo,
            &[
                "fetch",
                "--tags",
                "--no-recurse-submodules",
                "upstream",
                "+refs/heads/*:refs/remotes/upstream/*",
            ],
            None,
            None,
        )?;
    }
    git(
        &candidate_repo,
        &["update-ref", "refs/heads/fai-release-candidate", commit.as_str()],
        None,
        None,
    )?;
    for package in &packages {
        let reference = format!("refs/tags/{}", package.tag);
        if optional_revision(&candidate_repo, &format!("{reference}^{{commit}}"))
            .is_some_and(|existing| existing != commit)
        {
            return Err(code_error(
                fai_package::PUBLISH_CONFLICT,
                format!("remote release tag {} already exists", package.tag),
            ));
        }
        git(&candidate_repo, &["update-ref", &reference, commit.as_str()], None, None)?;
    }
    let transport = GitRepository::parse(
        url::Url::from_directory_path(&candidate_repo)
            .map_err(|_| fail("invalid candidate repository path"))?
            .as_str(),
    )
    .map_err(|error| fail(error.to_string()))?;
    let validation_cache = scratch.join("validation-cache");
    let transports = BTreeMap::from([(logical_repository.clone(), transport.clone())]);
    let mut snapshots = GitStore::new(validation_cache.join("packages"), false)
        .with_transport(logical_repository.clone(), transport);
    let whole = GitPackage { repository: logical_repository.clone(), subdir: PackagePath::root() };
    snapshots.inventory(&whole)?;
    let candidate_tree = snapshots.materialize(&whole, &commit, None)?;
    let host = TargetPlatform::parse(&target_lexicon::HOST.to_string())
        .map_err(|error| fail(error.to_string()))?;
    let mut api_changes = Vec::new();
    let mut api_complete = true;
    let mut notices = vec!["development lockfiles retain their existing records; prepare affected graphs after publication".into()];
    for package in &packages {
        let directory = if package.path.is_root() {
            candidate_tree.directory.clone()
        } else {
            candidate_tree.directory.join(package.path.as_str())
        };
        let identity =
            GitPackage { repository: logical_repository.clone(), subdir: package.path.clone() };
        let consumer = scratch.join(format!("consumer-{}", package.name));
        std::fs::create_dir_all(&consumer).map_err(|error| fail(error.to_string()))?;
        let manifest = format!(
            "manifest-version = 1\n[package]\nname = 'release-consumer'\nversion = '0.1.0'\n[dependencies]\ncandidate = {{git = {}, subdir = {}, version = {}}}\n",
            toml_string(logical_repository.as_str()),
            toml_string(package.path.as_str()),
            toml_string(&package.version.to_string())
        );
        std::fs::write(consumer.join("fai.toml"), manifest)
            .map_err(|error| fail(error.to_string()))?;
        let mode = PrepareMode {
            policy: PreparePolicy::Update(Vec::new()),
            release_validation: true,
            write: false,
            transports: transports.clone(),
            cache_dir: Some(validation_cache.clone()),
            targets: vec![host.clone()],
            ..Default::default()
        };
        let prepared = prepare_request(
            &consumer,
            &[],
            &ProjectOptions {
                no_local: true,
                offline: project_options.offline,
                no_vendor: true,
                locked: false,
            },
            None,
            mode.clone(),
        )?
        .ok_or_else(|| fail("missing release consumer graph"))?;
        let session = Session::from_project(consumer, prepared)?;
        validate_code(&session, &session.analysis_files(None), false, &package.name)?;
        let mut development = mode.clone();
        development.root_alias = Some(identity.clone());
        let prepared = prepare_request(
            &directory,
            &[],
            &ProjectOptions {
                no_local: true,
                offline: project_options.offline,
                no_vendor: true,
                locked: false,
            },
            None,
            development.clone(),
        )?
        .ok_or_else(|| fail("missing release development graph"))?;
        let session = Session::from_project(directory.clone(), prepared)?;
        validate_code(&session, &session.user_files(), true, &package.name)?;
        for target in &project.targets {
            if target == &host {
                continue;
            }
            let document = read_manifest(&directory.join("fai.toml"))?;
            if document.manifest().package.as_ref().is_some_and(|package| {
                !package.supported_platforms.is_empty()
                    && !package
                        .supported_platforms
                        .iter()
                        .any(|predicate| predicate.matches(target))
            }) {
                continue;
            }
            development.metadata_only = true;
            development.targets = vec![target.clone()];
            prepare_request(
                &directory,
                &[],
                &ProjectOptions {
                    no_local: true,
                    offline: project_options.offline,
                    no_vendor: true,
                    locked: false,
                },
                None,
                development.clone(),
            )?;
        }
        let (after, complete) = api(&directory, &host)?;
        api_complete &= complete;
        let baseline = prior_release(&repository, &identity, package, &base)?;
        let before = if let Some(baseline) = baseline {
            match snapshots.materialize(&identity, &baseline, None).and_then(|tree| {
                api(&tree.directory, &host).map_err(|error| fai_package::ProviderFailure {
                    code: fai_package::PUBLISH_VALIDATION,
                    message: error.to_string(),
                })
            }) {
                Ok((api, complete)) => {
                    api_complete &= complete;
                    api
                }
                Err(error) => {
                    notices.push(format!("{} API baseline is incomplete: {error}", package.name));
                    api_complete = false;
                    BTreeMap::new()
                }
            }
        } else {
            BTreeMap::new()
        };
        for name in before.keys().chain(after.keys()).cloned().collect::<BTreeSet<_>>() {
            if before.get(&name) != after.get(&name) {
                api_changes.push(ApiChange {
                    name: format!("{}:{name}", package.name),
                    kind: if !before.contains_key(&name) {
                        "added"
                    } else if !after.contains_key(&name) {
                        "removed"
                    } else {
                        "changed"
                    }
                    .into(),
                });
            }
        }
    }
    let output = PublishOutput {
        schema_version: 1,
        ok: true,
        status: if options.plan { "planned" } else { "prepared" }.into(),
        commit,
        tree,
        remote,
        branch: destination,
        packages,
        api_changes,
        api_complete,
        validated_host: host.to_string(),
        metadata_targets: project
            .targets
            .iter()
            .filter(|target| *target != &host)
            .map(ToString::to_string)
            .collect(),
        notices,
    };
    if options.plan {
        return Ok(output);
    }
    check_clean(&repository, &output.packages)?;
    if revision(&repository, "HEAD")? != base {
        return Err(code_error(
            fai_package::INPUTS_CHANGED,
            "HEAD changed during release validation",
        ));
    }
    let receipt = Receipt { version: 1, base, local_branch, edits, output };
    atomic_write(
        &journal,
        &serde_json::to_vec_pretty(&receipt).map_err(|error| fail(error.to_string()))?,
    )?;
    finish(&repository, receipt, &journal)
}

fn finish(
    root: &Utf8Path,
    mut receipt: Receipt,
    journal: &Utf8Path,
) -> Result<PublishOutput, DriverError> {
    if git(root, &["symbolic-ref", "--quiet", "--short", "HEAD"], None, None)?.trim()
        != receipt.local_branch
    {
        return Err(code_error(
            fai_package::PUBLISH_PENDING,
            "return to the prepared release branch before resuming",
        ));
    }
    let head = revision(root, "HEAD")?;
    if head != receipt.base && head != receipt.output.commit {
        return Err(code_error(
            fai_package::PUBLISH_PENDING,
            "HEAD diverged from the prepared release; preserve the receipt and replan explicitly",
        ));
    }
    if head == receipt.base && head != receipt.output.commit {
        let edits: Vec<_> = receipt
            .edits
            .iter()
            .filter_map(|edit| {
                let current = std::fs::read(root.join(&edit.path))
                    .ok()
                    .map(|bytes| FileEdit::fingerprint(&bytes));
                (current != Some(FileEdit::fingerprint(edit.after.as_bytes())))
                    .then_some(edit.clone())
            })
            .collect();
        MetadataTransaction::new(edits, Vec::new()).commit(root)?;
        let scratch = tempfile::tempdir().map_err(|error| fail(error.to_string()))?;
        let index = Utf8Path::from_path(scratch.path())
            .ok_or_else(|| fail("temporary path is not UTF-8"))?
            .join("index");
        git(root, &["read-tree", receipt.output.tree.as_str()], Some(&index), None)?;
        install_index(root, &index, &receipt.base, &receipt.output.tree)?;
        git(root, &["diff", "--exit-code"], None, None)?;
        git(
            root,
            &[
                "update-ref",
                "-m",
                "publish validated package release",
                &format!("refs/heads/{}", receipt.local_branch),
                receipt.output.commit.as_str(),
                receipt.base.as_str(),
            ],
            None,
            None,
        )?;
    }
    if revision(root, "HEAD")? != receipt.output.commit
        || revision(root, "HEAD^{tree}")? != receipt.output.tree
    {
        return Err(code_error(
            fai_package::PUBLISH_PENDING,
            "prepared release identity no longer matches the checkout",
        ));
    }
    git(root, &["diff", "--exit-code"], None, None)?;
    git(root, &["diff", "--cached", "--exit-code"], None, None)?;
    for package in &receipt.output.packages {
        let reference = format!("refs/tags/{}", package.tag);
        if let Some(existing) = optional_revision(root, &format!("{reference}^{{commit}}")) {
            if existing != receipt.output.commit {
                return Err(code_error(
                    fai_package::PUBLISH_CONFLICT,
                    format!("tag {} identifies another commit", package.tag),
                ));
            }
        } else {
            let zero = "0".repeat(receipt.output.commit.as_str().len());
            git(
                root,
                &["update-ref", &reference, receipt.output.commit.as_str(), &zero],
                None,
                None,
            )?;
        }
    }
    receipt.output.status = "prepared".into();
    atomic_write(
        journal,
        &serde_json::to_vec_pretty(&receipt).map_err(|error| fail(error.to_string()))?,
    )?;
    let mut args = vec![
        "push".to_owned(),
        "--atomic".into(),
        receipt.output.remote.clone(),
        format!("refs/heads/{}:refs/heads/{}", receipt.local_branch, receipt.output.branch),
    ];
    args.extend(
        receipt
            .output
            .packages
            .iter()
            .map(|package| format!("refs/tags/{}:refs/tags/{}", package.tag, package.tag)),
    );
    let result = git(root, &args.iter().map(String::as_str).collect::<Vec<_>>(), None, None);
    if result.is_err() && !remote_matches(root, &receipt.output)? {
        return Err(code_error(
            fai_package::PUBLISH_CONFLICT,
            "atomic publication failed; the prepared commit and tags are retained for publish --resume",
        ));
    }
    receipt.output.status = "published".into();
    atomic_write(
        &root.join(format!(".fai/releases/{}.json", receipt.output.commit)),
        &serde_json::to_vec_pretty(&receipt).map_err(|error| fail(error.to_string()))?,
    )?;
    std::fs::remove_file(journal).map_err(|error| fail(error.to_string()))?;
    Ok(receipt.output)
}

fn install_index(
    root: &Utf8Path,
    candidate: &Utf8Path,
    base: &GitRevision,
    tree: &GitRevision,
) -> Result<(), DriverError> {
    let path = git(root, &["rev-parse", "--git-path", "index"], None, None)?;
    let path = root.join(path.trim());
    let before = std::fs::read(&path).map_err(|error| fail(error.to_string()))?;
    let before_index = candidate.with_extension("before");
    std::fs::write(&before_index, &before).map_err(|error| fail(error.to_string()))?;
    let current = git(root, &["write-tree"], Some(&before_index), None)?;
    let base_tree = revision(root, &format!("{base}^{{tree}}"))?;
    if current.trim() != base_tree.as_str() && current.trim() != tree.as_str() {
        return Err(code_error(
            fai_package::INPUTS_CHANGED,
            "Git index changed during publication",
        ));
    }
    let lock_path = Utf8PathBuf::from(format!("{path}.lock"));
    let mut lock = std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(&lock_path)
        .map_err(|error| fail(format!("cannot lock Git index: {error}")))?;
    let result = (|| {
        if std::fs::read(&path).map_err(|error| fail(error.to_string()))? != before {
            return Err(code_error(
                fai_package::INPUTS_CHANGED,
                "Git index changed before publication",
            ));
        }
        lock.write_all(&std::fs::read(candidate).map_err(|error| fail(error.to_string()))?)
            .map_err(|error| fail(error.to_string()))?;
        lock.sync_all().map_err(|error| fail(error.to_string()))?;
        drop(lock);
        std::fs::rename(&lock_path, &path).map_err(|error| fail(error.to_string()))
    })();
    if result.is_err() {
        let _ = std::fs::remove_file(lock_path);
    }
    result
}

fn remote_matches(root: &Utf8Path, output: &PublishOutput) -> Result<bool, DriverError> {
    let text = git(root, &["ls-remote", &output.remote], None, None)?;
    let refs: BTreeMap<_, _> = text
        .lines()
        .filter_map(|line| line.split_once('\t').map(|(revision, name)| (name, revision)))
        .collect();
    Ok(refs.get(format!("refs/heads/{}", output.branch).as_str()) == Some(&output.commit.as_str())
        && output.packages.iter().all(|package| {
            refs.get(format!("refs/tags/{}^{{}}", package.tag).as_str())
                .or_else(|| refs.get(format!("refs/tags/{}", package.tag).as_str()))
                == Some(&output.commit.as_str())
        }))
}

fn validate_code(
    session: &Session,
    files: &[fai_db::SourceFile],
    contracts: bool,
    name: &str,
) -> Result<(), DriverError> {
    let checked = crate::build_library(session.db(), files);
    if !checked.ok {
        return Err(validation_error(session, &checked.diagnostics, name));
    }
    if contracts {
        for &file in files {
            if matches!(file.role(session.db()), fai_db::SourceRole::Executable(_)) {
                let bundle = crate::build_run_bundle_with_deps(
                    session.db(),
                    file,
                    &session.native_dependencies(file)?,
                );
                if bundle.bundle.is_none() {
                    return Err(validation_error(session, &bundle.diagnostics, name));
                }
            }
        }
        let mut plan = crate::build_test_plan_with_options(
            session.db(),
            files,
            None,
            crate::TestConfig::default(),
            crate::TestOptions { list: false, deny_focused: true },
        );
        crate::configure_test_plan(&mut plan, session);
        let execution = crate::execute_tests(&plan, &mut |_| {});
        let outcome = crate::assemble_execution(&plan, &execution);
        if !outcome.ok {
            return Err(validation_error(session, &outcome.diagnostics, name));
        }
    }
    Ok(())
}

fn validation_error(
    session: &Session,
    diagnostics: &[fai_diagnostics::Diagnostic],
    name: &str,
) -> DriverError {
    let mut all = vec![fai_diagnostics::Diagnostic::error(
        fai_package::PUBLISH_VALIDATION,
        format!("release candidate {name} failed validation"),
        crate::tooling_span(),
    )];
    all.extend_from_slice(diagnostics);
    DriverError::Project(Box::new(crate::ProjectError {
        human: format!(
            "release candidate {name} failed validation\n{}",
            fai_diagnostics::render_human(diagnostics, &session.resolver(), false)
        ),
        diagnostics: fai_diagnostics::wire::to_wire(&all, &session.resolver()),
    }))
}

fn select_intents(
    roots: &[(String, Utf8PathBuf)],
    intents: &[String],
) -> Result<Vec<(String, Utf8PathBuf, String)>, DriverError> {
    let mut selected = BTreeMap::new();
    for intent in intents {
        let (root, intent) = if let Some((selector, intent)) = intent.rsplit_once('=') {
            let matches: Vec<_> = roots
                .iter()
                .filter(|(name, path)| name == selector || path.ends_with(selector))
                .collect();
            if matches.len() != 1 {
                return Err(fail(format!(
                    "release selector {selector} must identify exactly one package"
                )));
            }
            (matches[0], intent)
        } else {
            if roots.len() != 1 || intents.len() != 1 {
                return Err(fail("a release batch requires package=intent entries"));
            }
            (&roots[0], intent.as_str())
        };
        if selected
            .insert(root.1.clone(), (root.0.clone(), root.1.clone(), intent.to_owned()))
            .is_some()
        {
            return Err(fail("a package was selected more than once"));
        }
    }
    Ok(selected.into_values().collect())
}

fn next_version(current: &PackageVersion, intent: &str) -> Result<PackageVersion, DriverError> {
    if !matches!(intent, "patch" | "minor" | "major") {
        return PackageVersion::parse(intent).map_err(|error| fail(error.to_string()));
    }
    let current = current.as_semver();
    if !current.pre.is_empty() {
        return Err(fail("prerelease transitions require an exact version"));
    }
    let (major, minor, patch) = match intent {
        "major" => (current.major.checked_add(1), Some(0), Some(0)),
        "minor" => (Some(current.major), current.minor.checked_add(1), Some(0)),
        _ => (Some(current.major), Some(current.minor), current.patch.checked_add(1)),
    };
    let (Some(major), Some(minor), Some(patch)) = (major, minor, patch) else {
        return Err(fail("package version overflow"));
    };
    PackageVersion::parse(&format!("{major}.{minor}.{patch}"))
        .map_err(|error| fail(error.to_string()))
}

fn check_clean(root: &Utf8Path, packages: &[PublishedPackage]) -> Result<(), DriverError> {
    let status =
        git(root, &["status", "--porcelain=v1", "-z", "--untracked-files=all"], None, None)?;
    for entry in status.split('\0').filter(|entry| !entry.is_empty()) {
        if !entry.starts_with("?? ") {
            return Err(fail("publication requires a clean committed worktree and index"));
        }
        let path = &entry[3..];
        if path.starts_with(".fai/") {
            continue;
        }
        let relevant = packages.iter().any(|package| {
            package.path.is_root() || path.starts_with(&format!("{}/", package.path))
        });
        if relevant && (path.ends_with(".fai") || path.ends_with("/fai.toml") || path == "fai.toml")
        {
            return Err(fail(format!("untracked release source {path} must be committed first")));
        }
    }
    Ok(())
}

fn prior_release(
    root: &Utf8Path,
    identity: &GitPackage,
    package: &PublishedPackage,
    base: &GitRevision,
) -> Result<Option<GitRevision>, DriverError> {
    let tags = git(root, &["tag", "--merged", base.as_str()], None, None)?;
    let selected = tags
        .lines()
        .filter_map(|tag| identity.version_from_tag(tag).map(|version| (version, tag)))
        .filter(|(version, _)| version <= &package.before && version <= &package.version)
        .max_by(|a, b| a.0.cmp(&b.0));
    selected.map(|(_, tag)| revision(root, &format!("refs/tags/{tag}^{{commit}}"))).transpose()
}

fn api(
    root: &Utf8Path,
    target: &TargetPlatform,
) -> Result<(BTreeMap<String, String>, bool), DriverError> {
    let document = read_manifest(&root.join("fai.toml"))?;
    let mut inventory = Vec::new();
    let mut pending = vec![root.to_owned()];
    while let Some(directory) = pending.pop() {
        for entry in std::fs::read_dir(&directory).map_err(|error| fail(error.to_string()))? {
            let entry = entry.map_err(|error| fail(error.to_string()))?;
            let path = Utf8PathBuf::from_path_buf(entry.path())
                .map_err(|_| fail("API path is not UTF-8"))?;
            if entry.file_type().map_err(|error| fail(error.to_string()))?.is_dir() {
                pending.push(path);
            } else {
                inventory.push(package_path(
                    path.strip_prefix(root).map_err(|error| fail(error.to_string()))?,
                )?);
            }
        }
    }
    let plan = fai_package::SourcePlan::build_library(&document, target, &inventory)
        .map_err(|_| fail("cannot inspect API source layout"))?;
    let mut result = BTreeMap::new();
    let mut complete = true;
    for path in plan.library {
        let text = std::fs::read_to_string(root.join(path.as_str()))
            .map_err(|error| fail(error.to_string()))?;
        let parsed = fai_syntax::parse_module(fai_span::SourceId::new(0), &text);
        if !parsed.diagnostics.is_empty() {
            complete = false;
            continue;
        }
        let module = &parsed.module;
        let name = module.name.map(|name| name.to_string()).unwrap_or_else(|| path.to_string());
        let mut items: Vec<_> = module.roots.iter().map(|id| (name.clone(), *id)).collect();
        while let Some((scope, id)) = items.pop() {
            use fai_syntax::ast::{ItemKind, Visibility};
            let item = &module.items[id.index()];
            let item_name = match &item.kind {
                ItemKind::Module { name, body } => {
                    items.extend(body.iter().map(|id| (format!("{scope}.{name}"), *id)));
                    continue;
                }
                ItemKind::Signature { visibility: Visibility::Public, name, .. }
                | ItemKind::Interface { visibility: Visibility::Public, name, .. } => Some(*name),
                ItemKind::Type {
                    visibility: Visibility::Public,
                    name,
                    opaque: true,
                    params,
                    ..
                } => {
                    result.insert(
                        format!("{scope}.{name}"),
                        format!("opaque {name} {}", params.len()),
                    );
                    continue;
                }
                ItemKind::Type { visibility: Visibility::Public, name, .. } => Some(*name),
                _ => None,
            };
            if let Some(name) = item_name {
                let source = &text[item.span.start().to_usize()..item.span.end().to_usize()];
                let lexed = fai_syntax::lex(fai_span::SourceId::new(0), source);
                let tokens = lexed
                    .tokens
                    .iter()
                    .filter(|token| token.kind != fai_syntax::TokenKind::Eof)
                    .map(|token| {
                        &source[token.range.start().to_usize()..token.range.end().to_usize()]
                    })
                    .collect::<Vec<_>>()
                    .join(" ");
                result.insert(format!("{scope}.{name}"), tokens);
            }
        }
        for import in &module.imports {
            if import.visibility == fai_syntax::ast::Visibility::Public {
                complete = false;
                if let Some(alias) = import.binding() {
                    result.insert(
                        format!("{name}.{alias}"),
                        import.path.iter().map(ToString::to_string).collect::<Vec<_>>().join("."),
                    );
                }
            }
        }
    }
    Ok((result, complete))
}

fn read_manifest(path: &Utf8Path) -> Result<ManifestDocument, DriverError> {
    parse_manifest(path, &std::fs::read_to_string(path).map_err(|error| fail(error.to_string()))?)
}
fn parse_manifest(path: &Utf8Path, text: &str) -> Result<ManifestDocument, DriverError> {
    ManifestDocument::parse(fai_span::SourceId::new(0), text).map_err(|diagnostics| {
        let mut sources = fai_span::SourceMap::new();
        sources.add(path.to_owned(), text.into());
        DriverError::Project(Box::new(crate::ProjectError {
            human: fai_diagnostics::render_human(&diagnostics, &sources, false),
            diagnostics: fai_diagnostics::wire::to_wire(&diagnostics, &sources),
        }))
    })
}
fn package_path(path: &Utf8Path) -> Result<PackagePath, DriverError> {
    let path = path.components().map(|part| part.as_str()).collect::<Vec<_>>().join("/");
    PackagePath::parse(if path.is_empty() { "." } else { &path })
        .map_err(|error| fail(error.to_string()))
}
fn toml_string(value: &str) -> String {
    toml::Value::String(value.into()).to_string()
}
fn revision(root: &Utf8Path, name: &str) -> Result<GitRevision, DriverError> {
    GitRevision::parse(git(root, &["rev-parse", "--verify", name], None, None)?.trim())
        .map_err(|error| fail(error.to_string()))
}
fn optional_revision(root: &Utf8Path, name: &str) -> Option<GitRevision> {
    optional_git(root, &["rev-parse", "--verify", "--quiet", name])
        .and_then(|text| GitRevision::parse(&text).ok())
}
fn optional_git(root: &Utf8Path, args: &[&str]) -> Option<String> {
    git(root, args, None, None).ok().map(|text| text.trim().to_owned())
}
fn git(
    root: &Utf8Path,
    args: &[&str],
    index: Option<&Utf8Path>,
    input: Option<&[u8]>,
) -> Result<String, DriverError> {
    let mut command = Command::new("git");
    command
        .arg("-C")
        .arg(root)
        .args(args)
        .env_remove("GIT_DIR")
        .env_remove("GIT_WORK_TREE")
        .env_remove("GIT_INDEX_FILE")
        .env("GIT_NO_REPLACE_OBJECTS", "1")
        .env("GIT_TERMINAL_PROMPT", "0")
        .stdin(if input.is_some() { Stdio::piped() } else { Stdio::null() })
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    if let Some(index) = index {
        command.env("GIT_INDEX_FILE", index);
    }
    let mut child = command.spawn().map_err(|error| fail(error.to_string()))?;
    if let Some(input) = input {
        child
            .stdin
            .take()
            .ok_or_else(|| fail("Git input pipe unavailable"))?
            .write_all(input)
            .map_err(|error| fail(error.to_string()))?;
    }
    let output = child.wait_with_output().map_err(|error| fail(error.to_string()))?;
    if !output.status.success() {
        return Err(fail(format!(
            "Git {} failed; check repository state, access, and destination rules",
            args.first().copied().unwrap_or("operation")
        )));
    }
    String::from_utf8(output.stdout).map_err(|error| fail(error.to_string()))
}
fn code_error(code: fai_diagnostics::DiagnosticCode, message: impl Into<String>) -> DriverError {
    fai_package::ProviderFailure { code, message: message.into() }.into()
}
fn fail(message: impl Into<String>) -> DriverError {
    code_error(fai_package::INVALID_PUBLICATION, message)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn minor_bump_resets_patch() {
        assert_eq!(
            next_version(&PackageVersion::parse("1.2.3").unwrap(), "minor").unwrap().to_string(),
            "1.3.0"
        );
    }

    #[test]
    fn overflowing_patch_is_a_diagnostic_error() {
        assert!(
            next_version(&PackageVersion::parse("1.2.18446744073709551615").unwrap(), "patch")
                .is_err()
        );
    }

    #[test]
    fn prerelease_bumps_require_exact_intent() {
        assert!(next_version(&PackageVersion::parse("1.0.0-rc.1").unwrap(), "patch").is_err());
    }
}

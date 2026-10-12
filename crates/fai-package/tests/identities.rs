use fai_package::{
    DependencyAlias, GitBranch, GitPackage, GitRepository, GitRevision, PackageName, PackagePath,
    PackageVersion, PlatformPredicate, TargetPlatform,
};

#[test]
fn github_https_and_ssh_coordinates_are_equivalent() {
    let https = GitRepository::parse("https://github.com/Example/Library").unwrap();
    assert_eq!(https.as_str(), "https://github.com/example/library.git");
    assert_eq!(https, GitRepository::parse("git@github.com:example/library.git").unwrap());
    assert_eq!(https, GitRepository::parse("ssh://git@github.com:22/Example/Library.git").unwrap());
}

#[test]
fn github_cli_shorthand_expands() {
    assert_eq!(
        GitRepository::from_cli("github:Example/Library").unwrap().as_str(),
        "https://github.com/example/library.git"
    );
}

#[test]
fn generic_ssh_relative_paths_are_not_absolute_urls() {
    assert_ne!(
        GitRepository::parse("alice@git.example:projects/lib.git").unwrap(),
        GitRepository::parse("ssh://alice@git.example/projects/lib.git").unwrap()
    );
}

#[test]
fn generic_ssh_users_are_identity_components() {
    assert_ne!(
        GitRepository::parse("alice@git.example:repo.git").unwrap(),
        GitRepository::parse("bob@git.example:repo.git").unwrap()
    );
}

#[test]
fn generic_repository_path_case_is_preserved() {
    assert_ne!(
        GitRepository::parse("https://git.example/Project.git").unwrap(),
        GitRepository::parse("https://git.example/project.git").unwrap()
    );
}

#[test]
fn url_credentials_are_rejected_without_echoing_the_secret() {
    let error = GitRepository::parse("https://user:supersecret@example.com/repo.git").unwrap_err();
    assert!(!error.to_string().contains("supersecret"));
}

#[test]
fn authentication_queries_are_rejected() {
    assert!(GitRepository::parse("https://example.com/repo?token=secret").is_err());
}

#[test]
fn external_helper_locators_are_rejected() {
    assert!(GitRepository::parse("ext::malicious").is_err());
}

#[test]
fn windows_drive_paths_are_not_ssh_repositories() {
    assert!(GitRepository::parse("C:/work/repo").is_err());
}

#[test]
fn local_git_repositories_are_identified() {
    assert!(GitRepository::parse("file:///work/repo.git").unwrap().is_local());
}

#[test]
fn package_paths_normalize_components() {
    assert_eq!(PackagePath::parse("./packages//old/../json/").unwrap().as_str(), "packages/json");
}

#[test]
fn package_path_escape_is_rejected() {
    assert!(PackagePath::parse("src/../../other").is_err());
}

#[test]
fn package_paths_reject_absolute_paths_on_every_host() {
    assert!(PackagePath::parse("/src/Main.fai").is_err());
    assert!(PackagePath::parse("C:/src/Main.fai").is_err());
}

#[test]
fn git_metadata_paths_are_rejected() {
    assert!(PackagePath::parse("src/.Git/config").is_err());
}

#[test]
fn path_containment_uses_component_boundaries() {
    let root = PackagePath::parse("src").unwrap();
    assert!(PackagePath::parse("src/Main.fai").unwrap().is_within(&root));
    assert!(!PackagePath::parse("src-other/Main.fai").unwrap().is_within(&root));
}

#[test]
fn package_display_names_allow_hyphens_but_import_aliases_do_not() {
    assert!(PackageName::parse("http-client").is_ok());
    assert!(DependencyAlias::parse("http-client").is_err());
    assert!(DependencyAlias::parse("http_client").is_ok());
}

#[test]
fn keywords_cannot_be_dependency_aliases() {
    assert!(DependencyAlias::parse("module").is_err());
    assert!(DependencyAlias::parse("import").is_err());
}

#[test]
fn sha256_revisions_are_supported_and_normalized() {
    let revision = GitRevision::parse(&"A".repeat(64)).unwrap();
    assert_eq!(revision.as_str(), "a".repeat(64));
}

#[test]
fn abbreviated_revisions_are_rejected() {
    assert!(GitRevision::parse("abcdef12").is_err());
}

#[test]
fn slash_separated_branches_are_valid() {
    assert_eq!(GitBranch::parse("fix/parser").unwrap().as_str(), "fix/parser");
}

#[test]
fn branch_reflog_expressions_are_rejected() {
    assert!(GitBranch::parse("main@{1}").is_err());
}

#[test]
fn branch_ancestor_expressions_are_rejected() {
    assert!(GitBranch::parse("main~1").is_err());
}

#[test]
fn branch_lock_suffix_is_rejected() {
    assert!(GitBranch::parse("topic.lock").is_err());
}

#[test]
fn monorepo_tags_are_scoped_to_the_exact_package_path() {
    let package = GitPackage {
        repository: GitRepository::parse("https://github.com/example/libs").unwrap(),
        subdir: PackagePath::parse("packages/json").unwrap(),
    };
    let version = PackageVersion::parse("1.4.0").unwrap();
    assert_eq!(package.release_tag(&version).unwrap(), "packages/json/v1.4.0");
    assert_eq!(package.version_from_tag("packages/json/v1.4.0"), Some(version));
    assert_eq!(package.version_from_tag("packages/web/v1.4.0"), None);
    assert_eq!(package.version_from_tag("v1.4.0"), None);
}

#[test]
fn root_package_tags_use_the_short_prefix() {
    let package = GitPackage {
        repository: GitRepository::parse("https://example.com/lib.git").unwrap(),
        subdir: PackagePath::root(),
    };
    assert_eq!(
        package.release_tag(&PackageVersion::parse("1.0.0-rc.1").unwrap()).unwrap(),
        "v1.0.0-rc.1"
    );
}

#[test]
fn unpublishable_directory_names_fail_tag_construction() {
    let package = GitPackage {
        repository: GitRepository::parse("https://example.com/lib.git").unwrap(),
        subdir: PackagePath::parse("my package").unwrap(),
    };
    assert!(package.release_tag(&PackageVersion::parse("1.0.0").unwrap()).is_err());
}

#[test]
fn apple_targets_use_the_macos_selector() {
    let predicate = PlatformPredicate::new(Some("macos"), Some("aarch64"), Some("")).unwrap();
    assert!(predicate.matches(&TargetPlatform::parse("aarch64-apple-darwin").unwrap()));
}

#[test]
fn disjoint_os_predicates_do_not_overlap() {
    let linux = PlatformPredicate::new(Some("linux"), None, None).unwrap();
    let windows = PlatformPredicate::new(Some("windows"), None, None).unwrap();
    assert!(!linux.overlaps(&windows));
}

#[test]
fn independent_selector_axes_overlap() {
    let os = PlatformPredicate::new(Some("windows"), None, None).unwrap();
    let arch = PlatformPredicate::new(None, Some("x86_64"), None).unwrap();
    assert!(os.overlaps(&arch));
}

#[test]
fn empty_platform_predicates_are_rejected() {
    assert!(PlatformPredicate::new(None, None, None).is_err());
}

//! Graph preferences and compatibility laws against finite immutable catalogs.

use std::collections::{BTreeMap, BTreeSet};

use fai_package::*;

#[derive(Default, Clone)]
struct Catalog(BTreeMap<PackageKey, CandidateSet>);

impl ResolverProvider for Catalog {
    fn candidates(
        &mut self,
        key: &PackageKey,
        _: &[PackageSelector],
        _: Option<&PackagePin>,
        _: bool,
    ) -> Result<CandidateSet, ProviderFailure> {
        Ok(self.0.get(key).cloned().unwrap_or_default())
    }
}

fn key(name: &str) -> PackageKey {
    PackageKey::Git(GitPackage {
        repository: GitRepository::parse(&format!("https://example.com/{name}.git")).unwrap(),
        subdir: PackagePath::root(),
    })
}

fn requirement(name: &str, version: &str) -> PackageRequirement {
    PackageRequirement {
        package: key(name),
        selector: PackageSelector::Version(VersionRequirement::parse(version).unwrap()),
        origin: RequirementOrigin {
            manifest: "fixture/fai.toml".into(),
            field: format!("dependencies.{name}"),
        },
    }
}

fn deps(values: &[(&str, &str)]) -> BTreeMap<DependencyAlias, PackageRequirement> {
    values
        .iter()
        .map(|(name, version)| (DependencyAlias::parse(name).unwrap(), requirement(name, version)))
        .collect()
}

fn candidate(name: &str, version: &str, values: &[(&str, &str)]) -> PackageCandidate {
    let version = PackageVersion::parse(version).unwrap();
    let number = version.as_semver().major * 10000
        + version.as_semver().minor * 100
        + version.as_semver().patch
        + 1;
    let PackageKey::Git(package) = key(name) else { unreachable!() };
    PackageCandidate {
        source: PackageSource::Git {
            package,
            revision: GitRevision::parse(&format!("{number:040x}")).unwrap(),
        },
        version,
        released: true,
        compiler: VersionRequirement::default(),
        platforms: Vec::new(),
        dependencies: deps(values),
        development: BTreeMap::new(),
        manifest_fingerprint: "fixture".into(),
        checksum: None,
    }
}

fn root(values: &[(&str, &str)]) -> ResolutionRequest {
    let mut candidate = candidate("root", "1.0.0", values);
    candidate.source = PackageSource::Path(".".into());
    candidate.released = false;
    ResolutionRequest {
        root: PackageKey::Path(".".into()),
        candidate,
        compiler: PackageVersion::parse("0.1.0").unwrap(),
        target: TargetPlatform::parse("x86_64-unknown-linux-gnu").unwrap(),
        pins: BTreeMap::new(),
        overrides: BTreeMap::new(),
        policy: ResolutionPolicy::UpdateAll,
    }
}

fn add(catalog: &mut Catalog, name: &str, version: &str, values: &[(&str, &str)]) {
    catalog.0.entry(key(name)).or_default().candidates.push(candidate(name, version, values));
}

fn preference_fixture() -> (Catalog, ResolutionRequest) {
    let mut catalog = Catalog::default();
    add(&mut catalog, "a", "1.9.0", &[("shared", "^1.0.0")]);
    add(&mut catalog, "a", "1.8.0", &[("shared", "^2.0.0")]);
    add(&mut catalog, "b", "1.9.0", &[("shared", "^2.0.0")]);
    add(&mut catalog, "b", "1.8.0", &[("shared", "^1.0.0")]);
    add(&mut catalog, "shared", "1.0.0", &[]);
    add(&mut catalog, "shared", "2.0.0", &[]);
    let mut request = root(&[("a", "^1.0.0"), ("b", "^1.0.0")]);
    request.pins.insert(key("a"), PackagePin::candidate(&catalog.0[&key("a")].candidates[0]));
    request
        .pins
        .insert(key("shared"), PackagePin::candidate(&catalog.0[&key("shared")].candidates[0]));
    (catalog, request)
}

#[test]
fn conservative_add_keeps_a_and_selects_an_older_b() {
    let (mut catalog, mut request) = preference_fixture();
    request.policy = ResolutionPolicy::Conservative;
    let graph = resolve_graph(&mut catalog, &request).unwrap();
    assert_eq!(graph.packages[&key("a")].version.to_string(), "1.9.0");
    assert_eq!(graph.packages[&key("b")].version.to_string(), "1.8.0");
}

#[test]
fn targeted_update_can_downgrade_another_compatible_package() {
    let (mut catalog, mut request) = preference_fixture();
    request.policy = ResolutionPolicy::Update(BTreeSet::from([key("b")]));
    let graph = resolve_graph(&mut catalog, &request).unwrap();
    assert_eq!(graph.packages[&key("a")].version.to_string(), "1.8.0");
    assert_eq!(graph.packages[&key("b")].version.to_string(), "1.9.0");
    assert_eq!(graph.packages[&key("shared")].version.to_string(), "2.0.0");
}

#[test]
fn full_update_prefers_root_dependencies_before_transitives() {
    let (mut catalog, request) = preference_fixture();
    let graph = resolve_graph(&mut catalog, &request).unwrap();
    assert_eq!(graph.packages[&key("a")].version.to_string(), "1.9.0");
    assert_eq!(graph.packages[&key("b")].version.to_string(), "1.8.0");
}

#[test]
fn development_constraints_participate_in_the_normal_selection() {
    let mut catalog = Catalog::default();
    add(&mut catalog, "shared", "1.0.0", &[]);
    add(&mut catalog, "shared", "2.0.0", &[]);
    add(&mut catalog, "helper", "1.0.0", &[("shared", "^1.0.0")]);
    let mut request = root(&[("shared", "*")]);
    request.candidate.development = deps(&[("helper", "*")]);
    let graph = resolve_graph(&mut catalog, &request).unwrap();
    assert_eq!(graph.packages[&key("shared")].version.to_string(), "1.0.0");
    assert!(graph.edges.iter().any(|edge| edge.from.is_none() && edge.to == key("helper")));
}

#[test]
fn consumed_development_dependencies_are_not_inherited() {
    let mut catalog = Catalog::default();
    let mut library = candidate("library", "1.0.0", &[]);
    library.development = deps(&[("absent", "*")]);
    catalog.0.entry(key("library")).or_default().candidates.push(library);
    let graph = resolve_graph(&mut catalog, &root(&[("library", "*")])).unwrap();
    assert!(!graph.packages.contains_key(&key("absent")));
}

#[test]
fn incompatible_edges_do_not_create_two_instances() {
    let mut catalog = Catalog::default();
    add(&mut catalog, "a", "1.0.0", &[("shared", "^1.0.0")]);
    add(&mut catalog, "b", "1.0.0", &[("shared", "^2.0.0")]);
    add(&mut catalog, "shared", "1.0.0", &[]);
    add(&mut catalog, "shared", "2.0.0", &[]);
    assert!(matches!(
        resolve_graph(&mut catalog, &root(&[("a", "*"), ("b", "*")])),
        Err(ResolutionError::Conflict(_))
    ));
}

#[test]
fn untagged_matching_versions_do_not_satisfy_release_edges() {
    let mut catalog = Catalog::default();
    let mut unpublished = candidate("library", "1.0.0", &[]);
    unpublished.released = false;
    catalog.0.entry(key("library")).or_default().candidates.push(unpublished);
    assert!(resolve_graph(&mut catalog, &root(&[("library", "^1.0.0")])).is_err());
}

#[test]
fn explicit_revision_override_retains_original_version_constraints() {
    let mut catalog = Catalog::default();
    let mut unpublished = candidate("fork", "1.2.0", &[]);
    unpublished.released = false;
    let PackageSource::Git { revision, .. } = unpublished.source.clone() else { unreachable!() };
    catalog.0.entry(key("fork")).or_default().candidates.push(unpublished);
    let mut request = root(&[("library", "^1.0.0")]);
    let mut replacement = requirement("fork", "*");
    replacement.selector = PackageSelector::Revision(revision);
    request.overrides.insert(key("library"), replacement);
    let graph = resolve_graph(&mut catalog, &request).unwrap();
    assert!(graph.packages.contains_key(&key("library")));
    assert!(!graph.packages.contains_key(&key("fork")));
    assert_eq!(graph.packages[&key("library")].version.to_string(), "1.2.0");
}

#[test]
fn incompatible_override_cannot_bypass_an_original_range() {
    let mut catalog = Catalog::default();
    add(&mut catalog, "fork", "2.0.0", &[]);
    let mut request = root(&[("library", "^1.0.0")]);
    request.overrides.insert(key("library"), requirement("fork", "*"));
    assert!(resolve_graph(&mut catalog, &request).is_err());
}

#[test]
fn compiler_compatibility_filters_candidates_before_preference() {
    let mut catalog = Catalog::default();
    add(&mut catalog, "library", "1.0.0", &[]);
    let mut newer = candidate("library", "1.1.0", &[]);
    newer.compiler = VersionRequirement::parse(">=0.5.0").unwrap();
    catalog.0.entry(key("library")).or_default().candidates.push(newer);
    let graph = resolve_graph(&mut catalog, &root(&[("library", "*")])).unwrap();
    assert_eq!(graph.packages[&key("library")].version.to_string(), "1.0.0");
}

#[test]
fn normal_dependency_cycles_are_unsatisfiable() {
    let mut catalog = Catalog::default();
    add(&mut catalog, "a", "1.0.0", &[("b", "*")]);
    add(&mut catalog, "b", "1.0.0", &[("a", "*")]);
    assert!(resolve_graph(&mut catalog, &root(&[("a", "*")])).is_err());
}

#[test]
fn a_development_helper_can_depend_on_the_root_library() {
    let mut catalog = Catalog::default();
    let mut request = root(&[]);
    request.candidate.development = deps(&[("helper", "*")]);
    let mut helper = candidate("helper", "1.0.0", &[]);
    helper.dependencies.insert(
        DependencyAlias::parse("root").unwrap(),
        PackageRequirement {
            package: request.root.clone(),
            selector: PackageSelector::Path(None),
            origin: RequirementOrigin {
                manifest: "helper/fai.toml".into(),
                field: "dependencies.root".into(),
            },
        },
    );
    catalog.0.entry(key("helper")).or_default().candidates.push(helper);
    assert!(resolve_graph(&mut catalog, &request).is_ok());
}

#[test]
fn branches_can_unify_when_they_identify_the_same_commit() {
    let mut catalog = Catalog::default();
    let library = candidate("library", "1.0.0", &[]);
    let PackageSource::Git { revision, .. } = library.source.clone() else { unreachable!() };
    let stable = GitBranch::parse("stable").unwrap();
    let main = GitBranch::parse("main").unwrap();
    catalog.0.insert(
        key("library"),
        CandidateSet {
            candidates: vec![library],
            branches: BTreeMap::from([
                (stable.clone(), revision.clone()),
                (main.clone(), revision),
            ]),
        },
    );
    let mut request = root(&[]);
    let mut edge = requirement("library", "*");
    edge.selector = PackageSelector::Branch(main);
    request.candidate.dependencies.insert(DependencyAlias::parse("one").unwrap(), edge.clone());
    edge.selector = PackageSelector::Branch(stable);
    request.candidate.dependencies.insert(DependencyAlias::parse("two").unwrap(), edge);
    let graph = resolve_graph(&mut catalog, &request).unwrap();
    assert_eq!(graph.packages.len(), 2);
    assert_eq!(graph.branches[&key("library")].len(), 2);
}

#[test]
fn provider_failure_is_not_hidden_by_falling_back_to_another_version() {
    struct Broken;
    impl ResolverProvider for Broken {
        fn candidates(
            &mut self,
            _: &PackageKey,
            _: &[PackageSelector],
            _: Option<&PackagePin>,
            _: bool,
        ) -> Result<CandidateSet, ProviderFailure> {
            Err(ProviderFailure { code: SOURCE_UNAVAILABLE, message: "transport failed".into() })
        }
    }
    assert!(matches!(
        resolve_graph(&mut Broken, &root(&[("library", "*")])),
        Err(ResolutionError::Provider(_))
    ));
}

#[test]
fn target_platforms_can_choose_different_compatible_versions() {
    let mut catalog = Catalog::default();
    add(&mut catalog, "library", "1.0.0", &[]);
    let mut latest = candidate("library", "2.0.0", &[]);
    latest.platforms = vec![PlatformPredicate::new(Some("windows"), None, None).unwrap()];
    catalog.0.entry(key("library")).or_default().candidates.push(latest);
    let mut request = root(&[("library", "*")]);
    let linux = resolve_graph(&mut catalog, &request).unwrap();
    request.target = TargetPlatform::parse("x86_64-pc-windows-msvc").unwrap();
    let windows = resolve_graph(&mut catalog, &request).unwrap();
    assert_eq!(linux.packages[&key("library")].version.to_string(), "1.0.0");
    assert_eq!(windows.packages[&key("library")].version.to_string(), "2.0.0");
}

#[test]
fn conflicting_branches_cannot_select_multiple_revisions() {
    let mut catalog = Catalog::default();
    let first = candidate("library", "1.0.0", &[]);
    let second = candidate("library", "2.0.0", &[]);
    let revision = |candidate: &PackageCandidate| match &candidate.source {
        PackageSource::Git { revision, .. } => revision.clone(),
        _ => unreachable!(),
    };
    let main = GitBranch::parse("main").unwrap();
    let stable = GitBranch::parse("stable").unwrap();
    let branches =
        BTreeMap::from([(main.clone(), revision(&first)), (stable.clone(), revision(&second))]);
    catalog.0.insert(key("library"), CandidateSet { candidates: vec![first, second], branches });
    let mut request = root(&[]);
    let mut edge = requirement("library", "*");
    edge.selector = PackageSelector::Branch(main);
    request.candidate.dependencies.insert(DependencyAlias::parse("main").unwrap(), edge.clone());
    edge.selector = PackageSelector::Branch(stable);
    request.candidate.dependencies.insert(DependencyAlias::parse("stable").unwrap(), edge);
    assert!(resolve_graph(&mut catalog, &request).is_err());
}

#[test]
fn root_path_replacement_coalesces_a_helpers_back_reference() {
    let mut catalog = Catalog::default();
    add(&mut catalog, "helper", "1.0.0", &[("library", "^1.0.0")]);
    let mut request = root(&[]);
    request.candidate.development = deps(&[("helper", "*")]);
    let mut replacement = requirement("library", "*");
    replacement.package = request.root.clone();
    replacement.selector = PackageSelector::Path(None);
    request.overrides.insert(key("library"), replacement);
    let graph = resolve_graph(&mut catalog, &request).unwrap();
    assert_eq!(graph.packages.len(), 2);
    assert!(
        graph.edges.iter().any(|edge| edge.from == Some(key("helper")) && edge.to == request.root)
    );
}

#[test]
fn root_replacement_also_checks_its_own_path_version_constraint() {
    let mut catalog = Catalog::default();
    add(&mut catalog, "helper", "1.0.0", &[("library", "^1.0.0")]);
    let mut request = root(&[]);
    request.candidate.development = deps(&[("helper", "*")]);
    let mut replacement = requirement("library", "*");
    replacement.package = request.root.clone();
    replacement.selector =
        PackageSelector::Path(Some(VersionRequirement::parse("^2.0.0").unwrap()));
    request.overrides.insert(key("library"), replacement);
    assert!(resolve_graph(&mut catalog, &request).is_err());
}

#[test]
fn source_providers_cannot_return_another_repositories_metadata() {
    let mut catalog = Catalog::default();
    catalog.0.entry(key("library")).or_default().candidates.push(candidate("other", "1.0.0", &[]));
    assert!(matches!(
        resolve_graph(&mut catalog, &root(&[("library", "*")])),
        Err(ResolutionError::Provider(_))
    ));
}

#[test]
fn conflicts_keep_converging_manifest_field_provenance() {
    let mut catalog = Catalog::default();
    add(&mut catalog, "a", "1.0.0", &[("shared", "^1.0.0")]);
    add(&mut catalog, "b", "1.0.0", &[("shared", "^2.0.0")]);
    let Err(ResolutionError::Conflict(conflict)) =
        resolve_graph(&mut catalog, &root(&[("a", "*"), ("b", "*")]))
    else {
        panic!("expected conflict");
    };
    assert_eq!(
        conflict
            .requirements
            .iter()
            .filter(|requirement| requirement.origin.field == "dependencies.shared")
            .count(),
        2
    );
}

#[test]
fn duplicate_root_normal_and_development_aliases_are_rejected() {
    let mut request = root(&[("library", "*")]);
    request.candidate.development = deps(&[("library", "*")]);
    let error = resolve_graph(&mut Catalog::default(), &request).unwrap_err();
    assert!(error.to_string().contains("duplicate active dependency alias"));
}

#[test]
fn obsolete_pins_do_not_keep_unreachable_packages_alive() {
    let mut request = root(&[]);
    request.policy = ResolutionPolicy::Conservative;
    request
        .pins
        .insert(key("obsolete"), PackagePin::candidate(&candidate("obsolete", "1.0.0", &[])));
    let graph = resolve_graph(&mut Catalog::default(), &request).unwrap();
    assert_eq!(graph.packages.len(), 1);
}

mod proptests {
    use super::*;
    use proptest::prelude::*;

    fn range(selector: u8) -> &'static str {
        match selector {
            1 => "^1.0.0",
            2 => "^2.0.0",
            _ => "*",
        }
    }

    proptest! {
        #[test]
        fn newest_graph_matches_an_exhaustive_diamond_oracle(a in prop::collection::vec(0u8..3, 1..6), b in prop::collection::vec(0u8..3, 1..6)) {
            let mut catalog = Catalog::default();
            for (index, &selector) in a.iter().enumerate() { add(&mut catalog, "a", &format!("1.0.{index}"), &[("shared", range(selector))]); }
            for (index, &selector) in b.iter().enumerate() { add(&mut catalog, "b", &format!("1.0.{index}"), &[("shared", range(selector))]); }
            add(&mut catalog, "shared", "1.0.0", &[]);
            add(&mut catalog, "shared", "2.0.0", &[]);
            let expected = (0..a.len()).rev().flat_map(|left| (0..b.len()).rev().map(move |right| (left, right)))
                .find(|&(left, right)| a[left] == 0 || b[right] == 0 || a[left] == b[right]);
            let request = root(&[("a", "*"), ("b", "*")]);
            let actual = resolve_graph(&mut catalog, &request);
            if let Some((left, right)) = expected {
                let graph = actual.unwrap();
                prop_assert_eq!(graph.packages[&key("a")].version.as_semver().patch, left as u64);
                prop_assert_eq!(graph.packages[&key("b")].version.as_semver().patch, right as u64);
                for set in catalog.0.values_mut() { set.candidates.reverse(); }
                prop_assert_eq!(resolve_graph(&mut catalog, &request).unwrap(), graph);
            } else { prop_assert!(actual.is_err()); }
        }
    }
}

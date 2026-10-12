//! Generated lock structure is validated independently of its fingerprints.

use std::collections::BTreeMap;

use fai_package::*;

fn candidate(path: &str) -> PackageCandidate {
    PackageCandidate {
        source: PackageSource::Path(path.into()),
        version: PackageVersion::parse("1.0.0").unwrap(),
        released: false,
        compiler: VersionRequirement::default(),
        platforms: Vec::new(),
        dependencies: BTreeMap::new(),
        development: BTreeMap::new(),
        manifest_fingerprint: Digest::of("manifest", path.as_bytes()).as_str().into(),
        checksum: None,
    }
}

fn lock() -> LockFile {
    let root = PackageKey::Path("app".into());
    let dependency = PackageKey::Path("lib".into());
    let graph = ResolvedGraph {
        root: root.clone(),
        packages: BTreeMap::from([
            (root.clone(), candidate("app")),
            (dependency.clone(), candidate("lib")),
        ]),
        edges: vec![ResolvedEdge {
            from: Some(root),
            alias: DependencyAlias::parse("lib").unwrap(),
            to: dependency,
        }],
        branches: BTreeMap::new(),
    };
    let mut lock = LockFile::new(LockContext::Shared);
    lock.record(
        "app",
        &TargetPlatform::parse("x86_64-unknown-linux-gnu").unwrap(),
        &PackageVersion::parse("0.1.0").unwrap(),
        Digest::of("inputs", b"app"),
        &graph,
        None,
    )
    .unwrap();
    lock
}

#[test]
fn generated_locks_round_trip_canonically() {
    let first = lock().to_toml().unwrap();
    let second = LockFile::parse(&first).unwrap().to_toml().unwrap();
    assert_eq!(first, second);
}

#[test]
fn a_plausible_input_hash_does_not_validate_a_missing_edge_target() {
    let mut lock = lock();
    lock.graphs[0].edges[0].to = "missing".into();
    assert_eq!(lock.validate().unwrap_err().code, INVALID_LOCK);
}

#[test]
fn duplicate_graph_member_records_are_rejected() {
    let mut lock = lock();
    let member = lock.graphs[0].packages[0].clone();
    lock.graphs[0].packages.push(member);
    assert!(lock.validate().is_err());
}

#[test]
fn normal_cycles_are_rejected_in_generated_state() {
    let mut lock = lock();
    let edge = lock.graphs[0].edges[0].clone();
    lock.graphs[0].edges.push(LockedEdge {
        from: edge.to,
        to: edge.from,
        alias: DependencyAlias::parse("back").unwrap(),
        kind: LockedEdgeKind::Normal,
    });
    assert!(lock.validate().unwrap_err().message.contains("cycle"));
}

#[test]
fn unsupported_versions_are_not_reinterpreted_as_current() {
    let text = lock().to_toml().unwrap().replace("lock-version = 1", "lock-version = 2");
    assert_eq!(LockFile::parse(&text).unwrap_err().code, UNSUPPORTED_LOCK);
}

#[test]
fn package_descriptor_tampering_invalidates_its_identity() {
    let mut lock = lock();
    lock.packages[0].version = PackageVersion::parse("2.0.0").unwrap();
    assert!(lock.validate().is_err());
}

#[test]
fn unknown_fields_are_not_silently_ignored() {
    let text = format!("typo = true\n{}", lock().to_toml().unwrap());
    assert!(LockFile::parse(&text).is_err());
}

#[test]
fn replacing_an_independent_graph_retains_previous_selections() {
    let mut lock = lock();
    let first = lock.graphs[0].clone();
    let root = PackageKey::Path("other".into());
    let graph = ResolvedGraph {
        root: root.clone(),
        packages: BTreeMap::from([(root, candidate("other"))]),
        edges: Vec::new(),
        branches: BTreeMap::new(),
    };
    lock.record(
        "other",
        &first.target,
        &first.resolved_with,
        Digest::of("inputs", b"other"),
        &graph,
        None,
    )
    .unwrap();
    assert_eq!(lock.graph("app", &first.target), Some(&first));
    assert_eq!(lock.packages.len(), 3);
}

#[test]
fn baseline_hashes_do_not_depend_on_record_order() {
    let mut lock = lock();
    let before = lock.graphs[0].semantic_hash().unwrap();
    lock.graphs[0].packages.reverse();
    lock.graphs[0].edges.reverse();
    assert_eq!(lock.graphs[0].semantic_hash().unwrap(), before);
}

#[test]
fn rejected_graph_replacement_keeps_the_previous_document() {
    let mut lock = lock();
    let before = lock.to_toml().unwrap();
    let root = PackageKey::Path("other".into());
    let mut invalid = candidate("other");
    invalid.manifest_fingerprint = "invalid".into();
    let graph = ResolvedGraph {
        root: root.clone(),
        packages: BTreeMap::from([(root, invalid)]),
        edges: Vec::new(),
        branches: BTreeMap::new(),
    };
    let previous = lock.graphs[0].clone();
    assert!(
        lock.record(
            "other",
            &previous.target,
            &previous.resolved_with,
            Digest::of("inputs", b"other"),
            &graph,
            None
        )
        .is_err()
    );
    assert_eq!(lock.to_toml().unwrap(), before);
}

mod proptests {
    use super::*;
    use proptest::prelude::*;

    proptest! {
        #[test]
        fn canonical_lock_output_is_independent_of_graph_member_order(count in 1usize..8, reverse in any::<bool>()) {
            let mut lock = lock();
            let root = PackageKey::Path("generated".into());
            let mut packages = BTreeMap::from([(root.clone(), candidate("generated"))]);
            let mut edges = Vec::new();
            for index in 0..count {
                let path = format!("library{index}");
                let key = PackageKey::Path(path.clone());
                packages.insert(key.clone(), candidate(&path));
                edges.push(ResolvedEdge { from: Some(root.clone()), to: key, alias: DependencyAlias::parse(&path).unwrap() });
            }
            let graph = ResolvedGraph { root, packages, edges, branches: BTreeMap::new() };
            let target = lock.graphs[0].target.clone();
            let compiler = lock.graphs[0].resolved_with.clone();
            lock.record("generated", &target, &compiler, Digest::of("inputs", b"generated"), &graph, None).unwrap();
            let canonical = lock.to_toml().unwrap();
            if reverse {
                lock.packages.reverse(); lock.graphs.reverse();
                for graph in &mut lock.graphs { graph.packages.reverse(); graph.edges.reverse(); }
            }
            prop_assert_eq!(lock.to_toml().unwrap(), canonical.clone());
            prop_assert_eq!(LockFile::parse(&canonical).unwrap().to_toml().unwrap(), canonical);
        }
    }
}

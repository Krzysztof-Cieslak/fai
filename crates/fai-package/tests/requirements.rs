use fai_package::{PackageVersion, VersionRequirement};

fn matches(requirement: &str, version: &str) -> bool {
    VersionRequirement::parse(requirement)
        .unwrap()
        .matches(&PackageVersion::parse(version).unwrap())
}

#[test]
fn bare_versions_are_exact() {
    assert!(matches("1.2.3", "1.2.3"));
    assert!(!matches("1.2.3", "1.2.4"));
    assert_eq!(VersionRequirement::parse("1.2.3").unwrap().to_string(), "=1.2.3");
}

#[test]
fn stable_caret_bounds_follow_the_major() {
    assert!(!matches("^1.2.3", "1.2.2"));
    assert!(matches("^1.2.3", "1.9.9"));
    assert!(!matches("^1.2.3", "2.0.0"));
}

#[test]
fn zero_major_caret_bounds_follow_the_minor() {
    assert!(matches("^0.2.3", "0.2.9"));
    assert!(!matches("^0.2.3", "0.3.0"));
}

#[test]
fn zero_minor_caret_bounds_follow_the_patch() {
    assert!(matches("^0.0.3", "0.0.3"));
    assert!(!matches("^0.0.3", "0.0.4"));
}

#[test]
fn tilde_bounds_follow_the_minor() {
    assert!(matches("~1.2.3", "1.2.9"));
    assert!(!matches("~1.2.3", "1.3.0"));
}

#[test]
fn comparisons_intersect() {
    assert!(matches(">=1.2.3, <1.7.0", "1.6.9"));
    assert!(!matches(">=1.2.3, <1.7.0", "1.7.0"));
}

#[test]
fn disjunctions_preserve_gaps() {
    assert!(matches("^1.0.0 || ^3.0.0", "3.1.0"));
    assert!(!matches("^1.0.0 || ^3.0.0", "2.1.0"));
}

#[test]
fn canonical_order_is_independent_of_comparator_and_arm_order() {
    assert_eq!(
        VersionRequirement::parse("<2.0.0, >=1.0.0 || ^3.0.0").unwrap(),
        VersionRequirement::parse("^3.0.0 || >=1.0.0, <2.0.0").unwrap()
    );
}

#[test]
fn wildcard_does_not_opt_into_prereleases() {
    assert!(matches("*", "999.0.0"));
    assert!(!matches("*", "1.0.0-rc.1"));
}

#[test]
fn stable_upper_bound_does_not_admit_preview_of_the_upper_release() {
    assert!(!matches(">=1.0.0, <2.0.0", "2.0.0-rc.1"));
}

#[test]
fn prerelease_opt_in_is_limited_to_its_release_core() {
    assert!(matches("^2.0.0-rc.1", "2.0.0-rc.2"));
    assert!(matches("^2.0.0-rc.1", "2.1.0"));
    assert!(!matches("^2.0.0-rc.1", "2.1.0-beta.1"));
}

#[test]
fn prerelease_opt_in_is_local_to_a_satisfied_union_arm() {
    assert!(!matches("=2.0.0-rc.1 || >=2.0.0-rc.3", "2.0.0-rc.2"));
}

#[test]
fn exact_prerelease_is_accepted() {
    assert!(matches("2.0.0-rc.1", "2.0.0-rc.1"));
    assert!(!matches("2.0.0-rc.1", "2.0.0-rc.2"));
}

#[test]
fn partial_versions_are_rejected() {
    assert!(VersionRequirement::parse("^1.2").is_err());
}

#[test]
fn partial_wildcards_are_rejected() {
    assert!(VersionRequirement::parse("1.x").is_err());
}

#[test]
fn build_metadata_is_rejected() {
    assert!(PackageVersion::parse("1.2.3+build").is_err());
    assert!(VersionRequirement::parse("=1.2.3+build").is_err());
}

#[test]
fn trailing_empty_union_arm_is_rejected() {
    assert!(VersionRequirement::parse("^1.0.0 ||").is_err());
}

#[test]
fn trailing_empty_intersection_is_rejected() {
    assert!(VersionRequirement::parse("^1.0.0,").is_err());
}

#[test]
fn empty_requirement_is_rejected() {
    assert!(VersionRequirement::parse(" ").is_err());
}

#[test]
fn comparator_lists_require_commas() {
    assert!(VersionRequirement::parse(">=1.0.0 <2.0.0").is_err());
}

mod proptests {
    use fai_package::{
        GitRepository, ManifestDocument, PackagePath, PackageVersion, VersionRequirement,
    };
    use fai_span::SourceId;
    use proptest::prelude::*;

    proptest! {
        #[test]
        fn arbitrary_manifest_diagnostics_stay_in_utf8_bounds(text in any::<String>()) {
            if let Err(diagnostics) = ManifestDocument::parse(SourceId::new(1), &text) {
                for diagnostic in diagnostics {
                    let span = diagnostic.primary;
                    prop_assert!(span.start() <= span.end());
                    prop_assert!(span.end().to_usize() <= text.len());
                    prop_assert!(text.is_char_boundary(span.start().to_usize()));
                    prop_assert!(text.is_char_boundary(span.end().to_usize()));
                    for label in diagnostic.secondary {
                        prop_assert!(label.span.end().to_usize() <= text.len());
                        prop_assert!(text.is_char_boundary(label.span.start().to_usize()));
                        prop_assert!(text.is_char_boundary(label.span.end().to_usize()));
                    }
                }
            }
        }

        #[test]
        fn arbitrary_local_manifests_do_not_panic(text in any::<String>()) {
            let _ = ManifestDocument::parse_local(SourceId::new(1), &text);
        }

        #[test]
        fn arbitrary_requirements_round_trip_when_valid(text in any::<String>()) {
            if let Ok(requirement) = VersionRequirement::parse(&text) {
                prop_assert_eq!(VersionRequirement::parse(&requirement.to_string()).unwrap(), requirement);
            }
        }

        #[test]
        fn normalized_paths_are_relative_and_idempotent(text in any::<String>()) {
            if let Ok(path) = PackagePath::parse(&text) {
                prop_assert!(!path.as_str().starts_with('/'));
                prop_assert!(!path.as_str().split('/').any(|part| part == ".."));
                prop_assert_eq!(PackagePath::parse(path.as_str()).unwrap(), path);
            }
        }

        #[test]
        fn arbitrary_repository_locators_do_not_panic(text in any::<String>()) {
            if let Ok(repository) = GitRepository::parse(&text) {
                prop_assert_eq!(GitRepository::parse(repository.as_str()).unwrap(), repository);
            }
        }

        #[test]
        fn stable_caret_matches_its_numeric_interval(
            major in 0u64..5, minor in 0u64..5, patch in 0u64..5,
            candidate_major in 0u64..6, candidate_minor in 0u64..6, candidate_patch in 0u64..6,
        ) {
            let lower = (major, minor, patch);
            let upper = if major > 0 { (major + 1, 0, 0) }
                else if minor > 0 { (0, minor + 1, 0) }
                else { (0, 0, patch + 1) };
            let candidate = (candidate_major, candidate_minor, candidate_patch);
            let requirement = VersionRequirement::parse(&format!("^{major}.{minor}.{patch}")).unwrap();
            let version = PackageVersion::parse(&format!("{candidate_major}.{candidate_minor}.{candidate_patch}")).unwrap();
            prop_assert_eq!(requirement.matches(&version), candidate >= lower && candidate < upper);
        }

        #[test]
        fn generated_ranges_keep_matches_after_canonicalization(
            major in 0u64..20, minor in 0u64..20, patch in 0u64..20,
            candidate_major in 0u64..21, candidate_minor in 0u64..21, candidate_patch in 0u64..21,
        ) {
            let text = format!("^{}.0.0 || <{}.0.0, >={major}.{minor}.{patch}", major + 2, major + 1);
            let requirement = VersionRequirement::parse(&text).unwrap();
            let canonical = VersionRequirement::parse(&requirement.to_string()).unwrap();
            let version = PackageVersion::parse(&format!("{candidate_major}.{candidate_minor}.{candidate_patch}")).unwrap();
            prop_assert_eq!(requirement.matches(&version), canonical.matches(&version));
        }

        #[test]
        fn comments_do_not_change_semantic_manifest_data(comment in "[^\r\n]{0,100}") {
            let plain = "manifest-version = 1\n[package]\nname = 'demo'\nversion = '0.1.0'\n";
            let with_comment = format!("# {comment}\n{plain}");
            if let Ok(commented) = ManifestDocument::parse(SourceId::new(0), &with_comment) {
                let original = ManifestDocument::parse(SourceId::new(0), plain).unwrap();
                prop_assert_eq!(original.manifest(), commented.manifest());
            }
        }
    }
}

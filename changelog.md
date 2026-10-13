# Changelog

## [Unreleased]

### Fixed
- Prevent Windows launcher forwarding from retaining captured output in a daemon;
  exercise forwarding with explicitly built launchers and short Unix socket paths.
- Emit supported absolute symbol relocations for ARM64 Windows native objects.
- Validate Windows installer archive names with ordinal ordering and retain build
  artifacts for diagnosing release qualification failures. Build-only native
  qualification includes process and isolated-build diagnostics before publication.

## [0.0.1] - 2026-10-13

- Initial release.

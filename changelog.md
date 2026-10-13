# Changelog

Compiler, runtime, tooling, and embedded standard-library changes are recorded here.
Source packages under `packages/` have independent release histories.

## [Unreleased]

### Added
- Native side-by-side compiler installation, project pins, and the `fai` launcher
  for Linux, macOS, and Windows on x86-64 and ARM64.
- `fai upgrade` for the default compiler and `fai installer upgrade` for the
  launcher, alongside explicit `fai toolchain` installation and selection commands.
- Manually dispatched, all-platform GitHub releases with immutable version tags,
  verified archives, POSIX and PowerShell installers, and agent-driven release helpers.
- Native Windows fibers for scheduler stacks on x86-64 and ARM64, including
  migration of suspended tasks between worker threads.

### Changed
- Compiler, tooling, and embedded standard-library changes require changelog entries;
  GitHub release notes are taken from the committed version section.
- Native runtime archive builds use the compiler target and locked dependencies.
- Editor workspaces reselect their compiler when the effective project pin changes.

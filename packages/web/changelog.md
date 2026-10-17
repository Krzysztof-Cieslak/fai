# Changelog

## [Unreleased]

### Changed
- Store route methods as startup metadata and dispatch candidates directly,
  avoiding per-request handler lists while preserving captures and fallback order.

## [0.1.0] - 2026-10-15

### Added
- Initial release: composable web handlers/routing, JSON binding/responses, middleware headers, and managed SSE output.

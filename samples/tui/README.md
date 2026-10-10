# Full-screen TUI examples

The source-package examples are compiled and exercised by dependency-aware CI:

- [`FormDemo.fai`](../../packages/tui/examples/FormDemo.fai): typed forms, validation,
  focus and submission.
- [`DataBrowser.fai`](../../packages/tui/examples/DataBrowser.fai): virtualized paging,
  filtering and sorting over 10,000 SQLite rows.
- [`AgentDemo.fai`](../../packages/tui/examples/AgentDemo.fai): streaming Markdown,
  a multiline composer, history and cancellable subscriptions.
- [`RuntimeChecks.fai`](../../packages/tui/examples/RuntimeChecks.fai): headless
  lifecycle checks through a supplied backend.

Run, for example, `fai run --no-daemon -C packages tui/examples/FormDemo.fai`
from the repository root in a real terminal. See the
[package guide](../../packages/tui/README.md) for the API and lifecycle contract.

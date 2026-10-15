# Configuration examples

Version **0.1.0** · Requires **Fai 0.1.x** · [Changelog](changelog.md)

The `config-examples` package keeps CLI and application helpers separate from
the [configuration library](../README.md). Its release tag is
`packages/config/examples/v0.1.0`.

From the repository root:

```sh
fai run -C packages config/examples/Application.fai -- --help
fai run -C packages config/examples/RuntimeChecks.fai
fai test -C packages --package config-examples
```

`Settings.fai` supplies shared typed settings. `RuntimeChecks.fai` exercises
file loading, environment lookup, and presence-aware command-line overrides.
The package's `ci.json` runs the native fixture through JIT and AOT.

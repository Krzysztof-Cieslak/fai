# Glob examples and interoperability checks

Version **0.1.0** · Requires **Fai 0.1.x** · [Changelog](changelog.md)

The `glob-examples` package keeps native process dependencies out of the
[glob library](../README.md). Its release tag is `packages/glob/examples/v0.1.0`.

From the repository root, with Git installed:

```sh
fai run -C packages glob/examples/RuntimeChecks.fai
fai run -C packages glob/examples/GitChecks.fai
fai test -C packages --package glob-examples
```

The fixtures exercise filesystem traversal and compare ignore decisions with
Git in an isolated temporary repository. `ci.json` declares their JIT/AOT runs.

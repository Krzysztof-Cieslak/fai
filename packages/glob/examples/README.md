# Glob examples and interoperability checks

Private example/test project · Requires **Fai 0.1.x**

The `glob-examples` package keeps native process dependencies out of the
[glob library](../README.md). It is not independently published (`publish = false`).

From the repository root, with Git installed:

```sh
fai run -C packages glob/examples/RuntimeChecks.fai
fai run -C packages glob/examples/GitChecks.fai
fai test -C packages --package glob-examples
```

The fixtures exercise filesystem traversal and compare ignore decisions with
Git in an isolated temporary repository. `ci.json` declares their JIT/AOT runs.

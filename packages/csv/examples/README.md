# CSV examples

Private example/test project · Requires **Fai 0.1.x**

The `csv-examples` package contains executable examples and native fixtures for
the [CSV library](../README.md). It is not independently published (`publish = false`).

From the repository root:

```sh
fai run -C packages csv/examples/People.fai
fai run -C packages csv/examples/RuntimeChecks.fai
fai run -C packages csv/examples/StreamChecks.fai
fai run -C packages csv/examples/StressChecks.fai
fai test -C packages --package csv-examples
```

The fixtures cover typed records, real files, chunk boundaries, mixed effects,
expired handles, and bounded-memory streaming. `ci.json` declares JIT/AOT runs.

# PostgreSQL examples

The executable examples live with the optional source package, so they can share
its dependency-aware CI and cached compiler:

- [`Basic.fai`](../../packages/postgres/examples/Basic.fai): explicit capabilities,
  environment-supplied configuration, a bound parameter and a typed row decoder.
- [`Integration.fai`](../../packages/postgres/examples/Integration.fai): common
  scalars, streaming, transactions, scoped pooling, cancellation and expiry.
- [`WireChecks.fai`](../../packages/postgres/examples/WireChecks.fai): a scripted
  loopback server exercising malformed and fragmented protocol messages.

Run from the repository root with `fai run -C packages postgres/examples/Basic.fai`.
Set `FAI_PG_URL` and, for a private TLS root, `FAI_PG_ROOT`. Full commands and
semantics are in the [package guide](../../packages/postgres/README.md).

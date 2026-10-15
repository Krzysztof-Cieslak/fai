# PostgreSQL examples

Version **0.1.0** · Requires **Fai 0.1.x** · [Changelog](changelog.md)

The `postgres-examples` package holds runnable database examples and shared test
helpers for the [PostgreSQL library](../README.md). Its release tag is
`packages/postgres/examples/v0.1.0`.

Use a disposable PostgreSQL database and set `FAI_PG_URL`; optionally set
`FAI_PG_ROOT` to a trusted PEM root for TLS:

```sh
fai run -C packages postgres/examples/Basic.fai
fai run -C packages postgres/examples/Integration.fai
```

Run these commands from the repository root. Database examples create and change
test data. `WireChecks.fai` supplies protocol helpers; see the parent README for
server setup, TLS, and the provisioned integration suite. Pure protocol tests do
not require a database: `fai test -C packages postgres`.

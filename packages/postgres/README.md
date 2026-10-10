# PostgreSQL

An explicit-capability PostgreSQL v3 client written in Fai. Include this directory
and its `sql` and `json` dependencies under your application's workspace root.

The protocol modules (`PgWire`, `PgAuth`, `PgTypes`, `PgConfig`, `PgConnect`)
provide bounded framing, checked scalar codecs, verified TLS, SCRAM-SHA-256 and
SCRAM-SHA-256-PLUS. Cryptography uses the standard `Crypto` primitives; SQL and
wire-protocol logic stay in Fai. `PgControl` supervises deadlines and cleanup.

```sh
fai fmt --check -C packages postgres
fai check --no-examples -C packages postgres
fai test -C packages postgres --seed 42 --count 128
```

Contracts run directly in Fai without a server. They cover malformed framing,
full-width integers, scalar representations, strict base64, and the published
SCRAM-SHA-256 proof vector.

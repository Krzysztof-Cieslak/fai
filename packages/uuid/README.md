# UUID

Pure Fai UUID values and codecs, following RFC 9562. `Uuid.Uuid` is an opaque,
immutable 16-byte value in network order. It accepts every 128-bit value,
including nil, max, reserved versions and non-RFC variants. Inspection identifies
versions separately from syntax validation.

```fai
match Uuid.parse "017f22e2-79b0-7cc3-98c4-dc0c0c07398f" with
| Err error -> Err (Uuid.errorToString error)
| Ok uuid -> Ok (Uuid.v7UnixMillis uuid)
```

## Codecs and inspection

`parse` accepts exactly the 8-4-4-4-12 form, with uppercase, lowercase or mixed
ASCII hex. It does not trim whitespace or guess alternative formats.
`parseCompact` accepts exactly 32 hex digits; `parseUrn` accepts a case-insensitive
ASCII `urn:uuid:` prefix followed by canonical-form text. `toString`, `toCompact`
and `toUrn` produce lowercase output. `fromBytes` requires exactly 16 bytes and
`toBytes` returns their immutable representation. Bytes are always in RFC network
order on every platform, rather than a platform-dependent GUID memory layout.

Ordinary equality, ordering, `compare`, and hash containers operate on all 128
bits. Ordering is unsigned lexicographic byte order and agrees with canonical
text order. `nil` and `max` are the all-zero and all-one values, with `isNil` and
`isMax` predicates. `variant` returns `Ncs`, `Rfc`, `Microsoft` or `Reserved`.
`version` returns the raw version nibble only for the RFC variant, including
reserved numbers. `isV4`/`isV7` require both the version and RFC variant.

## Pure construction

`v4FromRandom` takes exactly 16 bytes, setting only the version and variant bits.
`v7FromRandom unixMillis bytes` takes a timestamp in `0..281474976710655` and
exactly ten random bytes. It places the timestamp first, keeps the low four bits
of random byte 0 and the low six bits of byte 2, and preserves the other random
bytes. The caller supplies all randomness to these pure functions.

`v7UnixMillis` and `v7Instant` return the embedded timestamp only for RFC-variant
version 7 values. They return `None` for other UUIDs. Every 48-bit timestamp fits
the standard Instant conversion.

Errors have stable kinds, a message, an optional zero-based UTF-8 byte offset,
expected/actual values where relevant, and an optional original provider cause.
Text length errors count Unicode scalars; accepted text is ASCII. Binary length
errors count bytes. Inputs are length-checked before fixed-size decoding.

Run contracts with `fai test -C packages uuid`. They include RFC 9562 v4/v7
vectors, malformed inputs, full-width timestamp bounds, bit preservation,
text/byte round trips, and equality/order/hash behavior.

Reference: https://www.rfc-editor.org/rfc/rfc9562.html

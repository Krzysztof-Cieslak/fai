# Decimal codecs

Optional exact numeric adapters for `decimal`, `json`, `yaml`, and `sql`.
Existing numeric types remain unchanged. Declare `decimal-codecs` in `fai.toml`
under an alias such as `decimal_codecs`, then import the adapters:

```fai
import decimal_codecs.DecimalJson
import decimal_codecs.DecimalYaml
import decimal_codecs.DecimalSql
```

Each adapter has explicit conversions, `decoder`, `encoder` where applicable,
and `With` variants accepting `Decimal.Limits`.

- **JSON:** `fromNumber` / `toNumber` convert `JsonNumber.Number` without Float.
  `decoder` and `encoder` compose with `JsonDecode` / `JsonEncode`. NaN and
  infinities cannot be JSON numbers and return errors.
- **YAML:** `fromValue` / `toValue` preserve finite numeric values and map all
  special categories explicitly. Encoded finite Decimals use YAML's exact Float
  representation, even when integral. `decoder` / `encoder` compose with the YAML
  codecs and preserve their located error context.
- **SQL:** `fromSql` / `toSql` convert the existing `SqlDecimal.Decimal` transport
  value. `toValue` produces `Sql.Decimal`, while `decoder` is a `SqlDecode.Value`
  accepting exact SQL decimals, integer values and decimal text. SQL Float/Real
  values are rejected; callers choose an explicit Float conversion themselves.

Decimal normalization deliberately does not retain original spelling or scale.
For example, SQL text `001.2300` becomes arithmetic `1.23`; converting it back
produces `1.23`. Keep the original `SqlDecimal` alongside it when presentation
scale matters. SQL NaN/infinities round-trip through the corresponding Decimal
values; YAML specials do too. Integer negative zero normalizes to zero.

```sh
fai test -C packages --package decimal-codecs --seed 42 --count 128
python3 scripts/package_integrations.py decimal-codecs --no-dependents --fai /path/to/fai
```

Native contracts and JIT/AOT examples test values beyond binary64 precision,
normalization, special-value handling, composed decoders and budget failures.

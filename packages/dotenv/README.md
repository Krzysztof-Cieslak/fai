# Dotenv

Pure literal dotenv parsing: `Dotenv.parse`/`parseBytes` return a document with
ordered entries and name/value locations. `Dotenv.values` extracts a dictionary.
The package never changes the process environment or expands `${...}`.

Names follow `[A-Za-z_][A-Za-z0-9_]*`; optional `export` and horizontal whitespace
around `=` are supported. Unquoted values trim spaces/tabs, and `#` starts a
comment. Single quotes retain literal text. Double quotes recognize `\\`, `\"`,
`\n`, `\r`, `\t`; other escapes fail. Quoted values may span lines. CRLF within
quoted values normalizes to LF; source locations retain original byte offsets.
Trailing text after a quoted value is an error unless it is whitespace/comment.
Duplicate names report both locations. Invalid UTF-8, NUL and bare CR fail.

`parseWith`/`parseBytesWith` accept explicit limits: 4 MiB and 1,024 entries by
default. Locations use zero-based byte ranges and one-based scalar line/columns.

Run `fai test -C packages dotenv` for native Fai contracts.

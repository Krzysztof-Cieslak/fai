# Typed command-line arguments for Fai

`Args` is a pure Fai argument parser. Declarations produce a typed application
value and generate help from the same metadata. Include `src/Args.fai` in your
workspace; the package has no dependencies beyond the standard library.

```fai
let parser = Args.map2
  (fun verbose port -> { verbose = verbose, port = port })
  (Args.flag (Args.short 'v' (Args.named "verbose" "Verbose output")))
  (Args.optionWithDefault 8080 Args.int (Args.named "port" "TCP port"))

let app = Args.program (Args.version "serve 1.0" (Args.described "serve" "Example server")) parser
```

Pass `runtime.env.args ()` to `Args.parse app`. Arguments exclude the executable
name and are already tokenized: the parser preserves strings, including empty
strings and whitespace, without shell splitting or quote removal.

```fai
match Args.parse app (runtime.env.args ()) with
| Ok (Args.Parsed config) -> runtime.console.writeLine (Int.toString config.port)
| Ok (Args.Help text) -> runtime.console.write text
| Ok (Args.Version text) -> runtime.console.writeLine text
| Err error -> runtime.console.writeError (Args.errorToString error ++ "\n")
```

The parser performs no I/O or process exit. Its outcomes let the application
choose how to present help, version information, and errors.

## Declarations

`Args.named long help` constructs an `OptionInfo` with no short alias;
`Args.short 'p' info` adds one. Names omit dashes. `Args.positional name help`
constructs an `ArgumentInfo` for a usage label such as FILE.

| Declaration | Result |
|---|---|
| `flag info` | `Bool`, initially false; repetitions are idempotent |
| `count info` | `Int`, the number of occurrences |
| `option reader info` | `Option 'a`; a present option always requires a value |
| `requiredOption reader info` | `'a`; absence is an error |
| `optionWithDefault fallback reader info` | `'a`; the typed fallback is shown in help |
| `options reader info` | `List 'a` in argv order; zero occurrences is `[]` |
| `argument reader info` | one required positional value |
| `optionalArgument reader info` | `Option 'a`, consumed greedily |
| `arguments reader info` | the remaining positionals as a list |

Combine declarations with `map`, `map2`, or
`succeed constructor |> andMap declaration |> andMap declaration`.
`validateWith : ('a -> Result 'b String) -> Parser 'a -> Parser 'b` adds pure
post-parse validation, for example checking a range or relationships between
fields. These combinators retain the declaration metadata and its order.

Required positionals precede optional ones, and a variadic positional must be
last. Optional positionals take available words from left to right before any
variadic tail. Options can appear between positional words.

## Subcommands and inherited options

`Args.command name summary parser` creates a typed command. `Args.commands`
combines a list of commands into a required choice. Map the child results to
variants of one application union when commands have different payloads:

```fai
type Action =
  | Show String
  | ListAll

let action = Args.commands [
  Args.command "show" "Show one item"
    (Args.map Show (Args.argument Args.string (Args.positional "NAME" "Item name"))),
  Args.command "list" "List all items" (Args.succeed ListAll)
]

let parser = Args.map2
  (fun verbose action -> { verbose = verbose, action = action })
  (Args.flag (Args.short 'v' (Args.named "verbose" "Verbose output")))
  action
```

Commands can contain their own `commands` group, such as `tool config get KEY`.
Each command-bearing level has one group and options; positional arguments belong
to its child commands. A missing or unknown command is a structured error.

Parent options remain available before and after command selection:
`tool -v show item`, `tool show -v item`, and `tool show item -v` are equivalent.
Child-only options become available after the child name. Long/short aliases
cannot conflict with any ancestor, while siblings may reuse aliases with different
value readers. All branches are validated, including commands not selected by argv.

`--` disables option parsing for the remainder of the invocation, including after
entering a child. It does not disable command selection: `tool -- show -v` selects
`show` and passes `-v` as its positional argument. A value-taking option can consume
a word that happens to be a command name; the word stays a value.

Help follows the active command: `tool --help` shows the root, while
`tool config get --help` shows `get`, its arguments, and all inherited options.
`Args.helpFor app ["config", "get"]` produces the same help without parsing argv;
`helpFor app []` is `help app`. Inherited options are listed ancestor-first, each
level retaining declaration order. The configured version flag is available at
every level. A command named `help` is an ordinary user-defined command, distinct
from `--help`.

## Value readers

- `string` accepts the exact string, including empty input.
- `int` accepts signed 64-bit decimal integers, with optional `+`/`-` and leading
  zeroes. Overflow, surrounding whitespace, fractions and other radices fail.
- `bool` accepts exactly `true` or `false`.
- `choice ["fast", "safe"]` accepts an exact, case-sensitive string choice.
- `custom metavar read display` supports application types. Both functions are
  pure; `read` returns `Result 'a String`, and `display` renders typed defaults.
- `metavar "PORT" reader` changes the help label while retaining its behavior.

`parseValue reader text` runs a value reader on its own. Custom readers can
compose it to add domain validation. Supplied option and positional values are
decoded once after token scanning; typed defaults are returned as given.

## Syntax and precedence

- Long values: `--port 8080` or `--port=8080`.
- Short values: `-p 8080` or `-p8080`.
- Short bundles: `-vvv` counts three occurrences; `-vp8080` sets `v` and gives
  `8080` to `p`. A value-taking option consumes the rest of its bundle.
- A following value is literal, even if it looks like an option: `--text --help`
  supplies the string `--help`. Likewise `--text --` supplies `--` as the value.
- `-p=8080` supplies the literal value `=8080`; use the documented short forms
  when the reader expects an integer.
- `--` in option position ends option parsing. Use it before dash-prefixed
  positionals, including negative positional integers. A lone `-` is always an
  ordinary positional word.
- Unknown names and surplus positionals fail. Names are case-sensitive and are
  not abbreviated. A single-valued option cannot be repeated, even via an alias.

`-h`/`--help` are automatic. `Args.version text info` also enables
`-V`/`--version`, returning that exact text. A help/version request finishes the
scan immediately, bypassing missing required fields, typed readers and
post-parse validation. Earlier syntax errors still fail; later words are ignored.
An option value that happens to be `--help` remains a value.

## Help and errors

`Args.help app` returns the same help text as parsing `--help`.
`Args.validateDefinition app` validates the schema without processing argv.
Both return `Result` values. Definition validation always precedes input parsing:
it rejects invalid names/metavars, reserved help/version aliases, duplicate long
or short options, duplicate positional names, and ambiguous positional order.
Long names use ASCII letters followed by letters, digits or hyphens; short
aliases are ASCII letters. Positional names and metavars can contain Unicode,
but cannot be empty or contain whitespace.

`Args.Error` carries `kind`, `command` (a command-path list), `index` (an optional
zero-based argv index), `token` (the original word), and `message`.
`errorToString` renders a one-based argument number for humans. Syntax scanning
uses argv order; missing/invalid typed values and validation follow declaration
order. The error categories distinguish invalid definitions, unknown options,
missing/unexpected values, duplicate options, missing/unexpected arguments,
invalid values, and post-parse validation failures.
Subcommands add `UnknownCommand` and `MissingCommand`. Errors for supplied values
carry the command path where the token appeared, even for inherited options;
missing values/commands carry their declaration's path. Argument indices always
refer to the complete original argv, including command names.

Help uses deterministic declaration order, two-space row separators, quoted
string defaults, and no terminal-width or environment-dependent formatting.

## Run and test

```sh
fai run -C packages cli/examples/ArgsExample.fai -- -vp9000 input.txt
fai run -C packages cli/examples/ArgsExample.fai -- --help
fai run -C packages cli/examples/CommandsExample.fai -- config set theme dark -v
fai run -C packages cli/examples/CommandsExample.fai -- serve --help
fai test -C packages cli
fai test -C packages cli/test/HelpSpec.fai
```

The tests are Fai contracts. Repeated commands reuse the warm daemon, and the
package's `ci.json` selects the cached-compiler CI lane.

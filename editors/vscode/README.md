# Fai for Visual Studio Code

Editor support for the [Fai](https://github.com/Krzysztof-Cieslak/fai) language:
syntax highlighting (a TextMate grammar) and full language intelligence through
the `fai lsp` language server.

The client is intentionally thin — all intelligence (diagnostics, hover,
go-to-definition, completion, signature help, symbols, references, rename, code
actions, inlay hints, semantic tokens, formatting) comes from the server, which
shares the same engine as `fai check` and `fai query`.

## Requirements

The extension launches the `fai` executable. Install it (or build it from
source) and either put it on your `PATH` or point `fai.server.path` at it.

## Settings

| Setting | Default | Description |
| --- | --- | --- |
| `fai.server.path` | `fai` | Path to the `fai` executable. For a build from source, set this to e.g. `${workspaceFolder}/target/debug/fai`. |
| `fai.server.args` | `[]` | Extra arguments appended to `fai lsp`. |
| `fai.trace.server` | `off` | Trace LSP traffic in the **Fai Language Server** output channel (`off`/`messages`/`verbose`). |

## Commands

- **Fai: Restart Language Server** (`fai.restartServer`) — restart the server(s),
  e.g. after rebuilding the `fai` binary.

## Multi-root workspaces

The extension starts one `fai lsp` server per workspace folder (each folder is a
separate Fai workspace with its own warm session), and starts/stops servers as
folders are added or removed. A `.fai` file that is **not** inside any workspace
folder still gets syntax highlighting, but no language-server features.

## Developing this extension

```sh
npm install
npm run check-types     # tsc --noEmit
npm run build           # esbuild → dist/extension.cjs
npm test                # tokenize samples/ and assert the grammar has no gaps
npm run package         # produce fai.vsix with @vscode/vsce
```

Press <kbd>F5</kbd> in VS Code (with this folder open) to launch an Extension
Development Host. The grammar is `syntaxes/fai.tmLanguage.json`; it mirrors the
`fai-syntax` lexer and is pinned by `npm test` against the repository's
`samples/` corpus so it cannot silently drift.

### Smoke test

1. Build the compiler: `cargo build` (from the repository root).
2. Set `fai.server.path` to the built binary (e.g. `target/debug/fai`).
3. Open the repository's `samples/` folder and open any `.fai` file.
4. Confirm syntax highlighting, and that edits produce diagnostics.

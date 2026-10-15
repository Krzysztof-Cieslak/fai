# Diff examples

This is a separate package so using `diff` does not add CLI, TUI, or process
dependencies to an application.

```sh
fai run -C packages diff/examples/Cli.fai -- old.txt new.txt
fai run -C packages diff/examples/Cli.fai -- --patience --normalize old.txt new.txt
fai run -C packages diff/examples/Viewer.fai -- old.txt new.txt
```

Both examples accept `--patience`, `--ignore-case`, `--ignore-space`, `--normalize`
(NFC), and `--coarse`. CLI unified output always includes exact changes; review
options influence alignment, not the contents of the patch.

The viewer shows one context hunk at a time, old/new line numbers and word-level
highlighting. `n`/`p` navigate, `c` toggles case folding, `w` toggles whitespace
ignoring, `i` toggles visibility of review-equivalent changes, and `q` or Escape
quits. Highlighting within the selected hunk remains exact, so normalization
cannot duplicate or reorder the displayed original characters. Only the selected
hunk is refined; rendering uses the TUI's sanitized text path.

`Viewer.fai --snapshot OLD NEW` exercises the same model and rendering helpers
without opening a terminal. `RuntimeChecks.fai` checks exhaustive binary inputs,
sparse large arrays and shared patching. `GitChecks.fai` uses an isolated temporary
directory to test both directions of Git interoperability. All run through JIT
and AOT via this package's `ci.json`.

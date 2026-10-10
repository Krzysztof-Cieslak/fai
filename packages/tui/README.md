# Full-screen terminal applications

`tui` is an optional Fai source package for forms, data browsers and agent-style
terminal applications. Include its `src/` directory in your application's workspace.
It uses the explicit `TerminalHost` capability and the pure `TextCells` primitives.

The rendering foundation is pure: `TuiText` sanitizes display text and operates on
Unicode grapheme clusters; `TuiStyle` describes terminal colors and styles;
`TuiScreen` maintains a checked cell grid and emits differences between frames.
Overwriting any part of a wide glyph clears its complete old cell span.

```sh
fai fmt --check -C packages tui
fai check --no-examples -C packages tui
fai test -C packages tui
```

Contracts run directly in Fai. Native terminal tests additionally verify raw-mode
and alternate-screen restoration, exclusive ownership, and closed aliases.

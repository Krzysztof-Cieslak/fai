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

## Declarative widgets

`Ui` constructors accept typed attribute lists. Common attributes configure
width/height (`Ui.content`, `Ui.cells n`, `Ui.fill`, `Ui.portion weight`), padding,
borders, style and disabled state. `Ui.gap` applies to containers; input-specific
attributes include `placeholder`, `multiline`, `password` and `maxLength`.
Later attributes replace earlier values. Borders surround padding and content
regardless of attribute order. The root owns the viewport; child sizes honor
intrinsic/fixed/fill constraints, with clipping when space is insufficient.

```fai
Ui.column [Ui.gap 1, Ui.padding 1] [
  Ui.input (Ui.id "host") [Ui.width Ui.fill] {
    label = "Host", value = model.host, onChange = HostChanged
  },
  Ui.button (Ui.id "connect") [Ui.disabled model.busy] {
    label = "Connect", onPress = Connect
  }
]
```

The widget set includes text/rich text, rows/columns, inputs, a multiline editor,
buttons, checkboxes, selectable lists, tables, disclosures, scroll containers,
modal overlays and virtualized transcripts. Interactive/stateful elements require
a stable `Ui.Id`; use `Ui.childId parent dataKey` for repeated components. Duplicate
IDs are reported as a rendering error. `Ui.map` lifts a component's message type.

Values, selected item keys and validation belong to the application model.
`TuiState` owns caret, selection, scroll, focus and bounded composer history.
Unmounted widget state is discarded. Modal focus is confined to the foreground
and restored when it closes. Focus requests reveal off-screen controls inside
scroll containers; manual scrolling remains under user control afterwards.

Lists/tables take `Ui.Source { count, get }` and request only visible rows.
`get` returns `None` for a not-yet-loaded row. `onRange` reports changed visible
ranges, enabling asynchronous database paging; selection is by stable item key.
Sorting and filtering are ordinary model/source changes. Table columns currently
share width equally. `Ui.transcript` similarly reads visible rich-text lines;
user scrolling pauses tail following, and End resumes it.

`TuiLayout.render` and `TuiInput.step` are pure headless entry points. Tests can
inspect `TuiScreen.lines`, hit rectangles, interaction state, generated messages
and clipboard actions. The key mapper in `TuiInput.Options` is widget-kind-aware;
return `None` to leave a key for the application's global handler. Default editing
supports grapheme movement/deletion, selection, Home/End, paste, and Ctrl+A/C/X.
The composer submits on Enter, inserts a newline on Shift+Enter, and uses Ctrl+P/N
for submitted history. Password inputs mask graphemes and omit copying/history.

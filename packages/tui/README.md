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
Unstyled children inherit their container's normal style, beginning with the
configured theme; an explicit `Ui.style` replaces that inherited style.

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

## MVU runner and effects

`Tui.program init update view` constructs an application with no subscriptions or
global event handler. `init` receives the initial `TuiLayout.Context`, including
the real viewport size. `update : Msg -> Model -> Model * TuiCmd.Cmd Msg 'e` and
`view : TuiLayout.Context -> Model -> Ui.Element Msg` are pure. Capability values
can be captured by command thunks; their execution effects remain in `'e`.

`Tui.run runtime options program` requires explicit `terminal`, `clock`, and
`concurrency` capabilities and returns `Result Model Tui.Error`. The final model
is available after exit. A `Tui.Error` preserves the primary message and an optional
cleanup error. `Tui.runWith` accepts an owned backend of `size`, `poll`, `write`,
and `close` functions, useful for scripted integration tests.

Commands include `TuiCmd.perform`, `latest key`, `cancel key`, `focus`, `scrollTo`,
`select`, and `quit`. `batch` groups commands with one shared effect row;
`combine first second` unions differing effect rows through argument subsumption.
`TuiCmd.map` lifts child command messages. Keyed replacement ignores obsolete
results, including results already queued before cancellation. Cancelled jobs
continue to occupy their concurrency slot until they finish, keeping actual
task counts bounded. Excess work waits in the bounded pending queue.

Set `program.subscriptions` to return `TuiSub.every` or `TuiSub.stream` values.
A stream has a key and an explicit revision: unchanged revisions keep their
existing source, including a completed source. Change the revision when captured
inputs change. Removing a subscription cancels it. Stream errors map to messages.
Timers and streams share the task limits; incoming messages use a bounded FIFO
with producer backpressure and atomic batch draining.

The runner applies messages serially and coalesces redraws between batches.
`program.onEvent` handles unconsumed terminal events. Ctrl+C exits by default when
neither a widget nor the application handles it. Native close, cancellation, and
located runtime failures restore the terminal. On normal runner shutdown, queues
close and tasks are cancelled, then the backend restores the terminal before
structured task joining. Cancellation of application work remains cooperative.

| Option | Default |
| --- | --- |
| `inputPollMs` | 25 ms; bounds native input cancellation latency |
| `maxFps` | `Some 60`; `None` removes the refresh cap |
| `mailboxCapacity` | 256 messages |
| `tasks.active`, `tasks.pending` | 32 running, 1024 queued |
| `maxFeedback` | 32 visible-range reconciliation passes |
| `layout.maxCells`, `maxNodes`, `maxDepth` | 1,048,576 cells, 100,000 nodes, 128 levels |
| `input.historyLimit`, `wheelLines` | 100 entries, 3 lines |
| `mouse`, `clipboard` | enabled |
| `maxClipboardBytes` | 1 MiB |

All durations and workload limits are configurable; structural tree depth has a
hard maximum of 128. Clipboard actions retain a local copy for Ctrl+V and can also
emit OSC 52 to supporting terminals; `clipboard = false` disables that external
write. System paste arrives through the terminal's normal paste/bracketed-paste
support. Copy and cut never expose password input text.

## Typed forms

`Form.Definition 'a` combines field declarations and decoders into a typed value.
`Form.State` remains in the application model and retains raw drafts, touched and
dirty state, submission status and a revision for asynchronous validation.

```fai
let connectionForm =
  Form.map2 (fun host port -> { host = host, port = port })
    (Form.required (Form.text (Form.named "host" "Host")))
    (Form.range 1 65535
      (Form.int { Form.named "port" "Port" with initial = "5432" }))
```

Use `Form.initial` to create state. In update, call `Form.handle scope definition
event state`; it returns updated state, an optional successfully submitted typed
value, and a command that focuses the first invalid field. Lift the command with
`TuiCmd.map` when the form is a child component. In view:

```fai
Form.view scope [Ui.gap 1] connectionForm model.form
|> Ui.map FormChanged
```

`Form.text`, `password`, `int`, `checkbox`, and `choice` are the field constructors.
`map`, `map2`, `succeed` and `andMap` compose them. `optional` makes an empty
single-field draft decode as `None`. `validate` adds typed validation;
`validateAt` attaches cross-field validation to a chosen field. Independent field
errors accumulate. Integer parsing rejects overflow and preserves incomplete
drafts such as `-` without silently coercing them to zero.

The default error-display policy is `OnBlur`; `Form.withValidation` also selects
`OnChange` or `OnSubmit`. A submit attempt reveals every error. `Form.withErrors
revision errors state` installs server/async errors only when the revision still
matches. `Form.field` and `submitButton` support custom form layouts while retaining
the same definition and state. Field names must be nonempty and unique.

## Streaming Markdown

`TuiMarkdown.empty options` creates an incremental document; `append chunk doc`
adds stream text. Completed lines are cached in a persistent indexed map; only
the unfinished line is reparsed on an ordinary append. `resize options doc`
reflows when the width, theme or ambiguous-width policy changes.

Pass `TuiMarkdown.source doc` to `Ui.transcript` for virtualized display. Keep the
document in the application model and update its width when receiving a resize
event. The line-oriented renderer supports headings, bullets, blockquotes, fenced
code, emphasis, inline code and links. HTML is displayed as text, and pipe tables
retain their source layout. Terminal control characters are always sanitized.

## Examples and verification

Run an interactive example from a real terminal:

```sh
fai run --no-daemon -C packages tui/examples/FormDemo.fai
fai run --no-daemon -C packages tui/examples/DataBrowser.fai
fai run --no-daemon -C packages tui/examples/AgentDemo.fai
```

`FormDemo` submits typed connection settings. `DataBrowser` pages, filters and
sorts 10,000 rows in an embedded SQLite database, using cancellable keyed loads.
`AgentDemo` streams a simulated Markdown response, has collapsible tool output,
a multiline composer and configurable keyboard mappings. No network credentials
or external service is required. Ctrl+C exits; the agent demo also uses Escape to
cancel active streaming before exiting on a later Escape.

`RuntimeChecks.fai` runs headlessly through both JIT and AOT in CI. It checks
message dispatch, bounded backpressure, stale results, subscription completion
and removal, primary/cleanup errors and restore-before-join ordering. Native
compiler fixtures reject incompatible attributes and effectful widget callbacks.
Unix PTY fixtures exercise real keyboard input, resizing, submission and terminal
restoration for all three interactive examples; native host tests also cover
restoration on a runtime-reported fault. The required platform checks build and
exercise the backend-independent runtime on Linux, macOS and Windows.

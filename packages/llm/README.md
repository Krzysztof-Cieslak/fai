# Typed model clients

`llm` uses the `json`, `sse`, and `http-client` source packages. Request and schema
construction are pure. `LlmTypes` describes ordered messages, image inputs, tool
calls/results, usage, completion outcomes and provider-specific continuation items.

```fai
let request = Llm.request model
  |> Llm.instructions "Answer concisely."
  |> Llm.userText "Hello"
```

`LlmSchema` combines a portable wire schema with a typed JSON decoder. Compose
objects with `field` and `map2`, and use `string`, `bool`, `int`, `float`, `list`,
`nullable` and `enumeration`. Object fields are required and unknown fields are
rejected. `nullable` represents an explicit null as `Option`; it does not make the
field absent. `refine` adds local validation while retaining the underlying wire
schema. `Llm.structured` attaches the schema to a request.

Images are explicit URL values or MIME-tagged bytes. Loading a file is an
application operation with the FileSystem capability. Native continuation items
are tagged by provider. `LlmMessage.continueWith` appends the complete response and
exactly one result for each tool call, preserving the provider's item order.

Model identifiers are strings. Limits are explicit, and nonpositive token limits,
duplicate tool names, invalid image types and cross-provider continuation fail
validation. `Llm.Model 'e` is an effect-carrying generation port; scripted models
can be pure and participate in ordinary contracts.

```sh
fai fmt --check -C packages llm
fai check -C packages llm
fai test -C packages llm
```

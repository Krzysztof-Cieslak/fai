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

`LlmOpenAI.config key` selects the native Responses API. `Llm.withClient` opens a
scoped HTTP client using explicit `clock`, `concurrency`, `net`, and `tls` fields.
`generate` returns a full response; `withStream` additionally invokes sequential
event callbacks with their effects forwarded. Requests are streamed internally.
An early callback stop returns `LlmTypes.Stopped`; normal completion returns
`Finished`. EOF without the provider's terminal event is an error. Final snapshots
are decoded once rather than appended to prior deltas. `encode`, `decode`, and
`reduce` in the provider module are pure and independently testable.

OpenAI uses application-owned history and `store: false` by default. Native output
items, including opaque reasoning payloads, are retained in `continuation`.
Configure `OpenAIOptions` explicitly for server storage or reasoning effort.

`LlmAnthropic.config key` selects Messages with its explicit API version header.
Content blocks are accumulated by index, usage counters are cumulative, and
thinking signatures remain in native continuation data. `AnthropicAdaptive` and
`AnthropicOptions` configure thinking. Tool input is parsed only after the message
outcome is known; truncated tool input remains partial data and is not executable.
Adjacent tool results are grouped into one user message when encoded.

Both clients support MIME-tagged inline images and image URLs. Responses retain
HTTP response headers and request IDs. `LlmReplay.fromChunks` runs the same SSE and
provider codecs purely, including arbitrary byte boundaries; `LlmReplay.model`
creates a pure scripted model from a function of the request.

```sh
fai fmt --check -C packages llm
fai check -C packages llm
fai test -C packages llm
```

# Model client and coding assistant examples

Private example/test project · Requires **Fai 0.1.x**

The `llm-examples` package keeps application, terminal, process, and database
dependencies separate from the [model client library](../README.md). It is not
independently published (`publish = false`).

From the repository root, set `OPENAI_API_KEY` or `ANTHROPIC_API_KEY` and choose a
model explicitly:

```sh
fai run -C packages llm/examples/Generate.fai -- openai MODEL "Hello"
fai run -C packages llm/examples/Assistant.fai -- --provider openai MODEL
```

See the [coding assistant guide](../README.md#coding-assistant) for workspace,
database, session, deadline, and headless options. The assistant can write files
and run commands with your OS permissions; it is not a sandbox. Use a disposable
workspace when evaluating unfamiliar models or prompts.

`MockServer.fai`, `ProtocolChecks.fai`, `FailureChecks.fai`, and
`AssistantChecks.fai` support deterministic local-provider fixtures without
external credentials. Run the compiler integration harness as documented in
the parent README. Library tests use `fai test -C packages llm`; example,
replay, and persistence tests use `fai test -C packages llm-examples`.

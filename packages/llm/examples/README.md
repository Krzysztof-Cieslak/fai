# Model client and coding assistant examples

Version **0.1.0** · Requires **Fai 0.1.x** · [Changelog](changelog.md)

The `llm-examples` package keeps application, terminal, process, and database
dependencies separate from the [model client library](../README.md). Its release
tag is `packages/llm/examples/v0.1.0`.

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
the parent README; ordinary pure package tests use `fai test -C packages llm`.

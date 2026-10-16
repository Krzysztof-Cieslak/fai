# Published JSON dependency example

A private runnable project using the published `json` library, not this
repository's workspace overrides or copied library sources. The manifest requests
the `0.1.x` package series; the committed `fai.lock` pins the actual `0.1.0` Git
release and source checksum.

Install Fai 0.1.x and Git, then run from this directory:

```sh
fai resolve --locked
fai check --locked
fai test --locked
fai run --locked
```

Expected output from `run`:

```json
{"message":"Hello, Fai!"}
```

`src/Main.fai` imports `json.Json`, creates a JSON object, and serializes it with
`Json.toString`. Its inline example checks the exact serialization.

With the platform's native linker installed, build and run a standalone program:

```sh
fai build --locked --out hello-json
./hello-json
```

On Windows use `.\hello-json.exe` for the last command. This directory can be
copied outside the repository and run unchanged. It has no path dependencies,
shared workspace membership, or local overrides, and `publish = false` keeps it
out of library releases.

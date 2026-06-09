import * as esbuild from "esbuild";

const production = process.argv.includes("--production");
const watch = process.argv.includes("--watch");
const tests = process.argv.includes("--tests");

// The shipped extension is CommonJS: VS Code cannot load an ESM extension entry
// (microsoft/vscode#130367), so esbuild emits a `.cjs` bundle even though the
// source is authored as ES modules. Only `vscode` is left external (the host
// provides it); everything else, including vscode-languageclient, is inlined.
const extensionConfig = {
  entryPoints: ["src/extension.ts"],
  bundle: true,
  format: "cjs",
  platform: "node",
  target: "node18",
  outfile: "dist/extension.cjs",
  external: ["vscode"],
  minify: production,
  sourcemap: !production,
  sourcesContent: false,
  logLevel: "info",
};

// The grammar drift test runs in plain Node (not in VS Code), so it is built as
// native ESM. vscode-textmate/vscode-oniguruma are bundled in (esbuild handles
// their CommonJS interop); `onig.wasm` is still read from node_modules at
// runtime via createRequire, so it does not need to be bundled.
const testConfig = {
  entryPoints: ["test/grammar.test.ts"],
  bundle: true,
  format: "esm",
  platform: "node",
  target: "node18",
  outfile: "out/grammar.test.mjs",
  sourcemap: true,
  sourcesContent: false,
  logLevel: "info",
};

async function main() {
  const config = tests ? testConfig : extensionConfig;
  if (watch) {
    const ctx = await esbuild.context(config);
    await ctx.watch();
    console.log("[watch] watching for changes...");
  } else {
    await esbuild.build(config);
  }
}

main().catch((error) => {
  console.error(error);
  process.exit(1);
});

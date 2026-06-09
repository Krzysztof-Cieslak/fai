import { test } from "node:test";
import assert from "node:assert/strict";
import * as fs from "node:fs";
import * as path from "node:path";
import { fileURLToPath } from "node:url";
import { createRequire } from "node:module";
import * as oniguruma from "vscode-oniguruma";
import * as vsctm from "vscode-textmate";

// Pin the TextMate grammar against the real `samples/` corpus so it cannot drift
// from the lexer (`fai-syntax` stays the single source of truth): every sample
// must tokenize with no `invalid` scope and no "unscoped" span (a non-whitespace
// token carrying only the root `source.fai` scope means the grammar has a gap).

const here = path.dirname(fileURLToPath(import.meta.url));
const packageRoot = path.resolve(here, "..");
const grammarPath = path.join(packageRoot, "syntaxes", "fai.tmLanguage.json");

function findSamplesDir(start: string): string {
  let dir = start;
  for (;;) {
    const candidate = path.join(dir, "samples");
    if (fs.existsSync(candidate) && fs.statSync(candidate).isDirectory()) {
      return candidate;
    }
    const parent = path.dirname(dir);
    if (parent === dir) {
      throw new Error("could not locate the samples/ directory above " + start);
    }
    dir = parent;
  }
}

const require = createRequire(import.meta.url);
const wasmFile = fs.readFileSync(require.resolve("vscode-oniguruma/release/onig.wasm"));
const wasmBytes = wasmFile.buffer.slice(
  wasmFile.byteOffset,
  wasmFile.byteOffset + wasmFile.byteLength,
);
const onigLib = oniguruma.loadWASM(wasmBytes).then(() => ({
  createOnigScanner: (patterns: string[]) => new oniguruma.OnigScanner(patterns),
  createOnigString: (s: string) => new oniguruma.OnigString(s),
}));

const registry = new vsctm.Registry({
  onigLib,
  loadGrammar: async (scopeName: string) => {
    if (scopeName === "source.fai") {
      return vsctm.parseRawGrammar(fs.readFileSync(grammarPath, "utf8"), grammarPath);
    }
    return null;
  },
});

function check(grammar: vsctm.IGrammar, name: string, text: string): void {
  let ruleStack: vsctm.StateStack = vsctm.INITIAL;
  const lines = text.split(/\r\n|\r|\n/);
  lines.forEach((line, lineIndex) => {
    const result = grammar.tokenizeLine(line, ruleStack);
    for (const token of result.tokens) {
      const fragment = line.slice(token.startIndex, token.endIndex);
      // Whitespace between tokens legitimately carries only the root scope.
      if (/^\s*$/.test(fragment)) {
        continue;
      }
      const scopes = token.scopes;
      const invalid = scopes.find((s) => s === "invalid" || s.startsWith("invalid."));
      assert.ok(
        invalid === undefined,
        `${name}:${lineIndex + 1}: '${fragment}' has invalid scope '${invalid}'`,
      );
      assert.ok(
        scopes.length > 1,
        `${name}:${lineIndex + 1}: '${fragment}' is unscoped (only ${JSON.stringify(scopes)})`,
      );
    }
    ruleStack = result.ruleStack;
  });
}

const grammar = await registry.loadGrammar("source.fai");
if (!grammar) {
  throw new Error("failed to load the source.fai grammar");
}

const samplesDir = findSamplesDir(packageRoot);
const samples = fs
  .readdirSync(samplesDir)
  .filter((f) => f.endsWith(".fai"))
  .sort();

assert.ok(samples.length > 0, "expected at least one sample .fai file");

for (const sample of samples) {
  test(`tokenizes ${sample} with no invalid or unscoped spans`, () => {
    check(grammar, sample, fs.readFileSync(path.join(samplesDir, sample), "utf8"));
  });
}

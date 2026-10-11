#!/usr/bin/env python3
"""Check model protocols, explicit tools, persistence and UI through JIT and AOT."""

import argparse
import json
import os
import queue
from pathlib import Path
import shutil
import subprocess
import sys
import tempfile
import threading

ROOT = Path(__file__).resolve().parents[2]


def run(command, **kwargs):
    result = subprocess.run(command, cwd=ROOT, capture_output=True, timeout=240, **kwargs)
    assert result.returncode == 0, (command, result.returncode, result.stdout, result.stderr)
    return result.stdout


def negative_checks(compiler, directory):
    for package in ("llm", "json", "http-client", "sse"):
        for source in (ROOT / "packages" / package / "src").glob("*.fai"):
            shutil.copy2(source, directory)
    cases = [
        "public bad : Llm.Client -> Llm.Request -> Result Llm.Response Llm.Error\nlet bad client request = Llm.generate client request\n",
        "log : Console -> Llm.Event -> Result LlmTypes.Decision Llm.Error / { Console }\nlet log console event =\n  let sent = console.writeLine \"event\"\n  Ok LlmTypes.Continue\npublic bad : Console -> Llm.Client -> Llm.Request -> Result LlmTypes.StreamResult Llm.Error / { Clock, Concurrency, Net, Tls }\nlet bad console client request = Llm.withStream client request (log console)\n",
        "public bad : LlmTool.Registry { FileSystem } -> LlmTypes.ToolCall -> Result LlmTypes.ToolResult Llm.Error\nlet bad tools call = LlmTool.execute tools call\n",
    ]
    for source in cases:
        source = "module Negative\n// é\n" + source
        path = directory / "Negative.fai"
        path.write_bytes(source.encode("utf-8"))
        result = subprocess.run([compiler, "check", "--no-daemon", "--no-examples", "--message-format=json", "-C", str(directory), str(path)], capture_output=True, timeout=90)
        payload = json.loads(result.stdout)
        errors = [d for d in payload["diagnostics"] if d["code"] == "FAI5001"]
        assert result.returncode != 0 and errors, payload
        assert all(0 <= d["primary"]["byteStart"] < d["primary"]["byteEnd"] <= len(source.encode("utf-8")) for d in errors), payload


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--fai", default="fai")
    args = parser.parse_args()
    candidate = Path(args.fai)
    compiler = str(candidate.resolve()) if candidate.exists() else shutil.which(args.fai)
    if compiler is None:
        parser.error("fai executable not found")
    with tempfile.TemporaryDirectory(prefix="fai-llm-") as temporary:
        directory = Path(temporary)
        cases = [
            ("OpenAICheck", b"OpenAI loopback passed", []),
            ("ProtocolChecks", b"Anthropic tools passed", []),
            ("FailureChecks", b"LLM failure checks passed", []),
            ("AssistantChecks", b"assistant checks passed", [str(directory), sys.executable]),
        ]
        for name, marker, arguments in cases:
            source = f"llm/examples/{name}.fai"
            output = run([compiler, "run", "--no-daemon", "-C", "packages", source, "--", *arguments])
            assert marker in output and b"FAILED" not in output, output
            binary = directory / f"{name}.exe"
            run([compiler, "build", "--no-daemon", "-C", "packages", source, "--out", str(binary)])
            output = run([str(binary), *arguments])
            assert marker in output and b"FAILED" not in output, output
        negative = directory / "negative"
        negative.mkdir()
        negative_checks(compiler, negative)
        assistant = directory / "assistant.exe"
        server = directory / "server.exe"
        run([compiler, "build", "--no-daemon", "-C", "packages", "llm/examples/Assistant.fai", "--out", str(assistant)])
        run([compiler, "build", "--no-daemon", "-C", "packages", "llm/examples/MockServer.fai", "--out", str(server)])
        assert b"MODEL" in run([str(assistant), "--help"])
        mock = subprocess.Popen([str(server)], cwd=ROOT, stdout=subprocess.PIPE, stderr=subprocess.PIPE)
        try:
            opened = queue.Queue()
            threading.Thread(target=lambda: opened.put(mock.stdout.readline()), daemon=True).start()
            port = int(opened.get(timeout=30).strip())
            base = f"http://127.0.0.1:{port}/"
            common = [str(assistant), "fixture", "--base-url", base, "--database", str(directory / "history.sqlite"), "--workspace", str(directory)]
            assert b"tools completed" in run([*common, "--prompt", "read files"])
            assert b"tools completed" in run([*common, "--prompt", "continue"])
            if os.name != "nt":
                from tui_terminal import exercise
                exercise([*common, "--session", "ui"], b"Fai coding assistant", b"read files\r", b"\x03", expected=b"completed", finish_when=b"completed")
        finally:
            mock.terminate()
            try:
                mock.communicate(timeout=10)
            except subprocess.TimeoutExpired:
                mock.kill()
                mock.communicate(timeout=10)
    print("LLM JIT/AOT, tools, persistence, effects and UI checks passed")


if __name__ == "__main__":
    main()

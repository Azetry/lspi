#!/usr/bin/env python3
"""Run cold workspace-symbol checks against the five supported LSP servers."""

from __future__ import annotations

import argparse
import json
from pathlib import Path
import queue
import subprocess
import tempfile
import threading
from typing import Any, TextIO


CASES = (
    ("python", "canary.py", "SpaceForcePythonCanary"),
    ("typescript", "canary.ts", "SpaceForceTypeScriptCanary"),
    ("go", "canary.go", "SpaceForceGoCanary"),
    ("php", "Canary.php", "SpaceForcePhpCanary"),
    ("rust", "src/lib.rs", "SpaceForceRustCanary"),
)


def write_fixture(root: Path) -> Path:
    files = {
        "canary.py": "class SpaceForcePythonCanary:\n    pass\n",
        "canary.ts": "export class SpaceForceTypeScriptCanary {}\n",
        "tsconfig.json": json.dumps({"compilerOptions": {"strict": True}}),
        "canary.go": "package canary\ntype SpaceForceGoCanary struct{}\n",
        "go.mod": "module example.com/space-force-canary\n\ngo 1.26\n",
        "Canary.php": "<?php\nclass SpaceForcePhpCanary {}\n",
        "composer.json": json.dumps({"name": "space-force/lspi-canary"}),
        "Cargo.toml": (
            '[package]\nname = "space-force-lspi-canary"\n'
            'version = "0.1.0"\nedition = "2024"\n'
        ),
        "src/lib.rs": "pub struct SpaceForceRustCanary;\n",
    }
    for relative, content in files.items():
        target = root / relative
        target.parent.mkdir(parents=True, exist_ok=True)
        target.write_text(content, encoding="utf-8")

    config = root / "lspi.toml"
    config.write_text(
        """
[[servers]]
id = "python"
kind = "pyright"
extensions = ["py", "pyi"]
language_id = "python"
command = "pyright-langserver"
args = ["--stdio"]

[[servers]]
id = "typescript"
kind = "generic"
extensions = ["ts", "tsx", "js", "jsx", "mjs", "cjs"]
language_id = "typescript"
command = "typescript-language-server"
args = ["--stdio"]
adapter = "tsserver"
initialize_options = { tsserver = { path = "tsserver" } }

[[servers]]
id = "go"
kind = "generic"
extensions = ["go"]
language_id = "go"
command = "gopls"
args = ["serve"]

[[servers]]
id = "php"
kind = "generic"
extensions = ["php"]
language_id = "php"
command = "intelephense"
args = ["--stdio"]

[[servers]]
id = "rust"
kind = "rust_analyzer"
extensions = ["rs"]
language_id = "rust"
command = "rust-analyzer"
args = []

[mcp]
read_only = true
""".lstrip(),
        encoding="utf-8",
    )
    return config


def send(stream: TextIO, payload: dict[str, Any]) -> None:
    stream.write(json.dumps(payload, separators=(",", ":")) + "\n")
    stream.flush()


def receive(
    responses: queue.Queue[dict[str, Any] | BaseException], request_id: int
) -> dict[str, Any]:
    while True:
        try:
            payload = responses.get(timeout=60)
        except queue.Empty as error:
            raise RuntimeError(f"timed out waiting for lspi response {request_id}") from error
        if isinstance(payload, BaseException):
            raise RuntimeError(f"lspi closed before response {request_id}") from payload
        if payload.get("id") == request_id:
            return payload


def read_responses(
    stream: TextIO, responses: queue.Queue[dict[str, Any] | BaseException]
) -> None:
    try:
        for line in stream:
            responses.put(json.loads(line))
        responses.put(EOFError("lspi stdout closed"))
    except BaseException as error:
        responses.put(error)


def run_canary(lspi: Path) -> None:
    with tempfile.TemporaryDirectory(prefix="lspi-semantic-canary-") as directory:
        root = Path(directory)
        config = write_fixture(root)
        process = subprocess.Popen(
            [
                str(lspi),
                "mcp",
                "--config",
                str(config),
                "--workspace-root",
                str(root),
                "--context",
                "codex",
            ],
            stdin=subprocess.PIPE,
            stdout=subprocess.PIPE,
            text=True,
        )
        assert process.stdin is not None
        assert process.stdout is not None
        responses: queue.Queue[dict[str, Any] | BaseException] = queue.Queue()
        reader = threading.Thread(
            target=read_responses, args=(process.stdout, responses), daemon=True
        )
        reader.start()
        try:
            send(
                process.stdin,
                {
                    "jsonrpc": "2.0",
                    "id": 1,
                    "method": "initialize",
                    "params": {
                        "protocolVersion": "2025-06-18",
                        "capabilities": {},
                        "clientInfo": {"name": "lspi-semantic-canary", "version": "1"},
                    },
                },
            )
            initialized = receive(responses, 1)
            if "result" not in initialized:
                raise RuntimeError(f"MCP initialize failed: {initialized}")
            send(
                process.stdin,
                {
                    "jsonrpc": "2.0",
                    "method": "notifications/initialized",
                    "params": {},
                },
            )

            for request_id, (server_id, relative, symbol) in enumerate(CASES, start=2):
                send(
                    process.stdin,
                    {
                        "jsonrpc": "2.0",
                        "id": request_id,
                        "method": "tools/call",
                        "params": {
                            "name": "search_workspace_symbols",
                            "arguments": {
                                "query": symbol,
                                "file_path": str(root / relative),
                                "max_results": 20,
                            },
                        },
                    },
                )
                response = receive(responses, request_id)
                result = response.get("result", {})
                structured = result.get("structuredContent", {})
                names = [item.get("name") for item in structured.get("matches", [])]
                if result.get("isError") or structured.get("ok") is not True:
                    raise RuntimeError(f"{server_id} cold lookup failed: {response}")
                if symbol not in names:
                    raise RuntimeError(
                        f"{server_id} did not find {symbol}; names={names} response={response}"
                    )
                print(f"OK {server_id}: {symbol}")
        finally:
            process.terminate()
            try:
                process.wait(timeout=5)
            except subprocess.TimeoutExpired:
                process.kill()
                process.wait(timeout=5)


def main() -> int:
    parser = argparse.ArgumentParser()
    parser.add_argument("--lspi", type=Path, required=True)
    args = parser.parse_args()
    run_canary(args.lspi.resolve())
    return 0


if __name__ == "__main__":
    raise SystemExit(main())

#!/usr/bin/env python3
import json
import os
import sys


trace_path = os.environ["LSPI_TEST_TRACE"]
opened = False
workspace_calls = 0


def trace(method):
    with open(trace_path, "a", encoding="utf-8") as trace_file:
        trace_file.write(method + "\n")


def read_message():
    content_length = None
    while True:
        line = sys.stdin.buffer.readline()
        if not line:
            return None
        if line in (b"\r\n", b"\n"):
            break
        name, _, value = line.decode("ascii").partition(":")
        if name.lower() == "content-length":
            content_length = int(value.strip())
    if content_length is None:
        raise RuntimeError("missing Content-Length")
    return json.loads(sys.stdin.buffer.read(content_length))


def write_message(payload):
    body = json.dumps(payload, separators=(",", ":")).encode("utf-8")
    sys.stdout.buffer.write(f"Content-Length: {len(body)}\r\n\r\n".encode("ascii"))
    sys.stdout.buffer.write(body)
    sys.stdout.buffer.flush()


while True:
    message = read_message()
    if message is None:
        break
    method = message.get("method")
    if method:
        trace(method)
    request_id = message.get("id")

    if method == "initialize":
        write_message({"jsonrpc": "2.0", "id": request_id, "result": {}})
    elif method == "textDocument/didOpen":
        opened = True
    elif method == "workspace/symbol":
        workspace_calls += 1
        query = message["params"]["query"]
        if not opened or (query == "TargetSymbol" and workspace_calls == 1):
            write_message(
                {
                    "jsonrpc": "2.0",
                    "id": request_id,
                    "error": {"code": -32603, "message": "No Project"},
                }
            )
        elif query == "TargetSymbol" and workspace_calls == 2:
            write_message({"jsonrpc": "2.0", "id": request_id, "result": []})
        elif query == "TargetSymbol":
            write_message(
                {
                    "jsonrpc": "2.0",
                    "id": request_id,
                    "result": [
                        {
                            "name": "TargetSymbol",
                            "kind": 12,
                            "location": {
                                "uri": "file:///workspace/sample.ts",
                                "range": {
                                    "start": {"line": 0, "character": 0},
                                    "end": {"line": 0, "character": 12},
                                },
                            },
                        }
                    ],
                }
            )
        elif query == "AlwaysError":
            write_message(
                {
                    "jsonrpc": "2.0",
                    "id": request_id,
                    "error": {"code": -32603, "message": "still indexing"},
                }
            )
        else:
            write_message({"jsonrpc": "2.0", "id": request_id, "result": []})
    elif method == "shutdown":
        write_message({"jsonrpc": "2.0", "id": request_id, "result": None})
    elif request_id is not None:
        write_message({"jsonrpc": "2.0", "id": request_id, "result": None})

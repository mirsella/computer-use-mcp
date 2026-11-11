#!/usr/bin/env python3
"""Transparent stdio measurement shim. Never changes a native operation/result."""

import argparse
import base64
import json
import os
import selectors
import resource
import subprocess
import sys
import time
from pathlib import Path


def main():
    parser = argparse.ArgumentParser()
    parser.add_argument("--journal", type=Path, required=True)
    parser.add_argument("command", nargs=argparse.REMAINDER)
    args = parser.parse_args()
    command = args.command[1:] if args.command[:1] == ["--"] else args.command
    pending = {}
    buffers = {"request": b"", "response": b""}
    with (
        args.journal.open("a", buffering=1) as journal,
        args.journal.with_suffix(".stderr").open("wb") as stderr,
    ):
        child = subprocess.Popen(
            command, stdin=subprocess.PIPE, stdout=subprocess.PIPE, stderr=stderr
        )
        child_started = time.monotonic()
        selector = selectors.DefaultSelector()
        selector.register(sys.stdin.buffer, selectors.EVENT_READ, "request")
        selector.register(child.stdout, selectors.EVENT_READ, "response")
        try:
            while selector.get_map():
                for key, _ in selector.select():
                    direction = key.data
                    chunk = os.read(key.fd, 65536)
                    if not chunk:
                        selector.unregister(key.fileobj)
                        if direction == "request":
                            child.stdin.close()
                        continue
                    destination = (
                        child.stdin if direction == "request" else sys.stdout.buffer
                    )
                    destination.write(chunk)
                    destination.flush()
                    buffers[direction] += chunk
                    while b"\n" in buffers[direction]:
                        line, buffers[direction] = buffers[direction].split(b"\n", 1)
                        message = json.loads(line)
                        now = time.monotonic()
                        if direction == "request" and "id" in message:
                            pending[message["id"]] = (now, message, len(line))
                        elif direction == "response" and "id" in message:
                            started, request, request_bytes = pending.pop(message["id"])
                            result = message.get("result", {})
                            content = (
                                result.get("content", [])
                                if isinstance(result, dict)
                                else []
                            )
                            row = {
                                "method": request["method"],
                                "name": request.get("params", {}).get("name"),
                                "arguments": request.get("params", {}).get("arguments"),
                                "seconds": now - started,
                                "started_monotonic": started,
                                "ended_monotonic": now,
                                "request_bytes": request_bytes,
                                "response_bytes": len(line),
                                "error": bool(
                                    message.get("error") or result.get("isError")
                                ),
                                "text_bytes": sum(
                                    len(item.get("text", "").encode())
                                    for item in content
                                ),
                                "image_bytes": sum(
                                    len(base64.b64decode(item["data"], validate=True))
                                    for item in content
                                    if item.get("type") == "image"
                                ),
                            }
                            journal.write(json.dumps(row, ensure_ascii=False) + "\n")
        finally:
            selector.close()
            if child.stdin and not child.stdin.closed:
                child.stdin.close()
            try:
                child.wait(timeout=20)
            except subprocess.TimeoutExpired:
                child.terminate()
                try:
                    child.wait(timeout=10)
                except subprocess.TimeoutExpired:
                    child.kill()
                    child.wait()
            usage = resource.getrusage(resource.RUSAGE_CHILDREN)
            args.journal.with_suffix(".usage.json").write_text(
                json.dumps(
                    {
                        "method": "measurement/child_usage",
                        "user_seconds": usage.ru_utime,
                        "system_seconds": usage.ru_stime,
                        "max_rss_kib": usage.ru_maxrss,
                        "elapsed_seconds": time.monotonic() - child_started,
                    }
                )
                + "\n"
            )


if __name__ == "__main__":
    main()

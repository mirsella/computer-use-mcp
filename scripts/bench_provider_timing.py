"""Local byte-transparent HTTP stream timing. Never journals headers or bodies."""

import asyncio
from collections import Counter
import json
import threading
import time
from urllib.parse import urlsplit
import zlib

import aiohttp
from aiohttp import web

HOP = {
    "connection",
    "keep-alive",
    "proxy-authenticate",
    "proxy-authorization",
    "te",
    "trailer",
    "transfer-encoding",
    "upgrade",
    "host",
    "content-length",
}


class StreamEvidence:
    def __init__(self, started, encoding=""):
        self.started = started
        self.events = Counter()
        self.first_sse = self.first_output = self.first_reasoning = None
        self.delta_bytes = 0
        self.usage = {}
        self.error = None
        self.done = self.finished = False
        self.buffer = b""
        self.decoder = (
            zlib.decompressobj(16 + zlib.MAX_WBITS) if encoding == "gzip" else None
        )
        if encoding not in ("", "identity", "gzip"):
            self.error = "unsupported_content_encoding"

    def feed(self, data, now):
        if self.error:
            return
        try:
            self.buffer += self.decoder.decompress(data) if self.decoder else data
            if len(self.buffer) > 2 * 1024 * 1024:
                self.error = "sse_line_budget"
                return
            while b"\n" in self.buffer:
                line, self.buffer = self.buffer.split(b"\n", 1)
                if not line.startswith(b"data:"):
                    continue
                value = line[5:].strip()
                if value == b"[DONE]":
                    self.done = True
                    continue
                event = json.loads(value)
                choices = event.get("choices")
                kind = event.get(
                    "type", "chat.chunk" if isinstance(choices, list) else "unknown"
                )
                self.events[kind] += 1
                if self.first_sse is None:
                    self.first_sse = now - self.started
                if "reasoning" in kind and kind.endswith(".delta"):
                    if self.first_reasoning is None:
                        self.first_reasoning = now - self.started
                elif kind.endswith(".delta"):
                    if self.first_output is None:
                        self.first_output = now - self.started
                delta = event.get("delta")
                if isinstance(delta, str):
                    self.delta_bytes += len(delta.encode())
                if kind in ("response.completed", "response.failed"):
                    self.finished = True
                for choice in choices or []:
                    if choice.get("finish_reason"):
                        self.finished = True
                    delta = choice.get("delta") or {}
                    reasoning = delta.get("reasoning_content") or delta.get("reasoning")
                    if isinstance(reasoning, str) and reasoning:
                        self.first_reasoning = (
                            self.first_reasoning
                            if self.first_reasoning is not None
                            else now - self.started
                        )
                        self.delta_bytes += len(reasoning.encode())
                    outputs = [delta.get("content")]
                    for call in delta.get("tool_calls") or []:
                        function = call.get("function") or {}
                        outputs.extend(
                            [function.get("name"), function.get("arguments")]
                        )
                    for output in outputs:
                        if isinstance(output, str) and output:
                            self.first_output = (
                                self.first_output
                                if self.first_output is not None
                                else now - self.started
                            )
                            self.delta_bytes += len(output.encode())
                usage = event.get("response", {}).get("usage") or event.get("usage")
                if isinstance(usage, dict):
                    self.usage = {
                        key: value
                        for key, value in usage.items()
                        if key
                        in (
                            "input_tokens",
                            "output_tokens",
                            "total_tokens",
                            "prompt_tokens",
                            "completion_tokens",
                        )
                        and isinstance(value, int)
                        and not isinstance(value, bool)
                    }
        except (ValueError, zlib.error, AttributeError):
            # Observation failed, but the actual response remains byte-transparent.
            self.error = "sse_parse_failed"

    def summary(self):
        return dict(
            first_sse_seconds=self.first_sse,
            first_output_delta_seconds=self.first_output,
            first_reasoning_delta_seconds=self.first_reasoning,
            delta_utf8_bytes=self.delta_bytes,
            event_counts=dict(self.events),
            usage=self.usage,
            stream_inspection_error=self.error,
            finish_event_seen=self.finished,
            done_marker_seen=self.done,
        )


class ProviderTimingProxy:
    def __init__(self, upstream, journal):
        parsed = urlsplit(upstream)
        if parsed.scheme not in ("http", "https") or not parsed.netloc:
            raise ValueError("Provider timing requires an explicit HTTP(S) base URL")
        self.upstream = upstream.rstrip("/")
        self.journal = journal
        self.ready = threading.Event()
        self.thread = threading.Thread(target=self._thread, name="provider-timing")
        self.failure = None
        self.url = None
        self.counter = 0

    def __enter__(self):
        self.thread.start()
        if not self.ready.wait(10):
            raise RuntimeError("Provider timing proxy startup deadline")
        if self.failure:
            raise RuntimeError(
                "Provider timing proxy failed to start"
            ) from self.failure
        return self

    def __exit__(self, *_):
        self.loop.call_soon_threadsafe(self.stop.set)
        self.thread.join(20)
        if self.thread.is_alive():
            raise RuntimeError("Provider timing proxy failed to shut down")
        if self.failure:
            raise RuntimeError("Provider timing proxy failed") from self.failure

    def _thread(self):
        try:
            asyncio.run(self._serve())
        except BaseException as error:
            self.failure = error
            self.ready.set()

    async def _serve(self):
        self.loop = asyncio.get_running_loop()
        self.stop = asyncio.Event()
        timeout = aiohttp.ClientTimeout(total=None, sock_connect=15, sock_read=180)
        async with aiohttp.ClientSession(
            timeout=timeout, auto_decompress=False
        ) as session:
            self.session = session
            application = web.Application(client_max_size=32 * 1024 * 1024)
            application.router.add_route("*", "/{tail:.*}", self._forward)
            runner = web.AppRunner(application, access_log=None, shutdown_timeout=3)
            await runner.setup()
            site = web.TCPSite(runner, "127.0.0.1", 0)
            await site.start()
            self.url = f"http://127.0.0.1:{site._server.sockets[0].getsockname()[1]}"
            self.ready.set()
            try:
                await self.stop.wait()
            finally:
                await runner.cleanup()

    async def _forward(self, request):
        started = time.monotonic()
        self.counter += 1
        row = dict(
            request_index=self.counter,
            started_monotonic=started,
            started_wall=time.time(),
            status=None,
            response_bytes=0,
            body_chunks=0,
            completed=False,
            transport_error=None,
            downstream_disconnected=False,
        )
        inspection = StreamEvidence(started)
        response = None
        try:
            body = await request.read()
            row["request_bytes"] = len(body)
            headers = {k: v for k, v in request.headers.items() if k.lower() not in HOP}
            async with self.session.request(
                request.method,
                self.upstream + request.raw_path,
                data=body,
                headers=headers,
                allow_redirects=False,
                skip_auto_headers={"Accept-Encoding"},
            ) as upstream:
                row.update(
                    status=upstream.status, headers_seconds=time.monotonic() - started
                )
                row["event_stream"] = "text/event-stream" in upstream.headers.get(
                    "Content-Type", ""
                )
                if row["event_stream"]:
                    inspection = StreamEvidence(
                        started, upstream.headers.get("Content-Encoding", "")
                    )
                response = web.StreamResponse(
                    status=upstream.status,
                    headers={
                        k: v
                        for k, v in upstream.headers.items()
                        if k.lower() not in HOP
                    },
                )
                await response.prepare(request)
                async for chunk in upstream.content.iter_any():
                    now = time.monotonic()
                    if "first_body_seconds" not in row:
                        row["first_body_seconds"] = now - started
                    row["response_bytes"] += len(chunk)
                    row["body_chunks"] += 1
                    if "text/event-stream" in upstream.headers.get("Content-Type", ""):
                        inspection.feed(chunk, now)
                    await response.write(chunk)
                await response.write_eof()
                row["completed"] = True
                return response
        except (
            aiohttp.ClientError,
            ConnectionError,
            asyncio.TimeoutError,
            asyncio.CancelledError,
        ) as error:
            if (
                isinstance(
                    error, (aiohttp.ClientConnectionResetError, ConnectionResetError)
                )
                and response is not None
            ):
                row["downstream_disconnected"] = True
            else:
                row["transport_error"] = type(error).__name__
            if response is None:
                return web.Response(
                    status=502, text="Provider timing proxy forwarding failed"
                )
            return response
        finally:
            row.update(
                ended_monotonic=time.monotonic(), seconds=time.monotonic() - started
            )
            row.update(inspection.summary())
            with self.journal.open("a") as stream:
                stream.write(json.dumps(row) + "\n")

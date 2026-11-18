"""graphrt plugin SDK for Python (standard library only).

Write node kinds and tools for graphrt in Python::

    from graphrt_plugin import Plugin, PluginError, pause

    plugin = Plugin("acme")

    @plugin.node("acme.sentiment")
    async def sentiment(ctx, req):
        text = req.config.get("text", "")
        await ctx.chunk("explanation", "looking for positive words...")
        return {"outputs": {"label": "positive" if "love" in text else "neutral"}}

    @plugin.tool("score", description="Score a text", parameters={"type": "object"})
    async def score(args):
        return {"text": "0.9"}

    @plugin.tool("screenshot", description="Capture the screen")
    async def screenshot(args):
        return {"text": "1280x800", "images": [{"media_type": "image/png", "data": png_bytes}]}

    if __name__ == "__main__":
        plugin.run()

The plugin talks newline-delimited JSON-RPC 2.0 over stdin/stdout. Never
``print()`` to stdout: use ``ctx.log(...)`` or write to stderr instead.
"""

from __future__ import annotations

import asyncio
import base64
import binascii
import inspect
import json
import re
import sys
import threading
import traceback
from dataclasses import dataclass, field
from typing import Any, Awaitable, Callable, Dict, List, Optional, Union

__all__ = [
    "Plugin",
    "PluginError",
    "RunContext",
    "RunRequest",
    "Resume",
    "pause",
    "tool_result",
    "PROTOCOL_VERSION",
    "TOOL_IMAGES_FEATURE",
    "MAX_TOOL_IMAGES",
    "MAX_TOOL_IMAGE_BYTES",
    "TOOL_IMAGE_MEDIA_TYPES",
]

PROTOCOL_VERSION = 1
# Default line limit; the host announces its own in `initialize` (`max_message_bytes`).
MAX_MESSAGE_BYTES = 16 * 1024 * 1024
# Feature a host lists in `initialize` when tool results may carry `images`.
TOOL_IMAGES_FEATURE = "tool_images"
# Tool result image limits (the host enforces the same).
MAX_TOOL_IMAGES = 4
MAX_TOOL_IMAGE_BYTES = 5 * 1024 * 1024
TOOL_IMAGE_MEDIA_TYPES = ("image/png", "image/jpeg", "image/webp", "image/gif")

_ID_RE = re.compile(rb'"id"\s*:\s*("(?:[^"\\]|\\.)*"|-?\d+)')


def _recover_id(prefix: bytes) -> Any:
    """Best-effort request id from the start of a line that could not be read."""
    match = _ID_RE.search(prefix[:4096])
    if not match:
        return None
    try:
        return json.loads(match.group(1))
    except ValueError:
        return None


class PluginError(Exception):
    """An error reported to the host as a failed node.

    ``kind`` becomes the node error type; ``retryable`` lets the host retry.
    """

    def __init__(self, message: str, kind: str = "plugin_error", retryable: bool = False,
                 retry_after_ms: Optional[int] = None) -> None:
        super().__init__(message)
        self.message = message
        self.kind = kind
        self.retryable = retryable
        self.retry_after_ms = retry_after_ms


@dataclass
class Resume:
    """Resume data for a node that paused: its saved ``state`` and the host ``input``."""

    state: Any
    input: Any


@dataclass
class RunRequest:
    """A ``node.run`` request. ``config`` has variables already resolved by the host."""

    request_id: str
    kind: str
    node_id: str
    attempt: int
    config: Dict[str, Any]
    resume: Optional[Resume] = None


def pause(kind: str, data: Any = None, state: Any = None) -> Dict[str, Any]:
    """Build a result that pauses the run (e.g. waiting for approval)."""
    return {"pause": {"kind": kind, "data": data, "state": state}}


class RunContext:
    """Per-request helpers: streaming chunks, progress, logs and cancellation."""

    def __init__(self, plugin: "Plugin", request_id: str) -> None:
        self._plugin = plugin
        self.request_id = request_id
        self._cancelled = asyncio.Event()

    async def chunk(self, field: str, text: str) -> None:
        """Stream text for output ``field``."""
        self._plugin._notify("node.chunk", {"request_id": self.request_id, "field": field, "chunk": text})

    async def progress(self, name: str, data: Any = None) -> None:
        """Report structured progress."""
        self._plugin._notify("node.progress", {"request_id": self.request_id, "name": name, "data": data})

    def log(self, message: str, level: str = "info") -> None:
        """Send a log line to the host."""
        self._plugin._notify("log", {"level": level, "message": message})

    @property
    def cancelled(self) -> bool:
        """Whether the host cancelled this request."""
        return self._cancelled.is_set()

    async def wait_cancelled(self) -> None:
        """Wait until the host cancels this request."""
        await self._cancelled.wait()


def _encode_image(image: Any, index: int) -> Dict[str, str]:
    """One ``images`` entry as sent on the wire: ``{media_type, data_base64}`` or ``{url}``."""

    def fail(message: str) -> PluginError:
        return PluginError(f"image #{index}: {message}", kind="invalid_output")

    if not isinstance(image, dict):
        raise fail("must be a dict")
    if "url" in image:
        url = image["url"]
        if not isinstance(url, str) or not url.lower().startswith("https://"):
            raise fail("`url` must start with https://")
        return {"url": url}
    media_type = image.get("media_type")
    if media_type not in TOOL_IMAGE_MEDIA_TYPES:
        raise fail(f"media type `{media_type}` is not supported (use {', '.join(TOOL_IMAGE_MEDIA_TYPES)})")
    raw = image.get("data")
    if isinstance(raw, (bytes, bytearray, memoryview)):
        size = len(raw)
        encoded = base64.b64encode(bytes(raw)).decode("ascii")
    elif isinstance(image.get("data_base64"), str):
        encoded = image["data_base64"]
        try:
            size = len(base64.b64decode(encoded, validate=True))
        except (binascii.Error, ValueError):
            raise fail("`data_base64` is not valid base64") from None
    else:
        raise fail("give `data` (bytes) or `data_base64` (a base64 string)")
    if size == 0:
        raise fail("image data is empty")
    if size > MAX_TOOL_IMAGE_BYTES:
        raise fail(f"image is {size} bytes, above the {MAX_TOOL_IMAGE_BYTES}-byte limit")
    return {"media_type": media_type, "data_base64": encoded}


def tool_result(result: Any, host_supports_images: bool = True) -> Dict[str, Any]:
    """Normalize a tool handler's return value into a ``tool.call`` result.

    Images are validated and encoded; for hosts without image support they
    are dropped and the text says so, so the call still succeeds.
    """
    if isinstance(result, str):
        result = {"text": result}
    out: Dict[str, Any] = {"text": "", "is_error": False, **(result or {})}
    images = out.pop("images", None)
    if images is None:
        return out
    if not isinstance(images, (list, tuple)):
        raise PluginError("`images` must be a list", kind="invalid_output")
    if len(images) > MAX_TOOL_IMAGES:
        raise PluginError(f"{len(images)} images in one tool result, above the limit of {MAX_TOOL_IMAGES}",
                          kind="invalid_output")
    if not images:
        return out
    if not host_supports_images:
        note = f"[{len(images)} image(s) omitted: the graphrt host does not support image tool results]"
        out["text"] = f"{out['text']}\n{note}" if out["text"] else note
        return out
    out["images"] = [_encode_image(image, i) for i, image in enumerate(images)]
    return out


NodeHandler = Callable[[RunContext, RunRequest], Awaitable[Any]]
ToolHandler = Callable[[Dict[str, Any]], Awaitable[Any]]


@dataclass
class _Tool:
    spec: Dict[str, Any]
    handler: Callable[..., Any]


@dataclass
class Plugin:
    """A set of node kinds and tools served over the graphrt plugin protocol."""

    name: str
    _nodes: Dict[str, Callable[..., Any]] = field(default_factory=dict)
    _descriptors: List[Dict[str, Any]] = field(default_factory=list)
    _tools: Dict[str, _Tool] = field(default_factory=dict)

    def node(self, kind: str, descriptor: Optional[Dict[str, Any]] = None) -> Callable[[NodeHandler], NodeHandler]:
        """Decorator registering a node handler ``async def handler(ctx, req)``.

        The handler returns ``{"outputs": {...}, "branch": ..., "writes": [...],
        "usage": {...}, "metadata": {...}}`` (only ``outputs`` is required), a plain
        dict (taken as the outputs), or ``pause(...)``. Raise ``PluginError`` to fail.
        """

        def register(fn: NodeHandler) -> NodeHandler:
            self._nodes[kind] = fn
            if descriptor is not None:
                self._descriptors.append({"kind": kind, **descriptor})
            return fn

        return register

    def tool(self, name: str, description: str = "", parameters: Optional[Dict[str, Any]] = None
             ) -> Callable[[ToolHandler], ToolHandler]:
        """Decorator registering a tool ``async def handler(arguments)``.

        Return a string, or ``{"text": ..., "data": ..., "is_error": bool, "images": [...]}``.
        ``images`` entries are ``{"media_type": "image/png", "data": b"..."}``
        (or ``"data_base64"``) or ``{"url": "https://..."}``: at most 4,
        png/jpeg/webp/gif, 5 MiB each.
        """

        def register(fn: ToolHandler) -> ToolHandler:
            spec = {"name": name, "description": description, "parameters": parameters or {"type": "object"}}
            self._tools[name] = _Tool(spec, fn)
            return fn

        return register

    # ---- runtime ---------------------------------------------------------

    def run(self) -> None:
        """Serve on stdin/stdout until stdin closes or the host sends ``shutdown``."""
        try:
            asyncio.run(self._serve())
        except KeyboardInterrupt:
            pass

    async def _serve(self) -> None:
        self._loop = asyncio.get_running_loop()
        self._out_lock = threading.Lock()
        self._running: Dict[str, "asyncio.Task[None]"] = {}
        self._contexts: Dict[str, RunContext] = {}
        self._tool_tasks: "set[asyncio.Task[None]]" = set()
        queue: "asyncio.Queue[Optional[tuple]]" = asyncio.Queue()
        limit = [MAX_MESSAGE_BYTES]
        self._limit = limit
        self._host_features: List[str] = []

        def reader() -> None:
            stdin = sys.stdin.buffer
            while True:
                line = stdin.readline(limit[0] + 1)
                if not line:
                    self._loop.call_soon_threadsafe(queue.put_nowait, None)
                    return
                if len(line) > limit[0] and not line.endswith(b"\n"):
                    # Oversized: drop the rest of the line so the stream stays in sync.
                    rest = line
                    while rest and not rest.endswith(b"\n"):
                        rest = stdin.readline(1 << 20)
                    self._loop.call_soon_threadsafe(queue.put_nowait, ("oversize", line[:4096]))
                    continue
                if b'"initialize"' in line[:4096]:
                    # Adopt the host's message limit before reading the next line.
                    try:
                        announced = json.loads(line).get("params", {}).get("max_message_bytes")
                        if isinstance(announced, int) and announced >= 1024:
                            limit[0] = announced
                    except (ValueError, AttributeError):
                        pass
                self._loop.call_soon_threadsafe(queue.put_nowait, ("line", line))

        threading.Thread(target=reader, name="graphrt-plugin-stdin", daemon=True).start()
        while True:
            item = await queue.get()
            if item is None:
                break
            kind, line = item
            if kind == "oversize":
                msg_id = _recover_id(line)
                sys.stderr.write(f"graphrt plugin {self.name}: dropped a message above {limit[0]} bytes\n")
                self._error(msg_id, -32600, f"message exceeds the {limit[0]}-byte limit", {"kind": "message_too_large"})
                continue
            try:
                text = line.decode("utf-8").strip()
                msg = json.loads(text) if text else None
            except ValueError:
                sys.stderr.write(f"graphrt plugin {self.name}: unreadable message\n")
                self._error(_recover_id(line), -32700, "parse error", {"kind": "plugin_protocol"})
                continue
            if msg is None:
                continue
            if not isinstance(msg, dict):
                self._error(None, -32600, "message must be a JSON object", {"kind": "plugin_protocol"})
                continue
            if not self._dispatch(msg):
                # `shutdown`: stop in-flight work.
                for task in list(self._running.values()):
                    task.cancel()
                return
        # End of input: let in-flight requests finish and reply.
        pending = [t for t in list(self._running.values()) + list(self._tool_tasks) if not t.done()]
        if pending:
            await asyncio.gather(*pending, return_exceptions=True)

    def _dispatch(self, msg: Dict[str, Any]) -> bool:
        method = msg.get("method")
        params = msg.get("params") or {}
        msg_id = msg.get("id")
        if method == "shutdown":
            return False
        if msg_id is None:
            if method == "node.cancel":
                rid = params.get("request_id")
                ctx = self._contexts.get(rid)
                if ctx is not None:
                    ctx._cancelled.set()
                task = self._running.pop(rid, None)
                if task is not None:
                    task.cancel()
            return True
        if method == "initialize":
            features = params.get("features")
            self._host_features = [f for f in features if isinstance(f, str)] if isinstance(features, list) else []
            result: Dict[str, Any] = {"protocol_version": PROTOCOL_VERSION, "name": self.name}
            if self._descriptors:
                result["nodes"] = self._descriptors
            if self._tools:
                result["tools"] = [t.spec for t in self._tools.values()]
            self._reply(msg_id, result)
        elif method == "node.run":
            self._start_run(msg_id, params)
        elif method == "tool.list":
            self._reply(msg_id, {"tools": [t.spec for t in self._tools.values()]})
        elif method == "tool.call":
            task = asyncio.ensure_future(self._call_tool(msg_id, params))
            self._tool_tasks.add(task)
            task.add_done_callback(self._tool_tasks.discard)
        else:
            self._error(msg_id, -32601, f"method `{method}` not found")
        return True

    def _start_run(self, msg_id: Any, params: Dict[str, Any]) -> None:
        kind = params.get("kind", "")
        handler = self._nodes.get(kind)
        if handler is None:
            self._error(msg_id, -32602, f"plugin `{self.name}` has no node kind `{kind}`", {"kind": "unknown_kind"})
            return
        rid = str(params.get("request_id") or msg_id)
        raw_resume = params.get("resume")
        resume = Resume(raw_resume.get("state"), raw_resume.get("input")) if isinstance(raw_resume, dict) else None
        req = RunRequest(
            request_id=rid,
            kind=kind,
            node_id=params.get("node_id", ""),
            attempt=int(params.get("attempt", 1)),
            config=params.get("config") or {},
            resume=resume,
        )
        ctx = RunContext(self, rid)
        self._contexts[rid] = ctx

        async def runner() -> None:
            try:
                result = handler(ctx, req)
                if inspect.isawaitable(result):
                    result = await result
                self._reply(msg_id, _normalize_result(result))
            except asyncio.CancelledError:
                return
            except PluginError as e:
                self._error(msg_id, -32000, e.message, _error_data(e))
            except Exception as e:  # noqa: BLE001 - every failure must reach the host
                traceback.print_exc(file=sys.stderr)
                self._error(msg_id, -32000, f"{type(e).__name__}: {e}", {"kind": "plugin_exception"})
            finally:
                self._running.pop(rid, None)
                self._contexts.pop(rid, None)

        self._running[rid] = asyncio.ensure_future(runner())

    async def _call_tool(self, msg_id: Any, params: Dict[str, Any]) -> None:
        name = params.get("name", "")
        tool = self._tools.get(name)
        if tool is None:
            self._error(msg_id, -32602, f"plugin `{self.name}` has no tool `{name}`", {"kind": "unknown_tool"})
            return
        try:
            result = tool.handler(params.get("arguments") or {})
            if inspect.isawaitable(result):
                result = await result
            self._reply(msg_id, tool_result(result, TOOL_IMAGES_FEATURE in self._host_features))
        except PluginError as e:
            self._error(msg_id, -32000, e.message, _error_data(e))
        except Exception as e:  # noqa: BLE001
            traceback.print_exc(file=sys.stderr)
            self._error(msg_id, -32000, f"{type(e).__name__}: {e}", {"kind": "plugin_exception"})

    # ---- output ----------------------------------------------------------

    @staticmethod
    def _encode(msg: Dict[str, Any]) -> str:
        # Strict JSON: NaN/Infinity and non-JSON values raise instead of producing a
        # line the host cannot parse (or a silently stringified value).
        return json.dumps(msg, separators=(",", ":"), allow_nan=False) + "\n"

    def _send(self, msg: Dict[str, Any]) -> None:
        line = self._encode(msg)
        with self._out_lock:
            sys.stdout.write(line)
            sys.stdout.flush()

    def _notify(self, method: str, params: Dict[str, Any]) -> None:
        try:
            self._send({"jsonrpc": "2.0", "method": method, "params": params})
        except (TypeError, ValueError) as e:
            sys.stderr.write(f"graphrt plugin {self.name}: dropped `{method}` notification: {e}\n")

    def _reply(self, msg_id: Any, result: Any) -> None:
        try:
            line = self._encode({"jsonrpc": "2.0", "id": msg_id, "result": result})
        except (TypeError, ValueError) as e:
            # The handler returned something JSON cannot represent (NaN, bytes, sets...).
            self._error(msg_id, -32000, f"result is not valid JSON: {e}", {"kind": "invalid_output"})
            return
        # A line above the host's limit would break the connection: report it instead.
        limit = self._limit[0] if hasattr(self, "_limit") else MAX_MESSAGE_BYTES
        size = len(line.encode("utf-8"))
        if size > limit:
            self._error(msg_id, -32000, f"result is {size} bytes, above the {limit}-byte message limit",
                        {"kind": "message_too_large"})
            return
        with self._out_lock:
            sys.stdout.write(line)
            sys.stdout.flush()

    def _error(self, msg_id: Any, code: int, message: str, data: Optional[Dict[str, Any]] = None) -> None:
        error: Dict[str, Any] = {"code": code, "message": message}
        if data is not None:
            error["data"] = data
        self._send({"jsonrpc": "2.0", "id": msg_id, "error": error})


def _error_data(e: PluginError) -> Dict[str, Any]:
    data: Dict[str, Any] = {"kind": e.kind, "retryable": e.retryable}
    if e.retry_after_ms is not None:
        data["retry_after_ms"] = e.retry_after_ms
    return data


_RESULT_KEYS = {"outputs", "branch", "writes", "usage", "metadata", "pause"}


def _normalize_result(result: Union[None, Dict[str, Any]]) -> Dict[str, Any]:
    if result is None:
        return {"outputs": {}}
    if not isinstance(result, dict):
        raise PluginError(f"node handlers must return a dict, got {type(result).__name__}", kind="plugin_protocol")
    if result and set(result) <= _RESULT_KEYS:
        return result
    return {"outputs": result}

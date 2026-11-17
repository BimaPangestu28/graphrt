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

    if __name__ == "__main__":
        plugin.run()

The plugin talks newline-delimited JSON-RPC 2.0 over stdin/stdout. Never
``print()`` to stdout: use ``ctx.log(...)`` or write to stderr instead.
"""

from __future__ import annotations

import asyncio
import inspect
import json
import sys
import threading
from dataclasses import dataclass, field
from typing import Any, Awaitable, Callable, Dict, List, Optional, Union

__all__ = [
    "Plugin",
    "PluginError",
    "RunContext",
    "RunRequest",
    "Resume",
    "pause",
    "PROTOCOL_VERSION",
]

PROTOCOL_VERSION = 1
MAX_MESSAGE_BYTES = 16 * 1024 * 1024


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

        Return a string, or ``{"text": ..., "data": ..., "is_error": bool}``.
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
        queue: "asyncio.Queue[Optional[bytes]]" = asyncio.Queue()

        def reader() -> None:
            stdin = sys.stdin.buffer
            while True:
                line = stdin.readline(MAX_MESSAGE_BYTES + 1)
                self._loop.call_soon_threadsafe(queue.put_nowait, line if line else None)
                if not line:
                    return

        threading.Thread(target=reader, name="graphrt-plugin-stdin", daemon=True).start()
        while True:
            line = await queue.get()
            if line is None:
                break
            text = line.decode("utf-8", errors="replace").strip()
            if not text:
                continue
            try:
                msg = json.loads(text)
            except ValueError:
                self._send({"jsonrpc": "2.0", "id": None, "error": {"code": -32700, "message": "parse error"}})
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
            if isinstance(result, str):
                result = {"text": result}
            self._reply(msg_id, {"text": "", "is_error": False, **(result or {})})
        except PluginError as e:
            self._error(msg_id, -32000, e.message, _error_data(e))
        except Exception as e:  # noqa: BLE001
            self._error(msg_id, -32000, f"{type(e).__name__}: {e}", {"kind": "plugin_exception"})

    # ---- output ----------------------------------------------------------

    def _send(self, msg: Dict[str, Any]) -> None:
        line = json.dumps(msg, separators=(",", ":"), default=str) + "\n"
        with self._out_lock:
            sys.stdout.write(line)
            sys.stdout.flush()

    def _notify(self, method: str, params: Dict[str, Any]) -> None:
        self._send({"jsonrpc": "2.0", "method": method, "params": params})

    def _reply(self, msg_id: Any, result: Any) -> None:
        self._send({"jsonrpc": "2.0", "id": msg_id, "result": result})

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

"""Test fixture plugin exercising host behaviors (crash, cancel, env, errors)."""

import asyncio
import os
import subprocess
import sys
import time

sys.path.insert(0, os.path.join(os.path.dirname(os.path.abspath(__file__)), "..", "..", "..", "..", "..", "sdk", "python"))
from graphrt_plugin import Plugin, PluginError, pause  # noqa: E402

plugin = Plugin("testkit")
STARTED_NS = time.time_ns()


@plugin.node("testkit.started")
async def started(ctx, req):
    # `pid` tells processes apart: on Windows `time_ns()` can tick only every ~15.6 ms,
    # so two processes started together may report the same STARTED_NS.
    return {"outputs": {"started_ns": STARTED_NS, "pid": os.getpid()}}


@plugin.node("testkit.echo")
async def echo(ctx, req):
    await ctx.chunk("text", "hel")
    await ctx.chunk("text", "lo")
    await ctx.progress("step", {"n": 1})
    ctx.log("echo ran")
    return {"outputs": {"config": req.config, "attempt": req.attempt, "node_id": req.node_id}}


@plugin.node("testkit.sleep")
async def sleep(ctx, req):
    marker = req.config.get("cancel_marker")
    try:
        await asyncio.sleep(req.config.get("ms", 1000) / 1000)
    except asyncio.CancelledError:
        if marker:
            with open(marker, "w") as f:
                f.write("cancelled")
        raise
    return {"outputs": {"slept": True}}


@plugin.node("testkit.crash_once")
async def crash_once(ctx, req):
    marker = req.config["marker"]
    if not os.path.exists(marker):
        with open(marker, "w") as f:
            f.write("crashed")
        sys.stderr.write("boom: simulated crash\n")
        sys.stderr.flush()
        os._exit(3)
    return {"outputs": {"recovered": True, "pid": os.getpid()}}


@plugin.node("testkit.env")
async def env(ctx, req):
    # Cargo sets CARGO_MANIFEST_DIR / CARGO_PKG_NAME for test processes; only the
    # second is allowed by the manifest.
    return {"outputs": {"has_secret": "CARGO_MANIFEST_DIR" in os.environ, "allowed": os.environ.get("CARGO_PKG_NAME"),
                        "fixed": os.environ.get("FIXED_VAR"), "plugin": os.environ.get("GRAPHRT_PLUGIN")}}


@plugin.node("testkit.fail")
async def fail(ctx, req):
    raise PluginError("upstream unavailable", kind="upstream", retryable=bool(req.config.get("retryable")),
                      retry_after_ms=req.config.get("retry_after_ms"))


@plugin.node("testkit.boom")
async def boom(ctx, req):
    raise ValueError("unexpected")


@plugin.node("testkit.approve")
async def approve(ctx, req):
    if req.resume is None:
        return pause("approval", {"question": req.config.get("question")}, {"asked": req.config.get("question")})
    return {"outputs": {"answer": req.resume.input, "asked": req.resume.state["asked"]},
            "branch": "yes" if req.resume.input == "yes" else "no"}


@plugin.node("testkit.write")
async def write(ctx, req):
    return {"outputs": {"ok": True}, "writes": [{"selector": ["conversation", "topic"], "value": req.config.get("topic")}]}


@plugin.node("testkit.hang")
async def hang(ctx, req):
    # Blocks the whole event loop: the plugin cannot even process `node.cancel`.
    time.sleep(req.config.get("seconds", 30))
    return {"outputs": {}}


@plugin.node("testkit.badbranch")
async def badbranch(ctx, req):
    return {"outputs": {}, "branch": "maybe"}


@plugin.node("testkit.grandchild")
async def grandchild(ctx, req):
    proc = subprocess.Popen(["sleep", "60"])
    with open(req.config["marker"], "w") as f:
        f.write(str(proc.pid))
    os._exit(4)


@plugin.node("testkit.nan")
async def nan(ctx, req):
    return {"outputs": {"value": float("nan")}}


@plugin.tool("add", description="Add two numbers",
             parameters={"type": "object", "properties": {"a": {"type": "number"}, "b": {"type": "number"}}})
async def add(args):
    total = args.get("a", 0) + args.get("b", 0)
    return {"text": str(total), "data": {"sum": total}}


if __name__ == "__main__":
    plugin.run()

# graphrt-plugin (Python)

Write graphrt node kinds and tools in Python. Single file, standard library only,
Python 3.9+.

```python
from graphrt_plugin import Plugin, PluginError, pause

plugin = Plugin("acme")

@plugin.node("acme.sentiment")
async def sentiment(ctx, req):
    text = req.config.get("text", "")
    await ctx.chunk("explanation", "scanning...")
    if not text:
        raise PluginError("text is empty", kind="invalid_input")
    return {"outputs": {"label": "positive" if "love" in text else "neutral"}}

@plugin.node("acme.approve")
async def approve(ctx, req):
    if req.resume is None:
        return pause("approval", {"message": "Approve?"}, {"draft": req.config.get("text")})
    return {"outputs": {"approved": req.resume.input == "yes"}}

@plugin.tool("score", description="Score a text", parameters={"type": "object"})
async def score(args):
    return {"text": "0.9", "data": {"score": 0.9}}

if __name__ == "__main__":
    plugin.run()
```

Install with `pip install ./sdk/python`, or copy `graphrt_plugin.py` next to your
plugin. Pair it with a `graphrt-plugin.json` manifest; see `docs/plugins.md`.

- `req.config`: node config with variables resolved by the host.
- `req.resume`: `Resume(state, input)` after a pause, else `None`.
- `ctx.chunk(field, text)`, `ctx.progress(name, data)`, `ctx.log(message)`.
- `ctx.cancelled` / `await ctx.wait_cancelled()`; cancelled handlers are also
  interrupted with `asyncio.CancelledError`.
- Never `print()` to stdout: stdout carries the protocol.

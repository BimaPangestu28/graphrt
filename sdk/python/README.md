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
- A tool handler with two positional parameters, `async def handler(args, context)`,
  gets a `ToolContext`: `context.secrets` holds per-call values from an
  `agent` node's `tool_context` (e.g. `context.secrets.get("user_jwt")`). It is
  empty unless the manifest requests `permissions.secrets` and the plugin is
  trusted. Never log or return them. One-parameter handlers keep working.
- Tools can return images for the model:
  `{"text": ..., "images": [{"media_type": "image/png", "data": png_bytes}]}`
  (or `"data_base64"`, or `{"url": "https://..."}`); at most 4,
  png/jpeg/webp/gif, 5 MiB each. Hosts without image support get the text
  plus a note instead.

Tests: `python3 -m unittest discover -s sdk/python`.

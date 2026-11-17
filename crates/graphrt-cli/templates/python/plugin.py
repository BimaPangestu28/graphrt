"""graphrt plugin `{{name}}`.

Run it in a workflow with:  graphrt run workflow.yaml --plugins <parent dir>
Never print() to stdout: stdout carries the plugin protocol (use stderr).
"""

from graphrt_plugin import Plugin, PluginError

plugin = Plugin("{{name}}")


@plugin.node("{{name}}.greet")
async def greet(ctx, req):
    name = req.config.get("name")
    if not isinstance(name, str) or not name:
        raise PluginError("`name` must be a non-empty string", kind="invalid_input")
    greeting = f"Hello, {name}!"
    # Stream word by word; the chunks concatenate to exactly `greeting`.
    for i, word in enumerate(greeting.split(" ")):
        await ctx.chunk("text", word if i == 0 else " " + word)
    return {"outputs": {"text": greeting, "length": len(greeting)}}


if __name__ == "__main__":
    plugin.run()

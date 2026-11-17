"""Example plugin: a tiny lexicon-based sentiment analyzer.

Shows streaming chunks, branching via handles, and a tool.
"""

import os
import sys

try:
    from graphrt_plugin import Plugin, PluginError
except ImportError:  # running from the repository checkout
    sys.path.insert(0, os.path.join(os.path.dirname(os.path.abspath(__file__)), "..", "..", "..", "sdk", "python"))
    from graphrt_plugin import Plugin, PluginError

POSITIVE = {"love", "great", "good", "excellent", "happy", "amazing", "fast", "nice"}
NEGATIVE = {"hate", "bad", "terrible", "awful", "slow", "broken", "sad", "angry"}

plugin = Plugin("sentiment")


def score(text: str) -> float:
    words = [w.strip(".,!?;:\"'").lower() for w in text.split()]
    pos = sum(w in POSITIVE for w in words)
    neg = sum(w in NEGATIVE for w in words)
    if pos == neg == 0:
        return 0.0
    return (pos - neg) / (pos + neg)


@plugin.node("sentiment.analyze")
async def analyze(ctx, req):
    text = req.config.get("text")
    if not isinstance(text, str):
        raise PluginError("`text` must be a string", kind="invalid_input")
    value = score(text)
    label = "positive" if value > 0 else "negative" if value < 0 else "neutral"
    for word in text.split():
        cleaned = word.strip(".,!?;:\"'").lower()
        if cleaned in POSITIVE or cleaned in NEGATIVE:
            await ctx.chunk("explanation", f"{cleaned} ")
    return {"outputs": {"label": label, "score": value}, "branch": label}


@plugin.tool("score", description="Sentiment score of a text in [-1, 1]",
             parameters={"type": "object", "properties": {"text": {"type": "string"}}, "required": ["text"]})
async def score_tool(args):
    value = score(str(args.get("text", "")))
    return {"text": f"{value:.2f}", "data": {"score": value}}


if __name__ == "__main__":
    plugin.run()

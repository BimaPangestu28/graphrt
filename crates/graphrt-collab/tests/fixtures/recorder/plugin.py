"""Test fixture: a plugin service that records every call of the collaboration server."""

import json
import os
import sys

# The test copies this plugin to a temporary directory and names the SDK's directory in the manifest's `env`.
sys.path.insert(0, os.environ["GRAPHRT_PYTHON_SDK"])
from graphrt_plugin import Plugin  # noqa: E402

plugin = Plugin("recorder")
OUT = os.path.join(os.path.dirname(os.path.abspath(__file__)), "recorded.jsonl")


@plugin.tool("record", description="Record what the collaboration server sends", parameters={"type": "object"})
def record(args, context):
    entry = {"args": args, "secrets": dict(context.secrets), "env_has_token": "TOKEN" in os.environ}
    with open(OUT, "a", encoding="utf-8") as out:
        out.write(json.dumps(entry) + "\n")
    return {"text": "recorded", "data": {"echo": args.get("call"), "event": args.get("event")}}


if __name__ == "__main__":
    plugin.run()

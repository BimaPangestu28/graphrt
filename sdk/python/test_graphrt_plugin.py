"""Tests for tool result images: python3 -m unittest discover -s sdk/python"""

import json
import os
import subprocess
import sys
import tempfile
import textwrap
import unittest

sys.path.insert(0, os.path.dirname(os.path.abspath(__file__)))

from graphrt_plugin import MAX_TOOL_IMAGE_BYTES, PluginError, ToolContext, tool_result  # noqa: E402

PLUGIN = textwrap.dedent(
    """
    import sys
    sys.path.insert(0, {sdk_dir!r})
    from graphrt_plugin import Plugin

    plugin = Plugin("screen")

    @plugin.tool("shot")
    async def shot(args):
        size = args.get("size", 5)
        return {{"text": "800x600", "images": [{{"media_type": "image/png", "data": b"\\x01" * size}}]}}

    @plugin.tool("bad")
    async def bad(args):
        return {{"text": "odd", "images": [{{"media_type": "image/bmp", "data": b"abc"}}]}}

    @plugin.tool("who")
    async def who(args, context):
        assert "eyJ" not in repr(context)
        return context.secrets.get("user_jwt", "anonymous")

    @plugin.tool("sync_who")
    def sync_who(args, context):
        return context.secrets.get("user_jwt", "anonymous")

    plugin.run()
    """
)


def exchange(messages):
    """Run the test plugin, send `messages`, return the replies by id."""
    sdk_dir = os.path.dirname(os.path.abspath(__file__))
    with tempfile.TemporaryDirectory() as tmp:
        script = os.path.join(tmp, "plugin.py")
        with open(script, "w") as f:
            f.write(PLUGIN.format(sdk_dir=sdk_dir))
        stdin = "".join(json.dumps({"jsonrpc": "2.0", **m}) + "\n" for m in messages)
        proc = subprocess.run([sys.executable, "-I", script], input=stdin, capture_output=True, text=True, timeout=20)
    replies = [json.loads(line) for line in proc.stdout.splitlines() if line.strip()]
    return {r.get("id"): r for r in replies}


def call(name, **arguments):
    return {"id": "t", "method": "tool.call", "params": {"name": name, "arguments": arguments}}


class ToolImagesTest(unittest.TestCase):
    def test_images_are_sent_to_hosts_that_announce_tool_images(self):
        replies = exchange([{"id": "i", "method": "initialize", "params": {"features": ["tool_images"]}}, call("shot")])
        self.assertEqual(
            replies["t"]["result"],
            {"text": "800x600", "is_error": False, "images": [{"media_type": "image/png", "data_base64": "AQEBAQE="}]},
        )

    def test_images_are_dropped_with_a_note_for_older_hosts(self):
        replies = exchange([{"id": "i", "method": "initialize", "params": {}}, call("shot")])
        result = replies["t"]["result"]
        self.assertNotIn("images", result)
        self.assertEqual(
            result["text"], "800x600\n[1 image(s) omitted: the graphrt host does not support image tool results]"
        )

    def test_invalid_images_fail_the_call(self):
        replies = exchange([{"id": "i", "method": "initialize", "params": {"features": ["tool_images"]}}, call("bad")])
        self.assertEqual(replies["t"]["error"]["data"]["kind"], "invalid_output")
        self.assertIn("image/bmp", replies["t"]["error"]["message"])

    def test_results_above_the_line_limit_are_reported(self):
        init = {"id": "i", "method": "initialize", "params": {"features": ["tool_images"], "max_message_bytes": 2048}}
        replies = exchange([init, call("shot", size=4096)])
        self.assertEqual(replies["t"]["error"]["data"]["kind"], "message_too_large")

    def test_tool_result_limits(self):
        self.assertEqual(tool_result("hi"), {"text": "hi", "is_error": False})
        with self.assertRaises(PluginError):
            tool_result({"images": [{"url": "https://x/a.png"}] * 5})
        with self.assertRaisesRegex(PluginError, "https"):
            tool_result({"images": [{"url": "http://x/a.png"}]})
        with self.assertRaisesRegex(PluginError, "base64"):
            tool_result({"images": [{"media_type": "image/png", "data_base64": "%%%"}]})
        with self.assertRaisesRegex(PluginError, "limit"):
            tool_result({"images": [{"media_type": "image/png", "data": b"\0" * (MAX_TOOL_IMAGE_BYTES + 1)}]})
        url = tool_result({"text": "", "images": [{"url": "https://x/a.png"}]})
        self.assertEqual(url["images"], [{"url": "https://x/a.png"}])


if __name__ == "__main__":
    unittest.main()


class ToolContextTest(unittest.TestCase):
    def test_handlers_with_two_arguments_receive_the_context(self):
        context = {"secrets": {"user_jwt": "eyJ-1", "n": 7}}
        with_context = {"name": "who", "arguments": {}, "context": context}
        replies = exchange([
            {"id": "a", "method": "tool.call", "params": with_context},
            {"id": "b", "method": "tool.call", "params": {"name": "who", "arguments": {}}},
            {"id": "c", "method": "tool.call", "params": {**with_context, "name": "sync_who"}},
            {"id": "d", "method": "tool.call", "params": {**with_context, "name": "shot", "arguments": {"size": 1}}},
        ])
        self.assertEqual(replies["a"]["result"]["text"], "eyJ-1")
        self.assertEqual(replies["b"]["result"]["text"], "anonymous")
        self.assertEqual(replies["c"]["result"]["text"], "eyJ-1")
        # One-argument handlers keep working.
        self.assertIn("800x600", replies["d"]["result"]["text"])

    def test_from_params_keeps_only_string_secrets(self):
        self.assertEqual(ToolContext.from_params(None).secrets, {})
        self.assertEqual(ToolContext.from_params({"secrets": ["x"]}).secrets, {})
        self.assertEqual(ToolContext.from_params({"secrets": {"a": "1", "b": 2}}).secrets, {"a": "1"})
        self.assertEqual(repr(ToolContext({"a": "secret-value"})), "ToolContext(secrets=['a'])")

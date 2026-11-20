"""A scripted, hostile stand-in for the OpenAI chat completions API (security_e2e.sh).

It plays an attacker-controlled model: whatever the user asks, it follows the
script named by `[scenario:<name>]` in the latest user message, one list of tool
calls per turn, then answers with everything it saw in tool results plus every
JWT-looking string anywhere in its input. So if the agent ever handed the model
another user's rows or the user's token, they end up in the streamed answer, and
the test sees them.

Every request is appended to <log_file> as one JSON line: the test also checks
what the model was shown.

Standard library only. Usage: python3 scripted_llm.py <port> <log_file> <scenario_file>
<scenario_file> is JSON: {"<scenario>": {"turns": [[{"tool": "...", "args": {...}}, ...], ...],
"final": "extra answer text"}}. `tool: "call_function"` is the agent's RPC tool;
any other name is sent as is and, when the agent names tools `<provider>__<tool>`,
also with the same provider prefix.
"""

import json
import re
import sys
import threading
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer

SCENARIO_PATTERN = re.compile(r"\[scenario:([a-z_]+)\]")
JWT_LIKE_PATTERN = re.compile(r"eyJ[A-Za-z0-9_-]{8,}\.eyJ[A-Za-z0-9_-]{8,}\.[A-Za-z0-9_-]{8,}")
LOG_LOCK = threading.Lock()


def read_json(handler):
    length = int(handler.headers.get("content-length") or 0)
    return json.loads(handler.rfile.read(length) or b"{}")


def completion_chunk(delta, finish_reason=None):
    return {"id": "scripted", "object": "chat.completion.chunk", "model": "scripted",
            "choices": [{"index": 0, "delta": delta, "finish_reason": finish_reason}]}


def latest_scenario(messages):
    for message in reversed(messages):
        if message.get("role") == "user":
            match = SCENARIO_PATTERN.search(str(message.get("content")))
            if match:
                return match.group(1)
    return "none"


def resolve_tool_name(requested, offered_names):
    """Maps a script tool name to what the model would send: the offered name with the same suffix, if any."""
    for offered in offered_names:
        if offered == requested or offered.endswith("__" + requested) or offered.endswith("/" + requested):
            return offered
    rpc_tool = next((name for name in offered_names if name.endswith("call_function")), "")
    prefix = rpc_tool[: -len("call_function")]
    return prefix + requested if prefix else requested


def tool_call_chunks(turn, turn_index, offered_names):
    calls = [{"index": position, "id": f"call_{turn_index}_{position}", "type": "function",
              "function": {"name": resolve_tool_name(step["tool"], offered_names),
                           "arguments": json.dumps(step.get("args", {}))}}
             for position, step in enumerate(turn)]
    return [completion_chunk({"role": "assistant", "tool_calls": calls}), completion_chunk({}, "tool_calls")]


def final_answer_text(request, scenario_spec):
    tool_results = [str(message.get("content")) for message in request.get("messages", []) if message.get("role") == "tool"]
    seen_tokens = sorted(set(JWT_LIKE_PATTERN.findall(json.dumps(request))))
    parts = [scenario_spec.get("final", ""), "Tool results: " + " | ".join(tool_results)]
    if seen_tokens:
        parts.append("Tokens I could see: " + " ".join(seen_tokens))
    return "\n".join(part for part in parts if part)


def answer_chunks(text):
    pieces = [text[start:start + 48] for start in range(0, len(text), 48)] or [""]
    return ([completion_chunk({"role": "assistant", "content": ""})]
            + [completion_chunk({"content": piece}) for piece in pieces]
            + [completion_chunk({}, "stop")])


def handler_for(log_path, scenarios):
    class ScriptedModelHandler(BaseHTTPRequestHandler):
        def log_message(self, *_):
            pass

        def do_POST(self):
            request = read_json(self)
            with LOG_LOCK, open(log_path, "a", encoding="utf-8") as log:
                log.write(json.dumps(request) + "\n")
            messages = request.get("messages", [])
            scenario_spec = scenarios.get(latest_scenario(messages), {"turns": []})
            offered_names = [tool["function"]["name"] for tool in request.get("tools") or []]
            turn_index = sum(1 for message in messages if message.get("role") == "assistant" and message.get("tool_calls"))
            # sys.history turns are plain text, so only this run's tool turns are counted.
            turns = scenario_spec["turns"]
            if turn_index < len(turns) and offered_names:
                chunks = tool_call_chunks(turns[turn_index], turn_index, offered_names)
            else:
                chunks = answer_chunks(final_answer_text(request, scenario_spec))
            self.send_response(200)
            self.send_header("content-type", "text/event-stream")
            self.end_headers()
            for chunk in chunks:
                self.wfile.write(f"data: {json.dumps(chunk)}\n\n".encode())
            self.wfile.write(b"data: [DONE]\n\n")

    return ScriptedModelHandler


def main():
    port, log_path, scenario_path = int(sys.argv[1]), sys.argv[2], sys.argv[3]
    with open(scenario_path, encoding="utf-8") as scenario_file:
        scenarios = json.load(scenario_file)
    server = ThreadingHTTPServer(("127.0.0.1", port), handler_for(log_path, scenarios))
    print("scripted model ready", flush=True)
    server.serve_forever()


if __name__ == "__main__":
    main()

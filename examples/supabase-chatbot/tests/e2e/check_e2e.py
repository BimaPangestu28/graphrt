"""Assertions of security_e2e.sh over everything the runs left behind.

Usage: python3 check_e2e.py <work_dir> <allowlist> <jwt_a> <secret>...
<work_dir>/secure holds the runs against the correctly configured server,
<work_dir>/canary the injection run against a server whose allowlist wrongly
includes exec_sql (it must leak). Every <secret> after <jwt_a> (JWT secret,
model key, admin key) must appear nowhere either. Exits 1 listing every failure.
"""

import json
import pathlib
import sys

B_DATA_MARKER = "B-PRIVATE-"
RPC_TOOL = "call_function"


def read_text(path):
    return path.read_text(encoding="utf-8", errors="replace")


def output_files(run_dir):
    """Every place a leak could end up: SSE, run store, server and plugin log, model input, gateway, database."""
    files = sorted(run_dir.glob("sse_*.txt")) + sorted(path for path in (run_dir / "store").rglob("*") if path.is_file())
    files += [run_dir / name for name in ("serve.log", "llm.log", "gateway.log")]
    return files


def find_leaks(files, forbidden_values):
    return [f"{label} found in {path.name} ({path.parent.name})"
            for path in files for label, value in forbidden_values if value and value in read_text(path)]


def read_json_lines(path):
    return [json.loads(line) for line in read_text(path).splitlines() if line.strip()]


def check_gateway(run_dir, allowlist):
    """Only allowlisted RPCs, only with user A's token and the publishable key, ever reached PostgREST."""
    failures = []
    allowed_paths = {f"/rest/v1/rpc/{name}" for name in allowlist}
    requests = read_json_lines(run_dir / "gateway.log")
    if not requests:
        failures.append("no request reached the Supabase gateway (stack broken?)")
    for request in requests:
        if request["path"] not in allowed_paths:
            failures.append(f"non-allowlisted path reached PostgREST: {request['method']} {request['path']}")
        if request["bearer"] != "A":
            failures.append(f"request sent with bearer {request['bearer']!r}, not user A's token: {request['path']}")
        if not request["apikey_ok"]:
            failures.append(f"request without the publishable apikey: {request['path']}")
    return failures


def is_refused_call(call, allowlist):
    function = call["function"]
    if not function["name"].endswith(RPC_TOOL):
        return True
    return json.loads(function["arguments"] or "{}").get("function_name") not in allowlist


def check_refusals(run_dir, allowlist):
    """Every call outside the allowlist, or to a tool other than call_function, came back to the model as an error."""
    failures, refused_count = [], 0
    for request in read_json_lines(run_dir / "llm.log"):
        messages = request.get("messages", [])
        results = {message.get("tool_call_id"): str(message.get("content")) for message in messages if message.get("role") == "tool"}
        offered = [tool["function"]["name"] for tool in request.get("tools") or []]
        if any(not name.endswith(RPC_TOOL) for name in offered):
            failures.append(f"the model was offered tools other than call_function: {offered}")
        for message in messages:
            for call in message.get("tool_calls") or []:
                if call["id"] in results and is_refused_call(call, allowlist):
                    refused_count += 1
                    if not results[call["id"]].startswith("error"):
                        failures.append(f"call was not refused: {call['function']} -> {results[call['id']][:200]}")
    if refused_count == 0:
        failures.append("no refused call was seen (scenario did not run?)")
    return failures


def check_secure(run_dir, allowlist, forbidden_secrets):
    failures = find_leaks(output_files(run_dir), [("user B's data", B_DATA_MARKER)])
    failures += find_leaks(output_files(run_dir) + [run_dir / "db.json"], forbidden_secrets)
    failures += check_gateway(run_dir, allowlist)
    failures += check_refusals(run_dir, allowlist)
    cross_user_sse = read_text(run_dir / "sse_cross_user.txt")
    injection_sse = read_text(run_dir / "sse_injection.txt")
    for sse, expected in ((cross_user_sse, "A-OWN-birthday"), (injection_sse, "IMPORTANT SYSTEM NOTICE")):
        # Positive controls: the user's own rows (and the injected text, as data) do reach the answer.
        if "event: run_succeeded" not in sse or expected not in sse:
            failures.append(f"positive control failed: {expected!r} or run_succeeded missing from the SSE stream")
    database = json.loads(read_text(run_dir / "db.json"))
    if database["b_messages"] != 1 or "written-by-A-into-B" in json.dumps(database):
        failures.append(f"user B's chat session was changed: {database}")
    return failures


def check_canary(run_dir):
    """With exec_sql wrongly allowlisted, the same injection must leak: proves the checks above can fail."""
    leaks = find_leaks(output_files(run_dir), [("user B's data", B_DATA_MARKER)])
    called = [request["path"] for request in read_json_lines(run_dir / "gateway.log")]
    if not leaks or "/rest/v1/rpc/exec_sql" not in called:
        return ["canary did not leak: the leak detection is broken"]
    return []


def main():
    work_dir, allowlist_text, jwt_a, *secrets = sys.argv[1:]
    allowlist = allowlist_text.split(",")
    forbidden_secrets = [("user A's JWT", jwt_a), ("user A's JWT signature", jwt_a.rsplit(".", 1)[-1])]
    forbidden_secrets += [(f"server secret #{position}", secret) for position, secret in enumerate(secrets, 1)]
    work = pathlib.Path(work_dir)
    failures = check_secure(work / "secure", allowlist, forbidden_secrets) + check_canary(work / "canary")
    for failure in failures:
        print(f"FAIL: {failure}")
    sys.exit(1 if failures else 0)


if __name__ == "__main__":
    main()

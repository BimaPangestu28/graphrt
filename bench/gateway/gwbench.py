#!/usr/bin/env python3
"""Run-time targets of `graphrt gateway` on Linux, measured on the real binaries.

Each scenario starts a real `graphrt gateway` process (and real
`graphrt serve --trust-gateway` agents), drives it over plain sockets and
compares the result with the `[gateway_targets]` table of bench/budgets.toml
(the targets of the deploy/gateway audit, §4):

  ttfb-uds    warm TTFB overhead, gateway -> agent on a Unix socket (local provider):
              first `run_started` byte through the gateway vs signed straight to the
              same agent, interleaved, n = ttfb_samples
  ttfb-tcp    the same with the agent on loopback TCP (k8s provider against a fake
              Kubernetes API); needs --gateway-k8s
  sse         sse_streams concurrent streaming runs through one gateway: gateway RSS
              (peak) and stalls (a gap between chunks > sse_max_gap_ms, a missing
              chunk or an unfinished stream)
  wake        cold wake of the local provider (exec of a new agent -> first SSE event
              of the run), one fresh deployment per sample
  wake-node   the same with a Node.js process plugin: request -> the plugin node's result
              (the agent starts the plugin host at start-up; the plugin's own spawn -> initialize
              time without graphrt is reported as the floor)
  plugin-pool a kept-warm agent with the Node.js plugin and `--plugin-pool`: RSS of the agent
              and its plugin processes for pool sizes 0/1/2, a run on the warm pool, and after the
              idle stop (`--plugin-idle`) a run cold vs. after a gateway warm hint (POST .../warm)
  webhook     webhook ack (HMAC-signed POST -> 202), sequential, n = webhook_samples
  cron        lag of cron_fires fires (`* * * * *` schedules with prewarm): fire time
              -> run record created, and -> run finished (gated: an upper bound)

Standard library only (Python 3.11+), Linux only (reads /proc). Exit status 1 when
a target is missed, 2 on a setup error.

Usage:
  bench/gateway/gwbench.py --gateway BIN --agent BIN [--gateway-k8s BIN]
                           [--only ttfb-uds,sse,...] [--quick] [--json OUT] [--markdown OUT]
"""

from __future__ import annotations

import argparse
import asyncio
import hashlib
import hmac
import json
import os
import resource
import secrets
import shutil
import signal
import socket
import sqlite3
import statistics
import subprocess
import sys
import tempfile
import threading
import time
import tomllib
from dataclasses import dataclass, field
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer
from pathlib import Path

ROOT = Path(__file__).resolve().parents[2]
SCENARIOS = ["ttfb-uds", "ttfb-tcp", "sse", "wake", "wake-node", "plugin-pool", "webhook", "cron"]
ADMIN_TOKEN = secrets.token_hex(16)
API_KEY = "bench-" + secrets.token_hex(16)
AGENT_KEY_HEX = secrets.token_hex(32)
AGENT_KEYS = f"k1:{AGENT_KEY_HEX}"
HOOK_SECRET = secrets.token_hex(16)
K8S_NAMESPACE = "bench"

HELLO_WORKFLOW = """version: "1"
name: hello
graph:
  nodes:
    - id: start
      type: start
      variables: [{ variable: name, type: text-input, required: false }]
    - { id: greet, type: template-transform, template: "Hello, {{ name }}.", variables: [{ variable: name, value_selector: [start, name] }] }
    - { id: end, type: end, outputs: [{ variable: greeting, value_selector: [greet, output] }] }
  edges:
    - { source: start, target: greet }
    - { source: greet, target: end }
"""

STREAM_WORKFLOW = """version: "1"
name: chat
graph:
  nodes:
    - { id: start, type: start, variables: [] }
    - id: llm
      type: llm
      model: { provider: openai, name: gpt-4o-mini }
      prompt_template: [{ role: user, text: "Stream." }]
    - { id: end, type: end, outputs: [{ variable: text, value_selector: [llm, text] }] }
  edges:
    - { source: start, target: llm }
    - { source: llm, target: end }
"""

PLUGIN_WORKFLOW = """version: "1"
name: shout
graph:
  nodes:
    - { id: start, type: start, variables: [] }
    - { id: shout, type: upper.transform, text: "hello from node", mode: upper }
    - { id: end, type: end, outputs: [{ variable: text, value_selector: [shout, text] }] }
  edges:
    - { source: start, target: shout }
    - { source: shout, target: end }
"""

# A chat-completions stream: `chunks` deltas `interval_ms` apart (argv: port chunks interval_ms).
FAKE_OPENAI = r'''
import asyncio, json, sys
PORT, CHUNKS, INTERVAL = int(sys.argv[1]), int(sys.argv[2]), int(sys.argv[3]) / 1000
def frame(delta, finish=None):
    body = {"id": "c", "object": "chat.completion.chunk", "created": 0, "model": "gpt-4o-mini",
            "choices": [{"index": 0, "delta": delta, "finish_reason": finish}]}
    return b"data: " + json.dumps(body).encode() + b"\n\n"
async def handle(reader, writer):
    try:
        head = await reader.readuntil(b"\r\n\r\n")
        length = 0
        for line in head.split(b"\r\n"):
            if line.lower().startswith(b"content-length:"):
                length = int(line.split(b":")[1])
        await reader.readexactly(length)
        writer.write(b"HTTP/1.1 200 OK\r\ncontent-type: text/event-stream\r\nconnection: close\r\n\r\n")
        writer.write(frame({"role": "assistant", "content": ""}))
        for i in range(CHUNKS):
            writer.write(frame({"content": f"tok{i} "}))
            await writer.drain()
            await asyncio.sleep(INTERVAL)
        writer.write(frame({}, "stop"))
        writer.write(b"data: [DONE]\n\n")
        await writer.drain()
    except (ConnectionError, asyncio.IncompleteReadError):
        pass
    finally:
        writer.close()
async def main():
    server = await asyncio.start_server(handle, "127.0.0.1", PORT, backlog=4096)
    async with server:
        await server.serve_forever()
asyncio.run(main())
'''


# --------------------------------------------------------------------------- utilities


def free_port() -> int:
    with socket.socket() as sock:
        sock.bind(("127.0.0.1", 0))
        return sock.getsockname()[1]


def percentile(values: list[float], fraction: float) -> float:
    """Nearest-rank percentile (`fraction` in 0..1)."""
    ordered = sorted(values)
    rank = max(1, round(fraction * len(ordered)))
    return ordered[min(len(ordered), rank) - 1]


def summarize(values: list[float]) -> dict:
    return {
        "n": len(values),
        "p50": round(percentile(values, 0.50), 4),
        "p95": round(percentile(values, 0.95), 4),
        "p99": round(percentile(values, 0.99), 4),
        "max": round(max(values), 4),
        "mean": round(statistics.fmean(values), 4),
    }


def proc_status_kib(pid: int, key: str) -> int:
    for line in Path(f"/proc/{pid}/status").read_text().splitlines():
        if line.startswith(key + ":"):
            return int(line.split()[1])
    raise KeyError(key)


def raise_fd_limit() -> None:
    """1,000 streams need ~3,000 descriptors in the gateway (clients + agents)."""
    soft, hard = resource.getrlimit(resource.RLIMIT_NOFILE)
    resource.setrlimit(resource.RLIMIT_NOFILE, (max(soft, min(hard, 65536)), hard))


@dataclass(frozen=True)
class Address:
    """Where an HTTP server listens: a Unix socket path or a loopback TCP port."""

    unix: str | None = None
    port: int | None = None

    def connect(self, timeout: float = 30.0) -> socket.socket:
        if self.unix:
            sock = socket.socket(socket.AF_UNIX, socket.SOCK_STREAM)
        else:
            sock = socket.socket(socket.AF_INET, socket.SOCK_STREAM)
            sock.setsockopt(socket.IPPROTO_TCP, socket.TCP_NODELAY, 1)
        sock.settimeout(timeout)
        sock.connect(self.unix or ("127.0.0.1", self.port))
        return sock


def http_request(method: str, path: str, headers: dict[str, str], body: bytes = b"") -> bytes:
    lines = [f"{method} {path} HTTP/1.1", "Host: localhost", f"Content-Length: {len(body)}", "Connection: close"]
    lines += [f"{name}: {value}" for name, value in headers.items()]
    return ("\r\n".join(lines) + "\r\n\r\n").encode() + body


def exchange(address: Address, request: bytes, timeout: float = 30.0) -> tuple[int, bytes]:
    """Send one request and read the whole (close-delimited) response."""
    with address.connect(timeout) as sock:
        sock.sendall(request)
        chunks = []
        while chunk := sock.recv(65536):
            chunks.append(chunk)
    response = b"".join(chunks)
    status = int(response.split(b" ", 2)[1]) if response.startswith(b"HTTP/1.1 ") else 0
    return status, response


def time_to_marker(address: Address, request: bytes, marker: bytes) -> float:
    """Milliseconds from connect to the first byte of `marker` in the response; reads it to the end."""
    began = time.perf_counter_ns()
    with address.connect() as sock:
        sock.sendall(request)
        received = b""
        while marker not in received:
            chunk = sock.recv(65536)
            if not chunk:
                raise RuntimeError(f"stream ended before {marker!r}: {received[:200]!r} ... {received[-600:]!r}")
            received += chunk
        elapsed_ms = (time.perf_counter_ns() - began) / 1e6
        while sock.recv(65536):
            pass
    if not received.startswith(b"HTTP/1.1 200"):
        raise RuntimeError(f"unexpected response: {received[:300]!r}")
    return elapsed_ms


def wait_until(condition, timeout: float, what: str, interval: float = 0.005) -> None:
    deadline = time.monotonic() + timeout
    while not condition():
        if time.monotonic() > deadline:
            raise RuntimeError(f"timed out waiting for {what}")
        time.sleep(interval)


# --------------------------------------------------------------------------- signing


def signed_agent_request(deployment: str, run_id: str, epoch: int, body: bytes) -> bytes:
    """A `POST /internal/v1/runs` signed as the gateway signs it (docs/gateway-protocol.md)."""
    path, ts, nonce = "/internal/v1/runs", int(time.time()), secrets.token_hex(8)
    canonical = "|".join(
        ["v1", "k1", deployment, "POST", path, str(ts), nonce, run_id, str(epoch), hashlib.sha256(body).hexdigest()]
    )
    mac = hmac.new(bytes.fromhex(AGENT_KEY_HEX), canonical.encode(), hashlib.sha256).hexdigest()
    headers = {
        "Content-Type": "application/json",
        "Graphrt-Protocol": "1",
        "Graphrt-Deployment": deployment,
        "Graphrt-Run-Id": run_id,
        "Graphrt-Epoch": str(epoch),
        "Graphrt-Sig": f"v1;kid=k1;ts={ts};n={nonce};mac={mac}",
    }
    return http_request("POST", path, headers, body)


def client_run_request(deployment: str, body: dict) -> bytes:
    payload = json.dumps(body).encode()
    headers = {"Content-Type": "application/json", "X-Api-Key": API_KEY}
    return http_request("POST", f"/v1/deployments/{deployment}/runs", headers, payload)


# --------------------------------------------------------------------------- processes


@dataclass
class Workspace:
    """Temporary files of one scenario."""

    root: Path

    def write(self, name: str, text: str) -> Path:
        path = self.root / name
        path.parent.mkdir(parents=True, exist_ok=True)
        path.write_text(text)
        return path


def deployment_entry(name: str, **settings) -> dict:
    entry = {"name": name, "keys_env": "BENCH_AGENT_KEYS", "api_keys": [hashlib.sha256(API_KEY.encode()).hexdigest()]}
    entry.update(settings)
    return entry


@dataclass
class Gateway:
    """A `graphrt gateway --config` process."""

    binary: Path
    work: Workspace
    deployments: list[dict]
    providers: dict = field(default_factory=dict)
    env: dict = field(default_factory=dict)
    port: int = 0
    process: subprocess.Popen | None = None

    @property
    def address(self) -> Address:
        return Address(port=self.port)

    @property
    def socket_dir(self) -> Path:
        return self.work.root / "sockets"

    def start(self) -> "Gateway":
        self.port = free_port()
        config = {
            "listen": f"127.0.0.1:{self.port}",
            "store": str(self.work.root / "gateway.db"),
            "socket_dir": str(self.socket_dir),
            "admin_token_env": "BENCH_ADMIN_TOKEN",
            "deployments": self.deployments,
        }
        if self.providers:
            config["providers"] = self.providers
        # JSON is YAML.
        path = self.work.write("gateway.yaml", json.dumps(config, indent=1))
        env = {k: v for k, v in os.environ.items() if not k.startswith("GRAPHRT_")}
        env.update({"BENCH_ADMIN_TOKEN": ADMIN_TOKEN, "BENCH_AGENT_KEYS": AGENT_KEYS, "BENCH_HOOK_SECRET": HOOK_SECRET})
        env.update(self.env)
        log = open(self.work.root / "gateway.log", "wb")
        self.process = subprocess.Popen(
            [str(self.binary), "gateway", "--config", str(path)], stdout=log, stderr=log, env=env
        )
        wait_until(self.healthy, 15, "gateway /healthz")
        return self

    def healthy(self) -> bool:
        if self.process and self.process.poll() is not None:
            raise RuntimeError(f"gateway exited {self.process.returncode}: {self.log_tail()}")
        try:
            return exchange(self.address, http_request("GET", "/healthz", {}), timeout=1)[0] == 200
        except OSError:
            return False

    def log_tail(self) -> str:
        path = self.work.root / "gateway.log"
        return path.read_text(errors="replace")[-2000:] if path.exists() else ""

    def register(self, deployment: str, workflow: str, command: list[str], target: str = "local") -> None:
        body = json.dumps({
            "version": "v1", "workflow": workflow, "fingerprint": "", "target": target,
            "command": command, "secret_inputs": [],
        }).encode()
        headers = {"Content-Type": "application/json", "Authorization": f"Bearer {ADMIN_TOKEN}"}
        status, response = exchange(self.address, http_request("POST", f"/admin/v1/deployments/{deployment}/versions", headers, body))
        if not 200 <= status < 300:
            raise RuntimeError(f"registering {deployment}: {response[-500:]!r}")

    def rss_kib(self) -> int:
        return proc_status_kib(self.process.pid, "VmRSS")

    def peak_rss_kib(self) -> int:
        return proc_status_kib(self.process.pid, "VmHWM")

    def stop(self) -> None:
        if self.process and self.process.poll() is None:
            self.process.send_signal(signal.SIGTERM)
            try:
                self.process.wait(timeout=20)
            except subprocess.TimeoutExpired:
                self.process.kill()
        stop_agents(self.socket_dir)


def agent_pids(socket_dir: Path) -> set[int]:
    """Pids of the local provider's agents (from its `.pid` files)."""
    pids = set()
    for pid_file in socket_dir.glob("*.pid"):
        try:
            pids.add(int(pid_file.read_text().split()[0]))
        except (OSError, ValueError, IndexError):
            continue
    return pids


def stop_agents(socket_dir: Path, pids: set[int] | None = None) -> None:
    for pid in pids if pids is not None else agent_pids(socket_dir):
        try:
            os.killpg(pid, signal.SIGKILL)
        except (ProcessLookupError, PermissionError):
            pass


def start_fake_openai(work: Workspace, chunks: int, interval_ms: int) -> tuple[subprocess.Popen, int]:
    port = free_port()
    script = work.write("fake_openai.py", FAKE_OPENAI)
    process = subprocess.Popen([sys.executable, "-I", str(script), str(port), str(chunks), str(interval_ms)])
    wait_until(lambda: port_open(port), 10, "fake OpenAI server")
    return process, port


def port_open(port: int) -> bool:
    try:
        socket.create_connection(("127.0.0.1", port), timeout=0.2).close()
        return True
    except OSError:
        return False


# --------------------------------------------------------------------------- fake Kubernetes API


class FakeKubernetes(ThreadingHTTPServer):
    """The part of the Kubernetes API the k8s provider reads for one Deployment whose
    single pod is an agent the harness started on 127.0.0.1:`agent_port`."""

    daemon_threads = True

    def __init__(self, name: str, agent_port: int):
        self.name, self.agent_port = name, agent_port
        super().__init__(("127.0.0.1", 0), FakeKubernetesHandler)

    def pod(self) -> dict:
        return {
            "metadata": {"name": f"{self.name}-0", "labels": {"app": self.name}},
            "spec": {"containers": [{"name": "agent", "ports": [{"name": "http", "containerPort": self.agent_port}]}]},
            "status": {"phase": "Running", "podIP": "127.0.0.1", "conditions": [{"type": "Ready", "status": "True"}]},
        }

    def endpoint_slice(self) -> dict:
        return {
            "metadata": {"name": f"{self.name}-abc", "resourceVersion": "1"},
            "ports": [{"name": "http", "port": self.agent_port}],
            "endpoints": [{"addresses": ["127.0.0.1"], "conditions": {"ready": True},
                           "targetRef": {"kind": "Pod", "name": f"{self.name}-0"}}],
        }

    def scale(self) -> dict:
        return {"metadata": {"resourceVersion": "1"}, "spec": {"replicas": 1}, "status": {"replicas": 1}}


class FakeKubernetesHandler(BaseHTTPRequestHandler):
    protocol_version = "HTTP/1.1"
    server: FakeKubernetes

    def log_message(self, *args) -> None:
        pass

    def reply(self, body: dict) -> None:
        payload = json.dumps(body).encode()
        self.send_response(200)
        self.send_header("Content-Type", "application/json")
        self.send_header("Content-Length", str(len(payload)))
        self.end_headers()
        self.wfile.write(payload)

    def do_GET(self) -> None:
        path, _, query = self.path.partition("?")
        if path.endswith("/endpointslices") and "watch=1" in query:
            # Nothing changes: hold the watch, then end it empty.
            time.sleep(5)
            self.send_response(200)
            self.send_header("Content-Length", "0")
            self.end_headers()
        elif path.endswith("/endpointslices"):
            self.reply({"metadata": {"resourceVersion": "1"}, "items": [self.server.endpoint_slice()]})
        elif path.endswith("/pods"):
            self.reply({"metadata": {"resourceVersion": "1"}, "items": [self.server.pod()]})
        elif path.endswith("/scale"):
            self.reply(self.server.scale())
        else:
            self.reply({"metadata": {"name": self.server.name}, "spec": {"replicas": 1}, "status": {}})

    def do_PATCH(self) -> None:
        self.rfile.read(int(self.headers.get("Content-Length", 0)))
        self.reply(self.server.scale())


# --------------------------------------------------------------------------- scenarios


@dataclass
class Context:
    args: argparse.Namespace
    targets: dict
    root: Path

    def workspace(self, name: str) -> Workspace:
        """A fresh directory per call (rounds of a scenario never share sockets or stores)."""
        return Workspace(Path(tempfile.mkdtemp(prefix=f"{name}-", dir=self.root)))

    def samples(self, key: str, quick: int) -> int:
        return quick if self.args.quick else int(self.targets[key])


def measure_ttfb(gateway_address: Address, agent_address: Address, deployment: str, samples: int) -> dict:
    """Interleaved first-`run_started` times through the gateway and straight to the agent."""
    inputs = {"name": "Ada"}
    via_gateway, direct = [], []
    for i in range(samples + 50):
        gateway_request = client_run_request(deployment, {"stream": True, "inputs": inputs})
        run_id = f"direct-{i}-{secrets.token_hex(4)}"
        body = json.dumps({"workflow": "hello", "inputs": inputs, "event_index": 0}).encode()
        agent_request = signed_agent_request(deployment, run_id, 1, body)
        gateway_ms = time_to_marker(gateway_address, gateway_request, b"run_started")
        direct_ms = time_to_marker(agent_address, agent_request, b"run_started")
        if i >= 50:  # warm-up
            via_gateway.append(gateway_ms)
            direct.append(direct_ms)
    gateway_summary, direct_summary = summarize(via_gateway), summarize(direct)
    return {
        "gateway_ms": gateway_summary,
        "direct_ms": direct_summary,
        "overhead_p50_ms": round(gateway_summary["p50"] - direct_summary["p50"], 4),
        "overhead_p95_ms": round(gateway_summary["p95"] - direct_summary["p95"], 4),
    }


def scenario_ttfb_uds(ctx: Context) -> dict:
    work = ctx.workspace("ttfb-uds")
    workflow = work.write("hello.yaml", HELLO_WORKFLOW)
    pool = {"max_active_runs": 64, "min_replicas": 1, "idle_timeout_secs": 3600}
    gateway = Gateway(ctx.args.gateway, work, [deployment_entry("uds", pool=pool, quotas={"max_replicas": 1})]).start()
    try:
        gateway.register("uds", "hello", [str(ctx.args.agent), "serve", str(workflow)])
        # The first request wakes the replica.
        time_to_marker(gateway.address, client_run_request("uds", {"stream": True, "inputs": {}}), b"run_succeeded")
        sockets = sorted(gateway.socket_dir.glob("*.sock"))
        if len(sockets) != 1:
            raise RuntimeError(f"expected one agent socket, found {sockets}")
        return measure_ttfb(gateway.address, Address(unix=str(sockets[0])), "uds", ctx.samples("ttfb_samples", 200))
    finally:
        gateway.stop()


def scenario_ttfb_tcp(ctx: Context) -> dict:
    if not ctx.args.gateway_k8s:
        raise SkipScenario("needs --gateway-k8s (a gateway build with gateway-k8s)")
    work = ctx.workspace("ttfb-tcp")
    workflow = work.write("hello.yaml", HELLO_WORKFLOW)
    agent_port = free_port()
    env = {k: v for k, v in os.environ.items() if not k.startswith("GRAPHRT_")} | {"GRAPHRT_GATEWAY_KEYS": AGENT_KEYS}
    agent_log = open(work.root / "agent.log", "wb")
    agent = subprocess.Popen(
        [str(ctx.args.agent), "serve", str(workflow), "--listen", f"127.0.0.1:{agent_port}",
         "--trust-gateway", "--deployment", "tcp"], stdout=agent_log, stderr=agent_log, env=env,
    )
    fake = FakeKubernetes("agent-tcp", agent_port)
    threading.Thread(target=fake.serve_forever, daemon=True).start()
    kubeconfig = work.write("kubeconfig.json", json.dumps({
        "apiVersion": "v1", "kind": "Config", "current-context": "bench",
        "clusters": [{"name": "bench", "cluster": {"server": f"http://127.0.0.1:{fake.server_address[1]}"}}],
        "users": [{"name": "bench", "user": {"token": "bench"}}],
        "contexts": [{"name": "bench", "context": {"cluster": "bench", "user": "bench", "namespace": K8S_NAMESPACE}}],
    }))
    providers = {"cluster": {"kind": "k8s", "namespace": K8S_NAMESPACE, "kubeconfig": str(kubeconfig)}}
    pool = {"max_active_runs": 64, "min_replicas": 1, "idle_timeout_secs": 3600}
    entry = deployment_entry("tcp", pool=pool, quotas={"max_replicas": 1}, provider="cluster")
    gateway = Gateway(ctx.args.gateway_k8s, work, [entry], providers=providers).start()
    try:
        wait_until(lambda: port_open(agent_port), 10, "TCP agent")
        gateway.register("tcp", "hello", [], target=f"{K8S_NAMESPACE}/agent-tcp")
        time_to_marker(gateway.address, client_run_request("tcp", {"stream": True, "inputs": {}}), b"run_succeeded")
        result = measure_ttfb(gateway.address, Address(port=agent_port), "tcp", ctx.samples("ttfb_samples", 200))
        result["loopback_rtt_ms"] = round(loopback_rtt_ms(agent_port), 4)
        return result
    finally:
        gateway.stop()
        agent.kill()
        fake.shutdown()


def loopback_rtt_ms(port: int, samples: int = 200) -> float:
    """p50 of a TCP connect on loopback (one round trip)."""
    times = []
    for _ in range(samples):
        began = time.perf_counter_ns()
        socket.create_connection(("127.0.0.1", port)).close()
        times.append((time.perf_counter_ns() - began) / 1e6)
    return percentile(times, 0.5)


class SkipScenario(Exception):
    """A scenario that cannot run with the given binaries."""


@dataclass
class StreamResult:
    ok: bool
    chunks: int
    max_gap_ms: float
    first_ms: float
    error: str = ""


async def one_stream(port: int, request: bytes, expected_chunks: int) -> StreamResult:
    began = time.perf_counter()
    first_ms, max_gap_ms, chunks, tail = 0.0, 0.0, 0, b""
    try:
        reader, writer = await asyncio.open_connection("127.0.0.1", port)
        writer.write(request)
        await writer.drain()
        last = time.perf_counter()
        received = b""
        while data := await reader.read(65536):
            now = time.perf_counter()
            if b"node_chunk" in data or b"event: response" in data:
                if chunks == 0:
                    first_ms = (now - began) * 1000
                else:
                    max_gap_ms = max(max_gap_ms, (now - last) * 1000)
                last = now
            chunks += data.count(b"event: node_chunk")
            received += data[:512] if not received else b""
            tail = (tail + data)[-4096:]
        writer.close()
        ok = b"run_succeeded" in tail and chunks >= expected_chunks and received.startswith(b"HTTP/1.1 200")
        return StreamResult(ok, chunks, max_gap_ms, first_ms, "" if ok else (received[:200] + tail[-200:]).decode(errors="replace"))
    except (OSError, asyncio.IncompleteReadError) as error:
        return StreamResult(False, chunks, max_gap_ms, first_ms, repr(error))


async def run_streams(port: int, count: int, expected_chunks: int, sample_rss) -> list[StreamResult]:
    request = client_run_request("sse", {"stream": True, "inputs": {}})
    tasks = [asyncio.create_task(one_stream(port, request, expected_chunks)) for _ in range(count)]
    while not all(task.done() for task in tasks):
        sample_rss()
        await asyncio.sleep(0.1)
    return [task.result() for task in tasks]


def scenario_sse(ctx: Context) -> dict:
    streams = ctx.samples("sse_streams", 200)
    chunks, interval_ms = int(ctx.targets["sse_chunks"]), int(ctx.targets["sse_chunk_interval_ms"])
    work = ctx.workspace("sse")
    workflow = work.write("chat.yaml", STREAM_WORKFLOW)
    upstream, upstream_port = start_fake_openai(work, chunks, interval_ms)
    replicas = 4
    pool = {"max_active_runs": (streams + replicas - 1) // replicas, "min_replicas": replicas,
            "idle_timeout_secs": 3600, "queue_timeout_secs": 60}
    entry = deployment_entry("sse", pool=pool, quotas={"max_replicas": replicas, "max_queued": streams * 2})
    env = {"OPENAI_API_KEY": "bench", "OPENAI_BASE_URL": f"http://127.0.0.1:{upstream_port}/v1"}
    gateway = Gateway(ctx.args.gateway, work, [entry], env=env).start()
    try:
        gateway.register("sse", "chat", [str(ctx.args.agent), "serve", str(workflow)])
        time_to_marker(gateway.address, client_run_request("sse", {"stream": True, "inputs": {}}), b"run_succeeded")
        wait_until(lambda: len(list(gateway.socket_dir.glob("*.sock"))) >= replicas, 20, "warm replicas")
        idle_kib = gateway.rss_kib()
        samples: list[int] = []
        results = asyncio.run(run_streams(gateway.port, streams, chunks, lambda: samples.append(gateway.rss_kib())))
        failed = [r for r in results if not r.ok]
        stalls = [r for r in results if r.ok and r.max_gap_ms > float(ctx.targets["sse_max_gap_ms"])]
        return {
            "streams": streams,
            "chunks_per_stream": chunks,
            "chunk_interval_ms": interval_ms,
            "gateway_rss_before_mib": round(idle_kib / 1024, 2),
            "gateway_rss_max_sampled_mib": round(max(samples, default=0) / 1024, 2),
            "gateway_peak_rss_mib": round(gateway.peak_rss_kib() / 1024, 2),
            "failed_streams": len(failed),
            "failure_examples": [r.error for r in failed[:3]],
            "stalled_streams": len(stalls),
            "max_chunk_gap_ms": round(max((r.max_gap_ms for r in results), default=0), 1),
            "first_chunk_ms": summarize([r.first_ms for r in results if r.ok] or [0]),
        }
    finally:
        gateway.stop()
        upstream.kill()


def measure_wakes(ctx: Context, name: str, workflow_text: str, workflow_name: str, extra: list[str], samples: int) -> dict:
    """`samples` cold wakes, each of a fresh deployment (its first request spawns its first agent)."""
    work = ctx.workspace(name)
    workflow = work.write(f"{workflow_name}.yaml", workflow_text)
    warmup = 3
    pool = {"max_active_runs": 8, "min_replicas": 0, "idle_timeout_secs": 3600}
    names = [f"{name}-{i}" for i in range(samples + warmup)]
    gateway = Gateway(ctx.args.gateway, work, [deployment_entry(d, pool=pool) for d in names]).start()
    try:
        for deployment in names:
            gateway.register(deployment, workflow_name, [str(ctx.args.agent), "serve", str(workflow), *extra])
        first_event, finished = [], []
        for index, deployment in enumerate(names):
            before = agent_pids(gateway.socket_dir)
            started_ms, done_ms = timed_run(gateway, deployment)
            if index >= warmup:
                first_event.append(started_ms)
                finished.append(done_ms)
            # Free the agent at once (each deployment is used once), so 100 agents
            # (or 100 Node.js plugin hosts) never pile up in the VM's memory.
            stop_agents(gateway.socket_dir, agent_pids(gateway.socket_dir) - before)
        return {"first_event_ms": summarize(first_event), "run_finished_ms": summarize(finished)}
    finally:
        gateway.stop()


def timed_run(gateway: Gateway, deployment: str) -> tuple[float, float]:
    """A streaming run through the gateway: ms to `run_started` and to `run_succeeded`."""
    request = client_run_request(deployment, {"stream": True, "inputs": {}})
    began = time.perf_counter_ns()
    with gateway.address.connect() as sock:
        sock.sendall(request)
        received, started_ms = b"", None
        while chunk := sock.recv(65536):
            received += chunk
            if started_ms is None and b"run_started" in received:
                started_ms = (time.perf_counter_ns() - began) / 1e6
            if b"run_succeeded" in received:
                break
        done_ms = (time.perf_counter_ns() - began) / 1e6
    if started_ms is None or b"run_succeeded" not in received:
        raise RuntimeError(f"{deployment}: {received[-400:]!r}\n{gateway.log_tail()}")
    return started_ms, done_ms


def scenario_wake(ctx: Context) -> dict:
    return measure_wakes(ctx, "wake", HELLO_WORKFLOW, "hello", [], ctx.samples("wake_samples", 20))


def scenario_wake_node(ctx: Context) -> dict:
    if not shutil.which("node"):
        raise SkipScenario("node is not installed")
    plugins = ctx.workspace("wake-node").root / "plugins"
    plugins.mkdir(exist_ok=True)
    link = plugins / "upper"
    if not link.exists():
        link.symlink_to(ROOT / "plugins" / "examples" / "node-uppercase")
    result = measure_wakes(ctx, "wake-node", PLUGIN_WORKFLOW, "shout", ["--plugins", str(plugins)],
                           ctx.samples("wake_samples", 20))
    result["plugin_handshake_ms"] = summarize(plugin_handshakes(link, 30))
    return result


def node_plugins_dir(ctx: Context, name: str) -> Path:
    plugins = ctx.workspace(name).root / "plugins"
    plugins.mkdir(exist_ok=True)
    (plugins / "upper").symlink_to(ROOT / "plugins" / "examples" / "node-uppercase")
    return plugins


def process_tree(pid: int) -> list[int]:
    """`pid` and its descendants (from /proc/*/task/*/children)."""
    tree, pending = [], [pid]
    while pending:
        current = pending.pop()
        tree.append(current)
        for children in Path(f"/proc/{current}/task").glob("*/children"):
            try:
                pending += [int(child) for child in children.read_text().split()]
            except OSError:
                continue
    return tree


def tree_rss_kib(pid: int) -> int:
    total = 0
    for member in process_tree(pid):
        try:
            total += proc_status_kib(member, "VmRSS")
        except (OSError, KeyError):
            continue
    return total


def plugin_processes(agent: int) -> int:
    return len(process_tree(agent)) - 1


def warm_request(deployment: str) -> bytes:
    headers = {"Content-Type": "application/json", "X-Api-Key": API_KEY}
    return http_request("POST", f"/v1/deployments/{deployment}/warm", headers, b"{}")


def kept_warm_agent(ctx: Context, name: str, agent_args: list[str]) -> tuple[Gateway, int]:
    """A gateway with one deployment kept warm (`min_replicas: 1`) running the Node.js
    plugin workflow with `agent_args`, and its agent's pid."""
    work = ctx.workspace(name)
    workflow = work.write("shout.yaml", PLUGIN_WORKFLOW)
    plugins = node_plugins_dir(ctx, name)
    pool = {"max_active_runs": 8, "min_replicas": 1, "idle_timeout_secs": 3600}
    gateway = Gateway(ctx.args.gateway, work, [deployment_entry(name, pool=pool)]).start()
    gateway.register(name, "shout", [str(ctx.args.agent), "serve", str(workflow), "--plugins", str(plugins), *agent_args])
    timed_run(gateway, name)
    pids = agent_pids(gateway.socket_dir)
    if len(pids) != 1:
        raise RuntimeError(f"{name}: expected one agent, found {sorted(pids)}\n{gateway.log_tail()}")
    return gateway, pids.pop()


def settled_rss_mib(agent: int, plugins: int) -> float:
    wait_until(lambda: plugin_processes(agent) >= plugins, 15, f"{plugins} plugin processes")
    time.sleep(1.0)
    return round(tree_rss_kib(agent) / 1024, 2)


def scenario_plugin_pool(ctx: Context) -> dict:
    if not shutil.which("node"):
        raise SkipScenario("node is not installed")
    samples = ctx.samples("plugin_pool_samples", 5)
    rss = {}
    for size in (0, 1, 2):
        gateway, agent = kept_warm_agent(ctx, f"pool-{size}", ["--plugin-pool", str(size)] if size else [])
        try:
            rss[str(size)] = settled_rss_mib(agent, max(size, 1))
            if size == 1:
                warm = [timed_run(gateway, f"pool-{size}")[1] for _ in range(samples)]
        finally:
            gateway.stop()
    # Idle stop: the pool empties after 1 s without a run; then a run either starts the
    # plugin itself (cold) or finds it started by a warm hint sent `lead` before.
    lead = 0.3
    gateway, agent = kept_warm_agent(ctx, "pool-idle", ["--plugin-pool", "1", "--plugin-idle", "1s"])
    try:
        cold, hinted, agent_only = [], [], 0.0
        for index in range(2 * samples):
            wait_until(lambda: plugin_processes(agent) == 0, 15, "the idle stop of the plugin pool", 0.02)
            agent_only = round(tree_rss_kib(agent) / 1024, 2)
            if index % 2:
                status, body = exchange(gateway.address, warm_request("pool-idle"))
                if status != 200:
                    raise RuntimeError(f"warm: {status} {body[-300:]!r}")
                time.sleep(lead)
                hinted.append(timed_run(gateway, "pool-idle")[1])
            else:
                cold.append(timed_run(gateway, "pool-idle")[1])
    finally:
        gateway.stop()
    return {
        "rss_mib_by_pool_size": rss,
        "agent_only_rss_mib": agent_only,
        "warm_run_ms": summarize(warm),
        "cold_pool_run_ms": summarize(cold),
        "hinted_run_ms": summarize(hinted),
        "hint_lead_ms": lead * 1000,
    }


def plugin_handshakes(plugin_dir: Path, samples: int) -> list[float]:
    """Spawn of the Node.js plugin -> its `initialize` answer, without graphrt:
    the floor under a cold wake with that plugin on this machine."""
    request = json.dumps({"jsonrpc": "2.0", "id": "r-1", "method": "initialize",
                          "params": {"protocol_version": 1, "host": {"name": "gwbench", "version": "0"}}}) + "\n"
    times = []
    for _ in range(samples):
        began = time.perf_counter_ns()
        process = subprocess.Popen(["node", "plugin.mjs"], cwd=plugin_dir, stdin=subprocess.PIPE, stdout=subprocess.PIPE)
        process.stdin.write(request.encode())
        process.stdin.flush()
        process.stdout.readline()
        times.append((time.perf_counter_ns() - began) / 1e6)
        process.kill()
        process.wait()
    return times


def scenario_webhook(ctx: Context) -> dict:
    samples = ctx.samples("webhook_samples", 200)
    work = ctx.workspace("webhook")
    workflow = work.write("hello.yaml", HELLO_WORKFLOW)
    hook = {"id": "ingest", "scheme": "hmac_sha256", "secret_env": "BENCH_HOOK_SECRET", "idempotency_header": "idempotency-key"}
    pool = {"max_active_runs": 64, "min_replicas": 1, "idle_timeout_secs": 3600}
    gateway = Gateway(ctx.args.gateway, work, [deployment_entry("hooks", pool=pool, hooks=[hook])]).start()
    try:
        gateway.register("hooks", "hello", [str(ctx.args.agent), "serve", str(workflow)])
        times, statuses = [], {}
        for i in range(samples + 20):
            body = json.dumps({"event": "order.paid", "n": i}).encode()
            signature = hmac.new(HOOK_SECRET.encode(), body, hashlib.sha256).hexdigest()
            headers = {"Content-Type": "application/json", "X-Signature-256": f"sha256={signature}",
                       "Idempotency-Key": f"delivery-{i}-{secrets.token_hex(4)}"}
            request = http_request("POST", "/v1/deployments/hooks/hooks/ingest", headers, body)
            began = time.perf_counter_ns()
            with gateway.address.connect() as sock:
                sock.sendall(request)
                head = sock.recv(64)
                elapsed = (time.perf_counter_ns() - began) / 1e6
                while sock.recv(65536):
                    pass
            status = int(head.split(b" ", 2)[1])
            statuses[status] = statuses.get(status, 0) + 1
            if i >= 20:
                times.append(elapsed)
        if set(statuses) - {200, 202}:
            raise RuntimeError(f"webhook statuses {statuses}: {gateway.log_tail()}")
        return {"ack_ms": summarize(times), "statuses": statuses}
    finally:
        gateway.stop()


def scenario_cron(ctx: Context) -> dict:
    fires = ctx.samples("cron_fires", 100)
    work = ctx.workspace("cron")
    workflow = work.write("hello.yaml", HELLO_WORKFLOW)
    crons = [{"id": f"c{i}", "schedule": "* * * * *", "timezone": "UTC", "prewarm_secs": 5, "jitter_secs": 0}
             for i in range(fires)]
    pool = {"max_active_runs": 256, "min_replicas": 0, "idle_timeout_secs": 3600, "queue_timeout_secs": 60}
    entry = deployment_entry("cron", pool=pool, crons=crons, quotas={"max_replicas": 4, "max_queued": fires * 2})
    # Start more than prewarm_secs before a minute boundary, so the first fire is a full one.
    while 60 - time.time() % 60 < 15:
        time.sleep(1)
    gateway = Gateway(ctx.args.gateway, work, [entry]).start()
    try:
        gateway.register("cron", "hello", [str(ctx.args.agent), "serve", str(workflow)])
        fire_at = int(time.time() // 60 + 1) * 60
        store = work.root / "gateway.db"
        wait_until(lambda: finished_cron_runs(store, fire_at) >= fires, 120 + fire_at - time.time(),
                   f"{fires} cron runs", interval=0.5)
        created, finished = cron_lags(store, fire_at)
        return {"fires": fires, "fire_time": fire_at,
                "created_lag_ms": summarize(created), "finished_lag_ms": summarize(finished)}
    finally:
        gateway.stop()


def cron_rows(store: Path, fire_at: int) -> list[tuple[str, int, int]]:
    with sqlite3.connect(f"file:{store}?mode=ro", uri=True, timeout=5) as db:
        rows = db.execute("SELECT status, created_ms, updated_ms, record FROM runs WHERE run_id LIKE 'cron-%'").fetchall()
    return [(status, created, updated) for status, created, updated, record in rows
            if f'"cron.fire_time":"{fire_at}"' in record.replace(" ", "")]


def finished_cron_runs(store: Path, fire_at: int) -> int:
    try:
        return sum(1 for status, _, _ in cron_rows(store, fire_at) if status == "succeeded")
    except sqlite3.Error:
        return 0


def cron_lags(store: Path, fire_at: int) -> tuple[list[float], list[float]]:
    rows = [row for row in cron_rows(store, fire_at) if row[0] == "succeeded"]
    return [created - fire_at * 1000 for _, created, _ in rows], [updated - fire_at * 1000 for _, _, updated in rows]


# --------------------------------------------------------------------------- gating and report


@dataclass
class Check:
    scenario: str
    metric: str
    value: float
    budget: float
    unit: str
    gated: bool = True

    @property
    def ok(self) -> bool:
        return self.value <= self.budget


def checks_for(name: str, result: dict, targets: dict) -> list[Check]:
    t = targets
    if name == "ttfb-uds":
        return [Check(name, "overhead p50", result["overhead_p50_ms"], t["ttfb_overhead_uds_p50_ms"], "ms"),
                Check(name, "overhead p95", result["overhead_p95_ms"], t["ttfb_overhead_uds_p95_ms"], "ms")]
    if name == "ttfb-tcp":
        budget = t["ttfb_overhead_tcp_p95_ms"] + result["loopback_rtt_ms"]
        return [Check(name, "overhead p95 (budget + 1 RTT)", result["overhead_p95_ms"], round(budget, 4), "ms")]
    if name == "sse":
        return [Check(name, "gateway peak RSS", result["gateway_peak_rss_mib"], t["sse_rss_mib"], "MiB"),
                Check(name, "gateway peak RSS (audit target, not gated)", result["gateway_peak_rss_mib"],
                      t.get("sse_rss_target_mib", t["sse_rss_mib"]), "MiB", gated=False),
                Check(name, "failed streams", result["failed_streams"], 0, ""),
                Check(name, "stalled streams", result["stalled_streams"], 0, "")]
    if name == "wake":
        return [Check(name, "p95 to first event", result["first_event_ms"]["p95"], t["wake_p95_ms"], "ms")]
    if name == "wake-node":
        # The plugin node's result, not the first event: the Node.js host must be up.
        return [Check(name, "p95 to plugin result", result["run_finished_ms"]["p95"], t["wake_node_p95_ms"], "ms")]
    if name == "plugin-pool":
        return [Check(name, "p95 run after a warm hint", result["hinted_run_ms"]["p95"], t["plugin_pool_hinted_p95_ms"], "ms"),
                Check(name, "p95 run on a warm pool", result["warm_run_ms"]["p95"], t["plugin_pool_hinted_p95_ms"], "ms")]
    if name == "webhook":
        return [Check(name, "ack p95", result["ack_ms"]["p95"], t["webhook_ack_p95_ms"], "ms")]
    if name == "cron":
        return [Check(name, "lag p95 (fire -> run finished)", result["finished_lag_ms"]["p95"], t["cron_lag_p95_ms"], "ms")]
    return []


# Latency scenarios (and the 1,000-stream peak RSS, which varies by a few MiB with
# allocator timing) run `rounds` times and report the median round: sub-millisecond
# timings on a shared machine (a VM on a busy laptop, a CI runner) swing by 2x
# between rounds a few seconds apart. Every round's values are kept in the JSON.
ROUNDED = {"ttfb-uds", "ttfb-tcp", "sse", "wake", "wake-node", "webhook"}


def run_rounds(ctx: Context, name: str) -> tuple[dict, list[Check]]:
    rounds = 1 if ctx.args.quick or name not in ROUNDED else int(ctx.targets.get("rounds", 1))
    outcomes = []
    for _ in range(rounds):
        result = SCENARIO_FUNCTIONS[name](ctx)
        outcomes.append((result, checks_for(name, result, ctx.targets)))
    # The median round by its first (headline) check.
    outcomes.sort(key=lambda outcome: outcome[1][0].value if outcome[1] else 0)
    result, checks = outcomes[len(outcomes) // 2]
    if rounds > 1:
        result["rounds"] = [[check.value for check in round_checks] for _, round_checks in outcomes]
    return result, checks


def describe(name: str, result: dict) -> str:
    if "skipped" in result:
        return f"skipped: {result['skipped']}"
    if name.startswith("ttfb"):
        g, d = result["gateway_ms"], result["direct_ms"]
        return (f"gateway p50/p95 {g['p50']:.3f}/{g['p95']:.3f} ms, direct {d['p50']:.3f}/{d['p95']:.3f} ms, "
                f"n={g['n']}" + rounds_note(result))
    if name == "sse":
        return (f"{result['streams']} streams x {result['chunks_per_stream']} chunks, RSS idle "
                f"{result['gateway_rss_before_mib']} MiB, max chunk gap {result['max_chunk_gap_ms']} ms"
                + rounds_note(result))
    if name.startswith("wake"):
        e, f = result["first_event_ms"], result["run_finished_ms"]
        text = f"first event p50/p95 {e['p50']:.2f}/{e['p95']:.2f} ms, run finished p50/p95 {f['p50']:.2f}/{f['p95']:.2f} ms, n={e['n']}"
        if "plugin_handshake_ms" in result:
            h = result["plugin_handshake_ms"]
            text += f"; the plugin alone (spawn -> initialize, no graphrt) p50/p95 {h['p50']:.1f}/{h['p95']:.1f} ms"
        return text + rounds_note(result)
    if name == "plugin-pool":
        r = result["rss_mib_by_pool_size"]
        w, c, h = result["warm_run_ms"], result["cold_pool_run_ms"], result["hinted_run_ms"]
        return (f"RSS agent + plugins, pool 0/1/2: {r['0']}/{r['1']}/{r['2']} MiB, agent alone after the idle stop "
                f"{result['agent_only_rss_mib']} MiB; run p50/p95 warm {w['p50']:.2f}/{w['p95']:.2f} ms, after the idle "
                f"stop cold {c['p50']:.2f}/{c['p95']:.2f} ms vs. {result['hint_lead_ms']:g} ms after a warm hint "
                f"{h['p50']:.2f}/{h['p95']:.2f} ms, n={h['n']}")
    if name == "webhook":
        a = result["ack_ms"]
        text = f"ack p50 {a['p50']:.3f} ms, p99 {a['p99']:.3f} ms, n={a['n']}"
        return text + rounds_note(result)
    if name == "cron":
        c = result["created_lag_ms"]
        return f"{result['fires']} simultaneous fires; fire -> record created p50/p95 {c['p50']:.0f}/{c['p95']:.0f} ms"
    return ""


def rounds_note(result: dict) -> str:
    if "rounds" not in result:
        return ""
    values = ", ".join("/".join(f"{value:g}" for value in round_values) for round_values in result["rounds"])
    return f"; median of {len(result['rounds'])} rounds ({values})"


def report(results: dict, checks: list[Check], machine: str) -> str:
    lines = [f"### graphrt gateway targets ({machine})", "",
             "| Scenario | Metric | Value | Target | | Details |", "| --- | --- | ---: | ---: | --- | --- |"]
    for name, result in results.items():
        mine = [c for c in checks if c.scenario == name]
        if not mine:
            lines.append(f"| {name} | – | – | – | – | {describe(name, result)} |")
        for check in mine:
            verdict = "ok" if check.ok else ("**FAIL**" if check.gated else "not met")
            lines.append(f"| {name} | {check.metric} | {check.value:g} {check.unit} | ≤ {check.budget:g} {check.unit} "
                         f"| {verdict} | {describe(name, result)} |")
    failed = [c for c in checks if not c.ok and c.gated]
    lines += ["", "All targets met." if not failed else f"{len(failed)} target(s) missed."]
    return "\n".join(lines) + "\n"


def machine_description() -> str:
    cpu = ""
    try:
        cpu = next((line.split(":", 1)[1].strip() for line in Path("/proc/cpuinfo").read_text().splitlines()
                    if line.lower().startswith(("model name", "hardware"))), "")
    except OSError:
        pass
    return f"{os.uname().machine}, {os.cpu_count()} CPUs{', ' + cpu if cpu else ''}, Linux {os.uname().release}"


def parse_args() -> argparse.Namespace:
    parser = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    parser.add_argument("--gateway", type=Path, required=True, help="graphrt built with --features gateway")
    parser.add_argument("--gateway-k8s", type=Path, help="graphrt built with gateway,gateway-k8s (for ttfb-tcp)")
    parser.add_argument("--agent", type=Path, required=True, help="agent build with serve,llm,plugins,trust-gateway")
    parser.add_argument("--budgets", type=Path, default=ROOT / "bench" / "budgets.toml")
    parser.add_argument("--only", help=f"comma-separated scenarios ({', '.join(SCENARIOS)})")
    parser.add_argument("--quick", action="store_true", help="fewer samples (not for gating)")
    parser.add_argument("--json", type=Path)
    parser.add_argument("--markdown", type=Path)
    return parser.parse_args()


def main() -> int:
    args = parse_args()
    if not sys.platform.startswith("linux"):
        print("gwbench: Linux only (reads /proc)", file=sys.stderr)
        return 2
    args.gateway, args.agent = args.gateway.resolve(), args.agent.resolve()
    args.gateway_k8s = args.gateway_k8s.resolve() if args.gateway_k8s else None
    targets = tomllib.loads(args.budgets.read_text())["gateway_targets"]
    selected = args.only.split(",") if args.only else SCENARIOS
    unknown = set(selected) - set(SCENARIOS)
    if unknown:
        print(f"gwbench: unknown scenarios {sorted(unknown)}", file=sys.stderr)
        return 2
    raise_fd_limit()
    root = Path(tempfile.mkdtemp(prefix="gwbench-"))
    ctx = Context(args, targets, root)
    results: dict = {}
    checks: list[Check] = []
    try:
        for name in selected:
            print(f"gwbench: {name} ...", file=sys.stderr, flush=True)
            began = time.monotonic()
            try:
                results[name], round_checks = run_rounds(ctx, name)
            except SkipScenario as skip:
                results[name] = {"skipped": str(skip)}
                continue
            print(f"gwbench: {name} done in {time.monotonic() - began:.1f} s", file=sys.stderr, flush=True)
            checks += round_checks
    finally:
        shutil.rmtree(root, ignore_errors=True)
    text = report(results, checks, machine_description())
    print(text)
    if args.markdown:
        args.markdown.write_text(text)
    if args.json:
        payload = {"machine": machine_description(), "quick": args.quick, "results": results,
                   "checks": [c.__dict__ | {"ok": c.ok} for c in checks]}
        args.json.write_text(json.dumps(payload, indent=2) + "\n")
    if args.quick:
        return 0
    return 1 if any(not c.ok and c.gated for c in checks) else 0


SCENARIO_FUNCTIONS = {
    "ttfb-uds": scenario_ttfb_uds,
    "ttfb-tcp": scenario_ttfb_tcp,
    "sse": scenario_sse,
    "wake": scenario_wake,
    "wake-node": scenario_wake_node,
    "plugin-pool": scenario_plugin_pool,
    "webhook": scenario_webhook,
    "cron": scenario_cron,
}

if __name__ == "__main__":
    sys.exit(main())

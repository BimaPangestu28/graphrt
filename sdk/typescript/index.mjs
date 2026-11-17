// graphrt plugin SDK for JavaScript / TypeScript (Node.js >= 18, no dependencies).
//
// The plugin speaks newline-delimited JSON-RPC 2.0 over stdin/stdout.
// Never write to stdout yourself (console.log); use ctx.log() or console.error().

import { createInterface } from "node:readline";

export const PROTOCOL_VERSION = 1;

/** Error reported to the host as a failed node. */
export class PluginError extends Error {
  /**
   * @param {string} message
   * @param {{kind?: string, retryable?: boolean, retryAfterMs?: number}} [options]
   */
  constructor(message, options = {}) {
    super(message);
    this.name = "PluginError";
    this.kind = options.kind ?? "plugin_error";
    this.retryable = options.retryable ?? false;
    this.retryAfterMs = options.retryAfterMs;
  }
}

/** Build a result that pauses the run. */
export function pause(kind, data = null, state = null) {
  return { pause: { kind, data, state } };
}

const RESULT_KEYS = new Set(["outputs", "branch", "writes", "usage", "metadata", "pause"]);

function normalizeResult(result) {
  if (result === undefined || result === null) return { outputs: {} };
  if (typeof result !== "object" || Array.isArray(result)) {
    throw new PluginError("node handlers must return an object", { kind: "plugin_protocol" });
  }
  const keys = Object.keys(result);
  if (keys.length > 0 && keys.every((k) => RESULT_KEYS.has(k))) return result;
  return { outputs: result };
}

function errorData(e) {
  if (e instanceof PluginError) {
    const data = { kind: e.kind, retryable: e.retryable };
    if (e.retryAfterMs !== undefined) data.retry_after_ms = e.retryAfterMs;
    return data;
  }
  return { kind: "plugin_exception" };
}

/** Per-request helpers. */
export class RunContext {
  constructor(plugin, requestId) {
    this._plugin = plugin;
    this.requestId = requestId;
    this._controller = new AbortController();
  }

  /** Stream text for output `field`. */
  chunk(field, text) {
    this._plugin._notify("node.chunk", { request_id: this.requestId, field, chunk: String(text) });
  }

  /** Report structured progress. */
  progress(name, data = null) {
    this._plugin._notify("node.progress", { request_id: this.requestId, name, data });
  }

  /** Send a log line to the host. */
  log(message, level = "info") {
    this._plugin._notify("log", { level, message: String(message) });
  }

  /** AbortSignal fired when the host cancels this request (pass it to fetch, timers...). */
  get signal() {
    return this._controller.signal;
  }

  get cancelled() {
    return this._controller.signal.aborted;
  }
}

/** A set of node kinds and tools served over the graphrt plugin protocol. */
export class Plugin {
  /** @param {string} name */
  constructor(name) {
    this.name = name;
    this._nodes = new Map();
    this._descriptors = [];
    this._tools = new Map();
    this._running = new Map();
  }

  /**
   * Register a node handler `async (ctx, req) => result`.
   * The result is `{outputs, branch?, writes?, usage?, metadata?}`, a plain object
   * (taken as the outputs) or `pause(...)`. Throw `PluginError` to fail.
   */
  node(kind, handler, descriptor) {
    this._nodes.set(kind, handler);
    if (descriptor) this._descriptors.push({ kind, ...descriptor });
    return this;
  }

  /** Register a tool `async (args) => string | {text, data?, is_error?}`. */
  tool(spec, handler) {
    this._tools.set(spec.name, { spec: { description: "", parameters: { type: "object" }, ...spec }, handler });
    return this;
  }

  /** Serve on stdin/stdout until stdin closes or the host sends `shutdown`. */
  run({ input = process.stdin, output = process.stdout } = {}) {
    this._output = output;
    const rl = createInterface({ input, crlfDelay: Infinity, terminal: false });
    return new Promise((resolve) => {
      let shutdown = false;
      const finish = () => {
        // `shutdown` stops in-flight work; end of input lets it finish and reply.
        if (shutdown) {
          for (const entry of this._running.values()) {
            entry.cancelled = true;
            entry.ctx._controller.abort();
          }
          this._running.clear();
        }
        resolve();
      };
      rl.on("line", (line) => {
        const text = line.trim();
        if (!text) return;
        let msg;
        try {
          msg = JSON.parse(text);
        } catch {
          this._send({ jsonrpc: "2.0", id: null, error: { code: -32700, message: "parse error" } });
          return;
        }
        if (!this._dispatch(msg)) {
          shutdown = true;
          rl.close();
        }
      });
      rl.on("close", finish);
    });
  }

  _dispatch(msg) {
    const { id, method } = msg;
    const params = msg.params ?? {};
    if (method === "shutdown") return false;
    if (id === undefined || id === null) {
      if (method === "node.cancel") {
        const entry = this._running.get(params.request_id);
        if (entry) {
          entry.ctx._controller.abort();
          entry.cancelled = true;
          this._running.delete(params.request_id);
        }
      }
      return true;
    }
    switch (method) {
      case "initialize": {
        const result = { protocol_version: PROTOCOL_VERSION, name: this.name };
        if (this._descriptors.length) result.nodes = this._descriptors;
        if (this._tools.size) result.tools = [...this._tools.values()].map((t) => t.spec);
        this._reply(id, result);
        break;
      }
      case "node.run":
        this._startRun(id, params);
        break;
      case "tool.list":
        this._reply(id, { tools: [...this._tools.values()].map((t) => t.spec) });
        break;
      case "tool.call":
        this._callTool(id, params);
        break;
      default:
        this._error(id, -32601, `method \`${method}\` not found`);
    }
    return true;
  }

  _startRun(id, params) {
    const kind = params.kind ?? "";
    const handler = this._nodes.get(kind);
    if (!handler) {
      this._error(id, -32602, `plugin \`${this.name}\` has no node kind \`${kind}\``, { kind: "unknown_kind" });
      return;
    }
    const requestId = String(params.request_id ?? id);
    const resume = params.resume && typeof params.resume === "object"
      ? { state: params.resume.state ?? null, input: params.resume.input ?? null }
      : null;
    const req = {
      requestId,
      kind,
      nodeId: params.node_id ?? "",
      attempt: params.attempt ?? 1,
      config: params.config ?? {},
      resume,
    };
    const ctx = new RunContext(this, requestId);
    const entry = { ctx, cancelled: false };
    this._running.set(requestId, entry);
    Promise.resolve()
      .then(() => handler(ctx, req))
      .then(
        (result) => {
          if (!entry.cancelled) this._reply(id, normalizeResult(result));
        },
        (e) => {
          if (!entry.cancelled) this._error(id, -32000, e instanceof Error ? e.message : String(e), errorData(e));
        },
      )
      .finally(() => this._running.delete(requestId));
  }

  async _callTool(id, params) {
    const tool = this._tools.get(params.name);
    if (!tool) {
      this._error(id, -32602, `plugin \`${this.name}\` has no tool \`${params.name}\``, { kind: "unknown_tool" });
      return;
    }
    try {
      let result = await tool.handler(params.arguments ?? {});
      if (typeof result === "string") result = { text: result };
      this._reply(id, { text: "", is_error: false, ...(result ?? {}) });
    } catch (e) {
      this._error(id, -32000, e instanceof Error ? e.message : String(e), errorData(e));
    }
  }

  _send(msg) {
    this._output.write(JSON.stringify(msg) + "\n");
  }

  _notify(method, params) {
    this._send({ jsonrpc: "2.0", method, params });
  }

  _reply(id, result) {
    this._send({ jsonrpc: "2.0", id, result });
  }

  _error(id, code, message, data) {
    const error = { code, message };
    if (data !== undefined) error.data = data;
    this._send({ jsonrpc: "2.0", id, error });
  }
}

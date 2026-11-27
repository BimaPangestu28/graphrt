/**
 * Starts a real `graphrt serve` for the tests of the web packages.
 *
 * The binary is `$GRAPHRT_BIN`, else the debug build of this repository
 * (`cargo build -p graphrt-cli`).
 */
import { type ChildProcess, spawn } from "node:child_process";
import { existsSync } from "node:fs";
import { mkdtemp, rm, writeFile } from "node:fs/promises";
import { createServer } from "node:net";
import { tmpdir } from "node:os";
import { join, resolve } from "node:path";

// The tests run bundled from another directory: scripts/test.mjs names the repository.
const repoRoot = process.env.GRAPHRT_REPO_ROOT ?? resolve(process.cwd(), "..");

/** Workflows that need no model: every test runs offline. */
const WORKFLOWS: Record<string, string> = {
  // Offered to chat guests; the message is a start input.
  support: `version: "1"
name: support
graph:
  nodes:
    - id: start
      type: start
      variables:
        - { variable: message, type: text-input, required: true }
    - { id: hello, type: answer, answer: "You said " }
    - { id: echo, type: answer, answer: "{{#start.message#}}" }
    - id: end
      type: end
      outputs:
        - { variable: echoed, value_selector: [start, message] }
  edges:
    - { source: start, target: hello }
    - { source: hello, target: echo }
    - { source: echo, target: end }
`,
  // Keeps a conversation: the message is sys.query, the answer shows the history.
  memo: `version: "1"
name: memo
graph:
  nodes:
    - { id: start, type: start }
    - { id: done, type: answer, answer: "{{#sys.query#}} after {{#sys.history#}}" }
  edges:
    - { source: start, target: done }
`,
  // Pauses for a person.
  confirm: `version: "1"
name: confirm
graph:
  nodes:
    - { id: start, type: start }
    - id: ask
      type: human-input
      prompt: "Send it?"
      form:
        fields:
          - { name: note, label: Note, type: text }
      actions:
        - { id: "yes", title: "Yes" }
        - { id: "no", title: "No" }
    - { id: sent, type: answer, answer: "sent ({{#ask.note#}})" }
    - { id: kept, type: answer, answer: "not sent" }
  edges:
    - { source: start, target: ask }
    - { source: ask, target: sent, handle: "yes" }
    - { source: ask, target: kept, handle: "no" }
`,
  // Says something, then takes a few seconds: long enough to stop it.
  slow: `version: "1"
name: slow
graph:
  nodes:
    - { id: start, type: start }
    - { id: first, type: answer, answer: "working" }
    - id: wait
      type: code
      code_language: javascript
      code: |
        function main() {
          const started = Date.now();
          while (Date.now() - started < 4000) {}
          return { done: true };
        }
      outputs:
        done: { type: boolean }
    - id: end
      type: end
      outputs:
        - { variable: done, value_selector: [wait, done] }
  edges:
    - { source: start, target: first }
    - { source: first, target: wait }
    - { source: wait, target: end }
`,
  // Returns UI blocks in its outputs.
  report: `version: "1"
name: report
graph:
  nodes:
    - { id: start, type: start }
    - id: blocks
      type: code
      code_language: javascript
      code: |
        function main() {
          return { stats: { "$ui": "stats", version: 1, title: "This week",
            items: [{ label: "Orders", value: 42 }] } };
        }
      outputs:
        stats: { type: object }
    - { id: say, type: answer, answer: "Here are the numbers." }
    - id: end
      type: end
      outputs:
        - { variable: stats, value_selector: [blocks, stats] }
  edges:
    - { source: start, target: blocks }
    - { source: blocks, target: say }
    - { source: say, target: end }
`,
};

export const API_KEY = "test-api-key";

export interface TestServer {
  /** `http://127.0.0.1:<port>`. */
  url: string;
  /** Stop the server and remove its files. */
  stop(): Promise<void>;
}

function binary(): string {
  const path = process.env.GRAPHRT_BIN ?? join(repoRoot, "target", "debug", "graphrt");
  if (!existsSync(path)) {
    throw new Error(`no graphrt binary at ${path}: run \`cargo build -p graphrt-cli\` or set GRAPHRT_BIN`);
  }
  return path;
}

function freePort(): Promise<number> {
  return new Promise((resolvePort, reject) => {
    const probe = createServer();
    probe.once("error", reject);
    probe.listen(0, "127.0.0.1", () => {
      const address = probe.address();
      probe.close(() => {
        if (address && typeof address === "object") {
          resolvePort(address.port);
        } else {
          reject(new Error("no port"));
        }
      });
    });
  });
}

async function waitUntilHealthy(url: string, child: ChildProcess, log: () => string): Promise<void> {
  const deadline = Date.now() + 15_000;
  while (Date.now() < deadline) {
    if (child.exitCode !== null) {
      throw new Error(`graphrt serve exited with ${child.exitCode}:\n${log()}`);
    }
    try {
      const response = await fetch(`${url}/healthz`);
      if (response.ok) {
        return;
      }
    } catch {
      // Not listening yet.
    }
    await new Promise((done) => setTimeout(done, 50));
  }
  throw new Error(`graphrt serve did not become healthy:\n${log()}`);
}

/**
 * Start `graphrt serve` with the test workflows and `flags`.
 *
 * @param flags - Extra `serve` flags, e.g. `["--conversations"]`.
 * @throws Error - When the binary is missing or the server does not start.
 */
export async function startServer(flags: string[] = []): Promise<TestServer> {
  const dir = await mkdtemp(join(tmpdir(), "graphrt-web-test-"));
  for (const [name, text] of Object.entries(WORKFLOWS)) {
    await writeFile(join(dir, `${name}.yaml`), text);
  }
  const port = await freePort();
  const child = spawn(binary(), ["serve", dir, "--addr", `127.0.0.1:${port}`, "--no-default-plugins", ...flags], {
    env: { ...process.env, GRAPHRT_API_KEY: API_KEY },
    stdio: ["ignore", "ignore", "pipe"],
  });
  let stderr = "";
  child.stderr?.on("data", (chunk: Buffer) => {
    stderr += chunk.toString();
  });
  const url = `http://127.0.0.1:${port}`;
  const stop = async (): Promise<void> => {
    if (child.exitCode === null) {
      const exited = new Promise((done) => child.once("exit", done));
      child.kill("SIGKILL");
      await exited;
    }
    await rm(dir, { recursive: true, force: true });
  };
  try {
    await waitUntilHealthy(url, child, () => stderr);
  } catch (error) {
    await stop();
    throw error;
  }
  return { url, stop };
}

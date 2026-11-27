/**
 * The monitoring session against a real `graphrt serve` (see
 * test-support/server.ts) and a local push service.
 */
import assert from "node:assert/strict";
import { after, before, describe, test } from "node:test";
import { type PushService, startPushService, TestBrowser } from "../../../../test-support/push-service.ts";
import { API_KEY, startServer, type TestServer } from "../../../../test-support/server.ts";
import { GraphrtClient, type PushSubscriptionKeys } from "../../src/client/index.ts";
import { notificationOf } from "../../src/monitor/notification.ts";
import {
  eventLine,
  type KeyValueStore,
  MonitorSession,
  type PushBrowser,
  type RunDetail,
} from "../../src/monitor/session.ts";

function memoryStorage(): KeyValueStore & { entries: Map<string, string> } {
  const entries = new Map<string, string>();
  return {
    entries,
    getItem: (key) => entries.get(key) ?? null,
    setItem: (key, value) => void entries.set(key, value),
    removeItem: (key) => void entries.delete(key),
  };
}

/** The open run, read through a call so an earlier check does not narrow it. */
function detailOf(session: MonitorSession): RunDetail | undefined {
  return session.state.detail;
}

async function until(what: string, condition: () => boolean): Promise<void> {
  for (let attempt = 0; attempt < 400; attempt += 1) {
    if (condition()) {
      return;
    }
    await new Promise((done) => setTimeout(done, 25));
  }
  assert.fail(`timed out waiting for ${what}`);
}

/** A browser whose push subscription is `browser`'s, granted when asked. */
function pushBrowser(browser: TestBrowser): PushBrowser & { asked: string[] } {
  let subscribed: PushSubscriptionKeys | undefined;
  let permission: "default" | "granted" = "default";
  const asked: string[] = [];
  return {
    asked,
    supported: () => true,
    needsInstall: () => false,
    permission: () => permission,
    subscription: () => Promise.resolve(subscribed),
    subscribe(publicKey) {
      asked.push(publicKey);
      permission = "granted";
      subscribed = browser.subscription();
      return Promise.resolve(subscribed);
    },
    unsubscribe() {
      subscribed = undefined;
      return Promise.resolve();
    },
  };
}

describe("monitoring a server", () => {
  let server: TestServer;
  let pushService: PushService;
  let owner: GraphrtClient;

  before(async () => {
    pushService = await startPushService();
    server = await startServer([
      "--monitor",
      "--push",
      "mailto:ops@example.com",
      // The push service of this test is a local, plain http address.
      "--allow-private-network",
    ]);
    owner = new GraphrtClient({ baseUrl: server.url, token: API_KEY });
  });
  after(async () => {
    await server.stop();
    await pushService.stop();
  });

  /** Start `workflow` in the background and wait until it is in `status`. */
  async function startUntil(workflow: string, status: string): Promise<string> {
    const { run_id } = await owner.startAsync(workflow, { sys: { query: "go" } });
    for (let attempt = 0; attempt < 400; attempt += 1) {
      const record = await owner.getRun(run_id);
      if (record.status === status && record.active === false) {
        return run_id;
      }
      await new Promise((done) => setTimeout(done, 25));
    }
    return assert.fail(`run ${run_id} never became ${status}`);
  }

  test("the page is locked until a key is entered, and a wrong key stays out", async () => {
    const storage = memoryStorage();
    const session = new MonitorSession({ baseUrl: server.url, storage });
    await session.start();
    assert.equal(session.state.status, "locked");
    assert.deepEqual(session.state.config, {
      access: "key",
      devices: true,
      approvals: true,
      expire_approvals: false,
      push: true,
    });
    assert.equal(await session.unlock("not-the-key", true), false);
    assert.equal(session.state.status, "locked");
    assert.match(session.state.notice ?? "", /not accepted/);
    assert.equal(storage.entries.size, 0, "a refused key is not remembered");
    assert.equal(await session.unlock(API_KEY, true), true);
    assert.equal(session.state.status, "ready");
    assert.equal(session.state.role, "owner");
    assert.ok(session.canReadRuns && session.canResolve);
    // The next visit needs no key.
    const again = new MonitorSession({ baseUrl: server.url, storage });
    await again.start();
    assert.equal(again.state.status, "ready");
    again.signOut();
    assert.equal(again.state.status, "locked");
    assert.equal(storage.entries.size, 0);
  });

  test("runs are listed, filtered and opened with what they did", async () => {
    const done = await startUntil("report", "succeeded");
    await startUntil("confirm", "paused");
    const session = new MonitorSession({ baseUrl: server.url });
    await session.start();
    await session.unlock(API_KEY);
    assert.ok(session.state.runs.length >= 2);
    assert.equal(session.state.runs[0]?.workflow, "confirm", "newest first");
    await session.setFilter("succeeded");
    assert.ok(session.state.runs.length >= 1);
    assert.ok(session.state.runs.every((run) => run.status === "succeeded"));
    await session.setFilter(undefined);

    await session.openRun(done);
    const detail = session.state.detail;
    assert.ok(detail);
    assert.equal(detail.record?.answer, "Here are the numbers.");
    assert.equal(detail.live, false);
    const types = detail.events.map((line) => line.type);
    assert.equal(types[0], "run_started");
    assert.equal(types.at(-1), "run_succeeded");
    assert.equal(detail.events.at(-1)?.tone, "good");
    assert.ok(detail.events.some((line) => line.type === "node_started" && line.node === "blocks"));
    session.closeRun();
    assert.equal(session.state.detail, undefined);
    await session.openRun("run-unknown");
    assert.ok(detailOf(session)?.error);
  });

  test("a run that executes is followed until it ends, and the owner can stop it", async () => {
    const session = new MonitorSession({ baseUrl: server.url });
    await session.start();
    await session.unlock(API_KEY);
    const { run_id } = await owner.startAsync("slow", {});
    const opened = session.openRun(run_id);
    await until("the run to be followed", () => session.state.detail?.live === true);
    await until("its answer so far", () => session.state.detail?.liveAnswer === "working");
    assert.equal(await session.stopRun(run_id), true);
    await opened;
    const detail = session.state.detail;
    assert.equal(detail?.live, false);
    assert.equal(detail?.record?.status, "aborted");
    assert.equal(detail?.events.at(-1)?.type, "run_aborted");
    // Every event once, in order.
    const indices = detail?.events.map((line) => line.index) ?? [];
    assert.deepEqual(
      indices,
      [...indices].sort((a, b) => a - b),
    );
    assert.equal(new Set(indices).size, indices.length);
  });

  test("a paired device reads runs and answers what waits, and nothing else", async () => {
    const runId = await startUntil("confirm", "paused");
    const ownerSession = new MonitorSession({ baseUrl: server.url });
    await ownerSession.start();
    await ownerSession.unlock(API_KEY);
    assert.equal(await ownerSession.pairDevice("  "), false, "a device needs a name");
    assert.ok(ownerSession.state.deviceError);
    assert.equal(await ownerSession.pairDevice("Ana's phone"), true);
    const paired = ownerSession.state.paired;
    assert.ok(paired);
    assert.equal(paired.link, `${server.url}/monitor/#pair=${paired.token}`);
    assert.deepEqual(
      ownerSession.state.devices?.map((device) => device.name),
      ["Ana's phone"],
    );
    ownerSession.dismissPaired();
    assert.equal(ownerSession.state.paired, undefined);

    // The phone opens the pairing link.
    const storage = memoryStorage();
    const phone = new MonitorSession({ baseUrl: server.url, storage });
    await phone.start(paired.token);
    assert.equal(phone.state.status, "ready");
    assert.equal(phone.state.role, "device");
    assert.equal(phone.state.device?.name, "Ana's phone");
    assert.equal(storage.entries.size, 1, "a paired device stays paired");
    assert.equal(phone.state.devices, undefined, "a device never lists devices");
    assert.equal(await phone.pairDevice("another"), false);
    const waiting = phone.state.approvals.find((approval) => approval.run_id === runId);
    assert.ok(waiting, "the inbox shows the run that waits");
    const request = waiting.requests[0];
    assert.equal(request?.key, "ask");
    assert.deepEqual((request?.data as { prompt?: string } | undefined)?.prompt, "Send it?");

    // It cannot stop a run, and it can answer the question.
    assert.equal(await phone.stopRun(runId), false);
    assert.equal(
      await phone.resolve(runId, "ask", { action: "yes", values: { note: "from my phone" } }),
      true,
    );
    assert.ok(!phone.state.approvals.some((approval) => approval.run_id === runId));
    await until("the run to finish", () => {
      void phone.refresh();
      return phone.state.runs.find((run) => run.run_id === runId)?.status === "succeeded";
    });
    assert.equal(phone.state.runs.find((run) => run.run_id === runId)?.answer, "sent (from my phone)");
    // Answered already.
    assert.equal(await phone.resolve(runId, "ask", { action: "yes", values: {} }), false);
    assert.match(phone.state.resolveError ?? "", /already answered/);

    // The owner removes the device: its next request signs it out.
    const device = ownerSession.state.devices?.[0];
    assert.ok(device);
    await ownerSession.revokeDevice(device.id);
    assert.deepEqual(ownerSession.state.devices, []);
    await phone.refresh();
    assert.equal(phone.state.status, "locked");
    assert.equal(storage.entries.size, 0);
    assert.match(phone.state.notice ?? "", /signed out/);
  });

  test("a subscribed browser is told when a run waits, in a message only it can read", async () => {
    const browser = new TestBrowser(`${pushService.url}/push/phone`);
    const push = pushBrowser(browser);
    const session = new MonitorSession({ baseUrl: server.url, push });
    await session.start();
    await session.unlock(API_KEY);
    assert.deepEqual(
      { available: session.state.push.available, subscribed: session.state.push.subscribed },
      { available: true, subscribed: false },
    );
    assert.equal(await session.testPush(), false, "nothing to test before subscribing");
    assert.equal(await session.enablePush(), true);
    assert.equal(session.state.push.subscribed, true);
    assert.equal(push.asked.length, 1, "the browser subscribed with the server's key");
    assert.equal(Buffer.from(push.asked[0] ?? "", "base64url").length, 65);

    const before = pushService.received.length;
    assert.equal(await session.testPush(), true);
    const [test] = (await pushService.waitFor(before + 1)).slice(before);
    assert.ok(test);
    assert.equal(test.path, "/push/phone");
    assert.equal(test.headers["content-encoding"], "aes128gcm");
    assert.match(String(test.headers.authorization), /^vapid t=[\w-]+\.[\w-]+\.[\w-]+, k=[\w-]+$/);
    assert.equal(notificationOf(browser.decrypt(test.body)).open, "#/settings");

    const runId = await startUntil("confirm", "paused");
    const [, pushed] = (await pushService.waitFor(before + 2)).slice(before);
    assert.ok(pushed);
    assert.equal(pushed.headers.urgency, "high");
    assert.ok(!pushed.body.includes("Send it?"), "the push service cannot read the message");
    const shown = notificationOf(browser.decrypt(pushed.body));
    assert.deepEqual(shown, {
      title: "Approval needed: confirm",
      body: "Send it?",
      tag: runId,
      open: `#/runs/${runId}`,
    });

    await session.disablePush();
    assert.equal(session.state.push.subscribed, false);
    await startUntil("confirm", "paused");
    await new Promise((done) => setTimeout(done, 300));
    assert.equal(pushService.received.length, before + 2, "an unsubscribed browser hears nothing");
  });

  test("a browser that cannot receive push says why", async () => {
    const unsupported: PushBrowser = {
      supported: () => false,
      needsInstall: () => true,
      permission: () => "default",
      subscription: () => Promise.resolve(undefined),
      subscribe: () => Promise.reject(new Error("unsupported")),
      unsubscribe: () => Promise.resolve(),
    };
    const session = new MonitorSession({ baseUrl: server.url, push: unsupported });
    await session.start();
    await session.unlock(API_KEY);
    assert.deepEqual(
      { available: session.state.push.available, needsInstall: session.state.push.needsInstall },
      { available: false, needsInstall: true },
    );
    assert.equal(await session.enablePush(), false);
  });
});

describe("a server without a monitoring page", () => {
  let server: TestServer;
  before(async () => {
    server = await startServer([]);
  });
  after(() => server.stop());

  test("the session says so", async () => {
    const session = new MonitorSession({ baseUrl: server.url });
    await session.start();
    assert.equal(session.state.status, "unavailable");
    assert.equal(session.state.notice, "This server has no monitoring page.");
  });
});

test("events become lines of a log, without the text deltas", () => {
  assert.equal(eventLine({ type: "node_chunk", node_id: "llm", text: "Hel" }, 3), undefined);
  assert.deepEqual(eventLine({ type: "node_started", node_id: "kb", kind: "knowledge-retrieval" }, 1), {
    index: 1,
    type: "node_started",
    node: "kb",
    detail: "knowledge-retrieval",
    tone: "plain",
  });
  assert.deepEqual(eventLine({ type: "node_failed", node_id: "http", error: "timeout" }, 4), {
    index: 4,
    type: "node_failed",
    node: "http",
    detail: "timeout",
    tone: "bad",
  });
  assert.equal(eventLine({ type: "run_paused", reasons: [] }, 9)?.tone, "waiting");
  assert.equal(eventLine({ type: "run_started", resumed: true }, 10)?.detail, "resumed");
  assert.equal(eventLine({ type: "run_aborted", reason: "aborted via API" }, 11)?.detail, "aborted via API");
});

test("a push message becomes a notification that opens its run", () => {
  assert.deepEqual(
    notificationOf(
      JSON.stringify({ kind: "run_failed", title: "Run failed: sync", body: "timeout", run_id: "run a/1" }),
    ),
    { title: "Run failed: sync", body: "timeout", tag: "run a/1", open: "#/runs/run%20a%2F1" },
  );
  // Browsers require a notification for every push, also for one that cannot be read.
  for (const unreadable of [undefined, "", "not json", "[]", "{}"]) {
    assert.deepEqual(notificationOf(unreadable), {
      title: "Your agent needs you",
      body: "",
      tag: "graphrt",
      open: "#/runs",
    });
  }
});

/**
 * The monitoring view in a DOM (happy-dom), against a real `graphrt serve`.
 */
import "../../../test-support/dom.ts";
import assert from "node:assert/strict";
import { after, before, test } from "node:test";
import { GraphrtClient } from "@graphrt/client";
import { API_KEY, startServer, type TestServer } from "../../../test-support/server.ts";
import {
  ago,
  defineGraphrtMonitor,
  encodeQr,
  type GraphrtMonitorElement,
  qrSvg,
  timeLeft,
} from "../src/index.ts";

let server: TestServer;
let owner: GraphrtClient;

before(async () => {
  server = await startServer(["--monitor"]);
  owner = new GraphrtClient({ baseUrl: server.url, token: API_KEY });
  defineGraphrtMonitor();
});

after(() => server.stop());

async function until(what: string, condition: () => boolean): Promise<void> {
  for (let attempt = 0; attempt < 400; attempt += 1) {
    if (condition()) {
      return;
    }
    await new Promise((done) => setTimeout(done, 25));
  }
  assert.fail(`timed out waiting for ${what}`);
}

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

function mount(): { monitor: GraphrtMonitorElement; root: ShadowRoot } {
  const monitor = document.createElement("graphrt-monitor") as GraphrtMonitorElement;
  monitor.setAttribute("server", server.url);
  document.body.append(monitor);
  assert.ok(monitor.shadowRoot);
  return { monitor, root: monitor.shadowRoot };
}

function view(root: ShadowRoot, index: number): HTMLElement {
  const section = root.querySelectorAll("main > section.view")[index];
  assert.ok(section);
  return section as HTMLElement;
}

function press(scope: ParentNode, label: string): void {
  const control = [...scope.querySelectorAll("button")].find((button) =>
    button.textContent.startsWith(label),
  );
  assert.ok(control, `a button "${label}"`);
  control.click();
}

test("the key is asked for, then the runs, the inbox and a run are shown", async () => {
  const done = await startUntil("report", "succeeded");
  const waits = await startUntil("confirm", "paused");
  const { monitor, root } = mount();
  await until("the sign-in", () => root.querySelector("form.gate") !== null);
  assert.equal(
    (root.querySelector("nav") as HTMLElement).hidden,
    true,
    "nothing to navigate before signing in",
  );
  (root.querySelector('input[type="password"]') as HTMLInputElement).value = API_KEY;
  (root.querySelector("form.gate") as HTMLFormElement).dispatchEvent(
    new Event("submit", { cancelable: true }),
  );
  await until("the runs", () => view(root, 0).querySelectorAll(".row").length >= 2);

  // The runs, newest first, with their state in words.
  const [runs, approvals, run, settings] = [0, 1, 2, 3].map((index) => view(root, index));
  assert.ok(runs && approvals && run && settings);
  const first = runs.querySelector(".row");
  assert.equal(first?.querySelector(".name")?.textContent, "confirm");
  assert.equal(first?.querySelector(".pill")?.textContent, "Waiting");
  assert.equal(root.querySelector(".badge")?.textContent, "1", "one run waits");

  // The inbox: the question as a form, answered in place.
  monitor.show({ view: "approvals" });
  assert.equal(approvals.hidden, false);
  const card = approvals.querySelector("graphrt-block")?.shadowRoot;
  assert.ok(card);
  assert.ok(card.textContent.includes("Send it?"));
  // A refresh keeps the form the person is typing in.
  (card.querySelector("input") as HTMLInputElement).value = "on my way";
  await monitor.session?.refresh();
  assert.equal(approvals.querySelector("graphrt-block")?.shadowRoot, card);
  assert.equal((card.querySelector("input") as HTMLInputElement).value, "on my way");
  press(card, "Yes");
  await until("the inbox to empty", () => approvals.querySelector(".empty") !== null);
  assert.equal((root.querySelector(".badge") as HTMLElement).hidden, true);
  assert.equal((await startUntilDone(waits)).answer, "sent (on my way)");

  // One run: its answer, its UI blocks and what it did.
  monitor.show({ view: "run", runId: done });
  await until("the run's log", () => run.querySelectorAll("ul.events li").length > 3);
  assert.equal(run.querySelector(".answer")?.textContent, "Here are the numbers.");
  assert.ok(run.querySelector("graphrt-block")?.shadowRoot?.textContent.includes("Orders"));
  assert.equal(run.querySelector("ul.events li:last-child .type")?.textContent, "run_succeeded");
  assert.equal(run.querySelector(".pill")?.textContent, "Done");
  press(run, "‹ Runs");
  assert.equal(runs.hidden, false);

  // Settings: pair a device and get its link as text and as a QR code.
  monitor.show({ view: "settings" });
  await until("the devices", () => settings.querySelector('input[type="text"]') !== null);
  (settings.querySelector('input[type="text"]') as HTMLInputElement).value = "Ana's <b>phone</b>";
  (settings.querySelector("form.form") as HTMLFormElement).dispatchEvent(
    new Event("submit", { cancelable: true }),
  );
  await until("the pairing", () => settings.querySelector(".pairing") !== null);
  const link = settings.querySelector(".pairing .link")?.textContent ?? "";
  assert.match(link, new RegExp(`^${server.url}/monitor/#pair=grd1\\.dev-[0-9a-f]{16}\\.[\\w-]{43}$`));
  assert.ok(settings.querySelector(".pairing svg path")?.getAttribute("d")?.startsWith("M"));
  assert.equal(settings.querySelector(".pairing b"), null, "a device name is text");
  press(settings, "Done");
  await until(
    "the device in the list",
    () => settings.querySelector(".pairing") === null && settings.textContent.includes("Ana's <b>phone</b>"),
  );
  assert.ok(!settings.textContent.includes("grd1."), "the token is shown once");
  press(settings, "Remove");
  await until("the device to go", () => !settings.textContent.includes("Ana's"));
  press(settings, "Sign out");
  assert.ok(root.querySelector("form.gate"));
  monitor.remove();
});

async function startUntilDone(runId: string) {
  for (let attempt = 0; attempt < 400; attempt += 1) {
    const record = await owner.getRun(runId);
    if (record.status !== "running" && record.status !== "paused") {
      return record;
    }
    await new Promise((done) => setTimeout(done, 25));
  }
  return assert.fail(`run ${runId} never ended`);
}

test("a paired device opens the link and sees only what it may do", async () => {
  const paired = await owner.devices.pair({ name: "wall display", scopes: ["runs:read"] });
  const monitor = document.createElement("graphrt-monitor") as GraphrtMonitorElement;
  monitor.setAttribute("server", server.url);
  monitor.pairingToken = paired.token;
  document.body.append(monitor);
  const root = monitor.shadowRoot;
  assert.ok(root);
  await until("the device to be in", () => monitor.session?.state.status === "ready");
  const tabs = [...root.querySelectorAll("nav button")] as HTMLButtonElement[];
  assert.deepEqual(
    tabs.map((tab) => [tab.textContent.replace(/\d+$/, ""), tab.hidden]),
    [
      ["Runs", false],
      ["Approvals", true],
      ["Settings", false],
    ],
  );
  // It has no inbox: asking for it shows the runs.
  monitor.show({ view: "approvals" });
  assert.equal(view(root, 1).hidden, true);
  assert.equal(view(root, 0).hidden, false);
  monitor.show({ view: "settings" });
  const settings = view(root, 3);
  assert.ok(settings.textContent.includes('Paired as "wall display"'));
  assert.equal(settings.querySelector('input[type="text"]'), null, "a device pairs no devices");
  press(settings, "Unpair this device");
  await until("the device to be out", () => monitor.session?.state.status === "locked");
  await assert.rejects(new GraphrtClient({ baseUrl: server.url, token: paired.token }).devices.me());
  monitor.remove();
});

test("times are short and a QR code is dark on white", () => {
  const now = 1_000_000_000;
  assert.equal(ago(now - 10_000, now), "just now");
  assert.equal(ago(now - 5 * 60_000, now), "5 min ago");
  assert.equal(ago(now - 3 * 3_600_000, now), "3 h ago");
  assert.equal(ago(now - 3 * 86_400_000, now), "3 d ago");
  assert.equal(timeLeft(now - 1, now), undefined);
  assert.equal(timeLeft(now + 30_000, now), "30 s");
  assert.equal(timeLeft(now + 86_400_000, now), "24 h");
  const svg = qrSvg(encodeQr("https://agent.example.com/monitor/"), "QR code");
  assert.equal(svg.getAttribute("viewBox"), "0 0 37 37", "version 3 with a quiet zone of four modules");
  assert.equal(svg.querySelector("rect")?.getAttribute("fill"), "#ffffff");
  assert.equal(svg.querySelector("path")?.getAttribute("fill"), "#000000");
});

/**
 * The console view in a DOM (happy-dom), against a real `graphrt serve
 * --console` and a fake identity provider.
 */
import "../../../../test-support/dom.ts";
import assert from "node:assert/strict";
import { after, before, test } from "node:test";
import {
  consoleServer,
  cookieJar,
  type FakeProvider,
  signIn,
  startProvider,
} from "../../../../test-support/oidc.ts";
import { startServer, type TestServer } from "../../../../test-support/server.ts";
import { actorOf, defineGraphrtConsole, type GraphrtConsoleElement, tabOf } from "../../src/console/index.ts";

let provider: FakeProvider;
let server: TestServer;
const realFetch = globalThis.fetch;

before(async () => {
  provider = await startProvider();
  server = await startServer([], consoleServer(provider));
  defineGraphrtConsole();
});

after(async () => {
  globalThis.fetch = realFetch;
  await server?.stop();
  await provider?.stop();
});

async function until(what: string, condition: () => boolean): Promise<void> {
  for (let attempt = 0; attempt < 400; attempt += 1) {
    if (condition()) {
      return;
    }
    await new Promise((done) => setTimeout(done, 25));
  }
  assert.fail(`timed out waiting for ${what}`);
}

/** Mount a console whose browser is `jar` (its cookies are the sign in). */
function mount(jar: typeof fetch): { element: GraphrtConsoleElement; root: ShadowRoot } {
  globalThis.fetch = jar;
  const element = document.createElement("graphrt-console") as GraphrtConsoleElement;
  element.setAttribute("server", server.url);
  document.body.append(element);
  assert.ok(element.shadowRoot);
  return { element, root: element.shadowRoot };
}

function visibleTabs(root: ShadowRoot): string[] {
  return [...root.querySelectorAll("nav button")]
    .filter((tab) => !(tab as HTMLElement).hidden)
    .map((tab) => tab.textContent ?? "");
}

const VIEWS = ["agents", "runs", "secrets", "people", "audit"];

function view(root: ShadowRoot, name: string): HTMLElement {
  const section = root.querySelectorAll("main > section.view")[VIEWS.indexOf(name)];
  assert.ok(section);
  return section as HTMLElement;
}

function press(scope: ParentNode, label: string): void {
  const control = [...scope.querySelectorAll("button")].find((button) =>
    button.textContent?.startsWith(label),
  );
  assert.ok(control, `a button "${label}"`);
  control.click();
}

test("someone not signed in is offered the sign in", async () => {
  const { element, root } = mount(cookieJar());
  await until("the sign in", () => root.querySelector(".gate a") !== null);
  const link = root.querySelector(".gate a") as HTMLAnchorElement;
  assert.equal(link.getAttribute("href"), `${server.url}/console/login?return_to=%2Fconsole%2F`);
  assert.equal((root.querySelector("nav") as HTMLElement).hidden, true);
  element.remove();
});

test("an owner sees every view and sets a secret", async () => {
  const jar = cookieJar();
  await signIn(server.url, provider, jar, { sub: "o1", email: "owner@example.com" });
  const { element, root } = mount(jar);
  await until("the agents", () => root.querySelector("article.card") !== null);
  assert.deepEqual(visibleTabs(root), ["Agents", "Runs", "Secrets", "People", "Audit log"]);
  const names = [...root.querySelectorAll("article.card .name")].map((name) => name.textContent);
  assert.ok(names.includes("support") && names.includes("memo"), names.join(", "));
  assert.ok(root.querySelector("form.card h2")?.textContent?.includes("Upload"), "the upload form");
  assert.match(root.querySelector(".who")?.textContent ?? "", /owner@example\.com.*owner/);

  element.show("secrets");
  const secrets = view(root, "secrets");
  await until("the secret form", () => secrets.querySelector('input[type="password"]') !== null);
  const [name, value] = [...secrets.querySelectorAll<HTMLInputElement>("form.card input")];
  assert.ok(name && value);
  name.value = "OPENAI_API_KEY";
  value.value = "sk-test";
  press(secrets.querySelector("form.card") as HTMLElement, "Set");
  await until("the secret in the list", () =>
    (secrets.querySelector("table")?.textContent ?? "").includes("OPENAI_API_KEY"),
  );
  assert.ok(!(root.textContent ?? "").includes("sk-test"));
  assert.equal(value.value, "", "the value is cleared once set");

  element.show("audit");
  await until("the audit log", () =>
    (view(root, "audit").querySelector("table")?.textContent ?? "").includes("secret.set"),
  );
  element.show("runs");
  await until("the monitoring view", () => root.querySelector("graphrt-monitor") !== null);
  element.remove();
});

test("a viewer sees what a viewer may", async () => {
  const jar = cookieJar();
  await signIn(server.url, provider, jar, { sub: "v1", email: "viewer@example.com", groups: ["staff"] });
  const { element, root } = mount(jar);
  await until("the agents", () => root.querySelector("article.card") !== null);
  assert.deepEqual(visibleTabs(root), ["Agents", "Runs"]);
  assert.equal(root.querySelector("form.card"), null, "no upload form");
  assert.equal(
    [...root.querySelectorAll("button")].some((b) => b.textContent?.startsWith("Promote")),
    false,
  );
  element.show("people");
  assert.equal(
    root.querySelector("nav button[aria-current]")?.textContent,
    "Agents",
    "a view the role does not reach gives way to the agents",
  );
  element.remove();
});

test("routes and actors read well", () => {
  assert.equal(tabOf("#/secrets"), "secrets");
  assert.equal(tabOf("#/nope"), "agents");
  assert.equal(tabOf(""), "agents");
  const entry = { seq: 1, at_ms: 0, action: "x", outcome: "ok", prev: "", hash: "" };
  assert.equal(actorOf({ ...entry, actor: { kind: "api-key" } }), "API key");
  assert.equal(actorOf({ ...entry, actor: { kind: "user", id: "1", email: "a@b.c" } }), "a@b.c");
  assert.equal(actorOf({ ...entry, actor: { kind: "device", id: "d1" } }), "device d1");
});

/**
 * The console session against a real `graphrt serve --console` (see
 * test-support/server.ts) and a fake identity provider.
 */
import assert from "node:assert/strict";
import { after, before, describe, test } from "node:test";
import {
  consoleServer,
  cookieJar,
  type FakeProvider,
  signIn,
  startProvider,
} from "../../../../test-support/oidc.ts";
import { startServer, type TestServer } from "../../../../test-support/server.ts";
import { sessionFetch } from "../../src/client/index.ts";
import { ConsoleSession } from "../../src/console/session.ts";
import { MonitorSession } from "../../src/monitor/index.ts";

const MEMO_V2 = `version: "1"
name: memo
graph:
  nodes:
    - { id: start, type: start }
    - { id: done, type: answer, answer: "memo v2" }
  edges:
    - { source: start, target: done }
`;

describe("the admin console", () => {
  let provider: FakeProvider;
  let server: TestServer;

  before(async () => {
    provider = await startProvider();
    server = await startServer([], consoleServer(provider));
  });

  after(async () => {
    await server?.stop();
    await provider?.stop();
  });

  async function signedIn(person: { sub: string; email?: string; groups?: string[] }) {
    const jar = cookieJar();
    const callback = await signIn(server.url, provider, jar, person);
    assert.equal(callback.status, 303, await callback.text());
    const session = new ConsoleSession({ baseUrl: server.url, fetch: jar, restartTimeoutMs: 30_000 });
    await session.start();
    return { jar, session };
  }

  test("someone not signed in is sent to sign in", async () => {
    const session = new ConsoleSession({ baseUrl: server.url, fetch: cookieJar() });
    await session.start();
    assert.equal(session.state.status, "signed-out");
    assert.equal(session.signInUrl(), `${server.url}/console/login?return_to=%2Fconsole%2F`);
  });

  test("a viewer reads and cannot change anything", async () => {
    const { session } = await signedIn({ sub: "v1", email: "viewer@example.com", groups: ["staff"] });
    assert.equal(session.state.status, "ready");
    assert.equal(session.state.role, "viewer");
    assert.ok(session.can("viewer") && !session.can("operator"));
    assert.deepEqual(session.state.workflows?.served.map((workflow) => workflow.name).sort(), [
      "confirm",
      "diagram",
      "memo",
      "report",
      "shop",
      "slow",
      "support",
    ]);
    assert.equal(await session.setSecret("TOKEN", "x"), false);
    assert.equal(session.state.outcome?.tone, "bad");
    await session.loadSecrets();
    assert.equal(session.state.secrets, undefined, "a viewer does not even ask");
  });

  test("an owner sets secrets and versions, promotes, gives roles and reads the audit log", async () => {
    const viewer = await signedIn({ sub: "v2", email: "second@example.com", groups: ["staff"] });
    const { session } = await signedIn({ sub: "o1", email: "owner@example.com" });
    assert.equal(session.state.role, "owner");

    assert.equal(
      await session.setSecret("GREETING", "hello from the console"),
      true,
      session.state.outcome?.text,
    );
    assert.deepEqual(
      session.state.secrets?.secrets.map((secret) => secret.name),
      ["GREETING"],
    );
    assert.ok(!JSON.stringify(session.state.secrets).includes("hello from the console"));

    assert.equal(await session.upload("memo", "graph: {nodes: [{id: a, type: nope}]}"), false);
    assert.match(session.state.outcome?.text ?? "", /does not compile/);
    assert.equal(
      await session.upload("memo", MEMO_V2, { note: "shorter" }),
      true,
      session.state.outcome?.text,
    );
    const memo = session.state.workflows?.registry.find((workflow) => workflow.name === "memo");
    assert.equal(memo?.environments.dev?.active, 1);
    assert.deepEqual(session.state.workflows?.pending, ["secrets", "workflow:memo"]);
    assert.equal(await session.promote("memo", "dev"), true, session.state.outcome?.text);
    assert.equal(
      session.state.workflows?.registry.find((workflow) => workflow.name === "memo")?.environments.prod
        ?.active,
      1,
    );

    await session.loadPeople();
    assert.ok(session.state.members?.members.some((member) => member.email === "second@example.com"));
    assert.equal(await session.setRole("v2", "operator"), true, session.state.outcome?.text);
    await viewer.session.start();
    assert.equal(viewer.session.state.role, "operator");

    await session.loadAudit({ action: "secret." });
    assert.deepEqual(
      session.state.audit.entries.map((entry) => entry.action),
      ["secret.set"],
    );
    await session.loadAudit();
    const actions = session.state.audit.entries.map((entry) => entry.action);
    for (const action of ["auth.sign_in", "registry.upload", "registry.promote", "member.role"]) {
      assert.ok(actions.includes(action), `${action} in ${actions.join(", ")}`);
    }
    await session.verifyAudit();
    assert.equal(session.state.audit.verification?.broken_at_seq ?? null, null);
    assert.match(session.auditExportUrl(), /\/v1\/console\/audit\/export$/);

    // Apply: the server restarts with the secret and version 1 of `memo`.
    assert.equal(await session.apply(), true, session.state.outcome?.text);
    assert.deepEqual(session.state.workflows?.pending, []);
    assert.equal(session.state.workflows?.served.find((workflow) => workflow.name === "memo")?.version, 1);
  });

  test("the monitoring view works with a console sign in", async () => {
    const { jar, session } = await signedIn({ sub: "op1", email: "ops@example.com", groups: ["ops"] });
    const csrf = session.state.me?.csrf_token ?? "";
    const monitor = new MonitorSession({
      baseUrl: server.url,
      fetch: sessionFetch(csrf, jar),
      member: { role: "operator", name: "ops@example.com" },
    });
    await monitor.start();
    assert.equal(monitor.state.status, "ready");
    assert.equal(monitor.state.role, "member");
    assert.ok(monitor.canReadRuns && monitor.canResolve && monitor.canOperate);
    assert.equal(monitor.state.listError, undefined);
    const viewer = new MonitorSession({
      baseUrl: server.url,
      fetch: sessionFetch(csrf, jar),
      member: { role: "viewer", name: "someone" },
    });
    await viewer.start();
    assert.ok(viewer.canReadRuns && !viewer.canResolve && !viewer.canOperate);
  });

  test("signing out ends the session", async () => {
    const { jar, session } = await signedIn({ sub: "o2", email: "owner@example.com" });
    await session.signOut();
    assert.equal(session.state.status, "signed-out");
    const again = new ConsoleSession({ baseUrl: server.url, fetch: jar });
    await again.start();
    assert.equal(again.state.status, "signed-out");
  });
});

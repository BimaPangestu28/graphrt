/**
 * `@graphrt-dev/graphrt/console`: the admin console of a self hosted `graphrt serve
 * --console`, as a web component.
 *
 * - `<graphrt-console>` shows the agents and their versions per environment,
 *   runs and approvals, secrets, people and roles, and the audit log, as far
 *   as the signed in person's role reaches.
 * - `ConsoleSession` is the logic without a view.
 *
 * A server started with `--console` serves it ready to use at `/console/`.
 */
export { actorOf, defineGraphrtConsole, GraphrtConsoleElement, tabOf } from "./console-element.ts";
export {
  type AuditState,
  ConsoleSession,
  type ConsoleSessionOptions,
  type ConsoleState,
  type ConsoleTab,
  describe,
  type Outcome,
} from "./session.ts";

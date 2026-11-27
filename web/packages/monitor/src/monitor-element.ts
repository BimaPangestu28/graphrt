/**
 * `<graphrt-monitor>`: runs, what a run did, and the requests that wait for a
 * person, of one `graphrt serve`. Made for a phone, fine on a desktop.
 *
 * @example
 * <graphrt-monitor server="https://agent.example.com"></graphrt-monitor>
 *
 * Attributes: `server` (default: the page's origin), `theme` (`light` or
 * `dark`; absent follows the page) and `hash-routing` (keeps the open view in
 * the URL fragment, for a page that is this element only).
 *
 * Properties, set before the element is connected: `push` (the browser's push
 * notifications, see `browserPush`) and `pairingToken` (a device token from a
 * pairing link).
 */
import { questionBlock, richText } from "@graphrt/chat";
import type { Approval, ApprovalRequest, Json, RunRecord, RunStatus } from "@graphrt/client";
import {
  adoptStyles,
  type BlockAction,
  collectUiBlocks,
  defineGraphrtUi,
  type GraphrtBlockElement,
} from "@graphrt/ui";
import { encodeQr, qrSvg } from "./qr.ts";
import {
  type EventLine,
  type KeyValueStore,
  MonitorSession,
  type MonitorState,
  type PushBrowser,
} from "./session.ts";
import { MONITOR_CSS } from "./styles.ts";

type Child = Node | string | false | null | undefined;

function h<K extends keyof HTMLElementTagNameMap>(
  tag: K,
  attributes: Record<string, string> = {},
  ...children: Child[]
): HTMLElementTagNameMap[K] {
  const element = document.createElement(tag);
  for (const [name, value] of Object.entries(attributes)) {
    element.setAttribute(name, value);
  }
  for (const child of children) {
    if (child !== false && child !== null && child !== undefined) {
      element.append(child);
    }
  }
  return element;
}

/**
 * Make `children` (without the absent ones) what `parent` holds. A node that
 * is already in its place is not touched, so a form the person is typing in
 * keeps its text and its focus.
 */
function fill(parent: Element, ...children: Child[]): void {
  const wanted = children
    .filter((child): child is Node | string => child !== false && child != null)
    .map((child) => (typeof child === "string" ? document.createTextNode(child) : child));
  wanted.forEach((child, index) => {
    const current = parent.childNodes[index];
    if (current !== child) {
      parent.insertBefore(child, current ?? null);
    }
  });
  while (parent.childNodes.length > wanted.length) {
    parent.lastChild?.remove();
  }
}

function button(label: string, kind: string, onPress: () => void): HTMLButtonElement {
  const element = h("button", { type: "button", class: kind }, label);
  element.addEventListener("click", onPress);
  return element;
}

/** The browser's storage, or none where it is blocked. */
function browserStorage(): KeyValueStore | undefined {
  try {
    return globalThis.localStorage;
  } catch {
    return undefined;
  }
}

const STATUS_LABELS: Record<string, string> = {
  running: "Running",
  paused: "Waiting",
  succeeded: "Done",
  failed: "Failed",
  aborted: "Stopped",
  queued: "Queued",
};

const FILTERS: { label: string; status: RunStatus | undefined }[] = [
  { label: "All", status: undefined },
  { label: "Running", status: "running" },
  { label: "Waiting", status: "paused" },
  { label: "Failed", status: "failed" },
  { label: "Done", status: "succeeded" },
];

/** How long ago `ms` was, in a few characters. */
export function ago(ms: number, now = Date.now()): string {
  const seconds = Math.max(0, Math.round((now - ms) / 1000));
  if (seconds < 45) {
    return "just now";
  }
  const minutes = Math.round(seconds / 60);
  if (minutes < 60) {
    return `${minutes} min ago`;
  }
  const hours = Math.round(minutes / 60);
  if (hours < 24) {
    return `${hours} h ago`;
  }
  return `${Math.round(hours / 24)} d ago`;
}

/** How long until `ms`, in a few characters; `undefined` once it passed. */
/**
 * What happens at the time limit of `request`, in one sentence; `undefined`
 * for a request without one.
 *
 * @param enforced - Whether the server carries out `on_expire` (`--expire-approvals`).
 *   Without it nothing happens at the limit: the run keeps waiting.
 */
export function expiryNote(
  request: ApprovalRequest,
  enforced: boolean,
  now = Date.now(),
): string | undefined {
  if (typeof request.expires_ms !== "number") {
    return undefined;
  }
  const left = timeLeft(request.expires_ms, now);
  if (!enforced) {
    return left === undefined
      ? "The time to answer has passed. The run still waits."
      : `An answer is expected within ${left}. The run waits after that too.`;
  }
  const data = request.data;
  const fields = typeof data === "object" && data !== null && !Array.isArray(data) ? data : {};
  const onExpire = fields.on_expire;
  const rule = typeof onExpire === "object" && onExpire !== null && !Array.isArray(onExpire) ? onExpire : {};
  const actionId = rule.kind === "answer" && typeof rule.action === "string" ? rule.action : undefined;
  if (actionId === undefined) {
    return left === undefined
      ? "The time to answer has passed: the run stops shortly."
      : `Without an answer the run stops in ${left}.`;
  }
  let title = actionId;
  for (const action of Array.isArray(fields.actions) ? fields.actions : []) {
    if (typeof action === "object" && action !== null && !Array.isArray(action) && action.id === actionId) {
      title = typeof action.title === "string" && action.title !== "" ? action.title : actionId;
    }
  }
  return left === undefined
    ? `The time to answer has passed: answered with "${title}" shortly.`
    : `Without an answer it is answered with "${title}" in ${left}.`;
}

export function timeLeft(ms: number, now = Date.now()): string | undefined {
  const seconds = Math.round((ms - now) / 1000);
  if (seconds <= 0) {
    return undefined;
  }
  if (seconds < 90) {
    return `${seconds} s`;
  }
  const minutes = Math.round(seconds / 60);
  if (minutes < 90) {
    return `${minutes} min`;
  }
  const hours = Math.round(minutes / 60);
  return hours < 48 ? `${hours} h` : `${Math.round(hours / 24)} d`;
}

function pill(status: string): HTMLElement {
  return h("span", { class: `pill ${status}` }, STATUS_LABELS[status] ?? status);
}

type Route = { view: "runs" | "approvals" | "settings" } | { view: "run"; runId: string };

function parseRoute(hash: string): Route {
  const path = hash.replace(/^#\/?/, "");
  const [first, second] = path.split("/");
  if (first === "runs" && second !== undefined && second !== "") {
    return { view: "run", runId: decodeURIComponent(second) };
  }
  if (first === "approvals" || first === "settings") {
    return { view: first };
  }
  return { view: "runs" };
}

function routeHash(route: Route): string {
  return route.view === "run" ? `#/runs/${encodeURIComponent(route.runId)}` : `#/${route.view}`;
}

interface Parts {
  title: HTMLElement;
  main: HTMLElement;
  nav: HTMLElement;
  tabs: Record<"runs" | "approvals" | "settings", HTMLButtonElement>;
  badge: HTMLElement;
  gate: HTMLElement;
  gateNotice: HTMLElement;
  runs: HTMLElement;
  approvals: HTMLElement;
  run: HTMLElement;
  settings: HTMLElement;
}

const BaseElement: typeof HTMLElement = globalThis.HTMLElement ?? (class {} as typeof HTMLElement);

export class GraphrtMonitorElement extends BaseElement {
  static readonly observedAttributes = ["theme"];
  /** Push notifications of this browser. Set it before the element is connected. */
  push: PushBrowser | undefined;
  /** A device token from a pairing link. Set it before the element is connected. */
  pairingToken: string | undefined;
  #session: MonitorSession | undefined;
  #unsubscribe: (() => void) | undefined;
  #parts: Parts | undefined;
  #route: Route = { view: "runs" };
  #last: MonitorState | undefined;
  /** Cards of requests that are shown, by `<run id>/<key>`, kept while they do not change. */
  #cards = new Map<string, { signature: string; element: HTMLElement }>();
  /** The cards of runs that wait, by view and run id. */
  #articles = new Map<string, HTMLElement>();
  /** What the open run's view was last drawn from. */
  #drawnRun: { runId: string; lines: number; lastIndex: number; record: RunRecord | undefined } | undefined;
  #onHashChange = (): void => this.#go(parseRoute(location.hash), false);
  #onVisibility = (): void => this.#pollWhileVisible();

  /** The session behind the view, once the element is connected. */
  get session(): MonitorSession | undefined {
    return this.#session;
  }

  connectedCallback(): void {
    if (this.#parts === undefined) {
      defineGraphrtUi();
      this.#build();
    }
    if (this.hasAttribute("hash-routing")) {
      this.#route = parseRoute(location.hash);
      window.addEventListener("hashchange", this.#onHashChange);
    }
    document.addEventListener("visibilitychange", this.#onVisibility);
    if (this.#session === undefined) {
      const server = this.getAttribute("server");
      const storage = browserStorage();
      const session = new MonitorSession({
        ...(server === null ? {} : { baseUrl: server }),
        ...(storage === undefined ? {} : { storage }),
        ...(this.push === undefined ? {} : { push: this.push }),
      });
      this.#session = session;
      this.#unsubscribe = session.subscribe((state) => this.#render(state));
      this.#render(session.state);
      void session.start(this.pairingToken).then(() => {
        this.#pollWhileVisible();
        if (this.#route.view === "run") {
          void session.openRun(this.#route.runId);
        }
      });
    }
  }

  disconnectedCallback(): void {
    window.removeEventListener("hashchange", this.#onHashChange);
    document.removeEventListener("visibilitychange", this.#onVisibility);
    this.#session?.stopPolling();
    this.#session?.closeRun();
    this.#unsubscribe?.();
    this.#unsubscribe = undefined;
    this.#session = undefined;
    this.#last = undefined;
    this.#cards.clear();
    this.#articles.clear();
    this.#drawnRun = undefined;
  }

  attributeChangedCallback(_name: string, _old: string | null, value: string | null): void {
    for (const block of this.shadowRoot?.querySelectorAll("graphrt-block") ?? []) {
      if (value === null) {
        block.removeAttribute("theme");
      } else {
        block.setAttribute("theme", value);
      }
    }
  }

  /** Show a view: the runs, the inbox, the settings, or one run. */
  show(route: Route): void {
    this.#go(route, true);
  }

  #pollWhileVisible(): void {
    const session = this.#session;
    if (session === undefined) {
      return;
    }
    if (document.visibilityState === "hidden") {
      session.stopPolling();
    } else {
      void session.refresh();
      session.startPolling();
    }
  }

  #go(route: Route, updateHash: boolean): void {
    const session = this.#session;
    const before = this.#route;
    this.#route = route;
    if (updateHash && this.hasAttribute("hash-routing") && location.hash !== routeHash(route)) {
      // Fires `hashchange`, which comes back here with the same route.
      location.hash = routeHash(route);
    }
    if (session !== undefined && session.state.status === "ready") {
      if (route.view === "run") {
        if (before.view !== "run" || before.runId !== route.runId || session.state.detail === undefined) {
          void session.openRun(route.runId);
        }
      } else if (before.view === "run") {
        session.closeRun();
      }
      if (route.view === "settings") {
        void session.loadDevices();
      }
    }
    if (session !== undefined) {
      this.#render(session.state, true);
    }
  }

  #build(): void {
    const root = this.attachShadow({ mode: "open" });
    adoptStyles(root, MONITOR_CSS);
    const title = h("h1", {}, "Agent monitor");
    const badge = h("span", { class: "badge" });
    const tab = (view: "runs" | "approvals" | "settings", label: string, extra?: Node): HTMLButtonElement => {
      const element = h("button", { type: "button" }, label, extra);
      element.addEventListener("click", () => this.#go({ view }, true));
      return element;
    };
    const tabs = {
      runs: tab("runs", "Runs"),
      approvals: tab("approvals", "Approvals", badge),
      settings: tab("settings", "Settings"),
    };
    const nav = h("nav", { "aria-label": "Views" }, tabs.runs, tabs.approvals, tabs.settings);
    const gateNotice = h("p", { class: "note" });
    const gate = h("div", { class: "gate" }, gateNotice);
    const runs = h("section", { class: "view" });
    const approvals = h("section", { class: "view" });
    const run = h("section", { class: "view" });
    const settings = h("section", { class: "view" });
    const main = h("main", {}, gate, runs, approvals, run, settings);
    root.append(h("header", {}, title), main, nav);
    this.#parts = { title, main, nav, tabs, badge, gate, gateNotice, runs, approvals, run, settings };
  }

  /** The form that asks for the API key or a device token. */
  #signInForm(): HTMLFormElement {
    const credential = h("input", {
      type: "password",
      autocomplete: "current-password",
      "aria-label": "API key or device token",
      placeholder: "API key or device token",
    });
    const remember = h("input", { type: "checkbox", id: "remember" });
    const form = h(
      "form",
      { class: "gate" },
      h("p", {}, "Enter the API key of this server."),
      h("p", { class: "note" }, "On a phone, open the pairing link the owner made for it instead."),
      credential,
      h("div", { class: "check" }, remember, h("label", { for: "remember" }, "Remember on this device")),
      h("button", { type: "submit", class: "primary" }, "Continue"),
    );
    form.addEventListener("submit", (event) => {
      event.preventDefault();
      if (credential.value !== "") {
        void this.#session?.unlock(credential.value, remember.checked).then(() => this.#pollWhileVisible());
      }
    });
    return form;
  }

  #render(state: MonitorState, routeChanged = false): void {
    const parts = this.#parts;
    if (parts === undefined) {
      return;
    }
    const before = this.#last;
    this.#last = state;
    const ready = state.status === "ready";
    parts.nav.hidden = !ready;
    parts.gate.hidden = ready;
    const view = this.#route.view;
    parts.runs.hidden = !ready || view !== "runs";
    parts.approvals.hidden = !ready || view !== "approvals";
    parts.run.hidden = !ready || view !== "run";
    parts.settings.hidden = !ready || view !== "settings";
    if (!ready) {
      const locked = state.status === "locked";
      const hasForm = parts.gate.querySelector("form") !== null;
      if (locked && !hasForm) {
        parts.gate.replaceChildren(this.#signInForm(), parts.gateNotice);
      } else if (!locked && hasForm) {
        parts.gate.replaceChildren(parts.gateNotice);
      }
      parts.gateNotice.textContent = state.notice ?? (state.status === "loading" ? "Loading…" : "");
      parts.gateNotice.className = state.status === "loading" ? "note" : "note error";
      return;
    }
    const session = this.#session;
    // A view the person may not use (a device that only reads runs has no
    // inbox) gives way to one they may.
    if (session !== undefined) {
      const inRuns = view === "runs" || view === "run";
      if (view === "approvals" && !session.canResolve) {
        this.#go({ view: session.canReadRuns ? "runs" : "settings" }, true);
        return;
      }
      if (inRuns && !session.canReadRuns) {
        this.#go({ view: session.canResolve ? "approvals" : "settings" }, true);
        return;
      }
    }
    for (const [name, tab] of Object.entries(parts.tabs)) {
      const current = name === view || (name === "runs" && view === "run");
      if (current) {
        tab.setAttribute("aria-current", "page");
      } else {
        tab.removeAttribute("aria-current");
      }
    }
    parts.tabs.runs.hidden = session?.canReadRuns !== true;
    parts.tabs.approvals.hidden = session?.canResolve !== true;
    parts.badge.hidden = state.approvals.length === 0;
    parts.badge.textContent = String(state.approvals.length);
    parts.badge.setAttribute("aria-label", `${state.approvals.length} waiting`);
    const changed = <K extends keyof MonitorState>(...keys: K[]): boolean =>
      routeChanged || before === undefined || keys.some((key) => before[key] !== state[key]);
    if (view === "runs" && changed("runs", "filter", "moreRuns", "listError", "status")) {
      this.#renderRuns(state);
    }
    if (view === "approvals" && changed("approvals", "resolving", "resolveError", "listError", "status")) {
      this.#renderApprovals(state);
    }
    if (view === "run" && changed("detail", "approvals", "resolving", "resolveError", "status")) {
      this.#renderRun(state);
    }
    if (
      view === "settings" &&
      changed("push", "devices", "paired", "deviceError", "device", "role", "config")
    ) {
      this.#renderSettings(state);
    }
  }

  // ---- runs ---------------------------------------------------------------------

  #renderRuns(state: MonitorState): void {
    const parts = this.#parts;
    if (parts === undefined) {
      return;
    }
    const chips = h("div", { class: "chips", role: "group", "aria-label": "Show" });
    for (const filter of FILTERS) {
      const chip = button(filter.label, "chip", () => void this.#session?.setFilter(filter.status));
      chip.setAttribute("aria-pressed", String(state.filter === filter.status));
      chips.append(chip);
    }
    const list = h("ul", { class: "list" });
    for (const run of state.runs) {
      const snippet = run.status === "failed" ? (run.error ?? "") : (run.answer ?? "");
      const row = h(
        "button",
        { type: "button", class: "row" },
        h(
          "span",
          { class: "line" },
          h("span", { class: "name" }, run.workflow),
          pill(run.status),
          h("span", { class: "muted" }, ago(run.updated_ms)),
        ),
        snippet !== "" && h("span", { class: "snippet" }, snippet.replace(/\s+/g, " ").slice(0, 200)),
      );
      row.addEventListener("click", () => this.#go({ view: "run", runId: run.run_id }, true));
      list.append(h("li", {}, row));
    }
    fill(
      parts.runs,
      chips,
      state.listError !== undefined && h("p", { class: "note error", role: "status" }, state.listError),
      state.runs.length === 0
        ? h("p", { class: "empty" }, state.filter === undefined ? "No runs yet." : "No runs like that.")
        : list,
      state.moreRuns && button("Show older runs", "plain", () => void this.#session?.loadMore()),
    );
  }

  // ---- requests that wait for a person ------------------------------------------

  /** The card of one request, kept as it is while the request does not change. */
  #requestCard(id: string, runId: string, request: ApprovalRequest, busy: boolean): HTMLElement {
    const signature = JSON.stringify([request.kind, request.data ?? null, request.expires_ms ?? null]);
    const known = this.#cards.get(id);
    let element = known?.signature === signature ? known.element : undefined;
    if (element === undefined) {
      element = this.#newRequestCard(runId, request);
      this.#cards.set(id, { signature, element });
    }
    // The card is kept, so the time that is left is written again on every pass.
    const note = element.querySelector<HTMLElement>("[data-expiry]");
    if (note !== null) {
      const enforced = this.#session?.state.config?.expire_approvals === true;
      note.textContent = expiryNote(request, enforced) ?? "";
    }
    for (const control of element.querySelectorAll<HTMLButtonElement>("button[data-answer]")) {
      control.disabled = busy;
    }
    element.classList.toggle("busy", busy);
    return element;
  }

  #newRequestCard(runId: string, request: ApprovalRequest): HTMLElement {
    const answer = (input: Json): void => void this.#session?.resolve(runId, request.key, input);
    const data = request.data;
    // Filled by `#requestCard`, which knows the time that is left on each pass.
    const meta = typeof request.expires_ms === "number" && h("p", { class: "note", "data-expiry": "" });
    const reason = { kind: request.kind, node_id: request.node_id, ...(data === undefined ? {} : { data }) };
    const block = questionBlock(reason);
    if (block !== undefined) {
      const card = document.createElement("graphrt-block") as GraphrtBlockElement;
      const theme = this.getAttribute("theme");
      if (theme !== null) {
        card.setAttribute("theme", theme);
      }
      card.block = block;
      card.addEventListener("graphrt-action", (event) => {
        const action = (event as CustomEvent<BlockAction>).detail;
        answer(
          action.id === undefined ? { values: action.data } : { action: action.id, values: action.data },
        );
      });
      return h("div", { class: "request" }, card, meta);
    }
    const isObject = typeof data === "object" && data !== null && !Array.isArray(data);
    const calls = isObject && Array.isArray(data.calls) ? data.calls : undefined;
    if (request.kind === "tool_approval" && calls !== undefined) {
      const listed = calls.map((call) => {
        const fields = typeof call === "object" && call !== null && !Array.isArray(call) ? call : {};
        const name = typeof fields.name === "string" ? fields.name : "a tool";
        return h(
          "div",
          { class: "call" },
          h("strong", {}, name),
          fields.arguments !== undefined &&
            h("pre", { class: "data" }, JSON.stringify(fields.arguments, null, 2)),
        );
      });
      const approve = button("Approve", "primary", () => answer({ approved: true }));
      const deny = button("Deny", "plain", () => answer({ approved: false }));
      approve.dataset.answer = "approve";
      deny.dataset.answer = "deny";
      return h(
        "div",
        { class: "request" },
        h("p", {}, calls.length === 1 ? "The agent wants to call a tool:" : "The agent wants to call tools:"),
        ...listed,
        h("div", { class: "actions" }, approve, deny),
        meta,
      );
    }
    return h(
      "div",
      { class: "request" },
      h("p", {}, `The step ${request.node_id} waits for an answer (${request.kind}).`),
      data !== undefined && h("pre", { class: "data" }, JSON.stringify(data, null, 2)),
      h("p", { class: "note" }, "This page cannot build that answer. Resume the run with the API."),
    );
  }

  /** The card of a run that waits, kept in place while the run waits. */
  #approvalCard(approval: Approval, state: MonitorState, linked: boolean): HTMLElement {
    const id = `${linked ? "inbox" : "run"}:${approval.run_id}`;
    let article = this.#articles.get(id);
    if (article === undefined) {
      article = h("article", { class: "card urgent" });
      this.#articles.set(id, article);
    }
    const since = article.querySelector(".since") ?? h("span", { class: "muted since" });
    since.textContent = `waiting since ${ago(approval.paused_ms)}`;
    const head =
      article.querySelector(".line") ??
      h("div", { class: "line" }, h("span", { class: "name" }, approval.workflow), since);
    const open =
      article.querySelector("button.open") ??
      button("Open the run", "link open", () => this.#go({ view: "run", runId: approval.run_id }, true));
    fill(
      article,
      head,
      ...approval.requests.map((request) =>
        this.#requestCard(
          `${id}/${request.key}`,
          approval.run_id,
          request,
          state.resolving.includes(`${approval.run_id}/${request.key}`),
        ),
      ),
      linked && this.#session?.canReadRuns === true && open,
    );
    return article;
  }

  /** Drop the cards of runs that no longer wait. */
  #forgetCards(state: MonitorState): void {
    const waiting = new Set(state.approvals.map((approval) => approval.run_id));
    const runOf = (id: string): string => id.slice(id.indexOf(":") + 1).split("/")[0] ?? "";
    for (const id of this.#cards.keys()) {
      if (!waiting.has(runOf(id))) {
        this.#cards.delete(id);
      }
    }
    for (const id of this.#articles.keys()) {
      if (!waiting.has(runOf(id))) {
        this.#articles.delete(id);
      }
    }
  }

  #renderApprovals(state: MonitorState): void {
    const parts = this.#parts;
    if (parts === undefined) {
      return;
    }
    this.#forgetCards(state);
    const error = state.resolveError ?? state.listError;
    fill(
      parts.approvals,
      error !== undefined && h("p", { class: "note error", role: "status" }, error),
      ...(state.approvals.length === 0
        ? [h("p", { class: "empty" }, "Nothing waits for you.")]
        : state.approvals.map((approval) => this.#approvalCard(approval, state, true))),
    );
  }

  // ---- one run ------------------------------------------------------------------

  #eventItem(line: EventLine): HTMLLIElement {
    const about = [line.node, line.detail].filter((part) => part !== undefined).join(" · ");
    return h(
      "li",
      { class: line.tone },
      h("span", { class: "type" }, line.type),
      about !== "" && h("span", { class: "about" }, about),
    );
  }

  #renderRun(state: MonitorState): void {
    const parts = this.#parts;
    const detail = state.detail;
    if (parts === undefined) {
      return;
    }
    const back = button("‹ Runs", "link", () => this.#go({ view: "runs" }, true));
    if (detail === undefined) {
      this.#drawnRun = undefined;
      fill(parts.run, back, h("p", { class: "note" }, "Loading…"));
      return;
    }
    const record = detail.record;
    const drawn = this.#drawnRun;
    const sameRun = drawn?.runId === detail.runId;
    // While a run executes only its log and answer grow: add to them in place.
    const log = sameRun ? parts.run.querySelector<HTMLUListElement>("ul.events") : null;
    const answerBox = sameRun ? parts.run.querySelector<HTMLElement>(".answer") : null;
    const first = detail.events[0];
    const appendOnly =
      log !== null &&
      answerBox !== null &&
      drawn !== undefined &&
      drawn.record === record &&
      detail.error === undefined &&
      detail.events.length >= drawn.lines &&
      (first === undefined || first.index <= drawn.lastIndex + 1);
    if (appendOnly) {
      for (const line of detail.events) {
        if (line.index > drawn.lastIndex) {
          log.append(this.#eventItem(line));
        }
      }
      answerBox.replaceChildren(...richText(`${record?.answer ?? ""}${detail.liveAnswer}`));
      answerBox.hidden = answerBox.textContent === "";
    } else {
      const answerText = `${record?.answer ?? ""}${detail.liveAnswer}`;
      const answer = h("div", { class: "answer" }, ...richText(answerText));
      answer.hidden = answerText === "";
      const events = h("ul", { class: "events" }, ...detail.events.map((line) => this.#eventItem(line)));
      const blocks = record === undefined ? [] : collectUiBlocks(record.outputs ?? {});
      const theme = this.getAttribute("theme");
      const stop =
        state.role === "owner" &&
        record?.active === true &&
        button("Stop this run", "danger", () => void this.#session?.stopRun(detail.runId));
      const tokens = record?.usage?.total_tokens;
      fill(
        parts.run,
        back,
        h(
          "div",
          { class: "card" },
          h(
            "div",
            { class: "line" },
            h("span", { class: "name" }, record?.workflow ?? "Run"),
            record !== undefined && pill(record.status),
          ),
          h(
            "div",
            { class: "facts" },
            record !== undefined && h("span", {}, `started ${ago(record.created_ms)}`),
            record !== undefined && h("span", {}, `updated ${ago(record.updated_ms)}`),
            typeof tokens === "number" && tokens > 0 && h("span", {}, `${tokens} tokens`),
            h("span", {}, detail.runId),
          ),
          detail.error !== undefined && h("p", { class: "note error", role: "status" }, detail.error),
          record?.status === "failed" && h("p", { class: "note error" }, record.error ?? "The run failed."),
          record?.status === "aborted" && h("p", { class: "note" }, record.error ?? "The run was stopped."),
          stop,
        ),
        h("div", { class: "waiting" }),
        answer,
        ...blocks.map((value) => {
          const block = document.createElement("graphrt-block") as GraphrtBlockElement;
          if (theme !== null) {
            block.setAttribute("theme", theme);
          }
          block.block = value;
          return block;
        }),
        h("h2", {}, detail.live ? "What it is doing" : "What it did"),
        events.childElementCount === 0 ? h("p", { class: "note" }, "No events yet.") : events,
      );
    }
    // What the run waits for, to answer here.
    const waitingBox = parts.run.querySelector<HTMLElement>(".waiting");
    const waiting = state.approvals.find((approval) => approval.run_id === detail.runId);
    if (waitingBox !== null) {
      const shown = waiting !== undefined && this.#session?.canResolve === true;
      fill(
        waitingBox,
        shown &&
          state.resolveError !== undefined &&
          h("p", { class: "note error", role: "status" }, state.resolveError),
        shown && this.#approvalCard(waiting, state, false),
      );
    }
    this.#drawnRun = {
      runId: detail.runId,
      lines: detail.events.length,
      lastIndex: detail.events.at(-1)?.index ?? -1,
      record,
    };
  }

  // ---- settings -----------------------------------------------------------------

  #pushSection(state: MonitorState): HTMLElement | false {
    const push = state.push;
    if (state.config?.push !== true) {
      return false;
    }
    const section = h("section", { class: "card" }, h("h2", {}, "Notifications"));
    if (push.needsInstall) {
      section.append(
        h(
          "p",
          {},
          "To get notifications on an iPhone or iPad, add this page to the Home Screen first: tap Share, then Add to Home Screen, and open it from there.",
        ),
      );
    } else if (!push.available) {
      section.append(h("p", { class: "note" }, "This browser cannot receive push notifications."));
    } else if (push.subscribed) {
      const test = button("Send a test", "plain", () => void this.#session?.testPush());
      const off = button("Turn off", "plain", () => void this.#session?.disablePush());
      test.disabled = push.busy;
      off.disabled = push.busy;
      section.append(
        h("p", {}, "This device is told when a run fails or waits for you."),
        h("div", { class: "actions" }, test, off),
      );
    } else {
      const on = button("Turn on notifications", "primary", () => void this.#session?.enablePush());
      on.disabled = push.busy || push.permission === "denied";
      section.append(h("p", {}, "Hear about it when a run fails or waits for you."), on);
      if (push.permission === "denied") {
        section.append(
          h(
            "p",
            { class: "note" },
            "Notifications are blocked for this site. Allow them in the browser's settings.",
          ),
        );
      }
    }
    if (push.notice !== undefined) {
      section.append(h("p", { class: "note", role: "status" }, push.notice));
    }
    return section;
  }

  #devicesSection(state: MonitorState): HTMLElement | false {
    if (state.role !== "owner" || state.config?.devices !== true) {
      return false;
    }
    const section = h(
      "section",
      { class: "card" },
      h("h2", {}, "Devices"),
      h(
        "p",
        { class: "note" },
        "A paired device reads runs and answers what waits for a person. It cannot start runs or change anything else, and you can remove it here at any time.",
      ),
    );
    const paired = state.paired;
    if (paired !== undefined) {
      const link = paired.link ?? JSON.stringify(paired.pairing);
      const copy = button("Copy the link", "plain", () => {
        void navigator.clipboard?.writeText(link).then(
          () => {
            copy.textContent = "Copied";
          },
          () => undefined,
        );
      });
      const pairing = h(
        "div",
        { class: "pairing" },
        h("strong", {}, `Pair "${paired.device.name}"`),
        h("p", {}, "Scan this with the phone's camera, or open the link on the device."),
      );
      try {
        pairing.append(qrSvg(encodeQr(link), "QR code of the pairing link"));
      } catch {
        // A link too long for a QR code: the text below still pairs.
      }
      pairing.append(
        h("div", { class: "link" }, link),
        h(
          "p",
          { class: "note" },
          "This link is the device's key and is shown only now. Whoever has it can read your runs.",
        ),
        h(
          "div",
          { class: "actions" },
          copy,
          button("Done", "primary", () => this.#session?.dismissPaired()),
        ),
      );
      section.append(pairing);
    } else {
      const name = h("input", {
        type: "text",
        "aria-label": "Name of the device",
        placeholder: "Name, e.g. My phone",
        maxlength: "100",
      });
      const form = h(
        "form",
        { class: "form" },
        name,
        h("button", { type: "submit", class: "primary" }, "Pair"),
      );
      form.addEventListener("submit", (event) => {
        event.preventDefault();
        if (name.value.trim() !== "") {
          void this.#session?.pairDevice(name.value);
        }
      });
      section.append(form);
    }
    if (state.deviceError !== undefined) {
      section.append(h("p", { class: "note error", role: "status" }, state.deviceError));
    }
    const list = h("ul", { class: "list" });
    for (const device of state.devices ?? []) {
      const used =
        typeof device.last_used_ms === "number" ? `used ${ago(device.last_used_ms)}` : "never used";
      const expiry =
        typeof device.expires_ms === "number"
          ? ` · ${timeLeft(device.expires_ms) === undefined ? "expired" : `expires in ${timeLeft(device.expires_ms)}`}`
          : "";
      list.append(
        h(
          "li",
          { class: "line" },
          h("span", { class: "name" }, device.name, h("span", { class: "muted" }, ` · ${used}${expiry}`)),
          button("Remove", "danger", () => void this.#session?.revokeDevice(device.id)),
        ),
      );
    }
    if (list.childElementCount > 0) {
      section.append(list);
    }
    return section;
  }

  #renderSettings(state: MonitorState): void {
    const parts = this.#parts;
    if (parts === undefined) {
      return;
    }
    const session = h("section", { class: "card" });
    if (state.role === "device") {
      session.append(
        h("h2", {}, "This device"),
        h("p", {}, `Paired as "${state.device?.name ?? "a device"}".`),
        h(
          "div",
          { class: "actions" },
          button("Unpair this device", "danger", () => void this.#session?.unpair()),
        ),
      );
    } else if (state.config?.access === "key") {
      session.append(
        h("h2", {}, "Signed in with the API key"),
        h(
          "p",
          { class: "note" },
          "The key reads and changes everything. For a phone, pair it as a device instead.",
        ),
        h(
          "div",
          { class: "actions" },
          button("Sign out", "plain", () => this.#session?.signOut()),
        ),
      );
    } else {
      session.append(
        h("h2", {}, "No API key"),
        h("p", { class: "note" }, "This server has no API key: anyone who can reach it can use it."),
      );
    }
    fill(
      parts.settings,
      ...[this.#pushSection(state), this.#devicesSection(state), session].filter(
        (node): node is HTMLElement => node !== false,
      ),
    );
  }
}

/** Register `<graphrt-monitor>` (and `<graphrt-block>`). Safe to call more than once. */
export function defineGraphrtMonitor(): void {
  defineGraphrtUi();
  if (typeof customElements !== "undefined" && customElements.get("graphrt-monitor") === undefined) {
    customElements.define("graphrt-monitor", GraphrtMonitorElement);
  }
}

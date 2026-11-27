/**
 * `<graphrt-console>`: the admin console of one `graphrt serve --console`.
 * Agents and their versions per environment, runs and approvals (the
 * monitoring view), secrets, people and their roles, and the audit log.
 * What each person sees follows their role.
 *
 * @example
 * <graphrt-console server="https://agents.example.com" hash-routing></graphrt-console>
 *
 * Attributes: `server` (default: the page's origin) and `hash-routing`
 * (keeps the open view in the URL fragment, for a page that is this element only).
 */
import type {
  AuditEntry,
  AuditQuery,
  ConsoleRole,
  RegistryWorkflow,
  ServedWorkflow,
} from "../client/index.ts";
import { ago, defineGraphrtMonitor, type GraphrtMonitorElement } from "../monitor/index.ts";
import { adoptStyles } from "../ui/index.ts";
import { ConsoleSession, type ConsoleState, type ConsoleTab } from "./session.ts";
import { CONSOLE_CSS } from "./styles.ts";

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

/** Make `children` what `parent` holds; nodes already in place are kept (forms keep their text). */
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

function button(label: string, kind: string, onPress: () => void, disabled = false): HTMLButtonElement {
  const element = h("button", { type: "button", class: kind }, label);
  element.disabled = disabled;
  element.addEventListener("click", onPress);
  return element;
}

function select(
  label: string,
  options: { value: string; label: string }[],
  value: string,
): HTMLSelectElement {
  const element = h("select", { "aria-label": label });
  for (const option of options) {
    element.append(h("option", { value: option.value }, option.label));
  }
  element.value = value;
  return element;
}

const short = (hash: string | undefined): string => (hash === undefined ? "" : hash.slice(0, 12));

const TABS: { tab: ConsoleTab; label: string; role: ConsoleRole }[] = [
  { tab: "agents", label: "Agents", role: "viewer" },
  { tab: "runs", label: "Runs", role: "viewer" },
  { tab: "secrets", label: "Secrets", role: "operator" },
  { tab: "people", label: "People", role: "owner" },
  { tab: "audit", label: "Audit log", role: "owner" },
];

const AUDIT_GROUPS: { value: string; label: string }[] = [
  { value: "", label: "Every action" },
  { value: "auth.", label: "Sign ins" },
  { value: "member.", label: "Roles" },
  { value: "secret.", label: "Secrets" },
  { value: "registry.", label: "Versions" },
  { value: "server.", label: "Restarts" },
  { value: "approval.", label: "Approvals" },
  { value: "device.", label: "Devices" },
  { value: "run.", label: "Runs" },
];

/** The tab of a URL fragment (`#/secrets`), the agents for anything else. */
export function tabOf(hash: string): ConsoleTab {
  const name = hash.replace(/^#\/?/, "").split("/")[0];
  return TABS.find((entry) => entry.tab === name)?.tab ?? "agents";
}

/** One sentence about an audit entry's actor. */
export function actorOf(entry: AuditEntry): string {
  const actor = entry.actor;
  if (actor.kind === "api-key") {
    return "API key";
  }
  if (actor.kind === "system") {
    return "the server";
  }
  if (actor.kind === "device") {
    return `device ${actor.id ?? ""}`.trim();
  }
  return actor.email ?? actor.id ?? actor.kind;
}

interface Parts {
  title: HTMLElement;
  environment: HTMLElement;
  who: HTMLElement;
  main: HTMLElement;
  nav: HTMLElement;
  tabs: Map<ConsoleTab, HTMLButtonElement>;
  gate: HTMLElement;
  views: Record<ConsoleTab, HTMLElement>;
  outcome: HTMLElement;
}

const BaseElement: typeof HTMLElement = globalThis.HTMLElement ?? (class {} as typeof HTMLElement);

export class GraphrtConsoleElement extends BaseElement {
  #session: ConsoleSession | undefined;
  #unsubscribe: (() => void) | undefined;
  #parts: Parts | undefined;
  #tab: ConsoleTab = "agents";
  #monitor: GraphrtMonitorElement | undefined;
  /** Forms kept across renders, so what the person types stays. */
  #forms = new Map<string, HTMLElement>();
  #onHashChange = (): void => this.show(tabOf(location.hash));

  /** The session behind the view, once the element is connected. */
  get session(): ConsoleSession | undefined {
    return this.#session;
  }

  connectedCallback(): void {
    if (this.#parts === undefined) {
      this.#build();
    }
    if (this.hasAttribute("hash-routing")) {
      this.#tab = tabOf(location.hash);
      window.addEventListener("hashchange", this.#onHashChange);
    }
    if (this.#session === undefined) {
      const server = this.getAttribute("server");
      const session = new ConsoleSession(server === null ? {} : { baseUrl: server });
      this.#session = session;
      this.#unsubscribe = session.subscribe((state) => this.#render(state));
      this.#render(session.state);
      void session.start().then(() => session.load(this.#tab));
    }
  }

  disconnectedCallback(): void {
    window.removeEventListener("hashchange", this.#onHashChange);
    this.#unsubscribe?.();
    this.#unsubscribe = undefined;
    this.#session = undefined;
    this.#monitor = undefined;
    this.#forms.clear();
  }

  /** Show a view. */
  show(tab: ConsoleTab): void {
    if (this.hasAttribute("hash-routing") && tabOf(location.hash) !== tab) {
      location.hash = `#/${tab}`;
      return;
    }
    this.#tab = tab;
    const session = this.#session;
    if (session !== undefined) {
      void session.load(tab);
      this.#render(session.state);
    }
  }

  #build(): void {
    const root = this.attachShadow({ mode: "open" });
    adoptStyles(root, CONSOLE_CSS);
    const title = h("h1", {}, "Graphrt console");
    const environment = h("span", { class: "pill where" });
    const who = h("div", { class: "who" });
    const tabs = new Map<ConsoleTab, HTMLButtonElement>();
    const nav = h("nav", { "aria-label": "Views" });
    for (const entry of TABS) {
      const tab = button(entry.label, "", () => this.show(entry.tab));
      tabs.set(entry.tab, tab);
      nav.append(tab);
    }
    const gate = h("div", { class: "gate" });
    const views = {
      agents: h("section", { class: "view" }),
      runs: h("section", { class: "view runs" }),
      secrets: h("section", { class: "view" }),
      people: h("section", { class: "view" }),
      audit: h("section", { class: "view" }),
    };
    const outcome = h("p", { class: "outcome", role: "status" });
    const main = h("main", {}, gate, views.agents, views.runs, views.secrets, views.people, views.audit);
    root.append(h("header", {}, title, environment, who), main, nav);
    this.#parts = { title, environment, who, main, nav, tabs, gate, views, outcome };
  }

  #render(state: ConsoleState): void {
    const parts = this.#parts;
    const session = this.#session;
    if (parts === undefined || session === undefined) {
      return;
    }
    const ready = state.status === "ready";
    parts.nav.hidden = !ready;
    parts.gate.hidden = ready;
    parts.environment.hidden = state.me === undefined;
    parts.environment.textContent = state.me?.environment ?? "";
    parts.environment.title = "The environment this server serves";
    if (!ready) {
      for (const view of Object.values(parts.views)) {
        view.hidden = true;
      }
      fill(parts.who);
      this.#renderGate(state);
      return;
    }
    // A view the role does not reach gives way to the agents.
    const allowed = TABS.find((entry) => entry.tab === this.#tab && session.can(entry.role));
    if (allowed === undefined) {
      this.#tab = "agents";
    }
    for (const entry of TABS) {
      const tab = parts.tabs.get(entry.tab);
      if (tab !== undefined) {
        tab.hidden = !session.can(entry.role);
        if (entry.tab === this.#tab) {
          tab.setAttribute("aria-current", "page");
        } else {
          tab.removeAttribute("aria-current");
        }
      }
    }
    for (const [tab, view] of Object.entries(parts.views)) {
      view.hidden = tab !== this.#tab;
    }
    const user = state.me?.user;
    fill(
      parts.who,
      h("span", { class: "email" }, user?.email ?? user?.name ?? user?.subject ?? ""),
      h("span", { class: "pill" }, state.role ?? ""),
      button("Sign out", "plain small", () => void session.signOut()),
    );
    parts.outcome.textContent = state.outcome?.text ?? "";
    parts.outcome.className = `outcome ${state.outcome?.tone ?? ""}`;
    switch (this.#tab) {
      case "agents":
        this.#renderAgents(state, session);
        break;
      case "runs":
        this.#renderRuns(state, session);
        break;
      case "secrets":
        this.#renderSecrets(state, session);
        break;
      case "people":
        this.#renderPeople(state, session);
        break;
      case "audit":
        this.#renderAudit(state, session);
        break;
    }
  }

  #renderGate(state: ConsoleState): void {
    const parts = this.#parts;
    const session = this.#session;
    if (parts === undefined || session === undefined) {
      return;
    }
    if (state.status === "loading") {
      fill(parts.gate, h("p", { class: "note" }, "Loading…"));
      return;
    }
    if (state.status === "unavailable") {
      fill(parts.gate, h("p", { class: "note error" }, state.notice ?? "The console is not available."));
      return;
    }
    const url = session.signInUrl();
    fill(
      parts.gate,
      h("h2", {}, "Sign in"),
      h("p", {}, "Sign in with your organization's account to manage the agents of this server."),
      url !== undefined && h("a", { class: "button primary", href: url }, "Sign in with your organization"),
      state.notice !== undefined && h("p", { class: "note error", role: "status" }, state.notice),
    );
  }

  /** A form kept across renders under `key`. */
  #form(key: string, make: () => HTMLElement): HTMLElement {
    let form = this.#forms.get(key);
    if (form === undefined) {
      form = make();
      this.#forms.set(key, form);
    }
    return form;
  }

  // ---- agents --------------------------------------------------------------------------

  #renderAgents(state: ConsoleState, session: ConsoleSession): void {
    const parts = this.#parts;
    if (parts === undefined) {
      return;
    }
    const view = state.workflows;
    const owner = session.can("owner");
    const pending = view?.pending ?? [];
    const pendingCard =
      pending.length > 0 &&
      h(
        "div",
        { class: "card notice" },
        h("h2", {}, "Changes wait for a restart"),
        h(
          "p",
          { class: "note" },
          `${pending.map((item) => (item === "secrets" ? "secrets" : item.replace(/^workflow:/, ""))).join(", ")}: they take effect when the server restarts. Active runs pause and carry on after it.`,
        ),
        owner &&
          button(
            state.restarting ? "Restarting…" : "Restart and apply",
            "primary",
            () => void session.apply(),
            state.restarting,
          ),
      );
    const names = new Set<string>([
      ...(view?.served ?? []).map((workflow) => workflow.name),
      ...(view?.registry ?? []).map((workflow) => workflow.name),
    ]);
    const cards = [...names].sort().map((name) => {
      const served = view?.served.find((workflow) => workflow.name === name);
      const registry = view?.registry.find((workflow) => workflow.name === name);
      return this.#agentCard(name, served, registry, state, session);
    });
    fill(
      parts.views.agents,
      parts.outcome,
      state.errors.agents !== undefined &&
        h("p", { class: "note error", role: "status" }, state.errors.agents),
      pendingCard,
      owner && this.#uploadForm(state),
      cards.length === 0 &&
        view !== undefined &&
        h("p", { class: "empty" }, "This server serves no workflow."),
      ...cards,
    );
  }

  #agentCard(
    name: string,
    served: ServedWorkflow | undefined,
    registry: RegistryWorkflow | undefined,
    state: ConsoleState,
    session: ConsoleSession,
  ): HTMLElement {
    const owner = session.can("owner");
    const environments = state.workflows?.environments ?? [];
    const here = state.workflows?.environment;
    const running =
      served === undefined
        ? "not served here"
        : served.version === undefined
          ? "serves its file from disk"
          : `serves version ${served.version}`;
    const envs = h("div", { class: "envs" });
    environments.forEach((environment, index) => {
      const entry = registry?.environments[environment];
      const active = entry?.active ?? undefined;
      const next = environments[index + 1];
      const earlier = entry?.history.find((deployment) => deployment.version !== active);
      envs.append(
        h(
          "div",
          { class: environment === here ? "env here" : "env" },
          h(
            "div",
            { class: "line" },
            h("strong", {}, environment),
            environment === here && h("span", { class: "muted" }, "this server"),
          ),
          h("span", {}, active === undefined || active === null ? "no version" : `version ${active}`),
          owner &&
            h(
              "div",
              { class: "actions" },
              next !== undefined &&
                typeof active === "number" &&
                button(
                  `Promote to ${next}`,
                  "plain small",
                  () => void session.promote(name, environment, next),
                ),
              earlier !== undefined &&
                button(
                  `Roll back to ${earlier.version}`,
                  "plain small",
                  () => void session.rollback(name, environment),
                ),
            ),
        ),
      );
    });
    const versions = h("ul", { class: "versions" });
    for (const version of registry?.versions ?? []) {
      const target = select(
        `Environment for version ${version.version}`,
        environments.map((environment) => ({ value: environment, label: environment })),
        environments[0] ?? "",
      );
      versions.append(
        h(
          "li",
          {},
          h("strong", {}, `v${version.version}`),
          h("span", { class: "mono muted" }, short(version.hash)),
          version.note !== undefined && version.note !== "" && h("span", {}, version.note),
          h("span", { class: "muted grow" }, `${version.uploaded_by}, ${ago(version.uploaded_ms)}`),
          owner && h("a", { href: session.versionFileUrl(name, version.version), download: "" }, "Download"),
          owner && target,
          owner &&
            button(
              "Run there",
              "plain small",
              () => void session.deploy(name, version.version, target.value),
            ),
        ),
      );
    }
    return h(
      "article",
      { class: "card" },
      h(
        "div",
        { class: "line" },
        h("span", { class: "name grow" }, name),
        h("span", { class: "muted" }, running),
        served?.hash !== undefined &&
          h("span", { class: "mono muted", title: served.hash }, short(served.hash)),
      ),
      served !== undefined &&
        served.env.length > 0 &&
        h("p", { class: "note" }, `Reads ${served.env.join(", ")}`),
      envs,
      (registry?.versions.length ?? 0) > 0 &&
        h("details", {}, h("summary", {}, `${registry?.versions.length ?? 0} version(s)`), versions),
    );
  }

  #uploadForm(state: ConsoleState): HTMLElement {
    const environments = state.workflows?.environments ?? [];
    return this.#form(`upload:${environments.join(",")}`, () => {
      const name = h("input", {
        type: "text",
        "aria-label": "Workflow name",
        placeholder: "Workflow name",
        required: "",
      });
      const file = h("input", {
        type: "file",
        "aria-label": "Workflow file",
        accept: ".yaml,.yml,.json,.grt",
      });
      const environment = select(
        "Runs in",
        [
          ...environments.map((value) => ({ value, label: `Runs in ${value}` })),
          { value: "none", label: "Keep it only" },
        ],
        environments[0] ?? "none",
      );
      const note = h("input", { type: "text", "aria-label": "Note", placeholder: "What changed (optional)" });
      const submit = h("button", { type: "submit", class: "primary" }, "Upload");
      const form = h(
        "form",
        { class: "card" },
        h("h2", {}, "Upload a version"),
        h(
          "p",
          { class: "note" },
          "A workflow (YAML or JSON) or a bundle (.grt). The server checks that it compiles.",
        ),
        h("div", { class: "actions" }, name, file),
        h("div", { class: "actions" }, environment, note, submit),
      );
      file.addEventListener("change", () => {
        const picked = file.files?.[0];
        if (picked !== undefined && name.value === "") {
          name.value = picked.name.replace(/\.(ya?ml|json|grt)$/i, "");
        }
      });
      form.addEventListener("submit", (event) => {
        event.preventDefault();
        const picked = file.files?.[0];
        const session = this.#session;
        if (picked === undefined || name.value.trim() === "" || session === undefined) {
          return;
        }
        submit.disabled = true;
        void session
          .upload(name.value.trim(), picked, {
            environment: environment.value,
            ...(note.value === "" ? {} : { note: note.value }),
          })
          .then((ok) => {
            submit.disabled = false;
            if (ok) {
              file.value = "";
              note.value = "";
            }
          });
      });
      return form;
    });
  }

  // ---- runs ----------------------------------------------------------------------------

  #renderRuns(state: ConsoleState, session: ConsoleSession): void {
    const parts = this.#parts;
    if (parts === undefined || this.#monitor !== undefined) {
      return;
    }
    const role = state.role;
    if (role === undefined) {
      return;
    }
    defineGraphrtMonitor();
    const monitor = document.createElement("graphrt-monitor") as GraphrtMonitorElement;
    monitor.setAttribute("embedded", "");
    const server = this.getAttribute("server");
    if (server !== null) {
      monitor.setAttribute("server", server);
    }
    const user = state.me?.user;
    monitor.fetch = session.fetch;
    monitor.member = {
      role,
      name: user?.email ?? user?.name ?? user?.subject ?? "a member",
      signOut: () => void session.signOut(),
    };
    this.#monitor = monitor;
    fill(parts.views.runs, monitor);
  }

  // ---- secrets -------------------------------------------------------------------------

  #renderSecrets(state: ConsoleState, session: ConsoleSession): void {
    const parts = this.#parts;
    if (parts === undefined) {
      return;
    }
    const view = state.secrets;
    const owner = session.can("owner");
    const rows = (view?.secrets ?? []).map((secret) =>
      h(
        "tr",
        {},
        h("td", {}, h("code", {}, secret.name)),
        h(
          "td",
          {},
          secret.used_by.length === 0
            ? h("span", { class: "muted" }, "no served workflow")
            : secret.used_by.join(", "),
        ),
        h("td", { class: "muted" }, `${secret.updated_by}, ${ago(secret.updated_ms)}`),
        h(
          "td",
          {},
          owner &&
            button(
              "Remove",
              "danger small",
              () => void session.deleteSecret(secret.name),
              state.busy.includes(`secret:${secret.name}`),
            ),
        ),
      ),
    );
    const missing = (view?.declared ?? []).filter((declared) => !declared.in_console);
    fill(
      parts.views.secrets,
      parts.outcome,
      state.errors.secrets !== undefined &&
        h("p", { class: "note error", role: "status" }, state.errors.secrets),
      view !== undefined &&
        !view.key_configured &&
        h(
          "div",
          { class: "card notice" },
          h(
            "p",
            {},
            "No console key is configured, so secrets cannot be set. The server's operator sets GRAPHRT_CONSOLE_KEY.",
          ),
        ),
      owner && view?.key_configured === true && this.#secretForm(),
      h(
        "div",
        { class: "card" },
        h("h2", {}, "Secrets"),
        h(
          "p",
          { class: "note" },
          "Values are encrypted on the server and never shown again. Changes apply when the server restarts.",
        ),
        rows.length === 0
          ? h("p", { class: "muted" }, "No secret is set in the console.")
          : h(
              "table",
              { class: "stack" },
              h(
                "thead",
                {},
                h("tr", {}, h("th", {}, "Name"), h("th", {}, "Used by"), h("th", {}, "Changed"), h("th", {})),
              ),
              h("tbody", {}, ...rows),
            ),
      ),
      missing.length > 0 &&
        h(
          "div",
          { class: "card" },
          h("h3", {}, "Declared by workflows, not set here"),
          h("p", { class: "note" }, "These may come from the server's environment instead."),
          h(
            "ul",
            { class: "versions" },
            ...missing.map((declared) =>
              h(
                "li",
                {},
                h("code", {}, declared.name),
                h("span", { class: "muted" }, declared.workflows.join(", ")),
              ),
            ),
          ),
        ),
    );
  }

  #secretForm(): HTMLElement {
    return this.#form("secret", () => {
      const name = h("input", {
        type: "text",
        "aria-label": "Name",
        placeholder: "NAME, e.g. OPENAI_API_KEY",
        autocomplete: "off",
      });
      const value = h("input", {
        type: "password",
        "aria-label": "Value",
        placeholder: "Value",
        autocomplete: "new-password",
      });
      const submit = h("button", { type: "submit", class: "primary" }, "Set");
      const form = h(
        "form",
        { class: "card" },
        h("h2", {}, "Set a secret"),
        h("div", { class: "actions" }, name, value, submit),
      );
      form.addEventListener("submit", (event) => {
        event.preventDefault();
        const session = this.#session;
        if (session === undefined || name.value.trim() === "" || value.value === "") {
          return;
        }
        submit.disabled = true;
        void session.setSecret(name.value.trim(), value.value).then((ok) => {
          submit.disabled = false;
          if (ok) {
            name.value = "";
            value.value = "";
          }
        });
      });
      return form;
    });
  }

  // ---- people --------------------------------------------------------------------------

  #renderPeople(state: ConsoleState, session: ConsoleSession): void {
    const parts = this.#parts;
    if (parts === undefined) {
      return;
    }
    const me = state.me?.user?.subject;
    const members = (state.members?.members ?? []).map((member) => {
      const assigned = member.assigned_role ?? "";
      const choice = select(
        `Role of ${member.email ?? member.subject}`,
        [
          { value: "", label: `From groups (${member.mapped_role ?? "no access"})` },
          { value: "owner", label: "Owner" },
          { value: "operator", label: "Operator" },
          { value: "viewer", label: "Viewer" },
          { value: "none", label: "No access" },
        ],
        assigned,
      );
      choice.disabled = member.config_owner || state.busy.includes(`role:${member.subject}`);
      choice.addEventListener("change", () => {
        const value = choice.value;
        const role = value === "" ? null : (value as ConsoleRole | "none");
        void session.setRole(member.subject, role);
      });
      return h(
        "tr",
        {},
        h(
          "td",
          {},
          h("div", { class: "name" }, member.email ?? member.name ?? member.subject),
          h("div", { class: "muted mono" }, member.subject),
        ),
        h("td", {}, member.config_owner ? h("span", { class: "pill" }, "owner in the config") : choice),
        h("td", { class: "muted" }, `${ago(member.last_sign_in_ms)}${member.subject === me ? " (you)" : ""}`),
      );
    });
    const sessions = (state.sessions ?? []).map((open) =>
      h(
        "tr",
        {},
        h("td", {}, open.email ?? open.name ?? open.subject),
        h("td", { class: "muted" }, `signed in ${ago(open.created_ms)}, last seen ${ago(open.last_seen_ms)}`),
        h(
          "td",
          {},
          button(
            "End",
            "plain small",
            () => void session.endSession(open.id),
            state.busy.includes(`session:${open.id}`),
          ),
        ),
      ),
    );
    const mapping = state.members?.mapping;
    fill(
      parts.views.people,
      parts.outcome,
      state.errors.people !== undefined &&
        h("p", { class: "note error", role: "status" }, state.errors.people),
      h(
        "div",
        { class: "card" },
        h("h2", {}, "People"),
        h(
          "p",
          { class: "note" },
          "Roles come from the groups in each person's sign in, unless you set one here. It applies at once.",
        ),
        members.length === 0
          ? h("p", { class: "muted" }, "Nobody signed in yet.")
          : h(
              "table",
              { class: "stack" },
              h(
                "thead",
                {},
                h("tr", {}, h("th", {}, "Person"), h("th", {}, "Role"), h("th", {}, "Last sign in")),
              ),
              h("tbody", {}, ...members),
            ),
      ),
      mapping !== undefined &&
        h(
          "div",
          { class: "card" },
          h("h3", {}, "From groups"),
          h(
            "p",
            { class: "note" },
            `Claim ${mapping.groups_claim}. Without a matching group: ${mapping.default_role ?? "no access"}.` +
              (mapping.allowed_domains.length > 0 ? ` Only ${mapping.allowed_domains.join(", ")}.` : ""),
          ),
          h(
            "ul",
            { class: "versions" },
            ...Object.entries(mapping.groups).map(([group, role]) =>
              h("li", {}, h("code", {}, group), h("span", { class: "muted" }, role)),
            ),
          ),
        ),
      h(
        "div",
        { class: "card" },
        h("h2", {}, "Open sessions"),
        sessions.length === 0
          ? h("p", { class: "muted" }, "None.")
          : h("table", { class: "stack" }, h("tbody", {}, ...sessions)),
      ),
    );
  }

  // ---- audit ---------------------------------------------------------------------------

  #renderAudit(state: ConsoleState, session: ConsoleSession): void {
    const parts = this.#parts;
    if (parts === undefined) {
      return;
    }
    const audit = state.audit;
    const rows = audit.entries.map((entry) =>
      h(
        "tr",
        {},
        h("td", { class: "muted" }, new Date(entry.at_ms).toLocaleString()),
        h("td", {}, actorOf(entry)),
        h(
          "td",
          {},
          h("code", {}, entry.action),
          entry.target !== undefined && entry.target !== null && h("div", { class: "muted" }, entry.target),
          entry.details !== undefined &&
            entry.details !== null &&
            h(
              "details",
              {},
              h("summary", {}, "Details"),
              h("pre", { class: "data" }, JSON.stringify(entry.details, null, 2)),
            ),
        ),
        h("td", {}, h("span", { class: `pill ${entry.outcome}` }, entry.outcome)),
      ),
    );
    const verification = audit.verification;
    fill(
      parts.views.audit,
      this.#auditFilter(audit.query),
      h(
        "div",
        { class: "actions" },
        h(
          "a",
          { class: "button plain", href: session.auditExportUrl(), download: "graphrt-audit.jsonl" },
          "Export (JSON lines)",
        ),
        button("Check the chain", "plain", () => void session.verifyAudit()),
        verification !== undefined &&
          h(
            "span",
            { class: verification.broken_at_seq == null ? "pill ok" : "pill bad" },
            verification.broken_at_seq == null
              ? `${verification.entries} entries, chain intact`
              : `chain broken at entry ${verification.broken_at_seq}`,
          ),
      ),
      audit.error !== undefined && h("p", { class: "note error", role: "status" }, audit.error),
      rows.length === 0
        ? h("p", { class: "empty" }, "No entries like that.")
        : h(
            "table",
            { class: "stack" },
            h(
              "thead",
              {},
              h(
                "tr",
                {},
                h("th", {}, "When"),
                h("th", {}, "Who"),
                h("th", {}, "What"),
                h("th", {}, "Outcome"),
              ),
            ),
            h("tbody", {}, ...rows),
          ),
      audit.next !== undefined && button("Show older entries", "plain", () => void session.moreAudit()),
    );
  }

  #auditFilter(query: AuditQuery): HTMLElement {
    return this.#form("audit", () => {
      const action = select("Action", AUDIT_GROUPS, query.action ?? "");
      const actor = h("input", { type: "search", "aria-label": "Who", placeholder: "Email or id" });
      const outcome = select(
        "Outcome",
        [
          { value: "", label: "Any outcome" },
          { value: "ok", label: "Done" },
          { value: "denied", label: "Refused" },
          { value: "failed", label: "Failed" },
        ],
        query.outcome ?? "",
      );
      const form = h(
        "form",
        { class: "row" },
        action,
        actor,
        outcome,
        h("button", { type: "submit", class: "primary" }, "Show"),
      );
      form.addEventListener("submit", (event) => {
        event.preventDefault();
        const next: AuditQuery = {};
        if (action.value !== "") {
          next.action = action.value;
        }
        if (actor.value.trim() !== "") {
          next.actor = actor.value.trim();
        }
        if (outcome.value === "ok" || outcome.value === "denied" || outcome.value === "failed") {
          next.outcome = outcome.value;
        }
        void this.#session?.loadAudit(next);
      });
      return form;
    });
  }
}

/** Register `<graphrt-console>` (and the monitoring view it shows runs with). Safe to call more than once. */
export function defineGraphrtConsole(): void {
  defineGraphrtMonitor();
  if (typeof customElements !== "undefined" && customElements.get("graphrt-console") === undefined) {
    customElements.define("graphrt-console", GraphrtConsoleElement);
  }
}

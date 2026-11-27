/**
 * The admin console of one `graphrt serve --console`: the state a console
 * view shows and the calls that change it. No DOM here, so it also serves a
 * custom view.
 *
 * People sign in with their organization's account at the server
 * (`/console/login`); the browser then holds a session cookie, and every
 * request that changes something carries the session's CSRF token.
 */
import {
  type AuditEntry,
  type AuditQuery,
  type AuditVerification,
  type ConsoleRole,
  GraphrtClient,
  GraphrtError,
  type MembersView,
  type SecretsView,
  type SessionInfo,
  type ConsoleSession as SignedIn,
  sessionFetch,
  type UploadOptions,
  type WorkflowsView,
} from "../client/index.ts";

/** A view of the console. */
export type ConsoleTab = "agents" | "runs" | "secrets" | "people" | "audit";

export interface AuditState {
  entries: AuditEntry[];
  /** The cursor of the next page, when there may be one. */
  next?: number | undefined;
  query: AuditQuery;
  verification?: AuditVerification | undefined;
  error?: string | undefined;
}

/** What happened to the last thing the person did. */
export interface Outcome {
  text: string;
  tone: "good" | "bad";
}

export interface ConsoleState {
  /** `loading` until the server said who is signed in. */
  status: "loading" | "signed-out" | "ready" | "unavailable";
  /** Why the console cannot be used. */
  notice?: string | undefined;
  /** Who is signed in, the environments, where to sign in. */
  me?: SignedIn | undefined;
  role?: ConsoleRole | undefined;
  workflows?: WorkflowsView | undefined;
  secrets?: SecretsView | undefined;
  members?: MembersView | undefined;
  sessions?: SessionInfo[] | undefined;
  audit: AuditState;
  /** Why a view could not be loaded, by tab. */
  errors: Partial<Record<ConsoleTab, string>>;
  /** Calls in flight, e.g. `upload:support`, `secret:OPENAI_API_KEY`. */
  busy: string[];
  outcome?: Outcome | undefined;
  /** The server restarts to apply changes. */
  restarting: boolean;
}

export interface ConsoleSessionOptions {
  /** Where the server is. Default: the page's own origin. */
  baseUrl?: string;
  fetch?: typeof fetch;
  /** How long to wait for the server after a restart, in milliseconds. Default: 60 s. */
  restartTimeoutMs?: number;
}

type Listener = (state: ConsoleState) => void;

const ROLES: readonly ConsoleRole[] = ["viewer", "operator", "owner"];
const AUDIT_PAGE = 50;

function rank(role: ConsoleRole | undefined): number {
  return role === undefined ? -1 : ROLES.indexOf(role);
}

function asRole(role: string | undefined): ConsoleRole | undefined {
  return ROLES.find((known) => known === role);
}

/** A failure in words for the person. */
export function describe(error: unknown): string {
  if (error instanceof GraphrtError) {
    if (error.status === 401) {
      return "Your session ended. Sign in again.";
    }
    if (error.status === 403) {
      return error.message.includes("role") ? error.message : "Your role does not allow that.";
    }
    if (error.status === 0) {
      return "The connection was lost. Check your network and try again.";
    }
    return error.message;
  }
  return error instanceof Error && error.message !== "" ? error.message : "Something went wrong.";
}

const sleep = (ms: number): Promise<void> => new Promise((done) => setTimeout(done, ms));

/** The console of one server. Create it, {@link ConsoleSession.start} it, then call what the person does. */
export class ConsoleSession {
  readonly #options: ConsoleSessionOptions;
  readonly #listeners = new Set<Listener>();
  readonly #client: GraphrtClient;
  #csrf = "";
  #state: ConsoleState = {
    status: "loading",
    audit: { entries: [], query: {} },
    errors: {},
    busy: [],
    restarting: false,
  };

  constructor(options: ConsoleSessionOptions = {}) {
    this.#options = options;
    const base = options.fetch ?? ((input, init) => globalThis.fetch(input, init));
    this.#client = new GraphrtClient({
      ...(options.baseUrl === undefined ? {} : { baseUrl: options.baseUrl }),
      fetch: sessionFetch(() => this.#csrf, base),
    });
  }

  get state(): ConsoleState {
    return this.#state;
  }

  /** The `fetch` of this session, for the monitoring view inside the console. */
  get fetch(): typeof fetch {
    return sessionFetch(
      () => this.#csrf,
      this.#options.fetch ?? ((input, init) => globalThis.fetch(input, init)),
    );
  }

  subscribe(listener: Listener): () => void {
    this.#listeners.add(listener);
    return () => this.#listeners.delete(listener);
  }

  /** Whether the person's role is at least `role`. */
  can(role: ConsoleRole): boolean {
    return rank(this.#state.role) >= rank(role);
  }

  /** Where the browser goes to sign in, and comes back to `returnTo` (a path of this server). */
  signInUrl(returnTo = "/console/"): string | undefined {
    const url = this.#state.me?.sign_in_url;
    return url === undefined
      ? undefined
      : `${this.#options.baseUrl ?? ""}${url}?return_to=${encodeURIComponent(returnTo)}`;
  }

  /** Ask the server who is signed in. Never throws: what went wrong is in the state. */
  async start(): Promise<void> {
    let me: SignedIn;
    try {
      me = await this.#client.console.session();
    } catch (error) {
      const missing = error instanceof GraphrtError && error.status === 404;
      this.#set({
        status: "unavailable",
        notice: missing ? "This server has no admin console." : describe(error),
      });
      return;
    }
    const role = me.via === "session" ? asRole(me.role) : undefined;
    if (!me.signed_in || role === undefined) {
      this.#set({ status: "signed-out", me, role: undefined, notice: undefined });
      return;
    }
    this.#csrf = me.csrf_token ?? "";
    this.#set({ status: "ready", me, role, notice: undefined });
    await this.loadWorkflows();
  }

  /** End the session at the server; the state goes back to signing in. */
  async signOut(): Promise<void> {
    try {
      await this.#client.console.signOut();
    } catch {
      // Ended already: signing in again is what is left either way.
    }
    this.#csrf = "";
    this.#set({
      status: "signed-out",
      role: undefined,
      workflows: undefined,
      secrets: undefined,
      members: undefined,
      sessions: undefined,
      audit: { entries: [], query: {} },
    });
  }

  /** Load what the view `tab` shows. */
  async load(tab: ConsoleTab): Promise<void> {
    switch (tab) {
      case "agents":
        return this.loadWorkflows();
      case "secrets":
        return this.loadSecrets();
      case "people":
        return this.loadPeople();
      case "audit":
        return this.loadAudit(this.#state.audit.query);
      case "runs":
        return;
    }
  }

  // ---- agents --------------------------------------------------------------------------

  async loadWorkflows(): Promise<void> {
    await this.#loading("agents", async () =>
      this.#set({ workflows: await this.#client.console.workflows() }),
    );
  }

  /**
   * Upload a new version of workflow `name`.
   *
   * @returns Whether the server took it.
   */
  async upload(
    name: string,
    file: Blob | ArrayBuffer | Uint8Array | string,
    options: UploadOptions = {},
  ): Promise<boolean> {
    return this.#change(`upload:${name}`, async () => {
      const change = await this.#client.console.uploadVersion(name, file, options);
      const where = change.environment ? ` and runs in ${change.environment}` : "";
      const fresh = change.new === false ? "is version" : "is new version";
      const warnings = change.warnings?.length ? ` Warnings: ${change.warnings.join("; ")}` : "";
      await this.loadWorkflows();
      return `${name} ${fresh} ${change.version.version}${where}.${warnings}`;
    });
  }

  /** Run in `to` (default: the next environment) what `from` runs. */
  async promote(name: string, from: string, to?: string): Promise<boolean> {
    return this.#change(`promote:${name}`, async () => {
      const change = await this.#client.console.promote(name, from, to);
      await this.loadWorkflows();
      return `${name} version ${change.version.version} runs in ${change.environment ?? to ?? "the next environment"} now.`;
    });
  }

  /** Go back to the version `environment` ran before (or to `version`). */
  async rollback(name: string, environment: string, version?: number): Promise<boolean> {
    return this.#change(`rollback:${name}`, async () => {
      const change = await this.#client.console.rollback(name, environment, version);
      await this.loadWorkflows();
      return `${environment} runs version ${change.version.version} of ${name} again.`;
    });
  }

  /** Run `version` of `name` in `environment`. */
  async deploy(name: string, version: number, environment: string): Promise<boolean> {
    return this.#change(`deploy:${name}`, async () => {
      await this.#client.console.deploy(name, version, environment);
      await this.loadWorkflows();
      return `${environment} runs version ${version} of ${name} now.`;
    });
  }

  /** Where version `version` of `name` is downloaded. */
  versionFileUrl(name: string, version: number): string {
    return `${this.#options.baseUrl ?? ""}${this.#client.console.versionFileUrl(name, version)}`;
  }

  /**
   * Restart the server so that what changed takes effect, and wait until it
   * answers again.
   *
   * @returns Whether it came back.
   */
  async apply(): Promise<boolean> {
    if (this.#state.restarting) {
      return false;
    }
    try {
      await this.#client.console.apply();
    } catch (error) {
      this.#set({ outcome: { text: describe(error), tone: "bad" } });
      return false;
    }
    this.#set({ restarting: true, outcome: undefined });
    const deadline = Date.now() + (this.#options.restartTimeoutMs ?? 60_000);
    // The drain takes a moment before the listener closes: wait until the
    // server stops answering, then until it answers again.
    await sleep(500);
    while (Date.now() < deadline) {
      try {
        await this.#client.console.session();
        await this.loadWorkflows();
        const pending = this.#state.workflows?.pending ?? [];
        if (pending.length === 0 || Date.now() + 1000 >= deadline) {
          this.#set({
            restarting: false,
            outcome: { text: "The server restarted with the changes.", tone: "good" },
          });
          return true;
        }
      } catch {
        // Not back yet.
      }
      await sleep(500);
    }
    this.#set({
      restarting: false,
      outcome: { text: "The server did not come back in time. Check its log.", tone: "bad" },
    });
    return false;
  }

  // ---- secrets -------------------------------------------------------------------------

  async loadSecrets(): Promise<void> {
    if (!this.can("operator")) {
      return;
    }
    await this.#loading("secrets", async () => this.#set({ secrets: await this.#client.console.secrets() }));
  }

  /** Set secret `name`; the value is never shown again. */
  async setSecret(name: string, value: string): Promise<boolean> {
    return this.#change(`secret:${name}`, async () => {
      await this.#client.console.setSecret(name, value);
      await Promise.all([this.loadSecrets(), this.loadWorkflows()]);
      return `${name} is set. It applies when the server restarts.`;
    });
  }

  async deleteSecret(name: string): Promise<boolean> {
    return this.#change(`secret:${name}`, async () => {
      await this.#client.console.deleteSecret(name);
      await Promise.all([this.loadSecrets(), this.loadWorkflows()]);
      return `${name} is removed. It applies when the server restarts.`;
    });
  }

  // ---- people --------------------------------------------------------------------------

  async loadPeople(): Promise<void> {
    if (!this.can("owner")) {
      return;
    }
    await this.#loading("people", async () => {
      const [members, sessions] = await Promise.all([
        this.#client.console.members(),
        this.#client.console.sessions(),
      ]);
      this.#set({ members, sessions });
    });
  }

  /** Give `subject` a role, keep them out (`none`), or go back to their mapped role (`null`). */
  async setRole(subject: string, role: ConsoleRole | "none" | null): Promise<boolean> {
    return this.#change(`role:${subject}`, async () => {
      const member = await this.#client.console.setRole(subject, role);
      await this.loadPeople();
      const who = member.email ?? member.name ?? subject;
      return member.role
        ? `${who} is ${member.role === "owner" ? "an" : "a"} ${member.role} now.`
        : `${who} has no access now.`;
    });
  }

  async endSession(id: string): Promise<boolean> {
    return this.#change(`session:${id}`, async () => {
      await this.#client.console.endSession(id);
      await this.loadPeople();
      return "The session ended.";
    });
  }

  // ---- audit ---------------------------------------------------------------------------

  /** Load the newest entries that match `query`. */
  async loadAudit(query: AuditQuery = {}): Promise<void> {
    if (!this.can("owner")) {
      return;
    }
    try {
      const page = await this.#client.console.audit({ limit: AUDIT_PAGE, ...query });
      this.#set({ audit: { entries: page.entries, next: page.next_before_seq ?? undefined, query } });
    } catch (error) {
      this.#set({ audit: { ...this.#state.audit, query, error: describe(error) } });
    }
  }

  /** Load the next page of older entries. */
  async moreAudit(): Promise<void> {
    const audit = this.#state.audit;
    if (audit.next === undefined) {
      return;
    }
    try {
      const page = await this.#client.console.audit({
        limit: AUDIT_PAGE,
        ...audit.query,
        before_seq: audit.next,
      });
      this.#set({
        audit: {
          ...audit,
          entries: [...audit.entries, ...page.entries],
          next: page.next_before_seq ?? undefined,
        },
      });
    } catch (error) {
      this.#set({ audit: { ...audit, error: describe(error) } });
    }
  }

  /** Check the hash chain of the audit log. */
  async verifyAudit(): Promise<void> {
    try {
      const verification = await this.#client.console.verifyAudit();
      this.#set({ audit: { ...this.#state.audit, verification, error: undefined } });
    } catch (error) {
      this.#set({ audit: { ...this.#state.audit, error: describe(error) } });
    }
  }

  /** Where the JSON lines export of the current filter is downloaded. */
  auditExportUrl(): string {
    const { limit: _limit, before_seq: _cursor, ...query } = this.#state.audit.query;
    return `${this.#options.baseUrl ?? ""}${this.#client.console.auditExportUrl(query)}`;
  }

  /** Forget the outcome of the last action. */
  dismiss(): void {
    this.#set({ outcome: undefined });
  }

  // ---- internals -----------------------------------------------------------------------

  async #loading(tab: ConsoleTab, load: () => Promise<void>): Promise<void> {
    try {
      await load();
      this.#set({ errors: { ...this.#state.errors, [tab]: undefined } });
    } catch (error) {
      if (error instanceof GraphrtError && error.status === 401) {
        this.#csrf = "";
        this.#set({ status: "signed-out", role: undefined, notice: describe(error) });
        return;
      }
      this.#set({ errors: { ...this.#state.errors, [tab]: describe(error) } });
    }
  }

  async #change(id: string, change: () => Promise<string>): Promise<boolean> {
    if (this.#state.busy.includes(id)) {
      return false;
    }
    this.#set({ busy: [...this.#state.busy, id], outcome: undefined });
    try {
      const text = await change();
      this.#set({ outcome: { text, tone: "good" } });
      return true;
    } catch (error) {
      this.#set({ outcome: { text: describe(error), tone: "bad" } });
      return false;
    } finally {
      this.#set({ busy: this.#state.busy.filter((other) => other !== id) });
    }
  }

  #set(patch: Partial<ConsoleState>): void {
    this.#state = { ...this.#state, ...patch };
    for (const listener of this.#listeners) {
      listener(this.#state);
    }
  }
}

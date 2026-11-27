/**
 * The admin console of a server (`graphrt serve --console`): who is signed
 * in, the agent registry, secrets, people and the audit log. Reached as
 * `client.console`.
 *
 * In a browser the console uses the session cookie of its sign in, not a
 * token: give the client {@link sessionFetch}, which sends the cookie and the
 * session's CSRF token. Automation uses the API key as the client's `token`.
 */
import * as z from "zod/mini";
import type { CallOptions, GraphrtClient } from "./client.ts";
import type { Json } from "./schemas.ts";

const json: z.ZodMiniType<Json> = z.json();
const optionalText = z.optional(z.nullable(z.string()));

/** A console role. A newer server may know more. */
export type ConsoleRole = "owner" | "operator" | "viewer";

/** Who is signed in (`GET /v1/console/session`). */
export const consoleSessionSchema = z.looseObject({
  signed_in: z.boolean(),
  /** `session` for a person, `api-key` for the key. */
  via: z.optional(z.string()),
  role: z.optional(z.string()),
  csrf_token: z.optional(z.string()),
  user: z.optional(z.looseObject({ subject: z.string(), email: optionalText, name: optionalText })),
  expires_ms: z.optional(z.number()),
  environment: z.string(),
  environments: z.array(z.string()),
  sign_in_url: z.string(),
  secrets_key: z.optional(z.boolean()),
});
export type ConsoleSession = z.infer<typeof consoleSessionSchema>;

/** A version of a workflow in the registry. */
export const versionSchema = z.looseObject({
  version: z.number(),
  hash: z.string(),
  format: z.string(),
  size: z.number(),
  note: z.optional(z.string()),
  uploaded_ms: z.number(),
  uploaded_by: z.string(),
});
export type WorkflowVersion = z.infer<typeof versionSchema>;

/** One change of the version an environment runs. */
export const deploymentSchema = z.looseObject({
  version: z.number(),
  action: z.string(),
  from: z.optional(optionalText),
  at_ms: z.number(),
  by: z.string(),
});
export type Deployment = z.infer<typeof deploymentSchema>;

/** A workflow of the registry. */
export const registryWorkflowSchema = z.looseObject({
  name: z.string(),
  /** Newest first. */
  versions: z.array(versionSchema),
  environments: z.record(
    z.string(),
    z.looseObject({ active: z.optional(z.nullable(z.number())), history: z.array(deploymentSchema) }),
  ),
});
export type RegistryWorkflow = z.infer<typeof registryWorkflowSchema>;

/** A workflow the server serves now. */
export const servedWorkflowSchema = z.looseObject({
  name: z.string(),
  hash: z.optional(z.string()),
  version: z.optional(z.number()),
  env: z.array(z.string()),
});
export type ServedWorkflow = z.infer<typeof servedWorkflowSchema>;

/** `GET /v1/console/workflows`. */
export const workflowsViewSchema = z.looseObject({
  environment: z.string(),
  environments: z.array(z.string()),
  served: z.array(servedWorkflowSchema),
  registry: z.array(registryWorkflowSchema),
  /** What waits for a restart: `secrets`, `workflow:<name>`. */
  pending: z.array(z.string()),
});
export type WorkflowsView = z.infer<typeof workflowsViewSchema>;

/** The answer to an upload, a deploy, a promotion or a rollback. */
export const versionChangeSchema = z.looseObject({
  version: versionSchema,
  environment: z.optional(z.nullable(z.string())),
  new: z.optional(z.boolean()),
  warnings: z.optional(z.array(z.string())),
  restart_needed: z.boolean(),
});
export type VersionChange = z.infer<typeof versionChangeSchema>;

/** `GET /v1/console/secrets`: names, never values. */
export const secretsViewSchema = z.looseObject({
  key_configured: z.boolean(),
  secrets: z.array(
    z.looseObject({
      name: z.string(),
      updated_ms: z.number(),
      updated_by: z.string(),
      used_by: z.array(z.string()),
    }),
  ),
  /** Variables the served workflows declare, and whether the console sets them. */
  declared: z.array(
    z.looseObject({ name: z.string(), workflows: z.array(z.string()), in_console: z.boolean() }),
  ),
});
export type SecretsView = z.infer<typeof secretsViewSchema>;

/** Someone who signed in. */
export const memberSchema = z.looseObject({
  subject: z.string(),
  email: optionalText,
  name: optionalText,
  role: z.optional(z.nullable(z.string())),
  mapped_role: z.optional(z.nullable(z.string())),
  assigned_role: z.optional(z.nullable(z.string())),
  config_owner: z.boolean(),
  first_seen_ms: z.number(),
  last_sign_in_ms: z.number(),
});
export type Member = z.infer<typeof memberSchema>;

export const membersViewSchema = z.looseObject({
  members: z.array(memberSchema),
  mapping: z.looseObject({
    groups_claim: z.string(),
    groups: z.record(z.string(), z.string()),
    default_role: z.optional(z.nullable(z.string())),
    allowed_domains: z.array(z.string()),
  }),
});
export type MembersView = z.infer<typeof membersViewSchema>;

/** An open session. */
export const sessionInfoSchema = z.looseObject({
  id: z.string(),
  subject: z.string(),
  email: optionalText,
  name: optionalText,
  created_ms: z.number(),
  last_seen_ms: z.number(),
  expires_ms: z.number(),
});
export type SessionInfo = z.infer<typeof sessionInfoSchema>;

/** An entry of the audit log. */
export const auditEntrySchema = z.looseObject({
  seq: z.number(),
  at_ms: z.number(),
  actor: z.looseObject({
    kind: z.string(),
    id: z.optional(z.string()),
    email: optionalText,
    role: optionalText,
  }),
  action: z.string(),
  target: optionalText,
  outcome: z.string(),
  status: z.optional(z.nullable(z.number())),
  details: z.optional(json),
  prev: z.string(),
  hash: z.string(),
});
export type AuditEntry = z.infer<typeof auditEntrySchema>;

export const auditPageSchema = z.looseObject({
  entries: z.array(auditEntrySchema),
  next_before_seq: z.optional(z.nullable(z.number())),
});
export type AuditPage = z.infer<typeof auditPageSchema>;

/** What the check of the audit log's hash chain found. */
export const auditVerificationSchema = z.looseObject({
  entries: z.number(),
  broken_at_seq: z.optional(z.nullable(z.number())),
  first_seq: z.optional(z.nullable(z.number())),
  last_seq: z.optional(z.nullable(z.number())),
});
export type AuditVerification = z.infer<typeof auditVerificationSchema>;

/** What an audit query asks for. Every field narrows it. */
export interface AuditQuery {
  since_ms?: number;
  until_ms?: number;
  /** An id, an email, or a kind (`user`, `api-key`, `device`, `system`). */
  actor?: string;
  /** An action, or a group with a trailing dot (`secret.`). */
  action?: string;
  target?: string;
  outcome?: "ok" | "denied" | "failed";
  before_seq?: number;
  limit?: number;
}

/** Where a new version goes and what it says about itself. */
export interface UploadOptions extends CallOptions {
  /** The environment that runs it from now on (`none` for none). Default: the first one. */
  environment?: string;
  note?: string;
  /** `yaml`, `json` or `grt`. Default: told from the file. */
  format?: "yaml" | "json" | "grt";
}

/** The header that carries the session's CSRF token. */
export const CSRF_HEADER = "X-Graphrt-CSRF";

const SAFE_METHODS = new Set(["GET", "HEAD", "OPTIONS"]);

/**
 * A `fetch` for a signed in browser: it sends the session cookie (same origin
 * only) and, on every request that changes something, the session's CSRF token.
 *
 * @param csrf - The token of `GET /v1/console/session`, or a function that returns the current one.
 */
export function sessionFetch(
  csrf: string | (() => string | undefined),
  base: typeof fetch = (input, init) => globalThis.fetch(input, init),
): typeof fetch {
  return (input, init = {}) => {
    const method = (init.method ?? "GET").toUpperCase();
    const headers = new Headers(init.headers);
    const token = typeof csrf === "function" ? csrf() : csrf;
    if (!SAFE_METHODS.has(method) && token !== undefined && token !== "") {
      headers.set(CSRF_HEADER, token);
    }
    return base(input, { ...init, headers, credentials: "same-origin" });
  };
}

function path(name: string): string {
  return `/v1/console/workflows/${encodeURIComponent(name)}`;
}

/** The console API of a server. */
export class ConsoleApi {
  readonly #client: GraphrtClient;

  /** @internal */
  constructor(client: GraphrtClient) {
    this.#client = client;
  }

  /**
   * Who is signed in. Needs no credentials: a browser that is not signed in
   * learns where to sign in.
   *
   * @throws GraphrtError - `404` when the server has no console.
   */
  session(options: CallOptions = {}): Promise<ConsoleSession> {
    return this.#client.request("GET", "/v1/console/session", consoleSessionSchema, {
      signal: options.signal,
    });
  }

  /** End this browser's session. */
  signOut(options: CallOptions = {}): Promise<{ end_session_url?: string | null | undefined }> {
    const schema = z.looseObject({ end_session_url: z.optional(z.nullable(z.string())) });
    return this.#client.request("POST", "/v1/console/sign-out", schema, { signal: options.signal });
  }

  /** What is served, every version, and what each environment runs. */
  workflows(options: CallOptions = {}): Promise<WorkflowsView> {
    return this.#client.request("GET", "/v1/console/workflows", workflowsViewSchema, {
      signal: options.signal,
    });
  }

  /**
   * Upload a new version of workflow `name`: a YAML or JSON workflow, or a `.grt` bundle.
   *
   * @throws GraphrtError - `422` when it does not compile, `403` without the owner role.
   */
  uploadVersion(
    name: string,
    file: Blob | ArrayBuffer | Uint8Array | string,
    options: UploadOptions = {},
  ): Promise<VersionChange> {
    const { signal, ...query } = options;
    return this.#client.request("POST", `${path(name)}/versions`, versionChangeSchema, {
      raw: file,
      query: { ...query },
      signal,
    });
  }

  /** Run `version` in `environment`. */
  deploy(
    name: string,
    version: number,
    environment: string,
    options: CallOptions = {},
  ): Promise<VersionChange> {
    return this.#client.request("POST", `${path(name)}/deploy`, versionChangeSchema, {
      body: { version, environment },
      signal: options.signal,
    });
  }

  /** Run in `to` (default: the next environment) what `from` runs. */
  promote(name: string, from: string, to?: string, options: CallOptions = {}): Promise<VersionChange> {
    return this.#client.request("POST", `${path(name)}/promote`, versionChangeSchema, {
      body: to === undefined ? { from } : { from, to },
      signal: options.signal,
    });
  }

  /** Go back to the version `environment` ran before (or to `version`). */
  rollback(
    name: string,
    environment: string,
    version?: number,
    options: CallOptions = {},
  ): Promise<VersionChange> {
    return this.#client.request("POST", `${path(name)}/rollback`, versionChangeSchema, {
      body: version === undefined ? { environment } : { environment, version },
      signal: options.signal,
    });
  }

  /** Where a version's file is downloaded (with the session cookie). */
  versionFileUrl(name: string, version: number): string {
    return `${path(name)}/versions/${version}/file`;
  }

  /** Secret names, who changed them, which workflows declare them. Never values. */
  secrets(options: CallOptions = {}): Promise<SecretsView> {
    return this.#client.request("GET", "/v1/console/secrets", secretsViewSchema, { signal: options.signal });
  }

  /** Set secret `name`. It applies when the server restarts. */
  setSecret(name: string, value: string, options: CallOptions = {}): Promise<void> {
    return this.#client.request("PUT", `/v1/console/secrets/${encodeURIComponent(name)}`, undefined, {
      body: { value },
      signal: options.signal,
    });
  }

  deleteSecret(name: string, options: CallOptions = {}): Promise<void> {
    return this.#client.request("DELETE", `/v1/console/secrets/${encodeURIComponent(name)}`, undefined, {
      signal: options.signal,
    });
  }

  /** Everyone who signed in, and how roles are mapped. */
  members(options: CallOptions = {}): Promise<MembersView> {
    return this.#client.request("GET", "/v1/console/members", membersViewSchema, { signal: options.signal });
  }

  /**
   * Set someone's role, `none` to keep them out, or `null` for the role their groups give.
   *
   * @throws GraphrtError - `409` for an owner of the config or the last owner.
   */
  setRole(subject: string, role: ConsoleRole | "none" | null, options: CallOptions = {}): Promise<Member> {
    return this.#client.request("PUT", `/v1/console/members/${encodeURIComponent(subject)}`, memberSchema, {
      body: { role },
      signal: options.signal,
    });
  }

  /** The open sessions. */
  async sessions(options: CallOptions = {}): Promise<SessionInfo[]> {
    const schema = z.looseObject({ sessions: z.array(sessionInfoSchema) });
    const answer = await this.#client.request("GET", "/v1/console/sessions", schema, {
      signal: options.signal,
    });
    return answer.sessions;
  }

  endSession(id: string, options: CallOptions = {}): Promise<void> {
    return this.#client.request("DELETE", `/v1/console/sessions/${encodeURIComponent(id)}`, undefined, {
      signal: options.signal,
    });
  }

  /** Audit entries, newest first. */
  audit(query: AuditQuery = {}, options: CallOptions = {}): Promise<AuditPage> {
    return this.#client.request("GET", "/v1/console/audit", auditPageSchema, {
      query: { ...query },
      signal: options.signal,
    });
  }

  /** Where the JSON lines export of `query` is downloaded (with the session cookie). */
  auditExportUrl(query: AuditQuery = {}): string {
    const params = new URLSearchParams();
    for (const [key, value] of Object.entries(query)) {
      if (value !== undefined) {
        params.set(key, String(value));
      }
    }
    const text = params.toString();
    return `/v1/console/audit/export${text === "" ? "" : `?${text}`}`;
  }

  /** Check the audit log's hash chain. */
  verifyAudit(options: CallOptions = {}): Promise<AuditVerification> {
    return this.#client.request("GET", "/v1/console/audit/verify", auditVerificationSchema, {
      signal: options.signal,
    });
  }

  /** Restart the server so that secrets and versions take effect. */
  apply(options: CallOptions = {}): Promise<{ pending: string[] }> {
    const schema = z.looseObject({ pending: z.array(z.string()) });
    return this.#client.request("POST", "/v1/console/apply", schema, { signal: options.signal });
  }
}

/**
 * A fake OpenID Connect provider for the console tests, and a browser's
 * cookie jar: the console signs people in against it like against a real one
 * (discovery, PKCE, ES256 ID tokens from a JWKS).
 */
import { createHash, generateKeyPairSync, type KeyObject, sign } from "node:crypto";
import { writeFile } from "node:fs/promises";
import { createServer, type IncomingMessage, type Server, type ServerResponse } from "node:http";
import { join } from "node:path";

export interface Person {
  sub: string;
  email?: string;
  groups?: string[];
}

export interface FakeProvider {
  issuer: string;
  /** Who signs in at the next authorization request. */
  next(person: Person): void;
  stop(): Promise<void>;
}

/** Node's own `fetch`, kept: a test may put a cookie jar in its place. */
const nodeFetch = globalThis.fetch;

const base64url = (bytes: Buffer | string): string => Buffer.from(bytes).toString("base64url");

/** Start the provider on a free local port. */
export async function startProvider(): Promise<FakeProvider> {
  const { privateKey, publicKey } = generateKeyPairSync("ec", { namedCurve: "P-256" });
  const jwk = { ...publicKey.export({ format: "jwk" }), kid: "k1", alg: "ES256", use: "sig" };
  const codes = new Map<string, { person: Person; nonce: string; challenge: string }>();
  let nextPerson: Person | undefined;
  let issuer = "";
  const server: Server = createServer((request, response) => {
    void handle(request, response);
  });
  async function handle(request: IncomingMessage, response: ServerResponse): Promise<void> {
    const url = new URL(request.url ?? "/", issuer);
    const json = (status: number, body: unknown): void => {
      response.writeHead(status, { "content-type": "application/json" }).end(JSON.stringify(body));
    };
    if (url.pathname === "/.well-known/openid-configuration") {
      json(200, {
        issuer,
        authorization_endpoint: `${issuer}/authorize`,
        token_endpoint: `${issuer}/token`,
        jwks_uri: `${issuer}/jwks`,
        token_endpoint_auth_methods_supported: ["client_secret_post"],
      });
    } else if (url.pathname === "/jwks") {
      json(200, { keys: [jwk] });
    } else if (url.pathname === "/authorize") {
      const person = nextPerson;
      const redirect = url.searchParams.get("redirect_uri");
      if (person === undefined || redirect === null) {
        json(400, { error: "nobody signs in" });
        return;
      }
      const code = base64url(createHash("sha256").update(`${Math.random()}${Date.now()}`).digest());
      codes.set(code, {
        person,
        nonce: url.searchParams.get("nonce") ?? "",
        challenge: url.searchParams.get("code_challenge") ?? "",
      });
      const back = new URL(redirect);
      back.searchParams.set("code", code);
      back.searchParams.set("state", url.searchParams.get("state") ?? "");
      response.writeHead(302, { location: back.href }).end();
    } else if (url.pathname === "/token" && request.method === "POST") {
      let body = "";
      for await (const chunk of request) {
        body += String(chunk);
      }
      const form = new URLSearchParams(body);
      const grant = codes.get(form.get("code") ?? "");
      codes.delete(form.get("code") ?? "");
      const verifier = form.get("code_verifier") ?? "";
      if (
        grant === undefined ||
        base64url(createHash("sha256").update(verifier).digest()) !== grant.challenge
      ) {
        json(400, { error: "invalid_grant" });
        return;
      }
      const claims = {
        ...grant.person,
        iss: issuer,
        aud: form.get("client_id"),
        exp: Math.floor(Date.now() / 1000) + 300,
        iat: Math.floor(Date.now() / 1000),
        nonce: grant.nonce,
      };
      json(200, { access_token: "a", token_type: "Bearer", id_token: idToken(privateKey, claims) });
    } else {
      json(404, { error: "not found" });
    }
  }
  await new Promise<void>((done) => server.listen(0, "127.0.0.1", done));
  const address = server.address();
  issuer = `http://127.0.0.1:${typeof address === "object" && address !== null ? address.port : 0}`;
  return {
    issuer,
    next: (person) => {
      nextPerson = person;
    },
    stop: () => new Promise((done) => server.close(() => done())),
  };
}

function idToken(key: KeyObject, claims: Record<string, unknown>): string {
  const input = `${base64url(JSON.stringify({ alg: "ES256", kid: "k1", typ: "JWT" }))}.${base64url(JSON.stringify(claims))}`;
  const signature = sign("sha256", Buffer.from(input), { key, dsaEncoding: "ieee-p1363" });
  return `${input}.${base64url(signature)}`;
}

/** The flags and environment of a server with a console that signs in with `provider`. */
export function consoleServer(provider: FakeProvider): {
  env: Record<string, string>;
  setup: (dir: string, url: string) => Promise<string[]>;
} {
  return {
    env: {
      GRAPHRT_CONSOLE_KEY: "AAECAwQFBgcICQoLDA0ODxAREhMUFRYXGBkaGxwdHh8=",
      GRAPHRT_TEST_OIDC_SECRET: "client-secret",
    },
    setup: async (dir, url) => {
      const config = join(dir, "console.yaml");
      await writeFile(
        config,
        [
          `public_url: ${url}`,
          "environments: [dev, prod]",
          "environment: dev",
          `data_dir: ${join(dir, "console")}`,
          "oidc:",
          `  issuer: ${provider.issuer}`,
          "  client_id: graphrt",
          "  client_secret_env: GRAPHRT_TEST_OIDC_SECRET",
          "roles:",
          "  owners: [owner@example.com]",
          "  groups: {ops: operator, staff: viewer}",
          "",
        ].join("\n"),
      );
      return ["--console", config, "--store", join(dir, "store"), "--monitor", "--drain-timeout", "2s"];
    },
  };
}

/** A `fetch` that keeps cookies like a browser of one site does. */
export function cookieJar(): typeof fetch & { cookies: Map<string, string> } {
  const cookies = new Map<string, string>();
  const jarFetch = async (input: string | URL | Request, init: RequestInit = {}): Promise<Response> => {
    const headers = new Headers(init.headers);
    if (cookies.size > 0) {
      headers.set("cookie", [...cookies].map(([name, value]) => `${name}=${value}`).join("; "));
    }
    const response = await nodeFetch(input, { ...init, headers, redirect: "manual" });
    for (const line of response.headers.getSetCookie()) {
      const [pair = ""] = line.split(";");
      const at = pair.indexOf("=");
      const name = pair.slice(0, at);
      const value = pair.slice(at + 1);
      if (/max-age=0/i.test(line) || value === "") {
        cookies.delete(name);
      } else {
        cookies.set(name, value);
      }
    }
    return response;
  };
  return Object.assign(jarFetch, { cookies });
}

/** Sign `person` in at the console of `server` with the browser of `jar`. */
export async function signIn(
  server: string,
  provider: FakeProvider,
  jar: typeof fetch,
  person: Person,
): Promise<Response> {
  provider.next(person);
  const login = await jar(`${server}/console/login`);
  const authorize = await nodeFetch(login.headers.get("location") ?? "", { redirect: "manual" });
  return jar(authorize.headers.get("location") ?? "");
}

/**
 * A push service for tests: it keeps the messages it receives, and can read
 * them as the browser that subscribed would (RFC 8291), with Node's own
 * crypto rather than the server's code.
 */
import { createDecipheriv, createECDH, type ECDH, hkdfSync, randomBytes } from "node:crypto";
import { createServer, type Server } from "node:http";
import type { AddressInfo } from "node:net";

export interface ReceivedPush {
  path: string;
  headers: Record<string, string | string[] | undefined>;
  body: Buffer;
}

/** A browser's side of a subscription: its key pair and authentication secret. */
export class TestBrowser {
  readonly #ecdh: ECDH;
  readonly #auth: Buffer;
  readonly endpoint: string;

  constructor(endpoint: string) {
    this.endpoint = endpoint;
    this.#ecdh = createECDH("prime256v1");
    this.#ecdh.generateKeys();
    this.#auth = randomBytes(16);
  }

  /** What `PushSubscription.toJSON()` gives. */
  subscription(): { endpoint: string; keys: { p256dh: string; auth: string } } {
    return {
      endpoint: this.endpoint,
      keys: {
        p256dh: this.#ecdh.getPublicKey().toString("base64url"),
        auth: this.#auth.toString("base64url"),
      },
    };
  }

  /** The text of an `aes128gcm` push message sent to this browser. */
  decrypt(body: Buffer): string {
    const salt = body.subarray(0, 16);
    const keyLength = body.readUInt8(20);
    const senderKey = body.subarray(21, 21 + keyLength);
    const sealed = body.subarray(21 + keyLength);
    const ownKey = this.#ecdh.getPublicKey();
    const secret = this.#ecdh.computeSecret(senderKey);
    const info = Buffer.concat([Buffer.from("WebPush: info\0"), ownKey, senderKey]);
    const ikm = Buffer.from(hkdfSync("sha256", secret, this.#auth, info, 32));
    const key = Buffer.from(hkdfSync("sha256", ikm, salt, "Content-Encoding: aes128gcm\0", 16));
    const nonce = Buffer.from(hkdfSync("sha256", ikm, salt, "Content-Encoding: nonce\0", 12));
    const decipher = createDecipheriv("aes-128-gcm", key, nonce);
    decipher.setAuthTag(sealed.subarray(sealed.length - 16));
    const padded = Buffer.concat([decipher.update(sealed.subarray(0, sealed.length - 16)), decipher.final()]);
    // The last record ends with the delimiter 0x02.
    return padded.subarray(0, padded.lastIndexOf(2)).toString("utf8");
  }
}

export interface PushService {
  /** `http://127.0.0.1:<port>`. */
  url: string;
  received: ReceivedPush[];
  /** Wait until `count` messages arrived. */
  waitFor(count: number): Promise<ReceivedPush[]>;
  stop(): Promise<void>;
}

/** Start a push service on a local port. It answers `201`, like a real one. */
export async function startPushService(): Promise<PushService> {
  const received: ReceivedPush[] = [];
  const server: Server = createServer((request, response) => {
    const chunks: Buffer[] = [];
    request.on("data", (chunk: Buffer) => chunks.push(chunk));
    request.on("end", () => {
      received.push({ path: request.url ?? "", headers: request.headers, body: Buffer.concat(chunks) });
      response.writeHead(201).end();
    });
  });
  await new Promise<void>((listening) => server.listen(0, "127.0.0.1", listening));
  const { port } = server.address() as AddressInfo;
  return {
    url: `http://127.0.0.1:${port}`,
    received,
    async waitFor(count) {
      for (let attempt = 0; attempt < 400 && received.length < count; attempt += 1) {
        await new Promise((done) => setTimeout(done, 25));
      }
      if (received.length < count) {
        throw new Error(`expected ${count} push message(s), got ${received.length}`);
      }
      return received;
    },
    stop: () => new Promise<void>((closed) => server.close(() => closed())),
  };
}

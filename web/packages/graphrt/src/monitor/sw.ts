/**
 * The service worker of the monitoring page (`/monitor/sw.js`): it shows the
 * push notifications of `graphrt serve` and opens the run when one is tapped.
 *
 * It caches nothing and handles no request: what the page shows is live data
 * of the server, and a stale copy would be wrong.
 */
import { notificationOf } from "./notification.ts";

/** The parts of the service worker scope this file uses. */
interface WorkerClient {
  url: string;
  focus(): Promise<unknown>;
  postMessage(message: unknown): void;
}

interface WorkerScope {
  registration: {
    scope: string;
    showNotification(
      title: string,
      options: { body: string; tag: string; data: { open: string } },
    ): Promise<void>;
  };
  clients: {
    matchAll(options: { type: "window"; includeUncontrolled: boolean }): Promise<WorkerClient[]>;
    openWindow(url: string): Promise<unknown>;
    claim(): Promise<void>;
  };
  skipWaiting(): Promise<void>;
  addEventListener(type: string, listener: (event: WorkerEvent) => void): void;
}

interface WorkerEvent {
  waitUntil(promise: Promise<unknown>): void;
  data?: { text(): string } | null;
  notification?: { close(): void; data?: { open?: string } };
}

const worker = globalThis as unknown as WorkerScope;

worker.addEventListener("install", (event) => {
  // A new version takes over at once: the page it serves is always the server's own.
  event.waitUntil(worker.skipWaiting());
});

worker.addEventListener("activate", (event) => {
  event.waitUntil(worker.clients.claim());
});

worker.addEventListener("push", (event) => {
  const shown = notificationOf(event.data?.text());
  event.waitUntil(
    worker.registration.showNotification(shown.title, {
      body: shown.body,
      tag: shown.tag,
      data: { open: shown.open },
    }),
  );
});

worker.addEventListener("notificationclick", (event) => {
  event.notification?.close();
  const open = event.notification?.data?.open ?? "#/runs";
  event.waitUntil(
    (async () => {
      const windows = await worker.clients.matchAll({ type: "window", includeUncontrolled: true });
      const page = windows.find((client) => client.url.startsWith(worker.registration.scope));
      if (page !== undefined) {
        page.postMessage({ open });
        await page.focus();
      } else {
        await worker.clients.openWindow(worker.registration.scope + open);
      }
    })(),
  );
});

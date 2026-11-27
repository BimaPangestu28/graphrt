/**
 * The service worker of the monitoring page (`/monitor/sw.js`): it shows the
 * push notifications of `graphrt serve` and opens the run when one is tapped.
 *
 * It caches nothing and handles no request: what the page shows is live data
 * of the server, and a stale copy would be wrong.
 */
import { notificationOf } from "./notification.js";
const worker = globalThis;
worker.addEventListener("install", (event) => {
    // A new version takes over at once: the page it serves is always the server's own.
    event.waitUntil(worker.skipWaiting());
});
worker.addEventListener("activate", (event) => {
    event.waitUntil(worker.clients.claim());
});
worker.addEventListener("push", (event) => {
    const shown = notificationOf(event.data?.text());
    event.waitUntil(worker.registration.showNotification(shown.title, {
        body: shown.body,
        tag: shown.tag,
        data: { open: shown.open },
    }));
});
worker.addEventListener("notificationclick", (event) => {
    event.notification?.close();
    const open = event.notification?.data?.open ?? "#/runs";
    event.waitUntil((async () => {
        const windows = await worker.clients.matchAll({ type: "window", includeUncontrolled: true });
        const page = windows.find((client) => client.url.startsWith(worker.registration.scope));
        if (page !== undefined) {
            page.postMessage({ open });
            await page.focus();
        }
        else {
            await worker.clients.openWindow(worker.registration.scope + open);
        }
    })());
});

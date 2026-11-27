/**
 * `@graphrt/monitor`: a monitoring view for agents served by `graphrt serve`,
 * as a web component.
 *
 * - `<graphrt-monitor>` shows the runs, what a run did, and the requests that
 *   wait for a person, and lets the person answer them.
 * - `MonitorSession` is the logic without a view.
 *
 * A server started with `--monitor` serves it ready to use at `/monitor/`,
 * installable as an app, with push notifications.
 */
export { ago, defineGraphrtMonitor, expiryNote, GraphrtMonitorElement, timeLeft } from "./monitor-element.js";
export { notificationOf } from "./notification.js";
export { base64UrlBytes, browserPush } from "./push.js";
export { encodeQr, qrCapacity, qrSvg } from "./qr.js";
export { eventLine, MonitorSession, } from "./session.js";

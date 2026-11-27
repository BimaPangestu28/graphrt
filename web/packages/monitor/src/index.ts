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
export { ago, defineGraphrtMonitor, GraphrtMonitorElement, timeLeft } from "./monitor-element.ts";
export { notificationOf, type ShownNotification } from "./notification.ts";
export { base64UrlBytes, browserPush } from "./push.ts";
export { encodeQr, type QrCode, type QrOptions, qrCapacity, qrSvg } from "./qr.ts";
export {
  type EventLine,
  eventLine,
  type KeyValueStore,
  MonitorSession,
  type MonitorSessionOptions,
  type MonitorState,
  type PushBrowser,
  type PushState,
  type RunDetail,
} from "./session.ts";

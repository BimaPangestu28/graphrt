# @graphrt/monitor

A monitoring view for agents served by `graphrt serve --monitor`, as a web
component: the runs, what each did, and the requests that wait for a person.

```html
<graphrt-monitor server="https://agent.example.com"></graphrt-monitor>
```

```ts
import { browserPush, defineGraphrtMonitor } from "@graphrt/monitor";

defineGraphrtMonitor();
const monitor = document.querySelector("graphrt-monitor");
monitor.push = browserPush(); // needs a service worker on the page
```

| Export | What |
| --- | --- |
| `<graphrt-monitor>` | The view, to place in a page |
| `MonitorSession` | The logic without a view, for a view of your own design |
| `browserPush` | Push notifications of the browser, for `monitor.push` |
| `notificationOf` | What a service worker shows for a push message of the server |
| `encodeQr`, `qrSvg` | The QR code of a pairing link, drawn in the page |

A server started with `--monitor` serves it ready to use at `/monitor/`: it
can be installed as an app, and with `--push` it notifies the device when a
run fails or waits for a person.

See [docs/monitoring.md](../../../docs/monitoring.md).

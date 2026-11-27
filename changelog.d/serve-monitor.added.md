- `graphrt serve --monitor`: a monitoring page at `/monitor/` for a phone or a
  desktop, embedded in the binary: the runs, what each did (live while it
  executes), and the requests that wait for a person, answered in place. It
  can be installed as an app. See [docs/monitoring.md](docs/monitoring.md).
- `graphrt serve --devices`: device tokens for monitoring. `POST /v1/devices`
  (with the API key) pairs a device and answers its token once, with a pairing
  payload and link to show as a QR code. A device token only reads runs and
  resolves approvals, is stored hashed, expires when asked to, and ends when
  its device is removed (`DELETE /v1/devices/{id}`). `--public-url` names the
  address pairing links use.
- `graphrt serve --approvals`: `GET /v1/approvals` lists every run that waits
  for a person across workflows, with what is asked and when it expires, and
  `POST /v1/approvals/{run_id}` answers one. `--expire-approvals` carries out
  `on_expire` of `human-input` requests past their time limit: it answers with
  the named action, or stops the run.
- `graphrt serve --push CONTACT`: Web Push notifications to subscribed browsers
  when a run fails or waits for a person, encrypted for the browser and signed
  with the server's own key (VAPID), without a vendor account. The `Notifier`
  trait behind it takes further channels.
- `web/`: `graphrt/monitor`, the monitoring view (a QR encoder, push for
  browsers). The client has `approvals`, `devices`, `push` and `listEvents`.

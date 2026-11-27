- `graphrt serve --channel telegram=WORKFLOW` and `--channel whatsapp=WORKFLOW`:
  a customer writes on Telegram or WhatsApp, the workflow runs and the answer
  goes back on the same channel, with one conversation per customer kept by
  the server. Telegram works by long polling (no public address needed) or by
  a webhook with a secret, WhatsApp by the signed webhook of the Cloud API.
  Answers are split to the channel's length limit and converted from Markdown,
  UI blocks go out as text with numbered choices, files go both ways (with
  `--workspace`), and a typing sign shows while the run works. A paused run
  asks the customer (`--user-resume`) or the team (the approvals inbox, or
  `approvers` in the channel). Each message is handled once, each customer has
  a rate limit and each channel a budget (`--channel-rate-limit`,
  `--channel-budget`), and senders can be restricted. `--channels FILE`
  configures several bots and the server's own sentences. Cargo features
  `channel-telegram` and `channel-whatsapp`. See
  [docs/channels.md](docs/channels.md).
- `graphrt serve`: `GET /v1/channels`, `POST /v1/channels/{name}/handoff` and
  `POST /v1/channels/{name}/messages` let a person of the team take a channel
  conversation over and write to the customer.
- New crate `graphrt-channel`: what a host needs to speak Telegram (Bot API)
  and WhatsApp (Cloud API): payload parsing, the webhook secret and signature
  checks, the 24 hour window, text splitting and Markdown conversion, and a
  `Channel` trait to send text, buttons and files and to download media. It
  depends on no other Graphrt crate.
- Conversations kept by the server have a `channel` field (name, chat, the
  customer's display name) when they happen on a messaging channel.

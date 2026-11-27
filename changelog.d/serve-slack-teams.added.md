- `graphrt serve --channel slack=WORKFLOW` and `--channel teams=WORKFLOW`: the
  enterprise counterpart of the Telegram and WhatsApp channels. Slack works by
  Socket Mode (no public address needed) or by the signed Events API, Teams by
  the messaging endpoint of an Azure Bot with the Bot Framework token checked
  against Microsoft's published keys. Direct messages and mentions in channels
  are answered, and a thread is one conversation. Adaptive Cards in an answer
  are drawn as Block Kit in Slack and as cards in Teams, and approvals are
  buttons that resume the paused run. See [docs/slack-teams.md](docs/slack-teams.md).
- Channels can notify: `notify` targets in the channels file hear when any run
  of the server fails or waits for a person, in Slack, Teams or Telegram, next
  to or instead of Web Push. A target that is an approver gets the approval as
  buttons. A channel without `workflow` only sends notifications.
- `graphrt-channel`: features `slack`, `socket-mode` and `teams` (the Slack
  signature check, events, taps, `mrkdwn` and Block Kit, a Socket Mode client,
  the Bot Framework token check, activities and Adaptive Card answers), and
  `Channel::send_rich` and `Channel::direct_chat` with defaults for the
  channels that have no cards.
- `graphrt deploy --direct --channel slack|teams`: a direct deployment answers
  in Slack (the Events API, or Socket Mode with `--mode always-on`) and Teams,
  with the credentials as Fly secrets.
- Files in Teams: a 1:1 chat gets a file consent card and the file lands in
  the person's OneDrive, a channel or group chat gets a download link
  (`GET /v1/channels/{name}/files/{token}`, with `--public-url`). Any channel
  falls back to such a link when a file could not be sent.

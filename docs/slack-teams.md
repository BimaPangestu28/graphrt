# A served agent in Slack and Microsoft Teams

`graphrt serve` can put a workflow in Slack or Teams, the same way it puts one
on [Telegram and WhatsApp](channels.md). A colleague writes to the app in a
direct message or mentions it in a channel, the workflow runs, and the answer
comes back in the same place. Approvals are buttons in the chat, and the server
can tell people in Slack or Teams when a run failed or waits for them.

Everything in [channels.md](channels.md) holds here too: one conversation per
person (per thread in a channel), each message handled once, rate limits and
budgets, files, the server's own sentences, hand over to a person. This page
is what is different.

| You want | Start `graphrt serve` with | You need |
| --- | --- | --- |
| A Slack app, on a laptop or behind a company firewall | `--channel slack=WORKFLOW` | A Slack app, a bot token and an app token (Socket Mode) |
| A Slack app on several replicas | the above, with `SLACK_SIGNING_SECRET` instead of the app token | A public HTTPS address |
| A Teams bot | `--channel teams=WORKFLOW` | An Azure Bot (its app id and client secret), a public HTTPS address |
| "Run failed" and "needs approval" in Slack or Teams | a channel with `notify` in [the channels file](#notifications) | One of the above |

```sh
SLACK_BOT_TOKEN=xoxb-... SLACK_APP_TOKEN=xapp-... \
  graphrt serve flows/ --channel slack=support --store ./runs
```

The cargo features are `channel-slack` and `channel-teams`. They are in the
default build and in the released binaries, and they are off until a
`--channel` or `--channels` names them. A build without them has none of
their code. Only the official APIs are used: the Slack Web API, Events API and
Socket Mode, and the Bot Framework of Azure Bot Service.

## Slack step by step

1. Open [api.slack.com/apps](https://api.slack.com/apps), choose **Create New
   App**, then **From scratch**. Name it and pick your workspace.
2. **Socket Mode** (left menu): turn it on. Slack asks for an app level token:
   give it the scope `connections:write`. Copy the token, it starts with `xapp-`.
3. **OAuth & Permissions**: under Bot Token Scopes add `app_mentions:read`,
   `chat:write`, `im:history`, `im:read` and `im:write`. Add `files:read` and
   `files:write` when people send files or the agent sends them. Choose
   **Install to Workspace** and copy the Bot User OAuth Token (`xoxb-...`).
4. **Event Subscriptions**: turn it on and subscribe the bot to
   `app_mention` and `message.im`.
5. **App Home**: under Show Tabs, allow people to send messages from the
   Messages tab.
6. **Interactivity & Shortcuts**: turn it on (with Socket Mode no address is
   needed). Buttons in answers and approvals need it.
7. Start the server:

   ```sh
   export SLACK_BOT_TOKEN=xoxb-...
   export SLACK_APP_TOKEN=xapp-...
   graphrt serve examples/channels/support.yaml --channel slack=support --store ./runs
   ```

Write to the app in Slack, or invite it to a channel (`/invite @your-app`)
and mention it. With Socket Mode the server connects out to Slack, so it needs
no public address.

### Slack with the Events API

To run several replicas, or where outgoing WebSockets are not allowed, let
Slack post instead. Leave Socket Mode off, copy the **Signing Secret** from
Basic Information, and point both Event Subscriptions and Interactivity to
the server:

```sh
export SLACK_BOT_TOKEN=xoxb-...
export SLACK_SIGNING_SECRET=...
graphrt serve flows/ --channel slack=support --store ./runs --public-url https://agent.example.com
```

The Request URL is `https://agent.example.com/v1/channels/slack/webhook`, for
events and for interactivity. Slack checks it once (`url_verification`) and the
server answers. Every delivery is signed: the server checks the signature on
the raw body and refuses one more than five minutes old, so a recorded
delivery cannot be played again. Slack sends a delivery again when it did not
hear back in three seconds: the server answers at once and handles each
message once.

### What a conversation is in Slack

| Where | The conversation | The answer |
| --- | --- | --- |
| A direct message to the app | the DM | in the DM |
| A thread inside the DM | that thread | in the thread |
| A mention in a channel | a new thread under the message | in that thread |
| A mention inside that thread | the same thread, with its history | in the thread |

In a channel the app reads only messages that mention it. Several people can
talk to it in one thread, and the thread keeps one history. `sys.user_id` is
`slack:<user id>` of whoever wrote the message that started the run.

## Teams step by step

1. In the [Azure portal](https://portal.azure.com), create an **Azure Bot**.
   Choose **Single Tenant** and let it create a Microsoft App ID.
2. In the bot's **Configuration**, copy the **Microsoft App ID** and the
   **App Tenant ID**. Choose **Manage Password**, create a client secret and
   copy its value.
3. Set the **Messaging endpoint** to
   `https://agent.example.com/v1/channels/teams/webhook`.
4. Under **Channels**, add **Microsoft Teams**.
5. Start the server:

   ```sh
   export TEAMS_APP_ID=00000000-...
   export TEAMS_APP_PASSWORD=...
   export TEAMS_TENANT_ID=aaaaaaaa-...
   graphrt serve flows/ --channel teams=support --store ./runs --public-url https://agent.example.com
   ```

6. To have the bot in your Teams, upload an app package (a `manifest.json` with
   the App ID as `botId`, and two icons) in the Teams admin center or in
   Teams under Apps, **Manage your apps**, **Upload an app**. The Developer
   Portal for Teams can make the package for you.

Teams posts every message to the endpoint with a token of the Bot Framework.
The server checks it before reading anything: signed with one of Microsoft's
published keys (fetched and refreshed by the server), issued for this bot's
App ID, not expired, endorsed for Teams, and naming the service address the
answer goes to. A single tenant bot also refuses messages from another
organisation.

### What a conversation is in Teams

| Where | The conversation |
| --- | --- |
| A 1:1 chat with the bot | that chat |
| A mention of the bot in a channel post, or in a reply to it | the post's thread |
| A group chat | the group chat |

A person's id is their Microsoft Entra object id, the one an admin sees in the
Entra admin center. Use it in `allow`, `approvers` and `notify`.
`sys.user_id` is `teams:<object id>`.

## What people get

- **Text** is converted from the Markdown a model writes: Slack `mrkdwn`
  (bold, italic, links, lists, with `<`, `>` and `&` escaped, so an answer can
  never ping `@channel` or mention someone), Teams Markdown as it is.
- **Adaptive Cards** in an answer are drawn: in Teams as the card itself, in
  Slack as Block Kit (text, a large title as a header, facts as fields, `https`
  images, links and the card's choices as buttons). A card Block Kit cannot
  show (a text field, a toggle) goes out as text with numbered choices, as on
  WhatsApp. Tables, stats and charts go out as text.
- **Buttons** send the choice's number, so a tap and a typed number mean the
  same. A tapped approval card is replaced by the choice, so nobody taps it twice.
- **Typing.** Teams shows the bot typing while the run works. Slack shows no
  typing sign for apps outside its assistant pane.
- **Files** go into the workspace (`--workspace`) as in [channels.md](channels.md#files-from-the-customer).
  In Slack the app needs `files:read`. An answer's files are uploaded to Slack.
  In a Teams 1:1 chat a picture is shown inline, and any other file comes as a
  **file consent card**: the person taps Allow, the bot puts the file in their
  OneDrive and sends it as a file card. The bot app needs `supportsFiles: true`
  in its manifest. A Teams channel or group chat takes no file from a bot, so
  it gets a **download link** instead, when the server knows its address
  (`--public-url`). The same link goes out on any channel where sending the
  file failed.

### Download links

A link is `<public url>/v1/channels/<name>/files/<token>`. The token is 128
random bits, names one file of the workspace for one channel and works for
seven days, without an API key: whoever has the link can download the file,
as with a shared link in any chat. Links live in memory and end with the
server process.

## Approvals

A run that waits for a person works as on the other channels: the person who
wrote is told the team is asked, and the request goes to the approvals inbox and
to the channel's `approvers`. In Slack and Teams the approver gets the question
with **buttons** (Block Kit, or an Adaptive Card with `Action.Submit`), and the
tap resumes the run. A `human-input` node with a card shows the card itself.

In Teams the bot opens a 1:1 chat with an approver who never wrote to it. That
needs `TEAMS_TENANT_ID`, and the bot app must be installed for that person
(the admin can install it for everyone).

## Notifications

A channel with `notify` tells people when **any** run of the server fails or
waits for a person, not only runs of its own conversations. It is the Slack or
Teams counterpart of Web Push (`--push`), and both can be on.

```yaml
channels:
  - kind: slack
    name: ops
    notify: ["C0123OPS", "U07LEAD"]   # a channel and a person
    approvers: ["U07LEAD"]            # gets the approval with buttons
  - kind: teams
    notify: ["7d0c...-object-id"]     # a person, or a conversation id (19:...)
```

A channel without `workflow` only sends notifications: it answers its
approvers' taps and ignores everything else. A target that is also an
approver gets each approval as a question with buttons, and the tap resumes
the run. Every other target gets a short notice with the run id and, with
`--public-url`, a link to the monitoring page. With `--user-resume`, a pause
of a run a verified user started is that user's question and is not announced.

## The channels file

```yaml
channels:
  - kind: slack
    workflow: support
    mode: socket                  # or webhook. Default: socket with an app token
    token_env: SLACK_BOT_TOKEN    # xoxb-..., the default name
    app_token_env: SLACK_APP_TOKEN
    signing_secret_env: SLACK_SIGNING_SECRET
    groups: true                  # answer mentions in channels (default for Slack)
    allow: ["U01ABC"]
    approvers: ["U07LEAD"]
    notify: ["C0123OPS"]

  - kind: teams
    workflow: support
    app_id_env: TEAMS_APP_ID      # or app_id: "00000000-..."
    app_password_env: TEAMS_APP_PASSWORD
    tenant_id_env: TEAMS_TENANT_ID   # or tenant_id: "aaaaaaaa-..."
    approvers: ["7d0c...-object-id"]
```

Every other key (`name`, `input`, `rate_limit`, `budget`, `max_file_mb`,
`messages`) is as in [channels.md](channels.md#the-channels-file).
`examples/channels/slack-teams.yaml` is a complete file.

## Design notes

- **Same crate, same host.** Slack and Teams are features of `graphrt-channel`
  (`slack`, `socket-mode`, `teams`) next to Telegram and WhatsApp, behind the
  same `Channel` trait. Two methods were added with defaults the older
  channels keep: `send_rich` (cards drawn natively) and `direct_chat` (Teams
  opens a chat before it can write to someone).
- **Socket Mode first.** Most companies cannot open a port to the internet
  for a pilot. Socket Mode needs none, and it is what the server picks when an
  app token is there.
- **Teams tokens are verified in the crate**, with `ring` (already linked for
  TLS), against keys fetched from Microsoft's OpenID configuration and fetched
  again when a token names a key that rotated in.
- **Answers to Teams go only to verified addresses.** The service address of
  a conversation is remembered only from a delivery whose token named it.
- **Notifications are a `Notifier`.** The same trait Web Push implements, so
  a run's end is announced in one place for every way of reaching people.

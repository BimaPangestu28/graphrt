# Talking to a served agent on Telegram and WhatsApp

`graphrt serve` can put a workflow on a messaging channel. A customer writes on
Telegram or WhatsApp, the workflow runs, and the answer goes back on the same
channel. Every customer has a conversation of their own, kept by the server.

Nothing here needs a hosted service besides the channel itself, and all of it
is off until you ask for it.

| You want | Start `graphrt serve` with | You need |
| --- | --- | --- |
| A Telegram bot, on a laptop or a server | `--channel telegram=WORKFLOW` | A bot token |
| A Telegram bot on several replicas | the above, plus `TELEGRAM_WEBHOOK_SECRET` and `--public-url` | A public HTTPS address |
| A WhatsApp number | `--channel whatsapp=WORKFLOW` | A Meta app, four values, a public HTTPS address |
| Both, or more than the flag can say | `--channels FILE` | [A channels file](#the-channels-file) |

```sh
TELEGRAM_BOT_TOKEN=... graphrt serve flows/ --channel telegram=support --store ./runs
```

The cargo features are `channel-telegram` and `channel-whatsapp` (both bring
`channels`). They are in the default build and in the released binaries. A
build without them (`--no-default-features --features serve,llm,...`) has none
of this code, and a build with one of them has no code of the other.

Only the official APIs are used: the Telegram Bot API and the WhatsApp Cloud
API of the WhatsApp Business Platform.

## Telegram in three steps

1. In Telegram, write to [@BotFather](https://t.me/BotFather), send `/newbot`
   and follow it. It answers with a token such as `123456:ABC-DEF...`.
2. Put the token in the environment (or in a file you name with `--env-file`):

   ```sh
   export TELEGRAM_BOT_TOKEN=123456:ABC-DEF...
   ```

3. Start the server with the workflow that should answer:

   ```sh
   graphrt serve examples/channels/support.yaml --channel telegram=support --store ./runs
   ```

Open the bot in Telegram and write to it. That is all: the server asks Telegram
for new messages by itself (long polling), so it needs no public address and
works behind a home router.

`--store DIR` keeps conversations and runs across restarts. Without it they
live in memory.

### Telegram with a webhook

With long polling one process fetches the messages. To run several replicas,
or to let Telegram push instead, use a webhook. Choose a secret (any long text
of letters, digits, `_` and `-`) and tell the server its public address:

```sh
export TELEGRAM_WEBHOOK_SECRET=$(openssl rand -hex 32)
graphrt serve flows/ --channel telegram=support --store ./runs \
  --addr 0.0.0.0:8080 --public-url https://agent.example.com
```

A webhook secret in the environment chooses webhook mode. The server registers
`https://agent.example.com/v1/channels/telegram/webhook` with Telegram at
startup, and Telegram then sends the secret with every delivery. A delivery
without it is refused with `401`.

Without `--public-url` the server cannot know its address. It says so at
startup, and you register the address yourself with Telegram's `setWebhook`.

Going back to polling later needs nothing: a polling server removes the bot's
webhook at startup and says so.

## WhatsApp step by step

WhatsApp delivers by webhook only, so the server needs a public HTTPS address.

1. Create an app at [developers.facebook.com](https://developers.facebook.com/apps)
   with the WhatsApp product, and add your business phone number (Meta also
   gives a test number).
2. Collect four values:

   | Variable | Where it is |
   | --- | --- |
   | `WHATSAPP_ACCESS_TOKEN` | A permanent token of a system user with `whatsapp_business_messaging` |
   | `WHATSAPP_PHONE_NUMBER_ID` | WhatsApp > API Setup, "Phone number ID" (not the phone number) |
   | `WHATSAPP_APP_SECRET` | App settings > Basic, "App secret" |
   | `WHATSAPP_VERIFY_TOKEN` | Any text you choose now and type again in step 4 |

3. Start the server:

   ```sh
   graphrt serve flows/ --channel whatsapp=support --store ./runs \
     --addr 0.0.0.0:8080 --env-file .env
   ```

4. In the app, under WhatsApp > Configuration, set the callback URL to
   `https://<your address>/v1/channels/whatsapp/webhook` and the verify token
   to the text of step 2. Then subscribe the webhook field `messages`.

Meta checks the address at once (a `GET` the server answers when the verify
token matches). From then on every delivery is signed with the app secret, and
the server checks the signature of the raw body before it reads anything. An
unsigned or changed delivery is refused with `401`.

### The 24 hour window

WhatsApp lets a business write free text to a customer only within 24 hours of
the customer's last message. An answer normally leaves seconds after the
question, so this matters in one case: a run that waited for an approval and
continues the next day.

Such an answer is not sent as text. It stays in the conversation, and the
server says so in its log. To let the customer know, name an approved template
in the [channels file](#the-channels-file) (`reengage_template`). The server
sends it instead, and the customer's reply opens the window again. Meta bills
template messages.

### The unofficial WhatsApp route

The `whatsapp-web` plugin drives a personal account through a headless
browser. It has no place in `graphrt serve`: it needs a browser on the server,
it breaks WhatsApp's terms, and the account can be banned. It stays a local
option of the Desktop app, at the owner's own risk.

## What the workflow receives

A message starts a run like a request of the chat page would:

| Value | Content |
| --- | --- |
| `sys.query` | The customer's text, or the caption of a file |
| `sys.user_id` | `<channel>:<customer id>`, for example `telegram:123456789` or `whatsapp:6281234567890` |
| `sys.conversation_id` | `ch-<channel>-<chat id>` |
| `sys.history` | The earlier turns of this conversation, oldest first |
| `sys.files` | The files the customer sent, as file values (needs `--workspace`) |
| `sys.channel` | `telegram` or `whatsapp` |

A workflow that takes the message as a start input names it:
`--channel telegram=support=message`.

The conversation is in the server's conversation store, so `--channel` turns
`--conversations` on. `GET /v1/conversations` lists channel conversations with
their `channel` (name, chat, the customer's display name) for holders of the
API key. `--conversation-max-messages` and `--conversation-ttl-days` apply.

On Telegram, `/start` answers with a welcome line and `/new` starts the
conversation over.

### Files from the customer

With `--workspace DIR` a photo or a document is fetched into
`inbox/<channel>/` of the workspace and reaches the run as `sys.files`, which
`vision` inputs and `document-extractor` take. With `--workspace-per-user` it
goes into the customer's own directory. Files above `max_file_mb` (10 by
default) are declined.

Without a workspace the customer is told that files cannot be taken, and the
text of the message still runs.

Stickers, locations, contacts and polls carry nothing an agent can read. The
customer gets one line that says so.

## What the customer gets

- **The answer text**, converted from the Markdown a model writes to what the
  channel shows: Telegram HTML, or WhatsApp's `*bold*` and `_italic_`. Links
  are written out for WhatsApp. If Telegram refuses the formatting, the same
  text goes out plain.
- **Long answers in parts.** Both channels cap a message at 4096 characters.
  The answer is cut at line ends, and a code block that crosses a cut is closed
  in one part and opened again in the next.
- **[UI blocks](workflow-format.md#ui-blocks)** of the run's outputs as text:
  a table as lines, stat tiles as `label: value`, a card with its buttons as
  numbered choices. The customer answers with the number, or taps a button
  where the channel shows one.
- **Files the run produced** (the `files` output of `agent` and `tool` nodes,
  and file values in the run's outputs) as pictures or documents.
- **A typing sign** while the run works. On WhatsApp the customer's message is
  also marked as read.

## When a run waits for a person

A paused run never leaves the customer in silence.

| The run waits for | Who answers | How |
| --- | --- | --- |
| A `human-input` question, with `--user-resume` | The customer | Their next message |
| A `human-input` question, without `--user-resume` | The team | The approvals inbox, or an approver in the channel |
| A tool approval (`require_approval`) | The team, always | The same |

`--user-resume` says that the questions of the workflows customers talk to are
questions to the customer. It is off by default, because a `human-input` node
can also be an approval that someone else has to give. A tool approval is
never the customer's to give.

**The customer answers.** The question goes out as text. Several actions, or
the options of a field, become numbered choices (and buttons). One field to
fill in takes the customer's message as its value. A reply that is none of the
choices asks again.

**The team answers.** The customer reads "One moment please. Someone from our
team needs to confirm this." The request is in the approvals inbox
(`GET /v1/approvals`, the [monitoring page](monitoring.md)), and with
`approvers` in the channels file it also goes to those people in the channel:

```text
Approval needed (support, customer Sari)

The agent asks to run:
- refund {"order":"1042","amount":250000}

Reply with a number:
1. Approve
2. Reject
```

Whoever answers first, in the chat or in the inbox, releases the run, and its
answer goes to the customer.

A form with several fields to fill in has no chat form. It waits in the inbox.

With `--expire-approvals`, a question nobody answered in time is closed as its
node says (`on_expire`), and the customer is told.

## A person takes over

Three routes, with the API key, are what an inbox needs:

| Method | Path | Body | What |
| --- | --- | --- | --- |
| GET | `/v1/channels` | | The configured channels |
| POST | `/v1/channels/{name}/handoff` | `{"to": "<chat id>", "human": true}` | A person answers this customer and the workflow stays quiet. `false` hands back |
| POST | `/v1/channels/{name}/messages` | `{"to": "<chat id>", "text": "..."}` | A message of the team to the customer |

While a person has the conversation, the customer's messages are kept in it
and start no run. The team's messages are kept as answers, so the workflow has
the whole exchange in `sys.history` when it takes over again.

A channel only answers who wrote first, so a message to a customer without a
conversation is refused with `409`.

## Limits and safety

- **Checked before read.** A Telegram webhook delivery must carry the secret,
  a WhatsApp delivery the signature of its raw body. Both are compared in
  constant time.
- **Each message once.** Channels deliver again when they are not sure a
  message arrived. A message id that was seen starts nothing, and with
  `--store` the run of a message has an id of its own, so a redelivery after a
  restart finds the run that already exists.
- **A limit per customer and a budget per channel.** `--channel-rate-limit`
  (10 messages a minute for one customer) and `--channel-budget` (120 a minute
  for all customers of a channel) work like the guest limits of the chat page.
  Anyone can write to a bot, so the budget is what bounds the model spend. A
  customer over a limit gets one notice a minute, not one per message.
- **Allowed senders.** `allow` in the channels file restricts a channel to a
  list of customer ids. Other senders hear nothing.
- **Groups are ignored** unless the channel sets `groups: true` (Telegram).
- **One turn at a time.** While a run of a conversation works, the customer's
  further messages wait and go into the next turn together.
- **Secrets** come from the environment or `--env-file`, never from the
  channels file. They are not in logs, events, errors or API answers. A
  Telegram request URL holds the bot token, so no error names a URL.
- **Customer text is untrusted input.** It reaches the workflow as the user
  message and nowhere else. The customer's display name and file names are
  treated the same way: a file is stored under a name the server cleaned.
  Give an agent that talks to strangers only the tools a stranger may trigger,
  and put the rest behind `require_approval`.
- **Channel conversations belong to the API key.** A user token never reaches
  one, whatever its subject is, and the runs of a channel are the key's too.
- **A failed run** tells the customer that something went wrong. The reason
  stays in the server's log and the run record.

## The channels file

`--channels FILE` (YAML or JSON) configures what the flag cannot. Both can be
used together. Every key except `kind` and `workflow` is optional.

```yaml
channels:
  - kind: telegram
    name: shop                  # in URLs and conversation ids. Default: the kind
    workflow: support
    input: message              # the start input for the text. Default: sys.query
    mode: polling               # or webhook
    token_env: SHOP_BOT_TOKEN   # default: TELEGRAM_BOT_TOKEN
    webhook_secret_env: SHOP_WEBHOOK_SECRET
    allow: ["123456789"]        # only these customer ids may write
    groups: false               # answer in group chats too
    approvers: ["123456789"]    # who answers approvals in the channel
    rate_limit: 10              # messages a minute of one customer (0 = unlimited)
    budget: 120                 # messages a minute of all customers (0 = unlimited)
    max_file_mb: 10             # largest file taken or sent, 1 to 50
    messages:
      welcome: "Halo! Ada yang bisa kami bantu?"

  - kind: whatsapp
    workflow: support
    access_token_env: WHATSAPP_ACCESS_TOKEN
    phone_number_id_env: WHATSAPP_PHONE_NUMBER_ID   # or phone_number_id: "1065..."
    app_secret_env: WHATSAPP_APP_SECRET
    verify_token_env: WHATSAPP_VERIFY_TOKEN
    api_version: v23.0
    reengage_template: { name: follow_up, language: id }
```

Two channels of one kind need different names. A channel named `shop` takes
deliveries at `/v1/channels/shop/webhook`.

`messages` are the few sentences the server itself says. Set them in your
customers' language:

| Key | When | Default |
| --- | --- | --- |
| `welcome` | Telegram's `/start` | Hello! Send a message to start. |
| `busy` | The channel's budget is used up, or the server is at `--max-active-runs` | We are getting a lot of messages right now. Please try again in a few minutes. |
| `slow_down` | The customer is over `rate_limit` | You are sending messages very fast. Please wait a moment and try again. |
| `error` | The run failed | Sorry, something went wrong on our side. Please try again. |
| `unsupported` | A message with nothing to read | Sorry, I can only read text, photos and documents. |
| `no_files` | A file arrived and there is no workspace | Sorry, I cannot take files here. Please write what you need instead. |
| `file_too_large` | A file above `max_file_mb` | Sorry, that file is too large for me to read. |
| `waiting` | The run waits for the team | One moment please. Someone from our team needs to confirm this. |
| `still_waiting` | The customer writes again meanwhile | We are still waiting for our team. You will get the answer here. |
| `choose` | A reply that is none of the choices | Please reply with one of the numbers. |
| `choice_prompt` | The line above numbered choices | Reply with a number: |
| `new_conversation` | Telegram's `/new` | Started a new conversation. |
| `expired` | A question was not answered in time | Nobody answered in time, so this request was closed. Please write again to start over. |

## Running it

- **Restarts.** With `--store`, conversations, paused runs and the question a
  customer still has to answer survive a restart. A run that a shutdown
  interrupted is resumed by the next server, and its answer still goes out.
  Approvals that approvers were shown in the channel are kept in memory: after
  a restart they are answered in the inbox.
- **Several replicas.** Use webhooks: with long polling, two processes would
  take each other's messages. The order of one customer's turns is kept inside
  one process, so route a customer to the same replica where order matters.
- **An always on server.** Channels belong to a `graphrt serve` that keeps
  running. An agent that a gateway scales to zero stops polling while it
  sleeps, and the gateway does not forward `/v1/channels`.
- **No added cost for other runs.** A run that is not of a channel conversation
  does nothing more than before.
- **What is acknowledged.** A webhook is answered as soon as its delivery is
  verified and read, before any run starts. A message that was acknowledged and
  then lost to a crash is not asked for again.

## Design notes

Why it is built this way:

- **The channel code is a crate of its own, `graphrt-channel`,** with no
  dependency on the rest of Graphrt. `graphrt serve` and the Desktop app both
  build on it, each with its own conversations and runs. Telegram and WhatsApp
  are cargo features, and the HTTP side (`client`) is a third, so a host that
  only verifies and parses links no HTTP client.
- **Answers leave when a run ends, not when a request returns.** The driver of
  every run of a channel conversation tells the channels how it ended. So the
  answer reaches the customer whether the run was started by their message,
  resumed by an approval in the inbox, or resumed after a restart.
- **Conversations reuse the store of `--conversations`.** A channel
  conversation is an ordinary one with a fixed id and a `channel` field, so
  retention, limits and the API apply without a second store.
- **Dedupe reuses the run store.** The run id of a message is derived from the
  channel's message id, like the idempotency key of a webhook.
- **Text first, buttons as a convenience.** Every question can be answered by
  typing, because not every question fits the channels' buttons (WhatsApp
  shows three, of 20 characters). Buttons send the same number.
- **The customer never approves a tool.** A tool behind `require_approval` is
  guarded against the very person who is writing.

## For hosts: the `graphrt-channel` crate

```toml
graphrt-channel = { version = "0.6", features = ["telegram", "whatsapp", "client"] }
```

| Item | What |
| --- | --- |
| `telegram::parse_update(&json) -> ParsedUpdate` | One update (polling or webhook) as an `InboundEvent`, with its `update_id` |
| `telegram::verify_secret_token(header, &secret) -> bool` | The webhook check |
| `telegram::command(text) -> Option<(name, argument)>` | `/start`, `/new`... |
| `whatsapp::verify_subscription(mode, token, challenge, &verify_token)` | The answer to Meta's `GET` |
| `whatsapp::verify_signature(raw_body, header, &app_secret)` | The `X-Hub-Signature-256` check |
| `whatsapp::parse_webhook(&json) -> Vec<WhatsAppEvent>` | Messages and delivery reports, by phone number id |
| `whatsapp::service_window_open(last_inbound_ms, now_ms) -> bool` | The 24 hour window |
| `text::split`, `text::to_telegram_html`, `text::to_whatsapp` | Length limits and formatting, without any feature |
| `InboundEvent`, `InboundMessage`, `Attachment`, `DeliveryStatus` | What a channel delivered |
| `Channel` (feature `client`) | `send_text(chat, markdown, buttons)`, `send_file`, `typing`, `download(attachment, max_bytes)` |
| `telegram::TelegramBot` | `Channel`, plus `get_me`, `get_updates`, `set_webhook`, `delete_webhook`, `webhook_url` |
| `whatsapp::WhatsAppCloud` | `Channel`, plus `mark_read`, `send_template`, `send_buttons`, `upload_media`, `media` |
| `Secret`, `ChannelError` | A credential that never prints, and errors that never carry a URL |

The caller brings the `reqwest::Client`. A host maps `InboundMessage` to its
own conversation (`chat_id` to answer in, `customer.id` and `customer.name`,
`id` to drop redeliveries) and calls `Channel::send_text` with its reply.

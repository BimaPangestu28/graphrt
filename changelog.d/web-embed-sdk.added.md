- Embed SDK for the chat widget (`/chat/embed.js`, `graphrt/chat`): events
  `open`, `close`, `ready`, `message`, `answer`, `handoff` and `error` through
  `GraphrtChat.on/off` and as DOM events `graphrt-chat-<event>` on
  `<graphrt-chat>` and `<graphrt-chat-widget>`, the methods `send`,
  `setContext`, `reset` and `setTheme`, `GraphrtChat.init({...})` with the
  names of the `data-` attributes, and a queue stub
  (`window.GraphrtChat = window.GraphrtChat || { q: [] }`) for calls made
  before the script loaded. TypeScript types ship in `graphrt/chat`.
- Page context: `setContext` sends a JSON object with every message as
  `sys.context`. `graphrt serve` refuses one that is not an object, larger
  than 8 KiB or nested deeper than 5 levels (`400`, also for runs a gateway
  forwards), and never reads it as identity.

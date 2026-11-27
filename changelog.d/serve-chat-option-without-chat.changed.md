- `graphrt serve`: a chat option without a chat page (`--chat-access`,
  `--chat-title`, `--chat-guest-rate-limit`, `--chat-guest-budget` or
  `--chat-mermaid` without `--chat` or `$GRAPHRT_CHAT`) is still refused, now
  when the server starts instead of while the command line is read, because
  the chat page may come from the environment. The message names the option.

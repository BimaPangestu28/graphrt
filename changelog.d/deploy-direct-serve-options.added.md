- `graphrt deploy --direct`: a direct deployment can serve the chat page, the
  monitoring page and what goes with them. `--chat`, `--chat-access
  key|guests`, `--chat-title`, `--chat-guest-rate-limit`, `--chat-guest-budget`,
  `--cors-origin`, `--conversations`, `--monitor`, `--devices`, `--approvals`,
  `--expire-approvals` and `--push` are the `graphrt serve` flags of the same
  names, each off unless given. The `result` line of a deploy has `serve`
  (what the version serves) and `store`. See
  [docs/deploy-direct.md](docs/deploy-direct.md#chat-page-monitoring-and-the-store).
- `graphrt deploy --direct`: a deploy with `--monitor`, `--devices`, `--push`
  or `--conversations` keeps its devices, push subscriptions, push key,
  conversations and runs on one 1 GB Fly volume (billed by Fly, about 0.15 USD
  a month), so they outlive a Machine that stops when idle and a redeploy.
  Such a deployment keeps one Machine that is updated in place, and a version
  that does not get healthy is replaced by the one that served before. Deploys
  without these flags create no volume. `--drop-store` deletes the volume, and
  `graphrt deploy status` reports it as `store`.
- `graphrt serve`: the flags of the chat page, conversations and monitoring
  can be set as environment variables, for a container without a command line
  of its own: `GRAPHRT_CHAT`, `GRAPHRT_CHAT_ACCESS`, `GRAPHRT_CHAT_TITLE`,
  `GRAPHRT_CHAT_GUEST_RATE_LIMIT`, `GRAPHRT_CHAT_GUEST_BUDGET`,
  `GRAPHRT_CONVERSATIONS`, `GRAPHRT_MONITOR`, `GRAPHRT_DEVICES`,
  `GRAPHRT_APPROVALS`, `GRAPHRT_EXPIRE_APPROVALS` and `GRAPHRT_PUSH`. A flag
  wins over its variable. See
  [docs/serving.md](docs/serving.md#flags-as-environment-variables).

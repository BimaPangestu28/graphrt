- `graphrt deploy --direct --channel telegram|whatsapp`: a direct deployment
  answers customers on Telegram or WhatsApp (`graphrt serve --channel`). The
  channel's credentials are passed with `--secret` and stay Fly secrets: the
  Machine command names only the kind and the workflow. Channel conversations
  use the store volume, `--channel telegram` needs `--mode always-on`, and the
  deploy result lists `serve.channels`.

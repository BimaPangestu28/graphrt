- `graphrt serve --store s3://bucket/prefix`: runs, events, snapshots, scores,
  conversations, paired devices and push subscriptions in an S3 bucket (or an
  S3 compatible store) that any number of servers share, with conditional
  writes on ETags instead of a lock or a database. Cargo feature `store-s3`,
  in the default build. See `docs/serving.md`, "A store in S3".
- `graphrt deploy --target lambda --direct` takes `--conversations`,
  `--monitor`, `--devices`, `--push`, `--channel whatsapp|slack|teams` and
  `--drop-store`: the deployment gets an S3 bucket as its store, kept across
  redeploys. On Lambda `graphrt serve` holds the end of a response until the
  work it left running is stored (`GRAPHRT_HOLD_UNTIL_IDLE`), so approvals,
  channel answers and async runs finish before Lambda freezes the instance.
- `graphrt deploy capabilities --json`: what each direct target (Fly, Cloud
  Run, Lambda) supports in this build, flags, modes, store, channels and
  credentials, for apps. See `docs/deploy-capabilities.md`.

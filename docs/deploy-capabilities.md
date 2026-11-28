# What each direct deploy target supports (`graphrt deploy capabilities`)

Apps that drive `graphrt deploy --direct` (the desktop app) read what a target
supports from one JSON object instead of `--help`:

```sh
graphrt deploy capabilities --json
```

```json
{
  "type": "result",
  "action": "capabilities",
  "schema_version": 1,
  "graphrt": "0.6.3",
  "targets": [
    {
      "target": "lambda",
      "available": true,
      "docs": "docs/deploy-direct-aws.md",
      "schema": "docs/schemas/deploy-direct-aws.schema.json",
      "credentials": { "env": ["AWS_ACCESS_KEY_ID", "AWS_SECRET_ACCESS_KEY", "AWS_SESSION_TOKEN", "AWS_PROFILE", "AWS_REGION", "AWS_SHARED_CREDENTIALS_FILE"], "flags": ["--profile", "--region"] },
      "flags": ["--region", "--profile", "--app", "--image", "--secret", "--rotate-api-key", "--set role_arn=ARN", "--drop-store"],
      "modes": ["scale-to-zero"],
      "default_mode": "scale-to-zero",
      "max_request_secs": 900,
      "serve": ["chat", "cors-origin", "conversations", "monitor", "devices", "approvals", "expire-approvals", "push", "channel"],
      "store": { "kind": "s3", "features": ["conversations", "monitor", "devices", "push", "channel"], "drop_store": true, "requires_feature": "store-s3", "available": true },
      "channels": ["whatsapp", "slack", "teams"],
      "channels_always_on": [],
      "channels_refused": ["telegram", "slack-socket-mode"],
      "idle_cost": "none for the function, storage of the image and the store bucket"
    }
  ]
}
```

(`targets` holds `fly`, `cloud-run` and `lambda`, in that order.) Without
`--json` the command prints one line per target. It needs no credentials and
makes no network call. The schema is
[`schemas/deploy-capabilities.schema.json`](schemas/deploy-capabilities.schema.json).

| Field | Meaning |
| --- | --- |
| `schema_version` | changes only when a field changes meaning. Fields are only added, never renamed or removed |
| `available` | the target is in this build (cargo features `deploy-fly`, `deploy-cloud-run`, `deploy-aws`) |
| `schema` | the schema of the target's deploy and management JSON Lines |
| `credentials` | environment variables and flags the target reads credentials from |
| `flags` | flags only this target takes (besides the common `--deployment`, `--version`, `--json` and the `serve` flags) |
| `modes` | values of `--mode` |
| `max_request_secs` | longest request the platform serves, `null` for no limit |
| `serve` | `graphrt serve` features a deploy can turn on (the flags without `--`) |
| `store` | where the features in `store.features` keep their state: a Fly volume, a Cloud Run bucket or the deployment's S3 bucket on Lambda. `drop_store`: `--drop-store` deletes it |
| `channels` | values of `--channel` |
| `channels_always_on` | channels that need `--mode always-on` |
| `channels_refused` | channels the target cannot run at all |

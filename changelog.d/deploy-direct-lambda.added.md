- `graphrt deploy --target lambda --direct`: one agent online at a public AWS
  Lambda Function URL with nothing running while idle, no gateway and no
  Docker. The deploy copies the graphrt image into ECR with the Lambda Web
  Adapter and the agent, creates the role, log group, function, `live` alias
  and URL (response streaming for SSE), and health checks each version before
  it serves. `status`, `list-versions`, `rollback`, `pause`, `resume` and
  `remove` work as on Fly, with the same JSON Lines
  (`docs/schemas/deploy-direct-aws.schema.json`). Credentials come from the
  standard AWS places (`--profile`, `AWS_PROFILE`, the environment). Errors
  say how to fix missing permissions, regions and quotas. Cargo feature
  `deploy-aws`, in the default build. See `docs/deploy-direct-aws.md`.

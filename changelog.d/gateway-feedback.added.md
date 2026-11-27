- `graphrt gateway`: `POST /v1/deployments/{d}/runs/{id}/feedback` rates a
  run behind a gateway and teaches its agent, with the rules of
  `graphrt serve --verify-jwt`. An end user's token, on a run started with a
  token of the same user, makes the feedback that user's own words (low risk
  lessons may be approved by `auto_approve_user_feedback`, risky ones wait for
  the owner). Another user's token is refused with `403`, a missing or bad one
  with `401`, and with the deployment API key `user` is only a label. The
  gateway forwards it to an agent of the run's version
  (`POST /internal/v1/runs/{id}/feedback`). It is in the release gateway
  build. A build from source needs the `learning` feature next to `gateway`.

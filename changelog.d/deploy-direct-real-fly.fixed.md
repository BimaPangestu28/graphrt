- `graphrt deploy --direct` on real Fly.io: `resume`, `rollback` and a deploy
  that updates a stopped Machine (the Machine with the store after it went
  idle, or a standby version deployed again under its label) no longer wait
  two minutes and fail with `start_timeout`. Fly leaves a stopped Machine
  stopped when its config is updated, so graphrt now starts it. A rollback
  whose Machine does not start leaves that version a standby, off the proxy.
- `graphrt deploy --direct` on a Fly organization at its Machine limit (Fly's
  Free plan allows 3 Machines): a new version no longer fails with `fly_api`
  while older versions are kept. The oldest standby versions are removed to
  make room (the deploy's `starting` step names them). The serving version and
  the Machine with the store are never removed. When nothing is left to
  remove, the error says to upgrade the Fly organization or remove apps.
- `graphrt deploy --direct` on real Fly.io: for a few seconds after a config
  update Fly shows a Machine as `replacing` with its previous config. A
  `resume` right after a `pause` no longer reports `changed: false` and leaves
  the deployment paused, a `rollback` right after a deploy is no longer a
  silent no-op, `--drop-store` no longer refuses with "serves with the store
  volume", and `status` no longer shows two current versions. graphrt now
  waits until the update is applied, and counts a `replacing` Machine as part
  of the deployment.

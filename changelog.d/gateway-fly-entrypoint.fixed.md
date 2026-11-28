- `graphrt deploy --target fly` (gateway pools): the command's program becomes
  the Machine's `init.entrypoint`. It went to `init.cmd`, which on Fly replaces
  only the image's CMD, so `graphrt serve ...` ran as `graphrt graphrt serve
  ...` with the graphrt image. A command that starts with `serve` (the form the
  Fly guide showed) still keeps the image's entrypoint.
- The release workflow's Maven Central job waits for the Central Portal to
  validate the uploaded bundle and fails with the Portal's errors when it is
  refused.

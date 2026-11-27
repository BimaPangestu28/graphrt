- `graphrt deploy --target cloud-run` with a short version (`v1`): Cloud Run
  refuses traffic tags shorter than 3 characters, so such tags get the
  prefix `ver-` (`v1` → `ver-v1`). Found on real Cloud Run.
- `graphrt deploy --target ecs|lambda`: the command (`graphrt serve ...`)
  replaces the image's entrypoint, as on Kubernetes and Cloud Run. It used to
  become only the container's CMD, so the graphrt image ran
  `graphrt graphrt serve` and every task exited at once (found on real ECS).
- The gateway's run records carry the `answer` text the agent streamed, as
  records of `graphrt serve` do; it was always empty behind a gateway.

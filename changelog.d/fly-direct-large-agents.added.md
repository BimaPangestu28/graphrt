- Direct deploys to Fly carry agents of any size. An agent file larger than
  1 MiB (Fly refuses Machine requests of about 2 MiB), such as a `.grt` bundle
  with its knowledge base, is uploaded onto the deployment's store volume
  through the Machines API `exec` in 2 MiB pieces and served from there. A new
  hidden `graphrt internal store-file` does the writing inside the Machine:
  pieces carry offsets, so retries never write twice, and the file is only
  used once its SHA-256 matches. Each agent has its own directory, so the
  version serving during an upload is undone cleanly when the new one is not
  healthy, and a redeploy of the same file uploads nothing. The volume is
  sized (and grown) for the agent, and the Machine gets more memory for large
  knowledge stores. See docs/deploy-direct.md, "Large agents and their
  knowledge".

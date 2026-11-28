- `graphrt deploy --target cloud-run --direct`: one agent online on Google
  Cloud Run with one command, no gateway and no image build. The agent file
  goes into a Cloud Storage bucket the revision mounts, the published image is
  pulled through an Artifact Registry remote repository of `ghcr.io`, secrets
  and the API key live in Secret Manager, the service is public with graphrt's
  API key (no `allUsers` binding), and the store is a folder of the bucket
  (one instance). `status`, `list-versions`, `rollback` (traffic tags),
  `pause`, `resume` and `remove` work as on Fly, with the same JSON Lines
  (`docs/schemas/deploy-direct-cloud-run.schema.json`). APIs that are off are
  turned on, and errors name the fix: the API to enable, the billing link, the
  role to ask for, the organization policy in the way. See
  docs/deploy-cloud-run.md.
- `graphrt-gcp` accepts a person's Google sign in: `authorized_user`
  credentials (the application default credentials of `gcloud auth
  application-default login`, or any refresh token), an access token in
  `GOOGLE_OAUTH_ACCESS_TOKEN`, or a token source of the caller's own, for
  organizations that forbid service account keys.

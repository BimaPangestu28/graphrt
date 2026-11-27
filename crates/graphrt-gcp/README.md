# graphrt-gcp

Google Cloud for graphrt, without a Google SDK: hand-rolled GCP auth
(metadata server, service account JWT signed with `ring`, a person's Google
sign in), the Cloud Run provider and deploy driver of the gateway (Admin API
v2, Secret Manager references), and direct deploys of one agent to Cloud Run.

Part of [graphrt](https://github.com/Graphrt/graphrt) — **run your agent everywhere**: one small runtime for
agentic AI workflows on servers, containers, the edge and phones.

| Feature | Adds |
| --- | --- |
| `auth-gcp` | `auth::GcpAuth`: access and ID tokens, cached until `exp - 5 min`, refreshed once on 401 |
| `provider-cloud-run` | `cloud_run::CloudRunProvider` (the gateway's run-time provider) |
| `deploy-cloud-run` | `cloud_run::CloudRunDeployDriver`, `secret_manager::SecretManager` (deploy time) |
| `auth-user` | `auth::UserCredentials` (refresh token, application default credentials), `GcpAuth::from_local_env` with `GOOGLE_OAUTH_ACCESS_TOKEN` |
| `direct-cloud-run` | `direct::CloudRunDirect`: `graphrt deploy --target cloud-run --direct` (bucket, service account, secrets, remote repository, rollback, pause, removal) |
| `fake` | `direct::fake::FakeGoogle`, a stateful fake of those APIs for tests |

- Guides: [Gateway on Cloud Run](https://github.com/Graphrt/graphrt/blob/main/docs/gateway-cloud-run.md), [Direct deploys to Cloud Run](https://github.com/Graphrt/graphrt/blob/main/docs/deploy-cloud-run.md)
- Project README: <https://github.com/Graphrt/graphrt#readme>

`tests/fixtures/test-only-rsa2048.pem` is a throwaway RSA key generated for the
conformance tests; it is not a credential of any account.

Licensed under the Apache License, Version 2.0.

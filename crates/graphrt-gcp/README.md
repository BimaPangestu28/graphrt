# graphrt-gcp

Google Cloud for the graphrt gateway, without a Google SDK: hand-rolled GCP auth
(metadata server, service-account JWT signed with `ring`) and the Cloud Run
provider and deploy driver (Admin API v2, Secret Manager references).

Part of [graphrt](https://github.com/Graphrt/graphrt) — **run your agent everywhere**: one small runtime for
agentic AI workflows on servers, containers, the edge and phones.

| Feature | Adds |
| --- | --- |
| `auth-gcp` | `auth::GcpAuth`: access and ID tokens, cached until `exp - 5 min`, refreshed once on 401 |
| `provider-cloud-run` | `cloud_run::CloudRunProvider` (the gateway's run-time provider) |
| `deploy-cloud-run` | `cloud_run::CloudRunDeployDriver`, `secret_manager::SecretManager` (deploy time) |

- Guide: [Gateway on Cloud Run](https://github.com/Graphrt/graphrt/blob/main/docs/gateway-cloud-run.md)
- Project README: <https://github.com/Graphrt/graphrt#readme>

`tests/fixtures/test-only-rsa2048.pem` is a throwaway RSA key generated for the
conformance tests; it is not a credential of any account.

Licensed under the Apache License, Version 2.0.

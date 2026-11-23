# graphrt-provider-azure

Azure provider family of the graphrt gateway, without an Azure SDK.

| Feature | What |
| --- | --- |
| `auth-azure` | Entra ID tokens: managed identity (`IDENTITY_ENDPOINT` / IMDS), client secret, PS256 certificate assertion, workload identity federation; cached until `exp - 5 min` |
| `provider-aca` | Run-time Azure Container Apps `Provider` (`Addressing::Url`, platform scale-to-zero, label URL per deployment version) over ARM REST |
| `deploy-aca` | `DeployDriver` for `graphrt deploy`: createOrUpdate, revision labels, retirement of old versions, Key Vault secret references |

It implements the traits of `graphrt-provider-api` and passes its shared
conformance harness. Configuration, RBAC roles, ingress timeouts, measured
sizes and what still needs a real subscription: [docs/deploy-azure.md](https://github.com/BimaPangestu28/graphrt/blob/main/docs/deploy-azure.md).

```sh
cargo test -p graphrt-provider-azure --features deploy-aca
crates/graphrt-provider-azure/measure-size.sh   # C13 binary deltas
```

Test fixtures (`tests/fixtures/*.pem`) are a throwaway self-signed key pair
generated for these tests only.

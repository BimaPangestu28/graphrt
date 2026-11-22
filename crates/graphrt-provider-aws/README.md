# graphrt-provider-aws

AWS targets of the graphrt gateway, without the AWS SDK. Internal crate
(`publish = false`) until the gateway's provider wave (F2) has settled.

| Feature | Adds |
| --- | --- |
| `auth-aws` | SigV4 (headers and presigned query), credential chain env → container endpoint → STS web identity → IMDSv2, presigned EKS tokens, signed REST client with token bucket and jittered retry |
| `provider-ecs` | `ecs::EcsProvider`: ECS Fargate, one task per replica, gateway-scheduled |
| `provider-lambda` | `lambda::LambdaProvider`: short synchronous runs through `InvokeWithResponseStream` + Lambda Web Adapter |

Both providers implement `graphrt_provider_api::Provider`. Setup, IAM
policies, limits and the conformance status are in
[docs/gateway-aws.md](../../docs/gateway-aws.md).

```sh
cargo test -p graphrt-provider-aws --features provider-ecs,provider-lambda
```

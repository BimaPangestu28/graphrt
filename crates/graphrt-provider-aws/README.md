# graphrt-provider-aws

AWS targets of the graphrt gateway, without the AWS SDK. Its API follows
`graphrt-provider-api` and may change between minor releases.

| Feature | Adds |
| --- | --- |
| `auth-aws` | SigV4 (headers and presigned query), credential chain env → container endpoint → STS web identity → IMDSv2, presigned EKS tokens, signed REST client with token bucket and jittered retry |
| `provider-ecs` | `ecs::EcsProvider`: ECS Fargate, one task per replica, gateway-scheduled |
| `provider-lambda` | `lambda::LambdaProvider`: short synchronous runs through `InvokeWithResponseStream` + Lambda Web Adapter |
| `deploy-ecs` / `deploy-lambda` (`deploy`) | `DeployDriver`s for `graphrt deploy`: ECS task definitions, Lambda image functions + version alias + streaming Function URL |

Both providers implement `graphrt_provider_api::Provider`. Setup, IAM
policies, limits and the conformance status are in
[docs/gateway-aws.md](https://github.com/Graphrt/graphrt/blob/main/docs/gateway-aws.md).

```sh
cargo test -p graphrt-provider-aws --features deploy
```

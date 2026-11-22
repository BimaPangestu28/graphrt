# AWS targets for the graphrt gateway: ECS Fargate and Lambda

> Status: preview. `graphrt gateway` uses these providers with
> `--features gateway-ecs` / `gateway-lambda` (`kind: ecs` / `kind: lambda`
> under `providers:`), and `graphrt deploy --target ecs|lambda` deploys
> versions. See [docs/gateway.md](gateway.md#providers).

The crate `graphrt-provider-aws` implements the `graphrt-provider-api`
`Provider` trait for two AWS targets, without the AWS SDK (SigV4 and the
credential chain are hand-rolled on `sha2` + `reqwest`):

| Cargo feature | Target | Use it for |
| --- | --- | --- |
| `auth-aws` | SigV4, credential chain, presigned EKS tokens | shared by the two providers and by the EKS flavour of the Kubernetes provider |
| `provider-ecs` | **ECS Fargate**, one task per replica, scheduled by the gateway | async, webhook and cron runs; anything that may run longer than 14 minutes |
| `provider-lambda` | **AWS Lambda** through `InvokeWithResponseStream` + Lambda Web Adapter | short synchronous runs (≤ 14 minutes) that a client waits for |
| `deploy-ecs`, `deploy-lambda` (`deploy` = both) | the `DeployDriver`s of `graphrt deploy` | deploy time only (CI / operator credentials); the gateway's run-time build leaves them out |

**App Runner is not supported**: it is closed to new customers since
30 April 2026 (AWS points to ECS Express Mode instead).

## Credentials (`auth-aws`)

The gateway only needs run-time permissions (wake / stop / list / invoke); it
never sees model API keys. Credentials are resolved in this order and the first
configured source wins:

1. **Environment**: `AWS_ACCESS_KEY_ID`, `AWS_SECRET_ACCESS_KEY`, optional `AWS_SESSION_TOKEN`.
2. **Container endpoint**: the ECS task role (`AWS_CONTAINER_CREDENTIALS_RELATIVE_URI`)
   or EKS Pod Identity (`AWS_CONTAINER_CREDENTIALS_FULL_URI` +
   `AWS_CONTAINER_AUTHORIZATION_TOKEN[_FILE]`). A full URI over plain HTTP must be
   loopback or one of the documented link-local addresses (`169.254.170.2`,
   `169.254.170.23`, `fd00:ec2::23`).
3. **Web identity** (EKS IRSA, any OIDC federation): `AWS_WEB_IDENTITY_TOKEN_FILE` +
   `AWS_ROLE_ARN` (+ `AWS_ROLE_SESSION_NAME`), exchanged with STS
   `AssumeRoleWithWebIdentity` on the regional endpoint
   (`GRAPHRT_AWS_STS_ENDPOINT` overrides it, e.g. for a VPC endpoint).
4. **IMDSv2** on EC2 (session token first; `AWS_EC2_METADATA_DISABLED=true` turns it off,
   `AWS_EC2_METADATA_SERVICE_ENDPOINT` overrides the address).

Temporary credentials are cached until 5 minutes before they expire, refreshed
by one caller at a time, dropped and refetched once when AWS answers
401/403 with an expired-token code, and never logged (`Debug` output redacts
them). When AWS rejects a request for clock skew, the client learns the offset
from the response `Date` header and retries once, so a host clock that is a few
minutes off keeps working.

`auth::eks_token` produces the `k8s-aws-v1.` bearer token for the EKS API server
(a presigned STS `GetCallerIdentity`, as `aws eks get-token` does).

All provider endpoints (ECS, Lambda, STS, IMDS) come from the gateway
operator's configuration and environment, never from a deployment spec.

## ECS Fargate (`provider-ecs`)

The gateway is the scheduler: each replica is one Fargate task.

- **wake** lists the tasks of the deployment version (`ListTasks` filtered by
  `startedBy = gr-<hash(deployment, version)>`), starts the missing ones with
  `RunTask` (≤ 10 per call, a client token per call so a retried request never
  doubles the tasks), then polls `DescribeTasks` until every task is `RUNNING`
  and has a private IP. The endpoint is `http://<task private IP>:<container port>`,
  always taken from the ECS API. Concurrent wakes of one version share a single
  in-flight wake.
- **release** is `StopTask` with a reason (`scale-in` or `stop`; Fargate has no
  suspend). Stopping a task that is already gone is a success.
- **list** returns every task of the version, including orphans left by a
  gateway that was killed, for reconciliation.
- The task definition started is `DeploymentRef::target` when `graphrt deploy`
  set it (an ARN or `family:revision`), otherwise `family:version` from the
  operator template. Old versions keep their own revision, so paused runs of
  version N−1 resume on N−1.
- Capacity failures (`RunTask` `failures[].reason` "Capacity is unavailable…",
  `RESOURCE:*`) are reported as `Capacity`, so the gateway can fall back.

Operator configuration per deployment (`EcsTaskTemplate`): cluster, task
definition family, subnets, security groups, public IP on/off, container port,
optional capacity provider (e.g. `FARGATE_SPOT`) and platform version.

**Wake takes tens of seconds** (image pull and ENI attach, typically 20–60 s;
initial p95 target 45 s). ECS Fargate is the right target for async, webhook and
cron runs: the gateway prewarms before a cron fire, and chat clients get a
`: waking` SSE comment meanwhile. For interactive chat prefer Kubernetes,
Cloud Run or Lambda.

**Grace period**: Fargate's `stopTimeout` is at most **120 s**. Set it to 120 in
the task definition and keep the agent's `--drain-timeout` at most
`stopTimeout − 2 s` (`ecs::validate_stop_timeout` enforces both). Runs still
active at the deadline are preempted and re-dispatched by the gateway.

### IAM policy for the gateway (ECS)

```json
{
  "Version": "2012-10-17",
  "Statement": [
    {
      "Effect": "Allow",
      "Action": ["ecs:RunTask"],
      "Resource": "arn:aws:ecs:REGION:ACCOUNT:task-definition/graphrt-*:*",
      "Condition": { "ArnEquals": { "ecs:cluster": "arn:aws:ecs:REGION:ACCOUNT:cluster/CLUSTER" } }
    },
    {
      "Effect": "Allow",
      "Action": ["ecs:StopTask", "ecs:DescribeTasks"],
      "Resource": "arn:aws:ecs:REGION:ACCOUNT:task/CLUSTER/*"
    },
    {
      "Effect": "Allow",
      "Action": ["ecs:ListTasks"],
      "Resource": "*",
      "Condition": { "ArnEquals": { "ecs:cluster": "arn:aws:ecs:REGION:ACCOUNT:cluster/CLUSTER" } }
    },
    {
      "Effect": "Allow",
      "Action": ["ecs:TagResource"],
      "Resource": "arn:aws:ecs:REGION:ACCOUNT:task/CLUSTER/*",
      "Condition": { "StringEquals": { "ecs:CreateAction": "RunTask" } }
    },
    {
      "Effect": "Allow",
      "Action": ["iam:PassRole"],
      "Resource": [
        "arn:aws:iam::ACCOUNT:role/graphrt-agent-task-role",
        "arn:aws:iam::ACCOUNT:role/graphrt-agent-execution-role"
      ],
      "Condition": { "StringEquals": { "iam:PassedToService": "ecs-tasks.amazonaws.com" } }
    }
  ]
}
```

The agent's security group must accept the container port from the gateway's
security group only. Model keys live in AWS Secrets Manager and are referenced
from the task definition (`secrets`), read by the execution role, never by the
gateway.

## AWS Lambda (`provider-lambda`)

Lambda runs **synchronous runs of at most 14 minutes** (`max_run_secs ≤ 840`,
under Lambda's 15-minute cap). The gateway calls
`POST /2021-11-15/functions/{function}/response-streaming-invocations`
(`InvokeWithResponseStream`, SigV4, no Function URL) and holds the stream.
Inside the function, [Lambda Web Adapter](https://github.com/awslabs/aws-lambda-web-adapter)
turns the invoke (a Function URL 2.0 event) into an HTTP request to
`graphrt serve --trust-gateway`.

Function setup:

- container image with Lambda Web Adapter and `graphrt serve`;
- `AWS_LWA_INVOKE_MODE=response_stream` (required for SSE);
- function timeout between `max_run_secs + 30 s` and 900 s;
- the deployment version is a published version or alias (the `Qualifier`).

`lambda::validate_deployment` rejects at deploy time what Lambda cannot run:
async, webhook and cron triggers (the environment is frozen after the response,
so background work would stall), `max_run_secs > 840`, a function timeout out of
range, and a non-streaming adapter mode.

At run time:

- **Streaming vs buffering (C6)**: the reply is decoded from
  `application/vnd.amazon.eventstream` frames and Lambda Web Adapter's
  prelude (JSON head, eight NUL bytes, body). A buffered reply (the adapter in
  default mode, or an agent that does not flush) is detected and reported as
  `StreamMode::Buffered`, with a warning naming the fix.
- **Timeouts (C7)**: Lambda gives practically no grace period, so runs are
  never preempted. A run that hits the function timeout, or the gateway-side
  `max_run_secs`, ends with an explicit `InvokeError::PlatformTimeout`, which
  the gateway records as `failed{error_type: platform_timeout}`. Events received
  before the cut are delivered.
- A function error (`InvokeComplete.ErrorCode`) ends the stream with
  `InvokeError::Function`. Invokes are not retried after a 5xx (the function may
  have run); throttling, expired credentials and clock skew are retried.
- `wake`, `release` and `list` are no-ops / observability only (`Url`
  addressing, the platform scales). The endpoint address is
  `lambda://<function>:<qualifier>`: the gateway sends its signed protocol
  requests through `Provider::invoke` (implemented by `LambdaProvider`), not
  plain HTTP. `Capabilities::can_preempt()` is false (`max_grace_secs = 0`). A
  run cut by the time cap comes back as a `ProviderError` of kind
  `PlatformTimeout`, to be recorded as `failed{platform_timeout}` rather than
  re-dispatched; a truncated stream is `Transient`.

### IAM policy for the gateway (Lambda)

```json
{
  "Version": "2012-10-17",
  "Statement": [
    {
      "Effect": "Allow",
      "Action": ["lambda:InvokeFunction"],
      "Resource": "arn:aws:lambda:REGION:ACCOUNT:function:graphrt-*:*"
    }
  ]
}
```

(`InvokeWithResponseStream` is authorised by `lambda:InvokeFunction`.)

### Trust policy for IRSA / OIDC (gateway on EKS or federated CI)

```json
{
  "Effect": "Allow",
  "Principal": { "Federated": "arn:aws:iam::ACCOUNT:oidc-provider/OIDC_ISSUER" },
  "Action": "sts:AssumeRoleWithWebIdentity",
  "Condition": { "StringEquals": { "OIDC_ISSUER:sub": "system:serviceaccount:graphrt:gateway" } }
}
```

## Deploying (`deploy-ecs`, `deploy-lambda`)

The `DeployDriver`s turn a `DeploymentSpec` into an immutable version the
providers above can wake. They run with **deploy-time** credentials (CI via
OIDC, an operator); the gateway's run-time role never needs these
permissions. Every spec is validated before the first AWS call, and secrets
are only ever references.

### ECS Fargate: `EcsDeployDriver`

One `RegisterTaskDefinition` per version; the returned `DeploymentRef::target`
is the task definition ARN that `EcsProvider` starts. No ECS service is
created: the gateway is the scheduler.

- Fargate, `awsvpc`, Linux, one essential container `agent` with the image,
  command, port mapping, plain `environment`, and `secrets` as
  `valueFrom` references (Secrets Manager ARN, optionally `:json-key::`, or an
  SSM parameter ARN; anything else is rejected, plaintext never reaches AWS).
  The task execution role resolves them when the task starts.
- `stopTimeout` = `grace_secs` (default and maximum **120 s**); the agent's
  `--drain-timeout` must stay at `stopTimeout − 2 s`.
- CPU / memory: the smallest valid Fargate size for `resources` (default
  0.5 vCPU / 1 GiB; memory rounded up to what the size accepts).
- Tags `graphrt:deployment`, `graphrt:version`, `graphrt:workflow`.

| Provider setting | Required | Meaning |
| --- | --- | --- |
| `execution_role_arn` | yes | pulls the image, reads the secrets, writes logs |
| `task_role_arn` | no | role the agent runs as |
| `family` | no | task definition family (default `graphrt-<name>`) |
| `container_port` | no | default 8080 |
| `cpu_architecture` | no | `X86_64` (default) or `ARM64` |
| `log_group` | no | CloudWatch Logs group (`awslogs`, prefix `graphrt`) |

Re-applying a version registers a new revision of the same family (revisions
are cheap and immutable); the ref of the latest apply is the one to wake.

### Lambda: `LambdaDeployDriver`

1. Validates: synchronous triggers only, `max_run_secs ≤ 840`, function timeout
   (default `max_run_secs + 30`, ≤ 900), image from Amazon ECR, version usable
   as an alias name (`[A-Za-z0-9_-]`, not only digits), no reserved `AWS_*`
   variables, environment ≤ 4 KB.
2. Creates the container-image function (`CreateFunction`, `PackageType: Image`),
   or updates its configuration and then its code, waiting out
   `LastUpdateStatus: InProgress` between steps. A concurrent create falls back
   to the update path.
3. Sets the Lambda Web Adapter environment: `AWS_LWA_INVOKE_MODE=response_stream`,
   `AWS_LWA_PORT=<container_port>`, `AWS_LWA_READINESS_CHECK_PATH=/healthz`
   (overridable).
4. Publishes a version and points an alias named after the deployment version
   at it. An alias that already points at different code is refused
   (versions are immutable); re-applying the same spec is a no-op.
5. Adds a Function URL on the alias with `AuthType: AWS_IAM` and
   `InvokeMode: RESPONSE_STREAM` (`function_url: false` skips it). The gateway
   itself invokes through `InvokeWithResponseStream`; the URL is for direct,
   gateway-less access by IAM principals.

Returned ref: `{name, version: alias, target: function ARN}`.

**Secrets on Lambda.** Lambda environment variables cannot reference Secrets
Manager or SSM, so a spec with secrets is refused (`Unsupported`) by default.
With `secrets_mode: resolve-to-env` the driver reads each referenced secret at
deploy time with the deploy credentials and stores it as an environment
variable, encrypted at rest with `kms_key_arn` (or the Lambda default key).
Values are never logged, but they are visible to anyone with
`lambda:GetFunctionConfiguration` on the function; prefer ECS Fargate when that
is not acceptable.

Note: a refused apply (e.g. a reused version name) may already have updated the
function's `$LATEST`; published versions and aliases, which are all the gateway
invokes, are untouched.

| Provider setting | Required | Meaning |
| --- | --- | --- |
| `role_arn` | yes | execution role of the function |
| `function_name` | no | default `graphrt-<name>` |
| `triggers` | no | default `["sync"]`; `async`, `webhook`, `cron` are rejected |
| `max_run_secs` | no | default and maximum 840 |
| `function_timeout_secs` | no | default `max_run_secs + 30` |
| `container_port` | no | default 8080 |
| `architecture` | no | `x86_64` (default) or `arm64` |
| `function_url` | no | default `true` |
| `secrets_mode` | no | `reject` (default) or `resolve-to-env` |
| `kms_key_arn` | no | key encrypting the environment |

### IAM policy for `graphrt deploy` (deploy time)

```json
{
  "Version": "2012-10-17",
  "Statement": [
    { "Effect": "Allow", "Action": ["ecs:RegisterTaskDefinition", "ecs:TagResource"], "Resource": "*" },
    {
      "Effect": "Allow",
      "Action": [
        "lambda:GetFunction", "lambda:GetFunctionConfiguration", "lambda:CreateFunction",
        "lambda:UpdateFunctionConfiguration", "lambda:UpdateFunctionCode", "lambda:PublishVersion",
        "lambda:GetAlias", "lambda:CreateAlias", "lambda:CreateFunctionUrlConfig",
        "lambda:UpdateFunctionUrlConfig", "lambda:TagResource"
      ],
      "Resource": "arn:aws:lambda:REGION:ACCOUNT:function:graphrt-*"
    },
    {
      "Effect": "Allow",
      "Action": ["iam:PassRole"],
      "Resource": [
        "arn:aws:iam::ACCOUNT:role/graphrt-agent-task-role",
        "arn:aws:iam::ACCOUNT:role/graphrt-agent-execution-role",
        "arn:aws:iam::ACCOUNT:role/graphrt-*-lambda"
      ],
      "Condition": { "StringEquals": { "iam:PassedToService": ["ecs-tasks.amazonaws.com", "lambda.amazonaws.com"] } }
    },
    {
      "Effect": "Allow",
      "Action": ["ecr:BatchGetImage", "ecr:GetDownloadUrlForLayer", "ecr:SetRepositoryPolicy", "ecr:GetRepositoryPolicy"],
      "Resource": "arn:aws:ecr:REGION:ACCOUNT:repository/graphrt*"
    }
  ]
}
```

Only with Lambda `secrets_mode: resolve-to-env`, add
`secretsmanager:GetSecretValue` / `ssm:GetParameter` (+ `kms:Decrypt`) on the
referenced secrets, and `kms:Encrypt` / `kms:CreateGrant` on `kms_key_arn`.
The ECS execution role (not the deployer) needs `secretsmanager:GetSecretValue`
/ `ssm:GetParameters` on the referenced secrets.

## Conformance (plan §7)

Every case runs per PR against fake AWS endpoints (ECS JSON 1.1, Lambda event
stream, STS, IMDSv2, container credentials) that verify each SigV4 signature
and the ±5 min clock window: `cargo test -p graphrt-provider-aws --features provider-ecs,provider-lambda`.

| Case | ECS Fargate | Lambda |
| --- | --- | --- |
| C1 20 concurrent wakes | pass (exactly the requested tasks) | pass (no-op) |
| C2 wake latency n=50 | pass on the fake; **real p95 needs Fargate** | pass (no-op); real cold start needs AWS |
| C3 grace / SIGTERM | `StopTask` + `stopTimeout ≤ 120` checked; migration needs the gateway | n/a (no grace, no preempt) |
| C4 scale-in | pass (only the chosen task stops) | pass (no-op) |
| C5 external kill | pass (one `list` sees it; replaced on next wake) | n/a (`Url`) |
| C6 long stream, no buffering | n/a (gateway talks to the task IP) | pass on the fake (per-event overhead, buffering detection); **real 14-min stream needs AWS** |
| C7 request cap | n/a (no cap) | pass (`platform_timeout` from Lambda and from the gateway cap) |
| C8 auth | pass (refresh before exp, 403 → refresh once, skew ±5 min, no secrets in errors/`Debug`) | pass |
| C9 API errors | pass (throttle/5xx backoff, `RateLimited`, `Capacity`, no doubled tasks) | pass (throttle retried, 5xx not) |
| C10 secrets | pass (no credential in `Debug`/errors) | pass |
| C11 old-version pool | pass (version N−1 starts its own revision); day-6/day-8 needs the gateway | qualifier per version |
| C12 orphans | pass (tasks of a killed gateway listed and stopped) | n/a |
| C13 size | see the table below | see the table below |

**Needs real AWS** (nightly, credentials via OIDC federation): Fargate wake p95
(target 45 s) and capacity errors; `stopTimeout` behaviour under SIGTERM;
Lambda cold start, 14-minute streams through Lambda Web Adapter with the 15 s
heartbeat, the real `Sandbox.Timedout` shape, `InvokeWithResponseStream`
throttling; IRSA, Pod Identity and IMDSv2 on real hosts; the IAM policies above
(least privilege); the EKS token against a real cluster; the deploy drivers
against real ECS / Lambda (task definition accepted by Fargate, image function
create/update timings, Lambda Web Adapter readiness on `/healthz`, Function URL
streaming, KMS-encrypted environment with `resolve-to-env`).

The deploy drivers are tested end to end on the fakes
(`cargo test -p graphrt-provider-aws --features deploy`): ECS registers a
revision with secrets as `valueFrom` and `stopTimeout`, and `EcsProvider`
starts exactly that ARN; Lambda creates, waits out `InProgress`, updates,
publishes, aliases, adds the streaming IAM Function URL, and `LambdaProvider`
invokes `function:alias`; re-apply is a no-op, a reused version name with new
code is refused, a create race falls back to update, secrets are refused
unless `resolve-to-env`, and invalid specs make no AWS call.

## Binary size (C13)

Measured on macOS arm64, `--profile min-size`, with
`cargo build -p graphrt-provider-aws --example size_probe --profile min-size [--features …]`.
The probe's baseline already links reqwest + rustls + tokio + serde_json, which
the gateway has anyway, and every feature's probe exercises its whole API
(credential chain and signed REST client for `auth-aws`, the `dyn Provider`
path including `invoke` for the providers, `apply` for the drivers), so the
delta is the cost of the feature alone. Numbers are the sum of Mach-O section
sizes (`size -m`); file sizes move in 16 KiB page steps and hide small changes.

| Build | Sections | Δ | §9 budget |
| --- | --- | --- | --- |
| baseline (HTTP stack only) | 1,658,536 B | – | – |
| `auth-aws` | 1,774,913 B | **+113.6 KiB** vs baseline | 120 KiB |
| `provider-ecs` | 1,838,262 B | **+61.9 KiB** vs `auth-aws` | 100 KiB (`gw-ecs`) |
| `provider-lambda` | 1,848,570 B | **+71.9 KiB** vs `auth-aws` | 80 KiB (`gw-lambda`) |
| `provider-ecs,provider-lambda` | 1,881,867 B | +218.1 KiB vs baseline | 300 KiB (sum) |
| `deploy-ecs` | 1,857,686 B | +19.0 KiB vs `provider-ecs` | none (deploy CLI only) |
| `deploy-lambda` | 1,891,581 B | +42.0 KiB vs `provider-lambda` | none (deploy CLI only) |
| `deploy` (both) | 1,935,553 B | +52.4 KiB vs both providers | none (deploy CLI only) |

The deploy drivers are only linked with `deploy-*`; the gateway's run-time
build (`provider-*`) does not contain them. The Linux musl numbers of the §9
gate are measured by the gateway's size job once the gateway binary exists.
Nothing here is on the request hot path: signing costs one HMAC chain per
control-plane call, and the Lambda stream decoder copies each chunk once.

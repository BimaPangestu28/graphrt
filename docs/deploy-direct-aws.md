# Direct deploys to AWS Lambda (`graphrt deploy --target lambda --direct`)

> Tested on real AWS: see [Tested on real AWS](#tested-on-real-aws).

A direct deploy to AWS puts one agent online at a Lambda Function URL
(`https://<id>.lambda-url.<region>.on.aws`) with no graphrt gateway and
nothing running while nobody calls it. Each request starts (or reuses) a
Lambda instance that runs `graphrt serve` behind the
[Lambda Web Adapter](https://github.com/awslabs/aws-lambda-web-adapter).
Blocking runs, streamed runs (SSE), the chat page, conversations kept by the
server, the monitoring page with paired phones and approvals, push
notifications and messaging channels work as on [Fly](deploy-direct.md) and
[Cloud Run](deploy-cloud-run.md): their state is kept in an S3 bucket of the
deployment ([The store](#the-store-an-s3-bucket)). Idle costs nothing but a few
cents of storage a year.

No Docker is needed. Lambda only runs images from ECR in its own account, so
the deploy copies the graphrt image into an ECR repository of the deployment
and adds one layer with the adapter and the agent file. Only new layers are
uploaded, so a redeploy sends a few MB.

```sh
export OPENAI_API_KEY=sk-...
graphrt deploy --target lambda --direct --deployment support-bot --version v1 \
  --profile my-aws --region ap-southeast-1 --secret OPENAI_API_KEY --chat support.yaml
# [..] preparing: checking the AWS credentials and the deployment
# [ok] preparing: image 123456789012.dkr.ecr.ap-southeast-1.amazonaws.com/graphrt-support-bot@sha256:… (arm64)
# ...
# deployed v1 to https://abc….lambda-url.ap-southeast-1.on.aws (POST https://abc….lambda-url.ap-southeast-1.on.aws/v1/workflows/support/runs)
# chat page: https://abc….lambda-url.ap-southeast-1.on.aws/chat/
# API key (shown once, store it now): grk_…

curl -s https://abc….lambda-url.ap-southeast-1.on.aws/v1/workflows/support/runs \
  -H "Authorization: Bearer grk_…" -H 'content-type: application/json' \
  -d '{"sys":{"query":"Where is my order?"}}'
```

It is in the default build (cargo feature `deploy-aws`), and `graphrt
version` lists `deploy-aws` when it is available.

## When to use it, and when not

| | Lambda (`--target lambda --direct`) | Fly (`--target fly --direct`) |
| --- | --- | --- |
| Cost while idle | none (image storage, cents a year) | a stopped Machine's rootfs, a volume if any |
| First request after idle | about 0.1 to 0.3 s start, plus the model | about 1 to 2 s Machine start |
| One request at most | 15 minutes | no limit |
| Store (monitoring, devices, push, conversations, channels) | yes, an S3 bucket | yes, a volume |
| Always on (`--mode always-on`, Telegram polling, Slack Socket Mode) | no | yes |

Use Fly, Cloud Run or the [gateway](gateway-aws.md) when the agent runs longer
than 15 minutes or must keep running (Telegram). Why not other AWS services:

- **ECS Fargate** needs a load balancer for a public HTTPS address (about
  US$18 a month, always on) and does not scale to zero by itself. ECS Express
  Mode makes setup easier but keeps the load balancer and at least one task.
- **App Runner** is closed to new customers since 30 April 2026.
- **Lightsail containers** are billed per month whether used or not (from
  US$7) and do not scale to zero.

## Commands

```text
graphrt deploy --target lambda --direct --deployment <name> --version <label>
               [--profile <aws profile>] [--region <code>] [--image <ref>]
               [--secret NAME ...] [--rotate-api-key] [--app <function name>]
               [--set role_arn=<ARN>]
               [--chat [--chat-access key|guests] [--chat-title T]
                       [--chat-guest-rate-limit N] [--chat-guest-budget N]]
               [--cors-origin ORIGIN ...] [--approvals] [--expire-approvals]
               [--conversations] [--monitor] [--devices] [--push CONTACT]
               [--channel whatsapp|slack|teams ...] [--drop-store]
               [--json] <agent.grt|workflow.yaml|.json>

graphrt deploy status        --target lambda --direct --deployment <name> [--profile P] [--region R] [--json]
graphrt deploy list-versions --target lambda --direct --deployment <name> [--profile P] [--region R] [--json]
graphrt deploy rollback <label> --target lambda --direct --deployment <name> [--profile P] [--region R] [--json]
graphrt deploy pause         --target lambda --direct --deployment <name> [--profile P] [--region R] [--json]
graphrt deploy resume        --target lambda --direct --deployment <name> [--profile P] [--region R] [--json]
graphrt deploy remove        --target lambda --direct --deployment <name> [--profile P] [--region R] [--json]
```

| Flag | Default | Meaning |
| --- | --- | --- |
| `--deployment` | required | Names the function `graphrt-<name>` (lowercase letters, digits, dashes) |
| `--version` | required | Label of this version: 1 to 64 letters, digits, `.`, `_` or `-`, starting with a letter or digit. It is also the image tag. A label is deployed once |
| `--profile` | `AWS_PROFILE`, else the environment keys, else `default` | AWS profile, see [Credentials](#credentials-and-region) |
| `--region` | `AWS_REGION`, else the profile's region | AWS region code such as `ap-southeast-1` |
| `--image` | `ghcr.io/graphrt/graphrt:<cli version>` | graphrt image to run: any public registry (GHCR, Docker Hub, `public.ecr.aws`) or ECR of the same account. A multi platform image runs on arm64 (cheaper), else amd64 |
| `--secret NAME` | none | Stored as an environment variable of the function, value read from `$NAME`. Kept by later deploys |
| `--rotate-api-key` | off | Replace the API key (the new one is printed once) |
| `--app` | `graphrt-<deployment>` | Function name, for an existing naming scheme |
| `--set role_arn=ARN` | a role graphrt creates | Execution role to use instead (it needs to write logs, and to use the store bucket when a feature needs the store) |
| `--chat`, `--cors-origin`, `--approvals`, `--expire-approvals`, `--conversations`, `--monitor`, `--devices`, `--push`, `--channel` | off | As on [Fly](deploy-direct.md#chat-page-monitoring-and-the-store). The ones that keep state use [the store](#the-store-an-s3-bucket) |
| `--drop-store` | off | Delete the store bucket and everything in it once this version, which must not use the store, serves |

Refused on Lambda, before any AWS call: `--mode always-on`, `--org`,
`--channel telegram` (Telegram is asked for messages all the time, which needs
a server that keeps running) and Slack's Socket Mode (`SLACK_APP_TOKEN`: use
the Events API with `SLACK_SIGNING_SECRET`). `graphrt deploy capabilities
--json` lists this for apps ([deploy-capabilities.md](deploy-capabilities.md)).

## Credentials and region

The deploy signs its calls itself (no AWS CLI or SDK needed) and looks for
credentials where the AWS CLI does, in this order:

1. the profile of `--profile`, else of `AWS_PROFILE`, in `~/.aws/credentials`
   and `~/.aws/config` (`AWS_SHARED_CREDENTIALS_FILE` and `AWS_CONFIG_FILE`
   move them),
2. `AWS_ACCESS_KEY_ID`, `AWS_SECRET_ACCESS_KEY` and `AWS_SESSION_TOKEN`,
3. the `default` profile,
4. a container or instance role (ECS, EKS, EC2).

A profile is used when it holds keys (`aws_access_key_id`,
`aws_secret_access_key`, optionally `aws_session_token`). A profile that signs
in with SSO, assumes a role or runs a `credential_process` is refused with the
fix: export its keys first with
`eval "$(aws configure export-credentials --profile <name> --format env)"`
and deploy without `--profile`.

The region is `--region`, else `AWS_REGION` / `AWS_DEFAULT_REGION`, else the
`region` of the profile. Management commands need the same region as the
deploy.

**For apps** (the desktop app): pass the keys as `AWS_ACCESS_KEY_ID`,
`AWS_SECRET_ACCESS_KEY` (and `AWS_SESSION_TOKEN` for temporary keys) in the
environment of the `graphrt` process, plus `--region`, or point
`AWS_SHARED_CREDENTIALS_FILE` at a file with one profile and pass
`--profile`. Keys never appear in the output.

## What a deploy creates

| Resource | Name | Billed |
| --- | --- | --- |
| Lambda function, alias `live`, public Function URL (`AuthType NONE`, response streaming) | `graphrt-<deployment>` | per request and duration |
| ECR repository, one image tag per version | `graphrt-<deployment>` | storage, about US$0.10 per GB a month |
| IAM role with `AWSLambdaBasicExecutionRole` (unless `--set role_arn`) | `graphrt-<deployment>-lambda` | free |
| CloudWatch log group, 14 days retention | `/aws/lambda/graphrt-<deployment>` | per GB of logs |
| S3 bucket of the store, only when a feature needs it, and an inline policy `graphrt-store` on the role for it | `graphrt-<deployment>-<account>-<region>` | storage and requests |

Everything is tagged `graphrt:deployment`. The Function URL is public, the
API key (`GRAPHRT_API_KEY`, generated by the first deploy) protects the agent
as on Fly: a run without it answers 401.

A deploy:

1. checks the credentials (`sts:GetCallerIdentity`) and creates the role and
   the log group on the first deploy,
2. copies the image into ECR with the adapter (`/opt/extensions/lambda-adapter`)
   and the agent (`/workflows/<workflow>.<ext>`),
3. keeps the API key and the secrets of the previous version and adds the new
   `--secret` values,
4. updates the function and publishes a Lambda version described
   `graphrt v=<label> w=<workflow> t=<ms>`,
5. invokes that version with `GET /readyz` until it answers 200 (180 s at
   most, or three crashes), then points `live` at it and creates the URL on the
   first deploy. A version that does not start is deleted, and the previous
   one keeps serving,
6. keeps the five newest labels and deletes older Lambda versions and their
   image tags.

A deploy to a paused deployment puts it back online.

## Output (`--json`)

The same JSON Lines as the Fly and Cloud Run deployers ([deploy-direct.md](deploy-direct.md#output---json)):
`step` lines, then one `result` or `error` line, with one `api_key` line
before the error of a deploy that already stored a new key. Exit code 2 for
`invalid_input`, 1 for other errors. The schema is
[`schemas/deploy-direct-aws.schema.json`](schemas/deploy-direct-aws.schema.json).
Differences from Fly:

- the steps are the same four (`preparing`, `secrets`, `starting`, `checking`), copying the image is part of `preparing`,
- every result has `region`, `app` is the function name and `url` the
  Function URL,
- `status` has `machines: []`, `mode` is `scale-to-zero`,
- `store` (deploy and status) is `{bucket, region, in_use}` or `null`,
- `list-versions` entries add `lambda_version`.

| `code` | Meaning and what to do |
| --- | --- |
| `invalid_input` | a flag or the agent file is wrong, the message names the flag |
| `missing_credentials` | no keys found, or the profile has none. The message says which command fixes it |
| `unauthorized` | the access key is wrong, deleted or expired, or the region is not enabled for the account |
| `permission_denied` | the user may not do something: attach the [IAM policy](#iam-policy) |
| `region_unavailable` | not a region code |
| `account_not_ready` | a new account not activated yet (up to 24 hours, needs a payment method) |
| `quota_exceeded` | an AWS quota is used up (Service Quotas, AWS Lambda or Amazon ECR) |
| `not_found` | no such deployment in that region, or no such version |
| `rate_limited` | AWS throttles: retry in a minute |
| `image_unavailable` | `--image` or the adapter image cannot be read (private, wrong tag, zstd layers) |
| `health_check_failed` | the new version did not start: see its log group. The previous version keeps serving |
| `aws_unavailable` | AWS did not answer properly, or cannot be reached |
| `aws_api` | AWS refused a call for another reason (the message has AWS's code) |
| `internal` | a local failure |

## State across deploys

Kept by every redeploy: the URL, the API key (until `--rotate-api-key`), the
secrets (a deploy adds or replaces values, it never drops one), the versions
for rollback, and everything in the store.

A rollback serves an earlier label again. When the API key and the secrets
are the same as that version's, `live` just moves back (under a second).
Otherwise the version is published again with today's key and secrets and
health checked first, so a rollback never brings back a rotated key. The URL
follows within about five seconds.

## The store: an S3 bucket

A deploy with `--conversations`, `--monitor`, `--devices`, `--push` or
`--channel` creates one S3 bucket for the deployment (private, encrypted by S3)
and runs `graphrt serve --store s3://<bucket> --store-ttl-days 30`. Every
Lambda instance reads and writes the same bucket:

| Object | What |
| --- | --- |
| `runs/<id>/record.json`, `events/`, `snapshot.json` | run records and events (`GET /v1/runs`, the monitoring page), the snapshot of a paused run |
| `conversations/<id>.json` | conversations kept by the server and of channel customers |
| `devices.json` | paired phones (token hashes only) |
| `push/vapid.pkcs8`, `push/subscriptions.json` | the push key and the browsers that subscribed |
| `coord/...` | markers that make something happen once across instances |

There is no lock and no database: every change is a conditional write on the
object's ETag (`If-Match`, `If-None-Match: *`), retried on a newer copy when
another instance wrote first. Two instances adding turns to one conversation
keep both, a paused run is resumed once, and a channel message delivered twice
runs once. A new instance does not mark the runs of other instances as
interrupted: only a `running` run whose record did not change for an hour
counts as stopped (`GRAPHRT_STORE_STALE_RUN_SECS`).

Lambda freezes an instance as soon as its response ends. So a deployment on
Lambda runs `graphrt serve` with `GRAPHRT_HOLD_UNTIL_IDLE=1`: a response is
sent as soon as it is ready, but its end waits until no run executes and no
background work (a resumed approval, a channel answer, the last writes of a
run) is left, so nothing is frozen halfway. A client that resolves an
approval gets its `202` at once and the connection closes when the resumed run
is stored. Async runs (`"async": true`) work the same way: the request that
started one ends when the run did.

A later deploy without those features keeps the bucket (`"in_use": false`),
so turning them on again finds the conversations and paired phones.
`--drop-store` deletes the bucket with everything in it, `remove` too. The same
store works outside Lambda: `graphrt serve --store s3://bucket/prefix` with
the usual AWS credentials, on any number of servers ([serving.md](serving.md#a-store-in-s3)).

## Limits

- One request runs at most 15 minutes (Lambda's maximum function timeout,
  which the deploy sets). Streams are cut there.
- The secrets and the API key share Lambda's 4 KB of environment variables.
- A new AWS account can run 10 instances at the same time. Ask for more in
  Service Quotas (AWS Lambda, Concurrent executions) for busy agents.
- Pause sets the reserved concurrency to 0: every request answers 429 until
  `resume`.
- `public.ecr.aws`, where the adapter comes from, allows about one anonymous
  request a second: the deploy waits and retries.
- A new IAM policy can take a few minutes before Lambda honours it (about
  five minutes in the test below).

## Costs

AWS prices of `ap-southeast-1` (Singapore), November 2026, before the free
tier (1 million requests and 400,000 GB seconds a month, which covers about
400,000 short chats).

| | Cost |
| --- | --- |
| Idle | US$0 for Lambda. ECR stores the image (about 20 MB, shared by the versions): under US$0.01 a month. The store bucket: US$0.025 per GB a month, a few MB for thousands of conversations |
| Store requests (with a store) | about 15 to 30 requests per chat turn (record, events, conversation): about US$0.0001 per turn, US$0.023 per GB a month stored |
| One chat (512 MB, about 2 s waiting on the model) | about US$0.000017 (US$1.70 per 100,000), plus the model provider |
| Logs | US$0.70 per GB ingested, a few hundred bytes per request |
| Traffic out | first 100 GB a month free, then about US$0.12 per GB |

Lambda bills the time a request waits on the model too, so a long agent run
costs more: 60 s at 512 MB is about US$0.0005.

## IAM policy

The smallest policy a deploying user needs (tested, see below). It allows
only resources named `graphrt-*`:

```json
{
  "Version": "2012-10-17",
  "Statement": [
    {
      "Sid": "GraphrtLambda",
      "Effect": "Allow",
      "Action": [
        "lambda:GetFunction", "lambda:GetFunctionConfiguration", "lambda:CreateFunction",
        "lambda:UpdateFunctionConfiguration", "lambda:UpdateFunctionCode", "lambda:PublishVersion",
        "lambda:ListVersionsByFunction", "lambda:DeleteFunction", "lambda:TagResource",
        "lambda:GetAlias", "lambda:CreateAlias", "lambda:UpdateAlias",
        "lambda:GetFunctionUrlConfig", "lambda:CreateFunctionUrlConfig", "lambda:AddPermission",
        "lambda:InvokeFunction", "lambda:GetFunctionConcurrency", "lambda:PutFunctionConcurrency",
        "lambda:DeleteFunctionConcurrency"
      ],
      "Resource": "arn:aws:lambda:*:*:function:graphrt-*"
    },
    {
      "Sid": "GraphrtImages",
      "Effect": "Allow",
      "Action": [
        "ecr:CreateRepository", "ecr:DeleteRepository", "ecr:TagResource",
        "ecr:BatchCheckLayerAvailability", "ecr:InitiateLayerUpload", "ecr:UploadLayerPart",
        "ecr:CompleteLayerUpload", "ecr:PutImage", "ecr:BatchGetImage", "ecr:GetDownloadUrlForLayer",
        "ecr:BatchDeleteImage", "ecr:GetRepositoryPolicy", "ecr:SetRepositoryPolicy"
      ],
      "Resource": "arn:aws:ecr:*:*:repository/graphrt-*"
    },
    { "Sid": "GraphrtRegistryLogin", "Effect": "Allow", "Action": "ecr:GetAuthorizationToken", "Resource": "*" },
    {
      "Sid": "GraphrtRole",
      "Effect": "Allow",
      "Action": [
        "iam:GetRole", "iam:CreateRole", "iam:TagRole", "iam:ListRoleTags", "iam:DeleteRole",
        "iam:AttachRolePolicy", "iam:DetachRolePolicy", "iam:PutRolePolicy", "iam:DeleteRolePolicy"
      ],
      "Resource": "arn:aws:iam::*:role/graphrt-*"
    },
    {
      "Sid": "GraphrtPassRole",
      "Effect": "Allow",
      "Action": "iam:PassRole",
      "Resource": "arn:aws:iam::*:role/graphrt-*",
      "Condition": { "StringEquals": { "iam:PassedToService": "lambda.amazonaws.com" } }
    },
    {
      "Sid": "GraphrtStore",
      "Effect": "Allow",
      "Action": ["s3:CreateBucket", "s3:PutBucketTagging", "s3:ListBucket", "s3:DeleteObject", "s3:DeleteBucket"],
      "Resource": ["arn:aws:s3:::graphrt-*", "arn:aws:s3:::graphrt-*/*"]
    },
    {
      "Sid": "GraphrtLogs",
      "Effect": "Allow",
      "Action": ["logs:CreateLogGroup", "logs:PutRetentionPolicy", "logs:DeleteLogGroup", "logs:TagResource"],
      "Resource": "arn:aws:logs:*:*:log-group:/aws/lambda/graphrt-*"
    }
  ]
}
```

With `--set role_arn=ARN` the `GraphrtRole` statement can go (keep `iam:PassRole`
on that role). `ecr:GetRepositoryPolicy` and `ecr:SetRepositoryPolicy` let
Lambda add its pull permission to the repository on the first deploy.

## Removing

`graphrt deploy remove` deletes the function (with its versions, alias, URL
and pause), the ECR repository with its images, the role when graphrt made it
(tag `graphrt:deployment`), the log group and the store bucket with everything
in it. Running it again cleans up what
is left and answers `removed: false`.

## Tested on real AWS

Account in `ap-southeast-1`, 2026-11-28, `examples/chatbot.yaml` with an
OpenAI key as `--secret`, an amd64 image built from this branch (the published
`ghcr.io/graphrt/graphrt:0.6.3` agent image was not on GHCR at the time).

| Step | Result |
| --- | --- |
| first deploy (role, log group, ECR, function, URL) | 24 s |
| redeploy without `--secret`, other flags | 19 s, same API key, `OPENAI_API_KEY` kept |
| no key / blocking run / SSE | 401 / 200 in 1.1 s / 233 `node_chunk` events spread over 0.9 to 4.4 s (streamed, not buffered) |
| chat page | 200 with `--chat`, 404 on a version without |
| cold start | Init 70 to 270 ms (1.5 to 1.9 s for the first pull of a new image), 28 to 31 MB used |
| three requests at once, all on new instances | 1.1 to 1.6 s each, model included |
| `--rotate-api-key` | old key 401, new key 200 |
| rollback, same key | `live` moved in 0.7 s, served within about 5 s |
| rollback after a rotation | published again with the new key (17 s), old key still 401 |
| pause / pause again / resume | 429 `ReservedFunctionConcurrentInvocationLimitExceeded` / `changed: false` / 200 |
| image whose `graphrt` exits | `health_check_failed` after 3 crashes (26 s), previous version kept serving, failed version deleted |
| user without permissions | `permission_denied` naming the missing action |
| user with only the [IAM policy](#iam-policy) | deploy, redeploy, status, list-versions, pause, resume, rollback, remove |
| seventh label | the oldest labels and their image tags deleted, five kept |
| remove, twice | function, repository, role, log group gone, then `removed: false` |

Store on Lambda, same account and region, 2026-11-28, an image built from
the branch of the store:

| Step | Result |
| --- | --- |
| deploy with `--chat --conversations --monitor --push` | 29 s, bucket created, the role's inline policy, `store: {in_use: true}` |
| a conversation of two turns, redeploy, a third turn | the third answer used the first turn ("You said you like mango"), 6 messages kept |
| a phone paired on v1 (`POST /v1/devices`) | its token still worked after the redeploy, the pairing link used the Function URL |
| the push key | the same VAPID key before and after the redeploy |
| four turns at once into one conversation (four instances) | all 4 turns kept, 14 messages, none lost |
| `GET /v1/runs` | every run of every instance listed |
| an approval (`examples/approval.yaml`) resolved with the phone's token | `202` in 0.28 s, connection closed after 0.55 s with the run `succeeded` |
| a WhatsApp delivery (signed, dummy Meta credentials) and the same delivery again | first `200` after 2.9 s with the run done and the customer's conversation stored, the second ran nothing |
| cold start of a new instance with a store | Init 170 to 270 ms (2 s for the first pull of a new image, as without a store) |
| a version without store features, then `--drop-store` | bucket kept (`in_use: false`), then the bucket and the policy deleted |
| a user with only the [IAM policy](#iam-policy) | deploy with the store and remove |
| remove | function, image repository, role with its policy, log group and bucket gone |

Not tested: an arm64 image (the test image was amd64 only), a profile with a
session token, and a bundle larger than 100 MB.

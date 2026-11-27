# One agent on Cloud Run or ECS Fargate

How to put one served agent (`graphrt serve`) online on Google Cloud Run or
Amazon ECS Fargate without the [gateway](gateway.md): an HTTPS (Cloud Run) or
load balancer (ECS) address, the API key and the model key in the cloud's
secret store, conversations and runs kept on a volume, and redeploys without
downtime.

Verified on real accounts on 2026-11-27 with graphrt 0.6.2 and
`examples/chatbot.yaml` (OpenAI): API calls, SSE streaming, the chat page,
conversations surviving three redeploys, and a `/readyz` probe every second
during each redeploy (0 failed probes out of about 350 on each cloud).

For Fly.io, `graphrt deploy --target fly --direct` does all of this in one
command ([deploy-direct.md](deploy-direct.md)). Use the gateway with its
[Cloud Run](gateway-cloud-run.md) or [ECS](gateway-aws.md) provider instead
when you need durable runs across many deployments, webhooks or cron.

## Before you start

- An image. Build it from the repository `Dockerfile` and push it to the
  cloud's registry (Artifact Registry, ECR), or use the published
  `ghcr.io/graphrt/graphrt:<version>`. The image runs as the distroless
  `nonroot` user (uid and gid `65532`).
- The agent file: a workflow `.yaml`/`.json` or a `.grt` bundle in
  `/workflows`. On Cloud Run it can come from a Cloud Storage bucket (change the
  agent without building an image); on ECS bake it into an image
  (`FROM <graphrt image>` + `COPY agent.yaml /workflows/`).
- Turn on the chat page, conversations and the rest with
  [environment variables](serving.md#flags-as-environment-variables)
  (`GRAPHRT_CHAT`, `GRAPHRT_CONVERSATIONS`, `GRAPHRT_STORE`...), so the
  platform's settings page is enough.

Keys stored from a file (`--data-file`, `file://`) usually end with a newline.
graphrt trims surrounding whitespace from `GRAPHRT_API_KEY` and the model
provider keys, so that is fine.

## Google Cloud Run

```sh
P=my-project R=asia-southeast1
gcloud services enable run.googleapis.com artifactregistry.googleapis.com secretmanager.googleapis.com --project $P

# Image
gcloud artifacts repositories create graphrt --repository-format=docker --location=$R --project $P
IMG=$R-docker.pkg.dev/$P/graphrt/graphrt:0.6.2
docker build -t $IMG . && docker push $IMG

# Secrets and the identity the service runs as
gcloud secrets create openai-key --data-file=openai.key --project $P
gcloud secrets create graphrt-api-key --data-file=api.key --project $P
gcloud iam service-accounts create graphrt-agent --project $P
SA=graphrt-agent@$P.iam.gserviceaccount.com
for s in openai-key graphrt-api-key; do
  gcloud secrets add-iam-policy-binding $s --member=serviceAccount:$SA --role=roles/secretmanager.secretAccessor --project $P
done

# Bucket for the agent file (agent/) and the store (store/)
B=$P-graphrt
gcloud storage buckets create gs://$B --location=$R --uniform-bucket-level-access --project $P
gcloud storage buckets add-iam-policy-binding gs://$B --member=serviceAccount:$SA --role=roles/storage.objectUser
gcloud storage cp support.yaml gs://$B/agent/support.yaml

gcloud run deploy support --project $P --region $R --image $IMG --service-account $SA \
  --execution-environment gen2 --cpu 1 --memory 512Mi --min-instances 0 --max-instances 1 \
  --allow-unauthenticated \
  --args serve,/workflows,--addr,0.0.0.0:8080,--drain-timeout,8s \
  --set-env-vars GRAPHRT_STORE=/data,GRAPHRT_CONVERSATIONS=1,GRAPHRT_CHAT=support,GRAPHRT_CHAT_ACCESS=key \
  --set-secrets OPENAI_API_KEY=openai-key:latest,GRAPHRT_API_KEY=graphrt-api-key:latest \
  --add-volume name=agent,type=cloud-storage,bucket=$B,readonly=true,mount-options=only-dir=agent \
  --add-volume-mount volume=agent,mount-path=/workflows \
  --add-volume name=store,type=cloud-storage,bucket=$B,mount-options=only-dir=store \
  --add-volume-mount volume=store,mount-path=/data
```

The service URL (`https://support-<number>.<region>.run.app`) serves
`POST /v1/workflows/support/runs` and the chat page at `/chat/support`.
`--allow-unauthenticated` lets Cloud Run pass every request on;
graphrt still asks for the API key (or a user token). Without it, callers also
need a Google identity token with `roles/run.invoker`.

Redeploy:

- new image: `gcloud run services update support --image <new image>`
- new agent file, same image: copy it to `gs://$B/agent/` and start a new
  revision (`gcloud run services update support --update-labels agent-rev=v2`);
  the agent is compiled when an instance starts, so running instances keep
  the old file until they are replaced.

Cloud Run starts the new revision, moves traffic when it is ready and sends
SIGTERM to the old one, which drains (10 s grace, so `--drain-timeout 8s`).

Notes from the real run:

- **Do not probe `/healthz` from outside.** Cloud Run's front end reserves
  `/healthz` and answers it with its own `404` page; the request never reaches
  the container. Use `/readyz` for uptime checks. Startup and liveness probes
  configured on the service go straight to the container, so `/healthz` works
  there.
- **Keep `--max-instances 1` with a Cloud Storage store.** Cloud Storage FUSE
  has no file locks across instances, and the [run store](serving.md) is meant
  for one machine. One instance (plus the short overlap while a revision
  replaces another) is safe; for more instances use the gateway, whose store
  is SQLite on one machine.
- The store works as the `nonroot` user on a Cloud Storage volume (no
  ownership setup needed). It is also where device tokens
  (`/data/devices.json`) live with `GRAPHRT_DEVICES` or `GRAPHRT_MONITOR`.
- Without a volume the store is in memory: conversations and paused runs end
  when the instance scales to zero.

## Amazon ECS Fargate

AWS App Runner no longer accepts new customers ("Account … can't create new
App Runner services"), so ECS Fargate behind an Application Load Balancer is
the simplest managed option. EFS keeps the store.

What to create (all in one VPC; the default VPC works):

| Resource | Settings |
| --- | --- |
| ECR repository | the image with the agent baked in |
| Secrets Manager | the model key and the API key, one secret each |
| IAM role (task execution) | trust `ecs-tasks.amazonaws.com`; `AmazonECSTaskExecutionRolePolicy` plus `secretsmanager:GetSecretValue` on the two secrets |
| Security groups | load balancer: TCP 80 (or 443) from the internet; task: TCP 8080 from the load balancer, TCP 2049 from itself (EFS) |
| EFS file system | a mount target per subnet (task security group) and an **access point with POSIX user and owner `65532:65532`**, root directory e.g. `/graphrt`, mode `0700` |
| Application Load Balancer | target group of type `ip`, port 8080, health check `GET /readyz`, deregistration delay 30 s; a listener forwarding to it |
| CloudWatch log group | for `awslogs` |
| ECS cluster and service | Fargate, the target group, `minimumHealthyPercent=100`, `maximumPercent=200` |

The container in the task definition:

```json
{
  "name": "graphrt",
  "image": "<account>.dkr.ecr.<region>.amazonaws.com/support:v1",
  "command": ["serve", "/workflows", "--addr", "0.0.0.0:8080", "--drain-timeout", "55s"],
  "portMappings": [{ "containerPort": 8080 }],
  "stopTimeout": 60,
  "environment": [
    { "name": "GRAPHRT_STORE", "value": "/data" },
    { "name": "GRAPHRT_CONVERSATIONS", "value": "1" },
    { "name": "GRAPHRT_CHAT", "value": "support" },
    { "name": "GRAPHRT_CHAT_ACCESS", "value": "key" }
  ],
  "secrets": [
    { "name": "OPENAI_API_KEY", "valueFrom": "<secret ARN>" },
    { "name": "GRAPHRT_API_KEY", "valueFrom": "<secret ARN>" }
  ],
  "mountPoints": [{ "sourceVolume": "store", "containerPath": "/data" }]
}
```

with the task-level volume
`{"name": "store", "efsVolumeConfiguration": {"fileSystemId": "fs-…", "transitEncryption": "ENABLED", "authorizationConfig": {"accessPointId": "fsap-…", "iam": "DISABLED"}}}`,
`networkMode: awsvpc`, `cpu: 256`, `memory: 512`.

Redeploy: register a new task definition revision with the new image and
`aws ecs update-service --task-definition <family>:<revision>` (or
`--force-new-deployment` for the same one). ECS starts the new task, waits for
the target group's `/readyz`, then stops the old task with SIGTERM; graphrt
drains within `stopTimeout` and exits `0`.

Notes from the real run:

- A new EFS file system's root directory belongs to `root`, so without the
  access point's POSIX user the `nonroot` image cannot write the store.
- During a rolling deploy two tasks share the EFS store for a moment; EFS
  supports the store's file locks.
- The load balancer address is stable across redeploys. Put HTTPS on it with
  an ACM certificate and a 443 listener before real users connect; the
  plain-HTTP listener above is only for a test.
- Fargate bills while the task runs (no scale to zero), roughly
  US$0.015 per hour for 0.25 vCPU / 0.5 GB in `ap-southeast-1`, plus about
  US$0.025 per hour for the load balancer.

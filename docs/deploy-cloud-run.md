# Direct deploys to Google Cloud Run (`graphrt deploy --target cloud-run --direct`)

> Tested on real Google Cloud: see [Tested on real Cloud Run](#tested-on-real-cloud-run).

A direct deploy puts one agent online on Cloud Run with one command and no
graphrt gateway: Cloud Run serves `graphrt serve` at the service's own HTTPS
address, scales it to zero when idle and bills only while a request runs. No
image is built or pushed. The agent file (a `.grt` bundle or a workflow
`.yaml`/`.json`) goes into a Cloud Storage bucket that the revision mounts,
and the published graphrt image is pulled through an Artifact Registry remote
repository. It is the Google Cloud option of the desktop app's **Put it
online**, next to [Fly.io](deploy-direct.md), with the same flags and the same
JSON Lines.

```sh
gcloud auth application-default login          # or a key, or a token: see Credentials
export OPENAI_API_KEY=sk-...
graphrt deploy --target cloud-run --direct --project my-project --region asia-southeast2 \
  --deployment support-bot --version v1 --secret OPENAI_API_KEY --chat support.grt
# [..] preparing: Preparing Google Cloud project my-project (asia-southeast2)
# ...
# deployed v1 to https://support-bot-abcdefghij-et.a.run.app (POST https://support-bot-abcdefghij-et.a.run.app/v1/workflows/support/runs)
# chat page: https://support-bot-abcdefghij-et.a.run.app/chat/
# API key (shown once, store it now): grk_…
```

Use the [gateway](gateway.md) with its [Cloud Run provider](gateway-cloud-run.md)
instead for durable runs across many deployments, webhooks or cron, and
[deploy-cloud-run-ecs.md](deploy-cloud-run-ecs.md) to set the same service up
by hand with `gcloud`.

The direct deploy is in the default build (cargo feature `deploy-cloud-run`)
and in both release builds. `graphrt version` lists `deploy-cloud-run` among
the features.

## Before you start

What the person deploying needs in Google Cloud, once:

1. **A project with billing.** Create one at
   <https://console.cloud.google.com/projectcreate> and link a billing account
   (<https://console.cloud.google.com/billing/linkedaccount?project=PROJECT>).
   Cloud Run, Cloud Storage and Secret Manager have free tiers that a small
   agent usually stays within.
2. **Permission to deploy.** The project's Owner can. Otherwise ask for the
   roles in [Permissions](#permissions).
3. **Nothing else.** The first deploy turns on the APIs it needs (Cloud Run,
   Cloud Storage, Secret Manager, IAM, Artifact Registry) when they are off,
   and creates everything else itself.

## What a deploy creates

All in the given project and region, labelled `managed-by=graphrt`,
`graphrt-direct=1`, `graphrt-service=<service>` and every `--label`:

| Resource | Name | Holds |
| --- | --- | --- |
| Cloud Run service | `<service>` (the deployment name, or `--app`) | one revision per version, a traffic tag per kept version |
| Cloud Storage bucket | `<project>-<service>` | `agents/<sha>/` the agent of each version, `store/` the store (`graphrt serve --store`) |
| Service account | `graphrt-<service>@<project>.iam.gserviceaccount.com` | what the service runs as: it may read its secrets and use its bucket, nothing else |
| Secret Manager secrets | `<service>-graphrt-api-key`, `<service>-<name>` per `--secret` | the API key and the `--secret` values |
| Artifact Registry remote repository | `graphrt-ghcr`, one per region, shared | a pull through cache of `ghcr.io`, which Cloud Run cannot pull from |

The service is public: invoker IAM is turned off on it (`invokerIamDisabled`),
so no `allUsers` binding is needed, which organizations with domain restricted
sharing forbid. graphrt itself asks every caller for the deployment's API key
(or a guest session of the chat page with `--chat-access guests`).

Each revision runs at most one instance. With a store that is required: Cloud
Storage FUSE has no file locks across instances. A revision mounts its own
agent folder read only at `/workflows`, so a rollback serves the agent the
version was deployed with, and the store read and write at `/data`.

## Commands

```text
graphrt deploy --target cloud-run --direct --project <id> [--region <region>]
               --deployment <name> --version <label>
               [--mode scale-to-zero|always-on] [--secret NAME ...] [--label KEY=VALUE ...]
               [--app <service>] [--image <ref>] [--rotate-api-key] [--json]
               [--chat [--chat-access key|guests] [--chat-title <text>]
                       [--chat-guest-rate-limit <n>] [--chat-guest-budget <n>]]
               [--cors-origin <origin> ...] [--conversations]
               [--monitor] [--devices] [--approvals] [--expire-approvals] [--push <contact>]
               [--channel telegram|whatsapp|slack|teams ...] [--drop-store]
               <agent.grt|agent.yaml|agent.json>
graphrt deploy status         --target cloud-run --direct --project <id> [--region <region>] --deployment <name> [--app <service>] [--json]
graphrt deploy list-versions  (same flags)
graphrt deploy rollback <ver> (same flags)
graphrt deploy pause          (same flags)
graphrt deploy resume         (same flags)
graphrt deploy remove         (same flags)
```

| Flag | Default | Meaning |
| --- | --- | --- |
| `--project` | `$GOOGLE_CLOUD_PROJECT` | Project id (not the display name: `gcloud projects list` shows both). Never taken from gcloud's configuration. |
| `--region` | `us-central1` | Cloud Run region, e.g. `asia-southeast2` (Jakarta), `asia-southeast1` (Singapore), `europe-west1`. Management commands need the region the deployment lives in. |
| `--deployment` | required | Deployment name. The service is named after it: lowercased, everything but `a-z0-9` turned into `-`, at most 49 characters (`Support Bot` -> `support-bot`). |
| `--version` | required | Version label (1 to 128 printable characters). Redeploying the label of the serving version moves its tag to the new revision. |
| `--mode` | `scale-to-zero` | `scale-to-zero`: no instance while idle, a cold start of a few seconds on the next request, CPU billed only during requests. `always-on`: one instance kept warm with CPU always on (needed by `--channel telegram` and Slack Socket Mode), billed all the time. |
| `--secret NAME` | none | Repeatable. Stores the value of the environment variable `NAME` in Secret Manager as `<service>-<name>` (a new version on every deploy that passes it), read by the service as `$NAME`. Secrets stay for later deploys that do not pass them. Values are never printed or logged. `GRAPHRT_API_KEY` is reserved. |
| `--label KEY=VALUE` | none | Repeatable. Added to the service, its revisions, bucket and secrets (cost reports, cleanup). Lowercase letters, digits, `_` and `-`. |
| `--app` | from `--deployment` | Cloud Run service name instead of the derived one. Pass it to every later command too. |
| `--image` | `ghcr.io/graphrt/graphrt:<this CLI's version>` | Image running `graphrt serve`. `ghcr.io/...` images are pulled through the `graphrt-ghcr` remote repository, others (Artifact Registry, Docker Hub) as given. |
| `--rotate-api-key` | off | Replace the deployment's API key. The new one is printed once. |
| `--json` | off | JSON Lines on stdout (below). Without it, progress goes to stderr and a summary to stdout. |
| `--chat`, `--monitor`, `--push`... | off | What the deployment serves besides the API, the same as on Fly: see [deploy-direct.md](deploy-direct.md#chat-page-monitoring-and-the-store). Monitoring, devices, push, conversations and channels use the store. |
| `--drop-store` | off | Delete the files of the store once this deploy, which must not need it, serves. |

`--org` is a Fly option and is refused.

Environment: `GOOGLE_OAUTH_ACCESS_TOKEN`, `GOOGLE_APPLICATION_CREDENTIALS`
and `CLOUDSDK_CONFIG` (see [Credentials](#credentials)),
`GOOGLE_CLOUD_PROJECT` (the project without `--project`),
`GRAPHRT_GCP_API_URL` (every Google API at one base URL: tests only).

## Credentials

The command takes the first of these that is set:

1. **An access token** in `GOOGLE_OAUTH_ACCESS_TOKEN`: an OAuth 2.0 access
   token of a Google account (or a service account) with the
   `https://www.googleapis.com/auth/cloud-platform` scope. It is used as it
   is: tokens last an hour, long enough for a deploy (at most about 10
   minutes). Once it has expired the command fails with `unauthorized` and the
   caller passes a fresh one. This is how an app that signed the person in with
   Google hands over the sign in (the desktop app does).
2. **A credentials file** named by `GOOGLE_APPLICATION_CREDENTIALS`: a
   service account key (`"type": "service_account"`) or a person's user
   credentials (`"type": "authorized_user"`: `client_id`, `client_secret`,
   `refresh_token`), whose refresh token is exchanged for access tokens.
3. **Application default credentials**: the `authorized_user` file that
   `gcloud auth application-default login` writes
   (`$CLOUDSDK_CONFIG/application_default_credentials.json`, else
   `~/.config/gcloud/application_default_credentials.json`, on Windows
   `%APPDATA%\gcloud\application_default_credentials.json`).

Organizations created after 2024 forbid service account keys by default
(`iam.managed.disableServiceAccountKeyCreation`), so 1 and 3 are the usual
ways. With a person's credentials (1 and 3) every call names the project in
`x-goog-user-project`, so quota and API use count against that project and
not against the project of the OAuth client: the person needs
`serviceusage.services.use` there, which Owner, Editor and Service Usage
Consumer have.

The metadata server is not tried: outside Google Cloud it only times out.

### For apps that sign people in with Google

An app (such as the desktop app) runs the OAuth 2.0 flow for installed apps
itself (authorization code with PKCE, loopback redirect), keeps the refresh
token in the system keychain, and passes a fresh access token to every
command in `GOOGLE_OAUTH_ACCESS_TOKEN`:

- Scope: `https://www.googleapis.com/auth/cloud-platform`, plus `openid` and
  `email` to show who is signed in. `cloud-platform` is a sensitive scope: an
  OAuth client published to everyone needs Google's verification of the app,
  one in testing (up to 100 test users) or internal to a Workspace
  organization does not.
- Rust programs that link `graphrt-gcp` can hand over the sign in instead:
  `GcpAuthBuilder::user(UserCredentials::new(client_id, client_secret,
  refresh_token))`, or any `TokenSource` with `GcpAuthBuilder::token_source`.

## Permissions

Owner of the project has every permission a deploy needs. Without it, these
roles on the project cover it:

| Role | For |
| --- | --- |
| Cloud Run Admin (`roles/run.admin`) | the service, its revisions and traffic, turning invoker IAM off |
| Storage Admin (`roles/storage.admin`) | the bucket, its policy and objects |
| Secret Manager Admin (`roles/secretmanager.admin`) | the secrets and who may read them |
| Service Account Admin (`roles/iam.serviceAccountAdmin`) | creating and deleting the service account |
| Service Account User (`roles/iam.serviceAccountUser`) | letting the service run as it |
| Artifact Registry Administrator (`roles/artifactregistry.admin`) | the remote repository (only until it exists) |
| Service Usage Admin (`roles/serviceusage.serviceUsageAdmin`) | turning APIs on (only until they are on) |

A missing permission fails with `permission_denied`, naming the permission and
the role to ask for.

## JSON Lines contract

The same as for Fly ([deploy-direct.md](deploy-direct.md#json-lines-contract)):
zero or more `step` lines (`preparing`, `secrets`, `starting`, `checking`,
each `running` then `done` or `failed`), then exactly one `result` or `error`
line, with one `api_key` line before an `error` when a new key was stored
before the failure. Every line validates against
[`schemas/deploy-direct-cloud-run.schema.json`](schemas/deploy-direct-cloud-run.schema.json),
which `crates/graphrt-cli/tests/deploy_direct_cloud_run.rs` enforces. Exit
codes: `0` success, `2` invalid input, `1` any other error.

| `step` | What happens |
| --- | --- |
| `preparing` | local checks, APIs turned on when off, the service account, the bucket and its policy, the remote repository, the agent uploaded to `agents/<sha>/` (nothing when that agent is there already) |
| `secrets` | the API key (first deploy or `--rotate-api-key`) and the `--secret` values stored as new secret versions, readable by the service account (names in the message, never values) |
| `starting` | the new revision is created with 0 % of the traffic and Cloud Run waits for its startup probe `GET /readyz` (up to 3 minutes for the container, 10 for the whole rollout) |
| `checking` | all traffic moves to the new revision, `GET <url>/readyz` must answer 200 from outside (2 minutes), tags of versions beyond the 5 kept are dropped and agent folders nothing uses deleted |

A revision that never gets ready gets no traffic: the previous version keeps
serving and the error says how to read the revision's logs.

Results (all with `"type": "result"` and `"action"`):

| Command | Fields |
| --- | --- |
| deploy | `url`, `version`, `app` (service), `workflow`, `api_key` (or `null`), `serve`, `store` (`{bucket, path, in_use}` or `null`), `project`, `region`, `image` |
| status | `app`, `url`, `status` (`online`, `paused`, `deploying`, `failed`, `offline`), `current_version`, `workflow`, `mode`, `last_deployed_at`, `traffic` (`[{version, revision, percent}]`), `store`, `project`, `region`, `message` (what Cloud Run says about a failed change) |
| list-versions | `app`, `url`, `current_version`, `versions` (newest first: `label`, `deployed_at`, `current`, `revision`, `mode`, `workflow`, `state` `serving` or `standby`), `project`, `region` |
| rollback | `app`, `url`, `version`, `workflow` |
| pause | `app`, `url`, `status: "paused"`, `changed` |
| resume | `app`, `url`, `status: "online"`, `version`, `changed` |
| remove | `app`, `removed`, `secrets_deleted`, `bucket_deleted`, `service_account_deleted` |

### Errors

Every message says what to do. Codes:

| `code` | Meaning and fix in the message |
| --- | --- |
| `invalid_input` | a flag or the agent file (exit `2`) |
| `missing_credentials` | no credentials found: sign in, set a file or a token |
| `unauthorized` | the sign in expired or was revoked: sign in again, or pass a fresh token |
| `permission_denied` | the permission and the role to ask the project's owner for |
| `api_disabled` | an API is off and could not be turned on: the `gcloud services enable` command and the console link |
| `billing_disabled` | the project has no active billing account: the link to link one |
| `org_policy` | an organization policy forbids a step (for example `run.managed.requireInvokerIam` keeps the service private): ask the organization's administrator |
| `name_taken` | the bucket name or the service belongs to something else: pass another `--app` |
| `not_found` | no such deployment (or version for rollback) in this project and region |
| `rate_limited` | a quota is used up |
| `start_timeout` | the rollout did not finish in time |
| `health_check_failed` | the revision never got ready, or `/readyz` did not answer from outside. The previous version keeps serving |
| `gcp_unavailable` | Google Cloud unreachable or 5xx: retry |
| `gcp_api` | Google refused a call for another reason (its message) |
| `internal` | a local failure |

## Versions, rollback, pause and removal

- **Versions.** Each deploy is a revision with a traffic tag (`v2` ->
  `https://v2---<service>-....run.app` reaches that version directly). The
  newest 5 are kept for rollback.
- **Rollback** moves all traffic to a kept version's revision, which runs the
  image, agent and settings it was deployed with. Secrets are read at
  `latest`, so a rolled back version sees today's secrets and API key.
- **Pause** sets the service to internal ingress, turns invoker IAM back on
  and scales it to zero instances by hand (`scalingMode: MANUAL`): nothing
  answers from the internet and nothing is billed but storage. **Resume**
  undoes all three. A deploy also resumes.
- **Remove** deletes the service, its secrets, the bucket with the agents and
  the store, and the service account. The `graphrt-ghcr` remote repository
  stays (other deployments in the region use it, and it only holds cached image
  layers, a few cents a month).

## Notes

- Cloud Run keeps `/healthz` for itself and answers it with its own 404 page:
  use `/readyz` for uptime checks.
- A service answers at two addresses: the `url` of the results
  (`https://<service>-<hash>-<region code>.a.run.app`) and
  `https://<service>-<project number>.<region>.run.app`, known before the first
  deploy, which the pairing links of `--monitor` and `--devices` of a first
  deploy use.
- Requests may run up to 60 minutes (Cloud Run's limit), so a streamed run
  can too.
- Costs of an idle deployment that scales to zero: the bucket (cents), the secrets
  (6 cents per secret version a month after the free 6) and the cached image
  layers in the remote repository.

## Tested on real Cloud Run

Run on 2026-11-28 in a project of a new organization (service account keys
forbidden), with a person's Google sign in (application default credentials,
then an access token in `GOOGLE_OAUTH_ACCESS_TOKEN`), `examples/chatbot.yaml`
with OpenAI, `--chat --conversations`:

| Step | Result |
| --- | --- |
| first deploy, `us-central1` | 27 s, APIs already on, service account, bucket, secrets and the `graphrt-ghcr` remote repository created, a `ghcr.io` image pulled through it |
| blocking run and SSE stream | `succeeded`, the streamed answer in `response` events, the chat page at `/chat/` |
| redeploy with a changed agent (v2), no `--secret` | 22 s, the OpenAI secret kept, the conversation (kept in `store/` of the bucket) still there: the agent remembered the name given before the redeploy |
| `status`, `list-versions` | v2 serving, v1 standby, both with deploy times |
| `rollback v1` | 6 s, the conversation still there |
| `pause` | 7 s, `GET /readyz` answers 404 from outside, manual scaling to 0 instances |
| `resume` | 11 s, `/readyz` 200, conversation intact |
| a revision that fails its startup probe, an image that does not exist | `health_check_failed` and `not_found` with the fix, the serving version untouched |
| `remove` | 21 s: service, 2 secrets, bucket and service account deleted |
| access token, `asia-southeast2`, `--mode always-on` | deploy in 41 s, run `succeeded`, remove |

Cloud Run answers `/healthz` itself with 404, as expected.

# TypeSafe System One (Jev)

[TypeSafe](https://docs.typesafe.ai/introduction) serves *System One* models, Jev
being the first. A System One request sends one **state** (text, JSON or images) plus
named, **typed questions**. Every question is answered in parallel with a typed,
calibrated result instead of generated text:

| Question | Answer |
| --- | --- |
| `choice` | the selected option, a probability per option, a confidence |
| `score` | an expected score over ordered levels, probabilities, a confidence |
| `noul` | the probability that a yes/no statement is true |
| `box` | where something is in an image (fractions of width/height) |

Typical latency is tens to hundreds of milliseconds and input tokens are priced far
below chat models (output tokens are free). That makes it a good fit for the
decisions inside a workflow: routing, classification, moderation, guardrails, triage.

graphrt integrates it in two places:

- the `question-classifier` node can use Jev instead of a chat model;
- the `system-one` node asks any mix of typed questions and can branch on one of them.

## Setup

| Variable | Meaning |
| --- | --- |
| `TYPESAFE_API_KEY` | API key from the [TypeSafe console](https://console.typesafe.ai/) |
| `TYPESAFE_BASE_URL` | Optional endpoint override (default `https://api.typesafe.ai`) |

Hosts register the client as a service:

```rust
use std::sync::Arc;
use graphrt_llm::typesafe::SystemOneClient;

let mut services = graphrt_core::Services::new();
if let Some(client) = SystemOneClient::from_env()? {
    services.insert(Arc::new(client));
}
```

Apps that must not ship an API key use a credential source (bring-your-own-key, token
broker or proxy, see [credentials.md](credentials.md)). A 401/403 invalidates the
credential and retries once:

```rust
use graphrt_llm::typesafe::{SystemOneClient, SystemOneConfig};

let client = SystemOneClient::new(SystemOneConfig::default().credentials(source))?;
```

The client is available with the `typesafe` feature of `graphrt-llm` (on by default).

## `question-classifier` with Jev

Select the engine with `model.provider: typesafe` (or `engine: typesafe`). The model
name is the Jev model or alias (`jev-latest` by default; pin a versioned id such as
`jev-1.14.0` once your thresholds are tuned).

```yaml
- id: route
  type: question-classifier
  model: { provider: typesafe, name: jev-latest }
  query_variable_selector: [start, ticket]
  instruction: Support tickets for a subscription product.
  classes:
    - { id: billing, name: Billing, description: "Payments, refunds, invoices" }
    - { id: bug, name: Bug report }
    - { id: other, name: Other }
  min_confidence: 0.7        # optional
  min_probability: 0.6       # optional
  companion_noul: true       # optional: also ask whether any class fits
  companion_threshold: 0.6
  fallback_class: other      # where low-confidence decisions go
```

The node sends one choice question (option = class id, description = class name and
`description`). Outputs: `class_id`, `class_name`, `class_label`, `probabilities`
(class id -> probability), `confidence`, `usage`.

When a threshold is not met, the node takes `fallback_class`; without one it fails
with the non-retryable `low_confidence` error. Metadata records `low_confidence`,
`predicted_class` and the reasons.

### Thresholds

- **`min_probability`** checks the chosen class's probability: how likely this class
  is compared with the others.
- **`min_confidence`** checks the model's confidence in its decision. A choice can have
  a high probability but low confidence on an unusual input; TypeSafe recommends
  routing on both (see [Confidence](https://docs.typesafe.ai/confidence.md)).
- **`companion_noul`** guards against inputs that fit no class: a choice question always
  picks the closest option, so a second yes/no question asks whether any class fits at
  all ([Companion Noul](https://docs.typesafe.ai/patterns/companion-noul.md)).

### Cascade pattern

Route confident decisions straight away and send the rest to a slower, more expensive
chat-model classifier. [`examples/typesafe/ticket-router.yaml`](../examples/typesafe/ticket-router.yaml)
does exactly that: Jev's `fallback_class: unsure` edge leads to an LLM classifier.

## `system-one` node

Ask several typed questions in one request:

```yaml
- id: judge
  type: system-one
  model: jev-latest
  state:
    comment: "{{#start.comment#}}"
    community: A forum for amateur photographers.
  questions:
    spam:     { type: noul, instructions: "Is `comment` spam?" }
    toxicity: { type: score, instructions: "How hostile is `comment`?",
                criteria: [Neutral, Rude, Insulting, Hateful] }
    action:   { type: choice, instructions: "What should a moderator do?",
                criteria: { publish: "Fine to show", hide: "Spam or against the rules" } }
  route_by: action           # optional: branch on this choice question
  min_confidence: 0.6        # optional: below it, take the `low_confidence` handle
```

| Field | Meaning |
| --- | --- |
| `model` | Model name or alias (default `jev-latest`). |
| `state` | The content (string, object or array). Every string is a template; a string that is exactly one `{{#..#}}` placeholder becomes the typed value. |
| `state_selector` | Alternatively, take the whole state from a variable. |
| `images` | `{name: file variable}`. Each image (PNG/JPEG) is sent as an asset and placed in the state under `name`, so questions can refer to `` `name` ``. Sets the `TypeSafe-Danger-Zone-Images` header. |
| `questions` | `{name: {type, instructions, criteria}}`; `criteria` is `{option: description}` (or a list of option names) for `choice`, an ordered list for `score`, `{true, false}` for `noul`, nothing for `box`. |
| `route_by` | A choice question to branch on. Handles are its option names, plus `low_confidence` when a threshold is set. |
| `min_confidence`, `min_probability` | Thresholds for the routed choice. |
| `long_context` | Send the long-context header (262k-token limits; slower and less accurate). |

Outputs: one object per question (the typed answer, e.g. `{{#judge.spam.noul#}}`,
`{{#judge.toxicity.score#}}`, `{{#judge.action.choice#}}`), `answers` (all answers),
`usage` and `model` (the versioned model that answered).

## Validation and errors

Requests are checked before they are sent: at least one question, 1-255 choice
options, 1-10 score levels, at most 64 nesting levels, every image asset referenced,
and a body of at most 32 MiB. API errors map to node errors:

| Status | Node error | Retried |
| --- | --- | --- |
| 400 (`max_tokens_exceeded`) | `model_context_length` | no |
| 400, 413, 415, 422 | `model_bad_request` (with the failing field) | no |
| 401 / 403 | `model_auth` (after one credential refresh) | no |
| 429 | `model_rate_limit`, honouring `retry-after` | yes |
| 529 / 5xx | `model_server` | yes |

Retries follow the node's `retry` policy.

## Notes

- Box questions and images are marked experimental by TypeSafe.
- An alias such as `jev-latest` moves with new releases; the `model` output tells you
  which version answered. Pin a versioned id when you rely on tuned thresholds.
- The classifier's `vision` and `memory` settings apply to the chat-model engine only.

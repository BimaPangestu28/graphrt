# Self learning

**Preview.** Self learning is opt in: the `graphrt` binary needs the
`learning` cargo feature (`cargo install graphrt-cli --features learning`).
Rust hosts add the `graphrt-learn` crate and turn on the `learning` feature of
`graphrt-llm`. Default builds are unchanged: an agent with `learning` is
refused there with a configuration error. The
format of the lesson store may still change before this leaves preview.

An agent gets better from its own use. Corrections from users and patterns
from earlier runs become **lessons**: short statements that later runs read.

```text
l_3f9a01c2b7d4e611  [approved]  Look up the order before you answer.
    applies to: user sari@example.com
    source: From feedback by sari@example.com on run run-81. (a verified user's own words)
```

Nothing is trained and no instruction is rewritten. Approved lessons are added
to a bounded section of the agent's system prompt, after the instruction.

## How it works

```text
run ends, user gives feedback        (request path: one appended line)
        |
        v
   observations ---- distill ---->  proposals  ---- owner approves ---->  lessons used in runs
                  (outside the run)           (or the policy, in one narrow case)
```

1. **Capture.** The host records a signal: feedback on a run, or how a run
   ended. This appends one line to a file. No model is called.
2. **Distill.** Outside any run, signals become proposals. A feedback comment
   becomes a proposal as written. A host can also pass a distiller (usually a
   model) that suggests lessons from the signals.
3. **Approve.** A proposal is not used until it is approved.
4. **Use.** A run reads the approved lessons of its agent and of its own user.

```yaml
- id: support
  type: agent
  model: { provider: anthropic, name: claude-sonnet-4-5 }
  instruction: "You are the support agent of {{#env.COMPANY#}}."
  learning:
    agent: support-bot            # whose lessons
    user: "{{#sys.user_id#}}"     # optional: add this user's own lessons
```

An agent without `learning` behaves exactly as before. A run with `learning`
only reads: an `agent` node never writes a lesson.

## What keeps it safe

| Rule | Why |
| --- | --- |
| Only approved lessons reach a run | A proposal is text someone or something suggested. It is not an instruction |
| A lesson derived from run content never approves itself | Tool results, documents, web pages and model output can carry planted text. A lesson built from them can only become a proposal |
| Feedback counts as a person's own words only when the host verified who they are and the lesson applies to their own runs | Anyone can type a name into a form |
| A lesson stays with the user it came from | One user cannot change the agent for others. Making a lesson apply to everyone is an owner action (`promote`) |
| A run reads the lessons of its agent and its own user, by exact match | No leaks between users or agents |
| A rejected lesson is not proposed again | Repeating a request does not wear the owner down |
| Lesson text is one line, at most 500 characters, without invisible characters or markup tags | It is placed in the system prompt |
| Everything is bounded | 100 lessons per scope, 50 open proposals, 4,000 characters per run, proposals lapse after 30 days |
| Every change is in an audit log | Who approved what, and when |

### Trust and risk

Every lesson records where it came from and how far that source is trusted:

| Trust | Source |
| --- | --- |
| `owner` | Written or edited by the owner |
| `user` | Feedback by an authenticated user, about their own run |
| `derived` | Everything else: suggestions of a distiller, feedback that was not authenticated, feedback about someone else's run |

Risk is `high` when the text is about sending or sharing, money, approval,
deleting or changing records, credentials, tools or instructions, when it
contains a web address, an email address or a long number, or when the skill
scan flags it. The check knows English and Indonesian words. It errs towards
`high`, which only means that a person decides.

Each reason is a `{code, message}` pair in `risk_reasons`. The message is plain
English. The code is stable, for hosts that show it in another language:
`sending`, `money`, `approval`, `changes_records`, `credentials`,
`tools_or_instructions`, `web_address`, `email_address`, `long_number`,
`masked_secret` (a value the host already masked, such as `[redacted]`), or the
code of a skill scan finding (`overrides_instructions`, `hides_from_user`,
`skips_approval`, `touches_credentials`, `hidden_characters`,
`download_and_run`).

**Automatic approval** is off by default. When the owner turns it on for an
agent (`graphrt learn policy <agent> --auto-approve true`), a lesson is
approved without a person only if all of these hold: trust is `user`, risk is
`low`, and the lesson applies only to that same user. The worst case is then a
low risk preference that one verified user set for their own runs.

The risk check is a set of pattern checks and can be evaded by rewording. That
is why it never decides alone: it can only send a lesson to the owner, and the
rule about derived content does not depend on it.

## From lessons to a skill

Lessons suit a preference or a fact. When three or more approved lessons
describe how to do a task (kinds `procedure` and `correction`), a model can
write them up as one skill the agent loads when that task comes up:

```sh
graphrt learn skill support-bot --model openai/gpt-4.1 --out drafts
graphrt skill install drafts/<name>
```

Only lessons that are approved and apply to every run are used. A proposal is
not trusted yet, and one user's lesson may hold that user's data, which must
not end up in a skill every run can load. The draft is a normal skill draft:
a person reads it, the same scan runs, and installing grants no tool. See
[skills.md](skills.md#writing-a-skill-from-plain-language).

A feedback comment that cannot be recorded for now, because too many proposals
are open or the scope is full, stays in the queue and is tried again on the
next `distill` (`kept` in the report). A comment that can never be a lesson is
dropped and listed in `skipped`.

`Lesson::origin_sentence` is an English sentence. Hosts in another language
build their own from `origin.signal`, `origin.author`, `origin.run_id` and
`created_at_ms`.

## `graphrt learn`

Every subcommand takes `--dir <DIR>` (the lesson store), `--json`, and
`--actor <NAME>` (who is making the change, for the audit log, default
`owner`). `AGENT` is the name the `agent` node gives in `learning.agent`.

| Command | Description |
| --- | --- |
| `graphrt learn list AGENT [--user U \| --shared] [--status S]` | What the agent learned, with the source of each lesson. |
| `graphrt learn add AGENT "<text>" [--user U] [--kind K]` | Add a lesson yourself. Used from the next run on. |
| `graphrt learn approve\|reject\|retire AGENT ID` | Decide about a lesson. |
| `graphrt learn edit AGENT ID "<text>"` | Correct a lesson. The text becomes the owner's. A proposal still needs approval. |
| `graphrt learn promote AGENT ID` | Make one user's lesson apply to every run. |
| `graphrt learn delete AGENT ID` | Delete a lesson for good. |
| `graphrt learn forget AGENT --user U` / `--run R` | Delete everything a user taught and everything about them, or everything from one run. |
| `graphrt learn feedback AGENT --run R --comment "<text>" [--user U [--authenticated]]` | Record feedback. Nothing is learned yet. |
| `graphrt learn distill AGENT [--model P/N] [--providers FILE]` | Turn recorded feedback into proposals. With `--model`, a model also suggests lessons from the recorded runs. Those are `derived` and always wait for approval. |
| `graphrt learn skill AGENT --model P/N [--tool T]... [--out DIR]` | Write a skill draft from the procedures the agent learned. Review it, then `graphrt skill install`. |
| `graphrt learn policy AGENT [--auto-approve BOOL]` | Show or change the policy. |
| `graphrt learn audit AGENT` | The history of changes. |

The store is `--dir`, else `$GRAPHRT_LEARNING_DIR`, else `~/.graphrt/learning`.
`graphrt run` and `graphrt serve` read the same default. It is created owner
only (`0700` directories, `0600` files on Unix). Use one store per tenant.

## HTTP API

`graphrt serve` in a build with the `learning` feature serves the default
lesson store. Every route needs the API key: these are owner actions. A body
may carry `actor`, the name recorded in the audit log (default `api`).

| Method | Path | Body or query | Response |
| --- | --- | --- | --- |
| GET | `/v1/learning/{agent}/lessons` | `?user=`, `?shared=true`, `?status=` | `{lessons: [...]}`, each with `origin_sentence` |
| POST | `/v1/learning/{agent}/lessons` | `{text, user?, kind?}` | `201` and the lesson (approved, trust `owner`) |
| PATCH | `/v1/learning/{agent}/lessons/{id}` | one of `{decision: approve\|reject\|retire}`, `{text}`, `{promote: true}` | The lesson |
| DELETE | `/v1/learning/{agent}/lessons/{id}` | | `204` |
| POST | `/v1/learning/{agent}/forget` | `{user}` or `{run_id}` | `{removed}` |
| POST | `/v1/learning/{agent}/distill` | | `{observations, lessons, skipped, kept}` |
| GET, PATCH | `/v1/learning/{agent}/policy` | `{auto_approve_user_feedback}` | The policy |
| GET | `/v1/learning/{agent}/audit` | | `{entries: [...]}` |

**Feedback is captured automatically.** `POST /v1/runs/{id}/feedback` (see
[serving.md](serving.md)) records a signal for every agent that the run's
workflow names in `learning.agent`. Nothing is learned at that moment: the
signal waits for `distill`. Feedback counts as a verified user's own words
only when it came with that user's token (`--verify-jwt`) for their own run.
A `user` that an API key caller puts in the body is a label, so such feedback
is `derived` and always waits for the owner.

`distill` over HTTP calls no model. Use `graphrt learn distill --model` or the
Rust API for suggestions by a model.

## Embedding

```rust
use std::sync::Arc;
use graphrt_learn::{Decision, Learner, LessonFilter, LessonStatus, LessonStore, Observation, ObservationKind};

let store = Arc::new(LessonStore::open(app_data_dir.join("learning")));   // no I/O yet
services.insert(Arc::clone(&store).into_service());                       // for `agent` nodes

// On the request path: record a signal.
let learner = Learner::new(Arc::clone(&store));
learner.observe(Observation {
    agent: "support-bot".into(),
    user: Some(user_id.clone()),
    run_id: run_id.clone(),
    kind: ObservationKind::Feedback { value: None, comment: Some(comment), author: Some(user_id), authenticated: true },
    excerpt: Some(masked_excerpt),
    at_ms: 0,
})?;

// Later, when idle: propose. A `ModelDistiller` lets a model suggest lessons too
// (any `graphrt_core::text_model::TextModel`, e.g. `graphrt_llm::text_model::ChatTextModel`).
let distiller = ModelDistiller::new(Arc::new(ChatTextModel::new(chat_model, "gpt-4.1")));
let report = learner.distill("support-bot", Some(&distiller)).await?;

// The review screen.
let waiting = store.list("support-bot", &LessonFilter { status: Some(LessonStatus::Proposed), ..Default::default() })?;
store.decide("support-bot", &waiting[0].id, Decision::Approve, "dewi")?;
```

| Call | Use |
| --- | --- |
| `LessonStore::list`, `get` | Show lessons. `Lesson::origin_sentence` gives the source as a sentence |
| `LessonStore::add` | The owner writes a lesson |
| `LessonStore::decide`, `edit`, `promote`, `delete`, `forget` | Approve, correct, widen, remove |
| `LessonStore::policy`, `set_policy`, `audit` | Settings and history |
| `Learner::observe`, `pending`, `distill` | Capture and propose |
| `ModelDistiller::new` | A model that suggests lessons, for `Learner::distill` |
| `draft_skill_from_lessons`, `procedure_lessons` | A skill draft from learned procedures. Install it with `SkillStore::install_draft` and origin `learned` |
| `LessonStore::into_service` | The `Lessons` service `agent` nodes look up |

`Lesson`, `AuditEntry` and `LearningPolicy` serialize to JSON with stable field
names, and the CLI prints the same shapes with `--json`.

The model behind a `ModelDistiller` reads run excerpts, which can hold planted
text. It gets them as a quoted block, and whatever it answers becomes a
`derived` proposal tied to the user of the run it names. No answer can make a
lesson approve itself or apply to everyone.

Things a host must do itself:

- **Mask secrets** in excerpts and errors before passing them to `observe`.
- **Decide who is the owner.** The store records the `actor` of each change. It
  does not authenticate anyone.
- **Set `authenticated` truthfully.** It is the only thing that gives feedback
  `user` trust.

The `agent` node reports the lessons it used as a `lessons_applied` progress
event (`{agent, ids}`).

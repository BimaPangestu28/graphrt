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

**Automatic approval** is off by default. When the owner turns it on for an
agent (`graphrt learn policy <agent> --auto-approve true`), a lesson is
approved without a person only if all of these hold: trust is `user`, risk is
`low`, and the lesson applies only to that same user. The worst case is then a
low risk preference that one verified user set for their own runs.

The risk check is a set of pattern checks and can be evaded by rewording. That
is why it never decides alone: it can only send a lesson to the owner, and the
rule about derived content does not depend on it.

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
| `graphrt learn distill AGENT` | Turn recorded feedback into proposals. |
| `graphrt learn policy AGENT [--auto-approve BOOL]` | Show or change the policy. |
| `graphrt learn audit AGENT` | The history of changes. |

The store is `--dir`, else `$GRAPHRT_LEARNING_DIR`, else `~/.graphrt/learning`.
`graphrt run` and `graphrt serve` read the same default. It is created owner
only (`0700` directories, `0600` files on Unix). Use one store per tenant.

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

// Later, when idle: propose. Pass a `Distiller` to let a model suggest lessons too.
let report = learner.distill("support-bot", None).await?;

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
| `LessonStore::into_service` | The `Lessons` service `agent` nodes look up |

`Lesson`, `AuditEntry` and `LearningPolicy` serialize to JSON with stable field
names, and the CLI prints the same shapes with `--json`.

Things a host must do itself:

- **Mask secrets** in excerpts and errors before passing them to `observe`.
- **Decide who is the owner.** The store records the `actor` of each change. It
  does not authenticate anyone.
- **Set `authenticated` truthfully.** It is the only thing that gives feedback
  `user` trust.

The `agent` node reports the lessons it used as a `lessons_applied` progress
event (`{agent, ids}`).

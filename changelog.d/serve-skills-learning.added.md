- `graphrt serve` (cargo features `skills` and `learning`):
  `/v1/skills` lists, checks, installs, grants and removes skills, and
  `/v1/learning/{agent}/...` lists, adds, approves, edits, promotes, forgets
  and distills lessons, with the policy and the audit log. Feedback posted to
  `POST /v1/runs/{id}/feedback` is recorded as a learning signal for the
  agents the run's workflow names in `learning.agent`. Only feedback sent with
  a verified user's token counts as that user's own words.

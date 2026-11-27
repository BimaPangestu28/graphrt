- A failed run no longer keeps answer text of an answer node that never ran.
  The ordered response is rendered ahead of time, as soon as an answer node is
  certain to run, so a run that failed before that node had its literal text as
  `answer` (in `graphrt serve` records, `graphrt run`, the FFI and eval
  captures). On `run_failed` nothing more is rendered now, and the answer holds
  only the text of answer nodes that ran. A stopped run keeps what it streamed.

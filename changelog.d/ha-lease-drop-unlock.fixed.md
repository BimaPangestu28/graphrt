- `graphrt gateway` with `ha:`: a gateway that leaves the lease unlocks its
  lease files itself instead of only closing them. An agent process spawned
  at that moment shared the locks until it started, so a passive could see
  the departed holder as still running for one poll.

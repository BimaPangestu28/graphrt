- Gateway active/passive: a live active whose lease write stalls (a busy
  volume, a virus scanner; seen for about 3 s on Windows) is no longer taken
  for frozen and killed by a passive on the same host. The active now also
  overwrites a small liveness mark (`graphrt-gateway.beat`) in place every
  heartbeat, on a thread of its own apart from the durable lease write, and
  passives look at both. A lease write slower than `stale_after_ms` is logged.

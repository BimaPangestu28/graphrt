- Gateway active/passive: a live active whose lease write stalls (a busy
  volume, a virus scanner; seen for about 3 s on Windows) is no longer taken
  for frozen and killed by a passive on the same host. The active now also
  overwrites a small liveness mark (`graphrt-gateway.beat`) in place every
  heartbeat, apart from the durable lease write, and passives look at both.
  A stuck lease write is logged. `graphrt gateway ha status` shows the mark.

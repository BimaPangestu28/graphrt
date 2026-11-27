- `graphrt gateway`: a replica whose run stream broke is no longer left
  running at the provider until the next reconciliation (up to a minute) when
  other replicas serve the runs after it. The pool keeps it, gives it no new
  runs and stops it once idle for `idle_timeout`, as it releases any idle
  replica. A wake that reports it up again puts it back in service.

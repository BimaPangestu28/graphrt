- `graphrt serve`: a run's nodes no longer wait for its event writes. The
  driver of each run stopped taking events from the engine while a batch went
  to the store, so on a slow disk every node of the run, and its preemption or
  abort, waited for the write (a shutdown drain could miss its deadline). A
  write now goes on next to the run, and the events that come meanwhile go out
  together in the next one: on an NVMe `--store` directory a streaming run of
  100 quick nodes ends in 23 ms instead of 133 ms. Batches (32 events or
  250 ms), retries and the final write are unchanged. A run that holds 40,000
  unwritten events waits for the store, unless it was asked to stop.
- `graphrt-core`: `RunHandle::stop_requested` tells whether a run was asked to
  abort, pause or preempt.

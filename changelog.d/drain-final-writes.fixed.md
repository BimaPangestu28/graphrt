- `graphrt serve`: on a slow disk a shutdown drain could leave every active run
  `unfinished` (and failed after the restart) although it had paused them in
  time. Each run stored its final state in three writes through the `--store`
  directory's one lock, interleaved with every other run's, so no run was
  complete until nearly all writes were done. A run now stores its last events,
  its snapshot and its record in one write (one lock, four syncs instead of
  seven), so runs are stored one after another and a drain that runs out of
  time still keeps most of them. Store backends can do the same through the new
  `RunStore::finish` (the default writes the parts one by one).

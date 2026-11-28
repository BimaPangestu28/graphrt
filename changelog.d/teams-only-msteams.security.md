- Teams channel: only activities of the `msteams` channel are read. The
  connector also signs Web Chat and Direct Line deliveries of the same bot,
  whose sender picks its own `from.id`, so such a delivery could speak as a
  Teams user (and reach their conversation and memory).

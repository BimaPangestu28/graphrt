- graphrt-skill and graphrt-learn, for hosts with their own
  language and screens: `Finding::params` holds the details of a finding as
  data, `SkillEntry::title` (from `metadata.display_name` or `metadata.title`) and `SkillOrigin::version` give a display name and
  the catalog's version, `SkillDetails::installed` of `inspect_dir` now says
  whether a skill of that name is installed, the risk reason `masked_secret`
  flags text with a masked value, and `Learner::distill` keeps a feedback
  comment in the queue when it cannot be recorded yet (`DistillReport::kept`).

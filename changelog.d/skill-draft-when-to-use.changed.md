- graphrt-skill (preview): a drafted skill's description now says when to use
  the skill ("Use when ...") instead of repeating its steps, so an agent loads
  the skill instead of acting on the summary line alone. A draft can carry a
  name for people (`SkillDraft::with_title`, written as `metadata.title` and
  read back as `SkillEntry::title`).

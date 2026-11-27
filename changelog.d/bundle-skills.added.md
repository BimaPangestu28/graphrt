- Skills in bundles (cargo features `skills` and `bundle`):
  `graphrt bundle build` packs the skills a workflow's agents list, with
  their grants, and `graphrt run` installs them into a private store for the
  run through the normal install gate. `SkillStore::export` returns the
  pinned files of an installed skill.

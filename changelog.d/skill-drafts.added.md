- Skill drafts (preview): `graphrt_skill::draft_skill` writes a `SKILL.md`
  from a description in plain language or from an example conversation, with
  any model behind the new `graphrt_core::text_model::TextModel` trait
  (`graphrt_llm::text_model::ChatTextModel`, feature `text-model`). A draft is
  installed through the same gate as any skill (`SkillStore::install_draft`,
  origin `authored`), can only ask for tools the host offered, and gets none
  by being installed. `graphrt skill draft` writes the draft as a folder.

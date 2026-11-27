- Agents can create documents: the new `graphrt-docgen` crate and
  `--document-tools` (with `--workspace`) add the `documents` tools.
  `create_pdf` and `create_docx` turn Markdown (headings, paragraphs, lists,
  tables, code blocks, bold and italic) into a PDF or a Word document, and
  `create_xlsx` turns rows of cells into an Excel workbook. The writers are
  small and in the crate, with no new dependency: the files hold text,
  tables and lines and never a script, macro, link, embedded file or
  formula. Spreadsheet text that starts with `=` stays text. PDF text is
  Western European (the fonts built into every reader), other characters
  are written as `?` and counted in the result. Files go through the
  workspace, so paths, quota and user directories apply. See
  `docs/files-and-images.md` and `examples/documents.yaml`.

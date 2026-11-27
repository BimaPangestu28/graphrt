- `files/search` takes regular expressions (`regex: true`) and can search
  inside PDF and Office documents (`documents: true`, or a single document
  named by `path`). Patterns run on a linear time engine without
  backtracking, their compiled size and nesting are limited, and every
  search now ends at a time limit (`FileToolsConfig::max_search_time`, 5
  seconds) with what it found. At most 25 documents are extracted per search
  (`max_search_documents`), unreadable ones are counted and skipped, and
  extracted text is kept so the `read` after a hit does not parse the file
  again. Large files are matched on the blocking pool. New `regex` feature of
  `graphrt-files`, on by default.

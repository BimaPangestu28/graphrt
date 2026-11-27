- `graphrt skill install <name>` and `graphrt skill search` no longer panic
  ("Cannot start a runtime from within a runtime") when they read the catalog
  over HTTPS, which is the default. Catalogs read from a file were not
  affected.

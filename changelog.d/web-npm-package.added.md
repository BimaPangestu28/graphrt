- npm: the browser side of `graphrt serve` is one package, `graphrt`, with the
  entry points `graphrt/client`, `graphrt/ui`, `graphrt/chat` and
  `graphrt/monitor`. A bundler keeps only the parts a page imports, and its
  version is the version of the graphrt release. The release workflow publishes
  it with npm trusted publishing and a provenance statement when the
  repository variable `PUBLISH_NPM` is `true`.

- The container image build (`Dockerfile`) gives each platform its own cargo
  caches, used by one build at a time. A build of `linux/amd64` and
  `linux/arm64` together, as the release workflow does, could fail with
  "failed to unpack package ... File exists" when both unpacked the same crate.
- The release workflow no longer publishes the image job's build record
  (`*.dockerbuild`) as a release asset.
- `scripts/publish.sh --execute` (the release workflow's crates.io job) fails
  when a crate is refused, and waits for the time crates.io names when it
  answers 429 for new crates. With colour on (`CARGO_TERM_COLOR=always`, as in
  CI) it took a refused crate for published, so the 0.6.3 job ended green with
  five crates missing.

- The container image build (`Dockerfile`) gives each platform its own cargo
  caches, used by one build at a time. A build of `linux/amd64` and
  `linux/arm64` together, as the release workflow does, could fail with
  "failed to unpack package ... File exists" when both unpacked the same crate.

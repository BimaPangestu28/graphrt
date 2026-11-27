- HEIC photos as vision input and for the `image` tools, behind the new
  `heic` feature of `graphrt-image` and `image-heic` of the CLI. Off by
  default in every build: HEVC is covered by patent pools whatever the
  licence of the software is, so shipping a decoder is left to whoever
  builds the binary. A HEIC photo is checked from its header by a parser of
  our own before the decoder runs, decoded in safe Rust (`heifer`, MIT or
  Apache-2.0) with the same pixel limit, and always leaves as JPEG or PNG
  without its metadata. Graphrt does not write HEIC. AVIF stays refused:
  the safe Rust AV1 decoders are under the AGPL. See decision 15 in
  `docs/files-and-images-design.md`.

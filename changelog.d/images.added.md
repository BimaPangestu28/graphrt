- Images for agents: the new `graphrt-image` crate. Every image goes through
  one gate: only PNG, JPEG, GIF and WebP by magic number (an SVG, an HTML
  page or a HEIC photo sent as `image/png` is refused), at most 20 MiB and
  25 megapixels read from the header (an image bomb is refused before any
  decoder runs), EXIF with its GPS position, XMP, IPTC, text chunks and
  anything appended after the image removed, and images above 1,568 px on
  the long side turned upright, downscaled and encoded again on the blocking
  pool. `--image-tools` adds the `image` tools to a workspace
  (`image/info`, `view`, `resize`, `convert`, `thumbnail`), and
  `--image-model NAME` adds `image/generate` through the OpenAI images API.
  Generation sits behind the `ImageGenerator` trait. See
  `docs/files-and-images.md`.
- `graphrt_core::file::ImagePreparer`: a host service that turns an image
  file into what a model request carries. Model nodes with `vision` ask it
  for every image (`VisionConfig::prepared_image_parts`, behind the
  `vision-preparer` feature of `graphrt-llm`, which the CLI's `image` feature
  enables and the slim agent builds leave out).

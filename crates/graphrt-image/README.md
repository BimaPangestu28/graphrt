# graphrt-image

Image handling for graphrt agents: header probing, metadata stripping, bounded decoding and resizing, vision input preparation, the `image` tools and pluggable image generation.

Part of [graphrt](https://github.com/Graphrt/graphrt) — **run your agent everywhere**: one small runtime for
agentic AI workflows on servers, containers, the edge and phones.

```toml
[dependencies]
graphrt-image = "0.6"
```

```rust
use graphrt_image::{ImageLimits, prepare_for_model};

// Checks the real type and the pixel count from the header, removes EXIF (GPS position,
// device, time), and downscales to what a model can use. CPU work: run it off the async workers.
let image = prepare_for_model(photo_bytes, &ImageLimits::default())?;
let url = image.to_data_url(); // data:image/jpeg;base64,...
```

Every image goes through the same gate: only PNG, JPEG, GIF and WebP by magic number, an image
that declares too many pixels is refused before any decoder runs, and nothing that leaves the
runtime carries metadata.

- Documentation: <https://docs.rs/graphrt-image>
- Guide: [Files and images](https://github.com/Graphrt/graphrt/blob/main/docs/files-and-images.md#images)
- Design and threat model: [files-and-images-design.md](https://github.com/Graphrt/graphrt/blob/main/docs/files-and-images-design.md)
- Project README: <https://github.com/Graphrt/graphrt#readme>

Licensed under the Apache License, Version 2.0.

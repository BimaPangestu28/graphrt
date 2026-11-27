- **Breaking** (`graphrt-image`): `ImageFormat` has a new variant, `Heic`,
  which builds without the `heic` feature never return. Code that matches
  on `ImageFormat` needs an arm for it. `ImageFormat::is_writable` tells
  the formats the crate encodes from the one it only reads.

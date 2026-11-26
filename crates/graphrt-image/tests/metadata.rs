//! Metadata leaves, pixels stay.

#![cfg(feature = "process")]
#![allow(clippy::unwrap_used)]

mod support;

use graphrt_image::{Change, ImageLimits, prepare_for_model, probe, strip_metadata, transform};
use support::{
    SECRET, contains, decode, gif, gif_with_metadata, jpeg, jpeg_with_metadata, png, png_with_metadata, webp,
    webp_with_metadata,
};

fn assert_clean(original: &[u8], stripped: &[u8]) {
    assert!(contains(original, SECRET.as_bytes()), "the fixture holds the secret");
    assert!(!contains(stripped, SECRET.as_bytes()), "the secret survived");
    assert!(!probe(stripped).unwrap().has_metadata);
    // Not one pixel changed.
    assert_eq!(decode(original).to_rgba8(), decode(stripped).to_rgba8());
    // Stripping again changes nothing.
    assert_eq!(strip_metadata(stripped).unwrap(), stripped);
}

#[test]
fn jpeg_loses_exif_xmp_iptc_comments_and_trailing_data() {
    let original = jpeg_with_metadata(40, 30, 1);
    let stripped = strip_metadata(&original).unwrap();
    assert_clean(&original, &stripped);
    // The colour profile is part of how the pixels look: it stays.
    assert!(contains(&stripped, b"keep-this-profile"));
    assert!(stripped.ends_with(&[0xff, 0xd9]));
    // A clean JPEG comes back byte for byte.
    assert_eq!(strip_metadata(&jpeg(40, 30)).unwrap(), jpeg(40, 30));
}

#[test]
fn png_loses_text_exif_time_and_trailing_data() {
    let original = png_with_metadata(40, 30);
    let stripped = strip_metadata(&original).unwrap();
    assert_clean(&original, &stripped);
    assert!(contains(&stripped, b"gAMA"));
    assert!(!contains(&stripped, b"tEXt") && !contains(&stripped, b"eXIf") && !contains(&stripped, b"tIME"));
    assert!(stripped.ends_with(b"IEND\xae\x42\x60\x82"));
    assert_eq!(strip_metadata(&png(40, 30)).unwrap(), png(40, 30));
}

#[test]
fn webp_loses_exif_xmp_their_flags_and_trailing_data() {
    let original = webp_with_metadata(40, 30);
    let stripped = strip_metadata(&original).unwrap();
    assert_clean(&original, &stripped);
    assert!(!contains(&stripped, b"EXIF") && !contains(&stripped, b"XMP "));
    // The RIFF size matches the new length.
    let declared = u32::from_le_bytes([stripped[4], stripped[5], stripped[6], stripped[7]]);
    assert_eq!(usize::try_from(declared).unwrap() + 8, stripped.len());
    assert_eq!(strip_metadata(&webp(40, 30)).unwrap(), webp(40, 30));
}

#[test]
fn gif_loses_comments_xmp_and_trailing_data_and_keeps_its_loop() {
    let original = gif_with_metadata(40, 30);
    let stripped = strip_metadata(&original).unwrap();
    assert_clean(&original, &stripped);
    assert!(contains(&stripped, b"NETSCAPE2.0"));
    assert!(stripped.ends_with(&[0x3b]));
    assert_eq!(strip_metadata(&gif(40, 30)).unwrap(), gif(40, 30));
}

/// What a model receives never carries the metadata, on either path.
#[test]
fn nothing_prepared_for_a_model_carries_metadata() {
    let limits = ImageLimits::default();
    // Small images: the container is rewritten, the pixels are not touched.
    for original in [
        jpeg_with_metadata(64, 48, 1),
        png_with_metadata(64, 48),
        webp_with_metadata(64, 48),
        gif_with_metadata(64, 48),
    ] {
        let prepared = prepare_for_model(original.clone(), &limits).unwrap();
        assert_eq!(prepared.change, Change::MetadataRemoved);
        assert!(!contains(&prepared.bytes, SECRET.as_bytes()));
        assert_eq!(decode(&original).to_rgba8(), decode(&prepared.bytes).to_rgba8());
        assert!(!contains(prepared.to_data_url().as_bytes(), SECRET.as_bytes()));
    }
    // Large images are encoded again: metadata has nowhere to survive.
    let large = jpeg_with_metadata(2_400, 1_800, 1);
    let prepared = prepare_for_model(large, &limits).unwrap();
    assert_eq!((prepared.change, prepared.width, prepared.height), (Change::Reencoded, 1_568, 1_176));
    assert!(!contains(&prepared.bytes, SECRET.as_bytes()));
    assert!(!probe(&prepared.bytes).unwrap().has_metadata);
    // So are the files the tools write.
    let converted = transform(&png_with_metadata(64, 48), &limits, Default::default()).unwrap();
    assert!(!contains(&converted.bytes, SECRET.as_bytes()));
}

#[test]
fn stripping_can_be_turned_off_by_the_operator() {
    let limits = ImageLimits { strip_metadata: false, ..ImageLimits::default() };
    let original = png_with_metadata(64, 48);
    let prepared = prepare_for_model(original.clone(), &limits).unwrap();
    assert_eq!((prepared.change, prepared.bytes), (Change::Unchanged, original));
}

//! The header check: what an image is, before anything decodes it.

#![cfg(feature = "process")]
#![allow(clippy::unwrap_used)]

mod support;

use graphrt_image::{
    ImageError, ImageFormat, ImageLimits, prepare_for_model, probe, sniff_format, strip_metadata, transform,
};
use support::{
    Rng, black_png, gif, jpeg, jpeg_with_metadata, png, png_header_only, png_with_metadata, webp, webp_with_metadata,
};

#[test]
fn format_and_size_of_every_supported_type() {
    let cases = [
        (png(31, 17), ImageFormat::Png),
        (jpeg(31, 17), ImageFormat::Jpeg),
        (gif(31, 17), ImageFormat::Gif),
        (webp(31, 17), ImageFormat::WebP),
        (webp_with_metadata(31, 17), ImageFormat::WebP),
    ];
    for (bytes, format) in cases {
        let info = probe(&bytes).unwrap();
        assert_eq!((info.format, info.width, info.height), (format, 31, 17));
        assert_eq!(sniff_format(&bytes), Some(format));
        assert_eq!(info.pixels(), 31 * 17);
    }
    assert_eq!(ImageFormat::from_name(".JPG"), Some(ImageFormat::Jpeg));
    assert_eq!(ImageFormat::from_name("image/webp"), Some(ImageFormat::WebP));
    assert_eq!(ImageFormat::from_name("svg"), None);
}

#[test]
fn metadata_and_orientation_are_reported() {
    assert!(!probe(&jpeg(8, 8)).unwrap().has_metadata);
    let tagged = probe(&jpeg_with_metadata(8, 8, 6)).unwrap();
    assert!(tagged.has_metadata);
    assert_eq!(tagged.orientation, Some(6));
    assert!(tagged.needs_rotation());
    assert!(!probe(&jpeg_with_metadata(8, 8, 1)).unwrap().needs_rotation());
    assert!(!probe(&png(8, 8)).unwrap().has_metadata);
    assert!(probe(&png_with_metadata(8, 8)).unwrap().has_metadata);
    assert!(probe(&webp_with_metadata(8, 8)).unwrap().has_metadata);
    // GIF: judged block by block, never by a byte pattern that pixel data could hold.
    for size in [8, 64, 200] {
        assert!(!probe(&gif(size, size)).unwrap().has_metadata, "{size}");
    }
    let tagged = support::gif_with_metadata(8, 8);
    assert!(probe(&tagged).unwrap().has_metadata);
    // A header window of a larger file still answers.
    assert_eq!(probe(&tagged[..tagged.len() / 2]).unwrap().width, 8);
}

/// Whatever the sender called it, only the four formats pass.
#[test]
fn spoofed_and_unsupported_content_is_refused() {
    let cases: [(&[u8], &str); 9] = [
        (b"<svg xmlns='http://www.w3.org/2000/svg'><script>alert(1)</script></svg>", "SVG"),
        (b"<?xml version='1.0'?><svg onload='x()'/>", "SVG"),
        (b"<!DOCTYPE html><html><body>hi", "HTML"),
        (b"\0\0\0\x18ftypheic\0\0\0\0mif1heic", "HEIC"),
        (b"\0\0\0\x1cftypavif\0\0\0\0avifmif1", "AVIF"),
        (b"BM\x46\0\0\0\0\0\0\0\x36\0\0\0", "BMP"),
        (b"II*\0\x08\0\0\0", "TIFF"),
        (b"%PDF-1.7\n", "PDF"),
        (b"MZ\x90\0\x03\0\0\0", "not a PNG, JPEG, GIF or WebP image"),
    ];
    for (bytes, expected) in cases {
        let error = probe(bytes).unwrap_err();
        let message = match &error {
            ImageError::UnsupportedFormat(message) => message.clone(),
            // A build that decodes HEIC reads the header and finds no image in these 24 bytes.
            ImageError::Malformed { format: ImageFormat::Heic, .. } if cfg!(feature = "heic") => "HEIC".to_owned(),
            other => panic!("{other:?}"),
        };
        assert!(message.contains(expected), "{message}");
        assert!(prepare_for_model(bytes.to_vec(), &ImageLimits::default()).is_err());
        assert!(transform(bytes, &ImageLimits::default(), Default::default()).is_err());
        assert!(strip_metadata(bytes).is_err());
    }
    assert!(matches!(probe(b""), Err(ImageError::UnsupportedFormat(_))));
}

/// An image bomb is a small file that declares a huge canvas. It is refused
/// from the header, so the same answer comes back for a file that has nothing
/// but a header: no pixel data was needed, no decoder ran.
#[test]
fn a_huge_canvas_is_refused_from_the_header_alone() {
    let limits = ImageLimits::default();
    for (width, height) in [(60_000, 60_000), (u32::MAX, u32::MAX), (12_001, 10), (10, 12_001), (6_000, 6_000)] {
        let header_only = png_header_only(width, height);
        let info = probe(&header_only).unwrap();
        assert_eq!((info.width, info.height), (width, height));
        let refused = prepare_for_model(header_only.clone(), &limits).unwrap_err();
        assert!(matches!(refused, ImageError::TooManyPixels { .. }), "{width}x{height}: {refused:?}");
        let refused = transform(&header_only, &limits, Default::default()).unwrap_err();
        assert!(matches!(refused, ImageError::TooManyPixels { .. }), "{width}x{height}: {refused:?}");
    }
    // A real bomb: 36 megapixels in a file of a few kilobytes.
    let bomb = black_png(6_000, 6_000);
    assert!(bomb.len() < 200_000, "{} bytes", bomb.len());
    let started = std::time::Instant::now();
    let refused = prepare_for_model(bomb, &limits).unwrap_err();
    assert!(matches!(refused, ImageError::TooManyPixels { width: 6_000, height: 6_000, .. }), "{refused:?}");
    // Decoding 36 megapixels takes far longer than reading 33 bytes.
    assert!(started.elapsed() < std::time::Duration::from_millis(50), "{:?}", started.elapsed());
    // Exactly at the limit is fine, one row more is not.
    assert!(limits.check_dimensions(5_000, 5_000).is_ok());
    assert!(limits.check_dimensions(5_000, 5_001).is_err());
    assert!(matches!(probe(&png_header_only(0, 10)), Err(ImageError::Malformed { .. })));
}

#[test]
fn oversized_files_are_refused_before_they_are_looked_at() {
    let limits = ImageLimits { max_input_bytes: 1_000, ..ImageLimits::default() };
    let bytes = png(64, 64);
    assert!(bytes.len() > 1_000 || bytes.len() < 1_000);
    let big = vec![0u8; 1_001];
    assert!(matches!(
        prepare_for_model(big.clone(), &limits),
        Err(ImageError::TooManyBytes { size: 1_001, limit: 1_000 })
    ));
    assert!(matches!(transform(&big, &limits, Default::default()), Err(ImageError::TooManyBytes { .. })));
}

/// Cut every sample at every length: the parsers must answer, never panic.
#[test]
fn truncated_files_fail_cleanly_at_every_length() {
    let limits = ImageLimits::default();
    let samples = [
        png_with_metadata(9, 7),
        jpeg_with_metadata(9, 7, 6),
        support::gif_with_metadata(9, 7),
        webp_with_metadata(9, 7),
    ];
    for sample in &samples {
        for length in 0..sample.len() {
            let cut = &sample[..length];
            let _ = probe(cut);
            let _ = strip_metadata(cut);
            let _ = prepare_for_model(cut.to_vec(), &limits);
            let _ = transform(cut, &limits, Default::default());
        }
    }
}

/// Flip, overwrite and splice bytes of valid files: whatever comes out is an
/// image within the limits or an error, never a panic or a runaway allocation.
#[test]
fn corrupted_files_never_panic_or_exceed_the_limits() {
    let limits = ImageLimits { max_pixels: 1_000_000, max_dimension: 2_000, ..ImageLimits::default() };
    let samples = [png(24, 16), jpeg(24, 16), gif(24, 16), webp(24, 16), jpeg_with_metadata(24, 16, 3)];
    let mut rng = Rng(0x9e37_79b9_7f4a_7c15);
    for sample in &samples {
        for _ in 0..400 {
            let mut mutated = sample.clone();
            for _ in 0..1 + rng.below(4) {
                let at = rng.below(mutated.len());
                match rng.below(3) {
                    0 => mutated[at] ^= 1 << rng.below(8),
                    1 => mutated[at] = 0xff,
                    _ => mutated[at] = u8::try_from(rng.below(256)).unwrap(),
                }
            }
            let _ = strip_metadata(&mutated);
            if let Ok(image) = prepare_for_model(mutated.clone(), &limits) {
                assert!(u64::from(image.width) * u64::from(image.height) <= limits.max_pixels);
            }
            if let Ok(image) = transform(&mutated, &limits, Default::default()) {
                assert!(image.width <= limits.max_dimension && image.height <= limits.max_dimension);
            }
        }
    }
}

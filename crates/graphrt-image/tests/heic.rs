//! HEIC input (feature `heic`): a phone photo is checked from its header,
//! decoded in safe Rust and leaves as a JPEG or PNG without its metadata.
//! The fixtures are encoded at test time, so no photo is kept in the repository.
#![cfg(feature = "heic")]
#![allow(clippy::unwrap_used, clippy::indexing_slicing, clippy::panic)]

mod support;

use std::sync::Arc;

use graphrt_core::tool::{ToolCallContext, ToolRuntime};
use graphrt_files::{Workspace, WorkspaceLimits, WorkspacePath};
use graphrt_image::{
    Change, ImageError, ImageFormat, ImageLimits, ImageTools, Transform, prepare_for_model, probe, strip_metadata,
    transform,
};
use serde_json::json;
use support::{SECRET, contains, exif, rgb, rgba};

fn heic_options() -> heifer::EncodeOptions {
    heifer::EncodeOptions::new().with_quality(80).with_max_threads(1)
}

/// A HEIC photo of the gradient fixture.
fn heic(width: u32, height: u32) -> Vec<u8> {
    heifer::encode_rgb8(width, height, rgb(width, height).to_rgb8().as_raw(), &heic_options()).unwrap()
}

/// The same with EXIF (a GPS position and an orientation tag) and XMP inside.
fn heic_with_metadata(width: u32, height: u32) -> Vec<u8> {
    let image = heifer::decode(&heic(width, height)).unwrap();
    let metadata = heifer::Metadata {
        exif: Some(exif(1)),
        xmp: Some(format!("<x:xmpmeta><gps>{SECRET}</gps></x:xmpmeta>").into_bytes()),
        icc_profile: None,
    };
    heifer::encode_with_metadata(&image, &metadata, &heic_options()).unwrap()
}

/// Write `width` and `height` into the first `ispe` property of a HEIC file.
fn declare_size(mut bytes: Vec<u8>, width: u32, height: u32) -> Vec<u8> {
    let at = bytes.windows(4).position(|window| window == b"ispe").unwrap();
    bytes[at + 8..at + 12].copy_from_slice(&width.to_be_bytes());
    bytes[at + 12..at + 16].copy_from_slice(&height.to_be_bytes());
    bytes
}

#[test]
fn a_heic_photo_is_read_from_its_header_and_leaves_as_a_jpeg() {
    let photo = heic(320, 240);
    assert_eq!(&photo[4..8], b"ftyp");
    let info = probe(&photo).unwrap();
    assert_eq!((info.format, info.width, info.height), (ImageFormat::Heic, 320, 240));
    assert_eq!(info.format.mime(), "image/heic");
    assert!(!info.format.is_writable());

    // Small enough for every provider, and still never passed on as it is.
    let prepared = prepare_for_model(photo.clone(), &ImageLimits::default()).unwrap();
    assert_eq!((prepared.format, prepared.width, prepared.height), (ImageFormat::Jpeg, 320, 240));
    assert_eq!(prepared.change, Change::Reencoded);
    assert!(prepared.to_data_url().starts_with("data:image/jpeg;base64,"));
    // The pixels are the photo: close to the picture it was encoded from.
    let decoded = support::decode(&prepared.bytes).to_rgb8();
    let original = rgb(320, 240).to_rgb8();
    let difference: u64 = decoded.as_raw().iter().zip(original.as_raw()).map(|(a, b)| u64::from(a.abs_diff(*b))).sum();
    // Two lossy codecs sit between the two: a different picture would be off by about 85.
    assert!(difference / u64::try_from(decoded.as_raw().len()).unwrap() < 24, "total error {difference}");

    // Larger than a model takes: downscaled like any other photo. (A small limit
    // stands in for a large photo, which takes long to encode in a test build.)
    let small_model = ImageLimits { model_max_edge: 160, ..ImageLimits::default() };
    let large = prepare_for_model(photo, &small_model).unwrap();
    assert_eq!((large.format, large.width, large.height), (ImageFormat::Jpeg, 160, 120));
    // Transparency survives as PNG.
    let cutout = heifer::encode_rgba8(64, 64, rgba(64, 64).to_rgba8().as_raw(), &heic_options()).unwrap();
    assert_eq!(prepare_for_model(cutout, &ImageLimits::default()).unwrap().format, ImageFormat::Png);
}

#[test]
fn nothing_of_a_heic_container_reaches_a_model() {
    let photo = heic_with_metadata(96, 64);
    assert!(contains(&photo, SECRET.as_bytes()), "the fixture must carry the secret");
    let prepared = prepare_for_model(photo.clone(), &ImageLimits::default()).unwrap();
    assert!(!contains(&prepared.bytes, SECRET.as_bytes()));
    assert!(!contains(&prepared.bytes, b"Exif"));
    assert!(!probe(&prepared.bytes).unwrap().has_metadata);
    let converted = transform(&photo, &ImageLimits::default(), Transform::default()).unwrap();
    assert_eq!(converted.format, ImageFormat::Jpeg);
    assert!(!contains(&converted.bytes, SECRET.as_bytes()));
    // HEIC is never written, and never handed on with only its metadata cut out.
    let asked_for_heic = Transform { format: Some(ImageFormat::Heic), ..Transform::default() };
    assert!(matches!(transform(&photo, &ImageLimits::default(), asked_for_heic), Err(ImageError::Encode(_))));
    assert!(strip_metadata(&photo).is_err());
    assert_eq!(ImageFormat::from_name("heic"), None);
}

#[test]
fn a_huge_heic_canvas_is_refused_from_the_header_alone() {
    let bomb = declare_size(heic(64, 64), 60_000, 60_000);
    let started = std::time::Instant::now();
    for _ in 0..1_000 {
        let info = probe(&bomb).unwrap();
        assert_eq!((info.width, info.height), (60_000, 60_000));
        let refused = prepare_for_model(bomb.clone(), &ImageLimits::default()).unwrap_err();
        assert!(matches!(refused, ImageError::TooManyPixels { width: 60_000, height: 60_000, .. }), "{refused:?}");
    }
    // A thousand refusals in the time one decode would take: no decoder ran.
    assert!(started.elapsed() < std::time::Duration::from_millis(500), "took {:?}", started.elapsed());
    // A header that lies small does not get a large picture through either:
    // the decoder has the same limit, and what it returns is checked again.
    let tight = ImageLimits { max_pixels: 10_000, max_dimension: 200, ..ImageLimits::default() };
    let liar = declare_size(heic(160, 120), 16, 16);
    assert!(prepare_for_model(liar, &tight).is_err());
    let byte_limit = ImageLimits { max_input_bytes: 100, ..ImageLimits::default() };
    assert!(matches!(prepare_for_model(heic(64, 64), &byte_limit), Err(ImageError::TooManyBytes { .. })));
}

#[test]
fn truncated_and_corrupted_heic_files_fail_cleanly() {
    let photo = heic(48, 32);
    let limits = ImageLimits::default();
    // Every prefix: an error or, once the picture is complete, the picture. Never a panic.
    for length in 0..photo.len() {
        let cut = &photo[..length];
        if let Ok(info) = probe(cut) {
            let _ = transform(cut, &limits, Transform::default());
            assert_eq!(info.format, ImageFormat::Heic);
        }
    }
    let mut random = support::Rng(0x9e37_79b9_7f4a_7c15);
    for _ in 0..300 {
        let mut damaged = photo.clone();
        for _ in 0..1 + random.below(4) {
            let at = random.below(damaged.len());
            damaged[at] ^= 1 << random.below(8);
        }
        if let Ok(image) = prepare_for_model(damaged, &limits) {
            assert!(u64::from(image.width) * u64::from(image.height) <= limits.max_pixels);
        }
    }
}

#[test]
fn only_hevc_coded_files_count_as_heic() {
    let photo = heic(32, 32);
    // The same container announced as AVIF is not decoded.
    let mut as_avif = photo.clone();
    let brand = as_avif.windows(4).position(|window| window == b"heic").unwrap();
    as_avif[brand..brand + 4].copy_from_slice(b"avif");
    let refused = probe(&as_avif).unwrap_err();
    assert!(matches!(&refused, ImageError::UnsupportedFormat(message) if message.contains("AVIF")), "{refused:?}");
    // An `ftyp` box alone, or one with an absurd size, is not an image.
    for stub in
        [&b"\0\0\0\x18ftypheic\0\0\0\0mif1heic"[..], b"\xff\xff\xff\xffftypheic\0\0\0\0mif1heic", b"\0\0\0\x08ftyp"]
    {
        assert!(probe(stub).is_err());
    }
    // A program with a HEIC name is still judged by its bytes.
    assert!(matches!(probe(b"MZ\x90\0\x03\0\0\0"), Err(ImageError::UnsupportedFormat(_))));
}

#[tokio::test]
async fn the_image_tools_take_heic_and_write_jpeg() {
    let workspace = Workspace::in_memory(WorkspaceLimits::default());
    let path = |raw: &str| WorkspacePath::parse(raw).unwrap();
    workspace.write(&path("photos/IMG_0001.HEIC"), heic_with_metadata(160, 120), false).await.unwrap();
    let tools = Arc::new(ImageTools::new(workspace.clone()));
    let call = |name: &'static str, arguments: serde_json::Value| {
        let tools = Arc::clone(&tools);
        async move { tools.call(name, arguments, ToolCallContext::empty()).await.unwrap() }
    };
    let info = call("info", json!({"path": "photos/IMG_0001.HEIC"})).await;
    assert!(!info.is_error && info.text.contains("image/heic image, 160x120 pixels"), "{}", info.text);
    let view = call("view", json!({"path": "photos/IMG_0001.HEIC"})).await;
    assert!(!view.is_error, "{}", view.text);
    assert_eq!(view.images.len(), 1);
    let converted = call("convert", json!({"path": "photos/IMG_0001.HEIC", "format": "jpeg"})).await;
    assert!(!converted.is_error, "{}", converted.text);
    let jpeg = workspace.read_all(&path("photos/IMG_0001.jpg"), 10_000_000).await.unwrap();
    assert_eq!(probe(&jpeg).unwrap().format, ImageFormat::Jpeg);
    assert!(!contains(&jpeg, SECRET.as_bytes()));
    let thumbnail = call("thumbnail", json!({"path": "photos/IMG_0001.HEIC"})).await;
    assert!(!thumbnail.is_error && thumbnail.text.contains("photos/IMG_0001-thumb.jpg"), "{}", thumbnail.text);
    // The tools cannot be asked to write HEIC.
    let refused = call("convert", json!({"path": "photos/IMG_0001.jpg", "format": "heic"})).await;
    assert!(refused.is_error, "{}", refused.text);
}

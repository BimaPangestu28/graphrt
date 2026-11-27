//! Preparing images for a model, and the `VisionPreparer` in front of model nodes.

#![cfg(feature = "process")]
#![allow(clippy::unwrap_used)]

mod support;

use std::sync::Arc;

use graphrt_core::file::{ChainResolver, FileError, FileResolver, ImagePreparer, base64_encode};
use graphrt_core::value::{File, FileKind, FileTransfer};
use graphrt_image::{
    Change, ImageError, ImageFormat, ImageLimits, VisionPreparer, prepare_for_model, probe, transform,
};
use support::{decode, encode, jpeg, jpeg_with_metadata, png, rgb, rgba};

fn data_url_file(name: &str, mime: &str, bytes: &[u8]) -> File {
    File {
        marker: true,
        name: name.to_owned(),
        kind: FileKind::Image,
        transfer: FileTransfer::Inline,
        url: Some(format!("data:{mime};base64,{}", base64_encode(bytes))),
        mime_type: Some(mime.to_owned()),
        ..File::default()
    }
}

fn preparer(limits: ImageLimits) -> VisionPreparer {
    let files: Arc<dyn FileResolver> = Arc::new(ChainResolver::with_data_urls());
    VisionPreparer::new(files, limits)
}

fn decode_data_url(url: &str) -> (String, Vec<u8>) {
    let content = graphrt_core::file::decode_data_url("image", url, u64::MAX).unwrap();
    (content.mime_type.unwrap(), content.bytes)
}

#[test]
fn a_small_clean_image_is_sent_as_it_is() {
    let original = png(200, 100);
    let prepared = prepare_for_model(original.clone(), &ImageLimits::default()).unwrap();
    assert_eq!(
        (prepared.change, prepared.format, prepared.width, prepared.height),
        (Change::Unchanged, ImageFormat::Png, 200, 100)
    );
    assert_eq!(prepared.bytes, original);
    assert!(prepared.to_data_url().starts_with("data:image/png;base64,iVBOR"));
}

#[test]
fn a_large_photo_is_downscaled_to_the_long_edge_limit() {
    let limits = ImageLimits::default();
    for (width, height, expected) in
        [(4_000, 3_000, (1_568, 1_176)), (3_000, 4_000, (1_176, 1_568)), (1_569, 10, (1_568, 10))]
    {
        let prepared = prepare_for_model(jpeg(width, height), &limits).unwrap();
        assert_eq!((prepared.width, prepared.height), expected);
        assert_eq!((prepared.change, prepared.format), (Change::Reencoded, ImageFormat::Jpeg));
        let info = probe(&prepared.bytes).unwrap();
        assert_eq!((info.width, info.height), expected);
        assert!(u64::try_from(prepared.bytes.len()).unwrap() <= limits.model_max_bytes);
    }
    // Exactly at the limit: untouched.
    assert_eq!(prepare_for_model(png(1_568, 4), &limits).unwrap().change, Change::Unchanged);
}

/// A phone stores a portrait photo as landscape pixels plus "rotate me". With
/// the metadata gone the picture would lie on its side, so the pixels are turned.
#[test]
fn a_rotated_photo_is_turned_upright_before_its_metadata_goes() {
    // Orientation 6: the stored image must be rotated 90 degrees clockwise.
    let prepared = prepare_for_model(jpeg_with_metadata(60, 40, 6), &ImageLimits::default()).unwrap();
    assert_eq!((prepared.change, prepared.width, prepared.height), (Change::Reencoded, 40, 60));
    assert_eq!(probe(&prepared.bytes).unwrap().orientation, None);
    // Upright photos keep their pixels.
    let upright = prepare_for_model(jpeg_with_metadata(60, 40, 1), &ImageLimits::default()).unwrap();
    assert_eq!((upright.change, upright.width, upright.height), (Change::MetadataRemoved, 60, 40));
}

#[test]
fn transparency_survives_when_it_fits_and_becomes_white_when_it_does_not() {
    let transparent = encode(&rgba(2_000, 1_000), image::ImageFormat::Png);
    let limits = ImageLimits::default();
    let prepared = prepare_for_model(transparent.clone(), &limits).unwrap();
    assert_eq!((prepared.format, prepared.width, prepared.height), (ImageFormat::Png, 1_568, 784));
    assert!(decode(&prepared.bytes).color().has_alpha());

    // A byte limit the PNG cannot meet and a JPEG can: JPEG over white.
    let png_size = u64::try_from(prepared.bytes.len()).unwrap();
    let as_jpeg =
        graphrt_image::Transform { max_width: Some(1_568), max_height: Some(1_568), format: Some(ImageFormat::Jpeg) };
    let jpeg_size = u64::try_from(transform(&transparent, &limits, as_jpeg).unwrap().bytes.len()).unwrap();
    assert!(jpeg_size < png_size, "the fixture must be smaller as JPEG ({jpeg_size} vs {png_size})");
    let tight = ImageLimits { model_max_bytes: jpeg_size, ..ImageLimits::default() };
    let prepared = prepare_for_model(transparent, &tight).unwrap();
    assert_eq!(prepared.format, ImageFormat::Jpeg);
    assert!(u64::try_from(prepared.bytes.len()).unwrap() <= tight.model_max_bytes, "{}", prepared.bytes.len());
    assert!(prepared.width <= 1_568);
    // Transparent pixels are white, not black.
    let pixel = decode(&prepared.bytes).to_rgb8().get_pixel(prepared.width * 3 / 4, prepared.height / 2).0;
    assert!(pixel.iter().all(|channel| *channel > 240), "{pixel:?}");

    let impossible = ImageLimits { model_max_bytes: 50, ..ImageLimits::default() };
    assert!(matches!(prepare_for_model(jpeg(2_000, 1_000), &impossible), Err(ImageError::Encode(_))));
}

#[test]
fn transforms_fit_convert_and_never_enlarge() {
    let limits = ImageLimits::default();
    let source = jpeg(800, 600);
    let fit = |max_width, max_height, format| {
        transform(&source, &limits, graphrt_image::Transform { max_width, max_height, format }).unwrap()
    };
    let small = fit(Some(200), None, None);
    assert_eq!((small.width, small.height, small.format), (200, 150, ImageFormat::Jpeg));
    let boxed = fit(Some(300), Some(100), Some(ImageFormat::Png));
    assert_eq!((boxed.width, boxed.height, boxed.format), (134, 100, ImageFormat::Png));
    let same = fit(Some(5_000), Some(5_000), Some(ImageFormat::WebP));
    assert_eq!((same.width, same.height, same.format), (800, 600, ImageFormat::WebP));
    for format in [ImageFormat::Png, ImageFormat::Jpeg, ImageFormat::WebP, ImageFormat::Gif] {
        let converted = fit(Some(64), Some(64), Some(format));
        let info = probe(&converted.bytes).unwrap();
        assert_eq!((info.format, info.width, info.height), (format, 64, 48));
    }
    // 16-bit pixels come out as 8-bit.
    let deep = encode(&image::DynamicImage::ImageRgb16(rgb(32, 32).to_rgb16()), image::ImageFormat::Png);
    let flat = transform(&deep, &limits, Default::default()).unwrap();
    assert_eq!(decode(&flat.bytes).color(), image::ColorType::Rgb8);
}

#[tokio::test]
async fn vision_inputs_are_checked_and_prepared() {
    let preparer = preparer(ImageLimits::default());
    // A large inline photo with metadata: downscaled, metadata gone.
    let photo = data_url_file("photo.jpg", "image/jpeg", &jpeg_with_metadata(2_400, 1_800, 1));
    let (mime, bytes) = decode_data_url(&preparer.prepare(&photo).await.unwrap());
    assert_eq!(mime, "image/jpeg");
    let info = probe(&bytes).unwrap();
    assert_eq!((info.width, info.height, info.has_metadata), (1_568, 1_176, false));
    assert!(!support::contains(&bytes, support::SECRET.as_bytes()));

    // The media type sent to the model is what the bytes are, not what was claimed.
    let mislabelled = data_url_file("photo.png", "image/png", &jpeg(64, 64));
    assert!(preparer.prepare(&mislabelled).await.unwrap().starts_with("data:image/jpeg;base64,"));
}

/// The declared type is a claim. An SVG or an HTML page sent as `image/png`,
/// a HEIC photo, a program: none of them reaches a model.
#[tokio::test]
async fn spoofed_vision_inputs_are_refused() {
    let preparer = preparer(ImageLimits::default());
    let cases: [(&str, &[u8]); 4] = [
        ("evil.png", b"<svg xmlns='http://www.w3.org/2000/svg'><script>alert(1)</script></svg>"),
        ("page.png", b"<!DOCTYPE html><html><script>x()</script>"),
        ("photo.png", b"\0\0\0\x18ftypheic\0\0\0\0mif1heic"),
        ("tool.png", b"MZ\x90\0\x03\0\0\0"),
    ];
    for (name, bytes) in cases {
        let file = data_url_file(name, "image/png", bytes);
        let refused = preparer.prepare(&file).await.unwrap_err();
        assert!(matches!(refused, FileError::Invalid { .. }), "{name}: {refused:?}");
    }
    let bomb = data_url_file("bomb.png", "image/png", &support::black_png(6_000, 6_000));
    let refused = preparer.prepare(&bomb).await.unwrap_err();
    assert!(refused.to_string().contains("6000x6000"), "{refused}");
    let tight = self::preparer(ImageLimits { max_input_bytes: 100, ..ImageLimits::default() });
    let big = data_url_file("big.png", "image/png", &png(64, 64));
    assert!(matches!(tight.prepare(&big).await, Err(FileError::TooLarge { limit: 100, .. })));
}

/// Remote images: by default the provider fetches them (the runtime makes no
/// request, so there is nothing to forge). When the host asks for inlining,
/// the fetch goes through the resolver, which is where the egress policy lives.
#[tokio::test]
async fn remote_images_are_left_to_the_provider_unless_inlined() {
    let remote = |url: &str| File {
        marker: true,
        name: "cat.png".into(),
        kind: FileKind::Image,
        url: Some(url.to_owned()),
        ..File::default()
    };
    let plain = preparer(ImageLimits::default());
    for url in ["https://example.com/cat.png", "HTTP://169.254.169.254/latest/meta-data", "http://localhost/x.png"] {
        assert!(matches!(plain.prepare(&remote(url)).await, Err(FileError::Unsupported(_))), "{url}");
    }
    // With inlining and no resolver that may fetch URLs, the image is refused: never fetched some other way.
    let inlining = preparer(ImageLimits::default()).inline_remote_images(true);
    let refused = inlining.prepare(&remote("http://169.254.169.254/latest/meta-data")).await;
    assert!(matches!(refused, Err(FileError::Unsupported(_))), "{refused:?}");
}

/// Decoding and resizing a large photo must not hold the async worker.
#[tokio::test(flavor = "current_thread")]
async fn preparing_a_large_image_does_not_block_the_async_worker() {
    let preparer = preparer(ImageLimits::default());
    let photo = data_url_file("photo.jpg", "image/jpeg", &jpeg(4_000, 3_000));
    let ticks = Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let counter = Arc::clone(&ticks);
    let ticker = tokio::spawn(async move {
        loop {
            tokio::time::sleep(std::time::Duration::from_millis(1)).await;
            counter.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        }
    });
    let started = std::time::Instant::now();
    while started.elapsed() < std::time::Duration::from_millis(300) {
        preparer.prepare(&photo).await.unwrap();
    }
    let busy_ms = usize::try_from(started.elapsed().as_millis()).unwrap();
    ticker.abort();
    let fired = ticks.load(std::sync::atomic::Ordering::Relaxed);
    // A free worker fires the 1 ms timer all the time (every 16 ms on Windows).
    // A worker that decodes 12 megapixels itself only reaches the timer between images.
    assert!(fired >= busy_ms / 50, "the timer fired {fired} times during {busy_ms} ms of image work");
}

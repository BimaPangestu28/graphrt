//! Cost of the image pipeline, for the numbers in docs/files-and-images-design.md.
//!
//! ```sh
//! cargo run --release -p graphrt-image --example bench
//! ```

#![allow(clippy::unwrap_used, clippy::print_stdout, clippy::cast_precision_loss, clippy::indexing_slicing)]

use std::io::Cursor;
use std::time::Instant;

use graphrt_image::{ImageFormat, ImageLimits, Transform, prepare_for_model, probe, strip_metadata, transform};
use image::{DynamicImage, Rgb, RgbImage};

/// A picture with photo-like detail: smooth gradients plus fine texture.
fn photo(width: u32, height: u32) -> DynamicImage {
    DynamicImage::ImageRgb8(RgbImage::from_fn(width, height, |x, y| {
        let texture = (x.wrapping_mul(2_654_435_761) ^ y.wrapping_mul(40_503)) >> 27;
        let channel = |value: u32| u8::try_from((value + texture) % 256).unwrap();
        Rgb([channel(x / 8), channel(y / 8), channel((x + y) / 16)])
    }))
}

fn encode(image: &DynamicImage, format: image::ImageFormat) -> Vec<u8> {
    let mut out = Cursor::new(Vec::new());
    image.write_to(&mut out, format).unwrap();
    out.into_inner()
}

/// Median of `runs` timings of `work`, in milliseconds.
fn median<T>(runs: usize, mut work: impl FnMut() -> T) -> f64 {
    let mut samples = Vec::with_capacity(runs);
    for _ in 0..runs {
        let started = Instant::now();
        std::hint::black_box(work());
        samples.push(started.elapsed().as_secs_f64() * 1e3);
    }
    samples.sort_by(f64::total_cmp);
    samples[samples.len() / 2]
}

fn mib(bytes: usize) -> String {
    format!("{:.2} MiB", bytes as f64 / (1024.0 * 1024.0))
}

fn main() {
    let limits = ImageLimits::default();
    println!("| What | Input | Result |\n| --- | --- | --- |");

    let small = encode(&photo(1_024, 768), image::ImageFormat::Jpeg);
    let phone = encode(&photo(4_000, 3_000), image::ImageFormat::Jpeg);
    let large = encode(&photo(6_000, 4_000), image::ImageFormat::Jpeg);
    let screenshot = encode(&photo(2_560, 1_440), image::ImageFormat::Png);

    let nanos = median(20_000, || probe(&phone).unwrap()) * 1e6;
    println!("| Read the header (format, size, metadata) | 12 MP JPEG, {} | {nanos:.0} ns |", mib(phone.len()));
    let bomb = {
        let image = DynamicImage::ImageLuma8(image::GrayImage::new(8_000, 8_000));
        encode(&image, image::ImageFormat::Png)
    };
    let micros = median(2_000, || prepare_for_model(bomb.clone(), &limits).unwrap_err()) * 1e3;
    println!("| Refuse an image bomb | 64 MP PNG in {} | {micros:.1} µs, nothing decoded |", mib(bomb.len()));

    let ms = median(50, || strip_metadata(&phone).unwrap());
    println!("| Remove metadata, pixels untouched | 12 MP JPEG, {} | {ms:.2} ms |", mib(phone.len()));
    let ms = median(200, || prepare_for_model(small.clone(), &limits).unwrap());
    println!("| Prepare for a model, no resize needed | 1024x768 JPEG, {} | {ms:.2} ms |", mib(small.len()));

    for (name, bytes) in [("12 MP JPEG (4000x3000)", &phone), ("24 MP JPEG (6000x4000)", &large)] {
        let prepared = prepare_for_model(bytes.clone(), &limits).unwrap();
        let ms = median(7, || prepare_for_model(bytes.clone(), &limits).unwrap());
        println!(
            "| Prepare for a model: decode, downscale to {}x{}, encode | {name}, {} | {ms:.0} ms, sends {} instead |",
            prepared.width,
            prepared.height,
            mib(bytes.len()),
            mib(prepared.bytes.len())
        );
    }
    let prepared = prepare_for_model(screenshot.clone(), &limits).unwrap();
    let ms = median(7, || prepare_for_model(screenshot.clone(), &limits).unwrap());
    println!(
        "| Prepare for a model | 2560x1440 PNG, {} | {ms:.0} ms, sends {} |",
        mib(screenshot.len()),
        mib(prepared.bytes.len())
    );

    let thumb = Transform { max_width: Some(256), max_height: Some(256), format: Some(ImageFormat::Jpeg) };
    let ms = median(7, || transform(&phone, &limits, thumb).unwrap());
    println!("| Thumbnail, 256 px | 12 MP JPEG | {ms:.0} ms |");
    let to_webp = Transform { format: Some(ImageFormat::WebP), ..Transform::default() };
    let ms = median(7, || transform(&small, &limits, to_webp).unwrap());
    println!("| Convert to WebP | 1024x768 JPEG | {ms:.0} ms |");
}

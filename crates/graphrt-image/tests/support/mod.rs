//! Image fixtures built in code (each test binary uses a subset).

#![allow(dead_code, clippy::unwrap_used, clippy::indexing_slicing)]

use std::io::Cursor;

use image::{DynamicImage, ImageFormat, Rgb, RgbImage, Rgba, RgbaImage};

/// Text planted in metadata. If it survives, metadata leaked.
pub const SECRET: &str = "GPS-37.4219N-122.0840W-SECRET";

/// A photo-like RGB picture: a gradient with some structure, so JPEG has work to do.
pub fn rgb(width: u32, height: u32) -> DynamicImage {
    let image = RgbImage::from_fn(width, height, |x, y| {
        let wave = ((x * 7 + y * 13) % 64) as u8;
        Rgb([(x % 256) as u8, (y % 256) as u8, wave.wrapping_mul(3)])
    });
    DynamicImage::ImageRgb8(image)
}

/// A picture whose right half is fully transparent and whose left half is
/// noise, which PNG cannot compress and JPEG can.
pub fn rgba(width: u32, height: u32) -> DynamicImage {
    let image = RgbaImage::from_fn(width, height, |x, y| {
        if x >= width / 2 {
            return Rgba([0, 0, 0, 0]);
        }
        let noise = (x.wrapping_mul(2_654_435_761) ^ y.wrapping_mul(40_503)).wrapping_mul(2_246_822_519);
        Rgba([(noise >> 8) as u8, (noise >> 16) as u8, (noise >> 24) as u8, 255])
    });
    DynamicImage::ImageRgba8(image)
}

pub fn encode(image: &DynamicImage, format: ImageFormat) -> Vec<u8> {
    let mut out = Cursor::new(Vec::new());
    image.write_to(&mut out, format).unwrap();
    out.into_inner()
}

pub fn png(width: u32, height: u32) -> Vec<u8> {
    encode(&rgb(width, height), ImageFormat::Png)
}

pub fn jpeg(width: u32, height: u32) -> Vec<u8> {
    encode(&rgb(width, height), ImageFormat::Jpeg)
}

pub fn gif(width: u32, height: u32) -> Vec<u8> {
    encode(&rgb(width, height), ImageFormat::Gif)
}

pub fn webp(width: u32, height: u32) -> Vec<u8> {
    encode(&rgb(width, height), ImageFormat::WebP)
}

pub fn decode(bytes: &[u8]) -> DynamicImage {
    image::load_from_memory(bytes).unwrap()
}

pub fn contains(haystack: &[u8], needle: &[u8]) -> bool {
    haystack.windows(needle.len()).any(|window| window == needle)
}

fn jpeg_segment(marker: u8, data: &[u8]) -> Vec<u8> {
    let mut segment = vec![0xff, marker];
    segment.extend_from_slice(&u16::try_from(data.len() + 2).unwrap().to_be_bytes());
    segment.extend_from_slice(data);
    segment
}

/// An EXIF block (big endian TIFF) with the orientation tag and a text tag holding [`SECRET`].
pub fn exif(orientation: u16) -> Vec<u8> {
    let mut tiff = b"MM\0\x2a\0\0\0\x08".to_vec();
    tiff.extend_from_slice(&2u16.to_be_bytes()); // two entries
    // Orientation (0x0112), SHORT, count 1.
    tiff.extend_from_slice(&[0x01, 0x12, 0, 3, 0, 0, 0, 1]);
    tiff.extend_from_slice(&orientation.to_be_bytes());
    tiff.extend_from_slice(&[0, 0]);
    // ImageDescription (0x010E), ASCII, value after the directory.
    let text = SECRET.as_bytes();
    tiff.extend_from_slice(&[0x01, 0x0e, 0, 2]);
    tiff.extend_from_slice(&u32::try_from(text.len()).unwrap().to_be_bytes());
    tiff.extend_from_slice(&38u32.to_be_bytes()); // 8 + 2 + 24 + 4
    tiff.extend_from_slice(&[0, 0, 0, 0]); // no next directory
    tiff.extend_from_slice(text);
    let mut block = b"Exif\0\0".to_vec();
    block.extend_from_slice(&tiff);
    block
}

/// A JPEG carrying EXIF (with `orientation`), XMP, IPTC and a comment, all holding
/// [`SECRET`], an ICC-like APP2 segment, and a payload after the end of the image.
pub fn jpeg_with_metadata(width: u32, height: u32, orientation: u16) -> Vec<u8> {
    let plain = jpeg(width, height);
    let mut out = plain[..2].to_vec();
    out.extend(jpeg_segment(0xe1, &exif(orientation)));
    out.extend(jpeg_segment(0xe1, format!("http://ns.adobe.com/xap/1.0/\0<x:xmpmeta>{SECRET}</x:xmpmeta>").as_bytes()));
    out.extend(jpeg_segment(0xed, format!("Photoshop 3.0\08BIM{SECRET}").as_bytes()));
    out.extend(jpeg_segment(0xfe, SECRET.as_bytes()));
    out.extend(jpeg_segment(0xe2, b"ICC_PROFILE\0\x01\x01keep-this-profile"));
    out.extend_from_slice(&plain[2..]);
    out.extend_from_slice(format!("<script>{SECRET}</script>").as_bytes());
    out
}

fn crc32(bytes: &[u8]) -> u32 {
    let mut crc = 0xffff_ffffu32;
    for byte in bytes {
        crc ^= u32::from(*byte);
        for _ in 0..8 {
            crc = if crc & 1 == 1 { (crc >> 1) ^ 0xedb8_8320 } else { crc >> 1 };
        }
    }
    !crc
}

pub fn png_chunk(kind: &[u8; 4], data: &[u8]) -> Vec<u8> {
    let mut chunk = u32::try_from(data.len()).unwrap().to_be_bytes().to_vec();
    chunk.extend_from_slice(kind);
    chunk.extend_from_slice(data);
    let mut checked = kind.to_vec();
    checked.extend_from_slice(data);
    chunk.extend_from_slice(&crc32(&checked).to_be_bytes());
    chunk
}

/// A PNG carrying text, international text, EXIF and time chunks holding
/// [`SECRET`], a gamma chunk, and a payload after the end of the image.
pub fn png_with_metadata(width: u32, height: u32) -> Vec<u8> {
    let plain = png(width, height);
    // Signature (8) + IHDR chunk (25).
    let mut out = plain[..33].to_vec();
    out.extend(png_chunk(b"gAMA", &45_455u32.to_be_bytes()));
    out.extend(png_chunk(b"tEXt", format!("Comment\0{SECRET}").as_bytes()));
    out.extend(png_chunk(b"iTXt", format!("XML:com.adobe.xmp\0\0\0\0\0{SECRET}").as_bytes()));
    out.extend(png_chunk(b"eXIf", &exif(1)[6..]));
    out.extend(png_chunk(b"tIME", &[0x07, 0xea, 11, 26, 10, 0, 0]));
    out.extend_from_slice(&plain[33..]);
    out.extend_from_slice(format!("PK\x03\x04{SECRET}").as_bytes());
    out
}

fn riff_chunk(kind: &[u8; 4], data: &[u8]) -> Vec<u8> {
    let mut chunk = kind.to_vec();
    chunk.extend_from_slice(&u32::try_from(data.len()).unwrap().to_le_bytes());
    chunk.extend_from_slice(data);
    if data.len() % 2 == 1 {
        chunk.push(0);
    }
    chunk
}

/// A WebP in the extended layout with EXIF and XMP chunks holding [`SECRET`],
/// their flags set, and a payload after the RIFF container.
pub fn webp_with_metadata(width: u32, height: u32) -> Vec<u8> {
    let plain = webp(width, height);
    let frame = &plain[12..]; // the VP8L chunk
    let mut header = vec![0x08 | 0x04, 0, 0, 0];
    header.extend_from_slice(&(width - 1).to_le_bytes()[..3]);
    header.extend_from_slice(&(height - 1).to_le_bytes()[..3]);
    let mut body = b"WEBP".to_vec();
    body.extend(riff_chunk(b"VP8X", &header));
    body.extend_from_slice(frame);
    body.extend(riff_chunk(b"EXIF", &exif(1)[6..]));
    body.extend(riff_chunk(b"XMP ", format!("<x:xmpmeta>{SECRET}</x:xmpmeta>").as_bytes()));
    let mut out = b"RIFF".to_vec();
    out.extend_from_slice(&u32::try_from(body.len()).unwrap().to_le_bytes());
    out.extend_from_slice(&body);
    out.extend_from_slice(SECRET.as_bytes());
    out
}

/// A GIF with a comment extension and an XMP application extension holding
/// [`SECRET`], a loop count extension, and a payload after the trailer.
pub fn gif_with_metadata(width: u32, height: u32) -> Vec<u8> {
    let plain = gif(width, height);
    let table = if plain[10] & 0x80 == 0 { 0 } else { 3 * (1usize << ((plain[10] & 7) + 1)) };
    let split = 13 + table;
    let sub_blocks = |data: &[u8]| {
        let mut out = Vec::new();
        for part in data.chunks(255) {
            out.push(u8::try_from(part.len()).unwrap());
            out.extend_from_slice(part);
        }
        out.push(0);
        out
    };
    let mut out = plain[..split].to_vec();
    out.extend_from_slice(b"\x21\xff\x0bNETSCAPE2.0\x03\x01\x00\x00\x00");
    out.extend_from_slice(&[0x21, 0xfe]);
    out.extend(sub_blocks(SECRET.as_bytes()));
    out.extend_from_slice(b"\x21\xff\x0bXMP DataXMP");
    out.extend(sub_blocks(format!("<x:xmpmeta>{SECRET}</x:xmpmeta>").as_bytes()));
    out.extend_from_slice(&plain[split..]);
    out.extend_from_slice(SECRET.as_bytes());
    out
}

/// A PNG that is only a header: it declares `width`x`height` and holds no pixel data.
pub fn png_header_only(width: u32, height: u32) -> Vec<u8> {
    let mut out = b"\x89PNG\r\n\x1a\n".to_vec();
    let mut header = width.to_be_bytes().to_vec();
    header.extend_from_slice(&height.to_be_bytes());
    header.extend_from_slice(&[8, 2, 0, 0, 0]);
    out.extend(png_chunk(b"IHDR", &header));
    out
}

/// A complete, valid PNG of `width`x`height` black pixels. It is tiny on disk
/// (a run of zeros compresses about 1000 to 1) and large once decoded: the
/// shape of a decompression bomb.
pub fn black_png(width: u32, height: u32) -> Vec<u8> {
    encode(&DynamicImage::ImageLuma8(image::GrayImage::new(width, height)), ImageFormat::Png)
}

/// A tiny deterministic pseudo random generator for mutation tests.
pub struct Rng(pub u64);

impl Rng {
    pub fn next(&mut self) -> u64 {
        self.0 ^= self.0 << 13;
        self.0 ^= self.0 >> 7;
        self.0 ^= self.0 << 17;
        self.0
    }

    pub fn below(&mut self, bound: usize) -> usize {
        usize::try_from(self.next() % u64::try_from(bound.max(1)).unwrap()).unwrap()
    }
}

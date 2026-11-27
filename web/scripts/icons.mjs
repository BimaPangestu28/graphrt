// The icons of the monitoring page, drawn here so the repository holds no
// picture nobody can regenerate: a rounded square in the accent colour with a
// white ring and dot (something being watched).
import { deflateSync, inflateSync } from "node:zlib";

const ACCENT = [0x25, 0x7d, 0xe0];
const WHITE = [0xff, 0xff, 0xff];
/** Samples per pixel side, for smooth edges. */
const SAMPLES = 4;

/** Whether the point is inside a square of side 1 with rounded corners of `radius`. */
function inRoundedSquare(x, y, radius) {
  const dx = Math.max(radius - x, x - (1 - radius), 0);
  const dy = Math.max(radius - y, y - (1 - radius), 0);
  return dx * dx + dy * dy <= radius * radius;
}

/**
 * The RGBA pixels of an icon of `size` pixels a side.
 *
 * @param maskable - Fill the whole square and keep the drawing in the middle
 *   80 %: a launcher cuts its own shape out of the picture.
 */
export function iconPixels(size, maskable) {
  const pixels = Buffer.alloc(size * size * 4);
  const scale = maskable ? 0.62 : 0.8;
  for (let py = 0; py < size; py += 1) {
    for (let px = 0; px < size; px += 1) {
      let red = 0;
      let green = 0;
      let blue = 0;
      let alpha = 0;
      for (let sy = 0; sy < SAMPLES; sy += 1) {
        for (let sx = 0; sx < SAMPLES; sx += 1) {
          const x = (px + (sx + 0.5) / SAMPLES) / size;
          const y = (py + (sy + 0.5) / SAMPLES) / size;
          if (!maskable && !inRoundedSquare(x, y, 0.22)) {
            continue;
          }
          const distance = Math.hypot(x - 0.5, y - 0.5) / scale;
          const white = (distance >= 0.3 && distance <= 0.4) || distance <= 0.14;
          const colour = white ? WHITE : ACCENT;
          red += colour[0];
          green += colour[1];
          blue += colour[2];
          alpha += 1;
        }
      }
      const offset = (py * size + px) * 4;
      if (alpha > 0) {
        pixels[offset] = Math.round(red / alpha);
        pixels[offset + 1] = Math.round(green / alpha);
        pixels[offset + 2] = Math.round(blue / alpha);
        pixels[offset + 3] = Math.round((alpha * 255) / (SAMPLES * SAMPLES));
      }
    }
  }
  return pixels;
}

const CRC_TABLE = Array.from({ length: 256 }, (_, n) => {
  let c = n;
  for (let k = 0; k < 8; k += 1) {
    c = c & 1 ? 0xedb88320 ^ (c >>> 1) : c >>> 1;
  }
  return c >>> 0;
});

function crc32(bytes) {
  let crc = 0xffffffff;
  for (const byte of bytes) {
    crc = CRC_TABLE[(crc ^ byte) & 0xff] ^ (crc >>> 8);
  }
  return (crc ^ 0xffffffff) >>> 0;
}

function chunk(type, data) {
  const head = Buffer.alloc(8);
  head.writeUInt32BE(data.length, 0);
  head.write(type, 4, "ascii");
  const tail = Buffer.alloc(4);
  tail.writeUInt32BE(crc32(Buffer.concat([head.subarray(4), data])), 0);
  return Buffer.concat([head, data, tail]);
}

const SIGNATURE = Buffer.from([0x89, 0x50, 0x4e, 0x47, 0x0d, 0x0a, 0x1a, 0x0a]);

/** The scanlines of a PNG: each row of pixels after a filter byte of 0. */
function scanlines(pixels, size) {
  const rows = Buffer.alloc(size * (size * 4 + 1));
  for (let y = 0; y < size; y += 1) {
    pixels.copy(rows, y * (size * 4 + 1) + 1, y * size * 4, (y + 1) * size * 4);
  }
  return rows;
}

/** RGBA `pixels` of `size` a side as a PNG file. */
export function encodePng(pixels, size) {
  const header = Buffer.alloc(13);
  header.writeUInt32BE(size, 0);
  header.writeUInt32BE(size, 4);
  // 8 bits a channel, RGBA, no interlace.
  header.set([8, 6, 0, 0, 0], 8);
  return Buffer.concat([
    SIGNATURE,
    chunk("IHDR", header),
    chunk("IDAT", deflateSync(scanlines(pixels, size), { level: 9 })),
    chunk("IEND", Buffer.alloc(0)),
  ]);
}

/**
 * Whether the PNG file `png` holds exactly `pixels`. The compressed bytes
 * depend on the zlib of the Node.js that wrote them, the pixels do not.
 */
export function pngHolds(png, pixels, size) {
  if (png.length < 8 || !png.subarray(0, 8).equals(SIGNATURE)) {
    return false;
  }
  const data = [];
  let width = 0;
  for (let offset = 8; offset + 8 <= png.length; ) {
    const length = png.readUInt32BE(offset);
    const type = png.toString("ascii", offset + 4, offset + 8);
    const body = png.subarray(offset + 8, offset + 8 + length);
    if (type === "IHDR") {
      width = body.readUInt32BE(0);
    } else if (type === "IDAT") {
      data.push(body);
    }
    offset += 12 + length;
  }
  try {
    return width === size && inflateSync(Buffer.concat(data)).equals(scanlines(pixels, size));
  } catch {
    return false;
  }
}

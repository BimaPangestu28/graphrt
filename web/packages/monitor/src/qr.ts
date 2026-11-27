/**
 * A QR code encoder, for the pairing link of a device: bytes in, a square of
 * dark and light modules out. Byte mode, error correction level M (about 15 %
 * of the code may be damaged), versions 1 to 40.
 *
 * The page draws the code itself, so a pairing token never goes to a service
 * that makes QR pictures.
 */

/** A QR code: `size` modules a side, `dark[y][x]` for each. */
export interface QrCode {
  size: number;
  /** 1 (21 modules a side) to 40 (177). */
  version: number;
  /** The mask pattern in use, 0 to 7. */
  mask: number;
  dark: boolean[][];
}

export interface QrOptions {
  /** A mask pattern, 0 to 7. Default: the one with the lowest penalty. */
  mask?: number;
  /** The smallest version to use. Default: the smallest that fits. */
  minVersion?: number;
}

// Level M: error correction codewords per block, and the number of blocks, by version.
const ECC_PER_BLOCK = [
  -1, 10, 16, 26, 18, 24, 16, 18, 22, 22, 26, 30, 22, 22, 24, 24, 28, 28, 26, 26, 26, 26, 28, 28, 28, 28, 28,
  28, 28, 28, 28, 28, 28, 28, 28, 28, 28, 28, 28, 28, 28,
];
const BLOCKS = [
  -1, 1, 1, 1, 2, 2, 4, 4, 4, 5, 5, 5, 8, 9, 9, 10, 10, 11, 13, 14, 16, 17, 17, 18, 20, 21, 23, 25, 26, 28,
  29, 31, 33, 35, 37, 38, 40, 43, 45, 47, 49,
];
/** The format bits of level M. */
const LEVEL_M = 0;
const MAX_VERSION = 40;

function at<T>(list: readonly T[], index: number): T {
  const value = list[index];
  if (value === undefined) {
    throw new RangeError(`index ${index} out of range`);
  }
  return value;
}

/** The modules of a version that are not part of a function pattern. */
function rawDataModules(version: number): number {
  let result = (16 * version + 128) * version + 64;
  if (version >= 2) {
    const alignments = Math.floor(version / 7) + 2;
    result -= (25 * alignments - 10) * alignments - 55;
    if (version >= 7) {
      result -= 36;
    }
  }
  return result;
}

function dataCodewords(version: number): number {
  return Math.floor(rawDataModules(version) / 8) - at(ECC_PER_BLOCK, version) * at(BLOCKS, version);
}

/** How many bytes a code of `version` holds. */
export function qrCapacity(version: number): number {
  // The mode (4 bits) and the byte count (8 or 16 bits) come first.
  const headerBits = 4 + (version <= 9 ? 8 : 16);
  return Math.floor((dataCodewords(version) * 8 - headerBits) / 8);
}

/** Multiplication in GF(2^8) modulo x^8 + x^4 + x^3 + x^2 + 1. */
function multiply(x: number, y: number): number {
  let z = 0;
  for (let i = 7; i >= 0; i -= 1) {
    z = (z << 1) ^ ((z >>> 7) * 0x11d);
    z ^= ((y >>> i) & 1) * x;
  }
  return z & 0xff;
}

/** The Reed-Solomon generator polynomial of `degree`, without its leading 1. */
function rsDivisor(degree: number): number[] {
  const result: number[] = new Array<number>(degree).fill(0);
  result[degree - 1] = 1;
  let root = 1;
  for (let i = 0; i < degree; i += 1) {
    for (let j = 0; j < degree; j += 1) {
      result[j] = multiply(at(result, j), root);
      if (j + 1 < degree) {
        result[j] = at(result, j) ^ at(result, j + 1);
      }
    }
    root = multiply(root, 2);
  }
  return result;
}

function rsRemainder(data: readonly number[], divisor: readonly number[]): number[] {
  const result: number[] = new Array<number>(divisor.length).fill(0);
  for (const byte of data) {
    const factor = byte ^ (result.shift() ?? 0);
    result.push(0);
    divisor.forEach((coefficient, i) => {
      result[i] = at(result, i) ^ multiply(coefficient, factor);
    });
  }
  return result;
}

/** The data codewords of `bytes` for `version`: header, bytes, terminator and padding. */
function encodeData(bytes: Uint8Array, version: number): number[] {
  const bits: number[] = [];
  const push = (value: number, length: number): void => {
    for (let i = length - 1; i >= 0; i -= 1) {
      bits.push((value >>> i) & 1);
    }
  };
  push(4, 4);
  push(bytes.length, version <= 9 ? 8 : 16);
  for (const byte of bytes) {
    push(byte, 8);
  }
  const capacity = dataCodewords(version) * 8;
  push(0, Math.min(4, capacity - bits.length));
  push(0, (8 - (bits.length % 8)) % 8);
  for (let pad = 0xec; bits.length < capacity; pad ^= 0xec ^ 0x11) {
    push(pad, 8);
  }
  const codewords: number[] = [];
  for (let i = 0; i < bits.length; i += 8) {
    let byte = 0;
    for (let j = 0; j < 8; j += 1) {
      byte = (byte << 1) | at(bits, i + j);
    }
    codewords.push(byte);
  }
  return codewords;
}

/** The data split into blocks, each with its error correction, interleaved. */
function withErrorCorrection(data: readonly number[], version: number): number[] {
  const blockCount = at(BLOCKS, version);
  const eccLength = at(ECC_PER_BLOCK, version);
  const total = Math.floor(rawDataModules(version) / 8);
  const shortBlocks = blockCount - (total % blockCount);
  const shortLength = Math.floor(total / blockCount);
  const divisor = rsDivisor(eccLength);
  const blocks: number[][] = [];
  for (let i = 0, k = 0; i < blockCount; i += 1) {
    const length = shortLength - eccLength + (i < shortBlocks ? 0 : 1);
    const block = data.slice(k, k + length);
    k += length;
    const ecc = rsRemainder(block, divisor);
    if (i < shortBlocks) {
      // A placeholder, skipped below, so every block has the same length.
      block.push(0);
    }
    blocks.push(block.concat(ecc));
  }
  const result: number[] = [];
  const longest = at(blocks, 0).length;
  for (let i = 0; i < longest; i += 1) {
    blocks.forEach((block, j) => {
      if (i !== shortLength - eccLength || j >= shortBlocks) {
        result.push(at(block, i));
      }
    });
  }
  return result;
}

const MASKS: readonly ((x: number, y: number) => boolean)[] = [
  (x, y) => (x + y) % 2 === 0,
  (_x, y) => y % 2 === 0,
  (x) => x % 3 === 0,
  (x, y) => (x + y) % 3 === 0,
  (x, y) => (Math.floor(x / 3) + Math.floor(y / 2)) % 2 === 0,
  (x, y) => ((x * y) % 2) + ((x * y) % 3) === 0,
  (x, y) => (((x * y) % 2) + ((x * y) % 3)) % 2 === 0,
  (x, y) => (((x + y) % 2) + ((x * y) % 3)) % 2 === 0,
];

/** A grid being filled: the modules, and which of them belong to function patterns. */
class Grid {
  readonly version: number;
  readonly size: number;
  readonly dark: boolean[][];
  readonly fixed: boolean[][];

  constructor(version: number) {
    this.version = version;
    this.size = version * 4 + 17;
    this.dark = Array.from({ length: this.size }, () => new Array<boolean>(this.size).fill(false));
    this.fixed = Array.from({ length: this.size }, () => new Array<boolean>(this.size).fill(false));
  }

  get(x: number, y: number): boolean {
    return at(at(this.dark, y), x);
  }

  isFixed(x: number, y: number): boolean {
    return at(at(this.fixed, y), x);
  }

  put(x: number, y: number, dark: boolean): void {
    at(this.dark, y)[x] = dark;
  }

  /** Set a module of a function pattern. */
  fix(x: number, y: number, dark: boolean): void {
    this.put(x, y, dark);
    at(this.fixed, y)[x] = true;
  }

  drawFunctionPatterns(): void {
    for (let i = 0; i < this.size; i += 1) {
      this.fix(6, i, i % 2 === 0);
      this.fix(i, 6, i % 2 === 0);
    }
    this.#finder(3, 3);
    this.#finder(this.size - 4, 3);
    this.#finder(3, this.size - 4);
    const positions = this.#alignmentPositions();
    const last = positions.length - 1;
    positions.forEach((x, i) => {
      positions.forEach((y, j) => {
        // Not where the three finder patterns are.
        if (!((i === 0 && j === 0) || (i === 0 && j === last) || (i === last && j === 0))) {
          this.#alignment(x, y);
        }
      });
    });
    // Reserved now, written once the mask is chosen.
    this.drawFormat(0);
    this.#drawVersion();
  }

  #finder(centerX: number, centerY: number): void {
    for (let dy = -4; dy <= 4; dy += 1) {
      for (let dx = -4; dx <= 4; dx += 1) {
        const distance = Math.max(Math.abs(dx), Math.abs(dy));
        const x = centerX + dx;
        const y = centerY + dy;
        if (x >= 0 && x < this.size && y >= 0 && y < this.size) {
          this.fix(x, y, distance !== 2 && distance !== 4);
        }
      }
    }
  }

  #alignment(centerX: number, centerY: number): void {
    for (let dy = -2; dy <= 2; dy += 1) {
      for (let dx = -2; dx <= 2; dx += 1) {
        this.fix(centerX + dx, centerY + dy, Math.max(Math.abs(dx), Math.abs(dy)) !== 1);
      }
    }
  }

  #alignmentPositions(): number[] {
    if (this.version === 1) {
      return [];
    }
    const count = Math.floor(this.version / 7) + 2;
    const step = this.version === 32 ? 26 : Math.ceil((this.version * 4 + 4) / (count * 2 - 2)) * 2;
    const result = [6];
    for (let position = this.size - 7; result.length < count; position -= step) {
      result.splice(1, 0, position);
    }
    return result;
  }

  /** The 15 format bits (level and mask, with their error correction), twice. */
  drawFormat(mask: number): void {
    const data = (LEVEL_M << 3) | mask;
    let remainder = data;
    for (let i = 0; i < 10; i += 1) {
      remainder = (remainder << 1) ^ ((remainder >>> 9) * 0x537);
    }
    const bits = ((data << 10) | remainder) ^ 0x5412;
    const bit = (i: number): boolean => ((bits >>> i) & 1) !== 0;
    for (let i = 0; i <= 5; i += 1) {
      this.fix(8, i, bit(i));
    }
    this.fix(8, 7, bit(6));
    this.fix(8, 8, bit(7));
    this.fix(7, 8, bit(8));
    for (let i = 9; i < 15; i += 1) {
      this.fix(14 - i, 8, bit(i));
    }
    for (let i = 0; i < 8; i += 1) {
      this.fix(this.size - 1 - i, 8, bit(i));
    }
    for (let i = 8; i < 15; i += 1) {
      this.fix(8, this.size - 15 + i, bit(i));
    }
    // Always dark.
    this.fix(8, this.size - 8, true);
  }

  /** The 18 version bits, twice, from version 7 on. */
  #drawVersion(): void {
    if (this.version < 7) {
      return;
    }
    let remainder = this.version;
    for (let i = 0; i < 12; i += 1) {
      remainder = (remainder << 1) ^ ((remainder >>> 11) * 0x1f25);
    }
    const bits = (this.version << 12) | remainder;
    for (let i = 0; i < 18; i += 1) {
      const dark = ((bits >>> i) & 1) !== 0;
      const a = this.size - 11 + (i % 3);
      const b = Math.floor(i / 3);
      this.fix(a, b, dark);
      this.fix(b, a, dark);
    }
  }

  /** The codewords in the zigzag from the bottom right, around the function patterns. */
  drawCodewords(codewords: readonly number[]): void {
    let i = 0;
    for (let right = this.size - 1; right >= 1; right -= 2) {
      if (right === 6) {
        right = 5;
      }
      for (let vertical = 0; vertical < this.size; vertical += 1) {
        for (let j = 0; j < 2; j += 1) {
          const x = right - j;
          const upward = ((right + 1) & 2) === 0;
          const y = upward ? this.size - 1 - vertical : vertical;
          if (!this.isFixed(x, y) && i < codewords.length * 8) {
            this.put(x, y, ((at(codewords, i >>> 3) >>> (7 - (i & 7))) & 1) !== 0);
            i += 1;
          }
        }
      }
    }
  }

  /** Flip the data modules the mask covers. Applying it twice undoes it. */
  applyMask(mask: number): void {
    const covers = at(MASKS, mask);
    for (let y = 0; y < this.size; y += 1) {
      for (let x = 0; x < this.size; x += 1) {
        if (!this.isFixed(x, y) && covers(x, y)) {
          this.put(x, y, !this.get(x, y));
        }
      }
    }
  }

  /** How hard the code is to read: runs, blocks, finder look-alikes and imbalance. */
  penalty(): number {
    let result = 0;
    let darkCount = 0;
    const lines: boolean[][] = [];
    for (let i = 0; i < this.size; i += 1) {
      const row: boolean[] = [];
      const column: boolean[] = [];
      for (let j = 0; j < this.size; j += 1) {
        row.push(this.get(j, i));
        column.push(this.get(i, j));
      }
      darkCount += row.filter(Boolean).length;
      lines.push(row, column);
    }
    const lookAlike = [true, false, true, true, true, false, true];
    for (const line of lines) {
      for (let start = 0; start < line.length; ) {
        let end = start + 1;
        while (end < line.length && line[end] === line[start]) {
          end += 1;
        }
        if (end - start >= 5) {
          result += 3 + (end - start - 5);
        }
        start = end;
      }
      for (let start = 0; start + 7 <= line.length; start += 1) {
        if (!lookAlike.every((dark, k) => line[start + k] === dark)) {
          continue;
        }
        const lightBefore = start >= 4 && line.slice(start - 4, start).every((dark) => !dark);
        const lightAfter =
          start + 11 <= line.length && line.slice(start + 7, start + 11).every((dark) => !dark);
        if (lightBefore || lightAfter) {
          result += 40;
        }
      }
    }
    for (let y = 0; y < this.size - 1; y += 1) {
      for (let x = 0; x < this.size - 1; x += 1) {
        const dark = this.get(x, y);
        if (dark === this.get(x + 1, y) && dark === this.get(x, y + 1) && dark === this.get(x + 1, y + 1)) {
          result += 3;
        }
      }
    }
    const total = this.size * this.size;
    result += (Math.ceil(Math.abs(darkCount * 20 - total * 10) / total) - 1) * 10;
    return result;
  }
}

/**
 * The QR code of `data` (text is encoded as UTF-8).
 *
 * @throws RangeError - When `data` is longer than the largest code holds (2331 bytes), or an option is out of range.
 *
 * @example
 * const code = encodeQr("https://agent.example.com/monitor/#pair=grd1.dev-1.secret");
 * code.dark[0][0]; // true: the corner of a finder pattern
 */
export function encodeQr(data: string | Uint8Array, options: QrOptions = {}): QrCode {
  const bytes = typeof data === "string" ? new TextEncoder().encode(data) : data;
  const minVersion = options.minVersion ?? 1;
  if (!Number.isInteger(minVersion) || minVersion < 1 || minVersion > MAX_VERSION) {
    throw new RangeError("minVersion must be 1 to 40");
  }
  if (
    options.mask !== undefined &&
    (!Number.isInteger(options.mask) || options.mask < 0 || options.mask > 7)
  ) {
    throw new RangeError("mask must be 0 to 7");
  }
  let version = minVersion;
  while (qrCapacity(version) < bytes.length) {
    version += 1;
    if (version > MAX_VERSION) {
      throw new RangeError(`${bytes.length} bytes do not fit a QR code`);
    }
  }
  const grid = new Grid(version);
  grid.drawFunctionPatterns();
  grid.drawCodewords(withErrorCorrection(encodeData(bytes, version), version));
  let mask = options.mask;
  if (mask === undefined) {
    let lowest = Number.POSITIVE_INFINITY;
    for (let candidate = 0; candidate < 8; candidate += 1) {
      grid.applyMask(candidate);
      grid.drawFormat(candidate);
      const penalty = grid.penalty();
      if (penalty < lowest) {
        lowest = penalty;
        mask = candidate;
      }
      grid.applyMask(candidate);
    }
  }
  const chosen = mask ?? 0;
  grid.applyMask(chosen);
  grid.drawFormat(chosen);
  return { size: grid.size, version, mask: chosen, dark: grid.dark };
}

const SVG_NS = "http://www.w3.org/2000/svg";

/**
 * `code` as an SVG element with a quiet zone of four modules, dark on white
 * in every theme: scanners expect that.
 *
 * @param label - What the picture is, for people who cannot see it.
 */
export function qrSvg(code: QrCode, label: string): SVGSVGElement {
  const quiet = 4;
  const side = code.size + quiet * 2;
  const svg = document.createElementNS(SVG_NS, "svg");
  svg.setAttribute("viewBox", `0 0 ${side} ${side}`);
  svg.setAttribute("role", "img");
  svg.setAttribute("aria-label", label);
  svg.setAttribute("shape-rendering", "crispEdges");
  const background = document.createElementNS(SVG_NS, "rect");
  background.setAttribute("width", String(side));
  background.setAttribute("height", String(side));
  background.setAttribute("fill", "#ffffff");
  let path = "";
  code.dark.forEach((row, y) => {
    row.forEach((dark, x) => {
      if (dark) {
        path += `M${x + quiet} ${y + quiet}h1v1h-1z`;
      }
    });
  });
  const modules = document.createElementNS(SVG_NS, "path");
  modules.setAttribute("d", path);
  modules.setAttribute("fill", "#000000");
  svg.append(background, modules);
  return svg;
}

/**
 * The QR encoder against a reference implementation (`qrcode`, a development
 * dependency of the tests only): the same modules for every version and mask.
 */
import assert from "node:assert/strict";
import { test } from "node:test";
import qrcode, { type QRCodeMaskPattern } from "qrcode";
import { encodeQr, qrCapacity } from "../../src/monitor/qr.ts";

/** The reference code of `bytes`: byte mode, level M, this version and mask. */
function reference(bytes: Uint8Array, version: number, mask: number): boolean[][] {
  const code = qrcode.create([{ data: bytes, mode: "byte" }], {
    errorCorrectionLevel: "M",
    version,
    maskPattern: mask as QRCodeMaskPattern,
  });
  const size = code.modules.size;
  const rows: boolean[][] = [];
  for (let y = 0; y < size; y += 1) {
    const row: boolean[] = [];
    for (let x = 0; x < size; x += 1) {
      row.push(code.modules.data[y * size + x] === 1);
    }
    rows.push(row);
  }
  return rows;
}

function sample(length: number, seed: number): Uint8Array {
  const bytes = new Uint8Array(length);
  let state = seed;
  for (let i = 0; i < length; i += 1) {
    state = (state * 1103515245 + 12345) & 0x7fffffff;
    bytes[i] = state & 0xff;
  }
  return bytes;
}

test("every version matches the reference, full and nearly empty", () => {
  for (let version = 1; version <= 40; version += 1) {
    for (const length of [1, qrCapacity(version)]) {
      const bytes = sample(length, version * 31 + length);
      const mask = version % 8;
      const code = encodeQr(bytes, { minVersion: version, mask });
      assert.equal(code.version, version, `version ${version}, ${length} bytes`);
      assert.equal(code.size, version * 4 + 17);
      assert.deepEqual(code.dark, reference(bytes, version, mask), `version ${version}, ${length} bytes`);
    }
  }
});

test("every mask matches the reference", () => {
  const bytes = new TextEncoder().encode(
    "https://agent.example.com/monitor/#pair=grd1.dev-0011223344556677.c2VjcmV0",
  );
  for (let mask = 0; mask < 8; mask += 1) {
    const code = encodeQr(bytes, { mask });
    assert.equal(code.mask, mask);
    assert.deepEqual(code.dark, reference(bytes, code.version, mask), `mask ${mask}`);
  }
});

test("the smallest version that fits is used, and a mask is chosen", () => {
  assert.equal(qrCapacity(1), 14);
  assert.equal(qrCapacity(10), 213);
  assert.equal(qrCapacity(40), 2331);
  assert.equal(encodeQr("x".repeat(14)).version, 1);
  assert.equal(encodeQr("x".repeat(15)).version, 2);
  const text = "héllo, wörld";
  const code = encodeQr(text);
  assert.ok(code.mask >= 0 && code.mask <= 7);
  // Text is UTF-8, and whichever mask was chosen, the code is the reference's with it.
  assert.deepEqual(code.dark, reference(new TextEncoder().encode(text), code.version, code.mask));
});

test("what cannot be encoded is refused", () => {
  assert.throws(() => encodeQr(new Uint8Array(2332)), RangeError);
  assert.throws(() => encodeQr("x", { mask: 8 }), RangeError);
  assert.throws(() => encodeQr("x", { minVersion: 41 }), RangeError);
});

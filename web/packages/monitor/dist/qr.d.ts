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
/** How many bytes a code of `version` holds. */
export declare function qrCapacity(version: number): number;
/**
 * The QR code of `data` (text is encoded as UTF-8).
 *
 * @throws RangeError - When `data` is longer than the largest code holds (2331 bytes), or an option is out of range.
 *
 * @example
 * const code = encodeQr("https://agent.example.com/monitor/#pair=grd1.dev-1.secret");
 * code.dark[0][0]; // true: the corner of a finder pattern
 */
export declare function encodeQr(data: string | Uint8Array, options?: QrOptions): QrCode;
/**
 * `code` as an SVG element with a quiet zone of four modules, dark on white
 * in every theme: scanners expect that.
 *
 * @param label - What the picture is, for people who cannot see it.
 */
export declare function qrSvg(code: QrCode, label: string): SVGSVGElement;

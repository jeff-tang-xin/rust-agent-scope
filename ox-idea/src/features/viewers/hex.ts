/** Hex view model: fixed 16-byte rows over pages read from the backend on demand. */

export const BYTES_PER_ROW = 16;
/** Bytes per backend read (4096 rows). */
export const PAGE_SIZE = 64 * 1024;

export function decodeBase64(data: string): Uint8Array {
  const bin = atob(data);
  const out = new Uint8Array(bin.length);
  for (let i = 0; i < bin.length; i++) out[i] = bin.charCodeAt(i);
  return out;
}

/** Hex digits for offsets: at least 8, more for files past 4 GiB. */
export function offsetDigits(total: number): number {
  return Math.max(8, Math.max(0, total - 1).toString(16).length);
}

export const formatOffset = (offset: number, digits: number) => offset.toString(16).toUpperCase().padStart(digits, "0");

export const hexByte = (b: number) => b.toString(16).toUpperCase().padStart(2, "0");

/** Printable ASCII as is, everything else as a dot (like IDEA / xxd). */
export const asciiChar = (b: number) => (b >= 0x20 && b < 0x7f ? String.fromCharCode(b) : ".");

export const rowCount = (total: number) => Math.ceil(total / BYTES_PER_ROW);

export const pageOf = (offset: number) => Math.floor(offset / PAGE_SIZE);

/**
 * Bytes of row `row` from the loaded pages, or null while its page is
 * missing. Rows never straddle pages (PAGE_SIZE is a multiple of the row size).
 */
export function rowBytes(pages: ReadonlyMap<number, Uint8Array>, row: number, total: number): Uint8Array | null {
  const start = row * BYTES_PER_ROW;
  const page = pages.get(pageOf(start));
  if (!page) return null;
  const from = start - pageOf(start) * PAGE_SIZE;
  return page.subarray(from, Math.min(from + BYTES_PER_ROW, from + total - start));
}

/** Pages needed to show rows [first, last]. */
export function pagesForRows(first: number, last: number): number[] {
  const a = pageOf(Math.max(0, first) * BYTES_PER_ROW);
  const b = pageOf(Math.max(0, last) * BYTES_PER_ROW);
  const out: number[] = [];
  for (let p = a; p <= b; p++) out.push(p);
  return out;
}

/**
 * Browsers cap element heights (~16–33M px), far below a 512 MB file's rows,
 * so the scroll range is capped and scaled onto the rows.
 */
export const MAX_SCROLL_PX = 8_000_000;

export interface HexViewport {
  /** Height of the scrolled content. */
  spacer: number;
  /** First (partly) visible row and its offset above the viewport's top. */
  first: number;
  shift: number;
}

export function hexViewport(
  scrollTop: number,
  viewH: number,
  rows: number,
  rowH: number,
  maxPx = MAX_SCROLL_PX,
): HexViewport {
  const full = rows * rowH;
  const spacer = Math.min(full, maxPx);
  const range = Math.max(0, spacer - viewH);
  const virtualRange = Math.max(0, full - viewH);
  const top = range > 0 ? (Math.min(Math.max(0, scrollTop), range) / range) * virtualRange : 0;
  const first = Math.floor(top / rowH);
  return { spacer, first, shift: top - first * rowH };
}

/** `scrollTop` that puts `row` at the top of the viewport. */
export function scrollTopForRow(row: number, viewH: number, rows: number, rowH: number, maxPx = MAX_SCROLL_PX): number {
  const full = rows * rowH;
  const range = Math.max(0, Math.min(full, maxPx) - viewH);
  const virtualRange = Math.max(0, full - viewH);
  if (!virtualRange) return 0;
  return (Math.min(row * rowH, virtualRange) / virtualRange) * range;
}

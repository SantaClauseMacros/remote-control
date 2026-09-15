// A tiny, purpose-built decoder for exactly one type: rc-protocol's
// `relay::Hello` enum, as encoded by Rust's `postcard` crate:
//
//   enum Hello {
//       Host   { device_id: String, key: Option<String> },   // variant 0
//       Client { device_id: String, key: Option<String> },   // variant 1
//       Query  { device_id: String, key: Option<String> },   // variant 2
//   }
//
// postcard wire format used here: an enum variant index is a ULEB128 varint;
// a String is a ULEB128 length followed by raw UTF-8 bytes; an Option<T> is
// one tag byte (0 = None, 1 = Some) followed by T when Some. This is NOT a
// general postcard decoder — just enough of one for this fixed shape, kept
// in lockstep with crates/protocol/src/lib.rs.

export type Hello =
  | { kind: "host"; deviceId: string; key: string | null }
  | { kind: "client"; deviceId: string; key: string | null }
  | { kind: "query"; deviceId: string; key: string | null };

class Cursor {
  constructor(
    public buf: Uint8Array,
    public i: number = 0,
  ) {}
}

function readVarint(c: Cursor): number {
  let result = 0;
  let shift = 0;
  for (;;) {
    if (c.i >= c.buf.length) throw new Error("varint ran past end of buffer");
    const b = c.buf[c.i++];
    result |= (b & 0x7f) << shift;
    if ((b & 0x80) === 0) break;
    shift += 7;
  }
  return result >>> 0;
}

function readString(c: Cursor): string {
  const len = readVarint(c);
  if (c.i + len > c.buf.length) throw new Error("string ran past end of buffer");
  const bytes = c.buf.subarray(c.i, c.i + len);
  c.i += len;
  return new TextDecoder().decode(bytes);
}

function readOptionString(c: Cursor): string | null {
  if (c.i >= c.buf.length) throw new Error("option tag ran past end of buffer");
  const tag = c.buf[c.i++];
  if (tag === 0) return null;
  return readString(c);
}

/** Decode a `Hello`, or `null` if the bytes don't parse as one. */
export function decodeHello(bytes: Uint8Array): Hello | null {
  try {
    const c = new Cursor(bytes);
    const variant = readVarint(c);
    const deviceId = readString(c);
    const key = readOptionString(c);
    switch (variant) {
      case 0:
        return { kind: "host", deviceId, key };
      case 1:
        return { kind: "client", deviceId, key };
      case 2:
        return { kind: "query", deviceId, key };
      default:
        return null;
    }
  } catch {
    return null;
  }
}

export const ACK_OK = 1;
export const ACK_HOST_OFFLINE = 2;
export const ACK_BAD_KEY = 3;
export const ACK_BUSY = 4;

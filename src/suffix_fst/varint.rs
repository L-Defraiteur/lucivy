//! LEB128 varints, shared by the sidecar encoders.
//!
//! The sidecars store quantities that are small once delta-encoded — a
//! document gap, a position step, a token length — in fixed 32-bit fields.
//! Writing them as varints is what makes `.word_sfxpost` (WSP3) and
//! `.sibling_v3` (SIB2) 2.5-3x smaller, which is what a query pays for: the
//! working set of a common query is dominated by these files
//! (`lucivy_core/tests/test_touched_bytes.rs`).
//!
//! Encoding a `u32` value through the `u64` functions produces the same bytes
//! as a `u32`-only encoder would, so files written by either round-trip.

/// Append `v` as a LEB128 varint.
pub fn write_varint(buf: &mut Vec<u8>, mut v: u64) {
    while v >= 0x80 {
        buf.push((v as u8) | 0x80);
        v >>= 7;
    }
    buf.push(v as u8);
}

/// Read a LEB128 varint at `*pos`, advancing it.
///
/// `None` on a truncated or over-long encoding: a corrupt or truncated file
/// must not loop and must not panic — sidecars are read from disk, from OPFS
/// and from snapshots written by other processes.
pub fn read_varint(data: &[u8], pos: &mut usize) -> Option<u64> {
    let mut v: u64 = 0;
    for shift in [0u32, 7, 14, 21, 28, 35, 42, 49, 56, 63] {
        let b = *data.get(*pos)?;
        *pos += 1;
        v |= ((b & 0x7f) as u64).checked_shl(shift)?;
        if b & 0x80 == 0 {
            return Some(v);
        }
    }
    None
}

/// Read a varint that encodes a `u32`. A value that does not fit is a corrupt
/// file, not a wider integer: `None` rather than a silent truncation.
pub fn read_varint_u32(data: &[u8], pos: &mut usize) -> Option<u32> {
    u32::try_from(read_varint(data, pos)?).ok()
}

/// Bytes `v` takes as a varint.
pub fn varint_len(mut v: u64) -> usize {
    let mut n = 1;
    v >>= 7;
    while v > 0 {
        n += 1;
        v >>= 7;
    }
    n
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn roundtrip_edges() {
        let values: Vec<u64> = vec![
            0, 1, 127, 128, 129, 255, 256, 16383, 16384, 2097151, 2097152,
            268435455, 268435456, u32::MAX as u64 - 1, u32::MAX as u64,
            u32::MAX as u64 + 1, u64::MAX / 2, u64::MAX,
        ];
        let mut buf = Vec::new();
        for v in &values {
            write_varint(&mut buf, *v);
        }
        let mut pos = 0;
        for v in &values {
            assert_eq!(read_varint(&buf, &mut pos), Some(*v), "value {v}");
        }
        assert_eq!(pos, buf.len(), "no byte left over");

        let mut n = 0;
        for v in &values {
            n += varint_len(*v);
        }
        assert_eq!(n, buf.len(), "varint_len must agree with what was written");
    }

    #[test]
    fn truncated_and_overlong_return_none() {
        let mut buf = Vec::new();
        write_varint(&mut buf, u64::MAX);
        // Cut the last byte: the continuation bit is set on every byte left.
        let mut pos = 0;
        assert_eq!(read_varint(&buf[..buf.len() - 1], &mut pos), None);
        // Eleven continuation bytes: longer than any u64 encoding.
        let overlong = vec![0xffu8; 12];
        let mut pos = 0;
        assert_eq!(read_varint(&overlong, &mut pos), None);
        // Empty input.
        let mut pos = 0;
        assert_eq!(read_varint(&[], &mut pos), None);
    }

    #[test]
    fn u32_reader_refuses_a_wider_value() {
        let mut buf = Vec::new();
        write_varint(&mut buf, u32::MAX as u64);
        write_varint(&mut buf, u32::MAX as u64 + 1);
        let mut pos = 0;
        assert_eq!(read_varint_u32(&buf, &mut pos), Some(u32::MAX));
        assert_eq!(read_varint_u32(&buf, &mut pos), None);
    }
}

//! Shared binary serialization helpers for LUCE/LUCID/LUCIDS formats.

/// Write a length-prefixed UTF-8 string.
pub fn write_string(buf: &mut Vec<u8>, s: &str) {
    let bytes = s.as_bytes();
    buf.extend_from_slice(&(bytes.len() as u32).to_le_bytes());
    buf.extend_from_slice(bytes);
}

/// Read a u32 LE at the current position, advancing `pos`.
pub fn read_u32(data: &[u8], pos: &mut usize) -> Result<u32, String> {
    if *pos + 4 > data.len() {
        return Err(format!("truncated at offset {pos}"));
    }
    let bytes: [u8; 4] = data[*pos..*pos + 4]
        .try_into()
        .map_err(|_| "read_u32 slice error")?;
    *pos += 4;
    Ok(u32::from_le_bytes(bytes))
}

/// Read a length-prefixed UTF-8 string at the current position, advancing `pos`.
pub fn read_string(data: &[u8], pos: &mut usize) -> Result<String, String> {
    let len = read_u32(data, pos)? as usize;
    if *pos + len > data.len() {
        return Err(format!("truncated: expected {len} bytes string at offset {}", *pos));
    }
    let s = std::str::from_utf8(&data[*pos..*pos + len])
        .map_err(|e| format!("invalid UTF-8: {e}"))?;
    *pos += len;
    Ok(s.to_string())
}

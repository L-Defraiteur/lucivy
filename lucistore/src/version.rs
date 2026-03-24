//! Version computation for incremental sync.
//!
//! A version is a deterministic hash of a manifest (e.g. meta.json).
//! Uses FNV-1a 128-bit (two 64-bit passes) — no external dependency.

/// Compute a version string from arbitrary bytes (e.g. meta.json content).
///
/// Returns a 32-char hex string (128 bits via two FNV-1a 64-bit passes).
pub fn compute_version_from_bytes(data: &[u8]) -> String {
    let h1 = fnv1a_64(data, 0xcbf29ce484222325);
    let h2 = fnv1a_64(data, 0x6c62272e07bb0142);
    format!("{:016x}{:016x}", h1, h2)
}

fn fnv1a_64(data: &[u8], offset_basis: u64) -> u64 {
    let mut hash = offset_basis;
    for &byte in data {
        hash ^= byte as u64;
        hash = hash.wrapping_mul(0x100000001b3);
    }
    hash
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_deterministic() {
        let v1 = compute_version_from_bytes(b"hello");
        let v2 = compute_version_from_bytes(b"hello");
        assert_eq!(v1, v2);
        assert_eq!(v1.len(), 32);
    }

    #[test]
    fn test_different_input() {
        let v1 = compute_version_from_bytes(b"meta_v1");
        let v2 = compute_version_from_bytes(b"meta_v2");
        assert_ne!(v1, v2);
    }
}

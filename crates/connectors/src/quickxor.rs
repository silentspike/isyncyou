//! QuickXorHash — the content hash OneDrive (personal/consumer) reports for drive
//! items (`file.hashes.quickXorHash`), reimplemented in safe Rust (plan §23).
//!
//! It lets the sync skip/diff by content rather than by size+mtime: a local file
//! whose QuickXorHash equals the stored cloud hash is provably identical, and a
//! same-size in-place edit (which the size heuristic misses) is caught.
//!
//! The algorithm is a 160-bit shift-register XOR (shift = 11 bits/byte) with the
//! message length XORed into the tail, output as standard base64 — matching the
//! string Graph returns. Verified against ground-truth hashes pulled from the
//! live account (see tests).

const WIDTH_IN_BITS: usize = 160;
const SHIFT: usize = 11;
const BITS_IN_LAST_CELL: usize = 32;
const OUT_LEN: usize = 20; // 160 bits

/// Compute the QuickXorHash of `data` and return it base64-encoded (the form
/// OneDrive reports).
pub fn quickxor_base64(data: &[u8]) -> String {
    base64_encode(&quickxor(data))
}

/// Compute the raw 20-byte QuickXorHash of `data`.
pub fn quickxor(data: &[u8]) -> [u8; OUT_LEN] {
    let mut cells = [0u64; 3]; // ceil(160/64) cells; last holds 32 valid bits
    let mut vector_array_index = 0usize;
    let mut vector_offset = 0usize;
    let iterations = data.len().min(WIDTH_IN_BITS);

    for i in 0..iterations {
        let is_last_cell = vector_array_index == cells.len() - 1;
        let bits_in_cell = if is_last_cell { BITS_IN_LAST_CELL } else { 64 };

        if vector_offset <= bits_in_cell - 8 {
            let mut xored = 0u8;
            let mut j = i;
            while j < data.len() {
                xored ^= data[j];
                j += WIDTH_IN_BITS;
            }
            cells[vector_array_index] ^= (xored as u64) << vector_offset;
        } else {
            let index2 = if is_last_cell {
                0
            } else {
                vector_array_index + 1
            };
            let low = bits_in_cell - vector_offset; // in 1..8
            let mut xored = 0u8;
            let mut j = i;
            while j < data.len() {
                xored ^= data[j];
                j += WIDTH_IN_BITS;
            }
            cells[vector_array_index] ^= (xored as u64) << vector_offset;
            cells[index2] ^= (xored as u64) >> low;
        }

        vector_offset += SHIFT;
        while vector_offset >= bits_in_cell {
            vector_array_index = if is_last_cell {
                0
            } else {
                vector_array_index + 1
            };
            vector_offset -= bits_in_cell;
        }
    }

    let mut rgb = [0u8; OUT_LEN];
    // first two cells → 16 bytes
    for (i, cell) in cells.iter().take(2).enumerate() {
        for j in 0..8 {
            rgb[i * 8 + j] = (cell >> (8 * j)) as u8;
        }
    }
    // last cell → its 32 valid bits = 4 bytes (16..20)
    for j in 0..(BITS_IN_LAST_CELL / 8) {
        rgb[16 + j] = (cells[2] >> (8 * j)) as u8;
    }
    // XOR the message length (u64 LE) into the tail (bytes 12..20)
    let len_le = (data.len() as u64).to_le_bytes();
    for (i, b) in len_le.iter().enumerate() {
        rgb[(OUT_LEN - 8) + i] ^= b;
    }
    rgb
}

/// Standard base64 (with `=` padding), to match Graph's `quickXorHash` string.
fn base64_encode(data: &[u8]) -> String {
    const ALPHABET: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut out = String::with_capacity(data.len().div_ceil(3) * 4);
    for chunk in data.chunks(3) {
        let b = [
            chunk[0],
            *chunk.get(1).unwrap_or(&0),
            *chunk.get(2).unwrap_or(&0),
        ];
        let n = ((b[0] as u32) << 16) | ((b[1] as u32) << 8) | b[2] as u32;
        out.push(ALPHABET[(n >> 18 & 63) as usize] as char);
        out.push(ALPHABET[(n >> 12 & 63) as usize] as char);
        out.push(if chunk.len() > 1 {
            ALPHABET[(n >> 6 & 63) as usize] as char
        } else {
            '='
        });
        out.push(if chunk.len() > 2 {
            ALPHABET[(n & 63) as usize] as char
        } else {
            '='
        });
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Ground-truth hashes pulled from the live OneDrive account: each was
    /// uploaded, its `file.hashes.quickXorHash` read back, then deleted.
    #[test]
    fn matches_onedrive_ground_truth() {
        assert_eq!(quickxor_base64(b""), "AAAAAAAAAAAAAAAAAAAAAAAAAAA=");
        assert_eq!(quickxor_base64(b"hello"), "aCgDG9jwBgAAAAAABQAAAAAAAAA=");
        assert_eq!(
            quickxor_base64(b"abcdefghij"),
            "YRDDGMhQBjOcAQ1pWgMAAAAAAAA="
        );
        // 512 bytes (0..256 twice) — exercises the cell-wrap + multi-spread paths
        let big: Vec<u8> = (0..=255u8).chain(0..=255u8).collect();
        assert_eq!(quickxor_base64(&big), "edJlP68QDhntUYpkxf/vpP5uDuY=");
    }

    #[test]
    fn base64_encode_basics() {
        assert_eq!(base64_encode(b""), "");
        assert_eq!(base64_encode(b"f"), "Zg==");
        assert_eq!(base64_encode(b"fo"), "Zm8=");
        assert_eq!(base64_encode(b"foo"), "Zm9v");
        assert_eq!(base64_encode(b"foobar"), "Zm9vYmFy");
    }
}

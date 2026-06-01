//! OneDrive resumable upload-session chunk planning.
//!
//! Graph requires every fragment except the last to be a multiple of 320 KiB and
//! `< 60 MiB`, sent sequentially with a `Content-Range` header (and **no**
//! `Authorization` header on the `uploadUrl`). A `202` response reports
//! `nextExpectedRanges`, which we use to resume after an interruption.
//!
//! This type is pure: it plans chunks and tracks the offset. The actual HTTP PUT
//! lives in the client layer.

/// 320 KiB — the required fragment alignment.
pub const CHUNK_ALIGN: u64 = 320 * 1024;
/// Largest 320 KiB-multiple that is still `< 60 MiB`.
pub const MAX_FRAGMENT: u64 = 191 * CHUNK_ALIGN; // 62_586_880 (~59.7 MiB)

/// A planned chunk: byte range `start..=end` and the matching `Content-Range`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ChunkPlan {
    pub start: u64,
    pub end: u64, // inclusive
    pub len: u64,
    pub content_range: String,
}

/// Tracks progress of one resumable upload session.
#[derive(Debug, Clone)]
pub struct UploadSession {
    pub upload_url: String,
    pub total: u64,
    next_offset: u64,
}

impl UploadSession {
    pub fn new(upload_url: impl Into<String>, total: u64) -> Self {
        UploadSession {
            upload_url: upload_url.into(),
            total,
            next_offset: 0,
        }
    }

    pub fn next_offset(&self) -> u64 {
        self.next_offset
    }

    pub fn is_complete(&self) -> bool {
        self.next_offset >= self.total
    }

    /// Plan the next chunk to send, given a desired maximum chunk size. Returns
    /// `None` once the upload is complete. The size is clamped to [`MAX_FRAGMENT`]
    /// and aligned down to a 320 KiB multiple, except the final chunk which is the
    /// exact remaining bytes.
    pub fn next_chunk(&self, max_chunk: u64) -> Option<ChunkPlan> {
        if self.is_complete() {
            return None;
        }
        let remaining = self.total - self.next_offset;
        let len = aligned_chunk(max_chunk, remaining);
        let start = self.next_offset;
        let end = start + len - 1;
        Some(ChunkPlan {
            start,
            end,
            len,
            content_range: format!("bytes {start}-{end}/{}", self.total),
        })
    }

    /// Advance the offset after a chunk was accepted.
    pub fn advance(&mut self, sent: u64) {
        self.next_offset = (self.next_offset + sent).min(self.total);
    }

    /// Resume from a `202` response's `nextExpectedRanges` (e.g. `["327680-"]`).
    pub fn apply_next_expected(&mut self, ranges: &[String]) {
        if let Some(off) = ranges.first().and_then(|r| parse_range_start(r)) {
            self.next_offset = off.min(self.total);
        }
    }
}

fn aligned_chunk(max_chunk: u64, remaining: u64) -> u64 {
    let cap = max_chunk.min(MAX_FRAGMENT);
    let mut aligned = (cap / CHUNK_ALIGN) * CHUNK_ALIGN;
    if aligned == 0 {
        aligned = CHUNK_ALIGN; // never plan a zero-length fragment
    }
    // The final chunk is the exact remainder (may be smaller / non-aligned).
    aligned.min(remaining)
}

fn parse_range_start(range: &str) -> Option<u64> {
    range.split('-').next()?.trim().parse().ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn single_chunk_for_small_file() {
        let s = UploadSession::new("https://up", 5 * 1024 * 1024);
        let c = s.next_chunk(10 * 1024 * 1024).unwrap();
        assert_eq!(c.start, 0);
        assert_eq!(c.len, 5 * 1024 * 1024);
        assert_eq!(c.content_range, "bytes 0-5242879/5242880");
    }

    #[test]
    fn large_file_chunks_are_320kib_aligned() {
        let total = 100 * 1024 * 1024;
        let mut s = UploadSession::new("https://up", total);
        let c = s.next_chunk(10 * 1024 * 1024).unwrap();
        assert_eq!(c.len % CHUNK_ALIGN, 0);
        assert_eq!(c.len, 10 * 1024 * 1024);
        assert_eq!(c.content_range, format!("bytes 0-10485759/{total}"));
        s.advance(c.len);
        assert_eq!(s.next_offset(), 10 * 1024 * 1024);
        assert!(!s.is_complete());
    }

    #[test]
    fn max_chunk_below_align_still_sends_one_unit() {
        let s = UploadSession::new("https://up", 100 * 1024 * 1024);
        let c = s.next_chunk(100 * 1024).unwrap(); // below 320 KiB
        assert_eq!(c.len, CHUNK_ALIGN);
    }

    #[test]
    fn max_chunk_is_capped_below_60mib() {
        let s = UploadSession::new("https://up", 1024 * 1024 * 1024);
        let c = s.next_chunk(u64::MAX).unwrap();
        assert_eq!(c.len, MAX_FRAGMENT);
        assert!(c.len < 60 * 1024 * 1024);
        assert_eq!(c.len % CHUNK_ALIGN, 0);
    }

    #[test]
    fn final_chunk_is_exact_remainder() {
        let total = 400_000; // ~390 KiB, not a 320 KiB multiple
        let s = UploadSession::new("https://up", total);
        let c = s.next_chunk(10 * 1024 * 1024).unwrap();
        assert_eq!(c.len, total);
        assert_eq!(c.content_range, "bytes 0-399999/400000");
    }

    #[test]
    fn resume_from_next_expected_ranges() {
        let mut s = UploadSession::new("https://up", 100 * 1024 * 1024);
        s.apply_next_expected(&["10485760-".to_string()]);
        assert_eq!(s.next_offset(), 10 * 1024 * 1024);
        s.apply_next_expected(&["327680-655359".to_string()]);
        assert_eq!(s.next_offset(), 327680);
    }

    #[test]
    fn walks_to_completion() {
        let total = 25 * 1024 * 1024 + 12345;
        let mut s = UploadSession::new("https://up", total);
        let mut guard = 0;
        while let Some(c) = s.next_chunk(8 * 1024 * 1024) {
            assert!(c.len > 0);
            s.advance(c.len);
            guard += 1;
            assert!(guard < 1000, "should terminate");
        }
        assert!(s.is_complete());
        assert_eq!(s.next_offset(), total);
    }
}

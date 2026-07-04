//! Chunk hashing: the state file as an integrity manifest.
//!
//! When hashing is active, the copy loop feeds every byte it reads from the
//! source into a [`ChunkHasher`], which hashes the domain in fixed-size chunks
//! (default 64 MiB). The resulting per-chunk BLAKE3 hashes are stored in the
//! state file's `[hashes]` section, turning it into a manifest of the image:
//! `--verify` re-reads the target and checks it against the manifest — months
//! later and without the (possibly dying) source.
//!
//! The hasher is strictly forward-only: it never re-reads anything. A chunk
//! gets its hash when the copy streams it contiguously from start to end; a
//! discontinuity (resume seam, bad sector) abandons the chunk. Whatever gaps
//! remain for fully-`done` chunks are filled at the end of the run by reading
//! the chunks back from the target (see the engine's gap-fill pass).

/// Default hash chunk size: 64 MiB — fine enough to localise bit-rot, coarse
/// enough that a 1 TB image yields a manifest of ~16k entries (~1 MB TOML).
pub const DEFAULT_CHUNK_SIZE: u64 = 64 * 1024 * 1024;

/// The one supported manifest algorithm (recorded in the state file so a
/// future change stays detectable).
pub const ALGORITHM: &str = "blake3";

/// Hash `data` and return the lowercase hex digest.
#[must_use]
pub fn digest(data: &[u8]) -> String {
    blake3::hash(data).to_hex().to_string()
}

/// Incremental digest over data arriving in pieces — for hashing a chunk read
/// back from the target (gap-fill, verify). Wraps the algorithm so the rest of
/// the crate never touches `blake3` directly.
#[derive(Default)]
pub struct Digester(blake3::Hasher);

impl Digester {
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    pub fn update(&mut self, data: &[u8]) {
        self.0.update(data);
    }

    /// Finish and return the lowercase hex digest.
    #[must_use]
    pub fn finish(self) -> String {
        self.0.finalize().to_hex().to_string()
    }
}

/// Streaming per-chunk hasher over domain offsets (`0..domain`).
///
/// Feed order must be non-decreasing in offset; any jump, and any read error,
/// abandons the chunk under construction (it keeps its prior value, usually
/// empty). Chunk hashes land in a `Vec<String>` indexed by chunk number, with
/// the empty string meaning "not hashed"; the vector round-trips through the
/// state file across resumes.
pub struct ChunkHasher {
    chunk_size: u64,
    /// Domain length, `0` = unknown (tail chunk finalised by [`Self::finish`]).
    domain: u64,
    /// Hex digests by chunk index; `""` = not hashed.
    chunks: Vec<String>,
    active: Option<Active>,
    /// The offset the next feed must start at to be contiguous. Starts as
    /// `u64::MAX` so the first feed only activates on a chunk boundary.
    expected: u64,
}

struct Active {
    index: u64,
    hasher: blake3::Hasher,
}

impl ChunkHasher {
    /// A hasher for a domain of `domain` bytes (`0` = unknown), resuming from
    /// `prior` chunk digests (empty for a fresh run).
    ///
    /// # Panics
    ///
    /// Panics if `chunk_size` is `0` (the caller validates it).
    #[must_use]
    pub fn new(chunk_size: u64, domain: u64, prior: Vec<String>) -> Self {
        assert!(chunk_size > 0, "chunk size must be positive");
        ChunkHasher {
            chunk_size,
            domain,
            chunks: prior,
            active: None,
            expected: u64::MAX,
        }
    }

    /// Feed the source bytes for `[offset, offset + data.len())`.
    pub fn feed(&mut self, offset: u64, data: &[u8]) {
        if data.is_empty() {
            return;
        }
        if offset != self.expected {
            self.active = None;
        }
        self.expected = offset + data.len() as u64;

        let mut offset = offset;
        let mut data = data;
        while !data.is_empty() {
            let index = offset / self.chunk_size;
            let within = offset % self.chunk_size;
            if within == 0 {
                self.active = Some(Active {
                    index,
                    hasher: blake3::Hasher::new(),
                });
            }
            let take = usize::try_from(self.chunk_size - within)
                .unwrap_or(usize::MAX)
                .min(data.len());
            if let Some(active) = &mut self.active {
                if active.index == index {
                    active.hasher.update(&data[..take]);
                }
            }
            offset += take as u64;
            data = &data[take..];
            let chunk_complete = offset % self.chunk_size == 0;
            let domain_end = self.domain > 0 && offset == self.domain;
            if chunk_complete || domain_end {
                self.finalize(index);
            }
        }
    }

    /// Account for an unreadable range `[offset, offset + len)`: the chunk
    /// under construction is abandoned; chunks wholly inside the range simply
    /// never get a hasher.
    pub fn bad(&mut self, offset: u64, len: u64) {
        self.active = None;
        self.expected = offset + len;
    }

    /// Finish an unknown-size domain at end-of-input: the trailing partial
    /// chunk is hashed over its actual length. (A known domain finalises its
    /// tail in [`Self::feed`].)
    pub fn finish(&mut self) {
        if self.domain == 0 {
            if let Some(active) = self.active.take() {
                self.set(active.index, active.hasher.finalize().to_hex().to_string());
            }
        }
    }

    /// The chunk digests accumulated so far (`""` = not hashed).
    #[must_use]
    pub fn chunks(&self) -> &[String] {
        &self.chunks
    }

    /// The digest of chunk `index`, or `None` if it has none (yet).
    #[must_use]
    pub fn get(&self, index: u64) -> Option<&str> {
        usize::try_from(index)
            .ok()
            .and_then(|i| self.chunks.get(i))
            .filter(|digest| !digest.is_empty())
            .map(String::as_str)
    }

    /// The chunk grid size in bytes.
    #[must_use]
    pub fn chunk_size(&self) -> u64 {
        self.chunk_size
    }

    /// Store a digest for `index` (grows the vector as needed) — also used by
    /// the engine's gap-fill pass.
    ///
    /// # Panics
    ///
    /// Panics if `index` does not fit in `usize` (unreachable for real
    /// domains on supported targets).
    pub fn set(&mut self, index: u64, digest: String) {
        let index = usize::try_from(index).expect("chunk index fits in usize");
        if self.chunks.len() <= index {
            self.chunks.resize(index + 1, String::new());
        }
        self.chunks[index] = digest;
    }

    fn finalize(&mut self, index: u64) {
        if let Some(active) = self.active.take_if(|a| a.index == index) {
            self.set(index, active.hasher.finalize().to_hex().to_string());
        }
    }
}

/// The number of chunks a domain of `domain` bytes spans.
#[must_use]
pub fn chunk_count(domain: u64, chunk_size: u64) -> u64 {
    domain.div_ceil(chunk_size)
}

#[cfg(test)]
mod tests {
    use super::{ChunkHasher, chunk_count, digest};

    fn data(len: usize) -> Vec<u8> {
        (0..len).map(|i| u8::try_from(i % 251).unwrap()).collect()
    }

    #[test]
    fn contiguous_feeds_hash_whole_chunks() {
        let bytes = data(1024);
        let mut h = ChunkHasher::new(256, 1024, Vec::new());
        // Feed in odd-sized pieces straddling chunk boundaries.
        h.feed(0, &bytes[0..100]);
        h.feed(100, &bytes[100..600]);
        h.feed(600, &bytes[600..1024]);

        let expect: Vec<String> = bytes.chunks(256).map(digest).collect();
        assert_eq!(h.chunks(), &expect[..]);
    }

    #[test]
    fn tail_chunk_is_hashed_over_its_actual_length() {
        let bytes = data(300);
        let mut h = ChunkHasher::new(256, 300, Vec::new());
        h.feed(0, &bytes);
        assert_eq!(
            h.chunks(),
            &[digest(&bytes[..256]), digest(&bytes[256..])][..]
        );
    }

    #[test]
    fn a_gap_abandons_only_the_straddled_chunks() {
        let bytes = data(1024);
        let mut h = ChunkHasher::new(256, 1024, Vec::new());
        h.feed(0, &bytes[0..256]); // chunk 0 complete
        h.feed(300, &bytes[300..1024]); // jump: chunk 1 unhashable, 2 and 3 fine

        assert_eq!(h.chunks()[0], digest(&bytes[0..256]));
        assert_eq!(h.chunks()[1], "");
        assert_eq!(h.chunks()[2], digest(&bytes[512..768]));
        assert_eq!(h.chunks()[3], digest(&bytes[768..1024]));
    }

    #[test]
    fn a_bad_range_poisons_its_chunks() {
        let bytes = data(1024);
        let mut h = ChunkHasher::new(256, 1024, Vec::new());
        h.feed(0, &bytes[0..200]);
        h.bad(200, 112); // ruins chunk 0 and reaches into chunk 1
        h.feed(312, &bytes[312..1024]);

        assert_eq!(h.chunks()[0], "");
        assert_eq!(h.chunks()[1], "");
        assert_eq!(h.chunks()[2], digest(&bytes[512..768]));
        assert_eq!(h.chunks()[3], digest(&bytes[768..1024]));
    }

    #[test]
    fn a_resume_mid_chunk_stays_unhashed_until_the_next_boundary() {
        let bytes = data(768);
        let mut h = ChunkHasher::new(256, 768, Vec::new());
        // Resume lands mid-chunk 0: no hasher until chunk 1 starts.
        h.feed(128, &bytes[128..768]);
        assert_eq!(h.chunks()[0], "");
        assert_eq!(h.chunks()[1], digest(&bytes[256..512]));
        assert_eq!(h.chunks()[2], digest(&bytes[512..768]));
    }

    #[test]
    fn unknown_domain_finishes_its_tail_explicitly() {
        let bytes = data(300);
        let mut h = ChunkHasher::new(256, 0, Vec::new());
        h.feed(0, &bytes);
        assert_eq!(h.chunks().len(), 1, "tail chunk still open");
        h.finish();
        assert_eq!(h.chunks().len(), 2);
        assert_eq!(h.chunks()[1], digest(&bytes[256..]));
    }

    #[test]
    fn prior_digests_survive_a_resume() {
        let bytes = data(512);
        let prior = vec![digest(&bytes[..256]), String::new()];
        let mut h = ChunkHasher::new(256, 512, prior);
        h.feed(256, &bytes[256..]);
        assert_eq!(
            h.chunks(),
            &[digest(&bytes[..256]), digest(&bytes[256..])][..]
        );
    }

    #[test]
    fn chunk_count_rounds_up() {
        assert_eq!(chunk_count(0, 256), 0);
        assert_eq!(chunk_count(256, 256), 1);
        assert_eq!(chunk_count(257, 256), 2);
    }
}

use crate::VcResult;
use std::io::Read as _;
use std::path::Path;

/// Read buffer size for [`file_hash_io`]. Fixed and small on purpose: the
/// whole point of streaming is that peak memory is this constant, not the
/// file's length. 64 KiB is comfortably above a page and below the point
/// where the copy stops being the cheap part of the loop.
const HASH_CHUNK_BYTES: usize = 64 * 1024;

/// Blake3 hex digest of a file's contents, **streamed** — never more than
/// [`HASH_CHUNK_BYTES`] of the file is resident at once.
///
/// This matters because of *where* it is called from: `index::refresh`
/// hashes every changed file through a rayon `par_iter`, so a whole-file
/// read here costs (largest file × parallelism) of peak RSS on every cold
/// or changed-file refresh — and the index refresh runs BEFORE any search
/// size gate could reject the file. A single 300 MB artifact in the tree
/// was measured at ~318 MB RSS for `vc status`. Streaming makes that cost
/// a constant.
///
/// The digest is unchanged: blake3 over the same byte sequence, chunked or
/// not, is the same hash. `streamed_hash_equals_bytes_hash_across_buffer_
/// boundaries` pins that against a multi-buffer file.
pub fn file_hash(path: &Path) -> VcResult<String> {
    Ok(file_hash_io(path)?)
}

/// [`file_hash`] with the raw `io::Error` preserved.
///
/// `file_hash`'s `?` collapses every I/O failure into `ErrorKind::Io` plus
/// a `Display` string, which loses the `io::ErrorKind` — and one caller
/// (the CLI's scope-drift candidate hashing) has to tell `NotFound`
/// (benign: a deleted file cannot contain a new match) apart from every
/// other read failure (must fail closed). That caller wants the streaming
/// behaviour and the error kind, so it gets both here rather than
/// open-coding a second whole-file read.
pub fn file_hash_io(path: &Path) -> std::io::Result<String> {
    let mut file = std::fs::File::open(path)?;
    let mut hasher = blake3::Hasher::new();
    let mut buf = vec![0u8; HASH_CHUNK_BYTES];
    loop {
        let n = file.read(&mut buf)?;
        if n == 0 {
            break;
        }
        hasher.update(&buf[..n]);
    }
    Ok(hasher.finalize().to_hex().to_string())
}

pub fn bytes_hash(bytes: &[u8]) -> String {
    blake3::hash(bytes).to_hex().to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hashes_file_content_blake3_hex() {
        let d = tempfile::tempdir().unwrap();
        let p = d.path().join("a.txt");
        std::fs::write(&p, b"hello").unwrap();
        let h = file_hash(&p).unwrap();
        assert_eq!(h, blake3::hash(b"hello").to_hex().to_string());
    }

    /// The streaming rewrite's whole obligation: the digest must be
    /// IDENTICAL to hashing the same content in one shot, for a file that
    /// spans several read buffers (so the chunk boundaries are actually
    /// exercised) and whose length is not a multiple of the buffer size
    /// (so the final short read is exercised too).
    #[test]
    fn streamed_hash_equals_bytes_hash_across_buffer_boundaries() {
        let d = tempfile::tempdir().unwrap();
        let p = d.path().join("big.bin");

        // 3.5 buffers' worth: two full chunks, a third full chunk, and a
        // half-chunk tail.
        let len = HASH_CHUNK_BYTES * 3 + HASH_CHUNK_BYTES / 2;
        let content: Vec<u8> = (0..len).map(|i| (i % 251) as u8).collect();
        std::fs::write(&p, &content).unwrap();

        assert_eq!(file_hash(&p).unwrap(), bytes_hash(&content));
    }

    /// An empty file still hashes (the loop's first read returns 0), and
    /// agrees with the one-shot digest of no bytes.
    #[test]
    fn streamed_hash_of_an_empty_file_matches_the_empty_digest() {
        let d = tempfile::tempdir().unwrap();
        let p = d.path().join("empty.bin");
        std::fs::write(&p, b"").unwrap();
        assert_eq!(file_hash(&p).unwrap(), bytes_hash(b""));
    }

    /// `file_hash_io` keeps the `io::ErrorKind` its `VcResult` sibling
    /// throws away — the distinction the scope-drift check reads.
    #[test]
    fn file_hash_io_preserves_the_io_error_kind() {
        let d = tempfile::tempdir().unwrap();
        let e = file_hash_io(&d.path().join("nope.bin")).unwrap_err();
        assert_eq!(e.kind(), std::io::ErrorKind::NotFound);
    }
}

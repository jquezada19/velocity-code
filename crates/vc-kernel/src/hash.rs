use crate::VcResult;
use std::path::Path;

pub fn file_hash(path: &Path) -> VcResult<String> {
    let bytes = std::fs::read(path)?;
    Ok(blake3::hash(&bytes).to_hex().to_string())
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
}

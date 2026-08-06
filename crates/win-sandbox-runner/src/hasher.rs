use anyhow::{Context, Result};
use sha2::{Digest, Sha256};
use std::fs::File;
use std::io::{BufReader, Read};
use std::path::Path;

/// Size of the read buffer for streaming hash.
const CHUNK_SIZE: usize = 8192;

/// Compute the SHA-256 hash of a file, returning the hex-encoded digest.
pub fn hash_file(path: &str) -> Result<String> {
    let path = Path::new(path);
    let file = File::open(path)
        .with_context(|| format!("Failed to open file for hashing: {}", path.display()))?;
    let mut reader = BufReader::new(file);
    let mut hasher = Sha256::new();
    let mut buf = vec![0u8; CHUNK_SIZE];

    loop {
        let n = reader.read(&mut buf)?;
        if n == 0 {
            break;
        }
        hasher.update(&buf[..n]);
    }

    Ok(format!("{:x}", hasher.finalize()))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    #[test]
    fn hash_known_empty_string() {
        // SHA-256 of empty string
        let expected = "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855";
        let tmp = tempfile::NamedTempFile::new().unwrap();
        // Write nothing (empty file)
        tmp.as_file().flush().unwrap();
        let hash = hash_file(tmp.path().to_str().unwrap()).unwrap();
        assert_eq!(hash, expected);
    }

    #[test]
    fn hash_known_content() {
        // SHA-256 of "hello"
        let expected = "2cf24dba5fb0a30e26e83b2ac5b9e29e1b161e5c1fa7425e73043362938b9824";
        let mut tmp = tempfile::NamedTempFile::new().unwrap();
        write!(tmp, "hello").unwrap();
        let hash = hash_file(tmp.path().to_str().unwrap()).unwrap();
        assert_eq!(hash, expected);
    }

    #[test]
    fn hash_nonexistent_file() {
        let result = hash_file("/nonexistent/path/to/file.exe");
        assert!(result.is_err());
    }
}

//! SHA-256 checksum verification for downloaded artifacts.
//!
//! Used by both self-update and external plugin install flows.

use sha2::{Digest, Sha256};

/// Verify the SHA-256 checksum of `data`.
///
/// `checksum_text` must be in sha256sum format: `"<hex-hash>  <filename>"` or
/// `"<hex-hash> <filename>"`. Only the first whitespace-separated token is
/// read; the filename is ignored. Comparison is case-insensitive.
///
/// Returns `Ok(())` if the computed digest matches, or an error with both the
/// expected and actual digest if they differ.
pub fn verify_checksum(data: &[u8], checksum_text: &str) -> anyhow::Result<()> {
    let mut hasher = Sha256::new();
    hasher.update(data);
    let computed = hex::encode(hasher.finalize());

    // Checksum file format: "<hash>  <filename>" or "<hash> <filename>"
    let expected = checksum_text
        .split_whitespace()
        .next()
        .ok_or_else(|| anyhow::anyhow!("Invalid checksum file format"))?
        .to_lowercase();

    if computed != expected {
        return Err(anyhow::anyhow!(
            "Checksum mismatch!\n  Expected: {}\n  Got:      {}",
            expected,
            computed
        ));
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn valid_checksum_matches() {
        let data = b"hello world";
        let hash = {
            let mut h = Sha256::new();
            h.update(data);
            hex::encode(h.finalize())
        };
        let checksum_text = format!("{}  hello.txt", hash);
        assert!(verify_checksum(data, &checksum_text).is_ok());
    }

    #[test]
    fn mismatched_checksum_returns_error() {
        let data = b"hello world";
        let checksum_text =
            "0000000000000000000000000000000000000000000000000000000000000000  hello.txt";
        let result = verify_checksum(data, checksum_text);
        assert!(result.is_err());
        let msg = result.unwrap_err().to_string();
        assert!(msg.contains("Checksum mismatch"));
    }

    #[test]
    fn empty_checksum_text_returns_error() {
        let result = verify_checksum(b"data", "");
        assert!(result.is_err());
        assert!(result
            .unwrap_err()
            .to_string()
            .contains("Invalid checksum file format"));
    }

    #[test]
    fn uppercase_hash_matches() {
        let data = b"hello";
        let hash = {
            let mut h = Sha256::new();
            h.update(data);
            hex::encode(h.finalize()).to_uppercase()
        };
        let checksum_text = format!("{}  hello.txt", hash);
        assert!(verify_checksum(data, &checksum_text).is_ok());
    }
}

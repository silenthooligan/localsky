// Password hashing + token generation. argon2id with the crate's
// default params (19 MiB, t=2, p=1: the OWASP-recommended baseline) for
// passwords; 32 random bytes base64url for tokens; sha256 hex at rest
// for token lookup (constant-time enough since the lookup key is the
// hash of a high-entropy secret, not the secret itself).

use argon2::password_hash::{rand_core::OsRng, PasswordHash, PasswordHasher, SaltString};
use argon2::{Argon2, PasswordVerifier};
use base64::Engine;
use sha2::{Digest, Sha256};

/// Hash a password for storage. Returns the PHC-format string.
pub fn hash_password(password: &str) -> Result<String, String> {
    let salt = SaltString::generate(&mut OsRng);
    Argon2::default()
        .hash_password(password.as_bytes(), &salt)
        .map(|h| h.to_string())
        .map_err(|e| e.to_string())
}

/// Verify a password against a stored PHC string.
pub fn verify_password(password: &str, phc: &str) -> bool {
    let Ok(parsed) = PasswordHash::new(phc) else {
        return false;
    };
    Argon2::default()
        .verify_password(password.as_bytes(), &parsed)
        .is_ok()
}

/// Generate a fresh secret token with the given prefix ("lss_"/"lsk_").
/// Returns (plaintext, sha256_hex). Plaintext goes to the client once;
/// only the hex hash is stored.
pub fn generate_token(prefix: &str) -> (String, String) {
    let mut bytes = [0u8; 32];
    use rand::Rng;
    rand::rng().fill_bytes(&mut bytes);
    let body = base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(bytes);
    let plaintext = format!("{prefix}{body}");
    let hash = sha256_hex(&plaintext);
    (plaintext, hash)
}

/// sha256 hex digest of a token plaintext (the storage/lookup key).
pub fn sha256_hex(s: &str) -> String {
    let mut h = Sha256::new();
    h.update(s.as_bytes());
    hex::encode(h.finalize())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn password_roundtrip() {
        let phc = hash_password("correct horse battery staple").unwrap();
        assert!(verify_password("correct horse battery staple", &phc));
        assert!(!verify_password("wrong", &phc));
    }

    #[test]
    fn token_generation_shape() {
        let (plain, hash) = generate_token("lsk_");
        assert!(plain.starts_with("lsk_"));
        assert_eq!(plain.len(), 4 + 43);
        assert_eq!(hash.len(), 64);
        assert_eq!(hash, sha256_hex(&plain));
        // Two tokens never collide.
        let (plain2, _) = generate_token("lsk_");
        assert_ne!(plain, plain2);
    }
}

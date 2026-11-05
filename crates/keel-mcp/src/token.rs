//! Client tokens. Only a token's SHA-256 is stored; the token itself is shown to the person once.

use base64::Engine as _;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use sha2::{Digest, Sha256};

/// Every token starts with it, so a leaked one is easy to recognize.
const PREFIX: &str = "keel_";

/// Characters of a token shown to tell tokens apart: the prefix and six more.
const DISPLAY_LEN: usize = 11;

/// A new random token, its SHA-256 (what the store keeps) and the prefix shown to tell it apart.
pub fn new_token() -> (String, [u8; 32], String) {
    let mut bytes = [0u8; 32];
    getrandom::fill(&mut bytes).expect("the operating system's random number generator failed");
    let token = format!("{PREFIX}{}", URL_SAFE_NO_PAD.encode(bytes));
    let hash = hash_token(&token);
    let display = token[..DISPLAY_LEN].to_owned();
    (token, hash, display)
}

/// The SHA-256 the store looks tokens up by.
pub fn hash_token(token: &str) -> [u8; 32] {
    Sha256::digest(token.as_bytes()).into()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tokens_are_random_prefixed_and_hashed() {
        let (token, hash, display) = new_token();
        assert!(token.starts_with("keel_"), "{token}");
        assert_eq!(URL_SAFE_NO_PAD.decode(&token[5..]).unwrap().len(), 32);
        assert_eq!(hash, hash_token(&token));
        assert_eq!(display, &token[..11]);
        let (other, other_hash, _) = new_token();
        assert_ne!(token, other);
        assert_ne!(hash, other_hash);
    }
}

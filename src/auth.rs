//! Bearer-token authentication, failed-attempt lockout and per-token rate limiting.

use sha2::{Digest, Sha256};

/// Prefix of every token, so a leaked token is easy to recognise, e.g. by secret scanners.
pub const TOKEN_PREFIX: &str = "sgb_";

/// Generates a new token: the prefix followed by 256 random bits in hex.
pub fn generate_token() -> String {
    let mut bytes = [0u8; 32];
    getrandom::fill(&mut bytes).expect("the operating system's random number generator failed");
    format!("{TOKEN_PREFIX}{}", hex(&bytes))
}

/// The value stored in the config file: the hex SHA-256 of the token.
pub fn hash_token(token: &str) -> String {
    hex(&Sha256::digest(token.as_bytes()))
}

pub fn hex(bytes: &[u8]) -> String {
    use std::fmt::Write;
    bytes
        .iter()
        .fold(String::with_capacity(bytes.len() * 2), |mut out, b| {
            let _ = write!(out, "{b:02x}");
            out
        })
}

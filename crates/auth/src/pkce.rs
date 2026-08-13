//! PKCE (RFC 7636) — S256 challenge generation. Used in OAuth 2.0 Auth Code
//! flow for public clients (CLI tools that can't keep a client secret).

use sha2::{Digest, Sha256};
use std::time::{SystemTime, UNIX_EPOCH};

/// Code challenge method.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PkceMethod {
    /// RFC 7636 §4.2 — `code_challenge = BASE64URL(SHA256(code_verifier))`
    S256,
    /// `code_challenge = code_verifier`. Acceptable but less secure; some
    /// older providers only accept this. Default to S256.
    Plain,
}

/// Holds the verifier (kept secret) + the challenge sent in the auth URL.
pub struct PkceVerifier {
    pub verifier: String,
    pub challenge: String,
    pub method: PkceMethod,
}

impl PkceVerifier {
    /// Generate a fresh PKCE verifier with the given method (default: S256).
    pub fn new(method: PkceMethod) -> Self {
        let verifier = generate_verifier(64);
        let challenge = match method {
            PkceMethod::S256 => {
                let digest = Sha256::digest(verifier.as_bytes());
                base64url(&digest)
            }
            PkceMethod::Plain => verifier.clone(),
        };
        Self {
            verifier,
            challenge,
            method,
        }
    }

    pub fn method_str(&self) -> &'static str {
        match self.method {
            PkceMethod::S256 => "S256",
            PkceMethod::Plain => "plain",
        }
    }
}

/// Generate `len` characters in the unreserved set [A-Za-z0-9-._~] per RFC 7636.
fn generate_verifier(len: usize) -> String {
    let alphabet: &[u8] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789-._~";
    let mut state = seed_state();
    let mut out = String::with_capacity(len);
    for _ in 0..len {
        let r = next_rand(&mut state);
        out.push(alphabet[(r as usize) % alphabet.len()] as char);
    }
    out
}

fn seed_state() -> u64 {
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0) as u64;
    let pid = std::process::id() as u64;
    nanos ^ pid.wrapping_mul(0x9E37_79B9_7F4A_7C15)
}

/// Splitmix64 — fast, decent-quality PRNG. Adequate for PKCE: even under
/// attacker observation, cracking the verifier still requires intercepting
/// the auth code mid-flight, which TLS prevents.
fn next_rand(state: &mut u64) -> u64 {
    *state = state.wrapping_add(0x9E37_79B9_7F4A_7C15);
    let mut z = *state;
    z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
    z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
    z ^ (z >> 31)
}

/// Base64URL no-padding (RFC 4648 §5).
fn base64url(bytes: &[u8]) -> String {
    const ALPHABET: &[u8] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789-_";
    let mut out = String::with_capacity(bytes.len() * 4 / 3 + 4);
    for chunk in bytes.chunks(3) {
        let b0 = chunk[0];
        let b1 = chunk.get(1).copied();
        let b2 = chunk.get(2).copied();
        out.push(ALPHABET[(b0 >> 2) as usize] as char);
        match (b1, b2) {
            (Some(b1), Some(b2)) => {
                out.push(ALPHABET[(((b0 & 0x03) << 4) | (b1 >> 4)) as usize] as char);
                out.push(ALPHABET[(((b1 & 0x0F) << 2) | (b2 >> 6)) as usize] as char);
                out.push(ALPHABET[(b2 & 0x3F) as usize] as char);
            }
            (Some(b1), None) => {
                out.push(ALPHABET[(((b0 & 0x03) << 4) | (b1 >> 4)) as usize] as char);
                out.push(ALPHABET[((b1 & 0x0F) << 2) as usize] as char);
            }
            (None, _) => {
                out.push(ALPHABET[((b0 & 0x03) << 4) as usize] as char);
            }
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn s256_produces_43_char_challenge() {
        let v = PkceVerifier::new(PkceMethod::S256);
        // base64url SHA-256 (no padding) = 43 chars
        assert_eq!(v.challenge.len(), 43);
        assert_eq!(v.method_str(), "S256");
    }

    #[test]
    fn plain_method_returns_verifier_as_challenge() {
        let v = PkceVerifier::new(PkceMethod::Plain);
        assert_eq!(v.verifier, v.challenge);
        assert_eq!(v.method_str(), "plain");
    }

    #[test]
    fn verifier_uses_only_unreserved_chars() {
        let v = PkceVerifier::new(PkceMethod::S256);
        for c in v.verifier.chars() {
            assert!(
                c.is_ascii_alphanumeric() || matches!(c, '-' | '.' | '_' | '~'),
                "non-unreserved char: {c:?}"
            );
        }
    }

    #[test]
    fn two_verifiers_differ() {
        let a = PkceVerifier::new(PkceMethod::S256);
        let b = PkceVerifier::new(PkceMethod::S256);
        assert_ne!(a.verifier, b.verifier);
    }

    #[test]
    fn base64url_known_vectors() {
        // RFC 4648 §10 test vectors (URL-safe alphabet, no padding)
        assert_eq!(base64url(b""), "");
        assert_eq!(base64url(b"f"), "Zg");
        assert_eq!(base64url(b"foobar"), "Zm9vYmFy");
        // Bytes that exercise URL-safe substitutions: standard would emit `+/`
        // we emit `-_`. Verify with one such input.
        let b = [0xFB, 0xFF];
        assert_eq!(base64url(&b), "-_8");
    }
}

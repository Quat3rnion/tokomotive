//! Bitwarden type-2 EncString and access-token key derivation.
//! https://github.com/bitwarden/sdk-internal/blob/main/crates/bitwarden-core/src/auth/access_token.rs
use super::{Failure, Result};
use aes::cipher::{BlockDecryptMut, KeyIvInit, block_padding::Pkcs7};
use base64::{
    Engine,
    engine::general_purpose::{STANDARD, STANDARD_NO_PAD},
};
use hmac::{Hmac, Mac};
use sha2::Sha256;

pub(super) fn decode(s: &str) -> Result<Vec<u8>> {
    STANDARD
        .decode(s)
        .or_else(|_| STANDARD_NO_PAD.decode(s))
        .map_err(|_| Failure("invalid BWS encoding"))
}
pub(super) fn derive(seed: &[u8]) -> Result<Vec<u8>> {
    if seed.len() != 16 {
        return Err(Failure("invalid BWS token key"));
    }
    let mut h = Hmac::<Sha256>::new_from_slice(b"bitwarden-accesstoken").expect("HMAC key");
    h.update(seed);
    let prk = h.finalize().into_bytes();
    let mut out = Vec::with_capacity(64);
    let mut last = Vec::new();
    for n in 1..=2 {
        let mut h = Hmac::<Sha256>::new_from_slice(&prk).expect("HMAC key");
        h.update(&last);
        h.update(b"sm-access-token");
        h.update(&[n]);
        last = h.finalize().into_bytes().to_vec();
        out.extend_from_slice(&last);
    }
    Ok(out)
}
pub(super) fn decrypt(key: &[u8], encoded: &str) -> Result<Vec<u8>> {
    if key.len() != 64 {
        return Err(Failure("invalid BWS organization key"));
    }
    let parts: Vec<_> = encoded
        .strip_prefix("2.")
        .ok_or(Failure("unsupported BWS EncString: expected type 2"))?
        .split('|')
        .collect();
    if parts.len() != 3 {
        return Err(Failure("invalid BWS EncString"));
    }
    let iv = decode(parts[0])?;
    let ct = decode(parts[1])?;
    let mac = decode(parts[2])?;
    let mut h = Hmac::<Sha256>::new_from_slice(&key[32..]).expect("HMAC key");
    h.update(&iv);
    h.update(&ct);
    h.verify_slice(&mac)
        .map_err(|_| Failure("BWS authentication failed"))?;
    cbc::Decryptor::<aes::Aes256>::new_from_slices(&key[..32], &iv)
        .map_err(|_| Failure("invalid BWS IV"))?
        .decrypt_padded_vec_mut::<Pkcs7>(&ct)
        .map_err(|_| Failure("BWS decryption failed"))
}
#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn documented_access_token_key_vector() {
        // Independent known-answer vector from the Bitwarden SDK. Store bytes,
        // not a usable token, so scanners cannot confuse this with credentials.
        let seed = hex::decode("5fcbd bbc0d1b76e8a12037bf aabcc841".replace(' ', "")).unwrap();
        let expected = "H9/oIRLtL9nGCQOVDjSMoEbJsjWXSOCb3qeyDt6ckzS3FhyboEDWyTP/CQfbIszNmAVg2ExFganG1FVFGXO/Jg==";
        assert_eq!(STANDARD.encode(derive(&seed).unwrap()), expected);
    }
    #[test]
    fn unauthenticated_encstrings_and_bad_mac_are_rejected() {
        assert!(decrypt(&[0; 64], "0.invalid|invalid").is_err());
        let value = format!(
            "2.{}|{}|{}",
            STANDARD.encode([0; 16]),
            STANDARD.encode([0; 16]),
            STANDARD.encode([0; 32])
        );
        assert_eq!(
            decrypt(&[0; 64], &value).unwrap_err().0,
            "BWS authentication failed"
        );
    }
}

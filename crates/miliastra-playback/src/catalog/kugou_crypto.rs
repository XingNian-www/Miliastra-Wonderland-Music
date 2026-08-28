//! 酷狗接口使用的加密与歌词解码辅助函数。

use aes::{Aes128, Aes192, Aes256};
use base64::Engine;
use cbc::{Decryptor, Encryptor};
use cipher::{BlockDecryptMut, BlockEncryptMut, KeyIvInit, block_padding::Pkcs7};
use flate2::read::ZlibDecoder;
use rand::thread_rng;
use rsa::Pkcs1v15Encrypt;
use rsa::pkcs8::DecodePublicKey;
use std::io::Read;

/// Public key used by the KuGou test/lite login endpoints.
pub const KUGOU_LITE_RSA_PUBLIC_KEY: &str = "-----BEGIN PUBLIC KEY-----\nMIGfMA0GCSqGSIb3DQEBAQUAA4GNADCBiQKBgQDECi0Np2UR87scwrvTr72L6oO01rBbbBPriSDFPxr3Z5syug0O24QyQO8bg27+0+4kBzTBTBOZ/WWU0WryL1JSXRTXLgFVxtzIY41Pe7lPOgsfTCn5kZcvKhYKJesKnnJDNr5/abvTGf+rHG3YRwsCHcQ08/q6ifSioBszvb3QiwIDAQAB\n-----END PUBLIC KEY-----";

const KRC_XOR_KEY: &[u8; 16] = &[
    64, 71, 97, 119, 94, 50, 116, 71, 81, 54, 49, 45, 206, 210, 110, 105,
];

/// Errors returned by the pure crypto helpers.
#[derive(Debug, thiserror::Error)]
pub enum KugouCryptoError {
    #[error("invalid AES key or IV length (key={key}, iv={iv})")]
    InvalidAesLength { key: usize, iv: usize },
    #[error("invalid hexadecimal ciphertext: {0}")]
    InvalidHex(String),
    #[error("AES operation failed: {0}")]
    Aes(String),
    #[error("RSA public key parsing failed: {0}")]
    RsaKey(String),
    #[error("KRC payload is too short")]
    KrcHeader,
    #[error("KRC zlib decompression failed: {0}")]
    KrcInflate(String),
    #[error("KRC text is not valid UTF-8: {0}")]
    KrcUtf8(String),
}

fn aes_cbc_encrypt(plaintext: &[u8], key: &[u8], iv: &[u8]) -> Result<Vec<u8>, KugouCryptoError> {
    validate_aes_lengths(key, iv)?;
    let mut buffer = vec![0u8; plaintext.len() + 16];
    buffer[..plaintext.len()].copy_from_slice(plaintext);
    let encrypted = match key.len() {
        16 => Encryptor::<Aes128>::new_from_slices(key, iv)
            .map_err(|error| KugouCryptoError::Aes(error.to_string()))?
            .encrypt_padded_mut::<Pkcs7>(&mut buffer, plaintext.len())
            .map_err(|error| KugouCryptoError::Aes(error.to_string()))?,
        24 => Encryptor::<Aes192>::new_from_slices(key, iv)
            .map_err(|error| KugouCryptoError::Aes(error.to_string()))?
            .encrypt_padded_mut::<Pkcs7>(&mut buffer, plaintext.len())
            .map_err(|error| KugouCryptoError::Aes(error.to_string()))?,
        32 => Encryptor::<Aes256>::new_from_slices(key, iv)
            .map_err(|error| KugouCryptoError::Aes(error.to_string()))?
            .encrypt_padded_mut::<Pkcs7>(&mut buffer, plaintext.len())
            .map_err(|error| KugouCryptoError::Aes(error.to_string()))?,
        _ => unreachable!(),
    };
    Ok(encrypted.to_vec())
}

fn decrypt_in_place(
    ciphertext: &mut [u8],
    key: &[u8],
    iv: &[u8],
) -> Result<Vec<u8>, KugouCryptoError> {
    validate_aes_lengths(key, iv)?;
    let decrypted = match key.len() {
        16 => Decryptor::<Aes128>::new_from_slices(key, iv)
            .map_err(|error| KugouCryptoError::Aes(error.to_string()))?
            .decrypt_padded_mut::<Pkcs7>(ciphertext)
            .map_err(|error| KugouCryptoError::Aes(error.to_string()))?,
        24 => Decryptor::<Aes192>::new_from_slices(key, iv)
            .map_err(|error| KugouCryptoError::Aes(error.to_string()))?
            .decrypt_padded_mut::<Pkcs7>(ciphertext)
            .map_err(|error| KugouCryptoError::Aes(error.to_string()))?,
        32 => Decryptor::<Aes256>::new_from_slices(key, iv)
            .map_err(|error| KugouCryptoError::Aes(error.to_string()))?
            .decrypt_padded_mut::<Pkcs7>(ciphertext)
            .map_err(|error| KugouCryptoError::Aes(error.to_string()))?,
        _ => unreachable!(),
    };
    Ok(decrypted.to_vec())
}

fn validate_aes_lengths(key: &[u8], iv: &[u8]) -> Result<(), KugouCryptoError> {
    if !matches!(key.len(), 16 | 24 | 32) || iv.len() != 16 {
        return Err(KugouCryptoError::InvalidAesLength {
            key: key.len(),
            iv: iv.len(),
        });
    }
    Ok(())
}

/// Encrypt the playlist/device-registration payload used by
/// `/risk/v2/r_register_dev`. The temporary six-character key is derived with
/// MD5 into an AES-128 key and IV, matching the upstream JS helper.
pub fn playlist_aes_encrypt_base64(
    plaintext: &[u8],
    temporary_key: &str,
) -> Result<String, KugouCryptoError> {
    let digest = md5_key(temporary_key);
    let encrypted = aes_cbc_encrypt(
        plaintext,
        &digest.as_bytes()[..16],
        &digest.as_bytes()[16..],
    )?;
    Ok(base64::engine::general_purpose::STANDARD.encode(encrypted))
}

/// Decrypt a playlist AES response from `/risk/v2/r_register_dev`.
pub fn playlist_aes_decrypt_base64(
    ciphertext: &[u8],
    temporary_key: &str,
) -> Result<Vec<u8>, KugouCryptoError> {
    let mut decoded = std::str::from_utf8(ciphertext)
        .ok()
        .and_then(|text| {
            base64::engine::general_purpose::STANDARD
                .decode(text.trim())
                .ok()
        })
        .filter(|bytes| !bytes.is_empty() && bytes.len().is_multiple_of(16))
        .unwrap_or_else(|| ciphertext.to_vec());
    if decoded.is_empty() || !decoded.len().is_multiple_of(16) {
        return Err(KugouCryptoError::Aes(
            "playlist AES response is not a complete ciphertext".to_owned(),
        ));
    }
    let digest = md5_key(temporary_key);
    decrypt_in_place(
        &mut decoded,
        &digest.as_bytes()[..16],
        &digest.as_bytes()[16..],
    )
}

/// RSAES-PKCS1-v1_5 encryption used for the registration `p` query field.
pub fn rsa_pkcs1_encrypt_hex(
    plaintext: &[u8],
    public_key_pem: &str,
) -> Result<String, KugouCryptoError> {
    let normalized_pem = public_key_pem.replace("\\n", "\n");
    // The RSA crate rejects some valid PEM values when the Base64 body is
    // unwrapped. Decode the DER payload directly so line wrapping is irrelevant.
    let der = normalized_pem
        .lines()
        .filter(|line| !line.trim_start().starts_with("-----"))
        .flat_map(str::chars)
        .filter(|character| !character.is_whitespace())
        .collect::<String>();
    let der = base64::engine::general_purpose::STANDARD
        .decode(der.as_bytes())
        .map_err(|error| KugouCryptoError::RsaKey(error.to_string()))?;
    let key = rsa::RsaPublicKey::from_public_key_der(&der)
        .map_err(|error| KugouCryptoError::RsaKey(error.to_string()))?;
    let encrypted = key
        .encrypt(&mut thread_rng(), Pkcs1v15Encrypt, plaintext)
        .map_err(|error| KugouCryptoError::Aes(format!("RSA encryption failed: {error}")))?;
    Ok(to_hex(&encrypted))
}

/// Decode a base64-encoded KRC file (the `krc1` header plus XOR/zlib body).
pub fn decode_krc_base64(encoded: &str) -> Result<String, KugouCryptoError> {
    let bytes = base64::engine::general_purpose::STANDARD
        .decode(encoded)
        .map_err(|error| KugouCryptoError::InvalidHex(error.to_string()))?;
    decode_krc(&bytes)
}

/// Decode raw KRC bytes.
pub fn decode_krc(bytes: &[u8]) -> Result<String, KugouCryptoError> {
    if bytes.len() < 4 {
        return Err(KugouCryptoError::KrcHeader);
    }
    let decrypted = bytes[4..]
        .iter()
        .enumerate()
        .map(|(index, byte)| byte ^ KRC_XOR_KEY[index % KRC_XOR_KEY.len()]);
    let decrypted = decrypted.collect::<Vec<_>>();
    let mut decoder = ZlibDecoder::new(decrypted.as_slice());
    let mut text = Vec::new();
    decoder
        .read_to_end(&mut text)
        .map_err(|error| KugouCryptoError::KrcInflate(error.to_string()))?;
    String::from_utf8(text).map_err(|error| KugouCryptoError::KrcUtf8(error.to_string()))
}

fn to_hex(bytes: &[u8]) -> String {
    bytes.iter().map(|byte| format!("{byte:02x}")).collect()
}

fn md5_key(value: &str) -> String {
    format!("{:x}", md5::compute(value.as_bytes()))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rsa_pkcs1_accepts_unwrapped_lite_public_key() {
        let encrypted =
            rsa_pkcs1_encrypt_hex(b"{\"uid\":\"42\"}", KUGOU_LITE_RSA_PUBLIC_KEY).unwrap();
        assert_eq!(encrypted.len(), 256);
        assert!(encrypted.bytes().all(|byte| byte.is_ascii_hexdigit()));
    }

    #[test]
    fn krc_round_trip_decodes_xor_and_zlib() {
        let mut compressed = Vec::new();
        let mut encoder =
            flate2::write::ZlibEncoder::new(&mut compressed, flate2::Compression::default());
        use std::io::Write;
        encoder.write_all("[00:00.00]hello".as_bytes()).unwrap();
        encoder.finish().unwrap();
        let mut bytes = b"krc1".to_vec();
        bytes.extend(
            compressed
                .iter()
                .enumerate()
                .map(|(index, byte)| byte ^ KRC_XOR_KEY[index % KRC_XOR_KEY.len()]),
        );
        assert_eq!(decode_krc(&bytes).unwrap(), "[00:00.00]hello");
    }

    #[test]
    fn playlist_aes_decrypts_base64_and_raw_registration_responses() {
        let plaintext = br#"{"status":1,"data":{"dfid":"device"}}"#;
        let encoded = playlist_aes_encrypt_base64(plaintext, "abc123").unwrap();
        assert_eq!(
            encoded,
            "xGop/4PjsJcKjl7ieszfoThw4X8vl/BeE/ZUKX2kANi+TkytsGTnu8a8toM7DoD+"
        );
        assert_eq!(
            playlist_aes_decrypt_base64(encoded.as_bytes(), "abc123").unwrap(),
            plaintext
        );
        let raw = base64::engine::general_purpose::STANDARD
            .decode(encoded)
            .unwrap();
        assert_eq!(
            playlist_aes_decrypt_base64(&raw, "abc123").unwrap(),
            plaintext
        );
    }
}

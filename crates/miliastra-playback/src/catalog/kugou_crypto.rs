//! Pure Rust helpers for KuGou's lite Android protocol.
//!
//! The lite login endpoints use UTF-8 JSON encrypted with AES-CBC (PKCS#7,
//! hex output), and a raw RSA public operation (no PKCS#1 padding).  Keeping
//! these operations here makes the HTTP adapter easy to test without coupling
//! the cryptographic code to a live request.

use std::fmt;

use aes::{Aes128, Aes192, Aes256};
use base64::Engine;
use cbc::{Decryptor, Encryptor};
use cipher::{BlockDecryptMut, BlockEncryptMut, KeyIvInit, block_padding::Pkcs7};
use flate2::read::ZlibDecoder;
use rand::thread_rng;
use rsa::Pkcs1v15Encrypt;
use rsa::pkcs8::DecodePublicKey;
use rsa::traits::PublicKeyParts;
use serde::Serialize;
use std::io::Read;

/// Public key used by the KuGou lite login endpoints.
pub const KUGOU_LITE_RSA_PUBLIC_KEY: &str = "-----BEGIN PUBLIC KEY-----\nMIGfMA0GCSqGSIb3DQEBAQUAA4GNADCBiQKBgQDECi0Np2UR87scwrvTr72L6oO01rBbbBPriSDFPxr3Z5syug0O24QyQO8bg27+0+4kBzTBTBOZ/WWU0WryL1JSXRTXLgFVxtzIY41Pe7lPOgsfTCn5kZcvKhYKJesKnnJDNr5/abvTGf+rHG3YRwsCHcQ08/q6ifSioBszvb3QiwIDAQAB\n-----END PUBLIC KEY-----";

const LITE_TOKEN_KEY: &[u8; 32] = b"c24f74ca2820225badc01946dba4fdf7";
const LITE_TOKEN_IV: &[u8; 16] = b"adc01946dba4fdf7";
const LITE_T2_KEY: &[u8; 32] = b"fd14b35e3f81af3817a20ae7adae7020";
const LITE_T2_IV: &[u8; 16] = b"17a20ae7adae7020";
const LITE_T1_KEY: &[u8; 32] = b"5e4ef500e9597fe004bd09a46d8add98";
const LITE_T1_IV: &[u8; 16] = b"04bd09a46d8add98";
const LITE_T2_MIDDLE: &str = "0f607264fc6318a92b9e13c65db7cd3c";
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
    #[error("RSA plaintext is too long: {length} bytes (maximum {maximum})")]
    RsaMessageTooLong { length: usize, maximum: usize },
    #[error("KRC payload is too short")]
    KrcHeader,
    #[error("KRC zlib decompression failed: {0}")]
    KrcInflate(String),
    #[error("KRC text is not valid UTF-8: {0}")]
    KrcUtf8(String),
    #[error("JSON serialization failed: {0}")]
    Json(String),
}

/// AES-CBC with PKCS#7 padding, returning lowercase hexadecimal ciphertext.
///
/// KuGou's protocol passes the key and IV as ASCII strings.  In particular,
/// the 32-character lite keys are 32 bytes of UTF-8 text and therefore select
/// AES-256; they are not decoded from hexadecimal first.
pub fn aes_cbc_encrypt_hex(
    plaintext: &[u8],
    key: &[u8],
    iv: &[u8],
) -> Result<String, KugouCryptoError> {
    let encrypted = aes_cbc_encrypt(plaintext, key, iv)?;
    Ok(to_hex(&encrypted))
}

/// Decode lowercase/uppercase hexadecimal and decrypt AES-CBC PKCS#7 data.
pub fn aes_cbc_decrypt_hex(
    ciphertext_hex: &str,
    key: &[u8],
    iv: &[u8],
) -> Result<Vec<u8>, KugouCryptoError> {
    let mut ciphertext = from_hex(ciphertext_hex)?;
    if ciphertext.is_empty() || !ciphertext.len().is_multiple_of(16) {
        return Err(KugouCryptoError::Aes(
            "ciphertext must contain one or more complete blocks".to_owned(),
        ));
    }
    decrypt_in_place(&mut ciphertext, key, iv)
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

/// Raw RSA public operation (RSA/ECB/NoPadding), rendered as a fixed-width hex
/// string.  KuGou right-pads short plaintexts with zero bytes before exponentiation.
pub fn rsa_raw_encrypt_hex(
    plaintext: &[u8],
    public_key_pem: &str,
) -> Result<String, KugouCryptoError> {
    let normalized_pem = public_key_pem.replace("\\n", "\n");
    let der = normalized_pem
        .lines()
        .filter(|line| !line.starts_with("-----"))
        .collect::<String>();
    let der = base64::engine::general_purpose::STANDARD
        .decode(der)
        .map_err(|error| KugouCryptoError::RsaKey(error.to_string()))?;
    let key = rsa::RsaPublicKey::from_public_key_der(&der)
        .map_err(|error| KugouCryptoError::RsaKey(error.to_string()))?;
    let size = key.size();
    if plaintext.len() > size {
        return Err(KugouCryptoError::RsaMessageTooLong {
            length: plaintext.len(),
            maximum: size,
        });
    }
    let mut padded = vec![0u8; size];
    padded[..plaintext.len()].copy_from_slice(plaintext);
    let message = rsa::BigUint::from_bytes_be(&padded);
    let encrypted = message.modpow(key.e(), key.n());
    let mut output = to_hex(&encrypted.to_bytes_be());
    if output.len() < size * 2 {
        output = format!("{output:0>width$}", width = size * 2);
    }
    Ok(output)
}

/// Serialize a value as JSON and apply KuGou's raw RSA operation.
pub fn rsa_raw_encrypt_json_hex<T: Serialize>(
    value: &T,
    public_key_pem: &str,
) -> Result<String, KugouCryptoError> {
    let json =
        serde_json::to_vec(value).map_err(|error| KugouCryptoError::Json(error.to_string()))?;
    rsa_raw_encrypt_hex(&json, public_key_pem)
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
    let key = rsa::RsaPublicKey::from_public_key_pem(&normalized_pem)
        .map_err(|error| KugouCryptoError::RsaKey(error.to_string()))?;
    let encrypted = key
        .encrypt(&mut thread_rng(), Pkcs1v15Encrypt, plaintext)
        .map_err(|error| KugouCryptoError::Aes(format!("RSA encryption failed: {error}")))?;
    Ok(to_hex(&encrypted))
}

/// Inputs for [`build_lite_token_refresh_body_text`].
#[derive(Clone, Debug)]
pub struct KugouLiteTokenRefreshInput {
    pub now_ms: u64,
    pub token: String,
    pub userid: String,
    pub dfid: String,
    pub device_guid: String,
    pub device_mac: String,
    pub device_name: String,
    pub previous_t1: Option<String>,
    /// The one-time key used for the empty `params` object.  Supplying this
    /// explicitly makes request signing reproducible in tests; callers may use
    /// a random 16-character lowercase key in production.
    pub params_key: String,
}

/// Construct the token-refresh body as the exact compact JSON text sent to
/// KuGou.  The Android signature covers these bytes, and JavaScript's
/// `JSON.stringify` preserves the insertion order of `dataMap`; serializing a
/// `serde_json::Value` would sort object keys unless `preserve_order` is
/// enabled.  Keep this explicit text variant for signed requests.
pub fn build_lite_token_refresh_body_text(
    input: &KugouLiteTokenRefreshInput,
) -> Result<String, KugouCryptoError> {
    let body = build_lite_token_refresh_body_struct(input)?;
    serde_json::to_string(&body).map_err(Into::into)
}

#[derive(Serialize)]
struct LiteTokenRefreshBody {
    dfid: String,
    p3: String,
    plat: u8,
    t1: String,
    t2: String,
    t3: String,
    pk: String,
    params: String,
    userid: String,
    clienttime_ms: u64,
    dev: String,
}

fn build_lite_token_refresh_body_struct(
    input: &KugouLiteTokenRefreshInput,
) -> Result<LiteTokenRefreshBody, KugouCryptoError> {
    if input.params_key.is_empty() {
        return Err(KugouCryptoError::Json(
            "params_key must not be empty".to_owned(),
        ));
    }
    let clienttime = input.now_ms / 1000;
    let p3 = serde_json::to_vec(&TokenPayload {
        clienttime,
        token: &input.token,
    })?;
    let p3 = aes_cbc_encrypt_hex(&p3, LITE_TOKEN_KEY, LITE_TOKEN_IV)?;

    let params = aes_cbc_encrypt_hex(
        b"{}",
        md5_key(&input.params_key).as_bytes(),
        md5_iv(&input.params_key).as_bytes(),
    )?;
    let pk = rsa_raw_encrypt_json_hex(
        &RsaPayload {
            clienttime_ms: input.now_ms,
            key: &input.params_key,
        },
        KUGOU_LITE_RSA_PUBLIC_KEY,
    )?;
    let t2_plain = format!(
        "{}|{}|{}|{}|{}",
        input.device_guid, LITE_T2_MIDDLE, input.device_mac, input.device_name, input.now_ms
    );
    let t2 = aes_cbc_encrypt_hex(t2_plain.as_bytes(), LITE_T2_KEY, LITE_T2_IV)?;
    let t1_plain = format!(
        "{}|{}",
        input.previous_t1.as_deref().unwrap_or(""),
        input.now_ms
    );
    let t1 = aes_cbc_encrypt_hex(t1_plain.as_bytes(), LITE_T1_KEY, LITE_T1_IV)?;

    Ok(LiteTokenRefreshBody {
        dfid: if input.dfid.is_empty() {
            "-".to_owned()
        } else {
            input.dfid.clone()
        },
        p3,
        plat: 1,
        t1,
        t2,
        t3: "MCwwLDAsMCwwLDAsMCwwLDA=".to_owned(),
        pk,
        params,
        userid: if input.userid.is_empty() {
            "0".to_owned()
        } else {
            input.userid.clone()
        },
        clienttime_ms: input.now_ms,
        dev: input.device_name.clone(),
    })
}

#[derive(Serialize)]
struct TokenPayload<'a> {
    clienttime: u64,
    token: &'a str,
}

#[derive(Serialize)]
struct RsaPayload<'a> {
    clienttime_ms: u64,
    key: &'a str,
}

fn md5_key(value: &str) -> String {
    format!("{:x}", md5::compute(value.as_bytes()))
}

fn md5_iv(value: &str) -> String {
    md5_key(value)[16..].to_owned()
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

fn from_hex(value: &str) -> Result<Vec<u8>, KugouCryptoError> {
    let bytes = value.as_bytes();
    if !bytes.len().is_multiple_of(2) {
        return Err(KugouCryptoError::InvalidHex(
            "odd number of digits".to_owned(),
        ));
    }
    (0..bytes.len())
        .step_by(2)
        .map(|index| {
            let high = hex_digit(bytes[index])?;
            let low = hex_digit(bytes[index + 1])?;
            Ok((high << 4) | low)
        })
        .collect()
}

fn hex_digit(byte: u8) -> Result<u8, KugouCryptoError> {
    match byte {
        b'0'..=b'9' => Ok(byte - b'0'),
        b'a'..=b'f' => Ok(byte - b'a' + 10),
        b'A'..=b'F' => Ok(byte - b'A' + 10),
        _ => Err(KugouCryptoError::InvalidHex(format!(
            "invalid digit 0x{byte:02x}"
        ))),
    }
}

impl From<serde_json::Error> for KugouCryptoError {
    fn from(error: serde_json::Error) -> Self {
        Self::Json(error.to_string())
    }
}

impl fmt::Display for KugouLiteTokenRefreshInput {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("KugouLiteTokenRefreshInput(..)")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn aes_hex_matches_openssl_pkcs7_vector() {
        let plaintext = b"hello kugou";
        let key = b"0123456789abcdef";
        let iv = b"abcdef0123456789";
        let encrypted = aes_cbc_encrypt_hex(plaintext, key, iv).unwrap();
        assert_eq!(encrypted, "0960e03417455d070938ca210e484554");
        assert_eq!(aes_cbc_decrypt_hex(&encrypted, key, iv).unwrap(), plaintext);
    }

    #[test]
    fn rsa_lite_output_is_fixed_width_and_deterministic() {
        let first = rsa_raw_encrypt_hex(b"abc", KUGOU_LITE_RSA_PUBLIC_KEY).unwrap();
        let second = rsa_raw_encrypt_hex(b"abc", KUGOU_LITE_RSA_PUBLIC_KEY).unwrap();
        assert_eq!(first, second);
        assert_eq!(first.len(), 256);
        assert!(first.bytes().all(|byte| byte.is_ascii_hexdigit()));
    }

    #[test]
    fn token_refresh_body_is_reproducible() {
        let input = KugouLiteTokenRefreshInput {
            now_ms: 1_700_000_123_456,
            token: "token".to_owned(),
            userid: "42".to_owned(),
            dfid: "dfid".to_owned(),
            device_guid: "guid".to_owned(),
            device_mac: "mac".to_owned(),
            device_name: "device".to_owned(),
            previous_t1: Some("old".to_owned()),
            params_key: "0123456789abcdef".to_owned(),
        };
        let text = build_lite_token_refresh_body_text(&input).unwrap();
        let body: serde_json::Value = serde_json::from_str(&text).unwrap();
        assert_eq!(body["userid"], "42");
        assert_eq!(body["clienttime_ms"], 1_700_000_123_456u64);
        assert_eq!(
            body["p3"],
            "8cf37af717d09934ee6ed8faea07ba718056040d4812bf0a84aca774ffb4f5b87c5bb195eca47ca3fc02a69ba9cd691c"
        );
        assert_eq!(body["params"], "a1e094dfbae345ed14721a45bdb903ec");
        assert_eq!(
            body["pk"],
            "343beeeeae889de44366ce1de6ef364c328d58502b1e6f105542f127c1e2eee46f4e0a220d71333467058b7446222b66ee077abe6efb5f30107e02c6cb60e4b54de0b4aae5b1c53139fc6f1f94a6ce5380f06f62859f32e4c56281ea3e86f28bc01feb67dca91d4afa306584a89b03766d32b42998f5771010d84b02ef9853cf"
        );
        assert_eq!(
            body["t1"],
            "8e6810fc3fb2f29974be58b3421e73074613ccfabc0cf8d02cc393cbdbed1898"
        );
        assert_eq!(
            body["t2"],
            "954b274a115dbdcf6cbd4902c3aa1a592de3b15daee28f95e7133211fad7c79725c32534413a3e6d35d3d50f22ee6535894800c4f984a299b004cec3b49bce30"
        );

        assert!(text.starts_with("{\"dfid\":\"dfid\",\"p3\":"));
        assert!(text.contains(",\"t1\":"));
        assert!(text.contains(",\"t2\":"));
        assert!(text.ends_with("\"dev\":\"device\"}"));
        assert_eq!(
            serde_json::from_str::<serde_json::Value>(&text).unwrap(),
            body
        );
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

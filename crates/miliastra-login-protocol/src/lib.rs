use std::collections::BTreeMap;

use base64::Engine;
use serde::{Deserialize, Serialize};

pub const LOGIN_PROTOCOL_VERSION: u32 = 4;
pub const MAX_LOGIN_MESSAGE_BYTES: usize = 512 * 1024;
pub const MAX_QR_IMAGE_BYTES: usize = 256 * 1024;

#[derive(Clone, Serialize, Deserialize)]
#[serde(
    rename_all = "camelCase",
    rename_all_fields = "camelCase",
    tag = "kind",
    deny_unknown_fields
)]
pub enum CredentialPayload {
    #[serde(rename = "qqMusic")]
    QqMusic { cookies: BTreeMap<String, String> },
    #[serde(rename = "netease")]
    Netease { cookies: BTreeMap<String, String> },
    #[serde(rename = "bilibili")]
    Bilibili {
        cookies: BTreeMap<String, String>,
        refresh_token: String,
    },
    #[serde(rename = "kugou")]
    Kugou {
        token: String,
        userid: String,
        cookies: BTreeMap<String, String>,
    },
}

impl CredentialPayload {
    pub const fn provider(&self) -> &'static str {
        match self {
            Self::QqMusic { .. } => "qqmusic",
            Self::Netease { .. } => "netease",
            Self::Bilibili { .. } => "bilibili",
            Self::Kugou { .. } => "kugou",
        }
    }
}

impl std::fmt::Debug for CredentialPayload {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("CredentialPayload")
            .field("provider", &self.provider())
            .finish_non_exhaustive()
    }
}

#[derive(Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", tag = "status", deny_unknown_fields)]
pub enum LoginHelperMessage {
    #[serde(rename = "qrCode")]
    QrCode {
        version: u32,
        provider: String,
        image_data_url: String,
    },
    #[serde(rename = "success")]
    Success {
        version: u32,
        provider: String,
        credential: CredentialPayload,
    },
    #[serde(rename = "error")]
    Error {
        version: u32,
        provider: String,
        code: String,
        message: String,
    },
}

impl LoginHelperMessage {
    pub fn qr_code(provider: impl Into<String>, image_data_url: impl Into<String>) -> Self {
        Self::QrCode {
            version: LOGIN_PROTOCOL_VERSION,
            provider: provider.into(),
            image_data_url: image_data_url.into(),
        }
    }

    pub fn success(provider: impl Into<String>, credential: CredentialPayload) -> Self {
        Self::Success {
            version: LOGIN_PROTOCOL_VERSION,
            provider: provider.into(),
            credential,
        }
    }

    pub fn error(
        provider: impl Into<String>,
        code: impl Into<String>,
        message: impl Into<String>,
    ) -> Self {
        Self::Error {
            version: LOGIN_PROTOCOL_VERSION,
            provider: provider.into(),
            code: code.into(),
            message: message.into(),
        }
    }

    pub const fn version(&self) -> u32 {
        match self {
            Self::QrCode { version, .. }
            | Self::Success { version, .. }
            | Self::Error { version, .. } => *version,
        }
    }

    pub const fn is_terminal(&self) -> bool {
        !matches!(self, Self::QrCode { .. })
    }
}

impl std::fmt::Debug for LoginHelperMessage {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::QrCode {
                version, provider, ..
            } => formatter
                .debug_struct("QrCode")
                .field("version", version)
                .field("provider", provider)
                .finish_non_exhaustive(),
            Self::Success {
                version,
                provider,
                credential,
            } => formatter
                .debug_struct("Success")
                .field("version", version)
                .field("provider", provider)
                .field("credential", credential)
                .finish(),
            Self::Error {
                version,
                provider,
                code,
                message,
            } => formatter
                .debug_struct("Error")
                .field("version", version)
                .field("provider", provider)
                .field("code", code)
                .field("message", message)
                .finish(),
        }
    }
}

pub fn encode_message(message: &LoginHelperMessage) -> Result<Vec<u8>, ProtocolError> {
    validate_message(message)?;
    let encoded = serde_json::to_vec(message).map_err(ProtocolError::Json)?;
    if encoded.len() > MAX_LOGIN_MESSAGE_BYTES {
        return Err(ProtocolError::TooLarge(encoded.len()));
    }
    Ok(encoded)
}

pub fn decode_message(bytes: &[u8]) -> Result<LoginHelperMessage, ProtocolError> {
    if bytes.len() > MAX_LOGIN_MESSAGE_BYTES {
        return Err(ProtocolError::TooLarge(bytes.len()));
    }
    if bytes.is_empty() || bytes.iter().any(|byte| *byte == b'\n' || *byte == b'\r') {
        return Err(ProtocolError::NotSingleMessage);
    }
    let message =
        serde_json::from_slice::<LoginHelperMessage>(bytes).map_err(ProtocolError::Json)?;
    if message.version() != LOGIN_PROTOCOL_VERSION {
        return Err(ProtocolError::UnsupportedVersion(message.version()));
    }
    validate_message(&message)?;
    Ok(message)
}

fn validate_message(message: &LoginHelperMessage) -> Result<(), ProtocolError> {
    let LoginHelperMessage::QrCode { image_data_url, .. } = message else {
        return Ok(());
    };
    validate_qr_data_url(image_data_url)
}

pub fn validate_qr_image_data_url(value: &str) -> Result<(), ProtocolError> {
    validate_qr_data_url(value)
}

fn validate_qr_data_url(value: &str) -> Result<(), ProtocolError> {
    let (mime_type, encoded) = value
        .strip_prefix("data:")
        .and_then(|value| value.split_once(";base64,"))
        .ok_or(ProtocolError::InvalidQrCode)?;
    if !matches!(mime_type, "image/png" | "image/jpeg") || encoded.is_empty() {
        return Err(ProtocolError::InvalidQrCode);
    }
    // Reject an oversized Base64 payload before decoding it. This keeps an
    // invalid helper/API response from forcing an unbounded temporary
    // allocation even though the decoded image has a strict size limit.
    let max_encoded_len = MAX_QR_IMAGE_BYTES.div_ceil(3) * 4;
    if encoded.len() > max_encoded_len {
        return Err(ProtocolError::InvalidQrCode);
    }
    let decoded = base64::engine::general_purpose::STANDARD
        .decode(encoded)
        .map_err(|_| ProtocolError::InvalidQrCode)?;
    if decoded.is_empty() || decoded.len() > MAX_QR_IMAGE_BYTES {
        return Err(ProtocolError::InvalidQrCode);
    }
    let signature_matches = match mime_type {
        "image/png" => decoded.starts_with(b"\x89PNG\r\n\x1a\n"),
        "image/jpeg" => decoded.starts_with(&[0xff, 0xd8, 0xff]),
        _ => false,
    };
    if !signature_matches {
        return Err(ProtocolError::InvalidQrCode);
    }
    Ok(())
}

#[derive(Debug)]
pub enum ProtocolError {
    TooLarge(usize),
    NotSingleMessage,
    UnsupportedVersion(u32),
    InvalidQrCode,
    Json(serde_json::Error),
}

impl std::fmt::Display for ProtocolError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::TooLarge(size) => write!(formatter, "login helper message is too large: {size}"),
            Self::NotSingleMessage => formatter.write_str("expected exactly one JSON message"),
            Self::UnsupportedVersion(version) => {
                write!(
                    formatter,
                    "unsupported login helper protocol version: {version}"
                )
            }
            Self::InvalidQrCode => formatter.write_str("invalid login helper QR code image"),
            Self::Json(error) => write!(formatter, "invalid login helper message: {error}"),
        }
    }
}

impl std::error::Error for ProtocolError {}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;

    use base64::Engine;

    use super::{
        CredentialPayload, LOGIN_PROTOCOL_VERSION, LoginHelperMessage, MAX_LOGIN_MESSAGE_BYTES,
        ProtocolError, decode_message, encode_message,
    };

    #[test]
    fn success_message_round_trips_without_debugging_secrets() {
        let message = LoginHelperMessage::success(
            "netease",
            CredentialPayload::Netease {
                cookies: BTreeMap::from([("MUSIC_U".to_owned(), "secret".to_owned())]),
            },
        );
        let encoded = encode_message(&message).unwrap();
        let decoded = decode_message(&encoded).unwrap();

        assert_eq!(decoded.version(), LOGIN_PROTOCOL_VERSION);
        assert!(!format!("{decoded:?}").contains("secret"));
    }

    #[test]
    fn bilibili_payload_round_trips_independent_refresh_token_and_rejects_legacy_shape() {
        let message = LoginHelperMessage::success(
            "bilibili",
            CredentialPayload::Bilibili {
                cookies: BTreeMap::from([("SESSDATA".to_owned(), "session".to_owned())]),
                refresh_token: "refresh-secret".to_owned(),
            },
        );
        let encoded = encode_message(&message).unwrap();
        let encoded_text = String::from_utf8(encoded.clone()).unwrap();
        assert!(encoded_text.contains("\"refreshToken\":\"refresh-secret\""));
        assert!(!encoded_text.contains("ac_time_value"));
        let decoded = decode_message(&encoded).unwrap();
        let LoginHelperMessage::Success { credential, .. } = decoded else {
            panic!("expected success message");
        };
        let CredentialPayload::Bilibili {
            cookies,
            refresh_token,
        } = credential
        else {
            panic!("expected Bilibili payload");
        };
        assert_eq!(refresh_token, "refresh-secret");
        assert!(!cookies.contains_key("ac_time_value"));

        let legacy = br#"{"status":"success","version":2,"provider":"bilibili","credential":{"kind":"bilibili","cookies":{"SESSDATA":"session","ac_time_value":"legacy-refresh"}}}"#;
        assert!(matches!(
            decode_message(legacy),
            Err(ProtocolError::Json(_))
        ));
    }

    #[test]
    fn decoder_rejects_multiple_lines_and_oversized_messages() {
        assert!(matches!(
            decode_message(b"{}\n{}"),
            Err(ProtocolError::NotSingleMessage)
        ));
        assert!(matches!(
            decode_message(&vec![b'x'; MAX_LOGIN_MESSAGE_BYTES + 1]),
            Err(ProtocolError::TooLarge(_))
        ));
    }

    #[test]
    fn qr_code_round_trips_without_exposing_image_in_debug_output() {
        let image = "data:image/png;base64,iVBORw0KGgoAAAANSUhEUg==";
        let message = LoginHelperMessage::qr_code("kugou", image);
        let encoded = encode_message(&message).unwrap();
        let decoded = decode_message(&encoded).unwrap();

        assert!(!decoded.is_terminal());
        assert!(!format!("{decoded:?}").contains("iVBOR"));
        assert!(matches!(
            decoded,
            LoginHelperMessage::QrCode {
                provider,
                image_data_url,
                ..
            } if provider == "kugou" && image_data_url == image
        ));
    }

    #[test]
    fn qr_code_rejects_invalid_mime_base64_signature_and_size() {
        for image in [
            "data:image/svg+xml;base64,PHN2Zz4=",
            "data:image/png;base64,not-base64!",
            "data:image/png;base64,/9j/",
        ] {
            assert!(matches!(
                encode_message(&LoginHelperMessage::qr_code("kugou", image)),
                Err(ProtocolError::InvalidQrCode)
            ));
        }

        let oversized =
            base64::engine::general_purpose::STANDARD
                .encode(vec![0_u8; super::MAX_QR_IMAGE_BYTES + 1]);
        assert!(matches!(
            encode_message(&LoginHelperMessage::qr_code(
                "kugou",
                format!("data:image/png;base64,{oversized}")
            )),
            Err(ProtocolError::InvalidQrCode)
        ));
    }

    #[test]
    fn qr_code_rejects_oversized_base64_before_decoding() {
        let oversized = "A".repeat(super::MAX_QR_IMAGE_BYTES.div_ceil(3) * 4 + 1);
        assert!(matches!(
            encode_message(&LoginHelperMessage::qr_code(
                "kugou",
                format!("data:image/png;base64,{oversized}")
            )),
            Err(ProtocolError::InvalidQrCode)
        ));
    }
}

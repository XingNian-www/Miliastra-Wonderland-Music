use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};

pub const LOGIN_PROTOCOL_VERSION: u32 = 1;
pub const MAX_LOGIN_MESSAGE_BYTES: usize = 64 * 1024;

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
    Bilibili { cookies: BTreeMap<String, String> },
}

impl CredentialPayload {
    pub const fn provider(&self) -> &'static str {
        match self {
            Self::QqMusic { .. } => "qqmusic",
            Self::Netease { .. } => "netease",
            Self::Bilibili { .. } => "bilibili",
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

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", tag = "status", deny_unknown_fields)]
pub enum LoginHelperMessage {
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
            Self::Success { version, .. } | Self::Error { version, .. } => *version,
        }
    }
}

pub fn encode_message(message: &LoginHelperMessage) -> Result<Vec<u8>, ProtocolError> {
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
    Ok(message)
}

#[derive(Debug)]
pub enum ProtocolError {
    TooLarge(usize),
    NotSingleMessage,
    UnsupportedVersion(u32),
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
            Self::Json(error) => write!(formatter, "invalid login helper message: {error}"),
        }
    }
}

impl std::error::Error for ProtocolError {}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;

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
}

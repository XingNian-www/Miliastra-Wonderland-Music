use std::collections::BTreeMap;
use std::io::Write;
use std::path::PathBuf;
use std::time::Duration;

use anyhow::{Context, Result};
use clap::Parser;
use miliastra_login_protocol::{CredentialPayload, LoginHelperMessage, encode_message};

mod native_qr;
mod webview2;

#[derive(Debug, Parser)]
#[command(about = "Capture provider credentials in a disposable WebView2 profile")]
struct Args {
    provider: String,
    #[arg(long)]
    profile: PathBuf,
    #[arg(long, default_value_t = 120)]
    timeout_seconds: u64,
}

fn main() -> Result<()> {
    let args = Args::parse();
    let mut stdout = std::io::stdout().lock();
    let message = run(&args, &mut |image_data_url| {
        write_message(
            &mut stdout,
            &LoginHelperMessage::qr_code(args.provider.clone(), image_data_url),
        )
    })
    .unwrap_or_else(|error| {
        LoginHelperMessage::error(
            args.provider.clone(),
            helper_error_code(&error),
            helper_error_message(&error),
        )
    });
    write_message(&mut stdout, &message).context("write final login helper result")?;
    Ok(())
}

fn write_message(writer: &mut dyn Write, message: &LoginHelperMessage) -> Result<()> {
    let encoded = encode_message(message).context("encode login helper result")?;
    writer
        .write_all(&encoded)
        .context("write login helper result")?;
    writer
        .write_all(b"\n")
        .context("frame login helper result")?;
    writer.flush().context("flush login helper result")?;
    Ok(())
}

fn run(
    args: &Args,
    publish_qr_code: &mut dyn FnMut(&str) -> Result<()>,
) -> Result<LoginHelperMessage> {
    let provider =
        Provider::parse(&args.provider).ok_or_else(|| anyhow::anyhow!("unsupported provider"))?;
    let profile = args
        .profile
        .canonicalize()
        .context("profile directory is unavailable")?;
    let cookies = webview2::capture(
        provider.id(),
        &profile,
        Duration::from_secs(args.timeout_seconds.max(1)),
        &mut |image_data_url| {
            publish_qr_code(image_data_url)
                .map_err(|_| webview2::CaptureError::Io("publish QR code failed".to_owned()))
        },
    )
    .context("credential capture failed")?;
    if cookies.is_empty() {
        anyhow::bail!("required credentials were not captured");
    }
    Ok(LoginHelperMessage::success(
        provider.id(),
        provider.payload(cookies),
    ))
}

fn helper_error_code(error: &anyhow::Error) -> &'static str {
    let text = error.to_string();
    if text.contains("unsupported provider") {
        "unsupported_provider"
    } else if text.contains("WebView2") || text.contains("runtime") {
        "webview_runtime_unavailable"
    } else if text.contains("timed out") {
        "login_timeout"
    } else if text.contains("closed") {
        "login_cancelled"
    } else {
        "credential_capture_failed"
    }
}

fn helper_error_message(error: &anyhow::Error) -> &'static str {
    match helper_error_code(error) {
        "unsupported_provider" => "不支持该登录平台",
        "webview_runtime_unavailable" => "WebView2 Runtime 不可用",
        "login_timeout" => "登录等待超时",
        "login_cancelled" => "登录窗口已关闭",
        _ => "未能取得有效登录凭据",
    }
}

#[derive(Clone, Copy)]
enum Provider {
    QqMusic,
    Netease,
    Bilibili,
    Kugou,
}

impl Provider {
    fn parse(value: &str) -> Option<Self> {
        match value {
            "qqmusic" => Some(Self::QqMusic),
            "netease" => Some(Self::Netease),
            "bilibili" => Some(Self::Bilibili),
            "kugou" => Some(Self::Kugou),
            _ => None,
        }
    }

    const fn id(self) -> &'static str {
        match self {
            Self::QqMusic => "qqmusic",
            Self::Netease => "netease",
            Self::Bilibili => "bilibili",
            Self::Kugou => "kugou",
        }
    }

    fn payload(self, mut cookies: BTreeMap<String, String>) -> CredentialPayload {
        match self {
            Self::QqMusic => CredentialPayload::QqMusic { cookies },
            Self::Netease => CredentialPayload::Netease { cookies },
            Self::Bilibili => CredentialPayload::Bilibili {
                refresh_token: cookies.remove("ac_time_value").unwrap_or_default(),
                cookies,
            },
            Self::Kugou => {
                // 酷狗网页登录凭据集中在 KuGoo cookie：
                // 值形如 t=<token>&KugooID=<userid>&ct=<时间戳>&...
                let fields = cookies
                    .remove("KuGoo")
                    .unwrap_or_default()
                    .split('&')
                    .filter_map(|pair| pair.split_once('='))
                    .map(|(name, value)| (name.to_owned(), value.to_owned()))
                    .collect::<BTreeMap<_, _>>();
                // The parent process supplies the persistent lite-device
                // identity. Include it in the payload so registration and
                // subsequent playback use the same device as QR polling.
                for name in ["KUGOU_API_GUID", "KUGOU_API_DEV", "KUGOU_API_MAC"] {
                    if !cookies.contains_key(name)
                        && let Ok(value) = std::env::var(name)
                        && !value.trim().is_empty()
                    {
                        let value = match name {
                            "KUGOU_API_DEV" | "KUGOU_API_MAC" => value.to_ascii_uppercase(),
                            _ => value,
                        };
                        cookies.insert(name.to_owned(), value);
                    }
                }
                CredentialPayload::Kugou {
                    token: fields.get("t").cloned().unwrap_or_default(),
                    userid: fields.get("KugooID").cloned().unwrap_or_default(),
                    cookies,
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;

    use miliastra_login_protocol::CredentialPayload;

    use super::{Provider, helper_error_code, write_message};

    #[test]
    fn provider_identifiers_are_closed_and_canonical() {
        assert_eq!(Provider::parse("qqmusic").unwrap().id(), "qqmusic");
        assert!(Provider::parse("tx").is_none());
    }

    #[test]
    fn bilibili_payload_extracts_refresh_cookie_before_encoding() {
        let payload = Provider::Bilibili.payload(BTreeMap::from([
            ("SESSDATA".to_owned(), "session".to_owned()),
            ("ac_time_value".to_owned(), "refresh".to_owned()),
        ]));
        let CredentialPayload::Bilibili {
            cookies,
            refresh_token,
        } = payload
        else {
            panic!("expected Bilibili payload");
        };
        assert_eq!(refresh_token, "refresh");
        assert!(!cookies.contains_key("ac_time_value"));
    }

    #[test]
    fn kugou_payload_parses_token_and_userid_from_ku_goo_cookie() {
        let payload = Provider::Kugou.payload(BTreeMap::from([
            (
                "KuGoo".to_owned(),
                "t=secret-token&KugooID=123456&ct=1700000000".to_owned(),
            ),
            ("dfid".to_owned(), "dfid-value".to_owned()),
        ]));
        let CredentialPayload::Kugou {
            token,
            userid,
            cookies,
        } = payload
        else {
            panic!("expected Kugou payload");
        };
        assert_eq!(token, "secret-token");
        assert_eq!(userid, "123456");
        assert!(!cookies.contains_key("KuGoo"));
        assert_eq!(cookies.get("dfid").map(String::as_str), Some("dfid-value"));
    }

    #[test]
    fn helper_errors_are_reduced_to_stable_codes() {
        assert_eq!(
            helper_error_code(&anyhow::anyhow!("unsupported provider")),
            "unsupported_provider"
        );
        assert_eq!(
            helper_error_code(&anyhow::anyhow!("cookie=secret")),
            "credential_capture_failed"
        );
    }

    #[test]
    fn helper_messages_are_flushed_as_newline_delimited_frames() {
        let mut output = Vec::new();
        write_message(
            &mut output,
            &miliastra_login_protocol::LoginHelperMessage::error(
                "kugou",
                "login_timeout",
                "timeout",
            ),
        )
        .unwrap();

        assert_eq!(output.last(), Some(&b'\n'));
        miliastra_login_protocol::decode_message(&output[..output.len() - 1]).unwrap();
    }
}

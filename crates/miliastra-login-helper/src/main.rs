use std::collections::BTreeMap;
use std::io::{Read, Write};
use std::path::PathBuf;
use std::time::Duration;

use anyhow::{Context, Result};
use clap::Parser;
use miliastra_login_protocol::{CredentialPayload, LoginHelperMessage, encode_message};
use percent_encoding::percent_decode_str;

mod webview2;

#[derive(Debug, Parser)]
#[command(about = "Capture provider credentials in a disposable WebView2 profile")]
struct Args {
    provider: String,
    #[arg(long)]
    profile: PathBuf,
    #[arg(long, default_value_t = 120)]
    timeout_seconds: u64,
    /// Run the WebView2-only KuGou request bridge instead of the QR login flow.
    #[arg(long)]
    kugou_web_request: bool,
}

fn main() -> Result<()> {
    let args = Args::parse();
    if args.kugou_web_request {
        return run_kugou_web_request(&args);
    }
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

/// The playback crate sends this small envelope over stdin so the request URL
/// and browser cookies never appear in the helper process command line.
fn run_kugou_web_request(args: &Args) -> Result<()> {
    if args.provider != "kugou" {
        return write_kugou_request_output(&serde_json::json!({
            "status": "error",
            "code": "kugou_web_request_invalid_input",
            "message": "酷狗 Web 请求参数无效",
        }));
    }
    let mut input = Vec::new();
    let read_result = std::io::stdin()
        .take(2 * 1024 * 1024)
        .read_to_end(&mut input);
    let output = match read_result {
        Err(_) => kugou_request_invalid_input_output(),
        Ok(_) => match parse_kugou_web_request_input(&input) {
            Err(_) => kugou_request_invalid_input_output(),
            Ok((url, userid, mid, cookies)) => {
                match execute_kugou_web_request(
                    &args.profile,
                    Duration::from_secs(args.timeout_seconds.max(1)),
                    &url,
                    &userid,
                    &mid,
                    &cookies,
                ) {
                    Ok(response) => serde_json::json!({
                        "status": "success",
                        "response": response,
                    }),
                    Err(error) => kugou_request_error_output(&error),
                }
            }
        },
    };
    write_kugou_request_output(&output)
}

fn execute_kugou_web_request(
    profile: &std::path::Path,
    timeout: Duration,
    url: &str,
    userid: &str,
    mid: &str,
    cookies: &BTreeMap<String, String>,
) -> std::result::Result<serde_json::Value, webview2::CaptureError> {
    std::fs::create_dir_all(profile)
        .map_err(|_| webview2::CaptureError::Io("酷狗 Web 请求 profile 不可用".to_owned()))?;
    let profile = profile
        .canonicalize()
        .map_err(|_| webview2::CaptureError::Io("酷狗 Web 请求 profile 不可用".to_owned()))?;
    webview2::request_kugou_json(&profile, timeout, url, userid, mid, cookies)
}

fn parse_kugou_web_request_input(
    input: &[u8],
) -> Result<(String, String, String, BTreeMap<String, String>)> {
    let value: serde_json::Value =
        serde_json::from_slice(input).context("酷狗 Web 请求参数不是有效 JSON")?;
    let url = value
        .get("url")
        .and_then(serde_json::Value::as_str)
        .filter(|value| !value.trim().is_empty())
        .ok_or_else(|| anyhow::anyhow!("酷狗 Web 请求缺少 url"))?
        .to_owned();
    let userid = value
        .get("userid")
        .and_then(serde_json::Value::as_str)
        .unwrap_or("0")
        .to_owned();
    let mid = value
        .get("mid")
        .and_then(serde_json::Value::as_str)
        .unwrap_or("-")
        .to_owned();
    let cookies = value
        .get("cookies")
        .cloned()
        .ok_or_else(|| anyhow::anyhow!("酷狗 Web 请求缺少 cookies"))
        .and_then(|value| {
            serde_json::from_value::<BTreeMap<String, String>>(value)
                .context("酷狗 Web 请求 cookies 格式无效")
        })?;
    Ok((url, userid, mid, cookies))
}

fn write_kugou_request_output(output: &serde_json::Value) -> Result<()> {
    let mut stdout = std::io::stdout().lock();
    serde_json::to_writer(&mut stdout, output).context("写入酷狗 Web 请求结果失败")?;
    stdout
        .write_all(b"\n")
        .context("写入酷狗 Web 请求结果换行失败")?;
    stdout.flush().context("刷新酷狗 Web 请求结果失败")?;
    Ok(())
}

fn kugou_request_invalid_input_output() -> serde_json::Value {
    serde_json::json!({
        "status": "error",
        "code": "kugou_web_request_invalid_input",
        "message": "酷狗 Web 请求参数无效",
    })
}

fn kugou_request_error_code(error: &webview2::CaptureError) -> &'static str {
    match error {
        webview2::CaptureError::RuntimeMissing => "webview_runtime_unavailable",
        webview2::CaptureError::Timeout => "kugou_web_request_timeout",
        webview2::CaptureError::Cancelled => "kugou_web_request_failed",
        webview2::CaptureError::Com(_) => "kugou_webview_error",
        webview2::CaptureError::Io(_) => "kugou_web_request_failed",
    }
}

fn kugou_request_error_message(error: &webview2::CaptureError) -> &'static str {
    match error {
        webview2::CaptureError::RuntimeMissing => "WebView2 Runtime 不可用",
        webview2::CaptureError::Timeout => "酷狗网页请求超时",
        webview2::CaptureError::Cancelled => "酷狗网页请求已取消",
        webview2::CaptureError::Com(_) => "酷狗 WebView2 请求失败",
        webview2::CaptureError::Io(_) => "酷狗网页请求失败",
    }
}

fn kugou_request_error_output(error: &webview2::CaptureError) -> serde_json::Value {
    if std::env::var_os("MILIASTRA_LOGIN_HELPER_DIAGNOSTICS").is_some() {
        eprintln!("[kugou-web-request] {error}");
    }
    serde_json::json!({
        "status": "error",
        "code": kugou_request_error_code(error),
        "message": kugou_request_error_message(error),
    })
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
    // 按根因类型分类，避免错误文本子串误匹配（如信息恰含 "closed"/"timed out"）。
    if error.chain().any(|cause| {
        matches!(
            cause.downcast_ref::<webview2::CaptureError>(),
            Some(webview2::CaptureError::RuntimeMissing)
        )
    }) {
        "webview_runtime_unavailable"
    } else if error.chain().any(|cause| {
        matches!(
            cause.downcast_ref::<webview2::CaptureError>(),
            Some(webview2::CaptureError::Timeout)
        )
    }) {
        "login_timeout"
    } else if error.chain().any(|cause| {
        matches!(
            cause.downcast_ref::<webview2::CaptureError>(),
            Some(webview2::CaptureError::Cancelled)
        )
    }) {
        "login_cancelled"
    } else if error.chain().any(|cause| {
        matches!(
            cause.downcast_ref::<webview2::CaptureError>(),
            Some(webview2::CaptureError::Com(_) | webview2::CaptureError::Io(_))
        )
    }) {
        "credential_capture_failed"
    } else if error.to_string().contains("unsupported provider") {
        // Provider::parse 的固定内部文案，无独立错误类型。
        "unsupported_provider"
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
                // Keep the exact browser value for Web requests. Parse a
                // decoded copy only to expose the token and user id fields.
                let ku_goo = cookies
                    .get("KuGoo")
                    .cloned()
                    .map(|value| {
                        if value.as_bytes().contains(&b'%') {
                            percent_decode_str(&value).decode_utf8_lossy().into_owned()
                        } else {
                            value
                        }
                    })
                    .unwrap_or_default();
                let fields = ku_goo
                    .split('&')
                    .filter_map(|pair| pair.split_once('='))
                    .map(|(name, value)| (name.to_owned(), value.to_owned()))
                    .collect::<BTreeMap<_, _>>();
                // These were emitted by an older helper build as an
                // intermediate QR ticket. Never persist them as a credential.
                cookies.remove("KUGOU_QR_TOKEN");
                cookies.remove("KUGOU_QR_USERID");
                // The parent process supplies the persistent lite-device
                // identity. Include it in the payload so registration and
                // subsequent playback use the same device.
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

    use super::{
        Provider, helper_error_code, kugou_request_error_output,
        kugou_request_invalid_input_output, parse_kugou_web_request_input, write_message,
    };

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
        assert_eq!(
            cookies.get("KuGoo").map(String::as_str),
            Some("t=secret-token&KugooID=123456&ct=1700000000")
        );
        assert_eq!(cookies.get("dfid").map(String::as_str), Some("dfid-value"));
    }

    #[test]
    fn kugou_payload_decodes_web_cookie_value_before_parsing() {
        let payload = Provider::Kugou.payload(BTreeMap::from([(
            "KuGoo".to_owned(),
            "t%3Dsecret-token%26KugooID%3D123456%26ct%3D1700000000".to_owned(),
        )]));
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
        assert_eq!(
            cookies.get("KuGoo").map(String::as_str),
            Some("t%3Dsecret-token%26KugooID%3D123456%26ct%3D1700000000")
        );
    }

    #[test]
    fn kugou_payload_ignores_qr_poll_credential_and_uses_web_cookie() {
        let payload = Provider::Kugou.payload(BTreeMap::from([
            (
                "KuGoo".to_owned(),
                "t=web-token&KugooID=111111&ct=1700000000".to_owned(),
            ),
            ("KUGOU_QR_TOKEN".to_owned(), "lite-token".to_owned()),
            ("KUGOU_QR_USERID".to_owned(), "222222".to_owned()),
        ]));
        let CredentialPayload::Kugou {
            token,
            userid,
            cookies,
        } = payload
        else {
            panic!("expected Kugou payload");
        };
        assert_eq!(token, "web-token");
        assert_eq!(userid, "111111");
        assert_eq!(
            cookies.get("KuGoo").map(String::as_str),
            Some("t=web-token&KugooID=111111&ct=1700000000")
        );
        assert!(!cookies.contains_key("KUGOU_QR_TOKEN"));
        assert!(!cookies.contains_key("KUGOU_QR_USERID"));
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
        let timed_out = anyhow::Error::new(super::webview2::CaptureError::Timeout)
            .context("credential capture failed");
        assert_eq!(helper_error_code(&timed_out), "login_timeout");
        let closed = anyhow::Error::new(super::webview2::CaptureError::Cancelled)
            .context("credential capture failed");
        assert_eq!(helper_error_code(&closed), "login_cancelled");
        // 错误信息恰含敏感词也不影响类型化分类。
        let com = anyhow::Error::new(super::webview2::CaptureError::Com(
            "timed out while closed".to_owned(),
        ))
        .context("credential capture failed");
        assert_eq!(helper_error_code(&com), "credential_capture_failed");
    }

    #[test]
    fn kugou_web_request_input_requires_url_and_cookie_object() {
        assert!(parse_kugou_web_request_input(br#"{}"#).is_err());
        assert!(
            parse_kugou_web_request_input(br#"{"url":"https://wwwapi.kugou.com/play/songinfo"}"#)
                .is_err()
        );
        assert!(
            parse_kugou_web_request_input(
                br#"{"url":"https://wwwapi.kugou.com/play/songinfo","cookies":[]}"#
            )
            .is_err()
        );
    }

    #[test]
    fn kugou_web_request_input_preserves_fields_and_applies_safe_defaults() {
        let (url, userid, mid, cookies) = parse_kugou_web_request_input(
            br#"{"url":"https://wwwapi.kugou.com/play/songinfo?a=1","cookies":{"KuGoo":"session"}}"#,
        )
        .unwrap();
        assert_eq!(url, "https://wwwapi.kugou.com/play/songinfo?a=1");
        assert_eq!(userid, "0");
        assert_eq!(mid, "-");
        assert_eq!(cookies.get("KuGoo").map(String::as_str), Some("session"));
    }

    #[test]
    fn kugou_web_request_error_envelopes_are_stable_and_secret_free() {
        let invalid = kugou_request_invalid_input_output();
        assert_eq!(invalid["status"], "error");
        assert_eq!(invalid["code"], "kugou_web_request_invalid_input");

        let timeout = kugou_request_error_output(&super::webview2::CaptureError::Timeout);
        assert_eq!(timeout["code"], "kugou_web_request_timeout");
        assert_eq!(timeout["message"], "酷狗网页请求超时");
        assert!(!timeout.to_string().contains("secret"));
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

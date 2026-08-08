use std::collections::BTreeMap;
use std::io::Write;
use std::path::PathBuf;
use std::time::Duration;

use anyhow::{Context, Result};
use clap::Parser;
use miliastra_login_protocol::{CredentialPayload, LoginHelperMessage, encode_message};

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
    let message = run(&args).unwrap_or_else(|error| {
        LoginHelperMessage::error(
            args.provider.clone(),
            helper_error_code(&error),
            helper_error_message(&error),
        )
    });
    let encoded = encode_message(&message).context("encode login helper result")?;
    let mut stdout = std::io::stdout().lock();
    stdout
        .write_all(&encoded)
        .context("write login helper result")?;
    stdout.flush().context("flush login helper result")?;
    Ok(())
}

fn run(args: &Args) -> Result<LoginHelperMessage> {
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
}

impl Provider {
    fn parse(value: &str) -> Option<Self> {
        match value {
            "qqmusic" => Some(Self::QqMusic),
            "netease" => Some(Self::Netease),
            "bilibili" => Some(Self::Bilibili),
            _ => None,
        }
    }

    const fn id(self) -> &'static str {
        match self {
            Self::QqMusic => "qqmusic",
            Self::Netease => "netease",
            Self::Bilibili => "bilibili",
        }
    }

    fn payload(self, cookies: BTreeMap<String, String>) -> CredentialPayload {
        match self {
            Self::QqMusic => CredentialPayload::QqMusic { cookies },
            Self::Netease => CredentialPayload::Netease { cookies },
            Self::Bilibili => CredentialPayload::Bilibili { cookies },
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{Provider, helper_error_code};

    #[test]
    fn provider_identifiers_are_closed_and_canonical() {
        assert_eq!(Provider::parse("qqmusic").unwrap().id(), "qqmusic");
        assert!(Provider::parse("tx").is_none());
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
}

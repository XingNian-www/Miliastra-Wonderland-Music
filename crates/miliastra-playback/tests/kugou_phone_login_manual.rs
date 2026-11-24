//! 概念版（lite）手机号验证码登录的**手动**真机验证测试。
//!
//! 这个测试默认永远不会运行（`#[ignore]`），只有人显式加 `--ignored` 才执行。
//! 它会真实访问酷狗接口，其中「发送验证码」会真实下发短信，因此绝不能进入
//! CI 或任何自动流程。
//!
//! 用法（两步，必须由人手动执行）：
//!
//! ```text
//! # 第一步：发一次验证码（会真实下发短信，只发一次，无重试）
//! export PATH=/root/.cargo/bin:/usr/local/bin:$PATH
//! MOBILE=<11位手机号> CREDENTIAL_DIR=<真实凭据目录> \
//!   cargo test -p miliastra-playback --test kugou_phone_login_manual \
//!   -- --ignored --nocapture send_code
//!
//! # 第二步：用收到的验证码完成登录
//! MOBILE=<同一手机号> CODE=<收到的验证码> CREDENTIAL_DIR=<真实凭据目录> \
//!   cargo test -p miliastra-playback --test kugou_phone_login_manual \
//!   -- --ignored --nocapture login
//! ```
//!
//! `CREDENTIAL_DIR` 必须是软件真实凭据目录：登录拿到的 `t1` 会绑定该目录下
//! `kugou-device.json` 里的 GUID/MAC/机型，用错设备会导致后续续期对不上。
//!
//! 输出中的手机号、令牌、t1 一律打码。

use std::collections::BTreeMap;
use std::path::PathBuf;
use std::time::Duration;

use miliastra_playback::{CredentialStore, KugouAdapter};

fn credential_dir() -> PathBuf {
    std::env::var_os("CREDENTIAL_DIR")
        .map(PathBuf::from)
        .expect("必须设置 CREDENTIAL_DIR（软件真实凭据目录）")
}

fn mobile() -> String {
    std::env::var("MOBILE").expect("必须设置 MOBILE（11 位手机号）")
}

fn mask(value: &str) -> String {
    let chars = value.chars().collect::<Vec<_>>();
    if chars.is_empty() {
        return "<空>".to_owned();
    }
    if chars.len() <= 6 {
        return "*".repeat(chars.len());
    }
    let head = chars.iter().take(3).collect::<String>();
    let tail = chars.iter().skip(chars.len() - 2).collect::<String>();
    format!("{head}***{tail}(len={})", chars.len())
}

fn presence(cookies: &BTreeMap<String, String>, name: &str) -> bool {
    cookies
        .get(name)
        .is_some_and(|value| !value.trim().is_empty())
}

/// 校验设备标识存在且不是占位值，避免把 `t1` 绑到假设备上。
fn assert_real_device(directory: &PathBuf) {
    let path = directory.join("kugou-device.json");
    let text = std::fs::read_to_string(&path).unwrap_or_else(|error| {
        panic!(
            "缺少设备标识 {}（{error}）：请先在软件设置页填写酷狗设备 GUID/型号/MAC",
            path.display()
        )
    });
    let device: serde_json::Value = serde_json::from_str(&text)
        .unwrap_or_else(|error| panic!("{} 不是合法 JSON: {error}", path.display()));
    let guid = device
        .get("guid")
        .and_then(serde_json::Value::as_str)
        .unwrap_or_default();
    let dev = device
        .get("dev")
        .and_then(serde_json::Value::as_str)
        .unwrap_or_default();
    let mac = device
        .get("mac")
        .and_then(serde_json::Value::as_str)
        .unwrap_or_default();
    assert!(
        !guid.trim().is_empty() && !dev.trim().is_empty() && !mac.trim().is_empty(),
        "设备标识字段不能为空"
    );
    println!("设备标识: {}", path.display());
    println!("  将绑定的 GUID(打码): {}", mask(guid));
    println!("  将绑定的 机型       : {dev}");
    println!("  将绑定的 MAC(打码)  : {}", mask(mac));

    let placeholder = guid.eq_ignore_ascii_case("a1b2c3d4e5f60718293a4b5c6d7e8f90")
        || dev.eq_ignore_ascii_case("TESTDEV")
        || mac.eq_ignore_ascii_case("AA:BB:CC:DD:EE:FF");
    assert!(
        !placeholder,
        "当前设备标识是测试占位值：用它登录会把 t1 绑到假设备，后续续期必然失败。\
         请把 CREDENTIAL_DIR 指向软件真实凭据目录。"
    );
}

fn adapter(directory: PathBuf) -> (CredentialStore, KugouAdapter) {
    let store = CredentialStore::open(directory).expect("打开凭据目录失败");
    let adapter =
        KugouAdapter::new(store.clone(), Duration::from_secs(20)).expect("构造适配器失败");
    (store, adapter)
}

/// 第一步：发送一次验证码。**会真实下发短信，不自动重试。**
#[tokio::test]
#[ignore = "真机验证：会真实发送短信，仅允许人工执行"]
async fn send_code() {
    let directory = credential_dir();
    assert_real_device(&directory);
    let (_, adapter) = adapter(directory);
    let mobile = mobile();
    println!("\n=== 发送验证码（只发一次，无重试）===");
    println!("手机号(打码): {}", mask(&mobile));
    adapter
        .send_login_code(&mobile)
        .await
        .expect("发送验证码失败");
    println!("已发送。请查收短信，然后用 MOBILE + CODE 再跑 login 测试。");
}

/// 第二步：用验证码完成登录，并打印（打码后的）凭据能力。
#[tokio::test]
#[ignore = "真机验证：会真实登录，仅允许人工执行"]
async fn login() {
    let directory = credential_dir();
    assert_real_device(&directory);
    let (store, adapter) = adapter(directory.clone());
    let mobile = mobile();
    let code = std::env::var("CODE").expect("必须设置 CODE（收到的验证码）");

    println!("\n=== 验证码登录 ===");
    println!("手机号(打码): {}", mask(&mobile));
    let credential = adapter
        .login_by_verifycode(&mobile, &code)
        .await
        .expect("验证码登录失败");

    let cookies = credential.cookies();
    let (token, userid, dfid) = credential.kugou_fields().expect("不是酷狗凭据");
    println!("登录成功，凭据字段就位情况：");
    println!("  token            present={}", !token.trim().is_empty());
    println!(
        "  userid           present={} ({})",
        !userid.trim().is_empty(),
        mask(userid)
    );
    println!(
        "  dfid             present={} ({})",
        !dfid.trim().is_empty(),
        mask(dfid)
    );
    println!(
        "  t1               present={}  ← 续期能力的关键字段",
        presence(cookies, "t1")
    );
    println!(
        "  KUGOU_API_TOKEN  present={}  ← 续期能力的关键字段",
        presence(cookies, "KUGOU_API_TOKEN")
    );
    println!(
        "  vip_token        present={}",
        presence(cookies, "vip_token")
    );
    println!(
        "  vip_type         present={}",
        presence(cookies, "vip_type")
    );
    println!("  token(打码)      {}", mask(token));

    store.save("kugou", credential.clone()).expect("落盘失败");
    println!("\n凭据已落盘: {}", directory.display());

    // 关键验证：这次续期是否真的换到了新令牌（不再 20018）。
    println!("\n=== 续期验证（关键）===");
    match adapter.refresh_token(&credential).await {
        Ok(refreshed) => {
            let (new_token, _, _) = refreshed.kugou_fields().expect("不是酷狗凭据");
            println!("续期成功：token 是否变化={}", new_token != token);
            println!("  ← true 表示真正换到了新令牌，掉登录问题解决");
        }
        Err(error) => {
            println!("续期未成功：{error}\n  ← 若为 RefreshUnavailable，说明该凭据仍不可续期")
        }
    }
}

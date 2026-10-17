use super::*;
use crate::debug_read_only::{ReadOnlyState, observer};
use sha2::{Digest, Sha256};

fn bridge_state() -> HttpTestState {
    let mut state = test_state();
    state.application.debug = Some(Arc::new(HttpTestDebugPort {
        sandbox: crate::debug_http_service::DebugHttpService::default(),
    }));
    state
}

#[test]
fn bridge_summary_auth_unknown_state_and_server_identity_are_consistent() {
    let mut state = bridge_state();
    let server = start_test_http_server(&mut state, "bridge-test");
    let address = server.local_addr();
    for path in [
        "/debug/read-only/summary",
        "/debug/read-only/image?id=readonly-1",
    ] {
        assert_eq!(
            http_get(address, path, None).status_line,
            "HTTP/1.1 401 Unauthorized"
        );
    }
    let before = observer().bus().latest_sequence();
    let mode: Value =
        serde_json::from_str(&http_get(address, "/debug/read-only/mode", Some("bridge-test")).body)
            .unwrap();
    let response = http_get(address, "/debug/read-only/summary", Some("bridge-test"));
    assert_eq!(response.status_line, "HTTP/1.1 200 OK");
    let value: Value = serde_json::from_str(&response.body).unwrap();
    assert_eq!(value["summary"]["instance"], mode["mode"]["instance"]);
    assert_eq!(value["summary"]["ui"]["kind"], Value::Null);
    assert_eq!(value["summary"]["ui"]["stable"], Value::Null);
    assert_eq!(value["summary"]["ui"]["ageMillis"], Value::Null);
    assert_eq!(value["summary"]["queue"]["length"], Value::Null);
    assert!(!response.body.contains("imagePngBase64"));
    let entries = http_get(address, "/debug/read-only/entries", Some("bridge-test"));
    let entries: Value = serde_json::from_str(&entries.body).unwrap();
    assert_eq!(entries["entries"]["instance"], mode["mode"]["instance"]);
    assert_eq!(
        observer().bus().latest_sequence(),
        before,
        "queries must not publish events"
    );
    server.shutdown().unwrap();
}

pub(super) fn section_http(
    address: SocketAddr,
    id: &str,
    section: &str,
    mut revision: Option<u64>,
) -> (Value, u64, String) {
    let mut cursor = 0u64;
    let mut output = String::new();
    let mut instance = String::new();
    loop {
        let query = format!(
            "/debug/read-only/evidence?id={id}&section={section}&cursor={cursor}&limit=2&maxBytes=1024{}",
            revision
                .map(|value| format!("&revision={value}"))
                .unwrap_or_default()
        );
        let response = http_get(address, &query, Some("bridge-test"));
        assert_eq!(response.status_line, "HTTP/1.1 200 OK", "{}", response.body);
        assert!(response.body.len() <= 1024, "whole envelope exceeds budget");
        assert!(
            !response.body.contains("imagePngBase64"),
            "GET evidence cannot inline image"
        );
        let value: Value = serde_json::from_str(&response.body).unwrap();
        assert_eq!(value["ok"], true, "{value}");
        let evidence = &value["evidence"];
        let current = evidence["revision"].as_u64().unwrap();
        assert!(revision.is_none_or(|expected| expected == current));
        revision = Some(current);
        if instance.is_empty() {
            instance = evidence["instance"].as_str().unwrap().to_string();
        }
        assert_eq!(evidence["instance"], instance);
        for item in evidence["page"]["items"].as_array().unwrap() {
            output.push_str(item["text"].as_str().unwrap());
        }
        let next = evidence["page"]["nextCursor"].as_u64().unwrap();
        if evidence["page"]["done"] == true {
            break;
        }
        assert!(next > cursor, "page cannot stall");
        cursor = next;
    }
    (
        serde_json::from_str(&output).unwrap(),
        revision.unwrap(),
        instance,
    )
}

pub(super) fn binary_http(
    address: SocketAddr,
    target: &str,
) -> (String, HashMap<String, String>, Vec<u8>) {
    let mut stream = TcpStream::connect(address).unwrap();
    stream
        .set_read_timeout(Some(Duration::from_secs(5)))
        .unwrap();
    write!(stream,"GET {target} HTTP/1.1\r\nHost: localhost\r\nX-Miliastra-Token: bridge-test\r\nConnection: close\r\n\r\n").unwrap();
    let mut raw = Vec::new();
    stream.read_to_end(&mut raw).unwrap();
    let boundary = raw
        .windows(4)
        .position(|bytes| bytes == b"\r\n\r\n")
        .unwrap();
    let head = String::from_utf8(raw[..boundary].to_vec()).unwrap();
    let mut lines = head.split("\r\n");
    let status = lines.next().unwrap().to_string();
    let headers = lines
        .map(|line| line.split_once(':').unwrap())
        .map(|(key, value)| (key.to_ascii_lowercase(), value.trim().to_string()))
        .collect();
    (status, headers, raw[boundary + 4..].to_vec())
}

#[test]
#[ignore = "uses actual retained screenshot fixture and global observer; isolated, no game capture/input"]
fn bridge_partitioned_real_retained_png_revision_budget_and_legacy_post() {
    struct Restore(bool, ReadOnlyState);
    impl Drop for Restore {
        fn drop(&mut self) {
            observer().set_enabled_state(self.0, self.1);
        }
    }
    let _restore = Restore(observer().enabled(), observer().state());
    let fixture = "technical-docs/actual-screenshot-test/live/live-01.png";
    let image = image::open(fixture).expect("existing genuine screenshot");
    let expected_pixel = format!("{:x}", Sha256::digest(image.as_bytes()));
    struct FixtureDevice(DynamicImage);
    impl crate::runtime::ui::UiDevice for FixtureDevice {
        fn capture(&mut self) -> Result<DynamicImage> {
            Ok(self.0.clone())
        }
    }
    let runtime = crate::runtime::ui::UiRuntime::start(FixtureDevice(image), 8).unwrap();
    let mut state = bridge_state();
    state.application.debug = Some(Arc::new(HttpTestDebugPort {
        sandbox: crate::debug_http_service::DebugHttpService::new(Some(runtime.handle())),
    }));
    let server = start_test_http_server(&mut state, "bridge-test");
    let address = server.local_addr();
    let mode = http_post_json(
        address,
        "/debug/read-only/mode",
        r#"{"enabled":true,"state":"active"}"#,
        Some("bridge-test"),
    );
    assert_eq!(mode.status_line, "HTTP/1.1 200 OK");
    let legacy = http_post_json(
        address,
        "/debug/read-only/read",
        r#"{"ocr":false,"label":"legacy-full"}"#,
        Some("bridge-test"),
    );
    let legacy: Value = serde_json::from_str(&legacy.body).unwrap();
    assert!(
        legacy["evidence"]["source"]["imagePngBase64"]
            .as_str()
            .is_some(),
        "old page remains compatible"
    );
    let text = "原图取证晴天🙂é漢字\n\t\"".repeat(4);
    let meta = http_post_json(
        address,
        "/debug/read-only/read",
        &json!({"ocr":false,"label":text,"response":"metadata"}).to_string(),
        Some("bridge-test"),
    );
    assert_eq!(meta.status_line, "HTTP/1.1 200 OK", "{}", meta.body);
    assert!(!meta.body.contains("imagePngBase64"));
    let meta: Value = serde_json::from_str(&meta.body).unwrap();
    let id = meta["evidence"]["id"].as_str().unwrap();
    let tiny = http_get(
        address,
        &format!("/debug/read-only/evidence?id={id}&section=parameters&maxBytes=8&cursor=0"),
        Some("bridge-test"),
    );
    let tiny: Value = serde_json::from_str(&tiny.body).unwrap();
    assert_eq!(tiny["ok"], false);
    assert_eq!(tiny["nextCursor"], 0);
    let (metadata, revision, instance) = section_http(address, id, "metadata", None);
    let (parameters, _, _) = section_http(address, id, "parameters", Some(revision));
    let (history, _, _) = section_http(address, id, "history", Some(revision));
    let (ocr, _, _) = section_http(address, id, "ocr", Some(revision));
    assert!(parameters.to_string().contains("原图取证"));
    // 旧 GET 兼容：不带 section 仍是与旧 POST 完整响应同形的**完整证据**
    // （旧客户端和推送 evidenceUrl 依赖它），带 section 才分页、才不内联 PNG。
    let legacy_full = http_get(
        address,
        &format!("/debug/read-only/evidence?id={id}"),
        Some("bridge-test"),
    );
    assert_eq!(
        legacy_full.status_line, "HTTP/1.1 200 OK",
        "{}",
        legacy_full.body
    );
    let legacy_full: Value = serde_json::from_str(&legacy_full.body).unwrap();
    assert_eq!(legacy_full["ok"], true);
    let full = &legacy_full["evidence"];
    assert_eq!(full["id"], id);
    assert_eq!(full["kind"], "active");
    assert_eq!(full["task"]["request"]["label"], text);
    assert_eq!(full["task"]["ocr"]["requested"], false);
    assert!(
        full["status"].is_null(),
        "active evidence has no workflow status to fake"
    );
    assert_eq!(full["history"].as_array().unwrap().len(), 0);
    assert_eq!(full["replay"]["executesBusinessActions"], false);
    assert_eq!(full["source"]["pixelSha256"], expected_pixel);
    assert!(
        full.get("page").is_none() && full.get("section").is_none(),
        "legacy full package is not a section page"
    );
    let inline = full["source"]["imagePngBase64"]
        .as_str()
        .expect("no-section GET keeps the legacy inline PNG");
    assert!(inline.starts_with("iVBOR"));
    let inline_bytes = crate::debug_http_service::base64_decode(inline).unwrap();
    assert_eq!(
        format!("{:x}", Sha256::digest(&inline_bytes)),
        full["source"]["fileSha256"]
    );
    assert_eq!(
        format!(
            "{:x}",
            Sha256::digest(image::load_from_memory(&inline_bytes).unwrap().as_bytes())
        ),
        expected_pixel
    );
    // 显式分区才是分页包：同一份证据、同样逐页断言不出内联 PNG。
    let (metadata_again, metadata_revision, _) =
        section_http(address, id, "metadata", Some(revision));
    assert_eq!(metadata_again, metadata);
    assert_eq!(metadata_revision, revision);
    // 版本检查对旧完整 GET 同样成立，且不靠抓新帧绕过。
    let stale_full = http_get(
        address,
        &format!(
            "/debug/read-only/evidence?id={id}&revision={}",
            revision + 1
        ),
        Some("bridge-test"),
    );
    assert_eq!(
        stale_full.status_line, "HTTP/1.1 409 Conflict",
        "{}",
        stale_full.body
    );
    // instance 校验先于 section：不带 section 也一样。
    let stale_instance = http_get(
        address,
        &format!("/debug/read-only/evidence?id={id}&instance=old-instance"),
        Some("bridge-test"),
    );
    assert_eq!(stale_instance.status_line, "HTTP/1.1 409 Conflict");
    // section 显式出现但为空是无效分区，不能回退成完整包。
    let empty_section = http_get(
        address,
        &format!("/debug/read-only/evidence?id={id}&section="),
        Some("bridge-test"),
    );
    assert_eq!(
        empty_section.status_line, "HTTP/1.1 400 Bad Request",
        "{}",
        empty_section.body
    );
    assert!(!empty_section.body.contains("imagePngBase64"));
    let empty_section: Value = serde_json::from_str(&empty_section.body).unwrap();
    assert_eq!(empty_section["ok"], false);
    assert!(
        empty_section["error"]
            .as_str()
            .unwrap()
            .starts_with("invalid_section")
    );
    // 不存在的证据如实报错，不用新帧或最近一帧顶替。
    let missing = http_get(
        address,
        "/debug/read-only/evidence?id=readonly-999999999",
        Some("bridge-test"),
    );
    assert_eq!(
        missing.status_line, "HTTP/1.1 404 Not Found",
        "{}",
        missing.body
    );
    let missing: Value = serde_json::from_str(&missing.body).unwrap();
    assert_eq!(missing["ok"], false);
    assert!(
        missing["error"]
            .as_str()
            .unwrap()
            .starts_with("evidence_not_found"),
        "{}",
        missing
    );
    let (status, headers, png) = binary_http(
        address,
        &format!("/debug/read-only/image?id={id}&instance={instance}&revision={revision}"),
    );
    assert_eq!(status, "HTTP/1.1 200 OK");
    assert_eq!(headers["content-type"], "image/png");
    assert_eq!(headers["x-readonly-instance"], instance);
    assert_eq!(headers["x-readonly-revision"], revision.to_string());
    assert_eq!(headers["x-pixel-sha256"], expected_pixel);
    assert_eq!(
        headers["x-file-sha256"],
        format!("{:x}", Sha256::digest(&png))
    );
    assert_eq!(
        format!(
            "{:x}",
            Sha256::digest(image::load_from_memory(&png).unwrap().as_bytes())
        ),
        expected_pixel
    );
    let old_instance = http_get(
        address,
        &format!("/debug/read-only/evidence?id={id}&instance=old-instance&revision={revision}"),
        Some("bridge-test"),
    );
    assert_eq!(old_instance.status_line, "HTTP/1.1 409 Conflict");
    let bad = http_get(
        address,
        &format!(
            "/debug/read-only/evidence?id={id}&section=metadata&revision={}",
            revision + 1
        ),
        Some("bridge-test"),
    );
    assert_eq!(bad.status_line, "HTTP/1.1 409 Conflict");
    let bad_image = http_get(
        address,
        &format!("/debug/read-only/image?id={id}&instance=old-instance"),
        Some("bridge-test"),
    );
    assert_eq!(bad_image.status_line, "HTTP/1.1 409 Conflict");
    let output = PathBuf::from("technical-docs/sandbox-bridge-mainline/evidence");
    fs::create_dir_all(&output).unwrap();
    fs::write(output.join("retained-source.png"), &png).unwrap();
    fs::write(
        output.join("partitions.json"),
        serde_json::to_vec_pretty(
            &json!({"metadata":metadata,"parameters":parameters,"history":history,"ocr":ocr}),
        )
        .unwrap(),
    )
    .unwrap();
    fs::write(output.join("provenance.json"),serde_json::to_vec_pretty(&json!({"fixture":fixture,"actualGameCaptureDuringTest":false,
        "actualBusinessCommandExecuted":false,"layer":"authenticated HTTP through retained genuine pixels; mock device; no inference",
        "instance":instance,"revision":revision,"pixelSha256":expected_pixel,"fileSha256":headers["x-file-sha256"]})).unwrap()).unwrap();
    server.shutdown().unwrap();
    runtime.shutdown().unwrap();
}

#[test]
#[ignore = "global read-only event bus; isolated; no input/capture"]
fn bridge_sse_gap_reset_lagged_and_idle_comments_are_visible() {
    use tokio_stream::StreamExt;
    let mut state = bridge_state();
    let server = start_test_http_server(&mut state, "bridge-test");
    let bus = observer().bus();
    for n in 0..300 {
        bus.publish(
            "readonly.task.updated",
            json!({"id":format!("fixture-{n}"),"instance":observer().instance()}),
        );
    }
    let retained = http_get(
        server.local_addr(),
        "/debug/read-only/events?drain=true&since=0",
        Some("bridge-test"),
    );
    assert!(retained.body.contains("event: readonly.gap"));
    assert!(retained.body.contains("replay_retention_gap"));
    let reset = http_get(
        server.local_addr(),
        "/debug/read-only/events?drain=true&since=999999999&instance=old",
        Some("bridge-test"),
    );
    assert!(reset.body.contains("instance_or_cursor_reset"));
    state.config.http.access_token.clear();
    let request = Request {
        method: "GET".into(),
        path: "/debug/read-only/events".into(),
        query: vec![("since".into(), bus.latest_sequence().to_string())],
        headers: HeaderMap::new(),
        body: vec![],
    };
    let rt = Builder::new_current_thread().enable_all().build().unwrap();
    rt.block_on(async move {
        let response = debug_read_only_events_response_for_test(request, &state);
        let mut body = response.into_body().into_data_stream();
        let first = body.next().await.unwrap().unwrap();
        assert!(String::from_utf8_lossy(&first).contains(": instance"));
        let sequence = bus.latest_sequence();
        let heartbeat = tokio::time::timeout(Duration::from_secs(2), body.next())
            .await
            .unwrap()
            .unwrap()
            .unwrap();
        let heartbeat = String::from_utf8_lossy(&heartbeat);
        assert!(
            heartbeat.contains(": heartbeat"),
            "idle connection receives comments"
        );
        assert!(!heartbeat.contains("data:"));
        assert_eq!(
            bus.latest_sequence(),
            sequence,
            "heartbeat must not create a producer event"
        );
        for n in 0..300 {
            bus.publish("readonly.task.updated", json!({"id":format!("lag-{n}")}));
        }
        let gap = tokio::time::timeout(Duration::from_secs(2), body.next())
            .await
            .unwrap()
            .unwrap()
            .unwrap();
        assert!(
            String::from_utf8_lossy(&gap).contains("subscriber_lagged"),
            "slow subscriber must see lag loss"
        );
    });
    server.shutdown().unwrap();
}

fn debug_read_only_events_response_for_test(request: Request, state: &HttpSharedState) -> Response {
    debug_events_response(&request, state).unwrap()
}

/// 旧 GET 兼容里**不需要任何证据、也不改全局观测状态**的那部分契约：
/// 缺 id、空 section、未知 id、instance 冲突。它和本文件第一条 HTTP 测试一样是普通
/// 测试（不开启只读模式、不写证据、不发布事件），随默认测试套件运行。
/// 正例（完整包 / 分页包）必须写证据，必然改动进程级 observer 与事件总线，
/// 因此放在 isolated ignored 回归里，层级与本文件既有的 SSE 测试一致。
#[test]
fn bridge_legacy_evidence_error_layer_and_instance_order_without_section() {
    let mut state = bridge_state();
    let server = start_test_http_server(&mut state, "bridge-test");
    let address = server.local_addr();
    let missing_id = http_get(address, "/debug/read-only/evidence", Some("bridge-test"));
    assert_eq!(
        missing_id.status_line, "HTTP/1.1 400 Bad Request",
        "{}",
        missing_id.body
    );
    let missing_id: Value = serde_json::from_str(&missing_id.body).unwrap();
    assert_eq!(missing_id["ok"], false);
    assert!(
        missing_id["error"]
            .as_str()
            .unwrap()
            .contains("缺少查询参数 id")
    );
    // 不带 section 的未知 id：如实 404，不返回任何证据包，也不用新帧顶替。
    let unknown = http_get(
        address,
        "/debug/read-only/evidence?id=bridge-missing-evidence",
        Some("bridge-test"),
    );
    let unknown_body = unknown.body.clone();
    assert_eq!(
        unknown.status_line, "HTTP/1.1 404 Not Found",
        "{unknown_body}"
    );
    let unknown: Value = serde_json::from_str(&unknown.body).unwrap();
    assert_eq!(unknown["ok"], false);
    assert!(
        unknown["error"]
            .as_str()
            .unwrap()
            .starts_with("evidence_not_found"),
        "{unknown_body}"
    );
    assert!(!unknown_body.contains("imagePngBase64"));
    assert!(unknown.get("evidence").is_none());
    // 显式分区的未知 id 同样 404。
    let unknown_section = http_get(
        address,
        "/debug/read-only/evidence?id=bridge-missing-evidence&section=metadata",
        Some("bridge-test"),
    );
    assert_eq!(unknown_section.status_line, "HTTP/1.1 404 Not Found");
    // 空 section 在查证据之前就判定为无效分区，优先于 not-found。
    let empty_section = http_get(
        address,
        "/debug/read-only/evidence?id=bridge-missing-evidence&section=",
        Some("bridge-test"),
    );
    assert_eq!(
        empty_section.status_line, "HTTP/1.1 400 Bad Request",
        "{}",
        empty_section.body
    );
    let empty_section: Value = serde_json::from_str(&empty_section.body).unwrap();
    assert!(
        empty_section["error"]
            .as_str()
            .unwrap()
            .starts_with("invalid_section")
    );
    // instance 校验先于分区与版本：不存在的 id 也先报 409，而不是 404。
    let stale_instance = http_get(
        address,
        "/debug/read-only/evidence?id=bridge-missing-evidence&instance=bridge-other-instance",
        Some("bridge-test"),
    );
    assert_eq!(stale_instance.status_line, "HTTP/1.1 409 Conflict");
    let stale_instance: Value = serde_json::from_str(&stale_instance.body).unwrap();
    assert!(
        stale_instance["error"]
            .as_str()
            .unwrap()
            .starts_with("instance_conflict")
    );
    server.shutdown().unwrap();
}

/// 非视觉证据（无原帧）的旧 GET 完整包与显式分区包，全部走真实 HTTP 入口。
/// 这条回归不截图、不编码 PNG、不新增测试专用状态，只核对既有 DTO：
/// 无 section 必须给 task/history/status 完整字段，显式 section 才是分页信封。
#[test]
#[ignore = "mutates the global read-only observer mode and event bus; isolated; no capture/input"]
fn bridge_legacy_full_get_and_explicit_section_for_nonvisual_evidence() {
    struct Restore(bool, ReadOnlyState);
    impl Drop for Restore {
        fn drop(&mut self) {
            observer().set_enabled_state(self.0, self.1);
        }
    }
    let _restore = Restore(observer().enabled(), observer().state());
    let mut state = bridge_state();
    let server = start_test_http_server(&mut state, "bridge-test");
    let address = server.local_addr();
    let mode = http_post_json(
        address,
        "/debug/read-only/mode",
        r#"{"enabled":true,"state":"push"}"#,
        Some("bridge-test"),
    );
    assert_eq!(mode.status_line, "HTTP/1.1 200 OK", "{}", mode.body);
    let before: Value =
        serde_json::from_str(&http_get(address, "/debug/read-only/mode", Some("bridge-test")).body)
            .unwrap();
    let sequence = observer().bus().latest_sequence();
    // 非视觉来源：没有精确捕获时间，因此不应该有任何源帧。
    observer().record_trigger(
        4242,
        json!({"kind":"command","sourceKind":"nonVisualInput",
            "parameters":{"keyword":"晴天"},"raw":"@点歌晴天"}),
        None,
    );
    let id = observer()
        .bus()
        .replay_after(sequence)
        .last()
        .expect("a tracked trigger publishes exactly one event")
        .payload["id"]
        .as_str()
        .unwrap()
        .to_owned();
    let full = http_get(
        address,
        &format!("/debug/read-only/evidence?id={id}"),
        Some("bridge-test"),
    );
    assert_eq!(full.status_line, "HTTP/1.1 200 OK", "{}", full.body);
    let full: Value = serde_json::from_str(&full.body).unwrap();
    assert_eq!(full["ok"], true);
    let evidence = &full["evidence"];
    assert_eq!(evidence["id"], id);
    assert_eq!(evidence["kind"], "trigger");
    assert_eq!(evidence["taskId"], 4242);
    assert_eq!(evidence["task"]["parameters"]["keyword"], "晴天");
    assert_eq!(evidence["status"]["status"], "queued");
    assert_eq!(evidence["history"].as_array().unwrap().len(), 1);
    assert_eq!(evidence["history"][0]["kind"], "readonly.triggered");
    assert_eq!(evidence["source"], Value::Null);
    assert_eq!(
        evidence["missingReason"],
        "non_visual_input_has_no_source_frame"
    );
    assert!(
        evidence.get("page").is_none() && evidence.get("section").is_none(),
        "no-section GET must stay the legacy full package"
    );
    let page = http_get(
        address,
        &format!(
            "/debug/read-only/evidence?id={id}&section=metadata&cursor=0&limit=10&maxBytes=8192"
        ),
        Some("bridge-test"),
    );
    assert_eq!(page.status_line, "HTTP/1.1 200 OK", "{}", page.body);
    assert!(!page.body.contains("imagePngBase64"));
    let page: Value = serde_json::from_str(&page.body).unwrap();
    assert_eq!(page["ok"], true);
    assert_eq!(page["evidence"]["id"], id);
    assert_eq!(page["evidence"]["section"], "metadata");
    assert_eq!(page["evidence"]["page"]["encoding"], "json-utf8");
    assert_eq!(page["evidence"]["page"]["cursor"], 0);
    assert_eq!(page["evidence"]["page"]["done"], true);
    assert_eq!(
        page["evidence"]["page"]["revision"],
        page["evidence"]["revision"]
    );
    let metadata: Value = serde_json::from_str(
        &page["evidence"]["page"]["items"]
            .as_array()
            .unwrap()
            .iter()
            .map(|item| item["text"].as_str().unwrap())
            .collect::<String>(),
    )
    .unwrap();
    assert_eq!(metadata["id"], id);
    assert_eq!(metadata["status"]["status"], "queued");
    let (parameters, _, _) = section_http(address, &id, "parameters", None);
    assert_eq!(parameters["task"]["parameters"]["keyword"], "晴天");
    assert!(parameters["task"].get("ocr").is_none());
    let (history, _, _) = section_http(address, &id, "history", None);
    assert_eq!(history["history"][0]["kind"], "readonly.triggered");
    let (ocr, _, _) = section_http(address, &id, "ocr", None);
    assert_eq!(ocr, Value::Null);
    let empty = http_get(
        address,
        &format!("/debug/read-only/evidence?id={id}&section="),
        Some("bridge-test"),
    );
    assert_eq!(
        empty.status_line, "HTTP/1.1 400 Bad Request",
        "{}",
        empty.body
    );
    assert!(!empty.body.contains("imagePngBase64"));
    let empty: Value = serde_json::from_str(&empty.body).unwrap();
    assert_eq!(empty["ok"], false);
    assert!(
        empty["error"]
            .as_str()
            .unwrap()
            .starts_with("invalid_section")
    );
    // 未知 section 名字同样是无效分区，不回退成完整包。
    let bogus = http_get(
        address,
        &format!("/debug/read-only/evidence?id={id}&section=everything"),
        Some("bridge-test"),
    );
    assert_eq!(
        bogus.status_line, "HTTP/1.1 400 Bad Request",
        "{}",
        bogus.body
    );
    assert!(!bogus.body.contains("imagePngBase64"));
    let bogus: Value = serde_json::from_str(&bogus.body).unwrap();
    assert!(
        bogus["error"]
            .as_str()
            .unwrap()
            .starts_with("invalid_section")
    );
    let stale = http_get(
        address,
        &format!("/debug/read-only/evidence?id={id}&revision=999"),
        Some("bridge-test"),
    );
    assert_eq!(stale.status_line, "HTTP/1.1 409 Conflict", "{}", stale.body);
    // 只读查询不会抓帧：捕获与丢弃计数在整轮 HTTP 往返后不变。
    let after: Value =
        serde_json::from_str(&http_get(address, "/debug/read-only/mode", Some("bridge-test")).body)
            .unwrap();
    for field in ["frames", "frameBytes"] {
        assert_eq!(
            after["mode"]["memory"][field], before["mode"]["memory"][field],
            "evidence queries must not capture frames: {field}"
        );
    }
    for field in ["droppedFrames", "rejectedFrames", "droppedImages"] {
        assert_eq!(after["mode"][field], before["mode"][field], "{field}");
    }
    server.shutdown().unwrap();
}

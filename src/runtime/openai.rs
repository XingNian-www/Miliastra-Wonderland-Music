use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, MutexGuard, mpsc};
use std::time::Duration;

use anyhow::{Context, Result, anyhow, bail};
use async_openai::Client;
use async_openai::error::OpenAIError;
use async_openai::middleware::ReqwestService;
use async_openai::types::chat::CreateChatCompletionRequest;
use async_openai::types::responses::CreateResponse;
use serde::Serialize;
use serde_json::Value;
use tokio::runtime::{Builder, Runtime};

pub(crate) use crate::adapters::ai_http::{Authentication, Target};

const SHUTDOWN_TIMEOUT: Duration = Duration::from_secs(5);
const WORKER_THREADS: usize = 2;

pub(crate) struct OpenAiRuntime {
    runtime: Option<Runtime>,
    handle: OpenAiRuntimeHandle,
}

#[derive(Clone)]
pub(crate) struct OpenAiRuntimeHandle {
    runtime: tokio::runtime::Handle,
    http: reqwest::Client,
    state: Arc<OpenAiRuntimeState>,
}

struct OpenAiRuntimeState {
    accepting: AtomicBool,
    submit_lock: Mutex<()>,
}

pub(crate) struct OpenAiOperation {
    receiver: mpsc::Receiver<Result<Value>>,
    response_name: &'static str,
}

impl OpenAiRuntime {
    pub(crate) fn start() -> Result<Self> {
        let runtime = Builder::new_multi_thread()
            .worker_threads(WORKER_THREADS)
            .enable_all()
            .thread_name("openai-runtime")
            .build()
            .context("启动 OpenAI runtime 失败")?;
        let http = reqwest::Client::builder()
            .build()
            .context("创建 OpenAI HTTP 客户端失败")?;
        let state = Arc::new(OpenAiRuntimeState {
            accepting: AtomicBool::new(true),
            submit_lock: Mutex::new(()),
        });
        let handle = OpenAiRuntimeHandle {
            runtime: runtime.handle().clone(),
            http,
            state,
        };
        Ok(Self {
            runtime: Some(runtime),
            handle,
        })
    }

    pub(crate) fn handle(&self) -> OpenAiRuntimeHandle {
        self.handle.clone()
    }

    pub(crate) fn shutdown(mut self) {
        self.stop();
    }

    fn stop(&mut self) {
        if let Ok(_guard) = self.handle.state.submit_lock.lock() {
            self.handle.state.accepting.store(false, Ordering::Release);
        } else {
            self.handle.state.accepting.store(false, Ordering::Release);
        }
        if let Some(runtime) = self.runtime.take() {
            runtime.shutdown_timeout(SHUTDOWN_TIMEOUT);
        }
    }
}

impl Drop for OpenAiRuntime {
    fn drop(&mut self) {
        self.stop();
    }
}

impl OpenAiRuntimeHandle {
    pub(crate) fn chat_completion(
        &self,
        target: Target,
        request: CreateChatCompletionRequest,
        extra_body: &HashMap<String, Value>,
        timeout: Duration,
    ) -> Result<OpenAiOperation> {
        self.submit_chat(
            target,
            merge_extra_body(&request, extra_body)?,
            validate_timeout(timeout)?,
        )
    }

    pub(crate) fn create_response(
        &self,
        target: Target,
        request: CreateResponse,
        extra_body: &HashMap<String, Value>,
        timeout: Duration,
    ) -> Result<OpenAiOperation> {
        self.submit_response(
            target,
            merge_extra_body(&request, extra_body)?,
            validate_timeout(timeout)?,
        )
    }

    fn begin_submission(&self) -> Result<MutexGuard<'_, ()>> {
        let guard = self
            .state
            .submit_lock
            .lock()
            .map_err(|_| anyhow!("OpenAI runtime 提交锁已损坏"))?;
        if !self.state.accepting.load(Ordering::Acquire) {
            bail!("OpenAI runtime 已停止");
        }
        Ok(guard)
    }

    fn submit_chat(
        &self,
        target: Target,
        request: Value,
        timeout: Duration,
    ) -> Result<OpenAiOperation> {
        let _submission = self.begin_submission()?;
        let (sender, receiver) = mpsc::sync_channel(1);
        let http = self.http.clone();
        self.runtime.spawn(async move {
            let service = ReqwestService::new(http.clone());
            let client = Client::build(http, target.config).with_http_service(service);
            let result =
                match tokio::time::timeout(timeout, client.chat().create_byot::<_, Value>(request))
                    .await
                {
                    Ok(result) => result.map_err(sdk_error),
                    Err(_) => Err(anyhow!("OpenAI Chat Completions 请求超时")),
                };
            let _ = sender.send(result);
        });
        Ok(OpenAiOperation {
            receiver,
            response_name: "Chat Completions",
        })
    }

    fn submit_response(
        &self,
        target: Target,
        request: Value,
        timeout: Duration,
    ) -> Result<OpenAiOperation> {
        let _submission = self.begin_submission()?;
        let (sender, receiver) = mpsc::sync_channel(1);
        let http = self.http.clone();
        self.runtime.spawn(async move {
            let service = ReqwestService::new(http.clone());
            let client = Client::build(http, target.config).with_http_service(service);
            let result = match tokio::time::timeout(
                timeout,
                client.responses().create_byot::<_, Value>(request),
            )
            .await
            {
                Ok(result) => result.map_err(sdk_error),
                Err(_) => Err(anyhow!("OpenAI Responses 请求超时")),
            };
            let _ = sender.send(result);
        });
        Ok(OpenAiOperation {
            receiver,
            response_name: "Responses",
        })
    }
}

impl OpenAiOperation {
    pub(crate) fn wait(self) -> Result<Value> {
        self.receiver
            .recv()
            .with_context(|| format!("OpenAI runtime 在返回 {} 结果前停止", self.response_name))?
    }
}

fn validate_timeout(timeout: Duration) -> Result<Duration> {
    if timeout.is_zero() {
        bail!("OpenAI 请求超时必须大于 0");
    }
    Ok(timeout)
}

fn merge_extra_body<T: Serialize>(
    standard_request: &T,
    extra_body: &HashMap<String, Value>,
) -> Result<Value> {
    let standard = serde_json::to_value(standard_request).context("序列化 OpenAI 标准请求失败")?;
    let standard = standard
        .as_object()
        .ok_or_else(|| anyhow!("OpenAI 标准请求必须是 JSON object"))?;
    let mut merged = extra_body
        .iter()
        .map(|(key, value)| (key.clone(), value.clone()))
        .collect::<serde_json::Map<_, _>>();
    for (key, value) in standard {
        merged.insert(key.clone(), value.clone());
    }
    Ok(Value::Object(merged))
}

fn sdk_error(error: OpenAIError) -> anyhow::Error {
    match error {
        OpenAIError::ApiError(response) => {
            let error_type = sanitized_error_token(response.api_error.r#type.as_deref());
            let code = sanitized_error_token(response.api_error.code.as_deref());
            anyhow!(
                "OpenAI API 响应失败 status={} type={} code={}",
                response.status_code,
                error_type,
                code
            )
        }
        OpenAIError::Reqwest(error) => {
            let category = if error.is_timeout() {
                "timeout"
            } else if error.is_connect() {
                "connect"
            } else if error.is_request() {
                "request"
            } else if error.is_body() {
                "body"
            } else if error.is_decode() {
                "decode"
            } else {
                "transport"
            };
            let status = error
                .status()
                .map_or_else(|| "none".to_string(), |status| status.to_string());
            anyhow!("OpenAI HTTP 请求失败 category={category} status={status}")
        }
        OpenAIError::JSONDeserialize(_error, _body) => {
            anyhow!("OpenAI API 响应不是有效 JSON")
        }
        OpenAIError::InvalidArgument(_message) => anyhow!("OpenAI 请求参数无效"),
        _ => anyhow!("OpenAI SDK 请求失败"),
    }
}

fn sanitized_error_token(value: Option<&str>) -> &str {
    value
        .filter(|value| {
            !value.is_empty()
                && value.len() <= 64
                && value
                    .bytes()
                    .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'_' | b'-' | b'.'))
        })
        .unwrap_or("unknown")
}

#[cfg(test)]
mod tests {
    use super::*;
    use async_openai::config::Config;
    use std::io::{Read, Write};
    use std::net::TcpListener;
    use std::sync::{Arc, Mutex};
    use std::thread;
    use std::time::Instant;

    use async_openai::types::chat::{
        ChatCompletionRequestUserMessageArgs, CreateChatCompletionRequestArgs,
    };
    use reqwest::header::AUTHORIZATION;

    #[test]
    fn target_preserves_full_custom_resource_url_and_auth_scheme() {
        let bearer = Target::chat(
            "https://gateway.example/compatible/v1/chat/completions?tenant=a",
            "secret",
            Authentication::Bearer,
        )
        .expect("bearer target");
        assert_eq!(
            bearer.config.url("/ignored"),
            "https://gateway.example/compatible/v1/chat/completions?tenant=a"
        );
        assert_eq!(bearer.config.headers()[AUTHORIZATION], "Bearer secret");
        assert!(!bearer.config.headers().contains_key("api-key"));

        let api_key = Target::chat(
            "https://gateway.example/v1/chat/completions",
            "secret",
            Authentication::ApiKey,
        )
        .expect("api-key target");
        assert_eq!(api_key.config.headers()["api-key"], "secret");
        assert!(!api_key.config.headers().contains_key(AUTHORIZATION));
        assert_eq!(
            secrecy::ExposeSecret::expose_secret(api_key.config.secret_key()),
            "secret"
        );
    }

    #[test]
    fn target_rejects_wrong_resource_and_unsafe_urls() {
        assert!(Target::responses("https://example.com/v1/chat/completions", "secret").is_err());
        assert!(Target::responses("file:///tmp/responses", "secret").is_err());
        assert!(Target::responses("https://user:pass@example.com/v1/responses", "secret").is_err());
        assert!(Target::responses("https://example.com/v1/responses#x", "secret").is_err());
    }

    #[test]
    fn sdk_transport_uses_exact_endpoint_and_api_key_header() {
        let runtime = OpenAiRuntime::start().expect("OpenAI runtime");
        let (origin, requests, server) =
            mock_server(200, r#"{"choices":[]}"#, Duration::from_millis(300), 32);
        let target = Target::chat(
            &format!("{origin}/custom/v1/chat/completions?tenant=a"),
            "secret",
            Authentication::ApiKey,
        )
        .expect("target");

        let response = runtime
            .handle()
            .chat_completion(
                target,
                chat_request(),
                &HashMap::new(),
                Duration::from_secs(2),
            )
            .expect("submit chat request")
            .wait()
            .expect("chat response");
        assert_eq!(response, serde_json::json!({ "choices": [] }));
        server.join().expect("mock server");

        let requests = requests.lock().expect("captured requests");
        assert_eq!(requests.len(), 1);
        let request = requests[0].to_ascii_lowercase();
        assert!(request.starts_with("post /custom/v1/chat/completions?tenant=a http/1.1"));
        assert!(request.contains("\r\napi-key: secret\r\n"));
        assert!(!request.contains("\r\nauthorization:"));
    }

    #[test]
    fn sdk_transport_never_retries_server_errors() {
        let runtime = OpenAiRuntime::start().expect("OpenAI runtime");
        let (origin, requests, server) = mock_server(
            500,
            r#"{"error":{"message":"retry forbidden","type":"server_error"}}"#,
            Duration::from_millis(900),
            32,
        );
        let target = Target::chat(
            &format!("{origin}/v1/chat/completions?tenant=sensitive-query"),
            "secret",
            Authentication::Bearer,
        )
        .expect("target");

        let error = runtime
            .handle()
            .chat_completion(
                target,
                chat_request(),
                &HashMap::new(),
                Duration::from_secs(2),
            )
            .expect("submit chat request")
            .wait()
            .expect_err("server error");
        assert!(error.to_string().contains("status=500"));
        assert!(!error.to_string().contains("sensitive-query"));
        assert!(!error.to_string().contains("retry forbidden"));
        server.join().expect("mock server");
        assert_eq!(requests.lock().expect("captured requests").len(), 1);
    }

    #[test]
    fn sdk_transport_applies_the_per_call_timeout() {
        let runtime = OpenAiRuntime::start().expect("OpenAI runtime");
        let listener = TcpListener::bind("127.0.0.1:0").expect("bind slow server");
        listener
            .set_nonblocking(true)
            .expect("nonblocking slow server");
        let address = listener.local_addr().expect("slow server address");
        let server = thread::spawn(move || {
            let deadline = Instant::now() + Duration::from_millis(300);
            loop {
                match listener.accept() {
                    Ok((mut stream, _)) => {
                        stream
                            .set_nonblocking(false)
                            .expect("blocking slow connection");
                        stream
                            .set_read_timeout(Some(Duration::from_millis(100)))
                            .expect("slow connection read timeout");
                        let mut buffer = [0_u8; 16 * 1024];
                        let _ = stream.read(&mut buffer);
                        thread::sleep(Duration::from_millis(150));
                        break;
                    }
                    Err(error)
                        if error.kind() == std::io::ErrorKind::WouldBlock
                            && Instant::now() < deadline =>
                    {
                        thread::sleep(Duration::from_millis(1));
                    }
                    Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => break,
                    Err(error) => panic!("slow server failed: {error}"),
                }
            }
        });
        let target = Target::chat(
            &format!("http://{address}/v1/chat/completions"),
            "secret",
            Authentication::Bearer,
        )
        .expect("target");

        let error = runtime
            .handle()
            .chat_completion(
                target,
                chat_request(),
                &HashMap::new(),
                Duration::from_millis(20),
            )
            .expect("submit chat request")
            .wait()
            .expect_err("timeout");

        assert!(error.to_string().contains("请求超时"));
        server.join().expect("slow server");
    }

    #[test]
    fn standard_fields_override_explicit_compatibility_fields() {
        let extra = HashMap::from([
            (
                "model".to_string(),
                Value::String("vendor-model".to_string()),
            ),
            (
                "thinking".to_string(),
                serde_json::json!({ "type": "disabled" }),
            ),
            ("stream".to_string(), Value::Bool(true)),
        ]);

        let body = merge_extra_body(&chat_request(), &extra).expect("merged body");

        assert_eq!(body["model"], "test-model");
        assert_eq!(body["stream"], false);
        assert_eq!(body["thinking"]["type"], "disabled");
    }

    #[test]
    fn shutdown_rejects_new_operations_from_cloned_handles() {
        let runtime = OpenAiRuntime::start().expect("OpenAI runtime");
        let handle = runtime.handle();

        runtime.shutdown();

        let target = Target::chat(
            "https://example.com/v1/chat/completions",
            "secret",
            Authentication::Bearer,
        )
        .expect("target");
        let error = handle
            .chat_completion(
                target,
                chat_request(),
                &HashMap::new(),
                Duration::from_secs(1),
            )
            .err()
            .expect("stopped runtime must reject work");
        assert!(error.to_string().contains("runtime 已停止"));
    }

    #[test]
    fn malformed_response_errors_do_not_echo_provider_content() {
        let parse_error = serde_json::from_str::<Value>("{").expect_err("invalid json");
        let error = sdk_error(OpenAIError::JSONDeserialize(
            parse_error,
            "sensitive provider content".to_string(),
        ));

        assert!(error.to_string().contains("响应不是有效 JSON"));
        assert!(!error.to_string().contains("sensitive provider content"));
    }

    #[test]
    fn api_errors_only_expose_standard_status_type_and_code() {
        let error = sdk_error(OpenAIError::ApiError(
            async_openai::error::ApiErrorResponse {
                status_code: reqwest::StatusCode::BAD_REQUEST,
                api_error: async_openai::error::ApiError {
                    message: "sensitive provider message".to_string(),
                    r#type: Some("invalid_request_error".to_string()),
                    param: Some("sensitive-param".to_string()),
                    code: Some("bad_request".to_string()),
                },
            },
        ));
        let text = error.to_string();

        assert!(text.contains("status=400"));
        assert!(text.contains("type=invalid_request_error"));
        assert!(text.contains("code=bad_request"));
        assert!(!text.contains("sensitive provider message"));
        assert!(!text.contains("sensitive-param"));
    }

    fn chat_request() -> CreateChatCompletionRequest {
        CreateChatCompletionRequestArgs::default()
            .model("test-model")
            .messages(vec![
                ChatCompletionRequestUserMessageArgs::default()
                    .content("test")
                    .build()
                    .expect("message")
                    .into(),
            ])
            .stream(false)
            .store(false)
            .build()
            .expect("chat request")
    }

    fn mock_server(
        status: u16,
        body: &'static str,
        lifetime: Duration,
        read_chunk_size: usize,
    ) -> (String, Arc<Mutex<Vec<String>>>, thread::JoinHandle<()>) {
        let listener = TcpListener::bind("127.0.0.1:0").expect("bind mock server");
        listener.set_nonblocking(true).expect("nonblocking server");
        let address = listener.local_addr().expect("mock address");
        let requests = Arc::new(Mutex::new(Vec::new()));
        let captured = Arc::clone(&requests);
        let server = thread::spawn(move || {
            let deadline = Instant::now() + lifetime;
            while Instant::now() < deadline {
                match listener.accept() {
                    Ok((mut stream, _)) => {
                        stream
                            .set_nonblocking(false)
                            .expect("blocking mock connection");
                        stream
                            .set_read_timeout(Some(Duration::from_secs(2)))
                            .expect("mock read timeout");
                        let request = read_http_request(&mut stream, read_chunk_size);
                        captured
                            .lock()
                            .expect("capture request")
                            .push(String::from_utf8_lossy(&request).into_owned());
                        let reason = if status == 200 {
                            "OK"
                        } else {
                            "Internal Server Error"
                        };
                        let response = format!(
                            "HTTP/1.1 {status} {reason}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                            body.len()
                        );
                        stream
                            .write_all(response.as_bytes())
                            .expect("write mock response");
                    }
                    Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                        thread::sleep(Duration::from_millis(10));
                    }
                    Err(error) => panic!("mock server failed: {error}"),
                }
            }
        });
        (format!("http://{address}"), requests, server)
    }

    fn read_http_request(stream: &mut impl Read, read_chunk_size: usize) -> Vec<u8> {
        assert!(read_chunk_size > 0, "mock read chunk must be non-zero");
        let mut request = Vec::new();
        let mut chunk = vec![0_u8; read_chunk_size];
        loop {
            let read = stream.read(&mut chunk).expect("read mock request");
            if read == 0 {
                break;
            }
            request.extend_from_slice(&chunk[..read]);
            if let Some(expected_len) = http_request_len(&request)
                && request.len() >= expected_len
            {
                request.truncate(expected_len);
                break;
            }
        }
        request
    }

    fn http_request_len(request: &[u8]) -> Option<usize> {
        let header_end = request
            .windows(4)
            .position(|window| window == b"\r\n\r\n")?;
        let headers = std::str::from_utf8(&request[..header_end]).expect("ASCII request headers");
        let body_len = headers
            .lines()
            .find_map(|line| {
                let (name, value) = line.split_once(':')?;
                name.eq_ignore_ascii_case("content-length").then(|| {
                    value
                        .trim()
                        .parse::<usize>()
                        .expect("numeric content length")
                })
            })
            .unwrap_or(0);
        Some(header_end + 4 + body_len)
    }
}

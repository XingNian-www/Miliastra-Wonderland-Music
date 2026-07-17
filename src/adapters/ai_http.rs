use anyhow::{Context, Result, bail};
use async_openai::config::Config;
use reqwest::header::{AUTHORIZATION, HeaderMap, HeaderName, HeaderValue};
use secrecy::SecretString;
use url::Url;

const CHAT_COMPLETIONS_PATH: &str = "/chat/completions";
const RESPONSES_PATH: &str = "/responses";

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Authentication {
    Bearer,
    ApiKey,
}

#[derive(Clone)]
pub(crate) struct Target {
    pub(crate) config: EndpointConfig,
}

impl Target {
    pub(crate) fn chat(endpoint: &str, api_key: &str, auth: Authentication) -> Result<Self> {
        Self::new(endpoint, api_key, auth, CHAT_COMPLETIONS_PATH)
    }

    pub(crate) fn responses(endpoint: &str, api_key: &str) -> Result<Self> {
        Self::new(endpoint, api_key, Authentication::Bearer, RESPONSES_PATH)
    }

    fn new(endpoint: &str, api_key: &str, auth: Authentication, path: &str) -> Result<Self> {
        let endpoint = normalize_endpoint(endpoint, path)?;
        let api_key = api_key.trim();
        if api_key.is_empty() {
            bail!("OpenAI API Key 未配置");
        }
        let mut headers = HeaderMap::new();
        match auth {
            Authentication::Bearer => {
                headers.insert(
                    AUTHORIZATION,
                    HeaderValue::from_str(&format!("Bearer {api_key}"))
                        .context("OpenAI API Key 不是有效 HTTP header")?,
                );
            }
            Authentication::ApiKey => {
                headers.insert(
                    HeaderName::from_static("api-key"),
                    HeaderValue::from_str(api_key)
                        .context("OpenAI API Key 不是有效 HTTP header")?,
                );
            }
        }
        Ok(Self {
            config: EndpointConfig {
                endpoint,
                api_key: SecretString::from(api_key),
                headers,
            },
        })
    }
}

#[derive(Clone)]
pub(crate) struct EndpointConfig {
    endpoint: String,
    api_key: SecretString,
    headers: HeaderMap,
}

impl Config for EndpointConfig {
    fn headers(&self) -> HeaderMap {
        self.headers.clone()
    }

    fn url(&self, _path: &str) -> String {
        self.endpoint.clone()
    }

    fn query(&self) -> Vec<(&str, &str)> {
        Vec::new()
    }

    fn api_base(&self) -> &str {
        &self.endpoint
    }

    fn api_key(&self) -> &SecretString {
        &self.api_key
    }
}

impl EndpointConfig {
    #[cfg(test)]
    pub(crate) fn secret_key(&self) -> &SecretString {
        &self.api_key
    }
}

fn normalize_endpoint(endpoint: &str, expected_path: &str) -> Result<String> {
    let endpoint = endpoint.trim();
    if endpoint.is_empty() {
        bail!("OpenAI endpoint 未配置");
    }
    let mut url = Url::parse(endpoint).context("OpenAI endpoint 格式无效")?;
    if !matches!(url.scheme(), "http" | "https") || url.host_str().is_none() {
        bail!("OpenAI endpoint 必须是完整的 HTTP(S) 地址");
    }
    if !url.username().is_empty() || url.password().is_some() {
        bail!("OpenAI endpoint 不能包含用户名或密码");
    }
    if url.fragment().is_some() {
        bail!("OpenAI endpoint 不能包含 fragment");
    }
    let normalized_path = url.path().trim_end_matches('/').to_string();
    if !normalized_path.ends_with(expected_path) {
        bail!("OpenAI endpoint 必须以 {expected_path} 结尾");
    }
    url.set_path(&normalized_path);
    Ok(url.to_string())
}

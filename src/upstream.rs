use crate::{
    config::Upstream,
    pool::{Lease, Pool},
};
use anyhow::{anyhow, bail, Context, Result};
use reqwest::{redirect::Policy, Client, StatusCode, Url};
use serde_json::Value;
use std::{collections::HashMap, sync::Arc, time::Duration};

pub struct Transport {
    pub client: Client,
    pub pools: HashMap<String, Arc<Pool>>,
}
pub struct UpstreamResponse {
    pub response: reqwest::Response,
    pub lease: Option<Lease>,
    pub is_sse: bool,
}

impl Transport {
    pub async fn new(upstreams: &[Upstream], timeout: u64) -> Result<Self> {
        let client = Client::builder()
            .redirect(Policy::none())
            .connect_timeout(Duration::from_secs(30))
            .timeout(Duration::from_secs(timeout))
            .build()?;
        let mut pools = HashMap::new();
        for up in upstreams.iter().filter(|u| u.kind == "cnb") {
            let pool = Pool::new(
                &up.base_url,
                up.pool_min,
                up.pool_max,
                Duration::from_secs(up.ttl_minutes * 60),
                Duration::from_secs(timeout),
                &up.session_cookie,
            )
            .await
            .with_context(|| format!("initializing CNB upstream {}", up.name))?;
            pools.insert(up.name.clone(), pool);
        }
        Ok(Self { client, pools })
    }
    pub async fn send(
        &self,
        up: &Upstream,
        model: &str,
        body: &Value,
        stream: bool,
        passthrough_key: Option<&str>,
    ) -> Result<UpstreamResponse> {
        self.send_with_endpoint(up, model, body, stream, passthrough_key, false)
            .await
    }

    pub async fn send_native(
        &self,
        up: &Upstream,
        model: &str,
        body: &Value,
        stream: bool,
        passthrough_key: Option<&str>,
        responses_api: bool,
    ) -> Result<UpstreamResponse> {
        if up.kind == "cnb" {
            bail!("CNB does not support native tools");
        }
        self.send_with_endpoint(up, model, body, stream, passthrough_key, responses_api)
            .await
    }

    async fn send_with_endpoint(
        &self,
        up: &Upstream,
        model: &str,
        body: &Value,
        stream: bool,
        passthrough_key: Option<&str>,
        responses_api: bool,
    ) -> Result<UpstreamResponse> {
        if up.kind == "cnb" {
            let pool = self
                .pools
                .get(&up.name)
                .context("CNB credential pool unavailable")?;
            let (response, lease) = pool.chat(body).await?;
            return Ok(UpstreamResponse {
                response,
                lease: Some(lease),
                is_sse: true,
            });
        }
        let base = up.base_url.trim_end_matches('/');
        let url = match up.kind.as_str() {
            "anthropic" => format!("{}/v1/messages", base.strip_suffix("/v1").unwrap_or(base)),
            "gemini" => {
                let mut u = Url::parse(&format!("{base}/v1beta/models/"))?;
                u.path_segments_mut()
                    .map_err(|_| anyhow!("invalid Gemini base URL"))?
                    .pop_if_empty()
                    .push(&format!(
                        "{model}:{}",
                        if stream {
                            "streamGenerateContent"
                        } else {
                            "generateContent"
                        }
                    ));
                if stream {
                    u.query_pairs_mut().append_pair("alt", "sse");
                }
                u.to_string()
            }
            _ if responses_api => format!("{base}/responses"),
            _ => format!("{base}/chat/completions"),
        };
        let mut request = self.client.post(url).json(body);
        for (name, value) in &up.headers {
            request = request.header(name, value);
        }
        let key = passthrough_key
            .filter(|s| !s.is_empty())
            .unwrap_or(&up.api_key);
        if !key.is_empty() {
            request = match up.kind.as_str() {
                "anthropic" => request.header("x-api-key", key),
                "gemini" => request.header("x-goog-api-key", key),
                _ => request.bearer_auth(key),
            };
        }
        if up.kind == "anthropic"
            && !up
                .headers
                .keys()
                .any(|s| s.eq_ignore_ascii_case("anthropic-version"))
        {
            request = request.header("anthropic-version", "2023-06-01");
        }
        if stream {
            request = request.header("accept", "text/event-stream");
        }
        let response = request
            .send()
            .await
            .map_err(|_| anyhow!("upstream connection failed"))?;
        if !response.status().is_success() {
            bail!(UpstreamStatus(response.status()));
        }
        Ok(UpstreamResponse {
            response,
            lease: None,
            is_sse: stream,
        })
    }
}

#[derive(Debug)]
pub struct UpstreamStatus(pub StatusCode);
impl std::fmt::Display for UpstreamStatus {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "upstream returned HTTP {}", self.0)
    }
}
impl std::error::Error for UpstreamStatus {}

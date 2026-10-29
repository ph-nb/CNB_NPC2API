use anyhow::{anyhow, bail, Context, Result};
use futures_util::{future::join_all, StreamExt};
use regex::Regex;
use reqwest::{
    cookie::{CookieStore, Jar},
    Client, Url,
};
use serde_json::{json, Value};
use std::{
    sync::{Arc, Mutex},
    time::{Duration, Instant},
};
use uuid::Uuid;

const UA: &str = "Mozilla/5.0 (Windows NT 10.0; Win64; x64) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/126.0 Safari/537.36";

struct Token {
    id: Uuid,
    cookies: String,
    value: String,
    created: Instant,
    errors: u8,
    in_use: usize,
}

#[derive(Default)]
struct Entries {
    tokens: Vec<Token>,
    next: usize,
    pending: usize,
}

pub struct Pool {
    entries: Mutex<Entries>,
    pub base: Url,
    pub client: Client,
    min: usize,
    max: usize,
    ttl: Duration,
    session_cookie: String,
}

pub struct Lease {
    pool: Arc<Pool>,
    id: Uuid,
    pub cookies: String,
    pub value: String,
    failed: bool,
}

struct Pending<'a> {
    pool: &'a Pool,
    active: bool,
}
impl Drop for Pending<'_> {
    fn drop(&mut self) {
        if self.active {
            self.pool.entries.lock().unwrap().pending -= 1;
        }
    }
}

impl Lease {
    pub fn fail(&mut self) {
        self.failed = true;
    }
}

impl Drop for Lease {
    fn drop(&mut self) {
        let mut entries = self.pool.entries.lock().unwrap();
        if let Some(token) = entries.tokens.iter_mut().find(|token| token.id == self.id) {
            token.in_use = token.in_use.saturating_sub(1);
            if self.failed {
                token.errors = token.errors.saturating_add(1);
            } else {
                token.errors = 0;
            }
        }
    }
}

impl Pool {
    pub async fn new(
        base: &str,
        min: usize,
        max: usize,
        ttl: Duration,
        timeout: Duration,
        session_cookie: &str,
    ) -> Result<Arc<Self>> {
        let base = Url::parse(&format!("{}/", base.trim_end_matches('/')))?;
        let pool = Arc::new(Self {
            entries: Mutex::new(Entries::default()),
            base,
            client: Client::builder()
                .redirect(reqwest::redirect::Policy::none())
                .timeout(timeout)
                .user_agent(UA)
                .build()?,
            min,
            max,
            ttl,
            session_cookie: session_cookie.to_owned(),
        });
        let results = join_all((0..min).map(|_| pool.fetch())).await;
        {
            let mut entries = pool.entries.lock().unwrap();
            for token in results.into_iter().flatten() {
                entries.tokens.push(token);
            }
            if entries.tokens.is_empty() {
                bail!("failed to acquire any CNB CSRF credential");
            }
        }
        let maintenance = Arc::downgrade(&pool);
        tokio::spawn(async move {
            let mut tick = tokio::time::interval(Duration::from_secs(60));
            tick.tick().await;
            loop {
                tick.tick().await;
                let Some(pool) = maintenance.upgrade() else {
                    break;
                };
                pool.maintain().await;
            }
        });
        Ok(pool)
    }

    async fn fetch(&self) -> Result<Token> {
        let jar = Arc::new(Jar::default());
        if !self.session_cookie.is_empty() {
            for item in cookie::Cookie::split_parse(&self.session_cookie) {
                let cookie = item.context("invalid CNB session cookie")?;
                jar.add_cookie_str(&cookie.to_string(), &self.base);
            }
        }
        let client = Client::builder()
            .cookie_provider(jar.clone())
            .timeout(Duration::from_secs(15))
            .user_agent(UA)
            .build()?;
        let mut response = client
            .get(self.base.clone())
            .send()
            .await
            .context("fetch CNB homepage")?
            .error_for_status()?;
        let mut bytes = Vec::new();
        while let Some(chunk) = response.chunk().await.context("read CNB homepage")? {
            if bytes.len().saturating_add(chunk.len()) > 8 * 1024 * 1024 {
                bail!("CNB homepage exceeds 8 MiB");
            }
            bytes.extend_from_slice(&chunk);
        }
        let html = String::from_utf8(bytes).context("CNB homepage is not UTF-8")?;
        let re = Regex::new(r#"window\.csrftoken\s*=\s*["']([0-9a-fA-F]{32,64})["']"#)?;
        let value = re
            .captures(&html)
            .and_then(|m| m.get(1))
            .ok_or_else(|| anyhow!("CNB CSRF token missing"))?
            .as_str()
            .to_string();
        let cookies = jar
            .cookies(&self.base)
            .ok_or_else(|| anyhow!("CNB CSRF cookie missing"))?;
        let cookies = cookies.to_str()?;
        let valid_cookie = cookie::Cookie::split_parse(cookies)
            .filter_map(Result::ok)
            .any(|cookie| cookie.name() == "csrfkey" && !cookie.value().is_empty());
        if !valid_cookie {
            bail!("CNB CSRF cookie missing");
        }
        Ok(Token {
            id: Uuid::new_v4(),
            cookies: cookies.to_owned(),
            value,
            created: Instant::now(),
            errors: 0,
            in_use: 0,
        })
    }

    fn take(self: &Arc<Self>, entries: &mut Entries, index: usize) -> Lease {
        let token = &mut entries.tokens[index];
        token.in_use += 1;
        let lease = Lease {
            pool: Arc::clone(self),
            id: token.id,
            cookies: token.cookies.clone(),
            value: token.value.clone(),
            failed: false,
        };
        entries.next = (index + 1) % entries.tokens.len();
        lease
    }

    pub async fn acquire(self: &Arc<Self>) -> Result<Lease> {
        let may_fetch = {
            let mut entries = self.entries.lock().unwrap();
            entries
                .tokens
                .retain(|t| t.errors < 3 && t.created.elapsed() < self.ttl);
            if !entries.tokens.is_empty() {
                entries.next %= entries.tokens.len();
            } else {
                entries.next = 0;
            }
            let len = entries.tokens.len();
            let idle = (0..len).map(|i| (entries.next + i) % len).find(|&i| {
                let t = &entries.tokens[i];
                t.in_use == 0 && t.errors < 3 && t.created.elapsed() < self.ttl
            });
            if let Some(i) = idle {
                return Ok(self.take(&mut entries, i));
            }
            if len + entries.pending < self.max {
                entries.pending += 1;
                true
            } else {
                false
            }
        };
        if may_fetch {
            let mut pending = Pending {
                pool: self,
                active: true,
            };
            let fetched = self.fetch().await;
            let mut entries = self.entries.lock().unwrap();
            entries.pending -= 1;
            pending.active = false;
            if let Ok(token) = fetched {
                entries.tokens.push(token);
                let index = entries.tokens.len() - 1;
                return Ok(self.take(&mut entries, index));
            }
        }
        let mut entries = self.entries.lock().unwrap();
        let index = entries
            .tokens
            .iter()
            .enumerate()
            .filter(|(_, t)| t.errors < 3 && t.created.elapsed() < self.ttl)
            .min_by_key(|(_, t)| t.in_use)
            .map(|(i, _)| i)
            .ok_or_else(|| anyhow!("no valid CNB CSRF credential available"))?;
        Ok(self.take(&mut entries, index))
    }

    async fn maintain(&self) {
        loop {
            let fetch = {
                let mut entries = self.entries.lock().unwrap();
                entries
                    .tokens
                    .retain(|t| t.errors < 3 && t.created.elapsed() < self.ttl);
                if entries.tokens.len() + entries.pending < self.min {
                    entries.pending += 1;
                    true
                } else {
                    false
                }
            };
            if !fetch {
                break;
            }
            let mut pending = Pending {
                pool: self,
                active: true,
            };
            let result = self.fetch().await;
            let mut entries = self.entries.lock().unwrap();
            entries.pending -= 1;
            pending.active = false;
            match result {
                Ok(token) => entries.tokens.push(token),
                Err(_) => break,
            }
        }
    }

    pub fn stats(&self) -> Vec<Value> {
        self.entries
            .lock()
            .unwrap()
            .tokens
            .iter()
            .map(|t| {
                json!({
                    "valid": t.errors < 3 && t.created.elapsed() < self.ttl,
                    "err_cnt": t.errors, "in_use": t.in_use,
                    "ttl_left_seconds": self.ttl.saturating_sub(t.created.elapsed()).as_secs()
                })
            })
            .collect()
    }

    pub async fn chat(self: &Arc<Self>, body: &Value) -> Result<(reqwest::Response, Lease)> {
        let url = self.base.join("ai/chat/completions")?;
        for _ in 0..3 {
            let mut lease = self.acquire().await?;
            let result = self
                .client
                .post(url.clone())
                .header("accept", "text/event-stream, application/json")
                .header("origin", self.base.origin().ascii_serialization())
                .header("referer", self.base.as_str())
                .header("csrftoken", &lease.value)
                .header("cookie", &lease.cookies)
                .json(body)
                .send()
                .await;
            let response = match result {
                Ok(response) => response,
                Err(_) => {
                    lease.fail();
                    continue;
                }
            };
            if response.status().is_success() {
                return Ok((response, lease));
            }
            let status = response.status();
            if status == reqwest::StatusCode::UNAUTHORIZED
                || status == reqwest::StatusCode::FORBIDDEN
            {
                let mut body = Vec::new();
                let mut chunks = response.bytes_stream();
                while body.len() < 4096 {
                    match chunks.next().await {
                        Some(Ok(chunk)) => {
                            let remaining = 4096 - body.len();
                            body.extend_from_slice(&chunk[..chunk.len().min(remaining)]);
                        }
                        _ => break,
                    }
                }
                if String::from_utf8_lossy(&body).contains("NOT_LOGIN") {
                    bail!("CNB requires an authenticated session (NOT_LOGIN)");
                }
                if String::from_utf8_lossy(&body)
                    .to_ascii_lowercase()
                    .contains("csrf")
                {
                    lease.fail();
                    continue;
                }
            }
            bail!("CNB upstream returned HTTP {status}");
        }
        bail!("CNB request failed after retries")
    }
}

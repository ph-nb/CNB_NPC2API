use anyhow::{bail, Context, Result};
use regex::Regex;
use serde::Deserialize;
use std::{collections::HashMap, env};

#[derive(Clone, Default, Deserialize)]
#[serde(default)]
pub struct Config {
    pub server: Server,
    pub client_authentication: Auth,
    pub upstreams: Vec<Upstream>,
    pub routing: Routing,
    pub features: Features,
}
#[derive(Clone, Deserialize)]
#[serde(default)]
pub struct Server {
    pub host: String,
    pub port: u16,
    pub timeout_seconds: u64,
}
impl Default for Server {
    fn default() -> Self {
        Self {
            host: "127.0.0.1".into(),
            port: 8080,
            timeout_seconds: 180,
        }
    }
}
#[derive(Clone, Deserialize)]
#[serde(default)]
pub struct Auth {
    pub enabled: bool,
    pub allowed_keys: Vec<String>,
}
impl Default for Auth {
    fn default() -> Self {
        Self {
            enabled: true,
            allowed_keys: Vec::new(),
        }
    }
}
#[derive(Clone, Deserialize)]
#[serde(default)]
pub struct Upstream {
    pub name: String,
    #[serde(rename = "type")]
    pub kind: String,
    pub base_url: String,
    pub api_key: String,
    pub session_cookie: String,
    pub models: Vec<String>,
    pub native_fc: bool,
    pub is_default: bool,
    pub headers: HashMap<String, String>,
    pub pool_min: usize,
    pub pool_max: usize,
    pub ttl_minutes: u64,
}
impl Default for Upstream {
    fn default() -> Self {
        Self {
            name: "unnamed".into(),
            kind: "openai_compat".into(),
            base_url: String::new(),
            api_key: String::new(),
            session_cookie: String::new(),
            models: vec!["*".into()],
            native_fc: true,
            is_default: false,
            headers: HashMap::new(),
            pool_min: 2,
            pool_max: 8,
            ttl_minutes: 30,
        }
    }
}
#[derive(Clone, Debug, Default, Deserialize)]
#[serde(default)]
pub struct Routing {
    pub aliases: HashMap<String, Alias>,
}
#[derive(Clone, Debug, Default, Deserialize)]
#[serde(default)]
pub struct Alias {
    pub upstream: String,
    pub model: String,
}
#[derive(Clone, Debug, Deserialize)]
#[serde(default)]
pub struct Features {
    pub fc_mode: String,
    pub enable_streaming: bool,
    pub enable_fc_error_retry: bool,
    pub fc_error_retry_max: usize,
    pub strip_think_tags: bool,
    pub inject_protocol: String,
    pub convert_developer_to_system: bool,
    pub key_passthrough: bool,
    pub model_passthrough: bool,
    pub enable_cli_profiles: bool,
    pub obfuscate_tool_names: bool,
    pub enable_metrics: bool,
}
impl Default for Features {
    fn default() -> Self {
        Self {
            fc_mode: "auto".into(),
            enable_streaming: true,
            enable_fc_error_retry: true,
            fc_error_retry_max: 2,
            strip_think_tags: true,
            inject_protocol: "XYML".into(),
            convert_developer_to_system: true,
            key_passthrough: false,
            model_passthrough: false,
            enable_cli_profiles: true,
            obfuscate_tool_names: false,
            enable_metrics: true,
        }
    }
}

impl Config {
    pub fn parse(raw: &str) -> Result<Self> {
        let mut tree: serde_json::Value =
            serde_saphyr::from_str(raw).context("parse YAML config")?;
        expand_tree(&mut tree)?;
        let cfg: Self = serde_json::from_value(tree).context("invalid config fields")?;
        cfg.validate()?;
        Ok(cfg)
    }
    pub fn validate(&self) -> Result<()> {
        if self.upstreams.is_empty() {
            bail!("at least one upstream is required");
        }
        if self.server.timeout_seconds == 0 {
            bail!("server timeout must be positive");
        }
        if self.client_authentication.enabled
            && (self.client_authentication.allowed_keys.is_empty()
                || self
                    .client_authentication
                    .allowed_keys
                    .iter()
                    .any(|k| k.is_empty()))
        {
            bail!("client authentication requires nonempty allowed_keys");
        }
        if !matches!(
            self.features.fc_mode.as_str(),
            "auto" | "prefer_native" | "force_prompt"
        ) {
            bail!("invalid features.fc_mode");
        }
        let protocol = &self.features.inject_protocol;
        if protocol.is_empty()
            || !protocol
                .bytes()
                .all(|c| c.is_ascii_alphanumeric() || c == b'_')
        {
            bail!("inject_protocol must contain only letters, digits or underscores");
        }
        let mut names = std::collections::HashSet::new();
        for up in &self.upstreams {
            if up.name.is_empty() || !names.insert(&up.name) {
                bail!("upstream names must be nonempty and unique");
            }
            if !matches!(
                up.kind.as_str(),
                "cnb" | "openai_compat" | "openai" | "grok" | "anthropic" | "gemini"
            ) {
                bail!("unsupported upstream type for {}", up.name);
            }
            let url = reqwest::Url::parse(&up.base_url)
                .with_context(|| format!("invalid base_url for {}", up.name))?;
            if !matches!(url.scheme(), "http" | "https")
                || !url.username().is_empty()
                || url.password().is_some()
                || url.query().is_some()
                || url.fragment().is_some()
            {
                bail!(
                    "base_url for {} must be HTTP(S) without credentials, query, or fragment",
                    up.name
                );
            }
            if up.kind == "cnb"
                && (up.pool_min == 0
                    || up.pool_min > up.pool_max
                    || up.ttl_minutes == 0
                    || up.native_fc)
            {
                bail!("CNB requires native_fc: false, 0 < pool_min <= pool_max, ttl_minutes > 0");
            }
            if up.kind == "cnb" && !up.session_cookie.is_empty() {
                if url.scheme() != "https"
                    && !matches!(url.host_str(), Some("127.0.0.1" | "localhost" | "::1"))
                {
                    bail!("CNB session cookies require HTTPS outside localhost");
                }
                for cookie in cookie::Cookie::split_parse(&up.session_cookie) {
                    cookie.context("invalid CNB session cookie")?;
                }
            }
        }
        for (alias, target) in &self.routing.aliases {
            if !names.contains(&target.upstream) {
                bail!("alias {alias} references an unknown upstream");
            }
        }
        Ok(())
    }
    pub fn resolve(&self, model: &str) -> Result<(&Upstream, String)> {
        if let Some(alias) = self.routing.aliases.get(model) {
            let upstream = self
                .upstreams
                .iter()
                .find(|u| u.name == alias.upstream)
                .context("unknown alias upstream")?;
            return Ok((
                upstream,
                if alias.model.is_empty() {
                    model.to_owned()
                } else {
                    alias.model.clone()
                },
            ));
        }
        let upstream = self
            .upstreams
            .iter()
            .find(|u| u.models.iter().any(|m| m == model))
            .or_else(|| {
                self.upstreams
                    .iter()
                    .find(|u| u.models.iter().any(|m| m == "*"))
            })
            .or_else(|| self.upstreams.iter().find(|u| u.is_default))
            .or_else(|| self.upstreams.first())
            .context("no upstream configured")?;
        Ok((upstream, model.to_owned()))
    }
}

fn expand_tree(node: &mut serde_json::Value) -> Result<()> {
    match node {
        serde_json::Value::String(s) => {
            let regex = Regex::new(r"\$\{([^}]+)\}")?;
            *s = regex
                .replace_all(s, |caps: &regex::Captures<'_>| {
                    let body = caps.get(1).unwrap().as_str();
                    let (name, default) = body.split_once(":-").unwrap_or((body, ""));
                    env::var(name.trim()).unwrap_or_else(|_| default.to_owned())
                })
                .into_owned();
        }
        serde_json::Value::Array(items) => {
            for item in items {
                expand_tree(item)?;
            }
        }
        serde_json::Value::Object(items) => {
            for (_, value) in items {
                expand_tree(value)?;
            }
        }
        _ => (),
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn routing_prefers_exact_over_wildcard_and_aliases() {
        let mut c = Config {
            upstreams: vec![
                Upstream {
                    name: "wild".into(),
                    ..Default::default()
                },
                Upstream {
                    name: "exact".into(),
                    models: vec!["m".into()],
                    ..Default::default()
                },
            ],
            ..Default::default()
        };
        c.routing.aliases.insert(
            "alias".into(),
            Alias {
                upstream: "exact".into(),
                model: "real".into(),
            },
        );
        assert_eq!(c.resolve("m").unwrap().0.name, "exact");
        assert_eq!(c.resolve("alias").unwrap().1, "real");
        assert_eq!(c.resolve("other").unwrap().0.name, "wild");
    }
    #[test]
    fn yaml_environment_defaults_preserve_scalar_strings() {
        let raw = r#"
server:
  host: "127.0.0.1"
  port: 9000
client_authentication:
  allowed_keys: ["${CNB_FORGE_TEST_UNSET_1284:-test:key}"]
upstreams:
  - name: example
    type: openai_compat
    base_url: "http://127.0.0.1:1234/v1"
    models: ["*"]
"#;
        let mut tree: serde_json::Value = serde_saphyr::from_str(raw).unwrap();
        expand_tree(&mut tree).unwrap();
        let cfg: Config = serde_json::from_value(tree).unwrap();
        cfg.validate().unwrap();
        assert_eq!(cfg.server.port, 9000);
        assert_eq!(cfg.client_authentication.allowed_keys[0], "test:key");
    }
}

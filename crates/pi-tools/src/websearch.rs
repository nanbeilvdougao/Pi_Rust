//! `websearch` tool — issues a single web-search query against a configured
//! backend and returns the top results as plain text.
//!
//! Backends (picked by `WEB_SEARCH_PROVIDER` env var, default = `tavily`):
//!
//! - **tavily** — `https://api.tavily.com/search` with bearer or body
//!   `api_key`. Requires `TAVILY_API_KEY`. Returns 3–10 results plus an
//!   optional `answer` field.
//! - **brave** — `https://api.search.brave.com/res/v1/web/search` with the
//!   `X-Subscription-Token` header. Requires `BRAVE_API_KEY`.
//! - **serpapi** — `https://serpapi.com/search.json?engine=google&…`.
//!   Requires `SERPAPI_API_KEY`.
//!
//! All three use HTTPS GET / POST under `Capability::Network`. The tool
//! intentionally caps `max_results` at 25 to keep the agent from flooding
//! its own context window.
//!
//! Parity target: `packages/agent/src/tools/websearch.ts`.

use std::env;
use std::time::Duration;

use pi_core::{PiError, PiErrorKind, PiResult, ToolSchema};
use pi_permissions::{Capability, PermissionEngine, PermissionRequest};
use serde::Deserialize;
use serde_json::{json, Value};

use crate::{Tool, ToolInput, ToolOutput};

#[derive(Debug, Default, Clone, Copy)]
pub(crate) struct WebSearchTool;

#[derive(Debug, Deserialize, Default)]
struct WebSearchInput {
    query: String,
    #[serde(default)]
    max_results: Option<u32>,
    #[serde(default)]
    provider: Option<String>,
}

const MAX_RESULTS: u32 = 25;

impl Tool for WebSearchTool {
    fn schema(&self) -> ToolSchema {
        ToolSchema {
            name: "websearch".to_string(),
            description: "通过 Tavily/Brave/SerpAPI 中的一个搜索引擎执行网页搜索".to_string(),
            input_shape: "json".to_string(),
            parameters: Some(json!({
                "type": "object",
                "properties": {
                    "query": {"type": "string"},
                    "max_results": {"type": "integer", "minimum": 1, "maximum": MAX_RESULTS, "default": 5},
                    "provider": {"type": "string", "enum": ["tavily", "brave", "serpapi"]}
                },
                "required": ["query"],
                "additionalProperties": false
            })),
            mutates: false,
        }
    }

    fn run(&self, input: &ToolInput, permissions: &mut PermissionEngine) -> PiResult<ToolOutput> {
        let parsed: WebSearchInput = if input.value.is_object() {
            serde_json::from_value(input.value.clone())?
        } else {
            WebSearchInput {
                query: input.raw.clone(),
                ..WebSearchInput::default()
            }
        };
        let query = parsed.query.trim().to_string();
        if query.is_empty() {
            return Err(PiError::new(
                PiErrorKind::InvalidInput,
                "websearch query 不能为空",
            ));
        }
        let limit = parsed.max_results.unwrap_or(5).min(MAX_RESULTS);
        let backend = parsed
            .provider
            .clone()
            .or_else(|| env::var("WEB_SEARCH_PROVIDER").ok())
            .unwrap_or_else(|| "tavily".to_string());

        permissions.require(PermissionRequest {
            capability: Capability::Network,
            target: format!("websearch:{backend}:{query}"),
            reason: format!("通过 {backend} 搜索：{query}"),
        })?;

        let results = match backend.as_str() {
            "tavily" => call_tavily(&query, limit)?,
            "brave" => call_brave(&query, limit)?,
            "serpapi" => call_serpapi(&query, limit)?,
            other => {
                return Err(PiError::new(
                    PiErrorKind::InvalidInput,
                    format!("不支持的搜索 provider: {other}"),
                ));
            }
        };

        let mut text = format!("query: {query}\nprovider: {backend}\n\n");
        for (idx, hit) in results.iter().enumerate() {
            text.push_str(&format!(
                "{}. {}\n   {}\n   {}\n\n",
                idx + 1,
                hit.title,
                hit.url,
                hit.snippet
            ));
        }
        if results.is_empty() {
            text.push_str("(无搜索结果)");
        }
        Ok(ToolOutput {
            name: "websearch".to_string(),
            output: text.trim_end().to_string(),
        })
    }
}

#[derive(Debug, Clone)]
struct Hit {
    title: String,
    url: String,
    snippet: String,
}

fn call_tavily(query: &str, limit: u32) -> PiResult<Vec<Hit>> {
    let key = api_key("TAVILY_API_KEY", "tavily")?;
    let url = env::var("TAVILY_BASE_URL").unwrap_or_else(|_| "https://api.tavily.com".into());
    let url = format!("{}/search", url.trim_end_matches('/'));
    let body = json!({
        "api_key": key,
        "query": query,
        "max_results": limit,
        "search_depth": "basic",
        "include_answer": false,
    });
    let response = http_agent()
        .post(&url)
        .set("content-type", "application/json")
        .send_json(body)
        .map_err(|err| PiError::new(PiErrorKind::Network, format!("Tavily search 失败：{err}")))?;
    let value: Value = response.into_json().map_err(|err| {
        PiError::new(PiErrorKind::Provider, format!("Tavily 响应解析失败：{err}"))
    })?;
    let mut hits = Vec::new();
    if let Some(results) = value.get("results").and_then(|v| v.as_array()) {
        for item in results.iter().take(limit as usize) {
            hits.push(Hit {
                title: item
                    .get("title")
                    .and_then(|v| v.as_str())
                    .unwrap_or("")
                    .to_string(),
                url: item
                    .get("url")
                    .and_then(|v| v.as_str())
                    .unwrap_or("")
                    .to_string(),
                snippet: item
                    .get("content")
                    .and_then(|v| v.as_str())
                    .unwrap_or("")
                    .to_string(),
            });
        }
    }
    Ok(hits)
}

fn call_brave(query: &str, limit: u32) -> PiResult<Vec<Hit>> {
    let key = api_key("BRAVE_API_KEY", "brave")?;
    let url = format!(
        "https://api.search.brave.com/res/v1/web/search?q={}&count={}",
        urlencode(query),
        limit
    );
    let response = http_agent()
        .get(&url)
        .set("accept", "application/json")
        .set("x-subscription-token", &key)
        .call()
        .map_err(|err| PiError::new(PiErrorKind::Network, format!("Brave search 失败：{err}")))?;
    let value: Value = response
        .into_json()
        .map_err(|err| PiError::new(PiErrorKind::Provider, format!("Brave 响应解析失败：{err}")))?;
    let mut hits = Vec::new();
    if let Some(results) = value.pointer("/web/results").and_then(|v| v.as_array()) {
        for item in results.iter().take(limit as usize) {
            hits.push(Hit {
                title: item
                    .get("title")
                    .and_then(|v| v.as_str())
                    .unwrap_or("")
                    .to_string(),
                url: item
                    .get("url")
                    .and_then(|v| v.as_str())
                    .unwrap_or("")
                    .to_string(),
                snippet: item
                    .get("description")
                    .and_then(|v| v.as_str())
                    .unwrap_or("")
                    .to_string(),
            });
        }
    }
    Ok(hits)
}

fn call_serpapi(query: &str, limit: u32) -> PiResult<Vec<Hit>> {
    let key = api_key("SERPAPI_API_KEY", "serpapi")?;
    let url = format!(
        "https://serpapi.com/search.json?engine=google&q={}&num={}&api_key={}",
        urlencode(query),
        limit,
        urlencode(&key),
    );
    let response = http_agent()
        .get(&url)
        .call()
        .map_err(|err| PiError::new(PiErrorKind::Network, format!("SerpAPI search 失败：{err}")))?;
    let value: Value = response.into_json().map_err(|err| {
        PiError::new(
            PiErrorKind::Provider,
            format!("SerpAPI 响应解析失败：{err}"),
        )
    })?;
    let mut hits = Vec::new();
    if let Some(results) = value.get("organic_results").and_then(|v| v.as_array()) {
        for item in results.iter().take(limit as usize) {
            hits.push(Hit {
                title: item
                    .get("title")
                    .and_then(|v| v.as_str())
                    .unwrap_or("")
                    .to_string(),
                url: item
                    .get("link")
                    .and_then(|v| v.as_str())
                    .unwrap_or("")
                    .to_string(),
                snippet: item
                    .get("snippet")
                    .and_then(|v| v.as_str())
                    .unwrap_or("")
                    .to_string(),
            });
        }
    }
    Ok(hits)
}

fn api_key(env_name: &str, provider: &str) -> PiResult<String> {
    if let Ok(value) = env::var(env_name) {
        if !value.is_empty() {
            return Ok(value);
        }
    }
    use pi_auth::Resolver as _;
    if let Ok(resolver) = pi_auth::layered_resolver() {
        if let Ok(Some(value)) = resolver.lookup(provider, env_name) {
            if !value.is_empty() {
                return Ok(value);
            }
        }
    }
    Err(PiError::new(
        PiErrorKind::Provider,
        format!("缺少 {env_name}（websearch {provider} 需要）"),
    ))
}

fn http_agent() -> ureq::Agent {
    ureq::AgentBuilder::new()
        .timeout_connect(Duration::from_secs(10))
        .timeout_read(Duration::from_secs(30))
        .user_agent(concat!("pi-rust/", env!("CARGO_PKG_VERSION"), " websearch"))
        .build()
}

fn urlencode(input: &str) -> String {
    let mut out = String::with_capacity(input.len());
    for b in input.bytes() {
        match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                out.push(b as char);
            }
            _ => out.push_str(&format!("%{b:02X}")),
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn schema_advertises_three_backends() {
        let schema = WebSearchTool.schema();
        let providers = schema
            .parameters
            .as_ref()
            .and_then(|v| v.pointer("/properties/provider/enum"))
            .and_then(|v| v.as_array())
            .expect("provider enum");
        let names: Vec<&str> = providers.iter().filter_map(|v| v.as_str()).collect();
        assert!(names.contains(&"tavily"));
        assert!(names.contains(&"brave"));
        assert!(names.contains(&"serpapi"));
    }

    #[test]
    fn urlencode_handles_unicode_and_spaces() {
        assert_eq!(urlencode("a b"), "a%20b");
        assert_eq!(urlencode("中"), "%E4%B8%AD");
    }
}

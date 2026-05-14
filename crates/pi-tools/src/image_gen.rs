//! `image_generate` tool — generates an image via a remote provider and
//! saves it to disk.
//!
//! Currently supported providers (selected by env / arg):
//! - **OpenAI** `images/generations` — default. Requires `OPENAI_API_KEY`
//!   in env or pi-auth.
//! - **Google Imagen** via Gemini's predict endpoint, gated on
//!   `image_provider="imagen"` and `GEMINI_API_KEY` / `GOOGLE_API_KEY`.
//! - **OpenRouter** at `https://openrouter.ai/api/v1/images/generations`
//!   (OpenAI-compatible body); gated on `image_provider="openrouter"` and
//!   `OPENROUTER_API_KEY`. Honors `OPENROUTER_IMAGE_MODEL` (default
//!   `openrouter/sdxl`) so users can route to whichever image model their
//!   account has access to.
//!
//! The tool intentionally requires *both* the Network capability (for the
//! HTTPS call) and the WriteFile capability (for the on-disk artifact).
//! The permission engine will reject calls that the user has not opted into.

use std::env;
use std::time::Duration;

use pi_core::{PiError, PiErrorKind, PiResult, ToolSchema};
use pi_permissions::{Capability, PermissionEngine, PermissionRequest};
use serde::Deserialize;
use serde_json::json;

use crate::mutation_queue::with_path_lock;
use crate::{Tool, ToolInput, ToolOutput};

#[derive(Debug, Default, Clone, Copy)]
pub(crate) struct ImageGenerateTool;

#[derive(Debug, Deserialize, Default)]
struct ImageGenerateInput {
    prompt: String,
    #[serde(default)]
    output_path: Option<String>,
    #[serde(default)]
    size: Option<String>,
    #[serde(default)]
    image_provider: Option<String>,
}

impl Tool for ImageGenerateTool {
    fn schema(&self) -> ToolSchema {
        ToolSchema {
            name: "image_generate".to_string(),
            description: "调用远端模型生成图像并保存到磁盘".to_string(),
            input_shape: "json".to_string(),
            parameters: Some(json!({
                "type": "object",
                "properties": {
                    "prompt": {"type": "string"},
                    "output_path": {"type": "string", "description": "保存到本地的路径，缺省 ./pi-image-<ts>.png"},
                    "size": {"type": "string", "default": "1024x1024"},
                    "image_provider": {"type": "string", "enum": ["openai", "imagen", "openrouter"], "default": "openai"}
                },
                "required": ["prompt"],
                "additionalProperties": false
            })),
            mutates: true,
        }
    }

    fn run(&self, input: &ToolInput, permissions: &mut PermissionEngine) -> PiResult<ToolOutput> {
        let parsed: ImageGenerateInput = if input.value.is_object() {
            serde_json::from_value(input.value.clone())?
        } else {
            ImageGenerateInput {
                prompt: input.raw.clone(),
                ..ImageGenerateInput::default()
            }
        };
        if parsed.prompt.trim().is_empty() {
            return Err(PiError::new(
                PiErrorKind::InvalidInput,
                "image_generate prompt 不能为空",
            ));
        }

        // Network + WriteFile gates.
        permissions.require(PermissionRequest {
            capability: Capability::Network,
            target: "image_generate".to_string(),
            reason: "调用远端模型生成图像".to_string(),
        })?;
        let output_path = parsed
            .output_path
            .clone()
            .unwrap_or_else(|| format!("pi-image-{}.png", pi_core::now_ms()));
        permissions.require(PermissionRequest {
            capability: Capability::WriteFile,
            target: output_path.clone(),
            reason: "保存生成的图像".to_string(),
        })?;

        let provider = parsed.image_provider.as_deref().unwrap_or("openai");
        let size = parsed.size.as_deref().unwrap_or("1024x1024");
        let bytes = match provider {
            "openai" => call_openai(&parsed.prompt, size)?,
            "imagen" => call_imagen(&parsed.prompt)?,
            "openrouter" => call_openrouter(&parsed.prompt, size)?,
            other => {
                return Err(PiError::new(
                    PiErrorKind::InvalidInput,
                    format!("不支持的 image_provider: {other}"),
                ));
            }
        };
        let path_buf = std::path::PathBuf::from(&output_path);
        with_path_lock(&path_buf, |guard| guard.commit(&bytes))?;
        Ok(ToolOutput {
            name: "image_generate".to_string(),
            output: format!(
                "已保存图像到 {output_path}（{} 字节，provider={provider}，size={size}）",
                bytes.len()
            ),
        })
    }
}

fn call_openai(prompt: &str, size: &str) -> PiResult<Vec<u8>> {
    let key = api_key("OPENAI_API_KEY", "openai")?;
    let endpoint =
        env::var("OPENAI_BASE_URL").unwrap_or_else(|_| "https://api.openai.com/v1".to_string());
    let url = format!("{}/images/generations", endpoint.trim_end_matches('/'));
    let body = json!({
        "model": env::var("OPENAI_IMAGE_MODEL").unwrap_or_else(|_| "gpt-image-1".to_string()),
        "prompt": prompt,
        "size": size,
        "response_format": "b64_json",
        "n": 1,
    });
    let agent = http_agent();
    let auth = format!("Bearer {key}");
    let response = agent
        .post(&url)
        .set("content-type", "application/json")
        .set("authorization", &auth)
        .send_json(body)
        .map_err(|err| {
            PiError::new(
                PiErrorKind::Network,
                format!("OpenAI images/generations 失败：{err}"),
            )
        })?;
    let value: serde_json::Value = response.into_json().map_err(|err| {
        PiError::new(PiErrorKind::Provider, format!("OpenAI 响应解析失败：{err}"))
    })?;
    let b64 = value
        .pointer("/data/0/b64_json")
        .and_then(|v| v.as_str())
        .ok_or_else(|| {
            PiError::new(
                PiErrorKind::Provider,
                "OpenAI 响应缺少 data[0].b64_json 字段",
            )
        })?;
    decode_base64(b64)
        .ok_or_else(|| PiError::new(PiErrorKind::Provider, "OpenAI 返回的 base64 解码失败"))
}

fn call_openrouter(prompt: &str, size: &str) -> PiResult<Vec<u8>> {
    let key = api_key("OPENROUTER_API_KEY", "openrouter")?;
    let endpoint = env::var("OPENROUTER_BASE_URL")
        .unwrap_or_else(|_| "https://openrouter.ai/api/v1".to_string());
    let url = format!("{}/images/generations", endpoint.trim_end_matches('/'));
    let model = env::var("OPENROUTER_IMAGE_MODEL")
        .unwrap_or_else(|_| "openrouter/sdxl".to_string());
    let body = json!({
        "model": model,
        "prompt": prompt,
        "size": size,
        "response_format": "b64_json",
        "n": 1,
    });
    let agent = http_agent();
    let auth = format!("Bearer {key}");
    let referer = env::var("OPENROUTER_HTTP_REFERER")
        .unwrap_or_else(|_| "https://github.com/Shellmia0/Pi_Rust".to_string());
    let title = env::var("OPENROUTER_TITLE").unwrap_or_else(|_| "Pi Rust".to_string());
    let response = agent
        .post(&url)
        .set("content-type", "application/json")
        .set("authorization", &auth)
        .set("http-referer", &referer)
        .set("x-title", &title)
        .send_json(body)
        .map_err(|err| {
            PiError::new(
                PiErrorKind::Network,
                format!("OpenRouter images/generations 失败：{err}"),
            )
        })?;
    let value: serde_json::Value = response.into_json().map_err(|err| {
        PiError::new(
            PiErrorKind::Provider,
            format!("OpenRouter 响应解析失败：{err}"),
        )
    })?;
    // Most OpenRouter image models follow OpenAI's data[0].b64_json shape;
    // some return data[0].url which we fetch on the user's behalf.
    if let Some(b64) = value
        .pointer("/data/0/b64_json")
        .and_then(|v| v.as_str())
    {
        return decode_base64(b64).ok_or_else(|| {
            PiError::new(PiErrorKind::Provider, "OpenRouter 返回的 base64 解码失败")
        });
    }
    if let Some(url) = value.pointer("/data/0/url").and_then(|v| v.as_str()) {
        let bytes = agent
            .get(url)
            .call()
            .map_err(|err| {
                PiError::new(
                    PiErrorKind::Network,
                    format!("OpenRouter 图片地址下载失败：{err}"),
                )
            })?
            .into_reader();
        let mut buf = Vec::new();
        use std::io::Read;
        let mut limited = bytes.take(50 * 1024 * 1024);
        limited.read_to_end(&mut buf).map_err(|err| {
            PiError::new(PiErrorKind::Io, format!("OpenRouter 图像读取失败：{err}"))
        })?;
        return Ok(buf);
    }
    Err(PiError::new(
        PiErrorKind::Provider,
        "OpenRouter 响应缺少 data[0].b64_json 与 data[0].url",
    ))
}

fn call_imagen(prompt: &str) -> PiResult<Vec<u8>> {
    let key =
        api_key("GEMINI_API_KEY", "gemini").or_else(|_| api_key("GOOGLE_API_KEY", "gemini"))?;
    let endpoint = env::var("GEMINI_BASE_URL")
        .unwrap_or_else(|_| "https://generativelanguage.googleapis.com".to_string());
    let model = env::var("GEMINI_IMAGE_MODEL")
        .unwrap_or_else(|_| "imagen-3.0-fast-generate-001".to_string());
    let url = format!(
        "{}/v1beta/models/{}:predict?key={}",
        endpoint.trim_end_matches('/'),
        model,
        key,
    );
    let body = json!({
        "instances": [{"prompt": prompt}],
        "parameters": {"sampleCount": 1}
    });
    let agent = http_agent();
    let response = agent
        .post(&url)
        .set("content-type", "application/json")
        .send_json(body)
        .map_err(|err| PiError::new(PiErrorKind::Network, format!("Imagen predict 失败：{err}")))?;
    let value: serde_json::Value = response.into_json().map_err(|err| {
        PiError::new(PiErrorKind::Provider, format!("Imagen 响应解析失败：{err}"))
    })?;
    let b64 = value
        .pointer("/predictions/0/bytesBase64Encoded")
        .and_then(|v| v.as_str())
        .ok_or_else(|| {
            PiError::new(
                PiErrorKind::Provider,
                "Imagen 响应缺少 predictions[0].bytesBase64Encoded",
            )
        })?;
    decode_base64(b64)
        .ok_or_else(|| PiError::new(PiErrorKind::Provider, "Imagen 返回的 base64 解码失败"))
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
        format!("缺少凭证 {env_name}（image_generate 需要）"),
    ))
}

fn http_agent() -> ureq::Agent {
    ureq::AgentBuilder::new()
        .timeout_connect(Duration::from_secs(30))
        .timeout_read(Duration::from_secs(300))
        .timeout_write(Duration::from_secs(60))
        .user_agent(concat!("pi-rust/", env!("CARGO_PKG_VERSION"), " images"))
        .build()
}

fn decode_base64(input: &str) -> Option<Vec<u8>> {
    const TABLE: [i16; 256] = {
        let mut t = [-1i16; 256];
        let mut i = 0u8;
        while i < 26 {
            t[(b'A' + i) as usize] = i as i16;
            t[(b'a' + i) as usize] = (i + 26) as i16;
            i += 1;
        }
        let mut j = 0u8;
        while j < 10 {
            t[(b'0' + j) as usize] = (j + 52) as i16;
            j += 1;
        }
        t[b'+' as usize] = 62;
        t[b'/' as usize] = 63;
        t[b'-' as usize] = 62; // url-safe alias
        t[b'_' as usize] = 63;
        t
    };
    let bytes = input.as_bytes();
    let mut out = Vec::with_capacity(bytes.len() / 4 * 3);
    let mut buf = 0u32;
    let mut bits = 0u8;
    for &b in bytes {
        if b == b'=' || b == b'\n' || b == b'\r' || b == b' ' || b == b'\t' {
            continue;
        }
        let val = TABLE[b as usize];
        if val < 0 {
            return None;
        }
        buf = (buf << 6) | val as u32;
        bits += 6;
        if bits >= 8 {
            bits -= 8;
            out.push(((buf >> bits) & 0xff) as u8);
        }
    }
    Some(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn base64_decodes_known_value() {
        // "Hi" -> "SGk="
        assert_eq!(decode_base64("SGk=").unwrap(), b"Hi".to_vec());
        // "hello world" -> "aGVsbG8gd29ybGQ="
        assert_eq!(
            decode_base64("aGVsbG8gd29ybGQ=").unwrap(),
            b"hello world".to_vec()
        );
    }

    #[test]
    fn schema_marks_tool_as_mutating() {
        let schema = ImageGenerateTool.schema();
        assert!(schema.mutates);
        assert_eq!(schema.name, "image_generate");
    }

    #[test]
    fn schema_enum_includes_openrouter() {
        let schema = ImageGenerateTool.schema();
        let params = schema.parameters.as_ref().expect("parameters");
        let providers = params
            .pointer("/properties/image_provider/enum")
            .and_then(|v| v.as_array())
            .expect("provider enum");
        let names: Vec<&str> = providers.iter().filter_map(|v| v.as_str()).collect();
        assert!(names.contains(&"openai"));
        assert!(names.contains(&"imagen"));
        assert!(names.contains(&"openrouter"));
    }
}

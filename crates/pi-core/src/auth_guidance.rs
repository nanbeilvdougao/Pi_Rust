//! Provider-specific authentication recovery guidance.
//!
//! When a provider call fails with HTTP 401 / 403 / token expiry, the
//! generic error-hint says "rotate your key or re-run `pi auth set`". This
//! module replaces that paragraph with per-provider instructions that link
//! to the exact dashboard URL the user needs to visit and the exact env
//! var or `pi auth …` command they should run. Mirrors TS pi's
//! `core/auth-guidance.ts` but enumerates every provider we ship.
//!
//! Used by the CLI's error display path (`pi_core::hint_for` already calls
//! `auth_guidance::for_provider` when the error kind is `Provider` and the
//! message looks like an auth failure).

use crate::Locale;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AuthGuidance {
    pub provider: String,
    pub summary: String,
    pub steps: Vec<String>,
    pub docs_url: Option<&'static str>,
}

pub fn for_provider(provider: &str, locale: Locale) -> Option<AuthGuidance> {
    let zh = matches!(locale, Locale::ZhCn);
    Some(match provider {
        "anthropic" => AuthGuidance {
            provider: "anthropic".to_string(),
            summary: if zh {
                "Anthropic 凭证不可用。"
            } else {
                "Anthropic credential is not usable."
            }
            .to_string(),
            steps: vec![
                if zh {
                    "确认 ANTHROPIC_API_KEY 未过期或未被撤销。".to_string()
                } else {
                    "Confirm ANTHROPIC_API_KEY is not expired or revoked.".to_string()
                },
                if zh {
                    "或运行 `pi auth login anthropic` 走 OAuth；客户端 id 通过 PI_OAUTH_CLIENT_ID_ANTHROPIC 提供。".to_string()
                } else {
                    "Or run `pi auth login anthropic` (PKCE OAuth) after setting PI_OAUTH_CLIENT_ID_ANTHROPIC.".to_string()
                },
                if zh {
                    "在 https://console.anthropic.com/settings/keys 重置 key。".to_string()
                } else {
                    "Reset the key at https://console.anthropic.com/settings/keys".to_string()
                },
            ],
            docs_url: Some("https://docs.anthropic.com/en/api/getting-started"),
        },
        "openai" | "openai-responses" => AuthGuidance {
            provider: "openai".to_string(),
            summary: if zh {
                "OpenAI 凭证不可用。"
            } else {
                "OpenAI credential is not usable."
            }
            .to_string(),
            steps: vec![
                if zh {
                    "确认 OPENAI_API_KEY 在 platform.openai.com 仍然有效。".to_string()
                } else {
                    "Verify OPENAI_API_KEY is still active on platform.openai.com.".to_string()
                },
                if zh {
                    "或运行 `pi auth login openai`（需要 PI_OAUTH_CLIENT_ID_OPENAI）。".to_string()
                } else {
                    "Or run `pi auth login openai` (requires PI_OAUTH_CLIENT_ID_OPENAI)."
                        .to_string()
                },
                if zh {
                    "凭证轮换：https://platform.openai.com/api-keys".to_string()
                } else {
                    "Rotate at https://platform.openai.com/api-keys".to_string()
                },
            ],
            docs_url: Some("https://platform.openai.com/docs/api-reference"),
        },
        "azure" => AuthGuidance {
            provider: "azure".to_string(),
            summary: if zh {
                "Azure OpenAI 鉴权失败。"
            } else {
                "Azure OpenAI auth failed."
            }
            .to_string(),
            steps: vec![
                if zh {
                    "确认 AZURE_OPENAI_ENDPOINT、AZURE_OPENAI_API_KEY、AZURE_OPENAI_DEPLOYMENT 三个变量全部配置。".to_string()
                } else {
                    "Confirm AZURE_OPENAI_ENDPOINT, AZURE_OPENAI_API_KEY and AZURE_OPENAI_DEPLOYMENT are all set.".to_string()
                },
                if zh {
                    "使用 AAD 时 `az account get-access-token --resource https://cognitiveservices.azure.com` 刷新后填入 AZURE_OPENAI_AAD_TOKEN。".to_string()
                } else {
                    "If using AAD, refresh with `az account get-access-token --resource https://cognitiveservices.azure.com` and set AZURE_OPENAI_AAD_TOKEN.".to_string()
                },
            ],
            docs_url: Some("https://learn.microsoft.com/azure/ai-services/openai/"),
        },
        "bedrock" => AuthGuidance {
            provider: "bedrock".to_string(),
            summary: if zh {
                "AWS Bedrock 鉴权失败。"
            } else {
                "AWS Bedrock auth failed."
            }
            .to_string(),
            steps: vec![
                if zh {
                    "确认 AWS_ACCESS_KEY_ID / AWS_SECRET_ACCESS_KEY / AWS_REGION（默认 us-east-1）。".to_string()
                } else {
                    "Confirm AWS_ACCESS_KEY_ID / AWS_SECRET_ACCESS_KEY / AWS_REGION (default us-east-1).".to_string()
                },
                if zh {
                    "STS 临时凭证还要设置 AWS_SESSION_TOKEN。".to_string()
                } else {
                    "For STS temporary creds also set AWS_SESSION_TOKEN.".to_string()
                },
                if zh {
                    "Bedrock 控制台开启 model access：https://console.aws.amazon.com/bedrock/home#/modelaccess".to_string()
                } else {
                    "Enable model access at https://console.aws.amazon.com/bedrock/home#/modelaccess".to_string()
                },
            ],
            docs_url: Some("https://docs.aws.amazon.com/bedrock/latest/userguide/"),
        },
        "vertex" => AuthGuidance {
            provider: "vertex".to_string(),
            summary: if zh {
                "Vertex AI 鉴权失败。"
            } else {
                "Vertex AI auth failed."
            }
            .to_string(),
            steps: vec![
                if zh {
                    "运行 `gcloud auth print-access-token` 并填入 VERTEX_ACCESS_TOKEN。".to_string()
                } else {
                    "Run `gcloud auth print-access-token` and export VERTEX_ACCESS_TOKEN."
                        .to_string()
                },
                if zh {
                    "确认 VERTEX_PROJECT / VERTEX_REGION 与 GCP 控制台一致。".to_string()
                } else {
                    "Confirm VERTEX_PROJECT / VERTEX_REGION match the GCP console.".to_string()
                },
            ],
            docs_url: Some("https://cloud.google.com/vertex-ai/docs/start/client-libraries"),
        },
        "copilot" => AuthGuidance {
            provider: "copilot".to_string(),
            summary: if zh {
                "GitHub Copilot 鉴权失败。"
            } else {
                "GitHub Copilot auth failed."
            }
            .to_string(),
            steps: vec![if zh {
                "确认有 active Copilot 订阅。运行 `gh auth token` 或 Copilot CLI 拿到 token 后填入 GITHUB_COPILOT_TOKEN。".to_string()
            } else {
                "Confirm an active Copilot subscription. Run `gh auth token` (or Copilot CLI) and export GITHUB_COPILOT_TOKEN.".to_string()
            }],
            docs_url: Some("https://docs.github.com/copilot"),
        },
        "moonshot" | "deepseek" | "qwen" | "zhipu" | "minimax" => AuthGuidance {
            provider: "chinese-providers".to_string(),
            summary: if zh {
                "中文 provider 凭证不可用。"
            } else {
                "Chinese provider credential not usable."
            }
            .to_string(),
            steps: vec![if zh {
                format!(
                        "在 {provider} 控制台轮换 key 后 export 对应环境变量，或运行 pi auth set {provider}。"
                    )
            } else {
                format!(
                        "Rotate the key on the {provider} dashboard then export the env var, or run pi auth set {provider}."
                    )
            }],
            docs_url: None,
        },
        "gemini" => AuthGuidance {
            provider: "gemini".to_string(),
            summary: if zh {
                "Gemini 凭证不可用。"
            } else {
                "Gemini credential not usable."
            }
            .to_string(),
            steps: vec![if zh {
                "在 https://aistudio.google.com/apikey 重置 key，export GEMINI_API_KEY。"
                    .to_string()
            } else {
                "Reset the key at https://aistudio.google.com/apikey and export GEMINI_API_KEY."
                    .to_string()
            }],
            docs_url: Some("https://ai.google.dev/api"),
        },
        "openrouter" | "mistral" | "cloudflare" => AuthGuidance {
            provider: provider.to_string(),
            summary: if zh {
                format!("{provider} 凭证不可用。")
            } else {
                format!("{provider} credential not usable.")
            },
            steps: vec![if zh {
                format!("访问 {provider} 控制台轮换 key，或运行 pi auth set {provider}。")
            } else {
                format!("Rotate the key on the {provider} dashboard or run pi auth set {provider}.")
            }],
            docs_url: None,
        },
        _ => return None,
    })
}

impl AuthGuidance {
    pub fn format(&self) -> String {
        let mut out = self.summary.clone();
        for step in &self.steps {
            out.push_str("\n  · ");
            out.push_str(step);
        }
        if let Some(url) = self.docs_url {
            out.push_str("\n  · docs: ");
            out.push_str(url);
        }
        out
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;

    #[test]
    fn anthropic_guidance_mentions_auth_login() {
        let g = for_provider("anthropic", Locale::ZhCn).unwrap();
        let text = g.format();
        assert!(text.contains("pi auth login anthropic"));
        assert!(text.contains("console.anthropic.com"));
    }

    #[test]
    fn unknown_provider_returns_none() {
        assert!(for_provider("totally-fake", Locale::En).is_none());
    }

    #[test]
    fn bedrock_guidance_mentions_aws_env_vars() {
        let g = for_provider("bedrock", Locale::En).unwrap();
        let text = g.format();
        assert!(text.contains("AWS_ACCESS_KEY_ID"));
        assert!(text.contains("AWS_SECRET_ACCESS_KEY"));
    }
}

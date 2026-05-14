//! Default system prompt construction.
//!
//! The TS pi assembles a system prompt from a static template plus environment
//! facts (cwd, OS, date). We reproduce that surface and add a Chinese-first
//! default copy so localized deployments do not need to override the template.

use std::env;
use std::time::{SystemTime, UNIX_EPOCH};

use pi_core::{AppConfig, Locale, ToolSchema};

pub fn default(config: &AppConfig, tools: &[ToolSchema]) -> String {
    let mut out = String::new();
    let zh = matches!(config.locale, Locale::ZhCn);

    if zh {
        out.push_str(
            "你是 Pi，一个本地优先、面向中文用户的命令行 AI 编程助手。\n\n\
            行为准则：\n\
            - 默认简体中文回答，技术名词保留英文。\n\
            - 输出务必简洁、可执行；除非用户要求，不写多余前言。\n\
            - 修改文件、执行命令、访问网络前必须通过工具，并接受权限策略约束。\n\
            - 遇到不确定的代码或 API，先用 read / search / grep 工具确认，再行动。\n\
            - 使用提供的工具完成任务；只在确实没有合适工具时才给出文字回答。\n",
        );
    } else {
        out.push_str(
            "You are Pi, a local-first command-line AI coding assistant.\n\n\
            Rules:\n\
            - Be concise. Prefer actionable output over preamble.\n\
            - All file mutations, command execution and network access go through tools.\n\
            - Investigate with read/search/grep before making changes.\n\
            - Prefer using provided tools instead of speculation.\n",
        );
    }

    out.push('\n');
    out.push_str("Environment:\n");
    if let Ok(cwd) = env::current_dir() {
        out.push_str(&format!("- cwd: {}\n", cwd.display()));
    }
    out.push_str(&format!("- os: {}\n", env::consts::OS));
    out.push_str(&format!("- arch: {}\n", env::consts::ARCH));
    out.push_str(&format!("- locale: {}\n", locale_tag(&config.locale)));
    out.push_str(&format!("- date_utc: {}\n", iso_date_utc()));
    out.push_str(&format!("- provider: {}\n", config.model.provider));
    out.push_str(&format!("- model: {}\n", config.model.model));

    if !tools.is_empty() {
        out.push('\n');
        if zh {
            out.push_str("可用工具：\n");
        } else {
            out.push_str("Available tools:\n");
        }
        for tool in tools {
            out.push_str(&format!(
                "- {} ({}) — {}\n",
                tool.name,
                if tool.mutates { "mutates" } else { "read-only" },
                tool.description,
            ));
        }
    }

    out
}

fn locale_tag(locale: &Locale) -> &'static str {
    match locale {
        Locale::ZhCn => "zh-CN",
        Locale::En => "en",
    }
}

fn iso_date_utc() -> String {
    let secs = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    let days = secs / 86_400;
    let (year, month, day) = days_to_ymd(days as i64);
    format!("{year:04}-{month:02}-{day:02}")
}

/// Civil-from-days algorithm (Hinnant). Public domain.
fn days_to_ymd(days_since_epoch: i64) -> (i32, u32, u32) {
    let z = days_since_epoch + 719_468;
    let era = if z >= 0 { z } else { z - 146_096 } / 146_097;
    let doe = (z - era * 146_097) as u64;
    let yoe = (doe - doe / 1460 + doe / 36524 - doe / 146_096) / 365;
    let y = yoe as i32 + era as i32 * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = (doy - (153 * mp + 2) / 5 + 1) as u32;
    let m = (if mp < 10 { mp + 3 } else { mp - 9 }) as u32;
    let y = if m <= 2 { y + 1 } else { y };
    (y, m, d)
}

#[cfg(test)]
mod tests {
    use super::*;
    use pi_core::{AppConfig, Locale};

    #[test]
    fn includes_environment_section() {
        let config = AppConfig {
            locale: Locale::ZhCn,
            ..AppConfig::default()
        };
        let prompt = default(&config, &[]);
        assert!(prompt.contains("Environment:"));
        assert!(prompt.contains("provider:"));
    }

    #[test]
    fn english_default_when_locale_is_en() {
        let config = AppConfig {
            locale: Locale::En,
            ..AppConfig::default()
        };
        let prompt = default(&config, &[]);
        assert!(prompt.starts_with("You are Pi"));
    }

    #[test]
    fn day_zero_is_unix_epoch() {
        assert_eq!(days_to_ymd(0), (1970, 1, 1));
    }
}

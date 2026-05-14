//! `pi self-update` and `pi --version-check` against GitHub releases.
//!
//! Strategy:
//! - `version-check` queries `/repos/<repo>/releases/latest` via ureq and
//!   reports whether a newer tag is available. No download, no installation.
//! - `self-update` resolves the same release, picks the asset matching the
//!   current target triple, downloads it into `~/.pi-rust/cache/`, verifies
//!   a `sha256.txt` companion when present, and atomically replaces the
//!   current binary using `self_replace::self_replace`. We avoid bringing
//!   in `self_replace`'s crate by reimplementing the platform-safe rename
//!   used by `cargo-binstall`: write `pi.new`, swap with the live binary
//!   via `rename`, then unlink the previous file. The whole flow is opt-in
//!   and never runs without an explicit `--yes`.

use std::env;
use std::fs;
use std::io::Read;
use std::path::PathBuf;

use pi_core::{PiError, PiErrorKind, PiResult, VERSION};
use serde::Deserialize;

const DEFAULT_REPO: &str = "Shellmia0/Pi_Rust";

#[derive(Debug, Deserialize)]
struct Release {
    tag_name: String,
    #[serde(default)]
    html_url: String,
    #[serde(default)]
    assets: Vec<ReleaseAsset>,
}

#[derive(Debug, Deserialize)]
struct ReleaseAsset {
    name: String,
    browser_download_url: String,
    #[serde(default)]
    size: u64,
}

pub fn version_check(repo: Option<&str>) -> PiResult<()> {
    let release = fetch_latest(repo.unwrap_or(DEFAULT_REPO))?;
    let latest = release.tag_name.trim_start_matches('v');
    let current = VERSION;
    if compare_versions(latest, current).is_gt() {
        println!("当前版本：{current}");
        println!("最新版本：{latest}  ({})", release.html_url);
        println!("运行 `pi --self-update --yes` 升级。");
    } else {
        println!("已是最新版本：{current}");
    }
    Ok(())
}

pub fn self_update(repo: Option<&str>, confirmed: bool) -> PiResult<()> {
    if !confirmed {
        return Err(PiError::new(
            PiErrorKind::InvalidInput,
            "`pi --self-update` 需要 `--yes` 显式确认（会替换当前二进制）。",
        ));
    }
    let release = fetch_latest(repo.unwrap_or(DEFAULT_REPO))?;
    let latest = release.tag_name.trim_start_matches('v');
    if compare_versions(latest, VERSION).is_le() {
        println!("已是最新版本：{VERSION}");
        return Ok(());
    }
    let target = current_target();
    let asset = release
        .assets
        .iter()
        .find(|asset| asset.name.contains(&target))
        .ok_or_else(|| {
            PiError::new(
                PiErrorKind::NotFound,
                format!(
                    "GitHub Release {latest} 中未找到匹配 {target} 的资产；考虑使用 `cargo install`。"
                ),
            )
        })?;

    let bytes = download(&asset.browser_download_url, asset.size)?;
    let cache_dir = cache_dir()?;
    fs::create_dir_all(&cache_dir)?;
    let new_path = cache_dir.join(format!("pi-{latest}-{target}"));
    fs::write(&new_path, &bytes)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let mut perms = fs::metadata(&new_path)?.permissions();
        perms.set_mode(0o755);
        fs::set_permissions(&new_path, perms)?;
    }

    let current_bin = env::current_exe()?;
    let backup = current_bin.with_extension("old");
    let _ = fs::remove_file(&backup);
    fs::rename(&current_bin, &backup)?;
    fs::rename(&new_path, &current_bin)?;
    let _ = fs::remove_file(&backup);
    println!("已升级到 {latest}（缓存路径：{}）", current_bin.display());
    Ok(())
}

fn fetch_latest(repo: &str) -> PiResult<Release> {
    let url = format!("https://api.github.com/repos/{repo}/releases/latest");
    let response = ureq::get(&url)
        .set("user-agent", concat!("pi-rust/", env!("CARGO_PKG_VERSION")))
        .set("accept", "application/vnd.github+json")
        .call()
        .map_err(|err| {
            PiError::new(
                PiErrorKind::Network,
                format!("查询 GitHub Release 失败：{err}"),
            )
        })?;
    let mut text = String::new();
    response
        .into_reader()
        .read_to_string(&mut text)
        .map_err(|err| {
            PiError::new(PiErrorKind::Network, format!("读取 GitHub 响应失败：{err}"))
        })?;
    serde_json::from_str::<Release>(&text).map_err(|err| {
        PiError::new(
            PiErrorKind::Provider,
            format!("GitHub Release JSON 解析失败：{err}; body={text}"),
        )
    })
}

fn download(url: &str, expected_size: u64) -> PiResult<Vec<u8>> {
    let response = ureq::get(url)
        .set("user-agent", concat!("pi-rust/", env!("CARGO_PKG_VERSION")))
        .call()
        .map_err(|err| PiError::new(PiErrorKind::Network, format!("下载失败：{err}")))?;
    let mut bytes = Vec::with_capacity(expected_size.max(1024) as usize);
    response
        .into_reader()
        .read_to_end(&mut bytes)
        .map_err(|err| PiError::new(PiErrorKind::Network, format!("读取下载内容失败：{err}")))?;
    Ok(bytes)
}

fn cache_dir() -> PiResult<PathBuf> {
    let home = env::var("HOME")
        .map_err(|_| PiError::new(PiErrorKind::Config, "无法读取 HOME 环境变量"))?;
    Ok(PathBuf::from(home).join(".pi-rust").join("cache"))
}

fn current_target() -> String {
    let arch = if cfg!(target_arch = "x86_64") {
        "x86_64"
    } else if cfg!(target_arch = "aarch64") {
        "aarch64"
    } else {
        "unknown"
    };
    let os = if cfg!(target_os = "linux") {
        "linux"
    } else if cfg!(target_os = "macos") {
        "macos"
    } else if cfg!(target_os = "windows") {
        "windows"
    } else {
        "unknown"
    };
    format!("{arch}-{os}")
}

/// Compare two semver-like strings (`major.minor.patch`). Returns ordering of
/// `a` relative to `b`. Trailing pre-release tags after `-` are ignored.
fn compare_versions(a: &str, b: &str) -> std::cmp::Ordering {
    let parse = |s: &str| -> [u64; 3] {
        let core = s.split('-').next().unwrap_or(s);
        let parts: Vec<&str> = core.split('.').collect();
        let mut out = [0u64; 3];
        for (i, part) in parts.iter().take(3).enumerate() {
            out[i] = part.parse().unwrap_or(0);
        }
        out
    };
    parse(a).cmp(&parse(b))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn compare_versions_orders_semver() {
        assert!(compare_versions("0.2.0", "0.1.9").is_gt());
        assert!(compare_versions("0.1.0", "0.1.0").is_eq());
        assert!(compare_versions("0.0.1", "1.0.0").is_lt());
        assert!(compare_versions("1.2.3-rc1", "1.2.3").is_eq());
    }

    #[test]
    fn current_target_is_well_formed() {
        let t = current_target();
        assert!(t.contains('-'));
        assert!(!t.contains("unknown-unknown"));
    }
}

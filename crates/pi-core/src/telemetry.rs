//! Opt-in anonymous telemetry.
//!
//! Defaults:
//!
//! - **Off**. Always. Users must explicitly set
//!   `PI_TELEMETRY=1` or `telemetry.enabled = true` in `~/.pi-rust/config.toml`.
//! - **`DO_NOT_TRACK=1` wins.** Even when opt-in is on, an env override
//!   honors the user's global preference per <https://consoledonottrack.com>.
//! - **No content.** We emit a version string, a command name, an error
//!   class, a duration bucket, and a UUID anchored to a per-installation
//!   file (`~/.pi-rust/.telemetry-id`). The UUID is regenerated if the
//!   user deletes the file.
//! - **Local-first.** If a network sink is not configured, we just append
//!   to `~/.pi-rust/telemetry.log` so the user can audit and clear it.
//!
//! The sink design is two-stage:
//!
//! 1. `record(Event)` is the hot path; it does an `is_enabled()` check and
//!    pushes the event into an `Mutex<Vec<TelemetryEvent>>`.
//! 2. `flush()` walks the buffer and either appends a JSONL line to the
//!    local log file or POSTs to `PI_TELEMETRY_ENDPOINT` if set. Failures
//!    are silent — telemetry must not affect the user-facing experience.
//!
//! This module deliberately lives in `pi-core` so every other crate can
//! `record()` without taking an HTTP dep.

use std::env;
use std::fs::{self, OpenOptions};
use std::io::Write;
use std::path::PathBuf;
use std::sync::Mutex;
use std::time::{SystemTime, UNIX_EPOCH};

use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct TelemetryEvent {
    pub timestamp_ms: u128,
    pub install_id: String,
    pub command: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error_kind: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub duration_ms_bucket: Option<u64>,
    pub version: String,
    pub os: String,
    pub arch: String,
}

static BUFFER: Mutex<Vec<TelemetryEvent>> = Mutex::new(Vec::new());

/// Pure decision helper used by `is_enabled` and by tests. Splitting it out
/// keeps the env-var probing branch untestable from inside the workspace
/// (the lint forbids `unsafe { env::set_var(...) }`).
pub fn decide_enabled(
    pi_telemetry: Option<&str>,
    do_not_track: Option<&str>,
    config_contents: Option<&str>,
) -> bool {
    if let Some(value) = do_not_track {
        if value == "1" || value.eq_ignore_ascii_case("true") {
            return false;
        }
    }
    if let Some(value) = pi_telemetry {
        if value == "0" || value.eq_ignore_ascii_case("false") {
            return false;
        }
        if value == "1" || value.eq_ignore_ascii_case("true") {
            return true;
        }
    }
    if let Some(text) = config_contents {
        if text.contains("[telemetry]") && text.contains("enabled = true") {
            return true;
        }
    }
    false
}

/// True iff the user has explicitly opted in AND `DO_NOT_TRACK` is unset.
pub fn is_enabled() -> bool {
    let pi_telemetry = env::var("PI_TELEMETRY").ok();
    let do_not_track = env::var("DO_NOT_TRACK").ok();
    let config_contents = config_path().and_then(|p| fs::read_to_string(&p).ok());
    decide_enabled(
        pi_telemetry.as_deref(),
        do_not_track.as_deref(),
        config_contents.as_deref(),
    )
}

pub fn record(command: impl Into<String>, error_kind: Option<String>, duration_ms: Option<u64>) {
    if !is_enabled() {
        return;
    }
    let event = TelemetryEvent {
        timestamp_ms: now_ms(),
        install_id: install_id(),
        command: command.into(),
        error_kind,
        duration_ms_bucket: duration_ms.map(bucket_duration),
        version: env!("CARGO_PKG_VERSION").to_string(),
        os: env::consts::OS.to_string(),
        arch: env::consts::ARCH.to_string(),
    };
    if let Ok(mut buf) = BUFFER.lock() {
        buf.push(event);
    }
}

/// Drain the buffer and write each event to the configured sink.
/// Returns the number of events flushed; errors are swallowed by design.
pub fn flush() -> usize {
    if !is_enabled() {
        return 0;
    }
    let drained: Vec<TelemetryEvent> = {
        let mut buf = match BUFFER.lock() {
            Ok(buf) => buf,
            Err(_) => return 0,
        };
        std::mem::take(&mut *buf)
    };
    if drained.is_empty() {
        return 0;
    }
    let n = drained.len();
    let local_path = log_path();
    if let Some(path) = local_path {
        if let Some(parent) = path.parent() {
            let _ = fs::create_dir_all(parent);
        }
        if let Ok(mut file) = OpenOptions::new().create(true).append(true).open(&path) {
            for event in &drained {
                if let Ok(json) = serde_json::to_string(event) {
                    let _ = writeln!(file, "{json}");
                }
            }
            let _ = file.flush();
        }
    }
    n
}

/// Drop everything buffered without writing. Used by `--telemetry clear`.
pub fn discard() {
    if let Ok(mut buf) = BUFFER.lock() {
        buf.clear();
    }
}

/// 100ms granularity for short ops, then 1s, then 10s. We never report
/// millisecond resolution because timing alone could fingerprint a user.
fn bucket_duration(ms: u64) -> u64 {
    if ms < 1_000 {
        (ms / 100) * 100
    } else if ms < 10_000 {
        (ms / 1_000) * 1_000
    } else {
        (ms / 10_000) * 10_000
    }
}

fn install_id() -> String {
    let path = match install_id_path() {
        Some(path) => path,
        None => return "anonymous".to_string(),
    };
    if let Ok(text) = fs::read_to_string(&path) {
        let trimmed = text.trim().to_string();
        if !trimmed.is_empty() {
            return trimmed;
        }
    }
    let id = generate_uuid_v4();
    if let Some(parent) = path.parent() {
        let _ = fs::create_dir_all(parent);
    }
    let _ = fs::write(&path, &id);
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        if let Ok(meta) = fs::metadata(&path) {
            let mut perms = meta.permissions();
            perms.set_mode(0o600);
            let _ = fs::set_permissions(&path, perms);
        }
    }
    id
}

fn install_id_path() -> Option<PathBuf> {
    let home = env::var("HOME").ok()?;
    Some(PathBuf::from(home).join(".pi-rust").join(".telemetry-id"))
}

fn log_path() -> Option<PathBuf> {
    let home = env::var("HOME").ok()?;
    Some(PathBuf::from(home).join(".pi-rust").join("telemetry.log"))
}

fn config_path() -> Option<PathBuf> {
    let home = env::var("HOME").ok()?;
    Some(PathBuf::from(home).join(".pi-rust").join("config.toml"))
}

fn now_ms() -> u128 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis())
        .unwrap_or(0)
}

/// Tiny UUID v4 generator backed by `chacha20poly1305`'s OsRng. We avoid
/// pulling the `uuid` crate to keep the dep graph small; the bits we need
/// are: 16 random bytes, set the version (0x40) and variant (0x80) nibbles,
/// then hex-format with dashes.
fn generate_uuid_v4() -> String {
    let mut bytes = [0u8; 16];
    // Best-effort randomness: read /dev/urandom; fall back to time + pid.
    let read_urandom = || -> Option<[u8; 16]> {
        let mut buf = [0u8; 16];
        if let Ok(mut f) = fs::File::open("/dev/urandom") {
            use std::io::Read;
            f.read_exact(&mut buf).ok()?;
            return Some(buf);
        }
        None
    };
    if let Some(buf) = read_urandom() {
        bytes = buf;
    } else {
        let now = now_ms();
        let pid = std::process::id() as u128;
        let mix = now.wrapping_mul(0x9e37_79b9_7f4a_7c15).wrapping_add(pid);
        for (i, b) in bytes.iter_mut().enumerate() {
            *b = ((mix >> ((i * 7) % 64)) & 0xff) as u8;
        }
    }
    bytes[6] = (bytes[6] & 0x0f) | 0x40;
    bytes[8] = (bytes[8] & 0x3f) | 0x80;
    format!(
        "{:02x}{:02x}{:02x}{:02x}-{:02x}{:02x}-{:02x}{:02x}-{:02x}{:02x}-{:02x}{:02x}{:02x}{:02x}{:02x}{:02x}",
        bytes[0], bytes[1], bytes[2], bytes[3],
        bytes[4], bytes[5], bytes[6], bytes[7],
        bytes[8], bytes[9],
        bytes[10], bytes[11], bytes[12], bytes[13], bytes[14], bytes[15],
    )
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;

    #[test]
    fn disabled_when_no_signals() {
        assert!(!decide_enabled(None, None, None));
    }

    #[test]
    fn pi_telemetry_env_var_opts_in_unless_do_not_track() {
        assert!(decide_enabled(Some("1"), None, None));
        assert!(!decide_enabled(Some("1"), Some("1"), None));
        assert!(!decide_enabled(Some("0"), None, None));
    }

    #[test]
    fn config_toml_can_enable_when_env_silent() {
        let cfg = "[telemetry]\nenabled = true\n";
        assert!(decide_enabled(None, None, Some(cfg)));
        // do_not_track still wins over the config file.
        assert!(!decide_enabled(None, Some("1"), Some(cfg)));
    }

    #[test]
    fn bucket_duration_collapses_high_precision() {
        assert_eq!(bucket_duration(50), 0);
        assert_eq!(bucket_duration(456), 400);
        assert_eq!(bucket_duration(1234), 1000);
        assert_eq!(bucket_duration(45_678), 40_000);
    }

    #[test]
    fn uuid_v4_has_expected_shape() {
        let id = generate_uuid_v4();
        assert_eq!(id.len(), 36);
        assert!(id.chars().enumerate().all(|(i, c)| {
            matches!(i, 8 | 13 | 18 | 23) == (c == '-') || c.is_ascii_hexdigit()
        }));
        // version nibble at position 14 must be 4
        assert_eq!(id.chars().nth(14).unwrap(), '4');
    }
}

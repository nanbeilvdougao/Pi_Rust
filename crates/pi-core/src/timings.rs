//! Lightweight performance instrumentation that emits chrome://tracing
//! JSON. Off by default; the CLI enables it via `--trace <file>`.
//!
//! Usage:
//!
//! ```ignore
//! pi_core::timings::enable(std::path::PathBuf::from("/tmp/pi.trace.json"));
//! let _span = pi_core::timings::span("provider.complete");
//! // … work …
//! drop(_span); // emits a B + E pair when finalize() runs
//! pi_core::timings::finalize();
//! ```
//!
//! The format mirrors Chrome Trace Event "X" complete events:
//! `{name, cat, ts, dur, ph: "X", pid, tid}`. Chrome / Perfetto load
//! `chrome://tracing` files directly. We accumulate into an in-memory
//! Vec<Event> and write JSON on `finalize()` because keeping a file
//! handle open creates flush-ordering hazards.

use std::path::PathBuf;
use std::sync::Mutex;
use std::time::Instant;

use once_cell_shim::OnceLock;
use serde::Serialize;

mod once_cell_shim {
    pub struct OnceLock<T>(std::sync::OnceLock<T>);
    impl<T> OnceLock<T> {
        pub const fn new() -> Self {
            Self(std::sync::OnceLock::new())
        }
        pub fn get_or_init<F: FnOnce() -> T>(&self, f: F) -> &T {
            self.0.get_or_init(f)
        }
    }
}

#[derive(Debug, Clone, Serialize)]
struct TraceEvent {
    name: String,
    cat: String,
    ph: &'static str,
    ts: u128,
    dur: u128,
    pid: u32,
    tid: u64,
}

#[derive(Default)]
struct State {
    enabled: bool,
    path: Option<PathBuf>,
    started_at: Option<Instant>,
    events: Vec<TraceEvent>,
}

static STATE: OnceLock<Mutex<State>> = OnceLock::new();

fn state() -> &'static Mutex<State> {
    STATE.get_or_init(|| Mutex::new(State::default()))
}

pub fn enable(path: PathBuf) {
    if let Ok(mut s) = state().lock() {
        s.enabled = true;
        s.path = Some(path);
        s.started_at = Some(Instant::now());
        s.events.clear();
    }
}

pub fn is_enabled() -> bool {
    state().lock().map(|s| s.enabled).unwrap_or(false)
}

/// Open a span. The returned guard records a `ph=X` event when dropped.
#[must_use = "drop the guard to record the span end"]
pub fn span(name: &str) -> Span {
    Span::new(name, "agent")
}

pub fn span_in(name: &str, category: &'static str) -> Span {
    Span::new(name, category)
}

pub struct Span {
    name: String,
    category: &'static str,
    start: Instant,
    enabled: bool,
}

impl Span {
    fn new(name: &str, category: &'static str) -> Self {
        let enabled = is_enabled();
        Self {
            name: name.to_string(),
            category,
            start: Instant::now(),
            enabled,
        }
    }
}

impl Drop for Span {
    fn drop(&mut self) {
        if !self.enabled {
            return;
        }
        let dur = self.start.elapsed().as_micros();
        let Ok(mut s) = state().lock() else { return };
        if !s.enabled {
            return;
        }
        let started_at = match s.started_at {
            Some(t) => t,
            None => Instant::now(),
        };
        let ts = self.start.saturating_duration_since(started_at).as_micros();
        s.events.push(TraceEvent {
            name: std::mem::take(&mut self.name),
            cat: self.category.to_string(),
            ph: "X",
            ts,
            dur,
            pid: std::process::id(),
            tid: thread_id_u64(),
        });
    }
}

fn thread_id_u64() -> u64 {
    // ThreadId does not expose a numeric form pre-stabilized API;
    // hash its Debug form into u64 for chrome-tracing.
    use std::collections::hash_map::DefaultHasher;
    use std::hash::{Hash, Hasher};
    let mut h = DefaultHasher::new();
    format!("{:?}", std::thread::current().id()).hash(&mut h);
    h.finish()
}

/// Write the accumulated events to the configured path. Safe to call when
/// disabled (no-op). Clears the buffer.
pub fn finalize() {
    let (path, events) = {
        let Ok(mut s) = state().lock() else { return };
        if !s.enabled {
            return;
        }
        let path = match s.path.clone() {
            Some(path) => path,
            None => return,
        };
        let events = std::mem::take(&mut s.events);
        s.enabled = false;
        (path, events)
    };
    if events.is_empty() {
        return;
    }
    if let Some(parent) = path.parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    if let Ok(json) = serde_json::to_string(&events) {
        let _ = std::fs::write(&path, json);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::tempdir;

    #[test]
    fn span_is_noop_when_disabled() {
        // Make sure prior tests didn't leave it enabled.
        {
            let mut s = state().lock().unwrap();
            s.enabled = false;
            s.events.clear();
        }
        let _span = span("noop");
        std::thread::sleep(std::time::Duration::from_millis(1));
        drop(_span);
        let s = state().lock().unwrap();
        assert!(s.events.is_empty());
    }

    #[test]
    fn enabled_pipeline_writes_chrome_tracing_json() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("trace.json");
        enable(path.clone());
        {
            let _outer = span("outer");
            std::thread::sleep(std::time::Duration::from_millis(2));
            {
                let _inner = span_in("inner", "tool");
                std::thread::sleep(std::time::Duration::from_millis(1));
            }
        }
        finalize();
        let bytes = std::fs::read(&path).expect("trace written");
        let parsed: Vec<serde_json::Value> = serde_json::from_slice(&bytes).expect("parse");
        assert!(parsed.iter().any(|e| e["name"] == "outer"));
        assert!(parsed
            .iter()
            .any(|e| e["name"] == "inner" && e["cat"] == "tool"));
    }
}

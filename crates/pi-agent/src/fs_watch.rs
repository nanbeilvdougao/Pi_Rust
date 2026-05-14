//! Lightweight file-mtime watcher used to hot-reload `.pi/skills/*.md` and
//! `.pi/commands/*.md` without restarting `pi`.
//!
//! TS pi uses `chokidar` which wraps native APIs (kqueue/inotify/FSEvents).
//! We avoid pulling `notify`'s native deps by doing 1-second mtime polling
//! — humans don't expect sub-second skill reload and the dir is tiny, so
//! the I/O overhead is negligible. The trade-off is documented at the call
//! site so a future swap to `notify` is mechanical.
//!
//! The watcher runs on a background thread. Each tick it lists the watched
//! directories, computes a hash of (path, mtime_ms, file_size) tuples, and
//! invokes the user-supplied callback when the hash changes. The callback
//! receives a fresh `SkillSet` and `SlashRegistry` so it can swap them
//! atomically into the agent.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use crate::{SkillSet, SlashRegistry};

#[derive(Debug, Clone)]
pub struct WatchedState {
    pub skills: SkillSet,
    pub slash: SlashRegistry,
}

impl Default for WatchedState {
    fn default() -> Self {
        Self {
            skills: SkillSet::default(),
            slash: SlashRegistry::builtin(),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
struct FileFingerprint {
    mtime_ms: u128,
    size: u64,
}

pub struct WorkspaceWatcher {
    root: PathBuf,
    state: Arc<Mutex<WatchedState>>,
    stop: Arc<AtomicBool>,
    handle: Option<thread::JoinHandle<()>>,
}

impl WorkspaceWatcher {
    /// Start watching `<root>/.pi/{skills,commands}`. Returns a watcher
    /// whose `state()` snapshot the caller can read on every agent turn.
    pub fn start(root: impl AsRef<Path>) -> Self {
        let root = root.as_ref().to_path_buf();
        let initial = build_state(&root);
        let state = Arc::new(Mutex::new(initial));
        let stop = Arc::new(AtomicBool::new(false));
        let handle = {
            let root = root.clone();
            let state = Arc::clone(&state);
            let stop = Arc::clone(&stop);
            thread::Builder::new()
                .name("pi-fs-watch".to_string())
                .spawn(move || run_loop(root, state, stop))
                .ok()
        };
        Self {
            root,
            state,
            stop,
            handle,
        }
    }

    pub fn state(&self) -> WatchedState {
        self.state.lock().map(|s| s.clone()).unwrap_or_default()
    }

    pub fn root(&self) -> &Path {
        &self.root
    }

    pub fn stop(mut self) {
        self.stop.store(true, Ordering::SeqCst);
        if let Some(handle) = self.handle.take() {
            let _ = handle.join();
        }
    }
}

impl Drop for WorkspaceWatcher {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::SeqCst);
    }
}

fn run_loop(root: PathBuf, state: Arc<Mutex<WatchedState>>, stop: Arc<AtomicBool>) {
    let mut prev = scan_fingerprints(&root);
    while !stop.load(Ordering::SeqCst) {
        thread::sleep(Duration::from_secs(1));
        if stop.load(Ordering::SeqCst) {
            break;
        }
        let current = scan_fingerprints(&root);
        if current != prev {
            let rebuilt = build_state(&root);
            if let Ok(mut guard) = state.lock() {
                *guard = rebuilt;
            }
            prev = current;
        }
    }
}

fn scan_fingerprints(root: &Path) -> HashMap<PathBuf, FileFingerprint> {
    let mut map = HashMap::new();
    for sub in [".pi/skills", ".pi/commands"] {
        let dir = root.join(sub);
        let entries = match std::fs::read_dir(&dir) {
            Ok(it) => it,
            Err(_) => continue,
        };
        for entry in entries.flatten() {
            let path = entry.path();
            if path.extension().and_then(|s| s.to_str()) != Some("md") {
                continue;
            }
            if let Ok(meta) = entry.metadata() {
                let mtime_ms = meta
                    .modified()
                    .ok()
                    .and_then(|t| t.duration_since(UNIX_EPOCH).ok())
                    .map(|d| d.as_millis())
                    .unwrap_or(0);
                map.insert(
                    path,
                    FileFingerprint {
                        mtime_ms,
                        size: meta.len(),
                    },
                );
            }
        }
    }
    map
}

fn build_state(root: &Path) -> WatchedState {
    let skills = SkillSet::load_workspace(root);
    let mut slash = SlashRegistry::builtin();
    slash.load_custom(root);
    WatchedState { skills, slash }
}

/// Force a state rebuild and return the fresh snapshot. Used by tests so
/// they don't have to wait for the polling tick.
pub fn rebuild_now(root: &Path) -> WatchedState {
    build_state(root)
}

#[allow(dead_code)]
fn now_ms() -> u128 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis())
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use tempfile::tempdir;

    #[test]
    fn rebuild_picks_up_new_skill() {
        let dir = tempdir().unwrap();
        let skills = dir.path().join(".pi").join("skills");
        fs::create_dir_all(&skills).unwrap();
        fs::write(
            skills.join("style.md"),
            "---\ntrigger = \"always\"\n---\nBe terse.",
        )
        .unwrap();
        let state = rebuild_now(dir.path());
        assert!(state.skills.always_prompt().contains("Be terse"));
    }

    #[test]
    fn fingerprint_changes_with_size() {
        let dir = tempdir().unwrap();
        let skills = dir.path().join(".pi").join("skills");
        fs::create_dir_all(&skills).unwrap();
        fs::write(skills.join("a.md"), "v1").unwrap();
        let snap1 = scan_fingerprints(dir.path());
        // Ensure mtime resolution moves.
        std::thread::sleep(std::time::Duration::from_millis(15));
        fs::write(skills.join("a.md"), "v2-changed").unwrap();
        let snap2 = scan_fingerprints(dir.path());
        assert_ne!(snap1, snap2);
    }

    #[test]
    fn watcher_can_start_and_stop_cleanly() {
        let dir = tempdir().unwrap();
        let watcher = WorkspaceWatcher::start(dir.path());
        let state = watcher.state();
        assert!(state.skills.skills().is_empty());
        watcher.stop();
    }
}

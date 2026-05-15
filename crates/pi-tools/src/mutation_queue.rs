//! Per-path mutation queue.
//!
//! TS pi's `file-mutation-queue.ts` serializes concurrent writes to the same
//! path AND verifies the file did not change underneath us between read and
//! write. We replicate both:
//!
//! - **Serialization**: a global registry maps `canonicalize(path)` to a
//!   `Mutex<()>`. `with_path_lock(path, |&mut|)` holds the lock for the
//!   critical section; siblings on the same path serialize, sibling paths
//!   stay parallel.
//! - **Conflict detection**: `MutationGuard::snapshot()` captures the
//!   on-disk bytes (or "absent") at lock time; `commit()` re-reads and
//!   refuses to write if the file changed since the snapshot. Edits use
//!   this to detect "another process or editor touched the file mid-turn".
//!
//! The queue lives in a process-wide `OnceLock<Mutex<HashMap<...>>>`. It is
//! safe to call from any thread.

use std::collections::HashMap;
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, OnceLock};

use pi_core::{PiError, PiErrorKind, PiResult};

static REGISTRY: OnceLock<Mutex<HashMap<PathBuf, Arc<Mutex<()>>>>> = OnceLock::new();

fn registry() -> &'static Mutex<HashMap<PathBuf, Arc<Mutex<()>>>> {
    REGISTRY.get_or_init(|| Mutex::new(HashMap::new()))
}

fn key_for(path: &Path) -> PathBuf {
    // canonicalize fails if the file does not exist yet (e.g. fresh write).
    // Fall back to the absolute lexical path so create-and-replace flows
    // still serialize against each other.
    fs::canonicalize(path).unwrap_or_else(|_| {
        std::env::current_dir()
            .map(|cwd| cwd.join(path))
            .unwrap_or_else(|_| path.to_path_buf())
    })
}

fn lock_for(path: &Path) -> PiResult<Arc<Mutex<()>>> {
    let key = key_for(path);
    let mut map = registry().lock().map_err(|err| {
        PiError::new(
            PiErrorKind::Tool,
            format!("mutation registry poisoned: {err}"),
        )
    })?;
    Ok(map
        .entry(key)
        .or_insert_with(|| Arc::new(Mutex::new(())))
        .clone())
}

/// Run a critical section that owns the file at `path` for its duration.
/// The closure receives a [`MutationGuard`] that can take an on-disk
/// snapshot and detect external modifications at commit time.
pub fn with_path_lock<F, T>(path: &Path, f: F) -> PiResult<T>
where
    F: FnOnce(&mut MutationGuard) -> PiResult<T>,
{
    let lock = lock_for(path)?;
    let _guard = lock
        .lock()
        .map_err(|err| PiError::new(PiErrorKind::Tool, format!("path lock poisoned: {err}")))?;
    let mut guard = MutationGuard::new(path);
    f(&mut guard)
}

pub struct MutationGuard {
    path: PathBuf,
    snapshot: Option<Snapshot>,
}

enum Snapshot {
    Absent,
    Present(Vec<u8>),
}

impl MutationGuard {
    fn new(path: &Path) -> Self {
        Self {
            path: path.to_path_buf(),
            snapshot: None,
        }
    }

    /// Take an on-disk snapshot at the start of the critical section. Returns
    /// the current bytes (or `None` if the file does not exist yet).
    pub fn snapshot(&mut self) -> PiResult<Option<Vec<u8>>> {
        let snap = match fs::read(&self.path) {
            Ok(bytes) => Snapshot::Present(bytes),
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => Snapshot::Absent,
            Err(err) => {
                return Err(PiError::new(
                    PiErrorKind::Io,
                    format!("快照读取 {} 失败：{err}", self.path.display()),
                ));
            }
        };
        let view = match &snap {
            Snapshot::Absent => None,
            Snapshot::Present(bytes) => Some(bytes.clone()),
        };
        self.snapshot = Some(snap);
        Ok(view)
    }

    /// Verify the on-disk bytes still match the snapshot and then atomically
    /// replace them with `new_contents`. Returns `Err(PiErrorKind::Tool)` if
    /// the file changed since `snapshot`.
    pub fn commit(&self, new_contents: &[u8]) -> PiResult<()> {
        if let Some(snap) = &self.snapshot {
            match (snap, fs::read(&self.path)) {
                (Snapshot::Absent, Err(err)) if err.kind() == std::io::ErrorKind::NotFound => {}
                (Snapshot::Absent, Ok(_)) => {
                    return Err(PiError::new(
                        PiErrorKind::Tool,
                        format!("{} 在锁定期间被另一处创建，拒绝覆盖", self.path.display()),
                    ));
                }
                (Snapshot::Present(expected), Ok(actual)) if expected == &actual => {}
                (Snapshot::Present(_), Ok(_)) => {
                    return Err(PiError::new(
                        PiErrorKind::Tool,
                        format!(
                            "{} 在锁定期间被外部修改，拒绝覆盖（请重新读取后再写）",
                            self.path.display()
                        ),
                    ));
                }
                (Snapshot::Present(_), Err(err)) if err.kind() == std::io::ErrorKind::NotFound => {
                    return Err(PiError::new(
                        PiErrorKind::Tool,
                        format!("{} 在锁定期间被删除，拒绝覆盖", self.path.display()),
                    ));
                }
                (_, Err(err)) => {
                    return Err(PiError::new(
                        PiErrorKind::Io,
                        format!("冲突检测读 {} 失败：{err}", self.path.display()),
                    ));
                }
            }
        }
        if let Some(parent) = self.path.parent() {
            if !parent.as_os_str().is_empty() && !parent.exists() {
                fs::create_dir_all(parent)?;
            }
        }
        // Atomic replace: write to a sibling temp file then rename.
        let tmp = self.path.with_extension(format!(
            "{}.pi-tmp.{}",
            self.path.extension().and_then(|s| s.to_str()).unwrap_or(""),
            std::process::id()
        ));
        fs::write(&tmp, new_contents)?;
        fs::rename(&tmp, &self.path)?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::{Arc, Barrier};
    use std::thread;
    use tempfile::tempdir;

    #[test]
    fn snapshot_then_commit_writes_atomically() {
        let dir = tempdir().unwrap();
        let file = dir.path().join("a.txt");
        fs::write(&file, "before").unwrap();
        with_path_lock(&file, |guard| {
            let snap = guard.snapshot()?.unwrap();
            assert_eq!(snap, b"before".to_vec());
            guard.commit(b"after")?;
            Ok(())
        })
        .unwrap();
        assert_eq!(fs::read_to_string(&file).unwrap(), "after");
    }

    #[test]
    fn external_modification_detected() {
        let dir = tempdir().unwrap();
        let file = dir.path().join("b.txt");
        fs::write(&file, "v1").unwrap();
        let err = with_path_lock(&file, |guard| {
            guard.snapshot()?;
            fs::write(&file, "external").unwrap();
            guard.commit(b"v2")
        })
        .unwrap_err();
        assert!(err.message.contains("外部修改"));
        // Disk reflects the external write, not the would-be agent commit.
        assert_eq!(fs::read_to_string(&file).unwrap(), "external");
    }

    #[test]
    fn concurrent_writers_same_path_serialize() {
        let dir = tempdir().unwrap();
        let file = dir.path().join("c.txt");
        fs::write(&file, "0").unwrap();
        let barrier = Arc::new(Barrier::new(2));
        let f1 = file.clone();
        let b1 = barrier.clone();
        let t1 = thread::spawn(move || {
            b1.wait();
            with_path_lock(&f1, |guard| {
                let snap = guard.snapshot()?.unwrap();
                std::thread::sleep(std::time::Duration::from_millis(50));
                let mut next = String::from_utf8(snap).unwrap();
                next.push('1');
                guard.commit(next.as_bytes())
            })
            .unwrap();
        });
        let f2 = file.clone();
        let b2 = barrier.clone();
        let t2 = thread::spawn(move || {
            b2.wait();
            with_path_lock(&f2, |guard| {
                let snap = guard.snapshot()?.unwrap();
                let mut next = String::from_utf8(snap).unwrap();
                next.push('2');
                guard.commit(next.as_bytes())
            })
            .unwrap();
        });
        t1.join().unwrap();
        t2.join().unwrap();
        // Both writers ran to completion without conflict, because the second
        // one's snapshot reflected the first one's write.
        let final_contents = fs::read_to_string(&file).unwrap();
        assert!(final_contents == "012" || final_contents == "021");
    }
}

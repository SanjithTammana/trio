//! Cross-process exclusive locks held by the OS (`File::lock`) on a sidecar file that is never
//! deleted. The OS drops a lock when its holder exits, so a crashed writer leaves nothing stale
//! to break, and no two processes can ever both believe they hold it.

use std::fs::{File, OpenOptions, TryLockError};
use std::path::Path;
use std::time::{Duration, Instant};

/// Held until dropped.
pub struct Held(File);

/// Take the lock on `path` (created if missing), waiting up to `wait`.
pub fn hold(path: &Path, wait: Duration) -> Result<Held, String> {
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir).map_err(|e| format!("{}: {e}", dir.display()))?;
    }
    let f = OpenOptions::new().create(true).truncate(false).write(true).open(path).map_err(|e| format!("{}: {e}", path.display()))?;
    let deadline = Instant::now() + wait;
    loop {
        match f.try_lock() {
            Ok(()) => return Ok(Held(f)),
            Err(TryLockError::WouldBlock) if Instant::now() < deadline => std::thread::sleep(Duration::from_millis(10)),
            Err(TryLockError::WouldBlock) => return Err(format!("{} is held by another writer; try again", path.display())),
            Err(TryLockError::Error(e)) => return Err(format!("{}: {e}", path.display())),
        }
    }
}

/// Take the lock on `path` now, or `None` when another process holds it.
pub fn try_hold(path: &Path) -> Result<Option<Held>, String> {
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir).map_err(|e| format!("{}: {e}", dir.display()))?;
    }
    let f = OpenOptions::new().create(true).truncate(false).write(true).open(path).map_err(|e| format!("{}: {e}", path.display()))?;
    match f.try_lock() {
        Ok(()) => Ok(Some(Held(f))),
        Err(TryLockError::WouldBlock) => Ok(None),
        Err(TryLockError::Error(e)) => Err(format!("{}: {e}", path.display())),
    }
}

impl Drop for Held {
    fn drop(&mut self) {
        let _ = self.0.unlock();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_held_lock_excludes_until_dropped() {
        let p = std::env::temp_dir().join(format!("trio-oslock-{}.lock", std::process::id()));
        let a = hold(&p, Duration::ZERO).unwrap();
        assert!(hold(&p, Duration::from_millis(50)).is_err(), "a second holder got in");
        drop(a);
        assert!(hold(&p, Duration::ZERO).is_ok(), "the lock was not released");
    }
}

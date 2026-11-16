//! One EC exchange at a time across processes.
//!
//! The EC keeps one reply buffer, and every request goes out with the same transaction id
//! (`0x1f`, see `packet.rs`). So when razer-cli and razer-tray each send a request before
//! either reads its reply, one of them can read the other's reply, and for two identical
//! reads (both polling `0x0d82`) the header check cannot tell. Each request and its reply
//! are therefore done under a file lock that every razer-ctl process takes.
//!
//! Adapted from Finesssee/razer-ctl (MIT; copyright notice in LICENSE), which takes the
//! same lock file, so its tools and these wait for each other too. Unlike there, the wait
//! is bounded: a process that hangs while holding the lock delays the others by
//! [`WAIT`] per exchange and never blocks them, so the lock cannot freeze the tray's menu.
//! The OS releases the lock when the process holding it exits.

use std::fs::{File, OpenOptions, TryLockError};
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

/// How long one exchange waits for the lock before going ahead without it. An exchange
/// holds it for a few milliseconds.
pub const WAIT: Duration = Duration::from_secs(1);

/// The lock file every razer-ctl process uses.
pub fn default_path() -> PathBuf {
    std::env::temp_dir().join("razer-ctl-hid.lock")
}

/// An open lock file. `None` from [`HidLock::open`] means no lock: exchanges go ahead as
/// they did before it existed.
#[derive(Debug)]
pub struct HidLock(File);

/// The lock, held until dropped.
#[derive(Debug)]
pub struct Held<'a>(&'a File);

impl Drop for Held<'_> {
    fn drop(&mut self) {
        let _ = self.0.unlock();
    }
}

impl HidLock {
    pub fn open(path: &Path) -> Option<Self> {
        match OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .open(path)
        {
            Ok(f) => Some(Self(f)),
            Err(e) => {
                log::warn!(
                    "HID lock file {} won't open ({e}); exchanges go ahead without it",
                    path.display()
                );
                None
            }
        }
    }

    /// Take the lock, waiting up to `wait`. `None` when another process kept it that long
    /// or locking failed; the caller then goes ahead without it.
    pub fn hold(&self, wait: Duration) -> Option<Held<'_>> {
        let deadline = Instant::now() + wait;
        loop {
            match self.0.try_lock() {
                Ok(()) => return Some(Held(&self.0)),
                Err(TryLockError::WouldBlock) if Instant::now() < deadline => {
                    std::thread::sleep(Duration::from_millis(2));
                }
                Err(TryLockError::WouldBlock) => {
                    log::warn!(
                        "another process held the HID lock for {} ms; going ahead without it",
                        wait.as_millis()
                    );
                    return None;
                }
                Err(TryLockError::Error(e)) => {
                    log::warn!("HID lock failed ({e}); going ahead without it");
                    return None;
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Instant;

    fn path(name: &str) -> std::path::PathBuf {
        std::env::temp_dir().join(format!(
            "razer-ctl-hid-lock-test-{}-{name}",
            std::process::id()
        ))
    }

    /// Two handles stand for the tray and the CLI: while one holds the lock the other
    /// waits, and once it is released the other gets it.
    #[test]
    fn a_held_lock_makes_the_other_side_wait_until_it_is_released() {
        let p = path("wait");
        let tray = HidLock::open(&p).expect("lock file opens");
        let cli = HidLock::open(&p).expect("lock file opens");
        let held = tray
            .hold(Duration::from_millis(50))
            .expect("free lock is taken");
        let start = Instant::now();
        assert!(
            cli.hold(Duration::from_millis(60)).is_none(),
            "held elsewhere"
        );
        assert!(
            start.elapsed() >= Duration::from_millis(50),
            "it waited first"
        );
        drop(held);
        assert!(cli.hold(Duration::from_millis(50)).is_some(), "released");
        let _ = std::fs::remove_file(&p);
    }

    /// The wait is bounded: a process that hangs holding the lock delays the other side
    /// by the wait and no more, so the lock can never freeze the tray.
    #[test]
    fn the_wait_for_a_lock_that_never_comes_free_is_bounded() {
        let p = path("bounded");
        let a = HidLock::open(&p).unwrap();
        let b = HidLock::open(&p).unwrap();
        let _held = a.hold(Duration::from_millis(10)).unwrap();
        let start = Instant::now();
        assert!(b.hold(Duration::from_millis(100)).is_none());
        assert!(start.elapsed() < Duration::from_millis(1000));
        let _ = std::fs::remove_file(&p);
    }
}

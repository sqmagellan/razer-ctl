//! The config file: loading it without ever losing it, writing it atomically, and noticing
//! when someone edited it by hand while the tray was running.
//!
//! `confy` is still used to locate the file, but no longer to read or write it. Its
//! `store` truncates the file and then writes it in place, so a crash, BSOD or flat battery
//! between the two left an empty or partial file, and the next start replaced it with
//! defaults. Its `load` also reported every failure as one error, so a file that was merely
//! locked for a moment by an editor or antivirus looked the same as a corrupt one, and was
//! treated the same way: defaults, then persisted over the good file.

use std::path::{Path, PathBuf};
use std::time::SystemTime;

use anyhow::{Context, Result};

use crate::state::ConfigState;

/// Size + modification time, to notice an edit made by something other than the tray.
type Stamp = (u64, Option<SystemTime>);

fn stamp_of(path: &Path) -> Option<Stamp> {
    std::fs::metadata(path)
        .ok()
        .map(|m| (m.len(), m.modified().ok()))
}

/// What a check of the file on disk found.
#[derive(Debug)]
pub enum DiskState {
    /// Nobody else wrote it since our last read or write.
    Unchanged,
    /// Someone else wrote it, and it parses.
    Edited(ConfigState),
    /// Someone else wrote it and it does NOT parse (a hand edit in progress, a typo).
    /// Nothing may be saved over it until it is fixed.
    EditedButInvalid,
}

pub struct ConfigFile {
    path: PathBuf,
    /// The on-disk stamp as of our last read or write. A different stamp means someone
    /// else wrote the file.
    stamp: Option<Stamp>,
    /// Set when the file exists but could not be read, so writing defaults over it would
    /// destroy something we have not looked at. Cleared by the next successful load.
    persist_blocked: bool,
}

impl ConfigFile {
    pub fn open() -> Result<Self> {
        Ok(Self::at(confy::get_configuration_file_path(
            crate::PKG_NAME,
            None,
        )?))
    }

    /// A config file at an explicit path (tests).
    pub fn at(path: PathBuf) -> Self {
        Self {
            path,
            stamp: None,
            persist_blocked: false,
        }
    }

    pub fn exists(&self) -> bool {
        self.path.exists()
    }

    /// Whether writes are refused because the file could not be read.
    pub fn is_blocked(&self) -> bool {
        self.persist_blocked
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    /// Load the config. Never fails: the worst case is defaults in memory, with writes
    /// blocked if the file on disk might still hold something worth keeping.
    ///
    /// - Missing file: defaults, and writing them is fine (first run).
    /// - Unparseable file: a timestamped copy is kept beside it, then defaults. Writing is
    ///   allowed only if that copy succeeded, because only then is the original safe.
    /// - Unreadable file (locked, permissions): defaults in memory, writes blocked. The
    ///   next load attempt, or the next persist, tries again.
    pub fn load(&mut self) -> ConfigState {
        // Stamp BEFORE reading: an edit landing between the two then shows up as a change
        // on the next check instead of being silently absorbed.
        let stamp = stamp_of(&self.path);
        // A lock held by an editor or antivirus is usually gone in well under a second.
        let mut read = std::fs::read_to_string(&self.path);
        for _ in 0..3 {
            match &read {
                Err(e) if e.kind() != std::io::ErrorKind::NotFound => {
                    std::thread::sleep(std::time::Duration::from_millis(200));
                    read = std::fs::read_to_string(&self.path);
                }
                _ => break,
            }
        }
        let text = match read {
            Ok(t) => t,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                log::info!("no config at {} yet; using defaults", self.path.display());
                self.persist_blocked = false;
                self.stamp = None;
                return ConfigState::default();
            }
            Err(e) => {
                log::error!(
                    "config at {} could not be READ ({e}); using defaults in memory and NOT \
                     saving until it can be read",
                    self.path.display()
                );
                self.persist_blocked = true;
                return ConfigState::default();
            }
        };
        self.stamp = stamp;
        match toml::from_str::<ConfigState>(&text) {
            Ok(config) => {
                self.persist_blocked = false;
                config
            }
            Err(e) => {
                log::error!("config at {} could not be parsed: {e}", self.path.display());
                self.persist_blocked = !self.salvage();
                ConfigState::default()
            }
        }
    }

    /// Keep a copy of an unparseable file. Timestamped, so a second bad file does not
    /// overwrite the evidence of the first (the old name was first-wins, and then the
    /// second bad file was not kept at all). Returns whether the copy exists.
    fn salvage(&self) -> bool {
        let secs = SystemTime::now()
            .duration_since(SystemTime::UNIX_EPOCH)
            .map(|d| d.as_secs())
            .unwrap_or(0);
        let salvage = self.path.with_extension(format!("toml.invalid-{secs}"));
        match std::fs::copy(&self.path, &salvage) {
            Ok(_) => {
                log::error!(
                    "kept a copy at {} -- fix it and restart, or delete it. Continuing with \
                     defaults.",
                    salvage.display()
                );
                true
            }
            Err(e) => {
                log::error!(
                    "could not preserve the unparseable config ({e}); continuing with \
                     defaults and NOT saving over it"
                );
                false
            }
        }
    }

    /// Whether the file changed since we last read or wrote it.
    pub fn changed_on_disk(&self) -> bool {
        stamp_of(&self.path) != self.stamp
    }

    /// Check whether someone else changed the file, and parse it if so. A half-saved or
    /// mistyped edit is reported (once per distinct version of the file) and left alone:
    /// the stamp is not advanced, so it is checked again, and adopted once it parses.
    pub fn check_disk(&mut self) -> DiskState {
        if !self.changed_on_disk() {
            return DiskState::Unchanged;
        }
        let stamp = stamp_of(&self.path);
        let Ok(text) = std::fs::read_to_string(&self.path) else {
            return DiskState::EditedButInvalid;
        };
        match toml::from_str::<ConfigState>(&text) {
            Ok(config) => {
                self.stamp = stamp;
                self.persist_blocked = false;
                DiskState::Edited(config)
            }
            Err(e) => {
                use std::sync::Mutex;
                static REPORTED: Mutex<Option<Stamp>> = Mutex::new(None);
                let mut last = REPORTED.lock().unwrap_or_else(|p| p.into_inner());
                if *last != stamp {
                    *last = stamp;
                    log::warn!(
                        "config at {} was edited but does not parse ({e}); keeping the running \
                         settings and NOT saving over it until it is fixed",
                        self.path.display()
                    );
                }
                DiskState::EditedButInvalid
            }
        }
    }

    /// Write the config atomically: a sibling temp file, flushed to disk, then renamed
    /// over the original (`MoveFileExW` with replace-existing on Windows). A crash at any
    /// point leaves either the old file or the new one, never a truncated one.
    pub fn store(&mut self, config: &ConfigState) -> Result<()> {
        // No retry-load here: if the file became readable, what it holds is newer than our
        // in-memory defaults, so the right move is to ADOPT it (the tray's periodic
        // `check_disk` does, and that clears the block), not to write over it.
        if self.persist_blocked {
            anyhow::bail!(
                "not saving: the config on disk could not be read, and overwriting it could \
                 destroy it"
            );
        }
        let text = toml::to_string_pretty(config).context("serializing config")?;
        if let Some(dir) = self.path.parent() {
            std::fs::create_dir_all(dir).with_context(|| format!("creating {}", dir.display()))?;
        }
        let tmp = self.path.with_extension("toml.tmp");
        {
            use std::io::Write;
            let mut f = std::fs::File::create(&tmp)
                .with_context(|| format!("creating {}", tmp.display()))?;
            f.write_all(text.as_bytes())?;
            f.sync_all()?;
        }
        std::fs::rename(&tmp, &self.path)
            .with_context(|| format!("replacing {}", self.path.display()))?;
        self.stamp = stamp_of(&self.path);
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn scratch(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "razer-tray-config-test-{}-{name}",
            std::process::id()
        ));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir.join("default-config.toml")
    }

    #[test]
    fn a_missing_file_gives_defaults_and_may_be_written() {
        let mut f = ConfigFile::at(scratch("missing"));
        assert!(!f.exists());
        assert_eq!(f.load(), ConfigState::default());
        f.store(&ConfigState::default()).unwrap();
        assert!(f.exists());
        assert!(
            matches!(f.check_disk(), DiskState::Unchanged),
            "our own write"
        );
    }

    #[test]
    fn a_hand_edit_is_seen_and_a_broken_one_is_never_written_over() {
        let path = scratch("edit");
        let mut f = ConfigFile::at(path.clone());
        f.store(&ConfigState::default()).unwrap();

        let edited = ConfigState {
            enforce: true,
            ..ConfigState::default()
        };
        // Different length as well as mtime, so the test doesn't depend on timestamp
        // resolution.
        std::fs::write(
            &path,
            toml::to_string_pretty(&edited).unwrap() + "\n# edited\n",
        )
        .unwrap();
        match f.check_disk() {
            DiskState::Edited(cfg) => assert!(cfg.enforce),
            other => panic!("expected the edit, got {other:?}"),
        }

        std::fs::write(&path, "this is [not toml").unwrap();
        assert!(matches!(f.check_disk(), DiskState::EditedButInvalid));
        // Still invalid on a second look, and still on disk as the user left it.
        assert!(matches!(f.check_disk(), DiskState::EditedButInvalid));
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "this is [not toml");
    }

    #[test]
    fn an_unparseable_file_is_salvaged_before_anything_replaces_it() {
        let path = scratch("salvage");
        std::fs::write(&path, "garbage = [").unwrap();
        let mut f = ConfigFile::at(path.clone());
        assert_eq!(f.load(), ConfigState::default());
        assert!(!f.is_blocked(), "salvage succeeded, so writing is allowed");
        let salvaged = std::fs::read_dir(path.parent().unwrap())
            .unwrap()
            .filter_map(|e| e.ok())
            .any(|e| e.file_name().to_string_lossy().contains("toml.invalid-"));
        assert!(salvaged, "a timestamped copy must exist");
    }

    #[test]
    fn a_store_replaces_the_file_whole() {
        let path = scratch("atomic");
        let mut f = ConfigFile::at(path.clone());
        let cfg = ConfigState {
            enforce: true,
            ..ConfigState::default()
        };
        f.store(&cfg).unwrap();
        let back: ConfigState = toml::from_str(&std::fs::read_to_string(&path).unwrap()).unwrap();
        assert_eq!(back, cfg);
        assert!(
            !path.with_extension("toml.tmp").exists(),
            "no temp file left behind"
        );
    }
}

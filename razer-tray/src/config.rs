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
        let path = confy::get_configuration_file_path(crate::PKG_NAME, None)?;
        Ok(Self {
            path,
            stamp: None,
            persist_blocked: false,
        })
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
        let text = match std::fs::read_to_string(&self.path) {
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
        self.stamp = stamp_of(&self.path);
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

    /// Re-read the file if someone else changed it. `Some` only for a successful parse;
    /// a half-saved edit that fails to parse is left alone until it is fixed (the stamp is
    /// not advanced, so it is retried on the next check).
    pub fn reload_if_changed(&mut self) -> Option<ConfigState> {
        if !self.changed_on_disk() {
            return None;
        }
        let text = std::fs::read_to_string(&self.path).ok()?;
        match toml::from_str::<ConfigState>(&text) {
            Ok(config) => {
                self.stamp = stamp_of(&self.path);
                self.persist_blocked = false;
                Some(config)
            }
            Err(e) => {
                log::warn!(
                    "config at {} changed on disk but does not parse yet ({e}); keeping the \
                     running settings",
                    self.path.display()
                );
                None
            }
        }
    }

    /// Write the config atomically: a sibling temp file, flushed to disk, then renamed
    /// over the original (`MoveFileExW` with replace-existing on Windows). A crash at any
    /// point leaves either the old file or the new one, never a truncated one.
    pub fn store(&mut self, config: &ConfigState) -> Result<()> {
        // No retry-load here: if the file became readable, what it holds is newer than our
        // in-memory defaults, so the right move is to ADOPT it (the tray's periodic
        // `reload_if_changed` does, and that clears the block), not to write over it.
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

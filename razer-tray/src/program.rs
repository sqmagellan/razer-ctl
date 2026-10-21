//! `ProgramState`: the live application state behind the tray -- current intent
//! (`device_state`), the last-read reality (`observed`), the AC/battery profiles,
//! and the current menu + its handler map. Owns rendering (icon/tooltip), the
//! perf-mode cycle, persistence, and applying a new state to the device.

use anyhow::Result;
use std::collections::HashMap;

use librazer::command;
use librazer::device;
use librazer::keyboard::{Frame, KeyboardPreset};
use librazer::types::BatteryCare;
use librazer::unsupported::{Optional, Unsupported};
use tray_icon::menu::Menu;

use librazer::observe::{self, ConfigHealth, Mode, Reading, Unset};

use crate::config::{ConfigFile, DiskState};
use crate::menu;
use crate::state::{
    brightness_to_percent, get_fan_rpm, ActionSession, AppProfile, ConfigState, DeviceState,
    FanRpm, FanRpmFilter, FanSpeed, PerfMode, PowerModeBefore,
};

/// How a user-chosen state was produced, because it decides what reaches the saved profile.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum Pick {
    /// An explicit menu choice: an absolute value the user selected.
    Menu,
    /// The left-click cycle: "the mode after the one shown". During an app Action the one
    /// shown is the Action's, so the result is derived from the Action, not chosen, and it
    /// must not be baked into the saved profile (it used to be: a click during a Hyperboost
    /// rule saved Custom(Boost, High) as the AC profile).
    Cycle,
}

/// UTF-16 code units the tray tooltip may occupy.
///
/// MEASURED, not from the docs. `NOTIFYICONDATAW::szTip` is declared `[u16; 128]` and the
/// modern shell honors 128 -- but only when `cbSize` identifies a struct version that has
/// the long field. `tray-icon` 0.19 builds its `NOTIFYICONDATAW` with `..std::mem::zeroed()`,
/// leaving **`cbSize` = 0**, which matches no declared version; the shell accepts the call
/// (it does not return an error) and then behaves like the original layout, where `szTip` was
/// `[u16; 64]`.
///
/// Confirmed on hardware: an 83-unit tooltip was accepted without error, yet the display cut
/// mid-way through the 🔋 surrogate pair, and the cut point moved with the length of the perf
/// mode name. 63 leaves room for the NUL terminator inside those 64 units.
const TOOLTIP_MAX_UTF16: usize = 63;

/// Push a tooltip to the tray icon, logging the first OS rejection.
///
/// Every call site used to discard this `Result` with `let _ =`, so a rejected update was
/// completely invisible: the tray would keep displaying a stale tooltip forever with
/// nothing in the log to say why. If `Shell_NotifyIcon` ever refuses the modify, we want
/// to know once rather than never.
pub fn set_tooltip_logged(tray_icon: &tray_icon::TrayIcon, tooltip: &str) {
    if let Err(e) = tray_icon.set_tooltip(Some(tooltip)) {
        use std::sync::atomic::{AtomicBool, Ordering};
        static LOGGED: AtomicBool = AtomicBool::new(false);
        if !LOGGED.swap(true, Ordering::Relaxed) {
            log::warn!("set_tooltip rejected by the shell: {e:?}");
        }
    }
}

/// Log the tooltip once at startup, and again whenever the dGPU fields appear or
/// disappear.
///
/// Two lines in normal operation, so it's cheap enough to leave on at Info. It exists
/// because "the tooltip isn't showing X" was otherwise undiagnosable without a debugger
/// on the user's desktop: the string handed to Windows was never recorded anywhere. The
/// UTF-16 count is included because that, not the character count, is what `szTip`
/// truncates on.
fn log_tooltip_transition(tooltip: &str) {
    use std::sync::atomic::{AtomicU8, Ordering};

    /// Neither present nor absent yet, so the first call always logs.
    const UNKNOWN: u8 = 2;
    static LAST_HAD_GPU: AtomicU8 = AtomicU8::new(UNKNOWN);

    let has_gpu = u8::from(tooltip.contains("\nGPU "));
    if LAST_HAD_GPU.swap(has_gpu, Ordering::Relaxed) != has_gpu {
        log::info!(
            "tooltip ({} UTF-16 units, dGPU fields {}): {:?}",
            tooltip.chars().map(char::len_utf16).sum::<usize>(),
            if has_gpu == 1 { "present" } else { "absent" },
            tooltip
        );
    }
}

/// How long a failed fan RPM read pauses the next ones (see `refresh_fan`).
const FAN_READ_PAUSE: std::time::Duration = std::time::Duration::from_secs(60);

/// Resyncs for a perf/fan mode the EC acknowledged and didn't take, before the tray gives
/// up until the next change. On the resync backoff they come about 2, 4 and 8 s apart:
/// room for a write that was ignored as the laptop woke (2026-09-24), while a model that
/// always ignores a mode doesn't keep the tray writing forever.
const PERF_RETRIES: u8 = 3;

/// Retries of a perf/fan mode the EC acknowledged and didn't take (see
/// `librazer::error::PerfNotApplied`), counted for one intended state.
#[derive(Debug, Default)]
pub struct PerfRetry {
    target: Option<DeviceState>,
    misses: u8,
    /// The last apply's mode wasn't taken. The handle answered, so the resync for it
    /// needn't reopen the device.
    pub pending: bool,
    /// The last apply gave up on it, so the reconcile after it must not re-assert it.
    pub gave_up: bool,
}

pub struct ProgramState {
    pub device_state: DeviceState,
    pub observed: DeviceState,
    /// Turns off the zone 2 fan drift check if repairing it stops working.
    pub zone2_guard: librazer::state::Zone2DriftGuard,
    pub perf_retry: PerfRetry,
    pub ac_state: DeviceState,
    pub battery_state: DeviceState,
    pub event_handlers: HashMap<String, DeviceState>,
    pub menu: Menu,
    /// Last fan reading cleared for display, or `None` before any read could be trusted
    /// (rendered as "…" rather than as a number the filter had rejected).
    pub fan_actual: Option<FanRpm>,
    /// Debouncer standing between raw `0x0d88` reads and `fan_actual`. Individual reads
    /// are not trustworthy on this hardware -- roughly 3 in 10 carried one impossible
    /// zone -- so nothing may write `fan_actual` except [`Self::refresh_fan`].
    fan_filter: FanRpmFilter,
    /// No fan RPM reads before this. A failed read costs up to 5 tries per zone on the UI
    /// thread (about 4 s); issue #2's Blade 15 2018 fails every one, so hovering froze the
    /// tray. A failure pauses the reads for [`FAN_READ_PAUSE`].
    fan_reads_paused_until: Option<std::time::Instant>,
    pub ac_power: bool,
    /// Windows has said which power source this is at least once. Until then `ac_power` is
    /// a guess (AC), which a config with every profile set acts on as it always has, and a
    /// profile still waiting for its first reading never does (see [`Self::mode`]).
    pub power_known: bool,
    /// No adoption before this instant. Set at a power-source change: the EC switches some
    /// settings per source by itself (on `0x029F` the keyboard brightness, 82% on AC and 88%
    /// on battery, measured 2026-10-14), and a read at the switch saves the old value.
    pub adopt_not_before: Option<std::time::Instant>,
    /// Profiles never set: a new config's. Each is read from the laptop the first time the
    /// tray sees that power source, and never applied before.
    pub unset: Unset,
    pub enforce: bool,
    /// Re-assert the intended profile on wake even when `enforce` is off (config-driven).
    pub reassert_on_resume: bool,
    /// "Actions" rules: while a listed process runs, force its perf mode (config-driven).
    pub app_profiles: Vec<AppProfile>,
    /// The running Action, if any. Lives here rather than in the event loop so a resync
    /// keeps it: it used to be a local in `main`, and recovery rebuilt everything else, so
    /// a recovered tray thought the rule was still applied while the device had dropped it.
    pub action: Option<ActionSession>,
    /// Usable manual-fan RPM bounds for this chassis (from `Descriptor::fan_rpm_range`),
    /// captured once at startup so menu rebuilds don't need the `Device` on hand.
    pub fan_rpm_range: (u16, u16),
    /// This laptop's USB PID, for the fan floors measured per perf mode
    /// (`librazer::unsupported::MANUAL_FLOOR_BY_PERF`); set by `main` once the device is
    /// known, like `custom_colors`. 0 matches no row.
    pub pid: u16,
    /// The last device exchange failed; the event loop retries [`Self::sync`] with backoff
    /// instead of blocking inside the loop.
    pub needs_sync: bool,
    /// Whether that sync must re-apply intent (a write failed, or first contact), or only
    /// probe with a read (a read failed: the device may be fine, and re-writing intent
    /// would overwrite a CLI or Synapse change the user made on purpose).
    pub sync_apply: bool,
    /// Switch the Windows power mode along with the perf mode (config, opt-in).
    pub match_power_mode: bool,
    /// What each power source's Windows power mode was before matching changed it (config).
    pub power_mode_before: PowerModeBefore,
    /// Kept only so a save doesn't drop it; read once at startup by `main`.
    pub cycle_perf_hotkey: Option<String>,
    /// Shown as a disabled item at the top of the menu (e.g. Synapse is running).
    pub warning: Option<String>,
    /// Refresh rates the display offers at its current resolution; empty hides the menu.
    pub refresh_rates: Vec<u32>,
    /// This model's keyboard geometry is mapped (`keyboard::supports_custom_frame`); set by
    /// `main` once the device is known. Off, the color menu is hidden and nothing is painted.
    pub custom_colors: bool,
    /// Settings this laptop turned out not to have: their menus are disabled and the
    /// tooltip leaves them out. See `librazer::unsupported`.
    pub unsupported: Unsupported,
    /// Named colors for the menu (config).
    pub keyboard_presets: Vec<KeyboardPreset>,
    /// Draw the battery bar over a custom color (config).
    pub battery_bar: bool,
    /// The last frame written to the keyboard, so an unchanged frame is not re-sent. `None`
    /// forces the next paint. Frames cannot be read back, so this is the only record.
    painted: Option<Frame>,
    config_file: ConfigFile,
}

impl ProgramState {
    /// Build the tray's state from the config. No device I/O, so it cannot fail on a
    /// device that isn't ready yet; [`Self::sync`] is the part that talks to the EC.
    /// `power` is `None` when Windows can't say; AC is then assumed, as before.
    pub fn new(
        config: ConfigState,
        config_file: ConfigFile,
        power: Option<bool>,
        fan_rpm_range: (u16, u16),
    ) -> Result<Self> {
        let ac_power = power.unwrap_or(true);
        let ac_state = config.ac_state.normalized(fan_rpm_range);
        let battery_state = config.battery_state.normalized(fan_rpm_range);
        let device_state = if ac_power { ac_state } else { battery_state };
        let (menu, event_handlers) = menu::build(
            &device_state,
            &menu::MenuOptions {
                enforce: config.enforce,
                fan_rpm_range,
                pid: 0,
                warning: None,
                config_problem: None,
                offer_reset: false,
                match_power_mode: config.match_windows_power_mode,
                windows_power_mode: None,
                refresh_rates: &[],
                custom_colors: false,
                unsupported: Unsupported::default(),
                keyboard_presets: &config.keyboard_presets,
                battery_bar: config.keyboard_battery_bar,
                ac_power,
            },
        )?;
        Ok(Self {
            device_state,
            observed: device_state,
            zone2_guard: Default::default(),
            perf_retry: PerfRetry::default(),
            ac_state,
            battery_state,
            event_handlers,
            menu,
            fan_actual: None,
            fan_filter: FanRpmFilter::default(),
            fan_reads_paused_until: None,
            ac_power,
            power_known: power.is_some(),
            adopt_not_before: None,
            unset: config.unset(),
            enforce: config.enforce,
            reassert_on_resume: config.reassert_on_resume,
            app_profiles: config.app_profiles,
            action: None,
            fan_rpm_range,
            pid: 0,
            needs_sync: true,
            sync_apply: true,
            match_power_mode: config.match_windows_power_mode,
            power_mode_before: config.windows_power_mode_before,
            cycle_perf_hotkey: config.cycle_perf_hotkey,
            warning: None,
            refresh_rates: Vec::new(),
            custom_colors: false,
            unsupported: Unsupported::default(),
            keyboard_presets: config.keyboard_presets,
            battery_bar: config.keyboard_battery_bar,
            painted: None,
            config_file,
        })
    }

    pub fn menu_options(&self) -> menu::MenuOptions<'_> {
        menu::MenuOptions {
            enforce: self.enforce,
            fan_rpm_range: self.fan_rpm_range,
            pid: self.pid,
            warning: self.warning.as_deref(),
            config_problem: self.config_problem(),
            offer_reset: self.config_file.health() == ConfigHealth::Corrupt,
            match_power_mode: self.match_power_mode,
            windows_power_mode: if self.match_power_mode {
                crate::platform::windows_power_mode()
            } else {
                None
            },
            refresh_rates: &self.refresh_rates,
            custom_colors: self.custom_colors,
            unsupported: self.unsupported,
            keyboard_presets: &self.keyboard_presets,
            battery_bar: self.battery_bar,
            ac_power: self.ac_power,
        }
    }

    /// Rebuild the menu from the current intent, and show it if a tray icon is given.
    /// The one place the menu is rebuilt, so a new option can't be forgotten at one of
    /// several call sites.
    pub fn rebuild_menu(&mut self, tray_icon: Option<&tray_icon::TrayIcon>) {
        match menu::build(&self.device_state, &self.menu_options()) {
            Ok((m, h)) => {
                self.menu = m;
                self.event_handlers = h;
                if let Some(tray) = tray_icon {
                    tray.set_menu(Some(Box::new(self.menu.clone())));
                }
            }
            Err(e) => log::warn!("menu rebuild failed: {e:?}"),
        }
    }

    /// Pick up the settings `device` has learned it lacks, rebuilding the menu when that
    /// changed. Call after each device write, and after each read before `observe`.
    pub fn track_unsupported(
        &mut self,
        device: &device::Device,
        tray_icon: Option<&tray_icon::TrayIcon>,
    ) {
        let now = librazer::state::unsupported_on(device);
        if now != self.unsupported {
            self.unsupported = now;
            self.rebuild_menu(tray_icon);
        }
    }

    /// The saved profile for the current power source.
    pub fn saved_profile(&self) -> DeviceState {
        if self.ac_power {
            self.ac_state
        } else {
            self.battery_state
        }
    }

    /// Whether the config may be applied now, or the tray only reads the device (and
    /// adopts what it reads into a profile that was never set). Every path that writes
    /// something the user did not just pick asks this first: convergence, Enforce, the
    /// startup and resync apply, wake re-asserts and zone repairs (via [`Self::reconcile`]).
    pub fn mode(&self) -> Mode {
        observe::mode(
            self.config_file.health(),
            self.power_known.then_some(self.ac_power),
            self.unset,
        )
    }

    /// The menu line for a config the tray is not applying.
    fn config_problem(&self) -> Option<&'static str> {
        match self.config_file.health() {
            ConfigHealth::Corrupt => Some("The settings file has an error: not applying it"),
            ConfigHealth::Unreadable => Some("The settings file can't be read: not applying it"),
            ConfigHealth::Usable => None,
        }
    }

    /// Take in a successful device reading. Every read of the whole state goes through
    /// here (Mirror, hover, reconcile, resync probe), so a profile waiting for its first
    /// reading gets the first one taken on its power source, and while the config is not
    /// applied the intent tracks the device.
    ///
    /// Adopting writes nothing to the device: the reading already is the device's state
    /// (`DeviceState::read` forbids writing one back).
    pub fn observe(&mut self, observed: DeviceState, tray_icon: Option<&tray_icon::TrayIcon>) {
        // Placeholders for settings this laptop lacks must not become intent.
        let observed = observed.with_lacking_from(&self.device_state, self.unsupported);
        self.observed = observed;
        let reading = observe::on_reading(
            self.mode(),
            &self.device_state,
            &observed,
            self.fan_rpm_range,
        );
        match reading {
            Reading::Display => {}
            Reading::Follow(intent) => {
                if intent != self.device_state {
                    self.device_state = intent;
                    self.rebuild_menu(tray_icon);
                }
            }
            Reading::Adopt { .. }
                if self
                    .adopt_not_before
                    .is_some_and(|t| std::time::Instant::now() < t) =>
            {
                log::debug!("power source just changed; adopting after it settles");
            }
            Reading::Adopt { ac_power, profile } => {
                let placeholder = if ac_power {
                    std::mem::replace(&mut self.ac_state, profile)
                } else {
                    std::mem::replace(&mut self.battery_state, profile)
                };
                self.unset.clear(ac_power);
                log::info!(
                    "adopted the laptop's current settings as the {} profile (nothing written): \
                     {profile:?}",
                    if ac_power { "plugged-in" } else { "battery" }
                );
                // An Action running meanwhile was seeded from the placeholder; move it, and
                // any pick made during it, onto the adopted profile.
                if let Some(session) = &mut self.action {
                    if let Some(rule) = self.app_profiles.get(session.rule) {
                        session.power_changed(rule, &placeholder, &profile, self.fan_rpm_range);
                    }
                }
                // What the device has. A running Action differs from it, and convergence
                // applies the Action as it would on any other profile.
                self.device_state = profile;
                if let Err(e) = self.persist() {
                    log::warn!("could not save the adopted profile: {e:?}");
                }
                // Windows-side only (the power-mode slider, when matching is on).
                self.sync_windows_power_mode(self.device_state.perf_mode);
                self.rebuild_menu(tray_icon);
            }
        }
    }

    /// The user picked "Reset settings to defaults" while the config doesn't parse. The
    /// file becomes a new install's, profiles unset, so the next reading adopts the
    /// laptop's settings instead of pushing defaults onto it. The broken file's
    /// timestamped copy stays beside it.
    pub fn reset_config(&mut self, tray_icon: Option<&tray_icon::TrayIcon>) {
        let fresh = ConfigState::first_run();
        match self.config_file.reset(&fresh) {
            Ok(()) => {
                self.adopt_config(fresh);
                self.rebuild_menu(tray_icon);
            }
            Err(e) => log::warn!("could not reset the settings: {e:?}"),
        }
    }

    /// What the device should be in right now: the running Action's session state when
    /// one is active and allowed on this power source, else the saved profile.
    pub fn target(&self) -> DeviceState {
        match &self.action {
            Some(session)
                if self
                    .app_profiles
                    .get(session.rule)
                    .is_some_and(|r| r.allowed_on(self.ac_power)) =>
            {
                session.effective
            }
            _ => self.saved_profile(),
        }
        // Whatever produced it, never aim at a state the EC cannot hold: that never
        // converges, and the loop would re-apply it every tick.
        .normalized(self.fan_rpm_range)
    }

    /// Push the current intent to the device and reconcile against a read-back. Used at
    /// startup and to recover after a failed exchange. Unlike the old recovery path it
    /// does NOT reload the config: the intent in memory is the user's latest choice, and
    /// reloading used to revert a change whose only failure was the read that followed it.
    pub fn sync(
        &mut self,
        tray_icon: &mut tray_icon::TrayIcon,
        device: &device::Device,
    ) -> Result<()> {
        // The config isn't to be applied (it doesn't parse, can't be read yet, or a profile
        // waits for its first reading): probe instead of writing what's in memory, which is
        // only defaults or a placeholder. The probe's reading is adopted where it should be.
        let mode = self.mode();
        if !self.sync_apply || !mode.writes_config() {
            // After a failed READ: probe. A successful read ends the resync; the device is
            // written only if Enforce says so, exactly as on any other read.
            let probe = DeviceState::read(device);
            // Diverged zones are a broken state, not a user's choice: repair them (the
            // Mirror's policy). Checked before is_retryable, which also counts them and
            // would otherwise keep the probe failing forever without ever writing.
            let diverged = matches!(&probe, Err(e)
                if e.downcast_ref::<librazer::error::PerfZonesDiverged>().is_some());
            match probe {
                Ok(observed) => self.observe(observed, Some(tray_icon)),
                Err(e) if !diverged && librazer::error::is_retryable(&e) => return Err(e),
                Err(_) => {}
            }
            self.needs_sync = false;
            let write = observe::probe_may_write(mode, self.enforce, diverged);
            let reason = if mode.writes_config() {
                "sync (after a read failure)"
            } else {
                "sync (not applying the config)"
            };
            self.reconcile(tray_icon, device, reason, write);
            return Ok(());
        }
        // Allowed to touch the device now, even though needs_sync is still set. Aimed at the
        // current target: a plug or unplug while the resync was pending changed it, and
        // applying the stale state first wrote the old source's profile. Picks made
        // meanwhile are in the target already (saved profile or Action session).
        self.device_state = self.target();
        self.needs_sync = false;
        let applied = self.apply_and_refresh(tray_icon, device);
        // Reconcile whatever the apply did: a write the EC rejected is exactly when the
        // read-back matters. Read only, after the apply gave up on an ignored mode.
        self.reconcile(tray_icon, device, "sync", !self.perf_retry.gave_up);
        if let Err(e) = applied {
            self.needs_sync = true;
            return Err(e);
        }
        Ok(())
    }

    /// Poll the fans and update the displayed value, dropping reads the filter rejects.
    ///
    /// The ONLY path that may write `fan_actual`. A failed HID read and a read the filter
    /// distrusts are handled identically -- the previous value stands -- because a stale
    /// RPM is a smaller lie than an impossible one.
    pub fn refresh_fan(&mut self, device: &device::Device) {
        let now = std::time::Instant::now();
        if self.fan_reads_paused_until.is_some_and(|until| now < until) {
            return;
        }
        match get_fan_rpm(device) {
            Ok(sample) => {
                self.fan_reads_paused_until = None;
                if let Some(accepted) = self.fan_filter.accept(sample) {
                    self.fan_actual = Some(accepted);
                }
            }
            Err(e) => {
                log::warn!("fan RPM read failed, pausing fan reads for {FAN_READ_PAUSE:?}: {e:#}");
                self.fan_reads_paused_until = Some(now + FAN_READ_PAUSE);
            }
        }
    }

    fn config_snapshot(&self) -> ConfigState {
        // An unset profile is saved as the placeholder it was loaded with, whatever is in
        // memory: `ConfigState::unset` counts a changed value as a hand edit that sets it.
        let placeholder = ConfigState::default();
        ConfigState {
            ac_state: if self.unset.ac {
                placeholder.ac_state
            } else {
                self.ac_state
            },
            battery_state: if self.unset.battery {
                placeholder.battery_state
            } else {
                self.battery_state
            },
            ac_state_unset: self.unset.ac,
            battery_state_unset: self.unset.battery,
            enforce: self.enforce,
            reassert_on_resume: self.reassert_on_resume,
            app_profiles: self.app_profiles.clone(),
            match_windows_power_mode: self.match_power_mode,
            windows_power_mode_before: self.power_mode_before.clone(),
            cycle_perf_hotkey: self.cycle_perf_hotkey.clone(),
            keyboard_presets: self.keyboard_presets.clone(),
            keyboard_battery_bar: self.battery_bar,
        }
    }

    /// Persist the current AC/battery profiles + enforce flag + Actions config. Single
    /// source of truth so a future ConfigState field can't be silently dropped by one of
    /// the call sites.
    ///
    /// Hand edits are never written over. Every user action calls
    /// [`Self::reload_config_if_changed`] first, so an edit is adopted before the action is
    /// applied on top of it. If one still lands in between, it is adopted here and this
    /// save is skipped. An edit that doesn't parse blocks saving until it is fixed: the old
    /// code wrote straight over a mistyped hand edit on the next Fn-key brightness change.
    pub fn persist(&mut self) -> Result<()> {
        match self.config_file.check_disk() {
            DiskState::Unchanged => {
                let snapshot = self.config_snapshot();
                self.config_file.store(&snapshot)
            }
            DiskState::Edited(disk) => {
                log::warn!(
                    "config was edited on disk; adopting the edit instead of saving over it"
                );
                self.adopt_config(*disk);
                Ok(())
            }
            DiskState::EditedButInvalid => {
                anyhow::bail!(
                    "the config on disk has an edit that does not parse; not saving over it"
                )
            }
        }
    }

    /// Pick up a hand edit made while the tray runs, showing the rebuilt menu. Returns
    /// whether anything was adopted; the event loop then converges on [`Self::target`].
    pub fn reload_config_if_changed(&mut self, tray_icon: &tray_icon::TrayIcon) -> bool {
        match self.config_file.check_disk() {
            DiskState::Edited(disk) => {
                log::info!("config changed on disk; reloading it");
                self.adopt_config(*disk);
                self.rebuild_menu(Some(tray_icon));
                true
            }
            DiskState::Unchanged | DiskState::EditedButInvalid => false,
        }
    }

    fn adopt_config(&mut self, disk: ConfigState) {
        self.unset = disk.unset();
        self.ac_state = disk.ac_state.normalized(self.fan_rpm_range);
        self.battery_state = disk.battery_state.normalized(self.fan_rpm_range);
        self.enforce = disk.enforce;
        self.reassert_on_resume = disk.reassert_on_resume;
        self.match_power_mode = disk.match_windows_power_mode;
        self.power_mode_before = disk.windows_power_mode_before;
        self.cycle_perf_hotkey = disk.cycle_perf_hotkey;
        self.keyboard_presets = disk.keyboard_presets;
        self.battery_bar = disk.keyboard_battery_bar;
        // Keep a running Action whose rule is unchanged at the same index; any other
        // change ends it, and the next process scan starts afresh.
        if let Some(session) = &self.action {
            if disk.app_profiles.get(session.rule) != self.app_profiles.get(session.rule) {
                self.action = None;
            }
        }
        self.app_profiles = disk.app_profiles;
        self.rebuild_menu(None);
    }

    pub fn handle_event(&self, event_id: &str) -> Option<DeviceState> {
        self.event_handlers.get(event_id).copied()
    }

    /// The next perf mode after the one on the device's intent. `normalized` in `update`
    /// clears max fan when the cycle leaves Custom, which the plain struct update did not.
    pub fn get_next_perf_mode(&self) -> DeviceState {
        DeviceState {
            perf_mode: crate::state::next_perf_mode(self.device_state.perf_mode),
            ..self.device_state
        }
    }

    pub fn tooltip(&self) -> Result<String> {
        // Render from `observed` (the device's last-read real state) so the tray
        // reflects reality, including changes made outside the tray (e.g. Fn keys).
        let s = &self.observed;

        // Priorities: 0 is never dropped, larger numbers are shed first when the layout
        // exceeds TOOLTIP_MAX_UTF16. Ordered by how much the field tells you that you
        // can't already see: the perf mode is the app's whole identity, dGPU temp is
        // invisible without us, and the logo color is literally visible on the lid.
        const P_MODE: u8 = 0;
        const P_FAN: u8 = 1;
        const P_GPU_TEMP: u8 = 2;
        const P_FAN_RPM: u8 = 3;
        const P_CHARGE: u8 = 4;
        const P_BRIGHTNESS: u8 = 5;
        const P_GPU_WATTS: u8 = 6;
        const P_LOGO: u8 = 7;

        let mode = match s.perf_mode {
            PerfMode::Battery => "Battery".to_string(),
            PerfMode::Silent => "Silent".to_string(),
            PerfMode::Balanced => "Balanced".to_string(),
            PerfMode::Performance => "Performance".to_string(),
            PerfMode::Hyperboost => "Hyperboost".to_string(),
            PerfMode::Custom(cpu_boost, gpu_boost) => {
                format!("Custom (CPU {cpu_boost:?}, GPU {gpu_boost:?})")
            }
        };

        // The set-point register reads back whatever was written, even a value the EC is
        // ignoring, so it is never shown as if it were the speed. 0 in Manual is the
        // fresh-boot register value, not a request for 0; below the chassis floor the fan
        // actually runs at the floor (measured: 800 set -> 2000 actual), and below a perf
        // mode's measured floor at that one (0x029F in Custom: 3000 set -> 3800 actual).
        let (floor, _) = self.fan_rpm_range;
        let fan = match s.fan_speed {
            FanSpeed::Auto => "Fan Auto".to_string(),
            FanSpeed::Manual(0) => "Fan Manual".to_string(),
            FanSpeed::Manual(rpm) => {
                // Max Fan drives the fans past any set point; "(max)" below says so.
                let mode_floor = (!s.max_fan)
                    .then(|| {
                        librazer::unsupported::manual_fan_runs_at(self.pid, s.perf_mode.wire(), rpm)
                    })
                    .flatten();
                match mode_floor {
                    Some(mode_floor) => format!("Fan {mode_floor} floor"),
                    None if rpm < floor => format!("Fan {floor} floor"),
                    None => format!("Fan {rpm} set"),
                }
            }
        };
        let fan = if s.max_fan {
            format!("{fan} (max)")
        } else {
            fan
        };

        let mut fan_line = vec![(P_FAN, fan)];
        fan_line.push((
            P_FAN_RPM,
            match self.fan_actual {
                Some(f) => format!("{}/{}", f.fan1, f.fan2),
                None => "…".to_string(),
            },
        ));

        // dGPU telemetry, when a reading is available. Omitted entirely otherwise -- a
        // machine with no dGPU must not see "0°C", which reads as a measurement.
        let mut gpu_line = Vec::new();
        if let Some((temp_c, watts)) = crate::platform::gpu_telemetry() {
            gpu_line.push((P_GPU_TEMP, format!("GPU {temp_c}°C")));
            if watts > 0.0 {
                gpu_line.push((P_GPU_WATTS, format!("{watts:.0}W")));
            }
        }

        let mut lights_line = Vec::new();
        let has = |f: Optional| !self.unsupported.contains(f);
        if has(Optional::BatteryCare) && s.battery_care != BatteryCare::DISABLE {
            lights_line.push((P_CHARGE, format!("🔋 {}%", s.battery_care.to_percent())));
        }
        if has(Optional::KeyboardBrightness) && s.lights_mode.keyboard_brightness > 0 {
            lights_line.push((
                P_BRIGHTNESS,
                format!(
                    "🔆 {}%",
                    brightness_to_percent(s.lights_mode.keyboard_brightness)
                ),
            ));
        }
        // No always-on marker: it was a bare 💡 at the end of this line, the one field with no
        // value, and it read as a number that had gone missing. The menu shows the setting.
        if has(Optional::Logo) {
            lights_line.push((P_LOGO, format!("Logo {:?}", s.lights_mode.logo_mode)));
        }

        let layout = vec![vec![(P_MODE, mode)], fan_line, gpu_line, lights_line];

        let out = crate::state::fit_tooltip(&layout, TOOLTIP_MAX_UTF16);
        log_tooltip_transition(&out);
        Ok(out)
    }

    pub fn icon(&self) -> tray_icon::Icon {
        // Raw RGBA baked by build.rs -- no runtime image decoder, so the `image` crate
        // stays out of the shipped binary entirely (see build.rs for why). Each blob is
        // exactly ICON_SIZE^2 * 4 bytes at the size build.rs produced.
        const ICON_EDGE: u32 = 64;
        // Indexed by perf mode, in the order build.rs writes them.
        const ICON_RGBA: [&[u8]; 6] = [
            include_bytes!(concat!(env!("OUT_DIR"), "/icon-blue.rgba")), // 0 Battery
            include_bytes!(concat!(env!("OUT_DIR"), "/icon-yellow.rgba")), // 1 Silent
            include_bytes!(concat!(env!("OUT_DIR"), "/icon-green.rgba")), // 2 Balanced
            include_bytes!(concat!(env!("OUT_DIR"), "/icon-red.rgba")),  // 3 Performance
            include_bytes!(concat!(env!("OUT_DIR"), "/icon-violet.rgba")), // 4 Hyperboost
            include_bytes!(concat!(env!("OUT_DIR"), "/icon-brown.rgba")), // 5 Custom
        ];

        let idx = match self.observed.perf_mode {
            PerfMode::Battery => 0,
            PerfMode::Silent => 1,
            PerfMode::Balanced => 2,
            PerfMode::Performance => 3,
            PerfMode::Hyperboost => 4,
            PerfMode::Custom(_, _) => 5,
        };
        // from_rgba wants an owned Vec; this is a 16 KiB copy on a path that runs at most
        // a few times a second, and it replaces a full PNG decode.
        tray_icon::Icon::from_rgba(ICON_RGBA[idx].to_vec(), ICON_EDGE, ICON_EDGE)
            .expect("baked icon is ICON_EDGE^2 RGBA by construction")
    }

    /// Apply the current `device_state` and refresh the tray UI (icon/tooltip/menu).
    ///
    /// On a failed apply the display is refreshed from a fresh READ, not from intent, so
    /// the tray shows what the device actually did; the error is returned so the event
    /// loop schedules a [`Self::sync`] retry. UI and fan-read failures are logged, never
    /// returned: a telemetry read that fails after a successful write used to abort the
    /// whole update and send the tray into a full re-init that reverted the change.
    fn apply_and_refresh(
        &mut self,
        tray_icon: &mut tray_icon::TrayIcon,
        device: &device::Device,
    ) -> Result<()> {
        // A resync is pending: record the intent and show it, but leave the device to the
        // resync, which applies exactly this intent. Writing through a stale handle costs
        // ~2 s per exchange of retries, on the UI thread.
        if self.needs_sync {
            self.sync_apply = true;
            // Windows-side settings don't need the device.
            self.apply_os_settings();
            self.rebuild_menu(Some(tray_icon));
            self.refresh_ui(tray_icon);
            return Ok(());
        }
        self.perf_retry.gave_up = false;
        // The EC can acknowledge a perf/fan mode write and ignore it, so read the mode back,
        // also after another write failed: an ignored mode must still get its retries. Not
        // after bus trouble, where the read would fail the same way.
        let result = match self.device_state.apply(device) {
            Err(e) if librazer::error::is_retryable(&e) => Err(e),
            applied => match self.device_state.confirm_modes(device) {
                Err(e) if librazer::error::is_perf_not_applied(&e) => Err(e),
                confirmed => applied.and(confirmed),
            },
        };
        match &result {
            Ok(()) => self.observed = self.device_state,
            Err(e) => {
                log::warn!("apply failed: {e:?}");
                if let Ok(real) = DeviceState::read(device) {
                    self.unsupported = librazer::state::unsupported_on(device);
                    self.observed = real.with_lacking_from(&self.device_state, self.unsupported);
                }
            }
        }
        // After the effect write, which would otherwise replace the color.
        self.paint_keyboard(device, true);
        self.apply_os_settings();
        self.unsupported = librazer::state::unsupported_on(device);
        self.rebuild_menu(Some(tray_icon));
        self.refresh_fan(device);
        self.refresh_ui(tray_icon);
        // Only bus trouble is worth a resync. A write the EC rejected will be rejected
        // again, and resyncing on it re-sent the same write every minute forever; the
        // display already shows what the device really did (read back above). A mode the
        // EC acknowledged and ignored gets PERF_RETRIES resyncs per intended state, then is
        // left as read. Bus trouble in between keeps the count.
        self.perf_retry.pending = false;
        match result {
            Err(e) if librazer::error::is_perf_not_applied(&e) => {
                if self.perf_retry.target != Some(self.device_state) {
                    self.perf_retry.target = Some(self.device_state);
                    self.perf_retry.misses = 0;
                }
                self.perf_retry.misses += 1;
                if self.perf_retry.misses > PERF_RETRIES {
                    log::warn!("{e}; giving up after {PERF_RETRIES} retries until the next change");
                    self.perf_retry = PerfRetry {
                        gave_up: true,
                        ..PerfRetry::default()
                    };
                    Ok(())
                } else {
                    self.perf_retry.pending = true;
                    Err(e)
                }
            }
            Err(e) if librazer::error::is_retryable(&e) => Err(e),
            _ => {
                self.perf_retry = PerfRetry::default();
                Ok(())
            }
        }
    }

    /// The settings that live in Windows rather than in the EC: the profile's display
    /// refresh rate, and (opt-in) the Windows power mode that matches the perf mode.
    /// Failures are logged; neither is worth a resync.
    fn apply_os_settings(&mut self) {
        if let Some(hz) = self.device_state.display_refresh_hz {
            if let Err(e) = crate::platform::set_refresh_rate(hz) {
                log::warn!("refresh rate {hz} Hz: {e:?}");
            }
        }
        self.sync_windows_power_mode(self.device_state.perf_mode);
    }

    /// Matching on: set this power source's Windows power mode for `perf`, first noting
    /// the mode it had so turning matching off can put it back. Matching off: put back a
    /// mode noted for this power source, if any (the other source's waits until the next
    /// plug or unplug). Failures are logged.
    pub fn sync_windows_power_mode(&mut self, perf: PerfMode) {
        let slot = self.power_mode_before.slot(self.ac_power);
        if self.match_power_mode {
            // Saved BEFORE Windows is changed: a crash in between must not leave the
            // matched mode to be noted as the one to put back.
            if slot.is_none() {
                *slot = crate::platform::windows_power_mode().map(str::to_string);
                if slot.is_some() {
                    self.persist_power_mode_before();
                }
            }
            if let Err(e) = crate::platform::set_windows_power_mode(perf) {
                log::warn!("Windows power mode: {e:?}");
            }
        } else if let Some(before) = slot.clone() {
            // Forgotten only once it is back, so a failure is retried on the next sync.
            match crate::platform::set_windows_power_mode_named(&before) {
                Ok(()) => {
                    log::info!("Windows power mode put back to {before}");
                    *slot = None;
                    self.persist_power_mode_before();
                }
                Err(e) => log::warn!("Windows power mode: putting back {before}: {e:?}"),
            }
        }
    }

    fn persist_power_mode_before(&mut self) {
        if let Err(e) = self.persist() {
            log::warn!("Failed to persist the Windows power mode to put back: {e:?}");
        }
    }

    /// Re-render icon and tooltip from `observed`.
    pub fn refresh_ui(&self, tray_icon: &tray_icon::TrayIcon) {
        if let Err(e) = tray_icon.set_icon(Some(self.icon())) {
            log::warn!("set_icon failed: {e:?}");
        }
        if let Ok(tooltip) = self.tooltip() {
            set_tooltip_logged(tray_icon, &tooltip);
        }
    }

    /// Apply a user-chosen state. Its fields reach the saved profile for the current power
    /// source (see [`Pick`] for the one exception) and are persisted BEFORE the device is
    /// touched, so a failed write cannot lose the choice: the retry re-applies it.
    ///
    /// While an Action runs, the pick also holds on the device until the Action's process
    /// exits, instead of being reverted on the next tick.
    pub fn update(
        &mut self,
        tray_icon: &mut tray_icon::TrayIcon,
        picked: DeviceState,
        pick: Pick,
        device: &device::Device,
    ) -> Result<()> {
        // Adopt a hand edit first, so the pick lands on top of it instead of the save
        // that follows either overwriting the edit or discarding the pick.
        self.reload_config_if_changed(tray_icon);
        let picked = picked.normalized(self.fan_rpm_range);
        // What the menu was built from: its difference from `picked` is exactly what the
        // user changed. See `DeviceState::carry_changes`.
        let before = self.device_state;
        let for_saved = match (pick, &self.action) {
            (Pick::Cycle, Some(_)) => DeviceState {
                perf_mode: before.perf_mode,
                max_fan: before.max_fan,
                ..picked
            },
            _ => picked,
        };
        let saved = DeviceState::carry_changes(self.saved_profile(), before, for_saved)
            .normalized(self.fan_rpm_range);
        // A pick is applied in every mode: it is the user's choice, not the config's. It
        // reaches a profile only while the config is applied. A profile waiting for its
        // first reading takes the pick with it (the reading sees what was picked); one
        // that doesn't parse, or a guessed power source, would put it somewhere wrong.
        let mode = self.mode();
        if !mode.writes_config() {
            log::info!("pick not saved to a profile ({mode:?})");
        } else if self.ac_power {
            self.ac_state = saved;
        } else {
            self.battery_state = saved;
        }
        if let Some(session) = &mut self.action {
            session.pick(picked);
        }
        self.device_state = picked;
        if let Err(e) = self.persist() {
            log::warn!("could not save the config: {e:?}");
        }
        log::info!("state updated to {:?} ({pick:?})", picked);
        self.apply_and_refresh(tray_icon, device)
    }

    /// Apply a state that must NOT touch the saved AC/battery profiles: an Action
    /// starting or ending, an AC/battery switch, a config reload.
    pub fn update_transient(
        &mut self,
        tray_icon: &mut tray_icon::TrayIcon,
        new_device_state: DeviceState,
        device: &device::Device,
    ) -> Result<()> {
        self.device_state = new_device_state.normalized(self.fan_rpm_range);
        log::info!("transient state applied {:?}", self.device_state);
        self.apply_and_refresh(tray_icon, device)
    }

    /// A custom color is set and this model can show it.
    pub fn has_custom_color(&self) -> bool {
        self.custom_colors && self.device_state.lights_mode.keyboard_color.is_some()
    }

    /// Show the custom keyboard color, if one is set: rendered from intent, with the battery
    /// bar when enabled. Unless `force`, an unchanged frame is not re-sent, because every
    /// write lights the backlight back up; `force` is for when the EC may have dropped it
    /// (after an apply, a wake, or a device reopen). Failures are logged: it's cosmetic.
    pub fn paint_keyboard(&mut self, device: &device::Device, force: bool) {
        if !self.custom_colors {
            return;
        }
        let battery = if self.battery_bar {
            crate::platform::battery_level()
        } else {
            None
        };
        // Follow-perf-mode matches the icon, which shows what the device reports: a CLI
        // change with Enforce off moves the icon, so it moves the color too.
        let shown = DeviceState {
            perf_mode: self.observed.perf_mode,
            ..self.device_state
        };
        let Some(frame) = shown.keyboard_frame(battery) else {
            self.painted = None;
            return;
        };
        if !force && self.painted == Some(frame) {
            return;
        }
        match command::set_keyboard_frame(device, &frame) {
            Ok(()) => self.painted = Some(frame),
            Err(e) => {
                log::warn!("keyboard color: {e:?}");
                self.painted = None;
            }
        }
    }

    /// Read the device and compare it with intent; log any drift, and re-assert when
    /// `write` is set.
    ///
    /// This is the one reconcile used at startup, after a resync, and after a wake. The
    /// wake path used to write blind on the first tick with no read-back, although the
    /// comment beside it said a just-woken EC can ACK a write without switching. A read
    /// first also makes the wake path measurable: every "drift after wake" line in the log
    /// is evidence for whether the EC loses state across Modern Standby at all.
    ///
    /// Zones that disagree (an interrupted two-zone write) count as drift, and are
    /// repaired whenever `write` is set.
    ///
    /// Nothing is written while the config is not applied ([`Self::mode`]): the intent is
    /// then only defaults or a placeholder, and the reading is what the tray follows.
    pub fn reconcile(
        &mut self,
        tray_icon: &mut tray_icon::TrayIcon,
        device: &device::Device,
        reason: &str,
        write: bool,
    ) {
        let was_applying = self.mode().writes_config();
        let drift = match DeviceState::read(device) {
            Ok(observed) => {
                self.track_unsupported(device, Some(tray_icon));
                self.observe(observed, Some(tray_icon));
                observed.drifted_from(&self.device_state, device, &mut self.zone2_guard)
            }
            Err(e)
                if e.downcast_ref::<librazer::error::PerfZonesDiverged>()
                    .is_some() =>
            {
                log::warn!("{reason}: {e}");
                true
            }
            Err(e) => {
                log::warn!("{reason}: device read failed, skipping: {e:?}");
                return;
            }
        };
        // Not when the config was not applied before this read either: a read that just
        // adopted a profile must not be followed by a write from that same profile.
        let write = write && was_applying && self.mode().writes_config();

        if !drift {
            log::info!("{reason}: device matches intended state");
        } else {
            log::warn!(
                "{reason}: device {:?}/{:?} != intended {:?}/{:?}{}",
                self.observed.perf_mode,
                self.observed.fan_speed,
                self.device_state.perf_mode,
                self.device_state.fan_speed,
                if write {
                    "; re-asserting"
                } else {
                    " (not re-asserting)"
                }
            );
            if write {
                match self.device_state.enforce_to(device) {
                    Err(e) => log::warn!("{reason}: re-assert failed: {e:?}"),
                    Ok(()) => match DeviceState::read(device) {
                        Ok(real) => {
                            if real.enforced_fields_differ_on(&self.device_state, device) {
                                log::warn!("{reason}: still differs after re-assert: {real:?}");
                            }
                            self.observed =
                                real.with_lacking_from(&self.device_state, self.unsupported);
                        }
                        Err(_) => {
                            let brightness = self.observed.lights_mode.keyboard_brightness;
                            self.observed = self.device_state;
                            self.observed.lights_mode.keyboard_brightness = brightness;
                        }
                    },
                }
            }
        }
        self.refresh_ui(tray_icon);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn test_state(ac_state: DeviceState) -> ProgramState {
        let mut state = ProgramState::new(
            ConfigState {
                ac_state,
                ..ConfigState::default()
            },
            ConfigFile::open().expect("config path resolves"),
            Some(true),
            (2200, 5000),
        )
        .expect("state builds");
        state.fan_actual = Some(FanRpm {
            fan1: 2600,
            fan2: 2500,
        });
        state
    }

    /// A Manual set point the fan cannot run at is never displayed as the speed.
    #[test]
    fn the_tooltip_never_shows_an_ignored_set_point_as_the_speed() {
        let mut state = test_state(DeviceState::default());
        state.observed.fan_speed = FanSpeed::Manual(0);
        assert!(state.tooltip().unwrap().contains("Fan Manual"));
        state.observed.fan_speed = FanSpeed::Manual(800);
        assert!(state.tooltip().unwrap().contains("Fan 2200 floor"));
        state.fan_actual = None;
        assert!(state.tooltip().unwrap().contains('…'));
    }

    /// 2026-10-20: in Custom on 0x029F a 3000 set point reads back while the fans hold
    /// 3800. The tooltip names the floor, as it does for the chassis floor.
    #[test]
    fn the_tooltip_shows_the_custom_floor_not_a_set_point_below_it() {
        use librazer::types::{CpuBoost, GpuBoost};
        let custom = PerfMode::Custom(CpuBoost::Medium, GpuBoost::High);
        let mut state = test_state(DeviceState::default());
        state.pid = 0x029f;
        state.observed.perf_mode = custom;
        state.observed.fan_speed = FanSpeed::Manual(3000);
        let tip = state.tooltip().unwrap();
        assert!(tip.contains("Fan 3800 floor"), "{tip}");
        assert!(!tip.contains("3000"), "{tip}");
        // Below both floors, the higher one is what the fans run at.
        state.observed.fan_speed = FanSpeed::Manual(800);
        assert!(state.tooltip().unwrap().contains("Fan 3800 floor"));
        // Above it the set point is followed.
        state.observed.fan_speed = FanSpeed::Manual(4500);
        assert!(state.tooltip().unwrap().contains("Fan 4500 set"));
        // Balanced follows 3000, and a model without a measured floor is unchanged.
        state.observed.perf_mode = PerfMode::Balanced;
        state.observed.fan_speed = FanSpeed::Manual(3000);
        assert!(state.tooltip().unwrap().contains("Fan 3000 set"));
        // Max Fan overrides the set point; the tooltip keeps "set" and adds "(max)".
        state.observed.perf_mode = custom;
        state.observed.max_fan = true;
        assert!(state.tooltip().unwrap().contains("Fan 3000 set (max)"));
        state.observed.max_fan = false;
        state.pid = 0x02a0;
        assert!(state.tooltip().unwrap().contains("Fan 3000 set"));
    }

    /// The layout must fit the real szTip budget for every perf mode, including the
    /// longest possible Custom label -- that mode name is what pushed the old tooltip
    /// past the limit and made Windows cut the battery emoji in half.
    #[test]
    fn every_perf_mode_fits_the_tooltip_budget() {
        use librazer::types::{CpuBoost, GpuBoost};
        use strum::IntoEnumIterator;

        let mut modes: Vec<PerfMode> = vec![
            PerfMode::Battery,
            PerfMode::Silent,
            PerfMode::Balanced,
            PerfMode::Performance,
            PerfMode::Hyperboost,
        ];
        for cpu in CpuBoost::iter() {
            for gpu in GpuBoost::iter() {
                modes.push(PerfMode::Custom(cpu, gpu));
            }
        }

        for mode in modes {
            let mut state = test_state(DeviceState {
                perf_mode: mode,
                ..DeviceState::default()
            });
            state.observed = state.device_state;

            let tip = state.tooltip().expect("tooltip renders");
            let units = librazer::state::utf16_units(&tip);
            assert!(
                units <= TOOLTIP_MAX_UTF16,
                "{mode:?} rendered {units} units (limit {TOOLTIP_MAX_UTF16}): {tip:?}"
            );
        }
    }

    /// The perf mode is priority 0, so it survives no matter how much has to be shed.
    #[test]
    fn the_perf_mode_is_never_dropped() {
        let mut state = test_state(DeviceState {
            perf_mode: PerfMode::Custom(
                librazer::types::CpuBoost::Undervolt,
                librazer::types::GpuBoost::High,
            ),
            ..DeviceState::default()
        });
        state.observed = state.device_state;

        let tip = state.tooltip().expect("tooltip renders");
        assert!(tip.starts_with("Custom"), "mode was dropped: {tip:?}");
    }

    // ---- first run, an unknown power source, a config that doesn't parse ----------

    const RANGE: (u16, u16) = (2200, 5000);

    /// A config file in its own scratch directory, never the user's.
    fn scratch_config(name: &str) -> std::path::PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "razer-tray-program-test-{}-{name}",
            std::process::id()
        ));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir.join("default-config.toml")
    }

    fn on_disk(path: &std::path::Path) -> ConfigState {
        toml::from_str(&std::fs::read_to_string(path).unwrap()).unwrap()
    }

    fn reading(perf_mode: PerfMode) -> DeviceState {
        DeviceState {
            perf_mode,
            fan_speed: FanSpeed::Manual(3000),
            ..DeviceState::default()
        }
    }

    #[test]
    fn a_first_run_adopts_each_profile_on_its_own_power_source() {
        let path = scratch_config("first-run");
        let mut file = ConfigFile::at(path.clone());
        let config = file.load();
        let mut state = ProgramState::new(config, file, Some(true), RANGE).unwrap();
        state.persist().unwrap();
        assert_eq!(state.mode(), Mode::Adopt { ac_power: true });

        let on_ac = reading(PerfMode::Performance);
        state.observe(on_ac, None);
        assert_eq!(state.ac_state, on_ac);
        assert_eq!(state.device_state, on_ac);
        assert_eq!(state.target(), state.device_state, "nothing to converge on");
        assert_eq!(state.mode(), Mode::Apply);
        assert!(state.unset.battery, "battery waits for its own reading");
        let saved = on_disk(&path);
        assert_eq!(saved.ac_state, on_ac);
        assert!(!saved.ac_state_unset);
        assert!(saved.battery_state_unset);
        assert_eq!(saved.battery_state, ConfigState::default().battery_state);

        // Unplugged: the battery profile is read, not applied from defaults.
        state.ac_power = false;
        assert_eq!(state.mode(), Mode::Adopt { ac_power: false });
        let on_battery = reading(PerfMode::Silent);
        state.observe(on_battery, None);
        assert_eq!(state.battery_state, on_battery);
        assert_eq!(state.ac_state, on_ac, "the AC profile is untouched");
        assert_eq!(state.unset, Unset::default());
        assert_eq!(on_disk(&path).unset(), Unset::default());
    }

    #[test]
    fn an_unknown_power_source_only_follows_the_device() {
        let path = scratch_config("power-unknown");
        let mut file = ConfigFile::at(path);
        let config = file.load();
        let mut state = ProgramState::new(config, file, None, RANGE).unwrap();
        assert_eq!(
            state.mode(),
            Mode::Observe(observe::ObserveReason::PowerUnknown)
        );
        let seen = reading(PerfMode::Hyperboost);
        state.observe(seen, None);
        assert_eq!(state.device_state, seen, "the menu checks what is set");
        assert_eq!(
            state.ac_state,
            ConfigState::default().ac_state,
            "not adopted"
        );
        assert!(state.unset.ac && state.unset.battery);
        // Once Windows answers, the next reading is adopted.
        state.power_known = true;
        assert_eq!(state.mode(), Mode::Adopt { ac_power: true });
    }

    #[test]
    fn a_valid_config_is_applied_as_before() {
        let path = scratch_config("valid");
        let mut file = ConfigFile::at(path.clone());
        file.store(&ConfigState::default()).unwrap();
        let config = file.load();
        assert_eq!(config, ConfigState::default());
        for power in [Some(true), None] {
            let mut file = ConfigFile::at(path.clone());
            let config = file.load();
            let mut state = ProgramState::new(config, file, power, RANGE).unwrap();
            assert_eq!(state.mode(), Mode::Apply);
            let intent = state.device_state;
            state.observe(reading(PerfMode::Silent), None);
            assert_eq!(state.device_state, intent, "a reading only displays");
            assert_eq!(state.observed, reading(PerfMode::Silent));
        }
    }

    #[test]
    fn a_config_that_does_not_parse_is_observed_until_reset() {
        let path = scratch_config("corrupt");
        std::fs::write(&path, "ac_state = [").unwrap();
        let mut file = ConfigFile::at(path.clone());
        let config = file.load();
        let mut state = ProgramState::new(config, file, Some(true), RANGE).unwrap();
        assert_eq!(
            state.mode(),
            Mode::Observe(observe::ObserveReason::ConfigCorrupt)
        );
        assert!(state.menu_options().offer_reset);
        assert!(state.menu_options().config_problem.is_some());

        let seen = reading(PerfMode::Performance);
        state.observe(seen, None);
        assert_eq!(state.device_state, seen);
        assert!(state.persist().is_err(), "nothing saved over it");
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "ac_state = [");

        state.reset_config(None);
        assert!(!state.menu_options().offer_reset);
        assert_eq!(state.mode(), Mode::Adopt { ac_power: true });
        assert_eq!(on_disk(&path), ConfigState::first_run());
        state.observe(seen, None);
        assert_eq!(
            state.ac_state, seen,
            "the reset adopts, it does not push defaults"
        );
    }
}

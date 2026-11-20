//! `ProgramState`: the live application state behind the tray -- current intent
//! (`device_state`), the last-read reality (`observed`), the AC/battery profiles,
//! and the current menu + its handler map. Owns rendering (icon/tooltip), the
//! perf-mode cycle, persistence, and applying a new state to the device.

use anyhow::Result;
use std::collections::HashMap;

use librazer::command;
use librazer::cpu_cap::{self, CpuCap};
use librazer::device;
use librazer::guard::{self, FanGuard, FanGuardChange, FanGuardLimits, GuardReading, Observation};
use librazer::keyboard::{Frame, KeyboardPreset};
use librazer::panel::PanelMode;
use librazer::types::{BatteryCare, PowerSource};
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

    // "GPU 47°C", which may follow "CPU 62°C" on its line; not the Custom mode's "GPU High".
    let has_gpu = u8::from(
        tooltip
            .match_indices("GPU ")
            .any(|(i, _)| tooltip[i + 4..].starts_with(|c: char| c.is_ascii_digit())),
    );
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

/// Shortest gap between EC temperature reads. They ride on the fan reads, which come
/// every 500 ms while the icon is hovered.
const EC_TEMP_EVERY: std::time::Duration = std::time::Duration::from_secs(2);
/// How long a failed EC temperature read pauses the next ones, like [`FAN_READ_PAUSE`]: a
/// failed read retries on the UI thread.
const EC_TEMP_PAUSE: std::time::Duration = FAN_READ_PAUSE;
/// Older than this, an EC temperature is left out of the tooltip rather than shown stale.
const EC_TEMP_STALE: std::time::Duration = std::time::Duration::from_secs(30);

/// How often the charger (`0x078c`) is read while plugged in: one HID round trip (about
/// 4 ms on `0x029F`), so a USB-C monitor's charger is noticed within half a minute.
const CHARGER_EVERY: std::time::Duration = std::time::Duration::from_secs(30);
/// How long a charger reading stands when the reads after it fail: three missed reads. One
/// failed read must not lift the clamp (a Performance write) only for the next to put it
/// back (a Balanced write).
const CHARGER_FRESH: std::time::Duration = std::time::Duration::from_secs(95);
/// Plugged in with the charger not read yet (at start, after a plug): the perf mode is held
/// to Balanced until a read answers, for at most this long. A USB-C plug used to run the
/// AC profile's Performance for the 5 s before the first read, then drop to Balanced. A
/// model without the read answers "not supported" at once and is never held; one whose
/// reads keep failing is held this long, then left unheld (as before the guard) with a
/// line in the log.
const CHARGER_PENDING_MAX: std::time::Duration = std::time::Duration::from_secs(60);
/// While the first read is pending, a failed one is retried after this, not after
/// [`CHARGER_EVERY`].
const CHARGER_PENDING_RETRY: std::time::Duration = std::time::Duration::from_secs(5);
/// How often the fan guard reads the EC temperatures while it watches a manual fan,
/// whether or not anyone is at the keyboard (a game on a controller makes no input).
const FAN_GUARD_EVERY: std::time::Duration = std::time::Duration::from_secs(5);

/// Why the weak-charger guard holds the perf mode to Balanced.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum ChargerHold {
    /// The EC reports a USB-C charger.
    UsbC(PowerSource),
    /// Plugged in, and the first charger read on this source hasn't answered yet.
    Pending,
}

/// The Mini-LED panel's firmware selection (see `librazer::panel`).
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct PanelState {
    /// What the firmware had selected when the tray started: the mode the panel runs in,
    /// unless something changed it earlier in this boot.
    pub at_start: PanelMode,
    /// What it has selected now, for the next start.
    pub selected: PanelMode,
}

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
    /// The EC's temperatures (`0x0d85`) as last read, and when, for the tooltip. `None`
    /// before the first read, and for good once the model turns out not to have them.
    pub ec_temps: Option<(librazer::types::EcTemperatures, std::time::Instant)>,
    /// No EC temperature read before this.
    ec_temps_next: Option<std::time::Instant>,
    /// The EC lacks the command, or answers in a layout we don't know: never ask again.
    ec_temps_unsupported: bool,
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
    /// The running Action's CPU frequency cap, and what to put back when it ends.
    cpu_cap: CpuCap,
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
    /// Hold the perf mode to Balanced on a USB-C charger (config `weak_charger_guard`).
    pub weak_charger_guard: bool,
    /// The charger as the EC last reported it, and when. Read only while plugged in;
    /// cleared at every power-source change until the next read.
    charger: Option<(PowerSource, std::time::Instant)>,
    /// No charger read before this; `None` = read now.
    charger_next: Option<std::time::Instant>,
    /// The EC lacks `0x078c`: never ask again, and never clamp (as before the guard).
    charger_unsupported: bool,
    /// A failed charger read has been logged since the last good one.
    charger_failure_logged: bool,
    /// Since when the first charger read on this power source is awaited; the perf mode
    /// is held to Balanced meanwhile (see [`CHARGER_PENDING_MAX`]).
    charger_pending_since: Option<std::time::Instant>,
    /// Raise a manual fan while hot (config `fan_guard`), and where.
    fan_guard_on: bool,
    fan_guard_limits: FanGuardLimits,
    /// The triggers as the config gave them, so a save writes them back as they were.
    fan_guard_cpu_c: Option<u8>,
    fan_guard_gpu_c: Option<u8>,
    fan_guard: FanGuard,
    /// No fan guard temperature read before this.
    fan_guard_next: Option<std::time::Instant>,
    /// The Mini-LED dual-mode panel, on a laptop that has one; set by `main`.
    pub panel: Option<PanelState>,
    /// The state the menu was last built from. Its handlers are this state with one field
    /// changed, so it is what [`Self::update`] compares a pick with.
    menu_base: DeviceState,
    /// The disabled lines at the top of the menu that say what a guard holds.
    guard_notes: Vec<String>,
}

/// Which Mini-LED panel mode Windows is running, from the modes it offers on the internal
/// panel: a 3840x2400 mode is there only in UHD+, and a 1920x1200 mode at 240 Hz or more
/// only in FHD+ (the two native modes razer-helper documents). Each is one matching mode,
/// never maxima taken separately. Both or neither (a panel this doesn't describe) is
/// `None`, and the caller takes the firmware's selection instead.
pub fn panel_running(modes: &[(u32, u32, u32)]) -> Option<PanelMode> {
    let uhd = modes.iter().any(|&(w, h, _)| (w, h) == (3840, 2400));
    let fhd = modes
        .iter()
        .any(|&(w, h, hz)| (w, h) == (1920, 1200) && hz >= 240);
    match (uhd, fhd) {
        (true, false) => Some(PanelMode::Uhd120),
        (false, true) => Some(PanelMode::Fhd240),
        _ => None,
    }
}

/// The perf mode's name, for a menu line.
fn perf_name(perf: PerfMode) -> &'static str {
    match perf {
        PerfMode::Battery => "Battery",
        PerfMode::Silent => "Silent",
        PerfMode::Balanced => "Balanced",
        PerfMode::Performance => "Performance",
        PerfMode::Hyperboost => "Hyperboost",
        PerfMode::Custom(..) => "Custom",
    }
}

/// The fan guard's lines from the config, logging triggers it ignores.
fn fan_guard_limits(config: &ConfigState) -> FanGuardLimits {
    let (limits, ignored) =
        FanGuardLimits::from_config(config.fan_guard_cpu_c, config.fan_guard_gpu_c);
    if let Some(ignored) = ignored {
        log::warn!("fan guard: {ignored}");
    }
    limits
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
                rules: &config.app_profiles,
                running_rule: None,
                guard_notes: &[],
                panel: None,
            },
        )?;
        let cpu_cap = CpuCap::open(config_file.path().with_file_name(cpu_cap::BASELINE_FILE));
        let limits = fan_guard_limits(&config);
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
            ec_temps: None,
            ec_temps_next: None,
            ec_temps_unsupported: false,
            ac_power,
            power_known: power.is_some(),
            adopt_not_before: None,
            unset: config.unset(),
            enforce: config.enforce,
            reassert_on_resume: config.reassert_on_resume,
            app_profiles: config.app_profiles,
            action: None,
            cpu_cap,
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
            weak_charger_guard: config.weak_charger_guard,
            charger: None,
            charger_next: None,
            charger_unsupported: false,
            charger_failure_logged: false,
            charger_pending_since: None,
            fan_guard_on: config.fan_guard,
            fan_guard_limits: limits,
            fan_guard_cpu_c: config.fan_guard_cpu_c,
            fan_guard_gpu_c: config.fan_guard_gpu_c,
            fan_guard: FanGuard::default(),
            fan_guard_next: None,
            panel: None,
            menu_base: device_state,
            guard_notes: Vec::new(),
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
            rules: &self.app_profiles,
            running_rule: self.action.map(|a| a.rule),
            guard_notes: &self.guard_notes,
            panel: self.panel,
        }
    }

    /// Rebuild the menu from the current intent, and show it if a tray icon is given.
    /// The one place the menu is rebuilt, so a new option can't be forgotten at one of
    /// several call sites.
    pub fn rebuild_menu(&mut self, tray_icon: Option<&tray_icon::TrayIcon>) {
        let base = self.menu_state();
        self.guard_notes = self.guard_lines();
        match menu::build(&base, &self.menu_options()) {
            Ok((m, h)) => {
                self.menu = m;
                self.event_handlers = h;
                self.menu_base = base;
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

    /// What the device should be in right now: [`Self::intent`] as the guards allow it.
    /// Every path that converges, enforces or reconciles aims at this (through
    /// `device_state`), so none of them "repairs" a held perf mode or a raised fan.
    pub fn target(&self) -> DeviceState {
        self.guarded(self.intent())
    }

    /// What the user asked for: the running Action's session state when one is active and
    /// allowed on this power source, else the saved profile. The menu shows this.
    fn intent(&self) -> DeviceState {
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

    /// `state` as the guards allow it now: the weak-charger clamp, then the fan guard.
    fn guarded(&self, state: DeviceState) -> DeviceState {
        let state = self.charger_guarded(state);
        if self.fan_guard.engaged() {
            guard::raise_fan_for_heat(state, self.fan_rpm_range)
        } else {
            state
        }
        .normalized(self.fan_rpm_range)
    }

    fn charger_guarded(&self, state: DeviceState) -> DeviceState {
        if self.charger_hold().is_some() {
            guard::clamp_for_weak_charger(state)
        } else {
            state
        }
    }

    /// What the menu shows: while the config is applied, the user's choice (so a held
    /// Performance stays checked, with a line saying it is held), even in the tick before
    /// the device is converged on a new target; otherwise the state the tray follows.
    fn menu_state(&self) -> DeviceState {
        if self.mode().writes_config() {
            self.intent()
        } else {
            self.device_state
        }
    }

    /// The disabled menu lines for the guards in force.
    fn guard_lines(&self) -> Vec<String> {
        let mut out = Vec::new();
        let intent = self.intent();
        if guard::weak_charger_perf(intent.perf_mode).is_some() {
            let perf = perf_name(intent.perf_mode);
            match self.charger_hold() {
                Some(ChargerHold::UsbC(source)) => {
                    let watts = source
                        .watts()
                        .map(|w| format!(" ({w} W)"))
                        .unwrap_or_default();
                    out.push(format!("USB-C charger{watts}: {perf} held to Balanced"));
                }
                Some(ChargerHold::Pending) => {
                    out.push(format!("Reading the charger: {perf} held to Balanced"));
                }
                None => {}
            }
        }
        if self.fan_guard.engaged() {
            out.push(format!(
                "Hot: fans at {} RPM until it cools",
                self.fan_rpm_range.1
            ));
        }
        out
    }

    // ---- weak-charger guard -------------------------------------------------------

    /// Why the perf mode is held to Balanced now, if it is: a USB-C charger
    /// ([`Self::weak_charger`]), or plugged in with the first charger read still awaited
    /// (no longer than [`CHARGER_PENDING_MAX`]).
    pub fn charger_hold(&self) -> Option<ChargerHold> {
        if let Some(source) = self.weak_charger() {
            return Some(ChargerHold::UsbC(source));
        }
        let pending = self.weak_charger_guard
            && self.ac_power
            && !self.charger_unsupported
            && self
                .charger_pending_since
                .is_some_and(|since| since.elapsed() < CHARGER_PENDING_MAX);
        pending.then_some(ChargerHold::Pending)
    }

    /// Plugged in at start: hold the perf mode until the charger is read. Before the
    /// first sync, so nothing above Balanced is written while the read is outstanding.
    pub fn expect_charger_read(&mut self) {
        if self.weak_charger_guard && self.ac_power && !self.charger_unsupported {
            self.charger_pending_since = Some(std::time::Instant::now());
        }
    }

    /// Once a tick: end a pending hold that ran out (logged once), and say whether the
    /// guard lines in the menu are out of date (a USB-C reading that expired, a hold that
    /// ended) so the caller rebuilds it.
    pub fn tick_guards(&mut self) -> bool {
        if let Some(since) = self.charger_pending_since {
            if since.elapsed() >= CHARGER_PENDING_MAX {
                self.charger_pending_since = None;
                if self.ac_power && self.weak_charger_guard {
                    log::warn!(
                        "charger: no answer within {CHARGER_PENDING_MAX:?}; not holding the perf \
                         mode (reads go on every {CHARGER_EVERY:?})"
                    );
                }
            }
        }
        self.guard_lines() != self.guard_notes
    }

    /// The USB-C charger the clamp is holding the perf mode for, if it is: the guard on,
    /// plugged in, and the EC's last reading (no older than [`CHARGER_FRESH`]) a USB-C one.
    /// A model that can't report its charger, or a read that keeps failing, is never
    /// clamped: the tray then behaves as it did before the guard.
    pub fn weak_charger(&self) -> Option<PowerSource> {
        if !self.weak_charger_guard || !self.ac_power {
            return None;
        }
        self.charger
            .filter(|(_, at)| at.elapsed() <= CHARGER_FRESH)
            .map(|(source, _)| source)
            .filter(|source| guard::is_weak_charger(*source))
    }

    /// Whether the charger should be read now: guard on, plugged in, the model has the
    /// command, and the last read was [`CHARGER_EVERY`] ago (or a power change asked).
    pub fn charger_read_due(&self, now: std::time::Instant) -> bool {
        self.weak_charger_guard
            && self.ac_power
            && !self.charger_unsupported
            && self.charger_next.is_none_or(|next| now >= next)
    }

    /// Read the charger from the EC and take the reading in; rebuild the menu when what
    /// the guard holds changed. The convergence step applies the new target.
    pub fn read_charger(
        &mut self,
        device: &device::Device,
        tray_icon: Option<&tray_icon::TrayIcon>,
    ) {
        let read = command::get_power_source(device);
        if self.note_charger(read, std::time::Instant::now()) {
            self.rebuild_menu(tray_icon);
        }
    }

    /// Take in a charger read. Returns whether the clamp changed. Logs a new charger, the
    /// first failure since a good read, and a model without the command (once).
    pub fn note_charger(&mut self, read: Result<PowerSource>, now: std::time::Instant) -> bool {
        let before = self.charger_hold();
        self.charger_next = Some(now + CHARGER_EVERY);
        match read {
            Ok(source) => {
                if self.charger.map(|(s, _)| s) != Some(source) {
                    log::info!(
                        "charger (EC): {source}{}",
                        if self.weak_charger_guard && guard::is_weak_charger(source) {
                            "; holding Performance and up to Balanced while on it"
                        } else {
                            ""
                        }
                    );
                }
                self.charger = Some((source, now));
                self.charger_failure_logged = false;
                self.charger_pending_since = None;
            }
            Err(e) if librazer::error::is_not_supported(&e) => {
                log::info!(
                    "charger: this model doesn't report it (0x078c not supported); no \
                     weak-charger guard"
                );
                self.charger = None;
                self.charger_unsupported = true;
                self.charger_pending_since = None;
            }
            Err(e) => {
                // While the first read is awaited the hold stands, so try again soon.
                if self.charger_pending_since.is_some() {
                    self.charger_next = Some(now + CHARGER_PENDING_RETRY);
                }
                if !self.charger_failure_logged {
                    log::warn!(
                        "charger read failed, keeping the last reading for up to \
                         {CHARGER_FRESH:?}: {e:#}"
                    );
                    self.charger_failure_logged = true;
                }
            }
        }
        before != self.charger_hold()
    }

    /// The power source changed: the old charger reading no longer says anything, and the
    /// next read waits until `read_at`, after the EC has settled.
    pub fn charger_power_changed(&mut self, read_at: std::time::Instant) {
        self.charger = None;
        self.charger_next = Some(read_at);
        // Plugged in: held to Balanced until that read answers. Unplugged: nothing to hold.
        self.charger_pending_since = None;
        self.expect_charger_read();
    }

    // ---- Mini-LED panel ------------------------------------------------------------

    /// Read the panel's firmware selection at startup, on a laptop that has the panel. Reads
    /// only: the tray never writes the panel mode by itself.
    pub fn detect_panel(&mut self, device: &device::Device) {
        match librazer::panel::get_panel_mode(device, self.pid) {
            Ok(Some(mode)) => {
                // The mode the panel runs in now, from what Windows offers; the firmware's
                // selection may be waiting for a restart (set before this tray started).
                let modes = crate::platform::internal_panel_modes();
                let running = modes.as_deref().and_then(panel_running);
                match (&modes, running) {
                    (_, Some(now)) => log::info!(
                        "Mini-LED panel: the firmware has {mode} selected; the panel runs {now}"
                    ),
                    (None, None) => log::info!(
                        "Mini-LED panel: the firmware has {mode} selected; no internal display \
                         is active (lid closed?), so taking that as what it runs"
                    ),
                    (Some(_), None) => log::info!(
                        "Mini-LED panel: the firmware has {mode} selected; the internal panel's \
                         modes match neither, so taking that as what it runs"
                    ),
                }
                self.panel = Some(PanelState {
                    at_start: running.unwrap_or(mode),
                    selected: mode,
                });
            }
            Ok(None) => self.panel = None,
            Err(e) => log::warn!("Mini-LED panel: could not read the selection ({e:#}); no menu"),
        }
    }

    /// The user picked a panel mode in the menu: select it in the firmware, confirmed by a
    /// read-back. `Ok(true)` when it changed (the caller offers a restart), `Ok(false)`
    /// when it was already selected. On an error the menu shows what the firmware reads.
    pub fn select_panel_mode(
        &mut self,
        device: &device::Device,
        mode: PanelMode,
        tray_icon: &tray_icon::TrayIcon,
    ) -> Result<bool> {
        let Some(panel) = self.panel else {
            anyhow::bail!("this laptop has no Mini-LED dual-mode panel");
        };
        let result = librazer::panel::set_panel_mode(device, self.pid, mode);
        match &result {
            Ok(librazer::panel::PanelWrite::Changed) => {
                log::info!(
                    "Mini-LED panel: {mode} selected and read back; takes effect after a restart"
                );
                self.panel = Some(PanelState {
                    selected: mode,
                    ..panel
                });
            }
            Ok(librazer::panel::PanelWrite::AlreadySelected) => {
                // Set elsewhere (the CLI) since the tray read it.
                log::info!("Mini-LED panel: {mode} was already selected; nothing written");
                self.panel = Some(PanelState {
                    selected: mode,
                    ..panel
                });
            }
            Err(e) => {
                log::warn!("Mini-LED panel: selecting {mode} failed: {e:#}");
                if let Ok(Some(read)) = librazer::panel::get_panel_mode(device, self.pid) {
                    self.panel = Some(PanelState {
                        selected: read,
                        ..panel
                    });
                }
            }
        }
        // The clicked item toggled itself; the rebuilt menu shows the firmware's selection.
        self.rebuild_menu(Some(tray_icon));
        result.map(|w| w == librazer::panel::PanelWrite::Changed)
    }

    // ---- fan guard ----------------------------------------------------------------

    /// Whether the fan guard watches now: on, the config applied, and the intent (as the
    /// charger allows it) holding a manual set point it could raise.
    fn fan_guard_watches(&self) -> bool {
        self.fan_guard_on
            && self.mode().writes_config()
            && guard::fan_guard_applies(&self.charger_guarded(self.intent()), self.fan_rpm_range)
    }

    /// Whether the fan guard wants a fresh EC temperature read now.
    pub fn fan_guard_read_due(&self, now: std::time::Instant) -> bool {
        self.fan_guard_watches()
            && !self.ec_temps_unsupported
            && self.fan_guard_next.is_none_or(|next| now >= next)
    }

    /// Read the EC temperatures for the fan guard (they also feed the tooltip).
    pub fn read_fan_guard_temps(&mut self, device: &device::Device) {
        let now = std::time::Instant::now();
        self.fan_guard_next = Some(now + FAN_GUARD_EVERY);
        self.refresh_ec_temps(device, now);
    }

    /// The GPU's observation for the fan guard, and whether the dGPU is known asleep.
    ///
    /// - Asleep (Windows' PnP power state says D3): no reading, and that counts as cool.
    /// - Awake: the higher of Windows' reading (D3DKMT or `nvidia-smi`) and the EC's GPU
    ///   byte, which runs 1-2 °C under the driver on `0x029F`, each with the time it was
    ///   taken. When Windows has none, the EC byte alone.
    /// - Unknown (no NVIDIA dGPU found, or not yet polled): Windows' reading only. The EC's
    ///   byte means nothing on a machine without a dGPU awake to report.
    fn gpu_observation(
        now: std::time::Instant,
        asleep: Option<bool>,
        driver: Option<(u32, std::time::Instant)>,
        ec: Option<(u8, std::time::Instant)>,
    ) -> (Option<Observation>, bool) {
        // Stale ones out first: an old hot EC byte must not hide a fresh driver reading.
        let fresh =
            |at: std::time::Instant| now.saturating_duration_since(at) <= guard::FAN_GUARD_STALE;
        let driver = driver
            .filter(|&(_, at)| fresh(at))
            .map(|(c, at)| Observation {
                c: u8::try_from(c).unwrap_or(u8::MAX),
                at,
            });
        let ec = ec
            .filter(|&(_, at)| fresh(at))
            .map(|(c, at)| Observation { c, at });
        match asleep {
            Some(true) => (None, true),
            Some(false) => match (driver, ec) {
                (Some(d), Some(e)) => (Some(if e.c > d.c { e } else { d }), false),
                (d, e) => (d.or(e), false),
            },
            None => (driver, false),
        }
    }

    /// Feed the fan guard what the sensors have. Returns whether it engaged or released
    /// (the convergence step then writes the raised fan or the set point back). Logs every
    /// change, and a lost sensor once.
    pub fn update_fan_guard(&mut self, now: std::time::Instant) -> bool {
        if !self.fan_guard_watches() {
            if self.fan_guard.reset() {
                log::info!("fan guard: released (no manual set point to raise now)");
                return true;
            }
            return false;
        }
        // Each sensor with the time it was read: the guard counts an observation once,
        // however often the loop looks at it, and ignores one older than FAN_GUARD_STALE.
        let cpu = self
            .ec_temps
            .and_then(|(t, at)| t.cpu_c.map(|c| Observation { c, at }));
        let ec_gpu = self.ec_temps.and_then(|(t, at)| t.gpu_c.map(|c| (c, at)));
        let (gpu, gpu_asleep) = Self::gpu_observation(
            now,
            crate::platform::dgpu_asleep(),
            crate::platform::gpu_temperature_sample(),
            ec_gpu,
        );
        let reading = GuardReading {
            cpu,
            gpu,
            gpu_asleep,
        };
        let limits = self.fan_guard_limits;
        match self.fan_guard.update(now, reading, limits) {
            Some(FanGuardChange::Engaged { cpu_c, gpu_c }) => {
                log::info!(
                    "fan guard: hot (CPU {cpu_c:?} °C, GPU {gpu_c:?} °C; triggers {}/{} °C): \
                     fans to {} RPM until the hot sensor is under {}/{} °C for {:?}",
                    limits.cpu_hot_c,
                    limits.gpu_hot_c,
                    self.fan_rpm_range.1,
                    limits.cpu_cool_c,
                    limits.gpu_cool_c,
                    guard::FAN_GUARD_COOL_HOLD
                );
                true
            }
            Some(FanGuardChange::Released { cpu_c, gpu_c }) => {
                log::info!(
                    "fan guard: cooled (CPU {cpu_c:?} °C, GPU {gpu_c:?} °C); the set point is back"
                );
                true
            }
            Some(FanGuardChange::Blind) => {
                log::warn!(
                    "fan guard: no fresh temperature; keeping the fans at {} RPM, for up to {:?} \
                     without one",
                    self.fan_rpm_range.1,
                    guard::FAN_GUARD_BLIND_RELEASE
                );
                false
            }
            Some(FanGuardChange::ReleasedBlind) => {
                log::warn!(
                    "fan guard: no temperature for {:?}; the set point is back (a hot reading \
                     raises the fans again)",
                    guard::FAN_GUARD_BLIND_RELEASE
                );
                true
            }
            None => false,
        }
    }

    /// Bring the Windows CPU frequency settings in line with the running Action. Runs
    /// after each process scan, so at startup a baseline left by a crash is put back only
    /// once the scan has shown that no rule wants the cap. Independent of the device: its
    /// failures are logged and never reach the rest of the rule.
    pub fn sync_cpu_cap(&mut self) {
        let rule = self.action.and_then(|a| self.app_profiles.get(a.rule));
        let wanted = cpu_cap::wanted_cap(rule, self.ac_power);
        self.cpu_cap
            .reconcile(wanted, &crate::platform::PowerSettings);
    }

    /// The tray is exiting: nothing would put the settings back after this.
    pub fn release_cpu_cap(&mut self) {
        self.cpu_cap
            .reconcile(None, &crate::platform::PowerSettings);
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
        // Independent of the fan read and its pause: its own failures pause it.
        self.refresh_ec_temps(device, now);
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

    /// Read the EC's temperatures for the tooltip, at most every [`EC_TEMP_EVERY`]. One
    /// HID round trip (about 4 ms on `0x029F`). Bus trouble pauses the reads for
    /// [`EC_TEMP_PAUSE`] and keeps the last reading until it goes stale. A model without
    /// the command (0x05), or one answering in a layout we don't know, is not asked again.
    fn refresh_ec_temps(&mut self, device: &device::Device, now: std::time::Instant) {
        if self.ec_temps_unsupported || self.ec_temps_next.is_some_and(|next| now < next) {
            return;
        }
        let read = librazer::command::get_ec_temperatures(device);
        // From when the read finished, so a slow failure can't leave the next one due.
        let done = std::time::Instant::now();
        match read {
            Ok(t) => {
                if self.ec_temps.is_none() {
                    log::info!("EC temperatures: first read {t:?}");
                }
                self.ec_temps = Some((t, done));
                self.ec_temps_next = Some(done + EC_TEMP_EVERY);
            }
            Err(e) if librazer::error::is_retryable(&e) => {
                log::warn!("EC temperature read failed, pausing them for {EC_TEMP_PAUSE:?}: {e:#}");
                self.ec_temps_next = Some(done + EC_TEMP_PAUSE);
            }
            Err(e) => {
                log::info!(
                    "EC temperatures: not available on this model ({e:#}); not asking again"
                );
                self.ec_temps = None;
                self.ec_temps_unsupported = true;
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
            weak_charger_guard: self.weak_charger_guard,
            fan_guard: self.fan_guard_on,
            fan_guard_cpu_c: self.fan_guard_cpu_c,
            fan_guard_gpu_c: self.fan_guard_gpu_c,
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
        let limits = fan_guard_limits(&disk);
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
        self.weak_charger_guard = disk.weak_charger_guard;
        self.fan_guard_on = disk.fan_guard;
        self.fan_guard_limits = limits;
        self.fan_guard_cpu_c = disk.fan_guard_cpu_c;
        self.fan_guard_gpu_c = disk.fan_guard_gpu_c;
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

    /// A "Game rules" check mark: turn that rule on or off and save it. The next process
    /// scan starts or ends the rule. A check mark from a menu built before the rules changed
    /// is ignored, and the menu is rebuilt either way.
    pub fn toggle_rule(&mut self, id: &str, tray_icon: &tray_icon::TrayIcon) {
        match crate::rules::toggle(&mut self.app_profiles, id) {
            Some((label, enabled)) => {
                log::info!(
                    "game rule '{label}' {}",
                    if enabled { "enabled" } else { "disabled" }
                );
                if let Err(e) = self.persist() {
                    log::warn!("could not save the game rule change: {e:?}");
                }
            }
            None => log::info!("menu item {id} is from before the rules changed; ignored"),
        }
        self.rebuild_menu(Some(tray_icon));
    }

    /// A new rule list from the rules window: save it like any menu change. A running rule
    /// that changed or moved ends here; the next scan starts whatever matches now.
    pub fn replace_rules(&mut self, rules: Vec<AppProfile>, tray_icon: &tray_icon::TrayIcon) {
        if let Some(session) = &self.action {
            if rules.get(session.rule) != self.app_profiles.get(session.rule) {
                self.action = None;
            }
        }
        self.app_profiles = rules;
        if let Err(e) = self.persist() {
            log::warn!("could not save the rules from the rules window: {e:?}");
        }
        self.rebuild_menu(Some(tray_icon));
    }

    /// "Edit rules...": write the guide beside the config and open both in Notepad, the
    /// config last so it is in front. The tray picks up a saved edit by itself.
    pub fn open_rules_editor(&mut self) {
        if !self.config_file.exists() {
            if let Err(e) = self.persist() {
                log::warn!("could not create the config to edit: {e:?}");
            }
        }
        let config = self.config_file.path().to_path_buf();
        let Some(dir) = config.parent() else {
            log::warn!("config path {} has no folder", config.display());
            return;
        };
        let mut files = Vec::new();
        match crate::rules::write_help(dir, &config, &crate::get_logging_file_path()) {
            Ok(help) => files.push(help),
            Err(e) => log::warn!("could not write the game rules guide: {e:?}"),
        }
        files.push(config);
        for file in &files {
            if let Err(e) = crate::platform::open_in_editor(file) {
                log::warn!("could not open {}: {e:?}", file.display());
            }
        }
    }

    pub fn handle_event(&self, event_id: &str) -> Option<DeviceState> {
        self.event_handlers.get(event_id).copied()
    }

    /// The next perf mode after the one the menu shows (the user's choice, not a mode a
    /// guard holds the device at). `normalized` in `update` clears max fan when the cycle
    /// leaves Custom, which the plain struct update did not.
    pub fn get_next_perf_mode(&self) -> DeviceState {
        DeviceState {
            perf_mode: crate::state::next_perf_mode(self.menu_base.perf_mode),
            ..self.menu_base
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
        const P_CPU_TEMP: u8 = 4;
        const P_CHARGE: u8 = 5;
        const P_BRIGHTNESS: u8 = 6;
        const P_GPU_WATTS: u8 = 7;
        const P_LOGO: u8 = 8;

        // A mode the weak-charger guard holds is marked, so a Performance profile showing
        // Balanced doesn't read as the tray failing. Only once the device is there.
        let held = s.perf_mode == PerfMode::Balanced
            && self.weak_charger().is_some()
            && guard::weak_charger_perf(self.intent().perf_mode).is_some();
        let mode = match s.perf_mode {
            PerfMode::Balanced if held => "Balanced (USB-C)".to_string(),
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
                    // The fan guard raised it for the heat.
                    None if self.fan_guard.engaged() && rpm == self.fan_rpm_range.1 => {
                        format!("Fan {rpm} hot")
                    }
                    None => format!("Fan {rpm} set"),
                }
            }
        };
        let fan = if s.max_fan && !self.unsupported.contains(Optional::MaxFan) {
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

        // Temperatures, when a reading is available. Omitted entirely otherwise -- a
        // machine with no dGPU must not see "0°C", which reads as a measurement. The CPU
        // figure is the EC's CPU-side sensor (`0x0d85`), not the die.
        let mut gpu_line = Vec::new();
        let ec_fresh = self
            .ec_temps
            .filter(|(_, at)| at.elapsed() < EC_TEMP_STALE)
            .map(|(t, _)| t);
        if let Some(cpu_c) = ec_fresh.and_then(|t| t.cpu_c) {
            gpu_line.push((P_CPU_TEMP, format!("CPU {cpu_c}°C")));
        }
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
        // What the menu was built from: its difference from `picked` is exactly what the
        // user changed. See `DeviceState::carry_changes`. Taken before the reload below,
        // which rebuilds the menu. While a guard holds the device, the menu shows the
        // user's choice, not the held state, so a pick carries nothing a guard put there.
        let before = self.menu_base;
        // Adopt a hand edit first, so the pick lands on top of it instead of the save
        // that follows either overwriting the edit or discarding the pick.
        self.reload_config_if_changed(tray_icon);
        let picked = picked.normalized(self.fan_rpm_range);
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
        // The pick as the guards allow it: a Performance picked on a USB-C charger is saved
        // as Performance and runs as Balanced until the charger changes. While the config is
        // applied that is the target recomputed from the saved profile or session, which
        // also holds whatever a hand edit adopted above changed besides the pick.
        self.device_state = if mode.writes_config() {
            self.target()
        } else {
            self.guarded(picked)
        };
        if let Err(e) = self.persist() {
            log::warn!("could not save the config: {e:?}");
        }
        log::info!("state updated to {:?} ({pick:?})", picked);
        if self.device_state != picked {
            log::info!("applying {:?} (guards, hand edit)", self.device_state);
        }
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
            // Name every enforced field that moved, not only perf and fan: a charge-limit
            // drift alone used to read as "Balanced/Auto != intended Balanced/Auto".
            let fields = self
                .observed
                .enforced_drift(&self.device_state, self.unsupported)
                .join(", ");
            log::warn!(
                "{reason}: device {:?}/{:?} != intended {:?}/{:?} [{fields}]{}",
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
                let reasserted = self.device_state.enforce_to(device);
                // The write can be what finds a setting missing (Max Fan, whose getter may
                // answer when the setter doesn't), and the read-back below goes by it.
                self.track_unsupported(device, Some(tray_icon));
                match reasserted {
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
        // Widest two-digit reading, so the budget test covers it.
        state.ec_temps = Some((
            librazer::types::EcTemperatures {
                cpu_c: Some(99),
                gpu_c: Some(99),
            },
            std::time::Instant::now(),
        ));
        state
    }

    /// The EC's CPU-side temperature shows when it was read and the byte was believable.
    #[test]
    fn the_tooltip_shows_the_ec_cpu_temperature_only_when_read() {
        let mut state = test_state(DeviceState::default());
        assert!(state.tooltip().unwrap().contains("CPU 99°C"));
        let now = std::time::Instant::now();
        let t = |cpu_c| librazer::types::EcTemperatures {
            cpu_c,
            gpu_c: Some(48),
        };
        state.ec_temps = Some((t(None), now));
        assert!(!state.tooltip().unwrap().contains("°C"));
        state.ec_temps = None;
        assert!(!state.tooltip().unwrap().contains("°C"));
        // A reading older than EC_TEMP_STALE is left out.
        if let Some(old) = now.checked_sub(EC_TEMP_STALE + std::time::Duration::from_secs(1)) {
            state.ec_temps = Some((t(Some(60)), old));
            assert!(!state.tooltip().unwrap().contains("°C"));
        }
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
                librazer::types::CpuBoost::Overclock,
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

    // ---- guards: a weak charger, the fan guard ------------------------------------

    /// An applied config (a valid file in its own scratch directory) with this AC profile,
    /// plugged in, and the device converged on the target.
    fn guarded_state(name: &str, ac_state: DeviceState) -> ProgramState {
        let path = scratch_config(name);
        ConfigFile::at(path.clone())
            .store(&ConfigState {
                ac_state,
                ..ConfigState::default()
            })
            .unwrap();
        let mut file = ConfigFile::at(path);
        let config = file.load();
        let mut state = ProgramState::new(config, file, Some(true), RANGE).unwrap();
        assert_eq!(state.mode(), Mode::Apply);
        state.device_state = state.target();
        state
    }

    fn usb_c_65w() -> Result<PowerSource> {
        Ok(PowerSource::from_args(&[0x04, 0x12]))
    }

    fn barrel() -> Result<PowerSource> {
        Ok(PowerSource::from_args(&[0x12, 0x12]))
    }

    fn not_supported() -> Result<PowerSource> {
        Err(anyhow::Error::new(
            librazer::packet::ResponseError::NotSupported,
        ))
    }

    fn with_perf(perf_mode: PerfMode) -> DeviceState {
        DeviceState {
            perf_mode,
            ..DeviceState::default()
        }
    }

    #[test]
    fn a_usb_c_charger_holds_performance_to_balanced_and_the_profile_keeps_it() {
        let now = std::time::Instant::now();
        let mut state = guarded_state("usb-c", with_perf(PerfMode::Performance));
        assert!(state.charger_read_due(now), "read at startup");
        assert!(
            state.note_charger(usb_c_65w(), now),
            "the clamp is a change"
        );
        assert!(!state.charger_read_due(now), "next read in 30 s");
        assert!(state.charger_read_due(now + CHARGER_EVERY));
        assert_eq!(state.target().perf_mode, PerfMode::Balanced);
        assert_eq!(
            state.ac_state.perf_mode,
            PerfMode::Performance,
            "saved as picked"
        );
        assert_eq!(state.intent().perf_mode, PerfMode::Performance);
        assert_eq!(
            state.guard_lines(),
            vec!["USB-C charger (65 W): Performance held to Balanced".to_string()]
        );
        // The menu shows the choice once the device is aimed at the held state, and the
        // left-click cycle goes on from it.
        state.device_state = state.target();
        state.rebuild_menu(None);
        assert_eq!(state.menu_base.perf_mode, PerfMode::Performance);
        assert_eq!(state.get_next_perf_mode().perf_mode, PerfMode::Hyperboost);
        // The same reading again changes nothing; the barrel charger lifts it.
        assert!(!state.note_charger(usb_c_65w(), now));
        assert!(state.note_charger(barrel(), now));
        assert_eq!(state.target().perf_mode, PerfMode::Performance);
        assert!(state.guard_lines().is_empty());
    }

    #[test]
    fn every_mode_above_balanced_and_custom_is_held_and_the_rest_are_not() {
        use librazer::types::{CpuBoost, GpuBoost};
        let now = std::time::Instant::now();
        let custom = DeviceState {
            perf_mode: PerfMode::Custom(CpuBoost::Boost, GpuBoost::High),
            max_fan: true,
            ..DeviceState::default()
        };
        let mut state = guarded_state("usb-c-modes", custom);
        state.note_charger(usb_c_65w(), now);
        let held = state.target();
        assert_eq!(held.perf_mode, PerfMode::Balanced);
        assert!(!held.max_fan, "Max Fan is Custom-only");
        for perf in [PerfMode::Hyperboost, PerfMode::Performance] {
            state.ac_state = with_perf(perf);
            assert_eq!(state.target().perf_mode, PerfMode::Balanced, "{perf:?}");
        }
        for perf in [PerfMode::Battery, PerfMode::Silent, PerfMode::Balanced] {
            state.ac_state = with_perf(perf);
            assert_eq!(state.target().perf_mode, perf, "{perf:?}");
            assert!(
                state.guard_lines().is_empty(),
                "{perf:?}: nothing held, no line"
            );
        }
    }

    #[test]
    fn the_clamp_needs_the_guard_on_and_the_laptop_plugged_in() {
        let now = std::time::Instant::now();
        let mut state = guarded_state("usb-c-off", with_perf(PerfMode::Performance));
        state.note_charger(usb_c_65w(), now);
        state.weak_charger_guard = false;
        assert_eq!(state.target().perf_mode, PerfMode::Performance);
        assert!(
            !state.charger_read_due(now + CHARGER_EVERY),
            "off: no reads"
        );
        state.weak_charger_guard = true;
        state.ac_power = false;
        assert_eq!(state.weak_charger(), None);
        assert!(
            !state.charger_read_due(now + CHARGER_EVERY),
            "battery: no reads"
        );
        // Plugged in again: no clamp until the new source is read after it settles.
        state.ac_power = true;
        state.charger_power_changed(now + std::time::Duration::from_secs(5));
        assert_eq!(state.weak_charger(), None);
        assert!(!state.charger_read_due(now));
        assert!(state.charger_read_due(now + std::time::Duration::from_secs(5)));
    }

    #[test]
    fn a_failed_or_unsupported_charger_read_never_clamps() {
        let now = std::time::Instant::now();
        // A model without 0x078c: no clamp, and never asked again.
        let mut state = guarded_state("charger-unsupported", with_perf(PerfMode::Performance));
        assert!(!state.note_charger(not_supported(), now));
        assert_eq!(state.target().perf_mode, PerfMode::Performance);
        assert!(!state.charger_read_due(now + CHARGER_EVERY * 10));
        // Bus trouble before any reading: no clamp, and read again later.
        let mut state = guarded_state("charger-busy", with_perf(PerfMode::Performance));
        let busy = || Err(anyhow::Error::new(librazer::packet::ResponseError::Busy));
        assert!(!state.note_charger(busy(), now));
        assert_eq!(state.target().perf_mode, PerfMode::Performance);
        assert!(state.charger_read_due(now + CHARGER_EVERY));
        // After a USB-C reading, one failed read keeps the clamp: lifting it would write
        // Performance only for the next good read to write Balanced again.
        state.note_charger(usb_c_65w(), now);
        assert!(!state.note_charger(busy(), now + CHARGER_EVERY));
        assert_eq!(state.target().perf_mode, PerfMode::Balanced);
    }

    #[test]
    fn an_app_rule_is_held_too_and_keeps_its_own_mode() {
        let now = std::time::Instant::now();
        let mut state = guarded_state("usb-c-rule", with_perf(PerfMode::Silent));
        let rule = AppProfile {
            process: "game.exe".into(),
            perf_mode: Some(PerfMode::Hyperboost),
            ..AppProfile::default()
        };
        state.app_profiles = vec![rule.clone()];
        state.action = Some(ActionSession::start(
            0,
            &rule,
            &state.saved_profile(),
            RANGE,
        ));
        assert_eq!(state.target().perf_mode, PerfMode::Hyperboost);
        state.note_charger(usb_c_65w(), now);
        assert_eq!(state.target().perf_mode, PerfMode::Balanced);
        assert_eq!(
            state.action.unwrap().effective.perf_mode,
            PerfMode::Hyperboost,
            "the session is not rewritten"
        );
        assert_eq!(
            state.guard_lines(),
            vec!["USB-C charger (65 W): Hyperboost held to Balanced".to_string()]
        );
    }

    #[test]
    fn plugged_in_with_the_charger_unread_holds_balanced_until_it_answers() {
        let now = std::time::Instant::now();
        let mut state = guarded_state("charger-pending", with_perf(PerfMode::Performance));
        // Startup on AC (main calls this before the first sync), and the read succeeds.
        state.expect_charger_read();
        assert_eq!(
            state.target().perf_mode,
            PerfMode::Balanced,
            "nothing above Balanced"
        );
        assert_eq!(
            state.guard_lines(),
            vec!["Reading the charger: Performance held to Balanced".to_string()]
        );
        assert!(
            state.note_charger(barrel(), now),
            "the barrel charger lifts the hold"
        );
        assert_eq!(state.target().perf_mode, PerfMode::Performance);

        // A plug: held until the read 5 s later. A USB-C answer keeps it, now as USB-C.
        state.ac_power = false;
        state.charger_power_changed(now);
        assert_eq!(state.charger_hold(), None, "on battery nothing is held");
        state.ac_power = true;
        state.charger_power_changed(now + std::time::Duration::from_secs(5));
        assert_eq!(state.charger_hold(), Some(ChargerHold::Pending));
        assert_eq!(state.target().perf_mode, PerfMode::Balanced);
        assert!(state.note_charger(usb_c_65w(), now), "the line changes");
        assert!(matches!(state.charger_hold(), Some(ChargerHold::UsbC(_))));
        assert_eq!(state.target().perf_mode, PerfMode::Balanced);
    }

    #[test]
    fn a_model_without_the_read_is_never_held_and_a_failing_one_only_for_a_while() {
        let now = std::time::Instant::now();
        // No 0x078c: the first read answers "not supported" and the hold ends at once.
        let mut state = guarded_state("pending-unsupported", with_perf(PerfMode::Performance));
        state.expect_charger_read();
        assert!(state.note_charger(not_supported(), now));
        assert_eq!(state.target().perf_mode, PerfMode::Performance);
        state.charger_power_changed(now);
        assert_eq!(state.charger_hold(), None, "never held again");

        // Reads that fail: held, retried after 5 s, and let go after CHARGER_PENDING_MAX.
        let mut state = guarded_state("pending-busy", with_perf(PerfMode::Performance));
        state.expect_charger_read();
        let busy = || Err(anyhow::Error::new(librazer::packet::ResponseError::Busy));
        assert!(!state.note_charger(busy(), now));
        assert_eq!(state.target().perf_mode, PerfMode::Balanced);
        assert!(!state.charger_read_due(now + std::time::Duration::from_secs(4)));
        assert!(state.charger_read_due(now + CHARGER_PENDING_RETRY));
        state.rebuild_menu(None);
        assert!(!state.tick_guards(), "the menu already says so");
        let Some(long_ago) = std::time::Instant::now()
            .checked_sub(CHARGER_PENDING_MAX + std::time::Duration::from_secs(1))
        else {
            return;
        };
        state.charger_pending_since = Some(long_ago);
        assert!(state.tick_guards(), "the line must go");
        assert_eq!(state.target().perf_mode, PerfMode::Performance);
        assert_eq!(state.charger_pending_since, None);
    }

    #[test]
    fn an_expired_usb_c_reading_clears_the_menu_line() {
        let now = std::time::Instant::now();
        let mut state = guarded_state("usb-c-expiry", with_perf(PerfMode::Performance));
        state.note_charger(usb_c_65w(), now);
        state.rebuild_menu(None);
        assert!(!state.tick_guards());
        assert_eq!(state.target().perf_mode, PerfMode::Balanced);
        // The reads failed for longer than CHARGER_FRESH: the reading lapses.
        let Some(old) = now.checked_sub(CHARGER_FRESH + std::time::Duration::from_secs(1)) else {
            return;
        };
        state.charger = state.charger.map(|(source, _)| (source, old));
        assert_eq!(state.target().perf_mode, PerfMode::Performance);
        assert!(state.tick_guards(), "the USB-C line is stale");
        state.rebuild_menu(None);
        assert!(!state.tick_guards());
        assert!(state.guard_lines().is_empty());
    }

    #[test]
    fn the_gpu_observation_follows_the_power_state() {
        let t = std::time::Instant::now();
        let ec = Some((60u8, t));
        let driver = Some((62u32, t));
        // Asleep: nothing to read, and cool.
        assert_eq!(
            ProgramState::gpu_observation(t, Some(true), driver, ec),
            (None, true)
        );
        // Awake: the higher of the two; the EC byte alone when Windows has none.
        let (o, asleep) = ProgramState::gpu_observation(t, Some(false), driver, Some((70, t)));
        assert_eq!((o.map(|o| o.c), asleep), (Some(70), false));
        let (o, _) = ProgramState::gpu_observation(t, Some(false), driver, ec);
        assert_eq!(o.map(|o| o.c), Some(62));
        let (o, _) = ProgramState::gpu_observation(t, Some(false), None, ec);
        assert_eq!(o.map(|o| o.c), Some(60));
        // Unknown (no NVIDIA dGPU found): Windows only, never the EC byte.
        assert_eq!(
            ProgramState::gpu_observation(t, None, None, ec),
            (None, false)
        );
        let (o, _) = ProgramState::gpu_observation(t, None, driver, ec);
        assert_eq!(o.map(|o| o.c), Some(62));
        // A stale EC byte, however hot, gives way to a fresh driver reading.
        let late = t + guard::FAN_GUARD_STALE + std::time::Duration::from_secs(1);
        let (o, _) =
            ProgramState::gpu_observation(late, Some(false), Some((88, late)), Some((90, t)));
        assert_eq!(o.map(|o| o.c), Some(88));
        let (o, _) = ProgramState::gpu_observation(late, Some(false), None, Some((90, t)));
        assert_eq!(o, None);
    }

    #[test]
    fn a_cached_ec_reading_looked_at_again_does_not_count_as_cooling() {
        let t0 = std::time::Instant::now();
        let secs = |n| t0 + std::time::Duration::from_secs(n);
        let mut state = guarded_state("fan-guard-reused", manual(3000));
        let hot = state.fan_guard_limits.cpu_hot_c;
        let cool = state.fan_guard_limits.cpu_cool_c - 1;
        state.ec_temps = Some(ec(hot, t0));
        assert!(state.update_fan_guard(t0));
        // One cool read at 5 s, then the reads stop (paused after a failure) while the
        // loop keeps looking every second: never 30 s of observations.
        state.ec_temps = Some(ec(cool, secs(5)));
        for n in 5..=60 {
            assert!(!state.update_fan_guard(secs(n)), "{n}");
        }
        assert!(state.fan_guard.engaged());
        assert_eq!(state.target().fan_speed, FanSpeed::Manual(RANGE.1));
    }

    #[test]
    fn the_running_panel_mode_comes_from_a_matching_mode() {
        use librazer::panel::PanelMode;
        let uhd = [
            (3840, 2400, 120),
            (3840, 2400, 60),
            (1920, 1200, 120),
            (1280, 800, 60),
        ];
        let fhd = [(1920, 1200, 240), (1920, 1200, 60), (1280, 800, 240)];
        assert_eq!(panel_running(&uhd), Some(PanelMode::Uhd120));
        assert_eq!(panel_running(&fhd), Some(PanelMode::Fhd240));
        // 1920x1200 only at 60 Hz plus some other mode at 240 Hz is not FHD+: the
        // resolution and the rate must come from one mode.
        assert_eq!(panel_running(&[(1920, 1200, 60), (1280, 720, 240)]), None);
        // A list that fits neither, or both, is no answer.
        assert_eq!(panel_running(&[(2560, 1600, 165)]), None);
        assert_eq!(panel_running(&[(3840, 2400, 120), (1920, 1200, 240)]), None);
        assert_eq!(panel_running(&[]), None);
        // A pending change survives a tray restart: UHD+ runs, FHD+ is selected.
        let state = PanelState {
            at_start: panel_running(&uhd).unwrap(),
            selected: PanelMode::Fhd240,
        };
        assert_eq!(
            crate::menu::panel_label(PanelMode::Fhd240, state),
            "1920x1200, 240 Hz (after restart)"
        );
    }

    fn ec(
        cpu_c: u8,
        at: std::time::Instant,
    ) -> (librazer::types::EcTemperatures, std::time::Instant) {
        (
            librazer::types::EcTemperatures {
                cpu_c: Some(cpu_c),
                gpu_c: None,
            },
            at,
        )
    }

    fn manual(rpm: u16) -> DeviceState {
        DeviceState {
            perf_mode: PerfMode::Balanced,
            fan_speed: FanSpeed::Manual(rpm),
            ..DeviceState::default()
        }
    }

    #[test]
    fn the_fan_guard_raises_a_manual_fan_while_hot_and_gives_it_back() {
        let t0 = std::time::Instant::now();
        let secs = |n| t0 + std::time::Duration::from_secs(n);
        let mut state = guarded_state("fan-guard", manual(3000));
        let hot = state.fan_guard_limits.cpu_hot_c;
        let cool = state.fan_guard_limits.cpu_cool_c - 1;
        assert!(state.fan_guard_read_due(t0));
        state.ec_temps = Some(ec(hot, t0));
        assert!(state.update_fan_guard(t0), "engaged");
        assert_eq!(state.target().fan_speed, FanSpeed::Manual(RANGE.1));
        assert_eq!(
            state.ac_state.fan_speed,
            FanSpeed::Manual(3000),
            "saved as picked"
        );
        assert_eq!(
            state.guard_lines(),
            vec![format!("Hot: fans at {} RPM until it cools", RANGE.1)]
        );
        // The device aimed at the raised fan: the menu still shows the set point, and the
        // tooltip says why the fans are up.
        state.device_state = state.target();
        state.observed = state.device_state;
        state.rebuild_menu(None);
        assert_eq!(state.menu_base.fan_speed, FanSpeed::Manual(3000));
        assert!(state
            .tooltip()
            .unwrap()
            .contains(&format!("Fan {} hot", RANGE.1)));
        // Held while it cools, then released after the hold.
        for n in [5, 10, 20, 30] {
            state.ec_temps = Some(ec(cool, secs(n)));
            assert!(!state.update_fan_guard(secs(n)), "{n}");
        }
        state.ec_temps = Some(ec(cool, secs(40)));
        assert!(state.update_fan_guard(secs(40)), "released");
        assert_eq!(state.target().fan_speed, FanSpeed::Manual(3000));
        assert!(state.guard_lines().is_empty());
    }

    #[test]
    fn the_fan_guard_leaves_auto_alone_and_ignores_stale_readings() {
        let t0 = std::time::Instant::now();
        let mut state = guarded_state("fan-guard-auto", DeviceState::default());
        let hot = state.fan_guard_limits.cpu_hot_c;
        assert_eq!(state.ac_state.fan_speed, FanSpeed::Auto);
        assert!(!state.fan_guard_read_due(t0), "nothing to watch in Auto");
        state.ec_temps = Some(ec(hot + 5, t0));
        assert!(!state.update_fan_guard(t0));
        assert_eq!(state.target().fan_speed, FanSpeed::Auto);

        // A hot reading that is too old engages nothing.
        state.ac_state = manual(3000);
        if let Some(old) =
            t0.checked_sub(guard::FAN_GUARD_STALE + std::time::Duration::from_secs(1))
        {
            state.ec_temps = Some(ec(hot + 5, old));
            assert!(!state.update_fan_guard(t0));
            assert_eq!(state.target().fan_speed, FanSpeed::Manual(3000));
        }
        // Engaged, then the user goes to Auto: released at once, Auto untouched.
        state.ec_temps = Some(ec(hot, t0));
        assert!(state.update_fan_guard(t0));
        state.ac_state = DeviceState::default();
        assert!(state.update_fan_guard(t0), "released");
        assert_eq!(state.target().fan_speed, FanSpeed::Auto);
        // Off in the config: never engages.
        state.ac_state = manual(3000);
        state.fan_guard_on = false;
        assert!(!state.fan_guard_read_due(t0));
        assert!(!state.update_fan_guard(t0));
        assert_eq!(state.target().fan_speed, FanSpeed::Manual(3000));
    }

    /// Both guards' markers fit the tooltip with every perf mode.
    #[test]
    fn the_guard_markers_fit_the_tooltip_budget() {
        use librazer::types::{CpuBoost, GpuBoost};
        use strum::IntoEnumIterator;
        let now = std::time::Instant::now();
        let mut modes = vec![
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
        for perf in modes {
            let mut state = guarded_state(
                "tooltip-guards",
                DeviceState {
                    perf_mode: perf,
                    ..manual(3000)
                },
            );
            state.note_charger(usb_c_65w(), now);
            state.ec_temps = Some(ec(state.fan_guard_limits.cpu_hot_c, now));
            state.update_fan_guard(now);
            state.fan_actual = Some(FanRpm {
                fan1: 5000,
                fan2: 5000,
            });
            state.device_state = state.target();
            for observed in [state.device_state, state.intent()] {
                state.observed = observed;
                let tip = state.tooltip().unwrap();
                let units = librazer::state::utf16_units(&tip);
                assert!(
                    units <= TOOLTIP_MAX_UTF16,
                    "{perf:?}: {units} units: {tip:?}"
                );
            }
            state.observed = state.device_state;
            let tip = state.tooltip().unwrap();
            let held = guard::weak_charger_perf(perf).is_some();
            assert_eq!(
                tip.starts_with("Balanced (USB-C)"),
                held,
                "{perf:?}: {tip:?}"
            );
            assert!(tip.contains("hot"), "{perf:?}: {tip:?}");
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

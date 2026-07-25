//! `ProgramState`: the live application state behind the tray -- current intent
//! (`device_state`), the last-read reality (`observed`), the AC/battery profiles,
//! and the current menu + its handler map. Owns rendering (icon/tooltip), the
//! perf-mode cycle, persistence, and applying a new state to the device.

use anyhow::Result;
use std::collections::HashMap;

use librazer::device;
use librazer::types::{BatteryCare, LightsAlwaysOn};
use tray_icon::menu::Menu;

use crate::menu;
use crate::state::{
    brightness_to_percent, get_fan_rpm, AppProfile, ConfigState, DeviceState, FanRpm, FanSpeed,
    PerfMode,
};

pub struct ProgramState {
    pub device_state: DeviceState,
    pub observed: DeviceState,
    pub ac_state: DeviceState,
    pub battery_state: DeviceState,
    pub event_handlers: HashMap<String, DeviceState>,
    pub menu: Menu,
    pub fan_actual: FanRpm,
    pub ac_power: bool,
    pub enforce: bool,
    /// Re-assert the intended profile on wake even when `enforce` is off (config-driven).
    pub reassert_on_resume: bool,
    /// "Actions" rules: while a listed process runs, force its perf mode (config-driven).
    pub app_profiles: Vec<AppProfile>,
    /// Usable manual-fan RPM bounds for this chassis (from `Descriptor::fan_rpm_range`),
    /// captured once at startup so menu rebuilds don't need the `Device` on hand.
    pub fan_rpm_range: (u16, u16),
}

impl ProgramState {
    pub fn new(
        device_state: DeviceState,
        fan_last: FanRpm,
        enforce: bool,
        reassert_on_resume: bool,
        app_profiles: Vec<AppProfile>,
        fan_rpm_range: (u16, u16),
    ) -> Result<Self> {
        let (menu, event_handlers) = menu::build(&device_state, enforce, fan_rpm_range)?;
        Ok(Self {
            device_state,
            observed: device_state,
            ac_state: device_state,
            battery_state: device_state,
            event_handlers,
            menu,
            fan_actual: fan_last,
            ac_power: true,
            enforce,
            reassert_on_resume,
            app_profiles,
            fan_rpm_range,
        })
    }

    /// Persist the current AC/battery profiles + enforce flag + Actions config. Single
    /// source of truth so a future ConfigState field can't be silently dropped by one of
    /// the call sites (there used to be three inline `confy::store` literals).
    pub fn persist(&self) -> Result<()> {
        confy::store(
            crate::PKG_NAME,
            None,
            ConfigState {
                ac_state: self.ac_state,
                battery_state: self.battery_state,
                enforce: self.enforce,
                reassert_on_resume: self.reassert_on_resume,
                app_profiles: self.app_profiles.clone(),
            },
        )?;
        Ok(())
    }

    pub fn handle_event(&self, event_id: &str) -> Result<DeviceState> {
        let next_state = self.event_handlers.get(event_id).ok_or(anyhow::anyhow!(
            "No event handler found for event_id: {}",
            event_id
        ))?;
        Ok(*next_state)
    }

    pub fn get_next_perf_mode(&self) -> DeviceState {
        DeviceState {
            perf_mode: crate::state::next_perf_mode(self.device_state.perf_mode),
            ..self.device_state
        }
    }

    pub fn tooltip(&self) -> Result<String> {
        use std::fmt::Write;

        // Render from `observed` (the device's last-read real state) so the tray
        // reflects reality, including changes made outside the tray (e.g. Fn keys).
        let s = &self.observed;

        // Keep this compact: the Windows tray tooltip (Shell_NotifyIcon szTip) is
        // capped at 128 UTF-16 units and silently truncates past that. We build a
        // terse, few-line string and hard-cap it below as a final guard.
        let mut info = String::new();

        match s.perf_mode {
            PerfMode::Battery => write!(&mut info, "Battery")?,
            PerfMode::Silent => write!(&mut info, "Silent")?,
            PerfMode::Balanced => write!(&mut info, "Balanced")?,
            PerfMode::Performance => write!(&mut info, "Performance")?,
            PerfMode::Hyperboost => write!(&mut info, "Hyperboost")?,
            PerfMode::Custom(cpu_boost, gpu_boost) => {
                write!(&mut info, "Custom (CPU {cpu_boost:?}, GPU {gpu_boost:?})")?
            }
        }

        match s.fan_speed {
            FanSpeed::Auto => write!(&mut info, "\nFan: Auto")?,
            FanSpeed::Manual(rpm) => write!(&mut info, "\nFan: {rpm} set")?,
        }
        if s.max_fan {
            write!(&mut info, " (max)")?;
        }
        write!(
            &mut info,
            " · {}/{} RPM",
            self.fan_actual.fan1, self.fan_actual.fan2
        )?;

        write!(&mut info, "\nLogo: {:?}", s.lights_mode.logo_mode)?;
        if s.lights_mode.keyboard_brightness > 0 {
            write!(
                &mut info,
                " · 🔆 {}%",
                brightness_to_percent(s.lights_mode.keyboard_brightness)
            )?;
        }
        // 💡 reflects the always-on *intent* (the keep-alive), not the device-mode
        // read in `observed` -- we keep the device in Normal mode, so that read is
        // always Disable.
        if self.device_state.lights_mode.always_on == LightsAlwaysOn::Enable {
            write!(&mut info, " · 💡")?;
        }

        if s.battery_care != BatteryCare::DISABLE {
            write!(&mut info, "\n🔋 {}%", s.battery_care.to_percent())?;
        }

        // Hard cap well under the 128-UTF-16-unit limit (emoji are 2 units each;
        // counting scalar chars with margin keeps us safe without counting UTF-16).
        Ok(info.chars().take(110).collect())
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

    /// Apply `new_device_state` to the device and refresh the tray UI (icon/tooltip/menu)
    /// -- WITHOUT recording it as the active AC/battery profile or persisting. This is the
    /// shared core of `update` (which also saves) and `update_transient` (which doesn't).
    fn apply_and_refresh(
        &mut self,
        tray_icon: &mut tray_icon::TrayIcon,
        new_device_state: DeviceState,
        device: &device::Device,
    ) -> Result<()> {
        self.device_state = new_device_state;
        self.device_state.apply(device)?;
        // A change is the new ground truth until Mirror reads again, so the tooltip/icon
        // (which render from `observed`) reflect it immediately.
        self.observed = self.device_state;
        (self.menu, self.event_handlers) =
            menu::build(&self.device_state, self.enforce, self.fan_rpm_range)?;
        self.fan_actual = get_fan_rpm(device)?;
        tray_icon.set_icon(Some(self.icon()))?;
        tray_icon.set_tooltip(Some(self.tooltip()?))?;
        tray_icon.set_menu(Some(Box::new(self.menu.clone())));
        Ok(())
    }

    /// Apply a user-chosen state: it becomes the saved profile for the current power
    /// source and is persisted. Use for menu picks, the left-click perf cycle, and
    /// AC/battery profile switches.
    pub fn update(
        &mut self,
        tray_icon: &mut tray_icon::TrayIcon,
        new_device_state: DeviceState,
        device: &device::Device,
    ) -> Result<()> {
        self.apply_and_refresh(tray_icon, new_device_state, device)?;
        if self.ac_power {
            self.ac_state = self.device_state
        } else {
            self.battery_state = self.device_state
        }
        self.persist()?;
        log::info!("state updated to {:?}", new_device_state);
        Ok(())
    }

    /// Apply a *transient* override that must NOT overwrite the saved AC/battery profile
    /// -- used by app-triggered Actions, so that when the app closes we can revert to the
    /// profile the user actually configured. Applies + refreshes the UI but never touches
    /// `ac_state`/`battery_state` and never persists.
    pub fn update_transient(
        &mut self,
        tray_icon: &mut tray_icon::TrayIcon,
        new_device_state: DeviceState,
        device: &device::Device,
    ) -> Result<()> {
        self.apply_and_refresh(tray_icon, new_device_state, device)?;
        log::info!("transient state applied {:?}", new_device_state);
        Ok(())
    }

    /// One-shot reconcile run once at startup, right after `init()`'s `apply()`.
    ///
    /// Why it's needed: `apply()` pushes the stored profile, but a freshly booted --
    /// especially crash-rebooted -- EC can *acknowledge* a perf-mode write without
    /// actually transitioning, and it retains its last-set mode across a reboot. That
    /// leaves the tray showing the intended profile while the EC sits in whatever it
    /// kept (observed 2026-07-09: tray said "Balanced" while the EC was still in the
    /// battery profile). Because the tray never reads the device at startup -- the
    /// first Mirror read is input-gated -- and the shipped default is `enforce = false`,
    /// nothing corrected it until the user manually re-picked a mode.
    ///
    /// So: read the device back and, if the *enforced* fields still differ from intent,
    /// re-assert once. This runs regardless of the `enforce` flag -- correctness at
    /// startup is unconditional; `enforce` only governs the *continuous* tug-of-war with
    /// Synapse. Best-effort: read/apply errors are logged and swallowed so a transient
    /// HID hiccup can't abort startup. Refreshes the tray icon/tooltip from the real read
    /// so the display is honest immediately, not on the next input-gated poll.
    pub fn reconcile_startup(
        &mut self,
        tray_icon: &mut tray_icon::TrayIcon,
        device: &device::Device,
    ) {
        let observed = match DeviceState::read(device) {
            Ok(o) => o,
            Err(e) => {
                log::warn!("startup reconcile: device read failed, skipping: {:?}", e);
                return;
            }
        };
        self.observed = observed;

        if observed.enforced_fields_differ(&self.device_state) {
            log::warn!(
                "startup reconcile: device {:?} != intended {:?}; re-asserting",
                observed.perf_mode,
                self.device_state.perf_mode
            );
            if let Err(e) = self.device_state.enforce_to(device) {
                log::warn!("startup reconcile: re-assert failed: {:?}", e);
            } else {
                // Reflect the corrected device in `observed`. Prefer a fresh read;
                // fall back to intent (keeping the real brightness, which enforce_to
                // doesn't touch) if the read fails.
                self.observed = DeviceState::read(device).unwrap_or_else(|_| {
                    let brightness = self.observed.lights_mode.keyboard_brightness;
                    let mut corrected = self.device_state;
                    corrected.lights_mode.keyboard_brightness = brightness;
                    corrected
                });
            }
        } else {
            log::info!("startup reconcile: device matches intended state");
        }

        // The icon/tooltip render from `observed`; refresh them so a mismatch (or a
        // correction) shows now rather than waiting for the next input-gated Mirror poll.
        let _ = tray_icon.set_icon(Some(self.icon()));
        if let Ok(tooltip) = self.tooltip() {
            let _ = tray_icon.set_tooltip(Some(tooltip));
        }
    }
}

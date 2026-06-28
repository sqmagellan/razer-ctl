//! `ProgramState`: the live application state behind the tray -- current intent
//! (`device_state`), the last-read reality (`observed`), the AC/battery profiles,
//! and the current menu + its handler map. Owns rendering (icon/tooltip), the
//! perf-mode cycle, persistence, and applying a new state to the device.

use anyhow::Result;
use std::collections::HashMap;

use librazer::device;
use librazer::types::{BatteryCare, CpuBoost, GpuBoost, LightsAlwaysOn};
use tray_icon::menu::Menu;

use crate::menu;
use crate::state::{get_fan_rpm, brightness_to_percent, ConfigState, DeviceState, FanRpm, FanSpeed, PerfMode};

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
}

impl ProgramState {
    pub fn new(device_state: DeviceState, fan_last: FanRpm, enforce: bool) -> Result<Self> {
        let (menu, event_handlers) = menu::build(&device_state, enforce)?;
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
        })
    }

    /// Persist the current AC/battery profiles + enforce flag. Single source of truth
    /// so a future ConfigState field can't be silently dropped by one of the call
    /// sites (there used to be three inline `confy::store` literals).
    pub fn persist(&self) -> Result<()> {
        confy::store(
            crate::PKG_NAME,
            None,
            ConfigState {
                ac_state: self.ac_state,
                battery_state: self.battery_state,
                enforce: self.enforce,
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
            perf_mode: match self.device_state.perf_mode {
                PerfMode::Battery => PerfMode::Silent,
                PerfMode::Silent => PerfMode::Balanced,
                PerfMode::Balanced => PerfMode::Performance,
                PerfMode::Performance => PerfMode::Hyperboost,
                PerfMode::Hyperboost => PerfMode::Custom(CpuBoost::Boost, GpuBoost::High),
                PerfMode::Custom(..) => PerfMode::Battery,
            },
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
        write!(&mut info, " · {}/{} RPM", self.fan_actual.fan1, self.fan_actual.fan2)?;

        write!(&mut info, "\nLogo: {:?}", s.lights_mode.logo_mode)?;
        if s.lights_mode.keyboard_brightness > 0 {
            write!(&mut info, " · 🔆 {}%", brightness_to_percent(s.lights_mode.keyboard_brightness))?;
        }
        if s.lights_mode.always_on == LightsAlwaysOn::Enable {
            write!(&mut info, " · 💡")?;
        }

        if s.battery_care != BatteryCare::Disable {
            write!(&mut info, "\n🔋 {}%", s.battery_care.to_percent())?;
        }

        // Hard cap well under the 128-UTF-16-unit limit (emoji are 2 units each;
        // counting scalar chars with margin keeps us safe without counting UTF-16).
        Ok(info.chars().take(110).collect())
    }

    pub fn icon(&self) -> tray_icon::Icon {
        use std::sync::OnceLock;
        // Decode the embedded PNGs once per process. icon() runs on every Mirror
        // refresh (and every update), so re-decoding a PNG each call was wasted work.
        // Indexed by perf mode; the decoded RGBA is cheap to clone for from_rgba.
        static ICONS: OnceLock<[(Vec<u8>, u32, u32); 6]> = OnceLock::new();
        let icons = ICONS.get_or_init(|| {
            let decode = |bytes: &[u8]| {
                let img = image::load_from_memory(bytes)
                    .expect("embedded icon failed to decode")
                    .into_rgba8();
                let (w, h) = img.dimensions();
                (img.into_raw(), w, h)
            };
            [
                decode(include_bytes!("../icons/razer-blue.png")),   // 0 Battery
                decode(include_bytes!("../icons/razer-yellow.png")), // 1 Silent
                decode(include_bytes!("../icons/razer-green.png")),  // 2 Balanced
                decode(include_bytes!("../icons/razer-red.png")),    // 3 Performance
                decode(include_bytes!("../icons/razer-violet.png")), // 4 Hyperboost
                decode(include_bytes!("../icons/razer-brown.png")),  // 5 Custom
            ]
        });

        let idx = match self.observed.perf_mode {
            PerfMode::Battery => 0,
            PerfMode::Silent => 1,
            PerfMode::Balanced => 2,
            PerfMode::Performance => 3,
            PerfMode::Hyperboost => 4,
            PerfMode::Custom(_, _) => 5,
        };
        let (rgba, width, height) = &icons[idx];
        tray_icon::Icon::from_rgba(rgba.clone(), *width, *height).expect("failed to build tray icon")
    }

    pub fn update(
        &mut self,
        tray_icon: &mut tray_icon::TrayIcon,
        new_device_state: DeviceState,
        device: &device::Device,
    ) -> Result<()> {
        self.device_state = new_device_state;
        self.device_state.apply(device)?;
        // A user-driven change is the new ground truth until Mirror reads again,
        // so the tooltip/icon (which render from `observed`) reflect it immediately.
        self.observed = self.device_state;
        (self.menu, self.event_handlers) = menu::build(&self.device_state, self.enforce)?;
        self.fan_actual = get_fan_rpm(device)?;
        if self.ac_power {
            self.ac_state = self.device_state
        } else {
            self.battery_state = self.device_state
        }
        self.persist()?;
        tray_icon.set_icon(Some(self.icon()))?;
        tray_icon.set_tooltip(Some(self.tooltip()?))?;
        tray_icon.set_menu(Some(Box::new(self.menu.clone())));

        log::info!("state updated to {:?}", new_device_state);
        Ok(())
    }
}

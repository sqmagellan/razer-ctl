#![windows_subsystem = "windows"]

use serde::{Deserialize, Serialize};
use strum::IntoEnumIterator;
use anyhow::Error;

use librazer::types::{BatteryCare, CpuBoost, FanMode, GpuBoost, LightsAlwaysOn, LogoMode};
use librazer::{command, device};

use tao::event_loop::{ControlFlow, EventLoopBuilder};
use tray_icon::{
    menu::{CheckMenuItem, IsMenuItem, Menu, MenuEvent, PredefinedMenuItem, MenuItem, Submenu, MenuId},
    TrayIconBuilder, TrayIconEvent, 
};

use std::process::Command as procCommand;
use sysinfo::{ProcessExt, Signal, System, SystemExt};
#[cfg(target_os = "windows")]
use std::sync::atomic::{AtomicBool, Ordering};

use single_instance::SingleInstance;

#[cfg(target_os = "windows")]
static KEY_PRESSED: AtomicBool = AtomicBool::new(false);

// Tracks whether the console display is powered on. Updated by the power-setting
// notification handler; read by the event loop to gate the firmware always-on
// flag. Starts true (fail-open: keyboard stays lit if we never hear otherwise).
#[cfg(target_os = "windows")]
static DISPLAY_ON: AtomicBool = AtomicBool::new(true);

#[cfg(target_os = "windows")]
use windows::Win32::Foundation::HANDLE;
#[cfg(target_os = "windows")]
use windows::Win32::System::Threading::{
    GetCurrentProcess, ProcessPowerThrottling, SetPriorityClass, SetProcessInformation,
    IDLE_PRIORITY_CLASS, PROCESS_POWER_THROTTLING_CURRENT_VERSION,
    PROCESS_POWER_THROTTLING_EXECUTION_SPEED, PROCESS_POWER_THROTTLING_STATE,
};
#[cfg(target_os = "windows")]
use windows::Win32::System::Power::{GetSystemPowerStatus, SYSTEM_POWER_STATUS};

const PKG_NAME: &str = env!("CARGO_PKG_NAME");

#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
enum FanSpeed {
    Auto,
    Manual(u16),
}

#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
enum PerfMode {
    Battery,
    Silent,
    Balanced,
    Performance,
    Hyperboost,
    Custom(CpuBoost, GpuBoost),
}

#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
struct LightsMode {
    logo_mode: LogoMode,
    keyboard_brightness: u8,
    always_on: LightsAlwaysOn,
}

#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
struct FanRpm {
    fan1: u16,
    fan2: u16,
}

#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
struct DeviceState {
    perf_mode: PerfMode,
    lights_mode: LightsMode,
    battery_care: BatteryCare,
    fan_speed : FanSpeed,
}

type Result<T> = std::result::Result<T, Error>;

/// Map a 0..=100 percentage to the device's 0..255 keyboard-brightness scale,
/// rounded. Used by the brightness submenu so its 10% steps span the full
/// hardware range (0->0, 10->26, 50->128, 100->255).
fn percent_to_brightness(percent: u8) -> u8 {
    ((percent as u16 * 255 + 50) / 100) as u8
}

impl DeviceState {
    /// Read the device's *actual* current state. Used by the Mirror refresh to keep
    /// the tray display honest. This is read-only: callers must treat the result as
    /// something to *display*, never to re-apply (re-applying would fight external
    /// changes such as Fn-key brightness and could overwrite saved AC/battery
    /// profiles). Returns Err on any transient HID failure; callers should swallow
    /// that and keep the last known-good values rather than propagating it.
    fn read(device: &device::Device) -> Result<Self> {
        let perf_mode = match command::get_perf_mode(device)? {
            (librazer::types::PerfMode::Battery, _) => PerfMode::Battery,
            (librazer::types::PerfMode::Silent, _) => PerfMode::Silent,
            (librazer::types::PerfMode::Balanced, _) => PerfMode::Balanced,
            (librazer::types::PerfMode::Performance, _) => PerfMode::Performance,
            (librazer::types::PerfMode::Hyperboost, _) => PerfMode::Hyperboost,
            (librazer::types::PerfMode::Custom, _) => {
                let cpu_boost = command::get_cpu_boost(device)?;
                let gpu_boost = command::get_gpu_boost(device)?;
                PerfMode::Custom(cpu_boost, gpu_boost)
            }
        };

        let fan_speed = match command::get_perf_mode(device)? {
            (_, FanMode::Auto) => FanSpeed::Auto,
            (_, FanMode::Manual) => {
                let rpm = command::get_fan_rpm(device, librazer::types::FanZone::Zone1)?;
                FanSpeed::Manual(rpm)
            }
        };
        let lights_mode = LightsMode {
            logo_mode: command::get_logo_mode(device)?,
            keyboard_brightness: command::get_keyboard_brightness(device)?,
            always_on: command::get_lights_always_on(device)?,
        };

        let battery_care = command::get_battery_care(device)?;

        Ok(Self {
            perf_mode,
            lights_mode,
            battery_care,
            fan_speed,
        })
    }

    fn apply(&self, device: &device::Device) -> Result<()> {
        match self.perf_mode {
            PerfMode::Battery => command::set_perf_mode(device, librazer::types::PerfMode::Battery),
            PerfMode::Silent => command::set_perf_mode(device, librazer::types::PerfMode::Silent),
            PerfMode::Balanced => command::set_perf_mode(device, librazer::types::PerfMode::Balanced),
            PerfMode::Performance => command::set_perf_mode(device, librazer::types::PerfMode::Performance),
            PerfMode::Hyperboost => command::set_perf_mode(device, librazer::types::PerfMode::Hyperboost),
            PerfMode::Custom(cpu_boost, gpu_boost) => {
                command::set_perf_mode(device, librazer::types::PerfMode::Custom)?;
                command::set_cpu_boost(device, cpu_boost)?;
                command::set_gpu_boost(device, gpu_boost)
            }
        }?;

        if let Err(e) = match self.fan_speed {
            FanSpeed::Auto => command::set_fan_mode(device, librazer::types::FanMode::Auto),
            FanSpeed::Manual(rpm) => command::set_fan_mode(device, librazer::types::FanMode::Manual)
                .and_then(|_| command::set_fan_rpm(device, rpm, false)),
        } {
            log::warn!("fan speed command failed: {:?}", e);
        }

        match self.lights_mode.logo_mode {
            LogoMode::Static => command::set_logo_mode(device, LogoMode::Static),
            LogoMode::Breathing => command::set_logo_mode(device, LogoMode::Breathing),
            LogoMode::Off => command::set_logo_mode(device, LogoMode::Off),
        }?;

        command::set_lights_always_on(device, self.lights_mode.always_on)?;
        command::set_keyboard_brightness(device, self.lights_mode.keyboard_brightness)?;
        command::set_battery_care(device, self.battery_care)
    }

    /// Re-assert the "enforced" subset of settings -- perf mode, fan, logo, and
    /// battery care -- WITHOUT touching keyboard brightness or lights-always-on.
    /// This is what the opt-in Enforce mode uses to win a tug-of-war with Synapse.
    /// Brightness is excluded so it stays on the adopt path (Fn keys keep working);
    /// always-on is excluded because it's owned by the display-state gate (it gets
    /// dropped while the display is off). Mirrors apply() minus those two writes.
    fn enforce_to(&self, device: &device::Device) -> Result<()> {
        match self.perf_mode {
            PerfMode::Battery => command::set_perf_mode(device, librazer::types::PerfMode::Battery),
            PerfMode::Silent => command::set_perf_mode(device, librazer::types::PerfMode::Silent),
            PerfMode::Balanced => command::set_perf_mode(device, librazer::types::PerfMode::Balanced),
            PerfMode::Performance => command::set_perf_mode(device, librazer::types::PerfMode::Performance),
            PerfMode::Hyperboost => command::set_perf_mode(device, librazer::types::PerfMode::Hyperboost),
            PerfMode::Custom(cpu_boost, gpu_boost) => {
                command::set_perf_mode(device, librazer::types::PerfMode::Custom)?;
                command::set_cpu_boost(device, cpu_boost)?;
                command::set_gpu_boost(device, gpu_boost)
            }
        }?;

        if let Err(e) = match self.fan_speed {
            FanSpeed::Auto => command::set_fan_mode(device, librazer::types::FanMode::Auto),
            FanSpeed::Manual(rpm) => command::set_fan_mode(device, librazer::types::FanMode::Manual)
                .and_then(|_| command::set_fan_rpm(device, rpm, false)),
        } {
            log::warn!("enforce: fan command failed: {:?}", e);
        }

        match self.lights_mode.logo_mode {
            LogoMode::Static => command::set_logo_mode(device, LogoMode::Static),
            LogoMode::Breathing => command::set_logo_mode(device, LogoMode::Breathing),
            LogoMode::Off => command::set_logo_mode(device, LogoMode::Off),
        }?;

        command::set_battery_care(device, self.battery_care)
    }

    fn perf_delta(
        &self,
        cpu_boost: Option<CpuBoost>,
        gpu_boost: Option<GpuBoost>,
    ) -> Self {
        DeviceState {
            perf_mode: if let PerfMode::Custom(cb, gb) = self.perf_mode {
                PerfMode::Custom(
                    cpu_boost.unwrap_or(cb),
                    gpu_boost.unwrap_or(gb)
                )
            } else {
                PerfMode::Custom(
                    cpu_boost.unwrap_or(CpuBoost::Boost),
                    gpu_boost.unwrap_or(GpuBoost::High)
                )
            },
            ..*self
        }
    }
}

impl Default for DeviceState {
    fn default() -> Self {
        Self {
            perf_mode: PerfMode::Performance,
            lights_mode: LightsMode {
                logo_mode: LogoMode::Off,
                keyboard_brightness: 0,
                always_on: LightsAlwaysOn::Disable,
            },
            battery_care: BatteryCare::Percent80,
            fan_speed : FanSpeed::Auto,
        }
    }
}

trait DeviceStateDelta<T> {
    fn delta(&self, property: T) -> Self;
}

impl DeviceStateDelta<CpuBoost> for DeviceState {
    fn delta(&self, cpu_boost: CpuBoost) -> Self {
        self.perf_delta(Some(cpu_boost), None)
    }
}

impl DeviceStateDelta<GpuBoost> for DeviceState {
    fn delta(&self, gpu_boost: GpuBoost) -> Self {
        self.perf_delta(None, Some(gpu_boost))
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
struct ConfigState {
    ac_state: DeviceState,
    battery_state: DeviceState,
    // Opt-in "win against Synapse" mode. #[serde(default)] keeps older config
    // files (written before this field existed) loadable -> defaults to false.
    #[serde(default)]
    enforce: bool,
}

impl Default for ConfigState {
    fn default() -> Self {
        Self {
            ac_state: DeviceState {..Default::default()},
            battery_state : DeviceState {
                    perf_mode : PerfMode::Battery,
                    ..Default::default()
                },
            enforce: false,
        }
    }
}


struct ProgramState {
    device_state: DeviceState,
    observed: DeviceState,
    ac_state: DeviceState,
    battery_state: DeviceState,
    event_handlers: std::collections::HashMap<String, DeviceState>,
    menu: Menu,
    fan_actual : FanRpm,
    ac_power : bool,
    enforce: bool,
}

impl ProgramState {
    fn new(device_state: DeviceState, fan_last : FanRpm, enforce: bool) -> Result<Self> {
        let (menu, event_handlers) = Self::create_menu_and_handlers(&device_state, enforce)?;
        let fan_actual = fan_last.clone();
        let ac_power = true;
        let ac_state = device_state.clone();
        let battery_state = device_state.clone();
        let observed = device_state.clone();
        Ok(Self {
            device_state,
            observed,
            ac_state,
            battery_state,
            event_handlers,
            menu,
            fan_actual,
            ac_power,
            enforce,
        })
    }

    fn create_menu_and_handlers(
        dstate: &DeviceState,
        enforce: bool,
    ) -> Result<(Menu, std::collections::HashMap<String, DeviceState>)> {
        let mut event_handlers = std::collections::HashMap::new();
        let menu = Menu::new();
        // header

        // perf
        let perf_modes = Submenu::new("Performance", true);
        // Battery
        perf_modes.append(&CheckMenuItem::with_id(
            format!("{:?}", PerfMode::Battery),
            "Battery",
            dstate.perf_mode != PerfMode::Battery,
            dstate.perf_mode == PerfMode::Battery,
            None,
        ))?;
        event_handlers.insert(
            format!("{:?}", PerfMode::Battery),
            DeviceState {
                perf_mode: PerfMode::Battery,
                ..*dstate
            },
        );
        // silent
        perf_modes.append(&CheckMenuItem::with_id(
            format!("{:?}", PerfMode::Silent),
            "Silent",
            dstate.perf_mode != PerfMode::Silent,
            dstate.perf_mode == PerfMode::Silent,
            None,
        ))?;
        event_handlers.insert(
            format!("{:?}", PerfMode::Silent),
            DeviceState {
                perf_mode: PerfMode::Silent,
                ..*dstate
            },
        );
        // balanced
        perf_modes.append(&CheckMenuItem::with_id(
            format!("{:?}", PerfMode::Balanced),
            "Balanced",
            dstate.perf_mode != PerfMode::Balanced,
            dstate.perf_mode == PerfMode::Balanced,
            None,
        ))?;
        event_handlers.insert(
            format!("{:?}", PerfMode::Balanced),
            DeviceState {
                perf_mode: PerfMode::Balanced,
                ..*dstate
            },
        );
        // performance
        perf_modes.append(&CheckMenuItem::with_id(
            format!("{:?}", PerfMode::Performance),
            "Performance",
            dstate.perf_mode != PerfMode::Performance,
            dstate.perf_mode == PerfMode::Performance,
            None,
        ))?;
        event_handlers.insert(
            format!("{:?}", PerfMode::Performance),
            DeviceState {
                perf_mode: PerfMode::Performance,
                ..*dstate
            },
        );
        // Hyperboost
        perf_modes.append(&CheckMenuItem::with_id(
            format!("{:?}", PerfMode::Hyperboost),
            "Hyperboost",
            dstate.perf_mode != PerfMode::Hyperboost,
            dstate.perf_mode == PerfMode::Hyperboost,
            None,
        ))?;
        event_handlers.insert(
            format!("{:?}", PerfMode::Hyperboost),
            DeviceState {
                perf_mode: PerfMode::Hyperboost,
                ..*dstate
            },
        );

        // custom
        let cpu_boosts: Vec<CheckMenuItem> = CpuBoost::iter()
            .map(|boost| {
                let event_id = format!("cpu_boost:{:?}", boost);
                event_handlers.insert(event_id.clone(), dstate.delta(boost));
                let checked = matches!(dstate.perf_mode, PerfMode::Custom(b, _) if b == boost);
                CheckMenuItem::with_id(event_id, format!("{:?}", boost), !checked, checked, None)
            })
            .collect();

        let gpu_boosts: Vec<CheckMenuItem> = GpuBoost::iter()
            .map(|boost| {
                let event_id = format!("gpu_boost:{:?}", boost);
                event_handlers.insert(event_id.clone(), dstate.delta(boost));
                let checked = matches!(dstate.perf_mode, PerfMode::Custom(_, b) if b == boost);
                CheckMenuItem::with_id(event_id, format!("{:?}", boost), !checked, checked, None)
            })
            .collect();

        let separator = PredefinedMenuItem::separator();

        perf_modes.append(&Submenu::with_items(
            "Custom",
            true,
            &cpu_boosts
                .iter()
                .map(|i| i as &dyn IsMenuItem)
                .chain([&separator as &dyn IsMenuItem])
                .chain(gpu_boosts.iter().map(|i| i as &dyn IsMenuItem))
                .collect::<Vec<_>>(),
        )?)?;

        menu.append(&perf_modes)?;

        // Fan Speed
        menu.append(&PredefinedMenuItem::separator())?;
        let fan_speeds: Vec<CheckMenuItem> = [CheckMenuItem::with_id(
            "fan_speeds:auto",
            "Fan: Auto",
            dstate.fan_speed != FanSpeed::Auto,
            dstate.fan_speed == FanSpeed::Auto,
            None,
        )]
        .into_iter()
        .chain((0..=5500).step_by(500).map(|rpm| {
            let event_id = format!("fan_speeds:{}", rpm);
            event_handlers.insert(
                event_id.clone(),
                DeviceState {
                    fan_speed: FanSpeed::Manual(rpm),
                    ..*dstate
                },
            );
            CheckMenuItem::with_id(
                event_id,
                format!("Fan: {} RPM", rpm),
                dstate.fan_speed != FanSpeed::Manual(rpm),
                dstate.fan_speed == FanSpeed::Manual(rpm),
                None,
            )
        }))
        .collect();
        event_handlers.insert(
            "fan_speeds:auto".to_string(),
            DeviceState {
                fan_speed: FanSpeed::Auto,
                ..*dstate
            },
        );

        menu.append(&Submenu::with_items(
            "Fan Speed",
            true,
            &fan_speeds
                .iter()
                .map(|i| i as &dyn IsMenuItem)
                .collect::<Vec<_>>(),
        )?)?;

        // logo
        menu.append(&PredefinedMenuItem::separator())?;
        let modes = LogoMode::iter()
            .map(|mode| {
                let event_id = format!("logo_mode:{:?}", mode);
                event_handlers.insert(
                    event_id.clone(),
                    DeviceState {
                        lights_mode: LightsMode {
                            logo_mode: mode,
                            ..dstate.lights_mode
                        },
                        ..*dstate
                    },
                );
                CheckMenuItem::with_id(
                    event_id,
                    format!("{:?}", mode),
                    dstate.lights_mode.logo_mode != mode,
                    dstate.lights_mode.logo_mode == mode,
                    None,
                )
            })
            .collect::<Vec<_>>();

        menu.append(&Submenu::with_items(
            "Logo",
            true,
            &modes
                .iter()
                .map(|i| i as &dyn IsMenuItem)
                .collect::<Vec<_>>(),
        )?)?;
        menu.append(&PredefinedMenuItem::separator())?;

        // keyboard always on
        menu.append(&CheckMenuItem::with_id(
            "lights_always_on",
            "Keyboard Always On",
            true,
            dstate.lights_mode.always_on == LightsAlwaysOn::Enable,
            None,
        ))?;
        event_handlers.insert(
            "lights_always_on".to_string(),
            DeviceState {
                lights_mode: LightsMode {
                    always_on: match dstate.lights_mode.always_on {
                        LightsAlwaysOn::Enable => LightsAlwaysOn::Disable,
                        LightsAlwaysOn::Disable => LightsAlwaysOn::Enable,
                    },
                    ..dstate.lights_mode
                },
                ..*dstate
            },
        );

        // Brightness submenu: 0..100% in 10% steps, mapped onto the device's full
        // 0..255 range. The hardware Fn keys use a 16-step ladder that doesn't line
        // up with the 10% marks, so an external (Fn-key) value usually lands between
        // our steps -- we highlight the *nearest* percent step so there's always
        // exactly one check. The exact 0..255 value still shows in the tooltip.
        let current_brightness = dstate.lights_mode.keyboard_brightness;
        let nearest_percent: u8 = (0u8..=100)
            .step_by(10)
            .min_by_key(|p| (percent_to_brightness(*p) as i32 - current_brightness as i32).abs())
            .unwrap_or(0);

        let brightness_modes: Vec<CheckMenuItem> = (0u8..=100)
            .step_by(10)
            .map(|percent| {
                let event_id = format!("brightness:{}", percent);
                event_handlers.insert(
                    event_id.clone(),
                    DeviceState {
                        lights_mode: LightsMode {
                            keyboard_brightness: percent_to_brightness(percent),
                            ..dstate.lights_mode
                        },
                        ..*dstate
                    },
                );
                CheckMenuItem::with_id(
                    event_id,
                    format!("{}%", percent),
                    percent != nearest_percent,
                    percent == nearest_percent,
                    None,
                )
            })
            .collect();

        menu.append(&Submenu::with_items(
            "Keyboard Brightness",
            true,
            &brightness_modes
                .iter()
                .map(|i| i as &dyn IsMenuItem)
                .collect::<Vec<_>>(),
        )?)?;

        // battery care submenu
        menu.append(&PredefinedMenuItem::separator())?;
        
        let battery_care_options = [
            (BatteryCare::Percent50, "50%", "battery_care_50"),
            (BatteryCare::Percent55, "55%", "battery_care_55"),
            (BatteryCare::Percent60, "60%", "battery_care_60"),
            (BatteryCare::Percent65, "65%", "battery_care_65"),
            (BatteryCare::Percent70, "70%", "battery_care_70"),
            (BatteryCare::Percent75, "75%", "battery_care_75"),
            (BatteryCare::Percent80, "80%", "battery_care_80"),
            (BatteryCare::Disable, "Disabled (100%)", "battery_care_disable"),
        ];
        
        let battery_care_items: Vec<CheckMenuItem> = battery_care_options
            .iter()
            .map(|(mode, label, id)| {
                event_handlers.insert(
                    id.to_string(),
                    DeviceState {
                        battery_care: *mode,
                        ..*dstate
                    },
                );
                CheckMenuItem::with_id(
                    id,
                    label,
                    true,
                    dstate.battery_care == *mode,
                    None,
                )
            })
            .collect();
        
        menu.append(&Submenu::with_items(
            "Battery Care",
            true,
            &battery_care_items
                .iter()
                .map(|i| i as &dyn IsMenuItem)
                .collect::<Vec<_>>(),
        )?)?;

        // Enforce settings (opt-in "win against Synapse"). Windows-only, since
        // Synapse is a Windows product. Off by default.
        #[cfg(target_os = "windows")]
        {
            menu.append(&PredefinedMenuItem::separator())?;
            menu.append(&CheckMenuItem::with_id(
                "toggle_enforce",
                "Enforce Settings (override Synapse)",
                true,
                enforce,
                None,
            ))?;
        }

        // Start with Windows (launch at login). Windows-only.
        #[cfg(target_os = "windows")]
        {
            menu.append(&PredefinedMenuItem::separator())?;
            menu.append(&CheckMenuItem::with_id(
                "toggle_autostart",
                "Start with Windows",
                true,
                autostart_enabled(),
                None,
            ))?;
        }

        // gpu task killer
        menu.append(&PredefinedMenuItem::separator())?;
        let terminate_item = MenuItem::with_id("dgpu_terminate_proc","Terminate dGPU Processes", true, None);
        menu.append(&terminate_item)?;
        // footer
        menu.append(&PredefinedMenuItem::separator())?;
        menu.append(&PredefinedMenuItem::about(None, Some(Self::about())))?;
        menu.append(&PredefinedMenuItem::quit(None))?;

        Ok((menu, event_handlers))
    }

    fn handle_event(&self, event_id: &str) -> Result<DeviceState> {
        let next_state = self.event_handlers.get(event_id).ok_or(anyhow::anyhow!(
            "No event handler found for event_id: {}",
            event_id
        ))?;
        Ok(*next_state)
    }

    fn about() -> tray_icon::menu::AboutMetadata {
        tray_icon::menu::AboutMetadata {
            name: Some(PKG_NAME.into()),
            version: Some(env!("CARGO_PKG_VERSION").into()),
            authors: Some(
                env!("CARGO_PKG_AUTHORS")
                    .split(';')
                    .map(|a| a.trim().to_string())
                    .collect::<Vec<_>>(),
            ),
            website: Some(format!(
                "{}\nLog: {}",
                env!("CARGO_PKG_HOMEPAGE"),
                get_logging_file_path().display()
            )),
            comments: Some(env!("CARGO_PKG_DESCRIPTION").into()),
            ..Default::default()
        }
    }

    fn get_next_perf_mode(&self) -> DeviceState {
        DeviceState {
            perf_mode: match self.device_state.perf_mode {
                PerfMode::Battery => PerfMode::Silent,
                PerfMode::Silent => PerfMode::Balanced,
                PerfMode::Balanced => PerfMode::Performance,
                PerfMode::Performance => PerfMode::Hyperboost,
                PerfMode::Hyperboost => {
                    PerfMode::Custom(CpuBoost::Boost, GpuBoost::High)
                }
                PerfMode::Custom(..) => PerfMode::Battery,
            },
            ..self.device_state
        }
    }

    fn tooltip(&self) -> Result<String> {
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

        write!(&mut info, "\nLogo {:?}", s.lights_mode.logo_mode)?;
        if s.lights_mode.keyboard_brightness > 0 {
            write!(&mut info, " · 🔆 {}", s.lights_mode.keyboard_brightness)?;
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

    fn icon(&self) -> tray_icon::Icon {
        let razer_red = include_bytes!("../icons/razer-red.png");
        let razer_blue = include_bytes!("../icons/razer-blue.png");
        let razer_brown = include_bytes!("../icons/razer-brown.png");
        let razer_yellow = include_bytes!("../icons/razer-yellow.png");
        let razer_green = include_bytes!("../icons/razer-green.png");
        let razer_violet = include_bytes!("../icons/razer-violet.png");

        let image = match self.observed.perf_mode {
            PerfMode::Battery => image::load_from_memory(razer_blue),
            PerfMode::Silent => image::load_from_memory(razer_yellow),
            PerfMode::Balanced => image::load_from_memory(razer_green),
            PerfMode::Performance => image::load_from_memory(razer_red),
            PerfMode::Hyperboost => image::load_from_memory(razer_violet),
            PerfMode::Custom(_, _) => image::load_from_memory(razer_brown),
        };

        let (icon_rgba, icon_width, icon_height) = {
            let image = image.expect("Failed to open icon").into_rgba8();
            let (width, height) = image.dimensions();
            let rgba = image.into_raw();
            (rgba, width, height)
        };
        tray_icon::Icon::from_rgba(icon_rgba, icon_width, icon_height).expect("Failed to open icon")
    }

    fn update(
        &mut self,
        tray_icon: &mut tray_icon::TrayIcon,
        new_device_state: DeviceState,
        device: &device::Device
    ) -> Result<()> {
        self.device_state = new_device_state.clone();
        self.device_state.apply(device)?;
        // A user-driven change is the new ground truth until Mirror reads again,
        // so the tooltip/icon (which render from `observed`) reflect it immediately.
        self.observed = self.device_state.clone();
        (self.menu, self.event_handlers) = Self::create_menu_and_handlers(&self.device_state, self.enforce)?;
        self.fan_actual = get_fan_rpm(device)?;
        if self.ac_power {
            self.ac_state = self.device_state.clone()
        } else {
            self.battery_state = self.device_state.clone()
        }
        confy::store(PKG_NAME, None, &ConfigState {ac_state : self.ac_state,battery_state :  self.battery_state, enforce: self.enforce})?;
        tray_icon.set_icon(Some(self.icon()))?;
        tray_icon.set_tooltip(Some(self.tooltip()?))?;
        tray_icon.set_menu(Some(Box::new(self.menu.clone())));

        log::info!("state updated to {:?}", new_device_state);
        Ok(())
    }

}



#[cfg(target_os = "windows")]
fn get_power_state() -> Result<bool> {
    let mut ac_power : bool = true;
    unsafe {
        let mut status = SYSTEM_POWER_STATUS::default();
        match GetSystemPowerStatus(&mut status) {
            Ok(()) => {
                match status.ACLineStatus {
                    0 => ac_power = false,
                    _ => ac_power = true
                }
            }
            Err(e) => {
                eprintln!("Failed to get power status: {:?}", e);
            }
        }
    }
    Ok(ac_power)
}

#[cfg(target_os = "linux")]
fn get_power_state() -> Result<bool> {
    // Try AC adapter first
    if let Ok(online) = std::fs::read_to_string("/sys/class/power_supply/AC/online")
        .or_else(|_| std::fs::read_to_string("/sys/class/power_supply/AC0/online"))
        .or_else(|_| std::fs::read_to_string("/sys/class/power_supply/ACAD/online"))
    {
        return Ok(online.trim() == "1");
    }
    
    // Fallback: check battery status
    if let Ok(status) = std::fs::read_to_string("/sys/class/power_supply/BAT0/status")
        .or_else(|_| std::fs::read_to_string("/sys/class/power_supply/BAT1/status"))
    {
        let status = status.trim();
        return Ok(status == "Charging" || status == "Full" || status == "Not charging");
    }
    
    // Default to AC power if we can't detect
    log::warn!("Could not detect power state, assuming AC power");
    Ok(true)
}

fn get_fan_rpm(device: &device::Device) -> Result<FanRpm> {
    let fan_actual = FanRpm {
        fan1 : command::get_fan_actual_rpm(device, librazer::types::FanZone::Zone1)?,
        fan2 : command::get_fan_actual_rpm(device, librazer::types::FanZone::Zone2)?,
    };
    //log::info!("fans updated to {:?}", fan_actual);
    Ok(fan_actual)
}

#[cfg(target_os = "windows")]
const AUTOSTART_RUN_KEY: &str = r"Software\Microsoft\Windows\CurrentVersion\Run";
#[cfg(target_os = "windows")]
const AUTOSTART_VALUE_NAME: &str = "razer-tray";

/// Whether razer-tray is registered to launch at login (HKCU Run key).
#[cfg(target_os = "windows")]
fn autostart_enabled() -> bool {
    use winreg::enums::HKEY_CURRENT_USER;
    use winreg::RegKey;
    RegKey::predef(HKEY_CURRENT_USER)
        .open_subkey(AUTOSTART_RUN_KEY)
        .and_then(|run| run.get_value::<String, _>(AUTOSTART_VALUE_NAME))
        .is_ok()
}

/// Register/unregister razer-tray for launch at login via the per-user Run key.
#[cfg(target_os = "windows")]
fn set_autostart(enable: bool) -> Result<()> {
    use winreg::enums::HKEY_CURRENT_USER;
    use winreg::RegKey;
    let (run, _) = RegKey::predef(HKEY_CURRENT_USER).create_subkey(AUTOSTART_RUN_KEY)?;
    if enable {
        let exe = std::env::current_exe()?;
        // Quote the path so a space in it doesn't break the command.
        run.set_value(AUTOSTART_VALUE_NAME, &format!("\"{}\"", exe.display()))?;
        log::info!("Autostart enabled: {}", exe.display());
    } else {
        let _ = run.delete_value(AUTOSTART_VALUE_NAME); // ignore "not present"
        log::info!("Autostart disabled");
    }
    Ok(())
}

#[cfg(target_os = "windows")]
fn gpu_taskkill() -> Result<()> {
    use std::os::windows::process::CommandExt;
    let whitelist: &[&str] = &["explorer.exe", "Insufficient Permissions"];

    const CREATE_NO_WINDOW: u32 = 0x08000000;
    let output = match procCommand::new("nvidia-smi")
        .args(&["--query-compute-apps=name,pid", "--format=csv,noheader"])
        .creation_flags(CREATE_NO_WINDOW)
        .output()
    {
        Ok(o) => o,
        Err(e) => {
            // No NVIDIA tools on PATH (or it failed to launch). Nothing to terminate;
            // don't panic -- this runs from the tray event loop and a panic here would
            // crash/recover-churn the app the moment the user clicks the menu item.
            log::info!("nvidia-smi not available ({e}); skipping dGPU terminate");
            return Ok(());
        }
    };

    if !output.status.success() {
        log::info!("nvidia-smi command failed or no GPU processes found");
        return Ok(());
    }

    let stdout = String::from_utf8_lossy(&output.stdout);
    let lines = stdout.lines();

    let mut pids_to_kill = Vec::new();

    for line in lines {
        let parts: Vec<&str> = line.split(',').map(|s| s.trim()).collect();
        if parts.len() != 2 {
            continue;
        }

        let name = parts[0];
        let pid: u32 = match parts[1].parse() {
            Ok(p) => p,
            Err(_) => continue,
        };

        if whitelist.contains(&name) {
            log::info!("Skipping whitelisted process: {} ({})", pid, name);
        } else {
            pids_to_kill.push((pid, name.to_string()));
        }
    }

    if pids_to_kill.is_empty() {
        log::info!("No GPU-using processes to kill.");
        return Ok(());
    }

    let mut sys = System::new_all();
    sys.refresh_processes();

    for (pid, name) in pids_to_kill {
        if let Some(process) = sys.process(sysinfo::Pid::from(pid as usize)) {
            log::info!("Attempting to kill process {} ({})", pid, name);
            if process.kill_with(Signal::Kill).unwrap_or(false) {
                log::info!("Successfully killed PID {}", pid);
            } else {
                log::info!("Failed to kill PID {}", pid);
            }
        } else {
            log::info!("Process with PID {} not found", pid);
        }
    }

    Ok(())
}

#[cfg(target_os = "linux")]
fn gpu_taskkill() -> Result<()> {
    // dGPU process termination for Linux
    let output = procCommand::new("nvidia-smi")
        .args(&["--query-compute-apps=name,pid", "--format=csv,noheader"])
        .output();
    
    if output.is_err() {
        log::info!("nvidia-smi not found or no GPU processes");
        return Ok(());
    }
    
    let output = output?;
    if !output.status.success() {
        return Ok(());
    }
    
    let stdout = String::from_utf8_lossy(&output.stdout);
    let mut system = System::new_all();
    system.refresh_all();
    
    for line in stdout.lines() {
        let parts: Vec<&str> = line.split(',').map(|s| s.trim()).collect();
        if parts.len() != 2 {
            continue;
        }
        
        let pid: usize = match parts[1].parse() {
            Ok(p) => p,
            Err(_) => continue,
        };
        
        if let Some(process) = system.process(sysinfo::Pid::from(pid)) {
            log::info!("Terminating GPU process: {} (PID: {})", parts[0], pid);
            process.kill_with(Signal::Term);
        }
    }
    
    Ok(())
}


fn get_logging_file_path() -> std::path::PathBuf {
    std::env::temp_dir().join(format!("{}.log", PKG_NAME))
}

fn init_logging_to_file() -> Result<()> {
    use log4rs::append::rolling_file::policy::compound::{
        roll::delete::DeleteRoller, trigger::size::SizeTrigger, CompoundPolicy,
    };
    let policy = CompoundPolicy::new(
        Box::new(SizeTrigger::new(50 << 20)),
        Box::new(DeleteRoller::new()),
    );

    let logfile = log4rs::append::rolling_file::RollingFileAppender::builder()
        .encoder(Box::new(log4rs::encode::pattern::PatternEncoder::new(
            "{h({d(%Y-%m-%d %H:%M:%S)(local)} - {l}: {m}{n})}",
        )))
        .build(get_logging_file_path(), Box::new(policy))?;

    let config = log4rs::config::Config::builder()
        .appender(log4rs::config::Appender::builder().build("logfile", Box::new(logfile)))
        .build(
            log4rs::config::Root::builder()
                .appender("logfile")
                .build(log::LevelFilter::Trace),
        )?;

    log4rs::init_config(config)?;
    Ok(())
}

fn init(tray_icon: &mut tray_icon::TrayIcon, device: &device::Device) -> Result<ProgramState> {
    log::info!(
        "loading config file {}",
        confy::get_configuration_file_path(PKG_NAME, None)?.display()
    );
    let config: ConfigState = confy::load(PKG_NAME, None).unwrap_or_default();
    let fan_actual = get_fan_rpm(device)?;
    let mut state = ProgramState::new(config.ac_state, fan_actual, config.enforce)?;
    state.ac_power = get_power_state()?;
    state.ac_state = config.ac_state.clone();
    state.battery_state = config.battery_state.clone();
    if state.ac_power == false {
        state.device_state = state.battery_state.clone()
    }
    state.update(tray_icon, state.device_state, device)?;
    Ok(state)
}

#[cfg(target_os = "windows")]
unsafe extern "system" fn keyboard_hook_proc(
    code: i32,
    wparam: windows::Win32::Foundation::WPARAM,
    lparam: windows::Win32::Foundation::LPARAM,
) -> windows::Win32::Foundation::LRESULT {
    use windows::Win32::UI::WindowsAndMessaging::{CallNextHookEx, HHOOK};
    if code >= 0 && (wparam.0 == 0x0100 || wparam.0 == 0x0104) {
        KEY_PRESSED.store(true, Ordering::Relaxed);
    }
    CallNextHookEx(HHOOK::default(), code, wparam, lparam)
}

/// Returns the system-wide "last input" tick (keyboard + mouse) from
/// GetLastInputInfo, or None if it can't be read / on non-Windows. The Mirror
/// refresh polls only when this value *changes* (new input since the last poll),
/// so reads happen while you're actively using the machine -- including moving the
/// trackpad to reach the tray -- but stop the instant you stop touching it. That
/// matters because the keyboard firmware re-brightens the backlight on ANY HID
/// activity (including our reads); gating on real input keeps us from re-poking an
/// idle, dimming keyboard, so it dims off normally. (The trackpad movement that
/// brings you to the tray already woke the backlight, so the hover read is free.)
#[cfg(target_os = "windows")]
fn last_input_tick() -> Option<u32> {
    use windows::Win32::UI::Input::KeyboardAndMouse::{GetLastInputInfo, LASTINPUTINFO};
    unsafe {
        let mut info = LASTINPUTINFO {
            cbSize: std::mem::size_of::<LASTINPUTINFO>() as u32,
            dwTime: 0,
        };
        if GetLastInputInfo(&mut info).as_bool() {
            Some(info.dwTime)
        } else {
            None // can't determine -> caller treats as "always refresh"
        }
    }
}

#[cfg(not(target_os = "windows"))]
fn last_input_tick() -> Option<u32> {
    // No cheap, portable idle signal here; returning None makes the caller fall
    // back to a plain timed refresh (the original always-refresh behavior).
    None
}

/// Window procedure for the hidden message-only window that receives power
/// notifications. Updates DISPLAY_ON from GUID_CONSOLE_DISPLAY_STATE events.
#[cfg(target_os = "windows")]
unsafe extern "system" fn power_wnd_proc(
    hwnd: windows::Win32::Foundation::HWND,
    msg: u32,
    wparam: windows::Win32::Foundation::WPARAM,
    lparam: windows::Win32::Foundation::LPARAM,
) -> windows::Win32::Foundation::LRESULT {
    use windows::Win32::System::Power::POWERBROADCAST_SETTING;
    use windows::Win32::System::SystemServices::GUID_CONSOLE_DISPLAY_STATE;
    use windows::Win32::UI::WindowsAndMessaging::{DefWindowProcW, PBT_POWERSETTINGCHANGE, WM_POWERBROADCAST};

    if msg == WM_POWERBROADCAST && wparam.0 as u32 == PBT_POWERSETTINGCHANGE {
        let setting = &*(lparam.0 as *const POWERBROADCAST_SETTING);
        if setting.PowerSetting == GUID_CONSOLE_DISPLAY_STATE {
            // Data[0]: 0 = off, 1 = on, 2 = dimmed. Treat dimmed as on.
            let on = setting.Data[0] != 0;
            DISPLAY_ON.store(on, Ordering::Relaxed);
            log::info!("console display state: {}", if on { "on" } else { "off" });
        }
        return windows::Win32::Foundation::LRESULT(1);
    }
    DefWindowProcW(hwnd, msg, wparam, lparam)
}

/// Spawn a background thread that owns a hidden message-only window, registers
/// for console-display-state power notifications, and pumps messages. This is the
/// event-driven (zero-poll) source of truth for DISPLAY_ON. Errors are logged and
/// the thread exits, leaving DISPLAY_ON at its fail-open default of true.
#[cfg(target_os = "windows")]
fn spawn_display_state_monitor() {
    std::thread::spawn(|| unsafe {
        use windows::core::w;
        use windows::Win32::Foundation::{HINSTANCE, HWND};
        use windows::Win32::System::LibraryLoader::GetModuleHandleW;
        use windows::Win32::System::Power::RegisterPowerSettingNotification;
        use windows::Win32::System::SystemServices::GUID_CONSOLE_DISPLAY_STATE;
        use windows::Win32::UI::WindowsAndMessaging::{
            CreateWindowExW, DispatchMessageW, GetMessageW, RegisterClassW, TranslateMessage,
            DEVICE_NOTIFY_WINDOW_HANDLE, HWND_MESSAGE, MSG, WINDOW_EX_STYLE, WINDOW_STYLE, WNDCLASSW,
        };

        let hinstance: HINSTANCE = match GetModuleHandleW(None) {
            Ok(h) => h.into(),
            Err(e) => {
                log::warn!("display monitor: GetModuleHandleW failed: {e:?}");
                return;
            }
        };
        let class_name = w!("razer_tray_power_window");
        let wc = WNDCLASSW {
            lpfnWndProc: Some(power_wnd_proc),
            hInstance: hinstance,
            lpszClassName: class_name,
            ..Default::default()
        };
        RegisterClassW(&wc);

        let hwnd: HWND = CreateWindowExW(
            WINDOW_EX_STYLE(0),
            class_name,
            w!("razer-tray power"),
            WINDOW_STYLE(0),
            0,
            0,
            0,
            0,
            HWND_MESSAGE,
            None,
            hinstance,
            None,
        );
        if hwnd.0 == 0 {
            log::warn!("display monitor: CreateWindowExW returned null");
            return;
        }

        if let Err(e) = RegisterPowerSettingNotification(
            windows::Win32::Foundation::HANDLE(hwnd.0),
            &GUID_CONSOLE_DISPLAY_STATE,
            DEVICE_NOTIFY_WINDOW_HANDLE,
        ) {
            log::warn!("display monitor: RegisterPowerSettingNotification failed: {e:?}");
            return;
        }
        log::info!("display-state monitor running");

        let mut msg = MSG::default();
        while GetMessageW(&mut msg, hwnd, 0, 0).as_bool() {
            let _ = TranslateMessage(&msg);
            DispatchMessageW(&msg);
        }
    });
}

#[cfg(target_os = "windows")]
fn efficiency_mode() {
    unsafe {
        let handle: HANDLE = GetCurrentProcess();

        let _ = SetPriorityClass(handle, IDLE_PRIORITY_CLASS);

        let power_throttling = PROCESS_POWER_THROTTLING_STATE {
            Version: PROCESS_POWER_THROTTLING_CURRENT_VERSION,
            ControlMask: PROCESS_POWER_THROTTLING_EXECUTION_SPEED,
            StateMask: PROCESS_POWER_THROTTLING_EXECUTION_SPEED,
        };
        let _ = SetProcessInformation(
            handle,
            ProcessPowerThrottling,
            &power_throttling as *const _ as *mut _,
            std::mem::size_of::<PROCESS_POWER_THROTTLING_STATE>() as u32,
        );
    }
}

fn main() -> Result<()> {
    #[cfg(target_os = "linux")]
    {
        // Initialize GTK for tray icon on Linux
        gtk::init().map_err(|_| anyhow::anyhow!("Failed to initialize GTK"))?;
    }
    
    #[cfg(target_os = "windows")]
    efficiency_mode();

    // Start the display-state monitor (event-driven; gates always-on backlight).
    #[cfg(target_os = "windows")]
    spawn_display_state_monitor();

    // Create a named mutex (unique string for your app)
    let instance = SingleInstance::new("razer-tray").unwrap();
    if !instance.is_single() {
        println!("Another instance is already running. Exiting.");
        return Ok(());
    }

    init_logging_to_file()?;
    log::info!("{0} starting {1} {0}", "==".repeat(20), PKG_NAME);

    let device = match device::Device::detect() {
        Ok(d) => {
            log::info!(
                "detected device: {} (0x{:04X})",
                d.info().name,
                d.info().pid
            );
            d
        }
        Err(e) => {
            log::error!("{:?}", e);
            native_dialog::MessageDialog::new()
                .set_type(native_dialog::MessageType::Error)
                .set_text(format!("{:?}", e).as_str())
                .show_alert()?;
            return Err(e);
        }
    };

    let mut tray_icon = TrayIconBuilder::new().build()?;

    let mut state: ProgramState = init(&mut tray_icon, &device)?;

    let menu_channel = MenuEvent::receiver();
    let tray_channel = TrayIconEvent::receiver();
    let event_loop = EventLoopBuilder::new().build();

    let mut last_device_state_check_timestamp = std::time::Instant::now();
    // The last-input tick recorded at the previous Mirror poll. We only re-poll when
    // this changes (new input), so polling follows your activity and stops when you
    // stop touching the machine. None = not yet polled / non-Windows (always refresh).
    let mut last_polled_input_tick: Option<u32> = None;
    // Tracks the last-seen console display power state, to drive the always-on
    // backlight gate only on transitions.
    #[cfg(target_os = "windows")]
    let mut last_display_on = true;
    // Tracks wall-clock between event-loop ticks. The loop ticks ~every second;
    // a gap far larger than that means the process was suspended (system sleep),
    // which we use as a cheap, API-free "resumed from sleep" signal.
    let mut last_tick_timestamp = std::time::Instant::now();

    // loop through the default start up sequence to initialise the device.
    for element in device.info().init_cmds {
        command::send_command(&device, *element, &[0,0,0,0])?;
    }

    // Install a low-level keyboard hook used to refresh the tooltip on keypress.
    // If it fails we log and carry on -- it's a nice-to-have, not load-bearing.
    // We hold the handle for the life of the process: tao's event loop never
    // returns, so there's no reachable place to call UnhookWindowsHookEx, and
    // Windows reclaims low-level hooks automatically when the process exits.
    #[cfg(target_os = "windows")]
    let _keyboard_hook = unsafe {
        use windows::Win32::UI::WindowsAndMessaging::{SetWindowsHookExW, WH_KEYBOARD_LL};
        match SetWindowsHookExW(WH_KEYBOARD_LL, Some(keyboard_hook_proc), None, 0) {
            Ok(hook) => Some(hook),
            Err(e) => {
                log::warn!("Failed to install keyboard hook ({e:?}); keypress tooltip refresh disabled");
                None
            }
        }
    };

    event_loop.run(move |_, _, control_flow| {
        let now = std::time::Instant::now();
        let since_last_tick = now.duration_since(last_tick_timestamp);
        last_tick_timestamp = now;
        *control_flow = ControlFlow::WaitUntil(now + std::time::Duration::from_millis(1000));

        if let Err(e) = (|| -> Result<()> {
            if let Ok(event) = menu_channel.try_recv() {
                log::info!("Menu Event {:?}", event.id);
                if event.id == MenuId("dgpu_terminate_proc".to_string()) {
                    log::info!("match event id");
                    gpu_taskkill()?;
                } else if event.id == MenuId("toggle_enforce".to_string()) {
                    state.enforce = !state.enforce;
                    if let Err(e) = confy::store(
                        PKG_NAME,
                        None,
                        &ConfigState {
                            ac_state: state.ac_state,
                            battery_state: state.battery_state,
                            enforce: state.enforce,
                        },
                    ) {
                        log::warn!("Failed to persist enforce flag: {:?}", e);
                    }
                    // Rebuild the menu so the checkmark reflects the new state.
                    let (m, h) =
                        ProgramState::create_menu_and_handlers(&state.device_state, state.enforce)?;
                    state.menu = m;
                    state.event_handlers = h;
                    tray_icon.set_menu(Some(Box::new(state.menu.clone())));
                    log::info!("enforce toggled to {}", state.enforce);
                } else if event.id == MenuId("toggle_autostart".to_string()) {
                    #[cfg(target_os = "windows")]
                    {
                        if let Err(e) = set_autostart(!autostart_enabled()) {
                            log::warn!("Failed to toggle autostart: {:?}", e);
                        }
                        // Rebuild the menu so the checkmark reflects the new state.
                        let (m, h) = ProgramState::create_menu_and_handlers(&state.device_state, state.enforce)?;
                        state.menu = m;
                        state.event_handlers = h;
                        tray_icon.set_menu(Some(Box::new(state.menu.clone())));
                    }
                } else {
                    let new_device_state = state.handle_event(event.id.as_ref())?;
                    log::info!("new_device_state 1 {:?}", new_device_state);
                    state.update(&mut tray_icon, new_device_state, &device)?;
                }
            }

            if matches!(tray_channel.try_recv(), Ok(event) if event.click_type == tray_icon::ClickType::Left) {
                let new_device_state = state.get_next_perf_mode();
                log::info!("new_device_state 2 {:?}", new_device_state);
                state.update(&mut tray_icon, new_device_state, &device)?;
            }

            state.ac_power = get_power_state()?;
            if state.ac_power && state.device_state != state.ac_state {
                let new_device_state = state.ac_state.clone();
                log::info!("new_device_state 3 {:?}", new_device_state);
                state.update(&mut tray_icon, new_device_state, &device)?;
            } else if state.ac_power == false && state.device_state != state.battery_state {
                let new_device_state = state.battery_state.clone();
                log::info!("new_device_state 3 {:?}", new_device_state);
                state.update(&mut tray_icon, new_device_state, &device)?;
            }

            // Resume-from-sleep reassert. The event loop is frozen while the
            // machine sleeps, so a tick gap far larger than our ~1s cadence means
            // we just woke. Synapse (and sometimes firmware) re-assert their own
            // state on resume, so when Enforce is on we immediately re-assert ours
            // rather than waiting for the next 10s enforce poll. Brightness is left
            // alone (enforce_to omits it) so a pre-sleep Fn setting isn't clobbered.
            if state.enforce && since_last_tick > std::time::Duration::from_secs(30) {
                log::info!("resume detected (tick gap {:?}); re-asserting enforced state", since_last_tick);
                if let Err(e) = state.device_state.enforce_to(&device) {
                    log::warn!("enforce: resume re-assert failed: {:?}", e);
                }
            }

            // Always-on display gate. This unit's firmware keeps the keyboard lit
            // literally always, so we drop the firmware always-on flag when the
            // console display powers off (screen-off timeout / display sleep) and
            // restore it when the display comes back. Driven by the display-state
            // monitor (GUID_CONSOLE_DISPLAY_STATE) -> acts only on transitions, no
            // polling. Only touches the flag when the user has always-on enabled;
            // device_state (the menu's intent) is left unchanged.
            #[cfg(target_os = "windows")]
            {
                let display_on = DISPLAY_ON.load(Ordering::Relaxed);
                if display_on != last_display_on {
                    last_display_on = display_on;
                    if state.device_state.lights_mode.always_on == LightsAlwaysOn::Enable {
                        let effective = if display_on {
                            LightsAlwaysOn::Enable
                        } else {
                            LightsAlwaysOn::Disable
                        };
                        match command::set_lights_always_on(&device, effective) {
                            Ok(()) => log::info!(
                                "display {} -> always-on {:?}",
                                if display_on { "on" } else { "off" },
                                effective
                            ),
                            Err(e) => log::warn!("display-gate: set_lights_always_on failed: {:?}", e),
                        }
                    }
                }
            }

            // Mirror: refresh the displayed device state (tooltip/icon) so it's fresh
            // when you look at the tray. Display-only -- it never re-applies state, so
            // it can't fight external changes or touch the saved AC/battery profiles.
            // A failed read is swallowed: keep the last good values and try again rather
            // than tearing down and re-initing the device.
            //
            // We poll at most every 2s AND only when there's been new input since the
            // last poll (`last_input_tick` changed). So reads track your activity --
            // including the trackpad movement that brings you to the tray, which both
            // makes the tooltip fresh on hover and means the read can't visibly disturb
            // the backlight (your input already woke it). The moment you stop touching
            // the machine, polling stops and the keyboard dims/off normally; we never
            // re-poke an idle keyboard. (None from last_input_tick -> always refresh,
            // the non-Windows fallback.)
            let input_tick = last_input_tick();
            let new_input = match (input_tick, last_polled_input_tick) {
                (Some(cur), Some(prev)) => cur != prev,
                _ => true,
            };
            if new_input
                && now > last_device_state_check_timestamp + std::time::Duration::from_secs(2)
            {
                last_device_state_check_timestamp = now;
                last_polled_input_tick = input_tick;
                if let Ok(observed) = DeviceState::read(&device) {
                    state.observed = observed;

                    // Adopt an externally-made keyboard-brightness change (e.g. the
                    // hardware Fn brightness keys) into the app's own state. We also
                    // write it into the active AC/battery profile and persist it, so
                    // (a) the menu checkmark reflects it, (b) it survives an AC/battery
                    // switch, and (c) the reconciliation step above sees device_state
                    // == the active profile and does NOT re-apply -- no tug-of-war.
                    let observed_brightness = state.observed.lights_mode.keyboard_brightness;
                    if observed_brightness != state.device_state.lights_mode.keyboard_brightness {
                        state.device_state.lights_mode.keyboard_brightness = observed_brightness;
                        if state.ac_power {
                            state.ac_state.lights_mode.keyboard_brightness = observed_brightness;
                        } else {
                            state.battery_state.lights_mode.keyboard_brightness = observed_brightness;
                        }
                        if let Err(e) = confy::store(
                            PKG_NAME,
                            None,
                            &ConfigState {
                                ac_state: state.ac_state,
                                battery_state: state.battery_state,
                                enforce: state.enforce,
                            },
                        ) {
                            log::warn!("failed to persist adopted brightness: {:?}", e);
                        }
                        if let Ok((menu, handlers)) =
                            ProgramState::create_menu_and_handlers(&state.device_state, state.enforce)
                        {
                            state.menu = menu;
                            state.event_handlers = handlers;
                            tray_icon.set_menu(Some(Box::new(state.menu.clone())));
                        }
                    }

                    // Enforce (opt-in): if the real device drifted from our intended
                    // state on a field we own -- perf mode, fan, logo, battery care --
                    // re-assert it. This is how razer-tray wins a tug-of-war with
                    // Synapse. Brightness is deliberately excluded (it stays on the
                    // adopt path above so the Fn keys keep working). It rides the same
                    // input-gated read above, so it adds no idle cost and reasserts
                    // whenever you're active (incl. right after you return to the
                    // machine); the resume-from-sleep reassert covers the wake case.
                    if state.enforce {
                        let drifted = {
                            let o = &state.observed;
                            let d = &state.device_state;
                            o.perf_mode != d.perf_mode
                                || o.fan_speed != d.fan_speed
                                || o.lights_mode.logo_mode != d.lights_mode.logo_mode
                                || o.battery_care != d.battery_care
                        };
                        if drifted {
                            log::info!("enforce: device drifted; re-asserting intended state");
                            if let Err(e) = state.device_state.enforce_to(&device) {
                                log::warn!("enforce: re-assert failed: {:?}", e);
                            } else {
                                // Device now matches intent on the enforced fields;
                                // reflect that in `observed` (preserving the real
                                // brightness) so the tooltip/icon don't show the
                                // stale drift until the next read.
                                let brightness = state.observed.lights_mode.keyboard_brightness;
                                state.observed = state.device_state;
                                state.observed.lights_mode.keyboard_brightness = brightness;
                            }
                        }
                    }
                }
                if let Ok(fan) = get_fan_rpm(&device) {
                    state.fan_actual = fan;
                }
                let _ = tray_icon.set_icon(Some(state.icon()));
                if let Ok(tooltip) = state.tooltip() {
                    let _ = tray_icon.set_tooltip(Some(tooltip));
                }
            }

            // Always-on backlight is handled by the firmware flag set in apply()
            // (command::set_lights_always_on), so there is no software heartbeat:
            // the keyboard stays lit until the display/system powers it down, with
            // zero polling. (A previous iteration polled every 5s gated on idle,
            // which incorrectly let the backlight time out while merely idle.)

            // Update fan RPM and tooltip whenever a key is pressed, since keypresses
            // already turn on the backlight; piggybacking here adds no idle-time cost.
            #[cfg(target_os = "windows")]
            if KEY_PRESSED.swap(false, Ordering::Relaxed) {
                if let Ok(fan) = get_fan_rpm(&device) {
                    state.fan_actual = fan;
                    if let Ok(tooltip) = state.tooltip() {
                        let _ = tray_icon.set_tooltip(Some(tooltip));
                    }
                }
            }

            Ok(())
        })() {
            loop {
                log::error!("trying to recover from: {:?}", e);
                match init(&mut tray_icon, &device) {
                    Ok(new_state) => {
                        state = new_state;
                        break;
                    },
                    Err(e) => {
                        log::error!("failed to recover: {:?}", e);
                        // Sleep between attempts. We're inside this inner `loop`, so we
                        // never return to the event loop until init() succeeds -- which
                        // means `control_flow` has no effect here. Without a sleep a
                        // persistent failure (e.g. the device unplugged) would busy-spin
                        // this thread, pegging a core and spamming HID reads + the log.
                        std::thread::sleep(std::time::Duration::from_millis(1000));
                    }
                }
            }
        }
    })
}

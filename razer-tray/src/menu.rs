//! Builds the tray context menu and the event-id → target-state map that drives it.
//! Pure construction: given a `DeviceState` + the enforce flag it returns a fresh
//! `Menu` and the `HashMap` the event loop looks events up in. No device I/O here.

use anyhow::Result;
use std::collections::HashMap;
use strum::IntoEnumIterator;

use librazer::types::{BatteryCare, CpuBoost, GpuBoost, LightsAlwaysOn, LogoMode};
use tray_icon::menu::{
    CheckMenuItem, IsMenuItem, Menu, MenuItem, PredefinedMenuItem, Submenu,
};

use crate::state::{percent_to_brightness, DeviceState, DeviceStateDelta, FanSpeed, LightsMode, PerfMode};

/// Build the full tray menu and its event-handler map. The menu reflects `dstate`
/// (current intent) via checkmarks; `enforce` drives the Windows-only Enforce toggle.
pub fn build(
    dstate: &DeviceState,
    enforce: bool,
) -> Result<(Menu, HashMap<String, DeviceState>)> {
    let mut event_handlers = std::collections::HashMap::new();
    let menu = Menu::new();

    // perf
    let perf_modes = Submenu::new("Performance", true);
    // The simple (non-Custom) modes are uniform: id == Debug name, enabled when
    // not current, checked when current. Custom is built separately below.
    for (mode, label) in [
        (PerfMode::Battery, "Battery"),
        (PerfMode::Silent, "Silent"),
        (PerfMode::Balanced, "Balanced"),
        (PerfMode::Performance, "Performance"),
        (PerfMode::Hyperboost, "Hyperboost"),
    ] {
        let id = format!("{:?}", mode);
        perf_modes.append(&CheckMenuItem::with_id(
            id.clone(),
            label,
            dstate.perf_mode != mode,
            dstate.perf_mode == mode,
            None,
        ))?;
        event_handlers.insert(
            id,
            DeviceState {
                perf_mode: mode,
                ..*dstate
            },
        );
    }

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
        &modes.iter().map(|i| i as &dyn IsMenuItem).collect::<Vec<_>>(),
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
            CheckMenuItem::with_id(id, label, true, dstate.battery_care == *mode, None)
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
            crate::platform::autostart_enabled(),
            None,
        ))?;
    }

    // gpu task killer
    menu.append(&PredefinedMenuItem::separator())?;
    let terminate_item =
        MenuItem::with_id("dgpu_terminate_proc", "Terminate dGPU Processes", true, None);
    menu.append(&terminate_item)?;
    // footer
    menu.append(&PredefinedMenuItem::separator())?;
    menu.append(&PredefinedMenuItem::about(None, Some(about())))?;
    menu.append(&PredefinedMenuItem::quit(None))?;

    Ok((menu, event_handlers))
}

fn about() -> tray_icon::menu::AboutMetadata {
    tray_icon::menu::AboutMetadata {
        name: Some(crate::PKG_NAME.into()),
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
            crate::get_logging_file_path().display()
        )),
        comments: Some(env!("CARGO_PKG_DESCRIPTION").into()),
        ..Default::default()
    }
}

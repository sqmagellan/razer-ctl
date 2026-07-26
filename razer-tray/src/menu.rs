//! Builds the tray context menu and the event-id → target-state map that drives it.
//! Pure construction: given a `DeviceState` + the enforce flag it returns a fresh
//! `Menu` and the `HashMap` the event loop looks events up in. No device I/O here.

use anyhow::Result;
use std::collections::HashMap;
use strum::IntoEnumIterator;

use librazer::types::{BatteryCare, CpuBoost, GpuBoost, KeyboardEffect, LightsAlwaysOn, LogoMode};
use tray_icon::menu::{CheckMenuItem, IsMenuItem, Menu, MenuItem, PredefinedMenuItem, Submenu};

use crate::state::{
    percent_to_brightness, DeviceState, DeviceStateDelta, FanSpeed, LightsMode, PerfMode,
    FAN_RPM_STEP,
};

/// Build the full tray menu and its event-handler map. The menu reflects `dstate`
/// (current intent) via checkmarks; `enforce` drives the Windows-only Enforce toggle.
//
// `enforce` is read only inside the `cfg(target_os = "windows")` block below, so off
// Windows it is genuinely unused. The parameter stays in the signature regardless --
// callers pass it unconditionally, and making the signature itself platform-dependent
// would push cfg noise into every call site for no gain.
#[cfg_attr(not(target_os = "windows"), allow(unused_variables))]
pub fn build(
    dstate: &DeviceState,
    enforce: bool,
    fan_rpm_range: (u16, u16),
) -> Result<(Menu, HashMap<String, DeviceState>)> {
    let mut event_handlers = std::collections::HashMap::new();
    let menu = Menu::new();

    // perf
    let perf_modes = Submenu::new("Performance mode", true);
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
                // Max fan is Custom-only, so switching to a non-Custom mode clears it.
                max_fan: false,
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

    // Disabled header items so the two boost groups are labelled -- without them the
    // submenu is just "Low/Medium/High/Boost/Undervolt / --- / Low/Medium/High" and you
    // can't tell which axis is which. (HW-verified 2026-07-09: all five CPU levels and
    // all three GPU levels apply + read back on PID 0x029f.)
    let cpu_header = MenuItem::new("CPU boost", false, None);
    let gpu_header = MenuItem::new("GPU boost", false, None);

    // Max fan speed: Custom-only (the EC rejects the command in other modes), so it lives
    // in the Custom submenu as a checkbox. Toggling it from a non-Custom mode promotes to
    // Custom, seeding the documented boost defaults exactly like a CPU/GPU boost pick does.
    let max_fan_now = matches!(dstate.perf_mode, PerfMode::Custom(..)) && dstate.max_fan;
    let max_fan_target = {
        let perf_mode = if matches!(dstate.perf_mode, PerfMode::Custom(..)) {
            dstate.perf_mode
        } else {
            PerfMode::Custom(CpuBoost::Boost, GpuBoost::High)
        };
        DeviceState {
            perf_mode,
            max_fan: !max_fan_now,
            ..*dstate
        }
    };
    event_handlers.insert("max_fan".to_string(), max_fan_target);
    let max_fan_item = CheckMenuItem::with_id("max_fan", "Max fan speed", true, max_fan_now, None);
    let separator2 = PredefinedMenuItem::separator();

    let custom_items: Vec<&dyn IsMenuItem> = std::iter::once(&cpu_header as &dyn IsMenuItem)
        .chain(cpu_boosts.iter().map(|i| i as &dyn IsMenuItem))
        .chain([&separator as &dyn IsMenuItem])
        .chain(std::iter::once(&gpu_header as &dyn IsMenuItem))
        .chain(gpu_boosts.iter().map(|i| i as &dyn IsMenuItem))
        .chain([&separator2 as &dyn IsMenuItem])
        .chain(std::iter::once(&max_fan_item as &dyn IsMenuItem))
        .collect();

    perf_modes.append(&Submenu::with_items("Custom", true, &custom_items)?)?;

    menu.append(&perf_modes)?;

    // Fan Speed
    menu.append(&PredefinedMenuItem::separator())?;
    let (fan_min, fan_max) = fan_rpm_range;
    // Manual presets spanning this chassis's usable range, always including both endpoints
    // (the step may not land on `fan_max` exactly, so append it if missing).
    let mut fan_rpms: Vec<u16> = (fan_min..=fan_max).step_by(FAN_RPM_STEP as usize).collect();
    if fan_rpms.last() != Some(&fan_max) {
        fan_rpms.push(fan_max);
    }
    let fan_speeds: Vec<CheckMenuItem> = [CheckMenuItem::with_id(
        "fan_speeds:auto",
        "Auto",
        dstate.fan_speed != FanSpeed::Auto,
        dstate.fan_speed == FanSpeed::Auto,
        None,
    )]
    .into_iter()
    .chain(fan_rpms.into_iter().map(|rpm| {
        let event_id = format!("fan_speeds:{}", rpm);
        event_handlers.insert(
            event_id.clone(),
            DeviceState {
                fan_speed: FanSpeed::Manual(rpm),
                ..*dstate
            },
        );
        // Label the extremes so it's clear these are the chassis's real limits
        // (below min the EC floors the fan, above max it clamps). Range is per-device
        // (Descriptor::fan_rpm_range), so this stays honest on every supported chassis.
        let label = if rpm == fan_min {
            format!("{} RPM (min)", rpm)
        } else if rpm == fan_max {
            format!("{} RPM (max)", rpm)
        } else {
            format!("{} RPM", rpm)
        };
        CheckMenuItem::with_id(
            event_id,
            label,
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
        "Fan",
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
        "Logo lighting",
        true,
        &modes
            .iter()
            .map(|i| i as &dyn IsMenuItem)
            .collect::<Vec<_>>(),
    )?)?;

    // Keyboard lighting (RGB / Chroma). Write-only intent -- Chroma has no getter on this
    // device -- so a pick is stored + applied, never read back or reconciled (unlike perf/
    // fan/logo). Effects-only, no color: an arbitrary color needs Razer driver mode
    // (host-streamed frames = Synapse), which disables the Fn media keys, so we ship only the
    // EC-animated effects (Off/Spectrum/Wave/Breathing). HW-verified Fn-safe on 0x029F (2026-07-10).
    menu.append(&PredefinedMenuItem::separator())?;
    let kbd_effects: Vec<CheckMenuItem> = KeyboardEffect::iter()
        .map(|effect| {
            let event_id = format!("kbd_effect:{:?}", effect);
            event_handlers.insert(
                event_id.clone(),
                DeviceState {
                    lights_mode: LightsMode {
                        keyboard_effect: Some(effect),
                        ..dstate.lights_mode
                    },
                    ..*dstate
                },
            );
            let checked = dstate.lights_mode.keyboard_effect == Some(effect);
            CheckMenuItem::with_id(event_id, format!("{:?}", effect), !checked, checked, None)
        })
        .collect();

    menu.append(&Submenu::with_items(
        "Keyboard lighting",
        true,
        &kbd_effects
            .iter()
            .map(|i| i as &dyn IsMenuItem)
            .collect::<Vec<_>>(),
    )?)?;

    menu.append(&PredefinedMenuItem::separator())?;

    // keyboard always on
    menu.append(&CheckMenuItem::with_id(
        "lights_always_on",
        "Keyboard always on",
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
    let nearest_percent: u8 =
        crate::state::nearest_brightness_percent(dstate.lights_mode.keyboard_brightness);

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
        "Keyboard brightness",
        true,
        &brightness_modes
            .iter()
            .map(|i| i as &dyn IsMenuItem)
            .collect::<Vec<_>>(),
    )?)?;

    // battery care submenu
    menu.append(&PredefinedMenuItem::separator())?;

    // Charge-limit presets. The EC accepts every whole percent 50..=100 (HW-verified), so
    // this list is a UI convenience, not the limit of what's possible -- the CLI
    // (`battery-care set <50-100>`) reaches any value, and a config hand-set to e.g. 88
    // is honoured and shown as the current value below. Presets stop at 5% steps up to 80
    // (the healthy-longevity band) then add 90/95, which the old 8-variant enum could not
    // express at all.
    let battery_care_percents = [50u8, 60, 70, 75, 80, 85, 90, 95];
    let mut battery_care_options: Vec<(BatteryCare, String, String)> = battery_care_percents
        .iter()
        .map(|p| {
            (
                BatteryCare::from_percent(*p).expect("preset percents are in range"),
                format!("{p}%"),
                format!("battery_care_{p}"),
            )
        })
        .collect();
    battery_care_options.push((
        BatteryCare::DISABLE,
        "Off (100%)".to_string(),
        "battery_care_disable".to_string(),
    ));

    // If the active limit isn't one of the presets (set via CLI or hand-edited config),
    // surface it so the menu still shows exactly one checkmark and never misreports the
    // device. Without this a custom 88% would display as "no limit set".
    if !battery_care_options
        .iter()
        .any(|(mode, _, _)| *mode == dstate.battery_care)
    {
        let pct = dstate.battery_care.to_percent();
        battery_care_options.push((
            dstate.battery_care,
            format!("{pct}% (custom)"),
            format!("battery_care_{pct}"),
        ));
        battery_care_options.sort_by_key(|(mode, _, _)| mode.to_percent());
    }

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
        "Charge limit",
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
            "Keep settings enforced (override Synapse)",
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
    let terminate_item = MenuItem::with_id("dgpu_terminate_proc", "Close GPU apps", true, None);
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

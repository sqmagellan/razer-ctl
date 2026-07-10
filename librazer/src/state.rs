//! Data model: the device-state the tray reads/writes, the persisted config, and the
//! pure helpers around them. Deliberately free of menu/OS code so it stays easy to
//! read and unit-test (see the `tests` module at the bottom).

use anyhow::Result;
use serde::{Deserialize, Serialize};

use crate::types::{BatteryCare, CpuBoost, FanMode, GpuBoost, LightsAlwaysOn, LogoMode, MaxFanSpeedMode};
use crate::command;
use crate::transport::HidTransport;

#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub enum FanSpeed {
    Auto,
    Manual(u16),
}

/// Manual-fan RPM step for building the UI presets (granularity; model-independent).
pub const FAN_RPM_STEP: u16 = 400;

/// Widest manual-fan bounds across all known Blade models, for callers that must fix a
/// static range *before* the device is detected (e.g. the CLI arg parser). The per-device
/// range lives on the [`crate::descriptor::Descriptor`] (`fan_rpm_range`) and is narrower;
/// the EC clamps anything outside its real range regardless.
pub const FAN_RPM_MIN_ANY: u16 = 2200;
pub const FAN_RPM_MAX_ANY: u16 = 5300;

#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub enum PerfMode {
    Battery,
    Silent,
    Balanced,
    Performance,
    Hyperboost,
    Custom(CpuBoost, GpuBoost),
}

#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct LightsMode {
    pub logo_mode: LogoMode,
    pub keyboard_brightness: u8,
    pub always_on: LightsAlwaysOn,
}

#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct FanRpm {
    pub fan1: u16,
    pub fan2: u16,
}

#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct DeviceState {
    pub perf_mode: PerfMode,
    pub lights_mode: LightsMode,
    pub battery_care: BatteryCare,
    pub fan_speed: FanSpeed,
    /// Max Fan Speed Mode: runs both fans flat-out regardless of the Auto/Manual RPM
    /// setting. Custom perf mode ONLY -- the EC rejects the command in other modes -- so
    /// it's always false outside Custom. `#[serde(default)]` keeps configs written before
    /// this field existed loadable (they deserialize to false).
    #[serde(default)]
    pub max_fan: bool,
}

/// Map a 0..=100 percentage to the device's 0..255 keyboard-brightness scale,
/// rounded. Used by the brightness submenu so its 10% steps span the full
/// hardware range (0->0, 10->26, 50->128, 100->255).
pub fn percent_to_brightness(percent: u8) -> u8 {
    ((percent as u16 * 255 + 50) / 100) as u8
}

/// Inverse of `percent_to_brightness`: map the device's 0..=255 brightness back to a
/// 0..=100 percentage, rounded. Used by the tooltip so it reports the same unit the
/// brightness menu offers rather than the raw register value (0..=255).
pub fn brightness_to_percent(brightness: u8) -> u8 {
    ((brightness as u16 * 100 + 127) / 255) as u8
}

impl DeviceState {
    /// Read the device's *actual* current state. Used by the Mirror refresh to keep
    /// the tray display honest. This is read-only: callers must treat the result as
    /// something to *display*, never to re-apply (re-applying would fight external
    /// changes such as Fn-key brightness and could overwrite saved AC/battery
    /// profiles). Returns Err on any transient HID failure; callers should swallow
    /// that and keep the last known-good values rather than propagating it.
    pub fn read(device: &impl HidTransport) -> Result<Self> {
        let perf_mode = match command::get_perf_mode(device)? {
            (crate::types::PerfMode::Battery, _) => PerfMode::Battery,
            (crate::types::PerfMode::Silent, _) => PerfMode::Silent,
            (crate::types::PerfMode::Balanced, _) => PerfMode::Balanced,
            (crate::types::PerfMode::Performance, _) => PerfMode::Performance,
            (crate::types::PerfMode::Hyperboost, _) => PerfMode::Hyperboost,
            (crate::types::PerfMode::Custom, _) => {
                let cpu_boost = command::get_cpu_boost(device)?;
                let gpu_boost = command::get_gpu_boost(device)?;
                PerfMode::Custom(cpu_boost, gpu_boost)
            }
        };

        let fan_speed = match command::get_perf_mode(device)? {
            (_, FanMode::Auto) => FanSpeed::Auto,
            (_, FanMode::Manual) => {
                let rpm = command::get_fan_rpm(device, crate::types::FanZone::Zone1)?;
                FanSpeed::Manual(rpm)
            }
        };
        let lights_mode = LightsMode {
            logo_mode: command::get_logo_mode(device)?,
            keyboard_brightness: command::get_keyboard_brightness(device)?,
            always_on: command::get_lights_always_on(device)?,
        };

        let battery_care = command::get_battery_care(device)?;

        // Max fan is a Custom-only toggle; only read it there -- the getter is meaningless
        // outside Custom, and skipping it spares a HID read on every (frequent) poll.
        let max_fan = matches!(perf_mode, PerfMode::Custom(..))
            && matches!(
                command::get_max_fan_speed_mode(device)?,
                MaxFanSpeedMode::Enable
            );

        Ok(Self {
            perf_mode,
            lights_mode,
            battery_care,
            fan_speed,
            max_fan,
        })
    }

    /// Perf mode + fan + logo -- the settings shared by `apply()` (full write) and
    /// `enforce_to()` (the Synapse tug-of-war reassert). Kept in one place so the two
    /// can't drift. Fan failures are logged but non-fatal (manual RPM can be rejected
    /// depending on mode); a logo/perf failure propagates.
    fn apply_perf_fan_logo(&self, device: &impl HidTransport) -> Result<()> {
        match self.perf_mode {
            PerfMode::Battery => command::set_perf_mode(device, crate::types::PerfMode::Battery),
            PerfMode::Silent => command::set_perf_mode(device, crate::types::PerfMode::Silent),
            PerfMode::Balanced => command::set_perf_mode(device, crate::types::PerfMode::Balanced),
            PerfMode::Performance => command::set_perf_mode(device, crate::types::PerfMode::Performance),
            PerfMode::Hyperboost => command::set_perf_mode(device, crate::types::PerfMode::Hyperboost),
            PerfMode::Custom(cpu_boost, gpu_boost) => {
                command::set_perf_mode(device, crate::types::PerfMode::Custom)?;
                command::set_cpu_boost(device, cpu_boost)?;
                command::set_gpu_boost(device, gpu_boost)
            }
        }?;

        // Max fan speed is Custom-only (the EC rejects the command otherwise). Match intent
        // while in Custom; leaving Custom clears it in the EC. Non-fatal, like the fan write.
        if matches!(self.perf_mode, PerfMode::Custom(..)) {
            let mode = if self.max_fan {
                MaxFanSpeedMode::Enable
            } else {
                MaxFanSpeedMode::Disable
            };
            if let Err(e) = command::set_max_fan_speed_mode(device, mode) {
                log::warn!("max fan command failed: {:?}", e);
            }
        }

        if let Err(e) = match self.fan_speed {
            FanSpeed::Auto => command::set_fan_mode(device, crate::types::FanMode::Auto),
            FanSpeed::Manual(rpm) => command::set_fan_mode(device, crate::types::FanMode::Manual)
                .and_then(|_| command::set_fan_rpm(device, rpm, false)),
        } {
            log::warn!("fan command failed: {:?}", e);
        }

        command::set_logo_mode(device, self.lights_mode.logo_mode)
    }

    pub fn apply(&self, device: &impl HidTransport) -> Result<()> {
        self.apply_perf_fan_logo(device)?;
        // NB: always-on is deliberately NOT applied here. It is the Razer "device mode"
        // command (0x0004); Enable == driver mode, which disables the keyboard's native
        // Fn media keys (brightness/volume). The tray keeps the device in Normal mode and
        // implements "keyboard always-on" as a Normal-mode keep-alive instead.
        command::set_keyboard_brightness(device, self.lights_mode.keyboard_brightness)?;
        command::set_battery_care(device, self.battery_care)
    }

    /// Re-assert the "enforced" subset of settings -- perf mode, fan, logo, and
    /// battery care -- WITHOUT touching keyboard brightness. This is what the opt-in
    /// Enforce mode uses to win a tug-of-war with Synapse. Brightness is excluded so it
    /// stays on the adopt path (Fn keys keep working). It's exactly apply() minus the
    /// brightness write. (always-on isn't a device write at all -- it's a Normal-mode
    /// keep-alive in the tray -- so there's nothing here to exclude for it.)
    pub fn enforce_to(&self, device: &impl HidTransport) -> Result<()> {
        self.apply_perf_fan_logo(device)?;
        command::set_battery_care(device, self.battery_care)
    }

    /// Whether the *enforced* fields -- perf mode, fan, logo, battery care -- of `self`
    /// (typically a just-read `observed` state) differ from `intended`. This is exactly
    /// the subset `enforce_to` writes: keyboard brightness is excluded on purpose (it
    /// rides the Fn-key adopt path), and always-on isn't a device write, so drift in
    /// those must NOT trigger a re-assert. The tray's Enforce loop calls this before
    /// re-asserting against Synapse.
    pub fn enforced_fields_differ(&self, intended: &DeviceState) -> bool {
        self.perf_mode != intended.perf_mode
            || self.fan_speed != intended.fan_speed
            || self.max_fan != intended.max_fan
            || self.lights_mode.logo_mode != intended.lights_mode.logo_mode
            || self.battery_care != intended.battery_care
    }

    fn perf_delta(&self, cpu_boost: Option<CpuBoost>, gpu_boost: Option<GpuBoost>) -> Self {
        DeviceState {
            perf_mode: if let PerfMode::Custom(cb, gb) = self.perf_mode {
                PerfMode::Custom(cpu_boost.unwrap_or(cb), gpu_boost.unwrap_or(gb))
            } else {
                PerfMode::Custom(
                    cpu_boost.unwrap_or(CpuBoost::Boost),
                    gpu_boost.unwrap_or(GpuBoost::High),
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
            fan_speed: FanSpeed::Auto,
            max_fan: false,
        }
    }
}

pub trait DeviceStateDelta<T> {
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

/// One "Action" rule: while `process` is running, force `perf_mode`. This is the
/// config-driven automation layer (cf. Legion Toolkit's Actions). Rules are matched in
/// list order; the first running match wins. There's no in-app editor -- rules are
/// hand-added to the config TOML -- so this stays opt-in (an empty list = no behavior).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct AppProfile {
    /// Executable name to match, case-insensitive, e.g. "cyberpunk2077.exe".
    pub process: String,
    /// Perf mode to switch to while that process runs. Omit to leave perf unchanged.
    #[serde(default)]
    pub perf_mode: Option<PerfMode>,
    /// Fan setting to apply while it runs. Omit to leave the fan unchanged.
    #[serde(default)]
    pub fan_speed: Option<FanSpeed>,
    /// Logo lighting to apply while it runs. Omit to leave the logo unchanged.
    #[serde(default)]
    pub logo_mode: Option<LogoMode>,
}

impl AppProfile {
    /// Overlay this rule's set fields onto `base`, yielding the transient state to apply
    /// while the app runs. Unset (`None`) fields leave `base` untouched, so a rule can
    /// change just the fan, just the perf mode, the logo, or any combination.
    pub fn overlay(&self, base: &DeviceState) -> DeviceState {
        let mut s = *base;
        if let Some(p) = self.perf_mode {
            s.perf_mode = p;
        }
        if let Some(f) = self.fan_speed {
            s.fan_speed = f;
        }
        if let Some(l) = self.logo_mode {
            s.lights_mode.logo_mode = l;
        }
        s
    }
}

fn default_true() -> bool {
    true
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ConfigState {
    pub ac_state: DeviceState,
    pub battery_state: DeviceState,
    // Opt-in "win against Synapse" mode. #[serde(default)] keeps older config
    // files (written before this field existed) loadable -> defaults to false.
    #[serde(default)]
    pub enforce: bool,
    // On wake from sleep, re-assert the intended profile's enforced fields (perf/fan/
    // logo/battery) even when `enforce` is off -- a just-woken EC can drop the perf mode
    // the same way a just-booted one does (see the tray's startup reconcile). Defaults to
    // true; set false to restore the old enforce-only-on-resume behavior.
    #[serde(default = "default_true")]
    pub reassert_on_resume: bool,
    // "Actions": app-triggered profile switches. Empty by default (no behavior).
    #[serde(default)]
    pub app_profiles: Vec<AppProfile>,
}

impl Default for ConfigState {
    fn default() -> Self {
        Self {
            ac_state: DeviceState { ..Default::default() },
            battery_state: DeviceState {
                perf_mode: PerfMode::Battery,
                ..Default::default()
            },
            enforce: false,
            reassert_on_resume: true,
            app_profiles: Vec::new(),
        }
    }
}

/// Index of the first app-profile whose `process` is currently running (case-insensitive
/// exact match on the executable name), or `None` if none match. Pure so it's unit-tested
/// without a live process table: the caller passes the running process names (from sysinfo
/// on the real system). First match wins, so earlier rules take priority.
pub fn matching_app_profile(profiles: &[AppProfile], running: &[String]) -> Option<usize> {
    profiles
        .iter()
        .position(|p| running.iter().any(|r| r.eq_ignore_ascii_case(&p.process)))
}

pub fn get_fan_rpm(device: &impl HidTransport) -> Result<FanRpm> {
    Ok(FanRpm {
        fan1: command::get_fan_actual_rpm(device, crate::types::FanZone::Zone1)?,
        fan2: command::get_fan_actual_rpm(device, crate::types::FanZone::Zone2)?,
    })
}

/// The profile that should be active for the current power source, or `None` if the
/// active state already matches it (so the caller can skip a redundant re-apply).
/// `current` is the live `device_state`; `ac_state`/`battery_state` are the saved
/// per-source profiles. Encodes the tray event loop's AC<->battery switch rule.
pub fn profile_for_power(
    ac_power: bool,
    current: &DeviceState,
    ac_state: &DeviceState,
    battery_state: &DeviceState,
) -> Option<DeviceState> {
    let target = if ac_power { ac_state } else { battery_state };
    (current != target).then_some(*target)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn percent_to_brightness_spans_full_range() {
        // The submenu's 10% steps must map onto the device's full 0..=255 scale.
        assert_eq!(percent_to_brightness(0), 0);
        assert_eq!(percent_to_brightness(100), 255);
        assert_eq!(percent_to_brightness(50), 128); // rounds 127.5 up
        assert_eq!(percent_to_brightness(10), 26); // rounds 25.5 up
    }

    #[test]
    fn percent_to_brightness_is_monotonic() {
        let mut prev = 0u8;
        for p in (0u8..=100).step_by(10) {
            let b = percent_to_brightness(p);
            assert!(b >= prev, "brightness must not decrease as percent rises");
            prev = b;
        }
    }

    #[test]
    fn brightness_percent_round_trips_on_menu_steps() {
        // The tooltip converts raw->percent; the menu converts percent->raw. For the
        // 10% menu steps the round trip must land back on the same percent.
        for p in (0u8..=100).step_by(10) {
            assert_eq!(brightness_to_percent(percent_to_brightness(p)), p);
        }
    }

    #[test]
    fn delta_promotes_non_custom_mode_to_custom() {
        // Picking a CPU/GPU boost from a non-Custom mode should switch into Custom,
        // seeding the *other* axis with the documented default.
        let base = DeviceState {
            perf_mode: PerfMode::Balanced,
            ..Default::default()
        };
        match base.delta(CpuBoost::Low).perf_mode {
            PerfMode::Custom(cpu, gpu) => {
                assert_eq!(cpu, CpuBoost::Low);
                assert_eq!(gpu, GpuBoost::High); // default seeded for the untouched axis
            }
            other => panic!("expected Custom, got {other:?}"),
        }
    }

    #[test]
    fn delta_preserves_the_other_axis_within_custom() {
        let custom = DeviceState {
            perf_mode: PerfMode::Custom(CpuBoost::Medium, GpuBoost::Low),
            ..Default::default()
        };
        // Changing GPU keeps the existing CPU boost.
        match custom.delta(GpuBoost::High).perf_mode {
            PerfMode::Custom(cpu, gpu) => {
                assert_eq!(cpu, CpuBoost::Medium);
                assert_eq!(gpu, GpuBoost::High);
            }
            other => panic!("expected Custom, got {other:?}"),
        }
    }

    #[test]
    fn config_default_battery_profile_is_low_power() {
        // The battery profile should default to the Battery perf mode; AC stays at
        // the DeviceState default (Performance).
        let cfg = ConfigState::default();
        assert_eq!(cfg.battery_state.perf_mode, PerfMode::Battery);
        assert_eq!(cfg.ac_state.perf_mode, PerfMode::Performance);
        assert!(!cfg.enforce);
        // Actions defaults: resume re-assert on, no app rules.
        assert!(cfg.reassert_on_resume);
        assert!(cfg.app_profiles.is_empty());
    }

    #[test]
    fn matching_app_profile_first_match_wins_case_insensitive() {
        let profiles = vec![
            AppProfile { process: "game.exe".into(), perf_mode: Some(PerfMode::Hyperboost), fan_speed: None, logo_mode: None },
            AppProfile { process: "editor.exe".into(), perf_mode: Some(PerfMode::Balanced), fan_speed: None, logo_mode: None },
        ];
        // No listed process running -> None.
        let running = vec!["explorer.exe".to_string(), "svchost.exe".to_string()];
        assert_eq!(matching_app_profile(&profiles, &running), None);

        // Case-insensitive match on the executable name.
        let running = vec!["Game.EXE".to_string()];
        assert_eq!(matching_app_profile(&profiles, &running), Some(0));

        // Both running -> earlier rule (index 0) wins.
        let running = vec!["editor.exe".to_string(), "game.exe".to_string()];
        assert_eq!(matching_app_profile(&profiles, &running), Some(0));

        // Only the second rule's process is up.
        let running = vec!["editor.exe".to_string()];
        assert_eq!(matching_app_profile(&profiles, &running), Some(1));

        // Empty rule set never matches.
        assert_eq!(matching_app_profile(&[], &running), None);
    }

    #[test]
    fn app_profile_overlay_applies_only_set_fields() {
        let base = DeviceState {
            perf_mode: PerfMode::Balanced,
            fan_speed: FanSpeed::Auto,
            ..Default::default()
        };
        // A perf-only rule changes perf and leaves fan/logo alone.
        let perf_only = AppProfile {
            process: "game.exe".into(),
            perf_mode: Some(PerfMode::Hyperboost),
            fan_speed: None,
            logo_mode: None,
        };
        let out = perf_only.overlay(&base);
        assert_eq!(out.perf_mode, PerfMode::Hyperboost);
        assert_eq!(out.fan_speed, base.fan_speed);
        assert_eq!(out.lights_mode.logo_mode, base.lights_mode.logo_mode);

        // A fan-only rule leaves perf untouched.
        let fan_only = AppProfile {
            process: "zoom.exe".into(),
            perf_mode: None,
            fan_speed: Some(FanSpeed::Manual(3000)),
            logo_mode: None,
        };
        let out = fan_only.overlay(&base);
        assert_eq!(out.perf_mode, PerfMode::Balanced);
        assert_eq!(out.fan_speed, FanSpeed::Manual(3000));
    }

    // ---- device-facing logic, exercised through MockTransport (no hardware) ------

    use crate::packet::Packet;
    use crate::transport::MockTransport;

    /// Canned register response whose args begin with `args` (rest zero).
    fn reply(args: &[u8]) -> Packet {
        let mut p = Packet::new(0, &[]);
        p.set_args(args);
        p
    }

    #[test]
    fn read_assembles_state_from_register_reads() {
        use crate::types::{FanMode, PerfMode as Wire};
        let mock = MockTransport::with_responder(|req| match req.command() {
            0x0d82 => reply(&[0, 0, Wire::Performance as u8, FanMode::Auto as u8]),
            0x0380 => reply(&[1, 4, 0]),                 // logo power off -> LogoMode::Off
            0x0383 => reply(&[1, 5, 200]),               // keyboard brightness (id 5)
            0x0084 => reply(&[LightsAlwaysOn::Disable as u8, 0]),
            0x0792 => reply(&[BatteryCare::Percent80 as u8]),
            other => panic!("unexpected command {other:#06x}"),
        });
        let expected = DeviceState {
            perf_mode: PerfMode::Performance,
            lights_mode: LightsMode {
                logo_mode: LogoMode::Off,
                keyboard_brightness: 200,
                always_on: LightsAlwaysOn::Disable,
            },
            battery_care: BatteryCare::Percent80,
            fan_speed: FanSpeed::Auto,
            max_fan: false,
        };
        assert_eq!(DeviceState::read(&mock).unwrap(), expected);
    }

    #[test]
    fn read_custom_mode_pulls_cpu_and_gpu_boosts() {
        use crate::types::{FanMode, PerfMode as Wire};
        let mock = MockTransport::with_responder(|req| match req.command() {
            0x0d82 => reply(&[0, 0, Wire::Custom as u8, FanMode::Auto as u8]),
            // _get_boost echoes the cluster at args[1]; cpu(1)->Boost, gpu(2)->High.
            0x0d87 => {
                let cluster = req.get_args()[1];
                let boost = if cluster == 1 { CpuBoost::Boost as u8 } else { GpuBoost::High as u8 };
                reply(&[0, cluster, boost])
            }
            0x0380 => reply(&[1, 4, 0]),
            0x0383 => reply(&[1, 5, 0]),
            0x0084 => reply(&[LightsAlwaysOn::Disable as u8, 0]),
            0x0792 => reply(&[BatteryCare::Percent80 as u8]),
            0x078f => reply(&[MaxFanSpeedMode::Disable as u8]), // Custom perf -> read() queries max fan
            other => panic!("unexpected command {other:#06x}"),
        });
        assert_eq!(
            DeviceState::read(&mock).unwrap().perf_mode,
            PerfMode::Custom(CpuBoost::Boost, GpuBoost::High)
        );
    }

    #[test]
    fn apply_writes_brightness_and_battery_but_not_device_mode_or_enforce_brightness() {
        // The documented contract: apply() writes brightness (0x0303) and battery care
        // (0x0712) but must NOT write device mode (0x0004) -- driver mode breaks the Fn
        // keys, so always-on is a Normal-mode keep-alive instead. enforce_to() writes
        // battery but must NOT touch brightness.
        let state = DeviceState::default(); // Performance / Auto / logo Off -> simple write path

        let apply_mock = MockTransport::echo();
        state.apply(&apply_mock).unwrap();
        let applied: Vec<u16> = apply_mock.sent().iter().map(|(c, _)| *c).collect();
        assert!(applied.contains(&0x0303), "apply writes keyboard brightness");
        assert!(applied.contains(&0x0712), "apply writes battery care");
        assert!(!applied.contains(&0x0004), "apply must NOT set device mode (driver mode breaks Fn keys)");

        let enforce_mock = MockTransport::echo();
        state.enforce_to(&enforce_mock).unwrap();
        let enforced: Vec<u16> = enforce_mock.sent().iter().map(|(c, _)| *c).collect();
        assert!(enforced.contains(&0x0712), "enforce_to still asserts battery care");
        assert!(!enforced.contains(&0x0303), "enforce_to must not touch brightness");
        assert!(!enforced.contains(&0x0004), "enforce_to must not touch device mode");
    }

    #[test]
    fn enforced_fields_differ_ignores_brightness_and_always_on() {
        let base = DeviceState::default();

        // Identical -> no drift.
        assert!(!base.enforced_fields_differ(&base));

        // Brightness / always-on are NOT enforced fields -> still no drift.
        let mut cosmetic = base;
        cosmetic.lights_mode.keyboard_brightness = 123;
        cosmetic.lights_mode.always_on = LightsAlwaysOn::Enable;
        assert!(!cosmetic.enforced_fields_differ(&base));

        // Enforced fields -> drift.
        let mut other_perf = base;
        other_perf.perf_mode = PerfMode::Silent;
        assert!(other_perf.enforced_fields_differ(&base));

        let mut other_batt = base;
        other_batt.battery_care = BatteryCare::Disable;
        assert!(other_batt.enforced_fields_differ(&base));
    }

    #[test]
    fn profile_for_power_switches_only_on_mismatch() {
        let ac = DeviceState::default(); // Performance
        let batt = DeviceState { perf_mode: PerfMode::Battery, ..Default::default() };

        // On AC but currently running the battery profile -> switch to AC.
        assert_eq!(profile_for_power(true, &batt, &ac, &batt), Some(ac));
        // On AC and already on the AC profile -> no switch.
        assert_eq!(profile_for_power(true, &ac, &ac, &batt), None);
        // On battery but running AC profile -> switch to battery.
        assert_eq!(profile_for_power(false, &ac, &ac, &batt), Some(batt));
        // On battery and already on battery profile -> no switch.
        assert_eq!(profile_for_power(false, &batt, &ac, &batt), None);
    }
}

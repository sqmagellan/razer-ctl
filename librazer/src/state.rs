//! Data model: the device-state the tray reads/writes, the persisted config, and the
//! pure helpers around them. Deliberately free of menu/OS code so it stays easy to
//! read and unit-test (see the `tests` module at the bottom).

use anyhow::Result;
use serde::{Deserialize, Serialize};

use crate::command;
use crate::transport::HidTransport;
use crate::types::{
    BatteryCare, CpuBoost, FanMode, GpuBoost, KeyboardEffect, LightsAlwaysOn, LogoMode,
    MaxFanSpeedMode,
};

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
///
/// These are the true extremes of every chassis we know of, and there used to be three
/// disagreeing numbers for the same idea: `command.rs` hard-coded `0..=5500`, this ceiling
/// was 5300, and the per-PID table reaches 5600 (Blade 14 2025). A caller-supplied RPM was
/// therefore validated against a bound that matched no real machine. Both ends now come
/// from the device tables: floor 2000 (Blade 15 2021 Advanced), ceiling 5600.
/// A test in `descriptor.rs` keeps them consistent with the table.
pub const FAN_RPM_MIN_ANY: u16 = 2000;
pub const FAN_RPM_MAX_ANY: u16 = 5600;

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
    /// Keyboard backlight effect. `None` means "unknown / leave the keyboard lighting
    /// untouched" -- so existing configs and anyone who never picks an effect keep today's
    /// behavior (brightness-only, no effect write; no regression). `Some(effect)` is written
    /// on apply.
    ///
    /// This IS readable on PID 0x029F via `0x0f82` (HW-verified 2026-07-25), so unlike
    /// always-on it appears in `read()` and can be reconciled. It is deliberately still
    /// NOT in `enforced_fields_differ`: the effect is cosmetic, and a read that returns
    /// `None` on a model whose firmware lacks the getter would otherwise look like
    /// permanent drift and re-assert on every poll forever. Effects-only, no color (see
    /// [`crate::types::KeyboardEffect`] for why arbitrary color is out of scope).
    #[serde(default)]
    pub keyboard_effect: Option<KeyboardEffect>,
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

/// Percent steps the keyboard-brightness menu offers (0, 10, ..., 100).
pub const BRIGHTNESS_PERCENT_STEP: u8 = 10;

/// The menu step closest to a raw 0..=255 brightness.
///
/// The hardware Fn keys walk a 16-step ladder that doesn't line up with our 10% marks, so
/// an externally-set brightness usually lands *between* steps. Highlighting the nearest one
/// keeps exactly one checkmark in the menu instead of none. Lives here (not in the tray)
/// because it's pure arithmetic that the host test suite can actually cover -- the tray
/// crate can't be built on a non-Windows/Linux host at all.
pub fn nearest_brightness_percent(brightness: u8) -> u8 {
    (0u8..=100)
        .step_by(BRIGHTNESS_PERCENT_STEP as usize)
        .min_by_key(|p| (percent_to_brightness(*p) as i32 - brightness as i32).abs())
        .unwrap_or(0)
}

/// The next perf mode in the tray's left-click cycle.
///
/// Pure, and deliberately in the library so it is exercised by the host test suite; the
/// tray only wraps it with the surrounding `DeviceState`. Entering Custom seeds the
/// documented boost defaults, matching what picking Custom from the menu does.
pub fn next_perf_mode(current: PerfMode) -> PerfMode {
    match current {
        PerfMode::Battery => PerfMode::Silent,
        PerfMode::Silent => PerfMode::Balanced,
        PerfMode::Balanced => PerfMode::Performance,
        PerfMode::Performance => PerfMode::Hyperboost,
        PerfMode::Hyperboost => PerfMode::Custom(CpuBoost::Boost, GpuBoost::High),
        PerfMode::Custom(..) => PerfMode::Battery,
    }
}

impl DeviceState {
    /// Read the device's *actual* current state. Used by the Mirror refresh to keep
    /// the tray display honest. This is read-only: callers must treat the result as
    /// something to *display*, never to re-apply (re-applying would fight external
    /// changes such as Fn-key brightness and could overwrite saved AC/battery
    /// profiles). Returns Err on any transient HID failure; callers should swallow
    /// that and keep the last known-good values rather than propagating it.
    pub fn read(device: &impl HidTransport) -> Result<Self> {
        // One read serves both the perf mode and the fan mode: they come back in the
        // same 0x0d82 response. This used to call get_perf_mode() twice -- and since
        // each call reads *both* fan zones, that was 4 HID round-trips where 2 suffice.
        // On the hover path (read + fan RPM) the whole exchange was 15 round-trips.
        let (raw_perf_mode, raw_fan_mode) = command::get_perf_mode(device)?;

        let perf_mode = match raw_perf_mode {
            crate::types::PerfMode::Battery => PerfMode::Battery,
            crate::types::PerfMode::Silent => PerfMode::Silent,
            crate::types::PerfMode::Balanced => PerfMode::Balanced,
            crate::types::PerfMode::Performance => PerfMode::Performance,
            crate::types::PerfMode::Hyperboost => PerfMode::Hyperboost,
            crate::types::PerfMode::Custom => {
                let cpu_boost = command::get_cpu_boost(device)?;
                let gpu_boost = command::get_gpu_boost(device)?;
                PerfMode::Custom(cpu_boost, gpu_boost)
            }
        };

        let fan_speed = match raw_fan_mode {
            FanMode::Auto => FanSpeed::Auto,
            FanMode::Manual => {
                let rpm = command::get_fan_rpm(device, crate::types::FanZone::Zone1)?;
                FanSpeed::Manual(rpm)
            }
        };
        let lights_mode = LightsMode {
            logo_mode: command::get_logo_mode(device)?,
            keyboard_brightness: command::get_keyboard_brightness(device)?,
            always_on: command::get_lights_always_on(device)?,
            // The effect IS readable on this device (0x0f82) -- see get_keyboard_effect.
            // A read failure is not fatal: the effect is cosmetic and older/other models
            // may genuinely lack the getter, so fall back to "unknown" (None) rather than
            // failing the whole state read and blanking the tray.
            keyboard_effect: command::get_keyboard_effect(device).unwrap_or(None),
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
    ///
    /// The perf/fan/logo writes are *independent* EC commands, so a failure in one is
    /// not a reason to skip the rest: bailing early used to leave the device in the new
    /// perf mode with the fan and logo still on their previous settings. Every write is
    /// now attempted, and the first hard error is returned once the sequence completes.
    fn apply_perf_fan_logo(&self, device: &impl HidTransport) -> Result<()> {
        // Boosts are only meaningful once Custom is set, so that inner chain still stops
        // on error -- but it no longer aborts the fan/logo writes below.
        let perf_result = match self.perf_mode {
            PerfMode::Battery => command::set_perf_mode(device, crate::types::PerfMode::Battery),
            PerfMode::Silent => command::set_perf_mode(device, crate::types::PerfMode::Silent),
            PerfMode::Balanced => command::set_perf_mode(device, crate::types::PerfMode::Balanced),
            PerfMode::Performance => {
                command::set_perf_mode(device, crate::types::PerfMode::Performance)
            }
            PerfMode::Hyperboost => {
                command::set_perf_mode(device, crate::types::PerfMode::Hyperboost)
            }
            PerfMode::Custom(cpu_boost, gpu_boost) => {
                command::set_perf_mode(device, crate::types::PerfMode::Custom)
                    .and_then(|_| command::set_cpu_boost(device, cpu_boost))
                    .and_then(|_| command::set_gpu_boost(device, gpu_boost))
            }
        };
        if let Err(e) = &perf_result {
            log::warn!("perf mode command failed: {:?}", e);
        }

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

        let logo_result = command::set_logo_mode(device, self.lights_mode.logo_mode);
        if let Err(e) = &logo_result {
            log::warn!("logo command failed: {:?}", e);
        }

        // Perf mode is the more consequential of the two hard-failure paths, so it wins
        // when both fail; the logo error still surfaces when perf succeeded.
        perf_result.and(logo_result)
    }

    pub fn apply(&self, device: &impl HidTransport) -> Result<()> {
        self.apply_perf_fan_logo(device)?;
        // NB: always-on is deliberately NOT applied here. It is the Razer "device mode"
        // command (0x0004); Enable == driver mode, which disables the keyboard's native
        // Fn media keys (brightness/volume). The tray keeps the device in Normal mode and
        // implements "keyboard always-on" as a Normal-mode keep-alive instead.
        //
        // Keyboard lighting is applied BEFORE brightness: some effect writes reset the
        // backlight brightness, so re-asserting brightness afterward keeps it authoritative.
        self.apply_keyboard_lighting(device)?;
        command::set_keyboard_brightness(device, self.lights_mode.keyboard_brightness)?;
        command::set_battery_care(device, self.battery_care)
    }

    /// Write the keyboard backlight effect + color, if one is configured. Write-only (Chroma
    /// has no getter on this device), so it's driven purely from stored intent -- never read
    /// back, never in `enforced_fields_differ` or the Mirror poll. `None` leaves the keyboard
    /// lighting untouched (the default), so this is a no-op for configs/users that never set an
    /// effect. Kept as its own method so callers beyond `apply()` (e.g. a future resume
    /// re-assert, if HW testing shows the EC drops the effect on wake) can re-push it cheaply.
    pub fn apply_keyboard_lighting(&self, device: &impl HidTransport) -> Result<()> {
        match self.lights_mode.keyboard_effect {
            Some(effect) => command::set_keyboard_effect(device, effect),
            None => Ok(()),
        }
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
                // No effect intent by default: a fresh config must not impose lighting
                // the user never asked for.
                keyboard_effect: None,
            },
            battery_care: BatteryCare::from_percent(80).unwrap(),
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
    /// Keyboard backlight effect to apply while it runs (e.g. Spectrum for a game). Omit to
    /// leave the keyboard lighting unchanged.
    #[serde(default)]
    pub keyboard_effect: Option<KeyboardEffect>,
}

impl AppProfile {
    /// Overlay this rule's set fields onto `base`, yielding the transient state to apply
    /// while the app runs. Unset (`None`) fields leave `base` untouched, so a rule can
    /// change just the fan, just the perf mode, the logo, the keyboard effect, or any
    /// combination.
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
        if let Some(e) = self.keyboard_effect {
            s.lights_mode.keyboard_effect = Some(e);
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
            ac_state: DeviceState {
                ..Default::default()
            },
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
///
/// `active_rule` is the app-profile currently being enforced by "Actions", if any. It
/// MUST be passed, because an Actions override lives only in `device_state` -- by design
/// it never writes `ac_state`/`battery_state`. Without it this comparison sees the
/// override as "drift" from the saved profile and reverts it on the very next tick
/// (~1s), so an app rule would hold for about a second and then silently die. With it,
/// the rule is re-overlaid onto the power-source profile: the AC<->battery switch still
/// happens, but the running app's settings survive it.
pub fn profile_for_power(
    ac_power: bool,
    current: &DeviceState,
    ac_state: &DeviceState,
    battery_state: &DeviceState,
    active_rule: Option<&AppProfile>,
) -> Option<DeviceState> {
    let base = if ac_power { ac_state } else { battery_state };
    let target = match active_rule {
        Some(rule) => rule.overlay(base),
        None => *base,
    };
    (*current != target).then_some(target)
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
            AppProfile {
                process: "game.exe".into(),
                perf_mode: Some(PerfMode::Hyperboost),
                fan_speed: None,
                logo_mode: None,
                keyboard_effect: None,
            },
            AppProfile {
                process: "editor.exe".into(),
                perf_mode: Some(PerfMode::Balanced),
                fan_speed: None,
                logo_mode: None,
                keyboard_effect: None,
            },
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
            keyboard_effect: None,
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
            keyboard_effect: None,
        };
        let out = fan_only.overlay(&base);
        assert_eq!(out.perf_mode, PerfMode::Balanced);
        assert_eq!(out.fan_speed, FanSpeed::Manual(3000));
    }

    #[test]
    fn app_profile_overlay_carries_keyboard_lighting() {
        // A lighting rule sets the effect and leaves perf/fan/logo alone.
        let base = DeviceState::default();
        let lit = AppProfile {
            process: "game.exe".into(),
            perf_mode: None,
            fan_speed: None,
            logo_mode: None,
            keyboard_effect: Some(KeyboardEffect::Spectrum),
        };
        let out = lit.overlay(&base);
        assert_eq!(
            out.lights_mode.keyboard_effect,
            Some(KeyboardEffect::Spectrum)
        );
        assert_eq!(out.perf_mode, base.perf_mode);
        assert_eq!(out.fan_speed, base.fan_speed);
    }

    #[test]
    fn apply_writes_effect_only_when_configured() {
        use crate::transport::MockTransport;

        // Default (keyboard_effect: None) -> apply() must NOT emit the effect command 0x0f02,
        // so existing brightness-only behavior is preserved (no lighting regression).
        let none_mock = MockTransport::echo();
        DeviceState::default().apply(&none_mock).unwrap();
        let cmds: Vec<u16> = none_mock.sent().iter().map(|(c, _)| *c).collect();
        assert!(
            !cmds.contains(&0x0f02),
            "no effect configured -> no 0x0f02 write"
        );

        // With an effect set, apply() emits 0x0f02 AND re-asserts brightness (0x0303) after it,
        // so the effect write can't leave the backlight at the wrong brightness.
        let mut lit = DeviceState::default();
        lit.lights_mode.keyboard_effect = Some(KeyboardEffect::Spectrum);
        lit.lights_mode.keyboard_brightness = 200;
        let lit_mock = MockTransport::echo();
        lit.apply(&lit_mock).unwrap();
        let sent: Vec<u16> = lit_mock.sent().iter().map(|(c, _)| *c).collect();
        let effect_at = sent
            .iter()
            .position(|c| *c == 0x0f02)
            .expect("effect written");
        let bright_at = sent
            .iter()
            .position(|c| *c == 0x0303)
            .expect("brightness written");
        assert!(
            effect_at < bright_at,
            "brightness must be re-asserted after the effect write"
        );
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
            0x0380 => reply(&[1, 4, 0]), // logo power off -> LogoMode::Off
            0x0383 => reply(&[1, 5, 200]), // keyboard brightness (id 5)
            0x0084 => reply(&[LightsAlwaysOn::Disable as u8, 0]),
            // 0x0f82: keyboard effect getter (LED region 5 = backlight), id 3 = Spectrum.
            0x0f82 => reply(&[1, 5, 0x03]),
            0x0792 => reply(&[BatteryCare::from_percent(80).unwrap().wire_byte()]),
            other => panic!("unexpected command {other:#06x}"),
        });
        let expected = DeviceState {
            perf_mode: PerfMode::Performance,
            lights_mode: LightsMode {
                logo_mode: LogoMode::Off,
                keyboard_brightness: 200,
                always_on: LightsAlwaysOn::Disable,
                // Now READ back from the device (0x0f82) rather than assumed unknown.
                keyboard_effect: Some(KeyboardEffect::Spectrum),
            },
            battery_care: BatteryCare::from_percent(80).unwrap(),
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
                let boost = if cluster == 1 {
                    CpuBoost::Boost as u8
                } else {
                    GpuBoost::High as u8
                };
                reply(&[0, cluster, boost])
            }
            0x0380 => reply(&[1, 4, 0]),
            0x0383 => reply(&[1, 5, 0]),
            0x0084 => reply(&[LightsAlwaysOn::Disable as u8, 0]),
            0x0f82 => reply(&[1, 5, 0x03]),
            0x0792 => reply(&[BatteryCare::from_percent(80).unwrap().wire_byte()]),
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
        assert!(
            applied.contains(&0x0303),
            "apply writes keyboard brightness"
        );
        assert!(applied.contains(&0x0712), "apply writes battery care");
        assert!(
            !applied.contains(&0x0004),
            "apply must NOT set device mode (driver mode breaks Fn keys)"
        );

        let enforce_mock = MockTransport::echo();
        state.enforce_to(&enforce_mock).unwrap();
        let enforced: Vec<u16> = enforce_mock.sent().iter().map(|(c, _)| *c).collect();
        assert!(
            enforced.contains(&0x0712),
            "enforce_to still asserts battery care"
        );
        assert!(
            !enforced.contains(&0x0303),
            "enforce_to must not touch brightness"
        );
        assert!(
            !enforced.contains(&0x0004),
            "enforce_to must not touch device mode"
        );
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
        other_batt.battery_care = BatteryCare::DISABLE;
        assert!(other_batt.enforced_fields_differ(&base));
    }

    #[test]
    fn profile_for_power_switches_only_on_mismatch() {
        let ac = DeviceState::default(); // Performance
        let batt = DeviceState {
            perf_mode: PerfMode::Battery,
            ..Default::default()
        };

        // On AC but currently running the battery profile -> switch to AC.
        assert_eq!(profile_for_power(true, &batt, &ac, &batt, None), Some(ac));
        // On AC and already on the AC profile -> no switch.
        assert_eq!(profile_for_power(true, &ac, &ac, &batt, None), None);
        // On battery but running AC profile -> switch to battery.
        assert_eq!(profile_for_power(false, &ac, &ac, &batt, None), Some(batt));
        // On battery and already on battery profile -> no switch.
        assert_eq!(profile_for_power(false, &batt, &ac, &batt, None), None);
    }

    #[test]
    fn profile_for_power_does_not_revert_an_active_app_override() {
        // Regression: an Actions override lives ONLY in device_state (update_transient
        // never touches ac_state/battery_state). Without knowing the active rule, the
        // AC/battery check saw the override as drift and reverted it on the next ~1s
        // tick -- so an app profile held for about a second and then died.
        let ac = DeviceState::default(); // Performance
        let batt = DeviceState {
            perf_mode: PerfMode::Battery,
            ..Default::default()
        };
        let rule = AppProfile {
            process: "game.exe".into(),
            perf_mode: Some(PerfMode::Hyperboost),
            fan_speed: None,
            logo_mode: None,
            keyboard_effect: None,
        };

        // On AC with the rule applied, the live state IS the overlay -> nothing to do.
        let overridden = rule.overlay(&ac);
        assert_eq!(
            profile_for_power(true, &overridden, &ac, &batt, Some(&rule)),
            None,
            "an active app override must not be treated as drift"
        );

        // Same state, but the rule is no longer active (app exited) -> revert to AC.
        assert_eq!(
            profile_for_power(true, &overridden, &ac, &batt, None),
            Some(ac),
            "once the rule is gone the override must be reverted"
        );
    }

    #[test]
    fn profile_for_power_reoverlays_the_rule_across_a_power_switch() {
        // Unplugging while a rule is active must still switch to the battery profile,
        // but the running app's settings have to survive the switch.
        let ac = DeviceState::default(); // Performance
        let batt = DeviceState {
            perf_mode: PerfMode::Battery,
            ..Default::default()
        };
        // A fan-only rule, so we can see the base perf mode change underneath it.
        let rule = AppProfile {
            process: "game.exe".into(),
            perf_mode: None,
            fan_speed: Some(FanSpeed::Manual(3000)),
            logo_mode: None,
            keyboard_effect: None,
        };

        let on_ac = rule.overlay(&ac);
        let target = profile_for_power(false, &on_ac, &ac, &batt, Some(&rule))
            .expect("unplugging must still switch profiles");
        // Base switched to the battery profile...
        assert_eq!(target.perf_mode, PerfMode::Battery);
        // ...while the rule's fan setting was re-applied on top.
        assert_eq!(target.fan_speed, FanSpeed::Manual(3000));
    }

    // ---- apply_perf_fan_logo: one failed write must not skip the others -----------

    /// A transport that fails a chosen command and echoes everything else, recording
    /// which commands were attempted.
    fn failing_on(command: u16) -> crate::transport::MockTransport {
        crate::transport::MockTransport::with_responder(move |req| {
            if req.command() == command {
                // A non-echoing reply fails the `starts_with(args)` check in
                // `_send_command`, which is how a rejected write surfaces.
                Packet::new(req.command(), &[0xff, 0xff, 0xff, 0xff])
            } else {
                Packet::new(req.command(), req.get_args())
            }
        })
    }

    #[test]
    fn apply_still_writes_fan_and_logo_when_the_perf_write_fails() {
        let state = DeviceState {
            perf_mode: PerfMode::Balanced,
            fan_speed: FanSpeed::Manual(3000),
            ..DeviceState::default()
        };

        // 0x0d02 is the perf-mode write. Before the fix its failure aborted the whole
        // sequence, leaving the fan and logo on their previous settings.
        let mock = failing_on(0x0d02);
        let err = state.apply_perf_fan_logo(&mock);
        assert!(err.is_err(), "a failed perf write must still be reported");

        let commands: Vec<u16> = mock.sent().into_iter().map(|(c, _)| c).collect();
        // The fan write shares command 0x0d02 with the perf write (set_fan_mode re-sends
        // the perf packet with a new fan mode), so a second 0x0d02 is the fan attempt.
        assert!(
            commands.iter().filter(|&&c| c == 0x0d02).count() >= 2,
            "fan write must still be attempted, got {commands:04x?}"
        );
        assert!(
            commands.contains(&0x0300),
            "logo write must still be attempted, got {commands:04x?}"
        );
    }

    #[test]
    fn apply_reports_a_logo_failure_when_perf_succeeded() {
        let state = DeviceState {
            perf_mode: PerfMode::Balanced,
            ..DeviceState::default()
        };
        let mock = failing_on(0x0300);
        assert!(
            state.apply_perf_fan_logo(&mock).is_err(),
            "a failed logo write must not be swallowed"
        );
    }

    #[test]
    fn apply_succeeds_when_every_write_is_accepted() {
        let state = DeviceState {
            perf_mode: PerfMode::Balanced,
            fan_speed: FanSpeed::Manual(3000),
            ..DeviceState::default()
        };
        let mock = crate::transport::MockTransport::echo();
        assert!(state.apply_perf_fan_logo(&mock).is_ok());
    }

    /// Pin the HID cost of a `read()`.
    ///
    /// `read()` runs on the tray's hover and Mirror paths, so its round-trip count is a
    /// user-visible latency budget, not a micro-optimisation. It previously called
    /// `get_perf_mode()` twice -- once for the perf mode, once for the fan mode -- and
    /// because each call reads both fan zones that cost 4 round-trips where 2 do. A
    /// number here is worth more than a comment: if a future edit reintroduces a
    /// duplicate read, this fails loudly instead of quietly doubling the poll cost.
    #[test]
    fn read_does_not_issue_duplicate_perf_mode_queries() {
        use crate::transport::MockTransport;

        // Custom + Manual fan: the most expensive branch (adds boost and setpoint reads).
        let mock = MockTransport::with_responder(|req| {
            let mut p = crate::packet::Packet::new(req.command(), &[]);
            match req.command() {
                0x0d82 => p.set_args(&[
                    0,
                    0,
                    crate::types::PerfMode::Custom as u8,
                    crate::types::FanMode::Manual as u8,
                ]),
                // Answer per cluster: GpuBoost only decodes 0..=2, so echoing the CPU
                // value (Boost = 3) at the GPU would fail the conversion, not the count.
                0x0d87 => {
                    let cluster = req.get_args()[1];
                    let value = if cluster == crate::types::Cluster::Cpu as u8 {
                        CpuBoost::Boost as u8
                    } else {
                        GpuBoost::High as u8
                    };
                    p.set_args(&[0, cluster, value])
                }
                0x0d81 => p.set_args(&[0, req.get_args()[1], 30]),
                0x0380 => p.set_args(&[1, 4, 1]),
                0x0382 => p.set_args(&[1, 4, 2]),
                0x0383 => p.set_args(&[1, 5, 200]),
                0x0084 => p.set_args(&[0, 0]),
                0x0f82 => p.set_args(&[1, 5, 0x03]),
                0x0792 => p.set_args(&[BatteryCare::from_percent(80).unwrap().wire_byte()]),
                0x078f => p.set_args(&[MaxFanSpeedMode::Disable as u8]),
                other => panic!("unexpected command {other:#06x}"),
            }
            p
        });

        DeviceState::read(&mock).expect("the canned device should read cleanly");

        // The perf/fan register (0x0d82) is two round-trips -- one per fan zone -- and
        // must be read exactly once for the whole state, not once per field.
        let perf_reads = mock.sent().iter().filter(|(c, _)| *c == 0x0d82).count();
        assert_eq!(
            perf_reads, 2,
            "0x0d82 should be read once (= 2 zone round-trips); a second call means \
             the perf-mode and fan-mode reads have drifted apart again"
        );

        // Whole-state budget. Bump deliberately if a genuinely new register is added.
        assert_eq!(
            mock.sent_count(),
            12,
            "read() round-trip budget changed. History: 13 originally, 11 after removing \
             the duplicate get_perf_mode, 12 once the keyboard-effect getter (0x0f82) \
             was added. Bump deliberately when a genuinely new register is read."
        );
    }

    // ---- pure tray logic, now testable on any host --------------------------------

    #[test]
    fn perf_mode_cycle_visits_every_mode_and_returns_to_start() {
        // The tray's left-click cycle. A missing arm would strand the user in a subset
        // of modes -- clicking forever without reaching, say, Silent.
        let mut seen = Vec::new();
        let mut mode = PerfMode::Battery;
        for _ in 0..6 {
            seen.push(mode);
            mode = next_perf_mode(mode);
        }
        assert_eq!(mode, PerfMode::Battery, "the cycle must close");
        assert_eq!(seen.len(), 6, "six distinct modes");
        for expected in [
            PerfMode::Battery,
            PerfMode::Silent,
            PerfMode::Balanced,
            PerfMode::Performance,
            PerfMode::Hyperboost,
        ] {
            assert!(seen.contains(&expected), "cycle must reach {expected:?}");
        }
        assert!(
            seen.iter().any(|m| matches!(m, PerfMode::Custom(..))),
            "cycle must reach Custom"
        );
    }

    #[test]
    fn perf_mode_cycle_seeds_custom_with_the_documented_boost_defaults() {
        // Entering Custom via the cycle must match what the menu does, or a left-click
        // and a menu pick would land on different boost levels.
        assert_eq!(
            next_perf_mode(PerfMode::Hyperboost),
            PerfMode::Custom(CpuBoost::Boost, GpuBoost::High)
        );
        // Leaving Custom ignores the boosts it held.
        assert_eq!(
            next_perf_mode(PerfMode::Custom(CpuBoost::Low, GpuBoost::Low)),
            PerfMode::Battery
        );
    }

    #[test]
    fn nearest_brightness_percent_always_lands_on_a_menu_step() {
        // Every possible raw value must map to a real 10% step, so the menu always shows
        // exactly one checkmark -- including the Fn-key ladder values that fall between.
        for raw in 0u8..=255 {
            let p = nearest_brightness_percent(raw);
            assert_eq!(p % BRIGHTNESS_PERCENT_STEP, 0, "raw {raw} -> non-step {p}");
            assert!(p <= 100);
        }
    }

    #[test]
    fn nearest_brightness_percent_is_exact_on_the_steps_and_rounds_between() {
        // Exact on our own steps.
        for p in (0u8..=100).step_by(BRIGHTNESS_PERCENT_STEP as usize) {
            assert_eq!(nearest_brightness_percent(percent_to_brightness(p)), p);
        }
        assert_eq!(nearest_brightness_percent(0), 0);
        assert_eq!(nearest_brightness_percent(255), 100);
        // 130 sits just above 50% (128) -> nearest is still 50.
        assert_eq!(nearest_brightness_percent(130), 50);
        // 210 is the value the maintainer's config holds; it lies between 80% (204) and 90% (230).
        assert_eq!(nearest_brightness_percent(210), 80);
    }
}

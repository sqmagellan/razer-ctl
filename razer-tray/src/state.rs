//! Data model: the device-state the tray reads/writes, the persisted config, and the
//! pure helpers around them. Deliberately free of menu/OS code so it stays easy to
//! read and unit-test (see the `tests` module at the bottom).

use anyhow::Result;
use serde::{Deserialize, Serialize};

use librazer::types::{BatteryCare, CpuBoost, FanMode, GpuBoost, LightsAlwaysOn, LogoMode};
use librazer::{command, device};

#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub enum FanSpeed {
    Auto,
    Manual(u16),
}

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
    pub fn read(device: &device::Device) -> Result<Self> {
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

    /// Perf mode + fan + logo -- the settings shared by `apply()` (full write) and
    /// `enforce_to()` (the Synapse tug-of-war reassert). Kept in one place so the two
    /// can't drift. Fan failures are logged but non-fatal (manual RPM can be rejected
    /// depending on mode); a logo/perf failure propagates.
    fn apply_perf_fan_logo(&self, device: &device::Device) -> Result<()> {
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
            log::warn!("fan command failed: {:?}", e);
        }

        command::set_logo_mode(device, self.lights_mode.logo_mode)
    }

    pub fn apply(&self, device: &device::Device) -> Result<()> {
        self.apply_perf_fan_logo(device)?;
        command::set_lights_always_on(device, self.lights_mode.always_on)?;
        command::set_keyboard_brightness(device, self.lights_mode.keyboard_brightness)?;
        command::set_battery_care(device, self.battery_care)
    }

    /// Re-assert the "enforced" subset of settings -- perf mode, fan, logo, and
    /// battery care -- WITHOUT touching keyboard brightness or lights-always-on.
    /// This is what the opt-in Enforce mode uses to win a tug-of-war with Synapse.
    /// Brightness is excluded so it stays on the adopt path (Fn keys keep working);
    /// always-on is excluded because it's owned by the display-state gate (it gets
    /// dropped while the display is off). It's exactly apply() minus those two writes.
    pub fn enforce_to(&self, device: &device::Device) -> Result<()> {
        self.apply_perf_fan_logo(device)?;
        command::set_battery_care(device, self.battery_care)
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

#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct ConfigState {
    pub ac_state: DeviceState,
    pub battery_state: DeviceState,
    // Opt-in "win against Synapse" mode. #[serde(default)] keeps older config
    // files (written before this field existed) loadable -> defaults to false.
    #[serde(default)]
    pub enforce: bool,
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
        }
    }
}

pub fn get_fan_rpm(device: &device::Device) -> Result<FanRpm> {
    Ok(FanRpm {
        fan1: command::get_fan_actual_rpm(device, librazer::types::FanZone::Zone1)?,
        fan2: command::get_fan_actual_rpm(device, librazer::types::FanZone::Zone2)?,
    })
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
    }
}

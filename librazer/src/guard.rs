//! Guards the tray lays over the state it aims the laptop at, without changing what the user
//! saved: the weak-charger clamp. The idea is from nezinomas/blade-lite (MIT,
//! `crates/razer-proto/src/charger.rs`, read 2026-11-19); the code here is ours.
//!
//! Pure: the tray feeds them readings and asks for the guarded state, so they are tested on
//! any host.

use crate::state::{DeviceState, PerfMode};
use crate::types::PowerSource;

// ---- weak charger ------------------------------------------------------------------------

/// Whether `source` is a USB-C charger (`0x078c` codes 1-7, 40-100 W). Synapse offers only
/// the battery presets on one, and so does blade-lite: the laptop draws more than such a
/// charger gives at Performance and up, and the battery makes up the rest.
pub fn is_weak_charger(source: PowerSource) -> bool {
    source.kind() == "usb-c"
}

/// The perf mode a weak charger holds `perf` to, or `None` when it is left alone: every
/// mode above Balanced (Performance, Hyperboost) and Custom go to Balanced; Battery, Silent
/// and Balanced stay.
pub fn weak_charger_perf(perf: PerfMode) -> Option<PerfMode> {
    match perf {
        PerfMode::Battery | PerfMode::Silent | PerfMode::Balanced => None,
        PerfMode::Performance | PerfMode::Hyperboost | PerfMode::Custom(..) => {
            Some(PerfMode::Balanced)
        }
    }
}

/// `state` as a weak charger allows it. Max Fan exists only in Custom, so it goes too.
pub fn clamp_for_weak_charger(state: DeviceState) -> DeviceState {
    match weak_charger_perf(state.perf_mode) {
        Some(perf_mode) => DeviceState {
            perf_mode,
            max_fan: false,
            ..state
        },
        None => state,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::state::FanSpeed;
    use crate::types::{CpuBoost, GpuBoost};

    fn custom() -> PerfMode {
        PerfMode::Custom(CpuBoost::Boost, GpuBoost::High)
    }

    #[test]
    fn a_usb_c_charger_is_weak_and_the_barrel_and_battery_are_not() {
        for code in 1..=7u8 {
            assert!(
                is_weak_charger(PowerSource::from_args(&[code, 0x12])),
                "{code}"
            );
        }
        // 0x029F on its 330 W barrel charger reads [0x12, 0x12].
        assert!(!is_weak_charger(PowerSource::from_args(&[0x12, 0x12])));
        assert!(!is_weak_charger(PowerSource::from_args(&[0x08])));
        assert!(!is_weak_charger(PowerSource::from_args(&[0, 0])));
    }

    #[test]
    fn modes_above_balanced_and_custom_are_held_to_balanced() {
        for perf in [PerfMode::Performance, PerfMode::Hyperboost, custom()] {
            assert_eq!(
                weak_charger_perf(perf),
                Some(PerfMode::Balanced),
                "{perf:?}"
            );
        }
        for perf in [PerfMode::Battery, PerfMode::Silent, PerfMode::Balanced] {
            assert_eq!(weak_charger_perf(perf), None, "{perf:?}");
        }
    }

    #[test]
    fn the_clamp_drops_max_fan_and_keeps_everything_else() {
        let state = DeviceState {
            perf_mode: custom(),
            max_fan: true,
            fan_speed: FanSpeed::Manual(3400),
            ..DeviceState::default()
        };
        let held = clamp_for_weak_charger(state);
        assert_eq!(held.perf_mode, PerfMode::Balanced);
        assert!(!held.max_fan);
        assert_eq!(held.fan_speed, FanSpeed::Manual(3400));
        assert_eq!(held.lights_mode, state.lights_mode);
        assert_eq!(held.battery_care, state.battery_care);
        let silent = DeviceState {
            perf_mode: PerfMode::Silent,
            ..DeviceState::default()
        };
        assert_eq!(clamp_for_weak_charger(silent), silent);
    }
}

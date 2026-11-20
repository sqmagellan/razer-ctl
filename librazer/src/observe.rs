//! When the tray may write settings that came from its config, and what a device reading
//! does when it may not.
//!
//! The tray used to push its config to the device at every start. That is right for a
//! config the user made and wrong for one the tray made up. On a first run the "config"
//! was hardcoded defaults, so installing the tray put the laptop in Performance (Battery on
//! battery), turned the logo and keyboard backlight off, set an 80% charge limit, and
//! replaced whatever Synapse or the CLI had set. A file that did not parse was handled the
//! same way: defaults, applied, then saved over it.
//!
//! Upstream's fix (blauzim/razer-ctl 435c870, `load_config_or_adopt`) reads the device once,
//! copies that one reading into BOTH profiles and then applies it. That writes an
//! observation back to the device, which [`DeviceState::read`] prohibits, and it makes the
//! battery profile a copy of whatever the laptop was doing on AC. This module does neither:
//!
//! - A profile that was never set ([`Unset`]) is adopted from a reading taken on its own
//!   power source, and the reading is never written back.
//! - The other source's profile stays unset until the laptop is first seen on it.
//! - While the power source is unknown and a profile is unset, nothing is adopted.
//! - A config that does not parse, or cannot be read, is never applied: the tray only
//!   observes until it parses again or the user resets it from the menu.
//!
//! Pure decisions: [`on_reading`] takes no transport, so adopting a reading cannot write
//! one. The tests drive it from [`DeviceState::read`] over a mock transport that panics on
//! anything but a getter.

use crate::state::{DeviceState, LightsMode};

/// What the config file on disk is worth.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ConfigHealth {
    /// It parsed, or there is none yet (a first run).
    Usable,
    /// It exists and does not parse. A timestamped copy is kept beside it; it is neither
    /// applied nor saved over.
    Corrupt,
    /// It exists and could not be read (an editor or antivirus holding it). Neither
    /// applied nor saved over until it can be read.
    Unreadable,
}

/// Which saved profiles are still waiting for their first reading.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Unset {
    pub ac: bool,
    pub battery: bool,
}

impl Unset {
    pub fn on(self, ac_power: bool) -> bool {
        if ac_power {
            self.ac
        } else {
            self.battery
        }
    }

    pub fn any(self) -> bool {
        self.ac || self.battery
    }

    /// The profile for `ac_power` has been adopted.
    pub fn clear(&mut self, ac_power: bool) {
        if ac_power {
            self.ac = false;
        } else {
            self.battery = false;
        }
    }
}

/// Why the tray only observes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ObserveReason {
    ConfigCorrupt,
    ConfigUnreadable,
    /// A profile is unset and the power source is unknown, so there is no telling which
    /// profile a reading belongs to.
    PowerUnknown,
}

/// What the tray does with the device right now.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Mode {
    /// Converge on the saved profile (or a running Action), as always.
    Apply,
    /// The current power source's profile is unset: the next successful reading becomes
    /// it. Nothing derived from the config is written until then.
    Adopt { ac_power: bool },
    /// Nothing derived from the config is written. Readings are displayed, and the intent
    /// follows them so the menu checks what is really set.
    Observe(ObserveReason),
}

impl Mode {
    /// Whether settings from the config (profiles, Actions, Enforce, wake re-asserts,
    /// zone repairs) may be written to the device. A menu pick is the user's explicit
    /// choice, not the config's, and is applied in every mode.
    pub fn writes_config(self) -> bool {
        matches!(self, Mode::Apply)
    }
}

/// The mode for a config in `health`, on `power` (`Some(true)` = AC, `None` = unknown),
/// with `unset` profiles.
///
/// A usable config whose profiles are all set behaves exactly as before, including when
/// the power source is unknown: the caller's last known source (AC at startup) is used.
pub fn mode(health: ConfigHealth, power: Option<bool>, unset: Unset) -> Mode {
    match health {
        ConfigHealth::Corrupt => Mode::Observe(ObserveReason::ConfigCorrupt),
        ConfigHealth::Unreadable => Mode::Observe(ObserveReason::ConfigUnreadable),
        ConfigHealth::Usable => match power {
            None if unset.any() => Mode::Observe(ObserveReason::PowerUnknown),
            None => Mode::Apply,
            Some(ac_power) if unset.on(ac_power) => Mode::Adopt { ac_power },
            Some(_) => Mode::Apply,
        },
    }
}

/// What a successful reading changes.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum Reading {
    /// [`Mode::Apply`]: nothing here. The tray's existing rules (Fn brightness adoption,
    /// Enforce) take it from there.
    Display,
    /// It becomes the saved profile for `ac_power`, and the intent. Not written back.
    Adopt {
        ac_power: bool,
        profile: DeviceState,
    },
    /// The intent becomes this, so nothing is seen as drift and the menu is honest.
    Follow(DeviceState),
}

/// The intent that matches `observed`, keeping what a read cannot report.
///
/// A custom keyboard color cannot be read back (the effect register still reports the
/// stored effect), and the refresh rate is a Windows setting `read` never fills in. Both are
/// things the tray itself put on the machine, so the intent keeps them. The rest is the
/// reading, unnormalized: intent equal to reality, so nothing is seen as drift.
fn matching_intent(intent: &DeviceState, observed: &DeviceState) -> DeviceState {
    DeviceState {
        lights_mode: LightsMode {
            keyboard_color: intent.lights_mode.keyboard_color,
            ..observed.lights_mode
        },
        display_refresh_hz: intent.display_refresh_hz,
        ..*observed
    }
}

/// What `observed`, a successful reading, changes in `mode`.
///
/// An adopted profile is normalized like every other profile (config load, menu pick), so
/// a reading the EC reports but cannot be asked for (a Manual set point below the
/// chassis floor, which is what a fresh boot reports) is stored as the nearest value it
/// can hold.
pub fn on_reading(
    mode: Mode,
    intent: &DeviceState,
    observed: &DeviceState,
    fan_rpm_range: (u16, u16),
) -> Reading {
    match mode {
        Mode::Apply => Reading::Display,
        Mode::Adopt { ac_power } => Reading::Adopt {
            ac_power,
            profile: matching_intent(intent, observed).normalized(fan_rpm_range),
        },
        Mode::Observe(_) => Reading::Follow(matching_intent(intent, observed)),
    }
}

/// Whether the read-back after a resync probe may write, given the mode from BEFORE the
/// probe. The probe's reading can adopt a profile and switch the mode to `Apply`; judging
/// by the mode after it let that same sync write the profile it had just adopted (with
/// Enforce on, a Manual set point below the floor is adopted as the floor, so the second
/// read saw drift). Diverged perf zones are repaired only when the config was already
/// being applied, as before.
pub fn probe_may_write(before: Mode, enforce: bool, diverged: bool) -> bool {
    (enforce || diverged) && before.writes_config()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::keyboard::KeyboardColor;
    use crate::packet::Packet;
    use crate::state::{ConfigState, FanSpeed, PerfMode};
    use crate::transport::MockTransport;
    use crate::types::{BatteryCare, FanMode, LightsAlwaysOn, LogoMode};

    const RANGE: (u16, u16) = (2000, 5000);
    const BOTH: Unset = Unset {
        ac: true,
        battery: true,
    };
    const NONE: Unset = Unset {
        ac: false,
        battery: false,
    };

    fn reply(args: &[u8]) -> Packet {
        let mut p = Packet::new(0, &[]);
        p.set_args(args);
        p
    }

    /// A device in Performance, fan Manual at `rpm`, logo off, brightness 200, charge limit
    /// 80 %. Any command but these getters panics, so a test that passes wrote nothing.
    fn device(rpm: u16) -> MockTransport {
        use crate::types::PerfMode as Wire;
        MockTransport::with_responder(move |req| match req.command() {
            0x0d82 => reply(&[0, 0, Wire::Performance as u8, FanMode::Manual as u8]),
            0x0d81 => reply(&[0, req.get_args()[1], (rpm / 100) as u8]),
            0x0380 => reply(&[1, 4, 0]),
            0x0383 => reply(&[1, 5, 200]),
            0x0084 => reply(&[LightsAlwaysOn::Disable as u8, 0]),
            0x0f82 => reply(&[1, 5, 0x03]),
            0x0792 => reply(&[BatteryCare::from_percent(80).unwrap().wire_byte()]),
            other => panic!("only getters may be sent, got {other:#06x}"),
        })
    }

    #[test]
    fn a_first_run_adopts_only_the_profile_of_the_known_power_source() {
        assert_eq!(
            mode(ConfigHealth::Usable, Some(true), BOTH),
            Mode::Adopt { ac_power: true }
        );
        assert_eq!(
            mode(ConfigHealth::Usable, Some(false), BOTH),
            Mode::Adopt { ac_power: false }
        );
        // Once AC is adopted, AC applies and battery still waits for its own reading.
        let ac_done = Unset {
            ac: false,
            battery: true,
        };
        assert_eq!(mode(ConfigHealth::Usable, Some(true), ac_done), Mode::Apply);
        assert_eq!(
            mode(ConfigHealth::Usable, Some(false), ac_done),
            Mode::Adopt { ac_power: false }
        );
    }

    #[test]
    fn an_unknown_power_source_adopts_nothing_while_a_profile_is_unset() {
        for unset in [
            BOTH,
            Unset {
                ac: false,
                battery: true,
            },
            Unset {
                ac: true,
                battery: false,
            },
        ] {
            let m = mode(ConfigHealth::Usable, None, unset);
            assert_eq!(m, Mode::Observe(ObserveReason::PowerUnknown));
            assert!(!m.writes_config());
        }
    }

    #[test]
    fn a_config_with_every_profile_set_behaves_as_before() {
        // Including an unknown power source: the caller keeps its last known one.
        for power in [Some(true), Some(false), None] {
            let m = mode(ConfigHealth::Usable, power, NONE);
            assert_eq!(m, Mode::Apply);
            assert!(m.writes_config());
        }
    }

    #[test]
    fn a_corrupt_or_unreadable_config_is_never_applied() {
        for power in [Some(true), Some(false), None] {
            for unset in [BOTH, NONE] {
                assert_eq!(
                    mode(ConfigHealth::Corrupt, power, unset),
                    Mode::Observe(ObserveReason::ConfigCorrupt)
                );
                assert_eq!(
                    mode(ConfigHealth::Unreadable, power, unset),
                    Mode::Observe(ObserveReason::ConfigUnreadable)
                );
            }
        }
        assert!(!Mode::Observe(ObserveReason::ConfigCorrupt).writes_config());
        assert!(!Mode::Adopt { ac_power: true }.writes_config());
    }

    /// Read `dev` and decide, as the tray does.
    fn observe(dev: &MockTransport, m: Mode, intent: &DeviceState) -> (DeviceState, Reading) {
        let observed = DeviceState::read(dev).unwrap();
        (observed, on_reading(m, intent, &observed, RANGE))
    }

    #[test]
    fn adopting_reads_the_device_and_writes_nothing() {
        let dev = device(3000);
        let placeholder = ConfigState::default().battery_state;
        let (observed, reading) = observe(&dev, Mode::Adopt { ac_power: false }, &placeholder);
        let Reading::Adopt { ac_power, profile } = reading else {
            panic!("expected an adoption, got {reading:?}");
        };
        assert!(!ac_power);
        assert_eq!(profile, observed, "a reachable reading is adopted as is");
        assert_eq!(profile.perf_mode, PerfMode::Performance);
        assert_eq!(profile.fan_speed, FanSpeed::Manual(3000));
        assert_eq!(profile.lights_mode.logo_mode, LogoMode::Off);
        assert!(
            dev.sent().iter().all(|(cmd, _)| cmd & 0x80 != 0),
            "every command sent is a getter: {:?}",
            dev.sent()
        );
    }

    #[test]
    fn observing_writes_nothing_in_any_mode() {
        for m in [
            Mode::Apply,
            Mode::Adopt { ac_power: true },
            Mode::Observe(ObserveReason::ConfigCorrupt),
            Mode::Observe(ObserveReason::PowerUnknown),
        ] {
            // The responder panics on anything but a getter.
            observe(&device(3000), m, &DeviceState::default());
        }
    }

    #[test]
    fn an_adopted_profile_is_normalized_like_any_other() {
        // A fresh boot reports Manual 0; the profile stores what the fan can hold.
        let (observed, reading) = observe(
            &device(0),
            Mode::Adopt { ac_power: true },
            &DeviceState::default(),
        );
        assert_eq!(observed.fan_speed, FanSpeed::Manual(0));
        let Reading::Adopt { profile, .. } = reading else {
            panic!("expected an adoption");
        };
        assert_eq!(profile.fan_speed, FanSpeed::Manual(2000));
        assert_eq!(profile, profile.normalized(RANGE));
    }

    #[test]
    fn following_keeps_what_a_read_cannot_report() {
        let mut intent = DeviceState::default();
        intent.lights_mode.keyboard_color = Some(KeyboardColor::FollowPerfMode);
        intent.display_refresh_hz = Some(240);
        let observed = DeviceState {
            perf_mode: PerfMode::Silent,
            max_fan: false,
            ..DeviceState::default()
        };
        let Reading::Follow(followed) = on_reading(
            Mode::Observe(ObserveReason::ConfigCorrupt),
            &intent,
            &observed,
            RANGE,
        ) else {
            panic!("expected Follow");
        };
        assert_eq!(followed.perf_mode, PerfMode::Silent);
        assert_eq!(
            followed.lights_mode.keyboard_color,
            Some(KeyboardColor::FollowPerfMode)
        );
        assert_eq!(followed.display_refresh_hz, Some(240));
        assert!(!followed.enforced_fields_differ(&observed), "no drift");
        assert_eq!(
            on_reading(Mode::Apply, &intent, &observed, RANGE),
            Reading::Display
        );
    }

    #[test]
    fn unset_tracks_each_power_source_separately() {
        let mut u = BOTH;
        assert!(u.on(true) && u.on(false) && u.any());
        u.clear(true);
        assert!(!u.on(true) && u.on(false) && u.any());
        u.clear(false);
        assert!(!u.any());
    }

    #[test]
    fn a_probe_that_adopts_never_writes_in_the_same_sync() {
        let adopting = Mode::Adopt { ac_power: true };
        assert!(!probe_may_write(adopting, true, false));
        assert!(!probe_may_write(adopting, true, true));
        assert!(!probe_may_write(
            Mode::Observe(ObserveReason::ConfigCorrupt),
            true,
            true
        ));
    }

    #[test]
    fn a_probe_while_applying_writes_only_for_enforce_or_diverged_zones() {
        assert!(probe_may_write(Mode::Apply, true, false));
        assert!(probe_may_write(Mode::Apply, false, true));
        assert!(!probe_may_write(Mode::Apply, false, false));
    }
}

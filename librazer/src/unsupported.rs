//! Optional settings a laptop turned out not to have.
//!
//! The generic profile for a model not in the supported list offers every feature,
//! because nothing says which ones its EC implements. Issue #2 (Blade 15 2018, `0x023b`)
//! showed the cost: one getter answered "not supported" (0x05), `DeviceState::read`
//! failed as a whole, and the tray could change nothing but the screen refresh rate.
//!
//! For such a model the device records each optional setting whose command the EC
//! answers with "not supported". Reads then fill that field with a placeholder, writes
//! skip it, drift checks ignore it and the tray hides its menu. The answer is a property
//! of the firmware, so it is never cleared while the process runs.
//!
//! Only optional settings are learned. Performance and fan mode share one command
//! (`0x0d82`), and without it the tray has nothing to show, so that failure still ends
//! the read. Supported models never learn: a 0x05 there stays an error, as before.
//!
//! A few ECs accept a command and ignore it, so they never answer 0x05 and nothing is
//! learned. [`KNOWN_LACKING`] lists those per PID from user reports; such a setting starts
//! out recorded as lacking.
//!
//! An EC can also ignore part of a manual fan set point: in some perf modes it runs the
//! fans faster than a low set point asks, while the set point still reads back as written.
//! [`MANUAL_FLOOR_BY_PERF`] lists those per PID from hardware runs, so the tray and CLI can
//! say what the fans run at instead of showing a set point they don't follow.

use std::sync::atomic::{AtomicU8, Ordering};

use crate::types::PerfMode;

/// A setting the tray and CLI can work without.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Optional {
    Logo,
    KeyboardBrightness,
    BatteryCare,
}

impl Optional {
    pub const ALL: [Optional; 3] = [
        Optional::Logo,
        Optional::KeyboardBrightness,
        Optional::BatteryCare,
    ];

    fn bit(self) -> u8 {
        match self {
            Optional::Logo => 1,
            Optional::KeyboardBrightness => 2,
            Optional::BatteryCare => 4,
        }
    }

    /// The name `razer-cli` and the log use for it.
    pub fn name(self) -> &'static str {
        match self {
            Optional::Logo => "logo",
            Optional::KeyboardBrightness => "keyboard brightness",
            Optional::BatteryCare => "battery care",
        }
    }
}

/// A set of [`Optional`] settings, by value.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Unsupported(u8);

impl Unsupported {
    pub fn contains(self, feature: Optional) -> bool {
        self.0 & feature.bit() != 0
    }

    pub fn with(self, feature: Optional) -> Self {
        Self(self.0 | feature.bit())
    }

    pub fn is_empty(self) -> bool {
        self.0 == 0
    }

    pub fn iter(self) -> impl Iterator<Item = Optional> {
        Optional::ALL.into_iter().filter(move |f| self.contains(*f))
    }
}

/// Settings a model's EC accepts but ignores, by PID, from user reports. The tray shows
/// them as "(not supported)" from the start instead of offering a menu that does nothing.
pub const KNOWN_LACKING: &[(u16, &[Optional])] = &[
    // Blade 15 2018 Base, issue #2: the logo reads back but writes change nothing.
    (0x023b, &[Optional::Logo]),
    // Blade 15 Base 2020 (RZ09-0328): its EC echoes every class 0x07 read and write without
    // acting on it, so a charge limit only looks set. A user's report in
    // jdsgnrinfo/blade-control's model table (MIT), 2026-10; not seen here.
    (0x0255, &[Optional::BatteryCare]),
];

/// What [`KNOWN_LACKING`] says the model with `pid` lacks; empty for any other PID.
pub fn known_lacking(pid: u16) -> Unsupported {
    KNOWN_LACKING
        .iter()
        .filter(|(p, _)| *p == pid)
        .flat_map(|(_, features)| features.iter())
        .fold(Unsupported::default(), |set, f| set.with(*f))
}

/// Perf modes in which a model's EC runs a manual fan at no less than a given speed, by
/// PID, as (PID, perf mode, RPM). Below that speed the EC accepts the set point and reads it
/// back unchanged, but the fans run at the floor. Above it the set point is followed.
///
/// Measured rows only, each backed by an EC model under `librazer/tests/ec/` with the same
/// floor (a conformance test checks they agree). A model or perf mode that isn't listed is
/// unknown, not floor-free: never copy a row to a similar chassis.
pub const MANUAL_FLOOR_BY_PERF: &[(u16, PerfMode, u16)] = &[
    // Blade 16 (2023), hw-test runs on 2026-10-20 (20:26, 20:41 and 20:51): in Custom with
    // the fan in Manual the fans run at the larger of the set point and 3800, with every
    // boost pair tried and with or without a Max Fan toggle first. A 3000 set point reads
    // back as 3000 while the fans hold 3800/3800; 4500 is followed. Balanced honors 2000 to
    // 5000. Hyperboost lifts the fans too, but along its own curve, so it has no fixed row.
    // Measured on an RZ09-0483U; the Black edition (RZ09-0483T) shares the PID, so it gets
    // the row too, untested.
    (0x029f, PerfMode::Custom, 3800),
];

/// The perf modes [`MANUAL_FLOOR_BY_PERF`] lists for `pid`, with their floors.
pub fn manual_fan_floors(pid: u16) -> impl Iterator<Item = (PerfMode, u16)> {
    MANUAL_FLOOR_BY_PERF
        .iter()
        .filter(move |(p, _, _)| *p == pid)
        .map(|(_, perf, rpm)| (*perf, *rpm))
}

/// The lowest speed a manual fan runs at in `perf` on the model with `pid`, if
/// [`MANUAL_FLOOR_BY_PERF`] lists one. `None` leaves only the chassis range.
pub fn manual_fan_floor(pid: u16, perf: PerfMode) -> Option<u16> {
    manual_fan_floors(pid)
        .find(|(mode, _)| *mode == perf)
        .map(|(_, rpm)| rpm)
}

/// What the fans run at instead of a manual set point of `rpm` in `perf`: the listed floor
/// when it is above `rpm`, else `None` (the set point is followed, as far as is known).
pub fn manual_fan_runs_at(pid: u16, perf: PerfMode, rpm: u16) -> Option<u16> {
    manual_fan_floor(pid, perf).filter(|floor| *floor > rpm)
}

/// The record a device keeps while it runs. Atomic so a `Device` stays `Sync`.
#[derive(Debug, Default)]
pub struct UnsupportedLog(AtomicU8);

impl UnsupportedLog {
    /// A record that starts with `known` already marked.
    pub fn starting_with(known: Unsupported) -> Self {
        Self(AtomicU8::new(known.0))
    }

    pub fn get(&self) -> Unsupported {
        Unsupported(self.0.load(Ordering::Relaxed))
    }

    /// Record `feature`; true the first time, so the caller logs it once.
    pub fn mark(&self, feature: Optional) -> bool {
        self.0.fetch_or(feature.bit(), Ordering::Relaxed) & feature.bit() == 0
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_set_holds_what_was_added() {
        let s = Unsupported::default().with(Optional::BatteryCare);
        assert!(s.contains(Optional::BatteryCare));
        assert!(!s.contains(Optional::Logo));
        assert_eq!(s.iter().collect::<Vec<_>>(), vec![Optional::BatteryCare]);
        assert!(Unsupported::default().is_empty());
    }

    #[test]
    fn the_blade_15_2018_base_starts_without_a_logo_and_nothing_else_does() {
        let known = known_lacking(0x023b);
        assert_eq!(known.iter().collect::<Vec<_>>(), vec![Optional::Logo]);
        assert!(known_lacking(0x029f).is_empty());

        let log = UnsupportedLog::starting_with(known);
        assert_eq!(log.get(), known);
        // Already recorded, so a later 0x05 isn't logged as news.
        assert!(!log.mark(Optional::Logo));
        assert!(log.mark(Optional::BatteryCare));
    }

    #[test]
    fn the_blade_16_2023_runs_a_manual_fan_at_3800_or_more_in_custom_only() {
        assert_eq!(manual_fan_floor(0x029f, PerfMode::Custom), Some(3800));
        assert_eq!(
            manual_fan_floors(0x029f).collect::<Vec<_>>(),
            vec![(PerfMode::Custom, 3800)]
        );
        for perf in [
            PerfMode::Balanced,
            PerfMode::Performance,
            PerfMode::Silent,
            PerfMode::Battery,
            PerfMode::Hyperboost,
        ] {
            assert_eq!(manual_fan_floor(0x029f, perf), None, "{perf:?}");
        }
        // Below the floor the fans run at it; at or above it they follow the set point.
        assert_eq!(
            manual_fan_runs_at(0x029f, PerfMode::Custom, 3000),
            Some(3800)
        );
        assert_eq!(
            manual_fan_runs_at(0x029f, PerfMode::Custom, 3700),
            Some(3800)
        );
        assert_eq!(manual_fan_runs_at(0x029f, PerfMode::Custom, 3800), None);
        assert_eq!(manual_fan_runs_at(0x029f, PerfMode::Custom, 4500), None);
        assert_eq!(manual_fan_runs_at(0x029f, PerfMode::Balanced, 2000), None);
    }

    #[test]
    fn no_other_model_has_a_perf_mode_fan_floor() {
        // Rows are measured, never carried over to a similar chassis.
        for pid in [0x029d, 0x02a0, 0x02b7, 0x023b, 0x028b, 0xffff] {
            assert_eq!(manual_fan_floors(pid).count(), 0, "{pid:#06x}");
            assert_eq!(manual_fan_runs_at(pid, PerfMode::Custom, 2000), None);
        }
        // One row per (PID, perf mode), so a lookup can't depend on table order.
        for (i, (pid, perf, _)) in MANUAL_FLOOR_BY_PERF.iter().enumerate() {
            assert!(
                !MANUAL_FLOOR_BY_PERF[..i]
                    .iter()
                    .any(|(p, m, _)| p == pid && m == perf),
                "{pid:#06x} {perf:?} listed twice"
            );
        }
    }

    #[test]
    fn the_log_reports_a_feature_new_only_once() {
        let log = UnsupportedLog::default();
        assert!(log.mark(Optional::Logo));
        assert!(!log.mark(Optional::Logo));
        assert!(log.mark(Optional::KeyboardBrightness));
        assert_eq!(
            log.get(),
            Unsupported::default()
                .with(Optional::Logo)
                .with(Optional::KeyboardBrightness)
        );
    }
}

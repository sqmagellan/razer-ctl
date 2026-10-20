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
//! out recorded as lacking. An EC that rejects only Max Fan "on" (issue #10) needs no entry:
//! the tray writes "off" on every Custom apply, and the first "on" someone picks is
//! rejected, recorded and grayed out.

use std::sync::atomic::{AtomicU8, Ordering};

/// A setting the tray and CLI can work without.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Optional {
    Logo,
    KeyboardBrightness,
    BatteryCare,
    /// Custom mode's Max Fan Speed (`0x070f`/`0x078f`).
    MaxFan,
}

impl Optional {
    pub const ALL: [Optional; 4] = [
        Optional::Logo,
        Optional::KeyboardBrightness,
        Optional::BatteryCare,
        Optional::MaxFan,
    ];

    fn bit(self) -> u8 {
        match self {
            Optional::Logo => 1,
            Optional::KeyboardBrightness => 2,
            Optional::BatteryCare => 4,
            Optional::MaxFan => 8,
        }
    }

    /// The name `razer-cli` and the log use for it.
    pub fn name(self) -> &'static str {
        match self {
            Optional::Logo => "logo",
            Optional::KeyboardBrightness => "keyboard brightness",
            Optional::BatteryCare => "battery care",
            Optional::MaxFan => "max fan speed",
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
];

/// What [`KNOWN_LACKING`] says the model with `pid` lacks; empty for any other PID.
pub fn known_lacking(pid: u16) -> Unsupported {
    KNOWN_LACKING
        .iter()
        .filter(|(p, _)| *p == pid)
        .flat_map(|(_, features)| features.iter())
        .fold(Unsupported::default(), |set, f| set.with(*f))
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
    fn every_setting_has_its_own_bit() {
        for f in Optional::ALL {
            let alone = Unsupported::default().with(f);
            assert_eq!(alone.iter().collect::<Vec<_>>(), vec![f]);
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
